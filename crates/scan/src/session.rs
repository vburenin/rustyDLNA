//! Private staging ownership and transactional catalog publication.

use crate::{
    fill_missing_av_meta_with_db, library_write_guard, monitor_dirty_with_db,
    open_library_db_cancelled, path_with_suffix, rebuild_objects_with_db, remove_stale_cached_art,
    repair_video_titles_with_db, scan_io, scan_with_db, sha256_hex, unix_now_seconds, watch,
    CatalogUpdate, LibraryDb, ScanConfig, ScanDelta, ScanError, ScanResult,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

type BookmarkSnapshot = HashMap<i64, (i64, i64)>;
type LiveStagePublication = (u64, Option<BookmarkSnapshot>);

/// Opaque scanner work prepared against a disk-backed staging catalog.
/// Dropping it without publishing invalidates its originating session so a
/// later pass cannot build on changes the live database never received.
pub struct PreparedCatalogChange {
    update: Option<CatalogUpdate>,
    delta: ScanDelta,
    bookmark_expiry_cutoff: Option<i64>,
    stale_art: Vec<String>,
    token: u64,
    session_identity: std::sync::Arc<()>,
    invalidated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    finished: bool,
}

impl PreparedCatalogChange {
    /// Scanner change counts observed while preparing this work.
    pub fn delta(&self) -> &ScanDelta {
        &self.delta
    }
}

impl Drop for PreparedCatalogChange {
    fn drop(&mut self) {
        if !self.finished {
            self.invalidated
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

/// A committed live merge plus deferred in-memory hydration and cache cleanup.
/// Callers using `publish_to_db` should release the live writer, call the
/// originating session's `finish_publication`, then take/apply the catalog
/// parts and run stale-art cleanup after publication serialization ends.
pub struct PublishedCatalogChange {
    update: Option<CatalogUpdate>,
    delta: ScanDelta,
    committed_update_id: Option<u32>,
    writer_reopen_required: bool,
    bookmark_snapshot: Option<BookmarkSnapshot>,
    cleanup_cfg: ScanConfig,
    stale_art: Vec<String>,
}

impl PublishedCatalogChange {
    /// Whether the caller should reopen its pooled writer connection because
    /// a committed stage could not be detached from that connection.
    pub fn writer_reopen_required(&self) -> bool {
        self.writer_reopen_required
    }

    /// The live SystemUpdateID committed with the returned catalog mutation.
    /// Compatibility publishers that do not allocate generations return none.
    pub fn committed_update_id(&self) -> Option<u32> {
        self.committed_update_id
    }

    /// Take the catalog mutation and counts, applying any live bookmark
    /// snapshot after the writer lock has been released.
    pub fn take_parts(&mut self) -> (Option<CatalogUpdate>, ScanDelta) {
        if let (Some(CatalogUpdate::Replacement(catalog)), Some(bookmarks)) =
            (self.update.as_mut(), self.bookmark_snapshot.take())
        {
            LibraryDb::apply_catalog_bookmark_snapshot(catalog, &bookmarks);
        }
        (self.update.take(), std::mem::take(&mut self.delta))
    }

    /// Delete obsolete scanner cache artwork after publication locks release.
    pub fn cleanup_stale_art(&mut self) {
        remove_stale_cached_art(&self.cleanup_cfg, &self.stale_art);
        self.stale_art.clear();
    }

    /// Consume the publication and perform its deferred cache cleanup.
    pub fn into_parts(mut self) -> (Option<CatalogUpdate>, ScanDelta) {
        self.cleanup_stale_art();
        self.take_parts()
    }
}

/// Reusable scanner staging state. Runtime watchers should keep one session
/// across batches so targeted events avoid copying the whole live catalog.
/// All expensive filesystem/helper work mutates only this private stage.
pub struct ScanSession {
    cfg: ScanConfig,
    live_path: PathBuf,
    stage_path: PathBuf,
    stage_lock_path: PathBuf,
    _stage_lock: std::fs::File,
    stage: Option<LibraryDb>,
    invalidated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_identity: std::sync::Arc<()>,
    outstanding: Option<u64>,
    next_token: u64,
    pending_stage_reset_epoch: Option<u64>,
    #[cfg(test)]
    backup_count: usize,
}

impl ScanSession {
    /// Back up the configured live database into a private reusable stage.
    pub fn new(cfg: &ScanConfig) -> ScanResult<Self> {
        let live_path = cfg.db_path.clone().ok_or_else(|| {
            ScanError::Invariant("scan sessions require a persistent library database".into())
        })?;
        if let Some(parent) = live_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| scan_io(parent, error))?;
        }
        cleanup_stale_scan_stages(&live_path);
        let (stage_path, stage_lock_path, stage_lock) = reserve_scan_stage(&live_path)?;
        let mut session = Self {
            cfg: cfg.clone(),
            live_path,
            stage_path,
            stage_lock_path,
            _stage_lock: stage_lock,
            stage: None,
            invalidated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            session_identity: std::sync::Arc::new(()),
            outstanding: None,
            next_token: 1,
            pending_stage_reset_epoch: None,
            #[cfg(test)]
            backup_count: 0,
        };
        session.rebackup()?;
        Ok(session)
    }

    fn remove_stage_files(&self) {
        for path in [
            self.stage_path.clone(),
            path_with_suffix(&self.stage_path, "-wal"),
            path_with_suffix(&self.stage_path, "-shm"),
            path_with_suffix(&self.stage_path, "-journal"),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "could not remove scan stage")
                }
            }
        }
    }

    fn rebackup(&mut self) -> ScanResult<()> {
        self.cfg.check_cancelled()?;
        self.stage.take();
        self.remove_stage_files();
        let live = open_library_db_cancelled(&self.live_path, &self.cfg.cancellation)?;
        live.backup_to_path_cancelled(&self.stage_path, &self.cfg.cancellation)?;
        let stage = open_library_db_cancelled(&self.stage_path, &self.cfg.cancellation)?;
        stage.install_cancellation(self.cfg.cancellation.clone())?;
        stage.begin_scan_change_capture()?;
        self.stage = Some(stage);
        self.outstanding = None;
        self.pending_stage_reset_epoch = None;
        self.invalidated
            .store(false, std::sync::atomic::Ordering::Release);
        #[cfg(test)]
        {
            self.backup_count += 1;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn backup_count(&self) -> usize {
        self.backup_count
    }

    fn begin_prepare(&mut self) -> ScanResult<u64> {
        self.finish_publication();
        if self.invalidated.load(std::sync::atomic::Ordering::Acquire) {
            self.rebackup()?;
        }
        if self.outstanding.is_some() {
            return Err(ScanError::Invariant(
                "scan session already has unpublished work".into(),
            ));
        }
        let token = self.next_token;
        self.next_token = self.next_token.checked_add(1).ok_or_else(|| {
            ScanError::Invariant("scan session preparation token space was exhausted".into())
        })?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| ScanError::Invariant("scan stage is unavailable".into()))?;
        stage.clear_scan_change_capture()?;
        self.outstanding = Some(token);
        Ok(token)
    }

    fn prepared(
        &self,
        token: u64,
        update: Option<CatalogUpdate>,
        delta: ScanDelta,
        bookmark_expiry_cutoff: Option<i64>,
        stale_art: Vec<String>,
    ) -> PreparedCatalogChange {
        PreparedCatalogChange {
            update,
            delta,
            bookmark_expiry_cutoff,
            stale_art,
            token,
            session_identity: std::sync::Arc::clone(&self.session_identity),
            invalidated: std::sync::Arc::clone(&self.invalidated),
            finished: false,
        }
    }

    /// Prepare a complete scan without holding the live catalog writer.
    pub fn prepare_scan(&mut self, rebuild: bool) -> ScanResult<PreparedCatalogChange> {
        let cfg = self.cfg.clone();
        let token = self.begin_prepare()?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| ScanError::Invariant("scan stage is unavailable".into()))?;
        let result = scan_with_db(&cfg, rebuild, stage);
        match result {
            Ok(catalog) => {
                let delta = ScanDelta {
                    added: catalog.items.len(),
                    ..ScanDelta::default()
                };
                Ok(self.prepared(
                    token,
                    Some(CatalogUpdate::Replacement(catalog)),
                    delta,
                    bookmark_expiry_cutoff(&self.cfg),
                    Vec::new(),
                ))
            }
            Err(error) => {
                self.outstanding = None;
                self.invalidated
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    /// Prepare one full or targeted reconciliation. Only one prepared change
    /// may be outstanding, and it must be published by this same session.
    pub fn prepare_monitor(
        &mut self,
        dirty: &[PathBuf],
        incremental: bool,
    ) -> ScanResult<PreparedCatalogChange> {
        let cfg = self.cfg.clone();
        let token = self.begin_prepare()?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| ScanError::Invariant("scan stage is unavailable".into()))?;
        let mut stale_art = Vec::new();
        let result = monitor_dirty_with_db(&cfg, dirty, incremental, stage, Some(&mut stale_art));
        match result {
            Ok((update, delta)) => Ok(self.prepared(
                token,
                update,
                delta,
                dirty
                    .is_empty()
                    .then(|| bookmark_expiry_cutoff(&self.cfg))
                    .flatten(),
                stale_art,
            )),
            Err(error) => {
                self.outstanding = None;
                self.invalidated
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    /// Prepare deterministic object-table reconstruction in the private stage.
    pub fn prepare_rebuild_objects(&mut self) -> ScanResult<PreparedCatalogChange> {
        let cfg = self.cfg.clone();
        let token = self.begin_prepare()?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| ScanError::Invariant("scan stage is unavailable".into()))?;
        match rebuild_objects_with_db(&cfg, stage) {
            Ok(catalog) => Ok(self.prepared(
                token,
                Some(CatalogUpdate::Replacement(catalog)),
                ScanDelta {
                    changed: 1,
                    ..ScanDelta::default()
                },
                None,
                Vec::new(),
            )),
            Err(error) => {
                self.outstanding = None;
                self.invalidated
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    /// Prepare the persisted video-title policy repair in the private stage.
    pub fn prepare_video_title_repair(&mut self) -> ScanResult<PreparedCatalogChange> {
        let cfg = self.cfg.clone();
        let token = self.begin_prepare()?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| ScanError::Invariant("scan stage is unavailable".into()))?;
        match repair_video_titles_with_db(&cfg, stage) {
            Ok((catalog, delta)) => Ok(self.prepared(
                token,
                catalog.map(CatalogUpdate::Replacement),
                delta,
                None,
                Vec::new(),
            )),
            Err(error) => {
                self.outstanding = None;
                self.invalidated
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    /// Prepare startup object-integrity repair in the private stage.
    pub fn prepare_object_repair(&mut self) -> ScanResult<PreparedCatalogChange> {
        let cfg = self.cfg.clone();
        let token = self.begin_prepare()?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| ScanError::Invariant("scan stage is unavailable".into()))?;
        match watch::repair_objects_if_needed_with_db(&cfg, stage) {
            Ok((catalog, delta)) => Ok(self.prepared(
                token,
                catalog.map(CatalogUpdate::Replacement),
                delta,
                None,
                Vec::new(),
            )),
            Err(error) => {
                self.outstanding = None;
                self.invalidated
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    /// Probe and prepare missing stream metadata without a live writer lock.
    pub fn prepare_fill_missing_av_meta(&mut self) -> ScanResult<PreparedCatalogChange> {
        let cfg = self.cfg.clone();
        let token = self.begin_prepare()?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| ScanError::Invariant("scan stage is unavailable".into()))?;
        match fill_missing_av_meta_with_db(&cfg, stage) {
            Ok((catalog, delta)) => Ok(self.prepared(
                token,
                catalog.map(CatalogUpdate::Replacement),
                delta,
                None,
                Vec::new(),
            )),
            Err(error) => {
                self.outstanding = None;
                self.invalidated
                    .store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    /// Merge prepared journals into the bound live database transactionally.
    /// Dropped or failed work invalidates the stage; the next preparation then
    /// takes a new backup. For an actual catalog mutation, a supplied
    /// `update_id` must be exactly the next wrapping UPnP generation derived
    /// from the live transaction; a mismatch rolls back the merge. `None`
    /// preserves compatibility callers that do not allocate server
    /// generations, and no-op publications never consume a supplied ID.
    /// Bookmarks are hydrated from the live transaction.
    pub fn publish_to_db(
        &mut self,
        live: &LibraryDb,
        mut prepared: PreparedCatalogChange,
        update_id: Option<u32>,
    ) -> ScanResult<PublishedCatalogChange> {
        if !std::sync::Arc::ptr_eq(&self.session_identity, &prepared.session_identity)
            || self.outstanding != Some(prepared.token)
            || self.invalidated.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ScanError::Invariant(
                "prepared scan work no longer belongs to this session".into(),
            ));
        }
        // Both handles originate from the configured database path. Comparing
        // raw paths is byte-preserving on Unix and fails closed for aliases;
        // publication must not perform filesystem resolution while a pooled
        // live writer is held.
        if self.live_path != live.path {
            return Err(ScanError::Invariant(
                "prepared scan work was offered to a different live database".into(),
            ));
        }
        self.cfg.check_cancelled()?;
        live.attach_scan_stage(&self.stage_path)?;
        let result = (|| -> ScanResult<LiveStagePublication> {
            let transaction = live.transaction()?;
            let (live_epoch, stage_epoch) = live.attached_scan_epochs()?;
            if live_epoch != stage_epoch {
                return Err(ScanError::Invariant(format!(
                    "scan stage epoch {stage_epoch} does not match live epoch {live_epoch}"
                )));
            }
            let next_epoch = live_epoch.checked_add(1).ok_or_else(|| {
                ScanError::Invariant("scan catalog epoch space was exhausted".into())
            })?;
            live.begin_catalog_change_capture()?;
            live.merge_attached_scan_stage(prepared.bookmark_expiry_cutoff)?;
            let bookmark_snapshot = if matches!(
                prepared.update.as_ref(),
                Some(CatalogUpdate::Replacement(_))
            ) {
                Some(live.catalog_bookmark_snapshot()?)
            } else {
                let patch = live.load_catalog_patch()?;
                prepared.update = (!patch.changed_object_ids.is_empty()
                    || !patch.changed_detail_ids.is_empty()
                    || !patch.changed_album_art_ids.is_empty())
                .then_some(CatalogUpdate::Patch(patch));
                None
            };
            live.end_catalog_change_capture()?;
            if let (Some(id), true) = (update_id, prepared.update.is_some()) {
                let current = live.get_update_id()?;
                let expected = rusty_dlna_protocol::soap::next_system_update_id(current);
                if id != expected {
                    return Err(ScanError::Invariant(format!(
                        "scan publication update ID {id} does not follow live update ID {current}; expected {expected}"
                    )));
                }
                live.set_update_id(id)?;
            }
            live.set_scan_catalog_epoch(next_epoch)?;
            self.cfg.check_cancelled()?;
            transaction.commit()?;
            Ok((next_epoch, bookmark_snapshot))
        })();
        let detach_result = live.detach_scan_stage();
        self.outstanding = None;
        let (next_epoch, bookmark_snapshot) = match result {
            Ok(committed) => committed,
            Err(error) => {
                self.invalidated
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(error);
            }
        };
        let writer_reopen_required = if let Err(error) = detach_result {
            tracing::warn!(%error, "committed scan stage could not detach; next pass will recover the writer connection");
            true
        } else {
            false
        };
        prepared.finished = true;
        self.pending_stage_reset_epoch = Some(next_epoch);
        Ok(PublishedCatalogChange {
            committed_update_id: prepared.update.as_ref().and(update_id),
            update: prepared.update.take(),
            delta: std::mem::take(&mut prepared.delta),
            writer_reopen_required,
            bookmark_snapshot,
            cleanup_cfg: self.cfg.clone(),
            stale_art: std::mem::take(&mut prepared.stale_art),
        })
    }

    /// Finish private stage bookkeeping after the live writer lock is
    /// released. Failure makes the next preparation take a fresh backup.
    pub fn finish_publication(&mut self) {
        let Some(epoch) = self.pending_stage_reset_epoch.take() else {
            return;
        };
        let reset = self
            .stage
            .as_ref()
            .ok_or(rusqlite::Error::InvalidQuery)
            .and_then(|stage| {
                stage
                    .set_scan_catalog_epoch(epoch)
                    .and_then(|_| stage.clear_scan_change_capture())
            });
        if let Err(error) = reset {
            tracing::warn!(%error, "could not reuse scan stage; next pass will rebackup");
            self.invalidated
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Publish synchronously using the compatibility one-shot live writer.
    pub fn publish(
        &mut self,
        prepared: PreparedCatalogChange,
    ) -> ScanResult<PublishedCatalogChange> {
        let _write = library_write_guard();
        let live = open_library_db_cancelled(&self.live_path, &self.cfg.cancellation)?;
        let mut published = self.publish_to_db(&live, prepared, None)?;
        drop(live);
        self.finish_publication();
        // Compatibility one-shot scans historically serialized database and
        // content-addressed cache cleanup together. Keep the guard until the
        // stale path is gone so another direct scan cannot re-reference it in
        // the gap.
        published.cleanup_stale_art();
        Ok(published)
    }
}

impl Drop for ScanSession {
    fn drop(&mut self) {
        self.stage.take();
        self.remove_stage_files();
        // Unlink the reservation while its inode is still locked. Releasing
        // first would let cleanup acquire the old inode and race a new
        // create_new reservation at the same pathname.
        if let Err(error) = std::fs::remove_file(&self.stage_lock_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.stage_lock_path.display(), %error, "could not remove scan stage lock");
            }
        }
        // Keep the advisory lock through unlink. `stage_lock` releases it only
        // after this Drop implementation returns.
    }
}

#[cfg(test)]
struct PreemptedScanStage {
    stage_path: PathBuf,
    lock_path: PathBuf,
    _lock: std::fs::File,
}

#[cfg(test)]
type ScanStagePreemptionSink = std::sync::Arc<std::sync::Mutex<Option<PreemptedScanStage>>>;

#[cfg(test)]
static RESERVATION_PREEMPTIONS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, ScanStagePreemptionSink>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

#[cfg(test)]
static CLEANUP_PREEMPTIONS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, ScanStagePreemptionSink>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

#[cfg(test)]
fn install_preempted_scan_stage(
    stage_path: &Path,
    lock_path: &Path,
    sink: ScanStagePreemptionSink,
) {
    let replacement = (|| -> std::io::Result<PreemptedScanStage> {
        match std::fs::remove_file(lock_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lock_path)?;
        if !try_lock_scan_stage(&lock)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "replacement scan-stage lock was not acquired",
            ));
        }
        std::fs::write(stage_path, b"replacement-stage-sentinel")?;
        Ok(PreemptedScanStage {
            stage_path: stage_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            _lock: lock,
        })
    })();
    if let Ok(replacement) = replacement {
        *sink.lock().unwrap_or_else(|error| error.into_inner()) = Some(replacement);
    }
}

#[cfg(test)]
fn preempt_scan_stage_reservation_for_test(live_path: &Path, stage_path: &Path, lock_path: &Path) {
    let sink = RESERVATION_PREEMPTIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(live_path);
    if let Some(sink) = sink {
        install_preempted_scan_stage(stage_path, lock_path, sink);
    }
}

#[cfg(test)]
fn preempt_scan_stage_cleanup_for_test(lock_path: &Path) {
    let sink = CLEANUP_PREEMPTIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(lock_path);
    let Some(sink) = sink else {
        return;
    };
    let bytes = lock_path.as_os_str().as_encoded_bytes();
    let stage_path = PathBuf::from(OsStr::from_bytes(
        bytes.strip_suffix(b".lock").unwrap_or(bytes),
    ));
    install_preempted_scan_stage(&stage_path, lock_path, sink);
}

fn reserve_scan_stage(live_path: &Path) -> ScanResult<(PathBuf, PathBuf, std::fs::File)> {
    static NEXT_STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    loop {
        let sequence = NEXT_STAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stage_name = format!(
            "{}{}-{sequence}",
            scan_stage_prefix(live_path),
            std::process::id()
        );
        let stage_path = live_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(stage_name);
        let lock_path = path_with_suffix(&stage_path, ".lock");
        let lock = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(scan_io(&lock_path, error)),
        };
        #[cfg(test)]
        preempt_scan_stage_reservation_for_test(live_path, &stage_path, &lock_path);
        match try_lock_scan_stage(&lock) {
            Ok(true) => match locked_scan_stage_path_is_current(&lock, &lock_path) {
                Ok(true) => return Ok((stage_path, lock_path, lock)),
                Ok(false) => continue,
                Err(error) => return Err(scan_io(&lock_path, error)),
            },
            Ok(false) => continue,
            Err(error) => {
                return Err(scan_io(&lock_path, error));
            }
        }
    }
}

fn scan_stage_prefix(live_path: &Path) -> String {
    let digest = sha256_hex(live_path.as_os_str().as_encoded_bytes());
    // Retain 160 bits while keeping the transient SQLite filename bounded.
    format!(".rusty-dlna-scan-stage-{}-", &digest[..40])
}

fn try_lock_scan_stage(file: &std::fs::File) -> std::io::Result<bool> {
    // SAFETY: `file` owns a valid descriptor for the duration of this call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

fn locked_scan_stage_path_is_current(file: &std::fs::File, path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let descriptor = file.metadata()?;
    let pathname = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(descriptor.is_file()
        && pathname.is_file()
        && descriptor.nlink() == 1
        && pathname.nlink() == 1
        && descriptor.dev() == pathname.dev()
        && descriptor.ino() == pathname.ino())
}

#[cfg(test)]
fn unlock_scan_stage(file: &std::fs::File) {
    // SAFETY: `file` owns a valid descriptor for the duration of this call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "could not unlock scan stage");
    }
}

fn cleanup_stale_scan_stages(live_path: &Path) {
    let Some(parent) = live_path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let prefix = scan_stage_prefix(live_path).into_bytes();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        let Some(suffix) = bytes.strip_prefix(prefix.as_slice()) else {
            continue;
        };
        let Some(base_suffix) = suffix.strip_suffix(b".lock") else {
            continue;
        };
        let mut parts = base_suffix.split(|byte| *byte == b'-');
        if !parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
            || !parts
                .next()
                .is_some_and(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
            || parts.next().is_some()
        {
            continue;
        }
        let lock_path = entry.path();
        let Ok(lock) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
        else {
            continue;
        };
        if !lock.metadata().is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        if !try_lock_scan_stage(&lock).unwrap_or(false) {
            continue;
        }
        #[cfg(test)]
        preempt_scan_stage_cleanup_for_test(&lock_path);
        if !locked_scan_stage_path_is_current(&lock, &lock_path).unwrap_or(false) {
            continue;
        }
        let base = parent.join(OsStr::from_bytes(
            bytes.strip_suffix(b".lock").unwrap_or(bytes),
        ));
        for path in [
            base.clone(),
            path_with_suffix(&base, "-wal"),
            path_with_suffix(&base, "-shm"),
            path_with_suffix(&base, "-journal"),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => tracing::info!(path = %path.display(), "removed stale scan stage"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "could not remove stale scan stage")
                }
            }
        }
        if let Err(error) = std::fs::remove_file(&lock_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %lock_path.display(), %error, "could not remove stale scan stage lock");
            }
        }
    }
}

fn bookmark_expiry_cutoff(cfg: &ScanConfig) -> Option<i64> {
    (cfg.bookmark_retention_days > 0).then(|| {
        unix_now_seconds()
            .saturating_sub(i64::from(cfg.bookmark_retention_days).saturating_mul(86_400))
    })
}

#[cfg(test)]
mod tests;
