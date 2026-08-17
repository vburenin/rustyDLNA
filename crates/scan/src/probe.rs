//! Stream identity via libavformat / libavcodec (`ffmpeg-next` / `ffmpeg-sys-next`).
//! No ffmpeg CLI. Bounded `probesize` / `analyzeduration`.

use std::ffi::CStr;
use std::path::Path;
use std::ptr;
use std::sync::Once;

use ffmpeg_sys_next as sys;

use crate::{duration_str, AvMeta, SourceProbe};

#[derive(Clone, Debug, Default)]
pub struct MediaProbe {
    pub probe: SourceProbe,
    pub av: AvMeta,
}

static INIT: Once = Once::new();

fn init_libav() {
    INIT.call_once(|| {
        unsafe {
            sys::av_log_set_level(sys::AV_LOG_ERROR);
        }
    });
}

/// Open the file with libavformat and fill DETAILS. Never shells out.
pub fn probe_media(path: &Path) -> Option<MediaProbe> {
    init_libav();
    let file = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let path_s = path.display().to_string();
    tracing::info!(
        target: "rusty_dlna",
        file,
        path = %path_s,
        "libav exploring"
    );
    let Ok(path_c) = std::ffi::CString::new(path.to_string_lossy().as_ref()) else {
        tracing::info!(
            target: "rusty_dlna",
            file,
            path = %path_s,
            "libav probe failed (path is not a C string)"
        );
        return None;
    };
    let got = unsafe { probe_avformat(path_c.as_ptr()) };
    match &got {
        Some(m) => tracing::info!(
            target: "rusty_dlna",
            file,
            path = %path_s,
            container = %m.probe.container,
            video = %m.probe.video,
            audio = %m.probe.audio,
            hdr = %m.probe.hdr,
            width = m.probe.width,
            height = m.probe.height,
            resolution = m.av.resolution.as_deref().unwrap_or(""),
            duration = m.av.duration.as_deref().unwrap_or(""),
            bitrate = m.av.bitrate.unwrap_or(0),
            channels = m.av.channels.unwrap_or(0),
            samplerate = m.av.samplerate.unwrap_or(0),
            subs = %m.av.subs.as_deref().unwrap_or(""),
            "libav probe"
        ),
        None => tracing::info!(
            target: "rusty_dlna",
            file,
            path = %path_s,
            "libav probe failed"
        ),
    }
    got
}

unsafe fn probe_avformat(url: *const libc::c_char) -> Option<MediaProbe> {
    let mut opts: *mut sys::AVDictionary = ptr::null_mut();
    let k1 = c"probesize";
    let v1 = c"50000000";
    let k2 = c"analyzeduration";
    let v2 = c"15000000";
    sys::av_dict_set(&mut opts, k1.as_ptr(), v1.as_ptr(), 0);
    sys::av_dict_set(&mut opts, k2.as_ptr(), v2.as_ptr(), 0);

    let mut ctx: *mut sys::AVFormatContext = ptr::null_mut();
    let err = sys::avformat_open_input(&mut ctx, url, ptr::null_mut(), &mut opts);
    sys::av_dict_free(&mut opts);
    if err < 0 || ctx.is_null() {
        return None;
    }
    let _ = sys::avformat_find_stream_info(ctx, ptr::null_mut());

    let mut out = MediaProbe {
        probe: SourceProbe {
            container: String::new(),
            video: String::new(),
            hdr: String::new(),
            audio: String::new(),
            width: 0,
            height: 0,
        },
        av: AvMeta::default(),
    };
    let fmt = (*ctx).iformat;
    if !fmt.is_null() {
        out.probe.container = map_format(c_str((*fmt).name));
    }

    let dur = (*ctx).duration;
    if dur > 0 && dur != sys::AV_NOPTS_VALUE {
        let msec = dur / 1000;
        if msec > 0 {
            out.av.duration = Some(duration_str(msec));
        }
    }
    if (*ctx).bit_rate > 8 {
        out.av.bitrate = Some((*ctx).bit_rate / 8);
    }

    let mut videos: Vec<String> = Vec::new();
    let mut audios: Vec<String> = Vec::new();
    let mut subs: Vec<String> = Vec::new();
    let nb = (*ctx).nb_streams as isize;
    for i in 0..nb {
        let st = *(*ctx).streams.add(i as usize);
        if st.is_null() {
            continue;
        }
        let par = (*st).codecpar;
        if par.is_null() {
            continue;
        }
        match (*par).codec_type {
            t if t == sys::AVMediaType::AVMEDIA_TYPE_VIDEO => {
                let name = map_video((*par).codec_id);
                if out.av.creator.is_none() && is_divx_tag((*par).codec_tag) {
                    out.av.creator = Some("DiVX".into());
                }
                push_unique(&mut videos, name);
                if out.probe.width == 0 {
                    let w = (*par).width as u32;
                    let h = (*par).height as u32;
                    if w > 0 && h > 0 {
                        out.probe.width = w;
                        out.probe.height = h;
                        out.av.resolution = Some(format!("{w}x{h}"));
                    }
                    if let Some(p) = dovi_profile(st, par) {
                        out.probe.hdr = match p {
                            7 => "dv-p7".into(),
                            8 => "dv-p8".into(),
                            5 => "dv-p5".into(),
                            _ => "dv".into(),
                        };
                    } else if (*par).color_trc
                        == sys::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
                    {
                        out.probe.hdr = "hdr10".into();
                    } else {
                        out.probe.hdr = "sdr".into();
                    }
                }
            }
            t if t == sys::AVMediaType::AVMEDIA_TYPE_AUDIO => {
                let name = map_audio((*par).codec_id);
                let first = audios.is_empty();
                push_unique(&mut audios, name);
                if first {
                    if (*par).ch_layout.nb_channels > 0 {
                        out.av.channels = Some((*par).ch_layout.nb_channels as i64);
                    }
                    if (*par).sample_rate > 0 {
                        out.av.samplerate = Some((*par).sample_rate as i64);
                    }
                }
            }
            t if t == sys::AVMediaType::AVMEDIA_TYPE_SUBTITLE => {
                push_unique(&mut subs, map_subtitle((*par).codec_id));
            }
            _ => {}
        }
    }
    out.probe.video = videos.join(",");
    out.probe.audio = audios.join(",");
    if !subs.is_empty() {
        out.av.subs = Some(subs.join(","));
    }

    sys::avformat_close_input(&mut ctx);

    if out.probe.container.is_empty()
        && out.probe.video.is_empty()
        && out.av.duration.is_none()
    {
        return None;
    }
    if out.probe.hdr.is_empty() {
        out.probe.hdr = "sdr".into();
    }
    Some(out)
}

