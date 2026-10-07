mod config;
mod credentials;
mod events;
mod gateway;
mod health;
mod ownership;
mod protocol;
mod service;
mod state;
mod supervisor;
mod upgrade;

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Isolated macOS IB Gateway controller. No trading orders are submitted by this tool."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        default_value = "~/.config/ibkr-gateway-rs/config.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Create a disabled example configuration; never overwrite an existing file.
    Init,
    /// Validate configuration without accessing credentials or starting Gateway.
    Validate,
    /// Inspect installed JVM/bridge compatibility without starting or logging in to Gateway.
    Doctor,
    /// Store credentials interactively in macOS Keychain (never in command arguments).
    Credentials {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        stdin: bool,
        /// Recreate this application's own item after an executable signing-identity change.
        #[arg(long)]
        replace: bool,
        /// Authorize this executable to read an existing item without re-entering or replacing credentials.
        #[arg(long, conflicts_with_all = ["stdin", "replace", "check_access"])]
        authorize: bool,
        /// Verify existing credentials can be read without any Keychain UI.
        #[arg(long, conflicts_with_all = ["stdin", "replace", "authorize"])]
        check_access: bool,
    },
    /// Replace one profile from piped JSON; validates before saving private configuration.
    Configure {
        #[arg(long)]
        instance: String,
        #[arg(long, required = true)]
        stdin: bool,
    },
    /// Run one foreground supervisor (used by launchd).
    Run {
        #[arg(long)]
        instance: String,
        /// Perform explicit read-only enrollment once if this profile is still unbound.
        #[arg(long)]
        enroll_if_unbound: bool,
    },
    /// Authenticate an unbound profile read-only, pin its broker account IDs, and leave it ready for service handoff.
    Enroll {
        #[arg(long)]
        instance: String,
    },
    /// Show authoritative status from a running supervisor; omitted instance means all.
    Status {
        #[arg(long)]
        instance: Option<String>,
    },
    /// Record credential-redacted UI labels and component metadata in the private instance log.
    Diagnose {
        #[arg(long)]
        instance: String,
    },
    /// Load an installed service and explicitly start/resume its Gateway.
    Start {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        enroll_if_unbound: bool,
    },
    /// Stop only this instance's Gateway; its supervisor remains available.
    Stop {
        #[arg(long)]
        instance: String,
    },
    /// Request session-preserving native restart of only this instance.
    Restart {
        #[arg(long)]
        instance: String,
    },
    /// Resume automation after an operator has resolved an intervention state.
    Resume {
        #[arg(long)]
        instance: String,
    },
    /// Manage a per-user LaunchAgent. Install does not load/start it.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Check, apply, or schedule verified IB Gateway vendor upgrades.
    Upgrade {
        #[command(subcommand)]
        action: upgrade::Action,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        enroll_if_unbound: bool,
    },
    Uninstall {
        #[arg(long)]
        instance: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = execute(Cli::parse()).await {
        eprintln!("gatewayctl: {error:#}");
        let message: String = format!("{error:#}")
            .chars()
            .filter(|c| *c != '\0')
            .take(2048)
            .collect();
        if let Ok(message) = std::ffi::CString::new(message) {
            unsafe {
                libc::syslog(libc::LOG_ERR, c"gatewayctl: %s".as_ptr(), message.as_ptr());
            }
        }
        std::process::exit(1);
    }
}

