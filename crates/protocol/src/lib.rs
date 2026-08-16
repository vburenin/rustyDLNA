//! MiniDLNA-compatible protocol dialect.
//!
//! Constants and helpers here are the on-the-wire contract documented in
//! `docs/replica.md`. A rewrite that changes these without a client reason
//! will break TVs that already work.

pub mod clients;
pub mod date;
pub mod isolation;
pub mod object_id;
pub mod paths;
pub mod persist;
pub mod soap;
pub mod ssdp;

#[cfg(test)]
mod oracle;

pub use clients::{
    identify_friendly_name, identify_user_agent, identify_x_av_client_info, remap_mime,
    ClientFlags, ClientKind, ClientProfile, MatchKind, CLIENTS,
};
pub use date::{w3c_date_from_unix, w3c_normalize_date};
pub use isolation::{
    collides_with_live_minidlna, test_listen_ports, LIVE_MINIDLNA_HTTP_PORT, LIVE_MINIDLNA_SSDP_PORT,
    TEST_HTTP_PORT, TEST_SSDP_PORT,
};
pub use paths::{
    caption_from_path, media_item_id_from_path, strtoll_prefix, transcode_id_from_path,
};
pub use persist::http_should_persist;

pub const SERVER_NAME: &str = "rustyDLNA";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `Server:` product token. MiniDLNA used `MiniDLNA/1.3.3-kodi`.
/// Keep `DLNADOC/1.50 UPnP/1.0` — clients parse those tokens.
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
