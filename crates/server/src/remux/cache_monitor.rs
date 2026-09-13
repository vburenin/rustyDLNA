//! One bounded cache worker keeps filesystem maintenance out of child observers.
use super::*;
use std::collections::VecDeque;

const MAX_PENDING: usize = 64; // The process-wide helper gate admits at most 64.
const MAX_RETIRED_PATHS: usize = MAX_PENDING * 5;

#[derive(Default)]
struct State {
    queue: VecDeque<Weak<Check>>,
    reconcile: bool,
    retired: HashSet<PathBuf>,
    running: bool,
}

#[derive(Default)]
pub(crate) struct Worker {
    state: Mutex<State>,
    wake: Condvar,
    stopping: AtomicBool,
    #[cfg(test)]
    waiting: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Ordinary,
    Profile8,
    CacheOnly,
}

pub(super) struct Observation {
    pub(super) length: u64,
    pub(super) playable: bool,
    index: Option<hls::Index>,
    checked: bool,
}

struct Check {
    job: Weak<RemuxJob>,
    kind: Kind,
    pressure_due: bool,
    index: Mutex<Option<hls::Index>>,
    result: Mutex<Option<Result<Observation, String>>>,
    ready: Condvar,
    cancelled: AtomicBool,
}

/// Each attempt owns at most one queued/in-flight check. Dropping an attempt
/// retires its result; a late check cannot mutate a fallback or new generation.
pub(super) struct Monitor {
    pending: Option<Arc<Check>>,
    next: Instant,
    next_pressure: Instant,
    index: Option<hls::Index>,
    kind: Kind,
}

