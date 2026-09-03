//! Watch media dirs. File bursts use targeted reconciliation, expanding each
//! physical-directory event through every safe lexical directory alias. Tree
//! changes and overflow still use a bounded full reconciliation.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::{
    ends_with_ci, is_album_art_os_name_for_config, is_caption_os_name, is_probe_sidecar_os_name,
    is_skipped_dir_os_name, is_unfinished_name, looks_like_sample_file, path_excluded,
    path_is_allowed_dir, Catalog, CatalogUpdate, LibraryDb, PreparedCatalogChange, ScanConfig,
    ScanDelta, ScanError, ScanResult, ScanSession,
};

const MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF);

const FULL_RETRY_MIN: Duration = Duration::from_millis(500);
const FULL_RETRY_MAX: Duration = Duration::from_secs(30);

struct PendingFullRetry {
    pending: bool,
    delay: Duration,
    not_before: Instant,
    reset_on_failure: bool,
}

impl PendingFullRetry {
    fn new(now: Instant) -> Self {
        Self {
            pending: false,
            delay: FULL_RETRY_MIN,
            not_before: now,
            reset_on_failure: false,
        }
    }

    fn failed(&mut self, now: Instant) {
        if self.reset_on_failure {
            self.pending = true;
            self.delay = FULL_RETRY_MIN;
            self.reset_on_failure = false;
        } else if self.pending {
            self.delay = self.delay.saturating_mul(2).min(FULL_RETRY_MAX);
        } else {
            self.pending = true;
            self.delay = FULL_RETRY_MIN;
        }
        self.not_before = now + self.delay;
    }

    fn succeeded(&mut self) {
        self.pending = false;
        self.delay = FULL_RETRY_MIN;
        self.reset_on_failure = false;
    }

    fn new_event(&mut self, now: Instant) {
        self.delay = FULL_RETRY_MIN;
        self.not_before = now;
        self.reset_on_failure = self.pending;
    }

