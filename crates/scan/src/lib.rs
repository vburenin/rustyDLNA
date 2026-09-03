//! Library scan: skip rules, NFO dates, captions, inode reuse, SQLite store.

#[cfg(not(target_os = "linux"))]
compile_error!("rustyDLNA currently supports Linux only (inotify, /proc, and rooted openat2 I/O)");

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

mod catalog;
pub mod db;
mod metadata;
pub mod nfo;
mod playlist;
pub mod probe;
mod rooted_io;
pub mod watch;
pub use catalog::*;
pub use db::{
    mime_to_ext, CatalogDefaultOrder, CatalogQuery, CatalogQueryClause, CatalogQueryField,
    CatalogQueryOp, CatalogQueryPage, CatalogQuerySort, DetailStat, DetailStreamUpdate,
    ExistingDetail, InodeSource, LibraryDb, NewDetail, WebMediaKind, WebMediaSort,
};
pub use metadata::*;
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
    probe_image_with_cancellation, probe_image_with_timeout, probe_media,
    probe_media_with_cancellation, probe_media_with_timeout, scale_jpeg,
    scale_jpeg_file_with_options_cancelled_result, scale_jpeg_file_with_options_result,
    scale_jpeg_result, scale_jpeg_with_options_result, MediaHelperControl, MediaProbe,
};
#[cfg(test)]
pub(crate) use rooted_io::PATH_HEX_PREFIX;
pub use rooted_io::{
    media_rel_key_for_config, open_allowed_file, open_file_under_roots, path_from_db,
    path_is_allowed_dir, path_is_allowed_file, path_is_live_file, path_is_under_roots, path_to_db,
    rebase_media_path_for_config, RootedFile,
};
pub use rusty_dlna_helper::{
    CancellationToken, HelperAdmissionError, HelperGate, HelperMetrics, HelperPermit,
};
pub use watch::{
    repair_objects_if_needed, run_inotify, run_inotify_prepared_updates_until, run_inotify_until,
    run_inotify_updates_until, WatchTelemetry,
};

use rusty_dlna_protocol::object_id::{
    BROWSEDIR_ID, IMAGE_ALBUM_ID, IMAGE_ALL_ID, IMAGE_CAMERA_ID, IMAGE_DATE_ID, IMAGE_DIR_ID,
    IMAGE_ID, IMAGE_PLIST_ID, IMAGE_RATING_ID, IMAGE_RECENT_ID, MUSIC_ALBUM_ARTIST_ID,
    MUSIC_ALBUM_ID, MUSIC_ALL_ID, MUSIC_ARTIST_ID, MUSIC_COMPOSER_ID, MUSIC_CONTRIB_ARTIST_ID,
    MUSIC_DIR_ID, MUSIC_GENRE_ID, MUSIC_ID, MUSIC_PLIST_ID, MUSIC_RATING_ID, MUSIC_RECENT_ID,
    RECENT_MAX, ROOT_ID, VIDEO_ACTOR_ID, VIDEO_ALL_ID, VIDEO_DIR_ID, VIDEO_GENRE_ID, VIDEO_ID,
    VIDEO_PLIST_ID, VIDEO_RATING_ID, VIDEO_RECENT_ID, VIDEO_SERIES_ID,
};
use rusty_dlna_protocol::w3c_date_from_unix;
use rusty_dlna_protocol::{
    caption_format_for_extension, caption_format_for_name, caption_format_for_os_name,
    is_trickplay_directory_name, media_format_for_extension, media_format_for_name,
    CompactStreamMetadata, MediaKind, ResolvedMediaFormat,
};

/// Stable UI text for a filesystem name. Invalid UTF-8 is lossy for display
/// only, with a raw-byte digest so distinct names cannot collapse together.
fn display_os_name(name: &OsStr) -> String {
    if let Some(text) = name.to_str() {
        return text.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(name.as_encoded_bytes());
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
/// (`sample/` not `Sample/`).
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
    is_junk_dir(name)
        || is_sample_or_trailer_dir(name)
        || is_disc_structure_dir(name)
        || is_trickplay_directory_name(OsStr::new(name))
}

/// Raw-name counterpart used by directory walkers so generated preview
/// directories remain excluded even when a media stem is not valid UTF-8.
pub fn is_skipped_dir_os_name(name: &OsStr) -> bool {
    is_trickplay_directory_name(name) || name.to_str().is_some_and(is_skipped_dir)
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
            .any(|component| is_skipped_dir_os_name(component.as_os_str()))
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
    caption_format_for_name(name).is_some()
}

pub fn caption_ext(name: &str) -> &'static str {
    caption_format_for_name(name)
        .map(|format| format.extension)
        .unwrap_or("sub")
}

pub fn caption_http_mime(ext: &str) -> &'static str {
    caption_format_for_extension(ext)
        .map(|format| format.http_mime)
        .unwrap_or("text/plain")
}

/// Match an ASCII suffix without requiring the filesystem name before it to
/// be valid UTF-8. Media extensions and sidecar suffixes are ASCII file
/// conventions even when a user's title bytes are not.
fn ends_with_ci(name: &OsStr, suffix: &str) -> bool {
    let name = name.as_encoded_bytes();
    let suffix = suffix.as_bytes();
    name.len() >= suffix.len() && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn is_image_os_name(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .and_then(media_format_for_extension)
        .is_some_and(|format| format.image_mime.is_some())
}

fn is_caption_os_name(name: &OsStr) -> bool {
    caption_format_for_os_name(name).is_some()
}

/// Return whether rustyDLNA classifies the name as video.
pub fn is_video(name: &str) -> bool {
    media_format_for_name(name).is_some_and(|format| format.video_mime.is_some())
}

/// Return whether rustyDLNA classifies the name as audio.
pub fn is_audio(name: &str) -> bool {
    media_format_for_name(name).is_some_and(|format| format.audio_mime.is_some())
}

/// Return whether rustyDLNA classifies the name as an image (JPEG only).
pub fn is_image(name: &str) -> bool {
    media_format_for_name(name).is_some_and(|format| format.image_mime.is_some())
}

pub fn is_album_art_name(name: &str) -> bool {
    is_builtin_album_art_os_name(OsStr::new(name))
}

/// JPEG SOI: `FF D8 FF`.
pub fn is_jpeg_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff
}

/// Configured names take precedence, followed by built-in stem poster/fanart
/// and folder poster/folder/cover name variants.
const STEM_ART_SUFFIXES: &[&str] = &[
    "-poster.jpg",
    "-poster.jpeg",
    "-poster.png",
    "-fanart.jpg",
    "-fanart.jpeg",
    "-fanart.png",
];

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
    "albumart.jpg",
    "albumart.jpeg",
    "albumart.png",
    "albumartsmall.jpg",
    "albumartsmall.jpeg",
    "albumartsmall.png",
    "album.jpg",
    "album.jpeg",
    "album.png",
    "thumb.jpg",
    "thumb.jpeg",
    "thumb.png",
];

fn os_str_eq_ci(left: &OsStr, right: &OsStr) -> bool {
    left.as_encoded_bytes()
        .eq_ignore_ascii_case(right.as_encoded_bytes())
}

fn is_builtin_album_art_os_name(name: &OsStr) -> bool {
    STEM_ART_SUFFIXES
        .iter()
        .any(|suffix| ends_with_ci(name, suffix))
        || FOLDER_ART_NAMES
            .iter()
            .any(|candidate| os_str_eq_ci(name, OsStr::new(candidate)))
}

fn valid_configured_art_pattern(pattern: &str) -> bool {
    // Names are basenames by contract. Validation rejects separators, but
    // retain this guard for embedders that construct ScanConfig directly.
    Path::new(pattern).file_name().and_then(OsStr::to_str) == Some(pattern)
}

fn configured_art_pattern_parts(pattern: &str) -> Option<Vec<&str>> {
    if !valid_configured_art_pattern(pattern) {
        return None;
    }
    let mut parts = Vec::new();
    let mut rest = pattern;
    loop {
        let placeholder = ["{stem}", "%s"]
            .into_iter()
            .filter_map(|token| rest.find(token).map(|offset| (offset, token)))
            .min_by_key(|(offset, _)| *offset);
        let Some((offset, token)) = placeholder else {
            parts.push(rest);
            return Some(parts);
        };
        parts.push(&rest[..offset]);
        rest = &rest[offset + token.len()..];
    }
}

fn configured_art_name(pattern: &str, stem: Option<&OsStr>) -> Option<OsString> {
    let parts = configured_art_pattern_parts(pattern)?;
    let mut expanded = OsString::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            expanded.push(stem?);
        }
        expanded.push(part);
    }
    Some(expanded)
}

fn configured_art_pattern_matches(name: &OsStr, pattern: &str) -> bool {
    let Some(parts) = configured_art_pattern_parts(pattern) else {
        return false;
    };
    let placeholder_count = parts.len().saturating_sub(1);
    if placeholder_count == 0 {
        return os_str_eq_ci(name, OsStr::new(pattern));
    }
    let bytes = name.as_encoded_bytes();
    let literal_len: usize = parts.iter().map(|part| part.len()).sum();
    let Some(variable_len) = bytes.len().checked_sub(literal_len) else {
        return false;
    };
    if !variable_len.is_multiple_of(placeholder_count) {
        return false;
    }
    let stem_len = variable_len / placeholder_count;
    let mut cursor = 0;
    let mut expected_stem: Option<&[u8]> = None;
    for (index, part) in parts.iter().enumerate() {
        let end = cursor + part.len();
        if !bytes[cursor..end].eq_ignore_ascii_case(part.as_bytes()) {
            return false;
        }
        cursor = end;
        if index < placeholder_count {
            let end = cursor + stem_len;
            let stem = &bytes[cursor..end];
            if expected_stem.is_some_and(|expected| !stem.eq_ignore_ascii_case(expected)) {
                return false;
            }
            expected_stem.get_or_insert(stem);
            cursor = end;
        }
    }
    cursor == bytes.len()
}

fn album_art_candidate_names(media_path: &Path, configured: &[String]) -> Vec<OsString> {
    let stem = media_path.file_stem();
    let mut candidates: Vec<OsString> = configured
        .iter()
        .filter_map(|pattern| configured_art_name(pattern, stem))
        .collect();
    if let Some(stem) = stem {
        for suffix in STEM_ART_SUFFIXES {
            let mut name = stem.to_os_string();
            name.push(suffix);
            candidates.push(name);
        }
    }
    candidates.extend(FOLDER_ART_NAMES.iter().map(OsString::from));
    candidates
}

#[derive(Debug, Default)]
struct ArtworkInventory {
    exact: HashMap<OsString, PathBuf>,
    ascii_folded: HashMap<Vec<u8>, PathBuf>,
    #[cfg(test)]
    probes: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Default)]
struct ArtworkSelectionCache {
    local: HashMap<PathBuf, ArtworkInventory>,
    physical: HashMap<PathBuf, ArtworkInventory>,
}

impl ArtworkSelectionCache {
    fn select(&mut self, media_path: &Path, cfg: &ScanConfig) -> Option<PathBuf> {
        if let Some(parent) = media_path.parent() {
            let parent = parent.to_path_buf();
            if !self.local.contains_key(&parent) {
                let inventory = read_confined_artwork_inventory(&parent, cfg).unwrap_or_default();
                self.local.insert(parent.clone(), inventory);
            }
            if let Some(sidecar) = self.local.get(&parent).and_then(|inventory| {
                find_album_art_in_inventory(media_path, &cfg.album_art_names, inventory)
            }) {
                return Some(sidecar);
            }
        }
        find_album_art_for_physical_target_cached(media_path, cfg, &mut self.physical)
    }
}

impl ArtworkInventory {
    fn new(mut files: Vec<(OsString, PathBuf)>) -> Self {
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let mut inventory = Self::default();
        for (name, path) in files {
            inventory
                .ascii_folded
                .entry(ascii_fold_os_str(&name))
                .or_insert_with(|| path.clone());
            inventory.exact.insert(name, path);
        }
        inventory
    }

    fn get(&self, candidate: &OsStr) -> Option<&PathBuf> {
        #[cfg(test)]
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.exact
            .get(candidate)
            .or_else(|| self.ascii_folded.get(&ascii_fold_os_str(candidate)))
    }

    #[cfg(test)]
    fn probe_count(&self) -> usize {
        self.probes.load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn ascii_fold_os_str(name: &OsStr) -> Vec<u8> {
    name.as_encoded_bytes()
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect()
}

fn find_album_art_in_inventory(
    media_path: &Path,
    configured: &[String],
    files: &ArtworkInventory,
) -> Option<PathBuf> {
    for candidate in album_art_candidate_names(media_path, configured) {
        if let Some(path) = files.get(&candidate) {
            return Some(path.clone());
        }
    }
    None
}

fn find_album_art_with_config(video_path: &Path, configured: &[String]) -> Option<PathBuf> {
    let parent = video_path.parent()?;
    #[cfg(test)]
    record_artwork_directory_read(parent);
    let files: Vec<_> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            path.is_file().then(|| (entry.file_name(), path))
        })
        .collect();
    find_album_art_in_inventory(video_path, configured, &ArtworkInventory::new(files))
}

#[cfg(test)]
static ARTWORK_DIRECTORY_READ_PROBES: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

#[cfg(test)]
fn record_artwork_directory_read(dir: &Path) {
    let mut probes = ARTWORK_DIRECTORY_READ_PROBES
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(count) = probes.get_mut(dir) {
        *count += 1;
    }
}

