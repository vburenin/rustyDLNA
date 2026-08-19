//! Bounded M3U/M3U8/PLS ingestion and stable playlist object views.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use rusty_dlna_protocol::object_id::{IMAGE_PLIST_ID, MUSIC_PLIST_ID, VIDEO_PLIST_ID};

use crate::db::LibraryDb;
use crate::{
    display_os_name, file_mtime_unix, inode_key, is_skipped_dir, open_allowed_file, path_excluded,
    path_from_db, path_is_allowed_dir, path_is_allowed_file, path_to_db, scan_io, ScanConfig,
    ScanResult,
};

const MAX_PLAYLIST_BYTES: u64 = 1024 * 1024;

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
    if !path_is_allowed_dir(directory, cfg) {
        return Ok(());
    }
    let metadata = std::fs::metadata(directory).map_err(|error| scan_io(directory, error))?;
    if !seen.insert(inode_key(&metadata)) {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory).map_err(|error| scan_io(directory, error))? {
        let entry = entry.map_err(|error| scan_io(directory, error))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if (!cfg.include_hidden && name.starts_with('.')) || path_excluded(&path, &name, cfg) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| scan_io(&path, error))?;
        if file_type.is_dir() || (file_type.is_symlink() && path.is_dir()) {
            if !is_skipped_dir(&name) {
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

fn desired_playlists(db: &LibraryDb, cfg: &ScanConfig) -> ScanResult<Vec<DesiredPlaylist>> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for root in &cfg.media_dirs {
        collect_playlist_paths(cfg, root, &mut seen, &mut paths)?;
    }
    paths.sort();
    let detail_rows = db.all_detail_stats()?;
    let mut details = HashMap::new();
    for detail in detail_rows {
        let decoded = path_from_db(&detail.path);
        if let Ok(canonical) = decoded.canonicalize() {
            details.entry(canonical).or_insert(detail.id);
        }
        details.insert(decoded, detail.id);
    }
    let mut output = Vec::new();
    for path in paths {
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
        let mut bytes = Vec::with_capacity(metadata.len().min(MAX_PLAYLIST_BYTES) as usize);
        opened
            .file
            .by_ref()
            .take(MAX_PLAYLIST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| scan_io(&path, error))?;
        if bytes.len() as u64 > MAX_PLAYLIST_BYTES {
            tracing::warn!(path = %path.display(), "growing playlist exceeded size limit");
            continue;
        }
        let text = match decode_playlist(&path, &bytes) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "playlist rejected");
                continue;
            }
        };
        let mut detail_ids = Vec::new();
        for raw in parse_entries(&path, &text) {
            let Some(candidate) = entry_path(&path, &raw) else {
                continue;
            };
            let Ok(candidate_file) = open_allowed_file(&candidate, cfg) else {
                continue;
            };
            if let Some(detail_id) = details
                .get(&candidate_file.resolved_path)
                .or_else(|| details.get(&candidate))
            {
                detail_ids.push(*detail_id);
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
            device: device as i64,
            inode: inode as i64,
            detail_ids,
        });
    }
    Ok(output)
}

pub(crate) fn sync_playlists(db: &LibraryDb, cfg: &ScanConfig) -> ScanResult<bool> {
    let desired = desired_playlists(db, cfg)?;
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
    db.reset_playlist_found()?;
    db.clear_playlist_objects()?;
    for playlist in desired {
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
            db.upsert_object(
                &container,
                root,
                "container.storageFolder",
                None,
                &playlist.name,
                None,
            )?;
            for (position, (detail_id, (source, class, title))) in members.iter().enumerate() {
                let object = format!("{container}${position:X}");
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
    if db.delete_missing_playlists()? > 0 {
        changed = true;
    }
    Ok(changed)
}
