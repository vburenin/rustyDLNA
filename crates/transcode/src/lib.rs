//! Codec remaps: ad-hoc “if this stream, recode like that.”
//!
//! Default is serve the original. A `[[remap]]` row matches **codecs and
//! related stream traits** (container, HEVC, DV profile, audio, client
//! kind) — not titles or paths. First matching row wins.

use rusty_dlna_protocol::{
    identify_user_agent, ClientFlags, ClientKind, ClientProfile,
};
use serde::Deserialize;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecodeAction {
    /// Leave the file alone (useful to carve out an exception).
    Original,
    /// HEVC BL + rewrite RPU to Profile 8.1. Keeps DV; no NVENC.
    RemuxP8,
    /// Full encode to HDR10 (PQ + BT.2020). Drops DV.
    Hdr10,
    /// Copy video; convert or pick a lossy audio track.
    AudioAc3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioAction {
    Copy,
    ToAc3,
    ToAac,
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
#[serde(default)]
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

impl Default for RecodeAction {
    fn default() -> Self {
        RecodeAction::Original
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscodePlan {
    pub decision: Decision,
    pub action: RecodeAction,
    pub rule: Option<String>,
    pub keep_hdr10: bool,
    pub drop_dolby_vision: bool,
    pub video_encoder: String,
    pub audio: AudioAction,
    pub container: &'static str,
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
            audio: AudioAction::Copy,
            container: "original",
        }
    }
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
    if matches!(wl.as_str(), "google-cast" | "cast" | "crkey" | "streamer" | "chromecast") {
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

    pub fn matches_ua(&self, client: &ClientProfile, raw_ua: Option<&str>, src: &SourceMedia) -> bool {
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
    let profile = identify_user_agent(ua)
        .or_else(|| rusty_dlna_protocol::identify_x_av_client_info(ua));
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
    if src.hdr == HdrKind::Unknown {
        return TranscodePlan::default();
    }
    let Some(rule) = remaps.iter().find(|r| r.matches_ua(client, raw_ua, src)) else {
        return TranscodePlan::default();
    };
    plan_from_rule(rule, src)
}

fn plan_from_rule(rule: &RemapRule, src: &SourceMedia) -> TranscodePlan {
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
            audio: rule.audio_out.unwrap_or(AudioAction::Copy),
            container: "mp4",
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
                .unwrap_or_else(|| "hevc_nvenc".into()),
            audio: rule.audio_out.unwrap_or(if matches!(
                src.audio,
                AudioCodec::Ac3 | AudioCodec::Eac3 | AudioCodec::Aac
            ) {
                AudioAction::Copy
            } else {
                AudioAction::ToAc3
            }),
            container: "mp4",
        },
        RecodeAction::AudioAc3 => TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::AudioAc3,
            rule: rule.name.clone(),
            keep_hdr10: !matches!(src.hdr, HdrKind::Sdr),
            drop_dolby_vision: false,
            video_encoder: rule.encoder.clone().unwrap_or_else(|| "copy".into()),
            audio: rule.audio_out.unwrap_or(AudioAction::ToAc3),
            container: "original",
        },
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

/// ffmpeg argv for a live pipe. Remux-P8 copies video; HDR10 encodes.
pub fn ffmpeg_live_args(src_path: &str, plan: &TranscodePlan) -> Vec<String> {
    let mut a = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-i".into(),
        src_path.into(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
    ];
    match plan.action {
        RecodeAction::Original => {}
        RecodeAction::RemuxP8 | RecodeAction::AudioAc3 => {
            a.extend(["-c:v".into(), plan.video_encoder.clone()]);
        }
        RecodeAction::Hdr10 => {
            a.extend([
                "-vf".into(),
                "format=p010le".into(),
                "-c:v".into(),
                plan.video_encoder.clone(),
                "-profile:v".into(),
                "main10".into(),
                "-pix_fmt".into(),
                "p010le".into(),
                "-color_primaries".into(),
                "bt2020".into(),
                "-color_trc".into(),
                "smpte2084".into(),
                "-colorspace".into(),
                "bt2020nc".into(),
                "-tag:v".into(),
                "hvc1".into(),
            ]);
        }
    }
    match plan.audio {
        AudioAction::Copy => a.extend(["-c:a".into(), "copy".into()]),
        AudioAction::ToAc3 => a.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), "640k".into()]),
        AudioAction::ToAac => a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "256k".into()]),
    }
    a.extend(live_frag_tail("pipe:1"));
    a
}

