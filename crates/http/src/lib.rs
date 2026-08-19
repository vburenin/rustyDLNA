//! HTTP request classification, description XML, and media Range helpers.
//! The accept loop lives in `rusty-dlna`.

pub mod desc;
pub mod range;
pub mod request;

use rusty_dlna_protocol::http_should_persist;
use rusty_dlna_protocol::paths::{
    caption_default_url, ALBUM_ART_PREFIX, API_STATUS_PATH, CAPTIONS_PREFIX,
    CONNECTIONMGR_CONTROLURL, CONNECTIONMGR_PATH, CONTENTDIRECTORY_CONTROLURL,
    CONTENTDIRECTORY_PATH, HEALTH_PATH, ICONS_PREFIX, MEDIA_ITEMS_PREFIX, RESIZED_PREFIX,
    ROOTDESC_PATH, STATUS_PATH, THUMBNAILS_PREFIX, TRANSCODE_PREFIX,
    X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL, X_MS_MEDIARECEIVERREGISTRAR_PATH,
};

pub use desc::{
    gen_root_desc, minimal_scpd, scpd_connection_manager, scpd_content_directory, scpd_registrar,
    RootDescOpts, DEVICE_TYPE,
};
pub use range::{
    parse_byte_range, parse_open_range, range_len, read_file_range, ByteRange, RangeError,
};
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
    Health,
    ApiStatus,
    Presentation,
    NotFound,
}

