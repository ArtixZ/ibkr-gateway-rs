use crate::ownership;
use anyhow::{ensure, Result};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const MAX_LOG: u64 = 2 * 1024 * 1024;

pub fn allowed_native_line(text: &str) -> bool {
    [
        "Gateway native restart journal could not be recorded",
        "Gateway bridge connection unavailable; retrying private supervisor connection",
        "Gateway bridge socket close failed",
        "Gateway bridge event queue full; automation paused",
        "Gateway output capture failed",
    ]
    .contains(&text)
}

fn local_day(now: OffsetDateTime) -> Result<String> {
    let timestamp = now.unix_timestamp() as libc::time_t;
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    ensure!(
        !unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null(),
        "cannot determine tradebus local event date"
    );
    Ok(format!(
        "{:04}-{:02}-{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday
    ))
}

pub struct Events {
    pub instance: String,
    directory: PathBuf,
    tradebus: Option<PathBuf>,
}

impl Events {
    pub fn new(instance: &str, directory: &Path, tradebus: Option<PathBuf>) -> Result<Self> {
        let directory = directory.join("logs");
        ownership::private_dir(&directory)?;
        Ok(Self {
            instance: instance.into(),
            directory,
            tradebus,
        })
    }

    pub fn log(&self, kind: &str, message: &str) -> Result<()> {
        let path = self.directory.join("controller.jsonl");
        if path.exists() && fs::metadata(&path)?.len() >= MAX_LOG {
            for index in (1..4).rev() {
                let from = self.directory.join(format!("controller.{index}.jsonl"));
                if from.exists() {
                    fs::rename(
                        from,
                        self.directory
                            .join(format!("controller.{}.jsonl", index + 1)),
                    )?;
                }
            }
            fs::rename(&path, self.directory.join("controller.1.jsonl"))?;
        }
        let message: String = message.chars().take(2048).collect();
        let event = json!({
            "ts": OffsetDateTime::now_utc().format(&Rfc3339)?,
            "instance": self.instance, "kind": kind, "message": message
        });
        append(&path, &event)?;
        Ok(())
    }

    pub fn transition(&self, phase: &str, reason: &str, human: bool) -> Result<()> {
        self.log("transition", &format!("{phase}: {reason}"))?;
        if !human && phase != "ready" {
            return Ok(());
        }
        let Some(directory) = &self.tradebus else {
            return Ok(());
        };
        ensure!(
            directory.is_dir(),
            "configured tradebus event directory does not exist"
        );
        let now = OffsetDateTime::now_utc();
        let event = json!({
            "ts": now.format(&Rfc3339)?,
            "source": "gateway", "kind": "notify",
            "severity": if phase == "ready" { "ok" } else if human { "error" } else { "info" },
            "title": format!("Gateway {}: {}", self.instance, phase),
            "body": reason,
            "data": {
                "instance": self.instance,
                "human_notify": human,
                "watchtower_slug": format!("ibkr-gateway-{}-session", self.instance),
                "watchtower_oneshot": "true",
                "watchtower_status": if phase == "ready" { "ok" } else { "fail" }
            }
        });
        let date = local_day(now)?;
        append(&directory.join(format!("events-{date}.jsonl")), &event)
    }

    pub fn heartbeat(&self, phase: &str) -> Result<()> {
        let Some(directory) = &self.tradebus else {
            return Ok(());
        };
        ensure!(
            directory.is_dir(),
            "configured tradebus event directory does not exist"
        );
        let now = OffsetDateTime::now_utc();
        let event = json!({
            "ts": now.format(&Rfc3339)?,
            "source": "gateway", "kind": "notify", "severity": "heartbeat",
            "title": format!("Gateway {} monitor heartbeat", self.instance),
            "body": format!("gateway={phase}"),
            "data": {
                "instance": self.instance, "human_notify": false,
                "watchtower_slug": format!("ibkr-gateway-{}-monitor", self.instance),
                "watchtower_oneshot": "false",
                "watchtower_interval": "3600", "watchtower_grace": "600",
                "watchtower_status": "ok"
            }
        });
        let date = local_day(now)?;
        append(&directory.join(format!("events-{date}.jsonl")), &event)
    }

