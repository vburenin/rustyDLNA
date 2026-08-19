//! rustyDLNA on-the-wire protocol dialect.
//!
//! Constants and helpers here are the contract documented in
//! `docs/replica.md`. A rewrite that changes these without a client reason
//! will break TVs that already work.

pub mod client_cache;
pub mod clients;
pub mod date;
pub mod isolation;
pub mod media_format;
pub mod object_id;
pub mod paths;
pub mod persist;
pub mod soap;
pub mod ssdp;

#[cfg(test)]
mod oracle;

pub use client_cache::{ClientCache, ClientCacheEntry, CLIENT_CACHE_SLOTS, CLIENT_CACHE_TTL_SECS};
pub use clients::{
    identify_friendly_name, identify_friendly_name_ssdp, identify_model_name, identify_request,
    identify_user_agent, identify_x_av_client_info, remap_mime, remap_mime_full, ClientFlags,
    ClientKind, ClientProfile, MatchKind, CLIENTS,
};
pub use date::{utc_date_time, w3c_date_from_unix, w3c_normalize_date, UtcDateTime};
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
