//! HTTP request classification, description XML, and media Range helpers.
//! The accept loop lives in `rusty-dlna`.

pub mod desc;
pub mod range;
pub mod request;

use rusty_dlna_protocol::http_should_persist;
use rusty_dlna_protocol::paths::{
    ALBUM_ART_PREFIX, CAPTIONS_PREFIX, CONNECTIONMGR_CONTROLURL, CONNECTIONMGR_PATH,
    CONTENTDIRECTORY_CONTROLURL, CONTENTDIRECTORY_PATH, ICONS_PREFIX, MEDIA_ITEMS_PREFIX,
    RESIZED_PREFIX, ROOTDESC_PATH, STATUS_PATH, THUMBNAILS_PREFIX, TRANSCODE_PREFIX,
    X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL, X_MS_MEDIARECEIVERREGISTRAR_PATH,
};

pub use desc::{
    gen_root_desc, minimal_scpd, scpd_connection_manager, scpd_content_directory, scpd_registrar,
    RootDescOpts, DEVICE_TYPE,
};
pub use range::{parse_byte_range, range_len, read_file_range, ByteRange, RangeError};
pub use request::{header_block_complete, HttpRequest, ParseError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpRoute {
    RootDesc,
    ScpdContentDir,
    ScpdConnectionMgr,
    ScpdRegistrar,
    Soap,
    EventContentDir,
    EventConnectionMgr,
    EventRegistrar,
    MediaItem,
    Transcode,
    Thumbnail,
    AlbumArt,
    Resized,
    Icon,
    Caption,
    Status,
    Presentation,
    NotFound,
}

pub fn route(method: &str, path: &str) -> HttpRoute {
    if method.eq_ignore_ascii_case("POST") {
        // MiniDLNA ignores the POST target and keys only off SOAPAction.
        return HttpRoute::Soap;
    }
    if method.eq_ignore_ascii_case("SUBSCRIBE") || method.eq_ignore_ascii_case("UNSUBSCRIBE") {
        return match path {
            rusty_dlna_protocol::paths::CONTENTDIRECTORY_EVENTURL => HttpRoute::EventContentDir,
            rusty_dlna_protocol::paths::CONNECTIONMGR_EVENTURL => HttpRoute::EventConnectionMgr,
            rusty_dlna_protocol::paths::X_MS_MEDIARECEIVERREGISTRAR_EVENTURL => {
                HttpRoute::EventRegistrar
            }
            _ => HttpRoute::NotFound,
        };
    }
    match path {
        ROOTDESC_PATH => HttpRoute::RootDesc,
        CONTENTDIRECTORY_PATH => HttpRoute::ScpdContentDir,
        CONNECTIONMGR_PATH => HttpRoute::ScpdConnectionMgr,
        X_MS_MEDIARECEIVERREGISTRAR_PATH => HttpRoute::ScpdRegistrar,
        CONTENTDIRECTORY_CONTROLURL
        | CONNECTIONMGR_CONTROLURL
        | X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL => HttpRoute::Soap,
        STATUS_PATH => HttpRoute::Status,
        "/" => HttpRoute::Presentation,
        p if p.starts_with(MEDIA_ITEMS_PREFIX) => HttpRoute::MediaItem,
        p if p.starts_with(TRANSCODE_PREFIX) => HttpRoute::Transcode,
        p if p.starts_with(THUMBNAILS_PREFIX) => HttpRoute::Thumbnail,
        p if p.starts_with(ALBUM_ART_PREFIX) => HttpRoute::AlbumArt,
        p if p.starts_with(RESIZED_PREFIX) => HttpRoute::Resized,
        p if p.starts_with(ICONS_PREFIX) => HttpRoute::Icon,
        p if p.starts_with(CAPTIONS_PREFIX) => HttpRoute::Caption,
        _ => HttpRoute::NotFound,
    }
}

/// Media GET never persists (MiniDLNA forks + Connection: close).
pub fn persist_for_route(
    route: HttpRoute,
    httpver: Option<&str>,
    conn_close: bool,
    conn_keep: bool,
) -> bool {
    if route == HttpRoute::MediaItem
        || route == HttpRoute::Transcode
        || route == HttpRoute::Soap
    {
        return false;
    }
    http_should_persist(httpver, conn_close, conn_keep)
}

/// Host must be dotted IPv4 (MiniDLNA DNS-rebinding check).
pub fn valid_host_header(host: &str) -> bool {
    let (name, port) = match host.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host, None),
    };
    if let Some(p) = port {
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if p.parse::<u32>().ok().is_none_or(|n| n > 65535) {
            return false;
        }
    }
    if matches!(name.parse::<std::net::Ipv4Addr>(), Ok(a) if !a.is_unspecified()) {
        return true;
    }
    false
}

