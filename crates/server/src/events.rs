//! In-memory GENA subscriber table and NOTIFY behavior.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const MAX_SUBSCRIBERS: usize = 500;
pub const DEFAULT_TIMEOUT_SECS: u32 = 300;
pub const MIN_TIMEOUT_SECS: u32 = 30;
pub const MAX_TIMEOUT_SECS: u32 = 1800;
pub const NOTIFY_QUEUE_CAPACITY: usize = 512;
pub const NOTIFY_WORKERS: usize = 4;
pub const MAX_CONSECUTIVE_FAILURES: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventService {
    ContentDir,
    ConnMgr,
    Registrar,
}

#[derive(Clone, Debug)]
pub struct Subscriber {
    pub sid: String,
    pub callback: String,
    pub service: EventService,
    pub timeout_secs: u32,
    pub seq: u32,
    pub created: Instant,
    consecutive_failures: u8,
}

#[derive(Clone, Debug)]
pub struct NotifyJob {
    pub sid: String,
    pub callback: String,
    pub seq: u32,
}

#[derive(Clone, Debug)]
pub struct CallbackUrl {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub path: String,
}

impl CallbackUrl {
    pub fn as_url(&self) -> String {
        if self.port == 80 {
            format!("http://{}{}", self.ip, self.path)
        } else {
            format!("http://{}:{}{}", self.ip, self.port, self.path)
        }
    }

    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.ip.to_string()
        } else {
            format!("{}:{}", self.ip, self.port)
        }
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::from((self.ip, self.port))
    }
}

#[derive(Debug, Default)]
pub struct EventHub {
    subs: Vec<Subscriber>,
}

impl EventHub {
    pub fn new() -> Self {
        Self { subs: Vec::new() }
    }

    pub fn gc(&mut self) {
        let now = Instant::now();
        self.subs
            .retain(|s| now.duration_since(s.created).as_secs() < u64::from(s.timeout_secs.max(1)));
    }

    fn find_mut(&mut self, sid: &str) -> Option<&mut Subscriber> {
        let sid = sid.trim();
        self.subs.iter_mut().find(|s| s.sid == sid)
    }

    pub fn subscribe_new(
        &mut self,
        sid: String,
        callback: String,
        service: EventService,
        timeout_secs: u32,
    ) -> Result<NotifyJob, u16> {
        self.gc();
        if self.subs.len() >= MAX_SUBSCRIBERS {
            return Err(412);
        }
        let job = NotifyJob {
            sid: sid.clone(),
            callback: callback.clone(),
            seq: 0,
        };
        self.subs.push(Subscriber {
            sid,
            callback,
            service,
            timeout_secs,
            seq: 1,
            created: Instant::now(),
            consecutive_failures: 0,
        });
        Ok(job)
    }

    pub fn renew(&mut self, sid: &str, timeout_secs: u32) -> Result<u32, u16> {
        self.gc();
        let sub = self.find_mut(sid).ok_or(412u16)?;
        sub.timeout_secs = timeout_secs;
        sub.created = Instant::now();
        Ok(timeout_secs)
    }

    pub fn unsubscribe(&mut self, sid: &str) -> Result<(), u16> {
        self.gc();
        let sid = sid.trim();
        let before = self.subs.len();
        self.subs.retain(|s| s.sid != sid);
        if self.subs.len() == before {
            Err(412)
        } else {
            Ok(())
        }
    }

    pub fn take_content_dir_notifies(&mut self) -> Vec<NotifyJob> {
        self.gc();
        let mut jobs = Vec::new();
        for s in &mut self.subs {
            if s.service != EventService::ContentDir {
                continue;
            }
            let seq = s.seq;
            s.seq = seq.wrapping_add(1);
            if s.seq == 0 {
                s.seq = 1;
            }
            jobs.push(NotifyJob {
                sid: s.sid.clone(),
                callback: s.callback.clone(),
                seq,
            });
        }
        jobs
    }