async fn execute(cli: Cli) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "this release supports macOS only"
    );
    let supplied_path = if cli.config.is_absolute() || cli.config.starts_with("~") {
        cli.config
    } else {
        std::env::current_dir()?.join(cli.config)
    };
    let path = config::absolute(&supplied_path)?;
    if matches!(cli.action, Action::Init) {
        ensure!(!path.exists(), "configuration already exists");
        let parent = path.parent().context("configuration directory")?;
        ownership::private_dir(parent)?;
        ownership::atomic_write(&path, include_bytes!("../config.example.toml"))?;
        println!("Created {}. Both instances are disabled; configure account identities before enabling.", path.display());
        return Ok(());
    }
    let mut config = Config::load(&path)?;
    match cli.action {
        Action::Init => unreachable!(),
        Action::Validate => println!("Configuration valid; no Gateway was started."),
        Action::Upgrade { action } => upgrade::execute(config, &path, action).await?,
        Action::Doctor => {
            ownership::private_dir(&config.state_dir)?;
            let dir = config.state_dir.join("doctor");
            ownership::private_dir(&dir)?;
            let bridge = gateway::extract_bridge(&dir)?;
            let installation = gateway::Installation::discover(&config.gateway_home, &bridge)?;
            let output = tokio::time::timeout(
                Duration::from_secs(15),
                tokio::process::Command::new(installation.java)
                    .args(installation.options)
                    .arg("-cp")
                    .arg(installation.classpath)
                    .arg(gateway::MAIN_CLASS)
                    .arg("--inspect")
                    .env_remove("JAVA_TOOL_OPTIONS")
                    .env_remove("_JAVA_OPTIONS")
                    .env_remove("JDK_JAVA_OPTIONS")
                    .output(),
            )
            .await
            .context("JVM inspection deadline exceeded")??;
            ensure!(
                output.status.success(),
                "JVM/bridge compatibility inspection failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            print!("{}", String::from_utf8_lossy(&output.stdout));
            println!(
                "legacy_monitor_conflict={}",
                service::legacy_monitor_conflict().await?
            );
            println!("No Gateway login was attempted. UI and native session restart still require controlled qualification.");
        }
        Action::Credentials {
            instance,
            stdin,
            replace,
            authorize,
            check_access,
        } => {
            config.instance(&instance)?;
            if authorize {
                credentials::authorize(&instance)?;
                let verification = tokio::time::timeout(
                    Duration::from_secs(15),
                    tokio::process::Command::new(std::env::current_exe()?)
                        .arg("--config")
                        .arg(&path)
                        .args(["credentials", "--instance", &instance, "--check-access"])
                        .stdin(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .output(),
                )
                .await
                .context("fresh-process Keychain verification timed out")??;
                ensure!(verification.status.success(),
                    "unattended Keychain access is still denied in a fresh process; grant persistent access to this executable, not all applications");
                println!("Existing Keychain access verified for {instance}; credentials were not displayed or replaced.");
            } else if check_access {
                drop(credentials::load(&instance)?);
                println!("Unattended Keychain access verified for {instance}.");
            } else if stdin {
                credentials::import_stdin(&instance, replace)?;
                println!("Credentials stored in macOS Keychain for {instance}.");
            } else {
                credentials::set(&instance, replace)?;
                println!("Credentials stored in macOS Keychain for {instance}.");
            }
        }
        Action::Configure { instance, stdin: _ } => {
            config.instance(&instance)?;
            let bytes = credentials::read_stdin()?;
            let profile = serde_json::from_slice(&bytes).map_err(|_| {
                anyhow::anyhow!("invalid profile JSON; use the documented instance fields")
            })?;
            config.instances.insert(instance.clone(), profile);
            config.save(&path)?;
            println!("Saved the {instance} profile. A running service does not silently adopt configuration changes.");
        }
        Action::Run {
            instance,
            enroll_if_unbound,
        } => {
            if enroll_if_unbound
                && !config.instance(&instance)?.enabled
                && config.instance(&instance)?.is_unbound()
            {
                if !supervisor::enroll(config, &instance, path.clone(), true).await? {
                    return Ok(());
                }
                config = Config::load(&path)?;
            }
            supervisor::run(config, &instance).await?;
        }
        Action::Enroll { instance } => {
            if supervisor::enroll(config, &instance, path, false).await? {
                println!("Read-only enrollment completed; account identities are pinned. Start the normal service to verify and enable its configured API permissions.");
            } else {
                println!("Enrollment stopped without completing account binding.");
            }
        }
        Action::Diagnose { instance } => {
            config.instance(&instance)?;
            println!(
                "{}",
                supervisor::request(&config, &instance, "DIAGNOSE").await?
            );
        }
        Action::Status { instance } => {
            let names = match instance {
                Some(name) => {
                    config.instance(&name)?;
                    vec![name]
                }
                None => config.instances.keys().cloned().collect(),
            };
            let mut output = Vec::new();
            let mut missing = false;
            for name in names {
                match supervisor::request(&config, &name, "STATUS").await {
                    Ok(status) => output.push(serde_json::from_str::<serde_json::Value>(&status)?),
                    Err(_) if !config.instance(&name)?.enabled => {
                        output.push(serde_json::json!({"instance": name, "phase": "disabled"}));
                    }
                    Err(_) => {
                        missing = true;
                        output.push(serde_json::json!({"instance": name, "phase": "supervisor_unreachable"}));
                    }
                }
            }
            println!("{}", serde_json::to_string_pretty(&output)?);
            ensure!(
                !missing,
                "one or more supervisors are unreachable; stale state is not reported as healthy"
            );
        }
        Action::Start {
            instance,
            enroll_if_unbound,
        } => {
            ensure!(
                config.instance(&instance)?.enabled
                    || (enroll_if_unbound && config.instance(&instance)?.is_unbound()),
                "instance is disabled"
            );
            service::load(&instance).await?;
            let mut connected = false;
            for _ in 0..30 {
                if supervisor::request(&config, &instance, "START")
                    .await
                    .is_ok()
                {
                    connected = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            ensure!(
                connected,
                "supervisor did not accept start; inspect status and service errors"
            );
            println!(
                "Start requested for {instance}; use status to verify authenticated readiness."
            );
        }
        Action::Stop { instance } => {
            config.instance(&instance)?;
            println!("{}", supervisor::request(&config, &instance, "STOP").await?);
        }
        Action::Restart { instance } => {
            config.instance(&instance)?;
            println!(
                "{}",
                supervisor::request(&config, &instance, "RESTART").await?
            );
        }
        Action::Resume { instance } => {
            config.instance(&instance)?;
            println!(
                "{}",
                supervisor::request(&config, &instance, "RESUME").await?
            );
        }
        Action::Service {
            action:
                ServiceAction::Install {
                    instance,
                    enroll_if_unbound,
                },
        } => {
            let plist = service::install(&config, &path, &instance, enroll_if_unbound)?;
            println!(
                "Installed {}. It is not loaded; start explicitly when cutover is approved.",
                plist.display()
            );
        }
        Action::Service {
            action: ServiceAction::Uninstall { instance },
        } => {
            config.instance(&instance)?;
            let stopped = match supervisor::request(&config, &instance, "STOP").await {
                Ok(_) => true,
                Err(error) => {
                    eprintln!("gatewayctl: supervisor STOP unavailable; unloading service before owned-process reconciliation: {error:#}");
                    false
                }
            };
            service::uninstall(&instance).await?;
            if !stopped {
                supervisor::stop_unreachable(&config, &instance).await?;
            }
            println!("Uninstalled only the {instance} service.");
        }
    }
    Ok(())
}