pub fn timeseek_without_range(req: &HttpRequest) -> bool {
    let seek = req.header("TimeSeekRange.dlna.org").is_some()
        || req.header("PlaySpeed.dlna.org").is_some();
    seek && req.header("Range").is_none()
}

/// IMF-fixdate GMT (`Date:` header).
pub fn imf_fixdate_gmt(unix: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(unix)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[t.weekday().number_days_from_sunday() as usize],
        t.day(),
        MONTHS[u8::from(t.month()) as usize - 1],
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

pub fn now_imf_date() -> String {
    imf_fixdate_gmt(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

/// 8-hex DLNA flags + 24 zero hex. Video/audio get TM_S.
pub fn dlna_flags_hex(mime: &str) -> String {
    let flags = if mime.starts_with("image/") {
        0x00F0_0000u32
    } else {
        0x0170_0000u32
    };
    format!("{flags:08X}{:024}", 0)
}

/// `contentFeatures.dlna.org` / protocolInfo tail. Empty PN on HEVC MKV remux.
pub fn dlna_org_features(pn: Option<&str>, op: &str, ci: u8, mime: &str) -> String {
    let flags = dlna_flags_hex(mime);
    match pn {
        Some(p) if !p.is_empty() => {
            format!("DLNA.ORG_PN={p};DLNA.ORG_OP={op};DLNA.ORG_CI={ci};DLNA.ORG_FLAGS={flags}")
        }
        _ => format!("DLNA.ORG_OP={op};DLNA.ORG_CI={ci};DLNA.ORG_FLAGS={flags}"),
    }
}

pub fn protocol_info(mime: &str, pn: Option<&str>, dlna_client: bool, skip_pn: bool, ci: u8) -> String {
    if !dlna_client {
        return format!("http-get:*:{mime}:*");
    }
    let pn = if skip_pn { None } else { pn };
    format!("http-get:*:{mime}:{}", dlna_org_features(pn, "01", ci, mime))
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub persist: bool,
    /// When set, `body` is empty and the accept loop streams this file range
    /// (inclusive). Used so an 80 GiB remux is never slurp'd into RAM.
    pub file_range: Option<(std::path::PathBuf, u64, u64)>,
}

impl HttpResponse {
    pub fn new(status: u16, reason: &str) -> Self {
        Self {
            status,
            reason: reason.into(),
            headers: Vec::new(),
            body: Vec::new(),
            persist: false,
            file_range: None,
        }
    }

    pub fn set(&mut self, k: &str, v: impl ToString) {
        self.headers.push((k.into(), v.to_string()));
    }

    pub fn html(status: u16, reason: &str, msg: &str) -> Self {
        let body = format!("<HTML><HEAD><TITLE>{status} {reason}</TITLE></HEAD><BODY><H1>{status} {reason}</H1>{msg}</BODY></HTML>");
        let mut r = Self::new(status, reason);
        r.set("Content-Type", "text/html");
        r.body = body.into_bytes();
        r.set("Content-Length", r.body.len());
        r
    }

    pub fn xml(status: u16, body: String, persist: bool) -> Self {
        let mut r = Self::new(status, if status == 200 { "OK" } else { "Internal Server Error" });
        r.set("Content-Type", r#"text/xml; charset="utf-8""#);
        r.body = body.into_bytes();
        r.set("Content-Length", r.body.len());
        r.persist = persist;
        r
    }

    pub fn bytes_wire(&self, server: &str, date: &str) -> Vec<u8> {
        let conn = if self.persist { "keep-alive" } else { "close" };
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nConnection: {conn}\r\nDate: {date}\r\nServer: {server}\r\nEXT:\r\n",
            self.status, self.reason
        );
        let mut has_server = false;
        let mut has_date = false;
        let mut has_conn = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("Server") {
                has_server = true;
            }
            if k.eq_ignore_ascii_case("Date") {
                has_date = true;
            }
            if k.eq_ignore_ascii_case("Connection") {
                has_conn = true;
            }
            if k.eq_ignore_ascii_case("Server")
                || k.eq_ignore_ascii_case("Date")
                || k.eq_ignore_ascii_case("Connection")
                || k.eq_ignore_ascii_case("EXT")
            {
                continue;
            }
            out.push_str(k);
            out.push_str(": ");
            out.push_str(v);
            out.push_str("\r\n");
        }
        let _ = (has_server, has_date, has_conn);
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Original `/MediaItems/` headers: `Accept-Ranges: bytes`, `OP=01`, `CI=0`,
/// `Connection: close`. Empty `DLNA_PN` on HEVC MKV remux.
pub fn media_response(
    server: &str,
    date: &str,
    mime: &str,
    size: u64,
    range: Option<ByteRange>,
    body: Vec<u8>,
    pn: Option<&str>,
    ci: u8,
) -> HttpResponse {
    let (status, reason, clen, crange) = match range {
        None => (200u16, "OK", size, None),
        Some(r) => (
            206,
            "OK",
            range_len(r),
            Some(format!("bytes {}-{}/{}", r.start, r.end, size)),
        ),
    };
    let mut r = HttpResponse::new(status, reason);
    r.persist = false;
    r.set("realTimeInfo.dlna.org", "DLNA.ORG_TLAG=*");
    r.set(
        "transferMode.dlna.org",
        if mime.starts_with("image/") {
            "Interactive"
        } else {
            "Streaming"
        },
    );
    r.set("Content-Type", mime);
    r.set("Content-Length", clen);
    if let Some(cr) = crange {
        r.set("Content-Range", cr);
    }
    r.set("Accept-Ranges", "bytes");
    r.set(
        "contentFeatures.dlna.org",
        dlna_org_features(pn, "01", ci, mime),
    );
    r.body = body;
    let _ = (server, date);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch() {
        assert_eq!(route("GET", "/rootDesc.xml"), HttpRoute::RootDesc);
        assert_eq!(route("GET", "/MediaItems/42.mkv"), HttpRoute::MediaItem);
        assert_eq!(route("POST", "/whatever"), HttpRoute::Soap);
        assert_eq!(route("GET", "/ctl/ContentDir"), HttpRoute::Soap);
        assert_eq!(route("GET", "/Captions/9/0.srt"), HttpRoute::Caption);
        assert_eq!(route("GET", "/Transcode/3.mp4"), HttpRoute::Transcode);
    }

    #[test]
    fn media_never_keepalive() {
        assert!(!persist_for_route(
            HttpRoute::MediaItem,
            Some("HTTP/1.1"),
            false,
            true
        ));
        assert!(persist_for_route(
            HttpRoute::RootDesc,
            Some("HTTP/1.1"),
            false,
            false
        ));
    }

    #[test]
    fn host_rebinding() {
        assert!(valid_host_header("192.0.2.1:8200"));
        assert!(valid_host_header("127.0.0.1"));
        assert!(!valid_host_header("localhost"));
        assert!(!valid_host_header("0.0.0.0"));
        assert!(!valid_host_header("192.0.2.1:99999"));
    }
}
