use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rusty_dlna::{load_config, resolve_http_port, resolve_ssdp_port, serve, App, Config};
use rusty_dlna_protocol::server_header;
use rusty_dlna_ssdp::notify_alive;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "rusty-dlna",
    about = "Multithreaded DLNA server (MiniDLNA dialect)"
)]
struct Args {
    /// Config file (TOML). Living-room paths belong in a gitignored local file.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Print dialect / self-check and exit.
    #[arg(long)]
    check: bool,
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
        let dir = p.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
        (cfg, dir)
    } else {
        (Config::default(), std::env::current_dir()?)
    };

    let http_port = resolve_http_port(args.port);
    let ssdp_port = resolve_ssdp_port();
    let server = server_header("Linux");

    if args.check {
        run_check(&cfg, http_port, &server);
        return Ok(());
    }

    let app = App::from_config(cfg, http_port, ssdp_port, &cfg_dir);
    info!(
        %server,
        http = app.http_port,
        ssdp = app.ssdp_port,
        listen = %app.listen_ip,
        advertise = %app.advertise_ip,
        name = %app.cfg.friendly_name,
        items = app.catalog.lock().map(|c| c.items.len()).unwrap_or(0),
        remaps = app.remaps.len(),
        "rustyDLNA starting (scan in background)"
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(Arc::new(app)))?;
    Ok(())
}

fn run_check(cfg: &Config, port: u16, server: &str) {
    let uuid = "uuid:00000000-0000-0000-0000-000000000000";
    let pkts = notify_alive(uuid, "127.0.0.1", port, 895, server);
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
    println!("  port          {port}");
    println!("  ssdp notifies {}", pkts.len());
    println!("  kodi          {:?}", kodi.kind);
    println!("  dc:date       {date}");
    println!(
        "  transcode     enable={} encoder={} max_jobs={}",
        cfg.transcode.enable, cfg.transcode.encoder, cfg.transcode.max_jobs
    );
    println!("  ifaces        {:?}", cfg.network_interface);
    println!("  media_dir     {:?}", cfg.media_dir);
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
}