unsafe fn dovi_profile(
    st: *mut sys::AVStream,
    par: *mut sys::AVCodecParameters,
) -> Option<u8> {
    let kind = sys::AVPacketSideDataType::AV_PKT_DATA_DOVI_CONF;
    let mut sz = 0usize;
    let from_st = sys::av_stream_get_side_data(st, kind, &mut sz);
    if !from_st.is_null() && sz >= 3 {
        return Some(*from_st.add(2));
    }
    if !(*par).extradata.is_null() && (*par).extradata_size > 8 {
        let extra = std::slice::from_raw_parts((*par).extradata, (*par).extradata_size as usize);
        if let Some(p) = dv_profile_in_bytes(extra) {
            return Some(p);
        }
    }
    None
}

fn dv_profile_in_bytes(buf: &[u8]) -> Option<u8> {
    for tag in [b"dvcC".as_slice(), b"dvvC".as_slice()] {
        let mut start = 0;
        while let Some(rel) = buf[start..]
            .windows(4)
            .position(|w| w == tag)
        {
            let after = start + rel + 4;
            let end = buf.len().min(after + 24);
            let slice = &buf[after..end];
            for off in 0..12.min(slice.len().saturating_sub(3)) {
                if slice[off] == 1 && slice[off + 1] == 0 {
                    let profile = slice[off + 2] >> 1;
                    if (1..=20).contains(&profile) {
                        return Some(profile);
                    }
                }
            }
            start = after;
        }
    }
    None
}

fn is_divx_tag(tag: u32) -> bool {
    // little-endian fourcc
    let b = tag.to_le_bytes();
    matches!(&b, b"DIVX" | b"DX50" | b"XVID" | b"divx" | b"dx50" | b"xvid")
}

fn map_format(name: &str) -> String {
    if name.starts_with("matroska") {
        return "mkv".into();
    }
    if name.contains("mp4") || name.contains("mov") || name.contains("ismv") {
        return "mp4".into();
    }
    if name.contains("avi") {
        return "avi".into();
    }
    if name.contains("mpegts") {
        return "mpeg-ts".into();
    }
    if name.contains("mpeg") || name.contains("vob") || name.contains("svcd") {
        return "mpeg".into();
    }
    if name.contains("flv") {
        return "flv".into();
    }
    if name.contains("asf") || name.contains("wmv") {
        return "asf".into();
    }
    "mkv".into()
}

fn push_unique(out: &mut Vec<String>, name: String) {
    if name.is_empty() {
        return;
    }
    if !out.iter().any(|s| s == &name) {
        out.push(name);
    }
}

fn map_video(id: sys::AVCodecID) -> String {
    use sys::AVCodecID::*;
    match id {
        AV_CODEC_ID_HEVC => "hevc".into(),
        AV_CODEC_ID_H264 => "h264".into(),
        AV_CODEC_ID_MPEG2VIDEO => "mpeg2".into(),
        AV_CODEC_ID_MPEG4
        | AV_CODEC_ID_MSMPEG4V1
        | AV_CODEC_ID_MSMPEG4V2
        | AV_CODEC_ID_MSMPEG4V3 => "mpeg4".into(),
        AV_CODEC_ID_VP9 => "vp9".into(),
        AV_CODEC_ID_AV1 => "av1".into(),
        _ => "other".into(),
    }
}

