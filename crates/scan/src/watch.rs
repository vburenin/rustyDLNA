//! Watch media dirs. After a short settle window every burst becomes
//! one `monitor()` walk so directory-symlink aliases stay in sync.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::db::LibraryDb;
use crate::{
    is_album_art_name, is_caption_name, is_skipped_dir, is_unfinished_name,
    looks_like_sample_file, monitor_dirty, path_excluded, rebuild_objects, Catalog, ScanConfig,
    ScanDelta,
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
            action: if dir {
                "dir_relocated"
            } else {
                "relocated"
            },
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
        let file = ev
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
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
const SETTLE: Duration = Duration::from_secs(5);
/// Burst size that used to force a tree walk. Every burst reconciles now;
/// kept so tests still describe the old overflow threshold.
#[allow(dead_code)]
const RECONCILE_AFTER: usize = 64;

pub fn repair_objects_if_needed(cfg: &ScanConfig) -> (Option<Catalog>, ScanDelta) {
    let db = match &cfg.db_path {
        Some(p) => LibraryDb::open(p).expect("open files.db"),
        None => return (None, ScanDelta::default()),
    };
    let missing = db.details_missing_objects().unwrap_or(0);
    let dupes = db.folders_have_duplicate_inodes();
    if missing > 0 || dupes {
        tracing::info!(
            target: "rusty_dlna",
            missing,
            dupes,
            "repairing object tree"
        );
        let next = rebuild_objects(cfg);
        return (
            Some(next),
            ScanDelta {
                added: 0,
                removed: 0,
                changed: 1,
            },
        );
    }
    (None, ScanDelta::default())
}

/// Block on inotify. `on_change` is called after a settled burst.
pub fn run_inotify(
    cfg: ScanConfig,
    mut on_change: impl FnMut(Catalog, ScanDelta),
) -> std::io::Result<()> {
    let mut ino = Inotify::init()?;
    let mut wds: HashMap<WatchDescriptor, PathBuf> = HashMap::new();
    raise_watch_limit(65_536);
    for root in &cfg.media_dirs {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        add_tree_watches(&mut ino, &mut wds, &cfg, &root);
    }
    tracing::info!(
        target: "rusty_dlna",
        watches = wds.len(),
        settle_secs = SETTLE.as_secs(),
        "inotify ready"
    );

    let mut buf = [0u8; 65_536];
    loop {
        let mut batch = PendingBatch::default();
        match ino.read_events_blocking(&mut buf) {
            Ok(events) => collect_events(events, &wds, &cfg, &mut batch),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        if batch.is_empty() {
            continue;
        }

        let deadline = Instant::now() + SETTLE;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let ms = left.as_millis().min(i32::MAX as u128) as i32;
            match poll_fd(ino.as_raw_fd(), ms) {
                Ok(false) => break,
                Ok(true) => match ino.read_events(&mut buf) {
                    Ok(events) => collect_events(events, &wds, &cfg, &mut batch),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                },
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }

        apply_batch(&cfg, &mut ino, &mut wds, batch, &mut on_change);
    }
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
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Some(dir) = wds.get(&ev.wd) else { continue };
        let path = dir.join(name.as_ref());
        let excluded = path_excluded(&path, &name, cfg);
        let kind = FileEventKind::from_mask(ev.mask);
        if ev.mask.contains(EventMask::ISDIR)
            && ev.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
        {
            if excluded || is_skipped_dir(&name) {
                continue;
            }
            batch.note(path.clone(), ev.cookie, kind);
            batch.add_dir(path);
            continue;
        }
        if ev.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
            if ev.mask.contains(EventMask::ISDIR) {
                batch.note(path.clone(), ev.cookie, kind);
                batch.remove_tree(path);
            } else {
                if !is_sidecar_name(&name) {
                    batch.note(path.clone(), ev.cookie, kind);
                }
                batch.remove_file(path);
            }
            continue;
        }
        if excluded
            || is_unfinished_name(&name)
            || looks_like_sample_file(&name)
            || is_sidecar_name(&name)
        {
            continue;
        }
        let apply = ev.mask.intersects(EventMask::CLOSE_WRITE | EventMask::MOVED_TO)
            || (ev.mask.contains(EventMask::CREATE) && path_is_link_or_dir(&path));
        if ev.mask.contains(EventMask::CREATE) && !apply {
            // Regular-file CREATE is indexed on CLOSE_WRITE; still log it.
            batch.note(path.clone(), ev.cookie, kind);
        }
        if apply {
            if path.is_dir() {
                if !is_skipped_dir(&name) {
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

fn apply_batch(
    cfg: &ScanConfig,
    ino: &mut Inotify,
    wds: &mut HashMap<WatchDescriptor, PathBuf>,
    batch: PendingBatch,
    on_change: &mut impl FnMut(Catalog, ScanDelta),
) {
    // Incremental add/remove only sees the event path. A rename under a
    // real folder leaves directory-symlink aliases (genres/BY_YEAR/…)
    // stale. One tree reconcile is the source of truth.
    log_file_events(&batch.events);
    tracing::info!(
        target: "rusty_dlna",
        added = batch.add_files.len(),
        removed = batch.remove_files.len() + batch.remove_trees.len(),
        dirs = batch.add_dirs.len(),
        events = batch.events.len(),
        overflow = batch.overflow,
        mode = "reconcile",
        "inotify settled"
    );

    for dir in &batch.add_dirs {
        add_tree_watches(ino, wds, cfg, dir);
    }
    for dir in &batch.remove_trees {
        wds.retain(|_, p| !p.starts_with(dir));
    }
    let dirty: Vec<PathBuf> = batch.add_files.iter().cloned().collect();
    match monitor_dirty(cfg, &dirty) {
        (Some(cat), delta) => on_change(cat, delta),
        _ => {}
    }
}

fn is_sidecar_name(name: &str) -> bool {
    is_caption_name(name)
        || name.ends_with(".nfo")
        || name.ends_with(".probe.toml")
        || is_album_art_name(name)
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
        self.add_files.len() + self.remove_files.len() + self.remove_trees.len() + self.add_dirs.len()
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
    cfg: &ScanConfig,
    dir: &Path,
) {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if path_excluded(dir, name, cfg) || is_skipped_dir(name) {
        return;
    }
    if let Ok(wd) = ino.watches().add(dir, MASK) {
        wds.insert(wd, dir.to_path_buf());
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for ent in rd.filter_map(|e| e.ok()) {
        let path = ent.path();
        let n = ent.file_name().to_string_lossy().into_owned();
        if n.starts_with('.') {
            continue;
        }
        if path_excluded(&path, &n, cfg) || is_skipped_dir(&n) {
            continue;
        }
        if ent.file_type().map(|t| t.is_dir() || t.is_symlink()).unwrap_or(false) && path.is_dir()
        {
            add_tree_watches(ino, wds, cfg, &path);
        }
    }
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
        let _ = writeln!(f, "{want}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut b = PendingBatch::default();
        b.overflow = true;
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
}
