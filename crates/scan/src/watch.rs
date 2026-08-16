//! MiniDLNA `monitor_inotify.c`: watch media dirs, apply single-file
//! add/remove. Never rewrite the library on a timer.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::db::LibraryDb;
use crate::{
    ensure_folder_chain, index_one_file, is_album_art_name, is_caption_name, is_junk_dir,
    is_sample_or_trailer_dir, is_unfinished_name, looks_like_sample_file, path_excluded, Catalog,
    ScanConfig, ScanDelta,
};

const MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO);

pub fn repair_objects_if_needed(cfg: &ScanConfig) -> (Option<Catalog>, ScanDelta) {
    let db = match &cfg.db_path {
        Some(p) => LibraryDb::open(p).expect("open files.db"),
        None => return (None, ScanDelta::default()),
    };
    let missing = db.details_missing_objects().unwrap_or(0);
    if missing > 0 {
        eprintln!("rusty-dlna: {missing} DETAILS have no OBJECTS; repairing aliases");
        return crate::monitor(cfg);
    }
    (None, ScanDelta::default())
}

/// Block on inotify. `on_change` is called only after a real add/remove.
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
    eprintln!(
        "rusty-dlna inotify: {} directory watches (MiniDLNA-style)",
        wds.len()
    );

    let mut buf = [0u8; 65_536];
    loop {
        let events = match ino.read_events_blocking(&mut buf) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let mut added = 0usize;
        let mut removed = 0usize;
        let mut new_dirs: Vec<PathBuf> = Vec::new();
        for ev in events {
            let Some(name) = ev.name else { continue };
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let Some(dir) = wds.get(&ev.wd) else { continue };
            let path = dir.join(name.as_ref());
            let excluded = path_excluded(&path, &name, &cfg);
            if ev.mask.contains(EventMask::ISDIR)
                && ev.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
            {
                if excluded || is_junk_dir(&name) || is_sample_or_trailer_dir(&name) {
                    continue;
                }
                new_dirs.push(path);
                continue;
            }
            if ev.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
                if ev.mask.contains(EventMask::ISDIR) {
                    removed += remove_tree(&cfg, &path);
                    wds.retain(|_, p| !p.starts_with(&path));
                } else {
                    removed += remove_one(&cfg, &path);
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
            // Regular files: wait for CLOSE_WRITE / MOVED_TO (not CREATE of a 0-byte open).
            if ev.mask.intersects(EventMask::CLOSE_WRITE | EventMask::MOVED_TO)
                || (ev.mask.contains(EventMask::CREATE) && path_is_link_or_dir(&path))
            {
                if path.is_dir() {
                    if !is_junk_dir(&name) && !is_sample_or_trailer_dir(&name) {
                        new_dirs.push(path);
                    }
                } else if add_one(&cfg, &path) {
                    added += 1;
                }
            }
        }
        for dir in new_dirs {
            added += insert_directory(&mut ino, &mut wds, &cfg, &dir);
        }
        if added + removed == 0 {
            continue;
        }
        if let Some(dbp) = &cfg.db_path {
            if let Ok(db) = LibraryDb::open(dbp) {
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

fn remove_one(cfg: &ScanConfig, path: &Path) -> usize {
    let db = match &cfg.db_path {
        Some(p) => match LibraryDb::open(p) {
            Ok(d) => d,
            Err(_) => return 0,
        },
        None => return 0,
    };
    db.remove_path_and_symlink_aliases(&path.to_string_lossy())
        .unwrap_or(0)
}

fn remove_tree(cfg: &ScanConfig, dir: &Path) -> usize {
    let db = match &cfg.db_path {
        Some(p) => match LibraryDb::open(p) {
            Ok(d) => d,
            Err(_) => return 0,
        },
        None => return 0,
    };
    let prefix = format!("{}/", dir.to_string_lossy());
    let rows = db.all_detail_stats().unwrap_or_default();
    let mut n = 0;
    for (p, _, _, _) in rows {
        if p.starts_with(&prefix) || Path::new(&p) == dir {
            n += db.remove_path_and_symlink_aliases(&p).unwrap_or(0);
        }
    }
    n
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
