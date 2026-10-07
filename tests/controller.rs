use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn command(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gatewayctl"));
    command.arg("--config").arg(config);
    command
}

#[test]
fn upgrade_status_is_readable_while_an_updater_holds_its_lock() {
    use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let contents = include_str!("../config.example.toml")
        .replace("~/.local/share/ibkr-gateway-rs", &dir.path().join("state").to_string_lossy());
    fs::write(&config, contents).unwrap();
    let root = dir.path().join("state/.upgrades");
    fs::create_dir_all(root.join("paper")).unwrap();
    let lock = fs::File::create(root.join("upgrade.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let journal = root.join("paper/state.json");
    fs::write(&journal, r#"{"phase":"installing","reason":"fixture","transaction":null}"#).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
    let output = command(&config)
        .args(["upgrade", "status", "--instance", "paper"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["phase"], "installing");
    assert_eq!(value["instance"], "paper");
    assert!(value.get("transaction").is_none());
}

#[test]
fn unattended_upgrade_never_targets_live_or_accepts_arbitrary_download_urls() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, include_str!("../config.example.toml")).unwrap();
    let live = command(&config)
        .args(["upgrade", "run", "--instance", "live"])
        .output()
        .unwrap();
    assert!(!live.status.success());
    assert!(String::from_utf8_lossy(&live.stderr).contains("Paper qualification profile"));
    let url = command(&config)
        .args(["upgrade", "check", "--instance", "paper", "--channel", "https://example.invalid/installer"])
        .output()
        .unwrap();
    assert_eq!(url.status.code(), Some(2));
}

fn status(config: &Path, instance: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let output = command(config)
            .args(["status", "--instance", instance])
            .output()
            .unwrap();
        if output.status.success() {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            return value[0].clone();
        }
        assert!(
            Instant::now() < deadline,
            "supervisor not responsive: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn isolated_controllers_locks_and_persistent_stop_without_a_real_gateway() {
    let dir = tempfile::Builder::new()
        .prefix("gw-")
        .tempdir_in("/tmp")
        .unwrap();
    let config = dir.path().join("config.toml");
    let contents = format!(
        "gateway_home = {:?}\nstate_dir = {:?}\n",
        dir.path().join("not-installed").to_string_lossy(),
        dir.path().join("state").to_string_lossy()
    ) + r#"
[instances.paper]
enabled = true
mode = "paper"
api_port = 29401
monitor_client_id = 19001
expected_accounts = ["DUFAKE"]
api_orders = true
auto_restart_time = "23:45"
[instances.live]
enabled = true
mode = "live"
api_port = 29402
monitor_client_id = 19002
expected_accounts = ["UFAKE"]
api_orders = true
auto_restart_time = "23:50"
"#;
    fs::write(&config, contents).unwrap();
    let mut paper = Process(
        command(&config)
            .args(["run", "--instance", "paper"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut live = Process(
        command(&config)
            .args(["run", "--instance", "live"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    status(&config, "paper");
    status(&config, "live");
    let duplicate = command(&config)
        .args(["run", "--instance", "paper"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("ownership lock"));
    let stopped = command(&config)
        .args(["stop", "--instance", "paper"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let paper_status = status(&config, "paper");
    assert_eq!(paper_status["phase"], "stopped");
    assert_eq!(paper_status["desired_running"], false);
    assert!(paper_status["gateway_pid"].is_null());
    assert!(live.0.try_wait().unwrap().is_none());
    assert!(status(&config, "live")["gateway_pid"].is_null());
    unsafe {
        libc::kill(paper.0.id() as i32, libc::SIGTERM);
    }
    paper.0.wait().unwrap();
    let mut restarted = Process(
        command(&config)
            .args(["run", "--instance", "paper"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let persisted = status(&config, "paper");
    assert_eq!(persisted["desired_running"], false);
    assert_eq!(persisted["phase"], "stopped");
    assert!(restarted.0.try_wait().unwrap().is_none());
}

#[test]
fn disabled_profiles_cannot_start() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, include_str!("../config.example.toml")).unwrap();
    let output = command(&config)
        .args(["run", "--instance", "live"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("disabled"));
}

#[test]
fn disabled_profiles_have_successful_disabled_status() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, include_str!("../config.example.toml")).unwrap();
    let output = command(&config)
        .env("HOME", dir.path())
        .arg("status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(status
        .as_array()
        .unwrap()
        .iter()
        .all(|value| value["phase"] == "disabled"));
}

#[test]
fn relative_configuration_path_is_resolved_from_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("config.toml"),
        include_str!("../config.example.toml"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gatewayctl"))
        .current_dir(dir.path())
        .args(["--config", "config.toml", "validate"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn keychain_authorization_does_not_combine_with_credential_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    fs::write(&config, include_str!("../config.example.toml")).unwrap();
    for conflicting in ["--stdin", "--replace", "--check-access"] {
        let result = command(&config)
            .args([
                "credentials",
                "--instance",
                "paper",
                "--authorize",
                conflicting,
            ])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
    }
}
#[test]
fn first_run_service_requires_explicit_readonly_enrollment_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    fs::write(&config, include_str!("../config.example.toml")).unwrap();
    let denied = command(&config)
        .env("HOME", dir.path())
        .args(["service", "install", "--instance", "live"])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    let allowed = command(&config)
        .env("HOME", dir.path())
        .args([
            "service",
            "install",
            "--instance",
            "live",
            "--enroll-if-unbound",
        ])
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let plist = fs::read_to_string(
        dir.path()
            .join("Library/LaunchAgents/dev.ibkr.gatewayctl.live.plist"),
    )
    .unwrap();
    assert!(plist.contains("<string>--enroll-if-unbound</string>"));
    assert!(!plist.contains("password"));
}

#[test]
fn uninstall_of_never_loaded_service_removes_only_its_own_plist() {
    let dir = tempfile::tempdir().unwrap();
    let name = format!("fixture-{}", std::process::id());
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        include_str!("../config.example.toml")
            .replace("[instances.paper]", &format!("[instances.{name}]")),
    )
    .unwrap();
    let agents = dir.path().join("Library/LaunchAgents");
    fs::create_dir_all(&agents).unwrap();
    let plist = agents.join(format!("dev.ibkr.gatewayctl.{name}.plist"));
    fs::write(&plist, "fixture-only").unwrap();
    let unrelated = agents.join("unrelated.plist");
    fs::write(&unrelated, "preserve").unwrap();
    for _ in 0..2 {
        let output = command(&config)
            .env("HOME", dir.path())
            .args(["service", "uninstall", "--instance", &name])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!plist.exists());
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "preserve");
    }
}
