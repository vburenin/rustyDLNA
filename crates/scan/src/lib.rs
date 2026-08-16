//! Library scan: skip rules, NFO dates, captions, inode reuse, SQLite store.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub mod db;
pub mod watch;
pub use db::{mime_to_ext, LibraryDb};
pub use watch::{repair_objects_if_needed, run_inotify};

use rusty_dlna_protocol::object_id::{
    BROWSEDIR_ID, IMAGE_ALL_ID, IMAGE_ID, MUSIC_ALL_ID, MUSIC_ID, RECENT_MAX, ROOT_ID,
    VIDEO_ALL_ID, VIDEO_DIR_ID, VIDEO_ID, VIDEO_RECENT_ID,
};
use rusty_dlna_protocol::w3c_date_from_unix;
use rusty_dlna_protocol::w3c_normalize_date;

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
        .and_then(|s| s.to_str())
        .unwrap_or("");
    path_excluded(path, name, cfg)
        || path
            .components()
            .any(|c| c.as_os_str().to_str().is_some_and(is_skipped_dir))
        || is_unfinished_name(name)
        || looks_like_sample_file(name)
        || is_album_art_name(name)
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
    [
        ".mpg", ".mpeg", ".avi", ".divx", ".asf", ".wmv", ".mp4", ".m4v", ".mts",
        ".m2ts", ".m2t", ".mkv", ".vob", ".ts", ".flv", ".xvid", ".mov", ".3gp",
        ".rm", ".rmvb", ".webm",
    ]
    .iter()
    .any(|s| ends_with_ci(name, s))
}

/// dialect `is_audio`.
pub fn is_audio(name: &str) -> bool {
    [
        ".mp3", ".flac", ".wma", ".asf", ".fla", ".flc", ".m4a", ".aac", ".mp4",
        ".m4p", ".wav", ".ogg", ".pcm", ".3gp", ".dsf", ".dff",
    ]
    .iter()
    .any(|s| ends_with_ci(name, s))
}

/// dialect `is_image` — JPEG only (not PNG).
pub fn is_image(name: &str) -> bool {
    ends_with_ci(name, ".jpg") || ends_with_ci(name, ".jpeg")
}

