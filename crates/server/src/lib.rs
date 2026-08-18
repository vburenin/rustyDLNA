//! Accept loop, SOAP Browse, original GET/`Range`, and background remux.
//! Listen ports come from `RUSTY_DLNA_HTTP_PORT` / `RUSTY_DLNA_SSDP_PORT`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rusty_dlna_http::{
    caption_info_sec_url, dlna_get_header_invalid, dlna_org_features, dlna_strict,
    gen_root_desc, interactive_on_non_image, is_chunked, live_transcode_response, media_response,
    now_imf_date, parse_byte_range, persist_for_route, protocol_info, read_file_range,
    realtime_interactive_invalid, route, scpd_connection_manager, scpd_content_directory,
    scpd_registrar, set_caption_info_sec, streaming_on_image, timeseek_without_range,
    valid_host_header, wants_caption_info_sec, wants_content_language, ByteRange, HttpRequest,
    HttpResponse, HttpRoute, RangeError, RemuxJobSpec, RootDescOpts,
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
    caption_http_mime, collect_media_dirs, load_existing, monitor, repair_objects_if_needed,
    run_inotify, scan, Catalog, CatalogChild, LibraryDb, MediaItem, ScanConfig, ScanDelta,
    SourceProbe,
};

pub use rusty_dlna_scan::{ensure_pattern_fixture, ensure_show_fixture};
use rusty_dlna_soap::{
    apply_title_hack, bookmark_seconds, build_browse, default_order, dispatch_simple,
    empty_cd_response, extra_ci1_protocol_infos, magic_object_id, parse_filter,
    parse_search_criteria, parse_soap_call, parse_update_object_tags, row_matches, soap_fault,
    sort_or_709, DefaultOrder, DidlCaption, DidlObject, DidlRes, FilterBits, SearchRow, SoapCall,
    SoapOutcome, SortKey, SortSpec,
};
use rusty_dlna_ssdp::{
    jitter_ms, msearch_jitter_ms_range, msearch_replies, notify_byebye, parse_inbound_notify,
    parse_msearch, ALIVE_DUP_DELAY_MS,
};
use rusty_dlna_transcode::{
    cache_dest, cache_part, decide_for, ffmpeg_grow_args, hdr10_fallback_plan,
    pick_audio_index, probe_to_source, Decision, JobGate, RecodeAction,
    RemapRule,
};

mod events;
mod remux;
mod status;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_name")]
    pub friendly_name: String,
    #[serde(default)]
    pub network_interface: Vec<String>,
    #[serde(default)]
    pub media_dir: Vec<String>,
    #[serde(default)]
    pub exclude_dir: Vec<String>,
    #[serde(default)]
    pub exclude_file: Vec<String>,
    #[serde(default)]
    pub transcode: TranscodeCfg,
    #[serde(default)]
    pub remap: Vec<RemapRule>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default)]
    pub advertise_ip: Option<String>,
    /// Bind address. Default `0.0.0.0`. Set to a LAN IP (e.g. `192.0.2.20`).
    #[serde(default)]
    pub listen_ip: Option<String>,
    #[serde(default)]
    pub notify_interval: Option<u32>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub root_container: Option<String>,
    /// Database directory. Default: `cache_dir`. File is `files.db`.
    #[serde(default)]
    pub db_dir: Option<String>,
    /// Seconds between library rescans (new/changed/deleted files). 0 = off.
    #[serde(default = "default_rescan")]
    pub rescan_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            friendly_name: default_name(),
            network_interface: Vec::new(),
            media_dir: Vec::new(),
            exclude_dir: Vec::new(),
            exclude_file: Vec::new(),
            transcode: TranscodeCfg::default(),
            remap: Vec::new(),
            uuid: None,
            cache_dir: None,
            advertise_ip: None,
            listen_ip: None,
            notify_interval: None,
            serial: None,
            root_container: None,
            db_dir: None,
            rescan_secs: default_rescan(),
        }
    }
}

fn default_rescan() -> u64 {
    30
}

#[derive(Debug, serde::Deserialize)]
pub struct TranscodeCfg {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_encoder")]
    pub encoder: String,
    #[serde(default = "default_jobs")]
    pub max_jobs: u32,
}

impl Default for TranscodeCfg {
    fn default() -> Self {
        Self {
            enable: false,
            encoder: default_encoder(),
            max_jobs: default_jobs(),
        }
    }
}

fn default_name() -> String {
    "rustyDLNA".into()
}
fn default_encoder() -> String {
    "libx264".into()
}
fn default_jobs() -> u32 {
    16
}

pub fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}

