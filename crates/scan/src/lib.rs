//! Library scan: skip rules, NFO dates, captions, inode reuse, SQLite store.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub mod db;
pub mod nfo;
mod playlist;
pub mod probe;
pub mod watch;
pub use db::{
    mime_to_ext, CatalogDefaultOrder, CatalogQuery, CatalogQueryClause, CatalogQueryField,
    CatalogQueryOp, CatalogQueryPage, CatalogQuerySort, DetailStat, DetailStreamUpdate,
    ExistingDetail, InodeSource, LibraryDb, NewDetail,
};
pub use nfo::{
    episode_display_title, nfo_date_from_text, nfo_for_file, nfo_for_file_with_policy,
    nfo_for_file_with_policy_result, nfo_too_large, parse_nfo_text, split_genres, NfoError,
    NfoMeta,
};
pub use probe::{
    attached_pic_stream, extract_attached_pic, extract_attached_pic_result,
    extract_attached_pic_with_limits_result, extract_attached_pic_with_timeout_result,
    extract_exif_thumbnail_result, extract_exif_thumbnail_with_limit_result, generate_video_thumb,
    generate_video_thumb_result, generate_video_thumb_with_limits_result, probe_image,
    probe_image_with_timeout, probe_media, probe_media_with_timeout, scale_jpeg, scale_jpeg_result,
    scale_jpeg_with_options_result, MediaProbe,
};
pub use watch::{repair_objects_if_needed, run_inotify, run_inotify_until, WatchTelemetry};

use rusty_dlna_protocol::object_id::{
    BROWSEDIR_ID, IMAGE_ALBUM_ID, IMAGE_ALL_ID, IMAGE_CAMERA_ID, IMAGE_DATE_ID, IMAGE_DIR_ID,
    IMAGE_ID, IMAGE_PLIST_ID, IMAGE_RATING_ID, IMAGE_RECENT_ID, MUSIC_ALBUM_ARTIST_ID,
    MUSIC_ALBUM_ID, MUSIC_ALL_ID, MUSIC_ARTIST_ID, MUSIC_COMPOSER_ID, MUSIC_CONTRIB_ARTIST_ID,
    MUSIC_DIR_ID, MUSIC_GENRE_ID, MUSIC_ID, MUSIC_PLIST_ID, MUSIC_RATING_ID, MUSIC_RECENT_ID,
    RECENT_MAX, ROOT_ID, VIDEO_ACTOR_ID, VIDEO_ALL_ID, VIDEO_DIR_ID, VIDEO_GENRE_ID, VIDEO_ID,
    VIDEO_PLIST_ID, VIDEO_RATING_ID, VIDEO_RECENT_ID, VIDEO_SERIES_ID,
};
use rusty_dlna_protocol::w3c_date_from_unix;
use rusty_dlna_protocol::{media_format_for_name, MediaKind, ResolvedMediaFormat};

const PATH_HEX_PREFIX: &str = "RDLNA_PATH_HEX_V1:";
const PATH_UTF8_ESCAPE_PREFIX: &str = "RDLNA_PATH_UTF8_V1:";

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn unhex_bytes(encoded: &str) -> Option<Vec<u8>> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    if encoded.len() & 1 != 0 {
        return None;
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

/// Reversible SQLite TEXT representation. Ordinary UTF-8 stays readable;
/// invalid Unix bytes and reserved-prefix UTF-8 names are hex escaped.
pub fn path_to_db(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        match std::str::from_utf8(bytes) {
            Ok(text)
                if !text.starts_with(PATH_HEX_PREFIX)
                    && !text.starts_with(PATH_UTF8_ESCAPE_PREFIX) =>
            {
                text.to_string()
            }
            Ok(_) => format!("{PATH_UTF8_ESCAPE_PREFIX}{}", hex_bytes(bytes)),
            Err(_) => format!("{PATH_HEX_PREFIX}{}", hex_bytes(bytes)),
        }
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned()
    }
}

pub fn path_from_db(stored: &str) -> PathBuf {
    let encoded = stored
        .strip_prefix(PATH_HEX_PREFIX)
        .or_else(|| stored.strip_prefix(PATH_UTF8_ESCAPE_PREFIX));
    #[cfg(unix)]
    if let Some(bytes) = encoded.and_then(unhex_bytes) {
        use std::os::unix::ffi::OsStringExt;
        return PathBuf::from(std::ffi::OsString::from_vec(bytes));
    }
    PathBuf::from(stored)
}

/// Stable UI text for a filesystem name. Invalid UTF-8 is lossy for display
/// only, with a raw-byte digest so distinct names cannot collapse together.
fn display_os_name(name: &OsStr) -> String {
    if let Some(text) = name.to_str() {
        return text.to_string();
    }
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(name.as_encoded_bytes());
    format!(
        "{} [{:02x}{:02x}{:02x}{:02x}]",
        name.to_string_lossy(),
        digest[0],
        digest[1],
        digest[2],
        digest[3]
    )
}

pub fn is_junk_dir(name: &str) -> bool {
    matches!(
        name,
        "@eaDir"
            | "#recycle"
            | "lost+found"
            | "$RECYCLE.BIN"
            | "System Volume Information"
            | ".Trash"
    ) || name.starts_with(".Trash-")
}

/// Built-in sample/trailer skip is case-sensitive on the directory name
/// (`sample/` not `Sample/`) — dialect `is_sample` path.
pub fn is_sample_or_trailer_dir(name: &str) -> bool {
    name == "sample" || name == "trailer"
}

/// Blu-ray / DVD disc trees. Hundreds of menu/clip bitstreams, not titles.
pub fn is_disc_structure_dir(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "BDMV" | "CERTIFICATE" | "AACS" | "VIDEO_TS" | "AUDIO_TS"
    )
}

/// Any directory the walker must not enter.
pub fn is_skipped_dir(name: &str) -> bool {
    is_junk_dir(name) || is_sample_or_trailer_dir(name) || is_disc_structure_dir(name)
}

/// True when a stored path sits under a skip/exclude rule.
pub fn path_is_unwanted(path: &Path, cfg: &ScanConfig) -> bool {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let live = rebase_media_path_for_config(path, cfg);
    !path_is_allowed_file(&live, cfg)
        || !cfg
            .root_types_for_path(&live)
            .is_some_and(|types| types.allows(&name))
        || path_excluded(path, &name, cfg)
        || path
            .components()
            .any(|c| c.as_os_str().to_str().is_some_and(is_skipped_dir))
        || is_unfinished_name(&name)
        || looks_like_sample_file(&name)
        || is_album_art_name(&name)
}

pub fn is_unfinished_name(name: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        ".part",
        ".!qB",
        ".!ut",
        ".bc!",
        ".crdownload",
        ".aria2",
        ".download",
        ".tmp",
        ".encoding.mp4",
    ];
    SUFFIXES.iter().any(|s| name.ends_with(s))
}

pub fn looks_like_sample_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("-sample.")
        || lower.contains("_sample.")
        || lower.contains("-trailer.")
        || lower == "sample.mkv"
}

pub fn is_caption_name(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".srt")
        || l.ends_with(".ass")
        || l.ends_with(".ssa")
        || l.ends_with(".vtt")
        || l.ends_with(".smi")
        || l.ends_with(".sub")
}

pub fn caption_ext(name: &str) -> &'static str {
    let l = name.to_ascii_lowercase();
    if l.ends_with(".srt") {
        "srt"
    } else if l.ends_with(".ass") {
        "ass"
    } else if l.ends_with(".ssa") {
        "ssa"
    } else if l.ends_with(".vtt") {
        "vtt"
    } else if l.ends_with(".smi") {
        "smi"
    } else {
        "sub"
    }
}

pub fn caption_http_mime(ext: &str) -> &'static str {
    match ext {
        "srt" => "text/srt",
        "ass" | "ssa" => "text/x-ssa",
        "vtt" => "text/vtt",
        "smi" => "smi/caption",
        _ => "text/plain",
    }
}

/// dialect `ends_with` is `strcasecmp` on the suffix (`src/utils.c`).
fn ends_with_ci(name: &str, suffix: &str) -> bool {
    let nb = name.as_bytes();
    let sb = suffix.as_bytes();
    nb.len() >= sb.len() && nb[nb.len() - sb.len()..].eq_ignore_ascii_case(sb)
}

/// dialect `is_video` (`scanner skip rules`).
pub fn is_video(name: &str) -> bool {
    media_format_for_name(name).is_some_and(|format| format.video_mime.is_some())
}

/// dialect `is_audio`.
pub fn is_audio(name: &str) -> bool {
    media_format_for_name(name).is_some_and(|format| format.audio_mime.is_some())
}

/// dialect `is_image` — JPEG only (not PNG).
pub fn is_image(name: &str) -> bool {
    media_format_for_name(name).is_some_and(|format| format.image_mime.is_some())
}

pub fn is_album_art_name(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    if l.ends_with("-poster.jpg")
        || l.ends_with("-poster.jpeg")
        || l.ends_with("-poster.png")
        || l.ends_with("-fanart.jpg")
        || l.ends_with("-fanart.jpeg")
        || l.ends_with("-fanart.png")
    {
        return true;
    }
    matches!(
        l.as_str(),
        "cover.jpg"
            | "cover.jpeg"
            | "cover.png"
            | "folder.jpg"
            | "folder.jpeg"
            | "folder.png"
            | "poster.jpg"
            | "poster.jpeg"
            | "poster.png"
            | "albumart.jpg"
            | "albumart.jpeg"
            | "albumart.png"
            | "albumartsmall.jpg"
            | "albumartsmall.jpeg"
            | "albumartsmall.png"
            | "album.jpg"
            | "album.jpeg"
            | "album.png"
            | "thumb.jpg"
            | "thumb.jpeg"
            | "thumb.png"
    )
}

/// JPEG SOI: `FF D8 FF`.
pub fn is_jpeg_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff
}

/// First matching sidecar next to `video_path`. Stem poster, then fanart,
/// then folder poster/folder/cover (MiniDLNA case variants).
const FOLDER_ART_NAMES: &[&str] = &[
    "poster.jpg",
    "Poster.jpg",
    "poster.png",
    "poster.jpeg",
    "Poster.jpeg",
    "Poster.png",
    "folder.jpg",
    "Folder.jpg",
    "folder.png",
    "folder.jpeg",
    "Folder.jpeg",
    "Folder.png",
    "cover.jpg",
    "Cover.jpg",
    "cover.png",
    "cover.jpeg",
    "Cover.jpeg",
    "Cover.png",
];

pub fn find_album_art(video_path: &Path) -> Option<PathBuf> {
    let parent = video_path.parent()?;
    let stem = video_path.file_stem()?;
    for suffix in ["-poster.jpg", "-poster.png", "-fanart.jpg", "-fanart.png"] {
        let mut name = stem.to_os_string();
        name.push(suffix);
        let p = parent.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    for name in FOLDER_ART_NAMES {
        let p = parent.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn find_album_art_for_config(video_path: &Path, cfg: &ScanConfig) -> Option<PathBuf> {
    let parent = video_path.parent()?;
    if let Some(stem) = video_path.file_stem()?.to_str() {
        for configured in &cfg.album_art_names {
            // Names are basenames by contract. Validation rejects separators, but
            // retain this guard for embedders that construct ScanConfig directly.
            if Path::new(configured)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(configured)
            {
                continue;
            }
            let name = configured.replace("{stem}", stem).replace("%s", stem);
            let candidate = parent.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    find_album_art(video_path)
}

fn find_album_art_in_inventory(
    media_path: &Path,
    cfg: &ScanConfig,
    files: &HashMap<OsString, PathBuf>,
) -> Option<PathBuf> {
    let stem = media_path.file_stem()?;
    if let Some(stem_text) = stem.to_str() {
        for configured in &cfg.album_art_names {
            if Path::new(configured)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(configured.as_str())
            {
                continue;
            }
            let name = configured
                .replace("{stem}", stem_text)
                .replace("%s", stem_text);
            if let Some(path) = files.get(OsStr::new(&name)) {
                return Some(path.clone());
            }
        }
    }
    for suffix in ["-poster.jpg", "-poster.png", "-fanart.jpg", "-fanart.png"] {
        let mut name = stem.to_os_string();
        name.push(suffix);
        if let Some(path) = files.get(&name) {
            return Some(path.clone());
        }
    }
    for name in FOLDER_ART_NAMES {
        if let Some(path) = files.get(OsStr::new(name)) {
            return Some(path.clone());
        }
    }
    None
}

pub fn is_album_art_name_for_config(name: &str, cfg: &ScanConfig) -> bool {
    is_album_art_name(name)
        || cfg.album_art_names.iter().any(|pattern| {
            if pattern.contains("{stem}") || pattern.contains("%s") {
                let suffix = pattern
                    .split_once("{stem}")
                    .or_else(|| pattern.split_once("%s"))
                    .map(|(_, suffix)| suffix)
                    .unwrap_or("");
                !suffix.is_empty()
                    && name
                        .to_ascii_lowercase()
                        .ends_with(&suffix.to_ascii_lowercase())
            } else {
                name.eq_ignore_ascii_case(pattern)
            }
        })
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(data);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn source_image_identity(path: &Path) -> ScanResult<String> {
    use sha1::{Digest, Sha1};
    use std::io::{Read, Seek, SeekFrom};

    let metadata = std::fs::metadata(path).map_err(|error| scan_io(path, error))?;
    let mut hasher = Sha1::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .as_bytes(),
    );
    hasher.update(metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified() {
        if let Ok(modified) = modified.duration_since(std::time::UNIX_EPOCH) {
            hasher.update(modified.as_secs().to_le_bytes());
            hasher.update(modified.subsec_nanos().to_le_bytes());
        }
    }
    let mut file = std::fs::File::open(path).map_err(|error| scan_io(path, error))?;
    const SAMPLE: u64 = 32 * 1024;
    let mut offsets = [
        0,
        metadata.len().saturating_div(2).saturating_sub(SAMPLE / 2),
        metadata.len().saturating_sub(SAMPLE),
    ];
    offsets.sort_unstable();
    let mut last = None;
    let mut sample = vec![0; SAMPLE as usize];
    for offset in offsets {
        if last == Some(offset) {
            continue;
        }
        last = Some(offset);
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| scan_io(path, error))?;
        let read = file
            .read(&mut sample)
            .map_err(|error| scan_io(path, error))?;
        hasher.update(offset.to_le_bytes());
        hasher.update(&sample[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn image_within_limits(cfg: &ScanConfig, src: &Path) -> bool {
    let Some(image) = probe_image_with_timeout(src, cfg.external_command_timeout) else {
        return false;
    };
    u64::from(image.probe.width)
        .checked_mul(u64::from(image.probe.height))
        .is_some_and(|pixels| pixels > 0 && pixels <= cfg.image_max_pixels)
}

fn convert_image_to_jpeg(cfg: &ScanConfig, src: &Path, dest: &Path) -> ScanResult<bool> {
    crate::probe::with_atomic_image_destination(dest, |temporary| {
        let mut command = std::process::Command::new("ffmpeg");
        command
            .args(["-y", "-hide_banner", "-loglevel", "error", "-max_alloc"])
            .arg(cfg.image_memory_limit_bytes.to_string())
            .args(["-threads", "1", "-i"])
            .arg(src)
            .arg(temporary);
        crate::probe::command_status_with_timeout(&mut command, cfg.external_command_timeout)
            .map(|status| status.success())
    })
    .map_err(|error| scan_io(src, error))
}

/// JPEG sidecars stay in place. Anything else is converted once under
/// `{db_path.parent()}/art/{sha1}.jpg`. Memory DBs skip conversion.
pub fn persist_album_art_file(cfg: &ScanConfig, src: &Path) -> ScanResult<Option<PathBuf>> {
    let metadata = std::fs::metadata(src).map_err(|error| scan_io(src, error))?;
    if metadata.len() == 0 || metadata.len() > cfg.image_memory_limit_bytes {
        return Ok(None);
    }
    if !image_within_limits(cfg, src) {
        return Ok(None);
    }
    let mut magic = [0u8; 3];
    let jpeg = std::fs::File::open(src)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic))
        .is_ok()
        && is_jpeg_bytes(&magic);
    if jpeg {
        return Ok(Some(src.to_path_buf()));
    }
    let Some(cache) = cfg.db_path.as_ref().and_then(|path| path.parent()) else {
        return Ok(None);
    };
    let dest = cache
        .join("art")
        .join(format!("{}.jpg", source_image_identity(src)?));
    if dest.is_file() {
        return Ok(Some(dest));
    }
    if convert_image_to_jpeg(cfg, src, &dest)? {
        Ok(Some(dest))
    } else {
        Ok(None)
    }
}

fn store_album_art_path(db: &LibraryDb, stored: &Path, detail_id: i64) -> ScanResult<bool> {
    let stored_s = path_to_db(stored);
    let art_id = db.upsert_album_art(&stored_s)?;
    let prev = db.detail_album_art(detail_id)?;
    if prev == art_id {
        return Ok(false);
    }
    db.set_detail_album_art(detail_id, art_id)?;
    db.copy_album_art_to_inode_aliases(detail_id)?;
    Ok(true)
}

fn cache_art_jpeg(cfg: &ScanConfig, key: &str) -> Option<PathBuf> {
    let cache = cfg.db_path.as_ref()?.parent()?;
    Some(cache.join("art").join(format!("{key}.jpg")))
}

/// Resolve or generate artwork without touching SQLite. The cache identity is
/// physical-file based, so hard links and paths below symlinked directories
/// converge on one derivative instead of running ffmpeg once per DETAILS row.
fn prepare_album_art(cfg: &ScanConfig, path: &Path) -> ScanResult<Option<PathBuf>> {
    if !path_is_allowed_file(path, cfg) {
        return Ok(None);
    }
    if let Some(src) = find_album_art_for_config(path, cfg).filter(|p| path_is_allowed_file(p, cfg))
    {
        if let Some(stored) = persist_album_art_file(cfg, &src)? {
            return Ok(Some(stored));
        }
    }
    let stamp = if cfg.thumbnails {
        source_image_identity(path).ok()
    } else {
        None
    };
    let embedded_dest = stamp
        .as_deref()
        .and_then(|stamp| cache_art_jpeg(cfg, &format!("embed-{stamp}")));
    if let Some(dest) = embedded_dest.as_ref().filter(|dest| dest.is_file()) {
        return Ok(Some(dest.clone()));
    }
    let is_video_item = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_video);
    let thumbnail_dest = (is_video_item && cfg.thumbnails)
        .then(|| {
            stamp
                .as_deref()
                .and_then(|stamp| cache_art_jpeg(cfg, &format!("thumb-{stamp}")))
        })
        .flatten();
    // A generated thumbnail means the first preparation already found no
    // usable sidecar/embedded cover. Reuse that physical cache result instead
    // of retrying attached-picture extraction for every symlink alias and
    // every periodic reconciliation.
    if let Some(dest) = thumbnail_dest.as_ref().filter(|dest| dest.is_file()) {
        return Ok(Some(dest.clone()));
    }
    if let Some(dest) = embedded_dest {
        let image_thumb = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_image)
            && extract_exif_thumbnail_with_limit_result(
                path,
                &dest,
                cfg.image_memory_limit_bytes.min(usize::MAX as u64) as usize,
            )
            .map_err(|error| scan_io(&dest, error))?;
        let generated = dest.is_file()
            || image_thumb
            || extract_attached_pic_with_limits_result(
                path,
                &dest,
                cfg.external_command_timeout,
                cfg.image_memory_limit_bytes,
            )
            .map_err(|error| scan_io(&dest, error))?;
        if generated && dest.is_file() {
            return Ok(Some(dest));
        }
    }
    if let Some(dest) = thumbnail_dest {
        let generated = crate::probe::generate_video_thumb_with_limits_result(
            path,
            &dest,
            cfg.thumbnail_width,
            cfg.thumbnail_quality,
            cfg.thumbnail_filmstrip,
            cfg.external_command_timeout,
            cfg.image_memory_limit_bytes,
        )
        .map_err(|error| scan_io(&dest, error))?;
        if generated && dest.is_file() {
            return Ok(Some(dest));
        }
    }
    Ok(None)
}

fn apply_prepared_album_art(
    db: &LibraryDb,
    detail_id: i64,
    stored: Option<&Path>,
) -> ScanResult<bool> {
    match stored {
        Some(stored) => store_album_art_path(db, stored, detail_id),
        None => db.clear_detail_album_art(detail_id).map_err(Into::into),
    }
}

fn attach_album_art(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    detail_id: i64,
) -> ScanResult<bool> {
    let stored = prepare_album_art(cfg, path)?;
    apply_prepared_album_art(db, detail_id, stored.as_deref())
}

fn attach_album_art_for_index(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    detail_id: i64,
    prepared: Option<&PreparedPhysicalFile>,
) -> ScanResult<bool> {
    match prepared {
        Some(prepared) => apply_prepared_album_art(db, detail_id, prepared.album_art.as_deref()),
        None => attach_album_art(db, cfg, path, detail_id),
    }
}

fn remove_stale_cached_art(cfg: &ScanConfig, paths: &[String]) {
    let Some(owned_dir) = cfg
        .db_path
        .as_ref()
        .and_then(|db_path| db_path.parent())
        .map(|parent| parent.join("art"))
    else {
        return;
    };
    for path in paths {
        let path = path_from_db(path);
        if !path.starts_with(&owned_dir)
            || path.extension().and_then(|ext| ext.to_str()) != Some("jpg")
        {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::debug!(path = %path.display(), "removed unreferenced artwork cache"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not remove unreferenced artwork cache")
            }
        }
    }
}

fn apply_nfo_to_detail(db: &LibraryDb, id: i64, nfo: &NfoMeta) -> ScanResult<()> {
    if nfo.is_empty() {
        return Ok(());
    }
    db.update_detail_nfo(id, nfo)?;
    db.copy_nfo_to_inode_aliases(id)?;
    Ok(())
}

fn find_or_create_container(
    db: &LibraryDb,
    parent: &str,
    name: &str,
    class: &str,
) -> ScanResult<String> {
    if let Some(oid) = db.find_child_object(parent, name)? {
        if db.object_detail_id(&oid)?.is_none() {
            db.upsert_object(&oid, parent, class, None, name, None)?;
            return Ok(oid);
        }
    }
    let id = allocate_child_id(db, parent)?;
    db.upsert_object(&id, parent, class, None, name, None)?;
    Ok(id)
}

fn recent_root(id: &str) -> Option<&'static str> {
    match id {
        VIDEO_RECENT_ID => Some(VIDEO_RECENT_ID),
        MUSIC_RECENT_ID => Some(MUSIC_RECENT_ID),
        IMAGE_RECENT_ID => Some(IMAGE_RECENT_ID),
        _ => None,
    }
}

fn season_folder_title(disc: Option<i64>) -> Option<String> {
    match disc {
        Some(0) => Some("Specials".into()),
        Some(n) => Some(format!("Season {n}")),
        None => None,
    }
}

fn attach_video_virtuals(
    db: &LibraryDb,
    detail: i64,
    class: &str,
    browse_oid: &str,
) -> ScanResult<()> {
    if !class.contains("video") {
        return Ok(());
    }
    let fields = db.detail_group_fields(detail)?;
    let title = fields.title.unwrap_or_default();
    let device = fields.device;
    let inode = fields.inode;
    db.delete_detail_under_root(detail, VIDEO_SERIES_ID)?;
    db.delete_detail_under_root(detail, VIDEO_GENRE_ID)?;
    db.delete_detail_under_root(detail, VIDEO_ACTOR_ID)?;
    if let Some(show) = fields.album.as_deref().filter(|s| !s.is_empty()) {
        let show_id =
            find_or_create_container(db, VIDEO_SERIES_ID, show, "container.album.videoAlbum")?;
        let parent = match season_folder_title(fields.disc) {
            Some(season) => {
                find_or_create_container(db, &show_id, &season, "container.storageFolder")?
            }
            None => show_id,
        };
        let ep_name = episode_display_title(&title, Some(show));
        if !db.folder_has_inode(&parent, device, inode)? {
            let ep_id = format!("{parent}${detail:X}");
            db.upsert_object(
                &ep_id,
                &parent,
                class,
                Some(detail),
                &ep_name,
                Some(browse_oid),
            )?;
        }
    }
    if let Some(g) = fields.genre.as_deref().filter(|s| !s.is_empty()) {
        for name in split_genres(g) {
            let gid =
                find_or_create_container(db, VIDEO_GENRE_ID, &name, "container.genre.videoGenre")?;
            if !db.folder_has_inode(&gid, device, inode)? {
                let iid = format!("{gid}${detail:X}");
                db.upsert_object(&iid, &gid, class, Some(detail), &title, Some(browse_oid))?;
            }
        }
    }
    let tags = db.detail_tag_fields(detail)?;
    if let Some(actor) = tags
        .artist
        .or(tags.creator)
        .filter(|value| !value.is_empty())
    {
        let aid =
            find_or_create_container(db, VIDEO_ACTOR_ID, &actor, "container.person.movieActor")?;
        if !db.folder_has_inode(&aid, device, inode)? {
            let iid = format!("{aid}${detail:X}");
            db.upsert_object(&iid, &aid, class, Some(detail), &title, Some(browse_oid))?;
        }
    }
    Ok(())
}

fn attach_audio_virtuals(
    db: &LibraryDb,
    detail: i64,
    class: &str,
    browse_oid: &str,
) -> ScanResult<()> {
    let tags = db.detail_tag_fields(detail)?;
    for root in [
        MUSIC_GENRE_ID,
        MUSIC_ARTIST_ID,
        MUSIC_ALBUM_ID,
        MUSIC_CONTRIB_ARTIST_ID,
        MUSIC_ALBUM_ARTIST_ID,
        MUSIC_COMPOSER_ID,
        MUSIC_RATING_ID,
    ] {
        db.delete_detail_under_root(detail, root)?;
    }
    let title = tags
        .title
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(browse_oid)
        .to_string();
    if let Some(album) = tags.album.as_deref().filter(|s| !s.is_empty()) {
        let aid =
            find_or_create_container(db, MUSIC_ALBUM_ID, album, "container.album.musicAlbum")?;
        let iid = format!("{aid}${detail:X}");
        db.upsert_object(&iid, &aid, class, Some(detail), &title, Some(browse_oid))?;
    }
    if let Some(artist) = tags.artist.as_deref().filter(|s| !s.is_empty()) {
        let aid =
            find_or_create_container(db, MUSIC_ARTIST_ID, artist, "container.person.musicArtist")?;
        let parent = match tags.album.as_deref().filter(|s| !s.is_empty()) {
            Some(al) => find_or_create_container(db, &aid, al, "container.album.musicAlbum")?,
            None => aid,
        };
        let iid = format!("{parent}${detail:X}");
        db.upsert_object(&iid, &parent, class, Some(detail), &title, Some(browse_oid))?;
    }
    if let Some(g) = tags.genre.as_deref().filter(|s| !s.is_empty()) {
        for name in split_genres(g) {
            let gid =
                find_or_create_container(db, MUSIC_GENRE_ID, &name, "container.genre.musicGenre")?;
            let iid = format!("{gid}${detail:X}");
            db.upsert_object(&iid, &gid, class, Some(detail), &title, Some(browse_oid))?;
        }
    }
    for (root, value, class) in [
        (
            MUSIC_CONTRIB_ARTIST_ID,
            tags.contributor.as_deref(),
            "container.person.musicArtist",
        ),
        (
            MUSIC_ALBUM_ARTIST_ID,
            tags.album_artist.as_deref(),
            "container.person.musicArtist",
        ),
        (
            MUSIC_COMPOSER_ID,
            tags.composer.as_deref(),
            "container.person.musicArtist",
        ),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            let container = find_or_create_container(db, root, value, class)?;
            let item = format!("{container}${detail:X}");
            db.upsert_object(
                &item,
                &container,
                class,
                Some(detail),
                &title,
                Some(browse_oid),
            )?;
        }
    }
    if let Some(rating) = tags.rating {
        let label = rating.to_string();
        let container =
            find_or_create_container(db, MUSIC_RATING_ID, &label, "container.storageFolder")?;
        let item = format!("{container}${detail:X}");
        db.upsert_object(
            &item,
            &container,
            class,
            Some(detail),
            &title,
            Some(browse_oid),
        )?;
    }
    Ok(())
}

fn attach_image_virtuals(
    db: &LibraryDb,
    detail: i64,
    class: &str,
    browse_oid: &str,
) -> ScanResult<()> {
    let tags = db.detail_tag_fields(detail)?;
    db.delete_detail_under_root(detail, IMAGE_DATE_ID)?;
    db.delete_detail_under_root(detail, IMAGE_CAMERA_ID)?;
    db.delete_detail_under_root(detail, IMAGE_ALBUM_ID)?;
    db.delete_detail_under_root(detail, IMAGE_RATING_ID)?;
    let title = db
        .detail_group_fields(detail)?
        .title
        .unwrap_or_else(|| browse_oid.to_string());
    let day = tags
        .date
        .as_deref()
        .filter(|s| s.len() >= 10)
        .map(|s| s[..10].to_string())
        .unwrap_or_else(|| "Unknown Date".into());
    let date_id = find_or_create_container(db, IMAGE_DATE_ID, &day, "container.album.photoAlbum")?;
    let did = format!("{date_id}${detail:X}");
    db.upsert_object(
        &did,
        &date_id,
        class,
        Some(detail),
        &title,
        Some(browse_oid),
    )?;
    let camera = tags
        .creator
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown Camera");
    let cam_id = find_or_create_container(db, IMAGE_CAMERA_ID, camera, "container.storageFolder")?;
    let cam_date = find_or_create_container(db, &cam_id, &day, "container.album.photoAlbum")?;
    let cid = format!("{cam_date}${detail:X}");
    db.upsert_object(
        &cid,
        &cam_date,
        class,
        Some(detail),
        &title,
        Some(browse_oid),
    )?;
    if let Some(al) = tags.album.as_deref().filter(|s| !s.is_empty()) {
        let aid = find_or_create_container(db, IMAGE_ALBUM_ID, al, "container.album.photoAlbum")?;
        let iid = format!("{aid}${detail:X}");
        db.upsert_object(&iid, &aid, class, Some(detail), &title, Some(browse_oid))?;
    }
    if let Some(rating) = tags.rating {
        let label = rating.to_string();
        let rid = find_or_create_container(db, IMAGE_RATING_ID, &label, "container.storageFolder")?;
        let iid = format!("{rid}${detail:X}");
        db.upsert_object(&iid, &rid, class, Some(detail), &title, Some(browse_oid))?;
    }
    Ok(())
}

fn apply_nfo(db: &LibraryDb, cfg: &ScanConfig, path: &Path, detail_id: i64) -> ScanResult<bool> {
    let nfo = nfo_for_file_with_policy_result(path, &cfg.media_dirs, cfg.wide_links)?;
    if nfo.is_empty() {
        return Ok(false);
    }
    apply_nfo_to_detail(db, detail_id, &nfo)?;
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    if is_video(&name) {
        if let Some(browse) = db.browse_object_for_detail(detail_id)? {
            attach_video_virtuals(db, detail_id, "item.videoItem", &browse)?;
        }
    }
    Ok(true)
}

fn apply_nfo_in_dir(
    db: &LibraryDb,
    cfg: &ScanConfig,
    dir: &Path,
    recursive: bool,
) -> ScanResult<bool> {
    if !path_is_allowed_dir(dir, cfg) {
        return Ok(false);
    }
    let rd = std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))?;
    let mut any = false;
    for ent in rd {
        let ent = ent.map_err(|error| scan_io(dir, error))?;
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        let is_dir = match ent.file_type() {
            Ok(t) if t.is_dir() => true,
            Ok(t) if t.is_symlink() => path.is_dir(),
            _ => false,
        };
        if is_dir {
            if recursive
                && !is_skipped_dir(&name)
                && path_is_allowed_dir(&path, cfg)
                && apply_nfo_in_dir(db, cfg, &path, true)?
            {
                any = true;
            }
            continue;
        }
        if !is_video(&name) && !is_audio(&name) {
            continue;
        }
        if !path_is_allowed_file(&path, cfg) {
            continue;
        }
        let path_s = path_to_db(&path);
        if let Some(existing) = db.find_detail_by_path(&path_s)? {
            let id = existing.id;
            let before = db.detail_presentation(id)?;
            let title = path
                .file_stem()
                .map(display_os_name)
                .unwrap_or_else(|| "item".to_string());
            db.reset_detail_tags_to_file_defaults(id, &title, &file_mtime_date(&path))?;
            // Reconstruct the complete precedence chain. This is essential
            // for deletion: merely applying the now-empty NFO would retain
            // values written by the old sidecar forever.
            db.copy_embedded_tags_to_inode_aliases(id)?;
            persist_probe(db, cfg, &path, id)?;
            apply_nfo(db, cfg, &path, id)?;
            if let Some(browse) = db.browse_object_for_detail(id)? {
                let (_, class, _) = mime_and_class(&name);
                if is_video(&name) {
                    attach_video_virtuals(db, id, class, &browse)?;
                } else {
                    attach_audio_virtuals(db, id, class, &browse)?;
                }
            }
            if db.detail_presentation(id)? != before {
                any = true;
            }
        }
    }
    Ok(any)
}

/// Re-apply changed effective NFO metadata once per physical inode. Inotify
/// handles live sidecar deletion; the periodic pass must not reopen media when
/// its selected NFO already matches the persisted presentation.
fn presentation_matches_nfo(presentation: &db::DetailPresentation, nfo: &NfoMeta) -> bool {
    fn matches<T: PartialEq>(actual: &Option<T>, expected: &Option<T>) -> bool {
        expected
            .as_ref()
            .is_none_or(|expected| actual.as_ref() == Some(expected))
    }
    matches(&presentation.title, &nfo.title)
        && matches(&presentation.comment, &nfo.comment)
        && matches(&presentation.genre, &nfo.genre)
        && matches(&presentation.creator, &nfo.creator)
        && matches(&presentation.artist, &nfo.artist)
        && matches(&presentation.disc, &nfo.disc)
        && matches(&presentation.track, &nfo.track)
        && matches(&presentation.date, &nfo.date)
        && matches(&presentation.album, &nfo.showtitle)
}

fn refresh_nfo_periodic(db: &LibraryDb, cfg: &ScanConfig, rows: &[DetailStat]) -> ScanResult<bool> {
    let mut groups: HashMap<(i64, i64, i64), Vec<(i64, PathBuf)>> = HashMap::new();
    for row in rows {
        let unique = if row.inode == 0 { row.id } else { 0 };
        groups
            .entry((row.device, row.inode, unique))
            .or_default()
            .push((row.id, path_from_db(&row.path)));
    }
    let mut changed = false;
    for mut aliases in groups.into_values() {
        aliases.sort_by(|left, right| left.1.cmp(&right.1));
        let mut parsed = Vec::with_capacity(aliases.len());
        for (id, path) in &aliases {
            let live = rebase_media_path_for_config(path, cfg);
            let nfo = nfo_for_file_with_policy_result(&live, &cfg.media_dirs, cfg.wide_links)?;
            parsed.push((*id, live, nfo));
        }
        let selected = parsed
            .iter()
            .find(|(_, _, nfo)| !nfo.is_empty())
            .or_else(|| parsed.first());
        let Some((selected_id, _selected_path, selected_nfo)) = selected else {
            continue;
        };
        let before = aliases
            .iter()
            .map(|(id, _)| db.detail_presentation(*id))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if selected_nfo.is_empty() {
            // Inotify normally observes NFO deletion. If it happened while
            // the service was stopped, video title policy still gives us an
            // unambiguous cheap repair: no NFO means the filename is the
            // title. Do not reopen media merely to recover embedded tags.
            for ((id, path), presentation) in aliases.iter().zip(&before) {
                if !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_video)
                {
                    continue;
                }
                let wanted = path
                    .file_stem()
                    .map(display_os_name)
                    .unwrap_or_else(|| "item".to_string());
                if presentation.title.as_deref() == Some(wanted.as_str()) {
                    continue;
                }
                db.update_detail_title(*id, &wanted)?;
                let fields = db.detail_group_fields(*id)?;
                let series_title = episode_display_title(&wanted, fields.album.as_deref());
                db.update_detail_names_under_root(*id, VIDEO_SERIES_ID, &series_title)?;
                db.update_detail_names_under_root(*id, VIDEO_GENRE_ID, &wanted)?;
                db.update_detail_names_under_root(*id, VIDEO_ACTOR_ID, &wanted)?;
                changed = true;
            }
            continue;
        }
        // Inotify handles live NFO writes/deletes. The periodic pass only
        // needs libav when an effective NFO value differs from persisted
        // presentation metadata; unchanged media must never be re-probed.
        if before
            .iter()
            .all(|presentation| presentation_matches_nfo(presentation, selected_nfo))
        {
            continue;
        }
        // A periodic walk may discover NFO state written while the service was
        // offline. Apply its explicit overrides directly; reconstructing the
        // embedded base requires libav and is reserved for a real sidecar
        // inotify event, never routine reconciliation.
        apply_nfo_to_detail(db, *selected_id, selected_nfo)?;
        for (id, path) in &aliases {
            let name = path
                .file_name()
                .map(|value| value.to_string_lossy())
                .unwrap_or_default();
            let (_, class, _) = mime_and_class(&name);
            if let Some(browse) = db.browse_object_for_detail(*id)? {
                if is_video(&name) {
                    attach_video_virtuals(db, *id, class, &browse)?;
                } else if is_audio(&name) {
                    attach_audio_virtuals(db, *id, class, &browse)?;
                } else if is_image(&name) {
                    attach_image_virtuals(db, *id, class, &browse)?;
                }
            }
        }
        let after = aliases
            .iter()
            .map(|(id, _)| db.detail_presentation(*id))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        changed |= before != after;
    }
    Ok(changed)
}

