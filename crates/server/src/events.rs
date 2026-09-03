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
    pending: HashMap<String, VecDeque<QueuedNotify>>,
    ready: VecDeque<String>,
    in_flight: HashSet<String>,
    stopped: bool,
}

impl QueueState {
    fn enqueue(&mut self, queued: QueuedNotify, capacity: usize) -> bool {
        if self.stopped {
            return false;
        }
        let sid = queued.job.sid.clone();
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.pending.entry(sid.clone())
        {
            let pending = entry.get_mut();
            if queued.job.seq == 0 {
                if pending.front().is_some_and(|notify| notify.job.seq == 0) {
                    pending[0] = queued;
                } else {
                    pending.push_front(queued);
                }
            } else if pending.front().is_some_and(|notify| notify.job.seq == 0) {
                if pending.len() == 1 {
                    pending.push_back(queued);
                } else if let Some(latest) = pending.back_mut() {
                    *latest = queued;
                }
            } else {
                pending.clear();
                pending.push_back(queued);
            }
            return true;
        }
        if self.in_flight.contains(&sid) {
            self.pending.insert(sid, VecDeque::from([queued]));
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
        self.pending.insert(sid.clone(), VecDeque::from([queued]));
        self.ready.push_back(sid);
        true
    }

    fn take(&mut self) -> Option<QueuedNotify> {
        while let Some(sid) = self.ready.pop_front() {
            let Some(pending) = self.pending.get_mut(&sid) else {
                continue;
            };
            let queued = pending.pop_front();
            let empty = pending.is_empty();
            if empty {
                self.pending.remove(&sid);
            }
            if let Some(queued) = queued {
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

fn lock_queue(shared: &NotifyShared) -> std::sync::MutexGuard<'_, QueueState> {
    crate::lock_recover(&shared.state)
}

#[derive(Debug)]
pub struct NotifyDispatcher {
    shared: Arc<NotifyShared>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
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
                    lock_queue(&shared).stopped = true;
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
            workers: Mutex::new(handles),
            capacity,
        })
    }

    pub fn enqueue(&self, job: NotifyJob, body: String) -> bool {
        let mut state = lock_queue(&self.shared);
        let stopping = state.stopped || self.shared.stopping.load(Ordering::Acquire);
        let accepted = !stopping && state.enqueue(QueuedNotify { job, body }, self.capacity);
        drop(state);
        if accepted {
            self.shared.metrics.queued.fetch_add(1, Ordering::Relaxed);
            self.shared.available.notify_one();
        } else {
            self.shared.metrics.dropped.fetch_add(1, Ordering::Relaxed);
            if stopping {
                tracing::debug!("GENA notification dispatcher is stopping; notification dropped");
            } else {
                tracing::warn!(
                    capacity = self.capacity,
                    "GENA notification queue is full; notification dropped"
                );
            }
        }
        accepted
    }

    pub fn metrics(&self) -> NotifyMetrics {
        let mut metrics = self.shared.metrics.snapshot();
        let state = lock_queue(&self.shared);
        metrics.pending = state.pending.len();
        metrics.in_flight = state.in_flight.len();
        drop(state);
        metrics.capacity = self.capacity;
        let workers = crate::lock_recover(&self.workers);
        metrics.workers_total = workers.len();
        metrics.workers_alive = workers
            .iter()
            .filter(|worker| !worker.is_finished())
            .count();
        metrics.stopping = self.shared.stopping.load(Ordering::Acquire);
        metrics
    }

    /// Stop accepting notification work and wake idle workers. An in-flight
    /// socket operation observes the stop before any retry; its existing
    /// connect/read/write timeout remains the final bound.
    pub(crate) fn begin_shutdown(&self) {
        self.shared.stopping.store(true, Ordering::Release);
        let mut state = lock_queue(&self.shared);
        state.stopped = true;
        state.pending.clear();
        state.ready.clear();
        drop(state);
        self.shared.available.notify_all();
    }

    fn reap_finished_workers(&self) -> usize {
        let mut workers = crate::lock_recover(&self.workers);
        let mut finished = Vec::new();
        let mut index = 0usize;
        while index < workers.len() {
            if workers[index].is_finished() {
                finished.push(workers.swap_remove(index));
            } else {
                index += 1;
            }
        }
        let remaining = workers.len();
        drop(workers);
        for worker in finished {
            let _ = worker.join();
        }
        remaining
    }

    fn detach_workers(&self) -> usize {
        let workers = std::mem::take(&mut *crate::lock_recover(&self.workers));
        let count = workers.len();
        drop(workers);
        count
    }

    /// Reap notification workers until the shared graceful-shutdown deadline.
    /// Workers still inside a bounded socket call at the deadline are detached;
    /// they own only `NotifyShared`, observe `stopping` before retrying, and can
    /// no longer extend `App` destruction beyond the whole-process budget.
    pub(crate) async fn stop_until(&self, deadline: Instant) -> bool {
        self.begin_shutdown();
        loop {
            if self.reap_finished_workers() == 0 {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                let detached = self.detach_workers();
                tracing::warn!(
                    workers = detached,
                    "GENA notification workers did not exit before the graceful shutdown deadline"
                );
                return false;
            }
            tokio::time::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(10)),
            )
            .await;
        }
    }
}

