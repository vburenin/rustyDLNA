//! Codec remaps: ad-hoc “if this stream, recode like that.”
//!
//! Default is serve the original. A `[[remap]]` row matches **codecs and
//! related stream traits** (container, HEVC, DV profile, audio, client
//! kind) — not titles or paths. First matching row wins.

pub use rusty_dlna_helper::{JobGate, JobPermit};
use rusty_dlna_protocol::{
    identify_user_agent, ClientFlags, ClientKind, ClientProfile, CompactStreamMetadata,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceMedia {
    pub container: Container,
    pub video_codec: VideoCodec,
    pub hdr: HdrKind,
    pub audio: AudioCodec,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Container {
    Mkv,
    Mp4,
    MpegTs,
    Avi,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoCodec {
    Hevc,
    H264,
    Mpeg2,
    Mpeg4,
    Other,
}

/// What the bitstream actually is. Profile 8 on the Streamer is often
/// fine; Profile 7 (UHD BD BL+EL) is the usual problem child.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HdrKind {
    Sdr,
    Hdr10,
    /// Dual-layer UHD BD remux (`dvhe.07`).
    #[serde(alias = "dv-p7", alias = "dvhe.07", alias = "profile-7")]
    DolbyVisionProfile7,
    /// Single-layer streaming DV (`dvhe.08` / 8.1).
    #[serde(alias = "dv-p8", alias = "dvhe.08", alias = "profile-8")]
    DolbyVisionProfile8,
    /// IPTV-style Profile 5 (`dvhe.05`).
    #[serde(alias = "dv-p5", alias = "dvhe.05", alias = "profile-5")]
    DolbyVisionProfile5,
    #[serde(alias = "dv")]
    DolbyVisionOther,
    /// No successful probe yet. Remaps must not fire.
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HardwareDecode {
    #[default]
    None,
    Cuda,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioCodec {
    Aac,
    Ac3,
    Eac3,
    #[serde(alias = "truehd", alias = "atmos")]
    TrueHd,
    #[serde(alias = "dts-hd", alias = "dtshd")]
    Dts,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    ServeOriginal,
    Recode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecodeAction {
    /// Leave the file alone (useful to carve out an exception).
    #[default]
    Original,
    /// HEVC BL + rewrite RPU to Profile 8.1. Keeps DV; no NVENC.
    RemuxP8,
    /// Full encode to HDR10 (PQ + BT.2020). Drops DV.
    Hdr10,
    /// Copy video; convert or pick a lossy audio track.
    AudioAc3,
    /// Internal browser-compatibility MP4/AAC output.
    ///
    /// This action is constructed by the embedded web player and is rejected
    /// in user-authored `[[remap]]` rules.
    Browser,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioAction {
    Copy,
    ToAc3,
    ToAac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserQuality {
    Auto,
    UhdHigh,
    UhdOptimized,
    FullHd,
    DataSaver,
    Sd480,
    Low360,
}

const BROWSER_TIMELINE_CACHE_REVISION: &str = "aligned-seek-v2";
const BROWSER_CHAPTER_MAP_CACHE_REVISION: &str = "browser-no-chapters-v1";
const SDR_TONEMAP_CACHE_REVISION: &str = "sdr-tonemap-libplacebo-v2";
const BROWSER_HDR_SOURCE_CACHE_REVISION: &str = "browser-hdr-source-v1";
const BROWSER_AAC_FILTER_CACHE_REVISION: &str = "browser-aac-adtstoasc-v1";
const BROWSER_HEVC_TAG_CACHE_REVISION: &str = "browser-hevc-hvc1-v1";
const BROWSER_MIXED_COPY_SEEK_CACHE_REVISION: &str = "browser-mixed-copy-seek-v2";
const BROWSER_CUDA_DOWNLOAD_CACHE_REVISION: &str = "browser-cuda-download-v1";
const BROWSER_ADAPTIVE_H264_LEVEL_CACHE_REVISION: &str = "browser-adaptive-h264-level-v1";
const BROWSER_NVENC_IDR_CACHE_REVISION: &str = "browser-nvenc-idr-v1";
const BROWSER_DATA_SAVER_BASELINE_CACHE_REVISION: &str = "browser-data-saver-baseline-v1";
const BROWSER_HLS_CACHE_REVISION: &str = "browser-hls-v1";
const BROWSER_AI_UPSCALE_CACHE_REVISION: &str = "browser-ai-upscale-libplacebo-v1";
const PROFILE8_TOOLCHAIN_CACHE_REVISION: &str = "profile8-toolchain-v2";
const CACHE_DIGEST_HEX_BYTES: usize = 64;
const MAX_BROWSER_CACHE_KEY_BYTES: usize = 512;
const VERIFIED_EXECUTABLE_FD: std::os::fd::RawFd = 4;
/// Child descriptor reserved for an immutable browser AI-upscale shader.
pub const BROWSER_AI_UPSCALE_SHADER_FD: std::os::fd::RawFd = 5;

#[cfg(target_os = "linux")]
const VERIFIED_EXECUTABLE_CHILD_PATH: &str = "/proc/self/fd/4";
#[cfg(all(unix, not(target_os = "linux")))]
const VERIFIED_EXECUTABLE_CHILD_PATH: &str = "/dev/fd/4";

#[cfg(target_os = "linux")]
/// Child-visible path for [`BROWSER_AI_UPSCALE_SHADER_FD`].
pub const BROWSER_AI_UPSCALE_SHADER_CHILD_PATH: &str = "/proc/self/fd/5";
#[cfg(all(unix, not(target_os = "linux")))]
/// Child-visible path for [`BROWSER_AI_UPSCALE_SHADER_FD`].
pub const BROWSER_AI_UPSCALE_SHADER_CHILD_PATH: &str = "/dev/fd/5";

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// User-selected speed/latency tradeoff; independent of output resolution/HDR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserEncodingPreset {
    #[default]
    Balanced,
    FastStart,
    MaximumSpeed,
}

impl BrowserEncodingPreset {
    pub const ALL: [Self; 3] = [Self::Balanced, Self::FastStart, Self::MaximumSpeed];

    pub fn id(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::FastStart => "fast_start",
            Self::MaximumSpeed => "maximum_speed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Balanced => "Balanced",
            Self::FastStart => "Fast start",
            Self::MaximumSpeed => "Maximum speed",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Balanced => "Default quality and compression",
            Self::FastStart => "Less encoder buffering; may reduce quality at the same bitrate",
            Self::MaximumSpeed => "Faster encoding with a larger quality tradeoff",
        }
    }
}

/// Source and request traits that complete an embedded-browser output plan.
///
/// `TranscodePlan` owns codec/encoder selection. These options own the
/// browser-specific MP4 signaling, SDR conversion, and input timeline policy
/// that depends on the selected source streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserOutputOptions {
    pub encoding_preset: BrowserEncodingPreset,
    /// `None` for audio-only output.
    pub source_video: Option<VideoCodec>,
    pub selected_audio: AudioCodec,
    pub source_hdr: HdrKind,
    pub start_seconds: usize,
    /// HLS/Media Source delivery uses bounded input pacing.
    pub hls: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BrowserOutputPolicy {
    source_requires_sdr_tonemap: bool,
    apply_sdr_tonemap: bool,
    filter_copied_aac: bool,
    tag_hevc_as_hvc1: bool,
    seek: BrowserSeekPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserSeekPolicy {
    Accurate,
    PreserveCopiedVideoPreroll,
    PreciselyTrimCopiedAudioPreroll,
}

fn browser_output_policy(
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
) -> BrowserOutputPolicy {
    let has_video = options.source_video.is_some();
    let source_requires_sdr_tonemap = has_video && browser_requires_sdr_tonemap(options.source_hdr);
    let video_is_copied = has_video && plan.video_encoder == "copy";
    let audio_is_copied = plan.audio == AudioAction::Copy;
    let seek = if options.start_seconds == 0 || !has_video || video_is_copied == audio_is_copied {
        BrowserSeekPolicy::Accurate
    } else if audio_is_copied {
        BrowserSeekPolicy::PreciselyTrimCopiedAudioPreroll
    } else {
        BrowserSeekPolicy::PreserveCopiedVideoPreroll
    };
    BrowserOutputPolicy {
        source_requires_sdr_tonemap,
        apply_sdr_tonemap: source_requires_sdr_tonemap
            && plan.video_encoder != "copy"
            && plan.video_encoder != "hevc_nvenc",
        filter_copied_aac: audio_is_copied && options.selected_audio == AudioCodec::Aac,
        tag_hevc_as_hvc1: options.source_video == Some(VideoCodec::Hevc)
            && matches!(plan.video_encoder.as_str(), "copy" | "hevc_nvenc"),
        seek,
    }
}

impl BrowserQuality {
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::UhdHigh => "uhd_high",
            Self::UhdOptimized => "uhd_optimized",
            Self::FullHd => "full_hd",
            Self::DataSaver => "data_saver",
            Self::Sd480 => "sd_480",
            Self::Low360 => "low_360",
        }
    }

    pub fn max_width(self) -> u32 {
        match self {
            Self::Auto | Self::UhdHigh | Self::UhdOptimized => 3840,
            Self::FullHd => 1920,
            Self::DataSaver => 1280,
            Self::Sd480 => 854,
            Self::Low360 => 640,
        }
    }

    pub fn max_height(self) -> u32 {
        match self {
            Self::Auto | Self::UhdHigh | Self::UhdOptimized => 2160,
            Self::FullHd => 1080,
            Self::DataSaver => 720,
            Self::Sd480 => 480,
            Self::Low360 => 360,
        }
    }

    pub fn max_fps(self) -> u32 {
        30
    }

    pub fn h264_profile(self) -> &'static str {
        match self {
            Self::Auto | Self::UhdHigh | Self::UhdOptimized | Self::FullHd => "high",
            Self::DataSaver | Self::Sd480 | Self::Low360 => "baseline",
        }
    }

    pub fn h264_level(self) -> &'static str {
        match self {
            Self::Auto | Self::UhdHigh | Self::UhdOptimized => "5.1",
            Self::FullHd => "4.1",
            Self::DataSaver | Self::Sd480 => "3.1",
            Self::Low360 => "3.0",
        }
    }

    pub fn crf(self) -> u8 {
        match self {
            Self::Auto | Self::UhdHigh => 20,
            Self::UhdOptimized | Self::FullHd => 22,
            Self::DataSaver => 25,
            Self::Sd480 => 26,
            Self::Low360 => 27,
        }
    }

    pub fn max_video_kbps(self) -> u32 {
        match self {
            Self::Auto => 25_000,
            Self::UhdHigh => 25_000,
            Self::UhdOptimized => 16_000,
            Self::FullHd => 8_000,
            Self::DataSaver => 3_000,
            Self::Sd480 => 1_500,
            Self::Low360 => 800,
        }
    }

    pub fn buffer_kbps(self) -> u32 {
        self.max_video_kbps() * 2
    }

    pub fn audio_kbps(self) -> u32 {
        match self {
            Self::Auto | Self::UhdHigh | Self::UhdOptimized | Self::FullHd => 192,
            Self::DataSaver | Self::Sd480 => 128,
            Self::Low360 => 96,
        }
    }

    /// Whether this rendition is safe enough for automatic decoder recovery.
    /// Explicitly selected lower qualities remain available without making an
    /// ordinary playback failure silently collapse all the way to 360p.
    pub fn automatic_fallback(self) -> bool {
        self == Self::DataSaver
    }

    fn mobile_safe_h264(self) -> bool {
        matches!(self, Self::DataSaver | Self::Sd480 | Self::Low360)
    }

    pub fn expected_bandwidth_kbps(self) -> u32 {
        self.max_video_kbps() + self.audio_kbps() + 256
    }
}

/// One software name or a list. `client = "CrKey"` or
/// `clients = ["CrKey", "Kodi"]`. Empty = any software.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ClientSelector(pub Vec<String>);

impl ClientSelector {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

impl<'de> Deserialize<'de> for ClientSelector {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }
        Ok(match Option::<Raw>::deserialize(d)? {
            None => ClientSelector(Vec::new()),
            Some(Raw::One(s)) if s.is_empty() => ClientSelector(Vec::new()),
            Some(Raw::One(s)) => ClientSelector(vec![s]),
            Some(Raw::Many(v)) => ClientSelector(v),
        })
    }
}

/// One ad-hoc row. Unset match fields are wildcards.
/// `client` / `clients` name the **software** (UA token or profile),
/// not a title.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RemapRule {
    /// Optional label for logs (`name = "streamer-p7"`).
    pub name: Option<String>,
    #[serde(alias = "clients")]
    pub client: ClientSelector,
    pub container: Option<Container>,
    pub video: Option<VideoCodec>,
    pub hdr: Option<HdrKind>,
    pub audio: Option<AudioCodec>,
    pub action: RecodeAction,
    pub encoder: Option<String>,
    pub audio_out: Option<AudioAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscodePlan {
    pub decision: Decision,
    pub action: RecodeAction,
    pub rule: Option<String>,
    pub keep_hdr10: bool,
    pub drop_dolby_vision: bool,
    pub video_encoder: String,
    pub hardware_decode: HardwareDecode,
    pub audio: AudioAction,
    pub container: &'static str,
    /// `0:a:{n}` among audio streams. Prefer a lossy track when known.
    pub audio_index: usize,
    /// Explicit browser output envelope. Non-browser plans leave this unset.
    pub browser_quality: Option<BrowserQuality>,
    /// Descriptor-backed libplacebo shader selected for an explicit SDR-only
    /// upscale. The shader bytes are represented by `shader_sha256`.
    pub browser_ai_upscale: Option<BrowserAiUpscale>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Identity of one descriptor-backed browser neural-upscale shader.
pub struct BrowserAiUpscale {
    /// Validated operator-facing model name.
    pub model: String,
    /// SHA-256 of the immutable shader bytes inherited by FFmpeg.
    pub shader_sha256: String,
}

impl Default for TranscodePlan {
    fn default() -> Self {
        Self {
            decision: Decision::ServeOriginal,
            action: RecodeAction::Original,
            rule: None,
            keep_hdr10: true,
            drop_dolby_vision: false,
            video_encoder: "copy".into(),
            hardware_decode: HardwareDecode::None,
            audio: AudioAction::Copy,
            container: "original",
            audio_index: 0,
            browser_quality: None,
            browser_ai_upscale: None,
        }
    }
}

/// First `aac`/`ac3`/`eac3`/`mp3` in a comma-separated probe list, else 0.
pub fn pick_audio_index(audio_csv: &str) -> usize {
    audio_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .position(|c| matches!(c, "aac" | "ac3" | "eac3" | "mp3"))
        .unwrap_or(0)
}

/// Select from persisted per-stream descriptors rather than the de-duplicated
/// codec summary. Records are `global-stream:audio-ordinal:codec:channels`.
pub fn pick_audio_index_from_streams(descriptors: &str, fallback_audio_csv: &str) -> usize {
    let Ok(metadata) = CompactStreamMetadata::parse(descriptors) else {
        return pick_audio_index(fallback_audio_csv);
    };
    pick_audio_index_from_metadata(metadata, fallback_audio_csv)
}

/// Select an audio ordinal from already-validated compact stream metadata.
///
/// Three-field records remain valid for selection. The first valid record is
/// the fallback when no browser-compatible lossy codec is present.
pub fn pick_audio_index_from_metadata(
    metadata: CompactStreamMetadata<'_>,
    fallback_audio_csv: &str,
) -> usize {
    let mut first_audio_index = None;
    for record in metadata.audio_records() {
        first_audio_index.get_or_insert(record.audio_index);
        if matches!(record.codec, "aac" | "ac3" | "eac3" | "mp3") {
            return record.audio_index;
        }
    }
    first_audio_index.unwrap_or_else(|| pick_audio_index(fallback_audio_csv))
}

fn audio_map_arg(plan: &TranscodePlan) -> String {
    format!("0:a:{}?", plan.audio_index)
}

/// Select an H.264 encoder suitable for browser compatibility output. The
/// global encoder may be HEVC for DLNA HDR rules, which browsers cannot use
/// as a general fallback format.
pub fn browser_video_encoder(configured: &str) -> &'static str {
    match configured {
        "h264_nvenc" => "h264_nvenc",
        _ => "libx264",
    }
}

/// Select browser decode hardware independently from the output encoder.
///
/// H.264 software decode feeds NVENC without the CUDA download/re-upload path,
/// which substantially lowers first-fragment latency while retaining ample
/// sustained throughput. HEVC keeps CUDA decode because its CPU cost is much
/// higher and the browser HDR pipeline already crosses a hardware boundary.
pub fn browser_hardware_decode(video_encoder: &str, source_video: VideoCodec) -> HardwareDecode {
    if matches!(video_encoder, "h264_nvenc" | "hevc_nvenc") && source_video == VideoCodec::Hevc {
        HardwareDecode::Cuda
    } else {
        HardwareDecode::None
    }
}

/// Select decode hardware for a browser HDR10 encode.
///
/// Dolby Vision Profile 7 carries an enhancement layer that CUDA decode does
/// not reliably download as an ordinary Main 10 frame. Decode its HDR10 base
/// layer in software while retaining NVENC for the bounded output encode.
pub fn browser_hdr_hardware_decode(
    video_encoder: &str,
    source_video: VideoCodec,
    source_hdr: HdrKind,
) -> HardwareDecode {
    if source_hdr == HdrKind::DolbyVisionProfile7 {
        HardwareDecode::None
    } else {
        browser_hardware_decode(video_encoder, source_video)
    }
}

/// Select the encoder for a browser frame-order repair.
///
/// HEVC Main 10 output is reserved for HDR formats whose base layer can be
/// preserved without converting Dolby Vision-only presentation semantics.
/// Other HDR/Dolby Vision repairs use the configured H.264 compatibility path,
/// which applies the browser SDR tone-map policy.
pub fn browser_repair_video_encoder(
    configured: &str,
    source_video: VideoCodec,
    source_hdr: HdrKind,
    bit_depth: u32,
) -> &'static str {
    if configured == "h264_nvenc"
        && source_video == VideoCodec::Hevc
        && bit_depth > 8
        && matches!(source_hdr, HdrKind::Hdr10 | HdrKind::DolbyVisionProfile8)
    {
        "hevc_nvenc"
    } else {
        browser_video_encoder(configured)
    }
}

fn hdr10_encode_args(plan: &TranscodePlan) -> Vec<String> {
    let x264 = plan.video_encoder.contains("x264");
    let mut a = vec![
        "-vf".into(),
        if x264 {
            "format=yuv420p10le".into()
        } else {
            "format=p010le".into()
        },
        "-c:v".into(),
        plan.video_encoder.clone(),
        "-profile:v".into(),
        if x264 {
            "high10".into()
        } else {
            "main10".into()
        },
        "-pix_fmt".into(),
        if x264 {
            "yuv420p10le".into()
        } else {
            "p010le".into()
        },
        "-color_primaries".into(),
        "bt2020".into(),
        "-color_trc".into(),
        "smpte2084".into(),
        "-colorspace".into(),
        "bt2020nc".into(),
    ];
    if !x264 {
        a.extend(["-tag:v".into(), "hvc1".into()]);
    }
    a
}

/// Does `want` name this software? Tokens are UA fragments (`CrKey`,
/// `Kodi`), table names (`Google Cast / Streamer`), or aliases
/// (`google-cast`, `streamer`, `samsung`).
pub fn software_matches(want: &str, profile: &ClientProfile, raw_ua: Option<&str>) -> bool {
    let w = want.trim();
    if w.is_empty() || w.eq_ignore_ascii_case("any") || w == "*" {
        return true;
    }
    let wl = w.to_ascii_lowercase();
    if matches!(
        wl.as_str(),
        "google-cast" | "cast" | "crkey" | "streamer" | "chromecast"
    ) {
        return profile.kind == ClientKind::GoogleCast
            || raw_ua.is_some_and(|u| u.to_ascii_lowercase().contains("crkey"));
    }
    if wl == "kodi" {
        return profile.kind == ClientKind::Kodi
            || raw_ua.is_some_and(|u| u.to_ascii_lowercase().contains("kodi"));
    }
    if wl == "samsung" {
        return profile.flags.contains(ClientFlags::SAMSUNG);
    }
    if profile.name.eq_ignore_ascii_case(w) {
        return true;
    }
    if let Some(m) = profile.match_str {
        if m.eq_ignore_ascii_case(w) || w.contains(m) || m.contains(w) {
            return true;
        }
    }
    if raw_ua.is_some_and(|u| u.to_ascii_lowercase().contains(&wl)) {
        return true;
    }
    identify_user_agent(w).is_some_and(|p| p.kind == profile.kind && p.name == profile.name)
}

impl RemapRule {
    pub fn matches(&self, client: &ClientProfile, src: &SourceMedia) -> bool {
        self.matches_ua(client, None, src)
    }

    pub fn matches_ua(
        &self,
        client: &ClientProfile,
        raw_ua: Option<&str>,
        src: &SourceMedia,
    ) -> bool {
        if !self.client.is_empty() {
            let ok = self
                .client
                .tokens()
                .any(|t| software_matches(t, client, raw_ua));
            if !ok {
                return false;
            }
        }
        if self.container.is_some_and(|c| c != src.container) {
            return false;
        }
        if self.video.is_some_and(|v| v != src.video_codec) {
            return false;
        }
        if self.hdr.is_some_and(|h| h != src.hdr) {
            return false;
        }
        if self.audio.is_some_and(|a| a != src.audio) {
            return false;
        }
        true
    }
}

/// First matching remap. No match → original. Never infers a recode
/// from the client alone (Streamer plays most DV just fine).
pub fn decide(client: &ClientProfile, src: &SourceMedia, remaps: &[RemapRule]) -> TranscodePlan {
    decide_for(client, None, src, remaps)
}

/// Same, with the live User-Agent so `client = "CrKey"` can match the
/// particular software, not just the coarse profile.
pub fn decide_ua(ua: &str, src: &SourceMedia, remaps: &[RemapRule]) -> TranscodePlan {
    let profile =
        identify_user_agent(ua).or_else(|| rusty_dlna_protocol::identify_x_av_client_info(ua));
    let Some(profile) = profile else {
        return TranscodePlan::default();
    };
    decide_for(profile, Some(ua), src, remaps)
}

pub fn decide_for(
    client: &ClientProfile,
    raw_ua: Option<&str>,
    src: &SourceMedia,
    remaps: &[RemapRule],
) -> TranscodePlan {
    decide_for_with_default_encoder(client, raw_ua, src, remaps, None)
}

/// Decide using the process-wide encoder as the default for rules that
/// actually encode video (`action = "hdr10"`). Remux-P8 and audio-only
/// conversion always copy video, so applying a global encoder to them would
/// silently change the action's documented meaning.
pub fn decide_for_with_default_encoder(
    client: &ClientProfile,
    raw_ua: Option<&str>,
    src: &SourceMedia,
    remaps: &[RemapRule],
    default_encoder: Option<&str>,
) -> TranscodePlan {
    if src.hdr == HdrKind::Unknown {
        return TranscodePlan::default();
    }
    let Some(rule) = remaps.iter().find(|r| r.matches_ua(client, raw_ua, src)) else {
        return TranscodePlan::default();
    };
    plan_from_rule(rule, src, default_encoder)
}

fn plan_from_rule(
    rule: &RemapRule,
    src: &SourceMedia,
    default_encoder: Option<&str>,
) -> TranscodePlan {
    match rule.action {
        RecodeAction::Original => TranscodePlan {
            rule: rule.name.clone(),
            ..TranscodePlan::default()
        },
        RecodeAction::RemuxP8 => TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::RemuxP8,
            rule: rule.name.clone(),
            keep_hdr10: false,
            drop_dolby_vision: false,
            video_encoder: rule.encoder.clone().unwrap_or_else(|| "copy".into()),
            hardware_decode: HardwareDecode::None,
            audio: rule.audio_out.unwrap_or(AudioAction::Copy),
            container: "mp4",
            audio_index: 0,
            browser_quality: None,
            browser_ai_upscale: None,
        },
        RecodeAction::Hdr10 => TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Hdr10,
            rule: rule.name.clone(),
            keep_hdr10: true,
            drop_dolby_vision: true,
            video_encoder: rule
                .encoder
                .clone()
                .or_else(|| default_encoder.map(str::to_owned))
                .unwrap_or_else(|| "hevc_nvenc".into()),
            hardware_decode: HardwareDecode::None,
            audio: rule.audio_out.unwrap_or(
                if matches!(
                    src.audio,
                    AudioCodec::Ac3 | AudioCodec::Eac3 | AudioCodec::Aac
                ) {
                    AudioAction::Copy
                } else {
                    AudioAction::ToAc3
                },
            ),
            container: "mp4",
            audio_index: 0,
            browser_quality: None,
            browser_ai_upscale: None,
        },
        RecodeAction::AudioAc3 => TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::AudioAc3,
            rule: rule.name.clone(),
            keep_hdr10: !matches!(src.hdr, HdrKind::Sdr),
            drop_dolby_vision: false,
            video_encoder: rule.encoder.clone().unwrap_or_else(|| "copy".into()),
            hardware_decode: HardwareDecode::None,
            audio: rule.audio_out.unwrap_or(AudioAction::ToAc3),
            container: "original",
            audio_index: 0,
            browser_quality: None,
            browser_ai_upscale: None,
        },
        RecodeAction::Browser => TranscodePlan::default(),
    }
}

pub fn parse_remaps_toml(text: &str) -> Result<Vec<RemapRule>, toml::de::Error> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        remap: Vec<RemapRule>,
    }
    Ok(toml::from_str::<File>(text)?.remap)
}

fn valid_codec_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Reject rule combinations that otherwise produce misleading ffmpeg plans.
/// Binary/encoder availability is intentionally checked separately by the
/// server's `--check`, where actionable host diagnostics can be reported.
pub fn validate_remap_rules(remaps: &[RemapRule], default_encoder: &str) -> Result<(), String> {
    if !valid_codec_name(default_encoder) || default_encoder == "copy" {
        return Err(format!(
            "transcode.encoder must name a video encoder (for example libx264), got {default_encoder:?}"
        ));
    }
    for (index, rule) in remaps.iter().enumerate() {
        let label = rule
            .name
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("remap #{}", index + 1));
        if let Some(encoder) = rule.encoder.as_deref() {
            if !valid_codec_name(encoder) {
                return Err(format!("{label}: invalid encoder name {encoder:?}"));
            }
        }
        match rule.action {
            RecodeAction::Original => {
                if rule.encoder.is_some() || rule.audio_out.is_some() {
                    return Err(format!(
                        "{label}: action=original cannot set encoder or audio_out"
                    ));
                }
            }
            RecodeAction::RemuxP8 => {
                if rule.encoder.as_deref().is_some_and(|value| value != "copy") {
                    return Err(format!(
                        "{label}: action=remux-p8 must use encoder=copy; use action=hdr10 to encode video"
                    ));
                }
                if rule.video.is_some_and(|video| video != VideoCodec::Hevc) {
                    return Err(format!("{label}: action=remux-p8 only supports HEVC video"));
                }
                if rule.hdr.is_some_and(|hdr| {
                    !matches!(
                        hdr,
                        HdrKind::DolbyVisionProfile7 | HdrKind::DolbyVisionOther
                    )
                }) {
                    return Err(format!(
                        "{label}: action=remux-p8 requires Dolby Vision Profile 7 input"
                    ));
                }
            }
            RecodeAction::Hdr10 => {
                let encoder = rule.encoder.as_deref().unwrap_or(default_encoder);
                if encoder == "copy" {
                    return Err(format!(
                        "{label}: action=hdr10 must encode video and cannot use encoder=copy"
                    ));
                }
            }
            RecodeAction::AudioAc3 => {
                if rule.encoder.as_deref().is_some_and(|value| value != "copy") {
                    return Err(format!(
                        "{label}: action=audio-ac3 only converts audio and must use encoder=copy"
                    ));
                }
            }
            RecodeAction::Browser => {
                return Err(format!(
                    "{label}: action=browser is reserved for the embedded web player"
                ));
            }
        }
    }
    Ok(())
}