/// A regular child of a directory already accepted by the root policy cannot
/// escape through its final component. Only symlink children need another
/// canonical jail check.
fn directory_entry_is_allowed_file(
    entry: &std::fs::DirEntry,
    path: &Path,
    cfg: &ScanConfig,
) -> bool {
    entry.file_type().ok().is_some_and(|file_type| {
        file_type.is_file() || (file_type.is_symlink() && path_is_allowed_file(path, cfg))
    })
}

fn refresh_captions_in_dir(db: &LibraryDb, cfg: &ScanConfig, dir: &Path) -> ScanResult<bool> {
    if !path_is_allowed_dir(dir, cfg) {
        return Ok(false);
    }
    let mut media = Vec::new();
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))? {
        let entry = entry.map_err(|error| scan_io(dir, error))?;
        let path = entry.path();
        if !directory_entry_is_allowed_file(&entry, &path, cfg) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_video(&name) {
            media.push(path);
        } else if cfg.subtitles && is_caption_name(&name) {
            candidates.push(path);
        }
    }
    candidates.sort();
    let mut changed = false;
    for path in media {
        if let Some(existing) = db.find_detail_by_path(&path_to_db(&path))? {
            let captions = captions_from_candidates(&path, &candidates);
            changed |= db.replace_captions(existing.id, &captions)?;
        }
    }
    Ok(changed)
}

/// Refresh only media whose stem can own this caption. `touched` is true even
/// when the path list is unchanged, because CLOSE_WRITE changed subtitle
/// bytes served at the same URL and clients need one new update generation.
fn refresh_caption_event(db: &LibraryDb, cfg: &ScanConfig, sidecar: &Path) -> ScanResult<bool> {
    let Some(dir) = sidecar.parent() else {
        return Ok(false);
    };
    if !path_is_allowed_dir(dir, cfg) {
        return Ok(false);
    }
    let mut touched = false;
    for entry in std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))? {
        let entry = entry.map_err(|error| scan_io(dir, error))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_video(&name) || !directory_entry_is_allowed_file(&entry, &path, cfg) {
            continue;
        }
        if !caption_path_matches_media(sidecar, &path) {
            continue;
        }
        if let Some(existing) = db.find_detail_by_path(&path_to_db(&path))? {
            db.replace_captions(existing.id, &captions_for(&path, cfg)?)?;
            touched = true;
        }
    }
    Ok(touched)
}

pub fn artwork_path_matches_media(sidecar: &Path, media: &Path) -> bool {
    let Some(sidecar_name) = sidecar.file_name() else {
        return false;
    };
    if let Some(text) = sidecar_name.to_str() {
        let lower = text.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "poster.jpg"
                | "poster.jpeg"
                | "poster.png"
                | "folder.jpg"
                | "folder.jpeg"
                | "folder.png"
                | "cover.jpg"
                | "cover.jpeg"
                | "cover.png"
        ) {
            return true;
        }
    }
    let Some(stem) = media.file_stem().map(OsStr::as_encoded_bytes) else {
        return false;
    };
    let candidate = sidecar_name.as_encoded_bytes();
    [
        b"-poster.jpg".as_slice(),
        b"-poster.png",
        b"-fanart.jpg",
        b"-fanart.png",
    ]
    .iter()
    .any(|suffix| {
        candidate.len() == stem.len() + suffix.len()
            && candidate[..stem.len()] == *stem
            && candidate[stem.len()..].eq_ignore_ascii_case(suffix)
    })
}

fn refresh_artwork_event(db: &LibraryDb, cfg: &ScanConfig, sidecar: &Path) -> ScanResult<bool> {
    let Some(dir) = sidecar.parent() else {
        return Ok(false);
    };
    if !path_is_allowed_dir(dir, cfg) {
        return Ok(false);
    }
    let mut touched = false;
    for entry in std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))? {
        let entry = entry.map_err(|error| scan_io(dir, error))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if (!is_video(&name) && !is_audio(&name) && !is_image(&name))
            || !directory_entry_is_allowed_file(&entry, &path, cfg)
            || !artwork_path_matches_media(sidecar, &path)
        {
            continue;
        }
        if let Some(existing) = db.find_detail_by_path(&path_to_db(&path))? {
            attach_album_art(db, cfg, &path, existing.id)?;
            touched = true;
        }
    }
    Ok(touched)
}

fn attach_album_art_in_dir(db: &LibraryDb, cfg: &ScanConfig, dir: &Path) -> ScanResult<bool> {
    if !path_is_allowed_dir(dir, cfg) {
        return Ok(false);
    }
    let mut files = HashMap::new();
    let mut media = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))? {
        let entry = entry.map_err(|error| scan_io(dir, error))?;
        let path = entry.path();
        if !directory_entry_is_allowed_file(&entry, &path, cfg) {
            continue;
        }
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if is_video(&name_text) || is_audio(&name_text) {
            media.push(path.clone());
        }
        files.insert(name, path);
    }
    let mut any = false;
    for path in media {
        let path_s = path_to_db(&path);
        if let Some(existing) = db.find_detail_by_path(&path_s)? {
            let sidecar = find_album_art_in_inventory(&path, cfg, &files);
            let art_id = db.detail_album_art(existing.id)?;
            let current = (art_id > 0)
                .then(|| db.album_art_path(art_id))
                .transpose()?
                .flatten()
                .map(|stored| path_from_db(&stored));
            if let Some(current) = current.as_ref().filter(|stored| stored.is_file()) {
                let generated = current
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("thumb-") || name.starts_with("embed-"));
                if (generated && sidecar.is_none()) || (!generated && sidecar.is_some()) {
                    continue;
                }
            }
            if attach_album_art(db, cfg, &path, existing.id)? {
                any = true;
            }
        }
    }
    Ok(any)
}

/// Which media classes a `media_dir=` root accepts (dialect `V,` / `A,` / `P,`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaTypes {
    pub video: bool,
    pub audio: bool,
    pub image: bool,
}

impl MediaTypes {
    pub fn all() -> Self {
        Self {
            video: true,
            audio: true,
            image: true,
        }
    }
    pub fn none() -> Self {
        Self {
            video: false,
            audio: false,
            image: false,
        }
    }
    pub fn video_only() -> Self {
        Self {
            video: true,
            audio: false,
            image: false,
        }
    }
    pub fn audio_only() -> Self {
        Self {
            video: false,
            audio: true,
            image: false,
        }
    }
    pub fn union(self, other: Self) -> Self {
        Self {
            video: self.video || other.video,
            audio: self.audio || other.audio,
            image: self.image || other.image,
        }
    }
    pub fn allows(&self, name: &str) -> bool {
        (self.video && is_video(name))
            || (self.audio && is_audio(name))
            || (self.image && is_image(name))
    }
}

impl Default for MediaTypes {
    fn default() -> Self {
        Self::all()
    }
}

/// Parse dialect `media_dir=V,/path` or a bare path. Default types = AVP.
pub fn parse_media_dir(spec: &str) -> (MediaTypes, PathBuf) {
    if let Some((prefix, rest)) = spec.split_once(',') {
        let p = prefix.trim();
        if p.chars()
            .all(|c| matches!(c, 'A' | 'a' | 'V' | 'v' | 'P' | 'p'))
            && !rest.is_empty()
        {
            let mut t = MediaTypes {
                video: false,
                audio: false,
                image: false,
            };
            for c in p.chars() {
                match c {
                    'V' | 'v' => t.video = true,
                    'A' | 'a' => t.audio = true,
                    'P' | 'p' => t.image = true,
                    _ => {}
                }
            }
            return (t, PathBuf::from(rest.trim()));
        }
    }
    (MediaTypes::all(), PathBuf::from(spec))
}

/// Combine dialect `media_dir=` specs. Each prefix is parsed; the
/// returned `MediaTypes` is the **union** so a later `A,` cannot wipe an
/// earlier `V,`. Empty list → all types (AVP), no dirs.
pub fn collect_media_dirs<I, S>(specs: I) -> (Vec<PathBuf>, MediaTypes)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut dirs = Vec::new();
    let mut types = MediaTypes::none();
    let mut any = false;
    for spec in specs {
        let (t, p) = parse_media_dir(spec.as_ref());
        types = types.union(t);
        dirs.push(p);
        any = true;
    }
    if !any {
        types = MediaTypes::all();
    }
    (dirs, types)
}

/// One configured media root. Unlike the legacy `media_dirs`/`types` pair,
/// this keeps identity and the A/V/P mask attached to the directory that owns
/// them. `aliases` contains paths persisted by an earlier run (for example a
/// host path before the database was moved into a container).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRoot {
    pub configured_path: PathBuf,
    pub canonical_path: PathBuf,
    pub key: String,
    pub display_title: String,
    pub types: MediaTypes,
    pub aliases: Vec<PathBuf>,
}

impl MediaRoot {
    fn path_candidates(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.configured_path.as_path())
            .chain(std::iter::once(self.canonical_path.as_path()))
            .chain(self.aliases.iter().map(PathBuf::as_path))
    }
}

fn stable_media_root_key(title: &str) -> String {
    format!("root-{}", &sha1_hex(title.to_lowercase().as_bytes())[..16])
}

/// Parse and validate all configured media roots before the scanner or HTTP
/// server can use them. Empty input is valid (an empty library), but every
/// configured root must exist and be a directory. Canonically duplicate,
/// nested, and case-insensitively same-basename roots are rejected because
/// they make ownership and relocation ambiguous.
pub fn build_media_roots<I, S>(specs: I, config_dir: &Path) -> Result<Vec<MediaRoot>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut roots = Vec::new();
    for spec in specs {
        let (types, raw) = parse_media_dir(spec.as_ref());
        let configured_path = if raw.is_absolute() {
            raw
        } else {
            config_dir.join(raw)
        };
        let canonical_path = configured_path.canonicalize().map_err(|error| {
            format!(
                "media root does not exist or cannot be resolved: {} ({error})",
                configured_path.display()
            )
        })?;
        if !canonical_path.is_dir() {
            return Err(format!(
                "media root is not a directory: {}",
                configured_path.display()
            ));
        }
        let display_title = configured_path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "media root must have a UTF-8 directory name: {}",
                    configured_path.display()
                )
            })?
            .to_string();
        roots.push(MediaRoot {
            configured_path,
            canonical_path,
            key: stable_media_root_key(&display_title),
            display_title,
            types,
            aliases: Vec::new(),
        });
    }

    for left in 0..roots.len() {
        for right in left + 1..roots.len() {
            let a = &roots[left];
            let b = &roots[right];
            if a.canonical_path == b.canonical_path {
                return Err(format!(
                    "duplicate media roots resolve to {}",
                    a.canonical_path.display()
                ));
            }
            if a.canonical_path.starts_with(&b.canonical_path)
                || b.canonical_path.starts_with(&a.canonical_path)
            {
                return Err(format!(
                    "nested media roots are ambiguous: {} and {}",
                    a.configured_path.display(),
                    b.configured_path.display()
                ));
            }
            if a.display_title.eq_ignore_ascii_case(&b.display_title) {
                return Err(format!(
                    "media roots must have distinct directory names: {} and {}",
                    a.configured_path.display(),
                    b.configured_path.display()
                ));
            }
        }
    }
    Ok(roots)
}

/// dialect `GetVideoMetadata`: reject non-media. Strong container magic
/// (EBML/ftyp/RIFF/…) is proof the file is a real bitstream, not text.
/// Ambiguous headers (TS/MPEG/MP3) get a short `ffprobe`.
pub fn file_is_viable(path: &Path) -> bool {
    file_is_viable_with_timeout(path, std::time::Duration::from_secs(30))
}

fn file_is_viable_with_timeout(path: &Path, timeout: std::time::Duration) -> bool {
    match sniff_container(path) {
        Sniff::Reject => false,
        Sniff::Strong => true,
        Sniff::Weak => ffprobe_has_av_stream(path, timeout).unwrap_or(false),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sniff {
    Reject,
    Strong,
    Weak,
}

fn sniff_container(path: &Path) -> Sniff {
    if looks_like_av_container(path) {
        // looks_like already distinguished strong vs anything; refine:
        use std::io::Read;
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return Sniff::Reject,
        };
        let mut buf = [0u8; 16];
        let n = match f.read(&mut buf) {
            Ok(n) => n,
            Err(_) => return Sniff::Reject,
        };
        if n < 4 {
            return Sniff::Reject;
        }
        if (buf[0] == 0x1a && buf[1] == 0x45 && buf[2] == 0xdf && buf[3] == 0xa3)
            || (n >= 8 && matches!(&buf[4..8], b"ftyp" | b"mdat" | b"moov" | b"wide" | b"free"))
            || &buf[0..4] == b"RIFF"
            || &buf[0..3] == b"FLV"
            || (buf[0] == 0x30 && buf[1] == 0x26 && buf[2] == 0xb2 && buf[3] == 0x75)
            || &buf[0..4] == b"OggS"
            || (buf[0] == 0xff && buf[1] == 0xd8 && buf[2] == 0xff)
            || &buf[0..4] == b"fLaC"
        {
            return Sniff::Strong;
        }
        return Sniff::Weak;
    }
    Sniff::Reject
}

fn ffprobe_has_av_stream(path: &Path, timeout: std::time::Duration) -> Option<bool> {
    let mut command = std::process::Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-probesize",
            "262144",
            "-analyzeduration",
            "200000",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path);
    let out = crate::probe::command_output_with_timeout(&mut command, timeout).ok()?;
    if !out.status.success() {
        return Some(false);
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(
        s.lines()
            .any(|l| l.contains("video") || l.contains("audio")),
    )
}

/// First-bytes sniff so a `.mkv` that is actually text/NFO is not indexed.
pub fn looks_like_av_container(path: &Path) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 16];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    if n < 4 {
        return false;
    }
    // Matroska / WebM EBML
    if buf[0] == 0x1a && buf[1] == 0x45 && buf[2] == 0xdf && buf[3] == 0xa3 {
        return true;
    }
    // ISO BMFF (mp4/m4v/mov): size + "ftyp" / "mdat" / "moov"
    if n >= 8 && matches!(&buf[4..8], b"ftyp" | b"mdat" | b"moov" | b"wide" | b"free") {
        return true;
    }
    // RIFF AVI / WAV
    if &buf[0..4] == b"RIFF" {
        return true;
    }
    // MPEG-TS sync
    if buf[0] == 0x47 {
        return true;
    }
    // MPEG-PS / VOB pack
    if buf[0] == 0x00 && buf[1] == 0x00 && buf[2] == 0x01 {
        return true;
    }
    // FLV
    if &buf[0..3] == b"FLV" {
        return true;
    }
    // ASF / WMV
    if buf[0] == 0x30 && buf[1] == 0x26 && buf[2] == 0xb2 && buf[3] == 0x75 {
        return true;
    }
    // Ogg
    if &buf[0..4] == b"OggS" {
        return true;
    }
    // JPEG
    if buf[0] == 0xff && buf[1] == 0xd8 && buf[2] == 0xff {
        return true;
    }
    // ID3 / MP3
    if &buf[0..3] == b"ID3" || (buf[0] == 0xff && (buf[1] & 0xe0) == 0xe0) {
        return true;
    }
    // FLAC
    if &buf[0..4] == b"fLaC" {
        return true;
    }
    false
}

/// Minimal EBML header so tests can stand in for a real MKV without libav.
pub fn write_fake_mkv(path: &Path, size: usize) {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // Prefer a real container so `file_is_viable` / ffprobe pass.
    if std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=0.5:size=32x32:rate=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.5",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-hide_banner",
            "-loglevel",
            "error",
        ])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return;
    }
    let n = size.max(4);
    let mut data = vec![0u8; n];
    data[0] = 0x1a;
    data[1] = 0x45;
    data[2] = 0xdf;
    data[3] = 0xa3;
    for (i, b) in data.iter_mut().enumerate().skip(4) {
        *b = (i % 251) as u8;
    }
    std::fs::write(path, data).expect("write fake mkv");
}

/// ISO BMFF with `ftyp` + `mdat` and no `moov` — libav reports "moov atom not found".
pub fn write_incomplete_mp4(path: &Path, size: usize) {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let n = size.max(28);
    let mut data = vec![0u8; n];
    data[0..4].copy_from_slice(&20u32.to_be_bytes());
    data[4..8].copy_from_slice(b"ftyp");
    data[8..12].copy_from_slice(b"isom");
    data[12..16].copy_from_slice(&0u32.to_be_bytes());
    data[16..20].copy_from_slice(b"isom");
    let mdat = (n as u32 - 20).to_be_bytes();
    data[20..24].copy_from_slice(&mdat);
    data[24..28].copy_from_slice(b"mdat");
    std::fs::write(path, data).expect("write incomplete mp4");
}

fn resolved_media_format(name: &str, probe: Option<&MediaProbe>) -> Option<ResolvedMediaFormat> {
    resolved_media_format_with_hint(name, probe, None)
}

fn resolved_media_format_with_hint(
    name: &str,
    probe: Option<&MediaProbe>,
    mime_hint: Option<&str>,
) -> Option<ResolvedMediaFormat> {
    let format = media_format_for_name(name)?;
    let detected = probe
        .and_then(|got| {
            if !got.probe.video.is_empty() {
                Some(MediaKind::Video)
            } else if !got.probe.audio.is_empty() {
                Some(MediaKind::Audio)
            } else {
                None
            }
        })
        .or_else(|| match mime_hint.unwrap_or_default() {
            mime if mime.starts_with("video/") => Some(MediaKind::Video),
            mime if mime.starts_with("audio/") => Some(MediaKind::Audio),
            mime if mime.starts_with("image/") => Some(MediaKind::Image),
            _ => None,
        });
    Some(format.resolve(detected))
}

fn mime_and_class(name: &str) -> (&'static str, &'static str, &'static str) {
    resolved_media_format(name, None)
        .map(|format| (format.mime, format.upnp_class(), format.extension))
        .unwrap_or(("application/octet-stream", "item.videoItem", "bin"))
}

#[derive(Clone, Debug, Default)]
pub struct SourceProbe {
    pub container: String,
    pub video: String,
    pub hdr: String,
    pub audio: String,
    /// Comma-separated `global-stream:audio-ordinal:codec:channels` records.
    /// Unlike `audio`, this preserves duplicate codecs and real stream order.
    pub audio_streams: String,
    pub width: u32,
    pub height: u32,
}

pub fn parse_probe_toml(text: &str) -> SourceProbe {
    let mut p = SourceProbe::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"');
        match k {
            "container" => p.container = v.to_string(),
            "video" => p.video = v.to_string(),
            "hdr" => p.hdr = v.to_string(),
            "audio" => p.audio = v.to_string(),
            "audio_streams" => p.audio_streams = v.to_string(),
            "width" => p.width = v.parse().unwrap_or(0),
            "height" => p.height = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    p
}

#[derive(Clone, Debug)]
pub struct Caption {
    pub index: u32,
    pub path: PathBuf,
    pub ext: String,
}

#[derive(Clone, Debug)]
pub struct MediaItem {
    pub object_id: String,
    pub parent_id: String,
    pub detail_id: i64,
    pub title: String,
    pub class: String,
    pub date: String,
    pub path: PathBuf,
    pub mime: String,
    pub ext: String,
    pub size: u64,
    pub mtime: i64,
    pub captions: Vec<Caption>,
    pub probe: SourceProbe,
    pub dlna_pn: Option<String>,
    pub ref_id: Option<String>,
    pub device: u64,
    pub inode: u64,
    pub duration: Option<String>,
    pub bitrate: Option<i64>,
    pub resolution: Option<String>,
    pub channels: Option<i64>,
    pub samplerate: Option<i64>,
    /// `DETAILS.ALBUM_ART` (`0` = none).
    pub album_art: i64,
    pub creator: Option<String>,
    pub comment: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub contributor: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub disc: Option<i64>,
    pub track: Option<i64>,
    pub rating: Option<i64>,
    pub rotation: Option<i64>,
    /// `BOOKMARKS.SEC` (raw seconds). Default 0 when no row.
    pub bookmark_sec: i64,
    /// `BOOKMARKS.WATCH_COUNT`. Default 0 when no row.
    pub watch_count: i64,
}

/// dialect `duration_str` (`H:MM:SS.mmm` from milliseconds).
pub fn duration_str(msec: i64) -> String {
    let msec = msec.max(0);
    format!(
        "{}:{:02}:{:02}.{:03}",
        msec / 3_600_000,
        (msec / 60_000) % 60,
        (msec / 1000) % 60,
        msec % 1000
    )
}

#[derive(Clone, Debug, Default)]
pub struct AvMeta {
    pub duration: Option<String>,
    pub bitrate: Option<i64>,
    pub resolution: Option<String>,
    pub channels: Option<i64>,
    pub samplerate: Option<i64>,
    /// Embedded subtitle codecs, comma-separated (`dvd_subtitle,mov_text`).
    pub subs: Option<String>,
    /// AVI DiVX fourcc → `CREATOR=DiVX`.
    pub creator: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmbeddedTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub contributor: Option<String>,
    pub date: Option<String>,
    pub comment: Option<String>,
    pub disc: Option<i64>,
    pub track: Option<i64>,
    pub rating: Option<i64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub rotation: Option<i64>,
}

/// dialect `GetVideoMetadata` / lav: duration, bitrate/8, WxH, audio.
pub fn probe_av_meta(path: &Path) -> Option<AvMeta> {
    let mut command = std::process::Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-probesize",
            "10000000",
            "-analyzeduration",
            "5000000",
            "-show_entries",
            "format=duration,bit_rate:stream=codec_type,width,height,sample_rate,channels",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path);
    let out =
        crate::probe::command_output_with_timeout(&mut command, std::time::Duration::from_secs(30))
            .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut meta = AvMeta::default();
    let mut w = 0u32;
    let mut h = 0u32;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "duration" => {
                if let Ok(secs) = v.trim().parse::<f64>() {
                    if secs.is_finite() && secs > 0.0 {
                        meta.duration = Some(duration_str((secs * 1000.0) as i64));
                    }
                }
            }
            "bit_rate" => {
                if let Ok(br) = v.trim().parse::<i64>() {
                    if br > 8 {
                        meta.bitrate = Some(br / 8);
                    }
                }
            }
            "width" => w = v.trim().parse().unwrap_or(0),
            "height" => h = v.trim().parse().unwrap_or(0),
            "sample_rate" => meta.samplerate = v.trim().parse().ok(),
            "channels" => meta.channels = v.trim().parse().ok(),
            _ => {}
        }
    }
    if w > 0 && h > 0 {
        meta.resolution = Some(format!("{w}x{h}"));
    }
    if meta.duration.is_none() && meta.bitrate.is_none() && meta.resolution.is_none() {
        return None;
    }
    Some(meta)
}

#[derive(Clone, Debug)]
pub struct Container {
    pub object_id: String,
    pub parent_id: String,
    pub title: String,
    pub class: String,
    pub children: Vec<String>,
    pub searchable: bool,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    pub containers: HashMap<String, Container>,
    pub items: HashMap<String, MediaItem>,
    pub by_detail: HashMap<i64, String>,
    pub next_detail: i64,
    /// Unique browse-folder videos, capped at `RECENT_MAX`.
    pub recent_count: u32,
    /// Newest unique browse-folder item ids (already sorted).
    pub recent_ids: Vec<String>,
    /// `ALBUM_ART.ID` → stored JPEG path.
    pub album_art_paths: HashMap<i64, PathBuf>,
    recent_limit: usize,
    recent_cutoff_unix: Option<i64>,
}

