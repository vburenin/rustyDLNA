//! Human and machine-readable operational status without media-root paths.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use crate::metrics::ComponentState;
use crate::{available_filesystem_bytes, remux, unix_now, App, DbIntegrityResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Healthy,
    Degraded,
    Unhealthy,
}

impl Health {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }

    fn http_status(self) -> u16 {
        match self {
            Self::Unhealthy => 503,
            Self::Healthy | Self::Degraded => 200,
        }
    }
}

#[derive(Debug, Default)]
struct CatalogCounts {
    audio_records: u32,
    video_records: u32,
    image_records: u32,
    physical_inodes: usize,
    path_aliases: usize,
    media_records: usize,
    item_objects: usize,
    container_objects: usize,
    total_objects: usize,
    video_with_art: u32,
    captions: u32,
    estimated_memory_bytes: u64,
}

fn catalog_counts(app: &App) -> CatalogCounts {
    let cat = app
        .catalog
        .read()
        .unwrap_or_else(|error| error.into_inner());
    let mut counts = CatalogCounts::default();
    let mut physical_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut seen_details: HashSet<i64> = HashSet::new();
    for item in cat.items.values() {
        if !seen_details.insert(item.detail_id) {
            continue;
        }
        physical_inodes.insert((item.device, item.inode));
        counts.captions = counts.captions.saturating_add(item.captions.len() as u32);
        if item.mime.starts_with("audio/") {
            counts.audio_records = counts.audio_records.saturating_add(1);
        } else if item.mime.starts_with("video/") {
            counts.video_records = counts.video_records.saturating_add(1);
            if item.album_art > 0 {
                counts.video_with_art = counts.video_with_art.saturating_add(1);
            }
        } else if item.mime.starts_with("image/") {
            counts.image_records = counts.image_records.saturating_add(1);
        }
    }
    counts.physical_inodes = physical_inodes.len();
    counts.media_records = seen_details.len();
    counts.path_aliases = counts.media_records.saturating_sub(counts.physical_inodes);
    counts.item_objects = cat.items.len();
    counts.container_objects = cat.containers.len();
    counts.total_objects = counts.item_objects.saturating_add(counts.container_objects);
    counts.estimated_memory_bytes = cat.estimated_memory_bytes();
    counts
}

fn degrade(health: &mut Health) {
    if *health != Health::Unhealthy {
        *health = Health::Degraded;
    }
}