    fn ready(&self, now: Instant) -> bool {
        self.pending && now >= self.not_before
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileEventKind {
    Created,
    Updated,
    Deleted,
    MovedFrom,
    MovedTo,
    DirCreated,
    DirRemoved,
    DirMovedFrom,
    DirMovedTo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEvent {
    path: PathBuf,
    cookie: u32,
    kind: FileEventKind,
}

impl FileEventKind {
    fn from_mask(mask: EventMask) -> Option<Self> {
        let dir = mask.contains(EventMask::ISDIR);
        if mask.contains(EventMask::MOVED_FROM) {
            return Some(if dir {
                Self::DirMovedFrom
            } else {
                Self::MovedFrom
            });
        }
        if mask.contains(EventMask::MOVED_TO) {
            return Some(if dir { Self::DirMovedTo } else { Self::MovedTo });
        }
        if mask.contains(EventMask::DELETE) {
            return Some(if dir { Self::DirRemoved } else { Self::Deleted });
        }
        if mask.contains(EventMask::CREATE) {
            return Some(if dir { Self::DirCreated } else { Self::Created });
        }
        if mask.contains(EventMask::CLOSE_WRITE) {
            return Some(Self::Updated);
        }
        None
    }

    fn action(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
            Self::MovedFrom => "moved_away",
            Self::MovedTo => "moved_in",
            Self::DirCreated => "dir_created",
            Self::DirRemoved => "dir_removed",
            Self::DirMovedFrom => "dir_moved_away",
            Self::DirMovedTo => "dir_moved_in",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Created => "file created",
            Self::Updated => "file updated",
            Self::Deleted => "file deleted",
            Self::MovedFrom => "file moved away",
            Self::MovedTo => "file moved in",
            Self::DirCreated => "directory created",
            Self::DirRemoved => "directory removed",
            Self::DirMovedFrom => "directory moved away",
            Self::DirMovedTo => "directory moved in",
        }
    }
}

/// Pair MOVED_FROM + MOVED_TO by cookie so a rename is one relocate.
fn classify_file_events(events: &[FileEvent]) -> Vec<ClassifiedEvent> {
    let mut from: HashMap<u32, &FileEvent> = HashMap::new();
    let mut to: HashMap<u32, &FileEvent> = HashMap::new();
    for ev in events {
        match ev.kind {
            FileEventKind::MovedFrom | FileEventKind::DirMovedFrom if ev.cookie != 0 => {
                from.insert(ev.cookie, ev);
            }
            FileEventKind::MovedTo | FileEventKind::DirMovedTo if ev.cookie != 0 => {
                to.insert(ev.cookie, ev);
            }
            _ => {}
        }
    }
    let mut paired: HashSet<u32> = HashSet::new();
    let mut out = Vec::with_capacity(events.len());
    for (cookie, src) in &from {
        let Some(dst) = to.get(cookie) else {
            continue;
        };
        paired.insert(*cookie);
        let dir = matches!(
            src.kind,
            FileEventKind::DirMovedFrom | FileEventKind::DirMovedTo
        ) || matches!(dst.kind, FileEventKind::DirMovedTo);
        out.push(ClassifiedEvent {
            action: if dir { "dir_relocated" } else { "relocated" },
            message: if dir {
                "directory relocated"
            } else {
                "file relocated"
            },
            path: dst.path.clone(),
            from: Some(src.path.clone()),
            to: Some(dst.path.clone()),
        });
    }
    for ev in events {
        if ev.cookie != 0 && paired.contains(&ev.cookie) {
            continue;
        }
        out.push(ClassifiedEvent {
            action: ev.kind.action(),
            message: ev.kind.message(),
            path: ev.path.clone(),
            from: None,
            to: None,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassifiedEvent {
    action: &'static str,
    message: &'static str,
    path: PathBuf,
    from: Option<PathBuf>,
    to: Option<PathBuf>,
}

fn log_file_events(events: &[FileEvent]) {
    for ev in classify_file_events(events) {
        let file = ev.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        match (&ev.from, &ev.to) {
            (Some(from), Some(to)) => tracing::info!(
                target: "rusty_dlna",
                file,
                action = ev.action,
                from = %from.display(),
                to = %to.display(),
                "{}",
                ev.message
            ),
            _ => tracing::info!(
                target: "rusty_dlna",
                file,
                action = ev.action,
                path = %ev.path.display(),
                "{}",
                ev.message
            ),
        }
    }
}

/// Wait this long after the first event in a burst before applying.
const SETTLE: Duration = Duration::from_millis(250);
const MAX_BATCH_DELAY: Duration = Duration::from_secs(2);
const RECONCILE_AFTER: usize = 256;
const MAX_RETAINED_EVENT_DETAILS: usize = 256;
const MAX_WATCHED_DIRECTORIES: usize = 65_536;
const MAX_PENDING_CREATE_PATHS: usize = RECONCILE_AFTER;

type FileInode = (u64, u64);

#[derive(Default)]
struct PendingCreate {
    /// Lexical names emitted by the first CREATE record for this inode. A later
    /// MOVED_TO can replace them while the writer remains open.
    source_paths: HashSet<PathBuf>,
    /// Policy-eligible names to publish together once CLOSE_WRITE arrives.
    eligible_paths: HashSet<PathBuf>,
    create_events: usize,
    saw_single_link_create: bool,
    /// A rename can make the first CREATE path disappear before a delayed
    /// watcher stats it. In that case MOVED_FROM cannot be correlated, while
    /// MOVED_TO can still be matched through a surviving hard link.
    pending_move_from_count: usize,
    moved_to_without_known_source: bool,
}

#[derive(Default)]
struct PendingCreates {
    by_inode: HashMap<FileInode, PendingCreate>,
    inode_by_path: HashMap<PathBuf, FileInode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreateHistoryDisposition {
    WaitForClose(FileInode),
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveHistoryDisposition {
    Ready,
    WaitForClose(FileInode),
    Overflow,
}

impl PendingCreates {
    fn clear(&mut self) {
        self.by_inode.clear();
        self.inode_by_path.clear();
    }

    fn can_track_paths(&self, paths: &[PathBuf]) -> bool {
        let additional = paths
            .iter()
            .filter(|path| !self.inode_by_path.contains_key(path.as_path()))
            .count();
        self.inode_by_path
            .len()
            .checked_add(additional)
            .is_some_and(|total| total <= MAX_PENDING_CREATE_PATHS)
    }

    fn observe_create(
        &mut self,
        inode: FileInode,
        link_count: u64,
        paths: &[PathBuf],
    ) -> CreateHistoryDisposition {
        if !self.can_track_paths(paths) {
            self.clear();
            return CreateHistoryDisposition::Overflow;
        }
        let following_create = self.by_inode.contains_key(&inode);
        let pending = self.by_inode.entry(inode).or_default();
        pending.create_events = pending.create_events.saturating_add(1);
        pending.saw_single_link_create |= link_count <= 1;
        if !following_create {
            pending.source_paths.extend(paths.iter().cloned());
        }
        for path in paths {
            self.inode_by_path.insert(path.clone(), inode);
        }
        CreateHistoryDisposition::WaitForClose(inode)
    }

    fn remember_eligible(&mut self, inode: FileInode, path: PathBuf) {
        if let Some(pending) = self.by_inode.get_mut(&inode) {
            pending.eligible_paths.insert(path);
        }
    }

    fn observe_moved_to(&mut self, inode: FileInode, paths: &[PathBuf]) -> MoveHistoryDisposition {
        if !self.by_inode.contains_key(&inode) {
            return MoveHistoryDisposition::Ready;
        }
        if !self.can_track_paths(paths) {
            self.clear();
            return MoveHistoryDisposition::Overflow;
        }
        if let Some(pending) = self.by_inode.get_mut(&inode) {
            pending.source_paths.extend(paths.iter().cloned());
            if pending.pending_move_from_count > 0 {
                pending.pending_move_from_count -= 1;
            } else {
                pending.moved_to_without_known_source = true;
            }
        }
        for path in paths {
            self.inode_by_path.insert(path.clone(), inode);
        }
        MoveHistoryDisposition::WaitForClose(inode)
    }

    fn observe_unlink(&mut self, paths: &[PathBuf]) {
        for path in paths {
            let Some(inode) = self.inode_by_path.get(path).copied() else {
                continue;
            };
            if let Some(pending) = self.by_inode.get_mut(&inode) {
                pending.eligible_paths.remove(path);
            }
            // A directory watch still reports CLOSE_WRITE for an unlinked
            // writer. Keep its original name as the close-event correlation
            // anchor even though that name is no longer publishable.
        }
    }

    fn observe_moved_from(&mut self, paths: &[PathBuf]) {
        for path in paths {
            let Some(inode) = self.inode_by_path.get(path).copied() else {
                continue;
            };
            if let Some(pending) = self.by_inode.get_mut(&inode) {
                pending.source_paths.remove(path);
                pending.eligible_paths.remove(path);
                pending.pending_move_from_count = pending.pending_move_from_count.saturating_add(1);
            }
            // Retain `inode_by_path` until close/settle: a close record can
            // still carry a pre-rename pathname that no longer exists.
        }
    }

    fn close_inode(&mut self, paths: &[PathBuf]) -> Option<Vec<PathBuf>> {
        let inode = paths
            .iter()
            .find_map(|path| file_inode(path).or_else(|| self.inode_by_path.get(path).copied()))?;
        Some(self.remove_inode(inode))
    }

    fn remove_inode(&mut self, inode: FileInode) -> Vec<PathBuf> {
        let Some(pending) = self.by_inode.remove(&inode) else {
            return Vec::new();
        };
        self.inode_by_path
            .retain(|_, candidate| *candidate != inode);
        pending.eligible_paths.into_iter().collect()
    }

    fn settle(&mut self, batch: &mut PendingBatch) {
        let mut ready = Vec::new();
        let mut orphaned = false;
        for (inode, pending) in &self.by_inode {
            if pending.source_paths.is_empty() {
                orphaned = true;
                break;
            }
            // A lone nlink>1 CREATE with no preceding local CREATE is link(2)
            // applied to a closed file that may legitimately be unpublished or
            // outside the watched roots. It has no future CLOSE_WRITE here.
            if !pending.saw_single_link_create
                && pending.create_events == 1
                && !pending.moved_to_without_known_source
            {
                ready.push(*inode);
            }
        }
        if orphaned {
            self.clear();
            batch.collapse_to_full();
            return;
        }
        for inode in ready {
            for path in self.remove_inode(inode) {
                batch.add_file(path);
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct WatchTelemetry {
    pub watch_count: AtomicUsize,
    pub overflow_count: AtomicU64,
    /// Settled batches containing a kernel overflow incident. Each increment
    /// means an unknown positive number of records were lost.
    pub dropped_events_total: AtomicU64,
    pub batches: AtomicU64,
    /// Raw kernel records in settled/applied batches. Filtered-only chunks that
    /// require no pending recovery are not included.
    pub events_total: AtomicU64,
    pub pending_paths: AtomicUsize,
    pub pending_paths_peak: AtomicUsize,
    pub last_batch_paths: AtomicUsize,
    pub full_reconciles: AtomicU64,
    pub targeted_batches: AtomicU64,
}

pub fn repair_objects_if_needed(cfg: &ScanConfig) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    if cfg.db_path.is_none() {
        return Ok((None, ScanDelta::default()));
    }
    let mut session = ScanSession::new(cfg)?;
    let prepared = session.prepare_object_repair()?;
    let (update, delta) = session.publish(prepared)?.into_parts();
    let catalog = match update {
        Some(CatalogUpdate::Replacement(catalog)) => Some(catalog),
        Some(CatalogUpdate::Patch(_)) => {
            return Err(ScanError::Invariant(
                "object repair produced an incremental patch".into(),
            ));
        }
        None => None,
    };
    Ok((catalog, delta))
}

pub(crate) fn repair_objects_if_needed_with_db(
    cfg: &ScanConfig,
    db: &LibraryDb,
) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    let missing = db.details_missing_objects()?;
    let dupes = db.folders_have_duplicate_inodes()?;
    if missing > 0 {
        tracing::info!(
            target: "rusty_dlna",
            missing,
            dupes,
            "repairing object tree"
        );
        let next = crate::rebuild_objects_with_db(cfg, db)?;
        return Ok((
            Some(next),
            ScanDelta {
                added: 0,
                removed: 0,
                changed: 1,
            },
        ));
    }
    if dupes {
        tracing::info!(target: "rusty_dlna", "pruning duplicate object aliases");
        let transaction = db.transaction()?;
        let removed = db.prune_duplicate_folder_inodes()?;
        db.prune_empty_folders()?;
        transaction.commit()?;
        tracing::info!(
            target: "rusty_dlna",
            removed,
            "duplicate object aliases pruned"
        );
        return Ok((
            Some(crate::load_catalog_with_policy(db, cfg)?),
            ScanDelta {
                added: 0,
                removed,
                changed: usize::from(removed > 0),
            },
        ));
    }
    Ok((None, ScanDelta::default()))
}

/// Block on inotify. `on_change` is called after a settled burst.
pub fn run_inotify(
    cfg: ScanConfig,
    mut on_change: impl FnMut(Catalog, ScanDelta),
) -> std::io::Result<()> {
    let stopping = AtomicBool::new(false);
    run_inotify_until(cfg, &stopping, None, None, &mut on_change)
}

pub fn run_inotify_until(
    cfg: ScanConfig,
    stopping: &AtomicBool,
    scan_gate: Option<&Mutex<()>>,
    telemetry: Option<&WatchTelemetry>,
    mut on_change: impl FnMut(Catalog, ScanDelta),
) -> std::io::Result<()> {
    let reconcile_cfg = cfg.clone();
    run_inotify_core(
        cfg,
        stopping,
        scan_gate,
        telemetry,
        &mut |fallback, dirty| {
            let result = if fallback {
                crate::monitor(&reconcile_cfg)
            } else {
                crate::monitor_dirty(&reconcile_cfg, dirty)
            };
            if let (Some(catalog), delta) = result? {
                on_change(catalog, delta);
            }
            Ok(())
        },
    )
}

pub fn run_inotify_updates_until(
    cfg: ScanConfig,
    stopping: &AtomicBool,
    scan_gate: Option<&Mutex<()>>,
    telemetry: Option<&WatchTelemetry>,
    mut on_change: impl FnMut(CatalogUpdate, ScanDelta),
) -> std::io::Result<()> {
    let reconcile_cfg = cfg.clone();
    run_inotify_core(
        cfg,
        stopping,
        scan_gate,
        telemetry,
        &mut |fallback, dirty| {
            let result = if fallback {
                crate::monitor_incremental(&reconcile_cfg)
            } else {
                crate::monitor_dirty_incremental(&reconcile_cfg, dirty)
            };
            if let (Some(update), delta) = result? {
                on_change(update, delta);
            }
            Ok(())
        },
    )
}

/// Watch until stopped and synchronously hand each settled batch to a staged
/// publisher. The callback must publish through the supplied session before
/// returning; an error schedules a bounded full reconciliation even when no
/// later filesystem event arrives.
pub fn run_inotify_prepared_updates_until(
    cfg: ScanConfig,
    stopping: &AtomicBool,
    scan_gate: Option<&Mutex<()>>,
    telemetry: Option<&WatchTelemetry>,
    session: &Mutex<Option<ScanSession>>,
    mut on_change: impl FnMut(PreparedCatalogChange) -> ScanResult<()>,
) -> std::io::Result<()> {
    let reconcile_cfg = cfg.clone();
    run_inotify_core(
        cfg,
        stopping,
        scan_gate,
        telemetry,
        &mut |fallback, dirty| {
            let mut session_guard = session.lock().unwrap_or_else(|error| error.into_inner());
            if session_guard.is_none() {
                *session_guard = Some(ScanSession::new(&reconcile_cfg)?);
            }
            let prepared = session_guard
                .as_mut()
                .ok_or_else(|| {
                    ScanError::Invariant("prepared inotify scan session is unavailable".into())
                })?
                .prepare_monitor(if fallback { &[] } else { dirty }, true)?;
            drop(session_guard);
            on_change(prepared)
        },
    )
}

fn run_inotify_core(
    cfg: ScanConfig,
    stopping: &AtomicBool,
    scan_gate: Option<&Mutex<()>>,
    telemetry: Option<&WatchTelemetry>,
    on_reconcile: &mut impl FnMut(bool, &[PathBuf]) -> ScanResult<()>,
) -> std::io::Result<()> {
    let (mut ino, mut wds, mut directory_inodes, mut watched_directories) =
        match build_watch_tree_until(&cfg, stopping) {
            Ok(tree) => tree,
            Err(error) if watch_build_cancelled(&error, &cfg, stopping) => return Ok(()),
            Err(error) => return Err(error),
        };
    if let Some(telemetry) = telemetry {
        telemetry.watch_count.store(wds.len(), Ordering::Relaxed);
    }
    tracing::info!(
        target: "rusty_dlna",
        watches = wds.len(),
        settle_secs = SETTLE.as_secs(),
        "inotify ready"
    );

    let mut buf = [0u8; 65_536];
    let mut full_retry = PendingFullRetry::new(Instant::now());
    let mut pending_creates = PendingCreates::default();
    while !stopping.load(Ordering::Acquire) {
        let mut batch = PendingBatch::default();
        match poll_fd(ino.as_raw_fd(), 500) {
            Ok(false) => {
                if full_retry.ready(Instant::now()) {
                    let _guard = scan_gate
                        .map(|gate| gate.lock().unwrap_or_else(|error| error.into_inner()));
                    if let Err(error) = rebuild_watch_tree_until(
                        &cfg,
                        &mut ino,
                        &mut wds,
                        &mut directory_inodes,
                        &mut watched_directories,
                        stopping,
                    ) {
                        if watch_build_cancelled(&error, &cfg, stopping) {
                            return Ok(());
                        }
                        tracing::error!(%error, "could not rebuild inotify tree; retrying");
                        full_retry.failed(Instant::now());
                    } else if retry_pending_full_reconcile(on_reconcile) {
                        full_retry.failed(Instant::now());
                    } else {
                        full_retry.succeeded();
                    }
                    if let Some(telemetry) = telemetry {
                        telemetry.watch_count.store(wds.len(), Ordering::Relaxed);
                    }
                }
                continue;
            }
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        match ino.read_events(&mut buf) {
            Ok(events) => collect_events(
                events,
                &wds,
                &watched_directories,
                &cfg,
                &mut batch,
                &mut pending_creates,
            ),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        full_retry.new_event(Instant::now());
        observe_pending(telemetry, &batch);
        if !promote_pending_retry_batch(&mut batch, full_retry.pending) {
            continue;
        }
        if full_retry.pending || batch.requires_full_reconcile() {
            pending_creates.clear();
        }

        let maximum = Instant::now() + MAX_BATCH_DELAY;
        let mut deadline = Instant::now() + SETTLE;
        while !stopping.load(Ordering::Acquire) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let ms = left.as_millis().min(i32::MAX as u128) as i32;
            match poll_fd(ino.as_raw_fd(), ms) {
                Ok(false) => break,
                Ok(true) => match ino.read_events(&mut buf) {
                    Ok(events) => {
                        collect_events(
                            events,
                            &wds,
                            &watched_directories,
                            &cfg,
                            &mut batch,
                            &mut pending_creates,
                        );
                        observe_pending(telemetry, &batch);
                        deadline = (Instant::now() + SETTLE).min(maximum);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                },
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if stopping.load(Ordering::Acquire) {
            if let Some(telemetry) = telemetry {
                telemetry.pending_paths.store(0, Ordering::Relaxed);
            }
            break;
        }
        pending_creates.settle(&mut batch);
        batch.awaiting_create_resolution = false;
        if !promote_pending_retry_batch(&mut batch, full_retry.pending) {
            if let Some(telemetry) = telemetry {
                telemetry.pending_paths.store(0, Ordering::Relaxed);
            }
            continue;
        }
        if full_retry.pending || batch.requires_full_reconcile() {
            pending_creates.clear();
        }
        let _guard = scan_gate.map(|gate| gate.lock().unwrap_or_else(|error| error.into_inner()));
        let reconciled = apply_batch(
            &cfg,
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut directory_inodes,
                watched_directories: &mut watched_directories,
            },
            batch,
            telemetry,
            full_retry.pending,
            stopping,
            on_reconcile,
        );
        if reconciled {
            full_retry.succeeded();
        } else {
            full_retry.failed(Instant::now());
        }
        if let Some(telemetry) = telemetry {
            telemetry.pending_paths.store(0, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn promote_pending_retry_batch(batch: &mut PendingBatch, recovery_pending: bool) -> bool {
    if batch.is_empty() && recovery_pending {
        batch.full_only = true;
    }
    !batch.is_empty()
}

fn retry_pending_full_reconcile(
    on_reconcile: &mut impl FnMut(bool, &[PathBuf]) -> ScanResult<()>,
) -> bool {
    match on_reconcile(true, &[]) {
        Ok(()) => false,
        Err(error) => {
            tracing::error!(%error, "pending full inotify reconciliation failed; retrying after poll timeout");
            true
        }
    }
}

fn observe_pending(telemetry: Option<&WatchTelemetry>, batch: &PendingBatch) {
    let Some(telemetry) = telemetry else {
        return;
    };
    let pending = batch.unique_paths();
    telemetry.pending_paths.store(pending, Ordering::Relaxed);
    telemetry
        .pending_paths_peak
        .fetch_max(batch.observed_paths_peak.max(pending), Ordering::Relaxed);
}

fn collect_events<'a>(
    events: impl IntoIterator<Item = inotify::Event<&'a std::ffi::OsStr>>,
    wds: &WatchPaths,
    watched_directories: &WatchedDirectoryPaths,
    cfg: &ScanConfig,
    batch: &mut PendingBatch,
    pending_creates: &mut PendingCreates,
) {
    for ev in events {
        batch.observe_raw_event();
        if ev.mask.contains(EventMask::Q_OVERFLOW) {
            batch.overflow = true;
            batch.collapse_to_full();
            pending_creates.clear();
            continue;
        }
        if batch.full_only {
            continue;
        }
        let Some(name) = ev.name else {
            if ev
                .mask
                .intersects(EventMask::IGNORED | EventMask::DELETE_SELF | EventMask::MOVE_SELF)
            {
                batch.note(
                    wds.get(&ev.wd)
                        .and_then(|paths| paths.first())
                        .cloned()
                        .unwrap_or_default(),
                    ev.cookie,
                    Some(FileEventKind::DirRemoved),
                );
                batch.collapse_to_full();
            }
            continue;
        };
        let Some(dirs) = wds.get(&ev.wd) else {
            continue;
        };
        let paths = dirs.iter().map(|dir| dir.join(name)).collect::<Vec<_>>();
        let regular_identity = paths.iter().find_map(|path| regular_file_identity(path));
        let create_history =
            if ev.mask.contains(EventMask::CREATE) && !ev.mask.contains(EventMask::ISDIR) {
                regular_identity.map(|(inode, link_count)| {
                    let disposition = pending_creates.observe_create(inode, link_count, &paths);
                    batch.awaiting_create_resolution = true;
                    disposition
                })
            } else {
                None
            };
        if matches!(create_history, Some(CreateHistoryDisposition::Overflow)) {
            batch.collapse_to_full();
            continue;
        }
        let move_history =
            if ev.mask.contains(EventMask::MOVED_TO) && !ev.mask.contains(EventMask::ISDIR) {
                regular_identity
                    .map(|(inode, _)| pending_creates.observe_moved_to(inode, &paths))
                    .unwrap_or(MoveHistoryDisposition::Ready)
            } else {
                MoveHistoryDisposition::Ready
            };
        if move_history == MoveHistoryDisposition::Overflow {
            batch.collapse_to_full();
            continue;
        }
        if ev.mask.contains(EventMask::CLOSE_WRITE) {
            if let Some(aliases) = pending_creates.close_inode(&paths) {
                for alias in aliases {
                    batch.add_file(alias);
                }
            }
        }
        if ev.mask.contains(EventMask::DELETE) {
            pending_creates.observe_unlink(&paths);
        }
        if ev.mask.contains(EventMask::MOVED_FROM) {
            pending_creates.observe_moved_from(&paths);
        }

        let display_name = name.to_string_lossy();
        if !cfg.include_hidden && display_name.starts_with('.') {
            continue;
        }
        for path in paths {
            let excluded = path_excluded(&path, &display_name, cfg);
            let kind = FileEventKind::from_mask(ev.mask);
            if ev.mask.contains(EventMask::ISDIR)
                && ev.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
            {
                if excluded || is_skipped_dir_os_name(name) || !path_is_allowed_dir(&path, cfg) {
                    continue;
                }
                batch.note(path.clone(), ev.cookie, kind);
                batch.add_dir(path);
                continue;
            }
            if ev
                .mask
                .intersects(EventMask::DELETE | EventMask::MOVED_FROM)
            {
                let watched_directory_alias = watched_directories.contains(&path);
                if ev.mask.contains(EventMask::ISDIR) || watched_directory_alias {
                    if is_skipped_dir_os_name(name) {
                        continue;
                    }
                    batch.note(path.clone(), ev.cookie, kind);
                    batch.remove_tree(path);
                } else {
                    if !is_sidecar_name(name, cfg) {
                        batch.note(path.clone(), ev.cookie, kind);
                    }
                    batch.remove_file(path);
                }
                continue;
            }
            if drop_create_event(&display_name, excluded) {
                continue;
            }
            if let Some(CreateHistoryDisposition::WaitForClose(inode)) = create_history {
                pending_creates.remember_eligible(inode, path.clone());
            }
            if let MoveHistoryDisposition::WaitForClose(inode) = move_history {
                pending_creates.remember_eligible(inode, path.clone());
            }
            let create_ready = ev.mask.contains(EventMask::CREATE)
                && create_history.is_none()
                && path_is_link_or_dir(&path);
            let apply = ev.mask.contains(EventMask::CLOSE_WRITE)
                || (ev.mask.contains(EventMask::MOVED_TO)
                    && move_history == MoveHistoryDisposition::Ready)
                || create_ready;
            if ev.mask.contains(EventMask::CREATE) && !apply {
                // A newly opened ordinary file waits for CLOSE_WRITE. Creating
                // another name for a published inode has no close event and is
                // admitted above; unpublished aliases follow their writer's
                // eventual close through the pending-create history.
                batch.note(path.clone(), ev.cookie, kind);
            }
            if apply {
                if path.is_dir() {
                    if !is_skipped_dir_os_name(name) && path_is_allowed_dir(&path, cfg) {
                        batch.note(path.clone(), ev.cookie, kind);
                        batch.add_dir(path);
                    }
                } else {
                    batch.note(path.clone(), ev.cookie, kind);
                    batch.add_file(path);
                }
            }
        }
    }
}

type DirectoryInode = (u64, u64);
type WatchPaths = HashMap<WatchDescriptor, Vec<PathBuf>>;
type DirectoryWatches = HashMap<DirectoryInode, WatchDescriptor>;
type WatchedDirectoryPaths = HashSet<PathBuf>;

struct WatchState<'a> {
    ino: &'a mut Inotify,
    wds: &'a mut WatchPaths,
    directory_inodes: &'a mut DirectoryWatches,
    watched_directories: &'a mut WatchedDirectoryPaths,
}

type OwnedWatchTree = (Inotify, WatchPaths, DirectoryWatches, WatchedDirectoryPaths);

fn apply_batch(
    cfg: &ScanConfig,
    watch: WatchState<'_>,
    batch: PendingBatch,
    telemetry: Option<&WatchTelemetry>,
    force_full_reconcile: bool,
    stopping: &AtomicBool,
    on_reconcile: &mut impl FnMut(bool, &[PathBuf]) -> ScanResult<()>,
) -> bool {
    let WatchState {
        ino,
        wds,
        directory_inodes,
        watched_directories,
    } = watch;
    let fallback = force_full_reconcile || batch.requires_full_reconcile();
    if let Some(telemetry) = telemetry {
        record_batch_telemetry(telemetry, &batch, fallback);
    }
    log_file_events(&batch.events);
    tracing::info!(
        target: "rusty_dlna",
        added = batch.add_files.len(),
        removed = batch.remove_files.len() + batch.remove_trees.len(),
        dirs = batch.add_dirs.len(),
        events = batch.raw_events,
        discarded_event_details = batch.discarded_event_details,
        overflow = batch.overflow,
        mode = if fallback { "bounded-full" } else { "targeted" },
        "inotify settled"
    );

    if fallback {
        if let Err(error) = rebuild_watch_tree_until(
            cfg,
            ino,
            wds,
            directory_inodes,
            watched_directories,
            stopping,
        ) {
            if watch_build_cancelled(&error, cfg, stopping) {
                return true;
            }
            tracing::error!(%error, "could not rebuild inotify tree after directory or overflow event");
            return false;
        }
    }
    if let Some(telemetry) = telemetry {
        telemetry.watch_count.store(wds.len(), Ordering::Relaxed);
    }
    // Removed sidecars are just as significant as writes: they must clear
    // metadata/art/captions from the owning item. The path need not still
    // exist; `monitor_dirty` uses its parent and filename as the ownership key.
    let dirty: Vec<PathBuf> = batch
        .add_files
        .iter()
        .chain(batch.remove_files.iter())
        .cloned()
        .collect();
    match on_reconcile(fallback, &dirty) {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(%error, "inotify reconciliation failed; retaining published catalog");
            if fallback {
                return false;
            }
            tracing::warn!("targeted inotify publication failed; forcing full reconciliation");
            match on_reconcile(true, &[]) {
                Ok(()) => true,
                Err(error) => {
                    tracing::error!(%error, "forced full inotify reconciliation failed");
                    false
                }
            }
        }
    }
}

fn record_batch_telemetry(telemetry: &WatchTelemetry, batch: &PendingBatch, fallback: bool) {
    telemetry.batches.fetch_add(1, Ordering::Relaxed);
    telemetry
        .events_total
        .fetch_add(batch.raw_events, Ordering::Relaxed);
    telemetry.last_batch_paths.store(
        batch.observed_paths_peak.max(batch.unique_paths()),
        Ordering::Relaxed,
    );
    if batch.overflow {
        telemetry.overflow_count.fetch_add(1, Ordering::Relaxed);
        telemetry
            .dropped_events_total
            .fetch_add(1, Ordering::Relaxed);
    }
    if fallback {
        telemetry.full_reconciles.fetch_add(1, Ordering::Relaxed);
    } else {
        telemetry.targeted_batches.fetch_add(1, Ordering::Relaxed);
    }
}

fn is_sidecar_name(name: &std::ffi::OsStr, cfg: &ScanConfig) -> bool {
    is_caption_os_name(name)
        || ends_with_ci(name, ".nfo")
        || is_probe_sidecar_os_name(name)
        || is_album_art_os_name_for_config(name, cfg)
}

/// Junk / unfinished / samples are ignored. Poster and NFO writes must
/// reach `add_files` so `monitor_dirty` can attach them without a video rewrite.
fn drop_create_event(name: &str, excluded: bool) -> bool {
    excluded || is_unfinished_name(name) || looks_like_sample_file(name)
}

#[derive(Default, Debug)]
struct PendingBatch {
    overflow: bool,
    full_only: bool,
    awaiting_create_resolution: bool,
    raw_events: u64,
    discarded_event_details: u64,
    observed_paths_peak: usize,
    add_files: HashSet<PathBuf>,
    add_dirs: Vec<PathBuf>,
    remove_files: HashSet<PathBuf>,
    remove_trees: Vec<PathBuf>,
    events: Vec<FileEvent>,
}

impl PendingBatch {
    fn is_empty(&self) -> bool {
        !self.full_only
            && !self.overflow
            && !self.awaiting_create_resolution
            && self.add_files.is_empty()
            && self.add_dirs.is_empty()
            && self.remove_files.is_empty()
            && self.remove_trees.is_empty()
    }

    #[allow(dead_code)]
    fn unique_paths(&self) -> usize {
        self.add_files.len()
            + self.remove_files.len()
            + self.remove_trees.len()
            + self.add_dirs.len()
    }

    #[allow(dead_code)]
    fn should_reconcile(&self) -> bool {
        self.full_only || self.overflow
    }

    fn requires_full_reconcile(&self) -> bool {
        self.should_reconcile() || !self.add_dirs.is_empty() || !self.remove_trees.is_empty()
    }

    fn observe_raw_event(&mut self) {
        self.raw_events = self.raw_events.saturating_add(1);
    }

    fn collapse_to_full(&mut self) {
        self.full_only = true;
        self.awaiting_create_resolution = false;
        self.discarded_event_details = self
            .discarded_event_details
            .saturating_add(self.events.len() as u64);
        self.add_files.clear();
        self.add_dirs.clear();
        self.remove_files.clear();
        self.remove_trees.clear();
        self.events.clear();
    }

    fn enforce_path_bound(&mut self) {
        self.observed_paths_peak = self.observed_paths_peak.max(self.unique_paths());
        if self.unique_paths() >= RECONCILE_AFTER {
            self.collapse_to_full();
        }
    }

    fn add_file(&mut self, path: PathBuf) {
        if self.full_only {
            return;
        }
        self.remove_files.remove(&path);
        self.add_files.insert(path);
        self.enforce_path_bound();
    }

    fn add_dir(&mut self, path: PathBuf) {
        if self.full_only {
            return;
        }
        if !self.add_dirs.iter().any(|p| p == &path) {
            self.add_dirs.push(path);
        }
        self.enforce_path_bound();
    }

    fn remove_file(&mut self, path: PathBuf) {
        if self.full_only {
            return;
        }
        self.add_files.remove(&path);
        self.remove_files.insert(path);
        self.enforce_path_bound();
    }

    fn remove_tree(&mut self, path: PathBuf) {
        if self.full_only {
            return;
        }
        self.add_dirs.retain(|p| !p.starts_with(&path));
        self.add_files.retain(|p| !p.starts_with(&path));
        self.remove_files.retain(|p| !p.starts_with(&path));
        if !self.remove_trees.iter().any(|p| p == &path) {
            self.remove_trees.push(path);
        }
        self.enforce_path_bound();
    }

    fn note(&mut self, path: PathBuf, cookie: u32, kind: Option<FileEventKind>) {
        let Some(kind) = kind else {
            return;
        };
        if self.full_only {
            self.discarded_event_details = self.discarded_event_details.saturating_add(1);
            return;
        }
        if self.events.len() >= MAX_RETAINED_EVENT_DETAILS {
            self.discarded_event_details = self.discarded_event_details.saturating_add(1);
            return;
        }
        self.events.push(FileEvent { path, cookie, kind });
    }
}

fn poll_fd(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    loop {
        // SAFETY: `fds` contains one initialized `pollfd`, and the pointer is
        // valid and writable for the declared element count during the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(n > 0);
    }
}

fn regular_file_identity(path: &Path) -> Option<(FileInode, u64)> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata
        .is_file()
        .then(|| ((metadata.dev(), metadata.ino()), metadata.nlink()))
}

fn file_inode(path: &Path) -> Option<FileInode> {
    regular_file_identity(path).map(|(inode, _)| inode)
}

fn path_is_link_or_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
        .unwrap_or(false)
}

fn watch_build_cancelled(error: &io::Error, cfg: &ScanConfig, stopping: &AtomicBool) -> bool {
    error.kind() == io::ErrorKind::Interrupted
        && (stopping.load(Ordering::Acquire) || cfg.cancellation.is_cancelled())
}

#[cfg(test)]
fn add_tree_watches(
    ino: &mut Inotify,
    wds: &mut WatchPaths,
    directory_inodes: &mut DirectoryWatches,
    watched_directories: &mut WatchedDirectoryPaths,
    cfg: &ScanConfig,
    dir: &Path,
) -> io::Result<()> {
    add_tree_watches_with_limit(
        WatchState {
            ino,
            wds,
            directory_inodes,
            watched_directories,
        },
        cfg,
        dir,
        MAX_WATCHED_DIRECTORIES,
        &mut || false,
        &mut |ino, path| {
            ino.watches()
                .add(path, MASK)
                .map_err(|error| actionable_watch_error(path, error))
        },
    )
}

fn add_tree_watches_with_limit(
    watch: WatchState<'_>,
    cfg: &ScanConfig,
    dir: &Path,
    max_watches: usize,
    should_stop: &mut impl FnMut() -> bool,
    add_watch: &mut impl FnMut(&mut Inotify, &Path) -> io::Result<WatchDescriptor>,
) -> io::Result<()> {
    let WatchState {
        ino,
        wds,
        directory_inodes,
        watched_directories,
    } = watch;
    let mut pending = vec![(dir.to_path_buf(), Vec::<DirectoryInode>::new(), true)];
    while let Some((current, mut ancestors, required)) = pending.pop() {
        if should_stop() || cfg.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "inotify tree construction cancelled",
            ));
        }
        let raw_name = current.file_name().unwrap_or_default();
        let name = raw_name.to_string_lossy();
        let metadata = match std::fs::metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if !required && error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let inode = (metadata.dev(), metadata.ino());
        if !metadata.is_dir() || ancestors.contains(&inode) {
            continue;
        }
        if !path_is_allowed_dir(&current, cfg)
            || path_excluded(&current, &name, cfg)
            || is_skipped_dir_os_name(raw_name)
        {
            continue;
        }
        if !watched_directories.insert(current.clone()) {
            continue;
        }
        if watched_directories.len() > max_watches {
            return Err(io::Error::other(format!(
                "inotify traversal exceeds rustyDLNA's bounded {max_watches}-directory lexical-path budget"
            )));
        }
        let wd = if let Some(wd) = directory_inodes.get(&inode) {
            wd.clone()
        } else {
            if wds.len() >= max_watches {
                return Err(io::Error::other(format!(
                    "inotify tree exceeds rustyDLNA's bounded {max_watches}-directory watch budget"
                )));
            }
            let wd = match add_watch(ino, &current) {
                Ok(wd) => wd,
                Err(error) if !required && error.kind() == io::ErrorKind::NotFound => {
                    watched_directories.remove(&current);
                    continue;
                }
                Err(error) => return Err(error),
            };
            directory_inodes.insert(inode, wd.clone());
            wd
        };
        wds.entry(wd.clone()).or_default().push(current.clone());
        ancestors.push(inode);
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) if !required && error.kind() == io::ErrorKind::NotFound => {
                forget_watched_directory(
                    wds,
                    directory_inodes,
                    watched_directories,
                    inode,
                    &wd,
                    &current,
                );
                continue;
            }
            Err(error) => return Err(error),
        };
        for entry in entries {
            if should_stop() || cfg.cancellation.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "inotify tree construction cancelled",
                ));
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let path = entry.path();
            let raw_child_name = entry.file_name();
            let child_name = raw_child_name.to_string_lossy().into_owned();
            if (!cfg.include_hidden && child_name.starts_with('.'))
                || path_excluded(&path, &child_name, cfg)
                || is_skipped_dir_os_name(&raw_child_name)
            {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if file_type.is_dir() || (file_type.is_symlink() && path.is_dir()) {
                if pending.len().saturating_add(watched_directories.len()) >= max_watches {
                    return Err(io::Error::other(format!(
                        "inotify traversal exceeds rustyDLNA's bounded {max_watches}-directory lexical-path budget"
                    )));
                }
                pending.push((path, ancestors.clone(), false));
            }
        }
    }
    Ok(())
}

fn forget_watched_directory(
    wds: &mut WatchPaths,
    directory_inodes: &mut DirectoryWatches,
    watched_directories: &mut WatchedDirectoryPaths,
    inode: DirectoryInode,
    wd: &WatchDescriptor,
    path: &Path,
) {
    watched_directories.remove(path);
    let remove_watch = if let Some(paths) = wds.get_mut(wd) {
        paths.retain(|candidate| candidate != path);
        paths.is_empty()
    } else {
        false
    };
    if remove_watch {
        wds.remove(wd);
        directory_inodes.remove(&inode);
    }
}

fn actionable_watch_error(path: &Path, error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(libc::ENOSPC) {
        return io::Error::new(
            error.kind(),
            format!(
                "inotify watch capacity exhausted (ENOSPC) while adding {}; increase fs.inotify.max_user_watches as the host administrator and leave headroom for the current and replacement watch trees during an atomic rebuild",
                path.display()
            ),
        );
    }
    error
}

#[cfg(test)]
fn build_watch_tree(cfg: &ScanConfig) -> io::Result<OwnedWatchTree> {
    let stopping = AtomicBool::new(false);
    build_watch_tree_until(cfg, &stopping)
}

fn build_watch_tree_until(cfg: &ScanConfig, stopping: &AtomicBool) -> io::Result<OwnedWatchTree> {
    build_watch_tree_with(
        cfg,
        &mut || stopping.load(Ordering::Acquire),
        &mut |ino, path| {
            ino.watches()
                .add(path, MASK)
                .map_err(|error| actionable_watch_error(path, error))
        },
    )
}

fn build_watch_tree_with(
    cfg: &ScanConfig,
    should_stop: &mut impl FnMut() -> bool,
    add_watch: &mut impl FnMut(&mut Inotify, &Path) -> io::Result<WatchDescriptor>,
) -> io::Result<OwnedWatchTree> {
    let mut ino =
        Inotify::init().map_err(|error| actionable_watch_error(Path::new("inotify"), error))?;
    let mut wds = HashMap::new();
    let mut directory_inodes = HashMap::new();
    let mut watched_directories = HashSet::new();
    for root in &cfg.media_dirs {
        let root = std::fs::canonicalize(root)?;
        add_tree_watches_with_limit(
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut directory_inodes,
                watched_directories: &mut watched_directories,
            },
            cfg,
            &root,
            MAX_WATCHED_DIRECTORIES,
            should_stop,
            add_watch,
        )?;
    }
    Ok((ino, wds, directory_inodes, watched_directories))
}

#[cfg(test)]
fn rebuild_watch_tree(
    cfg: &ScanConfig,
    ino: &mut Inotify,
    wds: &mut WatchPaths,
    directory_inodes: &mut DirectoryWatches,
    watched_directories: &mut WatchedDirectoryPaths,
) -> io::Result<()> {
    let stopping = AtomicBool::new(false);
    rebuild_watch_tree_until(
        cfg,
        ino,
        wds,
        directory_inodes,
        watched_directories,
        &stopping,
    )
}

fn rebuild_watch_tree_until(
    cfg: &ScanConfig,
    ino: &mut Inotify,
    wds: &mut WatchPaths,
    directory_inodes: &mut DirectoryWatches,
    watched_directories: &mut WatchedDirectoryPaths,
    stopping: &AtomicBool,
) -> io::Result<()> {
    let (new_ino, new_wds, new_directory_inodes, new_watched_directories) =
        build_watch_tree_until(cfg, stopping)?;
    *ino = new_ino;
    *wds = new_wds;
    *directory_inodes = new_directory_inodes;
    *watched_directories = new_watched_directories;
    Ok(())
}

#[cfg(test)]
fn rebuild_watch_tree_with(
    cfg: &ScanConfig,
    ino: &mut Inotify,
    wds: &mut WatchPaths,
    directory_inodes: &mut DirectoryWatches,
    watched_directories: &mut WatchedDirectoryPaths,
    add_watch: &mut impl FnMut(&mut Inotify, &Path) -> io::Result<WatchDescriptor>,
) -> io::Result<()> {
    let (new_ino, new_wds, new_directory_inodes, new_watched_directories) =
        build_watch_tree_with(cfg, &mut || false, add_watch)?;
    *ino = new_ino;
    *wds = new_wds;
    *directory_inodes = new_directory_inodes;
    *watched_directories = new_watched_directories;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{mpsc, Arc};
    use std::thread;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "rusty-dlna-{label}-{}-{sequence}",
                std::process::id()
            )))
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

    fn fake_mkv(path: &Path) {
        fake_mkv_with_len(path, 64);
    }

    fn fake_mkv_with_len(path: &Path, len: usize) {
        let mut bytes = vec![0u8; len.max(4)];
        bytes[..4].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3]);
        std::fs::write(path, bytes).unwrap();
    }