impl Catalog {
    pub fn new() -> Self {
        let mut c = Self {
            containers: HashMap::new(),
            items: HashMap::new(),
            by_detail: HashMap::new(),
            next_detail: 1,
            recent_count: 0,
            recent_ids: Vec::new(),
            album_art_paths: HashMap::new(),
            recent_limit: RECENT_MAX,
            recent_cutoff_unix: None,
        };
        c.add_container(ROOT_ID, "-1", "root", "container.storageFolder", true);
        c.add_container(
            BROWSEDIR_ID,
            ROOT_ID,
            "Browse Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(MUSIC_ID, ROOT_ID, "Music", "container.storageFolder", true);
        c.add_container(VIDEO_ID, ROOT_ID, "Video", "container.storageFolder", true);
        c.add_container(
            IMAGE_ID,
            ROOT_ID,
            "Pictures",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_ALL_ID,
            VIDEO_ID,
            "All Video",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_DIR_ID,
            VIDEO_ID,
            "Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_RECENT_ID,
            VIDEO_ID,
            "Recently Added",
            "container.storageFolder",
            false,
        );
        c.add_container(
            VIDEO_SERIES_ID,
            VIDEO_ID,
            "Series",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_GENRE_ID,
            VIDEO_ID,
            "Genre",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_ACTOR_ID,
            VIDEO_ID,
            "Actor",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_PLIST_ID,
            VIDEO_ID,
            "Playlists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            VIDEO_RATING_ID,
            VIDEO_ID,
            "Rating",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ALL_ID,
            MUSIC_ID,
            "All Music",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_GENRE_ID,
            MUSIC_ID,
            "Genre",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ARTIST_ID,
            MUSIC_ID,
            "Artist",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ALBUM_ID,
            MUSIC_ID,
            "Album",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_DIR_ID,
            MUSIC_ID,
            "Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_PLIST_ID,
            MUSIC_ID,
            "Playlists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_CONTRIB_ARTIST_ID,
            MUSIC_ID,
            "Contributing Artists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_ALBUM_ARTIST_ID,
            MUSIC_ID,
            "Album Artist",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_COMPOSER_ID,
            MUSIC_ID,
            "Composer",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_RATING_ID,
            MUSIC_ID,
            "Rating",
            "container.storageFolder",
            true,
        );
        c.add_container(
            MUSIC_RECENT_ID,
            MUSIC_ID,
            "Recently Added",
            "container.storageFolder",
            false,
        );
        c.add_container(
            IMAGE_ALL_ID,
            IMAGE_ID,
            "All Pictures",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_DATE_ID,
            IMAGE_ID,
            "Date Taken",
            "container.album.photoAlbum",
            true,
        );
        c.add_container(
            IMAGE_ALBUM_ID,
            IMAGE_ID,
            "Album",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_CAMERA_ID,
            IMAGE_ID,
            "Camera",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_DIR_ID,
            IMAGE_ID,
            "Folders",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_PLIST_ID,
            IMAGE_ID,
            "Playlists",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_RATING_ID,
            IMAGE_ID,
            "Rating",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_RECENT_ID,
            IMAGE_ID,
            "Recently Added",
            "container.storageFolder",
            false,
        );
        c.link_child(ROOT_ID, BROWSEDIR_ID);
        c.link_child(ROOT_ID, MUSIC_ID);
        c.link_child(ROOT_ID, VIDEO_ID);
        c.link_child(ROOT_ID, IMAGE_ID);
        c.link_child(VIDEO_ID, VIDEO_ALL_ID);
        c.link_child(VIDEO_ID, VIDEO_DIR_ID);
        c.link_child(VIDEO_ID, VIDEO_RECENT_ID);
        c.link_child(VIDEO_ID, VIDEO_SERIES_ID);
        c.link_child(VIDEO_ID, VIDEO_GENRE_ID);
        c.link_child(VIDEO_ID, VIDEO_ACTOR_ID);
        c.link_child(VIDEO_ID, VIDEO_PLIST_ID);
        c.link_child(VIDEO_ID, VIDEO_RATING_ID);
        c.link_child(MUSIC_ID, MUSIC_ALL_ID);
        c.link_child(MUSIC_ID, MUSIC_GENRE_ID);
        c.link_child(MUSIC_ID, MUSIC_ARTIST_ID);
        c.link_child(MUSIC_ID, MUSIC_ALBUM_ID);
        c.link_child(MUSIC_ID, MUSIC_DIR_ID);
        c.link_child(MUSIC_ID, MUSIC_PLIST_ID);
        c.link_child(MUSIC_ID, MUSIC_CONTRIB_ARTIST_ID);
        c.link_child(MUSIC_ID, MUSIC_ALBUM_ARTIST_ID);
        c.link_child(MUSIC_ID, MUSIC_COMPOSER_ID);
        c.link_child(MUSIC_ID, MUSIC_RATING_ID);
        c.link_child(MUSIC_ID, MUSIC_RECENT_ID);
        c.link_child(IMAGE_ID, IMAGE_ALL_ID);
        c.link_child(IMAGE_ID, IMAGE_DATE_ID);
        c.link_child(IMAGE_ID, IMAGE_ALBUM_ID);
        c.link_child(IMAGE_ID, IMAGE_CAMERA_ID);
        c.link_child(IMAGE_ID, IMAGE_DIR_ID);
        c.link_child(IMAGE_ID, IMAGE_PLIST_ID);
        c.link_child(IMAGE_ID, IMAGE_RATING_ID);
        c.link_child(IMAGE_ID, IMAGE_RECENT_ID);
        c
    }

    fn add_container(
        &mut self,
        id: &str,
        parent: &str,
        title: &str,
        class: &str,
        searchable: bool,
    ) {
        self.containers.insert(
            id.to_string(),
            Container {
                object_id: id.to_string(),
                parent_id: parent.to_string(),
                title: title.to_string(),
                class: class.to_string(),
                children: Vec::new(),
                searchable,
            },
        );
    }

    fn link_child(&mut self, parent: &str, child: &str) {
        if let Some(p) = self.containers.get_mut(parent) {
            if !p.children.iter().any(|c| c == child) {
                p.children.push(child.to_string());
            }
        }
    }

    pub fn get_item_by_detail(&self, id: i64) -> Option<&MediaItem> {
        let oid = self.by_detail.get(&id)?;
        self.items.get(oid)
    }

    /// Approximate owned bytes for capacity planning. This includes value
    /// structs and the heap buffers directly owned by catalog strings,
    /// vectors, captions, paths, and index entries; allocator/hash-table
    /// bucket overhead is intentionally reported as an estimate.
    pub fn estimated_memory_bytes(&self) -> u64 {
        fn string_bytes(value: &String) -> usize {
            value.capacity()
        }
        fn optional_string_bytes(value: &Option<String>) -> usize {
            value.as_ref().map(string_bytes).unwrap_or(0)
        }
        let mut bytes = self
            .items
            .len()
            .saturating_mul(std::mem::size_of::<MediaItem>())
            .saturating_add(
                self.containers
                    .len()
                    .saturating_mul(std::mem::size_of::<Container>()),
            );
        for (key, item) in &self.items {
            bytes = bytes
                .saturating_add(key.capacity())
                .saturating_add(item.object_id.capacity())
                .saturating_add(item.parent_id.capacity())
                .saturating_add(item.title.capacity())
                .saturating_add(item.class.capacity())
                .saturating_add(item.date.capacity())
                .saturating_add(item.path.as_os_str().as_encoded_bytes().len())
                .saturating_add(item.mime.capacity())
                .saturating_add(item.ext.capacity())
                .saturating_add(item.probe.container.capacity())
                .saturating_add(item.probe.video.capacity())
                .saturating_add(item.probe.hdr.capacity())
                .saturating_add(item.probe.audio.capacity())
                .saturating_add(item.probe.audio_streams.capacity())
                .saturating_add(optional_string_bytes(&item.dlna_pn))
                .saturating_add(optional_string_bytes(&item.ref_id))
                .saturating_add(optional_string_bytes(&item.duration))
                .saturating_add(optional_string_bytes(&item.resolution))
                .saturating_add(optional_string_bytes(&item.creator))
                .saturating_add(optional_string_bytes(&item.comment))
                .saturating_add(optional_string_bytes(&item.artist))
                .saturating_add(optional_string_bytes(&item.album_artist))
                .saturating_add(optional_string_bytes(&item.composer))
                .saturating_add(optional_string_bytes(&item.contributor))
                .saturating_add(optional_string_bytes(&item.album))
                .saturating_add(optional_string_bytes(&item.genre))
                .saturating_add(
                    item.captions
                        .len()
                        .saturating_mul(std::mem::size_of::<Caption>()),
                );
            for caption in &item.captions {
                bytes = bytes
                    .saturating_add(caption.path.as_os_str().as_encoded_bytes().len())
                    .saturating_add(caption.ext.capacity());
            }
        }
        for (key, container) in &self.containers {
            bytes = bytes
                .saturating_add(key.capacity())
                .saturating_add(container.object_id.capacity())
                .saturating_add(container.parent_id.capacity())
                .saturating_add(container.title.capacity())
                .saturating_add(container.class.capacity())
                .saturating_add(
                    container
                        .children
                        .iter()
                        .map(|child| child.capacity())
                        .sum::<usize>(),
                );
        }
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    pub fn children_of(&self, id: &str) -> Option<Vec<CatalogChild>> {
        self.page_children(id, 0, usize::MAX).map(|(ch, _)| ch)
    }

    /// Sorted children, cloning only `[start, start+take)`. Folders first,
    /// then title (ASCII case-insensitive) so VLC shows expand controls
    /// above loose files.
    pub fn page_children(
        &self,
        id: &str,
        start: usize,
        take: usize,
    ) -> Option<(Vec<CatalogChild>, u32)> {
        if let Some(root) = recent_root(id) {
            let mut all = self.recent_items(root);
            let total = all.len() as u32;
            if start >= all.len() || take == 0 {
                return Some((Vec::new(), total));
            }
            let end = all.len().min(start.saturating_add(take));
            let page = all.drain(start..end).collect();
            return Some((page, total));
        }
        let c = self.containers.get(id)?;
        let mut keys: Vec<(bool, &str, &str)> = Vec::with_capacity(c.children.len());
        for ch in &c.children {
            if let Some(cont) = self.containers.get(ch) {
                keys.push((true, cont.title.as_str(), ch.as_str()));
            } else if let Some(it) = self.items.get(ch) {
                keys.push((false, it.title.as_str(), ch.as_str()));
            }
        }
        keys.sort_by(|a, b| match (a.0, b.0) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => cmp_ignore_ascii_case(a.1, b.1),
        });
        let total = keys.len() as u32;
        let page = keys
            .into_iter()
            .skip(start)
            .take(take)
            .filter_map(|(_, _, oid)| {
                if let Some(cont) = self.containers.get(oid) {
                    Some(CatalogChild::Container(cont.clone()))
                } else {
                    self.items
                        .get(oid)
                        .cloned()
                        .map(Box::new)
                        .map(CatalogChild::Item)
                }
            })
            .collect();
        Some((page, total))
    }

    pub fn displayed_child_count(&self, id: &str) -> u32 {
        if recent_root(id).is_some() {
            return self.recent_items(id).len() as u32;
        }
        self.containers
            .get(id)
            .map(|c| c.children.len() as u32)
            .unwrap_or(0)
    }

    pub fn displayed_container_count(&self, id: &str) -> u32 {
        if recent_root(id).is_some() {
            return 0;
        }
        self.containers
            .get(id)
            .map(|c| {
                c.children
                    .iter()
                    .filter(|ch| self.containers.contains_key(*ch))
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Newest unique videos (inode-deduped so symlink aliases count once),
    /// newest first, up to `RECENT_MAX`. Object IDs are `2$FF0$` + source id.
    pub fn recent_videos(&self) -> Vec<CatalogChild> {
        self.recent_items(VIDEO_RECENT_ID)
    }

    pub fn recent_items(&self, root: &str) -> Vec<CatalogChild> {
        if root == VIDEO_RECENT_ID && !self.recent_ids.is_empty() {
            return self
                .recent_ids
                .iter()
                .filter_map(|id| {
                    let it = self.items.get(id)?;
                    let mut clone = it.clone();
                    clone.object_id = format!("{root}${id}");
                    clone.parent_id = root.to_string();
                    Some(CatalogChild::Item(Box::new(clone)))
                })
                .collect();
        }
        let class_pat = match root {
            MUSIC_RECENT_ID => "audio",
            IMAGE_RECENT_ID => "image",
            _ => "video",
        };
        let mut items: Vec<&MediaItem> = self
            .items
            .values()
            .filter(|i| {
                i.class.contains(class_pat)
                    && i.ref_id.is_none()
                    && i.object_id.starts_with(BROWSEDIR_ID)
                    && self
                        .recent_cutoff_unix
                        .map(|cutoff| normalized_mtime_seconds(i.mtime) >= cutoff)
                        .unwrap_or(true)
            })
            .collect();
        items.sort_by(|a, b| {
            normalized_mtime_seconds(b.mtime)
                .cmp(&normalized_mtime_seconds(a.mtime))
                .then_with(|| b.mtime.cmp(&a.mtime))
                .then_with(|| path_is_symlink(&a.path).cmp(&path_is_symlink(&b.path)))
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.object_id.cmp(&b.object_id))
        });
        let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
        let mut ids = Vec::new();
        for it in items {
            let key = if it.inode != 0 {
                (it.device, it.inode)
            } else {
                (0, it.detail_id as u64)
            };
            if seen.contains_key(&key) {
                continue;
            }
            seen.insert(key, ());
            ids.push(it.object_id.clone());
            if ids.len() == self.recent_limit {
                break;
            }
        }
        ids.into_iter()
            .filter_map(|id| {
                let it = self.items.get(&id)?;
                let mut clone = it.clone();
                clone.object_id = format!("{root}${id}");
                clone.parent_id = root.to_string();
                Some(CatalogChild::Item(Box::new(clone)))
            })
            .collect()
    }

    fn rebuild_recent_index(&mut self) {
        self.recent_ids = self
            .recent_items(VIDEO_RECENT_ID)
            .into_iter()
            .filter_map(|ch| match ch {
                CatalogChild::Item(it) => it
                    .object_id
                    .strip_prefix(&format!("{VIDEO_RECENT_ID}$"))
                    .map(str::to_string),
                _ => None,
            })
            .collect();
        self.recent_count = self.recent_ids.len() as u32;
    }

    pub fn configure_recent_policy(&mut self, limit: usize, days: Option<u32>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        self.configure_recent_policy_at(limit, days, now);
    }

    pub fn configure_recent_policy_at(&mut self, limit: usize, days: Option<u32>, now: i64) {
        self.recent_limit = limit.max(1);
        self.recent_cutoff_unix =
            days.map(|days| now.saturating_sub(i64::from(days).saturating_mul(24 * 60 * 60)));
        self.recent_ids.clear();
        self.recent_count = 0;
        self.rebuild_recent_index();
    }

    pub fn metadata(&self, id: &str) -> Option<CatalogChild> {
        let prefix = format!("{VIDEO_RECENT_ID}$");
        if let Some(real) = id.strip_prefix(&prefix) {
            if !real.is_empty() {
                return self.metadata(real).map(|ch| match ch {
                    CatalogChild::Item(mut it) => {
                        it.object_id = id.to_string();
                        it.parent_id = VIDEO_RECENT_ID.to_string();
                        CatalogChild::Item(it)
                    }
                    other => other,
                });
            }
        }
        if let Some(c) = self.containers.get(id) {
            return Some(CatalogChild::Container(c.clone()));
        }
        if let Some(it) = self.items.get(id) {
            return Some(CatalogChild::Item(Box::new(it.clone())));
        }
        // Infuse / libupnp caches ObjectID. After a rebuild the Browse
        // Folders id may have changed; All Video is `2$8$` + detail hex
        // and some clients send the bare DETAILS.ID.
        self.metadata_by_detail(id)
    }

    fn metadata_by_detail(&self, id: &str) -> Option<CatalogChild> {
        let did = if let Some(hex) = id
            .strip_prefix(VIDEO_ALL_ID)
            .and_then(|s| s.strip_prefix('$'))
            .filter(|s| !s.is_empty() && !s.contains('$'))
        {
            i64::from_str_radix(hex, 16).ok()?
        } else if id.bytes().all(|b| b.is_ascii_digit()) {
            let n: i64 = id.parse().ok()?;
            // `0`/`1`/`2`/`3`/`64` are virtual containers, never DETAILS.ID.
            if matches!(n, 0 | 1 | 2 | 3 | 64) {
                return None;
            }
            n
        } else {
            return None;
        };
        let it = self.get_item_by_detail(did)?.clone();
        let mut it = it;
        if id.starts_with(VIDEO_ALL_ID) {
            it.object_id = id.to_string();
            it.parent_id = VIDEO_ALL_ID.to_string();
        }
        Some(CatalogChild::Item(Box::new(it)))
    }

    /// Mirror Browse Folders video files into `2$15` so Video/Folders works
    /// even when the last `files.db` predates this view.
    pub fn ensure_video_folder_mirrors(&mut self) {
        if !self.containers.contains_key(VIDEO_DIR_ID) {
            self.add_container(
                VIDEO_DIR_ID,
                VIDEO_ID,
                "Folders",
                "container.storageFolder",
                true,
            );
            self.link_child(VIDEO_ID, VIDEO_DIR_ID);
        }
        if !self.containers.contains_key(VIDEO_RECENT_ID) {
            self.add_container(
                VIDEO_RECENT_ID,
                VIDEO_ID,
                "Recently Added",
                "container.storageFolder",
                false,
            );
            self.link_child(VIDEO_ID, VIDEO_RECENT_ID);
        }
        if !self.containers.contains_key(VIDEO_SERIES_ID) {
            self.add_container(
                VIDEO_SERIES_ID,
                VIDEO_ID,
                "Series",
                "container.storageFolder",
                true,
            );
            self.link_child(VIDEO_ID, VIDEO_SERIES_ID);
        }
        if !self.containers.contains_key(VIDEO_GENRE_ID) {
            self.add_container(
                VIDEO_GENRE_ID,
                VIDEO_ID,
                "Genre",
                "container.storageFolder",
                true,
            );
            self.link_child(VIDEO_ID, VIDEO_GENRE_ID);
        }
        let videos: Vec<MediaItem> = self
            .items
            .values()
            .filter(|i| i.class.contains("video") && i.object_id.starts_with(BROWSEDIR_ID))
            .cloned()
            .collect();
        for it in videos {
            self.mirror_video_dir_ancestors(&it.parent_id);
            let vobj = browse_to_typed_dir(&it.object_id, VIDEO_DIR_ID);
            let vparent = browse_to_typed_dir(&it.parent_id, VIDEO_DIR_ID);
            if self.items.contains_key(&vobj) {
                continue;
            }
            let mut clone = it.clone();
            clone.object_id = vobj.clone();
            clone.parent_id = vparent.clone();
            clone.ref_id = Some(it.object_id.clone());
            self.link_child(&vparent, &vobj);
            self.items.insert(vobj, clone);
        }
        self.rebuild_recent_index();
    }

    fn mirror_video_dir_ancestors(&mut self, browse_folder_id: &str) {
        let mut chain = Vec::new();
        let mut cur = browse_folder_id.to_string();
        while cur != BROWSEDIR_ID && cur != ROOT_ID {
            chain.push(cur.clone());
            match self.containers.get(&cur) {
                Some(c) => cur = c.parent_id.clone(),
                None => break,
            }
        }
        chain.reverse();
        for bid in chain {
            let Some(cont) = self.containers.get(&bid).cloned() else {
                continue;
            };
            let vid = browse_to_typed_dir(&bid, VIDEO_DIR_ID);
            let vparent = browse_to_typed_dir(&cont.parent_id, VIDEO_DIR_ID);
            if !self.containers.contains_key(&vid) {
                self.add_container(&vid, &vparent, &cont.title, "container.storageFolder", true);
            }
            self.link_child(&vparent, &vid);
        }
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub enum CatalogChild {
    Container(Container),
    Item(Box<MediaItem>),
}

#[derive(Clone, Debug)]
pub struct ScanConfig {
    /// Authoritative production root model. When populated, all ownership,
    /// filtering, reconciliation, and relocation decisions use these records.
    pub media_roots: Vec<MediaRoot>,
    /// Compatibility mirror for older embedders and tests. New callers should
    /// populate `media_roots`; production keeps this equal to configured paths.
    pub media_dirs: Vec<PathBuf>,
    pub exclude_dirs: Vec<String>,
    pub exclude_files: Vec<String>,
    /// Include dot-prefixed files and directories. Default false matches the
    /// reference's privacy-oriented scan policy.
    pub include_hidden: bool,
    /// Extra folder-art basenames. `{stem}` and `%s` expand to the media stem.
    pub album_art_names: Vec<String>,
    pub subtitles: bool,
    pub thumbnails: bool,
    pub thumbnail_width: u32,
    pub thumbnail_quality: u8,
    pub thumbnail_filmstrip: bool,
    /// Maximum decoded still-image pixels and a per-allocation/source byte cap.
    pub image_max_pixels: u64,
    pub image_memory_limit_bytes: u64,
    /// Hard deadline for libav exploration and ffmpeg/ffprobe helpers.
    pub external_command_timeout: std::time::Duration,
    /// Maximum physical media files prepared concurrently. SQLite publication
    /// remains deterministic and single-threaded; only libav/filesystem/
    /// thumbnail work runs in this bounded pool.
    pub scan_workers: usize,
    /// Maximum unique physical items per Recently Added media class.
    pub recent_limit: usize,
    /// Optional mtime window in days. Future mtimes remain eligible.
    pub recent_days: Option<u32>,
    /// Keep Kodi resume positions and play counts for this many 24-hour days
    /// since their last update. Zero means indefinite retention.
    pub bookmark_retention_days: u32,
    /// dialect `media_dir=V,…` filter. Default = all (AVP).
    pub types: MediaTypes,
    /// dialect `files.db`. None = in-memory SQLite (tests).
    pub db_path: Option<PathBuf>,
    /// Follow directory/file symlinks whose canonical target is outside every
    /// configured media root. This is intentionally false by default because
    /// enabling it exposes content reachable through links below a media root.
    pub wide_links: bool,
    /// Optional lock-free-ish progress telemetry shared with the server.
    pub progress: Option<std::sync::Arc<ScanProgress>>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            media_roots: Vec::new(),
            media_dirs: Vec::new(),
            exclude_dirs: Vec::new(),
            exclude_files: Vec::new(),
            include_hidden: false,
            album_art_names: Vec::new(),
            subtitles: true,
            thumbnails: true,
            thumbnail_width: 320,
            thumbnail_quality: 2,
            thumbnail_filmstrip: false,
            image_max_pixels: 40_000_000,
            image_memory_limit_bytes: 256 * 1024 * 1024,
            external_command_timeout: std::time::Duration::from_secs(30),
            scan_workers: default_scan_workers(),
            recent_limit: RECENT_MAX,
            recent_days: None,
            bookmark_retention_days: 0,
            types: MediaTypes::default(),
            db_path: None,
            wide_links: false,
            progress: None,
        }
    }
}

/// A bounded, hardware-aware default. Sixteen keeps large initial scans busy
/// on modern hosts without allowing an unbounded number of libav/ffmpeg jobs.
pub fn default_scan_workers() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get().min(16))
        .unwrap_or(4)
        .max(1)
}

#[derive(Debug, Default)]
pub struct ScanProgress {
    files_seen: std::sync::atomic::AtomicU64,
    current_path: std::sync::Mutex<Option<PathBuf>>,
}

impl ScanProgress {
    fn reset(&self) {
        self.files_seen
            .store(0, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut path) = self.current_path.lock() {
            *path = None;
        }
    }

    fn record(&self, path: &Path) {
        self.files_seen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut current) = self.current_path.lock() {
            *current = Some(path.to_path_buf());
        }
    }

    pub fn snapshot(&self) -> (u64, Option<PathBuf>) {
        (
            self.files_seen.load(std::sync::atomic::Ordering::Relaxed),
            self.current_path
                .lock()
                .map(|path| path.clone())
                .unwrap_or_default(),
        )
    }
}

fn normalized_mtime_seconds(value: i64) -> i64 {
    // Schema generations before nanosecond replacement tracking stored Unix
    // seconds. Treat large values as nanoseconds so upgrades are seamless.
    if value.unsigned_abs() > 10_000_000_000 {
        value / 1_000_000_000
    } else {
        value
    }
}

#[derive(Clone, Copy)]
struct SelectedRoot<'a> {
    key: &'a str,
    title: &'a str,
    types: MediaTypes,
    relative_to: &'a Path,
    configured_path: &'a Path,
}

impl ScanConfig {
    fn selected_root<'a>(&'a self, path: &Path) -> Option<SelectedRoot<'a>> {
        if !self.media_roots.is_empty() {
            return self.media_roots.iter().find_map(|root| {
                root.path_candidates().find_map(|candidate| {
                    path.strip_prefix(candidate).ok().map(|_| SelectedRoot {
                        key: &root.key,
                        title: &root.display_title,
                        types: root.types,
                        relative_to: candidate,
                        configured_path: &root.configured_path,
                    })
                })
            });
        }
        self.media_dirs.iter().find_map(|root| {
            path.strip_prefix(root).ok().map(|_| SelectedRoot {
                key: root.file_name().and_then(|s| s.to_str()).unwrap_or("media"),
                title: root.file_name().and_then(|s| s.to_str()).unwrap_or("media"),
                types: self.types,
                relative_to: root,
                configured_path: root,
            })
        })
    }

    fn root_types_for_path(&self, path: &Path) -> Option<MediaTypes> {
        self.selected_root(path).map(|root| root.types)
    }

    fn root_title_for_path(&self, path: &Path) -> Option<&str> {
        self.selected_root(path).map(|root| root.title)
    }
}

/// Load the prior configured/canonical path for each stable root key, then
/// persist this run's paths. A moved database can therefore translate catalog
/// paths explicitly without guessing from directory component names.
pub fn load_and_persist_media_root_mappings(
    roots: &mut [MediaRoot],
    db_path: &Path,
) -> Result<(), String> {
    let db = open_library_db(db_path)
        .map_err(|error| format!("open media-root mapping database: {error}"))?;
    for root in roots.iter_mut() {
        for field in ["configured", "canonical"] {
            let setting_key = format!("media_root:{}:{field}", root.key);
            if let Some(value) = db
                .setting(&setting_key)
                .map_err(|error| format!("read media-root mapping {setting_key}: {error}"))?
            {
                let old = path_from_db(&value);
                if old != root.configured_path
                    && old != root.canonical_path
                    && !root.aliases.contains(&old)
                {
                    root.aliases.push(old);
                }
            }
        }
        db.set_setting(
            &format!("media_root:{}:configured", root.key),
            &path_to_db(&root.configured_path),
        )
        .map_err(|error| format!("persist configured media-root path: {error}"))?;
        db.set_setting(
            &format!("media_root:{}:canonical", root.key),
            &path_to_db(&root.canonical_path),
        )
        .map_err(|error| format!("persist canonical media-root path: {error}"))?;
    }

    for left in 0..roots.len() {
        for right in left + 1..roots.len() {
            for a in roots[left].path_candidates() {
                for b in roots[right].path_candidates() {
                    if a == b || a.starts_with(b) || b.starts_with(a) {
                        return Err(format!(
                            "persisted media-root mappings are ambiguous: {} and {}",
                            a.display(),
                            b.display()
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanDelta {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("library database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("scanner invariant failed: {0}")]
    Invariant(String),
    #[error(transparent)]
    Nfo(#[from] NfoError),
}

pub type ScanResult<T> = Result<T, ScanError>;

fn scan_io(path: &Path, source: std::io::Error) -> ScanError {
    ScanError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn is_corrupt_database_error(error: &rusqlite::Error) -> bool {
    let rusqlite::Error::SqliteFailure(error, _) = error else {
        return false;
    };
    let primary = error.extended_code & 0xff;
    primary == rusqlite::ffi::SQLITE_CORRUPT || primary == rusqlite::ffi::SQLITE_NOTADB
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// Open the catalog, preserving a corrupt database and its WAL sidecars before
/// creating a fresh database. Busy, permission, migration, and other errors
/// are propagated unchanged: those states are retryable and must never be
/// mistaken for corruption.
pub(crate) fn open_library_db(path: &Path) -> ScanResult<LibraryDb> {
    match LibraryDb::open(path) {
        Ok(db) => Ok(db),
        Err(error) if is_corrupt_database_error(&error) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let marker = format!(".corrupt-{stamp}-{}", std::process::id());
            let backup = path_with_suffix(path, &marker);
            std::fs::rename(path, &backup).map_err(|source| scan_io(path, source))?;
            for sidecar in ["-wal", "-shm"] {
                let source_path = path_with_suffix(path, sidecar);
                if source_path.exists() {
                    let backup_path = path_with_suffix(&backup, sidecar);
                    std::fs::rename(&source_path, &backup_path)
                        .map_err(|source| scan_io(&source_path, source))?;
                }
            }
            tracing::error!(
                target: "rusty_dlna",
                path = %path.display(),
                backup = %backup.display(),
                %error,
                "corrupt library database preserved; rebuilding a fresh catalog"
            );
            Ok(LibraryDb::open(path)?)
        }
        Err(error) => Err(error.into()),
    }
}

/// Persist the UPnP SystemUpdateID using the same checked/recovering database
/// open policy as scanner writes.
pub fn persist_system_update_id(path: &Path, id: u32) -> ScanResult<()> {
    open_library_db(path)?.set_update_id(id)?;
    Ok(())
}

pub fn path_is_live_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Album art / caption files served into RAM. Larger → 413.
pub const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;

fn canonical_path_is_under_roots(real: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| {
        root.canonicalize()
            .ok()
            .is_some_and(|canonical_root| real.starts_with(canonical_root))
    })
}

fn lexical_path_is_under_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| {
        path.starts_with(root)
            || root
                .canonicalize()
                .ok()
                .is_some_and(|canonical_root| path.starts_with(canonical_root))
    })
}

fn path_is_allowed_kind(
    path: &Path,
    roots: &[PathBuf],
    wide_links: bool,
    wanted: impl FnOnce(&std::fs::Metadata) -> bool,
) -> bool {
    if roots.is_empty() {
        return false;
    }
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(&real) else {
        return false;
    };
    if !wanted(&meta) {
        return false;
    }
    canonical_path_is_under_roots(&real, roots)
        || (wide_links && lexical_path_is_under_roots(path, roots))
}

/// Apply the scanner's root policy to a regular file. With `wide_links=false`
/// both the walked name and its final canonical target stay jailed. With the
/// explicit opt-in enabled, a link lexically below a configured root may point
/// outside it and the same rule is used by HTTP serving.
pub fn path_is_allowed_file(path: &Path, cfg: &ScanConfig) -> bool {
    path_is_allowed_kind(path, &cfg.media_dirs, cfg.wide_links, |meta| meta.is_file())
}

/// Directory counterpart to [`path_is_allowed_file`], used by both walkers and
/// the inotify watch builder before opening or descending a directory.
pub fn path_is_allowed_dir(path: &Path, cfg: &ScanConfig) -> bool {
    path_is_allowed_kind(path, &cfg.media_dirs, cfg.wide_links, |meta| meta.is_dir())
}

/// True if `path` is a regular file whose canonical location is under one
/// of `roots`. Follows symlinks, so a link that escapes the tree is false.
pub fn path_is_under_roots(path: &Path, roots: &[PathBuf]) -> bool {
    path_is_allowed_kind(path, roots, false, |meta| meta.is_file())
}

/// Rebase a persisted media path using the root record that owns it. This does
/// not search for a coincidentally matching directory-name component.
pub fn rebase_media_path_for_config(stored: &Path, cfg: &ScanConfig) -> PathBuf {
    if stored.as_os_str().is_empty() {
        return stored.to_path_buf();
    }
    let Some(root) = cfg.selected_root(stored) else {
        return stored.to_path_buf();
    };
    let Ok(relative) = stored.strip_prefix(root.relative_to) else {
        return stored.to_path_buf();
    };
    let candidate = root.configured_path.join(relative);
    if path_is_live_file(&candidate) {
        candidate
    } else {
        stored.to_path_buf()
    }
}

/// Stable reconciliation key qualified by its selected media-root identity.
pub fn media_rel_key_for_config(path: &Path, cfg: &ScanConfig) -> String {
    let Some(root) = cfg.selected_root(path) else {
        return path_to_db(path);
    };
    let Ok(relative) = path.strip_prefix(root.relative_to) else {
        return path_to_db(path);
    };
    format!("{}:{}", root.key, path_to_db(relative))
}

fn paths_are_same_media(stored: &str, event: &Path, cfg: &ScanConfig) -> bool {
    let stored_path = path_from_db(stored);
    if stored_path == event {
        return true;
    }
    let key = media_rel_key_for_config(event, cfg);
    !key.is_empty() && media_rel_key_for_config(&stored_path, cfg) == key
}

fn path_is_under_watched(stored: &str, dir: &Path, cfg: &ScanConfig) -> bool {
    let stored_path = path_from_db(stored);
    let stored_p = stored_path.as_path();
    if stored_p == dir || stored_p.starts_with(dir) {
        return true;
    }
    let drel = media_rel_key_for_config(dir, cfg);
    if drel.is_empty() {
        return false;
    }
    let frel = media_rel_key_for_config(stored_p, cfg);
    frel == drel || frel.starts_with(&format!("{drel}/"))
}

fn open_library(cfg: &ScanConfig) -> ScanResult<Option<LibraryDb>> {
    match &cfg.db_path {
        Some(p) => Ok(Some(open_library_db(p)?)),
        None => Ok(None),
    }
}

/// Drop DETAILS/OBJECTS for `path`, matching host-realpath vs container-mount
/// prefixes (e.g. `/mnt/pool/video/…` vs `/storage/video/…`).
pub fn forget_path(cfg: &ScanConfig, path: &Path) -> ScanResult<usize> {
    forget_matching(cfg, path, false)
}

/// Drop every DETAILS row under `dir`, using the same prefix-alias rules.
pub fn forget_tree(cfg: &ScanConfig, dir: &Path) -> ScanResult<usize> {
    forget_matching(cfg, dir, true)
}

fn forget_matching(cfg: &ScanConfig, path: &Path, tree: bool) -> ScanResult<usize> {
    let _write = library_write_guard();
    let Some(db) = open_library(cfg)? else {
        return Ok(0);
    };
    let transaction = db.transaction()?;
    let rows = db.all_detail_stats()?;
    let mut n = 0usize;
    for row in rows {
        let p = row.path;
        let hit = if tree {
            path_is_under_watched(&p, path, cfg)
        } else {
            paths_are_same_media(&p, path, cfg)
        };
        if hit {
            n += db.remove_path_and_symlink_aliases(&p)?;
        }
    }
    transaction.commit()?;
    Ok(n)
}

pub fn path_is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn cmp_ignore_ascii_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.as_bytes()
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.as_bytes().iter().map(|c| c.to_ascii_lowercase()))
}

fn file_mtime_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub(crate) fn path_excluded(path: &Path, name: &str, cfg: &ScanConfig) -> bool {
    for e in &cfg.exclude_dirs {
        if dir_exclude_matches(path, name, e) {
            return true;
        }
    }
    for e in &cfg.exclude_files {
        if basename_glob_matches(e, name) {
            return true;
        }
    }
    false
}

/// MiniDLNA `exclude_file` matching: basename only, ASCII case-insensitive,
/// with `*` (zero or more bytes) and `?` (one byte).
pub fn basename_glob_matches(pattern: &str, name: &str) -> bool {
    fn matches(pattern: &[u8], name: &[u8]) -> bool {
        match pattern.first().copied() {
            None => name.is_empty(),
            Some(b'*') => {
                let rest = pattern
                    .iter()
                    .position(|byte| *byte != b'*')
                    .unwrap_or(pattern.len());
                let pattern = &pattern[rest..];
                pattern.is_empty()
                    || (0..=name.len()).any(|offset| matches(pattern, &name[offset..]))
            }
            Some(b'?') => !name.is_empty() && matches(&pattern[1..], &name[1..]),
            Some(expected) => {
                name.first()
                    .is_some_and(|actual| expected.eq_ignore_ascii_case(actual))
                    && matches(&pattern[1..], &name[1..])
            }
        }
    }
    matches(pattern.as_bytes(), name.as_bytes())
}

/// dialect `exclude_dir`: a path component (`incomplete`) or a suffix
/// (`video/incomplete`). Never walk or index those trees.
fn dir_exclude_matches(path: &Path, name: &str, rule: &str) -> bool {
    let rule = rule.trim_matches('/');
    if rule.is_empty() {
        return false;
    }
    if name.eq_ignore_ascii_case(rule) {
        return true;
    }
    let path_l = path.to_string_lossy().to_ascii_lowercase();
    let rule_l = rule.to_ascii_lowercase();
    if path_l.split('/').any(|c| c == rule_l) {
        return true;
    }
    path_l.contains(&format!("/{rule_l}/")) || path_l.ends_with(&format!("/{rule_l}"))
}

fn file_mtime_date(path: &Path) -> String {
    let unix = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    w3c_date_from_unix(unix).unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

pub fn caption_path_matches_media(sidecar: &Path, media: &Path) -> bool {
    let Some(ext) = sidecar.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    if !matches!(
        ext.to_ascii_lowercase().as_str(),
        "srt" | "ass" | "ssa" | "vtt" | "smi" | "sub"
    ) {
        return false;
    }
    let Some(base) = sidecar.file_stem().map(OsStr::as_encoded_bytes) else {
        return false;
    };
    let Some(stem) = media.file_stem().map(OsStr::as_encoded_bytes) else {
        return false;
    };
    base == stem
        || base
            .strip_prefix(stem)
            .is_some_and(|variant| variant.starts_with(b".") && variant.len() > 1)
}

fn captions_from_candidates(file: &Path, candidates: &[PathBuf]) -> Vec<Caption> {
    candidates
        .iter()
        .filter(|path| caption_path_matches_media(path, file))
        .enumerate()
        .map(|(index, path)| Caption {
            index: index as u32,
            path: path.clone(),
            ext: path
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| caption_ext(&format!("x.{value}")))
                .unwrap_or("sub")
                .into(),
        })
        .collect()
}

fn captions_for(file: &Path, cfg: &ScanConfig) -> ScanResult<Vec<Caption>> {
    if !cfg.subtitles {
        return Ok(Vec::new());
    }
    let parent = match file.parent() {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };
    let rd = std::fs::read_dir(parent).map_err(|error| scan_io(parent, error))?;
    let mut names = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|error| scan_io(parent, error))?;
        let path = entry.path();
        if path_is_allowed_file(&path, cfg) && caption_path_matches_media(&path, file) {
            names.push(path);
        }
    }
    names.sort();
    Ok(captions_from_candidates(file, &names))
}

/// Cheap HDR guess from a title or path. Browse uses this so a folder
/// listing never waits on ffprobe of an 80 GiB remux.
pub fn guess_hdr_from_name(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.contains("dv-p7")
        || n.contains("dvp7")
        || n.contains("dvhe.07")
        || n.contains("profile-7")
        || n.contains("profile.7")
    {
        return Some("dv-p7");
    }
    if n.contains("dv-p8")
        || n.contains("dvp8")
        || n.contains("dvhe.08")
        || n.contains("profile-8")
        || n.contains("profile.8")
    {
        return Some("dv-p8");
    }
    let web = n.contains("web-dl") || n.contains("webdl") || n.contains("webrip");
    if web {
        return None;
    }
    // Hybrid remuxes are usually single-layer P8 / HDR10+ dual, not BL+EL P7.
    if n.contains("hybrid") {
        return None;
    }
    let dv = n.contains("dovi")
        || n.contains("dolby vision")
        || n.contains("dolbyvision")
        || n.contains(".dv.")
        || n.contains(".dv-")
        || n.contains("-dv-")
        || n.contains("-dv.")
        || n.contains(" hdr.dv")
        || n.contains(".hdr.dv")
        || n.contains(" hdr dv")
        || n.contains(" remux dv")
        || n.contains("bdremux dv")
        || n.contains(" bdremux.dv")
        || n.contains(" dv.hevc")
        || n.contains(".dv.hevc");
    let remux = n.contains("bdremux")
        || n.contains("bd remux")
        || n.contains("blu-ray remux")
        || n.contains("bluray remux")
        || n.contains("uhd remux")
        || n.contains("uhdremux");
    if dv && remux {
        return Some("dv-p7");
    }
    None
}

pub fn probe_toml_exists(file: &Path) -> bool {
    let named = PathBuf::from(format!("{}.probe.toml", file.display()));
    let stem = file.with_extension("probe.toml");
    named.is_file() || stem.is_file()
}

/// First codec when VIDEO/AUDIO store extra tracks as `aac,ac3`.
pub fn primary_codec(s: &str) -> &str {
    s.split(',')
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("")
}

/// `1920x800` / `1920X800` from DETAILS.RESOLUTION.
pub fn parse_resolution(s: Option<&str>) -> (u32, u32) {
    let Some(raw) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return (0, 0);
    };
    let Some((w, h)) = raw.split_once('x').or_else(|| raw.split_once('X')) else {
        return (0, 0);
    };
    (w.trim().parse().unwrap_or(0), h.trim().parse().unwrap_or(0))
}

/// DLNA PN from stored stream identity. Matroska stays empty (dialect).
pub fn dlna_pn_from_probe(
    container: &str,
    video: &str,
    audio: &str,
    _hdr: &str,
    width: u32,
    height: u32,
) -> Option<String> {
    let video = primary_codec(video);
    let audio = primary_codec(audio);
    if video.is_empty() {
        return None;
    }
    let hd = height >= 720 || width >= 1280;
    match container {
        "mkv" => None,
        "mp4" => match video {
            "h264" => Some(
                if hd {
                    if matches!(audio, "ac3" | "eac3") {
                        "AVC_MP4_MP_HD_AC3"
                    } else {
                        "AVC_MP4_MP_HD_AAC_MULT5"
                    }
                } else if matches!(audio, "ac3" | "eac3") {
                    "AVC_MP4_MP_SD_AC3"
                } else {
                    "AVC_MP4_MP_SD_AAC_MULT5"
                }
                .into(),
            ),
            "hevc" => Some(
                if hd {
                    if matches!(audio, "ac3" | "eac3") {
                        "HEVC_MP4_BL_Main10_L5_HD1080_AC3"
                    } else {
                        "HEVC_MP4_BL_Main10_L5_HD1080_AAC"
                    }
                } else {
                    "HEVC_MP4_BL_Main10_L4_HD720_AAC"
                }
                .into(),
            ),
            "mpeg4" => Some("MPEG4_P2_MP4_ASP_AAC".into()),
            "mpeg2" => Some("MPEG_PS_PAL".into()),
            _ => None,
        },
        "avi" => match video {
            "mpeg4" | "other" => Some("MPEG4_P2_AVI_ASP_L5_SO".into()),
            "h264" => Some(
                if hd {
                    "AVC_MP4_MP_HD_AAC_MULT5"
                } else {
                    "AVC_MP4_MP_SD_AAC_MULT5"
                }
                .into(),
            ),
            _ => None,
        },
        "mpeg-ts" | "ts" => match video {
            "h264" => Some(
                if hd {
                    "AVC_TS_MP_HD_AC3_ISO"
                } else {
                    "AVC_TS_MP_SD_AC3_ISO"
                }
                .into(),
            ),
            "mpeg2" | "mpeg4" => Some(
                if hd {
                    "MPEG_TS_HD_NA_ISO"
                } else {
                    "MPEG_TS_SD_NA_ISO"
                }
                .into(),
            ),
            "hevc" => Some("HEVC_TS_HD_EU_ISO".into()),
            _ => None,
        },
        _ => None,
    }
}

/// Container from DETAILS (or extension). HDR/codecs stay empty when unset —
/// a failed probe must not look like hevc/sdr. Extra tracks stay as `aac,ac3`.
pub fn probe_from_stored(
    ext: &str,
    container: Option<&str>,
    video: Option<&str>,
    audio: Option<&str>,
    audio_streams: Option<&str>,
    hdr: Option<&str>,
    resolution: Option<&str>,
) -> SourceProbe {
    let container = match container.filter(|s| !s.is_empty()) {
        Some(c) => c.to_string(),
        None => match ext {
            "mp4" | "m4v" => "mp4".into(),
            "avi" => "avi".into(),
            "ts" | "m2ts" | "mts" => "ts".into(),
            _ => "mkv".into(),
        },
    };
    let mut video = video.filter(|s| !s.is_empty()).unwrap_or("").to_string();
    if video == "other" && (container == "avi" || ext == "avi") {
        video = "mpeg4".into();
    }
    let (width, height) = parse_resolution(resolution);
    SourceProbe {
        container,
        video,
        audio: audio.filter(|s| !s.is_empty()).unwrap_or("").to_string(),
        audio_streams: audio_streams
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string(),
        hdr: hdr.filter(|s| !s.is_empty()).unwrap_or("").to_string(),
        width,
        height,
    }
}

pub fn apply_probe_to_detail(db: &LibraryDb, id: i64, got: &MediaProbe) -> ScanResult<()> {
    let mut got = got.clone();
    if got.probe.hdr.is_empty() {
        got.probe.hdr = "sdr".into();
    }
    db.update_detail_stream(
        id,
        DetailStreamUpdate {
            duration: got.av.duration.as_deref(),
            bitrate: got.av.bitrate,
            resolution: got.av.resolution.as_deref(),
            channels: got.av.channels,
            samplerate: got.av.samplerate,
            container: Some(got.probe.container.as_str()).filter(|s| !s.is_empty()),
            video: Some(got.probe.video.as_str()).filter(|s| !s.is_empty()),
            audio: Some(got.probe.audio.as_str()).filter(|s| !s.is_empty()),
            hdr: Some(got.probe.hdr.as_str()),
        },
    )?;
    db.update_detail_audio_streams(id, Some(&got.probe.audio_streams))?;
    let pn = dlna_pn_from_probe(
        &got.probe.container,
        &got.probe.video,
        &got.probe.audio,
        &got.probe.hdr,
        got.probe.width,
        got.probe.height,
    );
    db.update_detail_dlna_pn(id, pn.as_deref())?;
    if let Some(c) = got.av.creator.as_deref().filter(|s| !s.is_empty()) {
        db.update_detail_creator_if_empty(id, c)?;
    }
    db.update_detail_embedded_tags(id, &got.tags)?;
    db.mark_detail_stream_probed(id)?;
    db.copy_stream_to_inode_aliases(id)?;
    db.copy_embedded_tags_to_inode_aliases(id)?;
    Ok(())
}

/// Probe `path` and persist. Libav `None` still applies a `.probe.toml`
/// sidecar. With neither, stream columns stay unset so a later size change
/// can retry (growing MP4 with no moov yet).
fn persist_probe(db: &LibraryDb, cfg: &ScanConfig, path: &Path, id: i64) -> ScanResult<bool> {
    persist_probe_with(db, cfg, path, id, None)
}

fn persist_probe_with(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    id: i64,
    known: Option<MediaProbe>,
) -> ScanResult<bool> {
    if !path_is_allowed_file(path, cfg) {
        return Ok(false);
    }
    let got = if is_image(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""),
    ) {
        known.or_else(|| probe_image_with_timeout(path, cfg.external_command_timeout))
    } else {
        known.or_else(|| probe_media_with_timeout(path, cfg.external_command_timeout))
    };
    persist_prepared_probe(db, cfg, path, id, got)
}

/// Persist an already-attempted probe. `None` is a cached failure and must not
/// reopen the same physical file on the serial SQLite publication pass.
fn persist_prepared_probe(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    id: i64,
    got: Option<MediaProbe>,
) -> ScanResult<bool> {
    if !path_is_allowed_file(path, cfg) {
        return Ok(false);
    }
    if is_image(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""),
    ) {
        let Some(got) = got else {
            db.clear_detail_stream(id)?;
            return Ok(false);
        };
        db.update_detail_stream(
            id,
            DetailStreamUpdate {
                resolution: got.av.resolution.as_deref(),
                container: Some("jpeg"),
                ..DetailStreamUpdate::default()
            },
        )?;
        db.update_detail_embedded_tags(id, &got.tags)?;
        db.mark_detail_stream_probed(id)?;
        db.copy_stream_to_inode_aliases(id)?;
        db.copy_embedded_tags_to_inode_aliases(id)?;
        return Ok(true);
    }
    if let Some(mut got) = got {
        merge_sidecar(cfg, path, &mut got.probe)?;
        apply_probe_to_detail(db, id, &got)?;
        return Ok(true);
    }
    let mut probe = SourceProbe::default();
    merge_sidecar(cfg, path, &mut probe)?;
    if probe.hdr.is_empty() && probe.video.is_empty() && probe.audio.is_empty() {
        db.clear_detail_stream(id)?;
        return Ok(false);
    }
    apply_probe_to_detail(
        db,
        id,
        &MediaProbe {
            probe,
            av: AvMeta::default(),
            tags: EmbeddedTags::default(),
        },
    )?;
    Ok(true)
}

fn merge_sidecar(cfg: &ScanConfig, path: &Path, probe: &mut SourceProbe) -> ScanResult<()> {
    let named = PathBuf::from(format!("{}.probe.toml", path.display()));
    let stem = path.with_extension("probe.toml");
    for c in [named, stem] {
        if !path_is_allowed_file(&c, cfg) {
            continue;
        }
        let text = std::fs::read_to_string(&c).map_err(|error| scan_io(&c, error))?;
        let s = parse_probe_toml(&text);
        if !s.container.is_empty() {
            probe.container = s.container;
        }
        if !s.video.is_empty() {
            probe.video = s.video;
        }
        if !s.audio.is_empty() {
            probe.audio = s.audio;
        }
        if !s.hdr.is_empty() {
            probe.hdr = s.hdr;
        }
        if s.width > 0 {
            probe.width = s.width;
        }
        if s.height > 0 {
            probe.height = s.height;
        }
        return Ok(());
    }
    Ok(())
}

/// ffprobe codec / Dolby Vision profile. Used when no sidecar exists.
pub fn probe_stream_identity(path: &Path) -> Option<SourceProbe> {
    let mut command = std::process::Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-probesize",
            "8000000",
            "-analyzeduration",
            "4000000",
            "-show_entries",
            "stream=codec_type,codec_name,color_transfer:stream_side_data=dv_profile",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path);
    let out =
        crate::probe::command_output_with_timeout(&mut command, std::time::Duration::from_secs(30))
            .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut video = String::new();
    let mut audio = String::new();
    let mut color_transfer = String::new();
    let mut dv_profile: Option<i32> = None;
    let mut last_type = "";
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "codec_type" => last_type = v.trim(),
            "codec_name" => {
                let name = v.trim();
                if last_type == "video" && video.is_empty() {
                    video = name.to_string();
                } else if last_type == "audio" && audio.is_empty() {
                    audio = name.to_string();
                }
            }
            "color_transfer" => {
                if last_type == "video" && color_transfer.is_empty() {
                    color_transfer = v.trim().to_string();
                }
            }
            "dv_profile" if dv_profile.is_none() => dv_profile = v.trim().parse().ok(),
            _ => {}
        }
    }
    if video.is_empty() && audio.is_empty() && dv_profile.is_none() {
        return None;
    }
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("mkv");
    let mut p = SourceProbe::default();
    p.container = match ext {
        "mp4" | "m4v" => "mp4".into(),
        "avi" => "avi".into(),
        _ => "mkv".into(),
    };
    p.video = match video.as_str() {
        "h264" | "avc" => "h264".into(),
        "mpeg2video" | "mpeg2" => "mpeg2".into(),
        "" => p.video,
        other => other.to_string(),
    };
    p.audio = match audio.as_str() {
        "truehd" => "truehd".into(),
        "ac3" => "ac3".into(),
        "eac3" => "eac3".into(),
        "aac" => "aac".into(),
        "dts" | "dca" => "dts".into(),
        "" => p.audio,
        other => other.to_string(),
    };
    p.hdr = match dv_profile {
        Some(7) => "dv-p7".into(),
        Some(8) => "dv-p8".into(),
        Some(5) => "dv-p5".into(),
        Some(_) => "dv".into(),
        None if color_transfer == "smpte2084" => "hdr10".into(),
        None => "sdr".into(),
    };
    Some(p)
}