fn subscribe_and_admit_initial(
    hub: &Arc<Mutex<EventHub>>,
    sid: String,
    callback: String,
    service: EventService,
    timeout_secs: u32,
    admit: impl FnOnce(NotifyJob) -> bool,
) -> Result<(), u16> {
    // Content updates take the same lock before allocating their sequence, so
    // no later notification can overtake sequence zero.
    let mut hub = crate::lock_recover(hub);
    let job = hub.subscribe_new(sid.clone(), callback, service, timeout_secs)?;
    if admit(job) {
        return Ok(());
    }
    let _ = hub.unsubscribe(&sid);
    Err(503)
}

pub(super) fn subscribe_and_enqueue_initial(
    hub: &Arc<Mutex<EventHub>>,
    dispatcher: &NotifyDispatcher,
    sid: String,
    callback: String,
    service: EventService,
    timeout_secs: u32,
    body: impl FnOnce() -> String,
) -> Result<(), u16> {
    subscribe_and_admit_initial(hub, sid, callback, service, timeout_secs, |job| {
        dispatcher.enqueue(job, body())
    })
}

impl Drop for NotifyDispatcher {
    fn drop(&mut self) {
        self.begin_shutdown();
        let workers = std::mem::take(
            self.workers
                .get_mut()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for worker in workers {
            let _ = worker.join();
        }
    }
}

fn notify_worker(shared: Arc<NotifyShared>) {
    loop {
        let queued = {
            let mut state = lock_queue(&shared);
            while !state.stopped
                && !shared.stopping.load(Ordering::Acquire)
                && state.ready.is_empty()
            {
                state = shared
                    .available
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if state.stopped || shared.stopping.load(Ordering::Acquire) {
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
                if shared.stopping.load(Ordering::Acquire) {
                    break;
                }
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
            crate::lock_recover(&shared.hub).record_delivery(&queued.job.sid, delivered);
        }
        let mut state = lock_queue(&shared);
        state.complete(&queued.job.sid);
        drop(state);
        shared.available.notify_one();
    }
}

fn admit_content_dir_jobs(hub: &Arc<Mutex<EventHub>>, mut admit: impl FnMut(NotifyJob)) {
    // Sequence allocation and queue admission share the hub lock so two
    // concurrent publishers cannot enqueue newer sequence numbers first.
    let mut hub = crate::lock_recover(hub);
    let jobs = hub.take_content_dir_notifies();
    for job in jobs {
        admit(job);
    }
}

pub(super) fn notify_content_dir(
    hub: &Arc<Mutex<EventHub>>,
    dispatcher: &NotifyDispatcher,
    update_id: u32,
) {
    let body = propertyset(EventService::ContentDir, update_id);
    admit_content_dir_jobs(hub, |job| {
        dispatcher.enqueue(job, body.clone());
    });
}

pub(super) fn notify_content_dir_containers(
    hub: &Arc<Mutex<EventHub>>,
    dispatcher: &NotifyDispatcher,
    update_id: u32,
    container_ids: &[String],
) {
    let body = content_dir_propertyset(update_id, container_ids);
    admit_content_dir_jobs(hub, |job| {
        dispatcher.enqueue(job, body.clone());
    });
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
    fn queue_delivers_initial_sequence_before_coalesced_updates() {
        let mut state = QueueState::default();
        for sequence in 0..=2 {
            assert!(state.enqueue(
                QueuedNotify {
                    job: job("uuid:initial", sequence),
                    body: format!("sequence-{sequence}"),
                },
                1,
            ));
        }
        assert_eq!(state.pending.len(), 1);
        let initial = state.take().unwrap();
        assert_eq!(initial.job.seq, 0);
        assert_eq!(initial.body, "sequence-0");
        state.complete("uuid:initial");
        let latest = state.take().unwrap();
        assert_eq!(latest.job.seq, 2);
        assert_eq!(latest.body, "sequence-2");
    }

    #[test]
    fn reverse_enqueue_order_keeps_initial_sequence_first() {
        let mut state = QueueState::default();
        assert!(state.enqueue(
            QueuedNotify {
                job: job("uuid:reverse", 1),
                body: "update".into(),
            },
            1,
        ));
        assert!(state.enqueue(
            QueuedNotify {
                job: job("uuid:reverse", 0),
                body: "initial".into(),
            },
            1,
        ));
        assert_eq!(state.take().unwrap().job.seq, 0);
        state.complete("uuid:reverse");
        assert_eq!(state.take().unwrap().job.seq, 1);
    }

    #[test]
    fn initial_admission_is_atomic_and_rolls_back_failure() {
        use std::sync::mpsc;

        let hub = Arc::new(Mutex::new(EventHub::new()));
        assert_eq!(
            subscribe_and_admit_initial(
                &hub,
                "uuid:rejected".into(),
                "http://127.0.0.1:9/event".into(),
                EventService::ContentDir,
                MIN_TIMEOUT_SECS,
                |_| false,
            ),
            Err(503)
        );
        assert_eq!(crate::lock_recover(&hub).len(), 0);

        let (admit_started_tx, admit_started_rx) = mpsc::channel();
        let (release_admit_tx, release_admit_rx) = mpsc::channel();
        let subscriber_hub = Arc::clone(&hub);
        let subscriber = std::thread::spawn(move || {
            subscribe_and_admit_initial(
                &subscriber_hub,
                "uuid:ordered".into(),
                "http://127.0.0.1:9/event".into(),
                EventService::ContentDir,
                MIN_TIMEOUT_SECS,
                |initial| {
                    assert_eq!(initial.seq, 0);
                    admit_started_tx.send(()).unwrap();
                    release_admit_rx.recv().unwrap();
                    true
                },
            )
        });
        admit_started_rx.recv().unwrap();

        let (update_tx, update_rx) = mpsc::channel();
        let update_hub = Arc::clone(&hub);
        let update = std::thread::spawn(move || {
            let jobs = crate::lock_recover(&update_hub).take_content_dir_notifies();
            update_tx.send(jobs).unwrap();
        });
        assert!(
            update_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "content update must wait until sequence zero is admitted"
        );
        release_admit_tx.send(()).unwrap();
        subscriber.join().unwrap().unwrap();
        let jobs = update_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].seq, 1);
        update.join().unwrap();
    }

    #[test]
    fn concurrent_updates_allocate_and_admit_sequences_in_order() {
        use std::sync::mpsc;

        let hub = Arc::new(Mutex::new(EventHub::new()));
        crate::lock_recover(&hub)
            .subscribe_new(
                "uuid:updates".into(),
                "http://127.0.0.1:9/event".into(),
                EventService::ContentDir,
                MIN_TIMEOUT_SECS,
            )
            .unwrap();

        let (first_started_tx, first_started_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let (first_sequence_tx, first_sequence_rx) = mpsc::channel();
        let first_hub = Arc::clone(&hub);
        let first = std::thread::spawn(move || {
            admit_content_dir_jobs(&first_hub, |job| {
                first_started_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                first_sequence_tx.send(job.seq).unwrap();
            });
        });
        first_started_rx.recv().unwrap();

        let (second_sequence_tx, second_sequence_rx) = mpsc::channel();
        let second_hub = Arc::clone(&hub);
        let second = std::thread::spawn(move || {
            admit_content_dir_jobs(&second_hub, |job| {
                second_sequence_tx.send(job.seq).unwrap();
            });
        });
        assert!(
            second_sequence_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "a newer sequence must wait for admission of the prior update"
        );
        release_first_tx.send(()).unwrap();
        assert_eq!(
            first_sequence_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            1
        );
        assert_eq!(
            second_sequence_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            2
        );
        first.join().unwrap();
        second.join().unwrap();
    }

    #[test]
    fn poisoned_queue_state_does_not_hang_dispatcher_shutdown() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        use std::sync::mpsc;

        let dispatcher =
            NotifyDispatcher::with_limits(1, 1, Arc::new(Mutex::new(EventHub::new()))).unwrap();
        let shared = Arc::clone(&dispatcher.shared);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _state = shared.state.lock().unwrap();
            panic!("poison queue state");
        }))
        .is_err());
        assert_eq!(dispatcher.metrics().workers_total, 1);

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(dispatcher);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("poisoned dispatcher must stop its worker");
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
