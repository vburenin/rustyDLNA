//! Measure one unchanged full reconciliation without server-startup catalog load.

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use rusty_dlna_scan::{
    build_media_roots, load_and_persist_media_root_mappings, monitor, MediaTypes, ScanConfig,
};

fn main() {
    let root = env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("usage: benchmark_reconcile MEDIA_ROOT FILES_DB");
        std::process::exit(2);
    });
    let db_path = env::args().nth(2).map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("usage: benchmark_reconcile MEDIA_ROOT FILES_DB");
        std::process::exit(2);
    });
    let configured = format!("V,{}", root.display());
    let mut media_roots = build_media_roots(&[configured], std::path::Path::new("."))
        .unwrap_or_else(|error| {
            eprintln!("benchmark_reconcile: {error}");
            std::process::exit(2);
        });
    load_and_persist_media_root_mappings(&mut media_roots, &db_path).unwrap_or_else(|error| {
        eprintln!("benchmark_reconcile: {error}");
        std::process::exit(2);
    });
    let media_dirs = media_roots
        .iter()
        .map(|root| root.configured_path.clone())
        .collect();
    let cfg = ScanConfig {
        media_roots,
        media_dirs,
        types: MediaTypes::video_only(),
        db_path: Some(db_path),
        thumbnails: false,
        subtitles: false,
        ..ScanConfig::default()
    };
    let started = Instant::now();
    match monitor(&cfg) {
        Ok((_, delta)) => println!(
            "{{\"elapsed_ms\":{},\"added\":{},\"removed\":{},\"changed\":{}}}",
            started.elapsed().as_millis(),
            delta.added,
            delta.removed,
            delta.changed
        ),
        Err(error) => {
            eprintln!("benchmark_reconcile: {error}");
            std::process::exit(1);
        }
    }
}
