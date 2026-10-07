use crate::{
    config::{self, Config, Mode},
    events::Events,
    gateway, health,
    ownership::{self, Lock},
    service,
    state::{Phase, State},
    supervisor,
};
use anyhow::{bail, ensure, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::{sleep, timeout, Instant},
};

const IBKR_REQUIREMENT: &str =
    "=anchor apple generic and certificate leaf[subject.OU] = \"L6H6D9Q7DY\"";
const RESPONSE: &str = "executeLauncherAction$Boolean=false\n\
    sys.installForAllUsers$Boolean=false\n\
    sys.adminRights$Boolean=false\n";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Latest,
}

impl Channel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Latest => "latest",
        }
    }

    fn url(self) -> String {
        let channel = self.as_str();
        format!("https://download2.interactivebrokers.com/installers/ibgateway/{channel}-standalone/ibgateway-{channel}-standalone-macos-arm.dmg")
    }
}

#[derive(Args)]
pub struct Selection {
    #[arg(long)]
    instance: String,
    #[arg(long, value_enum, default_value = "latest")]
    channel: Channel,
}

#[derive(Subcommand)]
pub enum Action {
    /// Download and inspect the official signed installer without changing the running Gateway.
    Check {
        #[command(flatten)]
        selection: Selection,
    },
    /// Install a newer release alongside the old one, then restart only the selected Paper service.
    Run {
        #[command(flatten)]
        selection: Selection,
        /// Explicitly retry a release that failed qualification previously.
        #[arg(long)]
        retry_failed: bool,
        /// Reinstall the same version to repair or qualify the complete upgrade path.
        #[arg(long)]
        reinstall: bool,
        /// Restrict routine upgrades to a daily one-hour window; retirement incidents are urgent.
        #[arg(long)]
        scheduled_at: Option<String>,
    },
    /// Enable a daily Aqua LaunchAgent; does not replace the authorized supervisor.
    Install {
        #[command(flatten)]
        selection: Selection,
        /// Maintenance time in this Mac's local timezone.
        #[arg(long, default_value = "05:00")]
        at: String,
    },
    /// Remove only the scheduled updater, not the Gateway service.
    Uninstall {
        #[arg(long)]
        instance: String,
    },
    /// Show persisted upgrade state without contacting IBKR.
    Status {
        #[arg(long)]
        instance: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Release {
    name: String,
    version: String,
}

fn version_key(version: &str) -> Result<(u32, u32, u32, u8)> {
    ensure!(
        version.is_ascii() && (5..=12).contains(&version.len()),
        "unsupported vendor version format"
    );
    let (digits, revision) = match version.as_bytes().last() {
        Some(last) if last.is_ascii_lowercase() => {
            (&version[..version.len() - 1], *last - b'a' + 1)
        }
        _ => (version, 0),
    };
    ensure!(
        digits.len() >= 5 && digits.bytes().all(|c| c.is_ascii_digit()),
        "unsupported vendor version format"
    );
    Ok((
        digits[..2].parse()?,
        digits[2..4].parse()?,
        digits[4..].parse()?,
        revision,
    ))
}

fn release_from_xml(xml: &str) -> Result<Release> {
    let doc = roxmltree::Document::parse(xml).context("parse vendor release metadata")?;
    let general = doc
        .descendants()
        .find(|n| n.has_tag_name("general"))
        .context("vendor identity missing")?;
    ensure!(
        general.attribute("publisherName") == Some("Interactive Brokers LLC"),
        "unexpected vendor publisher"
    );
    ensure!(
        general
            .attribute("mediaName")
            .is_some_and(|v| v.starts_with("ibgateway-") && v.ends_with("-macos-arm")),
        "only the IB Gateway Apple silicon installer is supported"
    );
    let version = doc
        .descendants()
        .find(|n| n.attribute("name") == Some("fullVersion"))
        .and_then(|n| n.attribute("value"))
        .context("vendor fullVersion metadata missing")?;
    let (major, minor, _, _) = version_key(version)?;
    let name = general
        .attribute("applicationName")
        .context("vendor application name missing")?;
    ensure!(
        name == format!("IB Gateway {major}.{minor:02}"),
        "unsupported Gateway installation layout"
    );
    Ok(Release {
        name: name.into(),
        version: version.into(),
    })
}

fn installed_release(home: &Path) -> Result<Release> {
    release_from_xml(&fs::read_to_string(home.join(".install4j/i4jparams.conf"))?)
}

#[derive(Clone, Deserialize, Serialize)]
struct Transaction {
    original_config: String,
    updated_config: String,
    original_home: PathBuf,
    candidate_home: PathBuf,
    settings_backup: PathBuf,
    settings_saved: bool,
    start_requested: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct UpdateState {
    phase: String,
    reason: String,
    checked_at: i64,
    current: Option<Release>,
    available: Option<Release>,
    failed_version: Option<String>,
    transaction: Option<Transaction>,
    notification_pending: bool,
    notification_human: bool,
}

#[derive(Deserialize)]
struct Status {
    instance: String,
    phase: Phase,
    reason: String,
    desired_running: bool,
    gateway_pid: Option<i32>,
    #[serde(default)]
    upgrade_recommended: bool,
}

trait Control {
    async fn request(&self, config: &Config, instance: &str, action: &str) -> Result<String>;
    async fn unload(&self, instance: &str) -> Result<()>;
    async fn stop_unreachable(&self, config: &Config, instance: &str) -> Result<()>;
    async fn preflight(
        &self,
        program: &Path,
        config: &Path,
        instance: &str,
        probe: &Path,
    ) -> Result<()>;
    async fn start(&self, program: &Path, config: &Path, instance: &str) -> Result<()>;
    async fn wait_ready(&self, config: &Config, instance: &str) -> Result<Readiness>;
}

struct NativeControl;

impl Control for NativeControl {
    async fn request(&self, config: &Config, instance: &str, action: &str) -> Result<String> {
        supervisor::request(config, instance, action).await
    }

    async fn unload(&self, instance: &str) -> Result<()> {
        service::unload(instance).await
    }

    async fn stop_unreachable(&self, config: &Config, instance: &str) -> Result<()> {
        supervisor::stop_unreachable(config, instance).await
    }

    async fn preflight(
        &self,
        program: &Path,
        config: &Path,
        instance: &str,
        probe: &Path,
    ) -> Result<()> {
        command(
            program,
            &[
                "--config".into(),
                config.as_os_str().into(),
                "credentials".into(),
                "--instance".into(),
                instance.into(),
                "--check-access".into(),
            ],
            15,
        )
        .await
        .context("existing supervisor needs unattended Keychain access in this GUI session")?;
        command(
            program,
            &["--config".into(), probe.as_os_str().into(), "doctor".into()],
            30,
        )
        .await
        .context("existing authorized bridge is incompatible with the candidate installation")?;
        Ok(())
    }

    async fn start(&self, program: &Path, config: &Path, instance: &str) -> Result<()> {
        command(
            program,
            &[
                "--config".into(),
                config.as_os_str().into(),
                "start".into(),
                "--instance".into(),
                instance.into(),
            ],
            60,
        )
        .await?;
        Ok(())
    }

    async fn wait_ready(&self, config: &Config, instance: &str) -> Result<Readiness> {
        let deadline =
            Instant::now() + Duration::from_secs(config.recovery.startup_timeout_secs + 60);
        let mut healthy = None;
        loop {
            let status: Status =
                serde_json::from_str(&self.request(config, instance, "STATUS").await?)?;
            ensure!(
                status.instance == instance,
                "unexpected supervisor status instance"
            );
            match readiness(&status) {
                Readiness::Ready => {
                    let state: State = serde_json::from_slice(&ownership::read_private(
                        &config.directory(instance).join("state.json"),
                    )?)?;
                    let owner = state.owner.context("ready Gateway has no recorded owner")?;
                    ensure!(
                        Some(owner.pid) == status.gateway_pid
                            && gateway::expected_executable(
                                config,
                                instance,
                                &owner.executable()?
                            )?,
                        "ready Gateway does not belong to the selected installation"
                    );
                    health::verify_port_owner(config.instance(instance)?.api_port, owner.pid)
                        .await?;
                    let since = healthy.get_or_insert_with(Instant::now);
                    if since.elapsed()
                        >= Duration::from_secs(config.recovery.check_interval_secs.min(30) * 2)
                    {
                        return Ok(Readiness::Ready);
                    }
                }
                Readiness::Waiting => healthy = None,
                outcome => return Ok(outcome),
            }
            if Instant::now() >= deadline {
                return Ok(Readiness::Failed(
                    "candidate_readiness_deadline_exceeded".into(),
                ));
            }
            sleep(Duration::from_secs(2)).await;
        }
    }
}

struct Updater<C = NativeControl> {
    config: Config,
    config_path: PathBuf,
    instance: String,
    root: PathBuf,
    directory: PathBuf,
    state: UpdateState,
    events: Events,
    control: C,
    _lock: Lock,
}

async fn command(program: &Path, args: &[OsString], seconds: u64) -> Result<String> {
    async fn read_bounded(stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stream.take(1024 * 1024 + 1).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "command output exceeded its limit"
        );
        Ok(bytes)
    }
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("JAVA_TOOL_OPTIONS")
        .env_remove("_JAVA_OPTIONS")
        .env_remove("JDK_JAVA_OPTIONS")
        .env_remove("INSTALL4J_ARGUMENTS")
        .env_remove("INSTALL4J_JAVA_HOME_OVERRIDE")
        .env_remove("INSTALL4J_ADD_VM_PARAMS")
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().context("command PID missing")? as i32;
    let stdout = child.stdout.take().context("command stdout missing")?;
    let stderr = child.stderr.take().context("command stderr missing")?;
    let output = async {
        tokio::try_join!(
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            read_bounded(stdout),
            read_bounded(stderr),
        )
    };
    tokio::pin!(output);
    let result = timeout(Duration::from_secs(seconds), &mut output).await;
    let (status, stdout, stderr) = match result {
        Ok(Ok(output)) => output,
        error => {
            // The command and its installer helpers have their own process group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            if error.is_err() {
                let _ = timeout(Duration::from_secs(5), &mut output).await;
            }
            return match error {
                Ok(Err(error)) => Err(error),
                _ => bail!(
                    "{} timed out; its process group was terminated",
                    program.display()
                ),
            };
        }
    };
    ensure!(
        status.success(),
        "{} failed ({}): {}",
        program.display(),
        status,
        format!(
            "{}{}",
            String::from_utf8_lossy(&stderr),
            String::from_utf8_lossy(&stdout)
        )
        .chars()
        .take(2048)
        .collect::<String>()
    );
    String::from_utf8(stdout).context("invalid command output encoding")
}

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

async fn stopped_supervisor_lock(directory: &Path) -> Result<Lock> {
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        match Lock::acquire(&directory.join("supervisor.lock")) {
            Ok(lock) => return Ok(lock),
            Err(error) if error.is::<ownership::LockHeld>() && Instant::now() < deadline => {
                sleep(Duration::from_millis(100)).await;
            }
            Err(error) => {
                return Err(error).context("wait for the stopped supervisor to release ownership")
            }
        }
    }
}

