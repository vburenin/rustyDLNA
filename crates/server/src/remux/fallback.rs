//! Effective fallback recipes and bounded failure disclosure. A request pathname
//! is a reserved lookup slot; its validation stamp identifies the actual bytes.
//! No device-wide/title-independent failure observations are retained.

use std::ffi::OsString;

use rusty_dlna_http::{RemuxJobSpec, RemuxOutputExpectation};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FailureClass {
    Unsupported,
    Resource,
    Input,
    Unknown,
}

impl FailureClass {
    fn id(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Resource => "resource",
            Self::Input => "input",
            Self::Unknown => "unknown",
        }
    }
}

pub(super) fn classify(diagnostic: &str) -> FailureClass {
    // The supervised helper already limits stderr to 64 KiB. Inspect only a
    // bounded suffix and disclose fixed categories rather than helper text.
    let diagnostic = super::tail_str(diagnostic, 16 * 1024).to_ascii_lowercase();
    if [
        "out of memory",
        "cannot allocate memory",
        "resource temporarily unavailable",
        "device busy",
        "no space left",
        "too many concurrent sessions",
    ]
    .iter()
    .any(|value| diagnostic.contains(value))
    {
        FailureClass::Resource
    } else if [
        "invalid data found",
        "corrupt",
        "error while decoding",
        "invalid nal",
        "moov atom not found",
    ]
    .iter()
    .any(|value| diagnostic.contains(value))
    {
        FailureClass::Input
    } else if [
        "unknown encoder",
        "unknown decoder",
        "cannot load libcuda",
        "cannot load libnvidia",
        "no capable devices found",
        "unsupported device",
        "device does not support",
        "driver does not support",
        "minimum required nvidia driver",
        "function not implemented",
    ]
    .iter()
    .any(|value| diagnostic.contains(value))
    {
        FailureClass::Unsupported
    } else {
        FailureClass::Unknown
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Attempt {
    Primary,
    Hdr10,
    AlternateHardware,
    Portable,
}

impl Attempt {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Hdr10 => "hdr10",
            Self::AlternateHardware => "alternate hardware",
            Self::Portable => "portable encoders",
        }
    }
}

/// Only bounded, server-generated recipe fields reach the browser. Keys bind
/// exact quality/filter/tool/device details without exposing commands or paths.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct EffectiveRecipe {
    pub(super) attempt: Attempt,
    pub(super) video_encoder: String,
    pub(super) audio_encoder: String,
    pub(super) video_codec: Option<String>,
    pub(super) audio_codecs: Vec<String>,
    pub(super) dynamic_range: &'static str,
    pub(super) pixel_format: String,
    pub(super) preset: String,
    pub(super) quality: String,
    pub(super) max_video_bitrate: String,
    pub(super) identity: String,
    pub(super) previous_failure: Option<FailureClass>,
    pub(super) previous_attempt_ms: Option<u64>,
    pub(super) attempt_ms: Option<u64>,
    pub(super) cache_reuse: bool,
}

fn option(args: &[OsString], name: &str) -> String {
    args.windows(2)
        .rev()
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].to_string_lossy().chars().take(80).collect())
        .unwrap_or_default()
}

pub(super) fn recipe(spec: &RemuxJobSpec, args: &[OsString], attempt: Attempt) -> EffectiveRecipe {
    let video_encoder = option(args, "-c:v");
    let dynamic_range = if video_encoder == "copy" {
        "copied"
    } else if option(args, "-color_trc") == "smpte2084" {
        "hdr10"
    } else {
        "sdr"
    };
    let quality = ["-crf", "-cq"]
        .into_iter()
        .map(|key| option(args, key))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    let expected = expectation(spec, args);
    EffectiveRecipe {
        attempt,
        video_encoder,
        audio_encoder: option(args, "-c:a"),
        video_codec: expected
            .as_ref()
            .and_then(|expected| expected.video_codec.clone()),
        audio_codecs: expected
            .map(|expected| expected.audio_codecs.into_iter().take(32).collect())
            .unwrap_or_default(),
        dynamic_range,
        pixel_format: option(args, "-pix_fmt"),
        preset: option(args, "-preset"),
        quality,
        max_video_bitrate: option(args, "-maxrate"),
        identity: rusty_dlna_transcode::effective_fallback_cache_key(&spec.cache_key, args),
        previous_failure: None,
        previous_attempt_ms: None,
        attempt_ms: None,
        cache_reuse: false,
    }
}