pub fn route(method: &str, path: &str) -> HttpRoute {
    if method.eq_ignore_ascii_case("POST") {
        // The dialect ignores the POST target and keys only off SOAPAction.
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
        HEALTH_PATH => HttpRoute::Health,
        API_STATUS_PATH => HttpRoute::ApiStatus,
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

/// Media GET never persists (the dialect forks + Connection: close).
pub fn persist_for_route(
    route: HttpRoute,
    httpver: Option<&str>,
    conn_close: bool,
    conn_keep: bool,
) -> bool {
    if route == HttpRoute::MediaItem || route == HttpRoute::Transcode {
        return false;
    }
    http_should_persist(httpver, conn_close, conn_keep)
}

/// Incoming request body cap (SOAP / GENA). Media GET has no body.
/// Absolute safety ceiling. The server's configurable default is lower.
pub const MAX_HTTP_BODY: usize = 16 * 1024 * 1024;

pub fn http_body_too_large(len: usize) -> bool {
    len > MAX_HTTP_BODY
}

/// Host must be dotted IPv4 (dialect DNS-rebinding check).
pub fn valid_host_header(host: &str) -> bool {
    let (name, port) = match host.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host, None),
    };
    if let Some(p) = port {
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if !matches!(p.parse::<u32>(), Ok(0..=65535)) {
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

/// `getcontentFeatures.dlna.org` / `getAvailableSeekRange.dlna.org` must be `1`.
pub fn dlna_get_header_invalid(req: &HttpRequest) -> bool {
    for name in [
        "getcontentFeatures.dlna.org",
        "getAvailableSeekRange.dlna.org",
    ] {
        if let Some(v) = req.header(name) {
            if v.trim() != "1" {
                return true;
            }
        }
    }
    false
}

pub fn wants_content_language(req: &HttpRequest) -> bool {
    req.header("Accept-Language").is_some()
}

pub fn is_chunked(req: &HttpRequest) -> bool {
    req.header("Transfer-Encoding").is_some_and(|v| {
        v.to_ascii_lowercase()
            .split(',')
            .any(|t| t.trim() == "chunked")
    })
}

pub fn dlna_strict(req: &HttpRequest) -> bool {
    req.header("uctt.upnp.org").is_some()
}

/// `realTimeInfo` + `Interactive` → 400 (DLNA).
pub fn realtime_interactive_invalid(req: &HttpRequest) -> bool {
    req.header("realTimeInfo.dlna.org").is_some()
        && req
            .header("transferMode.dlna.org")
            .is_some_and(|v| v.eq_ignore_ascii_case("Interactive"))
}

/// `transferMode: Streaming` on an image → 406.
pub fn streaming_on_image(req: &HttpRequest, mime: &str) -> bool {
    mime.starts_with("image/")
        && req
            .header("transferMode.dlna.org")
            .is_some_and(|v| v.eq_ignore_ascii_case("Streaming"))
}

/// `Interactive` on a non-image `/MediaItems/` object → 406, except Samsung
/// unless `uctt.upnp.org` is set.
pub fn interactive_on_non_image(
    req: &HttpRequest,
    mime: &str,
    samsung: bool,
    strict: bool,
) -> bool {
    if mime.starts_with("image/") {
        return false;
    }
    if !req
        .header("transferMode.dlna.org")
        .is_some_and(|v| v.eq_ignore_ascii_case("Interactive"))
    {
        return false;
    }
    if samsung && !strict {
        return false;
    }
    true
}

/// IMF-fixdate GMT (`Date:` header).
pub fn imf_fixdate_gmt(unix: i64) -> String {
    let t = rusty_dlna_protocol::utc_date_time(unix);
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[t.weekday_from_sunday as usize],
        t.day,
        MONTHS[t.month as usize - 1],
        t.year,
        t.hour,
        t.minute,
        t.second
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

pub fn protocol_info(
    mime: &str,
    pn: Option<&str>,
    dlna_client: bool,
    skip_pn: bool,
    ci: u8,
) -> String {
    if !dlna_client {
        return format!("http-get:*:{mime}:*");
    }
    let pn = if skip_pn { None } else { pn };
    format!(
        "http-get:*:{mime}:{}",
        dlna_org_features(pn, "01", ci, mime)
    )
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
    pub file_range: Option<OpenFileRange>,
    /// When set, accept loop starts/attaches a background remux and streams
    /// the growing (or finished) dest. Probe disconnect must not kill ffmpeg.
    pub remux_job: Option<RemuxJobSpec>,
}

/// A response range backed by an already validated/open descriptor. Keeping
/// the descriptor in the response prevents symlink replacement between route
/// authorization, header generation, and body streaming.
#[derive(Clone, Debug)]
pub struct OpenFileRange {
    pub file: std::sync::Arc<std::fs::File>,
    pub path: std::path::PathBuf,
    pub start: u64,
    pub end: u64,
}

/// One `/Transcode/{id}` serve: ffmpeg writes `dest` (via `args`) in the
/// background; every concurrent GET shares that job.
#[derive(Clone, Debug)]
pub struct RemuxJobSpec {
    pub detail_id: i64,
    /// Complete immutable identity used for in-process job sharing.
    pub job_key: String,
    /// Source/plan/tool digest stored beside the finished cache file.
    pub cache_key: String,
    pub src: std::path::PathBuf,
    /// Root-validated source descriptor. Production jobs always set this;
    /// `None` is retained for synthetic command lifecycle tests.
    pub source_file: Option<std::sync::Arc<std::fs::File>>,
    pub dest: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
    /// Try `dovi_tool` P8.1 convert first; `args` is the hdr10 fallback.
    pub remux_p8: bool,
    pub audio_index: usize,
    pub audio: RemuxAudio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemuxAudio {
    Copy,
    Ac3,
    Aac,
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
            remux_job: None,
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
        let mut r = Self::new(
            status,
            if status == 200 {
                "OK"
            } else {
                "Internal Server Error"
            },
        );
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

/// Samsung media GET request header (any value, typically `1`).
pub const GET_CAPTION_INFO_SEC: &str = "getCaptionInfo.sec";
/// Response header naming the default caption URL.
pub const CAPTION_INFO_SEC: &str = "CaptionInfo.sec";

/// True when the client asked for `CaptionInfo.sec` on this media GET.
pub fn wants_caption_info_sec(req: &HttpRequest) -> bool {
    req.header(GET_CAPTION_INFO_SEC).is_some()
}

/// `http://{host}:{port}/Captions/{detail_id}.srt` (first/default caption).
pub fn caption_info_sec_url(host: &str, port: u16, detail_id: i64) -> String {
    caption_default_url(host, port, detail_id)
}

pub fn set_caption_info_sec(r: &mut HttpResponse, url: &str) {
    r.set(CAPTION_INFO_SEC, url);
}

/// Original `/MediaItems/` headers: `Accept-Ranges: bytes`, `OP=01`, `CI=0`,
/// `Connection: close`. Empty `DLNA_PN` on HEVC MKV remux.
pub struct MediaResponseOptions<'a> {
    pub server: &'a str,
    pub date: &'a str,
    pub mime: &'a str,
    pub size: u64,
    pub range: Option<ByteRange>,
    pub body: Vec<u8>,
    pub pn: Option<&'a str>,
    pub ci: u8,
}

pub fn media_response(options: MediaResponseOptions<'_>) -> HttpResponse {
    let MediaResponseOptions {
        server,
        date,
        mime,
        size,
        range,
        body,
        pn,
        ci,
    } = options;
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

/// Live remux: fragmented MP4 on a pipe. No `Content-Length`, no byte
/// seek — the player starts as soon as the first fragment is ready.
pub fn live_transcode_response(mime: &str) -> HttpResponse {
    let mut r = HttpResponse::new(200, "OK");
    r.persist = false;
    r.set("realTimeInfo.dlna.org", "DLNA.ORG_TLAG=*");
    r.set("transferMode.dlna.org", "Streaming");
    r.set("Content-Type", mime);
    r.set(
        "contentFeatures.dlna.org",
        dlna_org_features(None, "00", 1, mime),
    );
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
        assert_eq!(route("GET", "/health"), HttpRoute::Health);
        assert_eq!(route("GET", "/api/status"), HttpRoute::ApiStatus);
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
        assert!(!persist_for_route(
            HttpRoute::Transcode,
            Some("HTTP/1.1"),
            false,
            true
        ));
    }

    #[test]
    fn soap_uses_normal_http_persistence() {
        assert!(persist_for_route(
            HttpRoute::Soap,
            Some("HTTP/1.1"),
            false,
            false
        ));
        assert!(!persist_for_route(
            HttpRoute::Soap,
            Some("HTTP/1.1"),
            true,
            true
        ));
        assert!(persist_for_route(
            HttpRoute::Soap,
            Some("HTTP/1.0"),
            false,
            true
        ));
        assert!(!persist_for_route(
            HttpRoute::Soap,
            Some("HTTP/1.0"),
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

    #[test]
    fn request_body_has_a_hard_16_mib_ceiling() {
        assert_eq!(MAX_HTTP_BODY, 16 * 1024 * 1024);
        assert!(!http_body_too_large(0));
        assert!(!http_body_too_large(MAX_HTTP_BODY));
        assert!(http_body_too_large(MAX_HTTP_BODY + 1));
    }

    #[test]
    fn caption_info_sec_header_helper() {
        let req = HttpRequest::parse_headers(
            "GET /MediaItems/9.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\ngetCaptionInfo.sec: 1\r\n\r\n",
        )
        .unwrap();
        assert!(wants_caption_info_sec(&req));
        let url = caption_info_sec_url("192.0.2.10", 18200, 9);
        assert_eq!(url, "http://192.0.2.10:18200/Captions/9.srt");
        let mut r = HttpResponse::new(200, "OK");
        set_caption_info_sec(&mut r, &url);
        assert_eq!(
            r.headers
                .iter()
                .find(|(k, _)| k == CAPTION_INFO_SEC)
                .map(|(_, v)| v.as_str()),
            Some(url.as_str())
        );
    }

    #[test]
    fn getcontentfeatures_must_be_one() {
        let bad = HttpRequest::parse_headers(
            "GET /MediaItems/1.mkv HTTP/1.1\r\nHost: 127.0.0.1\r\ngetcontentFeatures.dlna.org: 0\r\n\r\n",
        )
        .unwrap();
        assert!(dlna_get_header_invalid(&bad));
        let ok = HttpRequest::parse_headers(
            "GET /MediaItems/1.mkv HTTP/1.1\r\nHost: 127.0.0.1\r\ngetcontentFeatures.dlna.org: 1\r\n\r\n",
        )
        .unwrap();
        assert!(!dlna_get_header_invalid(&ok));
        let seek = HttpRequest::parse_headers(
            "GET /MediaItems/1.mkv HTTP/1.1\r\nHost: 127.0.0.1\r\ngetAvailableSeekRange.dlna.org: 2\r\n\r\n",
        )
        .unwrap();
        assert!(dlna_get_header_invalid(&seek));
    }

    #[test]
    fn interactive_on_non_image_except_samsung() {
        let req = HttpRequest::parse_headers(
            "GET /MediaItems/1.mkv HTTP/1.1\r\nHost: 127.0.0.1\r\ntransferMode.dlna.org: Interactive\r\n\r\n",
        )
        .unwrap();
        assert!(interactive_on_non_image(
            &req,
            "video/x-matroska",
            false,
            false
        ));
        assert!(!interactive_on_non_image(
            &req,
            "video/x-matroska",
            true,
            false
        ));
        assert!(interactive_on_non_image(
            &req,
            "video/x-matroska",
            true,
            true
        ));
        assert!(!interactive_on_non_image(&req, "image/jpeg", false, false));
    }

    #[test]
    fn skip_dlna_pn_omits_pn_from_content_features() {
        let with_pn = dlna_org_features(Some("AVC_MP4_MP_SD_AC3"), "01", 0, "video/mp4");
        assert!(
            with_pn.contains("DLNA.ORG_PN=AVC_MP4_MP_SD_AC3"),
            "{with_pn}"
        );
        let skip = protocol_info("video/mp4", Some("AVC_MP4_MP_SD_AC3"), true, true, 0);
        assert!(!skip.contains("DLNA.ORG_PN="), "{skip}");
        assert!(skip.contains("DLNA.ORG_OP=01"), "{skip}");
    }
}
