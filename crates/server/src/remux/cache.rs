//! Transcode-cache discovery, accounting, and eviction.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

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
        // Completed media is immutable under its validation stamp. Successful
        // reads touch only stamp mtime; unstamped output retains its creation age.
        let used = match std::fs::symlink_metadata(rusty_dlna_transcode::cache_stamp_path(&path)) {
            Ok(stamp) if stamp.is_file() => stamp.modified()?,
            Ok(_) => metadata.modified()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => metadata.modified()?,
            Err(error) => return Err(error),
        };
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
    let mut quota_reclaim = total.saturating_sub(quota_bytes);
    let mut free_shortfall = if minimum_free_bytes == 0 {
        0
    } else {
        minimum_free_bytes.saturating_sub(available_bytes(directory)?)
    };
    if quota_reclaim > 0 || free_shortfall > 0 {
        finished.sort_by_key(|entry| entry.0);
    }
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
    tracing::debug!(
        cache_bytes = report.bytes,
        evicted_files = report.evicted_files,
        evicted_bytes = report.evicted_bytes,
        "transcode cache startup maintenance finished"
    );
    if report.limits_satisfied {
        Ok(report.bytes)
    } else {
        Err(unsatisfied_limits_error())
    }
}

// One reconciliation cadence for the process, independent of producer count.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const RECENCY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct CacheEntry {
    bytes: u64,
    used: SystemTime,
    completed: bool,
}

#[derive(Default)]
struct Inventory {
    entries: HashMap<PathBuf, CacheEntry>,
    bytes: u64,
    swept: Option<Instant>,
}

impl Inventory {
    fn replace(&mut self, path: PathBuf, entry: Option<CacheEntry>) {
        if let Some(previous) = self.entries.remove(&path) {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
        }
        if let Some(entry) = entry {
            self.bytes = self.bytes.saturating_add(entry.bytes);
            self.entries.insert(path, entry);
        }
    }
}

/// Reservations establish ownership before slow I/O and are checked together
/// with the live job registry before eviction. No snapshot grants unlink rights.
#[derive(Default)]
pub(crate) struct CacheCoordinator {
    inventory: Mutex<Inventory>,
    reserved: Mutex<HashSet<PathBuf>>,
    released: Condvar,
}

pub(super) struct Reservation<'a> {
    coordinator: &'a CacheCoordinator,
    path: PathBuf,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        crate::lock_recover(&self.coordinator.reserved).remove(&self.path);
        self.coordinator.released.notify_all();
    }
}

impl CacheCoordinator {
    pub(super) fn reserve(&self, path: &Path) -> Result<Reservation<'_>, String> {
        let deadline = Instant::now() + super::WEB_SUPERSEDED_JOB_HANDOFF;
        let mut reserved = crate::lock_recover(&self.reserved);
        while reserved.contains(path) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("transcode busy (cache output is being prepared)".into());
            }
            reserved = self
                .released
                .wait_timeout(reserved, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        reserved.insert(path.to_owned());
        Ok(Reservation {
            coordinator: self,
            path: path.to_owned(),
        })
    }

    fn try_reserve(&self, path: &Path) -> Option<Reservation<'_>> {
        crate::lock_recover(&self.reserved)
            .insert(path.to_owned())
            .then(|| Reservation {
                coordinator: self,
                path: path.to_owned(),
            })
    }
}

