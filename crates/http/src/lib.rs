//! HTTP request classification, description XML, and media Range helpers.
//! The accept loop lives in `rusty-dlna`.

pub mod desc;
pub mod range;
pub mod request;

use rusty_dlna_protocol::paths::{
    caption_default_url, ALBUM_ART_PREFIX, API_STATUS_PATH, CAPTIONS_PREFIX,
    CONNECTIONMGR_CONTROLURL, CONNECTIONMGR_PATH, CONTENTDIRECTORY_CONTROLURL,
    CONTENTDIRECTORY_PATH, HEALTH_PATH, ICONS_PREFIX, MEDIA_ITEMS_PREFIX, RESIZED_PREFIX,
    ROOTDESC_PATH, STATUS_PATH, THUMBNAILS_PREFIX, TRANSCODE_PREFIX,
    X_MS_MEDIARECEIVERREGISTRAR_CONTROLURL, X_MS_MEDIARECEIVERREGISTRAR_PATH,
};
use rusty_dlna_protocol::{
    escape_xml, http_should_persist, is_http_field_value, is_http_token, trim_http_ows,
};

pub use desc::{
    gen_root_desc, minimal_scpd, scpd_connection_manager, scpd_content_directory, scpd_registrar,
    RootDescOpts, DEVICE_TYPE,
};
pub use range::{
    parse_byte_range, parse_open_range, range_len, read_file_range, ByteRange, RangeError,
};
pub use request::{header_block_complete, HttpRequest, ParseError};

#[repr(usize)]
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
    WebLibrary,
    WebItem,
    WebTranscodeStatus,
    WebMedia,
    WebAsset,
    Presentation,
    WebDownload,
    NotFound,
}

impl HttpRoute {
    /// Number of closed-set route classes.
    pub const COUNT: usize = Self::NotFound as usize + 1;

    /// Every route class in telemetry index order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::RootDesc,
        Self::ScpdContentDir,
        Self::ScpdConnectionMgr,
        Self::ScpdRegistrar,
        Self::Soap,
        Self::EventContentDir,
        Self::EventConnectionMgr,
        Self::EventRegistrar,
        Self::MediaItem,
        Self::Transcode,
        Self::Thumbnail,
        Self::AlbumArt,
        Self::Resized,
        Self::Icon,
        Self::Caption,
        Self::Status,
        Self::Health,
        Self::ApiStatus,
        Self::WebLibrary,
        Self::WebItem,
        Self::WebTranscodeStatus,
        Self::WebMedia,
        Self::WebAsset,
        Self::Presentation,
        Self::WebDownload,
        Self::NotFound,
    ];

    /// Stable array index for fixed-cardinality server telemetry.
    pub const fn index(self) -> usize {
        self as usize
    }
}

