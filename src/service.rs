use crate::{
    config::{self, Config},
    ownership,
};
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

pub(crate) fn label(instance: &str) -> String {
    format!("dev.ibkr.gatewayctl.{instance}")
}

pub(crate) fn file(instance: &str) -> Result<PathBuf> {
    Ok(config::home()?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(instance))))
}

pub(crate) fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn install(
    config: &Config,
    config_path: &Path,
    instance: &str,
    enroll_if_unbound: bool,
) -> Result<PathBuf> {
    let profile = config.instance(instance)?;
    ensure!(
        profile.enabled || (enroll_if_unbound && profile.is_unbound()),
        "enable and validate the instance before installing its service"
    );
    let dir = config.directory(instance);
    ownership::private_dir(&config.state_dir)?;
    ownership::private_dir(&dir)?;
    ownership::private_dir(&dir.join("logs"))?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let mut arguments = vec![
        executable.to_string_lossy().to_string(),
        "--config".into(),
        config_path.canonicalize()?.to_string_lossy().to_string(),
        "run".into(),
        "--instance".into(),
        instance.into(),
    ];
    if enroll_if_unbound {
        arguments.push("--enroll-if-unbound".into());
    }
    let arguments = arguments
        .iter()
        .map(|s| format!("<string>{}</string>", xml(s)))
        .collect::<String>();
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\
         <key>Label</key><string>{}</string>\
         <key>ProgramArguments</key><array>{arguments}</array>\
         <key>RunAtLoad</key><true/>\
         <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\
         <key>ThrottleInterval</key><integer>30</integer>\
         <key>ExitTimeOut</key><integer>30</integer>\
         <key>ProcessType</key><string>Interactive</string>\
         <key>LimitLoadToSessionType</key><string>Aqua</string>\
         <key>Umask</key><integer>63</integer>\
         <key>StandardOutPath</key><string>/dev/null</string>\
         <key>StandardErrorPath</key><string>/dev/null</string>\
         </dict></plist>\n",
        label(instance)
    );
    let path = file(instance)?;
    fs::create_dir_all(path.parent().context("LaunchAgents directory")?)?;
    if path.exists() {
        ensure!(
            fs::read_to_string(&path)? == plist,
            "service file already exists with different content; uninstall it before replacing"
        );
    } else {
        ownership::atomic_write(&path, plist.as_bytes())?;
    }
    Ok(path)
}

pub async fn load(instance: &str) -> Result<()> {
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let target = format!("{domain}/{}", label(instance));
    let existing = Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()
        .await?;
    if existing.status.success() {
        if String::from_utf8(existing.stdout)?
            .lines()
            .any(|l| l.trim() == "state = running")
        {
            return Ok(());
        }
        let output = Command::new("/bin/launchctl")
            .args(["kickstart", &target])
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "launchctl kickstart failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    let output = Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(domain)
        .arg(file(instance)?)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "launchctl bootstrap failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

pub async fn unload(instance: &str) -> Result<()> {
    let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, label(instance));
    if job_present(&target).await? {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            Command::new("/bin/launchctl")
                .args(["bootout", &target])
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(
            output.status.success(),
            "launchctl bootout failed; service remains installed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

pub async fn uninstall(instance: &str) -> Result<()> {
    unload(instance).await?;
    match fs::remove_file(file(instance)?) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove this instance's LaunchAgent"),
    }
    Ok(())
}

fn job_output_present(output: &std::process::Output) -> Result<bool> {
    if output.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if error.contains("Could not find service")
        || error.contains("Could not find specified service")
    {
        return Ok(false);
    }
    bail!("launchd job inspection failed: {error}")
}

pub(crate) async fn job_present(target: &str) -> Result<bool> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new("/bin/launchctl")
            .args(["print", target])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    job_output_present(&output)
}

pub async fn legacy_monitor_conflict() -> Result<bool> {
    match config::home()?
        .join("Library/LaunchAgents/com.ibkr.monitor.plist")
        .symlink_metadata()
    {
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect legacy LaunchAgent installation"),
    }
    let target = format!("gui/{}/com.ibkr.monitor", unsafe { libc::geteuid() });
    job_present(&target).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plist_values_are_escaped() {
        assert_eq!(xml("a&<b>\"'"), "a&amp;&lt;b&gt;&quot;&apos;");
        assert_ne!(label("paper"), label("live"));
    }
    #[test]
    fn a_waiting_keepalive_job_is_still_a_conflict() {
        use std::os::unix::process::ExitStatusExt;
        let waiting = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"state = waiting\n".to_vec(),
            stderr: Vec::new(),
        };
        assert!(job_output_present(&waiting).unwrap());
        let denied = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"Operation not permitted".to_vec(),
        };
        assert!(job_output_present(&denied).is_err());
    }
}