/// ffmpeg argv that writes a **growing** fragmented MP4. First fragment is
/// playable; the rest is filled in the background. Not `+faststart`.
pub fn ffmpeg_grow_args(src_path: &str, dst_path: &str, plan: &TranscodePlan) -> Vec<String> {
    let mut a = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-nostdin".into(),
    ];
    if plan.hardware_decode == HardwareDecode::Cuda {
        a.extend([
            "-hwaccel".into(),
            "cuda".into(),
            "-hwaccel_output_format".into(),
            "cuda".into(),
        ]);
    }
    a.extend([
        "-i".into(),
        src_path.into(),
        "-map".into(),
        if plan.action == RecodeAction::Browser {
            "0:v:0?".into()
        } else {
            "0:v:0".into()
        },
        "-map".into(),
        audio_map_arg(plan),
    ]);
    if plan.action == RecodeAction::Browser {
        // FFmpeg otherwise maps input chapters into MP4 as an additional text
        // track. Browser output declares only its selected video and audio
        // codecs, and WebKit's Managed Media Source will not expose buffered
        // media when an undeclared chapter track is present.
        a.extend(["-map_chapters".into(), "-1".into()]);
    }
    match plan.action {
        RecodeAction::Original => {}
        RecodeAction::RemuxP8 | RecodeAction::AudioAc3 => {
            a.extend(["-c:v".into(), plan.video_encoder.clone()]);
        }
        RecodeAction::Hdr10 => {
            a.extend(hdr10_encode_args(plan));
        }
        RecodeAction::Browser if plan.video_encoder == "copy" => {
            a.extend(["-c:v".into(), "copy".into()]);
        }
        RecodeAction::Browser => {
            a.extend(["-c:v".into(), plan.video_encoder.clone()]);
            let quality = plan.browser_quality;
            let hevc_nvenc = plan.video_encoder == "hevc_nvenc";
            if matches!(plan.video_encoder.as_str(), "h264_nvenc" | "hevc_nvenc") {
                a.extend([
                    "-preset".into(),
                    "p4".into(),
                    "-tune".into(),
                    "hq".into(),
                    "-rc".into(),
                    "vbr".into(),
                    "-cq".into(),
                    quality
                        .map_or(22, BrowserQuality::crf)
                        .saturating_sub(u8::from(hevc_nvenc) * 2)
                        .to_string(),
                    "-b:v".into(),
                    "0".into(),
                    // HLS segments must begin with an independently decodable
                    // picture. NVENC otherwise may satisfy a forced keyframe
                    // request with a non-IDR I-frame.
                    "-forced-idr".into(),
                    "1".into(),
                ]);
                if plan.hardware_decode == HardwareDecode::Cuda {
                    a.extend([
                        "-vf".into(),
                        browser_scale_filter(
                            quality,
                            true,
                            if hevc_nvenc { "p010le" } else { "yuv420p" },
                        ),
                    ]);
                } else {
                    a.extend([
                        "-pix_fmt".into(),
                        if hevc_nvenc { "p010le" } else { "yuv420p" }.into(),
                    ]);
                    if quality.is_some() {
                        a.extend([
                            "-vf".into(),
                            browser_scale_filter(
                                quality,
                                false,
                                if hevc_nvenc { "p010le" } else { "yuv420p" },
                            ),
                        ]);
                    }
                }
            } else {
                a.extend([
                    "-preset".into(),
                    "veryfast".into(),
                    "-crf".into(),
                    quality.map_or(22, BrowserQuality::crf).to_string(),
                    "-pix_fmt".into(),
                    "yuv420p".into(),
                ]);
                if quality.is_some() {
                    a.extend([
                        "-vf".into(),
                        browser_scale_filter(quality, false, "yuv420p"),
                    ]);
                }
            }
            if let Some(quality) = quality {
                a.extend([
                    "-maxrate".into(),
                    format!("{}k", quality.max_video_kbps()),
                    "-bufsize".into(),
                    format!("{}k", quality.buffer_kbps()),
                    "-fpsmax".into(),
                    quality.max_fps().to_string(),
                ]);
            }
            if hevc_nvenc {
                a.extend([
                    "-profile:v".into(),
                    "main10".into(),
                    "-level:v".into(),
                    "5.1".into(),
                ]);
                if plan.keep_hdr10 {
                    a.extend([
                        "-color_primaries".into(),
                        "bt2020".into(),
                        "-color_trc".into(),
                        "smpte2084".into(),
                        "-colorspace".into(),
                        "bt2020nc".into(),
                    ]);
                }
            } else {
                a.extend([
                    "-profile:v".into(),
                    quality.map_or("high", BrowserQuality::h264_profile).into(),
                ]);
                if quality.is_some_and(BrowserQuality::mobile_safe_h264) {
                    // The mobile-safe rendition must not begin with reordered
                    // B-frames. Some Android Chromium decoders reject a
                    // growing fMP4 when its initial video DTS is negative even
                    // though the same complete file probes successfully.
                    a.extend(["-bf".into(), "0".into(), "-refs".into(), "1".into()]);
                }
                // Auto can retain anything from SD through 4K. A forced 5.1
                // level therefore overstates lower-resolution outputs and is
                // rejected by some Android decoders even when the encoded
                // stream itself fits Level 4.0. Let the encoder derive the
                // minimum valid level from the actual dimensions, rate, and
                // frame rate. Bounded profiles retain their explicit client
                // compatibility levels.
                if quality != Some(BrowserQuality::Auto) {
                    a.extend([
                        "-level:v".into(),
                        quality.map_or("4.1", BrowserQuality::h264_level).into(),
                    ]);
                }
            }
            a.extend(["-force_key_frames".into(), "expr:gte(t,n_forced*2)".into()]);
        }
    }
    match plan.audio {
        AudioAction::Copy => a.extend(["-c:a".into(), "copy".into()]),
        AudioAction::ToAc3 => a.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), "640k".into()]),
        AudioAction::ToAac => {
            let audio_kbps = plan.browser_quality.map_or(256, BrowserQuality::audio_kbps);
            a.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{audio_kbps}k"),
            ]);
            if plan.browser_quality.is_some() {
                a.extend(["-ac".into(), "2".into()]);
            }
        }
    }
    a.extend(live_frag_tail(dst_path));
    a
}

fn browser_scale_filter(quality: Option<BrowserQuality>, cuda: bool, pixel_format: &str) -> String {
    let Some(quality) = quality else {
        return format!("scale_cuda=format={pixel_format},hwdownload,format={pixel_format}");
    };
    if cuda {
        // A mid-stream decoder context change (for example, color metadata
        // becoming explicit in a later H.264 SPS) gives scale_cuda a new
        // hardware-frames context. Download the normalized frames here so
        // NVENC can upload them independently instead of FFmpeg inserting an
        // incompatible auto_scale link between the old and new contexts.
        format!(
            "scale_cuda=w='min(iw,{})':h='min(ih,{})':force_original_aspect_ratio=decrease:force_divisible_by=2:format={pixel_format},hwdownload,format={pixel_format}",
            quality.max_width(),
            quality.max_height()
        )
    } else {
        format!(
            "scale=w='min(iw,{})':h='min(ih,{})':force_original_aspect_ratio=decrease:force_divisible_by=2:flags=fast_bilinear,format={pixel_format}",
            quality.max_width(),
            quality.max_height()
        )
    }
}

/// Path-preserving variant used by the server. Command arguments remain
/// native OS strings so Unix filenames with arbitrary bytes reach ffmpeg
/// unchanged.
pub fn ffmpeg_grow_os_args(
    src_path: &Path,
    dst_path: &Path,
    plan: &TranscodePlan,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = ffmpeg_grow_args("", "", plan)
        .into_iter()
        .map(OsString::from)
        .collect();
    let input = args
        .iter()
        .position(|argument| argument == "-i")
        .map(|index| index + 1)
        .expect("ffmpeg argument builder must include an input");
    args[input] = src_path.as_os_str().to_os_string();
    if let Some(output) = args.last_mut() {
        *output = dst_path.as_os_str().to_os_string();
    }
    args
}

/// Whether browser-compatible video must be mapped to ordinary BT.709 SDR.
pub fn browser_requires_sdr_tonemap(hdr: HdrKind) -> bool {
    matches!(
        hdr,
        HdrKind::Hdr10
            | HdrKind::DolbyVisionProfile5
            | HdrKind::DolbyVisionProfile7
            | HdrKind::DolbyVisionProfile8
            | HdrKind::DolbyVisionOther
    )
}

fn apply_browser_sdr_tonemap(
    args: &mut Vec<OsString>,
    hardware_decode: HardwareDecode,
    quality: BrowserQuality,
) {
    let input = args
        .iter()
        .position(|argument| argument == "-i")
        .expect("ffmpeg browser command must include an input");
    args.splice(
        input..input,
        [
            OsString::from("-init_hw_device"),
            OsString::from("vulkan=vk:0"),
            OsString::from("-filter_hw_device"),
            OsString::from("vk"),
        ],
    );

    // libplacebo consumes the Dolby Vision RPU side data and maps the result
    // to ordinary BT.709 SDR. Profile 5 has no HDR10-compatible base-layer
    // colors, so merely dropping its metadata produces a purple image.
    // Scale in the same Vulkan pass that tone-maps the frame. The previous
    // graph tone-mapped at source resolution, downloaded (often 4K) frames to
    // host memory, and only then scaled them on the CPU. Producing the bounded
    // browser resolution in libplacebo makes the Vulkan-to-host transfer and
    // the eventual encoder upload proportional to the requested output.
    let libplacebo = format!(
        concat!(
            "libplacebo=apply_dolbyvision=true:colorspace=bt709:",
            "color_primaries=bt709:color_trc=bt709:range=tv:",
            "tonemapping=bt.2390:format=yuv420p:",
            "w='min(iw,{})':h='min(ih,{})':",
            "force_original_aspect_ratio=decrease:force_divisible_by=2"
        ),
        quality.max_width(),
        quality.max_height()
    );
    let filter = match hardware_decode {
        HardwareDecode::Cuda => {
            format!("hwdownload,format=p010le,hwupload,{libplacebo},hwdownload,format=yuv420p")
        }
        HardwareDecode::None => {
            format!("format=yuv420p10le,hwupload,{libplacebo},hwdownload,format=yuv420p")
        }
    };
    if let Some(vf) = args.iter().position(|argument| argument == "-vf") {
        args[vf + 1] = OsString::from(&filter);
    } else {
        let video_codec = args
            .iter()
            .position(|argument| argument == "-c:v")
            .expect("ffmpeg browser video command must include an encoder");
        args.splice(
            video_codec..video_codec,
            [OsString::from("-vf"), OsString::from(&filter)],
        );
    }

    let output = args
        .len()
        .checked_sub(1)
        .expect("ffmpeg browser command must include an output");
    args.splice(
        output..output,
        [
            OsString::from("-color_primaries"),
            OsString::from("bt709"),
            OsString::from("-color_trc"),
            OsString::from("bt709"),
            OsString::from("-colorspace"),
            OsString::from("bt709"),
            OsString::from("-color_range"),
            OsString::from("tv"),
        ],
    );
}

fn apply_browser_ai_upscale(
    args: &mut Vec<OsString>,
    hardware_decode: HardwareDecode,
    quality: BrowserQuality,
) {
    let input = args
        .iter()
        .position(|argument| argument == "-i")
        .expect("ffmpeg browser command must include an input");
    args.splice(
        input..input,
        [
            OsString::from("-init_hw_device"),
            OsString::from("vulkan=vk:0"),
            OsString::from("-filter_hw_device"),
            OsString::from("vk"),
        ],
    );
    let libplacebo = format!(
        concat!(
            "libplacebo=custom_shader_path={}:format=yuv420p:",
            "w={}:h={}:force_original_aspect_ratio=decrease:",
            "force_divisible_by=2"
        ),
        BROWSER_AI_UPSCALE_SHADER_CHILD_PATH,
        quality.max_width(),
        quality.max_height()
    );
    let filter = match hardware_decode {
        HardwareDecode::Cuda => {
            // Browser AI upscale is admitted only for 8-bit input. CUDA
            // exposes those decoded frames as semiplanar NV12, and
            // hwdownload requires the software format to match the hardware
            // frame descriptor exactly. Asking it for planar yuv420p makes
            // FFmpeg reject the graph before the first frame. libplacebo can
            // consume the NV12 Vulkan upload directly and still emits the
            // browser encoder's required yuv420p output.
            format!("hwdownload,format=nv12,hwupload,{libplacebo},hwdownload,format=yuv420p")
        }
        HardwareDecode::None => {
            format!("format=yuv420p,hwupload,{libplacebo},hwdownload,format=yuv420p")
        }
    };
    if let Some(vf) = args.iter().position(|argument| argument == "-vf") {
        args[vf + 1] = OsString::from(filter);
    } else {
        let video_codec = args
            .iter()
            .position(|argument| argument == "-c:v")
            .expect("ffmpeg browser command must include an encoder");
        args.splice(
            video_codec..video_codec,
            [OsString::from("-vf"), OsString::from(filter)],
        );
    }
}

fn apply_browser_input_seek(
    args: &mut Vec<OsString>,
    start_seconds: usize,
    policy: BrowserSeekPolicy,
) {
    if start_seconds == 0 {
        return;
    }
    let mut input = args
        .iter()
        .position(|argument| argument == "-i")
        .expect("ffmpeg browser command must include an input");
    if policy == BrowserSeekPolicy::PreciselyTrimCopiedAudioPreroll {
        const DECODE_PREROLL_SECONDS: usize = 5;
        let preroll = start_seconds.min(DECODE_PREROLL_SECONDS);
        let fast_seek = start_seconds.saturating_sub(preroll);
        if fast_seek > 0 {
            args.splice(
                input..input,
                [OsString::from("-ss"), OsString::from(fast_seek.to_string())],
            );
            input += 2;
        }
        // An input-only seek keeps copied AAC from the demuxer's preceding
        // keyframe while an encoded video begins at the requested timestamp.
        // Decode a bounded lead, then apply one output-side trim to both
        // streams so their first retained packets share the same timeline.
        args.splice(
            input + 2..input + 2,
            [OsString::from("-ss"), OsString::from(preroll.to_string())],
        );
        return;
    }
    let mut seek = Vec::with_capacity(3);
    if policy == BrowserSeekPolicy::PreserveCopiedVideoPreroll {
        seek.push(OsString::from("-noaccurate_seek"));
    }
    seek.extend([
        OsString::from("-ss"),
        OsString::from(start_seconds.to_string()),
    ]);
    args.splice(input..input, seek);
}

fn apply_browser_hls_keyframes(args: &mut [OsString], hls: bool) {
    if !hls {
        return;
    }
    if let Some(index) = args
        .iter()
        .position(|argument| argument == "-force_key_frames")
    {
        if let Some(expression) = args.get_mut(index + 1) {
            *expression = OsString::from("expr:gte(t,n_forced*1)");
        }
    }
}

fn apply_browser_hls_pacing(
    args: &mut Vec<OsString>,
    hls: bool,
    encoding_stream: bool,
    readrate_catchup: bool,
) {
    if !hls || !encoding_stream {
        return;
    }
    let input = args
        .iter()
        .position(|argument| argument == "-i")
        .expect("ffmpeg browser command must include an input");
    let mut pacing = vec![
        OsString::from("-readrate"),
        OsString::from("1"),
        OsString::from("-readrate_initial_burst"),
        OsString::from("30"),
    ];
    if readrate_catchup {
        // FFmpeg 8 defaults catch-up to 1.05x. That turns the nominal initial
        // burst into real-time decoding, including keyframe preroll discarded
        // by an accurate input seek. Let startup run at the pipeline's natural
        // rate; readrate still takes over once the thirty-second lead is
        // filled. Older FFmpeg releases do not expose this option and retain
        // their original unbounded catch-up behavior without it.
        pacing.extend([OsString::from("-readrate_catchup"), OsString::from("100")]);
    }
    args.splice(input..input, pacing);
}

fn apply_browser_encoding_preset(
    args: &mut Vec<OsString>,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
) {
    if options.source_video.is_none()
        || plan.video_encoder == "copy"
        || options.encoding_preset == BrowserEncodingPreset::Balanced
    {
        return;
    }
    let nvenc = matches!(plan.video_encoder.as_str(), "h264_nvenc" | "hevc_nvenc");
    let fastest = options.encoding_preset == BrowserEncodingPreset::MaximumSpeed;
    // Balanced is byte-for-byte unchanged. Preserve the established filters,
    // HDR signaling, bitrate caps, IDRs and timestamp-repair path for every preset.
    let preset = match (nvenc, fastest) {
        (true, false) => "p4",
        (true, true) => "p2",
        (false, false) => "veryfast",
        (false, true) => "ultrafast",
    };
    let tune = if nvenc { "ll" } else { "zerolatency" };
    for (flag, value) in [("-preset", preset), ("-tune", tune), ("-bf", "0")] {
        if let Some(index) = args.iter().position(|arg| arg == flag) {
            if let Some(argument) = args.get_mut(index + 1) {
                *argument = value.into();
            }
        } else {
            let output = args.len().saturating_sub(1);
            args.splice(output..output, [flag.into(), value.into()]);
        }
    }
    if nvenc {
        let output = args.len().saturating_sub(1);
        args.splice(
            output..output,
            [
                "-rc-lookahead".into(),
                "0".into(),
                "-zerolatency".into(),
                "1".into(),
                "-delay".into(),
                "0".into(),
            ],
        );
    }
}

fn browser_ffmpeg_os_args_with_readrate_catchup(
    src_path: &Path,
    dst_path: &Path,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
    readrate_catchup: bool,
) -> Vec<OsString> {
    debug_assert_eq!(plan.action, RecodeAction::Browser);
    let policy = browser_output_policy(plan, options);
    let mut args = ffmpeg_grow_os_args(src_path, dst_path, plan);
    apply_browser_encoding_preset(&mut args, plan, options);
    if policy.apply_sdr_tonemap {
        apply_browser_sdr_tonemap(
            &mut args,
            plan.hardware_decode,
            plan.browser_quality.unwrap_or(BrowserQuality::Auto),
        );
    }
    if plan.browser_ai_upscale.is_some() {
        debug_assert_eq!(options.source_hdr, HdrKind::Sdr);
        debug_assert_ne!(plan.video_encoder, "copy");
        apply_browser_ai_upscale(
            &mut args,
            plan.hardware_decode,
            plan.browser_quality.unwrap_or(BrowserQuality::Auto),
        );
    }
    if policy.filter_copied_aac {
        let output = args.len().saturating_sub(1);
        args.splice(
            output..output,
            [OsString::from("-bsf:a"), OsString::from("aac_adtstoasc")],
        );
    }
    if policy.tag_hevc_as_hvc1 {
        let output = args.len().saturating_sub(1);
        args.splice(
            output..output,
            [OsString::from("-tag:v"), OsString::from("hvc1")],
        );
    }
    // Some Android hardware decoders require every separately appended movie
    // fragment to begin at a random-access point even though MSE retains one
    // continuous SourceBuffer timeline. Keep the one-second IDR cadence for
    // both native HLS and MSE fragmented delivery.
    apply_browser_hls_keyframes(&mut args, options.hls);
    // Fragment consumers retain a bounded playback window. Produce an
    // unrestricted initial buffer, then stay close to playback rate instead
    // of encoding the remainder of a feature film as fast as the machine can
    // run. Streams that copy both tracks remain unpaced because they use
    // negligible CPU and do not create encoded output ahead of playback.
    apply_browser_hls_pacing(
        &mut args,
        options.hls,
        plan.video_encoder != "copy" || plan.audio != AudioAction::Copy,
        readrate_catchup,
    );
    apply_browser_input_seek(&mut args, options.start_seconds, policy.seek);
    args
}

/// Build the complete browser-compatible FFmpeg command without exposing raw
/// flag mutation to the server orchestration layer.
///
/// Daemon request paths that retain a verified FFmpeg executable should use
/// [`browser_ffmpeg_os_args_for_verified_ffmpeg`] so older supported releases
/// do not receive options introduced in FFmpeg 8.
pub fn browser_ffmpeg_os_args(
    src_path: &Path,
    dst_path: &Path,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
) -> Vec<OsString> {
    browser_ffmpeg_os_args_with_readrate_catchup(src_path, dst_path, plan, options, true)
}

/// Build browser arguments for the exact FFmpeg executable retained by a
/// request-time transcode identity.
pub fn browser_ffmpeg_os_args_for_verified_ffmpeg(
    src_path: &Path,
    dst_path: &Path,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
    ffmpeg: &VerifiedExecutable,
) -> Vec<OsString> {
    browser_ffmpeg_os_args_with_readrate_catchup(
        src_path,
        dst_path,
        plan,
        options,
        ffmpeg.supports_readrate_catchup(),
    )
}

/// Whether this primary browser plan uses the SDR-tonemap cache revision.
/// Kept separate from fallback argv policy so existing cache identities stay
/// stable when a negotiated HEVC repair has a software fallback.
pub fn browser_cache_uses_sdr_tonemap_revision(
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
) -> bool {
    browser_output_policy(plan, options).apply_sdr_tonemap
}

fn live_frag_tail(out: &str) -> Vec<String> {
    vec![
        // Input-side seeks and copied B-frames can leave the first fragment
        // with negative DTS/PTS. Chrome may repeatedly present and discard
        // those frames, which looks like playback moving forward then back.
        "-avoid_negative_ts".into(),
        "make_zero".into(),
        "-flush_packets".into(),
        "1".into(),
        "-frag_duration".into(),
        "1000000".into(),
        "-f".into(),
        "mp4".into(),
        "-movflags".into(),
        // AC-3-in-MP4 cannot write its sample entry into an immediate empty
        // moov. Delaying that initialization until the first packets arrive
        // keeps the output streamable and works for copied/AAC audio too.
        "frag_keyframe+empty_moov+delay_moov+default_base_moof".into(),
        out.into(),
    ]
}

fn live_frag_os_tail(out: &OsStr) -> Vec<OsString> {
    let mut tail: Vec<OsString> = live_frag_tail("").into_iter().map(OsString::from).collect();
    if let Some(last) = tail.last_mut() {
        *last = out.to_os_string();
    }
    tail
}

/// Cache path for a specific source/plan/tool identity. Keeping the digest in
/// the filename prevents two incompatible plans for one DETAILS row from
/// overwriting or attaching to the same output.
pub fn cache_dest_for_key(
    cache_dir: &std::path::Path,
    detail_id: i64,
    action: RecodeAction,
    cache_key: &str,
) -> std::path::PathBuf {
    let tag = match action {
        RecodeAction::Hdr10 => "hdr10",
        RecodeAction::RemuxP8 => "remux",
        RecodeAction::AudioAc3 => "ac3",
        RecodeAction::Browser => "web",
        RecodeAction::Original => "orig",
    };
    if action == RecodeAction::Browser {
        // Browser keys retain readable policy revisions for diagnostics and
        // cache stamps. Hash the complete key for the basename so adding a
        // revision cannot exceed the filesystem's per-component name limit.
        let mut hasher = Sha256::new();
        hasher.update(cache_key.as_bytes());
        let filename_key = lowercase_hex(&hasher.finalize());
        return cache_dir.join(format!("{detail_id}-{tag}-{filename_key}.mp4"));
    }
    cache_dir.join(format!("{detail_id}-{tag}-{cache_key}.mp4"))
}

/// Whether a generated cache key has the filename-safe shape for `action`.
///
/// Non-browser keys remain exactly one SHA-256 digest. Browser keys may carry a
/// bounded lowercase revision suffix so cache maintenance can reclaim both
/// current and older generated browser outputs without knowing policy revision
/// strings owned by this crate.
pub fn cache_key_has_safe_shape(action: RecodeAction, cache_key: &str) -> bool {
    let Some(digest) = cache_key.as_bytes().get(..CACHE_DIGEST_HEX_BYTES) else {
        return false;
    };
    if !digest.iter().all(u8::is_ascii_hexdigit) {
        return false;
    }
    let suffix = &cache_key.as_bytes()[CACHE_DIGEST_HEX_BYTES..];
    if action != RecodeAction::Browser {
        return suffix.is_empty();
    }
    if suffix.is_empty() {
        return true;
    }
    cache_key.len() <= MAX_BROWSER_CACHE_KEY_BYTES
        && suffix.first() == Some(&b'-')
        && suffix.last().is_some_and(u8::is_ascii_alphanumeric)
        && suffix
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && !suffix.windows(2).any(|pair| pair == b"--")
}

/// In-progress remux. Only rename to `cache_dest` when ffmpeg exits 0.
pub fn cache_part(dest: &std::path::Path) -> std::path::PathBuf {
    let mut p = dest.as_os_str().to_os_string();
    p.push(".part");
    std::path::PathBuf::from(p)
}

pub fn cache_stamp_path(dest: &std::path::Path) -> std::path::PathBuf {
    let mut p = dest.as_os_str().to_os_string();
    p.push(".src");
    std::path::PathBuf::from(p)
}

/// Stable-enough source fingerprint without hashing an entire multi-gigabyte
/// movie. It covers the canonical path, nanosecond mtime, Unix device/inode,
/// length, and samples from the beginning, middle, and end of the file.
pub fn source_identity(src: &std::path::Path) -> Option<String> {
    let canonical = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let file = std::fs::File::open(&canonical).ok()?;
    source_identity_file(&file, &canonical)
}

/// Descriptor-backed source fingerprint. `identity_path` is the resolved path
/// recorded at authorization time; all metadata and samples come from `file`.
/// Sampling uses positioned reads and never changes the cursor shared by
/// `file` or any descriptor cloned from it. Each sample accepts the first
/// successful read, including a short read, to preserve the established cache
/// identity; only interrupted reads are retried.
pub fn source_identity_file(
    file: &std::fs::File,
    identity_path: &std::path::Path,
) -> Option<String> {
    use std::os::unix::fs::FileExt;
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()));
    let mut hasher = source_identity_hasher(
        identity_path,
        metadata.len(),
        modified,
        metadata.dev(),
        metadata.ino(),
    );

    const SAMPLE: u64 = 64 * 1024;
    let end = metadata.len().saturating_sub(SAMPLE);
    let middle = metadata.len().saturating_div(2).saturating_sub(SAMPLE / 2);
    let mut positions = vec![0, middle, end];
    positions.sort_unstable();
    positions.dedup();
    let mut buffer = vec![0u8; SAMPLE as usize];
    for position in positions {
        let read = read_positioned_sample(&mut buffer, position, |remaining, offset| {
            file.read_at(remaining, offset)
        })?;
        hash_source_identity_sample(&mut hasher, position, &buffer[..read]);
    }
    Some(lowercase_hex(&hasher.finalize()))
}

