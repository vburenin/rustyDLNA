//! One-shot: rebuild OBJECTS from DETAILS paths. Does not re-probe media.
//!
//!   cargo run -p rusty-dlna-scan --example rebuild_objects -- \
//!     /var/cache/rusty-dlna/files.db /storage/video

use std::path::PathBuf;

use rusty_dlna_scan::{rebuild_objects, MediaTypes, ScanConfig};

fn main() {
    let mut args = std::env::args().skip(1);
    let db = PathBuf::from(args.next().expect("files.db path"));
    let root = PathBuf::from(args.next().expect("media root"));
    let cfg = ScanConfig {
        media_roots: Vec::new(),
        media_dirs: vec![root],
        db_path: Some(db),
        types: MediaTypes::video_only(),
        ..Default::default()
    };
    let cat = rebuild_objects(&cfg).unwrap_or_else(|error| {
        eprintln!("rebuild failed: {error}");
        std::process::exit(1);
    });
    eprintln!(
        "rebuild done: {} items, {} containers",
        cat.items.len(),
        cat.containers.len()
    );
}
