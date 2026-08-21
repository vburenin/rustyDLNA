//! Codec remaps: ad-hoc “if this stream, recode like that.”
//!
//! Default is serve the original. A `[[remap]]` row matches **codecs and
//! related stream traits** (container, HEVC, DV profile, audio, client
//! kind) — not titles or paths. First matching row wins.

use rusty_dlna_protocol::{identify_user_agent, ClientFlags, ClientKind, ClientProfile};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::ffi::OsString;
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
    FullHd,
    DataSaver,
}

impl BrowserQuality {
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::FullHd => "full_hd",
            Self::DataSaver => "data_saver",
        }
    }

    pub fn max_width(self) -> u32 {
        match self {
            Self::Auto => 3840,
            Self::FullHd => 1920,
            Self::DataSaver => 1280,
        }
    }

    pub fn max_height(self) -> u32 {
        match self {
            Self::Auto => 2160,
            Self::FullHd => 1080,
            Self::DataSaver => 720,
        }
    }

    pub fn max_fps(self) -> u32 {
        30
    }

    pub fn h264_profile(self) -> &'static str {
        match self {
            Self::Auto | Self::FullHd => "high",
            Self::DataSaver => "main",
        }
    }

    pub fn h264_level(self) -> &'static str {
        match self {
            Self::Auto => "5.1",
            Self::FullHd => "4.1",
            Self::DataSaver => "3.1",
        }
    }

    pub fn crf(self) -> u8 {
        match self {
            Self::Auto => 20,
            Self::FullHd => 22,
            Self::DataSaver => 25,
        }
    }

    pub fn max_video_kbps(self) -> u32 {
        match self {
            Self::Auto => 25_000,
            Self::FullHd => 8_000,
            Self::DataSaver => 3_000,
        }
    }

    pub fn buffer_kbps(self) -> u32 {
        self.max_video_kbps() * 2
    }

    pub fn audio_kbps(self) -> u32 {
        match self {
            Self::Auto | Self::FullHd => 192,
            Self::DataSaver => 128,
        }
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
    let streams = descriptors
        .split(',')
        .filter_map(|record| {
            let mut fields = record.split(':');
            let _global_index = fields.next()?.parse::<usize>().ok()?;
            let audio_index = fields.next()?.parse::<usize>().ok()?;
            let codec = fields.next()?.trim();
            Some((audio_index, codec))
        })
        .collect::<Vec<_>>();
    streams
        .iter()
        .find(|(_, codec)| matches!(*codec, "aac" | "ac3" | "eac3" | "mp3"))
        .map(|(audio_index, _)| *audio_index)
        .unwrap_or_else(|| {
            streams
                .first()
                .map(|(audio_index, _)| *audio_index)
                .unwrap_or_else(|| pick_audio_index(fallback_audio_csv))
        })
}

fn audio_map_arg(plan: &TranscodePlan) -> String {
    format!("0:a:{}?", plan.audio_index)
}

/// Select an H.264 encoder suitable for browser compatibility output. The
/// global encoder may be HEVC for DLNA HDR rules, which browsers cannot use
/// as a general fallback format.
pub fn browser_video_encoder(configured: &str) -> &str {
    match configured {
        "h264_nvenc" => "h264_nvenc",
        _ => "libx264",
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
                    "-level:v".into(),
                    quality.map_or("4.1", BrowserQuality::h264_level).into(),
                ]);
            }
            a.extend([
                "-force_key_frames".into(),
                "expr:gte(t,n_forced*2)".into(),
            ]);
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

