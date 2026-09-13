//! One background remux per title. Concurrent GETs share one producer; each
//! route decides whether its producer may outlive the last HTTP reader.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use rusty_dlna_http::{
    live_transcode_response, media_response, now_imf_date, parse_byte_range, parse_open_range,
    HttpRequest, HttpResponse, RangeError, RemuxAudio, RemuxJobSpec,
};
use rusty_dlna_transcode::{
    cache_is_fresh_for_key, cache_part, run_remux_p8_with_toolchain_stage_observed,
    write_cache_stamp_for_key, BrowserOutputOptions, RecodeAction, RemuxP8Error, RemuxP8Input,
    RemuxP8Stage, RemuxP8StageEvent, RemuxP8StageStatus, TranscodeCacheIdentity, TranscodePlan,
};

use crate::App;

mod cache;
mod cache_monitor;
pub(crate) mod fallback;
mod hls;
pub(crate) mod performance;
mod positional;
mod profile8;
#[cfg(test)]
mod validation_tests;

#[cfg(test)]
use cache::enforce_active_cache_limits;
use cache::maintain_app_cache;
pub(crate) use cache::{maintain_transcode_cache, CacheCoordinator};

const FIRST_BYTES: u64 = 16 * 1024;
const FIRST_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
// Chromium commonly pauses range reads while it parses a new fragmented MP4
// or refills its media pipeline. Keep the producer around long enough for the
// next request instead of treating that normal gap as abandonment.
const WEB_RECONNECT_GRACE: Duration = Duration::from_secs(30);
// An active browser periodically renews its generation while its media engine
// plays from already-buffered bytes. Chromium can leave no HTTP range reader
// attached for longer than the short reconnect grace during that normal gap.
// Bound a lost page/network by a longer lease without cancelling a generation
// that the browser still owns.
const WEB_ACTIVE_SESSION_LEASE: Duration = Duration::from_secs(2 * 60);
const WEB_EPHEMERAL_RETENTION: Duration = Duration::from_secs(30);
const MAX_WEB_PLAYBACK_SESSIONS: usize = 1024;
const WEB_SESSION_RETENTION: Duration = Duration::from_secs(10 * 60);
const WEB_PREPARATION_RETENTION: Duration = Duration::from_secs(2 * 60);
// A replacement browser rendition must not fail admission while the producer
// it just cancelled is still completing the bounded TERM-to-KILL handoff.
const WEB_SUPERSEDED_JOB_HANDOFF: Duration = Duration::from_secs(2);
const MAX_WEB_TRANSCODE_PREPARATIONS: usize = 64;
const WEB_REQUEST_CANCELLED: &str = "web playback request superseded";
const REMUX_CANCELLED: &str = "remux cancelled";
pub(crate) const MAX_MSE_FRAGMENT_CURSOR: usize = hls::MAX_INDEX_FRAGMENTS;

#[cfg(test)]
type P8TestRunner = fn(
    &Path,
    Instant,
    &AtomicBool,
    &mut dyn FnMut(RemuxP8StageEvent) -> Result<(), String>,
) -> Result<(), RemuxP8Error>;

#[cfg(test)]
fn p8_test_runners() -> &'static Mutex<HashMap<PathBuf, P8TestRunner>> {
    static RUNNERS: std::sync::OnceLock<Mutex<HashMap<PathBuf, P8TestRunner>>> =
        std::sync::OnceLock::new();
    RUNNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_profile8_pipeline(
    spec: &RemuxJobSpec,
    part: &Path,
    plan: &TranscodePlan,
    deadline: Instant,
    cancelled: &AtomicBool,
    observer: &mut dyn FnMut(RemuxP8StageEvent) -> Result<(), String>,
) -> Result<(), RemuxP8Error> {
    #[cfg(test)]
    let test_runner = crate::lock_recover(p8_test_runners()).remove(part);
    #[cfg(test)]
    if let Some(runner) = test_runner {
        return runner(part, deadline, cancelled, observer);
    }
    let toolchain = spec.profile8_toolchain.as_ref().ok_or_else(|| {
        RemuxP8Error::Pipeline("Profile-8 job is missing its toolchain snapshot".into())
    })?;
    if let Some(source) = spec.source_file.as_deref() {
        run_remux_p8_with_toolchain_stage_observed(
            toolchain,
            RemuxP8Input::OpenFile {
                file: source,
                identity_path: &spec.src,
            },
            part,
            plan,
            deadline,
            cancelled,
            observer,
        )
    } else {
        run_remux_p8_with_toolchain_stage_observed(
            toolchain,
            RemuxP8Input::Path(&spec.src),
            part,
            plan,
            deadline,
            cancelled,
            observer,
        )
    }
}

#[derive(Debug)]
pub(crate) struct RemuxMetrics {
    pub(crate) performance: performance::PerformanceMetrics,
    completed: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    coalesced_requests: AtomicU64,
    cache_maintenance: AtomicU64,
    cache_maintenance_failures: AtomicU64,
    cache_evicted_files: AtomicU64,
    cache_evicted_bytes: AtomicU64,
    cache_bytes: AtomicU64,
    cache_scans: AtomicU64,
    cache_scan_entries: AtomicU64,
    cache_lock_wait: AtomicDurationMetric,
    cache_registry_wait: AtomicDurationMetric,
    cache_sweep_duration: AtomicDurationMetric,
    cache_maintenance_duration: AtomicDurationMetric,
    web_requests: AtomicU64,
    web_seek_restarts: AtomicU64,
    web_cache_reuses: AtomicU64,
    web_prepared_reuses: AtomicU64,
    web_cancelled: AtomicU64,
    web_failures_busy: AtomicU64,
    web_failures_producer: AtomicU64,
    web_startup_initial_bytes: AtomicDurationMetric,
    web_startup_playlist_ready: AtomicDurationMetric,
    web_startup_mse_playlist_received: AtomicDurationMetric,
    web_startup_mse_init_fetched: AtomicDurationMetric,
    web_startup_mse_init_appended: AtomicDurationMetric,
    web_startup_mse_first_fragment_fetched: AtomicDurationMetric,
    web_startup_mse_first_fragment_appended: AtomicDurationMetric,
    web_startup_canplay: AtomicDurationMetric,
    web_startup_playing: AtomicDurationMetric,
}

#[derive(Debug, Default)]
pub(crate) struct AtomicDurationMetric {
    count: AtomicU64,
    sum_ms: AtomicU64,
    max_ms: AtomicU64,
    buckets: [AtomicU64; 16],
}

impl AtomicDurationMetric {
    pub(crate) fn record(&self, elapsed: Duration) {
        let millis = rusty_dlna_helper::duration_millis_saturating(elapsed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(millis, Ordering::Relaxed);
        self.max_ms.fetch_max(millis, Ordering::Relaxed);
        let bucket = DURATION_BUCKET_BOUNDS_MS
            .iter()
            .position(|bound| millis <= *bound)
            .unwrap_or(15);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> DurationMetric {
        DurationMetric {
            count: self.count.load(Ordering::Relaxed),
            sum_ms: self.sum_ms.load(Ordering::Relaxed),
            max_ms: self.max_ms.load(Ordering::Relaxed),
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
        }
    }
}

pub(crate) const DURATION_BUCKET_BOUNDS_MS: [u64; 16] = [
    1,
    2,
    5,
    10,
    25,
    50,
    100,
    250,
    500,
    1000,
    2500,
    5000,
    10000,
    30000,
    120000,
    u64::MAX,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DurationMetric {
    pub count: u64,
    pub sum_ms: u64,
    pub max_ms: u64,
    pub buckets: [u64; 16],
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RemuxStatus {
    pub supervisor_ready: bool,
    pub active: usize,
    pub queued: usize,
    pub completed_total: u64,
    pub failed_total: u64,
    pub cancelled_total: u64,
    pub cache_hits_total: u64,
    pub cache_misses_total: u64,
    pub coalesced_requests_total: u64,
    pub cache_maintenance_total: u64,
    pub cache_maintenance_failures_total: u64,
    pub cache_evicted_files_total: u64,
    pub cache_evicted_bytes_total: u64,
    pub cache_bytes: u64,
    pub oldest_job_secs: u64,
    pub cache_scans: u64,
    pub cache_scan_entries: u64,
    pub cache_lock_wait: DurationMetric,
    pub cache_registry_wait: DurationMetric,
    pub cache_sweep_duration: DurationMetric,
    pub cache_maintenance_duration: DurationMetric,
    pub web_requests_total: u64,
    pub web_seek_restarts_total: u64,
    pub web_cache_reuses_total: u64,
    pub web_prepared_reuses_total: u64,
    pub web_cancelled_total: u64,
    pub web_failures_busy_total: u64,
    pub web_failures_producer_total: u64,
    pub web_startup_initial_bytes: DurationMetric,
    pub web_startup_playlist_ready: DurationMetric,
    pub web_startup_mse_playlist_received: DurationMetric,
    pub web_startup_mse_init_fetched: DurationMetric,
    pub web_startup_mse_init_appended: DurationMetric,
    pub web_startup_mse_first_fragment_fetched: DurationMetric,
    pub web_startup_mse_first_fragment_appended: DurationMetric,
    pub web_startup_canplay: DurationMetric,
    pub web_startup_playing: DurationMetric,
}

impl RemuxMetrics {
    pub(crate) fn new(cache_bytes: u64) -> Self {
        Self {
            performance: performance::PerformanceMetrics::default(),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            coalesced_requests: AtomicU64::new(0),
            cache_maintenance: AtomicU64::new(0),
            cache_maintenance_failures: AtomicU64::new(0),
            cache_evicted_files: AtomicU64::new(0),
            cache_evicted_bytes: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(cache_bytes),
            cache_scans: AtomicU64::new(0),
            cache_scan_entries: AtomicU64::new(0),
            cache_lock_wait: AtomicDurationMetric::default(),
            cache_registry_wait: AtomicDurationMetric::default(),
            cache_sweep_duration: AtomicDurationMetric::default(),
            cache_maintenance_duration: AtomicDurationMetric::default(),
            web_requests: AtomicU64::new(0),
            web_seek_restarts: AtomicU64::new(0),
            web_cache_reuses: AtomicU64::new(0),
            web_prepared_reuses: AtomicU64::new(0),
            web_cancelled: AtomicU64::new(0),
            web_failures_busy: AtomicU64::new(0),
            web_failures_producer: AtomicU64::new(0),
            web_startup_initial_bytes: AtomicDurationMetric::default(),
            web_startup_playlist_ready: AtomicDurationMetric::default(),
            web_startup_mse_playlist_received: AtomicDurationMetric::default(),
            web_startup_mse_init_fetched: AtomicDurationMetric::default(),
            web_startup_mse_init_appended: AtomicDurationMetric::default(),
            web_startup_mse_first_fragment_fetched: AtomicDurationMetric::default(),
            web_startup_mse_first_fragment_appended: AtomicDurationMetric::default(),
            web_startup_canplay: AtomicDurationMetric::default(),
            web_startup_playing: AtomicDurationMetric::default(),
        }
    }

    fn record(&self, state: &RemuxState) {
        match state {
            RemuxState::Complete => self.completed.fetch_add(1, Ordering::Relaxed),
            RemuxState::Failed(_) => self.failed.fetch_add(1, Ordering::Relaxed),
            RemuxState::Cancelled => self.cancelled.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }

    fn subtract_cache_bytes(&self, bytes: u64) {
        let _ = self
            .cache_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(bytes))
            });
    }
}

impl Default for RemuxMetrics {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemuxState {
    Starting,
    Preprocessing,
    Growing,
    Complete,
    Failed(String),
    Cancelled,
}

pub struct RemuxJob {
    detail_id: i64,
    web_request_ids: Mutex<HashSet<u64>>,
    web_sessions: Mutex<HashMap<u64, u64>>,
    web: bool,
    /// Immutable, descriptor-backed request plan for reconnects and fixed
    /// fragment resources owned by an established browser generation.
    web_spec: Option<RemuxJobSpec>,
    cache_hit: bool,
    registry_finalized: AtomicBool,
    /// All producer I/O, permit drops and registry cleanup have finished.
    producer_finished: AtomicBool,
    /// One descriptor survives staging publication and pathname replacement.
    /// Only pin/replacement needs this lock; delivery and indexing use pread.
    output: Mutex<Option<Arc<std::fs::File>>>,
    startup_observations: WebStartupObservations,
    pub dest: PathBuf,
    pub part: PathBuf,
    pub(crate) state: Mutex<RemuxState>,
    pub(crate) changed: tokio::sync::Notify,
    cancelled: AtomicBool,
    clients: AtomicUsize,
    ever_had_client: AtomicBool,
    client_epoch: AtomicU64,
    disconnect_deadline: Mutex<Option<Instant>>,
    cacheable: bool,
    started: Instant,
    hls_index: Mutex<hls::Index>,
    effective_recipe: Mutex<Option<fallback::EffectiveRecipe>>,
}

#[derive(Debug, Default)]
struct WebStartupObservations {
    initial_bytes: AtomicBool,
    playlist_ready: AtomicBool,
    mse_playlist_received: AtomicBool,
    mse_init_fetched: AtomicBool,
    mse_init_appended: AtomicBool,
    mse_first_fragment_fetched: AtomicBool,
    mse_first_fragment_appended: AtomicBool,
    canplay: AtomicBool,
    playing: AtomicBool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebStartupEvent {
    MsePlaylistReceived,
    MseInitFetched,
    MseInitAppended,
    MseFirstFragmentFetched,
    MseFirstFragmentAppended,
    CanPlay,
    Playing,
}

impl WebStartupEvent {
    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        match value {
            "mse_playlist_received" => Some(Self::MsePlaylistReceived),
            "mse_init_fetched" => Some(Self::MseInitFetched),
            "mse_init_appended" => Some(Self::MseInitAppended),
            "mse_first_fragment_fetched" => Some(Self::MseFirstFragmentFetched),
            "mse_first_fragment_appended" => Some(Self::MseFirstFragmentAppended),
            "canplay" => Some(Self::CanPlay),
            "playing" => Some(Self::Playing),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::MsePlaylistReceived => "mse_playlist_received",
            Self::MseInitFetched => "mse_init_fetched",
            Self::MseInitAppended => "mse_init_appended",
            Self::MseFirstFragmentFetched => "mse_first_fragment_fetched",
            Self::MseFirstFragmentAppended => "mse_first_fragment_appended",
            Self::CanPlay => "canplay",
            Self::Playing => "playing",
        }
    }

    fn observation(self, observations: &WebStartupObservations) -> &AtomicBool {
        match self {
            Self::MsePlaylistReceived => &observations.mse_playlist_received,
            Self::MseInitFetched => &observations.mse_init_fetched,
            Self::MseInitAppended => &observations.mse_init_appended,
            Self::MseFirstFragmentFetched => &observations.mse_first_fragment_fetched,
            Self::MseFirstFragmentAppended => &observations.mse_first_fragment_appended,
            Self::CanPlay => &observations.canplay,
            Self::Playing => &observations.playing,
        }
    }

    fn metric(self, metrics: &RemuxMetrics) -> &AtomicDurationMetric {
        match self {
            Self::MsePlaylistReceived => &metrics.web_startup_mse_playlist_received,
            Self::MseInitFetched => &metrics.web_startup_mse_init_fetched,
            Self::MseInitAppended => &metrics.web_startup_mse_init_appended,
            Self::MseFirstFragmentFetched => &metrics.web_startup_mse_first_fragment_fetched,
            Self::MseFirstFragmentAppended => &metrics.web_startup_mse_first_fragment_appended,
            Self::CanPlay => &metrics.web_startup_canplay,
            Self::Playing => &metrics.web_startup_playing,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RecentRemuxState {
    state: &'static str,
    at: Instant,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WebPlaybackSessionState {
    latest_request_id: u64,
    cancelled: bool,
    cancelled_handoff: Option<WebCancelledProducerHandoff>,
    at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WebCancelledProducerHandoff {
    detail_id: i64,
    ai_upscale: bool,
}

pub(crate) struct WebTranscodePreparation {
    detail_id: i64,
    plan: TranscodePlan,
    options: BrowserOutputOptions,
    source_file: Arc<std::fs::File>,
    source_path: PathBuf,
    cache_identity: TranscodeCacheIdentity,
    at: Instant,
}

impl WebTranscodePreparation {
    pub(crate) fn new(
        detail_id: i64,
        plan: TranscodePlan,
        options: BrowserOutputOptions,
        source_file: Arc<std::fs::File>,
        source_path: PathBuf,
        cache_identity: TranscodeCacheIdentity,
    ) -> Self {
        Self {
            detail_id,
            plan,
            options: browser_preparation_options(options),
            source_file,
            source_path,
            cache_identity,
            at: Instant::now(),
        }
    }
}

#[derive(Default)]
struct EphemeralCleanupState {
    worker_running: bool,
    stopping: bool,
    generation: u64,
}

pub(crate) struct EphemeralCleanupScheduler {
    state: Mutex<EphemeralCleanupState>,
    changed: Condvar,
    #[cfg(test)]
    worker_starts: AtomicUsize,
    #[cfg(test)]
    fail_next_spawn: AtomicBool,
}

impl EphemeralCleanupScheduler {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(EphemeralCleanupState::default()),
            changed: Condvar::new(),
            #[cfg(test)]
            worker_starts: AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_spawn: AtomicBool::new(false),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, EphemeralCleanupState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned ephemeral cleanup scheduler");
            self.state.clear_poison();
            poisoned.into_inner()
        })
    }

    fn wait_timeout<'a>(
        &self,
        state: MutexGuard<'a, EphemeralCleanupState>,
        timeout: Duration,
    ) -> MutexGuard<'a, EphemeralCleanupState> {
        self.changed
            .wait_timeout(state, timeout)
            .map(|(state, _)| state)
            .unwrap_or_else(|poisoned| {
                tracing::error!("recovering poisoned ephemeral cleanup scheduler wait");
                self.state.clear_poison();
                poisoned.into_inner().0
            })
    }

    fn wake_or_start(self: &Arc<Self>, app: &Arc<App>, id: i64) {
        let should_spawn = {
            let mut state = self.lock_state();
            if state.stopping {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            self.changed.notify_one();
            if state.worker_running {
                false
            } else {
                state.worker_running = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let app_weak = Arc::downgrade(app);
        let scheduler = self.clone();
        #[cfg(test)]
        let fail_spawn = self.fail_next_spawn.swap(false, Ordering::AcqRel);
        #[cfg(not(test))]
        let fail_spawn = false;
        let spawned = if fail_spawn {
            Err(std::io::Error::other(
                "injected ephemeral cleanup thread failure",
            ))
        } else {
            std::thread::Builder::new()
                .name("remux-retention".into())
                .spawn(move || scheduler.run(app_weak))
                .map(|_| ())
        };
        match spawned {
            Ok(()) => {
                #[cfg(test)]
                self.worker_starts.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                tracing::warn!(id, %error, "could not schedule web segment cleanup");
                {
                    let mut state = self.lock_state();
                    state.worker_running = false;
                }
                // Resource exhaustion must not leave an output and registry entry
                // permanently retained merely because the timer could not start.
                let _ = sweep_ephemeral_cleanups(app, Instant::now(), true);
            }
        }
    }

    fn run(self: Arc<Self>, app: Weak<App>) {
        loop {
            let observed_generation = {
                let mut state = self.lock_state();
                if state.stopping {
                    state.worker_running = false;
                    return;
                }
                state.generation
            };
            let Some(app) = app.upgrade() else {
                let mut state = self.lock_state();
                state.worker_running = false;
                return;
            };
            let now = Instant::now();
            let next = sweep_ephemeral_cleanups(&app, now, false);
            drop(app);

            let mut state = self.lock_state();
            if state.stopping {
                state.worker_running = false;
                return;
            }
            if state.generation != observed_generation {
                continue;
            }
            match next {
                Some(next) => {
                    drop(self.wait_timeout(state, next.saturating_duration_since(Instant::now())))
                }
                None => {
                    state.worker_running = false;
                    return;
                }
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        let mut state = self.lock_state();
        state.stopping = true;
        state.generation = state.generation.wrapping_add(1);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn fail_next_spawn(&self) {
        self.fail_next_spawn.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn worker_starts(&self) -> usize {
        self.worker_starts.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn is_idle(&self) -> bool {
        !self.lock_state().worker_running
    }
}

impl RemuxJob {
    fn open_output(&self) -> std::io::Result<Arc<std::fs::File>> {
        let mut output = crate::lock_recover(&self.output);
        if self.cancelled.load(Ordering::Acquire) || self.err().is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "remux is unavailable",
            ));
        }
        if let Some(file) = output.as_ref() {
            return Ok(file.clone());
        }
        let file = std::fs::File::open(current_path(self))?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("remux output is not a regular file"));
        }
        let file = Arc::new(file);
        *output = Some(file.clone());
        Ok(file)
    }

    /// Readiness and descriptor publication share the fallback/publication lock.
    /// Opening an unready attempt must not pin it: fallback may still replace it.
    fn pin_ready_output(&self) -> Result<Option<PathBuf>, String> {
        let mut output = crate::lock_recover(&self.output);
        let complete = match self.state() {
            RemuxState::Complete => true,
            RemuxState::Growing => false,
            RemuxState::Starting | RemuxState::Preprocessing => return Ok(None),
            RemuxState::Failed(error) => return Err(error),
            RemuxState::Cancelled => return Err(REMUX_CANCELLED.into()),
        };
        if self.cancelled.load(Ordering::Acquire) {
            return Err(REMUX_CANCELLED.into());
        }
        let path = if complete {
            self.dest.clone()
        } else {
            self.part.clone()
        };
        let candidate = if let Some(file) = output.as_ref() {
            file.clone()
        } else {
            match std::fs::File::open(&path) {
                Ok(file) => Arc::new(file),
                Err(error) if !complete && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None)
                }
                Err(error) => return Err(format!("open ready remux: {error}")),
            }
        };
        let metadata = candidate
            .metadata()
            .map_err(|error| format!("stat ready remux: {error}"))?;
        if !metadata.is_file() {
            return Err("remux output is not a regular file".into());
        }
        if metadata.len() < if complete { 1 } else { FIRST_BYTES } {
            return if complete {
                Err("completed remux is missing or empty".into())
            } else {
                Ok(None)
            };
        }
        *output = Some(candidate);
        Ok(Some(path))
    }

    fn add_web_request(
        &self,
        session_id: Option<u64>,
        request_id: Option<u64>,
    ) -> Result<Option<u64>, String> {
        let mut sessions = crate::lock_recover(&self.web_sessions);
        let mut request_ids = crate::lock_recover(&self.web_request_ids);
        let replaced = session_id
            .zip(request_id)
            .and_then(|(id, _)| sessions.get(&id).copied());
        // Scoped owners are removed with the bounded global session registry.
        // Legacy callers may omit a session; bound those owners too, while
        // allowing existing owners and same-session replacements to reconnect.
        if request_id.is_some_and(|id| !request_ids.contains(&id))
            && replaced.is_none()
            && request_ids.len() >= MAX_WEB_PLAYBACK_SESSIONS
        {
            return Err("transcode busy (too many playback owners)".into());
        }
        if let Some((session_id, request_id)) = session_id.zip(request_id) {
            sessions.insert(session_id, request_id);
        }
        if let Some(replaced) = replaced.filter(|replaced| Some(*replaced) != request_id) {
            request_ids.remove(&replaced);
            if let Some(session_id) = session_id {
                crate::lock_recover(&self.hls_index).forget_generation(session_id, replaced);
            }
        }
        if let Some(request_id) = request_id {
            request_ids.insert(request_id);
        }
        Ok(replaced)
    }

    fn remove_web_request(&self, session_id: Option<u64>, request_id: u64) -> bool {
        let mut sessions = crate::lock_recover(&self.web_sessions);
        if let Some(session_id) = session_id {
            if sessions.get(&session_id) != Some(&request_id) {
                return false;
            }
            sessions.remove(&session_id);
        } else {
            sessions.retain(|_, attached_request_id| *attached_request_id != request_id);
        }
        drop(sessions);
        if let Some(session_id) = session_id {
            crate::lock_recover(&self.hls_index).forget_generation(session_id, request_id);
        }
        crate::lock_recover(&self.web_request_ids).remove(&request_id)
    }

    fn remove_web_session(&self, session_id: u64) -> Option<u64> {
        let request_id = crate::lock_recover(&self.web_sessions).remove(&session_id)?;
        crate::lock_recover(&self.web_request_ids).remove(&request_id);
        crate::lock_recover(&self.hls_index).forget_generation(session_id, request_id);
        Some(request_id)
    }

    fn has_web_requests(&self) -> bool {
        !crate::lock_recover(&self.web_request_ids).is_empty()
    }

    fn matches_web_request(&self, request_id: Option<u64>) -> bool {
        request_id.is_none_or(|request_id| {
            crate::lock_recover(&self.web_request_ids).contains(&request_id)
        })
    }

    fn owns_web_request(&self, session_id: Option<u64>, request_id: u64) -> bool {
        if let Some(session_id) = session_id {
            return crate::lock_recover(&self.web_sessions).get(&session_id) == Some(&request_id);
        }
        crate::lock_recover(&self.web_request_ids).contains(&request_id)
    }

    fn renew_disconnected_web_lease(&self, lease: Duration) {
        let mut deadline = crate::lock_recover(&self.disconnect_deadline);
        if self.clients.load(Ordering::Acquire) != 0 || deadline.is_none() {
            return;
        }
        if matches!(
            self.state(),
            RemuxState::Starting
                | RemuxState::Preprocessing
                | RemuxState::Growing
                | RemuxState::Complete
        ) {
            let now = Instant::now();
            *deadline = Some(now.checked_add(lease).unwrap_or(now));
        }
    }

    fn err(&self) -> Option<String> {
        match self.state.lock().ok()?.clone() {
            RemuxState::Failed(error) => Some(error),
            RemuxState::Cancelled => Some(REMUX_CANCELLED.into()),
            _ => None,
        }
    }

    fn state(&self) -> RemuxState {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| RemuxState::Failed("remux state poisoned".into()))
    }

    fn transition(&self, next: RemuxState) {
        if let Ok(mut state) = self.state.lock() {
            *state = next;
        }
        self.changed.notify_waiters();
    }

    fn notify_growth(&self) {
        self.changed.notify_waiters();
    }

    fn is_complete(&self) -> bool {
        matches!(self.state(), RemuxState::Complete)
    }

    fn cancel(&self) {
        let _deadline = crate::lock_recover(&self.disconnect_deadline);
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    fn attach_client(&self) {
        let mut deadline = crate::lock_recover(&self.disconnect_deadline);
        self.clients.fetch_add(1, Ordering::Relaxed);
        self.ever_had_client.store(true, Ordering::Release);
        self.client_epoch.fetch_add(1, Ordering::AcqRel);
        *deadline = None;
    }

    fn detach_client(
        self: &Arc<Self>,
        app: Arc<App>,
        job_key: String,
        continue_after_disconnect: bool,
        reconnect_grace: Duration,
        ephemeral_retention: Duration,
    ) {
        if self.cacheable {
            // Attaches and completion already take map -> deadline. Use the
            // same order so a last-client removal cannot race either one.
            let mut jobs = crate::lock_recover(&app.remuxes);
            let mut deadline = crate::lock_recover(&self.disconnect_deadline);
            let previous = self
                .clients
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |clients| {
                    clients.checked_sub(1)
                })
                .unwrap_or(0);
            self.client_epoch.fetch_add(1, Ordering::AcqRel);
            let mut retain_web_job = false;
            if previous <= 1 {
                *deadline = None;
                if self.registry_finalized.load(Ordering::Acquire) {
                    if self.web {
                        retain_web_job = true;
                    } else {
                        remove_job_locked(&mut jobs, &job_key, self);
                    }
                } else if !continue_after_disconnect {
                    match self.state() {
                        RemuxState::Starting | RemuxState::Preprocessing | RemuxState::Growing => {
                            let now = Instant::now();
                            *deadline = Some(now.checked_add(reconnect_grace).unwrap_or(now));
                        }
                        RemuxState::Complete | RemuxState::Failed(_) | RemuxState::Cancelled => {}
                    }
                }
            }
            drop(deadline);
            drop(jobs);
            if retain_web_job {
                // Fragmented delivery has intentional reader-free gaps. Keep
                // the completed job and its validated plan registered so
                // playlist/segment requests do not re-fingerprint the source,
                // and so cache maintenance cannot evict active playback.
                schedule_ephemeral_cleanup(&app, self, ephemeral_retention);
            }
            return;
        }

        let mut deadline = crate::lock_recover(&self.disconnect_deadline);
        let previous = self
            .clients
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |clients| {
                clients.checked_sub(1)
            })
            .unwrap_or(0);
        self.client_epoch.fetch_add(1, Ordering::AcqRel);
        if previous <= 1 && !continue_after_disconnect {
            match self.state() {
                RemuxState::Starting | RemuxState::Preprocessing | RemuxState::Growing => {
                    let now = Instant::now();
                    *deadline = Some(now.checked_add(reconnect_grace).unwrap_or(now));
                }
                RemuxState::Complete if !self.cacheable => {
                    *deadline = None;
                    drop(deadline);
                    schedule_ephemeral_cleanup(&app, self, ephemeral_retention);
                }
                RemuxState::Complete | RemuxState::Failed(_) | RemuxState::Cancelled => {
                    *deadline = None;
                }
            }
        }
    }

    fn reconnect_grace_expired(&self) -> bool {
        let mut deadline = crate::lock_recover(&self.disconnect_deadline);
        if self.clients.load(Ordering::Acquire) != 0 {
            *deadline = None;
            return false;
        }
        let expired = deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if expired {
            *deadline = None;
            self.cancelled.store(true, Ordering::Release);
            self.changed.notify_waiters();
        }
        expired
    }
}

fn remove_ephemeral_output(app: &App, job: &RemuxJob) {
    let Ok(_reservation) = app.transcode_cache.reserve(&job.dest) else {
        return;
    };
    let _maintenance = crate::lock_recover(&app.cache_maintenance);
    // A replacement may have registered while expired cleanup waited. Its
    // destination belongs to it; never delete a successor's bytes.
    if crate::lock_recover(&app.remuxes)
        .values()
        .any(|current| current.dest == job.dest && !std::ptr::eq(current.as_ref(), job))
    {
        return;
    }
    let _ = std::fs::remove_file(&job.dest);
    let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&job.dest));
    cache::refresh_artifacts(app, [job.dest.clone()]);
}

fn schedule_ephemeral_cleanup(app: &Arc<App>, job: &RemuxJob, retention: Duration) {
    let now = Instant::now();
    let cleanup_at = now.checked_add(retention).unwrap_or(now);
    let id = job.detail_id;
    {
        let mut deadline = crate::lock_recover(&job.disconnect_deadline);
        if job.clients.load(Ordering::Acquire) != 0 {
            *deadline = None;
            return;
        }
        *deadline = Some(cleanup_at);
    }
    app.ephemeral_cleanup.wake_or_start(app, id);
}

fn sweep_ephemeral_cleanups(app: &App, now: Instant, force: bool) -> Option<Instant> {
    let mut next = None;
    let mut expired = Vec::new();
    let mut jobs = crate::lock_recover(&app.remuxes);
    jobs.retain(|_, job| {
        if !job.web || !matches!(job.state(), RemuxState::Complete) {
            return true;
        }
        let mut deadline = crate::lock_recover(&job.disconnect_deadline);
        let Some(cleanup_at) = *deadline else {
            return true;
        };
        if job.clients.load(Ordering::Acquire) != 0 {
            *deadline = None;
            return true;
        }
        if !force && now < cleanup_at {
            next = Some(next.map_or(cleanup_at, |current: Instant| current.min(cleanup_at)));
            return true;
        }
        *deadline = None;
        if !job.cacheable {
            expired.push(job.clone());
        }
        tracing::debug!(
            id = job.detail_id,
            cacheable = job.cacheable,
            "expired reconnectable web job"
        );
        false
    });
    drop(jobs);
    for job in expired {
        remove_ephemeral_output(app, &job);
    }
    next
}

pub(crate) fn shutdown_ephemeral_cleanups(app: &App) {
    app.ephemeral_cleanup.shutdown();
    let _ = sweep_ephemeral_cleanups(app, Instant::now(), true);
}

