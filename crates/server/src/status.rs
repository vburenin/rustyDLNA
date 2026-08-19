//! Human and machine-readable operational status without media-root paths.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use crate::{available_filesystem_bytes, remux, unix_now, App};

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

fn catalog_counts(app: &App) -> (u32, u32, u32, usize, u32, u32, usize, u64) {
    let cat = app
        .catalog
        .read()
        .unwrap_or_else(|error| error.into_inner());
    let mut audio = 0u32;
    let mut video = 0u32;
    let mut image = 0u32;
    let mut video_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut video_art = 0u32;
    let mut captions = 0u32;
    let mut seen_details: HashSet<i64> = HashSet::new();
    for item in cat.items.values() {
        if !seen_details.insert(item.detail_id) {
            continue;
        }
        captions = captions.saturating_add(item.captions.len() as u32);
        if item.mime.starts_with("audio/") {
            audio = audio.saturating_add(1);
        } else if item.mime.starts_with("video/") {
            video = video.saturating_add(1);
            video_inodes.insert((item.device, item.inode));
            if item.album_art > 0 {
                video_art = video_art.saturating_add(1);
            }
        } else if item.mime.starts_with("image/") {
            image = image.saturating_add(1);
        }
    }
    (
        audio,
        video,
        image,
        video_inodes.len(),
        video_art,
        captions,
        cat.items.len() + cat.containers.len(),
        cat.estimated_memory_bytes(),
    )
}

fn status_value(app: &App) -> (Health, Value) {
    let (audio, video, image, video_inodes, video_art, captions, objects, estimated_bytes) =
        catalog_counts(app);
    let bytes_per_object = if objects == 0 {
        std::mem::size_of::<rusty_dlna_scan::MediaItem>() as u64
    } else {
        estimated_bytes / objects as u64
    };
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
    let subscribers = app
        .events
        .lock()
        .map(|mut events| events.len())
        .unwrap_or_default();
    let db_check = app
        .db_pool
        .as_ref()
        .map(|pool| pool.read(|db| db.quick_check()))
        .transpose();
    let db_ok = match db_check {
        Ok(Some(check)) => check == "ok",
        Ok(None) => true,
        Err(_) => false,
    };
    let cache_free_bytes = available_filesystem_bytes(&app.cache_dir).unwrap_or(0);
    let minimum_free_bytes = app.cfg.cache_min_free_mb.saturating_mul(1024 * 1024);
    let scan_stale_after = app.cfg.rescan_secs.max(300).saturating_mul(3);
    let scan_stale = scan
        .last_success_unix
        .is_some_and(|last| unix_now().saturating_sub(last) > scan_stale_after);
    let mut health = Health::Healthy;
    let mut reasons = Vec::new();
    if !db_ok {
        health = Health::Unhealthy;
        reasons.push("database quick_check failed");
    }
    if cache_free_bytes < minimum_free_bytes {
        if health != Health::Unhealthy {
            health = Health::Degraded;
        }
        reasons.push("cache free space is below the configured minimum");
    }
    if scan.last_error.is_some() || scan_stale {
        if health != Health::Unhealthy {
            health = Health::Degraded;
        }
        reasons.push(if scan_stale {
            "scanner success is stale"
        } else {
            "scanner reported an error"
        });
    }
    if !app.required_tools_ready {
        if health != Health::Unhealthy {
            health = Health::Degraded;
        }
        reasons.push("required transcode tools are unavailable");
    }
    let value = json!({
        "status": health.as_str(),
        "reasons": reasons,
        "listener": { "http": true, "ssdp_interfaces": app.ssdp_interfaces.len() },
        "database": { "quick_check": if db_ok { "ok" } else { "failed" } },
        "scanner": {
            "phase": scan.phase,
            "started_unix": scan.started_unix,
            "finished_unix": scan.finished_unix,
            "duration_ms": scan.duration_ms,
            "files_seen": scan_files_seen,
            "current_entry": scan_current_entry,
            "last_success_unix": scan.last_success_unix,
            "last_error": scan.last_error,
            "next_reconcile_unix": scan.next_reconcile_unix,
            "watch_count": app.scan_telemetry.watch_count.load(Ordering::Relaxed),
            "overflow_count": app.scan_telemetry.overflow_count.load(Ordering::Relaxed),
            "batches": app.scan_telemetry.batches.load(Ordering::Relaxed),
        },
        "catalog": {
            "audio": audio, "video": video, "image": image,
            "video_inodes": video_inodes, "video_with_art": video_art,
            "captions": captions,
            "objects": objects,
            "estimated_memory_bytes": estimated_bytes,
            "projected_memory_bytes": {
                "objects_10000": bytes_per_object.saturating_mul(10_000),
                "objects_100000": bytes_per_object.saturating_mul(100_000),
                "objects_1000000": bytes_per_object.saturating_mul(1_000_000),
            },
            "update_id": app.update_id.load(Ordering::Relaxed),
        },
        "transcode": {
            "active": remux.active,
            "queued": remux.queued,
            "completed_total": remux.completed_total,
            "failed_total": remux.failed_total,
            "cancelled_total": remux.cancelled_total,
            "cache_bytes": remux.cache_bytes,
            "oldest_job_seconds": remux.oldest_job_secs,
            "max_jobs": app.cfg.transcode.max_jobs,
            "required_tools_ready": app.required_tools_ready,
        },
        "helpers": {
            "active": helpers.active,
            "queued": helpers.queued,
            "max_active": helpers.max_active,
            "queue_capacity": helpers.queue_capacity,
            "admitted_total": helpers.admitted_total,
            "rejected_total": helpers.rejected_total,
            "timed_out_total": helpers.timed_out_total,
        },
        "events": {
            "subscribers": subscribers,
            "queued_total": events.queued,
            "dropped_total": events.dropped,
            "delivered_total": events.delivered,
            "failed_total": events.failed,
            "retries_total": events.retries,
        },
        "cache": {
            "free_bytes": cache_free_bytes,
            "minimum_free_bytes": minimum_free_bytes,
        },
        "client_cache_entries": app.client_cache.lock().map(|cache| cache.len()).unwrap_or(0),
    });
    (health, value)
}