fn status_value(app: &App, detailed: bool) -> (Health, Value) {
    let scan = app
        .scan_control
        .state
        .lock()
        .map(|state| state.clone())
        .unwrap_or_default();
    let (scan_files_seen, scan_current_entry) = app
        .scan_cfg
        .progress
        .as_ref()
        .map(|progress| progress.snapshot())
        .unwrap_or_default();
    let scan_current_entry = scan_current_entry.and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    let remux = remux::runtime_status(app);
    let helpers = app.helpers.metrics();
    let events = app.notify_dispatcher.metrics();
    let runtime = app.runtime_metrics.snapshot();
    let database = app.db_integrity.get(app.db_pool.clone());
    let pool = app
        .db_pool
        .as_ref()
        .map(|pool| pool.metrics())
        .unwrap_or_default();
    let stopping = app.scan_control.cancellation.is_cancelled();
    let (inotify_alive, periodic_alive, scan_workers_total, scan_workers_alive) = app
        .scan_control
        .threads
        .lock()
        .map(|workers| {
            let inotify_alive = workers
                .iter()
                .any(|worker| worker.role == "inotify" && !worker.handle.is_finished());
            let periodic_alive = workers
                .iter()
                .any(|worker| worker.role == "periodic_reconcile" && !worker.handle.is_finished());
            let alive = workers
                .iter()
                .filter(|worker| !worker.handle.is_finished())
                .count();
            (inotify_alive, periodic_alive, workers.len(), alive)
        })
        .unwrap_or_default();
    let inotify_state = if stopping {
        "stopping"
    } else if inotify_alive {
        "running"
    } else if scan_workers_total == 0 {
        "not_started"
    } else {
        "failed"
    };
    let periodic_state = if app.cfg.rescan_secs == 0 {
        "disabled"
    } else if stopping {
        "stopping"
    } else if periodic_alive {
        "running"
    } else if scan_workers_total == 0 {
        "not_started"
    } else {
        "failed"
    };
    let watcher_state = if stopping {
        "stopping"
    } else if inotify_alive && (app.cfg.rescan_secs == 0 || periodic_alive) {
        "running"
    } else if scan_workers_total == 0 {
        "not_started"
    } else if inotify_alive || periodic_alive {
        "degraded"
    } else {
        "failed"
    };
    let subscribers = app
        .events
        .lock()
        .map(|mut events| events.len())
        .unwrap_or_default();
    let cache_free_bytes = available_filesystem_bytes(&app.cache_dir).unwrap_or(0);
    let minimum_free_bytes = app.cfg.cache_min_free_mb.saturating_mul(1024 * 1024);
    let configured_rescan_max = if app.cfg.rescan_max_secs == 0 {
        app.cfg.rescan_secs
    } else {
        app.cfg.rescan_max_secs
    };
    let scan_stale_after = configured_rescan_max.max(300).saturating_mul(3);
    let scan_stale = scan
        .last_success_unix
        .is_some_and(|last| unix_now().saturating_sub(last) > scan_stale_after);
    let mut health = Health::Healthy;
    let mut reasons = Vec::new();
    match runtime.http_listener {
        ComponentState::Running => {}
        ComponentState::NotStarted => {
            degrade(&mut health);
            reasons.push("HTTP accept loop has not started");
        }
        ComponentState::Stopping | ComponentState::Stopped | ComponentState::Failed => {
            health = Health::Unhealthy;
            reasons.push("HTTP accept loop is not running");
        }
    }
    match runtime.ssdp {
        ComponentState::Running => {}
        ComponentState::NotStarted => {
            degrade(&mut health);
            reasons.push("SSDP task has not started");
        }
        ComponentState::Stopping | ComponentState::Stopped | ComponentState::Failed => {
            degrade(&mut health);
            reasons.push("SSDP discovery task is not running");
        }
    }
    match database.result {
        DbIntegrityResult::Ok | DbIntegrityResult::NotConfigured => {}
        DbIntegrityResult::Failed | DbIntegrityResult::Error => {
            health = Health::Unhealthy;
            reasons.push("cached database quick_check failed");
        }
    }
    if database.stale() {
        degrade(&mut health);
        reasons.push("database quick_check result is stale");
    }
    if app.db_pool.is_some() && pool.readers_available == 0 && pool.read_waiters > 0 {
        degrade(&mut health);
        reasons.push("database read pool is saturated");
    }
    if cache_free_bytes < minimum_free_bytes {
        degrade(&mut health);
        reasons.push("cache free space is below the configured minimum");
    }
    if watcher_state != "running" {
        if watcher_state == "failed" {
            health = Health::Unhealthy;
        } else {
            degrade(&mut health);
        }
        reasons.push("scanner/watch workers are not fully running");
    }
    if scan.last_error.is_some() || scan_stale {
        degrade(&mut health);
        reasons.push(if scan_stale {
            "scanner success is stale"
        } else {
            "scanner reported an error"
        });
    }
    if events.workers_alive != events.workers_total || events.stopping {
        degrade(&mut health);
        reasons.push("GENA notification workers are not fully running");
    }
    if helpers.queue_capacity > 0
        && helpers.queued >= helpers.queue_capacity
        && helpers.active >= helpers.max_active
    {
        degrade(&mut health);
        reasons.push("media helper queue is saturated");
    }
    if app.cfg.transcode.enable
        && (!remux.supervisor_ready || runtime.remux_supervisor != ComponentState::Running)
    {
        degrade(&mut health);
        reasons.push("remux supervisor is not running");
    }
    if !app.required_tools_ready {
        degrade(&mut health);
        reasons.push("required transcode tools are unavailable");
    }
    let mut helper_wait_cumulative = 0u64;
    let helper_wait_buckets = helpers
        .wait_duration_ms_buckets
        .iter()
        .enumerate()
        .map(|(index, count)| {
            helper_wait_cumulative = helper_wait_cumulative.saturating_add(*count);
            let bound = ["10", "50", "100", "500", "1000", "+Inf"][index];
            json!({
                "le_ms": bound,
                "count": helper_wait_cumulative,
            })
        })
        .collect::<Vec<_>>();
    let catalog_value = if detailed {
        let catalog = catalog_counts(app);
        let bytes_per_object = if catalog.total_objects == 0 {
            std::mem::size_of::<rusty_dlna_scan::MediaItem>() as u64
        } else {
            catalog.estimated_memory_bytes / catalog.total_objects as u64
        };
        json!({
            "audio_records": catalog.audio_records,
            "video_records": catalog.video_records,
            "image_records": catalog.image_records,
            "physical_inodes": catalog.physical_inodes,
            "path_aliases": catalog.path_aliases,
            "media_records": catalog.media_records,
            "item_objects": catalog.item_objects,
            "container_objects": catalog.container_objects,
            "total_objects": catalog.total_objects,
            "video_with_art": catalog.video_with_art,
            "captions": catalog.captions,
            "estimated_memory_bytes": catalog.estimated_memory_bytes,
            "projected_memory_bytes": {
                "objects_10000": bytes_per_object.saturating_mul(10_000),
                "objects_100000": bytes_per_object.saturating_mul(100_000),
                "objects_1000000": bytes_per_object.saturating_mul(1_000_000),
            },
            "update_id": app.update_id.load(Ordering::Relaxed),
        })
    } else {
        Value::Null
    };
    let value = json!({
        "status": health.as_str(),
        "reasons": reasons,
        "listener": {
            "http": runtime.http_listener.as_str(),
            "ssdp": runtime.ssdp.as_str(),
            "ssdp_interfaces": app.ssdp_interfaces.len(),
            "accepted_connections_total": runtime.accepted_connections,
            "active_connections": runtime.active_connections,
            "accept_errors_total": runtime.accept_errors,
        },
        "database": {
            "quick_check": database.result.as_str(),
            "checked_unix": database.checked_unix,
            "age_seconds": database.age_seconds(),
            "duration_ms": database.duration_ms,
            "runs_total": database.runs_total,
            "refresh_in_flight": database.refresh_in_flight,
            "refresh_after_seconds": crate::DB_CHECK_REFRESH_SECS,
            "stale_after_seconds": crate::DB_CHECK_STALE_SECS,
            "pool": {
                "configured": app.db_pool.is_some(),
                "readers_total": pool.reader_count,
                "readers_available": pool.readers_available,
                "read_active": pool.read_active,
                "read_waiters": pool.read_waiters,
                "writer_active": pool.writer_active,
                "reads_total": pool.reads_total,
                "writes_total": pool.writes_total,
                "errors_total": pool.errors_total,
                "read_wait_ms_total": pool.read_wait_ms_total,
                "read_wait_ms_max": pool.read_wait_ms_max,
            },
        },
        "scanner": {
            "worker_state": watcher_state,
            "workers_total": scan_workers_total,
            "workers_alive": scan_workers_alive,
            "workers": {
                "inotify": inotify_state,
                "periodic_reconcile": periodic_state,
            },
            "phase": scan.phase,
            "started_unix": scan.started_unix,
            "finished_unix": scan.finished_unix,
            "duration_ms": scan.duration_ms,
            "files_seen": scan_files_seen,
            "current_entry": scan_current_entry,
            "last_success_unix": scan.last_success_unix,
            "last_error": scan.last_error,
            "next_reconcile_unix": scan.next_reconcile_unix,
            "reconcile_interval_secs": scan.reconcile_interval_secs,
            "reconcile_min_secs": app.cfg.rescan_secs,
            "reconcile_max_secs": configured_rescan_max,
            "watch_count": app.scan_telemetry.watch_count.load(Ordering::Relaxed),
            "overflow_count": app.scan_telemetry.overflow_count.load(Ordering::Relaxed),
            "dropped_events_total": app.scan_telemetry.dropped_events_total.load(Ordering::Relaxed),
            "batches": app.scan_telemetry.batches.load(Ordering::Relaxed),
            "events_total": app.scan_telemetry.events_total.load(Ordering::Relaxed),
            "pending_paths": app.scan_telemetry.pending_paths.load(Ordering::Relaxed),
            "pending_paths_peak": app.scan_telemetry.pending_paths_peak.load(Ordering::Relaxed),
            "last_batch_paths": app.scan_telemetry.last_batch_paths.load(Ordering::Relaxed),
            "full_reconciles": app.scan_telemetry.full_reconciles.load(Ordering::Relaxed),
            "targeted_batches": app.scan_telemetry.targeted_batches.load(Ordering::Relaxed),
        },
        "catalog": catalog_value,
        "transcode": {
            "supervisor_state": if !app.cfg.transcode.enable { "disabled" } else if !remux.supervisor_ready { "failed" } else { runtime.remux_supervisor.as_str() },
            "active": remux.active,
            "queued": remux.queued,
            "completed_total": remux.completed_total,
            "failed_total": remux.failed_total,
            "cancelled_total": remux.cancelled_total,
            "cache_hits_total": remux.cache_hits_total,
            "cache_misses_total": remux.cache_misses_total,
            "coalesced_requests_total": remux.coalesced_requests_total,
            "cache_maintenance_total": remux.cache_maintenance_total,
            "cache_maintenance_failures_total": remux.cache_maintenance_failures_total,
            "cache_evicted_files_total": remux.cache_evicted_files_total,
            "cache_evicted_bytes_total": remux.cache_evicted_bytes_total,
            "cache_bytes": remux.cache_bytes,
            "oldest_job_seconds": remux.oldest_job_secs,
            "web_player": {
                "requests_total": remux.web_requests_total,
                "seek_restarts_total": remux.web_seek_restarts_total,
                "cache_reuses_total": remux.web_cache_reuses_total,
                "cancellations_total": remux.web_cancelled_total,
                "failures": {
                    "busy_total": remux.web_failures_busy_total,
                    "producer_total": remux.web_failures_producer_total,
                },
                "startup_to_first_playable_ms": {
                    "count": remux.web_startup_playable_count,
                    "sum_ms": remux.web_startup_playable_sum_ms,
                    "max_ms": remux.web_startup_playable_max_ms,
                },
            },
            "max_jobs": app.cfg.transcode.max_jobs,
            "required_tools_ready": app.required_tools_ready,
        },
        "helpers": {
            "active": helpers.active,
            "queued": helpers.queued,
            "max_active": helpers.max_active,
            "queue_capacity": helpers.queue_capacity,
            "admitted_total": helpers.admitted_total,
            "saturated_total": helpers.saturated_total,
            "queued_total": helpers.queued_total,
            "rejected_total": helpers.rejected_total,
            "timed_out_total": helpers.timed_out_total,
            "wait_duration_ms": {
                "count": helpers.queued_total,
                "sum_ms": helpers.wait_duration_ms_total,
                "max_ms": helpers.wait_duration_ms_max,
                "buckets": helper_wait_buckets,
            },
        },
        "events": {
            "worker_state": if events.stopping { "stopping" } else if events.workers_alive == events.workers_total { "running" } else { "degraded" },
            "workers_total": events.workers_total,
            "workers_alive": events.workers_alive,
            "pending": events.pending,
            "in_flight": events.in_flight,
            "capacity": events.capacity,
            "subscribers": subscribers,
            "queued_total": events.queued,
            "dropped_total": events.dropped,
            "delivered_total": events.delivered,
            "failed_total": events.failed,
            "retries_total": events.retries,
        },
        "metrics": if detailed { app.runtime_metrics.json() } else { Value::Null },
        "cache": {
            "free_bytes": cache_free_bytes,
            "minimum_free_bytes": minimum_free_bytes,
        },
        "client_cache_entries": app.client_cache.lock().map(|cache| cache.len()).unwrap_or(0),
    });
    (health, value)
}