struct RemuxCompletionGuard {
    app: Arc<App>,
    job_key: String,
    job: Arc<RemuxJob>,
}

impl RemuxCompletionGuard {
    fn new(app: Arc<App>, job_key: String, job: Arc<RemuxJob>) -> Self {
        Self { app, job_key, job }
    }
}

impl Drop for RemuxCompletionGuard {
    fn drop(&mut self) {
        if std::thread::panicking()
            && matches!(
                self.job.state(),
                RemuxState::Starting | RemuxState::Preprocessing | RemuxState::Growing
            )
        {
            tracing::error!(
                id = self.job.detail_id,
                dest = %self.job.dest.display(),
                "remux worker panicked"
            );
            self.job
                .transition(RemuxState::Failed("remux worker panicked".into()));
            cleanup_intermediates(&self.job.part);
        }
        if matches!(
            self.job.state(),
            RemuxState::Failed(_) | RemuxState::Cancelled
        ) {
            // Failure paths may have removed staging bytes after their last
            // observer scan. Refresh asynchronously so cleanup cannot wait
            // behind an unrelated image request's maintenance gate.
            self.app
                .transcode_cache
                .monitor
                .reconcile(&self.app, &self.job);
        }
        finish_job(&self.app, &self.job_key, &self.job);
        self.job.producer_finished.store(true, Ordering::Release);
        self.job.changed.notify_waiters();
    }
}

fn spawn_ffmpeg(
    app: Arc<App>,
    spec: RemuxJobSpec,
    job: Arc<RemuxJob>,
    helper_permit: rusty_dlna_helper::HelperPermit,
    job_permit: rusty_dlna_helper::JobPermit,
    ai_upscale_permit: Option<rusty_dlna_helper::JobPermit>,
) {
    let app_err = app.clone();
    let job_err = job.clone();
    let id_err = spec.detail_id;
    let key_err = spec.job_key.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("remux-{}", spec.detail_id))
        .spawn(move || {
            // Release execution permits before registry cleanup. A newer web
            // generation can then wait for the producer it cancelled while
            // holding the registry lock, without deadlocking this guard.
            let _completion_guard =
                RemuxCompletionGuard::new(app.clone(), spec.job_key.clone(), job.clone());
            // Reverse drop order releases the global helper first, followed
            // by the ordinary and AI job slots, before the completion guard.
            let _ai_upscale_permit = ai_upscale_permit;
            let _job_permit = job_permit;
            let _helper_permit = helper_permit;
            let dest = spec.dest.clone();
            let part = job.part.clone();
            let id = spec.detail_id;
            let deadline = job.started + Duration::from_secs(app.cfg.transcode.max_runtime_secs);
            let verify_timeout = Duration::from_secs(app.cfg.transcode.verify_timeout_secs);
            let mut output_fell_back = false;
            let mut previous_failure = None;
            let mut attempt_started = Instant::now();
            if spec.remux_p8 {
                let p8 = TranscodePlan {
                    action: RecodeAction::RemuxP8,
                    video_encoder: "copy".into(),
                    audio: match spec.audio {
                        RemuxAudio::Copy => rusty_dlna_transcode::AudioAction::Copy,
                        RemuxAudio::Ac3 => rusty_dlna_transcode::AudioAction::ToAc3,
                        RemuxAudio::Aac => rusty_dlna_transcode::AudioAction::ToAac,
                    },
                    audio_index: spec.audio_index,
                    container: "mp4",
                    ..TranscodePlan::default()
                };
                tracing::info!(id, dest = %dest.display(), "remux-p8 dovi_tool start");
                job.transition(RemuxState::Preprocessing);
                let mut sequence = app.remux_metrics.performance.profile8.begin();
                let mut cache_monitor = cache_monitor::Monitor::new();
                let mut observe_progress = |event: RemuxP8StageEvent| {
                    app.remux_metrics
                        .performance
                        .profile8
                        .record(&mut sequence, event);
                    if job.reconnect_grace_expired() {
                        job.cancel();
                    }
                    // The packet rewrite has verified source sample association
                    // and retained its timeline before this immutable final mux.
                    let final_mux = event.stage == RemuxP8Stage::FinalMux
                        && matches!(
                            event.status,
                            RemuxP8StageStatus::Started
                                | RemuxP8StageStatus::Progress
                                | RemuxP8StageStatus::Succeeded
                        );
                    let kind = if final_mux {
                        cache_monitor::Kind::Profile8
                    } else {
                        cache_monitor::Kind::CacheOnly
                    };
                    profile8::observe_final_mux(&app, &job, &mut cache_monitor, kind)?;
                    Ok(())
                };
                let p8_result = run_profile8_pipeline(
                    &spec,
                    &part,
                    &p8,
                    deadline,
                    &job.cancelled,
                    &mut observe_progress,
                );
                match p8_result {
                    Ok(()) => {
                        finalize_remux(
                            &app,
                            &job,
                            &spec,
                            verify_timeout,
                            &spec.output_expectation,
                            spec.cacheable,
                        );
                        return;
                    }
                    Err(RemuxP8Error::Observer(error)) => {
                        cleanup_intermediates(&part);
                        // Refresh after cleanup so the gauge reflects disk,
                        // whether pressure or final-mux inspection stopped it.
                        app.transcode_cache.monitor.reconcile(&app, &job);
                        job.transition(RemuxState::Failed(error));
                        return;
                    }
                    Err(RemuxP8Error::Cancelled(_)) => {
                        job.transition(RemuxState::Cancelled);
                        cleanup_intermediates(&part);
                        return;
                    }
                    Err(RemuxP8Error::Deadline(_)) => {
                        job.transition(RemuxState::Failed(
                            "transcode runtime exceeded configured deadline".into(),
                        ));
                        cleanup_intermediates(&part);
                        return;
                    }
                    Err(e) => {
                        let output = crate::lock_recover(&job.output);
                        if output.is_some() {
                            job.transition(RemuxState::Failed(
                                "Profile-8 output failed after its generation was pinned".into(),
                            ));
                            cleanup_intermediates(&part);
                            return;
                        }
                        tracing::warn!(id, dest = %dest.display(), "{e}; falling back to hdr10");
                        let _ = std::fs::remove_file(&part);
                        job.transition(RemuxState::Starting);
                        output_fell_back = true;
                        previous_failure = Some(fallback::classify(&e.to_string()));
                        let mut actual =
                            fallback::recipe(&spec, &spec.args, fallback::Attempt::Hdr10);
                        actual.previous_failure = previous_failure;
                        actual.previous_attempt_ms =
                            Some(rusty_dlna_helper::duration_millis_saturating(
                                attempt_started.elapsed(),
                            ));
                        *crate::lock_recover(&job.effective_recipe) = Some(actual);
                        attempt_started = Instant::now();
                        drop(output);
                    }
                }
            }
            let mut args = &spec.args;
            if !output_fell_back {
                *crate::lock_recover(&job.effective_recipe) =
                    Some(fallback::recipe(&spec, args, fallback::Attempt::Primary));
            }
            tracing::info!(id, dest = %dest.display(), "remux job start");
            let mut result = run_ffmpeg_growing(
                args,
                spec.source_file.as_deref(),
                spec.ai_upscale_shader_file.as_deref(),
                spec.verified_ffmpeg.as_ref(),
                &job,
                deadline,
                &app,
            );
            for (fallback_args, attempt) in [
                (
                    spec.hardware_fallback_args.as_ref(),
                    fallback::Attempt::AlternateHardware,
                ),
                (spec.fallback_args.as_ref(), fallback::Attempt::Portable),
            ] {
                let failed = matches!(&result, Ok((status, _)) if !status.success());
                if !failed || job.cancelled.load(Ordering::Acquire) {
                    break;
                }
                if let Some(fallback_args) = fallback_args {
                    // A pinned descriptor is an irrevocable generation boundary,
                    // including headers/index views before any media bytes. Hold
                    // the same lock as open_output while removing failed bytes.
                    let output = crate::lock_recover(&job.output);
                    if output.is_some() {
                        break;
                    }
                    let failure = result
                        .as_ref()
                        .ok()
                        .map(|(_, stderr)| fallback::classify(stderr))
                        .unwrap_or(fallback::FailureClass::Unknown);
                    // Every failed earlier path must be stably unsupported before
                    // a later successful attempt may bypass the primary next time.
                    let failure = match previous_failure {
                        Some(prior) if prior != fallback::FailureClass::Unsupported => prior,
                        _ => failure,
                    };
                    previous_failure = Some(failure);
                    let mut actual = fallback::recipe(&spec, fallback_args, attempt);
                    actual.previous_failure = Some(failure);
                    actual.previous_attempt_ms = Some(
                        rusty_dlna_helper::duration_millis_saturating(attempt_started.elapsed()),
                    );
                    *crate::lock_recover(&job.effective_recipe) = Some(actual);
                    let kind = attempt.label();
                    if attempt == fallback::Attempt::AlternateHardware {
                        app.remux_metrics
                            .performance
                            .fallbacks_hardware
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        app.remux_metrics
                            .performance
                            .fallbacks_portable
                            .fetch_add(1, Ordering::Relaxed);
                    }

                    tracing::warn!(
                        id,
                        dest = %dest.display(),
                        kind,
                        "negotiated compatible output failed; retrying fallback"
                    );
                    cleanup_intermediates(&part);
                    job.transition(RemuxState::Starting);
                    args = fallback_args;
                    output_fell_back = true;
                    drop(output);
                    attempt_started = Instant::now();
                    result = run_ffmpeg_growing(
                        args,
                        spec.source_file.as_deref(),
                        spec.ai_upscale_shader_file.as_deref(),
                        spec.verified_ffmpeg.as_ref(),
                        &job,
                        deadline,
                        &app,
                    );
                }
            }
            match result {
                Ok((status, _)) if status.success() => {
                    let expectation = fallback::expectation(&spec, args);
                    if let Some(actual) = crate::lock_recover(&job.effective_recipe).as_mut() {
                        actual.attempt_ms = Some(rusty_dlna_helper::duration_millis_saturating(
                            attempt_started.elapsed(),
                        ));
                    }
                    let mut published_spec = spec.clone();
                    if output_fell_back {
                        if let Some(actual) = crate::lock_recover(&job.effective_recipe).as_ref() {
                            published_spec.cache_key = actual.stamp_key();
                        }
                    }
                    finalize_remux(
                        &app,
                        &job,
                        &published_spec,
                        verify_timeout,
                        &expectation,
                        spec.cacheable && (!output_fell_back || expectation.is_some()),
                    );
                }
                Ok((status, stderr)) => {
                    let tail = tail_str(&stderr, 2000);
                    tracing::error!(
                        id,
                        %status,
                        dest = %dest.display(),
                        stderr = %tail,
                        "ffmpeg remux failed"
                    );
                    job.transition(RemuxState::Failed(format!("ffmpeg {status}: {tail}")));
                    let _ = std::fs::remove_file(&part);
                }
                Err(error) => {
                    let cache_pressure = error.starts_with("transcode cache limits: ");
                    if job.cancelled.load(Ordering::Acquire) || error == "cancelled" {
                        tracing::info!(
                            id,
                            dest = %dest.display(),
                            "remux job cancelled"
                        );
                        job.transition(RemuxState::Cancelled);
                    } else {
                        tracing::error!(id, dest = %dest.display(), %error, "ffmpeg spawn failed");
                        job.transition(RemuxState::Failed(error));
                    }
                    cleanup_intermediates(&part);
                    if cache_pressure {
                        // The failed pass included the now-removed staging
                        // file, so refresh accounting after cleanup.
                        app.transcode_cache.monitor.reconcile(&app, &job);
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!(id = id_err, %e, "remux thread spawn failed");
        job_err.transition(RemuxState::Failed(format!("thread: {e}")));
        finish_job(&app_err, &key_err, &job_err);
        job_err.producer_finished.store(true, Ordering::Release);
        job_err.changed.notify_waiters();
    }
}

fn run_ffmpeg_growing(
    args: &[std::ffi::OsString],
    source_file: Option<&std::fs::File>,
    ai_upscale_shader_file: Option<&std::fs::File>,
    verified_ffmpeg: Option<&rusty_dlna_transcode::VerifiedExecutable>,
    job: &Arc<RemuxJob>,
    deadline: Instant,
    app: &Arc<App>,
) -> Result<(std::process::ExitStatus, String), String> {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome, SupervisionError,
    };
    use std::ops::ControlFlow;

    let key = job
        .web_spec
        .as_ref()
        .and_then(|spec| spec.web_session_id.zip(spec.web_request_id))
        .map(|(session, request)| (job.detail_id, session, request));
    let _attempt_timing = app
        .remux_metrics
        .performance
        .timer(key, performance::Stage::HelperAttempt);
    let Some(executable) = args.first() else {
        return Err("empty transcode command".into());
    };
    let is_ffmpeg = Path::new(executable)
        .file_name()
        .is_some_and(|name| name == "ffmpeg");
    if is_ffmpeg && verified_ffmpeg.is_none() {
        return Err("production ffmpeg command is missing its verified executable".into());
    }
    if let Some(verified_ffmpeg) = verified_ffmpeg {
        verified_ffmpeg
            .verify_for_execution()
            .map_err(|error| format!("spawn {}: {error}", verified_ffmpeg.path().display()))?;
    }
    let mut command = verified_ffmpeg.map_or_else(
        || std::process::Command::new(executable),
        rusty_dlna_transcode::VerifiedExecutable::command,
    );
    let mut confined_args = args.to_vec();
    let inherited_source = source_file
        .map(rusty_dlna_transcode::reopen_media_input)
        .transpose()
        .map_err(|error| error.to_string())?;
    if inherited_source.is_some() {
        rusty_dlna_transcode::use_inherited_media_input(&mut confined_args, 0, 3)?;
    }
    command.args(&confined_args[1..]);
    let mut runner = SupervisedCommand::new(&mut command)
        .capture_stderr(CaptureConfig::new(64 * 1024, CaptureRetention::Tail));
    if let Some(source) = inherited_source.as_ref() {
        runner = runner
            .inherit_file_at(source, 3)
            .map_err(|error| format!("spawn {}: {error}", executable.to_string_lossy()))?;
    }
    if let Some(shader) = ai_upscale_shader_file {
        runner = runner
            .inherit_file_at(shader, rusty_dlna_transcode::BROWSER_AI_UPSCALE_SHADER_FD)
            .map_err(|error| format!("spawn {}: {error}", executable.to_string_lossy()))?;
    }
    if let Some(verified_ffmpeg) = verified_ffmpeg {
        runner = verified_ffmpeg
            .inherit_for_execution(runner)
            .map_err(|error| format!("spawn {}: {error}", verified_ffmpeg.path().display()))?;
    }

    enum Stop {
        Cancelled,
        Deadline,
        Cache(String),
    }
    let mut last_len = 0;
    let mut cache_monitor = cache_monitor::Monitor::new();
    let stop_reason = || {
        if job.cancelled.load(Ordering::Acquire) {
            return Some(Stop::Cancelled);
        }
        if job.reconnect_grace_expired() {
            job.cancel();
            return Some(Stop::Cancelled);
        }
        (Instant::now() >= deadline).then_some(Stop::Deadline)
    };
    let outcome = runner.run_until(deadline, POLL, || {
        if let Some(reason) = stop_reason() {
            return ControlFlow::Break(reason);
        }
        // Wake readers at the established 50 ms cadence without doing file IO
        // in the child observer or waiting for the periodic quota result.
        if matches!(job.state(), RemuxState::Growing) {
            job.notify_growth();
        }
        let mut observation = cache_monitor.poll(app, job, cache_monitor::Kind::Ordinary);
        if matches!(job.state(), RemuxState::Starting)
            && observation
                .as_ref()
                .is_ok_and(|value| value.as_ref().is_none_or(|value| !value.playable))
        {
            // Give a cheap worker result a bounded opportunity to arrive in
            // this observer tick. Request at most 5 ms of waiting; filesystem
            // and cache-gate work remain outside the child observer.
            cache_monitor.wait_pending(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            );
            if let Some(reason) = stop_reason() {
                return ControlFlow::Break(reason);
            }
            observation = cache_monitor.poll(app, job, cache_monitor::Kind::Ordinary);
        }
        match observation {
            Err(error) => return ControlFlow::Break(Stop::Cache(error)),
            Ok(Some(observation)) if !job.cancelled.load(Ordering::Acquire) => {
                if observation.playable && matches!(job.state(), RemuxState::Starting) {
                    // The worker sampled these bytes before its successful
                    // pressure pass. No pending/older result grants exposure.
                    job.transition(RemuxState::Growing);
                } else if observation.length != last_len {
                    job.notify_growth();
                }
                last_len = observation.length;
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    match outcome {
        Ok(SupervisedOutcome::Exited(output)) => {
            if output.status.success() && matches!(job.state(), RemuxState::Starting) {
                // A fast child can exit before the observer consumes its ready
                // result. Reap first, then inspect its final bytes once. Keeping
                // this observation owned restores early playback without
                // waiting for final verification or blocking child supervision.
                cache_monitor = cache_monitor::Monitor::new();
                cache_monitor
                    .poll(app, job, cache_monitor::Kind::Ordinary)
                    .map_err(|error| format!("transcode cache limits: {error}"))?;
                loop {
                    if job.reconnect_grace_expired() {
                        job.cancel();
                    }
                    if job.cancelled.load(Ordering::Acquire) {
                        return Err("cancelled".into());
                    }
                    if Instant::now() >= deadline {
                        return Err("transcode runtime exceeded configured deadline".into());
                    }
                    if let Some(observation) = cache_monitor
                        .take_ready()
                        .transpose()
                        .map_err(|error| format!("transcode cache limits: {error}"))?
                    {
                        if job.cancelled.load(Ordering::Acquire) {
                            return Err("cancelled".into());
                        }
                        if Instant::now() >= deadline {
                            return Err("transcode runtime exceeded configured deadline".into());
                        }
                        if observation.playable {
                            job.transition(RemuxState::Growing);
                        }
                        break;
                    }
                    cache_monitor.wait_pending(POLL);
                }
            }
            Ok((
                output.status,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Cancelled,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Cancelled,
                ..
            },
        ) => Err("cancelled".into()),
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Deadline,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Deadline,
                ..
            },
        ) => Err("transcode runtime exceeded configured deadline".into()),
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Cache(error),
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Cache(error),
                ..
            },
        ) => Err(format!("transcode cache limits: {error}")),
        Ok(SupervisedOutcome::Deadline { .. }) => {
            Err("transcode runtime exceeded configured deadline".into())
        }
        Err(SupervisionError::Spawn(error)) => {
            Err(format!("spawn {}: {error}", executable.to_string_lossy()))
        }
        Err(SupervisionError::Wait(error)) => {
            Err(format!("wait {}: {error}", executable.to_string_lossy()))
        }
        Err(error) => Err(error.to_string()),
    }
}

fn remove_job_locked(map: &mut HashMap<String, Arc<RemuxJob>>, key: &str, job: &Arc<RemuxJob>) {
    if map
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, job))
    {
        map.remove(key);
    }
}

fn remove_job(app: &App, key: &str, job: &Arc<RemuxJob>) {
    remove_job_locked(&mut crate::lock_recover(&app.remuxes), key, job);
}

fn record_recent_web_state(app: &App, detail_id: i64, request_id: u64, state: &'static str) {
    let mut recent = crate::lock_recover(&app.recent_remux_states);
    recent.retain(|_, value| value.at.elapsed() < Duration::from_secs(60));
    if recent.len() >= 128 && !recent.contains_key(&(detail_id, request_id)) {
        if let Some(oldest) = recent
            .iter()
            .min_by_key(|(_, value)| value.at)
            .map(|(id, _)| *id)
        {
            recent.remove(&oldest);
        }
    }
    recent.insert(
        (detail_id, request_id),
        RecentRemuxState {
            state,
            at: Instant::now(),
        },
    );
}

fn finish_job(app: &Arc<App>, key: &str, job: &Arc<RemuxJob>) {
    let state = job.state();
    app.remux_metrics.record(&state);
    if job.web {
        match &state {
            RemuxState::Failed(_) => app
                .remux_metrics
                .web_failures_producer
                .fetch_add(1, Ordering::Relaxed),
            RemuxState::Cancelled => app
                .remux_metrics
                .web_cancelled
                .fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }
    let public_state = match &state {
        RemuxState::Complete => "ready",
        RemuxState::Failed(_) => "failed",
        RemuxState::Cancelled => "cancelled",
        RemuxState::Starting | RemuxState::Preprocessing => "starting",
        RemuxState::Growing => "producing",
    };
    for request_id in crate::lock_recover(&job.web_request_ids).iter().copied() {
        record_recent_web_state(app, job.detail_id, request_id, public_state);
    }
    if matches!(state, RemuxState::Complete) && job.cacheable {
        // Keep a finished output registered while a response is serving it so
        // cache maintenance sees it in the protected-artifact snapshot. This
        // is atomic with attach/detach via the common map -> deadline order.
        let retain_web_job = {
            let mut jobs = crate::lock_recover(&app.remuxes);
            let _deadline = crate::lock_recover(&job.disconnect_deadline);
            job.registry_finalized.store(true, Ordering::Release);
            if job.clients.load(Ordering::Acquire) == 0 {
                if job.web && job.ever_had_client.load(Ordering::Acquire) {
                    true
                } else {
                    remove_job_locked(&mut jobs, key, job);
                    false
                }
            } else {
                false
            }
        };
        if retain_web_job {
            schedule_ephemeral_cleanup(app, job, WEB_EPHEMERAL_RETENTION);
        }
    } else if matches!(state, RemuxState::Complete) {
        if job.web && job.ever_had_client.load(Ordering::Acquire) {
            schedule_ephemeral_cleanup(app, job, WEB_EPHEMERAL_RETENTION);
        } else {
            remove_ephemeral_output(app, job);
            remove_job(app, key, job);
        }
    } else {
        remove_job(app, key, job);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputSnapshot {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl OutputSnapshot {
    fn read(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    fn unchanged(&self, file: &std::fs::File, path: &Path) -> Result<(), String> {
        let descriptor = file.metadata().map_err(|error| error.to_string())?;
        let pathname = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if !pathname.is_file() || *self != Self::read(&descriptor) || *self != Self::read(&pathname)
        {
            return Err("output changed during final verification".into());
        }
        Ok(())
    }
}

fn finalize_remux(
    app: &App,
    job: &RemuxJob,
    spec: &RemuxJobSpec,
    verify_timeout: Duration,
    expectation: &Option<rusty_dlna_http::RemuxOutputExpectation>,
    cacheable: bool,
) {
    let result = publish_finished_output(app, job, spec, verify_timeout, expectation, cacheable);
    if let Err(error) = result {
        cleanup_intermediates(&job.part);
        if job.cancelled.load(Ordering::Acquire) {
            job.transition(RemuxState::Cancelled);
        } else {
            tracing::error!(id = job.detail_id, %error, "remux output verification/publication failed");
            job.transition(RemuxState::Failed(
                if error.starts_with("transcode cache limits:") {
                    error
                } else {
                    format!("remux output verification failed: {error}")
                },
            ));
        }
    }
}

#[cfg(test)]
type PublicationTestHook = fn(&RemuxJob);

#[cfg(test)]
fn publication_test_hooks() -> &'static Mutex<HashMap<PathBuf, PublicationTestHook>> {
    static HOOKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, PublicationTestHook>>> =
        std::sync::OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn publish_finished_output(
    app: &App,
    job: &RemuxJob,
    spec: &RemuxJobSpec,
    verify_timeout: Duration,
    expectation: &Option<rusty_dlna_http::RemuxOutputExpectation>,
    cacheable: bool,
) -> Result<(), String> {
    let original_deadline = job.started + Duration::from_secs(app.cfg.transcode.max_runtime_secs);
    let deadline = original_deadline.min(Instant::now() + verify_timeout);
    let output = job.open_output().map_err(|error| error.to_string())?;
    let snapshot = {
        let file = output.as_ref();
        let snapshot = OutputSnapshot::read(&file.metadata().map_err(|error| error.to_string())?);
        if snapshot.bytes == 0 {
            return Err("ffmpeg produced empty remux".into());
        }
        if let Some(expected) = expectation {
            let started = Instant::now();
            let stats = hls::validate_finished(file, expected, deadline, &job.cancelled)?;
            tracing::debug!(
                id = job.detail_id,
                metadata_bytes = stats.metadata_bytes,
                samples = stats.samples,
                boxes = stats.boxes,
                elapsed_us = started.elapsed().as_micros(),
                "completed MP4 structurally validated"
            );
        } else if !cfg!(test) {
            return Err("missing negotiated output validation contract".into());
        }
        snapshot.unchanged(file, &job.part)?;
        snapshot
    };
    #[cfg(test)]
    if let Some(hook) = crate::lock_recover(publication_test_hooks()).remove(&job.part) {
        hook(job);
    }
    // The completed bytes remain protected staging artifacts during the quota check.
    // Acquire the shared gate before the completion lock: maintenance may
    // briefly inspect the registry, whose cancellation paths take completion.
    // Publication never reacquires the registry while holding either lock.
    let _maintenance = cache::lock_maintenance(app, || {
        job.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline
    })
    .map_err(|error| format!("transcode cache limits: {error}"))?;
    cache::maintain_locked_measured(app, false)
        .map_err(|error| format!("transcode cache limits: {error}"))?;
    // Cancellation and publication retain one ordering through Complete.
    let _completion = crate::lock_recover(&job.disconnect_deadline);
    cache::check_publication_limits(app, job)
        .map_err(|error| format!("transcode cache limits: {error}"))?;
    let _publication = crate::lock_recover(&job.output);
    let file = output.as_ref();
    if job.cancelled.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    if expectation.is_some() && Instant::now() >= deadline {
        return Err("final verification deadline exceeded".into());
    }
    snapshot.unchanged(file, &job.part)?;
    std::fs::rename(&job.part, &job.dest).map_err(|error| format!("remux rename: {error}"))?;
    let publish_result = (|| {
        // Rename can change ctime. Recheck content identity and take the new stable
        // snapshot before writing a versioned reusable stamp or exposing Complete.
        let published = OutputSnapshot::read(&file.metadata().map_err(|error| error.to_string())?);
        if published.device != snapshot.device
            || published.inode != snapshot.inode
            || published.bytes != snapshot.bytes
            || published.modified != snapshot.modified
        {
            return Err("output changed during publication".into());
        }
        if cacheable {
            if let Err(error) = write_cache_stamp_for_key(&job.dest, &spec.cache_key) {
                return Err(error.to_string());
            }
        }
        published.unchanged(file, &job.dest).and_then(|()| {
            if job.cancelled.load(Ordering::Acquire) {
                Err("cancelled".into())
            } else if expectation.is_some() && Instant::now() >= deadline {
                Err("final verification deadline exceeded".into())
            } else {
                Ok(())
            }
        })?;
        Ok(())
    })();
    if publish_result.is_err() {
        let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&job.dest));
        let _ = std::fs::remove_file(&job.dest);
    }
    cache::refresh_artifacts(app, [job.part.clone(), job.dest.clone()]);
    publish_result?;
    tracing::info!(id = job.detail_id, bytes = snapshot.bytes, "remux job done");
    job.transition(RemuxState::Complete);
    Ok(())
}

fn cleanup_intermediates(part: &Path) {
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(part.with_extension("hevc"));
    let _ = std::fs::remove_file(part.with_extension("p8.hevc"));
    let _ = std::fs::remove_file(part.with_extension("p8.mp4"));
}

fn tail_str(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.len() <= max {
        return t.to_string();
    }
    let mut start = t.len() - max;
    while !t.is_char_boundary(start) {
        start += 1;
    }
    t[start..].to_string()
}

/// Start or attach. `started` is true when this call launched ffmpeg.
#[cfg(test)]
pub fn attach(app: Arc<App>, spec: RemuxJobSpec) -> Result<Arc<RemuxJob>, String> {
    attach_job(app, spec, false)
}

fn attach_for_client(app: Arc<App>, spec: RemuxJobSpec) -> Result<Arc<RemuxJob>, String> {
    attach_job(app, spec, true)
}

fn attach_job(
    app: Arc<App>,
    spec: RemuxJobSpec,
    register_client: bool,
) -> Result<Arc<RemuxJob>, String> {
    let deadline = Instant::now() + WEB_SUPERSEDED_JOB_HANDOFF;
    loop {
        match attach_job_attempt(app.clone(), &spec, register_client)? {
            RemuxAttachment::Ready(job) => return Ok(job),
            RemuxAttachment::Retiring(job) => {
                // Never hold the registry lock while the old worker observes
                // cache pressure, reaps its helper, or cleans shared paths.
                while !job.producer_finished.load(Ordering::Acquire) {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err("transcode busy (previous producer is stopping)".into());
                    }
                    std::thread::sleep(POLL.min(remaining));
                }
                // Recheck generation tombstones and the registry after waiting.
                if Instant::now() >= deadline {
                    return Err("transcode busy (previous producer is stopping)".into());
                }
            }
        }
    }
}

enum RemuxAttachment {
    Ready(Arc<RemuxJob>),
    Retiring(Arc<RemuxJob>),
}

// The caller holds the global session lock. Preserve the session -> job-map
// lock order so expiration/eviction cannot race an attachment or cancellation.
fn prune_web_playback_sessions(
    app: &App,
    sessions: &mut HashMap<u64, WebPlaybackSessionState>,
    incoming: Option<u64>,
) {
    let mut removed = Vec::new();
    sessions.retain(|id, state| {
        let keep = state.at.elapsed() < WEB_SESSION_RETENTION;
        if !keep {
            removed.push(*id);
        }
        keep
    });
    if sessions.len() >= MAX_WEB_PLAYBACK_SESSIONS
        && incoming.is_some_and(|id| !sessions.contains_key(&id))
    {
        if let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, state)| state.at)
            .map(|(id, _)| *id)
        {
            sessions.remove(&oldest);
            removed.push(oldest);
        }
    }
    if !removed.is_empty() {
        let jobs = crate::lock_recover(&app.remuxes);
        for job in jobs.values().filter(|job| job.web) {
            for session_id in &removed {
                job.remove_web_session(*session_id);
            }
        }
    }
}

fn attach_job_attempt(
    app: Arc<App>,
    spec: &RemuxJobSpec,
    register_client: bool,
) -> Result<RemuxAttachment, String> {
    let key = spec
        .web_session_id
        .zip(spec.web_request_id)
        .map(|(session, request)| (spec.detail_id, session, request));
    let _admission_timing = app
        .remux_metrics
        .performance
        .timer(key, performance::Stage::Admission);
    // Reserve before the registry: maintenance can discover outside the map and
    // only unlink destinations for which it owns an exclusive reservation.
    let _reservation = app.transcode_cache.reserve(&spec.dest)?;
    let web = spec.job_key.starts_with("web:");
    let mut newer_generation = false;
    let mut superseded_producer = false;
    let mut superseded_ai_producer = false;
    let playback_sessions = if web {
        if let (Some(session_id), Some(request_id)) = (spec.web_session_id, spec.web_request_id) {
            let mut sessions = crate::lock_recover(&app.web_playback_sessions);
            prune_web_playback_sessions(&app, &mut sessions, Some(session_id));
            match sessions.get_mut(&session_id) {
                Some(state) if request_id < state.latest_request_id => {
                    return Err(WEB_REQUEST_CANCELLED.into());
                }
                Some(state) if request_id == state.latest_request_id && state.cancelled => {
                    return Err(WEB_REQUEST_CANCELLED.into());
                }
                Some(state) if request_id > state.latest_request_id => {
                    if let Some(handoff) = state.cancelled_handoff.take() {
                        if handoff.detail_id == spec.detail_id {
                            superseded_producer = true;
                            superseded_ai_producer = handoff.ai_upscale;
                        }
                    }
                    state.latest_request_id = request_id;
                    state.cancelled = false;
                    state.at = Instant::now();
                    newer_generation = true;
                }
                Some(state) => state.at = Instant::now(),
                None => {
                    sessions.insert(
                        session_id,
                        WebPlaybackSessionState {
                            latest_request_id: request_id,
                            cancelled: false,
                            cancelled_handoff: None,
                            at: Instant::now(),
                        },
                    );
                    newer_generation = true;
                }
            }
            Some(sessions)
        } else {
            None
        }
    } else {
        None
    };
    // A browser source generation can issue many HTTP requests: native-HLS
    // playlist refreshes, init/segment reads, and fragmented-MP4 reconnects
    // all carry the same session/request pair. Count the generation once,
    // rather than treating each transport request as a new playback request.
    let scoped_web_generation = spec.web_session_id.zip(spec.web_request_id).is_some();
    let new_web_generation = web && (!scoped_web_generation || newer_generation);
    if new_web_generation {
        app.remux_metrics
            .web_requests
            .fetch_add(1, Ordering::Relaxed);
    }
    // The map lock serializes cache validation/replacement with all attaches
    // for this process. Taking it before releasing the playback-session lock
    // makes registration atomic with an explicit cancellation.
    let mut map = crate::lock_recover(&app.remuxes);
    drop(playback_sessions);
    if newer_generation {
        if let Some(session_id) = spec.web_session_id {
            let mut superseded = Vec::new();
            for (job_key, job) in map.iter() {
                if job_key == &spec.job_key || !job.web {
                    continue;
                }
                if let Some(request_id) = job.remove_web_session(session_id) {
                    superseded.push((job.detail_id, request_id));
                    if !job.has_web_requests() {
                        superseded_producer = true;
                        superseded_ai_producer |= job
                            .web_spec
                            .as_ref()
                            .is_some_and(|spec| spec.ai_upscale_shader_file.is_some());
                        job.cancel();
                    }
                }
            }
            for (detail_id, request_id) in superseded {
                record_recent_web_state(app.as_ref(), detail_id, request_id, "cancelled");
            }
        }
    }
    // Resource requests belonging to an established source generation retain
    // its immutable recipe. A new owner of completed fallback bytes must pass
    // current negotiation, device identity and expiry checks, even while an
    // earlier owner's job remains registered. Keep filesystem work off the
    // registry lock and preserve the destination reservation across this gap.
    let completed_fallback = map.get(&spec.job_key).and_then(|job| {
        let existing_owner = spec
            .web_request_id
            .is_some_and(|request| job.owns_web_request(spec.web_session_id, request));
        if !job.is_complete() || existing_owner {
            return None;
        }
        let actual = crate::lock_recover(&job.effective_recipe).clone()?;
        (actual.attempt != fallback::Attempt::Primary).then(|| (job.clone(), actual.identity))
    });
    if let Some((completed, identity)) = completed_fallback {
        drop(map);
        let allowed = fallback::reusable(spec).is_some_and(|actual| actual.identity == identity);
        // Cancellation or supersession may win while the registry is unlocked.
        // Reestablish session -> registry ordering before registering an owner.
        let sessions = crate::lock_recover(&app.web_playback_sessions);
        if let Some((session, request)) = spec.web_session_id.zip(spec.web_request_id) {
            if sessions
                .get(&session)
                .is_none_or(|state| state.cancelled || state.latest_request_id != request)
            {
                return Err(WEB_REQUEST_CANCELLED.into());
            }
        }
        map = crate::lock_recover(&app.remuxes);
        drop(sessions);
        if !allowed
            && map
                .get(&spec.job_key)
                .is_some_and(|job| Arc::ptr_eq(job, &completed))
        {
            let sessions = crate::lock_recover(&completed.web_sessions);
            let same_session_request = spec
                .web_session_id
                .and_then(|session| sessions.get(&session).copied());
            let other_owners = sessions
                .keys()
                .any(|session| Some(*session) != spec.web_session_id)
                || crate::lock_recover(&completed.web_request_ids)
                    .iter()
                    .any(|request| Some(*request) != same_session_request);
            drop(sessions);
            if completed.clients.load(Ordering::Acquire) != 0 || other_owners {
                return Err(
                    "transcode busy (completed fallback is owned by another playback generation)"
                        .into(),
                );
            }
            if !completed.producer_finished.load(Ordering::Acquire) {
                return Ok(RemuxAttachment::Retiring(completed));
            }
            // No response or other playback owner can observe replacement.
            // Same-session older generations were superseded above; their next
            // request is rejected by the global generation tombstone.
            map.remove(&spec.job_key);
        }
    }
    if let Some(job) = map.get(&spec.job_key) {
        let mut disconnect_deadline = crate::lock_recover(&job.disconnect_deadline);
        if job.err().is_none() && !job.cancelled.load(Ordering::Acquire) {
            let replaced = job.add_web_request(spec.web_session_id, spec.web_request_id)?;
            if new_web_generation || !web {
                if let Some(replaced) = replaced {
                    if Some(replaced) != spec.web_request_id {
                        record_recent_web_state(
                            app.as_ref(),
                            spec.detail_id,
                            replaced,
                            "cancelled",
                        );
                    }
                }
                app.remux_metrics
                    .coalesced_requests
                    .fetch_add(1, Ordering::Relaxed);
                if web {
                    app.remux_metrics
                        .web_cache_reuses
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        id = spec.detail_id,
                        dest = %spec.dest.display(),
                        producer_reuse = true,
                        "web playback generation attached to existing remux"
                    );
                } else {
                    tracing::info!(
                        id = spec.detail_id,
                        dest = %spec.dest.display(),
                        "remux attach"
                    );
                }
            } else {
                tracing::debug!(
                    id = spec.detail_id,
                    session_id = spec.web_session_id,
                    request_id = spec.web_request_id,
                    "web media resource attached to active remux"
                );
            }
            if register_client {
                job.clients.fetch_add(1, Ordering::Relaxed);
                job.ever_had_client.store(true, Ordering::Release);
                job.client_epoch.fetch_add(1, Ordering::AcqRel);
                *disconnect_deadline = None;
            }
            let job = job.clone();
            drop(disconnect_deadline);
            drop(map);
            if job.is_complete() {
                cache::touch_recency(&job.dest);
            }
            return Ok(RemuxAttachment::Ready(job));
        }
        drop(disconnect_deadline);
        if !job.producer_finished.load(Ordering::Acquire) {
            return Ok(RemuxAttachment::Retiring(job.clone()));
        }
        map.remove(&spec.job_key);
    }
    // Existing producers and their staging/final artifacts are already in the
    // protected set. Resource reattachments above therefore need neither a
    // cache-directory scan nor a quota decision. New producers and reopened
    // completed outputs still pass the normal bounded cache gate.
    drop(map);
    // Keep the requested candidate protected even before it becomes a live job.
    // Discovery, validation, stale eviction and admission never hold the map.
    let primary_fresh = spec.cacheable && cache_is_fresh_for_key(&spec.dest, &spec.cache_key);
    let reused_fallback = (!primary_fresh).then(|| fallback::reusable(spec)).flatten();
    let fresh = primary_fresh || reused_fallback.is_some();
    if !fresh {
        if spec.dest.is_file() {
            let _maintenance = crate::lock_recover(&app.cache_maintenance);
            tracing::info!(
                id = spec.detail_id,
                dest = %spec.dest.display(),
                "stale remux cache, rebuilding"
            );
            if let Ok(metadata) = spec.dest.metadata() {
                if std::fs::remove_file(&spec.dest).is_ok() {
                    app.remux_metrics.subtract_cache_bytes(metadata.len());
                }
            }
            let _ = std::fs::remove_file(cache_part(&spec.dest));
            cache::refresh_artifacts(&app, [spec.dest.clone(), cache_part(&spec.dest)]);
        }
        // A cache hit returned above. Any remaining stamp is stale or orphaned and
        // must not make newly produced fallback bytes fresh under the primary key.
        let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&spec.dest));
    }
    let requested = HashSet::from([spec.dest.clone()]);
    maintain_app_cache(&app, &requested, false)
        .map_err(|error| format!("transcode cache limits: {error}"))?;
    if fresh {
        app.remux_metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        if new_web_generation {
            app.remux_metrics
                .web_cache_reuses
                .fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                id = spec.detail_id,
                cache_reuse = true,
                "web compatible transcode cache hit"
            );
        }
        let job = Arc::new(RemuxJob {
            detail_id: spec.detail_id,
            web_request_ids: Mutex::new(spec.web_request_id.into_iter().collect()),
            web_sessions: Mutex::new(
                spec.web_session_id
                    .zip(spec.web_request_id)
                    .into_iter()
                    .collect(),
            ),
            web,
            web_spec: web.then(|| spec.clone()),
            cache_hit: true,
            registry_finalized: AtomicBool::new(true),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest: spec.dest.clone(),
            part: cache_part(&spec.dest),
            state: Mutex::new(RemuxState::Complete),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(reused_fallback),
        });
        job.open_output()
            .map_err(|error| format!("open completed remux: {error}"))?;
        register_admitted_job(&app, spec, &job, register_client, register_client)?;
        cache::touch_recency(&job.dest);
        return Ok(RemuxAttachment::Ready(job));
    }
    app.remux_metrics
        .cache_misses
        .fetch_add(1, Ordering::Relaxed);
    if web && !spec.cacheable {
        app.remux_metrics
            .web_seek_restarts
            .fetch_add(1, Ordering::Relaxed);
    }
    // A quality change cancels the previous producer above, but its helper
    // thread needs a short bounded interval to reap FFmpeg and drop its
    // permits. Treat that as an ownership handoff, not fresh contention.
    // Other sessions and unrelated capacity pressure retain immediate busy
    // behavior.
    let job_permit = if superseded_producer {
        app.jobs.acquire_timeout(WEB_SUPERSEDED_JOB_HANDOFF)
    } else {
        app.jobs.try_acquire()
    }
    .ok_or_else(|| format!("transcode busy (max_jobs={})", app.cfg.transcode.max_jobs))?;
    let ai_upscale_permit = if spec.ai_upscale_shader_file.is_some() {
        let permit = if superseded_ai_producer {
            app.ai_upscale_jobs
                .acquire_timeout(WEB_SUPERSEDED_JOB_HANDOFF)
        } else {
            app.ai_upscale_jobs.try_acquire()
        };
        Some(permit.ok_or_else(|| {
            format!(
                "AI upscale busy (ai_upscale_max_jobs={})",
                app.cfg.web.ai_upscale_max_jobs
            )
        })?)
    } else {
        None
    };
    let helper_permit = app
        .helpers
        .try_acquire()
        .map_err(|error| format!("media helper busy: {error}"))?;
    let part = cache_part(&spec.dest);
    if part.exists() {
        let _ = std::fs::remove_file(&part);
    }
    if let Some(parent) = part.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let job = Arc::new(RemuxJob {
        detail_id: spec.detail_id,
        web_request_ids: Mutex::new(spec.web_request_id.into_iter().collect()),
        web_sessions: Mutex::new(
            spec.web_session_id
                .zip(spec.web_request_id)
                .into_iter()
                .collect(),
        ),
        web,
        web_spec: web.then(|| spec.clone()),
        cache_hit: false,
        registry_finalized: AtomicBool::new(false),
        producer_finished: AtomicBool::new(false),
        output: Mutex::new(None),
        startup_observations: WebStartupObservations::default(),
        dest: spec.dest.clone(),
        part: part.clone(),
        state: Mutex::new(RemuxState::Starting),
        changed: tokio::sync::Notify::new(),
        cancelled: AtomicBool::new(false),
        clients: AtomicUsize::new(0),
        ever_had_client: AtomicBool::new(false),
        client_epoch: AtomicU64::new(0),
        disconnect_deadline: Mutex::new(None),
        cacheable: spec.cacheable,
        started: Instant::now(),
        hls_index: Mutex::new(hls::Index::default()),
        effective_recipe: Mutex::new(None),
    });
    register_admitted_job(&app, spec, &job, register_client, true)?;
    spawn_ffmpeg(
        app.clone(),
        spec.clone(),
        job.clone(),
        helper_permit,
        job_permit,
        ai_upscale_permit,
    );
    Ok(RemuxAttachment::Ready(job))
}

