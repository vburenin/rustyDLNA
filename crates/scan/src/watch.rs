//! Watch media dirs. After a short settle window every burst becomes
//! one `monitor()` walk so directory-symlink aliases stay in sync.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::{
    is_album_art_name_for_config, is_caption_name, is_skipped_dir, is_unfinished_name,
    looks_like_sample_file, monitor, monitor_dirty, monitor_dirty_incremental, monitor_incremental,
    open_library_db, path_excluded, path_is_allowed_dir, rebuild_objects, Catalog, CatalogUpdate,
    ScanConfig, ScanDelta, ScanResult,
};

const MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO);

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

#[derive(Debug, Default)]
pub struct WatchTelemetry {
    pub watch_count: AtomicUsize,
    pub overflow_count: AtomicU64,
    /// Kernel overflow markers. Each marker means one or more events were
    /// dropped; inotify does not report the exact number lost.
    pub dropped_events_total: AtomicU64,
    pub batches: AtomicU64,
    pub events_total: AtomicU64,
    pub pending_paths: AtomicUsize,
    pub pending_paths_peak: AtomicUsize,
    pub last_batch_paths: AtomicUsize,
    pub full_reconciles: AtomicU64,
    pub targeted_batches: AtomicU64,
}

pub fn repair_objects_if_needed(cfg: &ScanConfig) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    let db = match &cfg.db_path {
        Some(p) => open_library_db(p)?,
        None => return Ok((None, ScanDelta::default())),
    };
    let missing = db.details_missing_objects()?;
    let dupes = db.folders_have_duplicate_inodes()?;
    if missing > 0 {
        tracing::info!(
            target: "rusty_dlna",
            missing,
            dupes,
            "repairing object tree"
        );
        let next = rebuild_objects(cfg)?;
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
            Some(crate::load_catalog_with_policy(&db, cfg)?),
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
    run_inotify_core(
        cfg,
        stopping,
        scan_gate,
        telemetry,
        false,
        &mut |update, delta| match update {
            CatalogUpdate::Replacement(catalog) => on_change(catalog, delta),
            CatalogUpdate::Patch(_) => unreachable!("full inotify monitor returned a patch"),
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
    run_inotify_core(cfg, stopping, scan_gate, telemetry, true, &mut on_change)
}

fn run_inotify_core(
    cfg: ScanConfig,
    stopping: &AtomicBool,
    scan_gate: Option<&Mutex<()>>,
    telemetry: Option<&WatchTelemetry>,
    incremental: bool,
    on_change: &mut impl FnMut(CatalogUpdate, ScanDelta),
) -> std::io::Result<()> {
    let mut ino = Inotify::init()?;
    let mut wds: HashMap<WatchDescriptor, PathBuf> = HashMap::new();
    let mut directory_inodes = HashSet::new();
    raise_watch_limit(65_536);
    for root in &cfg.media_dirs {
        let root = std::fs::canonicalize(root)?;
        add_tree_watches(&mut ino, &mut wds, &mut directory_inodes, &cfg, &root)?;
    }
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
    while !stopping.load(Ordering::Acquire) {
        let mut batch = PendingBatch::default();
        match poll_fd(ino.as_raw_fd(), 500) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        match ino.read_events(&mut buf) {
            Ok(events) => collect_events(events, &wds, &cfg, &mut batch),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        observe_pending(telemetry, &batch);
        if batch.is_empty() {
            continue;
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
                        collect_events(events, &wds, &cfg, &mut batch);
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
        let _guard = scan_gate.map(|gate| gate.lock().unwrap_or_else(|error| error.into_inner()));
        apply_batch(
            &cfg,
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut directory_inodes,
            },
            batch,
            telemetry,
            incremental,
            on_change,
        );
        if let Some(telemetry) = telemetry {
            telemetry.pending_paths.store(0, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn observe_pending(telemetry: Option<&WatchTelemetry>, batch: &PendingBatch) {
    let Some(telemetry) = telemetry else {
        return;
    };
    let pending = batch.unique_paths();
    telemetry.pending_paths.store(pending, Ordering::Relaxed);
    telemetry
        .pending_paths_peak
        .fetch_max(pending, Ordering::Relaxed);
}

fn collect_events<'a>(
    events: impl IntoIterator<Item = inotify::Event<&'a std::ffi::OsStr>>,
    wds: &HashMap<WatchDescriptor, PathBuf>,
    cfg: &ScanConfig,
    batch: &mut PendingBatch,
) {
    for ev in events {
        if ev.mask.contains(EventMask::Q_OVERFLOW) {
            batch.overflow = true;
            continue;
        }
        let Some(name) = ev.name else { continue };
        let display_name = name.to_string_lossy();
        if !cfg.include_hidden && display_name.starts_with('.') {
            continue;
        }
        let Some(dir) = wds.get(&ev.wd) else { continue };
        let path = dir.join(name);
        let excluded = path_excluded(&path, &display_name, cfg);
        let kind = FileEventKind::from_mask(ev.mask);
        if ev.mask.contains(EventMask::ISDIR)
            && ev.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
        {
            if excluded || is_skipped_dir(&display_name) || !path_is_allowed_dir(&path, cfg) {
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
            if ev.mask.contains(EventMask::ISDIR) {
                batch.note(path.clone(), ev.cookie, kind);
                batch.remove_tree(path);
            } else {
                if !is_sidecar_name(&display_name, cfg) {
                    batch.note(path.clone(), ev.cookie, kind);
                }
                batch.remove_file(path);
            }
            continue;
        }
        if drop_create_event(&display_name, excluded) {
            continue;
        }
        let apply = ev
            .mask
            .intersects(EventMask::CLOSE_WRITE | EventMask::MOVED_TO)
            || (ev.mask.contains(EventMask::CREATE) && path_is_link_or_dir(&path));
        if ev.mask.contains(EventMask::CREATE) && !apply {
            // Regular-file CREATE is indexed on CLOSE_WRITE; still log it.
            batch.note(path.clone(), ev.cookie, kind);
        }
        if apply {
            if path.is_dir() {
                if !is_skipped_dir(&display_name) && path_is_allowed_dir(&path, cfg) {
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

struct WatchState<'a> {
    ino: &'a mut Inotify,
    wds: &'a mut HashMap<WatchDescriptor, PathBuf>,
    directory_inodes: &'a mut HashSet<(u64, u64)>,
}

fn apply_batch(
    cfg: &ScanConfig,
    watch: WatchState<'_>,
    batch: PendingBatch,
    telemetry: Option<&WatchTelemetry>,
    incremental: bool,
    on_change: &mut impl FnMut(CatalogUpdate, ScanDelta),
) {
    let WatchState {
        ino,
        wds,
        directory_inodes,
    } = watch;
    let fallback =
        batch.should_reconcile() || !batch.add_dirs.is_empty() || !batch.remove_trees.is_empty();
    if let Some(telemetry) = telemetry {
        record_batch_telemetry(telemetry, &batch, fallback);
    }
    log_file_events(&batch.events);
    tracing::info!(
        target: "rusty_dlna",
        added = batch.add_files.len(),
        removed = batch.remove_files.len() + batch.remove_trees.len(),
        dirs = batch.add_dirs.len(),
        events = batch.events.len(),
        overflow = batch.overflow,
        mode = if fallback { "bounded-full" } else { "targeted" },
        "inotify settled"
    );

    for dir in &batch.remove_trees {
        wds.retain(|_, p| !p.starts_with(dir));
    }
    if !batch.remove_trees.is_empty() {
        directory_inodes.clear();
        directory_inodes.extend(wds.values().filter_map(|path| {
            std::fs::metadata(path)
                .ok()
                .filter(|meta| meta.is_dir())
                .map(|meta| (meta.dev(), meta.ino()))
        }));
    }
    for dir in &batch.add_dirs {
        if let Err(error) = add_tree_watches(ino, wds, directory_inodes, cfg, dir) {
            tracing::warn!(path = %dir.display(), %error, "could not add inotify subtree");
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
    let result = if incremental && fallback {
        monitor_incremental(cfg)
    } else if incremental {
        monitor_dirty_incremental(cfg, &dirty)
    } else if fallback {
        monitor(cfg).map(|(catalog, delta)| (catalog.map(CatalogUpdate::Replacement), delta))
    } else {
        monitor_dirty(cfg, &dirty)
            .map(|(catalog, delta)| (catalog.map(CatalogUpdate::Replacement), delta))
    };
    match result {
        Ok((Some(update), delta)) => on_change(update, delta),
        Ok((None, _)) => {}
        Err(error) => {
            tracing::error!(%error, "inotify reconciliation failed; retaining published catalog")
        }
    }
}

fn record_batch_telemetry(telemetry: &WatchTelemetry, batch: &PendingBatch, fallback: bool) {
    telemetry.batches.fetch_add(1, Ordering::Relaxed);
    telemetry
        .events_total
        .fetch_add(batch.events.len() as u64, Ordering::Relaxed);
    telemetry
        .last_batch_paths
        .store(batch.unique_paths(), Ordering::Relaxed);
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

fn is_sidecar_name(name: &str, cfg: &ScanConfig) -> bool {
    is_caption_name(name)
        || name.to_ascii_lowercase().ends_with(".nfo")
        || name.ends_with(".probe.toml")
        || is_album_art_name_for_config(name, cfg)
}

/// Junk / unfinished / samples are ignored. Poster and NFO writes must
/// reach `add_files` so `monitor_dirty` can attach them without a video rewrite.
fn drop_create_event(name: &str, excluded: bool) -> bool {
    excluded || is_unfinished_name(name) || looks_like_sample_file(name)
}

#[derive(Default, Debug)]
struct PendingBatch {
    overflow: bool,
    add_files: HashSet<PathBuf>,
    add_dirs: Vec<PathBuf>,
    remove_files: HashSet<PathBuf>,
    remove_trees: Vec<PathBuf>,
    events: Vec<FileEvent>,
}

impl PendingBatch {
    fn is_empty(&self) -> bool {
        !self.overflow
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
        self.overflow || self.unique_paths() >= RECONCILE_AFTER
    }

    fn add_file(&mut self, path: PathBuf) {
        self.remove_files.remove(&path);
        self.add_files.insert(path);
    }

    fn add_dir(&mut self, path: PathBuf) {
        if !self.add_dirs.iter().any(|p| p == &path) {
            self.add_dirs.push(path);
        }
    }

    fn remove_file(&mut self, path: PathBuf) {
        self.add_files.remove(&path);
        self.remove_files.insert(path);
    }

    fn remove_tree(&mut self, path: PathBuf) {
        self.add_dirs.retain(|p| !p.starts_with(&path));
        self.add_files.retain(|p| !p.starts_with(&path));
        self.remove_files.retain(|p| !p.starts_with(&path));
        if !self.remove_trees.iter().any(|p| p == &path) {
            self.remove_trees.push(path);
        }
    }

    fn note(&mut self, path: PathBuf, cookie: u32, kind: Option<FileEventKind>) {
        let Some(kind) = kind else {
            return;
        };
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

fn path_is_link_or_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink() || m.is_dir())
        .unwrap_or(false)
}

fn add_tree_watches(
    ino: &mut Inotify,
    wds: &mut HashMap<WatchDescriptor, PathBuf>,
    directory_inodes: &mut HashSet<(u64, u64)>,
    cfg: &ScanConfig,
    dir: &Path,
) -> io::Result<()> {
    let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let metadata = std::fs::metadata(dir)?;
    let inode = (metadata.dev(), metadata.ino());
    if !metadata.is_dir() || directory_inodes.contains(&inode) {
        return Ok(());
    }
    if !path_is_allowed_dir(dir, cfg) || path_excluded(dir, name, cfg) || is_skipped_dir(name) {
        return Ok(());
    }
    let wd = ino.watches().add(dir, MASK)?;
    directory_inodes.insert(inode);
    wds.insert(wd, dir.to_path_buf());
    let rd = std::fs::read_dir(dir)?;
    for ent in rd {
        let ent = ent?;
        let path = ent.path();
        let n = ent.file_name().to_string_lossy().into_owned();
        if !cfg.include_hidden && n.starts_with('.') {
            continue;
        }
        if path_excluded(&path, &n, cfg) || is_skipped_dir(&n) {
            continue;
        }
        if ent
            .file_type()
            .map(|t| t.is_dir() || t.is_symlink())
            .unwrap_or(false)
        {
            add_tree_watches(ino, wds, directory_inodes, cfg, &path)?;
        }
    }
    Ok(())
}

fn raise_watch_limit(want: u32) {
    let path = "/proc/sys/fs/inotify/max_user_watches";
    let cur = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(8192);
    if cur >= want {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path) {
        if let Err(error) = writeln!(f, "{want}") {
            tracing::warn!(%error, "could not raise inotify watch limit");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(&[0x1a, 0x45, 0xdf, 0xa3]);
        std::fs::write(path, bytes).unwrap();
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
    fn poster_and_nfo_writes_are_not_dropped() {
        let cfg = ScanConfig::default();
        assert!(is_sidecar_name("clip-poster.jpg", &cfg));
        assert!(is_sidecar_name("movie.nfo", &cfg));
        assert!(is_sidecar_name("tvshow.nfo", &cfg));
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
        b.remove_files.clear();
        b.remove_file(PathBuf::from("/v/one.mkv"));
        assert!(!b.should_reconcile());
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
        let mut directory_inodes = HashSet::new();
        let mut outside_published = false;
        let mut batch = PendingBatch::default();
        batch.add_dir(escape.clone());
        apply_batch(
            &cfg,
            WatchState {
                ino: &mut ino,
                wds: &mut wds,
                directory_inodes: &mut directory_inodes,
            },
            batch,
            None,
            false,
            &mut |update, _| {
                let CatalogUpdate::Replacement(catalog) = update else {
                    panic!("full watcher test received a patch");
                };
                outside_published = catalog.items.values().any(|item| item.title == "secret");
            },
        );
        assert!(!outside_published);
        assert!(!wds.values().any(|path| path.starts_with(&escape)));

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
            },
            batch,
            None,
            false,
            &mut |update, _| {
                let CatalogUpdate::Replacement(catalog) = update else {
                    panic!("full watcher test received a patch");
                };
                safe_published = catalog
                    .items
                    .values()
                    .any(|item| item.path.ends_with("safe-alias/inside.mkv"));
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
        let mut directory_inodes = HashSet::new();
        add_tree_watches(&mut ino, &mut wds, &mut directory_inodes, &cfg, &root).unwrap();
        assert_eq!(directory_inodes.len(), 2);
        assert_eq!(wds.len(), 2);
    }
}
