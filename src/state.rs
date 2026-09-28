use crate::{
    config::{Instance, Recovery},
    ownership::Identity,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Stopped,
    Starting,
    Authenticating,
    AwaitingMfa,
    Configuring,
    Ready,
    Reconnecting,
    NativeRestarting,
    BackingOff,
    NeedsAttention,
    Stopping,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Authenticating => "authenticating",
            Self::AwaitingMfa => "awaiting_mfa",
            Self::Configuring => "configuring",
            Self::Ready => "ready",
            Self::Reconnecting => "reconnecting",
            Self::NativeRestarting => "native_restarting",
            Self::BackingOff => "backing_off",
            Self::NeedsAttention => "needs_attention",
            Self::Stopping => "stopping",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct State {
    pub phase: Phase,
    pub reason: String,
    pub desired_running: bool,
    pub owner: Option<Identity>,
    pub generation: String,
    pub restarts: u32,
    #[serde(default)]
    pub login_recovery_attempts: u32,
    pub profile: Instance,
    #[serde(default)]
    pub backoff_until_unix: i64,
    #[serde(default)]
    pub notification_pending: bool,
    #[serde(default)]
    pub notification_human: bool,
    #[serde(default)]
    pub resume_session: Option<String>,
    #[serde(default)]
    pub launch_pending: bool,
    #[serde(default)]
    pub native_restart_due_unix: Option<i64>,
    #[serde(default)]
    pub require_full_login: bool,
}

impl State {
    pub fn new(profile: Instance) -> Self {
        Self {
            phase: Phase::Stopped,
            reason: "not_started".into(),
            desired_running: true,
            owner: None,
            generation: String::new(),
            restarts: 0,
            login_recovery_attempts: 0,
            profile,
            backoff_until_unix: 0,
            notification_pending: false,
            notification_human: false,
            resume_session: None,
            launch_pending: false,
            native_restart_due_unix: None,
            require_full_login: false,
        }
    }

    pub fn reserve_restart(&mut self, policy: &Recovery) -> Option<u64> {
        if self.restarts >= policy.max_restarts {
            return None;
        }
        let delay = policy
            .backoff_initial_secs
            .saturating_mul(1_u64 << self.restarts.min(32))
            .min(policy.backoff_max_secs);
        self.restarts += 1;
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_budget_is_bounded_and_persistent() {
        let config: crate::config::Config =
            toml::from_str(include_str!("../config.example.toml")).unwrap();
        let mut state = State::new(config.instances["paper"].clone());
        let policy = Recovery::default();
        let delays: Vec<u64> = (0..policy.max_restarts)
            .map(|_| state.reserve_restart(&policy).unwrap())
            .collect();
        assert_eq!(&delays[..5], &[60, 120, 240, 480, 600]);
        assert_eq!(state.reserve_restart(&policy), None);
        let mut restored: State =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(restored.reserve_restart(&policy), None);
    }
}