// Recheck cancellation after unlocked filesystem/admission work. Reservation
// ownership prevents another producer for this destination during the gap.
fn register_admitted_job(
    app: &App,
    spec: &RemuxJobSpec,
    job: &Arc<RemuxJob>,
    register_client: bool,
    retain: bool,
) -> Result<(), String> {
    let sessions = crate::lock_recover(&app.web_playback_sessions);
    if let Some((session_id, request_id)) = spec.web_session_id.zip(spec.web_request_id) {
        if sessions
            .get(&session_id)
            .is_none_or(|state| state.cancelled || state.latest_request_id != request_id)
        {
            return Err(WEB_REQUEST_CANCELLED.into());
        }
    }
    let mut map = crate::lock_recover(&app.remuxes);
    drop(sessions);
    if register_client {
        job.attach_client();
    }
    if retain {
        map.insert(spec.job_key.clone(), job.clone());
    }
    Ok(())
}

fn browser_preparation_options(mut options: BrowserOutputOptions) -> BrowserOutputOptions {
    options.start_seconds = 0;
    options
}

fn prune_web_transcode_preparations(preparations: &mut HashMap<u64, WebTranscodePreparation>) {
    preparations.retain(|_, preparation| preparation.at.elapsed() < WEB_PREPARATION_RETENTION);
}

/// Reuse source sampling and the verified FFmpeg snapshot across seek
/// generations in one browser playback session.
pub(crate) fn prepared_web_transcode(
    app: &App,
    detail_id: i64,
    session_id: Option<u64>,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
) -> Option<(Arc<std::fs::File>, PathBuf, TranscodeCacheIdentity)> {
    let session_id = session_id?;
    let mut preparations = crate::lock_recover(&app.web_transcode_preparations);
    prune_web_transcode_preparations(&mut preparations);
    let preparation = preparations.get_mut(&session_id)?;
    if preparation.detail_id != detail_id
        || preparation.plan != *plan
        || preparation.options != browser_preparation_options(options)
    {
        return None;
    }
    preparation.at = Instant::now();
    app.remux_metrics
        .web_prepared_reuses
        .fetch_add(1, Ordering::Relaxed);
    tracing::debug!(
        detail_id,
        session_id,
        start_seconds = options.start_seconds,
        "reusing prepared web transcode identity"
    );
    Some((
        Arc::clone(&preparation.source_file),
        preparation.source_path.clone(),
        preparation
            .cache_identity
            .with_browser_options(plan, options),
    ))
}

pub(crate) fn remember_web_transcode_preparation(
    app: &App,
    session_id: Option<u64>,
    preparation: WebTranscodePreparation,
) {
    let Some(session_id) = session_id else {
        return;
    };
    let mut preparations = crate::lock_recover(&app.web_transcode_preparations);
    prune_web_transcode_preparations(&mut preparations);
    if preparations.len() >= MAX_WEB_TRANSCODE_PREPARATIONS
        && !preparations.contains_key(&session_id)
    {
        if let Some(oldest) = preparations
            .iter()
            .min_by_key(|(_, preparation)| preparation.at)
            .map(|(session_id, _)| *session_id)
        {
            preparations.remove(&oldest);
        }
    }
    preparations.insert(session_id, preparation);
}

fn touch_web_transcode_preparation(app: &App, detail_id: i64, session_id: Option<u64>) {
    let Some(session_id) = session_id else {
        return;
    };
    let mut preparations = crate::lock_recover(&app.web_transcode_preparations);
    prune_web_transcode_preparations(&mut preparations);
    if let Some(preparation) = preparations
        .get_mut(&session_id)
        .filter(|preparation| preparation.detail_id == detail_id)
    {
        preparation.at = Instant::now();
    }
}

/// Reuse the immutable plan already bound to a browser source generation.
///
/// Playlist polling, fixed fragment requests, and media-element reconnects all
/// carry the same session/request pair. Returning the original descriptor-
/// backed spec avoids repeatedly sampling a multi-gigabyte source and querying
/// tool identity while preserving the generation's exact output semantics.
pub(crate) fn active_web_job_spec(
    app: &App,
    detail_id: i64,
    session_id: Option<u64>,
    request_id: Option<u64>,
) -> Option<RemuxJobSpec> {
    let (session_id, request_id) = session_id.zip(request_id)?;
    let jobs = crate::lock_recover(&app.remuxes);
    let mut spec = jobs
        .values()
        .find(|job| {
            job.web
                && job.detail_id == detail_id
                && job.owns_web_request(Some(session_id), request_id)
                && !matches!(job.state(), RemuxState::Failed(_) | RemuxState::Cancelled)
        })
        .and_then(|job| job.web_spec.clone())?;
    // Equivalent outputs may be shared by multiple browser tabs. The media
    // plan and descriptor are immutable, but ownership on the cloned request
    // must remain scoped to the caller rather than the generation that first
    // launched the shared producer.
    spec.web_session_id = Some(session_id);
    spec.web_request_id = Some(request_id);
    Some(spec)
}

pub(crate) fn web_job_effective_recipe(
    app: &App,
    detail_id: i64,
    session_id: Option<u64>,
    request_id: Option<u64>,
) -> Option<fallback::EffectiveRecipe> {
    let request_id = request_id?;
    let jobs = crate::lock_recover(&app.remuxes);
    let job = jobs.values().find(|job| {
        job.web && job.detail_id == detail_id && job.owns_web_request(session_id, request_id)
    })?;
    let actual = crate::lock_recover(&job.effective_recipe).clone();
    actual
}

pub(crate) fn web_job_state(
    app: &App,
    detail_id: i64,
    request_id: Option<u64>,
) -> (&'static str, Option<u64>) {
    if let Ok(jobs) = app.remuxes.lock() {
        if let Some(job) = jobs
            .values()
            .find(|job| job.detail_id == detail_id && job.matches_web_request(request_id))
        {
            return match job.state() {
                RemuxState::Starting => ("starting", Some(1)),
                RemuxState::Preprocessing => ("queued", Some(1)),
                RemuxState::Growing => ("producing", None),
                RemuxState::Complete => ("ready", None),
                RemuxState::Failed(_) => ("failed", Some(1)),
                RemuxState::Cancelled => ("cancelled", None),
            };
        }
    }
    let mut recent = crate::lock_recover(&app.recent_remux_states);
    recent.retain(|_, value| value.at.elapsed() < Duration::from_secs(60));
    let recent_state = if let Some(request_id) = request_id {
        recent.get(&(detail_id, request_id))
    } else {
        recent
            .iter()
            .filter(|((id, _), _)| *id == detail_id)
            .max_by_key(|(_, state)| state.at)
            .map(|(_, state)| state)
    };
    if let Some(state) = recent_state {
        let state = (state.state, (state.state == "failed").then_some(1));
        return state;
    }
    drop(recent);
    let helpers = app.helpers.metrics();
    if app.jobs.in_use() >= app.cfg.transcode.max_jobs as usize
        || helpers.active >= helpers.max_active
    {
        ("queued", Some(1))
    } else {
        ("idle", None)
    }
}

/// Return the exact media time represented by complete fragments currently
/// available from a web-compatible producer. Parsing is incremental and only
/// happens when a client asks for status, so ordinary media delivery does not
/// gain a polling cost.
pub(crate) fn web_job_produced_seconds(
    app: &App,
    detail_id: i64,
    request_id: Option<u64>,
) -> Option<f64> {
    let job = {
        let jobs = crate::lock_recover(&app.remuxes);
        jobs.values()
            .find(|job| job.detail_id == detail_id && job.matches_web_request(request_id))
            .cloned()
    }?;
    // Small primary/preprocessing artifacts can still be replaced by portable
    // fallback. Status polling must not pin those provisional bytes.
    let state = job.state();
    if !matches!(state, RemuxState::Growing | RemuxState::Complete) {
        return None;
    }
    let complete = state == RemuxState::Complete;
    let output = job.open_output().ok()?;
    let mut index = crate::lock_recover(&job.hls_index);
    index.update_file(&output, complete).ok()?;
    index.produced_duration_seconds()
}

fn keep_web_request_alive_for(
    app: &App,
    detail_id: i64,
    session_id: Option<u64>,
    request_id: u64,
    lease: Duration,
) -> bool {
    let playback_sessions = session_id.and_then(|session_id| {
        let mut sessions = crate::lock_recover(&app.web_playback_sessions);
        prune_web_playback_sessions(app, &mut sessions, None);
        let state = sessions.get_mut(&session_id)?;
        if state.cancelled || state.latest_request_id != request_id {
            return None;
        }
        state.at = Instant::now();
        Some(sessions)
    });
    if session_id.is_some() && playback_sessions.is_none() {
        return false;
    }

    // Match attach/cancel's playback-session -> job-map lock ordering so a
    // heartbeat cannot revive a generation concurrently superseded by DELETE.
    let jobs = crate::lock_recover(&app.remuxes);
    drop(playback_sessions);
    let Some(job) = jobs.values().find(|job| {
        job.web && job.detail_id == detail_id && job.owns_web_request(session_id, request_id)
    }) else {
        return false;
    };
    job.renew_disconnected_web_lease(lease);
    true
}

pub(crate) fn keep_web_request_alive(
    app: &App,
    detail_id: i64,
    session_id: Option<u64>,
    request_id: u64,
) -> bool {
    let retained = keep_web_request_alive_for(
        app,
        detail_id,
        session_id,
        request_id,
        WEB_ACTIVE_SESSION_LEASE,
    );
    if retained {
        touch_web_transcode_preparation(app, detail_id, session_id);
    }
    retained
}

pub(crate) fn record_browser_timing(
    app: &App,
    detail_id: i64,
    session_id: u64,
    request_id: u64,
    stage: performance::Stage,
    elapsed_ms: u64,
) -> bool {
    let key = (detail_id, session_id, request_id);
    if elapsed_ms > 120_000 || !app.remux_metrics.performance.contains(key) {
        return false;
    }
    let jobs = crate::lock_recover(&app.remuxes);
    let owns = jobs.values().any(|job| {
        job.web
            && job.detail_id == detail_id
            && job.owns_web_request(Some(session_id), request_id)
            && !job.cancelled.load(Ordering::Acquire)
            && !matches!(job.state(), RemuxState::Failed(_) | RemuxState::Cancelled)
    });
    if owns {
        app.remux_metrics
            .performance
            .record(Some(key), stage, Duration::from_millis(elapsed_ms));
    }
    owns
}

/// Record the first occurrence of a browser startup phase for an active source
/// generation. Elapsed time comes from the server-owned job clock rather than
/// a client duration, so reports cannot inject arbitrary metric values.
pub(crate) fn record_web_startup_event(
    app: &App,
    detail_id: i64,
    session_id: u64,
    request_id: u64,
    event: WebStartupEvent,
) -> bool {
    let jobs = crate::lock_recover(&app.remuxes);
    let Some(job) = jobs.values().find(|job| {
        job.web
            && job.detail_id == detail_id
            && job.owns_web_request(Some(session_id), request_id)
            && !matches!(job.state(), RemuxState::Failed(_) | RemuxState::Cancelled)
    }) else {
        return false;
    };
    if event
        .observation(&job.startup_observations)
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let elapsed = job.started.elapsed();
        event.metric(&app.remux_metrics).record(elapsed);
        tracing::info!(
            id = job.detail_id,
            request_id,
            session_id,
            startup_event = event.as_str(),
            startup_elapsed_ms = rusty_dlna_helper::duration_millis_saturating(elapsed),
            cache_reuse = job.cache_hit,
            "web compatible startup phase reached"
        );
    }
    true
}

/// Cancel an explicitly superseded browser generation without disturbing a
/// producer that is still owned by another playback session. Recording the
/// cancellation before inspecting jobs also rejects a late media GET whose
/// DELETE won the network race.
pub(crate) fn cancel_web_request(
    app: &App,
    detail_id: i64,
    session_id: Option<u64>,
    request_id: u64,
) -> bool {
    let mut playback_sessions = session_id.map(|session_id| {
        let mut sessions = crate::lock_recover(&app.web_playback_sessions);
        prune_web_playback_sessions(app, &mut sessions, Some(session_id));
        match sessions.get_mut(&session_id) {
            Some(state) if request_id >= state.latest_request_id => {
                state.latest_request_id = request_id;
                state.cancelled = true;
                state.at = Instant::now();
            }
            Some(state) => state.at = Instant::now(),
            None => {
                sessions.insert(
                    session_id,
                    WebPlaybackSessionState {
                        latest_request_id: request_id,
                        cancelled: true,
                        cancelled_handoff: None,
                        at: Instant::now(),
                    },
                );
            }
        }
        sessions
    });
    let mut jobs = crate::lock_recover(&app.remuxes);
    let mut matched = false;
    let mut cancelled_producer = false;
    let mut cancelled_ai_producer = false;
    for job in jobs.values_mut() {
        if !job.web || job.detail_id != detail_id {
            continue;
        }
        if job.remove_web_request(session_id, request_id) {
            matched = true;
            if !job.has_web_requests()
                && matches!(
                    job.state(),
                    RemuxState::Starting | RemuxState::Preprocessing | RemuxState::Growing
                )
            {
                cancelled_producer = true;
                cancelled_ai_producer |= job
                    .web_spec
                    .as_ref()
                    .is_some_and(|spec| spec.ai_upscale_shader_file.is_some());
                job.cancel();
            }
        }
    }
    if cancelled_producer {
        if let (Some(session_id), Some(sessions)) = (session_id, playback_sessions.as_mut()) {
            if let Some(state) = sessions.get_mut(&session_id) {
                if state.cancelled && state.latest_request_id == request_id {
                    state.cancelled_handoff = Some(WebCancelledProducerHandoff {
                        detail_id,
                        ai_upscale: cancelled_ai_producer,
                    });
                }
            }
        }
    }
    drop(jobs);
    drop(playback_sessions);
    if matched || session_id.is_some() {
        record_recent_web_state(app, detail_id, request_id, "cancelled");
        true
    } else {
        false
    }
}

#[cfg(test)]
type ReadinessTestHook = fn(&RemuxJob);

#[cfg(test)]
fn readiness_test_hooks() -> &'static Mutex<HashMap<PathBuf, ReadinessTestHook>> {
    static HOOKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, ReadinessTestHook>>> =
        std::sync::OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn wait_ready(job: &Arc<RemuxJob>) -> Result<PathBuf, String> {
    wait_ready_until(job, Instant::now() + FIRST_WAIT).await
}

async fn wait_ready_until(job: &Arc<RemuxJob>, mut deadline: Instant) -> Result<PathBuf, String> {
    loop {
        let notified = job.changed.notified();
        match job.state() {
            RemuxState::Complete | RemuxState::Growing => {
                let pin_job = job.clone();
                let pinned = tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    {
                        let hook =
                            crate::lock_recover(readiness_test_hooks()).remove(&pin_job.part);
                        if let Some(hook) = hook {
                            hook(&pin_job);
                        }
                    }
                    pin_job.pin_ready_output()
                })
                .await
                .map_err(|error| format!("remux readiness task: {error}"))??;
                if let Some(path) = pinned {
                    return Ok(path);
                }
                // An unpinned attempt can fail after the Growing observation.
                // Its fallback clears the pathname and returns to Starting
                // under the same output lock. Retain this request's deadline.
            }
            RemuxState::Failed(error) => return Err(error),
            RemuxState::Cancelled => return Err(REMUX_CANCELLED.into()),
            RemuxState::Preprocessing => {
                // Earlier Dolby Vision stages remain private. The final mux
                // becomes Growing only after a complete playable segment.
                notified.await;
                deadline = Instant::now() + FIRST_WAIT;
                continue;
            }
            RemuxState::Starting => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
            return Err(format!(
                "remux produced no data in {}s",
                FIRST_WAIT.as_secs()
            ));
        }
    }
}

// Deferred remux responses bypass the route handler's HEAD body suppression,
// including errors raised during admission, readiness and range handling.
async fn write_remux_response(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    mut response: HttpResponse,
    head: bool,
) -> std::io::Result<bool> {
    if head {
        response.body.clear();
    }
    crate::socket_write_http_response(app, sock, &response).await
}

pub async fn serve_remux(
    app: &Arc<App>,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    mut spec: RemuxJobSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let head = req.method.eq_ignore_ascii_case("HEAD");
    let resumable_download = req.method.eq_ignore_ascii_case("GET")
        && req.path.starts_with("/web/media/")
        && matches!(
            req.header("X-RustyDLNA-Download"),
            Some("resumable" | "progressive")
        )
        && web_delivery(req).is_none();
    if resumable_download {
        // The system download queue reconnects after a preparation response,
        // including while its app is suspended. Keep this bounded producer
        // alive between those readers; explicit generation cancellation still
        // owns removal. Browser playback retains its reconnect grace.
        spec.continue_after_disconnect = true;
    }
    let attach_app = app.clone();
    let attach_spec = spec.clone();
    // Admission may wait for a cancelled producer's bounded cleanup handoff.
    // Keep it off the asynchronous socket worker, including cache filesystem I/O.
    let attached = tokio::task::spawn_blocking(move || {
        let job = attach_for_client(attach_app.clone(), attach_spec.clone())?;
        // If the awaiting request disappears during admission, dropping the
        // blocking task's result must still release its newly registered reader.
        Ok::<_, String>(RemuxClient {
            app: attach_app,
            job,
            job_key: attach_spec.job_key,
            continue_after_disconnect: attach_spec.continue_after_disconnect,
        })
    })
    .await?;
    let _client = match attached {
        Ok(j) => j,
        Err(e) => {
            if e == WEB_REQUEST_CANCELLED {
                tracing::debug!(
                    id = spec.detail_id,
                    path = %req.path,
                    ua = req.user_agent().unwrap_or("-"),
                    "superseded web media request ignored"
                );
            } else {
                tracing::error!(
                    id = spec.detail_id,
                    path = %req.path,
                    ua = req.user_agent().unwrap_or("-"),
                    "{e}"
                );
            }
            let err = if req.path.starts_with("/web/media/") {
                if e == WEB_REQUEST_CANCELLED {
                    crate::web_ui::transcode_stream_error(409, "transcode_cancelled")
                } else {
                    app.remux_metrics
                        .web_failures_busy
                        .fetch_add(1, Ordering::Relaxed);
                    crate::web_ui::transcode_stream_error(503, "transcode_busy")
                }
            } else {
                let mut response = HttpResponse::html(
                    503,
                    "Service Unavailable",
                    "compatible media is temporarily unavailable",
                );
                response.set("Retry-After", "1");
                response
            };
            write_remux_response(app, sock, err, head).await?;
            return Ok(());
        }
    };
    let job = _client.job.clone();
    if resumable_download {
        if req.header("X-RustyDLNA-Download") == Some("progressive") {
            return serve_progressive_download(app, sock, req, &job, spec.mime).await;
        }
        return serve_resumable_download(app, sock, req, &job, spec.mime).await;
    }
    let _path = match wait_ready(&job).await {
        Ok(p) => p,
        Err(e) => {
            let cancelled = e == REMUX_CANCELLED;
            if cancelled {
                tracing::debug!(
                    id = spec.detail_id,
                    path = %req.path,
                    ua = req.user_agent().unwrap_or("-"),
                    "superseded web media request cancelled before initial bytes"
                );
            } else {
                tracing::error!(
                    id = spec.detail_id,
                    path = %req.path,
                    ua = req.user_agent().unwrap_or("-"),
                    "{e}"
                );
            }
            let err = if req.path.starts_with("/web/media/") {
                if cancelled {
                    crate::web_ui::transcode_stream_error(409, "transcode_cancelled")
                } else {
                    crate::web_ui::transcode_stream_error(500, "transcode_failed")
                }
            } else {
                HttpResponse::html(
                    500,
                    "Internal Server Error",
                    "compatible media generation failed",
                )
            };
            write_remux_response(app, sock, err, head).await?;
            return Ok(());
        }
    };
    // wait_ready pins the ready attempt before exposing response headers.
    if job.web
        && job
            .startup_observations
            .initial_bytes
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        let elapsed = job.started.elapsed();
        app.remux_metrics.web_startup_initial_bytes.record(elapsed);
        tracing::info!(
            id = job.detail_id,
            startup_to_initial_bytes_ms = rusty_dlna_helper::duration_millis_saturating(elapsed),
            cache_reuse = job.cache_hit,
            "web compatible media produced initial bytes"
        );
    }
    match web_delivery(req) {
        Some("hls") => return serve_fragment_playlist(app, sock, req, &job, head, false).await,
        Some("mse") => return serve_fragment_playlist(app, sock, req, &job, head, true).await,
        Some("hls_init" | "mse_init") => {
            return serve_hls_resource(app, sock, req, &job, "video/mp4", head).await
        }
        Some("hls_segment" | "mse_segment") => {
            return serve_hls_resource(app, sock, req, &job, "video/iso.segment", head).await
        }
        _ => {}
    }
    let finished = job.is_complete() && current_len_async(&job).await? > 0;
    if finished {
        return serve_finished(app, sock, req, &job, spec.mime, head).await;
    }
    serve_growing(app, sock, req, &job, spec.mime, head).await
}

