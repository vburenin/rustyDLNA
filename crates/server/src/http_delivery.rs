//! Per-connection observations at the socket boundary, independent of route preparation.
use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rusty_dlna_http::HttpRoute;
use serde_json::{json, Map, Value};

use crate::{metrics::Histogram, App};

#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Completed,
    Truncated,
    Disconnected,
    TimedOut,
    Cancelled,
    Failed,
}

const OUTCOMES: [&str; 6] = [
    "completed",
    "truncated",
    "disconnected",
    "timed_out",
    "cancelled",
    "failed",
];

#[derive(Debug)]
pub(crate) struct Counters {
    statuses: [AtomicU64; 500],
    outcomes: [AtomicU64; 6],
    header_bytes: AtomicU64,
    body_bytes: AtomicU64,
    first_write: Histogram,
    headers: Histogram,
    duration: Histogram,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            statuses: std::array::from_fn(|_| AtomicU64::new(0)),
            outcomes: std::array::from_fn(|_| AtomicU64::new(0)),
            header_bytes: AtomicU64::new(0),
            body_bytes: AtomicU64::new(0),
            first_write: Histogram::default(),
            headers: Histogram::default(),
            duration: Histogram::default(),
        }
    }
}

impl Counters {
    pub(crate) fn json(&self) -> Value {
        let statuses: Map<String, Value> = self
            .statuses
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let count = value.load(Ordering::Relaxed);
                (count > 0).then(|| ((index + 100).to_string(), json!(count)))
            })
            .collect();
        let outcomes: Map<String, Value> = OUTCOMES
            .iter()
            .zip(&self.outcomes)
            .map(|(name, count)| ((*name).into(), json!(count.load(Ordering::Relaxed))))
            .collect();
        json!({
            "statuses": statuses,
            "outcomes": outcomes,
            "header_bytes_total": self.header_bytes.load(Ordering::Relaxed),
            "body_bytes_total": self.body_bytes.load(Ordering::Relaxed),
            "first_write_ms": self.first_write.json(),
            "headers_ms": self.headers.json(),
            "duration_ms": self.duration.json(),
        })
    }
}

struct Observation {
    started: Instant,
    route: Option<HttpRoute>,
    head: bool,
    status: Option<u16>,
    header_remaining: u64,
    headers_written: bool,
    first_write: bool,
    promised: Option<u64>,
    body_written: u64,
    outcome: Option<Outcome>,
}

impl Default for Observation {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            route: None,
            head: false,
            status: None,
            header_remaining: 0,
            headers_written: false,
            first_write: false,
            promised: None,
            body_written: 0,
            outcome: None,
        }
    }
}

struct Connection {
    app: Arc<App>,
    response: Option<Observation>,
}

impl Connection {
    fn finish(&mut self, fallback: Outcome) {
        let Some(response) = self.response.take() else {
            return;
        };
        let counters = self.app.runtime_metrics.delivery(response.route);
        let outcome = response.outcome.unwrap_or_else(|| {
            if matches!(fallback, Outcome::Completed)
                && (!response.headers_written
                    || response.promised.is_some_and(|n| response.body_written < n))
            {
                Outcome::Truncated
            } else {
                fallback
            }
        });
        counters.outcomes[outcome as usize].fetch_add(1, Ordering::Relaxed);
        counters.duration.observe(response.started.elapsed());
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Aborting a socket task still closes its observation exactly once.
        self.finish(Outcome::Cancelled);
    }
}

tokio::task_local! {
    // Writes stay in the owning socket task. Blocking preparation/helper jobs
    // never inherit this scope or record a prepared response as wire delivery.
    static CONNECTION: RefCell<Connection>;
}

pub(crate) async fn scope<T>(app: Arc<App>, future: impl Future<Output = T>) -> T {
    CONNECTION
        .scope(
            RefCell::new(Connection {
                app,
                response: None,
            }),
            future,
        )
        .await
}

fn with_connection(update: impl FnOnce(&mut Connection)) {
    let _ = CONNECTION.try_with(|connection| update(&mut connection.borrow_mut()));
}

pub(crate) fn begin_request() {
    with_connection(|connection| {
        connection.response.get_or_insert_with(Observation::default);
    });
}

pub(crate) fn parsed_request(route: HttpRoute, head: bool) {
    with_connection(|connection| {
        let response = connection.response.get_or_insert_with(Observation::default);
        response.route = Some(route);
        response.head = head;
    });
}

pub(crate) fn response_header(wire: &[u8]) {
    let Some(length) = rusty_dlna_http::header_block_complete(wire) else {
        return;
    };
    let Ok(header) = std::str::from_utf8(&wire[..length]) else {
        return;
    };
    with_connection(|connection| {
        let response = connection.response.get_or_insert_with(Observation::default);
        // Inspect the serialized status, including the serializer's fixed 500
        // fallback. No prepared response status enters these counters.
        response.status = header
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok());
        response.header_remaining = length as u64;
        response.promised = if response.head {
            Some(0)
        } else {
            header.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("Content-Length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
        };
    });
}

pub(crate) fn written(bytes: usize) {
    with_connection(|connection| {
        let Some(response) = connection.response.as_mut() else {
            return;
        };
        let counters = connection.app.runtime_metrics.delivery(response.route);
        if !response.first_write {
            response.first_write = true;
            counters.first_write.observe(response.started.elapsed());
        }
        let header_bytes = response.header_remaining.min(bytes as u64);
        response.header_remaining -= header_bytes;
        counters
            .header_bytes
            .fetch_add(header_bytes, Ordering::Relaxed);
        let body_bytes = (bytes as u64).saturating_sub(header_bytes);
        response.body_written = response.body_written.saturating_add(body_bytes);
        counters.body_bytes.fetch_add(body_bytes, Ordering::Relaxed);
        if !response.headers_written && response.header_remaining == 0 {
            if let Some(status) = response.status.filter(|status| (100..600).contains(status)) {
                counters.statuses[usize::from(status - 100)].fetch_add(1, Ordering::Relaxed);
                counters.headers.observe(response.started.elapsed());
                response.headers_written = true;
            }
        }
    });
}

pub(crate) fn failed(outcome: Outcome) {
    with_connection(|connection| {
        if let Some(response) = connection.response.as_mut() {
            response.outcome.get_or_insert(outcome);
        }
    });
}

pub(crate) fn io_failed(error: &io::Error) {
    failed(match error.kind() {
        io::ErrorKind::UnexpectedEof => Outcome::Truncated,
        io::ErrorKind::TimedOut => Outcome::TimedOut,
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected => Outcome::Disconnected,
        io::ErrorKind::Interrupted => Outcome::Cancelled,
        _ => Outcome::Failed,
    });
}

pub(crate) fn finish(success: bool) {
    with_connection(|connection| {
        connection.finish(if success {
            Outcome::Completed
        } else {
            Outcome::Failed
        });
    });
}