fn read_positioned_sample(
    buffer: &mut [u8],
    position: u64,
    mut read_at: impl FnMut(&mut [u8], u64) -> std::io::Result<usize>,
) -> Option<usize> {
    const MAX_CONSECUTIVE_INTERRUPTS: usize = 16;
    let mut interrupted = 0usize;
    loop {
        match read_at(buffer, position) {
            Ok(read) => return Some(read),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                interrupted = interrupted.checked_add(1)?;
                if interrupted > MAX_CONSECUTIVE_INTERRUPTS {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

fn source_identity_hasher(
    identity_path: &std::path::Path,
    len: u64,
    modified: Option<(u64, u32)>,
    device: u64,
    inode: u64,
) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(identity_path.as_os_str().as_encoded_bytes());
    hasher.update(len.to_le_bytes());
    if let Some((seconds, nanoseconds)) = modified {
        hasher.update(seconds.to_le_bytes());
        hasher.update(nanoseconds.to_le_bytes());
    }
    hasher.update(device.to_le_bytes());
    hasher.update(inode.to_le_bytes());
    hasher
}

fn hash_source_identity_sample(hasher: &mut Sha256, position: u64, bytes: &[u8]) {
    hasher.update(position.to_le_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolFileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl ToolFileIdentity {
    fn same_open_file_contents(&self, other: &Self) -> bool {
        self.len == other.len && self.modified == other.modified && {
            #[cfg(unix)]
            {
                self.device == other.device && self.inode == other.inode
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ToolVersionFlavor {
    Ffmpeg,
    Ffprobe,
    Dovi,
}

impl ToolVersionFlavor {
    const fn version_arg(self) -> &'static str {
        match self {
            Self::Ffmpeg | Self::Ffprobe => "-version",
            Self::Dovi => "--version",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Ffmpeg => "ffmpeg",
            Self::Ffprobe => "ffprobe",
            Self::Dovi => "dovi_tool",
        }
    }
}

/// One executable whose path, file identity, and reported version were
/// verified together. Fields are private so callers cannot manufacture a
/// trusted snapshot without running the bounded query path.
#[derive(Clone, Debug)]
pub struct VerifiedExecutable {
    path: std::path::PathBuf,
    identity: ToolFileIdentity,
    fingerprint: String,
    file: std::sync::Arc<std::fs::File>,
}

impl PartialEq for VerifiedExecutable {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.identity == other.identity
            && self.fingerprint == other.fingerprint
    }
}

impl Eq for VerifiedExecutable {}

impl VerifiedExecutable {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Whether this FFmpeg release accepts the input catch-up pacing option.
    ///
    /// The option first shipped in FFmpeg 8. Unknown development/vendor
    /// version formats conservatively omit this optional tuning flag.
    pub fn supports_readrate_catchup(&self) -> bool {
        ffmpeg_major_version(&self.fingerprint).is_some_and(|major| major >= 8)
    }

    /// Recheck content-relevant metadata on the retained inode immediately
    /// before it is handed to a child process.
    pub fn verify_for_execution(&self) -> Result<(), String> {
        self.verify_current("executable")
    }

    /// Build a command that executes the already-open executable inode rather
    /// than resolving its pathname again in the child.
    pub fn command(&self) -> std::process::Command {
        use std::os::unix::process::CommandExt;

        let mut command = std::process::Command::new(VERIFIED_EXECUTABLE_CHILD_PATH);
        command.arg0(&self.path);
        command
    }

    /// Add the pinned executable descriptor to a supervised child.
    pub fn inherit_for_execution<'a>(
        &self,
        runner: rusty_dlna_helper::SupervisedCommand<'a>,
    ) -> std::io::Result<rusty_dlna_helper::SupervisedCommand<'a>> {
        self.verify_for_execution()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        runner.inherit_file_at(&self.file, VERIFIED_EXECUTABLE_FD)
    }

    fn verify_current(&self, label: &str) -> Result<(), String> {
        let current = tool_file_identity_file(&self.file)
            .ok_or_else(|| format!("snapshotted {label} executable is no longer accessible"))?;
        // Renaming a replacement over the original path changes the retained
        // inode's ctime/link count without changing its bytes. Keep ctime in
        // cache fingerprints, but do not reject that safe pinned-inode case.
        if !current.same_open_file_contents(&self.identity) {
            return Err(format!(
                "snapshotted {label} executable changed before execution"
            ));
        }
        Ok(())
    }
}

fn ffmpeg_major_version(fingerprint: &str) -> Option<u32> {
    let version = fingerprint
        .split_once('|')
        .map_or(fingerprint, |(version, _)| version)
        .strip_prefix("ffmpeg version ")?
        .split_whitespace()
        .next()?
        .trim_start_matches(['n', 'N']);
    let digits = version
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    (digits > 0)
        .then(|| version[..digits].parse().ok())
        .flatten()
}

/// Inode-pinned tools used by one Profile-8 cache identity and producer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile8ToolchainSnapshot {
    ffmpeg: VerifiedExecutable,
    ffprobe: VerifiedExecutable,
    dovi_tool: VerifiedExecutable,
}

impl Profile8ToolchainSnapshot {
    pub fn ffmpeg(&self) -> &VerifiedExecutable {
        &self.ffmpeg
    }

    pub fn ffprobe(&self) -> &VerifiedExecutable {
        &self.ffprobe
    }

    pub fn dovi_tool(&self) -> &VerifiedExecutable {
        &self.dovi_tool
    }

    fn verify_current(&self) -> Result<(), String> {
        self.ffmpeg.verify_current("ffmpeg")?;
        self.ffprobe.verify_current("ffprobe")?;
        self.dovi_tool.verify_current("dovi_tool")
    }

    fn query_with_ffmpeg(
        ffmpeg: VerifiedExecutable,
        control: Option<ToolQueryControl<'_>>,
    ) -> Result<Self, ToolQueryError> {
        let dovi = dovi_tool_path().ok_or_else(|| ToolQueryError::Query {
            executable: std::path::PathBuf::from("dovi_tool"),
            message: "not found on PATH".into(),
        })?;
        Ok(Self {
            ffmpeg,
            ffprobe: tool_snapshot(
                std::path::Path::new("ffprobe"),
                ToolVersionFlavor::Ffprobe,
                control,
            )?,
            dovi_tool: tool_snapshot(&dovi, ToolVersionFlavor::Dovi, control)?,
        })
    }

    #[cfg(test)]
    fn query_paths(
        ffmpeg: &std::path::Path,
        ffprobe: &std::path::Path,
        dovi_tool: &std::path::Path,
        control: Option<ToolQueryControl<'_>>,
    ) -> Result<Self, ToolQueryError> {
        Ok(Self {
            ffmpeg: tool_snapshot(ffmpeg, ToolVersionFlavor::Ffmpeg, control)?,
            ffprobe: tool_snapshot(ffprobe, ToolVersionFlavor::Ffprobe, control)?,
            dovi_tool: tool_snapshot(dovi_tool, ToolVersionFlavor::Dovi, control)?,
        })
    }

    fn query_under_remux_control(control: &mut RemuxP8Control<'_>) -> Result<Self, RemuxP8Error> {
        control.check("Profile-8 tool query")?;
        let dovi = dovi_tool_path().ok_or_else(|| {
            RemuxP8Error::Pipeline(
                "tool version query failed for dovi_tool: not found on PATH".into(),
            )
        })?;
        Self::query_paths_under_remux_control(
            std::path::Path::new("ffmpeg"),
            std::path::Path::new("ffprobe"),
            &dovi,
            control,
        )
    }

    fn query_paths_under_remux_control(
        ffmpeg: &std::path::Path,
        ffprobe: &std::path::Path,
        dovi_tool: &std::path::Path,
        control: &mut RemuxP8Control<'_>,
    ) -> Result<Self, RemuxP8Error> {
        Ok(Self {
            ffmpeg: tool_snapshot_under_remux_control(ffmpeg, ToolVersionFlavor::Ffmpeg, control)?,
            ffprobe: tool_snapshot_under_remux_control(
                ffprobe,
                ToolVersionFlavor::Ffprobe,
                control,
            )?,
            dovi_tool: tool_snapshot_under_remux_control(
                dovi_tool,
                ToolVersionFlavor::Dovi,
                control,
            )?,
        })
    }

    #[cfg(test)]
    fn cache_signature(&self) -> String {
        format!(
            "revision={PROFILE8_TOOLCHAIN_CACHE_REVISION}\nffmpeg={}\nffprobe={}\ndovi={}",
            self.ffmpeg.fingerprint, self.ffprobe.fingerprint, self.dovi_tool.fingerprint
        )
    }
}

type ToolVersionCache = std::sync::Mutex<
    std::collections::HashMap<(ToolVersionFlavor, std::path::PathBuf), (ToolFileIdentity, String)>,
>;

/// Daemon-owned admission and cancellation for request-time tool queries.
#[derive(Clone, Copy, Debug)]
pub struct ToolQueryControl<'a> {
    helpers: &'a std::sync::Arc<rusty_dlna_helper::HelperGate>,
    cancellation: &'a rusty_dlna_helper::CancellationToken,
    admission_timeout: std::time::Duration,
    query_timeout: std::time::Duration,
}

impl<'a> ToolQueryControl<'a> {
    pub fn new(
        helpers: &'a std::sync::Arc<rusty_dlna_helper::HelperGate>,
        cancellation: &'a rusty_dlna_helper::CancellationToken,
        admission_timeout: std::time::Duration,
    ) -> Self {
        Self {
            helpers,
            cancellation,
            admission_timeout,
            query_timeout: std::time::Duration::from_secs(10),
        }
    }

    /// Override the hard version-query deadline.
    pub const fn query_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.query_timeout = timeout;
        self
    }
}

/// Failure to obtain a trustworthy request-time tool fingerprint.
#[derive(Debug)]
pub enum ToolQueryError {
    Busy(rusty_dlna_helper::HelperAdmissionError),
    Cancelled,
    Deadline {
        executable: std::path::PathBuf,
    },
    Query {
        executable: std::path::PathBuf,
        message: String,
    },
}

impl std::fmt::Display for ToolQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(error) => write!(formatter, "media helper busy: {error}"),
            Self::Cancelled => formatter.write_str("tool version query cancelled"),
            Self::Deadline { executable } => {
                write!(
                    formatter,
                    "tool version query timed out: {}",
                    executable.display()
                )
            }
            Self::Query {
                executable,
                message,
            } => write!(
                formatter,
                "tool version query failed for {}: {message}",
                executable.display()
            ),
        }
    }
}

impl std::error::Error for ToolQueryError {}

fn tool_file_identity_file(file: &std::fs::File) -> Option<ToolFileIdentity> {
    tool_file_identity_from_metadata(file.metadata().ok()?)
}

#[cfg(test)]
fn tool_file_identity(path: &std::path::Path) -> Option<ToolFileIdentity> {
    tool_file_identity_from_metadata(path.metadata().ok()?)
}

fn tool_file_identity_from_metadata(metadata: std::fs::Metadata) -> Option<ToolFileIdentity> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(ToolFileIdentity {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        changed_seconds: metadata.ctime(),
        #[cfg(unix)]
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

fn resolve_tool_path(executable: &std::path::Path) -> std::path::PathBuf {
    let search_path = std::env::var_os("PATH");
    resolve_tool_path_with_search(executable, search_path.as_deref())
}

fn resolve_tool_path_with_search(
    executable: &std::path::Path,
    search_path: Option<&std::ffi::OsStr>,
) -> std::path::PathBuf {
    let candidate = if executable.components().count() > 1 {
        executable.to_path_buf()
    } else {
        search_path
            .and_then(|path| {
                std::env::split_paths(path)
                    .map(|dir| dir.join(executable))
                    .find(|candidate| candidate.is_file())
            })
            .unwrap_or_else(|| executable.to_path_buf())
    };
    std::fs::canonicalize(&candidate).unwrap_or(candidate)
}

fn tool_version_cache() -> &'static ToolVersionCache {
    static VERSIONS: std::sync::OnceLock<ToolVersionCache> = std::sync::OnceLock::new();
    VERSIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn cached_tool_fingerprint(
    flavor: ToolVersionFlavor,
    resolved: &std::path::Path,
    identity: &ToolFileIdentity,
) -> Option<String> {
    tool_version_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(flavor, resolved.to_path_buf()))
        .filter(|(cached_identity, _)| cached_identity == identity)
        .map(|(_, fingerprint)| fingerprint.clone())
}

fn cache_tool_fingerprint(
    flavor: ToolVersionFlavor,
    resolved: std::path::PathBuf,
    identity: ToolFileIdentity,
    fingerprint: String,
) {
    tool_version_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert((flavor, resolved), (identity, fingerprint));
}

fn tool_query_lock(
    control: Option<ToolQueryControl<'_>>,
    admission_deadline: Option<std::time::Instant>,
) -> Result<std::sync::MutexGuard<'static, ()>, ToolQueryError> {
    let query = tool_query_mutex();
    let Some(control) = control else {
        return Ok(query
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()));
    };
    loop {
        if control.cancellation.is_cancelled() {
            return Err(ToolQueryError::Cancelled);
        }
        match query.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                return Ok(poisoned.into_inner());
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let remaining = admission_deadline
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(ToolQueryError::Busy(
                rusty_dlna_helper::HelperAdmissionError::TimedOut,
            ));
        }
        std::thread::sleep(
            remaining
                .unwrap_or(std::time::Duration::from_millis(20))
                .min(std::time::Duration::from_millis(20)),
        );
    }
}

fn tool_query_mutex() -> &'static std::sync::Mutex<()> {
    static QUERY: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    QUERY.get_or_init(|| std::sync::Mutex::new(()))
}

fn tool_identity_fingerprint(
    flavor: ToolVersionFlavor,
    resolved: &std::path::Path,
    identity: &ToolFileIdentity,
    version: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(flavor.label().as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(resolved.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(resolved.to_string_lossy().as_bytes());
    hasher.update(identity.len.to_le_bytes());
    if let Some(modified) = identity.modified {
        match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => {
                hasher.update([1]);
                hasher.update(duration.as_secs().to_le_bytes());
                hasher.update(duration.subsec_nanos().to_le_bytes());
            }
            Err(error) => {
                hasher.update([2]);
                hasher.update(error.duration().as_secs().to_le_bytes());
                hasher.update(error.duration().subsec_nanos().to_le_bytes());
            }
        }
    } else {
        hasher.update([0]);
    }
    #[cfg(unix)]
    {
        hasher.update(identity.device.to_le_bytes());
        hasher.update(identity.inode.to_le_bytes());
        hasher.update(identity.changed_seconds.to_le_bytes());
        hasher.update(identity.changed_nanoseconds.to_le_bytes());
    }
    hasher.update(version.as_bytes());
    format!("{version}|{}", lowercase_hex(&hasher.finalize()))
}

fn open_tool_executable(
    resolved: &std::path::Path,
    message: &'static str,
) -> Result<(std::sync::Arc<std::fs::File>, ToolFileIdentity), ToolQueryError> {
    let file = std::fs::File::open(resolved).map_err(|error| ToolQueryError::Query {
        executable: resolved.to_path_buf(),
        message: format!("{message}: {error}"),
    })?;
    let identity = tool_file_identity_file(&file).ok_or_else(|| ToolQueryError::Query {
        executable: resolved.to_path_buf(),
        message: message.into(),
    })?;
    Ok((std::sync::Arc::new(file), identity))
}

fn tool_snapshot(
    executable: &std::path::Path,
    flavor: ToolVersionFlavor,
    control: Option<ToolQueryControl<'_>>,
) -> Result<VerifiedExecutable, ToolQueryError> {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome,
    };
    use std::ops::ControlFlow;

    if control.is_some_and(|control| control.cancellation.is_cancelled()) {
        return Err(ToolQueryError::Cancelled);
    }
    let mut resolved = resolve_tool_path(executable);
    let (mut executable_file, mut identity) =
        open_tool_executable(&resolved, "cannot fingerprint executable")?;
    if let Some(fingerprint) = cached_tool_fingerprint(flavor, &resolved, &identity) {
        return Ok(VerifiedExecutable {
            path: resolved,
            identity,
            fingerprint,
            file: executable_file,
        });
    }
    let admission_deadline = control
        .and_then(|control| std::time::Instant::now().checked_add(control.admission_timeout));
    let _query_guard = tool_query_lock(control, admission_deadline)?;
    if control.is_some_and(|control| control.cancellation.is_cancelled()) {
        return Err(ToolQueryError::Cancelled);
    }
    resolved = resolve_tool_path(executable);
    (executable_file, identity) = open_tool_executable(&resolved, "cannot fingerprint executable")?;
    if let Some(fingerprint) = cached_tool_fingerprint(flavor, &resolved, &identity) {
        return Ok(VerifiedExecutable {
            path: resolved,
            identity,
            fingerprint,
            file: executable_file,
        });
    }
    let _helper_permit = if let Some(control) = control {
        let remaining = admission_deadline
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(control.admission_timeout);
        Some(
            control
                .helpers
                .acquire_timeout_cancelled(remaining, control.cancellation)
                .map_err(|error| match error {
                    rusty_dlna_helper::HelperAdmissionError::Cancelled => ToolQueryError::Cancelled,
                    error => ToolQueryError::Busy(error),
                })?,
        )
    } else {
        None
    };
    if control.is_some_and(|control| control.cancellation.is_cancelled()) {
        return Err(ToolQueryError::Cancelled);
    }
    let admitted_resolved = resolve_tool_path(executable);
    let (admitted_file, admitted_identity) = open_tool_executable(
        &admitted_resolved,
        "cannot fingerprint executable after admission",
    )?;
    resolved = admitted_resolved;
    executable_file = admitted_file;
    identity = admitted_identity;
    if let Some(fingerprint) = cached_tool_fingerprint(flavor, &resolved, &identity) {
        return Ok(VerifiedExecutable {
            path: resolved,
            identity,
            fingerprint,
            file: executable_file,
        });
    }
    let mut command = std::process::Command::new(VERIFIED_EXECUTABLE_CHILD_PATH);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.arg0(&resolved);
    }
    command.arg(flavor.version_arg());
    let stdout_capture = CaptureConfig::new(64 * 1024, CaptureRetention::Head);
    let stderr_capture = CaptureConfig::new(64 * 1024, CaptureRetention::Tail);
    let query_timeout = control
        .map(|control| control.query_timeout)
        .unwrap_or(std::time::Duration::from_secs(10));
    let deadline = std::time::Instant::now()
        .checked_add(query_timeout)
        .ok_or_else(|| ToolQueryError::Query {
            executable: resolved.clone(),
            message: "query timeout is too large".into(),
        })?;
    let version_runner = SupervisedCommand::new(&mut command)
        .capture_stdout(stdout_capture)
        .capture_stderr(stderr_capture);
    let version_runner = version_runner
        .inherit_file_at(&executable_file, VERIFIED_EXECUTABLE_FD)
        .map_err(|error| ToolQueryError::Query {
            executable: resolved.clone(),
            message: format!("cannot inherit executable descriptor: {error}"),
        })?;
    let version =
        match version_runner.run_until(deadline, std::time::Duration::from_millis(20), || {
            if control.is_some_and(|control| control.cancellation.is_cancelled()) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }) {
            Ok(SupervisedOutcome::Exited(output)) if output.status.success() => {
                let text = if output.stdout.is_empty() {
                    String::from_utf8_lossy(&output.stderr)
                } else {
                    String::from_utf8_lossy(&output.stdout)
                };
                text.lines()
                    .find(|line| !line.trim().is_empty())
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| ToolQueryError::Query {
                        executable: resolved.clone(),
                        message: "successful version query returned no version".into(),
                    })?
            }
            Ok(SupervisedOutcome::Exited(output)) => {
                let diagnostics = if output.stderr.is_empty() {
                    String::from_utf8_lossy(&output.stdout)
                } else {
                    String::from_utf8_lossy(&output.stderr)
                };
                return Err(ToolQueryError::Query {
                    executable: resolved,
                    message: if diagnostics.trim().is_empty() {
                        format!("exited {}", output.status)
                    } else {
                        format!("exited {}: {}", output.status, diagnostics.trim())
                    },
                });
            }
            Ok(SupervisedOutcome::Deadline { .. }) => {
                return Err(ToolQueryError::Deadline {
                    executable: resolved,
                });
            }
            Ok(SupervisedOutcome::NotStarted { .. } | SupervisedOutcome::Stopped { .. }) => {
                return Err(ToolQueryError::Cancelled);
            }
            Err(error) => {
                return Err(ToolQueryError::Query {
                    executable: resolved,
                    message: error.to_string(),
                });
            }
        };
    if control.is_some_and(|control| control.cancellation.is_cancelled()) {
        return Err(ToolQueryError::Cancelled);
    }
    let final_identity =
        tool_file_identity_file(&executable_file).ok_or_else(|| ToolQueryError::Query {
            executable: resolved.clone(),
            message: "cannot fingerprint executable after version query".into(),
        })?;
    if final_identity != identity {
        return Err(ToolQueryError::Query {
            executable: resolved,
            message: "executable changed during version query".into(),
        });
    }
    let fingerprint = tool_identity_fingerprint(flavor, &resolved, &identity, &version);
    cache_tool_fingerprint(
        flavor,
        resolved.clone(),
        identity.clone(),
        fingerprint.clone(),
    );
    Ok(VerifiedExecutable {
        path: resolved,
        identity,
        fingerprint,
        file: executable_file,
    })
}

fn tool_snapshot_under_remux_control(
    executable: &std::path::Path,
    flavor: ToolVersionFlavor,
    control: &mut RemuxP8Control<'_>,
) -> Result<VerifiedExecutable, RemuxP8Error> {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome,
    };
    use std::ops::ControlFlow;

    let operation = format!("{} version query", flavor.label());
    control.check(&operation)?;
    let mut resolved = resolve_tool_path(executable);
    let (mut executable_file, mut identity) =
        open_tool_executable(&resolved, "cannot fingerprint executable")
            .map_err(|error| RemuxP8Error::Pipeline(error.to_string()))?;
    if let Some(fingerprint) = cached_tool_fingerprint(flavor, &resolved, &identity) {
        return Ok(VerifiedExecutable {
            path: resolved,
            identity,
            fingerprint,
            file: executable_file,
        });
    }

    let query = tool_query_mutex();
    let _query_guard = loop {
        control.check(&operation)?;
        match query.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    control.check(&operation)?;
    resolved = resolve_tool_path(executable);
    (executable_file, identity) = open_tool_executable(&resolved, "cannot fingerprint executable")
        .map_err(|error| RemuxP8Error::Pipeline(error.to_string()))?;
    if let Some(fingerprint) = cached_tool_fingerprint(flavor, &resolved, &identity) {
        return Ok(VerifiedExecutable {
            path: resolved,
            identity,
            fingerprint,
            file: executable_file,
        });
    }

    let provisional = VerifiedExecutable {
        path: resolved.clone(),
        identity: identity.clone(),
        fingerprint: String::new(),
        file: executable_file.clone(),
    };
    let mut command = provisional.command();
    command.arg(flavor.version_arg());
    let runner = SupervisedCommand::new(&mut command)
        .capture_stdout(CaptureConfig::new(64 * 1024, CaptureRetention::Head))
        .capture_stderr(CaptureConfig::new(64 * 1024, CaptureRetention::Tail));
    let runner = provisional.inherit_for_execution(runner).map_err(|error| {
        RemuxP8Error::Pipeline(format!(
            "inherit executable for {}: {error}",
            resolved.display()
        ))
    })?;
    enum Stop {
        Cancelled,
        Deadline,
        Observer(String),
    }
    let output = match runner.run_until(
        control.deadline,
        std::time::Duration::from_millis(20),
        || match control.check(&operation) {
            Ok(()) => ControlFlow::Continue(()),
            Err(RemuxP8ControlStop::Cancelled(_)) => ControlFlow::Break(Stop::Cancelled),
            Err(RemuxP8ControlStop::Deadline(_)) => ControlFlow::Break(Stop::Deadline),
            Err(RemuxP8ControlStop::Observer(error)) => ControlFlow::Break(Stop::Observer(error)),
        },
    ) {
        Ok(SupervisedOutcome::Exited(output)) if output.status.success() => output,
        Ok(SupervisedOutcome::Exited(output)) => {
            let diagnostics = if output.stderr.is_empty() {
                String::from_utf8_lossy(&output.stdout)
            } else {
                String::from_utf8_lossy(&output.stderr)
            };
            return Err(RemuxP8Error::Pipeline(format!(
                "tool version query failed for {}: exited {}: {}",
                resolved.display(),
                output.status,
                diagnostics.trim()
            )));
        }
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Cancelled,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Cancelled,
                ..
            },
        ) => return Err(RemuxP8Error::Cancelled(format!("{operation} cancelled"))),
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Deadline,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Deadline,
                ..
            }
            | SupervisedOutcome::Deadline { .. },
        ) => {
            return Err(RemuxP8Error::Deadline(format!("{operation} timed out")));
        }
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Observer(error),
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Observer(error),
                ..
            },
        ) => return Err(RemuxP8Error::Observer(error)),
        Err(error) => {
            return Err(RemuxP8Error::Pipeline(format!(
                "tool version query failed for {}: {error}",
                resolved.display()
            )));
        }
    };
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    let version = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .ok_or_else(|| {
            RemuxP8Error::Pipeline(format!(
                "tool version query failed for {}: successful query returned no version",
                resolved.display()
            ))
        })?;
    control.check(&operation)?;
    let final_identity = tool_file_identity_file(&executable_file).ok_or_else(|| {
        RemuxP8Error::Pipeline(format!(
            "tool version query failed for {}: cannot fingerprint executable after version query",
            resolved.display()
        ))
    })?;
    if final_identity != identity {
        return Err(RemuxP8Error::Pipeline(format!(
            "tool version query failed for {}: executable changed during version query",
            resolved.display()
        )));
    }
    let fingerprint = tool_identity_fingerprint(flavor, &resolved, &identity, version);
    cache_tool_fingerprint(
        flavor,
        resolved.clone(),
        identity.clone(),
        fingerprint.clone(),
    );
    Ok(VerifiedExecutable {
        path: resolved,
        identity,
        fingerprint,
        file: executable_file,
    })
}

fn tool_version(
    executable: &std::path::Path,
    flavor: ToolVersionFlavor,
    control: Option<ToolQueryControl<'_>>,
) -> Result<String, ToolQueryError> {
    tool_snapshot(executable, flavor, control).map(|snapshot| snapshot.fingerprint)
}

fn tool_version_for_cache_key(
    executable: &std::path::Path,
    flavor: ToolVersionFlavor,
    control: Option<ToolQueryControl<'_>>,
) -> Result<String, ToolQueryError> {
    match tool_version(executable, flavor, control) {
        Ok(fingerprint) => Ok(fingerprint),
        Err(error) if control.is_none() => Ok(format!("unavailable:{error}")),
        Err(error) => Err(error),
    }
}

/// Cache digest plus the verified toolchain that must produce those bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscodeCacheIdentity {
    base_cache_key: String,
    cache_key: String,
    ffmpeg: VerifiedExecutable,
    profile8_toolchain: Option<Profile8ToolchainSnapshot>,
}

impl TranscodeCacheIdentity {
    pub fn cache_key(&self) -> &str {
        &self.cache_key
    }

    /// Exact ffmpeg executable fingerprinted into this identity.
    pub fn ffmpeg(&self) -> &VerifiedExecutable {
        &self.ffmpeg
    }

    pub fn profile8_toolchain(&self) -> Option<&Profile8ToolchainSnapshot> {
        self.profile8_toolchain.as_ref()
    }

    /// Reuse the already sampled source and verified toolchain for another
    /// browser output generation whose only request-specific traits are in
    /// `options` (for example, a new seek offset).
    pub fn with_browser_options(
        &self,
        plan: &TranscodePlan,
        options: BrowserOutputOptions,
    ) -> Self {
        debug_assert_eq!(plan.action, RecodeAction::Browser);
        let mut identity = self.clone();
        identity.cache_key =
            browser_cache_key_from_base(identity.base_cache_key.clone(), plan, options);
        identity
    }
}

/// Digest of everything that can materially change cached output.
pub fn transcode_cache_key(
    src: &std::path::Path,
    plan: &TranscodePlan,
    remux_p8: bool,
) -> Option<String> {
    let source = source_identity(src)?;
    transcode_cache_key_from_identity(&source, plan, remux_p8, None).ok()
}

pub fn transcode_cache_key_file(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    remux_p8: bool,
) -> Option<String> {
    let source = source_identity_file(file, identity_path)?;
    transcode_cache_key_from_identity(&source, plan, remux_p8, None).ok()
}

/// Controlled file-based cache identity for daemon request paths.
pub fn transcode_cache_key_file_controlled(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    remux_p8: bool,
    control: ToolQueryControl<'_>,
) -> Result<Option<String>, ToolQueryError> {
    transcode_cache_identity_file_controlled(file, identity_path, plan, remux_p8, control)
        .map(|identity| identity.map(|identity| identity.cache_key))
}

