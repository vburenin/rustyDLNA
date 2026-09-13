//! Private, immediately unlinked files. No corpus path is ever opened for writing.
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn anonymous(bytes: &[u8]) -> File {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "rdlna-fuzz-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    std::fs::remove_file(path).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    file
}

// Production rewrite opens these descriptors through its ordinary file path.
#[allow(dead_code)]
pub fn path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
