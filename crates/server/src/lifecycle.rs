//! Daemon lifecycle, library workers, HTTP sockets, and SSDP ownership.
#![warn(missing_docs)]

use super::*;

pub(super) fn os_version() -> String {
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
    app.runtime_metrics
        .set_http_listener(ComponentState::Running);
    tracing::info!(%http_addr, "http listen");
    if let Err(error) = spawn_library_watch(app.clone()) {
        app.runtime_metrics
            .set_http_listener(ComponentState::Failed);
        return Err(error.into());
    }
    app.runtime_metrics
        .set_remux_supervisor(ComponentState::Running);
    let ssdp_app = app.clone();
    app.runtime_metrics.set_ssdp(ComponentState::Running);
    let ssdp = tokio::spawn(async move {
        let result = ssdp_loop(ssdp_app.clone()).await;
        ssdp_app.runtime_metrics.set_ssdp(ComponentState::Failed);
        match result {
            Ok(()) => tracing::warn!("SSDP loop stopped unexpectedly"),
            Err(error) => tracing::warn!(%error, "SSDP loop failed"),
        }
    });
    let failure = tokio::select! {
        _ = accept_loop(listener, app.clone()) => {
            app.runtime_metrics.set_http_listener(ComponentState::Failed);
            Some("HTTP accept loop stopped unexpectedly".to_string())
        }
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal");
            None
        }
    };
    let shutdown_started = Instant::now();
    let shutdown_budget = Duration::from_secs(app.cfg.shutdown_timeout_secs);
    let shutdown_deadline = shutdown_started + shutdown_budget;
    if app.runtime_metrics.snapshot().http_listener != ComponentState::Failed {
        app.runtime_metrics
            .set_http_listener(ComponentState::Stopping);
    }
    if app.runtime_metrics.snapshot().ssdp != ComponentState::Failed {
        app.runtime_metrics.set_ssdp(ComponentState::Stopping);
    }
    app.runtime_metrics
        .set_remux_supervisor(ComponentState::Stopping);
    if !ssdp.is_finished() {
        ssdp.abort();
    }
    // Broadcast cancellation before waiting on any one subsystem. Byebye is
    // sent up front so even an unexpected slow join cannot hide the device's
    // departure from control points.
    app.scan_control.cancellation.cancel();
    remux::cancel_all(&app);
    app.notify_dispatcher.begin_shutdown();
    send_byebye(&app).await;
    stop_library_watch_until(&app, shutdown_deadline);
    let remaining = shutdown_deadline.saturating_duration_since(Instant::now());
    remux::wait_for_shutdown(&app, remaining).await;
    let notifications_stopped = app.notify_dispatcher.stop_until(shutdown_deadline).await;
    let shutdown_elapsed = shutdown_started.elapsed();
    let jobs_remaining = app.jobs.in_use();
    let deadline_exceeded = shutdown_deadline_exceeded(
        shutdown_elapsed,
        shutdown_budget,
        jobs_remaining,
        notifications_stopped,
    );
    app.runtime_metrics.record_shutdown(shutdown_elapsed);
    tracing::info!(
        duration_ms = shutdown_elapsed.as_millis(),
        budget_ms = shutdown_budget.as_millis(),
        deadline_exceeded,
        jobs_remaining,
        notifications_stopped,
        "graceful shutdown complete"
    );
    if failure.is_none() {
        app.runtime_metrics
            .set_http_listener(ComponentState::Stopped);
        app.runtime_metrics.set_ssdp(ComponentState::Stopped);
        app.runtime_metrics
            .set_remux_supervisor(ComponentState::Stopped);
    }
    match failure {
        Some(message) => {
            app.runtime_metrics
                .set_http_listener(ComponentState::Failed);
            app.runtime_metrics.set_ssdp(ComponentState::Failed);
            app.runtime_metrics
                .set_remux_supervisor(ComponentState::Failed);
            Err(std::io::Error::other(message).into())
        }
        None => Ok(()),
    }
}

fn shutdown_deadline_exceeded(
    elapsed: Duration,
    budget: Duration,
    jobs_remaining: usize,
    notifications_stopped: bool,
) -> bool {
    elapsed > budget || jobs_remaining != 0 || !notifications_stopped
}

pub(super) async fn accept_loop(listener: tokio::net::TcpListener, app: Arc<App>) {
    let connections = Arc::new(tokio::sync::Semaphore::new(app.cfg.max_connections));
    loop {
        let permit = match connections.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                app.runtime_metrics.accept_error();
                tracing::warn!("accept: {e}");
                continue;
            }
        };
        app.runtime_metrics.connection_opened();
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_conn(app.clone(), sock, peer).await {
                let msg = e.to_string();
                if msg.contains("Broken pipe") || msg.contains("Connection reset") {
                    tracing::debug!(%peer, "conn: {e}");
                } else {
                    tracing::error!(%peer, "conn: {e}");
                }
            }
            app.runtime_metrics.connection_closed();
        });
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
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
    let pkts = match try_notify_byebye(&app.uuid) {
        Ok(packets) => packets,
        Err(error) => {
            tracing::error!(%error, "cannot construct SSDP byebye packets");
            return;
        }
    };
    let dests = ssdp_notify_dests(app);
    let interfaces = if app.ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT {
        app.ssdp_interfaces
            .iter()
            .map(|(address, _)| *address)
            .collect::<Vec<_>>()
    } else {
        vec![Ipv4Addr::UNSPECIFIED]
    };
    for interface in &interfaces {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        );
        let sock = match socket.and_then(|socket| {
            socket.bind(&socket2::SockAddr::from(SocketAddrV4::new(*interface, 0)))?;
            if !interface.is_unspecified() {
                socket.set_multicast_if_v4(interface)?;
            }
            socket.set_nonblocking(true)?;
            let standard: std::net::UdpSocket = socket.into();
            tokio::net::UdpSocket::from_std(standard)
        }) {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(%interface, %error, "byebye socket");
                continue;
            }
        };
        // Six types × 2, no LOCATION/SERVER/CACHE-CONTROL (packet builder).
        for _ in 0..2 {
            for packet in &pkts {
                for (ip, port) in &dests {
                    let _ = sock.send_to(packet.as_bytes(), (*ip, *port)).await;
                }
            }
        }
    }
    tracing::info!(
        packets = pkts.len() * 2 * interfaces.len(),
        interfaces = interfaces.len(),
        "SSDP byebye sent"
    );
}

