use crate::{
    config::Config,
    credentials,
    events::Events,
    gateway, health,
    ownership::{self, Identity, Lock},
    protocol, service,
    state::{Phase, State},
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    process::Child,
    sync::mpsc,
    task::JoinHandle,
    time::{timeout, Instant},
};

enum Event {
    Bridge(String, Vec<String>),
    Disconnected(String),
    Native(String, String),
    NativeSummary(String, u64),
    Health(String, Result<()>),
    Discovery(String, Result<Vec<String>>),
}

struct Enrollment {
    config_path: PathBuf,
    target: crate::config::Instance,
    persistent: bool,
}

struct Supervisor {
    config: Config,
    name: String,
    dir: PathBuf,
    state: State,
    log: Events,
    child: Option<Child>,
    bridge: Option<OwnedWriteHalf>,
    reader: Option<JoinHandle<()>>,
    probe: Option<JoinHandle<()>>,
    last_pulse: Instant,
    started: Instant,
    healthy_since: Option<Instant>,
    next_start: Instant,
    next_probe: Instant,
    failures: u32,
    configured: bool,
    writable: bool,
    enabling_orders: bool,
    resuming_session: bool,
    next_notification: Instant,
    next_heartbeat: Instant,
    next_legacy_check: Instant,
    restart_deadline: Instant,
    orders_deadline: Option<Instant>,
    api_unresponsive_failures: u32,
    enrollment: Option<Enrollment>,
    enrollment_complete: bool,
    enrollment_cancelled: bool,
}

#[derive(Debug)]
struct BridgeUnavailable;
impl std::fmt::Display for BridgeUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("owned Gateway bridge is disconnected")
    }
}
impl std::error::Error for BridgeUnavailable {}

fn enrollment_promotion(
    previous: &crate::config::Instance,
    next: &crate::config::Instance,
) -> bool {
    if previous.api_orders || !previous.is_unbound() || !next.enabled || next.is_unbound() {
        return false;
    }
    let mut promoted = previous.clone();
    promoted.enabled = next.enabled;
    promoted.api_orders = next.api_orders;
    promoted.expected_accounts = next.expected_accounts.clone();
    promoted == *next
}

fn native_deadline(
    now: Instant,
    now_unix: i64,
    scheduled: Option<i64>,
    policy: &crate::config::Recovery,
) -> Instant {
    let remaining = match scheduled {
        Some(due) => due
            .saturating_add(policy.native_shutdown_timeout_secs as i64)
            .saturating_sub(now_unix)
            .max(0) as u64,
        None => policy.restart_grace_secs,
    };
    now + Duration::from_secs(remaining.min(86400))
}

fn listener(path: &Path) -> Result<UnixListener> {
    ensure!(
        path.as_os_str().len() < 104,
        "private socket path is too long for macOS"
    );
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
            "refusing to replace a non-owned/non-socket runtime entry"
        );
        fs::remove_file(path)?;
    }
    let socket = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(socket)
}

fn verify_peer(stream: &UnixStream, pid: Option<i32>) -> Result<()> {
    let credentials = stream.peer_cred()?;
    ensure!(
        credentials.uid() == unsafe { libc::geteuid() },
        "IPC peer belongs to a different user"
    );
    if let Some(pid) = pid {
        ensure!(
            credentials.pid() == Some(pid),
            "IPC peer is not the owned Gateway"
        );
    }
    Ok(())
}

pub async fn request(config: &Config, instance: &str, action: &str) -> Result<String> {
    config.instance(instance)?;
    timeout(Duration::from_secs(25), async {
        let mut stream =
            UnixStream::connect(config.directory(instance).join("control.sock")).await?;
        verify_peer(&stream, None)?;
        protocol::write(&mut stream, &[action]).await?;
        let response = protocol::read(&mut stream).await?;
        ensure!(response.len() == 2, "invalid control response");
        ensure!(
            response[0] == "OK",
            "supervisor rejected request: {}",
            response[1]
        );
        Ok(response[1].clone())
    })
    .await
    .context("supervisor control deadline exceeded")?
}

pub async fn stop_unreachable(config: &Config, instance: &str) -> Result<()> {
    let dir = config.directory(instance);
    if !dir.try_exists()? {
        return Ok(());
    }
    ownership::private_dir(&dir)?;
    let _lock = Lock::acquire(&dir.join("supervisor.lock")).context(
        "a supervisor still owns this instance; resolve its control connection before cleanup",
    )?;
    let path = dir.join("state.json");
    if !path.try_exists()? {
        return Ok(());
    }
    let mut state: State = serde_json::from_slice(&ownership::read_private(&path)?)?;
    if let Some(owner) = &state.owner {
        if owner.alive()? {
            ensure!(
                gateway::expected_executable(config, instance, &owner.executable()?)?,
                "recorded orphan is not the configured Gateway executable; refusing to signal"
            );
            gateway::terminate(owner, &mut None).await?;
        }
    }
    state.owner = None;
    state.desired_running = false;
    state.phase = Phase::Stopped;
    state.reason = "service_uninstalled".into();
    state.launch_pending = false;
    ownership::atomic_write(&path, &serde_json::to_vec(&state)?)?;
    Ok(())
}

pub async fn run(config: Config, name: &str) -> Result<()> {
    run_inner(config, name, None).await.map(|_| ())
}

pub async fn enroll(
    mut config: Config,
    name: &str,
    config_path: PathBuf,
    persistent: bool,
) -> Result<bool> {
    let target = config.instance(name)?.clone();
    ensure!(!target.enabled, "disable a profile before enrollment");
    ensure!(
        target.is_unbound(),
        "existing pinned account identities cannot be silently replaced by enrollment"
    );
    let mut readonly = target.clone();
    readonly.enabled = true;
    readonly.api_orders = false;
    config.instances.insert(name.to_owned(), readonly);
    run_inner(
        config,
        name,
        Some(Enrollment {
            config_path,
            target,
            persistent,
        }),
    )
    .await
}