#[cfg(unix)]
fn inode_key(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn inode_key(meta: &std::fs::Metadata) -> (u64, u64) {
    (0, meta.len())
}

/// Load `{db_dir}/files.db` without walking the tree so HTTP/SSDP can
/// bind immediately. Missing or unreadable DB → virtual containers only.
pub fn load_existing(cfg: &ScanConfig) -> Catalog {
    let Some(path) = &cfg.db_path else {
        return Catalog::new();
    };
    if !path.exists() {
        return Catalog::new();
    }
    match open_library_db(path).and_then(|db| {
        let n = db.detail_count()?;
        let cat = load_catalog_with_policy(&db, cfg)?;
        Ok((n, cat))
    }) {
        Ok((n, cat)) if !cat.items.is_empty() || !cat.containers.is_empty() => {
            tracing::info!(
                target: "rusty_dlna",
                details = n,
                path = %path.display(),
                "library loaded"
            );
            cat
        }
        Ok(_) => Catalog::new(),
        Err(e) => {
            tracing::warn!(
                target: "rusty_dlna",
                path = %path.display(),
                error = %e,
                "library load failed"
            );
            Catalog::new()
        }
    }
}

pub fn load_catalog_with_policy(db: &LibraryDb, cfg: &ScanConfig) -> ScanResult<Catalog> {
    let mut catalog = db.load_catalog()?;
    catalog.configure_recent_policy(cfg.recent_limit, cfg.recent_days);
    Ok(catalog)
}

pub fn scan(cfg: &ScanConfig) -> ScanResult<Catalog> {
    scan_inner(cfg, true)
}

/// Background refresh: do not wipe OBJECTS; skip files whose SIZE+TIMESTAMP
/// are unchanged; never descend into `exclude_dir` (e.g. `incomplete`).
pub fn scan_refresh(cfg: &ScanConfig) -> ScanResult<Catalog> {
    scan_inner(cfg, false)
}

/// One-time repair for catalogs created while embedded stream/container titles
/// could overwrite a video's filename. Video release tags are frequently an
/// encoder, uploader, or audio-track label rather than a human-facing movie
/// name, so normalize every video to filename/NFO. The persisted policy
/// revision makes later starts O(1).
pub fn repair_video_titles_if_needed(cfg: &ScanConfig) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    const POLICY_KEY: &str = "video_title_policy_rev";
    const POLICY_REV: &str = "3";

    let _write = library_write_guard();
    let db = match &cfg.db_path {
        Some(path) => open_library_db(path)?,
        None => return Ok((None, ScanDelta::default())),
    };
    if db.setting(POLICY_KEY)?.as_deref() == Some(POLICY_REV) {
        return Ok((None, ScanDelta::default()));
    }
    let transaction = db.transaction()?;
    let mut changed = 0usize;
    let mut desired_by_physical: HashMap<(i64, i64, String), String> = HashMap::new();
    for row in db.video_detail_titles()? {
        let db::VideoDetailTitle {
            id,
            path: stored,
            title: current,
            device,
            inode,
        } = row;
        let stored_path = path_from_db(&stored);
        let filename_title = stored_path
            .file_stem()
            .map(display_os_name)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| current.clone());
        let key = (device, inode, filename_title.clone());
        let wanted = if let Some(wanted) = desired_by_physical.get(&key) {
            wanted.clone()
        } else {
            let live = rebase_media_path_for_config(&stored_path, cfg);
            let nfo_title = if path_is_allowed_file(&live, cfg) {
                nfo_for_file_with_policy_result(&live, &cfg.media_dirs, cfg.wide_links)?.title
            } else {
                None
            };
            let wanted = nfo_title.unwrap_or(filename_title);
            desired_by_physical.insert(key, wanted.clone());
            wanted
        };
        let mut item_changed = false;
        if current != wanted {
            db.update_detail_title(id, &wanted)?;
            item_changed = true;
        }
        let fields = db.detail_group_fields(id)?;
        let series_title = episode_display_title(&wanted, fields.album.as_deref());
        item_changed |= db.update_detail_names_under_root(id, VIDEO_SERIES_ID, &series_title)? > 0;
        item_changed |= db.update_detail_names_under_root(id, VIDEO_GENRE_ID, &wanted)? > 0;
        item_changed |= db.update_detail_names_under_root(id, VIDEO_ACTOR_ID, &wanted)? > 0;
        changed += usize::from(item_changed);
    }
    db.set_setting(POLICY_KEY, POLICY_REV)?;
    transaction.commit()?;
    let delta = ScanDelta {
        changed,
        ..ScanDelta::default()
    };
    if changed == 0 {
        return Ok((None, delta));
    }
    tracing::info!(
        target: "rusty_dlna",
        changed,
        "repaired video titles from filenames/NFO"
    );
    Ok((Some(load_catalog_with_policy(&db, cfg)?), delta))
}

fn scan_inner(cfg: &ScanConfig, rebuild: bool) -> ScanResult<Catalog> {
    if let Some(progress) = &cfg.progress {
        progress.reset();
    }
    let db = match &cfg.db_path {
        Some(p) => open_library_db(p)?,
        None => LibraryDb::open_memory()?,
    };
    let transaction = db.transaction()?;
    if rebuild {
        db.clear_objects()?;
    }
    db.seed_virtual_containers()?;
    {
        let mut walker = DbWalker {
            db: &db,
            cfg,
            walk_stack: HashMap::new(),
            rebuild,
            indexed: 0,
            pending: Vec::new(),
        };
        for root in &cfg.media_dirs {
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            let title = cfg
                .root_title_for_path(&root)
                .unwrap_or("media")
                .to_string();
            walker.walk(&root, BROWSEDIR_ID, &title)?;
        }
        walker.index_pending()?;
    }
    db.prune_missing_files()?;
    db.prune_excluded_paths(cfg)?;
    playlist::sync_playlists(&db, cfg)?;
    db.prune_empty_folders()?;
    let expired_bookmarks =
        db.prune_expired_bookmarks(cfg.bookmark_retention_days, unix_now_seconds())?;
    if expired_bookmarks > 0 {
        tracing::info!(
            target: "rusty_dlna",
            removed = expired_bookmarks,
            retention_days = cfg.bookmark_retention_days,
            "expired playback state pruned"
        );
    }
    if rebuild {
        db.set_setting("video_title_policy_rev", "3")?;
    }
    transaction.commit()?;
    let n = db.detail_count()?;
    tracing::info!(
        target: "rusty_dlna",
        details = n,
        path = %db.path.display(),
        "scan complete"
    );
    load_catalog_with_policy(&db, cfg)
}

pub(crate) fn library_write_guard() -> std::sync::MutexGuard<'static, ()> {
    static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
    M.lock().unwrap_or_else(|e| e.into_inner())
}

fn unix_now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// List files on disk (skip `exclude_dir` / junk), compare to DETAILS, apply
/// only adds/changes/deletes. Does not wipe OBJECTS or rewrite unchanged rows.
pub fn monitor(cfg: &ScanConfig) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    monitor_dirty(cfg, &[])
}

fn log_library_file(path: impl AsRef<Path>, action: &'static str, message: &'static str) {
    let path = path.as_ref();
    let file = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    tracing::info!(
        target: "rusty_dlna",
        file,
        action,
        path = %path.display(),
        "{message}"
    );
}

/// Like [`monitor`], but restat `dirty` paths (inotify CLOSE_WRITE / move
/// targets) so SIZE/TIMESTAMP stay current without stating the whole tree.
pub fn monitor_dirty(
    cfg: &ScanConfig,
    dirty: &[PathBuf],
) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    let started = std::time::Instant::now();
    let restat_all = dirty.is_empty();
    if let Some(progress) = &cfg.progress {
        progress.reset();
    }
    if restat_all {
        tracing::info!(target: "rusty_dlna", "library reconciliation started");
    }
    let _write = library_write_guard();
    let db = match &cfg.db_path {
        Some(p) => open_library_db(p)?,
        None => LibraryDb::open_memory()?,
    };
    let listed = list_media_files(cfg)?;
    let db_rows = db.all_detail_stats()?;
    if restat_all {
        tracing::info!(
            target: "rusty_dlna",
            files = listed.len(),
            details = db_rows.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "library walk complete"
        );
    }
    let mut listed_by_rel: HashMap<String, &ListedFile> = HashMap::new();
    for st in listed.values() {
        listed_by_rel.insert(media_rel_key_for_config(&st.path, cfg), st);
    }
    let dirty_rels: HashSet<String> = dirty
        .iter()
        .map(|p| media_rel_key_for_config(p, cfg))
        .collect();
    let transaction = db.transaction()?;
    db.seed_virtual_containers()?;
    // Periodic `monitor()` (empty dirty) restats every listed row so a
    // replaced file (new inode / new size) cannot stay frozen. Inotify
    // passes only restat dirty paths. Missing optional stream fields are not
    // evidence that an unchanged file needs another libav pass: some damaged
    // or unusual streams legitimately leave fields empty. A failed probe is
    // retried when the file's size, timestamp, device, or inode changes.
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut changed = 0usize;
    for row in &db_rows {
        let decoded = path_from_db(&row.path);
        let key = media_rel_key_for_config(&decoded, cfg);
        if (path_is_unwanted(&decoded, cfg) || !listed_by_rel.contains_key(&key))
            && db.remove_path_and_symlink_aliases(&row.path)? > 0
        {
            log_library_file(&decoded, "removed", "library file removed");
            removed += 1;
        }
    }
    // Removal can delete several inode aliases at once. Rebuild the lookup
    // from live rows before matching additions; stale IDs would otherwise be
    // updated or queried later in the same transaction.
    let live_db_rows = db.all_detail_stats()?;
    let mut in_db_by_rel: HashMap<String, Vec<DetailStat>> = HashMap::new();
    for row in &live_db_rows {
        in_db_by_rel
            .entry(media_rel_key_for_config(&path_from_db(&row.path), cfg))
            .or_default()
            .push(row.clone());
    }
    // Same on-disk file stored under the host realpath and the container
    // mount is one library item. Keep the live listed path, drop extras.
    for (key, rows) in &in_db_by_rel {
        if rows.len() < 2 || !listed_by_rel.contains_key(key) {
            continue;
        }
        let listed_path = path_to_db(&listed_by_rel[key].path);
        let keep = rows
            .iter()
            .find(|row| row.path == listed_path)
            .or_else(|| {
                rows.iter()
                    .find(|row| path_is_live_file(&path_from_db(&row.path)))
            })
            .map(|row| row.path.clone());
        let Some(keep) = keep else {
            continue;
        };
        for row in rows {
            if row.path == keep {
                continue;
            }
            db.remove_detail_id(row.id)?;
            log_library_file(
                path_from_db(&row.path),
                "removed",
                "library duplicate path dropped",
            );
            removed += 1;
        }
    }
    for (path_s, st) in &listed {
        let key = media_rel_key_for_config(&st.path, cfg);
        let existing = in_db_by_rel.get(&key).and_then(|rows| {
            rows.iter()
                .find(|row| &row.path == path_s)
                .or_else(|| rows.first())
        });
        match existing {
            Some(row) => {
                let mut live_dev = row.device;
                let mut live_ino = row.inode;
                if restat_all || dirty_rels.contains(&key) {
                    let meta = match std::fs::metadata(&st.path) {
                        Ok(meta) if meta.is_file() => Some(meta),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(scan_io(&st.path, error)),
                        Ok(_) => None,
                    };
                    if let Some(meta) = meta {
                        let size = meta.len() as i64;
                        let mtime = file_mtime_unix(&meta);
                        let (new_dev, new_ino) = inode_key(&meta);
                        live_dev = new_dev as i64;
                        live_ino = new_ino as i64;
                        let grew = size != row.size
                            || mtime != row.timestamp
                            || live_dev != row.device
                            || live_ino != row.inode;
                        if grew {
                            if grew
                                && !file_is_viable_with_timeout(
                                    &st.path,
                                    cfg.external_command_timeout,
                                )
                            {
                                db.remove_path_and_symlink_aliases(&row.path)?;
                                log_library_file(&st.path, "removed", "library file removed");
                                removed += 1;
                                continue;
                            }
                            if grew {
                                tracing::info!(
                                    target: "rusty_dlna",
                                    file = st.path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                                    action = "updated",
                                    path = %st.path.display(),
                                    old_size = row.size,
                                    size,
                                    old_inode = row.inode,
                                    inode = live_ino,
                                    "file updated"
                                );
                                db.update_detail_stat(row.id, size, mtime, live_dev, live_ino)?;
                                let title = st
                                    .path
                                    .file_stem()
                                    .and_then(|value| value.to_str())
                                    .unwrap_or("item");
                                db.reset_detail_tags_to_file_defaults(
                                    row.id,
                                    title,
                                    &file_mtime_date(&st.path),
                                )?;
                            }
                            let probed = apply_or_reuse_probe(
                                &db, cfg, &st.path, row.id, live_dev, live_ino,
                            )?;
                            if grew {
                                apply_nfo(&db, cfg, &st.path, row.id)?;
                                refresh_replaced_inode_aliases(
                                    &db,
                                    InodeReplacement {
                                        old_device: row.device,
                                        old_inode: row.inode,
                                        source_id: row.id,
                                        new_device: live_dev,
                                        new_inode: live_ino,
                                        size,
                                        timestamp: mtime,
                                    },
                                )?;
                            }
                            if probed || grew {
                                changed += 1;
                            }
                        }
                    }
                }
                if attach_listed_if_missing(&db, cfg, &st.path, row.id, live_dev, live_ino)? {
                    changed += 1;
                }
            }
            None => {
                // Rel-key miss is not "new": the row may already exist at
                // this exact path (genre aliases) or under another prefix.
                if db.find_detail_by_path(path_s)?.is_some() {
                    continue;
                }
                if let Some(folder_id) = ensure_folder_chain(&db, cfg, &st.path)? {
                    if index_one_file(&db, cfg, &st.path, &folder_id)? {
                        log_library_file(&st.path, "added", "library file added");
                        added += 1;
                    }
                }
            }
        }
    }
    let mut sidecar_changed = false;
    if restat_all {
        tracing::info!(
            target: "rusty_dlna",
            files = listed.len(),
            "library paths checked; reconciling metadata and sidecars"
        );
        let nfo_started = std::time::Instant::now();
        sidecar_changed |= refresh_nfo_periodic(&db, cfg, &db.all_detail_stats()?)?;
        tracing::info!(
            target: "rusty_dlna",
            elapsed_ms = nfo_started.elapsed().as_millis(),
            "library NFO reconciliation complete"
        );
        let mut parents = HashSet::new();
        for st in listed.values() {
            if let Some(p) = st.path.parent() {
                parents.insert(p.to_path_buf());
            }
        }
        let artwork_started = std::time::Instant::now();
        for dir in &parents {
            sidecar_changed |= attach_album_art_in_dir(&db, cfg, dir)?;
        }
        tracing::info!(
            target: "rusty_dlna",
            directories = parents.len(),
            elapsed_ms = artwork_started.elapsed().as_millis(),
            "library artwork reconciliation complete"
        );
        let captions_started = std::time::Instant::now();
        for dir in &parents {
            sidecar_changed |= refresh_captions_in_dir(&db, cfg, dir)?;
        }
        tracing::info!(
            target: "rusty_dlna",
            directories = parents.len(),
            elapsed_ms = captions_started.elapsed().as_millis(),
            "library caption reconciliation complete"
        );
    }
    let mut nfo_dirs: HashMap<PathBuf, bool> = HashMap::new();
    for d in dirty {
        let name = d.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let Some(dir) = d.parent() else {
            continue;
        };
        if is_album_art_name_for_config(name, cfg) {
            sidecar_changed |= refresh_artwork_event(&db, cfg, d)?;
            continue;
        }
        if is_caption_name(name) {
            sidecar_changed |= refresh_caption_event(&db, cfg, d)?;
            continue;
        }
        if ends_with_ci(name, ".nfo") {
            let recursive = name.eq_ignore_ascii_case("tvshow.nfo");
            nfo_dirs
                .entry(dir.to_path_buf())
                .and_modify(|current| *current |= recursive)
                .or_insert(recursive);
        }
    }
    for (dir, recursive) in nfo_dirs {
        sidecar_changed |= apply_nfo_in_dir(&db, cfg, &dir, recursive)?;
    }
    changed += usize::from(sidecar_changed);
    if playlist::sync_playlists(&db, cfg)? {
        changed += 1;
    }
    if restat_all {
        let expired_bookmarks =
            db.prune_expired_bookmarks(cfg.bookmark_retention_days, unix_now_seconds())?;
        if expired_bookmarks > 0 {
            tracing::info!(
                target: "rusty_dlna",
                removed = expired_bookmarks,
                retention_days = cfg.bookmark_retention_days,
                "expired playback state pruned"
            );
            changed += expired_bookmarks;
        }
    }
    removed += db.prune_empty_folders()?;
    let stale_art = db.prune_unreferenced_album_art()?;
    let delta = ScanDelta {
        added,
        removed,
        changed,
    };
    if added + removed + changed == 0 {
        transaction.commit()?;
        remove_stale_cached_art(cfg, &stale_art);
        if restat_all {
            tracing::info!(
                target: "rusty_dlna",
                files = listed.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "library reconciliation complete; no changes"
            );
        }
        return Ok((None, delta));
    }
    transaction.commit()?;
    remove_stale_cached_art(cfg, &stale_art);
    let n = db.detail_count()?;
    tracing::info!(
        target: "rusty_dlna",
        added,
        removed,
        changed,
        details = n,
        elapsed_ms = started.elapsed().as_millis(),
        path = %db.path.display(),
        "library reconciliation complete"
    );
    Ok((Some(load_catalog_with_policy(&db, cfg)?), delta))
}

#[derive(Clone)]
struct ListedFile {
    path: PathBuf,
}

fn apply_or_reuse_probe(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    id: i64,
    device: i64,
    inode: i64,
) -> ScanResult<bool> {
    if let Some(src) = db.find_inode_probe_source(device, inode, id)? {
        db.copy_stream_from(src, id)?;
        return Ok(true);
    }
    persist_probe(db, cfg, path, id)
}

fn apply_or_reuse_prepared_probe(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    id: i64,
    device: i64,
    inode: i64,
    prepared: Option<&PreparedPhysicalFile>,
) -> ScanResult<bool> {
    // A preparation was requested only when every persisted alias was stale.
    // Do not copy one of those old rows over the freshly obtained probe.
    if let Some(prepared) = prepared.filter(|prepared| prepared.probe_attempted) {
        return persist_prepared_probe(db, cfg, path, id, prepared.probe.clone());
    }
    if let Some(src) = db.find_inode_probe_source(device, inode, id)? {
        db.copy_stream_from(src, id)?;
        return Ok(true);
    }
    persist_probe(db, cfg, path, id)
}

/// When a path is replaced (new inode), sibling DETAILS rows that still
/// name the old inode are restatted. Those that now resolve to the new
/// file get the new SIZE/INODE and a copied probe — no second libav.
#[derive(Clone, Copy, Debug)]
struct InodeReplacement {
    old_device: i64,
    old_inode: i64,
    source_id: i64,
    new_device: i64,
    new_inode: i64,
    size: i64,
    timestamp: i64,
}

fn refresh_replaced_inode_aliases(db: &LibraryDb, replacement: InodeReplacement) -> ScanResult<()> {
    let InodeReplacement {
        old_device,
        old_inode,
        source_id,
        new_device,
        new_inode,
        size,
        timestamp,
    } = replacement;
    if old_inode == 0 || (old_device == new_device && old_inode == new_inode) {
        return Ok(());
    }
    let siblings = db.details_with_inode(old_device, old_inode)?;
    for (sid, spath) in siblings {
        if sid == source_id {
            continue;
        }
        let decoded = path_from_db(&spath);
        let Ok(meta) = std::fs::metadata(&decoded) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let (d, i) = inode_key(&meta);
        if d as i64 != new_device || i as i64 != new_inode {
            continue;
        }
        db.update_detail_stat(sid, size, timestamp, new_device, new_inode)?;
        db.copy_stream_from(source_id, sid)?;
        tracing::info!(
            target: "rusty_dlna",
            file = decoded.file_name().map(display_os_name).unwrap_or_default(),
            action = "updated",
            path = %decoded.display(),
            old_inode,
            inode = new_inode,
            size,
            "library alias updated"
        );
    }
    Ok(())
}

fn attach_listed_if_missing(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    detail_id: i64,
    device: i64,
    inode: i64,
) -> ScanResult<bool> {
    let Some(folder_id) = ensure_folder_chain(db, cfg, path)? else {
        return Ok(false);
    };
    let title = path.file_stem().map(display_os_name).unwrap_or_default();
    if title.is_empty() {
        return Ok(false);
    }
    if db.folder_has_inode_named(&folder_id, device, inode, &title)? {
        return Ok(false);
    }
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let (_, class, _) = mime_and_class(&name);
    attach_objects(
        db,
        &folder_id,
        detail_id,
        &title,
        class,
        device as u64,
        inode as u64,
    )?;
    Ok(true)
}

fn list_media_files(cfg: &ScanConfig) -> ScanResult<HashMap<String, ListedFile>> {
    let mut out = HashMap::new();
    let mut walk_stack: HashMap<(u64, u64), ()> = HashMap::new();
    for root in &cfg.media_dirs {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let title = cfg
            .root_title_for_path(&root)
            .unwrap_or("media")
            .to_string();
        list_into(&mut out, cfg, &mut walk_stack, &root, &title)?;
    }
    Ok(out)
}

fn list_into(
    out: &mut HashMap<String, ListedFile>,
    cfg: &ScanConfig,
    walk_stack: &mut HashMap<(u64, u64), ()>,
    dir: &Path,
    title: &str,
) -> ScanResult<()> {
    let Ok(metadata) = std::fs::metadata(dir) else {
        return Ok(());
    };
    if !metadata.is_dir() {
        return Ok(());
    }
    let dir_key = inode_key(&metadata);
    if walk_stack.contains_key(&dir_key) {
        return Ok(());
    }
    if !path_is_allowed_dir(dir, cfg) || path_excluded(dir, title, cfg) {
        return Ok(());
    }
    walk_stack.insert(dir_key, ());
    let rd = std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))?;
    for ent in rd {
        let ent = ent.map_err(|error| scan_io(dir, error))?;
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        if !cfg.include_hidden && name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = ent.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
        if is_dir {
            if is_skipped_dir(&name) || path_excluded(&path, &name, cfg) {
                continue;
            }
            list_into(out, cfg, walk_stack, &path, &name)?;
            continue;
        }
        if is_unfinished_name(&name)
            || looks_like_sample_file(&name)
            || is_caption_name(&name)
            || ends_with_ci(&name, ".nfo")
            || name.ends_with(".probe.toml")
            || path_excluded(&path, &name, cfg)
            || is_album_art_name_for_config(&name, cfg)
            || !cfg
                .root_types_for_path(&path)
                .is_some_and(|types| types.allows(&name))
            || (!file_type.is_file() && !file_type.is_symlink())
            || (file_type.is_symlink() && !path_is_allowed_file(&path, cfg))
        {
            continue;
        }
        // Existence only. Stat the whole tree on every reconcile was
        // tens of thousands of HDD/NAS syscalls and stalled Browse.
        let path_s = path_to_db(&path);
        if let Some(progress) = &cfg.progress {
            progress.record(&path);
        }
        out.insert(path_s, ListedFile { path });
        if out.len().is_multiple_of(5_000) {
            tracing::info!(
                target: "rusty_dlna",
                files = out.len(),
                "library walk progress"
            );
        }
    }
    walk_stack.remove(&dir_key);
    Ok(())
}

pub(crate) fn ensure_folder_chain(
    db: &LibraryDb,
    cfg: &ScanConfig,
    file: &Path,
) -> ScanResult<Option<String>> {
    let Some(parent) = file.parent() else {
        return Ok(None);
    };
    if cfg.media_dirs.is_empty() {
        return Ok(None);
    }
    // Walked path only. Do not canonicalize — that collapses a directory
    // symlink (genres/BY_YEAR/2010/Movies/Despicable Me → kids/Movies/…)
    // into the real folder and every alias lands in the same Browse list.
    let Some(selected) = cfg.selected_root(parent) else {
        return Ok(None);
    };
    let Ok(rel) = parent.strip_prefix(selected.relative_to) else {
        return Ok(None);
    };
    let root_title = selected.title;
    if path_excluded(parent, root_title, cfg) {
        return Ok(None);
    }
    let mut folder_id = folder_object_id(db, BROWSEDIR_ID, root_title)?;
    for comp in rel.components() {
        let name = display_os_name(comp.as_os_str());
        if name.is_empty() || name == "." {
            continue;
        }
        if is_skipped_dir(&name) || path_excluded(parent, &name, cfg) {
            return Ok(None);
        }
        folder_id = folder_object_id(db, &folder_id, &name)?;
    }
    Ok(Some(folder_id))
}

pub(crate) fn index_one_file(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    folder_id: &str,
) -> ScanResult<bool> {
    index_one_file_with_prepared(db, cfg, path, folder_id, None)
}