#[cfg(test)]
fn reset_artwork_directory_read_count(dir: &Path) {
    ARTWORK_DIRECTORY_READ_PROBES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(dir.to_path_buf(), 0);
}

#[cfg(test)]
fn take_artwork_directory_read_count(dir: &Path) -> usize {
    ARTWORK_DIRECTORY_READ_PROBES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(dir)
        .unwrap_or_default()
}

pub fn find_album_art(video_path: &Path) -> Option<PathBuf> {
    find_album_art_with_config(video_path, &[])
}

fn find_album_art_for_config(video_path: &Path, cfg: &ScanConfig) -> Option<PathBuf> {
    find_album_art_with_config(video_path, &cfg.album_art_names)
}

/// Prefer artwork beside the browsed path, then beside its physical target.
/// The latter lets a symlink-only library entry inherit the poster maintained
/// with the real media file without requiring poster symlinks in every view.
fn find_album_art_for_media_path(video_path: &Path, cfg: &ScanConfig) -> Option<PathBuf> {
    find_album_art_for_config(video_path, cfg)
        .or_else(|| find_album_art_for_physical_target(video_path, cfg))
}

fn find_album_art_for_physical_target(video_path: &Path, cfg: &ScanConfig) -> Option<PathBuf> {
    let mut inventories = HashMap::new();
    find_album_art_for_physical_target_cached(video_path, cfg, &mut inventories)
}

fn find_album_art_for_physical_target_cached(
    video_path: &Path,
    cfg: &ScanConfig,
    inventories: &mut HashMap<PathBuf, ArtworkInventory>,
) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(video_path).ok()?;
    if canonical == video_path {
        return None;
    }
    let parent = canonical.parent()?.to_path_buf();
    if !inventories.contains_key(&parent) {
        let inventory = read_confined_artwork_inventory(&parent, cfg).unwrap_or_default();
        inventories.insert(parent.clone(), inventory);
    }
    find_album_art_in_inventory(&canonical, &cfg.album_art_names, inventories.get(&parent)?)
}

fn read_confined_artwork_inventory(dir: &Path, cfg: &ScanConfig) -> Option<ArtworkInventory> {
    if !path_is_allowed_dir(dir, cfg) {
        return None;
    }
    #[cfg(test)]
    record_artwork_directory_read(dir);
    let files = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            directory_entry_is_allowed_file(&entry, &path, cfg).then(|| (entry.file_name(), path))
        })
        .collect();
    Some(ArtworkInventory::new(files))
}

pub fn is_album_art_name_for_config(name: &str, cfg: &ScanConfig) -> bool {
    is_album_art_os_name_for_config(OsStr::new(name), cfg)
}

fn is_album_art_os_name_for_config(name: &OsStr, cfg: &ScanConfig) -> bool {
    is_builtin_album_art_os_name(name)
        || cfg
            .album_art_names
            .iter()
            .any(|pattern| configured_art_pattern_matches(name, pattern))
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    lowercase_hex(&Sha256::digest(data))
}

#[cfg(test)]
fn source_image_identity(path: &Path) -> ScanResult<String> {
    let file = std::fs::File::open(path).map_err(|error| scan_io(path, error))?;
    source_image_identity_file(&file, path)
}

fn source_image_identity_file(file: &std::fs::File, identity_path: &Path) -> ScanResult<String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::FileExt;

    let metadata = file
        .metadata()
        .map_err(|error| scan_io(identity_path, error))?;
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(
        std::fs::canonicalize(identity_path)
            .unwrap_or_else(|_| identity_path.to_path_buf())
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
        let read = file
            .read_at(&mut sample, offset)
            .map_err(|error| scan_io(identity_path, error))?;
        hasher.update(offset.to_le_bytes());
        hasher.update(&sample[..read]);
    }
    Ok(lowercase_hex(&hasher.finalize()))
}

fn image_file_within_limits(cfg: &ScanConfig, src: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;

    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", src.as_raw_fd()));
    let Some(image) = crate::probe::probe_image_with_cancellation(
        &proc_path,
        cfg.external_command_timeout,
        &cfg.cancellation,
    ) else {
        return false;
    };
    u64::from(image.probe.width)
        .checked_mul(u64::from(image.probe.height))
        .is_some_and(|pixels| pixels > 0 && pixels <= cfg.image_max_pixels)
}

fn convert_image_file_to_jpeg(
    cfg: &ScanConfig,
    src: &std::fs::File,
    identity_path: &Path,
    dest: &Path,
) -> ScanResult<bool> {
    crate::probe::with_atomic_image_destination(dest, |temporary| {
        let mut command = std::process::Command::new("ffmpeg");
        command
            .args([
                "-nostdin",
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-max_alloc",
            ])
            .arg(cfg.image_memory_limit_bytes.to_string())
            .args(["-threads", "1", "-i", "/proc/self/fd/3"])
            .arg(temporary);
        crate::probe::command_status_with_file_cancellation(
            &mut command,
            src,
            cfg.external_command_timeout,
            &cfg.cancellation,
        )
        .map(|status| status.success())
    })
    .map_err(|error| scan_io(identity_path, error))
}

