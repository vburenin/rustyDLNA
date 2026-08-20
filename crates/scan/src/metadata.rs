//! Media identity, stream metadata, and request-facing item records.
#![warn(missing_docs)]

use super::*;

#[allow(missing_docs)]
#[derive(Clone, Debug, Default)]
pub struct SourceProbe {
    pub container: String,
    pub video: String,
    pub hdr: String,
    pub audio: String,
    /// Comma-separated `global-stream:audio-ordinal:codec:channels` records.
    /// Optional percent-encoded language, title, and default-disposition
    /// fields follow. Unlike `audio`, this preserves stream order and labels.
    pub audio_streams: String,
    pub video_profile: String,
    pub video_level: u32,
    pub pixel_format: String,
    pub bit_depth: u32,
    pub frame_rate: String,
    pub audio_layout: String,
    pub codec_string: String,
    pub width: u32,
    pub height: u32,
}

/// Parse the scanner's compact probe-sidecar representation.
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
            "video_profile" => p.video_profile = v.to_string(),
            "video_level" => p.video_level = v.parse().unwrap_or(0),
            "pixel_format" => p.pixel_format = v.to_string(),
            "bit_depth" => p.bit_depth = v.parse().unwrap_or(0),
            "frame_rate" => p.frame_rate = v.to_string(),
            "audio_layout" => p.audio_layout = v.to_string(),
            "codec_string" => p.codec_string = v.to_string(),
            "width" => p.width = v.parse().unwrap_or(0),
            "height" => p.height = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    p
}

#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub struct Caption {
    pub index: u32,
    pub path: PathBuf,
    pub ext: String,
}

#[allow(missing_docs)]
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

#[allow(missing_docs)]
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

#[allow(missing_docs)]
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
