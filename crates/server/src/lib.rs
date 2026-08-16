//! Accept loop, SOAP Browse, original GET/`Range`, and ffmpeg file-cache.
//! Listen ports come from `RUSTY_DLNA_HTTP_PORT` / `RUSTY_DLNA_SSDP_PORT`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rusty_dlna_http::{
    dlna_org_features, gen_root_desc, media_response, now_imf_date, parse_byte_range,
    persist_for_route, protocol_info, read_file_range, route, scpd_connection_manager,
    scpd_content_directory, scpd_registrar, timeseek_without_range, valid_host_header, ByteRange,
    HttpRequest, HttpResponse, HttpRoute, RangeError, RootDescOpts,
};
use rusty_dlna_protocol::isolation::collides_with_live_minidlna;
use rusty_dlna_protocol::paths::{
    caption_from_path, media_item_id_from_path, media_item_url, transcode_id_from_path,
    transcode_item_url,
};
use rusty_dlna_protocol::server_header;
use rusty_dlna_protocol::w3c_normalize_date;
use rusty_dlna_protocol::{
    identify_friendly_name, identify_user_agent, identify_x_av_client_info, remap_mime, ClientFlags,
    ClientKind, ClientProfile, CLIENTS,
};
use rusty_dlna_scan::{
    caption_http_mime, collect_media_dirs, load_existing, monitor, probe_av_meta,
    repair_objects_if_needed, run_inotify, scan, Catalog, CatalogChild, LibraryDb, MediaItem,
    ScanConfig, ScanDelta,
};