/// JPEG sidecars stay in place. Anything else is converted once under
/// `{db_path.parent()}/art/{sha256}.jpg`. Memory DBs skip conversion.
pub fn persist_album_art_file(cfg: &ScanConfig, src: &Path) -> ScanResult<Option<PathBuf>> {
    cfg.check_cancelled()?;
    let opened = match open_allowed_file(src, cfg) {
        Ok(opened) => opened,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(scan_io(src, error)),
    };
    let metadata = opened
        .file
        .metadata()
        .map_err(|error| scan_io(src, error))?;
    if metadata.len() == 0 || metadata.len() > cfg.image_memory_limit_bytes {
        return Ok(None);
    }
    let _helper_permit = acquire_scan_helper(cfg)?;
    if !image_file_within_limits(cfg, &opened.file) {
        return Ok(None);
    }
    let mut magic = [0u8; 3];
    use std::os::unix::fs::FileExt;
    let jpeg =
        opened.file.read_at(&mut magic, 0).ok() == Some(magic.len()) && is_jpeg_bytes(&magic);
    if jpeg {
        return Ok(Some(src.to_path_buf()));
    }
    let Some(cache) = cfg.db_path.as_ref().and_then(|path| path.parent()) else {
        return Ok(None);
    };
    let dest = cache.join("art").join(format!(
        "{}.jpg",
        source_image_identity_file(&opened.file, src)?
    ));
    if dest.is_file() {
        return Ok(Some(dest));
    }
    if convert_image_file_to_jpeg(cfg, &opened.file, src, &dest)? {
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
fn prepare_album_art(
    cfg: &ScanConfig,
    path: &Path,
    opened: &RootedFile,
) -> ScanResult<Option<PathBuf>> {
    let sidecar = find_album_art_for_media_path(path, cfg);
    prepare_album_art_with_sidecar(cfg, path, opened, sidecar.as_deref())
}

fn prepare_album_art_with_sidecar(
    cfg: &ScanConfig,
    path: &Path,
    opened: &RootedFile,
    sidecar: Option<&Path>,
) -> ScanResult<Option<PathBuf>> {
    cfg.check_cancelled()?;
    if let Some(src) = sidecar.filter(|candidate| path_is_allowed_file(candidate, cfg)) {
        if let Some(stored) = persist_album_art_file(cfg, src)? {
            return Ok(Some(stored));
        }
    }
    let stamp = if cfg.thumbnails {
        source_image_identity_file(&opened.file, path).ok()
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
        let _helper_permit = acquire_scan_helper(cfg)?;
        let proc_path = opened.proc_path();
        let image_thumb = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_image)
            && extract_exif_thumbnail_with_limit_result(
                &proc_path,
                &dest,
                cfg.image_memory_limit_bytes.min(usize::MAX as u64) as usize,
            )
            .map_err(|error| scan_io(&dest, error))?;
        let generated = dest.is_file()
            || image_thumb
            || crate::probe::extract_attached_pic_file_with_limits_cancelled_result(
                &opened.file,
                &dest,
                cfg.external_command_timeout,
                cfg.image_memory_limit_bytes,
                &cfg.cancellation,
            )
            .map_err(|error| scan_io(&dest, error))?;
        if generated && dest.is_file() {
            return Ok(Some(dest));
        }
    }
    if let Some(dest) = thumbnail_dest {
        let _helper_permit = acquire_scan_helper(cfg)?;
        let generated = crate::probe::generate_video_thumb_file_with_limits_cancelled_result(
            &opened.file,
            &dest,
            cfg.thumbnail_width,
            cfg.thumbnail_quality,
            cfg.thumbnail_filmstrip,
            MediaHelperControl {
                timeout: cfg.external_command_timeout,
                max_alloc_bytes: cfg.image_memory_limit_bytes,
                cancellation: &cfg.cancellation,
            },
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

#[derive(Clone, Debug)]
enum PreparedAlbumArt {
    Ready(Option<PathBuf>),
    /// A per-file helper or filesystem failure must not clear artwork already
    /// attached to a published detail or abort an otherwise valid scan.
    PreserveExisting,
}

fn recover_prepared_album_art(
    cfg: &ScanConfig,
    path: &Path,
    prepared: ScanResult<Option<PathBuf>>,
) -> ScanResult<PreparedAlbumArt> {
    match prepared {
        Ok(stored) => Ok(PreparedAlbumArt::Ready(stored)),
        Err(error) if recoverable_artwork_error(cfg, &error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "artwork preparation failed; preserving existing artwork and continuing"
            );
            Ok(PreparedAlbumArt::PreserveExisting)
        }
        Err(error) => Err(error),
    }
}

fn apply_index_album_art(
    db: &LibraryDb,
    detail_id: i64,
    prepared: &PreparedAlbumArt,
) -> ScanResult<bool> {
    match prepared {
        PreparedAlbumArt::Ready(stored) => {
            apply_prepared_album_art(db, detail_id, stored.as_deref())
        }
        PreparedAlbumArt::PreserveExisting => Ok(false),
    }
}

fn attach_album_art_with_sidecar(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    detail_id: i64,
    sidecar: Option<&Path>,
) -> ScanResult<bool> {
    let opened = match open_allowed_file(path, cfg) {
        Ok(opened) => opened,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(scan_io(path, error)),
    };
    let stored = prepare_album_art_with_sidecar(cfg, path, &opened, sidecar)?;
    apply_prepared_album_art(db, detail_id, stored.as_deref())
}

fn attach_album_art_for_index(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    detail_id: i64,
    prepared: Option<&PreparedPhysicalFile>,
    selection: IndexArtworkSelection<'_>,
    opened: &RootedFile,
) -> ScanResult<bool> {
    match prepared {
        Some(prepared) => apply_index_album_art(db, detail_id, &prepared.album_art),
        None => {
            let prepared = match selection {
                IndexArtworkSelection::Discover => prepare_album_art(cfg, path, opened),
                IndexArtworkSelection::Selected(sidecar) => {
                    prepare_album_art_with_sidecar(cfg, path, opened, sidecar)
                }
            };
            let prepared = recover_prepared_album_art(cfg, path, prepared)?;
            apply_index_album_art(db, detail_id, &prepared)
        }
    }
}

#[derive(Clone, Copy)]
enum IndexArtworkSelection<'a> {
    Discover,
    Selected(Option<&'a Path>),
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
                && !is_skipped_dir_os_name(&ent.file_name())
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
        && presentation.outline.as_ref() == nfo.about.as_ref()
        && presentation.plot.as_ref() == nfo.plot.as_ref()
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
                if db.clear_detail_nfo_descriptions(*id)? {
                    changed = true;
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
    artwork_path_matches_media_with_names(sidecar, media, &[])
}

fn artwork_path_matches_media_with_names(
    sidecar: &Path,
    media: &Path,
    configured: &[String],
) -> bool {
    let Some(sidecar_name) = sidecar.file_name() else {
        return false;
    };
    album_art_candidate_names(media, configured)
        .iter()
        .any(|candidate| os_str_eq_ci(sidecar_name, candidate))
}

fn refresh_artwork_event(db: &LibraryDb, cfg: &ScanConfig, sidecar: &Path) -> ScanResult<bool> {
    let Some(dir) = sidecar.parent() else {
        return Ok(false);
    };
    if !path_is_allowed_dir(dir, cfg) {
        return Ok(false);
    }
    let mut files = Vec::new();
    let mut media = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| scan_io(dir, error))? {
        let entry = entry.map_err(|error| scan_io(dir, error))?;
        let path = entry.path();
        if !directory_entry_is_allowed_file(&entry, &path, cfg) {
            continue;
        }
        let file_name = entry.file_name();
        files.push((file_name.clone(), path.clone()));
        let name = file_name.to_string_lossy();
        if is_video(&name) || is_audio(&name) || is_image(&name) {
            media.push(path);
        }
    }
    let files = ArtworkInventory::new(files);
    let mut touched = false;
    for path in media {
        if !artwork_path_matches_media_with_names(sidecar, &path, &cfg.album_art_names) {
            continue;
        }
        if let Some(existing) = db.find_detail_by_path(&path_to_db(&path))? {
            let selected = find_album_art_in_inventory(&path, &cfg.album_art_names, &files);
            attach_album_art_with_sidecar(db, cfg, &path, existing.id, selected.as_deref())?;
            touched = true;
        }
    }
    Ok(touched)
}

fn recoverable_artwork_error(cfg: &ScanConfig, error: &ScanError) -> bool {
    if cfg.cancellation.is_cancelled() {
        return false;
    }
    match error {
        ScanError::Io { source, .. } => source.kind() != std::io::ErrorKind::Interrupted,
        ScanError::HelperAdmission(
            HelperAdmissionError::Rejected | HelperAdmissionError::TimedOut,
        ) => true,
        _ => false,
    }
}

fn attach_album_art_in_dir(
    db: &LibraryDb,
    cfg: &ScanConfig,
    dir: &Path,
    failed_inodes: &mut HashSet<(i64, i64)>,
    physical_artwork_inventories: &mut HashMap<PathBuf, ArtworkInventory>,
) -> ScanResult<bool> {
    if !path_is_allowed_dir(dir, cfg) {
        return Ok(false);
    }
    let mut files = Vec::new();
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
        files.push((name, path));
    }
    let files = ArtworkInventory::new(files);
    let mut any = false;
    for path in media {
        let path_s = path_to_db(&path);
        if let Some(existing) = db.find_detail_by_path(&path_s)? {
            let sidecar =
                find_album_art_in_inventory(&path, &cfg.album_art_names, &files).or_else(|| {
                    find_album_art_for_physical_target_cached(
                        &path,
                        cfg,
                        physical_artwork_inventories,
                    )
                    .filter(|candidate| path_is_allowed_file(candidate, cfg))
                });
            let physical = (existing.device, existing.inode);
            if sidecar.is_none() && failed_inodes.contains(&physical) {
                continue;
            }
            let art_id = db.detail_album_art(existing.id)?;
            let current = (art_id > 0)
                .then(|| db.album_art_path(art_id))
                .transpose()?
                .flatten()
                .map(|stored| path_from_db(&stored));
            if let Some(current) = current.as_ref().filter(|stored| stored.is_file()) {
                let cached = current
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("thumb-") || name.starts_with("embed-"));
                if cached && sidecar.is_none() {
                    continue;
                }
                // A real sidecar selected through another hardlink/symlink is
                // authoritative for the inode. A poster-less alias must never
                // replace it with embedded art or a generated video frame.
                if !cached
                    && (!is_direct_physical_path(&path)
                        || sidecar
                            .as_ref()
                            .is_none_or(|candidate| candidate == current))
                {
                    continue;
                }
            }
            match attach_album_art_with_sidecar(db, cfg, &path, existing.id, sidecar.as_deref()) {
                Ok(changed) => any |= changed,
                Err(error) if recoverable_artwork_error(cfg, &error) => {
                    failed_inodes.insert(physical);
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "artwork preparation failed; preserving existing artwork and continuing"
                    );
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(any)
}

/// Which media classes a `media_dir=` root accepts (`V,` / `A,` / `P,`).
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

    fn allows_os_name(&self, name: &OsStr) -> bool {
        let Some(format) = Path::new(name)
            .extension()
            .and_then(OsStr::to_str)
            .and_then(media_format_for_extension)
        else {
            return false;
        };
        (self.video && format.video_mime.is_some())
            || (self.audio && format.audio_mime.is_some())
            || (self.image && format.image_mime.is_some())
    }
}

impl Default for MediaTypes {
    fn default() -> Self {
        Self::all()
    }
}

/// Parse `media_dir=V,/path` or a bare path. Default types are AVP.
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

/// Combine rustyDLNA `media_dir=` specs. Each prefix is parsed; the
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
    format!(
        "root-{}",
        &sha256_hex(title.to_lowercase().as_bytes())[..16]
    )
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

/// Reject non-media before probing. Strong container magic
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

fn file_is_viable_opened(file: &std::fs::File, cfg: &ScanConfig) -> ScanResult<bool> {
    cfg.check_cancelled()?;
    use std::os::fd::AsRawFd;

    let stable_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    Ok(match sniff_container(&stable_path) {
        Sniff::Reject => false,
        Sniff::Strong => true,
        Sniff::Weak => {
            let _helper_permit = acquire_scan_helper(cfg)?;
            ffprobe_file_has_av_stream(file, cfg).unwrap_or(false)
        }
    })
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

fn ffprobe_file_has_av_stream(file: &std::fs::File, cfg: &ScanConfig) -> Option<bool> {
    let mut command = std::process::Command::new("ffprobe");
    command.args([
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
        "/proc/self/fd/3",
    ]);
    let out = crate::probe::command_output_supervised_for_file(
        &mut command,
        file,
        cfg.external_command_timeout,
        &cfg.cancellation,
    )
    .ok()?;
    if !out.status.success() {
        return Some(false);
    }
    let output = String::from_utf8_lossy(&out.stdout);
    Some(
        output
            .lines()
            .any(|line| line.contains("video") || line.contains("audio")),
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
    let mut command = std::process::Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
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
        .stdin(std::process::Stdio::null());
    if probe::command_status_with_timeout(&mut command, std::time::Duration::from_secs(30))
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
    /// Include dot-prefixed files and directories. Defaults to false as a
    /// privacy-oriented scan policy.
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
    /// `media_dir=V,…` filter. Default is all media classes (AVP).
    pub types: MediaTypes,
    /// rustyDLNA `files.db`. None selects in-memory SQLite for tests.
    pub db_path: Option<PathBuf>,
    /// Follow directory/file symlinks whose canonical target is outside every
    /// configured media root. This is intentionally false by default because
    /// enabling it exposes content reachable through links below a media root.
    pub wide_links: bool,
    /// Optional lock-free-ish progress telemetry shared with the server.
    pub progress: Option<std::sync::Arc<ScanProgress>>,
    /// Daemon-wide admission shared by scan probes, image derivation, and
    /// server-side remux/transcode helpers. Embedders may omit it.
    pub helper_gate: Option<std::sync::Arc<HelperGate>>,
    pub helper_queue_timeout: std::time::Duration,
    /// Cooperative daemon-lifetime cancellation. Directory walks, metadata
    /// preparation, helpers, and SQLite publication all observe this token.
    pub cancellation: CancellationToken,
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
            helper_gate: None,
            helper_queue_timeout: std::time::Duration::from_secs(30),
            cancellation: CancellationToken::default(),
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

/// Keep discovery, physical-file grouping, and prepared metadata bounded even
/// when an initial scan contains hundreds of thousands of paths. Each batch is
/// prepared concurrently and published to SQLite in walk order before the
/// scanner discovers more files.
const SCAN_PREPARATION_BATCH_FILES: usize = 512;

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
    let transaction = db
        .immediate_transaction()
        .map_err(|error| format!("start media-root mapping transaction: {error}"))?;
    const MAX_HISTORICAL_ALIASES_PER_ROOT: usize = 64;
    const MAX_HISTORICAL_ALIAS_BYTES_PER_ROOT: usize = 64 * 1024;
    const MAX_PERSISTED_MEDIA_ROOT_SETTINGS: u64 = 4096;
    const MAX_PERSISTED_MEDIA_ROOT_SETTING_BYTES: u64 = 4 * 1024 * 1024;
    let (mapping_count, mapping_bytes) =
        LibraryDb::transaction_settings_prefix_size(&transaction, "media_root:")
            .map_err(|error| format!("inspect persisted media-root mappings: {error}"))?;
    if mapping_count > MAX_PERSISTED_MEDIA_ROOT_SETTINGS
        || mapping_bytes > MAX_PERSISTED_MEDIA_ROOT_SETTING_BYTES
    {
        return Err(format!(
            "persisted media-root mappings exceed the bounded settings budget ({mapping_count} rows / {mapping_bytes} bytes)"
        ));
    }
    let persisted = LibraryDb::transaction_settings_with_prefix(&transaction, "media_root:")
        .map_err(|error| format!("read persisted media-root mappings: {error}"))?;
    let persisted_by_key = persisted.iter().cloned().collect::<HashMap<_, _>>();
    let mut candidates = roots.to_vec();
    for root in &mut candidates {
        let alias_prefix = format!("media_root:{}:alias:", root.key);
        for (setting_key, value) in persisted
            .iter()
            .filter(|(key, _)| key.starts_with(&alias_prefix))
        {
            let old = path_from_db(value);
            let expected_key = media_root_alias_setting_key(&root.key, &old);
            if *setting_key != expected_key {
                return Err(format!(
                    "persisted media-root alias key does not match its path: {setting_key}"
                ));
            }
            if old != root.configured_path
                && old != root.canonical_path
                && !root.aliases.contains(&old)
            {
                root.aliases.push(old);
            }
        }
        for field in ["configured", "canonical"] {
            let setting_key = format!("media_root:{}:{field}", root.key);
            if let Some(value) = persisted_by_key.get(&setting_key) {
                let old = path_from_db(value);
                if old != root.configured_path
                    && old != root.canonical_path
                    && !root.aliases.contains(&old)
                {
                    root.aliases.push(old);
                }
            }
        }
        root.aliases.sort();
        root.aliases.dedup();
        root.aliases
            .retain(|path| path != &root.configured_path && path != &root.canonical_path);
        let alias_bytes = root.aliases.iter().try_fold(0usize, |total, path| {
            total.checked_add(path.as_os_str().as_encoded_bytes().len())
        });
        if root.aliases.len() > MAX_HISTORICAL_ALIASES_PER_ROOT
            || alias_bytes.is_none_or(|bytes| bytes > MAX_HISTORICAL_ALIAS_BYTES_PER_ROOT)
        {
            return Err(format!(
                "media root {} exceeds the historical-alias limit ({} paths / {} bytes)",
                root.key, MAX_HISTORICAL_ALIASES_PER_ROOT, MAX_HISTORICAL_ALIAS_BYTES_PER_ROOT
            ));
        }
    }

    for left in 0..candidates.len() {
        for right in left + 1..candidates.len() {
            for a in candidates[left].path_candidates() {
                for b in candidates[right].path_candidates() {
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

    let mut settings = BTreeMap::new();
    for root in &candidates {
        insert_media_root_setting(
            &mut settings,
            format!("media_root:{}:configured", root.key),
            path_to_db(&root.configured_path),
        )?;
        insert_media_root_setting(
            &mut settings,
            format!("media_root:{}:canonical", root.key),
            path_to_db(&root.canonical_path),
        )?;
        for alias in &root.aliases {
            insert_media_root_setting(
                &mut settings,
                media_root_alias_setting_key(&root.key, alias),
                path_to_db(alias),
            )?;
        }
    }
    let desired_bytes = settings.iter().try_fold(0u64, |total, (key, value)| {
        total
            .checked_add(key.len() as u64)?
            .checked_add(value.len() as u64)
    });
    if settings.len() as u64 > MAX_PERSISTED_MEDIA_ROOT_SETTINGS
        || desired_bytes.is_none_or(|bytes| bytes > MAX_PERSISTED_MEDIA_ROOT_SETTING_BYTES)
    {
        return Err("desired media-root mappings exceed the bounded settings budget".into());
    }
    for (key, _) in &persisted {
        if !settings.contains_key(key) {
            LibraryDb::delete_transaction_setting(&transaction, key)
                .map_err(|error| format!("remove stale media-root path {key}: {error}"))?;
        }
    }
    for (key, value) in &settings {
        LibraryDb::set_transaction_setting(&transaction, key, value)
            .map_err(|error| format!("persist media-root path {key}: {error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("commit media-root paths: {error}"))?;
    roots.clone_from_slice(&candidates);
    Ok(())
}

fn media_root_alias_setting_key(root_key: &str, path: &Path) -> String {
    format!(
        "media_root:{root_key}:alias:{}",
        sha256_hex(path.as_os_str().as_encoded_bytes())
    )
}

fn insert_media_root_setting(
    settings: &mut BTreeMap<String, String>,
    key: String,
    value: String,
) -> Result<(), String> {
    if let Some(previous) = settings.insert(key.clone(), value.clone()) {
        if previous != value {
            return Err(format!(
                "media-root mapping key collision for {key}; refusing to discard either raw path"
            ));
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

#[derive(Debug)]
pub enum CatalogUpdate {
    Replacement(Catalog),
    Patch(CatalogPatch),
}

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
    fn backup_count(&self) -> usize {
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
    #[error(transparent)]
    HelperAdmission(#[from] HelperAdmissionError),
    #[error("library scan cancelled")]
    Cancelled,
}

pub type ScanResult<T> = Result<T, ScanError>;

fn scan_io(path: &Path, source: std::io::Error) -> ScanError {
    ScanError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn sqlite_file_size(bytes: u64) -> ScanResult<i64> {
    i64::try_from(bytes)
        .map_err(|_| ScanError::Invariant("media file size exceeds SQLite's i64 range".into()))
}

fn sqlite_i64_from_u64_bits(value: u64) -> i64 {
    i64::from_ne_bytes(value.to_ne_bytes())
}

pub(crate) fn u64_from_sqlite_i64_bits(value: i64) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

fn acquire_scan_helper(cfg: &ScanConfig) -> ScanResult<Option<HelperPermit>> {
    let acquired = cfg
        .helper_gate
        .as_ref()
        .map(|gate| gate.acquire_timeout_cancelled(cfg.helper_queue_timeout, &cfg.cancellation))
        .transpose();
    match acquired {
        Err(HelperAdmissionError::Cancelled) => Err(ScanError::Cancelled),
        Err(error) => Err(error.into()),
        Ok(permit) => Ok(permit),
    }
}

impl ScanConfig {
    fn check_cancelled(&self) -> ScanResult<()> {
        if self.cancellation.is_cancelled() {
            Err(ScanError::Cancelled)
        } else {
            Ok(())
        }
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
    open_library_db_controlled(path, None)
}

fn open_library_db_cancelled(
    path: &Path,
    cancellation: &CancellationToken,
) -> ScanResult<LibraryDb> {
    open_library_db_controlled(path, Some(cancellation))
}

fn open_library_db_controlled(
    path: &Path,
    cancellation: Option<&CancellationToken>,
) -> ScanResult<LibraryDb> {
    let open = || match cancellation {
        Some(cancellation) => LibraryDb::open_with_cancellation(path, cancellation.clone()),
        None => LibraryDb::open(path),
    };
    match open() {
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
            Ok(open()?)
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

/// Album art / caption files served into RAM. Larger → 413.
pub const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;

fn paths_are_same_media(stored: &str, event: &Path, cfg: &ScanConfig) -> bool {
    let stored_path = path_from_db(stored);
    if stored_path == event {
        return true;
    }
    let key = media_rel_key_for_config(event, cfg);
    !key.is_empty() && media_rel_key_for_config(&stored_path, cfg) == key
}

fn equivalent_media_paths(path: &Path, cfg: &ScanConfig) -> Vec<PathBuf> {
    let Some(selected) = cfg.selected_root(path) else {
        return vec![path.to_path_buf()];
    };
    let Some(relative) = path.strip_prefix(selected.relative_to).ok() else {
        return vec![path.to_path_buf()];
    };
    let mut paths = cfg
        .media_roots
        .iter()
        .find(|root| root.key == selected.key)
        .map(|root| {
            root.path_candidates()
                .map(|candidate| candidate.join(relative))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![path.to_path_buf()]);
    paths.sort();
    paths.dedup();
    paths
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
        Some(p) => Ok(Some(open_library_db_cancelled(p, &cfg.cancellation)?)),
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
    cfg.check_cancelled()?;
    let _write = library_write_guard();
    let Some(db) = open_library(cfg)? else {
        return Ok(0);
    };
    let transaction = db.transaction()?;
    let rows = db.all_detail_stats()?;
    let mut n = 0usize;
    for row in rows {
        cfg.check_cancelled()?;
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
    cfg.check_cancelled()?;
    if n > 0 {
        advance_scan_catalog_epoch(&db)?;
    }
    transaction.commit()?;
    Ok(n)
}

fn advance_scan_catalog_epoch(db: &LibraryDb) -> ScanResult<u64> {
    let current = db.scan_catalog_epoch()?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| ScanError::Invariant("scan catalog epoch space was exhausted".into()))?;
    db.set_scan_catalog_epoch(next)?;
    Ok(next)
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

/// rustyDLNA `exclude_file` matching: basename only, ASCII case-insensitive,
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

/// rustyDLNA `exclude_dir`: a path component (`incomplete`) or a suffix
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
        .ok()
        .and_then(|metadata| file_mtime_date_from_metadata(&metadata));
    w3c_date_from_unix(unix.unwrap_or(0)).unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

fn file_mtime_date_from_metadata(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

fn caption_variant_for_media<'a>(sidecar: &'a Path, media: &Path) -> Option<&'a [u8]> {
    let base = sidecar.file_stem().map(OsStr::as_encoded_bytes)?;
    let stem = media.file_stem().map(OsStr::as_encoded_bytes)?;
    let variant = base.strip_prefix(stem)?;
    if variant.is_empty() {
        Some(variant)
    } else {
        variant
            .strip_prefix(b".")
            .filter(|variant| !variant.is_empty())
    }
}

pub fn caption_path_matches_media(sidecar: &Path, media: &Path) -> bool {
    sidecar
        .file_name()
        .and_then(caption_format_for_os_name)
        .is_some()
        && caption_variant_for_media(sidecar, media).is_some()
}

/// Return a lowercase two- or three-letter language code from a caption
/// variant owned by `media`.
///
/// Ownership uses the same raw filename-byte grammar as caption association:
/// an exact media stem followed by a dot-owned variant. The first `.`, `-`, or
/// `_` ends the language subtag. Invalid UTF-8 in the shared stem is preserved
/// and never compared through a lossy string.
pub fn caption_language_for_media(sidecar: &Path, media: &Path) -> Option<String> {
    sidecar.file_name().and_then(caption_format_for_os_name)?;
    let variant = caption_variant_for_media(sidecar, media)?;
    let candidate = variant
        .split(|byte| matches!(byte, b'.' | b'-' | b'_'))
        .next()?;
    ((2..=3).contains(&candidate.len()) && candidate.iter().all(|byte| byte.is_ascii_alphabetic()))
        .then(|| {
            candidate
                .iter()
                .map(|byte| char::from(byte.to_ascii_lowercase()))
                .collect()
        })
}

fn caption_extension_for_path(path: &Path) -> &'static str {
    path.file_name()
        .and_then(caption_format_for_os_name)
        .map(|format| format.extension)
        .unwrap_or("sub")
}

fn captions_from_candidates(file: &Path, candidates: &[PathBuf]) -> Vec<Caption> {
    candidates
        .iter()
        .filter(|path| caption_path_matches_media(path, file))
        .enumerate()
        .map(|(index, path)| Caption {
            index: index as u32,
            path: path.clone(),
            ext: caption_extension_for_path(path).into(),
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
    probe_sidecar_candidates(file)
        .into_iter()
        .any(|candidate| candidate.is_file())
}

const NO_PROBE_SIDECAR_FINGERPRINT: &str = "v2:none";

fn probe_sidecar_candidates(file: &Path) -> [PathBuf; 2] {
    [
        path_with_suffix(file, ".probe.toml"),
        file.with_extension("probe.toml"),
    ]
}

fn is_probe_sidecar_os_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().ends_with(b".probe.toml")
}

fn probe_sidecar_path_matches_media(sidecar: &Path, media: &Path) -> bool {
    probe_sidecar_candidates(media)
        .into_iter()
        .any(|candidate| candidate == sidecar)
}

fn probe_sidecar_fingerprint(sidecar: &SourceProbe) -> String {
    if sidecar.container.is_empty()
        && sidecar.video.is_empty()
        && sidecar.audio.is_empty()
        && sidecar.hdr.is_empty()
        && sidecar.width == 0
        && sidecar.height == 0
    {
        return NO_PROBE_SIDECAR_FINGERPRINT.to_string();
    }
    let mut canonical = Vec::new();
    for value in [
        sidecar.container.as_bytes(),
        sidecar.video.as_bytes(),
        sidecar.audio.as_bytes(),
        sidecar.hdr.as_bytes(),
    ] {
        canonical.extend_from_slice(&(value.len() as u64).to_le_bytes());
        canonical.extend_from_slice(value);
    }
    canonical.extend_from_slice(&sidecar.width.to_le_bytes());
    canonical.extend_from_slice(&sidecar.height.to_le_bytes());
    format!("v2:{}", sha256_hex(&canonical))
}

fn apply_probe_sidecar(probe: &mut SourceProbe, sidecar: &SourceProbe) {
    if !sidecar.container.is_empty() {
        probe.container.clone_from(&sidecar.container);
    }
    if !sidecar.video.is_empty() {
        probe.video.clone_from(&sidecar.video);
    }
    if !sidecar.audio.is_empty() {
        probe.audio.clone_from(&sidecar.audio);
    }
    if !sidecar.hdr.is_empty() {
        probe.hdr.clone_from(&sidecar.hdr);
    }
    if sidecar.width > 0 {
        probe.width = sidecar.width;
    }
    if sidecar.height > 0 {
        probe.height = sidecar.height;
    }
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

/// DLNA PN from stored stream identity. Matroska deliberately stays empty.
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
    let compact_metadata = audio_streams
        .filter(|value| !value.is_empty())
        .and_then(|value| CompactStreamMetadata::parse(value).ok());
    let audio_streams = compact_metadata
        .map(|metadata| metadata.as_str().to_owned())
        .unwrap_or_default();
    let mut probe = SourceProbe {
        container,
        video,
        audio: audio.filter(|s| !s.is_empty()).unwrap_or("").to_string(),
        audio_streams,
        hdr: hdr.filter(|s| !s.is_empty()).unwrap_or("").to_string(),
        width,
        height,
        ..SourceProbe::default()
    };
    if let Some(metadata) = compact_metadata {
        if let Some(video) = metadata.video_capabilities() {
            probe.video_profile = video.profile.into_owned();
            probe.video_level = video.level;
            probe.pixel_format = video.pixel_format.into_owned();
            probe.bit_depth = video.bit_depth;
            probe.frame_rate = video.frame_rate.into_owned();
            probe.codec_string = video.codec_string.into_owned();
            probe.audio_layout = video.audio_layout.into_owned();
        }
        if let Some(mode) = metadata.timestamp_mode() {
            probe.video_timestamp_mode = mode.to_owned();
        }
    }
    probe
}

pub fn apply_probe_to_detail(db: &LibraryDb, id: i64, got: &MediaProbe) -> ScanResult<()> {
    apply_probe_to_detail_inner(db, id, got, false)
}

fn replace_probe_on_detail(db: &LibraryDb, id: i64, got: &MediaProbe) -> ScanResult<()> {
    apply_probe_to_detail_inner(db, id, got, true)
}

fn apply_probe_to_detail_inner(
    db: &LibraryDb,
    id: i64,
    got: &MediaProbe,
    exact: bool,
) -> ScanResult<()> {
    let mut got = got.clone();
    if got.probe.hdr.is_empty() {
        got.probe.hdr = "sdr".into();
    }
    let stream = DetailStreamUpdate {
        duration: got.av.duration.as_deref(),
        bitrate: got.av.bitrate,
        resolution: got.av.resolution.as_deref(),
        channels: got.av.channels,
        samplerate: got.av.samplerate,
        container: Some(got.probe.container.as_str()).filter(|s| !s.is_empty()),
        video: Some(got.probe.video.as_str()).filter(|s| !s.is_empty()),
        audio: Some(got.probe.audio.as_str()).filter(|s| !s.is_empty()),
        hdr: Some(got.probe.hdr.as_str()),
    };
    if exact {
        db.replace_detail_stream(id, stream)?;
    } else {
        db.update_detail_stream(id, stream)?;
    }
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
    if exact {
        db.copy_stream_to_matching_probe_sidecar_aliases(id)?;
    } else {
        db.copy_stream_to_inode_aliases(id)?;
    }
    db.copy_embedded_tags_to_inode_aliases(id)?;
    Ok(())
}

/// Probe `path` and persist. Libav `None` still applies a `.probe.toml`
/// sidecar. With neither, stream columns stay unset so a later size change
/// can retry (growing MP4 with no moov yet).
fn persist_probe(db: &LibraryDb, cfg: &ScanConfig, path: &Path, id: i64) -> ScanResult<bool> {
    let opened = match open_allowed_file(path, cfg) {
        Ok(opened) => opened,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(scan_io(path, error)),
    };
    persist_probe_with_opened(db, cfg, path, id, None, &opened)
}

fn persist_probe_with_opened(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    id: i64,
    known: Option<MediaProbe>,
    opened: &RootedFile,
) -> ScanResult<bool> {
    let got = match known {
        Some(got) => Some(got),
        None => probe_opened_media(cfg, path, opened)?,
    };
    persist_prepared_probe(db, cfg, path, id, got)
}

fn probe_opened_media(
    cfg: &ScanConfig,
    path: &Path,
    opened: &RootedFile,
) -> ScanResult<Option<MediaProbe>> {
    cfg.check_cancelled()?;
    let helper_permit = acquire_scan_helper(cfg)?;
    let stable_path = opened.proc_path();
    let got = if path.file_name().is_some_and(is_image_os_name) {
        crate::probe::probe_image_with_cancellation(
            &stable_path,
            cfg.external_command_timeout,
            &cfg.cancellation,
        )
    } else {
        crate::probe::probe_media_with_cancellation(
            &stable_path,
            cfg.external_command_timeout,
            &cfg.cancellation,
        )
    };
    drop(helper_permit);
    cfg.check_cancelled()?;
    Ok(got)
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
    if path.file_name().is_some_and(is_image_os_name) {
        db.set_detail_probe_sidecar_fingerprint(id, NO_PROBE_SIDECAR_FINGERPRINT)?;
        let Some(got) = got else {
            db.clear_detail_stream_matching_probe_sidecar(id)?;
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
        db.copy_stream_to_matching_probe_sidecar_aliases(id)?;
        db.copy_embedded_tags_to_inode_aliases(id)?;
        return Ok(true);
    }
    if let Some(mut got) = got {
        let fingerprint = merge_sidecar(cfg, path, &mut got.probe)?;
        db.set_detail_probe_sidecar_fingerprint(id, &fingerprint)?;
        replace_probe_on_detail(db, id, &got)?;
        return Ok(true);
    }
    let mut probe = SourceProbe::default();
    let fingerprint = merge_sidecar(cfg, path, &mut probe)?;
    db.set_detail_probe_sidecar_fingerprint(id, &fingerprint)?;
    if probe.hdr.is_empty() && probe.video.is_empty() && probe.audio.is_empty() {
        db.clear_detail_stream_matching_probe_sidecar(id)?;
        return Ok(false);
    }
    replace_probe_on_detail(
        db,
        id,
        &MediaProbe {
            probe,
            av: AvMeta::default(),
            tags: EmbeddedTags::default(),
            audio_tracks: Vec::new(),
            chapters: Vec::new(),
        },
    )?;
    Ok(true)
}

fn merge_sidecar(cfg: &ScanConfig, path: &Path, probe: &mut SourceProbe) -> ScanResult<String> {
    for c in probe_sidecar_candidates(path) {
        let mut opened = match open_allowed_file(&c, cfg) {
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
            Err(error) => return Err(scan_io(&c, error)),
        };
        const MAX_PROBE_SIDECAR_READ_BYTES: usize = 64 * 1024;
        const MAX_PROBE_SIDECAR_BYTES: u64 = MAX_PROBE_SIDECAR_READ_BYTES as u64;
        let metadata = opened.file.metadata().map_err(|error| scan_io(&c, error))?;
        if metadata.len() > MAX_PROBE_SIDECAR_BYTES {
            continue;
        }
        let bytes = match rusty_dlna_helper::read_to_end_bounded(
            &mut opened.file,
            MAX_PROBE_SIDECAR_READ_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(rusty_dlna_helper::BoundedReadError::LimitExceeded { .. }) => continue,
            Err(rusty_dlna_helper::BoundedReadError::Io(error)) => {
                return Err(scan_io(&c, error));
            }
        };
        let text = String::from_utf8(bytes).map_err(|error| {
            scan_io(
                &c,
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })?;
        let s = parse_probe_toml(&text);
        let fingerprint = probe_sidecar_fingerprint(&s);
        apply_probe_sidecar(probe, &s);
        return Ok(fingerprint);
    }
    Ok(NO_PROBE_SIDECAR_FINGERPRINT.to_string())
}

fn current_probe_sidecar_fingerprint(cfg: &ScanConfig, path: &Path) -> ScanResult<String> {
    if path.file_name().is_some_and(is_image_os_name) {
        return Ok(NO_PROBE_SIDECAR_FINGERPRINT.to_string());
    }
    merge_sidecar(cfg, path, &mut SourceProbe::default())
}

#[derive(Debug)]
struct ProbeSidecarDetail {
    path: PathBuf,
    id: i64,
    device: i64,
    inode: i64,
}

fn refresh_probe_sidecars_for_details(
    db: &LibraryDb,
    cfg: &ScanConfig,
    mut details: Vec<ProbeSidecarDetail>,
) -> ScanResult<bool> {
    details.sort_by(|left, right| {
        (left.device, left.inode, &left.path).cmp(&(right.device, right.inode, &right.path))
    });
    let mut changed = false;
    let mut last_raw_probe: Option<(PhysicalFileKey, Option<MediaProbe>)> = None;
    for detail in details {
        cfg.check_cancelled()?;
        let current = current_probe_sidecar_fingerprint(cfg, &detail.path)?;
        let previous = db.detail_probe_sidecar_fingerprint(detail.id)?;
        if previous.as_deref() == Some(current.as_str()) {
            continue;
        }
        let opened = match open_allowed_file(&detail.path, cfg) {
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
            Err(error) => return Err(scan_io(&detail.path, error)),
        };
        let metadata = opened
            .file
            .metadata()
            .map_err(|error| scan_io(&detail.path, error))?;
        let physical = PhysicalFileKey::new(&detail.path, &metadata)?;
        let raw = if let Some((cached_physical, raw)) = &last_raw_probe {
            if cached_physical == &physical {
                raw.clone()
            } else {
                let raw = probe_opened_media(cfg, &detail.path, &opened)?;
                last_raw_probe = Some((physical, raw.clone()));
                raw
            }
        } else {
            let raw = probe_opened_media(cfg, &detail.path, &opened)?;
            last_raw_probe = Some((physical, raw.clone()));
            raw
        };
        persist_prepared_probe(db, cfg, &detail.path, detail.id, raw)?;
        changed |=
            db.detail_probe_sidecar_fingerprint(detail.id)?.as_deref() != previous.as_deref();
    }
    Ok(changed)
}

fn probe_sidecar_owner_paths(cfg: &ScanConfig, sidecar: &Path) -> ScanResult<Vec<PathBuf>> {
    let Some(name) = sidecar.file_name() else {
        return Ok(Vec::new());
    };
    if !is_probe_sidecar_os_name(name) {
        return Ok(Vec::new());
    }
    let Some(parent) = sidecar.parent() else {
        return Ok(Vec::new());
    };
    if !path_is_allowed_dir(parent, cfg) {
        return Ok(Vec::new());
    }
    let mut owners = Vec::new();
    for entry in std::fs::read_dir(parent).map_err(|error| scan_io(parent, error))? {
        cfg.check_cancelled()?;
        let entry = entry.map_err(|error| scan_io(parent, error))?;
        let path = entry.path();
        let entry_name = entry.file_name();
        if !entry
            .file_type()
            .ok()
            .is_some_and(|kind| kind.is_file() || kind.is_symlink())
            || !cfg
                .root_types_for_path(&path)
                .is_some_and(|types| types.allows_os_name(&entry_name))
            || !probe_sidecar_path_matches_media(sidecar, &path)
            || !path_is_allowed_file(&path, cfg)
        {
            continue;
        }
        owners.push(path);
    }
    owners.sort();
    Ok(owners)
}

fn probe_sidecar_details_for_owners(
    db: &LibraryDb,
    cfg: &ScanConfig,
    owners: &[PathBuf],
) -> ScanResult<Vec<ProbeSidecarDetail>> {
    let mut details = Vec::new();
    for media in owners {
        let stored_paths = equivalent_media_paths(media, cfg)
            .into_iter()
            .map(|path| path_to_db(&path))
            .collect::<Vec<_>>();
        let Some(row) = db.detail_stats_for_paths(&stored_paths)?.into_iter().next() else {
            continue;
        };
        if paths_are_same_media(&row.path, media, cfg) {
            details.push(ProbeSidecarDetail {
                path: media.clone(),
                id: row.id,
                device: row.device,
                inode: row.inode,
            });
        }
    }
    Ok(details)
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
    scan_published(cfg, true)
}

/// Background refresh: do not wipe OBJECTS; skip files whose SIZE+TIMESTAMP
/// are unchanged; never descend into `exclude_dir` (e.g. `incomplete`).
pub fn scan_refresh(cfg: &ScanConfig) -> ScanResult<Catalog> {
    scan_published(cfg, false)
}

fn scan_published(cfg: &ScanConfig, rebuild: bool) -> ScanResult<Catalog> {
    if cfg.db_path.is_none() {
        return scan_inner(cfg, rebuild);
    }
    let mut session = ScanSession::new(cfg)?;
    let prepared = session.prepare_scan(rebuild)?;
    let published = session.publish(prepared)?;
    let (update, _) = published.into_parts();
    let Some(CatalogUpdate::Replacement(catalog)) = update else {
        return Err(ScanError::Invariant(
            "a prepared scan did not produce a replacement catalog".into(),
        ));
    };
    Ok(catalog)
}

/// One-time repair for catalogs created while embedded stream/container titles
/// could overwrite a video's filename. Video release tags are frequently an
/// encoder, uploader, or audio-track label rather than a human-facing movie
/// name, so normalize every video to filename/NFO. The persisted policy
/// revision makes later starts O(1).
pub fn repair_video_titles_if_needed(cfg: &ScanConfig) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    if cfg.db_path.is_some() {
        let mut session = ScanSession::new(cfg)?;
        let prepared = session.prepare_video_title_repair()?;
        let (update, delta) = session.publish(prepared)?.into_parts();
        let catalog = match update {
            Some(CatalogUpdate::Replacement(catalog)) => Some(catalog),
            Some(CatalogUpdate::Patch(_)) => {
                return Err(ScanError::Invariant(
                    "video title repair produced an incremental patch".into(),
                ));
            }
            None => None,
        };
        return Ok((catalog, delta));
    }
    let db = match &cfg.db_path {
        Some(path) => open_library_db_cancelled(path, &cfg.cancellation)?,
        None => return Ok((None, ScanDelta::default())),
    };
    repair_video_titles_with_db(cfg, &db)
}

fn repair_video_titles_with_db(
    cfg: &ScanConfig,
    db: &LibraryDb,
) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    const POLICY_KEY: &str = "video_title_policy_rev";
    const POLICY_REV: &str = "3";

    cfg.check_cancelled()?;
    if db.setting(POLICY_KEY)?.as_deref() == Some(POLICY_REV) {
        return Ok((None, ScanDelta::default()));
    }
    let transaction = db.transaction()?;
    let mut changed = 0usize;
    let mut desired_by_physical: HashMap<(i64, i64, String), String> = HashMap::new();
    for row in db.video_detail_titles()? {
        cfg.check_cancelled()?;
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
    cfg.check_cancelled()?;
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
    Ok((Some(load_catalog_with_policy(db, cfg)?), delta))
}

fn scan_inner(cfg: &ScanConfig, rebuild: bool) -> ScanResult<Catalog> {
    cfg.check_cancelled()?;
    if let Some(progress) = &cfg.progress {
        progress.reset();
    }
    let db = match &cfg.db_path {
        Some(p) => open_library_db_cancelled(p, &cfg.cancellation)?,
        None => LibraryDb::open_memory()?,
    };
    db.install_cancellation(cfg.cancellation.clone())?;
    scan_with_db(cfg, rebuild, &db)
}

fn scan_with_db(cfg: &ScanConfig, rebuild: bool, db: &LibraryDb) -> ScanResult<Catalog> {
    cfg.check_cancelled()?;
    let transaction = db.transaction()?;
    if rebuild {
        db.clear_objects()?;
    }
    db.seed_virtual_containers()?;
    {
        let mut walker = DbWalker {
            db,
            cfg,
            walk_stack: HashMap::new(),
            rebuild,
            indexed: 0,
            pending: Vec::new(),
            pending_limit: SCAN_PREPARATION_BATCH_FILES,
            preparation_batches: 0,
            peak_pending: 0,
            physical_artwork_inventories: HashMap::new(),
        };
        for root in &cfg.media_dirs {
            cfg.check_cancelled()?;
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            let title = cfg
                .root_title_for_path(&root)
                .unwrap_or("media")
                .to_string();
            walker.walk(&root, BROWSEDIR_ID, &title)?;
        }
        walker.index_pending()?;
        tracing::info!(
            target: "rusty_dlna",
            batches = walker.preparation_batches,
            peak_pending = walker.peak_pending,
            batch_limit = walker.pending_limit,
            "ordered scan publication complete"
        );
    }
    db.prune_missing_files()?;
    cfg.check_cancelled()?;
    db.prune_excluded_paths(cfg)?;
    cfg.check_cancelled()?;
    playlist::sync_playlists(db, cfg)?;
    cfg.check_cancelled()?;
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
    cfg.check_cancelled()?;
    transaction.commit()?;
    let n = db.detail_count()?;
    tracing::info!(
        target: "rusty_dlna",
        details = n,
        path = %db.path.display(),
        "scan complete"
    );
    load_catalog_with_policy(db, cfg)
}

fn fill_missing_av_meta_with_db(
    cfg: &ScanConfig,
    db: &LibraryDb,
) -> ScanResult<(Option<Catalog>, ScanDelta)> {
    cfg.check_cancelled()?;
    let transaction = db.transaction()?;
    let mut filled = 0usize;
    let mut last_raw_probe: Option<(PhysicalFileKey, Option<MediaProbe>)> = None;
    for nullable_identity in [true, false] {
        let mut after = (i64::MIN, i64::MIN, i64::MIN);
        loop {
            let rows = db.stream_probe_candidate_batch(
                nullable_identity,
                after,
                SCAN_PREPARATION_BATCH_FILES,
            )?;
            if rows.is_empty() {
                break;
            }
            if filled == 0 {
                tracing::info!("filling missing stream metadata from files");
            }
            for row in rows {
                cfg.check_cancelled()?;
                after = (row.device, row.inode, row.id);
                let decoded = path_from_db(&row.path);
                if path_is_unwanted(&decoded, cfg) {
                    continue;
                }
                let live = rebase_media_path_for_config(&decoded, cfg);
                let opened = match open_allowed_file(&live, cfg) {
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
                    Err(error) => return Err(scan_io(&live, error)),
                };
                let metadata = opened
                    .file
                    .metadata()
                    .map_err(|error| scan_io(&live, error))?;
                let physical = PhysicalFileKey::new(&live, &metadata)?;
                let got = if let Some((cached_physical, got)) = &last_raw_probe {
                    if cached_physical == &physical {
                        got.clone()
                    } else {
                        let got = probe_opened_media(cfg, &live, &opened)?;
                        last_raw_probe = Some((physical, got.clone()));
                        got
                    }
                } else {
                    let got = probe_opened_media(cfg, &live, &opened)?;
                    last_raw_probe = Some((physical, got.clone()));
                    got
                };
                cfg.check_cancelled()?;
                persist_prepared_probe(db, cfg, &live, row.id, got)?;
                filled += 1;
                if filled.is_multiple_of(200) {
                    tracing::info!(filled, "stream metadata progress");
                }
            }
        }
    }
    if filled > 0 {
        tracing::info!(filled, "stream metadata fill done");
    }
    let derived = db.backfill_derived_stream_fields()?;
    if derived > 0 {
        tracing::info!(
            n = derived,
            "backfilled DLNA_PN / mpeg4 from stored stream columns"
        );
    }
    cfg.check_cancelled()?;
    transaction.commit()?;
    let changed = filled + derived;
    let catalog = if changed > 0 {
        Some(load_catalog_with_policy(db, cfg)?)
    } else {
        None
    };
    Ok((
        catalog,
        ScanDelta {
            changed,
            ..ScanDelta::default()
        },
    ))
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

pub fn monitor_incremental(cfg: &ScanConfig) -> ScanResult<(Option<CatalogUpdate>, ScanDelta)> {
    monitor_dirty_inner(cfg, &[], true)
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
    let (update, delta) = monitor_dirty_inner(cfg, dirty, false)?;
    let catalog = match update {
        Some(CatalogUpdate::Replacement(catalog)) => Some(catalog),
        Some(CatalogUpdate::Patch(_)) => Some(reload_published_catalog(cfg)?),
        None => None,
    };
    Ok((catalog, delta))
}

fn reload_published_catalog(cfg: &ScanConfig) -> ScanResult<Catalog> {
    let path = cfg.db_path.as_ref().ok_or_else(|| {
        ScanError::Invariant("an incremental patch requires a persistent library database".into())
    })?;
    let db = open_library_db_cancelled(path, &cfg.cancellation)?;
    load_catalog_with_policy(&db, cfg)
}

pub fn monitor_dirty_incremental(
    cfg: &ScanConfig,
    dirty: &[PathBuf],
) -> ScanResult<(Option<CatalogUpdate>, ScanDelta)> {
    monitor_dirty_inner(cfg, dirty, true)
}

fn monitor_dirty_inner(
    cfg: &ScanConfig,
    dirty: &[PathBuf],
    incremental: bool,
) -> ScanResult<(Option<CatalogUpdate>, ScanDelta)> {
    if cfg.db_path.is_some() {
        let mut session = ScanSession::new(cfg)?;
        let prepared = session.prepare_monitor(dirty, incremental)?;
        return Ok(session.publish(prepared)?.into_parts());
    }
    let _write = library_write_guard();
    let db = match &cfg.db_path {
        Some(p) => open_library_db_cancelled(p, &cfg.cancellation)?,
        None => LibraryDb::open_memory()?,
    };
    monitor_dirty_with_db(cfg, dirty, incremental, &db, None)
}

struct CatalogChangeCapture<'a> {
    db: &'a LibraryDb,
}

impl Drop for CatalogChangeCapture<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.db.end_catalog_change_capture() {
            tracing::warn!(%error, "could not release temporary catalog change capture");
        }
    }
}

fn monitor_dirty_with_db(
    cfg: &ScanConfig,
    dirty: &[PathBuf],
    incremental: bool,
    db: &LibraryDb,
    mut deferred_stale_art: Option<&mut Vec<String>>,
) -> ScanResult<(Option<CatalogUpdate>, ScanDelta)> {
    cfg.check_cancelled()?;
    let started = std::time::Instant::now();
    let restat_all = dirty.is_empty();
    if let Some(progress) = &cfg.progress {
        progress.reset();
    }
    if restat_all {
        tracing::info!(target: "rusty_dlna", "library reconciliation started");
    }
    db.install_cancellation(cfg.cancellation.clone())?;
    let _capture = if incremental {
        db.begin_catalog_change_capture()?;
        Some(CatalogChangeCapture { db })
    } else {
        None
    };
    cfg.check_cancelled()?;
    let mut targeted_dirty = dirty.to_vec();
    let mut probe_sidecar_owners = HashMap::new();
    if !restat_all {
        for path in dirty {
            if path.file_name().is_some_and(is_probe_sidecar_os_name) {
                let owners = probe_sidecar_owner_paths(cfg, path)?;
                targeted_dirty.extend(owners.iter().cloned());
                probe_sidecar_owners.insert(path.clone(), owners);
            }
        }
        targeted_dirty.sort();
        targeted_dirty.dedup();
    }
    let listed = if restat_all {
        list_media_files(cfg)?
    } else {
        list_dirty_media_files(cfg, &targeted_dirty)?
    };
    let dirty_db_paths: Vec<String> = targeted_dirty
        .iter()
        .flat_map(|path| equivalent_media_paths(path, cfg))
        .map(|path| path_to_db(&path))
        .collect();
    let db_rows = if restat_all {
        db.all_detail_stats()?
    } else {
        db.detail_stats_for_paths(&dirty_db_paths)?
    };
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
        cfg.check_cancelled()?;
        listed_by_rel.insert(media_rel_key_for_config(&st.path, cfg), st);
    }
    let dirty_rels: HashSet<String> = targeted_dirty
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
        cfg.check_cancelled()?;
        let decoded = path_from_db(&row.path);
        let key = media_rel_key_for_config(&decoded, cfg);
        if !restat_all && !dirty_rels.contains(&key) {
            continue;
        }
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
    let live_db_rows = if restat_all {
        db.all_detail_stats()?
    } else {
        db.detail_stats_for_paths(&dirty_db_paths)?
    };
    let mut in_db_by_rel: HashMap<String, Vec<DetailStat>> = HashMap::new();
    for row in &live_db_rows {
        cfg.check_cancelled()?;
        in_db_by_rel
            .entry(media_rel_key_for_config(&path_from_db(&row.path), cfg))
            .or_default()
            .push(row.clone());
    }
    // Same on-disk file stored under the host realpath and the container
    // mount is one library item. Keep the live listed path, drop extras.
    for (key, rows) in &in_db_by_rel {
        cfg.check_cancelled()?;
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
            cfg.check_cancelled()?;
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
    let mut indexing_artwork = ArtworkSelectionCache::default();
    for (path_s, st) in &listed {
        cfg.check_cancelled()?;
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
                    let opened = match open_allowed_file(&st.path, cfg) {
                        Ok(opened) => Some(opened),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound
                                    | std::io::ErrorKind::PermissionDenied
                                    | std::io::ErrorKind::InvalidInput
                            ) =>
                        {
                            None
                        }
                        Err(error) => return Err(scan_io(&st.path, error)),
                    };
                    if let Some(opened) = opened {
                        let meta = opened
                            .file
                            .metadata()
                            .map_err(|error| scan_io(&st.path, error))?;
                        let size = sqlite_file_size(meta.len())?;
                        let mtime = file_mtime_unix(&meta);
                        let (new_dev, new_ino) = inode_key(&meta);
                        live_dev = sqlite_i64_from_u64_bits(new_dev);
                        live_ino = sqlite_i64_from_u64_bits(new_ino);
                        let grew = size != row.size
                            || mtime != row.timestamp
                            || live_dev != row.device
                            || live_ino != row.inode;
                        if grew {
                            if grew && !file_is_viable_opened(&opened.file, cfg)? {
                                db.remove_path_and_symlink_aliases(&row.path)?;
                                log_library_file(&st.path, "removed", "library file removed");
                                removed += 1;
                                continue;
                            }
                            if grew {
                                tracing::info!(
                                    target: "rusty_dlna",
                                    file = st.path.file_name().map(display_os_name).unwrap_or_default(),
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
                                    .map(display_os_name)
                                    .unwrap_or_else(|| "item".to_string());
                                db.reset_detail_tags_to_file_defaults(
                                    row.id,
                                    &title,
                                    &w3c_date_from_unix(
                                        file_mtime_date_from_metadata(&meta).unwrap_or(0),
                                    )
                                    .unwrap_or_else(|| "1970-01-01T00:00:00Z".into()),
                                )?;
                            }
                            let probed = apply_or_reuse_probe(
                                db, cfg, &st.path, row.id, live_dev, live_ino, &opened,
                            )?;
                            if grew {
                                apply_nfo(db, cfg, &st.path, row.id)?;
                                refresh_replaced_inode_aliases(
                                    db,
                                    cfg,
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
                if attach_listed_if_missing(db, cfg, &st.path, row.id, live_dev, live_ino)? {
                    changed += 1;
                }
            }
            None => {
                // Rel-key miss is not "new": the row may already exist at
                // this exact path (genre aliases) or under another prefix.
                if db.find_detail_by_path(path_s)?.is_some() {
                    continue;
                }
                if let Some(folder_id) = ensure_folder_chain(db, cfg, &st.path)? {
                    let sidecar = indexing_artwork.select(&st.path, cfg);
                    if index_one_file_with_selected_artwork(
                        db,
                        cfg,
                        &st.path,
                        &folder_id,
                        sidecar.as_deref(),
                    )? {
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
        let probe_sidecar_started = std::time::Instant::now();
        let mut probe_rows = db.all_detail_stats()?;
        probe_rows.sort_by(|left, right| left.path.cmp(&right.path));
        let mut probe_details = Vec::new();
        for row in &probe_rows {
            cfg.check_cancelled()?;
            let path = rebase_media_path_for_config(&path_from_db(&row.path), cfg);
            if path_is_allowed_file(&path, cfg) {
                probe_details.push(ProbeSidecarDetail {
                    path,
                    id: row.id,
                    device: row.device,
                    inode: row.inode,
                });
            }
        }
        let probe_sidecar_changed = refresh_probe_sidecars_for_details(db, cfg, probe_details)?;
        sidecar_changed |= probe_sidecar_changed;
        tracing::info!(
            target: "rusty_dlna",
            elapsed_ms = probe_sidecar_started.elapsed().as_millis(),
            "library probe-sidecar reconciliation complete"
        );
        let nfo_started = std::time::Instant::now();
        let nfo_changed = refresh_nfo_periodic(db, cfg, &db.all_detail_stats()?)?;
        sidecar_changed |= nfo_changed;
        tracing::info!(
            target: "rusty_dlna",
            elapsed_ms = nfo_started.elapsed().as_millis(),
            "library NFO reconciliation complete"
        );
        let mut parents = HashSet::new();
        for st in listed.values() {
            cfg.check_cancelled()?;
            if let Some(p) = st.path.parent() {
                parents.insert(p.to_path_buf());
            }
        }
        let artwork_started = std::time::Instant::now();
        let mut artwork_changed = false;
        let mut artwork_failures = HashSet::new();
        let mut physical_artwork_inventories = HashMap::new();
        for dir in &parents {
            cfg.check_cancelled()?;
            artwork_changed |= attach_album_art_in_dir(
                db,
                cfg,
                dir,
                &mut artwork_failures,
                &mut physical_artwork_inventories,
            )?;
        }
        sidecar_changed |= artwork_changed;
        tracing::info!(
            target: "rusty_dlna",
            directories = parents.len(),
            failed_physical_files = artwork_failures.len(),
            elapsed_ms = artwork_started.elapsed().as_millis(),
            "library artwork reconciliation complete"
        );
        let captions_started = std::time::Instant::now();
        let mut captions_changed = false;
        for dir in &parents {
            cfg.check_cancelled()?;
            captions_changed |= refresh_captions_in_dir(db, cfg, dir)?;
        }
        sidecar_changed |= captions_changed;
        tracing::info!(
            target: "rusty_dlna",
            directories = parents.len(),
            elapsed_ms = captions_started.elapsed().as_millis(),
            "library caption reconciliation complete"
        );
        tracing::info!(
            target: "rusty_dlna",
            nfo_changed,
            probe_sidecar_changed,
            artwork_changed,
            captions_changed,
            "library sidecar reconciliation result"
        );
    }
    let mut nfo_dirs: HashMap<PathBuf, bool> = HashMap::new();
    let mut dirty_probe_details = Vec::new();
    for d in dirty {
        cfg.check_cancelled()?;
        let Some(name) = d.file_name() else {
            continue;
        };
        let Some(dir) = d.parent() else {
            continue;
        };
        if is_album_art_os_name_for_config(name, cfg) {
            sidecar_changed |= refresh_artwork_event(db, cfg, d)?;
            continue;
        }
        if is_caption_os_name(name) {
            sidecar_changed |= refresh_caption_event(db, cfg, d)?;
            continue;
        }
        if is_probe_sidecar_os_name(name) {
            if let Some(owners) = probe_sidecar_owners.get(d) {
                dirty_probe_details.extend(probe_sidecar_details_for_owners(db, cfg, owners)?);
            }
            continue;
        }
        if ends_with_ci(name, ".nfo") {
            let recursive = name.as_encoded_bytes().len() == "tvshow.nfo".len()
                && ends_with_ci(name, "tvshow.nfo");
            nfo_dirs
                .entry(dir.to_path_buf())
                .and_modify(|current| *current |= recursive)
                .or_insert(recursive);
        }
    }
    dirty_probe_details.sort_by(|left, right| (left.id, &left.path).cmp(&(right.id, &right.path)));
    dirty_probe_details.dedup_by(|left, right| left.id == right.id && left.path == right.path);
    sidecar_changed |= refresh_probe_sidecars_for_details(db, cfg, dirty_probe_details)?;
    for (dir, recursive) in nfo_dirs {
        cfg.check_cancelled()?;
        sidecar_changed |= apply_nfo_in_dir(db, cfg, &dir, recursive)?;
    }
    changed += usize::from(sidecar_changed);
    let playlists_changed = if restat_all || dirty.iter().any(|path| playlist::is_playlist(path)) {
        playlist::sync_playlists(db, cfg)?
    } else {
        false
    };
    if playlists_changed {
        changed += 1;
    }
    if restat_all {
        tracing::info!(
            target: "rusty_dlna",
            playlists_changed,
            "library playlist reconciliation result"
        );
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
        cfg.check_cancelled()?;
        transaction.commit()?;
        if let Some(deferred) = &mut deferred_stale_art {
            deferred.extend(stale_art);
        } else {
            remove_stale_cached_art(cfg, &stale_art);
        }
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
    cfg.check_cancelled()?;
    transaction.commit()?;
    if let Some(deferred) = deferred_stale_art {
        deferred.extend(stale_art);
    } else {
        remove_stale_cached_art(cfg, &stale_art);
    }
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
    let update = if incremental {
        CatalogUpdate::Patch(db.load_catalog_patch()?)
    } else {
        CatalogUpdate::Replacement(load_catalog_with_policy(db, cfg)?)
    };
    Ok((Some(update), delta))
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
    opened: &RootedFile,
) -> ScanResult<bool> {
    let fingerprint = current_probe_sidecar_fingerprint(cfg, path)?;
    if let Some(src) = db.find_inode_probe_source_with_sidecar(device, inode, id, &fingerprint)? {
        db.set_detail_probe_sidecar_fingerprint(id, &fingerprint)?;
        db.copy_stream_from_matching_probe_sidecar(src, id)?;
        return Ok(true);
    }
    persist_probe_with_opened(db, cfg, path, id, None, opened)
}

fn apply_or_reuse_prepared_probe(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    id: i64,
    physical: (i64, i64),
    prepared: Option<&PreparedPhysicalFile>,
    opened: &RootedFile,
) -> ScanResult<bool> {
    // A preparation was requested only when every persisted alias was stale.
    // Do not copy one of those old rows over the freshly obtained probe.
    if let Some(prepared) = prepared.filter(|prepared| prepared.probe_attempted) {
        return persist_prepared_probe(db, cfg, path, id, prepared.probe.clone());
    }
    let fingerprint = current_probe_sidecar_fingerprint(cfg, path)?;
    if let Some(src) =
        db.find_inode_probe_source_with_sidecar(physical.0, physical.1, id, &fingerprint)?
    {
        db.set_detail_probe_sidecar_fingerprint(id, &fingerprint)?;
        db.copy_stream_from_matching_probe_sidecar(src, id)?;
        return Ok(true);
    }
    persist_probe_with_opened(db, cfg, path, id, None, opened)
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

fn refresh_replaced_inode_aliases(
    db: &LibraryDb,
    cfg: &ScanConfig,
    replacement: InodeReplacement,
) -> ScanResult<()> {
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
        let Ok(opened) = open_allowed_file(&decoded, cfg) else {
            continue;
        };
        let meta = opened
            .file
            .metadata()
            .map_err(|error| scan_io(&decoded, error))?;
        let (d, i) = inode_key(&meta);
        if sqlite_i64_from_u64_bits(d) != new_device || sqlite_i64_from_u64_bits(i) != new_inode {
            continue;
        }
        db.update_detail_stat(sid, size, timestamp, new_device, new_inode)?;
        apply_or_reuse_probe(db, cfg, &decoded, sid, new_device, new_inode, &opened)?;
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
    attach_objects(db, &folder_id, detail_id, &title, class, device, inode)?;
    Ok(true)
}

fn list_media_files(cfg: &ScanConfig) -> ScanResult<HashMap<String, ListedFile>> {
    cfg.check_cancelled()?;
    let mut out = HashMap::new();
    let mut walk_stack: HashMap<(u64, u64), ()> = HashMap::new();
    for root in &cfg.media_dirs {
        cfg.check_cancelled()?;
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let title = cfg
            .root_title_for_path(&root)
            .unwrap_or("media")
            .to_string();
        list_into(&mut out, cfg, &mut walk_stack, &root, &title)?;
    }
    Ok(out)
}

fn list_dirty_media_files(
    cfg: &ScanConfig,
    dirty: &[PathBuf],
) -> ScanResult<HashMap<String, ListedFile>> {
    let mut listed = HashMap::new();
    for path in dirty {
        cfg.check_cancelled()?;
        let Some(name) = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
        else {
            continue;
        };
        if (!cfg.include_hidden && name.starts_with('.'))
            || is_unfinished_name(&name)
            || looks_like_sample_file(&name)
            || is_caption_name(&name)
            || ends_with_ci(OsStr::new(&name), ".nfo")
            || path.file_name().is_some_and(is_probe_sidecar_os_name)
            || path_excluded(path, &name, cfg)
            || is_album_art_name_for_config(&name, cfg)
            || !cfg
                .root_types_for_path(path)
                .is_some_and(|types| types.allows(&name))
            || !path_is_allowed_file(path, cfg)
        {
            continue;
        }
        if let Some(progress) = &cfg.progress {
            progress.record(path);
        }
        listed.insert(path_to_db(path), ListedFile { path: path.clone() });
    }
    Ok(listed)
}

fn list_into(
    out: &mut HashMap<String, ListedFile>,
    cfg: &ScanConfig,
    walk_stack: &mut HashMap<(u64, u64), ()>,
    dir: &Path,
    title: &str,
) -> ScanResult<()> {
    cfg.check_cancelled()?;
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
        cfg.check_cancelled()?;
        let ent = ent.map_err(|error| scan_io(dir, error))?;
        let path = ent.path();
        let raw_name = ent.file_name();
        let name = raw_name.to_string_lossy().into_owned();
        if !cfg.include_hidden && name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = ent.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
        if is_dir {
            if is_skipped_dir_os_name(&raw_name) || path_excluded(&path, &name, cfg) {
                continue;
            }
            list_into(out, cfg, walk_stack, &path, &name)?;
            continue;
        }
        if is_unfinished_name(&name)
            || looks_like_sample_file(&name)
            || is_caption_name(&name)
            || ends_with_ci(OsStr::new(&name), ".nfo")
            || is_probe_sidecar_os_name(&raw_name)
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
        if is_skipped_dir_os_name(comp.as_os_str()) || path_excluded(parent, &name, cfg) {
            return Ok(None);
        }
        folder_id = folder_object_id(db, &folder_id, &name)?;
    }
    Ok(Some(folder_id))
}

#[cfg(test)]
pub(crate) fn index_one_file(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    folder_id: &str,
) -> ScanResult<bool> {
    index_one_file_with_artwork(
        db,
        cfg,
        path,
        folder_id,
        None,
        IndexArtworkSelection::Discover,
    )
}

fn index_one_file_with_prepared(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    folder_id: &str,
    prepared: Option<&PreparedPhysicalFile>,
) -> ScanResult<bool> {
    index_one_file_with_artwork(
        db,
        cfg,
        path,
        folder_id,
        prepared,
        IndexArtworkSelection::Discover,
    )
}

fn index_one_file_with_selected_artwork(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    folder_id: &str,
    sidecar: Option<&Path>,
) -> ScanResult<bool> {
    index_one_file_with_artwork(
        db,
        cfg,
        path,
        folder_id,
        None,
        IndexArtworkSelection::Selected(sidecar),
    )
}

fn index_one_file_with_artwork(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    folder_id: &str,
    prepared: Option<&PreparedPhysicalFile>,
    artwork_selection: IndexArtworkSelection<'_>,
) -> ScanResult<bool> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty()
        || !cfg
            .root_types_for_path(path)
            .is_some_and(|types| types.allows(&name))
    {
        return Ok(false);
    }
    let opened = match open_allowed_file(path, cfg) {
        Ok(opened) => opened,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(scan_io(path, error)),
    };
    let meta = opened
        .file
        .metadata()
        .map_err(|error| scan_io(path, error))?;
    let current_physical = PhysicalFileKey::new(path, &meta)?;
    let probe_sidecar_fingerprint = current_probe_sidecar_fingerprint(cfg, path)?;
    let prepared = prepared.filter(|prepared| {
        prepared.physical == current_physical
            && prepared.probe_sidecar_fingerprint == probe_sidecar_fingerprint
    });
    let stable_path = opened.proc_path();
    let (dev, ino) = inode_key(&meta);
    let device = sqlite_i64_from_u64_bits(dev);
    let inode = sqlite_i64_from_u64_bits(ino);
    let inode_source =
        db.find_inode_source_with_probe_sidecar(device, inode, &probe_sidecar_fingerprint)?;
    let eager_probe = if prepared.is_none()
        && inode_source.is_none()
        && media_format_for_name(&name).is_some_and(|format| format.is_ambiguous())
    {
        let _probe_permit = acquire_scan_helper(cfg)?;
        crate::probe::probe_media_with_cancellation(
            &stable_path,
            cfg.external_command_timeout,
            &cfg.cancellation,
        )
    } else {
        None
    };
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
    let mtime_date = w3c_date_from_unix(file_mtime_date_from_metadata(&meta).unwrap_or(0))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into());
    let size = sqlite_file_size(meta.len())?;
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
        let stat_changed =
            old_sz != size || old_ts != mtime || old_dev != device || old_ino != inode;
        let (stored_fingerprint, stored_probe_revision) = db.detail_probe_state(id)?;
        let sidecar_current =
            stored_fingerprint.as_deref() == Some(probe_sidecar_fingerprint.as_str());
        if stat_changed {
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
            db.update_detail_stat(id, size, mtime, device, inode)?;
            db.reset_detail_tags_to_file_defaults(id, &title, &mtime_date)?;
            apply_or_reuse_prepared_probe(db, cfg, path, id, (device, inode), prepared, &opened)?;
            refresh_replaced_inode_aliases(
                db,
                cfg,
                InodeReplacement {
                    old_device: old_dev,
                    old_inode: old_ino,
                    source_id: id,
                    new_device: device,
                    new_inode: inode,
                    size,
                    timestamp: mtime,
                },
            )?;
        } else if !sidecar_current
            || stored_probe_revision < db::STREAM_PROBE_REVISION
            || prepared.is_some_and(|prepared| prepared.probe_attempted)
        {
            apply_or_reuse_prepared_probe(db, cfg, path, id, (device, inode), prepared, &opened)?;
        }
        apply_nfo_to_detail(db, id, &nfo)?;
        attach_objects(db, folder_id, id, &title, class, device, inode)?;
        attach_album_art_for_index(db, cfg, path, id, prepared, artwork_selection, &opened)?;
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
                if old_sz != size || old_ts != mtime || old_dev != device || old_ino != inode {
                    db.update_detail_stat(source.id, size, mtime, device, inode)?;
                    apply_or_reuse_prepared_probe(
                        db,
                        cfg,
                        path,
                        source.id,
                        (device, inode),
                        prepared,
                        &opened,
                    )?;
                }
            }
            attach_objects(db, folder_id, source.id, &title, class, device, inode)?;
            attach_album_art_for_index(
                db,
                cfg,
                path,
                source.id,
                prepared,
                artwork_selection,
                &opened,
            )?;
            return Ok(true);
        }
        if source.size == size && source.timestamp >= mtime {
            let id = db.clone_detail_for_path(source.id, &path_s, size, mtime, device, inode)?;
            attach_objects(db, folder_id, id, &title, class, device, inode)?;
            attach_album_art_for_index(db, cfg, path, id, prepared, artwork_selection, &opened)?;
            return Ok(true);
        }
    }

    if format_probe.is_none() && !file_is_viable_opened(&opened.file, cfg)? {
        return Ok(false);
    }
    let date = nfo.date.clone().unwrap_or(mtime_date);
    let detail = db.insert_detail(NewDetail {
        path: &path_s,
        size,
        timestamp: mtime,
        title: display_title,
        date: &date,
        mime,
        device,
        inode,
        dlna_pn: None,
    })?;
    let caps = captions_for(path, cfg)?;
    db.replace_captions(detail, &caps)?;
    if let Some(prepared) = prepared.filter(|prepared| prepared.probe_attempted) {
        persist_prepared_probe(db, cfg, path, detail, prepared.probe.clone())?;
    } else {
        persist_probe_with_opened(db, cfg, path, detail, eager_probe, &opened)?;
    }
    // Explicit sidecars override embedded tags. Embedded audio/image tags
    // override filename defaults, but video titles deliberately remain the
    // filename stem unless an NFO supplies a curated title.
    apply_nfo_to_detail(db, detail, &nfo)?;
    attach_objects(db, folder_id, detail, &title, class, device, inode)?;
    attach_album_art_for_index(db, cfg, path, detail, prepared, artwork_selection, &opened)?;
    Ok(true)
}

fn allocate_child_id(db: &LibraryDb, parent_id: &str) -> ScanResult<String> {
    let mut n = db.next_child_seq(parent_id)?;
    loop {
        let id = format!("{parent_id}${n:X}");
        if !db.object_exists(&id)? {
            return Ok(id);
        }
        n = n.checked_add(1).ok_or_else(|| {
            ScanError::Invariant(format!(
                "child object ID suffix space was exhausted for parent {parent_id}"
            ))
        })?;
    }
}

fn attach_objects(
    db: &LibraryDb,
    folder_id: &str,
    detail: i64,
    title: &str,
    class: &str,
    device: i64,
    inode: i64,
) -> ScanResult<()> {
    if db.folder_has_inode_named(folder_id, device, inode, title)? {
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
        if !db.all_video_has_inode(device, inode)? {
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
    cfg.check_cancelled()?;
    if cfg.db_path.is_some() {
        let mut session = ScanSession::new(cfg)?;
        let prepared = session.prepare_rebuild_objects()?;
        let (update, _) = session.publish(prepared)?.into_parts();
        let Some(CatalogUpdate::Replacement(catalog)) = update else {
            return Err(ScanError::Invariant(
                "a prepared object rebuild did not produce a replacement catalog".into(),
            ));
        };
        return Ok(catalog);
    }
    let db = match &cfg.db_path {
        Some(p) => open_library_db_cancelled(p, &cfg.cancellation)?,
        None => return Ok(Catalog::new()),
    };
    rebuild_objects_with_db(cfg, &db)
}

fn rebuild_objects_with_db(cfg: &ScanConfig, db: &LibraryDb) -> ScanResult<Catalog> {
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
        cfg.check_cancelled()?;
        db.restore_object(&row)?;
    }
    for row in items {
        cfg.check_cancelled()?;
        if row.detail_id.is_some_and(|d| live_details.contains(&d)) {
            db.restore_object(&row)?;
        }
    }
    db.prune_duplicate_folder_inodes()?;
    let mut n = 0usize;
    let mut indexing_artwork = ArtworkSelectionCache::default();
    for row in &rows {
        cfg.check_cancelled()?;
        let p = path_from_db(&row.path);
        if !path_is_live_file(&p) || path_is_unwanted(&p, cfg) {
            continue;
        }
        if let Some(folder) = ensure_folder_chain(db, cfg, &p)? {
            let sidecar = indexing_artwork.select(&p, cfg);
            if index_one_file_with_selected_artwork(db, cfg, &p, &folder, sidecar.as_deref())? {
                n += 1;
            }
        }
    }
    playlist::sync_playlists(db, cfg)?;
    db.prune_empty_folders()?;
    cfg.check_cancelled()?;
    transaction.commit()?;
    tracing::info!(target: "rusty_dlna", files = n, "objects rebuilt");
    load_catalog_with_policy(db, cfg)
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
    fn new(path: &Path, meta: &std::fs::Metadata) -> ScanResult<Self> {
        let (device, inode) = inode_key(meta);
        let fallback_path = (inode == 0)
            .then(|| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
        Ok(Self {
            device: sqlite_i64_from_u64_bits(device),
            inode: sqlite_i64_from_u64_bits(inode),
            size: sqlite_file_size(meta.len())?,
            timestamp: file_mtime_unix(meta),
            fallback_path,
        })
    }
}

#[derive(Clone, Debug)]
struct PendingFile {
    path: PathBuf,
    folder_id: String,
    physical: PhysicalFileKey,
    probe_sidecar_fingerprint: String,
    album_art_sidecar: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct PreparedPhysicalFile {
    physical: PhysicalFileKey,
    probe_sidecar_fingerprint: String,
    probe_attempted: bool,
    probe: Option<MediaProbe>,
    mime_hint: Option<String>,
    album_art: PreparedAlbumArt,
}

#[derive(Debug)]
struct PreparationGroup {
    physical: PhysicalFileKey,
    probe_sidecar_fingerprint: String,
    source: Option<InodeSource>,
    probe_required: bool,
}

#[derive(Debug)]
struct PhysicalPreparationGroup {
    physical: PhysicalFileKey,
    representative: usize,
    representative_is_direct: bool,
    probe_required: bool,
    inherited_album_art_sidecar: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct PreparedRawPhysicalFile {
    physical: PhysicalFileKey,
    physical_changed: bool,
    probe: Option<MediaProbe>,
    album_art: PreparedAlbumArt,
}

#[derive(Debug, Default)]
struct PreparedBatch {
    by_physical_and_sidecar: HashMap<(PhysicalFileKey, String), PreparedPhysicalFile>,
    worker_indices: HashSet<usize>,
}

fn is_direct_physical_path(path: &Path) -> bool {
    std::fs::canonicalize(path)
        .ok()
        .is_some_and(|canonical| canonical == path)
}

/// Small reusable bounded pool. Jobs may finish out of order, but results are
/// returned in input order so subsequent SQLite IDs stay deterministic.
fn run_bounded_jobs<T, R, F>(
    jobs: &[T],
    workers: usize,
    cancellation: &CancellationToken,
    function: F,
) -> Vec<Option<R>>
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
                if cancellation.is_cancelled() {
                    break;
                }
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
        .map(|slot| slot.into_inner().unwrap_or_else(|error| error.into_inner()))
        .collect()
}

#[cfg(test)]
fn prepare_pending_files(
    db: &LibraryDb,
    cfg: &ScanConfig,
    pending: &[PendingFile],
) -> ScanResult<PreparedBatch> {
    prepare_pending_files_with_inventories(db, cfg, pending, &mut HashMap::new())
}

fn prepare_pending_files_with_inventories(
    db: &LibraryDb,
    cfg: &ScanConfig,
    pending: &[PendingFile],
    physical_artwork_inventories: &mut HashMap<PathBuf, ArtworkInventory>,
) -> ScanResult<PreparedBatch> {
    cfg.check_cancelled()?;
    let mut group_by_pair: HashMap<(PhysicalFileKey, String), usize> = HashMap::new();
    let mut groups: Vec<PreparationGroup> = Vec::new();
    let mut physical_group_by_key: HashMap<PhysicalFileKey, usize> = HashMap::new();
    let mut physical_groups: Vec<PhysicalPreparationGroup> = Vec::new();
    for (index, file) in pending.iter().enumerate() {
        let direct = is_direct_physical_path(&file.path);
        let pair_key = (
            file.physical.clone(),
            file.probe_sidecar_fingerprint.clone(),
        );
        if group_by_pair.contains_key(&pair_key) {
            if direct {
                if let Some(group_index) = physical_group_by_key.get(&file.physical).copied() {
                    let group = &mut physical_groups[group_index];
                    if !group.representative_is_direct {
                        group.representative = index;
                        group.representative_is_direct = true;
                    }
                }
            }
            continue;
        }
        let source = if file.physical.inode == 0 {
            None
        } else {
            db.find_inode_source_with_probe_sidecar(
                file.physical.device,
                file.physical.inode,
                &file.probe_sidecar_fingerprint,
            )?
        };
        let source_is_current = source.as_ref().is_some_and(|source| {
            source.size == file.physical.size
                && source.timestamp >= file.physical.timestamp
                && source.stream_probe_rev >= db::STREAM_PROBE_REVISION
        });
        let physical_index =
            if let Some(group_index) = physical_group_by_key.get(&file.physical).copied() {
                let group = &mut physical_groups[group_index];
                if direct && !group.representative_is_direct {
                    group.representative = index;
                    group.representative_is_direct = true;
                }
                group.probe_required |= !source_is_current;
                group_index
            } else {
                let group_index = physical_groups.len();
                physical_group_by_key.insert(file.physical.clone(), group_index);
                physical_groups.push(PhysicalPreparationGroup {
                    physical: file.physical.clone(),
                    representative: index,
                    representative_is_direct: direct,
                    probe_required: !source_is_current,
                    inherited_album_art_sidecar: None,
                });
                group_index
            };
        debug_assert_eq!(physical_groups[physical_index].physical, file.physical);
        group_by_pair.insert(pair_key, groups.len());
        groups.push(PreparationGroup {
            physical: file.physical.clone(),
            probe_sidecar_fingerprint: file.probe_sidecar_fingerprint.clone(),
            source,
            probe_required: !source_is_current,
        });
    }

    for group in &mut physical_groups {
        let file = &pending[group.representative];
        if !group.representative_is_direct && file.album_art_sidecar.is_none() {
            group.inherited_album_art_sidecar = find_album_art_for_physical_target_cached(
                &file.path,
                cfg,
                physical_artwork_inventories,
            );
        }
    }

    tracing::info!(
        target: "rusty_dlna",
        paths = pending.len(),
        physical_files = physical_groups.len(),
        semantic_groups = groups.len(),
        aliases = pending.len().saturating_sub(physical_groups.len()),
        workers = cfg.scan_workers.max(1),
        "preparing scan files"
    );
    let started = std::time::Instant::now();
    let results: Vec<Option<ScanResult<PreparedRawPhysicalFile>>> = run_bounded_jobs(
        &physical_groups,
        cfg.scan_workers,
        &cfg.cancellation,
        |group| {
            cfg.check_cancelled()?;
            let file = &pending[group.representative];
            if let Some(progress) = &cfg.progress {
                progress.record(&file.path);
            }
            let opened =
                open_allowed_file(&file.path, cfg).map_err(|error| scan_io(&file.path, error))?;
            let metadata = opened
                .file
                .metadata()
                .map_err(|error| scan_io(&file.path, error))?;
            let current_physical = PhysicalFileKey::new(&file.path, &metadata)?;
            let physical_changed = current_physical != group.physical;
            let probe_attempted = physical_changed || group.probe_required;
            let _probe_permit = if probe_attempted {
                acquire_scan_helper(cfg)?
            } else {
                None
            };
            let probe = probe_attempted
                .then(|| {
                    let stable_path = opened.proc_path();
                    if file.path.file_name().is_some_and(is_image_os_name) {
                        crate::probe::probe_image_with_cancellation(
                            &stable_path,
                            cfg.external_command_timeout,
                            &cfg.cancellation,
                        )
                    } else {
                        crate::probe::probe_media_with_cancellation(
                            &stable_path,
                            cfg.external_command_timeout,
                            &cfg.cancellation,
                        )
                    }
                })
                .flatten();
            cfg.check_cancelled()?;
            drop(_probe_permit);
            let sidecar = file
                .album_art_sidecar
                .as_deref()
                .or(group.inherited_album_art_sidecar.as_deref());
            let album_art = recover_prepared_album_art(
                cfg,
                &file.path,
                prepare_album_art_with_sidecar(cfg, &file.path, &opened, sidecar),
            )?;
            Ok(PreparedRawPhysicalFile {
                physical: current_physical,
                physical_changed,
                probe,
                album_art,
            })
        },
    );

    let mut batch = PreparedBatch::default();
    let mut raw_by_physical = HashMap::new();
    for (group, result) in physical_groups.into_iter().zip(results) {
        batch.worker_indices.insert(group.representative);
        raw_by_physical.insert(group.physical, result.ok_or(ScanError::Cancelled)??);
    }
    for group in groups {
        let raw = raw_by_physical.get(&group.physical).ok_or_else(|| {
            ScanError::Invariant("prepared physical probe result is unavailable".into())
        })?;
        let unchanged_physical = raw.physical == group.physical;
        batch.by_physical_and_sidecar.insert(
            (
                group.physical.clone(),
                group.probe_sidecar_fingerprint.clone(),
            ),
            PreparedPhysicalFile {
                physical: raw.physical.clone(),
                probe_sidecar_fingerprint: group.probe_sidecar_fingerprint,
                probe_attempted: raw.physical_changed || group.probe_required,
                probe: raw.probe.clone(),
                mime_hint: unchanged_physical
                    .then(|| group.source.as_ref().map(|source| source.mime.clone()))
                    .flatten(),
                album_art: raw.album_art.clone(),
            },
        );
    }
    tracing::info!(
        target: "rusty_dlna",
        physical_files = batch.by_physical_and_sidecar.len(),
        elapsed_ms = rusty_dlna_helper::duration_millis_saturating(started.elapsed()),
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
    pending_limit: usize,
    preparation_batches: usize,
    peak_pending: usize,
    physical_artwork_inventories: HashMap<PathBuf, ArtworkInventory>,
}

impl DbWalker<'_> {
    fn queue_pending(&mut self, file: PendingFile) -> ScanResult<()> {
        self.pending.push(file);
        self.peak_pending = self.peak_pending.max(self.pending.len());
        if self.pending.len() >= self.pending_limit.max(1) {
            self.index_pending()?;
        }
        Ok(())
    }

    fn index_pending(&mut self) -> ScanResult<()> {
        self.cfg.check_cancelled()?;
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        let prepared = prepare_pending_files_with_inventories(
            self.db,
            self.cfg,
            &pending,
            &mut self.physical_artwork_inventories,
        )?;
        for (index, file) in pending.iter().enumerate() {
            self.cfg.check_cancelled()?;
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
                prepared.by_physical_and_sidecar.get(&(
                    file.physical.clone(),
                    file.probe_sidecar_fingerprint.clone(),
                )),
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
        self.preparation_batches += 1;
        Ok(())
    }

    fn walk(&mut self, dir: &Path, parent_id: &str, title: &str) -> ScanResult<()> {
        self.cfg.check_cancelled()?;
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
            self.cfg.check_cancelled()?;
            ents.push(entry.map_err(|error| scan_io(dir, error))?);
        }
        ents.sort_by_key(|e| e.file_name());
        let artwork_files = ArtworkInventory::new(
            ents.iter()
                .filter_map(|entry| {
                    let path = entry.path();
                    directory_entry_is_allowed_file(entry, &path, self.cfg)
                        .then(|| (entry.file_name(), path))
                })
                .collect(),
        );
        for ent in ents {
            self.cfg.check_cancelled()?;
            let path = ent.path();
            let raw_name = ent.file_name();
            let name = raw_name.to_string_lossy().into_owned();
            if !self.cfg.include_hidden && name.starts_with('.') {
                continue;
            }
            let is_dir = match ent.file_type() {
                Ok(t) if t.is_dir() => true,
                Ok(t) if t.is_symlink() => path.is_dir(),
                _ => false,
            };
            if is_dir {
                if is_skipped_dir_os_name(&raw_name)
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
                || ends_with_ci(OsStr::new(&name), ".nfo")
                || is_probe_sidecar_os_name(&raw_name)
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
            let opened = match open_allowed_file(&path, self.cfg) {
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
            let meta = opened
                .file
                .metadata()
                .map_err(|error| scan_io(&path, error))?;
            let physical = PhysicalFileKey::new(&path, &meta)?;
            let existing = self.db.find_detail_by_path(&path_to_db(&path))?;
            let probe_sidecar_fingerprint = current_probe_sidecar_fingerprint(self.cfg, &path)?;
            let unchanged = existing.as_ref().is_some_and(|existing| {
                existing.size == physical.size
                    && existing.timestamp == physical.timestamp
                    && existing.device == physical.device
                    && existing.inode == physical.inode
            });
            let (probe_sidecar_current, probe_revision_current) = match &existing {
                Some(existing) => {
                    let (stored_fingerprint, revision) = self.db.detail_probe_state(existing.id)?;
                    (
                        stored_fingerprint.as_deref() == Some(probe_sidecar_fingerprint.as_str()),
                        revision >= db::STREAM_PROBE_REVISION,
                    )
                }
                None => (false, false),
            };
            if unchanged && probe_sidecar_current && probe_revision_current && !self.rebuild {
                continue;
            }
            self.queue_pending(PendingFile {
                album_art_sidecar: find_album_art_in_inventory(
                    &path,
                    &self.cfg.album_art_names,
                    &artwork_files,
                ),
                path,
                folder_id: folder_id.clone(),
                physical,
                probe_sidecar_fingerprint,
            })?;
        }
        if let Some(key) = dir_key {
            self.walk_stack.remove(&key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
