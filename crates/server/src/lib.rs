//! Accept loop, SOAP Browse, original GET/`Range`, and background remux.
//! Listen ports come from `RUSTY_DLNA_HTTP_PORT` / `RUSTY_DLNA_SSDP_PORT`.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rusty_dlna_http::{
    caption_info_sec_url, dlna_get_header_invalid, dlna_org_features, dlna_strict, gen_root_desc,
    interactive_on_non_image, is_chunked, live_transcode_response, media_response, now_imf_date,
    parse_byte_range, persist_for_route, protocol_info, realtime_interactive_invalid, route,
    scpd_connection_manager, scpd_content_directory, scpd_registrar, set_caption_info_sec,
    streaming_on_image, timeseek_without_range, valid_host_header, wants_caption_info_sec,
    wants_content_language, ByteRange, HttpRequest, HttpResponse, HttpRoute, MediaResponseOptions,
    OpenFileRange, RangeError, RemuxAudio, RemuxJobSpec, RootDescOpts,
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
    load_catalog_with_policy, load_existing, monitor_incremental, repair_objects_if_needed,
    repair_video_titles_if_needed, run_inotify_updates_until, scan, Catalog, CatalogChild,
    CatalogUpdate, Container as CatalogContainer, HelperGate, LibraryDb, MediaItem, MediaTypes,
    ScanConfig, ScanDelta, ScanProgress, ScanResult, SourceProbe, WatchTelemetry,
};

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
    hdr10_fallback_plan, pick_audio_index_from_streams, probe_to_source, source_identity_file,
    transcode_cache_key_file, AudioAction, Decision, JobGate, RecodeAction, RemapRule,
    TranscodePlan,
};

mod catalog_query;
mod config;
mod events;
mod http_app;
mod lifecycle;
mod metrics;
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
pub(crate) use http_app::available_filesystem_bytes;
#[cfg(test)]
use http_app::{
    derived_image_key, fetch_renderer_description, prune_derived_image_cache, renderer_xml_body,
    MAX_RENDERER_DESCRIPTION_BYTES,
};
use http_app::{
    sniff_renderer_location, trusted_renderer_location, RendererFetchLimiter, SsdpReplyLimiter,
};
pub use lifecycle::serve;
#[cfg(test)]
use lifecycle::{
    accept_loop, apply_catalog, handle_conn, next_reconcile_interval_secs,
    reply_interface_for_sender, spawn_library_watch, stop_library_watch, stop_library_watch_until,
    ReconcileOutcome,
};
use lifecycle::{
    active_ipv4_interfaces, default_route_interface, os_version, read_open_file_range,
    select_advertise_ip, select_ssdp_interfaces, unix_now, usable_lan_ipv4, InterfaceV4,
};
pub(crate) use lifecycle::{socket_write_all, stream_file_range};
use metrics::{ComponentState, RuntimeMetrics};
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
    pub(crate) helpers: Arc<HelperGate>,
    pub(crate) remuxes: Mutex<HashMap<String, Arc<remux::RemuxJob>>>,
    pub(crate) remux_metrics: remux::RemuxMetrics,
    events: Arc<Mutex<events::EventHub>>,
    notify_dispatcher: events::NotifyDispatcher,
    derived_image_locks: Vec<Mutex<()>>,
    pub(crate) client_cache: Mutex<ClientCache>,
    scan_control: Arc<ScanControl>,
    scan_telemetry: Arc<WatchTelemetry>,
    db_pool: Option<Arc<DbPool>>,
    catalog_query_cache: Mutex<CatalogQueryCache>,
    db_integrity: Arc<DbIntegrityCache>,
    required_tools_ready: bool,
    runtime_metrics: RuntimeMetrics,
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
    reconcile_interval_secs: Option<u64>,
}

#[derive(Debug, Default)]
struct ScanControl {
    cancellation: rusty_dlna_scan::CancellationToken,
    gate: Mutex<()>,
    sleep: Mutex<()>,
    wake: std::sync::Condvar,
    threads: Mutex<Vec<ScanWorker>>,
    state: Mutex<ScanRuntimeState>,
}

#[derive(Debug)]
struct ScanWorker {
    role: &'static str,
    handle: std::thread::JoinHandle<()>,
}