fn entry_from_metadata(
    path: &Path,
    metadata: std::fs::Metadata,
) -> std::io::Result<Option<CacheEntry>> {
    if !metadata.is_file() {
        return Ok(None);
    }
    let completed = generated_cache_mp4(path);
    if !completed && !generated_intermediate(path) {
        return Ok(None);
    }
    let mut used = metadata.modified()?;
    if completed {
        match std::fs::symlink_metadata(rusty_dlna_transcode::cache_stamp_path(path)) {
            Ok(stamp) if stamp.is_file() => used = stamp.modified()?,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(CacheEntry {
        bytes: metadata.len(),
        used,
        completed,
    }))
}

fn read_entry(path: &Path) -> std::io::Result<Option<CacheEntry>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => entry_from_metadata(path, metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn discover(app: &App) -> std::io::Result<Inventory> {
    let started = Instant::now();
    #[cfg(test)]
    run_test_hook(discovery_test_hooks(), &app.cache_dir);
    app.remux_metrics
        .cache_scans
        .fetch_add(1, Ordering::Relaxed);
    let mut inventory = Inventory::default();
    for entry in std::fs::read_dir(&app.cache_dir)? {
        let entry = entry?;
        app.remux_metrics
            .cache_scan_entries
            .fetch_add(1, Ordering::Relaxed);
        let path = entry.path();
        if let Some(entry) = read_entry(&path)? {
            inventory.bytes = inventory.bytes.saturating_add(entry.bytes);
            inventory.entries.insert(path, entry);
        }
    }
    inventory.swept = Some(Instant::now());
    app.remux_metrics
        .cache_sweep_duration
        .record(started.elapsed());
    Ok(inventory)
}

fn maintain_inventory(
    app: &App,
    requested: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<u64> {
    let waited = Instant::now();
    // The shared gate also orders derived-image admission/publication checks.
    // No caller may hold the job registry while acquiring it.
    let _maintenance = crate::lock_recover(&app.cache_maintenance);
    app.remux_metrics.cache_lock_wait.record(waited.elapsed());
    let mut inventory = crate::lock_recover(&app.transcode_cache.inventory);
    let sweep = startup
        || inventory
            .swept
            .is_none_or(|at| at.elapsed() >= SWEEP_INTERVAL);
    if sweep {
        *inventory = discover(app)?;
    }
    let waited = Instant::now();
    let jobs = crate::lock_recover(&app.remuxes);
    app.remux_metrics
        .cache_registry_wait
        .record(waited.elapsed());
    let active = active_artifacts(jobs.values());
    drop(jobs);
    // Only growing/registered artifacts and the admission candidate need fresh
    // stats between sweeps. Completed entries retain incremental accounting.
    for path in active.iter().chain(requested) {
        inventory.replace(path.clone(), read_entry(path)?);
    }
    let quota = app.cfg.transcode.cache_max_mb.saturating_mul(1024 * 1024);
    let minimum_free = app.cfg.cache_min_free_mb.saturating_mul(1024 * 1024);
    let mut free_shortfall = if minimum_free == 0 {
        0
    } else {
        minimum_free.saturating_sub(crate::available_filesystem_bytes(&app.cache_dir)?)
    };
    let max_age =
        Duration::from_secs(u64::from(app.cfg.transcode.cache_max_age_days).saturating_mul(86_400));
    let now = SystemTime::now();
    if sweep || inventory.bytes > quota || free_shortfall > 0 {
        let pressure = inventory.bytes > quota || free_shortfall > 0;
        let mut candidates = inventory
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.completed
                    && (pressure || now.duration_since(entry.used).unwrap_or_default() > max_age)
            })
            .map(|(path, entry)| (path.clone(), *entry))
            .collect::<Vec<_>>();
        if inventory.bytes > quota || free_shortfall > 0 {
            candidates.sort_unstable_by_key(|(_, entry)| entry.used);
        }
        #[cfg(test)]
        run_test_hook(eviction_test_hooks(), &app.cache_dir);
        // A recently touched candidate is deferred behind the original LRU
        // candidates, then reconsidered if quota still requires reclamation.
        for _ in 0..2 {
            let mut refreshed = Vec::new();
            for (path, entry) in candidates {
                let aged = now.duration_since(entry.used).unwrap_or_default() > max_age;
                if !aged && inventory.bytes <= quota && free_shortfall == 0 {
                    continue;
                }
                let Some(_reservation) = app.transcode_cache.try_reserve(&path) else {
                    continue;
                };
                // Registration must reserve this exact destination before touching
                // it. Keep that reservation through unlink, after releasing the map.
                let waited = Instant::now();
                let jobs = crate::lock_recover(&app.remuxes);
                app.remux_metrics
                    .cache_registry_wait
                    .record(waited.elapsed());
                let protected = jobs.values().any(|job| job.dest == path);
                drop(jobs);
                if protected || requested.contains(&path) {
                    continue;
                }
                // Recency can have advanced since discovery; readers hold the
                // reservation while touching the stamp. Re-evaluate age/LRU safely.
                let current = read_entry(&path)?;
                if let Some(current) = current.filter(|current| current.used > entry.used) {
                    inventory.replace(path.clone(), Some(current));
                    refreshed.push((path, current));
                    continue;
                }
                let removed = match std::fs::remove_file(&path) {
                    Ok(()) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error),
                };
                let _ = std::fs::remove_file(rusty_dlna_transcode::cache_stamp_path(&path));
                inventory.replace(path, None);
                if removed {
                    app.remux_metrics
                        .cache_evicted_files
                        .fetch_add(1, Ordering::Relaxed);
                    app.remux_metrics.cache_evicted_bytes.fetch_add(
                        current.map_or(entry.bytes, |entry| entry.bytes),
                        Ordering::Relaxed,
                    );
                }
                if free_shortfall > 0 {
                    free_shortfall = minimum_free
                        .saturating_sub(crate::available_filesystem_bytes(&app.cache_dir)?);
                }
            }
            if inventory.bytes <= quota && free_shortfall == 0 {
                break;
            }
            refreshed.sort_unstable_by_key(|(_, entry)| entry.used);
            candidates = refreshed;
        }
    }
    app.remux_metrics
        .cache_bytes
        .store(inventory.bytes, Ordering::Relaxed);
    if inventory.bytes > quota || free_shortfall > 0 {
        Err(unsatisfied_limits_error())
    } else {
        Ok(inventory.bytes)
    }
}

