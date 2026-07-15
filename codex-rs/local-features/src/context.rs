use std::collections::VecDeque;

const EWMA_ALPHA: f64 = 0.25;
const MAX_SAMPLES: usize = 8;
const MIN_RESERVE_TOKENS: i64 = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionReason {
    PredictedNextTurnReserve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionDecision {
    Keep,
    Schedule { reason: CompactionReason },
}

#[derive(Debug, Default)]
pub struct AdaptiveContextPolicy {
    turn_deltas: VecDeque<i64>,
    last_usage: Option<i64>,
}

impl AdaptiveContextPolicy {
    pub fn observe_usage(&mut self, active_context_tokens: i64) {
        if let Some(previous) = self.last_usage {
            let delta = active_context_tokens.saturating_sub(previous);
            if delta > 0 {
                if self.turn_deltas.len() == MAX_SAMPLES {
                    self.turn_deltas.pop_front();
                }
                self.turn_deltas.push_back(delta);
            }
        }
        self.last_usage = Some(active_context_tokens);
    }

    pub fn reset_window(&mut self, active_context_tokens: i64) {
        self.last_usage = Some(active_context_tokens);
    }

    pub fn estimated_next_turn_tokens(&self) -> Option<i64> {
        let mut estimate = None;
        for sample in &self.turn_deltas {
            estimate = Some(match estimate {
                None => *sample as f64,
                Some(previous) => EWMA_ALPHA * (*sample as f64) + (1.0 - EWMA_ALPHA) * previous,
            });
        }
        estimate.map(|value| value.ceil() as i64)
    }

    pub fn decide(
        &self,
        active_context_tokens: i64,
        context_window: i64,
        explicit_auto_compact_limit: Option<i64>,
    ) -> CompactionDecision {
        if explicit_auto_compact_limit.is_some() || context_window <= 0 {
            return CompactionDecision::Keep;
        }
        let Some(next_turn) = self.estimated_next_turn_tokens() else {
            return CompactionDecision::Keep;
        };
        let upper = context_window.saturating_mul(35).saturating_div(100);
        let lower = MIN_RESERVE_TOKENS.min(upper);
        let reserve = next_turn.saturating_mul(2).clamp(lower, upper);
        if active_context_tokens >= context_window.saturating_sub(reserve) {
            CompactionDecision::Schedule {
                reason: CompactionReason::PredictedNextTurnReserve,
            }
        } else {
            CompactionDecision::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_when_usage_enters_predicted_reserve() {
        let mut policy = AdaptiveContextPolicy::default();
        policy.observe_usage(10_000);
        policy.observe_usage(20_000);
        policy.observe_usage(30_000);
        assert_eq!(policy.estimated_next_turn_tokens(), Some(10_000));
        assert!(matches!(
            policy.decide(81_000, 100_000, None),
            CompactionDecision::Schedule { .. }
        ));
        assert_eq!(
            policy.decide(79_000, 100_000, None),
            CompactionDecision::Keep
        );
    }

    #[test]
    fn explicit_limit_always_defers_to_canonical_policy() {
        let mut policy = AdaptiveContextPolicy::default();
        policy.observe_usage(1_000);
        policy.observe_usage(20_000);
        assert_eq!(
            policy.decide(99_000, 100_000, Some(50_000)),
            CompactionDecision::Keep
        );
    }

    #[test]
    fn retains_only_eight_recent_samples() {
        let mut policy = AdaptiveContextPolicy::default();
        policy.observe_usage(0);
        for usage in 1..=12 {
            policy.observe_usage(usage * 1_000);
        }
        assert_eq!(policy.turn_deltas.len(), 8);
    }
}
