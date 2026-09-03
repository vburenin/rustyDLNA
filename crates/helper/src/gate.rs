//! Fair process-wide and per-feature admission gates.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::CancellationToken;

/// Snapshot of bounded media-helper admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HelperMetrics {
    pub active: usize,
    pub queued: usize,
    pub max_active: usize,
    pub queue_capacity: usize,
    pub admitted_total: u64,
    pub saturated_total: u64,
    pub queued_total: u64,
    pub rejected_total: u64,
    pub timed_out_total: u64,
    pub wait_duration_ms_total: u64,
    pub wait_duration_ms_max: u64,
    /// Non-cumulative buckets: <=10, <=50, <=100, <=500, <=1000, +Inf ms.
    pub wait_duration_ms_buckets: [u64; 6],
}

/// Failure to enter the bounded global media-helper queue.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HelperAdmissionError {
    #[error("media helper queue is full")]
    Rejected,
    #[error("media helper queue wait timed out")]
    TimedOut,
    #[error("media helper queue wait cancelled")]
    Cancelled,
}

#[derive(Debug, Default)]
struct HelperGateState {
    active: usize,
    queue: VecDeque<u64>,
    next_ticket: u64,
}

/// Fair process-wide admission for expensive media work.
///
/// Waiters are served FIFO, queue growth is bounded, and the returned permit
/// releases automatically on every success, error, and panic path.
#[derive(Debug)]
pub struct HelperGate {
    max_active: usize,
    queue_capacity: usize,
    state: Mutex<HelperGateState>,
    changed: Condvar,
    admitted_total: AtomicU64,
    saturated_total: AtomicU64,
    queued_total: AtomicU64,
    rejected_total: AtomicU64,
    timed_out_total: AtomicU64,
    wait_duration_ms_total: AtomicU64,
    wait_duration_ms_max: AtomicU64,
    wait_duration_ms_buckets: [AtomicU64; 6],
}