/// A bounded range has a truthful wire length even while the output grows.
/// Native clients durably append these ranges and use the final total as the
/// completion boundary. Pinning prevents producer fallback from replacing bytes
/// already exposed to a reader; the validator also rejects a replaced job.
async fn serve_progressive_download(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    job: &Arc<RemuxJob>,
    mime: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (start, requested_end) = match req.header("Range").map(parse_open_range) {
        Some(Ok(range)) => range,
        None => (0, None),
        Some(Err(_)) => {
            write_remux_response(
                app,
                sock,
                HttpResponse::html(400, "Bad Request", "invalid range"),
                false,
            )
            .await?;
            return Ok(());
        }
    };
    if wait_ready(job).await.is_err() {
        return serve_resumable_download(app, sock, req, job, mime).await;
    }
    let maximum_end = start.saturating_add(64 * 1024 * 1024 - 1);
    let requested_end = requested_end.unwrap_or(maximum_end).min(maximum_end);
    // Batch slow producers without waiting for the entire movie. A timeout can
    // still expose a smaller available prefix; no received bytes are discarded.
    let minimum = if start == 0 {
        1024 * 1024
    } else {
        8 * 1024 * 1024
    };
    let need = start
        .saturating_add(minimum)
        .min(requested_end.saturating_add(1));
    let _ = wait_offset(job, need).await;
    let metadata_job = job.clone();
    let (size, etag, complete) = tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::MetadataExt;
        let complete = metadata_job.is_complete();
        let output = metadata_job.open_output()?;
        let metadata = output.metadata()?;
        // File creation identity survives append, final rename, registry
        // eviction and server restart. A process/job identity would incorrectly
        // reject a paused download when the same cached file is reattached.
        let etag = metadata.created().ok().map(|created| {
            let identity = format!("{created:?}:{}:{}", metadata.dev(), metadata.ino());
            let digest = Sha256::digest(identity.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!("\"{digest}\"")
        });
        Ok::<_, std::io::Error>((metadata.len(), etag, complete))
    })
    .await??;
    let Some(etag) = etag else {
        // Filesystems without stable creation metadata retain finalized-only
        // delivery instead of promising an unsafe validator for growing bytes.
        return serve_resumable_download(app, sock, req, job, mime).await;
    };
    if req.header("If-Match").is_some_and(|value| value != etag) {
        write_remux_response(
            app,
            sock,
            HttpResponse::html(412, "Precondition Failed", "prepared output changed"),
            false,
        )
        .await?;
        return Ok(());
    }
    if !rusty_dlna_http::range::if_range_matches(req.header("If-Range"), Some(&etag)) {
        // A native archive for an earlier output cannot be joined to this one.
        // The ordinary finalized response ignores its stale Range, as required
        // by If-Range; a growing replacement remains a preparation response.
        return serve_resumable_download(app, sock, req, job, mime).await;
    }
    if start >= size {
        if !complete {
            return serve_resumable_download(app, sock, req, job, mime).await;
        }
        let mut response =
            HttpResponse::html(416, "Requested Range Not Satisfiable", "range past EOF");
        response.set("Content-Range", format!("bytes */{size}"));
        response.set("ETag", etag);
        response.set("X-RustyDLNA-Download", "progressive");
        write_remux_response(app, sock, response, false).await?;
        return Ok(());
    }
    let end = requested_end.min(size - 1);
    let total = if complete {
        size.to_string()
    } else {
        "*".to_owned()
    };
    let mut response = live_transcode_response(mime);
    response.status = 206;
    response.reason = "Partial Content".into();
    response.set("Content-Range", format!("bytes {start}-{end}/{total}"));
    response.set("Content-Length", end - start + 1);
    response.set("ETag", etag);
    response.set("X-RustyDLNA-Download", "progressive");
    if write_remux_response(app, sock, response, false).await? {
        stream_growing(app, sock, job, start, Some(end)).await?;
    }
    Ok(())
}

/// Native background downloads need a stable validator and a final length.
/// A close-delimited growing response cannot distinguish EOF from a network
/// interruption, and cannot supply URLSession's native resume contract.
async fn serve_resumable_download(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    job: &Arc<RemuxJob>,
    mime: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response = match job.state() {
        RemuxState::Complete => {
            let pin_job = job.clone();
            tokio::task::spawn_blocking(move || pin_job.pin_ready_output()).await??;
            return serve_finished(app, sock, req, job, mime, false).await;
        }
        RemuxState::Failed(_) => crate::web_ui::transcode_stream_error(500, "transcode_failed"),
        RemuxState::Cancelled => crate::web_ui::transcode_stream_error(409, "transcode_cancelled"),
        _ => {
            let mut response = HttpResponse::new(202, "Accepted");
            response.set("Content-Length", "0");
            response.set("X-RustyDLNA-Download", "preparing");
            response.set("Retry-After", "30");
            response.set("Cache-Control", "private, no-store");
            response
        }
    };
    write_remux_response(app, sock, response, false).await?;
    Ok(())
}

fn web_delivery(req: &HttpRequest) -> Option<&str> {
    req.query.split('&').find_map(|entry| {
        let (name, value) = entry.split_once('=')?;
        (name == "delivery").then_some(value)
    })
}

fn fragment_resource_uris(
    req: &HttpRequest,
    playlist_delivery: &str,
) -> Result<(String, String, usize), String> {
    let mut found = false;
    let mut mse_after = None;
    let query = req
        .query
        .split('&')
        .map(|entry| {
            let (name, value) = entry
                .split_once('=')
                .ok_or_else(|| "invalid HLS media query".to_owned())?;
            if name == "delivery" {
                if found || value != playlist_delivery {
                    return Err("invalid fragmented delivery query".into());
                }
                found = true;
                Ok(None)
            } else if name == "mse_after" {
                if playlist_delivery != "mse" || mse_after.is_some() {
                    return Err("invalid Media Source fragment cursor".into());
                }
                let cursor = value
                    .parse::<usize>()
                    .map_err(|_| "invalid Media Source fragment cursor".to_owned())?;
                if cursor > MAX_MSE_FRAGMENT_CURSOR {
                    return Err("Media Source fragment cursor is too large".into());
                }
                mse_after = Some(cursor);
                // The cursor controls the playlist response only. Fixed init
                // and fragment resource URLs must remain stable across polls.
                Ok(None)
            } else {
                Ok(Some(entry.to_owned()))
            }
        })
        .collect::<Result<Vec<_>, String>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("&");
    if !found {
        return Err("HLS delivery query is missing".into());
    }
    let media_path = req
        .path
        .strip_suffix(".m3u8")
        .ok_or_else(|| "fragment playlist path has the wrong extension".to_owned())?;
    let separator = if query.is_empty() { '?' } else { '&' };
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    Ok((
        format!("{media_path}.mp4{query}{separator}delivery={playlist_delivery}_init"),
        format!("{media_path}.m4s{query}{separator}delivery={playlist_delivery}_segment"),
        mse_after.unwrap_or(0),
    ))
}

async fn serve_fragment_playlist(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    job: &Arc<RemuxJob>,
    head: bool,
    media_source: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let deadline = Instant::now() + FIRST_WAIT;
    let playlist_delivery = if media_source { "mse" } else { "hls" };
    let all_fragments_independent = !media_source
        && job
            .web_spec
            .as_ref()
            .is_some_and(|spec| spec.hls_all_fragments_independent);
    let (init_uri, segment_uri, mse_after) = fragment_resource_uris(req, playlist_delivery)?;
    let timing_key = crate::web_ui::playback_timing_key(req, job.detail_id);
    let mut first_fragment_observed = false;
    let playlist = loop {
        match job.state() {
            RemuxState::Failed(error) => return Err(error.into()),
            RemuxState::Cancelled => {
                tracing::debug!(
                    id = job.detail_id,
                    path = %req.path,
                    ua = req.user_agent().unwrap_or("-"),
                    "superseded fragment playlist request cancelled"
                );
                let response = crate::web_ui::transcode_stream_error(409, "transcode_cancelled");
                write_remux_response(app, sock, response, head).await?;
                return Ok(());
            }
            _ => {}
        }
        let complete = job.is_complete();
        let index_job = job.clone();
        let init_uri = init_uri.clone();
        let segment_uri = segment_uri.clone();
        let observe_fragment = !first_fragment_observed;
        let indexed = tokio::task::spawn_blocking(move || {
            let output = index_job
                .open_output()
                .map_err(|error| format!("open HLS media: {error}"))?;
            let mut index = crate::lock_recover(&index_job.hls_index);
            index.update_file(&output, complete)?;
            let observed = (observe_fragment && index.produced_duration_seconds().is_some())
                .then(Instant::now);
            let playlist = if media_source {
                index
                    .has_mse_fragments_after(mse_after, complete)
                    .then(|| index.mse_playlist_view(mse_after))
                    .transpose()
            } else if all_fragments_independent {
                index
                    .has_independent_startup_buffer(complete)
                    .then(|| {
                        index.playlist_view_for(
                            true,
                            timing_key.map(|(_, session, request)| (session, request)),
                        )
                    })
                    .transpose()
            } else {
                index
                    .has_startup_buffer(complete)
                    .then(|| {
                        index.playlist_view_for(
                            false,
                            timing_key.map(|(_, session, request)| (session, request)),
                        )
                    })
                    .transpose()
            };
            drop(index);
            let playlist = playlist.and_then(|view| {
                view.map(|view| view.render(&init_uri, &segment_uri))
                    .transpose()
            });
            Ok::<_, String>((observed, playlist))
        })
        .await?;
        let indexed = match indexed {
            Ok((observed, playlist)) => {
                if let Some(observed) = observed {
                    first_fragment_observed = true;
                    if let Some(key) = timing_key {
                        app.remux_metrics.performance.since_entry_at(
                            key,
                            performance::Stage::FirstCompleteFragment,
                            observed,
                        );
                    }
                }
                playlist
            }
            Err(error) => Err(error),
        };
        match indexed {
            Ok(Some(playlist)) => break playlist,
            Ok(None) if !complete && Instant::now() < deadline => {
                let notified = job.changed.notified();
                match job.state() {
                    RemuxState::Failed(error) => return Err(error.into()),
                    RemuxState::Cancelled => {
                        tracing::debug!(
                            id = job.detail_id,
                            path = %req.path,
                            ua = req.user_agent().unwrap_or("-"),
                            "superseded fragment playlist request cancelled"
                        );
                        let response =
                            crate::web_ui::transcode_stream_error(409, "transcode_cancelled");
                        write_remux_response(app, sock, response, head).await?;
                        return Ok(());
                    }
                    _ => {}
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let _ = tokio::time::timeout(remaining.min(POLL), notified).await;
            }
            Ok(None) => return Err("transcode produced no complete media segment".into()),
            Err(error) => {
                if error.starts_with("resource_limit:") {
                    let response = crate::web_ui::transcode_stream_error(413, "resource_limit");
                    write_remux_response(app, sock, response, head).await?;
                    return Ok(());
                }

                // The producer removes its partial file immediately after
                // publishing Cancelled. If cleanup wins this race, retain the
                // cancellation contract instead of reporting the missing
                // obsolete file as a playlist failure.
                if job.state() == RemuxState::Cancelled {
                    tracing::debug!(
                        id = job.detail_id,
                        path = %req.path,
                        ua = req.user_agent().unwrap_or("-"),
                        "superseded fragment playlist request cancelled during cleanup"
                    );
                    let response =
                        crate::web_ui::transcode_stream_error(409, "transcode_cancelled");
                    write_remux_response(app, sock, response, head).await?;
                    return Ok(());
                }
                return Err(error.into());
            }
        }
    };
    if job
        .startup_observations
        .playlist_ready
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let elapsed = job.started.elapsed();
        app.remux_metrics.web_startup_playlist_ready.record(elapsed);
        tracing::info!(
            id = job.detail_id,
            startup_to_playlist_ready_ms = rusty_dlna_helper::duration_millis_saturating(elapsed),
            delivery = playlist_delivery,
            cache_reuse = job.cache_hit,
            "web compatible fragment playlist became ready"
        );
    }
    let mut response = HttpResponse::new(200, "OK");
    response.set("Content-Type", "application/vnd.apple.mpegurl");
    if media_source {
        // Indexing pinned this attempt before the playlist view was created.
        // Disclose only fixed, supported fallback formats, before any init or
        // movie-fragment bytes can enter the client's empty SourceBuffer.
        if let Some(actual) = crate::lock_recover(&job.effective_recipe).as_ref() {
            if let Some(video) = actual.mse_video_output() {
                response.set(rusty_dlna_protocol::MSE_VIDEO_OUTPUT_HEADER, video.id());
            }
            if let Some(audio) = actual.mse_audio_codec() {
                response.set(rusty_dlna_protocol::MSE_AUDIO_CODEC_HEADER, audio);
            }
        }
    }
    response.set("Cache-Control", "no-store");
    response.set("Content-Length", playlist.len());
    if !head {
        response.body = playlist.into_bytes();
    }
    write_remux_response(app, sock, response, head).await?;
    Ok(())
}

fn hls_resource_slice(req: &HttpRequest) -> Result<(u64, u64), String> {
    let mut offset = None;
    let mut length = None;
    for entry in req.query.split('&') {
        let Some((name, value)) = entry.split_once('=') else {
            return Err("invalid HLS resource query".into());
        };
        let target = match name {
            "hls_offset" => &mut offset,
            "hls_length" => &mut length,
            _ => continue,
        };
        if target.is_some() {
            return Err("duplicate HLS resource range".into());
        }
        *target = Some(
            value
                .parse::<u64>()
                .map_err(|_| "invalid HLS resource range".to_owned())?,
        );
    }
    let offset = offset.ok_or_else(|| "HLS resource offset is missing".to_owned())?;
    let length = length
        .filter(|length| *length > 0)
        .ok_or_else(|| "HLS resource length is missing".to_owned())?;
    offset
        .checked_add(length)
        .ok_or_else(|| "HLS resource range overflow".to_owned())?;
    Ok((offset, length))
}