    /// Record a completed asynchronous delivery. A callback is removed only
    /// after several consecutive failures so a brief renderer restart does
    /// not discard an otherwise valid subscription.
    pub fn record_delivery(&mut self, sid: &str, delivered: bool) {
        let Some(sub) = self.find_mut(sid) else {
            return;
        };
        if delivered {
            sub.consecutive_failures = 0;
            return;
        }
        sub.consecutive_failures = sub.consecutive_failures.saturating_add(1);
        if sub.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            self.subs.retain(|candidate| candidate.sid != sid);
        }
    }

    pub fn len(&mut self) -> usize {
        self.gc();
        self.subs.len()
    }
}

pub fn parse_timeout(header: Option<&str>) -> u32 {
    let Some(value) = header.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_TIMEOUT_SECS;
    };
    let Some(value) = value
        .strip_prefix("Second-")
        .or_else(|| value.strip_prefix("second-"))
    else {
        return DEFAULT_TIMEOUT_SECS;
    };
    if value.eq_ignore_ascii_case("infinite") {
        return MAX_TIMEOUT_SECS;
    }
    value
        .trim()
        .parse::<u32>()
        .ok()
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)
}

pub fn service_from_path(path: &str) -> Option<EventService> {
    match path {
        rusty_dlna_protocol::paths::CONTENTDIRECTORY_EVENTURL => Some(EventService::ContentDir),
        rusty_dlna_protocol::paths::CONNECTIONMGR_EVENTURL => Some(EventService::ConnMgr),
        rusty_dlna_protocol::paths::X_MS_MEDIARECEIVERREGISTRAR_EVENTURL => {
            Some(EventService::Registrar)
        }
        _ => None,
    }
}

pub fn parse_callback(header: &str) -> Option<CallbackUrl> {
    let start = header.find('<')?;
    let rest = &header[start + 1..];
    let end = rest.find('>')?;
    parse_http_callback(rest[..end].trim())
}

fn parse_http_callback(url: &str) -> Option<CallbackUrl> {
    let rest = url.strip_prefix("http://")?;
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if auth.is_empty() {
        return None;
    }
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h, p.parse().ok()?)
        }
        _ => (auth, 80u16),
    };
    if port == 0 {
        return None;
    }
    let ip: Ipv4Addr = host.parse().ok()?;
    let path = sanitize_callback_path(path)?;
    Some(CallbackUrl { ip, port, path })
}

/// Origin-form path only: leading `/`, printable ASCII, no CTL / space /
/// backslash. Stops `NOTIFY {path}` header injection via a lone LF in Callback.
fn sanitize_callback_path(path: &str) -> Option<String> {
    if path.is_empty() {
        return Some("/".into());
    }
    if !path.starts_with('/') || path.len() > 1024 {
        return None;
    }
    if !path
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && b != b'\\')
    {
        return None;
    }
    Some(path.to_string())
}

pub fn peer_ipv4(peer: SocketAddr) -> Option<Ipv4Addr> {
    match peer {
        SocketAddr::V4(v) => Some(*v.ip()),
        SocketAddr::V6(v) => v.ip().to_ipv4_mapped().or(v.ip().to_ipv4()),
    }
}

pub fn propertyset(service: EventService, update_id: u32) -> String {
    match service {
        EventService::ContentDir => content_dir_propertyset(update_id, &[]),
        EventService::ConnMgr => format!(
            "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\" xmlns:s=\"urn:schemas-upnp-org:service:ConnectionManager:1\">\r\n  <e:property><SourceProtocolInfo>{}</SourceProtocolInfo></e:property>\r\n  <e:property><SinkProtocolInfo></SinkProtocolInfo></e:property>\r\n  <e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>\r\n</e:propertyset>",
            rusty_dlna_soap::protocol_info_source()
        ),
        EventService::Registrar => {
            "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\" xmlns:s=\"urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1\">\r\n  <e:property><AuthorizationGrantedUpdateID>1</AuthorizationGrantedUpdateID></e:property>\r\n</e:propertyset>".into()
        }
    }
}

