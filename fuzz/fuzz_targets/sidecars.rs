#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_scan::{artwork_path_matches_media, caption_path_matches_media};

fuzz_target!(|data: &[u8]| {
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