#[cfg(test)]
fn prepare_catalog_generation(
    app: &App,
    id: u32,
    hydrate: impl FnOnce(&LibraryDb) -> rusqlite::Result<()>,
) -> ScanResult<()> {
    let Some(pool) = app.db_pool.as_ref() else {
        return Err(rusty_dlna_scan::ScanError::Invariant(
            "catalog publication requires the application database pool".into(),
        ));
    };
    pool.write_scan(&app.scan_cfg.cancellation, |db| {
        let transaction = db.transaction()?;
        // Acquire SQLite's writer before reading canonical bookmark state so
        // no later database writer can slip between hydration and update-ID
        // persistence. A hydration failure rolls the tentative ID back.
        db.set_update_id(id)?;
        hydrate(db)?;
        transaction.commit()
    })
}

#[cfg(test)]
pub(crate) fn apply_catalog(
    app: &App,
    mut next: Catalog,
    delta: ScanDelta,
    why: &'static str,
) -> ScanResult<()> {
    if app.scan_cfg.cancellation.is_cancelled() {
        return Err(rusty_dlna_scan::ScanError::Cancelled);
    }
    let items = next.items.len();
    // Publication serialization covers update-ID allocation and persistence,
    // but the catalog write lock is deferred until the in-memory commit so
    // SQLite writer contention does not block Browse.
    let _publication = lock_recover(&app.catalog_publication);
    if app.scan_cfg.cancellation.is_cancelled() {
        return Err(rusty_dlna_scan::ScanError::Cancelled);
    }
    let id =
        rusty_dlna_protocol::soap::next_system_update_id(app.update_id.load(Ordering::Acquire));
    prepare_catalog_generation(app, id, |db| db.hydrate_catalog_bookmarks(&mut next))?;
    let mut published = write_recover(&app.catalog);
    *published = next;
    app.invalidate_catalog_query_cache();
    app.update_id.store(id, Ordering::Release);
    app.notify_catalog_publication(&published, id, None);
    drop(published);
    tracing::info!(
        items,
        added = delta.added,
        removed = delta.removed,
        changed = delta.changed,
        "{why}"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn apply_catalog_update(
    app: &App,
    update: CatalogUpdate,
    delta: ScanDelta,
    why: &'static str,
) -> ScanResult<()> {
    let mut patch = match update {
        CatalogUpdate::Replacement(catalog) => return apply_catalog(app, catalog, delta, why),
        CatalogUpdate::Patch(patch) => patch,
    };
    if app.scan_cfg.cancellation.is_cancelled() {
        return Err(rusty_dlna_scan::ScanError::Cancelled);
    }
    let _publication = lock_recover(&app.catalog_publication);
    if app.scan_cfg.cancellation.is_cancelled() {
        return Err(rusty_dlna_scan::ScanError::Cancelled);
    }
    let id =
        rusty_dlna_protocol::soap::next_system_update_id(app.update_id.load(Ordering::Acquire));
    prepare_catalog_generation(app, id, |db| db.hydrate_catalog_patch_bookmarks(&mut patch))?;
    let mut published = write_recover(&app.catalog);
    published.apply_patch(patch);
    let items = published.items.len();
    app.invalidate_catalog_query_cache();
    app.update_id.store(id, Ordering::Release);
    app.notify_catalog_publication(&published, id, None);
    drop(published);
    tracing::info!(
        items,
        added = delta.added,
        removed = delta.removed,
        changed = delta.changed,
        publication = "incremental",
        "{why}"
    );
    Ok(())
}

pub(super) fn prepare_scan_change(
    app: &App,
    prepare: impl FnOnce(&mut ScanSession) -> ScanResult<PreparedCatalogChange>,
) -> ScanResult<PreparedCatalogChange> {
    let mut session = lock_recover(&app.scan_session);
    if session.is_none() {
        *session = Some(ScanSession::new(&app.scan_cfg)?);
    }
    let session = session.as_mut().ok_or_else(|| {
        rusty_dlna_scan::ScanError::Invariant("scan session initialization was lost".into())
    })?;
    prepare(session)
}

#[cfg(test)]
static PREPARED_PUBLICATION_FAILURES: std::sync::LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
pub(crate) fn fail_prepared_publications_for_test(path: &Path, failures: usize) {
    PREPARED_PUBLICATION_FAILURES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(path.to_path_buf(), failures);
}

#[cfg(test)]
pub(crate) fn clear_prepared_publication_failures_for_test(path: &Path) {
    PREPARED_PUBLICATION_FAILURES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(path);
}

#[cfg(test)]
fn take_prepared_publication_failure(path: Option<&Path>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let mut failures = PREPARED_PUBLICATION_FAILURES
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(remaining) = failures.get_mut(path) else {
        return false;
    };
    if *remaining == 0 {
        failures.remove(path);
        return false;
    }
    *remaining -= 1;
    if *remaining == 0 {
        failures.remove(path);
    }
    true
}

pub(super) fn apply_prepared_catalog_change(
    app: &App,
    prepared: PreparedCatalogChange,
    why: &'static str,
) -> ScanResult<()> {
    if app.scan_cfg.cancellation.is_cancelled() {
        return Err(rusty_dlna_scan::ScanError::Cancelled);
    }
    #[cfg(test)]
    if take_prepared_publication_failure(app.scan_cfg.db_path.as_deref()) {
        return Err(rusty_dlna_scan::ScanError::Invariant(
            "injected prepared catalog publication failure".into(),
        ));
    }
    let _publication = lock_recover(&app.catalog_publication);
    if app.scan_cfg.cancellation.is_cancelled() {
        return Err(rusty_dlna_scan::ScanError::Cancelled);
    }
    let update_id =
        rusty_dlna_protocol::soap::next_system_update_id(app.update_id.load(Ordering::Acquire));
    let Some(pool) = app.db_pool.as_ref() else {
        return Err(rusty_dlna_scan::ScanError::Invariant(
            "prepared catalog publication requires the application database pool".into(),
        ));
    };
    let mut session_guard = lock_recover(&app.scan_session);
    let session = session_guard.as_mut().ok_or_else(|| {
        rusty_dlna_scan::ScanError::Invariant("prepared scan session is unavailable".into())
    })?;
    let mut published = match pool.write_scan(&app.scan_cfg.cancellation, |db| {
        session.publish_to_db(db, prepared, Some(update_id))
    }) {
        Ok(published) => published,
        Err(error) => {
            if let Err(reopen_error) = pool.reopen_writer(&app.scan_cfg.cancellation) {
                tracing::error!(%reopen_error, "could not reopen the catalog writer after failed staged publication");
            }
            return Err(error);
        }
    };
    if published.writer_reopen_required() {
        if let Err(error) = pool.reopen_writer(&app.scan_cfg.cancellation) {
            tracing::error!(%error, "could not reopen the catalog writer after scan-stage detach failure");
        }
    }
    let publication_id = published.committed_update_id();
    let (update, delta) = published.take_parts();
    let Some(update) = update else {
        drop(_publication);
        session.finish_publication();
        drop(session_guard);
        published.cleanup_stale_art();
        return Ok(());
    };
    let mut catalog = write_recover(&app.catalog);
    match update {
        CatalogUpdate::Replacement(next) => *catalog = next,
        CatalogUpdate::Patch(patch) => catalog.apply_patch(patch),
    }
    let id = publication_id.unwrap_or(update_id);
    let items = catalog.items.len();
    app.invalidate_catalog_query_cache();
    app.update_id.store(id, Ordering::Release);
    app.notify_catalog_publication(&catalog, id, None);
    drop(catalog);
    drop(_publication);
    session.finish_publication();
    drop(session_guard);
    published.cleanup_stale_art();
    tracing::info!(
        items,
        added = delta.added,
        removed = delta.removed,
        changed = delta.changed,
        publication = "staged",
        "{why}"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReconcileOutcome {
    pub(super) success: bool,
    pub(super) changed: bool,
}

fn reconcile_library(app: &App, why: &'static str) -> ReconcileOutcome {
    match prepare_scan_change(app, |session| session.prepare_monitor(&[], true)) {
        Ok(prepared) => {
            let changed =
                prepared.delta().added + prepared.delta().removed + prepared.delta().changed > 0;
            if let Err(error) = apply_prepared_catalog_change(app, prepared, why) {
                tracing::error!(%error, "{why} publication failed; retaining published catalog");
                ReconcileOutcome {
                    success: false,
                    changed: false,
                }
            } else {
                ReconcileOutcome {
                    success: true,
                    changed,
                }
            }
        }
        Err(error) => {
            tracing::error!(%error, "{why} failed; retaining published catalog");
            ReconcileOutcome {
                success: false,
                changed: false,
            }
        }
    }
}

const RECONCILE_DUTY_MULTIPLIER: u64 = 20;

pub(super) fn next_reconcile_interval_secs(
    minimum: u64,
    maximum: u64,
    current: u64,
    outcome: ReconcileOutcome,
    elapsed: Duration,
) -> u64 {
    let maximum = maximum.max(minimum);
    if !outcome.success || outcome.changed || maximum == minimum {
        return minimum;
    }
    let elapsed_secs = u64::try_from(elapsed.as_millis().div_ceil(1_000)).unwrap_or(u64::MAX);
    current
        .max(minimum)
        .saturating_mul(2)
        .max(elapsed_secs.saturating_mul(RECONCILE_DUTY_MULTIPLIER))
        .clamp(minimum, maximum)
}

pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn scan_phase_started(control: &ScanControl, phase: &str) -> Instant {
    let mut state = control
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.phase = phase.to_string();
    state.started_unix = Some(unix_now());
    state.last_error = None;
    Instant::now()
}

fn scan_phase_finished(
    control: &ScanControl,
    phase: &str,
    started: Instant,
    error: Option<String>,
) {
    let mut state = control
        .state
        .lock()
        .unwrap_or_else(|failure| failure.into_inner());
    let now = unix_now();
    state.phase = phase.to_string();
    state.finished_unix = Some(now);
    state.duration_ms = Some(duration_millis_saturating(started.elapsed()));
    if let Some(error) = error {
        state.last_error = Some(error);
    } else {
        state.last_success_unix = Some(now);
        state.last_error = None;
    }
}

const STARTUP_RETRY_MIN: Duration = Duration::from_millis(500);
const STARTUP_RETRY_MAX: Duration = Duration::from_secs(30);

fn startup_maintenance_pass(app: &App) -> ScanResult<()> {
    if app.scan_cfg.cancellation.is_cancelled() {
        return Err(rusty_dlna_scan::ScanError::Cancelled);
    }
    let empty = app
        .catalog
        .read()
        .map(|catalog| catalog.items.is_empty())
        .unwrap_or(true);
    if empty {
        tracing::info!("empty library; full scan from disk");
        let prepared = prepare_scan_change(app, |session| session.prepare_scan(true))?;
        apply_prepared_catalog_change(app, prepared, "full scan done")?;
    } else {
        let prepared = prepare_scan_change(app, ScanSession::prepare_video_title_repair)?;
        apply_prepared_catalog_change(app, prepared, "video title repair")?;
        let prepared = prepare_scan_change(app, ScanSession::prepare_object_repair)?;
        apply_prepared_catalog_change(app, prepared, "library object repair")?;
        if !reconcile_library(app, "library reconcile").success {
            return Err(rusty_dlna_scan::ScanError::Invariant(
                "initial library reconciliation failed".into(),
            ));
        }
    }
    fill_missing_av_meta(app)
}

fn wait_for_scan_retry(control: &ScanControl, delay: Duration) -> bool {
    if control.cancellation.is_cancelled() {
        return false;
    }
    let sleeper = control
        .sleep
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _wait = control
        .wake
        .wait_timeout_while(sleeper, delay, |_| !control.cancellation.is_cancelled())
        .unwrap_or_else(|error| error.into_inner());
    !control.cancellation.is_cancelled()
}

fn run_startup_maintenance_until_success(app: &App) -> bool {
    // Startup owns serialization until the complete ordered maintenance pass
    // succeeds. Periodic reconciliation must not interleave between retries.
    let _serial = app
        .scan_control
        .gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut retry_delay = STARTUP_RETRY_MIN;
    loop {
        if app.scan_control.cancellation.is_cancelled() {
            return false;
        }
        let started = scan_phase_started(&app.scan_control, "initializing");
        let result = startup_maintenance_pass(app);
        match result {
            Ok(()) => {
                scan_phase_finished(&app.scan_control, "watching", started, None);
                return !app.scan_control.cancellation.is_cancelled();
            }
            Err(error) => {
                tracing::error!(%error, retry_ms = retry_delay.as_millis(), "startup library maintenance failed; retrying the ordered initialization pass");
                scan_phase_finished(
                    &app.scan_control,
                    "initializing-retry",
                    started,
                    Some(error.to_string()),
                );
                if !wait_for_scan_retry(&app.scan_control, retry_delay) {
                    return false;
                }
                retry_delay = retry_delay.saturating_mul(2).min(STARTUP_RETRY_MAX);
            }
        }
    }
}

pub(super) fn spawn_library_watch(app: Arc<App>) -> std::io::Result<()> {
    let rescan_secs = app.cfg.rescan_secs;
    let rescan_max_secs = if app.cfg.rescan_max_secs == 0 {
        rescan_secs
    } else {
        app.cfg.rescan_max_secs
    };
    let inotify_app = app.clone();
    let inotify_handle = std::thread::Builder::new()
        .name("inotify".into())
        .spawn(move || {
            let app = inotify_app;
            let cfg = app.scan_cfg.clone();
            if !run_startup_maintenance_until_success(&app) {
                return;
            }
            if app.scan_control.cancellation.is_cancelled() {
                return;
            }
            let watch_app = app.clone();
            let control = app.scan_control.clone();
            let telemetry = app.scan_telemetry.clone();
            if let Err(e) = run_inotify_prepared_updates_until(
                cfg,
                control.cancellation.as_atomic(),
                Some(&control.gate),
                Some(&telemetry),
                &app.scan_session,
                move |prepared| {
                    let started = scan_phase_started(&watch_app.scan_control, "publishing");
                    let result = apply_prepared_catalog_change(
                        &watch_app,
                        prepared,
                        "inotify library update",
                    );
                    let failure = result.as_ref().err().map(ToString::to_string);
                    scan_phase_finished(&watch_app.scan_control, "watching", started, failure);
                    result
                },
            ) {
                tracing::warn!("inotify: {e}");
                if !control.cancellation.is_cancelled() {
                    scan_phase_finished(&control, "degraded", Instant::now(), Some(e.to_string()));
                }
            }
        })?;
    app.scan_control
        .threads
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(ScanWorker {
            role: "inotify",
            handle: inotify_handle,
        });
    if rescan_secs > 0 {
        let periodic_app = app.clone();
        let periodic = match std::thread::Builder::new()
            .name("rescan".into())
            .spawn(move || {
                let app = periodic_app;
                let mut interval_secs = rescan_secs;
                loop {
                    {
                        let mut state = app
                            .scan_control
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        state.reconcile_interval_secs = Some(interval_secs);
                        state.next_reconcile_unix = Some(unix_now().saturating_add(interval_secs));
                    }
                    let sleeper = app
                        .scan_control
                        .sleep
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let _ = app
                        .scan_control
                        .wake
                        .wait_timeout(sleeper, Duration::from_secs(interval_secs));
                    if app.scan_control.cancellation.is_cancelled() {
                        break;
                    }
                    let _serial = app
                        .scan_control
                        .gate
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let started = scan_phase_started(&app.scan_control, "periodic-reconcile");
                    let outcome = reconcile_library(&app, "periodic rescan");
                    let elapsed = started.elapsed();
                    let previous_interval_secs = interval_secs;
                    interval_secs = next_reconcile_interval_secs(
                        rescan_secs,
                        rescan_max_secs,
                        interval_secs,
                        outcome,
                        elapsed,
                    );
                    tracing::info!(
                        previous_interval_secs,
                        interval_secs,
                        maximum_interval_secs = rescan_max_secs,
                        changed = outcome.changed,
                        success = outcome.success,
                        elapsed_ms = duration_millis_saturating(elapsed),
                        "periodic reconciliation cadence selected"
                    );
                    scan_phase_finished(
                        &app.scan_control,
                        "watching",
                        started,
                        (!outcome.success).then(|| "periodic reconciliation failed".into()),
                    );
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                stop_library_watch(&app);
                return Err(error);
            }
        };
        app.scan_control
            .threads
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(ScanWorker {
                role: "periodic_reconcile",
                handle: periodic,
            });
    }
    Ok(())
}

pub(super) fn stop_library_watch(app: &App) {
    stop_library_watch_until(
        app,
        Instant::now() + Duration::from_secs(app.cfg.shutdown_timeout_secs),
    );
}

pub(super) fn stop_library_watch_until(app: &App, deadline: Instant) {
    app.scan_control.cancellation.cancel();
    app.scan_control.wake.notify_all();
    let handles = app
        .scan_control
        .threads
        .lock()
        .map(|mut handles| handles.drain(..).collect::<Vec<_>>())
        .unwrap_or_default();
    for worker in handles {
        while !worker.handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !worker.handle.is_finished() {
            tracing::error!(
                role = worker.role,
                "library worker did not exit before the graceful shutdown deadline"
            );
            // Dropping a JoinHandle detaches the thread. The daemon can now
            // return from `serve`; process exit releases any kernel-held file
            // or SQLite locks even if a filesystem syscall never returned.
            continue;
        }
        if worker.handle.join().is_err() {
            tracing::error!(
                role = worker.role,
                "library worker panicked during shutdown"
            );
        }
    }
}

#[cfg(test)]
fn with_admitted_rooted_file<T>(
    scan_cfg: &rusty_dlna_scan::ScanConfig,
    helpers: &Arc<rusty_dlna_scan::HelperGate>,
    path: &Path,
    operation: impl FnOnce(&Path) -> T,
) -> rusty_dlna_scan::ScanResult<Option<T>> {
    let opened = match rusty_dlna_scan::open_allowed_file(path, scan_cfg) {
        Ok(opened) => opened,
        Err(_) => return Ok(None),
    };
    let _helper_permit = match helpers
        .acquire_timeout_cancelled(scan_cfg.helper_queue_timeout, &scan_cfg.cancellation)
    {
        Ok(permit) => permit,
        Err(rusty_dlna_scan::HelperAdmissionError::Cancelled) => {
            return Err(rusty_dlna_scan::ScanError::Cancelled);
        }
        Err(error) => return Err(error.into()),
    };
    Ok(Some(operation(&opened.proc_path())))
}

fn fill_missing_av_meta(app: &App) -> rusty_dlna_scan::ScanResult<()> {
    if app.scan_cfg.db_path.is_none() {
        return Ok(());
    }
    let prepared = prepare_scan_change(app, ScanSession::prepare_fill_missing_av_meta)?;
    apply_prepared_catalog_change(app, prepared, "stream metadata update")
}

fn bind_udp_reuse(addr: SocketAddrV4) -> std::io::Result<socket2::Socket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    sock.set_reuse_address(true)?;
    // rustyDLNA uses SO_REUSEADDR only. SO_REUSEPORT lets the kernel hash
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
        let group = Ipv4Addr::new(239, 255, 255, 250);
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

async fn send_alive(sock: &tokio::net::UdpSocket, app: &App, interface_ip: Ipv4Addr) {
    if app.ssdp_port != rusty_dlna_protocol::ssdp::SSDP_PORT {
        return;
    }
    let dest = (
        rusty_dlna_protocol::ssdp::SSDP_MCAST_ADDR,
        rusty_dlna_protocol::ssdp::SSDP_PORT,
    );
    let pkts = match rusty_dlna_ssdp::try_notify_alive(
        &app.uuid,
        &interface_ip.to_string(),
        app.http_port,
        app.notify_interval,
        &app.server,
    ) {
        Ok(packets) => packets,
        Err(error) => {
            tracing::error!(%error, "cannot construct SSDP alive packets");
            return;
        }
    };
    for p in &pkts {
        if let Err(e) = sock.send_to(p.as_bytes(), dest).await {
            tracing::warn!("SSDP NOTIFY: {e}");
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InterfaceV4 {
    pub(super) name: String,
    pub(super) addr: Ipv4Addr,
    pub(super) netmask: Ipv4Addr,
}

pub(super) fn usable_lan_ipv4(addr: Ipv4Addr) -> bool {
    !addr.is_unspecified()
        && !addr.is_loopback()
        && !addr.is_multicast()
        && !addr.is_broadcast()
        && !addr.is_link_local()
}

pub(super) fn select_ssdp_interfaces(
    configured: &[String],
    primary: Ipv4Addr,
    ssdp_port: u16,
    interfaces: &[InterfaceV4],
) -> Result<Vec<(Ipv4Addr, Ipv4Addr)>, String> {
    let live = ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    let mut selected = Vec::new();
    if configured.is_empty() {
        selected.extend(
            interfaces
                .iter()
                .filter(|interface| interface.addr == primary)
                .map(|interface| (interface.addr, interface.netmask)),
        );
    } else {
        for token in configured {
            let token = token.trim();
            let parsed = token.parse::<Ipv4Addr>().ok();
            selected.extend(
                interfaces
                    .iter()
                    .filter(|interface| {
                        parsed.is_some_and(|address| address == interface.addr)
                            || interface.name == token
                    })
                    .map(|interface| (interface.addr, interface.netmask)),
            );
        }
    }
    selected.retain(|(address, _)| usable_lan_ipv4(*address) || (!live && address.is_loopback()));
    selected.sort_unstable();
    selected.dedup();
    if selected.is_empty() {
        selected.push((primary, Ipv4Addr::new(255, 255, 255, 255)));
    } else if !selected.iter().any(|(address, _)| *address == primary) {
        let mask = interfaces
            .iter()
            .find(|interface| interface.addr == primary)
            .map(|interface| interface.netmask)
            .unwrap_or(Ipv4Addr::new(255, 255, 255, 255));
        selected.push((primary, mask));
        selected.sort_unstable();
        selected.dedup();
    }
    Ok(selected)
}

#[cfg(unix)]
pub(super) fn active_ipv4_interfaces() -> Vec<InterfaceV4> {
    // SAFETY: `getifaddrs` initializes a linked list on success. Each node and
    // sockaddr is inspected only while that list is live, family checks guard
    // IPv4 casts, and `freeifaddrs` is called exactly once before returning.
    unsafe {
        let mut ifa = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return Vec::new();
        }
        let mut cur = ifa;
        let mut found = Vec::new();
        while !cur.is_null() {
            let entry = &*cur;
            let cname = if entry.ifa_name.is_null() {
                ""
            } else {
                std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_str()
                    .unwrap_or("")
            };
            if entry.ifa_flags & libc::IFF_UP as u32 != 0 {
                if let Some(addr) = entry.ifa_addr.as_ref() {
                    if addr.sa_family as i32 == libc::AF_INET {
                        let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        let octets = u32::from_be(sin.sin_addr.s_addr);
                        found.push(InterfaceV4 {
                            name: cname.to_owned(),
                            addr: Ipv4Addr::from(octets),
                            netmask: entry
                                .ifa_netmask
                                .as_ref()
                                .filter(|netmask| netmask.sa_family as i32 == libc::AF_INET)
                                .map(|_| {
                                    let sin = &*(entry.ifa_netmask as *const libc::sockaddr_in);
                                    Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))
                                })
                                .unwrap_or(Ipv4Addr::UNSPECIFIED),
                        });
                    }
                }
            }
            cur = entry.ifa_next;
        }
        libc::freeifaddrs(ifa);
        found.sort_by(|left, right| left.name.cmp(&right.name).then(left.addr.cmp(&right.addr)));
        found.dedup();
        found
    }
}

#[cfg(not(unix))]
pub(super) fn active_ipv4_interfaces() -> Vec<InterfaceV4> {
    Vec::new()
}

pub(super) fn default_route_interface() -> Option<String> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    routes
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            if columns.len() < 7 || columns[1] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(columns[3], 16).ok()?;
            if flags & libc::RTF_UP as u32 == 0 {
                return None;
            }
            let metric = columns[6].parse::<u64>().ok()?;
            Some((metric, columns[0].to_owned()))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, interface)| interface)
}

fn route_source_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // UDP connect performs route selection without sending a packet.
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(address) if usable_lan_ipv4(*address.ip()) => Some(*address.ip()),
        _ => None,
    }
}

