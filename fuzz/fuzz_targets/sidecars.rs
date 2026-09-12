#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_scan::{artwork_path_matches_media, caption_path_matches_media};

// Compile the exact pure production converter without exporting a fuzz-only
// server API or linking listener/job orchestration into this parser harness.
#[path = "../../crates/server/src/web_caption.rs"]
mod web_caption;

fuzz_target!(|data: &[u8]| {
    use rusty_dlna_protocol::CaptionWebVttConversion;
    for conversion in [
        CaptionWebVttConversion::SubRipToWebVtt,
        CaptionWebVttConversion::SubStationAlphaToWebVtt,
        CaptionWebVttConversion::SamiToWebVtt,
        CaptionWebVttConversion::ValidateWebVtt,
    ] {
        if let Ok(converted) = web_caption::caption_to_webvtt(conversion, data) {
            assert!(converted.starts_with(b"WEBVTT"));
            assert!(converted.len() <= 6 * rusty_dlna_scan::MAX_SIDECAR_BYTES as usize);
            if converted.len() <= rusty_dlna_scan::MAX_SIDECAR_BYTES as usize {
                assert!(web_caption::caption_to_webvtt(
                    CaptionWebVttConversion::ValidateWebVtt,
                    &converted
                )
                .is_ok());
            }
        }
    }
    let split = data.len() / 2;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let sidecar = std::path::PathBuf::from(std::ffi::OsString::from_vec(data[..split].to_vec()));
        let media = std::path::PathBuf::from(std::ffi::OsString::from_vec(data[split..].to_vec()));
        let _ = caption_path_matches_media(&sidecar, &media);
        let _ = artwork_path_matches_media(&sidecar, &media);
    }
    #[cfg(not(unix))]
    if let Ok(text) = std::str::from_utf8(data) {
        let middle = text
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index >= text.len() / 2)
            .unwrap_or(text.len());
        let (sidecar, media) = text.split_at(middle);
        let _ = caption_path_matches_media(sidecar.as_ref(), media.as_ref());
        let _ = artwork_path_matches_media(sidecar.as_ref(), media.as_ref());
    }
});
