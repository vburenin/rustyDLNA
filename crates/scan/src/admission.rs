//! Container sniffing, bounded stream admission, and shared MIME resolution.

use crate::{acquire_scan_helper, probe, MediaProbe, ScanConfig, ScanResult};
use rusty_dlna_protocol::{media_format_for_name, MediaKind, ResolvedMediaFormat};
use std::path::{Path, PathBuf};

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

pub(super) fn file_is_viable_opened(file: &std::fs::File, cfg: &ScanConfig) -> ScanResult<bool> {
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

pub(super) fn resolved_media_format(
    name: &str,
    probe: Option<&MediaProbe>,
) -> Option<ResolvedMediaFormat> {
    resolved_media_format_with_hint(name, probe, None)
}

pub(super) fn resolved_media_format_with_hint(
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

pub(super) fn mime_and_class(name: &str) -> (&'static str, &'static str, &'static str) {
    resolved_media_format(name, None)
        .map(|format| (format.mime, format.upnp_class(), format.extension))
        .unwrap_or(("application/octet-stream", "item.videoItem", "bin"))
}
