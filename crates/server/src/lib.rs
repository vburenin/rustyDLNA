//! Accept loop, SOAP Browse, original GET/`Range`, and background remux.
//! Listen ports come from `RUSTY_DLNA_HTTP_PORT` / `RUSTY_DLNA_SSDP_PORT`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rusty_dlna_http::{
    caption_info_sec_url, dlna_get_header_invalid, dlna_org_features, dlna_strict, gen_root_desc,
    interactive_on_non_image, is_chunked, live_transcode_response, media_response, now_imf_date,
    parse_byte_range, persist_for_route, protocol_info, read_file_range,
    realtime_interactive_invalid, route, scpd_connection_manager, scpd_content_directory,
    scpd_registrar, set_caption_info_sec, streaming_on_image, timeseek_without_range,
    valid_host_header, wants_caption_info_sec, wants_content_language, ByteRange, HttpRequest,
    HttpResponse, HttpRoute, MediaResponseOptions, RangeError, RemuxAudio, RemuxJobSpec,
    RootDescOpts,
};
use rusty_dlna_protocol::isolation::collides_with_live_ports;
use rusty_dlna_protocol::paths::{
    album_art_id_from_path, album_art_url, caption_default_url, caption_from_path,
    caption_indexed_url, media_item_id_from_path, media_item_url, transcode_id_from_path,
    transcode_item_url,
};
use rusty_dlna_protocol::server_header;
use rusty_dlna_protocol::w3c_normalize_date;
use rusty_dlna_protocol::{
    identify_friendly_name, identify_friendly_name_ssdp, identify_model_name, identify_request,
    identify_user_agent, remap_mime_full, ClientCache, ClientFlags, ClientKind, ClientProfile,
    CLIENTS,
};
use rusty_dlna_scan::{
    build_media_roots, caption_http_mime, load_and_persist_media_root_mappings,
    load_catalog_with_policy, load_existing, monitor, repair_objects_if_needed,
    repair_video_titles_if_needed, run_inotify_until, scan, Catalog, CatalogChild,
    Container as CatalogContainer, LibraryDb, MediaItem, MediaTypes, ScanConfig, ScanDelta,
    ScanProgress, ScanResult, SourceProbe, WatchTelemetry,
};

pub use rusty_dlna_scan::{ensure_pattern_fixture, ensure_show_fixture};
use rusty_dlna_soap::{
    apply_title_hack, bookmark_seconds, build_browse_bounded, default_order, dispatch_simple,
    empty_cd_response, extra_ci1_protocol_infos, magic_object_id, parse_filter, parse_soap_call,
    parse_update_object_tags, soap_fault, sort_or_709, try_parse_search_criteria,
    try_parse_soap_call, DefaultOrder, DidlCaption, DidlObject, DidlRes, FilterBits, SoapCall,
    SoapOutcome, UpdateObjectParseError,
};
use rusty_dlna_ssdp::{
    jitter_ms, msearch_jitter_ms_range, msearch_replies, notify_byebye, parse_inbound_notify,
    parse_msearch, ALIVE_DUP_DELAY_MS,
};
use rusty_dlna_transcode::{
    cache_dest_for_key, cache_part, decide_for_with_default_encoder, ffmpeg_grow_os_args,
    hdr10_fallback_plan, pick_audio_index_from_streams, probe_to_source, source_identity,
    transcode_cache_key, AudioAction, Decision, JobGate, RecodeAction, RemapRule, TranscodePlan,
};

mod catalog_query;
mod config;
mod events;
mod remux;
mod status;

use catalog_query::*;
#[cfg(test)]
use config::normalize_uuid;
use config::{command_version, load_or_create_uuid, validate_http_config};
pub use config::{
    load_config, resolve_http_port, resolve_ssdp_port, validate_transcode_tools, Config,
    ConfigLoadError, ConfigValidationError, TranscodeCfg,
};
#[cfg(test)]
use rusty_dlna_scan::{
    CatalogDefaultOrder, CatalogQuery, CatalogQueryClause, CatalogQueryField, CatalogQueryOp,
    CatalogQuerySort,
};
#[cfg(test)]
use rusty_dlna_soap::{SortKey, SortSpec};

/// Shared-state poison policy: log the invariant breach and recover the inner
/// value so one panicking request cannot take the daemon down. Callers still
/// validate DB/catalog generations before publishing externally visible data.
pub(crate) fn lock_recover<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        tracing::error!(
            resource = std::any::type_name::<T>(),
            "recovering poisoned mutex"
        );
        poisoned.into_inner()
    })
}

fn read_recover<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| {
        tracing::error!(
            resource = std::any::type_name::<T>(),
            "recovering poisoned read lock"
        );
        poisoned.into_inner()
    })
}

fn write_recover<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| {
        tracing::error!(
            resource = std::any::type_name::<T>(),
            "recovering poisoned write lock"
        );
        poisoned.into_inner()
    })
}

pub struct App {
    pub cfg: Config,
    pub catalog: RwLock<Catalog>,
    pub remaps: Vec<RemapRule>,
    pub scan_cfg: ScanConfig,
    pub server: String,
    pub uuid: String,
    pub http_port: u16,
    pub ssdp_port: u16,
    pub advertise_ip: String,
    pub listen_ip: std::net::Ipv4Addr,
    /// Selected SSDP egress addresses and their subnet masks.
    pub ssdp_interfaces: Vec<(Ipv4Addr, Ipv4Addr)>,
    pub notify_interval: u32,
    pub cache_dir: PathBuf,
    pub update_id: AtomicU32,
    pub jobs: JobGate,
    pub(crate) remuxes: Mutex<HashMap<String, Arc<remux::RemuxJob>>>,
    pub(crate) remux_metrics: remux::RemuxMetrics,
    events: Arc<Mutex<events::EventHub>>,
    notify_dispatcher: events::NotifyDispatcher,
    derived_image_locks: Vec<Mutex<()>>,
    pub(crate) client_cache: Mutex<ClientCache>,
    scan_control: Arc<ScanControl>,
    scan_telemetry: Arc<WatchTelemetry>,
    db_pool: Option<DbPool>,
    required_tools_ready: bool,
    #[cfg(test)]
    test_tree: Option<TestTree>,
}

/// Failure while validating configuration and constructing the runtime state.
///
/// Variants retain the affected path or value and preserve concrete source
/// errors where one exists, so startup failures are actionable without parsing
/// an unstructured string.
#[derive(Debug, thiserror::Error)]
pub enum AppInitError {
    #[error(transparent)]
    Config(#[from] ConfigValidationError),
    #[error("HTTP and SSDP ports must be non-zero")]
    ZeroPort,
    #[error("invalid media-root configuration: {message}")]
    MediaRoots { message: String },
    #[error("cannot update media-root identity mappings in {path}: {message}")]
    MediaRootMappings { path: PathBuf, message: String },
    #[error("cannot load or create server identity in {path}: {message}")]
    Identity { path: PathBuf, message: String },
    #[error("cannot maintain transcode cache {path}: {source}")]
    TranscodeCache {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("listen_ip/advertise_ip is not a valid IPv4 address: {value}")]
    ListenAddress { value: String },
    #[error("cannot select advertisement network: {message}")]
    Advertisement { message: String },
    #[error("cannot select SSDP interfaces: {message}")]
    SsdpInterfaces { message: String },
    #[error("cannot open {role} database connection {path}: {source}")]
    DatabaseOpen {
        role: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("cannot read database update ID from {path}: {source}")]
    DatabaseUpdateId {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("cannot create bounded GENA notification workers: {source}")]
    NotificationWorkers {
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
struct TestTree(PathBuf);

#[cfg(test)]
impl TestTree {
    fn new(label: &str) -> Self {
        static SEQUENCE: AtomicU32 = AtomicU32::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusty-dlna-server-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Debug, Default)]
struct ScanRuntimeState {
    phase: String,
    started_unix: Option<u64>,
    finished_unix: Option<u64>,
    duration_ms: Option<u64>,
    last_success_unix: Option<u64>,
    last_error: Option<String>,
    next_reconcile_unix: Option<u64>,
}

#[derive(Debug, Default)]
struct ScanControl {
    stopping: AtomicBool,
    gate: Mutex<()>,
    sleep: Mutex<()>,
    wake: std::sync::Condvar,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    state: Mutex<ScanRuntimeState>,
}

struct DbPool {
    readers: Mutex<Vec<LibraryDb>>,
    reader_available: std::sync::Condvar,
    writer: Mutex<LibraryDb>,
}

impl DbPool {
    fn open(path: &Path, readers: usize) -> Result<Self, AppInitError> {
        let writer = LibraryDb::open(path).map_err(|source| AppInitError::DatabaseOpen {
            role: "writer",
            path: path.to_path_buf(),
            source,
        })?;
        let readers = (0..readers.max(1))
            .map(|_| {
                LibraryDb::open_read_only(path).map_err(|source| AppInitError::DatabaseOpen {
                    role: "read-only",
                    path: path.to_path_buf(),
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            readers: Mutex::new(readers),
            reader_available: std::sync::Condvar::new(),
            writer: Mutex::new(writer),
        })
    }

    fn read<T>(
        &self,
        query: impl FnOnce(&LibraryDb) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut available = self
            .readers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while available.is_empty() {
            available = self
                .reader_available
                .wait(available)
                .unwrap_or_else(|error| error.into_inner());
        }
        let db = loop {
            if let Some(db) = available.pop() {
                break db;
            }
            available = self
                .reader_available
                .wait(available)
                .unwrap_or_else(|error| error.into_inner());
        };
        drop(available);
        let result = query(&db);
        let mut available = self
            .readers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        available.push(db);
        drop(available);
        self.reader_available.notify_one();
        result
    }

    fn write<T>(
        &self,
        query: impl FnOnce(&LibraryDb) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let db = self
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        query(&db)
    }
}

/// MiniDLNA refuses to grow a SOAP response past roughly two MiB.  Keep the
/// candidate page finite too: `RequestedCount=0` means "as many as possible",
/// not permission to materialize an entire library before applying the byte
/// ceiling.
const MAX_SOAP_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOAP_PAGE_OBJECTS: usize = 4096;

#[derive(Clone, Copy)]
enum CatalogChildRef<'a> {
    Container(&'a CatalogContainer),
    Item(&'a MediaItem),
}

/// Kodi's Platinum UPnP stack URI-encodes `$` in `UpdateObject` IDs and
/// appends a slash. MiniDLNA normalizes that wire form before catalog lookup.
fn normalize_soap_object_id(raw: &str) -> Option<String> {
    if raw.len() > 1024 {
        return None;
    }
    let mut decoded = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let hi = *bytes.get(index + 1)?;
        let lo = *bytes.get(index + 2)?;
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        decoded.push(nibble(hi)? << 4 | nibble(lo)?);
        index += 3;
    }
    if decoded.iter().any(|byte| *byte < b' ' || *byte == 0x7f) {
        return None;
    }
    let decoded = String::from_utf8(decoded).ok()?;
    Some(decoded.strip_suffix('/').unwrap_or(&decoded).to_string())
}

impl CatalogChildRef<'_> {
    fn to_owned(self) -> CatalogChild {
        match self {
            Self::Container(value) => CatalogChild::Container(value.clone()),
            Self::Item(value) => CatalogChild::Item(Box::new(value.clone())),
        }
    }
}

impl App {
    pub fn from_config(cfg: Config, http_port: u16, ssdp_port: u16, config_dir: &Path) -> Self {
        Self::try_from_config(cfg, http_port, ssdp_port, config_dir)
            .unwrap_or_else(|error| panic!("invalid rustyDLNA configuration: {error}"))
    }

    pub fn try_from_config(
        mut cfg: Config,
        http_port: u16,
        ssdp_port: u16,
        config_dir: &Path,
    ) -> Result<Self, AppInitError> {
        validate_http_config(&cfg)?;
        if http_port == 0 || ssdp_port == 0 {
            return Err(AppInitError::ZeroPort);
        }
        let resolve_dir = |configured: Option<&String>, fallback: PathBuf| {
            configured
                .map(|value| {
                    let path = PathBuf::from(value);
                    if path.is_absolute() {
                        path
                    } else {
                        config_dir.join(path)
                    }
                })
                .unwrap_or(fallback)
        };
        let cache_dir = resolve_dir(cfg.cache_dir.as_ref(), config_dir.join("cache"));
        let db_dir = resolve_dir(cfg.db_dir.as_ref(), cache_dir.clone());
        let db_path = db_dir.join("files.db");
        let mut media_roots = build_media_roots(&cfg.media_dir, config_dir)
            .map_err(|message| AppInitError::MediaRoots { message })?;
        load_and_persist_media_root_mappings(&mut media_roots, &db_path).map_err(|message| {
            AppInitError::MediaRootMappings {
                path: db_path.clone(),
                message,
            }
        })?;
        let media_dirs = media_roots
            .iter()
            .map(|root| root.configured_path.clone())
            .collect();
        let legacy_type_union = media_roots
            .iter()
            .fold(MediaTypes::none(), |all, root| all.union(root.types));
        let scan_progress = Arc::new(ScanProgress::default());
        let scan_cfg = ScanConfig {
            media_roots,
            media_dirs,
            exclude_dirs: cfg.exclude_dir.clone(),
            exclude_files: cfg.exclude_file.clone(),
            include_hidden: cfg.include_hidden,
            album_art_names: cfg.album_art_names.clone(),
            subtitles: cfg.subtitles,
            thumbnails: cfg.thumbnails,
            thumbnail_width: cfg.thumbnail_width,
            thumbnail_quality: cfg.thumbnail_quality,
            thumbnail_filmstrip: cfg.thumbnail_filmstrip,
            image_max_pixels: cfg.derived_image_max_pixels,
            image_memory_limit_bytes: cfg.derived_image_memory_mb * 1024 * 1024,
            external_command_timeout: Duration::from_secs(cfg.scan_command_timeout_secs),
            scan_workers: cfg.scan_workers,
            recent_limit: cfg.recent_limit,
            recent_days: cfg.recent_days,
            bookmark_retention_days: cfg.bookmark_retention_days,
            // Compatibility summary only. Per-root masks are authoritative.
            types: legacy_type_union,
            db_path: Some(db_path),
            wide_links: cfg.wide_links,
            progress: Some(scan_progress),
        };
        if cfg.wide_links {
            tracing::warn!(
                "wide_links=true: symlinks below media roots may expose outside files over DLNA"
            );
        }
        let catalog = load_existing(&scan_cfg);
        let remaps = std::mem::take(&mut cfg.remap);
        let uuid = load_or_create_uuid(&cache_dir, cfg.uuid.as_deref()).map_err(|message| {
            AppInitError::Identity {
                path: cache_dir.clone(),
                message,
            }
        })?;
        remux::maintain_transcode_cache(
            &cache_dir,
            cfg.transcode.cache_max_mb.saturating_mul(1024 * 1024),
            cfg.transcode.cache_max_age_days,
            cfg.cache_min_free_mb.saturating_mul(1024 * 1024),
            &std::collections::HashSet::new(),
            true,
        )
        .map_err(|source| AppInitError::TranscodeCache {
            path: cache_dir.clone(),
            source,
        })?;
        let listen_ip: std::net::Ipv4Addr =
            match cfg.listen_ip.as_deref().or(cfg.advertise_ip.as_deref()) {
                Some(raw) => raw.parse().map_err(|_| AppInitError::ListenAddress {
                    value: raw.to_owned(),
                })?,
                None => std::net::Ipv4Addr::UNSPECIFIED,
            };
        let interfaces = active_ipv4_interfaces();
        let advertise_ip_addr = select_advertise_ip(
            cfg.advertise_ip.as_deref(),
            listen_ip,
            &cfg.network_interface,
            ssdp_port,
            &interfaces,
            default_route_interface().as_deref(),
        )
        .map_err(|message| AppInitError::Advertisement { message })?;
        let ssdp_interfaces = select_ssdp_interfaces(
            &cfg.network_interface,
            advertise_ip_addr,
            ssdp_port,
            &interfaces,
        )
        .map_err(|message| AppInitError::SsdpInterfaces { message })?;
        let advertise_ip = advertise_ip_addr.to_string();
        let notify_interval = cfg.notify_interval.unwrap_or(895);
        let max_jobs = cfg.transcode.max_jobs.max(1) as usize;
        let required_tools_ready = !cfg.transcode.enable
            || (command_version("ffmpeg").is_ok() && command_version("ffprobe").is_ok());
        let db_pool = scan_cfg
            .db_path
            .as_ref()
            .map(|path| DbPool::open(path, 4))
            .transpose()?;
        let update_id = db_pool
            .as_ref()
            .map(|pool| pool.read(LibraryDb::get_update_id))
            .transpose()
            .map_err(|source| AppInitError::DatabaseUpdateId {
                path: scan_cfg
                    .db_path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("files.db")),
                source,
            })?
            .unwrap_or(1);
        let events = Arc::new(Mutex::new(events::EventHub::new()));
        let notify_dispatcher = events::NotifyDispatcher::new(Arc::clone(&events))
            .map_err(|source| AppInitError::NotificationWorkers { source })?;
        Ok(Self {
            cfg,
            catalog: RwLock::new(catalog),
            remaps,
            scan_cfg,
            server: server_header(&os_version()),
            uuid,
            http_port,
            ssdp_port,
            advertise_ip,
            listen_ip,
            ssdp_interfaces,
            notify_interval,
            cache_dir,
            update_id: AtomicU32::new(update_id),
            jobs: JobGate::new(max_jobs),
            remuxes: Mutex::new(HashMap::new()),
            remux_metrics: remux::RemuxMetrics::default(),
            events,
            notify_dispatcher,
            derived_image_locks: (0..64).map(|_| Mutex::new(())).collect(),
            client_cache: Mutex::new(ClientCache::new()),
            scan_control: Arc::new(ScanControl::default()),
            scan_telemetry: Arc::new(WatchTelemetry::default()),
            db_pool,
            required_tools_ready,
            #[cfg(test)]
            test_tree: None,
        })
    }

    pub fn isolation_ok(&self) -> Result<(), String> {
        if collides_with_live_ports(self.http_port, self.ssdp_port) {
            return Err(format!(
                "refusing to bind live ports {}/{} from a test listener; set RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900",
                self.http_port, self.ssdp_port
            ));
        }
        Ok(())
    }

    pub fn identify(&self, req: &HttpRequest) -> &'static ClientProfile {
        self.identify_peer(
            req,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9)),
        )
    }

    pub fn identify_peer(&self, req: &HttpRequest, peer: SocketAddr) -> &'static ClientProfile {
        let specific = identify_request(
            req.user_agent(),
            req.header("X-AV-Client-Info"),
            req.header("FriendlyName"),
            None,
        );
        let ip = match peer {
            SocketAddr::V4(v) => *v.ip(),
            SocketAddr::V6(_) => Ipv4Addr::LOCALHOST,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mac = lookup_arp_mac(ip);
        let mut cache = lock_recover(&self.client_cache);
        if let Some(p) = specific {
            cache.remember(ip, p, mac, now)
        } else if let Some(p) = cache.search(ip, now, mac) {
            p
        } else {
            &CLIENTS[0]
        }
    }

    /// Tests that do not pass a peer use a unique 127.0.0.x so UA-matrix
    /// rows do not share the 25-slot IPv4 cache. Production uses `handle_from`.
    pub fn handle(&self, req: &HttpRequest) -> HttpResponse {
        static SEQ: AtomicU32 = AtomicU32::new(1);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let host = Ipv4Addr::new(127, 0, 0, ((n % 250) + 1) as u8);
        self.handle_from(req, SocketAddr::from((host, 9)))
    }

    /// Shipped request handler. The accept loop passes the real peer.
    pub fn handle_from(&self, req: &HttpRequest, peer: SocketAddr) -> HttpResponse {
        let method = req.method.as_str();
        if !(method.eq_ignore_ascii_case("GET")
            || method.eq_ignore_ascii_case("HEAD")
            || method.eq_ignore_ascii_case("POST")
            || method.eq_ignore_ascii_case("SUBSCRIBE")
            || method.eq_ignore_ascii_case("UNSUBSCRIBE"))
        {
            return HttpResponse::html(501, "Not Implemented", "unsupported method");
        }
        if let Some(r) = host_rebinding_reject(req) {
            return r;
        }
        if rusty_dlna_http::http_body_too_large(req.body.len())
            || req.body.len() > self.cfg.max_request_body_bytes
        {
            return HttpResponse::html(413, "Payload Too Large", "body too large");
        }
        if timeseek_without_range(req) {
            return HttpResponse::html(406, "Not Acceptable", "TimeSeek/PlaySpeed without Range");
        }
        if dlna_get_header_invalid(req) {
            return HttpResponse::html(400, "Bad Request", "invalid DLNA get header");
        }
        if realtime_interactive_invalid(req) {
            return HttpResponse::html(400, "Bad Request", "realTimeInfo+Interactive");
        }
        let r = route(&req.method, &req.path);
        let persist = persist_for_route(
            r,
            Some(req.version.as_str()),
            req.conn_close(),
            req.conn_keep(),
        );
        let mut resp = match r {
            HttpRoute::RootDesc => self.root_desc(req, peer),
            HttpRoute::ScpdContentDir => {
                HttpResponse::xml(200, scpd_content_directory().to_string(), persist)
            }
            HttpRoute::ScpdConnectionMgr => {
                HttpResponse::xml(200, scpd_connection_manager().to_string(), persist)
            }
            HttpRoute::ScpdRegistrar => {
                HttpResponse::xml(200, scpd_registrar().to_string(), persist)
            }
            HttpRoute::EventContentDir
            | HttpRoute::EventConnectionMgr
            | HttpRoute::EventRegistrar => self.gena(req, persist, peer),
            HttpRoute::Soap => self.soap(req, persist, peer),
            HttpRoute::MediaItem => self.media(req, false, peer),
            HttpRoute::Transcode => self.media(req, true, peer),
            HttpRoute::AlbumArt => self.album_art(req),
            HttpRoute::Caption => self.caption(req),
            HttpRoute::Icon => icon_response(&req.path),
            HttpRoute::Status | HttpRoute::Presentation => {
                let body = status::status_html(self);
                let mut r = HttpResponse::new(200, "OK");
                r.set("Content-Type", "text/html");
                r.body = body.into_bytes();
                r.set("Content-Length", r.body.len());
                r
            }
            HttpRoute::Health | HttpRoute::ApiStatus => {
                let (status_code, body) = status::status_json(self, r == HttpRoute::Health);
                let mut response = HttpResponse::new(
                    status_code,
                    if status_code == 200 {
                        "OK"
                    } else {
                        "Service Unavailable"
                    },
                );
                response.set("Content-Type", "application/json");
                response.body = body.into_bytes();
                response.set("Content-Length", response.body.len());
                response
            }
            HttpRoute::Thumbnail => self.thumbnail(req),
            HttpRoute::Resized => self.resized(req),
            HttpRoute::NotFound => HttpResponse::html(404, "Not Found", "not found"),
        };
        if r == HttpRoute::MediaItem || r == HttpRoute::Transcode || r == HttpRoute::Soap {
            resp.persist = false;
        } else {
            resp.persist = persist && !is_chunked(req);
        }
        if wants_content_language(req) {
            resp.set("Content-Language", "en");
        }
        if method.eq_ignore_ascii_case("HEAD") {
            resp.body.clear();
            // The accept loop sends these payloads after `bytes_wire`, so a
            // HEAD response must suppress them explicitly as well.
            resp.file_range = None;
            resp.remux_job = None;
        }
        if resp.status >= 400 && r != HttpRoute::Soap {
            tracing::error!(
                status = resp.status,
                method = %method,
                path = %req.path,
                ua = req.user_agent().unwrap_or("-"),
                "http error"
            );
        }
        resp
    }

    fn root_desc(&self, req: &HttpRequest, peer: SocketAddr) -> HttpResponse {
        tracing::debug!(
            host = req.header("Host").unwrap_or("-"),
            ua = req.user_agent().unwrap_or("-"),
            "rootDesc"
        );
        let client = self.identify_peer(req, peer);
        let opts = RootDescOpts {
            friendly_name: self.cfg.friendly_name.clone(),
            uuid: self.uuid.clone(),
            model_number: "1".into(),
            manufacturer: "Justin Maggard".into(),
            model_name: "Windows Media Connect compatible (rustyDLNA)".into(),
            model_description: "rustyDLNA on Linux".into(),
            serial: self.cfg.serial.clone().unwrap_or_else(|| "1".into()),
            presentation_url: Some(format!("http://{}:{}/", self.advertise_ip, self.http_port)),
            xbox: client.kind == ClientKind::Xbox,
            samsung_dcm10: client.flags.contains(ClientFlags::SAMSUNG_DCM10),
        };
        let xml = gen_root_desc(&opts);
        HttpResponse::xml(200, xml, true)
    }

    /// GENA subscribe/unsubscribe (`replica.md` §9). Peer IPv4 is required
    /// so CALLBACK cannot point at a third host.
    fn gena(&self, req: &HttpRequest, persist: bool, peer: SocketAddr) -> HttpResponse {
        let method = req.method.to_ascii_uppercase();
        let sid_hdr = req.header("SID").map(str::trim).filter(|s| !s.is_empty());
        let cb_hdr = req
            .header("Callback")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if method == "UNSUBSCRIBE" {
            let Some(sid) = sid_hdr else {
                return HttpResponse::html(412, "Precondition Failed", "missing SID");
            };
            let res = lock_recover(&self.events).unsubscribe(sid);
            return match res {
                Ok(()) => {
                    let mut r = HttpResponse::new(200, "OK");
                    r.set("Content-Length", "0");
                    r.persist = persist;
                    r
                }
                Err(st) => HttpResponse::html(st, "Precondition Failed", "unknown SID"),
            };
        }
        if method != "SUBSCRIBE" {
            return HttpResponse::html(404, "Not Found", "not found");
        }
        if sid_hdr.is_some() && cb_hdr.is_some() {
            return HttpResponse::html(400, "Bad Request", "SID and Callback");
        }
        if let Some(raw_cb) = cb_hdr {
            let Some(nt) = req.header("NT").map(str::trim).filter(|s| !s.is_empty()) else {
                return HttpResponse::html(400, "Bad Request", "missing NT");
            };
            if !nt.eq_ignore_ascii_case("upnp:event") {
                return HttpResponse::html(412, "Precondition Failed", "NT");
            }
            let Some(cb) = events::parse_callback(raw_cb) else {
                return HttpResponse::html(412, "Precondition Failed", "Callback");
            };
            let Some(peer_ip) = events::peer_ipv4(peer) else {
                return HttpResponse::html(412, "Precondition Failed", "peer");
            };
            if cb.ip != peer_ip {
                return HttpResponse::html(412, "Precondition Failed", "Callback host");
            }
            let Some(service) = events::service_from_path(&req.path) else {
                return HttpResponse::html(412, "Precondition Failed", "service");
            };
            let timeout = events::parse_timeout(req.header("Timeout"));
            let sid = format!("uuid:{}", uuid::Uuid::new_v4());
            let job = {
                let mut hub = lock_recover(&self.events);
                match hub.subscribe_new(sid.clone(), cb.as_url(), service, timeout) {
                    Ok(job) => job,
                    Err(st) => {
                        return HttpResponse::html(st, "Precondition Failed", "subscriber table");
                    }
                }
            };
            let update_id = self.update_id.load(Ordering::Relaxed);
            self.notify_dispatcher
                .enqueue(job, events::propertyset(service, update_id));
            let mut r = HttpResponse::new(200, "OK");
            r.set("SID", sid);
            r.set("Timeout", format!("Second-{timeout}"));
            r.set("Content-Length", "0");
            r.persist = persist;
            return r;
        }
        if let Some(sid) = sid_hdr {
            let timeout = events::parse_timeout(req.header("Timeout"));
            return match lock_recover(&self.events).renew(sid, timeout) {
                Ok(timeout) => {
                    let mut r = HttpResponse::new(200, "OK");
                    r.set("SID", sid);
                    r.set("Timeout", format!("Second-{timeout}"));
                    r.set("Content-Length", "0");
                    r.persist = persist;
                    r
                }
                Err(st) => HttpResponse::html(st, "Precondition Failed", "unknown SID"),
            };
        }
        HttpResponse::html(412, "Precondition Failed", "missing Callback or SID")
    }

    fn soap(&self, req: &HttpRequest, persist: bool, peer: SocketAddr) -> HttpResponse {
        let action = req.header("SOAPAction").unwrap_or("");
        let body = String::from_utf8_lossy(&req.body);
        let call = match try_parse_soap_call(action, &body) {
            Ok(call) => call,
            Err(error) => {
                tracing::warn!(%error, "malformed SOAP request");
                let fallback = parse_soap_call(action, "");
                return soap_fault_logged(
                    SoapOutcome::fault402(),
                    persist,
                    &fallback,
                    req.user_agent().unwrap_or("-"),
                );
            }
        };
        let client = self.identify_peer(req, peer);
        let filter_bits = parse_filter(
            call.filter.as_deref(),
            client.flags.contains(ClientFlags::SAMSUNG),
        );
        if call.method.is_none() {
            return soap_fault_logged(
                SoapOutcome::fault401(),
                persist,
                &call,
                req.user_agent().unwrap_or("-"),
            );
        }
        if call.method == Some("X_SetBookmark") {
            return self.soap_set_bookmark(&call, client, persist, req.user_agent().unwrap_or("-"));
        }
        if call.method == Some("UpdateObject") {
            return self.soap_update_object(
                &call,
                client,
                persist,
                req.user_agent().unwrap_or("-"),
            );
        }
        if let Some(out) = dispatch_simple(
            &call,
            client,
            &self.uuid,
            self.update_id.load(Ordering::Relaxed),
            self.cfg.root_container.as_deref(),
        ) {
            return soap_outcome_http(out, persist, &call, req.user_agent().unwrap_or("-"));
        }
        let is_search = call.method == Some("Search");
        if !is_search {
            // Browse
            let Some(oid_raw) = call.object_id.as_deref() else {
                return soap_fault_logged(
                    SoapOutcome::fault402(),
                    persist,
                    &call,
                    req.user_agent().unwrap_or("-"),
                );
            };
            let Some(flag) = call.browse_flag.as_deref() else {
                return soap_fault_logged(
                    SoapOutcome::fault402(),
                    persist,
                    &call,
                    req.user_agent().unwrap_or("-"),
                );
            };
            if call.starting_index < 0 || call.requested_count < 0 {
                return soap_fault_logged(
                    SoapOutcome::fault402(),
                    persist,
                    &call,
                    req.user_agent().unwrap_or("-"),
                );
            }
            let oid = self.remap_object_id(oid_raw, client);
            if flag != "BrowseDirectChildren" && flag != "BrowseMetadata" {
                return soap_fault_logged(
                    SoapOutcome::fault402(),
                    persist,
                    &call,
                    req.user_agent().unwrap_or("-"),
                );
            }
            let remapped_root = flag == "BrowseDirectChildren" && oid != oid_raw;
            let ua = req.user_agent();
            let start = call.starting_index as usize;
            let take = if call.requested_count == 0 {
                MAX_SOAP_PAGE_OBJECTS
            } else {
                (call.requested_count as usize).min(MAX_SOAP_PAGE_OBJECTS)
            };
            let direct_sort = if flag == "BrowseDirectChildren" {
                match sort_or_709(call.sort_criteria.as_deref(), client) {
                    Ok(sort) => Some(sort),
                    Err(709) => {
                        return soap_fault_logged(
                            SoapOutcome::fault709(),
                            persist,
                            &call,
                            req.user_agent().unwrap_or("-"),
                        );
                    }
                    Err(_) => Some(Vec::new()),
                }
            } else {
                None
            };
            let order = default_order(client);
            let db_page = direct_sort.as_ref().and_then(|sort| {
                query_db_children(
                    self.db_pool.as_ref(),
                    self.scan_cfg.db_path.as_deref(),
                    &oid,
                    sort,
                    order,
                    start,
                    take,
                )
            });
            let (mut didl, total) = {
                let cat = read_recover(&self.catalog);
                if flag == "BrowseMetadata" {
                    match cat.metadata(&oid) {
                        Some(ch) => (
                            vec![self.to_didl_ref(
                                catalog_child_as_ref(&ch),
                                &cat,
                                client,
                                ua,
                                &filter_bits,
                            )],
                            1u32,
                        ),
                        None => {
                            return soap_fault_logged(
                                SoapOutcome::fault701(),
                                persist,
                                &call,
                                req.user_agent().unwrap_or("-"),
                            );
                        }
                    }
                } else {
                    let sort = direct_sort.as_deref().unwrap_or_default();
                    let displayed_total = cat.page_children(&oid, 0, 0).map(|(_, total)| total);
                    if let Some((objects, total)) = db_page.as_ref().and_then(|page| {
                        (Some(page.total) == displayed_total)
                            .then(|| {
                                materialize_db_page(&cat, page).map(|items| (items, page.total))
                            })
                            .flatten()
                    }) {
                        (
                            objects
                                .into_iter()
                                .map(|child| {
                                    self.to_didl_ref(child, &cat, client, ua, &filter_bits)
                                })
                                .collect(),
                            total,
                        )
                    } else if sort.is_empty() && order == DefaultOrder::FoldersFirst {
                        match cat.page_children(&oid, start, take) {
                            Some((objects, total)) => (
                                objects
                                    .iter()
                                    .map(|child| {
                                        self.to_didl_ref(
                                            catalog_child_as_ref(child),
                                            &cat,
                                            client,
                                            ua,
                                            &filter_bits,
                                        )
                                    })
                                    .collect(),
                                total,
                            ),
                            None => {
                                return soap_fault_logged(
                                    SoapOutcome::fault701(),
                                    persist,
                                    &call,
                                    req.user_agent().unwrap_or("-"),
                                );
                            }
                        }
                    } else {
                        match sorted_child_page(&cat, &oid, start, take, sort, order) {
                            Some((objects, total)) => (
                                objects
                                    .iter()
                                    .map(|child| {
                                        self.to_didl_ref(
                                            catalog_child_as_ref(child),
                                            &cat,
                                            client,
                                            ua,
                                            &filter_bits,
                                        )
                                    })
                                    .collect(),
                                total,
                            ),
                            None => {
                                return soap_fault_logged(
                                    SoapOutcome::fault701(),
                                    persist,
                                    &call,
                                    req.user_agent().unwrap_or("-"),
                                );
                            }
                        }
                    }
                }
            };
            if remapped_root {
                for object in &mut didl {
                    // rustyDLNA magic container parentid_sql = "0": children of
                    // remapped root are advertised with parentID of the requested id.
                    object.parent_id = oid_raw.to_string();
                }
            }
            let (xml, returned) = build_browse_bounded(
                false,
                &didl,
                total,
                self.update_id.load(Ordering::Relaxed),
                &filter_bits,
                MAX_SOAP_RESPONSE_BYTES,
            );
            tracing::info!(
                ua = req.user_agent().unwrap_or("-"),
                oid = oid_raw,
                flag,
                n = returned,
                candidates = didl.len(),
                total,
                "SOAP Browse"
            );
            // rustyDLNA closes SOAP; libupnp keep-alive + our parser has
            // dropped VLC's next Browse (folders then look like files).
            let mut r = HttpResponse::xml(200, xml, false);
            r.persist = false;
            return r;
        }
        if call.starting_index < 0 || call.requested_count < 0 {
            return soap_fault_logged(
                SoapOutcome::fault402(),
                persist,
                &call,
                req.user_agent().unwrap_or("-"),
            );
        }
        let Some(scope_raw) = call.object_id.as_deref().filter(|id| !id.is_empty()) else {
            return soap_fault_logged(
                SoapOutcome::fault402(),
                persist,
                &call,
                req.user_agent().unwrap_or("-"),
            );
        };
        let ua = req.user_agent();
        let sort = match sort_or_709(call.sort_criteria.as_deref(), client) {
            Ok(s) => s,
            Err(709) => {
                return soap_fault_logged(
                    SoapOutcome::fault709(),
                    persist,
                    &call,
                    req.user_agent().unwrap_or("-"),
                );
            }
            Err(_) => Vec::new(),
        };
        let clauses = match try_parse_search_criteria(call.search_criteria.as_deref()) {
            Ok(clauses) => clauses,
            Err(error) => {
                tracing::info!(%error, "invalid SearchCriteria");
                return soap_fault_logged(
                    SoapOutcome::fault708(),
                    persist,
                    &call,
                    req.user_agent().unwrap_or("-"),
                );
            }
        };
        let start = call.starting_index as usize;
        let take = if call.requested_count == 0 {
            MAX_SOAP_PAGE_OBJECTS
        } else {
            (call.requested_count as usize).min(MAX_SOAP_PAGE_OBJECTS)
        };
        let scope = self.remap_object_id(scope_raw, client);
        let order = default_order(client);
        let query = catalog_query(&clauses, &sort, order);
        let db_page = query_db_search(
            self.db_pool.as_ref(),
            self.scan_cfg.db_path.as_deref(),
            &scope,
            &query,
            start,
            take,
        );
        let (didl, total) = {
            let cat = read_recover(&self.catalog);
            if let Some(page) = db_page.as_ref() {
                if let Some(objects) = materialize_db_page(&cat, page) {
                    let didl = objects
                        .into_iter()
                        .map(|child| self.to_didl_ref(child, &cat, client, ua, &filter_bits))
                        .collect();
                    (didl, page.total)
                } else {
                    search_memory_page(
                        self,
                        &cat,
                        &scope,
                        &clauses,
                        &sort,
                        order,
                        start,
                        take,
                        client,
                        ua,
                        &filter_bits,
                    )
                }
            } else {
                search_memory_page(
                    self,
                    &cat,
                    &scope,
                    &clauses,
                    &sort,
                    order,
                    start,
                    take,
                    client,
                    ua,
                    &filter_bits,
                )
            }
        };
        let (xml, returned) = build_browse_bounded(
            true,
            &didl,
            total,
            self.update_id.load(Ordering::Relaxed),
            &filter_bits,
            MAX_SOAP_RESPONSE_BYTES,
        );
        tracing::info!(
            ua = req.user_agent().unwrap_or("-"),
            n = returned,
            candidates = didl.len(),
            total,
            "SOAP Search"
        );
        let mut r = HttpResponse::xml(200, xml, false);
        r.persist = false;
        r
    }