fn map_subtitle(id: sys::AVCodecID) -> String {
    let name = unsafe { c_str(sys::avcodec_get_name(id)) };
    if name.is_empty() {
        "other".into()
    } else {
        name.to_string()
    }
}

fn map_audio(id: sys::AVCodecID) -> String {
    use sys::AVCodecID::*;
    match id {
        AV_CODEC_ID_TRUEHD => "truehd".into(),
        AV_CODEC_ID_EAC3 => "eac3".into(),
        AV_CODEC_ID_AC3 => "ac3".into(),
        AV_CODEC_ID_AAC => "aac".into(),
        AV_CODEC_ID_DTS => "dts".into(),
        AV_CODEC_ID_FLAC => "flac".into(),
        AV_CODEC_ID_MP3 => "mp3".into(),
        _ => "other".into(),
    }
}

/// Stream index of the first `AV_DISPOSITION_ATTACHED_PIC`, if any.
pub fn attached_pic_stream(path: &Path) -> Option<i32> {
    init_libav();
    let path_c = std::ffi::CString::new(path.to_string_lossy().as_ref()).ok()?;
    unsafe {
        let mut ctx: *mut sys::AVFormatContext = ptr::null_mut();
        if sys::avformat_open_input(&mut ctx, path_c.as_ptr(), ptr::null_mut(), ptr::null_mut()) < 0
            || ctx.is_null()
        {
            return None;
        }
        let _ = sys::avformat_find_stream_info(ctx, ptr::null_mut());
        let nb = (*ctx).nb_streams as isize;
        let mut found = None;
        for i in 0..nb {
            let st = *(*ctx).streams.add(i as usize);
            if st.is_null() {
                continue;
            }
            if (*st).disposition & sys::AV_DISPOSITION_ATTACHED_PIC != 0 {
                found = Some(i as i32);
                break;
            }
        }
        sys::avformat_close_input(&mut ctx);
        found
    }
}

/// Decode attached cover art to JPEG. `dest` must end in `.jpg`.
pub fn extract_attached_pic(src: &Path, dest: &Path) -> bool {
    let Some(idx) = attached_pic_stream(src) else {
        return false;
    };
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
        ])
        .arg(src)
        .args(["-map", &format!("0:{idx}"), "-frames:v", "1", "-an"])
        .arg(dest)
        .status()
        .map(|s| s.success() && dest.is_file())
        .unwrap_or(false)
}

/// One-shot poster when there is no sidecar and no attached pic.
/// Scale `src` to at most `w`×`h` JPEG at `dest` (`/Resized/`).
pub fn scale_jpeg(src: &Path, dest: &Path, w: u32, h: u32) -> bool {
    if w == 0 || h == 0 {
        return false;
    }
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let vf = format!("scale={w}:{h}:force_original_aspect_ratio=decrease");
    std::process::Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(src)
        .args(["-frames:v", "1", "-vf", &vf, "-an"])
        .arg(dest)
        .status()
        .map(|s| s.success() && dest.is_file())
        .unwrap_or(false)
}

pub fn generate_video_thumb(src: &Path, dest: &Path) -> bool {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            "1",
            "-i",
        ])
        .arg(src)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=320:-2",
            "-an",
        ])
        .arg(dest)
        .status()
        .map(|s| s.success() && dest.is_file())
        .unwrap_or(false)
}

unsafe fn c_str(p: *const libc::c_char) -> &'static str {
    if p.is_null() {
        return "";
    }
    CStr::from_ptr(p).to_str().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write_fake_mkv;

    #[test]
    fn libav_reads_real_container() {
        let dir = std::env::temp_dir().join(format!("libav-probe-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("clip.mkv");
        write_fake_mkv(&p, 64);
        let got = probe_media(&p).expect("libav probe");
        assert!(
            !got.probe.video.is_empty(),
            "{got:?}"
        );
        assert!(got.av.duration.is_some() || got.av.resolution.is_some(), "{got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mpeg4_is_not_other() {
        assert_eq!(map_video(sys::AVCodecID::AV_CODEC_ID_MPEG4), "mpeg4");
        assert_eq!(map_video(sys::AVCodecID::AV_CODEC_ID_MSMPEG4V3), "mpeg4");
        assert_eq!(map_video(sys::AVCodecID::AV_CODEC_ID_HEVC), "hevc");
    }

    #[test]
    fn libav_reads_frozen_ii_when_present() {
        let p = Path::new(
            "/storage/video/kids/Movies/Frozen/02 - Frozen II (2019) - 2160p UHD BDRemux Hybrid DoVi.mkv",
        );
        if !p.is_file() {
            return;
        }
        let got = probe_media(p).expect("libav Frozen II");
        assert_eq!(got.probe.video, "hevc", "{got:?}");
        assert_eq!(got.probe.hdr, "dv-p8", "{got:?}");
    }
}
