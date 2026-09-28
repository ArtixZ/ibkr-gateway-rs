use std::{path::PathBuf, process::Command};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::Child,
    time::{timeout, Duration},
};

fn classpath(include_fixtures: bool) -> std::ffi::OsString {
    let out = PathBuf::from(env!("OUT_DIR"));
    let mut paths = Vec::new();
    if include_fixtures {
        paths.push(out.join("test-classes"));
    }
    paths.push(out.join("gateway-bridge.jar"));
    paths.push(PathBuf::from(env!("GATEWAYCTL_BUILD_GATEWAY")).join(".install4j/i4jruntime.jar"));
    paths.push(PathBuf::from(env!("GATEWAYCTL_BUILD_GATEWAY")).join("jars/*"));
    std::env::join_paths(paths).unwrap()
}

fn bridge_command() -> Command {
    let mut command = Command::new(env!("GATEWAYCTL_BUILD_JAVA"));
    command
        .args(["-Djava.awt.headless=true", "-Xmx128m", "-cp"])
        .arg(classpath(true));
    command
}

#[test]
fn swing_and_protocol_fixtures() {
    let output = bridge_command()
        .arg("dev.ibkr.gateway.BridgeSelfTest")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn production_bridge_does_not_package_the_synthetic_gateway() {
    let jar_tool = PathBuf::from(env!("GATEWAYCTL_BUILD_JAVA")).with_file_name("jar");
    let output = Command::new(jar_tool)
        .args(["--list", "--file"])
        .arg(PathBuf::from(env!("OUT_DIR")).join("gateway-bridge.jar"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let contents = String::from_utf8(output.stdout).unwrap();
    assert!(contents.contains("dev/ibkr/gateway/GatewayBridge.class"));
    assert!(!contents.contains("ibgateway/"));
    assert!(!contents.contains("BridgeSelfTest"));
}

#[test]
fn native_restart_journal_is_generation_scoped_and_keeps_broker_token() {
    let dir = tempfile::tempdir().unwrap();
    let output = bridge_command()
        .args(["dev.ibkr.gateway.BridgeSelfTest", "restart"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("restart.request")).unwrap(),
        "0123456789abcdef0123456789abcdef\nsession123\n"
    );
    assert!(dir.path().join("settings/session123/autorestart").is_file());
}

#[test]
fn vendor_entry_and_restart_interface_can_be_inspected_without_login() {
    let output = Command::new(env!("GATEWAYCTL_BUILD_JAVA"))
        .arg("-cp")
        .arg(classpath(false))
        .args(["dev.ibkr.gateway.GatewayBridge", "--inspect"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("bridge_protocol=3"));
}

async fn read(stream: &mut UnixStream) -> Vec<String> {
    let size = stream.read_u32().await.unwrap() as usize;
    assert!(size <= 65536);
    let mut body = vec![0; size];
    stream.read_exact(&mut body).await.unwrap();
    let count = u16::from_be_bytes(body[..2].try_into().unwrap());
    let mut cursor = 2;
    let mut fields = Vec::new();
    for _ in 0..count {
        let length = u32::from_be_bytes(body[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        fields.push(String::from_utf8(body[cursor..cursor + length].to_vec()).unwrap());
        cursor += length;
    }
    fields
}

async fn write(stream: &mut UnixStream, fields: &[&str]) {
    let mut body = (fields.len() as u16).to_be_bytes().to_vec();
    for field in fields {
        body.extend_from_slice(&(field.len() as u32).to_be_bytes());
        body.extend_from_slice(field.as_bytes());
    }
    stream.write_u32(body.len() as u32).await.unwrap();
    stream.write_all(&body).await.unwrap();
}

struct GuiOptions {
    mode: &'static str,
    mfa_timeout: u64,
    config_timeout: u64,
    config_error: bool,
    config_stall: bool,
}

impl GuiOptions {
    fn for_mode(mode: &'static str) -> Self {
        Self {
            mode,
            mfa_timeout: 300,
            config_timeout: 30,
            config_error: false,
            config_stall: false,
        }
    }
}

struct Gui {
    child: Child,
    stream: UnixStream,
    listener: UnixListener,
    settings: PathBuf,
    root: PathBuf,
    options: GuiOptions,
    _directory: tempfile::TempDir,
}

impl Gui {
    async fn new(options: GuiOptions) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new()
            .prefix("bridge-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = directory.path().canonicalize().unwrap();
        let settings = root.join("settings");
        std::fs::create_dir(&settings).unwrap();
        let listener = UnixListener::bind(root.join("bridge.sock")).unwrap();
        std::fs::set_permissions(
            root.join("bridge.sock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let (child, stream) = Self::spawn(&root, &settings, &listener, &options, false).await;
        Self {
            child,
            stream,
            listener,
            settings,
            root,
            options,
            _directory: directory,
        }
    }

    async fn spawn(
        root: &std::path::Path,
        settings: &std::path::Path,
        listener: &UnixListener,
        options: &GuiOptions,
        resume: bool,
    ) -> (Child, UnixStream) {
        let mut command = tokio::process::Command::new(env!("GATEWAYCTL_BUILD_JAVA"));
        command
            .args([
                "-Xmx128m",
                "-Djava.awt.headless=false",
                "-Dgatewayctl.fixture=true",
                "-Dgatewayctl.generation=0123456789abcdef0123456789abcdef",
                "-Dtwslaunch.autoupdate.serviceImpl=com.ib.tws.twslaunch.install4j.Install4jAutoUpdateService",
            ])
            .arg(format!("-Dgatewayctl.fixture.mode={}", options.mode))
            .arg("-Dgatewayctl.fixture.mfa=true")
            .arg(format!(
                "-Dgatewayctl.fixture.config-error={}",
                options.config_error
            ))
            .arg(format!(
                "-Dgatewayctl.fixture.config-stall={}",
                options.config_stall
            ))
            .arg(format!("-Dgatewayctl.runtime={}", root.display()));
        command
            .arg(format!("-DjtsConfigDir={}", settings.display()))
            .arg(format!(
                "-javaagent:{}",
                PathBuf::from(env!("OUT_DIR"))
                    .join("gateway-bridge.jar")
                    .display()
            ));
        if resume {
            command.arg("-Drestart=fixture-session");
        }
        let child = command
            .arg("-cp")
            .arg(classpath(true))
            .arg("ibgateway.GWClient")
            .arg(settings)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let (mut stream, _) = timeout(Duration::from_secs(15), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let hello = timeout(Duration::from_secs(5), read(&mut stream))
            .await
            .unwrap();
        assert_eq!(&hello[..2], ["HELLO", "3"]);
        write(
            &mut stream,
            &[
                "CONFIG",
                options.mode,
                "29991",
                "23:45",
                "true",
                "3",
                &options.mfa_timeout.to_string(),
                &options.config_timeout.to_string(),
                if resume { "true" } else { "false" },
            ],
        )
        .await;
        (child, stream)
    }

    async fn state(&mut self, expected: &str) -> Vec<String> {
        timeout(Duration::from_secs(40), async {
            loop {
                let fields = read(&mut self.stream).await;
                if fields.first().map(String::as_str) == Some("STATE") {
                    eprintln!(
                        "synthetic {} waiting for {expected}: {fields:?}",
                        self.options.mode
                    );
                    if fields[1] == expected {
                        return fields;
                    }
                    assert_ne!(fields[1], "needs_attention", "{fields:?}");
                }
            }
        })
        .await
        .unwrap_or_else(|error| panic!("GUI deadline waiting for {expected}: {error}"))
    }

    async fn login(&mut self) {
        self.state("login_required").await;
        write(
            &mut self.stream,
            &["LOGIN", "fixture-user", "fixture-secret"],
        )
        .await;
    }

    fn user_action(&self, name: &str) {
        std::fs::write(self.settings.join(name), "synthetic user action").unwrap();
    }

    async fn await_file(&self, name: &str) {
        timeout(Duration::from_secs(5), async {
            while !self.settings.join(name).exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn stop(&mut self) {
        write(&mut self.stream, &["STOP"]).await;
        assert!(timeout(Duration::from_secs(5), self.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    }

    async fn exercise_ready_and_restart(&mut self) {
        self.state("configured_readonly").await;
        write(&mut self.stream, &["ENABLE_ORDERS"]).await;
        self.state("configured_writable").await;
        assert_eq!(
            std::fs::read_to_string(self.settings.join("fixture-settings")).unwrap(),
            "29991\nfalse\ntrue\n11:45\ntrue\ntrue\n"
        );
        write(&mut self.stream, &["RESTART"]).await;
        let scheduled = timeout(Duration::from_secs(5), async {
            loop {
                let fields = read(&mut self.stream).await;
                if fields[0] == "RESTART_SCHEDULED" {
                    return fields[1].parse::<i64>().unwrap();
                }
            }
        })
        .await
        .unwrap();
        let delta = scheduled - time::OffsetDateTime::now_utc().unix_timestamp();
        assert!(
            (119..=180).contains(&delta),
            "unexpected scheduled restart offset {delta}"
        );
        assert_eq!(
            timeout(Duration::from_secs(30), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .code(),
            Some(0)
        );
        assert!(std::fs::read_to_string(self.root.join("restart.request"))
            .unwrap()
            .ends_with("fixture-session\n"));
        let mut local: libc::tm = unsafe { std::mem::zeroed() };
        let timestamp = scheduled as libc::time_t;
        assert!(!unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null());
        let hour = if local.tm_hour % 12 == 0 {
            12
        } else {
            local.tm_hour % 12
        };
        let saved = std::fs::read_to_string(self.settings.join("fixture-settings")).unwrap();
        let fields: Vec<_> = saved.lines().collect();
        assert_eq!(fields[3], format!("{hour:02}:{:02}", local.tm_min));
        assert_eq!(fields[4], (local.tm_hour >= 12).to_string());
        let (child, stream) = Self::spawn(
            &self.root,
            &self.settings,
            &self.listener,
            &self.options,
            true,
        )
        .await;
        self.child = child;
        self.stream = stream;
        self.state("configured_readonly").await;
        self.stop().await;
    }
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic windows only, never a broker login"]
async fn synthetic_gui_paper_login_notice_readback_and_native_restart() {
    let mut gui = Gui::new(GuiOptions::for_mode("paper")).await;
    gui.login().await;
    gui.exercise_ready_and_restart().await;
}

#[tokio::test]
#[ignore = "requires Aqua; MFA approval is simulated only in the fixture"]
async fn synthetic_gui_live_login_mfa_pause_readback_and_native_restart() {
    let mut gui = Gui::new(GuiOptions::for_mode("live")).await;
    gui.login().await;
    gui.state("awaiting_mfa").await;
    for _ in 0..3 {
        assert_eq!(
            timeout(Duration::from_secs(5), read(&mut gui.stream))
                .await
                .unwrap(),
            ["PULSE"]
        );
    }
    assert!(!gui.settings.join("fixture-configuration-opened").exists());
    assert!(!gui.settings.join("fixture-settings").exists());
    gui.user_action("fixture-user-approved");
    gui.exercise_ready_and_restart().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic MFA cancellation"]
async fn synthetic_gui_cancelled_mfa_requires_explicit_resume() {
    let mut gui = Gui::new(GuiOptions::for_mode("live")).await;
    gui.login().await;
    gui.state("awaiting_mfa").await;
    gui.user_action("fixture-user-cancelled");
    assert_eq!(
        gui.state("needs_attention").await[2],
        "mfa_cancelled_or_expired"
    );
    assert!(!gui.settings.join("fixture-configuration-opened").exists());
    write(&mut gui.stream, &["RESUME"]).await;
    gui.state("login_required").await;
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic MFA timeout"]
async fn synthetic_gui_expired_mfa_stops_without_retrying_login() {
    let mut options = GuiOptions::for_mode("live");
    options.mfa_timeout = 3;
    let mut gui = Gui::new(options).await;
    gui.login().await;
    gui.state("awaiting_mfa").await;
    assert_eq!(gui.state("needs_attention").await[2], "mfa_not_completed");
    assert!(!gui.settings.join("fixture-configuration-opened").exists());
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic read-only login is not trading authority"]
async fn synthetic_gui_readonly_login_cannot_be_promoted_by_resume() {
    let mut gui = Gui::new(GuiOptions::for_mode("live")).await;
    gui.login().await;
    gui.state("awaiting_mfa").await;
    gui.user_action("fixture-user-readonly");
    assert_eq!(
        gui.state("needs_attention").await[2],
        "read_only_login_cannot_provide_trading"
    );
    for command in ["RESUME", "RESUME", "ENABLE_ORDERS"] {
        write(&mut gui.stream, &[command]).await;
        assert_eq!(
            gui.state("needs_attention").await[2],
            "read_only_login_requires_fresh_session"
        );
    }
    assert!(!gui.settings.join("fixture-configuration-opened").exists());
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic modal validation and recovery"]
async fn synthetic_gui_validation_dialog_does_not_publish_stale_success() {
    let mut options = GuiOptions::for_mode("paper");
    options.config_error = true;
    let mut gui = Gui::new(options).await;
    gui.login().await;
    assert_eq!(
        gui.state("needs_attention").await[2],
        "unrecognized_modal_dialog"
    );
    gui.user_action("fixture-dismiss-config-error");
    gui.await_file("fixture-config-error-dismissed").await;
    assert!(!gui.settings.join("fixture-settings").exists());
    write(&mut gui.stream, &["RESUME"]).await;
    gui.state("configured_readonly").await;
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic configuration progress deadline"]
async fn synthetic_gui_configuration_stall_is_explicit() {
    let mut options = GuiOptions::for_mode("paper");
    options.config_stall = true;
    options.config_timeout = 3;
    let mut gui = Gui::new(options).await;
    gui.login().await;
    assert_eq!(
        gui.state("needs_attention").await[2],
        "configuration_stalled"
    );
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic broker disconnect prompt"]
async fn synthetic_gui_relogin_requests_supervised_login_without_reclaiming_a_session() {
    let mut gui = Gui::new(GuiOptions::for_mode("paper")).await;
    gui.login().await;
    gui.state("configured_readonly").await;
    gui.user_action("fixture-connection-relogin");
    assert_eq!(
        gui.state("resume_login_required").await[2],
        "broker_connection_requires_fresh_login"
    );
    assert!(!gui
        .settings
        .join("fixture-connection-relogin-clicked")
        .exists());
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic informational server disconnect"]
async fn synthetic_gui_server_disconnect_notice_does_not_latch_intervention() {
    let mut gui = Gui::new(GuiOptions::for_mode("paper")).await;
    gui.login().await;
    gui.state("configured_readonly").await;
    gui.user_action("fixture-connection-notice");
    gui.state("connection_lost").await;
    gui.await_file("fixture-connection-notice-clicked").await;
    gui.state("configured_readonly").await;
    std::fs::remove_file(gui.settings.join("fixture-connection-notice-clicked")).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    gui.user_action("fixture-connection-notice");
    gui.state("connection_lost").await;
    gui.await_file("fixture-connection-notice-clicked").await;
    gui.state("configured_readonly").await;
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; synthetic competing session remains protected"]
async fn synthetic_gui_existing_session_is_never_reclaimed_by_connection_recovery() {
    let mut gui = Gui::new(GuiOptions::for_mode("paper")).await;
    gui.login().await;
    gui.state("configured_readonly").await;
    gui.user_action("fixture-connection-conflict");
    assert_eq!(
        gui.state("needs_attention").await[2],
        "authentication_or_session_conflict"
    );
    assert!(!gui
        .settings
        .join("fixture-connection-conflict-clicked")
        .exists());
    gui.stop().await;
}

#[tokio::test]
#[ignore = "requires Aqua; connection recovery must not resubmit an MFA login"]
async fn synthetic_gui_disconnect_during_mfa_does_not_start_another_login() {
    let mut gui = Gui::new(GuiOptions::for_mode("live")).await;
    gui.login().await;
    gui.state("awaiting_mfa").await;
    gui.user_action("fixture-connection-notice");
    assert_eq!(
        gui.state("needs_attention").await[2],
        "connection_lost_during_mfa"
    );
    assert!(!gui
        .settings
        .join("fixture-connection-notice-clicked")
        .exists());
    assert!(!gui.settings.join("fixture-configuration-opened").exists());
    gui.stop().await;
}
