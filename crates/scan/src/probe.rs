//! Stream identity via libavformat / libavcodec (`ffmpeg-next` / `ffmpeg-sys-next`).
//! No ffmpeg CLI. Bounded `probesize` / `analyzeduration`.

use std::ffi::CStr;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::ptr;

extern "C" {
    fn rusty_dlna_codec_side_data(
        parameters: *mut sys::AVCodecParameters,
        kind: sys::AVPacketSideDataType,
        size: *mut usize,
    ) -> *const u8;
}
use std::sync::Once;
use std::time::{Duration, Instant};

fn path_cstring(path: &Path) -> Option<std::ffi::CString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
    }
    #[cfg(not(unix))]
    {
        std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()
    }
}

use ffmpeg_sys_next as sys;
use rusty_dlna_protocol::{
    encode_compact_stream_metadata, CompactAudioRecordInput, CompactChapterInput,
    CompactStreamMetadataInput, CompactStreamMetadataWriteError, CompactVideoCapabilitiesInput,
    MAX_COMPACT_AUDIO_RECORDS, MAX_COMPACT_CHAPTERS,
};

use crate::{duration_str, AvMeta, CancellationToken, EmbeddedTags, SourceProbe};

/// Maximum accepted byte length of one embedded container/stream tag.
/// Oversized values are omitted before allocating an owned Rust string.
pub const MAX_EMBEDDED_TAG_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct MediaHelperControl<'a> {
    pub timeout: Duration,
    pub max_alloc_bytes: u64,
    pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, Default)]