async fn run_inner(config: Config, name: &str, enrollment: Option<Enrollment>) -> Result<bool> {
    let profile = config.instance(name)?.clone();
    ensure!(profile.enabled, "instance is disabled");
    ownership::private_dir(&config.state_dir)?;
    let dir = config.directory(name);
    ownership::private_dir(&dir)?;
    ownership::private_dir(&dir.join("settings"))?;
    let _lock = Lock::acquire(&dir.join("supervisor.lock"))?;
    let state_path = dir.join("state.json");
    let mut state = if state_path.exists() {
        serde_json::from_slice::<State>(&ownership::read_private(&state_path)?).context(
            "invalid persisted state; inspect it rather than resetting ownership/retry limits",
        )?
    } else {
        State::new(profile.clone())
    };
    let alive = state
        .owner
        .as_ref()
        .map(Identity::alive)
        .transpose()?
        .unwrap_or(false);
    let changed = state.profile != profile;
    if changed && alive {
        state.phase = Phase::NeedsAttention;
        state.reason = "configuration_changed_while_gateway_running".into();
    } else {
        state.profile = profile;
    }
    if state.phase == Phase::Ready {
        state.phase = Phase::Starting;
        state.reason = "revalidating_owned_gateway_after_supervisor_start".into();
    }
    let controls = listener(&dir.join("control.sock"))?;
    let bridges = listener(&dir.join("bridge.sock"))?;
    let log = Events::new(name, &dir, config.tradebus_events.clone())?;
    let now = Instant::now();
    let restart_deadline = native_deadline(
        now,
        time::OffsetDateTime::now_utc().unix_timestamp(),
        state.native_restart_due_unix,
        &config.recovery,
    );
    let backoff = state
        .backoff_until_unix
        .saturating_sub(time::OffsetDateTime::now_utc().unix_timestamp())
        .clamp(
            0,
            config.recovery.backoff_max_secs.min(i64::MAX as u64) as i64,
        ) as u64;
    let mut supervisor = Supervisor {
        config,
        name: name.into(),
        dir,
        state,
        log,
        child: None,
        bridge: None,
        reader: None,
        probe: None,
        last_pulse: now,
        started: now,
        healthy_since: None,
        next_start: now + Duration::from_secs(backoff),
        next_probe: now,
        failures: 0,
        configured: false,
        writable: false,
        enabling_orders: false,
        resuming_session: false,
        next_notification: now,
        next_heartbeat: now,
        next_legacy_check: now,
        restart_deadline,
        orders_deadline: None,
        api_unresponsive_failures: 0,
        enrollment,
        enrollment_complete: false,
        enrollment_cancelled: false,
    };
    supervisor.save()?;
    supervisor.log.log(
        "supervisor",
        "Started; reconciling only owned instance resources",
    )?;
    let (sender, mut receiver) = mpsc::channel::<Event>(128);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            incoming = controls.accept() => {
                let (mut stream, _) = incoming?;
                let result = async {
                    verify_peer(&stream, None)?;
                    let fields = timeout(Duration::from_secs(2), protocol::read(&mut stream)).await??;
                    ensure!(fields.len() == 1, "one control action required");
                    supervisor.control(&fields[0]).await
                }.await;
                let (kind, body) = match result {
                    Ok(body) => ("OK", body),
                    Err(error) => ("ERROR", format!("{error:#}")),
                };
                match timeout(Duration::from_secs(2), protocol::write(&mut stream, &[kind, &body])).await {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => supervisor.log.log("control_error", &error.to_string())?,
                    Err(error) => supervisor.log.log("control_error", &error.to_string())?,
                }
            }
            incoming = bridges.accept() => {
                let (stream, _) = incoming?;
                if let Err(error) = supervisor.attach(stream, sender.clone()).await {
                    supervisor.log.log("bridge_rejected", &error.to_string())?;
                }
            }
            Some(event) = receiver.recv() => {
                if let Err(error) = supervisor.event(event).await {
                    supervisor.runtime_failure(error)?;
                }
            }
            _ = tick.tick() => {
                if let Err(error) = supervisor.tick(sender.clone()).await {
                    supervisor.runtime_failure(error)?;
                }
            }
        }
        if supervisor.enrollment_complete {
            for path in ["control.sock", "bridge.sock"] {
                fs::remove_file(supervisor.dir.join(path))?;
            }
            return Ok(true);
        }
        if supervisor.enrollment_cancelled {
            break;
        }
    }
    supervisor.stop().await?;
    supervisor.transition(Phase::Stopped, "supervisor_shutdown", false)?;
    for path in ["control.sock", "bridge.sock"] {
        fs::remove_file(supervisor.dir.join(path))?;
    }
    Ok(false)
}