    fn to_didl_ref(
        &self,
        child: CatalogChildRef<'_>,
        cat: &Catalog,
        client: &ClientProfile,
        ua: Option<&str>,
        bits: &FilterBits,
    ) -> DidlObject {
        match child {
            CatalogChildRef::Container(c) => {
                let av = match c.object_id.chars().next() {
                    Some('1') => Some('M'),
                    Some('2') => Some('V'),
                    Some('3') => Some('P'),
                    _ => None,
                };
                let search_classes = if c.object_id == rusty_dlna_protocol::object_id::ROOT_ID {
                    vec![
                        "object.item.audioItem".into(),
                        "object.item.imageItem".into(),
                        "object.item.videoItem".into(),
                    ]
                } else {
                    vec![]
                };
                DidlObject {
                    id: c.object_id.clone(),
                    parent_id: c.parent_id.clone(),
                    title: c.title.clone(),
                    class: c.class.clone(),
                    date: None,
                    restricted: true,
                    searchable: Some(c.searchable),
                    child_count: Some(cat.displayed_child_count(&c.object_id)),
                    child_container_count: Some(cat.displayed_container_count(&c.object_id)),
                    is_container: true,
                    resources: vec![],
                    album_art_uri: None,
                    album_art_profile: false,
                    creator: None,
                    description: None,
                    artist: None,
                    actor: None,
                    album: None,
                    genre: None,
                    track: None,
                    season: None,
                    episode: None,
                    captions: vec![],
                    last_playback_position: None,
                    playback_count: None,
                    dcm_info: None,
                    ref_id: None,
                    search_classes,
                    av_media_class: av,
                }
            }
            CatalogChildRef::Item(it) => {
                let date = w3c_normalize_date(&it.date);
                let art_url = (it.album_art > 0).then(|| {
                    album_art_url(
                        &self.advertise_ip,
                        self.http_port,
                        it.album_art,
                        it.detail_id,
                    )
                });
                let audio = it.class.contains("audio");
                let video = it.class.contains("video");
                let captions = it
                    .captions
                    .iter()
                    .map(|c| DidlCaption {
                        ext: c.ext.clone(),
                        url: caption_indexed_url(
                            &self.advertise_ip,
                            self.http_port,
                            it.detail_id,
                            c.index,
                            &c.ext,
                        ),
                    })
                    .collect();
                let convert_ms = client.flags.contains(ClientFlags::CONVERT_MS);
                let pos = if convert_ms {
                    it.bookmark_sec.saturating_mul(1000)
                } else {
                    it.bookmark_sec
                };
                let last_playback_position = (it.bookmark_sec > 0).then_some(pos);
                let playback_count = (it.watch_count > 0).then_some(it.watch_count);
                let dcm_info = (bits.sec && it.bookmark_sec > 0)
                    .then(|| format!("CREATIONDATE=0,FOLDER={},BM={}", it.title, pos));
                let title = apply_title_hack(&it.title, &it.ext, client, !it.captions.is_empty());
                DidlObject {
                    id: it.object_id.clone(),
                    parent_id: it.parent_id.clone(),
                    title,
                    class: it.class.clone(),
                    date: Some(date),
                    restricted: true,
                    searchable: None,
                    child_count: None,
                    child_container_count: None,
                    is_container: false,
                    resources: self.item_resources(it, client, ua, bits),
                    album_art_uri: if audio { art_url } else { None },
                    album_art_profile: audio && client_wants_art_profile(client),
                    creator: it.creator.clone(),
                    description: it.comment.clone(),
                    artist: it.artist.clone(),
                    actor: if video { it.artist.clone() } else { None },
                    album: it.album.clone(),
                    genre: it.genre.clone(),
                    track: if audio { it.track } else { None },
                    season: if video { it.disc } else { None },
                    episode: if video { it.track } else { None },
                    captions,
                    last_playback_position,
                    playback_count,
                    dcm_info,
                    ref_id: it.ref_id.clone(),
                    search_classes: vec![],
                    av_media_class: None,
                }
            }
        }
    }

    fn soap_set_bookmark(
        &self,
        call: &SoapCall,
        client: &ClientProfile,
        persist: bool,
        ua: &str,
    ) -> HttpResponse {
        let Some(oid) = call.object_id.as_deref().filter(|s| !s.is_empty()) else {
            return soap_fault_logged(SoapOutcome::fault402(), persist, call, ua);
        };
        let Some(pos) = call.pos_second else {
            return soap_fault_logged(SoapOutcome::fault402(), persist, call, ua);
        };
        let Some(detail_id) = self.resolve_detail_id(oid, client) else {
            return soap_fault_logged(SoapOutcome::fault701(), persist, call, ua);
        };
        let sec = bookmark_seconds(pos, client.flags.contains(ClientFlags::CONVERT_MS));
        if let Err(error) = self.persist_bookmark(detail_id, Some(sec), None) {
            tracing::error!(
                target: "rusty_dlna",
                %error,
                detail_id,
                action = "X_SetBookmark",
                "playback bookmark persistence failed"
            );
            return soap_fault_logged(SoapOutcome::fault501(), persist, call, ua);
        }
        tracing::info!(
            target: "rusty_dlna",
            detail_id,
            position_seconds = sec,
            client = client.name,
            "playback bookmark updated"
        );
        soap_outcome_http(
            SoapOutcome::Ok(empty_cd_response("X_SetBookmark")),
            persist,
            call,
            ua,
        )
    }

    fn soap_update_object(
        &self,
        call: &SoapCall,
        client: &ClientProfile,
        persist: bool,
        ua: &str,
    ) -> HttpResponse {
        let Some(oid) = call.object_id.as_deref().filter(|s| !s.is_empty()) else {
            return soap_fault_logged(SoapOutcome::fault402(), persist, call, ua);
        };
        let (Some(current), Some(new)) = (
            call.current_tag_value.as_deref(),
            call.new_tag_value.as_deref(),
        ) else {
            return soap_fault_logged(SoapOutcome::fault402(), persist, call, ua);
        };
        let Some(detail_id) = self.resolve_detail_id(oid, client) else {
            return soap_fault_logged(SoapOutcome::fault701(), persist, call, ua);
        };
        let tags = match parse_update_object_tags(current, new) {
            Ok(tags) => tags,
            Err(UpdateObjectParseError::InvalidCurrent) => {
                return soap_fault_logged(SoapOutcome::fault702(), persist, call, ua);
            }
            Err(UpdateObjectParseError::InvalidNew) => {
                return soap_fault_logged(SoapOutcome::fault703(), persist, call, ua);
            }
            Err(UpdateObjectParseError::ReadOnlyTag) => {
                return soap_fault_logged(SoapOutcome::fault705(), persist, call, ua);
            }
            Err(UpdateObjectParseError::ParameterMismatch) => {
                return soap_fault_logged(SoapOutcome::fault706(), persist, call, ua);
            }
        };
        let convert_ms = client.flags.contains(ClientFlags::CONVERT_MS);
        // -1 / values < 30 store as 0 (clear). Do not map -1 to None —
        // that leaves BOOKMARKS.SEC unchanged.
        // CurrentTagValue is a concurrency token, so only unit conversion is
        // valid here. Applying the under-30-seconds storage normalization
        // would incorrectly make stale values such as 1 match stored zero.
        let expected_sec = tags.last_playback_position.map(|value| {
            if convert_ms {
                value.current / 1000
            } else {
                value.current
            }
        });
        let sec = tags
            .last_playback_position
            .map(|value| bookmark_seconds(value.new, convert_ms));
        let expected_watch = tags.playback_count.map(|value| value.current);
        let watch = tags.playback_count.map(|value| value.new);
        match self.persist_bookmark_if_current(detail_id, expected_sec, sec, expected_watch, watch)
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    target: "rusty_dlna",
                    detail_id,
                    position_seconds = ?expected_sec,
                    playback_count = ?expected_watch,
                    client = client.name,
                    "UpdateObject rejected stale CurrentTagValue"
                );
                return soap_fault_logged(SoapOutcome::fault702(), persist, call, ua);
            }
            Err(error) => {
                tracing::error!(
                    target: "rusty_dlna",
                    %error,
                    detail_id,
                    action = "UpdateObject",
                    "playback bookmark persistence failed"
                );
                return soap_fault_logged(SoapOutcome::fault501(), persist, call, ua);
            }
        }
        tracing::info!(
            target: "rusty_dlna",
            detail_id,
            position_seconds = ?sec,
            playback_count = ?watch,
            client = client.name,
            "playback bookmark updated"
        );
        soap_outcome_http(
            SoapOutcome::Ok(empty_cd_response("UpdateObject")),
            persist,
            call,
            ua,
        )
    }

    /// Samsung `A`/`V`/`I` and `root_container=` rewrite. Shared by Browse and Search.
    fn remap_object_id(&self, oid_raw: &str, client: &ClientProfile) -> String {
        let mut oid = magic_object_id(oid_raw, client);
        if oid == rusty_dlna_protocol::object_id::ROOT_ID {
            match self.cfg.root_container.as_deref() {
                Some("V") | Some("v") | Some("2") => {
                    oid = rusty_dlna_protocol::object_id::VIDEO_ID.to_string();
                }
                Some("A") | Some("1") => {
                    oid = rusty_dlna_protocol::object_id::MUSIC_ID.to_string();
                }
                Some("I") | Some("3") => {
                    oid = rusty_dlna_protocol::object_id::IMAGE_ID.to_string();
                }
                Some("64") => {
                    oid = rusty_dlna_protocol::object_id::BROWSEDIR_ID.to_string();
                }
                _ => {}
            }
        }
        oid
    }

    /// Catalog item by object id, `REF_ID` alias, magic container, or detail id.
    fn resolve_detail_id(&self, oid_raw: &str, client: &ClientProfile) -> Option<i64> {
        let normalized = normalize_soap_object_id(oid_raw)?;
        let oid = magic_object_id(&normalized, client);
        let cat = read_recover(&self.catalog);
        if let Some(it) = cat.items.get(&normalized).or_else(|| cat.items.get(&oid)) {
            return Some(it.detail_id);
        }
        match cat.metadata(&normalized).or_else(|| cat.metadata(&oid)) {
            Some(CatalogChild::Item(it)) => Some(it.detail_id),
            _ => None,
        }
    }

    fn persist_bookmark(
        &self,
        detail_id: i64,
        sec: Option<i64>,
        watch: Option<i64>,
    ) -> rusqlite::Result<()> {
        if sec.is_none() && watch.is_none() {
            return Ok(());
        }
        // Serialize bookmark publication with catalog replacement so the
        // catalog, persisted SystemUpdateID, and event body describe one
        // generation. Kodi caches Browse results and invalidates them from
        // ContainerUpdateIDs rather than SystemUpdateID alone.
        let mut cat = write_recover(&self.catalog);
        let update_id = self.update_id.load(Ordering::Relaxed).saturating_add(1);
        if let Some(pool) = self.db_pool.as_ref() {
            pool.write(|db| {
                let transaction = db.transaction()?;
                db.update_bookmark(detail_id, sec, watch)?;
                db.set_update_id(update_id)?;
                transaction.commit()
            })?;
        }
        let mut parent_ids = std::collections::BTreeSet::new();
        for it in cat.items.values_mut() {
            if it.detail_id == detail_id {
                parent_ids.insert(it.parent_id.clone());
                if let Some(s) = sec {
                    it.bookmark_sec = s;
                }
                if let Some(w) = watch {
                    it.watch_count = w;
                }
            }
        }
        self.update_id.store(update_id, Ordering::Relaxed);
        drop(cat);
        let parent_ids = parent_ids.into_iter().collect::<Vec<_>>();
        events::notify_content_dir_containers(
            &self.events,
            &self.notify_dispatcher,
            update_id,
            &parent_ids,
        );
        Ok(())
    }

    /// Atomically apply Kodi's `UpdateObject` values only when every supplied
    /// current value still matches the persisted state.
    fn persist_bookmark_if_current(
        &self,
        detail_id: i64,
        expected_sec: Option<i64>,
        sec: Option<i64>,
        expected_watch: Option<i64>,
        watch: Option<i64>,
    ) -> rusqlite::Result<bool> {
        let mut cat = write_recover(&self.catalog);
        let update_id = self.update_id.load(Ordering::Relaxed).saturating_add(1);
        if let Some(pool) = self.db_pool.as_ref() {
            let applied = pool.write(|db| {
                let transaction = db.transaction()?;
                let (current_sec, current_watch) = db.get_bookmark(detail_id)?.unwrap_or((0, 0));
                if expected_sec.is_some_and(|expected| expected != current_sec)
                    || expected_watch.is_some_and(|expected| expected != current_watch)
                {
                    return Ok(false);
                }
                db.update_bookmark(detail_id, sec, watch)?;
                db.set_update_id(update_id)?;
                transaction.commit()?;
                Ok(true)
            })?;
            if !applied {
                return Ok(false);
            }
        } else {
            let current = cat
                .items
                .values()
                .find(|item| item.detail_id == detail_id)
                .map(|item| (item.bookmark_sec, item.watch_count))
                .unwrap_or((0, 0));
            if expected_sec.is_some_and(|expected| expected != current.0)
                || expected_watch.is_some_and(|expected| expected != current.1)
            {
                return Ok(false);
            }
        }

        let mut parent_ids = std::collections::BTreeSet::new();
        for item in cat.items.values_mut() {
            if item.detail_id == detail_id {
                parent_ids.insert(item.parent_id.clone());
                if let Some(sec) = sec {
                    item.bookmark_sec = sec;
                }
                if let Some(watch) = watch {
                    item.watch_count = watch;
                }
            }
        }
        self.update_id.store(update_id, Ordering::Relaxed);
        drop(cat);
        let parent_ids = parent_ids.into_iter().collect::<Vec<_>>();
        events::notify_content_dir_containers(
            &self.events,
            &self.notify_dispatcher,
            update_id,
            &parent_ids,
        );
        Ok(true)
    }

    /// Browse and GET /Transcode: DETAILS row only. No ffprobe, no cache toml.
    fn browse_probe(&self, it: &MediaItem) -> SourceProbe {
        it.probe.clone()
    }

    fn item_resources(
        &self,
        it: &MediaItem,
        client: &ClientProfile,
        ua: Option<&str>,
        bits: &FilterBits,
    ) -> Vec<DidlRes> {
        let mime = remap_mime_full(
            client,
            &it.mime,
            it.creator.as_deref(),
            it.dlna_pn.as_deref(),
        );
        let orig_url = media_item_url(&self.advertise_ip, self.http_port, it.detail_id, &it.ext);
        let dlna = client.flags.contains(ClientFlags::DLNA);
        let skip = client.flags.contains(ClientFlags::SKIP_DLNA_PN);
        let mut bitrate = it.bitrate;
        if bitrate.is_some() && client.flags.contains(ClientFlags::MS_PFS) {
            bitrate = bitrate.map(|b| b / 8);
        }
        let orig = DidlRes {
            url: orig_url,
            protocol_info: protocol_info(&mime, it.dlna_pn.as_deref(), dlna, skip, 0),
            size: Some(it.size),
            duration: it.duration.clone(),
            bitrate,
            resolution: it.resolution.clone(),
            sample_frequency: it.samplerate,
            nr_audio_channels: it.channels,
            pv_subtitle_type: None,
            pv_subtitle_uri: None,
        };
        let probe = self.browse_probe(it);
        let src = probe_to_source(
            &probe.container,
            &probe.video,
            &probe.hdr,
            &probe.audio,
            probe.width,
            probe.height,
        );
        let plan = self.transcode_plan(client, ua, &src);
        if plan.decision == Decision::Recode {
            tracing::debug!(
                title = %it.title,
                hdr = %probe.hdr,
                rule = plan.rule.as_deref().unwrap_or("-"),
                ua = ua.unwrap_or("-"),
                "remap advertised"
            );
        }
        // Any matching remap: advertise the remux first so the client
        // (Kodi included) plays /Transcode/ instead of the original P7.
        let mut res = if plan.decision == Decision::Recode {
            let remap_url = transcode_item_url(&self.advertise_ip, self.http_port, it.detail_id);
            // Live fMP4 pipe (OP=00). Do not stat the remux cache on Browse.
            let remap = DidlRes {
                url: remap_url,
                protocol_info: format!(
                    "http-get:*:video/mp4:{}",
                    dlna_org_features(None, "00", 1, "video/mp4")
                ),
                size: None,
                duration: it.duration.clone(),
                bitrate: None,
                resolution: it.resolution.clone(),
                sample_frequency: it.samplerate,
                nr_audio_channels: it.channels,
                pv_subtitle_type: None,
                pv_subtitle_uri: None,
            };
            vec![remap, orig]
        } else {
            let mut res = vec![orig];
            if it.mime.starts_with("image/") {
                let (srcw, srch) = parse_wh(it.resolution.as_deref());
                let no_resize = client.flags.contains(ClientFlags::NO_RESIZE);
                let resize_thumbs = client.flags.contains(ClientFlags::RESIZE_THUMBS);
                if !no_resize {
                    if srcw > 4096 || srch > 4096 {
                        res.push(resized_didl(self, it, 4096, 4096, "JPEG_LRG"));
                    }
                    if srcw > 1024 || srch > 768 {
                        res.push(resized_didl(self, it, 1024, 768, "JPEG_MED"));
                    }
                    if srcw > 640 || srch > 480 {
                        res.push(resized_didl(self, it, 640, 480, "JPEG_SM"));
                    }
                }
                if resize_thumbs {
                    res.push(resized_didl(self, it, 160, 160, "JPEG_TN"));
                } else {
                    res.push(DidlRes {
                        url: format!(
                            "http://{}:{}/Thumbnails/{}.jpg",
                            self.advertise_ip, self.http_port, it.detail_id
                        ),
                        protocol_info: "http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_TN;DLNA.ORG_CI=1"
                            .into(),
                        size: None,
                        duration: None,
                        bitrate: None,
                        resolution: None,
                        sample_frequency: None,
                        nr_audio_channels: None,
                        pv_subtitle_type: None,
                        pv_subtitle_uri: None,
                    });
                }
            }
            if client.flags.contains(ClientFlags::CAPTION_RES) {
                for cap in &it.captions {
                    let url = caption_indexed_url(
                        &self.advertise_ip,
                        self.http_port,
                        it.detail_id,
                        cap.index,
                        &cap.ext,
                    );
                    res.push(DidlRes {
                        url,
                        protocol_info: format!("http-get:*:{}:*", caption_http_mime(&cap.ext)),
                        size: None,
                        duration: None,
                        bitrate: None,
                        resolution: None,
                        sample_frequency: None,
                        nr_audio_channels: None,
                        pv_subtitle_type: None,
                        pv_subtitle_uri: None,
                    });
                }
            }
            res
        };
        if bits.pv && !it.captions.is_empty() {
            if let Some(first) = res.first_mut() {
                first.pv_subtitle_type = Some("SRT".into());
                first.pv_subtitle_uri = Some(caption_default_url(
                    &self.advertise_ip,
                    self.http_port,
                    it.detail_id,
                ));
            }
        }
        if plan.decision != Decision::Recode {
            // Extra CI=1 rows inspect the stored mime/PN. Sony BDP HTTP remaps
            // mkv/mpeg → divx after this, so do not pass the remapped type here.
            for (emime, info) in
                extra_ci1_protocol_infos(client.kind, &it.mime, it.dlna_pn.as_deref())
            {
                res.push(DidlRes {
                    url: media_item_url(&self.advertise_ip, self.http_port, it.detail_id, &it.ext),
                    protocol_info: format!("http-get:*:{emime}:{info}"),
                    size: Some(it.size),
                    duration: it.duration.clone(),
                    bitrate,
                    resolution: it.resolution.clone(),
                    sample_frequency: it.samplerate,
                    nr_audio_channels: it.channels,
                    pv_subtitle_type: None,
                    pv_subtitle_uri: None,
                });
            }
        }
        self.push_video_album_art(&mut res, it, client);
        res
    }

    fn push_video_album_art(&self, res: &mut Vec<DidlRes>, it: &MediaItem, client: &ClientProfile) {
        if it.album_art <= 0 || !it.class.contains("video") {
            return;
        }
        if client.flags.contains(ClientFlags::MS_PFS) {
            return;
        }
        let url = album_art_url(
            &self.advertise_ip,
            self.http_port,
            it.album_art,
            it.detail_id,
        );
        res.push(DidlRes {
            url: url.clone(),
            protocol_info: "http-get:*:image/jpeg:DLNA.ORG_PN=JPEG_TN".into(),
            size: None,
            duration: None,
            bitrate: None,
            resolution: None,
            sample_frequency: None,
            nr_audio_channels: None,
            pv_subtitle_type: None,
            pv_subtitle_uri: None,
        });
        if client.kind == ClientKind::SamsungSeriesCde {
            res.push(DidlRes {
                url,
                protocol_info: format!(
                    "http-get:*:image/jpeg:{}",
                    dlna_org_features(Some("JPEG_SM"), "01", 1, "image/jpeg")
                ),
                size: None,
                duration: None,
                bitrate: None,
                resolution: Some("320x320".into()),
                sample_frequency: None,
                nr_audio_channels: None,
                pv_subtitle_type: None,
                pv_subtitle_uri: None,
            });
        }
    }

    fn media(&self, req: &HttpRequest, transcode: bool, peer: SocketAddr) -> HttpResponse {
        let id = if transcode {
            transcode_id_from_path(&req.path)
        } else {
            media_item_id_from_path(&req.path)
        };
        let Some(id) = id else {
            return HttpResponse::html(404, "Not Found", "bad id");
        };
        let Some(item) = read_recover(&self.catalog).get_item_by_detail(id).cloned() else {
            return HttpResponse::html(404, "Not Found", "no such object");
        };
        let client = self.identify_peer(req, peer);
        if !transcode
            && query_has_album_art(&req.query)
            && client.flags.contains(ClientFlags::MS_PFS)
        {
            if item.album_art > 0 {
                return self.serve_album_art(item.album_art, req);
            }
            return HttpResponse::html(404, "Not Found", "no album art");
        }
        if transcode {
            let probe = self.browse_probe(&item);
            let src = probe_to_source(
                &probe.container,
                &probe.video,
                &probe.hdr,
                &probe.audio,
                probe.width,
                probe.height,
            );
            let plan = self.transcode_plan(client, req.user_agent(), &src);
            if plan.decision != Decision::Recode {
                // Same DETAILS row as Browse. Serve original instead of 404
                // if a client still has a leftover /Transcode/ URL.
                tracing::info!(
                    title = %item.title,
                    hdr = %probe.hdr,
                    ua = req.user_agent().unwrap_or("-"),
                    "transcode GET serves original"
                );
            } else {
                let src_path =
                    rusty_dlna_scan::rebase_media_path_for_config(&item.path, &self.scan_cfg);
                if !rusty_dlna_scan::path_is_allowed_file(&src_path, &self.scan_cfg) {
                    tracing::error!(path = %src_path.display(), title = %item.title, "media missing");
                    return HttpResponse::html(404, "Not Found", "missing file");
                }
                let mut plan = plan;
                plan.audio_index =
                    pick_audio_index_from_streams(&probe.audio_streams, &probe.audio);
                let remux_p8 = plan.action == RecodeAction::RemuxP8;
                let Some(cache_key) = transcode_cache_key(&src_path, &plan, remux_p8) else {
                    return HttpResponse::html(
                        500,
                        "Internal Server Error",
                        "cannot fingerprint transcode source",
                    );
                };
                let dest =
                    cache_dest_for_key(&self.cache_dir, item.detail_id, plan.action, &cache_key);
                let part = cache_part(&dest);
                let grow_plan = if remux_p8 {
                    hdr10_fallback_plan(&plan)
                } else {
                    plan.clone()
                };
                tracing::info!(
                    title = %item.title,
                    hdr = %probe.hdr,
                    rule = plan.rule.as_deref().unwrap_or("-"),
                    remux_p8,
                    audio_index = plan.audio_index,
                    method = %req.method,
                    range = req.header("Range").unwrap_or("-"),
                    ua = req.user_agent().unwrap_or("-"),
                    dest = %dest.display(),
                    "transcode GET"
                );
                let args = ffmpeg_grow_os_args(&src_path, &part, &grow_plan);
                let job_key = format!("{}:{cache_key}:{args:?}", item.detail_id);
                let mut r = live_transcode_response("video/mp4");
                r.remux_job = Some(RemuxJobSpec {
                    detail_id: item.detail_id,
                    job_key,
                    cache_key,
                    src: src_path.clone(),
                    dest: dest.clone(),
                    args,
                    remux_p8,
                    audio_index: plan.audio_index,
                    audio: match plan.audio {
                        AudioAction::Copy => RemuxAudio::Copy,
                        AudioAction::ToAc3 => RemuxAudio::Ac3,
                        AudioAction::ToAac => RemuxAudio::Aac,
                    },
                });
                return r;
            }
        }
        let mime = remap_mime_full(
            client,
            &item.mime,
            item.creator.as_deref(),
            item.dlna_pn.as_deref(),
        );
        let skip = client.flags.contains(ClientFlags::SKIP_DLNA_PN);
        let pn = if skip { None } else { item.dlna_pn.clone() };
        let ci = 0u8;
        let path = rusty_dlna_scan::rebase_media_path_for_config(&item.path, &self.scan_cfg);
        if path.exists() && !rusty_dlna_scan::path_is_allowed_file(&path, &self.scan_cfg) {
            return HttpResponse::html(403, "Forbidden", "path escaped media dir");
        }
        let strict = dlna_strict(req);
        let samsung = client.flags.contains(ClientFlags::SAMSUNG);
        if streaming_on_image(req, &mime) {
            return HttpResponse::html(406, "Not Acceptable", "Streaming not allowed on image");
        }
        if interactive_on_non_image(req, &mime, samsung, strict) {
            return HttpResponse::html(406, "Not Acceptable", "Interactive not allowed");
        }
        let size = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "media missing");
                return HttpResponse::html(404, "Not Found", "missing file");
            }
        };
        let range = match req.header("Range") {
            None => None,
            Some(v) => match parse_byte_range(v, size) {
                Ok(r) => r,
                Err(RangeError::Invalid) => {
                    return HttpResponse::html(400, "Bad Request", "invalid range");
                }
                Err(RangeError::Unsatisfiable) => {
                    let mut r = HttpResponse::html(
                        416,
                        "Requested Range Not Satisfiable",
                        "range past EOF",
                    );
                    r.set("Content-Range", format!("bytes */{size}"));
                    return r;
                }
            },
        };
        let (start, end) = match range {
            Some(ByteRange { start, end }) => (start, end),
            None => (0, size.saturating_sub(1)),
        };
        const RAM_CAP: u64 = 8 * 1024 * 1024;
        let span = end.saturating_sub(start).saturating_add(1);
        let caption_sec = if wants_caption_info_sec(req) && !item.captions.is_empty() {
            Some(caption_info_sec_url(
                &self.advertise_ip,
                self.http_port,
                item.detail_id,
            ))
        } else {
            None
        };
        if req.method.eq_ignore_ascii_case("HEAD") {
            let mut r = media_response(MediaResponseOptions {
                server: &self.server,
                date: &now_imf_date(),
                mime: &mime,
                size,
                range,
                body: Vec::new(),
                pn: pn.as_deref(),
                ci,
            });
            if let Some(url) = caption_sec.as_deref() {
                set_caption_info_sec(&mut r, url);
            }
            return r;
        }
        if span > RAM_CAP {
            let mut r = media_response(MediaResponseOptions {
                server: &self.server,
                date: &now_imf_date(),
                mime: &mime,
                size,
                range,
                body: Vec::new(),
                pn: pn.as_deref(),
                ci,
            });
            if let Some(url) = caption_sec.as_deref() {
                set_caption_info_sec(&mut r, url);
            }
            r.file_range = Some((path, start, end));
            return r;
        }
        let body = match read_file_range(&path, start, end) {
            Ok(b) => b,
            Err(e) => return HttpResponse::html(500, "Internal Server Error", &e.to_string()),
        };
        let mut r = media_response(MediaResponseOptions {
            server: &self.server,
            date: &now_imf_date(),
            mime: &mime,
            size,
            range,
            body,
            pn: pn.as_deref(),
            ci,
        });
        if let Some(url) = caption_sec.as_deref() {
            set_caption_info_sec(&mut r, url);
        }
        r
    }

    fn transcode_plan(
        &self,
        client: &ClientProfile,
        ua: Option<&str>,
        src: &rusty_dlna_transcode::SourceMedia,
    ) -> TranscodePlan {
        if !self.cfg.transcode.enable {
            return TranscodePlan::default();
        }
        decide_for_with_default_encoder(
            client,
            ua,
            src,
            &self.remaps,
            Some(&self.cfg.transcode.encoder),
        )
    }

    fn album_art(&self, req: &HttpRequest) -> HttpResponse {
        let Some(id) = album_art_id_from_path(&req.path) else {
            return HttpResponse::html(404, "Not Found", "bad art id");
        };
        self.serve_album_art(id, req)
    }

    fn serve_album_art(&self, art_id: i64, req: &HttpRequest) -> HttpResponse {
        if req.header("Range").is_some() {
            return HttpResponse::html(406, "Not Acceptable", "Range not allowed on image");
        }
        if req
            .header("transferMode.dlna.org")
            .is_some_and(|v| v.eq_ignore_ascii_case("Streaming"))
        {
            return HttpResponse::html(406, "Not Acceptable", "Streaming not allowed on image");
        }
        let path = {
            let cat = read_recover(&self.catalog);
            cat.album_art_paths.get(&art_id).cloned()
        };
        let Some(path) = path else {
            return HttpResponse::html(404, "Not Found", "no such art");
        };
        let path = rusty_dlna_scan::rebase_media_path_for_config(&path, &self.scan_cfg);
        let body = match self.read_sidecar(&path) {
            Ok(b) => b,
            Err(r) => return *r,
        };
        let mut r = HttpResponse::new(200, "OK");
        r.set("Content-Type", "image/jpeg");
        r.set("transferMode.dlna.org", "Interactive");
        r.set("contentFeatures.dlna.org", "DLNA.ORG_PN=JPEG_TN");
        r.set("Content-Length", body.len());
        r.body = body;
        r
    }

    fn thumbnail(&self, req: &HttpRequest) -> HttpResponse {
        let rest = req
            .path
            .strip_prefix(rusty_dlna_protocol::paths::THUMBNAILS_PREFIX)
            .unwrap_or("");
        let Some(id) = rusty_dlna_protocol::paths::strtoll_prefix(rest) else {
            return HttpResponse::html(404, "Not Found", "bad thumb id");
        };
        if req.header("Range").is_some() || streaming_on_image(req, "image/jpeg") {
            return HttpResponse::html(406, "Not Acceptable", "Streaming/Range on image");
        }
        let Some(item) = read_recover(&self.catalog).get_item_by_detail(id).cloned() else {
            return HttpResponse::html(404, "Not Found", "no item");
        };
        if item.album_art <= 0 {
            return HttpResponse::html(404, "Not Found", "no thumbnail");
        }
        self.serve_album_art(item.album_art, req)
    }

    fn resized(&self, req: &HttpRequest) -> HttpResponse {
        let rest = req
            .path
            .strip_prefix(rusty_dlna_protocol::paths::RESIZED_PREFIX)
            .unwrap_or("");
        let Some(id) = rusty_dlna_protocol::paths::strtoll_prefix(rest) else {
            return HttpResponse::html(404, "Not Found", "bad resized id");
        };
        if req.header("Range").is_some() || streaming_on_image(req, "image/jpeg") {
            return HttpResponse::html(406, "Not Acceptable", "Streaming/Range on image");
        }
        let (w, h) = parse_resize_query(&req.query);
        let Some(item) = read_recover(&self.catalog).get_item_by_detail(id).cloned() else {
            return HttpResponse::html(404, "Not Found", "no item");
        };
        let src = if item.mime.starts_with("image/") {
            rusty_dlna_scan::rebase_media_path_for_config(&item.path, &self.scan_cfg)
        } else if item.album_art > 0 {
            let p = {
                let cat = read_recover(&self.catalog);
                cat.album_art_paths.get(&item.album_art).cloned()
            };
            let Some(p) = p else {
                return HttpResponse::html(404, "Not Found", "no art");
            };
            rusty_dlna_scan::rebase_media_path_for_config(&p, &self.scan_cfg)
        } else {
            return HttpResponse::html(404, "Not Found", "nothing to resize");
        };
        if !rusty_dlna_scan::path_is_allowed_file(&src, &self.scan_cfg) {
            return HttpResponse::html(404, "Not Found", "resize source escaped");
        }
        let pixels = u64::from(w).checked_mul(u64::from(h));
        if w > self.cfg.derived_image_max_dimension
            || h > self.cfg.derived_image_max_dimension
            || matches!(pixels, Some(pixels) if pixels > self.cfg.derived_image_max_pixels)
        {
            return HttpResponse::html(400, "Bad Request", "resize exceeds configured limits");
        }
        let Some(source_pixels) = rusty_dlna_scan::probe_image_with_timeout(
            &src,
            Duration::from_secs(self.cfg.derived_image_timeout_secs),
        )
        .and_then(|image| u64::from(image.probe.width).checked_mul(u64::from(image.probe.height))) else {
            return HttpResponse::html(404, "Not Found", "resize source is not a valid image");
        };
        if source_pixels == 0 || source_pixels > self.cfg.derived_image_max_pixels {
            return HttpResponse::html(
                413,
                "Payload Too Large",
                "source image exceeds pixel limit",
            );
        }
        let Some(identity) = source_identity(&src) else {
            return HttpResponse::html(404, "Not Found", "resize source missing");
        };
        let key = derived_image_key(
            &identity,
            w,
            h,
            self.cfg.derived_image_quality,
            item.rotation.unwrap_or(0),
        );
        let derived_dir = self.cache_dir.join("derived-images");
        if let Err(error) = prune_derived_image_cache(
            &derived_dir,
            self.cfg.derived_image_cache_mb * 1024 * 1024,
            self.cfg.derived_image_cache_age_days,
            self.cfg.cache_min_free_mb * 1024 * 1024,
        ) {
            tracing::warn!(%error, "cannot maintain derived-image cache");
            return HttpResponse::html(507, "Insufficient Storage", "image cache limits");
        }
        let dest = derived_dir.join(format!("{key}.jpg"));
        let stripe = derived_image_lock_index(&key, self.derived_image_locks.len());
        let _guard = self.derived_image_locks[stripe]
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !dest.is_file()
            && !rusty_dlna_scan::scale_jpeg_with_options_result(
                &src,
                &dest,
                w,
                h,
                self.cfg.derived_image_quality,
                Duration::from_secs(self.cfg.derived_image_timeout_secs),
                self.cfg.derived_image_memory_mb * 1024 * 1024,
            )
            .unwrap_or(false)
        {
            return HttpResponse::html(404, "Not Found", "resize failed");
        }
        if let Err(error) = prune_derived_image_cache(
            &derived_dir,
            self.cfg.derived_image_cache_mb * 1024 * 1024,
            self.cfg.derived_image_cache_age_days,
            self.cfg.cache_min_free_mb * 1024 * 1024,
        ) {
            tracing::warn!(%error, "derived-image cache remains above a configured limit");
            return HttpResponse::html(507, "Insufficient Storage", "image cache limits");
        }
        match std::fs::read(&dest) {
            Ok(body) => {
                if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&dest) {
                    let _ = file.set_modified(std::time::SystemTime::now());
                }
                let pn = jpeg_pn_for_size(w, h);
                let mut r = HttpResponse::new(200, "OK");
                r.set("Content-Type", "image/jpeg");
                r.set("transferMode.dlna.org", "Interactive");
                r.set(
                    "contentFeatures.dlna.org",
                    dlna_org_features(Some(pn), "01", 1, "image/jpeg"),
                );
                r.set("Content-Length", body.len());
                r.body = body;
                r
            }
            Err(_) => HttpResponse::html(404, "Not Found", "resize missing"),
        }
    }

    fn caption(&self, req: &HttpRequest) -> HttpResponse {
        let Some((id, idx)) = caption_from_path(&req.path) else {
            return HttpResponse::html(404, "Not Found", "bad caption");
        };
        let Some(item) = read_recover(&self.catalog).get_item_by_detail(id).cloned() else {
            return HttpResponse::html(404, "Not Found", "no item");
        };
        let Some(cap) = item.captions.iter().find(|c| c.index == idx) else {
            return HttpResponse::html(404, "Not Found", "no caption");
        };
        let cap_path = rusty_dlna_scan::rebase_media_path_for_config(&cap.path, &self.scan_cfg);
        match self.read_sidecar(&cap_path) {
            Ok(body) => {
                let mut r = HttpResponse::new(200, "OK");
                r.set("Content-Type", caption_http_mime(&cap.ext));
                r.set("Content-Length", body.len());
                r.body = body;
                r
            }
            Err(r) => *r,
        }
    }

    /// Art / captions: regular file under media_dir or cache_dir, size-capped.
    fn read_sidecar(&self, path: &Path) -> Result<Vec<u8>, Box<HttpResponse>> {
        let allowed_media = rusty_dlna_scan::path_is_allowed_file(path, &self.scan_cfg);
        let allowed_cache =
            rusty_dlna_scan::path_is_under_roots(path, std::slice::from_ref(&self.cache_dir));
        if !allowed_media && !allowed_cache {
            return Err(Box::new(HttpResponse::html(
                404,
                "Not Found",
                "sidecar escaped",
            )));
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                return Err(Box::new(HttpResponse::html(
                    404,
                    "Not Found",
                    "sidecar missing",
                )))
            }
        };
        if meta.len() > rusty_dlna_scan::MAX_SIDECAR_BYTES {
            return Err(Box::new(HttpResponse::html(
                413,
                "Payload Too Large",
                "sidecar too large",
            )));
        }
        match std::fs::read(path) {
            Ok(b) => Ok(b),
            Err(_) => Err(Box::new(HttpResponse::html(
                404,
                "Not Found",
                "sidecar missing",
            ))),
        }
    }
}

