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
    cache_is_fresh_for_key, cache_part, run_remux_p8_with_toolchain_observed,
    write_cache_stamp_for_key, BrowserOutputOptions, RecodeAction, RemuxP8Error, RemuxP8Input,
    TranscodeCacheIdentity, TranscodePlan,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::App;

mod cache;
mod hls;

pub(crate) use cache::maintain_transcode_cache;
use cache::{enforce_active_cache_limits, maintain_app_cache};

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
const WEB_SESSION_RETENTION: Duration = Duration::from_secs(10 * 60);
const WEB_PREPARATION_RETENTION: Duration = Duration::from_secs(2 * 60);
// A replacement browser rendition must not fail admission while the producer
// it just cancelled is still completing the bounded TERM-to-KILL handoff.
const WEB_SUPERSEDED_JOB_HANDOFF: Duration = Duration::from_secs(2);
const MAX_WEB_TRANSCODE_PREPARATIONS: usize = 64;
const WEB_REQUEST_CANCELLED: &str = "web playback request superseded";
const REMUX_CANCELLED: &str = "remux cancelled";
pub(crate) const MAX_MSE_FRAGMENT_CURSOR: usize = 20_000;

#[cfg(test)]
type P8TestRunner = fn(
    &Path,
    Instant,
    &AtomicBool,
    &mut dyn FnMut() -> Result<(), String>,
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
    observer: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), RemuxP8Error> {
    #[cfg(test)]
    if let Some(runner) = crate::lock_recover(p8_test_runners()).remove(part) {
        return runner(part, deadline, cancelled, observer);
    }
    let toolchain = spec.profile8_toolchain.as_ref().ok_or_else(|| {
        RemuxP8Error::Pipeline("Profile-8 job is missing its toolchain snapshot".into())
    })?;
    if let Some(source) = spec.source_file.as_deref() {
        run_remux_p8_with_toolchain_observed(
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
        run_remux_p8_with_toolchain_observed(
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
struct AtomicDurationMetric {
    count: AtomicU64,
    sum_ms: AtomicU64,
    max_ms: AtomicU64,
}

impl AtomicDurationMetric {
    fn record(&self, elapsed: Duration) {
        let millis = rusty_dlna_helper::duration_millis_saturating(elapsed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(millis, Ordering::Relaxed);
        self.max_ms.fetch_max(millis, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DurationMetric {
        DurationMetric {
            count: self.count.load(Ordering::Relaxed),
            sum_ms: self.sum_ms.load(Ordering::Relaxed),
            max_ms: self.max_ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DurationMetric {
    pub count: u64,
    pub sum_ms: u64,
    pub max_ms: u64,
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
    fn add_web_request(&self, session_id: Option<u64>, request_id: Option<u64>) -> Option<u64> {
        let replaced = session_id
            .zip(request_id)
            .and_then(|(session_id, request_id)| {
                crate::lock_recover(&self.web_sessions).insert(session_id, request_id)
            });
        let mut request_ids = crate::lock_recover(&self.web_request_ids);
        if let Some(replaced) = replaced.filter(|replaced| Some(*replaced) != request_id) {
            request_ids.remove(&replaced);
        }
        if let Some(request_id) = request_id {
            request_ids.insert(request_id);
        }
        replaced
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
        crate::lock_recover(&self.web_request_ids).remove(&request_id)
    }

    fn remove_web_session(&self, session_id: u64) -> Option<u64> {
        let request_id = crate::lock_recover(&self.web_sessions).remove(&session_id)?;
        crate::lock_recover(&self.web_request_ids).remove(&request_id);
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
        }
        expired
    }
}

fn remove_ephemeral_output(app: &App, job: &RemuxJob) {
    let bytes = job
        .dest
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if std::fs::remove_file(&job.dest).is_ok() {
        app.remux_metrics.subtract_cache_bytes(bytes);
    }
    let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&job.dest));
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
            remove_ephemeral_output(app, job);
        }
        tracing::debug!(
            id = job.detail_id,
            cacheable = job.cacheable,
            "expired reconnectable web job"
        );
        false
    });
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
        finish_job(&self.app, &self.job_key, &self.job);
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
            let _completion_guard = RemuxCompletionGuard::new(
                app.clone(),
                spec.job_key.clone(),
                job.clone(),
            );
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
                let mut next_cache_check = Instant::now();
                let mut observe_cache_pressure = || {
                    let now = Instant::now();
                    if now < next_cache_check {
                        return Ok(());
                    }
                    enforce_active_cache_limits(&app).map(|_| ()).map_err(|error| {
                        error.to_string()
                    })?;
                    next_cache_check = now + Duration::from_secs(1);
                    Ok(())
                };
                let p8_result = run_profile8_pipeline(
                    &spec,
                    &part,
                    &p8,
                    deadline,
                    &job.cancelled,
                    &mut observe_cache_pressure,
                );
                match p8_result {
                    Ok(()) => {
                        finalize_remux(&app, &job, id, &dest, &part, verify_timeout, true);
                        if job.is_complete() && spec.cacheable {
                            if let Err(error) =
                                write_cache_stamp_for_key(&dest, &spec.cache_key)
                            {
                                tracing::warn!(id, dest = %dest.display(), %error, "cache stamp write failed");
                            }
                        }
                        return;
                    }
                    Err(RemuxP8Error::Observer(error)) => {
                        cleanup_intermediates(&part);
                        // The failed pressure pass accounted the bytes it saw;
                        // refresh after cleanup so the gauge reflects disk.
                        let _ = enforce_active_cache_limits(&app);
                        job.transition(RemuxState::Failed(format!(
                            "transcode cache limits: {error}"
                        )));
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
                        tracing::warn!(id, dest = %dest.display(), "{e}; falling back to hdr10");
                        let _ = std::fs::remove_file(&part);
                        job.transition(RemuxState::Starting);
                        output_fell_back = true;
                    }
                }
            }
            let mut args = &spec.args;
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
            let primary_failed = matches!(&result, Ok((status, _)) if !status.success());
            if primary_failed
                && !job.cancelled.load(Ordering::Acquire)
                && current_len(&job) < FIRST_BYTES
            {
                if let Some(fallback) = spec.fallback_args.as_ref() {
                    tracing::warn!(
                        id,
                        dest = %dest.display(),
                        "negotiated compatible output failed; retrying with portable encoders"
                    );
                    cleanup_intermediates(&part);
                    job.transition(RemuxState::Starting);
                    args = fallback;
                    output_fell_back = true;
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
                    let production_ffmpeg = args.first().is_some_and(|executable| {
                        Path::new(executable)
                            .file_name()
                            .is_some_and(|name| name == "ffmpeg")
                    });
                    finalize_remux(
                        &app,
                        &job,
                        id,
                        &dest,
                        &part,
                        verify_timeout,
                        production_ffmpeg,
                    );
                    if job.is_complete() && spec.cacheable && !output_fell_back {
                        if let Err(error) = write_cache_stamp_for_key(&dest, &spec.cache_key) {
                            tracing::warn!(id, dest = %dest.display(), %error, "cache stamp write failed");
                        }
                    }
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
                        let _ = enforce_active_cache_limits(&app);
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!(id = id_err, %e, "remux thread spawn failed");
        job_err.transition(RemuxState::Failed(format!("thread: {e}")));
        finish_job(&app_err, &key_err, &job_err);
    }
}

fn run_ffmpeg_growing(
    args: &[std::ffi::OsString],
    source_file: Option<&std::fs::File>,
    ai_upscale_shader_file: Option<&std::fs::File>,
    verified_ffmpeg: Option<&rusty_dlna_transcode::VerifiedExecutable>,
    job: &RemuxJob,
    deadline: Instant,
    app: &App,
) -> Result<(std::process::ExitStatus, String), String> {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome, SupervisionError,
    };
    use std::ops::ControlFlow;

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
    command.args(&args[1..]);
    let mut runner = SupervisedCommand::new(&mut command)
        .capture_stderr(CaptureConfig::new(64 * 1024, CaptureRetention::Tail));
    if let Some(source) = source_file {
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
    let mut next_cache_check = Instant::now();
    let outcome = runner.run_until(deadline, POLL, || {
        if job.cancelled.load(Ordering::Acquire) {
            return ControlFlow::Break(Stop::Cancelled);
        }
        if job.reconnect_grace_expired() {
            job.cancel();
            return ControlFlow::Break(Stop::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return ControlFlow::Break(Stop::Deadline);
        }
        let mut cache_checked = false;
        if now >= next_cache_check {
            if let Err(error) = enforce_active_cache_limits(app) {
                return ControlFlow::Break(Stop::Cache(error.to_string()));
            }
            next_cache_check = now + Duration::from_secs(1);
            cache_checked = true;
        }
        let len = job
            .part
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if len != last_len {
            last_len = len;
            let becoming_playable =
                len >= FIRST_BYTES && matches!(job.state(), RemuxState::Starting);
            // Do not expose the first playable fragment based on a cache pass
            // that happened before the child produced it. Fast helpers can
            // otherwise finish an oversized output inside the one-second
            // maintenance interval and let a waiter open bytes that final
            // admission will reject.
            if becoming_playable && !cache_checked {
                if let Err(error) = enforce_active_cache_limits(app) {
                    return ControlFlow::Break(Stop::Cache(error.to_string()));
                }
                next_cache_check = now + Duration::from_secs(1);
            }
            if becoming_playable {
                job.transition(RemuxState::Growing);
            } else {
                job.notify_growth();
            }
        }
        ControlFlow::Continue(())
    });
    match outcome {
        Ok(SupervisedOutcome::Exited(output)) => Ok((
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )),
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

fn finalize_remux(
    app: &App,
    job: &RemuxJob,
    id: i64,
    dest: &Path,
    part: &Path,
    verify_timeout: Duration,
    verify: bool,
) {
    let n = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if n == 0 {
        let msg = "ffmpeg produced empty remux".to_string();
        tracing::error!(id, dest = %dest.display(), "{msg}");
        job.transition(RemuxState::Failed(msg));
        let _ = std::fs::remove_file(part);
        return;
    }
    if verify {
        if let Err(error) = verify_finished_output(part, verify_timeout, &job.cancelled) {
            let message = format!("remux output verification failed: {error}");
            tracing::error!(id, dest = %dest.display(), %message);
            job.transition(RemuxState::Failed(message));
            cleanup_intermediates(part);
            return;
        }
    }
    // The completed bytes are still a protected staging artifact here. Check
    // quota and the real minimum-free reserve before rename makes the output
    // observable as Complete; otherwise a waiter could open a file that the
    // producer immediately deletes during post-publication maintenance.
    if let Err(error) = enforce_active_cache_limits(app) {
        cleanup_intermediates(part);
        // The failed pass accounted the staging bytes. Refresh after cleanup
        // so the exported cache gauge reflects the bytes still on disk.
        let _ = enforce_active_cache_limits(app);
        job.transition(RemuxState::Failed(format!(
            "transcode cache limits: {error}"
        )));
        return;
    }
    if let Err(e) = std::fs::rename(part, dest) {
        let msg = format!("remux rename: {e}");
        tracing::error!(id, dest = %dest.display(), "{msg}");
        job.transition(RemuxState::Failed(msg));
        cleanup_intermediates(part);
        return;
    }
    tracing::info!(id, dest = %dest.display(), bytes = n, "remux job done");
    app.remux_metrics
        .cache_bytes
        .fetch_add(n, Ordering::Relaxed);
    job.transition(RemuxState::Complete);
}

fn verify_finished_output(
    path: &Path,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome, SupervisionError,
    };
    use std::ops::ControlFlow;

    let mut command = std::process::Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=format_name",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path);
    let runner = SupervisedCommand::new(&mut command)
        .capture_stderr(CaptureConfig::new(64 * 1024, CaptureRetention::Head));
    let deadline = Instant::now() + timeout;
    let outcome = runner.run_until(deadline, Duration::from_millis(20), || {
        if cancelled.load(Ordering::Acquire) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    match outcome {
        Ok(SupervisedOutcome::Exited(output)) if output.status.success() => Ok(()),
        Ok(SupervisedOutcome::Exited(output)) => {
            let diagnostics = String::from_utf8_lossy(&output.stderr);
            Err(if diagnostics.trim().is_empty() {
                format!("ffprobe exited {}", output.status)
            } else {
                diagnostics.trim().to_string()
            })
        }
        Ok(SupervisedOutcome::NotStarted { .. } | SupervisedOutcome::Stopped { .. }) => {
            Err("cancelled".into())
        }
        Ok(SupervisedOutcome::Deadline { .. }) => Err("ffprobe verification timed out".into()),
        Err(SupervisionError::Spawn(error) | SupervisionError::Wait(error)) => {
            Err(error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
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
    let web = spec.job_key.starts_with("web:");
    let mut newer_generation = false;
    let mut superseded_producer = false;
    let mut superseded_ai_producer = false;
    let playback_sessions = if web {
        if let (Some(session_id), Some(request_id)) = (spec.web_session_id, spec.web_request_id) {
            let mut sessions = crate::lock_recover(&app.web_playback_sessions);
            sessions.retain(|_, state| state.at.elapsed() < WEB_SESSION_RETENTION);
            if sessions.len() >= 1024 && !sessions.contains_key(&session_id) {
                if let Some(oldest) = sessions
                    .iter()
                    .min_by_key(|(_, state)| state.at)
                    .map(|(session_id, _)| *session_id)
                {
                    sessions.remove(&oldest);
                }
            }
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
    if let Some(job) = map.get(&spec.job_key) {
        if job.err().is_none() {
            if new_web_generation || !web {
                if let Some(replaced) =
                    job.add_web_request(spec.web_session_id, spec.web_request_id)
                {
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
                job.attach_client();
            }
            return Ok(job.clone());
        }
        map.remove(&spec.job_key);
    }
    // Existing producers and their staging/final artifacts are already in the
    // protected set. Resource reattachments above therefore need neither a
    // cache-directory scan nor a quota decision. New producers and reopened
    // completed outputs still pass the normal bounded cache gate.
    let protected = cache::active_artifacts(map.values());
    maintain_app_cache(&app, &protected, false)
        .map_err(|error| format!("transcode cache limits: {error}"))?;
    if spec.cacheable && cache_is_fresh_for_key(&spec.dest, &spec.cache_key) {
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
        });
        if register_client {
            job.attach_client();
            map.insert(spec.job_key.clone(), job.clone());
        }
        return Ok(job);
    }
    if spec.dest.is_file() {
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
    }
    // A cache hit returned above. Any remaining stamp is stale or orphaned and
    // must not make newly produced fallback bytes fresh under the primary key.
    let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&spec.dest));
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
    });
    map.insert(spec.job_key.clone(), job.clone());
    if register_client {
        job.attach_client();
    }
    drop(map);
    spawn_ffmpeg(
        app,
        spec,
        job.clone(),
        helper_permit,
        job_permit,
        ai_upscale_permit,
    );
    Ok(job)
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

fn keep_web_request_alive_for(
    app: &App,
    detail_id: i64,
    session_id: Option<u64>,
    request_id: u64,
    lease: Duration,
) -> bool {
    let playback_sessions = session_id.and_then(|session_id| {
        let mut sessions = crate::lock_recover(&app.web_playback_sessions);
        sessions.retain(|_, state| state.at.elapsed() < WEB_SESSION_RETENTION);
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
        sessions.retain(|_, state| state.at.elapsed() < WEB_SESSION_RETENTION);
        if sessions.len() >= 1024 && !sessions.contains_key(&session_id) {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, state)| state.at)
                .map(|(session_id, _)| *session_id)
            {
                sessions.remove(&oldest);
            }
        }
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

pub async fn wait_ready(job: &RemuxJob) -> Result<PathBuf, String> {
    let mut deadline = Instant::now() + FIRST_WAIT;
    loop {
        let notified = job.changed.notified();
        match job.state() {
            RemuxState::Complete => {
                if job.dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
                    return Ok(job.dest.clone());
                }
                return Err("completed remux is missing or empty".into());
            }
            RemuxState::Growing => {
                if job
                    .part
                    .metadata()
                    .map(|m| m.len() >= FIRST_BYTES)
                    .unwrap_or(false)
                {
                    return Ok(job.part.clone());
                }
            }
            RemuxState::Failed(error) => return Err(error),
            RemuxState::Cancelled => return Err(REMUX_CANCELLED.into()),
            RemuxState::Preprocessing => {
                // Dolby Vision conversion does not expose playable bytes.
                // Start the first-fragment deadline only after fallback begins.
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

pub async fn serve_remux(
    app: &Arc<App>,
    sock: &mut tokio::net::TcpStream,
    req: &HttpRequest,
    spec: RemuxJobSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let head = req.method.eq_ignore_ascii_case("HEAD");
    let job = match attach_for_client(app.clone(), spec.clone()) {
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
            crate::socket_write_http_response(app, sock, &err).await?;
            return Ok(());
        }
    };
    let _client = RemuxClient {
        app: app.clone(),
        job: job.clone(),
        job_key: spec.job_key.clone(),
        continue_after_disconnect: spec.continue_after_disconnect,
    };
    let path = match wait_ready(&job).await {
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
            crate::socket_write_http_response(app, sock, &err).await?;
            return Ok(());
        }
    };
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
    let finished = path == job.dest
        && job.dest.is_file()
        && job.dest.metadata().map(|m| m.len() > 0).unwrap_or(false);
    if finished {
        return serve_finished(app, sock, req, &job.dest, spec.mime, head).await;
    }
    serve_growing(app, sock, req, &job, spec.mime, head).await
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
                crate::socket_write_http_response(app, sock, &response).await?;
                return Ok(());
            }
            _ => {}
        }
        let complete = job.is_complete();
        let path = current_path(job);
        let indexed = {
            let mut index = crate::lock_recover(&job.hls_index);
            index.update(&path, complete).and_then(|()| {
                if media_source {
                    index
                        .has_mse_fragments_after(mse_after, complete)
                        .then(|| index.mse_playlist_after(&init_uri, &segment_uri, mse_after))
                        .transpose()
                } else if all_fragments_independent {
                    index
                        .has_independent_startup_buffer(complete)
                        .then(|| index.independent_fragment_playlist(&init_uri, &segment_uri))
                        .transpose()
                } else {
                    index
                        .has_startup_buffer(complete)
                        .then(|| index.playlist(&init_uri, &segment_uri))
                        .transpose()
                }
            })
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
                        crate::socket_write_http_response(app, sock, &response).await?;
                        return Ok(());
                    }
                    _ => {}
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let _ = tokio::time::timeout(remaining.min(POLL), notified).await;
            }
            Ok(None) => return Err("transcode produced no complete media segment".into()),
            Err(error) => {
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
                    crate::socket_write_http_response(app, sock, &response).await?;
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
    response.set("Cache-Control", "no-store");
    response.set("Content-Length", playlist.len());
    if !head {
        response.body = playlist.into_bytes();
    }
    crate::socket_write_http_response(app, sock, &response).await?;
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
            crate::socket_write_http_response(app, sock, &response).await?;
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
        crate::socket_write_http_response(app, sock, &response).await?;
        return Ok(());
    }
    if current_len(job) < slice_end {
        return Err("HLS resource is outside the compatible output".into());
    }
    let range = match req.header("Range") {
        None => None,
        Some(value) => match parse_byte_range(value, length) {
            Ok(range) => range,
            Err(RangeError::Invalid) => {
                let err = HttpResponse::html(400, "Bad Request", "invalid HLS resource range");
                crate::socket_write_http_response(app, sock, &err).await?;
                return Ok(());
            }
            Err(RangeError::Unsatisfiable) => {
                let mut err = HttpResponse::html(
                    416,
                    "Requested Range Not Satisfiable",
                    "range past HLS resource",
                );
                err.set("Content-Range", format!("bytes */{length}"));
                crate::socket_write_http_response(app, sock, &err).await?;
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
        crate::socket_write_http_response(app, sock, &response).await?;
        return Ok(());
    }
    if !crate::socket_write_http_response(app, sock, &response).await? {
        return Ok(());
    }
    let path = current_path(job);
    if let Err(error) = crate::stream_file_range(app, sock, &path, start, end).await {
        if job.state() == RemuxState::Cancelled {
            tracing::debug!(
                id = job.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "superseded compatible media resource closed during cleanup"
            );
            return Ok(());
        }
        return Err(error.into());
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
    dest: &Path,
    mime: &str,
    head: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let size = dest.metadata()?.len();
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(dest) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
    let range = match req.header("Range") {
        None => None,
        Some(v) => match parse_byte_range(v, size) {
            Ok(r) => r,
            Err(RangeError::Invalid) => {
                tracing::error!(path = %req.path, range = v, "invalid Range");
                let err = HttpResponse::html(400, "Bad Request", "invalid range");
                crate::socket_write_http_response(app, sock, &err).await?;
                return Ok(());
            }
            Err(RangeError::Unsatisfiable) => {
                tracing::error!(path = %req.path, range = v, size, "range past remux EOF");
                let mut err =
                    HttpResponse::html(416, "Requested Range Not Satisfiable", "range past EOF");
                err.set("Content-Range", format!("bytes */{size}"));
                crate::socket_write_http_response(app, sock, &err).await?;
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
    if head {
        crate::socket_write_http_response(app, sock, &resp).await?;
        return Ok(());
    }
    if !crate::socket_write_http_response(app, sock, &resp).await? {
        return Ok(());
    }
    crate::stream_file_range(app, sock, dest, start, end).await?;
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
                crate::socket_write_http_response(app, sock, &err).await?;
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
        crate::socket_write_http_response(app, sock, &resp).await?;
        return Ok(());
    }
    let have = current_len(job);
    if start >= have {
        let mut resp = HttpResponse::html(
            416,
            "Requested Range Not Satisfiable",
            "range past remux output",
        );
        resp.set("Content-Range", format!("bytes */{have}"));
        crate::socket_write_http_response(app, sock, &resp).await?;
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
    let valid_wire = crate::socket_write_http_response(app, sock, &resp).await?;
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
    let valid_wire = crate::socket_write_http_response(app, sock, &resp).await?;
    if head || !valid_wire {
        return Ok(());
    }
    stream_growing(app, sock, job, 0, None).await
}

fn current_len(job: &RemuxJob) -> u64 {
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

async fn wait_offset(job: &RemuxJob, need: u64) -> Result<(), String> {
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
        if current_len(job) >= need || state == RemuxState::Complete && current_len(job) > 0 {
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
    let mut path = current_path(job);
    let mut f = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) if job.web && job.state() == RemuxState::Cancelled => {
            tracing::debug!(
                id = job.detail_id,
                dest = %path.display(),
                "superseded compatible media stream closed during cleanup"
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut pos = start;
    let mut buf = vec![0u8; 64 * 1024];
    let mut sent = 0u64;
    loop {
        let notified = job.changed.notified();
        if let Some(e) = end {
            if pos > e {
                break;
            }
        }
        if let Some(err) = job.err() {
            if sent == 0 {
                if job.web && err == REMUX_CANCELLED {
                    tracing::debug!(
                        id = job.detail_id,
                        dest = %path.display(),
                        "superseded compatible media stream cancelled before response bytes"
                    );
                    return Ok(());
                }
                tracing::error!(dest = %path.display(), "{err}");
                return Err(err.into());
            }
            break;
        }
        let size = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => {
                let next = current_path(job);
                if next != path && next.is_file() {
                    path = next;
                    f = tokio::fs::File::open(&path).await?;
                    f.seek(std::io::SeekFrom::Start(pos)).await?;
                    continue;
                }
                if job.is_complete() {
                    break;
                }
                0
            }
        };
        if pos < size {
            let want = match end {
                Some(e) => (e + 1).saturating_sub(pos).min(size - pos),
                None => size - pos,
            };
            let n = std::cmp::min(buf.len(), want as usize);
            let got = f.read(&mut buf[..n]).await?;
            if got == 0 {
                notified.await;
                continue;
            }
            if let Err(e) = crate::socket_write_all(app, sock, &buf[..got]).await {
                if sent == 0 {
                    tracing::error!(dest = %path.display(), %e, "client dropped before remux bytes");
                    return Err(e.into());
                }
                return Ok(());
            }
            pos += got as u64;
            sent += got as u64;
            continue;
        }
        if job.is_complete() || job.dest.is_file() && pos >= current_len(job) {
            break;
        }
        notified.await;
    }
    if sent == 0 {
        tracing::error!(dest = %path.display(), "remux stream sent 0 bytes");
    }
    Ok(())
}

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

    struct TempDir(PathBuf);

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

    fn temp_dir(label: &str) -> TempDir {
        TempDir::new(label)
    }

    fn test_app(dir: &Path, max_jobs: u32) -> Arc<App> {
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

    fn job_spec(dir: &Path, key: &str, command: Vec<String>) -> RemuxJobSpec {
        let src = dir.join("source.mkv");
        if !src.exists() {
            std::fs::write(&src, b"source bytes").unwrap();
        }
        RemuxJobSpec {
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

    fn wait_for_terminal_cleanup(app: &App, job: &RemuxJob) {
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

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
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
        });
        crate::lock_recover(&app.remuxes).insert(key.clone(), job.clone());
        (key, job)
    }

    fn growing_test_job(dir: &Path, id: i64, bytes: &[u8]) -> Arc<RemuxJob> {
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
    fn repeated_web_resources_reuse_the_active_job_without_recounting_the_generation() {
        let dir = temp_dir("web-resource-reattach");
        let app = test_app(&dir, 1);
        let (job_key, first) = completed_ephemeral_job(&app, &dir, 42);
        first.add_web_request(Some(9), Some(77));
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
        enforce_active_cache_limits(&app).unwrap();
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
            .open(&dest)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();
        std::fs::write(&victim, vec![3u8; 600_000]).unwrap();
        enforce_active_cache_limits(&app).unwrap();
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
        enforce_active_cache_limits(&app).unwrap();
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
        });

        finalize_remux(&app, &job, 42, &dest, &part, Duration::ZERO, false);

        let RemuxState::Failed(error) = job.state() else {
            panic!("rename failure must fail the job");
        };
        assert!(error.starts_with("remux rename: "));
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
        observer: &mut dyn FnMut() -> Result<(), String>,
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
                match observer() {
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
        let mut observer = || Ok(());
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finished_and_growing_transcode_head_emit_headers_only() {
        use tokio::io::AsyncReadExt;

        let dir = temp_dir("head-wire");
        let app = test_app(&dir, 1);
        let dest = dir.join("finished.mp4");
        std::fs::write(&dest, b"finished-transcode-bytes").unwrap();
        let finished_req = HttpRequest::parse_headers(
            "HEAD /Transcode/42.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-7\r\n\r\n",
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server_dest = dest.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_finished(
                &server_app,
                &mut socket,
                &finished_req,
                &server_dest,
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
    fn corrupt_finished_output_is_rejected_before_publish() {
        if std::process::Command::new("ffprobe")
            .arg("-version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        let dir = temp_dir("verify-corrupt");
        let corrupt = dir.join("corrupt.mp4.part");
        std::fs::write(&corrupt, b"not an mp4").unwrap();
        let cancelled = AtomicBool::new(false);
        assert!(verify_finished_output(&corrupt, Duration::from_secs(2), &cancelled).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
