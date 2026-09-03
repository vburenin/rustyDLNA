//! rustyDLNA on-the-wire protocol contract.
//!
//! Constants and helpers here define executable compatibility behavior. A
//! rewrite that changes them without a renderer reason can break TVs that
//! already work.

pub mod captions;
pub mod client_cache;
pub mod clients;
pub mod date;
pub mod http_syntax;
pub mod isolation;
pub mod media_format;
pub mod object_id;
pub mod paths;
pub mod persist;
pub mod protocol_info;
pub mod soap;
pub mod ssdp;
pub mod stream_metadata;
pub mod trickplay;
pub mod xml;

#[cfg(test)]
mod contract;

pub use captions::{
    caption_format_for_extension, caption_format_for_name, caption_format_for_os_name,
    CaptionFormat, CaptionWebVttConversion, CAPTION_FORMATS,
};
pub use client_cache::{ClientCache, ClientCacheEntry, CLIENT_CACHE_SLOTS, CLIENT_CACHE_TTL_SECS};
pub use clients::{
    identify_friendly_name, identify_friendly_name_ssdp, identify_model_name, identify_request,
    identify_user_agent, identify_x_av_client_info, remap_mime, remap_mime_full, ClientFlags,
    ClientKind, ClientProfile, MatchKind, CLIENTS,
};
pub use date::{utc_date_time, w3c_date_from_unix, w3c_normalize_date, UtcDateTime};
pub use http_syntax::{is_http_field_value, is_http_token, trim_http_ows};
pub use isolation::{
    collides_with_live_ports, test_listen_ports, LIVE_HTTP_PORT, LIVE_SSDP_PORT, TEST_HTTP_PORT,
    TEST_SSDP_PORT,
};
pub use media_format::{
    media_format_for_extension, media_format_for_name, wildcard_protocol_info_entries, MediaFormat,
    MediaKind, ResolvedMediaFormat, MEDIA_FORMATS,
};
pub use paths::{
    album_art_id_from_path, caption_from_path, media_item_id_from_path, strtoll_prefix,
    transcode_id_from_path,
};
pub use persist::http_should_persist;
pub use protocol_info::{protocol_info_source, PROTOCOL_INFO_SOURCE};
pub use stream_metadata::{
    decode_compact_stream_field, encode_compact_stream_field, encode_compact_stream_metadata,
    CompactAudioRecord, CompactAudioRecordInput, CompactChapter, CompactChapterInput,
    CompactFieldDecodeError, CompactStreamMetadata, CompactStreamMetadataInput,
    CompactStreamMetadataParseError, CompactStreamMetadataWriteError, CompactVideoCapabilities,
    CompactVideoCapabilitiesInput, MAX_COMPACT_AUDIO_RECORDS, MAX_COMPACT_CHAPTERS,
    MAX_COMPACT_CHAPTER_TITLE_CHARS, MAX_COMPACT_STREAM_METADATA_BYTES,
};
pub use trickplay::{
    is_trickplay_directory_name, trickplay_directory_for_media, trickplay_frame_count,
    trickplay_interval_seconds, trickplay_interval_seconds_for_layout, trickplay_layout_is_valid,
    trickplay_sheet_count, trickplay_sheet_count_for_layout, TRICKPLAY_ASSET_REVISION_HEX_LEN,
    TRICKPLAY_DIRECTORY_NAME, TRICKPLAY_DIRECTORY_SUFFIX, TRICKPLAY_FRAMES_PER_SHEET,
    TRICKPLAY_FRAME_HEIGHT, TRICKPLAY_FRAME_WIDTH, TRICKPLAY_LEGACY_DIRECTORY_SUFFIX,
    TRICKPLAY_MANIFEST_NAME, TRICKPLAY_MAX_DURATION_SECONDS, TRICKPLAY_MAX_FRAME_DIMENSION,
    TRICKPLAY_MAX_FRAME_PIXELS, TRICKPLAY_MAX_LAYOUT_AXIS, TRICKPLAY_MAX_MANIFEST_BYTES,
    TRICKPLAY_MAX_SHEETS, TRICKPLAY_MAX_SHEET_BYTES, TRICKPLAY_MAX_SHEET_DIMENSION,
    TRICKPLAY_MAX_SHEET_PIXELS, TRICKPLAY_MIN_FRAME_DIMENSION, TRICKPLAY_SCHEMA_VERSION,
    TRICKPLAY_SHEET_COLUMNS, TRICKPLAY_SHEET_ROWS, TRICKPLAY_TARGET_FRAMES,
};
pub use xml::{escape_xml, escape_xml_into, escape_xml_text, escape_xml_text_into};

pub const SERVER_NAME: &str = "rustyDLNA";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `Server:` product token. Keep `DLNADOC/1.50 UPnP/1.0` — clients parse those tokens.
pub fn server_header(os_version: &str) -> String {
    format!("{os_version} DLNADOC/1.50 UPnP/1.0 {SERVER_NAME}/{SERVER_VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_header_has_dlna_tokens() {
        let s = server_header("Linux");
        assert!(s.contains("DLNADOC/1.50"));
        assert!(s.contains("UPnP/1.0"));
        assert!(s.contains("rustyDLNA/"));
    }
}