pub(super) fn select_advertise_ip(
    configured: Option<&str>,
    listen_ip: Ipv4Addr,
    configured_interfaces: &[String],
    ssdp_port: u16,
    interfaces: &[InterfaceV4],
    default_route: Option<&str>,
) -> Result<Ipv4Addr, String> {
    let live = ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    for configured_interface in configured_interfaces {
        let configured_interface = configured_interface.trim();
        if configured_interface.is_empty() {
            return Err("network_interface entries must not be empty".into());
        }
        let found = configured_interface
            .parse::<Ipv4Addr>()
            .ok()
            .is_some_and(|addr| interfaces.iter().any(|interface| interface.addr == addr))
            || interfaces
                .iter()
                .any(|interface| interface.name == configured_interface);
        if !found {
            return Err(format!(
                "network_interface {configured_interface:?} has no enabled IPv4 address"
            ));
        }
    }

    if let Some(configured) = configured {
        let addr = configured
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("advertise_ip is not a valid IPv4 address: {configured}"))?;
        if addr.is_unspecified() || addr.is_multicast() || addr.is_broadcast() {
            return Err(format!(
                "advertise_ip {addr} is not a usable unicast address"
            ));
        }
        if live && addr.is_loopback() {
            return Err(format!(
                "advertise_ip {addr} is loopback and cannot be announced on live SSDP port 1900"
            ));
        }
        if live && !interfaces.iter().any(|interface| interface.addr == addr) {
            return Err(format!(
                "advertise_ip {addr} is not assigned to an enabled local interface"
            ));
        }
        return Ok(addr);
    }

    if !listen_ip.is_unspecified() {
        if live && !usable_lan_ipv4(listen_ip) {
            return Err(format!(
                "listen_ip {listen_ip} is not usable as a live SSDP advertisement address"
            ));
        }
        if live
            && !interfaces
                .iter()
                .any(|interface| interface.addr == listen_ip)
        {
            return Err(format!(
                "listen_ip {listen_ip} is not assigned to an enabled local interface"
            ));
        }
        return Ok(listen_ip);
    }

    let mut candidates = if configured_interfaces.is_empty() {
        Vec::new()
    } else {
        configured_interfaces
            .iter()
            .flat_map(|configured_interface| {
                let configured_interface = configured_interface.trim();
                interfaces.iter().filter_map(move |interface| {
                    let matches = configured_interface
                        .parse::<Ipv4Addr>()
                        .is_ok_and(|addr| interface.addr == addr)
                        || interface.name == configured_interface;
                    matches.then_some(interface.addr)
                })
            })
            .collect::<Vec<_>>()
    };
    candidates.retain(|addr| usable_lan_ipv4(*addr));
    candidates.sort_unstable();
    candidates.dedup();
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }
    if candidates.len() > 1 {
        if let Some(source) = route_source_ipv4().filter(|source| candidates.contains(source)) {
            return Ok(source);
        }
        // All candidates remain active SSDP endpoints. This value is only the
        // primary URL for HTTP responses that have no ingress-interface hint.
        return Ok(candidates[0]);
    }

    if let Some(default_route) = default_route {
        let mut route_candidates = interfaces
            .iter()
            .filter(|interface| interface.name == default_route)
            .map(|interface| interface.addr)
            .filter(|addr| usable_lan_ipv4(*addr))
            .collect::<Vec<_>>();
        route_candidates.sort_unstable();
        route_candidates.dedup();
        if let Some(source) = route_source_ipv4().filter(|source| route_candidates.contains(source))
        {
            return Ok(source);
        }
        if route_candidates.len() == 1 {
            return Ok(route_candidates[0]);
        }
    }

    let mut all = interfaces
        .iter()
        .map(|interface| interface.addr)
        .filter(|addr| usable_lan_ipv4(*addr))
        .collect::<Vec<_>>();
    all.sort_unstable();
    all.dedup();
    match all.as_slice() {
        [only] => Ok(*only),
        [] if !live => Ok(Ipv4Addr::LOCALHOST),
        [] => Err(
            "no usable LAN IPv4 address found for SSDP; set network_interface or advertise_ip"
                .into(),
        ),
        _ => Err(format!(
            "multiple LAN IPv4 addresses found {all:?}; set network_interface or advertise_ip"
        )),
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

pub(super) fn reply_interface_for_sender(
    sender: Ipv4Addr,
    interfaces: &[(Ipv4Addr, Ipv4Addr)],
    primary: Ipv4Addr,
) -> Ipv4Addr {
    let sender = u32::from(sender);
    let best_prefix = interfaces
        .iter()
        .filter(|(address, mask)| {
            let mask = u32::from(*mask);
            u32::from(*address) & mask == sender & mask
        })
        .map(|(_, mask)| u32::from(*mask).count_ones())
        .max();
    let Some(best_prefix) = best_prefix else {
        return primary;
    };
    // A NIC can have multiple addresses in the same subnet. Prefix length
    // cannot distinguish those aliases, so honor the configured primary
    // instead of depending on interface enumeration order.
    interfaces
        .iter()
        .find(|(address, mask)| {
            *address == primary
                && u32::from(*mask).count_ones() == best_prefix
                && (u32::from(*address) & u32::from(*mask) == sender & u32::from(*mask))
        })
        .or_else(|| {
            interfaces.iter().find(|(address, mask)| {
                u32::from(*mask).count_ones() == best_prefix
                    && (u32::from(*address) & u32::from(*mask) == sender & u32::from(*mask))
            })
        })
        .map(|(address, _)| *address)
        .unwrap_or(primary)
}

async fn ssdp_loop(app: Arc<App>) -> std::io::Result<()> {
    let recv_sock = ssdp_recv_bind(&app)?;
    let live = app.ssdp_port == rusty_dlna_protocol::ssdp::SSDP_PORT;
    let iface = ssdp_iface(&app);
    if live {
        let group = Ipv4Addr::new(239, 255, 255, 250);
        for ip in app.ssdp_interfaces.iter().map(|(address, _)| *address) {
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
    let recv = Arc::new(tokio::net::UdpSocket::from_std(recv_std)?);

    // Kodi/Platinum drops replies whose source port is not 1900. Maintain one
    // egress socket per selected interface so source address and LOCATION
    // always describe the same reachable endpoint.
    let mut sends: Vec<(Ipv4Addr, Arc<tokio::net::UdpSocket>)> = Vec::new();
    if live {
        for (bind_ip, _) in &app.ssdp_interfaces {
            let socket = bind_udp_reuse(SocketAddrV4::new(*bind_ip, app.ssdp_port))?;
            socket.set_multicast_if_v4(bind_ip)?;
            socket.set_multicast_ttl_v4(4)?;
            socket.set_multicast_loop_v4(false)?;
            let socket = into_std_udp(socket);
            tracing::info!(interface = %bind_ip, addr = %socket.local_addr()?, "SSDP reply/notify socket");
            sends.push((*bind_ip, Arc::new(tokio::net::UdpSocket::from_std(socket)?)));
        }
    }
    tracing::info!(%recv_addr, port = app.ssdp_port, "ssdp listen");

    if !sends.is_empty() {
        for (ip, socket) in &sends {
            send_alive(socket, &app, *ip).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(jitter_ms(
            ALIVE_DUP_DELAY_MS,
        )))
        .await;
        for (ip, socket) in &sends {
            send_alive(socket, &app, *ip).await;
        }
    }

    let mut buf = vec![0u8; 2048];
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(
        app.notify_interval.max(1) as u64,
    ));
    let renderer_workers = Arc::new(tokio::sync::Semaphore::new(4));
    let reply_workers = Arc::new(tokio::sync::Semaphore::new(64));
    let mut renderer_limiter = RendererFetchLimiter::default();
    let mut reply_limiter = SsdpReplyLimiter::default();
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !sends.is_empty() {
                    for (ip, socket) in &sends {
                        send_alive(socket, &app, *ip).await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(jitter_ms(ALIVE_DUP_DELAY_MS))).await;
                    for (ip, socket) in &sends {
                        send_alive(socket, &app, *ip).await;
                    }
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
                    let SocketAddr::V4(sender) = from else {
                        continue;
                    };
                    let loc = n.location;
                    if trusted_renderer_location(&loc, from, &active_ipv4_interfaces()).is_none() {
                        tracing::warn!(%from, location = %loc, "rejected renderer NOTIFY location");
                        continue;
                    }
                    let key = format!(
                        "{}|{}|{loc}",
                        sender.ip(),
                        n.usn.as_deref().unwrap_or("-")
                    );
                    if !renderer_limiter.allow(*sender.ip(), &key, std::time::Instant::now()) {
                        tracing::debug!(%from, "renderer fetch rate-limited/deduplicated");
                        continue;
                    }
                    let Ok(permit) = renderer_workers.clone().try_acquire_owned() else {
                        tracing::debug!(%from, "renderer worker pool full");
                        continue;
                    };
                    let app2 = Arc::clone(&app);
                    let server = n.server;
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        sniff_renderer_location(&loc, &server, from, &app2);
                    });
                    continue;
                }
                let Ok(ms) = parse_msearch(&text) else { continue };
                let SocketAddr::V4(sender) = from else {
                    continue;
                };
                let reply_ip = reply_interface_for_sender(
                    *sender.ip(),
                    &app.ssdp_interfaces,
                    iface,
                );
                let date = now_imf_date();
                let replies = match try_msearch_replies(
                    &app.uuid,
                    &ms.st,
                    &reply_ip.to_string(),
                    app.http_port,
                    app.notify_interval,
                    &app.server,
                    &date,
                ) {
                    Ok(replies) => replies,
                    Err(error) => {
                        tracing::error!(%error, "cannot construct SSDP search replies");
                        continue;
                    }
                };
                if replies.is_empty() {
                    continue;
                }
                if !reply_limiter.allow(*sender.ip(), replies.len(), std::time::Instant::now()) {
                    tracing::debug!(%from, replies = replies.len(), "M-SEARCH reply rate-limited");
                    continue;
                }
                let Ok(permit) = reply_workers.clone().try_acquire_owned() else {
                    tracing::debug!(%from, "M-SEARCH scheduler full");
                    continue;
                };
                tracing::info!(%from, st = %ms.st, n = replies.len(), "SSDP M-SEARCH reply");
                let all = ms.st == rusty_dlna_protocol::ssdp::ST_ALL;
                let out = sends
                    .iter()
                    .find(|(ip, _)| *ip == reply_ip)
                    .map(|(_, socket)| Arc::clone(socket))
                    .unwrap_or_else(|| Arc::clone(&recv));
                tokio::spawn(async move {
                    let _permit = permit;
                    tokio::time::sleep(Duration::from_millis(jitter_ms(
                        msearch_jitter_ms_range_for_mx(all, ms.mx),
                    )))
                    .await;
                    for reply in replies {
                        if let Err(error) = out.send_to(reply.as_bytes(), from).await {
                            tracing::warn!(%from, %error, "SSDP reply send failed");
                        }
                    }
                });
            }
        }
    }
}

