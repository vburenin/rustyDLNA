//! HTTP paths used by description and SOAP dispatch.

pub const ROOTDESC_PATH: &str = "/rootDesc.xml";
pub const CONTENTDIRECTORY_PATH: &str = "/ContentDir.xml";
pub const CONTENTDIRECTORY_CONTROLURL: &str = "/ctl/ContentDir";
pub const CONTENTDIRECTORY_EVENTURL: &str = "/evt/ContentDir";
pub const CONNECTIONMGR_PATH: &str = "/ConnectionMgr.xml";
pub const CONNECTIONMGR_CONTROLURL: &str = "/ctl/ConnectionMgr";
pub const CONNECTIONMGR_EVENTURL: &str = "/evt/ConnectionMgr";
pub const X_MS_MEDIARECEIVERREGISTRAR_PATH: &str = "/X_MS_MediaReceiverRegistrar.xml";
pub const X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL: &str = "/ctl/X_MS_MediaReceiverRegistrar";
pub const X_MS_MEDIARECEIVERREGISTRAR_EVENTURL: &str = "/evt/X_MS_MediaReceiverRegistrar";

pub const MEDIA_ITEMS_PREFIX: &str = "/MediaItems/";
pub const TRANSCODE_PREFIX: &str = "/Transcode/";
pub const THUMBNAILS_PREFIX: &str = "/Thumbnails/";
pub const ALBUM_ART_PREFIX: &str = "/AlbumArt/";
pub const RESIZED_PREFIX: &str = "/Resized/";
pub const ICONS_PREFIX: &str = "/icons/";
pub const CAPTIONS_PREFIX: &str = "/Captions/";
pub const STATUS_PATH: &str = "/status";
pub const HEALTH_PATH: &str = "/health";
pub const API_STATUS_PATH: &str = "/api/status";

/// Parse a leading integer and ignore any suffix
/// (so `/MediaItems/{id}.{ext}` treats `{ext}` as decorative).
pub fn strtoll_prefix(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let b = s.as_bytes();
    let mut end = 0;
    if b[0] == b'+' || b[0] == b'-' {
        end = 1;
    }
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 || (end == 1 && (b[0] == b'+' || b[0] == b'-')) {
        return None;
    }
    s[..end].parse().ok()
}

fn strip_query(path: &str) -> &str {
    path.split_once('?').map(|(p, _)| p).unwrap_or(path)
}

pub fn media_item_id_from_path(path: &str) -> Option<i64> {
    let rest = strip_query(path).strip_prefix(MEDIA_ITEMS_PREFIX)?;
    strtoll_prefix(rest)
}

pub fn transcode_id_from_path(path: &str) -> Option<i64> {
    let rest = strip_query(path).strip_prefix(TRANSCODE_PREFIX)?;
    strtoll_prefix(rest)
}

fn caption_index_prefix(value: &str) -> Option<u32> {
    let value = value.trim();
    let bytes = value.as_bytes();
    let sign_len = usize::from(matches!(bytes.first(), Some(b'+') | Some(b'-')));
    let end = bytes[sign_len..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
        + sign_len;
    if end == sign_len {
        // The pre-indexed caption route historically treats a nonnumeric
        // decorative component as the default caption.
        return Some(0);
    }
    let digits = &value[sign_len..end];
    if sign_len == 1 && bytes[0] == b'-' {
        return digits.bytes().all(|byte| byte == b'0').then_some(0);
    }
    digits.parse().ok()
}

/// `/Captions/{id}/{index}.{ext}` or `/Captions/{id}.srt`.
pub fn caption_from_path(path: &str) -> Option<(i64, u32)> {
    let rest = strip_query(path).strip_prefix(CAPTIONS_PREFIX)?;
    let id = strtoll_prefix(rest)?;
    let after_id = rest.trim_start_matches(|c: char| c == '+' || c == '-' || c.is_ascii_digit());
    if let Some(indexed) = after_id.strip_prefix('/') {
        Some((id, caption_index_prefix(indexed)?))
    } else {
        Some((id, 0))
    }
}

pub fn transcode_item_url(host: &str, port: u16, detail_id: i64) -> String {
    format!("http://{host}:{port}{TRANSCODE_PREFIX}{detail_id}.mp4")
}

/// SSDP `LOCATION` is always this path on the iface that received the search.
pub fn location_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}{ROOTDESC_PATH}")
}

