use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Paper,
    Live,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Live => "live",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub gateway_home: PathBuf,
    pub state_dir: PathBuf,
    pub instances: BTreeMap<String, Instance>,
    #[serde(default)]
    pub recovery: Recovery,
    pub tradebus_events: Option<PathBuf>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub enabled: bool,
    pub mode: Mode,
    pub api_port: u16,
    pub monitor_client_id: i32,
    pub expected_accounts: Vec<String>,
    pub api_orders: bool,
    pub auto_restart_time: String,
}

impl Instance {
    pub fn is_unbound(&self) -> bool {
        self.expected_accounts
            .iter()
            .all(|account| account.contains("ACCOUNT"))
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Recovery {
    pub check_interval_secs: u64,
    pub failure_threshold: u32,
    pub startup_timeout_secs: u64,
    pub restart_grace_secs: u64,
    pub native_shutdown_timeout_secs: u64,
    pub session_resume_timeout_secs: u64,
    pub mfa_timeout_secs: u64,
    pub ui_stall_timeout_secs: u64,
    pub backoff_initial_secs: u64,
    pub backoff_max_secs: u64,
    pub max_restarts: u32,
    pub healthy_reset_secs: u64,
}

impl Default for Recovery {
    fn default() -> Self {
        Self {
            check_interval_secs: 10,
            failure_threshold: 3,
            startup_timeout_secs: 90,
            restart_grace_secs: 300,
            native_shutdown_timeout_secs: 120,
            session_resume_timeout_secs: 120,
            mfa_timeout_secs: 300,
            ui_stall_timeout_secs: 120,
            backoff_initial_secs: 60,
            backoff_max_secs: 600,
            max_restarts: 10,
            healthy_reset_secs: 300,
        }
    }
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is required")
}

pub fn absolute(path: &Path) -> Result<PathBuf> {
    let expanded = if path == Path::new("~") {
        home()?
    } else if let Ok(tail) = path.strip_prefix("~/") {
        home()?.join(tail)
    } else {
        path.to_owned()
    };
    ensure!(
        expanded.is_absolute(),
        "paths must be absolute or start with ~/"
    );
    ensure!(
        !expanded
            .components()
            .any(|c| matches!(c, Component::ParentDir)),
        "parent traversal is not permitted in configured paths"
    );
    let mut ancestor = expanded.as_path();
    let mut tail = Vec::new();
    while !ancestor.exists() {
        tail.push(ancestor.file_name().context("invalid path")?);
        ancestor = ancestor.parent().context("invalid path")?;
    }
    let mut result = ancestor.canonicalize().context("resolve configured path")?;
    for part in tail.into_iter().rev() {
        result.push(part);
    }
    Ok(result)
}

impl Config {
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        crate::ownership::atomic_write(path, toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path).context("read configuration metadata")?;
        ensure!(metadata.is_file(), "configuration must be a regular file");
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "configuration must be owned by you and not writable by other users"
        );
        let text = fs::read_to_string(path).context("read configuration")?;
        let mut config: Self = toml::from_str(&text).map_err(|_| {
            anyhow::anyhow!(
                "invalid configuration; check field names/types against config.example.toml"
            )
        })?;
        config.gateway_home = absolute(&config.gateway_home)?;
        config.state_dir = absolute(&config.state_dir)?;
        config.tradebus_events = config
            .tradebus_events
            .as_deref()
            .map(absolute)
            .transpose()?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.instances.is_empty(),
            "at least one instance is required"
        );
        ensure!(
            self.state_dir != self.gateway_home
                && !self.state_dir.starts_with(&self.gateway_home)
                && !self.gateway_home.starts_with(&self.state_dir),
            "state and vendor installation directories must be separate"
        );
        let mut ports = BTreeMap::new();
        for (name, instance) in &self.instances {
            ensure!(
                !name.is_empty()
                    && name.len() <= 24
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
                "instance names must contain 1-24 lowercase letters, digits or hyphens"
            );
            ensure!(instance.api_port >= 1024, "API ports must be unprivileged");
            ensure!(
                ports.insert(instance.api_port, name).is_none(),
                "instances must use different API ports"
            );
            ensure!(
                instance.monitor_client_id > 0,
                "monitor client ID must be nonzero"
            );
            let time = instance.auto_restart_time.split(':').collect::<Vec<_>>();
            ensure!(
                time.len() == 2
                    && time[0].len() == 2
                    && time[1].len() == 2
                    && time[0].parse::<u8>().is_ok_and(|h| h < 24)
                    && time[1].parse::<u8>().is_ok_and(|m| m < 60),
                "auto_restart_time must be HH:MM in Gateway's local timezone"
            );
            if instance.enabled {
                ensure!(
                    !instance.expected_accounts.is_empty()
                        && instance.expected_accounts.iter().all(|a| {
                            !a.is_empty()
                                && a.len() < 64
                                && a.bytes().all(|c| c.is_ascii_alphanumeric())
                                && !a.contains("ACCOUNT")
                        }),
                    "enabled instances require actual expected_accounts; never guess account identity"
                );
            }
        }
        let r = &self.recovery;
        ensure!(
            [
                r.check_interval_secs,
                r.startup_timeout_secs,
                r.restart_grace_secs,
                r.native_shutdown_timeout_secs,
                r.session_resume_timeout_secs,
                r.mfa_timeout_secs,
                r.ui_stall_timeout_secs,
                r.backoff_initial_secs,
                r.backoff_max_secs,
                r.healthy_reset_secs,
            ]
            .iter()
            .all(|value| *value <= 86400)
                && r.failure_threshold <= 100
                && r.max_restarts <= 100,
            "recovery durations are limited to one day and counters to 100"
        );
        if r.check_interval_secs == 0
            || r.failure_threshold == 0
            || r.startup_timeout_secs == 0
            || r.restart_grace_secs == 0
            || r.native_shutdown_timeout_secs == 0
            || r.session_resume_timeout_secs == 0
            || r.mfa_timeout_secs == 0
            || r.ui_stall_timeout_secs == 0
            || r.backoff_initial_secs == 0
            || r.backoff_max_secs < r.backoff_initial_secs
            || r.max_restarts == 0
            || r.healthy_reset_secs == 0
        {
            bail!("recovery limits must be positive and backoff maximum >= initial");
        }
        Ok(())
    }

    pub fn instance(&self, name: &str) -> Result<&Instance> {
        self.instances.get(name).context("unknown instance")
    }

    pub fn directory(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(include_str!("../config.example.toml")).unwrap()
    }

    #[test]
    fn example_is_valid_and_disabled() {
        let c = config();
        c.validate().unwrap();
        assert!(c.instances.values().all(|i| !i.enabled));
    }

    #[test]
    fn rejects_port_collision_and_unsafe_names() {
        let mut c = config();
        c.instances.get_mut("live").unwrap().api_port = 4002;
        assert!(c.validate().is_err());
        let mut c = config();
        let paper = c.instances.remove("paper").unwrap();
        c.instances.insert("../live".into(), paper);
        assert!(c.validate().is_err());
    }

    #[test]
    fn refuses_enabled_placeholder_identity() {
        let mut c = config();
        c.instances.get_mut("live").unwrap().enabled = true;
        assert!(c.validate().is_err());
    }
}