fn supervisor_program(plist: &str, config_path: &Path, instance: &str) -> Result<PathBuf> {
    let doc = roxmltree::Document::parse_with_options(
        plist,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .context("parse supervisor LaunchAgent")?;
    let array = doc
        .descendants()
        .find(|n| n.has_tag_name("key") && n.text() == Some("ProgramArguments"))
        .and_then(|n| n.next_sibling_element())
        .filter(|n| n.has_tag_name("array"))
        .context("supervisor ProgramArguments missing")?;
    let values: Vec<_> = array
        .children()
        .filter(|n| n.is_element())
        .map(|n| n.text().context("invalid supervisor argument"))
        .collect::<Result<_>>()?;
    ensure!(
        (values.len() == 6 || (values.len() == 7 && values[6] == "--enroll-if-unbound"))
            && values[1] == "--config"
            && Path::new(values[2]) == config_path
            && values[3..6] == ["run", "--instance", instance],
        "supervisor service does not use this configuration and instance"
    );
    let program = PathBuf::from(values[0]);
    ensure!(
        program.is_absolute(),
        "supervisor executable must be absolute"
    );
    Ok(program)
}

impl Updater {
    fn open(config: Config, path: &Path, instance: &str) -> Result<Self> {
        config.instance(instance)?;
        ownership::private_dir(&config.state_dir)?;
        let root = config.state_dir.join(".upgrades");
        ownership::private_dir(&root)?;
        let lock = Lock::acquire(&root.join("upgrade.lock"))?;
        let directory = root.join(instance);
        ownership::private_dir(&directory)?;
        let state_path = directory.join("state.json");
        let state = if state_path.try_exists()? {
            serde_json::from_slice(&ownership::read_private(&state_path)?)
                .context("invalid upgrade journal; inspect it rather than clearing it")?
        } else {
            UpdateState {
                phase: "idle".into(),
                ..UpdateState::default()
            }
        };
        let events = Events::new(instance, &directory, config.tradebus_events.clone())?;
        Ok(Self {
            config,
            config_path: path.into(),
            instance: instance.into(),
            root,
            directory,
            state,
            events,
            control: NativeControl,
            _lock: lock,
        })
    }
}

impl<C: Control> Updater<C> {
    fn save(&self) -> Result<()> {
        ownership::atomic_write(
            &self.directory.join("state.json"),
            &serde_json::to_vec(&self.state)?,
        )
    }

    fn report(&mut self, phase: &str, reason: &str, human: bool) -> Result<()> {
        let changed = self.state.phase != phase || self.state.reason != reason;
        self.state.phase = phase.into();
        self.state.reason = reason.into();
        if changed {
            self.state.notification_pending = true;
            self.state.notification_human = human;
        }
        self.save()?;
        self.deliver_notification()
    }

    fn deliver_notification(&mut self) -> Result<()> {
        if self.state.notification_pending {
            match self.events.upgrade_status(
                &self.state.phase,
                &self.state.reason,
                self.state.notification_human,
            ) {
                Ok(()) => {
                    self.state.notification_pending = false;
                    self.save()?;
                }
                Err(error) => self
                    .events
                    .log("upgrade_notification_error", &format!("{error:#}"))?,
            }
        }
        Ok(())
    }

    fn public_status(&self) -> serde_json::Value {
        public_status(&self.instance, &self.state)
    }

    async fn status(&self) -> Result<Status> {
        let response = self
            .control
            .request(&self.config, &self.instance, "STATUS")
            .await?;
        let status: Status = serde_json::from_str(&response)?;
        ensure!(
            status.instance == self.instance,
            "unexpected supervisor status instance"
        );
        Ok(status)
    }

    fn program(&self) -> Result<PathBuf> {
        let path = service::file(&self.instance)?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o022 == 0,
            "supervisor LaunchAgent must be an owned regular file not writable by others"
        );
        let program = supervisor_program(
            &fs::read_to_string(path)?,
            &self.config_path,
            &self.instance,
        )?;
        let metadata = fs::metadata(&program)?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o022 == 0,
            "supervisor executable must be owned and not writable by others"
        );
        Ok(program)
    }

    async fn others_stopped(&self) -> Result<()> {
        for (name, profile) in &self.config.instances {
            if name == &self.instance {
                continue;
            }
            match self.control.request(&self.config, name, "STATUS").await {
                Ok(response) => {
                    let status: Status = serde_json::from_str(&response)?;
                    ensure!(
                        !status.desired_running
                            && status.gateway_pid.is_none()
                            && status.phase == Phase::Stopped,
                        "another instance shares gateway_home; stop {name} before upgrading"
                    );
                }
                Err(error) => {
                    let state_path = self.config.directory(name).join("state.json");
                    if !state_path.try_exists()? && !profile.enabled {
                        continue;
                    }
                    let state: State = serde_json::from_slice(
                        &ownership::read_private(&state_path).with_context(|| {
                            format!("cannot establish that {name} is stopped: {error:#}")
                        })?,
                    )?;
                    ensure!(
                        !state.desired_running
                            && state.owner.is_none()
                            && state.phase == Phase::Stopped,
                        "unreachable instance {name} is not verifiably stopped"
                    );
                }
            }
        }
        Ok(())
    }

    async fn eligible(&self, status: &Status) -> Result<bool> {
        if !status.desired_running {
            return Ok(false);
        }
        if status.phase == Phase::Ready {
            return Ok(true);
        }
        if status.phase == Phase::NeedsAttention
            && status.gateway_pid.is_none()
            && matches!(
                status.reason.as_str(),
                "unrecognized_modal_dialog"
                    | "gateway_exited_during_authentication_or_intervention"
            )
        {
            return legacy_held_retirement(&self.controller_log()?);
        }
        if status.phase == Phase::NeedsAttention && status.reason == "unrecognized_modal_dialog" {
            // Older authorized supervisors do not expose upgrade_recommended.
            let since = OffsetDateTime::now_utc().format(&Rfc3339)?;
            self.control
                .request(&self.config, &self.instance, "DIAGNOSE")
                .await?;
            sleep(Duration::from_secs(2)).await;
            let log = self.controller_log()?;
            return legacy_retirement_notice(&log, &since);
        }
        Ok(status.upgrade_recommended && status.phase == Phase::Reconnecting)
    }

    fn controller_log(&self) -> Result<String> {
        let path = self
            .config
            .directory(&self.instance)
            .join("logs/controller.jsonl");
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.len() <= 3 * 1024 * 1024,
            "controller log must be private, owned, regular, and bounded"
        );
        fs::read_to_string(path).context("read controller upgrade diagnostics")
    }

    async fn download(&self, channel: Channel) -> Result<PathBuf> {
        let cache = self.root.join("downloads");
        ownership::private_dir(&cache)?;
        let archive = cache.join(format!("{}.dmg", channel.as_str()));
        let etag = archive.with_extension("etag");
        let remote_time = archive.with_extension("remote-time");
        let part = cache.join(format!("{}.part", ownership::nonce()?));
        let new_etag = part.with_extension("etag");
        let mut args = arguments(&[
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "30",
            "--max-time",
            "600",
            "--max-filesize",
            "1073741824",
            "--remote-time",
            "--write-out",
            "%{http_code}",
        ]);
        args.extend([
            OsString::from("--output"),
            part.clone().into_os_string(),
            "--etag-save".into(),
            new_etag.clone().into_os_string(),
        ]);
        if archive.try_exists()? && etag.try_exists()? {
            ensure!(
                fs::symlink_metadata(&archive)?.is_file() && fs::symlink_metadata(&etag)?.is_file(),
                "invalid download cache"
            );
            args.extend(["--etag-compare".into(), etag.clone().into_os_string()]);
        }
        // IBKR's CDN can ignore ETags while honoring Last-Modified.
        if archive.try_exists()? && remote_time.try_exists()? {
            let recorded = ownership::read_private(&remote_time)?;
            if recorded == fs::metadata(&archive)?.mtime().to_string().as_bytes() {
                args.extend(["--time-cond".into(), archive.clone().into_os_string()]);
            }
        }
        args.push(channel.url().into());
        let result = command(Path::new("/usr/bin/curl"), &args, 620).await;
        let outcome = (|| -> Result<()> {
            let code = result?;
            match code.as_str() {
                "200" => {
                    fs::set_permissions(&part, fs::Permissions::from_mode(0o600))?;
                    fs::rename(&part, &archive)?;
                    ownership::atomic_write(
                        &remote_time,
                        fs::metadata(&archive)?.mtime().to_string().as_bytes(),
                    )?;
                    if new_etag.try_exists()? {
                        fs::set_permissions(&new_etag, fs::Permissions::from_mode(0o600))?;
                        fs::rename(&new_etag, &etag)?;
                    }
                }
                "304" => ensure!(
                    archive.is_file(),
                    "server returned not-modified without a cached installer"
                ),
                _ => bail!("unexpected installer HTTP status {code}"),
            }
            Ok(())
        })();
        for path in [&part, &new_etag] {
            if path.try_exists()? {
                fs::remove_file(path)?;
            }
        }
        outcome?;
        Ok(archive)
    }

    async fn prepare(
        &mut self,
        channel: Channel,
        check: bool,
        reinstall: bool,
        retry: bool,
    ) -> Result<Option<PathBuf>> {
        let current = installed_release(&self.config.gateway_home)?;
        self.state.current = Some(current.clone());
        self.state.checked_at = OffsetDateTime::now_utc().unix_timestamp();
        self.save()?;
        let archive = self.download(channel).await?;
        let mount = self.root.join("mounted-installer");
        ownership::private_dir(&mount)?;
        command(
            Path::new("/usr/bin/hdiutil"),
            &[
                "attach".into(),
                "-readonly".into(),
                "-nobrowse".into(),
                "-noautoopen".into(),
                "-mountpoint".into(),
                mount.clone().into_os_string(),
                archive.into_os_string(),
            ],
            120,
        )
        .await?;
        let result = self
            .prepare_mounted(&mount, &current, check, reinstall, retry)
            .await;
        let detached = command(
            Path::new("/usr/bin/hdiutil"),
            &["detach".into(), mount.into_os_string()],
            60,
        )
        .await;
        match (result, detached) {
            (Ok(value), Ok(_)) => Ok(value),
            (Err(error), Ok(_)) => Err(error),
            (Ok(_), Err(error)) => Err(error).context("detach private installer mount"),
            (Err(error), Err(cleanup)) => {
                Err(error).context(format!("installer detach also failed: {cleanup:#}"))
            }
        }
    }

    async fn prepare_mounted(
        &mut self,
        mount: &Path,
        current: &Release,
        check: bool,
        reinstall: bool,
        retry: bool,
    ) -> Result<Option<PathBuf>> {
        let apps: Vec<_> = fs::read_dir(mount)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
            .collect();
        ensure!(
            apps.len() == 1,
            "expected exactly one installer application"
        );
        let app = &apps[0];
        command(
            Path::new("/usr/bin/codesign"),
            &[
                "--verify".into(),
                "--deep".into(),
                "--strict".into(),
                "-R".into(),
                IBKR_REQUIREMENT.into(),
                app.clone().into_os_string(),
            ],
            60,
        )
        .await?;
        command(
            Path::new("/usr/sbin/spctl"),
            &[
                "--assess".into(),
                "--type".into(),
                "execute".into(),
                app.clone().into_os_string(),
            ],
            90,
        )
        .await?;
        let release = release_from_xml(&fs::read_to_string(
            app.join("Contents/Resources/app/i4jparams.conf"),
        )?)?;
        self.state.available = Some(release.clone());
        self.save()?;
        let ordering = version_key(&release.version)?.cmp(&version_key(&current.version)?);
        if ordering.is_lt() || (ordering.is_eq() && !reinstall) {
            self.report(
                "current",
                if ordering.is_lt() {
                    "channel_is_older_no_downgrade"
                } else {
                    "installed_version_is_current"
                },
                false,
            )?;
            return Ok(None);
        }
        if check {
            self.report("available", "new_signed_release_available", false)?;
            return Ok(None);
        }
        ensure!(
            retry || self.state.failed_version.as_ref() != Some(&release.version),
            "release {} previously failed; use --retry-failed only after resolving the cause",
            release.version
        );
        self.report(
            "installing",
            "installing_verified_release_side_by_side",
            false,
        )?;
        let managed = config::home()?.join("Applications/gatewayctl-upgrades");
        ownership::private_dir(&managed)?;
        let build = managed.join(format!("{}-{}", release.version, ownership::nonce()?));
        ownership::private_dir(&build)?;
        let candidate = build.join(&release.name);
        let installer_home = build.join("installer-home");
        ownership::private_dir(&installer_home)?;
        ownership::private_dir(&installer_home.join("Desktop"))?;
        let response = build.join("response.varfile");
        ownership::atomic_write(&response, RESPONSE.as_bytes())?;
        let result = command(
            &app.join("Contents/MacOS/JavaApplicationStub"),
            &[
                "-q".into(),
                "-dir".into(),
                candidate.clone().into_os_string(),
                "-varfile".into(),
                response.into_os_string(),
                "-Dinstall4j.suppressUnattendedReboot=true".into(),
                format!("-Duser.home={}", installer_home.display()).into(),
                format!("-Dinstall4j.log={}", build.join("installer.log").display()).into(),
            ],
            600,
        )
        .await;
        if let Err(error) = result {
            self.state.failed_version = Some(release.version);
            self.save()?;
            return Err(error)
                .context("unattended installer failed; existing Gateway was not stopped");
        }
        let verified = (|| -> Result<()> {
            ensure!(
                installed_release(&candidate)? == release,
                "installed version does not match signed installer"
            );
            let response = fs::read_to_string(candidate.join(".install4j/response.varfile"))?;
            ensure!(
                response
                    .lines()
                    .any(|line| line == "executeLauncherAction$Boolean=false")
                    && response
                        .lines()
                        .any(|line| line == "sys.adminRights$Boolean=false")
                    && response
                        .lines()
                        .any(|line| line == "sys.installForAllUsers$Boolean=false"),
                "installer did not retain no-launch/no-elevation settings"
            );
            Ok(())
        })();
        if let Err(error) = verified {
            self.state.failed_version = Some(release.version);
            self.save()?;
            return Err(error);
        }
        Ok(Some(candidate))
    }

    async fn preflight(&self, program: &Path, candidate: &Path) -> Result<()> {
        self.others_stopped().await?;
        let mut probe = self.config.clone();
        probe.gateway_home = candidate.into();
        probe.state_dir = self.directory.join("compatibility");
        probe.tradebus_events = None;
        let path = self.directory.join("compatibility.toml");
        probe.save(&path)?;
        self.control
            .preflight(program, &self.config_path, &self.instance, &path)
            .await
    }

    async fn start(&self, program: &Path) -> Result<()> {
        self.control
            .start(program, &self.config_path, &self.instance)
            .await
    }

    async fn wait_ready(&self) -> Result<Readiness> {
        self.control.wait_ready(&self.config, &self.instance).await
    }

    async fn cutover(&mut self, program: &Path, candidate: PathBuf) -> Result<()> {
        self.preflight(program, &candidate).await?;
        let status = self.status().await?;
        ensure!(
            self.eligible(&status).await?,
            "Paper is no longer eligible for unattended upgrade"
        );
        let original_config = fs::read_to_string(&self.config_path)?;
        let current = Config::load(&self.config_path)?;
        ensure!(
            toml::to_string(&current)? == toml::to_string(&self.config)?,
            "configuration changed while preparing upgrade"
        );
        let mut updated = self.config.clone();
        updated.gateway_home = candidate.clone();
        updated.validate()?;
        let backup = self
            .directory
            .join(format!("settings-{}", ownership::nonce()?));
        let transaction = Transaction {
            original_config,
            updated_config: toml::to_string_pretty(&updated)?,
            original_home: self.config.gateway_home.clone(),
            candidate_home: candidate,
            settings_backup: backup,
            settings_saved: false,
            start_requested: false,
        };
        self.state.transaction = Some(transaction.clone());
        self.report(
            "restarting",
            "restarting_existing_authorized_paper_supervisor",
            false,
        )?;
        let result = async {
            self.control
                .request(&self.config, &self.instance, "STOP")
                .await?;
            self.control.unload(&self.instance).await?;
            let lock = stopped_supervisor_lock(&self.config.directory(&self.instance)).await?;
            self.others_stopped().await?;
            ensure!(
                fs::read_to_string(&self.config_path)? == transaction.original_config,
                "configuration changed before cutover"
            );
            let settings = self.config.directory(&self.instance).join("settings");
            ownership::private_dir(&settings)?;
            command(
                Path::new("/bin/cp"),
                &[
                    "-cR".into(),
                    settings.into_os_string(),
                    transaction.settings_backup.clone().into_os_string(),
                ],
                120,
            )
            .await?;
            self.state
                .transaction
                .as_mut()
                .context("missing transaction")?
                .settings_saved = true;
            self.save()?;
            ownership::atomic_write(&self.config_path, transaction.updated_config.as_bytes())?;
            self.config = updated;
            drop(lock);
            self.state
                .transaction
                .as_mut()
                .context("missing transaction")?
                .start_requested = true;
            self.save()?;
            self.start(program).await?;
            self.wait_ready().await
        }
        .await;
        match result {
            Ok(Readiness::Ready) => self.complete(),
            Ok(Readiness::Approval(reason)) => self.report("needs_attention", &reason, true),
            Ok(Readiness::Stopped) => self.report(
                "needs_attention",
                "operator_stopped_during_upgrade_no_automatic_restart",
                true,
            ),
            Ok(Readiness::Waiting) => unreachable!(),
            Ok(Readiness::Failed(reason)) => self.rollback(program, &reason).await,
            Err(error) => {
                self.events
                    .log("upgrade_cutover_error", &format!("{error:#}"))?;
                self.rollback(program, "candidate_cutover_failed").await
            }
        }
    }

    fn complete(&mut self) -> Result<()> {
        self.state.current = Some(installed_release(&self.config.gateway_home)?);
        if let Some(transaction) = &self.state.transaction {
            if transaction.settings_saved && transaction.settings_backup.try_exists()? {
                ensure!(
                    transaction.settings_backup.parent() == Some(self.directory.as_path()),
                    "invalid settings backup location"
                );
                ownership::private_dir(&transaction.settings_backup)?;
                fs::remove_dir_all(&transaction.settings_backup)
                    .context("remove verified upgrade's temporary settings snapshot")?;
            }
        }
        self.state.transaction = None;
        self.state.failed_version = None;
        self.report("ready", "upgraded_gateway_account_and_api_verified", false)
    }

    async fn rollback(&mut self, program: &Path, reason: &str) -> Result<()> {
        let transaction = self
            .state
            .transaction
            .clone()
            .context("no upgrade transaction to roll back")?;
        self.state.failed_version = self.state.available.as_ref().map(|r| r.version.clone());
        self.save()?;
        self.others_stopped().await?;
        let text = fs::read_to_string(&self.config_path)?;
        ensure!(
            text == transaction.original_config || text == transaction.updated_config,
            "configuration changed externally; rollback requires inspection"
        );
        match self.status().await {
            Ok(status) => {
                ensure!(
                    status.desired_running || !transaction.start_requested,
                    "operator stopped the instance; rollback will not restart it"
                );
                self.control
                    .request(&self.config, &self.instance, "STOP")
                    .await?;
                self.control.unload(&self.instance).await?;
            }
            Err(_) => {
                self.control.unload(&self.instance).await?;
                drop(stopped_supervisor_lock(&self.config.directory(&self.instance)).await?);
                self.control
                    .stop_unreachable(&self.config, &self.instance)
                    .await?;
            }
        }
        let lock = stopped_supervisor_lock(&self.config.directory(&self.instance)).await?;
        if transaction.settings_saved {
            let settings = self.config.directory(&self.instance).join("settings");
            ownership::private_dir(&transaction.settings_backup)?;
            if settings.try_exists()? {
                fs::rename(
                    settings,
                    self.directory
                        .join(format!("candidate-settings-{}", ownership::nonce()?)),
                )?;
            }
            fs::rename(
                &transaction.settings_backup,
                self.config.directory(&self.instance).join("settings"),
            )?;
        }
        ensure!(
            fs::read_to_string(&self.config_path)? == text,
            "configuration changed during rollback"
        );
        ownership::atomic_write(&self.config_path, transaction.original_config.as_bytes())?;
        self.config = Config::load(&self.config_path)?;
        drop(lock);
        self.start(program).await?;
        match self.wait_ready().await? {
            Readiness::Ready => {
                self.state.current = Some(installed_release(&self.config.gateway_home)?);
                self.state.transaction = None;
                self.report("rolled_back", reason, true)
            }
            _ => self.report(
                "needs_attention",
                "rollback_requires_operator_inspection_no_further_login_attempts",
                true,
            ),
        }
    }

    async fn reconcile(&mut self) -> Result<bool> {
        let Some(transaction) = &self.state.transaction else {
            return Ok(false);
        };
        if self.config.gateway_home == transaction.candidate_home {
            ensure!(
                fs::read_to_string(&self.config_path)? == transaction.updated_config,
                "configuration changed during pending upgrade; inspect before accepting it"
            );
            let status = self.status().await?;
            if status.phase == Phase::Ready && matches!(self.wait_ready().await?, Readiness::Ready)
            {
                self.complete()?;
            } else {
                self.report(
                    "needs_attention",
                    "upgrade_pending_approval_or_interrupted_no_automatic_login_retry",
                    true,
                )?;
            }
        } else {
            self.report(
                "needs_attention",
                "interrupted_upgrade_requires_inspection_no_automatic_login_retry",
                true,
            )?;
        }
        Ok(true)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Readiness {
    Waiting,
    Ready,
    Approval(String),
    Failed(String),
    Stopped,
}

fn readiness(status: &Status) -> Readiness {
    if !status.desired_running {
        return Readiness::Stopped;
    }
    match status.phase {
        Phase::Ready => Readiness::Ready,
        Phase::AwaitingMfa => Readiness::Approval(status.reason.clone()),
        Phase::NeedsAttention => {
            if status.reason.starts_with("unsupported_ui_")
                || matches!(
                    status.reason.as_str(),
                    "configuration_stalled"
                        | "startup_preflight_failed"
                        | "api_order_configuration_deadline_exceeded"
                        | "startup_or_login_requires_inspection"
                )
            {
                Readiness::Failed(status.reason.clone())
            } else {
                Readiness::Approval(status.reason.clone())
            }
        }
        _ => Readiness::Waiting,
    }
}

fn public_status(instance: &str, state: &UpdateState) -> serde_json::Value {
    serde_json::json!({
        "instance": instance, "phase": state.phase, "reason": state.reason,
        "checked_at": state.checked_at,
        "installed": state.current, "available": state.available,
        "failed_version": state.failed_version,
        "upgrade_in_progress": state.transaction.is_some(),
        "notification_pending": state.notification_pending,
    })
}

fn legacy_retirement_notice(log: &str, since: &str) -> Result<bool> {
    let diagnostics: Vec<_> = log_events(log)?
        .into_iter()
        .filter(|event| {
            event["kind"] == "dialog_diagnostic"
                && event["ts"].as_str().is_some_and(|ts| ts >= since)
        })
        .filter_map(|event| event["message"].as_str().map(str::to_owned))
        .collect();
    if diagnostics.iter().any(|message| sensitive_dialog(message)) {
        return Ok(false);
    }
    Ok(diagnostics.iter().any(|message| {
        known_retirement_message(message) && message.contains("visible_password_fields=0\n")
    }))
}

fn log_events(log: &str) -> Result<Vec<serde_json::Value>> {
    log.lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("invalid controller diagnostic log; refusing automatic intervention recovery")
}

fn sensitive_dialog(message: &str) -> bool {
    let body = message.to_lowercase();
    [
        "second factor",
        "security code",
        "passkey",
        "existing session",
        "another session",
        "other session",
        "same username",
        "same user name",
        "invalid password",
        "invalid username",
        "verification code",
        "challenge code",
        "response code",
    ]
    .iter()
    .any(|word| body.contains(word))
}

fn known_retirement_message(message: &str) -> bool {
    let body = message
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let buttons: Vec<_> = message
        .lines()
        .filter_map(|line| line.strip_prefix("button: "))
        .collect();
    !sensitive_dialog(message)
        && (message.starts_with("IBKR Gateway\n") || message.starts_with("IB Gateway\n"))
        && body.contains("the version of the application you are running,")
        && body.contains("needs to be upgraded, as it will be desupported on")
        && body.contains("the minimum supported version at that time will be")
        && body.contains("the new version can be downloaded here")
        && !body.contains("no longer supported")
        && !body.contains("has been desupported")
        && buttons == ["OK"]
}

fn legacy_held_retirement(log: &str) -> Result<bool> {
    let mut notice = false;
    let mut held = false;
    for event in log_events(log)? {
        let message = event["message"]
            .as_str()
            .context("controller log message is missing")?;
        if event["kind"] == "dialog_diagnostic" {
            if sensitive_dialog(message) {
                notice = false;
                held = false;
            } else if known_retirement_message(message) {
                notice = true;
            }
        } else if event["kind"] == "transition" {
            match message {
                "needs_attention: unrecognized_modal_dialog" => {
                    held = notice;
                    notice = false;
                }
                "needs_attention: gateway_exited_during_authentication_or_intervention" => {}
                _ => {
                    notice = false;
                    held = false;
                }
            }
        }
    }
    Ok(held)
}

fn schedule(at: &str) -> Result<(u8, u8)> {
    let (hour, minute) = at
        .split_once(':')
        .context("maintenance time must be HH:MM")?;
    ensure!(
        hour.len() == 2 && minute.len() == 2,
        "maintenance time must be HH:MM"
    );
    let (hour, minute): (u8, u8) = (hour.parse()?, minute.parse()?);
    ensure!(hour < 24 && minute < 60, "invalid maintenance time");
    Ok((hour, minute))
}

fn maintenance_due(at: &str, now: i64, last_checked: i64, phase: &str) -> Result<bool> {
    fn local(timestamp: i64) -> Result<libc::tm> {
        let timestamp = timestamp as libc::time_t;
        let mut value: libc::tm = unsafe { std::mem::zeroed() };
        ensure!(
            !unsafe { libc::localtime_r(&timestamp, &mut value) }.is_null(),
            "cannot resolve local maintenance time"
        );
        Ok(value)
    }
    let (hour, minute) = schedule(at)?;
    let now = local(now)?;
    let last = local(last_checked)?;
    if matches!(phase, "current" | "ready")
        && now.tm_year == last.tm_year
        && now.tm_yday == last.tm_yday
    {
        return Ok(false);
    }
    let start = i32::from(hour) * 60 + i32::from(minute);
    let elapsed = (now.tm_hour * 60 + now.tm_min - start + 1440) % 1440;
    Ok(elapsed < 60)
}

fn updater_label(instance: &str) -> String {
    format!("dev.ibkr.gatewayctl.upgrade.{instance}")
}

fn updater_file(instance: &str) -> Result<PathBuf> {
    Ok(config::home()?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", updater_label(instance))))
}

fn updater_plist(
    executable: &Path,
    config: &Path,
    selection: &Selection,
    at: &str,
) -> Result<String> {
    let (hour, minute) = schedule(at)?;
    let args = [
        executable.to_string_lossy().into_owned(),
        "--config".into(),
        config.to_string_lossy().into_owned(),
        "upgrade".into(),
        "run".into(),
        "--instance".into(),
        selection.instance.clone(),
        "--channel".into(),
        selection.channel.as_str().into(),
        "--scheduled-at".into(),
        at.into(),
    ];
    let args = args
        .iter()
        .map(|value| format!("<string>{}</string>", service::xml(value)))
        .collect::<String>();
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
        <key>Label</key><string>{}</string><key>ProgramArguments</key><array>{args}</array>\
        <key>StartCalendarInterval</key><dict><key>Hour</key><integer>{hour}</integer>\
        <key>Minute</key><integer>{minute}</integer></dict>\
        <key>StartInterval</key><integer>900</integer>\
        <key>ProcessType</key><string>Interactive</string>\
        <key>LimitLoadToSessionType</key><string>Aqua</string><key>Umask</key><integer>63</integer>\
        <key>StandardOutPath</key><string>/dev/null</string>\
        <key>StandardErrorPath</key><string>/dev/null</string>\
        </dict></plist>\n",
        updater_label(&selection.instance)
    ))
}