pub fn media_item_url(host: &str, port: u16, detail_id: i64, ext: &str) -> String {
    format!("http://{host}:{port}{MEDIA_ITEMS_PREFIX}{detail_id}.{ext}")
}

pub fn caption_indexed_url(host: &str, port: u16, detail_id: i64, index: u32, ext: &str) -> String {
    format!("http://{host}:{port}{CAPTIONS_PREFIX}{detail_id}/{index}.{ext}")
}

pub fn caption_default_url(host: &str, port: u16, detail_id: i64) -> String {
    format!("http://{host}:{port}{CAPTIONS_PREFIX}{detail_id}.srt")
}

/// `/AlbumArt/{artId}-{detailId}.jpg` — only the leading integer is `ALBUM_ART.ID`.
pub fn album_art_id_from_path(path: &str) -> Option<i64> {
    let rest = strip_query(path).strip_prefix(ALBUM_ART_PREFIX)?;
    strtoll_prefix(rest)
}

pub fn album_art_url(host: &str, port: u16, art_id: i64, detail_id: i64) -> String {
    format!("http://{host}:{port}{ALBUM_ART_PREFIX}{art_id}-{detail_id}.jpg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_literals_locked() {
        assert_eq!(ROOTDESC_PATH, "/rootDesc.xml");
        assert_eq!(CONTENTDIRECTORY_CONTROLURL, "/ctl/ContentDir");
        assert_eq!(CONNECTIONMGR_CONTROLURL, "/ctl/ConnectionMgr");
        assert_eq!(CONTENTDIRECTORY_EVENTURL, "/evt/ContentDir");
        assert_eq!(MEDIA_ITEMS_PREFIX, "/MediaItems/");
        assert_eq!(CAPTIONS_PREFIX, "/Captions/");
    }

    #[test]
    fn location_points_at_rootdesc() {
        assert_eq!(
            location_url("192.0.2.1", 8200),
            "http://192.0.2.1:8200/rootDesc.xml"
        );
    }

    #[test]
    fn strtoll_ignores_media_extension() {
        assert_eq!(media_item_id_from_path("/MediaItems/42.mkv"), Some(42));
        assert_eq!(media_item_id_from_path("/MediaItems/042.mp4"), Some(42));
        assert_eq!(media_item_id_from_path("/MediaItems/7"), Some(7));
        assert_eq!(
            media_item_id_from_path("/MediaItems/9.mkv?albumArt=true"),
            Some(9)
        );
        assert_eq!(media_item_id_from_path("/Captions/1.srt"), None);
        assert_eq!(caption_from_path("/Captions/9/1.srt"), Some((9, 1)));
        assert_eq!(caption_from_path("/Captions/9.srt"), Some((9, 0)));
        assert_eq!(
            caption_from_path("/Captions/9/4294967295.srt"),
            Some((9, u32::MAX))
        );
        assert_eq!(caption_from_path("/Captions/9/4294967296.srt"), None);
        assert_eq!(
            caption_from_path("/Captions/9/9223372036854775808.srt"),
            None
        );
        assert_eq!(caption_from_path("/Captions/9/-1.srt"), None);
        assert_eq!(
            caption_from_path("/Captions/9/-9223372036854775809.srt"),
            None
        );
        assert_eq!(caption_from_path("/Captions/9/-0.srt"), Some((9, 0)));
        assert_eq!(caption_from_path("/Captions/9/legacy.srt"), Some((9, 0)));
        assert_eq!(
            caption_from_path("/Captions/9/7.decorative.srt"),
            Some((9, 7))
        );
        assert_eq!(transcode_id_from_path("/Transcode/12.mp4"), Some(12));
        assert_eq!(album_art_id_from_path("/AlbumArt/7-3.jpg"), Some(7));
        assert_eq!(album_art_id_from_path("/AlbumArt/042-1.jpg"), Some(42));
        assert_eq!(
            album_art_url("192.0.2.1", 18200, 3, 9),
            "http://192.0.2.1:18200/AlbumArt/3-9.jpg"
        );
    }
}
