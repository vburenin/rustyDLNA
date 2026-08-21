//! One background remux per title. Concurrent GETs share one producer; each
//! route decides whether its producer may outlive the last HTTP reader.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusty_dlna_http::{
    live_transcode_response, media_response, now_imf_date, parse_byte_range, parse_open_range,
    HttpRequest, HttpResponse, RangeError, RemuxAudio, RemuxJobSpec,
};
use rusty_dlna_transcode::{
    cache_is_fresh_for_key, cache_part, run_remux_p8_controlled, run_remux_p8_file_controlled,
    write_cache_stamp_for_key, RecodeAction, TranscodePlan,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::App;

const FIRST_BYTES: u64 = 16 * 1024;
const FIRST_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
const CHILD_TERM_GRACE: Duration = Duration::from_millis(200);
// Chromium commonly pauses range reads while it parses a new fragmented MP4
// or refills its media pipeline. Keep the producer around long enough for the
// next request instead of treating that normal gap as abandonment.
const WEB_RECONNECT_GRACE: Duration = Duration::from_secs(30);
const WEB_EPHEMERAL_RETENTION: Duration = Duration::from_secs(30);

fn terminate_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        // SAFETY: every child using this helper is placed in a fresh process
        // group before spawn, so the negative PID cannot target the daemon.
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = Instant::now() + CHILD_TERM_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                return;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break,
        }
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
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
    web_cancelled: AtomicU64,
    web_failures_busy: AtomicU64,
    web_failures_producer: AtomicU64,
    web_startup_playable_count: AtomicU64,
    web_startup_playable_sum_ms: AtomicU64,
    web_startup_playable_max_ms: AtomicU64,
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
    pub web_cancelled_total: u64,
    pub web_failures_busy_total: u64,
    pub web_failures_producer_total: u64,
    pub web_startup_playable_count: u64,
    pub web_startup_playable_sum_ms: u64,
    pub web_startup_playable_max_ms: u64,
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
            web_cancelled: AtomicU64::new(0),
            web_failures_busy: AtomicU64::new(0),
            web_failures_producer: AtomicU64::new(0),
            web_startup_playable_count: AtomicU64::new(0),
            web_startup_playable_sum_ms: AtomicU64::new(0),
            web_startup_playable_max_ms: AtomicU64::new(0),
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

    fn record_web_startup(&self, elapsed: Duration) {
        let millis = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        self.web_startup_playable_count
            .fetch_add(1, Ordering::Relaxed);
        self.web_startup_playable_sum_ms
            .fetch_add(millis, Ordering::Relaxed);
        self.web_startup_playable_max_ms
            .fetch_max(millis, Ordering::Relaxed);
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
    web: bool,
    cache_hit: bool,
    playable_observed: AtomicBool,
    pub dest: PathBuf,
    pub part: PathBuf,
    pub(crate) state: Mutex<RemuxState>,
    pub(crate) changed: tokio::sync::Notify,
    cancelled: AtomicBool,
    cancel_when_idle: AtomicBool,
    clients: AtomicUsize,
    ever_had_client: AtomicBool,
    client_epoch: AtomicU64,
    disconnect_deadline: Mutex<Option<Instant>>,
    cacheable: bool,
    started: Instant,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RecentRemuxState {
    state: &'static str,
    at: Instant,
}

impl RemuxJob {
    fn add_web_request(&self, request_id: Option<u64>) {
        if let Some(request_id) = request_id {
            crate::lock_recover(&self.web_request_ids).insert(request_id);
        }
    }

    fn matches_web_request(&self, request_id: Option<u64>) -> bool {
        request_id.is_none_or(|request_id| {
            crate::lock_recover(&self.web_request_ids).contains(&request_id)
        })
    }

    fn err(&self) -> Option<String> {
        match self.state.lock().ok()?.clone() {
            RemuxState::Failed(error) => Some(error),
            RemuxState::Cancelled => Some("remux cancelled".into()),
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
        self.cancel_when_idle.store(false, Ordering::Release);
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
        let mut deadline = crate::lock_recover(&self.disconnect_deadline);
        let previous = self.clients.fetch_sub(1, Ordering::AcqRel);
        self.client_epoch.fetch_add(1, Ordering::AcqRel);
        if previous <= 1 && self.cancel_when_idle.swap(false, Ordering::AcqRel) {
            *deadline = None;
            drop(deadline);
            self.cancel();
            return;
        }
        if previous <= 1 && !continue_after_disconnect {
            match self.state() {
                RemuxState::Starting | RemuxState::Preprocessing | RemuxState::Growing => {
                    *deadline = Some(Instant::now() + reconnect_grace);
                }
                RemuxState::Complete if !self.cacheable => {
                    *deadline = None;
                    drop(deadline);
                    schedule_ephemeral_cleanup(
                        app,
                        job_key,
                        self.clone(),
                        ephemeral_retention,
                    );
                    return;
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
        let _ = app.remux_metrics.cache_bytes.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| Some(current.saturating_sub(bytes)),
        );
    }
    let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&job.dest));
}

fn schedule_ephemeral_cleanup(
    app: Arc<App>,
    job_key: String,
    job: Arc<RemuxJob>,
    retention: Duration,
) {
    let epoch = job.client_epoch.load(Ordering::Acquire);
    let id = job.detail_id;
    let spawned = std::thread::Builder::new()
        .name(format!("remux-retain-{id}"))
        .spawn(move || {
            std::thread::sleep(retention);
            let mut jobs = crate::lock_recover(&app.remuxes);
            let same_job = jobs
                .get(&job_key)
                .is_some_and(|current| Arc::ptr_eq(current, &job));
            if same_job
                && job.clients.load(Ordering::Acquire) == 0
                && job.client_epoch.load(Ordering::Acquire) == epoch
                && matches!(job.state(), RemuxState::Complete)
            {
                remove_ephemeral_output(&app, &job);
                jobs.remove(&job_key);
                tracing::debug!(id, "expired reconnectable web segment");
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(id, %error, "could not schedule web segment cleanup");
    }
}

fn spawn_ffmpeg(
    app: Arc<App>,
    spec: RemuxJobSpec,
    job: Arc<RemuxJob>,
    helper_permit: rusty_dlna_scan::HelperPermit,
) {
    let app_err = app.clone();
    let job_err = job.clone();
    let id_err = spec.detail_id;
    let key_err = spec.job_key.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("remux-{}", spec.detail_id))
        .spawn(move || {
            let _helper_permit = helper_permit;
            let dest = spec.dest.clone();
            let part = job.part.clone();
            let id = spec.detail_id;
            let deadline = job.started + Duration::from_secs(app.cfg.transcode.max_runtime_secs);
            let verify_timeout = Duration::from_secs(app.cfg.transcode.verify_timeout_secs);
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
                let p8_result = if let Some(source) = spec.source_file.as_deref() {
                    run_remux_p8_file_controlled(
                        source,
                        &spec.src,
                        &part,
                        &p8,
                        deadline,
                        &job.cancelled,
                    )
                } else {
                    run_remux_p8_controlled(&spec.src, &part, &p8, deadline, &job.cancelled)
                };
                match p8_result {
                    Ok(()) => {
                        if let Err(error) = enforce_active_cache_limits(&app) {
                            job.transition(RemuxState::Failed(format!(
                                "transcode cache limits: {error}"
                            )));
                            cleanup_intermediates(&part);
                            finish_job(&app, &spec.job_key, &job);
                            return;
                        }
                        finalize_remux(&app, &job, id, &dest, &part, verify_timeout, true);
                        reject_completed_cache_over_limit(&app, &job, &dest);
                        if job.is_complete() && spec.cacheable {
                            if let Err(error) =
                                write_cache_stamp_for_key(&dest, &spec.cache_key)
                            {
                                tracing::warn!(id, dest = %dest.display(), %error, "cache stamp write failed");
                            }
                        }
                        finish_job(&app, &spec.job_key, &job);
                        return;
                    }
                    Err(e) => {
                        if job.cancelled.load(Ordering::Acquire) {
                            job.transition(RemuxState::Cancelled);
                            cleanup_intermediates(&part);
                            finish_job(&app, &spec.job_key, &job);
                            return;
                        }
                        tracing::warn!(id, dest = %dest.display(), "{e}; falling back to hdr10");
                        let _ = std::fs::remove_file(&part);
                        job.transition(RemuxState::Starting);
                    }
                }
            }
            let mut args = &spec.args;
            tracing::info!(id, dest = %dest.display(), "remux job start");
            let mut result = run_ffmpeg_growing(
                args,
                spec.source_file.as_deref(),
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
                    result = run_ffmpeg_growing(
                        args,
                        spec.source_file.as_deref(),
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
                    reject_completed_cache_over_limit(&app, &job, &dest);
                    if job.is_complete() && spec.cacheable {
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
                    if job.cancelled.load(Ordering::Acquire) || error == "cancelled" {
                        tracing::info!(
                            id,
                            dest = %dest.display(),
                            "remux job cancelled after its reconnect window expired"
                        );
                        job.transition(RemuxState::Cancelled);
                    } else {
                        tracing::error!(id, dest = %dest.display(), %error, "ffmpeg spawn failed");
                        job.transition(RemuxState::Failed(error));
                    }
                    cleanup_intermediates(&part);
                }
            }
            finish_job(&app, &spec.job_key, &job);
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
    job: &RemuxJob,
    deadline: Instant,
    app: &App,
) -> Result<(std::process::ExitStatus, String), String> {
    use std::io::Read;

    let Some(executable) = args.first() else {
        return Err("empty transcode command".into());
    };
    let mut command = std::process::Command::new(executable);
    command.args(&args[1..]);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if let Some(source) = source_file {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        const CHILD_SOURCE_FD: libc::c_int = 3;
        let source_fd = source.as_raw_fd();
        // SAFETY: only async-signal-safe dup2/fcntl calls run after fork. The
        // source descriptor is owned by the job spec through child completion.
        unsafe {
            command.pre_exec(move || {
                if source_fd != CHILD_SOURCE_FD && libc::dup2(source_fd, CHILD_SOURCE_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(CHILD_SOURCE_FD, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {}: {error}", executable.to_string_lossy()))?;
    let stderr = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        const MAX_STDERR: usize = 64 * 1024;
        let Some(mut stderr) = stderr else {
            return Vec::new();
        };
        let mut tail = Vec::with_capacity(MAX_STDERR);
        let mut chunk = [0u8; 4096];
        loop {
            let read = match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            tail.extend_from_slice(&chunk[..read]);
            if tail.len() > MAX_STDERR {
                let discard = tail.len() - MAX_STDERR;
                tail.drain(..discard);
            }
        }
        tail
    });

    let mut last_len = 0;
    let mut next_cache_check = Instant::now();
    let status = loop {
        if job.cancelled.load(Ordering::Acquire) {
            terminate_process_group(&mut child);
            let _ = stderr_reader.join();
            return Err("cancelled".into());
        }
        if job.reconnect_grace_expired() {
            job.cancel();
            terminate_process_group(&mut child);
            let _ = stderr_reader.join();
            return Err("cancelled".into());
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            let _ = stderr_reader.join();
            return Err("transcode runtime exceeded configured deadline".into());
        }
        if Instant::now() >= next_cache_check {
            if let Err(error) = enforce_active_cache_limits(app) {
                terminate_process_group(&mut child);
                let _ = stderr_reader.join();
                return Err(format!("transcode cache limits: {error}"));
            }
            next_cache_check = Instant::now() + Duration::from_secs(1);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_process_group(&mut child);
                return Err(format!("wait {}: {error}", executable.to_string_lossy()));
            }
        }
        let len = job
            .part
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if len != last_len {
            last_len = len;
            if len >= FIRST_BYTES && matches!(job.state(), RemuxState::Starting) {
                job.transition(RemuxState::Growing);
            } else {
                job.notify_growth();
            }
        }
        std::thread::sleep(POLL);
    };
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok((status, String::from_utf8_lossy(&stderr).into_owned()))
}

fn remove_job(app: &App, key: &str, job: &Arc<RemuxJob>) {
    if let Ok(mut map) = app.remuxes.lock() {
        if map
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, job))
        {
            map.remove(key);
        }
    }
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
    let mut recent = crate::lock_recover(&app.recent_remux_states);
    recent.retain(|_, value| value.at.elapsed() < Duration::from_secs(60));
    if recent.len() >= 128 {
        if let Some(oldest) = recent
            .iter()
            .min_by_key(|(_, value)| value.at)
            .map(|(id, _)| *id)
        {
            recent.remove(&oldest);
        }
    }
    for request_id in crate::lock_recover(&job.web_request_ids).iter().copied() {
        recent.insert(
            (job.detail_id, request_id),
            RecentRemuxState {
                state: public_state,
                at: Instant::now(),
            },
        );
    }
    drop(recent);
    if matches!(state, RemuxState::Complete) && !job.cacheable {
        if job.web && job.ever_had_client.load(Ordering::Acquire) {
            schedule_ephemeral_cleanup(
                app.clone(),
                key.to_owned(),
                job.clone(),
                WEB_EPHEMERAL_RETENTION,
            );
        } else {
            remove_ephemeral_output(app, job);
            remove_job(app, key, job);
        }
    } else {
        remove_job(app, key, job);
    }
    app.jobs.release();
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
    if let Err(e) = std::fs::rename(part, dest) {
        let msg = format!("remux rename: {e}");
        tracing::error!(id, dest = %dest.display(), "{msg}");
        job.transition(RemuxState::Failed(msg));
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
    use std::io::Read;

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
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stderr = child.stderr.take();
    let diagnostics = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stderr) = stderr {
            let _ = stderr.take(64 * 1024).read_to_end(&mut bytes);
        }
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            terminate_process_group(&mut child);
            let _ = diagnostics.join();
            return Err("cancelled".into());
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            let _ = diagnostics.join();
            return Err("ffprobe verification timed out".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                terminate_process_group(&mut child);
                let _ = diagnostics.join();
                return Err(error.to_string());
            }
        }
    };
    let diagnostics = diagnostics.join().unwrap_or_default();
    if !status.success() {
        let diagnostics = String::from_utf8_lossy(&diagnostics);
        return Err(if diagnostics.trim().is_empty() {
            format!("ffprobe exited {status}")
        } else {
            diagnostics.trim().to_string()
        });
    }
    Ok(())
}

fn cleanup_intermediates(part: &Path) {
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(part.with_extension("hevc"));
    let _ = std::fs::remove_file(part.with_extension("p8.hevc"));
}

fn reject_completed_cache_over_limit(app: &App, job: &RemuxJob, dest: &Path) {
    if !job.is_complete() {
        return;
    }
    if let Err(error) = enforce_active_cache_limits(app) {
        let bytes = dest.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        if std::fs::remove_file(dest).is_ok() {
            app.remux_metrics
                .cache_bytes
                .fetch_sub(bytes, Ordering::Relaxed);
        }
        let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(dest));
        job.transition(RemuxState::Failed(format!(
            "completed transcode exceeds cache limits: {error}"
        )));
    }
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
    if web {
        app.remux_metrics
            .web_requests
            .fetch_add(1, Ordering::Relaxed);
    }
    // The map lock serializes cache validation/replacement with all attaches
    // for this process. A source or plan change gets a different key/path.
    let mut map = crate::lock_recover(&app.remuxes);
    let protected = map
        .values()
        .flat_map(|job| [job.dest.clone(), job.part.clone()])
        .collect::<HashSet<_>>();
    maintain_app_cache(&app, &protected, false)
        .map_err(|error| format!("transcode cache limits: {error}"))?;
    if spec.cacheable && cache_is_fresh_for_key(&spec.dest, &spec.cache_key) {
        app.remux_metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        if web {
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
            web,
            cache_hit: true,
            playable_observed: AtomicBool::new(false),
            dest: spec.dest.clone(),
            part: cache_part(&spec.dest),
            state: Mutex::new(RemuxState::Complete),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            cancel_when_idle: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
        });
        if register_client {
            job.attach_client();
        }
        return Ok(job);
    }
    if let Some(job) = map.get(&spec.job_key) {
        if job.err().is_none() {
            job.add_web_request(spec.web_request_id);
            app.remux_metrics
                .coalesced_requests
                .fetch_add(1, Ordering::Relaxed);
            if web {
                app.remux_metrics
                    .web_cache_reuses
                    .fetch_add(1, Ordering::Relaxed);
            }
            tracing::info!(
                id = spec.detail_id,
                dest = %spec.dest.display(),
                web,
                cache_reuse = web,
                "remux attach"
            );
            if register_client {
                job.attach_client();
            }
            return Ok(job.clone());
        }
        map.remove(&spec.job_key);
    }
    if spec.dest.is_file() {
        tracing::info!(
            id = spec.detail_id,
            dest = %spec.dest.display(),
            "stale remux cache, rebuilding"
        );
        if let Ok(metadata) = spec.dest.metadata() {
            if std::fs::remove_file(&spec.dest).is_ok() {
                app.remux_metrics
                    .cache_bytes
                    .fetch_sub(metadata.len(), Ordering::Relaxed);
            }
        }
        let _ = std::fs::remove_file(cache_part(&spec.dest));
        let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&spec.dest));
    }
    app.remux_metrics
        .cache_misses
        .fetch_add(1, Ordering::Relaxed);
    if web && !spec.cacheable {
        app.remux_metrics
            .web_seek_restarts
            .fetch_add(1, Ordering::Relaxed);
    }
    let helper_permit = app
        .helpers
        .try_acquire()
        .map_err(|error| format!("media helper busy: {error}"))?;
    if !app.jobs.try_add() {
        return Err(format!(
            "transcode busy (max_jobs={})",
            app.cfg.transcode.max_jobs
        ));
    }
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
        web,
        cache_hit: false,
        playable_observed: AtomicBool::new(false),
        dest: spec.dest.clone(),
        part: part.clone(),
        state: Mutex::new(RemuxState::Starting),
        changed: tokio::sync::Notify::new(),
        cancelled: AtomicBool::new(false),
        cancel_when_idle: AtomicBool::new(false),
        clients: AtomicUsize::new(0),
        ever_had_client: AtomicBool::new(false),
        client_epoch: AtomicU64::new(0),
        disconnect_deadline: Mutex::new(None),
        cacheable: spec.cacheable,
        started: Instant::now(),
    });
    map.insert(spec.job_key.clone(), job.clone());
    if register_client {
        job.attach_client();
    }
    drop(map);
    spawn_ffmpeg(app, spec, job.clone(), helper_permit);
    Ok(job)
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