pub fn resolve_http_port(cli: u16) -> u16 {
    std::env::var("RUSTY_DLNA_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(cli)
}

pub fn resolve_ssdp_port() -> u16 {
    std::env::var("RUSTY_DLNA_SSDP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(rusty_dlna_protocol::ssdp::SSDP_PORT)
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
    pub notify_interval: u32,
    pub cache_dir: PathBuf,
    pub update_id: AtomicU32,
    pub jobs: JobGate,
    pub(crate) remuxes: Mutex<HashMap<i64, Arc<remux::RemuxJob>>>,
    events: Mutex<events::EventHub>,
    pub(crate) client_cache: Mutex<ClientCache>,
}

struct DidlSnap {
    child: CatalogChild,
    child_count: Option<u32>,
    child_container_count: Option<u32>,
}

impl App {
    pub fn from_config(
        mut cfg: Config,
        http_port: u16,
        ssdp_port: u16,
        config_dir: &Path,
    ) -> Self {
        let (raw_dirs, types) = collect_media_dirs(&cfg.media_dir);
        let media_dirs: Vec<PathBuf> = raw_dirs
            .into_iter()
            .map(|pb| {
                if pb.is_absolute() {
                    pb
                } else {
                    config_dir.join(pb)
                }
            })
            .collect();
        let cache_dir = cfg
            .cache_dir
            .as_ref()
            .or(cfg.db_dir.as_ref())
            .map(|p| {
                let pb = PathBuf::from(p);
                if pb.is_absolute() {
                    pb
                } else {
                    config_dir.join(pb)
                }
            })
            .unwrap_or_else(|| config_dir.join("cache"));
        let db_path = cache_dir.join("files.db");
        let scan_cfg = ScanConfig {
            media_dirs,
            exclude_dirs: cfg.exclude_dir.clone(),
            exclude_files: cfg.exclude_file.clone(),
            types,
            db_path: Some(db_path),
        };
        let catalog = load_existing(&scan_cfg);
        let remaps = std::mem::take(&mut cfg.remap);
        let uuid = cfg
            .uuid
            .clone()
            .unwrap_or_else(|| "uuid:00000000-0000-4000-8000-000000000001".into());
        let listen_ip: std::net::Ipv4Addr = cfg
            .listen_ip
            .as_deref()
            .or(cfg.advertise_ip.as_deref())
            .and_then(|s| s.parse().ok())
            .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        let advertise_ip = cfg
            .advertise_ip
            .clone()
            .unwrap_or_else(|| {
                if !listen_ip.is_unspecified() {
                    listen_ip.to_string()
                } else {
                    "127.0.0.1".into()
                }
            });
        let notify_interval = cfg.notify_interval.unwrap_or(895);
        let max_jobs = cfg.transcode.max_jobs.max(1) as usize;
        let update_id = scan_cfg
            .db_path
            .as_ref()
            .and_then(|p| LibraryDb::open(p).ok())
            .map(|db| db.get_update_id())
            .unwrap_or(1);
        Self {
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
            notify_interval,
            cache_dir,
            update_id: AtomicU32::new(update_id),
            jobs: JobGate::new(max_jobs),
            remuxes: Mutex::new(HashMap::new()),
            events: Mutex::new(events::EventHub::new()),
            client_cache: Mutex::new(ClientCache::new()),
        }
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
        self.identify_peer(req, "127.0.0.1:9".parse().unwrap())
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
        let mut cache = self.client_cache.lock().expect("client cache");
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
        if rusty_dlna_http::http_body_too_large(req.body.len()) {
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
            serial: self
                .cfg
                .serial
                .clone()
                .unwrap_or_else(|| "1".into()),
            presentation_url: Some(format!(
                "http://{}:{}/",
                self.advertise_ip, self.http_port
            )),
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
            let res = self
                .events
                .lock()
                .expect("events")
                .unsubscribe(sid);
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
            let nt = req.header("NT").map(str::trim).filter(|s| !s.is_empty());
            if nt.is_none() {
                return HttpResponse::html(400, "Bad Request", "missing NT");
            }
            if !nt.unwrap().eq_ignore_ascii_case("upnp:event") {
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
            let timeout = req
                .header("Timeout")
                .and_then(|t| {
                    t.split("Second-")
                        .nth(1)
                        .and_then(|s| s.trim().parse::<u32>().ok())
                })
                .filter(|n| *n > 0)
                .unwrap_or(events::DEFAULT_TIMEOUT_SECS);
            let sid = format!(
                "uuid:{}",
                uuid_v4_like(self.update_id.load(Ordering::Relaxed))
            );
            let job = {
                let mut hub = self.events.lock().expect("events");
                match hub.subscribe_new(sid.clone(), cb.as_url(), service, timeout) {
                    Ok(job) => job,
                    Err(st) => {
                        return HttpResponse::html(st, "Precondition Failed", "subscriber table");
                    }
                }
            };
            let update_id = self.update_id.load(Ordering::Relaxed);
            events::spawn_notify(job, events::propertyset(service, update_id));
            let mut r = HttpResponse::new(200, "OK");
            r.set("SID", sid);
            r.set("Timeout", format!("Second-{timeout}"));
            r.set("Content-Length", "0");
            r.persist = persist;
            return r;
        }
        if let Some(sid) = sid_hdr {
            return match self.events.lock().expect("events").renew(sid) {
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
        let call = parse_soap_call(action, &body);
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
            return self.soap_update_object(&call, client, persist, req.user_agent().unwrap_or("-"));
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
                usize::MAX
            } else {
                call.requested_count as usize
            };
            let prepared = {
                let cat = self.catalog.read().expect("catalog");
                let (page, total) = if flag == "BrowseMetadata" {
                    match cat.metadata(&oid) {
                        Some(ch) => (vec![ch], 1u32),
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
                    match cat.page_children(&oid, 0, usize::MAX) {
                        Some((mut all, _)) => {
                            let order = default_order(client);
                            // Empty SortCriteria + folders-first already matches
                            // page_children (and Recent mtime-desc). Re-sort only
                            // for client keys or FORCE_SORT / LG defaults.
                            if !sort.is_empty() || order != DefaultOrder::FoldersFirst {
                                sort_catalog_children(&mut all, &sort, order);
                            }
                            let total = all.len() as u32;
                            let page: Vec<_> = all.into_iter().skip(start).take(take).collect();
                            (page, total)
                        }
                        None => {
                            return soap_fault_logged(
                                SoapOutcome::fault701(),
                                persist,
                                &call,
                                req.user_agent().unwrap_or("-"),
                            );
                        }
                    }
                };
                let slice: Vec<_> = page
                    .iter()
                    .map(|ch| self.snapshot_didl(ch, &cat))
                    .collect();
                (slice, total)
            };
            let (slice, total) = prepared;
            let didl: Vec<DidlObject> = slice
                .into_iter()
                .map(|snap| {
                    let mut o = self.to_didl_snap(snap, client, ua, &filter_bits);
                    // rustyDLNA magic container parentid_sql = "0": children of
                    // remapped root are advertised with parentID of the requested id.
                    if remapped_root {
                        o.parent_id = oid_raw.to_string();
                    }
                    o
                })
                .collect();
            let xml = build_browse(
                false,
                &didl,
                didl.len() as u32,
                total,
                self.update_id.load(Ordering::Relaxed),
                &filter_bits,
            );
            tracing::info!(
                ua = req.user_agent().unwrap_or("-"),
                oid = oid_raw,
                flag,
                n = didl.len(),
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
        let clauses = parse_search_criteria(call.search_criteria.as_deref());
        let start = call.starting_index as usize;
        let take = if call.requested_count == 0 {
            usize::MAX
        } else {
            call.requested_count as usize
        };
        let (slice, total) = {
            let cat = self.catalog.read().expect("catalog");
            let scope = self.remap_object_id(call.object_id.as_deref().unwrap_or("0"), client);
            let scoped = search_scope(&cat, &scope);
            let mut hits: Vec<CatalogChild> = Vec::new();
            for c in cat.containers.values() {
                if c.object_id == rusty_dlna_protocol::object_id::ROOT_ID {
                    continue;
                }
                if !scoped.contains(&c.object_id) {
                    continue;
                }
                let row = container_search_row(c);
                if row_matches(&clauses, &row) {
                    hits.push(CatalogChild::Container(c.clone()));
                }
            }
            for it in cat.items.values() {
                if !item_in_scope(it, &scoped) {
                    continue;
                }
                let row = item_search_row(it);
                if row_matches(&clauses, &row) {
                    hits.push(CatalogChild::Item(it.clone()));
                }
            }
            sort_catalog_children(&mut hits, &sort, default_order(client));
            let total = hits.len() as u32;
            let page: Vec<_> = hits
                .into_iter()
                .skip(start)
                .take(take)
                .map(|ch| self.snapshot_didl(&ch, &cat))
                .collect();
            (page, total)
        };
        let didl: Vec<DidlObject> = slice
            .into_iter()
            .map(|snap| self.to_didl_snap(snap, client, ua, &filter_bits))
            .collect();
        let xml = build_browse(
            true,
            &didl,
            didl.len() as u32,
            total,
            self.update_id.load(Ordering::Relaxed),
            &filter_bits,
        );
        let mut r = HttpResponse::xml(200, xml, false);
        r.persist = false;
        r
    }

    fn snapshot_didl(&self, ch: &CatalogChild, cat: &Catalog) -> DidlSnap {
        match ch {
            CatalogChild::Container(c) => DidlSnap {
                child: ch.clone(),
                child_count: Some(cat.displayed_child_count(&c.object_id)),
                child_container_count: Some(cat.displayed_container_count(&c.object_id)),
            },
            CatalogChild::Item(_) => DidlSnap {
                child: ch.clone(),
                child_count: None,
                child_container_count: None,
            },
        }
    }

    fn to_didl_snap(
        &self,
        snap: DidlSnap,
        client: &ClientProfile,
        ua: Option<&str>,
        bits: &FilterBits,
    ) -> DidlObject {
        match snap.child {
            CatalogChild::Container(c) => {
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
                    id: c.object_id,
                    parent_id: c.parent_id,
                    title: c.title,
                    class: c.class,
                    date: None,
                    restricted: true,
                    searchable: Some(c.searchable),
                    child_count: snap.child_count,
                    child_container_count: snap.child_container_count,
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
            CatalogChild::Item(it) => {
                let date = w3c_normalize_date(&it.date);
                let art_url = (it.album_art > 0).then(|| {
                    album_art_url(&self.advertise_ip, self.http_port, it.album_art, it.detail_id)
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
                let dcm_info = (bits.sec && it.bookmark_sec > 0).then(|| {
                    format!("CREATIONDATE=0,FOLDER={},BM={}", it.title, pos)
                });
                let title = apply_title_hack(
                    &it.title,
                    &it.ext,
                    client,
                    !it.captions.is_empty(),
                );
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
                    resources: self.item_resources(&it, client, ua, bits),
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
        let Some(detail_id) = self.resolve_detail_id(oid, client) else {
            return soap_fault_logged(SoapOutcome::fault701(), persist, call, ua);
        };
        let pos = call.pos_second.unwrap_or(0);
        let sec = bookmark_seconds(pos, client.flags.contains(ClientFlags::CONVERT_MS));
        self.persist_bookmark(detail_id, Some(sec), None);
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
        let Some(detail_id) = self.resolve_detail_id(oid, client) else {
            return soap_fault_logged(SoapOutcome::fault701(), persist, call, ua);
        };
        let tags = parse_update_object_tags(
            call.current_tag_value.as_deref(),
            call.new_tag_value.as_deref(),
        );
        let convert_ms = client.flags.contains(ClientFlags::CONVERT_MS);
        // -1 / values < 30 store as 0 (clear). Do not map -1 to None —
        // that leaves BOOKMARKS.SEC unchanged.
        let sec = tags
            .last_playback_position
            .map(|pos| bookmark_seconds(pos, convert_ms));
        self.persist_bookmark(detail_id, sec, tags.playback_count);
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
        let oid = magic_object_id(oid_raw, client);
        let cat = self.catalog.read().ok()?;
        if let Some(it) = cat.items.get(oid_raw).or_else(|| cat.items.get(&oid)) {
            return Some(it.detail_id);
        }
        match cat.metadata(oid_raw).or_else(|| cat.metadata(&oid)) {
            Some(CatalogChild::Item(it)) => Some(it.detail_id),
            _ => None,
        }
    }

    fn persist_bookmark(&self, detail_id: i64, sec: Option<i64>, watch: Option<i64>) {
        if sec.is_none() && watch.is_none() {
            return;
        }
        if let Some(path) = self.scan_cfg.db_path.as_ref() {
            if let Ok(db) = LibraryDb::open(path) {
                if let Some(s) = sec {
                    let _ = db.set_bookmark(detail_id, s);
                }
                if let Some(w) = watch {
                    let _ = db.set_watch_count(detail_id, w);
                }
            }
        }
        if let Ok(mut cat) = self.catalog.write() {
            for it in cat.items.values_mut() {
                if it.detail_id == detail_id {
                    if let Some(s) = sec {
                        it.bookmark_sec = s;
                    }
                    if let Some(w) = watch {
                        it.watch_count = w;
                    }
                }
            }
        }
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
        let orig_url = media_item_url(
            &self.advertise_ip,
            self.http_port,
            it.detail_id,
            &it.ext,
        );
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
        let plan = decide_for(client, ua, &src, &self.remaps);
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
            for (emime, info) in extra_ci1_protocol_infos(
                client.kind,
                &it.mime,
                it.dlna_pn.as_deref(),
            ) {
                res.push(DidlRes {
                    url: media_item_url(
                        &self.advertise_ip,
                        self.http_port,
                        it.detail_id,
                        &it.ext,
                    ),
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
        let Some(item) = self
            .catalog
            .read()
            .expect("catalog")
            .get_item_by_detail(id)
            .cloned()
        else {
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
            let plan = decide_for(client, req.user_agent(), &src, &self.remaps);
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
            let src_path = rusty_dlna_scan::rebase_media_path(&item.path, &self.scan_cfg.media_dirs);
            if !src_path.is_file() {
                tracing::error!(path = %src_path.display(), title = %item.title, "media missing");
                return HttpResponse::html(404, "Not Found", "missing file");
            }
            let mut plan = plan;
            plan.audio_index = pick_audio_index(&probe.audio);
            let dest = cache_dest(&self.cache_dir, item.detail_id, plan.action);
            let part = cache_part(&dest);
            let remux_p8 = plan.action == RecodeAction::RemuxP8;
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
            let mut r = live_transcode_response("video/mp4");
            r.remux_job = Some(RemuxJobSpec {
                detail_id: item.detail_id,
                src: src_path.clone(),
                dest: dest.clone(),
                args: ffmpeg_grow_args(
                    &src_path.to_string_lossy(),
                    &part.to_string_lossy(),
                    &grow_plan,
                ),
                remux_p8,
                audio_index: plan.audio_index,
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
        let path = rusty_dlna_scan::rebase_media_path(&item.path, &self.scan_cfg.media_dirs);
        if path.exists() && !rusty_dlna_scan::path_is_under_roots(&path, &self.scan_cfg.media_dirs)
        {
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
        if span > RAM_CAP {
            let mut r = media_response(
                &self.server,
                &now_imf_date(),
                &mime,
                size,
                range,
                Vec::new(),
                pn.as_deref(),
                ci,
            );
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
        let mut r = media_response(
            &self.server,
            &now_imf_date(),
            &mime,
            size,
            range,
            body,
            pn.as_deref(),
            ci,
        );
        if let Some(url) = caption_sec.as_deref() {
            set_caption_info_sec(&mut r, url);
        }
        r
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
            let cat = self.catalog.read().expect("catalog");
            cat.album_art_paths.get(&art_id).cloned()
        };
        let Some(path) = path else {
            return HttpResponse::html(404, "Not Found", "no such art");
        };
        let path = rusty_dlna_scan::rebase_media_path(&path, &self.scan_cfg.media_dirs);
        let body = match self.read_sidecar(&path) {
            Ok(b) => b,
            Err(r) => return r,
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
        let Some(item) = self
            .catalog
            .read()
            .expect("catalog")
            .get_item_by_detail(id)
            .cloned()
        else {
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
        let Some(item) = self
            .catalog
            .read()
            .expect("catalog")
            .get_item_by_detail(id)
            .cloned()
        else {
            return HttpResponse::html(404, "Not Found", "no item");
        };
        let src = if item.mime.starts_with("image/") {
            rusty_dlna_scan::rebase_media_path(&item.path, &self.scan_cfg.media_dirs)
        } else if item.album_art > 0 {
            let p = {
                let cat = self.catalog.read().expect("catalog");
                cat.album_art_paths.get(&item.album_art).cloned()
            };
            let Some(p) = p else {
                return HttpResponse::html(404, "Not Found", "no art");
            };
            rusty_dlna_scan::rebase_media_path(&p, &self.scan_cfg.media_dirs)
        } else {
            return HttpResponse::html(404, "Not Found", "nothing to resize");
        };
        let dest = self.cache_dir.join(format!("resized-{id}-{w}x{h}.jpg"));
        if !dest.is_file() && !rusty_dlna_scan::scale_jpeg(&src, &dest, w, h) {
            return HttpResponse::html(404, "Not Found", "resize failed");
        }
        match std::fs::read(&dest) {
            Ok(body) => {
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
        let Some(item) = self
            .catalog
            .read()
            .expect("catalog")
            .get_item_by_detail(id)
            .cloned()
        else {
            return HttpResponse::html(404, "Not Found", "no item");
        };
        let Some(cap) = item.captions.iter().find(|c| c.index == idx) else {
            return HttpResponse::html(404, "Not Found", "no caption");
        };
        let cap_path = rusty_dlna_scan::rebase_media_path(&cap.path, &self.scan_cfg.media_dirs);
        match self.read_sidecar(&cap_path) {
            Ok(body) => {
                let mut r = HttpResponse::new(200, "OK");
                r.set("Content-Type", caption_http_mime(&cap.ext));
                r.set("Content-Length", body.len());
                r.body = body;
                r
            }
            Err(r) => r,
        }
    }

    /// Art / captions: regular file under media_dir or cache_dir, size-capped.
    fn read_sidecar(&self, path: &Path) -> Result<Vec<u8>, HttpResponse> {
        let mut roots = self.scan_cfg.media_dirs.clone();
        roots.push(self.cache_dir.clone());
        if !rusty_dlna_scan::path_is_under_roots(path, &roots) {
            return Err(HttpResponse::html(404, "Not Found", "sidecar escaped"));
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return Err(HttpResponse::html(404, "Not Found", "sidecar missing")),
        };
        if meta.len() > rusty_dlna_scan::MAX_SIDECAR_BYTES {
            return Err(HttpResponse::html(413, "Payload Too Large", "sidecar too large"));
        }
        match std::fs::read(path) {
            Ok(b) => Ok(b),
            Err(_) => Err(HttpResponse::html(404, "Not Found", "sidecar missing")),
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

fn container_search_row(c: &rusty_dlna_scan::Container) -> SearchRow<'_> {
    SearchRow {
        title: &c.title,
        class: &c.class,
        id: &c.object_id,
        parent_id: &c.parent_id,
        is_container: true,
        ..SearchRow::default()
    }
}

fn item_search_row(it: &MediaItem) -> SearchRow<'_> {
    SearchRow {
        title: &it.title,
        creator: it.creator.as_deref().unwrap_or(""),
        date: &it.date,
        class: &it.class,
        artist: it.artist.as_deref().unwrap_or(""),
        genre: it.genre.as_deref().unwrap_or(""),
        album: it.album.as_deref().unwrap_or(""),
        id: &it.object_id,
        parent_id: &it.parent_id,
        ref_id: it.ref_id.as_deref(),
        is_container: false,
        ..SearchRow::default()
    }
}

fn search_scope(cat: &Catalog, root: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    if root.is_empty() || root == rusty_dlna_protocol::object_id::ROOT_ID {
        for c in cat.containers.keys() {
            out.insert(c.clone());
        }
        for it in cat.items.keys() {
            out.insert(it.clone());
        }
        return out;
    }
    let mut stack = vec![root.to_string()];
    while let Some(id) = stack.pop() {
        if !out.insert(id.clone()) {
            continue;
        }
        if let Some(c) = cat.containers.get(&id) {
            for ch in &c.children {
                stack.push(ch.clone());
            }
        }
    }
    for it in cat.items.values() {
        if out.contains(&it.parent_id) {
            out.insert(it.object_id.clone());
        }
        if let Some(r) = &it.ref_id {
            if out.contains(r) {
                out.insert(it.object_id.clone());
            }
        }
    }
    out
}

fn item_in_scope(it: &MediaItem, scoped: &std::collections::HashSet<String>) -> bool {
    scoped.contains(&it.object_id)
        || scoped.contains(&it.parent_id)
        || it.ref_id.as_ref().is_some_and(|r| scoped.contains(r))
}

fn sort_catalog_children(children: &mut [CatalogChild], specs: &[SortSpec], default: DefaultOrder) {
    children.sort_by(|a, b| cmp_children(a, b, specs, default));
}

fn cmp_children(
    a: &CatalogChild,
    b: &CatalogChild,
    specs: &[SortSpec],
    default: DefaultOrder,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if !specs.is_empty() {
        for spec in specs {
            let ord = cmp_sort_key(a, b, spec.key);
            let ord = if spec.descending { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        return Ordering::Equal;
    }
    match default {
        DefaultOrder::FoldersFirst => match (is_folder(a), is_folder(b)) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => cmp_ci(child_title(a), child_title(b)),
        },
        DefaultOrder::Lg => {
            let c = cmp_ci(child_class(a), child_class(b));
            if c != Ordering::Equal {
                return c;
            }
            cmp_ci(child_title(a), child_title(b))
        }
        DefaultOrder::ForceSort => {
            let c = cmp_ci(child_class(a), child_class(b));
            if c != Ordering::Equal {
                return c;
            }
            let c = child_disc(a).cmp(&child_disc(b));
            if c != Ordering::Equal {
                return c;
            }
            let c = child_track(a).cmp(&child_track(b));
            if c != Ordering::Equal {
                return c;
            }
            cmp_ci(child_title(a), child_title(b))
        }
    }
}

fn is_folder(ch: &CatalogChild) -> bool {
    matches!(ch, CatalogChild::Container(_))
}

fn child_title(ch: &CatalogChild) -> &str {
    match ch {
        CatalogChild::Container(c) => &c.title,
        CatalogChild::Item(i) => &i.title,
    }
}

fn child_class(ch: &CatalogChild) -> &str {
    match ch {
        CatalogChild::Container(c) => &c.class,
        CatalogChild::Item(i) => &i.class,
    }
}

fn child_date(ch: &CatalogChild) -> &str {
    match ch {
        CatalogChild::Container(_) => "",
        CatalogChild::Item(i) => &i.date,
    }
}

fn child_album(ch: &CatalogChild) -> &str {
    match ch {
        CatalogChild::Container(_) => "",
        CatalogChild::Item(i) => i.album.as_deref().unwrap_or(""),
    }
}

fn child_disc(ch: &CatalogChild) -> i64 {
    match ch {
        CatalogChild::Container(_) => 0,
        CatalogChild::Item(i) => i.disc.unwrap_or(0),
    }
}

fn child_track(ch: &CatalogChild) -> i64 {
    match ch {
        CatalogChild::Container(_) => 0,
        CatalogChild::Item(i) => i.track.unwrap_or(0),
    }
}

fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())
}

fn cmp_sort_key(a: &CatalogChild, b: &CatalogChild, key: SortKey) -> std::cmp::Ordering {
    match key {
        SortKey::Title => cmp_ci(child_title(a), child_title(b)),
        SortKey::Date => child_date(a).cmp(child_date(b)),
        SortKey::Class => cmp_ci(child_class(a), child_class(b)),
        SortKey::Album => cmp_ci(child_album(a), child_album(b)),
        SortKey::EpisodeNumber | SortKey::Track => child_track(a).cmp(&child_track(b)),
    }
}

fn sniff_renderer_location(url: &str, server: &str, app: &App) {
    let Some((host, port, path)) = split_http_url(url) else {
        return;
    };
    let Ok(ip) = host.parse::<Ipv4Addr>() else {
        return;
    };
    let Ok(mut sock) = std::net::TcpStream::connect_timeout(
        &SocketAddr::from((ip, port)),
        std::time::Duration::from_millis(400),
    ) else {
        return;
    };
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(600)));
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    );
    use std::io::{Read, Write};
    if sock.write_all(req.as_bytes()).is_err() {
        return;
    }
    let mut buf = String::new();
    let _ = sock.read_to_string(&mut buf);
    let friendly = xml_tag_loose(&buf, "friendlyName");
    let model = xml_tag_loose(&buf, "modelName");
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
    let _ = app
        .client_cache
        .lock()
        .expect("client cache")
        .remember(ip, profile, None, now);
}

fn split_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => (h.to_string(), p.parse().ok()?),
        _ => (auth.to_string(), 80u16),
    };
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

fn soap_outcome_http(
    out: SoapOutcome,
    persist: bool,
    call: &SoapCall,
    ua: &str,
) -> HttpResponse {
    if matches!(out, SoapOutcome::Fault { .. }) {
        return soap_fault_logged(out, persist, call, ua);
    }
    soap_to_http(out, persist)
}

/// UPnP SOAP faults are HTTP 500. 701 is a client holding a stale
/// ObjectID (Infuse caches them) — not a server failure.
fn soap_fault_logged(
    out: SoapOutcome,
    persist: bool,
    call: &SoapCall,
    ua: &str,
) -> HttpResponse {
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

/// 1×1 PNG (magic `\x89PNG`).
const ICON_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
    0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
    0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// 1×1 JPEG (SOI `FF D8` + JFIF). Must not be PNG bytes.
const ICON_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06,
    0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0a, 0x0c, 0x14, 0x0d, 0x0c, 0x0b, 0x0b,
    0x0c, 0x19, 0x12, 0x13, 0x0f, 0x14, 0x1d, 0x1a, 0x1f, 0x1e, 0x1d, 0x1a, 0x1c, 0x1c, 0x20,
    0x24, 0x2e, 0x27, 0x20, 0x22, 0x2c, 0x23, 0x1c, 0x1c, 0x28, 0x37, 0x29, 0x2c, 0x30, 0x31,
    0x34, 0x34, 0x34, 0x1f, 0x27, 0x39, 0x3d, 0x38, 0x32, 0x3c, 0x2e, 0x33, 0x34, 0x32, 0xff,
    0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xff, 0xc4, 0x00,
    0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
    0xff, 0xc4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00,
    0x3f, 0x00, 0x7f, 0xff, 0xd9,
];

fn icon_response(path: &str) -> HttpResponse {
    let lower = path.to_ascii_lowercase();
    let (mime, body) = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        ("image/jpeg", ICON_JPEG)
    } else {
        ("image/png", ICON_PNG)
    };
    let mut r = HttpResponse::new(200, "OK");
    r.set("Content-Type", mime);
    r.set("Content-Length", body.len());
    r.body = body.to_vec();
    r
}

fn uuid_v4_like(n: u32) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(n as u64);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (t >> 32) as u32,
        ((t >> 16) as u16) & 0xffff,
        (t as u16) & 0x0fff,
        n as u16 & 0x0fff,
        t & 0x0000_ffff_ffff
    )
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
    let ssdp_app = app.clone();
    let ssdp = tokio::spawn(async move {
        if let Err(e) = ssdp_loop(ssdp_app).await {
            tracing::warn!("ssdp loop: {e}");
        }
    });
    spawn_library_watch(app.clone());
    tokio::select! {
        _ = accept_loop(listener, app.clone()) => {}
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal");
        }
    }
    ssdp.abort();
    send_byebye(&app).await;
    Ok(())
}

async fn accept_loop(listener: tokio::net::TcpListener, app: Arc<App>) {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("accept: {e}");
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
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
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
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
        return;
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
    let sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("byebye socket: {e}");
            return;
        }
    };
    // Six types × 2, no LOCATION/SERVER/CACHE-CONTROL (packet builder).
    for _ in 0..2 {
        for p in &pkts {
            for (ip, port) in &dests {
                let _ = sock.send_to(p.as_bytes(), (*ip, *port)).await;
            }
        }
    }
}

fn persist_update_id(app: &App, id: u32) {
    let Some(path) = app.scan_cfg.db_path.as_ref() else {
        return;
    };
    match LibraryDb::open(path) {
        Ok(db) => {
            if let Err(e) = db.set_update_id(id) {
                tracing::warn!(error = %e, "persist update_id");
            }
        }
        Err(e) => tracing::warn!(error = %e, "open db to persist update_id"),
    }
}

fn bump_update_id(app: &App) -> u32 {
    let id = app.update_id.fetch_add(1, Ordering::Relaxed) + 1;
    persist_update_id(app, id);
    id
}

pub(crate) fn apply_catalog(app: &App, next: Catalog, delta: ScanDelta, why: &'static str) {
    let items = next.items.len();
    *app.catalog.write().expect("catalog") = next;
    let id = bump_update_id(app);
    events::notify_content_dir(&app.events, id);
    tracing::info!(
        items,
        added = delta.added,
        removed = delta.removed,
        changed = delta.changed,
        "{why}"
    );
}

fn reconcile_library(app: &App, why: &'static str) {
    match monitor(&app.scan_cfg) {
        (Some(next), delta) => apply_catalog(app, next, delta, why),
        _ => {}
    }
}

fn spawn_library_watch(app: Arc<App>) {
    let rescan_secs = app.cfg.rescan_secs;
    let inotify_app = app.clone();
    std::thread::Builder::new()
        .name("inotify".into())
        .spawn(move || {
            let app = inotify_app;
            let cfg = app.scan_cfg.clone();
            let empty = app
                .catalog
                .read()
                .map(|c| c.items.is_empty())
                .unwrap_or(true);
            if empty {
                tracing::info!("empty library; full scan from disk");
                let next = scan(&cfg);
                let items = next.items.len();
                *app.catalog.write().expect("catalog") = next;
                let id = bump_update_id(&app);
                events::notify_content_dir(&app.events, id);
                tracing::info!(items, "full scan done");
            } else {
                match repair_objects_if_needed(&cfg) {
                    (Some(next), delta) => {
                        apply_catalog(&app, next, delta, "library object repair")
                    }
                    _ => {}
                }
                // Drop gone files / empty folders left by inotify path-prefix misses.
                reconcile_library(&app, "library reconcile");
            }
            fill_missing_av_meta(&app);
            let watch_app = app.clone();
            if let Err(e) = run_inotify(cfg, move |next, delta| {
                apply_catalog(&watch_app, next, delta, "inotify library update");
            }) {
                tracing::warn!("inotify: {e}");
            }
        })
        .expect("inotify thread");
    if rescan_secs > 0 {
        std::thread::Builder::new()
            .name("rescan".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(rescan_secs));
                // Skip if a walk is still running. A 30s interval with a
                // 45s tree walk was stacking +11k "adds" every cycle.
                reconcile_library(&app, "periodic rescan");
            })
            .expect("rescan thread");
    }
}

fn fill_missing_av_meta(app: &App) {
    let Some(dbp) = app.scan_cfg.db_path.clone() else {
        return;
    };
    let Ok(db) = LibraryDb::open(&dbp) else {
        return;
    };
    let rows = db.details_missing_stream_meta().unwrap_or_default();
    let mut filled = 0usize;
    if !rows.is_empty() {
        tracing::info!(n = rows.len(), "filling missing stream metadata from files");
        let mut seen = std::collections::HashSet::new();
        for (id, path) in rows {
            if path.contains("/incomplete/") {
                continue;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            let live = rusty_dlna_scan::rebase_media_path(
                std::path::Path::new(&path),
                &app.scan_cfg.media_dirs,
            );
            let Some(got) = rusty_dlna_scan::probe_media(&live) else {
                continue;
            };
            rusty_dlna_scan::apply_probe_to_detail(&db, id, &got);
            filled += 1;
            if filled % 200 == 0 {
                tracing::info!(filled, "stream metadata progress");
            }
        }
        tracing::info!(filled, "stream metadata fill done");
    }
    let derived = db.backfill_derived_stream_fields().unwrap_or_else(|e| {
        tracing::warn!(%e, "derived stream backfill failed");
        0
    });
    if derived > 0 {
        tracing::info!(n = derived, "backfilled DLNA_PN / mpeg4 from stored stream columns");
    }
    if filled > 0 || derived > 0 {
        if let Ok(next) = db.load_catalog() {
            *app.catalog.write().expect("catalog") = next;
        }
    }
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
        let group: Ipv4Addr = rusty_dlna_protocol::ssdp::SSDP_MCAST_ADDR
            .parse()
            .unwrap();
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

async fn send_alive(sock: &tokio::net::UdpSocket, app: &App) {
    if app.ssdp_port != rusty_dlna_protocol::ssdp::SSDP_PORT {
        return;
    }
    let dest = (
        rusty_dlna_protocol::ssdp::SSDP_MCAST_ADDR,
        rusty_dlna_protocol::ssdp::SSDP_PORT,
    );
    let pkts = rusty_dlna_ssdp::notify_alive(
        &app.uuid,
        &app.advertise_ip,
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

fn configured_iface_addrs(names: &[String]) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if let Ok(ip) = name.parse::<Ipv4Addr>() {
            out.push(ip);
            continue;
        }
        if let Some(ip) = ipv4_for_iface(name) {
            out.push(ip);
        }
    }
    out
}

#[cfg(unix)]
fn ipv4_for_iface(name: &str) -> Option<Ipv4Addr> {
    unsafe {
        let mut ifa = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return None;
        }
        let mut cur = ifa;
        let mut found = None;
        while !cur.is_null() {
            let entry = &*cur;
            let cname = if entry.ifa_name.is_null() {
                ""
            } else {
                std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_str()
                    .unwrap_or("")
            };
            if cname == name {
                if let Some(addr) = entry.ifa_addr.as_ref() {
                    if addr.sa_family as i32 == libc::AF_INET {
                        let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        let oct = u32::from_be(sin.sin_addr.s_addr);
                        found = Some(Ipv4Addr::from(oct));
                        break;
                    }
                }
            }
            cur = entry.ifa_next;
        }
        libc::freeifaddrs(ifa);
        found
    }
}

#[cfg(not(unix))]
fn ipv4_for_iface(_name: &str) -> Option<Ipv4Addr> {
    None
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

async fn ssdp_loop(app: Arc<App>) -> std::io::Result<()> {
    let recv_sock = ssdp_recv_bind(&app)?;
    let live = app.ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    let iface = ssdp_iface(&app);
    if live {
        let group: Ipv4Addr = rusty_dlna_protocol::ssdp::SSDP_MCAST_ADDR
            .parse()
            .unwrap();
        let extras = configured_iface_addrs(&app.cfg.network_interface);
        let join_list: Vec<Ipv4Addr> = if extras.is_empty() {
            vec![iface]
        } else {
            extras
        };
        for ip in join_list {
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
    let recv = tokio::net::UdpSocket::from_std(recv_std)?;

    // Kodi/Platinum drops SSDP replies whose UDP source port is not 1900.
    // Reply from the advertise unicast IP (rustyDLNA used the primary
    // 192.0.2.10) so LOCATION host and UDP source match.
    let send = if live {
        let bind_ip = if iface.is_unspecified() {
            app.listen_ip
        } else {
            iface
        };
        let n = match bind_udp_reuse(SocketAddrV4::new(bind_ip, app.ssdp_port)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "SSDP reply bind {bind_ip}:{}: {e}; trying 0.0.0.0:{}",
                    app.ssdp_port,
                    app.ssdp_port
                );
                bind_udp_reuse(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, app.ssdp_port))?
            }
        };
        if !iface.is_unspecified() {
            let _ = n.set_multicast_if_v4(&iface);
        }
        let _ = n.set_multicast_ttl_v4(4);
        let _ = n.set_multicast_loop_v4(false);
        let stdn = into_std_udp(n);
        tracing::info!(addr = %stdn.local_addr()?, "SSDP reply/notify socket");
        Some(tokio::net::UdpSocket::from_std(stdn)?)
    } else {
        None
    };
    tracing::info!(%recv_addr, port = app.ssdp_port, "ssdp listen");

    let send_sock = send.as_ref();
    if let Some(s) = send_sock {
        send_alive(s, &app).await;
        tokio::time::sleep(std::time::Duration::from_millis(jitter_ms(ALIVE_DUP_DELAY_MS))).await;
        send_alive(s, &app).await;
    }

    let mut buf = vec![0u8; 2048];
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(
        app.notify_interval.max(1) as u64,
    ));
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(s) = send_sock {
                    send_alive(s, &app).await;
                    tokio::time::sleep(std::time::Duration::from_millis(jitter_ms(ALIVE_DUP_DELAY_MS))).await;
                    send_alive(s, &app).await;
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
                    let app2 = Arc::clone(&app);
                    let loc = n.location.clone();
                    let server = n.server.clone();
                    tokio::task::spawn_blocking(move || {
                        sniff_renderer_location(&loc, &server, &app2);
                    });
                    continue;
                }
                let Ok(ms) = parse_msearch(&text) else { continue };
                let date = now_imf_date();
                let replies = msearch_replies(
                    &app.uuid,
                    &ms.st,
                    &app.advertise_ip,
                    app.http_port,
                    app.notify_interval,
                    &app.server,
                    &date,
                );
                if replies.is_empty() {
                    continue;
                }
                tracing::info!(%from, st = %ms.st, n = replies.len(), "SSDP M-SEARCH reply");
                let all = ms.st == rusty_dlna_protocol::ssdp::ST_ALL;
                tokio::time::sleep(std::time::Duration::from_millis(jitter_ms(
                    msearch_jitter_ms_range(all),
                )))
                .await;
                let out = send_sock.unwrap_or(&recv);
                for r in &replies {
                    if let Err(e) = out.send_to(r.as_bytes(), from).await {
                        tracing::warn!(%from, "SSDP reply send: {e}");
                    }
                }
                // Second copy: Platinum often misses the first datagram.
                for r in &replies {
                    let _ = out.send_to(r.as_bytes(), from).await;
                }
            }
        }
    }
}

async fn handle_conn(
    app: Arc<App>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut persist_left = 100u32;
    loop {
        let mut buf = vec![0u8; 0];
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(i) = rusty_dlna_http::header_block_complete(&buf) {
                break i;
            }
            if buf.len() > 64 * 1024 {
                return Ok(());
            }
        };
        let head = std::str::from_utf8(&buf[..header_end])?;
        let mut req = HttpRequest::parse_headers(head).map_err(|e| format!("parse: {e:?}"))?;
        let need = req.content_length().unwrap_or(0);
        if rusty_dlna_http::http_body_too_large(need) {
            let resp = HttpResponse::html(413, "Payload Too Large", "body too large");
            sock.write_all(&resp.bytes_wire(&app.server, &now_imf_date()))
                .await?;
            break;
        }
        let mut body = buf[header_end..].to_vec();
        while body.len() < need {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(need);
        req.body = body;
        let resp = app.handle_from(&req, peer);
        persist_left = persist_left.saturating_sub(1);
        if let Some(spec) = resp.remux_job.clone() {
            remux::serve_remux(&app, &mut sock, &req, spec).await?;
            break;
        }
        let wire = resp.bytes_wire(&app.server, &now_imf_date());
        sock.write_all(&wire).await?;
        if let Some((path, start, end)) = resp.file_range.clone() {
            stream_file_range(&mut sock, &path, start, end).await?;
        }
        if !resp.persist || persist_left == 0 {
            break;
        }
    }
    Ok(())
}

pub(crate) async fn stream_file_range(
    sock: &mut tokio::net::TcpStream,
    path: &std::path::Path,
    start: u64,
    end: u64,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
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
        sock.write_all(&buf[..got]).await?;
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
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
        0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06,
        0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D,
        0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
        0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28,
        0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32,
        0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
        0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0xFF, 0xC4,
        0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00,
        0x3F, 0x00, 0x2A, 0x1F, 0xFF, 0xD9,
    ];

    fn workspace() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn testdata_app() -> App {
        let root = workspace();
        let lib = root.join("testdata/library");
        ensure_pattern_fixture(&lib);
        rusty_dlna_scan::ensure_show_fixture(&lib);
        let nfo = lib.join("video/movie.nfo");
        if !nfo.exists() {
            let _ = std::fs::create_dir_all(lib.join("video"));
            let _ = std::fs::write(&nfo, "<movie><year>1999</year></movie>\n");
            let _ = std::fs::write(lib.join("video/movie.srt"), "1\n00:00:00,000 --> 00:00:01,000\nhi\n");
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
            cache_dir: Some(format!(
                "testdata/cache/t-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )),
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
            },
            ..Config::default()
        };
        let app = App::from_config(cfg, 18200, 11900, &root);
        let cat = scan(&app.scan_cfg);
        *app.catalog.write().expect("catalog") = cat;
        app
    }

    fn req(raw: &str) -> HttpRequest {
        HttpRequest::parse_headers(raw).unwrap()
    }

    fn get(path: &str, ua: &str) -> String {
        format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: {ua}\r\n\r\n"
        )
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
        let sub = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:8200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        );
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
        use std::io::Read;
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
                    let _ = sock.read_to_end(&mut buf);
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
        let new_ok = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        );
        let r = app.handle_from(&new_ok, peer50);
        assert_eq!(r.status, 200);
        let sid = resp_header(&r, "SID").unwrap_or("");
        assert!(sid.starts_with("uuid:"), "{sid}");
        assert_eq!(resp_header(&r, "Timeout"), Some("Second-300"));

        let inject = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt\nX-Injected: 1>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        );
        assert_eq!(app.handle_from(&inject, peer50).status, 412);

        let mismatch = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        );
        assert_eq!(app.handle_from(&mismatch, peer1).status, 412);

        let both = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             SID: uuid:already-have-one\r\n\
             NT: upnp:event\r\n\
             Content-Length: 0\r\n\r\n",
        );
        assert_eq!(app.handle_from(&both, peer50).status, 400);

        let no_nt = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             Callback: <http://192.0.2.50:1234/evt>\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        );
        assert_eq!(app.handle_from(&no_nt, peer50).status, 400);

        let renew_unknown = req(
            "SUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: uuid:00000000-0000-4000-8000-ffffffffffff\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        );
        assert_eq!(app.handle_from(&renew_unknown, peer50).status, 412);

        let unsub_unknown = req(
            "UNSUBSCRIBE /evt/ContentDir HTTP/1.1\r\n\
             Host: 192.0.2.10:18200\r\n\
             SID: uuid:00000000-0000-4000-8000-ffffffffffff\r\n\
             Content-Length: 0\r\n\r\n",
        );
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
        assert_eq!(resp_header(&r, "Timeout"), Some("Second-300"));
        assert_eq!(resp_header(&r, "SID"), Some(sid));
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
        let cat = app.catalog.read().expect("catalog").clone();
        apply_catalog(
            &app,
            cat,
            ScanDelta {
                changed: 1,
                ..ScanDelta::default()
            },
            "test catalog bump",
        );
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
            assert_eq!(db.get_update_id(), after);
        }
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
        let unsub_rebind = req(
            "UNSUBSCRIBE /evt/ContentDir HTTP/1.1\r\nHost: evil.test\r\nSID: uuid:x\r\n\r\n",
        );
        assert_eq!(app.handle(&unsub_rebind).status, 400);
    }

    #[test]
    fn sidecar_size_and_symlink_jail() {
        let app = testdata_app();
        let (art_id, detail_id) = {
            let cat = app.catalog.read().expect("catalog");
            let movie = cat
                .items
                .values()
                .find(|i| i.path.ends_with("movie.mkv"))
                .expect("movie");
            (movie.album_art, movie.detail_id)
        };
        assert!(art_id > 0);

        let outside = std::env::temp_dir().join(format!(
            "rdlna-secret-{}-{}",
            std::process::id(),
            detail_id
        ));
        std::fs::write(&outside, b"not-a-poster").unwrap();
        {
            let mut cat = app.catalog.write().expect("catalog");
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
            let mut cat = app.catalog.write().expect("catalog");
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
            let mut cat = app.catalog.write().expect("catalog");
            cat.album_art_paths.insert(art_id, link.clone());
        }
        let via_link = app.handle(&req(&get(
            &format!("/AlbumArt/{art_id}-{detail_id}.jpg"),
            "Kodi/21.0",
        )));
        assert_eq!(via_link.status, 404, "symlink out of tree must 404");

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_file(&big);
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
        let body = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>{oid}</ObjectID><BrowseFlag>{flag}</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#
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
        assert!(
            video.contains("object.container.storageFolder"),
            "{video}"
        );
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
            !items.contains("&lt;dc:title&gt;movie&lt;/dc:title&gt;")
                && !items.contains(">movie<"),
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
        assert!(genres.contains("Drama") || genres.contains("Crime"), "{genres}");
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
        assert!(items.contains("The Show") || items.contains("Pilot"), "{items}");
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
        assert!(items.contains("/AlbumArt/"), "DIDL missing /AlbumArt/: {items}");
        assert!(
            items.contains("JPEG_TN") || items.contains("albumArtURI"),
            "DIDL missing JPEG_TN or albumArtURI: {items}"
        );
        let (art_id, detail_id) = parse_album_art_url(&items).expect("parse art url from DIDL");
        assert!(art_id > 0, "art id from DIDL");
        {
            let cat = app.catalog.read().expect("catalog");
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
        assert_eq!(resp_header(&r, "transferMode.dlna.org"), Some("Interactive"));
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
            .status()
            .map(|s| s.success())
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
        let (st, xml) = soap_browse(&app, "0", "BrowseDirectChildren", "VLC/3.0.21 LibVLC/3.0.21");
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
        assert!(!xml.contains("&lt;item "), "root video view is folders only: {xml}");
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
        let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert!(kodi.contains("dvp7"));
        // testdata remap is CrKey-only, so Kodi still sees the original.
        assert!(!kodi.contains("/Transcode/"));
        let (_, cr) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "CrKey/1.54.384650 DLNADOC/1.50",
        );
        let t = cr.find("/Transcode/").expect("CrKey DIDL missing remap");
        let m = cr.find("/MediaItems/").expect("CrKey DIDL missing original");
        assert!(t < m, "remux res must be listed first: {cr}");
        assert!(cr.contains("DLNA.ORG_CI=1"));
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
        let t = kodi.find("/Transcode/").expect("Kodi DIDL missing remap");
        let m = kodi.find("/MediaItems/").expect("Kodi DIDL missing original");
        assert!(t < m, "remux res must be listed first: {kodi}");
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
        assert!(!pc_fl.contains("id=&quot;A&quot;"), "PC must not use A: {pc_fl}");
        assert!(!pc_fl.contains("id=&quot;V&quot;"), "PC must not use V: {pc_fl}");
        assert!(!pc_fl.contains("id=&quot;I&quot;"), "PC must not use I: {pc_fl}");
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
        let (_, j5500) = soap_browse(&app, "2$8", "BrowseDirectChildren", "DLNADOC/1.50 [BD]J5500");
        assert!(
            !j5500.contains("video/x-matroska:DLNA.ORG_PN=")
                && !j5500.contains("video/x-mkv:DLNA.ORG_PN="),
            "J5500 media res must skip DLNA.ORG_PN: {j5500}"
        );

        // Xbox rootDesc modelNumber=1
        let xbox = app.handle(&req(&get("/rootDesc.xml", "Xbox/360")));
        assert!(String::from_utf8_lossy(&xbox.body).contains("<modelNumber>1</modelNumber>"));

        // CrKey: transcode res first on DV P7
        let (_, cr) = soap_browse(&app, "2$8", "BrowseDirectChildren", "CrKey/1.54 DLNADOC/1.50");
        let t = cr.find("/Transcode/").expect("crkey remap");
        let m = cr.find("/MediaItems/").expect("crkey original");
        assert!(t < m, "remux res must be listed first: {cr}");
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

    fn set_detail_dlna_pn(app: &App, detail_id: i64, pn: &str) {
        let mut cat = app.catalog.write().expect("catalog");
        let oid = cat
            .by_detail
            .get(&detail_id)
            .cloned()
            .expect("by_detail oid");
        cat.items
            .get_mut(&oid)
            .expect("by_detail item")
            .dlna_pn = Some(pn.into());
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
            xml.contains("&lt;upnp:lastPlaybackPosition&gt;120000&lt;/upnp:lastPlaybackPosition&gt;")
                || xml.contains("<upnp:lastPlaybackPosition>120000</upnp:lastPlaybackPosition>"),
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
    fn transcode_get_is_live_pipe_not_full_file() {
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
            .find(|i| i.title == "dvp7")
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
            spec.args.iter().any(|s| s.contains("frag_keyframe")),
            "{:?}",
            spec.args
        );
        assert!(!spec.args.iter().any(|s| s.contains("faststart")), "{:?}", spec.args);
        assert!(
            spec.args.iter().any(|s| s.ends_with(".part")),
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
        let dest = rusty_dlna_transcode::cache_dest(
            &app.cache_dir,
            id,
            rusty_dlna_transcode::RecodeAction::RemuxP8,
        );
        assert!(!dest.is_file(), "handle() must not wait for a finished cache");
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
            .find(|i| i.title == "dvp7")
            .cloned()
            .expect("dvp7");
        let live = rusty_dlna_scan::rebase_media_path(&dvp7.path, &app.scan_cfg.media_dirs);
        let dest = rusty_dlna_transcode::cache_dest(
            &app.cache_dir,
            dvp7.detail_id,
            rusty_dlna_transcode::RecodeAction::RemuxP8,
        );
        if let Some(p) = dest.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let src = app.cache_dir.join("dvp7-stamp-src.mkv");
        let _ = std::fs::copy(&live, &src);
        let payload = b"0123456789abcdefFINISHED_REMUX_BYTES";
        std::fs::write(&dest, payload).unwrap();
        rusty_dlna_transcode::write_cache_stamp(&dest, &src);
        assert!(rusty_dlna_transcode::cache_is_fresh(&dest, &src));

        let spec = RemuxJobSpec {
            detail_id: dvp7.detail_id,
            src: src.clone(),
            dest: dest.clone(),
            args: vec!["ffmpeg".into(), "-version".into()],
            remux_p8: true,
            audio_index: 0,
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
                remux::serve_remux(&app2, &mut sock, &req, spec2).await.unwrap();
            });
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let mut buf = Vec::new();
            let _ = c.read_to_end(&mut buf);
            let _ = h.await;
            let text = String::from_utf8_lossy(&buf).into_owned();
            let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
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
        assert!(
            !rusty_dlna_transcode::cache_is_fresh(&dest, &src),
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
            let body = format!(
                r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>no-such-object</ObjectID><BrowseFlag>BrowseMetadata</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#
            );
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
        let tmp = workspace().join(format!(
            "testdata/cache/twodir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
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
        let cat = scan(&app.scan_cfg);
        let titles: Vec<_> = cat.items.values().map(|i| i.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| *t == "clip"),
            "video under V dir must be accepted: {titles:?}"
        );
        assert!(
            titles.iter().any(|t| *t == "song"),
            "audio under A dir must be accepted: {titles:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
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
        assert!(body.contains("refresh") || body.contains("Refresh") || body.contains("20"), "{body}");
        assert!(
            !body.contains("<H1>200 OK</H1>"),
            "status must be the document, not the error-page wrapper: {body}"
        );
        assert!(
            body.contains("<h1>rustyDLNA-test</h1>") || body.contains("<title>rustyDLNA-test</title>"),
            "{body}"
        );
        let root = app.handle(&req(&get("/", "Kodi/21.0")));
        assert_eq!(root.status, 200);
        assert_eq!(root.body, r.body);
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
            "Kodi/21.0",
        );
        assert_eq!(st, 200, "{xml}");
        assert!(xml.contains("Fixture Movie"), "or must hit video: {xml}");
        assert!(xml.contains("Fixture Track"), "or must hit audio: {xml}");
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
        assert!(!xml.contains(" size=&quot;") && !xml.contains(" size=\""), "Filter without res@size: {xml}");
        assert!(!xml.contains("&lt;res ") && !xml.contains("<res "), "Filter without res: {xml}");
        let (st, star) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert_eq!(st, 200);
        assert!(star.contains("size=&quot;") || star.contains("size=\""), "{star}");
    }

    #[test]
    fn browse_recent_keeps_mtime_order() {
        let app = testdata_app();
        {
            let mut cat = app.catalog.write().expect("catalog");
            let mut videos: Vec<String> = cat
                .items
                .values()
                .filter(|i| {
                    i.class.contains("video")
                        && i.ref_id.is_none()
                        && i.object_id.starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
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
        let fresh = titled.find("Zzz Fresh").expect("fresh title after +dc:title");
        assert!(
            early < fresh,
            "explicit +dc:title must still re-sort Recent: {titled}"
        );
    }

    #[test]
    fn browse_force_sort_track_order() {
        let app = testdata_app();
        {
            let mut cat = app.catalog.write().expect("catalog");
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
        let envelope = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#
        );
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
            &req(&get(
                &format!("/Transcode/{tid}.mp4"),
                "DLNADOC/1.50",
            )),
            cr_peer,
        );
        assert_ne!(tget.status, 404, "cached CrKey must still remap Transcode GET");
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
    fn recent_keeps_old_mtime_and_caps_at_200() {
        let app = testdata_app();
        let movie = {
            let cat = app.catalog.read().unwrap();
            cat.items
                .values()
                .find(|i| {
                    i.path.ends_with("movie.mkv")
                        && i.ref_id.is_none()
                        && i.object_id.starts_with(rusty_dlna_protocol::object_id::BROWSEDIR_ID)
                })
                .cloned()
                .expect("browse-folder movie.mkv")
        };
        {
            let mut cat = app.catalog.write().expect("catalog");
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
            let mut cat = app.catalog.write().expect("catalog");
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
        let (st, xml) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "LGE_DLNA_SDK/1.6.0",
        );
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
            let body = format!(
                r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>2$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#
            );
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