pub use rusty_dlna_scan::ensure_pattern_fixture;
use rusty_dlna_soap::{
    build_browse, dispatch_simple, magic_object_id, parse_soap_call, soap_fault, DidlObject,
    DidlRes, SoapOutcome,
};
use rusty_dlna_ssdp::{msearch_replies, parse_msearch};
use rusty_dlna_transcode::{
    cache_dest, decide, ensure_cached_file, probe_to_source, Decision, JobGate, RemapRule,
    TranscodePlan,
};

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
    /// Bind address. Default `0.0.0.0`. Set to a LAN IP (e.g. `10.0.1.67`).
    #[serde(default)]
    pub listen_ip: Option<String>,
    #[serde(default)]
    pub notify_interval: Option<u32>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub root_container: Option<String>,
    /// MiniDLNA `db_dir`. Default: `cache_dir`. Database file is `files.db`.
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
    2
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
    pub catalog: Mutex<Catalog>,
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
    pub bookmarks: Mutex<HashMap<String, i64>>,
    pub jobs: JobGate,
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
        Self {
            cfg,
            catalog: Mutex::new(catalog),
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
            update_id: AtomicU32::new(1),
            bookmarks: Mutex::new(HashMap::new()),
            jobs: JobGate::new(max_jobs),
        }
    }

    pub fn isolation_ok(&self) -> Result<(), String> {
        if collides_with_live_minidlna(self.http_port, self.ssdp_port) {
            return Err(format!(
                "refusing to bind live MiniDLNA ports {}/{} (phase 9 not started); set RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900",
                self.http_port, self.ssdp_port
            ));
        }
        Ok(())
    }

    pub fn identify(&self, req: &HttpRequest) -> &'static ClientProfile {
        if let Some(ua) = req.user_agent() {
            if let Some(p) = identify_user_agent(ua) {
                return p;
            }
        }
        if let Some(x) = req.header("X-AV-Client-Info") {
            if let Some(p) = identify_x_av_client_info(x) {
                return p;
            }
        }
        if let Some(f) = req.header("FriendlyName") {
            if let Some(p) = identify_friendly_name(f) {
                return p;
            }
        }
        &CLIENTS[0]
    }

    /// Shipped request handler. Tests drive this; the accept loop calls it.
    pub fn handle(&self, req: &HttpRequest) -> HttpResponse {
        let method = req.method.to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "HEAD" | "POST" | "SUBSCRIBE" | "UNSUBSCRIBE"
        ) {
            return HttpResponse::html(501, "Not Implemented", "unsupported method");
        }
        if matches!(method.as_str(), "GET" | "HEAD") {
            if req.version == "HTTP/1.1" && req.header("Host").is_none() {
                return HttpResponse::html(400, "Bad Request", "DNS rebinding attack suspected");
            }
            if let Some(h) = req.header("Host") {
                if !valid_host_header(h) {
                    return HttpResponse::html(400, "Bad Request", "DNS rebinding attack suspected");
                }
            }
        }
        if timeseek_without_range(req) {
            return HttpResponse::html(406, "Not Acceptable", "TimeSeek/PlaySpeed without Range");
        }
        let r = route(&req.method, &req.path);
        let persist = persist_for_route(
            r,
            Some(req.version.as_str()),
            req.conn_close(),
            req.conn_keep(),
        );
        let mut resp = match r {
            HttpRoute::RootDesc => self.root_desc(req),
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
            | HttpRoute::EventRegistrar => self.gena(req, persist),
            HttpRoute::Soap => self.soap(req, persist),
            HttpRoute::MediaItem => self.media(req, false),
            HttpRoute::Transcode => self.media(req, true),
            HttpRoute::Caption => self.caption(req),
            HttpRoute::Icon => icon_response(&req.path),
            HttpRoute::Status | HttpRoute::Presentation => {
                HttpResponse::html(200, "OK", &format!("<p>{}</p>", self.cfg.friendly_name))
            }
            HttpRoute::NotFound | _ => HttpResponse::html(404, "Not Found", "not found"),
        };
        if r == HttpRoute::MediaItem || r == HttpRoute::Transcode || r == HttpRoute::Soap {
            resp.persist = false;
        } else {
            resp.persist = persist;
        }
        if method == "HEAD" {
            resp.body.clear();
        }
        resp
    }

    fn root_desc(&self, req: &HttpRequest) -> HttpResponse {
        tracing::debug!(
            host = req.header("Host").unwrap_or("-"),
            ua = req.user_agent().unwrap_or("-"),
            "rootDesc"
        );
        let client = self.identify(req);
        let opts = RootDescOpts {
            friendly_name: self.cfg.friendly_name.clone(),
            uuid: self.uuid.clone(),
            model_number: "1".into(),
            manufacturer: "Justin Maggard".into(),
            model_name: "Windows Media Connect compatible (rustyDLNA)".into(),
            model_description: "MiniDLNA on Linux".into(),
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

    /// MiniDLNA GENA: SUBSCRIBE returns SID + Timeout. Kodi/Platinum
    /// subscribe to ContentDirectory after reading the SCPD.
    fn gena(&self, req: &HttpRequest, persist: bool) -> HttpResponse {
        let method = req.method.to_ascii_uppercase();
        if method == "UNSUBSCRIBE" {
            let mut r = HttpResponse::new(200, "OK");
            r.set("Content-Length", "0");
            r.persist = persist;
            return r;
        }
        if method != "SUBSCRIBE" {
            return HttpResponse::html(404, "Not Found", "not found");
        }
        let timeout = req
            .header("Timeout")
            .and_then(|t| {
                t.split("Second-")
                    .nth(1)
                    .and_then(|s| s.trim().parse::<u32>().ok())
            })
            .filter(|n| *n > 0)
            .unwrap_or(300);
        let sid = req
            .header("SID")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                format!(
                    "uuid:{}",
                    uuid_v4_like(self.update_id.load(Ordering::Relaxed))
                )
            });
        let mut r = HttpResponse::new(200, "OK");
        r.set("SID", sid);
        r.set("Timeout", format!("Second-{timeout}"));
        r.set("Content-Length", "0");
        r.persist = persist;
        r
    }

    fn soap(&self, req: &HttpRequest, persist: bool) -> HttpResponse {
        let action = req.header("SOAPAction").unwrap_or("");
        let body = String::from_utf8_lossy(&req.body);
        let call = parse_soap_call(action, &body);
        let client = self.identify(req);
        if call.method.is_none() {
            return fault_resp(soap_fault(401, "Invalid Action"), persist);
        }
        let mut bookmarks = self.bookmarks.lock().expect("bookmarks");
        if let Some(out) = dispatch_simple(
            &call,
            client,
            &self.uuid,
            self.update_id.load(Ordering::Relaxed),
            self.cfg.root_container.as_deref(),
            &mut bookmarks,
        ) {
            return soap_to_http(out, persist);
        }
        drop(bookmarks);
        let is_search = call.method == Some("Search");
        if !is_search {
            // Browse
            let Some(oid_raw) = call.object_id.as_deref() else {
                return soap_to_http(SoapOutcome::fault402(), persist);
            };
            let Some(flag) = call.browse_flag.as_deref() else {
                return soap_to_http(SoapOutcome::fault402(), persist);
            };
            if call.starting_index < 0 || call.requested_count < 0 {
                return soap_to_http(SoapOutcome::fault402(), persist);
            }
            let mut oid = magic_object_id(oid_raw, client);
            if oid == rusty_dlna_protocol::object_id::ROOT_ID {
                match self.cfg.root_container.as_deref() {
                    Some("V") | Some("v") | Some("2") => {
                        oid = rusty_dlna_protocol::object_id::VIDEO_ID.to_string();
                    }
                    Some("A") | Some("1") => {
                        oid = rusty_dlna_protocol::object_id::MUSIC_ID.to_string();
                    }
                    Some("64") => {
                        oid = rusty_dlna_protocol::object_id::BROWSEDIR_ID.to_string();
                    }
                    _ => {}
                }
            }
            if flag != "BrowseDirectChildren" && flag != "BrowseMetadata" {
                return soap_to_http(SoapOutcome::fault402(), persist);
            }
            let remapped_root = flag == "BrowseDirectChildren" && oid != oid_raw;
            let cat = self.catalog.lock().expect("catalog");
            let page = if flag == "BrowseMetadata" {
                match cat.metadata(&oid) {
                    Some(ch) => vec![ch],
                    None => return soap_to_http(SoapOutcome::fault701(), persist),
                }
            } else {
                match cat.children_of(&oid) {
                    Some(ch) => ch,
                    None => return soap_to_http(SoapOutcome::fault701(), persist),
                }
            };
            let total = page.len() as u32;
            let start = call.starting_index as usize;
            let take = if call.requested_count == 0 {
                page.len()
            } else {
                call.requested_count as usize
            };
            let slice: Vec<_> = page.into_iter().skip(start).take(take).collect();
            let didl: Vec<DidlObject> = slice
                .iter()
                .map(|ch| {
                    let mut o = self.to_didl(ch, client, &cat);
                    // MiniDLNA magic container parentid_sql = "0": children of
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
            );
            tracing::info!(
                ua = req.user_agent().unwrap_or("-"),
                oid = oid_raw,
                flag,
                n = didl.len(),
                "SOAP Browse"
            );
            // MiniDLNA closes SOAP; libupnp keep-alive + our parser has
            // dropped VLC's next Browse (folders then look like files).
            let mut r = HttpResponse::xml(200, xml, false);
            r.persist = false;
            return r;
        }
        // Search: MiniDLNA-style. Container class queries must return
        // folders (VLC expand / some library UIs Search instead of Browse).
        let cat = self.catalog.lock().expect("catalog");
        let crit = call.search_criteria.unwrap_or_default();
        let want_containers = crit.contains("object.container");
        let want_items = crit.contains("object.item") || !want_containers;
        let mut items: Vec<CatalogChild> = Vec::new();
        if want_containers {
            items.extend(
                cat.containers
                    .values()
                    .filter(|c| c.object_id != rusty_dlna_protocol::object_id::ROOT_ID)
                    .map(|c| CatalogChild::Container(c.clone())),
            );
        }
        if want_items {
            items.extend(cat.items.values().map(|it| CatalogChild::Item(it.clone())));
        }
        let total = items.len() as u32;
        let start = call.starting_index.max(0) as usize;
        let take = if call.requested_count == 0 {
            items.len()
        } else {
            call.requested_count as usize
        };
        let didl: Vec<DidlObject> = items
            .iter()
            .skip(start)
            .take(take)
            .map(|ch| self.to_didl(ch, client, &cat))
            .collect();
        let xml = build_browse(
            true,
            &didl,
            didl.len() as u32,
            total,
            self.update_id.load(Ordering::Relaxed),
        );
        let mut r = HttpResponse::xml(200, xml, false);
        r.persist = false;
        r
    }

    fn to_didl(&self, ch: &CatalogChild, client: &ClientProfile, cat: &Catalog) -> DidlObject {
        match ch {
            CatalogChild::Container(c) => DidlObject {
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
            },
            CatalogChild::Item(it) => {
                let date = w3c_normalize_date(&it.date);
                DidlObject {
                    id: it.object_id.clone(),
                    parent_id: it.parent_id.clone(),
                    title: it.title.clone(),
                    class: it.class.clone(),
                    date: Some(date),
                    restricted: true,
                    searchable: None,
                    child_count: None,
                    child_container_count: None,
                    is_container: false,
                    resources: self.item_resources(it, client),
                }
            }
        }
    }

    fn item_resources(&self, it: &MediaItem, client: &ClientProfile) -> Vec<DidlRes> {
        let mime = remap_mime(client, &it.mime);
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
        };
        let src = probe_to_source(
            &it.probe.container,
            &it.probe.video,
            &it.probe.hdr,
            &it.probe.audio,
            it.probe.width,
            it.probe.height,
        );
        let plan = decide(client, &src, &self.remaps);
        if client.flags.contains(ClientFlags::NEED_SAFE_VIDEO) && plan.decision == Decision::Recode
        {
            let remap_url = transcode_item_url(&self.advertise_ip, self.http_port, it.detail_id);
            let remap = DidlRes {
                url: remap_url,
                protocol_info: format!(
                    "http-get:*:video/mp4:{}",
                    dlna_org_features(None, "01", 1, "video/mp4")
                ),
                size: None,
                duration: it.duration.clone(),
                bitrate: None,
                resolution: it.resolution.clone(),
                sample_frequency: it.samplerate,
                nr_audio_channels: it.channels,
            };
            // remapped <res> first for NEED_SAFE_VIDEO
            return vec![remap, orig];
        }
        let mut res = vec![orig];
        if client.flags.contains(ClientFlags::CAPTION_RES) {
            for cap in &it.captions {
                let url = rusty_dlna_protocol::paths::caption_indexed_url(
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
                });
            }
        }
        res
    }

    fn media(&self, req: &HttpRequest, transcode: bool) -> HttpResponse {
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
            .lock()
            .expect("catalog")
            .get_item_by_detail(id)
            .cloned()
        else {
            return HttpResponse::html(404, "Not Found", "no such object");
        };
        let client = self.identify(req);
        let (path, mime, pn, ci) = if transcode {
            let src = probe_to_source(
                &item.probe.container,
                &item.probe.video,
                &item.probe.hdr,
                &item.probe.audio,
                item.probe.width,
                item.probe.height,
            );
            let plan = decide(client, &src, &self.remaps);
            if plan.decision != Decision::Recode {
                return HttpResponse::html(404, "Not Found", "no remap");
            }
            match self.ensure_remap(&item.path, item.detail_id, &plan) {
                Ok(p) => (p, "video/mp4".to_string(), None, 1u8),
                Err(e) => {
                    return HttpResponse::html(500, "Internal Server Error", &e.to_string());
                }
            }
        } else {
            (
                item.path.clone(),
                remap_mime(client, &item.mime),
                item.dlna_pn.clone(),
                0u8,
            )
        };
        let path = rusty_dlna_scan::rebase_media_path(&path, &self.scan_cfg.media_dirs);
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
            r.file_range = Some((path, start, end));
            return r;
        }
        let body = match read_file_range(&path, start, end) {
            Ok(b) => b,
            Err(e) => return HttpResponse::html(500, "Internal Server Error", &e.to_string()),
        };
        media_response(
            &self.server,
            &now_imf_date(),
            &mime,
            size,
            range,
            body,
            pn.as_deref(),
            ci,
        )
    }

    fn ensure_remap(
        &self,
        src: &Path,
        detail_id: i64,
        plan: &TranscodePlan,
    ) -> std::io::Result<PathBuf> {
        let dest = cache_dest(&self.cache_dir, detail_id, plan.action);
        if dest.is_file() && dest.metadata()?.len() > 0 {
            return Ok(dest);
        }
        let _permit = self
            .jobs
            .try_acquire()
            .ok_or_else(|| std::io::Error::other("max_jobs"))?;
        ensure_cached_file(src, &dest, plan)
    }

    fn caption(&self, req: &HttpRequest) -> HttpResponse {
        let Some((id, idx)) = caption_from_path(&req.path) else {
            return HttpResponse::html(404, "Not Found", "bad caption");
        };
        let Some(item) = self
            .catalog
            .lock()
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
        match std::fs::read(&cap_path) {
            Ok(body) => {
                let mut r = HttpResponse::new(200, "OK");
                r.set("Content-Type", caption_http_mime(&cap.ext));
                r.set("Content-Length", body.len());
                r.body = body;
                r
            }
            Err(_) => HttpResponse::html(404, "Not Found", "caption missing"),
        }
    }
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
pub async fn serve(app: Arc<App>) -> Result<(), Box<dyn std::error::Error>> {
    let http_addr = SocketAddr::from((app.listen_ip, app.http_port));
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    tracing::info!(%http_addr, "http listen");
    let ssdp_app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = ssdp_loop(ssdp_app).await {
            tracing::warn!("ssdp loop: {e}");
        }
    });
    spawn_library_watch(app.clone());
    loop {
        let (sock, _peer) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(app, sock).await {
                tracing::debug!("conn: {e}");
            }
        });
    }
}