/// Controlled daemon identity retaining the exact verified tools that must be
/// handed to the eventual output-producing job.
pub fn transcode_cache_identity_file_controlled(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    remux_p8: bool,
    control: ToolQueryControl<'_>,
) -> Result<Option<TranscodeCacheIdentity>, ToolQueryError> {
    transcode_cache_identity_file_controlled_with_ffmpeg(
        file,
        identity_path,
        plan,
        remux_p8,
        std::path::Path::new("ffmpeg"),
        control,
    )
}

fn transcode_cache_identity_file_controlled_with_ffmpeg(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    remux_p8: bool,
    ffmpeg_path: &std::path::Path,
    control: ToolQueryControl<'_>,
) -> Result<Option<TranscodeCacheIdentity>, ToolQueryError> {
    let Some(source) = source_identity_file(file, identity_path) else {
        return Ok(None);
    };
    let ffmpeg = tool_snapshot(ffmpeg_path, ToolVersionFlavor::Ffmpeg, Some(control))?;
    let profile8_toolchain = if remux_p8 {
        Some(Profile8ToolchainSnapshot::query_with_ffmpeg(
            ffmpeg.clone(),
            Some(control),
        )?)
    } else {
        None
    };
    let cache_key = transcode_cache_key_with_tools(
        &source,
        plan,
        remux_p8,
        ffmpeg.fingerprint(),
        profile8_toolchain
            .as_ref()
            .map(|toolchain| toolchain.ffprobe.fingerprint.as_str()),
        profile8_toolchain
            .as_ref()
            .map(|toolchain| toolchain.dovi_tool.fingerprint.as_str()),
    );
    Ok(Some(TranscodeCacheIdentity {
        base_cache_key: cache_key.clone(),
        cache_key,
        ffmpeg,
        profile8_toolchain,
    }))
}

fn browser_cache_key_from_base(
    mut cache_key: String,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
) -> String {
    debug_assert_eq!(plan.action, RecodeAction::Browser);
    let policy = browser_output_policy(plan, options);
    if options.source_video.is_some()
        && plan.video_encoder != "copy"
        && options.encoding_preset != BrowserEncodingPreset::Balanced
    {
        cache_key.push_str("-browser-encoding-v1-");
        cache_key.push_str(options.encoding_preset.id());
    }
    cache_key.push('-');
    cache_key.push_str(BROWSER_TIMELINE_CACHE_REVISION);
    cache_key.push('-');
    cache_key.push_str(BROWSER_CHAPTER_MAP_CACHE_REVISION);
    if browser_cache_uses_sdr_tonemap_revision(plan, options) {
        cache_key.push('-');
        cache_key.push_str(SDR_TONEMAP_CACHE_REVISION);
    }
    if policy.source_requires_sdr_tonemap {
        cache_key.push('-');
        cache_key.push_str(BROWSER_HDR_SOURCE_CACHE_REVISION);
    }
    if policy.filter_copied_aac {
        cache_key.push('-');
        cache_key.push_str(BROWSER_AAC_FILTER_CACHE_REVISION);
    }
    if policy.tag_hevc_as_hvc1 {
        cache_key.push('-');
        cache_key.push_str(BROWSER_HEVC_TAG_CACHE_REVISION);
    }
    if policy.seek != BrowserSeekPolicy::Accurate {
        cache_key.push('-');
        cache_key.push_str(BROWSER_MIXED_COPY_SEEK_CACHE_REVISION);
    }
    if plan.hardware_decode == HardwareDecode::Cuda && plan.video_encoder != "copy" {
        cache_key.push('-');
        cache_key.push_str(BROWSER_CUDA_DOWNLOAD_CACHE_REVISION);
    }
    if plan.browser_quality == Some(BrowserQuality::Auto)
        && matches!(plan.video_encoder.as_str(), "h264_nvenc" | "libx264")
    {
        cache_key.push('-');
        cache_key.push_str(BROWSER_ADAPTIVE_H264_LEVEL_CACHE_REVISION);
    }
    if matches!(plan.video_encoder.as_str(), "h264_nvenc" | "hevc_nvenc") {
        cache_key.push('-');
        cache_key.push_str(BROWSER_NVENC_IDR_CACHE_REVISION);
    }
    if plan
        .browser_quality
        .is_some_and(BrowserQuality::mobile_safe_h264)
        && matches!(plan.video_encoder.as_str(), "h264_nvenc" | "libx264")
    {
        cache_key.push('-');
        cache_key.push_str(BROWSER_DATA_SAVER_BASELINE_CACHE_REVISION);
    }
    if options.hls {
        cache_key.push('-');
        cache_key.push_str(BROWSER_HLS_CACHE_REVISION);
    }
    if let Some(upscale) = &plan.browser_ai_upscale {
        let mut identity = Sha256::new();
        identity.update(upscale.model.as_bytes());
        identity.update([0]);
        identity.update(upscale.shader_sha256.as_bytes());
        cache_key.push('-');
        cache_key.push_str(BROWSER_AI_UPSCALE_CACHE_REVISION);
        cache_key.push('-');
        cache_key.push_str(&lowercase_hex(&identity.finalize()));
    }
    if options.start_seconds > 0 {
        cache_key.push_str(&format!("-start-{}", options.start_seconds));
    }
    cache_key
}

/// Browser cache identity with timeline and SDR-tonemap policy revisions.
pub fn browser_transcode_cache_key_file(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
) -> Option<String> {
    let cache_key = transcode_cache_key_file(file, identity_path, plan, false)?;
    Some(browser_cache_key_from_base(cache_key, plan, options))
}

/// Controlled browser cache identity for daemon request paths.
pub fn browser_transcode_cache_key_file_controlled(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
    control: ToolQueryControl<'_>,
) -> Result<Option<String>, ToolQueryError> {
    browser_transcode_cache_identity_file_controlled(file, identity_path, plan, options, control)
        .map(|identity| identity.map(|identity| identity.cache_key))
}

/// Controlled browser cache identity retaining the exact verified ffmpeg that
/// must produce the cache entry.
pub fn browser_transcode_cache_identity_file_controlled(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    options: BrowserOutputOptions,
    control: ToolQueryControl<'_>,
) -> Result<Option<TranscodeCacheIdentity>, ToolQueryError> {
    transcode_cache_identity_file_controlled(file, identity_path, plan, false, control)
        .map(|identity| identity.map(|identity| identity.with_browser_options(plan, options)))
}

fn transcode_cache_key_from_identity(
    source: &str,
    plan: &TranscodePlan,
    remux_p8: bool,
    control: Option<ToolQueryControl<'_>>,
) -> Result<String, ToolQueryError> {
    let ffmpeg = tool_version_for_cache_key(
        std::path::Path::new("ffmpeg"),
        ToolVersionFlavor::Ffmpeg,
        control,
    )?;
    let ffprobe = if remux_p8 {
        Some(tool_version_for_cache_key(
            std::path::Path::new("ffprobe"),
            ToolVersionFlavor::Ffprobe,
            control,
        )?)
    } else {
        None
    };
    let dovi = if remux_p8 {
        match dovi_tool_path() {
            Some(path) => Some(tool_version_for_cache_key(
                &path,
                ToolVersionFlavor::Dovi,
                control,
            )?),
            None if control.is_none() => Some("unavailable".into()),
            None => {
                return Err(ToolQueryError::Query {
                    executable: std::path::PathBuf::from("dovi_tool"),
                    message: "not found on PATH".into(),
                });
            }
        }
    } else {
        None
    };
    Ok(transcode_cache_key_with_tools(
        source,
        plan,
        remux_p8,
        &ffmpeg,
        ffprobe.as_deref(),
        dovi.as_deref(),
    ))
}

fn transcode_cache_key_with_tools(
    source: &str,
    plan: &TranscodePlan,
    remux_p8: bool,
    ffmpeg: &str,
    ffprobe: Option<&str>,
    dovi: Option<&str>,
) -> String {
    let browser_quality = plan
        .browser_quality
        .map(|quality| {
            format!(
                "{}:{}x{}:{}fps:{}:{}:crf{}:{}k:{}k",
                quality.id(),
                quality.max_width(),
                quality.max_height(),
                quality.max_fps(),
                quality.h264_profile(),
                quality.h264_level(),
                quality.crf(),
                quality.max_video_kbps(),
                quality.audio_kbps(),
            )
        })
        .unwrap_or_else(|| "none".into());
    let profile8_signature = if remux_p8 {
        format!(
            "dovi={}\nffprobe={}\nprofile8_revision={PROFILE8_TOOLCHAIN_CACHE_REVISION}",
            dovi.unwrap_or("unavailable"),
            ffprobe.unwrap_or("unavailable")
        )
    } else {
        "dovi=unused".into()
    };
    let signature = format!(
        "source={source}\naction={:?}\nencoder={}\nhardware_decode={:?}\naudio={:?}\naudio_index={}\ncontainer={}\nkeep_hdr10={}\ndrop_dolby_vision={}\nbrowser_quality={browser_quality}\nremux_p8={remux_p8}\nffmpeg={ffmpeg}\n{profile8_signature}\nbuild={}",
        plan.action,
        plan.video_encoder,
        plan.hardware_decode,
        plan.audio,
        plan.audio_index,
        plan.container,
        plan.keep_hdr10,
        plan.drop_dolby_vision,
        env!("CARGO_PKG_VERSION"),
    );
    let mut hasher = Sha256::new();
    hasher.update(signature.as_bytes());
    lowercase_hex(&hasher.finalize())
}

pub fn write_cache_stamp_for_key(dest: &std::path::Path, cache_key: &str) -> std::io::Result<()> {
    std::fs::write(cache_stamp_path(dest), cache_key)
}

pub fn cache_is_fresh_for_key(dest: &std::path::Path, cache_key: &str) -> bool {
    let Ok(metadata) = dest.metadata() else {
        return false;
    };
    if metadata.len() == 0 {
        return false;
    }
    std::fs::read_to_string(cache_stamp_path(dest))
        .ok()
        .is_some_and(|have| have.trim() == cache_key)
}

pub fn dovi_tool_path() -> Option<std::path::PathBuf> {
    let p = std::env::var_os("DOVI_TOOL").map(std::path::PathBuf::from);
    if let Some(p) = p {
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("dovi_tool");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// HDR10 encode used when `dovi_tool` is missing or convert fails.
pub fn hdr10_fallback_plan(from: &TranscodePlan) -> TranscodePlan {
    TranscodePlan {
        decision: Decision::Recode,
        action: RecodeAction::Hdr10,
        rule: from.rule.clone(),
        keep_hdr10: true,
        drop_dolby_vision: true,
        video_encoder: if from.video_encoder == "copy" {
            "libx264".into()
        } else {
            from.video_encoder.clone()
        },
        hardware_decode: HardwareDecode::None,
        // A fallback changes only the video treatment.  In particular, a
        // caller that selected audio copy must not get a surprise AAC encode
        // merely because dovi_tool is unavailable.
        audio: from.audio,
        container: "mp4",
        audio_index: from.audio_index,
        browser_quality: from.browser_quality,
        browser_ai_upscale: None,
    }
}

/// Why a controlled Profile-8 preprocessing pipeline stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum RemuxP8Error {
    Cancelled(String),
    Deadline(String),
    Observer(String),
    Pipeline(String),
}

impl std::fmt::Display for RemuxP8Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled(message)
            | Self::Deadline(message)
            | Self::Observer(message)
            | Self::Pipeline(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RemuxP8Error {}

impl From<String> for RemuxP8Error {
    fn from(message: String) -> Self {
        Self::Pipeline(message)
    }
}

impl From<&str> for RemuxP8Error {
    fn from(message: &str) -> Self {
        Self::Pipeline(message.to_owned())
    }
}

struct RemuxP8Control<'a> {
    deadline: std::time::Instant,
    cancelled: &'a std::sync::atomic::AtomicBool,
    observer: &'a mut dyn FnMut() -> Result<(), String>,
}

enum RemuxP8ControlStop {
    Cancelled(String),
    Deadline(String),
    Observer(String),
}

impl From<RemuxP8ControlStop> for RemuxP8Error {
    fn from(stop: RemuxP8ControlStop) -> Self {
        match stop {
            RemuxP8ControlStop::Cancelled(message) => Self::Cancelled(message),
            RemuxP8ControlStop::Deadline(message) => Self::Deadline(message),
            RemuxP8ControlStop::Observer(message) => Self::Observer(message),
        }
    }
}

impl RemuxP8Control<'_> {
    fn check(&mut self, operation: &str) -> Result<(), RemuxP8ControlStop> {
        use std::sync::atomic::Ordering;

        if self.cancelled.load(Ordering::Acquire) {
            return Err(RemuxP8ControlStop::Cancelled(format!(
                "{operation} cancelled"
            )));
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(RemuxP8ControlStop::Deadline(format!(
                "{operation} timed out"
            )));
        }
        (self.observer)().map_err(RemuxP8ControlStop::Observer)
    }
}

#[cfg(test)]
fn run_cmd_controlled(
    args: &[OsString],
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    source: Option<&std::fs::File>,
) -> Result<(), String> {
    let mut observer = || Ok(());
    let mut control = RemuxP8Control {
        deadline,
        cancelled,
        observer: &mut observer,
    };
    run_cmd_controlled_output(args, source, None, false, &mut control)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
fn run_cmd_capture_controlled(
    args: &[OsString],
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    source: Option<&std::fs::File>,
) -> Result<Vec<u8>, String> {
    let mut observer = || Ok(());
    let mut control = RemuxP8Control {
        deadline,
        cancelled,
        observer: &mut observer,
    };
    run_cmd_controlled_output(args, source, None, true, &mut control)
        .map_err(|error| error.to_string())
}

fn run_cmd_controlled_output(
    args: &[OsString],
    source: Option<&std::fs::File>,
    verified_executable: Option<&VerifiedExecutable>,
    capture_stdout: bool,
    control: &mut RemuxP8Control<'_>,
) -> Result<Vec<u8>, RemuxP8Error> {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureOverflow, CaptureReadError, CaptureRetention, CapturedStream,
        SupervisedCommand, SupervisedOutcome, SupervisionError,
    };
    use std::ops::ControlFlow;

    if args.is_empty() {
        return Err(RemuxP8Error::Pipeline("empty command".into()));
    }
    let mut command = verified_executable.map_or_else(
        || std::process::Command::new(&args[0]),
        VerifiedExecutable::command,
    );
    command.args(&args[1..]);
    const MAX_STDOUT: usize = 64 * 1024;
    const MAX_STDERR: usize = 64 * 1024;
    let mut runner = SupervisedCommand::new(&mut command)
        .capture_stderr(CaptureConfig::new(MAX_STDERR, CaptureRetention::Tail));
    if capture_stdout {
        runner = runner.capture_stdout(
            CaptureConfig::new(MAX_STDOUT, CaptureRetention::Head)
                .overflow(CaptureOverflow::Error)
                .read_error(CaptureReadError::Error),
        );
    }
    if let Some(source) = source {
        runner = runner.inherit_file_at(source, 3).map_err(|error| {
            format!("inherit source for {}: {error}", args[0].to_string_lossy())
        })?;
    }
    if let Some(executable) = verified_executable {
        runner = executable.inherit_for_execution(runner).map_err(|error| {
            format!(
                "inherit executable for {}: {error}",
                executable.path().display()
            )
        })?;
    }
    enum Stop {
        Cancelled,
        Deadline,
        Observer(String),
    }
    let executable = args[0].to_string_lossy().into_owned();
    let outcome = runner.run_until(
        control.deadline,
        std::time::Duration::from_millis(50),
        || match control.check(&executable) {
            Ok(()) => ControlFlow::Continue(()),
            Err(RemuxP8ControlStop::Cancelled(_)) => ControlFlow::Break(Stop::Cancelled),
            Err(RemuxP8ControlStop::Deadline(_)) => ControlFlow::Break(Stop::Deadline),
            Err(RemuxP8ControlStop::Observer(error)) => ControlFlow::Break(Stop::Observer(error)),
        },
    );
    match outcome {
        Ok(SupervisedOutcome::Exited(output)) if output.status.success() => Ok(output.stdout),
        Ok(SupervisedOutcome::Exited(output)) => {
            let tail = String::from_utf8_lossy(&output.stderr);
            Err(RemuxP8Error::Pipeline(format!(
                "{executable}: {}",
                tail.trim()
            )))
        }
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Cancelled,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Cancelled,
                ..
            },
        ) => Err(RemuxP8Error::Cancelled(format!("{executable} cancelled"))),
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Deadline,
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Deadline,
                ..
            }
            | SupervisedOutcome::Deadline { .. },
        ) => Err(RemuxP8Error::Deadline(format!("{executable} timed out"))),
        Ok(
            SupervisedOutcome::NotStarted {
                reason: Stop::Observer(error),
            }
            | SupervisedOutcome::Stopped {
                reason: Stop::Observer(error),
                ..
            },
        ) => Err(RemuxP8Error::Observer(error)),
        Err(SupervisionError::Spawn(error)) => Err(RemuxP8Error::Pipeline(format!(
            "spawn {executable}: {error}"
        ))),
        Err(SupervisionError::Wait(error)) => Err(RemuxP8Error::Pipeline(format!(
            "wait {executable}: {error}"
        ))),
        Err(SupervisionError::CaptureOverflow {
            stream: CapturedStream::Stdout,
            limit,
        }) => Err(RemuxP8Error::Pipeline(format!(
            "helper stdout exceeded {limit} bytes"
        ))),
        Err(SupervisionError::CaptureIo {
            stream: CapturedStream::Stdout,
            source,
        }) => Err(RemuxP8Error::Pipeline(format!(
            "read helper stdout: {source}"
        ))),
        Err(SupervisionError::CapturePanicked(CapturedStream::Stdout)) => Err(
            RemuxP8Error::Pipeline(format!("{executable} stdout reader panicked")),
        ),
        Err(error) => Err(RemuxP8Error::Pipeline(error.to_string())),
    }
}

#[derive(Clone, Copy, Debug)]
struct Mp4BoxSpan {
    offset: u64,
    size: u64,
    header_size: u64,
    kind: [u8; 4],
}

impl Mp4BoxSpan {
    fn content_start(self) -> u64 {
        self.offset + self.header_size
    }

    fn end(self) -> u64 {
        self.offset + self.size
    }
}

fn check_mp4_signal_control(control: &mut RemuxP8Control<'_>) -> Result<(), RemuxP8Error> {
    control.check("MP4 signaling").map_err(Into::into)
}

fn read_mp4_box(
    file: &mut std::fs::File,
    offset: u64,
    parent_end: u64,
) -> Result<Mp4BoxSpan, String> {
    use std::io::{Read, Seek, SeekFrom};

    let remaining = parent_end
        .checked_sub(offset)
        .ok_or_else(|| "invalid MP4 box bounds".to_string())?;
    if remaining < 8 {
        return Err(format!("truncated MP4 box header at byte {offset}"));
    }
    let mut header = [0u8; 16];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut header[..8]))
        .map_err(|error| format!("read MP4 box header at byte {offset}: {error}"))?;
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let kind = [header[4], header[5], header[6], header[7]];
    let (size, header_size) = match size32 {
        0 => (remaining, 8),
        1 => {
            if remaining < 16 {
                return Err(format!("truncated large MP4 box at byte {offset}"));
            }
            file.read_exact(&mut header[8..16])
                .map_err(|error| format!("read large MP4 box at byte {offset}: {error}"))?;
            (
                u64::from_be_bytes([
                    header[8], header[9], header[10], header[11], header[12], header[13],
                    header[14], header[15],
                ]),
                16,
            )
        }
        size => (u64::from(size), 8),
    };
    if size < header_size || size > remaining {
        return Err(format!("invalid MP4 box size {size} at byte {offset}"));
    }
    Ok(Mp4BoxSpan {
        offset,
        size,
        header_size,
        kind,
    })
}

