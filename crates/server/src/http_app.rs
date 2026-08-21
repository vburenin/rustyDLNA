//! Runtime construction and the request-to-response HTTP application.
#![warn(missing_docs)]

use super::*;

impl App {
    fn cached_catalog_query_page(
        &self,
        generation: u32,
        key: &str,
    ) -> Option<rusty_dlna_scan::CatalogQueryPage> {
        self.catalog_query_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(generation, key)
    }

    fn cache_catalog_query_page(
        &self,
        generation: u32,
        key: String,
        page: rusty_dlna_scan::CatalogQueryPage,
    ) {
        self.catalog_query_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(generation, key, page);
    }

    /// Construct the application or panic when its configuration is invalid.
    pub fn from_config(cfg: Config, http_port: u16, ssdp_port: u16, config_dir: &Path) -> Self {
        Self::try_from_config(cfg, http_port, ssdp_port, config_dir)
            .unwrap_or_else(|error| panic!("invalid rustyDLNA configuration: {error}"))
    }

    /// Validate configuration and construct all owned runtime state.
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
        let helpers = Arc::new(HelperGate::new(
            cfg.helper_max_jobs,
            cfg.helper_queue_capacity,
        ));
        let cancellation = rusty_dlna_scan::CancellationToken::default();
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
            helper_gate: Some(Arc::clone(&helpers)),
            helper_queue_timeout: Duration::from_secs(cfg.helper_queue_timeout_secs),
            cancellation: cancellation.clone(),
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
        let initial_cache_bytes = remux::maintain_transcode_cache(
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
            .map(|path| DbPool::open(path, 4).map(Arc::new))
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
        let db_integrity = Arc::new(DbIntegrityCache::new(db_pool.as_ref()));
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
            helpers,
            remuxes: Mutex::new(HashMap::new()),
            recent_remux_states: Mutex::new(HashMap::new()),
            remux_metrics: remux::RemuxMetrics::new(initial_cache_bytes),
            events,
            notify_dispatcher,
            derived_image_locks: (0..64).map(|_| Mutex::new(())).collect(),
            client_cache: Mutex::new(ClientCache::new()),
            scan_control: Arc::new(ScanControl {
                cancellation,
                ..ScanControl::default()
            }),
            scan_telemetry: Arc::new(WatchTelemetry::default()),
            db_pool,
            catalog_query_cache: Mutex::new(CatalogQueryCache::default()),
            db_integrity,
            required_tools_ready,
            runtime_metrics: RuntimeMetrics::default(),
            #[cfg(test)]
            test_tree: None,
        })
    }

    /// Verify that configured listener ports do not collide with protected live ports.
    pub fn isolation_ok(&self) -> Result<(), String> {
        if collides_with_live_ports(self.http_port, self.ssdp_port) {
            return Err(format!(
                "refusing to bind live ports {}/{} from a test listener; set RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900",
                self.http_port, self.ssdp_port
            ));
        }
        Ok(())
    }

    /// Identify a client using request headers without a peer-address cache key.
    pub fn identify(&self, req: &HttpRequest) -> &'static ClientProfile {
        self.identify_peer(
            req,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9)),
        )
    }

    /// Identify a client and update the bounded per-peer compatibility cache.
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
    /// Handle a request using an unspecified peer address.
    pub fn handle(&self, req: &HttpRequest) -> HttpResponse {
        static SEQ: AtomicU32 = AtomicU32::new(1);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let host = Ipv4Addr::new(127, 0, 0, ((n % 250) + 1) as u8);
        self.handle_from(req, SocketAddr::from((host, 9)))
    }

    /// Shipped request handler. The accept loop passes the real peer.
    /// Route one parsed HTTP request from a known peer.
    pub fn handle_from(&self, req: &HttpRequest, peer: SocketAddr) -> HttpResponse {
        let started = Instant::now();
        let request_route = route(&req.method, &req.path);
        let response = self.handle_from_inner(req, peer);
        self.runtime_metrics.record_request(
            request_route,
            response.status,
            started.elapsed(),
            req.header("SOAPAction"),
            &response.body,
        );
        response
    }

    fn handle_from_inner(&self, req: &HttpRequest, peer: SocketAddr) -> HttpResponse {
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
            HttpRoute::Status => {
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
            HttpRoute::WebLibrary => web_ui::library(self, req),
            HttpRoute::WebItem => web_ui::item(self, req),
            HttpRoute::WebTranscodeStatus => web_ui::transcode_status(self, req),
            HttpRoute::WebMedia => web_ui::media(self, req, peer),
            HttpRoute::WebAsset => web_ui::asset(self, &req.path),
            HttpRoute::Thumbnail => self.thumbnail(req),
            HttpRoute::Resized => self.resized(req),
            HttpRoute::Presentation => web_ui::presentation(self),
            HttpRoute::NotFound => HttpResponse::html(404, "Not Found", "not found"),
        };
        if matches!(
            r,
            HttpRoute::MediaItem | HttpRoute::Transcode | HttpRoute::WebMedia
        ) {
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

    /// GENA subscribe/unsubscribe. Peer IPv4 is required
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
            let query_generation = self.update_id.load(Ordering::Acquire);
            let query_cache_key = format!(
                "browse\u{1f}{oid}\u{1f}{direct_sort:?}\u{1f}{order:?}\u{1f}{start}\u{1f}{take}"
            );
            let db_page = direct_sort.as_ref().and_then(|sort| {
                self.cached_catalog_query_page(query_generation, &query_cache_key)
                    .or_else(|| {
                        query_db_children(
                            self.db_pool.as_deref(),
                            self.scan_cfg.db_path.as_deref(),
                            &oid,
                            sort,
                            order,
                            start,
                            take,
                        )
                    })
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
                    let displayed_total = cat
                        .containers
                        .contains_key(&oid)
                        .then(|| cat.displayed_child_count(&oid));
                    if let Some((objects, total)) = db_page.as_ref().and_then(|page| {
                        (Some(page.total) == displayed_total)
                            .then(|| {
                                materialize_db_page(&cat, page).map(|items| {
                                    self.cache_catalog_query_page(
                                        query_generation,
                                        query_cache_key.clone(),
                                        page.clone(),
                                    );
                                    (items, page.total)
                                })
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
        let query_generation = self.update_id.load(Ordering::Acquire);
        let query_cache_key =
            format!("search\u{1f}{scope}\u{1f}{query:?}\u{1f}{start}\u{1f}{take}");
        let db_page = self
            .cached_catalog_query_page(query_generation, &query_cache_key)
            .or_else(|| {
                query_db_search(
                    self.db_pool.as_deref(),
                    self.scan_cfg.db_path.as_deref(),
                    &scope,
                    &query,
                    start,
                    take,
                )
            });
        let (didl, total) = {
            let cat = read_recover(&self.catalog);
            if let Some(page) = db_page.as_ref() {
                if let Some(objects) = materialize_db_page(&cat, page) {
                    self.cache_catalog_query_page(query_generation, query_cache_key, page.clone());
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

    pub(super) fn to_didl_ref(
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

    pub(crate) fn media(
        &self,
        req: &HttpRequest,
        transcode: bool,
        peer: SocketAddr,
    ) -> HttpResponse {
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
                let opened = match rusty_dlna_scan::open_allowed_file(&src_path, &self.scan_cfg) {
                    Ok(opened) => opened,
                    Err(error) => {
                        tracing::error!(path = %src_path.display(), title = %item.title, %error, "media missing");
                        return HttpResponse::html(404, "Not Found", "missing file");
                    }
                };
                let mut plan = plan;
                plan.audio_index =
                    pick_audio_index_from_streams(&probe.audio_streams, &probe.audio);
                let remux_p8 = plan.action == RecodeAction::RemuxP8;
                let Some(cache_key) =
                    transcode_cache_key_file(&opened.file, &opened.resolved_path, &plan, remux_p8)
                else {
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
                    let mut fallback = hdr10_fallback_plan(&plan);
                    fallback.video_encoder = self.cfg.transcode.encoder.clone();
                    fallback
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
                let args = ffmpeg_grow_os_args(Path::new("/proc/self/fd/3"), &part, &grow_plan);
                let fallback_args = if grow_plan.video_encoder.ends_with("_nvenc") {
                    let mut software = grow_plan.clone();
                    software.video_encoder = "libx264".into();
                    Some(ffmpeg_grow_os_args(
                        Path::new("/proc/self/fd/3"),
                        &part,
                        &software,
                    ))
                } else {
                    None
                };
                let job_key = format!("{}:{cache_key}:{args:?}", item.detail_id);
                let mut r = live_transcode_response("video/mp4");
                r.remux_job = Some(RemuxJobSpec {
                    detail_id: item.detail_id,
                    web_request_id: None,
                    mime: "video/mp4",
                    job_key,
                    cache_key,
                    src: opened.resolved_path.clone(),
                    source_file: Some(Arc::new(opened.file)),
                    dest: dest.clone(),
                    args,
                    fallback_args,
                    continue_after_disconnect: self.cfg.transcode.continue_after_disconnect,
                    cacheable: true,
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
        let strict = dlna_strict(req);
        let samsung = client.flags.contains(ClientFlags::SAMSUNG);
        if streaming_on_image(req, &mime) {
            return HttpResponse::html(406, "Not Acceptable", "Streaming not allowed on image");
        }
        if interactive_on_non_image(req, &mime, samsung, strict) {
            return HttpResponse::html(406, "Not Acceptable", "Interactive not allowed");
        }
        let mut opened = match rusty_dlna_scan::open_allowed_file(&path, &self.scan_cfg) {
            Ok(opened) => opened,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return HttpResponse::html(403, "Forbidden", "path escaped media dir");
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "media missing");
                return HttpResponse::html(404, "Not Found", "missing file");
            }
        };
        let size = match opened.file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "media metadata failed");
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
            r.file_range = Some(OpenFileRange {
                file: Arc::new(opened.file),
                path,
                start,
                end,
            });
            return r;
        }
        let body = match read_open_file_range(&mut opened.file, start, end) {
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
        let opened = self.open_sidecar_file(&src);
        let Ok(opened) = opened else {
            return HttpResponse::html(404, "Not Found", "resize source escaped");
        };
        let stable_src = opened.proc_path();
        let pixels = u64::from(w).checked_mul(u64::from(h));
        if w > self.cfg.derived_image_max_dimension
            || h > self.cfg.derived_image_max_dimension
            || matches!(pixels, Some(pixels) if pixels > self.cfg.derived_image_max_pixels)
        {
            return HttpResponse::html(400, "Bad Request", "resize exceeds configured limits");
        }
        let Some(identity) = source_identity_file(&opened.file, &opened.resolved_path) else {
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
        if !dest.is_file() {
            let _helper_permit = match self.helpers.acquire_timeout_cancelled(
                Duration::from_secs(self.cfg.helper_queue_timeout_secs),
                &self.scan_cfg.cancellation,
            ) {
                Ok(permit) => permit,
                Err(error) => {
                    tracing::warn!(%error, key, "derived-image helper admission rejected");
                    let mut response = HttpResponse::html(
                        503,
                        "Service Unavailable",
                        "media helper capacity exhausted",
                    );
                    response.set("Retry-After", "1");
                    return response;
                }
            };
            let Some(source_pixels) = rusty_dlna_scan::probe_image_with_cancellation(
                &stable_src,
                Duration::from_secs(self.cfg.derived_image_timeout_secs),
                &self.scan_cfg.cancellation,
            )
            .and_then(|image| {
                u64::from(image.probe.width).checked_mul(u64::from(image.probe.height))
            }) else {
                return HttpResponse::html(404, "Not Found", "resize source is not a valid image");
            };
            if source_pixels == 0 || source_pixels > self.cfg.derived_image_max_pixels {
                return HttpResponse::html(
                    413,
                    "Payload Too Large",
                    "source image exceeds pixel limit",
                );
            }
            if !rusty_dlna_scan::scale_jpeg_file_with_options_cancelled_result(
                &opened.file,
                &dest,
                w,
                h,
                self.cfg.derived_image_quality,
                rusty_dlna_scan::MediaHelperControl {
                    timeout: Duration::from_secs(self.cfg.derived_image_timeout_secs),
                    max_alloc_bytes: self.cfg.derived_image_memory_mb * 1024 * 1024,
                    cancellation: &self.scan_cfg.cancellation,
                },
            )
            .unwrap_or(false)
            {
                return HttpResponse::html(404, "Not Found", "resize failed");
            }
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
                if req
                    .query
                    .split('&')
                    .any(|parameter| parameter == "format=webvtt")
                {
                    return web_ui::browser_caption_response(self, &cap.ext, &body);
                }
                let mut r = HttpResponse::new(200, "OK");
                r.set("Content-Type", caption_http_mime(&cap.ext));
                r.set("Content-Length", body.len());
                r.body = body;
                r
            }
            Err(r) => *r,
        }
    }

    fn open_sidecar_file(&self, path: &Path) -> std::io::Result<rusty_dlna_scan::RootedFile> {
        rusty_dlna_scan::open_allowed_file(path, &self.scan_cfg).or_else(|_| {
            let mut internal_roots = vec![self.cache_dir.clone()];
            if let Some(art_dir) = self
                .scan_cfg
                .db_path
                .as_deref()
                .and_then(Path::parent)
                .map(|directory| directory.join("art"))
                .filter(|directory| directory != &self.cache_dir)
            {
                internal_roots.push(art_dir);
            }
            rusty_dlna_scan::open_file_under_roots(path, &internal_roots, false)
        })
    }

    /// Art / captions: regular file under media, cache, or generated-art roots.
    pub(super) fn read_sidecar(&self, path: &Path) -> Result<Vec<u8>, Box<HttpResponse>> {
        let opened = self.open_sidecar_file(path).map_err(|_| {
            Box::new(HttpResponse::html(
                404,
                "Not Found",
                "sidecar escaped or missing",
            ))
        })?;
        if opened
            .file
            .metadata()
            .map(|metadata| metadata.len() > rusty_dlna_scan::MAX_SIDECAR_BYTES)
            .unwrap_or(true)
        {
            return Err(Box::new(HttpResponse::html(
                413,
                "Payload Too Large",
                "sidecar too large",
            )));
        }
        use std::io::Read;
        let mut body = Vec::new();
        if opened
            .file
            .take(rusty_dlna_scan::MAX_SIDECAR_BYTES + 1)
            .read_to_end(&mut body)
            .is_err()
        {
            return Err(Box::new(HttpResponse::html(
                404,
                "Not Found",
                "sidecar missing",
            )));
        }
        if body.len() as u64 > rusty_dlna_scan::MAX_SIDECAR_BYTES {
            return Err(Box::new(HttpResponse::html(
                413,
                "Payload Too Large",
                "sidecar too large",
            )));
        }
        Ok(body)
    }
}

/// HTTP/1.1 requires a dotted-IPv4 Host on every method (SOAP / GENA included).
/// Any present Host must pass rustyDLNA's rebinding check.
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

pub(super) const MAX_RENDERER_DESCRIPTION_BYTES: usize = 256 * 1024;
const MAX_RENDERER_HEADER_BYTES: usize = 16 * 1024;

#[derive(Default)]
pub(super) struct RendererFetchLimiter {
    recent_keys: HashMap<String, std::time::Instant>,
    sender_windows: HashMap<Ipv4Addr, std::collections::VecDeque<std::time::Instant>>,
}

impl RendererFetchLimiter {
    pub(super) fn allow(&mut self, sender: Ipv4Addr, key: &str, now: std::time::Instant) -> bool {
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
pub(super) struct SsdpReplyLimiter {
    senders: HashMap<Ipv4Addr, (std::time::Instant, usize)>,
}

impl SsdpReplyLimiter {
    pub(super) fn allow(
        &mut self,
        sender: Ipv4Addr,
        datagrams: usize,
        now: std::time::Instant,
    ) -> bool {
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

pub(super) fn trusted_renderer_location(
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

pub(super) fn renderer_xml_body(response: &[u8]) -> Option<&[u8]> {
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

pub(super) fn fetch_renderer_description(ip: Ipv4Addr, port: u16, path: &str) -> Option<String> {
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

pub(super) fn sniff_renderer_location(url: &str, server: &str, sender: SocketAddr, app: &App) {
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

pub(super) fn derived_image_key(
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
pub(crate) fn available_filesystem_bytes(path: &Path) -> std::io::Result<u64> {
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
pub(crate) fn available_filesystem_bytes(_path: &Path) -> std::io::Result<u64> {
    Ok(u64::MAX)
}

pub(super) fn prune_derived_image_cache(
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
        SoapOutcome::Fault { http, code, desc } => {
            let mut r = fault_resp(soap_fault(code, desc), persist);
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