pub fn route(method: &str, path: &str) -> HttpRoute {
    if method.eq_ignore_ascii_case("POST") {
        if path.starts_with("/api/web/transcode/") {
            return HttpRoute::WebTranscodeStatus;
        }
        // All other POST targets are SOAP requests keyed by SOAPAction.
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
        "/api/web/library" => HttpRoute::WebLibrary,
        "/" => HttpRoute::Presentation,
        p if p.starts_with("/api/web/transcode/") => HttpRoute::WebTranscodeStatus,
        p if p.starts_with("/api/web/item/") => HttpRoute::WebItem,
        p if p.starts_with("/api/web/preview/") => HttpRoute::WebItem,
        p if p.starts_with("/web/preview/") => HttpRoute::WebAsset,
        p if p.starts_with("/web/media/") => HttpRoute::WebMedia,
        p if p.starts_with("/web/download/") => HttpRoute::WebDownload,
        "/favicon.ico"
        | "/web/app.css"
        | "/web/app.js"
        | "/web/api.js"
        | "/web/core.js"
        | "/web/library.js"
        | "/web/player.js"
        | "/web/preferences.js"
        | "/web/store.js" => HttpRoute::WebAsset,
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

/// Media GET never persists; streamed responses use `Connection: close`.
pub fn persist_for_route(
    route: HttpRoute,
    httpver: Option<&str>,
    conn_close: bool,
    conn_keep: bool,
) -> bool {
    if matches!(
        route,
        HttpRoute::MediaItem | HttpRoute::Transcode | HttpRoute::WebMedia | HttpRoute::WebDownload
    ) {
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

/// Host must be dotted IPv4 (rustyDLNA DNS-rebinding check).
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
            .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
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

/// An outbound response cannot be represented safely as HTTP/1.1 wire bytes.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseWireError {
    /// The status code is outside HTTP's three-digit range.
    InvalidStatus(u16),
    /// The reason phrase contains a forbidden field-value byte.
    InvalidReason,
    /// The supplied `Server` value is empty or contains a forbidden byte.
    InvalidServer,
    /// The supplied `Date` value is empty or contains a forbidden byte.
    InvalidDate,
    /// A response header name is not an HTTP token.
    InvalidHeaderName { index: usize },
    /// A response header value contains a forbidden byte.
    InvalidHeaderValue { index: usize },
    /// A `Content-Length` value is not one unsigned decimal integer.
    InvalidContentLength { index: usize },
    /// More than one `Content-Length` field was supplied.
    DuplicateContentLength,
    /// A non-empty in-memory body disagrees with its declared length.
    ContentLengthMismatch { declared: u64, actual: usize },
    /// A persistent response has a non-empty body but no framing length.
    PersistentBodyWithoutContentLength,
    /// `Transfer-Encoding` and `Content-Length` were both supplied.
    TransferEncodingWithContentLength,
    /// rustyDLNA does not serialize transfer-coded response bodies.
    UnsupportedTransferEncoding,
}

impl std::fmt::Display for ResponseWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidStatus(status) => write!(f, "invalid HTTP status {status}"),
            Self::InvalidReason => f.write_str("invalid HTTP reason phrase"),
            Self::InvalidServer => f.write_str("invalid HTTP Server value"),
            Self::InvalidDate => f.write_str("invalid HTTP Date value"),
            Self::InvalidHeaderName { index } => {
                write!(f, "invalid HTTP response header name at index {index}")
            }
            Self::InvalidHeaderValue { index } => {
                write!(f, "invalid HTTP response header value at index {index}")
            }
            Self::InvalidContentLength { index } => {
                write!(f, "invalid Content-Length at header index {index}")
            }
            Self::DuplicateContentLength => f.write_str("duplicate Content-Length"),
            Self::ContentLengthMismatch { declared, actual } => write!(
                f,
                "Content-Length {declared} does not match response body length {actual}"
            ),
            Self::PersistentBodyWithoutContentLength => {
                f.write_str("persistent response body has no Content-Length")
            }
            Self::TransferEncodingWithContentLength => {
                f.write_str("Transfer-Encoding conflicts with Content-Length")
            }
            Self::UnsupportedTransferEncoding => {
                f.write_str("response Transfer-Encoding is unsupported")
            }
        }
    }
}

impl std::error::Error for ResponseWireError {}