struct DbPool {
    readers: Mutex<Vec<LibraryDb>>,
    reader_available: std::sync::Condvar,
    writer: Mutex<LibraryDb>,
    reader_count: usize,
    read_active: AtomicUsize,
    read_waiters: AtomicUsize,
    writer_active: AtomicBool,
    reads_total: AtomicU64,
    writes_total: AtomicU64,
    errors_total: AtomicU64,
    read_wait_ms_total: AtomicU64,
    read_wait_ms_max: AtomicU64,
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
        let reader_count = readers.len();
        Ok(Self {
            readers: Mutex::new(readers),
            reader_available: std::sync::Condvar::new(),
            writer: Mutex::new(writer),
            reader_count,
            read_active: AtomicUsize::new(0),
            read_waiters: AtomicUsize::new(0),
            writer_active: AtomicBool::new(false),
            reads_total: AtomicU64::new(0),
            writes_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            read_wait_ms_total: AtomicU64::new(0),
            read_wait_ms_max: AtomicU64::new(0),
        })
    }

    fn read<T>(
        &self,
        query: impl FnOnce(&LibraryDb) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let wait_started = Instant::now();
        let mut available = self
            .readers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let waited = available.is_empty();
        if waited {
            self.read_waiters.fetch_add(1, Ordering::Relaxed);
        }
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
        if waited {
            self.read_waiters.fetch_sub(1, Ordering::Relaxed);
            let wait_ms = wait_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            self.read_wait_ms_total
                .fetch_add(wait_ms, Ordering::Relaxed);
            self.read_wait_ms_max.fetch_max(wait_ms, Ordering::Relaxed);
        }
        drop(available);
        self.read_active.fetch_add(1, Ordering::Relaxed);
        let result = query(&db);
        self.read_active.fetch_sub(1, Ordering::Relaxed);
        self.reads_total.fetch_add(1, Ordering::Relaxed);
        if result.is_err() {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }
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
        self.writer_active.store(true, Ordering::Release);
        let result = query(&db);
        self.writer_active.store(false, Ordering::Release);
        self.writes_total.fetch_add(1, Ordering::Relaxed);
        if result.is_err() {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn write_scan<T>(
        &self,
        cancellation: &rusty_dlna_scan::CancellationToken,
        query: impl FnOnce(&LibraryDb) -> rusqlite::Result<T>,
    ) -> ScanResult<T> {
        let db = loop {
            match self.writer.try_lock() {
                Ok(db) => break db,
                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if cancellation.is_cancelled() {
                        return Err(rusty_dlna_scan::ScanError::Cancelled);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        if cancellation.is_cancelled() {
            return Err(rusty_dlna_scan::ScanError::Cancelled);
        }
        db.install_cancellation(cancellation.clone())?;
        self.writer_active.store(true, Ordering::Release);
        let result = query(&db);
        self.writer_active.store(false, Ordering::Release);
        self.writes_total.fetch_add(1, Ordering::Relaxed);
        if result.is_err() {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }
        result.map_err(Into::into)
    }

    fn metrics(&self) -> DbPoolMetrics {
        let readers_available = self
            .readers
            .lock()
            .map(|readers| readers.len())
            .unwrap_or_default();
        DbPoolMetrics {
            reader_count: self.reader_count,
            readers_available,
            read_active: self.read_active.load(Ordering::Relaxed),
            read_waiters: self.read_waiters.load(Ordering::Relaxed),
            writer_active: self.writer_active.load(Ordering::Acquire),
            reads_total: self.reads_total.load(Ordering::Relaxed),
            writes_total: self.writes_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            read_wait_ms_total: self.read_wait_ms_total.load(Ordering::Relaxed),
            read_wait_ms_max: self.read_wait_ms_max.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct DbPoolMetrics {
    reader_count: usize,
    readers_available: usize,
    read_active: usize,
    read_waiters: usize,
    writer_active: bool,
    reads_total: u64,
    writes_total: u64,
    errors_total: u64,
    read_wait_ms_total: u64,
    read_wait_ms_max: u64,
}

const DB_CHECK_REFRESH_SECS: u64 = 300;
const DB_CHECK_STALE_SECS: u64 = 900;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbIntegrityResult {
    Ok,
    Failed,
    Error,
    NotConfigured,
}

impl DbIntegrityResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::NotConfigured => "not_configured",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DbIntegritySnapshot {
    result: DbIntegrityResult,
    checked_unix: u64,
    duration_ms: u64,
    runs_total: u64,
    refresh_in_flight: bool,
}

impl DbIntegritySnapshot {
    fn age_seconds(self) -> u64 {
        unix_now().saturating_sub(self.checked_unix)
    }

    fn stale(self) -> bool {
        self.result != DbIntegrityResult::NotConfigured && self.age_seconds() > DB_CHECK_STALE_SECS
    }
}

#[derive(Debug)]
struct DbIntegrityCache {
    snapshot: Mutex<DbIntegritySnapshot>,
    refresh_in_flight: Arc<AtomicBool>,
}

impl DbIntegrityCache {
    fn new(pool: Option<&Arc<DbPool>>) -> Self {
        let started = Instant::now();
        let result = pool.map_or(DbIntegrityResult::NotConfigured, |pool| {
            check_database_integrity(pool)
        });
        Self {
            snapshot: Mutex::new(DbIntegritySnapshot {
                result,
                checked_unix: unix_now(),
                duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                runs_total: u64::from(pool.is_some()),
                refresh_in_flight: false,
            }),
            refresh_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return the cached result immediately and schedule at most one bounded
    /// asynchronous refresh when its normal refresh age has elapsed.
    fn get(self: &Arc<Self>, pool: Option<Arc<DbPool>>) -> DbIntegritySnapshot {
        let mut snapshot = *self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if snapshot.result != DbIntegrityResult::NotConfigured
            && snapshot.age_seconds() >= DB_CHECK_REFRESH_SECS
            && !self.refresh_in_flight.swap(true, Ordering::AcqRel)
        {
            let Some(pool) = pool else {
                self.refresh_in_flight.store(false, Ordering::Release);
                return snapshot;
            };
            let cache = Arc::clone(self);
            let refreshing = Arc::clone(&self.refresh_in_flight);
            let spawn = std::thread::Builder::new()
                .name("db-quick-check".into())
                .spawn(move || {
                    let started = Instant::now();
                    let result = check_database_integrity(&pool);
                    let duration_ms =
                        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    let mut current = cache
                        .snapshot
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    current.result = result;
                    current.checked_unix = unix_now();
                    current.duration_ms = duration_ms;
                    current.runs_total = current.runs_total.saturating_add(1);
                    drop(current);
                    refreshing.store(false, Ordering::Release);
                });
            if let Err(error) = spawn {
                tracing::warn!(%error, "could not start cached database integrity refresh");
                self.refresh_in_flight.store(false, Ordering::Release);
            }
        }
        snapshot.refresh_in_flight = self.refresh_in_flight.load(Ordering::Acquire);
        snapshot
    }
}

fn check_database_integrity(pool: &DbPool) -> DbIntegrityResult {
    match pool.read(|db| db.quick_check()) {
        Ok(check) if check == "ok" => DbIntegrityResult::Ok,
        Ok(_) => DbIntegrityResult::Failed,
        Err(error) => {
            tracing::error!(%error, "database quick_check failed");
            DbIntegrityResult::Error
        }
    }
}

/// MiniDLNA refuses to grow a SOAP response past roughly two MiB.  Keep the
/// candidate page finite too: `RequestedCount=0` means "as many as possible",
/// not permission to materialize an entire library before applying the byte
/// ceiling.
const MAX_SOAP_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOAP_PAGE_OBJECTS: usize = 4096;

const CATALOG_QUERY_CACHE_ENTRIES: usize = 256;
const CATALOG_QUERY_CACHE_KEY_BYTES: usize = 8 * 1024;

#[derive(Debug)]
struct CatalogQueryCacheEntry {
    generation: u32,
    key: String,
    page: rusty_dlna_scan::CatalogQueryPage,
}

#[derive(Debug, Default)]
struct CatalogQueryCache {
    entries: VecDeque<CatalogQueryCacheEntry>,
}

impl CatalogQueryCache {
    fn get(&mut self, generation: u32, key: &str) -> Option<rusty_dlna_scan::CatalogQueryPage> {
        self.entries.retain(|entry| entry.generation == generation);
        let position = self.entries.iter().position(|entry| entry.key == key)?;
        let entry = self.entries.remove(position)?;
        let page = entry.page.clone();
        self.entries.push_back(entry);
        Some(page)
    }

    fn insert(&mut self, generation: u32, key: String, page: rusty_dlna_scan::CatalogQueryPage) {
        if key.len() > CATALOG_QUERY_CACHE_KEY_BYTES {
            return;
        }
        self.entries
            .retain(|entry| entry.generation == generation && entry.key != key);
        self.entries.push_back(CatalogQueryCacheEntry {
            generation,
            key,
            page,
        });
        while self.entries.len() > CATALOG_QUERY_CACHE_ENTRIES {
            self.entries.pop_front();
        }
    }
}

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

#[cfg(test)]
mod tests;
