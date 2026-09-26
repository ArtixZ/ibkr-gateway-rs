use crate::{config::Config, ownership};
use anyhow::{bail, ensure, Context, Result};
use std::{
    fs,
    os::unix::fs::{symlink, MetadataExt},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};

const BRIDGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gateway-bridge.jar"));
pub const MAIN_CLASS: &str = "dev.ibkr.gateway.GatewayBridge";

pub struct Installation {
    pub java: PathBuf,
    pub classpath: String,
    pub options: Vec<String>,
}

impl Installation {
    pub fn discover(home: &Path, bridge: &Path) -> Result<Self> {
        let jre = home.join(".install4j/jre.bundle/Contents/Home");
        let java = [jre.join("bin/java"), jre.join("jre/bin/java")]
            .into_iter()
            .find(|p| p.is_file())
            .context("bundled macOS JVM not found")?;
        let jars = home.join("jars");
        ensure!(jars.is_dir(), "Gateway jars directory not found");
        let runtime = home.join(".install4j/i4jruntime.jar");
        ensure!(runtime.is_file(), "install4j runtime not found");
        let classpath = std::env::join_paths([jars.join("*"), runtime, bridge.to_owned()])?
            .into_string()
            .map_err(|_| anyhow::anyhow!("JVM paths must be UTF-8"))?;
        let xml = fs::read_to_string(home.join(".install4j/i4jparams.conf"))?;
        let doc = roxmltree::Document::parse(&xml).context("parse vendor JVM options")?;
        let extra = doc
            .descendants()
            .find(|n| n.attribute("name") == Some("javaOptions"))
            .and_then(|n| n.attribute("value"))
            .context("vendor javaOptions metadata missing; unsupported installation")?;
        let mut options = shlex::split(extra).context("invalid vendor javaOptions quoting")?;
        for line in fs::read_to_string(home.join("ibgateway.vmoptions"))?.lines() {
            let option = line.trim();
            if option.is_empty() || option.starts_with('#') {
                continue;
            }
            if option.starts_with("-javaagent:") || option.starts_with("-agentpath:") {
                bail!("unexpected injected agent in vendor JVM options");
            }
            options.push(option.to_owned());
        }
        Ok(Self {
            java,
            classpath,
            options,
        })
    }
}

pub fn extract_bridge(dir: &Path) -> Result<PathBuf> {
    let path = dir.join("gateway-bridge.jar");
    if !path.exists() || fs::read(&path)? != BRIDGE {
        ownership::atomic_write(&path, BRIDGE)?;
    }
    Ok(path)
}

fn application_name(home: &Path) -> Result<String> {
    Ok(format!(
        "{}.app",
        home.file_name()
            .and_then(|n| n.to_str())
            .context("Gateway installation name is invalid")?
    ))
}

pub fn native_executable(config: &Config, name: &str) -> Result<PathBuf> {
    Ok(config
        .directory(name)
        .join("installation")
        .join(application_name(&config.gateway_home)?)
        .join("Contents/MacOS/JavaApplicationStub"))
}

pub fn expected_executable(config: &Config, name: &str, executable: &Path) -> Result<bool> {
    let private = native_executable(config, name)?;
    let java = Installation::discover(
        &config.gateway_home,
        &config.directory(name).join("gateway-bridge.jar"),
    )?
    .java;
    let executable = executable.canonicalize()?;
    if private.is_file() && private.canonicalize()? == executable {
        ensure!(native_snapshot_matches(config, name)?,
            "Gateway installation changed; stop this instance before rebuilding its native launcher");
        return Ok(true);
    }
    Ok(java.canonicalize()? == executable)
}

fn source_application(home: &Path) -> Result<PathBuf> {
    let normal = home.join(application_name(home)?);
    let renamed = home.join(format!(
        "{}-1.app",
        home.file_name()
            .and_then(|s| s.to_str())
            .context("Gateway name missing")?
    ));
    let application = if normal.is_dir() { normal } else { renamed };
    ensure!(
        application.is_dir(),
        "native Gateway application bundle not found"
    );
    Ok(application)
}

fn source_fingerprint(home: &Path) -> Result<String> {
    let app = source_application(home)?;
    let mut paths = vec![
        app.join("Contents/Info.plist"),
        app.join("Contents/MacOS/JavaApplicationStub"),
        home.join(".install4j/i4jparams.conf"),
        home.join(".install4j/i4jruntime.jar"),
    ];
    for entry in fs::read_dir(home.join("jars"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "jar") {
            paths.push(path);
        }
    }
    let release = home.join(".install4j/jre.bundle/Contents/Home/release");
    if release.try_exists()? {
        paths.push(release);
    }
    paths.sort();
    let mut fingerprint = format!("v2\n{}\n", home.display());
    for path in paths {
        let metadata = fs::metadata(&path)?;
        fingerprint.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            path.display(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec()
        ));
    }
    Ok(fingerprint)
}

