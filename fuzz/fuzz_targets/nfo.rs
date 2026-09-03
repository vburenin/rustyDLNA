#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_scan::{nfo_date_from_text, parse_nfo_text, split_genres};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let metadata = parse_nfo_text(text);
    let _ = nfo_date_from_text(text);
    let _ = split_genres(text);
    assert!(metadata.genre.iter().all(|genre| !genre.is_empty()));
});
