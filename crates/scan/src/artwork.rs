//! Artwork selection, bounded generation, inode sharing, and cache publication.

use crate::{
    acquire_scan_helper, directory_entry_is_allowed_file, ends_with_ci,
    extract_exif_thumbnail_with_limit_result, is_audio, is_direct_physical_path, is_image,
    is_video, lowercase_hex, open_allowed_file, path_from_db, path_is_allowed_dir,
    path_is_allowed_file, path_to_db, scan_io, HelperAdmissionError, LibraryDb, MediaHelperControl,
    PreparedPhysicalFile, RootedFile, ScanConfig, ScanError, ScanResult,
};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

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
pub(super) struct ArtworkInventory {
    exact: HashMap<OsString, PathBuf>,
    ascii_folded: HashMap<Vec<u8>, PathBuf>,
    #[cfg(test)]
    probes: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Default)]
pub(super) struct ArtworkSelectionCache {
    local: HashMap<PathBuf, ArtworkInventory>,
    physical: HashMap<PathBuf, ArtworkInventory>,
}

impl ArtworkSelectionCache {
    pub(super) fn select(&mut self, media_path: &Path, cfg: &ScanConfig) -> Option<PathBuf> {
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
    pub(super) fn new(mut files: Vec<(OsString, PathBuf)>) -> Self {
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

    pub(super) fn get(&self, candidate: &OsStr) -> Option<&PathBuf> {
        #[cfg(test)]
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.exact
            .get(candidate)
            .or_else(|| self.ascii_folded.get(&ascii_fold_os_str(candidate)))
    }

    #[cfg(test)]
    pub(super) fn probe_count(&self) -> usize {
        self.probes.load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn ascii_fold_os_str(name: &OsStr) -> Vec<u8> {
    name.as_encoded_bytes()
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect()
}

pub(super) fn find_album_art_in_inventory(
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
pub(super) fn reset_artwork_directory_read_count(dir: &Path) {
    ARTWORK_DIRECTORY_READ_PROBES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(dir.to_path_buf(), 0);
}

#[cfg(test)]
pub(super) fn take_artwork_directory_read_count(dir: &Path) -> usize {
    ARTWORK_DIRECTORY_READ_PROBES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(dir)
        .unwrap_or_default()
}

pub fn find_album_art(video_path: &Path) -> Option<PathBuf> {
    find_album_art_with_config(video_path, &[])
}

pub(super) fn find_album_art_for_config(video_path: &Path, cfg: &ScanConfig) -> Option<PathBuf> {
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

pub(super) fn find_album_art_for_physical_target_cached(
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

pub(super) fn is_album_art_os_name_for_config(name: &OsStr, cfg: &ScanConfig) -> bool {
    is_builtin_album_art_os_name(name)
        || cfg
            .album_art_names
            .iter()
            .any(|pattern| configured_art_pattern_matches(name, pattern))
}

#[cfg(test)]
pub(super) fn source_image_identity(path: &Path) -> ScanResult<String> {
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

pub(super) fn prepare_album_art_with_sidecar(
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
pub(super) enum PreparedAlbumArt {
    Ready(Option<PathBuf>),
    /// A per-file helper or filesystem failure must not clear artwork already
    /// attached to a published detail or abort an otherwise valid scan.
    PreserveExisting,
}

pub(super) fn recover_prepared_album_art(
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

pub(super) fn apply_index_album_art(
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

pub(super) fn attach_album_art_for_index(
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
pub(super) enum IndexArtworkSelection<'a> {
    Discover,
    Selected(Option<&'a Path>),
}

pub(super) fn remove_stale_cached_art(cfg: &ScanConfig, paths: &[String]) {
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

pub fn artwork_path_matches_media(sidecar: &Path, media: &Path) -> bool {
    artwork_path_matches_media_with_names(sidecar, media, &[])
}

pub(super) fn artwork_path_matches_media_with_names(
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

pub(super) fn refresh_artwork_event(
    db: &LibraryDb,
    cfg: &ScanConfig,
    sidecar: &Path,
) -> ScanResult<bool> {
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

pub(super) fn recoverable_artwork_error(cfg: &ScanConfig, error: &ScanError) -> bool {
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

pub(super) fn attach_album_art_in_dir(
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