/// HTTP/1.1 requires a dotted-IPv4 Host on every method (SOAP / GENA included).
/// Any present Host must pass the dialect rebinding check.
fn host_rebinding_reject(req: &HttpRequest) -> Option<HttpResponse> {
    let http11 = req.version.eq_ignore_ascii_case("HTTP/1.1");
    match req.header("Host") {
        None if http11 => Some(HttpResponse::html(
            400,
            "Bad Request",
            "DNS rebinding attack suspected",
        )),
        Some(h) if !valid_host_header(h) => Some(HttpResponse::html(
            400,
            "Bad Request",
            "DNS rebinding attack suspected",
        )),
        _ => None,
    }
}

fn lookup_arp_mac(ip: Ipv4Addr) -> Option<[u8; 6]> {
    let text = std::fs::read_to_string("/proc/net/arp").ok()?;
    let want = ip.to_string();
    for line in text.lines().skip(1) {
        let mut it = line.split_whitespace();
        let addr = it.next()?;
        if addr != want {
            continue;
        }
        let _hw = it.next()?;
        let _flags = it.next()?;
        let mac = it.next()?;
        let mut out = [0u8; 6];
        let mut i = 0;
        for part in mac.split(':') {
            if i >= 6 {
                break;
            }
            out[i] = u8::from_str_radix(part, 16).ok()?;
            i += 1;
        }
        if i == 6 && out != [0; 6] {
            return Some(out);
        }
    }
    None
}

const MAX_RENDERER_DESCRIPTION_BYTES: usize = 256 * 1024;
const MAX_RENDERER_HEADER_BYTES: usize = 16 * 1024;

#[derive(Default)]
struct RendererFetchLimiter {
    recent_keys: HashMap<String, std::time::Instant>,
    sender_windows: HashMap<Ipv4Addr, std::collections::VecDeque<std::time::Instant>>,
}

impl RendererFetchLimiter {
    fn allow(&mut self, sender: Ipv4Addr, key: &str, now: std::time::Instant) -> bool {
        const KEY_TTL: Duration = Duration::from_secs(10 * 60);
        const SENDER_WINDOW: Duration = Duration::from_secs(60);
        const MAX_PER_SENDER: usize = 4;

        self.recent_keys
            .retain(|_, seen| now.saturating_duration_since(*seen) < KEY_TTL);
        if self.recent_keys.contains_key(key) {
            return false;
        }
        let sender_requests = self.sender_windows.entry(sender).or_default();
        while sender_requests
            .front()
            .is_some_and(|seen| now.saturating_duration_since(*seen) >= SENDER_WINDOW)
        {
            sender_requests.pop_front();
        }
        if sender_requests.len() >= MAX_PER_SENDER {
            return false;
        }
        sender_requests.push_back(now);
        self.recent_keys.insert(key.to_owned(), now);
        true
    }
}

#[derive(Default)]
struct SsdpReplyLimiter {
    senders: HashMap<Ipv4Addr, (std::time::Instant, usize)>,
}

impl SsdpReplyLimiter {
    fn allow(&mut self, sender: Ipv4Addr, datagrams: usize, now: std::time::Instant) -> bool {
        const WINDOW: Duration = Duration::from_secs(1);
        const MAX_DATAGRAMS: usize = 12;
        self.senders
            .retain(|_, (start, _)| now.saturating_duration_since(*start) < WINDOW * 2);
        let entry = self.senders.entry(sender).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= WINDOW {
            *entry = (now, 0);
        }
        if entry.1.saturating_add(datagrams) > MAX_DATAGRAMS {
            return false;
        }
        entry.1 += datagrams;
        true
    }
}

fn ipv4_masked(addr: Ipv4Addr, mask: Ipv4Addr) -> u32 {
    u32::from(addr) & u32::from(mask)
}

fn renderer_sender_is_on_link(sender: Ipv4Addr, interfaces: &[InterfaceV4]) -> bool {
    usable_lan_ipv4(sender)
        && interfaces.iter().any(|interface| {
            usable_lan_ipv4(interface.addr)
                && !interface.netmask.is_unspecified()
                && ipv4_masked(sender, interface.netmask)
                    == ipv4_masked(interface.addr, interface.netmask)
        })
}

fn trusted_renderer_location(
    url: &str,
    sender: SocketAddr,
    interfaces: &[InterfaceV4],
) -> Option<(Ipv4Addr, u16, String)> {
    let (host, port, path) = split_http_url(url)?;
    let Ok(ip) = host.parse::<Ipv4Addr>() else {
        return None;
    };
    let SocketAddr::V4(sender) = sender else {
        return None;
    };
    if ip != *sender.ip() || !renderer_sender_is_on_link(ip, interfaces) {
        return None;
    }
    Some((ip, port, path))
}

fn renderer_xml_body(response: &[u8]) -> Option<&[u8]> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?
        + 4;
    if header_end > MAX_RENDERER_HEADER_BYTES {
        return None;
    }
    let headers = std::str::from_utf8(&response[..header_end]).ok()?;
    let mut lines = headers.split("\r\n");
    let status = lines.next()?;
    let mut status_parts = status.split_whitespace();
    if !matches!(status_parts.next(), Some("HTTP/1.0" | "HTTP/1.1"))
        || status_parts.next() != Some("200")
    {
        return None;
    }
    let mut content_type = None;
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("Content-Type") {
            content_type = Some(value.to_ascii_lowercase());
        } else if name.eq_ignore_ascii_case("Content-Length") {
            let parsed = value.parse::<usize>().ok()?;
            if content_length.is_some_and(|previous| previous != parsed) {
                return None;
            }
            content_length = Some(parsed);
        } else if name.eq_ignore_ascii_case("Transfer-Encoding") {
            return None;
        }
    }
    let content_type = content_type?;
    let mime = content_type.split(';').next()?.trim();
    if mime != "text/xml" && mime != "application/xml" && !mime.ends_with("+xml") {
        return None;
    }
    let body = &response[header_end..];
    let body = match content_length {
        Some(length) if length <= MAX_RENDERER_DESCRIPTION_BYTES && body.len() >= length => {
            &body[..length]
        }
        Some(_) => return None,
        None if body.len() <= MAX_RENDERER_DESCRIPTION_BYTES => body,
        None => return None,
    };
    Some(body)
}

fn fetch_renderer_description(ip: Ipv4Addr, port: u16, path: &str) -> Option<String> {
    let Ok(mut sock) = std::net::TcpStream::connect_timeout(
        &SocketAddr::from((ip, port)),
        Duration::from_millis(400),
    ) else {
        return None;
    };
    let _ = sock.set_write_timeout(Some(Duration::from_millis(400)));
    let deadline = std::time::Instant::now() + Duration::from_millis(1200);
    let req = format!("GET {path} HTTP/1.0\r\nHost: {ip}\r\nConnection: close\r\n\r\n");
    use std::io::{Read, Write};
    if sock.write_all(req.as_bytes()).is_err() {
        return None;
    }
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let _ = sock.set_read_timeout(Some(remaining));
        match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&chunk[..read]);
                if response.len() > MAX_RENDERER_HEADER_BYTES + MAX_RENDERER_DESCRIPTION_BYTES {
                    return None;
                }
                if let Some(header_end) = response
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                {
                    if header_end > MAX_RENDERER_HEADER_BYTES {
                        return None;
                    }
                    let headers = std::str::from_utf8(&response[..header_end]).ok()?;
                    if let Some(length) = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("Content-Length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    }) {
                        if length > MAX_RENDERER_DESCRIPTION_BYTES {
                            return None;
                        }
                        if response.len() >= header_end + length {
                            break;
                        }
                    }
                }
            }
            Err(_) => return None,
        }
    }
    let body = renderer_xml_body(&response)?;
    std::str::from_utf8(body).ok().map(str::to_owned)
}

fn sniff_renderer_location(url: &str, server: &str, sender: SocketAddr, app: &App) {
    let Some((ip, port, path)) = trusted_renderer_location(url, sender, &active_ipv4_interfaces())
    else {
        tracing::warn!(%sender, location = url, "rejected renderer description target");
        return;
    };
    let Some(description) = fetch_renderer_description(ip, port, &path) else {
        return;
    };
    let friendly = xml_tag_loose(&description, "friendlyName");
    let model = xml_tag_loose(&description, "modelName");
    let profile = friendly
        .as_deref()
        .and_then(identify_friendly_name_ssdp)
        .or_else(|| friendly.as_deref().and_then(identify_friendly_name))
        .or_else(|| model.as_deref().and_then(identify_model_name))
        .or_else(|| identify_user_agent(server));
    let Some(profile) = profile else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = lock_recover(&app.client_cache).remember(ip, profile, None, now);
}

fn split_http_url(url: &str) -> Option<(String, u16, String)> {
    if url.bytes().any(|byte| byte < b' ' || byte == 0x7f) || url.contains(['#', '@']) {
        return None;
    }
    let rest = url.strip_prefix("http://")?;
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => (h.to_string(), p.parse().ok()?),
        _ => (auth.to_string(), 80u16),
    };
    if host.is_empty() || port == 0 || !path.starts_with('/') {
        return None;
    }
    Some((host, port, path.to_string()))
}

fn xml_tag_loose(hay: &str, tag: &str) -> Option<String> {
    rusty_dlna_soap::xml_tag_text(hay, tag)
}

fn parse_wh(res: Option<&str>) -> (u32, u32) {
    let Some(s) = res else {
        return (0, 0);
    };
    let Some((w, h)) = s.split_once('x') else {
        return (0, 0);
    };
    (w.parse().unwrap_or(0), h.parse().unwrap_or(0))
}

fn resized_didl(app: &App, it: &MediaItem, w: u32, h: u32, pn: &str) -> DidlRes {
    DidlRes {
        url: format!(
            "http://{}:{}/Resized/{}.jpg?width={w},height={h}",
            app.advertise_ip, app.http_port, it.detail_id
        ),
        protocol_info: format!(
            "http-get:*:image/jpeg:{}",
            dlna_org_features(Some(pn), "01", 1, "image/jpeg")
        ),
        size: None,
        duration: None,
        bitrate: None,
        resolution: Some(format!("{w}x{h}")),
        sample_frequency: None,
        nr_audio_channels: None,
        pv_subtitle_type: None,
        pv_subtitle_uri: None,
    }
}

fn jpeg_pn_for_size(w: u32, h: u32) -> &'static str {
    if w <= 160 && h <= 160 {
        "JPEG_TN"
    } else if w <= 640 && h <= 480 {
        "JPEG_SM"
    } else if w <= 1024 && h <= 768 {
        "JPEG_MED"
    } else {
        "JPEG_LRG"
    }
}

fn parse_resize_query(query: &str) -> (u32, u32) {
    let mut w = 160u32;
    let mut h = 160u32;
    for part in query.split([',', '&']) {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("width=") {
            w = v.parse().unwrap_or(w).clamp(1, 4096);
        } else if let Some(v) = part.strip_prefix("height=") {
            h = v.parse().unwrap_or(h).clamp(1, 4096);
        }
    }
    (w, h)
}

fn derived_image_key(
    identity: &str,
    width: u32,
    height: u32,
    quality: u8,
    rotation: i64,
) -> String {
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(b"rustydlna-derived-image-v2\0");
    hasher.update(identity.as_bytes());
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    hasher.update([quality]);
    hasher.update(rotation.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

fn derived_image_lock_index(key: &str, count: usize) -> usize {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % count.max(1)
}

#[cfg(unix)]
fn available_filesystem_bytes(path: &Path) -> std::io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in cache path"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a live NUL-terminated string and `stats` points to
    // writable storage for one `statvfs` value. The OS initializes it on 0.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call above initialized the complete structure.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(not(unix))]
fn available_filesystem_bytes(_path: &Path) -> std::io::Result<u64> {
    Ok(u64::MAX)
}

fn prune_derived_image_cache(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let now = std::time::SystemTime::now();
    let max_age = Duration::from_secs(u64::from(max_age_days).saturating_mul(86_400));
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        if path.extension().and_then(|value| value.to_str()) != Some("jpg") {
            if path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.contains(".tmp."))
            {
                let _ = std::fs::remove_file(path);
            }
            continue;
        }
        let used = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        if now.duration_since(used).unwrap_or_default() > max_age {
            let _ = std::fs::remove_file(path);
            continue;
        }
        entries.push((used, metadata.len(), path));
    }
    entries.sort_by_key(|entry| entry.0);
    let total = entries
        .iter()
        .fold(0u64, |total, entry| total.saturating_add(entry.1));
    let free = available_filesystem_bytes(directory).unwrap_or(u64::MAX);
    let reclaim_for_quota = total.saturating_sub(quota_bytes);
    let reclaim_for_space = minimum_free_bytes.saturating_sub(free);
    let mut reclaim = reclaim_for_quota.max(reclaim_for_space);
    for (_, bytes, path) in entries {
        if reclaim == 0 {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            reclaim = reclaim.saturating_sub(bytes);
        }
    }
    if reclaim > 0 {
        return Err(std::io::Error::other(
            "derived-image cache cannot satisfy quota/free-space requirement",
        ));
    }
    Ok(())
}

fn query_has_album_art(query: &str) -> bool {
    query.split('&').any(|p| {
        let p = p.strip_prefix('?').unwrap_or(p);
        p == "albumArt=true" || p.eq_ignore_ascii_case("albumArt=true")
    })
}

fn client_wants_art_profile(client: &ClientProfile) -> bool {
    client.flags.contains(ClientFlags::SAMSUNG)
        || matches!(
            client.kind,
            ClientKind::SamsungBdJ5500
                | ClientKind::SamsungSeriesCdeBdp
                | ClientKind::SamsungSeriesQ
                | ClientKind::SamsungSeriesCde
                | ClientKind::SamsungSeriesA
                | ClientKind::SamsungSeriesB
        )
}

fn soap_to_http(out: SoapOutcome, persist: bool) -> HttpResponse {
    match out {
        SoapOutcome::Ok(xml) => HttpResponse::xml(200, xml, persist),
        SoapOutcome::Fault {
            http,
            code,
            desc,
            persist: fault_persist,
        } => {
            let mut r = fault_resp(soap_fault(code, desc), fault_persist);
            r.status = http;
            if http == 500 {
                r.reason = "Internal Server Error".into();
            }
            r
        }
    }
}

fn soap_outcome_http(out: SoapOutcome, persist: bool, call: &SoapCall, ua: &str) -> HttpResponse {
    if matches!(out, SoapOutcome::Fault { .. }) {
        return soap_fault_logged(out, persist, call, ua);
    }
    soap_to_http(out, persist)
}

/// UPnP SOAP faults are HTTP 500. 701 is a client holding a stale
/// ObjectID (Infuse caches them) — not a server failure.
fn soap_fault_logged(out: SoapOutcome, persist: bool, call: &SoapCall, ua: &str) -> HttpResponse {
    if let SoapOutcome::Fault { code, desc, .. } = &out {
        if *code == 701 {
            tracing::debug!(
                ua,
                method = call.method.unwrap_or("-"),
                oid = call.object_id.as_deref().unwrap_or("-"),
                flag = call.browse_flag.as_deref().unwrap_or("-"),
                code,
                desc = *desc,
                "SOAP no such object"
            );
        } else {
            tracing::warn!(
                ua,
                method = call.method.unwrap_or("-"),
                oid = call.object_id.as_deref().unwrap_or("-"),
                flag = call.browse_flag.as_deref().unwrap_or("-"),
                code,
                desc = *desc,
                "SOAP fault"
            );
        }
    }
    soap_to_http(out, persist)
}

fn fault_resp(xml: String, persist: bool) -> HttpResponse {
    let mut r = HttpResponse::xml(500, xml, persist);
    r.reason = "Internal Server Error".into();
    r
}

const ICON_SMALL_PNG: &[u8] = include_bytes!("../../../assets/icon-48.png");
const ICON_SMALL_JPEG: &[u8] = include_bytes!("../../../assets/icon-48.jpg");
const ICON_LARGE_PNG: &[u8] = include_bytes!("../../../assets/icon-120.png");
const ICON_LARGE_JPEG: &[u8] = include_bytes!("../../../assets/icon-120.jpg");

fn icon_response(path: &str) -> HttpResponse {
    let lower = path.to_ascii_lowercase();
    let large = lower.contains("/lrg.");
    let (mime, body) = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        (
            "image/jpeg",
            if large {
                ICON_LARGE_JPEG
            } else {
                ICON_SMALL_JPEG
            },
        )
    } else {
        (
            "image/png",
            if large {
                ICON_LARGE_PNG
            } else {
                ICON_SMALL_PNG
            },
        )
    };
    let mut r = HttpResponse::new(200, "OK");
    r.set("Content-Type", mime);
    r.set("Content-Length", body.len());
    r.body = body.to_vec();
    r
}

fn os_version() -> String {
    let ver = std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|v| v.split_whitespace().nth(2).map(|s| s.to_string()))
        .unwrap_or_else(|| "Linux".into());
    if ver.starts_with("Linux/") {
        ver
    } else if ver == "Linux" {
        "Linux/1.0".into()
    } else {
        format!("Linux/{ver}")
    }
}

/// Bind HTTP + SSDP immediately. Library walk runs in the background.
/// SIGTERM / SIGINT / return send SSDP byebye twice (six types × 2).
pub async fn serve(app: Arc<App>) -> Result<(), Box<dyn std::error::Error>> {
    let http_addr = SocketAddr::from((app.listen_ip, app.http_port));
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    tracing::info!(%http_addr, "http listen");
    spawn_library_watch(app.clone())?;
    let ssdp_app = app.clone();
    let ssdp = tokio::spawn(async move {
        if let Err(e) = ssdp_loop(ssdp_app).await {
            tracing::warn!("ssdp loop: {e}");
        }
    });
    tokio::select! {
        _ = accept_loop(listener, app.clone()) => {}
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal");
        }
    }
    ssdp.abort();
    stop_library_watch(&app);
    remux::cancel_all(&app);
    remux::wait_for_shutdown(&app, Duration::from_secs(5)).await;
    send_byebye(&app).await;
    Ok(())
}

async fn accept_loop(listener: tokio::net::TcpListener, app: Arc<App>) {
    let connections = Arc::new(tokio::sync::Semaphore::new(app.cfg.max_connections));
    loop {
        let permit = match connections.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("accept: {e}");
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_conn(app, sock, peer).await {
                let msg = e.to_string();
                if msg.contains("Broken pipe") || msg.contains("Connection reset") {
                    tracing::debug!("conn: {e}");
                } else {
                    tracing::error!("conn: {e}");
                }
            }
        });
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

fn ssdp_notify_dests(app: &App) -> Vec<(Ipv4Addr, u16)> {
    let mut dests = Vec::new();
    if let Ok(sink) = std::env::var("RUSTY_DLNA_SSDP_SINK") {
        if let Some((h, p)) = sink.rsplit_once(':') {
            if let (Ok(ip), Ok(port)) = (h.parse::<Ipv4Addr>(), p.parse::<u16>()) {
                dests.push((ip, port));
            }
        }
    }
    if app.ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT {
        if let Ok(g) = rusty_dlna_protocol::ssdp::SSDP_MCAST_ADDR.parse() {
            dests.push((g, rusty_dlna_protocol::ssdp::SSDP_PORT));
        }
    } else {
        dests.push((Ipv4Addr::LOCALHOST, app.ssdp_port));
    }
    dests
}

async fn send_byebye(app: &App) {
    let pkts = notify_byebye(&app.uuid);
    let dests = ssdp_notify_dests(app);
    let interfaces = if app.ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT {
        app.ssdp_interfaces
            .iter()
            .map(|(address, _)| *address)
            .collect::<Vec<_>>()
    } else {
        vec![Ipv4Addr::UNSPECIFIED]
    };
    for interface in &interfaces {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        );
        let sock = match socket.and_then(|socket| {
            socket.bind(&socket2::SockAddr::from(SocketAddrV4::new(*interface, 0)))?;
            if !interface.is_unspecified() {
                socket.set_multicast_if_v4(interface)?;
            }
            socket.set_nonblocking(true)?;
            let standard: std::net::UdpSocket = socket.into();
            tokio::net::UdpSocket::from_std(standard)
        }) {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(%interface, %error, "byebye socket");
                continue;
            }
        };
        // Six types × 2, no LOCATION/SERVER/CACHE-CONTROL (packet builder).
        for _ in 0..2 {
            for packet in &pkts {
                for (ip, port) in &dests {
                    let _ = sock.send_to(packet.as_bytes(), (*ip, *port)).await;
                }
            }
        }
    }
    tracing::info!(
        packets = pkts.len() * 2 * interfaces.len(),
        interfaces = interfaces.len(),
        "SSDP byebye sent"
    );
}

fn persist_update_id(app: &App, id: u32) -> ScanResult<()> {
    let Some(pool) = app.db_pool.as_ref() else {
        return Ok(());
    };
    pool.write(|db| db.set_update_id(id)).map_err(Into::into)
}

pub(crate) fn apply_catalog(
    app: &App,
    next: Catalog,
    delta: ScanDelta,
    why: &'static str,
) -> ScanResult<()> {
    let items = next.items.len();
    // The catalog write lock serializes publication and update-ID allocation.
    // Persist first: if SQLite is busy/full/read-only, readers keep seeing the
    // previous catalog and its matching SystemUpdateID.
    let mut published = app
        .catalog
        .write()
        .unwrap_or_else(|error| error.into_inner());
    let id = app.update_id.load(Ordering::Relaxed).saturating_add(1);
    persist_update_id(app, id)?;
    *published = next;
    app.update_id.store(id, Ordering::Relaxed);
    drop(published);
    events::notify_content_dir(&app.events, &app.notify_dispatcher, id);
    tracing::info!(
        items,
        added = delta.added,
        removed = delta.removed,
        changed = delta.changed,
        "{why}"
    );
    Ok(())
}