pub fn status_json(app: &App, health_only: bool) -> (u16, String) {
    let (health, mut value) = status_value(app, !health_only);
    if health_only {
        value = json!({
            "status": health.as_str(),
            "reasons": value["reasons"],
            "listener": value["listener"],
            "database": value["database"],
            "scanner": value["scanner"],
            "cache": value["cache"],
            "helpers": value["helpers"],
            "events": value["events"],
            "transcode": value["transcode"],
        });
    }
    (
        health.http_status(),
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into()),
    )
}

pub fn status_html(app: &App) -> String {
    let (_, status) = status_value(app, true);
    let catalog = &status["catalog"];
    let scanner = &status["scanner"];
    let transcode = &status["transcode"];
    let helpers = &status["helpers"];
    format!(
        "<html><head><title>{name}</title><meta http-equiv=\"refresh\" content=\"20\"></head><body>\
         <h1>{name}</h1><p>Health: {health}</p><table>\
         <tr><td>Audio</td><td>{audio}</td></tr><tr><td>Video</td><td>{video}</td></tr>\
         <tr><td>Image</td><td>{image}</td></tr><tr><td>Physical files</td><td>{physical}</td></tr>\
         <tr><td>Path aliases</td><td>{aliases}</td></tr><tr><td>Media records</td><td>{records}</td></tr>\
         <tr><td>Item objects</td><td>{items}</td></tr><tr><td>Containers</td><td>{containers}</td></tr>\
         <tr><td>Total objects</td><td>{total}</td></tr><tr><td>Captions</td><td>{captions}</td></tr>\
         <tr><td>UpdateID</td><td>{update_id}</td></tr>\
         <tr><td>Scan phase</td><td>{phase}</td></tr><tr><td>Inotify watches</td><td>{watches}</td></tr>\
         <tr><td>Transcodes active</td><td>{active}</td></tr><tr><td>Transcodes queued</td><td>{queued}</td></tr>\
         <tr><td>Media helpers active</td><td>{helper_active}</td></tr><tr><td>Media helpers queued</td><td>{helper_queued}</td></tr>\
         <tr><td>Transcode cache bytes</td><td>{cache_bytes}</td></tr></table>\
         <p><a href=\"/api/status\">Machine-readable status</a></p></body></html>",
        name = html_esc(&app.cfg.friendly_name),
        health = status["status"].as_str().unwrap_or("unknown"),
        audio = catalog["audio_records"],
        video = catalog["video_records"],
        image = catalog["image_records"],
        physical = catalog["physical_inodes"],
        aliases = catalog["path_aliases"],
        records = catalog["media_records"],
        items = catalog["item_objects"],
        containers = catalog["container_objects"],
        total = catalog["total_objects"],
        captions = catalog["captions"],
        update_id = catalog["update_id"],
        phase = html_esc(scanner["phase"].as_str().unwrap_or("idle")),
        watches = scanner["watch_count"],
        active = transcode["active"],
        queued = transcode["queued"],
        helper_active = helpers["active"],
        helper_queued = helpers["queued"],
        cache_bytes = transcode["cache_bytes"],
    )
}

fn html_esc(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(character),
        }
    }
    escaped
}
