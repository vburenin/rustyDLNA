//! Transcode-cache discovery, accounting, and eviction.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::{remux::RemuxJob, App};

fn generated_cache_mp4(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("mp4") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let mut fields = stem.splitn(3, '-');
    let Some(id) = fields.next() else {
        return false;
    };
    let action = match fields.next() {
        Some("hdr10") => rusty_dlna_transcode::RecodeAction::Hdr10,
        Some("remux") => rusty_dlna_transcode::RecodeAction::RemuxP8,
        Some("ac3") => rusty_dlna_transcode::RecodeAction::AudioAc3,
        Some("web") => rusty_dlna_transcode::RecodeAction::Browser,
        Some("orig") => rusty_dlna_transcode::RecodeAction::Original,
        _ => return false,
    };
    id.parse::<i64>().is_ok()
        && fields
            .next()
            .is_some_and(|key| rusty_dlna_transcode::cache_key_has_safe_shape(action, key))
}

fn generated_intermediate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    for suffix in [
        ".part.p8.hevc",
        ".part.hevc",
        ".p8.hevc",
        ".p8.mp4",
        ".hevc",
        ".part",
    ] {
        if let Some(base) = name.strip_suffix(suffix) {
            return generated_cache_mp4(&path.with_file_name(base));
        }
    }
    false
}

fn generated_cache_stamp_output(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let output = path.with_file_name(name.strip_suffix(".src")?);
    generated_cache_mp4(&output).then_some(output)
}

#[derive(Clone, Copy, Debug, Default)]
struct CacheMaintenance {
    bytes: u64,
    evicted_files: u64,
    evicted_bytes: u64,
    limits_satisfied: bool,
}

pub(super) fn active_artifacts<'a>(
    jobs: impl Iterator<Item = &'a Arc<RemuxJob>>,
) -> HashSet<PathBuf> {
    jobs.flat_map(|job| {
        [
            job.dest.clone(),
            job.part.clone(),
            job.part.with_extension("hevc"),
            job.part.with_extension("p8.hevc"),
            job.part.with_extension("p8.mp4"),
        ]
    })
    .collect()
}

fn unsatisfied_limits_error() -> std::io::Error {
    std::io::Error::other("quota or minimum-free-space target cannot be satisfied")
}