fn reconcile_library(app: &App, why: &'static str) -> bool {
    match monitor(&app.scan_cfg) {
        Ok((Some(next), delta)) => {
            if let Err(error) = apply_catalog(app, next, delta, why) {
                tracing::error!(%error, "{why} publication failed; retaining published catalog");
                false
            } else {
                true
            }
        }
        Ok((None, _)) => true,
        Err(error) => {
            tracing::error!(%error, "{why} failed; retaining published catalog");
            false
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn scan_phase_started(control: &ScanControl, phase: &str) -> Instant {
    let mut state = control
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.phase = phase.to_string();
    state.started_unix = Some(unix_now());
    state.last_error = None;
    Instant::now()
}

fn scan_phase_finished(
    control: &ScanControl,
    phase: &str,
    started: Instant,
    error: Option<String>,
) {
    let mut state = control
        .state
        .lock()
        .unwrap_or_else(|failure| failure.into_inner());
    let now = unix_now();
    state.phase = phase.to_string();
    state.finished_unix = Some(now);
    state.duration_ms = Some(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
    if let Some(error) = error {
        state.last_error = Some(error);
    } else {
        state.last_success_unix = Some(now);
        state.last_error = None;
    }
}

fn spawn_library_watch(app: Arc<App>) -> std::io::Result<()> {
    let rescan_secs = app.cfg.rescan_secs;
    let inotify_app = app.clone();
    let inotify_handle = std::thread::Builder::new()
        .name("inotify".into())
        .spawn(move || {
            let app = inotify_app;
            let cfg = app.scan_cfg.clone();
            {
                let _serial = app
                    .scan_control
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let started = scan_phase_started(&app.scan_control, "initializing");
                let empty = app
                    .catalog
                    .read()
                    .map(|c| c.items.is_empty())
                    .unwrap_or(true);
                let mut failure = None;
                if empty {
                    tracing::info!("empty library; full scan from disk");
                    match scan(&cfg) {
                        Ok(next) => {
                            let items = next.items.len();
                            if let Err(error) = apply_catalog(
                                &app,
                                next,
                                ScanDelta {
                                    added: items,
                                    ..ScanDelta::default()
                                },
                                "full scan done",
                            ) {
                                tracing::error!(%error, "full scan publication failed; retaining published catalog");
                                failure = Some(error.to_string());
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "full scan failed; serving the existing read-only catalog and retrying later");
                            failure = Some(error.to_string());
                        }
                    }
                } else {
                    match repair_video_titles_if_needed(&cfg) {
                        Ok((Some(next), delta)) => {
                            if let Err(error) =
                                apply_catalog(&app, next, delta, "video title repair")
                            {
                                tracing::error!(%error, "video title repair publication failed; retaining published catalog");
                                failure = Some(error.to_string());
                            }
                        }
                        Ok((None, _)) => {}
                        Err(error) => {
                            tracing::error!(%error, "video title repair failed; retaining published catalog");
                            failure = Some(error.to_string());
                        }
                    }
                    match repair_objects_if_needed(&cfg) {
                        Ok((Some(next), delta)) => {
                            if let Err(error) =
                                apply_catalog(&app, next, delta, "library object repair")
                            {
                                tracing::error!(%error, "library object repair publication failed; retaining published catalog");
                                failure = Some(error.to_string());
                            }
                        }
                        Ok((None, _)) => {}
                        Err(error) => {
                            tracing::error!(%error, "library object repair failed; retaining published catalog");
                            failure = Some(error.to_string());
                        }
                    }
                    if !reconcile_library(&app, "library reconcile") {
                        failure = Some("initial library reconciliation failed".into());
                    }
                }
                if let Err(error) = fill_missing_av_meta(&app) {
                    tracing::error!(%error, "stream metadata fill failed; retaining published catalog");
                    failure = Some(error.to_string());
                }
                scan_phase_finished(&app.scan_control, "watching", started, failure);
            }
            let watch_app = app.clone();
            let control = app.scan_control.clone();
            let telemetry = app.scan_telemetry.clone();
            if let Err(e) = run_inotify_until(
                cfg,
                &control.stopping,
                Some(&control.gate),
                Some(&telemetry),
                move |next, delta| {
                    let started = scan_phase_started(&watch_app.scan_control, "publishing");
                    let failure = apply_catalog(
                        &watch_app,
                        next,
                        delta,
                        "inotify library update",
                    )
                    .err()
                    .map(|error| error.to_string());
                    scan_phase_finished(&watch_app.scan_control, "watching", started, failure);
                },
            ) {
                tracing::warn!("inotify: {e}");
                if !control.stopping.load(Ordering::Acquire) {
                    scan_phase_finished(
                        &control,
                        "degraded",
                        Instant::now(),
                        Some(e.to_string()),
                    );
                }
            }
        })?;
    app.scan_control
        .threads
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(inotify_handle);
    if rescan_secs > 0 {
        let periodic_app = app.clone();
        let periodic = match std::thread::Builder::new()
            .name("rescan".into())
            .spawn(move || {
                let app = periodic_app;
                loop {
                    {
                        let mut state = app
                            .scan_control
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        state.next_reconcile_unix = Some(unix_now().saturating_add(rescan_secs));
                    }
                    let sleeper = app
                        .scan_control
                        .sleep
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let _ = app
                        .scan_control
                        .wake
                        .wait_timeout(sleeper, Duration::from_secs(rescan_secs));
                    if app.scan_control.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let _serial = app
                        .scan_control
                        .gate
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let started = scan_phase_started(&app.scan_control, "periodic-reconcile");
                    let ok = reconcile_library(&app, "periodic rescan");
                    scan_phase_finished(
                        &app.scan_control,
                        "watching",
                        started,
                        (!ok).then(|| "periodic reconciliation failed".into()),
                    );
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                stop_library_watch(&app);
                return Err(error);
            }
        };
        app.scan_control
            .threads
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(periodic);
    }
    Ok(())
}

fn stop_library_watch(app: &App) {
    app.scan_control.stopping.store(true, Ordering::Release);
    app.scan_control.wake.notify_all();
    let handles = app
        .scan_control
        .threads
        .lock()
        .map(|mut handles| handles.drain(..).collect::<Vec<_>>())
        .unwrap_or_default();
    for handle in handles {
        if handle.join().is_err() {
            tracing::error!("library worker panicked during shutdown");
        }
    }
}

fn fill_missing_av_meta(app: &App) -> rusty_dlna_scan::ScanResult<()> {
    let Some(dbp) = app.scan_cfg.db_path.clone() else {
        return Ok(());
    };
    let db = LibraryDb::open(&dbp)?;
    let transaction = db.transaction()?;
    let rows = db.details_missing_stream_meta()?;
    let mut filled = 0usize;
    if !rows.is_empty() {
        tracing::info!(n = rows.len(), "filling missing stream metadata from files");
        let mut seen = std::collections::HashSet::new();
        for (id, path) in rows {
            let decoded = rusty_dlna_scan::path_from_db(&path);
            if rusty_dlna_scan::path_is_unwanted(&decoded, &app.scan_cfg) {
                continue;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            let live = rusty_dlna_scan::rebase_media_path_for_config(&decoded, &app.scan_cfg);
            if !rusty_dlna_scan::path_is_allowed_file(&live, &app.scan_cfg) {
                continue;
            }
            let Some(got) = rusty_dlna_scan::probe_media(&live) else {
                // A readable path with an unsupported or malformed stream was
                // still attempted. Record that fact so startup does not retry
                // the same unchanged file forever; a stat change resets it.
                db.clear_detail_stream(id)?;
                continue;
            };
            rusty_dlna_scan::apply_probe_to_detail(&db, id, &got)?;
            filled += 1;
            if filled / 200 * 200 == filled {
                tracing::info!(filled, "stream metadata progress");
            }
        }
        tracing::info!(filled, "stream metadata fill done");
    }
    let derived = db.backfill_derived_stream_fields()?;
    if derived > 0 {
        tracing::info!(
            n = derived,
            "backfilled DLNA_PN / mpeg4 from stored stream columns"
        );
    }
    transaction.commit()?;
    if filled > 0 || derived > 0 {
        let next = load_catalog_with_policy(&db, &app.scan_cfg)?;
        apply_catalog(
            app,
            next,
            ScanDelta {
                added: 0,
                removed: 0,
                changed: filled + derived,
            },
            "stream metadata update",
        )?;
    }
    Ok(())
}

fn bind_udp_reuse(addr: SocketAddrV4) -> std::io::Result<socket2::Socket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    sock.set_reuse_address(true)?;
    // The dialect uses SO_REUSEADDR only. SO_REUSEPORT lets the kernel hash
    // multicast M-SEARCH to Home Assistant's :1900 socket instead of us.
    sock.bind(&socket2::SockAddr::from(std::net::SocketAddr::V4(addr)))?;
    sock.set_nonblocking(true)?;
    Ok(sock)
}

fn into_std_udp(sock: socket2::Socket) -> std::net::UdpSocket {
    sock.into()
}

fn ssdp_recv_bind(app: &App) -> std::io::Result<socket2::Socket> {
    let live = app.ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    if live {
        let group = Ipv4Addr::new(239, 255, 255, 250);
        // rustyDLNA Linux receive bind: 239.255.255.250:1900 (not the unicast IP).
        // Binding 192.0.2.20:1900 never sees Kodi/VLC multicast M-SEARCH.
        match bind_udp_reuse(SocketAddrV4::new(group, app.ssdp_port)) {
            Ok(s) => return Ok(s),
            Err(e) => {
                tracing::warn!(
                    "SSDP bind {group}:{}: {e}; falling back to 0.0.0.0",
                    app.ssdp_port
                );
                return bind_udp_reuse(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, app.ssdp_port));
            }
        }
    }
    bind_udp_reuse(SocketAddrV4::new(app.listen_ip, app.ssdp_port))
}

async fn send_alive(sock: &tokio::net::UdpSocket, app: &App, interface_ip: Ipv4Addr) {
    if app.ssdp_port != rusty_dlna_protocol::ssdp::SSDP_PORT {
        return;
    }
    let dest = (
        rusty_dlna_protocol::ssdp::SSDP_MCAST_ADDR,
        rusty_dlna_protocol::ssdp::SSDP_PORT,
    );
    let pkts = rusty_dlna_ssdp::notify_alive(
        &app.uuid,
        &interface_ip.to_string(),
        app.http_port,
        app.notify_interval,
        &app.server,
    );
    for p in &pkts {
        if let Err(e) = sock.send_to(p.as_bytes(), dest).await {
            tracing::warn!("SSDP NOTIFY: {e}");
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InterfaceV4 {
    name: String,
    addr: Ipv4Addr,
    netmask: Ipv4Addr,
}

fn usable_lan_ipv4(addr: Ipv4Addr) -> bool {
    !addr.is_unspecified()
        && !addr.is_loopback()
        && !addr.is_multicast()
        && !addr.is_broadcast()
        && !addr.is_link_local()
}

fn select_ssdp_interfaces(
    configured: &[String],
    primary: Ipv4Addr,
    ssdp_port: u16,
    interfaces: &[InterfaceV4],
) -> Result<Vec<(Ipv4Addr, Ipv4Addr)>, String> {
    let live = ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    let mut selected = Vec::new();
    if configured.is_empty() {
        selected.extend(
            interfaces
                .iter()
                .filter(|interface| interface.addr == primary)
                .map(|interface| (interface.addr, interface.netmask)),
        );
    } else {
        for token in configured {
            let token = token.trim();
            let parsed = token.parse::<Ipv4Addr>().ok();
            selected.extend(
                interfaces
                    .iter()
                    .filter(|interface| {
                        parsed.is_some_and(|address| address == interface.addr)
                            || interface.name == token
                    })
                    .map(|interface| (interface.addr, interface.netmask)),
            );
        }
    }
    selected.retain(|(address, _)| usable_lan_ipv4(*address) || (!live && address.is_loopback()));
    selected.sort_unstable();
    selected.dedup();
    if selected.is_empty() {
        selected.push((primary, Ipv4Addr::new(255, 255, 255, 255)));
    } else if !selected.iter().any(|(address, _)| *address == primary) {
        let mask = interfaces
            .iter()
            .find(|interface| interface.addr == primary)
            .map(|interface| interface.netmask)
            .unwrap_or(Ipv4Addr::new(255, 255, 255, 255));
        selected.push((primary, mask));
        selected.sort_unstable();
        selected.dedup();
    }
    Ok(selected)
}

#[cfg(unix)]
fn active_ipv4_interfaces() -> Vec<InterfaceV4> {
    // SAFETY: `getifaddrs` initializes a linked list on success. Each node and
    // sockaddr is inspected only while that list is live, family checks guard
    // IPv4 casts, and `freeifaddrs` is called exactly once before returning.
    unsafe {
        let mut ifa = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return Vec::new();
        }
        let mut cur = ifa;
        let mut found = Vec::new();
        while !cur.is_null() {
            let entry = &*cur;
            let cname = if entry.ifa_name.is_null() {
                ""
            } else {
                std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_str()
                    .unwrap_or("")
            };
            if entry.ifa_flags & libc::IFF_UP as u32 != 0 {
                if let Some(addr) = entry.ifa_addr.as_ref() {
                    if addr.sa_family as i32 == libc::AF_INET {
                        let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        let octets = u32::from_be(sin.sin_addr.s_addr);
                        found.push(InterfaceV4 {
                            name: cname.to_owned(),
                            addr: Ipv4Addr::from(octets),
                            netmask: entry
                                .ifa_netmask
                                .as_ref()
                                .filter(|netmask| netmask.sa_family as i32 == libc::AF_INET)
                                .map(|_| {
                                    let sin = &*(entry.ifa_netmask as *const libc::sockaddr_in);
                                    Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))
                                })
                                .unwrap_or(Ipv4Addr::UNSPECIFIED),
                        });
                    }
                }
            }
            cur = entry.ifa_next;
        }
        libc::freeifaddrs(ifa);
        found.sort_by(|left, right| left.name.cmp(&right.name).then(left.addr.cmp(&right.addr)));
        found.dedup();
        found
    }
}

#[cfg(not(unix))]
fn active_ipv4_interfaces() -> Vec<InterfaceV4> {
    Vec::new()
}

fn default_route_interface() -> Option<String> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    routes
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            if columns.len() < 7 || columns[1] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(columns[3], 16).ok()?;
            if flags & libc::RTF_UP as u32 == 0 {
                return None;
            }
            let metric = columns[6].parse::<u64>().ok()?;
            Some((metric, columns[0].to_owned()))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, interface)| interface)
}

fn route_source_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // UDP connect performs route selection without sending a packet.
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(address) if usable_lan_ipv4(*address.ip()) => Some(*address.ip()),
        _ => None,
    }
}

fn select_advertise_ip(
    configured: Option<&str>,
    listen_ip: Ipv4Addr,
    configured_interfaces: &[String],
    ssdp_port: u16,
    interfaces: &[InterfaceV4],
    default_route: Option<&str>,
) -> Result<Ipv4Addr, String> {
    let live = ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    for configured_interface in configured_interfaces {
        let configured_interface = configured_interface.trim();
        if configured_interface.is_empty() {
            return Err("network_interface entries must not be empty".into());
        }
        let found = configured_interface
            .parse::<Ipv4Addr>()
            .ok()
            .is_some_and(|addr| interfaces.iter().any(|interface| interface.addr == addr))
            || interfaces
                .iter()
                .any(|interface| interface.name == configured_interface);
        if !found {
            return Err(format!(
                "network_interface {configured_interface:?} has no enabled IPv4 address"
            ));
        }
    }

    if let Some(configured) = configured {
        let addr = configured
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("advertise_ip is not a valid IPv4 address: {configured}"))?;
        if addr.is_unspecified() || addr.is_multicast() || addr.is_broadcast() {
            return Err(format!(
                "advertise_ip {addr} is not a usable unicast address"
            ));
        }
        if live && addr.is_loopback() {
            return Err(format!(
                "advertise_ip {addr} is loopback and cannot be announced on live SSDP port 1900"
            ));
        }
        if live && !interfaces.iter().any(|interface| interface.addr == addr) {
            return Err(format!(
                "advertise_ip {addr} is not assigned to an enabled local interface"
            ));
        }
        return Ok(addr);
    }

    if !listen_ip.is_unspecified() {
        if live && !usable_lan_ipv4(listen_ip) {
            return Err(format!(
                "listen_ip {listen_ip} is not usable as a live SSDP advertisement address"
            ));
        }
        if live
            && !interfaces
                .iter()
                .any(|interface| interface.addr == listen_ip)
        {
            return Err(format!(
                "listen_ip {listen_ip} is not assigned to an enabled local interface"
            ));
        }
        return Ok(listen_ip);
    }

    let mut candidates = if configured_interfaces.is_empty() {
        Vec::new()
    } else {
        configured_interfaces
            .iter()
            .flat_map(|configured_interface| {
                let configured_interface = configured_interface.trim();
                interfaces.iter().filter_map(move |interface| {
                    let matches = configured_interface
                        .parse::<Ipv4Addr>()
                        .is_ok_and(|addr| interface.addr == addr)
                        || interface.name == configured_interface;
                    matches.then_some(interface.addr)
                })
            })
            .collect::<Vec<_>>()
    };
    candidates.retain(|addr| usable_lan_ipv4(*addr));
    candidates.sort_unstable();
    candidates.dedup();
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }
    if candidates.len() > 1 {
        if let Some(source) = route_source_ipv4().filter(|source| candidates.contains(source)) {
            return Ok(source);
        }
        // All candidates remain active SSDP endpoints. This value is only the
        // primary URL for HTTP responses that have no ingress-interface hint.
        return Ok(candidates[0]);
    }

    if let Some(default_route) = default_route {
        let mut route_candidates = interfaces
            .iter()
            .filter(|interface| interface.name == default_route)
            .map(|interface| interface.addr)
            .filter(|addr| usable_lan_ipv4(*addr))
            .collect::<Vec<_>>();
        route_candidates.sort_unstable();
        route_candidates.dedup();
        if let Some(source) = route_source_ipv4().filter(|source| route_candidates.contains(source))
        {
            return Ok(source);
        }
        if route_candidates.len() == 1 {
            return Ok(route_candidates[0]);
        }
    }

    let mut all = interfaces
        .iter()
        .map(|interface| interface.addr)
        .filter(|addr| usable_lan_ipv4(*addr))
        .collect::<Vec<_>>();
    all.sort_unstable();
    all.dedup();
    match all.as_slice() {
        [only] => Ok(*only),
        [] if !live => Ok(Ipv4Addr::LOCALHOST),
        [] => Err(
            "no usable LAN IPv4 address found for SSDP; set network_interface or advertise_ip"
                .into(),
        ),
        _ => Err(format!(
            "multiple LAN IPv4 addresses found {all:?}; set network_interface or advertise_ip"
        )),
    }
}

fn ssdp_iface(app: &App) -> Ipv4Addr {
    app.advertise_ip
        .parse()
        .ok()
        .filter(|a: &Ipv4Addr| !a.is_unspecified() && !a.is_loopback())
        .or_else(|| {
            if app.listen_ip.is_unspecified() {
                None
            } else {
                Some(app.listen_ip)
            }
        })
        .unwrap_or(Ipv4Addr::UNSPECIFIED)
}

fn reply_interface_for_sender(
    sender: Ipv4Addr,
    interfaces: &[(Ipv4Addr, Ipv4Addr)],
    primary: Ipv4Addr,
) -> Ipv4Addr {
    let sender = u32::from(sender);
    interfaces
        .iter()
        .filter(|(address, mask)| {
            let mask = u32::from(*mask);
            u32::from(*address) & mask == sender & mask
        })
        .max_by_key(|(_, mask)| u32::from(*mask).count_ones())
        .map(|(address, _)| *address)
        .unwrap_or(primary)
}

async fn ssdp_loop(app: Arc<App>) -> std::io::Result<()> {
    let recv_sock = ssdp_recv_bind(&app)?;
    let live = app.ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    let iface = ssdp_iface(&app);
    if live {
        let group = Ipv4Addr::new(239, 255, 255, 250);
        for ip in app.ssdp_interfaces.iter().map(|(address, _)| *address) {
            if let Err(e) = recv_sock.join_multicast_v4(&group, &ip) {
                tracing::warn!("SSDP IP_ADD_MEMBERSHIP {ip}: {e}");
            } else {
                tracing::info!(%group, %ip, "SSDP joined multicast");
            }
        }
        let _ = recv_sock.set_multicast_ttl_v4(4);
        let _ = recv_sock.set_multicast_loop_v4(false);
    }
    let recv_std = into_std_udp(recv_sock);
    let recv_addr = recv_std.local_addr()?;
    let recv = Arc::new(tokio::net::UdpSocket::from_std(recv_std)?);

    // Kodi/Platinum drops replies whose source port is not 1900. Maintain one
    // egress socket per selected interface so source address and LOCATION
    // always describe the same reachable endpoint.
    let mut sends: Vec<(Ipv4Addr, Arc<tokio::net::UdpSocket>)> = Vec::new();
    if live {
        for (bind_ip, _) in &app.ssdp_interfaces {
            let socket = bind_udp_reuse(SocketAddrV4::new(*bind_ip, app.ssdp_port))?;
            socket.set_multicast_if_v4(bind_ip)?;
            socket.set_multicast_ttl_v4(4)?;
            socket.set_multicast_loop_v4(false)?;
            let socket = into_std_udp(socket);
            tracing::info!(interface = %bind_ip, addr = %socket.local_addr()?, "SSDP reply/notify socket");
            sends.push((*bind_ip, Arc::new(tokio::net::UdpSocket::from_std(socket)?)));
        }
    }
    tracing::info!(%recv_addr, port = app.ssdp_port, "ssdp listen");

    if !sends.is_empty() {
        for (ip, socket) in &sends {
            send_alive(socket, &app, *ip).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(jitter_ms(
            ALIVE_DUP_DELAY_MS,
        )))
        .await;
        for (ip, socket) in &sends {
            send_alive(socket, &app, *ip).await;
        }
    }

    let mut buf = vec![0u8; 2048];
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(
        app.notify_interval.max(1) as u64,
    ));
    let renderer_workers = Arc::new(tokio::sync::Semaphore::new(4));
    let reply_workers = Arc::new(tokio::sync::Semaphore::new(64));
    let mut renderer_limiter = RendererFetchLimiter::default();
    let mut reply_limiter = SsdpReplyLimiter::default();
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !sends.is_empty() {
                    for (ip, socket) in &sends {
                        send_alive(socket, &app, *ip).await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(jitter_ms(ALIVE_DUP_DELAY_MS))).await;
                    for (ip, socket) in &sends {
                        send_alive(socket, &app, *ip).await;
                    }
                }
            }
            rec = recv.recv_from(&mut buf) => {
                let (n, from) = match rec {
                    Ok(v) => v,
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::WouldBlock
                            && e.kind() != std::io::ErrorKind::Interrupted
                        {
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                        continue;
                    }
                };
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(n) = parse_inbound_notify(&text) {
                    let SocketAddr::V4(sender) = from else {
                        continue;
                    };
                    let loc = n.location;
                    if trusted_renderer_location(&loc, from, &active_ipv4_interfaces()).is_none() {
                        tracing::warn!(%from, location = %loc, "rejected renderer NOTIFY location");
                        continue;
                    }
                    let key = format!(
                        "{}|{}|{loc}",
                        sender.ip(),
                        n.usn.as_deref().unwrap_or("-")
                    );
                    if !renderer_limiter.allow(*sender.ip(), &key, std::time::Instant::now()) {
                        tracing::debug!(%from, "renderer fetch rate-limited/deduplicated");
                        continue;
                    }
                    let Ok(permit) = renderer_workers.clone().try_acquire_owned() else {
                        tracing::debug!(%from, "renderer worker pool full");
                        continue;
                    };
                    let app2 = Arc::clone(&app);
                    let server = n.server;
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        sniff_renderer_location(&loc, &server, from, &app2);
                    });
                    continue;
                }
                let Ok(ms) = parse_msearch(&text) else { continue };
                let SocketAddr::V4(sender) = from else {
                    continue;
                };
                let reply_ip = reply_interface_for_sender(
                    *sender.ip(),
                    &app.ssdp_interfaces,
                    iface,
                );
                let date = now_imf_date();
                let replies = msearch_replies(
                    &app.uuid,
                    &ms.st,
                    &reply_ip.to_string(),
                    app.http_port,
                    app.notify_interval,
                    &app.server,
                    &date,
                );
                if replies.is_empty() {
                    continue;
                }
                if !reply_limiter.allow(*sender.ip(), replies.len(), std::time::Instant::now()) {
                    tracing::debug!(%from, replies = replies.len(), "M-SEARCH reply rate-limited");
                    continue;
                }
                let Ok(permit) = reply_workers.clone().try_acquire_owned() else {
                    tracing::debug!(%from, "M-SEARCH scheduler full");
                    continue;
                };
                tracing::info!(%from, st = %ms.st, n = replies.len(), "SSDP M-SEARCH reply");
                let all = ms.st == rusty_dlna_protocol::ssdp::ST_ALL;
                let out = sends
                    .iter()
                    .find(|(ip, _)| *ip == reply_ip)
                    .map(|(_, socket)| Arc::clone(socket))
                    .unwrap_or_else(|| Arc::clone(&recv));
                tokio::spawn(async move {
                    let _permit = permit;
                    tokio::time::sleep(Duration::from_millis(jitter_ms(
                        msearch_jitter_ms_range(all),
                    )))
                    .await;
                    for reply in replies {
                        if let Err(error) = out.send_to(reply.as_bytes(), from).await {
                            tracing::warn!(%from, %error, "SSDP reply send failed");
                        }
                    }
                });
            }
        }
    }
}

async fn handle_conn(
    app: Arc<App>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;
    const MAX_HEADER_BYTES: usize = 64 * 1024;

    let mut persist_left = 100u32;
    let mut pending = Vec::new();
    let mut request_number = 0u32;
    let mut tmp = [0u8; 4096];
    loop {
        let header_wait = if request_number > 0 && pending.is_empty() {
            app.cfg.keep_alive_timeout_secs
        } else {
            app.cfg.header_read_timeout_secs
        };
        let header_deadline = tokio::time::Instant::now() + Duration::from_secs(header_wait);
        let header_end = loop {
            if let Some(end) = rusty_dlna_http::header_block_complete(&pending) {
                if end > MAX_HEADER_BYTES {
                    let response = HttpResponse::html(
                        431,
                        "Request Header Fields Too Large",
                        "headers too large",
                    );
                    socket_write_all(
                        &app,
                        &mut sock,
                        &response.bytes_wire(&app.server, &now_imf_date()),
                    )
                    .await?;
                    return Ok(());
                }
                break end;
            }
            if pending.len() > MAX_HEADER_BYTES {
                let response =
                    HttpResponse::html(431, "Request Header Fields Too Large", "headers too large");
                socket_write_all(
                    &app,
                    &mut sock,
                    &response.bytes_wire(&app.server, &now_imf_date()),
                )
                .await?;
                return Ok(());
            }
            let n = match tokio::time::timeout_at(header_deadline, sock.read(&mut tmp)).await {
                Ok(read) => read?,
                Err(_) => {
                    let response = HttpResponse::html(408, "Request Timeout", "header timeout");
                    let _ = socket_write_all(
                        &app,
                        &mut sock,
                        &response.bytes_wire(&app.server, &now_imf_date()),
                    )
                    .await;
                    return Ok(());
                }
            };
            if n == 0 {
                if pending.is_empty() {
                    return Ok(());
                }
                let response = HttpResponse::html(400, "Bad Request", "incomplete headers");
                socket_write_all(
                    &app,
                    &mut sock,
                    &response.bytes_wire(&app.server, &now_imf_date()),
                )
                .await?;
                return Ok(());
            }
            pending.extend_from_slice(&tmp[..n]);
        };
        let head = match std::str::from_utf8(&pending[..header_end]) {
            Ok(head) => head,
            Err(_) => {
                let response = HttpResponse::html(400, "Bad Request", "headers are not UTF-8");
                socket_write_all(
                    &app,
                    &mut sock,
                    &response.bytes_wire(&app.server, &now_imf_date()),
                )
                .await?;
                return Ok(());
            }
        };
        let mut req = match HttpRequest::parse_headers(head) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%peer, ?error, "rejected malformed HTTP request");
                let response = HttpResponse::html(400, "Bad Request", "malformed request");
                socket_write_all(
                    &app,
                    &mut sock,
                    &response.bytes_wire(&app.server, &now_imf_date()),
                )
                .await?;
                return Ok(());
            }
        };
        let need = req.content_length().unwrap_or(0);
        if rusty_dlna_http::http_body_too_large(need) || need > app.cfg.max_request_body_bytes {
            let resp = HttpResponse::html(413, "Payload Too Large", "body too large");
            socket_write_all(
                &app,
                &mut sock,
                &resp.bytes_wire(&app.server, &now_imf_date()),
            )
            .await?;
            return Ok(());
        }
        let request_end = header_end
            .checked_add(need)
            .ok_or("request length overflow")?;
        let body_deadline =
            tokio::time::Instant::now() + Duration::from_secs(app.cfg.body_read_timeout_secs);
        while pending.len() < request_end {
            let n = match tokio::time::timeout_at(body_deadline, sock.read(&mut tmp)).await {
                Ok(read) => read?,
                Err(_) => {
                    let response = HttpResponse::html(408, "Request Timeout", "body timeout");
                    let _ = socket_write_all(
                        &app,
                        &mut sock,
                        &response.bytes_wire(&app.server, &now_imf_date()),
                    )
                    .await;
                    return Ok(());
                }
            };
            if n == 0 {
                let response = HttpResponse::html(400, "Bad Request", "incomplete body");
                socket_write_all(
                    &app,
                    &mut sock,
                    &response.bytes_wire(&app.server, &now_imf_date()),
                )
                .await?;
                return Ok(());
            }
            pending.extend_from_slice(&tmp[..n]);
        }
        req.body = pending[header_end..request_end].to_vec();
        pending.drain(..request_end);
        let handler_app = app.clone();
        let handler_request = req.clone();
        let resp =
            tokio::task::spawn_blocking(move || handler_app.handle_from(&handler_request, peer))
                .await
                .map_err(|error| format!("request worker failed: {error}"))?;
        request_number = request_number.saturating_add(1);
        persist_left = persist_left.saturating_sub(1);
        if let Some(spec) = resp.remux_job.clone() {
            remux::serve_remux(&app, &mut sock, &req, spec).await?;
            break;
        }
        let wire = resp.bytes_wire(&app.server, &now_imf_date());
        socket_write_all(&app, &mut sock, &wire).await?;
        if let Some((path, start, end)) = resp.file_range.clone() {
            stream_file_range(&app, &mut sock, &path, start, end).await?;
        }
        if !resp.persist || persist_left == 0 {
            break;
        }
    }
    Ok(())
}

pub(crate) async fn socket_write_all(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    bytes: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    tokio::time::timeout(
        Duration::from_secs(app.cfg.write_timeout_secs),
        sock.write_all(bytes),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "socket write timeout"))?
}

