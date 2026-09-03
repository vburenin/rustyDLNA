use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rusty_dlna::{
    load_config, serve, try_resolve_http_port, try_resolve_ssdp_port, App, AppPreflight, Config,
};
use rusty_dlna_protocol::server_header;
use rusty_dlna_ssdp::try_notify_alive;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "rusty-dlna", about = "Multithreaded DLNA server", version)]
struct Args {
    /// Config file (TOML). Living-room paths belong in a gitignored local file.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Print the protocol/configuration self-check and exit.
    #[arg(long)]
    check: bool,
    /// Print the resolved, secret-free configuration and exit.
    #[arg(long)]
    print_effective_config: bool,
    /// Reconcile the media library once and exit.
    #[arg(long)]
    rescan: bool,
    /// Rebuild database objects from the configured media roots and exit.
    #[arg(long)]
    rebuild_database: bool,
    /// Run SQLite quick_check/migrations and exit.
    #[arg(long)]
    database_check: bool,
    /// HTTP port. Overridden by `RUSTY_DLNA_HTTP_PORT`.
    #[arg(short, long, default_value_t = 8200)]
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let (cfg, cfg_dir) = if let Some(p) = &args.config {
        let cfg = load_config(p)?;
        let dir = p
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        (cfg, dir)
    } else {
        (Config::default(), std::env::current_dir()?)
    };

    let http_port = try_resolve_http_port(args.port)
        .map_err(|error| format!("configuration error: {error}"))?;
    let ssdp_port =
        try_resolve_ssdp_port().map_err(|error| format!("configuration error: {error}"))?;
    let server = server_header("Linux");
    let preflight = App::preflight_config(cfg, http_port, ssdp_port, &cfg_dir)
        .map_err(|error| format!("configuration error: {error}"))?;

    if args.print_effective_config {
        println!("{:#?}", preflight.config());
        println!("http_port = {}", preflight.http_port());
        println!("ssdp_port = {}", preflight.ssdp_port());
        println!("listen_ip = {}", preflight.listen_ip());
        println!("advertise_ip = {}", preflight.advertise_ip());
        println!("cache_dir = {}", preflight.cache_dir().display());
        println!("database = {}", preflight.database_path().display());
        return Ok(());
    }

    // Preserve the established precedence of database maintenance over
    // --check when callers supply multiple modes. A diagnostic-only check
    // never constructs runtime state or touches storage.
    if args.check && !args.database_check && !args.rebuild_database && !args.rescan {
        run_check(&preflight, &server)?;
        return Ok(());
    }

    let app = preflight
        .into_app()
        .map_err(|error| format!("configuration error: {error}"))?;

    if args.database_check {
        let path = app
            .scan_cfg
            .db_path
            .as_deref()
            .ok_or("database is disabled")?;
        let db = rusty_dlna_scan::LibraryDb::open(path)?;
        let check = db.quick_check()?;
        if check != "ok" {
            return Err(format!("database quick_check failed: {check}").into());
        }
        println!("database OK: {}", path.display());
        return Ok(());
    }

    if args.rebuild_database {
        let catalog = rusty_dlna_scan::rebuild_objects(&app.scan_cfg)?;
        println!("database rebuilt: {} catalog objects", catalog.items.len());
        return Ok(());
    }

    if args.rescan {
        let (_, delta) = rusty_dlna_scan::monitor(&app.scan_cfg)?;
        println!(
            "rescan complete: added={} removed={} changed={}",
            delta.added, delta.removed, delta.changed
        );
        return Ok(());
    }

    info!(
        %server,
        http = app.http_port,
        ssdp = app.ssdp_port,
        listen = %app.listen_ip,
        advertise = %app.advertise_ip,
        name = %app.cfg.friendly_name,
        items = app.catalog.read().map(|c| c.items.len()).unwrap_or(0),
        remaps = app.remaps.len(),
        "rustyDLNA starting (scan in background)"
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(Arc::new(app)))?;
    Ok(())
}

fn run_check(preflight: &AppPreflight, server: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = preflight.config();
    let uuid = preflight.diagnostic_uuid()?;
    let pkts = try_notify_alive(
        &uuid,
        preflight.advertise_ip(),
        preflight.http_port(),
        preflight.notify_interval(),
        server,
    )?;
    assert_eq!(pkts.len(), 6);
    assert!(pkts[0].contains("/rootDesc.xml"));
    assert!(pkts[0].contains("ssdp:alive"));

    let kodi = rusty_dlna_protocol::identify_user_agent("Kodi/21.0").expect("kodi");
    assert_eq!(kodi.kind, rusty_dlna_protocol::ClientKind::Kodi);

    let date = rusty_dlna_protocol::w3c_normalize_date("2024-03-15T14:30:00");
    assert_eq!(date, "2024-03-15T14:30:00Z");

    println!("rustyDLNA check OK");
    println!("  server        {server}");
    println!("  friendly_name {}", cfg.friendly_name);
    println!("  uuid          {uuid}");
    println!("  listen        {}", preflight.listen_ip());
    println!("  advertise     {}", preflight.advertise_ip());
    println!("  port          {}", preflight.http_port());
    println!("  ssdp notifies {}", pkts.len());
    println!("  kodi          {:?}", kodi.kind);
    println!("  dc:date       {date}");
    println!(
        "  transcode     enable={} encoder={} max_jobs={}",
        cfg.transcode.enable, cfg.transcode.encoder, cfg.transcode.max_jobs
    );
    println!(
        "  web player   enable={} encoder={}",
        cfg.web.enable, cfg.web.encoder
    );
    println!(
        "  helpers       max_jobs={} queue={} timeout={}s",
        cfg.helper_max_jobs, cfg.helper_queue_capacity, cfg.helper_queue_timeout_secs
    );
    println!("  ifaces        {:?}", cfg.network_interface);
    println!("  media_dir     {:?}", cfg.media_dir);
    println!("  wide_links    {}", cfg.wide_links);
    for note in rusty_dlna::validate_transcode_tools_with_web(
        cfg.transcode.enable,
        &cfg.transcode.encoder,
        &cfg.remap,
        cfg.web.enable,
        &cfg.web.encoder,
    )? {
        println!("  tool          {note}");
    }
    for note in preflight.validate_ai_upscale_tools()? {
        println!("  tool          {note}");
    }
    println!("  remaps        {}", cfg.remap.len());
    for r in &cfg.remap {
        println!(
            "    - {} client={:?} hdr={:?} video={:?} audio={:?} action={:?}",
            r.name.as_deref().unwrap_or("unnamed"),
            r.client.0,
            r.hdr,
            r.video,
            r.audio,
            r.action
        );
    }
    Ok(())
}