impl Monitor {
    #[cfg(test)]
    pub(super) fn result_ready(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|check| crate::lock_recover(&check.result).is_some())
    }

    pub(super) fn new() -> Self {
        Self {
            pending: None,
            next: Instant::now(),
            next_pressure: Instant::now(),
            index: None,
            kind: Kind::CacheOnly,
        }
    }

    /// Wait only within the caller's budget: at most 5 ms during ordinary child
    /// startup, or one supervision interval after the child has been reaped.
    pub(super) fn wait_pending(&self, timeout: Duration) {
        if let Some(check) = &self.pending {
            let result = crate::lock_recover(&check.result);
            let _wait = check
                .ready
                .wait_timeout_while(result, timeout, |result| result.is_none())
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(super) fn take_ready(&mut self) -> Option<Result<Observation, String>> {
        let result = self
            .pending
            .as_ref()
            .and_then(|check| crate::lock_recover(&check.result).take());
        if result.is_some() {
            self.pending = None;
        }
        result
    }

    pub(super) fn poll(
        &mut self,
        app: &Arc<App>,
        job: &Arc<RemuxJob>,
        kind: Kind,
    ) -> Result<Option<Observation>, String> {
        if self.kind != kind {
            if let Some(check) = self.pending.take() {
                check.cancelled.store(true, Ordering::Release);
            }
            self.index = None;
            self.next = Instant::now();
        }
        let result = self.take_ready();
        let mut observed = None;
        if let Some(result) = result {
            let mut result = result?;
            self.index = result.index.take();
            if result.checked {
                self.next_pressure = Instant::now() + Duration::from_secs(1);
            }
            self.next = if kind == Kind::CacheOnly
                || result.playable
                || matches!(job.state(), RemuxState::Growing)
            {
                self.next_pressure
            } else {
                Instant::now()
            };
            if self.kind == kind {
                observed = Some(result);
            }
        }
        if self.pending.is_none() && (self.kind != kind || Instant::now() >= self.next) {
            let check = Arc::new(Check {
                job: Arc::downgrade(job),
                kind,
                pressure_due: Instant::now() >= self.next_pressure,
                index: Mutex::new(self.index.take()),
                result: Mutex::new(None),
                ready: Condvar::new(),
                cancelled: AtomicBool::new(false),
            });
            app.transcode_cache.monitor.enqueue(app, &check)?;
            self.pending = Some(check);
            self.kind = kind;
        }
        Ok(observed)
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        if let Some(check) = &self.pending {
            check.cancelled.store(true, Ordering::Release);
        }
    }
}

impl Worker {
    fn enqueue(self: &Arc<Self>, app: &Arc<App>, check: &Arc<Check>) -> Result<(), String> {
        let mut state = crate::lock_recover(&self.state);
        state.queue.retain(|item| {
            item.upgrade()
                .is_some_and(|check| !check.cancelled.load(Ordering::Acquire))
        });
        if self.stopping.load(Ordering::Acquire) {
            return Err("cache worker is stopping".into());
        }
        if state.queue.len() >= MAX_PENDING {
            return Err("cache observation queue is full".into());
        }
        state.queue.push_back(Arc::downgrade(check));
        self.start(app, &mut state)
    }

    pub(super) fn reconcile(self: &Arc<Self>, app: &Arc<App>, job: &Arc<RemuxJob>) {
        let mut state = crate::lock_recover(&self.state);
        if !state.reconcile {
            for path in cache::active_artifacts(std::iter::once(job)) {
                if state.retired.len() == MAX_RETIRED_PATHS {
                    // Coalesce arbitrarily many retirements behind a stalled
                    // worker into one eventual reconciliation, with bounded RAM.
                    state.retired.clear();
                    state.reconcile = true;
                    break;
                }
                state.retired.insert(path);
            }
        }
        if let Err(error) = self.start(app, &mut state) {
            tracing::warn!(%error, "cannot schedule cache reconciliation");
        }
    }

    fn start(self: &Arc<Self>, app: &Arc<App>, state: &mut State) -> Result<(), String> {
        if self.stopping.load(Ordering::Acquire) {
            return Err("cache worker is stopping".into());
        }
        if !state.running {
            let worker = self.clone();
            let app = Arc::downgrade(app);
            std::thread::Builder::new()
                .name("remux-cache".into())
                .spawn(move || worker.run(app))
                .map_err(|error| format!("cache worker: {error}"))?;
            state.running = true;
        }
        self.wake.notify_one();
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    /// The caller holds the shared maintenance gate. Every admission consumes
    /// retirements before considering eviction, even if this worker is parked.
    pub(super) fn take_retired(&self) -> (HashSet<PathBuf>, bool) {
        let mut state = crate::lock_recover(&self.state);
        (
            std::mem::take(&mut state.retired),
            std::mem::take(&mut state.reconcile),
        )
    }

    #[cfg(test)]
    pub(super) fn waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    fn run(self: Arc<Self>, app: Weak<App>) {
        loop {
            let (checks, cleanup) = {
                let mut state = crate::lock_recover(&self.state);
                while state.queue.is_empty()
                    && !state.reconcile
                    && state.retired.is_empty()
                    && !self.stopping.load(Ordering::Acquire)
                {
                    state = self
                        .wake
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                if self.stopping.load(Ordering::Acquire) {
                    state.running = false;
                    return;
                }
                let checks = state
                    .queue
                    .drain(..)
                    .filter_map(|check| check.upgrade())
                    .collect::<Vec<_>>();
                (checks, state.reconcile || !state.retired.is_empty())
            };
            let Some(app) = app.upgrade() else {
                crate::lock_recover(&self.state).running = false;
                return;
            };
            let live = || {
                checks.iter().any(|check| {
                    !check.cancelled.load(Ordering::Acquire)
                        && check
                            .job
                            .upgrade()
                            .is_some_and(|job| !job.cancelled.load(Ordering::Acquire))
                })
            };
            // A parked shared gate must not keep this worker alive at shutdown.
            // Once acquired, filesystem calls can still block in the kernel;
            // child termination never joins this worker or waits on its result.
            let cancelled = || {
                self.stopping.load(Ordering::Acquire)
                    || app.scan_cfg.cancellation.is_cancelled()
                    || (!cleanup && !live())
            };
            let mut observations = Vec::new();
            let mut pressure_due = cleanup;
            if !cancelled() {
                for check in &checks {
                    let observation = (|| {
                        let job = check.job.upgrade().ok_or("playback attempt retired")?;
                        if check.cancelled.load(Ordering::Acquire)
                            || job.cancelled.load(Ordering::Acquire)
                        {
                            return Err("playback attempt cancelled".into());
                        }
                        let length = match job.part.metadata() {
                            Ok(metadata) => metadata.len(),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                            Err(error) => return Err(error.to_string()),
                        };
                        let mut index = crate::lock_recover(&check.index).take();
                        let playable = match check.kind {
                            Kind::Ordinary => length >= FIRST_BYTES,
                            Kind::CacheOnly => false,
                            Kind::Profile8 => {
                                matches!(job.state(), RemuxState::Growing)
                                    || profile8::inspect_final_mux(
                                        &job.part,
                                        index.get_or_insert_default(),
                                    )?
                            }
                        };
                        pressure_due |= check.pressure_due
                            || (playable
                                && matches!(
                                    job.state(),
                                    RemuxState::Starting | RemuxState::Preprocessing
                                ));
                        Ok(Observation {
                            length,
                            playable,
                            index,
                            checked: false,
                        })
                    })();
                    observations.push(observation);
                }
            } else {
                observations
                    .resize_with(checks.len(), || Err("cache observation cancelled".into()));
            }
            let result = if pressure_due {
                #[cfg(test)]
                self.waiting.store(true, Ordering::Release);
                let result =
                    cache::maintain_background(&app, cancelled).map_err(|error| error.to_string());
                #[cfg(test)]
                self.waiting.store(false, Ordering::Release);
                result
            } else {
                Ok(())
            };
            for (check, observation) in checks.iter().zip(observations) {
                if !check.cancelled.load(Ordering::Acquire) {
                    *crate::lock_recover(&check.result) =
                        Some(result.clone().and(observation).map(|mut observation| {
                            observation.checked = pressure_due;
                            observation
                        }));
                    check.ready.notify_one();
                }
            }
            // Do not keep App alive while idle; App::drop wakes this worker.
            if app.scan_cfg.cancellation.is_cancelled() {
                self.stop();
            }
            drop(app);
            if self.stopping.load(Ordering::Acquire) {
                crate::lock_recover(&self.state).running = false;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{growing_test_job, temp_dir, test_app, wait_until};
    use super::*;

    #[test]
    fn failed_forced_reconciliation_is_retried_before_admission() {
        let root = temp_dir("cache-reconciliation-retry");
        let dir = root.join("cache");
        std::fs::create_dir(&dir).unwrap();
        let app = test_app(&dir, 1);
        enforce_active_cache_limits(&app).unwrap();
        let scans = runtime_status(&app).cache_scans;
        crate::lock_recover(&app.transcode_cache.monitor.state).reconcile = true;
        let hidden = root.join("temporarily-unavailable");
        std::fs::rename(&dir, &hidden).unwrap();
        assert!(enforce_active_cache_limits(&app).is_err());
        std::fs::rename(&hidden, &dir).unwrap();
        let output = rusty_dlna_transcode::cache_dest_for_key(
            &dir,
            42,
            RecodeAction::Hdr10,
            &"c".repeat(64),
        );
        std::fs::write(&output, b"external output").unwrap();
        assert_eq!(enforce_active_cache_limits(&app).unwrap(), 15);
        assert_eq!(std::fs::read(&output).unwrap(), b"external output");
        assert_eq!(runtime_status(&app).cache_scans, scans + 2);
    }

    #[test]
    fn retired_artifacts_are_refreshed_before_admission_without_scanning() {
        let dir = temp_dir("retired-cache-accounting");
        let app = test_app(&dir, 1);
        let mut job = growing_test_job(&dir, 42, b"cancelled staging");
        let dest = rusty_dlna_transcode::cache_dest_for_key(
            &dir,
            42,
            RecodeAction::Hdr10,
            &"a".repeat(64),
        );
        let part = cache_part(&dest);
        std::fs::rename(&job.part, &part).unwrap();
        Arc::get_mut(&mut job).unwrap().dest = dest;
        Arc::get_mut(&mut job).unwrap().part = part;
        let keeper = rusty_dlna_transcode::cache_dest_for_key(
            &dir,
            43,
            RecodeAction::Hdr10,
            &"b".repeat(64),
        );
        std::fs::write(&keeper, b"completed output").unwrap();
        crate::lock_recover(&app.remuxes).insert("retiring".into(), job.clone());
        assert_eq!(enforce_active_cache_limits(&app).unwrap(), 33);
        let scans = runtime_status(&app).cache_scans;
        let gate = crate::lock_recover(&app.cache_maintenance);
        std::fs::remove_file(&job.part).unwrap();
        crate::lock_recover(&app.remuxes).clear();
        app.transcode_cache.monitor.reconcile(&app, &job);
        // This is a competing admission with the worker still waiting for the
        // same gate. It must consume pending retirements before considering LRU.
        assert_eq!(cache::maintain_locked_measured(&app, false).unwrap(), 16);
        assert_eq!(std::fs::read(&keeper).unwrap(), b"completed output");
        assert!(!job.part.exists());
        assert_eq!(runtime_status(&app).cache_scans, scans);
        drop(gate);
    }

    #[test]
    fn retired_attempt_cannot_publish_a_late_readiness_observation() {
        let dir = temp_dir("retired-cache-observation");
        let app = test_app(&dir, 1);
        let job = growing_test_job(&dir, 42, &vec![0; FIRST_BYTES as usize]);
        job.transition(RemuxState::Starting);
        let gate = crate::lock_recover(&app.cache_maintenance);
        let mut old = Monitor::new();
        assert!(old.poll(&app, &job, Kind::Ordinary).unwrap().is_none());
        wait_until(Duration::from_secs(2), || {
            app.transcode_cache.monitor.waiting()
        });
        drop(old);
        std::fs::write(&job.part, b"new attempt").unwrap();
        let mut replacement = Monitor::new();
        assert!(replacement
            .poll(&app, &job, Kind::Ordinary)
            .unwrap()
            .is_none());
        drop(gate);
        wait_until(Duration::from_secs(2), || replacement.result_ready());
        let observation = replacement
            .poll(&app, &job, Kind::Ordinary)
            .unwrap()
            .unwrap();
        assert!(!observation.playable);
        assert_eq!(observation.length, 11);
        assert_eq!(job.state(), RemuxState::Starting);
        assert!(job.pin_ready_output().unwrap().is_none());
    }
}