pub(crate) async fn stream_file_range(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    path: &std::path::Path,
    start: u64,
    end: u64,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut left = end.saturating_sub(start).saturating_add(1);
    let mut buf = vec![0u8; 64 * 1024];
    while left > 0 {
        let n = std::cmp::min(left as usize, buf.len());
        let got = f.read(&mut buf[..n]).await?;
        if got == 0 {
            break;
        }
        socket_write_all(app, sock, &buf[..got]).await?;
        left -= got as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_dlna_scan::{ensure_pattern_fixture, scan};
    use rusty_dlna_soap::xml_tag_text;

    const TINY_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06,
        0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B,
        0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20,
        0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31,
        0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF,
        0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00,
        0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x08, 0xFF, 0xC4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08,
        0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x2A, 0x1F, 0xFF, 0xD9,
    ];

    fn workspace() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn testdata_app() -> App {
        let root = workspace();
        let test_tree = TestTree::new("fixtures");
        let lib = root.join("testdata/library");
        ensure_pattern_fixture(&lib);
        rusty_dlna_scan::ensure_show_fixture(&lib);
        let nfo = lib.join("video/movie.nfo");
        if !nfo.exists() {
            let _ = std::fs::create_dir_all(lib.join("video"));
            let _ = std::fs::write(&nfo, "<movie><year>1999</year></movie>\n");
            let _ = std::fs::write(
                lib.join("video/movie.srt"),
                "1\n00:00:00,000 --> 00:00:01,000\nhi\n",
            );
            let _ = std::fs::write(lib.join("video/movie.en.srt"), "en\n");
        }
        let poster = lib.join("video/movie-poster.jpg");
        if !poster.exists() {
            let _ = std::fs::write(&poster, TINY_JPEG);
        }
        let _ = std::fs::create_dir_all(lib.join("music"));
        let song = lib.join("music/song.flac");
        if !song.exists() {
            let mut flac = b"fLaC".to_vec();
            flac.extend_from_slice(&[0u8; 48]);
            let _ = std::fs::write(&song, flac);
        }
        let song_nfo = lib.join("music/song.nfo");
        if !song_nfo.exists() {
            let _ = std::fs::write(
                &song_nfo,
                "<musicvideo><title>Fixture Track</title><genre>Jazz</genre><studio>Fixture Band</studio></musicvideo>\n",
            );
        }
        let _ = std::fs::create_dir_all(lib.join("pictures"));
        let pic = lib.join("pictures/shot.jpg");
        if !pic.exists() {
            let _ = std::fs::write(&pic, TINY_JPEG);
        }
        let dvp7 = lib.join("video/dvp7.mkv");
        if !dvp7.exists() {
            let _ = std::fs::write(&dvp7, b"not-a-real-mkv");
        }
        let guessed = lib.join("video/Movie.2024.2160p.UHD.BDRemux.HDR.DV.HEVC.mkv");
        if !guessed.exists() {
            let _ = std::fs::write(&guessed, b"not-a-real-mkv");
        }
        let probe = lib.join("video/dvp7.probe.toml");
        if !probe.exists() {
            let _ = std::fs::write(
                &probe,
                "container = \"mkv\"\nvideo = \"hevc\"\nhdr = \"dv-p7\"\naudio = \"truehd\"\n",
            );
        }
        let _ = std::fs::create_dir_all(lib.join("sample"));
        let _ = std::fs::write(lib.join("sample/ignored.mkv"), b"x");
        let _ = std::fs::create_dir_all(lib.join("@eaDir"));
        let _ = std::fs::write(lib.join("@eaDir/junk.mkv"), b"x");
        let _ = std::fs::create_dir_all(lib.join("exclude_me"));
        let _ = std::fs::write(lib.join("exclude_me/secret.mkv"), b"x");
        let _ = std::fs::write(lib.join("unfinished.mkv.part"), b"x");
        let cfg = Config {
            friendly_name: "rustyDLNA-test".into(),
            media_dir: vec!["testdata/library".into()],
            exclude_dir: vec!["exclude_me".into()],
            cache_dir: Some(test_tree.path().join("cache").display().to_string()),
            db_dir: Some(test_tree.path().join("database").display().to_string()),
            rescan_secs: 0,
            remap: rusty_dlna_transcode::parse_remaps_toml(
                r#"
[[remap]]
name = "crkey-dvp7"
client = "CrKey"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "to-aac"
"#,
            )
            .unwrap(),
            transcode: TranscodeCfg {
                enable: true,
                encoder: "libx264".into(),
                max_jobs: 1,
                ..TranscodeCfg::default()
            },
            ..Config::default()
        };
        let mut app = App::from_config(cfg, 18200, 11900, &root);
        let cat = scan(&app.scan_cfg).unwrap();
        *write_recover(&app.catalog) = cat;
        app.test_tree = Some(test_tree);
        app
    }

    fn req(raw: &str) -> HttpRequest {
        HttpRequest::parse_headers(raw).unwrap()
    }

    fn get(path: &str, ua: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\n\r\n")
    }

    fn resp_header<'a>(r: &'a HttpResponse, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn parse_album_art_url(didl: &str) -> Option<(i64, i64)> {
        let idx = didl.find("/AlbumArt/")?;
        let rest = &didl[idx + "/AlbumArt/".len()..];
        let art: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let art_id = art.parse().ok()?;
        let after = rest.get(art.len()..)?;
        let after = after.strip_prefix('-')?;
        let det: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let detail_id = det.parse().ok()?;
        Some((art_id, detail_id))
    }

    #[test]
    fn test_ports_do_not_collide_with_live() {
        assert!(!collides_with_live_ports(18200, 11900));
        assert!(collides_with_live_ports(8200, 11900));
        assert!(collides_with_live_ports(18200, 1900));
    }

    #[test]
    fn scan_policy_config_defaults_aliases_and_ranges_are_validated() {
        let parsed: Config = toml::from_str(
            r#"
enable_subtitles = false
enable_thumbnail = false
enable_thumbnail_filmstrip = true
thumbnail_width = 640
thumbnail_quality = 5
scan_command_timeout_secs = 9
scan_workers = 16
bookmark_retention_days = 90
album_art_names = ["AlbumArt.jpg", "{stem}-cover.png"]
"#,
        )
        .unwrap();
        assert!(!parsed.subtitles);
        assert!(!parsed.thumbnails);
        assert!(parsed.thumbnail_filmstrip);
        assert_eq!(parsed.thumbnail_width, 640);
        assert_eq!(parsed.scan_workers, 16);
        assert_eq!(parsed.bookmark_retention_days, 90);
        assert!(validate_http_config(&parsed).is_ok());

        let mut invalid = Config {
            thumbnail_width: 0,
            ..Config::default()
        };
        assert!(validate_http_config(&invalid)
            .unwrap_err()
            .to_string()
            .contains("thumbnail_width"));
        invalid = Config::default();
        invalid.album_art_names = vec!["../outside.jpg".into()];
        assert!(validate_http_config(&invalid)
            .unwrap_err()
            .to_string()
            .contains("album_art_names"));
        invalid = Config::default();
        invalid.scan_command_timeout_secs = 0;
        assert!(validate_http_config(&invalid)
            .unwrap_err()
            .to_string()
            .contains("scan_command_timeout_secs"));
        invalid = Config::default();
        invalid.scan_workers = 0;
        assert!(validate_http_config(&invalid)
            .unwrap_err()
            .to_string()
            .contains("scan_workers"));
        invalid.scan_workers = 65;
        assert!(validate_http_config(&invalid)
            .unwrap_err()
            .to_string()
            .contains("scan_workers"));
        invalid = Config::default();
        invalid.bookmark_retention_days = 36_501;
        assert!(validate_http_config(&invalid)
            .unwrap_err()
            .to_string()
            .contains("bookmark_retention_days"));
        assert!(toml::from_str::<Config>("friendy_name = \"typo\"")
            .unwrap_err()
            .to_string()
            .contains("unknown field"));
        assert!(toml::from_str::<Config>("[transcode]\nenabel = true")
            .unwrap_err()
            .to_string()
            .contains("unknown field"));
        assert!(toml::from_str::<Config>("[[remap]]\nacton = \"original\"")
            .unwrap_err()
            .to_string()
            .contains("unknown field"));
    }

    #[test]
    fn cache_and_database_directories_are_independent_and_config_relative() {
        let test_tree = TestTree::new("split-dirs");
        let tmp = test_tree.path();
        let app = App::from_config(
            Config {
                cache_dir: Some("derived-cache".into()),
                db_dir: Some("database".into()),
                rescan_secs: 0,
                ..Config::default()
            },
            18200,
            11900,
            tmp,
        );
        assert_eq!(app.cache_dir, tmp.join("derived-cache"));
        assert_eq!(
            app.scan_cfg.db_path.as_deref(),
            Some(tmp.join("database/files.db").as_path())
        );
        assert!(tmp.join("database/files.db").is_file());
    }

    #[test]
    fn transcode_validation_is_actionable_and_disabled_mode_needs_no_tools() {
        let mut cfg = Config::default();
        cfg.transcode.enable = true;
        cfg.transcode.encoder = "copy".into();
        let error = match App::try_from_config(cfg, 18200, 11900, &workspace()) {
            Ok(_) => panic!("invalid transcode encoder was accepted"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("must name a video encoder"),
            "{error}"
        );

        assert!(validate_transcode_tools(false, "not installed", &[])
            .unwrap()
            .is_empty());
        let missing_encoder = rusty_dlna_transcode::parse_remaps_toml(
            r#"
[[remap]]
hdr = "dv-p7"
action = "hdr10"
encoder = "rusty_encoder_that_does_not_exist"
"#,
        )
        .unwrap();
        let error = validate_transcode_tools(true, "libx264", &missing_encoder).unwrap_err();
        assert!(error.to_string().contains("ffmpeg -encoders"), "{error}");
    }

    #[test]
    fn uuid_is_validated_normalized_unique_and_persisted() {
        assert_eq!(
            normalize_uuid("4D696E69-444C-164E-9D41-98B7852028D3").unwrap(),
            "uuid:4d696e69-444c-164e-9d41-98b7852028d3"
        );
        assert_eq!(
            normalize_uuid("uuid:4d696e69-444c-164e-9d41-98b7852028d3").unwrap(),
            "uuid:4d696e69-444c-164e-9d41-98b7852028d3"
        );
        assert!(normalize_uuid("uuid:not-a-uuid").is_err());

        let test_tree = TestTree::new("uuid");
        let base = test_tree.path().to_path_buf();
        let first_dir = base.join("one");
        let second_dir = base.join("two");
        let first = load_or_create_uuid(&first_dir, None).unwrap();
        let again = load_or_create_uuid(&first_dir, None).unwrap();
        let second = load_or_create_uuid(&second_dir, None).unwrap();
        assert_eq!(first, again, "UUID must survive restart");
        assert_ne!(first, second, "independent caches need independent UUIDs");
        assert_eq!(&first[19..20], "4", "generated UUID must be version 4");
        assert!(matches!(&first[24..25], "8" | "9" | "a" | "b"));
        assert_eq!(
            std::fs::read_to_string(first_dir.join("uuid"))
                .unwrap()
                .trim(),
            first
        );

        let invalid = Config {
            uuid: Some("broken".into()),
            cache_dir: Some(base.join("invalid").display().to_string()),
            rescan_secs: 0,
            ..Config::default()
        };
        let error = App::try_from_config(invalid, 18200, 11900, &base)
            .err()
            .expect("invalid configured UUID");
        assert!(error.to_string().contains("uuid must be"), "{error}");
    }

    #[test]
    fn advertisement_selection_validates_interfaces_and_live_addresses() {
        let interfaces = vec![
            InterfaceV4 {
                name: "eth0".into(),
                addr: "192.0.2.20".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
            },
            InterfaceV4 {
                name: "eth1".into(),
                addr: "198.51.100.8".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
            },
            InterfaceV4 {
                name: "lo".into(),
                addr: Ipv4Addr::LOCALHOST,
                netmask: "255.0.0.0".parse().unwrap(),
            },
        ];
        let live = rusty_dlna_protocol::ssdp::SSDP_PORT;
        assert_eq!(
            select_advertise_ip(
                Some("192.0.2.20"),
                Ipv4Addr::UNSPECIFIED,
                &[],
                live,
                &interfaces,
                None,
            )
            .unwrap(),
            "192.0.2.20".parse::<Ipv4Addr>().unwrap()
        );
        assert!(select_advertise_ip(
            Some("203.0.113.9"),
            Ipv4Addr::UNSPECIFIED,
            &[],
            live,
            &interfaces,
            None,
        )
        .unwrap_err()
        .contains("not assigned"));
        assert!(select_advertise_ip(
            Some("127.0.0.1"),
            Ipv4Addr::UNSPECIFIED,
            &[],
            live,
            &interfaces,
            None,
        )
        .unwrap_err()
        .contains("loopback"));
        assert_eq!(
            select_advertise_ip(
                Some("127.0.0.1"),
                Ipv4Addr::UNSPECIFIED,
                &[],
                11900,
                &interfaces,
                None,
            )
            .unwrap(),
            Ipv4Addr::LOCALHOST
        );
        assert_eq!(
            select_advertise_ip(
                None,
                Ipv4Addr::UNSPECIFIED,
                &["eth1".into()],
                live,
                &interfaces,
                None,
            )
            .unwrap(),
            "198.51.100.8".parse::<Ipv4Addr>().unwrap()
        );
        assert!(select_advertise_ip(
            None,
            Ipv4Addr::UNSPECIFIED,
            &["missing0".into()],
            live,
            &interfaces,
            None,
        )
        .unwrap_err()
        .contains("no enabled IPv4"));
        assert_eq!(
            select_advertise_ip(
                None,
                Ipv4Addr::UNSPECIFIED,
                &[],
                live,
                &interfaces,
                Some("eth0"),
            )
            .unwrap(),
            "192.0.2.20".parse::<Ipv4Addr>().unwrap()
        );
        assert!(
            select_advertise_ip(None, Ipv4Addr::UNSPECIFIED, &[], live, &interfaces, None,)
                .unwrap_err()
                .contains("multiple LAN")
        );
        assert!(
            select_advertise_ip(None, Ipv4Addr::UNSPECIFIED, &[], live, &[], None,)
                .unwrap_err()
                .contains("no usable LAN")
        );

        let selected = select_ssdp_interfaces(
            &["eth0".into(), "eth1".into()],
            "192.0.2.20".parse().unwrap(),
            live,
            &interfaces,
        )
        .unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(
            reply_interface_for_sender(
                "198.51.100.77".parse().unwrap(),
                &selected,
                "192.0.2.20".parse().unwrap(),
            ),
            "198.51.100.8".parse::<Ipv4Addr>().unwrap()
        );
        let packets = msearch_replies(
            "uuid:test",
            "upnp:rootdevice",
            "198.51.100.8",
            8200,
            900,
            "Linux/1 UPnP/1.0 rustyDLNA/1",
            "Tue, 18 Aug 2026 00:00:00 GMT",
        );
        assert!(packets[0].contains("LOCATION: http://198.51.100.8:8200/rootDesc.xml"));

        let mut same_name = interfaces.clone();
        same_name.push(InterfaceV4 {
            name: "eth0".into(),
            addr: "192.0.2.21".parse().unwrap(),
            netmask: "255.255.255.0".parse().unwrap(),
        });
        let selected = select_ssdp_interfaces(
            &["eth0".into()],
            "192.0.2.20".parse().unwrap(),
            live,
            &same_name,
        )
        .unwrap();
        assert_eq!(selected.len(), 2, "all IPv4 addresses on a named interface");
    }

    #[test]
    fn renderer_location_policy_blocks_spoofed_and_off_link_targets() {
        let interfaces = vec![InterfaceV4 {
            name: "eth0".into(),
            addr: "192.0.2.20".parse().unwrap(),
            netmask: "255.255.255.0".parse().unwrap(),
        }];
        let sender: SocketAddr = "192.0.2.55:1900".parse().unwrap();
        assert_eq!(
            trusted_renderer_location(
                "http://192.0.2.55:1400/description.xml",
                sender,
                &interfaces,
            ),
            Some((
                "192.0.2.55".parse().unwrap(),
                1400,
                "/description.xml".into()
            ))
        );
        for url in [
            "http://192.0.2.99/description.xml",
            "http://127.0.0.1/description.xml",
            "http://239.255.255.250/description.xml",
            "http://192.0.2.55@127.0.0.1/description.xml",
            "http://192.0.2.55/description.xml\r\nX-Evil: yes",
        ] {
            assert!(
                trusted_renderer_location(url, sender, &interfaces).is_none(),
                "trusted spoofed URL {url:?}"
            );
        }
        assert!(trusted_renderer_location(
            "http://198.51.100.55/description.xml",
            "198.51.100.55:1900".parse().unwrap(),
            &interfaces,
        )
        .is_none());
    }

    #[test]
    fn renderer_http_response_requires_bounded_successful_xml() {
        let body = b"<root><friendlyName>Living Room</friendlyName></root>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut valid = response.into_bytes();
        valid.extend_from_slice(body);
        assert_eq!(renderer_xml_body(&valid), Some(body.as_slice()));

        for response in [
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/xml\r\nContent-Length: 0\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: 9999999\r\n\r\n",
        ] {
            assert!(renderer_xml_body(response).is_none());
        }
        let mut oversized = b"HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\n\r\n".to_vec();
        oversized.resize(oversized.len() + MAX_RENDERER_DESCRIPTION_BYTES + 1, b'x');
        assert!(renderer_xml_body(&oversized).is_none());
    }

    #[test]
    fn renderer_and_msearch_limiters_deduplicate_floods() {
        let sender: Ipv4Addr = "192.0.2.55".parse().unwrap();
        let now = std::time::Instant::now();
        let mut renderer = RendererFetchLimiter::default();
        assert!(renderer.allow(sender, "one", now));
        assert!(!renderer.allow(sender, "one", now));
        assert!(renderer.allow(sender, "two", now));
        assert!(renderer.allow(sender, "three", now));
        assert!(renderer.allow(sender, "four", now));
        assert!(!renderer.allow(sender, "five", now));
        assert!(renderer.allow(sender, "five", now + Duration::from_secs(61)));

        let mut replies = SsdpReplyLimiter::default();
        assert!(replies.allow(sender, 6, now));
        assert!(replies.allow(sender, 6, now));
        assert!(!replies.allow(sender, 1, now));
        assert!(replies.allow(sender, 6, now + Duration::from_secs(2)));
    }

    #[test]
    fn renderer_fetch_has_a_total_slow_response_deadline() {
        use std::io::Read;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request);
            std::thread::sleep(Duration::from_millis(1500));
        });
        let started = std::time::Instant::now();
        assert!(fetch_renderer_description(Ipv4Addr::LOCALHOST, port, "/slow").is_none());
        assert!(started.elapsed() < Duration::from_millis(1400));
        server.join().unwrap();
    }

    #[test]
    fn rootdesc_mediaserver_and_xbox() {
        let app = testdata_app();
        let r = app.handle(&req(&get("/rootDesc.xml", "Kodi/21.0")));
        let body = String::from_utf8_lossy(&r.body);
        assert_eq!(r.status, 200);
        assert!(body.contains("MediaServer:1"));
        assert!(body.contains("/ctl/ContentDir"));
        assert!(body.contains("/icons/sm.png"));
        let xbox = app.handle(&req(&get("/rootDesc.xml", "Xbox/1.0")));
        let xb = String::from_utf8_lossy(&xbox.body);
        assert!(xb.contains("<modelNumber>1</modelNumber>"));
        assert!(xb.contains(": 1"));
        let tv = app.handle(&req(&get(
            "/rootDesc.xml",
            "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0",
        )));
        let t = String::from_utf8_lossy(&tv.body);
        assert!(t.contains("sec:ProductCap"));
    }

    #[test]
    fn kodi_scpd_and_subscribe() {
        let app = testdata_app();
        let scpd = app.handle(&req(&get("/ContentDir.xml", "Kodi/21.0 Platinum/1.0.5.13")));
        assert_eq!(scpd.status, 200);
        let body = String::from_utf8_lossy(&scpd.body);
        assert!(body.contains("<name>Browse</name>"), "{body}");
        assert!(body.contains("BrowseDirectChildren"), "{body}");
        let sub = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:8200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
        let peer: SocketAddr = "192.0.2.50:1234".parse().unwrap();
        let r = app.handle_from(&sub, peer);
        assert_eq!(r.status, 200, "Platinum SUBSCRIBE must not 404");
        let sid = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("SID"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert!(sid.starts_with("uuid:"), "{sid}");
    }

    fn accept_notify(listener: &std::net::TcpListener, timeout: std::time::Duration) -> String {
        use std::io::{Read, Write};
        let start = std::time::Instant::now();
        listener.set_nonblocking(true).ok();
        loop {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    listener.set_nonblocking(false).ok();
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .ok();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 2048];
                    loop {
                        match sock.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                buf.extend_from_slice(&chunk[..read]);
                                let Some(header_end) =
                                    buf.windows(4).position(|w| w == b"\r\n\r\n")
                                else {
                                    continue;
                                };
                                let headers = String::from_utf8_lossy(&buf[..header_end]);
                                let content_length = headers
                                    .lines()
                                    .find_map(|line| {
                                        let (name, value) = line.split_once(':')?;
                                        name.eq_ignore_ascii_case("Content-Length")
                                            .then(|| value.trim().parse::<usize>().ok())
                                            .flatten()
                                    })
                                    .unwrap_or(0);
                                if buf.len() >= header_end + 4 + content_length {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = sock.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return String::from_utf8_lossy(&buf).into_owned();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() > timeout {
                        panic!("timed out waiting for GENA NOTIFY");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("notify accept: {e}"),
            }
        }
    }

    #[test]
    fn gena_subscribe_rules() {
        let app = testdata_app();
        let peer50: SocketAddr = "192.0.2.50:1234".parse().unwrap();
        let peer1: SocketAddr = "192.0.2.1:9".parse().unwrap();
        let new_ok = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
        let r = app.handle_from(&new_ok, peer50);
        assert_eq!(r.status, 200);
        let sid = resp_header(&r, "SID").unwrap_or("");
        assert!(sid.starts_with("uuid:"), "{sid}");
        assert_eq!(
            uuid::Uuid::parse_str(sid.trim_start_matches("uuid:"))
                .unwrap()
                .get_version_num(),
            4
        );
        assert_eq!(resp_header(&r, "Timeout"), Some("Second-300"));

        let inject = "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt\nX-Injected: 1>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n";
        assert!(matches!(
            HttpRequest::parse_headers(inject),
            Err(rusty_dlna_http::ParseError::InvalidHeaderValue)
        ));

        let mismatch = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
        assert_eq!(app.handle_from(&mismatch, peer1).status, 412);

        let both = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             SID: uuid:already-have-one\r\n\
             NT: upnp:event\r\n\
             Content-Length: 0\r\n\r\n");
        assert_eq!(app.handle_from(&both, peer50).status, 400);

        let no_nt = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
        assert_eq!(app.handle_from(&no_nt, peer50).status, 400);

        let renew_unknown = req("SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: uuid:00000000-0000-4000-8000-ffffffffffff\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n");
        assert_eq!(app.handle_from(&renew_unknown, peer50).status, 412);

        let unsub_unknown = req("UNSUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: uuid:00000000-0000-4000-8000-ffffffffffff\r\n\
             Content-Length: 0\r\n\r\n");
        assert_eq!(app.handle_from(&unsub_unknown, peer50).status, 412);

        let renew = req(&format!(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: {sid}\r\n\
             Timeout: Second-1800\r\n\
             Content-Length: 0\r\n\r\n"
        ));
        let r = app.handle_from(&renew, peer50);
        assert_eq!(r.status, 200);
        assert_eq!(resp_header(&r, "Timeout"), Some("Second-1800"));
        assert_eq!(resp_header(&r, "SID"), Some(sid));

        let renew_low = req(&format!(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\nHost: 192.0.2.10:18200\r\nSID: {sid}\r\nTimeout: Second-1\r\nContent-Length: 0\r\n\r\n"
        ));
        let r = app.handle_from(&renew_low, peer50);
        assert_eq!(resp_header(&r, "Timeout"), Some("Second-30"));
    }

    #[test]
    fn gena_notify_on_catalog_bump() {
        let app = testdata_app();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("notify listener");
        let addr = listener.local_addr().expect("listener addr");
        let port = addr.port();
        assert_ne!(port, 8200, "test callback must not be LAN :8200");
        let sub = req(&format!(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 127.0.0.1:18200\r\n\
             Callback: <http://127.0.0.1:{port}/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n"
        ));
        let r = app.handle_from(&sub, addr);
        assert_eq!(r.status, 200);
        let n0 = accept_notify(&listener, std::time::Duration::from_secs(3));
        assert!(
            n0.contains("NTS: upnp:propchange") || n0.contains("NTS:upnp:propchange"),
            "{n0}"
        );
        assert!(n0.contains("SystemUpdateID"), "{n0}");
        assert!(n0.contains("SEQ: 0"), "{n0}");
        let id0 = n0
            .split("<SystemUpdateID>")
            .nth(1)
            .and_then(|s| s.split('<').next())
            .unwrap_or("");
        let before = app.update_id.load(Ordering::Relaxed);
        let cat = read_recover(&app.catalog).clone();
        apply_catalog(
            &app,
            cat,
            ScanDelta {
                changed: 1,
                ..ScanDelta::default()
            },
            "test catalog bump",
        )
        .unwrap();
        let after = app.update_id.load(Ordering::Relaxed);
        assert!(after > before, "update_id {before} -> {after}");
        let n1 = accept_notify(&listener, std::time::Duration::from_secs(3));
        assert!(
            n1.contains("NTS: upnp:propchange") || n1.contains("NTS:upnp:propchange"),
            "{n1}"
        );
        assert!(n1.contains("SEQ: 1"), "{n1}");
        assert!(
            n1.contains(&format!("<SystemUpdateID>{after}</SystemUpdateID>")),
            "{n1}"
        );
        if !id0.is_empty() {
            assert_ne!(id0, after.to_string());
        }
        if let Some(p) = app.scan_cfg.db_path.as_ref() {
            let db = LibraryDb::open(p).expect("db");
            assert_eq!(db.get_update_id().unwrap(), after);
        }
    }

    #[test]
    fn catalog_publication_failure_keeps_catalog_and_update_id_in_sync() {
        let app = testdata_app();
        let path = app.scan_cfg.db_path.as_ref().unwrap().clone();
        let mut next = app
            .catalog
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let item_id = next.items.keys().next().expect("fixture item").clone();
        let old_title = next.items[&item_id].title.clone();
        next.items.get_mut(&item_id).unwrap().title = "must-not-publish".into();
        let old_update_id = app.update_id.load(Ordering::Relaxed);

        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let result = apply_catalog(
            &app,
            next,
            ScanDelta {
                changed: 1,
                ..ScanDelta::default()
            },
            "injected publication failure",
        );
        assert!(result.is_err());
        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(app.update_id.load(Ordering::Relaxed), old_update_id);
        assert_eq!(
            app.catalog
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .items[&item_id]
                .title,
            old_title
        );
    }

    #[test]
    fn host_and_timeseek_errors() {
        let app = testdata_app();
        let no_host = req("GET /rootDesc.xml HTTP/1.1\r\n\r\n");
        assert_eq!(app.handle(&no_host).status, 400);
        let local = req("GET /rootDesc.xml HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(app.handle(&local).status, 400);
        let ts = req(
            "GET /MediaItems/1.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nTimeSeekRange.dlna.org: npt=0-\r\n\r\n",
        );
        assert_eq!(app.handle(&ts).status, 406);

        let soap_rebind = req(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: attacker.example\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(app.handle(&soap_rebind).status, 400);
        let soap_no_host = req(
            "POST /ctl/ContentDir HTTP/1.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(app.handle(&soap_no_host).status, 400);
        let sub_rebind = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\nHost: localhost\r\nCallback: <http://127.0.0.1:9/e>\r\nNT: upnp:event\r\n\r\n",
        );
        assert_eq!(app.handle(&sub_rebind).status, 400);
        let unsub_rebind =
            req("UNSUBSCRIBE /evt/ContentDir HTTP/1.1\r\nHost: evil.test\r\nSID: uuid:x\r\n\r\n");
        assert_eq!(app.handle(&unsub_rebind).status, 400);
    }

    #[test]
    fn sidecar_size_and_symlink_jail() {
        let app = testdata_app();
        let (art_id, detail_id) = {
            let cat = read_recover(&app.catalog);
            let movie = cat
                .items
                .values()
                .find(|i| i.path.ends_with("movie.mkv"))
                .expect("movie");
            (movie.album_art, movie.detail_id)
        };
        assert!(art_id > 0);

        let outside_tree = TestTree::new("sidecar-outside");
        let outside = outside_tree.path().join(format!("secret-{detail_id}"));
        std::fs::write(&outside, b"not-a-poster").unwrap();
        {
            let mut cat = write_recover(&app.catalog);
            cat.album_art_paths.insert(art_id, outside.clone());
        }
        let escaped = app.handle(&req(&get(
            &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
            "Kodi/21.0",
        )));
        assert_eq!(escaped.status, 404, "path outside media/cache must 404");

        let cache = app.cache_dir.clone();
        let _ = std::fs::create_dir_all(&cache);
        let big = cache.join(format!(
            "rdlna-oversized-{}-{}.jpg",
            std::process::id(),
            detail_id
        ));
        {
            let f = std::fs::File::create(&big).unwrap();
            f.set_len(rusty_dlna_scan::MAX_SIDECAR_BYTES + 1).unwrap();
        }
        {
            let mut cat = write_recover(&app.catalog);
            cat.album_art_paths.insert(art_id, big.clone());
        }
        let huge = app.handle(&req(&get(
            &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
            "Kodi/21.0",
        )));
        assert_eq!(huge.status, 413, "oversized sidecar must 413");

        let link = cache.join(format!(
            "rdlna-escape-{}-{}.jpg",
            std::process::id(),
            detail_id
        ));
        let _ = std::os::unix::fs::symlink(&outside, &link);
        {
            let mut cat = write_recover(&app.catalog);
            cat.album_art_paths.insert(art_id, link.clone());
        }
        let via_link = app.handle(&req(&get(
            &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
            "Kodi/21.0",
        )));
        assert_eq!(via_link.status, 404, "symlink out of tree must 404");

        let _ = std::fs::remove_file(&big);
        let _ = std::fs::remove_file(&link);
    }

    #[cfg(unix)]
    #[test]
    fn http_uses_the_same_wide_links_policy_as_the_scanner() {
        let mut app = testdata_app();
        let root = app.scan_cfg.media_dirs[0].join("video");
        let suffix = format!("{}", std::process::id());
        let outside_tree = TestTree::new("wide-source");
        let outside = outside_tree.path().join(format!("source-{suffix}.mkv"));
        let link = root.join(format!("wide-link-{suffix}.mkv"));
        std::fs::write(&outside, b"outside media bytes").unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let mut item = movie_fixture(&app);
        item.detail_id = 9_100_001;
        item.object_id = "wide-link-object".into();
        item.path = link.clone();
        item.size = std::fs::metadata(&outside).unwrap().len();
        {
            let mut cat = app.catalog.write().unwrap();
            cat.by_detail.insert(item.detail_id, item.object_id.clone());
            cat.items.insert(item.object_id.clone(), item.clone());
        }
        let request = req(&format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
            item.detail_id
        ));
        assert_eq!(app.handle(&request).status, 403);

        app.cfg.wide_links = true;
        app.scan_cfg.wide_links = true;
        let allowed = app.handle(&request);
        assert_eq!(allowed.status, 200);
        assert_eq!(allowed.body, b"outside media bytes");

        let _ = std::fs::remove_file(&link);
    }

    fn soap_action(app: &App, action: &str, inner: &str, ua: &str) -> (u16, String) {
        let body = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">{inner}</u:{action}></s:Body></s:Envelope>"#
        );
        let raw = format!(
            "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#{action}\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
            body.len()
        );
        let mut req = HttpRequest::parse_headers(&raw).unwrap();
        req.body = body.into_bytes();
        let r = app.handle(&req);
        (r.status, String::from_utf8_lossy(&r.body).into_owned())
    }

    fn status_row_count(body: &str, label: &str) -> Option<u32> {
        let marker = format!("<tr><td>{label}</td><td>");
        let rest = body.split(&marker).nth(1)?;
        rest.split("</td>").next()?.parse().ok()
    }

    fn soap_browse(app: &App, oid: &str, flag: &str, ua: &str) -> (u16, String) {
        soap_browse_page(app, oid, flag, ua, 0, 0)
    }

    fn soap_browse_page(
        app: &App,
        oid: &str,
        flag: &str,
        ua: &str,
        start: usize,
        requested: usize,
    ) -> (u16, String) {
        let body = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>{oid}</ObjectID><BrowseFlag>{flag}</BrowseFlag><Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>{requested}</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#
        );
        let raw = format!(
            "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
            body.len()
        );
        let mut req = HttpRequest::parse_headers(&raw).unwrap();
        req.body = body.into_bytes();
        let r = app.handle(&req);
        (r.status, String::from_utf8_lossy(&r.body).into_owned())
    }

    fn add_large_catalog_page(app: &App, count: usize) {
        let mut cat = write_recover(&app.catalog);
        let template = cat.items.values().next().cloned().expect("fixture item");
        let mut ids = Vec::with_capacity(count);
        for index in 0..count {
            let id = format!("2$8$B{index:X}");
            let mut item = template.clone();
            item.object_id = id.clone();
            item.parent_id = "2$8".into();
            item.detail_id = 1_000_000 + index as i64;
            item.title = format!("Bounded item {index:06} {}", "<&snow雪>".repeat(96));
            item.ref_id = None;
            cat.items.insert(id.clone(), item);
            ids.push(id);
        }
        cat.containers
            .get_mut("2$8")
            .expect("all-video container")
            .children
            .extend(ids);
    }

    #[test]
    fn browse_root_and_kodi_original() {
        let app = testdata_app();
        let (st, xml) = soap_browse(&app, "0", "BrowseDirectChildren", "Kodi/21.0 (Linux)");
        assert_eq!(st, 200);
        assert!(xml.contains("&lt;DIDL-Lite"));
        assert!(xml.contains("id=\"64\"") || xml.contains("id=&quot;64&quot;"));
        assert!(xml.contains("id=\"1\"") || xml.contains("id=&quot;1&quot;"));
        assert!(xml.contains("id=\"2\"") || xml.contains("id=&quot;2&quot;"));
        let (stv, video) = soap_browse(&app, "2", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(stv, 200);
        assert!(video.contains("All Video"), "{video}");
        assert!(video.contains("Recently Added"), "{video}");
        assert!(video.contains("Folders"), "{video}");
        assert!(video.contains("Series"), "{video}");
        assert!(video.contains("Genre"), "{video}");
        assert!(
            video.contains("id=\"2$8\"") || video.contains("id=&quot;2$8&quot;"),
            "{video}"
        );
        assert!(
            video.contains("id=\"2$15\"") || video.contains("id=&quot;2$15&quot;"),
            "{video}"
        );
        assert!(
            video.contains("id=\"2$FF0\"") || video.contains("id=&quot;2$FF0&quot;"),
            "{video}"
        );
        assert!(
            video.contains("&lt;container ") && video.contains("storageUsed"),
            "folder DIDL (container + storageUsed) required for VLC expand: {video}"
        );
        assert!(video.contains("object.container.storageFolder"), "{video}");
        // items live under 2$8
        let (st2, items) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st2, 200);
        assert!(items.contains("/MediaItems/"));
        assert!(
            items.contains("size="),
            "res@size required for VLC: {items}"
        );
        assert!(
            items.contains("duration="),
            "res@duration H:MM:SS.mmm required for VLC length: {items}"
        );
        assert!(
            items.contains("dc:date&gt;") || items.contains("&lt;dc:date&gt;"),
            "missing dc:date in {items}"
        );
        // date is …Z or 10 chars
        assert!(
            items.contains("1999-01-01") || items.contains("Z&lt;/dc:date"),
            "date not normalized: {items}"
        );
        assert!(!items.contains("/Transcode/"), "Kodi must stay original");
    }

    #[test]
    fn browse_uses_nfo_title_not_filename() {
        let app = testdata_app();
        let (st, items) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(
            items.contains("Fixture Movie"),
            "DIDL dc:title must use NFO title: {items}"
        );
        assert!(
            !items.contains("&lt;dc:title&gt;movie&lt;/dc:title&gt;") && !items.contains(">movie<"),
            "filename-only title must not be the movie item title: {items}"
        );
    }

    #[test]
    fn browse_series_seasons_and_genre() {
        let app = testdata_app();
        let (st, series) = soap_browse(&app, "2$E", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(
            series.contains("The Show"),
            "Series must list showtitle: {series}"
        );
        let show_id = {
            let cat = app.catalog.read().unwrap();
            cat.containers
                .values()
                .find(|c| c.parent_id == "2$E" && c.title == "The Show")
                .map(|c| c.object_id.clone())
                .expect("show container")
        };
        let (st, seasons) = soap_browse(&app, &show_id, "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(seasons.contains("Season 1"), "{seasons}");
        let season_id = {
            let cat = app.catalog.read().unwrap();
            cat.containers
                .values()
                .find(|c| c.parent_id == show_id && c.title == "Season 1")
                .map(|c| c.object_id.clone())
                .expect("season 1")
        };
        let (st, eps) = soap_browse(&app, &season_id, "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(eps.contains("Pilot"), "episode title under season: {eps}");
        assert!(eps.contains("/MediaItems/"), "{eps}");
        assert!(
            eps.contains("upnp:episodeSeason") || eps.contains("episodeSeason"),
            "{eps}"
        );
        let (st, genres) = soap_browse(&app, "2$9", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(
            genres.contains("Drama") || genres.contains("Crime"),
            "{genres}"
        );
        let drama_id = {
            let cat = app.catalog.read().unwrap();
            cat.containers
                .values()
                .find(|c| c.parent_id == "2$9" && (c.title == "Drama" || c.title == "Crime"))
                .map(|c| c.object_id.clone())
                .expect("genre folder")
        };
        let (st, items) = soap_browse(&app, &drama_id, "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(
            items.contains("The Show") || items.contains("Pilot"),
            "{items}"
        );
    }

    #[test]
    fn album_art_get_and_didl() {
        let app = testdata_app();
        let (st, items) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(
            items.contains("movie")
                || items.contains("Fixture Movie")
                || items.contains("&lt;dc:title&gt;movie"),
            "movie item missing: {items}"
        );
        assert!(
            items.contains("/AlbumArt/"),
            "DIDL missing /AlbumArt/: {items}"
        );
        assert!(
            items.contains("JPEG_TN") || items.contains("albumArtURI"),
            "DIDL missing JPEG_TN or albumArtURI: {items}"
        );
        let (art_id, detail_id) = parse_album_art_url(&items).expect("parse art url from DIDL");
        assert!(art_id > 0, "art id from DIDL");
        {
            let cat = read_recover(&app.catalog);
            let movie = cat
                .items
                .values()
                .find(|i| i.path.ends_with("movie.mkv"))
                .expect("movie in catalog");
            assert_eq!(art_id, movie.album_art);
            assert_eq!(detail_id, movie.detail_id);
        }

        let r = app.handle(&req(&get(
            &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
            "Kodi/21.0",
        )));
        assert_eq!(r.status, 200, "album art GET");
        assert_eq!(resp_header(&r, "Content-Type"), Some("image/jpeg"));
        assert!(
            r.body.len() >= 3 && r.body[0] == 0xff && r.body[1] == 0xd8,
            "JPEG magic"
        );
        assert_eq!(
            resp_header(&r, "transferMode.dlna.org"),
            Some("Interactive")
        );
        let feats = resp_header(&r, "contentFeatures.dlna.org").unwrap_or("");
        assert!(feats.contains("JPEG_TN"), "contentFeatures={feats}");

        let missing = app.handle(&req(&get("/AlbumArt/999999-1.jpg", "Kodi/21.0")));
        assert_eq!(missing.status, 404);

        let streaming = format!(
            "GET /AlbumArt/{art_id}-{detail_id}.jpg HTTP/1.1\r\nHost: 127.0.0.1:18200\r\ntransferMode.dlna.org: Streaming\r\n\r\n"
        );
        assert_eq!(app.handle(&req(&streaming)).status, 406);

        let ranged = format!(
            "GET /AlbumArt/{art_id}-{detail_id}.jpg HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-10\r\n\r\n"
        );
        assert_eq!(app.handle(&req(&ranged)).status, 406);

        let thumb = app.handle(&req(&get(
            &format!("/Thumbnails/{detail_id}.jpg"),
            "Kodi/21.0",
        )));
        assert_eq!(thumb.status, 200, "native thumb uses album art");
        assert_eq!(resp_header(&thumb, "Content-Type"), Some("image/jpeg"));
        let ffmpeg_ok = std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !ffmpeg_ok {
            eprintln!("skip /Resized/ GET (ffmpeg missing)");
        } else {
            let resized = app.handle(&req(&get(
                &format!("/Resized/{detail_id}.jpg?width=160,height=160"),
                "Kodi/21.0",
            )));
            assert_eq!(resized.status, 200, "resized GET");
            assert_eq!(resp_header(&resized, "Content-Type"), Some("image/jpeg"));
            assert_eq!(
                resp_header(&resized, "transferMode.dlna.org"),
                Some("Interactive")
            );
            assert!(
                resized.body.len() >= 2 && resized.body[0] == 0xff && resized.body[1] == 0xd8,
                "JPEG magic"
            );
            let feats = resp_header(&resized, "contentFeatures.dlna.org").unwrap_or("");
            assert!(feats.contains("JPEG_TN"), "contentFeatures={feats}");
            assert!(
                feats.contains("DLNA.ORG_CI=1"),
                "contentFeatures CI=1: {feats}"
            );
        }

        let xbox = app.handle(&req(&get(
            &format!("/MediaItems/{detail_id}.mkv?albumArt=true"),
            "Xbox/2.0.58767.0 UPnP/1.0 Xbox/2.0.58767.0",
        )));
        assert_eq!(xbox.status, 200);
        assert_eq!(resp_header(&xbox, "Content-Type"), Some("image/jpeg"));
        assert!(xbox.body.starts_with(&[0xff, 0xd8]));
    }

    #[test]
    fn root_container_v_browse_zero_parent_is_root() {
        let mut app = testdata_app();
        app.cfg.root_container = Some("V".into());
        let (st, xml) = soap_browse(
            &app,
            "0",
            "BrowseDirectChildren",
            "VLC/3.0.21 LibVLC/3.0.21",
        );
        assert_eq!(st, 200);
        assert!(xml.contains("All Video"), "{xml}");
        assert!(xml.contains("Folders"), "{xml}");
        assert!(xml.contains("Recently Added"), "{xml}");
        assert!(xml.contains("Series"), "{xml}");
        assert!(xml.contains("Genre"), "{xml}");
        assert!(
            xml.contains("parentID=\"0\"") || xml.contains("parentID=&quot;0&quot;"),
            "remapped root advertises parentID=0: {xml}"
        );
        assert!(
            xml.contains("&lt;container ")
                && xml.contains("object.container.storageFolder")
                && xml.contains("storageUsed&gt;-1"),
            "VLC expand marker needs container + storageFolder + storageUsed: {xml}"
        );
        assert!(
            !xml.contains("&lt;item "),
            "root video view is folders only: {xml}"
        );
    }

    #[test]
    fn browse_metadata_root_and_search() {
        let app = testdata_app();
        let (st, meta) = soap_browse(&app, "0", "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(
            meta.contains("parentID=\"-1\"") || meta.contains("parentID=&quot;-1&quot;"),
            "{meta}"
        );
        let body = r#"<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Search>"#;
        let raw = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:x#Search\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut req = HttpRequest::parse_headers(&raw).unwrap();
        req.body = body.as_bytes().to_vec();
        let r = app.handle(&req);
        assert_eq!(r.status, 200);
        let xml = String::from_utf8_lossy(&r.body);
        assert!(xml.contains("SearchResponse"));
        assert!(xml.contains("&lt;DIDL-Lite"));
        assert!(xml.contains("/MediaItems/"));
    }

    #[test]
    fn large_browse_and_search_are_byte_and_page_bounded() {
        let mut app = testdata_app();
        // This fixture extends only the in-memory generation; production
        // requests use the SQLite query path when DB/catalog populations agree.
        app.scan_cfg.db_path = None;
        add_large_catalog_page(&app, 5_000);
        let expected_browse_total = app
            .catalog
            .read()
            .unwrap()
            .containers
            .get("2$8")
            .unwrap()
            .children
            .len() as u32;

        let started = std::time::Instant::now();
        let (status, browse) =
            soap_browse_page(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0", 0, 0);
        assert_eq!(status, 200);
        assert!(browse.len() <= MAX_SOAP_RESPONSE_BYTES, "{}", browse.len());
        let returned: u32 = xml_tag_text(&browse, "NumberReturned")
            .unwrap()
            .parse()
            .unwrap();
        let total: u32 = xml_tag_text(&browse, "TotalMatches")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(total, expected_browse_total);
        assert!(
            returned > 0 && returned < total,
            "returned={returned} total={total}"
        );
        assert!(returned as usize <= MAX_SOAP_PAGE_OBJECTS);

        let (_, page) = soap_browse_page(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0", 10, 3);
        assert_eq!(xml_tag_text(&page, "NumberReturned").as_deref(), Some("3"));
        assert_eq!(
            xml_tag_text(&page, "TotalMatches")
                .unwrap()
                .parse::<u32>()
                .unwrap(),
            expected_browse_total
        );
        let (_, past_end) = soap_browse_page(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "Kodi/21.0",
            usize::try_from(i32::MAX).unwrap(),
            0,
        );
        assert_eq!(
            xml_tag_text(&past_end, "NumberReturned").as_deref(),
            Some("0")
        );
        assert_eq!(
            xml_tag_text(&past_end, "TotalMatches")
                .unwrap()
                .parse::<u32>()
                .unwrap(),
            expected_browse_total
        );

        let (status, search) = soap_action(
            &app,
            "Search",
            r#"<ContainerID>0</ContainerID><SearchCriteria>dc:title contains "Bounded item"</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>"#,
            "Kodi/21.0",
        );
        assert_eq!(status, 200);
        assert!(search.len() <= MAX_SOAP_RESPONSE_BYTES, "{}", search.len());
        assert_eq!(
            xml_tag_text(&search, "TotalMatches").as_deref(),
            Some("5000")
        );
        let search_returned: u32 = xml_tag_text(&search, "NumberReturned")
            .unwrap()
            .parse()
            .unwrap();
        assert!(search_returned > 0 && search_returned < 5_000);

        let (_, search_past_end) = soap_action(
            &app,
            "Search",
            r#"<ContainerID>0</ContainerID><SearchCriteria>dc:title contains "Bounded item"</SearchCriteria><Filter>*</Filter><StartingIndex>2147483647</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>"#,
            "Kodi/21.0",
        );
        assert_eq!(
            xml_tag_text(&search_past_end, "NumberReturned").as_deref(),
            Some("0")
        );
        assert_eq!(
            xml_tag_text(&search_past_end, "TotalMatches").as_deref(),
            Some("5000")
        );
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "large-catalog regression took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn pagination_totals_and_sort_stability_hold_for_every_small_page() {
        let app = testdata_app();
        let cat = app.catalog.read().unwrap();
        let parent = rusty_dlna_protocol::object_id::BROWSEDIR_ID;
        let (all, total) =
            sorted_child_page(&cat, parent, 0, usize::MAX, &[], DefaultOrder::FoldersFirst)
                .expect("Browse Folders container");
        assert_eq!(usize::try_from(total).unwrap(), all.len());
        for start in 0..=all.len().saturating_add(2) {
            for take in 0..=all.len().saturating_add(2) {
                let (page, page_total) =
                    sorted_child_page(&cat, parent, start, take, &[], DefaultOrder::FoldersFirst)
                        .unwrap();
                assert_eq!(page_total, total);
                assert_eq!(page.len(), take.min(all.len().saturating_sub(start)));
                let page_ids: Vec<_> = page
                    .iter()
                    .map(|child| match child {
                        CatalogChild::Container(value) => value.object_id.as_str(),
                        CatalogChild::Item(value) => value.object_id.as_str(),
                    })
                    .collect();
                let expected_ids: Vec<_> = all
                    .iter()
                    .skip(start)
                    .take(take)
                    .map(|child| match child {
                        CatalogChild::Container(value) => value.object_id.as_str(),
                        CatalogChild::Item(value) => value.object_id.as_str(),
                    })
                    .collect();
                assert_eq!(page_ids, expected_ids);
            }
        }

        let template = cat.items.values().next().expect("fixture item").clone();
        drop(cat);
        let mut equal_keys = Vec::new();
        for id in ["stable-c", "stable-a", "stable-b"] {
            let mut item = template.clone();
            item.object_id = id.into();
            item.title = "same title".into();
            equal_keys.push(CatalogChild::Item(Box::new(item)));
        }
        sort_catalog_children(
            &mut equal_keys,
            &[SortSpec {
                key: SortKey::Title,
                descending: false,
            }],
            DefaultOrder::FoldersFirst,
        );
        let ids: Vec<_> = equal_keys
            .iter()
            .map(|child| match child {
                CatalogChild::Container(value) => value.object_id.as_str(),
                CatalogChild::Item(value) => value.object_id.as_str(),
            })
            .collect();
        assert_eq!(ids, ["stable-c", "stable-a", "stable-b"]);
    }

    #[test]
    fn published_catalog_object_ids_are_globally_unique() {
        let app = testdata_app();
        let cat = app.catalog.read().unwrap();
        let mut ids = std::collections::HashSet::new();
        for id in cat.containers.keys().chain(cat.items.keys()) {
            assert!(ids.insert(id), "duplicate published object ID: {id}");
        }
        for (parent_id, container) in &cat.containers {
            let mut children = std::collections::HashSet::new();
            for child_id in &container.children {
                assert!(
                    children.insert(child_id),
                    "duplicate child {child_id} under {parent_id}"
                );
            }
        }
    }

    #[test]
    fn request_time_sqlite_query_matches_the_published_catalog_generation() {
        let app = testdata_app();
        let query = CatalogQuery {
            groups: vec![vec![CatalogQueryClause {
                field: CatalogQueryField::Class,
                op: CatalogQueryOp::DerivedFrom("object.item.videoItem".into()),
            }]],
            sort: vec![CatalogQuerySort {
                field: CatalogQueryField::Title,
                descending: false,
            }],
            default_order: CatalogDefaultOrder::FoldersFirst,
        };
        let page = query_db_search(
            app.db_pool.as_ref(),
            app.scan_cfg.db_path.as_deref(),
            "0",
            &query,
            0,
            MAX_SOAP_PAGE_OBJECTS,
        )
        .expect("SQLite query path");
        let cat = app.catalog.read().unwrap();
        assert_eq!(page.population, catalog_population(&cat));
        let materialized = materialize_db_page(&cat, &page).expect("same DB/catalog generation");
        assert_eq!(materialized.len(), page.object_ids.len());
        assert!(page.total >= 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_soap_client_disconnect_terminates_handler() {
        use tokio::io::AsyncWriteExt;

        let mut configured = testdata_app();
        configured.scan_cfg.db_path = None;
        let app = Arc::new(configured);
        add_large_catalog_page(&app, 5_000);
        let body = r#"<u:Browse><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse>"#;
        let request = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            let (socket, peer) = listener.accept().await.unwrap();
            handle_conn(server_app, socket, peer).await
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        drop(client);
        let outcome = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("disconnected SOAP handler did not terminate")
            .expect("handler task panicked");
        if let Err(error) = outcome {
            let text = error.to_string();
            assert!(
                text.contains("Broken pipe")
                    || text.contains("reset")
                    || text.contains("closed")
                    || text.contains("Connection"),
                "unexpected disconnect error: {text}"
            );
        }
    }

    #[test]
    fn missing_objectid_is_402_unknown_is_401() {
        let app = testdata_app();
        let body = r#"<s:Envelope><s:Body><u:Browse></u:Browse></s:Body></s:Envelope>"#;
        let raw = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut req = HttpRequest::parse_headers(&raw).unwrap();
        req.body = body.as_bytes().to_vec();
        let r = app.handle(&req);
        assert_eq!(r.status, 500);
        let xml = String::from_utf8_lossy(&r.body);
        assert!(xml.contains("<errorCode>402</errorCode>"));

        let raw = "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Nope\"\r\nContent-Length: 0\r\n\r\n";
        let r = app.handle(&req_from(raw, b""));
        assert_eq!(r.status, 500);
        assert!(String::from_utf8_lossy(&r.body).contains("<errorCode>401</errorCode>"));
    }

    fn req_from(headers: &str, body: &[u8]) -> HttpRequest {
        let mut r = HttpRequest::parse_headers(headers).unwrap();
        r.body = body.to_vec();
        r
    }

    async fn raw_connection(app: Arc<App>, bytes: &[u8], close_write: bool) -> Vec<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, peer) = listener.accept().await.unwrap();
            handle_conn(app, socket, peer).await.unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        if !bytes.is_empty() {
            client.write_all(bytes).await.unwrap();
        }
        if close_write {
            client.shutdown().await.unwrap();
        }
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(4), client.read_to_end(&mut response))
            .await
            .expect("server response timeout")
            .unwrap();
        server.await.unwrap();
        response
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_parser_preserves_pipeline_and_rejects_smuggling() {
        let app = Arc::new(testdata_app());
        let pipeline = concat!(
            "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
            "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n\r\n"
        );
        let response = raw_connection(app.clone(), pipeline.as_bytes(), true).await;
        let text = String::from_utf8_lossy(&response);
        assert_eq!(
            text.matches("HTTP/1.1 200 OK").count(),
            2,
            "pipelined response bytes: {text}"
        );

        let body_pipeline = concat!(
            "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n",
            "Content-Length: 1\r\n\r\n",
            "x",
            "GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n\r\n"
        );
        let response = raw_connection(app.clone(), body_pipeline.as_bytes(), true).await;
        let text = String::from_utf8_lossy(&response);
        assert_eq!(
            text.matches("HTTP/1.1 200 OK").count(),
            2,
            "body plus pipelined request was not preserved: {text}"
        );

        for malformed in [
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1, 1\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1\r\nTransfer-Encoding: identity\r\n\r\n",
            "GET / HTTP/1.1\r\nHost : 127.0.0.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nhOsT: 127.0.0.2\r\n\r\n",
            "GET / HTTP/9.9\r\nHost: 127.0.0.1\r\n\r\n",
        ] {
            let response = raw_connection(app.clone(), malformed.as_bytes(), true).await;
            assert!(
                response.starts_with(b"HTTP/1.1 400 Bad Request"),
                "malformed request was not 400: {}",
                String::from_utf8_lossy(&response)
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_body_caps_incomplete_bodies_and_slow_headers_are_bounded() {
        let mut configured = testdata_app();
        configured.cfg.max_request_body_bytes = 8;
        configured.cfg.header_read_timeout_secs = 1;
        configured.cfg.body_read_timeout_secs = 1;
        let app = Arc::new(configured);

        let oversized = raw_connection(
            app.clone(),
            b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 9\r\n\r\n",
            true,
        )
        .await;
        assert!(oversized.starts_with(b"HTTP/1.1 413 Payload Too Large"));

        let incomplete = raw_connection(
            app.clone(),
            b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\n\r\nab",
            true,
        )
        .await;
        assert!(incomplete.starts_with(b"HTTP/1.1 400 Bad Request"));

        let started = std::time::Instant::now();
        let timeout = raw_connection(app, b"G", false).await;
        assert!(timeout.starts_with(b"HTTP/1.1 408 Request Timeout"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_connections_holds_excess_clients_in_the_kernel_backlog() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut configured = testdata_app();
        configured.cfg.max_connections = 1;
        let app = Arc::new(configured);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(accept_loop(listener, app));

        let mut first = tokio::net::TcpStream::connect(address).await.unwrap();
        first.write_all(b"G").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut second = tokio::net::TcpStream::connect(address).await.unwrap();
        second
            .write_all(
                b"GET /rootDesc.xml HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut byte = [0u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(200), second.read(&mut byte))
                .await
                .is_err(),
            "second connection was serviced while the sole permit was occupied"
        );

        first.shutdown().await.unwrap();
        drop(first);
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), second.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        server.abort();
    }

    #[test]
    fn original_get_and_two_ranges() {
        let app = testdata_app();
        let movie = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|i| i.path.ends_with("movie.mkv"))
            .cloned()
            .expect("movie fixture");
        let id = movie.detail_id;
        let path = movie.path.clone();
        let expect = std::fs::read(&path).unwrap();
        let raw = format!(
            "GET /MediaItems/{id}.ignored HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n"
        );
        let r = app.handle(&req(&raw));
        assert_eq!(r.status, 200);
        assert_eq!(r.body, expect);
        let hdrs = r
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(hdrs.contains("Accept-Ranges: bytes"));
        assert!(hdrs.contains("DLNA.ORG_OP=01"));
        assert!(hdrs.contains("DLNA.ORG_CI=0"));
        assert!(!r.persist);

        let r1 = app.handle(&req(&format!(
            "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-99\r\n\r\n"
        )));
        assert_eq!(r1.status, 206);
        assert_eq!(r1.body, expect[0..100]);
        let r2 = app.handle(&req(&format!(
            "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=100-199\r\n\r\n"
        )));
        assert_eq!(r2.status, 206);
        assert_eq!(r2.body, expect[100..200]);

        let bad = app.handle(&req(&format!(
            "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=abc\r\n\r\n"
        )));
        assert_eq!(bad.status, 400);
        let past = app.handle(&req(&format!(
            "GET /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=9999999-99999999\r\n\r\n"
        )));
        assert_eq!(past.status, 416);
    }

    #[cfg(unix)]
    #[test]
    fn original_get_opens_non_utf8_catalog_path_without_loss() {
        use std::os::unix::ffi::OsStringExt;

        let mut app = testdata_app();
        let template = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|item| item.path.ends_with("movie.mkv"))
            .cloned()
            .expect("movie fixture");
        let test_tree = TestTree::new("http-nonutf8");
        let dir = test_tree.path().to_path_buf();
        let mut raw_name = b"movie-".to_vec();
        raw_name.push(0x80);
        raw_name.extend_from_slice(b".mkv");
        let path = dir.join(std::ffi::OsString::from_vec(raw_name));
        let expected = b"non-utf8 media body";
        std::fs::write(&path, expected).unwrap();
        app.scan_cfg.media_roots.clear();
        app.scan_cfg.media_dirs = vec![dir.clone()];
        let mut item = template;
        item.object_id = "64$nonutf8".into();
        item.detail_id = 9_876_543;
        item.path = path;
        item.size = expected.len() as u64;
        {
            let mut catalog = app.catalog.write().unwrap();
            catalog
                .by_detail
                .insert(item.detail_id, item.object_id.clone());
            catalog.items.insert(item.object_id.clone(), item.clone());
        }

        let response = app.handle(&req(&format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
            item.detail_id
        )));
        assert_eq!(response.status, 200);
        assert_eq!(response.body, expected);
    }

    fn assert_head_has_no_payload(r: &HttpResponse) {
        assert!(r.body.is_empty(), "HEAD retained an in-memory body");
        assert!(r.file_range.is_none(), "HEAD retained a file stream");
        assert!(r.remux_job.is_none(), "HEAD retained a remux stream");
        let wire = r.bytes_wire("test", "Thu, 01 Jan 1970 00:00:00 GMT");
        let split = wire
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTTP header terminator");
        assert_eq!(&wire[split + 4..], b"", "HEAD emitted wire body bytes");
    }

    #[test]
    fn head_suppresses_every_media_payload_without_changing_metadata() {
        let mut app = testdata_app();
        let movie = movie_fixture(&app);
        let id = movie.detail_id;

        let small = app.handle(&req(&format!(
            "HEAD /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n"
        )));
        assert_eq!(small.status, 200);
        assert_eq!(
            resp_header(&small, "Content-Length"),
            Some(movie.size.to_string().as_str())
        );
        assert_head_has_no_payload(&small);

        let ranged = app.handle(&req(&format!(
            "HEAD /MediaItems/{id}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-15\r\n\r\n"
        )));
        assert_eq!(ranged.status, 206);
        assert_eq!(resp_header(&ranged, "Content-Length"), Some("16"));
        assert_eq!(
            resp_header(&ranged, "Content-Range"),
            Some(format!("bytes 0-15/{}", movie.size).as_str())
        );
        assert_head_has_no_payload(&ranged);

        // A sparse file crosses the streaming threshold without allocating a
        // large test buffer. HEAD must not leave a deferred file stream.
        let big_path = app.cache_dir.join("head-large.mkv");
        std::fs::create_dir_all(&app.cache_dir).unwrap();
        let big_size = 9 * 1024 * 1024;
        std::fs::File::create(&big_path)
            .unwrap()
            .set_len(big_size)
            .unwrap();
        app.scan_cfg.media_dirs.push(app.cache_dir.clone());
        let mut big = movie.clone();
        big.detail_id = 9_000_001;
        big.object_id = "head-large-object".into();
        big.path = big_path.clone();
        big.size = big_size;
        {
            let mut cat = app.catalog.write().unwrap();
            cat.by_detail.insert(big.detail_id, big.object_id.clone());
            cat.items.insert(big.object_id.clone(), big.clone());
        }
        let large = app.handle(&req(&format!(
            "HEAD /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
            big.detail_id
        )));
        assert_eq!(large.status, 200);
        assert_eq!(
            resp_header(&large, "Content-Length"),
            Some(big_size.to_string().as_str())
        );
        assert_head_has_no_payload(&large);

        let transcode = {
            let dvp7 = app
                .catalog
                .read()
                .unwrap()
                .items
                .values()
                .find(|item| item.path.ends_with("dvp7.mkv"))
                .cloned()
                .expect("dvp7 fixture");
            app.handle(&req(&format!(
                "HEAD /Transcode/{}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: CrKey/1.54.384650 DLNADOC/1.50\r\n\r\n",
                dvp7.detail_id
            )))
        };
        assert_eq!(transcode.status, 200);
        assert_head_has_no_payload(&transcode);

        let art = movie.album_art;
        assert!(art > 0);
        let art_head = app.handle(&req(&format!(
            "HEAD /AlbumArt/{art}-{id}.jpg HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n"
        )));
        assert_eq!(art_head.status, 200);
        assert_head_has_no_payload(&art_head);

        let caption = movie.captions.first().expect("caption fixture");
        let caption_head = app.handle(&req(&format!(
            "HEAD /Captions/{id}/{}.{} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
            caption.index, caption.ext
        )));
        assert_eq!(caption_head.status, 200);
        assert_head_has_no_payload(&caption_head);

        let missing = app.handle(&req(
            "HEAD /MediaItems/999999.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\n\r\n",
        ));
        assert_eq!(missing.status, 404);
        assert_head_has_no_payload(&missing);
        let _ = std::fs::remove_file(big_path);
    }

    #[test]
    fn transcode_get_without_remap_serves_original() {
        let app = testdata_app();
        let movie = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|i| i.path.ends_with("movie.mkv"))
            .cloned()
            .expect("movie fixture");
        let id = movie.detail_id;
        let expect = std::fs::read(&movie.path).unwrap();
        let r = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n"
        )));
        assert_eq!(r.status, 200, "must not 404 a guessed remux URL");
        assert!(r.remux_job.is_none(), "SDR original is not remuxed");
        assert_eq!(r.body, expect);
    }

    #[test]
    fn crkey_dvp7_remap_first_kodi_original() {
        let app = testdata_app();
        let dvp7 = dvp7_fixture(&app);
        let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert!(kodi.contains(&format!("/MediaItems/{}.mkv", dvp7.detail_id)));
        // testdata remap is CrKey-only, so Kodi still sees the original.
        assert!(!kodi.contains(&format!("/Transcode/{}.mp4", dvp7.detail_id)));
        let (_, cr) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "CrKey/1.54.384650 DLNADOC/1.50",
        );
        assert_transcode_before_original(&cr, dvp7.detail_id);
        assert!(cr.contains("DLNA.ORG_CI=1"));
    }

    #[test]
    fn transcode_config_gates_jobs_and_controls_encoder_and_audio() {
        let mut app = testdata_app();
        let dvp7 = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|item| item.path.ends_with("dvp7.mkv"))
            .cloned()
            .expect("dvp7 fixture");
        let id = dvp7.detail_id;
        let crkey = "CrKey/1.54.384650 DLNADOC/1.50";

        app.cfg.transcode.enable = false;
        let (_, disabled_didl) = soap_browse(&app, "2$8", "BrowseDirectChildren", crkey);
        assert!(
            !disabled_didl.contains("/Transcode/"),
            "disabled transcode leaked into DIDL: {disabled_didl}"
        );
        let disabled = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
        )));
        assert_eq!(disabled.status, 200);
        assert!(disabled.remux_job.is_none());
        assert_eq!(disabled.body, std::fs::read(&dvp7.path).unwrap());

        app.cfg.transcode.enable = true;
        app.cfg.transcode.encoder = "libx264".into();
        app.remaps = rusty_dlna_transcode::parse_remaps_toml(
            r#"
[[remap]]
client = "CrKey"
hdr = "dv-p7"
action = "hdr10"
audio_out = "copy"
"#,
        )
        .unwrap();
        let global_default = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
        )));
        let global_spec = global_default.remux_job.expect("global encoder job");
        assert!(
            global_spec.args.iter().any(|arg| arg == "libx264"),
            "global encoder missing: {:?}",
            global_spec.args
        );

        app.remaps = rusty_dlna_transcode::parse_remaps_toml(
            r#"
[[remap]]
client = "CrKey"
hdr = "dv-p7"
action = "hdr10"
encoder = "hevc_nvenc"
"#,
        )
        .unwrap();
        let override_resp = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
        )));
        let override_spec = override_resp.remux_job.expect("rule encoder job");
        assert!(override_spec.args.iter().any(|arg| arg == "hevc_nvenc"));
        assert!(!override_spec.args.iter().any(|arg| arg == "libx264"));

        for (configured, expected) in [
            ("copy", RemuxAudio::Copy),
            ("to-ac3", RemuxAudio::Ac3),
            ("to-aac", RemuxAudio::Aac),
        ] {
            app.remaps = rusty_dlna_transcode::parse_remaps_toml(&format!(
                r#"
[[remap]]
client = "CrKey"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "{configured}"
"#
            ))
            .unwrap();
            let response = app.handle(&req(&format!(
                "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {crkey}\r\n\r\n"
            )));
            let spec = response.remux_job.expect("Profile-8 job");
            assert_eq!(spec.audio, expected, "audio_out={configured}");
        }
    }

    #[test]
    fn kodi_p7_remap_advertises_transcode() {
        let mut app = testdata_app();
        app.remaps = rusty_dlna_transcode::parse_remaps_toml(
            r#"
[[remap]]
name = "kodi-dvp7"
client = "Kodi"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "to-aac"
"#,
        )
        .unwrap();
        let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert_transcode_before_original(&kodi, dvp7_fixture(&app).detail_id);
        let (_, plat) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13",
        );
        assert!(
            plat.contains("/Transcode/"),
            "Platinum UA must match Kodi remap: {plat}"
        );
        assert!(kodi.contains("DLNA.ORG_CI=1"));
        let t0 = std::time::Instant::now();
        let (_, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(200),
            "Browse must not ffprobe: {:?}",
            t0.elapsed()
        );
        assert!(xml.contains("/Transcode/"), "{xml}");
    }

    fn feature_list(app: &App, ua: &str) -> String {
        let raw = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: {ua}\r\nSOAPAction: \"urn:x#X_GetFeatureList\"\r\nContent-Length: 0\r\n\r\n"
        );
        let r = app.handle(&req(&raw));
        String::from_utf8_lossy(&r.body).into_owned()
    }

    #[test]
    fn client_matrix_handlers() {
        let app = testdata_app();
        // Kodi: original MKV res, date Z or 10 chars, caption <res>
        let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert!(kodi.contains("/MediaItems/"));
        assert!(!kodi.contains("/Transcode/"));
        assert!(kodi.contains("1999-01-01") || kodi.contains("Z&lt;/dc:date"));
        assert!(
            kodi.contains("/Captions/"),
            "FLAG_CAPTION_RES Kodi Browse must list /Captions/: {kodi}"
        );

        // SEC_HHP_[PC] is not Samsung BASICVIEW
        let pc = app.handle(&req(&get(
            "/rootDesc.xml",
            "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0",
        )));
        assert!(!String::from_utf8_lossy(&pc.body).contains("sec:ProductCap"));
        let pc_fl = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0");
        assert!(pc_fl.contains("id=&quot;1&quot;"), "{pc_fl}");
        assert!(pc_fl.contains("id=&quot;2&quot;"), "{pc_fl}");
        assert!(pc_fl.contains("id=&quot;3&quot;"), "{pc_fl}");
        assert!(
            !pc_fl.contains("id=&quot;A&quot;"),
            "PC must not use A: {pc_fl}"
        );
        assert!(
            !pc_fl.contains("id=&quot;V&quot;"),
            "PC must not use V: {pc_fl}"
        );
        assert!(
            !pc_fl.contains("id=&quot;I&quot;"),
            "PC must not use I: {pc_fl}"
        );
        let (_, pc_didl) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0",
        );
        assert!(
            !pc_didl.contains("sec:CaptionInfoEx") && !pc_didl.contains("xmlns:sec"),
            "AllShare must stay non-Samsung: {pc_didl}"
        );

        // SEC_HHP_[TV] → A/V/I + x-mkv
        let tv_fl = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0");
        assert!(tv_fl.contains("id=&quot;A&quot;") && tv_fl.contains("id=&quot;V&quot;"));
        let (_, tv) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0",
        );
        assert!(tv.contains("video/x-mkv"), "{tv}");

        // [BD]J5500: FLAG_SKIP_DLNA_PN — media protocolInfo has no PN.
        // Album-art JPEG_TN <res> is still emitted (Phase 11).
        let (_, j5500) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "DLNADOC/1.50 [BD]J5500",
        );
        assert!(
            !j5500.contains("video/x-matroska:DLNA.ORG_PN=")
                && !j5500.contains("video/x-mkv:DLNA.ORG_PN="),
            "J5500 media res must skip DLNA.ORG_PN: {j5500}"
        );

        // Xbox rootDesc modelNumber=1
        let xbox = app.handle(&req(&get("/rootDesc.xml", "Xbox/360")));
        assert!(String::from_utf8_lossy(&xbox.body).contains("<modelNumber>1</modelNumber>"));

        // CrKey: transcode res first on DV P7
        let (_, cr) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "CrKey/1.54 DLNADOC/1.50",
        );
        assert_transcode_before_original(&cr, dvp7_fixture(&app).detail_id);
        assert!(cr.contains("DLNA.ORG_CI=1"));

        // Generic DLNADOC/1.50 is not NEED_SAFE_VIDEO
        let generic = identify_user_agent("DLNADOC/1.50 UPnP/1.0").unwrap();
        assert!(!generic.flags.contains(ClientFlags::NEED_SAFE_VIDEO));
        let (_, gen) = soap_browse(&app, "2$8", "BrowseDirectChildren", "DLNADOC/1.50");
        assert!(!gen.contains("/Transcode/"));
    }

    fn movie_fixture(app: &App) -> rusty_dlna_scan::MediaItem {
        let cat = app.catalog.read().unwrap();
        let any = cat
            .items
            .values()
            .find(|i| i.path.ends_with("movie.mkv"))
            .cloned()
            .expect("movie.mkv fixture");
        cat.get_item_by_detail(any.detail_id)
            .cloned()
            .unwrap_or(any)
    }

    fn dvp7_fixture(app: &App) -> rusty_dlna_scan::MediaItem {
        app.catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|item| item.path.ends_with("dvp7.mkv"))
            .cloned()
            .expect("dvp7 fixture")
    }

    fn assert_transcode_before_original(xml: &str, detail_id: i64) {
        let transcode = format!("/Transcode/{detail_id}.mp4");
        let original = format!("/MediaItems/{detail_id}.mkv");
        let transcode_pos = xml
            .find(&transcode)
            .unwrap_or_else(|| panic!("DIDL missing {transcode}: {xml}"));
        let original_pos = xml
            .find(&original)
            .unwrap_or_else(|| panic!("DIDL missing {original}: {xml}"));
        assert!(
            transcode_pos < original_pos,
            "transcode resource must precede the matching original: {xml}"
        );
    }

    fn set_detail_dlna_pn(app: &App, detail_id: i64, pn: &str) {
        let mut cat = write_recover(&app.catalog);
        let oid = cat
            .by_detail
            .get(&detail_id)
            .cloned()
            .expect("by_detail oid");
        cat.items.get_mut(&oid).expect("by_detail item").dlna_pn = Some(pn.into());
    }

    #[test]
    fn setbookmark_then_browse_position() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let (st, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!(
                "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("X_SetBookmarkResponse"), "{xml}");

        let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;")
                || xml.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"),
            "lastPlaybackPosition 120 missing: {xml}"
        );

        let dbp = app.scan_cfg.db_path.as_ref().expect("testdata db_path");
        let db = LibraryDb::open(dbp).unwrap();
        let got = db.get_bookmark(movie.detail_id).unwrap();
        assert_eq!(
            got.map(|(s, _)| s),
            Some(120),
            "BOOKMARKS.SEC for detail {}",
            movie.detail_id
        );
    }

    #[test]
    fn samsung_q_bookmark_convert_ms_bm() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let q = "SEC_HHP_[TV] Samsung Q";
        let (st, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!(
                "<ObjectID>{}</ObjectID><PosSecond>120000</PosSecond>",
                movie.object_id
            ),
            q,
        );
        assert_eq!(st, 200, "{xml}");

        let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", q);
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains(
                "&lt;upnp:lastPlaybackPosition&gt;120000&lt;/upnp:lastPlaybackPosition&gt;"
            ) || xml.contains("<upnp:lastPlaybackPosition>120000</upnp:lastPlaybackPosition>"),
            "lastPlaybackPosition 120000 missing: {xml}"
        );
        assert!(
            xml.contains("BM=120000"),
            "dcmInfo BM=120000 missing: {xml}"
        );

        let (st, kodi) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st, 200, "{kodi}");
        assert!(
            kodi.contains("&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;")
                || kodi.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"),
            "Kodi lastPlaybackPosition 120 missing: {kodi}"
        );
        assert!(
            !kodi.contains("120000"),
            "Kodi must keep stored seconds, not ms: {kodi}"
        );
    }

    #[test]
    fn kodi_soap_object_id_normalization_is_strict() {
        assert_eq!(
            normalize_soap_object_id("64%241%245%244%241/"),
            Some("64$1$5$4$1".into())
        );
        assert_eq!(normalize_soap_object_id("64%241%2f"), Some("64$1".into()));
        assert_eq!(normalize_soap_object_id("64$1$5"), Some("64$1$5".into()));
        for invalid in ["64%", "64%2", "64%GG", "64%00$1"] {
            assert_eq!(normalize_soap_object_id(invalid), None, "{invalid}");
        }
        assert_eq!(normalize_soap_object_id(&"x".repeat(1025)), None);
    }

    #[test]
    fn kodi_encoded_updateobject_persists_and_browses_resume_position() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let kodi_oid = format!("{}/", movie.object_id.replace('$', "%24"));
        let new_tags =
            "&lt;upnp:lastPlaybackPosition&gt;00:02:00&lt;/upnp:lastPlaybackPosition&gt;";
        let kodi_ua = "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13";
        let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{kodi_oid}</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>{new_tags}</NewTagValue>"
            ),
            kodi_ua,
        );
        assert_eq!(status, 200, "{xml}");
        assert!(xml.contains("UpdateObjectResponse"), "{xml}");

        let (status, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", kodi_ua);
        assert_eq!(status, 200, "{xml}");
        assert!(
            xml.contains("&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;")
                || xml.contains("<upnp:lastPlaybackPosition>120</upnp:lastPlaybackPosition>"),
            "resume position missing from Browse response: {xml}"
        );
        let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((120, 0)));
    }

    #[test]
    fn kodi_bookmark_update_invalidates_every_cached_parent_container() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let expected_parents = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .filter(|item| item.detail_id == movie.detail_id)
            .map(|item| item.parent_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!expected_parents.is_empty());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let subscribe = req(&format!(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 127.0.0.1:18200\r\n\
             Callback: <http://127.0.0.1:{}/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
            addr.port()
        ));
        assert_eq!(app.handle_from(&subscribe, addr).status, 200);
        let initial = accept_notify(&listener, std::time::Duration::from_secs(3));
        assert!(initial.contains("<ContainerUpdateIDs></ContainerUpdateIDs>"));

        let before = app.update_id.load(Ordering::Relaxed);
        let (status, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!(
                "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
                movie.object_id
            ),
            "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13",
        );
        assert_eq!(status, 200, "{xml}");
        let after = app.update_id.load(Ordering::Relaxed);
        assert_eq!(after, before.saturating_add(1));

        let notify = accept_notify(&listener, std::time::Duration::from_secs(3));
        assert!(
            notify.contains(&format!("<SystemUpdateID>{after}</SystemUpdateID>")),
            "{notify}"
        );
        for parent in expected_parents {
            assert!(
                notify.contains(&format!("{parent},{after}")),
                "parent {parent:?} missing from {notify}"
            );
        }
        let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_update_id().unwrap(), after);
    }

    #[test]
    fn bookmark_database_failure_returns_action_failed_without_catalog_drift() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let db_path = app.scan_cfg.db_path.as_ref().unwrap();
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_bookmark_write BEFORE INSERT ON BOOKMARKS
             BEGIN SELECT RAISE(FAIL, 'forced bookmark failure'); END;",
        )
        .unwrap();
        drop(conn);

        let (status, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!(
                "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>501</errorCode>"), "{xml}");
        assert_eq!(
            app.catalog
                .read()
                .unwrap()
                .get_item_by_detail(movie.detail_id)
                .unwrap()
                .bookmark_sec,
            0,
            "the in-memory catalog must not claim a write SQLite rejected"
        );
        let db = LibraryDb::open(db_path).unwrap();
        assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), None);
    }

    #[test]
    fn updateobject_playcount_and_position() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let new_tags = "&lt;upnp:playCount&gt;3&lt;/upnp:playCount&gt;&lt;upnp:lastPlaybackPosition&gt;90&lt;/upnp:lastPlaybackPosition&gt;";
        let (st, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>{new_tags}</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("UpdateObjectResponse"), "{xml}");

        let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("&lt;upnp:playbackCount&gt;3&lt;/upnp:playbackCount&gt;")
                || xml.contains("<upnp:playbackCount>3</upnp:playbackCount>"),
            "playbackCount 3 missing: {xml}"
        );
        assert!(
            xml.contains("&lt;upnp:lastPlaybackPosition&gt;90&lt;/upnp:lastPlaybackPosition&gt;")
                || xml.contains("<upnp:lastPlaybackPosition>90</upnp:lastPlaybackPosition>"),
            "lastPlaybackPosition 90 missing: {xml}"
        );

        let (st, xml) = soap_action(
            &app,
            "UpdateObject",
            "<CurrentTagValue></CurrentTagValue><NewTagValue></NewTagValue>",
            "Kodi/21.0",
        );
        assert_eq!(st, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");

        let (st, xml) = soap_action(
            &app,
            "UpdateObject",
            "<ObjectID>999999</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>&lt;upnp:playCount&gt;1&lt;/upnp:playCount&gt;</NewTagValue>",
            "Kodi/21.0",
        );
        assert_eq!(st, 500, "{xml}");
        assert!(xml.contains("<errorCode>701</errorCode>"), "{xml}");
    }

    #[test]
    fn updateobject_minus_one_clears_bookmark() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let (st, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!(
                "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{xml}");

        let new_tags = "&lt;upnp:lastPlaybackPosition&gt;-1&lt;/upnp:lastPlaybackPosition&gt;";
        let current_tags = "&lt;upnp:lastPlaybackPosition&gt;120&lt;/upnp:lastPlaybackPosition&gt;";
        let (st, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>{current_tags}</CurrentTagValue><NewTagValue>{new_tags}</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("UpdateObjectResponse"), "{xml}");

        let (st, xml) = soap_browse(&app, &movie.object_id, "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        assert!(
            !xml.contains("lastPlaybackPosition"),
            "cleared bookmark must omit lastPlaybackPosition: {xml}"
        );

        let dbp = app.scan_cfg.db_path.as_ref().expect("testdata db_path");
        let db = LibraryDb::open(dbp).unwrap();
        let got = db.get_bookmark(movie.detail_id).unwrap();
        assert_eq!(
            got.map(|(s, _)| s),
            Some(0),
            "BOOKMARKS.SEC must be 0 after lastPlaybackPosition=-1"
        );
    }

    #[test]
    fn setbookmark_missing_position_is_402_without_clearing_state() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let (status, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!(
                "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(status, 200, "{xml}");
        let update_id = app.update_id.load(Ordering::Relaxed);

        let (status, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!("<ObjectID>{}</ObjectID>", movie.object_id),
            "Kodi/21.0",
        );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
        assert_eq!(app.update_id.load(Ordering::Relaxed), update_id);
        let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((120, 0)));
    }

    #[test]
    fn updateobject_rejects_missing_malformed_and_unsupported_tag_arguments() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let oid = &movie.object_id;
        let cases = [
            (
                format!(
                    "<ObjectID>{oid}</ObjectID><NewTagValue>upnp:playCount=1</NewTagValue>"
                ),
                402,
            ),
            (
                format!(
                    "<ObjectID>{oid}</ObjectID><CurrentTagValue>upnp:playCount=0</CurrentTagValue>"
                ),
                402,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue>broken</CurrentTagValue><NewTagValue>upnp:playCount=1</NewTagValue>"),
                702,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue></CurrentTagValue><NewTagValue>broken</NewTagValue>"),
                703,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue>dc:title=Old</CurrentTagValue><NewTagValue>dc:title=New</NewTagValue>"),
                705,
            ),
            (
                format!("<ObjectID>{oid}</ObjectID><CurrentTagValue>upnp:playCount=0</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>"),
                706,
            ),
        ];
        for (body, code) in cases {
            let (status, xml) = soap_action(&app, "UpdateObject", &body, "Kodi/21.0");
            assert_eq!(status, 500, "code {code}: {xml}");
            assert!(
                xml.contains(&format!("<errorCode>{code}</errorCode>")),
                "{xml}"
            );
        }
        let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), None);
    }

    #[test]
    fn updateobject_current_value_is_an_optimistic_concurrency_guard() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let initial_update_id = app.update_id.load(Ordering::Relaxed);
        let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>upnp:lastPlaybackPosition=1</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>702</errorCode>"), "{xml}");
        assert_eq!(app.update_id.load(Ordering::Relaxed), initial_update_id);
        let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), None);
        drop(db);

        let (status, xml) = soap_action(
            &app,
            "X_SetBookmark",
            &format!(
                "<ObjectID>{}</ObjectID><PosSecond>120</PosSecond>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(status, 200, "{xml}");
        let update_id = app.update_id.load(Ordering::Relaxed);

        let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>upnp:lastPlaybackPosition=60</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>702</errorCode>"), "{xml}");
        assert_eq!(app.update_id.load(Ordering::Relaxed), update_id);
        let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((120, 0)));
        drop(db);

        let (status, xml) = soap_action(
            &app,
            "UpdateObject",
            &format!(
                "<ObjectID>{}</ObjectID><CurrentTagValue>upnp:lastPlaybackPosition=120</CurrentTagValue><NewTagValue>upnp:lastPlaybackPosition=90</NewTagValue>",
                movie.object_id
            ),
            "Kodi/21.0",
        );
        assert_eq!(status, 200, "{xml}");
        let db = LibraryDb::open(app.scan_cfg.db_path.as_ref().unwrap()).unwrap();
        assert_eq!(db.get_bookmark(movie.detail_id).unwrap(), Some((90, 0)));
    }

    /// True if a DIDL `<res>` body (escaped or raw) points at `/Captions/`.
    fn didl_res_has_captions(xml: &str) -> bool {
        for (open, close) in [("&lt;res", "&lt;/res&gt;"), ("<res", "</res>")] {
            let mut rest = xml;
            while let Some(start) = rest.find(open) {
                rest = &rest[start..];
                let Some(end) = rest.find(close) else {
                    break;
                };
                if rest[..end].contains("/Captions/") {
                    return true;
                }
                rest = &rest[end + close.len()..];
            }
        }
        false
    }

    #[test]
    fn samsung_captioninfoex_and_header() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        assert!(
            !movie.captions.is_empty(),
            "movie.mkv must have sidecar captions"
        );
        let ua = "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0";
        let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", ua);
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("sec:CaptionInfoEx"), "{xml}");
        assert!(xml.contains("xmlns:sec"), "{xml}");
        assert!(xml.contains("/Captions/"), "{xml}");
        assert!(
            xml.contains(&format!("/Captions/{}/", movie.detail_id))
                || xml.contains(&format!("/Captions/{}.srt", movie.detail_id)),
            "CaptionInfoEx URL missing for detail {}: {xml}",
            movie.detail_id
        );

        let raw = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\ngetCaptionInfo.sec: 1\r\n\r\n",
            movie.detail_id
        );
        let r = app.handle(&req(&raw));
        assert_eq!(r.status, 200, "media GET");
        let hdr = resp_header(&r, "CaptionInfo.sec").unwrap_or("");
        assert!(
            hdr.contains(&format!("/Captions/{}.srt", movie.detail_id)),
            "CaptionInfo.sec={hdr}"
        );
    }

    #[test]
    fn kodi_caption_res_no_sec_by_default() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("/Captions/"), "{xml}");
        assert!(
            xml.contains(&format!("/Captions/{}/0.srt", movie.detail_id))
                || xml.contains(&format!("/Captions/{}/1.srt", movie.detail_id)),
            "Kodi caption <res> must be indexed /Captions/{{id}}/n.srt: {xml}"
        );
        assert!(!xml.contains("sec:CaptionInfoEx"), "{xml}");
        assert!(!xml.contains("xmlns:sec"), "{xml}");
        assert!(!xml.contains("xmlns:pv"), "{xml}");
        assert!(!xml.contains("pv:subtitle"), "{xml}");
        assert!(didl_res_has_captions(&xml), "{xml}");
    }

    #[test]
    fn samsung_bdp_no_caption_res() {
        let app = testdata_app();
        let ua = "DLNADOC/1.50 SEC_HHP_BD-D5100/1.0";
        let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", ua);
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("/MediaItems/"),
            "BDP still lists media URLs: {xml}"
        );
        assert!(
            !didl_res_has_captions(&xml),
            "SEC_HHP_BD must not get caption <res> (folder-browse bug): {xml}"
        );
    }

    #[test]
    fn soap_caps_and_unknown_path_browse() {
        let app = testdata_app();
        for (action, needle) in [
            ("GetSearchCapabilities", "dc:title"),
            ("GetSortCapabilities", "dc:date"),
            ("GetProtocolInfo", "video/x-matroska"),
            ("GetSystemUpdateID", "<Id>"),
        ] {
            let raw = format!(
                "POST /not-a-real-path HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#{action}\"\r\nContent-Length: 0\r\n\r\n"
            );
            let r = app.handle(&req(&raw));
            assert_eq!(r.status, 200, "{action}");
            assert!(
                String::from_utf8_lossy(&r.body).contains(needle),
                "{action} {}",
                String::from_utf8_lossy(&r.body)
            );
        }
    }

    #[test]
    fn transcode_get_serves_growing_file_before_completion() {
        let mut app = testdata_app();
        app.remaps = rusty_dlna_transcode::parse_remaps_toml(
            r#"
[[remap]]
name = "kodi-dvp7"
client = "Kodi"
hdr = "dv-p7"
action = "remux-p8"
encoder = "copy"
audio_out = "to-aac"
"#,
        )
        .unwrap();
        let dvp7 = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|i| i.path.ends_with("dvp7.mkv"))
            .cloned()
            .expect("dvp7");
        let id = dvp7.detail_id;
        let t0 = std::time::Instant::now();
        let r = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13\r\n\r\n"
        )));
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(500),
            "GET /Transcode must not wait for a full remux ({:?})",
            t0.elapsed()
        );
        assert_eq!(r.status, 200);
        assert!(r.body.is_empty(), "body is streamed after headers");
        let spec = r.remux_job.expect("background remux job");
        assert_eq!(spec.detail_id, id);
        assert_eq!(spec.args[0], "ffmpeg");
        assert!(!spec.args.iter().any(|s| s == "pipe:1"), "{:?}", spec.args);
        assert!(
            spec.args
                .iter()
                .any(|s| s.to_string_lossy().contains("frag_keyframe")),
            "{:?}",
            spec.args
        );
        assert!(
            !spec
                .args
                .iter()
                .any(|s| s.to_string_lossy().contains("faststart")),
            "{:?}",
            spec.args
        );
        assert!(
            spec.args
                .iter()
                .any(|s| s.to_string_lossy().ends_with(".part")),
            "must write a .part file: {:?}",
            spec.args
        );
        let hdrs = r
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!hdrs.to_ascii_lowercase().contains("accept-ranges"));
        assert!(!hdrs.to_ascii_lowercase().contains("content-length"));
        assert!(hdrs.contains("DLNA.ORG_OP=00"), "{hdrs}");
        assert!(hdrs.contains("DLNA.ORG_CI=1"), "{hdrs}");
        let dest = spec.dest.clone();
        assert!(
            !dest.is_file(),
            "handle() must not wait for a finished cache"
        );
        assert!(spec.remux_p8, "dvp7 remux-p8 must attempt dovi convert");

        let kodi_orig = testdata_app();
        let miss = kodi_orig.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n"
        )));
        assert_eq!(miss.status, 200, "no remap → original, not 404");
        assert!(miss.remux_job.is_none());
    }

    #[test]
    fn remux_finished_range_and_stale_rebuild() {
        use std::io::Read;
        use std::sync::Arc;
        let app = testdata_app();
        let dvp7 = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|i| i.path.ends_with("dvp7.mkv"))
            .cloned()
            .expect("dvp7");
        let live = rusty_dlna_scan::rebase_media_path_for_config(&dvp7.path, &app.scan_cfg);
        let src = app.cache_dir.join("dvp7-stamp-src.mkv");
        let _ = std::fs::copy(&live, &src);
        let cache_key = rusty_dlna_transcode::source_identity(&src).unwrap();
        let dest = rusty_dlna_transcode::cache_dest_for_key(
            &app.cache_dir,
            dvp7.detail_id,
            rusty_dlna_transcode::RecodeAction::RemuxP8,
            &cache_key,
        );
        if let Some(p) = dest.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let payload = b"0123456789abcdefFINISHED_REMUX_BYTES";
        std::fs::write(&dest, payload).unwrap();
        rusty_dlna_transcode::write_cache_stamp_for_key(&dest, &cache_key).unwrap();
        assert!(rusty_dlna_transcode::cache_is_fresh_for_key(
            &dest, &cache_key
        ));

        let spec = RemuxJobSpec {
            detail_id: dvp7.detail_id,
            job_key: format!("{}:{cache_key}:fixture", dvp7.detail_id),
            cache_key: cache_key.clone(),
            src: src.clone(),
            dest: dest.clone(),
            args: vec!["ffmpeg".into(), "-version".into()],
            remux_p8: true,
            audio_index: 0,
            audio: RemuxAudio::Copy,
        };
        let req = HttpRequest::parse_headers(
            "GET /Transcode/1.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nRange: bytes=0-15\r\n\r\n",
        )
        .unwrap();
        let app = Arc::new(app);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (hdr, body) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app2 = app.clone();
            let spec2 = spec.clone();
            let h = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                remux::serve_remux(&app2, &mut sock, &req, spec2)
                    .await
                    .unwrap();
            });
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let mut buf = Vec::new();
            let _ = c.read_to_end(&mut buf);
            let _ = h.await;
            let text = String::from_utf8_lossy(&buf).into_owned();
            let split = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap_or(buf.len());
            let body = if split + 4 <= buf.len() {
                buf[split + 4..].to_vec()
            } else {
                Vec::new()
            };
            (text, body)
        });
        assert!(hdr.contains("206") || hdr.contains("HTTP/1.1 206"), "{hdr}");
        assert!(
            hdr.to_ascii_lowercase().contains("accept-ranges: bytes"),
            "{hdr}"
        );
        assert!(hdr.contains("DLNA.ORG_OP=01"), "{hdr}");
        assert_eq!(body, &payload[..16]);

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&src, b"replaced-source-bytes-to-bust-stamp").unwrap();
        let replaced_key = rusty_dlna_transcode::source_identity(&src).unwrap();
        assert_ne!(replaced_key, cache_key, "source identity must change");
        assert!(
            !rusty_dlna_transcode::cache_is_fresh_for_key(&dest, &replaced_key),
            "replaced source must invalidate remux dest"
        );
    }

    #[test]
    fn empty_remap_is_original_for_everyone() {
        let mut app = testdata_app();
        app.remaps.clear();
        let (_, cr) = soap_browse(&app, "2$8", "BrowseDirectChildren", "CrKey/1.54");
        assert!(!cr.contains("/Transcode/"));
    }

    fn response_content_type(r: &HttpResponse) -> &str {
        r.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    fn body_is_png(body: &[u8]) -> bool {
        body.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
    }

    fn body_is_jpeg(body: &[u8]) -> bool {
        body.len() >= 3 && body[0] == 0xff && body[1] == 0xd8 && body[2] == 0xff
    }

    fn png_dimensions(body: &[u8]) -> Option<(u32, u32)> {
        body.get(16..24).map(|dimensions| {
            (
                u32::from_be_bytes(dimensions[..4].try_into().unwrap()),
                u32::from_be_bytes(dimensions[4..].try_into().unwrap()),
            )
        })
    }

    fn jpeg_dimensions(body: &[u8]) -> Option<(u32, u32)> {
        let mut offset = 2usize;
        while offset + 8 < body.len() {
            if body[offset] != 0xff {
                offset += 1;
                continue;
            }
            let marker = body[offset + 1];
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                return Some((
                    u16::from_be_bytes([body[offset + 7], body[offset + 8]]) as u32,
                    u16::from_be_bytes([body[offset + 5], body[offset + 6]]) as u32,
                ));
            }
            let length = u16::from_be_bytes([body[offset + 2], body[offset + 3]]) as usize;
            if length < 2 {
                return None;
            }
            offset = offset.checked_add(length + 2)?;
        }
        None
    }

    #[test]
    fn icon_png_and_jpg_magic_matches_content_type() {
        let app = testdata_app();
        let png = app.handle(&req(&get("/icons/sm.png", "Kodi/21.0")));
        assert_eq!(png.status, 200);
        let png_ct = response_content_type(&png);
        assert!(
            png_ct.eq_ignore_ascii_case("image/png"),
            "png Content-Type={png_ct}"
        );
        assert!(
            body_is_png(&png.body),
            "png body missing PNG magic: {:02x?}",
            &png.body[..png.body.len().min(8)]
        );

        let jpg = app.handle(&req(&get("/icons/sm.jpg", "Kodi/21.0")));
        assert_eq!(jpg.status, 200);
        let jpg_ct = response_content_type(&jpg);
        assert!(
            jpg_ct.eq_ignore_ascii_case("image/jpeg"),
            "jpg Content-Type={jpg_ct}"
        );
        assert!(
            body_is_jpeg(&jpg.body),
            "jpg body missing JPEG SOI: {:02x?}",
            &jpg.body[..jpg.body.len().min(8)]
        );
        assert!(
            !body_is_png(&jpg.body),
            "image/jpeg response must not be PNG bytes"
        );

        assert_eq!(png_dimensions(&png.body), Some((48, 48)));
        assert_eq!(jpeg_dimensions(&jpg.body), Some((48, 48)));
        let large_png = app.handle(&req(&get("/icons/lrg.png", "Kodi/21.0")));
        let large_jpeg = app.handle(&req(&get("/icons/lrg.jpg", "Kodi/21.0")));
        assert_eq!(png_dimensions(&large_png.body), Some((120, 120)));
        assert_eq!(jpeg_dimensions(&large_jpeg.body), Some((120, 120)));
    }

    #[test]
    fn derived_image_keys_and_cache_limits_prevent_stale_unbounded_files() {
        let test_tree = TestTree::new("derived-cache");
        let cache = test_tree.path().to_path_buf();
        let key_a = derived_image_key("source-a", 160, 160, 2, 0);
        let key_b = derived_image_key("source-b", 160, 160, 2, 0);
        assert_ne!(key_a, key_b, "source replacement must change the cache key");
        assert_ne!(
            key_a,
            derived_image_key("source-a", 640, 480, 2, 0),
            "geometry is part of the cache key"
        );

        let old = cache.join(format!("{key_a}.jpg"));
        let new = cache.join(format!("{key_b}.jpg"));
        std::fs::write(&old, vec![1u8; 800]).unwrap();
        std::fs::write(&new, vec![2u8; 800]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();
        prune_derived_image_cache(&cache, 900, 36_500, 0).unwrap();
        assert!(!old.exists(), "oldest entry is evicted first");
        assert!(new.exists());
    }

    fn soap_fault_persist(out: SoapOutcome) -> bool {
        match out {
            SoapOutcome::Fault { persist, .. } => persist,
            SoapOutcome::Ok(_) => panic!("expected Fault persist bit"),
        }
    }

    #[test]
    fn soap_faults_are_500_upnperror_with_outcome_persist() {
        let app = testdata_app();

        let body = r#"<s:Envelope><s:Body><u:Browse></u:Browse></s:Body></s:Envelope>"#;
        let raw = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let r402 = app.handle(&req_from(&raw, body.as_bytes()));
        assert_eq!(r402.status, 500);
        let xml402 = String::from_utf8_lossy(&r402.body);
        assert!(xml402.contains("UPnPError"), "{xml402}");
        assert!(xml402.contains("<errorCode>402</errorCode>"), "{xml402}");
        assert_eq!(r402.persist, soap_fault_persist(SoapOutcome::fault402()));

        let r401 = app.handle(&req_from(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Nope\"\r\nContent-Length: 0\r\n\r\n",
            b"",
        ));
        assert_eq!(r401.status, 500);
        let xml401 = String::from_utf8_lossy(&r401.body);
        assert!(xml401.contains("UPnPError"), "{xml401}");
        assert!(xml401.contains("<errorCode>401</errorCode>"), "{xml401}");
        assert_eq!(r401.persist, soap_fault_persist(SoapOutcome::fault401()));

        let (st701, xml701) = soap_browse(&app, "no-such-object", "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st701, 500);
        assert!(xml701.contains("UPnPError"), "{xml701}");
        assert!(xml701.contains("<errorCode>701</errorCode>"), "{xml701}");
        let r701 = {
            let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>no-such-object</ObjectID><BrowseFlag>BrowseMetadata</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#.to_string();
            let raw = format!(
                "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
                body.len()
            );
            let mut req = HttpRequest::parse_headers(&raw).unwrap();
            req.body = body.into_bytes();
            app.handle(&req)
        };
        assert_eq!(r701.persist, soap_fault_persist(SoapOutcome::fault701()));
    }

    #[test]
    fn browse_metadata_accepts_detail_id_and_all_video_hex() {
        let app = testdata_app();
        let movie = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|i| i.title == "movie" || i.path.ends_with("movie.mkv"))
            .cloned()
            .expect("movie fixture");
        let did = movie.detail_id;
        let all_video = format!("2$8${did:X}");
        let (st, xml) = soap_browse(
            &app,
            &all_video,
            "BrowseMetadata",
            "Darwin/25.6.0, UPnP/1.0, Portable SDK for UPnP devices/1.14.13",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("/MediaItems/"), "{xml}");
        // Bare DETAILS.ID only when it cannot be a virtual container (`0`/`1`/`2`/`3`/`64`).
        if did > 64 {
            let (st2, xml2) = soap_browse(&app, &did.to_string(), "BrowseMetadata", "Kodi/21.0");
            assert_eq!(st2, 200, "{xml2}");
            assert!(xml2.contains("/MediaItems/"), "{xml2}");
        }
    }

    #[test]
    fn two_typed_media_dirs_keep_both_classes() {
        let test_tree = TestTree::new("two-media-dirs");
        let tmp = test_tree.path().to_path_buf();
        let vdir = tmp.join("video");
        let adir = tmp.join("audio");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::create_dir_all(&adir).unwrap();
        rusty_dlna_scan::write_fake_mkv(&vdir.join("clip.mkv"), 64);
        let mut flac = b"fLaC".to_vec();
        flac.extend_from_slice(&[0u8; 48]);
        std::fs::write(adir.join("song.flac"), flac).unwrap();
        let cfg = Config {
            friendly_name: "twodir".into(),
            media_dir: vec![
                format!("V,{}", vdir.display()),
                format!("A,{}", adir.display()),
            ],
            cache_dir: Some(tmp.join("cache").display().to_string()),
            rescan_secs: 0,
            ..Config::default()
        };
        let app = App::from_config(cfg, 18200, 11900, &tmp);
        assert!(
            app.scan_cfg.types.video,
            "V, prefix must survive a later A, (got {:?})",
            app.scan_cfg.types
        );
        assert!(
            app.scan_cfg.types.audio,
            "A, prefix must remain (got {:?})",
            app.scan_cfg.types
        );
        let cat = scan(&app.scan_cfg).unwrap();
        let titles: Vec<_> = cat.items.values().map(|i| i.title.as_str()).collect();
        assert!(
            titles.contains(&"clip"),
            "video under V dir must be accepted: {titles:?}"
        );
        assert!(
            titles.contains(&"song"),
            "audio under A dir must be accepted: {titles:?}"
        );
    }

    #[test]
    fn every_supported_format_agrees_across_scan_browse_and_get() {
        let test_tree = TestTree::new("formats");
        let tmp = test_tree.path().to_path_buf();
        let media = tmp.join("media");
        std::fs::create_dir_all(&media).unwrap();
        let video_fixture = tmp.join("fixture.mkv");
        rusty_dlna_scan::write_fake_mkv(&video_fixture, 64);
        let audio_fixture = tmp.join("fixture.wav");
        let mut wav = Vec::from(b"RIFF".as_slice());
        wav.extend_from_slice(&36u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8_000u32.to_le_bytes());
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&audio_fixture, wav).unwrap();

        let mut expected = Vec::new();
        for (index, format) in rusty_dlna_protocol::MEDIA_FORMATS.iter().enumerate() {
            let resolved = format.resolve(None);
            let path = media.join(format!("format-{index}.{}", format.extension));
            match resolved.kind {
                rusty_dlna_protocol::MediaKind::Video => {
                    std::fs::copy(&video_fixture, &path).unwrap();
                }
                rusty_dlna_protocol::MediaKind::Audio => {
                    std::fs::copy(&audio_fixture, &path).unwrap();
                }
                rusty_dlna_protocol::MediaKind::Image => {
                    std::fs::write(&path, TINY_JPEG).unwrap();
                }
            }
            expected.push((path, resolved));
            if format.is_ambiguous() {
                let path = media.join(format!("audio-only-{index}.{}", format.extension));
                std::fs::copy(&audio_fixture, &path).unwrap();
                expected.push((
                    path,
                    format.resolve(Some(rusty_dlna_protocol::MediaKind::Audio)),
                ));
            }
        }

        let cfg = Config {
            friendly_name: "format-map".into(),
            media_dir: vec![media.display().to_string()],
            cache_dir: Some(tmp.join("cache").display().to_string()),
            thumbnails: false,
            rescan_secs: 0,
            ..Config::default()
        };
        let app = App::from_config(cfg, 18200, 11900, &tmp);
        *app.catalog.write().unwrap() = scan(&app.scan_cfg).unwrap();
        for (path, resolved) in expected {
            let item = app
                .catalog
                .read()
                .unwrap()
                .items
                .values()
                .find(|item| item.path == path)
                .cloned()
                .unwrap_or_else(|| panic!("{} was not indexed", path.display()));
            assert_eq!(item.mime, resolved.mime, "{}", path.display());
            assert_eq!(item.class, resolved.upnp_class(), "{}", path.display());

            let response = app.handle(&req(&get(
                &format!("/MediaItems/{}.{}", item.detail_id, item.ext),
                "FormatMapTest/1.0",
            )));
            assert_eq!(response.status, 200, "GET {}", path.display());
            let get_mime = resp_header(&response, "Content-Type")
                .unwrap_or_else(|| panic!("GET {} has no Content-Type", path.display()));

            let (status, didl) =
                soap_browse(&app, &item.object_id, "BrowseMetadata", "FormatMapTest/1.0");
            assert_eq!(status, 200, "Browse {}: {didl}", path.display());
            assert!(
                didl.contains(get_mime),
                "Browse {} did not advertise GET MIME {}: {didl}",
                path.display(),
                get_mime
            );
        }
    }

    #[test]
    fn status_lists_video_count() {
        let app = testdata_app();
        let r = app.handle(&req(&get("/status", "Kodi/21.0")));
        assert_eq!(r.status, 200);
        let body = String::from_utf8_lossy(&r.body);
        assert!(body.contains("Video"), "{body}");
        let video_n = status_row_count(&body, "Video").expect("Video row");
        assert!(video_n >= 1, "Video count {video_n}: {body}");
        let audio_n = status_row_count(&body, "Audio").expect("Audio row");
        assert!(audio_n >= 1, "Audio count {audio_n}: {body}");
        let image_n = status_row_count(&body, "Image").expect("Image row");
        assert!(image_n >= 1, "Image count {image_n}: {body}");
        assert!(
            body.contains("refresh") || body.contains("Refresh") || body.contains("20"),
            "{body}"
        );
        assert!(
            !body.contains("<H1>200 OK</H1>"),
            "status must be the document, not the error-page wrapper: {body}"
        );
        assert!(
            body.contains("<h1>rustyDLNA-test</h1>")
                || body.contains("<title>rustyDLNA-test</title>"),
            "{body}"
        );
        let root = app.handle(&req(&get("/", "Kodi/21.0")));
        assert_eq!(root.status, 200);
        assert_eq!(root.body, r.body);
    }

    #[test]
    fn machine_status_is_structured_and_does_not_expose_absolute_paths() {
        let app = testdata_app();
        let response = app.handle(&req(&get("/api/status", "Operator/1.0")));
        assert_eq!(response.status, 200);
        assert_eq!(
            resp_header(&response, "Content-Type"),
            Some("application/json")
        );
        let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert!(matches!(
            value["status"].as_str(),
            Some("healthy" | "degraded")
        ));
        assert!(value["database"]["quick_check"].is_string());
        assert!(value["transcode"]["cache_bytes"].is_u64());
        assert!(value["transcode"]["required_tools_ready"].is_boolean());
        assert!(value["scanner"]["files_seen"].is_u64());
        assert!(value["catalog"]["estimated_memory_bytes"].is_u64());
        assert!(value["catalog"]["projected_memory_bytes"]["objects_10000"].is_u64());
        assert!(value["catalog"]["projected_memory_bytes"]["objects_100000"].is_u64());
        assert!(value["catalog"]["projected_memory_bytes"]["objects_1000000"].is_u64());
        let body = String::from_utf8(response.body).unwrap();
        assert!(
            !body.contains("/home/"),
            "status leaked an absolute path: {body}"
        );

        let health = app.handle(&req(&get("/health", "Operator/1.0")));
        assert!(matches!(health.status, 200 | 503));
        serde_json::from_slice::<serde_json::Value>(&health.body).unwrap();
    }

    #[test]
    fn scan_workers_are_owned_report_progress_and_join_on_shutdown() {
        let test_tree = TestTree::new("scan-shutdown");
        let tmp = test_tree.path();
        let media = tmp.join("video");
        std::fs::create_dir_all(&media).unwrap();
        rusty_dlna_scan::write_fake_mkv(&media.join("movie.mkv"), 64);
        let app = Arc::new(App::from_config(
            Config {
                friendly_name: "scan-shutdown".into(),
                media_dir: vec![media.display().to_string()],
                cache_dir: Some(tmp.join("cache").display().to_string()),
                rescan_secs: 1,
                ..Config::default()
            },
            18200,
            11900,
            tmp,
        ));
        spawn_library_watch(app.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        stop_library_watch(&app);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(app.scan_control.stopping.load(Ordering::Acquire));
        assert!(app.scan_control.threads.lock().unwrap().is_empty());
        let (seen, current) = app.scan_cfg.progress.as_ref().unwrap().snapshot();
        assert!(seen >= 1);
        assert!(current.is_some());
    }

    #[test]
    fn search_samsung_v_finds_fixture() {
        let app = testdata_app();
        let tv = "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0";
        let (st, xml) = soap_action(
            &app,
            "Search",
            r#"<ContainerID>V</ContainerID><SearchCriteria>dc:title contains "Fixture"</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            tv,
        );
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("Fixture Movie"),
            "Search ContainerID=V must find Fixture Movie: {xml}"
        );
        assert!(
            xml.contains("/MediaItems/"),
            "Search ContainerID=V must list a media URL: {xml}"
        );
        let (st_img, img) = soap_action(
            &app,
            "Search",
            r#"<ContainerID>I</ContainerID><SearchCriteria>dc:title contains "Fixture"</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            tv,
        );
        assert_eq!(st_img, 200, "{img}");
        assert!(
            !img.contains("Fixture Movie"),
            "Search ContainerID=I must not return the video title: {img}"
        );
    }

    #[test]
    fn search_xbox_exists_false_skips_refid_aliases() {
        let app = testdata_app();
        let xbox = "Xbox/360";
        let (st, xml) = soap_action(
            &app,
            "Search",
            r#"<ContainerID>0</ContainerID><SearchCriteria>upnp:class derivedfrom "object.item.videoItem" and @refID exists false</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            xbox,
        );
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("Fixture Movie"),
            "Xbox exists false must still find the original: {xml}"
        );
        assert!(
            !xml.contains("refID="),
            "Xbox exists false must omit alias rows: {xml}"
        );
    }

    #[test]
    fn search_or_matches_either_class() {
        let app = testdata_app();
        let (st, xml) = soap_action(
            &app,
            "Search",
            r#"<ContainerID>0</ContainerID><SearchCriteria>(upnp:class derivedfrom "object.item.audioItem") or (upnp:class derivedfrom "object.item.videoItem")</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            "FormatMapTest/1.0",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("Fixture Movie"), "or must hit video: {xml}");
        assert!(xml.contains("Fixture Track"), "or must hit audio: {xml}");
    }

    #[test]
    fn malformed_soap_arguments_and_search_criteria_return_standard_faults() {
        let app = testdata_app();

        for criteria in [
            r#"unknown:field contains "Fixture""#,
            r#"dc:title approximately "Fixture""#,
            r#"(dc:title contains "Fixture""#,
        ] {
            let (status, xml) = soap_action(
                &app,
                "Search",
                &format!(
                    "<ContainerID>0</ContainerID><SearchCriteria>{criteria}</SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"
                ),
                "SearchFaultTest/1.0",
            );
            assert_eq!(status, 500, "{xml}");
            assert!(xml.contains("<errorCode>708</errorCode>"), "{xml}");
        }

        let (status, invalid_integer) = soap_action(
            &app,
            "Browse",
            r#"<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>not-a-number</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            "SoapFaultTest/1.0",
        );
        assert_eq!(status, 500, "{invalid_integer}");
        assert!(
            invalid_integer.contains("<errorCode>402</errorCode>"),
            "{invalid_integer}"
        );

        let body = br#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>0</ObjectID></s:Body></s:Envelope>"#;
        let raw = format!(
            "POST /ctl/ContentDir HTTP/1.1\r\nHost: 127.0.0.1\r\nSOAPAction: \"urn:x#Browse\"\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut request = HttpRequest::parse_headers(&raw).unwrap();
        request.body = body.to_vec();
        let response = app.handle(&request);
        let xml = String::from_utf8_lossy(&response.body);
        assert_eq!(response.status, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
    }

    #[test]
    fn browse_listed_filter_omits_res_size() {
        let app = testdata_app();
        let (st, xml) = soap_action(
            &app,
            "Browse",
            r#"<ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>dc:title,upnp:class</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("Fixture Movie"), "{xml}");
        assert!(
            !xml.contains(" size=&quot;") && !xml.contains(" size=\""),
            "Filter without res@size: {xml}"
        );
        assert!(
            !xml.contains("&lt;res ") && !xml.contains("<res "),
            "Filter without res: {xml}"
        );
        let (st, star) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(
            star.contains("size=&quot;") || star.contains("size=\""),
            "{star}"
        );
    }

    #[test]
    fn browse_recent_keeps_mtime_order() {
        let app = testdata_app();
        {
            let mut cat = write_recover(&app.catalog);
            let mut videos: Vec<String> = cat
                .items
                .values()
                .filter(|i| {
                    i.class.contains("video")
                        && i.ref_id.is_none()
                        && i.object_id
                            .starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
                })
                .map(|i| i.object_id.clone())
                .collect();
            videos.sort();
            assert!(
                videos.len() >= 2,
                "need two browse-folder videos, got {videos:?}"
            );
            let older = videos[0].clone();
            let newer = videos[1].clone();
            if let Some(it) = cat.items.get_mut(&older) {
                it.title = "Aaa Early".into();
                it.mtime = 1;
            }
            if let Some(it) = cat.items.get_mut(&newer) {
                it.title = "Zzz Fresh".into();
                it.mtime = 9_999_999;
            }
            cat.recent_ids = vec![newer, older];
            cat.recent_count = 2;
        }
        let (st, xml) = soap_browse(&app, "2$FF0", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        let fresh = xml.find("Zzz Fresh").expect("fresh title in recent DIDL");
        let early = xml.find("Aaa Early").expect("old title in recent DIDL");
        assert!(
            fresh < early,
            "Recently Added must stay mtime-desc, not title: {xml}"
        );

        let (st, titled) = soap_action(
            &app,
            "Browse",
            "<ObjectID>2$FF0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+dc:title</SortCriteria>",
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{titled}");
        let early = titled.find("Aaa Early").expect("old title after +dc:title");
        let fresh = titled
            .find("Zzz Fresh")
            .expect("fresh title after +dc:title");
        assert!(
            early < fresh,
            "explicit +dc:title must still re-sort Recent: {titled}"
        );
    }

    #[test]
    fn browse_force_sort_track_order() {
        let mut app = testdata_app();
        app.scan_cfg.db_path = None;
        {
            let mut cat = write_recover(&app.catalog);
            let mut videos: Vec<String> = cat
                .items
                .values()
                .filter(|i| {
                    i.class.contains("video")
                        && i.ref_id.is_none()
                        && i.object_id
                            .starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
                })
                .map(|i| i.object_id.clone())
                .collect();
            videos.sort();
            assert!(
                videos.len() >= 2,
                "need two browse-folder videos, got {videos:?}"
            );
            let high_oid = videos[0].clone();
            let low_oid = videos[1].clone();
            let high_detail = cat.items.get(&high_oid).unwrap().detail_id;
            let low_detail = cat.items.get(&low_oid).unwrap().detail_id;
            for it in cat.items.values_mut() {
                if it.detail_id == high_detail {
                    it.title = "Aaa HighTrack".into();
                    it.disc = Some(2);
                    it.track = Some(5);
                } else if it.detail_id == low_detail {
                    it.title = "Zzz LowTrack".into();
                    it.disc = Some(1);
                    it.track = Some(1);
                }
            }
        }
        let (st, xml) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "Panasonic DLNADOC/1.50",
        );
        assert_eq!(st, 200, "{xml}");
        let low = xml.find("Zzz LowTrack").expect("Zzz LowTrack in DIDL");
        let high = xml.find("Aaa HighTrack").expect("Aaa HighTrack in DIDL");
        assert!(
            low < high,
            "FORCE_SORT disc/track must put Zzz LowTrack before Aaa HighTrack: {xml}"
        );
        let (st709, body709) = soap_action(
            &app,
            "Browse",
            "<ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria>+notAField</SortCriteria>",
            "DLNADOC/1.50",
        );
        assert_eq!(st709, 500, "{body709}");
        assert!(body709.contains("709"), "{body709}");
    }

    #[test]
    fn cache_keeps_kodi_when_generic_ua_follows() {
        let app = testdata_app();
        let peer: SocketAddr = "192.0.2.40:1234".parse().unwrap();
        let (st, kodi) = {
            let body = r#"<ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#;
            let envelope = format!(
                r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">{body}</u:Browse></s:Body></s:Envelope>"#
            );
            let raw = format!(
                "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
                envelope.len()
            );
            let mut req = HttpRequest::parse_headers(&raw).unwrap();
            req.body = envelope.into_bytes();
            let r = app.handle_from(&req, peer);
            (r.status, String::from_utf8_lossy(&r.body).into_owned())
        };
        assert_eq!(st, 200);
        assert!(kodi.contains("/Captions/"), "{kodi}");
        assert!(!kodi.contains("/Transcode/"));

        let movie = movie_fixture(&app);
        let gen = app.handle_from(
            &req(&get(
                &format!("/MediaItems/{}.mkv", movie.detail_id),
                "DLNADOC/1.50",
            )),
            peer,
        );
        assert_eq!(gen.status, 200);

        let cr_peer: SocketAddr = "192.0.2.41:1234".parse().unwrap();
        let envelope = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#.to_string();
        let raw = format!(
            "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: CrKey/1.54.384650 DLNADOC/1.50\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
            envelope.len()
        );
        let mut creq = HttpRequest::parse_headers(&raw).unwrap();
        creq.body = envelope.into_bytes();
        let cr = app.handle_from(&creq, cr_peer);
        let cr_xml = String::from_utf8_lossy(&cr.body);
        assert!(cr_xml.contains("/Transcode/"), "{cr_xml}");
        let tid = cr_xml
            .split("/Transcode/")
            .nth(1)
            .and_then(|s| {
                s.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<i64>()
                    .ok()
            })
            .expect("transcode id");
        let tget = app.handle_from(
            &req(&get(&format!("/Transcode/{tid}.mp4"), "DLNADOC/1.50")),
            cr_peer,
        );
        assert_ne!(
            tget.status, 404,
            "cached CrKey must still remap Transcode GET"
        );
    }

    #[test]
    fn cache_expires_after_one_hour() {
        let app = testdata_app();
        let peer: SocketAddr = "192.0.2.42:9".parse().unwrap();
        let ip: Ipv4Addr = "192.0.2.42".parse().unwrap();
        let _ = app.handle_from(&req(&get("/rootDesc.xml", "Kodi/21.0")), peer);
        {
            let mut cache = app.client_cache.lock().unwrap();
            cache.set_age(ip, 0);
        }
        assert!(
            app.client_cache
                .lock()
                .unwrap()
                .search(ip, 3601, None)
                .is_none(),
            "expired without MAC"
        );
        let _ = app.handle_from(&req(&get("/rootDesc.xml", "Kodi/21.0")), peer);
        {
            let mut cache = app.client_cache.lock().unwrap();
            cache.set_age(ip, 0);
        }
        assert!(
            app.client_cache
                .lock()
                .unwrap()
                .search(ip, 3601, Some([1, 2, 3, 4, 5, 6]))
                .is_none(),
            "HTTP stores no MAC"
        );
        {
            let kodi = identify_user_agent("Kodi/21.0").expect("kodi");
            let mut cache = app.client_cache.lock().unwrap();
            cache.remember(ip, kodi, Some([1, 2, 3, 4, 5, 6]), 0);
            assert_eq!(
                cache
                    .search(ip, 3601, Some([1, 2, 3, 4, 5, 6]))
                    .map(|p| p.kind),
                Some(ClientKind::Kodi)
            );
        }
    }

    #[test]
    fn pfs_xbox_browse_eight_is_video_all() {
        let app = testdata_app();
        let (st8, eight) = soap_browse(&app, "8", "BrowseDirectChildren", "Xbox/360");
        let (st, all) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Xbox/360");
        assert_eq!(st8, 200, "{eight}");
        assert_eq!(st, 200, "{all}");
        assert!(
            eight.contains("/MediaItems/"),
            "Xbox Browse 8 must remap to Video All: {eight}"
        );
        assert!(
            eight.contains("NumberReturned") && all.contains("NumberReturned"),
            "both pages return items"
        );
        let n8 = xml_tag_text(&eight, "NumberReturned").unwrap_or_default();
        let nall = xml_tag_text(&all, "NumberReturned").unwrap_or_default();
        assert_eq!(n8, nall, "8 and 2$8 must return the same page");
    }

    #[test]
    fn feature_list_dcm10_and_root_container_collapse() {
        let mut app = testdata_app();
        let tv = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0");
        assert!(tv.contains("id=&quot;A&quot;"), "{tv}");
        assert!(tv.contains("id=&quot;V&quot;"), "{tv}");
        assert!(tv.contains("id=&quot;I&quot;"), "{tv}");
        app.cfg.root_container = Some("V".into());
        let collapsed = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0");
        let video_hits = collapsed.matches("id=&quot;2&quot;").count()
            + collapsed.matches("id=&quot;V&quot;").count();
        assert!(
            video_hits >= 3,
            "non-64 root_container collapses FeatureList: {collapsed}"
        );
        app.cfg.root_container = Some("64".into());
        let folders = feature_list(&app, "Kodi/21.0");
        assert!(folders.contains("id=&quot;1$14&quot;"), "{folders}");
        assert!(folders.contains("id=&quot;2$15&quot;"), "{folders}");
        assert!(folders.contains("id=&quot;3$16&quot;"), "{folders}");
    }

    #[test]
    fn setbookmark_missing_object_is_701() {
        let app = testdata_app();
        let (st, xml) = soap_action(
            &app,
            "X_SetBookmark",
            "<ObjectID>no-such-object</ObjectID><PosSecond>90</PosSecond>",
            "Kodi/21.0",
        );
        assert_eq!(st, 500, "{xml}");
        assert!(xml.contains("<errorCode>701</errorCode>"), "{xml}");
    }

    #[test]
    fn search_negative_starting_index_is_402() {
        let app = testdata_app();
        let (st, xml) = soap_action(
            &app,
            "Search",
            r#"<ContainerID>0</ContainerID><SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>-1</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            "Kodi/21.0",
        );
        assert_eq!(st, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
    }

    #[test]
    fn search_missing_container_id_is_402() {
        let app = testdata_app();
        let (status, xml) = soap_action(
            &app,
            "Search",
            r#"<SearchCriteria></SearchCriteria><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria>"#,
            "Kodi/21.0",
        );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
    }

    #[test]
    fn connection_and_registrar_required_arguments_reach_http_faults() {
        let app = testdata_app();
        for body in ["", "<ConnectionID>invalid</ConnectionID>"] {
            let (status, xml) = soap_action(&app, "GetCurrentConnectionInfo", body, "Kodi/21.0");
            assert_eq!(status, 500, "{xml}");
            assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
        }
        let (status, xml) = soap_action(
            &app,
            "GetCurrentConnectionInfo",
            "<ConnectionID>1</ConnectionID>",
            "Kodi/21.0",
        );
        assert_eq!(status, 500, "{xml}");
        assert!(xml.contains("<errorCode>701</errorCode>"), "{xml}");
        let (status, xml) = soap_action(
            &app,
            "GetCurrentConnectionInfo",
            "<ConnectionID>0</ConnectionID>",
            "Kodi/21.0",
        );
        assert_eq!(status, 200, "{xml}");

        for method in ["IsAuthorized", "IsValidated"] {
            let (status, xml) = soap_action(&app, method, "", "Kodi/21.0");
            assert_eq!(status, 500, "{xml}");
            assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
            let (status, xml) = soap_action(
                &app,
                method,
                "<DeviceID>uuid:client</DeviceID>",
                "Kodi/21.0",
            );
            assert_eq!(status, 200, "{xml}");
        }
    }

    #[test]
    fn query_state_variable_connection_status_missing_unknown() {
        let app = testdata_app();
        let (st, xml) = soap_action(
            &app,
            "QueryStateVariable",
            "<varName>ConnectionStatus</varName>",
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("<return>Connected</return>"), "{xml}");
        let (st, xml) = soap_action(&app, "QueryStateVariable", "", "Kodi/21.0");
        assert_eq!(st, 500, "{xml}");
        assert!(xml.contains("<errorCode>402</errorCode>"), "{xml}");
        let (st, xml) = soap_action(
            &app,
            "QueryStateVariable",
            "<varName>NotAVariable</varName>",
            "Kodi/21.0",
        );
        assert_eq!(st, 500, "{xml}");
        assert!(xml.contains("<errorCode>404</errorCode>"), "{xml}");
    }

    #[test]
    fn browse_metadata_root_includes_search_class() {
        let app = testdata_app();
        let (st, meta) = soap_browse(&app, "0", "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st, 200, "{meta}");
        assert!(
            meta.contains("searchClass") && meta.contains("includeDerived"),
            "{meta}"
        );
        assert!(meta.contains("object.item.audioItem"), "{meta}");
        assert!(meta.contains("object.item.imageItem"), "{meta}");
        assert!(meta.contains("object.item.videoItem"), "{meta}");
    }

    #[test]
    fn didl_alias_items_have_refid() {
        let app = testdata_app();
        let alias = {
            let cat = app.catalog.read().unwrap();
            cat.items
                .values()
                .find(|i| i.ref_id.as_ref().is_some_and(|r| !r.is_empty()))
                .cloned()
                .expect("virtual alias with REF_ID")
        };
        let (st, xml) = soap_browse(&app, &alias.object_id, "BrowseMetadata", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        let rid = alias.ref_id.as_deref().unwrap();
        assert!(
            xml.contains(&format!("refID=\"{rid}\""))
                || xml.contains(&format!("refID=&quot;{rid}&quot;")),
            "alias {} missing refID={rid}: {xml}",
            alias.object_id
        );
    }

    #[test]
    fn music_and_image_virtuals_browse_ok() {
        let app = testdata_app();
        for oid in ["1", "3", "1$4", "3$B", "1$FF0", "3$FF0"] {
            let (st, xml) = soap_browse(&app, oid, "BrowseDirectChildren", "Kodi/21.0");
            assert_eq!(st, 200, "{oid} {xml}");
            assert!(xml.contains("BrowseResponse"), "{oid} {xml}");
            assert!(
                !xml.contains("<errorCode>"),
                "virtual {oid} must not fault: {xml}"
            );
        }
        let (st, music) = soap_browse(&app, "1", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{music}");
        assert!(
            music.contains("id=\"1$4\"")
                || music.contains("All Music")
                || music.contains("Recently Added"),
            "{music}"
        );
        let (st, pics) = soap_browse(&app, "3", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{pics}");
        assert!(
            pics.contains("id=\"3$B\"")
                || pics.contains("All Pictures")
                || pics.contains("Recently Added"),
            "{pics}"
        );
        let (st, tracks) = soap_browse(&app, "1$4", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{tracks}");
        assert!(
            tracks.contains("Fixture Track"),
            "All Music must list the FLAC fixture: {tracks}"
        );
    }

    #[test]
    fn checked_fixtures_match_minidlna_scan_didl_and_get_contract() {
        let app = testdata_app();
        let expected = [
            (
                "music/song.flac",
                "audio/x-flac",
                "item.audioItem.musicTrack",
                "Fixture Track",
            ),
            (
                "music/song.mp3",
                "audio/mpeg",
                "item.audioItem.musicTrack",
                "Fixture Track",
            ),
            ("video/tagged.mp4", "video/mp4", "item.videoItem", "tagged"),
            (
                "pictures/shot.jpg",
                "image/jpeg",
                "item.imageItem.photo",
                "Fixture Photo",
            ),
        ];

        for (relative, mime, class, title) in expected {
            let item = {
                let catalog = app.catalog.read().unwrap();
                catalog
                    .items
                    .values()
                    .find(|item| item.ref_id.is_none() && item.path.ends_with(relative))
                    .cloned()
                    .unwrap_or_else(|| panic!("checked fixture was not indexed: {relative}"))
            };
            assert_eq!(item.mime, mime, "{relative}");
            assert_eq!(item.class, class, "{relative}");
            assert_eq!(item.title, title, "{relative}");

            let (status, didl) = soap_browse(&app, &item.object_id, "BrowseMetadata", "Kodi/21.0");
            assert_eq!(status, 200, "{relative}: {didl}");
            assert!(didl.contains(class), "{relative}: {didl}");
            assert!(
                didl.contains(&format!("http-get:*:{mime}:")),
                "{relative}: {didl}"
            );

            let response = app.handle(&req(&get(
                &format!("/MediaItems/{}.{}", item.detail_id, item.ext),
                "FixtureParity/1.0",
            )));
            assert_eq!(response.status, 200, "GET {relative}");
            assert_eq!(resp_header(&response, "Content-Type"), Some(mime));
            let expected_length = item.size.to_string();
            assert_eq!(
                resp_header(&response, "Content-Length"),
                Some(expected_length.as_str())
            );
            if response.file_range.is_none() {
                assert_eq!(response.body, std::fs::read(&item.path).unwrap());
            } else {
                assert_eq!(
                    response
                        .file_range
                        .as_ref()
                        .map(|(path, _, _)| path.as_path()),
                    Some(item.path.as_path())
                );
            }
        }

        let song = app
            .catalog
            .read()
            .unwrap()
            .items
            .values()
            .find(|item| item.ref_id.is_none() && item.path.ends_with("music/song.flac"))
            .cloned()
            .unwrap();
        // Sidecar `studio` intentionally overrides embedded `artist`; the
        // remaining embedded fields survive the NFO merge.
        assert_eq!(song.artist.as_deref(), Some("Fixture Band"));
        assert_eq!(song.album_artist.as_deref(), Some("Fixture Album Artist"));
        assert_eq!(song.album.as_deref(), Some("Fixture Album"));
        assert_eq!(song.composer.as_deref(), Some("Fixture Composer"));
        assert_eq!(song.track, Some(2));
        assert_eq!(song.disc, Some(1));
    }

    #[test]
    fn recent_keeps_old_mtime_and_caps_at_200() {
        let app = testdata_app();
        let movie = {
            let cat = app.catalog.read().unwrap();
            cat.items
                .values()
                .find(|i| {
                    i.path.ends_with("movie.mkv")
                        && i.ref_id.is_none()
                        && i.object_id
                            .starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
                })
                .cloned()
                .expect("browse-folder movie.mkv")
        };
        {
            let mut cat = write_recover(&app.catalog);
            if let Some(it) = cat.items.get_mut(&movie.object_id) {
                it.mtime = 1;
                it.ref_id = None;
            }
            cat.recent_ids.clear();
            cat.recent_count = 0;
        }
        let (st, xml) = soap_browse(&app, "2$FF0", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("Fixture Movie") || xml.contains(&movie.title),
            "old-mtime files stay in Recent (no 90-day window): {xml}"
        );
        {
            let mut cat = write_recover(&app.catalog);
            for i in 0..210i64 {
                let oid = format!("64$RECENTTEST${i:X}");
                let mut clone = movie.clone();
                clone.object_id = oid.clone();
                clone.parent_id = "64".into();
                clone.ref_id = None;
                clone.detail_id = 50_000 + i;
                clone.title = format!("cap{i:03}");
                clone.mtime = 2_000_000_000 + i;
                clone.inode = 80_000 + i as u64;
                clone.device = 9;
                cat.items.insert(oid, clone);
            }
            cat.recent_ids.clear();
            cat.recent_count = 0;
        }
        let (st, xml) = soap_browse(&app, "2$FF0", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200, "{xml}");
        let returned: u32 = xml_tag_text(&xml, "NumberReturned")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let total: u32 = xml_tag_text(&xml, "TotalMatches")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        assert!(
            returned <= 200 && total <= 200,
            "RECENT_MAX is 200 unique items: returned={returned} total={total}"
        );
        assert_eq!(total, 200, "cap is exactly 200 with 210+ videos: {xml}");
    }

    #[test]
    fn getcontentfeatures_not_one_is_400() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let bad = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\ngetcontentFeatures.dlna.org: 0\r\n\r\n",
            movie.detail_id
        );
        assert_eq!(app.handle(&req(&bad)).status, 400);
        let ok = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\ngetcontentFeatures.dlna.org: 1\r\n\r\n",
            movie.detail_id
        );
        assert_eq!(app.handle(&req(&ok)).status, 200);
    }

    #[test]
    fn interactive_on_non_image_is_406_except_samsung() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let kodi = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\ntransferMode.dlna.org: Interactive\r\n\r\n",
            movie.detail_id
        );
        assert_eq!(app.handle(&req(&kodi)).status, 406);
        let samsung = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0\r\ntransferMode.dlna.org: Interactive\r\n\r\n",
            movie.detail_id
        );
        assert_eq!(app.handle(&req(&samsung)).status, 200);
        let strict = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: DLNADOC/1.50 SEC_HHP_[TV]UE40D7000/1.0\r\ntransferMode.dlna.org: Interactive\r\nuctt.upnp.org: 1\r\n\r\n",
            movie.detail_id
        );
        assert_eq!(app.handle(&req(&strict)).status, 406);
    }

    #[test]
    fn skip_dlna_pn_omits_pn_on_http_content_features() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        set_detail_dlna_pn(&app, movie.detail_id, "AVC_MP4_MP_SD_AC3");
        let kodi = app.handle(&req(&format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n",
            movie.detail_id
        )));
        assert_eq!(kodi.status, 200);
        let kfeats = resp_header(&kodi, "contentFeatures.dlna.org").unwrap_or("");
        assert!(
            kfeats.contains("DLNA.ORG_PN=AVC_MP4_MP_SD_AC3"),
            "Kodi keeps PN on the by_detail item: {kfeats}"
        );
        let j5500 = app.handle(&req(&format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: DLNADOC/1.50 [BD]J5500\r\n\r\n",
            movie.detail_id
        )));
        assert_eq!(j5500.status, 200);
        let feats = resp_header(&j5500, "contentFeatures.dlna.org").unwrap_or("");
        assert!(
            !feats.contains("DLNA.ORG_PN="),
            "J5500 SKIP_DLNA_PN must omit PN on contentFeatures: {feats}"
        );
        assert!(feats.contains("DLNA.ORG_OP=01"), "{feats}");
    }

    #[test]
    fn lg_browse_captioned_title_ends_with_dot() {
        let app = testdata_app();
        let (st, xml) = soap_browse(&app, "2$8", "BrowseDirectChildren", "LGE_DLNA_SDK/1.6.0");
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("Fixture Movie."),
            "LG caption hack appends '.': {xml}"
        );
    }

    #[test]
    fn toshiba_browse_extra_ci1_res() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        set_detail_dlna_pn(&app, movie.detail_id, "MPEG_TS_HD_NA");
        let oid = {
            let cat = app.catalog.read().unwrap();
            cat.by_detail
                .get(&movie.detail_id)
                .cloned()
                .expect("by_detail oid")
        };
        let (st, xml) = soap_browse(
            &app,
            &oid,
            "BrowseMetadata",
            "UPnP/1.0 DLNADOC/1.50 Intel_SDK_for_UPnP_devices/1.2",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(
            xml.contains("DLNA.ORG_PN=MPEG_PS_NTSC") && xml.contains("DLNA.ORG_CI=1"),
            "Toshiba extra CI=1 res: {xml}"
        );
    }

    #[test]
    fn sony_bdp_get_remaps_mkv_to_divx() {
        let app = testdata_app();
        let movie = movie_fixture(&app);
        let raw = format!(
            "GET /MediaItems/{}.mkv HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: UPnP/1.0 DLNADOC/1.50\r\nX-AV-Client-Info: av=\"5.0\"; cn=\"Sony Corporation\"; mv=\"2.0\"\r\n\r\n",
            movie.detail_id
        );
        let r = app.handle(&req(&raw));
        assert_eq!(r.status, 200);
        assert_eq!(resp_header(&r, "Content-Type"), Some("video/divx"));

        let (st, xml) = {
            let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#.to_string();
            let hdr = format!(
                "POST /whatever HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nX-AV-Client-Info: av=\"5.0\"; mv=\"2.0\"\r\nSOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"\r\nContent-Length: {}\r\nContent-Type: text/xml\r\n\r\n",
                body.len()
            );
            let mut req = HttpRequest::parse_headers(&hdr).unwrap();
            req.body = body.into_bytes();
            let r = app.handle(&req);
            (r.status, String::from_utf8_lossy(&r.body).into_owned())
        };
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("video/divx"), "primary remapped mime: {xml}");
        assert!(
            xml.contains("MPEG_PS_NTSC") && xml.contains("DLNA.ORG_CI=1"),
            "extra CI=1 still uses original mkv mime: {xml}"
        );
    }
}