fn find_profile8_sample_entry_path(
    file: &mut std::fs::File,
    start: u64,
    end: u64,
    depth: usize,
    spans: &mut Vec<Mp4BoxSpan>,
    control: &mut RemuxP8Control<'_>,
) -> Result<bool, RemuxP8Error> {
    const CONTAINER_PATH: [[u8; 4]; 6] =
        [*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"];
    if start > end {
        return Err("invalid MP4 box bounds".into());
    }
    let mut offset = start;
    while offset < end {
        check_mp4_signal_control(control)?;
        let span = read_mp4_box(file, offset, end)?;
        offset = span.end();
        if span.kind != CONTAINER_PATH[depth] {
            continue;
        }
        spans.push(span);
        if depth + 1 == CONTAINER_PATH.len() {
            let entries_start = span
                .content_start()
                .checked_add(8)
                .ok_or_else(|| "MP4 stsd offset overflow".to_string())?;
            if entries_start > span.end() {
                return Err("truncated MP4 stsd full-box header".into());
            }
            let mut entry_offset = entries_start;
            while entry_offset < span.end() {
                check_mp4_signal_control(control)?;
                let entry = read_mp4_box(file, entry_offset, span.end())?;
                entry_offset = entry.end();
                if entry.kind == *b"hvc1" || entry.kind == *b"hev1" {
                    spans.push(entry);
                    return Ok(true);
                }
            }
        } else if find_profile8_sample_entry_path(
            file,
            span.content_start(),
            span.end(),
            depth + 1,
            spans,
            control,
        )? {
            return Ok(true);
        }
        spans.pop();
    }
    Ok(false)
}

fn profile8_sample_entry_path(
    file: &mut std::fs::File,
    file_len: u64,
    control: &mut RemuxP8Control<'_>,
) -> Result<Vec<Mp4BoxSpan>, RemuxP8Error> {
    let mut spans = Vec::with_capacity(7);
    if find_profile8_sample_entry_path(file, 0, file_len, 0, &mut spans, control)? {
        Ok(spans)
    } else {
        Err("HEVC sample entry not found in intermediate MP4".into())
    }
}

fn validate_profile8_media_data_layout(
    file: &mut std::fs::File,
    file_len: u64,
    moov: Mp4BoxSpan,
    control: &mut RemuxP8Control<'_>,
) -> Result<(), RemuxP8Error> {
    let mut offset = 0;
    while offset < file_len {
        check_mp4_signal_control(control)?;
        let span = read_mp4_box(file, offset, file_len)?;
        offset = span.end();
        if span.kind == *b"mdat" && span.end() > moov.offset {
            return Err(
                "unsupported MP4 layout: media data must precede the moov box for signaling".into(),
            );
        }
    }
    Ok(())
}

fn entry_has_dolby_vision_configuration(
    file: &mut std::fs::File,
    entry: Mp4BoxSpan,
    control: &mut RemuxP8Control<'_>,
) -> Result<bool, RemuxP8Error> {
    const VISUAL_SAMPLE_ENTRY_FIELDS: u64 = 78;
    let children_start = entry
        .content_start()
        .checked_add(VISUAL_SAMPLE_ENTRY_FIELDS)
        .ok_or_else(|| "MP4 sample-entry offset overflow".to_string())?;
    if children_start > entry.end() {
        return Err("truncated HEVC visual sample entry".into());
    }
    let mut offset = children_start;
    while offset < entry.end() {
        check_mp4_signal_control(control)?;
        let child = read_mp4_box(file, offset, entry.end())?;
        offset = child.end();
        if matches!(&child.kind, b"dvcC" | b"dvvC" | b"dvwC") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn insert_mp4_bytes(
    file: &mut std::fs::File,
    file_len: u64,
    insert_at: u64,
    bytes: &[u8],
    control: &mut RemuxP8Control<'_>,
) -> Result<(), RemuxP8Error> {
    use std::io::{Read, Seek, SeekFrom, Write};

    if insert_at > file_len {
        return Err("MP4 insertion offset is past end of file".into());
    }
    let growth = u64::try_from(bytes.len()).map_err(|_| "MP4 insertion is too large")?;
    let grown_len = file_len
        .checked_add(growth)
        .ok_or_else(|| "MP4 file size overflow".to_string())?;
    check_mp4_signal_control(control)?;
    file.set_len(grown_len)
        .map_err(|error| format!("extend MP4 staging file: {error}"))?;
    const MOVE_CHUNK_BYTES: u64 = 64 * 1024;
    let mut buffer = [0u8; MOVE_CHUNK_BYTES as usize];
    let mut read_end = file_len;
    while read_end > insert_at {
        check_mp4_signal_control(control)?;
        let chunk_len = usize::try_from((read_end - insert_at).min(MOVE_CHUNK_BYTES))
            .map_err(|_| "MP4 move chunk is too large")?;
        let read_start = read_end - u64::try_from(chunk_len).map_err(|_| "MP4 move overflow")?;
        file.seek(SeekFrom::Start(read_start))
            .and_then(|_| file.read_exact(&mut buffer[..chunk_len]))
            .map_err(|error| format!("read MP4 tail at byte {read_start}: {error}"))?;
        let write_start = read_start
            .checked_add(growth)
            .ok_or_else(|| "MP4 move offset overflow".to_string())?;
        file.seek(SeekFrom::Start(write_start))
            .and_then(|_| file.write_all(&buffer[..chunk_len]))
            .map_err(|error| format!("write MP4 tail at byte {write_start}: {error}"))?;
        read_end = read_start;
    }
    check_mp4_signal_control(control)?;
    file.seek(SeekFrom::Start(insert_at))
        .and_then(|_| file.write_all(bytes))
        .map_err(|error| format!("write Dolby Vision MP4 signaling: {error}"))?;
    Ok(())
}

fn signal_profile8_in_mp4_with_control(
    path: &Path,
    level: u8,
    control: &mut RemuxP8Control<'_>,
) -> Result<(), RemuxP8Error> {
    use std::io::{Seek, SeekFrom, Write};

    if level > 63 {
        return Err(format!("invalid Dolby Vision level {level}").into());
    }
    check_mp4_signal_control(control)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    let spans = profile8_sample_entry_path(&mut file, file_len, control)?;
    let entry = spans
        .last()
        .copied()
        .ok_or_else(|| "HEVC sample entry path was empty".to_string())?;
    let moov = spans
        .first()
        .copied()
        .ok_or_else(|| "HEVC sample entry path was empty".to_string())?;
    validate_profile8_media_data_layout(&mut file, file_len, moov, control)?;
    if entry_has_dolby_vision_configuration(&mut file, entry, control)? {
        return Err("intermediate MP4 already has Dolby Vision signaling".into());
    }

    // ISO/IEC 14496-15 Dolby Vision decoder configuration record: v1.0,
    // Profile 8, source level, RPU+BL present, no EL, HDR10 compatibility 1.
    let mut dvvc = [0u8; 32];
    dvvc[..4].copy_from_slice(&32u32.to_be_bytes());
    dvvc[4..8].copy_from_slice(b"dvvC");
    dvvc[8] = 1;
    let flags = (8u16 << 9) | (u16::from(level) << 3) | (1 << 2) | 1;
    dvvc[10..12].copy_from_slice(&flags.to_be_bytes());
    dvvc[12] = 1 << 4;

    let mut grown_sizes = Vec::with_capacity(spans.len());
    for span in &spans {
        if span.header_size != 8 || span.size > u64::from(u32::MAX) {
            return Err(format!(
                "unsupported large {:?} MP4 box",
                String::from_utf8_lossy(&span.kind)
            )
            .into());
        }
        let grown = span
            .size
            .checked_add(u64::try_from(dvvc.len()).map_err(|_| "MP4 box growth overflow")?)
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| "MP4 box size overflow".to_string())?;
        grown_sizes.push((span.offset, grown));
    }
    insert_mp4_bytes(&mut file, file_len, entry.end(), &dvvc, control)?;
    for (offset, grown) in grown_sizes {
        check_mp4_signal_control(control)?;
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.write_all(&grown.to_be_bytes()))
            .map_err(|error| format!("update MP4 box size at byte {offset}: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
fn signal_profile8_in_mp4(
    path: &Path,
    level: u8,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let mut observer = || Ok(());
    let mut control = RemuxP8Control {
        deadline,
        cancelled,
        observer: &mut observer,
    };
    signal_profile8_in_mp4_with_control(path, level, &mut control)
        .map_err(|error| error.to_string())
}

/// BL + P8.1 RPU via `dovi_tool -m 2 convert --discard`. Writes `dest_part`
/// under the caller's deadline and cancellation token.
pub fn run_remux_p8_controlled(
    src: &std::path::Path,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    run_remux_p8_controlled_observed(src, dest_part, plan, deadline, cancelled, || Ok(()))
        .map_err(|error| error.to_string())
}

pub fn run_remux_p8_file_controlled(
    src: &std::fs::File,
    identity_path: &std::path::Path,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    run_remux_p8_file_controlled_observed(
        src,
        identity_path,
        dest_part,
        plan,
        deadline,
        cancelled,
        || Ok(()),
    )
    .map_err(|error| error.to_string())
}

/// Run Profile-8 preprocessing while periodically invoking caller-owned
/// pressure checks. Returning an observer error stops and reaps the current
/// helper before this function returns.
pub fn run_remux_p8_controlled_observed(
    src: &std::path::Path,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    mut observer: impl FnMut() -> Result<(), String>,
) -> Result<(), RemuxP8Error> {
    let toolchain = {
        let mut control = RemuxP8Control {
            deadline,
            cancelled,
            observer: &mut observer,
        };
        Profile8ToolchainSnapshot::query_under_remux_control(&mut control)?
    };
    run_remux_p8_with_toolchain_observed(
        &toolchain,
        RemuxP8Input::Path(src),
        dest_part,
        plan,
        deadline,
        cancelled,
        &mut observer,
    )
}

/// File-descriptor variant of [`run_remux_p8_controlled_observed`].
pub fn run_remux_p8_file_controlled_observed(
    src: &std::fs::File,
    identity_path: &std::path::Path,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    mut observer: impl FnMut() -> Result<(), String>,
) -> Result<(), RemuxP8Error> {
    let toolchain = {
        let mut control = RemuxP8Control {
            deadline,
            cancelled,
            observer: &mut observer,
        };
        Profile8ToolchainSnapshot::query_under_remux_control(&mut control)?
    };
    run_remux_p8_with_toolchain_observed(
        &toolchain,
        RemuxP8Input::OpenFile {
            file: src,
            identity_path,
        },
        dest_part,
        plan,
        deadline,
        cancelled,
        &mut observer,
    )
}

/// Source passed to a snapshot-pinned Profile-8 producer.
#[derive(Clone, Copy, Debug)]
pub enum RemuxP8Input<'a> {
    /// Compatibility path input. Daemon jobs should prefer [`Self::OpenFile`].
    Path(&'a std::path::Path),
    /// Already confined source descriptor plus its cache-identity path.
    OpenFile {
        file: &'a std::fs::File,
        identity_path: &'a std::path::Path,
    },
}

/// Snapshot-pinned Profile-8 producer. The exact executable paths used to
/// fingerprint the cache identity produce the output.
pub fn run_remux_p8_with_toolchain_observed(
    toolchain: &Profile8ToolchainSnapshot,
    input: RemuxP8Input<'_>,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    mut observer: impl FnMut() -> Result<(), String>,
) -> Result<(), RemuxP8Error> {
    run_remux_p8_impl(
        toolchain,
        input,
        dest_part,
        plan,
        deadline,
        cancelled,
        &mut observer,
    )
}

struct Profile8Commands {
    probe_level: Vec<OsString>,
    extract: Vec<OsString>,
    convert: Vec<OsString>,
    wrap: Vec<OsString>,
    mux: Vec<OsString>,
}

fn profile8_commands(
    toolchain: &Profile8ToolchainSnapshot,
    source_arg: OsString,
    hevc: &Path,
    p8: &Path,
    p8_mp4: &Path,
    dest_part: &Path,
    plan: &TranscodePlan,
) -> Profile8Commands {
    let extract = vec![
        toolchain.ffmpeg.path.as_os_str().to_os_string(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-nostdin".into(),
        "-i".into(),
        source_arg.clone(),
        "-map".into(),
        "0:v:0".into(),
        "-c:v".into(),
        "copy".into(),
        "-bsf:v".into(),
        "hevc_mp4toannexb".into(),
        "-an".into(),
        "-f".into(),
        "hevc".into(),
        hevc.as_os_str().to_os_string(),
    ];
    let convert = vec![
        toolchain.dovi_tool.path.as_os_str().to_os_string(),
        "-m".into(),
        "2".into(),
        "convert".into(),
        "--discard".into(),
        hevc.as_os_str().to_os_string(),
        "-o".into(),
        p8.as_os_str().to_os_string(),
    ];
    let probe_level = vec![
        toolchain.ffprobe.path.as_os_str().to_os_string(),
        "-v".into(),
        "error".into(),
        "-select_streams".into(),
        "v:0".into(),
        "-show_entries".into(),
        "stream_side_data=dv_level".into(),
        "-of".into(),
        "default=noprint_wrappers=1:nokey=1".into(),
        source_arg.clone(),
    ];
    let wrap = vec![
        toolchain.ffmpeg.path.as_os_str().to_os_string(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-nostdin".into(),
        "-fflags".into(),
        "+genpts".into(),
        "-i".into(),
        p8.as_os_str().to_os_string(),
        "-map".into(),
        "0:v:0".into(),
        "-c:v".into(),
        "copy".into(),
        "-tag:v".into(),
        "hvc1".into(),
        "-an".into(),
        p8_mp4.as_os_str().to_os_string(),
    ];
    let mut mux = vec![
        toolchain.ffmpeg.path.as_os_str().to_os_string(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-nostdin".into(),
        "-i".into(),
        p8_mp4.as_os_str().to_os_string(),
        "-i".into(),
        source_arg,
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        format!("1:a:{}?", plan.audio_index).into(),
        "-c:v".into(),
        "copy".into(),
        "-tag:v".into(),
        "hvc1".into(),
        "-strict".into(),
        "unofficial".into(),
    ];
    match plan.audio {
        AudioAction::Copy => mux.extend(["-c:a".into(), "copy".into()]),
        AudioAction::ToAc3 => {
            mux.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), "640k".into()])
        }
        AudioAction::ToAac => {
            mux.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "256k".into()])
        }
    }
    mux.extend(live_frag_os_tail(dest_part.as_os_str()));
    Profile8Commands {
        probe_level,
        extract,
        convert,
        wrap,
        mux,
    }
}

fn run_remux_p8_impl(
    toolchain: &Profile8ToolchainSnapshot,
    input: RemuxP8Input<'_>,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    observer: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), RemuxP8Error> {
    let (src, source_file) = match input {
        RemuxP8Input::Path(src) => (src, None),
        RemuxP8Input::OpenFile {
            file,
            identity_path,
        } => (identity_path, Some(file)),
    };
    toolchain.verify_current().map_err(RemuxP8Error::Pipeline)?;
    if let Some(parent) = dest_part.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let hevc = dest_part.with_extension("hevc");
    let p8 = dest_part.with_extension("p8.hevc");
    let p8_mp4 = dest_part.with_extension("p8.mp4");
    let _ = std::fs::remove_file(&hevc);
    let _ = std::fs::remove_file(&p8);
    let _ = std::fs::remove_file(&p8_mp4);
    let source_arg = if source_file.is_some() {
        OsString::from("/proc/self/fd/3")
    } else {
        src.as_os_str().to_os_string()
    };
    let commands = profile8_commands(toolchain, source_arg, &hevc, &p8, &p8_mp4, dest_part, plan);
    let mut control = RemuxP8Control {
        deadline,
        cancelled,
        observer,
    };
    let result = (|| {
        toolchain
            .ffprobe
            .verify_current("ffprobe")
            .map_err(RemuxP8Error::Pipeline)?;
        let output = run_cmd_controlled_output(
            &commands.probe_level,
            source_file,
            Some(toolchain.ffprobe()),
            true,
            &mut control,
        )?;
        let level = String::from_utf8(output)
            .map_err(|_| "ffprobe Dolby Vision level was not UTF-8".to_string())?;
        let level = level
            .lines()
            .find_map(|line| line.trim().parse::<u8>().ok())
            .filter(|level| *level <= 63)
            .ok_or_else(|| "ffprobe did not report a valid Dolby Vision level".to_string())?;

        toolchain
            .ffmpeg
            .verify_current("ffmpeg")
            .map_err(RemuxP8Error::Pipeline)?;
        run_cmd_controlled_output(
            &commands.extract,
            source_file,
            Some(toolchain.ffmpeg()),
            false,
            &mut control,
        )?;
        toolchain
            .dovi_tool
            .verify_current("dovi_tool")
            .map_err(RemuxP8Error::Pipeline)?;
        run_cmd_controlled_output(
            &commands.convert,
            None,
            Some(toolchain.dovi_tool()),
            false,
            &mut control,
        )?;
        let _ = std::fs::remove_file(&hevc);
        toolchain
            .ffmpeg
            .verify_current("ffmpeg")
            .map_err(RemuxP8Error::Pipeline)?;
        run_cmd_controlled_output(
            &commands.wrap,
            None,
            Some(toolchain.ffmpeg()),
            false,
            &mut control,
        )?;
        let _ = std::fs::remove_file(&p8);
        signal_profile8_in_mp4_with_control(&p8_mp4, level, &mut control)?;
        toolchain
            .ffmpeg
            .verify_current("ffmpeg")
            .map_err(RemuxP8Error::Pipeline)?;
        run_cmd_controlled_output(
            &commands.mux,
            source_file,
            Some(toolchain.ffmpeg()),
            false,
            &mut control,
        )?;
        let _ = std::fs::remove_file(&p8_mp4);
        Ok(())
    })();
    let _ = std::fs::remove_file(&hevc);
    let _ = std::fs::remove_file(&p8);
    let _ = std::fs::remove_file(&p8_mp4);
    result
}

fn first_codec(s: &str) -> &str {
    s.split(',')
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("")
}

fn audio_codec_from_name(audio: &str) -> AudioCodec {
    match first_codec(audio) {
        "truehd" | "atmos" => AudioCodec::TrueHd,
        "ac3" => AudioCodec::Ac3,
        "eac3" => AudioCodec::Eac3,
        "dts" | "dts-hd" => AudioCodec::Dts,
        "aac" => AudioCodec::Aac,
        _ => AudioCodec::Other,
    }
}

/// Normalize a selected browser audio codec without assuming probe casing.
pub fn browser_audio_codec_from_name(audio: &str) -> AudioCodec {
    match first_codec(audio).to_ascii_lowercase().as_str() {
        "truehd" | "atmos" => AudioCodec::TrueHd,
        "ac3" => AudioCodec::Ac3,
        "eac3" => AudioCodec::Eac3,
        "dts" | "dts-hd" => AudioCodec::Dts,
        "aac" => AudioCodec::Aac,
        _ => AudioCodec::Other,
    }
}

pub fn probe_to_source(
    container: &str,
    video: &str,
    hdr: &str,
    audio: &str,
    w: u32,
    h: u32,
) -> SourceMedia {
    let video = first_codec(video);
    let audio = first_codec(audio);
    let hdr = first_codec(hdr);
    SourceMedia {
        container: match container {
            "mp4" => Container::Mp4,
            "avi" => Container::Avi,
            "mpeg-ts" | "ts" => Container::MpegTs,
            "" => Container::Other,
            _ => Container::Mkv,
        },
        video_codec: match video {
            "h264" | "avc" => VideoCodec::H264,
            "mpeg2" => VideoCodec::Mpeg2,
            "mpeg4" | "msmpeg4v3" | "xvid" | "divx" => VideoCodec::Mpeg4,
            "hevc" | "h265" => VideoCodec::Hevc,
            _ => VideoCodec::Other,
        },
        hdr: match hdr {
            "dv-p7" | "dvhe.07" | "profile-7" => HdrKind::DolbyVisionProfile7,
            "dv-p8" | "dvhe.08" | "profile-8" => HdrKind::DolbyVisionProfile8,
            "dv-p5" | "dvhe.05" => HdrKind::DolbyVisionProfile5,
            "hdr10" => HdrKind::Hdr10,
            "dv" => HdrKind::DolbyVisionOther,
            "sdr" => HdrKind::Sdr,
            _ => HdrKind::Unknown,
        },
        audio: audio_codec_from_name(audio),
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_dlna_protocol::identify_user_agent;

    #[cfg(unix)]
    fn tool_test_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rdlna-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    fn write_fake_version_tool(path: &Path, expected_arg: &str, version: &str) {
        write_executable_script(
            path,
            &format!(
                "if [ \"$1\" = '{expected_arg}' ]; then printf '%s\\n' '{version}'; else exit 9; fi"
            ),
        );
    }

    #[cfg(unix)]
    fn fake_profile8_toolchain(dir: &Path) -> Profile8ToolchainSnapshot {
        Profile8ToolchainSnapshot::query_paths(
            &dir.join("ffmpeg"),
            &dir.join("ffprobe"),
            &dir.join("dovi_tool"),
            None,
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn profile8_snapshot_cache_key(toolchain: &Profile8ToolchainSnapshot) -> String {
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::RemuxP8,
            video_encoder: "copy".into(),
            audio: AudioAction::Copy,
            container: "mp4",
            ..TranscodePlan::default()
        };
        transcode_cache_key_with_tools(
            "fake-source-identity",
            &plan,
            true,
            toolchain.ffmpeg().fingerprint(),
            Some(toolchain.ffprobe().fingerprint()),
            Some(toolchain.dovi_tool().fingerprint()),
        )
    }

    fn test_mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8usize.checked_add(payload.len()).unwrap()).unwrap();
        let mut result = Vec::with_capacity(size as usize);
        result.extend_from_slice(&size.to_be_bytes());
        result.extend_from_slice(kind);
        result.extend_from_slice(payload);
        result
    }

    fn test_profile8_moov() -> Vec<u8> {
        let mut entry_payload = vec![0; 78];
        entry_payload.extend(test_mp4_box(b"hvcC", &[1, 2, 3, 4]));
        let entry = test_mp4_box(b"hvc1", &entry_payload);
        let mut stsd_payload = vec![0; 8];
        stsd_payload[7] = 1;
        stsd_payload.extend(entry);
        let stsd = test_mp4_box(b"stsd", &stsd_payload);
        let stbl = test_mp4_box(b"stbl", &stsd);
        let minf = test_mp4_box(b"minf", &stbl);
        let mdia = test_mp4_box(b"mdia", &minf);
        let trak = test_mp4_box(b"trak", &mdia);
        test_mp4_box(b"moov", &trak)
    }

    fn expected_profile8_dvvc(level: u8) -> [u8; 32] {
        let mut dvvc = [0u8; 32];
        dvvc[..4].copy_from_slice(&32u32.to_be_bytes());
        dvvc[4..8].copy_from_slice(b"dvvC");
        dvvc[8] = 1;
        let flags = (8u16 << 9) | (u16::from(level) << 3) | (1 << 2) | 1;
        dvvc[10..12].copy_from_slice(&flags.to_be_bytes());
        dvvc[12] = 1 << 4;
        dvvc
    }

    fn p7_truehd() -> SourceMedia {
        SourceMedia {
            container: Container::Mkv,
            video_codec: VideoCodec::Hevc,
            hdr: HdrKind::DolbyVisionProfile7,
            audio: AudioCodec::TrueHd,
            width: 3840,
            height: 2160,
        }
    }

    fn p8_ac3() -> SourceMedia {
        SourceMedia {
            container: Container::Mkv,
            video_codec: VideoCodec::Hevc,
            hdr: HdrKind::DolbyVisionProfile8,
            audio: AudioCodec::Ac3,
            width: 3840,
            height: 2160,
        }
    }

    fn sample_remaps() -> Vec<RemapRule> {
        parse_remaps_toml(
            r#"
[[remap]]
name = "cast-p7"
client = "google-cast"
hdr = "dv-p7"
action = "hdr10"
encoder = "hevc_nvenc"
audio_out = "to-ac3"

[[remap]]
name = "cast-truehd"
client = "crkey"
audio = "truehd"
action = "audio-ac3"
"#,
        )
        .unwrap()
    }

    #[test]
    fn no_rules_means_original_even_for_cast_p7() {
        let cast = identify_user_agent("CrKey/1.54").unwrap();
        let p = decide(cast, &p7_truehd(), &[]);
        assert_eq!(p.decision, Decision::ServeOriginal);
    }

    #[test]
    fn remap_validation_rejects_contradictory_actions_and_bad_encoder_names() {
        let original = parse_remaps_toml(
            r#"
[[remap]]
action = "original"
audio_out = "to-aac"
"#,
        )
        .unwrap();
        assert!(validate_remap_rules(&original, "libx264")
            .unwrap_err()
            .contains("action=original"));

        let p8_encode = parse_remaps_toml(
            r#"
[[remap]]
hdr = "dv-p7"
action = "remux-p8"
encoder = "hevc_nvenc"
"#,
        )
        .unwrap();
        assert!(validate_remap_rules(&p8_encode, "libx264")
            .unwrap_err()
            .contains("encoder=copy"));

        let hdr_copy = parse_remaps_toml(
            r#"
[[remap]]
hdr = "dv-p7"
action = "hdr10"
encoder = "copy"
"#,
        )
        .unwrap();
        assert!(validate_remap_rules(&hdr_copy, "libx264")
            .unwrap_err()
            .contains("cannot use encoder=copy"));

        let bad_name = parse_remaps_toml(
            r#"
[[remap]]
hdr = "dv-p7"
action = "hdr10"
encoder = "-y bad"
"#,
        )
        .unwrap();
        assert!(validate_remap_rules(&bad_name, "libx264")
            .unwrap_err()
            .contains("invalid encoder name"));
        assert!(validate_remap_rules(&sample_remaps(), "libx264").is_ok());
    }

    #[test]
    fn kodi_ignores_cast_only_rule() {
        let kodi = identify_user_agent("Kodi/21.0").unwrap();
        let p = decide(kodi, &p7_truehd(), &sample_remaps());
        assert_eq!(p.decision, Decision::ServeOriginal);
        assert!(p.rule.is_none());
    }

    #[test]
    fn cast_p7_hits_hdr10_remap() {
        let cast = identify_user_agent("CrKey/1.54 DLNADOC/1.50").unwrap();
        let p = decide(cast, &p7_truehd(), &sample_remaps());
        assert_eq!(p.decision, Decision::Recode);
        assert_eq!(p.action, RecodeAction::Hdr10);
        assert_eq!(p.rule.as_deref(), Some("cast-p7"));
        assert!(p.drop_dolby_vision);
        let args = ffmpeg_grow_args("/media/movie.mkv", "/cache/movie.mp4.part", &p);
        assert!(args.contains(&"hevc_nvenc".into()));
        assert!(args.contains(&"smpte2084".into()));
        assert!(!args.iter().any(|s| s.contains("faststart")));
    }

    #[test]
    fn cast_p8_is_not_p7() {
        let cast = identify_user_agent("CrKey/1.54").unwrap();
        let p = decide(cast, &p8_ac3(), &sample_remaps());
        assert_eq!(p.decision, Decision::ServeOriginal);
    }

    #[test]
    fn first_matching_row_wins() {
        let remaps = parse_remaps_toml(
            r#"
[[remap]]
client = "google-cast"
hdr = "dv-p7"
action = "remux-p8"

[[remap]]
client = "google-cast"
hdr = "dv-p7"
action = "hdr10"
"#,
        )
        .unwrap();
        let cast = identify_user_agent("CrKey/1").unwrap();
        let p = decide(cast, &p7_truehd(), &remaps);
        assert_eq!(p.action, RecodeAction::RemuxP8);
    }

    #[test]
    fn mpeg4_is_not_hevc() {
        let s = probe_to_source("avi", "mpeg4", "sdr", "ac3", 720, 480);
        assert_eq!(s.video_codec, VideoCodec::Mpeg4);
        assert_eq!(s.hdr, HdrKind::Sdr);
        assert_eq!(s.container, Container::Avi);
    }

    #[test]
    fn extra_tracks_use_first_codec() {
        let s = probe_to_source("mp4", "h264", "sdr", "aac,ac3", 1920, 1080);
        assert_eq!(s.video_codec, VideoCodec::H264);
        assert_eq!(s.audio, AudioCodec::Aac);
        assert_eq!(s.width, 1920);
        assert_eq!(s.height, 1080);
    }

    #[test]
    fn empty_probe_is_unknown_not_hevc_sdr() {
        let s = probe_to_source("", "", "", "", 0, 0);
        assert_eq!(s.video_codec, VideoCodec::Other);
        assert_eq!(s.hdr, HdrKind::Unknown);
        assert_eq!(s.audio, AudioCodec::Other);
        let remaps = parse_remaps_toml(
            r#"
[[remap]]
client = "kodi"
hdr = "sdr"
action = "hdr10"
"#,
        )
        .unwrap();
        let kodi = identify_user_agent("Kodi/21.0").unwrap();
        let p = decide(kodi, &s, &remaps);
        assert_eq!(p.decision, Decision::ServeOriginal);
    }

    #[test]
    fn aliases_parse() {
        let r = parse_remaps_toml(
            r#"
[[remap]]
client = "streamer"
hdr = "dvhe.07"
video = "hevc"
container = "mkv"
audio = "truehd"
action = "hdr10"
"#,
        )
        .unwrap();
        assert_eq!(r[0].client.0, vec!["streamer".to_string()]);
        assert_eq!(r[0].hdr, Some(HdrKind::DolbyVisionProfile7));
        assert_eq!(r[0].video, Some(VideoCodec::Hevc));
    }

    #[test]
    fn same_codec_different_software() {
        let remaps = parse_remaps_toml(
            r#"
[[remap]]
name = "only-crkey"
client = "CrKey"
hdr = "dv-p7"
action = "hdr10"

[[remap]]
name = "only-kodi"
client = "Kodi"
hdr = "dv-p7"
action = "original"
"#,
        )
        .unwrap();
        let p7 = p7_truehd();
        let cast = decide_ua("CrKey/1.54 DLNADOC/1.50", &p7, &remaps);
        assert_eq!(cast.action, RecodeAction::Hdr10);
        assert_eq!(cast.rule.as_deref(), Some("only-crkey"));
        let kodi = decide_ua("Kodi/21.0 (Linux)", &p7, &remaps);
        assert_eq!(kodi.decision, Decision::ServeOriginal);
        assert_eq!(kodi.rule.as_deref(), Some("only-kodi"));
        let samsung = identify_user_agent("SEC_HHP_[TV]UE40D7000/1.0").unwrap();
        let p = decide(samsung, &p7, &remaps);
        assert!(p.rule.is_none());
        assert_eq!(p.decision, Decision::ServeOriginal);
    }

    #[test]
    fn clients_list_matches_either() {
        let remaps = parse_remaps_toml(
            r#"
[[remap]]
clients = ["CrKey", "BubbleUPnP"]
audio = "truehd"
action = "audio-ac3"
"#,
        )
        .unwrap();
        let p7 = p7_truehd();
        assert_eq!(
            decide_ua("CrKey/1.0", &p7, &remaps).action,
            RecodeAction::AudioAc3
        );
        assert_eq!(
            decide_ua("BubbleUPnP/3.0", &p7, &remaps).action,
            RecodeAction::AudioAc3
        );
        assert_eq!(
            decide_ua("Kodi/21.0", &p7, &remaps).decision,
            Decision::ServeOriginal
        );
    }

    #[test]
    fn growing_cache_argv_uses_fragmented_file_not_stdout() {
        let cast = identify_user_agent("CrKey/1.54").unwrap();
        let p = decide(cast, &p7_truehd(), &sample_remaps());
        let grow = ffmpeg_grow_args("/media/movie.mkv", "/cache/1-hdr10.mp4.part", &p);
        assert_eq!(grow[0], "ffmpeg");
        assert!(grow.contains(&"/cache/1-hdr10.mp4.part".into()));
        assert!(!grow.iter().any(|s| s == "pipe:1"));
        assert!(!grow.iter().any(|s| s.contains("faststart")));
        assert!(grow.iter().any(|s| s.contains("frag_keyframe")));
        assert!(grow.iter().any(|s| s.contains("delay_moov")));
        assert!(grow
            .windows(2)
            .any(|pair| pair == ["-avoid_negative_ts", "make_zero"]));
    }

    #[test]
    fn browser_encoder_uses_nvenc_quality_options_with_software_default() {
        assert_eq!(browser_video_encoder("h264_nvenc"), "h264_nvenc");
        assert_eq!(browser_video_encoder("hevc_nvenc"), "libx264");
        assert_eq!(browser_video_encoder("libx264"), "libx264");
        assert_eq!(
            browser_hardware_decode("h264_nvenc", VideoCodec::H264),
            HardwareDecode::None
        );
        assert_eq!(
            browser_hardware_decode("h264_nvenc", VideoCodec::Hevc),
            HardwareDecode::Cuda
        );
        assert_eq!(
            browser_hardware_decode("libx264", VideoCodec::Hevc),
            HardwareDecode::None
        );
        assert_eq!(
            browser_hdr_hardware_decode(
                "hevc_nvenc",
                VideoCodec::Hevc,
                HdrKind::DolbyVisionProfile7,
            ),
            HardwareDecode::None
        );
        assert_eq!(
            browser_hdr_hardware_decode("hevc_nvenc", VideoCodec::Hevc, HdrKind::Hdr10),
            HardwareDecode::Cuda
        );

        let mut plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            video_encoder: "h264_nvenc".into(),
            audio: AudioAction::ToAac,
            container: "mp4",
            ..TranscodePlan::default()
        };
        let args = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(args.windows(2).any(|pair| pair == ["-preset", "p4"]));
        assert!(args.windows(2).any(|pair| pair == ["-cq", "22"]));
        assert!(!args.iter().any(|arg| arg == "-crf"));

        plan.hardware_decode = HardwareDecode::Cuda;
        let cuda = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(cuda.windows(2).any(|pair| pair == ["-hwaccel", "cuda"]));
        assert!(cuda
            .windows(2)
            .any(|pair| pair == ["-vf", "scale_cuda=format=yuv420p,hwdownload,format=yuv420p"]));
        assert!(!cuda.iter().any(|arg| arg == "-pix_fmt"));

        plan.browser_quality = Some(BrowserQuality::Auto);
        let uhd = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(uhd
            .iter()
            .any(|arg| arg.contains("scale_cuda=w='min(iw,3840)':h='min(ih,2160)'")));
        assert!(uhd.windows(2).any(|pair| pair == ["-cq", "20"]));
        assert!(uhd.windows(2).any(|pair| pair == ["-profile:v", "high"]));
        assert!(!uhd.iter().any(|arg| arg == "-level:v"));
        assert!(uhd.windows(2).any(|pair| pair == ["-maxrate", "25000k"]));
        assert!(uhd.windows(2).any(|pair| pair == ["-bufsize", "50000k"]));

        plan.browser_quality = Some(BrowserQuality::UhdHigh);
        let uhd_high = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(uhd_high
            .windows(2)
            .any(|pair| pair == ["-maxrate", "25000k"]));
        assert!(uhd_high
            .windows(2)
            .any(|pair| pair == ["-bufsize", "50000k"]));

        plan.browser_quality = Some(BrowserQuality::UhdOptimized);
        let uhd_optimized = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(uhd_optimized
            .windows(2)
            .any(|pair| pair == ["-maxrate", "16000k"]));

        plan.browser_quality = Some(BrowserQuality::FullHd);
        let full_hd = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(full_hd
            .iter()
            .any(|arg| arg.contains("scale_cuda=w='min(iw,1920)':h='min(ih,1080)'")));
        assert!(full_hd.windows(2).any(|pair| pair == ["-level:v", "4.1"]));
        assert!(full_hd.windows(2).any(|pair| pair == ["-maxrate", "8000k"]));

        plan.browser_quality = Some(BrowserQuality::DataSaver);
        let saver = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(saver
            .iter()
            .any(|arg| arg.contains("scale_cuda=w='min(iw,1280)':h='min(ih,720)'")));
        assert!(saver
            .windows(2)
            .any(|pair| pair == ["-profile:v", "baseline"]));
        assert!(saver.windows(2).any(|pair| pair == ["-level:v", "3.1"]));
        assert!(saver.windows(2).any(|pair| pair == ["-bf", "0"]));
        assert!(saver.windows(2).any(|pair| pair == ["-refs", "1"]));
        assert!(saver.windows(2).any(|pair| pair == ["-maxrate", "3000k"]));
        assert!(saver.windows(2).any(|pair| pair == ["-bufsize", "6000k"]));
        assert!(saver.windows(2).any(|pair| pair == ["-fpsmax", "30"]));
        assert!(saver.windows(2).any(|pair| pair == ["-b:a", "128k"]));
        assert!(saver.windows(2).any(|pair| pair == ["-ac", "2"]));

        plan.browser_quality = Some(BrowserQuality::Sd480);
        let sd = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(sd
            .iter()
            .any(|arg| arg.contains("scale_cuda=w='min(iw,854)':h='min(ih,480)'")));
        assert!(sd.windows(2).any(|pair| pair == ["-maxrate", "1500k"]));

        plan.browser_quality = Some(BrowserQuality::Low360);
        let low = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(low
            .iter()
            .any(|arg| arg.contains("scale_cuda=w='min(iw,640)':h='min(ih,360)'")));
        assert!(low.windows(2).any(|pair| pair == ["-level:v", "3.0"]));
        assert!(low.windows(2).any(|pair| pair == ["-maxrate", "800k"]));
        assert!(low.windows(2).any(|pair| pair == ["-b:a", "96k"]));

        plan.video_encoder = "hevc_nvenc".into();
        plan.hardware_decode = HardwareDecode::Cuda;
        plan.browser_quality = Some(BrowserQuality::Auto);
        plan.keep_hdr10 = true;
        let repaired_hevc = ffmpeg_grow_args("source.mp4", "output.mp4.part", &plan);
        assert!(repaired_hevc
            .iter()
            .any(|arg| arg.contains("format=p010le")));
        assert!(repaired_hevc.windows(2).any(|pair| pair == ["-cq", "18"]));
        assert!(repaired_hevc
            .windows(2)
            .any(|pair| pair == ["-profile:v", "main10"]));
        assert!(repaired_hevc
            .windows(2)
            .any(|pair| pair == ["-color_trc", "smpte2084"]));
        assert!(!repaired_hevc
            .windows(2)
            .any(|pair| pair == ["-profile:v", "high"]));
    }

    #[test]
    fn browser_repair_encoder_preserves_only_supported_hevc_hdr_formats() {
        for (video, hdr, bit_depth, expected) in [
            (VideoCodec::H264, HdrKind::Hdr10, 10, "h264_nvenc"),
            (VideoCodec::Hevc, HdrKind::Hdr10, 8, "h264_nvenc"),
            (VideoCodec::Hevc, HdrKind::Hdr10, 10, "hevc_nvenc"),
            (
                VideoCodec::Hevc,
                HdrKind::DolbyVisionProfile8,
                10,
                "hevc_nvenc",
            ),
            (
                VideoCodec::Hevc,
                HdrKind::DolbyVisionProfile5,
                10,
                "h264_nvenc",
            ),
            (
                VideoCodec::Hevc,
                HdrKind::DolbyVisionProfile7,
                10,
                "h264_nvenc",
            ),
            (
                VideoCodec::Hevc,
                HdrKind::DolbyVisionOther,
                10,
                "h264_nvenc",
            ),
        ] {
            assert_eq!(
                browser_repair_video_encoder("h264_nvenc", video, hdr, bit_depth),
                expected,
                "{video:?} {hdr:?} {bit_depth}-bit"
            );
            assert_eq!(
                browser_repair_video_encoder("libx264", video, hdr, bit_depth),
                "libx264"
            );
        }
    }

    #[test]
    fn browser_dolby_vision_repair_uses_real_sdr_tonemap_policy() {
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::Hevc),
            selected_audio: AudioCodec::Aac,
            source_hdr: HdrKind::DolbyVisionProfile7,
            start_seconds: 0,
            hls: false,
        };
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            keep_hdr10: false,
            drop_dolby_vision: true,
            video_encoder: browser_repair_video_encoder(
                "h264_nvenc",
                VideoCodec::Hevc,
                options.source_hdr,
                10,
            )
            .into(),
            hardware_decode: HardwareDecode::Cuda,
            audio: AudioAction::Copy,
            container: "mp4",
            browser_quality: Some(BrowserQuality::Auto),
            ..TranscodePlan::default()
        };

        let args = browser_ffmpeg_os_args(
            Path::new("source.mp4"),
            Path::new("output.mp4.part"),
            &plan,
            options,
        );

        assert!(browser_cache_uses_sdr_tonemap_revision(&plan, options));
        assert!(args
            .iter()
            .any(|argument| argument.to_string_lossy().contains("libplacebo")));
        assert!(args.windows(2).any(|pair| pair == ["-color_trc", "bt709"]));
    }

    #[test]
    fn browser_output_argv_policy_snapshot() {
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            video_encoder: "libx264".into(),
            audio: AudioAction::Copy,
            container: "mp4",
            audio_index: 1,
            browser_quality: Some(BrowserQuality::FullHd),
            ..TranscodePlan::default()
        };
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::Hevc),
            selected_audio: browser_audio_codec_from_name("AAC"),
            source_hdr: HdrKind::DolbyVisionProfile7,
            start_seconds: 30,
            hls: false,
        };
        let args = browser_ffmpeg_os_args(
            Path::new("source.mkv"),
            Path::new("output.mp4.part"),
            &plan,
            options,
        )
        .into_iter()
        .map(|argument| argument.into_string().unwrap())
        .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "ffmpeg",
                "-hide_banner",
                "-nostats",
                "-y",
                "-nostdin",
                "-init_hw_device",
                "vulkan=vk:0",
                "-filter_hw_device",
                "vk",
                "-ss",
                "25",
                "-i",
                "source.mkv",
                "-ss",
                "5",
                "-map",
                "0:v:0?",
                "-map",
                "0:a:1?",
                "-map_chapters",
                "-1",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "22",
                "-pix_fmt",
                "yuv420p",
                "-vf",
                concat!(
                    "format=yuv420p10le,hwupload,",
                    "libplacebo=apply_dolbyvision=true:colorspace=bt709:",
                    "color_primaries=bt709:color_trc=bt709:range=tv:",
                    "tonemapping=bt.2390:format=yuv420p:",
                    "w='min(iw,1920)':h='min(ih,1080)':",
                    "force_original_aspect_ratio=decrease:",
                    "force_divisible_by=2,",
                    "hwdownload,format=yuv420p"
                ),
                "-maxrate",
                "8000k",
                "-bufsize",
                "16000k",
                "-fpsmax",
                "30",
                "-profile:v",
                "high",
                "-level:v",
                "4.1",
                "-force_key_frames",
                "expr:gte(t,n_forced*2)",
                "-c:a",
                "copy",
                "-avoid_negative_ts",
                "make_zero",
                "-flush_packets",
                "1",
                "-frag_duration",
                "1000000",
                "-f",
                "mp4",
                "-movflags",
                "frag_keyframe+empty_moov+delay_moov+default_base_moof",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
                "-bsf:a",
                "aac_adtstoasc",
                "output.mp4.part",
            ]
        );
    }

    #[test]
    fn browser_ai_upscale_uses_the_descriptor_shader_and_owns_cache_identity() {
        let mut plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            video_encoder: "h264_nvenc".into(),
            audio: AudioAction::ToAac,
            container: "mp4",
            browser_quality: Some(BrowserQuality::FullHd),
            browser_ai_upscale: Some(BrowserAiUpscale {
                model: "fsrcnnx-16".into(),
                shader_sha256: "a".repeat(64),
            }),
            ..TranscodePlan::default()
        };
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::H264),
            selected_audio: AudioCodec::Aac,
            source_hdr: HdrKind::Sdr,
            start_seconds: 0,
            hls: true,
        };
        let args = browser_ffmpeg_os_args(
            Path::new("source.mp4"),
            Path::new("output.mp4.part"),
            &plan,
            options,
        )
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-init_hw_device", "vulkan=vk:0"]));
        let filter = args
            .windows(2)
            .find(|pair| pair[0] == "-vf")
            .map(|pair| pair[1].as_str())
            .expect("AI filter");
        assert!(filter.contains(&format!(
            "custom_shader_path={BROWSER_AI_UPSCALE_SHADER_CHILD_PATH}"
        )));
        assert!(filter.contains("w=1920:h=1080"));
        assert!(!filter.contains("min(iw"));

        plan.hardware_decode = HardwareDecode::Cuda;
        let cuda_args = browser_ffmpeg_os_args(
            Path::new("source.mp4"),
            Path::new("output.mp4.part"),
            &plan,
            options,
        );
        let cuda_filter = cuda_args
            .windows(2)
            .find(|pair| pair[0] == "-vf")
            .map(|pair| pair[1].to_string_lossy())
            .expect("CUDA AI filter");
        assert!(cuda_filter.starts_with("hwdownload,format=nv12,hwupload,"));
        assert!(!cuda_filter.contains("hwdownload,format=yuv420p,hwupload"));

        let first = browser_cache_key_from_base("base".into(), &plan, options);
        assert!(first.contains(BROWSER_AI_UPSCALE_CACHE_REVISION));
        assert_eq!(first.rsplit_once('-').unwrap().1.len(), 64);
        plan.browser_ai_upscale.as_mut().unwrap().shader_sha256 = "b".repeat(64);
        let second = browser_cache_key_from_base("base".into(), &plan, options);
        assert_ne!(first, second);
    }

    #[test]
    fn browser_encoding_presets_preserve_stream_policy_and_own_cache_identity() {
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::Hevc),
            selected_audio: AudioCodec::Aac,
            source_hdr: HdrKind::Hdr10,
            start_seconds: 30,
            hls: true,
        };
        for encoder in ["h264_nvenc", "hevc_nvenc", "libx264", "copy"] {
            let plan = TranscodePlan {
                action: RecodeAction::Browser,
                video_encoder: encoder.into(),
                browser_quality: Some(BrowserQuality::FullHd),
                hardware_decode: if encoder.contains("nvenc") {
                    HardwareDecode::Cuda
                } else {
                    HardwareDecode::None
                },
                keep_hdr10: encoder == "hevc_nvenc",
                audio: AudioAction::Copy,
                ..TranscodePlan::default()
            };
            let args = |options| {
                browser_ffmpeg_os_args(Path::new("source"), Path::new("output"), &plan, options)
            };
            let key = |options| browser_cache_key_from_base("base".into(), &plan, options);
            let balanced = args(options);
            let mut keys = std::collections::HashSet::new();
            for preset in BrowserEncodingPreset::ALL {
                assert_eq!(BrowserEncodingPreset::parse(preset.id()), Some(preset));
                let selected = BrowserOutputOptions {
                    encoding_preset: preset,
                    ..options
                };
                let actual = args(selected);
                keys.insert(key(selected));
                if encoder == "copy" || preset == BrowserEncodingPreset::Balanced {
                    assert_eq!(actual, balanced);
                    assert_eq!(key(selected), key(options));
                    continue;
                }
                let expected_preset = match (encoder.contains("nvenc"), preset) {
                    (true, BrowserEncodingPreset::FastStart) => "p4",
                    (true, _) => "p2",
                    (false, BrowserEncodingPreset::FastStart) => "veryfast",
                    (false, _) => "ultrafast",
                };
                assert!(actual
                    .windows(2)
                    .any(|pair| pair == ["-preset", expected_preset]));
                assert!(actual.windows(2).any(|pair| pair
                    == [
                        "-tune",
                        if encoder.contains("nvenc") {
                            "ll"
                        } else {
                            "zerolatency"
                        }
                    ]));
                assert!(actual.windows(2).any(|pair| pair == ["-bf", "0"]));
                for flag in [
                    "-vf",
                    "-c:v",
                    "-c:a",
                    "-ss",
                    "-maxrate",
                    "-bufsize",
                    "-force_key_frames",
                    "-color_trc",
                    "-tag:v",
                ] {
                    let value = |values: &Vec<OsString>| {
                        values
                            .windows(2)
                            .find(|pair| pair[0] == flag)
                            .map(|pair| pair[1].clone())
                    };
                    assert_eq!(
                        value(&actual),
                        value(&balanced),
                        "{encoder} {preset:?} {flag}"
                    );
                }
            }
            assert_eq!(keys.len(), if encoder == "copy" { 1 } else { 3 });
            let audio_only = BrowserOutputOptions {
                source_video: None,
                ..options
            };
            assert_eq!(
                args(audio_only),
                args(BrowserOutputOptions {
                    encoding_preset: BrowserEncodingPreset::FastStart,
                    ..audio_only
                })
            );
        }
        assert_eq!(BrowserEncodingPreset::parse("unknown"), None);
    }

    #[test]
    fn browser_encoding_presets_produce_decodable_monotonic_fragments() {
        let tmp = tool_test_dir("encoding-presets");
        let source = tmp.join("source.mkv");
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let deadline = || std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut generate: Vec<OsString> = [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "3",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "24",
            "-c:a",
            "aac",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        generate.push(source.as_os_str().to_owned());
        run_cmd_controlled(&generate, deadline(), &cancelled, None).unwrap();
        let mut encoders = vec!["libx264"];
        if std::env::var("RUSTY_DLNA_TEST_NVENC").as_deref() == Ok("1") {
            encoders.extend(["h264_nvenc", "hevc_nvenc"]);
        }
        for encoder in encoders {
            for preset in BrowserEncodingPreset::ALL {
                let output = tmp.join(format!("{encoder}-{}.mp4", preset.id()));
                let plan = TranscodePlan {
                    action: RecodeAction::Browser,
                    video_encoder: encoder.into(),
                    audio: AudioAction::ToAac,
                    browser_quality: Some(BrowserQuality::FullHd),
                    ..TranscodePlan::default()
                };
                let options = BrowserOutputOptions {
                    encoding_preset: preset,
                    source_video: Some(VideoCodec::H264),
                    selected_audio: AudioCodec::Aac,
                    source_hdr: HdrKind::Sdr,
                    start_seconds: 0,
                    hls: true,
                };
                // Exercise portable pacing too: contributor FFmpeg may predate 8.
                let args = browser_ffmpeg_os_args_with_readrate_catchup(
                    &source, &output, &plan, options, false,
                );
                run_cmd_controlled(&args, deadline(), &cancelled, None).unwrap();
                let bytes = std::fs::read(&output).unwrap();
                assert!(bytes.windows(4).any(|word| word == b"moof"));
                let mut probe: Vec<OsString> = [
                    "ffprobe",
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "packet=dts_time,flags",
                    "-of",
                    "csv=p=0",
                ]
                .into_iter()
                .map(OsString::from)
                .collect();
                probe.push(output.as_os_str().to_owned());
                let probe_output =
                    run_cmd_capture_controlled(&probe, deadline(), &cancelled, None).unwrap();
                let text = String::from_utf8(probe_output).unwrap();
                let packets: Vec<_> = text.lines().filter(|line| !line.is_empty()).collect();
                assert!(!packets.is_empty());
                assert!(packets[0].split(',').nth(1).unwrap().contains('K'));
                let times: Vec<f64> = packets
                    .iter()
                    .map(|packet| packet.split(',').next().unwrap().parse().unwrap())
                    .collect();
                assert!(times.windows(2).all(|pair| pair[1] > pair[0]));
                let decode = vec![
                    "ffmpeg".into(),
                    "-v".into(),
                    "error".into(),
                    "-i".into(),
                    output.into_os_string(),
                    "-f".into(),
                    "null".into(),
                    "-".into(),
                ];
                run_cmd_controlled(&decode, deadline(), &cancelled, None).unwrap();
            }
        }
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn browser_copy_options_add_aac_and_hevc_mp4_signaling() {
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            video_encoder: "copy".into(),
            audio: AudioAction::Copy,
            container: "mp4",
            browser_quality: Some(BrowserQuality::Auto),
            ..TranscodePlan::default()
        };
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::Hevc),
            selected_audio: AudioCodec::Aac,
            source_hdr: HdrKind::DolbyVisionProfile8,
            start_seconds: 0,
            hls: false,
        };

        let args = browser_ffmpeg_os_args(
            Path::new("source.mp4"),
            Path::new("output.mp4.part"),
            &plan,
            options,
        );

        assert_eq!(
            &args[args.len() - 5..],
            [
                "-bsf:a",
                "aac_adtstoasc",
                "-tag:v",
                "hvc1",
                "output.mp4.part"
            ]
        );
        assert!(!args.iter().any(|argument| argument == "-init_hw_device"));
    }

    #[test]
    fn browser_cache_option_key_revisions_are_stable() {
        let mut plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            keep_hdr10: false,
            video_encoder: "libx264".into(),
            audio: AudioAction::ToAac,
            container: "mp4",
            browser_quality: Some(BrowserQuality::Auto),
            ..TranscodePlan::default()
        };
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::H264),
            selected_audio: AudioCodec::TrueHd,
            source_hdr: HdrKind::DolbyVisionProfile7,
            start_seconds: 120,
            hls: false,
        };

        assert_eq!(
            browser_cache_key_from_base("base".into(), &plan, options),
            concat!(
                "base-aligned-seek-v2-browser-no-chapters-v1-",
                "sdr-tonemap-libplacebo-v2-",
                "browser-hdr-source-v1-browser-adaptive-h264-level-v1-start-120"
            )
        );
        plan.video_encoder = "copy".into();
        assert_eq!(
            browser_cache_key_from_base("base".into(), &plan, options),
            concat!(
                "base-aligned-seek-v2-browser-no-chapters-v1-",
                "browser-hdr-source-v1-",
                "browser-mixed-copy-seek-v2-start-120"
            )
        );
        plan.video_encoder = "hevc_nvenc".into();
        assert_eq!(
            browser_cache_key_from_base("base".into(), &plan, options),
            concat!(
                "base-aligned-seek-v2-browser-no-chapters-v1-",
                "browser-hdr-source-v1-browser-nvenc-idr-v1-start-120"
            )
        );
        plan.hardware_decode = HardwareDecode::Cuda;
        assert_eq!(
            browser_cache_key_from_base("base".into(), &plan, options),
            concat!(
                "base-aligned-seek-v2-browser-no-chapters-v1-",
                "browser-hdr-source-v1-browser-cuda-download-v1-",
                "browser-nvenc-idr-v1-start-120"
            )
        );
        plan.hardware_decode = HardwareDecode::None;
        let sdr = BrowserOutputOptions {
            source_hdr: HdrKind::Sdr,
            start_seconds: 0,
            ..options
        };
        assert_eq!(
            browser_cache_key_from_base("base".into(), &plan, sdr),
            concat!(
                "base-aligned-seek-v2-browser-no-chapters-v1-",
                "browser-nvenc-idr-v1"
            )
        );
        let mse = BrowserOutputOptions { hls: true, ..sdr };
        assert_eq!(
            browser_cache_key_from_base("base".into(), &plan, mse),
            concat!(
                "base-aligned-seek-v2-browser-no-chapters-v1-",
                "browser-nvenc-idr-v1-browser-hls-v1"
            )
        );
        let mse_args = browser_ffmpeg_os_args(
            Path::new("source.mkv"),
            Path::new("output.mp4.part"),
            &plan,
            mse,
        );
        assert!(mse_args
            .windows(2)
            .any(|pair| pair == ["-force_key_frames", "expr:gte(t,n_forced*1)"]));
        assert!(mse_args.windows(2).any(|pair| pair == ["-readrate", "1"]));
        assert!(mse_args
            .windows(2)
            .any(|pair| pair == ["-readrate_initial_burst", "30"]));
        assert!(mse_args
            .windows(2)
            .any(|pair| pair == ["-readrate_catchup", "100"]));
        let legacy_mse_args = browser_ffmpeg_os_args_with_readrate_catchup(
            Path::new("source.mkv"),
            Path::new("output.mp4.part"),
            &plan,
            mse,
            false,
        );
        assert!(legacy_mse_args
            .windows(2)
            .any(|pair| pair == ["-readrate_initial_burst", "30"]));
        assert!(!legacy_mse_args
            .iter()
            .any(|argument| argument == "-readrate_catchup"));
        let mut copy_plan = plan.clone();
        copy_plan.video_encoder = "copy".into();
        let mixed_hls_args = browser_ffmpeg_os_args(
            Path::new("source.mkv"),
            Path::new("output.mp4.part"),
            &copy_plan,
            mse,
        );
        assert!(mixed_hls_args
            .windows(2)
            .any(|pair| pair == ["-readrate", "1"]));
        assert!(mixed_hls_args
            .windows(2)
            .any(|pair| pair == ["-readrate_catchup", "100"]));
        copy_plan.audio = AudioAction::Copy;
        let copied_hls_args = browser_ffmpeg_os_args(
            Path::new("source.mkv"),
            Path::new("output.mp4.part"),
            &copy_plan,
            mse,
        );
        assert!(!copied_hls_args.iter().any(|arg| arg == "-readrate"));
        assert!(!copied_hls_args.iter().any(|arg| arg == "-readrate_catchup"));

        plan.video_encoder = "h264_nvenc".into();
        plan.browser_quality = Some(BrowserQuality::DataSaver);
        assert_eq!(
            browser_cache_key_from_base("base".into(), &plan, sdr),
            concat!(
                "base-aligned-seek-v2-browser-no-chapters-v1-",
                "browser-nvenc-idr-v1-browser-data-saver-baseline-v1"
            )
        );
    }

    #[test]
    fn ffmpeg_readrate_catchup_requires_a_known_version_eight_or_newer() {
        assert_eq!(
            ffmpeg_major_version("ffmpeg version 6.1.1-3ubuntu5|identity"),
            Some(6)
        );
        assert_eq!(
            ffmpeg_major_version("ffmpeg version 8.0.1 Copyright (c) FFmpeg|identity"),
            Some(8)
        );
        assert_eq!(
            ffmpeg_major_version("ffmpeg version n9.2-static|identity"),
            Some(9)
        );
        assert_eq!(
            ffmpeg_major_version("ffmpeg version N-119831-gabcdef|identity"),
            None
        );
        assert_eq!(ffmpeg_major_version("vendor wrapper|identity"), None);
    }

    #[test]
    fn cache_key_shape_accepts_bounded_browser_revisions_only() {
        let digest = "a".repeat(CACHE_DIGEST_HEX_BYTES);
        assert!(!cache_key_has_safe_shape(
            RecodeAction::Hdr10,
            &"a".repeat(40)
        ));
        for action in [
            RecodeAction::Original,
            RecodeAction::RemuxP8,
            RecodeAction::Hdr10,
            RecodeAction::AudioAc3,
        ] {
            assert!(cache_key_has_safe_shape(action, &digest));
            assert!(!cache_key_has_safe_shape(
                action,
                &format!("{digest}-aligned-seek-v2")
            ));
        }

        for suffix in [
            "",
            "-timeline-zero-v1",
            concat!(
                "-aligned-seek-v2-browser-no-chapters-v1-",
                "sdr-tonemap-libplacebo-v2-",
                "browser-hdr-source-v1-browser-aac-adtstoasc-v1-",
                "browser-mixed-copy-seek-v2-start-120"
            ),
        ] {
            assert!(cache_key_has_safe_shape(
                RecodeAction::Browser,
                &format!("{digest}{suffix}")
            ));
        }

        for invalid in [
            "short".to_owned(),
            format!("g{digest}"),
            format!("{digest}-"),
            format!("{digest}-two--hyphens"),
            format!("{digest}-UPPER"),
            format!("{digest}-unsafe_name"),
            format!("{digest}-{}", "a".repeat(MAX_BROWSER_CACHE_KEY_BYTES)),
        ] {
            assert!(!cache_key_has_safe_shape(RecodeAction::Browser, &invalid));
        }
    }

    #[test]
    fn browser_cache_destination_hashes_full_key_into_a_bounded_filename() {
        let cache_key = concat!(
            "dfa68335bfd3d1fd5acab192100de1eb7c1b0d4c0dbf9903cb02195b1767213d",
            "-aligned-seek-v2-browser-no-chapters-v1-sdr-tonemap-libplacebo-v2-",
            "browser-hdr-source-v1-browser-cuda-download-v1-browser-nvenc-idr-v1-",
            "browser-data-saver-baseline-v1-browser-hls-v1"
        );
        let destination = cache_dest_for_key(
            Path::new("/var/cache/rusty-dlna"),
            5,
            RecodeAction::Browser,
            cache_key,
        );
        let part = cache_part(&destination);
        let name = destination.file_name().unwrap().to_str().unwrap();
        let part_name = part.file_name().unwrap().to_str().unwrap();

        assert_eq!(name.len(), "5-web-.mp4".len() + CACHE_DIGEST_HEX_BYTES);
        assert!(part_name.len() <= 255, "{part_name}");
        assert_ne!(
            destination,
            cache_dest_for_key(
                Path::new("/var/cache/rusty-dlna"),
                5,
                RecodeAction::Browser,
                &format!("{cache_key}-start-120"),
            ),
            "every full-key revision must select a distinct output"
        );
    }

    #[test]
    fn browser_cache_identity_tracks_every_arg_affecting_option() {
        let encoded_plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            keep_hdr10: false,
            video_encoder: "libx264".into(),
            audio: AudioAction::ToAac,
            container: "mp4",
            browser_quality: Some(BrowserQuality::Auto),
            ..TranscodePlan::default()
        };
        let copy_plan = TranscodePlan {
            video_encoder: "copy".into(),
            audio: AudioAction::Copy,
            ..encoded_plan.clone()
        };
        let encoded_options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::H264),
            selected_audio: AudioCodec::Other,
            source_hdr: HdrKind::Sdr,
            start_seconds: 0,
            hls: false,
        };
        let copy_options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::H264),
            selected_audio: AudioCodec::Other,
            source_hdr: HdrKind::Sdr,
            start_seconds: 0,
            hls: false,
        };
        let args = |plan: &TranscodePlan, options| {
            browser_ffmpeg_os_args(Path::new("source"), Path::new("output"), plan, options)
        };
        let key = |plan: &TranscodePlan, options| {
            let base =
                transcode_cache_key_from_identity("fixed-source", plan, false, None).unwrap();
            browser_cache_key_from_base(base, plan, options)
        };

        let copied_aac = BrowserOutputOptions {
            selected_audio: AudioCodec::Aac,
            ..copy_options
        };
        assert_ne!(args(&copy_plan, copy_options), args(&copy_plan, copied_aac));
        assert_ne!(key(&copy_plan, copy_options), key(&copy_plan, copied_aac));

        let copied_hevc = BrowserOutputOptions {
            source_video: Some(VideoCodec::Hevc),
            ..copy_options
        };
        assert_ne!(
            args(&copy_plan, copy_options),
            args(&copy_plan, copied_hevc)
        );
        assert_ne!(key(&copy_plan, copy_options), key(&copy_plan, copied_hevc));

        let hdr = BrowserOutputOptions {
            source_hdr: HdrKind::Hdr10,
            ..encoded_options
        };
        assert_ne!(
            args(&encoded_plan, encoded_options),
            args(&encoded_plan, hdr)
        );
        assert_ne!(key(&encoded_plan, encoded_options), key(&encoded_plan, hdr));

        let started = BrowserOutputOptions {
            start_seconds: 30,
            ..copy_options
        };
        assert_ne!(args(&copy_plan, copy_options), args(&copy_plan, started));
        assert_ne!(key(&copy_plan, copy_options), key(&copy_plan, started));

        let mse = BrowserOutputOptions {
            hls: true,
            ..encoded_options
        };
        assert_ne!(
            args(&encoded_plan, encoded_options),
            args(&encoded_plan, mse)
        );
        assert_ne!(key(&encoded_plan, encoded_options), key(&encoded_plan, mse));

        let mixed_plan = TranscodePlan {
            audio: AudioAction::ToAac,
            ..copy_plan.clone()
        };
        let mixed_video = BrowserOutputOptions {
            start_seconds: 30,
            ..copy_options
        };
        let mixed_audio_only = BrowserOutputOptions {
            source_video: None,
            ..mixed_video
        };
        assert_ne!(
            args(&mixed_plan, mixed_video),
            args(&mixed_plan, mixed_audio_only)
        );
        assert_ne!(
            key(&mixed_plan, mixed_video),
            key(&mixed_plan, mixed_audio_only)
        );

        let full_hd_plan = TranscodePlan {
            browser_quality: Some(BrowserQuality::FullHd),
            ..encoded_plan.clone()
        };
        assert_ne!(
            args(&encoded_plan, encoded_options),
            args(&full_hd_plan, encoded_options)
        );
        assert_ne!(
            key(&encoded_plan, encoded_options),
            key(&full_hd_plan, encoded_options)
        );
    }

    #[test]
    fn browser_cache_identity_ignores_non_output_labels() {
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            rule: Some("first-label".into()),
            video_encoder: "libx264".into(),
            audio: AudioAction::Copy,
            container: "mp4",
            browser_quality: Some(BrowserQuality::Auto),
            ..TranscodePlan::default()
        };
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::H264),
            selected_audio: AudioCodec::Ac3,
            source_hdr: HdrKind::Hdr10,
            start_seconds: 0,
            hls: false,
        };
        let args = |plan: &TranscodePlan, options| {
            browser_ffmpeg_os_args(Path::new("source"), Path::new("output"), plan, options)
        };
        let key = |plan: &TranscodePlan, options| {
            let base =
                transcode_cache_key_from_identity("fixed-source", plan, false, None).unwrap();
            browser_cache_key_from_base(base, plan, options)
        };

        let relabeled_audio = BrowserOutputOptions {
            selected_audio: AudioCodec::TrueHd,
            ..options
        };
        assert_eq!(args(&plan, options), args(&plan, relabeled_audio));
        assert_eq!(key(&plan, options), key(&plan, relabeled_audio));

        let equivalent_hdr = BrowserOutputOptions {
            source_hdr: HdrKind::DolbyVisionProfile7,
            ..options
        };
        assert_eq!(args(&plan, options), args(&plan, equivalent_hdr));
        assert_eq!(key(&plan, options), key(&plan, equivalent_hdr));

        let equivalent_video = BrowserOutputOptions {
            source_video: Some(VideoCodec::Mpeg4),
            ..options
        };
        assert_eq!(args(&plan, options), args(&plan, equivalent_video));
        assert_eq!(key(&plan, options), key(&plan, equivalent_video));

        let relabeled_plan = TranscodePlan {
            decision: Decision::ServeOriginal,
            rule: Some("second-label".into()),
            ..plan.clone()
        };
        assert_eq!(args(&plan, options), args(&relabeled_plan, options));
        assert_eq!(key(&plan, options), key(&relabeled_plan, options));
    }

    #[cfg(unix)]
    #[test]
    fn growing_argv_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let src = std::path::PathBuf::from(OsString::from_vec(b"/media/movie-\x80.mkv".to_vec()));
        let dst = std::path::PathBuf::from(OsString::from_vec(b"/cache/movie-\x81.part".to_vec()));
        let args = ffmpeg_grow_os_args(&src, &dst, &TranscodePlan::default());
        let input = args.iter().position(|argument| argument == "-i").unwrap() + 1;
        assert_eq!(args[input].as_bytes(), src.as_os_str().as_bytes());
        assert_eq!(args.last().unwrap().as_bytes(), dst.as_os_str().as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn source_identity_distinguishes_lossy_colliding_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let source = std::env::temp_dir().join(format!(
            "rusty-dlna-source-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&source, b"same descriptor and bytes").unwrap();
        let file = std::fs::File::open(&source).unwrap();
        let first = std::path::PathBuf::from(OsString::from_vec(b"movie-\x80.mkv".to_vec()));
        let second = std::path::PathBuf::from(OsString::from_vec(b"movie-\x81.mkv".to_vec()));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());

        let first_identity = source_identity_file(&file, &first).unwrap();
        let second_identity = source_identity_file(&file, &second).unwrap();

        assert_ne!(first_identity, second_identity);
        std::fs::remove_file(source).unwrap();
    }

    #[test]
    fn source_identity_file_preserves_the_shared_descriptor_cursor() {
        use std::io::{Read, Seek, SeekFrom};

        let directory = tool_test_dir("source-identity-cursor");
        let source = directory.join("source.bin");
        let bytes: Vec<u8> = (0..=255).cycle().take(192 * 1024).collect();
        std::fs::write(&source, &bytes).unwrap();
        let mut file = std::fs::File::open(&source).unwrap();
        file.seek(SeekFrom::Start(137)).unwrap();
        let mut clone = file.try_clone().unwrap();

        let before = source_identity_file(&file, &source).unwrap();

        assert_eq!(file.stream_position().unwrap(), 137);
        assert_eq!(clone.stream_position().unwrap(), 137);
        let mut next = [0u8; 16];
        clone.read_exact(&mut next).unwrap();
        assert_eq!(&next, &bytes[137..153]);
        assert_eq!(before, source_identity_file(&file, &source).unwrap());
        assert_eq!(file.stream_position().unwrap(), 153);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    fn seek_read_source_identity(
        mut file: std::fs::File,
        identity_path: &std::path::Path,
    ) -> Option<String> {
        use sha2::Digest;
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;

        let metadata = file.metadata().ok()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
        let mut hasher = Sha256::new();
        hasher.update(identity_path.as_os_str().as_encoded_bytes());
        hasher.update(metadata.len().to_le_bytes());
        if let Some(modified) = modified {
            hasher.update(modified.as_secs().to_le_bytes());
            hasher.update(modified.subsec_nanos().to_le_bytes());
        }
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
        const SAMPLE: u64 = 64 * 1024;
        let mut positions = vec![
            0,
            metadata.len().saturating_div(2).saturating_sub(SAMPLE / 2),
            metadata.len().saturating_sub(SAMPLE),
        ];
        positions.sort_unstable();
        positions.dedup();
        let mut buffer = vec![0u8; SAMPLE as usize];
        for position in positions {
            file.seek(SeekFrom::Start(position)).ok()?;
            let read = file.read(&mut buffer).ok()?;
            hasher.update(position.to_le_bytes());
            hasher.update((read as u64).to_le_bytes());
            hasher.update(&buffer[..read]);
        }
        Some(lowercase_hex(&hasher.finalize()))
    }

    #[cfg(unix)]
    #[test]
    fn positioned_source_identity_matches_seek_read_digest_bytes() {
        let directory = tool_test_dir("source-identity-compat");
        let source = directory.join("source.bin");
        let bytes: Vec<u8> = (0..=250).cycle().take(333 * 1024 + 17).collect();
        std::fs::write(&source, bytes).unwrap();
        let positioned =
            source_identity_file(&std::fs::File::open(&source).unwrap(), &source).unwrap();
        let seek_read =
            seek_read_source_identity(std::fs::File::open(&source).unwrap(), &source).unwrap();
        assert_eq!(
            positioned, seek_read,
            "positioned reads must preserve the SHA-256 identity grammar"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn positioned_source_samples_match_legacy_one_read_on_short_success() {
        use sha2::Digest;

        let source: Vec<u8> = (0..=255).cycle().take(64 * 1024).collect();
        let position = 31u64;
        let mut buffer = vec![0u8; source.len()];
        let mut calls = 0usize;
        let read = read_positioned_sample(&mut buffer, position, |remaining, offset| {
            calls += 1;
            let start = usize::try_from(offset - position).unwrap();
            let len = remaining.len().min(7);
            remaining[..len].copy_from_slice(&source[start..start + len]);
            Ok(len)
        })
        .unwrap();
        assert_eq!(read, 7, "legacy sampling accepts the first successful read");
        assert_eq!(&buffer[..read], &source[..read]);
        assert_eq!(calls, 1, "a successful short read must not be filled");

        let mut positioned = source_identity_hasher(
            Path::new("/fixed/short-read.mkv"),
            source.len() as u64,
            Some((1_700_000_000, 123_456_789)),
            7,
            11,
        );
        hash_source_identity_sample(&mut positioned, position, &buffer[..read]);
        let mut seek_read = source_identity_hasher(
            Path::new("/fixed/short-read.mkv"),
            source.len() as u64,
            Some((1_700_000_000, 123_456_789)),
            7,
            11,
        );
        hash_source_identity_sample(&mut seek_read, position, &source[..7]);
        assert_eq!(
            lowercase_hex(&positioned.finalize()),
            lowercase_hex(&seek_read.finalize()),
            "positioned sampling must retain the exact digest grammar"
        );
    }

    #[test]
    fn positioned_source_samples_bound_interrupted_retries() {
        let source: Vec<u8> = (0..=255).cycle().take(64 * 1024).collect();
        let position = 31u64;
        let mut buffer = vec![0u8; source.len()];
        let mut calls = 0usize;
        let read = read_positioned_sample(&mut buffer, position, |remaining, offset| {
            calls += 1;
            if calls <= 3 {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            let start = usize::try_from(offset - position).unwrap();
            let len = remaining.len().min(7);
            remaining[..len].copy_from_slice(&source[start..start + len]);
            Ok(len)
        })
        .unwrap();
        assert_eq!(read, 7);
        assert_eq!(&buffer[..read], &source[..read]);
        assert_eq!(calls, 4);

        calls = 0;
        assert!(read_positioned_sample(&mut buffer, position, |_, _| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
        })
        .is_none());
        assert_eq!(calls, 17, "interrupted reads use the smaller retry cap");
    }

    #[test]
    fn source_identity_byte_grammar_has_a_fixed_golden_digest() {
        use sha2::Digest;

        let mut hasher = source_identity_hasher(
            Path::new("/fixed/source-identity.mkv"),
            987_654_321,
            Some((1_700_000_000, 123_456_789)),
            0x0102_0304,
            0x0506_0708,
        );
        hash_source_identity_sample(&mut hasher, 0, b"beginning");
        hash_source_identity_sample(&mut hasher, 493_794_392, b"middle");
        hash_source_identity_sample(&mut hasher, 987_588_785, b"ending");
        assert_eq!(
            lowercase_hex(&hasher.finalize()),
            "df17d7667b296fa6e811b8c43395d7026ffd8068442b33f1e78dc6ab9c58c4ff"
        );
    }

    #[test]
    fn concurrent_source_identity_calls_do_not_disturb_streaming_cursor() {
        use std::io::{Read, Seek, SeekFrom};

        let directory = tool_test_dir("source-identity-concurrent");
        let source = directory.join("source.bin");
        let bytes: Vec<u8> = (0..=255).cycle().take(512 * 1024).collect();
        std::fs::write(&source, &bytes).unwrap();
        let mut file = std::fs::File::open(&source).unwrap();
        file.seek(SeekFrom::Start(211)).unwrap();
        let mut reader = file.try_clone().unwrap();
        let file = std::sync::Arc::new(file);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let identities = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let file = std::sync::Arc::clone(&file);
                let barrier = std::sync::Arc::clone(&barrier);
                let source = source.clone();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    source_identity_file(&file, &source).unwrap()
                }));
            }
            barrier.wait();
            let mut streamed = Vec::new();
            reader.read_to_end(&mut streamed).unwrap();
            assert_eq!(streamed, bytes[211..]);
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
        let mut position_probe = file.try_clone().unwrap();
        assert_eq!(
            position_probe.stream_position().unwrap(),
            bytes.len() as u64
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn profile8_signaling_inserts_exact_record_and_updates_ancestor_sizes() {
        let tmp = tool_test_dir("profile8-signaling");
        let path = tmp.join("video-only.mp4");
        let mut initial = test_mp4_box(b"ftyp", b"isom");
        initial.extend(test_profile8_moov());
        initial.extend(test_mp4_box(b"free", b"tail"));
        std::fs::write(&path, &initial).unwrap();

        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut before_file = std::fs::File::open(&path).unwrap();
        let before = {
            let mut observer = || Ok(());
            let mut control = RemuxP8Control {
                deadline,
                cancelled: &cancelled,
                observer: &mut observer,
            };
            profile8_sample_entry_path(&mut before_file, initial.len() as u64, &mut control)
                .unwrap()
        };
        let insertion = before.last().unwrap().end() as usize;

        signal_profile8_in_mp4(&path, 6, deadline, &cancelled).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), initial.len() + 32);
        assert_eq!(
            &bytes[insertion..insertion + 32],
            &expected_profile8_dvvc(6)
        );
        let mut after_file = std::fs::File::open(&path).unwrap();
        let mut observer = || Ok(());
        let mut control = RemuxP8Control {
            deadline,
            cancelled: &cancelled,
            observer: &mut observer,
        };
        let after =
            profile8_sample_entry_path(&mut after_file, bytes.len() as u64, &mut control).unwrap();
        assert_eq!(before.len(), after.len());
        for (before, after) in before.iter().zip(&after) {
            assert_eq!(after.offset, before.offset);
            assert_eq!(after.size, before.size + 32);
        }
        assert!(entry_has_dolby_vision_configuration(
            &mut after_file,
            *after.last().unwrap(),
            &mut control,
        )
        .unwrap());

        let duplicate_len = bytes.len();
        assert!(signal_profile8_in_mp4(&path, 6, deadline, &cancelled)
            .unwrap_err()
            .contains("already has Dolby Vision signaling"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            duplicate_len as u64
        );
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn profile8_signaling_scans_sparse_media_sized_payload_with_bounded_memory() {
        use std::io::{Seek, SeekFrom, Write};

        const SPARSE_MDAT_BYTES: u64 = 512 * 1024 * 1024;
        let tmp = tool_test_dir("profile8-sparse-scan");
        let path = tmp.join("video-only.mp4");
        let moov = test_profile8_moov();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&(SPARSE_MDAT_BYTES as u32).to_be_bytes())
            .unwrap();
        file.write_all(b"mdat").unwrap();
        file.set_len(SPARSE_MDAT_BYTES).unwrap();
        file.seek(SeekFrom::Start(SPARSE_MDAT_BYTES)).unwrap();
        file.write_all(&moov).unwrap();
        drop(file);

        let cancelled = std::sync::atomic::AtomicBool::new(false);
        signal_profile8_in_mp4(
            &path,
            6,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            &cancelled,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            SPARSE_MDAT_BYTES + moov.len() as u64 + 32
        );
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn profile8_signaling_rejects_media_data_after_moov_without_mutation() {
        let tmp = tool_test_dir("profile8-faststart-layout");
        let path = tmp.join("video-only.mp4");
        let mut initial = test_profile8_moov();
        initial.extend(test_mp4_box(b"mdat", b"media bytes"));
        std::fs::write(&path, &initial).unwrap();

        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let error = signal_profile8_in_mp4(
            &path,
            6,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            &cancelled,
        )
        .unwrap_err();
        assert!(
            error.contains("media data must precede the moov"),
            "{error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn profile8_signaling_observes_cancellation_while_moving_large_tail() {
        use std::io::Write;
        use std::sync::atomic::Ordering;

        const SPARSE_TAIL_BYTES: u64 = 512 * 1024 * 1024;
        let tmp = tool_test_dir("profile8-cancel-move");
        let path = tmp.join("video-only.mp4");
        let moov = test_profile8_moov();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&moov).unwrap();
        file.write_all(&(SPARSE_TAIL_BYTES as u32).to_be_bytes())
            .unwrap();
        file.write_all(b"free").unwrap();
        let original_len = moov.len() as u64 + SPARSE_TAIL_BYTES;
        file.set_len(original_len).unwrap();
        drop(file);

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observer_cancelled = cancelled.clone();
        let observer_path = path.clone();
        let observer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if std::fs::metadata(&observer_path)
                    .map(|metadata| metadata.len() > original_len)
                    .unwrap_or(false)
                {
                    observer_cancelled.store(true, Ordering::Release);
                    return true;
                }
                std::thread::yield_now();
            }
            false
        });
        let error = signal_profile8_in_mp4(
            &path,
            6,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            &cancelled,
        )
        .unwrap_err();
        assert!(
            observer.join().unwrap(),
            "did not observe staging-file growth"
        );
        assert!(error.contains("cancelled"), "{error}");
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn profile8_signaling_large_tail_respects_hard_deadline() {
        use std::io::Write;

        const SPARSE_TAIL_BYTES: u64 = 512 * 1024 * 1024;
        let tmp = tool_test_dir("profile8-deadline-move");
        let path = tmp.join("video-only.mp4");
        let moov = test_profile8_moov();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&moov).unwrap();
        file.write_all(&(SPARSE_TAIL_BYTES as u32).to_be_bytes())
            .unwrap();
        file.write_all(b"free").unwrap();
        file.set_len(moov.len() as u64 + SPARSE_TAIL_BYTES).unwrap();
        drop(file);

        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let started = std::time::Instant::now();
        let error = signal_profile8_in_mp4(
            &path,
            6,
            started + std::time::Duration::from_millis(10),
            &cancelled,
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn grow_file_emits_ftyp_before_process_exits() {
        use std::process::{Command, Stdio};
        if Command::new("ffmpeg")
            .args(["-nostdin", "-version"])
            .stdin(Stdio::null())
            .output()
            .is_err()
        {
            eprintln!("skip grow file (no ffmpeg)");
            return;
        }
        let src = std::env::temp_dir().join(format!("rdlna-grow-src-{}.mkv", std::process::id()));
        let dest =
            std::env::temp_dir().join(format!("rdlna-grow-dst-{}.mp4.part", std::process::id()));
        let mk = Command::new("ffmpeg")
            .args([
                "-y",
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=6:size=160x90:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                src.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .status();
        if !mk.map(|s| s.success()).unwrap_or(false) {
            let _ = std::fs::remove_file(&src);
            eprintln!("skip grow file (could not make fixture)");
            return;
        }
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::RemuxP8,
            video_encoder: "copy".into(),
            audio: AudioAction::ToAac,
            container: "mp4",
            ..TranscodePlan::default()
        };
        let args = ffmpeg_grow_args(&src.to_string_lossy(), &dest.to_string_lossy(), &plan);
        let mut child = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn grow ffmpeg");
        let t0 = std::time::Instant::now();
        let mut got = Vec::new();
        while t0.elapsed() < std::time::Duration::from_secs(5) {
            if let Ok(b) = std::fs::read(&dest) {
                if b.len() >= 8 {
                    got = b;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
        assert!(got.len() >= 8, "expected growing fMP4 bytes");
        assert_eq!(&got[4..8], b"ftyp", "first box must be ftyp");
    }

    #[cfg(unix)]
    #[test]
    fn copied_aac_seek_starts_with_encoded_video() {
        use std::process::{Command, Stdio};

        for tool in ["ffmpeg", "ffprobe"] {
            if Command::new(tool)
                .args(["-version"])
                .stdin(Stdio::null())
                .output()
                .is_err()
            {
                eprintln!("skip mixed seek timeline ({tool} unavailable)");
                return;
            }
        }
        let tmp = tool_test_dir("mixed-seek-timeline");
        let source = tmp.join("source.mp4");
        let output = tmp.join("output.mp4.part");
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=8:size=160x90:rate=24000/1001",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=8",
                "-c:v",
                "libx264",
                "-g",
                "12",
                "-keyint_min",
                "12",
                "-sc_threshold",
                "0",
                "-bf",
                "0",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(&source)
            .stdin(Stdio::null())
            .output()
            .expect("generate mixed-seek fixture");
        assert!(
            generated.status.success(),
            "fixture generation failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            video_encoder: "libx264".into(),
            audio: AudioAction::Copy,
            container: "mp4",
            browser_quality: Some(BrowserQuality::DataSaver),
            ..TranscodePlan::default()
        };
        let options = BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::Mpeg4),
            selected_audio: AudioCodec::Aac,
            source_hdr: HdrKind::Sdr,
            start_seconds: 3,
            hls: false,
        };
        let args = browser_ffmpeg_os_args(&source, &output, &plan, options);
        let converted = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .output()
            .expect("run mixed copied-audio seek");
        assert!(
            converted.status.success(),
            "mixed seek failed: {}",
            String::from_utf8_lossy(&converted.stderr)
        );

        let probed = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=index,codec_type,start_time",
                "-of",
                "csv=p=0",
            ])
            .arg(&output)
            .stdin(Stdio::null())
            .output()
            .expect("probe mixed seek timeline");
        assert!(
            probed.status.success(),
            "mixed seek probe failed: {}",
            String::from_utf8_lossy(&probed.stderr)
        );
        let streams = String::from_utf8_lossy(&probed.stdout);
        let start = |kind: &str| {
            streams.lines().find_map(|line| {
                let mut fields = line.split(',');
                let _index = fields.next()?;
                (fields.next()? == kind)
                    .then(|| fields.next()?.parse::<f64>().ok())
                    .flatten()
            })
        };
        let video_start = start("video").expect("encoded video start");
        let audio_start = start("audio").expect("copied audio start");
        assert!(
            (video_start - audio_start).abs() <= 0.05,
            "mixed output starts out of sync: video={video_start} audio={audio_start}\n{streams}"
        );
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn genuine_truehd_audio_remap_produces_probeable_fragmented_ac3() {
        use std::process::{Command, Stdio};

        let tmp = std::env::temp_dir().join(format!(
            "rdlna-truehd-remap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/generate-advanced-fixtures.sh");
        let generated = Command::new(script)
            .arg(&tmp)
            .stdin(Stdio::null())
            .output()
            .expect("run advanced fixture generator");
        assert!(
            generated.status.success(),
            "advanced fixture generation failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&generated.stdout),
            String::from_utf8_lossy(&generated.stderr)
        );

        let source = probe_to_source("mkv", "h264", "sdr", "truehd", 64, 36);
        let rules = parse_remaps_toml(
            r#"
[[remap]]
name = "genuine-truehd"
client = "CrKey"
audio = "truehd"
action = "audio-ac3"
encoder = "copy"
"#,
        )
        .unwrap();
        let mut plan = decide_ua("CrKey/1.54", &source, &rules);
        plan.audio_index = pick_audio_index_from_streams("1:0:truehd:6", "truehd");
        assert_eq!(plan.decision, Decision::Recode);
        assert_eq!(plan.action, RecodeAction::AudioAc3);
        assert_eq!(plan.audio, AudioAction::ToAc3);

        let input = tmp.join("truehd.mkv");
        let output = tmp.join("truehd-ac3.mp4.part");
        let args = ffmpeg_grow_os_args(&input, &output, &plan);
        let converted = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .output()
            .expect("run genuine TrueHD audio remap");
        assert!(
            converted.status.success(),
            "TrueHD remap failed: {}",
            String::from_utf8_lossy(&converted.stderr)
        );
        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes.windows(4).any(|window| window == b"moof"));

        let probed = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,codec_name",
                "-of",
                "csv=p=0",
            ])
            .arg(&output)
            .stdin(Stdio::null())
            .output()
            .expect("probe remapped fMP4");
        assert!(
            probed.status.success(),
            "ffprobe rejected remapped output: {}",
            String::from_utf8_lossy(&probed.stderr)
        );
        let streams = String::from_utf8_lossy(&probed.stdout);
        assert!(
            streams.lines().any(|line| line.contains("h264")),
            "{streams}"
        );
        assert!(
            streams.lines().any(|line| line.contains("ac3")),
            "{streams}"
        );
        assert!(
            !streams.lines().any(|line| line.contains("truehd")),
            "{streams}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn genuine_profile7_converts_to_signaled_playable_profile8() {
        use std::process::{Command, Stdio};

        let Some(dovi) = dovi_tool_path() else {
            eprintln!("skip genuine Profile 7 conversion (dovi_tool unavailable)");
            return;
        };
        if Command::new("ffmpeg")
            .args(["-nostdin", "-version"])
            .stdin(Stdio::null())
            .output()
            .is_err()
        {
            eprintln!("skip genuine Profile 7 conversion (ffmpeg unavailable)");
            return;
        }

        let tmp = std::env::temp_dir().join(format!(
            "rdlna-dvp7-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/library/video/dvp7.mkv");
        assert!(input.is_file(), "tracked genuine Profile 7 fixture missing");
        assert!(
            !input.with_extension("probe.toml").exists(),
            "genuine fixture must not depend on a probe sidecar"
        );
        let output = tmp.join("profile8.mp4.part");
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::RemuxP8,
            video_encoder: "copy".into(),
            audio: AudioAction::ToAc3,
            audio_index: 0,
            container: "mp4",
            ..TranscodePlan::default()
        };
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        run_remux_p8_controlled(
            &input,
            &output,
            &plan,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            &cancelled,
        )
        .unwrap();

        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes.windows(4).any(|window| window == b"moof"));
        assert!(bytes.windows(4).any(|window| window == b"dvvC"));

        let probed = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_streams",
                "-show_entries",
                "stream=codec_name,pix_fmt,color_space,color_transfer,color_primaries:stream_side_data",
                "-of",
                "json",
            ])
            .arg(&output)
            .stdin(Stdio::null())
            .output()
            .expect("probe converted Profile 8 fMP4");
        assert!(
            probed.status.success(),
            "ffprobe rejected Profile 8 fMP4: {}",
            String::from_utf8_lossy(&probed.stderr)
        );
        let probe = String::from_utf8_lossy(&probed.stdout);
        for expected in [
            "\"codec_name\": \"hevc\"",
            "\"codec_name\": \"ac3\"",
            "\"pix_fmt\": \"yuv420p10le\"",
            "\"color_space\": \"bt2020nc\"",
            "\"color_transfer\": \"smpte2084\"",
            "\"color_primaries\": \"bt2020\"",
            "\"dv_profile\": 8",
            "\"rpu_present_flag\": 1",
            "\"el_present_flag\": 0",
            "\"bl_present_flag\": 1",
            "\"dv_bl_signal_compatibility_id\": 1",
        ] {
            assert!(probe.contains(expected), "missing {expected} in {probe}");
        }

        let decoded = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&output)
            .args(["-map", "0:v:0", "-frames:v", "1", "-f", "null", "-"])
            .stdin(Stdio::null())
            .output()
            .expect("decode a converted fragment");
        assert!(
            decoded.status.success(),
            "converted fMP4 was not decodable: {}",
            String::from_utf8_lossy(&decoded.stderr)
        );

        let hevc = tmp.join("profile8.hevc");
        let extracted = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(&output)
            .args([
                "-map",
                "0:v:0",
                "-c:v",
                "copy",
                "-bsf:v",
                "hevc_mp4toannexb",
                "-an",
                "-f",
                "hevc",
            ])
            .arg(&hevc)
            .stdin(Stdio::null())
            .output()
            .expect("extract converted HEVC");
        assert!(extracted.status.success());
        let rpu = tmp.join("profile8-rpu.bin");
        let extracted_rpu = Command::new(&dovi)
            .args(["extract-rpu", "-i"])
            .arg(&hevc)
            .arg("-o")
            .arg(&rpu)
            .stdin(Stdio::null())
            .output()
            .expect("extract converted RPU");
        assert!(
            extracted_rpu.status.success(),
            "converted stream lacks RPU: {}",
            String::from_utf8_lossy(&extracted_rpu.stderr)
        );
        let info = Command::new(dovi)
            .args(["info", "-i"])
            .arg(&rpu)
            .args(["-f", "0"])
            .stdin(Stdio::null())
            .output()
            .expect("inspect converted RPU");
        let rpu_info = String::from_utf8_lossy(&info.stdout);
        assert!(info.status.success(), "{rpu_info}");
        assert!(rpu_info.contains("\"dovi_profile\": 8"), "{rpu_info}");
        assert!(
            rpu_info.contains("\"disable_residual_flag\": true"),
            "{rpu_info}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn job_gate_caps_background_jobs() {
        let gate = std::sync::Arc::new(JobGate::new(1));
        let permit = gate.try_acquire().expect("first job");
        assert!(gate.try_acquire().is_none());
        drop(permit);
        assert!(gate.try_acquire().is_some());
    }

    #[test]
    fn remux_p8_pipeline_and_audio_pick() {
        assert_eq!(pick_audio_index("truehd,ac3,aac"), 1);
        assert_eq!(pick_audio_index("aac"), 0);
        assert_eq!(pick_audio_index("truehd,dts"), 0);
        assert_eq!(
            pick_audio_index_from_streams("1:0:truehd:8,2:1:ac3:6,3:2:ac3:2", "truehd,ac3"),
            1,
            "duplicate codec summary must not erase the real lossy ordinal"
        );
        assert_eq!(
            pick_audio_index_from_streams(
                "malformed,1:7:truehd,2:nope:aac:2,3:9:eac3",
                "truehd,ac3"
            ),
            9,
            "three-field records stay valid and malformed records are skipped"
        );
        assert_eq!(
            pick_audio_index_from_streams("malformed,1:7:truehd", "truehd,ac3"),
            7,
            "the first valid descriptor is the fallback"
        );
        let oversized = format!(
            "1:9:aac,{}",
            "x".repeat(rusty_dlna_protocol::MAX_COMPACT_STREAM_METADATA_BYTES)
        );
        assert_eq!(
            pick_audio_index_from_streams(&oversized, "truehd,ac3"),
            1,
            "over-budget metadata must use the bounded codec-summary fallback"
        );
        let bounded = (0..=rusty_dlna_protocol::MAX_COMPACT_AUDIO_RECORDS)
            .map(|index| {
                let codec = if index == rusty_dlna_protocol::MAX_COMPACT_AUDIO_RECORDS {
                    "aac"
                } else {
                    "truehd"
                };
                format!("{index}:{index}:{codec}")
            })
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            pick_audio_index_from_streams(&bounded, "aac"),
            0,
            "records beyond the parser budget cannot affect selection"
        );
        let mut plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::RemuxP8,
            video_encoder: "copy".into(),
            audio: AudioAction::ToAac,
            container: "mp4",
            audio_index: 1,
            ..TranscodePlan::default()
        };
        let grow = ffmpeg_grow_args("/media/movie.mkv", "/cache/1.part", &plan);
        assert!(grow.iter().any(|s| s == "0:a:1?"), "{grow:?}");
        let fb = hdr10_fallback_plan(&plan);
        assert_eq!(fb.action, RecodeAction::Hdr10);
        assert_ne!(fb.video_encoder, "copy");
        assert_eq!(fb.audio, AudioAction::ToAac);
        assert_eq!(fb.audio_index, 1);
        plan.audio = AudioAction::Copy;
        assert_eq!(hdr10_fallback_plan(&plan).audio, AudioAction::Copy);
        plan.audio = AudioAction::ToAc3;
        assert_eq!(hdr10_fallback_plan(&plan).audio, AudioAction::ToAc3);
        plan.audio_index = pick_audio_index("truehd,eac3");
        assert_eq!(plan.audio_index, 1);
        let x264 = hdr10_fallback_plan(&plan);
        let args = ffmpeg_grow_args("/m.mkv", "/o.part", &x264);
        assert!(args.iter().any(|s| s == "high10"), "{args:?}");
        assert!(!args.iter().any(|s| s == "main10"), "{args:?}");
    }

    #[test]
    fn remux_p8_falls_back_without_dovi() {
        if dovi_tool_path().is_some() {
            eprintln!("dovi_tool present; skip missing-binary path");
            return;
        }
        let src = std::env::temp_dir().join(format!("rdlna-nodovi-{}.mkv", std::process::id()));
        let dest =
            std::env::temp_dir().join(format!("rdlna-nodovi-{}.mp4.part", std::process::id()));
        let _ = std::fs::write(&src, b"not-hevc");
        let plan = TranscodePlan {
            action: RecodeAction::RemuxP8,
            video_encoder: "copy".into(),
            ..TranscodePlan::default()
        };
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let err = run_remux_p8_controlled(
            &src,
            &dest,
            &plan,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            &cancelled,
        )
        .unwrap_err();
        assert!(err.contains("dovi_tool"), "{err}");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn keyed_cache_changes_for_same_size_source_and_effective_plan() {
        let tmp = std::env::temp_dir().join(format!(
            "rdlna-keyed-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let src = tmp.join("src.mkv");
        std::fs::write(&src, b"same-size-content-a").unwrap();
        let mut plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Hdr10,
            video_encoder: "libx264".into(),
            audio: AudioAction::Copy,
            container: "mp4",
            ..TranscodePlan::default()
        };
        let first = transcode_cache_key(&src, &plan, false).unwrap();
        let first_dest = cache_dest_for_key(&tmp, 7, plan.action, &first);

        // Size and (on coarse filesystems) timestamp can remain unchanged;
        // sampled content must still invalidate the identity.
        std::fs::write(&src, b"same-size-content-b").unwrap();
        let replaced = transcode_cache_key(&src, &plan, false).unwrap();
        assert_ne!(replaced, first);
        assert_ne!(
            cache_dest_for_key(&tmp, 7, plan.action, &replaced),
            first_dest
        );

        plan.video_encoder = "hevc_nvenc".into();
        let encoder_changed = transcode_cache_key(&src, &plan, false).unwrap();
        assert_ne!(encoder_changed, replaced);
        plan.audio = AudioAction::ToAc3;
        let audio_changed = transcode_cache_key(&src, &plan, false).unwrap();
        assert_ne!(audio_changed, encoder_changed);
        plan.keep_hdr10 = !plan.keep_hdr10;
        let hdr_policy_changed = transcode_cache_key(&src, &plan, false).unwrap();
        assert_ne!(hdr_policy_changed, audio_changed);
        plan.action = RecodeAction::Browser;
        plan.browser_quality = Some(BrowserQuality::Auto);
        let uhd = transcode_cache_key(&src, &plan, false).unwrap();
        plan.browser_quality = Some(BrowserQuality::FullHd);
        let full_hd = transcode_cache_key(&src, &plan, false).unwrap();
        assert_ne!(full_hd, uhd);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn cached_tool_version_refreshes_after_executable_replacement() {
        let tmp = tool_test_dir("tool-version");
        let tool = tmp.join("fake-tool");
        let replacement = tmp.join("replacement");
        let write_tool = |path: &Path, version: &str| {
            write_executable_script(path, &format!("printf '%s\\n' '{version}'"));
        };

        write_tool(&tool, "tool-v1");
        let first = tool_version(&tool, ToolVersionFlavor::Ffmpeg, None).unwrap();
        assert!(first.starts_with("tool-v1|"), "{first}");
        write_tool(&replacement, "tool-v2");
        std::fs::rename(&replacement, &tool).unwrap();
        let second = tool_version(&tool, ToolVersionFlavor::Ffmpeg, None).unwrap();
        assert!(second.starts_with("tool-v2|"), "{second}");
        assert_ne!(second, first);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn profile8_cache_identity_tracks_ffprobe_version() {
        let tmp = tool_test_dir("p8-ffprobe-identity");
        write_fake_version_tool(&tmp.join("ffmpeg"), "-version", "ffmpeg-v1");
        write_fake_version_tool(&tmp.join("ffprobe"), "-version", "ffprobe-v1");
        write_fake_version_tool(&tmp.join("dovi_tool"), "--version", "dovi-v1");
        let first = fake_profile8_toolchain(&tmp);
        let first_key = profile8_snapshot_cache_key(&first);

        write_fake_version_tool(&tmp.join("ffprobe"), "-version", "ffprobe-v2");
        let second = fake_profile8_toolchain(&tmp);
        assert_ne!(
            first.ffprobe().fingerprint(),
            second.ffprobe().fingerprint()
        );
        assert_ne!(first_key, profile8_snapshot_cache_key(&second));
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn profile8_cache_identity_represents_every_executable() {
        let tmp = tool_test_dir("p8-complete-toolchain");
        let ffmpeg = tmp.join("ffmpeg");
        let ffprobe = tmp.join("ffprobe");
        let dovi = tmp.join("dovi_tool");
        write_fake_version_tool(&ffmpeg, "-version", "ffmpeg-v1");
        write_fake_version_tool(&ffprobe, "-version", "ffprobe-v1");
        write_fake_version_tool(&dovi, "--version", "dovi-v1");
        let first = fake_profile8_toolchain(&tmp);
        let signature = first.cache_signature();
        for fingerprint in [
            first.ffmpeg().fingerprint(),
            first.ffprobe().fingerprint(),
            first.dovi_tool().fingerprint(),
        ] {
            assert!(signature.contains(fingerprint), "{signature}");
        }
        let first_key = profile8_snapshot_cache_key(&first);

        write_fake_version_tool(&ffmpeg, "-version", "ffmpeg-v2");
        let ffmpeg_changed = fake_profile8_toolchain(&tmp);
        let ffmpeg_key = profile8_snapshot_cache_key(&ffmpeg_changed);
        assert_ne!(first_key, ffmpeg_key);

        write_fake_version_tool(&ffprobe, "-version", "ffprobe-v2");
        let ffprobe_changed = fake_profile8_toolchain(&tmp);
        let ffprobe_key = profile8_snapshot_cache_key(&ffprobe_changed);
        assert_ne!(ffmpeg_key, ffprobe_key);

        write_fake_version_tool(&dovi, "--version", "dovi-v2");
        let dovi_changed = fake_profile8_toolchain(&tmp);
        assert_ne!(ffprobe_key, profile8_snapshot_cache_key(&dovi_changed));
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn profile8_execution_uses_snapshotted_inode_after_path_replacement() {
        let snapshotted = tool_test_dir("p8-snapshotted-path");
        let later_path = tool_test_dir("p8-later-path");
        for dir in [&snapshotted, &later_path] {
            write_fake_version_tool(&dir.join("ffmpeg"), "-version", "ffmpeg-v1");
            write_fake_version_tool(&dir.join("dovi_tool"), "--version", "dovi-v1");
        }
        let snapshotted_probe_ran = snapshotted.join("probe-ran");
        let later_probe_ran = later_path.join("probe-ran");
        write_executable_script(
            &snapshotted.join("ffprobe"),
            &format!(
                "if [ \"$1\" = '-version' ]; then printf '%s\\n' ffprobe-v1; else printf x > '{}'; printf '%s\\n' 6; fi",
                snapshotted_probe_ran.display()
            ),
        );
        write_executable_script(
            &later_path.join("ffprobe"),
            &format!(
                "if [ \"$1\" = '-version' ]; then printf '%s\\n' ffprobe-v2; else printf x > '{}'; printf '%s\\n' 9; fi",
                later_probe_ran.display()
            ),
        );
        let toolchain = fake_profile8_toolchain(&snapshotted);
        let replacement = snapshotted.join("ffprobe-replacement");
        write_executable_script(
            &replacement,
            &format!(
                "if [ \"$1\" = '-version' ]; then printf '%s\\n' ffprobe-v2; else printf x > '{}'; printf '%s\\n' 9; fi",
                later_probe_ran.display()
            ),
        );
        std::fs::rename(&replacement, snapshotted.join("ffprobe")).unwrap();
        let changed_search = std::env::join_paths([&later_path]).unwrap();
        assert_eq!(
            resolve_tool_path_with_search(Path::new("ffmpeg"), Some(&changed_search)),
            std::fs::canonicalize(later_path.join("ffmpeg")).unwrap()
        );

        let plan = TranscodePlan {
            action: RecodeAction::RemuxP8,
            ..TranscodePlan::default()
        };
        let commands = profile8_commands(
            &toolchain,
            OsString::from("source.mkv"),
            Path::new("stage.hevc"),
            Path::new("stage.p8.hevc"),
            Path::new("stage.p8.mp4"),
            Path::new("output.mp4.part"),
            &plan,
        );
        assert_eq!(commands.probe_level[0], toolchain.ffprobe().path());
        assert_eq!(commands.extract[0], toolchain.ffmpeg().path());
        assert_eq!(commands.wrap[0], toolchain.ffmpeg().path());
        assert_eq!(commands.mux[0], toolchain.ffmpeg().path());
        assert_eq!(commands.convert[0], toolchain.dovi_tool().path());
        toolchain.verify_current().unwrap();

        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let mut observer = || Ok(());
        let mut control = RemuxP8Control {
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(2),
            cancelled: &cancelled,
            observer: &mut observer,
        };
        let output = run_cmd_controlled_output(
            &commands.probe_level,
            None,
            Some(toolchain.ffprobe()),
            true,
            &mut control,
        )
        .unwrap();
        assert_eq!(output, b"6\n");
        assert!(snapshotted_probe_ran.exists());
        assert!(!later_probe_ran.exists());

        std::fs::remove_dir_all(snapshotted).unwrap();
        std::fs::remove_dir_all(later_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verified_executable_rejects_in_place_mutation_before_spawn() {
        let tmp = tool_test_dir("verified-tool-in-place-write");
        let tool = tmp.join("ffmpeg");
        write_fake_version_tool(&tool, "-version", "ffmpeg-v1");
        let snapshot = tool_snapshot(&tool, ToolVersionFlavor::Ffmpeg, None).unwrap();

        write_fake_version_tool(&tool, "-version", "ffmpeg-v2-with-different-bytes");

        let error = snapshot.verify_current("ffmpeg").unwrap_err();
        assert!(error.contains("changed before execution"), "{error}");
        let mut command = std::process::Command::new("true");
        let error = snapshot
            .inherit_for_execution(rusty_dlna_helper::SupervisedCommand::new(&mut command))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compatibility_tool_queries_check_cancel_and_deadline_before_spawn() {
        let tmp = tool_test_dir("compat-query-control");
        let marker = tmp.join("version-query-ran");
        for (name, arg) in [
            ("ffmpeg", "-version"),
            ("ffprobe", "-version"),
            ("dovi_tool", "--version"),
        ] {
            write_executable_script(
                &tmp.join(name),
                &format!(
                    "printf x >> '{}'; if [ \"$1\" = '{arg}' ]; then printf '%s\\n' version; fi",
                    marker.display()
                ),
            );
        }

        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let mut observer = || Ok(());
        let mut control = RemuxP8Control {
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
            cancelled: &cancelled,
            observer: &mut observer,
        };
        let cancelled_result = Profile8ToolchainSnapshot::query_paths_under_remux_control(
            &tmp.join("ffmpeg"),
            &tmp.join("ffprobe"),
            &tmp.join("dovi_tool"),
            &mut control,
        );
        assert!(matches!(cancelled_result, Err(RemuxP8Error::Cancelled(_))));
        assert!(!marker.exists());

        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let mut observer = || Ok(());
        let mut control = RemuxP8Control {
            deadline: std::time::Instant::now(),
            cancelled: &cancelled,
            observer: &mut observer,
        };
        let deadline_result = Profile8ToolchainSnapshot::query_paths_under_remux_control(
            &tmp.join("ffmpeg"),
            &tmp.join("ffprobe"),
            &tmp.join("dovi_tool"),
            &mut control,
        );
        assert!(matches!(deadline_result, Err(RemuxP8Error::Deadline(_))));
        assert!(!marker.exists());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_profile8_identity_retains_the_fingerprinted_ffmpeg() {
        let tmp = tool_test_dir("non-p8-ffmpeg-snapshot");
        let ffmpeg = tmp.join("ffmpeg");
        let source = tmp.join("source.mkv");
        write_fake_version_tool(&ffmpeg, "-version", "ffmpeg-v1");
        std::fs::write(&source, b"source bytes").unwrap();
        let source_file = std::fs::File::open(&source).unwrap();
        let gate = std::sync::Arc::new(rusty_dlna_helper::HelperGate::new(1, 0));
        let cancellation = rusty_dlna_helper::CancellationToken::default();
        let identity = transcode_cache_identity_file_controlled_with_ffmpeg(
            &source_file,
            &source,
            &TranscodePlan::default(),
            false,
            &ffmpeg,
            // This assertion is about identity handoff, not admission. Allow
            // parallel tool-query regressions to finish with the global
            // single-flight lock before this isolated gate is acquired.
            ToolQueryControl::new(&gate, &cancellation, std::time::Duration::from_secs(10)),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            identity.ffmpeg().path(),
            std::fs::canonicalize(ffmpeg).unwrap()
        );
        assert!(identity.ffmpeg().fingerprint().starts_with("ffmpeg-v1|"));
        assert!(identity.profile8_toolchain().is_none());
        let source_identity = source_identity_file(&source_file, &source).unwrap();
        assert_eq!(
            identity.cache_key(),
            transcode_cache_key_with_tools(
                &source_identity,
                &TranscodePlan::default(),
                false,
                identity.ffmpeg().fingerprint(),
                None,
                None,
            )
        );
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dovi_version_query_uses_long_flag_and_rejects_failure() {
        let tmp = tool_test_dir("dovi-version-flag");
        let tool = tmp.join("dovi_tool");
        write_executable_script(
            &tool,
            "if [ \"$1\" = \"--version\" ]; then printf '%s\\n' dovi-v1; else exit 9; fi",
        );
        let fingerprint = tool_version(&tool, ToolVersionFlavor::Dovi, None).unwrap();
        assert!(fingerprint.starts_with("dovi-v1|"), "{fingerprint}");

        let failing = tmp.join("failing-tool");
        write_executable_script(&failing, "printf '%s\\n' rejected >&2; exit 7");
        assert!(matches!(
            tool_version(&failing, ToolVersionFlavor::Ffmpeg, None),
            Err(ToolQueryError::Query { .. })
        ));
        let legacy = tool_version_for_cache_key(&failing, ToolVersionFlavor::Ffmpeg, None).unwrap();
        assert!(legacy.starts_with("unavailable:"), "{legacy}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn controlled_remux_key_fails_closed_when_dovi_tool_is_missing() {
        if dovi_tool_path().is_some()
            || tool_file_identity(&resolve_tool_path(Path::new("ffmpeg"))).is_none()
        {
            eprintln!("skip missing dovi_tool cache-key path");
            return;
        }
        let tmp = tool_test_dir("controlled-missing-dovi");
        let source = tmp.join("source.mkv");
        std::fs::write(&source, b"source bytes").unwrap();
        let file = std::fs::File::open(&source).unwrap();
        let plan = TranscodePlan {
            action: RecodeAction::RemuxP8,
            ..TranscodePlan::default()
        };
        let helpers = std::sync::Arc::new(rusty_dlna_helper::HelperGate::new(1, 0));
        let cancellation = rusty_dlna_helper::CancellationToken::default();
        let error = transcode_cache_key_file_controlled(
            &file,
            &source,
            &plan,
            true,
            ToolQueryControl::new(&helpers, &cancellation, std::time::Duration::from_secs(10)),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ToolQueryError::Query { ref executable, .. }
                if executable == Path::new("dovi_tool")
        ));
        assert!(transcode_cache_key_file(&file, &source, &plan, true).is_some());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn controlled_tool_query_admission_and_cache_hits_are_fail_closed() {
        let tmp = tool_test_dir("tool-query-admission");
        let tool = tmp.join("fake-tool");
        let marker = tmp.join("spawned");
        write_executable_script(
            &tool,
            &format!("printf x >> '{}'; printf '%s\\n' tool-v1", marker.display()),
        );
        let gate = std::sync::Arc::new(rusty_dlna_helper::HelperGate::new(1, 0));
        let cancelled = rusty_dlna_helper::CancellationToken::default();
        cancelled.cancel();
        assert!(matches!(
            tool_version(
                &tool,
                ToolVersionFlavor::Ffmpeg,
                Some(ToolQueryControl::new(
                    &gate,
                    &cancelled,
                    std::time::Duration::from_secs(1),
                )),
            ),
            Err(ToolQueryError::Cancelled)
        ));
        assert!(!marker.exists());
        assert_eq!(gate.metrics().admitted_total, 0);

        let live = rusty_dlna_helper::CancellationToken::default();
        let held = gate.try_acquire().unwrap();
        assert!(matches!(
            tool_version(
                &tool,
                ToolVersionFlavor::Ffmpeg,
                Some(ToolQueryControl::new(
                    &gate,
                    &live,
                    std::time::Duration::ZERO,
                )),
            ),
            Err(ToolQueryError::Busy(_))
        ));
        assert!(!marker.exists());
        assert_eq!(gate.metrics().admitted_total, 1);
        drop(held);

        let fingerprint = tool_version(
            &tool,
            ToolVersionFlavor::Ffmpeg,
            Some(ToolQueryControl::new(
                &gate,
                &live,
                std::time::Duration::from_secs(10),
            )),
        )
        .unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"x");
        assert_eq!(gate.metrics().admitted_total, 2);
        let held = gate.try_acquire().unwrap();
        let admitted_before_hit = gate.metrics().admitted_total;
        assert_eq!(
            tool_version(
                &tool,
                ToolVersionFlavor::Ffmpeg,
                Some(ToolQueryControl::new(
                    &gate,
                    &live,
                    std::time::Duration::ZERO,
                )),
            )
            .unwrap(),
            fingerprint
        );
        assert_eq!(gate.metrics().admitted_total, admitted_before_hit);
        drop(held);
        assert_eq!(gate.metrics().active, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn controlled_tool_query_is_single_flight() {
        let tmp = tool_test_dir("tool-single-flight");
        let tool = tmp.join("fake-tool");
        let launches = tmp.join("launches");
        write_executable_script(
            &tool,
            &format!(
                "printf x >> '{}'; sleep 0.1; printf '%s\\n' tool-v1",
                launches.display()
            ),
        );
        let gate = std::sync::Arc::new(rusty_dlna_helper::HelperGate::new(4, 8));
        let cancellation = rusty_dlna_helper::CancellationToken::default();
        let mut workers = Vec::new();
        for _ in 0..6 {
            let gate = gate.clone();
            let cancellation = cancellation.clone();
            let tool = tool.clone();
            workers.push(std::thread::spawn(move || {
                tool_version(
                    &tool,
                    ToolVersionFlavor::Ffmpeg,
                    Some(ToolQueryControl::new(
                        &gate,
                        &cancellation,
                        std::time::Duration::from_secs(10),
                    )),
                )
                .unwrap()
            }));
        }
        let fingerprints = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(fingerprints.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(std::fs::read(&launches).unwrap(), b"x");
        assert_eq!(gate.metrics().admitted_total, 1);
        assert_eq!(gate.metrics().active, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn controlled_tool_query_observes_in_flight_cancellation() {
        let tmp = tool_test_dir("tool-query-cancel");
        let tool = tmp.join("fake-tool");
        let started = tmp.join("started");
        write_executable_script(
            &tool,
            &format!(
                "printf ready > '{}'; trap '' TERM; sleep 5; printf '%s\\n' too-late",
                started.display()
            ),
        );
        let gate = std::sync::Arc::new(rusty_dlna_helper::HelperGate::new(1, 0));
        let cancellation = rusty_dlna_helper::CancellationToken::default();
        let worker_gate = gate.clone();
        let worker_cancellation = cancellation.clone();
        let worker_tool = tool.clone();
        let worker = std::thread::spawn(move || {
            tool_version(
                &worker_tool,
                ToolVersionFlavor::Ffmpeg,
                Some(ToolQueryControl::new(
                    &worker_gate,
                    &worker_cancellation,
                    std::time::Duration::from_secs(10),
                )),
            )
        });
        let wait_until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !started.exists() && std::time::Instant::now() < wait_until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(started.exists(), "version helper did not start");
        cancellation.cancel();
        assert!(matches!(
            worker.join().unwrap(),
            Err(ToolQueryError::Cancelled)
        ));
        assert_eq!(gate.metrics().active, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn controlled_tool_query_deadline_reaps_the_process_group() {
        let tmp = tool_test_dir("tool-query-deadline");
        let tool = tmp.join("fake-tool");
        let pid_file = tmp.join("pid");
        write_executable_script(
            &tool,
            &format!(
                "printf '%s' $$ > '{}'; trap '' TERM; sleep 5; printf '%s\\n' too-late",
                pid_file.display()
            ),
        );
        let gate = std::sync::Arc::new(rusty_dlna_helper::HelperGate::new(1, 0));
        let cancellation = rusty_dlna_helper::CancellationToken::default();
        let result = tool_version(
            &tool,
            ToolVersionFlavor::Ffmpeg,
            Some(
                ToolQueryControl::new(&gate, &cancellation, std::time::Duration::from_secs(10))
                    .query_timeout(std::time::Duration::from_millis(20)),
            ),
        );
        assert!(matches!(result, Err(ToolQueryError::Deadline { .. })));
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        let wait_until = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while proc_path.exists() && std::time::Instant::now() < wait_until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!proc_path.exists(), "version helper {pid} was not reaped");
        assert_eq!(gate.metrics().active, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn controlled_helpers_bound_stdout_and_keep_stderr_tail() {
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let overflow: Vec<OsString> = ["sh", "-c", "head -c 70000 /dev/zero"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let error = run_cmd_capture_controlled(
            &overflow,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            &cancelled,
            None,
        )
        .unwrap_err();
        assert_eq!(error, "helper stdout exceeded 65536 bytes");

        let noisy: Vec<OsString> = [
            "sh",
            "-c",
            "printf PREFIX >&2; head -c 70000 /dev/zero | tr '\\0' x >&2; printf TAIL >&2; exit 7",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let error = run_cmd_controlled(
            &noisy,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            &cancelled,
            None,
        )
        .unwrap_err();
        assert!(error.ends_with("TAIL"), "{error}");
        assert!(!error.contains("PREFIX"), "{error}");
        assert!(error.len() <= 64 * 1024 + 16, "{}", error.len());
    }

    #[cfg(unix)]
    #[test]
    fn controlled_helpers_preserve_non_utf8_arguments() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let argument = OsString::from_vec(b"media-\x80-name".to_vec());
        let args = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("printf %s \"$1\""),
            OsString::from("sh"),
            argument.clone(),
        ];
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let output = run_cmd_capture_controlled(
            &args,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            &cancelled,
            None,
        )
        .unwrap();
        assert_eq!(output, argument.as_bytes());
    }

    #[test]
    fn controlled_helpers_do_not_spawn_after_pre_cancellation() {
        let marker =
            std::env::temp_dir().join(format!("rdlna-pre-cancelled-helper-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let args = vec![OsString::from("touch"), marker.as_os_str().to_os_string()];
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let error = run_cmd_controlled(
            &args,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            &cancelled,
            None,
        )
        .unwrap_err();
        assert_eq!(error, "touch cancelled");
        assert!(!marker.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn profile8_observer_stops_and_reaps_the_active_stage_group() {
        let tmp = tool_test_dir("p8-stage-observer");
        let leader = tmp.join("leader.pid");
        let descendant = tmp.join("descendant.pid");
        let script = format!(
            "echo $$ > '{}'; trap '' TERM; sleep 30 & echo $! > '{}'; wait",
            leader.display(),
            descendant.display()
        );
        let args = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from(script),
        ];
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let mut observer = || {
            if leader.is_file() && descendant.is_file() {
                Err("simulated cache pressure".into())
            } else {
                Ok(())
            }
        };
        let started = std::time::Instant::now();
        let mut control = RemuxP8Control {
            deadline: started + std::time::Duration::from_secs(3),
            cancelled: &cancelled,
            observer: &mut observer,
        };

        let error = run_cmd_controlled_output(&args, None, None, false, &mut control).unwrap_err();

        assert_eq!(
            error,
            RemuxP8Error::Observer("simulated cache pressure".into())
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        for marker in [&leader, &descendant] {
            let pid = std::fs::read_to_string(marker).unwrap();
            let process = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
            let gone = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while process.exists() && std::time::Instant::now() < gone {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(!process.exists(), "stage process {} survived", pid.trim());
        }
        std::fs::remove_dir_all(tmp).unwrap();
    }
}