fn native_snapshot_matches(config: &Config, name: &str) -> Result<bool> {
    let root = config.directory(name).join("installation");
    let recorded = String::from_utf8(ownership::read_private(&root.join("gatewayctl-source"))?)?;
    if recorded == source_fingerprint(&config.gateway_home)? {
        return Ok(true);
    }
    if recorded != config.gateway_home.to_string_lossy() {
        return Ok(false);
    }
    let source = source_application(&config.gateway_home)?;
    let cached = root.join(application_name(&config.gateway_home)?);
    for relative in ["Contents/Info.plist", "Contents/MacOS/JavaApplicationStub"] {
        if fs::read(source.join(relative))? != fs::read(cached.join(relative))? {
            return Ok(false);
        }
    }
    Ok(
        fs::read(config.gateway_home.join(".install4j/i4jparams.conf"))?
            == fs::read(root.join(".install4j/i4jparams.conf"))?,
    )
}

fn clone_directory(source: &Path, destination: &Path) -> Result<()> {
    let status = std::process::Command::new("/bin/cp")
        .args(["-cR"])
        .arg(source)
        .arg(destination)
        .status()
        .context("clone native launcher metadata")?;
    ensure!(
        status.success(),
        "cannot clone the installed Gateway into a private launcher directory"
    );
    Ok(())
}

fn prepare_native_installation(config: &Config, name: &str) -> Result<PathBuf> {
    let root = config.directory(name).join("installation");
    if root.exists() {
        ownership::private_dir(&root)?;
        if native_snapshot_matches(config, name)? {
            ownership::atomic_write(
                &root.join("gatewayctl-source"),
                source_fingerprint(&config.gateway_home)?.as_bytes(),
            )?;
            return Ok(root);
        }
    }
    let fingerprint = source_fingerprint(&config.gateway_home)?;
    let staging = config
        .directory(name)
        .join(format!("installation-{}", ownership::nonce()?));
    ownership::private_dir(&staging)?;
    let prepared = (|| -> Result<()> {
        let app_name = application_name(&config.gateway_home)?;
        clone_directory(
            &source_application(&config.gateway_home)?,
            &staging.join(&app_name),
        )?;
        let metadata = staging.join(".install4j");
        ownership::private_dir(&metadata)?;
        for entry in fs::read_dir(config.gateway_home.join(".install4j"))? {
            let entry = entry?;
            let target = metadata.join(entry.file_name());
            let source = entry.path();
            if entry.file_name() == "jre.bundle" {
                symlink(source.canonicalize()?, target)?;
            } else if entry.file_type()?.is_dir() {
                clone_directory(&source, &target)?;
            } else if entry.file_type()?.is_symlink() {
                symlink(source.canonicalize()?, target)?;
            } else {
                fs::copy(source, target)?;
            }
        }
        symlink(config.gateway_home.join("jars"), staging.join("jars"))?;
        if config.gateway_home.join("data").is_dir() {
            symlink(config.gateway_home.join("data"), staging.join("data"))?;
        }
        ownership::atomic_write(&staging.join("gatewayctl-source"), fingerprint.as_bytes())?;
        ensure!(
            source_fingerprint(&config.gateway_home)? == fingerprint,
            "Gateway installation changed while preparing its native launcher"
        );
        Ok(())
    })();
    if let Err(error) = prepared {
        fs::remove_dir_all(&staging).context("clean partial private launcher preparation")?;
        return Err(error);
    }
    if root.exists() {
        let retired = config
            .directory(name)
            .join(format!("installation-retired-{}", ownership::nonce()?));
        fs::rename(&root, &retired)?;
        if let Err(error) = fs::rename(&staging, &root) {
            fs::rename(&retired, &root).context("restore prior native launcher")?;
            fs::remove_dir_all(&staging).context("clean unpublished native launcher")?;
            return Err(error).context("publish replacement native launcher");
        }
        fs::remove_dir_all(retired).context("clean retired private launcher metadata")?;
    } else {
        fs::rename(&staging, &root)?;
    }
    Ok(root)
}