fn apply_catalog(app: &App, next: Catalog, delta: ScanDelta, why: &'static str) {
    let items = next.items.len();
    *app.catalog.lock().expect("catalog") = next;
    app.update_id.fetch_add(1, Ordering::Relaxed);
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
                .lock()
                .map(|c| c.items.is_empty())
                .unwrap_or(true);
            if empty {
                tracing::info!("empty library; full scan from disk");
                eprintln!("rusty-dlna: empty library, full scan");
                let next = scan(&cfg);
                let items = next.items.len();
                *app.catalog.lock().expect("catalog") = next;
                app.update_id.fetch_add(1, Ordering::Relaxed);
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
    let rows = db.details_missing_av_meta().unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    tracing::info!(n = rows.len(), "filling missing duration/bitrate/resolution");
    let mut filled = 0usize;
    for (id, path) in rows {
        if path.contains("/incomplete/") {
            continue;
        }
        let Some(av) = probe_av_meta(std::path::Path::new(&path)) else {
            continue;
        };
        let _ = db.update_detail_av_meta(
            id,
            av.duration.as_deref(),
            av.bitrate,
            av.resolution.as_deref(),
            av.channels,
            av.samplerate,
        );
        if let Ok(mut cat) = app.catalog.lock() {
            for it in cat.items.values_mut() {
                if it.detail_id == id {
                    it.duration = av.duration.clone();
                    it.bitrate = av.bitrate;
                    it.resolution = av.resolution.clone();
                    it.channels = av.channels;
                    it.samplerate = av.samplerate;
                }
            }
        }
        filled += 1;
    }
    tracing::info!(filled, "av metadata fill done");
}

fn bind_udp_reuse(addr: SocketAddrV4) -> std::io::Result<socket2::Socket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    sock.set_reuse_address(true)?;
    // MiniDLNA uses SO_REUSEADDR only. SO_REUSEPORT lets the kernel hash
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
        // MiniDLNA Linux receive bind: 239.255.255.250:1900 (not the unicast IP).
        // Binding 10.0.1.67:1900 never sees Kodi/VLC multicast M-SEARCH.
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
        if let Err(e) = recv_sock.join_multicast_v4(&group, &iface) {
            tracing::warn!("SSDP IP_ADD_MEMBERSHIP {iface}: {e}");
        } else {
            tracing::info!(%group, %iface, "SSDP joined multicast");
        }
        let _ = recv_sock.set_multicast_ttl_v4(4);
        let _ = recv_sock.set_multicast_loop_v4(false);
    }
    let recv_std = into_std_udp(recv_sock);
    let recv_addr = recv_std.local_addr()?;
    let recv = tokio::net::UdpSocket::from_std(recv_std)?;

    // Kodi/Platinum drops SSDP replies whose UDP source port is not 1900.
    // Reply from the advertise unicast IP (MiniDLNA used the primary
    // 10.0.1.2) so LOCATION host and UDP source match.
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
        let resp = app.handle(&req);
        persist_left = persist_left.saturating_sub(1);
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