/// ffmpeg argv that writes a **growing** fragmented MP4. First fragment is
/// playable; the rest is filled in the background. Not `+faststart`.
pub fn ffmpeg_grow_args(src_path: &str, dst_path: &str, plan: &TranscodePlan) -> Vec<String> {
    let mut a = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-i".into(),
        src_path.into(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
    ];
    match plan.action {
        RecodeAction::Original => {}
        RecodeAction::RemuxP8 | RecodeAction::AudioAc3 => {
            a.extend(["-c:v".into(), plan.video_encoder.clone()]);
        }
        RecodeAction::Hdr10 => {
            a.extend([
                "-vf".into(),
                "format=p010le".into(),
                "-c:v".into(),
                plan.video_encoder.clone(),
                "-profile:v".into(),
                "main10".into(),
                "-pix_fmt".into(),
                "p010le".into(),
                "-color_primaries".into(),
                "bt2020".into(),
                "-color_trc".into(),
                "smpte2084".into(),
                "-colorspace".into(),
                "bt2020nc".into(),
                "-tag:v".into(),
                "hvc1".into(),
            ]);
        }
    }
    match plan.audio {
        AudioAction::Copy => a.extend(["-c:a".into(), "copy".into()]),
        AudioAction::ToAc3 => a.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), "640k".into()]),
        AudioAction::ToAac => a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "256k".into()]),
    }
    a.extend(live_frag_tail(dst_path));
    a
}

fn live_frag_tail(out: &str) -> Vec<String> {
    vec![
        "-flush_packets".into(),
        "1".into(),
        "-frag_duration".into(),
        "1000000".into(),
        "-f".into(),
        "mp4".into(),
        "-movflags".into(),
        "frag_keyframe+empty_moov+default_base_moof".into(),
        out.into(),
    ]
}

/// ffmpeg argv that writes a **finished file** (remux/hdr10 cache).
/// No `pipe:1`, no `-ss` seek restart — Range is served from the file.
pub fn ffmpeg_file_args(src_path: &str, dst_path: &str, plan: &TranscodePlan) -> Vec<String> {
    let mut a = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-nostats".into(),
        "-y".into(),
        "-i".into(),
        src_path.into(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
    ];
    match plan.action {
        RecodeAction::Original => {}
        RecodeAction::RemuxP8 | RecodeAction::AudioAc3 => {
            a.extend(["-c:v".into(), plan.video_encoder.clone()]);
        }
        RecodeAction::Hdr10 => {
            a.extend([
                "-vf".into(),
                "format=p010le".into(),
                "-c:v".into(),
                plan.video_encoder.clone(),
                "-profile:v".into(),
                "main10".into(),
                "-pix_fmt".into(),
                "p010le".into(),
                "-color_primaries".into(),
                "bt2020".into(),
                "-color_trc".into(),
                "smpte2084".into(),
                "-colorspace".into(),
                "bt2020nc".into(),
                "-tag:v".into(),
                "hvc1".into(),
            ]);
        }
    }
    match plan.audio {
        AudioAction::Copy => a.extend(["-c:a".into(), "copy".into()]),
        AudioAction::ToAc3 => a.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), "640k".into()]),
        AudioAction::ToAac => a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "256k".into()]),
    }
    a.extend([
        "-f".into(),
        "mp4".into(),
        "-movflags".into(),
        "+faststart".into(),
        dst_path.into(),
    ]);
    a
}

/// Child that is killed on `Drop` (client disconnect / job cancel).
pub struct FfmpegJob {
    pub child: std::process::Child,
}

impl Drop for FfmpegJob {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
pub struct JobGate {
    max: usize,
    cur: std::sync::atomic::AtomicUsize,
}

pub struct JobPermit<'a> {
    gate: &'a JobGate,
}