    pub fn upgrade_notice(&self) -> Result<()> {
        let message = "IBKR warned that this Gateway version will be desupported. \
            Plan a software upgrade before the vendor deadline; the advisory does not block this session.";
        self.log("upgrade_notice", message)?;
        let Some(directory) = &self.tradebus else {
            return Ok(());
        };
        ensure!(
            directory.is_dir(),
            "configured tradebus event directory does not exist"
        );
        let now = OffsetDateTime::now_utc();
        let event = json!({
            "ts": now.format(&Rfc3339)?,
            "source": "gateway", "kind": "notify", "severity": "warn",
            "title": format!("Gateway {}: software upgrade recommended", self.instance),
            "body": message,
            "data": {
                "instance": self.instance,
                "human_notify": true
            }
        });
        let date = local_day(now)?;
        append(&directory.join(format!("events-{date}.jsonl")), &event)
    }

    pub fn upgrade_status(&self, phase: &str, message: &str, human: bool) -> Result<()> {
        self.log("vendor_upgrade", &format!("{phase}: {message}"))?;
        if !human && phase != "ready" {
            return Ok(());
        }
        let Some(directory) = &self.tradebus else {
            return Ok(());
        };
        ensure!(directory.is_dir(), "configured tradebus event directory does not exist");
        let now = OffsetDateTime::now_utc();
        let event = json!({
            "ts": now.format(&Rfc3339)?,
            "source": "gateway", "kind": "notify",
            "severity": if human { "error" } else { "ok" },
            "title": format!("Gateway {} upgrade: {phase}", self.instance),
            "body": message,
            "data": {
                "instance": self.instance, "human_notify": human,
                "watchtower_slug": format!("ibkr-gateway-{}-upgrade", self.instance),
                "watchtower_oneshot": "true",
                "watchtower_status": if human { "fail" } else { "ok" }
            }
        });
        append(&directory.join(format!("events-{}.jsonl", local_day(now)?)), &event)
    }

    pub fn native_log(&self, text: &str) -> Result<()> {
        if allowed_native_line(text) {
            self.log("gateway", text)
        } else {
            self.log(
                "gateway",
                "[native Gateway output omitted; inspect its private settings logs locally]",
            )
        }
    }
}

fn append(path: &Path, event: &serde_json::Value) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "log locking failed"
    );
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let original_length = file.metadata()?.len();
    if let Err(error) = file.write_all(&line) {
        file.set_len(original_length)?;
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_retirement_notice_does_not_report_or_clear_a_session_outage() {
        let dir = tempfile::tempdir().unwrap();
        let e = Events::new("paper", dir.path(), Some(dir.path().into())).unwrap();
        e.upgrade_notice().unwrap();
        e.transition("ready", "verified", false).unwrap();
        let event_file = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("events-")
            })
            .unwrap();
        let events: Vec<serde_json::Value> = fs::read_to_string(event_file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["severity"], "warn");
        assert_eq!(events[0]["data"]["human_notify"], true);
        assert_eq!(events[0]["data"]["instance"], "paper");
        assert!(events[0]["data"].get("watchtower_slug").is_none());
        assert!(events[0]["data"].get("watchtower_status").is_none());
        assert_eq!(
            events[1]["data"]["watchtower_slug"],
            "ibkr-gateway-paper-session"
        );
        assert_eq!(events[1]["data"]["watchtower_status"], "ok");
    }

    #[test]
    fn native_restart_journal_failure_remains_visible() {
        assert!(allowed_native_line(
            "Gateway native restart journal could not be recorded"
        ));
    }
    #[test]
    fn credentials_redacted_and_incident_ids_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let e = Events::new("paper", dir.path(), Some(dir.path().into())).unwrap();
        let marker = ["unstructured", "fixture", "value"].join("-");
        e.native_log(&marker).unwrap();
        e.transition("ready", "verified", false).unwrap();
        let logs = fs::read_to_string(dir.path().join("logs/controller.jsonl")).unwrap();
        assert!(!logs.contains(&marker));
        let event_file = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("events-")
            })
            .unwrap();
        let event = fs::read_to_string(event_file).unwrap();
        assert!(event.contains("ibkr-gateway-paper-session"));
        assert!(!event.contains("\"ibkr-gateway-session\""));
    }
    #[test]
    fn planned_restarts_do_not_open_outage_incidents() {
        let dir = tempfile::tempdir().unwrap();
        let e = Events::new("live", dir.path(), Some(dir.path().into())).unwrap();
        e.transition("native_restarting", "scheduled", false)
            .unwrap();
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("events-")));
        e.transition("awaiting_mfa", "approval_required", true)
            .unwrap();
        assert!(fs::read_dir(dir.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("events-")));
    }
}