pub fn command(
    config: &Config,
    name: &str,
    generation: &str,
    restart: Option<&str>,
) -> Result<Command> {
    let dir = config.directory(name);
    let bridge = extract_bridge(&dir)?;
    let installation = prepare_native_installation(config, name)?;
    let settings = dir.join("settings");
    ownership::private_dir(&settings)?;
    let app = installation.join(application_name(&config.gateway_home)?);
    let mut options = String::new();
    for line in fs::read_to_string(config.gateway_home.join("ibgateway.vmoptions"))?.lines() {
        let line = line.trim();
        ensure!(
            !line.starts_with("-javaagent:") && !line.starts_with("-agentpath:"),
            "unexpected agent in the original Gateway options"
        );
        if !line.starts_with("-DvmOptionsPath=")
            && !line.starts_with("-DjtsConfigDir=")
            && !line.starts_with("-Drestart=")
        {
            options.push_str(line);
            options.push('\n');
        }
    }
    options.push_str(&format!(
        "-DvmOptionsPath={}\n-DjtsConfigDir={}\n-Dgatewayctl.runtime={}\n-Dgatewayctl.generation={generation}\n-DskipUpdateCheck=true\n-javaagent:{}\n",
        installation.join("ibgateway.vmoptions").display(), settings.display(), dir.display(), bridge.display()
    ));
    if let Some(session) = restart {
        ensure!(
            valid_session(session)
                && fs::symlink_metadata(settings.join(session)).is_ok_and(|m| m.is_dir())
                && fs::symlink_metadata(settings.join(session).join("autorestart"))
                    .is_ok_and(|m| m.is_file()),
            "native restart state is absent or invalid; full login required"
        );
        options.push_str(&format!("-Drestart={session}\n"));
    }
    ownership::atomic_write(
        &installation.join("ibgateway.vmoptions"),
        options.as_bytes(),
    )?;
    ownership::atomic_write(
        &app.join("Contents/vmoptions.txt"),
        format!(
            "-include-options {}\n",
            installation.join("ibgateway.vmoptions").display()
        )
        .as_bytes(),
    )?;
    let mut command = Command::new(native_executable(config, name)?);
    command
        .current_dir(&installation)
        .env_remove("JAVA_TOOL_OPTIONS")
        .env_remove("_JAVA_OPTIONS")
        .env_remove("JDK_JAVA_OPTIONS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    Ok(command)
}

pub fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && session
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub async fn terminate(identity: &ownership::Identity, child: &mut Option<Child>) -> Result<()> {
    identity.signal(libc::SIGTERM)?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if let Some(child) = child.as_mut() {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
        }
        if !identity.alive()? {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    identity.signal(libc::SIGKILL)?;
    if let Some(child) = child.as_mut() {
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await??;
    } else {
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while identity.alive()? && tokio::time::Instant::now() < until {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        ensure!(
            !identity.alive()?,
            "owned Gateway did not terminate; ownership retained for inspection"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_identifiers_cannot_escape_settings() {
        assert!(valid_session("abc_0123-def"));
        for invalid in ["", ".", "..", "../live", "/tmp/foo", "a/b", "a:b"] {
            assert!(!valid_session(invalid));
        }
    }

    #[test]
    fn native_snapshot_refreshes_after_in_place_vendor_changes() {
        let directory = tempfile::tempdir().unwrap();
        let mut config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.gateway_home = directory.path().join("vendor");
        config.state_dir = directory.path().join("state");
        ownership::private_dir(&config.directory("paper")).unwrap();
        let bundle = config.gateway_home.join("vendor.app");
        fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        fs::write(
            bundle.join("Contents/MacOS/JavaApplicationStub"),
            "binary-fixture",
        )
        .unwrap();
        fs::write(bundle.join("Contents/Info.plist"), "first-version").unwrap();
        fs::create_dir_all(
            config
                .gateway_home
                .join(".install4j/jre.bundle/Contents/Home"),
        )
        .unwrap();
        fs::create_dir(config.gateway_home.join("jars")).unwrap();
        fs::write(
            config.gateway_home.join(".install4j/i4jparams.conf"),
            "<root/>",
        )
        .unwrap();
        fs::write(config.gateway_home.join(".install4j/i4jruntime.jar"), []).unwrap();
        let installed = prepare_native_installation(&config, "paper").unwrap();
        assert!(native_snapshot_matches(&config, "paper").unwrap());
        fs::write(
            bundle.join("Contents/Info.plist"),
            "second-version-with-new-metadata",
        )
        .unwrap();
        assert!(!native_snapshot_matches(&config, "paper").unwrap());
        assert_eq!(
            prepare_native_installation(&config, "paper").unwrap(),
            installed
        );
        assert!(native_snapshot_matches(&config, "paper").unwrap());
        assert_eq!(
            fs::read_to_string(installed.join("vendor.app/Contents/Info.plist")).unwrap(),
            "second-version-with-new-metadata"
        );
        assert!(!fs::read_dir(config.directory("paper"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("installation-")));
    }
}