const INVALID_RESPONSE_WIRE: &[u8] =
    b"HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

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
    /// Stable browser player identity shared by every source generation for
    /// one selected title. A newer generation supersedes older jobs owned by
    /// the same player without disturbing other players sharing the output.
    pub web_session_id: Option<u64>,
    /// Browser source-generation identifier used for pollable status and
    /// explicit cancellation. Reconnects for one source reuse the same ID.
    pub web_request_id: Option<u64>,
    /// HTTP media type for both growing and completed cached output.
    pub mime: &'static str,
    /// Complete immutable identity used for in-process job sharing.
    pub job_key: String,
    /// Source/plan/tool digest stored beside the finished cache file.
    pub cache_key: String,
    pub src: std::path::PathBuf,
    /// Root-validated source descriptor. Production jobs always set this;
    /// `None` is retained for synthetic command lifecycle tests.
    pub source_file: Option<std::sync::Arc<std::fs::File>>,
    /// Immutable libplacebo custom shader inherited at the transcode crate's
    /// reserved browser AI-upscale descriptor, when that filter is selected.
    pub ai_upscale_shader_file: Option<std::sync::Arc<std::fs::File>>,
    pub dest: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
    /// Optional portable command used when negotiated stream copying or a
    /// hardware producer fails before emitting a playable first fragment.
    pub fallback_args: Option<Vec<std::ffi::OsString>>,
    /// Whether the producer may keep running after its last HTTP reader goes
    /// away. DLNA cache jobs may opt in; interactive browser streams do not.
    pub continue_after_disconnect: bool,
    /// Whether a completed output may remain in the shared transcode cache.
    /// Seek tails are ephemeral so historical seeks cannot consume the cache.
    pub cacheable: bool,
    /// Every completed movie fragment is guaranteed to begin at a random-
    /// access point, so native HLS may publish it without waiting for the next
    /// fragment boundary. False retains stream-aware segment coalescing.
    pub hls_all_fragments_independent: bool,
    /// Try `dovi_tool` P8.1 convert first; `args` is the hdr10 fallback.
    pub remux_p8: bool,
    /// Exact verified ffmpeg represented by the cache identity. Production
    /// jobs always set this; `None` is only for synthetic command tests.
    pub verified_ffmpeg: Option<rusty_dlna_transcode::VerifiedExecutable>,
    /// Exact verified executables represented by a Profile-8 cache identity.
    pub profile8_toolchain: Option<rusty_dlna_transcode::Profile8ToolchainSnapshot>,
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

    /// Set one response field, replacing every existing case-insensitive
    /// occurrence so singleton fields cannot become ambiguous accidentally.
    pub fn set(&mut self, k: &str, v: impl ToString) {
        let value = v.to_string();
        let Some(first) = self
            .headers
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(k))
        else {
            self.headers.push((k.into(), value));
            return;
        };
        self.headers[first] = (k.into(), value);
        let mut index = first + 1;
        while index < self.headers.len() {
            if self.headers[index].0.eq_ignore_ascii_case(k) {
                self.headers.remove(index);
            } else {
                index += 1;
            }
        }
    }

    /// Append a response field without replacing an existing value.
    /// Callers must use this only for fields whose wire grammar permits it;
    /// response framing validation still rejects ambiguous singleton fields.
    pub fn append(&mut self, k: &str, v: impl ToString) {
        self.headers.push((k.into(), v.to_string()));
    }

    pub fn html(status: u16, reason: &str, msg: &str) -> Self {
        let reason_html = escape_xml(reason);
        let message_html = escape_xml(msg);
        let body = format!(
            "<HTML><HEAD><TITLE>{status} {reason_html}</TITLE></HEAD><BODY><H1>{status} {reason_html}</H1>{message_html}</BODY></HTML>"
        );
        let mut r = Self::new(status, reason);
        r.set("Content-Type", "text/html; charset=utf-8");
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

    /// Serialize a response after validating every public wire field.
    pub fn try_bytes_wire(&self, server: &str, date: &str) -> Result<Vec<u8>, ResponseWireError> {
        if !(100..=599).contains(&self.status) {
            return Err(ResponseWireError::InvalidStatus(self.status));
        }
        if !is_http_field_value(&self.reason) {
            return Err(ResponseWireError::InvalidReason);
        }
        if trim_http_ows(server).is_empty() || !is_http_field_value(server) {
            return Err(ResponseWireError::InvalidServer);
        }
        if trim_http_ows(date).is_empty() || !is_http_field_value(date) {
            return Err(ResponseWireError::InvalidDate);
        }

        let mut content_length = None;
        let mut transfer_encoding = false;
        for (index, (name, value)) in self.headers.iter().enumerate() {
            if !is_http_token(name) {
                return Err(ResponseWireError::InvalidHeaderName { index });
            }
            if !is_http_field_value(value) {
                return Err(ResponseWireError::InvalidHeaderValue { index });
            }
            if name.eq_ignore_ascii_case("Content-Length") {
                let value = trim_http_ows(value);
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(ResponseWireError::InvalidContentLength { index });
                }
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| ResponseWireError::InvalidContentLength { index })?;
                if content_length.replace(parsed).is_some() {
                    return Err(ResponseWireError::DuplicateContentLength);
                }
            } else if name.eq_ignore_ascii_case("Transfer-Encoding") {
                transfer_encoding = true;
            }
        }
        if transfer_encoding && content_length.is_some() {
            return Err(ResponseWireError::TransferEncodingWithContentLength);
        }
        if transfer_encoding {
            return Err(ResponseWireError::UnsupportedTransferEncoding);
        }
        if !self.body.is_empty() {
            match content_length {
                Some(declared) if usize::try_from(declared) != Ok(self.body.len()) => {
                    return Err(ResponseWireError::ContentLengthMismatch {
                        declared,
                        actual: self.body.len(),
                    });
                }
                None if self.persist => {
                    return Err(ResponseWireError::PersistentBodyWithoutContentLength);
                }
                _ => {}
            }
        }

        let conn = if self.persist { "keep-alive" } else { "close" };
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nConnection: {conn}\r\nDate: {date}\r\nServer: {server}\r\nEXT:\r\n",
            self.status, self.reason
        );
        for (k, v) in &self.headers {
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
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        Ok(bytes)
    }

    /// Serialize a response, returning a fixed empty 500 response if public
    /// fields were mutated into an unsafe or unsupported wire representation.
    /// New production code should use [`Self::try_bytes_wire`] so it can log
    /// the validation error.
    pub fn bytes_wire(&self, server: &str, date: &str) -> Vec<u8> {
        self.try_bytes_wire(server, date)
            .unwrap_or_else(|_| INVALID_RESPONSE_WIRE.to_vec())
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
            "Partial Content",
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
        assert_eq!(route("GET", "/api/web/library"), HttpRoute::WebLibrary);
        assert_eq!(route("GET", "/api/web/item/3"), HttpRoute::WebItem);
        assert_eq!(route("GET", "/api/web/preview/3"), HttpRoute::WebItem);
        assert_eq!(
            route("GET", "/web/preview/3/0123456789abcdef/0.jpg"),
            HttpRoute::WebAsset
        );
        assert_eq!(
            route("GET", "/api/web/transcode/3"),
            HttpRoute::WebTranscodeStatus
        );
        assert_eq!(
            route("DELETE", "/api/web/transcode/3"),
            HttpRoute::WebTranscodeStatus
        );
        assert_eq!(
            route("POST", "/api/web/transcode/3"),
            HttpRoute::WebTranscodeStatus
        );
        assert_eq!(route("GET", "/web/app.js"), HttpRoute::WebAsset);
        assert_eq!(route("GET", "/favicon.ico"), HttpRoute::WebAsset);
        assert_eq!(route("GET", "/web/media/3.mp4"), HttpRoute::WebMedia);
        assert_eq!(route("GET", "/web/download/3"), HttpRoute::WebDownload);
    }

    #[test]
    fn telemetry_route_set_matches_stable_indices() {
        assert_eq!(HttpRoute::ALL.len(), HttpRoute::COUNT);
        for (index, route) in HttpRoute::ALL.into_iter().enumerate() {
            assert_eq!(route.index(), index);
        }
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
        assert!(!persist_for_route(
            HttpRoute::WebDownload,
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
    fn html_response_escapes_untrusted_text_and_invalid_scalars() {
        let response = HttpResponse::html(
            500,
            "Bad <reason> & \"status\"",
            "helper: <script>alert('path')</script>\u{1}",
        );
        let body = String::from_utf8(response.body).expect("HTML response is UTF-8");

        assert!(body.contains("Bad &lt;reason&gt; &amp; &quot;status&quot;"));
        assert!(
            body.contains("helper: &lt;script&gt;alert(&apos;path&apos;)&lt;/script&gt;\u{fffd}")
        );
        assert!(!body.contains("<script>"));
        assert!(!body.contains('\u{1}'));
        assert!(response.headers.iter().any(|(name, value)| {
            name == "Content-Type" && value == "text/html; charset=utf-8"
        }));
    }

    #[test]
    fn response_wire_valid_output_is_unchanged_and_legacy_matches() {
        let mut response = HttpResponse::new(200, "OK");
        response.persist = true;
        response.set("Content-Type", "text/plain");
        response.set("Server", "ignored-server");
        response.set("Date", "ignored-date");
        response.set("Connection", "ignored-connection");
        response.set("EXT", "ignored-extension");
        response.set("Content-Length", " 3\t");
        response.body = b"abc".to_vec();
        let expected = b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nDate: Thu, 01 Jan 1970 00:00:00 GMT\r\nServer: rustyDLNA-test\r\nEXT:\r\nContent-Type: text/plain\r\nContent-Length:  3\t\r\n\r\nabc";

        assert_eq!(
            response
                .try_bytes_wire("rustyDLNA-test", "Thu, 01 Jan 1970 00:00:00 GMT")
                .unwrap(),
            expected
        );
        assert_eq!(
            response.bytes_wire("rustyDLNA-test", "Thu, 01 Jan 1970 00:00:00 GMT"),
            expected
        );
    }

    #[test]
    fn response_wire_rejects_invalid_status_and_line_fields() {
        let response = HttpResponse::new(99, "OK");
        assert_eq!(
            response.try_bytes_wire("server", "date"),
            Err(ResponseWireError::InvalidStatus(99))
        );
        let response = HttpResponse::new(600, "OK");
        assert_eq!(
            response.try_bytes_wire("server", "date"),
            Err(ResponseWireError::InvalidStatus(600))
        );

        for reason in ["OK\r\nInjected: yes", "OK\0bad", "OK\u{7f}bad"] {
            let response = HttpResponse::new(200, reason);
            assert_eq!(
                response.try_bytes_wire("server", "date"),
                Err(ResponseWireError::InvalidReason)
            );
        }
        let response = HttpResponse::new(200, "OK");
        for (server, expected) in [
            ("", ResponseWireError::InvalidServer),
            (" \t", ResponseWireError::InvalidServer),
            ("server\r\nInjected: yes", ResponseWireError::InvalidServer),
            ("server\0bad", ResponseWireError::InvalidServer),
            ("server\u{7f}bad", ResponseWireError::InvalidServer),
        ] {
            assert_eq!(response.try_bytes_wire(server, "date"), Err(expected));
        }
        for (date, expected) in [
            ("", ResponseWireError::InvalidDate),
            (" \t", ResponseWireError::InvalidDate),
            ("date\r\nInjected: yes", ResponseWireError::InvalidDate),
            ("date\0bad", ResponseWireError::InvalidDate),
            ("date\u{7f}bad", ResponseWireError::InvalidDate),
        ] {
            assert_eq!(response.try_bytes_wire("server", date), Err(expected));
        }
    }

    #[test]
    fn response_wire_validates_all_headers_including_reserved_fields() {
        let mut response = HttpResponse::new(200, "OK");
        response.headers.push(("Bad Name".into(), "value".into()));
        assert_eq!(
            response.try_bytes_wire("server", "date"),
            Err(ResponseWireError::InvalidHeaderName { index: 0 })
        );

        for value in ["value\r\nInjected: yes", "value\0bad", "value\u{7f}bad"] {
            for name in ["X-Test", "Server", "Date", "Connection", "EXT"] {
                let mut response = HttpResponse::new(200, "OK");
                response.headers.push((name.into(), value.into()));
                assert_eq!(
                    response.try_bytes_wire("server", "date"),
                    Err(ResponseWireError::InvalidHeaderValue { index: 0 }),
                    "reserved header {name} accepted value {value:?}"
                );
            }
        }
    }

    #[test]
    fn response_wire_rejects_ambiguous_or_unsupported_framing() {
        for value in ["", " \t", "+1", "1, 1", "1x", "18446744073709551616"] {
            let mut response = HttpResponse::new(200, "OK");
            response.set("Content-Length", value);
            assert_eq!(
                response.try_bytes_wire("server", "date"),
                Err(ResponseWireError::InvalidContentLength { index: 0 }),
                "accepted Content-Length {value:?}"
            );
        }

        for second in ["3", "4"] {
            let mut duplicate = HttpResponse::new(200, "OK");
            duplicate.set("Content-Length", "3");
            duplicate.append("content-length", second);
            assert_eq!(
                duplicate.try_bytes_wire("server", "date"),
                Err(ResponseWireError::DuplicateContentLength)
            );
        }

        let mut conflicting = HttpResponse::new(200, "OK");
        conflicting.set("Content-Length", "3");
        conflicting.set("Transfer-Encoding", "chunked");
        assert_eq!(
            conflicting.try_bytes_wire("server", "date"),
            Err(ResponseWireError::TransferEncodingWithContentLength)
        );

        let mut transfer_coded = HttpResponse::new(200, "OK");
        transfer_coded.set("Transfer-Encoding", "chunked");
        assert_eq!(
            transfer_coded.try_bytes_wire("server", "date"),
            Err(ResponseWireError::UnsupportedTransferEncoding)
        );

        let mut mismatch = HttpResponse::new(200, "OK");
        mismatch.set("Content-Length", "2");
        mismatch.body = b"abc".to_vec();
        assert_eq!(
            mismatch.try_bytes_wire("server", "date"),
            Err(ResponseWireError::ContentLengthMismatch {
                declared: 2,
                actual: 3
            })
        );
        assert_eq!(mismatch.bytes_wire("server", "date"), INVALID_RESPONSE_WIRE);
        assert!(!mismatch
            .bytes_wire("server", "date")
            .windows(3)
            .any(|window| window == b"abc"));

        let mut unframed_persistent = HttpResponse::new(200, "OK");
        unframed_persistent.persist = true;
        unframed_persistent.body = b"abc".to_vec();
        assert_eq!(
            unframed_persistent.try_bytes_wire("server", "date"),
            Err(ResponseWireError::PersistentBodyWithoutContentLength)
        );

        let mut close_delimited = HttpResponse::new(200, "OK");
        close_delimited.body = b"abc".to_vec();
        let close_wire = close_delimited.try_bytes_wire("server", "date").unwrap();
        assert!(close_wire.starts_with(b"HTTP/1.1 200 OK\r\nConnection: close\r\n"));
        assert!(close_wire.ends_with(b"\r\n\r\nabc"));

        let mut deferred_or_head = HttpResponse::new(200, "OK");
        deferred_or_head.persist = true;
        deferred_or_head.set("Content-Length", "99");
        assert!(deferred_or_head.try_bytes_wire("server", "date").is_ok());
    }

    #[test]
    fn response_set_replaces_case_insensitive_duplicates() {
        let mut response = HttpResponse::new(200, "OK");
        response.append("X-Mode", "old");
        response.append("x-mode", "stale");
        response.set("X-Mode", "current");
        assert_eq!(response.headers, vec![("X-Mode".into(), "current".into())]);
    }

    #[test]
    fn legacy_response_wire_failure_is_a_fixed_empty_500() {
        let mut response = HttpResponse::new(200, "OK\r\nInjected: yes");
        response.set("X-Injected", "yes");
        response.body = b"secret response body".to_vec();
        assert_eq!(response.bytes_wire("server", "date"), INVALID_RESPONSE_WIRE);
    }

    #[test]
    fn media_ranges_use_the_standard_partial_content_reason() {
        let response = media_response(MediaResponseOptions {
            server: "server",
            date: "date",
            mime: "video/mp4",
            size: 10,
            range: Some(ByteRange { start: 2, end: 4 }),
            body: Vec::new(),
            pn: None,
            ci: 0,
        });
        assert_eq!(response.status, 206);
        assert_eq!(response.reason, "Partial Content");
        assert!(response
            .try_bytes_wire("server", "date")
            .unwrap()
            .starts_with(b"HTTP/1.1 206 Partial Content\r\n"));
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