impl EffectiveRecipe {
    pub(super) fn mse_video_output(&self) -> Option<crate::web_ui::BrowserVideoOutput> {
        if self.attempt == Attempt::Primary || self.video_encoder == "copy" {
            return None;
        }
        match (self.video_codec.as_deref(), self.dynamic_range) {
            (Some("h264"), "sdr") => Some(crate::web_ui::BrowserVideoOutput::H264Sdr),
            (Some("hevc"), "hdr10") => Some(crate::web_ui::BrowserVideoOutput::HevcHdr10),
            _ => None,
        }
    }

    pub(super) fn mse_audio_codec(&self) -> Option<&str> {
        if self.attempt == Attempt::Primary
            || self.audio_encoder == "copy"
            || self.audio_encoder.is_empty()
        {
            return None;
        }
        match self.audio_codecs.as_slice() {
            [codec]
                if rusty_dlna_transcode::browser_audio_codec_from_name(codec)
                    == rusty_dlna_transcode::AudioCodec::Aac =>
            {
                Some(codec)
            }
            _ => None,
        }
    }

    pub(super) fn stamp_key(&self) -> String {
        format!(
            "{}-effective-v1-{}",
            self.identity,
            self.previous_failure.unwrap_or(FailureClass::Unknown).id()
        )
    }
}

pub(super) fn expectation(
    spec: &RemuxJobSpec,
    args: &[OsString],
) -> Option<RemuxOutputExpectation> {
    let mut expected = spec.output_expectation.clone()?;
    for pair in args.windows(2) {
        let option = pair[0].to_string_lossy();
        let codec = pair[1].to_string_lossy();
        if codec != "copy" && (option == "-c:a" || option.starts_with("-c:a:")) {
            if option == "-c:a" {
                expected.audio_codecs.fill(codec.into_owned());
            } else if let Some(index) = option
                .strip_prefix("-c:a:")
                .and_then(|index| index.parse::<usize>().ok())
            {
                if let Some(track) = expected.audio_codecs.get_mut(index) {
                    *track = codec.into_owned();
                }
            }
        }
        if pair[0] == "-c:v" && pair[1] != "copy" {
            expected.video_copy = false;
            expected.video_codec = Some(
                if pair[1].to_string_lossy().contains("264") {
                    "h264"
                } else {
                    "hevc"
                }
                .into(),
            );
        }
    }
    Some(expected)
}