/// Cancel an explicitly superseded browser request without disturbing a
/// coalesced job that is also serving a different playback request.
pub(crate) fn cancel_web_request(app: &App, detail_id: i64, request_id: u64) -> bool {
    let job = app.remuxes.lock().ok().and_then(|jobs| {
        jobs.values()
            .find(|job| {
                if !job.web || job.detail_id != detail_id {
                    return false;
                }
                let request_ids = crate::lock_recover(&job.web_request_ids);
                request_ids.len() == 1 && request_ids.contains(&request_id)
            })
            .cloned()
    });
    let Some(job) = job else {
        return false;
    };
    job.cancel_when_idle.store(true, Ordering::Release);
    if job.clients.load(Ordering::Acquire) == 0 {
        job.cancel();
    }
    true
}

fn generated_cache_mp4(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("mp4") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let mut fields = stem.splitn(3, '-');
    fields.next().is_some_and(|id| id.parse::<i64>().is_ok())
        && fields
            .next()
            .is_some_and(|tag| matches!(tag, "hdr10" | "remux" | "ac3" | "web" | "orig"))
        && fields
            .next()
            .is_some_and(|key| key.len() == 40 && key.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn generated_intermediate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    for suffix in [".part.p8.hevc", ".part.hevc", ".p8.hevc", ".hevc", ".part"] {
        if let Some(base) = name.strip_suffix(suffix) {
            return generated_cache_mp4(&path.with_file_name(base));
        }
    }
    false
}

#[derive(Clone, Copy, Debug, Default)]
struct CacheMaintenance {
    bytes: u64,
    evicted_files: u64,
    evicted_bytes: u64,
}

fn maintain_transcode_cache_report(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
    protected: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<CacheMaintenance> {
    std::fs::create_dir_all(directory)?;
    let now = std::time::SystemTime::now();
    let max_age = Duration::from_secs(u64::from(max_age_days).saturating_mul(86_400));
    let mut finished = Vec::new();
    let mut total = 0u64;
    let mut evicted_files = 0u64;
    let mut evicted_bytes = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        if generated_intermediate(&path) {
            if startup && !protected.contains(&path) {
                if std::fs::remove_file(&path).is_ok() {
                    evicted_files = evicted_files.saturating_add(1);
                    evicted_bytes = evicted_bytes.saturating_add(metadata.len());
                } else {
                    total = total.saturating_add(metadata.len());
                }
            } else {
                total = total.saturating_add(metadata.len());
            }
            continue;
        }
        if !generated_cache_mp4(&path) {
            continue;
        }
        total = total.saturating_add(metadata.len());
        let used = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        if !protected.contains(&path)
            && now.duration_since(used).unwrap_or_default() > max_age
            && std::fs::remove_file(&path).is_ok()
        {
            total = total.saturating_sub(metadata.len());
            evicted_files = evicted_files.saturating_add(1);
            evicted_bytes = evicted_bytes.saturating_add(metadata.len());
            let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&path));
            continue;
        }
        if !protected.contains(&path) {
            finished.push((used, metadata.len(), path));
        }
    }
    finished.sort_by_key(|entry| entry.0);
    let free = crate::available_filesystem_bytes(directory).unwrap_or(u64::MAX);
    let mut reclaim = total
        .saturating_sub(quota_bytes)
        .max(minimum_free_bytes.saturating_sub(free));
    for (_, bytes, path) in finished {
        if reclaim == 0 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&path));
            reclaim = reclaim.saturating_sub(bytes);
            total = total.saturating_sub(bytes);
            evicted_files = evicted_files.saturating_add(1);
            evicted_bytes = evicted_bytes.saturating_add(bytes);
        }
    }
    if reclaim > 0 {
        return Err(std::io::Error::other(
            "quota or minimum-free-space target cannot be satisfied",
        ));
    }
    Ok(CacheMaintenance {
        bytes: total,
        evicted_files,
        evicted_bytes,
    })
}