fn maintain_transcode_cache_report(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
    protected: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<CacheMaintenance> {
    maintain_transcode_cache_report_with_free_space(
        directory,
        quota_bytes,
        max_age_days,
        minimum_free_bytes,
        protected,
        startup,
        (
            crate::available_filesystem_bytes,
            std::fs::DirEntry::metadata,
        ),
    )
}

fn maintain_transcode_cache_report_with_free_space<F, M>(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
    protected: &HashSet<PathBuf>,
    startup: bool,
    io: (F, M),
) -> std::io::Result<CacheMaintenance>
where
    F: FnMut(&Path) -> std::io::Result<u64>,
    M: FnMut(&std::fs::DirEntry) -> std::io::Result<std::fs::Metadata>,
{
    let (mut available_bytes, mut entry_metadata) = io;
    std::fs::create_dir_all(directory)?;
    let now = std::time::SystemTime::now();
    let max_age = Duration::from_secs(u64::from(max_age_days).saturating_mul(86_400));
    let mut finished = Vec::new();
    let mut total = 0u64;
    let mut evicted_files = 0u64;
    let mut evicted_bytes = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry_metadata(&entry) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if let Some(output) = generated_cache_stamp_output(&path) {
            let stamp_is_protected = protected.contains(&path) || protected.contains(&output);
            if startup
                && !stamp_is_protected
                && !output.is_file()
                && std::fs::remove_file(&path).is_ok()
            {
                evicted_files = evicted_files.saturating_add(1);
                evicted_bytes = evicted_bytes.saturating_add(metadata.len());
            }
            continue;
        }
        if generated_intermediate(&path) {
            if startup && !protected.contains(&path) {
                if std::fs::remove_file(&path).is_ok() {
                    evicted_files = evicted_files.saturating_add(1);
                    evicted_bytes = evicted_bytes.saturating_add(metadata.len());
                } else {
                    total = total.saturating_add(metadata.len());
                }
            } else {
                total = total.saturating_add(metadata.len());
            }
            continue;
        }
        if !generated_cache_mp4(&path) {
            continue;
        }
        total = total.saturating_add(metadata.len());
        let used = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        if !protected.contains(&path)
            && now.duration_since(used).unwrap_or_default() > max_age
            && std::fs::remove_file(&path).is_ok()
        {
            total = total.saturating_sub(metadata.len());
            evicted_files = evicted_files.saturating_add(1);
            evicted_bytes = evicted_bytes.saturating_add(metadata.len());
            let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&path));
            continue;
        }
        if !protected.contains(&path) {
            finished.push((used, metadata.len(), path));
        }
    }
    finished.sort_by_key(|entry| entry.0);
    let mut quota_reclaim = total.saturating_sub(quota_bytes);
    let mut free_shortfall = if minimum_free_bytes == 0 {
        0
    } else {
        minimum_free_bytes.saturating_sub(available_bytes(directory)?)
    };
    for (_, bytes, path) in finished {
        if quota_reclaim == 0 && free_shortfall == 0 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&path));
            quota_reclaim = quota_reclaim.saturating_sub(bytes);
            total = total.saturating_sub(bytes);
            evicted_files = evicted_files.saturating_add(1);
            evicted_bytes = evicted_bytes.saturating_add(bytes);
            if free_shortfall > 0 {
                free_shortfall = minimum_free_bytes.saturating_sub(available_bytes(directory)?);
            }
        }
    }
    Ok(CacheMaintenance {
        bytes: total,
        evicted_files,
        evicted_bytes,
        limits_satisfied: quota_reclaim == 0 && free_shortfall == 0,
    })
}