impl HelperGate {
    pub fn new(max_active: usize, queue_capacity: usize) -> Self {
        Self {
            max_active: max_active.max(1),
            queue_capacity,
            state: Mutex::new(HelperGateState::default()),
            changed: Condvar::new(),
            admitted_total: AtomicU64::new(0),
            saturated_total: AtomicU64::new(0),
            queued_total: AtomicU64::new(0),
            rejected_total: AtomicU64::new(0),
            timed_out_total: AtomicU64::new(0),
            wait_duration_ms_total: AtomicU64::new(0),
            wait_duration_ms_max: AtomicU64::new(0),
            wait_duration_ms_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, HelperGateState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn admit(self: &Arc<Self>, state: &mut HelperGateState) -> HelperPermit {
        state.active += 1;
        self.admitted_total.fetch_add(1, Ordering::Relaxed);
        HelperPermit {
            gate: Arc::clone(self),
        }
    }

    fn observe_wait(&self, started: Instant) {
        let millis = crate::duration_millis_saturating(started.elapsed());
        let bucket = [10, 50, 100, 500, 1_000]
            .iter()
            .position(|bound| millis <= *bound)
            .unwrap_or(5);
        self.wait_duration_ms_total
            .fetch_add(millis, Ordering::Relaxed);
        self.wait_duration_ms_max
            .fetch_max(millis, Ordering::Relaxed);
        self.wait_duration_ms_buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Immediate admission. A queued waiter is never bypassed.
    pub fn try_acquire(self: &Arc<Self>) -> Result<HelperPermit, HelperAdmissionError> {
        let mut state = self.lock_state();
        if state.active < self.max_active && state.queue.is_empty() {
            return Ok(self.admit(&mut state));
        }
        self.saturated_total.fetch_add(1, Ordering::Relaxed);
        self.rejected_total.fetch_add(1, Ordering::Relaxed);
        Err(HelperAdmissionError::Rejected)
    }

    pub fn acquire_timeout(
        self: &Arc<Self>,
        timeout: Duration,
    ) -> Result<HelperPermit, HelperAdmissionError> {
        self.acquire_timeout_until(timeout, || false)
    }

    pub fn acquire_timeout_cancelled(
        self: &Arc<Self>,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<HelperPermit, HelperAdmissionError> {
        self.acquire_timeout_until(timeout, || cancellation.is_cancelled())
    }

    fn acquire_timeout_until(
        self: &Arc<Self>,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
    ) -> Result<HelperPermit, HelperAdmissionError> {
        let mut state = self.lock_state();
        if cancelled() {
            return Err(HelperAdmissionError::Cancelled);
        }
        if state.active < self.max_active && state.queue.is_empty() {
            return Ok(self.admit(&mut state));
        }
        self.saturated_total.fetch_add(1, Ordering::Relaxed);
        if state.queue.len() >= self.queue_capacity {
            self.rejected_total.fetch_add(1, Ordering::Relaxed);
            return Err(HelperAdmissionError::Rejected);
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.queue.push_back(ticket);
        self.queued_total.fetch_add(1, Ordering::Relaxed);
        let wait_started = Instant::now();
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if cancelled() {
                if let Some(position) = state.queue.iter().position(|queued| *queued == ticket) {
                    state.queue.remove(position);
                }
                self.observe_wait(wait_started);
                self.changed.notify_all();
                return Err(HelperAdmissionError::Cancelled);
            }
            if state.active < self.max_active && state.queue.front() == Some(&ticket) {
                state.queue.pop_front();
                self.observe_wait(wait_started);
                return Ok(self.admit(&mut state));
            }
            let remaining =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                if let Some(position) = state.queue.iter().position(|queued| *queued == ticket) {
                    state.queue.remove(position);
                }
                self.timed_out_total.fetch_add(1, Ordering::Relaxed);
                self.observe_wait(wait_started);
                self.changed.notify_all();
                return Err(HelperAdmissionError::TimedOut);
            }
            let waited = self.changed.wait_timeout(
                state,
                remaining
                    .unwrap_or(Duration::from_millis(20))
                    .min(Duration::from_millis(20)),
            );
            state = match waited {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    pub fn metrics(&self) -> HelperMetrics {
        let state = self.lock_state();
        HelperMetrics {
            active: state.active,
            queued: state.queue.len(),
            max_active: self.max_active,
            queue_capacity: self.queue_capacity,
            admitted_total: self.admitted_total.load(Ordering::Relaxed),
            saturated_total: self.saturated_total.load(Ordering::Relaxed),
            queued_total: self.queued_total.load(Ordering::Relaxed),
            rejected_total: self.rejected_total.load(Ordering::Relaxed),
            timed_out_total: self.timed_out_total.load(Ordering::Relaxed),
            wait_duration_ms_total: self.wait_duration_ms_total.load(Ordering::Relaxed),
            wait_duration_ms_max: self.wait_duration_ms_max.load(Ordering::Relaxed),
            wait_duration_ms_buckets: std::array::from_fn(|index| {
                self.wait_duration_ms_buckets[index].load(Ordering::Relaxed)
            }),
        }
    }
}

#[derive(Debug)]
pub struct HelperPermit {
    gate: Arc<HelperGate>,
}

impl Drop for HelperPermit {
    fn drop(&mut self) {
        let mut state = self.gate.lock_state();
        state.active = state.active.saturating_sub(1);
        self.gate.changed.notify_all();
    }
}

/// Independent cap for concurrently running title-level remux jobs.
#[derive(Clone, Debug)]
pub struct JobGate {
    inner: Arc<JobGateInner>,
}

#[derive(Debug)]
struct JobGateInner {
    max: usize,
    state: Mutex<JobGateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct JobGateState {
    manual: usize,
    permits: usize,
}

impl JobGateState {
    fn in_use(&self) -> usize {
        self.manual.saturating_add(self.permits)
    }
}

impl JobGateInner {
    fn lock_state(&self) -> MutexGuard<'_, JobGateState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl JobGate {
    pub fn new(max: usize) -> Self {
        Self {
            inner: Arc::new(JobGateInner {
                max: max.max(1),
                state: Mutex::new(JobGateState::default()),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn in_use(&self) -> usize {
        self.inner.lock_state().in_use()
    }

    /// Reserve one title slot without a lifetime-bound permit.
    ///
    /// This is retained for compatibility with the original transcode-crate
    /// API. New orchestration should use [`Self::try_acquire`] so unwind and
    /// early-return paths release automatically.
    pub fn try_add(&self) -> bool {
        let mut state = self.inner.lock_state();
        if state.in_use() >= self.inner.max {
            return false;
        }
        state.manual = state.manual.saturating_add(1);
        true
    }

    /// Release a slot reserved through [`Self::try_add`].
    ///
    /// RAII callers release by dropping their [`JobPermit`] instead.
    pub fn release(&self) {
        let mut state = self.inner.lock_state();
        state.manual = state.manual.saturating_sub(1);
        self.inner.changed.notify_all();
    }

    /// Acquire one title slot. The permit releases the slot automatically.
    pub fn try_acquire(&self) -> Option<JobPermit> {
        let mut state = self.inner.lock_state();
        if state.in_use() >= self.inner.max {
            return None;
        }
        state.permits = state.permits.saturating_add(1);
        Some(JobPermit {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Wait for a title slot for at most `timeout`.
    ///
    /// This is intended for bounded handoffs where the caller has already
    /// cancelled the producer that owns the slot. Ordinary admission remains
    /// immediate through [`Self::try_acquire`].
    pub fn acquire_timeout(&self, timeout: Duration) -> Option<JobPermit> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.inner.lock_state();
        loop {
            if state.in_use() < self.inner.max {
                state.permits = state.permits.saturating_add(1);
                return Some(JobPermit {
                    inner: Arc::clone(&self.inner),
                });
            }
            let remaining =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                return None;
            }
            let waited = self.inner.changed.wait_timeout(
                state,
                remaining
                    .unwrap_or(Duration::from_millis(20))
                    .min(Duration::from_millis(20)),
            );
            state = match waited {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

#[derive(Debug)]
pub struct JobPermit {
    inner: Arc<JobGateInner>,
}

impl Drop for JobPermit {
    fn drop(&mut self) {
        let mut state = self.inner.lock_state();
        state.permits = state.permits.saturating_sub(1);
        self.inner.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_permits_release_and_metrics_are_bounded() {
        let gate = Arc::new(HelperGate::new(1, 1));
        let permit = gate.try_acquire().expect("first helper");
        assert!(matches!(
            gate.try_acquire(),
            Err(HelperAdmissionError::Rejected)
        ));
        assert_eq!(gate.metrics().active, 1);
        drop(permit);
        assert!(gate.try_acquire().is_ok());
        let metrics = gate.metrics();
        assert_eq!(metrics.max_active, 1);
        assert_eq!(metrics.queue_capacity, 1);
        assert_eq!(metrics.rejected_total, 1);
    }

    #[test]
    fn queued_wait_can_time_out_or_cancel() {
        let gate = Arc::new(HelperGate::new(1, 2));
        let _permit = gate.try_acquire().expect("hold helper");
        assert!(matches!(
            gate.acquire_timeout(Duration::from_millis(1)),
            Err(HelperAdmissionError::TimedOut)
        ));
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        assert!(matches!(
            gate.acquire_timeout_cancelled(Duration::from_secs(1), &cancellation),
            Err(HelperAdmissionError::Cancelled)
        ));
        assert_eq!(gate.metrics().timed_out_total, 1);
    }

    #[test]
    fn unrepresentable_timeout_remains_cancellable_without_panicking() {
        let gate = Arc::new(HelperGate::new(1, 1));
        let _permit = gate.try_acquire().expect("hold helper");
        let cancellation = CancellationToken::default();
        let waiter_gate = Arc::clone(&gate);
        let waiter_cancellation = cancellation.clone();
        let waiter = std::thread::spawn(move || {
            waiter_gate.acquire_timeout_cancelled(Duration::MAX, &waiter_cancellation)
        });
        let queued_deadline = Instant::now() + Duration::from_secs(1);
        while gate.metrics().queued == 0 {
            assert!(
                Instant::now() < queued_deadline,
                "waiter did not enter queue"
            );
            std::thread::yield_now();
        }
        cancellation.cancel();
        assert!(matches!(
            waiter.join().expect("join waiter"),
            Err(HelperAdmissionError::Cancelled)
        ));
        assert_eq!(gate.metrics().queued, 0);
    }

    #[test]
    fn title_slots_are_raii_and_never_over_admit() {
        let gate = JobGate::new(1);
        let permit = gate.try_acquire().expect("first job");
        assert!(gate.try_acquire().is_none());
        assert_eq!(gate.in_use(), 1);
        drop(permit);
        assert_eq!(gate.in_use(), 0);
        assert!(gate.try_acquire().is_some());
    }

    #[test]
    fn cloned_title_gate_handles_share_permits() {
        let gate = JobGate::new(1);
        let cloned = gate.clone();
        let permit = cloned.try_acquire().expect("cloned handle acquires");
        assert_eq!(gate.in_use(), 1);
        assert!(gate.try_acquire().is_none());
        drop(permit);
        assert_eq!(gate.in_use(), 0);
    }

    #[test]
    fn title_slot_timed_wait_observes_permit_release_and_timeout() {
        let gate = JobGate::new(1);
        let permit = gate.try_acquire().expect("hold title slot");
        assert!(gate.acquire_timeout(Duration::from_millis(1)).is_none());

        let waiting_gate = gate.clone();
        let waiter =
            std::thread::spawn(move || waiting_gate.acquire_timeout(Duration::from_secs(1)));
        std::thread::sleep(Duration::from_millis(10));
        drop(permit);
        let handed_off = waiter.join().expect("join title-slot waiter");
        assert!(handed_off.is_some());
    }

    #[test]
    fn legacy_title_slot_methods_remain_compatible() {
        let gate = JobGate::new(1);
        gate.release();
        assert_eq!(gate.in_use(), 0);
        assert!(gate.try_add());
        assert!(!gate.try_add());
        gate.release();
        assert_eq!(gate.in_use(), 0);
        gate.release();
        assert_eq!(gate.in_use(), 0);
        assert!(gate.try_add());

        let gate = JobGate::new(1);
        let permit = gate.try_acquire().expect("RAII compatibility permit");
        gate.release();
        assert_eq!(gate.in_use(), 1);
        assert!(gate.try_acquire().is_none());
        drop(permit);
        assert_eq!(gate.in_use(), 0);
        assert!(gate.try_add());
    }

    #[test]
    fn manual_and_raii_title_slots_release_only_their_own_reservations() {
        let gate = JobGate::new(2);
        assert!(gate.try_add());
        let first_permit = gate.try_acquire().expect("RAII slot beside manual slot");
        assert_eq!(gate.in_use(), 2);

        drop(first_permit);
        assert_eq!(gate.in_use(), 1);
        let second_permit = gate.try_acquire().expect("released RAII slot");
        assert_eq!(gate.in_use(), 2);

        gate.release();
        assert_eq!(gate.in_use(), 1);
        let third_permit = gate.try_acquire().expect("released manual slot");
        assert_eq!(gate.in_use(), 2);
        drop(second_permit);
        drop(third_permit);
        assert_eq!(gate.in_use(), 0);
    }

    #[test]
    fn title_slot_releases_during_unwind() {
        let gate = Arc::new(JobGate::new(1));
        let unwind_gate = Arc::clone(&gate);
        let result = std::panic::catch_unwind(move || {
            let _permit = unwind_gate.try_acquire().expect("job permit");
            panic!("exercise permit drop");
        });
        assert!(result.is_err());
        assert_eq!(gate.in_use(), 0);
    }

    #[test]
    fn helper_slot_releases_during_unwind() {
        let gate = Arc::new(HelperGate::new(1, 0));
        let unwind_gate = Arc::clone(&gate);
        let result = std::panic::catch_unwind(move || {
            let _permit = unwind_gate.try_acquire().expect("helper permit");
            panic!("exercise helper permit drop");
        });
        assert!(result.is_err());
        assert_eq!(gate.metrics().active, 0);
    }

    #[test]
    fn failed_thread_spawn_drops_captured_permits() {
        let helpers = Arc::new(HelperGate::new(1, 0));
        let jobs = Arc::new(JobGate::new(1));
        let helper_permit = helpers.try_acquire().expect("helper permit");
        let job_permit = jobs.try_acquire().expect("job permit");
        let spawned = std::thread::Builder::new()
            .stack_size(usize::MAX)
            .spawn(move || {
                let _helper_permit = helper_permit;
                let _job_permit = job_permit;
            });
        assert!(
            spawned.is_err(),
            "impossible stack size unexpectedly spawned"
        );
        assert_eq!(helpers.metrics().active, 0);
        assert_eq!(jobs.in_use(), 0);
    }
}