fn index_one_file_with_prepared(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    folder_id: &str,
    prepared: Option<&PreparedPhysicalFile>,
) -> ScanResult<bool> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty()
        || !cfg
            .root_types_for_path(path)
            .is_some_and(|types| types.allows(&name))
        || !path_is_allowed_file(path, cfg)
    {
        return Ok(false);
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Err(error) => return Err(scan_io(path, error)),
        Ok(_) => return Ok(false),
    };
    let (dev, ino) = inode_key(&meta);
    let inode_source = db.find_inode_source(dev as i64, ino as i64)?;
    let eager_probe = (prepared.is_none() && inode_source.is_none())
        .then(|| {
            media_format_for_name(&name)
                .filter(|format| format.is_ambiguous())
                .and_then(|_| probe_media_with_timeout(path, cfg.external_command_timeout))
        })
        .flatten();
    let format_probe = prepared
        .and_then(|prepared| prepared.probe.as_ref())
        .or(eager_probe.as_ref());
    let Some(format) = resolved_media_format_with_hint(
        &name,
        format_probe,
        prepared
            .and_then(|prepared| prepared.mime_hint.as_deref())
            .or_else(|| inode_source.as_ref().map(|source| source.mime.as_str())),
    ) else {
        return Ok(false);
    };
    let mime = format.mime;
    let class = format.upnp_class();
    let mtime = file_mtime_unix(&meta);
    let size = meta.len() as i64;
    let path_s = path_to_db(path);
    let title = path
        .file_stem()
        .map(display_os_name)
        .unwrap_or_else(|| name.clone());
    let nfo = nfo_for_file_with_policy_result(path, &cfg.media_dirs, cfg.wide_links)?;
    let display_title = nfo.title.as_deref().unwrap_or(&title);

    if let Some(existing) = db.find_detail_by_path(&path_s)? {
        let ExistingDetail {
            id,
            size: old_sz,
            timestamp: old_ts,
            device: old_dev,
            inode: old_ino,
        } = existing;
        if old_sz != size || old_ts != mtime || old_dev != dev as i64 || old_ino != ino as i64 {
            tracing::info!(
                target: "rusty_dlna",
                file = name.as_str(),
                action = "updated",
                path = %path.display(),
                old_size = old_sz,
                size,
                old_inode = old_ino,
                inode = ino,
                "file updated"
            );
            db.update_detail_stat(id, size, mtime, dev as i64, ino as i64)?;
            db.reset_detail_tags_to_file_defaults(id, &title, &file_mtime_date(path))?;
            apply_or_reuse_prepared_probe(db, cfg, path, id, dev as i64, ino as i64, prepared)?;
            refresh_replaced_inode_aliases(
                db,
                InodeReplacement {
                    old_device: old_dev,
                    old_inode: old_ino,
                    source_id: id,
                    new_device: dev as i64,
                    new_inode: ino as i64,
                    size,
                    timestamp: mtime,
                },
            )?;
        }
        apply_nfo_to_detail(db, id, &nfo)?;
        attach_objects(db, folder_id, id, &title, class, dev, ino)?;
        attach_album_art_for_index(db, cfg, path, id, prepared)?;
        return Ok(true);
    }

    // rustyDLNA clone_detail_for_path: symlink/hardlink of a known inode
    // reuses TITLE/DATE/MIME/DLNA_PN — no GetVideoMetadata / ffprobe.
    if let Some(source) = inode_source {
        let src_path_decoded = path_from_db(&source.path);
        let src_key = media_rel_key_for_config(&src_path_decoded, cfg);
        let new_key = media_rel_key_for_config(path, cfg);
        // Host realpath vs container mount of the same file — not a new alias.
        if !src_key.is_empty() && src_key == new_key {
            if let Some(existing) = db.find_detail_by_path(&source.path)? {
                let ExistingDetail {
                    size: old_sz,
                    timestamp: old_ts,
                    device: old_dev,
                    inode: old_ino,
                    ..
                } = existing;
                if old_sz != size
                    || old_ts != mtime
                    || old_dev != dev as i64
                    || old_ino != ino as i64
                {
                    db.update_detail_stat(source.id, size, mtime, dev as i64, ino as i64)?;
                    apply_or_reuse_prepared_probe(
                        db, cfg, path, source.id, dev as i64, ino as i64, prepared,
                    )?;
                }
            }
            attach_objects(db, folder_id, source.id, &title, class, dev, ino)?;
            attach_album_art_for_index(db, cfg, path, source.id, prepared)?;
            return Ok(true);
        }
        if source.size == size && source.timestamp >= mtime {
            let id =
                db.clone_detail_for_path(source.id, &path_s, size, mtime, dev as i64, ino as i64)?;
            attach_objects(db, folder_id, id, &title, class, dev, ino)?;
            attach_album_art_for_index(db, cfg, path, id, prepared)?;
            return Ok(true);
        }
    }

    if format_probe.is_none() && !file_is_viable_with_timeout(path, cfg.external_command_timeout) {
        return Ok(false);
    }
    let date = nfo.date.clone().unwrap_or_else(|| file_mtime_date(path));
    let detail = db.insert_detail(NewDetail {
        path: &path_s,
        size,
        timestamp: mtime,
        title: display_title,
        date: &date,
        mime,
        device: dev as i64,
        inode: ino as i64,
        dlna_pn: None,
    })?;
    let caps = captions_for(path, cfg)?;
    db.replace_captions(detail, &caps)?;
    if let Some(prepared) = prepared.filter(|prepared| prepared.probe_attempted) {
        persist_prepared_probe(db, cfg, path, detail, prepared.probe.clone())?;
    } else {
        persist_probe_with(db, cfg, path, detail, eager_probe)?;
    }
    // Explicit sidecars override embedded tags. Embedded audio/image tags
    // override filename defaults, but video titles deliberately remain the
    // filename stem unless an NFO supplies a curated title.
    apply_nfo_to_detail(db, detail, &nfo)?;
    attach_objects(db, folder_id, detail, &title, class, dev, ino)?;
    attach_album_art_for_index(db, cfg, path, detail, prepared)?;
    Ok(true)
}

fn allocate_child_id(db: &LibraryDb, parent_id: &str) -> ScanResult<String> {
    let mut n = db.next_child_seq(parent_id)?;
    loop {
        let id = format!("{parent_id}${n:X}");
        if !db.object_exists(&id)? {
            return Ok(id);
        }
        n += 1;
    }
}

fn attach_objects(
    db: &LibraryDb,
    folder_id: &str,
    detail: i64,
    title: &str,
    class: &str,
    dev: u64,
    ino: u64,
) -> ScanResult<()> {
    if db.folder_has_inode_named(folder_id, dev as i64, ino as i64, title)? {
        let browse = db
            .browse_object_for_detail(detail)?
            .unwrap_or_else(|| format!("{folder_id}$0"));
        if class.contains("video") {
            attach_video_virtuals(db, detail, class, &browse)?;
        } else if class.contains("audio") {
            attach_audio_virtuals(db, detail, class, &browse)?;
        } else if class.contains("image") {
            attach_image_virtuals(db, detail, class, &browse)?;
        }
        return Ok(());
    }
    let object_id = match db.find_child_object(folder_id, title)? {
        Some(oid) => match db.object_detail_id(&oid)? {
            None => oid,
            Some(did) if did == detail => oid,
            // Same title, different file — never steal the other item's row.
            Some(_) => allocate_child_id(db, folder_id)?,
        },
        None => allocate_child_id(db, folder_id)?,
    };
    db.upsert_object(&object_id, folder_id, class, Some(detail), title, None)?;
    if class.contains("video") {
        if !db.all_video_has_inode(dev as i64, ino as i64)? {
            let vid = format!("{VIDEO_ALL_ID}${detail:X}");
            db.upsert_object(
                &vid,
                VIDEO_ALL_ID,
                class,
                Some(detail),
                title,
                Some(&object_id),
            )?;
        }
        ensure_typed_dir_chain(db, folder_id, VIDEO_DIR_ID)?;
        let vdir = browse_to_typed_dir(folder_id, VIDEO_DIR_ID);
        let vobj = browse_to_typed_dir(&object_id, VIDEO_DIR_ID);
        db.upsert_object(&vobj, &vdir, class, Some(detail), title, Some(&object_id))?;
        attach_video_virtuals(db, detail, class, &object_id)?;
    } else if class.contains("audio") {
        let aid = format!("{MUSIC_ALL_ID}${detail:X}");
        db.upsert_object(
            &aid,
            MUSIC_ALL_ID,
            class,
            Some(detail),
            title,
            Some(&object_id),
        )?;
        ensure_typed_dir_chain(db, folder_id, MUSIC_DIR_ID)?;
        let mdir = browse_to_typed_dir(folder_id, MUSIC_DIR_ID);
        let mobj = browse_to_typed_dir(&object_id, MUSIC_DIR_ID);
        db.upsert_object(&mobj, &mdir, class, Some(detail), title, Some(&object_id))?;
        attach_audio_virtuals(db, detail, class, &object_id)?;
    } else if class.contains("image") {
        let iid = format!("{IMAGE_ALL_ID}${detail:X}");
        db.upsert_object(
            &iid,
            IMAGE_ALL_ID,
            class,
            Some(detail),
            title,
            Some(&object_id),
        )?;
        ensure_typed_dir_chain(db, folder_id, IMAGE_DIR_ID)?;
        let idir = browse_to_typed_dir(folder_id, IMAGE_DIR_ID);
        let iobj = browse_to_typed_dir(&object_id, IMAGE_DIR_ID);
        db.upsert_object(&iobj, &idir, class, Some(detail), title, Some(&object_id))?;
        attach_image_virtuals(db, detail, class, &object_id)?;
    }
    Ok(())
}

/// Rebuild OBJECTS from live DETAILS paths without re-probing files.
/// Fixes folder-id reuse that left children under the wrong title
/// (e.g. a new show folder inheriting a deleted sibling's items).
pub fn rebuild_objects(cfg: &ScanConfig) -> ScanResult<Catalog> {
    let db = match &cfg.db_path {
        Some(p) => open_library_db(p)?,
        None => return Ok(Catalog::new()),
    };
    let transaction = db.transaction()?;
    db.prune_missing_files()?;
    db.prune_excluded_paths(cfg)?;
    let rows = db.all_detail_stats()?;
    let saved = db.snapshot_objects()?;
    let live_details: HashSet<i64> = rows.iter().map(|row| row.id).collect();
    db.clear_objects()?;
    db.seed_virtual_containers()?;
    // Put the old IDs back first so Infuse cached ObjectIDs still Browse.
    // Folders before items so find_child_object(parent, name) hits.
    let (folders, items): (Vec<_>, Vec<_>) = saved.into_iter().partition(|r| r.detail_id.is_none());
    for row in folders {
        db.restore_object(&row)?;
    }
    for row in items {
        if row.detail_id.is_some_and(|d| live_details.contains(&d)) {
            db.restore_object(&row)?;
        }
    }
    db.prune_duplicate_folder_inodes()?;
    let mut n = 0usize;
    for row in &rows {
        let p = path_from_db(&row.path);
        if !path_is_live_file(&p) || path_is_unwanted(&p, cfg) {
            continue;
        }
        if let Some(folder) = ensure_folder_chain(&db, cfg, &p)? {
            if index_one_file(&db, cfg, &p, &folder)? {
                n += 1;
            }
        }
    }
    playlist::sync_playlists(&db, cfg)?;
    db.prune_empty_folders()?;
    transaction.commit()?;
    tracing::info!(target: "rusty_dlna", files = n, "objects rebuilt");
    load_catalog_with_policy(&db, cfg)
}

/// Re-walk the tree into SQLite, drop missing/dangling paths (and their
/// symlink aliases), return a fresh catalog + delta vs `prev`.
pub fn rescan(cfg: &ScanConfig, prev: &Catalog) -> ScanResult<(Catalog, ScanDelta)> {
    let (next, delta) = monitor(cfg)?;
    Ok((next.unwrap_or_else(|| prev.clone()), delta))
}

fn browse_to_typed_dir(browse_id: &str, typed: &str) -> String {
    if browse_id == BROWSEDIR_ID {
        typed.to_string()
    } else if let Some(rest) = browse_id.strip_prefix(BROWSEDIR_ID) {
        format!("{typed}{rest}")
    } else {
        browse_id.to_string()
    }
}

fn parent_object_id(id: &str) -> Option<&str> {
    id.rfind('$').map(|i| &id[..i])
}

fn ensure_typed_dir_chain(db: &LibraryDb, browse_folder_id: &str, typed: &str) -> ScanResult<()> {
    let mut chain = Vec::new();
    let mut cur = browse_folder_id.to_string();
    while cur != BROWSEDIR_ID {
        chain.push(cur.clone());
        match parent_object_id(&cur) {
            Some(p) => cur = p.to_string(),
            None => break,
        }
    }
    chain.reverse();
    for browse in chain {
        let Some(parent_browse) = parent_object_id(&browse) else {
            continue;
        };
        let typed_id = browse_to_typed_dir(&browse, typed);
        let typed_parent = browse_to_typed_dir(parent_browse, typed);
        let name = db.object_name(&browse)?.unwrap_or_else(|| browse.clone());
        db.upsert_object(
            &typed_id,
            &typed_parent,
            "container.storageFolder",
            None,
            &name,
            Some(&browse),
        )?;
    }
    Ok(())
}

fn folder_object_id(db: &LibraryDb, parent_id: &str, title: &str) -> ScanResult<String> {
    if let Some(id) = db.find_child_object(parent_id, title)? {
        return Ok(id);
    }
    let id = allocate_child_id(db, parent_id)?;
    db.upsert_object(&id, parent_id, "container.storageFolder", None, title, None)?;
    Ok(id)
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct PhysicalFileKey {
    device: i64,
    inode: i64,
    size: i64,
    timestamp: i64,
    /// Only populated on platforms/filesystems without a useful inode.
    fallback_path: Option<PathBuf>,
}

impl PhysicalFileKey {
    fn new(path: &Path, meta: &std::fs::Metadata) -> Self {
        let (device, inode) = inode_key(meta);
        let fallback_path = (inode == 0)
            .then(|| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
        Self {
            device: device as i64,
            inode: inode as i64,
            size: meta.len() as i64,
            timestamp: file_mtime_unix(meta),
            fallback_path,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingFile {
    path: PathBuf,
    folder_id: String,
    physical: PhysicalFileKey,
}

#[derive(Clone, Debug)]
struct PreparedPhysicalFile {
    probe_attempted: bool,
    probe: Option<MediaProbe>,
    mime_hint: Option<String>,
    album_art: Option<PathBuf>,
}

#[derive(Debug)]
struct PreparationGroup {
    physical: PhysicalFileKey,
    representative: usize,
    representative_is_direct: bool,
    source: Option<InodeSource>,
}

#[derive(Debug, Default)]
struct PreparedBatch {
    by_physical: HashMap<PhysicalFileKey, PreparedPhysicalFile>,
    worker_indices: HashSet<usize>,
}

fn is_direct_physical_path(path: &Path) -> bool {
    std::fs::canonicalize(path)
        .ok()
        .is_some_and(|canonical| canonical == path)
}

/// Small reusable bounded pool. Jobs may finish out of order, but results are
/// returned in input order so subsequent SQLite IDs stay deterministic.
fn run_bounded_jobs<T, R, F>(jobs: &[T], workers: usize, function: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if jobs.is_empty() {
        return Vec::new();
    }
    let worker_count = workers.max(1).min(jobs.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<R>>> = (0..jobs.len())
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let function = &function;
            let next = &next;
            let results = &results;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(job) = jobs.get(index) else {
                    break;
                };
                let result = function(job);
                *results[index]
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(result);
            });
        }
    });
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|error| error.into_inner())
                .expect("bounded scan worker did not publish its result")
        })
        .collect()
}

fn prepare_pending_files(
    db: &LibraryDb,
    cfg: &ScanConfig,
    pending: &[PendingFile],
) -> ScanResult<PreparedBatch> {
    let mut group_by_physical: HashMap<PhysicalFileKey, usize> = HashMap::new();
    let mut groups: Vec<PreparationGroup> = Vec::new();
    for (index, file) in pending.iter().enumerate() {
        let direct = is_direct_physical_path(&file.path);
        if let Some(group_index) = group_by_physical.get(&file.physical).copied() {
            let group = &mut groups[group_index];
            if direct && !group.representative_is_direct {
                group.representative = index;
                group.representative_is_direct = true;
            }
            continue;
        }
        let source = if file.physical.inode == 0 {
            None
        } else {
            db.find_inode_source(file.physical.device, file.physical.inode)?
        };
        group_by_physical.insert(file.physical.clone(), groups.len());
        groups.push(PreparationGroup {
            physical: file.physical.clone(),
            representative: index,
            representative_is_direct: direct,
            source,
        });
    }

    tracing::info!(
        target: "rusty_dlna",
        paths = pending.len(),
        physical_files = groups.len(),
        aliases = pending.len().saturating_sub(groups.len()),
        workers = cfg.scan_workers.max(1),
        "preparing scan files"
    );
    let started = std::time::Instant::now();
    let results: Vec<ScanResult<PreparedPhysicalFile>> =
        run_bounded_jobs(&groups, cfg.scan_workers, |group| {
            let file = &pending[group.representative];
            if let Some(progress) = &cfg.progress {
                progress.record(&file.path);
            }
            let source_is_current = group.source.as_ref().is_some_and(|source| {
                source.size == group.physical.size
                    && source.timestamp >= group.physical.timestamp
                    && source.stream_probe_rev >= 3
            });
            let probe_attempted = !source_is_current;
            let probe = probe_attempted
                .then(|| {
                    let name = file
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    if is_image(name) {
                        probe_image_with_timeout(&file.path, cfg.external_command_timeout)
                    } else {
                        probe_media_with_timeout(&file.path, cfg.external_command_timeout)
                    }
                })
                .flatten();
            let album_art = prepare_album_art(cfg, &file.path)?;
            Ok(PreparedPhysicalFile {
                probe_attempted,
                probe,
                mime_hint: group.source.as_ref().map(|source| source.mime.clone()),
                album_art,
            })
        });

    let mut batch = PreparedBatch::default();
    for (group, result) in groups.into_iter().zip(results) {
        batch.worker_indices.insert(group.representative);
        batch.by_physical.insert(group.physical, result?);
    }
    tracing::info!(
        target: "rusty_dlna",
        physical_files = batch.by_physical.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "scan file preparation complete"
    );
    Ok(batch)
}

struct DbWalker<'a> {
    db: &'a LibraryDb,
    cfg: &'a ScanConfig,
    walk_stack: HashMap<(u64, u64), ()>,
    rebuild: bool,
    indexed: usize,
    pending: Vec<PendingFile>,
}

impl DbWalker<'_> {
    fn index_pending(&mut self) -> ScanResult<()> {
        let prepared = prepare_pending_files(self.db, self.cfg, &self.pending)?;
        for (index, file) in self.pending.iter().enumerate() {
            if !prepared.worker_indices.contains(&index) {
                if let Some(progress) = &self.cfg.progress {
                    progress.record(&file.path);
                }
            }
            if index_one_file_with_prepared(
                self.db,
                self.cfg,
                &file.path,
                &file.folder_id,
                prepared.by_physical.get(&file.physical),
            )? {
                self.indexed += 1;
                if self.indexed / 100 * 100 == self.indexed {
                    tracing::info!(
                        target: "rusty_dlna",
                        indexed = self.indexed,
                        "scan progress"
                    );
                }
            }
        }
        Ok(())
    }

    fn walk(&mut self, dir: &Path, parent_id: &str, title: &str) -> ScanResult<()> {
        if !path_is_allowed_dir(dir, self.cfg) || path_excluded(dir, title, self.cfg) {
            return Ok(());
        }
        let dir_key = std::fs::metadata(dir).ok().map(|m| inode_key(&m));
        if let Some(key) = dir_key {
            // Cycle only (symlink dir pointing at an ancestor). Alias trees stay walkable.
            if self.walk_stack.contains_key(&key) {
                return Ok(());
            }
            self.walk_stack.insert(key, ());
        }
        let rd = std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))?;
        let folder_id = if parent_id == BROWSEDIR_ID {
            folder_object_id(self.db, parent_id, title)?
        } else {
            parent_id.to_string()
        };

        let mut ents = Vec::new();
        for entry in rd {
            ents.push(entry.map_err(|error| scan_io(dir, error))?);
        }
        ents.sort_by_key(|e| e.file_name());
        for ent in ents {
            let path = ent.path();
            let name = ent.file_name().to_string_lossy().into_owned();
            if !self.cfg.include_hidden && name.starts_with('.') {
                continue;
            }
            let is_dir = match ent.file_type() {
                Ok(t) if t.is_dir() => true,
                Ok(t) if t.is_symlink() => path.is_dir(),
                _ => false,
            };
            if is_dir {
                if is_skipped_dir(&name)
                    || path_excluded(&path, &name, self.cfg)
                    || !path_is_allowed_dir(&path, self.cfg)
                {
                    continue;
                }
                let folder_name = display_os_name(&ent.file_name());
                let child = folder_object_id(self.db, &folder_id, &folder_name)?;
                self.walk(&path, &child, &folder_name)?;
                continue;
            }
            if is_unfinished_name(&name)
                || looks_like_sample_file(&name)
                || is_caption_name(&name)
                || ends_with_ci(&name, ".nfo")
                || name.ends_with(".probe.toml")
                || path_excluded(&path, &name, self.cfg)
                || is_album_art_name_for_config(&name, self.cfg)
            {
                continue;
            }
            if !self
                .cfg
                .root_types_for_path(&path)
                .is_some_and(|types| types.allows(&name))
            {
                continue;
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => m,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(scan_io(&path, error)),
                Ok(_) => continue,
            };
            let physical = PhysicalFileKey::new(&path, &meta);
            let existing = self.db.find_detail_by_path(&path_to_db(&path))?;
            let unchanged = existing.as_ref().is_some_and(|existing| {
                existing.size == physical.size
                    && existing.timestamp == physical.timestamp
                    && existing.device == physical.device
                    && existing.inode == physical.inode
            });
            if unchanged && !self.rebuild {
                continue;
            }
            self.pending.push(PendingFile {
                path,
                folder_id: folder_id.clone(),
                physical,
            });
        }
        if let Some(key) = dir_key {
            self.walk_stack.remove(&key);
        }
        Ok(())
    }
}

/// Write the 1 MiB patterned fixture. `root` is `testdata/library`.
pub fn ensure_pattern_fixture(root: &Path) -> PathBuf {
    let video = root.join("video");
    let _ = std::fs::create_dir_all(&video);
    let movie = video.join("movie.mkv");
    if !file_is_viable(&movie) {
        write_fake_mkv(&movie, 1024 * 1024);
    }
    movie
}