    struct RunningWatcher {
        stopping: Arc<AtomicBool>,
        telemetry: Arc<WatchTelemetry>,
        updates: mpsc::Receiver<(Catalog, ScanDelta)>,
        handle: Option<thread::JoinHandle<io::Result<()>>>,
    }

    impl RunningWatcher {
        fn start(cfg: ScanConfig, minimum_watches: usize) -> Self {
            let stopping = Arc::new(AtomicBool::new(false));
            let telemetry = Arc::new(WatchTelemetry::default());
            let (send, updates) = mpsc::channel();
            let thread_stopping = Arc::clone(&stopping);
            let thread_telemetry = Arc::clone(&telemetry);
            let handle = thread::spawn(move || {
                run_inotify_until(
                    cfg,
                    thread_stopping.as_ref(),
                    None,
                    Some(thread_telemetry.as_ref()),
                    move |catalog, delta| {
                        let _ = send.send((catalog, delta));
                    },
                )
            });
            let watcher = Self {
                stopping,
                telemetry,
                updates,
                handle: Some(handle),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while watcher.telemetry.watch_count.load(Ordering::Acquire) < minimum_watches {
                assert!(
                    Instant::now() < deadline,
                    "inotify watcher did not become ready"
                );
                assert!(
                    !watcher
                        .handle
                        .as_ref()
                        .is_some_and(thread::JoinHandle::is_finished),
                    "inotify watcher exited before becoming ready"
                );
                thread::sleep(Duration::from_millis(5));
            }
            watcher
        }

        fn wait_for_catalog(
            &self,
            mut predicate: impl FnMut(&Catalog) -> bool,
        ) -> (Catalog, ScanDelta) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let update = self
                    .updates
                    .recv_timeout(remaining)
                    .expect("inotify watcher did not publish the expected catalog");
                if predicate(&update.0) {
                    return update;
                }
            }
        }