pub(super) async fn handle_conn(
    app: Arc<App>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;
    const MAX_HEADER_BYTES: usize = 64 * 1024;

    let mut persist_left = 100u32;
    let mut pending = Vec::new();
    let mut request_number = 0u32;
    let mut tmp = [0u8; 4096];
    loop {
        let header_wait = if request_number > 0 && pending.is_empty() {
            app.cfg.keep_alive_timeout_secs
        } else {
            app.cfg.header_read_timeout_secs
        };
        let header_deadline = tokio::time::Instant::now() + Duration::from_secs(header_wait);
        let header_end = loop {
            if let Some(end) = rusty_dlna_http::header_block_complete(&pending) {
                if end > MAX_HEADER_BYTES {
                    let response = HttpResponse::html(
                        431,
                        "Request Header Fields Too Large",
                        "headers too large",
                    );
                    crate::socket_write_http_response(&app, &mut sock, &response).await?;
                    return Ok(());
                }
                break end;
            }
            if pending.len() > MAX_HEADER_BYTES {
                let response =
                    HttpResponse::html(431, "Request Header Fields Too Large", "headers too large");
                crate::socket_write_http_response(&app, &mut sock, &response).await?;
                return Ok(());
            }
            let n = match tokio::time::timeout_at(header_deadline, sock.read(&mut tmp)).await {
                Ok(read) => read?,
                Err(_) => {
                    let response = HttpResponse::html(408, "Request Timeout", "header timeout");
                    let _ = crate::socket_write_http_response(&app, &mut sock, &response).await;
                    return Ok(());
                }
            };
            if n == 0 {
                if pending.is_empty() {
                    return Ok(());
                }
                let response = HttpResponse::html(400, "Bad Request", "incomplete headers");
                crate::socket_write_http_response(&app, &mut sock, &response).await?;
                return Ok(());
            }
            pending.extend_from_slice(&tmp[..n]);
        };
        let head = match std::str::from_utf8(&pending[..header_end]) {
            Ok(head) => head,
            Err(_) => {
                let response = HttpResponse::html(400, "Bad Request", "headers are not UTF-8");
                crate::socket_write_http_response(&app, &mut sock, &response).await?;
                return Ok(());
            }
        };
        let mut req = match HttpRequest::parse_headers(head) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%peer, ?error, "rejected malformed HTTP request");
                let response = HttpResponse::html(400, "Bad Request", "malformed request");
                crate::socket_write_http_response(&app, &mut sock, &response).await?;
                return Ok(());
            }
        };
        let need = req.content_length().unwrap_or(0);
        if rusty_dlna_http::http_body_too_large(need) || need > app.cfg.max_request_body_bytes {
            let resp = HttpResponse::html(413, "Payload Too Large", "body too large");
            crate::socket_write_http_response(&app, &mut sock, &resp).await?;
            return Ok(());
        }
        let request_end = header_end
            .checked_add(need)
            .ok_or("request length overflow")?;
        let body_deadline =
            tokio::time::Instant::now() + Duration::from_secs(app.cfg.body_read_timeout_secs);
        while pending.len() < request_end {
            let n = match tokio::time::timeout_at(body_deadline, sock.read(&mut tmp)).await {
                Ok(read) => read?,
                Err(_) => {
                    let response = HttpResponse::html(408, "Request Timeout", "body timeout");
                    let _ = crate::socket_write_http_response(&app, &mut sock, &response).await;
                    return Ok(());
                }
            };
            if n == 0 {
                let response = HttpResponse::html(400, "Bad Request", "incomplete body");
                crate::socket_write_http_response(&app, &mut sock, &response).await?;
                return Ok(());
            }
            pending.extend_from_slice(&tmp[..n]);
        }
        req.body = pending[header_end..request_end].to_vec();
        pending.drain(..request_end);
        let handler_app = app.clone();
        let handler_request = req.clone();
        let resp =
            tokio::task::spawn_blocking(move || handler_app.handle_from(&handler_request, peer))
                .await
                .map_err(|error| format!("request worker failed: {error}"))?;
        request_number = request_number.saturating_add(1);
        persist_left = persist_left.saturating_sub(1);
        if let Some(spec) = resp.remux_job.clone() {
            remux::serve_remux(&app, &mut sock, &req, spec)
                .await
                .map_err(|error| format!("{} {}: {error}", req.method, req.path))?;
            break;
        }
        let valid_wire = crate::socket_write_http_response(&app, &mut sock, &resp)
            .await
            .map_err(|error| format!("{} {} response: {error}", req.method, req.path))?;
        if !valid_wire {
            return Ok(());
        }
        if let Some(range) = resp.file_range.as_ref() {
            stream_open_file_range(
                &app,
                &mut sock,
                range.file.try_clone()?,
                range.start,
                range.end,
            )
            .await
            .map_err(|error| format!("{} {} media: {error}", req.method, req.path))?;
        }
        if !resp.persist || persist_left == 0 {
            break;
        }
    }
    Ok(())
}