pub(super) fn maintain_app_cache(
    app: &App,
    requested: &HashSet<PathBuf>,
    startup: bool,
) -> std::io::Result<u64> {
    let started = Instant::now();
    app.remux_metrics
        .cache_maintenance
        .fetch_add(1, Ordering::Relaxed);
    let result = maintain_inventory(app, requested, startup);
    app.remux_metrics
        .cache_maintenance_duration
        .record(started.elapsed());
    if result.is_err() {
        app.remux_metrics
            .cache_maintenance_failures
            .fetch_add(1, Ordering::Relaxed);
    }
    result
}

pub(super) fn enforce_active_cache_limits(app: &App) -> std::io::Result<u64> {
    maintain_app_cache(app, &HashSet::new(), false)
}

/// Caller holds shared maintenance gate; publication and deletion do not scan.
pub(super) fn refresh_artifacts(app: &App, paths: impl IntoIterator<Item = PathBuf>) {
    let mut inventory = crate::lock_recover(&app.transcode_cache.inventory);
    for path in paths {
        match read_entry(&path) {
            Ok(entry) => inventory.replace(path, entry),
            Err(_) => inventory.swept = None,
        }
    }
    app.remux_metrics
        .cache_bytes
        .store(inventory.bytes, Ordering::Relaxed);
}

/// Final admission while the shared image/video gate is held. The preceding
/// maintenance may have released that gate before publication acquired it.
pub(super) fn check_publication_limits(app: &App, job: &RemuxJob) -> std::io::Result<()> {
    refresh_artifacts(app, [job.part.clone(), job.dest.clone()]);
    let bytes = crate::lock_recover(&app.transcode_cache.inventory).bytes;
    let quota = app.cfg.transcode.cache_max_mb.saturating_mul(1024 * 1024);
    let minimum_free = app.cfg.cache_min_free_mb.saturating_mul(1024 * 1024);
    if bytes > quota
        || (minimum_free > 0 && crate::available_filesystem_bytes(&app.cache_dir)? < minimum_free)
    {
        return Err(unsatisfied_limits_error());
    }
    Ok(())
}

/// Called from blocking delivery/admission work with a live job or reservation.
/// Touch only stamp mtime: validated output identity and validators stay stable.
pub(super) fn touch_recency(dest: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(stamp) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(rusty_dlna_transcode::cache_stamp_path(dest))
    else {
        return;
    };
    let Ok(metadata) = stamp.metadata() else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    let now = SystemTime::now();
    if metadata
        .modified()
        .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() >= RECENCY_INTERVAL)
    {
        let _ = stamp.set_modified(now);
    }
}