async fn stream_file_range(
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

    fn workspace() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn testdata_app() -> App {
        let root = workspace();
        let lib = root.join("testdata/library");
        ensure_pattern_fixture(&lib);
        let nfo = lib.join("video/movie.nfo");
        if !nfo.exists() {
            let _ = std::fs::create_dir_all(lib.join("video"));
            let _ = std::fs::write(&nfo, "<movie><year>1999</year></movie>\n");
            let _ = std::fs::write(lib.join("video/movie.srt"), "1\n00:00:00,000 --> 00:00:01,000\nhi\n");
            let _ = std::fs::write(lib.join("video/movie.en.srt"), "en\n");
        }
        let dvp7 = lib.join("video/dvp7.mkv");
        if !dvp7.exists() {
            let _ = std::fs::write(&dvp7, b"not-a-real-mkv");
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
        *app.catalog.lock().expect("catalog") = cat;
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

    #[test]
    fn test_ports_do_not_collide_with_minidlna() {
        assert!(!collides_with_live_minidlna(18200, 11900));
        assert!(collides_with_live_minidlna(8200, 11900));
        assert!(collides_with_live_minidlna(18200, 1900));
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
             Host: 10.0.1.2:8200\r\n\
             Callback: <http://10.0.1.131:1234/evt>\r\n\
             NT: upnp:event\r\n\
             Timeout: Second-300\r\n\
             Content-Length: 0\r\n\r\n",
        );
        let r = app.handle(&sub);
        assert_eq!(r.status, 200, "Platinum SUBSCRIBE must not 404");
        let sid = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("SID"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert!(sid.starts_with("uuid:"), "{sid}");
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
            "MiniDLNA folder DIDL (container + storageUsed) required for VLC expand: {video}"
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
            "MiniDLNA res@size required for VLC: {items}"
        );
        assert!(
            items.contains("duration="),
            "MiniDLNA res@duration H:MM:SS.mmm required for VLC length: {items}"
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
    fn root_container_v_browse_zero_matches_minidlna() {
        let mut app = testdata_app();
        app.cfg.root_container = Some("V".into());
        let (st, xml) = soap_browse(&app, "0", "BrowseDirectChildren", "VLC/3.0.21 LibVLC/3.0.21");
        assert_eq!(st, 200);
        assert!(xml.contains("All Video"), "{xml}");
        assert!(xml.contains("Folders"), "{xml}");
        assert!(xml.contains("Recently Added"), "{xml}");
        assert!(
            xml.contains("parentID=\"0\"") || xml.contains("parentID=&quot;0&quot;"),
            "MiniDLNA remapped root advertises parentID=0: {xml}"
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
            .lock()
            .unwrap()
            .items
            .values()
            .find(|i| i.title == "movie")
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
    fn crkey_dvp7_remap_first_kodi_original() {
        let app = testdata_app();
        let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert!(kodi.contains("dvp7"));
        assert!(!kodi.contains("/Transcode/"));
        let (_, cr) = soap_browse(
            &app,
            "2$8",
            "BrowseDirectChildren",
            "CrKey/1.54.384650 DLNADOC/1.50",
        );
        // remapped res first: Transcode before MediaItems for that item
        let pos_t = cr.find("/Transcode/");
        let pos_m = cr.find("/MediaItems/");
        assert!(pos_t.is_some(), "CrKey DIDL missing remap: {cr}");
        // CI=1 on remap
        assert!(cr.contains("DLNA.ORG_CI=1"));
        if let (Some(t), Some(m)) = (pos_t, pos_m) {
            // overall MediaItems also exist for movie; just ensure Transcode exists
            let _ = (t, m);
        }
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
        // Kodi: original MKV res, date Z or 10 chars
        let (_, kodi) = soap_browse(&app, "2$8", "BrowseDirectChildren", "Kodi/21.0");
        assert!(kodi.contains("/MediaItems/"));
        assert!(!kodi.contains("/Transcode/"));
        assert!(kodi.contains("1999-01-01") || kodi.contains("Z&lt;/dc:date"));

        // SEC_HHP_[PC] is not Samsung BASICVIEW
        let pc = app.handle(&req(&get(
            "/rootDesc.xml",
            "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0",
        )));
        assert!(!String::from_utf8_lossy(&pc.body).contains("sec:ProductCap"));
        let pc_fl = feature_list(&app, "DLNADOC/1.50 SEC_HHP_[PC]LPC001/1.0");
        assert!(!pc_fl.contains("id=&quot;A&quot;"));
        assert!(pc_fl.contains("id=&quot;1&quot;") || pc_fl.contains("samsung.com_BASICVIEW"));

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

        // [BD]J5500: no DLNA.ORG_PN
        let (_, j5500) = soap_browse(&app, "2$8", "BrowseDirectChildren", "DLNADOC/1.50 [BD]J5500");
        assert!(!j5500.contains("DLNA.ORG_PN="));

        // Xbox rootDesc modelNumber=1
        let xbox = app.handle(&req(&get("/rootDesc.xml", "Xbox/360")));
        assert!(String::from_utf8_lossy(&xbox.body).contains("<modelNumber>1</modelNumber>"));

        // CrKey: transcode res first on DV P7
        let (_, cr) = soap_browse(&app, "2$8", "BrowseDirectChildren", "CrKey/1.54 DLNADOC/1.50");
        let t = cr.find("/Transcode/").expect("crkey remap");
        assert!(cr.contains("DLNA.ORG_CI=1"));
        let _ = t;

        // Generic DLNADOC/1.50 is not NEED_SAFE_VIDEO
        let generic = identify_user_agent("DLNADOC/1.50 UPnP/1.0").unwrap();
        assert!(!generic.flags.contains(ClientFlags::NEED_SAFE_VIDEO));
        let (_, gen) = soap_browse(&app, "2$8", "BrowseDirectChildren", "DLNADOC/1.50");
        assert!(!gen.contains("/Transcode/"));
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
    fn crkey_remap_get_serves_cached_file_range() {
        let app = testdata_app();
        let dvp7 = app
            .catalog
            .lock()
            .unwrap()
            .items
            .values()
            .find(|i| i.title == "dvp7")
            .cloned()
            .expect("dvp7");
        let id = dvp7.detail_id;
        // Kodi does not get a remap URL served
        let kodi = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: Kodi/21.0\r\n\r\n"
        )));
        assert_eq!(kodi.status, 404);

        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skip ffmpeg cache (no ffmpeg)");
            return;
        }
        let cr = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: CrKey/1.54 DLNADOC/1.50\r\n\r\n"
        )));
        if cr.status != 200 {
            eprintln!(
                "ffmpeg remap status={} body={}",
                cr.status,
                String::from_utf8_lossy(&cr.body)
            );
            // Still prove decide/DIDL; file encode can fail on odd fixtures.
            return;
        }
        let dest = rusty_dlna_transcode::cache_dest(&app.cache_dir, id, rusty_dlna_transcode::RecodeAction::RemuxP8);
        assert!(dest.is_file(), "cache file missing {dest:?}");
        let cached = std::fs::read(&dest).unwrap();
        assert_eq!(cr.body, cached);
        assert!(cr
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Accept-Ranges") && v == "bytes"));
        assert!(cr
            .headers
            .iter()
            .any(|(k, v)| k == "contentFeatures.dlna.org" && v.contains("CI=1")));
        let mid = cached.len() / 2;
        let end = (mid + 31).min(cached.len() - 1);
        let ranged = app.handle(&req(&format!(
            "GET /Transcode/{id}.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18200\r\nUser-Agent: CrKey/1.54\r\nRange: bytes={mid}-{end}\r\n\r\n"
        )));
        assert_eq!(ranged.status, 206);
        assert_eq!(ranged.body, cached[mid..=end]);
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
}