pub(crate) fn maintain_transcode_cache(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
    protected: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<u64> {
    maintain_transcode_cache_report(
        directory,
        quota_bytes,
        max_age_days,
        minimum_free_bytes,
        protected,
        startup,
    )
    .map(|report| report.bytes)
}

fn maintain_app_cache(
    app: &App,
    protected: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<u64> {
    app.remux_metrics
        .cache_maintenance
        .fetch_add(1, Ordering::Relaxed);
    match maintain_transcode_cache_report(
        &app.cache_dir,
        app.cfg.transcode.cache_max_mb.saturating_mul(1024 * 1024),
        app.cfg.transcode.cache_max_age_days,
        app.cfg.cache_min_free_mb.saturating_mul(1024 * 1024),
        protected,
        startup,
    ) {
        Ok(report) => {
            app.remux_metrics
                .cache_bytes
                .store(report.bytes, Ordering::Relaxed);
            app.remux_metrics
                .cache_evicted_files
                .fetch_add(report.evicted_files, Ordering::Relaxed);
            app.remux_metrics
                .cache_evicted_bytes
                .fetch_add(report.evicted_bytes, Ordering::Relaxed);
            Ok(report.bytes)
        }
        Err(error) => {
            app.remux_metrics
                .cache_maintenance_failures
                .fetch_add(1, Ordering::Relaxed);
            Err(error)
        }
    }
}

fn enforce_active_cache_limits(app: &App) -> std::io::Result<u64> {
    let protected = app
        .remuxes
        .lock()
        .map(|jobs| {
            jobs.values()
                .flat_map(|job| [job.dest.clone(), job.part.clone()])
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    maintain_app_cache(app, &protected, false)
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
            RemuxState::Cancelled => return Err("remux cancelled".into()),
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
            tracing::error!(
                id = spec.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "{e}"
            );
            let err = if req.path.starts_with("/web/media/") {
                app.remux_metrics
                    .web_failures_busy
                    .fetch_add(1, Ordering::Relaxed);
                crate::web_ui::transcode_stream_error(503, "transcode_busy")
            } else {
                let mut response = HttpResponse::html(503, "Service Unavailable", &e);
                response.set("Retry-After", "1");
                response
            };
            crate::socket_write_all(app, sock, &err.bytes_wire(&app.server, &now_imf_date()))
                .await?;
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
            tracing::error!(
                id = spec.detail_id,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "{e}"
            );
            let err = if req.path.starts_with("/web/media/") {
                let code = if e.contains("cancel") {
                    "transcode_cancelled"
                } else {
                    "transcode_failed"
                };
                crate::web_ui::transcode_stream_error(500, code)
            } else {
                HttpResponse::html(500, "Internal Server Error", &e)
            };
            crate::socket_write_all(app, sock, &err.bytes_wire(&app.server, &now_imf_date()))
                .await?;
            return Ok(());
        }
    };
    if job.web
        && job
            .playable_observed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        app.remux_metrics.record_web_startup(job.started.elapsed());
        tracing::info!(
            id = job.detail_id,
            startup_to_first_playable_ms = job.started.elapsed().as_millis() as u64,
            cache_reuse = job.cache_hit,
            "web compatible media became playable"
        );
    }
    let finished = path == job.dest
        && job.dest.is_file()
        && job.dest.metadata().map(|m| m.len() > 0).unwrap_or(false);
    if finished {
        return serve_finished(app, sock, req, &job.dest, spec.mime, head).await;
    }
    serve_growing(app, sock, req, &job, spec.mime, head).await
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
    let jobs = app
        .remuxes
        .lock()
        .map(|jobs| jobs.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
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
        web_cancelled_total: app.remux_metrics.web_cancelled.load(Ordering::Relaxed),
        web_failures_busy_total: app.remux_metrics.web_failures_busy.load(Ordering::Relaxed),
        web_failures_producer_total: app
            .remux_metrics
            .web_failures_producer
            .load(Ordering::Relaxed),
        web_startup_playable_count: app
            .remux_metrics
            .web_startup_playable_count
            .load(Ordering::Relaxed),
        web_startup_playable_sum_ms: app
            .remux_metrics
            .web_startup_playable_sum_ms
            .load(Ordering::Relaxed),
        web_startup_playable_max_ms: app
            .remux_metrics
            .web_startup_playable_max_ms
            .load(Ordering::Relaxed),
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
                crate::socket_write_all(app, sock, &err.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
                return Ok(());
            }
            Err(RangeError::Unsatisfiable) => {
                tracing::error!(path = %req.path, range = v, size, "range past remux EOF");
                let mut err =
                    HttpResponse::html(416, "Requested Range Not Satisfiable", "range past EOF");
                err.set("Content-Range", format!("bytes */{size}"));
                crate::socket_write_all(app, sock, &err.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
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
        crate::socket_write_all(app, sock, &resp.bytes_wire(&app.server, &now_imf_date())).await?;
        return Ok(());
    }
    crate::socket_write_all(app, sock, &resp.bytes_wire(&app.server, &now_imf_date())).await?;
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
        None => (0u64, None),
        Some(v) => match parse_open_range(v) {
            Ok(p) => p,
            Err(_) => {
                tracing::error!(path = %req.path, range = v, "invalid Range on growing remux");
                let err = HttpResponse::html(400, "Bad Request", "invalid range");
                crate::socket_write_all(app, sock, &err.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
                return Ok(());
            }
        },
    };
    let (start, end) = open;
    let probe = end.is_some_and(|e| e.saturating_sub(start) < 2 * 1024 * 1024);
    if probe {
        if let Some(e) = end {
            if let Err(err) = wait_offset(job, e.saturating_add(1)).await {
                tracing::error!(id = %job.dest.display(), "{err}");
                let resp = HttpResponse::html(500, "Internal Server Error", &err);
                crate::socket_write_all(app, sock, &resp.bytes_wire(&app.server, &now_imf_date()))
                    .await?;
                return Ok(());
            }
            let have = current_len(job);
            let end = e.min(have.saturating_sub(1));
            let mut resp = live_transcode_response(mime);
            resp.status = 206;
            resp.reason = "OK".into();
            resp.set("Content-Range", format!("bytes {start}-{end}/*"));
            resp.set(
                "Content-Length",
                end.saturating_sub(start).saturating_add(1),
            );
            crate::socket_write_all(app, sock, &resp.bytes_wire(&app.server, &now_imf_date()))
                .await?;
            if !head {
                stream_growing(app, sock, job, start, Some(end)).await?;
            }
            return Ok(());
        }
    }
    let resp = live_transcode_response(mime);
    crate::socket_write_all(app, sock, &resp.bytes_wire(&app.server, &now_imf_date())).await?;
    if head {
        return Ok(());
    }
    stream_growing(app, sock, job, start, end).await
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
            return Err("remux cancelled".into());
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
    let mut f = tokio::fs::File::open(&path).await?;
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
            web_request_id: None,
            mime: "video/mp4",
            job_key: format!("42:{key}:{command:?}"),
            cache_key: key.into(),
            src,
            source_file: None,
            dest: dir.join(format!("{key}.mp4")),
            args: command.into_iter().map(Into::into).collect(),
            fallback_args: None,
            continue_after_disconnect: true,
            cacheable: true,
            remux_p8: false,
            audio_index: 0,
            audio: RemuxAudio::Copy,
        }
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

    #[tokio::test]
    async fn wait_ready_is_not_timed_out_during_preprocessing() {
        let tmp = TempDir::new("wait-ready");
        let dest = tmp.join("out.mp4");
        let part = tmp.join("out.mp4.part");
        let job = Arc::new(RemuxJob {
            detail_id: 42,
            web_request_ids: Mutex::new(HashSet::new()),
            web: false,
            cache_hit: false,
            playable_observed: AtomicBool::new(false),
            dest: dest.clone(),
            part,
            state: Mutex::new(RemuxState::Preprocessing),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            cancel_when_idle: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
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

        let part = dir.join("growing.mp4.part");
        std::fs::write(&part, vec![0x5a; FIRST_BYTES as usize]).unwrap();
        let growing = Arc::new(RemuxJob {
            detail_id: 43,
            web_request_ids: Mutex::new(HashSet::new()),
            web: false,
            cache_hit: false,
            playable_observed: AtomicBool::new(false),
            dest: dir.join("growing.mp4"),
            part,
            state: Mutex::new(RemuxState::Growing),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            cancel_when_idle: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
        });
        let growing_req = HttpRequest::parse_headers(
            "HEAD /Transcode/43.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server_job = growing.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_growing(
                &server_app,
                &mut socket,
                &growing_req,
                &server_job,
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
        assert!(String::from_utf8_lossy(&bytes).contains("transferMode.dlna.org: Streaming"));
        assert!(wire_body(&bytes).is_empty());
        let _ = std::fs::remove_dir_all(dir);
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
            web_request_id: None,
            mime: "video/mp4",
            job_key: format!("42:{first_key}:copy"),
            cache_key: first_key.clone(),
            src: src.clone(),
            source_file: None,
            dest: dest.clone(),
            args: vec![
                "cp".into(),
                src.as_os_str().to_os_string(),
                part.as_os_str().to_os_string(),
            ],
            fallback_args: None,
            continue_after_disconnect: true,
            cacheable: true,
            remux_p8: false,
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
            web_request_id: None,
            mime: "video/mp4",
            job_key: format!("42:{second_key}:copy"),
            cache_key: second_key,
            src: src.clone(),
            source_file: None,
            dest: dest.clone(),
            args: vec![
                "cp".into(),
                src.as_os_str().to_os_string(),
                part.as_os_str().to_os_string(),
            ],
            fallback_args: None,
            continue_after_disconnect: true,
            cacheable: true,
            remux_p8: false,
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
    fn superseded_web_request_cancels_as_soon_as_its_reader_detaches() {
        let dir = temp_dir("web-explicit-cancel");
        let app = test_app(&dir, 1);
        let mut spec = job_spec(
            &dir,
            "web-explicit-cancel",
            vec!["sleep".into(), "30".into()],
        );
        spec.job_key = "web:42:explicit-cancel".into();
        spec.web_request_id = Some(77);
        spec.cacheable = false;
        spec.continue_after_disconnect = false;
        let job = attach_for_client(app.clone(), spec).unwrap();

        assert!(cancel_web_request(&app, 42, 77));
        assert!(!job.cancelled.load(Ordering::Acquire));
        job.detach_client(
            app.clone(),
            "web:42:explicit-cancel".into(),
            false,
            WEB_RECONNECT_GRACE,
            Duration::ZERO,
        );
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Cancelled);
    }

    #[test]
    fn cancelling_one_request_does_not_kill_a_shared_web_job() {
        let dir = temp_dir("web-shared-cancel");
        let app = test_app(&dir, 1);
        let mut first = job_spec(
            &dir,
            "web-shared-cancel",
            vec!["sleep".into(), "30".into()],
        );
        first.job_key = "web:42:shared-cancel".into();
        first.web_request_id = Some(77);
        first.cacheable = false;
        first.continue_after_disconnect = false;
        let mut second = first.clone();
        second.web_request_id = Some(88);
        let job = attach_for_client(app.clone(), first).unwrap();
        let shared = attach_for_client(app.clone(), second).unwrap();
        assert!(Arc::ptr_eq(&job, &shared));

        assert!(!cancel_web_request(&app, 42, 77));
        assert!(!job.cancelled.load(Ordering::Acquire));
        for attached in [job.clone(), shared] {
            attached.detach_client(
                app.clone(),
                "web:42:shared-cancel".into(),
                false,
                Duration::from_millis(20),
                Duration::ZERO,
            );
        }
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Cancelled);
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
        assert!(dest.is_file(), "an active reconnect must retain the completed output");

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
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(dest).unwrap(), expected);
        let _ = std::fs::remove_dir_all(dir);
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
            web: false,
            cache_hit: false,
            playable_observed: AtomicBool::new(false),
            dest: dir.join("idle.mp4"),
            part: dir.join("idle.mp4.part"),
            state: Mutex::new(RemuxState::Starting),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            cancel_when_idle: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
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
        let key_a = "a".repeat(40);
        let key_b = "b".repeat(40);
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
        let old = dir.join(format!("1-hdr10-{}.mp4", "a".repeat(40)));
        let new = dir.join(format!("2-remux-{}.mp4", "b".repeat(40)));
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