impl JobGate {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            cur: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn try_acquire(&self) -> Option<JobPermit<'_>> {
        use std::sync::atomic::Ordering;
        loop {
            let c = self.cur.load(Ordering::SeqCst);
            if c >= self.max {
                return None;
            }
            if self
                .cur
                .compare_exchange(c, c + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(JobPermit { gate: self });
            }
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
        self.cur
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for JobPermit<'_> {
    fn drop(&mut self) {
        self.gate
            .cur
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub fn cache_dest(cache_dir: &std::path::Path, detail_id: i64, action: RecodeAction) -> std::path::PathBuf {
    let tag = match action {
        RecodeAction::Hdr10 => "hdr10",
        RecodeAction::RemuxP8 => "remux",
        RecodeAction::AudioAc3 => "ac3",
        RecodeAction::Original => "orig",
    };
    cache_dir.join(format!("{detail_id}-{tag}.mp4"))
}

/// In-progress remux. Only rename to `cache_dest` when ffmpeg exits 0.
pub fn cache_part(dest: &std::path::Path) -> std::path::PathBuf {
    let mut p = dest.as_os_str().to_os_string();
    p.push(".part");
    std::path::PathBuf::from(p)
}

/// Run ffmpeg CLI to a cache file. Returns the dest path. Existing non-empty
/// dest is reused so Range scrub does not restart a pipe.
pub fn ensure_cached_file(
    src: &std::path::Path,
    dest: &std::path::Path,
    plan: &TranscodePlan,
) -> std::io::Result<std::path::PathBuf> {
    if dest.is_file() && dest.metadata()?.len() > 0 {
        return Ok(dest.to_path_buf());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let args = ffmpeg_file_args(
        &src.to_string_lossy(),
        &dest.to_string_lossy(),
        plan,
    );
    let mut cmd = std::process::Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    if !dest.is_file() || dest.metadata()?.len() == 0 {
        return Err(std::io::Error::other("ffmpeg produced no cache file"));
    }
    Ok(dest.to_path_buf())
}

fn first_codec(s: &str) -> &str {
    s.split(',').map(str::trim).find(|p| !p.is_empty()).unwrap_or("")
}

pub fn probe_to_source(container: &str, video: &str, hdr: &str, audio: &str, w: u32, h: u32) -> SourceMedia {
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
        let args = ffmpeg_live_args("/media/movie.mkv", &p);
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
    fn file_cache_argv_is_not_a_live_pipe() {
        let cast = identify_user_agent("CrKey/1.54").unwrap();
        let p = decide(cast, &p7_truehd(), &sample_remaps());
        let args = ffmpeg_file_args("/media/movie.mkv", "/cache/1-hdr10.mp4", &p);
        assert_eq!(args[0], "ffmpeg");
        assert!(args.contains(&"+faststart".into()));
        assert!(!args.iter().any(|s| s == "pipe:1"));
        assert!(!args.iter().any(|s| s == "-ss"));
        assert!(args.last().unwrap().ends_with("1-hdr10.mp4"));
        let live = ffmpeg_live_args("/media/movie.mkv", &p);
        assert!(live.contains(&"pipe:1".into()));
        assert!(live.contains(&"-flush_packets".into()));
        assert!(live.contains(&"-frag_duration".into()));
        assert!(live
            .iter()
            .any(|s| s.contains("frag_keyframe+empty_moov+default_base_moof")));
        assert!(!live.iter().any(|s| s.contains("faststart")));
        let grow = ffmpeg_grow_args("/media/movie.mkv", "/cache/1-hdr10.mp4.part", &p);
        assert!(grow.contains(&"/cache/1-hdr10.mp4.part".into()));
        assert!(!grow.iter().any(|s| s == "pipe:1"));
        assert!(!grow.iter().any(|s| s.contains("faststart")));
        assert!(grow.iter().any(|s| s.contains("frag_keyframe")));
    }

    #[test]
    fn live_pipe_emits_ftyp_before_process_exits() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skip live pipe (no ffmpeg)");
            return;
        }
        let src = std::env::temp_dir().join(format!("rdlna-live-{}.mkv", std::process::id()));
        let mk = Command::new("ffmpeg")
            .args([
                "-y",
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
            .status();
        if !mk.map(|s| s.success()).unwrap_or(false) {
            let _ = std::fs::remove_file(&src);
            eprintln!("skip live pipe (could not make fixture)");
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
        let args = ffmpeg_live_args(&src.to_string_lossy(), &plan);
        let mut child = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn live ffmpeg");
        let mut stdout = child.stdout.take().expect("stdout");
        let mut buf = [0u8; 32];
        let n = stdout.read(&mut buf).expect("first fragment");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&src);
        assert!(n >= 8, "expected fMP4 bytes, got {n}");
        assert_eq!(&buf[4..8], b"ftyp", "first box must be ftyp, got {buf:x?}");
    }

    #[test]
    fn grow_file_emits_ftyp_before_process_exits() {
        use std::process::{Command, Stdio};
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skip grow file (no ffmpeg)");
            return;
        }
        let src = std::env::temp_dir().join(format!("rdlna-grow-src-{}.mkv", std::process::id()));
        let dest = std::env::temp_dir().join(format!("rdlna-grow-dst-{}.mp4.part", std::process::id()));
        let mk = Command::new("ffmpeg")
            .args([
                "-y",
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
    fn drop_kills_ffmpeg_job_and_max_jobs_caps() {
        let gate = JobGate::new(1);
        let p1 = gate.try_acquire().expect("first slot");
        assert!(gate.try_acquire().is_none());
        drop(p1);
        assert!(gate.try_acquire().is_some());

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        {
            let _job = FfmpegJob { child };
        }
        // SIGKILL has been sent; wait briefly for the zombie to clear.
        for _ in 0..20 {
            if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            } else {
                break;
            }
        }
        let still = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        if let Some(st) = still {
            // zombie 'Z' is acceptable; running 'R'/'S' is not
            let state = st.split_whitespace().nth(2).unwrap_or("");
            assert_eq!(state, "Z", "child should be dead after Drop, stat={st}");
        }
    }
}