pub struct MediaProbe {
    pub probe: SourceProbe,
    pub av: AvMeta,
    pub tags: EmbeddedTags,
    /// Ordered audio streams with request-facing labels. The compact catalog
    /// stream descriptor persists these labels for ordinary playback.
    pub audio_tracks: Vec<AudioTrackProbe>,
    pub chapters: Vec<ChapterProbe>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChapterProbe {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub title: Option<String>,
}

/// One audio stream discovered by libavformat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioTrackProbe {
    /// Zero-based audio-stream ordinal used by ffmpeg's `0:a:N` selector.
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

static INIT: Once = Once::new();

fn init_libav() {
    // SAFETY: this process-wide libav logging setter accepts an enum value and
    // `Once` guarantees the global mutation is performed only once.
    INIT.call_once(|| unsafe {
        #[cfg(test)]
        let level = sys::AV_LOG_QUIET;
        #[cfg(not(test))]
        // libav labels recoverable bitstream defects (for example truncated
        // H.264 user-data SEI or an invalid VUI time base) as AV_LOG_ERROR
        // even when avformat can still identify the file successfully.  The
        // scanner reports an actual failed probe itself, so keep only fatal
        // process/library diagnostics on stderr here.
        let level = sys::AV_LOG_FATAL;
        sys::av_log_set_level(level);
    });
}

unsafe fn dictionary_value(dictionary: *mut sys::AVDictionary, keys: &[&str]) -> Option<String> {
    if dictionary.is_null() {
        return None;
    }
    for key in keys {
        let Ok(key) = std::ffi::CString::new(*key) else {
            continue;
        };
        let entry = sys::av_dict_get(dictionary, key.as_ptr(), ptr::null(), 0);
        if entry.is_null() || (*entry).value.is_null() {
            continue;
        }
        let raw = CStr::from_ptr((*entry).value).to_bytes();
        if raw.len() > MAX_EMBEDDED_TAG_BYTES {
            continue;
        }
        let value = String::from_utf8_lossy(raw);
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn tag_number(value: Option<String>) -> Option<i64> {
    value?.split('/').next()?.trim().parse().ok()
}

unsafe fn fill_tags_from_dictionary(tags: &mut EmbeddedTags, dictionary: *mut sys::AVDictionary) {
    macro_rules! fill {
        ($field:ident, $keys:expr) => {
            if tags.$field.is_none() {
                tags.$field = dictionary_value(dictionary, $keys);
            }
        };
    }
    fill!(title, &["title"]);
    fill!(artist, &["artist"]);
    fill!(
        album_artist,
        &["album_artist", "albumartist", "album artist"]
    );
    fill!(album, &["album"]);
    fill!(genre, &["genre"]);
    fill!(composer, &["composer"]);
    fill!(contributor, &["performer", "contributor"]);
    fill!(date, &["date", "year", "creation_time"]);
    fill!(comment, &["comment", "description"]);
    fill!(camera_make, &["make", "camera_make"]);
    fill!(camera_model, &["model", "camera_model"]);
    if tags.disc.is_none() {
        tags.disc = tag_number(dictionary_value(dictionary, &["disc", "discnumber"]));
    }
    if tags.track.is_none() {
        tags.track = tag_number(dictionary_value(dictionary, &["track", "tracknumber"]));
    }
    if tags.rating.is_none() {
        tags.rating = tag_number(dictionary_value(dictionary, &["rating", "rate"]));
    }
    if tags.rotation.is_none() {
        tags.rotation = tag_number(dictionary_value(dictionary, &["rotate", "rotation"]));
    }
}

fn fill_missing_tags(tags: &mut EmbeddedTags, fallback: EmbeddedTags) {
    macro_rules! fill {
        ($field:ident) => {
            if tags.$field.is_none() {
                tags.$field = fallback.$field;
            }
        };
    }
    fill!(title);
    fill!(artist);
    fill!(album_artist);
    fill!(album);
    fill!(genre);
    fill!(composer);
    fill!(contributor);
    fill!(date);
    fill!(comment);
    fill!(disc);
    fill!(track);
    fill!(rating);
    fill!(camera_make);
    fill!(camera_model);
    fill!(rotation);
}

/// Open the file with libavformat and fill DETAILS. Never shells out.
pub fn probe_media(path: &Path) -> Option<MediaProbe> {
    probe_media_with_timeout(path, Duration::from_secs(30))
}

pub fn probe_media_with_timeout(path: &Path, timeout: Duration) -> Option<MediaProbe> {
    probe_media_with_cancellation(path, timeout, &CancellationToken::default())
}

pub fn probe_media_with_cancellation(
    path: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Option<MediaProbe> {
    init_libav();
    let file = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let path_s = path.display().to_string();
    tracing::debug!(
        target: "rusty_dlna",
        file,
        path = %path_s,
        "libav exploring"
    );
    let Some(path_c) = path_cstring(path) else {
        tracing::warn!(
            target: "rusty_dlna",
            file,
            path = %path_s,
            "libav probe failed (path is not a C string)"
        );
        return None;
    };
    // SAFETY: `path_c` is NUL-terminated for the entire call. The helper owns
    // and closes every libav allocation it creates before returning.
    let got = unsafe { probe_avformat(path_c.as_ptr(), timeout, cancellation) };
    match &got {
        Some(m) => tracing::debug!(
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
        None => tracing::warn!(
            target: "rusty_dlna",
            file,
            path = %path_s,
            "libav probe failed"
        ),
    }
    got
}

/// Probe a still image without exposing libav's image decoder as a video
/// stream identity. Only dimensions are useful to ContentDirectory.
pub fn probe_image(path: &Path) -> Option<MediaProbe> {
    probe_image_with_timeout(path, Duration::from_secs(30))
}

pub fn probe_image_with_timeout(path: &Path, timeout: Duration) -> Option<MediaProbe> {
    probe_image_with_cancellation(path, timeout, &CancellationToken::default())
}

pub fn probe_image_with_cancellation(
    path: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Option<MediaProbe> {
    let mut image = probe_media_with_cancellation(path, timeout, cancellation)?;
    image.probe.container = "jpeg".into();
    image.probe.video.clear();
    image.probe.audio.clear();
    image.probe.hdr.clear();
    image.av.duration = None;
    image.av.bitrate = None;
    image.av.channels = None;
    image.av.samplerate = None;
    apply_exif(path, &mut image);
    Some(image)
}

fn exif_ascii(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let field = exif.get_field(tag, exif::In::PRIMARY)?;
    let exif::Value::Ascii(values) = &field.value else {
        return None;
    };
    values.iter().find_map(|value| {
        let value = value.strip_suffix(&[0]).unwrap_or(value);
        let value = String::from_utf8_lossy(value).trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn exif_date(value: String) -> Option<String> {
    // EXIF timestamps are ASCII. Validate every byte before slicing: lossy
    // decoding of malformed tag bytes may have introduced multibyte U+FFFD.
    let bytes = value.as_bytes();
    // Retain the valid date-only forms accepted by the metadata normalizer.
    if (bytes.len() == 4 && bytes.iter().all(u8::is_ascii_digit))
        || (bytes.len() == 10
            && bytes.iter().enumerate().all(|(index, byte)| {
                if matches!(index, 4 | 7) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_digit()
                }
            }))
    {
        return Some(rusty_dlna_protocol::w3c_normalize_date(&value));
    }
    let timestamp = bytes.get(..19)?;
    let exif = timestamp[4] == b':' && timestamp[7] == b':';
    for (index, byte) in timestamp.iter().enumerate() {
        let valid = match index {
            4 | 7 => *byte == if exif { b':' } else { b'-' },
            13 | 16 => *byte == b':',
            10 => *byte == b' ' || (!exif && *byte == b'T'),
            _ => byte.is_ascii_digit(),
        };
        if !valid {
            return None;
        }
    }
    if exif {
        Some(format!(
            "{}-{}-{}T{}Z",
            &value[0..4],
            &value[5..7],
            &value[8..10],
            &value[11..19]
        ))
    } else if bytes.len() == 19
        || (bytes.len() == 20 && bytes[19] == b'Z')
        || (bytes.len() == 25
            && matches!(bytes[19], b'+' | b'-')
            && bytes[22] == b':'
            && [20, 21, 23, 24]
                .iter()
                .all(|index| bytes[*index].is_ascii_digit()))
    {
        Some(rusty_dlna_protocol::w3c_normalize_date(&value))
    } else {
        None
    }
}

fn apply_exif(path: &Path, image: &mut MediaProbe) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(file)) else {
        return;
    };
    image.tags.camera_make = exif_ascii(&exif, exif::Tag::Make);
    image.tags.camera_model = exif_ascii(&exif, exif::Tag::Model);
    image.tags.album = exif_ascii(&exif, exif::Tag(exif::Context::Tiff, 0x010d));
    image.tags.rating = exif
        .get_field(exif::Tag(exif::Context::Tiff, 0x4746), exif::In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .map(i64::from);
    image.tags.comment = exif_ascii(&exif, exif::Tag::ImageDescription)
        .or_else(|| exif_ascii(&exif, exif::Tag::UserComment));
    image.tags.title = image.tags.comment.clone();
    image.tags.date = exif_ascii(&exif, exif::Tag::DateTimeOriginal)
        .or_else(|| exif_ascii(&exif, exif::Tag::DateTime))
        .and_then(exif_date);
    let orientation = exif
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|field| field.value.get_uint(0));
    image.tags.rotation = match orientation {
        Some(3) => Some(180),
        Some(6) => Some(90),
        Some(8) => Some(270),
        _ => Some(0),
    };
    let width = exif
        .get_field(exif::Tag::PixelXDimension, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::ImageWidth, exif::In::PRIMARY))
        .and_then(|field| field.value.get_uint(0))
        .unwrap_or(image.probe.width);
    let height = exif
        .get_field(exif::Tag::PixelYDimension, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::ImageLength, exif::In::PRIMARY))
        .and_then(|field| field.value.get_uint(0))
        .unwrap_or(image.probe.height);
    let (display_width, display_height) = if matches!(image.tags.rotation, Some(90 | 270)) {
        (height, width)
    } else {
        (width, height)
    };
    if display_width > 0 && display_height > 0 {
        image.probe.width = display_width;
        image.probe.height = display_height;
        image.av.resolution = Some(format!("{display_width}x{display_height}"));
    }
}

struct ProbeDeadline {
    expires: Instant,
    cancellation: CancellationToken,
}

fn checked_helper_deadline(timeout: Duration) -> Option<Instant> {
    Instant::now().checked_add(timeout.max(Duration::from_secs(1)))
}

unsafe extern "C" fn interrupt_expired(opaque: *mut libc::c_void) -> libc::c_int {
    if opaque.is_null() {
        return 0;
    }
    let deadline = &*(opaque.cast::<ProbeDeadline>());
    i32::from(deadline.cancellation.is_cancelled() || Instant::now() >= deadline.expires)
}

unsafe fn probe_avformat(
    url: *const libc::c_char,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Option<MediaProbe> {
    let expires = checked_helper_deadline(timeout)?;
    let mut opts: *mut sys::AVDictionary = ptr::null_mut();
    let k1 = c"probesize";
    let v1 = c"50000000";
    let k2 = c"analyzeduration";
    let v2 = c"15000000";
    sys::av_dict_set(&mut opts, k1.as_ptr(), v1.as_ptr(), 0);
    sys::av_dict_set(&mut opts, k2.as_ptr(), v2.as_ptr(), 0);

    let mut deadline = Box::new(ProbeDeadline {
        expires,
        cancellation: cancellation.clone(),
    });
    let mut ctx = sys::avformat_alloc_context();
    if ctx.is_null() {
        sys::av_dict_free(&mut opts);
        return None;
    }
    (*ctx).interrupt_callback = sys::AVIOInterruptCB {
        callback: Some(interrupt_expired),
        opaque: (&mut *deadline as *mut ProbeDeadline).cast(),
    };
    let err = sys::avformat_open_input(&mut ctx, url, ptr::null_mut(), &mut opts);
    sys::av_dict_free(&mut opts);
    if err < 0 || ctx.is_null() {
        if !ctx.is_null() {
            sys::avformat_close_input(&mut ctx);
        }
        return None;
    }
    let _ = sys::avformat_find_stream_info(ctx, ptr::null_mut());

    let mut out = MediaProbe {
        probe: SourceProbe {
            container: String::new(),
            video: String::new(),
            hdr: String::new(),
            audio: String::new(),
            audio_streams: String::new(),
            video_profile: String::new(),
            video_level: 0,
            pixel_format: String::new(),
            bit_depth: 0,
            frame_rate: String::new(),
            video_timestamp_mode: String::new(),
            audio_layout: String::new(),
            codec_string: String::new(),
            width: 0,
            height: 0,
        },
        av: AvMeta::default(),
        tags: EmbeddedTags::default(),
        audio_tracks: Vec::new(),
        chapters: Vec::new(),
    };
    let fmt = (*ctx).iformat;
    if !fmt.is_null() {
        out.probe.container = map_format(c_str((*fmt).name));
    }
    fill_tags_from_dictionary(&mut out.tags, (*ctx).metadata);

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
    let mut primary_video_index = None;
    let mut primary_video_delay = 0;
    let mut audios: Vec<String> = Vec::new();
    let mut audio_stream_indices: Vec<usize> = Vec::new();
    let mut audio_stream_tags = EmbeddedTags::default();
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
            sys::AVMediaType::AVMEDIA_TYPE_VIDEO => {
                let name = map_video((*par).codec_id);
                if out.av.creator.is_none() && is_divx_tag((*par).codec_tag) {
                    out.av.creator = Some("DiVX".into());
                }
                push_unique(&mut videos, name);
                if out.probe.width == 0 {
                    primary_video_index = Some(i as i32);
                    primary_video_delay = (*par).video_delay.max(0);
                    let w = (*par).width as u32;
                    let h = (*par).height as u32;
                    if w > 0 && h > 0 {
                        out.probe.width = w;
                        out.probe.height = h;
                        out.av.resolution = Some(format!("{w}x{h}"));
                    }
                    if let Some(p) = dovi_profile(par) {
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
                    out.probe.video_profile =
                        c_str(sys::avcodec_profile_name((*par).codec_id, (*par).profile))
                            .to_owned();
                    out.probe.video_level = (*par).level.max(0) as u32;
                    if (*par).format >= 0 {
                        let pixel_format =
                            std::mem::transmute::<i32, sys::AVPixelFormat>((*par).format);
                        out.probe.pixel_format =
                            c_str(sys::av_get_pix_fmt_name(pixel_format)).to_owned();
                        let parameter_depth =
                            [(*par).bits_per_raw_sample, (*par).bits_per_coded_sample]
                                .into_iter()
                                .filter(|depth| (1..=16).contains(depth))
                                .max()
                                .unwrap_or(0) as u32;
                        // Some demuxers report total packed pixel width (for
                        // example 24 for 8-bit YUV) as bits_per_coded_sample.
                        // Browser capability checks need per-component depth.
                        out.probe.bit_depth =
                            pixel_format_bit_depth(&out.probe.pixel_format).max(parameter_depth);
                    }
                    let rate = (*st).avg_frame_rate;
                    if rate.num > 0 && rate.den > 0 {
                        out.probe.frame_rate = format!("{}/{}", rate.num, rate.den);
                    }
                    if (*par).codec_id == sys::AVCodecID::AV_CODEC_ID_H264 {
                        out.probe.codec_string = h264_rfc6381(par).unwrap_or_default();
                    } else if (*par).codec_id == sys::AVCodecID::AV_CODEC_ID_HEVC {
                        out.probe.codec_string = hevc_rfc6381(par).unwrap_or_default();
                    }
                }
            }
            sys::AVMediaType::AVMEDIA_TYPE_AUDIO => {
                // Track titles such as "MVO", "English", or "Commentary"
                // describe this audio stream, not a containing movie. Keep
                // them as item tags only for an audio-only media file.
                fill_tags_from_dictionary(&mut audio_stream_tags, (*st).metadata);
                let name = map_audio((*par).codec_id);
                let first = out.audio_tracks.is_empty();
                let audio_ordinal = out.audio_tracks.len();
                let language = dictionary_value((*st).metadata, &["language"]);
                let title = dictionary_value((*st).metadata, &["title"]);
                let default = (*st).disposition & sys::AV_DISPOSITION_DEFAULT != 0;
                out.audio_tracks.push(AudioTrackProbe {
                    index: audio_ordinal,
                    codec: name.to_owned(),
                    channels: (*par).ch_layout.nb_channels.max(0) as u32,
                    language: language.clone(),
                    title: title.clone(),
                    default,
                });
                audio_stream_indices.push(i as usize);
                push_unique(&mut audios, name.clone());
                if first {
                    out.probe.audio_layout =
                        channel_layout_label((*par).ch_layout.nb_channels.max(0) as u32).into();
                    if name == "aac" {
                        if !out.probe.codec_string.is_empty() {
                            out.probe.codec_string.push(',');
                        }
                        out.probe.codec_string.push_str("mp4a.40.2");
                    }
                    if (*par).ch_layout.nb_channels > 0 {
                        out.av.channels = Some((*par).ch_layout.nb_channels as i64);
                    }
                    if (*par).sample_rate > 0 {
                        out.av.samplerate = Some((*par).sample_rate as i64);
                    }
                }
            }
            sys::AVMediaType::AVMEDIA_TYPE_SUBTITLE => {
                push_unique(&mut subs, map_subtitle((*par).codec_id));
            }
            _ => {}
        }
    }
    out.probe.video = videos.join(",");
    out.probe.audio = audios.join(",");
    out.probe.video_timestamp_mode = primary_video_index.map_or_else(
        || "valid".to_owned(),
        |stream_index| classify_video_timestamps(ctx, stream_index, primary_video_delay).to_owned(),
    );
    if videos.is_empty() {
        fill_missing_tags(&mut out.tags, audio_stream_tags);
    }
    if !subs.is_empty() {
        out.av.subs = Some(subs.join(","));
    }

    for index in 0..(*ctx).nb_chapters as usize {
        let chapter = *(*ctx).chapters.add(index);
        if chapter.is_null() || (*chapter).time_base.den <= 0 {
            continue;
        }
        let scale = (*chapter).time_base.num as f64 / (*chapter).time_base.den as f64;
        let start_seconds = (*chapter).start as f64 * scale;
        let end_seconds = (*chapter).end as f64 * scale;
        if start_seconds.is_finite() && start_seconds >= 0.0 {
            out.chapters.push(ChapterProbe {
                start_seconds,
                end_seconds: end_seconds.max(start_seconds),
                title: dictionary_value((*chapter).metadata, &["title"]),
            });
        }
    }
    out.probe.audio_streams = persisted_stream_metadata(
        &out.probe,
        &out.audio_tracks,
        &audio_stream_indices,
        &out.chapters,
    );

    sys::avformat_close_input(&mut ctx);

    if out.probe.container.is_empty() && out.probe.video.is_empty() && out.av.duration.is_none() {
        return None;
    }
    if out.probe.hdr.is_empty() {
        out.probe.hdr = "sdr".into();
    }
    Some(out)
}

/// Detect muxes which contain reordered/B frames but assign decode timestamps
/// as presentation timestamps. Such files decode successfully, yet present in
/// a forward/forward/back cadence when their video packets are stream-copied.
unsafe fn classify_video_timestamps(
    ctx: *mut sys::AVFormatContext,
    stream_index: i32,
    video_delay: i32,
) -> &'static str {
    if ctx.is_null() || video_delay <= 0 {
        return "valid";
    }
    let mut packet = sys::av_packet_alloc();
    if packet.is_null() {
        return "valid";
    }
    let mut matching_timestamps = 0usize;
    let mut differing_timestamps = 0usize;
    let mut packets_read = 0usize;
    while packets_read < 512 && matching_timestamps + differing_timestamps < 64 {
        if sys::av_read_frame(ctx, packet) < 0 {
            break;
        }
        packets_read += 1;
        if (*packet).stream_index == stream_index
            && (*packet).pts != sys::AV_NOPTS_VALUE
            && (*packet).dts != sys::AV_NOPTS_VALUE
        {
            if (*packet).pts == (*packet).dts {
                matching_timestamps += 1;
            } else {
                differing_timestamps += 1;
            }
        }
        sys::av_packet_unref(packet);
    }
    sys::av_packet_free(&mut packet);
    if matching_timestamps >= 12 && differing_timestamps == 0 {
        "broken-reordered"
    } else {
        "valid"
    }
}

fn persisted_stream_metadata(
    probe: &SourceProbe,
    audio_tracks: &[AudioTrackProbe],
    audio_stream_indices: &[usize],
    chapters: &[ChapterProbe],
) -> String {
    if let Ok(encoded) =
        encode_probe_stream_metadata(probe, audio_tracks, audio_stream_indices, chapters, true)
    {
        return encoded;
    }

    // Labels and chapter titles originate in untrusted container metadata and
    // can consume the complete persisted budget. Retry without those optional
    // fields and cap audio descriptors at the shared reader limit. Capability
    // and timestamp records remain byte-for-byte intact.
    let audio_limit = audio_tracks
        .len()
        .min(audio_stream_indices.len())
        .min(MAX_COMPACT_AUDIO_RECORDS);
    if let Ok(encoded) = encode_probe_stream_metadata(
        probe,
        &audio_tracks[..audio_limit],
        &audio_stream_indices[..audio_limit],
        &[],
        false,
    ) {
        return encoded;
    }

    // Generated capability values are short libav names and the timestamp
    // mode is one of two scanner literals, so the essential-only form is
    // expected to fit even if every audio descriptor had to be discarded.
    if let Ok(encoded) = encode_probe_stream_metadata(probe, &[], &[], &[], false) {
        return encoded;
    }

    // Retain the safety-sensitive timestamp classification if an unexpected
    // future capability source violates the invariant above. Empty @v fields
    // make consumers choose conservative capability defaults.
    let timestamp_mode = if probe.video_timestamp_mode == "broken-reordered" {
        "broken-reordered"
    } else {
        "valid"
    };
    format!("@v::0::0:::,@t:{timestamp_mode}")
}

fn encode_probe_stream_metadata(
    probe: &SourceProbe,
    audio_tracks: &[AudioTrackProbe],
    audio_stream_indices: &[usize],
    chapters: &[ChapterProbe],
    include_optional_audio_fields: bool,
) -> Result<String, CompactStreamMetadataWriteError> {
    let audio_record_count = audio_tracks.len().min(audio_stream_indices.len());
    if audio_record_count > MAX_COMPACT_AUDIO_RECORDS {
        return Err(CompactStreamMetadataWriteError::TooManyAudioRecords {
            count: audio_record_count,
            max: MAX_COMPACT_AUDIO_RECORDS,
        });
    }
    let audio_records = audio_tracks
        .iter()
        .zip(audio_stream_indices)
        .map(|(track, global_index)| CompactAudioRecordInput {
            global_index: *global_index,
            audio_index: track.index,
            codec: &track.codec,
            channels: track.channels,
            language: if include_optional_audio_fields {
                track.language.as_deref()
            } else {
                None
            },
            title: if include_optional_audio_fields {
                track.title.as_deref()
            } else {
                None
            },
            default: track.default,
        })
        .collect::<Vec<_>>();
    let chapters = chapters
        .iter()
        .take(MAX_COMPACT_CHAPTERS)
        .map(|chapter| CompactChapterInput {
            start_seconds: chapter.start_seconds,
            end_seconds: chapter.end_seconds,
            title: chapter.title.as_deref(),
        })
        .collect::<Vec<_>>();
    encode_compact_stream_metadata(CompactStreamMetadataInput {
        audio_records: &audio_records,
        video: CompactVideoCapabilitiesInput {
            profile: &probe.video_profile,
            level: probe.video_level,
            pixel_format: &probe.pixel_format,
            bit_depth: probe.bit_depth,
            frame_rate: &probe.frame_rate,
            codec_string: &probe.codec_string,
            audio_layout: &probe.audio_layout,
        },
        timestamp_mode: &probe.video_timestamp_mode,
        chapters: &chapters,
    })
}

fn pixel_format_bit_depth(pixel_format: &str) -> u32 {
    if pixel_format.contains("12") {
        12
    } else if pixel_format.contains("10") || pixel_format.starts_with("p010") {
        10
    } else if pixel_format.is_empty() {
        0
    } else {
        8
    }
}

fn channel_layout_label(channels: u32) -> &'static str {
    match channels {
        1 => "mono",
        2 => "stereo",
        6 => "5.1",
        8 => "7.1",
        _ => "unknown",
    }
}

unsafe fn h264_rfc6381(par: *mut sys::AVCodecParameters) -> Option<String> {
    if (*par).extradata_size >= 4 && !(*par).extradata.is_null() {
        let bytes = std::slice::from_raw_parts((*par).extradata, (*par).extradata_size as usize);
        if bytes.first() == Some(&1) {
            return Some(format!(
                "avc1.{:02X}{:02X}{:02X}",
                bytes[1], bytes[2], bytes[3]
            ));
        }
    }
    let profile = u8::try_from((*par).profile).ok()?;
    let level = u8::try_from((*par).level).ok()?;
    Some(format!("avc1.{profile:02X}00{level:02X}"))
}

unsafe fn hevc_rfc6381(par: *mut sys::AVCodecParameters) -> Option<String> {
    if (*par).extradata_size < 13 || (*par).extradata.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts((*par).extradata, (*par).extradata_size as usize);
    hevc_codec_from_hvcc(bytes, (*par).codec_tag)
}

fn hevc_codec_from_hvcc(bytes: &[u8], codec_tag: u32) -> Option<String> {
    if bytes.len() < 13 || bytes[0] != 1 {
        return None;
    }
    let profile_space = match bytes[1] >> 6 {
        1 => "A",
        2 => "B",
        3 => "C",
        _ => "",
    };
    let tier = if bytes[1] & 0x20 != 0 { 'H' } else { 'L' };
    let profile_idc = bytes[1] & 0x1f;
    let compatibility = u32::from_be_bytes(bytes[2..6].try_into().ok()?).reverse_bits();
    let prefix = if codec_tag == u32::from_le_bytes(*b"hev1") {
        "hev1"
    } else {
        "hvc1"
    };
    let mut codec = format!(
        "{prefix}.{profile_space}{profile_idc}.{compatibility:X}.{tier}{}",
        bytes[12]
    );
    let constraint_len = bytes[6..12]
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    if constraint_len > 0 {
        codec.push('.');
        for byte in &bytes[6..6 + constraint_len] {
            use std::fmt::Write as _;
            let _ = write!(codec, "{byte:02X}");
        }
    }
    Some(codec)
}

unsafe fn dovi_profile(par: *mut sys::AVCodecParameters) -> Option<u8> {
    let kind = sys::AVPacketSideDataType::AV_PKT_DATA_DOVI_CONF;
    let mut sz = 0usize;
    let from_st = rusty_dlna_codec_side_data(par, kind, &mut sz);
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
        while let Some(rel) = buf[start..].windows(4).position(|w| w == tag) {
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
    matches!(
        &b,
        b"DIVX" | b"DX50" | b"XVID" | b"divx" | b"dx50" | b"xvid"
    )
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
    // SAFETY: libav returns a process-lifetime codec-name string for any codec
    // identifier; `c_str` also handles a null pointer by returning "".
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
    attached_pic_stream_with_timeout(path, Duration::from_secs(30))
}

pub fn attached_pic_stream_with_timeout(path: &Path, timeout: Duration) -> Option<i32> {
    attached_pic_stream_with_cancellation(path, timeout, &CancellationToken::default())
}

fn attached_pic_stream_with_cancellation(
    path: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Option<i32> {
    attached_picture_with_cancellation(path, timeout, 0, cancellation)
        .map(|picture| picture.stream_index)
}

struct AttachedPicture {
    stream_index: i32,
    jpeg: Option<Vec<u8>>,
}

/// Inspect attached-picture metadata once and copy an already-JPEG packet
/// while the libav context still owns it. Some large Matroska files expose
/// cover art only through `AVStream::attached_pic`; asking the ffmpeg CLI to
/// wait for a normal demuxed packet makes it read until the helper deadline.
fn attached_picture_with_cancellation(
    path: &Path,
    timeout: Duration,
    max_jpeg_bytes: u64,
    cancellation: &CancellationToken,
) -> Option<AttachedPicture> {
    init_libav();
    let path_c = path_cstring(path)?;
    let expires = checked_helper_deadline(timeout)?;
    // SAFETY: the CString and boxed deadline remain live while libav uses
    // their pointers. Stream and packet pointers are read only while their
    // format context remains open. Packet copies are bounded before allocation,
    // and each successful context allocation is closed on every return path.
    unsafe {
        let mut deadline = Box::new(ProbeDeadline {
            expires,
            cancellation: cancellation.clone(),
        });
        let mut ctx = sys::avformat_alloc_context();
        if ctx.is_null() {
            return None;
        }
        (*ctx).interrupt_callback = sys::AVIOInterruptCB {
            callback: Some(interrupt_expired),
            opaque: (&mut *deadline as *mut ProbeDeadline).cast(),
        };
        if sys::avformat_open_input(&mut ctx, path_c.as_ptr(), ptr::null_mut(), ptr::null_mut()) < 0
            || ctx.is_null()
        {
            if !ctx.is_null() {
                sys::avformat_close_input(&mut ctx);
            }
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
                let packet = &(*st).attached_pic;
                let jpeg = usize::try_from(packet.size)
                    .ok()
                    .filter(|size| {
                        *size > 0
                            && u64::try_from(*size)
                                .ok()
                                .is_some_and(|size| max_jpeg_bytes > 0 && size <= max_jpeg_bytes)
                            && !packet.data.is_null()
                    })
                    .and_then(|size| {
                        let bytes = std::slice::from_raw_parts(packet.data.cast_const(), size);
                        crate::is_jpeg_bytes(bytes).then(|| {
                            let mut copied = Vec::new();
                            copied.try_reserve_exact(size).ok()?;
                            copied.extend_from_slice(bytes);
                            Some(copied)
                        })?
                    });
                found = Some(AttachedPicture {
                    stream_index: i as i32,
                    jpeg,
                });
                break;
            }
        }
        sys::avformat_close_input(&mut ctx);
        found
    }
}

/// Decode attached cover art to JPEG. `dest` must end in `.jpg`.
pub fn extract_attached_pic(src: &Path, dest: &Path) -> bool {
    extract_attached_pic_result(src, dest).unwrap_or(false)
}

pub fn extract_attached_pic_result(src: &Path, dest: &Path) -> std::io::Result<bool> {
    extract_attached_pic_with_timeout_result(src, dest, Duration::from_secs(30))
}

pub fn extract_attached_pic_with_timeout_result(
    src: &Path,
    dest: &Path,
    timeout: Duration,
) -> std::io::Result<bool> {
    extract_attached_pic_with_limits_result(src, dest, timeout, 256 * 1024 * 1024)
}

pub fn extract_attached_pic_with_limits_result(
    src: &Path,
    dest: &Path,
    timeout: Duration,
    max_alloc_bytes: u64,
) -> std::io::Result<bool> {
    let Some(picture) = attached_picture_with_cancellation(
        src,
        timeout,
        max_alloc_bytes,
        &CancellationToken::default(),
    ) else {
        return Ok(false);
    };
    if let Some(jpeg) = picture.jpeg {
        return with_atomic_image_destination(dest, |temporary| {
            std::fs::write(temporary, jpeg)?;
            Ok(true)
        });
    }
    with_atomic_image_destination(dest, |temporary| {
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
            .arg(max_alloc_bytes.max(16 * 1024 * 1024).to_string())
            .arg("-i")
            .arg(src)
            .args([
                "-map",
                &format!("0:{}", picture.stream_index),
                "-frames:v",
                "1",
                "-an",
            ])
            .arg(temporary);
        command_status_with_timeout(&mut command, timeout).map(|status| status.success())
    })
}

pub(crate) fn extract_attached_pic_file_with_limits_cancelled_result(
    src: &File,
    dest: &Path,
    timeout: Duration,
    max_alloc_bytes: u64,
    cancellation: &CancellationToken,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;

    let proc_path = std::path::PathBuf::from(format!("/proc/self/fd/{}", src.as_raw_fd()));
    let Some(picture) =
        attached_picture_with_cancellation(&proc_path, timeout, max_alloc_bytes, cancellation)
    else {
        return Ok(false);
    };
    if let Some(jpeg) = picture.jpeg {
        return with_atomic_image_destination(dest, |temporary| {
            std::fs::write(temporary, jpeg)?;
            Ok(true)
        });
    }
    with_atomic_image_destination(dest, |temporary| {
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
            .arg(max_alloc_bytes.max(16 * 1024 * 1024).to_string())
            .args(["-i", "/proc/self/fd/3"])
            .args([
                "-map",
                &format!("0:{}", picture.stream_index),
                "-frames:v",
                "1",
                "-an",
            ])
            .arg(temporary);
        command_status_with_file_cancellation(&mut command, src, timeout, cancellation)
            .map(|status| status.success())
    })
}

/// One-shot poster when there is no sidecar and no attached pic.
/// Scale `src` to at most `w`×`h` JPEG at `dest` (`/Resized/`).
pub fn scale_jpeg(src: &Path, dest: &Path, w: u32, h: u32) -> bool {
    scale_jpeg_result(src, dest, w, h).unwrap_or(false)
}

pub fn scale_jpeg_result(src: &Path, dest: &Path, w: u32, h: u32) -> std::io::Result<bool> {
    scale_jpeg_with_options_result(
        src,
        dest,
        w,
        h,
        2,
        Duration::from_secs(30),
        256 * 1024 * 1024,
    )
}

pub fn scale_jpeg_with_options_result(
    src: &Path,
    dest: &Path,
    w: u32,
    h: u32,
    quality: u8,
    timeout: Duration,
    max_alloc_bytes: u64,
) -> std::io::Result<bool> {
    if w == 0 || h == 0 {
        return Ok(false);
    }
    let vf = format!("scale={w}:{h}:force_original_aspect_ratio=decrease");
    with_atomic_image_destination(dest, |temporary| {
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
            .arg(max_alloc_bytes.max(16 * 1024 * 1024).to_string())
            .arg("-threads")
            .arg("1")
            .arg("-i")
            .arg(src)
            .args([
                "-frames:v",
                "1",
                "-vf",
                &vf,
                "-q:v",
                &quality.clamp(2, 31).to_string(),
                "-an",
            ])
            .arg(temporary);
        command_status_with_timeout(&mut command, timeout).map(|status| status.success())
    })
}

/// Descriptor-backed resize used after rooted authorization. The descriptor
/// is duplicated into the child after fork, so the source pathname is never
/// reopened and the descriptor is not leaked to unrelated child processes.
pub fn scale_jpeg_file_with_options_result(
    src: &File,
    dest: &Path,
    w: u32,
    h: u32,
    quality: u8,
    timeout: Duration,
    max_alloc_bytes: u64,
) -> std::io::Result<bool> {
    scale_jpeg_file_with_options_cancelled_result(
        src,
        dest,
        w,
        h,
        quality,
        MediaHelperControl {
            timeout,
            max_alloc_bytes,
            cancellation: &CancellationToken::default(),
        },
    )
}

pub fn scale_jpeg_file_with_options_cancelled_result(
    src: &File,
    dest: &Path,
    w: u32,
    h: u32,
    quality: u8,
    control: MediaHelperControl<'_>,
) -> std::io::Result<bool> {
    if w == 0 || h == 0 {
        return Ok(false);
    }
    let vf = format!("scale={w}:{h}:force_original_aspect_ratio=decrease");
    with_atomic_image_destination(dest, |temporary| {
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
            .arg(control.max_alloc_bytes.max(16 * 1024 * 1024).to_string())
            .args(["-threads", "1", "-i", "/proc/self/fd/3"])
            .args([
                "-frames:v",
                "1",
                "-vf",
                &vf,
                "-q:v",
                &quality.clamp(2, 31).to_string(),
                "-an",
            ])
            .arg(temporary);
        command_status_with_file_cancellation(
            &mut command,
            src,
            control.timeout,
            control.cancellation,
        )
        .map(|status| status.success())
    })
}

pub fn generate_video_thumb(src: &Path, dest: &Path) -> bool {
    generate_video_thumb_result(src, dest).unwrap_or(false)
}

pub fn generate_video_thumb_result(src: &Path, dest: &Path) -> std::io::Result<bool> {
    generate_video_thumb_with_options_result(src, dest, 320, 2, false, Duration::from_secs(30))
}

pub fn generate_video_thumb_with_options_result(
    src: &Path,
    dest: &Path,
    width: u32,
    quality: u8,
    filmstrip: bool,
    timeout: Duration,
) -> std::io::Result<bool> {
    generate_video_thumb_with_limits_result(
        src,
        dest,
        width,
        quality,
        filmstrip,
        timeout,
        256 * 1024 * 1024,
    )
}

pub fn generate_video_thumb_with_limits_result(
    src: &Path,
    dest: &Path,
    width: u32,
    quality: u8,
    filmstrip: bool,
    timeout: Duration,
    max_alloc_bytes: u64,
) -> std::io::Result<bool> {
    if width == 0 {
        return Ok(false);
    }
    let filter = if filmstrip {
        let cell = (width / 4).max(1);
        format!("fps=1/600,scale={cell}:-2,tile=4x1")
    } else {
        format!("scale={width}:-2")
    };
    let frames = if filmstrip { "4" } else { "1" };
    with_atomic_image_destination(dest, |temporary| {
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
            .arg(max_alloc_bytes.max(16 * 1024 * 1024).to_string())
            .args(["-threads", "1", "-ss", "1", "-i"])
            .arg(src)
            .args([
                "-frames:v",
                frames,
                "-vf",
                &filter,
                "-q:v",
                &quality.clamp(2, 31).to_string(),
                "-an",
            ])
            .arg(temporary);
        command_status_with_timeout(&mut command, timeout).map(|status| status.success())
    })
}

pub(crate) fn generate_video_thumb_file_with_limits_cancelled_result(
    src: &File,
    dest: &Path,
    width: u32,
    quality: u8,
    filmstrip: bool,
    control: MediaHelperControl<'_>,
) -> std::io::Result<bool> {
    if width == 0 {
        return Ok(false);
    }
    let filter = if filmstrip {
        let cell = (width / 4).max(1);
        format!("fps=1/600,scale={cell}:-2,tile=4x1")
    } else {
        format!("scale={width}:-2")
    };
    let frames = if filmstrip { "4" } else { "1" };
    with_atomic_image_destination(dest, |temporary| {
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
            .arg(control.max_alloc_bytes.max(16 * 1024 * 1024).to_string())
            .args(["-threads", "1", "-ss", "1", "-i", "/proc/self/fd/3"])
            .args([
                "-frames:v",
                frames,
                "-vf",
                &filter,
                "-q:v",
                &quality.clamp(2, 31).to_string(),
                "-an",
            ])
            .arg(temporary);
        command_status_with_file_cancellation(
            &mut command,
            src,
            control.timeout,
            control.cancellation,
        )
        .map(|status| status.success())
    })
}

static IMAGE_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn with_atomic_image_destination<F>(dest: &Path, generate: F) -> std::io::Result<bool>
where
    F: FnOnce(&Path) -> std::io::Result<bool>,
{
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let sequence = IMAGE_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image.jpg");
    let temporary =
        dest.with_file_name(format!(".{name}.{}-{sequence}.tmp.jpg", std::process::id()));
    let generated = match generate(&temporary) {
        Ok(generated) => generated,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if !generated || !temporary.is_file() {
        let _ = std::fs::remove_file(&temporary);
        return Ok(false);
    }
    match std::fs::rename(&temporary, dest) {
        Ok(()) => Ok(true),
        // Another preparation worker may have won the same physical-source
        // cache key. Its complete atomic output is equally valid.
        Err(_) if dest.is_file() => {
            let _ = std::fs::remove_file(&temporary);
            Ok(true)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

pub fn command_status_with_timeout(
    command: &mut std::process::Command,
    timeout: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    command_output_with_timeout(command, timeout).map(|output| output.status)
}

#[cfg(test)]
pub(crate) fn command_status_with_cancellation(
    command: &mut std::process::Command,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> std::io::Result<std::process::ExitStatus> {
    command_output_with_cancellation(command, timeout, cancellation).map(|output| output.status)
}

pub(crate) fn command_status_with_file_cancellation(
    command: &mut std::process::Command,
    source: &File,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> std::io::Result<std::process::ExitStatus> {
    command_output_supervised(command, timeout, cancellation, Some(source))
        .map(|output| output.status)
}

/// Run a metadata helper with a deadline while continuously draining bounded
/// stdout/stderr. Draining avoids child-process pipe deadlocks; retaining only
/// the first 256 KiB prevents corrupt inputs from growing scanner memory.
pub fn command_output_with_timeout(
    command: &mut std::process::Command,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    command_output_with_cancellation(command, timeout, &CancellationToken::default())
}

pub(crate) fn command_output_with_cancellation(
    command: &mut std::process::Command,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> std::io::Result<std::process::Output> {
    command_output_supervised(command, timeout, cancellation, None)
}

pub(crate) fn command_output_supervised_for_file(
    command: &mut std::process::Command,
    source: &File,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> std::io::Result<std::process::Output> {
    command_output_supervised(command, timeout, cancellation, Some(source))
}

fn command_output_supervised(
    command: &mut std::process::Command,
    timeout: Duration,
    cancellation: &CancellationToken,
    source: Option<&File>,
) -> std::io::Result<std::process::Output> {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome, SupervisionError,
    };
    use std::ops::ControlFlow;

    const LIMIT: usize = 256 * 1024;
    let capture = CaptureConfig::new(LIMIT, CaptureRetention::Head);
    let mut runner = SupervisedCommand::new(command)
        .capture_stdout(capture)
        .capture_stderr(capture);
    if let Some(source) = source {
        runner = runner.inherit_file_at(source, 3)?;
    }
    let deadline = checked_helper_deadline(timeout).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "external command timeout is too large",
        )
    })?;
    match runner.run_until(deadline, Duration::from_millis(20), || {
        if cancellation.is_cancelled() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }) {
        Ok(SupervisedOutcome::Exited(output)) => Ok(std::process::Output {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        }),
        Ok(SupervisedOutcome::NotStarted { .. } | SupervisedOutcome::Stopped { .. }) => {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "external command cancelled",
            ))
        }
        Ok(SupervisedOutcome::Deadline { .. }) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("external command exceeded {} seconds", timeout.as_secs()),
        )),
        Err(SupervisionError::Spawn(error) | SupervisionError::Wait(error)) => Err(error),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

/// Extract the JPEG stored in EXIF IFD1, when present. Offsets in these tags
/// are relative to the TIFF buffer returned by `kamadak-exif`.
pub fn extract_exif_thumbnail_result(src: &Path, dest: &Path) -> std::io::Result<bool> {
    extract_exif_thumbnail_with_limit_result(src, dest, 64 * 1024 * 1024)
}

pub fn extract_exif_thumbnail_with_limit_result(
    src: &Path,
    dest: &Path,
    max_bytes: usize,
) -> std::io::Result<bool> {
    let file = File::open(src)?;
    let exif = match exif::Reader::new().read_from_container(&mut BufReader::new(file)) {
        Ok(exif) => exif,
        Err(exif::Error::NotFound(_)) => return Ok(false),
        Err(error) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
    };
    let offset = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In(1))
        .and_then(|field| field.value.get_uint(0));
    let length = exif
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In(1))
        .and_then(|field| field.value.get_uint(0));
    let (Some(offset), Some(length)) = (offset, length) else {
        return Ok(false);
    };
    let start = offset as usize;
    let Some(end) = start.checked_add(length as usize) else {
        return Ok(false);
    };
    let Some(jpeg) = exif.buf().get(start..end) else {
        return Ok(false);
    };
    if jpeg.len() < 3 || jpeg.len() > max_bytes || jpeg[..3] != [0xff, 0xd8, 0xff] {
        return Ok(false);
    }
    with_atomic_image_destination(dest, |temporary| {
        std::fs::write(temporary, jpeg)?;
        Ok(true)
    })
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
    use rusty_dlna_protocol::{CompactStreamMetadata, MAX_COMPACT_STREAM_METADATA_BYTES};
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rusty-dlna-{label}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create probe test directory");
            Self(path)
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = PathBuf;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    #[test]
    fn exif_dates_require_complete_ascii_timestamps() {
        for value in [
            "",
            "202",
            "2026:01:01",
            "2026:01:01 12:34:5é",
            "2026:01:01 12:34:5�",
            "202é:01:01 12:34:56",
            "2026:01:01 12-34:56",
            "2026:aa:01 12:34:56",
        ] {
            assert_eq!(exif_date(value.into()), None, "{value}");
        }
        assert_eq!(
            exif_date("2026:01:01 12:34:56".into()).as_deref(),
            Some("2026-01-01T12:34:56Z")
        );
        for value in ["2026", "2026-01-01"] {
            assert_eq!(exif_date(value.into()).as_deref(), Some("2026-01-01"));
        }
        assert_eq!(
            exif_date("2026-01-01T12:34:56+05:30".into()).as_deref(),
            Some("2026-01-01T12:34:56+05:30")
        );
        for value in ["2026-01-01T12:34:56", "2026-01-01T12:34:56Z"] {
            assert_eq!(
                exif_date(value.into()).as_deref(),
                Some("2026-01-01T12:34:56Z")
            );
        }
    }

    #[test]
    fn persisted_stream_metadata_matches_the_legacy_probe_bytes() {
        let probe = SourceProbe {
            video_profile: "Main 10".into(),
            video_level: 153,
            pixel_format: "yuv420p10le".into(),
            bit_depth: 10,
            frame_rate: "24000/1001".into(),
            codec_string: "hvc1.2.4.L153.B0,mp4a.40.2".into(),
            audio_layout: "5.1".into(),
            video_timestamp_mode: "broken-reordered".into(),
            ..SourceProbe::default()
        };
        let tracks = [
            AudioTrackProbe {
                index: 0,
                codec: "truehd".into(),
                channels: 6,
                ..AudioTrackProbe::default()
            },
            AudioTrackProbe {
                index: 1,
                codec: "aac".into(),
                channels: 2,
                language: Some("en,US".into()),
                title: Some("Dub: Кино|100%".into()),
                default: true,
            },
        ];
        let chapters = [ChapterProbe {
            start_seconds: 1.25,
            end_seconds: 4.0,
            title: Some("Intro: Кино".into()),
        }];
        assert_eq!(
            persisted_stream_metadata(&probe, &tracks, &[1, 2], &chapters),
            concat!(
                "1:0:truehd:6,",
                "2:1:aac:2:en%2CUS:Dub%3A %D0%9A%D0%B8%D0%BD%D0%BE%7C100%25:1,",
                "@v:Main 10:153:yuv420p10le:10:24000%2F1001:",
                "hvc1.2.4.L153.B0%2Cmp4a.40.2:5.1,",
                "@t:broken-reordered,",
                "@c:1250:4000:Intro%3A %D0%9A%D0%B8%D0%BD%D0%BE"
            )
        );
    }

    #[test]
    fn persisted_stream_metadata_drops_optional_bulk_before_essential_markers() {
        let probe = SourceProbe {
            video_profile: "High".into(),
            video_level: 41,
            pixel_format: "yuv420p".into(),
            bit_depth: 8,
            frame_rate: "24/1".into(),
            codec_string: "avc1.640029,ac-3".into(),
            audio_layout: "5.1".into(),
            video_timestamp_mode: "broken-reordered".into(),
            ..SourceProbe::default()
        };
        let track = AudioTrackProbe {
            index: 0,
            codec: "ac3".into(),
            channels: 6,
            language: Some("eng".into()),
            title: Some(",".repeat(MAX_COMPACT_STREAM_METADATA_BYTES)),
            default: true,
        };
        let chapter = ChapterProbe {
            start_seconds: 1.0,
            end_seconds: 2.0,
            title: Some("discarded chapter".into()),
        };
        let encoded = persisted_stream_metadata(&probe, &[track], &[1], &[chapter]);
        assert_eq!(
            encoded,
            concat!(
                "1:0:ac3:6:::1,",
                "@v:High:41:yuv420p:8:24%2F1:avc1.640029%2Cac-3:5.1,",
                "@t:broken-reordered"
            )
        );
        let metadata = CompactStreamMetadata::parse(&encoded).unwrap();
        assert!(metadata.has_video_capabilities_marker());
        assert_eq!(metadata.timestamp_mode(), Some("broken-reordered"));
        assert_eq!(metadata.chapters().count(), 0);
        let audio = metadata.audio_records().next().unwrap();
        assert_eq!(audio.decoded_language().unwrap(), None);
        assert_eq!(audio.decoded_title().unwrap(), None);
        assert!(audio.default);
    }

    #[test]
    fn persisted_stream_metadata_caps_audio_and_keeps_capability_records() {
        let probe = SourceProbe {
            video_timestamp_mode: "valid".into(),
            ..SourceProbe::default()
        };
        let tracks = (0..=MAX_COMPACT_AUDIO_RECORDS)
            .map(|index| AudioTrackProbe {
                index,
                codec: "aac".into(),
                channels: 2,
                ..AudioTrackProbe::default()
            })
            .collect::<Vec<_>>();
        let indices = (0..=MAX_COMPACT_AUDIO_RECORDS).collect::<Vec<_>>();
        let encoded = persisted_stream_metadata(&probe, &tracks, &indices, &[]);
        let raw_audio = encoded
            .split(',')
            .take_while(|record| !record.starts_with('@'))
            .collect::<Vec<_>>();
        assert_eq!(raw_audio.len(), MAX_COMPACT_AUDIO_RECORDS);
        assert_eq!(
            raw_audio.last().copied(),
            Some("1023:1023:aac:2"),
            "the writer must omit descriptors beyond the shared bound"
        );
        let metadata = CompactStreamMetadata::parse(&encoded).unwrap();
        assert_eq!(metadata.audio_records().count(), MAX_COMPACT_AUDIO_RECORDS);
        assert!(metadata.has_video_capabilities_marker());
        assert_eq!(metadata.timestamp_mode(), Some("valid"));
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn external_command_deadline_kills_a_stuck_helper() {
        let started = Instant::now();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 2"]);
        let error = command_status_with_timeout(&mut command, Duration::from_millis(50))
            .expect_err("sleep must exceed the one-second minimum deadline");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(1800));
    }

    #[test]
    fn external_command_rejects_unrepresentable_timeout_without_spawning() {
        let tmp = TempDir::new("oversized-timeout");
        let marker = tmp.join("spawned");
        let mut command = std::process::Command::new("touch");
        command.arg(&marker);
        let error = command_status_with_timeout(&mut command, Duration::MAX)
            .expect_err("unrepresentable timeout must fail before spawn");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!marker.exists());
    }

    #[test]
    fn libav_helpers_reject_unrepresentable_timeouts_without_panicking() {
        let tmp = TempDir::new("oversized-libav-timeout");
        let media = tmp.join("media.bin");
        std::fs::write(&media, b"not media").unwrap();
        assert!(probe_media_with_timeout(&media, Duration::MAX).is_none());
        assert!(attached_pic_stream_with_timeout(&media, Duration::MAX).is_none());
    }

    #[test]
    fn external_command_capture_drains_and_keeps_only_the_first_256_kib() {
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "head -c 300000 /dev/zero; head -c 300000 /dev/zero >&2",
        ]);
        let output = command_output_with_timeout(&mut command, Duration::from_secs(2)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 256 * 1024);
        assert_eq!(output.stderr.len(), 256 * 1024);
    }