async fn serve_hls_resource(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    job: &Arc<RemuxJob>,
    mime: &str,
    head: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (offset, length) = hls_resource_slice(req)?;
    let slice_end = offset
        .checked_add(length)
        .ok_or("HLS resource range overflow")?;
    if let Err(error) = wait_offset(job, slice_end).await {
        if error == REMUX_CANCELLED {
            tracing::debug!(
                id = job.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "superseded compatible media resource request cancelled"
            );
            let response = crate::web_ui::transcode_stream_error(409, "transcode_cancelled");
            write_remux_response(app, sock, response, head).await?;
            return Ok(());
        }
        return Err(error.into());
    }
    if job.state() == RemuxState::Cancelled {
        tracing::debug!(
            id = job.detail_id,
            path = %req.path,
            ua = req.user_agent().unwrap_or("-"),
            "superseded compatible media resource request cancelled"
        );
        let response = crate::web_ui::transcode_stream_error(409, "transcode_cancelled");
        write_remux_response(app, sock, response, head).await?;
        return Ok(());
    }
    if current_len_async(job).await? < slice_end {
        return Err("HLS resource is outside the compatible output".into());
    }
    let range = match req.header("Range") {
        None => None,
        Some(value) => match parse_byte_range(value, length) {
            Ok(range) => range,
            Err(RangeError::Invalid) => {
                let err = HttpResponse::html(400, "Bad Request", "invalid HLS resource range");
                write_remux_response(app, sock, err, head).await?;
                return Ok(());
            }
            Err(RangeError::Unsatisfiable) => {
                let mut err = HttpResponse::html(
                    416,
                    "Requested Range Not Satisfiable",
                    "range past HLS resource",
                );
                err.set("Content-Range", format!("bytes */{length}"));
                write_remux_response(app, sock, err, head).await?;
                return Ok(());
            }
        },
    };
    let (relative_start, relative_end) = range
        .map(|range| (range.start, range.end))
        .unwrap_or((0, length - 1));
    let start = offset
        .checked_add(relative_start)
        .ok_or("HLS resource start overflow")?;
    let end = offset
        .checked_add(relative_end)
        .ok_or("HLS resource end overflow")?;
    let mut response = media_response(rusty_dlna_http::MediaResponseOptions {
        server: &app.server,
        date: &now_imf_date(),
        mime,
        size: length,
        range,
        body: Vec::new(),
        pn: None,
        ci: 1,
    });
    response.set("Cache-Control", "no-store");
    response.persist = false;
    if head {
        write_remux_response(app, sock, response, head).await?;
        return Ok(());
    }
    if !write_remux_response(app, sock, response, head).await? {
        return Ok(());
    }
    if let Err(error) = stream_growing(app, sock, job, start, Some(end)).await {
        if job.state() == RemuxState::Cancelled {
            tracing::debug!(
                id = job.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "superseded compatible media resource closed during cleanup"
            );
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

struct RemuxClient {
    app: Arc<App>,
    job: Arc<RemuxJob>,
    job_key: String,
    continue_after_disconnect: bool,
}

impl Drop for RemuxClient {
    fn drop(&mut self) {
        self.job.detach_client(
            self.app.clone(),
            self.job_key.clone(),
            self.continue_after_disconnect,
            WEB_RECONNECT_GRACE,
            WEB_EPHEMERAL_RETENTION,
        );
    }
}

pub(crate) fn cancel_all(app: &App) {
    let jobs = crate::lock_recover(&app.remuxes)
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for job in jobs {
        job.cancel();
    }
}

pub(crate) async fn wait_for_shutdown(app: &App, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if app.jobs.in_use() == 0 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::error!(
                jobs = app.jobs.in_use(),
                "transcode jobs did not reap before shutdown deadline"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(crate) fn runtime_status(app: &App) -> RemuxStatus {
    let (supervisor_ready, active, queued, oldest_job_secs) = match app.remuxes.lock() {
        Ok(jobs) => {
            let mut active = 0usize;
            let mut queued = 0usize;
            let mut oldest = 0u64;
            for job in jobs.values() {
                match job.state() {
                    RemuxState::Starting | RemuxState::Preprocessing => queued += 1,
                    RemuxState::Growing => active += 1,
                    _ => {}
                }
                oldest = oldest.max(job.started.elapsed().as_secs());
            }
            (true, active, queued, oldest)
        }
        Err(_) => (false, 0, 0, 0),
    };
    RemuxStatus {
        supervisor_ready,
        active,
        queued,
        completed_total: app.remux_metrics.completed.load(Ordering::Relaxed),
        failed_total: app.remux_metrics.failed.load(Ordering::Relaxed),
        cancelled_total: app.remux_metrics.cancelled.load(Ordering::Relaxed),
        cache_hits_total: app.remux_metrics.cache_hits.load(Ordering::Relaxed),
        cache_misses_total: app.remux_metrics.cache_misses.load(Ordering::Relaxed),
        coalesced_requests_total: app.remux_metrics.coalesced_requests.load(Ordering::Relaxed),
        cache_maintenance_total: app.remux_metrics.cache_maintenance.load(Ordering::Relaxed),
        cache_maintenance_failures_total: app
            .remux_metrics
            .cache_maintenance_failures
            .load(Ordering::Relaxed),
        cache_evicted_files_total: app
            .remux_metrics
            .cache_evicted_files
            .load(Ordering::Relaxed),
        cache_evicted_bytes_total: app
            .remux_metrics
            .cache_evicted_bytes
            .load(Ordering::Relaxed),
        cache_bytes: app.remux_metrics.cache_bytes.load(Ordering::Relaxed),
        oldest_job_secs,
        cache_scans: app.remux_metrics.cache_scans.load(Ordering::Relaxed),
        cache_scan_entries: app.remux_metrics.cache_scan_entries.load(Ordering::Relaxed),
        cache_lock_wait: app.remux_metrics.cache_lock_wait.snapshot(),
        cache_registry_wait: app.remux_metrics.cache_registry_wait.snapshot(),
        cache_sweep_duration: app.remux_metrics.cache_sweep_duration.snapshot(),
        cache_maintenance_duration: app.remux_metrics.cache_maintenance_duration.snapshot(),
        web_requests_total: app.remux_metrics.web_requests.load(Ordering::Relaxed),
        web_seek_restarts_total: app.remux_metrics.web_seek_restarts.load(Ordering::Relaxed),
        web_cache_reuses_total: app.remux_metrics.web_cache_reuses.load(Ordering::Relaxed),
        web_prepared_reuses_total: app
            .remux_metrics
            .web_prepared_reuses
            .load(Ordering::Relaxed),
        web_cancelled_total: app.remux_metrics.web_cancelled.load(Ordering::Relaxed),
        web_failures_busy_total: app.remux_metrics.web_failures_busy.load(Ordering::Relaxed),
        web_failures_producer_total: app
            .remux_metrics
            .web_failures_producer
            .load(Ordering::Relaxed),
        web_startup_initial_bytes: app.remux_metrics.web_startup_initial_bytes.snapshot(),
        web_startup_playlist_ready: app.remux_metrics.web_startup_playlist_ready.snapshot(),
        web_startup_mse_playlist_received: app
            .remux_metrics
            .web_startup_mse_playlist_received
            .snapshot(),
        web_startup_mse_init_fetched: app.remux_metrics.web_startup_mse_init_fetched.snapshot(),
        web_startup_mse_init_appended: app.remux_metrics.web_startup_mse_init_appended.snapshot(),
        web_startup_mse_first_fragment_fetched: app
            .remux_metrics
            .web_startup_mse_first_fragment_fetched
            .snapshot(),
        web_startup_mse_first_fragment_appended: app
            .remux_metrics
            .web_startup_mse_first_fragment_appended
            .snapshot(),
        web_startup_canplay: app.remux_metrics.web_startup_canplay.snapshot(),
        web_startup_playing: app.remux_metrics.web_startup_playing.snapshot(),
    }
}

async fn serve_finished(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    job: &Arc<RemuxJob>,
    mime: &str,
    head: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metadata_job = job.clone();
    let (size, etag) = tokio::task::spawn_blocking(move || {
        cache::touch_recency(&metadata_job.dest);
        let output = metadata_job.open_output()?;
        let metadata = output.metadata()?;
        use std::os::unix::fs::OpenOptionsExt;
        let etag = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(rusty_dlna_transcode::cache_stamp_path(&metadata_job.dest))
            .ok()
            .and_then(|stamp| {
                let stamp_metadata = stamp
                    .metadata()
                    .ok()
                    .filter(|metadata| metadata.is_file())?;
                rusty_dlna_http::range::completed_cache_etag(&metadata, &stamp_metadata)
            });
        Ok::<_, std::io::Error>((metadata.len(), etag))
    })
    .await??;
    let requested_range = req.header("Range").filter(|_| {
        rusty_dlna_http::range::if_range_matches(req.header("If-Range"), etag.as_deref())
    });
    let range = match requested_range {
        None => None,
        Some(v) => match parse_byte_range(v, size) {
            Ok(r) => r,
            Err(RangeError::Invalid) => {
                tracing::error!(path = %req.path, range = v, "invalid Range");
                let err = HttpResponse::html(400, "Bad Request", "invalid range");
                write_remux_response(app, sock, err, head).await?;
                return Ok(());
            }
            Err(RangeError::Unsatisfiable) => {
                tracing::error!(path = %req.path, range = v, size, "range past remux EOF");
                let mut err =
                    HttpResponse::html(416, "Requested Range Not Satisfiable", "range past EOF");
                err.set("Content-Range", format!("bytes */{size}"));
                write_remux_response(app, sock, err, head).await?;
                return Ok(());
            }
        },
    };
    let (start, end) = match range {
        Some(r) => (r.start, r.end),
        None => (0, size.saturating_sub(1)),
    };
    let mut resp = media_response(rusty_dlna_http::MediaResponseOptions {
        server: &app.server,
        date: &now_imf_date(),
        mime,
        size,
        range,
        body: Vec::new(),
        pn: None,
        ci: 1,
    });
    resp.persist = false;
    if let Some(etag) = etag.as_deref() {
        resp.set("ETag", etag);
    }
    if head {
        write_remux_response(app, sock, resp, head).await?;
        return Ok(());
    }
    if !write_remux_response(app, sock, resp, head).await? {
        return Ok(());
    }
    stream_growing(app, sock, job, start, Some(end)).await?;
    Ok(())
}

async fn serve_growing(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    job: &Arc<RemuxJob>,
    mime: &str,
    head: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let open = match req.header("Range") {
        None => None,
        Some(v) => match parse_open_range(v) {
            Ok(p) => Some(p),
            Err(_) => {
                tracing::error!(path = %req.path, range = v, "invalid Range on growing remux");
                let err = HttpResponse::html(400, "Bad Request", "invalid range");
                write_remux_response(app, sock, err, head).await?;
                return Ok(());
            }
        },
    };
    let Some((start, requested_end)) = open else {
        return serve_open_growing(app, sock, job, mime, head).await;
    };
    if start == 0 && requested_end.is_none() {
        return serve_open_growing(app, sock, job, mime, head).await;
    }

    // Browsers can reconnect to a growing fMP4 with a nonzero open range after
    // parsing the initial fragments. A 200 response whose body begins at that
    // offset is invalid and some media engines abandon it immediately. Serve a
    // fixed snapshot as a real partial response while the producer continues.
    let small_probe = requested_end.is_some_and(|end| end.saturating_sub(start) < 2 * 1024 * 1024);
    let need = if small_probe {
        requested_end.unwrap_or(start).saturating_add(1)
    } else {
        start.saturating_add(1)
    };
    if let Err(err) = wait_offset(job, need).await {
        let cancelled = job.web && err == REMUX_CANCELLED;
        if cancelled {
            tracing::debug!(
                id = job.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "superseded compatible media range request cancelled"
            );
        } else {
            tracing::error!(id = %job.dest.display(), "{err}");
        }
        let resp = if cancelled {
            crate::web_ui::transcode_stream_error(409, "transcode_cancelled")
        } else {
            HttpResponse::html(
                500,
                "Internal Server Error",
                "compatible media range is unavailable",
            )
        };
        write_remux_response(app, sock, resp, head).await?;
        return Ok(());
    }
    let have = current_len_async(job).await?;
    if start >= have {
        let mut resp = HttpResponse::html(
            416,
            "Requested Range Not Satisfiable",
            "range past remux output",
        );
        resp.set("Content-Range", format!("bytes */{have}"));
        write_remux_response(app, sock, resp, head).await?;
        return Ok(());
    }
    let end = requested_end.unwrap_or(have - 1).min(have - 1);
    let mut resp = live_transcode_response(mime);
    resp.status = 206;
    resp.reason = "Partial Content".into();
    resp.set("Content-Range", format!("bytes {start}-{end}/*"));
    resp.set(
        "Content-Length",
        end.saturating_sub(start).saturating_add(1),
    );
    let valid_wire = write_remux_response(app, sock, resp, head).await?;
    if valid_wire && !head {
        stream_growing(app, sock, job, start, Some(end)).await?;
    }
    Ok(())
}

async fn serve_open_growing(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    job: &Arc<RemuxJob>,
    mime: &str,
    head: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resp = live_transcode_response(mime);
    let valid_wire = write_remux_response(app, sock, resp, head).await?;
    if head || !valid_wire {
        return Ok(());
    }
    stream_growing(app, sock, job, 0, None).await
}

fn current_len(job: &RemuxJob) -> u64 {
    let output = crate::lock_recover(&job.output).clone();
    if let Some(output) = output {
        return output.metadata().map(|m| m.len()).unwrap_or(0);
    }
    if job.dest.is_file() {
        return job.dest.metadata().map(|m| m.len()).unwrap_or(0);
    }
    job.part.metadata().map(|m| m.len()).unwrap_or(0)
}

fn current_path(job: &RemuxJob) -> PathBuf {
    if job.dest.is_file() {
        job.dest.clone()
    } else {
        job.part.clone()
    }
}

async fn current_len_async(job: &Arc<RemuxJob>) -> Result<u64, String> {
    let job = job.clone();
    tokio::task::spawn_blocking(move || current_len(&job))
        .await
        .map_err(|error| format!("remux metadata task: {error}"))
}

async fn wait_offset(job: &Arc<RemuxJob>, need: u64) -> Result<(), String> {
    let deadline = Instant::now() + FIRST_WAIT;
    loop {
        let notified = job.changed.notified();
        let state = job.state();
        if let RemuxState::Failed(error) = state {
            return Err(error);
        }
        if state == RemuxState::Cancelled {
            return Err(REMUX_CANCELLED.into());
        }
        let len = current_len_async(job).await?;
        if len >= need || state == RemuxState::Complete && len > 0 {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
            return Err(format!("remux offset {need} not reached"));
        }
    }
}

async fn stream_growing(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    job: &Arc<RemuxJob>,
    start: u64,
    end: Option<u64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let open_job = job.clone();
    let output = match tokio::task::spawn_blocking(move || open_job.open_output()).await? {
        Ok(file) => file,
        Err(_) if job.web && job.cancelled.load(Ordering::Acquire) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut pos = start;
    let mut buf = vec![0u8; positional::READ_BYTES];
    let mut sent = 0u64;
    loop {
        let notified = job.changed.notified();
        if end.is_some_and(|end| pos > end) {
            break;
        }
        if let Some(err) = job.err().or_else(|| {
            job.cancelled
                .load(Ordering::Acquire)
                .then(|| REMUX_CANCELLED.into())
        }) {
            if sent == 0 && !(job.web && err == REMUX_CANCELLED) {
                return Err(err.into());
            }
            return Ok(());
        }
        let complete = job.is_complete();
        let read_output = output.clone();
        let (returned, got, size) = tokio::task::spawn_blocking(move || {
            // Keep the first read small for first-byte latency, then amortize
            // blocking-pool scheduling with one bounded, backpressured batch.
            let limit = if sent == 0 { 64 * 1024 } else { buf.len() };
            let got = positional::read_chunk(&read_output, &mut buf[..limit], pos, end)?;
            // A positioned read observes growth directly. Metadata is needed
            // only at EOF to distinguish a truncated inode from producer lag.
            let size = if got == 0 {
                read_output.metadata()?.len()
            } else {
                0
            };
            Ok::<_, std::io::Error>((buf, got, size))
        })
        .await??;
        buf = returned;
        if got > 0 {
            let write = crate::socket_write_all(app, sock, &buf[..got]);
            let stopped = async {
                loop {
                    let changed = job.changed.notified();
                    if let Some(error) = job.err().or_else(|| {
                        job.cancelled
                            .load(Ordering::Acquire)
                            .then(|| REMUX_CANCELLED.into())
                    }) {
                        return error;
                    }
                    changed.await;
                }
            };
            let result = tokio::select! {
                result = write => result,
                error = stopped => {
                    if sent == 0 && !(job.web && error == REMUX_CANCELLED) {
                        return Err(error.into());
                    }
                    return Ok(());
                }
            };
            if let Err(error) = result {
                if sent == 0 {
                    return Err(error.into());
                }
                return Ok(());
            }
            pos = pos.saturating_add(got as u64);
            sent = sent.saturating_add(got as u64);
            continue;
        }
        if pos < size {
            // Growth can race a zero-byte pread and the following metadata
            // check. Retry against the same inode instead of calling that EOF.
            continue;
        }
        if pos > size || complete && end.is_some_and(|end| pos <= end) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "remux output ended before its promised range",
            )
            .into());
        }
        if complete {
            break;
        }
        if job.is_complete() {
            // Publication may have raced this read's size snapshot. Read the
            // pinned descriptor once more before accepting complete EOF.
            continue;
        }
        notified.await;
    }
    Ok(())
}

#[cfg(test)]
mod control_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct TraceCapture(Arc<Mutex<Vec<u8>>>);

    struct TraceCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for TraceCaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            crate::lock_recover(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for TraceCapture {
        type Writer = TraceCaptureWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            TraceCaptureWriter(self.0.clone())
        }
    }

    impl TraceCapture {
        fn text(&self) -> String {
            String::from_utf8(crate::lock_recover(&self.0).clone()).unwrap()
        }
    }

    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(1);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rusty-remux-{label}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create remux test directory");
            Self(path)
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = PathBuf;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) fn temp_dir(label: &str) -> TempDir {
        TempDir::new(label)
    }

    pub(super) fn test_app(dir: &Path, max_jobs: u32) -> Arc<App> {
        Arc::new(App::from_config(
            crate::Config {
                cache_dir: Some(dir.display().to_string()),
                transcode: crate::TranscodeCfg {
                    enable: true,
                    encoder: "libx264".into(),
                    max_jobs,
                    ..crate::TranscodeCfg::default()
                },
                rescan_secs: 0,
                ..crate::Config::default()
            },
            18200,
            11900,
            dir,
        ))
    }

    pub(super) fn job_spec(dir: &Path, key: &str, command: Vec<String>) -> RemuxJobSpec {
        let src = dir.join("source.mkv");
        if !src.exists() {
            std::fs::write(&src, b"source bytes").unwrap();
        }
        RemuxJobSpec {
            output_expectation: None,
            detail_id: 42,
            web_session_id: None,
            web_request_id: None,
            mime: "video/mp4",
            job_key: format!("42:{key}:{command:?}"),
            cache_key: key.into(),
            src,
            source_file: None,
            ai_upscale_shader_file: None,
            dest: dir.join(format!("{key}.mp4")),
            args: command.into_iter().map(Into::into).collect(),
            hardware_fallback_args: None,
            fallback_args: None,
            continue_after_disconnect: true,
            cacheable: true,
            hls_all_fragments_independent: false,
            remux_p8: false,
            verified_ffmpeg: None,
            profile8_toolchain: None,
            audio_index: 0,
            audio: RemuxAudio::Copy,
        }
    }

    fn attach_started_long_running_job(app: Arc<App>, mut spec: RemuxJobSpec) -> Arc<RemuxJob> {
        let part = cache_part(&spec.dest);
        let command = format!(
            "dd if=/dev/zero of=\"$1\" bs={FIRST_BYTES} count=1 2>/dev/null; exec sleep 30"
        );
        spec.args = vec![
            "sh".into(),
            "-c".into(),
            command.into(),
            "rustydlna-job".into(),
            part.into_os_string(),
        ];
        let job = attach_for_client(app, spec).unwrap();
        wait_until(Duration::from_secs(10), || {
            matches!(job.state(), RemuxState::Growing)
        });
        job
    }

    pub(super) fn wait_for_terminal_cleanup(app: &App, job: &RemuxJob) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(
                job.state(),
                RemuxState::Complete | RemuxState::Failed(_) | RemuxState::Cancelled
            ) && app.remuxes.lock().unwrap().is_empty()
                && app.jobs.in_use() == 0
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "job did not clean up: state={:?} map={} permits={}",
            job.state(),
            app.remuxes.lock().unwrap().len(),
            app.jobs.in_use()
        );
    }

    #[test]
    fn ai_upscale_has_an_independent_realtime_job_gate() {
        let dir = temp_dir("ai-upscale-gate");
        let app = test_app(&dir, 2);
        let _held_ai_job = app.ai_upscale_jobs.try_acquire().unwrap();
        let mut spec = job_spec(&dir, "ai-upscale-gate", vec!["must-not-spawn".into()]);
        spec.ai_upscale_shader_file = Some(Arc::new(std::fs::File::open(&spec.src).unwrap()));

        let error = match attach_for_client(app.clone(), spec) {
            Ok(_) => panic!("a second AI producer bypassed its realtime gate"),
            Err(error) => error,
        };
        assert!(error.contains("AI upscale busy"), "{error}");
        assert_eq!(app.jobs.in_use(), 0);
        assert_eq!(app.ai_upscale_jobs.in_use(), 1);
    }

    pub(super) fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !condition() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(condition(), "condition was not met within {timeout:?}");
    }

    fn completed_ephemeral_job(app: &Arc<App>, dir: &Path, id: i64) -> (String, Arc<RemuxJob>) {
        let key = format!("web:{id}:ephemeral");
        let dest = dir.join(format!("{id}-web-{}.mp4", "a".repeat(64)));
        std::fs::write(&dest, format!("ephemeral output {id}")).unwrap();
        let job = Arc::new(RemuxJob {
            detail_id: id,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: true,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(true),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest,
            part: dir.join(format!("{id}.mp4.part")),
            state: Mutex::new(RemuxState::Complete),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(true),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: false,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        });
        crate::lock_recover(&app.remuxes).insert(key.clone(), job.clone());
        (key, job)
    }

    pub(super) fn growing_test_job(dir: &Path, id: i64, bytes: &[u8]) -> Arc<RemuxJob> {
        let part = dir.join(format!("growing-{id}.mp4.part"));
        std::fs::write(&part, bytes).unwrap();
        Arc::new(RemuxJob {
            detail_id: id,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest: dir.join(format!("growing-{id}.mp4")),
            part,
            state: Mutex::new(RemuxState::Growing),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        })
    }

    async fn growing_wire(app: Arc<App>, job: Arc<RemuxJob>, request: &str, head: bool) -> Vec<u8> {
        use tokio::io::AsyncReadExt;

        let request = HttpRequest::parse_headers(request).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_growing(&app, &mut socket, &request, &job, "video/mp4", head)
                .await
                .unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap();
        bytes
    }

    #[test]
    fn cached_web_job_owners_follow_session_eviction_and_expiration() {
        let dir = temp_dir("web-owner-bound");
        let app = test_app(&dir, 1);
        let (key, job) = completed_ephemeral_job(&app, &dir, 42);
        let mut spec = job_spec(&dir, "unused", vec![]);
        spec.job_key = key;
        for id in 1..=(MAX_WEB_PLAYBACK_SESSIONS as u64 * 2 + 2) {
            spec.web_session_id = Some(id);
            spec.web_request_id = Some(id);
            assert!(matches!(
                attach_job_attempt(app.clone(), &spec, false).unwrap(),
                RemuxAttachment::Ready(_)
            ));
            assert!(crate::lock_recover(&job.web_request_ids).len() <= MAX_WEB_PLAYBACK_SESSIONS);
            assert!(crate::lock_recover(&job.web_sessions).len() <= MAX_WEB_PLAYBACK_SESSIONS);
        }
        assert!(!job.owns_web_request(None, 1));
        assert!(!cancel_web_request(&app, 42, None, 1));
        let newest = spec.web_request_id.unwrap();
        assert!(keep_web_request_alive(&app, 42, Some(newest), newest));
        {
            let mut sessions = crate::lock_recover(&app.web_playback_sessions);
            for (id, state) in sessions.iter_mut() {
                if *id != newest {
                    state.at = Instant::now() - WEB_SESSION_RETENTION;
                }
            }
        }
        assert!(keep_web_request_alive(&app, 42, Some(newest), newest));
        assert_eq!(crate::lock_recover(&job.web_request_ids).len(), 1);
        assert_eq!(crate::lock_recover(&job.web_sessions).len(), 1);
        assert!(cancel_web_request(&app, 42, Some(newest), newest));
        assert!(!job.has_web_requests());
    }

    #[test]
    fn unscoped_web_job_owners_are_bounded_without_evicting_other_readers() {
        let dir = temp_dir("unscoped-owner-bound");
        let app = test_app(&dir, 1);
        let (_, job) = completed_ephemeral_job(&app, &dir, 42);
        for id in 0..MAX_WEB_PLAYBACK_SESSIONS as u64 {
            job.add_web_request(None, Some(id)).unwrap();
        }
        let extra = MAX_WEB_PLAYBACK_SESSIONS as u64;
        assert!(job.add_web_request(Some(extra), Some(extra)).is_err());
        assert!(!job.owns_web_request(Some(extra), extra));
        job.add_web_request(None, Some(0)).unwrap();
        assert!(job.remove_web_request(None, 0));
        job.add_web_request(Some(extra), Some(extra)).unwrap();
        assert_eq!(
            job.add_web_request(Some(extra), Some(extra + 1)).unwrap(),
            Some(extra)
        );
        assert!(!job.owns_web_request(None, extra));
        assert!(job.owns_web_request(Some(extra), extra + 1));
        assert!(job.owns_web_request(None, 1));
        assert_eq!(
            crate::lock_recover(&job.web_request_ids).len(),
            MAX_WEB_PLAYBACK_SESSIONS
        );
    }

    #[test]
    fn mse_cursor_covers_eight_hour_history_and_rejects_beyond_index_budget() {
        for (cursor, accepted) in [
            (20_001, true),
            (28_800, true),
            (100_000, true),
            (100_001, false),
        ] {
            let request = HttpRequest::parse_headers(&format!(
                "GET /web/media/42.m3u8?delivery=mse&mse_after={cursor} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
            )).unwrap();
            assert_eq!(fragment_resource_uris(&request, "mse").is_ok(), accepted);
        }
    }

    #[test]
    fn hls_playlist_uses_fixed_resource_urls_and_strict_slices() {
        let request = HttpRequest::parse_headers(concat!(
            "GET /web/media/42.m3u8?mode=compatible&request=7&delivery=hls HTTP/1.1\r\n",
            "Host: 127.0.0.1\r\n\r\n"
        ))
        .unwrap();
        let (init, segment, cursor) = fragment_resource_uris(&request, "hls").unwrap();
        assert_eq!(cursor, 0);
        assert_eq!(
            init,
            "/web/media/42.mp4?mode=compatible&request=7&delivery=hls_init"
        );
        assert_eq!(
            segment,
            "/web/media/42.m4s?mode=compatible&request=7&delivery=hls_segment"
        );

        let mse_request = HttpRequest::parse_headers(concat!(
            "GET /web/media/42.m3u8?mode=compatible&request=7&delivery=mse&mse_after=17 HTTP/1.1\r\n",
            "Host: 127.0.0.1\r\n\r\n"
        ))
        .unwrap();
        let (mse_init, mse_segment, cursor) = fragment_resource_uris(&mse_request, "mse").unwrap();
        assert_eq!(cursor, 17);
        assert_eq!(
            mse_init,
            "/web/media/42.mp4?mode=compatible&request=7&delivery=mse_init"
        );
        assert_eq!(
            mse_segment,
            "/web/media/42.m4s?mode=compatible&request=7&delivery=mse_segment"
        );

        assert!(fragment_resource_uris(&mse_request, "hls").is_err());

        let resource = HttpRequest::parse_headers(concat!(
            "GET /web/media/42.m4s?delivery=hls_segment&hls_offset=1446&hls_length=4096 HTTP/1.1\r\n",
            "Host: 127.0.0.1\r\n\r\n"
        ))
        .unwrap();
        assert_eq!(hls_resource_slice(&resource).unwrap(), (1446, 4096));

        let duplicate = HttpRequest::parse_headers(concat!(
            "GET /web/media/42.m4s?hls_offset=0&hls_offset=1&hls_length=4 HTTP/1.1\r\n",
            "Host: 127.0.0.1\r\n\r\n"
        ))
        .unwrap();
        assert!(hls_resource_slice(&duplicate).is_err());
    }

    #[test]
    fn startup_phase_metrics_are_bounded_and_browser_events_are_generation_scoped() {
        let metric = AtomicDurationMetric::default();
        metric.record(Duration::from_millis(7));
        metric.record(Duration::from_millis(3));
        assert_eq!(
            metric.snapshot(),
            DurationMetric {
                count: 2,
                sum_ms: 10,
                max_ms: 7,
                buckets: [0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            }
        );

        let dir = temp_dir("canplay-metric");
        let app = test_app(&dir, 1);
        let (_key, job) = completed_ephemeral_job(&app, &dir, 42);
        crate::lock_recover(&job.web_request_ids).insert(77);
        crate::lock_recover(&job.web_sessions).insert(12, 77);

        for event in [
            WebStartupEvent::MsePlaylistReceived,
            WebStartupEvent::MseInitFetched,
            WebStartupEvent::MseInitAppended,
            WebStartupEvent::MseFirstFragmentFetched,
            WebStartupEvent::MseFirstFragmentAppended,
            WebStartupEvent::CanPlay,
            WebStartupEvent::Playing,
        ] {
            assert!(record_web_startup_event(&app, 42, 12, 77, event));
            assert!(record_web_startup_event(&app, 42, 12, 77, event));
            assert!(!record_web_startup_event(&app, 42, 12, 78, event));
        }
        let status = runtime_status(&app);
        assert_eq!(status.web_startup_mse_playlist_received.count, 1);
        assert_eq!(status.web_startup_mse_init_fetched.count, 1);
        assert_eq!(status.web_startup_mse_init_appended.count, 1);
        assert_eq!(status.web_startup_mse_first_fragment_fetched.count, 1);
        assert_eq!(status.web_startup_mse_first_fragment_appended.count, 1);
        assert_eq!(status.web_startup_canplay.count, 1);
        assert_eq!(status.web_startup_playing.count, 1);
    }

    #[test]
    fn browser_elapsed_reports_require_current_ownership_and_bounded_duration() {
        let dir = temp_dir("browser-elapsed-ownership");
        let app = test_app(&dir, 1);
        let (_, job) = completed_ephemeral_job(&app, &dir, 42);
        job.add_web_request(Some(12), Some(77)).unwrap();
        let metrics = &app.remux_metrics.performance;
        for key in [(42, 12, 77), (42, 12, 78), (42, 13, 77), (43, 12, 77)] {
            metrics.begin(key);
        }
        use performance::Stage;
        assert!(!record_browser_timing(
            &app,
            42,
            12,
            78,
            Stage::SelectionToFrame,
            10
        ));
        assert!(!record_browser_timing(
            &app,
            42,
            13,
            77,
            Stage::SelectionToFrame,
            10
        ));
        assert!(!record_browser_timing(
            &app,
            43,
            12,
            77,
            Stage::SelectionToFrame,
            10
        ));
        assert!(!record_browser_timing(
            &app,
            42,
            12,
            77,
            Stage::SelectionToFrame,
            120_001
        ));
        assert!(record_browser_timing(
            &app,
            42,
            12,
            77,
            Stage::SelectionToFrame,
            120_000
        ));
        assert!(record_browser_timing(
            &app,
            42,
            12,
            77,
            Stage::SelectionToFrame,
            10
        ));
        assert!(record_browser_timing(
            &app,
            42,
            12,
            77,
            Stage::SeekToFrame,
            10
        ));
        assert!(record_browser_timing(
            &app,
            42,
            12,
            77,
            Stage::SeekToFrame,
            20
        ));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["stages_ms"]["selection_to_frame"]["count"], 1);
        assert_eq!(
            snapshot["stages_ms"]["selection_to_frame"]["sum_ms"],
            120_000
        );
        assert_eq!(snapshot["stages_ms"]["seek_to_frame"]["count"], 2);
        job.cancel();
        assert!(!record_browser_timing(
            &app,
            42,
            12,
            77,
            Stage::SeekToFrame,
            30
        ));
        assert_eq!(metrics.snapshot()["stages_ms"]["seek_to_frame"]["count"], 2);
        crate::lock_recover(&app.remuxes).clear();
    }

    #[tokio::test]
    async fn first_fragment_timing_precedes_playlist_threshold_and_uses_request_owner() {
        use tokio::io::AsyncReadExt;
        let dir = temp_dir("first-fragment-observation");
        let app = test_app(&dir, 1);
        let mut bytes = hls::tests::fixture();
        // Shorten the selected track's sample duration to 250 ms, retaining
        // one complete fragment below the one-second playlist threshold.
        let trex = bytes.windows(4).position(|value| value == b"trex").unwrap();
        bytes[trex + 16..trex + 20].copy_from_slice(&250_u32.to_be_bytes());
        let mut end = 0;
        loop {
            let size = u32::from_be_bytes(bytes[end..end + 4].try_into().unwrap()) as usize;
            let media = &bytes[end + 4..end + 8] == b"mdat";
            end += size;
            if media {
                break;
            }
        }
        let job = growing_test_job(&dir, 42, &bytes[..end]);
        app.remux_metrics.performance.begin((42, 1, 10));
        app.remux_metrics.performance.begin((42, 2, 20));
        let request = HttpRequest::parse_headers("GET /web/media/42.m3u8?delivery=mse&session=2&request=20 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let served = job.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_fragment_playlist(&server_app, &mut socket, &request, &served, false, true)
                .await
                .unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = app.remux_metrics.performance.snapshot();
                if snapshot["stages_ms"]["preparation_to_first_complete_fragment"]["count"] == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("first complete fragment observation does not await a playable playlist");
        let snapshot = app.remux_metrics.performance.snapshot();
        assert!(
            snapshot["recent"][0]["stages_ms"]["preparation_to_first_complete_fragment"].is_null()
        );
        assert!(
            snapshot["recent"][1]["stages_ms"]["preparation_to_first_complete_fragment"].is_u64()
        );
        assert_eq!(runtime_status(&app).web_startup_playlist_ready.count, 0);
        job.cancel();
        job.transition(RemuxState::Cancelled);
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 409"));
    }

    #[test]
    fn playlist_timing_key_uses_current_request_and_rejects_duplicate_or_invalid_fields() {
        for (query, expected) in [
            ("session=12&request=78&delivery=mse", Some((42, 12, 78))),
            ("session=%31%32&request=78&delivery=mse", Some((42, 12, 78))),
            ("session=12&request=77&request=78", None),
            ("session=12&request=bad", None),
            ("request=78", None),
        ] {
            let req = HttpRequest::parse_headers(&format!(
                "GET /web/media/42.m3u8?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
            ))
            .unwrap();
            assert_eq!(crate::web_ui::playback_timing_key(&req, 42), expected);
        }
    }

    #[test]
    fn repeated_web_resources_reuse_the_active_job_without_recounting_the_generation() {
        let dir = temp_dir("web-resource-reattach");
        let app = test_app(&dir, 1);
        let (job_key, first) = completed_ephemeral_job(&app, &dir, 42);
        first.add_web_request(Some(9), Some(77)).unwrap();
        crate::lock_recover(&app.web_playback_sessions).insert(
            9,
            WebPlaybackSessionState {
                latest_request_id: 77,
                cancelled: false,
                cancelled_handoff: None,
                at: Instant::now(),
            },
        );
        let mut spec = job_spec(&dir, "web-resource-reattach", Vec::new());
        spec.job_key = job_key.clone();
        spec.web_session_id = Some(9);
        spec.web_request_id = Some(77);
        spec.dest = first.dest.clone();
        spec.cacheable = false;

        let first_metrics = runtime_status(&app);
        assert_eq!(first_metrics.web_requests_total, 0);
        assert_eq!(first_metrics.cache_maintenance_total, 0);
        assert_eq!(first_metrics.coalesced_requests_total, 0);
        assert_eq!(first_metrics.web_cache_reuses_total, 0);

        let traces = TraceCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(traces.clone())
            .finish();
        let (shared, clients) = tracing::subscriber::with_default(subscriber, || {
            let mut clients = Vec::new();
            // Native HLS can refresh its playlist and fetch fragments while
            // paused. Every resource carries the established generation IDs.
            for _ in 0..4 {
                let resource = attach_for_client(app.clone(), spec.clone()).unwrap();
                assert!(Arc::ptr_eq(&first, &resource));
                clients.push(resource);
            }

            // A different browser generation sharing the producer is a
            // genuine coalesced request and remains visible in reuse metrics.
            let mut second_generation = spec;
            second_generation.web_session_id = Some(10);
            second_generation.web_request_id = Some(88);
            let shared = attach_for_client(app.clone(), second_generation).unwrap();
            clients.push(shared.clone());
            (shared, clients)
        });
        assert!(Arc::ptr_eq(&first, &shared));
        let info_logs = traces.text();
        assert_eq!(
            info_logs
                .matches("web playback generation attached to existing remux")
                .count(),
            1,
            "{info_logs}"
        );
        assert!(!info_logs.contains("web media resource attached"));
        assert!(!info_logs.contains("remux attach"));

        let resource_metrics = runtime_status(&app);
        assert_eq!(resource_metrics.web_requests_total, 1);
        assert_eq!(resource_metrics.cache_maintenance_total, 0);
        assert_eq!(resource_metrics.coalesced_requests_total, 1);
        assert_eq!(resource_metrics.web_cache_reuses_total, 1);

        for client in clients {
            client.detach_client(
                app.clone(),
                job_key.clone(),
                true,
                Duration::ZERO,
                Duration::ZERO,
            );
        }
    }

    #[tokio::test]
    async fn growing_failure_keeps_internal_diagnostics_out_of_the_response() {
        let dir = temp_dir("growing-public-error");
        let app = test_app(&dir, 1);
        let job = growing_test_job(&dir, 71, b"fragment");
        *crate::lock_recover(&job.state) =
            RemuxState::Failed("ffmpeg: <script>alert(1)</script> /private/media/title.mkv".into());

        let wire = growing_wire(
            app,
            job,
            "GET /Transcode/71.mp4 HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=64-\r\n\r\n",
            false,
        )
        .await;
        let wire = String::from_utf8(wire).expect("HTTP response is UTF-8");

        assert!(wire.starts_with("HTTP/1.1 500 Internal Server Error\r\n"));
        assert!(wire.contains("compatible media range is unavailable"));
        assert!(!wire.contains("ffmpeg"));
        assert!(!wire.contains("<script>"));
        assert!(!wire.contains("/private/media"));
    }

    #[tokio::test]
    async fn cancelled_fragment_playlist_is_an_expected_conflict() {
        use tokio::io::AsyncReadExt;

        let dir = temp_dir("cancelled-fragment-playlist");
        let app = test_app(&dir, 1);
        let (_key, job) = completed_ephemeral_job(&app, &dir, 42);
        std::fs::remove_file(&job.dest).unwrap();
        job.transition(RemuxState::Cancelled);
        let request = HttpRequest::parse_headers(concat!(
            "GET /web/media/42.m3u8?mode=compatible&request=7&delivery=mse&mse_after=0 HTTP/1.1\r\n",
            "Host: 127.0.0.1\r\n",
            "User-Agent: Android regression test\r\n\r\n"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_fragment_playlist(&server_app, &mut socket, &request, &job, false, true).await
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await.unwrap();
        server
            .await
            .unwrap()
            .expect("a superseded playlist request is handled");

        let wire = String::from_utf8(wire).expect("HTTP response is UTF-8");
        assert!(wire.starts_with("HTTP/1.1 409 Conflict\r\n"), "{wire}");
        assert!(wire.contains("\"code\":\"transcode_cancelled\""), "{wire}");
        assert_eq!(runtime_status(&app).web_failures_producer_total, 0);
    }

    #[test]
    fn cache_hits_coalesce_and_remain_eviction_protected_until_the_last_client() {
        let dir = temp_dir("active-cache-hit");
        let app = Arc::new(App::from_config(
            crate::Config {
                cache_dir: Some(dir.display().to_string()),
                transcode: crate::TranscodeCfg {
                    enable: true,
                    cache_max_mb: 1,
                    ..crate::TranscodeCfg::default()
                },
                rescan_secs: 0,
                ..crate::Config::default()
            },
            18200,
            11900,
            &dir,
        ));
        let cache_key = "a".repeat(64);
        let dest = dir.join(format!("42-hdr10-{cache_key}.mp4"));
        std::fs::write(&dest, vec![1u8; 600_000]).unwrap();
        write_cache_stamp_for_key(&dest, &cache_key).unwrap();
        let mut spec = job_spec(&dir, &cache_key, Vec::new());
        spec.dest = dest.clone();
        spec.cache_key = cache_key;
        let job_key = spec.job_key.clone();

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let app = app.clone();
            let spec = spec.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                attach_for_client(app, spec).unwrap()
            }));
        }
        let jobs = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert!(jobs
            .iter()
            .all(|job| Arc::ptr_eq(job, jobs.first().unwrap())));
        assert_eq!(crate::lock_recover(&app.remuxes).len(), 1);
        let metrics = runtime_status(&app);
        assert_eq!(metrics.cache_hits_total, 1);
        assert_eq!(metrics.coalesced_requests_total, 7);

        let victim = dir.join(format!("43-hdr10-{}.mp4", "b".repeat(64)));
        std::fs::write(&victim, vec![2u8; 600_000]).unwrap();
        // Simulate the reconciliation cadence after this external fixture write.
        maintain_app_cache(&app, &HashSet::new(), true).unwrap();
        assert!(dest.exists(), "active cache hit must remain protected");
        assert!(!victim.exists());

        for job in &jobs[..jobs.len() - 1] {
            job.detach_client(
                app.clone(),
                job_key.clone(),
                true,
                Duration::ZERO,
                Duration::ZERO,
            );
        }
        assert_eq!(crate::lock_recover(&app.remuxes).len(), 1);
        jobs.last().unwrap().detach_client(
            app.clone(),
            job_key,
            true,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert!(crate::lock_recover(&app.remuxes).is_empty());
        assert!(dest.exists(), "detach must not shorten cache lifetime");
        assert!(rusty_dlna_transcode::cache_stamp_path(&dest).exists());

        std::fs::File::options()
            .write(true)
            .open(rusty_dlna_transcode::cache_stamp_path(&dest))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();
        std::fs::write(&victim, vec![3u8; 600_000]).unwrap();
        // Simulate the reconciliation cadence after this external fixture write.
        maintain_app_cache(&app, &HashSet::new(), true).unwrap();
        assert!(!dest.exists(), "unprotected oldest output may be evicted");
        assert!(!rusty_dlna_transcode::cache_stamp_path(&dest).exists());
        assert!(victim.exists());
    }

    #[test]
    fn produced_cacheable_output_stays_protected_until_its_client_detaches() {
        let dir = temp_dir("active-produced-output");
        let app = Arc::new(App::from_config(
            crate::Config {
                cache_dir: Some(dir.display().to_string()),
                transcode: crate::TranscodeCfg {
                    enable: true,
                    cache_max_mb: 1,
                    ..crate::TranscodeCfg::default()
                },
                rescan_secs: 0,
                ..crate::Config::default()
            },
            18200,
            11900,
            &dir,
        ));
        let cache_key = "c".repeat(64);
        let dest =
            rusty_dlna_transcode::cache_dest_for_key(&dir, 42, RecodeAction::Hdr10, &cache_key);
        let part = cache_part(&dest);
        let source = dir.join("source.mkv");
        std::fs::write(&source, vec![1u8; 600_000]).unwrap();
        let mut spec = job_spec(
            &dir,
            &cache_key,
            vec![
                "cp".into(),
                source.display().to_string(),
                part.display().to_string(),
            ],
        );
        spec.job_key = "42:active-produced-output".into();
        spec.dest = dest.clone();
        spec.cache_key = cache_key;
        let job_key = spec.job_key.clone();

        let job = attach_for_client(app.clone(), spec).unwrap();
        wait_until(Duration::from_secs(2), || {
            job.is_complete() && app.jobs.in_use() == 0
        });
        assert!(!job.cache_hit);
        assert!(crate::lock_recover(&app.remuxes)
            .get(&job_key)
            .is_some_and(|current| Arc::ptr_eq(current, &job)));
        assert_eq!(std::fs::read(&dest).unwrap().len(), 600_000);

        let victim = rusty_dlna_transcode::cache_dest_for_key(
            &dir,
            43,
            RecodeAction::Hdr10,
            &"d".repeat(64),
        );
        std::fs::write(&victim, vec![2u8; 600_000]).unwrap();
        // Simulate the reconciliation cadence after this external fixture write.
        maintain_app_cache(&app, &HashSet::new(), true).unwrap();
        assert!(dest.exists(), "the actively served output was evicted");
        assert!(!victim.exists());

        job.detach_client(app.clone(), job_key, true, Duration::ZERO, Duration::ZERO);
        assert!(crate::lock_recover(&app.remuxes).is_empty());
        assert!(dest.exists(), "detach must not shorten cache lifetime");
        assert!(rusty_dlna_transcode::cache_stamp_path(&dest).exists());
    }

    #[test]
    fn remux_rename_failure_cleans_all_intermediates() {
        let dir = temp_dir("rename-cleanup");
        let app = test_app(&dir, 1);
        let dest = dir.join("destination-directory");
        std::fs::create_dir(&dest).unwrap();
        let part = dir.join("rename.mp4.part");
        for path in [
            part.clone(),
            part.with_extension("hevc"),
            part.with_extension("p8.hevc"),
            part.with_extension("p8.mp4"),
        ] {
            std::fs::write(path, b"staging").unwrap();
        }
        let job = Arc::new(RemuxJob {
            detail_id: 42,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest: dest.clone(),
            part: part.clone(),
            state: Mutex::new(RemuxState::Starting),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        });

        finalize_remux(
            &app,
            &job,
            &job_spec(&dir, "quota", Vec::new()),
            Duration::from_secs(1),
            &None,
            false,
        );

        let RemuxState::Failed(error) = job.state() else {
            panic!("rename failure must fail the job");
        };
        assert!(error.contains("remux rename: "));
        assert!(dest.is_dir());
        assert!(!part.exists());
        assert!(!part.with_extension("hevc").exists());
        assert!(!part.with_extension("p8.hevc").exists());
        assert!(!part.with_extension("p8.mp4").exists());
        assert_eq!(app.remux_metrics.cache_bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn completed_output_is_rejected_before_publish_when_it_exceeds_cache_limits() {
        let dir = temp_dir("pre-publish-cache-admission");
        let app = Arc::new(App::from_config(
            crate::Config {
                cache_dir: Some(dir.display().to_string()),
                transcode: crate::TranscodeCfg {
                    enable: true,
                    cache_max_mb: 1,
                    max_jobs: 1,
                    ..crate::TranscodeCfg::default()
                },
                rescan_secs: 0,
                ..crate::Config::default()
            },
            18200,
            11900,
            &dir,
        ));
        let dest = rusty_dlna_transcode::cache_dest_for_key(
            &dir,
            42,
            RecodeAction::Hdr10,
            &"a".repeat(64),
        );
        let part = cache_part(&dest);
        let mut spec = job_spec(
            &dir,
            "pre-publish-cache-admission",
            vec![
                "truncate".into(),
                "-s".into(),
                (2 * 1024 * 1024).to_string(),
                part.display().to_string(),
            ],
        );
        spec.dest = dest.clone();
        let job = attach_for_client(app.clone(), spec).unwrap();

        let error = tokio::time::timeout(Duration::from_secs(2), wait_ready(&job))
            .await
            .expect("cache admission must resolve the waiting client")
            .expect_err("over-quota output must never become ready");

        assert_eq!(
            error,
            "transcode cache limits: quota or minimum-free-space target cannot be satisfied"
        );
        wait_for_terminal_cleanup(&app, &job);
        assert!(!dest.exists());
        assert!(!part.exists());
        assert!(!rusty_dlna_transcode::cache_stamp_path(&dest).exists());
        assert_eq!(app.remux_metrics.cache_bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn ephemeral_cleanups_share_one_worker_and_restart_after_idle() {
        let dir = temp_dir("cleanup-worker-bound");
        let app = test_app(&dir, 1);
        let mut outputs = Vec::new();
        for id in 1..=24 {
            let (_, job) = completed_ephemeral_job(&app, &dir, id);
            outputs.push(job.dest.clone());
            schedule_ephemeral_cleanup(&app, &job, Duration::from_millis(100));
        }
        assert_eq!(app.ephemeral_cleanup.worker_starts(), 1);
        wait_until(Duration::from_secs(2), || {
            crate::lock_recover(&app.remuxes).is_empty() && app.ephemeral_cleanup.is_idle()
        });
        assert!(outputs.iter().all(|path| !path.exists()));

        let (_, restarted) = completed_ephemeral_job(&app, &dir, 25);
        schedule_ephemeral_cleanup(&app, &restarted, Duration::from_millis(20));
        wait_until(Duration::from_secs(2), || {
            !restarted.dest.exists() && app.ephemeral_cleanup.is_idle()
        });
        assert_eq!(app.ephemeral_cleanup.worker_starts(), 2);
    }

    #[test]
    fn cleanup_worker_wakes_for_an_earlier_deadline() {
        let dir = temp_dir("cleanup-worker-wake");
        let app = test_app(&dir, 1);
        let (_, later) = completed_ephemeral_job(&app, &dir, 1);
        schedule_ephemeral_cleanup(&app, &later, Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(10));
        let (_, earlier) = completed_ephemeral_job(&app, &dir, 2);
        schedule_ephemeral_cleanup(&app, &earlier, Duration::from_millis(20));

        wait_until(Duration::from_millis(200), || !earlier.dest.exists());
        assert!(later.dest.exists());
        assert_eq!(app.ephemeral_cleanup.worker_starts(), 1);
        wait_until(Duration::from_secs(1), || {
            !later.dest.exists() && app.ephemeral_cleanup.is_idle()
        });
    }

    #[test]
    fn cleanup_spawn_failure_removes_output_stamp_and_registry_entry() {
        let dir = temp_dir("cleanup-spawn-failure");
        let app = test_app(&dir, 1);
        let (_, job) = completed_ephemeral_job(&app, &dir, 1);
        let stamp = rusty_dlna_transcode::cache_stamp_path(&job.dest);
        std::fs::write(&stamp, b"stamp").unwrap();
        app.ephemeral_cleanup.fail_next_spawn();

        schedule_ephemeral_cleanup(&app, &job, WEB_EPHEMERAL_RETENTION);

        assert!(!job.dest.exists());
        assert!(!stamp.exists());
        assert!(crate::lock_recover(&app.remuxes).is_empty());
        assert!(app.ephemeral_cleanup.is_idle());
        assert_eq!(app.ephemeral_cleanup.worker_starts(), 0);
    }

    #[test]
    fn cleanup_scheduler_recovers_poison_and_does_not_keep_app_alive() {
        let dir = temp_dir("cleanup-poison-lifecycle");
        let app = test_app(&dir, 1);
        let scheduler = app.ephemeral_cleanup.clone();
        let poisoned = scheduler.clone();
        let result = std::panic::catch_unwind(move || {
            let _state = poisoned.state.lock().unwrap();
            panic!("poison scheduler state");
        });
        assert!(result.is_err());

        let (_, job) = completed_ephemeral_job(&app, &dir, 1);
        schedule_ephemeral_cleanup(&app, &job, Duration::from_secs(30));
        assert_eq!(scheduler.worker_starts(), 1);
        let weak = Arc::downgrade(&app);
        drop(job);
        drop(app);
        wait_until(Duration::from_secs(1), || weak.upgrade().is_none());
        wait_until(Duration::from_secs(1), || scheduler.is_idle());
    }

    #[test]
    fn app_drop_force_sweeps_pending_ephemeral_output_without_retaining_app() {
        let dir = temp_dir("cleanup-app-drop");
        let app = test_app(&dir, 1);
        let (_, job) = completed_ephemeral_job(&app, &dir, 1);
        let dest = job.dest.clone();
        let stamp = rusty_dlna_transcode::cache_stamp_path(&dest);
        std::fs::write(&stamp, b"stamp").unwrap();
        schedule_ephemeral_cleanup(&app, &job, WEB_EPHEMERAL_RETENTION);
        let app_weak = Arc::downgrade(&app);
        let job_weak = Arc::downgrade(&job);
        let scheduler = app.ephemeral_cleanup.clone();
        drop(job);

        drop(app);

        wait_until(Duration::from_secs(1), || app_weak.upgrade().is_none());
        wait_until(Duration::from_secs(1), || scheduler.is_idle());
        assert!(!dest.exists());
        assert!(!stamp.exists());
        assert!(
            job_weak.upgrade().is_none(),
            "App shutdown retained the pending registry job"
        );
    }

    fn cache_pressure_p8_stage(
        part: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        observer: &mut dyn FnMut(RemuxP8StageEvent) -> Result<(), String>,
    ) -> Result<(), RemuxP8Error> {
        use rusty_dlna_helper::{SupervisedCommand, SupervisedOutcome};
        use std::ops::ControlFlow;

        let p8 = part.with_extension("p8.hevc");
        let leader = part.with_extension("p8-stage.pid");
        let descendant = part.with_extension("p8-stage-child.pid");
        let script = format!(
            "echo $$ > '{}'; dd if=/dev/zero of='{}' bs=1048576 count=2 2>/dev/null; trap '' TERM; sleep 30 & echo $! > '{}'; wait",
            leader.display(),
            p8.display(),
            descendant.display()
        );
        let mut command = std::process::Command::new("sh");
        command.args(["-c", &script]);
        enum Stop {
            Cancelled,
            Deadline,
            Observer(String),
        }
        let outcome = SupervisedCommand::new(&mut command)
            .run_until(deadline, Duration::from_millis(50), || {
                if cancelled.load(Ordering::Acquire) {
                    return ControlFlow::Break(Stop::Cancelled);
                }
                if Instant::now() >= deadline {
                    return ControlFlow::Break(Stop::Deadline);
                }
                match observer(RemuxP8StageEvent {
                    stage: RemuxP8Stage::Conversion,
                    status: RemuxP8StageStatus::Progress,
                    elapsed: Duration::ZERO,
                    input_bytes: None,
                    output_bytes: None,
                    io: None,
                }) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(error) => ControlFlow::Break(Stop::Observer(error)),
                }
            })
            .map_err(|error| RemuxP8Error::Pipeline(error.to_string()))?;
        match outcome {
            SupervisedOutcome::NotStarted {
                reason: Stop::Cancelled,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Cancelled,
                ..
            } => Err(RemuxP8Error::Cancelled("test P8 stage cancelled".into())),
            SupervisedOutcome::NotStarted {
                reason: Stop::Deadline,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Deadline,
                ..
            }
            | SupervisedOutcome::Deadline { .. } => {
                Err(RemuxP8Error::Deadline("test P8 stage timed out".into()))
            }
            SupervisedOutcome::NotStarted {
                reason: Stop::Observer(error),
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Observer(error),
                ..
            } => Err(RemuxP8Error::Observer(error)),
            SupervisedOutcome::Exited(output) => Err(RemuxP8Error::Pipeline(format!(
                "test P8 stage exited unexpectedly: {}",
                output.status
            ))),
        }
    }

    #[test]
    fn cache_cleanup_accounting_never_wraps_below_zero() {
        let metrics = RemuxMetrics::default();
        metrics.subtract_cache_bytes(1);
        assert_eq!(metrics.cache_bytes.load(Ordering::Relaxed), 0);

        metrics.cache_bytes.store(10, Ordering::Relaxed);
        metrics.subtract_cache_bytes(20);
        assert_eq!(metrics.cache_bytes.load(Ordering::Relaxed), 0);

        metrics.cache_bytes.store(20, Ordering::Relaxed);
        metrics.subtract_cache_bytes(5);
        assert_eq!(metrics.cache_bytes.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn failed_pressure_pass_reports_partial_eviction_and_protected_bytes() {
        let dir = temp_dir("partial-pressure-accounting");
        let mut config = crate::Config {
            cache_dir: Some(dir.display().to_string()),
            rescan_secs: 0,
            ..crate::Config::default()
        };
        config.transcode.enable = true;
        config.transcode.cache_max_mb = 1;
        let app = Arc::new(App::from_config(config, 18200, 11900, &dir));
        let protected_dest = rusty_dlna_transcode::cache_dest_for_key(
            &dir,
            1,
            RecodeAction::RemuxP8,
            &"a".repeat(64),
        );
        let protected_part = cache_part(&protected_dest);
        let protected_bytes = 1_200_000u64;
        std::fs::write(&protected_part, vec![0u8; protected_bytes as usize]).unwrap();
        let job = Arc::new(RemuxJob {
            detail_id: 1,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest: protected_dest,
            part: protected_part.clone(),
            state: Mutex::new(RemuxState::Preprocessing),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        });
        crate::lock_recover(&app.remuxes).insert("protected".into(), job);
        let protected = cache::active_artifacts(crate::lock_recover(&app.remuxes).values());
        assert_eq!(protected.len(), 5);
        assert!(protected.contains(&protected_part.with_extension("hevc")));
        assert!(protected.contains(&protected_part.with_extension("p8.hevc")));
        assert!(protected.contains(&protected_part.with_extension("p8.mp4")));

        let victim =
            rusty_dlna_transcode::cache_dest_for_key(&dir, 2, RecodeAction::Hdr10, &"b".repeat(64));
        let victim_bytes = 600_000u64;
        std::fs::write(&victim, vec![1u8; victim_bytes as usize]).unwrap();
        let victim_stamp = rusty_dlna_transcode::cache_stamp_path(&victim);
        std::fs::write(&victim_stamp, b"stamp").unwrap();

        let error = enforce_active_cache_limits(&app).unwrap_err();

        assert_eq!(
            error.to_string(),
            "quota or minimum-free-space target cannot be satisfied"
        );
        assert!(!victim.exists());
        assert!(!victim_stamp.exists());
        assert!(protected_part.exists());
        let metrics = runtime_status(&app);
        assert_eq!(metrics.cache_bytes, protected_bytes);
        assert_eq!(metrics.cache_evicted_files_total, 1);
        assert_eq!(metrics.cache_evicted_bytes_total, victim_bytes);
        assert_eq!(metrics.cache_maintenance_failures_total, 1);
        crate::lock_recover(&app.remuxes).clear();
    }

    #[test]
    fn concurrent_cache_maintenance_serializes_eviction_accounting() {
        let dir = temp_dir("concurrent-cache-maintenance");
        let mut config = crate::Config {
            cache_dir: Some(dir.display().to_string()),
            rescan_secs: 0,
            ..crate::Config::default()
        };
        config.transcode.enable = true;
        config.transcode.cache_max_mb = 1;
        let app = Arc::new(App::from_config(config, 18200, 11900, &dir));
        const FILES: u64 = 32;
        const BYTES: u64 = 64 * 1024;
        const EVICTED: u64 = 16;
        for id in 0..FILES {
            let output = rusty_dlna_transcode::cache_dest_for_key(
                &dir,
                id as i64,
                RecodeAction::Hdr10,
                &format!("{id:064x}"),
            );
            std::fs::write(&output, vec![0u8; BYTES as usize]).unwrap();
            std::fs::write(rusty_dlna_transcode::cache_stamp_path(&output), b"stamp").unwrap();
        }
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let app = app.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    maintain_app_cache(&app, &HashSet::new(), false)
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            assert_eq!(worker.join().unwrap().unwrap(), 1024 * 1024);
        }
        let metrics = runtime_status(&app);
        assert_eq!(metrics.cache_maintenance_total, 8);
        assert_eq!(metrics.cache_maintenance_failures_total, 0);
        assert_eq!(metrics.cache_evicted_files_total, EVICTED);
        assert_eq!(metrics.cache_evicted_bytes_total, EVICTED * BYTES);
        assert_eq!(metrics.cache_bytes, 1024 * 1024);
    }

    #[test]
    fn completion_guard_cleans_up_registry_permits_and_metrics_on_panic() {
        let dir = temp_dir("worker-panic");
        let app = test_app(&dir, 1);
        let key = "worker-panic".to_string();
        let dest = dir.join("worker-panic.mp4");
        let part = cache_part(&dest);
        let p8_mp4 = part.with_extension("p8.mp4");
        std::fs::write(&part, b"partial output").unwrap();
        std::fs::write(&p8_mp4, b"partial p8 wrapper").unwrap();
        let job = Arc::new(RemuxJob {
            detail_id: 42,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest,
            part: part.clone(),
            state: Mutex::new(RemuxState::Starting),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        });
        crate::lock_recover(&app.remuxes).insert(key.clone(), job.clone());

        let worker_app = app.clone();
        let worker_job = job.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _helper_permit = worker_app.helpers.try_acquire().unwrap();
            let _job_permit = worker_app.jobs.try_acquire().unwrap();
            let _completion_guard = RemuxCompletionGuard::new(worker_app, key, worker_job);
            panic!("simulated remux worker panic");
        }));

        assert!(result.is_err());
        assert_eq!(
            job.state(),
            RemuxState::Failed("remux worker panicked".into())
        );
        assert!(crate::lock_recover(&app.remuxes).is_empty());
        assert_eq!(app.jobs.in_use(), 0);
        assert_eq!(app.helpers.metrics().active, 0);
        assert_eq!(runtime_status(&app).failed_total, 1);
        assert!(!part.exists());
        assert!(!p8_mp4.exists());
    }

    #[test]
    fn runtime_deadline_precedes_cache_pressure_before_spawn() {
        let dir = temp_dir("deadline-precedence");
        let mut config = crate::Config {
            cache_dir: Some(dir.display().to_string()),
            rescan_secs: 0,
            ..crate::Config::default()
        };
        config.transcode.enable = true;
        config.transcode.cache_max_mb = 1;
        let app = Arc::new(App::from_config(config, 18200, 11900, &dir));
        let key = "deadline-precedence".to_string();
        let dest = dir.join(format!("42-web-{}.mp4", "a".repeat(64)));
        let part = cache_part(&dest);
        std::fs::write(&part, vec![0u8; 1024 * 1024 + 1]).unwrap();
        let job = Arc::new(RemuxJob {
            detail_id: 42,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest,
            part,
            state: Mutex::new(RemuxState::Starting),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        });
        crate::lock_recover(&app.remuxes).insert(key.clone(), job.clone());
        assert!(enforce_active_cache_limits(&app).is_err());

        let args = vec![std::ffi::OsString::from("must-not-spawn")];
        let error =
            run_ffmpeg_growing(&args, None, None, None, &job, Instant::now(), &app).unwrap_err();

        assert_eq!(error, "transcode runtime exceeded configured deadline");

        let args = vec![std::ffi::OsString::from("ffmpeg")];
        let error = run_ffmpeg_growing(
            &args,
            None,
            None,
            None,
            &job,
            Instant::now() + Duration::from_secs(1),
            &app,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "production ffmpeg command is missing its verified executable"
        );
        remove_job(&app, &key, &job);
    }

    #[test]
    fn profile8_pipeline_requires_the_cache_identity_toolchain() {
        let dir = temp_dir("profile8-missing-toolchain");
        let mut spec = job_spec(&dir, "profile8-missing-toolchain", Vec::new());
        spec.remux_p8 = true;
        let plan = TranscodePlan {
            action: RecodeAction::RemuxP8,
            ..TranscodePlan::default()
        };
        let cancelled = AtomicBool::new(false);
        let mut observer = |_| Ok(());
        let error = run_profile8_pipeline(
            &spec,
            &cache_part(&spec.dest),
            &plan,
            Instant::now() + Duration::from_secs(1),
            &cancelled,
            &mut observer,
        )
        .unwrap_err();
        assert_eq!(
            error,
            RemuxP8Error::Pipeline("Profile-8 job is missing its toolchain snapshot".into())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn profile8_cache_pressure_stops_stage_and_cleans_job_state() {
        let dir = temp_dir("profile8-cache-pressure");
        let mut config = crate::Config {
            cache_dir: Some(dir.display().to_string()),
            rescan_secs: 0,
            ..crate::Config::default()
        };
        config.transcode.enable = true;
        config.transcode.cache_max_mb = 1;
        config.transcode.max_jobs = 1;
        let app = Arc::new(App::from_config(config, 18200, 11900, &dir));
        let cache_key = "a".repeat(64);
        let dest =
            rusty_dlna_transcode::cache_dest_for_key(&dir, 42, RecodeAction::RemuxP8, &cache_key);
        let part = cache_part(&dest);
        let mut spec = job_spec(&dir, "profile8-pressure", vec!["must-not-run".into()]);
        spec.dest = dest.clone();
        spec.cache_key = cache_key;
        spec.remux_p8 = true;
        crate::lock_recover(p8_test_runners()).insert(part.clone(), cache_pressure_p8_stage);

        let started = Instant::now();
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);

        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            job.state(),
            RemuxState::Failed(
                "transcode cache limits: quota or minimum-free-space target cannot be satisfied"
                    .into()
            )
        );
        for artifact in [
            dest.clone(),
            rusty_dlna_transcode::cache_stamp_path(&dest),
            part.clone(),
            part.with_extension("hevc"),
            part.with_extension("p8.hevc"),
            part.with_extension("p8.mp4"),
        ] {
            assert!(
                !artifact.exists(),
                "artifact survived: {}",
                artifact.display()
            );
        }
        for marker in [
            part.with_extension("p8-stage.pid"),
            part.with_extension("p8-stage-child.pid"),
        ] {
            let pid = std::fs::read_to_string(&marker).unwrap();
            let process = PathBuf::from(format!("/proc/{}", pid.trim()));
            let gone = Instant::now() + Duration::from_secs(2);
            while process.exists() && Instant::now() < gone {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                !process.exists(),
                "P8 stage process {} survived",
                pid.trim()
            );
        }
        let metrics = runtime_status(&app);
        assert_eq!(metrics.failed_total, 1);
        assert_eq!(metrics.cache_maintenance_failures_total, 1);
        assert_eq!(metrics.cache_bytes, 0);
        assert_eq!(app.helpers.metrics().active, 0);
        assert_eq!(app.jobs.in_use(), 0);
        assert!(crate::lock_recover(&app.remuxes).is_empty());
    }

    #[tokio::test]
    async fn wait_ready_is_not_timed_out_during_preprocessing() {
        let tmp = TempDir::new("wait-ready");
        let dest = tmp.join("out.mp4");
        let part = tmp.join("out.mp4.part");
        let job = Arc::new(RemuxJob {
            detail_id: 42,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest: dest.clone(),
            part,
            state: Mutex::new(RemuxState::Preprocessing),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        });
        assert!(!dest.exists());
        let writer = {
            let dest = dest.clone();
            let job = job.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(80)).await;
                std::fs::write(&dest, vec![0u8; FIRST_BYTES as usize]).unwrap();
                job.transition(RemuxState::Complete);
            })
        };
        let t0 = Instant::now();
        let got = tokio::time::timeout(Duration::from_secs(2), wait_ready(&job))
            .await
            .expect("wait_ready must return during silent prepass, not after FIRST_WAIT")
            .expect("wait_ready");
        assert_eq!(got, dest);
        assert!(t0.elapsed() < Duration::from_secs(2));
        writer.await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn wire_body(bytes: &[u8]) -> &[u8] {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTTP header terminator");
        &bytes[split + 4..]
    }

    fn compatible_connection_fixture() -> (Arc<App>, Arc<RemuxJob>, String, Vec<u8>) {
        let app = Arc::new(crate::tests::testdata_app());
        let id = crate::read_recover(&app.catalog)
            .items
            .values()
            .find(|item| item.path.ends_with("tagged.mp4"))
            .expect("tagged video fixture")
            .detail_id;
        let url = format!(
            "/web/media/{id}.mp4?mode=compatible&quality=auto&video_mode=copy&audio_mode=copy&session=41&request=42"
        );
        let request = HttpRequest::parse_headers(&format!(
            "GET {url} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n"
        ))
        .unwrap();
        let spec = app.handle(&request).remux_job.expect("compatible job");
        // Register deterministic output under the real route's job identity;
        // HTTP requests still pass through parsing, routing and remux admission.
        let payload: Vec<u8> = (0..FIRST_BYTES).map(|index| (index % 251) as u8).collect();
        let mut job = growing_test_job(&app.cache_dir, id, &payload);
        let fixture = Arc::get_mut(&mut job).unwrap();
        let part = cache_part(&spec.dest);
        std::fs::rename(&fixture.part, &part).unwrap();
        fixture.part = part;
        fixture.dest = spec.dest.clone();
        fixture.web = true;
        fixture.web_spec = Some(spec.clone());
        fixture
            .add_web_request(spec.web_session_id, spec.web_request_id)
            .unwrap();
        crate::lock_recover(&app.remuxes).insert(spec.job_key, job.clone());
        (app, job, url, payload)
    }

    async fn compatible_connection_wire(
        app: &Arc<App>,
        url: &str,
        method: &str,
        range: Option<&str>,
    ) -> Vec<u8> {
        let range = range.map_or_else(String::new, |range| format!("Range: {range}\r\n"));
        let request = format!(
            "{method} {url} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n{range}\r\n"
        );
        crate::tests::raw_connection(app.clone(), request.as_bytes(), false).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progressive_download_serves_growing_ranges_with_one_validator_and_final_total() {
        let (app, job, url, mut payload) = compatible_connection_fixture();
        let request = |headers: &str| {
            format!(
            "GET {url} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\nX-RustyDLNA-Download: progressive\r\n{headers}\r\n"
        )
        };
        let first = crate::tests::raw_connection(
            app.clone(),
            request("Range: bytes=0-8191\r\n").as_bytes(),
            false,
        )
        .await;
        assert_eq!(wire_body(&first), &payload[..8192]);
        let headers = String::from_utf8_lossy(&first[..first.len() - 8192]);
        assert!(headers.contains("Content-Range: bytes 0-8191/*\r\n"));
        assert_eq!(job.state(), RemuxState::Growing);
        assert!(crate::lock_recover(&job.output).is_some());
        assert!(crate::lock_recover(&job.disconnect_deadline).is_none());
        let etag = headers
            .lines()
            .find_map(|line| line.strip_prefix("ETag: "))
            .unwrap()
            .to_owned();
        let extra = vec![29_u8; FIRST_BYTES as usize];
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&job.part)
            .unwrap()
            .write_all(&extra)
            .unwrap();
        payload.extend(extra);
        let middle = crate::tests::raw_connection(
            app.clone(),
            request(&format!("Range: bytes=8192-20000\r\nIf-Match: {etag}\r\n")).as_bytes(),
            false,
        )
        .await;
        assert_eq!(wire_body(&middle), &payload[8192..20001]);
        let headers = String::from_utf8_lossy(&middle[..middle.len() - 11809]);
        assert!(headers.contains(&format!("ETag: {etag}\r\n")));
        assert!(headers.contains("Content-Range: bytes 8192-20000/*\r\n"));
        std::fs::rename(&job.part, &job.dest).unwrap();
        write_cache_stamp_for_key(&job.dest, &job.web_spec.as_ref().unwrap().cache_key).unwrap();
        job.transition(RemuxState::Complete);
        let tail = crate::tests::raw_connection(
            app.clone(),
            request(&format!("Range: bytes=20001-\r\nIf-Match: {etag}\r\n")).as_bytes(),
            false,
        )
        .await;
        assert_eq!(wire_body(&tail), &payload[20001..]);
        let headers = String::from_utf8_lossy(&tail[..tail.len() - (payload.len() - 20001)]);
        assert!(headers.contains(&format!("ETag: {etag}\r\n")));
        assert!(headers.contains(&format!(
            "Content-Range: bytes 20001-{}/{}\r\n",
            payload.len() - 1,
            payload.len()
        )));
        // The cache registry may expire during a long pause. Reattaching the
        // same finished inode must still accept the growing response's tag.
        crate::lock_recover(&app.remuxes).clear();
        let reattached = crate::tests::raw_connection(
            app.clone(),
            request(&format!("Range: bytes=8192-16383\r\nIf-Match: {etag}\r\n")).as_bytes(),
            false,
        )
        .await;
        assert_eq!(wire_body(&reattached), &payload[8192..16384]);
        let stale = crate::tests::raw_connection(
            app.clone(),
            request("Range: bytes=8192-16383\r\nIf-Range: \"older-file\"\r\n").as_bytes(),
            false,
        )
        .await;
        assert!(stale.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(wire_body(&stale), payload);
        let complete = crate::tests::raw_connection(
            app.clone(),
            request(&format!(
                "Range: bytes={}-\r\nIf-Match: {etag}\r\n",
                payload.len()
            ))
            .as_bytes(),
            false,
        )
        .await;
        let headers = String::from_utf8_lossy(&complete);
        assert!(headers.starts_with("HTTP/1.1 416 "));
        assert!(headers.contains(&format!("Content-Range: bytes */{}\r\n", payload.len())));
        assert!(headers.contains(&format!("ETag: {etag}\r\n")));
        let replaced = crate::tests::raw_connection(
            app.clone(),
            request("Range: bytes=8192-16383\r\nIf-Match: \"replaced-output\"\r\n").as_bytes(),
            false,
        )
        .await;
        assert!(replaced.starts_with(b"HTTP/1.1 412 Precondition Failed\r\n"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_download_prepares_then_resumes_the_same_finalized_bytes() {
        let (app, job, url, payload) = compatible_connection_fixture();
        let request = |headers: &str| {
            format!(
                "GET {url} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\nX-RustyDLNA-Download: resumable\r\n{headers}\r\n"
            )
        };
        // Preparation replies never transfer an unvalidated prefix, and the
        // next system-owned request retains the same producer/generation.
        for _ in 0..2 {
            let bytes =
                crate::tests::raw_connection(app.clone(), request("").as_bytes(), false).await;
            let headers = String::from_utf8_lossy(&bytes);
            assert!(
                headers.starts_with("HTTP/1.1 202 Accepted\r\n"),
                "{headers}"
            );
            assert!(headers.contains("\r\nX-RustyDLNA-Download: preparing\r\n"));
            assert!(headers.contains("\r\nRetry-After: 30\r\n"));
            assert!(wire_body(&bytes).is_empty());
            assert_eq!(job.state(), RemuxState::Growing);
            assert!(crate::lock_recover(&job.disconnect_deadline).is_none());
            assert!(crate::lock_recover(&job.output).is_none());
        }
        // Ordinary browser reads keep streaming the growing file.
        let browser = compatible_connection_wire(&app, &url, "GET", Some("bytes=0-7")).await;
        assert_eq!(wire_body(&browser), &payload[..8]);

        std::fs::rename(&job.part, &job.dest).unwrap();
        write_cache_stamp_for_key(&job.dest, &job.web_spec.as_ref().unwrap().cache_key).unwrap();
        job.transition(RemuxState::Complete);
        let full = crate::tests::raw_connection(app.clone(), request("").as_bytes(), false).await;
        assert_eq!(wire_body(&full), payload);
        let headers = String::from_utf8_lossy(&full[..full.len() - payload.len()]);
        assert!(headers.contains(&format!("\r\nContent-Length: {}\r\n", payload.len())));
        let etag = headers
            .lines()
            .find_map(|line| line.strip_prefix("ETag: "))
            .unwrap();
        let offset = payload.len() / 2;
        let range = format!("Range: bytes={offset}-\r\nIf-Range: {etag}\r\n");
        for _ in 0..2 {
            let resumed =
                crate::tests::raw_connection(app.clone(), request(&range).as_bytes(), false).await;
            assert!(resumed.starts_with(b"HTTP/1.1 206 Partial Content\r\n"));
            assert_eq!(wire_body(&resumed), &payload[offset..]);
        }
        let changed = crate::tests::raw_connection(
            app.clone(),
            request("Range: bytes=8-\r\nIf-Range: \"different-generation\"\r\n").as_bytes(),
            false,
        )
        .await;
        assert!(changed.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(wire_body(&changed), payload);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_download_cancelled_preparation_does_not_serve_partial_media() {
        let (app, job, url, _) = compatible_connection_fixture();
        job.transition(RemuxState::Cancelled);
        let request = HttpRequest::parse_headers(&format!(
            "GET {url} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nX-RustyDLNA-Download: resumable\r\n\r\n"
        ))
        .unwrap();
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_resumable_download(&app, &mut socket, &request, &job, "video/mp4")
                .await
                .unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap();
        assert!(bytes.starts_with(b"HTTP/1.1 409 Conflict\r\n"));
        let body: serde_json::Value = serde_json::from_slice(wire_body(&bytes)).unwrap();
        assert_eq!(body["error"]["code"], "transcode_cancelled");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_compatible_head_through_connection_reports_final_length() {
        let (app, job, url, payload) = compatible_connection_fixture();
        std::fs::rename(&job.part, &job.dest).unwrap();
        job.transition(RemuxState::Complete);

        for method in ["HEAD", "GET"] {
            for (range, expected, status) in [
                (None, payload.as_slice(), "200 OK"),
                (Some("bytes=0-7"), &payload[..8], "206 Partial Content"),
            ] {
                let bytes = compatible_connection_wire(&app, &url, method, range).await;
                let headers =
                    String::from_utf8_lossy(&bytes[..bytes.len() - wire_body(&bytes).len()]);
                assert!(
                    headers.starts_with(&format!("HTTP/1.1 {status}\r\n")),
                    "{headers}"
                );
                assert!(
                    headers.contains("\r\nContent-Type: video/mp4\r\n"),
                    "{headers}"
                );
                assert!(
                    headers.contains(&format!("\r\nContent-Length: {}\r\n", expected.len())),
                    "{method} {range:?}: {headers}"
                );
                assert!(
                    headers.contains("\r\nAccept-Ranges: bytes\r\n"),
                    "{headers}"
                );
                if range.is_some() {
                    assert!(
                        headers.contains(&format!(
                            "\r\nContent-Range: bytes 0-7/{}\r\n",
                            payload.len()
                        )),
                        "{headers}"
                    );
                }
                assert_eq!(
                    wire_body(&bytes),
                    if method == "HEAD" { &[] } else { expected }
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compatible_head_through_connection_suppresses_range_error_bodies() {
        let (app, job, url, payload) = compatible_connection_fixture();
        for complete in [false, true] {
            if complete {
                std::fs::rename(&job.part, &job.dest).unwrap();
                job.transition(RemuxState::Complete);
            }
            for (range, status) in [
                ("bytes=invalid".to_owned(), "400 Bad Request"),
                (
                    format!("bytes={}-", payload.len()),
                    "416 Requested Range Not Satisfiable",
                ),
            ] {
                // A growing producer can still satisfy a range beyond its current end.
                if !complete && status.starts_with("416") {
                    continue;
                }
                for method in ["HEAD", "GET"] {
                    let bytes = compatible_connection_wire(&app, &url, method, Some(&range)).await;
                    let headers = String::from_utf8_lossy(&bytes);
                    assert!(
                        headers.starts_with(&format!("HTTP/1.1 {status}\r\n")),
                        "{headers}"
                    );
                    assert_eq!(wire_body(&bytes).is_empty(), method == "HEAD", "{headers}");
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn growing_compatible_head_through_connection_keeps_length_unknown() {
        let (app, job, url, payload) = compatible_connection_fixture();
        for range in [None, Some("bytes=0-")] {
            let bytes = compatible_connection_wire(&app, &url, "HEAD", range).await;
            let headers = String::from_utf8_lossy(&bytes);
            assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{headers}");
            assert!(
                !headers.to_ascii_lowercase().contains("content-length:"),
                "{headers}"
            );
            assert!(
                !headers.to_ascii_lowercase().contains("transfer-encoding:"),
                "{headers}"
            );
            assert!(wire_body(&bytes).is_empty());
            assert_eq!(job.state(), RemuxState::Growing);
        }
        for method in ["HEAD", "GET"] {
            let bytes = compatible_connection_wire(&app, &url, method, Some("bytes=0-7")).await;
            let headers = String::from_utf8_lossy(&bytes);
            assert!(
                headers.starts_with("HTTP/1.1 206 Partial Content\r\n"),
                "{headers}"
            );
            assert!(headers.contains("\r\nContent-Length: 8\r\n"), "{headers}");
            assert!(
                headers.contains("\r\nContent-Range: bytes 0-7/*\r\n"),
                "{headers}"
            );
            assert_eq!(
                wire_body(&bytes),
                if method == "HEAD" { &[] } else { &payload[..8] }
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finished_and_growing_transcode_head_emit_headers_only() {
        use tokio::io::AsyncReadExt;

        let dir = temp_dir("head-wire");
        let app = test_app(&dir, 1);
        let finished_job = growing_test_job(&dir, 44, b"finished-transcode-bytes");
        std::fs::rename(&finished_job.part, &finished_job.dest).unwrap();
        finished_job.transition(RemuxState::Complete);
        let finished_req = HttpRequest::parse_headers(
            "HEAD /Transcode/42.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-7\r\n\r\n",
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_finished(
                &server_app,
                &mut socket,
                &finished_req,
                &finished_job,
                "video/mp4",
                true,
            )
            .await
            .unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        server.await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("Content-Length: 8"));
        assert!(wire_body(&bytes).is_empty());

        let growing_bytes = vec![0x5a; FIRST_BYTES as usize];
        let growing = growing_test_job(&dir, 43, &growing_bytes);
        let bytes = growing_wire(
            app,
            growing,
            "HEAD /Transcode/43.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
            true,
        )
        .await;
        assert!(String::from_utf8_lossy(&bytes).contains("transferMode.dlna.org: Streaming"));
        assert!(String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 200 OK"));
        assert!(!String::from_utf8_lossy(&bytes).contains("Content-Length"));
        assert!(wire_body(&bytes).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn growing_transcode_ranges_are_bounded_partial_responses() {
        let dir = temp_dir("growing-range-wire");
        let app = test_app(&dir, 1);
        let body = vec![0x5a; FIRST_BYTES as usize];
        let growing = growing_test_job(&dir, 44, &body);

        let bytes = growing_wire(
            app.clone(),
            growing.clone(),
            "GET /Transcode/44.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=4096-\r\n\r\n",
            false,
        )
        .await;
        let headers = String::from_utf8_lossy(&bytes);
        assert!(headers.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(headers.contains("Content-Range: bytes 4096-16383/*"));
        assert!(headers.contains("Content-Length: 12288"));
        assert_eq!(wire_body(&bytes), &body[4096..]);
        assert_eq!(growing.state(), RemuxState::Growing);

        let head = growing_wire(
            app.clone(),
            growing.clone(),
            "HEAD /Transcode/44.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=4096-\r\n\r\n",
            true,
        )
        .await;
        let head_headers = String::from_utf8_lossy(&head);
        assert!(head_headers.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(head_headers.contains("Content-Range: bytes 4096-16383/*"));
        assert!(head_headers.contains("Content-Length: 12288"));
        assert!(wire_body(&head).is_empty());

        let large = growing_wire(
            app.clone(),
            growing.clone(),
            "GET /Transcode/44.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-2097152\r\n\r\n",
            false,
        )
        .await;
        let large_headers = String::from_utf8_lossy(&large);
        assert!(large_headers.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(large_headers.contains("Content-Range: bytes 0-16383/*"));
        assert!(large_headers.contains("Content-Length: 16384"));
        assert_eq!(wire_body(&large), body);

        let zero_open = growing_wire(
            app,
            growing,
            "HEAD /Transcode/44.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-\r\n\r\n",
            true,
        )
        .await;
        let zero_headers = String::from_utf8_lossy(&zero_open);
        assert!(zero_headers.starts_with("HTTP/1.1 200 OK"));
        assert!(!zero_headers.contains("Content-Length"));
        assert!(wire_body(&zero_open).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn growing_open_range_waits_for_its_first_byte() {
        use std::io::Write;

        let dir = temp_dir("growing-range-wait");
        let app = test_app(&dir, 1);
        let growing = growing_test_job(&dir, 45, &[0x31; 4096]);
        let response = tokio::spawn(growing_wire(
            app,
            growing.clone(),
            "GET /Transcode/45.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=4096-\r\n\r\n",
            false,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut part = std::fs::OpenOptions::new()
            .append(true)
            .open(&growing.part)
            .unwrap();
        part.write_all(&[0x32; 4096]).unwrap();
        growing.changed.notify_waiters();

        let bytes = response.await.unwrap();
        let headers = String::from_utf8_lossy(&bytes);
        assert!(headers.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(headers.contains("Content-Range: bytes 4096-8191/*"));
        assert!(headers.contains("Content-Length: 4096"));
        assert_eq!(wire_body(&bytes), &[0x32; 4096]);

        let complete = growing_test_job(&dir, 46, &[0x41; 1024]);
        complete.transition(RemuxState::Complete);
        let bytes = growing_wire(
            test_app(&dir, 1),
            complete,
            "GET /Transcode/46.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=4096-\r\n\r\n",
            false,
        )
        .await;
        let headers = String::from_utf8_lossy(&bytes);
        assert!(headers.starts_with("HTTP/1.1 416 Requested Range Not Satisfiable"));
        assert!(headers.contains("Content-Range: bytes */1024"));
    }

    #[test]
    fn successful_and_failed_jobs_release_map_entries_and_permits() {
        let dir = temp_dir("terminal-cleanup");
        let app = test_app(&dir, 1);
        let dest = dir.join("success.mp4");
        let part = cache_part(&dest);
        let success = job_spec(
            &dir,
            "success",
            vec![
                "cp".into(),
                dir.join("source.mkv").display().to_string(),
                part.display().to_string(),
            ],
        );
        let cached_success = success.clone();
        let success_job = attach(app.clone(), success).unwrap();
        wait_for_terminal_cleanup(&app, &success_job);
        assert_eq!(success_job.state(), RemuxState::Complete);
        assert!(dest.is_file());
        let cached_job = attach(app.clone(), cached_success).unwrap();
        assert_eq!(cached_job.state(), RemuxState::Complete);
        let cache_metrics = runtime_status(&app);
        assert_eq!(cache_metrics.cache_hits_total, 1);
        assert_eq!(cache_metrics.cache_misses_total, 1);

        let failure = job_spec(
            &dir,
            "failure",
            vec!["/definitely/missing/rusty-dlna-command".into()],
        );
        let failure_job = attach(app.clone(), failure).unwrap();
        wait_for_terminal_cleanup(&app, &failure_job);
        assert!(matches!(failure_job.state(), RemuxState::Failed(_)));

        let retry = job_spec(
            &dir,
            "failure",
            vec!["/definitely/missing/rusty-dlna-command".into()],
        );
        let retry_job = attach(app.clone(), retry).unwrap();
        assert!(!Arc::ptr_eq(&failure_job, &retry_job));
        wait_for_terminal_cleanup(&app, &retry_job);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn complete_job_keys_separate_plans_and_deduplicate_same_plan() {
        let dir = temp_dir("job-keys");
        let app = test_app(&dir, 2);
        let first = job_spec(&dir, "plan-a", vec!["sleep".into(), "1".into()]);
        let second = job_spec(&dir, "plan-b", vec!["sleep".into(), "1".into()]);
        let same = first.clone();
        let first_job = attach(app.clone(), first).unwrap();
        let same_job = attach(app.clone(), same).unwrap();
        assert!(Arc::ptr_eq(&first_job, &same_job));
        let second_job = attach(app.clone(), second).unwrap();
        assert!(!Arc::ptr_eq(&first_job, &second_job));
        assert_eq!(app.remuxes.lock().unwrap().len(), 2);
        wait_for_terminal_cleanup(&app, &first_job);
        // Both one-second commands normally finish in the same monitor tick.
        wait_for_terminal_cleanup(&app, &second_job);
        let metrics = runtime_status(&app);
        assert_eq!(metrics.cache_misses_total, 2);
        assert_eq!(metrics.coalesced_requests_total, 1);
        assert_eq!(metrics.cache_hits_total, 0);
        assert!(metrics.cache_maintenance_total >= 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn simultaneous_stale_cache_attaches_share_one_rebuild() {
        let dir = temp_dir("stale-race");
        let app = test_app(&dir, 1);
        let spec = job_spec(&dir, "stale", vec!["sleep".into(), "1".into()]);
        std::fs::write(&spec.dest, b"old output").unwrap();
        std::fs::write(
            rusty_dlna_transcode::cache_stamp_path(&spec.dest),
            "different-key",
        )
        .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let app = app.clone();
            let spec = spec.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                attach(app, spec).unwrap()
            }));
        }
        let jobs = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert!(jobs.iter().skip(1).all(|job| Arc::ptr_eq(&jobs[0], job)));
        assert_eq!(app.remuxes.lock().unwrap().len(), 1);
        wait_for_terminal_cleanup(&app, &jobs[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn source_replacement_after_completion_starts_a_new_job_and_output() {
        let dir = temp_dir("source-replacement");
        let app = test_app(&dir, 1);
        let src = dir.join("source.mkv");
        std::fs::write(&src, b"first-source-version").unwrap();
        let first_key = rusty_dlna_transcode::source_identity(&src).unwrap();
        let dest = dir.join("shared-output.mp4");
        let part = cache_part(&dest);
        let first = RemuxJobSpec {
            output_expectation: None,
            detail_id: 42,
            web_session_id: None,
            web_request_id: None,
            mime: "video/mp4",
            job_key: format!("42:{first_key}:copy"),
            cache_key: first_key.clone(),
            src: src.clone(),
            source_file: None,
            ai_upscale_shader_file: None,
            dest: dest.clone(),
            args: vec![
                "cp".into(),
                src.as_os_str().to_os_string(),
                part.as_os_str().to_os_string(),
            ],
            hardware_fallback_args: None,
            fallback_args: None,
            continue_after_disconnect: true,
            cacheable: true,
            hls_all_fragments_independent: false,
            remux_p8: false,
            verified_ffmpeg: None,
            profile8_toolchain: None,
            audio_index: 0,
            audio: RemuxAudio::Copy,
        };
        let first_job = attach(app.clone(), first).unwrap();
        wait_for_terminal_cleanup(&app, &first_job);
        assert_eq!(std::fs::read(&dest).unwrap(), b"first-source-version");

        // Same length replacement exercises the sub-second/source-fingerprint
        // portion rather than relying only on a size change.
        std::fs::write(&src, b"other-source-version").unwrap();
        let second_key = rusty_dlna_transcode::source_identity(&src).unwrap();
        assert_ne!(first_key, second_key);
        let second = RemuxJobSpec {
            output_expectation: None,
            detail_id: 42,
            web_session_id: None,
            web_request_id: None,
            mime: "video/mp4",
            job_key: format!("42:{second_key}:copy"),
            cache_key: second_key,
            src: src.clone(),
            source_file: None,
            ai_upscale_shader_file: None,
            dest: dest.clone(),
            args: vec![
                "cp".into(),
                src.as_os_str().to_os_string(),
                part.as_os_str().to_os_string(),
            ],
            hardware_fallback_args: None,
            fallback_args: None,
            continue_after_disconnect: true,
            cacheable: true,
            hls_all_fragments_independent: false,
            remux_p8: false,
            verified_ffmpeg: None,
            profile8_toolchain: None,
            audio_index: 0,
            audio: RemuxAudio::Copy,
        };
        let second_job = attach(app.clone(), second).unwrap();
        assert!(!Arc::ptr_eq(&first_job, &second_job));
        wait_for_terminal_cleanup(&app, &second_job);
        assert_eq!(std::fs::read(&dest).unwrap(), b"other-source-version");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn many_ephemeral_seek_jobs_leave_no_completed_cache_tails() {
        let dir = temp_dir("ephemeral-web-seeks");
        let app = test_app(&dir, 1);
        for second in 1..=24 {
            let mut spec = job_spec(
                &dir,
                &format!("seek-{second}"),
                vec![
                    "cp".into(),
                    dir.join("source.mkv").display().to_string(),
                    dir.join(format!("seek-{second}.mp4.part"))
                        .display()
                        .to_string(),
                ],
            );
            spec.dest = dir.join(format!("seek-{second}.mp4"));
            spec.args[2] = cache_part(&spec.dest).as_os_str().to_os_string();
            spec.cacheable = false;
            spec.continue_after_disconnect = false;
            let job = attach(app.clone(), spec).unwrap();
            wait_for_terminal_cleanup(&app, &job);
            assert!(!job.dest.exists(), "ephemeral seek {second} was retained");
        }
        assert_eq!(app.remux_metrics.cache_bytes.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn web_disconnect_grace_allows_reconnect_then_cancels_an_abandoned_job() {
        let dir = temp_dir("web-reconnect-grace");
        let app = test_app(&dir, 1);
        let mut spec = job_spec(
            &dir,
            "web-reconnect-grace",
            vec!["sleep".into(), "30".into()],
        );
        spec.job_key = "web:42:reconnect-grace".into();
        spec.cacheable = false;
        spec.continue_after_disconnect = false;
        let retry = spec.clone();
        let job = attach_for_client(app.clone(), spec).unwrap();
        job.detach_client(
            app.clone(),
            "web:42:reconnect-grace".into(),
            false,
            Duration::from_millis(100),
            Duration::ZERO,
        );
        std::thread::sleep(Duration::from_millis(25));
        let reconnected = attach_for_client(app.clone(), retry).unwrap();
        assert!(Arc::ptr_eq(&job, &reconnected));
        std::thread::sleep(Duration::from_millis(125));
        assert!(!job.cancelled.load(Ordering::Acquire));
        assert!(!matches!(job.state(), RemuxState::Cancelled));

        job.detach_client(
            app.clone(),
            "web:42:reconnect-grace".into(),
            false,
            Duration::from_millis(20),
            Duration::ZERO,
        );
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Cancelled);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn completed_web_cache_job_reuses_its_plan_and_lease_without_losing_output() {
        let dir = temp_dir("web-complete-lease");
        let app = test_app(&dir, 1);
        let mut spec = job_spec(&dir, "web-complete-lease", Vec::new());
        spec.job_key = "web:42:complete-lease".into();
        spec.web_session_id = Some(9);
        spec.web_request_id = Some(77);
        spec.continue_after_disconnect = false;
        let part = cache_part(&spec.dest);
        spec.args = vec![
            "cp".into(),
            spec.src.as_os_str().to_os_string(),
            part.as_os_str().to_os_string(),
        ];
        let dest = spec.dest.clone();
        let cache_key = spec.cache_key.clone();
        let job = attach_for_client(app.clone(), spec).unwrap();
        wait_until(Duration::from_secs(3), || job.is_complete());
        wait_until(Duration::from_secs(3), || {
            job.registry_finalized.load(Ordering::Acquire)
        });

        let reused = active_web_job_spec(&app, 42, Some(9), Some(77))
            .expect("active generation should retain its validated plan");
        assert_eq!(reused.job_key, "web:42:complete-lease");

        job.detach_client(
            app.clone(),
            "web:42:complete-lease".into(),
            false,
            Duration::ZERO,
            Duration::from_secs(10),
        );
        assert!(keep_web_request_alive_for(
            &app,
            42,
            Some(9),
            77,
            Duration::from_secs(30),
        ));

        // Sweep past the original retention deadline without relying on a
        // loaded runner to wake within millisecond sleeps. The heartbeat's
        // renewed lease must retain the registered plan.
        sweep_ephemeral_cleanups(&app, Instant::now() + Duration::from_secs(15), false);
        assert!(active_web_job_spec(&app, 42, Some(9), Some(77)).is_some());

        sweep_ephemeral_cleanups(&app, Instant::now() + Duration::from_secs(60), false);
        assert!(active_web_job_spec(&app, 42, Some(9), Some(77)).is_none());
        assert!(dest.is_file(), "retention must not delete cacheable output");
        assert!(cache_is_fresh_for_key(&dest, &cache_key));
    }

    #[test]
    fn active_web_generation_heartbeat_renews_a_reader_free_job() {
        let dir = temp_dir("web-active-lease");
        let app = test_app(&dir, 1);
        let mut spec = job_spec(&dir, "web-active-lease", vec!["sleep".into(), "30".into()]);
        spec.job_key = "web:42:active-lease".into();
        spec.web_session_id = Some(9);
        spec.web_request_id = Some(77);
        spec.cacheable = false;
        spec.continue_after_disconnect = false;
        let job = attach_for_client(app.clone(), spec).unwrap();
        job.detach_client(
            app.clone(),
            "web:42:active-lease".into(),
            false,
            Duration::from_millis(80),
            Duration::ZERO,
        );

        std::thread::sleep(Duration::from_millis(30));
        assert!(keep_web_request_alive_for(
            &app,
            42,
            Some(9),
            77,
            Duration::from_millis(250),
        ));
        assert!(!keep_web_request_alive_for(
            &app,
            42,
            Some(9),
            76,
            Duration::from_secs(1),
        ));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!job.cancelled.load(Ordering::Acquire));
        assert!(!matches!(job.state(), RemuxState::Cancelled));

        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Cancelled);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_web_cancellation_stops_its_only_producer_immediately() {
        let dir = temp_dir("web-explicit-cancel");
        let app = test_app(&dir, 1);
        let mut spec = job_spec(
            &dir,
            "web-explicit-cancel",
            vec!["sleep".into(), "30".into()],
        );
        spec.job_key = "web:42:explicit-cancel".into();
        spec.web_session_id = Some(9);
        spec.web_request_id = Some(77);
        spec.cacheable = false;
        spec.continue_after_disconnect = false;
        let job = attach_for_client(app.clone(), spec).unwrap();

        assert!(cancel_web_request(&app, 42, Some(9), 77));
        assert!(job.cancelled.load(Ordering::Acquire));
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Cancelled);
    }

    #[test]
    fn cancelling_one_request_does_not_kill_a_shared_web_job() {
        let dir = temp_dir("web-shared-cancel");
        let app = test_app(&dir, 1);
        let mut first = job_spec(&dir, "web-shared-cancel", vec!["sleep".into(), "30".into()]);
        first.job_key = "web:42:shared-cancel".into();
        first.web_session_id = Some(9);
        first.web_request_id = Some(77);
        first.cacheable = false;
        first.continue_after_disconnect = false;
        let mut second = first.clone();
        second.web_session_id = Some(10);
        second.web_request_id = Some(88);
        let job = attach_for_client(app.clone(), first).unwrap();
        let shared = attach_for_client(app.clone(), second).unwrap();
        assert!(Arc::ptr_eq(&job, &shared));
        let shared_spec = active_web_job_spec(&app, 42, Some(10), Some(88)).unwrap();
        assert_eq!(shared_spec.web_session_id, Some(10));
        assert_eq!(shared_spec.web_request_id, Some(88));

        assert!(cancel_web_request(&app, 42, Some(9), 77));
        assert!(!job.cancelled.load(Ordering::Acquire));
        assert!(cancel_web_request(&app, 42, Some(10), 88));
        assert!(job.cancelled.load(Ordering::Acquire));
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Cancelled);
    }

    #[test]
    fn cancellation_that_arrives_before_media_get_rejects_the_late_request() {
        let dir = temp_dir("web-cancel-before-get");
        let app = test_app(&dir, 1);
        assert!(cancel_web_request(&app, 42, Some(9), 77));

        let mut spec = job_spec(
            &dir,
            "web-cancel-before-get",
            vec!["sleep".into(), "30".into()],
        );
        spec.job_key = "web:42:cancel-before-get".into();
        spec.web_session_id = Some(9);
        spec.web_request_id = Some(77);
        spec.cacheable = false;

        assert_eq!(
            attach_for_client(app.clone(), spec).err().as_deref(),
            Some(WEB_REQUEST_CANCELLED)
        );
        assert_eq!(app.jobs.in_use(), 0);
        assert!(app.remuxes.lock().unwrap().is_empty());
    }

    #[test]
    fn newer_playback_generation_cancels_every_older_session_job() {
        let dir = temp_dir("web-generation-supersede");
        let app = test_app(&dir, 2);
        let mut first = job_spec(
            &dir,
            "web-generation-first",
            vec!["sleep".into(), "30".into()],
        );
        first.job_key = "web:42:generation-first".into();
        first.web_session_id = Some(9);
        first.web_request_id = Some(77);
        first.cacheable = false;
        let stale = first.clone();
        let first_job = attach_for_client(app.clone(), first).unwrap();

        let mut second = job_spec(
            &dir,
            "web-generation-second",
            vec!["sleep".into(), "30".into()],
        );
        second.job_key = "web:42:generation-second".into();
        second.web_session_id = Some(9);
        second.web_request_id = Some(78);
        second.cacheable = false;
        let second_job = attach_for_client(app.clone(), second).unwrap();

        assert!(first_job.cancelled.load(Ordering::Acquire));
        assert!(!second_job.cancelled.load(Ordering::Acquire));
        assert_eq!(
            attach_for_client(app.clone(), stale).err().as_deref(),
            Some(WEB_REQUEST_CANCELLED)
        );
        assert_eq!(web_job_state(&app, 42, Some(77)), ("cancelled", None));

        assert!(cancel_web_request(&app, 42, Some(9), 78));
        wait_for_terminal_cleanup(&app, &first_job);
        wait_for_terminal_cleanup(&app, &second_job);
    }

    #[test]
    fn newer_ai_generation_waits_for_the_superseded_gpu_permit() {
        let dir = temp_dir("web-ai-generation-handoff");
        let app = test_app(&dir, 1);
        let mut first = job_spec(
            &dir,
            "web-ai-generation-first",
            vec!["sleep".into(), "30".into()],
        );
        first.job_key = "web:42:ai-generation-first".into();
        first.web_session_id = Some(9);
        first.web_request_id = Some(77);
        first.cacheable = false;
        first.ai_upscale_shader_file = Some(Arc::new(std::fs::File::open(&first.src).unwrap()));
        let first_job = attach_started_long_running_job(app.clone(), first);
        assert_eq!(app.jobs.in_use(), 1);
        assert_eq!(app.ai_upscale_jobs.in_use(), 1);

        let mut second = job_spec(
            &dir,
            "web-ai-generation-second",
            vec!["sleep".into(), "30".into()],
        );
        second.job_key = "web:42:ai-generation-second".into();
        second.web_session_id = Some(9);
        second.web_request_id = Some(78);
        second.cacheable = false;
        second.ai_upscale_shader_file = Some(Arc::new(std::fs::File::open(&second.src).unwrap()));
        let second_job = attach_for_client(app.clone(), second).unwrap();

        assert!(first_job.cancelled.load(Ordering::Acquire));
        assert!(!second_job.cancelled.load(Ordering::Acquire));
        assert_eq!(app.jobs.in_use(), 1);
        assert_eq!(app.ai_upscale_jobs.in_use(), 1);

        assert!(cancel_web_request(&app, 42, Some(9), 78));
        wait_for_terminal_cleanup(&app, &first_job);
        wait_for_terminal_cleanup(&app, &second_job);
    }

    #[test]
    fn newer_ai_generation_inherits_the_explicit_cancellation_handoff() {
        let dir = temp_dir("web-ai-explicit-cancel-handoff");
        let app = test_app(&dir, 1);
        let mut first = job_spec(
            &dir,
            "web-ai-explicit-cancel-first",
            vec!["sleep".into(), "30".into()],
        );
        first.job_key = "web:42:ai-explicit-cancel-first".into();
        first.web_session_id = Some(9);
        first.web_request_id = Some(77);
        first.cacheable = false;
        first.ai_upscale_shader_file = Some(Arc::new(std::fs::File::open(&first.src).unwrap()));
        let first_job = attach_started_long_running_job(app.clone(), first);

        assert!(cancel_web_request(&app, 42, Some(9), 77));
        assert!(first_job.cancelled.load(Ordering::Acquire));
        assert_eq!(app.jobs.in_use(), 1);
        assert_eq!(app.ai_upscale_jobs.in_use(), 1);
        assert_eq!(
            crate::lock_recover(&app.web_playback_sessions)
                .get(&9)
                .and_then(|state| state.cancelled_handoff),
            Some(WebCancelledProducerHandoff {
                detail_id: 42,
                ai_upscale: true,
            })
        );

        let mut second = job_spec(
            &dir,
            "web-ai-explicit-cancel-second",
            vec!["sleep".into(), "30".into()],
        );
        second.job_key = "web:42:ai-explicit-cancel-second".into();
        second.web_session_id = Some(9);
        second.web_request_id = Some(78);
        second.cacheable = false;
        second.ai_upscale_shader_file = Some(Arc::new(std::fs::File::open(&second.src).unwrap()));
        let second_job = attach_for_client(app.clone(), second).unwrap();

        assert!(!second_job.cancelled.load(Ordering::Acquire));
        assert_eq!(app.jobs.in_use(), 1);
        assert_eq!(app.ai_upscale_jobs.in_use(), 1);

        assert!(cancel_web_request(&app, 42, Some(9), 78));
        wait_for_terminal_cleanup(&app, &first_job);
        wait_for_terminal_cleanup(&app, &second_job);
    }

    #[test]
    fn completed_ephemeral_web_output_survives_a_reconnect_window() {
        let dir = temp_dir("web-complete-reconnect");
        let app = test_app(&dir, 1);
        let dest = dir.join("web-complete-reconnect.mp4");
        let part = cache_part(&dest);
        let mut spec = job_spec(
            &dir,
            "web-complete-reconnect",
            vec![
                "cp".into(),
                dir.join("source.mkv").display().to_string(),
                part.display().to_string(),
            ],
        );
        spec.job_key = "web:42:complete-reconnect".into();
        spec.dest = dest.clone();
        spec.cacheable = false;
        spec.continue_after_disconnect = false;
        let retry = spec.clone();
        let job = attach_for_client(app.clone(), spec).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while (!matches!(job.state(), RemuxState::Complete) || app.jobs.in_use() != 0)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job.state(), RemuxState::Complete);
        assert!(dest.is_file());
        assert_eq!(app.jobs.in_use(), 0);

        job.detach_client(
            app.clone(),
            "web:42:complete-reconnect".into(),
            false,
            Duration::ZERO,
            Duration::from_millis(100),
        );
        std::thread::sleep(Duration::from_millis(25));
        let reconnected = attach_for_client(app.clone(), retry).unwrap();
        assert!(Arc::ptr_eq(&job, &reconnected));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            dest.is_file(),
            "an active reconnect must retain the completed output"
        );

        reconnected.detach_client(
            app.clone(),
            "web:42:complete-reconnect".into(),
            false,
            Duration::ZERO,
            Duration::from_millis(20),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while (dest.exists() || !app.remuxes.lock().unwrap().is_empty())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!dest.exists());
        assert!(app.remuxes.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn completed_web_status_is_scoped_to_the_source_request_id() {
        let dir = temp_dir("web-status-request");
        let app = test_app(&dir, 1);
        app.recent_remux_states.lock().unwrap().insert(
            (42, 7),
            RecentRemuxState {
                state: "cancelled",
                at: Instant::now(),
            },
        );
        assert_eq!(web_job_state(&app, 42, Some(7)), ("cancelled", None));
        assert_eq!(web_job_state(&app, 42, Some(8)), ("idle", None));
        assert_eq!(web_job_state(&app, 42, None), ("cancelled", None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unicode_stderr_tail_never_slices_inside_a_character() {
        let text = format!("prefix-{}-suffix", "é".repeat(2000));
        let tail = tail_str(&text, 2001);
        assert!(tail.ends_with("suffix"));
        assert!(tail.len() <= 2002);
    }

    #[test]
    fn cancellation_kills_and_reaps_the_child_process() {
        let dir = temp_dir("cancel-reap");
        let app = test_app(&dir, 1);
        let pid_file = dir.join("child.pid");
        let part = cache_part(&dir.join("cancel.mp4"));
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let mut spec = job_spec(
            &dir,
            "cancel",
            vec!["sh".into(), "-c".into(), script, "rustydlna-job".into()],
        );
        spec.dest = dir.join("cancel.mp4");
        let job = attach(app.clone(), spec).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_file.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        cancel_all(&app);
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Cancelled);
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child must be reaped"
        );
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_gate_cannot_delay_running_child_cancellation_or_deadline() {
        for profile8 in [false, true] {
            for deadline in [false, true] {
                let dir = temp_dir("blocked-cache-child");
                let mut config = crate::Config {
                    cache_dir: Some(dir.display().to_string()),
                    rescan_secs: 0,
                    ..crate::Config::default()
                };
                config.transcode.enable = true;
                config.transcode.max_jobs = 1;
                config.transcode.max_runtime_secs = 5;
                let app = Arc::new(App::from_config(config, 18200, 11900, &dir));
                let mut spec = job_spec(&dir, "blocked-cache", Vec::new());
                spec.dest = rusty_dlna_transcode::cache_dest_for_key(
                    &dir,
                    42,
                    RecodeAction::Hdr10,
                    &"b".repeat(64),
                );
                let part = cache_part(&spec.dest);
                let leader = part.with_extension("p8-stage.pid");
                let descendant = part.with_extension("p8-stage-child.pid");
                if profile8 {
                    spec.remux_p8 = true;
                    crate::lock_recover(p8_test_runners())
                        .insert(part.clone(), cache_pressure_p8_stage);
                } else {
                    spec.args = vec![
                        "sh".into(), "-c".into(),
                        "echo $$ > \"$1\"; dd if=/dev/zero of=\"$2\" bs=1048576 count=2 2>/dev/null; trap '' TERM; sleep 30 & echo $! > \"$3\"; wait".into(),
                        "cache-gate-child".into(), leader.clone().into_os_string(),
                        part.clone().into_os_string(), descendant.clone().into_os_string(),
                    ];
                }
                let job = attach(app.clone(), spec).unwrap();
                struct CancelOnDrop(Arc<RemuxJob>);
                impl Drop for CancelOnDrop {
                    fn drop(&mut self) {
                        self.0.cancel();
                        let until = Instant::now() + Duration::from_secs(3);
                        while !self.0.producer_finished.load(Ordering::Acquire)
                            && Instant::now() < until
                        {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
                let _cancel = CancelOnDrop(job.clone());
                wait_until(Duration::from_secs(2), || descendant.is_file());
                let pids: Vec<_> = [&leader, &descendant]
                    .into_iter()
                    .map(|path| {
                        let pid = std::fs::read_to_string(path)
                            .unwrap()
                            .trim()
                            .parse::<u32>()
                            .unwrap();
                        PathBuf::from(format!("/proc/{pid}"))
                    })
                    .collect();
                assert!(pids.iter().all(|path| path.exists()));
                let gate = crate::lock_recover(&app.cache_maintenance);
                wait_until(Duration::from_secs(2), || {
                    app.transcode_cache.monitor.waiting()
                });
                if !deadline {
                    cancel_all(&app);
                }
                wait_until(Duration::from_secs(6), || {
                    job.producer_finished.load(Ordering::Acquire)
                        && app.jobs.in_use() == 0
                        && app.helpers.metrics().active == 0
                });
                if deadline {
                    assert!(
                        matches!(job.state(), RemuxState::Failed(error) if error.contains("deadline"))
                    );
                } else {
                    assert_eq!(job.state(), RemuxState::Cancelled);
                }
                wait_until(Duration::from_secs(2), || {
                    pids.iter().all(|path| !path.exists())
                });
                assert!(!part.exists());
                assert!(!part.with_extension("p8.hevc").exists());
                assert!(!job.dest.exists());
                assert!(crate::lock_recover(&app.remuxes).is_empty());
                // Every assertion above is made while the actual shared image/
                // video maintenance gate is still held by this thread.
                drop(gate);
                app.scan_cfg.cancellation.cancel();
                let weak = Arc::downgrade(&app);
                drop(app);
                wait_until(Duration::from_secs(2), || weak.upgrade().is_none());
            }
        }
    }

    #[test]
    fn successful_child_exit_keeps_readiness_owned_before_publication() {
        let fixture_dir = temp_dir("post-exit-media");
        let fixture = fixture_dir.join("source.mp4");
        let generated = rusty_dlna_helper::SupervisedCommand::new(
            std::process::Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=160x90:rate=24",
                    "-t",
                    "2",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-threads",
                    "2",
                    "-g",
                    "24",
                    "-pix_fmt",
                    "yuv420p",
                    "-an",
                    "-movflags",
                    "+frag_keyframe+empty_moov",
                ])
                .arg(&fixture),
        )
        .run_until(Instant::now() + Duration::from_secs(10), POLL, || {
            std::ops::ControlFlow::<()>::Continue(())
        })
        .unwrap();
        assert!(
            matches!(generated, rusty_dlna_helper::SupervisedOutcome::Exited(output) if output.status.success())
        );
        let media = std::fs::read(fixture).unwrap();
        assert!(media.len() >= FIRST_BYTES as usize);
        for mode in ["ready", "short", "cancel", "disconnect", "deadline"] {
            let dir = temp_dir("post-exit-readiness");
            let app = test_app(&dir, 1);
            let job = growing_test_job(&dir, 42, b"");
            job.transition(RemuxState::Starting);
            crate::lock_recover(&app.remuxes).insert("post-exit".into(), job.clone());
            let source = dir.join("source.mp4");
            let bytes = if mode == "short" {
                &b"too short"[..]
            } else {
                &media
            };
            std::fs::write(&source, bytes).unwrap();
            let leader = dir.join("leader.pid");
            let args = vec![
                "sh".into(),
                "-c".into(),
                "echo $$ > \"$1\"; cp \"$2\" \"$3\"".into(),
                "post-exit-child".into(),
                leader.clone().into_os_string(),
                source.into_os_string(),
                job.part.clone().into_os_string(),
            ];
            let (sent, received) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let gate = crate::lock_recover(&app.cache_maintenance);
                struct CancelOnDrop<'a>(&'a RemuxJob);
                impl Drop for CancelOnDrop<'_> {
                    fn drop(&mut self) {
                        self.0.cancel();
                    }
                }
                let _cancel = CancelOnDrop(&job);
                scope.spawn(|| {
                    let budget = if mode == "deadline" {
                        Duration::from_millis(500)
                    } else {
                        Duration::from_secs(5)
                    };
                    sent.send(run_ffmpeg_growing(
                        &args,
                        None,
                        None,
                        None,
                        &job,
                        Instant::now() + budget,
                        &app,
                    ))
                    .unwrap();
                });
                wait_until(Duration::from_secs(2), || leader.is_file());
                let pid = std::fs::read_to_string(&leader)
                    .unwrap()
                    .trim()
                    .parse::<u32>()
                    .unwrap();
                wait_until(Duration::from_secs(2), || {
                    !Path::new(&format!("/proc/{pid}")).exists()
                });
                assert_eq!(job.state(), RemuxState::Starting);
                assert!(job.pin_ready_output().unwrap().is_none());
                match mode {
                    "ready" | "short" => drop(gate),
                    "cancel" => job.cancel(),
                    "disconnect" => {
                        *crate::lock_recover(&job.disconnect_deadline) = Some(Instant::now())
                    }
                    _ => {}
                }
                let result = received.recv_timeout(Duration::from_secs(2)).unwrap();
                if mode == "ready" {
                    assert!(result.unwrap().0.success());
                    assert_eq!(job.state(), RemuxState::Growing);
                    let ready = job.pin_ready_output().unwrap().unwrap();
                    assert_eq!(std::fs::read(ready).unwrap(), media);
                    assert!(
                        !job.dest.exists(),
                        "readiness precedes completed publication"
                    );
                } else if mode == "short" {
                    assert!(result.unwrap().0.success());
                    assert_eq!(job.state(), RemuxState::Starting);
                    assert!(job.pin_ready_output().unwrap().is_none());
                } else {
                    let expected = if mode == "deadline" {
                        "deadline"
                    } else {
                        "cancelled"
                    };
                    assert!(result.unwrap_err().contains(expected));
                }
            });
        }
    }

    #[test]
    fn cache_gate_cannot_delay_cancellation_after_child_exit() {
        let dir = temp_dir("blocked-cache-publication");
        let app = test_app(&dir, 1);
        let mut spec = job_spec(&dir, "blocked-cache-publication", Vec::new());
        let release = dir.join("release");
        let leader = dir.join("leader.pid");
        spec.args = vec![
            "sh".into(),
            "-c".into(),
            "echo $$ > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; cp \"$3\" \"$4\"".into(),
            "publication-child".into(),
            leader.clone().into_os_string(),
            release.clone().into_os_string(),
            spec.src.as_os_str().to_owned(),
            cache_part(&spec.dest).into_os_string(),
        ];
        let job = attach(app.clone(), spec).unwrap();
        wait_until(Duration::from_secs(2), || leader.is_file());
        let pid = std::fs::read_to_string(&leader)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let gate = crate::lock_recover(&app.cache_maintenance);
        std::fs::write(&release, b"finish").unwrap();
        wait_until(Duration::from_secs(2), || {
            !Path::new(&format!("/proc/{pid}")).exists()
        });
        assert!(!job.producer_finished.load(Ordering::Acquire));
        job.cancel();
        wait_until(Duration::from_secs(2), || {
            job.producer_finished.load(Ordering::Acquire)
                && app.jobs.in_use() == 0
                && app.helpers.metrics().active == 0
        });
        assert_eq!(job.state(), RemuxState::Cancelled);
        assert!(!job.part.exists());
        assert!(!job.dest.exists());
        drop(gate);
    }

    #[test]
    fn failed_hardware_command_retries_software_before_publishing() {
        let dir = temp_dir("hardware-fallback");
        let app = test_app(&dir, 1);
        let dest = dir.join("fallback.mp4");
        let part = cache_part(&dest);
        let mut spec = job_spec(
            &dir,
            "hardware-fallback",
            vec!["sh".into(), "-c".into(), "exit 1".into()],
        );
        spec.dest = dest.clone();
        spec.fallback_args = Some(vec![
            "cp".into(),
            spec.src.as_os_str().to_os_string(),
            part.as_os_str().to_os_string(),
        ]);
        let expected = std::fs::read(&spec.src).unwrap();
        let cache_key = spec.cache_key.clone();
        let stamp = rusty_dlna_transcode::cache_stamp_path(&dest);
        assert!(!dest.exists());
        std::fs::write(&stamp, &cache_key).unwrap();
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(&dest).unwrap(), expected);
        assert!(!stamp.exists());
        assert!(!cache_is_fresh_for_key(&dest, &cache_key));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hardware_fallback_precedes_portable_output_and_never_stamps_primary_cache() {
        for hardware_succeeds in [true, false] {
            let dir = temp_dir("hardware-fallback-chain");
            let app = test_app(&dir, 1);
            let mut spec = job_spec(
                &dir,
                "hardware-fallback-chain",
                vec!["sh".into(), "-c".into(), "exit 1".into()],
            );
            let dest = spec.dest.clone();
            let part = cache_part(&dest);
            let copy = vec![
                "cp".into(),
                spec.src.as_os_str().to_owned(),
                part.as_os_str().to_owned(),
            ];
            let fail = vec!["sh".into(), "-c".into(), "exit 1".into()];
            spec.hardware_fallback_args = Some(if hardware_succeeds {
                copy.clone()
            } else {
                fail.clone()
            });
            spec.fallback_args = Some(if hardware_succeeds { fail } else { copy });
            let expected = std::fs::read(&spec.src).unwrap();
            let key = spec.cache_key.clone();
            let stamp = rusty_dlna_transcode::cache_stamp_path(&dest);
            std::fs::write(&stamp, &key).unwrap();
            let job = attach(app.clone(), spec).unwrap();
            wait_for_terminal_cleanup(&app, &job);
            assert_eq!(job.state(), RemuxState::Complete);
            assert_eq!(std::fs::read(&dest).unwrap(), expected);
            assert!(!stamp.exists());
            assert!(!cache_is_fresh_for_key(&dest, &key));
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn successful_hdr10_fallback_does_not_publish_a_profile8_cache_stamp() {
        let dir = temp_dir("profile8-fallback-stamp");
        let app = test_app(&dir, 1);
        let dest = dir.join("profile8-fallback.mp4");
        let part = cache_part(&dest);
        let mut spec = job_spec(&dir, "profile8-fallback", Vec::new());
        spec.dest = dest.clone();
        spec.remux_p8 = true;
        spec.args = vec![
            "cp".into(),
            spec.src.as_os_str().to_os_string(),
            part.as_os_str().to_os_string(),
        ];
        let expected = std::fs::read(&spec.src).unwrap();
        let cache_key = spec.cache_key.clone();

        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);

        assert_eq!(job.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(&dest).unwrap(), expected);
        assert!(!rusty_dlna_transcode::cache_stamp_path(&dest).exists());
        assert!(!cache_is_fresh_for_key(&dest, &cache_key));
    }

    #[test]
    fn runtime_deadline_and_last_client_policy_cancel_work() {
        let dir = temp_dir("runtime-timeout");
        let mut config = crate::Config {
            cache_dir: Some(dir.display().to_string()),
            rescan_secs: 0,
            ..crate::Config::default()
        };
        config.transcode.enable = true;
        config.transcode.max_jobs = 1;
        config.transcode.max_runtime_secs = 1;
        let app = Arc::new(App::from_config(config, 18200, 11900, &dir));
        let job = attach(
            app.clone(),
            job_spec(&dir, "timeout", vec!["sleep".into(), "30".into()]),
        )
        .unwrap();
        let started = Instant::now();
        wait_for_terminal_cleanup(&app, &job);
        assert!(matches!(job.state(), RemuxState::Failed(_)));
        assert!(started.elapsed() < Duration::from_secs(3));

        let idle = Arc::new(RemuxJob {
            detail_id: 44,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(true),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest: dir.join("idle.mp4"),
            part: dir.join("idle.mp4.part"),
            state: Mutex::new(RemuxState::Starting),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        });
        idle.attach_client();
        idle.detach_client(
            app.clone(),
            "idle".into(),
            false,
            Duration::from_millis(80),
            Duration::ZERO,
        );
        assert!(!idle.reconnect_grace_expired());
        std::thread::sleep(Duration::from_millis(20));
        idle.attach_client();
        std::thread::sleep(Duration::from_millis(80));
        assert!(!idle.reconnect_grace_expired());
        assert!(!idle.cancelled.load(Ordering::Acquire));
        idle.detach_client(
            app.clone(),
            "idle".into(),
            false,
            Duration::from_millis(20),
            Duration::ZERO,
        );
        std::thread::sleep(Duration::from_millis(30));
        assert!(idle.reconnect_grace_expired());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_cleanup_removes_intermediates_and_evicts_oldest_generated_output() {
        let dir = temp_dir("cache-cleanup");
        let key_a = "a".repeat(64);
        let key_b = "b".repeat(64);
        let old = dir.join(format!("1-hdr10-{key_a}.mp4"));
        let new = dir.join(format!("2-remux-{key_b}.mp4"));
        std::fs::write(&old, vec![1u8; 800]).unwrap();
        std::fs::write(&new, vec![2u8; 800]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();
        let stale_part = cache_part(&new);
        std::fs::write(&stale_part, b"partial").unwrap();
        maintain_transcode_cache(&dir, 900, 36_500, 0, &HashSet::new(), true).unwrap();
        assert!(!stale_part.exists());
        assert!(!old.exists(), "oldest completed output is LRU victim");
        assert!(new.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_maintenance_metrics_count_evictions_and_bytes() {
        let dir = temp_dir("cache-metrics");
        let app = Arc::new(App::from_config(
            crate::Config {
                cache_dir: Some(dir.display().to_string()),
                transcode: crate::TranscodeCfg {
                    enable: true,
                    cache_max_mb: 1,
                    ..crate::TranscodeCfg::default()
                },
                rescan_secs: 0,
                ..crate::Config::default()
            },
            18200,
            11900,
            &dir,
        ));
        let old = dir.join(format!("1-hdr10-{}.mp4", "a".repeat(64)));
        let new = dir.join(format!("2-remux-{}.mp4", "b".repeat(64)));
        std::fs::write(&old, vec![1u8; 800_000]).unwrap();
        std::fs::write(&new, vec![2u8; 800_000]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();

        maintain_app_cache(&app, &HashSet::new(), false).unwrap();
        let metrics = runtime_status(&app);
        assert_eq!(metrics.cache_maintenance_total, 1);
        assert_eq!(metrics.cache_maintenance_failures_total, 0);
        assert_eq!(metrics.cache_evicted_files_total, 1);
        assert_eq!(metrics.cache_evicted_bytes_total, 800_000);
        assert_eq!(metrics.cache_bytes, 800_000);
        assert!(!old.exists());
        assert!(new.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn same_key_restart_must_not_reuse_cancelled_producer() {
        let dir = temp_dir("review-same-key-restart");
        let app = test_app(&dir, 1);
        let mut first = job_spec(&dir, "review-restart", vec!["sleep".into(), "30".into()]);
        first.job_key = "web:42:review-restart".into();
        first.web_session_id = Some(9);
        first.web_request_id = Some(77);
        first.continue_after_disconnect = false;
        let mut second = first.clone();
        second.web_request_id = Some(78);
        let first_job = attach_started_long_running_job(app.clone(), first);
        assert!(cancel_web_request(&app, 42, Some(9), 77));
        let second_job = attach_for_client(app.clone(), second).unwrap();
        let reused = Arc::ptr_eq(&first_job, &second_job);
        let cancelled = second_job.cancelled.load(Ordering::Acquire);
        second_job.cancel();
        wait_for_terminal_cleanup(&app, &second_job);
        assert!(
            !reused && !cancelled,
            "new request reused cancelled producer: reused={reused}, cancelled={cancelled}"
        );
    }

    #[test]
    fn finished_cache_bytes_must_equal_actual_output() {
        let dir = temp_dir("review-cache-accounting");
        let app = test_app(&dir, 1);
        let key = "b".repeat(64);
        let mut spec = job_spec(&dir, &key, Vec::new());
        spec.dest = rusty_dlna_transcode::cache_dest_for_key(&dir, 42, RecodeAction::Hdr10, &key);
        spec.args = vec![
            "cp".into(),
            spec.src.as_os_str().to_owned(),
            cache_part(&spec.dest).into_os_string(),
        ];
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete);
        let bytes = job.dest.metadata().unwrap().len();
        assert_eq!(app.remux_metrics.cache_bytes.load(Ordering::Relaxed), bytes);
    }

    #[test]
    fn growing_file_open_survives_atomic_publication() {
        use tokio::io::AsyncReadExt;
        for mode in ["full", "range", "fragment", "playlist"] {
            let dir = temp_dir("growing-rename");
            let app = test_app(&dir, 1);
            let bytes = hls::tests::fixture();
            let job = growing_test_job(&dir, 73, &bytes);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap();
            runtime.block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let connect = tokio::net::TcpStream::connect(listener.local_addr().unwrap());
                let (client, accepted) = tokio::join!(connect, listener.accept());
                let mut client = client.unwrap();
                let (mut socket, _) = accepted.unwrap();
                let (release, wait_release) = std::sync::mpsc::channel();
                let (started, wait_started) = tokio::sync::oneshot::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    wait_release.recv_timeout(Duration::from_secs(5)).unwrap();
                });
                wait_started.await.unwrap();
                {
                    let stream = async {
                        match mode {
                            "range" => stream_growing(&app, &mut socket, &job, 6, Some(13)).await,
                            "fragment" => {
                                let req = HttpRequest::parse_headers(
                                    "GET /web/media/73.m4s?delivery=mse_segment&hls_offset=6&hls_length=8 HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes=1-4\r\n\r\n",
                                ).unwrap();
                                serve_hls_resource(&app, &mut socket, &req, &job, "video/iso.segment", false).await
                            }
                            "playlist" => {
                                let req = HttpRequest::parse_headers(
                                    "GET /web/media/73.m3u8?delivery=hls HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                                ).unwrap();
                                serve_fragment_playlist(&app, &mut socket, &req, &job, false, false).await
                            }
                            _ => stream_growing(&app, &mut socket, &job, 0, None).await,
                        }
                    };
                    tokio::pin!(stream);
                    tokio::select! {
                        result = &mut stream => panic!("file open unexpectedly finished: {result:?}"),
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {},
                    }
                    std::fs::rename(&job.part, &job.dest).unwrap();
                    job.transition(RemuxState::Complete);
                    release.send(()).unwrap();
                    blocker.await.unwrap();
                    stream.await.unwrap();
                }
                drop(socket);
                let mut received = Vec::new();
                client.read_to_end(&mut received).await.unwrap();
                match mode {
                    "full" => assert_eq!(received, bytes),
                    "range" => assert_eq!(received, bytes[6..14]),
                    "fragment" => assert_eq!(wire_body(&received), &bytes[7..11]),
                    "playlist" => {
                        let playlist = std::str::from_utf8(wire_body(&received)).unwrap();
                        assert!(playlist.contains("#EXTINF:"));
                        assert!(playlist.contains("delivery=hls_segment"));
                    }
                    _ => unreachable!(),
                }
            });
        }
    }

    #[test]
    fn same_key_handoff_waits_for_term_cleanup_and_gpu_permits() {
        for disconnect in [false, true] {
            let dir = temp_dir("same-key-cleanup");
            let app = test_app(&dir, 1);
            let mut first = job_spec(&dir, "same-key-cleanup", Vec::new());
            first.job_key = "web:42:same-key-cleanup".into();
            first.web_session_id = Some(9);
            first.web_request_id = Some(77);
            first.cacheable = false;
            first.continue_after_disconnect = false;
            first.ai_upscale_shader_file = Some(Arc::new(std::fs::File::open(&first.src).unwrap()));
            let key = first.job_key.clone();
            let mut second = first.clone();
            second.web_request_id = Some(78);
            second.args = vec![
                "cp".into(),
                second.src.as_os_str().to_owned(),
                cache_part(&second.dest).into_os_string(),
            ];
            let first_job = attach_started_long_running_job(app.clone(), first);
            if disconnect {
                first_job.detach_client(
                    app.clone(),
                    key.clone(),
                    false,
                    Duration::ZERO,
                    Duration::ZERO,
                );
                wait_until(Duration::from_secs(2), || {
                    first_job.cancelled.load(Ordering::Acquire)
                });
            } else {
                assert!(cancel_web_request(&app, 42, Some(9), 77));
            }
            let second_job = attach_for_client(app.clone(), second).unwrap();
            assert!(!Arc::ptr_eq(&first_job, &second_job));
            assert!(first_job.producer_finished.load(Ordering::Acquire));
            wait_until(Duration::from_secs(3), || {
                second_job.producer_finished.load(Ordering::Acquire)
            });
            assert_eq!(second_job.state(), RemuxState::Complete);
            assert_eq!(std::fs::read(&second_job.dest).unwrap(), b"source bytes");
            assert_eq!(app.jobs.in_use(), 0);
            assert_eq!(app.ai_upscale_jobs.in_use(), 0);
            // A stale owner's final registry removal/detach cannot claim the
            // replacement's entry or unlink its output.
            remove_job(&app, &key, &first_job);
            if !disconnect {
                first_job.detach_client(
                    app.clone(),
                    key.clone(),
                    false,
                    Duration::ZERO,
                    Duration::ZERO,
                );
            }
            assert!(Arc::ptr_eq(
                crate::lock_recover(&app.remuxes).get(&key).unwrap(),
                &second_job
            ));
            assert_eq!(std::fs::read(&second_job.dest).unwrap(), b"source bytes");
            second_job.detach_client(app.clone(), key, false, Duration::ZERO, Duration::ZERO);
            sweep_ephemeral_cleanups(&app, Instant::now(), true);
            assert!(crate::lock_recover(&app.remuxes).is_empty());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stopping_same_key_returns_bounded_busy_without_blocking_socket_runtime() {
        use tokio::io::AsyncReadExt;
        let dir = temp_dir("same-key-busy");
        let app = test_app(&dir, 1);
        let job = growing_test_job(&dir, 42, b"old output");
        job.cancel();
        job.producer_finished.store(false, Ordering::Release);
        let mut spec = job_spec(&dir, "same-key-busy", vec!["false".into()]);
        spec.job_key = "web:42:same-key-busy".into();
        crate::lock_recover(&app.remuxes).insert(spec.job_key.clone(), job.clone());
        let req =
            HttpRequest::parse_headers("GET /web/media/42.mp4 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_remux(&server_app, &mut socket, &req, spec)
                .await
                .unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "handoff blocked the socket runtime"
        );
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        assert!(std::str::from_utf8(&response)
            .unwrap()
            .contains("503 Service Unavailable"));
        job.producer_finished.store(true, Ordering::Release);
        crate::lock_recover(&app.remuxes).clear();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_output_survives_final_replacement_and_parallel_ranges() {
        let dir = temp_dir("pinned-replacement");
        let app = test_app(&dir, 1);
        let original: Vec<u8> = (0..200).collect();
        let job = growing_test_job(&dir, 42, &original);
        job.open_output().unwrap();
        std::fs::rename(&job.part, &job.dest).unwrap();
        job.transition(RemuxState::Complete);
        let replacement = dir.join("replacement");
        std::fs::write(&replacement, b"unrelated replacement output").unwrap();
        std::fs::rename(&replacement, &job.dest).unwrap();
        let mut readers = tokio::task::JoinSet::new();
        for start in 0..20 {
            let app = app.clone();
            let job = job.clone();
            let expected = original[start..start + 40].to_vec();
            readers.spawn(async move {
                let request = format!("GET /Transcode/42.mp4 HTTP/1.1\r\nHost: 127.0.0.1\r\nRange: bytes={start}-{}\r\n\r\n", start + 39);
                let response = growing_wire(app, job, &request, false).await;
                assert_eq!(wire_body(&response), expected);
            });
        }
        while let Some(result) = readers.join_next().await {
            result.unwrap();
        }
        assert_eq!(
            std::fs::read(&job.dest).unwrap(),
            b"unrelated replacement output"
        );
    }

    #[test]
    fn failed_publication_and_cancelled_output_do_not_reopen_other_paths() {
        let dir = temp_dir("failed-publication");
        let app = test_app(&dir, 1);
        let job = growing_test_job(&dir, 42, b"staging");
        job.open_output().unwrap();
        std::fs::create_dir(&job.dest).unwrap();
        finalize_remux(
            &app,
            &job,
            &job_spec(&dir, "failed", Vec::new()),
            Duration::from_secs(1),
            &None,
            false,
        );
        assert!(matches!(job.state(), RemuxState::Failed(_)));
        assert!(!job.part.exists());
        assert!(job.open_output().is_err());
        let other = growing_test_job(&dir, 43, b"cancelled staging");
        other.cancel();
        finalize_remux(
            &app,
            &other,
            &job_spec(&dir, "cancelled", Vec::new()),
            Duration::from_secs(1),
            &None,
            false,
        );
        assert_eq!(other.state(), RemuxState::Cancelled);
        std::fs::write(&other.dest, b"other generation").unwrap();
        assert!(other.open_output().is_err());
        assert_eq!(std::fs::read(&other.dest).unwrap(), b"other generation");
    }

    #[test]
    fn cache_gauge_tracks_concurrent_completion_failure_and_ephemeral_expiry() {
        let dir = temp_dir("cache-gauge-lifecycle");
        let app = test_app(&dir, 3);
        let existing = dir.join(format!("40-hdr10-{}.mp4", "a".repeat(64)));
        std::fs::write(&existing, vec![0u8; 600]).unwrap();
        let mut jobs = Vec::new();
        for (id, fill) in [(41, 'b'), (42, 'c')] {
            let key = fill.to_string().repeat(64);
            let mut spec = job_spec(&dir, &key, Vec::new());
            spec.dest =
                rusty_dlna_transcode::cache_dest_for_key(&dir, id, RecodeAction::Hdr10, &key);
            spec.args = vec![
                "cp".into(),
                spec.src.as_os_str().to_owned(),
                cache_part(&spec.dest).into_os_string(),
            ];
            jobs.push(attach(app.clone(), spec).unwrap());
        }
        let maintenance_app = app.clone();
        let maintenance = std::thread::spawn(move || {
            for _ in 0..10 {
                enforce_active_cache_limits(&maintenance_app).unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        for job in &jobs {
            wait_until(Duration::from_secs(3), || {
                job.producer_finished.load(Ordering::Acquire)
            });
            assert_eq!(job.state(), RemuxState::Complete);
        }
        maintenance.join().unwrap();
        let expected = 600
            + jobs
                .iter()
                .map(|job| job.dest.metadata().unwrap().len())
                .sum::<u64>();
        assert_eq!(runtime_status(&app).cache_bytes, expected);

        let key = "d".repeat(64);
        let mut failed = job_spec(&dir, &key, Vec::new());
        failed.dest = rusty_dlna_transcode::cache_dest_for_key(&dir, 43, RecodeAction::Hdr10, &key);
        failed.args = vec![
            "sh".into(),
            "-c".into(),
            format!(
                "dd if=/dev/zero of=\"$1\" bs={FIRST_BYTES} count=1 2>/dev/null; sleep 0.3; exit 1"
            )
            .into(),
            "failed-producer".into(),
            cache_part(&failed.dest).into_os_string(),
        ];
        let failed_job = attach(app.clone(), failed).unwrap();
        wait_until(Duration::from_secs(3), || {
            failed_job.state() == RemuxState::Growing
        });
        assert_eq!(runtime_status(&app).cache_bytes, expected + FIRST_BYTES);
        wait_until(Duration::from_secs(3), || {
            failed_job.producer_finished.load(Ordering::Acquire)
        });
        assert!(matches!(failed_job.state(), RemuxState::Failed(_)));
        assert_eq!(runtime_status(&app).cache_bytes, expected);

        let key = "e".repeat(64);
        let mut ephemeral = job_spec(&dir, &key, Vec::new());
        ephemeral.job_key = "web:44:ephemeral-gauge".into();
        ephemeral.web_session_id = Some(9);
        ephemeral.web_request_id = Some(77);
        ephemeral.cacheable = false;
        ephemeral.dest =
            rusty_dlna_transcode::cache_dest_for_key(&dir, 44, RecodeAction::Hdr10, &key);
        ephemeral.args = vec![
            "cp".into(),
            ephemeral.src.as_os_str().to_owned(),
            cache_part(&ephemeral.dest).into_os_string(),
        ];
        let ephemeral_key = ephemeral.job_key.clone();
        let ephemeral_job = attach_for_client(app.clone(), ephemeral).unwrap();
        wait_until(Duration::from_secs(3), || {
            ephemeral_job.producer_finished.load(Ordering::Acquire)
        });
        assert_eq!(runtime_status(&app).cache_bytes, expected + 12);
        ephemeral_job.detach_client(
            app.clone(),
            ephemeral_key,
            false,
            Duration::ZERO,
            Duration::ZERO,
        );
        sweep_ephemeral_cleanups(&app, Instant::now(), true);
        assert_eq!(runtime_status(&app).cache_bytes, expected);

        let key = "f".repeat(64);
        let mut rename_failure = job_spec(&dir, &key, Vec::new());
        rename_failure.dest =
            rusty_dlna_transcode::cache_dest_for_key(&dir, 45, RecodeAction::Hdr10, &key);
        std::fs::create_dir(&rename_failure.dest).unwrap();
        rename_failure.args = vec![
            "cp".into(),
            rename_failure.src.as_os_str().to_owned(),
            cache_part(&rename_failure.dest).into_os_string(),
        ];
        let failed_job = attach(app.clone(), rename_failure).unwrap();
        wait_until(Duration::from_secs(3), || {
            failed_job.producer_finished.load(Ordering::Acquire)
        });
        assert!(matches!(failed_job.state(), RemuxState::Failed(_)));
        assert!(!failed_job.part.exists());
        assert_eq!(runtime_status(&app).cache_bytes, expected);
    }

    #[test]
    fn output_metadata_does_not_wait_for_index_lock() {
        let dir = temp_dir("pinned-cursor-contention");
        let job = growing_test_job(&dir, 42, b"output");
        job.open_output().unwrap();
        let index_job = job.clone();
        let (locked, acquired) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _index = crate::lock_recover(&index_job.hls_index);
            locked.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        acquired.recv_timeout(Duration::from_secs(1)).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert_eq!(
                tokio::time::timeout(Duration::from_millis(500), current_len_async(&job))
                    .await
                    .unwrap()
                    .unwrap(),
                6
            );
            release.send(()).unwrap();
        });
        holder.join().unwrap();
    }
    #[test]
    fn cancellation_before_helper_spawn_releases_same_key_for_replacement() {
        let dir = temp_dir("cancel-before-spawn");
        let app = test_app(&dir, 1);
        let job = growing_test_job(&dir, 42, b"old staging");
        job.producer_finished.store(false, Ordering::Release);
        job.transition(RemuxState::Starting);
        job.cancel();
        let marker = dir.join("old-helper-started");
        let mut spec = job_spec(&dir, "cancel-before-spawn", Vec::new());
        spec.job_key = "web:42:cancel-before-spawn".into();
        spec.web_session_id = Some(9);
        spec.web_request_id = Some(77);
        spec.dest = job.dest.clone();
        spec.args = vec!["touch".into(), marker.as_os_str().to_owned()];
        let mut replacement = spec.clone();
        replacement.web_request_id = Some(78);
        replacement.args = vec![
            "cp".into(),
            replacement.src.as_os_str().to_owned(),
            cache_part(&replacement.dest).into_os_string(),
        ];
        crate::lock_recover(&app.remuxes).insert(spec.job_key.clone(), job.clone());
        let helper = app.helpers.try_acquire().unwrap();
        let permit = app.jobs.try_acquire().unwrap();
        spawn_ffmpeg(app.clone(), spec, job.clone(), helper, permit, None);
        let newer = attach_for_client(app.clone(), replacement).unwrap();
        assert!(!Arc::ptr_eq(&job, &newer));
        wait_until(Duration::from_secs(3), || {
            newer.producer_finished.load(Ordering::Acquire)
        });
        assert_eq!(job.state(), RemuxState::Cancelled);
        assert!(!marker.exists());
        assert_eq!(newer.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(&newer.dest).unwrap(), b"source bytes");
        assert_eq!(app.jobs.in_use(), 0);
    }
    #[test]
    fn status_before_portable_fallback_does_not_pin_failed_primary_bytes() {
        let dir = temp_dir("status-before-fallback");
        let app = test_app(&dir, 1);
        let release = dir.join("release-primary");
        let mut spec = job_spec(&dir, "status-before-fallback", Vec::new());
        spec.job_key = "web:42:status-before-fallback".into();
        spec.web_request_id = Some(77);
        spec.args = vec![
            "sh".into(),
            "-c".into(),
            "printf failed > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; exit 1".into(),
            "primary".into(),
            cache_part(&spec.dest).into_os_string(),
            release.as_os_str().to_owned(),
        ];
        spec.fallback_args = Some(vec![
            "cp".into(),
            spec.src.as_os_str().to_owned(),
            cache_part(&spec.dest).into_os_string(),
        ]);
        let job = attach(app.clone(), spec).unwrap();
        wait_until(Duration::from_secs(3), || {
            job.part
                .metadata()
                .is_ok_and(|metadata| metadata.len() == 6)
        });
        assert_eq!(job.state(), RemuxState::Starting);
        assert_eq!(web_job_produced_seconds(&app, 42, Some(77)), None);
        std::fs::write(&release, b"release").unwrap();
        wait_until(Duration::from_secs(3), || {
            job.producer_finished.load(Ordering::Acquire)
        });
        assert_eq!(job.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(&job.dest).unwrap(), b"source bytes");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let received = runtime.block_on(growing_wire(
            app,
            job,
            "GET /Transcode/42.mp4 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            false,
        ));
        assert_eq!(wire_body(&received), b"source bytes");
    }

    #[test]
    fn pinned_output_closes_fallback_even_before_first_bytes() {
        let dir = temp_dir("pinned-before-fallback");
        let app = test_app(&dir, 1);
        let release = dir.join("release-primary");
        let fallback_marker = dir.join("fallback-started");
        let mut spec = job_spec(&dir, "pinned-before-fallback", Vec::new());
        spec.args = vec![
            "sh".into(),
            "-c".into(),
            "printf failed > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; exit 1".into(),
            "primary".into(),
            cache_part(&spec.dest).into_os_string(),
            release.as_os_str().to_owned(),
        ];
        spec.fallback_args = Some(vec!["touch".into(), fallback_marker.as_os_str().to_owned()]);
        let job = attach(app.clone(), spec).unwrap();
        wait_until(Duration::from_secs(3), || {
            job.part
                .metadata()
                .is_ok_and(|metadata| metadata.len() == 6)
        });
        let pinned = job.open_output().unwrap();
        assert_eq!(pinned.metadata().unwrap().len(), 6);
        std::fs::write(&release, b"release").unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert!(matches!(job.state(), RemuxState::Failed(_)));
        assert!(!fallback_marker.exists());
        assert_eq!(
            pinned.metadata().unwrap().len(),
            6,
            "old output is never replaced underneath its readers"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unpinned_output_above_first_bytes_may_retry_before_any_exposure() {
        let dir = temp_dir("large-unpinned-fallback");
        let app = test_app(&dir, 1);
        let release = dir.join("release-primary");
        let mut spec = job_spec(&dir, "large-unpinned-fallback", Vec::new());
        spec.args = vec![
            "sh".into(),
            "-c".into(),
            "head -c 32768 /dev/zero > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; exit 1"
                .into(),
            "primary".into(),
            cache_part(&spec.dest).into_os_string(),
            release.as_os_str().to_owned(),
        ];
        spec.fallback_args = Some(vec![
            "cp".into(),
            spec.src.as_os_str().to_owned(),
            cache_part(&spec.dest).into_os_string(),
        ]);
        let job = attach(app.clone(), spec).unwrap();
        wait_until(Duration::from_secs(3), || {
            job.state() == RemuxState::Growing
        });
        assert!(crate::lock_recover(&job.output).is_none());
        std::fs::write(&release, b"release").unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(&job.dest).unwrap(), b"source bytes");
        let _ = std::fs::remove_dir_all(dir);
    }
}