/// Only candidates explicitly offered by current negotiation may match. A
/// successful fallback after contention/corrupt input/unknown failure remains
/// correctly stamped but does not suppress the next primary attempt.
pub(super) fn reusable(spec: &RemuxJobSpec) -> Option<EffectiveRecipe> {
    if !spec.cacheable || spec.output_expectation.is_none() {
        return None;
    }
    // Retry healthy primary paths periodically. Use immutable output mtime,
    // not the recency stamp that active readers renew. This is a per-artifact
    // preference bound, never a global device-disable observation.
    let age = spec.dest.metadata().ok()?.modified().ok()?.elapsed().ok()?;
    if age > std::time::Duration::from_secs(60 * 60) {
        return None;
    }
    for (args, attempt) in [
        (spec.remux_p8.then_some(&spec.args), Attempt::Hdr10),
        (
            spec.hardware_fallback_args.as_ref(),
            Attempt::AlternateHardware,
        ),
        (spec.fallback_args.as_ref(), Attempt::Portable),
    ] {
        let Some(args) = args else { continue };
        let mut actual = recipe(spec, args, attempt);
        actual.previous_failure = Some(FailureClass::Unsupported);
        if rusty_dlna_transcode::cache_is_fresh_for_key(&spec.dest, &actual.stamp_key()) {
            actual.cache_reuse = true;
            return Some(actual);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_failures_do_not_promote_transient_or_input_errors_to_hardware_observations() {
        assert_eq!(
            classify("Cannot load libcuda.so.1"),
            FailureClass::Unsupported
        );
        assert_eq!(
            classify("Unknown encoder h264_nvenc; out of memory"),
            FailureClass::Resource
        );
        assert_eq!(
            classify("Device does not support format; corrupt input"),
            FailureClass::Input
        );
        assert_eq!(
            classify("Error initializing output stream"),
            FailureClass::Unknown
        );
        assert_eq!(
            classify(&format!("Unknown encoder {}", "x".repeat(65536))),
            FailureClass::Unknown
        );
    }

    #[test]
    fn fallback_headers_omit_primary_and_copied_streams() {
        let directory = crate::remux::tests::temp_dir("fallback-header-contract");
        let mut spec =
            crate::remux::tests::job_spec(&directory, "fallback-header-contract", vec![]);
        spec.output_expectation = Some(RemuxOutputExpectation {
            video_codec: Some("hevc".into()),
            audio_codecs: vec!["aac".into()],
            duration_seconds: None,
            seek_seconds: 0.0,
            video_copy: true,
        });
        let copied = recipe(
            &spec,
            &[
                "ffmpeg".into(),
                "-c:v".into(),
                "copy".into(),
                "-c:a".into(),
                "copy".into(),
            ],
            Attempt::Portable,
        );
        assert!(copied.mse_video_output().is_none());
        assert!(copied.mse_audio_codec().is_none());
        let args = [
            "ffmpeg".into(),
            "-c:v".into(),
            "libx264".into(),
            "-c:a".into(),
            "aac".into(),
        ];
        let primary = recipe(&spec, &args, Attempt::Primary);
        assert!(primary.mse_video_output().is_none());
        assert!(primary.mse_audio_codec().is_none());
        let fallback = recipe(&spec, &args, Attempt::Portable);
        assert_eq!(
            fallback.mse_video_output(),
            Some(crate::web_ui::BrowserVideoOutput::H264Sdr)
        );
        assert_eq!(fallback.mse_audio_codec(), Some("aac"));
        let _ = std::fs::remove_dir_all(directory);
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use crate::remux::*;

    #[test]
    fn registered_fallback_keeps_existing_owners_but_rechecks_new_generations() {
        use crate::remux::tests::{job_spec, temp_dir, test_app};
        for expired in [false, true] {
            let dir = temp_dir("registered-fallback-recheck");
            let app = test_app(&dir, 1);
            let mut spec = job_spec(
                &dir,
                "registered-fallback-recheck",
                vec!["must-not-spawn".into()],
            );
            spec.job_key = "web:42:registered-fallback-recheck".into();
            spec.web_session_id = Some(9);
            spec.web_request_id = Some(77);
            spec.output_expectation = Some(RemuxOutputExpectation {
                video_codec: None,
                audio_codecs: vec![],
                duration_seconds: None,
                seek_seconds: 0.0,
                video_copy: false,
            });
            spec.fallback_args = Some(vec![
                "cp".into(),
                spec.src.as_os_str().to_owned(),
                rusty_dlna_transcode::cache_part(&spec.dest).into_os_string(),
            ]);
            std::fs::write(&spec.dest, b"validated cached bytes").unwrap();
            let mut actual = recipe(
                &spec,
                spec.fallback_args.as_ref().unwrap(),
                Attempt::Portable,
            );
            actual.previous_failure = Some(FailureClass::Unsupported);
            rusty_dlna_transcode::write_cache_stamp_for_key(&spec.dest, &actual.stamp_key())
                .unwrap();
            let job = attach_for_client(app.clone(), spec.clone()).unwrap();
            assert!(job.cache_hit);
            if expired {
                std::fs::File::open(&spec.dest)
                    .unwrap()
                    .set_modified(
                        std::time::SystemTime::now() - std::time::Duration::from_secs(3601),
                    )
                    .unwrap();
                rusty_dlna_transcode::write_cache_stamp_for_key(&spec.dest, &actual.stamp_key())
                    .unwrap();
            }
            // The old source generation remains bound to its pinned recipe.
            let existing = attach_for_client(app.clone(), spec.clone()).unwrap();
            assert!(std::sync::Arc::ptr_eq(&job, &existing));
            let mut new_owner = spec.clone();
            new_owner.web_session_id = Some(10);
            new_owner.web_request_id = Some(88);
            if !expired {
                // Same primary key but the current negotiation no longer offers
                // the successful fallback; registered output cannot bypass it.
                new_owner.fallback_args = None;
            }
            let error = match attach_for_client(app.clone(), new_owner) {
                Ok(_) => panic!("new generation bypassed fallback compatibility/expiry"),
                Err(error) => error,
            };
            assert!(error.starts_with("transcode busy"), "{error}");
            assert!(!job.owns_web_request(Some(10), 88));
            assert!(job.owns_web_request(Some(9), 77));
            assert!(!job.cancelled.load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(std::fs::read(&job.dest).unwrap(), b"validated cached bytes");
            assert_eq!(app.jobs.in_use(), 0);
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
