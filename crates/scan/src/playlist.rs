//! Bounded M3U/M3U8/PLS ingestion and stable playlist object views.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusty_dlna_helper::{read_to_end_bounded, BoundedReadError};
use rusty_dlna_protocol::object_id::{IMAGE_PLIST_ID, MUSIC_PLIST_ID, VIDEO_PLIST_ID};

use crate::db::LibraryDb;
use crate::{
    display_os_name, file_mtime_unix, inode_key, is_skipped_dir_os_name, open_allowed_file,
    path_excluded, path_from_db, path_is_allowed_dir, path_is_allowed_file, path_to_db, scan_io,
    ScanConfig, ScanResult,
};

#[cfg(test)]
thread_local! {
    static CANCEL_AFTER_CHECKPOINTS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn cancel_after_checkpoints(checkpoints: usize) {
    CANCEL_AFTER_CHECKPOINTS.set(Some(checkpoints));
}

fn checkpoint(cfg: &ScanConfig) -> ScanResult<()> {
    #[cfg(test)]
    CANCEL_AFTER_CHECKPOINTS.with(|remaining| {
        if let Some(count) = remaining.get() {
            if count <= 1 {
                remaining.set(None);
                cfg.cancellation.cancel();
            } else {
                remaining.set(Some(count - 1));
            }
        }
    });
    cfg.check_cancelled()
}

const MAX_PLAYLIST_READ_BYTES: usize = 1024 * 1024;
const MAX_PLAYLIST_BYTES: u64 = MAX_PLAYLIST_READ_BYTES as u64;

#[derive(Debug)]
struct DesiredPlaylist {
    path: PathBuf,
    name: String,
    timestamp: i64,
    device: i64,
    inode: i64,
    detail_ids: Vec<i64>,
}

pub(crate) fn is_playlist(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("m3u")
                || extension.eq_ignore_ascii_case("m3u8")
                || extension.eq_ignore_ascii_case("pls")
        })
}

fn collect_playlist_paths(
    cfg: &ScanConfig,
    directory: &Path,
    seen: &mut HashSet<(u64, u64)>,
    output: &mut Vec<PathBuf>,
) -> ScanResult<()> {
    checkpoint(cfg)?;
    if !path_is_allowed_dir(directory, cfg) {
        return Ok(());
    }
    let metadata = std::fs::metadata(directory).map_err(|error| scan_io(directory, error))?;
    if !seen.insert(inode_key(&metadata)) {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory).map_err(|error| scan_io(directory, error))? {
        checkpoint(cfg)?;
        let entry = entry.map_err(|error| scan_io(directory, error))?;
        let path = entry.path();
        let raw_name = entry.file_name();
        let name = raw_name.to_string_lossy().into_owned();
        if (!cfg.include_hidden && name.starts_with('.')) || path_excluded(&path, &name, cfg) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| scan_io(&path, error))?;
        if file_type.is_dir() || (file_type.is_symlink() && path.is_dir()) {
            if !is_skipped_dir_os_name(&raw_name) {
                collect_playlist_paths(cfg, &path, seen, output)?;
            }
        } else if is_playlist(&path) && path_is_allowed_file(&path, cfg) {
            output.push(path);
        }
    }
    Ok(())
}

fn decode_playlist(path: &Path, bytes: &[u8]) -> Result<String, String> {
    if bytes.contains(&0) {
        return Err("contains NUL bytes".into());
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_string()),
        Err(_)
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8")) =>
        {
            Err("M3U8 is not valid UTF-8".into())
        }
        Err(_) => Ok(bytes.iter().map(|byte| char::from(*byte)).collect()),
    }
}

fn parse_entries(path: &Path, text: &str) -> Vec<String> {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pls"))
    {
        let mut entries = Vec::new();
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let Some(number) = key
                .to_ascii_lowercase()
                .strip_prefix("file")
                .and_then(|number| number.parse::<usize>().ok())
            else {
                continue;
            };
            entries.push((number, value.trim().to_string()));
        }
        entries.sort_by_key(|(number, _)| *number);
        return entries.into_iter().map(|(_, value)| value).collect();
    }
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

fn percent_decode_path(value: &str) -> Option<PathBuf> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut input = value.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hi = input.next()?;
        let lo = input.next()?;
        let hex = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        bytes.push(hex(hi)? * 16 + hex(lo)?);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

