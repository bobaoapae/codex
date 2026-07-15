use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalErrorClass {
    Capacity,
    TransientNetwork,
    UsageLimit,
    Policy,
    Sandbox,
    Cancelled,
    Other(String),
}

impl GoalErrorClass {
    pub fn key(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "other".to_string())
    }

    fn recoverable(&self) -> bool {
        matches!(
            self,
            Self::Capacity | Self::TransientNetwork | Self::Other(_)
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSupervisorState {
    pub retry_sequence: u64,
    pub error_class: Option<String>,
    pub not_before: i64,
    pub blocker_count: u32,
    pub last_activity: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalSupervisorDecision {
    Continue { not_before: i64 },
    Block,
    StopNonRecoverable,
}

impl GoalSupervisorState {
    pub fn record_error(
        &mut self,
        error: &GoalErrorClass,
        now_unix_seconds: i64,
    ) -> GoalSupervisorDecision {
        self.last_activity = now_unix_seconds;
        let key = error.key();
        if self.error_class.as_deref() == Some(key.as_str()) {
            self.blocker_count = self.blocker_count.saturating_add(1);
        } else {
            self.error_class = Some(key);
            self.blocker_count = 1;
            self.retry_sequence = 0;
        }
        if !error.recoverable() {
            return GoalSupervisorDecision::StopNonRecoverable;
        }
        if self.blocker_count >= 3 {
            return GoalSupervisorDecision::Block;
        }
        self.retry_sequence = self.retry_sequence.saturating_add(1);
        let delay = 5_i64
            .saturating_mul(i64::try_from(self.retry_sequence).unwrap_or(i64::MAX))
            .min(60);
        self.not_before = now_unix_seconds.saturating_add(delay);
        GoalSupervisorDecision::Continue {
            not_before: self.not_before,
        }
    }

    pub fn record_success(&mut self, now_unix_seconds: i64) {
        self.retry_sequence = 0;
        self.error_class = None;
        self.not_before = 0;
        self.blocker_count = 0;
        self.last_activity = now_unix_seconds;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn third_consecutive_blocker_blocks_goal() {
        let mut state = GoalSupervisorState::default();
        assert!(matches!(
            state.record_error(&GoalErrorClass::Capacity, 100),
            GoalSupervisorDecision::Continue { .. }
        ));
        assert!(matches!(
            state.record_error(&GoalErrorClass::Capacity, 110),
            GoalSupervisorDecision::Continue { .. }
        ));
        assert_eq!(
            state.record_error(&GoalErrorClass::Capacity, 120),
            GoalSupervisorDecision::Block
        );
    }

    #[test]
    fn changed_blocker_restarts_consecutive_audit() {
        let mut state = GoalSupervisorState::default();
        state.record_error(&GoalErrorClass::Capacity, 100);
        state.record_error(&GoalErrorClass::Capacity, 110);
        assert!(matches!(
            state.record_error(&GoalErrorClass::TransientNetwork, 120),
            GoalSupervisorDecision::Continue { .. }
        ));
        assert_eq!(state.blocker_count, 1);
    }

    #[test]
    fn policy_usage_and_sandbox_are_not_retried() {
        for error in [
            GoalErrorClass::UsageLimit,
            GoalErrorClass::Policy,
            GoalErrorClass::Sandbox,
            GoalErrorClass::Cancelled,
        ] {
            assert_eq!(
                GoalSupervisorState::default().record_error(&error, 100),
                GoalSupervisorDecision::StopNonRecoverable
            );
        }
    }
}