pub(crate) fn maintain_transcode_cache(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
    protected: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<u64> {
    let report = maintain_transcode_cache_report(
        directory,
        quota_bytes,
        max_age_days,
        minimum_free_bytes,
        protected,
        startup,
    )?;
    if report.limits_satisfied {
        Ok(report.bytes)
    } else {
        Err(unsatisfied_limits_error())
    }
}

pub(super) fn maintain_app_cache(
    app: &App,
    protected: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<u64> {
    let _maintenance = crate::lock_recover(&app.cache_maintenance);
    app.remux_metrics
        .cache_maintenance
        .fetch_add(1, Ordering::Relaxed);
    match maintain_transcode_cache_report(
        &app.cache_dir,
        app.cfg.transcode.cache_max_mb.saturating_mul(1024 * 1024),
        app.cfg.transcode.cache_max_age_days,
        app.cfg.cache_min_free_mb.saturating_mul(1024 * 1024),
        protected,
        startup,
    ) {
        Ok(report) => {
            app.remux_metrics
                .cache_bytes
                .store(report.bytes, Ordering::Relaxed);
            app.remux_metrics
                .cache_evicted_files
                .fetch_add(report.evicted_files, Ordering::Relaxed);
            app.remux_metrics
                .cache_evicted_bytes
                .fetch_add(report.evicted_bytes, Ordering::Relaxed);
            if report.limits_satisfied {
                Ok(report.bytes)
            } else {
                app.remux_metrics
                    .cache_maintenance_failures
                    .fetch_add(1, Ordering::Relaxed);
                Err(unsatisfied_limits_error())
            }
        }
        Err(error) => {
            app.remux_metrics
                .cache_maintenance_failures
                .fetch_add(1, Ordering::Relaxed);
            Err(error)
        }
    }
}

pub(super) fn enforce_active_cache_limits(app: &App) -> std::io::Result<u64> {
    // Keep registration and the protected snapshot atomic. Every path that
    // takes both locks follows map -> maintenance ordering.
    let jobs = crate::lock_recover(&app.remuxes);
    let protected = active_artifacts(jobs.values());
    maintain_app_cache(app, &protected, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_browser_key(fill: char) -> String {
        format!(
            "{}-aligned-seek-v2-browser-no-chapters-v1-sdr-tonemap-libplacebo-v2-browser-hdr-source-v1-browser-aac-adtstoasc-v1-browser-mixed-copy-seek-v1-browser-cuda-download-v1-start-120",
            fill.to_string().repeat(64)
        )
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rusty-dlna-remux-cache-{label}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create cache test directory");
            Self(path)
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = PathBuf;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn eviction_preserves_protected_outputs_and_removes_victim_stamps() {
        let directory = TempDir::new("protected");
        let protected = directory.join(format!("1-hdr10-{}.mp4", "a".repeat(64)));
        let victim = directory.join(format!("2-web-{}.mp4", current_browser_key('b')));
        let unrelated = directory.join("user-owned.mp4");
        std::fs::write(&protected, vec![1u8; 600]).unwrap();
        std::fs::write(&victim, vec![2u8; 600]).unwrap();
        std::fs::write(&unrelated, vec![3u8; 600]).unwrap();
        let protected_stamp = rusty_dlna_transcode::cache_stamp_path(&protected);
        let victim_stamp = rusty_dlna_transcode::cache_stamp_path(&victim);
        std::fs::write(&protected_stamp, "protected").unwrap();
        std::fs::write(&victim_stamp, "victim").unwrap();

        let protected_paths = HashSet::from([protected.clone()]);
        let bytes =
            maintain_transcode_cache(&directory, 600, 36_500, 0, &protected_paths, false).unwrap();

        assert_eq!(bytes, 600);
        assert!(protected.exists());
        assert!(protected_stamp.exists());
        assert!(!victim.exists());
        assert!(!victim_stamp.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn startup_removes_only_unprotected_generated_intermediates() {
        let directory = TempDir::new("intermediates");
        let first = directory.join(format!("1-web-{}.mp4", current_browser_key('a')));
        let second = directory.join(format!(
            "2-web-{}-timeline-zero-v1-start-30.mp4",
            "b".repeat(64)
        ));
        let protected_part = rusty_dlna_transcode::cache_part(&first);
        let protected_p8_mp4 = protected_part.with_extension("p8.mp4");
        let stale_part = rusty_dlna_transcode::cache_part(&second);
        let stale_p8 = stale_part.with_extension("p8.hevc");
        let stale_p8_mp4 = stale_part.with_extension("p8.mp4");
        std::fs::write(&protected_part, b"active").unwrap();
        std::fs::write(&protected_p8_mp4, b"active-p8").unwrap();
        std::fs::write(&stale_part, b"stale").unwrap();
        std::fs::write(&stale_p8, b"stale-p8").unwrap();
        std::fs::write(&stale_p8_mp4, b"stale-p8-mp4").unwrap();

        let protected_paths = HashSet::from([protected_part.clone(), protected_p8_mp4.clone()]);
        let report = maintain_transcode_cache_report(
            &directory,
            u64::MAX,
            36_500,
            0,
            &protected_paths,
            true,
        )
        .unwrap();

        assert_eq!(report.bytes, 15);
        assert_eq!(report.evicted_files, 3);
        assert_eq!(report.evicted_bytes, 25);
        assert!(protected_part.exists());
        assert!(protected_p8_mp4.exists());
        assert!(!stale_part.exists());
        assert!(!stale_p8.exists());
        assert!(!stale_p8_mp4.exists());
    }

    #[test]
    fn startup_removes_only_orphan_generated_stamps() {
        let directory = TempDir::new("stamps");
        let current = directory.join(format!("1-web-{}.mp4", current_browser_key('a')));
        let current_stamp = rusty_dlna_transcode::cache_stamp_path(&current);
        let orphan = directory.join(format!(
            "2-web-{}-timeline-zero-v1-start-30.mp4",
            "b".repeat(64)
        ));
        let orphan_stamp = rusty_dlna_transcode::cache_stamp_path(&orphan);
        let protected = directory.join(format!("3-remux-{}.mp4", "c".repeat(64)));
        let protected_stamp = rusty_dlna_transcode::cache_stamp_path(&protected);
        let unrelated = directory.join("user-owned.mp4.src");
        std::fs::write(&current, b"complete").unwrap();
        std::fs::write(&current_stamp, b"current").unwrap();
        std::fs::write(&orphan_stamp, b"orphan").unwrap();
        std::fs::write(&protected_stamp, b"active").unwrap();
        std::fs::write(&unrelated, b"unrelated").unwrap();

        let protected_paths = HashSet::from([protected]);
        let report = maintain_transcode_cache_report(
            &directory,
            u64::MAX,
            36_500,
            0,
            &protected_paths,
            true,
        )
        .unwrap();

        assert_eq!(report.evicted_files, 1);
        assert_eq!(report.evicted_bytes, 6);
        assert!(current.exists());
        assert!(current_stamp.exists());
        assert!(!orphan_stamp.exists());
        assert!(protected_stamp.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn age_limit_evicts_generated_browser_output_and_stamp() {
        let directory = TempDir::new("browser-age");
        let output = rusty_dlna_transcode::cache_dest_for_key(
            &directory,
            1,
            rusty_dlna_transcode::RecodeAction::Browser,
            &current_browser_key('a'),
        );
        let stamp = rusty_dlna_transcode::cache_stamp_path(&output);
        std::fs::write(&output, b"old-browser-output").unwrap();
        std::fs::write(&stamp, b"stamp").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&output)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();

        let report =
            maintain_transcode_cache_report(&directory, u64::MAX, 1, 0, &HashSet::new(), false)
                .unwrap();

        assert_eq!(report.bytes, 0);
        assert_eq!(report.evicted_files, 1);
        assert!(!output.exists());
        assert!(!stamp.exists());
    }

    #[test]
    fn minimum_free_rechecks_actual_space_after_each_unlink() {
        let directory = TempDir::new("minimum-free-recheck");
        let first = directory.join(format!("1-hdr10-{}.mp4", "a".repeat(64)));
        let second = directory.join(format!("2-hdr10-{}.mp4", "b".repeat(64)));
        std::fs::write(&first, vec![0u8; 100]).unwrap();
        std::fs::write(&second, vec![0u8; 100]).unwrap();
        let mut readings = [0, 0, 100].into_iter();

        let report = maintain_transcode_cache_report_with_free_space(
            &directory,
            u64::MAX,
            36_500,
            100,
            &HashSet::new(),
            false,
            (
                |_| {
                    Ok(readings
                        .next()
                        .expect("one initial and two post-unlink reads"))
                },
                std::fs::DirEntry::metadata,
            ),
        )
        .unwrap();

        assert!(report.limits_satisfied);
        assert_eq!(report.bytes, 0);
        assert_eq!(report.evicted_files, 2);
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(readings.next().is_none());
    }

    #[test]
    fn minimum_free_query_errors_fail_closed() {
        let directory = TempDir::new("minimum-free-error");
        let error = maintain_transcode_cache_report_with_free_space(
            &directory,
            u64::MAX,
            36_500,
            1,
            &HashSet::new(),
            false,
            (
                |_| Err(std::io::Error::other("statvfs unavailable")),
                std::fs::DirEntry::metadata,
            ),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "statvfs unavailable");
    }

    #[test]
    fn metadata_errors_fail_closed() {
        let directory = TempDir::new("metadata-error");
        let output = directory.join(format!("1-hdr10-{}.mp4", "a".repeat(64)));
        std::fs::write(&output, vec![0u8; 100]).unwrap();

        let error = maintain_transcode_cache_report_with_free_space(
            &directory,
            u64::MAX,
            36_500,
            0,
            &HashSet::new(),
            false,
            (
                |_| Ok(u64::MAX),
                |_| Err(std::io::Error::other("metadata failed")),
            ),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "metadata failed");
        assert!(output.exists());
    }
}