pub(crate) async fn socket_write_all(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    bytes: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    tokio::time::timeout(
        Duration::from_secs(app.cfg.write_timeout_secs),
        sock.write_all(bytes),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "socket write timeout"))?
}

pub(crate) async fn stream_file_range(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    path: &std::path::Path,
    start: u64,
    end: u64,
) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    stream_open_file_range(app, sock, file, start, end).await
}

pub(crate) async fn stream_open_file_range(
    app: &App,
    sock: &mut tokio::net::TcpStream,
    file: std::fs::File,
    start: u64,
    end: u64,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::from_std(file);
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut left = end.saturating_sub(start).saturating_add(1);
    let mut buf = vec![0u8; 64 * 1024];
    while left > 0 {
        let n = std::cmp::min(left as usize, buf.len());
        let got = f.read(&mut buf[..n]).await?;
        if got == 0 {
            break;
        }
        socket_write_all(app, sock, &buf[..got]).await?;
        left -= got as u64;
    }
    Ok(())
}

pub(super) fn read_open_file_range(
    file: &mut std::fs::File,
    start: u64,
    end: u64,
) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek};

    file.seek(std::io::SeekFrom::Start(start))?;
    let length = end
        .saturating_sub(start)
        .saturating_add(1)
        .try_into()
        .map_err(|_| std::io::Error::other("range does not fit memory"))?;
    let mut body = vec![0; length];
    file.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod rooted_probe_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "rusty-dlna-rooted-fill-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_metadata_probe_keeps_rooted_inode_and_obeys_global_admission() {
        let temp = TempDir::new();
        let media = temp.0.join("media");
        std::fs::create_dir_all(&media).unwrap();
        let inside = media.join("inside.mp4");
        let outside = temp.0.join("outside.mp4");
        let alias = media.join("alias.mp4");
        std::fs::write(&inside, b"inside inode").unwrap();
        std::fs::write(&outside, b"outside inode").unwrap();
        std::os::unix::fs::symlink(&inside, &alias).unwrap();
        let gate = Arc::new(rusty_dlna_scan::HelperGate::new(1, 2));
        let cfg = rusty_dlna_scan::ScanConfig {
            media_dirs: vec![media.clone()],
            helper_queue_timeout: Duration::from_millis(20),
            ..rusty_dlna_scan::ScanConfig::default()
        };

        let bytes = with_admitted_rooted_file(&cfg, &gate, &alias, |stable_path| {
            std::fs::remove_file(&alias).unwrap();
            std::os::unix::fs::symlink(&outside, &alias).unwrap();
            std::fs::read(stable_path).unwrap()
        })
        .unwrap()
        .unwrap();
        assert_eq!(bytes, b"inside inode");

        let called = AtomicBool::new(false);
        let held = gate.try_acquire().unwrap();
        let error = with_admitted_rooted_file(&cfg, &gate, &inside, |_| {
            called.store(true, Ordering::Release);
        })
        .unwrap_err();
        assert!(matches!(
            error,
            rusty_dlna_scan::ScanError::HelperAdmission(
                rusty_dlna_scan::HelperAdmissionError::TimedOut
            )
        ));
        assert!(!called.load(Ordering::Acquire));

        let cancelled_cfg = rusty_dlna_scan::ScanConfig {
            media_dirs: vec![media],
            helper_queue_timeout: Duration::from_secs(1),
            ..rusty_dlna_scan::ScanConfig::default()
        };
        cancelled_cfg.cancellation.cancel();
        let error = with_admitted_rooted_file(&cancelled_cfg, &gate, &inside, |_| ()).unwrap_err();
        assert!(matches!(error, rusty_dlna_scan::ScanError::Cancelled));
        drop(held);
    }
}