impl Supervisor {
    fn consume_restart_marker(&mut self) -> Result<Option<bool>> {
        let marker = self.dir.join("restart.request");
        if !marker.try_exists()? {
            return Ok(None);
        }
        let bytes = match ownership::read_private(&marker) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.log
                    .log("restart_marker_error", &format!("{error:#}"))?;
                self.transition(
                    Phase::NeedsAttention,
                    "native_restart_marker_unreadable",
                    true,
                )?;
                return Ok(Some(false));
            }
        };
        let parsed = std::str::from_utf8(&bytes).ok().and_then(|content| {
            let mut lines = content.lines();
            if lines.next() != Some(self.state.generation.as_str()) {
                return None;
            }
            let session = lines.next()?;
            if lines.next().is_some() || (!session.is_empty() && !gateway::valid_session(session)) {
                return None;
            }
            Some(if session.is_empty() {
                None
            } else {
                Some(session.to_owned())
            })
        });
        let Some(session) = parsed else {
            fs::rename(marker, self.dir.join("restart.rejected"))?;
            self.transition(
                Phase::NeedsAttention,
                "native_restart_marker_quarantined",
                true,
            )?;
            return Ok(Some(false));
        };
        self.state.resume_session = if self.state.require_full_login {
            None
        } else {
            session
        };
        self.save()?;
        fs::remove_file(marker)?;
        Ok(Some(true))
    }

    fn runtime_failure(&mut self, error: anyhow::Error) -> Result<()> {
        if error.is::<std::io::Error>()
            || error.is::<tokio::time::error::Elapsed>()
            || error.is::<BridgeUnavailable>()
        {
            self.log.log("runtime_error", &format!("{error:#}"))?;
            self.transition(
                Phase::NeedsAttention,
                "supervisor_io_requires_attention",
                true,
            )
        } else {
            Err(error)
        }
    }

    fn save(&self) -> Result<()> {
        ownership::atomic_write(
            &self.dir.join("state.json"),
            &serde_json::to_vec(&self.state)?,
        )
    }

    fn transition(&mut self, phase: Phase, reason: &str, human: bool) -> Result<()> {
        if self.state.phase == phase
            && self.state.reason == reason
            && self.state.notification_human == human
        {
            return Ok(());
        }
        if phase != Phase::Ready {
            self.healthy_since = None;
        }
        self.state.phase = phase;
        self.state.reason = reason.into();
        self.state.notification_pending = true;
        self.state.notification_human = human;
        self.save()?;
        self.deliver_notification()?;
        Ok(())
    }

    fn deliver_notification(&mut self) -> Result<()> {
        self.next_notification = Instant::now() + Duration::from_secs(60);
        match self.log.transition(
            self.state.phase.as_str(),
            &self.state.reason,
            self.state.notification_human,
        ) {
            Ok(()) => {
                self.state.notification_pending = false;
                self.save()?;
            }
            Err(error) => self.log.log("notification_error", &format!("{error:#}"))?,
        }
        Ok(())
    }

    fn public_status(&self) -> String {
        serde_json::json!({
            "instance": self.name,
            "mode": self.state.profile.mode,
            "api_port": self.state.profile.api_port,
            "phase": self.state.phase,
            "reason": self.state.reason,
            "gateway_pid": self.state.owner.as_ref().map(|o| o.pid),
            "desired_running": self.state.desired_running,
            "restart_count": self.state.restarts,
            "bridge_connected": self.bridge.is_some(),
            "ui_pulse_age_secs": self.last_pulse.elapsed().as_secs(),
            "api_orders_enabled": self.writable && self.state.phase == Phase::Ready,
            "notification_pending": self.state.notification_pending,
            "read_only_enrollment": self.enrollment.is_some(),
        })
        .to_string()
    }

    async fn send(&mut self, fields: &[&str]) -> Result<()> {
        let writer = self.bridge.as_mut().ok_or(BridgeUnavailable)?;
        timeout(Duration::from_secs(2), protocol::write(writer, fields)).await??;
        Ok(())
    }

    async fn control(&mut self, action: &str) -> Result<String> {
        match action {
            "STATUS" => return Ok(self.public_status()),
            "DIAGNOSE" => {
                self.send(&["DIAGNOSE"]).await?;
                return Ok("Requested credential-redacted UI diagnostics in the instance's private controller log".into());
            }
            "STOP" => {
                self.state.desired_running = false;
                self.state.resume_session = None;
                self.stop().await?;
                self.transition(Phase::Stopped, "operator_stopped", false)?;
                self.enrollment_cancelled = self
                    .enrollment
                    .as_ref()
                    .is_some_and(|enrollment| !enrollment.persistent);
            }
            "START" | "RESUME" => {
                if self.state.desired_running
                    && self.state.owner.is_some()
                    && self.state.phase != Phase::NeedsAttention
                {
                    return Ok(self.public_status());
                }
                if self.state.launch_pending {
                    ensure!(
                        self.state.owner.is_none()
                            && self.state.reason
                                == "interrupted_launch_requires_ownership_reconciliation",
                        "launch still pending; inspect status before acknowledging it"
                    );
                    self.state.launch_pending = false;
                    self.state.generation.clear();
                }
                ensure!(
                    self.state.profile == *self.config.instance(&self.name)?
                        || self.state.owner.is_none()
                        || enrollment_promotion(
                            &self.state.profile,
                            self.config.instance(&self.name)?
                        ),
                    "stop the existing Gateway before applying changed configuration"
                );
                let profile_changed = self.state.profile != *self.config.instance(&self.name)?;
                self.state.profile = self.config.instance(&self.name)?.clone();
                self.state.desired_running = true;
                self.state.restarts = 0;
                self.state.backoff_until_unix = 0;
                self.next_start = Instant::now();
                self.failures = 0;
                self.api_unresponsive_failures = 0;
                self.enabling_orders = false;
                self.orders_deadline = None;
                self.configured = false;
                self.writable = false;
                if let Some(probe) = self.probe.take() {
                    probe.abort();
                }
                if self.bridge.is_some() {
                    if profile_changed {
                        let fields = self.bridge_configuration();
                        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
                        self.send(&fields).await?;
                    }
                    self.send(&["RESUME"]).await?;
                }
                self.started = Instant::now();
                self.transition(Phase::Starting, "operator_requested_start", false)?;
            }
            "RESTART" => {
                ensure!(
                    self.state.phase == Phase::Ready,
                    "native restart requires a ready Gateway"
                );
                self.send(&["RESTART"]).await?;
                self.started = Instant::now();
                self.state.native_restart_due_unix = None;
                self.restart_deadline =
                    Instant::now() + Duration::from_secs(self.config.recovery.restart_grace_secs);
                self.transition(
                    Phase::NativeRestarting,
                    "operator_requested_native_restart",
                    false,
                )?;
            }
            _ => bail!("unknown control command"),
        }
        self.save()?;
        Ok(self.public_status())
    }

    async fn attach(&mut self, mut stream: UnixStream, sender: mpsc::Sender<Event>) -> Result<()> {
        verify_peer(&stream, None)?;
        let hello = timeout(Duration::from_secs(3), protocol::read(&mut stream)).await??;
        ensure!(
            hello.len() == 4
                && hello[0] == "HELLO"
                && hello[1] == protocol::BRIDGE_VERSION
                && hello[3] == self.state.generation,
            "bridge protocol, process or generation mismatch"
        );
        let pid: i32 = hello[2]
            .parse()
            .context("invalid bridge process identity")?;
        verify_peer(&stream, Some(pid))?;
        let prior_owner_exited = self
            .state
            .owner
            .as_ref()
            .map(|owner| owner.alive().map(|alive| !alive))
            .transpose()?
            .unwrap_or(false);
        if prior_owner_exited {
            self.state.owner = None;
            self.child = None;
        }
        if self.state.owner.is_none() {
            ensure!(
                self.state.launch_pending
                    || (self.state.desired_running
                        && matches!(
                            self.state.phase,
                            Phase::BackingOff | Phase::NativeRestarting
                        )),
                "no pending launch may be adopted"
            );
            ensure!(
                self.consume_restart_marker()? != Some(false),
                "native restart journal requires attention"
            );
            let identity =
                Identity::read(pid)?.context("pending launch process no longer exists")?;
            ensure!(
                gateway::expected_executable(&self.config, &self.name, &identity.executable()?)?,
                "pending bridge does not use the expected Gateway JVM"
            );
            self.state.owner = Some(identity);
            self.state.launch_pending = false;
            self.resuming_session = self.state.resume_session.is_some();
            self.state.resume_session = None;
            self.configured = false;
            self.writable = false;
            self.enabling_orders = false;
            self.orders_deadline = None;
            self.started = Instant::now();
            self.transition(Phase::Starting, "adopting_native_gateway_launch", false)?;
            self.save()?;
        }
        let owner = self.state.owner.as_ref().context("missing bridge owner")?;
        ensure!(
            owner.pid == pid && owner.alive()?,
            "Gateway process identity no longer matches"
        );
        let fields = self.bridge_configuration();
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        protocol::write(&mut stream, &fields).await?;
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        let (mut reader, writer) = stream.into_split();
        self.bridge = Some(writer);
        self.last_pulse = Instant::now();
        let generation = self.state.generation.clone();
        self.reader = Some(tokio::spawn(async move {
            loop {
                match protocol::read(&mut reader).await {
                    Ok(fields) => {
                        if sender
                            .send(Event::Bridge(generation.clone(), fields))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = sender.send(Event::Disconnected(generation)).await;
                        return;
                    }
                }
            }
        }));
        Ok(())
    }

    fn bridge_configuration(&self) -> Vec<String> {
        let p = &self.state.profile;
        vec![
            "CONFIG".into(),
            p.mode.as_str().into(),
            p.api_port.to_string(),
            p.auto_restart_time.clone(),
            p.api_orders.to_string(),
            self.config.recovery.session_resume_timeout_secs.to_string(),
            self.config.recovery.mfa_timeout_secs.to_string(),
            self.config.recovery.startup_timeout_secs.to_string(),
            self.resuming_session.to_string(),
        ]
    }

    async fn start(&mut self, sender: mpsc::Sender<Event>) -> Result<()> {
        if service::legacy_monitor_conflict().await? {
            self.transition(
                Phase::NeedsAttention,
                "legacy_monitor_conflict_disable_before_cutover",
                true,
            )?;
            return Ok(());
        }
        if self.consume_restart_marker()? == Some(false) {
            return Ok(());
        }
        if tokio::net::TcpListener::bind(("127.0.0.1", self.state.profile.api_port))
            .await
            .is_err()
        {
            self.transition(
                Phase::NeedsAttention,
                "api_port_already_in_use_no_takeover",
                true,
            )?;
            return Ok(());
        }
        let generation = ownership::nonce()?;
        let mut restart = self.state.resume_session.clone();
        if let Some(session) = &restart {
            ensure!(
                gateway::valid_session(session),
                "invalid persisted native restart identity"
            );
            match fs::symlink_metadata(self.dir.join("settings").join(session).join("autorestart"))
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.log.log(
                        "native_resume_missing",
                        "Native resume state expired; proceeding with full credential login",
                    )?;
                    restart = None;
                    self.state.resume_session = None;
                    self.save()?;
                }
                Err(error) => return Err(error).context("inspect native resume state"),
            }
        }
        let mut command =
            gateway::command(&self.config, &self.name, &generation, restart.as_deref())?;
        self.state.generation = generation.clone();
        self.state.launch_pending = true;
        self.save()?;
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.state.launch_pending = false;
                self.save()?;
                return Err(error).context("spawn owned Gateway JVM");
            }
        };
        let pid = child.id().context("Gateway child has no PID")? as i32;
        let identity = Identity::read(pid)?.context("Gateway exited during startup")?;
        self.state.owner = Some(identity);
        self.state.launch_pending = false;
        self.state.resume_session = None;
        self.resuming_session = restart.is_some();
        self.child = Some(child);
        self.configured = false;
        self.writable = false;
        self.enabling_orders = false;
        self.started = Instant::now();
        self.last_pulse = Instant::now();
        self.next_probe = Instant::now();
        self.failures = 0;
        self.api_unresponsive_failures = 0;
        self.orders_deadline = None;
        self.state.native_restart_due_unix = None;
        self.transition(
            Phase::Starting,
            if restart.is_some() {
                "resuming_native_session"
            } else {
                "starting_full_login"
            },
            false,
        )?;
        child = self.child.take().context("owned child unavailable")?;
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::pin(s) as std::pin::Pin<Box<dyn AsyncRead + Send>>),
            child
                .stderr
                .take()
                .map(|s| Box::pin(s) as std::pin::Pin<Box<dyn AsyncRead + Send>>),
        ]
        .into_iter()
        .flatten()
        {
            tokio::spawn(native_output(pipe, sender.clone(), generation.clone()));
        }
        self.child = Some(child);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(probe) = self.probe.take() {
            probe.abort();
        }
        if self.state.owner.is_some() {
            self.transition(Phase::Stopping, "stopping_owned_gateway", false)?;
            if self.bridge.is_some() {
                if let Err(error) = self.send(&["STOP"]).await {
                    self.log.log("shutdown_warning", &error.to_string())?;
                }
                let until = Instant::now() + Duration::from_secs(3);
                while Instant::now() < until {
                    if let Some(child) = self.child.as_mut() {
                        if child.try_wait()?.is_some() {
                            break;
                        }
                    }
                    if !self
                        .state
                        .owner
                        .as_ref()
                        .context("missing owner")?
                        .alive()?
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            gateway::terminate(
                self.state.owner.as_ref().context("missing owner")?,
                &mut self.child,
            )
            .await?;
        }
        self.child = None;
        self.state.owner = None;
        self.state.launch_pending = false;
        self.bridge = None;
        self.configured = false;
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        let marker = self.dir.join("restart.request");
        if marker.exists() {
            let content = String::from_utf8(ownership::read_private(&marker)?)?;
            if content.lines().next() == Some(self.state.generation.as_str()) {
                fs::remove_file(marker)?;
            } else {
                fs::rename(marker, self.dir.join("restart.rejected"))?;
                self.log.log(
                    "restart_marker_rejected",
                    "Quarantined a foreign-generation restart marker",
                )?;
            }
        }
        self.save()?;
        Ok(())
    }

    async fn cold_restart(&mut self, reason: &str) -> Result<()> {
        self.stop().await?;
        self.state.resume_session = None;
        self.schedule_restart(false, reason)
    }

    fn schedule_restart(&mut self, native: bool, reason: &str) -> Result<()> {
        if let Some(delay) = self.state.reserve_restart(&self.config.recovery) {
            let delay = if native { 2 } else { delay };
            self.next_start = Instant::now() + Duration::from_secs(delay);
            self.state.backoff_until_unix = time::OffsetDateTime::now_utc()
                .unix_timestamp()
                .saturating_add(delay.min(i64::MAX as u64) as i64);
            self.transition(Phase::BackingOff, reason, false)?;
        } else {
            self.transition(Phase::NeedsAttention, "restart_budget_exhausted", true)?;
        }
        Ok(())
    }

    async fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Discovery(generation, result) if generation == self.state.generation => {
                self.probe = None;
                if self.state.phase == Phase::NeedsAttention
                    || self.state.phase == Phase::AwaitingMfa
                {
                    return Ok(());
                }
                match result {
                    Ok(accounts) => {
                        ensure!(
                            self.configured && !self.writable && !self.state.profile.api_orders,
                            "enrollment cannot proceed without verified read-only settings"
                        );
                        let enrollment = self
                            .enrollment
                            .as_ref()
                            .context("unexpected account discovery result")?;
                        let mut target_config = Config::load(&enrollment.config_path)?;
                        ensure!(
                            target_config.instance(&self.name)? == &enrollment.target,
                            "profile changed during account enrollment; refusing to overwrite it"
                        );
                        let mut target = enrollment.target.clone();
                        target.enabled = true;
                        target.expected_accounts = accounts;
                        target_config
                            .instances
                            .insert(self.name.clone(), target.clone());
                        target_config.save(&enrollment.config_path)?;
                        self.state.profile = target;
                        self.state.desired_running = true;
                        self.transition(
                            Phase::Starting,
                            "read_only_enrollment_verified_pending_service_handoff",
                            false,
                        )?;
                        self.enrollment_complete = true;
                    }
                    Err(error) => {
                        self.log.log("enrollment_error", &format!("{error:#}"))?;
                        self.failures = self.failures.saturating_add(1);
                        self.transition(
                            Phase::Reconnecting,
                            "read_only_account_discovery_failed",
                            self.failures >= self.config.recovery.failure_threshold,
                        )?;
                    }
                }
            }
            Event::Native(generation, line) if generation == self.state.generation => {
                self.log.native_log(&line)?
            }
            Event::NativeSummary(generation, count) if generation == self.state.generation => {
                self.log.log(
                    "gateway_output_summary",
                    &format!("{count} native output lines omitted"),
                )?;
            }
            Event::Disconnected(generation) if generation == self.state.generation => {
                self.bridge = None;
                self.log.log(
                    "bridge_disconnected",
                    "Awaiting owned bridge reconnection or child exit",
                )?;
            }
            Event::Health(generation, result) if generation == self.state.generation => {
                self.probe = None;
                if !self.configured
                    || matches!(
                        self.state.phase,
                        Phase::AwaitingMfa
                            | Phase::NeedsAttention
                            | Phase::NativeRestarting
                            | Phase::Stopped
                    )
                {
                    return Ok(());
                }
                match result {
                    Ok(()) => {
                        self.failures = 0;
                        self.api_unresponsive_failures = 0;
                        if self.last_pulse.elapsed().as_secs()
                            >= self.config.recovery.ui_stall_timeout_secs
                        {
                            self.transition(
                                Phase::Reconnecting,
                                "ui_stalled_api_responsive",
                                true,
                            )?;
                            return Ok(());
                        }
                        if self.state.profile.api_orders && !self.writable {
                            if !self.enabling_orders {
                                self.send(&["ENABLE_ORDERS"]).await?;
                                self.enabling_orders = true;
                                self.orders_deadline = Some(
                                    Instant::now()
                                        + Duration::from_secs(
                                            self.config.recovery.startup_timeout_secs,
                                        ),
                                );
                            }
                            self.transition(
                                Phase::Configuring,
                                "enabling_verified_account_api_orders",
                                false,
                            )?;
                        } else {
                            self.state.require_full_login = false;
                            self.healthy_since.get_or_insert_with(Instant::now);
                            self.transition(Phase::Ready, "account_and_api_verified", false)?;
                        }
                    }
                    Err(error) => {
                        self.healthy_since = None;
                        let detail = format!("{error:#}");
                        self.log.log("health_failure", &detail)?;
                        let issue = error.downcast_ref::<health::Issue>().copied();
                        if issue == Some(health::Issue::AccountMismatch) {
                            self.stop().await?;
                            self.transition(
                                Phase::NeedsAttention,
                                "account_identity_mismatch",
                                true,
                            )?;
                        } else {
                            self.failures = self.failures.saturating_add(1);
                            if matches!(
                                issue,
                                Some(
                                    health::Issue::ApiNotListening
                                        | health::Issue::Deadline
                                        | health::Issue::HandshakeUnavailable
                                )
                            ) {
                                self.api_unresponsive_failures =
                                    self.api_unresponsive_failures.saturating_add(1);
                            } else {
                                self.api_unresponsive_failures = 0;
                            }
                            let persistent =
                                self.failures >= self.config.recovery.failure_threshold;
                            let reason = if issue == Some(health::Issue::UpstreamDisconnected) {
                                "broker_connectivity_lost"
                            } else if issue == Some(health::Issue::ClientConflict) {
                                "monitor_client_id_conflict"
                            } else {
                                "api_probe_failed"
                            };
                            self.transition(Phase::Reconnecting, reason, persistent)?;
                            if persistent && issue == Some(health::Issue::ClientConflict) {
                                self.transition(Phase::NeedsAttention, reason, true)?;
                                return Ok(());
                            }
                            // A failed API client handshake can be a client-ID conflict or an
                            // upstream outage. Do not disrupt a responsive Gateway to fix it.
                            if persistent && issue == Some(health::Issue::ApiNotListening) {
                                if self.bridge.is_some() {
                                    self.send(&["RESTART"]).await?;
                                    self.started = Instant::now();
                                    self.state.native_restart_due_unix = None;
                                    self.restart_deadline = Instant::now()
                                        + Duration::from_secs(
                                            self.config.recovery.restart_grace_secs,
                                        );
                                    self.transition(
                                        Phase::NativeRestarting,
                                        "recovering_unresponsive_api",
                                        false,
                                    )?;
                                } else {
                                    self.cold_restart("api_and_bridge_unresponsive").await?;
                                }
                            }
                        }
                    }
                }
            }
            Event::Bridge(generation, fields)
                if generation == self.state.generation && self.state.owner.is_some() =>
            {
                if fields.first().map(String::as_str) == Some("PULSE") && fields.len() == 1 {
                    self.last_pulse = Instant::now();
                    return Ok(());
                }
                if fields.first().map(String::as_str) == Some("DIALOG") && fields.len() == 3 {
                    let name = self.name.clone();
                    match timeout(
                        Duration::from_secs(5),
                        tokio::task::spawn_blocking(move || credentials::load(&name)),
                    )
                    .await
                    {
                        Ok(Ok(Ok(credentials))) => {
                            let diagnostic = credentials.redact(
                                &fields[1..].join("\n"),
                                &self.state.profile.expected_accounts,
                            );
                            self.log.log("dialog_diagnostic", &diagnostic)?;
                        }
                        _ => self.log.log(
                            "dialog_diagnostic",
                            "Dialog details withheld: credential redaction unavailable",
                        )?,
                    }
                    return Ok(());
                }
                if fields.first().map(String::as_str) == Some("RESTART_SCHEDULED")
                    && fields.len() == 2
                {
                    if self.state.phase != Phase::NeedsAttention {
                        let scheduled: i64 = fields[1]
                            .parse()
                            .context("invalid native restart schedule")?;
                        let now_unix = time::OffsetDateTime::now_utc().unix_timestamp();
                        ensure!(
                            (now_unix - 3600..=now_unix + 3600).contains(&scheduled),
                            "native restart schedule is out of bounds"
                        );
                        self.state.native_restart_due_unix = Some(scheduled);
                        self.restart_deadline = native_deadline(
                            Instant::now(),
                            now_unix,
                            Some(scheduled),
                            &self.config.recovery,
                        );
                        self.save()?;
                    }
                    return Ok(());
                }
                ensure!(
                    fields.len() == 3 && fields[0] == "STATE",
                    "unexpected bridge message"
                );
                if self.state.phase == Phase::NeedsAttention {
                    return Ok(());
                }
                let phase = fields[1].as_str();
                match phase {
                    "starting" => {}
                    "login_required" => {
                        self.transition(Phase::Authenticating, "preparing_automatic_login", false)?;
                        let name = self.name.clone();
                        match timeout(
                            Duration::from_secs(5),
                            tokio::task::spawn_blocking(move || credentials::load(&name)),
                        )
                        .await
                        {
                            Ok(Ok(Ok(secret))) => {
                                self.send(&["LOGIN", &secret.username, &secret.password])
                                    .await?
                            }
                            _ => self.transition(
                                Phase::NeedsAttention,
                                "keychain_credentials_unavailable",
                                true,
                            )?,
                        }
                    }
                    "authenticating" => {
                        self.transition(Phase::Authenticating, "automatic_login_submitted", false)?
                    }
                    "awaiting_mfa" => {
                        self.transition(Phase::AwaitingMfa, "broker_requires_user_approval", true)?
                    }
                    "needs_attention" => {
                        if fields[2].starts_with("read_only_login") {
                            self.state.require_full_login = true;
                            self.state.resume_session = None;
                        }
                        self.transition(Phase::NeedsAttention, &fields[2], true)?
                    }
                    "resume_login_required" => {
                        self.cold_restart("native_session_requires_fresh_credentials")
                            .await?
                    }
                    "configured_readonly" | "configured_writable" => {
                        if self.enrollment.is_some() && phase == "configured_writable" {
                            self.stop().await?;
                            self.transition(
                                Phase::NeedsAttention,
                                "enrollment_must_remain_read_only",
                                true,
                            )?;
                            return Ok(());
                        }
                        self.configured = true;
                        self.writable = phase == "configured_writable";
                        if self.writable {
                            self.enabling_orders = false;
                            self.orders_deadline = None;
                        }
                        if self.state.phase != Phase::NativeRestarting {
                            self.transition(
                                Phase::Configuring,
                                "verifying_api_account_identity",
                                false,
                            )?;
                            self.next_probe = Instant::now();
                        }
                    }
                    "native_restarting" => {
                        if self.state.phase != Phase::NativeRestarting {
                            self.started = Instant::now();
                            self.restart_deadline = native_deadline(
                                Instant::now(),
                                time::OffsetDateTime::now_utc().unix_timestamp(),
                                self.state.native_restart_due_unix,
                                &self.config.recovery,
                            );
                        }
                        self.transition(
                            Phase::NativeRestarting,
                            "gateway_requested_restart",
                            false,
                        )?;
                    }
                    _ => {
                        self.transition(Phase::NeedsAttention, "unsupported_bridge_state", true)?
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn tick(&mut self, sender: mpsc::Sender<Event>) -> Result<()> {
        let now = Instant::now();
        if self.state.owner.is_some() && now >= self.next_legacy_check {
            self.next_legacy_check = now + Duration::from_secs(30);
            if service::legacy_monitor_conflict().await? {
                self.transition(
                    Phase::NeedsAttention,
                    "legacy_monitor_conflict_disable_before_cutover",
                    true,
                )?;
            }
        }
        if self.state.notification_pending && now >= self.next_notification {
            self.deliver_notification()?;
        }
        if now >= self.next_heartbeat {
            self.next_heartbeat = now + Duration::from_secs(3600);
            if let Err(error) = self.log.heartbeat(self.state.phase.as_str()) {
                self.log
                    .log("heartbeat_delivery_error", &format!("{error:#}"))?;
            }
        }
        let mut exited = false;
        if let Some(child) = self.child.as_mut() {
            exited = child.try_wait()?.is_some();
        } else if let Some(owner) = &self.state.owner {
            exited = !owner.alive()?;
        }
        if exited {
            self.child = None;
            self.state.owner = None;
            self.bridge = None;
            if let Some(reader) = self.reader.take() {
                reader.abort();
            }
            if let Some(probe) = self.probe.take() {
                probe.abort();
            }
            let marker = self.consume_restart_marker()?;
            if marker == Some(false) {
                return Ok(());
            }
            if marker == Some(true) {
                if self.state.desired_running
                    && !matches!(self.state.phase, Phase::AwaitingMfa | Phase::NeedsAttention)
                {
                    self.state.launch_pending = true;
                    self.state.native_restart_due_unix = None;
                    self.started = Instant::now();
                    self.transition(
                        Phase::NativeRestarting,
                        "waiting_for_native_gateway_relaunch",
                        false,
                    )?;
                } else {
                    self.log.log(
                        "native_resume_held",
                        "Native resume is saved; automatic restart remains inhibited",
                    )?;
                }
            } else if self.state.desired_running {
                if matches!(
                    self.state.phase,
                    Phase::AwaitingMfa | Phase::Authenticating | Phase::NeedsAttention
                ) {
                    self.transition(
                        Phase::NeedsAttention,
                        "gateway_exited_during_authentication_or_intervention",
                        true,
                    )?;
                } else {
                    self.schedule_restart(false, "gateway_process_exited")?;
                }
            }
            self.save()?;
        }
        if !self.state.desired_running
            || matches!(self.state.phase, Phase::NeedsAttention | Phase::AwaitingMfa)
        {
            return Ok(());
        }
        if self.state.owner.is_none() {
            if self.state.launch_pending {
                if self.started.elapsed().as_secs() > self.config.recovery.startup_timeout_secs {
                    self.state.launch_pending = false;
                    self.transition(
                        Phase::NeedsAttention,
                        "interrupted_launch_requires_ownership_reconciliation",
                        true,
                    )?;
                }
                return Ok(());
            }
            if now >= self.next_start {
                if let Err(error) = self.start(sender.clone()).await {
                    self.log.log("startup_error", &format!("{error:#}"))?;
                    self.transition(Phase::NeedsAttention, "startup_preflight_failed", true)?;
                }
            }
            return Ok(());
        }
        if self.state.phase == Phase::NativeRestarting {
            if now >= self.restart_deadline {
                self.transition(
                    Phase::NeedsAttention,
                    "native_restart_deadline_exceeded_process_preserved",
                    true,
                )?;
            }
            return Ok(());
        }
        if self.orders_deadline.is_some_and(|deadline| now >= deadline) {
            self.transition(
                Phase::NeedsAttention,
                "api_order_configuration_deadline_exceeded",
                true,
            )?;
            return Ok(());
        }
        if !self.configured {
            let deadline = self.config.recovery.startup_timeout_secs
                + if self.resuming_session {
                    self.config.recovery.session_resume_timeout_secs
                } else {
                    0
                };
            if self.started.elapsed().as_secs() >= deadline {
                self.transition(
                    Phase::NeedsAttention,
                    "startup_or_login_requires_inspection",
                    true,
                )?;
            }
            return Ok(());
        }
        if self.last_pulse.elapsed().as_secs() >= self.config.recovery.ui_stall_timeout_secs {
            if self.api_unresponsive_failures >= self.config.recovery.failure_threshold {
                self.cold_restart("owned_ui_and_api_unresponsive").await?;
                return Ok(());
            }
            self.transition(
                Phase::Reconnecting,
                "ui_stalled_awaiting_api_corroboration",
                true,
            )?;
        }
        if self.state.phase == Phase::Ready
            && self
                .healthy_since
                .is_some_and(|t| t.elapsed().as_secs() >= self.config.recovery.healthy_reset_secs)
            && self.state.restarts != 0
        {
            self.state.restarts = 0;
            self.save()?;
        }
        if now >= self.next_probe && self.probe.is_none() {
            self.next_probe = now + Duration::from_secs(self.config.recovery.check_interval_secs);
            let profile = self.state.profile.clone();
            let owner = self
                .state
                .owner
                .clone()
                .context("missing process ownership")?;
            let generation = self.state.generation.clone();
            let enrolling = self.enrollment.is_some();
            self.probe = Some(tokio::spawn(async move {
                if enrolling {
                    let result = async {
                        ensure!(owner.alive()?, "Gateway identity changed before enrollment");
                        health::verify_port_owner(profile.api_port, owner.pid).await?;
                        health::discover(&profile).await
                    }
                    .await;
                    let _ = sender.send(Event::Discovery(generation, result)).await;
                    return;
                }
                let result = async {
                    ensure!(
                        owner.alive()?,
                        "Gateway identity changed before health check"
                    );
                    health::verify_port_owner(profile.api_port, owner.pid).await?;
                    health::probe(&profile).await
                }
                .await;
                let _ = sender.send(Event::Health(generation, result)).await;
            }));
        }
        Ok(())
    }
}

async fn native_output(
    mut pipe: std::pin::Pin<Box<dyn AsyncRead + Send>>,
    sender: mpsc::Sender<Event>,
    generation: String,
) {
    let mut buffer = [0; 4096];
    let mut line = Vec::new();
    let mut omitted = 0_u64;
    let mut summary = tokio::time::interval(Duration::from_secs(5));
    summary.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            read = pipe.read(&mut buffer) => {
                let length = match read {
                    Ok(0) => break,
                    Ok(length) => length,
                    Err(_) => {
                        let _ = sender.try_send(Event::Native(generation.clone(), "Gateway output capture failed".into()));
                        break;
                    }
                };
                for byte in &buffer[..length] {
                    if *byte == b'\n' {
                        let text = String::from_utf8_lossy(&line);
                        if crate::events::allowed_native_line(&text) {
                            if sender.try_send(Event::Native(generation.clone(), text.into_owned())).is_err() {
                                omitted = omitted.saturating_add(1);
                            }
                        } else {
                            omitted = omitted.saturating_add(1);
                        }
                        line.clear();
                    } else if line.len() < 4096 {
                        line.push(*byte);
                    }
                }
            }
            _ = summary.tick() => {
                if omitted > 0 && sender.try_send(Event::NativeSummary(generation.clone(), omitted)).is_ok() {
                    omitted = 0;
                }
            }
        }
        if sender.is_closed() {
            return;
        }
    }
    if !line.is_empty() {
        omitted = omitted.saturating_add(1);
    }
    if omitted > 0 {
        let _ = sender.try_send(Event::NativeSummary(generation, omitted));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn readonly_enrollment_pins_identity_before_handoff_without_enabling_orders() {
        let mut f = fixture();
        let vendor = tempfile::tempdir().unwrap();
        f.supervisor.config.gateway_home = vendor.path().join("not-installed");
        let path = f.supervisor.dir.join("enroll-config.toml");
        f.supervisor.config.save(&path).unwrap();
        let target = f.supervisor.config.instance("paper").unwrap().clone();
        f.supervisor.state.profile.api_orders = false;
        f.supervisor.enrollment = Some(Enrollment {
            config_path: path.clone(),
            target,
            persistent: false,
        });
        f.supervisor
            .event(Event::Discovery(
                "fixture-generation".into(),
                Ok(vec!["DUOBSERVED".into()]),
            ))
            .await
            .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.instances["paper"].expected_accounts, ["DUOBSERVED"]);
        assert!(config.instances["paper"].enabled);
        assert!(config.instances["paper"].api_orders);
        assert!(!f.supervisor.writable);
        assert!(f.supervisor.enrollment_complete);
        assert!(f.child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn interrupted_enrollment_can_promote_only_the_same_readonly_profile() {
        let mut f = fixture();
        let mut target = f.supervisor.config.instances["paper"].clone();
        target.enabled = true;
        target.expected_accounts = vec!["DUVERIFIED".into()];
        f.supervisor
            .config
            .instances
            .insert("paper".into(), target.clone());
        f.supervisor.state.profile.enabled = true;
        f.supervisor.state.profile.api_orders = false;
        f.supervisor.state.phase = Phase::NeedsAttention;
        assert!(enrollment_promotion(&f.supervisor.state.profile, &target));
        let mut other_mode = target.clone();
        other_mode.mode = crate::config::Mode::Live;
        assert!(!enrollment_promotion(
            &f.supervisor.state.profile,
            &other_mode
        ));
        let (stream, mut receiver) = UnixStream::pair().unwrap();
        f.supervisor.bridge = Some(stream.into_split().1);
        f.supervisor.control("RESUME").await.unwrap();
        let configuration = protocol::read(&mut receiver).await.unwrap();
        assert_eq!(configuration[0], "CONFIG");
        assert_eq!(configuration[4], "true");
        assert_eq!(protocol::read(&mut receiver).await.unwrap(), ["RESUME"]);
        assert!(!f.supervisor.configured);
        assert!(!f.supervisor.writable);
        assert_eq!(f.supervisor.state.profile.expected_accounts, ["DUVERIFIED"]);
    }

    #[tokio::test]
    async fn stop_finishes_standalone_enrollment_but_keeps_service_enrollment_resumable() {
        for persistent in [false, true] {
            let mut f = fixture();
            f.supervisor.enrollment = Some(Enrollment {
                config_path: f.supervisor.dir.join("unused.toml"),
                target: f.supervisor.state.profile.clone(),
                persistent,
            });
            f.supervisor.control("STOP").await.unwrap();
            assert_eq!(f.supervisor.enrollment_cancelled, !persistent);
            assert!(!f.supervisor.state.desired_running);
        }
    }

    #[tokio::test]
    async fn enrollment_does_not_replace_an_existing_pinned_identity() {
        let f = fixture();
        let mut config = f.supervisor.config.clone();
        config.instances.get_mut("paper").unwrap().expected_accounts = vec!["DUKNOWN".into()];
        let error = enroll(config, "paper", f.supervisor.dir.join("unused.toml"), false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("pinned account identities"));
    }

    #[test]
    fn native_deadline_is_relative_to_scheduled_time_not_request_time() {
        let policy = crate::config::Recovery::default();
        let now = Instant::now();
        assert_eq!(
            native_deadline(now, 1000, Some(1180), &policy) - now,
            Duration::from_secs(300)
        );
        assert_eq!(native_deadline(now, 1500, Some(1180), &policy), now);
    }

    #[tokio::test]
    async fn expired_native_restart_preserves_the_process_and_session() {
        let mut f = fixture();
        f.supervisor.state.phase = Phase::NativeRestarting;
        f.supervisor.restart_deadline = Instant::now() - Duration::from_secs(1);
        let (sender, _) = mpsc::channel(1);
        f.supervisor.tick(sender).await.unwrap();
        assert_eq!(f.supervisor.state.phase, Phase::NeedsAttention);
        assert!(f.child.try_wait().unwrap().is_none());
        assert!(f.supervisor.state.owner.is_some());
    }

    #[tokio::test]
    async fn ui_stall_with_successful_api_does_not_kill_or_report_ready() {
        let mut f = fixture();
        f.supervisor.last_pulse = Instant::now() - Duration::from_secs(180);
        f.supervisor.writable = true;
        f.supervisor
            .event(Event::Health("fixture-generation".into(), Ok(())))
            .await
            .unwrap();
        assert_eq!(f.supervisor.state.phase, Phase::Reconnecting);
        assert_eq!(f.supervisor.state.restarts, 0);
        assert!(f.child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn api_order_configuration_has_a_deadline() {
        let mut f = fixture();
        f.supervisor.state.phase = Phase::Configuring;
        f.supervisor.orders_deadline = Some(Instant::now() - Duration::from_secs(1));
        let (sender, _) = mpsc::channel(1);
        f.supervisor.tick(sender).await.unwrap();
        assert_eq!(
            f.supervisor.state.reason,
            "api_order_configuration_deadline_exceeded"
        );
        assert!(f.child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn transient_client_conflict_does_not_disable_monitoring() {
        let mut f = fixture();
        f.supervisor
            .event(Event::Health(
                "fixture-generation".into(),
                Err(health::Issue::ClientConflict.into()),
            ))
            .await
            .unwrap();
        assert_eq!(f.supervisor.state.phase, Phase::Reconnecting);
        assert_eq!(f.supervisor.state.restarts, 0);
    }

    #[tokio::test]
    async fn start_is_idempotent_during_mfa_and_ready_states() {
        let mut f = fixture();
        f.supervisor.state.restarts = 2;
        f.supervisor.control("START").await.unwrap();
        assert_eq!(f.supervisor.state.phase, Phase::Ready);
        assert_eq!(f.supervisor.state.restarts, 2);
        f.supervisor.state.phase = Phase::AwaitingMfa;
        f.supervisor.control("START").await.unwrap();
        assert_eq!(f.supervisor.state.phase, Phase::AwaitingMfa);
    }

    #[tokio::test]
    async fn an_expired_interrupted_launch_can_be_acknowledged() {
        let mut f = fixture();
        f.supervisor.state.owner = None;
        f.supervisor.state.launch_pending = true;
        f.supervisor.state.phase = Phase::NeedsAttention;
        f.supervisor.state.reason = "interrupted_launch_requires_ownership_reconciliation".into();
        f.supervisor.control("RESUME").await.unwrap();
        assert!(!f.supervisor.state.launch_pending);
        assert_eq!(f.supervisor.state.phase, Phase::Starting);
    }

    #[tokio::test]
    async fn foreign_restart_marker_is_quarantined_not_a_crash_loop() {
        let mut f = fixture();
        f.child.kill().unwrap();
        f.child.wait().unwrap();
        ownership::atomic_write(
            &f.supervisor.dir.join("restart.request"),
            b"foreign\nsession\n",
        )
        .unwrap();
        let (sender, _) = mpsc::channel(1);
        f.supervisor.tick(sender).await.unwrap();
        assert_eq!(
            f.supervisor.state.reason,
            "native_restart_marker_quarantined"
        );
        assert!(f.supervisor.dir.join("restart.rejected").exists());
        assert!(!f.supervisor.dir.join("restart.request").exists());
        assert_eq!(f.supervisor.state.restarts, 0);
    }

    #[tokio::test]
    async fn native_restart_marker_cannot_clear_an_intervention_state() {
        let mut f = fixture();
        f.supervisor.state.phase = Phase::NeedsAttention;
        f.supervisor.state.reason = "unrecognized_modal_dialog".into();
        f.child.kill().unwrap();
        f.child.wait().unwrap();
        ownership::atomic_write(
            &f.supervisor.dir.join("restart.request"),
            b"fixture-generation\nsession\n",
        )
        .unwrap();
        let (sender, _) = mpsc::channel(1);
        f.supervisor.tick(sender).await.unwrap();
        assert_eq!(f.supervisor.state.phase, Phase::NeedsAttention);
        assert_eq!(f.supervisor.state.restarts, 0);
        assert_eq!(
            f.supervisor.state.resume_session.as_deref(),
            Some("session")
        );
        let saved: State = serde_json::from_slice(
            &ownership::read_private(&f.supervisor.dir.join("state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.resume_session.as_deref(), Some("session"));
    }

    #[tokio::test]
    async fn a_readonly_login_cannot_supply_a_resume_token_for_trading() {
        let mut f = fixture();
        f.supervisor
            .event(bridge(
                "needs_attention",
                "read_only_login_cannot_provide_trading",
            ))
            .await
            .unwrap();
        assert!(f.supervisor.state.require_full_login);
        f.child.kill().unwrap();
        f.child.wait().unwrap();
        ownership::atomic_write(
            &f.supervisor.dir.join("restart.request"),
            b"fixture-generation\nreadonly-session\n",
        )
        .unwrap();
        let (sender, _) = mpsc::channel(1);
        f.supervisor.tick(sender).await.unwrap();
        assert!(f.supervisor.state.require_full_login);
        assert!(f.supervisor.state.resume_session.is_none());
        assert_eq!(f.supervisor.state.phase, Phase::NeedsAttention);
    }

    #[tokio::test]
    async fn resume_invalidates_prior_configuration_and_inflight_health_results() {
        let mut f = fixture();
        f.supervisor.writable = true;
        f.supervisor.state.phase = Phase::NeedsAttention;
        f.supervisor.state.reason = "configuration_stalled".into();
        f.supervisor.control("RESUME").await.unwrap();
        assert!(!f.supervisor.configured);
        assert!(!f.supervisor.writable);
        f.supervisor
            .event(Event::Health("fixture-generation".into(), Ok(())))
            .await
            .unwrap();
        assert_eq!(f.supervisor.state.phase, Phase::Starting);
        assert!(!f.supervisor.enabling_orders);
    }

    #[tokio::test]
    async fn disconnected_bridge_command_is_an_explicit_intervention_not_a_crash() {
        let mut f = fixture();
        let error = f
            .supervisor
            .event(Event::Health("fixture-generation".into(), Ok(())))
            .await
            .unwrap_err();
        f.supervisor.runtime_failure(error).unwrap();
        assert_eq!(
            f.supervisor.state.reason,
            "supervisor_io_requires_attention"
        );
        assert!(f.child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn bridge_attachment_rejects_wrong_generation_and_peer_pid() {
        let mut f = fixture();
        let (sender, _) = mpsc::channel(1);
        for (generation, pid, expected) in [
            ("wrong", std::process::id(), "generation mismatch"),
            (
                "fixture-generation",
                f.child.id(),
                "IPC peer is not the owned Gateway",
            ),
        ] {
            let (left, mut right) = UnixStream::pair().unwrap();
            protocol::write(
                &mut right,
                &[
                    "HELLO",
                    protocol::BRIDGE_VERSION,
                    &pid.to_string(),
                    generation,
                ],
            )
            .await
            .unwrap();
            let error = f.supervisor.attach(left, sender.clone()).await.unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[tokio::test]
    async fn a_pending_launch_is_adopted_only_with_matching_executable_and_generation() {
        let mut f = fixture();
        let install = &f.supervisor.config.gateway_home;
        let bin = install.join(".install4j/jre.bundle/Contents/Home/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir(install.join("jars")).unwrap();
        fs::write(install.join(".install4j/i4jruntime.jar"), []).unwrap();
        fs::write(
            install.join(".install4j/i4jparams.conf"),
            "<root><variable name=\"javaOptions\" value=\"\"/></root>",
        )
        .unwrap();
        fs::write(install.join("ibgateway.vmoptions"), "").unwrap();
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), bin.join("java")).unwrap();
        f.supervisor.state.owner = None;
        f.supervisor.state.launch_pending = true;
        let (left, mut right) = UnixStream::pair().unwrap();
        protocol::write(
            &mut right,
            &[
                "HELLO",
                protocol::BRIDGE_VERSION,
                &std::process::id().to_string(),
                "fixture-generation",
            ],
        )
        .await
        .unwrap();
        let (sender, _receiver) = mpsc::channel(4);
        f.supervisor.attach(left, sender).await.unwrap();
        assert_eq!(protocol::read(&mut right).await.unwrap()[0], "CONFIG");
        assert_eq!(
            f.supervisor.state.owner.as_ref().unwrap().pid,
            std::process::id() as i32
        );
        assert!(!f.supervisor.state.launch_pending);
        f.supervisor.reader.take().unwrap().abort();
    }

    #[tokio::test]
    async fn native_log_flood_does_not_backpressure_the_child() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(4096);
        let (sender, _receiver) = mpsc::channel(1);
        let task = tokio::spawn(native_output(
            Box::pin(reader),
            sender,
            "fixture-generation".into(),
        ));
        timeout(Duration::from_secs(3), async {
            for _ in 0..5000 {
                writer
                    .write_all(b"arbitrary vendor output that must not be copied\n")
                    .await
                    .unwrap();
            }
            drop(writer);
            task.await.unwrap();
        })
        .await
        .unwrap();
    }

    struct Fixture {
        supervisor: Supervisor,
        child: std::process::Child,
        _directory: tempfile::TempDir,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let mut config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.state_dir = directory.path().into();
        config.gateway_home = directory.path().join("not-installed");
        let dir = config.directory("paper");
        ownership::private_dir(&dir).unwrap();
        let log = Events::new("paper", &dir, None).unwrap();
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut state = State::new(config.instances["paper"].clone());
        state.owner = Identity::read(child.id() as i32).unwrap();
        state.generation = "fixture-generation".into();
        state.phase = Phase::Ready;
        let now = Instant::now();
        Fixture {
            supervisor: Supervisor {
                config,
                name: "paper".into(),
                dir,
                state,
                log,
                child: None,
                bridge: None,
                reader: None,
                probe: None,
                last_pulse: now,
                started: now,
                healthy_since: None,
                next_start: now,
                next_probe: now,
                failures: 0,
                configured: true,
                writable: false,
                enabling_orders: false,
                resuming_session: false,
                next_notification: now,
                next_heartbeat: now,
                next_legacy_check: now + Duration::from_secs(3600),
                restart_deadline: now + Duration::from_secs(300),
                orders_deadline: None,
                api_unresponsive_failures: 0,
                enrollment: None,
                enrollment_complete: false,
                enrollment_cancelled: false,
            },
            child,
            _directory: directory,
        }
    }

    fn bridge(state: &str, reason: &str) -> Event {
        Event::Bridge(
            "fixture-generation".into(),
            vec!["STATE".into(), state.into(), reason.into()],
        )
    }

    #[tokio::test]
    async fn duplicate_mfa_state_is_not_a_notification_storm() {
        let mut fixture = fixture();
        let s = &mut fixture.supervisor;
        s.event(bridge("awaiting_mfa", "required")).await.unwrap();
        s.event(bridge("awaiting_mfa", "required")).await.unwrap();
        assert_eq!(s.state.phase, Phase::AwaitingMfa);
        let logs = fs::read_to_string(s.dir.join("logs/controller.jsonl")).unwrap();
        assert_eq!(logs.lines().count(), 1);
        assert_eq!(s.state.restarts, 0);
    }

    #[tokio::test]
    async fn explicit_intervention_cannot_be_cleared_by_stale_bridge_state() {
        let mut fixture = fixture();
        let s = &mut fixture.supervisor;
        s.transition(Phase::NeedsAttention, "unrecognized_modal_dialog", true)
            .unwrap();
        s.event(bridge("configured_writable", "old_state"))
            .await
            .unwrap();
        assert_eq!(s.state.phase, Phase::NeedsAttention);
        s.event(Event::Health("old-generation".into(), Ok(())))
            .await
            .unwrap();
        assert_eq!(s.state.phase, Phase::NeedsAttention);
    }

    #[tokio::test]
    async fn upstream_loss_does_not_restart_a_healthy_jvm() {
        let mut fixture = fixture();
        let s = &mut fixture.supervisor;
        for _ in 0..5 {
            s.event(Event::Health(
                "fixture-generation".into(),
                Err(health::Issue::UpstreamDisconnected.into()),
            ))
            .await
            .unwrap();
        }
        assert_eq!(s.state.phase, Phase::Reconnecting);
        assert_eq!(s.state.restarts, 0);
        assert!(fixture.child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn order_access_is_enabled_only_after_identity_check_and_rechecked() {
        let mut fixture = fixture();
        let s = &mut fixture.supervisor;
        let (stream, mut receiver) = UnixStream::pair().unwrap();
        let (_, writer) = stream.into_split();
        s.bridge = Some(writer);
        s.event(Event::Health("fixture-generation".into(), Ok(())))
            .await
            .unwrap();
        assert_eq!(
            protocol::read(&mut receiver).await.unwrap(),
            ["ENABLE_ORDERS"]
        );
        assert_eq!(s.state.phase, Phase::Configuring);
        s.event(bridge("configured_writable", "settings_verified"))
            .await
            .unwrap();
        assert_eq!(s.state.phase, Phase::Configuring);
        s.event(Event::Health("fixture-generation".into(), Ok(())))
            .await
            .unwrap();
        assert_eq!(s.state.phase, Phase::Ready);
    }

    #[tokio::test]
    async fn wrong_account_stops_only_the_owned_gateway() {
        let mut fixture = fixture();
        let mut unrelated = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let result = fixture
            .supervisor
            .event(Event::Health(
                "fixture-generation".into(),
                Err(health::Issue::AccountMismatch.into()),
            ))
            .await;
        let unrelated_alive = unrelated.try_wait().unwrap().is_none();
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        result.unwrap();
        fixture.child.wait().unwrap();
        assert!(unrelated_alive);
        assert_eq!(fixture.supervisor.state.phase, Phase::NeedsAttention);
        assert!(fixture.supervisor.state.owner.is_none());
    }

    #[test]
    fn pending_native_resume_survives_state_serialization() {
        let mut fixture = fixture();
        let s = &mut fixture.supervisor;
        s.state.resume_session = Some("opaque-session".into());
        s.schedule_restart(true, "native_restart_pending").unwrap();
        let saved: State =
            serde_json::from_slice(&ownership::read_private(&s.dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(saved.resume_session.as_deref(), Some("opaque-session"));
        assert!(saved.backoff_until_unix >= time::OffsetDateTime::now_utc().unix_timestamp());
        assert_eq!(saved.restarts, 1);
    }
}