pub fn is_album_art_name(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    matches!(
        l.as_str(),
        "cover.jpg"
            | "cover.jpeg"
            | "folder.jpg"
            | "folder.jpeg"
            | "poster.jpg"
            | "poster.jpeg"
            | "albumart.jpg"
            | "albumartsmall.jpg"
            | "album.jpg"
            | "thumb.jpg"
    ) || l.ends_with("-poster.jpg")
        || l.ends_with("-fanart.jpg")
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
        (self.video && is_video(name)) || (self.audio && is_audio(name)) || (self.image && is_image(name))
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
        if p.chars().all(|c| matches!(c, 'A' | 'a' | 'V' | 'v' | 'P' | 'p')) && !rest.is_empty() {
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

/// dialect `GetVideoMetadata`: reject non-media. Strong container magic
/// (EBML/ftyp/RIFF/…) is proof the file is a real bitstream, not text.
/// Ambiguous headers (TS/MPEG/MP3) get a short `ffprobe`.
pub fn file_is_viable(path: &Path) -> bool {
    match sniff_container(path) {
        Sniff::Reject => false,
        Sniff::Strong => true,
        Sniff::Weak => ffprobe_has_av_stream(path).unwrap_or(false),
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

fn ffprobe_has_av_stream(path: &Path) -> Option<bool> {
    let out = std::process::Command::new("ffprobe")
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
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return Some(false);
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.lines().any(|l| l.contains("video") || l.contains("audio")))
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

fn mime_and_class(name: &str) -> (&'static str, &'static str, &'static str) {
    let l = name.to_ascii_lowercase();
    if l.ends_with(".mkv") {
        ("video/x-matroska", "item.videoItem", "mkv")
    } else if l.ends_with(".mp4") || l.ends_with(".m4v") {
        ("video/mp4", "item.videoItem", "mp4")
    } else if l.ends_with(".avi") {
        ("video/x-msvideo", "item.videoItem", "avi")
    } else if l.ends_with(".mov") {
        ("video/quicktime", "item.videoItem", "mov")
    } else if l.ends_with(".ts") || l.ends_with(".m2ts") {
        ("video/mpeg", "item.videoItem", "ts")
    } else if l.ends_with(".mp3") {
        ("audio/mpeg", "item.audioItem.musicTrack", "mp3")
    } else if l.ends_with(".flac") {
        ("audio/x-flac", "item.audioItem.musicTrack", "flac")
    } else if l.ends_with(".jpg") || l.ends_with(".jpeg") {
        ("image/jpeg", "item.imageItem.photo", "jpg")
    } else if l.ends_with(".png") {
        ("image/png", "item.imageItem.photo", "png")
    } else {
        ("application/octet-stream", "item.videoItem", "bin")
    }
}

pub fn nfo_date_from_text(text: &str) -> Option<String> {
    for tag in ["premiered", "aired", "year"] {
        if let Some(start) = text.find(&format!("<{tag}>")) {
            let rest = &text[start + tag.len() + 2..];
            if let Some(end) = rest.find(&format!("</{tag}>")) {
                let raw = rest[..end].trim();
                if !raw.is_empty() {
                    return Some(w3c_normalize_date(raw));
                }
            }
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct SourceProbe {
    pub container: String,
    pub video: String,
    pub hdr: String,
    pub audio: String,
    pub width: u32,
    pub height: u32,
}

impl Default for SourceProbe {
    fn default() -> Self {
        Self {
            container: "mkv".into(),
            video: "hevc".into(),
            hdr: "sdr".into(),
            audio: "aac".into(),
            width: 0,
            height: 0,
        }
    }
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
}

/// dialect `GetVideoMetadata` / lav: duration, bitrate/8, WxH, audio.
pub fn probe_av_meta(path: &Path) -> Option<AvMeta> {
    let out = std::process::Command::new("ffprobe")
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
        .arg(path)
        .output()
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
    if meta.duration.is_none()
        && meta.bitrate.is_none()
        && meta.resolution.is_none()
    {
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
        c.add_container(IMAGE_ID, ROOT_ID, "Pictures", "container.storageFolder", true);
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
            MUSIC_ALL_ID,
            MUSIC_ID,
            "All Music",
            "container.storageFolder",
            true,
        );
        c.add_container(
            IMAGE_ALL_ID,
            IMAGE_ID,
            "All Pictures",
            "container.storageFolder",
            true,
        );
        c.link_child(ROOT_ID, BROWSEDIR_ID);
        c.link_child(ROOT_ID, MUSIC_ID);
        c.link_child(ROOT_ID, VIDEO_ID);
        c.link_child(ROOT_ID, IMAGE_ID);
        c.link_child(VIDEO_ID, VIDEO_ALL_ID);
        c.link_child(VIDEO_ID, VIDEO_DIR_ID);
        c.link_child(VIDEO_ID, VIDEO_RECENT_ID);
        c.link_child(MUSIC_ID, MUSIC_ALL_ID);
        c.link_child(IMAGE_ID, IMAGE_ALL_ID);
        c
    }

    fn add_container(&mut self, id: &str, parent: &str, title: &str, class: &str, searchable: bool) {
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

    fn next_child_id(&self, parent: &str) -> String {
        let n = self
            .containers
            .get(parent)
            .map(|c| c.children.len() + 1)
            .unwrap_or(1);
        format!("{parent}${n:X}")
    }

    pub fn get_item_by_detail(&self, id: i64) -> Option<&MediaItem> {
        let oid = self.by_detail.get(&id)?;
        self.items.get(oid)
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
        if id == VIDEO_RECENT_ID {
            let mut all = self.recent_videos();
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
                    self.items.get(oid).cloned().map(CatalogChild::Item)
                }
            })
            .collect();
        Some((page, total))
    }

    pub fn displayed_child_count(&self, id: &str) -> u32 {
        if id == VIDEO_RECENT_ID {
            return self.recent_count;
        }
        self.containers
            .get(id)
            .map(|c| c.children.len() as u32)
            .unwrap_or(0)
    }

    pub fn displayed_container_count(&self, id: &str) -> u32 {
        if id == VIDEO_RECENT_ID {
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
        let owned = if self.recent_ids.is_empty() {
            self.compute_recent_ids()
        } else {
            Vec::new()
        };
        let ids: &[String] = if owned.is_empty() {
            &self.recent_ids
        } else {
            &owned
        };
        ids.iter()
            .filter_map(|id| {
                let it = self.items.get(id)?;
                let mut clone = it.clone();
                clone.object_id = format!("{VIDEO_RECENT_ID}${id}");
                clone.parent_id = VIDEO_RECENT_ID.to_string();
                Some(CatalogChild::Item(clone))
            })
            .collect()
    }

    fn compute_recent_ids(&self) -> Vec<String> {
        let mut items: Vec<&MediaItem> = self
            .items
            .values()
            .filter(|i| {
                i.class.contains("video")
                    && i.ref_id.is_none()
                    && i.object_id.starts_with(BROWSEDIR_ID)
            })
            .collect();
        items.sort_by(|a, b| {
            b.mtime
                .cmp(&a.mtime)
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
            if ids.len() == RECENT_MAX {
                break;
            }
        }
        ids
    }

    fn rebuild_recent_index(&mut self) {
        self.recent_ids = self.compute_recent_ids();
        self.recent_count = self.recent_ids.len() as u32;
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
        self.items.get(id).cloned().map(CatalogChild::Item)
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
                self.add_container(
                    &vid,
                    &vparent,
                    &cont.title,
                    "container.storageFolder",
                    true,
                );
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
    Item(MediaItem),
}

#[derive(Clone, Debug, Default)]
pub struct ScanConfig {
    pub media_dirs: Vec<PathBuf>,
    pub exclude_dirs: Vec<String>,
    pub exclude_files: Vec<String>,
    /// dialect `media_dir=V,…` filter. Default = all (AVP).
    pub types: MediaTypes,
    /// dialect `files.db`. None = in-memory SQLite (tests).
    pub db_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanDelta {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

pub fn path_is_live_file(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// If `stored` is missing, rebase it onto a configured media root.
///
/// Host scans often persist a realpath while the container only mounts
/// that tree at a media root. Match the media-root directory name in
/// `stored` and try the remainder under each configured root.
pub fn rebase_media_path(stored: &Path, roots: &[PathBuf]) -> PathBuf {
    if stored.as_os_str().is_empty() {
        return stored.to_path_buf();
    }
    if path_is_live_file(stored) {
        return stored.to_path_buf();
    }
    for root in roots {
        let Some(key) = root.file_name() else {
            continue;
        };
        if let Some(rel) = rel_after_component(stored, key) {
            let cand = root.join(&rel);
            if path_is_live_file(&cand) {
                return cand;
            }
        }
    }
    stored.to_path_buf()
}

/// Relative path after the media-root directory name (`video` in
/// `/storage/video` and `/mnt/pool/video`).
pub fn media_rel_key(path: &Path, roots: &[PathBuf]) -> String {
    for root in roots {
        if let Some(key) = root.file_name() {
            if let Some(rel) = rel_after_component(path, key) {
                return rel.to_string_lossy().into_owned();
            }
        }
        if let Ok(rel) = path.strip_prefix(root) {
            return rel.to_string_lossy().into_owned();
        }
    }
    path.to_string_lossy().into_owned()
}

fn rel_after_component(path: &Path, key: &std::ffi::OsStr) -> Option<PathBuf> {
    let comps: Vec<_> = path.components().collect();
    let i = comps.iter().position(|c| c.as_os_str() == key)?;
    let mut rel = PathBuf::new();
    for c in &comps[i + 1..] {
        rel.push(c.as_os_str());
    }
    Some(rel)
}

fn paths_are_same_media(stored: &str, event: &Path, roots: &[PathBuf]) -> bool {
    if Path::new(stored) == event {
        return true;
    }
    let key = media_rel_key(event, roots);
    !key.is_empty() && media_rel_key(Path::new(stored), roots) == key
}

fn path_is_under_watched(stored: &str, dir: &Path, roots: &[PathBuf]) -> bool {
    let stored_p = Path::new(stored);
    if stored_p == dir || stored_p.starts_with(dir) {
        return true;
    }
    let drel = media_rel_key(dir, roots);
    if drel.is_empty() {
        return false;
    }
    let frel = media_rel_key(stored_p, roots);
    frel == drel || frel.starts_with(&format!("{drel}/"))
}

fn open_library(cfg: &ScanConfig) -> Option<LibraryDb> {
    match &cfg.db_path {
        Some(p) => LibraryDb::open(p).ok(),
        None => None,
    }
}

/// Drop DETAILS/OBJECTS for `path`, matching host-realpath vs container-mount
/// prefixes (e.g. `/mnt/pool/video/…` vs `/storage/video/…`).
pub fn forget_path(cfg: &ScanConfig, path: &Path) -> usize {
    forget_matching(cfg, path, false)
}

/// Drop every DETAILS row under `dir`, using the same prefix-alias rules.
pub fn forget_tree(cfg: &ScanConfig, dir: &Path) -> usize {
    forget_matching(cfg, dir, true)
}

fn forget_matching(cfg: &ScanConfig, path: &Path, tree: bool) -> usize {
    let _write = library_write_guard();
    let Some(db) = open_library(cfg) else {
        return 0;
    };
    let rows = db.all_detail_stats().unwrap_or_default();
    let mut n = 0usize;
    for (p, _, _, _, _, _) in rows {
        let hit = if tree {
            path_is_under_watched(&p, path, &cfg.media_dirs)
        } else {
            paths_are_same_media(&p, path, &cfg.media_dirs)
        };
        if hit {
            n += db.remove_path_and_symlink_aliases(&p).unwrap_or(0);
        }
    }
    n
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
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn path_excluded(path: &Path, name: &str, cfg: &ScanConfig) -> bool {
    for e in &cfg.exclude_dirs {
        if dir_exclude_matches(path, name, e) {
            return true;
        }
    }
    for e in &cfg.exclude_files {
        if name == e {
            return true;
        }
    }
    false
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

fn captions_for(file: &Path) -> Vec<Caption> {
    let parent = match file.parent() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let stem = match file.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut caps = Vec::new();
    let rd = match std::fs::read_dir(parent) {
        Ok(r) => r,
        Err(_) => return caps,
    };
    let mut names: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(stem) && is_caption_name(n))
        })
        .collect();
    names.sort();
    for (i, p) in names.into_iter().enumerate() {
        let ext = caption_ext(p.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        caps.push(Caption {
            index: i as u32,
            path: p,
            ext: ext.into(),
        });
    }
    caps
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

/// Filename / sidecar-free probe. Catalog load must not open a
/// `.probe.toml` next to every video (that was 2 failed opens × N files).
pub fn probe_from_name(title: &str, path: &Path, ext: &str) -> SourceProbe {
    let mut p = SourceProbe::default();
    p.container = match ext {
        "mp4" | "m4v" => "mp4".into(),
        "avi" => "avi".into(),
        _ => "mkv".into(),
    };
    if let Some(hdr) = guess_hdr_from_name(title).or_else(|| {
        path.file_name()
            .and_then(|n| n.to_str())
            .and_then(guess_hdr_from_name)
    }) {
        p.hdr = hdr.to_string();
    }
    p
}

/// ffprobe codec / Dolby Vision profile. Used when no sidecar exists.
pub fn probe_stream_identity(path: &Path) -> Option<SourceProbe> {
    let out = std::process::Command::new("ffprobe")
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
        .arg(path)
        .output()
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
            "dv_profile" => {
                if dv_profile.is_none() {
                    dv_profile = v.trim().parse().ok();
                }
            }
            _ => {}
        }
    }
    if video.is_empty() && audio.is_empty() && dv_profile.is_none() {
        return None;
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("mkv");
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

fn nfo_for(file: &Path) -> Option<String> {
    let nfo = file.with_extension("nfo");
    std::fs::read_to_string(nfo)
        .ok()
        .and_then(|t| nfo_date_from_text(&t))
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
    match LibraryDb::open(path).and_then(|db| {
        let n = db.detail_count().unwrap_or(0);
        let cat = db.load_catalog()?;
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

pub fn scan(cfg: &ScanConfig) -> Catalog {
    scan_inner(cfg, true)
}

/// Background refresh: do not wipe OBJECTS; skip files whose SIZE+TIMESTAMP
/// are unchanged; never descend into `exclude_dir` (e.g. `incomplete`).
pub fn scan_refresh(cfg: &ScanConfig) -> Catalog {
    scan_inner(cfg, false)
}

fn scan_inner(cfg: &ScanConfig, rebuild: bool) -> Catalog {
    let db = match &cfg.db_path {
        Some(p) => LibraryDb::open(p).expect("open files.db"),
        None => LibraryDb::open_memory().expect("open memory db"),
    };
    if rebuild {
        let _ = db.clear_objects();
    }
    let _ = db.seed_virtual_containers();
    let _ = db.begin();
    let media_dirs = cfg.media_dirs.clone();
    let mut walk_stack: HashMap<(u64, u64), ()> = HashMap::new();
    let mut indexed = 0usize;
    for root in &media_dirs {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let title = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("media")
            .to_string();
        walk_into_db(
            &db,
            cfg,
            &mut walk_stack,
            &root,
            BROWSEDIR_ID,
            &title,
            rebuild,
            &mut indexed,
        );
    }
    let _ = db.commit();
    let _ = db.prune_missing_files();
    let _ = db.prune_excluded_paths(cfg);
    let _ = db.prune_empty_folders();
    let n = db.detail_count().unwrap_or(0);
    tracing::info!(
        target: "rusty_dlna",
        details = n,
        path = %db.path.display(),
        "scan complete"
    );
    db.load_catalog().unwrap_or_else(|_| Catalog::new())
}

pub(crate) fn library_write_guard() -> std::sync::MutexGuard<'static, ()> {
    static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
    M.lock().unwrap_or_else(|e| e.into_inner())
}

/// List files on disk (skip `exclude_dir` / junk), compare to DETAILS, apply
/// only adds/changes/deletes. Does not wipe OBJECTS or rewrite unchanged rows.
pub fn monitor(cfg: &ScanConfig) -> (Option<Catalog>, ScanDelta) {
    monitor_dirty(cfg, &[])
}

/// Like [`monitor`], but restat `dirty` paths (inotify CLOSE_WRITE / move
/// targets) so SIZE/TIMESTAMP stay current without stating the whole tree.
pub fn monitor_dirty(cfg: &ScanConfig, dirty: &[PathBuf]) -> (Option<Catalog>, ScanDelta) {
    let _write = library_write_guard();
    let db = match &cfg.db_path {
        Some(p) => LibraryDb::open(p).expect("open files.db"),
        None => LibraryDb::open_memory().expect("open memory db"),
    };
    let listed = list_media_files(cfg);
    let db_rows = db.all_detail_stats().unwrap_or_default();
    let mut listed_by_rel: HashMap<String, &ListedFile> = HashMap::new();
    for (p, st) in &listed {
        listed_by_rel.insert(media_rel_key(Path::new(p), &cfg.media_dirs), st);
    }
    let dirty_rels: HashSet<String> = dirty
        .iter()
        .map(|p| media_rel_key(p, &cfg.media_dirs))
        .collect();
    let mut in_db_by_rel: HashMap<String, Vec<(String, i64, i64, i64, i64, i64)>> = HashMap::new();
    for (p, id, sz, ts, dev, ino) in &db_rows {
        in_db_by_rel
            .entry(media_rel_key(Path::new(p), &cfg.media_dirs))
            .or_default()
            .push((p.clone(), *id, *sz, *ts, *dev, *ino));
    }
    let _ = db.seed_virtual_containers();
    let _ = db.begin();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut changed = 0usize;
    for (path, ..) in &db_rows {
        let key = media_rel_key(Path::new(path), &cfg.media_dirs);
        if path_is_unwanted(Path::new(path), cfg) || !listed_by_rel.contains_key(&key) {
            if db.remove_path_and_symlink_aliases(path).unwrap_or(0) > 0 {
                removed += 1;
            }
        }
    }
    // Same on-disk file stored under the host realpath and the container
    // mount is one library item. Keep the live listed path, drop extras.
    for (key, rows) in &in_db_by_rel {
        if rows.len() < 2 || !listed_by_rel.contains_key(key) {
            continue;
        }
        let listed_path = listed_by_rel[key].path.to_string_lossy();
        let keep = rows
            .iter()
            .find(|(p, ..)| p.as_str() == listed_path)
            .or_else(|| {
                rows.iter()
                    .find(|(p, ..)| path_is_live_file(Path::new(p)))
            })
            .map(|(p, ..)| p.clone());
        let Some(keep) = keep else {
            continue;
        };
        for (p, id, ..) in rows {
            if p == &keep {
                continue;
            }
            if db.remove_detail_id(*id).is_ok() {
                removed += 1;
            }
        }
    }
    for (path_s, st) in &listed {
        let key = media_rel_key(Path::new(path_s), &cfg.media_dirs);
        let existing = in_db_by_rel.get(&key).and_then(|v| {
            v.iter()
                .find(|(p, ..)| p == path_s)
                .or_else(|| v.first())
        });
        match existing {
            Some((db_path, id, sz, ts, dev, ino)) => {
                if dirty_rels.contains(&key) {
                    if let Some(meta) = std::fs::metadata(&st.path).ok().filter(|m| m.is_file()) {
                        let size = meta.len() as i64;
                        let mtime = file_mtime_unix(&meta);
                        if size != *sz || mtime != *ts {
                            if !file_is_viable(&st.path) {
                                let _ = db.remove_path_and_symlink_aliases(db_path);
                                removed += 1;
                                continue;
                            }
                            let _ = db.update_detail_stat(*id, size, mtime);
                            changed += 1;
                        }
                    }
                }
                if attach_listed_if_missing(&db, cfg, &st.path, *id, *dev, *ino) {
                    changed += 1;
                }
            }
            None => {
                // Rel-key miss is not "new": the row may already exist at
                // this exact path (genre aliases) or under another prefix.
                if db.find_detail_by_path(path_s).ok().flatten().is_some() {
                    continue;
                }
                if let Some(folder_id) = ensure_folder_chain(&db, cfg, &st.path) {
                    if index_one_file(&db, cfg, &st.path, &folder_id) {
                        added += 1;
                    }
                }
            }
        }
    }
    let _ = db.commit();
    removed += db.prune_empty_folders().unwrap_or(0);
    let delta = ScanDelta {
        added,
        removed,
        changed,
    };
    if added + removed + changed == 0 {
        return (None, delta);
    }
    let n = db.detail_count().unwrap_or(0);
    tracing::info!(
        target: "rusty_dlna",
        added,
        removed,
        changed,
        details = n,
        path = %db.path.display(),
        "library monitor"
    );
    (
        db.load_catalog().ok(),
        delta,
    )
}

#[derive(Clone)]
struct ListedFile {
    path: PathBuf,
}

fn attach_listed_if_missing(
    db: &LibraryDb,
    cfg: &ScanConfig,
    path: &Path,
    detail_id: i64,
    device: i64,
    inode: i64,
) -> bool {
    let Some(folder_id) = ensure_folder_chain(db, cfg, path) else {
        return false;
    };
    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if title.is_empty() {
        return false;
    }
    if db.folder_has_inode_named(&folder_id, device, inode, title) {
        return false;
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let (_, class, _) = mime_and_class(name);
    attach_objects(
        db,
        &folder_id,
        detail_id,
        title,
        class,
        device as u64,
        inode as u64,
    );
    true
}

fn list_media_files(cfg: &ScanConfig) -> HashMap<String, ListedFile> {
    let mut out = HashMap::new();
    let mut walk_stack: HashMap<(u64, u64), ()> = HashMap::new();
    for root in &cfg.media_dirs {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let title = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("media")
            .to_string();
        list_into(&mut out, cfg, &mut walk_stack, &root, &title);
    }
    out
}

fn list_into(
    out: &mut HashMap<String, ListedFile>,
    cfg: &ScanConfig,
    walk_stack: &mut HashMap<(u64, u64), ()>,
    dir: &Path,
    title: &str,
) {
    if path_excluded(dir, title, cfg) {
        return;
    }
    let dir_key = std::fs::metadata(dir).ok().map(|m| inode_key(&m));
    if let Some(key) = dir_key {
        if walk_stack.contains_key(&key) {
            return;
        }
        walk_stack.insert(key, ());
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for ent in rd.filter_map(|e| e.ok()) {
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = match ent.file_type() {
            Ok(t) if t.is_dir() => true,
            Ok(t) if t.is_symlink() => path.is_dir(),
            _ => false,
        };
        if is_dir {
            if is_skipped_dir(&name) || path_excluded(&path, &name, cfg) {
                continue;
            }
            list_into(out, cfg, walk_stack, &path, &name);
            continue;
        }
        if is_unfinished_name(&name)
            || looks_like_sample_file(&name)
            || is_caption_name(&name)
            || ends_with_ci(&name, ".nfo")
            || name.ends_with(".probe.toml")
            || path_excluded(&path, &name, cfg)
            || is_album_art_name(&name)
            || !cfg.types.allows(&name)
        {
            continue;
        }
        // Existence only. Stat the whole tree on every reconcile was
        // tens of thousands of HDD/NAS syscalls and stalled Browse.
        let path_s = path.to_string_lossy().into_owned();
        out.insert(
            path_s,
            ListedFile {
                path,
            },
        );
    }
    if let Some(key) = dir_key {
        walk_stack.remove(&key);
    }
}

pub(crate) fn ensure_folder_chain(db: &LibraryDb, cfg: &ScanConfig, file: &Path) -> Option<String> {
    let parent = file.parent()?;
    if cfg.media_dirs.is_empty() {
        return None;
    }
    // Walked path only. Do not canonicalize — that collapses a directory
    // symlink (genres/BY_YEAR/2010/Movies/Despicable Me → kids/Movies/…)
    // into the real folder and every alias lands in the same Browse list.
    let rel = media_rel_key(parent, &cfg.media_dirs);
    if rel == parent.to_string_lossy() {
        return None;
    }
    let root_title = matching_root_title(parent, &cfg.media_dirs).unwrap_or("media");
    if path_excluded(parent, root_title, cfg) {
        return None;
    }
    let mut folder_id = folder_object_id(db, BROWSEDIR_ID, root_title);
    for comp in Path::new(&rel).components() {
        let name = comp.as_os_str().to_string_lossy();
        if name.is_empty() || name == "." {
            continue;
        }
        if is_skipped_dir(&name) || path_excluded(parent, &name, cfg) {
            return None;
        }
        folder_id = folder_object_id(db, &folder_id, &name);
    }
    Some(folder_id)
}

fn matching_root_title<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a str> {
    for root in roots {
        if path.starts_with(root) {
            return root.file_name().and_then(|s| s.to_str());
        }
        if let Some(key) = root.file_name() {
            if path.components().any(|c| c.as_os_str() == key) {
                return key.to_str();
            }
        }
    }
    roots
        .first()
        .and_then(|r| r.file_name())
        .and_then(|s| s.to_str())
}

pub(crate) fn index_one_file(db: &LibraryDb, cfg: &ScanConfig, path: &Path, folder_id: &str) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() || !cfg.types.allows(&name) {
        return false;
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => return false,
    };
    let (dev, ino) = inode_key(&meta);
    let (mime, class, _ext) = mime_and_class(&name);
    let mtime = file_mtime_unix(&meta);
    let size = meta.len() as i64;
    let path_s = path.to_string_lossy().into_owned();
    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&name)
        .to_string();

    if let Some((id, old_sz, old_ts)) = db.find_detail_by_path(&path_s).ok().flatten() {
        if old_sz != size || old_ts != mtime {
            let _ = db.update_detail_stat(id, size, mtime);
        }
        attach_objects(db, folder_id, id, &title, class, dev, ino);
        return true;
    }

    // rustyDLNA clone_detail_for_path: symlink/hardlink of a known inode
    // reuses TITLE/DATE/MIME/DLNA_PN — no GetVideoMetadata / ffprobe.
    if let Ok(Some((src, src_ts, src_path))) = db.find_inode_source(dev as i64, ino as i64) {
        let src_key = media_rel_key(Path::new(&src_path), &cfg.media_dirs);
        let new_key = media_rel_key(path, &cfg.media_dirs);
        // Host realpath vs container mount of the same file — not a new alias.
        if !src_key.is_empty() && src_key == new_key {
            if let Some((_, old_sz, old_ts)) = db.find_detail_by_path(&src_path).ok().flatten() {
                if old_sz != size || old_ts != mtime {
                    let _ = db.update_detail_stat(src, size, mtime);
                }
            }
            attach_objects(db, folder_id, src, &title, class, dev, ino);
            return true;
        }
        if src_ts >= mtime {
            if let Ok(id) =
                db.clone_detail_for_path(src, &path_s, size, mtime, dev as i64, ino as i64)
            {
                attach_objects(db, folder_id, id, &title, class, dev, ino);
                return true;
            }
        }
    }

    if !file_is_viable(path) {
        return false;
    }
    let date = nfo_for(path).unwrap_or_else(|| file_mtime_date(path));
    let Ok(detail) = db.insert_detail(
        &path_s,
        size,
        mtime,
        &title,
        &date,
        mime,
        dev as i64,
        ino as i64,
        None,
    ) else {
        return false;
    };
    let caps = captions_for(path);
    let _ = db.replace_captions(detail, &caps);
    if let Some(av) = probe_av_meta(path) {
        let _ = db.update_detail_av_meta(
            detail,
            av.duration.as_deref(),
            av.bitrate,
            av.resolution.as_deref(),
            av.channels,
            av.samplerate,
        );
    }
    attach_objects(db, folder_id, detail, &title, class, dev, ino);
    true
}

fn allocate_child_id(db: &LibraryDb, parent_id: &str) -> String {
    let mut n = db.next_child_seq(parent_id).unwrap_or(1);
    loop {
        let id = format!("{parent_id}${n:X}");
        if !db.object_exists(&id) {
            return id;
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
) {
    if db.folder_has_inode_named(folder_id, dev as i64, ino as i64, title) {
        return;
    }
    let object_id = match db.find_child_object(folder_id, title) {
        Some(oid) => match db.object_detail_id(&oid) {
            None => oid,
            Some(did) if did == detail => oid,
            // Same title, different file — never steal the other item's row.
            Some(_) => allocate_child_id(db, folder_id),
        },
        None => allocate_child_id(db, folder_id),
    };
    let _ = db.upsert_object(&object_id, folder_id, class, Some(detail), title, None);
    if class.contains("video") {
        if !db
            .all_video_has_inode(dev as i64, ino as i64)
            .unwrap_or(false)
        {
            let vid = format!("{VIDEO_ALL_ID}${detail:X}");
            let _ = db.upsert_object(
                &vid,
                VIDEO_ALL_ID,
                class,
                Some(detail),
                title,
                Some(&object_id),
            );
        }
        ensure_typed_dir_chain(db, folder_id, VIDEO_DIR_ID);
        let vdir = browse_to_typed_dir(folder_id, VIDEO_DIR_ID);
        let vobj = browse_to_typed_dir(&object_id, VIDEO_DIR_ID);
        let _ = db.upsert_object(&vobj, &vdir, class, Some(detail), title, Some(&object_id));
    } else if class.contains("audio") {
        let aid = format!("{MUSIC_ALL_ID}${detail:X}");
        let _ = db.upsert_object(&aid, MUSIC_ALL_ID, class, Some(detail), title, Some(&object_id));
    } else if class.contains("image") {
        let iid = format!("{IMAGE_ALL_ID}${detail:X}");
        let _ = db.upsert_object(&iid, IMAGE_ALL_ID, class, Some(detail), title, Some(&object_id));
    }
}

/// Rebuild OBJECTS from live DETAILS paths without re-probing files.
/// Fixes folder-id reuse that left children under the wrong title
/// (e.g. a new show folder inheriting a deleted sibling's items).
pub fn rebuild_objects(cfg: &ScanConfig) -> Catalog {
    let db = match &cfg.db_path {
        Some(p) => LibraryDb::open(p).expect("open files.db"),
        None => return Catalog::new(),
    };
    let _ = db.prune_missing_files();
    let _ = db.prune_excluded_paths(cfg);
    let rows = db.all_detail_stats().unwrap_or_default();
    let _ = db.clear_objects();
    let _ = db.seed_virtual_containers();
    let _ = db.begin();
    let mut n = 0usize;
    for (path, ..) in &rows {
        let p = Path::new(path);
        if !path_is_live_file(p) || path_is_unwanted(p, cfg) {
            continue;
        }
        if let Some(folder) = ensure_folder_chain(&db, cfg, p) {
            if index_one_file(&db, cfg, p, &folder) {
                n += 1;
            }
        }
    }
    let _ = db.commit();
    let _ = db.prune_empty_folders();
    tracing::info!(target: "rusty_dlna", files = n, "objects rebuilt");
    db.load_catalog().unwrap_or_else(|_| Catalog::new())
}

/// Re-walk the tree into SQLite, drop missing/dangling paths (and their
/// symlink aliases), return a fresh catalog + delta vs `prev`.
pub fn rescan(cfg: &ScanConfig, prev: &Catalog) -> (Catalog, ScanDelta) {
    match monitor(cfg) {
        (Some(next), delta) => (next, delta),
        (None, delta) => (prev.clone(), delta),
    }
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

fn ensure_typed_dir_chain(db: &LibraryDb, browse_folder_id: &str, typed: &str) {
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
        let name = db.object_name(&browse).unwrap_or_else(|| browse.clone());
        let _ = db.upsert_object(
            &typed_id,
            &typed_parent,
            "container.storageFolder",
            None,
            &name,
            Some(&browse),
        );
    }
}

fn folder_object_id(db: &LibraryDb, parent_id: &str, title: &str) -> String {
    if let Some(id) = db.find_child_object(parent_id, title) {
        return id;
    }
    let id = allocate_child_id(db, parent_id);
    let _ = db.upsert_object(&id, parent_id, "container.storageFolder", None, title, None);
    id
}

fn walk_into_db(
    db: &LibraryDb,
    cfg: &ScanConfig,
    walk_stack: &mut HashMap<(u64, u64), ()>,
    dir: &Path,
    parent_id: &str,
    title: &str,
    rebuild: bool,
    indexed: &mut usize,
) {
    if path_excluded(dir, title, cfg) {
        return;
    }
    let dir_key = std::fs::metadata(dir).ok().map(|m| inode_key(&m));
    if let Some(key) = dir_key {
        // Cycle only (symlink dir pointing at an ancestor). Alias trees stay walkable.
        if walk_stack.contains_key(&key) {
            return;
        }
        walk_stack.insert(key, ());
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let folder_id = if parent_id == BROWSEDIR_ID {
        folder_object_id(db, parent_id, title)
    } else {
        parent_id.to_string()
    };

    let mut ents: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    ents.sort_by_key(|e| e.file_name());
    for ent in ents {
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = match ent.file_type() {
            Ok(t) if t.is_dir() => true,
            Ok(t) if t.is_symlink() => path.is_dir(),
            _ => false,
        };
        if is_dir {
            if is_skipped_dir(&name) || path_excluded(&path, &name, cfg) {
                continue;
            }
            let child = folder_object_id(db, &folder_id, &name);
            walk_into_db(db, cfg, walk_stack, &path, &child, &name, rebuild, indexed);
            continue;
        }
        if is_unfinished_name(&name)
            || looks_like_sample_file(&name)
            || is_caption_name(&name)
            || ends_with_ci(&name, ".nfo")
            || name.ends_with(".probe.toml")
            || path_excluded(&path, &name, cfg)
            || is_album_art_name(&name)
        {
            continue;
        }
        if !cfg.types.allows(&name) {
            continue;
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let path_s = path.to_string_lossy().into_owned();
        let existing = db.find_detail_by_path(&path_s).ok().flatten();
        let size = meta.len() as i64;
        let mtime = file_mtime_unix(&meta);
        let unchanged = existing
            .as_ref()
            .is_some_and(|(_, old_sz, old_ts)| *old_sz == size && *old_ts == mtime);
        if unchanged && !rebuild {
            continue;
        }
        if index_one_file(db, cfg, &path, &folder_id) {
            *indexed += 1;
            if *indexed % 100 == 0 {
                tracing::info!(target: "rusty_dlna", indexed, "scan progress");
            }
        }
    }
    if let Some(key) = dir_key {
        walk_stack.remove(&key);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_named_mkv_is_not_viable() {
        let p = std::env::temp_dir().join(format!("rusty-not-video-{}.mkv", std::process::id()));
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
        assert_eq!(guess_hdr_from_name("clip.dv-p7.mkv"), Some("dv-p7"));
        assert_eq!(guess_hdr_from_name("clip.dv-p8.mkv"), Some("dv-p8"));
    }

    #[test]
    fn page_children_matches_children_of_slice() {
        let mut cat = Catalog::new();
        cat.add_container("64$1", BROWSEDIR_ID, "video", "container.storageFolder", true);
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

    #[test]
    fn scan_skips_junk_sample_exclude_and_reads_nfo_captions() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-scan-{}",
            std::process::id()
        ));
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
        std::fs::write(tmp.join("video/not-video.mkv"), b"this is a text file not a video").unwrap();
        std::fs::write(tmp.join("video/notes.txt"), b"ignore").unwrap();

        let cat = scan(&ScanConfig {
            media_dirs: vec![tmp.clone()],
            exclude_dirs: vec!["exclude_me".into()],
            exclude_files: vec![],
            ..Default::default()
        });
        assert!(
            !cat.items.values().any(|i| i.title == "not-video" || i.title == "notes"),
            "text must not be indexed as video"
        );
        let titles: Vec<_> = cat.items.values().map(|i| i.title.as_str()).collect();
        assert!(titles.iter().any(|t| *t == "movie"));
        assert!(titles.iter().any(|t| *t == "dvp7"));
        assert!(!titles.iter().any(|t| *t == "skip" || *t == "secret" || *t == "junk"));
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-inode-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let a = tmp.join("video/orig.mkv");
        write_fake_mkv(&a, 64);
        let b = tmp.join("video/alias.mkv");
        let _ = std::fs::remove_file(&b);
        std::fs::hard_link(&a, &b).unwrap();
        let dbp = tmp.join("files.db");
        let cat = scan(&ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(dbp.clone()),
            ..Default::default()
        });
        let orig = cat.items.values().find(|i| i.title == "orig").unwrap();
        let alias = cat.items.values().find(|i| i.title == "alias").unwrap();
        let db = LibraryDb::open(&dbp).unwrap();
        let o = db.find_detail_by_path(&orig.path.to_string_lossy()).unwrap().unwrap();
        let arow = db.find_detail_by_path(&alias.path.to_string_lossy()).unwrap().unwrap();
        // rustyDLNA: one DETAILS row per path; same DEVICE+INODE.
        assert_ne!(o.0, arow.0);
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
    fn symlink_clones_known_detail_without_second_probe() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-clone-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("movies")).unwrap();
        std::fs::create_dir_all(tmp.join("genre")).unwrap();
        write_fake_mkv(&tmp.join("movies/film.mkv"), 64);
        std::fs::write(tmp.join("movies/film.nfo"), "<year>1999</year>").unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg);
        let orig = c1.items.values().find(|i| i.title == "film").unwrap();
        assert!(orig.date.contains("1999"), "nfo date on original {}", orig.date);
        std::os::unix::fs::symlink(
            tmp.join("movies/film.mkv"),
            tmp.join("genre/film-link.mkv"),
        )
        .unwrap();
        let (next, d) = rescan(&cfg, &c1);
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-sql-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let orig = tmp.join("video/orig.mkv");
        write_fake_mkv(&orig, 128);
        let link = tmp.join("video/link.mkv");
        std::os::unix::fs::symlink(&orig, &link).unwrap();
        let dbp = tmp.join("files.db");
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(dbp.clone()),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg);
        assert!(cat.items.values().any(|i| i.title == "orig"));
        assert!(cat.items.values().any(|i| i.title == "link"));
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
        assert!(!cat2.items.values().any(|i| i.title == "orig" || i.title == "link"));

        // rescan also prunes
        write_fake_mkv(&orig, 128);
        std::os::unix::fs::symlink(&orig, &tmp.join("video/link2.mkv")).ok();
        let _ = scan(&cfg);
        std::fs::remove_file(&orig).unwrap();
        let cat3 = scan(&cfg);
        assert!(
            !cat3.items.values().any(|i| i.path.ends_with("link2.mkv")),
            "rescan must drop dangling symlink aliases"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn removing_old_path_keeps_live_symlink_aliases() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-live-alias-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("old")).unwrap();
        std::fs::create_dir_all(tmp.join("action")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/action")).unwrap();
        write_fake_mkv(&tmp.join("old/film.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg);
        std::fs::rename(tmp.join("old/film.mkv"), tmp.join("action/film.mkv")).unwrap();
        std::os::unix::fs::symlink(
            tmp.join("action/film.mkv"),
            tmp.join("genres/action/film.mkv"),
        )
        .unwrap();
        let _ = monitor(&cfg);
        let n = forget_path(&cfg, &tmp.join("old/film.mkv"));
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-rescan-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        write_fake_mkv(&tmp.join("video/a.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg);
        write_fake_mkv(&tmp.join("video/b.mkv"), 64);
        let (c2, d) = rescan(&cfg, &c1);
        assert!(d.added >= 1);
        assert!(c2.items.values().any(|i| i.title == "b"));
        std::fs::remove_file(tmp.join("video/b.mkv")).unwrap();
        let (c3, d2) = rescan(&cfg, &c2);
        assert!(d2.removed >= 1);
        assert!(!c3.items.values().any(|i| i.title == "b"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn video_has_all_recent_and_folders() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-video-views-{}",
            std::process::id()
        ));
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
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        });
        let video = cat.children_of(VIDEO_ID).expect("video");
        let titles: Vec<_> = video
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Container(x) => Some(x.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["All Video", "Folders", "Recently Added"]);
        assert!(cat.containers.contains_key(VIDEO_DIR_ID));
        let folders = cat.children_of(VIDEO_DIR_ID).expect("folders");
        assert!(
            !folders.is_empty(),
            "Video/Folders must list the media tree"
        );
        let recent = cat.children_of(VIDEO_RECENT_ID).expect("recent");
        let recent_titles: Vec<_> = recent
            .iter()
            .filter_map(|c| match c {
                CatalogChild::Item(i) => Some(i.title.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            recent_titles.contains(&"fresh"),
            "recent={recent_titles:?}"
        );
        assert!(
            recent_titles.contains(&"stale"),
            "no time window: old files stay in recent: {recent_titles:?}"
        );
        assert_eq!(
            recent_titles.iter().filter(|t| **t == "fresh" || t.starts_with("fresh")).count(),
            1,
            "symlink alias must not duplicate recent: {recent_titles:?}"
        );
        assert!(
            !recent_titles.iter().any(|t| t.contains("alias")),
            "recent must not list symlink alias: {recent_titles:?}"
        );
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
    }

    #[test]
    fn monitor_skips_incomplete_and_only_applies_delta() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-monitor-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("movies")).unwrap();
        std::fs::create_dir_all(tmp.join("incomplete")).unwrap();
        write_fake_mkv(&tmp.join("movies/keep.mkv"), 64);
        write_fake_mkv(&tmp.join("incomplete/wip.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            exclude_dirs: vec!["incomplete".into()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg);
        assert!(c1.items.values().any(|i| i.title == "keep"));
        assert!(
            !c1.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("incomplete")),
            "incomplete must never be indexed"
        );
        let (none, d0) = monitor(&cfg);
        assert!(none.is_none(), "unchanged library must not rewrite");
        assert_eq!(d0, ScanDelta::default());
        write_fake_mkv(&tmp.join("movies/new.mkv"), 64);
        write_fake_mkv(&tmp.join("incomplete/another.mkv"), 64);
        let (some, d) = monitor(&cfg);
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-load-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        write_fake_mkv(&tmp.join("video/kept.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let scanned = scan(&cfg);
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
    fn rebase_media_path_maps_host_realpath_onto_media_root() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-rebase-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("storage/video");
        let rel = PathBuf::from("shows/clip.mkv");
        write_fake_mkv(&root.join(&rel), 64);
        let stored = tmp.join("storage/pool/video").join(&rel);
        assert!(!stored.exists(), "stored realpath must be absent");
        let got = rebase_media_path(&stored, &[root.clone()]);
        assert_eq!(got, root.join(&rel));
        assert!(path_is_live_file(&got));
        let live = root.join(&rel);
        assert_eq!(rebase_media_path(&live, &[root.clone()]), live);
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
    fn media_rel_key_equates_host_realpath_and_mount() {
        let root = PathBuf::from("/storage/video");
        assert_eq!(
            media_rel_key(Path::new("/storage/video/Show/ep.mkv"), &[root.clone()]),
            "Show/ep.mkv"
        );
        assert_eq!(
            media_rel_key(
                Path::new("/mnt/pool/video/Show/ep.mkv"),
                &[root.clone()]
            ),
            "Show/ep.mkv"
        );
        assert!(paths_are_same_media(
            "/mnt/pool/video/Show/ep.mkv",
            Path::new("/storage/video/Show/ep.mkv"),
            &[root.clone()]
        ));
        assert!(path_is_under_watched(
            "/mnt/pool/video/Show/S01/ep.mkv",
            Path::new("/storage/video/Show"),
            &[root]
        ));
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-dirlink-{}",
            std::process::id()
        ));
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
        std::fs::create_dir_all(root.join("genres/BY_YEAR/2010/Movies")).unwrap();
        std::os::unix::fs::symlink(
            root.join("kids/Movies/Despicable Me"),
            root.join("genres/BY_YEAR/2010/Movies/Despicable Me"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg);
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

        let rebuilt = rebuild_objects(&cfg);
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
            !db.folders_have_duplicate_inodes(),
            "no inode+title twice in one folder"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_drops_moved_file_and_empty_folder() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-move-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("old")).unwrap();
        std::fs::create_dir_all(tmp.join("new")).unwrap();
        write_fake_mkv(&tmp.join("old/episode.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg);
        assert!(c1.items.values().any(|i| i.path.ends_with("old/episode.mkv")));
        std::fs::rename(tmp.join("old/episode.mkv"), tmp.join("new/episode.mkv")).unwrap();
        let (c2, d) = rescan(&cfg, &c1);
        assert!(d.removed >= 1, "move must drop the source: {d:?}");
        assert!(d.added >= 1, "move must index the dest: {d:?}");
        assert!(
            !c2.items
                .values()
                .any(|i| i.path.to_string_lossy().contains("/old/")),
            "source path must leave the catalog"
        );
        assert!(c2.items.values().any(|i| i.path.ends_with("new/episode.mkv")));
        assert!(
            !c2.containers.values().any(|c| c.title == "old"),
            "empty source folder must be pruned"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn forget_path_matches_host_realpath_prefix() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-forget-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("mnt/video");
        std::fs::create_dir_all(root.join("Show")).unwrap();
        write_fake_mkv(&root.join("Show/ep.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg);
        let dbp = cfg.db_path.as_ref().unwrap();
        {
            let conn = rusqlite::Connection::open(dbp).unwrap();
            conn.execute(
                "UPDATE DETAILS SET PATH = ?1 WHERE PATH = ?2",
                rusqlite::params![
                    tmp.join("host/z2/video/Show/ep.mkv")
                        .to_string_lossy()
                        .as_ref(),
                    root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                ],
            )
            .unwrap();
        }
        let n = forget_path(&cfg, &root.join("Show/ep.mkv"));
        assert!(n >= 1, "inotify mount path must delete the realpath row");
        let db = LibraryDb::open(dbp).unwrap();
        let left = db
            .all_detail_stats()
            .unwrap()
            .into_iter()
            .filter(|(p, ..)| p.ends_with("ep.mkv"))
            .count();
        assert_eq!(left, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_does_not_rewrite_equivalent_realpath_prefix() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-equiv-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("mnt/video");
        std::fs::create_dir_all(root.join("Show")).unwrap();
        write_fake_mkv(&root.join("Show/ep.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![root.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg);
        {
            let conn = rusqlite::Connection::open(cfg.db_path.as_ref().unwrap()).unwrap();
            conn.execute(
                "UPDATE DETAILS SET PATH = ?1 WHERE PATH = ?2",
                rusqlite::params![
                    tmp.join("host/z2/video/Show/ep.mkv")
                        .to_string_lossy()
                        .as_ref(),
                    root.join("Show/ep.mkv").to_string_lossy().as_ref(),
                ],
            )
            .unwrap();
        }
        let (none, d) = monitor(&cfg);
        assert!(
            none.is_none(),
            "same file under host realpath must not be rewritten: {d:?}"
        );
        assert_eq!(d, ScanDelta::default());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn next_child_seq_skips_gaps_so_upsert_cannot_rename_a_folder() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-seq-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let dbp = tmp.join("files.db");
        let db = LibraryDb::open(&dbp).unwrap();
        db.seed_virtual_containers().unwrap();
        db.upsert_object("64$1", "64", "container.storageFolder", None, "video", None)
            .unwrap();
        db.upsert_object("64$1$1", "64$1", "container.storageFolder", None, "keep", None)
            .unwrap();
        db.upsert_object("64$1$2", "64$1", "container.storageFolder", None, "mid", None)
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
        assert!(db.object_exists("64$1$3"));
        let name = db.object_name("64$1$3").unwrap();
        assert_eq!(name, "sport");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn new_folder_after_sibling_delete_does_not_inherit_old_children() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-id-reuse-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("keep")).unwrap();
        std::fs::create_dir_all(tmp.join("mid")).unwrap();
        std::fs::create_dir_all(tmp.join("sport")).unwrap();
        write_fake_mkv(&tmp.join("keep/keep.mkv"), 64);
        write_fake_mkv(&tmp.join("mid/mid.mkv"), 64);
        write_fake_mkv(&tmp.join("sport/game.mkv"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg);
        let sport = c1
            .containers
            .values()
            .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
            .expect("sport folder")
            .clone();
        assert!(
            c1.items
                .values()
                .any(|i| i.parent_id == sport.object_id && i.title == "game")
        );

        std::fs::remove_file(tmp.join("keep/keep.mkv")).unwrap();
        let _ = std::fs::remove_dir(tmp.join("keep"));
        let (c2, _) = rescan(&cfg, &c1);
        let sport2 = c2
            .containers
            .values()
            .find(|c| c.title == "sport" && c.object_id.starts_with("64"))
            .expect("sport survives")
            .clone();
        assert_eq!(sport2.object_id, sport.object_id);

        std::fs::create_dir_all(
            tmp.join("Fallout.S01.Hybrid.2160p.Remux.DoVi.HDR10Plus.H"),
        )
        .unwrap();
        write_fake_mkv(
            &tmp.join("Fallout.S01.Hybrid.2160p.Remux.DoVi.HDR10Plus.H/ep.mkv"),
            64,
        );
        let (c3, d) = rescan(&cfg, &c2);
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
            fallout_titles.iter().any(|t| *t == "ep"),
            "Fallout children={fallout_titles:?}"
        );
        assert!(
            !fallout_titles.iter().any(|t| *t == "game"),
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-noop-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("action")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/crime")).unwrap();
        write_fake_mkv(&tmp.join("action/film.mkv"), 64);
        std::os::unix::fs::symlink(
            tmp.join("action/film.mkv"),
            tmp.join("genres/crime/film.mkv"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg);
        let (first, d1) = monitor(&cfg);
        let _ = first;
        let _ = d1;
        let (second, d2) = monitor(&cfg);
        assert!(
            second.is_none(),
            "unchanged library must not rewrite: {d2:?}"
        );
        assert_eq!(d2.added, 0, "must not count already-indexed files as adds");
        assert_eq!(d2.removed, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn symlink_dir_alias_does_not_steal_original_object() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-steal-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("action/Now You See Me")).unwrap();
        std::fs::create_dir_all(tmp.join("genres/action")).unwrap();
        write_fake_mkv(
            &tmp.join("action/Now You See Me/film.mkv"),
            64,
        );
        std::os::unix::fs::symlink(
            tmp.join("action/Now You See Me"),
            tmp.join("genres/action/Now You See Me"),
        )
        .unwrap();
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg);
        let orig_path = tmp.join("action/Now You See Me/film.mkv");
        let orig = c1
            .items
            .values()
            .find(|i| i.path == orig_path && i.ref_id.is_none())
            .expect("original")
            .clone();
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let alias = tmp.join("genres/action/Now You See Me/film.mkv");
        let folder = ensure_folder_chain(&db, &cfg, &alias).expect("genre folder chain");
        assert!(
            folder.contains('$'),
            "genre path must get its own folder id: {folder}"
        );
        assert_ne!(
            folder, orig.parent_id,
            "symlink-dir walk must not collapse onto the original folder"
        );
        assert!(index_one_file(&db, &cfg, &alias, &folder));
        let c2 = db.load_catalog().unwrap();
        let still = c2.items.get(&orig.object_id).expect("original object kept");
        assert_eq!(
            still.detail_id, orig.detail_id,
            "alias must not steal the original OBJECTS.DETAIL_ID"
        );
        assert_eq!(still.parent_id, orig.parent_id);
        assert!(
            c2.items.values().any(|i| {
                i.path.ends_with("genres/action/Now You See Me/film.mkv")
                    && i.parent_id == folder
            }),
            "alias should live under the genre folder"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn disc_structure_is_not_indexed() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-bdmv-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("movie/BDMV/STREAM")).unwrap();
        write_fake_mkv(&tmp.join("movie/title.mkv"), 64);
        write_fake_mkv(&tmp.join("movie/BDMV/STREAM/00001.m2ts"), 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let cat = scan(&cfg);
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-realias-{}",
            std::process::id()
        ));
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
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg);
        let n = forget_path(&cfg, &real);
        assert!(n >= 1, "real path row must drop; live alias stays: {n}");
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let after_forget = db.all_detail_stats().unwrap();
        assert!(
            after_forget
                .iter()
                .any(|(p, ..)| p.contains("genres/BY_YEAR")),
            "dir-symlink alias must survive deleting the real path: {after_forget:?}"
        );
        assert!(
            !after_forget
                .iter()
                .any(|(p, ..)| p.ends_with("kids/Movies/The Incredibles/02 - Incredibles 2.mkv")),
            "real path must be gone before monitor: {after_forget:?}"
        );
        drop(db);
        let (some, d) = monitor(&cfg);
        let _ = some;
        assert!(d.added >= 1, "monitor must reindex the live real path: {d:?}");
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let rows = db.all_detail_stats().unwrap();
        assert!(
            rows.iter()
                .any(|(p, ..)| p.ends_with("kids/Movies/The Incredibles/02 - Incredibles 2.mkv")),
            "real path back in DETAILS: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|(p, ..)| p.contains("genres/BY_YEAR") && p.ends_with("02 - Incredibles 2.mkv")),
            "alias path must stay: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn monitor_rename_updates_real_path_and_dir_symlink_alias() {
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-renalias-{}",
            std::process::id()
        ));
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
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let c1 = scan(&cfg);
        assert!(c1.items.values().any(|i| i.path.ends_with("action/Jason Bourne/old.mkv")));
        assert!(c1
            .items
            .values()
            .any(|i| i.path.to_string_lossy().contains("genres/BY_YEAR") && i.path.ends_with("old.mkv")));
        let new = tmp.join("action/Jason Bourne/02 - The Bourne Supremacy.mkv");
        std::fs::rename(&old, &new).unwrap();
        let (c2, d) = rescan(&cfg, &c1);
        assert!(d.removed >= 1, "old name must leave: {d:?}");
        assert!(d.added >= 1, "new name must be indexed: {d:?}");
        assert!(
            c2.items
                .values()
                .any(|i| i.path.ends_with("action/Jason Bourne/02 - The Bourne Supremacy.mkv")),
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
        let tmp = std::env::temp_dir().join(format!(
            "rusty-dlna-rebuild-miss-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("video")).unwrap();
        let keep = tmp.join("video/keep.mkv");
        let gone = tmp.join("video/gone.mkv");
        write_fake_mkv(&keep, 64);
        write_fake_mkv(&gone, 64);
        let cfg = ScanConfig {
            media_dirs: vec![tmp.clone()],
            db_path: Some(tmp.join("files.db")),
            types: MediaTypes::video_only(),
            ..Default::default()
        };
        let _ = scan(&cfg);
        std::fs::remove_file(&gone).unwrap();
        let _ = rebuild_objects(&cfg);
        let db = LibraryDb::open(cfg.db_path.as_ref().unwrap()).unwrap();
        let rows = db.all_detail_stats().unwrap();
        assert!(
            rows.iter().any(|(p, ..)| p.ends_with("keep.mkv")),
            "live file stays"
        );
        assert!(
            !rows.iter().any(|(p, ..)| p.ends_with("gone.mkv")),
            "missing file must leave DETAILS, not just OBJECTS: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