fn browser_scale_filter(
    quality: Option<BrowserQuality>,
    cuda: bool,
    pixel_format: &str,
) -> String {
    let Some(quality) = quality else {
        return format!("scale_cuda=format={pixel_format}");
    };
    if cuda {
        format!(
            "scale_cuda=w='min(iw,{})':h='min(ih,{})':force_original_aspect_ratio=decrease:force_divisible_by=2:format={pixel_format}",
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

#[derive(Debug)]
pub struct JobGate {
    max: usize,
    cur: std::sync::atomic::AtomicUsize,
}

impl JobGate {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            cur: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn in_use(&self) -> usize {
        self.cur.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Increment without a lifetime-bound permit (background remux thread).
    pub fn try_add(&self) -> bool {
        use std::sync::atomic::Ordering;
        loop {
            let c = self.cur.load(Ordering::SeqCst);
            if c >= self.max {
                return false;
            }
            if self
                .cur
                .compare_exchange(c, c + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub fn release(&self) {
        self.cur.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
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
    cache_dir.join(format!("{detail_id}-{tag}-{cache_key}.mp4"))
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
pub fn source_identity_file(
    file: &std::fs::File,
    identity_path: &std::path::Path,
) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let metadata = file.metadata().ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
    let mut hasher = Sha1::new();
    hasher.update(identity_path.to_string_lossy().as_bytes());
    hasher.update(metadata.len().to_le_bytes());
    if let Some(modified) = modified {
        hasher.update(modified.as_secs().to_le_bytes());
        hasher.update(modified.subsec_nanos().to_le_bytes());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
    }

    let mut file = file.try_clone().ok()?;
    const SAMPLE: u64 = 64 * 1024;
    let end = metadata.len().saturating_sub(SAMPLE);
    let middle = metadata.len().saturating_div(2).saturating_sub(SAMPLE / 2);
    let mut positions = vec![0, middle, end];
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
    Some(format!("{:x}", hasher.finalize()))
}

fn tool_version(executable: &std::path::Path) -> String {
    let output = std::process::Command::new(executable)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(output) => {
            let text = if output.stdout.is_empty() {
                String::from_utf8_lossy(&output.stderr)
            } else {
                String::from_utf8_lossy(&output.stdout)
            };
            text.lines().next().unwrap_or("unknown").trim().to_owned()
        }
        Err(error) => format!("unavailable:{error}"),
    }
}

/// Digest of everything that can materially change cached output.
pub fn transcode_cache_key(
    src: &std::path::Path,
    plan: &TranscodePlan,
    remux_p8: bool,
) -> Option<String> {
    let source = source_identity(src)?;
    Some(transcode_cache_key_from_identity(&source, plan, remux_p8))
}

pub fn transcode_cache_key_file(
    file: &std::fs::File,
    identity_path: &std::path::Path,
    plan: &TranscodePlan,
    remux_p8: bool,
) -> Option<String> {
    let source = source_identity_file(file, identity_path)?;
    Some(transcode_cache_key_from_identity(&source, plan, remux_p8))
}

fn transcode_cache_key_from_identity(source: &str, plan: &TranscodePlan, remux_p8: bool) -> String {
    let ffmpeg = tool_version(std::path::Path::new("ffmpeg"));
    let dovi = if remux_p8 {
        dovi_tool_path()
            .map(|path| tool_version(&path))
            .unwrap_or_else(|| "unavailable".into())
    } else {
        "unused".into()
    };
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
    let signature = format!(
        "source={source}\naction={:?}\nencoder={}\nhardware_decode={:?}\naudio={:?}\naudio_index={}\ncontainer={}\nbrowser_quality={browser_quality}\nremux_p8={remux_p8}\nffmpeg={ffmpeg}\ndovi={dovi}\nbuild={}",
        plan.action,
        plan.video_encoder,
        plan.hardware_decode,
        plan.audio,
        plan.audio_index,
        plan.container,
        env!("CARGO_PKG_VERSION"),
    );
    let mut hasher = Sha1::new();
    hasher.update(signature.as_bytes());
    format!("{:x}", hasher.finalize())
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
    }
}

fn inherit_source_fd(command: &mut std::process::Command, source_fd: Option<std::os::fd::RawFd>) {
    use std::os::unix::process::CommandExt;

    let Some(source_fd) = source_fd else {
        return;
    };
    const CHILD_SOURCE_FD: libc::c_int = 3;
    // SAFETY: only async-signal-safe dup2/fcntl calls run after fork. The
    // caller keeps the source File alive through child completion.
    unsafe {
        command.pre_exec(move || {
            if source_fd != CHILD_SOURCE_FD && libc::dup2(source_fd, CHILD_SOURCE_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(CHILD_SOURCE_FD, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn terminate_process_group(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    #[cfg(unix)]
    unsafe {
        // SAFETY: run_cmd_controlled starts each helper as a new process-group
        // leader, so this signal reaches only that helper tree.
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                return Some(status);
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => break,
        }
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    child.wait().ok()
}

fn run_cmd_controlled(
    args: &[String],
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    source_fd: Option<std::os::fd::RawFd>,
) -> Result<(), String> {
    run_cmd_controlled_output(args, deadline, cancelled, source_fd, false).map(|_| ())
}

fn run_cmd_capture_controlled(
    args: &[String],
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    source_fd: Option<std::os::fd::RawFd>,
) -> Result<Vec<u8>, String> {
    run_cmd_controlled_output(args, deadline, cancelled, source_fd, true)
}

fn run_cmd_controlled_output(
    args: &[String],
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
    source_fd: Option<std::os::fd::RawFd>,
    capture_stdout: bool,
) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::sync::atomic::Ordering;

    if args.is_empty() {
        return Err("empty command".into());
    }
    let mut command = std::process::Command::new(&args[0]);
    command
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(if capture_stdout {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    inherit_source_fd(&mut command, source_fd);
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", args[0]))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || -> Result<Vec<u8>, String> {
        const MAX_STDOUT: usize = 64 * 1024;
        let Some(mut stdout) = stdout else {
            return Ok(Vec::new());
        };
        let mut captured = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stdout
                .read(&mut chunk)
                .map_err(|error| format!("read helper stdout: {error}"))?;
            if read == 0 {
                break;
            }
            if captured.len().saturating_add(read) > MAX_STDOUT {
                return Err(format!("helper stdout exceeded {MAX_STDOUT} bytes"));
            }
            captured.extend_from_slice(&chunk[..read]);
        }
        Ok(captured)
    });
    let stderr_reader = std::thread::spawn(move || {
        const MAX_STDERR: usize = 64 * 1024;
        let Some(mut stderr) = stderr else {
            return Vec::new();
        };
        let mut tail = std::collections::VecDeque::with_capacity(MAX_STDERR);
        let mut chunk = [0u8; 4096];
        loop {
            let read = match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            tail.extend(&chunk[..read]);
            while tail.len() > MAX_STDERR {
                tail.pop_front();
            }
        }
        tail.into_iter().collect::<Vec<_>>()
    });
    let (status, stopped) = loop {
        if cancelled.load(Ordering::Acquire) {
            break (terminate_process_group(&mut child), Some("cancelled"));
        }
        if std::time::Instant::now() >= deadline {
            break (terminate_process_group(&mut child), Some("timed out"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), None),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(error) => {
                terminate_process_group(&mut child);
                return Err(format!("wait {}: {error}", args[0]));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| format!("{} stdout reader panicked", args[0]))??;
    let stderr = stderr_reader.join().unwrap_or_default();
    if let Some(stopped) = stopped {
        return Err(format!("{} {stopped}", args[0]));
    }
    if !status.is_some_and(|status| status.success()) {
        let tail = String::from_utf8_lossy(&stderr);
        return Err(format!("{}: {}", args[0], tail.trim()));
    }
    Ok(stdout)
}

#[derive(Clone, Copy, Debug)]
struct Mp4BoxSpan {
    offset: usize,
    size: usize,
    header_size: usize,
    kind: [u8; 4],
}

impl Mp4BoxSpan {
    fn content_start(self) -> usize {
        self.offset + self.header_size
    }

    fn end(self) -> usize {
        self.offset + self.size
    }
}

fn mp4_boxes(data: &[u8], start: usize, end: usize) -> Result<Vec<Mp4BoxSpan>, String> {
    if start > end || end > data.len() {
        return Err("invalid MP4 box bounds".into());
    }
    let mut boxes = Vec::new();
    let mut offset = start;
    while offset < end {
        if end - offset < 8 {
            return Err(format!("truncated MP4 box header at byte {offset}"));
        }
        let size32 = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
        let kind = data[offset + 4..offset + 8].try_into().unwrap();
        let (size, header_size) = match size32 {
            0 => (end - offset, 8),
            1 => {
                if end - offset < 16 {
                    return Err(format!("truncated large MP4 box at byte {offset}"));
                }
                let size64 = u64::from_be_bytes(data[offset + 8..offset + 16].try_into().unwrap());
                let size = usize::try_from(size64)
                    .map_err(|_| format!("MP4 box at byte {offset} is too large"))?;
                (size, 16)
            }
            size => (size as usize, 8),
        };
        if size < header_size || size > end - offset {
            return Err(format!("invalid MP4 box size {size} at byte {offset}"));
        }
        boxes.push(Mp4BoxSpan {
            offset,
            size,
            header_size,
            kind,
        });
        offset += size;
    }
    Ok(boxes)
}

fn boxes_of_kind(
    data: &[u8],
    start: usize,
    end: usize,
    kind: &[u8; 4],
) -> Result<Vec<Mp4BoxSpan>, String> {
    Ok(mp4_boxes(data, start, end)?
        .into_iter()
        .filter(|span| &span.kind == kind)
        .collect())
}

fn profile8_sample_entry_path(data: &[u8]) -> Result<Vec<Mp4BoxSpan>, String> {
    for moov in boxes_of_kind(data, 0, data.len(), b"moov")? {
        for trak in boxes_of_kind(data, moov.content_start(), moov.end(), b"trak")? {
            for mdia in boxes_of_kind(data, trak.content_start(), trak.end(), b"mdia")? {
                for minf in boxes_of_kind(data, mdia.content_start(), mdia.end(), b"minf")? {
                    for stbl in boxes_of_kind(data, minf.content_start(), minf.end(), b"stbl")? {
                        for stsd in boxes_of_kind(data, stbl.content_start(), stbl.end(), b"stsd")?
                        {
                            let entries_start = stsd
                                .content_start()
                                .checked_add(8)
                                .ok_or_else(|| "MP4 stsd offset overflow".to_string())?;
                            if entries_start > stsd.end() {
                                return Err("truncated MP4 stsd full-box header".into());
                            }
                            for entry in mp4_boxes(data, entries_start, stsd.end())? {
                                if entry.kind == *b"hvc1" || entry.kind == *b"hev1" {
                                    return Ok(vec![moov, trak, mdia, minf, stbl, stsd, entry]);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Err("HEVC sample entry not found in intermediate MP4".into())
}

fn signal_profile8_in_mp4(path: &Path, level: u8) -> Result<(), String> {
    if level > 63 {
        return Err(format!("invalid Dolby Vision level {level}"));
    }
    let mut data =
        std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let spans = profile8_sample_entry_path(&data)?;
    let entry = *spans.last().unwrap();
    const VISUAL_SAMPLE_ENTRY_FIELDS: usize = 78;
    let children_start = entry
        .content_start()
        .checked_add(VISUAL_SAMPLE_ENTRY_FIELDS)
        .ok_or_else(|| "MP4 sample-entry offset overflow".to_string())?;
    if children_start > entry.end() {
        return Err("truncated HEVC visual sample entry".into());
    }
    if mp4_boxes(&data, children_start, entry.end())?
        .iter()
        .any(|span| matches!(&span.kind, b"dvcC" | b"dvvC" | b"dvwC"))
    {
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

    for span in &spans {
        if span.header_size != 8 || span.size > u32::MAX as usize {
            return Err(format!(
                "unsupported large {:?} MP4 box",
                String::from_utf8_lossy(&span.kind)
            ));
        }
        let grown = span
            .size
            .checked_add(dvvc.len())
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| "MP4 box size overflow".to_string())?;
        data[span.offset..span.offset + 4].copy_from_slice(&grown.to_be_bytes());
    }
    data.splice(entry.end()..entry.end(), dvvc);
    std::fs::write(path, data).map_err(|error| format!("write {}: {error}", path.display()))
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
    run_remux_p8_impl(src, None, dest_part, plan, deadline, cancelled)
}

pub fn run_remux_p8_file_controlled(
    src: &std::fs::File,
    identity_path: &std::path::Path,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    run_remux_p8_impl(
        identity_path,
        Some(src.as_raw_fd()),
        dest_part,
        plan,
        deadline,
        cancelled,
    )
}

fn run_remux_p8_impl(
    src: &std::path::Path,
    source_fd: Option<std::os::fd::RawFd>,
    dest_part: &std::path::Path,
    plan: &TranscodePlan,
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let dovi = dovi_tool_path().ok_or_else(|| "dovi_tool not on PATH".to_string())?;
    if let Some(parent) = dest_part.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let hevc = dest_part.with_extension("hevc");
    let p8 = dest_part.with_extension("p8.hevc");
    let p8_mp4 = dest_part.with_extension("p8.mp4");
    let _ = std::fs::remove_file(&hevc);
    let _ = std::fs::remove_file(&p8);
    let _ = std::fs::remove_file(&p8_mp4);
    let source_arg = if source_fd.is_some() {
        "/proc/self/fd/3".to_string()
    } else {
        src.to_string_lossy().into_owned()
    };
    let extract = vec![
        "ffmpeg".into(),
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
        hevc.to_string_lossy().into_owned(),
    ];
    let convert = vec![
        dovi.to_string_lossy().into_owned(),
        "-m".into(),
        "2".into(),
        "convert".into(),
        "--discard".into(),
        hevc.to_string_lossy().into_owned(),
        "-o".into(),
        p8.to_string_lossy().into_owned(),
    ];
    let probe_level = vec![
        "ffprobe".into(),
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
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-nostdin".into(),
        "-fflags".into(),
        "+genpts".into(),
        "-i".into(),
        p8.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-c:v".into(),
        "copy".into(),
        "-tag:v".into(),
        "hvc1".into(),
        "-an".into(),
        p8_mp4.to_string_lossy().into_owned(),
    ];
    let mut mux = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-nostdin".into(),
        "-i".into(),
        p8_mp4.to_string_lossy().into_owned(),
        "-i".into(),
        source_arg,
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        format!("1:a:{}?", plan.audio_index),
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
    mux.extend(live_frag_tail(&dest_part.to_string_lossy()));
    let result = run_cmd_capture_controlled(&probe_level, deadline, cancelled, source_fd)
        .and_then(|output| {
            let level = String::from_utf8(output)
                .map_err(|_| "ffprobe Dolby Vision level was not UTF-8".to_string())?;
            level
                .lines()
                .find_map(|line| line.trim().parse::<u8>().ok())
                .filter(|level| *level <= 63)
                .ok_or_else(|| "ffprobe did not report a valid Dolby Vision level".to_string())
        })
        .and_then(|level| {
            run_cmd_controlled(&extract, deadline, cancelled, source_fd).map(|_| level)
        })
        .and_then(|level| run_cmd_controlled(&convert, deadline, cancelled, None).map(|_| level))
        .and_then(|level| run_cmd_controlled(&wrap, deadline, cancelled, None).map(|_| level))
        .and_then(|level| {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err("MP4 signaling cancelled".into());
            }
            if std::time::Instant::now() >= deadline {
                return Err("MP4 signaling timed out".into());
            }
            signal_profile8_in_mp4(&p8_mp4, level)
        })
        .and_then(|_| run_cmd_controlled(&mux, deadline, cancelled, source_fd));
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
        audio: match audio {
            "truehd" | "atmos" => AudioCodec::TrueHd,
            "ac3" => AudioCodec::Ac3,
            "eac3" => AudioCodec::Eac3,
            "dts" | "dts-hd" => AudioCodec::Dts,
            "aac" => AudioCodec::Aac,
            _ => AudioCodec::Other,
        },
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_dlna_protocol::identify_user_agent;

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
            .any(|pair| pair == ["-vf", "scale_cuda=format=yuv420p"]));
        assert!(!cuda.iter().any(|arg| arg == "-pix_fmt"));

        plan.browser_quality = Some(BrowserQuality::Auto);
        let uhd = ffmpeg_grow_args("source.mkv", "output.mp4.part", &plan);
        assert!(uhd
            .iter()
            .any(|arg| arg.contains("scale_cuda=w='min(iw,3840)':h='min(ih,2160)'")));
        assert!(uhd.windows(2).any(|pair| pair == ["-cq", "20"]));
        assert!(uhd.windows(2).any(|pair| pair == ["-profile:v", "high"]));
        assert!(uhd.windows(2).any(|pair| pair == ["-level:v", "5.1"]));
        assert!(uhd.windows(2).any(|pair| pair == ["-maxrate", "25000k"]));
        assert!(uhd.windows(2).any(|pair| pair == ["-bufsize", "50000k"]));

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
        assert!(saver.windows(2).any(|pair| pair == ["-profile:v", "main"]));
        assert!(saver.windows(2).any(|pair| pair == ["-level:v", "3.1"]));
        assert!(saver.windows(2).any(|pair| pair == ["-maxrate", "3000k"]));
        assert!(saver.windows(2).any(|pair| pair == ["-bufsize", "6000k"]));
        assert!(saver.windows(2).any(|pair| pair == ["-fpsmax", "30"]));
        assert!(saver.windows(2).any(|pair| pair == ["-b:a", "128k"]));
        assert!(saver.windows(2).any(|pair| pair == ["-ac", "2"]));

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
        let gate = JobGate::new(1);
        assert!(gate.try_add());
        assert!(!gate.try_add());
        gate.release();
        assert!(gate.try_add());
        gate.release();
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
        plan.action = RecodeAction::Browser;
        plan.browser_quality = Some(BrowserQuality::Auto);
        let uhd = transcode_cache_key(&src, &plan, false).unwrap();
        plan.browser_quality = Some(BrowserQuality::FullHd);
        let full_hd = transcode_cache_key(&src, &plan, false).unwrap();
        assert_ne!(full_hd, uhd);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