fn entry_path(playlist: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim().trim_matches('"');
    let path = raw
        .strip_prefix("file://")
        .and_then(percent_decode_path)
        .unwrap_or_else(|| PathBuf::from(raw));
    if path.is_absolute() {
        Some(path)
    } else {
        Some(playlist.parent()?.join(path))
    }
}

fn desired_playlists(
    db: &LibraryDb,
    cfg: &ScanConfig,
    paths: Vec<PathBuf>,
) -> ScanResult<Vec<DesiredPlaylist>> {
    // Only opened members and their relevant inode aliases are resolved. In
    // particular, an empty discovery performs no detail lookup/canonicalization.
    let mut details = HashMap::new();
    let mut output = Vec::new();
    for path in paths {
        checkpoint(cfg)?;
        let mut opened = match open_allowed_file(&path, cfg) {
            Ok(opened) => opened,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                ) =>
            {
                continue;
            }
            Err(error) => return Err(scan_io(&path, error)),
        };
        let metadata = opened
            .file
            .metadata()
            .map_err(|error| scan_io(&path, error))?;
        if metadata.len() > MAX_PLAYLIST_BYTES {
            tracing::warn!(path = %path.display(), bytes = metadata.len(), "oversized playlist rejected");
            continue;
        }
        let bytes = match read_to_end_bounded(&mut opened.file, MAX_PLAYLIST_READ_BYTES) {
            Ok(bytes) => bytes,
            Err(BoundedReadError::LimitExceeded { .. }) => {
                tracing::warn!(path = %path.display(), "growing playlist exceeded size limit");
                continue;
            }
            Err(BoundedReadError::Io(error)) => return Err(scan_io(&path, error)),
        };
        let text = match decode_playlist(&path, &bytes) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "playlist rejected");
                continue;
            }
        };
        let mut detail_ids = Vec::new();
        for raw in parse_entries(&path, &text) {
            checkpoint(cfg)?;
            let Some(candidate) = entry_path(&path, &raw) else {
                continue;
            };
            let Ok(candidate_file) = open_allowed_file(&candidate, cfg) else {
                continue;
            };
            let resolved = &candidate_file.resolved_path;
            let detail_id = if let Some(cached) = details.get(resolved) {
                *cached
            } else {
                let direct = db.detail_stats_for_paths(&[path_to_db(resolved)])?;
                let mut found = direct.first().map(|detail| detail.id);
                if found.is_none() {
                    let metadata = candidate_file
                        .file
                        .metadata()
                        .map_err(|error| scan_io(&candidate, error))?;
                    let (device, inode) = inode_key(&metadata);
                    let mut aliases = db.details_with_inode(
                        super::sqlite_i64_from_u64_bits(device),
                        super::sqlite_i64_from_u64_bits(inode),
                    )?;
                    aliases.sort_by_key(|(id, _)| *id);
                    for (id, stored) in aliases {
                        checkpoint(cfg)?;
                        // Match the old canonical-path representative: first
                        // detail ID unless the canonical path itself is stored.
                        // Hard links with distinct canonical paths stay distinct.
                        if open_allowed_file(&path_from_db(&stored), cfg)
                            .is_ok_and(|opened| opened.resolved_path == *resolved)
                        {
                            found = Some(id);
                            break;
                        }
                    }
                }
                details.insert(resolved.clone(), found);
                found
            };
            let detail_id = match detail_id {
                Some(id) => Some(id),
                None => db
                    .detail_stats_for_paths(&[path_to_db(&candidate)])?
                    .first()
                    .map(|detail| detail.id),
            };
            if let Some(detail_id) = detail_id {
                detail_ids.push(detail_id);
            }
        }
        let (device, inode) = inode_key(&metadata);
        output.push(DesiredPlaylist {
            name: path
                .file_stem()
                .map(display_os_name)
                .unwrap_or_else(|| "Playlist".to_string()),
            path,
            timestamp: file_mtime_unix(&metadata),
            device: super::sqlite_i64_from_u64_bits(device),
            inode: super::sqlite_i64_from_u64_bits(inode),
            detail_ids,
        });
    }
    Ok(output)
}

pub(crate) fn sync_playlists(db: &LibraryDb, cfg: &ScanConfig) -> ScanResult<bool> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for root in &cfg.media_dirs {
        collect_playlist_paths(cfg, root, &mut seen, &mut paths)?;
    }
    sync_playlist_paths(db, cfg, paths)
}