/// ContentDirectory event body. `ContainerUpdateIDs` is the cache
/// invalidation signal consumed by Kodi's Platinum browser; each value is a
/// `container-id,update-id` pair.
pub fn content_dir_propertyset(update_id: u32, container_ids: &[String]) -> String {
    let mut updates = String::new();
    for container_id in container_ids {
        if !updates.is_empty() {
            updates.push(',');
        }
        updates.push_str(&rusty_dlna_soap::xml_escape(container_id));
        updates.push(',');
        updates.push_str(&update_id.to_string());
    }
    format!(
        "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\" xmlns:s=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\r\n  <e:property><SystemUpdateID>{update_id}</SystemUpdateID></e:property>\r\n  <e:property><ContainerUpdateIDs>{updates}</ContainerUpdateIDs></e:property>\r\n</e:propertyset>"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryOutcome {
    Delivered,
    RetryableFailure,
    PermanentFailure,
}

fn send_notify_outcome(job: &NotifyJob, body: &str) -> DeliveryOutcome {
    let Some(cb) = parse_http_callback(&job.callback) else {
        return DeliveryOutcome::PermanentFailure;
    };
    let host = cb.host_header();
    let wire = format!(
        "NOTIFY {path} HTTP/1.1\r\n\
Host: {host}\r\n\
Content-Type: text/xml; charset=\"utf-8\"\r\n\
Content-Length: {n}\r\n\
NT: upnp:event\r\n\
NTS: upnp:propchange\r\n\
SID: {sid}\r\n\
SEQ: {seq}\r\n\
Connection: close\r\n\
Cache-Control: no-cache\r\n\
\r\n\
{body}",
        path = cb.path,
        n = body.len(),
        sid = job.sid,
        seq = job.seq,
    );
    let Ok(mut sock) = TcpStream::connect_timeout(&cb.socket_addr(), Duration::from_millis(500))
    else {
        return DeliveryOutcome::RetryableFailure;
    };
    let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    if sock.write_all(wire.as_bytes()).is_err() {
        return DeliveryOutcome::RetryableFailure;
    }
    let _ = sock.shutdown(std::net::Shutdown::Write);
    let mut status_line = Vec::with_capacity(64);
    let Ok(read) = BufReader::new(sock)
        .take(129)
        .read_until(b'\n', &mut status_line)
    else {
        return DeliveryOutcome::RetryableFailure;
    };
    if read == 0 || read > 128 {
        return DeliveryOutcome::PermanentFailure;
    }
    let Ok(status_line) = std::str::from_utf8(&status_line) else {
        return DeliveryOutcome::PermanentFailure;
    };
    let mut fields = status_line
        .trim_end_matches(['\r', '\n'])
        .split_whitespace();
    let Some(version @ ("HTTP/1.0" | "HTTP/1.1")) = fields.next() else {
        return DeliveryOutcome::PermanentFailure;
    };
    let _ = version;
    let Some(status) = fields
        .next()
        .filter(|status| status.len() == 3)
        .and_then(|status| status.parse::<u16>().ok())
    else {
        return DeliveryOutcome::PermanentFailure;
    };
    match status {
        200..=299 => DeliveryOutcome::Delivered,
        408 | 425 | 429 | 500..=599 => DeliveryOutcome::RetryableFailure,
        _ => DeliveryOutcome::PermanentFailure,
    }
}

#[cfg(test)]
pub fn send_notify(job: &NotifyJob, body: &str) -> bool {
    send_notify_outcome(job, body) == DeliveryOutcome::Delivered
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotifyMetrics {
    pub queued: u64,
    pub dropped: u64,
    pub delivered: u64,
    pub failed: u64,
    pub retries: u64,
    pub pending: usize,
    pub in_flight: usize,
    pub capacity: usize,
    pub workers_total: usize,
    pub workers_alive: usize,
    pub stopping: bool,
}

#[derive(Debug, Default)]
struct AtomicNotifyMetrics {
    queued: AtomicU64,
    dropped: AtomicU64,
    delivered: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
}

impl AtomicNotifyMetrics {
    fn snapshot(&self) -> NotifyMetrics {
        NotifyMetrics {
            queued: self.queued.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            delivered: self.delivered.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            ..NotifyMetrics::default()
        }
    }
}

#[derive(Debug)]
struct QueuedNotify {
    job: NotifyJob,
    body: String,
}

#[derive(Debug, Default)]
struct QueueState {
    pending: HashMap<String, QueuedNotify>,
    ready: VecDeque<String>,
    in_flight: HashSet<String>,
    stopped: bool,
}

impl QueueState {
    fn enqueue(&mut self, queued: QueuedNotify, capacity: usize) -> bool {
        let sid = queued.job.sid.clone();
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.pending.entry(sid.clone())
        {
            entry.insert(queued);
            return true;
        }
        if self.in_flight.contains(&sid) {
            self.pending.insert(sid, queued);
            return true;
        }
        let unique = self.in_flight.len()
            + self
                .pending
                .keys()
                .filter(|pending| !self.in_flight.contains(*pending))
                .count();
        if unique >= capacity {
            return false;
        }
        self.pending.insert(sid.clone(), queued);
        self.ready.push_back(sid);
        true
    }

    fn take(&mut self) -> Option<QueuedNotify> {
        while let Some(sid) = self.ready.pop_front() {
            if let Some(queued) = self.pending.remove(&sid) {
                self.in_flight.insert(sid);
                return Some(queued);
            }
        }
        None
    }

    fn complete(&mut self, sid: &str) {
        self.in_flight.remove(sid);
        if self.pending.contains_key(sid) && !self.ready.iter().any(|ready| ready == sid) {
            self.ready.push_back(sid.to_string());
        }
    }
}

#[derive(Debug)]
struct NotifyShared {
    state: Mutex<QueueState>,
    available: Condvar,
    stopping: AtomicBool,
    hub: Arc<Mutex<EventHub>>,
    metrics: AtomicNotifyMetrics,
}

#[derive(Debug)]
pub struct NotifyDispatcher {
    shared: Arc<NotifyShared>,
    workers: Vec<std::thread::JoinHandle<()>>,
    capacity: usize,
}

impl NotifyDispatcher {
    pub fn new(hub: Arc<Mutex<EventHub>>) -> std::io::Result<Self> {
        Self::with_limits(NOTIFY_WORKERS, NOTIFY_QUEUE_CAPACITY, hub)
    }

    fn with_limits(
        workers: usize,
        capacity: usize,
        hub: Arc<Mutex<EventHub>>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(NotifyShared {
            state: Mutex::new(QueueState::default()),
            available: Condvar::new(),
            stopping: AtomicBool::new(false),
            hub,
            metrics: AtomicNotifyMetrics::default(),
        });
        let mut handles = Vec::new();
        for index in 0..workers.max(1) {
            let shared_for_worker = Arc::clone(&shared);
            match std::thread::Builder::new()
                .name(format!("gena-notify-{index}"))
                .spawn(move || notify_worker(shared_for_worker))
            {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    shared.stopping.store(true, Ordering::Release);
                    if let Ok(mut state) = shared.state.lock() {
                        state.stopped = true;
                    }
                    shared.available.notify_all();
                    for handle in handles {
                        let _ = handle.join();
                    }
                    return Err(error);
                }
            }
        }
        Ok(Self {
            shared,
            workers: handles,
            capacity,
        })
    }

    pub fn enqueue(&self, job: NotifyJob, body: String) -> bool {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let accepted = state.enqueue(QueuedNotify { job, body }, self.capacity);
        drop(state);
        if accepted {
            self.shared.metrics.queued.fetch_add(1, Ordering::Relaxed);
            self.shared.available.notify_one();
        } else {
            self.shared.metrics.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                capacity = self.capacity,
                "GENA notification queue is full; notification dropped"
            );
        }
        accepted
    }

    pub fn metrics(&self) -> NotifyMetrics {
        let mut metrics = self.shared.metrics.snapshot();
        if let Ok(state) = self.shared.state.lock() {
            metrics.pending = state.pending.len();
            metrics.in_flight = state.in_flight.len();
        }
        metrics.capacity = self.capacity;
        metrics.workers_total = self.workers.len();
        metrics.workers_alive = self
            .workers
            .iter()
            .filter(|worker| !worker.is_finished())
            .count();
        metrics.stopping = self.shared.stopping.load(Ordering::Acquire);
        metrics
    }
}

impl Drop for NotifyDispatcher {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        if let Ok(mut state) = self.shared.state.lock() {
            state.stopped = true;
            state.pending.clear();
            state.ready.clear();
        }
        self.shared.available.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn notify_worker(shared: Arc<NotifyShared>) {
    loop {
        let queued = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            while !state.stopped && state.ready.is_empty() {
                state = shared
                    .available
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if state.stopped {
                return;
            }
            state.take()
        };
        let Some(queued) = queued else {
            continue;
        };
        let mut delivered = false;
        for (attempt, delay) in [0, 100, 250].into_iter().enumerate() {
            if shared.stopping.load(Ordering::Acquire) {
                break;
            }
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
            match send_notify_outcome(&queued.job, &queued.body) {
                DeliveryOutcome::Delivered => {
                    delivered = true;
                    shared.metrics.delivered.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                DeliveryOutcome::PermanentFailure => break,
                DeliveryOutcome::RetryableFailure => {
                    if attempt < 2 {
                        shared.metrics.retries.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if !delivered && !shared.stopping.load(Ordering::Acquire) {
            shared.metrics.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(sid = %queued.job.sid, callback = %queued.job.callback, "GENA notification delivery failed after retries");
        }
        if !shared.stopping.load(Ordering::Acquire) {
            shared
                .hub
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .record_delivery(&queued.job.sid, delivered);
        }
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.complete(&queued.job.sid);
        drop(state);
        shared.available.notify_one();
    }
}

pub fn notify_content_dir(
    hub: &Arc<Mutex<EventHub>>,
    dispatcher: &NotifyDispatcher,
    update_id: u32,
) {
    let jobs = match hub.lock() {
        Ok(mut h) => h.take_content_dir_notifies(),
        Err(_) => return,
    };
    let body = propertyset(EventService::ContentDir, update_id);
    for job in jobs {
        dispatcher.enqueue(job, body.clone());
    }
}

pub fn notify_content_dir_containers(
    hub: &Arc<Mutex<EventHub>>,
    dispatcher: &NotifyDispatcher,
    update_id: u32,
    container_ids: &[String],
) {
    let jobs = match hub.lock() {
        Ok(mut h) => h.take_content_dir_notifies(),
        Err(_) => return,
    };
    let body = content_dir_propertyset(update_id, container_ids);
    for job in jobs {
        dispatcher.enqueue(job, body.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(sid: &str, seq: u32) -> NotifyJob {
        NotifyJob {
            sid: sid.into(),
            callback: "http://127.0.0.1:9/event".into(),
            seq,
        }
    }

    #[test]
    fn parse_callback_accepts_plain_ipv4() {
        let cb = parse_callback("<http://192.0.2.50:1234/evt>").expect("ok");
        assert_eq!(cb.ip, Ipv4Addr::new(192, 0, 2, 50));
        assert_eq!(cb.port, 1234);
        assert_eq!(cb.path, "/evt");
    }

    #[test]
    fn parse_callback_rejects_header_injection() {
        assert!(parse_callback("<http://192.0.2.50:1234/evt\nHost: pwn>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234/evt\r\nX: 1>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234/foo bar>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234/foo\\bar>").is_none());
        assert!(parse_callback("<http://192.0.2.50:1234>").is_some());
    }

    #[test]
    fn every_event_propertyset_is_well_formed_xml() {
        for service in [
            EventService::ContentDir,
            EventService::ConnMgr,
            EventService::Registrar,
        ] {
            let xml = propertyset(service, u32::MAX);
            let document = roxmltree::Document::parse(&xml)
                .unwrap_or_else(|error| panic!("{service:?}: {error}\n{xml}"));
            assert_eq!(document.root_element().tag_name().name(), "propertyset");
        }
    }

    #[test]
    fn content_directory_event_lists_kodi_container_invalidations() {
        let xml = content_dir_propertyset(42, &["2$15<&".into(), "64$1".into()]);
        let document = roxmltree::Document::parse(&xml).unwrap();
        let updates = document
            .descendants()
            .find(|node| node.tag_name().name() == "ContainerUpdateIDs")
            .and_then(|node| node.text())
            .unwrap();
        assert_eq!(updates, "2$15<&,42,64$1,42");
        assert!(xml.contains("2$15&lt;&amp;,42"), "{xml}");
    }

    #[test]
    fn timeout_values_are_clamped_and_infinite_is_deliberate() {
        assert_eq!(parse_timeout(None), DEFAULT_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("Second-1")), MIN_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("Second-999999")), MAX_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("Second-infinite")), MAX_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("garbage")), DEFAULT_TIMEOUT_SECS);

        let mut hub = EventHub::new();
        let initial = hub
            .subscribe_new(
                "uuid:test".into(),
                "http://127.0.0.1/event".into(),
                EventService::ContentDir,
                MIN_TIMEOUT_SECS,
            )
            .unwrap();
        assert_eq!(initial.seq, 0);
        assert_eq!(
            hub.renew("uuid:test", MAX_TIMEOUT_SECS).unwrap(),
            MAX_TIMEOUT_SECS
        );
    }

    #[test]
    fn bounded_queue_coalesces_newest_sequence_per_sid() {
        let mut state = QueueState::default();
        assert!(state.enqueue(
            QueuedNotify {
                job: job("uuid:a", 1),
                body: "one".into(),
            },
            2,
        ));
        assert!(state.enqueue(
            QueuedNotify {
                job: job("uuid:a", 2),
                body: "two".into(),
            },
            2,
        ));
        assert_eq!(state.pending.len(), 1);
        let first = state.take().unwrap();
        assert_eq!(first.job.seq, 2);
        assert!(state.enqueue(
            QueuedNotify {
                job: job("uuid:a", 3),
                body: "three".into(),
            },
            2,
        ));
        assert!(state.enqueue(
            QueuedNotify {
                job: job("uuid:b", 1),
                body: "other".into(),
            },
            2,
        ));
        assert!(!state.enqueue(
            QueuedNotify {
                job: job("uuid:c", 1),
                body: "overflow".into(),
            },
            2,
        ));
        state.complete("uuid:a");
        assert_eq!(state.take().unwrap().job.seq, 1); // b was already ready
        assert_eq!(state.take().unwrap().job.seq, 3);
    }

    #[test]
    fn callback_requires_a_successful_http_status() {
        fn serve(status: &'static str) -> (std::net::TcpListener, NotifyJob) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let callback = format!("http://{address}/event");
            let job = NotifyJob {
                sid: "uuid:test".into(),
                callback,
                seq: 0,
            };
            let server = listener.try_clone().unwrap();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut socket, _) = server.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request);
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                socket.write_all(response.as_bytes()).unwrap();
            });
            (listener, job)
        }
        let (_listener, rejected) = serve("500 Failed");
        assert!(!send_notify(&rejected, "<x/>"));
        let (_listener, accepted) = serve("204 No Content");
        assert!(send_notify(&accepted, "<x/>"));
    }

    #[test]
    fn random_subscription_ids_are_version_four_and_collision_free() {
        let mut ids = HashSet::new();
        for _ in 0..10_000 {
            let id = uuid::Uuid::new_v4();
            assert_eq!(id.get_version_num(), 4);
            assert!(ids.insert(id));
        }
    }
}
