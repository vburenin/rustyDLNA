//! Watch media dirs and apply add/remove after a short settle window.
//! A flood or `Q_OVERFLOW` becomes one `monitor()` walk instead of
//! per-file work.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::db::LibraryDb;
use crate::{
    ensure_folder_chain, forget_path, forget_tree, index_one_file, is_album_art_name,
    is_caption_name, is_junk_dir, is_sample_or_trailer_dir, is_unfinished_name,
    looks_like_sample_file, path_excluded, Catalog, ScanConfig, ScanDelta,
};

const MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO);

/// Wait this long after the first event in a burst before applying.
const SETTLE: Duration = Duration::from_secs(5);
/// More unique paths than this (or a queue overflow) → one tree reconcile.
const RECONCILE_AFTER: usize = 64;

pub fn repair_objects_if_needed(cfg: &ScanConfig) -> (Option<Catalog>, ScanDelta) {
    let db = match &cfg.db_path {
        Some(p) => LibraryDb::open(p).expect("open files.db"),
        None => return (None, ScanDelta::default()),
    };
    let missing = db.details_missing_objects().unwrap_or(0);
    if missing > 0 {
        tracing::info!(target: "rusty_dlna", missing, "DETAILS have no OBJECTS; repairing aliases");
        return crate::monitor(cfg);
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
        if ev.mask.contains(EventMask::ISDIR)
            && ev.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
        {
            if excluded || is_junk_dir(&name) || is_sample_or_trailer_dir(&name) {
                continue;
            }
            batch.add_dir(path);
            continue;
        }
        if ev.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
            if ev.mask.contains(EventMask::ISDIR) {
                batch.remove_tree(path);
            } else {
                batch.remove_file(path);
            }
            continue;
        }
        if excluded
            || is_unfinished_name(&name)
            || looks_like_sample_file(&name)
            || is_caption_name(&name)
            || name.ends_with(".nfo")
            || name.ends_with(".probe.toml")
            || is_album_art_name(&name)
        {
            continue;
        }
        if ev.mask.intersects(EventMask::CLOSE_WRITE | EventMask::MOVED_TO)
            || (ev.mask.contains(EventMask::CREATE) && path_is_link_or_dir(&path))
        {
            if path.is_dir() {
                if !is_junk_dir(&name) && !is_sample_or_trailer_dir(&name) {
                    batch.add_dir(path);
                }
            } else {
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
    let reconcile = batch.should_reconcile();
    tracing::info!(
        target: "rusty_dlna",
        added = batch.add_files.len(),
        removed = batch.remove_files.len() + batch.remove_trees.len(),
        dirs = batch.add_dirs.len(),
        overflow = batch.overflow,
        mode = if reconcile { "reconcile" } else { "incremental" },
        "inotify settled"
    );

    if reconcile {
        for dir in &batch.add_dirs {
            add_tree_watches(ino, wds, cfg, dir);
        }
        for dir in &batch.remove_trees {
            wds.retain(|_, p| !p.starts_with(dir));
        }
        match crate::monitor(cfg) {
            (Some(cat), delta) => on_change(cat, delta),
            _ => {}
        }
        return;
    }

    let mut added = 0usize;
    let mut removed = 0usize;
    for dir in &batch.remove_trees {
        removed += forget_tree(cfg, dir);
        wds.retain(|_, p| !p.starts_with(dir));
    }
    for path in &batch.remove_files {
        if batch
            .remove_trees
            .iter()
            .any(|dir| path.starts_with(dir))
        {
            continue;
        }
        removed += forget_path(cfg, path);
    }
    for dir in &batch.add_dirs {
        added += insert_directory(ino, wds, cfg, dir);
    }
    for path in &batch.add_files {
        if add_one(cfg, path) {
            added += 1;
        }
    }
    if added + removed == 0 {
        return;
    }
    if let Some(dbp) = &cfg.db_path {
        if let Ok(db) = LibraryDb::open(dbp) {
            removed += db.prune_empty_folders().unwrap_or(0);
            if let Ok(cat) = db.load_catalog() {
                on_change(
                    cat,
                    ScanDelta {
                        added,
                        removed,
                        changed: 0,
                    },
                );
            }
        }
    }
}

#[derive(Default, Debug)]
struct PendingBatch {
    overflow: bool,
    add_files: HashSet<PathBuf>,
    add_dirs: Vec<PathBuf>,
    remove_files: HashSet<PathBuf>,
    remove_trees: Vec<PathBuf>,
}

impl PendingBatch {
    fn is_empty(&self) -> bool {
        !self.overflow
            && self.add_files.is_empty()
            && self.add_dirs.is_empty()
            && self.remove_files.is_empty()
            && self.remove_trees.is_empty()
    }

    fn unique_paths(&self) -> usize {
        self.add_files.len() + self.remove_files.len() + self.remove_trees.len() + self.add_dirs.len()
    }

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
    if path_excluded(dir, name, cfg) || is_junk_dir(name) || is_sample_or_trailer_dir(name) {
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
        if path_excluded(&path, &n, cfg) || is_junk_dir(&n) || is_sample_or_trailer_dir(&n) {
            continue;
        }
        if ent.file_type().map(|t| t.is_dir() || t.is_symlink()).unwrap_or(false) && path.is_dir()
        {
            add_tree_watches(ino, wds, cfg, &path);
        }
    }
}

fn insert_directory(
    ino: &mut Inotify,
    wds: &mut HashMap<WatchDescriptor, PathBuf>,
    cfg: &ScanConfig,
    dir: &Path,
) -> usize {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if path_excluded(dir, name, cfg) {
        return 0;
    }
    add_tree_watches(ino, wds, cfg, dir);
    let mut n = 0;
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for ent in rd.filter_map(|e| e.ok()) {
        let path = ent.path();
        let nm = ent.file_name().to_string_lossy().into_owned();
        if nm.starts_with('.') || path_excluded(&path, &nm, cfg) {
            continue;
        }
        if path.is_dir() {
            n += insert_directory(ino, wds, cfg, &path);
        } else if add_one(cfg, &path) {
            n += 1;
        }
    }
    n
}

fn add_one(cfg: &ScanConfig, path: &Path) -> bool {
    let _write = crate::library_write_guard();
    let db = match &cfg.db_path {
        Some(p) => match LibraryDb::open(p) {
            Ok(d) => d,
            Err(_) => return false,
        },
        None => return false,
    };
    let Some(folder) = ensure_folder_chain(&db, cfg, path) else {
        return false;
    };
    index_one_file(&db, cfg, path, &folder)
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
