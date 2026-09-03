//! Fixed-cardinality runtime telemetry exposed through `/api/status`.
//!
//! Metrics intentionally use atomics and bounded label sets. Request paths,
//! user agents, object IDs, and other attacker-controlled values never become
//! labels, so a long-running server cannot grow this state without bound.

use std::array;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use rusty_dlna_http::HttpRoute;
use serde_json::{json, Map, Value};

const LATENCY_BOUNDS_MS: [u64; 10] = [1, 5, 10, 25, 50, 100, 250, 500, 1_000, 5_000];
const ROUTE_COUNT: usize = HttpRoute::COUNT;
const SOAP_ACTION_COUNT: usize = 14;
const SOAP_FAULT_COUNT: usize = 12;

fn route_name(route: HttpRoute) -> &'static str {
    match route {
        HttpRoute::RootDesc => "root_description",
        HttpRoute::ScpdContentDir => "scpd_content_directory",
        HttpRoute::ScpdConnectionMgr => "scpd_connection_manager",
        HttpRoute::ScpdRegistrar => "scpd_registrar",
        HttpRoute::Soap => "soap",
        HttpRoute::EventContentDir => "event_content_directory",
        HttpRoute::EventConnectionMgr => "event_connection_manager",
        HttpRoute::EventRegistrar => "event_registrar",
        HttpRoute::MediaItem => "media_item",
        HttpRoute::Transcode => "transcode",
        HttpRoute::Thumbnail => "thumbnail",
        HttpRoute::AlbumArt => "album_art",
        HttpRoute::Resized => "resized",
        HttpRoute::Icon => "icon",
        HttpRoute::Caption => "caption",
        HttpRoute::Status => "status",
        HttpRoute::Health => "health",
        HttpRoute::ApiStatus => "api_status",
        HttpRoute::WebLibrary => "web_library",
        HttpRoute::WebItem => "web_item",
        HttpRoute::WebTranscodeStatus => "web_transcode_status",
        HttpRoute::WebMedia => "web_media",
        HttpRoute::WebAsset => "web_asset",
        HttpRoute::Presentation => "presentation",
        HttpRoute::WebDownload => "web_download",
        HttpRoute::NotFound => "not_found",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComponentState {
    NotStarted = 0,
    Running = 1,
    Stopping = 2,
    Stopped = 3,
    Failed = 4,
}

impl ComponentState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
struct AtomicComponentState(AtomicU8);

impl Default for AtomicComponentState {
    fn default() -> Self {
        Self(AtomicU8::new(ComponentState::NotStarted as u8))
    }
}

impl AtomicComponentState {
    fn load(&self) -> ComponentState {
        match self.0.load(Ordering::Acquire) {
            1 => ComponentState::Running,
            2 => ComponentState::Stopping,
            3 => ComponentState::Stopped,
            4 => ComponentState::Failed,
            _ => ComponentState::NotStarted,
        }
    }

    fn store(&self, state: ComponentState) {
        self.0.store(state as u8, Ordering::Release);
    }
}

#[derive(Debug)]
struct Histogram {
    buckets: [AtomicU64; LATENCY_BOUNDS_MS.len() + 1],
    count: AtomicU64,
    sum_ms: AtomicU64,
    max_ms: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_ms: AtomicU64::new(0),
            max_ms: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    fn observe(&self, elapsed: Duration) {
        let millis = rusty_dlna_helper::duration_millis_saturating(elapsed);
        let bucket = LATENCY_BOUNDS_MS
            .iter()
            .position(|bound| millis <= *bound)
            .unwrap_or(LATENCY_BOUNDS_MS.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(millis, Ordering::Relaxed);
        self.max_ms.fetch_max(millis, Ordering::Relaxed);
    }

    fn json(&self) -> Value {
        let mut cumulative = 0u64;
        let mut buckets = Vec::with_capacity(LATENCY_BOUNDS_MS.len() + 1);
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(count.load(Ordering::Relaxed));
            buckets.push(if let Some(bound) = LATENCY_BOUNDS_MS.get(index) {
                json!({ "le_ms": bound, "count": cumulative })
            } else {
                json!({ "le_ms": "+Inf", "count": cumulative })
            });
        }
        json!({
            "count": self.count.load(Ordering::Relaxed),
            "sum_ms": self.sum_ms.load(Ordering::Relaxed),
            "max_ms": self.max_ms.load(Ordering::Relaxed),
            "buckets": buckets,
        })
    }
}

#[derive(Debug, Default)]
struct RouteMetrics {
    responses: [AtomicU64; 6],
    latency: Histogram,
}

impl RouteMetrics {
    fn record(&self, status: u16, elapsed: Duration) {
        let family = usize::from(status / 100).min(5);
        self.responses[family].fetch_add(1, Ordering::Relaxed);
        self.latency.observe(elapsed);
    }

    fn json(&self) -> Value {
        json!({
            "requests_total": self.latency.count.load(Ordering::Relaxed),
            "responses": {
                "other": self.responses[0].load(Ordering::Relaxed),
                "1xx": self.responses[1].load(Ordering::Relaxed),
                "2xx": self.responses[2].load(Ordering::Relaxed),
                "3xx": self.responses[3].load(Ordering::Relaxed),
                "4xx": self.responses[4].load(Ordering::Relaxed),
                "5xx": self.responses[5].load(Ordering::Relaxed),
            },
            "latency_ms": self.latency.json(),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeSnapshot {
    pub http_listener: ComponentState,
    pub ssdp: ComponentState,
    pub remux_supervisor: ComponentState,
    pub accepted_connections: u64,
    pub active_connections: usize,
    pub accept_errors: u64,
}

#[derive(Debug)]
pub(crate) struct RuntimeMetrics {
    http_listener: AtomicComponentState,
    ssdp: AtomicComponentState,
    remux_supervisor: AtomicComponentState,
    accepted_connections: AtomicU64,
    active_connections: AtomicUsize,
    accept_errors: AtomicU64,
    routes: [RouteMetrics; ROUTE_COUNT],
    soap_actions: [AtomicU64; SOAP_ACTION_COUNT],
    soap_faults: [AtomicU64; SOAP_FAULT_COUNT],
    browse_latency: Histogram,
    search_latency: Histogram,
    shutdown_latency: Histogram,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            http_listener: AtomicComponentState::default(),
            ssdp: AtomicComponentState::default(),
            remux_supervisor: AtomicComponentState::default(),
            accepted_connections: AtomicU64::new(0),
            active_connections: AtomicUsize::new(0),
            accept_errors: AtomicU64::new(0),
            routes: array::from_fn(|_| RouteMetrics::default()),
            soap_actions: array::from_fn(|_| AtomicU64::new(0)),
            soap_faults: array::from_fn(|_| AtomicU64::new(0)),
            browse_latency: Histogram::default(),
            search_latency: Histogram::default(),
            shutdown_latency: Histogram::default(),
        }
    }
}

impl RuntimeMetrics {
    pub(crate) fn set_http_listener(&self, state: ComponentState) {
        self.http_listener.store(state);
    }

    pub(crate) fn set_ssdp(&self, state: ComponentState) {
        self.ssdp.store(state);
    }

    pub(crate) fn set_remux_supervisor(&self, state: ComponentState) {
        self.remux_supervisor.store(state);
    }

    pub(crate) fn connection_opened(&self) {
        self.accepted_connections.fetch_add(1, Ordering::Relaxed);
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn connection_closed(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn accept_error(&self) {
        self.accept_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_request(
        &self,
        route: HttpRoute,
        status: u16,
        elapsed: Duration,
        soap_action_header: Option<&str>,
        response_body: &[u8],
    ) {
        self.routes[route.index()].record(status, elapsed);
        if route != HttpRoute::Soap {
            return;
        }
        let action = soap_action_index(soap_action_header);
        self.soap_actions[action].fetch_add(1, Ordering::Relaxed);
        match action {
            0 => self.browse_latency.observe(elapsed),
            1 => self.search_latency.observe(elapsed),
            _ => {}
        }
        if status >= 400 {
            let code = std::str::from_utf8(response_body)
                .ok()
                .and_then(|body| rusty_dlna_soap::xml_tag_text(body, "errorCode"))
                .and_then(|code| code.parse::<u16>().ok());
            self.soap_faults[soap_fault_index(code)].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_shutdown(&self, elapsed: Duration) {
        self.shutdown_latency.observe(elapsed);
    }

    pub(crate) fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            http_listener: self.http_listener.load(),
            ssdp: self.ssdp.load(),
            remux_supervisor: self.remux_supervisor.load(),
            accepted_connections: self.accepted_connections.load(Ordering::Relaxed),
            active_connections: self.active_connections.load(Ordering::Relaxed),
            accept_errors: self.accept_errors.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn json(&self) -> Value {
        let mut routes = Map::new();
        for route in HttpRoute::ALL {
            routes.insert(
                route_name(route).to_string(),
                self.routes[route.index()].json(),
            );
        }
        let mut actions = Map::new();
        for (index, name) in SOAP_ACTION_NAMES.iter().enumerate() {
            actions.insert(
                (*name).to_string(),
                json!(self.soap_actions[index].load(Ordering::Relaxed)),
            );
        }
        let mut faults = Map::new();
        for (index, name) in SOAP_FAULT_NAMES.iter().enumerate() {
            faults.insert(
                (*name).to_string(),
                json!(self.soap_faults[index].load(Ordering::Relaxed)),
            );
        }
        let runtime = self.snapshot();
        json!({
            "http": {
                "listener_state": runtime.http_listener.as_str(),
                "accepted_connections_total": runtime.accepted_connections,
                "active_connections": runtime.active_connections,
                "accept_errors_total": runtime.accept_errors,
                "routes": routes,
            },
            "runtime": {
                "ssdp_state": runtime.ssdp.as_str(),
                "remux_supervisor_state": runtime.remux_supervisor.as_str(),
            },
            "soap": {
                "actions_total": actions,
                "faults_total": faults,
                "browse_latency_ms": self.browse_latency.json(),
                "search_latency_ms": self.search_latency.json(),
            },
            "shutdown": {
                "duration_ms": self.shutdown_latency.json(),
            },
        })
    }
}

const SOAP_ACTION_NAMES: [&str; SOAP_ACTION_COUNT] = [
    "Browse",
    "Search",
    "GetSearchCapabilities",
    "GetSortCapabilities",
    "GetSystemUpdateID",
    "GetProtocolInfo",
    "GetCurrentConnectionIDs",
    "GetCurrentConnectionInfo",
    "IsAuthorized",
    "IsValidated",
    "RegisterDevice",
    "X_SetBookmark",
    "UpdateObject",
    "unknown",
];

const SOAP_FAULT_NAMES: [&str; SOAP_FAULT_COUNT] = [
    "401",
    "402",
    "501",
    "701",
    "702",
    "703",
    "705",
    "706",
    "708",
    "709",
    "other",
    "unparseable",
];

fn soap_action_index(header: Option<&str>) -> usize {
    let action = header
        .map(str::trim)
        .map(|value| value.trim_matches('"'))
        .and_then(|value| value.rsplit('#').next())
        .unwrap_or("");
    SOAP_ACTION_NAMES
        .iter()
        .position(|known| *known == action)
        .unwrap_or(SOAP_ACTION_COUNT - 1)
}

fn soap_fault_index(code: Option<u16>) -> usize {
    match code {
        Some(401) => 0,
        Some(402) => 1,
        Some(501) => 2,
        Some(701) => 3,
        Some(702) => 4,
        Some(703) => 5,
        Some(705) => 6,
        Some(706) => 7,
        Some(708) => 8,
        Some(709) => 9,
        Some(_) => 10,
        None => 11,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histograms_are_cumulative_and_fixed_size() {
        let histogram = Histogram::default();
        histogram.observe(Duration::from_millis(0));
        histogram.observe(Duration::from_millis(25));
        histogram.observe(Duration::from_millis(9_000));
        let value = histogram.json();
        assert_eq!(value["count"], 3);
        assert_eq!(value["max_ms"], 9_000);
        assert_eq!(value["buckets"].as_array().unwrap().len(), 11);
        assert_eq!(value["buckets"][10]["count"], 3);
    }

    #[test]
    fn request_labels_never_include_paths_or_unknown_actions() {
        let metrics = RuntimeMetrics::default();
        metrics.record_request(
            HttpRoute::NotFound,
            404,
            Duration::from_millis(2),
            None,
            b"not found",
        );
        metrics.record_request(
            HttpRoute::Soap,
            500,
            Duration::from_millis(3),
            Some("\"urn:test#AttackerControlled\""),
            b"<errorCode>12345</errorCode>",
        );
        metrics.record_request(
            HttpRoute::WebItem,
            200,
            Duration::from_millis(1),
            None,
            b"{}",
        );
        metrics.record_shutdown(Duration::from_millis(42));
        let value = metrics.json();
        assert_eq!(value["http"]["routes"]["not_found"]["responses"]["4xx"], 1);
        assert_eq!(value["http"]["routes"]["web_item"]["responses"]["2xx"], 1);
        assert_eq!(value["soap"]["actions_total"]["unknown"], 1);
        assert_eq!(value["soap"]["faults_total"]["other"], 1);
        assert_eq!(value["shutdown"]["duration_ms"]["count"], 1);
        assert_eq!(value["shutdown"]["duration_ms"]["max_ms"], 42);
        assert!(!value.to_string().contains("AttackerControlled"));
    }
}