#[cfg(test)]
mod shutdown_accounting_tests {
    use super::*;
    use crate::events::{EventHub, NotifyDispatcher, NotifyJob};
    use std::io::Read;
    use std::sync::mpsc;

    #[tokio::test]
    async fn in_flight_notification_teardown_is_included_in_shutdown_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let callback = format!("http://{}/event", listener.local_addr().unwrap());
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let callback_server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let read = socket.read(&mut request).unwrap();
            assert!(read > 0, "notification request must reach the callback");
            request_seen_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        let dispatcher = NotifyDispatcher::new(Arc::new(Mutex::new(EventHub::new()))).unwrap();
        assert!(dispatcher.enqueue(
            NotifyJob {
                sid: "uuid:shutdown-accounting".into(),
                callback,
                seq: 1,
            },
            "<event/>".into(),
        ));
        request_seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("notification worker must reach the withholding callback");

        let shutdown_started = Instant::now();
        let shutdown_budget = Duration::from_millis(75);
        let shutdown_deadline = shutdown_started + shutdown_budget;
        dispatcher.begin_shutdown();

        // Model scanner/remux teardown consuming part of the shared budget
        // while the callback keeps its response open.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let notifications_stopped = dispatcher.stop_until(shutdown_deadline).await;
        let shutdown_elapsed = shutdown_started.elapsed();
        let deadline_exceeded =
            shutdown_deadline_exceeded(shutdown_elapsed, shutdown_budget, 0, notifications_stopped);

        assert!(!notifications_stopped);
        assert!(deadline_exceeded);
        assert!(shutdown_elapsed >= shutdown_budget);
        assert_eq!(dispatcher.metrics().workers_total, 0);

        release_tx.send(()).unwrap();
        callback_server.join().unwrap();
    }
}