pub(crate) fn sync_targeted_playlists(
    db: &LibraryDb,
    cfg: &ScanConfig,
    dirty: &[PathBuf],
) -> ScanResult<bool> {
    checkpoint(cfg)?;
    let mut paths: Vec<_> = db
        .playlists()?
        .into_iter()
        .map(|playlist| path_from_db(&playlist.path))
        .collect();
    if paths.is_empty() && !dirty.iter().any(|path| is_playlist(path)) {
        // Ordinary media arrivals must not turn a no-playlist library into
        // an OBJECTS-table cleanup pass on every targeted watcher batch.
        return Ok(false);
    }
    paths.extend(dirty.iter().filter(|path| is_playlist(path)).cloned());
    paths.retain(|path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        !path_excluded(path, &name, cfg)
            && cfg.media_dirs.iter().any(|root| {
                path.strip_prefix(root).is_ok_and(|relative| {
                    relative.components().all(|component| {
                        let name = component.as_os_str();
                        (cfg.include_hidden || !name.as_encoded_bytes().starts_with(b"."))
                            && !is_skipped_dir_os_name(name)
                    })
                })
            })
    });
    sync_playlist_paths(db, cfg, paths)
}

fn sync_playlist_paths(
    db: &LibraryDb,
    cfg: &ScanConfig,
    mut paths: Vec<PathBuf>,
) -> ScanResult<bool> {
    checkpoint(cfg)?;
    paths.sort();
    paths.dedup();
    let desired = desired_playlists(db, cfg, paths)?;
    let existing = db.playlists()?;
    let mut by_inode: HashMap<(i64, i64), _> = existing
        .iter()
        .filter(|row| row.inode != 0)
        .map(|row| ((row.device, row.inode), row))
        .collect();
    let by_path: HashMap<&str, _> = existing
        .iter()
        .map(|row| (row.path.as_str(), row))
        .collect();
    let mut changed = false;
    let mut desired_objects = HashSet::new();
    db.reset_playlist_found()?;
    for playlist in desired {
        checkpoint(cfg)?;
        let path = path_to_db(&playlist.path);
        let old = by_inode
            .remove(&(playlist.device, playlist.inode))
            .or_else(|| by_path.get(path.as_str()).copied());
        if match old {
            None => true,
            Some(row) => {
                row.name != playlist.name
                    || row.path != path
                    || row.timestamp != playlist.timestamp
                    || row.device != playlist.device
                    || row.inode != playlist.inode
            }
        } {
            changed = true;
        }
        let playlist_id = db.upsert_playlist(
            old.map(|row| row.id),
            &playlist.name,
            &path,
            playlist.timestamp,
            playlist.device,
            playlist.inode,
        )?;
        if db.playlist_detail_ids(playlist_id)? != playlist.detail_ids {
            changed = true;
            db.replace_playlist_items(playlist_id, &playlist.detail_ids)?;
        }
        for (root, kind) in [
            (MUSIC_PLIST_ID, "audio"),
            (VIDEO_PLIST_ID, "video"),
            (IMAGE_PLIST_ID, "image"),
        ] {
            let mut members = Vec::new();
            for detail_id in &playlist.detail_ids {
                checkpoint(cfg)?;
                if let Some(source) = db.playlist_object_source(*detail_id)? {
                    if source.1.contains(kind) {
                        members.push((*detail_id, source));
                    }
                }
            }
            if members.is_empty() {
                continue;
            }
            let container = format!("{root}${playlist_id:X}");
            desired_objects.insert(container.clone());
            db.upsert_object(
                &container,
                root,
                "container.storageFolder",
                None,
                &playlist.name,
                None,
            )?;
            for (position, (detail_id, (source, class, title))) in members.iter().enumerate() {
                checkpoint(cfg)?;
                let object = format!("{container}${position:X}");
                desired_objects.insert(object.clone());
                db.upsert_object(
                    &object,
                    &container,
                    class,
                    Some(*detail_id),
                    title,
                    Some(source),
                )?;
            }
        }
    }
    checkpoint(cfg)?;
    db.delete_missing_playlist_objects(&desired_objects)?;
    if db.delete_missing_playlists()? > 0 {
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::NewDetail, tests::TempPath};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn insert(db: &LibraryDb, path: &Path, device: i64, inode: i64) -> i64 {
        db.insert_detail(NewDetail {
            path: &path_to_db(path),
            size: 1,
            timestamp: 1,
            title: "Member",
            date: "2026-01-01",
            mime: "video/x-matroska",
            device,
            inode,
            dlna_pn: None,
        })
        .unwrap()
    }

    #[test]
    fn no_playlists_do_not_read_detail_rows() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let db = LibraryDb::open_memory().unwrap();
        db.connection()
            .authorizer(Some(|context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "DETAILS",
                        ..
                    }
                ) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
        assert!(desired_playlists(&db, &ScanConfig::default(), Vec::new())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn targeted_media_without_playlists_does_not_scan_catalog_objects() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let db = LibraryDb::open_memory().unwrap();
        db.connection()
            .authorizer(Some(|context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "DETAILS" | "OBJECTS",
                        ..
                    }
                ) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
        assert!(!sync_targeted_playlists(
            &db,
            &ScanConfig::default(),
            &[PathBuf::from("/generated/arrival.mkv")],
        )
        .unwrap());
    }

    #[test]
    fn playlist_resolution_work_ignores_unrelated_details() {
        let tmp = TempPath::new("playlist-resolution-work");
        std::fs::create_dir_all(&tmp).unwrap();
        let media = tmp.join("member.mkv");
        std::fs::write(&media, b"member").unwrap();
        let playlist = tmp.join("Queue.m3u");
        std::fs::write(&playlist, "member.mkv\nmember.mkv\n").unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![tmp.to_path_buf()],
            ..Default::default()
        };
        let mut samples = Vec::new();
        for size in [1000, 4000] {
            let db = LibraryDb::open_memory().unwrap();
            let transaction = db.transaction().unwrap();
            let expected = insert(&db, &media, 1, 1);
            for index in 0..size {
                insert(
                    &db,
                    &tmp.join(format!("unrelated-{index}.mkv")),
                    1,
                    index + 2,
                );
            }
            transaction.commit().unwrap();
            let steps = Arc::new(AtomicUsize::new(0));
            let observed = steps.clone();
            db.connection()
                .progress_handler(
                    1,
                    Some(move || {
                        observed.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
            let desired = desired_playlists(&db, &cfg, vec![playlist.clone()]).unwrap();
            assert_eq!(desired[0].detail_ids, [expected, expected]);
            samples.push(steps.load(Ordering::Relaxed));
        }
        eprintln!(
            "playlist_resolution_vm_steps size1000={} size4000={}",
            samples[0], samples[1]
        );
        assert!(
            samples[1] <= samples[0] + 10,
            "indexed member lookup must ignore unrelated details"
        );
    }

    #[cfg(unix)]
    #[test]
    fn playlist_resolution_preserves_canonical_alias_and_non_utf8_members() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let tmp = TempPath::new("playlist-alias-members");
        std::fs::create_dir_all(&tmp).unwrap();
        let media = tmp.join("canonical.mkv");
        let alias = tmp.join("alias.mkv");
        let hardlink = tmp.join("hardlink.mkv");
        let raw = tmp.join(std::ffi::OsString::from_vec(b"raw-\xff.mkv".to_vec()));
        std::fs::write(&media, b"member").unwrap();
        std::fs::write(&raw, b"raw").unwrap();
        symlink(&media, &alias).unwrap();
        std::fs::hard_link(&media, &hardlink).unwrap();
        let (device, inode) = inode_key(&std::fs::metadata(&media).unwrap());
        let device = super::super::sqlite_i64_from_u64_bits(device);
        let inode = super::super::sqlite_i64_from_u64_bits(inode);
        let db = LibraryDb::open_memory().unwrap();
        let alias_id = insert(&db, &alias, device, inode);
        let canonical_id = insert(&db, &media, device, inode);
        let hardlink_id = insert(&db, &hardlink, device, inode);
        let raw_id = insert(&db, &raw, 1, 2);
        let playlist = tmp.join("Queue.m3u8");
        std::fs::write(
            &playlist,
            format!(
                "alias.mkv\nhardlink.mkv\nfile://{}/raw-%FF.mkv\n",
                tmp.display()
            ),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![tmp.to_path_buf()],
            ..Default::default()
        };
        let desired = desired_playlists(&db, &cfg, vec![playlist.clone()]).unwrap();
        assert_eq!(desired[0].detail_ids, [canonical_id, hardlink_id, raw_id]);
        db.connection()
            .execute("DELETE FROM DETAILS WHERE ID = ?1", [canonical_id])
            .unwrap();
        let desired = desired_playlists(&db, &cfg, vec![playlist]).unwrap();
        assert_eq!(desired[0].detail_ids, [alias_id, hardlink_id, raw_id]);
    }
}
