//! Container sniffing, bounded stream admission, and shared MIME resolution.

use crate::{acquire_scan_helper, probe, MediaProbe, ScanConfig, ScanResult};
use rusty_dlna_protocol::{media_format_for_name, MediaKind, ResolvedMediaFormat};
use std::path::{Path, PathBuf};

/// Reject non-media before probing. Strong container magic
/// (EBML/ftyp/RIFF/…) is proof the file is a real bitstream, not text.
/// Ambiguous headers (TS/BDAV/MPEG/MP3) get a short `ffprobe`.
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

/// Bytes read for the header sniff. Large enough to see the sync byte of
/// the first three 192-byte BDAV/AVCHD transport packets (offsets 4, 196, 388).
const SNIFF_BYTES: u64 = 512;
const BDAV_PACKET_BYTES: usize = 192;

fn sniff_container(path: &Path) -> Sniff {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return Sniff::Reject;
    };
    let mut buf = Vec::with_capacity(SNIFF_BYTES as usize);
    if file.take(SNIFF_BYTES).read_to_end(&mut buf).is_err() {
        return Sniff::Reject;
    }
    classify_header(&buf)
}

/// Classify a file's first bytes. Strong container magic (EBML/ftyp/RIFF/…)
/// is proof the file is a real bitstream, not text. Ambiguous headers
/// (TS/BDAV/MPEG/MP3) are only plausible and get a short `ffprobe`.
fn classify_header(buf: &[u8]) -> Sniff {
    let n = buf.len();
    if n < 4 {
        return Sniff::Reject;
    }
    let strong = (buf[0] == 0x1a && buf[1] == 0x45 && buf[2] == 0xdf && buf[3] == 0xa3)
        // ISO BMFF (mp4/m4v/mov): size + "ftyp" / "mdat" / "moov"
        || (n >= 8 && matches!(&buf[4..8], b"ftyp" | b"mdat" | b"moov" | b"wide" | b"free"))
        // RIFF AVI / WAV
        || &buf[0..4] == b"RIFF"
        || &buf[0..3] == b"FLV"
        // ASF / WMV
        || (buf[0] == 0x30 && buf[1] == 0x26 && buf[2] == 0xb2 && buf[3] == 0x75)
        || &buf[0..4] == b"OggS"
        // JPEG
        || (buf[0] == 0xff && buf[1] == 0xd8 && buf[2] == 0xff)
        || &buf[0..4] == b"fLaC"
        // DSD Stream File (.dsf): "DSD " plus its fixed 28-byte (LE u64)
        // header-chunk size, so a text file starting "DSD " is not admitted.
        || (n >= 12 && &buf[0..4] == b"DSD " && buf[4..12] == 28u64.to_le_bytes())
        // DSDIFF (.dff): "FRM8", 8-byte size, form type "DSD "
        || (n >= 16 && &buf[0..4] == b"FRM8" && &buf[12..16] == b"DSD ")
        // RealMedia (.rm/.rmvb)
        || &buf[0..4] == b".RMF";
    if strong {
        return Sniff::Strong;
    }
    let weak =
        // MPEG-TS sync
        buf[0] == 0x47
        // BDAV/AVCHD transport stream (.m2ts/.mts): 4-byte TP_extra_header
        // before each 188-byte packet, so the sync byte repeats every 192.
        || bdav_sync_bytes_present(buf)
        // MPEG-PS / VOB pack
        || (buf[0] == 0x00 && buf[1] == 0x00 && buf[2] == 0x01)
        // ID3 / MP3 frame sync
        || &buf[0..3] == b"ID3"
        || (buf[0] == 0xff && (buf[1] & 0xe0) == 0xe0);
    if weak {
        Sniff::Weak
    } else {
        Sniff::Reject
    }
}

fn bdav_sync_bytes_present(buf: &[u8]) -> bool {
    let offsets = [4, 4 + BDAV_PACKET_BYTES, 4 + 2 * BDAV_PACKET_BYTES];
    // A clip shorter than three packets only needs the packets it holds.
    let mut checked = 0usize;
    for offset in offsets {
        match buf.get(offset) {
            Some(0x47) => checked += 1,
            Some(_) => return false,
            None => break,
        }
    }
    checked > 0 && (checked == offsets.len() || buf.len() < offsets[checked])
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
        .args(rusty_dlna_protocol::media_input::inherited_media_input_options(3))
        .args(["-i", "fd:"]);
    let file = std::fs::File::open(path).ok()?;
    let out = crate::probe::command_output_supervised_for_file(
        &mut command,
        &file,
        timeout,
        &crate::CancellationToken::default(),
    )
    .ok()?;
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
    ]);
    command
        .args(rusty_dlna_protocol::media_input::inherited_media_input_options(3))
        .args(["-i", "fd:"]);
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
    sniff_container(path) != Sniff::Reject
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bdav_header(len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        for packet in buf.chunks_mut(BDAV_PACKET_BYTES) {
            let header = [0x0e, 0xbf, 0x46, 0x22, 0x47];
            let n = header.len().min(packet.len());
            packet[..n].copy_from_slice(&header[..n]);
        }
        buf
    }

    #[test]
    fn header_classifier_recognizes_bdav_dsd_and_realmedia() {
        assert_eq!(classify_header(&bdav_header(512)), Sniff::Weak);
        assert_eq!(classify_header(&bdav_header(300)), Sniff::Weak);
        let mut broken = bdav_header(512);
        broken[196] = 0;
        assert_eq!(classify_header(&broken), Sniff::Reject);
        let mut broken = bdav_header(512);
        broken[388] = 0;
        assert_eq!(classify_header(&broken), Sniff::Reject);

        assert_eq!(classify_header(b"DSD \x1c\0\0\0\0\0\0\0"), Sniff::Strong);
        assert_eq!(classify_header(b"DSD notes about a track"), Sniff::Reject);
        assert_eq!(classify_header(b"DSD \x1c\0\0"), Sniff::Reject);
        assert_eq!(
            classify_header(b"FRM8\0\0\0\0\0\0\x01\0DSD "),
            Sniff::Strong
        );
        assert_eq!(
            classify_header(b"FRM8\0\0\0\0\0\0\x01\0AIFF"),
            Sniff::Reject
        );
        assert_eq!(classify_header(b".RMF\0\0\0\x12"), Sniff::Strong);
    }

    #[test]
    fn header_classifier_keeps_existing_strong_weak_and_reject_rules() {
        assert_eq!(classify_header(&[0x1a, 0x45, 0xdf, 0xa3]), Sniff::Strong);
        assert_eq!(classify_header(b"\0\0\0\x20ftypisom"), Sniff::Strong);
        assert_eq!(classify_header(b"fLaC\0\0\0\x22"), Sniff::Strong);
        assert_eq!(classify_header(&[0x47, 0x40, 0x11, 0x10]), Sniff::Weak);
        assert_eq!(classify_header(&[0x00, 0x00, 0x01, 0xba]), Sniff::Weak);
        assert_eq!(classify_header(b"ID3\x04"), Sniff::Weak);
        assert_eq!(classify_header(b"readme pretending"), Sniff::Reject);
        assert_eq!(classify_header(b"DSD"), Sniff::Reject);
    }
}