#[cfg(test)]
type DiscoveryHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
fn discovery_test_hooks() -> &'static Mutex<HashMap<PathBuf, DiscoveryHook>> {
    static HOOKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, DiscoveryHook>>> =
        std::sync::OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn eviction_test_hooks() -> &'static Mutex<HashMap<PathBuf, DiscoveryHook>> {
    static HOOKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, DiscoveryHook>>> =
        std::sync::OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn run_test_hook(hooks: &Mutex<HashMap<PathBuf, DiscoveryHook>>, directory: &Path) {
    let hook = crate::lock_recover(hooks).remove(directory);
    if let Some(hook) = hook {
        hook();
    }
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
            .open(&stamp)
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
    fn test_app(directory: &Path) -> Arc<App> {
        let mut config = crate::Config {
            cache_dir: Some(directory.display().to_string()),
            rescan_secs: 0,
            cache_min_free_mb: 0,
            ..crate::Config::default()
        };
        config.transcode.enable = true;
        config.transcode.cache_max_mb = 1;
        Arc::new(App::from_config(config, 18200, 11900, directory))
    }

    fn test_job(directory: &Path, id: i64) -> Arc<RemuxJob> {
        use super::super::{hls, RemuxState, WebStartupObservations};
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
        let dest = directory.join(format!("{id}-hdr10-{id:064x}.mp4"));
        Arc::new(RemuxJob {
            detail_id: id,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: true,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(false),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            part: rusty_dlna_transcode::cache_part(&dest),
            dest,
            state: Mutex::new(RemuxState::Growing),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(1),
            ever_had_client: AtomicBool::new(true),
            client_epoch: AtomicU64::new(1),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        })
    }

    #[test]
    fn active_growth_and_deletion_are_accounted_without_rescanning_per_producer() {
        for producers in [1, 2, 8] {
            let directory = TempDir::new("incremental");
            let app = test_app(&directory);
            let jobs = (0..producers)
                .map(|id| test_job(&directory, id))
                .collect::<Vec<_>>();
            for job in &jobs {
                std::fs::write(&job.part, b"x").unwrap();
                crate::lock_recover(&app.remuxes).insert(job.detail_id.to_string(), job.clone());
            }
            assert_eq!(enforce_active_cache_limits(&app).unwrap(), producers as u64);
            for job in &jobs {
                std::fs::write(&job.part, [0; 100]).unwrap();
            }
            std::thread::scope(|scope| {
                for _ in 0..producers {
                    let app = &app;
                    scope.spawn(move || {
                        assert_eq!(
                            enforce_active_cache_limits(app).unwrap(),
                            producers as u64 * 100
                        )
                    });
                }
            });
            assert_eq!(app.remux_metrics.cache_scans.load(Ordering::Relaxed), 1);
            for job in &jobs {
                std::fs::remove_file(&job.part).unwrap();
            }
            assert_eq!(enforce_active_cache_limits(&app).unwrap(), 0);
            assert_eq!(app.remux_metrics.cache_scans.load(Ordering::Relaxed), 1);
            crate::lock_recover(&app.remuxes).clear();
        }
    }

    #[test]
    fn blocked_metadata_leaves_registration_status_and_cancellation_responsive_with_artwork() {
        let directory = TempDir::new("blocked-metadata");
        let app = test_app(&directory);
        let job = test_job(&directory, 1);
        std::fs::write(&job.part, [0; 100]).unwrap();
        crate::lock_recover(&app.remuxes).insert("active".into(), job.clone());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        crate::lock_recover(discovery_test_hooks()).insert(
            directory.0.clone(),
            Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }),
        );
        let worker_app = app.clone();
        let worker = std::thread::spawn(move || enforce_active_cache_limits(&worker_app));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            app.remuxes.try_lock().is_ok(),
            "blocked metadata must not own the registry"
        );
        assert_eq!(super::super::runtime_status(&app).active, 1);
        let mut spec = super::super::tests::job_spec(&directory, "active", Vec::new());
        spec.dest = job.dest.clone();
        spec.job_key = "active".into();
        let attached = super::super::attach_for_client(app.clone(), spec).unwrap();
        assert!(Arc::ptr_eq(&attached, &job));
        job.cancel();
        assert!(job.cancelled.load(Ordering::Acquire));
        let images = directory.join("derived-images");
        std::fs::create_dir_all(&images).unwrap();
        let image_app = app.clone();
        let image_worker = std::thread::spawn(move || {
            let key = "a".repeat(64);
            let _active = image_app
                .derived_images
                .activate(&key, &image_app.cache_maintenance);
            let dest = images.join(format!("{key}.jpg"));
            std::fs::write(&dest, [0; 200]).unwrap();
            let report = image_app
                .derived_images
                .maintain(&image_app.cache_maintenance, &images, 1024, 30, 0)
                .unwrap();
            assert!(report.limits_satisfied);
            assert!(dest.exists());
        });
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), 100);
        image_worker.join().unwrap();
        assert!(job.part.exists());
        crate::lock_recover(&app.remuxes).clear();
    }

    #[test]
    fn eviction_rechecks_live_registry_after_candidate_snapshot() {
        let directory = TempDir::new("stale-snapshot");
        let app = test_app(&directory);
        let job = test_job(&directory, 1);
        let victim = test_job(&directory, 2);
        std::fs::write(&job.dest, vec![0; 700_000]).unwrap();
        std::fs::write(&victim.dest, vec![0; 700_000]).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        crate::lock_recover(eviction_test_hooks()).insert(
            directory.0.clone(),
            Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }),
        );
        let worker_app = app.clone();
        let worker = std::thread::spawn(move || enforce_active_cache_limits(&worker_app));
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        {
            let _reservation = app.transcode_cache.reserve(&job.dest).unwrap();
            crate::lock_recover(&app.remuxes).insert("new-reader".into(), job.clone());
        }
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), 700_000);
        assert!(job.dest.exists());
        assert!(!victim.dest.exists());
        crate::lock_recover(&app.remuxes).clear();
    }

    #[test]
    fn pending_admission_reserves_candidate_and_external_deletion_converges_at_sweep() {
        let directory = TempDir::new("reserved-candidate");
        let app = test_app(&directory);
        let job = test_job(&directory, 1);
        let victim = test_job(&directory, 2);
        std::fs::write(&job.dest, vec![0; 700_000]).unwrap();
        std::fs::write(&victim.dest, vec![0; 700_000]).unwrap();
        let reservation = app.transcode_cache.reserve(&job.dest).unwrap();
        assert_eq!(enforce_active_cache_limits(&app).unwrap(), 700_000);
        assert!(job.dest.exists());
        assert!(!victim.dest.exists());
        drop(reservation);
        std::fs::remove_file(&job.dest).unwrap();
        crate::lock_recover(&app.transcode_cache.inventory).swept =
            Some(Instant::now() - SWEEP_INTERVAL);
        assert_eq!(enforce_active_cache_limits(&app).unwrap(), 0);
        assert_eq!(app.remux_metrics.cache_scans.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn throttled_recency_does_not_change_output_or_stamp_contents() {
        let directory = TempDir::new("recency");
        let output = directory.join(format!("1-hdr10-{}.mp4", "a".repeat(64)));
        std::fs::write(&output, [0; 128]).unwrap();
        let key = "a".repeat(64);
        rusty_dlna_transcode::write_cache_stamp_for_key(&output, &key).unwrap();
        let stamp = rusty_dlna_transcode::cache_stamp_path(&output);
        let old = SystemTime::now() - RECENCY_INTERVAL * 2;
        std::fs::File::open(&stamp)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let original_stamp = std::fs::read(&stamp).unwrap();
        let original_output = std::fs::metadata(&output).unwrap().modified().unwrap();
        touch_recency(&output);
        let used = std::fs::metadata(&stamp).unwrap().modified().unwrap();
        assert!(used > old);
        touch_recency(&output);
        assert_eq!(std::fs::metadata(&stamp).unwrap().modified().unwrap(), used);
        assert_eq!(std::fs::read(&stamp).unwrap(), original_stamp);
        assert_eq!(
            std::fs::metadata(&output).unwrap().modified().unwrap(),
            original_output
        );
        assert!(rusty_dlna_transcode::cache_is_fresh_for_key(&output, &key));
    }

    #[test]
    fn recently_used_unprotected_output_can_still_satisfy_quota_pressure() {
        let directory = TempDir::new("recency-pressure");
        let app = test_app(&directory);
        let output = directory.join(format!("42-hdr10-{}.mp4", "a".repeat(64)));
        std::fs::write(&output, vec![0; 700_000]).unwrap();
        rusty_dlna_transcode::write_cache_stamp_for_key(&output, &"a".repeat(64)).unwrap();
        let stamp = rusty_dlna_transcode::cache_stamp_path(&output);
        std::fs::File::open(stamp)
            .unwrap()
            .set_modified(SystemTime::now() - RECENCY_INTERVAL * 2)
            .unwrap();
        assert_eq!(enforce_active_cache_limits(&app).unwrap(), 700_000);
        touch_recency(&output);
        let job = test_job(&directory, 1);
        std::fs::write(&job.part, vec![0; 700_000]).unwrap();
        crate::lock_recover(&app.remuxes).insert("active".into(), job.clone());
        assert_eq!(enforce_active_cache_limits(&app).unwrap(), 700_000);
        assert!(!output.exists());
        assert!(job.part.exists());
        crate::lock_recover(&app.remuxes).clear();
    }

    #[tokio::test]
    async fn raw_hls_and_mse_resource_reads_refresh_the_same_throttled_recency() {
        use tokio::io::AsyncReadExt;
        let directory = TempDir::new("delivery-recency");
        let app = test_app(&directory);
        let key = "d".repeat(64);
        let output = directory.join(format!("42-hdr10-{key}.mp4"));
        std::fs::write(&output, [0; 128]).unwrap();
        rusty_dlna_transcode::write_cache_stamp_for_key(&output, &key).unwrap();
        let stamp = rusty_dlna_transcode::cache_stamp_path(&output);
        let stamp_contents = std::fs::read(&stamp).unwrap();
        let modified = std::fs::metadata(&output).unwrap().modified().unwrap();
        let mut spec = super::super::tests::job_spec(&directory, &key, Vec::new());
        spec.dest = output.clone();
        spec.job_key = "web:42:recency".into();
        for delivery in ["", "hls_init", "mse_init"] {
            let old = SystemTime::now() - RECENCY_INTERVAL * 2;
            std::fs::File::open(&stamp)
                .unwrap()
                .set_modified(old)
                .unwrap();
            let query = if delivery.is_empty() {
                String::new()
            } else {
                format!("?delivery={delivery}&hls_offset=0&hls_length=8")
            };
            let request = rusty_dlna_http::HttpRequest::parse_headers(&format!(
                "GET /web/media/42.mp4{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
            ))
            .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server_app = app.clone();
            let served_spec = spec.clone();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                super::super::serve_remux(&server_app, &mut socket, &request, served_spec)
                    .await
                    .unwrap();
            });
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            server.await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK"), "{delivery}");
            assert!(
                std::fs::metadata(&stamp).unwrap().modified().unwrap() > old,
                "{delivery}"
            );
            assert_eq!(std::fs::read(&stamp).unwrap(), stamp_contents);
            assert_eq!(
                std::fs::metadata(&output).unwrap().modified().unwrap(),
                modified
            );
            assert!(rusty_dlna_transcode::cache_is_fresh_for_key(&output, &key));
        }
        super::super::shutdown_ephemeral_cleanups(&app);
        crate::lock_recover(&app.remuxes).clear();
    }

    /// Filesystem/registry microbenchmark, not a playback latency claim.
    /// Run with --ignored --nocapture; generated files are always temporary.
    #[test]
    #[ignore = "100/1k/10k/100k cache benchmark; run explicitly"]
    fn cache_scale_benchmark() {
        for entries in [100, 1_000, 10_000, 100_000] {
            let directory = TempDir::new("scale-benchmark");
            let mut config = crate::Config {
                cache_dir: Some(directory.display().to_string()),
                rescan_secs: 0,
                cache_min_free_mb: 0,
                ..crate::Config::default()
            };
            config.transcode.enable = true;
            let app = Arc::new(App::from_config(config, 18200, 11900, &directory));
            for id in 0..entries {
                let output = directory.join(format!("{id}-hdr10-{id:064x}.mp4"));
                std::fs::write(output, b"x").unwrap();
            }
            for producers in [1, 2, 8] {
                let mut samples = Vec::new();
                let scans_before = app.remux_metrics.cache_maintenance.load(Ordering::Relaxed);
                let full_scans_before = app.remux_metrics.cache_scans.load(Ordering::Relaxed);
                for _ in 0..9 {
                    let barrier = std::sync::Barrier::new(producers);
                    let elapsed = std::thread::scope(|scope| {
                        let handles = (0..producers)
                            .map(|_| {
                                let app = &app;
                                let barrier = &barrier;
                                scope.spawn(move || {
                                    barrier.wait();
                                    let start = std::time::Instant::now();
                                    enforce_active_cache_limits(app).unwrap();
                                    start.elapsed().as_secs_f64() * 1000.0
                                })
                            })
                            .collect::<Vec<_>>();
                        handles
                            .into_iter()
                            .map(|handle| handle.join().unwrap())
                            .collect::<Vec<_>>()
                    });
                    samples.extend(elapsed);
                }
                samples.sort_by(f64::total_cmp);
                let mean = samples.iter().sum::<f64>() / samples.len() as f64;
                let deviation = (samples
                    .iter()
                    .map(|sample| (sample - mean).powi(2))
                    .sum::<f64>()
                    / samples.len() as f64)
                    .sqrt();
                let percentile =
                    |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
                println!("cache_benchmark entries={entries} producers={producers} n={} p50_ms={:.3} p95_ms={:.3} max_ms={:.3} mean_ms={mean:.3} stddev_ms={deviation:.3} maintenance_calls={} full_scans={} p99_reliable=false conditions=warm-generated-one-byte-unstamped-files", samples.len(), percentile(0.5), percentile(0.95), samples.last().unwrap(), app.remux_metrics.cache_maintenance.load(Ordering::Relaxed) - scans_before, app.remux_metrics.cache_scans.load(Ordering::Relaxed) - full_scans_before);
            }
        }
    }
}
