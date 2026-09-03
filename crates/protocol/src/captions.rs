//! Canonical caption extension, MIME, and browser-conversion policy.
//!
//! Caption extensions are ASCII filesystem conventions. Name lookup examines
//! only those ASCII bytes, so a non-UTF-8 title stem remains filesystem
//! identity rather than being converted through a lossy display string.

use std::ffi::OsStr;

/// A supported conversion from a text-caption format to browser WebVTT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionWebVttConversion {
    /// Validate and normalize an existing WebVTT document.
    ValidateWebVtt,
    /// Convert SubRip (`.srt`) cues.
    SubRipToWebVtt,
    /// Convert SubStation Alpha or Advanced SubStation Alpha cues.
    SubStationAlphaToWebVtt,
    /// Convert Synchronized Accessible Media Interchange (`.smi`) cues.
    SamiToWebVtt,
}

/// Caption policy associated with one canonical filename extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptionFormat {
    /// Canonical lowercase extension without a leading dot.
    pub extension: &'static str,
    /// HTTP MIME type used when serving the original sidecar.
    pub http_mime: &'static str,
    /// Browser WebVTT conversion, or `None` when the format is not safely
    /// convertible by rustyDLNA.
    pub webvtt_conversion: Option<CaptionWebVttConversion>,
}

/// Every caption extension admitted by the scanner.
pub const CAPTION_FORMATS: &[CaptionFormat] = &[
    CaptionFormat {
        extension: "srt",
        http_mime: "text/srt",
        webvtt_conversion: Some(CaptionWebVttConversion::SubRipToWebVtt),
    },
    CaptionFormat {
        extension: "ass",
        http_mime: "text/x-ssa",
        webvtt_conversion: Some(CaptionWebVttConversion::SubStationAlphaToWebVtt),
    },
    CaptionFormat {
        extension: "ssa",
        http_mime: "text/x-ssa",
        webvtt_conversion: Some(CaptionWebVttConversion::SubStationAlphaToWebVtt),
    },
    CaptionFormat {
        extension: "vtt",
        http_mime: "text/vtt",
        webvtt_conversion: Some(CaptionWebVttConversion::ValidateWebVtt),
    },
    CaptionFormat {
        extension: "smi",
        http_mime: "smi/caption",
        webvtt_conversion: Some(CaptionWebVttConversion::SamiToWebVtt),
    },
    CaptionFormat {
        extension: "sub",
        http_mime: "text/plain",
        webvtt_conversion: None,
    },
];

fn caption_format_for_extension_bytes(extension: &[u8]) -> Option<CaptionFormat> {
    CAPTION_FORMATS
        .iter()
        .copied()
        .find(|format| extension.eq_ignore_ascii_case(format.extension.as_bytes()))
}

fn extension_bytes(name: &[u8]) -> Option<&[u8]> {
    let dot = name.iter().rposition(|byte| *byte == b'.')?;
    let extension = &name[dot + 1..];
    (!extension.is_empty()).then_some(extension)
}

/// Look up a caption format from an extension without a leading dot.
pub fn caption_format_for_extension(extension: &str) -> Option<CaptionFormat> {
    caption_format_for_extension_bytes(extension.as_bytes())
}

/// Look up a caption format from a UTF-8 filename, case-insensitively.
pub fn caption_format_for_name(name: &str) -> Option<CaptionFormat> {
    caption_format_for_extension_bytes(extension_bytes(name.as_bytes())?)
}

/// Look up a caption format from a filesystem filename, case-insensitively.
///
/// On Unix, bytes before the final ASCII extension need not be valid UTF-8.
pub fn caption_format_for_os_name(name: &OsStr) -> Option<CaptionFormat> {
    caption_format_for_extension_bytes(extension_bytes(name.as_encoded_bytes())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_exhaustive_unique_and_round_trips() {
        let expected = [
            (
                "srt",
                "text/srt",
                Some(CaptionWebVttConversion::SubRipToWebVtt),
            ),
            (
                "ass",
                "text/x-ssa",
                Some(CaptionWebVttConversion::SubStationAlphaToWebVtt),
            ),
            (
                "ssa",
                "text/x-ssa",
                Some(CaptionWebVttConversion::SubStationAlphaToWebVtt),
            ),
            (
                "vtt",
                "text/vtt",
                Some(CaptionWebVttConversion::ValidateWebVtt),
            ),
            (
                "smi",
                "smi/caption",
                Some(CaptionWebVttConversion::SamiToWebVtt),
            ),
            ("sub", "text/plain", None),
        ];
        assert_eq!(CAPTION_FORMATS.len(), expected.len());
        for (format, (extension, mime, conversion)) in CAPTION_FORMATS.iter().zip(expected) {
            assert_eq!(format.extension, extension);
            assert_eq!(format.http_mime, mime);
            assert_eq!(format.webvtt_conversion, conversion);
            assert_eq!(caption_format_for_extension(extension), Some(*format));
            assert_eq!(
                caption_format_for_name(&format!("title.{extension}")),
                Some(*format)
            );
            let uppercase = extension.to_ascii_uppercase();
            assert_eq!(caption_format_for_extension(&uppercase), Some(*format));
            assert_eq!(
                caption_format_for_name(&format!("title.{uppercase}")),
                Some(*format)
            );
        }
        for (index, left) in CAPTION_FORMATS.iter().enumerate() {
            assert!(!CAPTION_FORMATS[index + 1..]
                .iter()
                .any(|right| left.extension == right.extension));
        }
    }

    #[test]
    fn extension_and_name_lookups_are_ascii_case_insensitive_and_exact() {
        assert_eq!(
            caption_format_for_extension("SrT").map(|format| format.extension),
            Some("srt")
        );
        assert_eq!(
            caption_format_for_name("title.EN.SsA").map(|format| format.extension),
            Some("ssa")
        );
        assert_eq!(
            caption_format_for_name(".srt").map(|format| format.extension),
            Some("srt")
        );
        for rejected in ["", ".", "title", "title.srt.bak", "title.srtx"] {
            assert_eq!(caption_format_for_name(rejected), None, "{rejected:?}");
        }
        assert_eq!(caption_format_for_extension(".srt"), None);
    }

    #[cfg(unix)]
    #[test]
    fn os_name_lookup_preserves_non_utf8_stem_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let name = std::ffi::OsString::from_vec(b"title-\x80.En.SrT".to_vec());
        assert_eq!(
            caption_format_for_os_name(&name).map(|format| format.extension),
            Some("srt")
        );
        let unknown = std::ffi::OsString::from_vec(b"title-\x80.Sr\xff".to_vec());
        assert_eq!(caption_format_for_os_name(&unknown), None);
    }
}