    #[test]
    fn libav_interrupt_callback_observes_cancellation_before_deadline() {
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let mut deadline = ProbeDeadline {
            expires: Instant::now() + Duration::from_secs(30),
            cancellation,
        };
        // SAFETY: the callback receives a live ProbeDeadline pointer for the
        // duration of this direct invocation.
        let interrupted = unsafe {
            interrupt_expired((&mut deadline as *mut ProbeDeadline).cast::<libc::c_void>())
        };
        assert_eq!(interrupted, 1);
    }

    #[cfg(unix)]
    #[test]
    fn external_command_cancellation_terminates_and_reaps_a_stubborn_group() {
        let tmp = TempDir::new("cancel-helper");
        let pid_path = tmp.join("pid");
        let cancellation = CancellationToken::default();
        let cancel = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel.cancel();
        });
        let mut command = std::process::Command::new("sh");
        command
            .args([
                "-c",
                "echo $$ > \"$1\"; trap '' TERM; while :; do sleep 1; done",
                "sh",
            ])
            .arg(&pid_path);
        let started = Instant::now();
        let error =
            command_status_with_cancellation(&mut command, Duration::from_secs(30), &cancellation)
                .expect_err("cancelled helper must fail");
        canceller.join().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(1));
        let pid: libc::pid_t = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal zero only queries whether this test child remains.
        let result = unsafe { libc::kill(pid, 0) };
        assert_eq!(result, -1, "helper process {pid} still exists");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_helpers_never_inherit_an_open_terminal_stdin() {
        use std::os::fd::FromRawFd;
        use std::process::Stdio;

        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors on success. Each is
        // immediately transferred to exactly one owned File below.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: successful openpty returned unique live descriptors.
        let master = unsafe { File::from_raw_fd(master) };
        // SAFETY: successful openpty returned unique live descriptors.
        let slave = unsafe { File::from_raw_fd(slave) };

        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "read terminal_input"])
            .stdin(Stdio::from(slave));
        let started = Instant::now();
        let status = command_status_with_timeout(&mut command, Duration::from_secs(2))
            .expect("helper should receive EOF from /dev/null");
        assert!(
            !status.success(),
            "shell read unexpectedly received terminal input"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(master);
    }

    #[test]
    fn concurrent_image_outputs_are_atomic_and_leave_no_temporary_files() {
        let dir = TempDir::new("atomic-image");
        let dest = dir.join("result.jpg");
        let mut workers = Vec::new();
        for byte in 1u8..=8 {
            let dest = dest.clone();
            workers.push(std::thread::spawn(move || {
                with_atomic_image_destination(&dest, |temporary| {
                    std::fs::write(temporary, vec![byte; 4096])?;
                    Ok(true)
                })
                .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let bytes = std::fs::read(&dest).unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }
    use crate::write_fake_mkv;

    #[test]
    fn attached_mjpeg_is_copied_from_libav_header_metadata() {
        let dir = TempDir::new("attached-jpeg-packet");
        let poster = dir.join("cover.jpg");
        let video = dir.join("video.mkv");
        let media = dir.join("with-cover.mkv");
        let poster_status = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:size=32x32",
                "-frames:v",
                "1",
            ])
            .arg(&poster)
            .stdin(std::process::Stdio::null())
            .status();
        let video_status = poster_status
            .ok()
            .filter(|status| status.success())
            .and_then(|_| {
                std::process::Command::new("ffmpeg")
                    .args([
                        "-nostdin",
                        "-y",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc=duration=1:size=64x64:rate=2",
                        "-c:v",
                        "libx264",
                        "-pix_fmt",
                        "yuv420p",
                    ])
                    .arg(&video)
                    .stdin(std::process::Stdio::null())
                    .status()
                    .ok()
            });
        let media_status = video_status
            .filter(|status| status.success())
            .and_then(|_| {
                std::process::Command::new("mkvmerge")
                    .args(["--quiet", "-o"])
                    .arg(&media)
                    .arg(&video)
                    .args(["--attachment-mime-type", "image/jpeg", "--attach-file"])
                    .arg(&poster)
                    .stdin(std::process::Stdio::null())
                    .status()
                    .ok()
            });
        if !media_status.is_some_and(|status| status.success()) {
            eprintln!("skip attached JPEG packet test (fixture generation unavailable)");
            return;
        }

        let picture = attached_picture_with_cancellation(
            &media,
            Duration::from_secs(2),
            1024 * 1024,
            &CancellationToken::default(),
        )
        .expect("Matroska fixture must expose attached picture metadata");
        let jpeg = picture
            .jpeg
            .expect("MJPEG attached picture must be copied from AVStream metadata");
        assert!(crate::is_jpeg_bytes(&jpeg));

        let extracted = dir.join("extracted.jpg");
        assert!(extract_attached_pic_with_limits_result(
            &media,
            &extracted,
            Duration::from_secs(2),
            1024 * 1024,
        )
        .unwrap());
        assert!(crate::is_jpeg_bytes(&std::fs::read(extracted).unwrap()));
    }

    #[test]
    fn libav_reads_real_container() {
        let dir = TempDir::new("libav-probe");
        let p = dir.join("clip.mkv");
        write_fake_mkv(&p, 64);
        let got = probe_media(&p).expect("libav probe");
        assert!(!got.probe.video.is_empty(), "{got:?}");
        assert!(
            got.av.duration.is_some() || got.av.resolution.is_some(),
            "{got:?}"
        );
    }

    #[test]
    fn tracked_dolby_vision_fixture_probes_without_a_sidecar() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/library/video/dvp7.mkv");
        assert!(fixture.is_file(), "tracked Profile 7 fixture is missing");
        assert!(
            !fixture.with_extension("probe.toml").exists(),
            "Profile 7 must come from the container, not a synthetic sidecar"
        );
        let media = probe_media(&fixture).expect("probe tracked genuine Profile 7 fixture");
        assert_eq!(media.probe.container, "mkv");
        assert_eq!(media.probe.video, "hevc");
        assert_eq!(media.probe.hdr, "dv-p7");
        assert_eq!(media.probe.audio, "truehd");
        assert!(media.probe.audio_streams.starts_with("1:0:truehd:6,@v:"));
        assert_eq!(media.probe.video_timestamp_mode, "valid");
        assert!(media.probe.audio_streams.contains(",@t:valid"));
        assert_eq!((media.probe.width, media.probe.height), (256, 144));
    }

    #[test]
    fn generated_advanced_media_is_bounded_and_probed_from_real_streams() {
        let dir = TempDir::new("advanced-media");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/generate-advanced-fixtures.sh");
        let generated = std::process::Command::new(&script)
            .arg(dir.as_ref())
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run advanced fixture generator");
        assert!(
            generated.status.success(),
            "advanced fixture generation failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&generated.stdout),
            String::from_utf8_lossy(&generated.stderr)
        );

        let truehd = probe_media(&dir.join("truehd.mkv")).expect("probe genuine TrueHD");
        assert_eq!(truehd.probe.video, "h264");
        assert_eq!(truehd.probe.audio, "truehd");
        assert!(truehd.probe.audio_streams.starts_with("1:0:truehd:6,@v:"));

        let hdr10 = probe_media(&dir.join("hdr10-mastering.mkv")).expect("probe genuine HDR10");
        assert_eq!(hdr10.probe.video, "hevc");
        assert_eq!(hdr10.probe.hdr, "hdr10");
        assert_eq!((hdr10.probe.width, hdr10.probe.height), (64, 36));

        let unusual =
            probe_media(&dir.join("unusual-layout.mkv")).expect("probe unusual stream layout");
        assert_eq!(unusual.probe.video, "h264");
        assert_eq!(unusual.probe.audio, "truehd,ac3");
        assert!(unusual
            .probe
            .audio_streams
            .starts_with("0:0:truehd:6:::1,2:1:ac3:1,@v:"));
        assert_eq!(unusual.av.subs.as_deref(), Some("subrip"));

        let oversized = probe_media(&dir.join("oversized-metadata.mkv"))
            .expect("probe oversized embedded metadata");
        assert_eq!(oversized.tags.title.as_deref(), Some("bounded-title"));
        assert_eq!(
            oversized.tags.comment, None,
            "an embedded tag above the allocation budget must be omitted"
        );

        for name in ["truncated.mkv", "corrupt.mkv"] {
            let started = Instant::now();
            let probe = probe_media_with_timeout(&dir.join(name), Duration::from_secs(2));
            assert!(
                probe.as_ref().is_none_or(|media| {
                    media.probe.video.is_empty() && media.probe.audio.is_empty()
                }),
                "malformed fixture unexpectedly exposed playable streams: {name}: {probe:?}"
            );
            assert!(started.elapsed() < Duration::from_secs(3));
        }

        let cfg = crate::ScanConfig {
            media_dirs: vec![dir.to_path_buf()],
            db_path: Some(dir.join("files.db")),
            types: crate::MediaTypes::video_only(),
            thumbnails: false,
            subtitles: false,
            ..crate::ScanConfig::default()
        };
        let catalog = crate::scan(&cfg).expect("scan generated advanced fixtures");
        for valid in [
            "truehd.mkv",
            "hdr10-mastering.mkv",
            "unusual-layout.mkv",
            "oversized-metadata.mkv",
        ] {
            assert!(
                catalog
                    .items
                    .values()
                    .any(|item| item.path.ends_with(valid)),
                "valid advanced fixture was not indexed: {valid}"
            );
        }
        for invalid in ["truncated.mkv", "corrupt.mkv"] {
            for item in catalog
                .items
                .values()
                .filter(|item| item.path.ends_with(invalid))
            {
                assert!(
                    item.probe.video.is_empty()
                        && item.probe.audio.is_empty()
                        && item.duration.is_none(),
                    "malformed fixture must never gain fabricated stream metadata: {item:?}"
                );
            }
        }
    }

    #[test]
    fn mpeg4_is_not_other() {
        assert_eq!(map_video(sys::AVCodecID::AV_CODEC_ID_MPEG4), "mpeg4");
        assert_eq!(map_video(sys::AVCodecID::AV_CODEC_ID_MSMPEG4V3), "mpeg4");
        assert_eq!(map_video(sys::AVCodecID::AV_CODEC_ID_HEVC), "hevc");
    }

    #[test]
    fn hevc_hvcc_metadata_becomes_an_rfc_6381_codec_string() {
        let mut hvcc = [0u8; 23];
        hvcc[0] = 1;
        hvcc[1] = 1;
        hvcc[2..6].copy_from_slice(&0x6000_0000u32.to_be_bytes());
        hvcc[6] = 0xb0;
        hvcc[12] = 93;
        assert_eq!(
            hevc_codec_from_hvcc(&hvcc, u32::from_le_bytes(*b"hvc1")).as_deref(),
            Some("hvc1.1.6.L93.B0")
        );
        assert_eq!(
            hevc_codec_from_hvcc(&hvcc, u32::from_le_bytes(*b"hev1")).as_deref(),
            Some("hev1.1.6.L93.B0")
        );
        let mut invalid = hvcc;
        invalid[0] = 0;
        assert_eq!(hevc_codec_from_hvcc(&invalid, 0), None);
    }
}