        fn finish(mut self) {
            self.stopping.store(true, Ordering::Release);
            let result = self
                .handle
                .take()
                .expect("watcher thread handle")
                .join()
                .expect("watcher thread panicked");
            result.expect("watcher stopped with an error");
        }
    }

    impl Drop for RunningWatcher {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn rename_in_one_burst_is_remove_then_add() {
        let mut b = PendingBatch::default();
        b.remove_file(PathBuf::from("/v/old.mkv"));
        b.add_file(PathBuf::from("/v/new.mkv"));
        assert!(b.remove_files.contains(Path::new("/v/old.mkv")));
        assert!(b.add_files.contains(Path::new("/v/new.mkv")));
        assert!(!b.should_reconcile());
    }

    #[test]
    fn create_then_delete_same_path_is_just_a_remove() {
        let mut b = PendingBatch::default();
        b.add_file(PathBuf::from("/v/tmp.mkv"));
        b.remove_file(PathBuf::from("/v/tmp.mkv"));
        assert!(b.add_files.is_empty());
        assert!(b.remove_files.contains(Path::new("/v/tmp.mkv")));
    }

    #[test]
    fn delete_then_recreate_same_path_is_just_an_add() {
        let mut b = PendingBatch::default();
        b.remove_file(PathBuf::from("/v/ep.mkv"));
        b.add_file(PathBuf::from("/v/ep.mkv"));
        assert!(b.remove_files.is_empty());
        assert!(b.add_files.contains(Path::new("/v/ep.mkv")));
    }

    #[test]
    fn pending_create_history_tracks_rename_unlink_and_orphan_fallbacks() {
        let inode = (7, 11);
        let original = PathBuf::from("/v/original.mkv");
        let alias = PathBuf::from("/v/alias.mkv");
        let renamed = PathBuf::from("/v/renamed.mkv");
        let mut pending = PendingCreates::default();
        assert_eq!(
            pending.observe_create(inode, 1, std::slice::from_ref(&original)),
            CreateHistoryDisposition::WaitForClose(inode)
        );
        pending.remember_eligible(inode, original.clone());
        assert_eq!(
            pending.observe_create(inode, 2, std::slice::from_ref(&alias)),
            CreateHistoryDisposition::WaitForClose(inode)
        );
        pending.remember_eligible(inode, alias.clone());

        pending.observe_moved_from(std::slice::from_ref(&original));
        assert_eq!(
            pending.observe_moved_to(inode, std::slice::from_ref(&renamed)),
            MoveHistoryDisposition::WaitForClose(inode)
        );
        pending.remember_eligible(inode, renamed.clone());
        let mut batch = PendingBatch::default();
        pending.settle(&mut batch);
        assert!(!batch.requires_full_reconcile());
        let mut closed = pending.remove_inode(inode);
        closed.sort();
        assert_eq!(closed, [alias.clone(), renamed.clone()]);
        assert!(pending.by_inode.is_empty());
        assert!(pending.inode_by_path.is_empty());

        // If event consumption is delayed until the first CREATE name has
        // already been renamed, only the hard-link CREATE can be statted.
        // MOVED_TO must keep that ambiguous inode pending until CLOSE_WRITE.
        let mut pending = PendingCreates::default();
        pending.observe_create(inode, 2, std::slice::from_ref(&alias));
        pending.remember_eligible(inode, alias.clone());
        assert_eq!(
            pending.observe_moved_to(inode, std::slice::from_ref(&renamed)),
            MoveHistoryDisposition::WaitForClose(inode)
        );
        pending.remember_eligible(inode, renamed.clone());
        let mut batch = PendingBatch::default();
        pending.settle(&mut batch);
        assert!(batch.add_files.is_empty());
        let closed = pending.remove_inode(inode);
        assert_eq!(
            closed.into_iter().collect::<HashSet<_>>(),
            [alias.clone(), renamed.clone()].into()
        );

        // Renaming a closed hard link has a correlated MOVED_FROM and still
        // has no CLOSE_WRITE to wait for.
        let mut pending = PendingCreates::default();
        pending.observe_create(inode, 2, std::slice::from_ref(&alias));
        pending.remember_eligible(inode, alias.clone());
        pending.observe_moved_from(std::slice::from_ref(&alias));
        assert_eq!(
            pending.observe_moved_to(inode, std::slice::from_ref(&renamed)),
            MoveHistoryDisposition::WaitForClose(inode)
        );
        pending.remember_eligible(inode, renamed.clone());
        let mut batch = PendingBatch::default();
        pending.settle(&mut batch);
        assert_eq!(batch.add_files, [renamed.clone()].into());
        assert!(pending.by_inode.is_empty());

        let mut pending = PendingCreates::default();
        pending.observe_create(inode, 1, std::slice::from_ref(&original));
        pending.remember_eligible(inode, original.clone());
        pending.observe_create(inode, 2, std::slice::from_ref(&alias));
        pending.remember_eligible(inode, alias.clone());
        pending.observe_unlink(std::slice::from_ref(&original));
        let mut batch = PendingBatch::default();
        pending.settle(&mut batch);
        assert!(!batch.requires_full_reconcile());
        let closed = pending.remove_inode(inode);
        assert_eq!(closed.as_slice(), std::slice::from_ref(&alias));

        let mut pending = PendingCreates::default();
        pending.observe_create(inode, 1, std::slice::from_ref(&original));
        pending.remember_eligible(inode, alias);
        pending.observe_moved_from(std::slice::from_ref(&original));
        let mut batch = PendingBatch::default();
        pending.settle(&mut batch);
        assert!(batch.requires_full_reconcile());
        assert!(pending.by_inode.is_empty());
        assert!(pending.inode_by_path.is_empty());
    }

    #[test]
    fn pending_create_history_overflow_is_bounded_and_explicit() {
        let mut pending = PendingCreates::default();
        let paths = (0..MAX_PENDING_CREATE_PATHS)
            .map(|index| PathBuf::from(format!("/v/{index}.mkv")))
            .collect::<Vec<_>>();
        assert_eq!(
            pending.observe_create((1, 1), 1, &paths),
            CreateHistoryDisposition::WaitForClose((1, 1))
        );
        assert_eq!(pending.inode_by_path.len(), MAX_PENDING_CREATE_PATHS);
        assert_eq!(
            pending.observe_create((1, 2), 1, &[PathBuf::from("/v/overflow.mkv")]),
            CreateHistoryDisposition::Overflow
        );
        assert!(pending.by_inode.is_empty());
        assert!(pending.inode_by_path.is_empty());
    }

    #[test]
    fn poster_and_nfo_writes_are_not_dropped() {
        let cfg = ScanConfig::default();
        assert!(is_sidecar_name(
            std::ffi::OsStr::new("clip-poster.jpg"),
            &cfg
        ));
        assert!(is_sidecar_name(std::ffi::OsStr::new("movie.nfo"), &cfg));
        assert!(is_sidecar_name(std::ffi::OsStr::new("tvshow.nfo"), &cfg));
        assert!(!drop_create_event("clip-poster.jpg", false));
        assert!(!drop_create_event("movie.nfo", false));
        assert!(!drop_create_event("tvshow.nfo", false));
        assert!(drop_create_event("unfinished.mkv.part", false));
        assert!(drop_create_event("clip-poster.jpg", true));
    }

    #[test]
    fn tree_delete_swallows_child_file_events() {
        let mut b = PendingBatch::default();
        b.remove_file(PathBuf::from("/v/genres/a.mkv"));
        b.add_file(PathBuf::from("/v/genres/b.mkv"));
        b.remove_tree(PathBuf::from("/v/genres"));
        assert!(b.remove_files.is_empty());
        assert!(b.add_files.is_empty());
        assert_eq!(b.remove_trees, [PathBuf::from("/v/genres")]);
    }

    #[test]
    fn rename_pair_is_one_relocate() {
        let events = [
            FileEvent {
                path: PathBuf::from("/v/old.mkv"),
                cookie: 7,
                kind: FileEventKind::MovedFrom,
            },
            FileEvent {
                path: PathBuf::from("/v/new.mkv"),
                cookie: 7,
                kind: FileEventKind::MovedTo,
            },
        ];
        let got = classify_file_events(&events);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].action, "relocated");
        assert_eq!(got[0].from.as_deref(), Some(Path::new("/v/old.mkv")));
        assert_eq!(got[0].to.as_deref(), Some(Path::new("/v/new.mkv")));
    }

    #[test]
    fn unpaired_move_and_delete_stay_separate() {
        let events = [
            FileEvent {
                path: PathBuf::from("/v/gone.mkv"),
                cookie: 1,
                kind: FileEventKind::MovedFrom,
            },
            FileEvent {
                path: PathBuf::from("/v/dead.mkv"),
                cookie: 0,
                kind: FileEventKind::Deleted,
            },
            FileEvent {
                path: PathBuf::from("/v/wrote.mkv"),
                cookie: 0,
                kind: FileEventKind::Updated,
            },
        ];
        let got = classify_file_events(&events);
        let actions: Vec<_> = got.iter().map(|e| e.action).collect();
        assert_eq!(actions, ["moved_away", "deleted", "updated"]);
    }

    #[test]
    fn overflow_or_large_burst_reconciles() {
        let b = PendingBatch {
            overflow: true,
            ..PendingBatch::default()
        };
        assert!(b.should_reconcile());
        let mut b = PendingBatch::default();
        for i in 0..RECONCILE_AFTER {
            b.remove_file(PathBuf::from(format!("/v/{i}.mkv")));
        }
        assert!(b.should_reconcile());
        assert!(
            b.remove_files.is_empty(),
            "full-only state drops path payload"
        );
        let mut b = PendingBatch::default();
        b.remove_file(PathBuf::from("/v/one.mkv"));
        assert!(!b.should_reconcile());
    }

    #[test]
    fn repeated_and_unique_event_floods_have_constant_retained_payload() {
        let repeated = PathBuf::from("/v/repeated.mkv");
        let mut duplicate_batch = PendingBatch::default();
        for _ in 0..100_000 {
            duplicate_batch.observe_raw_event();
            duplicate_batch.note(repeated.clone(), 0, Some(FileEventKind::Updated));
            duplicate_batch.add_file(repeated.clone());
        }
        assert!(!duplicate_batch.should_reconcile());
        assert_eq!(duplicate_batch.raw_events, 100_000);
        assert_eq!(duplicate_batch.events.len(), MAX_RETAINED_EVENT_DETAILS);
        assert_eq!(duplicate_batch.add_files.len(), 1);
        assert!(duplicate_batch.discarded_event_details > 99_000);

        let mut unique_batch = PendingBatch::default();
        for index in 0..100_000 {
            unique_batch.observe_raw_event();
            let path = PathBuf::from(format!("/v/{index}.mkv"));
            unique_batch.note(path.clone(), 0, Some(FileEventKind::Updated));
            unique_batch.add_file(path);
        }
        assert!(unique_batch.should_reconcile());
        assert_eq!(unique_batch.raw_events, 100_000);
        assert!(unique_batch.events.is_empty());
        assert!(unique_batch.add_files.is_empty());

        let telemetry = WatchTelemetry::default();
        record_batch_telemetry(&telemetry, &unique_batch, true);
        assert_eq!(telemetry.events_total.load(Ordering::Relaxed), 100_000);
        assert_eq!(telemetry.dropped_events_total.load(Ordering::Relaxed), 0);
        assert!(unique_batch.discarded_event_details > 99_000);
    }

    #[test]
    fn pending_backlog_telemetry_is_bounded_and_records_peaks() {
        let telemetry = WatchTelemetry::default();
        let mut batch = PendingBatch::default();
        batch.add_file(PathBuf::from("/v/one.mkv"));
        batch.add_file(PathBuf::from("/v/two.mkv"));
        observe_pending(Some(&telemetry), &batch);
        assert_eq!(telemetry.pending_paths.load(Ordering::Relaxed), 2);
        assert_eq!(telemetry.pending_paths_peak.load(Ordering::Relaxed), 2);

        batch.remove_file(PathBuf::from("/v/one.mkv"));
        observe_pending(Some(&telemetry), &batch);
        assert_eq!(telemetry.pending_paths.load(Ordering::Relaxed), 2);
        assert_eq!(telemetry.pending_paths_peak.load(Ordering::Relaxed), 2);

        let empty = PendingBatch::default();
        observe_pending(Some(&telemetry), &empty);
        assert_eq!(telemetry.pending_paths.load(Ordering::Relaxed), 0);
        assert_eq!(telemetry.pending_paths_peak.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn overflow_telemetry_records_dropped_marker_and_full_reconcile() {
        let telemetry = WatchTelemetry::default();
        let batch = PendingBatch {
            overflow: true,
            ..PendingBatch::default()
        };
        record_batch_telemetry(&telemetry, &batch, true);
        assert_eq!(telemetry.batches.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.overflow_count.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.dropped_events_total.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.full_reconciles.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.targeted_batches.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn watch_tree_rebuild_is_atomic_and_does_not_accumulate_descriptors() {
        let tmp = TempDir::new("watch-rebuild-atomic");
        let root = tmp.join("media");
        std::fs::create_dir_all(root.join("first")).unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            ..ScanConfig::default()
        };

        let (mut ino, mut wds, mut inodes, mut watched_directories) =
            build_watch_tree(&cfg).unwrap();
        assert_eq!(wds.len(), 2);
        std::fs::rename(root.join("first"), root.join("second")).unwrap();
        for _ in 0..8 {
            rebuild_watch_tree(
                &cfg,
                &mut ino,
                &mut wds,
                &mut inodes,
                &mut watched_directories,
            )
            .unwrap();
            assert_eq!(wds.len(), 2);
            assert!(wds.values().flatten().any(|path| path.ends_with("second")));
            assert_eq!(inodes.len(), 2);
        }

        let old_fd = ino.as_raw_fd();
        let old_paths = wds.values().cloned().collect::<HashSet<_>>();
        let old_inodes = inodes.clone();
        let mut additions = 0usize;
        let failed = rebuild_watch_tree_with(
            &cfg,
            &mut ino,
            &mut wds,
            &mut inodes,
            &mut watched_directories,
            &mut |candidate, path| {
                additions += 1;
                if additions == 2 {
                    return Err(io::Error::other("injected watch-add failure"));
                }
                candidate.watches().add(path, MASK)
            },
        );
        assert!(failed.is_err());
        assert_eq!(ino.as_raw_fd(), old_fd);
        assert_eq!(wds.values().cloned().collect::<HashSet<_>>(), old_paths);
        assert_eq!(inodes, old_inodes);
    }

    #[test]
    fn pending_full_retry_rebuilds_before_an_ordinary_file_event_reconcile() {
        let tmp = TempDir::new("watch-rebuild-retry-event");
        let root = tmp.join("media");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            ..ScanConfig::default()
        };
        let (mut ino, mut wds, mut inodes, mut watched_directories) =
            build_watch_tree(&cfg).unwrap();

        let mut additions = 0usize;
        assert!(rebuild_watch_tree_with(
            &cfg,
            &mut ino,
            &mut wds,
            &mut inodes,
            &mut watched_directories,
            &mut |candidate, path| {
                additions += 1;
                if additions == 2 {
                    return Err(io::Error::other("injected rebuild failure"));
                }
                candidate.watches().add(path, MASK)
            },
        )
        .is_err());

        std::fs::create_dir_all(root.join("new-after-failure")).unwrap();
        let mut batch = PendingBatch::default();
        batch.add_file(root.join("ordinary.mkv"));
        let mut calls = Vec::new();
        assert!(apply_batch(
            &cfg,
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut inodes,
                watched_directories: &mut watched_directories,
            },
            batch,
            None,
            true,
            &AtomicBool::new(false),
            &mut |fallback, dirty| {
                calls.push((fallback, dirty.to_vec()));
                Ok(())
            },
        ));
        assert_eq!(calls, [(true, vec![root.join("ordinary.mkv")])]);
        assert!(
            wds.values()
                .flatten()
                .any(|path| path.ends_with("new-after-failure")),
            "forced full retry must rebuild the stale watch tree first"
        );
    }

    #[test]
    fn watch_tree_traversal_has_a_user_space_directory_bound() {
        let tmp = TempDir::new("watch-tree-bound");
        let root = tmp.join("media");
        std::fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            ..ScanConfig::default()
        };
        let mut ino = Inotify::init().unwrap();
        let mut wds = HashMap::new();
        let mut inodes = HashMap::new();
        let mut watched_directories = HashSet::new();
        let error = add_tree_watches_with_limit(
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut inodes,
                watched_directories: &mut watched_directories,
            },
            &cfg,
            &root,
            3,
            &mut || false,
            &mut |candidate, path| candidate.watches().add(path, MASK),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("bounded 3-directory lexical-path budget"));
        assert!(wds.len() <= 3);
        assert!(inodes.len() <= 3);
    }

    #[test]
    fn watch_tree_skips_disappearing_children_but_requires_its_configured_root() {
        let tmp = TempDir::new("watch-tree-disappearing-child");
        let root = tmp.join("media");
        let gone_before_watch = root.join("gone-before-watch");
        let gone_after_watch = root.join("gone-after-watch");
        std::fs::create_dir_all(&gone_before_watch).unwrap();
        std::fs::create_dir_all(&gone_after_watch).unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            ..ScanConfig::default()
        };
        let mut ino = Inotify::init().unwrap();
        let mut wds = HashMap::new();
        let mut inodes = HashMap::new();
        let mut watched_directories = HashSet::new();
        add_tree_watches_with_limit(
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut inodes,
                watched_directories: &mut watched_directories,
            },
            &cfg,
            &root,
            MAX_WATCHED_DIRECTORIES,
            &mut || false,
            &mut |candidate, path| {
                if path == gone_before_watch {
                    std::fs::remove_dir(path)?;
                }
                let wd = candidate.watches().add(path, MASK)?;
                if path == gone_after_watch {
                    std::fs::remove_dir(path)?;
                }
                Ok(wd)
            },
        )
        .unwrap();
        assert_eq!(watched_directories, HashSet::from([root.clone()]));
        assert_eq!(wds.len(), 1);
        assert_eq!(inodes.len(), 1);

        let missing = tmp.join("missing-root");
        let mut ino = Inotify::init().unwrap();
        let error = add_tree_watches_with_limit(
            WatchState {
                ino: &mut ino,
                wds: &mut HashMap::new(),
                directory_inodes: &mut HashMap::new(),
                watched_directories: &mut HashSet::new(),
            },
            &cfg,
            &missing,
            MAX_WATCHED_DIRECTORIES,
            &mut || false,
            &mut |candidate, path| candidate.watches().add(path, MASK),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn watch_tree_initial_and_replacement_builds_observe_shutdown() {
        let tmp = TempDir::new("watch-tree-cancel");
        let root = tmp.join("media");
        for index in 0..32 {
            std::fs::create_dir_all(root.join(format!("branch-{index}/child"))).unwrap();
        }
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            ..ScanConfig::default()
        };
        let stopping = AtomicBool::new(true);
        let error = build_watch_tree_until(&cfg, &stopping).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);

        stopping.store(false, Ordering::Release);
        let (mut ino, mut wds, mut inodes, mut watched_directories) =
            build_watch_tree_until(&cfg, &stopping).unwrap();
        let old_fd = ino.as_raw_fd();
        let old_paths = wds.values().cloned().collect::<HashSet<_>>();
        stopping.store(true, Ordering::Release);
        let error = rebuild_watch_tree_until(
            &cfg,
            &mut ino,
            &mut wds,
            &mut inodes,
            &mut watched_directories,
            &stopping,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(ino.as_raw_fd(), old_fd);
        assert_eq!(wds.values().cloned().collect::<HashSet<_>>(), old_paths);

        let mut ino = Inotify::init().unwrap();
        let mut partial_wds = HashMap::new();
        let mut partial_inodes = HashMap::new();
        let mut partial_watched_directories = HashSet::new();
        let mut checks = 0usize;
        let error = add_tree_watches_with_limit(
            WatchState {
                ino: &mut ino,
                wds: &mut partial_wds,
                directory_inodes: &mut partial_inodes,
                watched_directories: &mut partial_watched_directories,
            },
            &cfg,
            &root,
            MAX_WATCHED_DIRECTORIES,
            &mut || {
                checks += 1;
                checks > 4
            },
            &mut |candidate, path| candidate.watches().add(path, MASK),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(checks > 4, "cancellation must occur after traversal starts");
        assert!(
            !partial_wds.is_empty(),
            "the partial traversal must have installed at least one watch"
        );
        assert!(
            partial_wds.len() < 65,
            "cancellation must bound partial work"
        );
    }

    #[test]
    fn public_watcher_startup_cancellation_is_a_graceful_shutdown() {
        let tmp = TempDir::new("watch-startup-cancel");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root],
            ..ScanConfig::default()
        };
        let stopping = AtomicBool::new(true);
        let result = run_inotify_until(cfg.clone(), &stopping, None, None, |_, _| {
            panic!("a cancelled watcher must never reconcile")
        });
        assert!(result.is_ok());

        cfg.cancellation.cancel();
        let stopping = AtomicBool::new(false);
        let result = run_inotify_until(cfg, &stopping, None, None, |_, _| {
            panic!("a cancelled watcher must never reconcile")
        });
        assert!(result.is_ok());
    }

    #[test]
    fn nameless_watched_directory_self_event_forces_full_recovery() {
        let tmp = TempDir::new("watch-self-event");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            ..ScanConfig::default()
        };
        let mut ino = Inotify::init().unwrap();
        let wd = ino.watches().add(&root, MASK).unwrap();
        let wds = HashMap::from([(wd, vec![root.clone()])]);
        std::fs::remove_dir(&root).unwrap();
        assert!(poll_fd(ino.as_raw_fd(), 2_000).unwrap());
        let mut buffer = [0u8; 4096];
        let events = ino.read_events(&mut buffer).unwrap();
        let mut batch = PendingBatch::default();
        let watched_directories = HashSet::from([root.clone()]);
        collect_events(
            events,
            &wds,
            &watched_directories,
            &cfg,
            &mut batch,
            &mut PendingCreates::default(),
        );
        assert!(batch.should_reconcile());
        assert!(batch.raw_events > 0);
        assert!(
            batch.events.is_empty(),
            "full-only state clears diagnostics"
        );
    }

    #[test]
    fn watch_capacity_error_is_actionable_without_mutating_sysctls() {
        let error = actionable_watch_error(
            Path::new("/media/example"),
            io::Error::from_raw_os_error(libc::ENOSPC),
        );
        let message = error.to_string();
        assert!(message.contains("ENOSPC"));
        assert!(message.contains("fs.inotify.max_user_watches"));
        assert!(message.contains("host administrator"));
        assert!(message.contains("current and replacement watch trees"));
    }

    #[test]
    fn failed_targeted_and_immediate_full_reconcile_retries_without_another_event() {
        let cfg = ScanConfig::default();
        let mut ino = Inotify::init().unwrap();
        let mut wds = HashMap::new();
        let mut directory_inodes = HashMap::new();
        let mut watched_directories = HashSet::new();
        let mut batch = PendingBatch::default();
        batch.add_file(PathBuf::from("/media/new.mkv"));
        let mut calls = Vec::new();
        let mut reconcile = |fallback: bool, dirty: &[PathBuf]| {
            calls.push((fallback, dirty.to_vec()));
            if calls.len() < 3 {
                Err(ScanError::Invariant("injected publication failure".into()))
            } else {
                Ok(())
            }
        };

        let settled = apply_batch(
            &cfg,
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut directory_inodes,
                watched_directories: &mut watched_directories,
            },
            batch,
            None,
            false,
            &AtomicBool::new(false),
            &mut reconcile,
        );
        assert!(!settled, "both immediate publication attempts failed");

        let remains_pending = retry_pending_full_reconcile(&mut reconcile);
        assert!(
            !remains_pending,
            "poll-timeout retry must eventually settle"
        );
        assert_eq!(calls.len(), 3);
        assert!(!calls[0].0);
        assert_eq!(calls[0].1, [PathBuf::from("/media/new.mkv")]);
        assert!(calls[1].0 && calls[1].1.is_empty());
        assert!(calls[2].0 && calls[2].1.is_empty());
    }

    #[test]
    fn pending_full_reconcile_uses_bounded_backoff_and_resets_on_events() {
        let start = Instant::now();
        let mut retry = PendingFullRetry::new(start);
        retry.failed(start);
        assert!(!retry.ready(start + FULL_RETRY_MIN / 2));
        assert!(retry.ready(start + FULL_RETRY_MIN));
        retry.failed(start + FULL_RETRY_MIN);
        assert_eq!(retry.delay, Duration::from_secs(1));
        for step in 0..10 {
            retry.failed(start + Duration::from_secs(step + 2));
        }
        assert_eq!(retry.delay, FULL_RETRY_MAX);
        retry.new_event(start + Duration::from_secs(20));
        assert_eq!(retry.delay, FULL_RETRY_MIN);
        assert!(retry.ready(start + Duration::from_secs(20)));
        retry.failed(start + Duration::from_secs(20));
        assert_eq!(
            retry.delay, FULL_RETRY_MIN,
            "a failed attempt caused by a new event restarts at the minimum delay"
        );
        retry.succeeded();
        assert!(!retry.pending);
    }

    #[test]
    fn filtered_events_cannot_starve_a_pending_full_reconcile() {
        let mut filtered = PendingBatch::default();
        filtered.observe_raw_event();
        assert!(!promote_pending_retry_batch(&mut filtered, false));
        assert!(promote_pending_retry_batch(&mut filtered, true));
        assert!(filtered.should_reconcile());
        assert_eq!(filtered.raw_events, 1);
    }

    #[test]
    fn consumed_event_recovers_a_failed_prepared_publication_without_a_new_event() {
        let tmp = TempDir::new("watch-prepared-retry");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        fake_mkv(&root.join("base.mkv"));
        let added = root.join("added.mkv");
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..Default::default()
        };
        crate::scan(&cfg).unwrap();
        fake_mkv(&added);
        let wrong = LibraryDb::open(&tmp.join("wrong.db")).unwrap();
        let mut session = ScanSession::new(&cfg).unwrap();
        let mut attempts = 0usize;
        {
            let mut reconcile = |fallback: bool, dirty: &[PathBuf]| {
                attempts += 1;
                let prepared = session.prepare_monitor(if fallback { &[] } else { dirty }, true)?;
                if attempts < 3 {
                    session.publish_to_db(&wrong, prepared, None)?;
                } else {
                    session.publish(prepared)?;
                }
                Ok(())
            };
            let mut ino = Inotify::init().unwrap();
            let mut wds = HashMap::new();
            let mut directory_inodes = HashMap::new();
            let mut watched_directories = HashSet::new();
            let mut batch = PendingBatch::default();
            batch.add_file(added.clone());
            assert!(!apply_batch(
                &cfg,
                WatchState {
                    ino: &mut ino,
                    wds: &mut wds,
                    directory_inodes: &mut directory_inodes,
                    watched_directories: &mut watched_directories,
                },
                batch,
                None,
                false,
                &AtomicBool::new(false),
                &mut reconcile,
            ));
            assert!(!retry_pending_full_reconcile(&mut reconcile));
        }
        assert_eq!(attempts, 3);
        assert_eq!(session.backup_count(), 3);
        assert!(LibraryDb::open(cfg.db_path.as_ref().unwrap())
            .unwrap()
            .find_detail_by_path(&crate::path_to_db(&added))
            .unwrap()
            .is_some());
    }

    #[test]
    fn real_inotify_indexes_a_new_hard_link_without_close_write() {
        let tmp = TempDir::new("watch-hard-link-create");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("original.mkv");
        let alias = root.join("alias.mkv");
        fake_mkv(&original);
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 1);

        std::fs::hard_link(&original, &alias).unwrap();
        let (catalog, delta) = watcher
            .wait_for_catalog(|catalog| catalog.items.values().any(|item| item.path == alias));
        assert!(delta.added > 0 || delta.changed > 0, "{delta:?}");
        assert!(catalog.items.values().any(|item| item.path == original));
        assert_eq!(
            watcher.telemetry.full_reconciles.load(Ordering::Acquire),
            0,
            "a hard-link CREATE must stay targeted"
        );
        assert!(watcher.telemetry.targeted_batches.load(Ordering::Acquire) > 0);
        watcher.finish();
    }

    #[test]
    fn real_inotify_indexes_a_closed_hard_link_from_outside_the_catalog() {
        let tmp = TempDir::new("watch-outside-hard-link-create");
        let root = tmp.join("media");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let source = outside.join("source.mkv");
        let alias = root.join("alias.mkv");
        fake_mkv(&source);
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 1);

        std::fs::hard_link(&source, &alias).unwrap();
        let (catalog, delta) = watcher
            .wait_for_catalog(|catalog| catalog.items.values().any(|item| item.path == alias));
        assert!(delta.added > 0, "{delta:?}");
        assert!(catalog.items.values().all(|item| item.path != source));
        assert_eq!(watcher.telemetry.full_reconciles.load(Ordering::Acquire), 0);
        assert!(watcher.telemetry.targeted_batches.load(Ordering::Acquire) > 0);
        watcher.finish();
    }

    #[test]
    fn real_inotify_waits_for_close_when_a_new_file_is_hard_linked_while_open() {
        let tmp = TempDir::new("watch-open-hard-link-create");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("original.mkv");
        let alias = root.join("alias.mkv");
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 1);

        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&original)
            .unwrap();
        file.write_all(&[0x1a, 0x45, 0xdf, 0xa3]).unwrap();
        file.sync_all().unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        assert!(matches!(
            watcher.updates.recv_timeout(Duration::from_millis(750)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        file.write_all(&[0u8; 124]).unwrap();
        drop(file);
        let (_catalog, delta) = watcher.wait_for_catalog(|catalog| {
            [original.as_path(), alias.as_path()]
                .into_iter()
                .all(|path| {
                    catalog
                        .items
                        .values()
                        .any(|item| item.path == path && item.size == 128)
                })
        });
        assert!(delta.added >= 2, "{delta:?}");
        assert_eq!(watcher.telemetry.full_reconciles.load(Ordering::Acquire), 0);
        watcher.finish();
    }

    #[test]
    fn real_inotify_tracks_a_hard_link_writer_renamed_before_close() {
        let tmp = TempDir::new("watch-open-hard-link-rename");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("original.mkv");
        let alias = root.join("alias.mkv");
        let renamed = root.join("renamed.mkv");
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 1);

        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&original)
            .unwrap();
        file.write_all(&[0x1a, 0x45, 0xdf, 0xa3]).unwrap();
        file.sync_all().unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        std::fs::rename(&original, &renamed).unwrap();
        assert!(matches!(
            watcher.updates.recv_timeout(Duration::from_millis(750)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        file.write_all(&[0u8; 124]).unwrap();
        drop(file);
        let (catalog, delta) = watcher.wait_for_catalog(|catalog| {
            [alias.as_path(), renamed.as_path()]
                .into_iter()
                .all(|path| {
                    catalog
                        .items
                        .values()
                        .any(|item| item.path == path && item.size == 128)
                })
        });
        assert!(delta.added >= 2, "{delta:?}");
        assert!(catalog.items.values().all(|item| item.path != original));
        assert_eq!(watcher.telemetry.full_reconciles.load(Ordering::Acquire), 0);
        watcher.finish();
    }

    #[test]
    fn real_inotify_tracks_a_hard_link_writer_unlinked_before_close() {
        let tmp = TempDir::new("watch-open-hard-link-unlink");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("original.mkv");
        let alias = root.join("alias.mkv");
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 1);

        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&original)
            .unwrap();
        file.write_all(&[0x1a, 0x45, 0xdf, 0xa3]).unwrap();
        file.sync_all().unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        std::fs::remove_file(&original).unwrap();
        assert!(matches!(
            watcher.updates.recv_timeout(Duration::from_millis(750)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        file.write_all(&[0u8; 124]).unwrap();
        drop(file);
        let (catalog, delta) = watcher.wait_for_catalog(|catalog| {
            catalog
                .items
                .values()
                .any(|item| item.path == alias && item.size == 128)
        });
        assert!(delta.added > 0, "{delta:?}");
        assert!(catalog.items.values().all(|item| item.path != original));
        assert_eq!(watcher.telemetry.full_reconciles.load(Ordering::Acquire), 0);
        watcher.finish();
    }

    #[test]
    fn real_inotify_expands_a_new_child_through_directory_aliases() {
        let tmp = TempDir::new("watch-directory-alias-create");
        let root = tmp.join("media");
        let physical_dir = root.join("physical");
        let alias_dir = root.join("alias");
        std::fs::create_dir_all(&physical_dir).unwrap();
        std::os::unix::fs::symlink(&physical_dir, &alias_dir).unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 2);
        let physical = physical_dir.join("new.mkv");
        let alias = alias_dir.join("new.mkv");

        fake_mkv(&physical);
        let (catalog, delta) = watcher.wait_for_catalog(|catalog| {
            catalog.items.values().any(|item| item.path == physical)
                && catalog.items.values().any(|item| item.path == alias)
        });
        assert!(delta.added >= 2, "{delta:?}");
        assert!(catalog.items.values().any(|item| item.path == physical));
        assert!(catalog.items.values().any(|item| item.path == alias));
        assert_eq!(
            watcher.telemetry.full_reconciles.load(Ordering::Acquire),
            0,
            "a file event beneath a known alias must stay targeted"
        );
        watcher.finish();
    }

    #[test]
    fn real_inotify_updates_every_directory_alias() {
        let tmp = TempDir::new("watch-directory-alias-update");
        let root = tmp.join("media");
        let physical_dir = root.join("physical");
        let alias_dir = root.join("alias");
        std::fs::create_dir_all(&physical_dir).unwrap();
        std::os::unix::fs::symlink(&physical_dir, &alias_dir).unwrap();
        let physical = physical_dir.join("existing.mkv");
        let alias = alias_dir.join("existing.mkv");
        fake_mkv(&physical);
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 2);

        fake_mkv_with_len(&physical, 192);
        let (catalog, delta) = watcher.wait_for_catalog(|catalog| {
            [physical.as_path(), alias.as_path()]
                .into_iter()
                .all(|path| {
                    catalog
                        .items
                        .values()
                        .any(|item| item.path == path && item.size == 192)
                })
        });
        assert!(delta.changed >= 2, "{delta:?}");
        assert!([physical.as_path(), alias.as_path()]
            .into_iter()
            .all(|path| {
                catalog
                    .items
                    .values()
                    .any(|item| item.path == path && item.size == 192)
            }));
        assert_eq!(
            watcher.telemetry.full_reconciles.load(Ordering::Acquire),
            0,
            "same-inode updates must stay targeted"
        );
        watcher.finish();
    }

    #[test]
    fn real_inotify_removes_children_of_deleted_and_moved_directory_aliases() {
        let tmp = TempDir::new("watch-directory-alias-remove");
        let root = tmp.join("media");
        let physical_dir = root.join("physical");
        let deleted_alias = root.join("delete-alias");
        let moved_alias = root.join("move-alias");
        let moved_outside = tmp.join("moved-alias");
        std::fs::create_dir_all(&physical_dir).unwrap();
        std::os::unix::fs::symlink(&physical_dir, &deleted_alias).unwrap();
        std::os::unix::fs::symlink(&physical_dir, &moved_alias).unwrap();
        let physical = physical_dir.join("existing.mkv");
        let deleted_child = deleted_alias.join("existing.mkv");
        let moved_child = moved_alias.join("existing.mkv");
        fake_mkv(&physical);
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 2);

        std::fs::remove_file(&deleted_alias).unwrap();
        std::fs::rename(&moved_alias, &moved_outside).unwrap();
        let (catalog, delta) = watcher.wait_for_catalog(|catalog| {
            catalog.items.values().any(|item| item.path == physical)
                && catalog
                    .items
                    .values()
                    .all(|item| item.path != deleted_child && item.path != moved_child)
        });
        assert!(delta.removed >= 2, "{delta:?}");
        assert!(catalog.items.values().any(|item| item.path == physical));
        assert!(watcher.telemetry.full_reconciles.load(Ordering::Acquire) > 0);
        watcher.finish();
    }

    #[test]
    fn real_inotify_waits_for_close_before_publishing_an_ordinary_create() {
        let tmp = TempDir::new("watch-ordinary-create-close");
        let root = tmp.join("media");
        std::fs::create_dir_all(&root).unwrap();
        let created = root.join("created.mkv");
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            ..ScanConfig::default()
        };
        crate::scan(&cfg).unwrap();
        let watcher = RunningWatcher::start(cfg, 1);

        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&created)
            .unwrap();
        let mut first = vec![0u8; 64];
        first[..4].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3]);
        file.write_all(&first).unwrap();
        file.sync_all().unwrap();
        assert!(matches!(
            watcher.updates.recv_timeout(Duration::from_millis(750)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        file.write_all(&[0u8; 64]).unwrap();
        drop(file);
        let (catalog, delta) = watcher.wait_for_catalog(|catalog| {
            catalog
                .items
                .values()
                .any(|item| item.path == created && item.size == 128)
        });
        assert!(delta.added > 0, "{delta:?}");
        assert!(catalog
            .items
            .values()
            .any(|item| item.path == created && item.size == 128));
        assert_eq!(watcher.telemetry.full_reconciles.load(Ordering::Acquire), 0);
        watcher.finish();
    }

    #[cfg(unix)]
    #[test]
    fn inotify_reconcile_never_watches_or_indexes_an_outside_link() {
        let tmp = TempDir::new("watch-jail");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("media");
        let inside = root.join("inside");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        fake_mkv(&inside.join("inside.mkv"));
        fake_mkv(&outside.join("secret.mkv"));
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: crate::MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = crate::scan(&cfg);

        let escape = root.join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        let mut ino = Inotify::init().unwrap();
        let mut wds = HashMap::new();
        let mut directory_inodes = HashMap::new();
        let mut watched_directories = HashSet::new();
        let mut outside_published = false;
        let mut batch = PendingBatch::default();
        batch.add_dir(escape.clone());
        apply_batch(
            &cfg,
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut directory_inodes,
                watched_directories: &mut watched_directories,
            },
            batch,
            None,
            false,
            &AtomicBool::new(false),
            &mut |_, _| {
                if let (Some(catalog), _) = crate::monitor(&cfg)? {
                    outside_published = catalog.items.values().any(|item| item.title == "secret");
                }
                Ok(())
            },
        );
        assert!(!outside_published);
        assert!(!wds.values().flatten().any(|path| path.starts_with(&escape)));

        let safe_alias = root.join("safe-alias");
        std::os::unix::fs::symlink(&inside, &safe_alias).unwrap();
        let mut safe_published = false;
        let mut batch = PendingBatch::default();
        batch.add_dir(safe_alias.clone());
        apply_batch(
            &cfg,
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut directory_inodes,
                watched_directories: &mut watched_directories,
            },
            batch,
            None,
            false,
            &AtomicBool::new(false),
            &mut |_, _| {
                if let (Some(catalog), _) = crate::monitor(&cfg)? {
                    safe_published = catalog
                        .items
                        .values()
                        .any(|item| item.path.ends_with("safe-alias/inside.mkv"));
                }
                Ok(())
            },
        );
        assert!(
            safe_published,
            "safe link must reconcile through inotify path"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn inotify_tree_deduplicates_directory_aliases_and_cycles() {
        let tmp = TempDir::new("watch-inode-dedupe");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("media");
        let physical = root.join("physical");
        std::fs::create_dir_all(&physical).unwrap();
        std::os::unix::fs::symlink(&physical, root.join("alias")).unwrap();
        std::os::unix::fs::symlink(&root, physical.join("cycle")).unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            types: crate::MediaTypes::video_only(),
            ..Default::default()
        };
        let mut ino = Inotify::init().unwrap();
        let mut wds = HashMap::new();
        let mut directory_inodes = HashMap::new();
        let mut watched_directories = HashSet::new();
        add_tree_watches(
            &mut ino,
            &mut wds,
            &mut directory_inodes,
            &mut watched_directories,
            &cfg,
            &root,
        )
        .unwrap();
        assert_eq!(directory_inodes.len(), 2);
        assert_eq!(wds.len(), 2);
        let lexical_paths = wds.values().flatten().collect::<HashSet<_>>();
        assert!(lexical_paths.contains(&root));
        assert!(lexical_paths.contains(&physical));
        assert!(lexical_paths.contains(&root.join("alias")));
        assert!(!lexical_paths.contains(&physical.join("cycle")));
        assert_eq!(watched_directories.len(), 3);
    }
}