/// TV-show tree used by Series/Genre Browse tests (`video/The Show/…`).
pub fn ensure_show_fixture(root: &Path) {
    let show = root.join("video/The Show");
    let _ = std::fs::create_dir_all(&show);
    let tv = show.join("tvshow.nfo");
    if !tv.exists() {
        let _ = std::fs::write(
            &tv,
            "<tvshow><title>The Show</title><genre>Drama</genre><genre>Crime</genre></tvshow>\n",
        );
    }
    for (stem, title, season, ep) in [("S01E01", "Pilot", 1, 1), ("S01E02", "Second", 1, 2)] {
        let mkv = show.join(format!("{stem}.mkv"));
        if !file_is_viable(&mkv) {
            write_fake_mkv(&mkv, 64);
        }
        let nfo = show.join(format!("{stem}.nfo"));
        if !nfo.exists() {
            let _ = std::fs::write(
                &nfo,
                format!(
                    "<episodedetails><title>{title}</title><season>{season}</season><episode>{ep}</episode></episodedetails>\n"
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempPath(PathBuf);

    impl TempPath {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "rusty-dlna-{label}-{}-{sequence}",
                std::process::id()
            )))
        }
    }

    impl std::ops::Deref for TempPath {
        type Target = PathBuf;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl AsRef<Path> for TempPath {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            if self.0.is_dir() {
                let _ = std::fs::remove_dir_all(&self.0);
            } else {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    #[test]
    fn seeded_virtual_views_match_minidlna_oracle() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("docs/oracle/containers.c");
        let reference = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let scanner_path = path.with_file_name("scanner.h");
        let scanner = std::fs::read_to_string(&scanner_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", scanner_path.display()));
        let catalog = Catalog::new();

        for (id, title) in [
            (MUSIC_RECENT_ID, "Recently Added"),
            (VIDEO_RECENT_ID, "Recently Added"),
            (IMAGE_RECENT_ID, "Recently Added"),
            (MUSIC_ALL_ID, "All Music"),
            (VIDEO_ALL_ID, "All Video"),
            (IMAGE_ALL_ID, "All Pictures"),
            (MUSIC_GENRE_ID, "Genre"),
            (MUSIC_ARTIST_ID, "Artist"),
            (MUSIC_ALBUM_ID, "Album"),
            (IMAGE_DATE_ID, "Date Taken"),
            (IMAGE_CAMERA_ID, "Camera"),
        ] {
            assert!(
                reference.contains(id) || scanner.contains(&format!("\"{id}\"")),
                "reference missing virtual view {id}"
            );
            let container = catalog
                .containers
                .get(id)
                .unwrap_or_else(|| panic!("catalog missing virtual view {id}"));
            assert_eq!(container.title, title, "title for virtual view {id}");
        }

        for (alias, target) in [
            ("4", MUSIC_ALL_ID),
            ("5", MUSIC_GENRE_ID),
            ("6", MUSIC_ARTIST_ID),
            ("7", MUSIC_ALBUM_ID),
            ("8", VIDEO_ALL_ID),
            ("B", IMAGE_ALL_ID),
            ("C", IMAGE_DATE_ID),
            ("14", MUSIC_DIR_ID),
            ("15", VIDEO_DIR_ID),
            ("16", IMAGE_DIR_ID),
            ("D2", IMAGE_CAMERA_ID),
        ] {
            assert!(
                reference.contains(&format!("NULL, \"{alias}\", &")),
                "reference missing alias {alias} -> {target}"
            );
            assert!(catalog.containers.contains_key(target));
        }
    }

    #[test]
    fn text_named_mkv_is_not_viable() {
        let p = TempPath::new("not-video.mkv");
        std::fs::write(&p, b"this is a readme pretending to be a movie\n").unwrap();
        assert!(!looks_like_av_container(&p));
        assert!(!file_is_viable(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn guess_hdr_from_name_disc_remux_vs_web() {
        assert_eq!(
            guess_hdr_from_name("01 - Despicable Me (2010) - 2160p UHD BluRay Remux"),
            None,
            "no DV token"
        );
        assert_eq!(
            guess_hdr_from_name("04 - Despicable Me 4 (2024) - 2160p UHD BDRemux HDR DV"),
            Some("dv-p7")
        );
        assert_eq!(
            guess_hdr_from_name("Movie.2024.2160p.UHD.BDRemux.HDR.DV.HEVC"),
            Some("dv-p7")
        );
        assert_eq!(
            guess_hdr_from_name("Show.S01E01.2160p.WEB-DL.DDP5.1.DV.H.265"),
            None,
            "WEB-DL DoVi is usually P8"
        );
        assert_eq!(
            guess_hdr_from_name("02 - Frozen II (2019) - 2160p UHD BDRemux Hybrid DoVi"),
            None,
            "Hybrid DoVi remux is usually P8"
        );
        assert_eq!(guess_hdr_from_name("clip.dv-p7.mkv"), Some("dv-p7"));
        assert_eq!(guess_hdr_from_name("clip.dv-p8.mkv"), Some("dv-p8"));
    }

    #[test]
    fn page_children_matches_children_of_slice() {
        let mut cat = Catalog::new();
        cat.add_container(
            "64$1",
            BROWSEDIR_ID,
            "video",
            "container.storageFolder",
            true,
        );
        cat.link_child(BROWSEDIR_ID, "64$1");
        for i in 0..20 {
            let oid = format!("64$1${i:X}");
            cat.items.insert(
                oid.clone(),
                MediaItem {
                    object_id: oid.clone(),
                    parent_id: "64$1".into(),
                    detail_id: i + 1,
                    title: format!("m{i:02}"),
                    class: "item.videoItem".into(),
                    date: "2024-01-01".into(),
                    path: PathBuf::from(format!("/m/{i}.mkv")),
                    mime: "video/x-matroska".into(),
                    ext: "mkv".into(),
                    size: 1000,
                    mtime: 1,
                    captions: vec![],
                    probe: SourceProbe::default(),
                    dlna_pn: None,
                    ref_id: None,
                    device: 1,
                    inode: i as u64 + 1,
                    duration: None,
                    bitrate: None,
                    resolution: None,
                    channels: None,
                    samplerate: None,
                    album_art: 0,
                    creator: None,
                    comment: None,
                    artist: None,
                    album_artist: None,
                    composer: None,
                    contributor: None,
                    album: None,
                    genre: None,
                    disc: None,
                    track: None,
                    rating: None,
                    rotation: None,
                    bookmark_sec: 0,
                    watch_count: 0,
                },
            );
            cat.link_child("64$1", &oid);
        }
        let all = cat.children_of("64$1").unwrap();
        let (page, total) = cat.page_children("64$1", 5, 7).unwrap();
        assert_eq!(total, all.len() as u32);
        assert_eq!(page.len(), 7);
        for (a, b) in page.iter().zip(all.iter().skip(5)) {
            match (a, b) {
                (CatalogChild::Item(x), CatalogChild::Item(y)) => {
                    assert_eq!(x.object_id, y.object_id);
                }
                _ => panic!("expected items"),
            }
        }
    }

    #[test]
    fn skip_rules() {
        assert!(is_junk_dir("@eaDir"));
        assert!(is_sample_or_trailer_dir("sample"));
        assert!(!is_sample_or_trailer_dir("Sample"));
        assert!(is_unfinished_name("movie.mkv.part"));
        assert!(looks_like_sample_file("Movie-sample.mkv"));
        assert!(is_disc_structure_dir("BDMV"));
        assert!(is_disc_structure_dir("bdmv"));
        assert!(is_disc_structure_dir("VIDEO_TS"));
        assert!(is_skipped_dir("CERTIFICATE"));
        assert!(!is_skipped_dir("Movies"));
    }

    #[test]
    fn exclusion_hidden_and_sidecar_policy_options_are_effective() {
        assert!(basename_glob_matches("*-extra.?kv", "Movie-EXTRA.mkv"));
        assert!(basename_glob_matches("foo?.mkv", "FOO1.MKV"));
        assert!(!basename_glob_matches("foo?.mkv", "foo12.mkv"));

        let tmp = TempPath::new("scan-policy");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".hidden-dir")).unwrap();
        for path in [
            tmp.join("keep.mkv"),
            tmp.join("movie-extra.mkv"),
            tmp.join(".hidden.mkv"),
            tmp.join(".hidden-dir/inside.mkv"),
        ] {
            write_fake_mkv(&path, 64);
        }
        std::fs::write(tmp.join("keep.en.srt"), "caption").unwrap();
        std::fs::write(tmp.join("MyArt.jpg"), TINY_JPEG).unwrap();
        let base = ScanConfig {
            media_dirs: vec![tmp.clone()],
            types: MediaTypes::video_only(),
            exclude_files: vec!["*-extra.*".into()],
            album_art_names: vec!["MyArt.jpg".into()],
            subtitles: false,
            thumbnails: false,
            ..Default::default()
        };
        assert!(is_album_art_name_for_config("myart.JPG", &base));
        let catalog = scan(&base).unwrap();
        let originals: Vec<_> = catalog
            .items
            .values()
            .filter(|item| item.ref_id.is_none())
            .collect();
        assert_eq!(originals.len(), 1, "{originals:#?}");
        assert_eq!(originals[0].path, tmp.join("keep.mkv"));
        assert!(originals[0].captions.is_empty());
        assert!(
            originals[0].album_art > 0,
            "configured sidecar remains enabled"
        );

        let visible = ScanConfig {
            include_hidden: true,
            album_art_names: Vec::new(),
            ..base
        };
        let catalog = scan(&visible).unwrap();
        let paths: HashSet<_> = catalog
            .items
            .values()
            .filter(|item| item.ref_id.is_none())
            .map(|item| item.path.clone())
            .collect();
        assert!(paths.contains(&tmp.join(".hidden.mkv")));
        assert!(paths.contains(&tmp.join(".hidden-dir/inside.mkv")));
        assert!(!paths.contains(&tmp.join("movie-extra.mkv")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn duration_str_hms_millis() {
        assert_eq!(duration_str(0), "0:00:00.000");
        assert_eq!(duration_str(3_661_234), "1:01:01.234");
    }

    #[test]
    fn nfo_year_becomes_ten_char_date() {
        assert_eq!(
            nfo_date_from_text("<movie><year>1999</year></movie>").as_deref(),
            Some("1999-01-01")
        );
    }

    const EPISODE_NFO: &str = r#"<episodedetails>
  <showtitle>The Show</showtitle>
  <title>Pilot</title>
  <plot>The plot text</plot>
  <genre>Drama</genre>
  <genre>Crime</genre>
  <director>Jane Doe</director>
  <studio>Network</studio>
  <season>1</season>
  <episode>2</episode>
  <premiered>2020-05-01</premiered>
</episodedetails>"#;

    #[test]
    fn nfo_title_plot_show_season() {
        let parsed = parse_nfo_text(EPISODE_NFO);
        assert_eq!(parsed.title.as_deref(), Some("The Show - Pilot"));
        assert_eq!(parsed.showtitle.as_deref(), Some("The Show"));
        assert_eq!(parsed.episode_title.as_deref(), Some("Pilot"));
        assert_eq!(
            episode_display_title("The Show - Pilot", Some("The Show")),
            "Pilot"
        );
        assert_eq!(parsed.comment.as_deref(), Some("The plot text"));
        assert_eq!(parsed.genre.as_deref(), Some("Drama / Crime"));
        assert_eq!(parsed.creator.as_deref(), Some("Jane Doe"));
        assert!(
            parsed.artist.as_deref() == Some("Network")
                || parsed.artist.as_deref() == Some("The Show"),
            "artist={:?}",
            parsed.artist
        );
        assert_eq!(parsed.disc, Some(1));
        assert_eq!(parsed.track, Some(2));
        assert!(
            parsed
                .date
                .as_deref()
                .is_some_and(|d| d.starts_with("2020-05-01")),
            "date={:?}",
            parsed.date
        );

        let credits = parse_nfo_text("<credits>Pat Lee</credits>");
        assert_eq!(credits.creator.as_deref(), Some("Pat Lee"));
        let studio = parse_nfo_text("<studio>Network</studio>");
        assert_eq!(studio.creator.as_deref(), Some("Network"));
        assert_eq!(studio.artist.as_deref(), Some("Network"));
        let director_wins = parse_nfo_text(
            "<director>Jane Doe</director><credits>Pat Lee</credits><studio>Network</studio>",
        );
        assert_eq!(director_wins.creator.as_deref(), Some("Jane Doe"));

        assert!(nfo_too_large(64 * 1024 + 1));
        assert!(!nfo_too_large(64 * 1024));

        let tmp = TempPath::new("nfo-ep");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("show")).unwrap();
        write_fake_mkv(&tmp.join("show/S01E01.mkv"), 64);
        std::fs::write(tmp.join("show/S01E01.nfo"), EPISODE_NFO).unwrap();
        let huge = "x".repeat(64 * 1024 + 1);
        std::fs::write(
            tmp.join("show/huge.nfo"),
            format!("<title>TooBig</title>{huge}"),
        )
        .unwrap();
        let huge_meta = nfo_for_file(&tmp.join("show/huge.mkv"), std::slice::from_ref(&tmp));
        assert!(huge_meta.title.is_none(), "skip >64KiB: {huge_meta:?}");

        let cat = scan(&ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            types: MediaTypes::video_only(),
            ..Default::default()
        })
        .unwrap();
        let ep = cat
            .items
            .values()
            .find(|i| i.title == "The Show - Pilot")
            .expect("episode title from nfo");
        assert!(
            ep.comment
                .as_deref()
                .is_some_and(|c| c.contains("The plot text")),
            "comment={:?}",
            ep.comment
        );
        assert_eq!(ep.genre.as_deref(), Some("Drama / Crime"));
        assert_eq!(ep.creator.as_deref(), Some("Jane Doe"));
        assert!(
            ep.artist.as_deref() == Some("Network") || ep.artist.as_deref() == Some("The Show"),
            "artist={:?}",
            ep.artist
        );
        assert_eq!(ep.disc, Some(1));
        assert_eq!(ep.track, Some(2));
        assert!(ep.date.starts_with("2020-05-01"), "date={}", ep.date);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tvshow_nfo_inherited_by_episode() {
        let tmp = TempPath::new("nfo-tv");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("show")).unwrap();
        std::fs::write(
            tmp.join("show/tvshow.nfo"),
            r#"<tvshow>
  <title>The Show</title>
  <plot>Show plot</plot>
  <genre>Drama</genre>
  <studio>Network</studio>
</tvshow>"#,
        )
        .unwrap();
        write_fake_mkv(&tmp.join("show/S01E01.mkv"), 64);
        std::fs::write(
            tmp.join("show/S01E01.nfo"),
            "<episodedetails><title>Pilot</title></episodedetails>\n",
        )
        .unwrap();
        write_fake_mkv(&tmp.join("show/S01E02.mkv"), 64);
        std::fs::write(
            tmp.join("show/S01E02.nfo"),
            "<episodedetails><title>Second</title><plot>Own plot</plot></episodedetails>\n",
        )
        .unwrap();

        let cat = scan(&ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            types: MediaTypes::video_only(),
            ..Default::default()
        })
        .unwrap();
        let ep1 = cat
            .items
            .values()
            .find(|i| i.path.ends_with("S01E01.mkv") && i.ref_id.is_none())
            .expect("S01E01");
        assert_eq!(ep1.title, "The Show - Pilot");
        assert_eq!(ep1.comment.as_deref(), Some("Show plot"));
        assert_eq!(ep1.genre.as_deref(), Some("Drama"));
        let ep2 = cat
            .items
            .values()
            .find(|i| i.path.ends_with("S01E02.mkv") && i.ref_id.is_none())
            .expect("S01E02");
        assert_eq!(ep2.title, "The Show - Second");
        assert_eq!(ep2.comment.as_deref(), Some("Own plot"));
        assert_eq!(ep2.genre.as_deref(), Some("Drama"));
        assert_eq!(ep1.album.as_deref(), Some("The Show"));
        assert_eq!(ep2.album.as_deref(), Some("The Show"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn series_and_genre_trees_from_nfo() {
        let tmp = TempPath::new("series");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("The Show")).unwrap();
        std::fs::write(
            tmp.join("The Show/tvshow.nfo"),
            r#"<tvshow><title>The Show</title><genre>Drama</genre><genre>Crime</genre></tvshow>"#,
        )
        .unwrap();
        write_fake_mkv(&tmp.join("The Show/S01E01.mkv"), 64);
        std::fs::write(
            tmp.join("The Show/S01E01.nfo"),
            "<episodedetails><title>Pilot</title><season>1</season><episode>1</episode></episodedetails>\n",
        )
        .unwrap();
        write_fake_mkv(&tmp.join("The Show/S01E02.mkv"), 64);
        std::fs::write(
            tmp.join("The Show/S01E02.nfo"),
            "<episodedetails><title>Second</title><season>1</season><episode>2</episode></episodedetails>\n",
        )
        .unwrap();
        write_fake_mkv(&tmp.join("The Show/S02E01.mkv"), 64);
        std::fs::write(
            tmp.join("The Show/S02E01.nfo"),
            "<episodedetails><title>Return</title><season>2</season><episode>1</episode></episodedetails>\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.join("movies")).unwrap();
        write_fake_mkv(&tmp.join("movies/film.mkv"), 64);
        std::fs::write(
            tmp.join("movies/film.nfo"),
            "<movie><title>Standalone</title><genre>Action</genre></movie>\n",
        )
        .unwrap();

        let cat = scan(&ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        })
        .unwrap();
        assert!(cat.containers.contains_key(VIDEO_SERIES_ID));
        assert!(cat.containers.contains_key(VIDEO_GENRE_ID));
        let series = cat.children_of(VIDEO_SERIES_ID).expect("series");
        let shows: Vec<_> = series
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Container(x) => Some(x.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(shows, ["The Show"], "{shows:?}");
        let show = series
            .iter()
            .find_map(|c| match c {
                CatalogChild::Container(x) if x.title == "The Show" => Some(x),
                _ => None,
            })
            .expect("show container");
        let seasons = cat.children_of(&show.object_id).expect("seasons");
        let season_titles: Vec<_> = seasons
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Container(x) => Some(x.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(season_titles, ["Season 1", "Season 2"], "{season_titles:?}");
        let s1 = seasons
            .iter()
            .find_map(|c| match c {
                CatalogChild::Container(x) if x.title == "Season 1" => Some(x),
                _ => None,
            })
            .unwrap();
        let eps = cat.children_of(&s1.object_id).expect("s1 eps");
        let ep_titles: Vec<_> = eps
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Item(i) => Some(i.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ep_titles, ["Pilot", "Second"], "{ep_titles:?}");
        assert!(
            eps.iter().all(|c| match c {
                CatalogChild::Item(i) => i.ref_id.is_some(),
                _ => true,
            }),
            "series items must be REF_ID aliases"
        );
        let genres = cat.children_of(VIDEO_GENRE_ID).expect("genres");
        let genre_names: Vec<_> = genres
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Container(x) => Some(x.title.as_str()),
                _ => None,
            })
            .collect();
        assert!(genre_names.contains(&"Drama"), "{genre_names:?}");
        assert!(genre_names.contains(&"Crime"), "{genre_names:?}");
        assert!(genre_names.contains(&"Action"), "{genre_names:?}");
        let action = genres
            .iter()
            .find_map(|c| match c {
                CatalogChild::Container(x) if x.title == "Action" => Some(x),
                _ => None,
            })
            .unwrap();
        let action_items = cat.children_of(&action.object_id).expect("action items");
        assert!(
            action_items.iter().any(|c| match c {
                CatalogChild::Item(i) => i.title.contains("Standalone"),
                _ => false,
            }),
            "{action_items:?}"
        );
        let show_ids: Vec<_> = series
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Container(x) => Some(x.object_id.as_str()),
                _ => None,
            })
            .collect();
        for id in show_ids {
            let kids = cat.children_of(id).unwrap_or_default();
            assert!(
                !kids.iter().any(|c| match c {
                    CatalogChild::Item(i) => i.title.contains("Standalone"),
                    _ => false,
                }),
                "movie must not appear under Series"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bookmark_survives_reopen() {
        let tmp = TempPath::new("bookmark");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let dbp = tmp.join("files.db");
        let detail_id;
        {
            let db = LibraryDb::open(&dbp).unwrap();
            detail_id = db
                .insert_detail(NewDetail {
                    path: "/media/bookmark.mp4",
                    size: 1,
                    timestamp: 1,
                    title: "bookmark",
                    date: "",
                    mime: "video/mp4",
                    device: 1,
                    inode: 1,
                    dlna_pn: None,
                })
                .unwrap();
            db.set_bookmark(detail_id, 120).unwrap();
            db.set_watch_count(detail_id, 3).unwrap();
            assert_eq!(db.get_bookmark(detail_id).unwrap(), Some((120, 3)));
        }
        let db = LibraryDb::open(&dbp).unwrap();
        assert_eq!(
            db.get_bookmark(detail_id).unwrap(),
            Some((120, 3)),
            "BOOKMARKS must survive LibraryDb::open"
        );
        assert_eq!(db.get_bookmark(7).unwrap(), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bookmark_retention_uses_last_update_and_zero_is_indefinite() {
        let db = LibraryDb::open_memory().unwrap();
        let insert = |path: &'static str, inode| {
            db.insert_detail(NewDetail {
                path,
                size: 1,
                timestamp: 1,
                title: path,
                date: "",
                mime: "video/mp4",
                device: 1,
                inode,
                dlna_pn: None,
            })
            .unwrap()
        };
        let old = insert("/media/old.mp4", 1);
        let fresh = insert("/media/fresh.mp4", 2);
        db.update_bookmark(old, Some(120), Some(1)).unwrap();
        db.update_bookmark(fresh, Some(240), Some(2)).unwrap();

        let now = 20_000_000_i64;
        db.connection()
            .execute(
                "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
                rusqlite::params![now - 91 * 86_400, old],
            )
            .unwrap();
        db.connection()
            .execute(
                "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
                rusqlite::params![now - 90 * 86_400, fresh],
            )
            .unwrap();

        assert_eq!(db.prune_expired_bookmarks(0, now).unwrap(), 0);
        assert_eq!(db.get_bookmark(old).unwrap(), Some((120, 1)));
        assert_eq!(db.prune_expired_bookmarks(90, now).unwrap(), 1);
        assert_eq!(db.get_bookmark(old).unwrap(), None);
        assert_eq!(
            db.get_bookmark(fresh).unwrap(),
            Some((240, 2)),
            "state exactly on the retention boundary remains valid"
        );
    }

    #[test]
    fn full_reconciliation_prunes_expired_bookmarks_and_republishes_catalog() {
        let tmp = TempPath::new("bookmark-reconcile");
        let media = tmp.join("video");
        std::fs::create_dir_all(&media).unwrap();
        write_fake_mkv(&media.join("movie.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![media],
            db_path: Some(tmp.join("files.db")),
            bookmark_retention_days: 90,
            types: MediaTypes::video_only(),
            ..ScanConfig::default()
        };
        let initial = scan(&cfg).unwrap();
        let item = initial.items.values().next().unwrap().clone();
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        db.set_bookmark(item.detail_id, 120).unwrap();
        db.connection()
            .execute(
                "UPDATE BOOKMARKS SET UPDATED_AT=?1 WHERE ID=?2",
                rusqlite::params![unix_now_seconds() - 91 * 86_400, item.detail_id],
            )
            .unwrap();
        drop(db);

        let (catalog, delta) = monitor(&cfg).unwrap();
        assert_eq!(delta.changed, 1);
        let catalog = catalog.expect("bookmark expiry must republish the catalog");
        assert_eq!(
            catalog
                .get_item_by_detail(item.detail_id)
                .unwrap()
                .bookmark_sec,
            0
        );
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_bookmark(item.detail_id).unwrap(), None);
    }

    #[test]
    fn scan_skips_junk_sample_exclude_and_reads_nfo_captions() {
        let tmp = TempPath::new("scan");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        std::fs::create_dir_all(tmp.join("sample")).unwrap();
        std::fs::create_dir_all(tmp.join("@eaDir")).unwrap();
        std::fs::create_dir_all(tmp.join("exclude_me")).unwrap();
        write_fake_mkv(&tmp.join("video/movie.mkv"), 64);
        std::fs::write(tmp.join("video/movie.nfo"), "<year>1999</year>").unwrap();
        std::fs::write(tmp.join("video/movie.srt"), b"1").unwrap();
        std::fs::write(tmp.join("video/movie.en.srt"), b"2").unwrap();
        std::fs::write(tmp.join("sample/skip.mkv"), b"x").unwrap();
        std::fs::write(tmp.join("@eaDir/junk.mkv"), b"x").unwrap();
        std::fs::write(tmp.join("exclude_me/secret.mkv"), b"x").unwrap();
        std::fs::write(tmp.join("unfinished.mkv.part"), b"x").unwrap();
        std::fs::write(
            tmp.join("video/dvp7.probe.toml"),
            "hdr = \"dv-p7\"\naudio = \"truehd\"\n",
        )
        .unwrap();
        write_fake_mkv(&tmp.join("video/dvp7.mkv"), 64);
        std::fs::write(
            tmp.join("video/not-video.mkv"),
            b"this is a text file not a video",
        )
        .unwrap();
        std::fs::write(tmp.join("video/notes.txt"), b"ignore").unwrap();

        let cat = scan(&ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            exclude_dirs: vec!["exclude_me".into()],
            exclude_files: vec![],
            ..Default::default()
        })
        .unwrap();
        assert!(
            !cat.items
                .values()
                .any(|i| i.title == "not-video" || i.title == "notes"),
            "text must not be indexed as video"
        );
        let titles: Vec<_> = cat.items.values().map(|i| i.title.as_str()).collect();
        assert!(titles.contains(&"movie"));
        assert!(titles.contains(&"dvp7"));
        assert!(!titles
            .iter()
            .any(|t| *t == "skip" || *t == "secret" || *t == "junk"));
        let movie = cat
            .items
            .values()
            .find(|i| i.title == "movie" && i.parent_id != VIDEO_ALL_ID)
            .unwrap();
        assert_eq!(movie.date, "1999-01-01");
        assert!(movie.captions.len() >= 2);
        let dvp7 = cat.items.values().find(|i| i.title == "dvp7").unwrap();
        assert_eq!(dvp7.probe.hdr, "dv-p7");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn inode_reuse_for_hardlink_alias() {
        let tmp = TempPath::new("inode");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let a = tmp.join("video/orig.mkv");
        write_fake_mkv(&a, 64);
        let b = tmp.join("video/alias.mkv");
        let _ = std::fs::remove_file(&b);
        std::fs::hard_link(&a, &b).unwrap();
        let dbp = tmp.join("files.db");
        let cat = scan(&ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(dbp.clone()),
            ..Default::default()
        })
        .unwrap();
        let orig = cat
            .items
            .values()
            .find(|i| i.path.ends_with("orig.mkv"))
            .expect("orig.mkv");
        let alias = cat
            .items
            .values()
            .find(|i| i.path.ends_with("alias.mkv"))
            .expect("alias.mkv");
        let db = LibraryDb::open(&dbp).unwrap();
        let o = db
            .find_detail_by_path(&orig.path.to_string_lossy())
            .unwrap()
            .unwrap();
        let arow = db
            .find_detail_by_path(&alias.path.to_string_lossy())
            .unwrap()
            .unwrap();
        // rustyDLNA: one DETAILS row per path; same DEVICE+INODE.
        assert_ne!(o.id, arow.id);
        let all_video: Vec<_> = cat
            .items
            .values()
            .filter(|i| i.parent_id == VIDEO_ALL_ID)
            .collect();
        assert_eq!(all_video.len(), 1, "All Video must not list hardlink twice");
        assert_eq!(orig.date, alias.date, "alias must clone original DATE");
        assert_eq!(orig.mime, alias.mime, "alias must clone original MIME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bounded_scan_jobs_use_parallelism_without_exceeding_limit() {
        let jobs: Vec<usize> = (0..24).collect();
        let active = std::sync::atomic::AtomicUsize::new(0);
        let peak = std::sync::atomic::AtomicUsize::new(0);
        let results = run_bounded_jobs(&jobs, 4, |job| {
            let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            job * 2
        });
        assert_eq!(results, jobs.iter().map(|job| job * 2).collect::<Vec<_>>());
        assert!(peak.load(std::sync::atomic::Ordering::SeqCst) > 1);
        assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 4);
    }

    #[cfg(unix)]
    #[test]
    fn physical_preparation_prefers_real_path_and_groups_directory_symlink() {
        let tmp = TempPath::new("prepare-alias");
        std::fs::create_dir_all(tmp.join("zz-real")).unwrap();
        let direct = tmp.join("zz-real/movie.mkv");
        write_fake_mkv(&direct, 64);
        let alias_dir = tmp.join("00-alias");
        std::os::unix::fs::symlink(tmp.join("zz-real"), &alias_dir).unwrap();
        let alias = alias_dir.join("movie.mkv");
        let alias_meta = std::fs::metadata(&alias).unwrap();
        let direct_meta = std::fs::metadata(&direct).unwrap();
        let pending = vec![
            PendingFile {
                path: alias.clone(),
                folder_id: "alias".into(),
                physical: PhysicalFileKey::new(&alias, &alias_meta),
            },
            PendingFile {
                path: direct.clone(),
                folder_id: "direct".into(),
                physical: PhysicalFileKey::new(&direct, &direct_meta),
            },
        ];
        let db = LibraryDb::open_memory().unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            types: MediaTypes::video_only(),
            thumbnails: false,
            scan_workers: 4,
            ..Default::default()
        };
        let prepared = prepare_pending_files(&db, &cfg, &pending).unwrap();
        assert_eq!(prepared.by_physical.len(), 1);
        assert_eq!(prepared.worker_indices, HashSet::from([1]));
        assert_eq!(
            source_image_identity(&direct).unwrap(),
            source_image_identity(&alias).unwrap(),
            "cache identity must follow the physical file, not its alias path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_alias_reuses_one_generated_thumbnail() {
        let tmp = TempPath::new("thumb-alias");
        let real_dir = tmp.join("zz-real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let direct = real_dir.join("movie.mp4");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=64x64:rate=2",
                "-threads",
                "1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&direct)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !generated {
            eprintln!("skip alias thumbnail reuse (ffmpeg fixture unavailable)");
            return;
        }
        let alias_dir = tmp.join("00-alias");
        std::os::unix::fs::symlink(&real_dir, &alias_dir).unwrap();
        let alias = alias_dir.join("movie.mp4");
        let db_path = tmp.join("cache/files.db");
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(db_path.clone()),
            types: MediaTypes::video_only(),
            thumbnails: true,
            scan_workers: 4,
            ..Default::default()
        };
        scan(&cfg).unwrap();
        let db = LibraryDb::open(&db_path).unwrap();
        let alias_detail = db
            .find_detail_by_path(&path_to_db(&alias))
            .unwrap()
            .expect("alias detail");
        let direct_detail = db
            .find_detail_by_path(&path_to_db(&direct))
            .unwrap()
            .expect("direct detail");
        let alias_art = db.detail_album_art(alias_detail.id).unwrap();
        let direct_art = db.detail_album_art(direct_detail.id).unwrap();
        assert!(alias_art > 0);
        assert_eq!(alias_art, direct_art, "aliases must share one artwork row");
        assert!(
            db.details_missing_stream_meta().unwrap().is_empty(),
            "the initial scan must persist its prepared probe for every alias"
        );
        let (_, unchanged) = monitor(&cfg).unwrap();
        assert_eq!(unchanged, ScanDelta::default());
        let thumbnails = std::fs::read_dir(tmp.join("cache/art"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("thumb-"))
            .count();
        assert_eq!(thumbnails, 1, "ffmpeg must generate one physical thumbnail");
    }

    /// 1×1 JPEG (SOI + JFIF + EOI). Real enough for sidecar + HTTP magic checks.
    const TINY_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06,
        0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B,
        0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20,
        0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31,
        0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF,
        0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00,
        0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x08, 0xFF, 0xC4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08,
        0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x2A, 0x1F, 0xFF, 0xD9,
    ];

    #[test]
    fn jpeg_magic_and_art_name_png() {
        assert!(is_jpeg_bytes(TINY_JPEG));
        assert!(!is_jpeg_bytes(b"\x89PNG"));
        assert!(is_album_art_name("clip-poster.png"));
        assert!(is_album_art_name("clip-fanart.png"));
        assert!(is_album_art_name("poster.png"));
        assert!(is_album_art_name("Poster.jpg"));
        assert!(!is_album_art_name("clip.mkv"));
    }

    #[test]
    fn art_sidecar_indexed_and_cloned() {
        let tmp = TempPath::new("art");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write_fake_mkv(&tmp.join("clip.mkv"), 64);
        std::fs::write(tmp.join("clip-poster.jpg"), TINY_JPEG).unwrap();
        let dbp = tmp.join("files.db");
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(dbp.clone()),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg).unwrap();
        let video = cat
            .items
            .values()
            .find(|i| i.path.ends_with("clip.mkv"))
            .expect("clip indexed");
        assert!(video.album_art > 0, "sidecar must set ALBUM_ART: {video:?}");
        assert!(
            !cat.items
                .values()
                .any(|i| i.title.contains("poster") || i.title == "clip-poster"),
            "art files stay skipped"
        );
        assert!(cat.album_art_paths.contains_key(&video.album_art));

        let alias = tmp.join("alias.mkv");
        let _ = std::fs::remove_file(&alias);
        std::fs::hard_link(tmp.join("clip.mkv"), &alias).unwrap();
        let (cat2, _) = rescan(&cfg, &cat).unwrap();
        let clip2 = cat2
            .items
            .values()
            .find(|i| i.path.ends_with("clip.mkv"))
            .expect("clip after clone");
        let alias_item = cat2
            .items
            .values()
            .find(|i| i.path.ends_with("alias.mkv"))
            .expect("hardlink alias");
        assert_eq!(
            alias_item.album_art, clip2.album_art,
            "clone must share ALBUM_ART id"
        );
        assert!(alias_item.album_art > 0);

        let poster = tmp.join("clip-poster.jpg");
        let _ = std::fs::write(&poster, TINY_JPEG);
        let (cat3, _) = monitor_dirty(&cfg, &[poster]).unwrap();
        let cat3 = cat3.unwrap_or(cat2);
        let ids: Vec<i64> = cat3
            .items
            .values()
            .filter(|i| i.path.ends_with("clip.mkv") || i.path.ends_with("alias.mkv"))
            .map(|i| i.album_art)
            .collect();
        assert!(ids.len() >= 2, "both aliases still listed");
        assert!(ids.iter().all(|id| *id == ids[0] && *id > 0));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sidecar_write_replace_and_delete_recompute_one_catalog_generation() {
        let tmp = TempPath::new("sidecar-lifecycle");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let media = tmp.join("movie.mkv");
        let nfo = tmp.join("movie.nfo");
        let caption = tmp.join("movie.en.srt");
        let unrelated_caption = tmp.join("movie2.srt");
        let poster = tmp.join("movie-poster.jpg");
        write_fake_mkv(&media, 64);
        std::fs::write(
            &nfo,
            "<movie><title>Sidecar Title</title><genre>Drama</genre></movie>",
        )
        .unwrap();
        std::fs::write(&caption, "first subtitle").unwrap();
        std::fs::write(&unrelated_caption, "must not attach").unwrap();
        std::fs::write(&poster, TINY_JPEG).unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };

        let initial = scan(&cfg).unwrap();
        let item = initial
            .items
            .values()
            .find(|item| item.path == media && item.ref_id.is_none())
            .unwrap();
        let detail_id = item.detail_id;
        let sidecar_art_id = item.album_art;
        assert_eq!(item.title, "Sidecar Title");
        assert_eq!(item.captions.len(), 1, "movie2.srt must not attach");
        assert!(sidecar_art_id > 0);

        std::fs::write(&caption, "replacement subtitle bytes").unwrap();
        let (catalog, delta) = monitor_dirty(&cfg, std::slice::from_ref(&caption)).unwrap();
        assert!(catalog.is_some());
        assert_eq!(delta.changed, 1, "one settled sidecar burst is one change");

        // A replacement at the same path still invalidates clients even
        // though its ALBUM_ART row/path is stable.
        std::fs::write(&poster, TINY_JPEG).unwrap();
        let (_, delta) = monitor_dirty(&cfg, std::slice::from_ref(&poster)).unwrap();
        assert_eq!(delta.changed, 1);

        // Malformed sidecar parsing is transactional: the last published DB
        // generation remains fully readable and unchanged.
        std::fs::write(&nfo, "<movie><title>broken</movie>").unwrap();
        assert!(monitor_dirty(&cfg, std::slice::from_ref(&nfo)).is_err());
        let retained = open_library_db(cfg.db_path.as_ref().unwrap())
            .unwrap()
            .load_catalog()
            .unwrap();
        assert_eq!(retained.by_detail[&detail_id], item.object_id);
        assert_eq!(retained.items[&item.object_id].title, "Sidecar Title");

        std::fs::remove_file(&nfo).unwrap();
        std::fs::remove_file(&caption).unwrap();
        std::fs::remove_file(&poster).unwrap();
        let (catalog, delta) =
            monitor_dirty(&cfg, &[nfo.clone(), caption.clone(), poster.clone()]).unwrap();
        assert_eq!(delta.changed, 1, "a multi-sidecar burst bumps once");
        let catalog = catalog.unwrap();
        let item = &catalog.items[&catalog.by_detail[&detail_id]];
        assert_eq!(item.title, "movie", "NFO deletion restores filename title");
        assert!(item.captions.is_empty(), "caption deletion must propagate");
        assert_ne!(
            item.album_art, sidecar_art_id,
            "deleted art cannot stay selected"
        );
        assert!(
            !catalog.items.values().any(|candidate| {
                candidate.detail_id == detail_id && candidate.parent_id.starts_with(VIDEO_GENRE_ID)
            }),
            "NFO deletion must remove stale genre aliases"
        );
        let db = open_library_db(cfg.db_path.as_ref().unwrap()).unwrap();
        assert!(db.album_art_path(sidecar_art_id).unwrap().is_none());

        // A periodic reconcile (no inotify paths) reaches the same desired
        // state for sidecar additions and deletions.
        std::fs::write(&nfo, "<movie><title>Periodic Title</title></movie>").unwrap();
        std::fs::write(&caption, "periodic subtitle").unwrap();
        std::fs::write(&poster, TINY_JPEG).unwrap();
        let (catalog, delta) = monitor(&cfg).unwrap();
        assert_eq!(delta.changed, 1);
        let catalog = catalog.unwrap();
        let item = &catalog.items[&catalog.by_detail[&detail_id]];
        assert_eq!(item.title, "Periodic Title");
        assert_eq!(item.captions.len(), 1);
        assert!(item.album_art > 0);

        std::fs::remove_file(&nfo).unwrap();
        std::fs::remove_file(&caption).unwrap();
        std::fs::remove_file(&poster).unwrap();
        let (catalog, delta) = monitor(&cfg).unwrap();
        assert_eq!(delta.changed, 1);
        let catalog = catalog.unwrap();
        let item = &catalog.items[&catalog.by_detail[&detail_id]];
        assert_eq!(item.title, "movie");
        assert!(item.captions.is_empty());
        assert_eq!(item.album_art, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn art_embedded_and_thumbnail_fallback() {
        let tmp = TempPath::new("embed-art");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let poster = tmp.join("cover.jpg");
        std::fs::write(&poster, TINY_JPEG).unwrap();
        let embedded = tmp.join("withcover.mp4");
        let mk = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x64:rate=2",
                "-i",
            ])
            .arg(&poster)
            .args([
                "-map",
                "0",
                "-map",
                "1",
                "-c:v:0",
                "libx264",
                "-pix_fmt:v:0",
                "yuv420p",
                "-c:v:1",
                "mjpeg",
                "-disposition:1",
                "attached_pic",
            ])
            .arg(&embedded)
            .status();
        if !mk.map(|s| s.success()).unwrap_or(false) {
            eprintln!("skip embedded art (could not mux attached_pic)");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        assert!(
            attached_pic_stream(&embedded).is_some(),
            "fixture must expose attached_pic"
        );
        let bare = tmp.join("bare.mkv");
        write_fake_mkv(&bare, 64);
        let dbp = tmp.join("files.db");
        let cat = scan(&ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(dbp),
            types: MediaTypes::video_only(),
            ..Default::default()
        })
        .unwrap();
        let cover = cat
            .items
            .values()
            .find(|i| i.path.ends_with("withcover.mp4"))
            .expect("embedded");
        assert!(cover.album_art > 0, "attached pic must become ALBUM_ART");
        let art_path = cat.album_art_paths.get(&cover.album_art).expect("art path");
        let bytes = std::fs::read(art_path).unwrap_or_default();
        assert!(is_jpeg_bytes(&bytes), "extracted art must be jpeg");
        let thumb = cat
            .items
            .values()
            .find(|i| i.path.ends_with("bare.mkv"))
            .expect("bare");
        assert!(
            thumb.album_art > 0,
            "video without sidecar/embed gets a thumbnail"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_empty_dirty_attaches_new_poster() {
        let tmp = TempPath::new("art-restat");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write_fake_mkv(&tmp.join("clip.mkv"), 64);
        let dbp = tmp.join("files.db");
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(dbp.clone()),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg).unwrap();
        let before = cat
            .items
            .values()
            .find(|i| i.path.ends_with("clip.mkv"))
            .expect("clip");
        let before_art = before.album_art;
        std::fs::write(tmp.join("clip-poster.jpg"), TINY_JPEG).unwrap();
        let (cat2, delta) = monitor(&cfg).unwrap();
        let cat2 = cat2.expect("restat must notice new art");
        let after = cat2
            .items
            .values()
            .find(|i| i.path.ends_with("clip.mkv"))
            .expect("clip after restat");
        assert!(
            after.album_art > 0,
            "periodic/startup monitor attaches poster"
        );
        assert!(
            delta.changed >= 1 || after.album_art != before_art,
            "sidecar must attach or replace a generated thumb"
        );
        let (none, delta2) = monitor(&cfg).unwrap();
        assert!(none.is_none(), "second restat must not rewrite: {delta2:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn symlink_clones_known_detail_without_second_probe() {
        let tmp = TempPath::new("clone");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("movies")).unwrap();
        std::fs::create_dir_all(tmp.join("genre")).unwrap();
        write_fake_mkv(&tmp.join("movies/film.mkv"), 64);
        std::fs::write(tmp.join("movies/film.nfo"), "<year>1999</year>").unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        let orig = c1.items.values().find(|i| i.title == "film").unwrap();
        assert!(
            orig.date.contains("1999"),
            "nfo date on original {}",
            orig.date
        );
        std::os::unix::fs::symlink(tmp.join("movies/film.mkv"), tmp.join("genre/film-link.mkv"))
            .unwrap();
        let (next, d) = rescan(&cfg, &c1).unwrap();
        assert!(d.added >= 1);
        let alias = next
            .items
            .values()
            .find(|i| i.path.ends_with("film-link.mkv"))
            .expect("symlink alias row");
        assert_eq!(alias.date, orig.date, "cloned DATE, not re-read nfo");
        assert_eq!(alias.mime, orig.mime);
        assert_eq!(alias.device, orig.device);
        assert_eq!(alias.inode, orig.inode);
        assert_ne!(alias.detail_id, orig.detail_id);
        let all_video: Vec<_> = next
            .items
            .values()
            .filter(|i| i.parent_id == VIDEO_ALL_ID)
            .collect();
        assert_eq!(all_video.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sqlite_files_db_roundtrip_and_delete_original_drops_symlinks() {
        let tmp = TempPath::new("sql");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let orig = tmp.join("video/orig.mkv");
        write_fake_mkv(&orig, 128);
        let link = tmp.join("video/link.mkv");
        std::os::unix::fs::symlink(&orig, &link).unwrap();
        let dbp = tmp.join("files.db");
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(dbp.clone()),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg).unwrap();
        assert!(cat.items.values().any(|i| i.path.ends_with("orig.mkv")));
        assert!(cat.items.values().any(|i| i.path.ends_with("link.mkv")));
        assert!(dbp.is_file(), "files.db must exist on disk");

        std::fs::remove_file(&orig).unwrap();
        // dangling symlink
        assert!(path_is_symlink(&link));
        assert!(!path_is_live_file(&link));

        let db = LibraryDb::open(&dbp).unwrap();
        let n = db
            .remove_path_and_symlink_aliases(&orig.to_string_lossy())
            .unwrap();
        assert!(n >= 1);
        let cat2 = db.load_catalog().unwrap();
        assert!(!cat2
            .items
            .values()
            .any(|i| i.title == "orig" || i.title == "link"));

        // rescan also prunes
        write_fake_mkv(&orig, 128);
        std::os::unix::fs::symlink(&orig, tmp.join("video/link2.mkv")).ok();
        let _ = scan(&cfg).unwrap();
        std::fs::remove_file(&orig).unwrap();
        let cat3 = scan(&cfg).unwrap();
        assert!(
            !cat3.items.values().any(|i| i.path.ends_with("link2.mkv")),
            "rescan must drop dangling symlink aliases"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn removing_old_path_keeps_live_symlink_aliases() {
        let tmp = TempPath::new("live-alias");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("old")).unwrap();
        std::fs::create_dir_all(tmp.join("action")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/action")).unwrap();
        write_fake_mkv(&tmp.join("old/film.mkv"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg).unwrap();
        std::fs::rename(tmp.join("old/film.mkv"), tmp.join("action/film.mkv")).unwrap();
        std::os::unix::fs::symlink(
            tmp.join("action/film.mkv"),
            tmp.join("genres/action/film.mkv"),
        )
        .unwrap();
        let _ = monitor(&cfg).unwrap();
        let n = forget_path(&cfg, &tmp.join("old/film.mkv")).unwrap();
        let _ = n;
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let cat = db.load_catalog().unwrap();
        assert!(
            cat.items
                .values()
                .any(|i| i.path.ends_with("action/film.mkv")),
            "moved file must stay"
        );
        assert!(
            cat.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("genres/action/film.mkv")),
            "live genre symlink must survive deleting the old path"
        );
        assert!(
            !cat.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("/old/")),
            "old path must be gone"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rescan_detects_new_and_removed() {
        let tmp = TempPath::new("rescan");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        write_fake_mkv(&tmp.join("video/a.mkv"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        write_fake_mkv(&tmp.join("video/b.mkv"), 64);
        let (c2, d) = rescan(&cfg, &c1).unwrap();
        assert!(d.added >= 1);
        assert!(c2.items.values().any(|i| i.title == "b"));
        std::fs::remove_file(tmp.join("video/b.mkv")).unwrap();
        let (c3, d2) = rescan(&cfg, &c2).unwrap();
        assert!(d2.removed >= 1);
        assert!(!c3.items.values().any(|i| i.title == "b"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn video_has_all_recent_and_folders() {
        let tmp = TempPath::new("video-views");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("movies")).unwrap();
        write_fake_mkv(&tmp.join("movies/fresh.mkv"), 64);
        write_fake_mkv(&tmp.join("movies/stale.mkv"), 64);
        let stale = tmp.join("movies/stale.mkv");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(400 * 86_400);
        let _ = std::fs::File::open(&stale).and_then(|f| f.set_modified(old));
        std::os::unix::fs::symlink(
            tmp.join("movies/fresh.mkv"),
            tmp.join("movies/fresh-alias.mkv"),
        )
        .unwrap();
        let cat = scan(&ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        })
        .unwrap();
        let video = cat.children_of(VIDEO_ID).expect("video");
        let titles: Vec<_> = video
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Container(x) => Some(x.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            titles,
            [
                "Actor",
                "All Video",
                "Folders",
                "Genre",
                "Playlists",
                "Rating",
                "Recently Added",
                "Series"
            ]
        );
        assert!(cat.containers.contains_key(VIDEO_DIR_ID));
        let folders = cat.children_of(VIDEO_DIR_ID).expect("folders");
        assert!(
            !folders.is_empty(),
            "Video/Folders must list the media tree"
        );
        let recent = cat.children_of(VIDEO_RECENT_ID).expect("recent");
        let recent_paths: Vec<_> = recent
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Item(i) => Some(i.path.display().to_string()),
                _ => None,
            })
            .collect();
        assert!(
            recent_paths.iter().any(|p| p.ends_with("fresh.mkv")),
            "recent={recent_paths:?}"
        );
        assert!(
            recent_paths.iter().any(|p| p.ends_with("stale.mkv")),
            "no time window: old files stay in recent: {recent_paths:?}"
        );
        assert_eq!(
            recent_paths
                .iter()
                .filter(|p| p.ends_with("fresh.mkv") || p.contains("fresh"))
                .count(),
            1,
            "symlink alias must not duplicate recent: {recent_paths:?}"
        );
        assert!(
            !recent_paths.iter().any(|p| p.contains("alias")),
            "recent must not list symlink alias: {recent_paths:?}"
        );
        let restarted = load_existing(&ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            recent_limit: 1,
            recent_days: Some(90),
            ..Default::default()
        });
        let restarted_recent = restarted.recent_videos();
        assert_eq!(restarted_recent.len(), 1);
        let CatalogChild::Item(restarted_item) = &restarted_recent[0] else {
            panic!("recent must contain an item");
        };
        assert!(restarted_item.path.ends_with("fresh.mkv"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn recent_is_200_unique_inodes_no_time_window() {
        let mut cat = Catalog::new();
        for i in 0..250i64 {
            let oid = format!("64$1${i:X}");
            cat.items.insert(
                oid.clone(),
                MediaItem {
                    object_id: oid,
                    parent_id: "64$1".into(),
                    detail_id: i + 1,
                    title: format!("m{i:03}"),
                    class: "item.videoItem".into(),
                    date: "2024-01-01".into(),
                    path: PathBuf::from(format!("/m/{i}.mkv")),
                    mime: "video/x-matroska".into(),
                    ext: "mkv".into(),
                    size: 1000,
                    mtime: 1_700_000_000 + i,
                    captions: vec![],
                    probe: SourceProbe::default(),
                    dlna_pn: None,
                    ref_id: None,
                    device: 8,
                    inode: 10_000 + i as u64,
                    duration: None,
                    bitrate: None,
                    resolution: None,
                    channels: None,
                    samplerate: None,
                    album_art: 0,
                    creator: None,
                    comment: None,
                    artist: None,
                    album_artist: None,
                    composer: None,
                    contributor: None,
                    album: None,
                    genre: None,
                    disc: None,
                    track: None,
                    rating: None,
                    rotation: None,
                    bookmark_sec: 0,
                    watch_count: 0,
                },
            );
            let alias = format!("64$2${i:X}");
            cat.items.insert(
                alias.clone(),
                MediaItem {
                    object_id: alias,
                    parent_id: "64$2".into(),
                    detail_id: 1_000 + i,
                    title: format!("m{i:03}-alias"),
                    class: "item.videoItem".into(),
                    date: "2024-01-01".into(),
                    path: PathBuf::from(format!("/genre/{i}.mkv")),
                    mime: "video/x-matroska".into(),
                    ext: "mkv".into(),
                    size: 1000,
                    mtime: 1_700_000_000 + i,
                    captions: vec![],
                    probe: SourceProbe::default(),
                    dlna_pn: None,
                    ref_id: None,
                    device: 8,
                    inode: 10_000 + i as u64,
                    duration: None,
                    bitrate: None,
                    resolution: None,
                    channels: None,
                    samplerate: None,
                    album_art: 0,
                    creator: None,
                    comment: None,
                    artist: None,
                    album_artist: None,
                    composer: None,
                    contributor: None,
                    album: None,
                    genre: None,
                    disc: None,
                    track: None,
                    rating: None,
                    rotation: None,
                    bookmark_sec: 0,
                    watch_count: 0,
                },
            );
        }
        let recent = cat.recent_videos();
        assert_eq!(recent.len(), 200, "cap is 200 unique movies");
        let mut inodes = std::collections::HashSet::new();
        for ch in &recent {
            let CatalogChild::Item(it) = ch else {
                panic!("recent must be items");
            };
            assert!(
                inodes.insert((it.device, it.inode)),
                "duplicate inode in recent {}",
                it.title
            );
            assert!(
                !it.title.contains("alias"),
                "kept original not symlink alias: {}",
                it.title
            );
        }
        // newest 200 unique: mtime 1_700_000_000+50 .. +249
        let CatalogChild::Item(first) = &recent[0] else {
            panic!();
        };
        assert_eq!(first.title, "m249");
        let CatalogChild::Item(last) = &recent[199] else {
            panic!();
        };
        assert_eq!(last.title, "m050");

        let now = 1_700_200_000i64;
        let cutoff = now - 86_400;
        cat.items.get_mut("64$1$F9").unwrap().mtime = now + 60; // future clock skew
        cat.items.get_mut("64$2$F9").unwrap().mtime = now + 60;
        cat.items.get_mut("64$1$F8").unwrap().mtime = cutoff * 1_000_000_000; // exact, nanos
        cat.items.get_mut("64$2$F8").unwrap().mtime = cutoff * 1_000_000_000;
        cat.items.get_mut("64$1$F7").unwrap().mtime = cutoff - 1;
        cat.items.get_mut("64$2$F7").unwrap().mtime = cutoff - 1;
        cat.configure_recent_policy_at(10, Some(1), now);
        let bounded = cat.recent_videos();
        assert_eq!(
            bounded.len(),
            2,
            "window boundary + alias dedup: {bounded:#?}"
        );
        let titles: Vec<_> = bounded
            .iter()
            .filter_map(|child| match child {
                CatalogChild::Item(item) => Some(item.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["m249", "m248"]);
    }

    #[test]
    fn monitor_skips_incomplete_and_only_applies_delta() {
        let tmp = TempPath::new("monitor");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("movies")).unwrap();
        std::fs::create_dir_all(tmp.join("incomplete")).unwrap();
        write_fake_mkv(&tmp.join("movies/keep.mkv"), 64);
        write_fake_mkv(&tmp.join("incomplete/wip.mkv"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            exclude_dirs: vec!["incomplete".into()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        assert!(c1.items.values().any(|i| i.title == "keep"));
        assert!(
            !c1.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("incomplete")),
            "incomplete must never be indexed"
        );
        let (none, d0) = monitor(&cfg).unwrap();
        assert!(none.is_none(), "unchanged library must not rewrite");
        assert_eq!(d0, ScanDelta::default());
        write_fake_mkv(&tmp.join("movies/new.mkv"), 64);
        write_fake_mkv(&tmp.join("incomplete/another.mkv"), 64);
        let (some, d) = monitor(&cfg).unwrap();
        let c2 = some.expect("new file");
        assert!(d.added >= 1);
        assert!(c2.items.values().any(|i| i.title == "new"));
        assert!(
            !c2.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("incomplete")),
            "monitor must not pick up incomplete"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_existing_reads_db_without_walking() {
        let tmp = TempPath::new("load");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        write_fake_mkv(&tmp.join("video/kept.mkv"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let scanned = scan(&cfg).unwrap();
        assert!(scanned.items.values().any(|i| i.title == "kept"));
        let loaded = load_existing(&cfg);
        assert!(
            loaded.items.values().any(|i| i.title == "kept"),
            "startup must serve the last files.db without a tree walk"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_media_dir_v_prefix() {
        let (t, p) = parse_media_dir("V,/storage/video");
        assert_eq!(t, MediaTypes::video_only());
        assert_eq!(p, PathBuf::from("/storage/video"));
    }

    #[test]
    fn path_is_under_roots_rejects_escape_symlink() {
        let tmp = TempPath::new("jail");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("video");
        std::fs::create_dir_all(&root).unwrap();
        let inside = root.join("poster.jpg");
        std::fs::write(&inside, b"ok").unwrap();
        assert!(path_is_under_roots(&inside, std::slice::from_ref(&root)));
        let outside = tmp.join("secret.txt");
        std::fs::write(&outside, b"no").unwrap();
        assert!(!path_is_under_roots(&outside, std::slice::from_ref(&root)));
        let link = root.join("escape.jpg");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(path_is_symlink(&link));
        assert!(
            !path_is_under_roots(&link, std::slice::from_ref(&root)),
            "symlink out of media root must fail"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn scanner_jails_directory_links_but_keeps_safe_aliases_and_wide_links_opt_in() {
        let tmp = TempPath::new("tree-jail");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("media");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        write_fake_mkv(&root.join("real/inside.mkv"), 64);
        write_fake_mkv(&outside.join("secret.mkv"), 64);
        std::os::unix::fs::symlink(root.join("real"), root.join("inside-alias")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("outside-alias")).unwrap();
        std::os::unix::fs::symlink(tmp.join("missing"), root.join("broken-alias")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("real/loop-to-root")).unwrap();

        let strict = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let strict_cat = scan(&strict).unwrap();
        assert!(strict_cat
            .items
            .values()
            .any(|item| item.path.ends_with("inside-alias/inside.mkv")));
        assert!(!strict_cat
            .items
            .values()
            .any(|item| item.path.to_string_lossy().contains("outside-alias")));
        assert!(!path_is_allowed_dir(&root.join("outside-alias"), &strict));
        assert!(!path_is_allowed_dir(&root.join("broken-alias"), &strict));

        let wide = ScanConfig {
            media_roots: Vec::new(),
            wide_links: true,
            ..strict.clone()
        };
        let wide_cat = scan(&wide).unwrap();
        assert!(wide_cat
            .items
            .values()
            .any(|item| item.path.ends_with("outside-alias/secret.mkv")));
        assert!(path_is_allowed_file(
            &root.join("outside-alias/secret.mkv"),
            &wide
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn scanner_root_policy_also_jails_nfo_captions_and_artwork() {
        let tmp = TempPath::new("sidecar-jail");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("media");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        write_fake_mkv(&root.join("clip.mkv"), 64);
        std::fs::write(
            outside.join("metadata.nfo"),
            "<movie><title>Escaped</title></movie>",
        )
        .unwrap();
        std::fs::write(outside.join("captions.srt"), "outside subtitle").unwrap();
        std::fs::write(outside.join("poster.jpg"), TINY_JPEG).unwrap();
        std::os::unix::fs::symlink(outside.join("metadata.nfo"), root.join("clip.nfo")).unwrap();
        std::os::unix::fs::symlink(outside.join("captions.srt"), root.join("clip.srt")).unwrap();
        std::os::unix::fs::symlink(outside.join("poster.jpg"), root.join("clip-poster.jpg"))
            .unwrap();

        let strict = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let strict_cat = scan(&strict).unwrap();
        let item = strict_cat
            .items
            .values()
            .find(|item| item.path.ends_with("clip.mkv") && item.ref_id.is_none())
            .unwrap();
        assert_eq!(item.title, "clip");
        assert!(item.captions.is_empty());
        assert_eq!(item.album_art, 0);

        let wide = ScanConfig {
            media_roots: Vec::new(),
            wide_links: true,
            ..strict
        };
        let wide_cat = scan(&wide).unwrap();
        let item = wide_cat
            .items
            .values()
            .find(|item| item.path.ends_with("clip.mkv") && item.ref_id.is_none())
            .unwrap();
        assert_eq!(item.title, "Escaped");
        assert_eq!(item.captions.len(), 1);
        assert!(item.album_art > 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn monitor_removes_an_inside_alias_retargeted_outside_the_root() {
        let tmp = TempPath::new("retarget-jail");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("media");
        let inside = root.join("inside");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        write_fake_mkv(&inside.join("movie.mkv"), 64);
        write_fake_mkv(&outside.join("secret.mkv"), 64);
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&inside, &alias).unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let first = scan(&cfg).unwrap();
        assert!(first
            .items
            .values()
            .any(|item| item.path.ends_with("alias/movie.mkv")));

        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        let (updated, delta) = monitor(&cfg).unwrap();
        assert!(delta.removed > 0);
        let updated = updated.expect("retarget must publish a catalog update");
        assert!(!updated
            .items
            .values()
            .any(|item| item.path.to_string_lossy().contains("/alias/")));
        assert!(!updated.items.values().any(|item| item.title == "secret"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rebase_media_path_uses_explicit_root_alias() {
        let tmp = TempPath::new("rebase");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("storage/video");
        let rel = PathBuf::from("shows/clip.mkv");
        write_fake_mkv(&root.join(&rel), 64);
        let stored = tmp.join("storage/pool/video").join(&rel);
        assert!(!stored.exists(), "stored realpath must be absent");
        let cfg = ScanConfig {
            media_roots: vec![MediaRoot {
                configured_path: root.clone(),
                canonical_path: root.canonicalize().unwrap(),
                key: "root-video".into(),
                display_title: "video".into(),
                types: MediaTypes::video_only(),
                aliases: vec![tmp.join("storage/pool/video")],
            }],
            media_dirs: vec![root.clone()],
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let got = rebase_media_path_for_config(&stored, &cfg);
        assert_eq!(got, root.join(&rel));
        assert!(path_is_live_file(&got));
        let live = root.join(&rel);
        assert_eq!(rebase_media_path_for_config(&live, &cfg), live);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_media_dirs_unions_v_then_a() {
        let (dirs, t) = collect_media_dirs(["V,/storage/video", "A,/storage/audio"]);
        assert_eq!(dirs.len(), 2);
        assert!(t.video, "later A, must not wipe V: {t:?}");
        assert!(t.audio, "A, must be kept: {t:?}");
        assert!(!t.image);
        assert_eq!(dirs[0], PathBuf::from("/storage/video"));
        assert_eq!(dirs[1], PathBuf::from("/storage/audio"));
    }

    #[test]
    fn root_qualified_key_equates_only_explicit_relocation_aliases() {
        let root = PathBuf::from("/storage/video");
        let cfg = ScanConfig {
            media_roots: vec![MediaRoot {
                configured_path: root.clone(),
                canonical_path: root.clone(),
                key: "root-video".into(),
                display_title: "video".into(),
                types: MediaTypes::video_only(),
                aliases: vec![PathBuf::from("/mnt/pool/video")],
            }],
            media_dirs: vec![root.clone()],
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        assert_eq!(
            media_rel_key_for_config(Path::new("/storage/video/Show/ep.mkv"), &cfg),
            "root-video:Show/ep.mkv"
        );
        assert_eq!(
            media_rel_key_for_config(Path::new("/mnt/pool/video/Show/ep.mkv"), &cfg),
            "root-video:Show/ep.mkv"
        );
        assert!(paths_are_same_media(
            "/mnt/pool/video/Show/ep.mkv",
            Path::new("/storage/video/Show/ep.mkv"),
            &cfg
        ));
        assert!(path_is_under_watched(
            "/mnt/pool/video/Show/S01/ep.mkv",
            Path::new("/storage/video/Show"),
            &cfg
        ));
    }

    #[test]
    fn root_qualified_path_normalization_is_reversible_and_collision_free() {
        let roots = [
            ("video-root", "/srv/video", "/mnt/video"),
            ("audio-root", "/srv/audio", "/mnt/audio"),
        ];
        let cfg = ScanConfig {
            media_roots: roots
                .iter()
                .map(|(key, configured, alias)| MediaRoot {
                    configured_path: PathBuf::from(configured),
                    canonical_path: PathBuf::from(configured),
                    key: (*key).into(),
                    display_title: (*key).into(),
                    types: MediaTypes::all(),
                    aliases: vec![PathBuf::from(alias)],
                })
                .collect(),
            media_dirs: roots
                .iter()
                .map(|(_, configured, _)| PathBuf::from(configured))
                .collect(),
            types: MediaTypes::all(),
            ..Default::default()
        };

        let relatives = ["one.mkv", "nested/two.flac", "space name/三.jpg"];
        let mut keys = HashSet::new();
        for (root_key, configured, alias) in roots {
            for relative in relatives {
                let canonical_key =
                    media_rel_key_for_config(&PathBuf::from(configured).join(relative), &cfg);
                let alias_key =
                    media_rel_key_for_config(&PathBuf::from(alias).join(relative), &cfg);
                assert_eq!(canonical_key, alias_key);
                assert!(canonical_key.starts_with(&format!("{root_key}:")));
                assert!(keys.insert(canonical_key), "normalized path collision");
            }
        }
    }

    #[test]
    fn media_root_validation_rejects_missing_duplicate_nested_and_same_basename() {
        let tmp = TempPath::new("root-validation");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("one/child")).unwrap();
        std::fs::create_dir_all(tmp.join("two/child")).unwrap();

        assert!(build_media_roots(["missing"], &tmp)
            .unwrap_err()
            .contains("does not exist"));
        assert!(build_media_roots(["one", "one"], &tmp)
            .unwrap_err()
            .contains("duplicate"));
        assert!(build_media_roots(["one", "one/child"], &tmp)
            .unwrap_err()
            .contains("nested"));
        assert!(build_media_roots(["one/child", "two/child"], &tmp)
            .unwrap_err()
            .contains("distinct directory names"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn per_root_masks_keys_and_persisted_relocation_survive_reconcile() {
        let tmp = TempPath::new("root-identity");
        let old_parent = tmp.join("host");
        let new_parent = tmp.join("container");
        let video = old_parent.join("video");
        let audio = old_parent.join("audio");
        std::fs::create_dir_all(video.join("Show")).unwrap();
        std::fs::create_dir_all(audio.join("Show")).unwrap();
        write_fake_mkv(&video.join("Show/episode.mkv"), 64);
        write_fake_mkv(&audio.join("wrong-video.mkv"), 64);
        let mut flac = b"fLaC".to_vec();
        flac.extend_from_slice(&[0; 48]);
        std::fs::write(audio.join("Show/episode.flac"), &flac).unwrap();
        std::fs::write(video.join("wrong-audio.flac"), &flac).unwrap();
        let db_path = tmp.join("cache/files.db");

        let mut roots = build_media_roots(
            [
                format!("V,{}", video.display()),
                format!("A,{}", audio.display()),
            ],
            &tmp,
        )
        .unwrap();
        load_and_persist_media_root_mappings(&mut roots, &db_path).unwrap();
        let cfg = ScanConfig {
            media_dirs: roots
                .iter()
                .map(|root| root.configured_path.clone())
                .collect(),
            media_roots: roots,
            db_path: Some(db_path.clone()),
            types: MediaTypes::all(),
            ..Default::default()
        };
        assert_ne!(
            media_rel_key_for_config(&video.join("Show/same.name"), &cfg),
            media_rel_key_for_config(&audio.join("Show/same.name"), &cfg),
            "identical relative paths in different roots must remain distinct"
        );
        let first = scan(&cfg).unwrap();
        let titles: HashSet<_> = first
            .items
            .values()
            .map(|item| item.title.as_str())
            .collect();
        assert!(titles.contains("episode"));
        assert!(!titles.contains("wrong-video"));
        assert!(!titles.contains("wrong-audio"));
        let episode_paths: HashSet<_> = first
            .items
            .values()
            .filter(|item| item.title == "episode")
            .map(|item| item.path.clone())
            .collect();
        assert_eq!(episode_paths.len(), 2);

        std::fs::create_dir_all(&new_parent).unwrap();
        std::fs::rename(&video, new_parent.join("video")).unwrap();
        std::fs::rename(&audio, new_parent.join("audio")).unwrap();
        let mut moved_roots = build_media_roots(
            [
                format!("V,{}", new_parent.join("video").display()),
                format!("A,{}", new_parent.join("audio").display()),
            ],
            &tmp,
        )
        .unwrap();
        load_and_persist_media_root_mappings(&mut moved_roots, &db_path).unwrap();
        assert!(
            moved_roots.iter().all(|root| !root.aliases.is_empty()),
            "old host paths must be explicit aliases"
        );
        let moved = ScanConfig {
            media_dirs: moved_roots
                .iter()
                .map(|root| root.configured_path.clone())
                .collect(),
            media_roots: moved_roots,
            db_path: Some(db_path),
            types: MediaTypes::all(),
            ..Default::default()
        };
        let (published, delta) = monitor(&moved).unwrap();
        assert!(
            published.is_none(),
            "relocation alone must not rewrite catalog rows"
        );
        assert_eq!(delta, ScanDelta::default());
        for item in load_existing(&moved).items.values() {
            let live = rebase_media_path_for_config(&item.path, &moved);
            assert!(live.is_file(), "{} did not rebase", item.path.display());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn every_supported_extension_scans_with_canonical_mime_class_and_image_probe() {
        let tmp = TempPath::new("format-map");
        std::fs::create_dir_all(&tmp).unwrap();
        let video_fixture = tmp.join("fixture-video.mkv");
        write_fake_mkv(&video_fixture, 64);
        let audio_fixture = tmp.join("fixture-audio.wav");
        let mut wav = Vec::from(b"RIFF".as_slice());
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8_000u32.to_le_bytes());
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&audio_fixture, wav).unwrap();

        let mut expected = Vec::new();
        for (index, format) in rusty_dlna_protocol::MEDIA_FORMATS.iter().enumerate() {
            let resolved = format.resolve(None);
            let path = tmp.join(format!("format-{index}.{}", format.extension));
            match resolved.kind {
                MediaKind::Video => std::fs::copy(&video_fixture, &path).unwrap(),
                MediaKind::Audio => std::fs::copy(&audio_fixture, &path).unwrap(),
                MediaKind::Image => {
                    std::fs::write(&path, TINY_JPEG).unwrap();
                    TINY_JPEG.len() as u64
                }
            };
            expected.push((path, resolved));

            if format.is_ambiguous() {
                let audio_path = tmp.join(format!("audio-only-{index}.{}", format.extension));
                std::fs::copy(&audio_fixture, &audio_path).unwrap();
                expected.push((audio_path, format.resolve(Some(MediaKind::Audio))));
            }
        }
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::all(),
            ..Default::default()
        };
        let catalog = scan(&cfg).unwrap();
        for (path, resolved) in expected {
            let items: Vec<_> = catalog
                .items
                .values()
                .filter(|item| item.path == path)
                .collect();
            assert!(!items.is_empty(), "{} was not indexed", path.display());
            for item in items {
                assert_eq!(item.mime, resolved.mime, "{}", path.display());
                assert_eq!(item.class, resolved.upnp_class(), "{}", path.display());
                assert_ne!(item.mime, "application/octet-stream");
                if resolved.kind == MediaKind::Image {
                    assert_eq!(item.probe.container, "jpeg");
                    assert!(item.probe.video.is_empty());
                    assert!(item.probe.audio.is_empty());
                    assert!(item.probe.hdr.is_empty());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn container_named<'a>(cat: &'a Catalog, parent: &str, title: &str) -> Option<&'a str> {
        let c = cat.containers.get(parent)?;
        c.children.iter().find_map(|ch| {
            let cc = cat.containers.get(ch)?;
            (cc.title == title).then_some(ch.as_str())
        })
    }

    fn item_titles(cat: &Catalog, parent: &str) -> Vec<String> {
        let Some(c) = cat.containers.get(parent) else {
            return Vec::new();
        };
        let mut t: Vec<String> = c
            .children
            .iter()
            .filter_map(|ch| cat.items.get(ch).map(|i| i.title.clone()))
            .collect();
        t.sort();
        t
    }

    #[test]
    fn dir_symlink_does_not_duplicate_real_folder() {
        let tmp = TempPath::new("dirlink");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("video");
        std::fs::create_dir_all(root.join("kids/Movies/Despicable Me")).unwrap();
        write_fake_mkv(
            &root.join("kids/Movies/Despicable Me/01 - Despicable Me.mkv"),
            64,
        );
        write_fake_mkv(
            &root.join("kids/Movies/Despicable Me/02 - Despicable Me 2.mkv"),
            64,
        );
        std::fs::write(
            root.join("kids/Movies/Despicable Me/01 - Despicable Me.nfo"),
            "<movie><genre>Animation</genre></movie>",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("genres/BY_YEAR/2010/Movies")).unwrap();
        std::os::unix::fs::symlink(
            root.join("kids/Movies/Despicable Me"),
            root.join("genres/BY_YEAR/2010/Movies/Despicable Me"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg).unwrap();
        let kids = container_named(&cat, BROWSEDIR_ID, "video")
            .and_then(|id| container_named(&cat, id, "kids"))
            .and_then(|id| container_named(&cat, id, "Movies"))
            .and_then(|id| container_named(&cat, id, "Despicable Me"))
            .expect("kids/Movies/Despicable Me");
        let year = container_named(&cat, BROWSEDIR_ID, "video")
            .and_then(|id| container_named(&cat, id, "genres"))
            .and_then(|id| container_named(&cat, id, "BY_YEAR"))
            .and_then(|id| container_named(&cat, id, "2010"))
            .and_then(|id| container_named(&cat, id, "Movies"))
            .and_then(|id| container_named(&cat, id, "Despicable Me"))
            .expect("BY_YEAR/2010/Movies/Despicable Me");
        assert_ne!(kids, year, "symlink dir must keep its own folder id");
        assert_eq!(
            item_titles(&cat, kids),
            vec![
                "01 - Despicable Me".to_string(),
                "02 - Despicable Me 2".to_string()
            ]
        );
        assert_eq!(
            item_titles(&cat, year),
            vec![
                "01 - Despicable Me".to_string(),
                "02 - Despicable Me 2".to_string()
            ]
        );
        let animation =
            container_named(&cat, VIDEO_GENRE_ID, "Animation").expect("Animation genre container");
        assert_eq!(
            item_titles(&cat, animation),
            vec!["01 - Despicable Me"],
            "physical video must occur once in a virtual view"
        );

        let rebuilt = rebuild_objects(&cfg).unwrap();
        let kids = container_named(&rebuilt, BROWSEDIR_ID, "video")
            .and_then(|id| container_named(&rebuilt, id, "kids"))
            .and_then(|id| container_named(&rebuilt, id, "Movies"))
            .and_then(|id| container_named(&rebuilt, id, "Despicable Me"))
            .expect("rebuilt kids folder");
        assert_eq!(
            item_titles(&rebuilt, kids).len(),
            2,
            "rebuild must not dump year-alias clones into kids/: {:?}",
            item_titles(&rebuilt, kids)
        );
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        assert!(
            !db.folders_have_duplicate_inodes().unwrap(),
            "no inode+title twice in one folder"
        );
        let alias_detail = db
            .find_detail_by_path(&path_to_db(
                &root.join("genres/BY_YEAR/2010/Movies/Despicable Me/01 - Despicable Me.mkv"),
            ))
            .unwrap()
            .unwrap()
            .id;
        db.upsert_object(
            &format!("{animation}$DEADBEEF"),
            animation,
            "item.videoItem",
            Some(alias_detail),
            "01 - Despicable Me",
            None,
        )
        .unwrap();
        assert!(db.folders_have_duplicate_inodes().unwrap());
        drop(db);

        let (repaired, delta) = repair_objects_if_needed(&cfg).unwrap();
        assert_eq!(delta.removed, 1);
        assert_eq!(
            item_titles(&repaired.unwrap(), animation),
            vec!["01 - Despicable Me"]
        );
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        assert!(!db.folders_have_duplicate_inodes().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_drops_moved_file_and_empty_folder() {
        let tmp = TempPath::new("move");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("old")).unwrap();
        std::fs::create_dir_all(tmp.join("new")).unwrap();
        write_fake_mkv(&tmp.join("old/episode.mkv"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        assert!(c1
            .items
            .values()
            .any(|i| i.path.ends_with("old/episode.mkv")));
        std::fs::rename(tmp.join("old/episode.mkv"), tmp.join("new/episode.mkv")).unwrap();
        let (c2, d) = rescan(&cfg, &c1).unwrap();
        assert!(d.removed >= 1, "move must drop the source: {d:?}");
        assert!(d.added >= 1, "move must index the dest: {d:?}");
        assert!(
            !c2.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("/old/")),
            "source path must leave the catalog"
        );
        assert!(c2
            .items
            .values()
            .any(|i| i.path.ends_with("new/episode.mkv")));
        assert!(
            !c2.containers.values().any(|c| c.title == "old"),
            "empty source folder must be pruned"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn forget_path_matches_host_realpath_prefix() {
        let tmp = TempPath::new("forget");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("mnt/video");
        std::fs::create_dir_all(root.join("Show")).unwrap();
        write_fake_mkv(&root.join("Show/ep.mkv"), 64);
        let old_root = tmp.join("host/z2/video");
        let cfg = ScanConfig {
            media_roots: vec![MediaRoot {
                configured_path: root.clone(),
                canonical_path: root.canonicalize().unwrap(),
                key: "root-video".into(),
                display_title: "video".into(),
                types: MediaTypes::video_only(),
                aliases: vec![old_root.clone()],
            }],
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg).unwrap();
        let dbp = cfg.db_path.as_ref().unwrap();
        {
            let conn = rusqlite::Connection::open(dbp).unwrap();
            conn.execute(
                "UPDATE DETAILS SET PATH = ?1 WHERE PATH = ?2",
                rusqlite::params![
                    old_root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                    root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                ],
            )
            .unwrap();
        }
        let n = forget_path(&cfg, &root.join("Show/ep.mkv")).unwrap();
        assert!(n >= 1, "inotify mount path must delete the realpath row");
        let db = LibraryDb::open(dbp).unwrap();
        let left = db
            .all_detail_stats()
            .unwrap()
            .into_iter()
            .filter(|row| row.path.ends_with("ep.mkv"))
            .count();
        assert_eq!(left, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_does_not_rewrite_equivalent_realpath_prefix() {
        let tmp = TempPath::new("equiv");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("mnt/video");
        std::fs::create_dir_all(root.join("Show")).unwrap();
        write_fake_mkv(&root.join("Show/ep.mkv"), 64);
        let old_root = tmp.join("host/z2/video");
        let cfg = ScanConfig {
            media_roots: vec![MediaRoot {
                configured_path: root.clone(),
                canonical_path: root.canonicalize().unwrap(),
                key: "root-video".into(),
                display_title: "video".into(),
                types: MediaTypes::video_only(),
                aliases: vec![old_root.clone()],
            }],
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg).unwrap();
        {
            let conn = rusqlite::Connection::open(cfg.db_path.as_ref().unwrap()).unwrap();
            conn.execute(
                "UPDATE DETAILS SET PATH = ?1 WHERE PATH = ?2",
                rusqlite::params![
                    old_root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                    root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                ],
            )
            .unwrap();
        }
        let (none, d) = monitor(&cfg).unwrap();
        assert!(
            none.is_none(),
            "same file under host realpath must not be rewritten: {d:?}"
        );
        assert_eq!(d, ScanDelta::default());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn next_child_seq_skips_gaps_so_upsert_cannot_rename_a_folder() {
        let tmp = TempPath::new("seq");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let dbp = tmp.join("files.db");
        let db = LibraryDb::open(&dbp).unwrap();
        db.seed_virtual_containers().unwrap();
        db.upsert_object("64$1", "64", "container.storageFolder", None, "video", None)
            .unwrap();
        db.upsert_object(
            "64$1$1",
            "64$1",
            "container.storageFolder",
            None,
            "keep",
            None,
        )
        .unwrap();
        db.upsert_object(
            "64$1$2",
            "64$1",
            "container.storageFolder",
            None,
            "mid",
            None,
        )
        .unwrap();
        db.upsert_object(
            "64$1$3",
            "64$1",
            "container.storageFolder",
            None,
            "sport",
            None,
        )
        .unwrap();
        drop(db);
        // Delete $1 only. count(*)+1 == 3, which is still "sport".
        {
            let conn = rusqlite::Connection::open(&dbp).unwrap();
            conn.execute("DELETE FROM OBJECTS WHERE OBJECT_ID = '64$1$1'", [])
                .unwrap();
        }
        let db = LibraryDb::open(&dbp).unwrap();
        let next = db.next_child_seq("64$1").unwrap();
        assert_eq!(next, 4, "must be max(2,3)+1, not count(*)+1=3");
        assert!(db.object_exists("64$1$3").unwrap());
        let name = db.object_name("64$1$3").unwrap().unwrap();
        assert_eq!(name, "sport");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn new_folder_after_sibling_delete_does_not_inherit_old_children() {
        let tmp = TempPath::new("id-reuse");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("keep")).unwrap();
        std::fs::create_dir_all(tmp.join("mid")).unwrap();
        std::fs::create_dir_all(tmp.join("sport")).unwrap();
        write_fake_mkv(&tmp.join("keep/keep.mkv"), 64);
        write_fake_mkv(&tmp.join("mid/mid.mkv"), 64);
        write_fake_mkv(&tmp.join("sport/game.mkv"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        let sport = c1
            .containers
            .values()
            .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
            .expect("sport folder")
            .clone();
        assert!(c1
            .items
            .values()
            .any(|i| i.parent_id == sport.object_id && i.title == "game"));

        std::fs::remove_file(tmp.join("keep/keep.mkv")).unwrap();
        let _ = std::fs::remove_dir(tmp.join("keep"));
        let (c2, _) = rescan(&cfg, &c1).unwrap();
        let sport2 = c2
            .containers
            .values()
            .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
            .expect("sport survives")
            .clone();
        assert_eq!(sport2.object_id, sport.object_id);

        std::fs::create_dir_all(tmp.join("Fallout.S01.Hybrid.2160p.Remux.DoVi.HDR10Plus.H"))
            .unwrap();
        write_fake_mkv(
            &tmp.join("Fallout.S01.Hybrid.2160p.Remux.DoVi.HDR10Plus.H/ep.mkv"),
            64,
        );
        let (c3, d) = rescan(&cfg, &c2).unwrap();
        assert!(d.added >= 1, "Fallout episode must be added: {d:?}");

        let fallout = c3
            .containers
            .values()
            .find(|c| c.title.starts_with("Fallout.S01") && c.object_id.starts_with("64"))
            .expect("Fallout folder");
        assert_ne!(
            fallout.object_id, sport2.object_id,
            "Fallout must get a new id, not sport's"
        );
        let fallout_titles: Vec<_> = c3
            .items
            .values()
            .filter(|i| i.parent_id == fallout.object_id)
            .map(|i| i.title.as_str())
            .collect();
        assert!(
            fallout_titles.contains(&"ep"),
            "Fallout children={fallout_titles:?}"
        );
        assert!(
            !fallout_titles.contains(&"game"),
            "sport file leaked into Fallout: {fallout_titles:?}"
        );
        let sport3 = c3
            .containers
            .values()
            .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
            .expect("sport must keep its name");
        assert!(
            c3.items
                .values()
                .any(|i| i.parent_id == sport3.object_id && i.title == "game"),
            "sport/game.mkv must stay under sport"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_second_pass_does_not_readd_existing_files() {
        let tmp = TempPath::new("noop");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("action")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/crime")).unwrap();
        write_fake_mkv(&tmp.join("action/film.mkv"), 64);
        std::fs::write(
            tmp.join("action/film.nfo"),
            "<movie><genre>Crime</genre></movie>",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            tmp.join("action/film.mkv"),
            tmp.join("genres/crime/film.mkv"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg).unwrap();
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        db.connection()
            .execute("UPDATE DETAILS SET STREAM_PROBE_REV = 0, GENRE = NULL", [])
            .unwrap();
        drop(db);
        let (first, d1) = monitor(&cfg).unwrap();
        let _ = first;
        let _ = d1;
        let (second, d2) = monitor(&cfg).unwrap();
        assert!(
            second.is_none(),
            "unchanged library must not rewrite: {d2:?}"
        );
        assert_eq!(d2.added, 0, "must not count already-indexed files as adds");
        assert_eq!(d2.removed, 0);
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let reprobed: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM DETAILS WHERE STREAM_PROBE_REV != 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reprobed, 0, "periodic NFO refresh reopened unchanged media");
        let restored_genres: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM DETAILS WHERE GENRE = 'Crime'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            restored_genres, 2,
            "NFO override was not restored to aliases"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn symlink_dir_alias_does_not_steal_original_object() {
        let tmp = TempPath::new("steal");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("action/Now You See Me")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/action")).unwrap();
        write_fake_mkv(&tmp.join("action/Now You See Me/film.mkv"), 64);
        std::os::unix::fs::symlink(
            tmp.join("action/Now You See Me"),
            tmp.join("genres/action/Now You See Me"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        let orig_path = tmp.join("action/Now You See Me/film.mkv");
        let orig = c1
            .items
            .values()
            .find(|i| i.path == orig_path && i.ref_id.is_none())
            .expect("original")
            .clone();
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let alias = tmp.join("genres/action/Now You See Me/film.mkv");
        let folder = ensure_folder_chain(&db, &cfg, &alias)
            .unwrap()
            .expect("genre folder chain");
        assert!(
            folder.contains('$'),
            "genre path must get its own folder id: {folder}"
        );
        assert_ne!(
            folder, orig.parent_id,
            "symlink-dir walk must not collapse onto the original folder"
        );
        assert!(index_one_file(&db, &cfg, &alias, &folder).unwrap());
        let c2 = db.load_catalog().unwrap();
        let still = c2.items.get(&orig.object_id).expect("original object kept");
        assert_eq!(
            still.detail_id, orig.detail_id,
            "alias must not steal the original OBJECTS.DETAIL_ID"
        );
        assert_eq!(still.parent_id, orig.parent_id);
        assert!(
            c2.items.values().any(|i| {
                i.path.ends_with("genres/action/Now You See Me/film.mkv") && i.parent_id == folder
            }),
            "alias should live under the genre folder"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn disc_structure_is_not_indexed() {
        let tmp = TempPath::new("bdmv");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("movie/BDMV/STREAM")).unwrap();
        write_fake_mkv(&tmp.join("movie/title.mkv"), 64);
        write_fake_mkv(&tmp.join("movie/BDMV/STREAM/00001.m2ts"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg).unwrap();
        assert!(cat.items.values().any(|i| i.title == "title"));
        assert!(
            !cat.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("BDMV")),
            "BDMV streams must not be catalogued"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_readds_real_path_kept_only_as_dir_symlink_alias() {
        let tmp = TempPath::new("realias");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("kids/Movies/The Incredibles")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/BY_YEAR/2004/Movies")).unwrap();
        let real = tmp.join("kids/Movies/The Incredibles/02 - Incredibles 2.mkv");
        write_fake_mkv(&real, 64);
        std::os::unix::fs::symlink(
            tmp.join("kids/Movies/The Incredibles"),
            tmp.join("genres/BY_YEAR/2004/Movies/The Incredibles"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg).unwrap();
        let n = forget_path(&cfg, &real).unwrap();
        assert!(n >= 1, "real path row must drop; live alias stays: {n}");
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let after_forget = db.all_detail_stats().unwrap();
        assert!(
            after_forget
                .iter()
                .any(|row| row.path.contains("genres/BY_YEAR")),
            "dir-symlink alias must survive deleting the real path: {after_forget:?}"
        );
        assert!(
            !after_forget.iter().any(|row| row
                .path
                .ends_with("kids/Movies/The Incredibles/02 - Incredibles 2.mkv")),
            "real path must be gone before monitor: {after_forget:?}"
        );
        drop(db);
        let (some, d) = monitor(&cfg).unwrap();
        let _ = some;
        assert!(
            d.added >= 1,
            "monitor must reindex the live real path: {d:?}"
        );
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let rows = db.all_detail_stats().unwrap();
        assert!(
            rows.iter().any(|row| row
                .path
                .ends_with("kids/Movies/The Incredibles/02 - Incredibles 2.mkv")),
            "real path back in DETAILS: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| {
                row.path.contains("genres/BY_YEAR") && row.path.ends_with("02 - Incredibles 2.mkv")
            }),
            "alias path must stay: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_rename_updates_real_path_and_dir_symlink_alias() {
        let tmp = TempPath::new("renalias");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("action/Jason Bourne")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/BY_YEAR/2004/Movies")).unwrap();
        let old = tmp.join("action/Jason Bourne/old.mkv");
        write_fake_mkv(&old, 64);
        std::os::unix::fs::symlink(
            tmp.join("action/Jason Bourne"),
            tmp.join("genres/BY_YEAR/2004/Movies/Jason Bourne"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        assert!(c1
            .items
            .values()
            .any(|i| i.path.ends_with("action/Jason Bourne/old.mkv")));
        assert!(c1
            .items
            .values()
            .any(|i| i.path.to_string_lossy().contains("genres/BY_YEAR")
                && i.path.ends_with("old.mkv")));
        let new = tmp.join("action/Jason Bourne/02 - The Bourne Supremacy.mkv");
        std::fs::rename(&old, &new).unwrap();
        let (c2, d) = rescan(&cfg, &c1).unwrap();
        assert!(d.removed >= 1, "old name must leave: {d:?}");
        assert!(d.added >= 1, "new name must be indexed: {d:?}");
        assert!(
            c2.items.values().any(|i| i
                .path
                .ends_with("action/Jason Bourne/02 - The Bourne Supremacy.mkv")),
            "real folder must list the new name"
        );
        assert!(
            c2.items.values().any(|i| {
                let p = i.path.to_string_lossy();
                p.contains("genres/BY_YEAR") && p.ends_with("02 - The Bourne Supremacy.mkv")
            }),
            "dir-symlink alias must list the new name"
        );
        assert!(
            !c2.items.values().any(|i| i.path.ends_with("old.mkv")),
            "old name must be gone from every alias"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rebuild_objects_drops_missing_details() {
        let tmp = TempPath::new("rebuild-miss");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let keep = tmp.join("video/keep.mkv");
        let gone = tmp.join("video/gone.mkv");
        write_fake_mkv(&keep, 64);
        write_fake_mkv(&gone, 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg).unwrap();
        std::fs::remove_file(&gone).unwrap();
        let _ = rebuild_objects(&cfg).unwrap();
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let rows = db.all_detail_stats().unwrap();
        assert!(
            rows.iter().any(|row| row.path.ends_with("keep.mkv")),
            "live file stays"
        );
        assert!(
            !rows.iter().any(|row| row.path.ends_with("gone.mkv")),
            "missing file must leave DETAILS, not just OBJECTS: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rebuild_objects_keeps_browse_folder_ids() {
        let tmp = TempPath::new("rebuild-ids");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        write_fake_mkv(&tmp.join("video/keep.mkv"), 64);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let first = scan(&cfg).unwrap();
        let before: Vec<String> = first
            .items
            .values()
            .filter(|i| i.path.ends_with("keep.mkv"))
            .map(|i| i.object_id.clone())
            .collect();
        assert!(!before.is_empty(), "scan must index keep.mkv");
        let _ = rebuild_objects(&cfg).unwrap();
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let after = db.load_catalog().unwrap();
        for id in &before {
            assert!(
                after.items.contains_key(id) || after.containers.contains_key(id),
                "rebuild must keep ObjectID {id} (Infuse caches it)"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn probe_from_stored_uses_resolution_and_keeps_empty() {
        let p = probe_from_stored(
            "mkv",
            Some("mkv"),
            Some("hevc"),
            Some("truehd,ac3"),
            Some("1:0:truehd:8,2:1:ac3:6"),
            Some("dv-p8"),
            Some("3840x2160"),
        );
        assert_eq!(p.width, 3840);
        assert_eq!(p.height, 2160);
        assert_eq!(p.audio, "truehd,ac3");
        assert_eq!(primary_codec(&p.audio), "truehd");
        let empty = probe_from_stored("mkv", None, None, None, None, None, None);
        assert!(empty.video.is_empty() && empty.hdr.is_empty() && empty.audio.is_empty());
        assert_eq!(empty.width, 0);
        assert_eq!(empty.container, "mkv");
    }

    #[test]
    fn probe_from_stored_avi_other_is_mpeg4() {
        let p = probe_from_stored(
            "avi",
            Some("avi"),
            Some("other"),
            Some("ac3"),
            Some("1:0:ac3:6"),
            Some("sdr"),
            Some("720x480"),
        );
        assert_eq!(p.video, "mpeg4");
        assert_eq!(p.width, 720);
        assert_eq!(p.height, 480);
        assert_eq!(
            dlna_pn_from_probe(&p.container, &p.video, &p.audio, &p.hdr, p.width, p.height)
                .as_deref(),
            Some("MPEG4_P2_AVI_ASP_L5_SO")
        );
    }

    #[test]
    fn dlna_pn_mkv_hevc_stays_empty_mp4_is_written() {
        assert_eq!(
            dlna_pn_from_probe("mkv", "hevc", "truehd", "dv-p8", 3840, 2160),
            None
        );
        assert_eq!(
            dlna_pn_from_probe("mp4", "h264", "aac", "sdr", 1920, 1080).as_deref(),
            Some("AVC_MP4_MP_HD_AAC_MULT5")
        );
        assert_eq!(
            dlna_pn_from_probe("mp4", "hevc", "eac3", "dv-p8", 3840, 2160).as_deref(),
            Some("HEVC_MP4_BL_Main10_L5_HD1080_AC3")
        );
    }

    #[test]
    fn apply_probe_writes_dlna_pn_and_multi_audio() {
        let db = LibraryDb::open_memory().unwrap();
        let id = db
            .insert_detail(NewDetail {
                path: "/tmp/clip.mp4",
                size: 10,
                timestamp: 1,
                title: "clip",
                date: "2024-01-01",
                mime: "video/mp4",
                device: 1,
                inode: 1,
                dlna_pn: None,
            })
            .unwrap();
        let got = MediaProbe {
            probe: SourceProbe {
                container: "mp4".into(),
                video: "h264".into(),
                hdr: "sdr".into(),
                audio: "aac,ac3".into(),
                audio_streams: "1:0:aac:2,2:1:ac3:6".into(),
                width: 1920,
                height: 800,
            },
            av: AvMeta {
                duration: Some("1:00:00.000".into()),
                resolution: Some("1920x800".into()),
                channels: Some(2),
                samplerate: Some(48000),
                ..AvMeta::default()
            },
            tags: EmbeddedTags::default(),
        };
        apply_probe_to_detail(&db, id, &got).unwrap();
        db.upsert_object("64$1$1", "64$1", "item.videoItem", Some(id), "clip", None)
            .unwrap();
        let cat = db.load_catalog().unwrap();
        let it = cat
            .items
            .values()
            .find(|i| i.detail_id == id)
            .expect("item");
        assert_eq!(it.probe.audio, "aac,ac3");
        assert_eq!(it.probe.audio_streams, "1:0:aac:2,2:1:ac3:6");
        assert_eq!(it.probe.width, 1920);
        assert_eq!(it.probe.height, 800);
        assert_eq!(it.dlna_pn.as_deref(), Some("AVC_MP4_MP_HD_AAC_MULT5"));
    }

    #[test]
    fn sidecar_applies_when_libav_fails() {
        let tmp = TempPath::new("sidecar");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let p = tmp.join("video/dvp7.mp4");
        write_incomplete_mp4(&p, 64 * 1024);
        std::fs::write(
            tmp.join("video/dvp7.probe.toml"),
            "container = \"mkv\"\nvideo = \"hevc\"\nhdr = \"dv-p7\"\naudio = \"truehd\"\n",
        )
        .unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg).unwrap();
        let it = cat
            .items
            .values()
            .find(|i| i.path.ends_with("dvp7.mp4"))
            .expect("indexed");
        assert_eq!(it.probe.hdr, "dv-p7", "{it:?}");
        assert_eq!(it.probe.video, "hevc");
        assert_eq!(it.probe.audio, "truehd");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn backfill_rewrites_avi_other_and_pn() {
        let db = LibraryDb::open_memory().unwrap();
        let id = db
            .insert_detail(NewDetail {
                path: "/media/clip.avi",
                size: 10,
                timestamp: 1,
                title: "clip",
                date: "2024-01-01",
                mime: "video/x-msvideo",
                device: 1,
                inode: 2,
                dlna_pn: None,
            })
            .unwrap();
        db.update_detail_stream(
            id,
            DetailStreamUpdate {
                duration: Some("0:01:00.000"),
                resolution: Some("720x480"),
                channels: Some(2),
                samplerate: Some(48000),
                container: Some("avi"),
                video: Some("other"),
                audio: Some("ac3"),
                hdr: Some("sdr"),
                ..DetailStreamUpdate::default()
            },
        )
        .unwrap();
        let n = db.backfill_derived_stream_fields().unwrap();
        assert!(n >= 1, "expected derived rewrite, n={n}");
        db.upsert_object("64$1$1", "64$1", "item.videoItem", Some(id), "clip", None)
            .unwrap();
        let cat = db.load_catalog().unwrap();
        let it = cat.items.values().find(|i| i.detail_id == id).unwrap();
        assert_eq!(it.probe.video, "mpeg4");
        assert_eq!(it.probe.width, 720);
        assert_eq!(it.dlna_pn.as_deref(), Some("MPEG4_P2_AVI_ASP_L5_SO"));
    }

    #[test]
    fn monitor_updates_replaced_file_and_relinked_aliases() {
        let tmp = TempPath::new("inode-replace");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let a = tmp.join("video/orig.mp4");
        let b = tmp.join("video/alias.mp4");
        write_incomplete_mp4(&a, 64 * 1024);
        std::fs::hard_link(&a, &b).unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg).unwrap();
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let first_a = db
            .find_detail_by_path(&a.to_string_lossy())
            .unwrap()
            .unwrap();
        let first_b = db
            .find_detail_by_path(&b.to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(first_a.size, 64 * 1024);
        assert_eq!(first_a.inode, first_b.inode);

        std::fs::remove_file(&a).unwrap();
        write_incomplete_mp4(&a, 256 * 1024);
        let (_, d) = monitor(&cfg).unwrap();
        assert!(d.changed >= 1, "replace must count as a change: {d:?}");
        let second_a = db
            .find_detail_by_path(&a.to_string_lossy())
            .unwrap()
            .unwrap();
        let second_b = db
            .find_detail_by_path(&b.to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(second_a.size, 256 * 1024, "replaced path must get new size");
        assert_ne!(
            second_a.inode, first_a.inode,
            "replaced path must get new inode"
        );
        assert_eq!(
            second_b.size,
            64 * 1024,
            "untouched hardlink stays on old file"
        );
        assert_eq!(second_b.inode, first_b.inode);

        std::fs::remove_file(&b).unwrap();
        std::fs::hard_link(&a, &b).unwrap();
        let (_, d2) = monitor(&cfg).unwrap();
        assert!(d2.changed >= 1, "relink must update alias: {d2:?}");
        let third_b = db
            .find_detail_by_path(&b.to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(third_b.size, 256 * 1024);
        assert_eq!(third_b.inode, second_a.inode);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn failed_probe_retries_after_size_change() {
        let tmp = TempPath::new("reprobe");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let p = tmp.join("video/growing.mp4");
        write_incomplete_mp4(&p, 64 * 1024);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg).unwrap();
        let it = c1
            .items
            .values()
            .find(|i| i.path.ends_with("growing.mp4"))
            .expect("indexed while incomplete");
        assert!(
            it.probe.hdr.is_empty() && it.duration.is_none(),
            "failed probe must not store fake sdr: {it:?}"
        );
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        assert!(
            db.details_missing_stream_meta().unwrap().is_empty(),
            "a failed attempt must be recorded instead of retried forever"
        );
        let (_, unchanged) = monitor(&cfg).unwrap();
        assert_eq!(
            unchanged,
            ScanDelta::default(),
            "an unchanged failed file must not trigger another probe"
        );
        write_fake_mkv(&p, 0);
        let (c2, d) = rescan(&cfg, &c1).unwrap();
        assert!(
            d.changed >= 1
                || c2
                    .items
                    .values()
                    .any(|i| { i.path.ends_with("growing.mp4") && i.duration.is_some() }),
            "size change must re-probe: {d:?}"
        );
        let it2 = c2
            .items
            .values()
            .find(|i| i.path.ends_with("growing.mp4"))
            .expect("still indexed");
        assert!(
            it2.duration.is_some() && !it2.probe.hdr.is_empty(),
            "finished file must get stream metadata: {it2:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn corrupt_database_is_backed_up_before_fresh_recovery() {
        let tmp = TempPath::new("corrupt-recovery");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("files.db");
        let corrupt = b"this is deliberately not a sqlite database";
        std::fs::write(&path, corrupt).unwrap();

        let db = open_library_db(&path).unwrap();
        assert_eq!(db.detail_count().unwrap(), 0);
        drop(db);
        let backups: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("files.db.corrupt-"))
            })
            .collect();
        assert_eq!(backups.len(), 1, "backup files: {backups:?}");
        assert_eq!(std::fs::read(&backups[0]).unwrap(), corrupt);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn failed_scan_transaction_preserves_previous_catalog_generation() {
        let tmp = TempPath::new("atomic-scan");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("video");
        std::fs::create_dir_all(&root).unwrap();
        write_fake_mkv(&root.join("old.mkv"), 4096);
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let before = scan(&cfg).unwrap();
        assert!(before
            .items
            .values()
            .any(|item| item.path.ends_with("old.mkv")));
        let before_ids: HashSet<_> = before.items.keys().cloned().collect();
        {
            let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
            db.connection()
                .execute_batch(
                    "CREATE TRIGGER fail_new_detail BEFORE INSERT ON DETAILS BEGIN
                       SELECT RAISE(ABORT, 'injected detail failure');
                     END;",
                )
                .unwrap();
        }
        write_fake_mkv(&root.join("new.mkv"), 4096);
        let error = scan_refresh(&cfg).unwrap_err();
        assert!(error.to_string().contains("injected detail failure"));

        let after = load_existing(&cfg);
        let after_ids: HashSet<_> = after.items.keys().cloned().collect();
        assert_eq!(after_ids, before_ids);
        assert!(!after
            .items
            .values()
            .any(|item| item.path.ends_with("new.mkv")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tagged_audio_populates_metadata_views_and_sidecar_precedence() {
        let tmp = TempPath::new("audio-tags");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("music");
        std::fs::create_dir_all(&root).unwrap();
        let audio = root.join("tagged.flac");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.1",
                "-c:a",
                "flac",
                "-metadata",
                "title=Tagged Song",
                "-metadata",
                "artist=Track Artist",
                "-metadata",
                "album_artist=Album Artist",
                "-metadata",
                "album=Tagged Album",
                "-metadata",
                "genre=Jazz",
                "-metadata",
                "composer=Composer Name",
                "-metadata",
                "performer=Guest Name",
                "-metadata",
                "track=3/12",
                "-metadata",
                "disc=2/2",
                "-metadata",
                "date=2024-02-03",
                "-metadata",
                "comment=Tagged comment",
            ])
            .arg(&audio)
            .status()
            .unwrap();
        assert!(status.success());
        let direct = probe_media(&audio).expect("tagged FLAC probe");
        assert_eq!(
            direct.tags.artist.as_deref(),
            Some("Track Artist"),
            "{direct:?}"
        );
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::audio_only(),
            ..Default::default()
        };
        let catalog = scan(&cfg).unwrap();
        let item = catalog
            .items
            .values()
            .find(|item| item.path == audio)
            .expect("tagged audio item");
        assert_eq!(item.title, "Tagged Song");
        assert_eq!(item.artist.as_deref(), Some("Track Artist"));
        assert_eq!(item.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(item.album.as_deref(), Some("Tagged Album"));
        assert_eq!(item.genre.as_deref(), Some("Jazz"));
        assert_eq!(item.composer.as_deref(), Some("Composer Name"));
        assert_eq!(item.contributor.as_deref(), Some("Guest Name"));
        assert_eq!(item.track, Some(3));
        assert_eq!(item.disc, Some(2));
        assert!(item.date.starts_with("2024-02-03"), "{}", item.date);
        for (root_id, title) in [
            (MUSIC_ARTIST_ID, "Track Artist"),
            (MUSIC_ALBUM_ARTIST_ID, "Album Artist"),
            (MUSIC_ALBUM_ID, "Tagged Album"),
            (MUSIC_GENRE_ID, "Jazz"),
            (MUSIC_COMPOSER_ID, "Composer Name"),
            (MUSIC_CONTRIB_ARTIST_ID, "Guest Name"),
        ] {
            let children = catalog.children_of(root_id).unwrap();
            assert!(
                children.iter().any(|child| matches!(
                    child,
                    CatalogChild::Container(container) if container.title == title
                )),
                "missing {title} below {root_id}"
            );
        }

        let nfo = root.join("tagged.nfo");
        std::fs::write(&nfo, "<musicvideo><title>Sidecar Wins</title></musicvideo>").unwrap();
        let (updated, delta) = monitor_dirty(&cfg, &[nfo]).unwrap();
        assert_eq!(delta.changed, 1);
        let updated = updated.unwrap();
        let item = updated
            .items
            .values()
            .find(|item| item.path == audio)
            .expect("updated tagged audio item");
        assert_eq!(item.title, "Sidecar Wins");
        assert_eq!(item.artist.as_deref(), Some("Track Artist"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn video_audio_track_title_never_replaces_filename_or_nfo_title() {
        let tmp = TempPath::new("video-track-title");
        let root = tmp.join("video");
        std::fs::create_dir_all(&root).unwrap();
        let video = root.join("Actual Movie Name.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x64:rate=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:duration=1",
                "-shortest",
                "-threads",
                "1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-metadata:s:a:0",
                "title=MVO [HDRezka Studio]",
                "-metadata",
                "title=Rip_by_M@kSIMus",
            ])
            .arg(&video)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !status {
            eprintln!("skip video track-title policy (ffmpeg fixture unavailable)");
            return;
        }
        std::fs::write(
            video.with_extension("nfo"),
            "<movie><genre>Test Genre</genre></movie>",
        )
        .unwrap();
        let direct = probe_media(&video).expect("video probe");
        assert_eq!(direct.tags.title.as_deref(), Some("Rip_by_M@kSIMus"));

        let db_path = tmp.join("files.db");
        let cfg = ScanConfig {
            media_dirs: vec![root],
            db_path: Some(db_path.clone()),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let catalog = scan(&cfg).unwrap();
        let item = catalog
            .items
            .values()
            .find(|item| item.path == video)
            .expect("video item");
        assert_eq!(item.title, "Actual Movie Name");

        let db = LibraryDb::open(&db_path).unwrap();
        let id = db
            .find_detail_by_path(&path_to_db(&video))
            .unwrap()
            .unwrap()
            .id;
        db.update_detail_title(id, "MVO").unwrap();
        assert_eq!(
            db.update_detail_names_under_root(id, VIDEO_GENRE_ID, "Нарезка")
                .unwrap(),
            1
        );
        db.set_setting("video_title_policy_rev", "2").unwrap();
        drop(db);
        let (repaired, delta) = repair_video_titles_if_needed(&cfg).unwrap();
        assert_eq!(delta.changed, 1);
        let repaired = repaired.unwrap();
        assert_eq!(
            repaired
                .items
                .values()
                .find(|item| item.path == video)
                .unwrap()
                .title,
            "Actual Movie Name"
        );
        let genre = container_named(&repaired, VIDEO_GENRE_ID, "Test Genre").unwrap();
        assert_eq!(item_titles(&repaired, genre), vec!["Actual Movie Name"]);

        std::fs::write(
            video.with_extension("nfo"),
            "<movie><title>Curated NFO Name</title></movie>",
        )
        .unwrap();
        let db = LibraryDb::open(&db_path).unwrap();
        db.update_detail_title(id, "MVO").unwrap();
        db.set_setting("video_title_policy_rev", "0").unwrap();
        drop(db);
        let (repaired, delta) = repair_video_titles_if_needed(&cfg).unwrap();
        assert_eq!(delta.changed, 1);
        assert_eq!(
            repaired
                .unwrap()
                .items
                .values()
                .find(|item| item.path == video)
                .unwrap()
                .title,
            "Curated NFO Name"
        );
    }

    fn jpeg_with_test_exif(base_jpeg: &[u8]) -> Vec<u8> {
        let strings = [
            (0x010f_u16, b"TestCam\0".as_slice()),
            (0x0110_u16, b"Model X\0".as_slice()),
            (0x010e_u16, b"A photo\0".as_slice()),
            (0x0132_u16, b"2024:02:03 04:05:06\0".as_slice()),
            (0x010d_u16, b"Vacation\0".as_slice()),
        ];
        let entry_count = 9_u16;
        let data_start = 8 + 2 + usize::from(entry_count) * 12 + 4;
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&entry_count.to_le_bytes());
        let mut tail = Vec::new();
        for (tag, value) in strings {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&2_u16.to_le_bytes());
            tiff.extend_from_slice(&(value.len() as u32).to_le_bytes());
            let offset = (data_start + tail.len()) as u32;
            tiff.extend_from_slice(&offset.to_le_bytes());
            tail.extend_from_slice(value);
        }
        for (tag, value) in [(0x0100_u16, 2_u32), (0x0101_u16, 4_u32)] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&4_u16.to_le_bytes());
            tiff.extend_from_slice(&1_u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&6_u16.to_le_bytes());
        tiff.extend_from_slice(&0_u16.to_le_bytes());
        tiff.extend_from_slice(&0x4746_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&4_u16.to_le_bytes());
        tiff.extend_from_slice(&0_u16.to_le_bytes());
        let ifd1_offset = (data_start + tail.len()) as u32;
        tiff.extend_from_slice(&ifd1_offset.to_le_bytes());
        tiff.extend_from_slice(&tail);
        let thumbnail_offset = ifd1_offset + 2 + 2 * 12 + 4;
        tiff.extend_from_slice(&2_u16.to_le_bytes());
        for (tag, value) in [
            (0x0201_u16, thumbnail_offset),
            (0x0202_u16, TINY_JPEG.len() as u32),
        ] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&4_u16.to_le_bytes());
            tiff.extend_from_slice(&1_u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        tiff.extend_from_slice(TINY_JPEG);

        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut jpeg = base_jpeg.to_vec();
        let mut segment = vec![0xff, 0xe1];
        segment.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(&app1);
        jpeg.splice(2..2, segment);
        jpeg
    }

    #[test]
    fn jpeg_exif_populates_oriented_metadata_and_image_views() {
        let tmp = TempPath::new("image-exif");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("pictures");
        std::fs::create_dir_all(&root).unwrap();
        let image = root.join("photo.jpg");
        let base = tmp.join("base.jpg");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=2x4:d=0.1",
                "-frames:v",
                "1",
            ])
            .arg(&base)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(&image, jpeg_with_test_exif(&std::fs::read(&base).unwrap())).unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes {
                video: false,
                audio: false,
                image: true,
            },
            ..Default::default()
        };
        let catalog = scan(&cfg).unwrap();
        let item = catalog
            .items
            .values()
            .find(|item| item.path == image)
            .expect("EXIF image");
        assert_eq!(item.title, "A photo");
        assert_eq!(item.comment.as_deref(), Some("A photo"));
        assert_eq!(item.creator.as_deref(), Some("TestCam Model X"));
        assert_eq!(item.date, "2024-02-03T04:05:06Z");
        assert_eq!(item.rotation, Some(90));
        assert_eq!(item.album.as_deref(), Some("Vacation"));
        assert_eq!(item.rating, Some(4));
        assert_eq!(item.resolution.as_deref(), Some("4x2"));
        assert!(item.album_art > 0, "EXIF thumbnail should become album art");
        assert_eq!(
            std::fs::read(catalog.album_art_paths.get(&item.album_art).unwrap()).unwrap(),
            TINY_JPEG
        );
        for (root_id, title) in [
            (IMAGE_DATE_ID, "2024-02-03"),
            (IMAGE_CAMERA_ID, "TestCam Model X"),
            (IMAGE_ALBUM_ID, "Vacation"),
            (IMAGE_RATING_ID, "4"),
        ] {
            let children = catalog.children_of(root_id).unwrap();
            assert!(children.iter().any(|child| matches!(
                child,
                CatalogChild::Container(container) if container.title == title
            )));
        }
        let resized = tmp.join("resized.jpg");
        assert!(scale_jpeg_result(&image, &resized, 20, 20).unwrap());
        let resized_probe = probe_image(&resized).unwrap();
        assert_eq!(
            (resized_probe.probe.width, resized_probe.probe.height),
            (20, 10),
            "resized output must apply EXIF orientation"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn playlists_parse_refresh_and_keep_ids_across_rename() {
        let tmp = TempPath::new("playlists");
        let _ = std::fs::remove_dir_all(&tmp);
        let audio_root = tmp.join("audio");
        let video_root = tmp.join("video");
        let image_root = tmp.join("pictures");
        for root in [&audio_root, &video_root, &image_root] {
            std::fs::create_dir_all(root).unwrap();
        }
        let song = audio_root.join("café.flac");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=0.1",
                "-c:a",
                "flac",
            ])
            .arg(&song)
            .status()
            .unwrap();
        assert!(status.success());
        let movie = video_root.join("movie.mkv");
        write_fake_mkv(&movie, 4096);
        let picture = image_root.join("photo.jpg");
        std::fs::write(&picture, TINY_JPEG).unwrap();
        let outside = tmp.join("outside.mkv");
        write_fake_mkv(&outside, 4096);

        let playlist = audio_root.join("Mixed.m3u");
        let mut latin1 = b"#EXTM3U\ncaf".to_vec();
        latin1.push(0xe9);
        latin1.extend_from_slice(
            b".flac\n../video/movie.mkv\n../video/movie.mkv\n../pictures/photo.jpg\n../outside.mkv\nmissing.mp3\n",
        );
        std::fs::write(&playlist, latin1).unwrap();
        std::fs::write(
            audio_root.join("Ordered.pls"),
            "[playlist]\nFile2=../video/movie.mkv\nFile1=café.flac\nNumberOfEntries=2\n",
        )
        .unwrap();
        std::fs::write(audio_root.join("bad.m3u8"), [0xff, 0xfe]).unwrap();
        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![audio_root.clone(), video_root.clone(), image_root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::all(),
            ..Default::default()
        };
        let catalog = scan(&cfg).unwrap();
        let roots = [MUSIC_PLIST_ID, VIDEO_PLIST_ID, IMAGE_PLIST_ID];
        for root in roots {
            let children = catalog.children_of(root).unwrap();
            assert!(
                children.iter().any(|child| matches!(
                    child,
                    CatalogChild::Container(container) if container.title == "Mixed"
                )),
                "missing Mixed below {root}"
            );
        }
        let mixed = catalog
            .children_of(VIDEO_PLIST_ID)
            .unwrap()
            .into_iter()
            .find_map(|child| match child {
                CatalogChild::Container(container) if container.title == "Mixed" => Some(container),
                _ => None,
            })
            .unwrap();
        let stable_id = mixed.object_id.clone();
        let members = catalog.children_of(&mixed.object_id).unwrap();
        assert_eq!(
            members.len(),
            2,
            "duplicate entries are ordered and preserved"
        );
        assert!(members.iter().all(|member| matches!(
            member,
            CatalogChild::Item(item) if item.path == movie
        )));

        let renamed = audio_root.join("Renamed.m3u");
        std::fs::rename(&playlist, &renamed).unwrap();
        let (updated, delta) = monitor(&cfg).unwrap();
        assert!(delta.changed >= 1);
        let updated = updated.unwrap();
        let renamed_container = updated
            .children_of(VIDEO_PLIST_ID)
            .unwrap()
            .into_iter()
            .find_map(|child| match child {
                CatalogChild::Container(container) if container.title == "Renamed" => {
                    Some(container)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(renamed_container.object_id, stable_id);

        std::fs::write(&renamed, "../pictures/photo.jpg\n").unwrap();
        let (updated, delta) = monitor_dirty(&cfg, std::slice::from_ref(&renamed)).unwrap();
        assert!(delta.changed >= 1);
        let updated = updated.unwrap();
        assert!(!updated.children_of(VIDEO_PLIST_ID).unwrap().iter().any(
            |child| matches!(child, CatalogChild::Container(container) if container.title == "Renamed")
        ));
        assert!(updated.children_of(IMAGE_PLIST_ID).unwrap().iter().any(
            |child| matches!(child, CatalogChild::Container(container) if container.title == "Renamed")
        ));

        std::fs::remove_file(&renamed).unwrap();
        let (updated, delta) = monitor(&cfg).unwrap();
        assert!(delta.changed >= 1);
        let updated = updated.unwrap();
        assert!(!updated
            .containers
            .values()
            .any(|container| container.title == "Renamed"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_survive_scan_restart_caption_and_rename() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let tmp = TempPath::new("nonutf8");
        let media = tmp.join("video");
        std::fs::create_dir_all(&media).unwrap();
        let raw_path = |byte: u8| {
            let mut name = b"clip-".to_vec();
            name.push(byte);
            name.extend_from_slice(b".mkv");
            media.join(OsString::from_vec(name))
        };
        let first = raw_path(0x80);
        let second = raw_path(0x81);
        write_fake_mkv(&first, 64);
        write_fake_mkv(&second, 64);
        let mut caption_name = first.file_stem().unwrap().as_bytes().to_vec();
        caption_name.extend_from_slice(b".en.srt");
        let caption = media.join(OsString::from_vec(caption_name));
        std::fs::write(&caption, "1\n00:00:00,000 --> 00:00:01,000\nhello\n").unwrap();

        let cfg = ScanConfig {
            media_roots: Vec::new(),
            media_dirs: vec![media.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let lossy_first = first.file_name().unwrap().to_string_lossy();
        assert!(is_video(&lossy_first), "media classifier: {lossy_first:?}");
        assert!(!is_unfinished_name(&lossy_first));
        assert!(!is_caption_name(&lossy_first));
        assert!(!is_album_art_name_for_config(&lossy_first, &cfg));
        assert!(!path_excluded(&first, &lossy_first, &cfg));
        assert!(cfg
            .root_types_for_path(&first)
            .is_some_and(|types| types.allows(&lossy_first)));
        assert!(path_is_allowed_file(&first, &cfg));
        assert!(file_is_viable(&first));
        let catalog = scan(&cfg).unwrap();
        let first_item = catalog
            .items
            .values()
            .find(|item| item.ref_id.is_none() && item.path == first)
            .expect("first invalid-byte path");
        let second_item = catalog
            .items
            .values()
            .find(|item| item.ref_id.is_none() && item.path == second)
            .expect("second invalid-byte path");
        assert_ne!(first_item.detail_id, second_item.detail_id);
        assert_ne!(first_item.title, second_item.title);
        assert_eq!(first_item.captions.len(), 1);
        assert_eq!(first_item.captions[0].path, caption);
        assert_eq!(first_item.captions[0].ext, "srt");
        assert_eq!(path_from_db(&path_to_db(&first)), first);
        assert_ne!(path_to_db(&first), path_to_db(&second));

        let restarted = load_existing(&cfg);
        assert!(restarted.items.values().any(|item| item.path == first));
        assert!(restarted.items.values().any(|item| item.path == second));
        assert!(restarted
            .items
            .values()
            .find(|item| item.ref_id.is_none() && item.path == first)
            .is_some_and(|item| item.captions.iter().any(|cap| cap.path == caption)));

        let renamed = raw_path(0x82);
        std::fs::rename(&first, &renamed).unwrap();
        let (updated, delta) = monitor_dirty(&cfg, &[first.clone(), renamed.clone()]).unwrap();
        assert!(delta.added >= 1 && delta.removed >= 1, "{delta:?}");
        let updated = updated.expect("catalog changed");
        assert!(!updated.items.values().any(|item| item.path == first));
        assert!(updated.items.values().any(|item| item.path == second));
        assert!(updated.items.values().any(|item| item.path == renamed));

        let reserved = PathBuf::from(format!("{PATH_HEX_PREFIX}ordinary"));
        assert_eq!(path_from_db(&path_to_db(&reserved)), reserved);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
