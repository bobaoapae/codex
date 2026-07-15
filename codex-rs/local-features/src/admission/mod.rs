use std::time::Duration;
use std::time::Instant;

const AGING_AFTER: Duration = Duration::from_secs(2 * 60);
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(5 * 60);
const RECOVERY_COMPLETIONS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPriority {
    UserDirected,
    ActiveGoalCriticalPath,
    Requested,
    Proactive,
}

impl AdmissionPriority {
    fn rank(self) -> u8 {
        match self {
            Self::UserDirected => 0,
            Self::ActiveGoalCriticalPath => 1,
            Self::Requested => 2,
            Self::Proactive => 3,
        }
    }
}

#[derive(Debug)]
pub struct AdmissionRequest<T> {
    pub request_id: u64,
    pub parent_id: Option<String>,
    pub priority: AdmissionPriority,
    pub payload: T,
    enqueued_at: Instant,
    sequence: u64,
}

#[derive(Debug)]
pub struct AdmissionController<T> {
    hard_limit: usize,
    effective_limit: usize,
    running: usize,
    next_sequence: u64,
    completions_without_pressure: usize,
    last_pressure_at: Option<Instant>,
    queue: Vec<AdmissionRequest<T>>,
}

impl<T> AdmissionController<T> {
    pub fn new(hard_limit: usize) -> Self {
        let hard_limit = hard_limit.max(1);
        Self {
            hard_limit,
            effective_limit: hard_limit.min(4),
            running: 0,
            next_sequence: 0,
            completions_without_pressure: 0,
            last_pressure_at: None,
            queue: Vec::new(),
        }
    }

    pub fn effective_limit(&self) -> usize {
        self.effective_limit
    }

    pub fn running(&self) -> usize {
        self.running
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn enqueue(
        &mut self,
        request_id: u64,
        parent_id: Option<String>,
        priority: AdmissionPriority,
        payload: T,
        now: Instant,
    ) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.queue.push(AdmissionRequest {
            request_id,
            parent_id,
            priority,
            payload,
            enqueued_at: now,
            sequence,
        });
    }

    pub fn admit_next(&mut self, now: Instant) -> Option<AdmissionRequest<T>> {
        if self.running >= self.effective_limit || self.running >= self.hard_limit {
            return None;
        }
        let index = self
            .queue
            .iter()
            .enumerate()
            .min_by_key(|(_, request)| {
                let aged = now.saturating_duration_since(request.enqueued_at) >= AGING_AFTER;
                let rank = request.priority.rank().saturating_sub(u8::from(aged));
                (rank, request.sequence)
            })
            .map(|(index, _)| index)?;
        self.running = self.running.saturating_add(1);
        Some(self.queue.remove(index))
    }

    pub fn cancel_parent(&mut self, parent_id: &str) -> Vec<AdmissionRequest<T>> {
        let mut cancelled = Vec::new();
        let mut retained = Vec::with_capacity(self.queue.len());
        for request in self.queue.drain(..) {
            if request.parent_id.as_deref() == Some(parent_id) {
                cancelled.push(request);
            } else {
                retained.push(request);
            }
        }
        self.queue = retained;
        cancelled
    }

    pub fn record_capacity_pressure(&mut self, now: Instant) {
        self.effective_limit = (self.effective_limit / 2).max(1);
        self.completions_without_pressure = 0;
        self.last_pressure_at = Some(now);
    }

    pub fn record_completion(&mut self, now: Instant) {
        self.running = self.running.saturating_sub(1);
        self.completions_without_pressure = self.completions_without_pressure.saturating_add(1);
        let cooldown_elapsed = self
            .last_pressure_at
            .is_none_or(|pressure| now.saturating_duration_since(pressure) >= RECOVERY_COOLDOWN);
        if self.completions_without_pressure >= RECOVERY_COMPLETIONS
            && cooldown_elapsed
            && self.effective_limit < self.hard_limit
        {
            self.effective_limit += 1;
            self.completions_without_pressure = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_is_fifo_and_aging_promotes_one_level() {
        let start = Instant::now();
        let mut controller = AdmissionController::new(4);
        controller.enqueue(1, None, AdmissionPriority::Proactive, "old", start);
        controller.enqueue(
            2,
            None,
            AdmissionPriority::Requested,
            "new",
            start + AGING_AFTER,
        );
        let admitted = controller
            .admit_next(start + AGING_AFTER)
            .expect("admitted request");
        assert_eq!(admitted.request_id, 1);
    }

    #[test]
    fn pressure_halves_and_five_completions_recover_after_cooldown() {
        let start = Instant::now();
        let mut controller = AdmissionController::<()>::new(8);
        assert_eq!(controller.effective_limit(), 4);
        controller.record_capacity_pressure(start);
        assert_eq!(controller.effective_limit(), 2);
        for _ in 0..5 {
            controller.record_completion(start + RECOVERY_COOLDOWN);
        }
        assert_eq!(controller.effective_limit(), 3);
    }

    #[test]
    fn hard_and_effective_limits_are_never_exceeded() {
        let now = Instant::now();
        let mut controller = AdmissionController::new(2);
        for request_id in 0..3 {
            controller.enqueue(request_id, None, AdmissionPriority::Requested, (), now);
        }
        assert!(controller.admit_next(now).is_some());
        assert!(controller.admit_next(now).is_some());
        assert!(controller.admit_next(now).is_none());
        assert_eq!(controller.running(), 2);
    }

    #[test]
    fn cancellation_removes_only_matching_parent_requests() {
        let now = Instant::now();
        let mut controller = AdmissionController::new(4);
        controller.enqueue(1, Some("a".into()), AdmissionPriority::Requested, (), now);
        controller.enqueue(2, Some("b".into()), AdmissionPriority::Requested, (), now);
        assert_eq!(controller.cancel_parent("a").len(), 1);
        assert_eq!(controller.queued(), 1);
    }
}