pub async fn execute(config: Config, path: &Path, action: Action) -> Result<()> {
    ensure!(
        std::env::consts::ARCH == "aarch64",
        "automatic upgrades support Apple silicon macOS only"
    );
    match action {
        Action::Status { instance } => {
            config.instance(&instance)?;
            let state_path = config
                .state_dir
                .join(".upgrades")
                .join(&instance)
                .join("state.json");
            let state = if state_path.try_exists()? {
                serde_json::from_slice(&ownership::read_private(&state_path)?)?
            } else {
                UpdateState {
                    phase: "idle".into(),
                    ..UpdateState::default()
                }
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&public_status(&instance, &state))?
            );
        }
        Action::Install { selection, at } => {
            ensure!(
                config.instance(&selection.instance)?.mode == Mode::Paper,
                "automatic upgrades require a Paper qualification profile"
            );
            ensure!(
                config.instance(&selection.instance)?.enabled
                    && !config.instance(&selection.instance)?.is_unbound(),
                "scheduled upgrades require an enabled, account-bound Paper profile"
            );
            let updater = Updater::open(config, path, &selection.instance)?;
            updater.program()?;
            let plist = updater_plist(
                &std::env::current_exe()?.canonicalize()?,
                path,
                &selection,
                &at,
            )?;
            let file = updater_file(&selection.instance)?;
            if file.try_exists()? {
                ensure!(fs::read_to_string(&file)? == plist, "updater service already exists with different options; uninstall it before replacing");
            } else {
                ownership::atomic_write(&file, plist.as_bytes())?;
            }
            if !service::job_present(&format!(
                "gui/{}/{}",
                unsafe { libc::geteuid() },
                updater_label(&selection.instance)
            ))
            .await?
            {
                command(
                    Path::new("/bin/launchctl"),
                    &[
                        "bootstrap".into(),
                        format!("gui/{}", unsafe { libc::geteuid() }).into(),
                        file.into_os_string(),
                    ],
                    30,
                )
                .await?;
            }
            println!("Automatic {} upgrades enabled for {} at {at} local time; existing supervisor unchanged.",
                selection.channel.as_str(), selection.instance);
        }
        Action::Uninstall { instance } => {
            config.instance(&instance)?;
            let updater = Updater::open(config, path, &instance)?;
            ensure!(
                updater.state.transaction.is_none(),
                "resolve the pending upgrade before removing its updater"
            );
            let target = format!(
                "gui/{}/{}",
                unsafe { libc::geteuid() },
                updater_label(&instance)
            );
            if service::job_present(&target).await? {
                command(
                    Path::new("/bin/launchctl"),
                    &["bootout".into(), target.into()],
                    30,
                )
                .await?;
            }
            let file = updater_file(&instance)?;
            if file.try_exists()? {
                fs::remove_file(file)?;
            }
            println!("Removed only the {instance} scheduled updater.");
        }
        Action::Check { selection } => {
            let mut updater = Updater::open(config, path, &selection.instance)?;
            updater.deliver_notification()?;
            if !updater.reconcile().await? {
                if let Err(error) = updater.prepare(selection.channel, true, false, false).await {
                    updater.report("failed", &format!("{error:#}"), true)?;
                    return Err(error);
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&updater.public_status())?
            );
        }
        Action::Run {
            selection,
            retry_failed,
            reinstall,
            scheduled_at,
        } => {
            ensure!(
                config.instance(&selection.instance)?.mode == Mode::Paper,
                "automatic upgrades require a Paper qualification profile"
            );
            let mut updater = Updater::open(config, path, &selection.instance)?;
            updater.deliver_notification()?;
            let result = async {
                if updater.reconcile().await? {
                    return Ok(());
                }
                let status = updater.status().await?;
                if !updater.eligible(&status).await? {
                    return updater.report(
                        "deferred",
                        "instance_stopped_or_requires_non_upgrade_intervention",
                        status.desired_running,
                    );
                }
                let urgent = status.phase == Phase::NeedsAttention || status.upgrade_recommended;
                if let Some(at) = &scheduled_at {
                    ensure!(
                        !reinstall && !retry_failed,
                        "scheduled runs cannot force a reinstall or retry a failed build"
                    );
                    if !urgent
                        && !maintenance_due(
                            at,
                            OffsetDateTime::now_utc().unix_timestamp(),
                            updater.state.checked_at,
                            &updater.state.phase,
                        )?
                    {
                        if updater.state.phase == "idle" {
                            updater.report("scheduled", "waiting_for_maintenance_window", false)?;
                        }
                        return Ok(());
                    }
                }
                updater.others_stopped().await?;
                let program = updater.program()?;
                if let Some(candidate) = updater
                    .prepare(selection.channel, false, reinstall, retry_failed)
                    .await?
                {
                    if let Err(error) = updater.cutover(&program, candidate).await {
                        updater.state.failed_version = updater
                            .state
                            .available
                            .as_ref()
                            .map(|release| release.version.clone());
                        updater.save()?;
                        return Err(error);
                    }
                } else if urgent {
                    updater.report(
                        "needs_attention",
                        "retirement_notice_without_a_newer_release_requires_inspection",
                        true,
                    )?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                updater.report("failed", &format!("{error:#}"), true)?;
                return Err(error);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&updater.public_status())?
            );
            ensure!(
                !matches!(
                    updater.state.phase.as_str(),
                    "failed" | "needs_attention" | "rolled_back"
                ),
                "upgrade did not complete: {}",
                updater.state.reason
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
    };

    fn metadata(name: &str, version: &str) -> String {
        format!(
            "<root><general applicationName=\"{name}\" publisherName=\"Interactive Brokers LLC\" \
            mediaName=\"ibgateway-latest-standalone-macos-arm\"/>\
            <variable name=\"fullVersion\" value=\"{version}\"/></root>"
        )
    }

    #[test]
    fn release_ordering_handles_patch_digits_and_refuses_unsafe_metadata() {
        let versions = ["10441g", "10501", "10511a", "10511b", "105110a", "10521a"];
        for pair in versions.windows(2) {
            assert!(version_key(pair[0]).unwrap() < version_key(pair[1]).unwrap());
        }
        for version in [
            "",
            "1051",
            "../10511b",
            "10511B",
            "10.51.1b",
            "10511bb",
            "１０５１１b",
        ] {
            assert!(version_key(version).is_err(), "{version}");
        }
        assert_eq!(
            release_from_xml(&metadata("IB Gateway 10.51", "10511b"))
                .unwrap()
                .version,
            "10511b"
        );
        for xml in [
            metadata("../IB Gateway 10.51", "10511b"),
            metadata("IB Gateway 10.51", "10441g"),
            metadata("IB Gateway 10.51", "10511b")
                .replace("Interactive Brokers LLC", "Other Vendor"),
            metadata("IB Gateway 10.51", "10511b").replace("macos-arm", "macosx-x64"),
        ] {
            assert!(release_from_xml(&xml).is_err());
        }
    }

    #[test]
    fn scheduled_job_never_replaces_supervisor_or_runs_reinstall() {
        let selection = Selection {
            instance: "paper".into(),
            channel: Channel::Latest,
        };
        let xml = updater_plist(
            Path::new("/fixture/updater&new"),
            Path::new("/fixture/config.toml"),
            &selection,
            "05:30",
        )
        .unwrap();
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert!(doc
            .descendants()
            .any(|node| node.text() == Some("/fixture/updater&new")));
        assert!(xml.contains("dev.ibkr.gatewayctl.upgrade.paper"));
        assert!(xml.contains("<integer>5</integer>"));
        assert!(xml.contains("<integer>30</integer>"));
        assert!(xml.contains("<key>StartInterval</key><integer>900</integer>"));
        assert!(xml.contains("--scheduled-at"));
        for forbidden in [
            "--reinstall",
            "--retry-failed",
            "KeepAlive",
            "RunAtLoad",
            "credentials",
            "password",
        ] {
            assert!(!xml.contains(forbidden), "{forbidden}");
        }
        for bad in ["5:00", "05:0", "24:00", "12:60", "12:30:00", "-1:00"] {
            assert!(schedule(bad).is_err());
        }
        assert!(RESPONSE.contains("executeLauncherAction$Boolean=false"));
        assert!(!RESPONSE.contains("=true"));
        assert_eq!(Channel::Latest.url(), "https://download2.interactivebrokers.com/installers/ibgateway/latest-standalone/ibgateway-latest-standalone-macos-arm.dmg");
    }

    #[test]
    fn existing_supervisor_arguments_must_match_the_config_and_instance() {
        let xml =
            "<?xml version=\"1.0\"?>\
            <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\
            <plist><dict><key>ProgramArguments</key><array><string>/trusted/supervisor</string>\
            <string>--config</string><string>/private/config.toml</string><string>run</string>\
            <string>--instance</string><string>paper</string></array></dict></plist>";
        assert_eq!(
            supervisor_program(xml, Path::new("/private/config.toml"), "paper").unwrap(),
            Path::new("/trusted/supervisor")
        );
        assert!(supervisor_program(xml, Path::new("/another/config.toml"), "paper").is_err());
        assert!(supervisor_program(xml, Path::new("/private/config.toml"), "live").is_err());
        assert!(supervisor_program(
            &xml.replace("/trusted/supervisor", "relative"),
            Path::new("/private/config.toml"),
            "paper"
        )
        .is_err());
    }

    fn diagnostic(message: &str, ts: &str) -> String {
        serde_json::json!({"kind":"dialog_diagnostic", "ts":ts, "message":message}).to_string()
    }

    #[test]
    fn legacy_recovery_requires_a_fresh_exact_advisory_without_authentication_context() {
        let message = "IBKR Gateway\nvisible_password_fields=0\n\
            The version of the application you are running, [number].1, needs to be upgraded, \
            as it will be desupported on [number]. The minimum supported version at that time \
            will be [number].1. The new version can be downloaded here .\nbutton: OK\n";
        let since = "2026-01-01T00:00:00Z";
        let fresh = "2026-01-01T00:00:02Z";
        assert!(legacy_retirement_notice(&diagnostic(message, fresh), since).unwrap());
        assert!(
            !legacy_retirement_notice(&diagnostic(message, "2025-12-31T23:59:59Z"), since).unwrap()
        );
        for altered in [
            message.replace("will be desupported", "has been desupported"),
            message.replace("IBKR Gateway", "Unknown"),
            message.replace("visible_password_fields=0", "visible_password_fields=1"),
            format!("{message}button: Upgrade now\n"),
            format!("{message}Security code: [redacted]\n"),
        ] {
            assert!(!legacy_retirement_notice(&diagnostic(&altered, fresh), since).unwrap());
        }
        let log = format!(
            "{}\n{}",
            diagnostic(message, fresh),
            diagnostic("Existing session detected", fresh)
        );
        assert!(!legacy_retirement_notice(&log, since).unwrap());
        assert!(legacy_retirement_notice("{invalid", since).is_err());
    }

    #[test]
    fn held_native_restart_is_recoverable_only_when_linked_to_the_retirement_notice() {
        let message = "IBKR Gateway\nThe version of the application you are running, 1044.1, \
            needs to be upgraded, as it will be desupported on 20990101. \
            The minimum supported version at that time will be 1051.1. \
            The new version can be downloaded here .\nbutton: OK\n";
        let transition =
            |message: &str| serde_json::json!({"kind":"transition","message":message}).to_string();
        let log = format!(
            "{}\n{}\n{}",
            diagnostic(message, "2026-01-01T00:00:00Z"),
            transition("needs_attention: unrecognized_modal_dialog"),
            transition("needs_attention: gateway_exited_during_authentication_or_intervention")
        );
        assert!(legacy_held_retirement(&log).unwrap());
        for later in [
            transition("stopped: operator_stopped"),
            transition("authenticating: automatic_login_submitted"),
            transition("needs_attention: authentication_or_session_conflict"),
            diagnostic("Second Factor Authentication", "2026-01-01T00:01:00Z"),
        ] {
            assert!(!legacy_held_retirement(&format!("{log}\n{later}")).unwrap());
        }
        assert!(!legacy_held_retirement(
            &log.replace("will be desupported", "has been desupported")
        )
        .unwrap());
    }

    #[test]
    fn maintenance_window_is_local_bounded_and_idempotent_after_success() {
        let stamp = |day, hour, minute| {
            let mut local: libc::tm = unsafe { std::mem::zeroed() };
            local.tm_year = 126;
            local.tm_mon = 9;
            local.tm_mday = day;
            local.tm_hour = hour;
            local.tm_min = minute;
            local.tm_isdst = -1;
            unsafe { libc::mktime(&mut local) }
        };
        let yesterday = stamp(5, 5, 0);
        for (hour, minute, due) in [
            (4, 59, false),
            (5, 0, true),
            (5, 59, true),
            (6, 0, false),
            (12, 0, false),
        ] {
            assert_eq!(
                maintenance_due("05:00", stamp(6, hour, minute), yesterday, "current").unwrap(),
                due
            );
        }
        assert!(!maintenance_due("05:00", stamp(6, 5, 30), stamp(6, 5, 0), "current").unwrap());
        assert!(maintenance_due("05:00", stamp(6, 5, 30), stamp(6, 5, 0), "failed").unwrap());
    }

    fn status(phase: Phase, reason: &str, desired_running: bool) -> Status {
        Status {
            instance: "paper".into(),
            phase,
            reason: reason.into(),
            desired_running,
            gateway_pid: Some(123),
            upgrade_recommended: false,
        }
    }

    #[test]
    fn authentication_and_operator_stops_never_trigger_automatic_rollback_logins() {
        for reason in [
            "authentication_or_session_conflict",
            "mfa_not_completed",
            "keychain_credentials_unavailable",
            "unrecognized_modal_dialog",
            "read_only_login_requires_fresh_session",
        ] {
            assert_eq!(
                readiness(&status(Phase::NeedsAttention, reason, true)),
                Readiness::Approval(reason.into())
            );
        }
        assert_eq!(
            readiness(&status(Phase::AwaitingMfa, "approval", true)),
            Readiness::Approval("approval".into())
        );
        assert_eq!(
            readiness(&status(Phase::Ready, "operator_stop", false)),
            Readiness::Stopped
        );
        assert_eq!(
            readiness(&status(
                Phase::NeedsAttention,
                "configuration_stalled",
                true
            )),
            Readiness::Failed("configuration_stalled".into())
        );
    }

    struct FakeControl {
        current: RefCell<Status>,
        calls: RefCell<Vec<String>>,
        outcomes: RefCell<VecDeque<Readiness>>,
        original_home: PathBuf,
        reject_preflight: Cell<bool>,
        other_active: Cell<bool>,
        edit_config_on_failure: Cell<bool>,
    }

    impl Control for FakeControl {
        async fn request(&self, _: &Config, instance: &str, action: &str) -> Result<String> {
            self.calls.borrow_mut().push(format!("{instance}:{action}"));
            if instance != "paper" {
                if self.other_active.get() {
                    return Ok(serde_json::json!({
                        "instance": instance, "phase":"ready", "reason":"verified",
                        "desired_running":true, "gateway_pid": 456
                    })
                    .to_string());
                }
                bail!("fixture sibling is not loaded");
            }
            if action == "STOP" {
                let mut current = self.current.borrow_mut();
                current.desired_running = false;
                current.phase = Phase::Stopped;
                current.gateway_pid = None;
            }
            let current = self.current.borrow();
            Ok(serde_json::json!({
                "instance": current.instance, "phase":current.phase, "reason":current.reason,
                "desired_running":current.desired_running, "gateway_pid":current.gateway_pid
            })
            .to_string())
        }

        async fn unload(&self, instance: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("{instance}:UNLOAD"));
            Ok(())
        }

        async fn stop_unreachable(&self, _: &Config, instance: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("{instance}:RECONCILE_STOP"));
            Ok(())
        }

        async fn preflight(&self, _: &Path, _: &Path, _: &str, _: &Path) -> Result<()> {
            self.calls.borrow_mut().push("PREFLIGHT".into());
            ensure!(
                !self.reject_preflight.get(),
                "fixture bridge is incompatible"
            );
            Ok(())
        }

        async fn start(&self, _: &Path, path: &Path, instance: &str) -> Result<()> {
            let config = Config::load(path)?;
            self.calls.borrow_mut().push(format!(
                "{instance}:START:{}",
                config.gateway_home.display()
            ));
            let mut current = self.current.borrow_mut();
            current.desired_running = true;
            current.phase = Phase::Ready;
            current.gateway_pid = Some(123);
            if config.gateway_home != self.original_home {
                fs::write(
                    config.directory(instance).join("settings/fixture"),
                    "migrated-settings",
                )?;
            }
            Ok(())
        }

        async fn wait_ready(&self, config: &Config, _: &str) -> Result<Readiness> {
            let outcome = self
                .outcomes
                .borrow_mut()
                .pop_front()
                .context("no fixture readiness outcome")?;
            match &outcome {
                Readiness::Approval(reason) => {
                    *self.current.borrow_mut() = status(Phase::AwaitingMfa, reason, true);
                }
                Readiness::Stopped => self.current.borrow_mut().desired_running = false,
                Readiness::Failed(_) if self.edit_config_on_failure.get() => {
                    fs::write(
                        config.state_dir.parent().unwrap().join("config.toml"),
                        "externally edited",
                    )?;
                }
                _ => {}
            }
            Ok(outcome)
        }
    }

    struct Fixture {
        updater: Updater<FakeControl>,
        candidate: PathBuf,
        _directory: tempfile::TempDir,
    }

    fn fixture(outcomes: Vec<Readiness>) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let mut config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.gateway_home = directory.path().join("old");
        config.state_dir = directory.path().join("state");
        config.instances.get_mut("paper").unwrap().enabled = true;
        config.instances.get_mut("paper").unwrap().expected_accounts = vec!["DUFIXTURE".into()];
        let candidate = directory.path().join("new");
        for (home, name, version) in [
            (&config.gateway_home, "IB Gateway 10.44", "10441g"),
            (&candidate, "IB Gateway 10.51", "10511b"),
        ] {
            fs::create_dir_all(home.join(".install4j")).unwrap();
            fs::write(
                home.join(".install4j/i4jparams.conf"),
                metadata(name, version),
            )
            .unwrap();
        }
        let settings = config.directory("paper").join("settings");
        ownership::private_dir(&settings).unwrap();
        fs::write(settings.join("fixture"), "original-settings").unwrap();
        let path = directory.path().join("config.toml");
        config.save(&path).unwrap();
        let config = Config::load(&path).unwrap();
        let candidate = candidate.canonicalize().unwrap();
        let updater = Updater::open(config, &path, "paper").unwrap();
        let control = FakeControl {
            current: RefCell::new(status(Phase::Ready, "verified", true)),
            calls: RefCell::new(Vec::new()),
            outcomes: RefCell::new(outcomes.into()),
            original_home: updater.config.gateway_home.clone(),
            reject_preflight: Cell::new(false),
            other_active: Cell::new(false),
            edit_config_on_failure: Cell::new(false),
        };
        let mut updater = Updater {
            config: updater.config,
            config_path: updater.config_path,
            instance: updater.instance,
            root: updater.root,
            directory: updater.directory,
            state: updater.state,
            events: updater.events,
            control,
            _lock: updater._lock,
        };
        updater.state.current = Some(installed_release(&updater.config.gateway_home).unwrap());
        updater.state.available = Some(installed_release(&candidate).unwrap());
        Fixture {
            updater,
            candidate,
            _directory: directory,
        }
    }

    #[tokio::test]
    async fn upgrade_commits_only_after_readiness_and_preserves_the_existing_supervisor() {
        let mut f = fixture(vec![Readiness::Ready]);
        let original_home = f.updater.config.gateway_home.clone();
        f.updater
            .cutover(
                Path::new("/trusted/unchanged-supervisor"),
                f.candidate.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            Config::load(&f.updater.config_path).unwrap().gateway_home,
            f.candidate
        );
        assert_eq!(f.updater.state.phase, "ready");
        assert!(f.updater.state.transaction.is_none());
        assert!(f.updater.state.failed_version.is_none());
        assert!(original_home.exists());
        let calls = f.updater.control.calls.borrow();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("paper:START:"))
                .count(),
            1
        );
        assert!(calls
            .iter()
            .all(|call| !call.contains("live:STOP") && !call.contains("live:UNLOAD")));
        assert_eq!(
            fs::read_to_string(f.updater.config.directory("paper").join("settings/fixture"))
                .unwrap(),
            "migrated-settings"
        );
        assert!(!f.updater.public_status().to_string().contains("DUFIXTURE"));
    }

    #[tokio::test]
    async fn incompatible_upgrade_restores_config_and_settings_then_latches_failed_release() {
        let mut f = fixture(vec![
            Readiness::Failed("unsupported_ui_button".into()),
            Readiness::Ready,
        ]);
        let original = fs::read_to_string(&f.updater.config_path).unwrap();
        f.updater
            .cutover(Path::new("/trusted/supervisor"), f.candidate)
            .await
            .unwrap();
        assert_eq!(f.updater.state.phase, "rolled_back");
        assert_eq!(f.updater.state.failed_version.as_deref(), Some("10511b"));
        assert!(f.updater.state.transaction.is_none());
        assert_eq!(
            fs::read_to_string(&f.updater.config_path).unwrap(),
            original
        );
        assert_eq!(
            fs::read_to_string(f.updater.config.directory("paper").join("settings/fixture"))
                .unwrap(),
            "original-settings"
        );
        assert_eq!(
            f.updater
                .control
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("paper:START:"))
                .count(),
            2
        );
        let restored: UpdateState = serde_json::from_slice(
            &ownership::read_private(&f.updater.directory.join("state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.failed_version.as_deref(), Some("10511b"));
    }

    #[tokio::test]
    async fn mfa_keeps_candidate_and_never_repeats_login_on_the_next_scheduled_run() {
        let mut f = fixture(vec![Readiness::Approval(
            "broker_requires_user_approval".into(),
        )]);
        f.updater
            .cutover(Path::new("/trusted/supervisor"), f.candidate.clone())
            .await
            .unwrap();
        assert_eq!(f.updater.state.phase, "needs_attention");
        assert!(f.updater.state.transaction.is_some());
        assert_eq!(
            Config::load(&f.updater.config_path).unwrap().gateway_home,
            f.candidate
        );
        assert!(f.updater.reconcile().await.unwrap());
        assert_eq!(
            f.updater
                .control
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("paper:START:"))
                .count(),
            1
        );
        *f.updater.control.current.borrow_mut() = status(Phase::Ready, "verified", true);
        f.updater
            .control
            .outcomes
            .borrow_mut()
            .push_back(Readiness::Ready);
        assert!(f.updater.reconcile().await.unwrap());
        assert_eq!(f.updater.state.phase, "ready");
        assert!(f.updater.state.transaction.is_none());
    }

    #[tokio::test]
    async fn explicit_stop_during_upgrade_is_not_overridden() {
        let mut f = fixture(vec![Readiness::Stopped]);
        f.updater
            .cutover(Path::new("/trusted/supervisor"), f.candidate)
            .await
            .unwrap();
        assert_eq!(
            f.updater.state.reason,
            "operator_stopped_during_upgrade_no_automatic_restart"
        );
        assert!(!f.updater.control.current.borrow().desired_running);
        assert_eq!(
            f.updater
                .control
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("paper:START:"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn active_sibling_or_failed_preflight_cannot_stop_paper() {
        for active_sibling in [true, false] {
            let mut f = fixture(vec![]);
            f.updater.control.other_active.set(active_sibling);
            f.updater.control.reject_preflight.set(!active_sibling);
            let original = fs::read_to_string(&f.updater.config_path).unwrap();
            assert!(f
                .updater
                .cutover(Path::new("/trusted/supervisor"), f.candidate)
                .await
                .is_err());
            assert!(f.updater.state.transaction.is_none());
            assert_eq!(
                fs::read_to_string(&f.updater.config_path).unwrap(),
                original
            );
            assert!(!f
                .updater
                .control
                .calls
                .borrow()
                .iter()
                .any(|call| call.ends_with(":STOP")));
        }
    }

    #[tokio::test]
    async fn rollback_never_overwrites_an_external_configuration_edit() {
        let mut f = fixture(vec![Readiness::Failed("configuration_stalled".into())]);
        f.updater.control.edit_config_on_failure.set(true);
        let error = f
            .updater
            .cutover(Path::new("/trusted/supervisor"), f.candidate)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("configuration changed externally"));
        assert_eq!(
            fs::read_to_string(&f.updater.config_path).unwrap(),
            "externally edited"
        );
        assert_eq!(
            f.updater
                .control
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("paper:START:"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn incomplete_settings_snapshot_is_never_restored() {
        let mut f = fixture(vec![Readiness::Ready]);
        let original = fs::read_to_string(&f.updater.config_path).unwrap();
        let partial = f.updater.directory.join("settings-partial-fixture");
        ownership::private_dir(&partial).unwrap();
        fs::write(partial.join("fixture"), "incomplete-copy").unwrap();
        f.updater.control.current.borrow_mut().desired_running = false;
        f.updater.state.transaction = Some(Transaction {
            original_config: original.clone(),
            updated_config: original,
            original_home: f.updater.config.gateway_home.clone(),
            candidate_home: f.candidate,
            settings_backup: partial,
            settings_saved: false,
            start_requested: false,
        });
        f.updater
            .rollback(Path::new("/trusted/supervisor"), "snapshot_failed")
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(f.updater.config.directory("paper").join("settings/fixture"))
                .unwrap(),
            "original-settings"
        );
        assert_eq!(f.updater.state.phase, "rolled_back");
    }

    #[test]
    fn updater_notification_failure_is_visible_and_retried_without_leaking_config() {
        let mut f = fixture(vec![]);
        let delivery = f.updater.directory.join("unavailable-events");
        f.updater.events =
            Events::new("paper", &f.updater.directory, Some(delivery.clone())).unwrap();
        f.updater
            .report("needs_attention", "approval_required", true)
            .unwrap();
        assert!(f.updater.state.notification_pending);
        assert_eq!(f.updater.public_status()["notification_pending"], true);
        fs::create_dir(&delivery).unwrap();
        f.updater.deliver_notification().unwrap();
        assert!(!f.updater.state.notification_pending);
        let event = fs::read_dir(delivery)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let contents = fs::read_to_string(event).unwrap();
        assert!(contents.contains("ibkr-gateway-paper-upgrade"));
        assert!(!contents.contains("ibkr-gateway-paper-session"));
        assert!(!contents.contains("DUFIXTURE"));
    }

    #[tokio::test]
    async fn read_only_eligibility_does_not_resume_stopped_or_conflicting_sessions() {
        let f = fixture(vec![]);
        for state in [
            status(Phase::Stopped, "operator_stopped", false),
            status(
                Phase::NeedsAttention,
                "authentication_or_session_conflict",
                true,
            ),
            status(Phase::AwaitingMfa, "broker_requires_user_approval", true),
        ] {
            assert!(!f.updater.eligible(&state).await.unwrap());
        }
        assert!(f.updater.control.calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn command_output_and_runtime_are_bounded() {
        assert!(command(Path::new("/bin/sleep"), &arguments(&["10"]), 1)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        assert!(command(Path::new("/usr/bin/yes"), &[], 10)
            .await
            .unwrap_err()
            .to_string()
            .contains("output exceeded"));
    }

    #[tokio::test]
    async fn cutover_waits_for_asynchronous_launchagent_shutdown_without_ignoring_other_lock_errors(
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("supervisor.lock");
        let held = Lock::acquire(&path).unwrap();
        let release = tokio::spawn(async move {
            sleep(Duration::from_millis(150)).await;
            drop(held);
        });
        let acquired = timeout(
            Duration::from_secs(2),
            stopped_supervisor_lock(directory.path()),
        )
        .await
        .unwrap()
        .unwrap();
        release.await.unwrap();
        drop(acquired);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(directory.path().join("missing"), &path).unwrap();
        assert!(timeout(
            Duration::from_secs(1),
            stopped_supervisor_lock(directory.path())
        )
        .await
        .unwrap()
        .is_err());
    }
}