pub fn status_json(app: &App, health_only: bool) -> (u16, String) {
    let (health, mut value) = status_value(app);
    if health_only {
        value = json!({
            "status": health.as_str(),
            "reasons": value["reasons"],
            "database": value["database"],
            "scanner": {
                "phase": value["scanner"]["phase"],
                "last_success_unix": value["scanner"]["last_success_unix"],
                "last_error": value["scanner"]["last_error"],
            },
            "cache": value["cache"],
            "helpers": value["helpers"],
        });
    }
    (
        health.http_status(),
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into()),
    )
}

pub fn status_html(app: &App) -> String {
    let (_, status) = status_value(app);
    let catalog = &status["catalog"];
    let scanner = &status["scanner"];
    let transcode = &status["transcode"];
    let helpers = &status["helpers"];
    format!(
        "<html><head><title>{name}</title><meta http-equiv=\"refresh\" content=\"20\"></head><body>\
         <h1>{name}</h1><p>Health: {health}</p><table>\
         <tr><td>Audio</td><td>{audio}</td></tr><tr><td>Video</td><td>{video}</td></tr>\
         <tr><td>Image</td><td>{image}</td></tr><tr><td>Captions</td><td>{captions}</td></tr>\
         <tr><td>UpdateID</td><td>{update_id}</td></tr>\
         <tr><td>Scan phase</td><td>{phase}</td></tr><tr><td>Inotify watches</td><td>{watches}</td></tr>\
         <tr><td>Transcodes active</td><td>{active}</td></tr><tr><td>Transcodes queued</td><td>{queued}</td></tr>\
         <tr><td>Media helpers active</td><td>{helper_active}</td></tr><tr><td>Media helpers queued</td><td>{helper_queued}</td></tr>\
         <tr><td>Transcode cache bytes</td><td>{cache_bytes}</td></tr></table>\
         <p><a href=\"/api/status\">Machine-readable status</a></p></body></html>",
        name = html_esc(&app.cfg.friendly_name),
        health = status["status"].as_str().unwrap_or("unknown"),
        audio = catalog["audio"],
        video = catalog["video"],
        image = catalog["image"],
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
