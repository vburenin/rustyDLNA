#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_dlna_protocol::paths::{
    album_art_id_from_path, album_art_url, caption_from_path, caption_indexed_url,
    media_item_id_from_path, media_item_url, strtoll_prefix, transcode_id_from_path,
    transcode_item_url,
};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = strtoll_prefix(text);
    let _ = media_item_id_from_path(text);
    let _ = transcode_id_from_path(text);
    let _ = caption_from_path(text);
    let _ = album_art_id_from_path(text);

    let mut id_bytes = [0_u8; 8];
    let prefix = &data[..data.len().min(8)];
    id_bytes[..prefix.len()].copy_from_slice(prefix);
    let id = i64::from_le_bytes(id_bytes).saturating_abs();
    assert_eq!(media_item_id_from_path(&media_item_url("127.0.0.1", 8200, id, "mkv")[21..]), Some(id));
    assert_eq!(transcode_id_from_path(&transcode_item_url("127.0.0.1", 8200, id)[21..]), Some(id));
    assert_eq!(caption_from_path(&caption_indexed_url("127.0.0.1", 8200, id, 7, "srt")[21..]), Some((id, 7)));
    assert_eq!(album_art_id_from_path(&album_art_url("127.0.0.1", 8200, id, 1)[21..]), Some(id));
});
