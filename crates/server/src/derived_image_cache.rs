//! Bounded, concurrency-safe ownership for on-demand JPEG cache files.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::{available_filesystem_bytes, lock_recover};

const DERIVED_IMAGE_STRIPES: usize = 64;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
const MAX_INVENTORY_ENTRIES: usize = 65_536;

#[derive(Clone, Copy)]
struct ImageEntry {
    bytes: u64,
    used: SystemTime,
}

#[derive(Default)]
struct Inventory {
    directory: PathBuf,
    swept: Option<Instant>,
    entries: HashMap<PathBuf, ImageEntry>,
    recency: BTreeSet<(SystemTime, PathBuf)>,
    temporary: HashSet<PathBuf>,
    bytes: u64,
}

impl Inventory {
    fn replace(&mut self, path: PathBuf, entry: Option<ImageEntry>) -> std::io::Result<()> {
        if let Some(old) = self.entries.remove(&path) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
            self.recency.remove(&(old.used, path.clone()));
        }
        if let Some(entry) = entry {
            if self.entries.len() + self.temporary.len() >= MAX_INVENTORY_ENTRIES {
                return Err(std::io::Error::other(
                    "derived-image inventory capacity exceeded",
                ));
            }
            self.bytes = self.bytes.saturating_add(entry.bytes);
            self.recency.insert((entry.used, path.clone()));
            self.entries.insert(path, entry);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DerivedCacheMaintenance {
    pub(crate) bytes: u64,
    pub(crate) quota_satisfied: bool,
    pub(crate) limits_satisfied: bool,
}

pub(crate) struct DerivedImageCache {
    stripes: Vec<Mutex<()>>,
    active: Mutex<HashMap<String, usize>>,
    inventory: Mutex<Inventory>,
    #[cfg(test)]
    pub(crate) work: ImageCacheWork,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct ImageCacheWork {
    pub(crate) scans: std::sync::atomic::AtomicU64,
    pub(crate) stats: std::sync::atomic::AtomicU64,
    pub(crate) lock_wait_nanos: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl ImageCacheWork {
    pub(crate) fn snapshot(&self) -> [u64; 3] {
        use std::sync::atomic::Ordering::Relaxed;
        [
            self.scans.load(Relaxed),
            self.stats.load(Relaxed),
            self.lock_wait_nanos.load(Relaxed),
        ]
    }
}

impl DerivedImageCache {
    pub(crate) fn new() -> Self {
        Self {
            stripes: (0..DERIVED_IMAGE_STRIPES).map(|_| Mutex::new(())).collect(),
            active: Mutex::new(HashMap::new()),
            inventory: Mutex::new(Inventory::default()),
            #[cfg(test)]
            work: ImageCacheWork::default(),
        }
    }

    /// Serialize one cache key and register all of its publication artifacts.
    ///
    /// Registration is synchronized with maintenance so a scan that started
    /// before this request either finishes before publication starts or sees
    /// the key as protected.
    pub(crate) fn activate<'a>(&'a self, key: &str, maintenance: &Mutex<()>) -> ActiveImage<'a> {
        let stripe = derived_image_lock_index(key, self.stripes.len());
        let stripe_guard = lock_recover(&self.stripes[stripe]);
        let maintenance_guard = lock_recover(maintenance);
        *lock_recover(&self.active)
            .entry(key.to_owned())
            .or_default() += 1;
        drop(maintenance_guard);
        ActiveImage {
            cache: self,
            key: key.to_owned(),
            _stripe: stripe_guard,
        }
    }

    pub(crate) fn maintain(
        &self,
        maintenance: &Mutex<()>,
        directory: &Path,
        quota_bytes: u64,
        max_age_days: u32,
        minimum_free_bytes: u64,
    ) -> std::io::Result<DerivedCacheMaintenance> {
        #[cfg(test)]
        let waited = std::time::Instant::now();
        let _maintenance = lock_recover(maintenance);
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::Relaxed;
            self.work.lock_wait_nanos.fetch_add(
                waited.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                Relaxed,
            );
        }
        let protected = lock_recover(&self.active)
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let mut inventory = lock_recover(&self.inventory);
        #[cfg(test)]
        if inventory.directory != directory
            || inventory
                .swept
                .is_none_or(|at| at.elapsed() >= RECONCILE_INTERVAL)
        {
            self.work
                .scans
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        maintain_derived_image_cache_with(
            &mut inventory,
            directory,
            quota_bytes,
            max_age_days,
            minimum_free_bytes,
            &protected,
            (available_filesystem_bytes, |path: &Path| {
                #[cfg(test)]
                self.work
                    .stats
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::fs::symlink_metadata(path)
            }),
        )
    }

    pub(crate) fn maintain_startup(
        &self,
        directory: &Path,
        quota_bytes: u64,
        max_age_days: u32,
        minimum_free_bytes: u64,
    ) -> std::io::Result<DerivedCacheMaintenance> {
        maintain_derived_image_cache_with(
            &mut lock_recover(&self.inventory),
            directory,
            quota_bytes,
            max_age_days,
            minimum_free_bytes,
            &HashSet::new(),
            (available_filesystem_bytes, |path: &Path| {
                std::fs::symlink_metadata(path)
            }),
        )
    }

    /// Keep deletion and accounting ordered with video/image maintenance,
    /// including rejection after a post-publication filesystem error.
    pub(crate) fn reject(&self, maintenance: &Mutex<()>, path: &Path) -> std::io::Result<()> {
        let _maintenance = lock_recover(maintenance);
        remove_image(path)?;
        lock_recover(&self.inventory).replace(path.to_owned(), None)
    }
}

impl Default for DerivedImageCache {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct ActiveImage<'a> {
    cache: &'a DerivedImageCache,
    key: String,
    _stripe: MutexGuard<'a, ()>,
}

impl Drop for ActiveImage<'_> {
    fn drop(&mut self) {
        let mut active = lock_recover(&self.cache.active);
        let remove = match active.get_mut(&self.key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            active.remove(&self.key);
        }
    }
}

pub(crate) fn derived_image_key(
    identity: &str,
    width: u32,
    height: u32,
    quality: u8,
    rotation: i64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rustydlna-derived-image-v3\0");
    hasher.update(identity.as_bytes());
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    hasher.update([quality]);
    hasher.update(rotation.to_le_bytes());
    let digest = hasher.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = String::with_capacity(digest.len() * 2);
    for byte in digest {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

fn derived_image_lock_index(key: &str, count: usize) -> usize {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % count.max(1)
}

fn artifact_is_protected(name: &str, protected: &HashSet<String>) -> bool {
    protected.iter().any(|key| {
        name == format!("{key}.jpg")
            || (name.starts_with(&format!(".{key}.jpg.")) && name.ends_with(".tmp.jpg"))
    })
}

fn is_atomic_temporary(name: &str) -> bool {
    name.starts_with('.') && name.contains(".jpg.") && name.ends_with(".tmp.jpg")
}

fn path_is_protected(path: &Path, protected: &HashSet<String>) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| artifact_is_protected(name, protected))
}

fn remove_image(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

// Bound index memory by reclaiming the oldest unprotected files at capacity.
// This happens before insertion, including discovery, so a large stale cache
// can be repaired at startup instead of preventing the daemon from starting.
fn make_inventory_room<M>(
    inventory: &mut Inventory,
    protected: &HashSet<String>,
    metadata: &mut M,
) -> std::io::Result<()>
where
    M: FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
{
    if inventory.entries.len() + inventory.temporary.len() < MAX_INVENTORY_ENTRIES {
        return Ok(());
    }
    let mut deferred = Vec::new();
    let result = (|| {
        for _ in 0..inventory.entries.len().saturating_mul(2) {
            let Some((used, path)) = inventory.recency.pop_first() else {
                break;
            };
            // Restore the popped record on every failed metadata/unlink path.
            deferred.push((used, path.clone()));
            if path_is_protected(&path, protected) {
                continue;
            }
            let current = match metadata(&path) {
                Ok(value) if value.is_file() => Some(ImageEntry {
                    bytes: value.len(),
                    used: value.modified()?,
                }),
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            if let Some(current) = current {
                if current.used > used {
                    inventory.replace(path, Some(current))?;
                    deferred.pop();
                    continue;
                }
                if remove_image(&path).is_err() {
                    continue;
                }
            }
            inventory.replace(path, None)?;
            deferred.pop();
            return Ok(());
        }
        Err(std::io::Error::other(
            "derived-image inventory has no reclaimable capacity",
        ))
    })();
    inventory.recency.extend(deferred);
    result
}

fn maintain_derived_image_cache_with<F, M>(
    inventory: &mut Inventory,
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
    protected: &HashSet<String>,
    io: (F, M),
) -> std::io::Result<DerivedCacheMaintenance>
where
    F: FnMut(&Path) -> std::io::Result<u64>,
    M: FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
{
    let (mut available_bytes, mut entry_metadata) = io;
    let read_entry = |path: &Path, metadata: &mut M| -> std::io::Result<Option<ImageEntry>> {
        match metadata(path) {
            Ok(value) if value.is_file() => Ok(Some(ImageEntry {
                bytes: value.len(),
                used: value.modified()?,
            })),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    };
    let is_protected = |path: &Path| path_is_protected(path, protected);
    let remove = remove_image;
    if inventory.directory != directory
        || inventory
            .swept
            .is_none_or(|at| at.elapsed() >= RECONCILE_INTERVAL)
    {
        std::fs::create_dir_all(directory)?;
        // Publish only a complete reconciliation. A metadata error cannot turn
        // partial discovery into an optimistic undercount on the next request.
        let mut discovered = Inventory {
            directory: directory.to_owned(),
            ..Inventory::default()
        };
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            let Some(value) = read_entry(&path, &mut entry_metadata)? else {
                continue;
            };
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_atomic_temporary)
            {
                if is_protected(&path) {
                    make_inventory_room(&mut discovered, protected, &mut entry_metadata)?;
                    discovered.temporary.insert(path);
                } else {
                    remove(&path)?;
                }
            } else if path.extension().and_then(|value| value.to_str()) == Some("jpg") {
                if discovered.entries.len() + discovered.temporary.len() >= MAX_INVENTORY_ENTRIES
                    && !is_protected(&path)
                    && discovered
                        .recency
                        .iter()
                        .find(|(_, path)| !is_protected(path))
                        .is_none_or(|(used, _)| value.used < *used)
                {
                    remove(&path)?;
                    continue;
                }
                make_inventory_room(&mut discovered, protected, &mut entry_metadata)?;
                discovered.replace(path, Some(value))?;
            }
        }
        discovered.swept = Some(Instant::now());
        *inventory = discovered;
    }
    // Only active final files can be published/removed by server writers
    // between reconciliations. There are at most 64 stripe owners. Temporary
    // files remain unexposed and protected until helper cleanup/guard release.
    for key in protected {
        let path = directory.join(format!("{key}.jpg"));
        let entry = read_entry(&path, &mut entry_metadata)?;
        if entry.is_some() && !inventory.entries.contains_key(&path) {
            make_inventory_room(inventory, protected, &mut entry_metadata)?;
        }
        inventory.replace(path, entry)?;
    }
    let abandoned = inventory
        .temporary
        .iter()
        .filter(|path| !is_protected(path))
        .cloned()
        .collect::<Vec<_>>();
    for path in abandoned {
        remove(&path)?;
        inventory.temporary.remove(&path);
    }
    let now = SystemTime::now();
    let max_age = Duration::from_secs(u64::from(max_age_days).saturating_mul(86_400));
    let mut free_shortfall = if minimum_free_bytes == 0 {
        0
    } else {
        minimum_free_bytes.saturating_sub(available_bytes(directory)?)
    };
    // On ordinary misses this checks the oldest timestamp in O(1) and never
    // walks the inventory. Pressure/age reclamation alone enumerates candidates.
    for pass in 0..2 {
        let pressure = inventory.bytes > quota_bytes || free_shortfall > 0;
        let candidates = inventory
            .recency
            .iter()
            .take_while(|(used, _)| {
                pressure || now.duration_since(*used).unwrap_or_default() > max_age
            })
            .filter(|(_, path)| !is_protected(path))
            .cloned()
            .collect::<Vec<_>>();
        for (used, path) in candidates {
            if inventory.bytes <= quota_bytes
                && free_shortfall == 0
                && now.duration_since(used).unwrap_or_default() <= max_age
            {
                break;
            }
            let current = read_entry(&path, &mut entry_metadata)?;
            inventory.replace(path.clone(), current)?;
            let Some(current) = current else {
                if free_shortfall > 0 {
                    free_shortfall = minimum_free_bytes.saturating_sub(available_bytes(directory)?);
                }
                continue;
            };
            // Warm readers touch the file while holding their stripe. Refresh
            // recency before unlink; defer newly used files behind older ones.
            if pass == 0 && current.used > used {
                continue;
            }
            if inventory.bytes <= quota_bytes
                && free_shortfall == 0
                && now.duration_since(current.used).unwrap_or_default() <= max_age
            {
                continue;
            }
            if remove(&path).is_ok() {
                inventory.replace(path, None)?;
                if free_shortfall > 0 {
                    free_shortfall = minimum_free_bytes.saturating_sub(available_bytes(directory)?);
                }
            }
        }
    }
    let quota_satisfied = inventory.bytes <= quota_bytes;
    Ok(DerivedCacheMaintenance {
        bytes: inventory.bytes,
        quota_satisfied,
        limits_satisfied: quota_satisfied && free_shortfall == 0,
    })
}

#[cfg(test)]
pub(crate) fn prune_derived_image_cache(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
) -> std::io::Result<()> {
    let report = maintain_derived_image_cache_with(
        &mut Inventory::default(),
        directory,
        quota_bytes,
        max_age_days,
        minimum_free_bytes,
        &HashSet::new(),
        (available_filesystem_bytes, |path: &Path| {
            std::fs::symlink_metadata(path)
        }),
    )?;
    if report.limits_satisfied {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "derived-image cache cannot satisfy quota/free-space requirement",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rusty-dlna-derived-cache-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn disk_bytes(directory: &Path) -> u64 {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum()
    }

    #[test]
    fn bounded_inventory_reclaims_capacity_at_startup_and_publication() {
        let directory = test_directory("capacity");
        for index in 0..=MAX_INVENTORY_ENTRIES {
            std::fs::write(directory.join(format!("{index:064x}.jpg")), [1]).unwrap();
        }
        let cache = DerivedImageCache::new();
        let maintenance = Mutex::new(());
        let report = cache.maintain_startup(&directory, u64::MAX, 30, 0).unwrap();
        assert!(report.limits_satisfied);
        assert_eq!(report.bytes, MAX_INVENTORY_ENTRIES as u64);
        assert_eq!(disk_bytes(&directory), MAX_INVENTORY_ENTRIES as u64);
        let key = "f".repeat(64);
        let _active = cache.activate(&key, &maintenance);
        let path = directory.join(format!("{key}.jpg"));
        std::fs::write(&path, [2]).unwrap();
        let report = cache
            .maintain(&maintenance, &directory, u64::MAX, 30, 0)
            .unwrap();
        assert!(report.limits_satisfied);
        assert_eq!(std::fs::read(&path).unwrap(), [2]);
        assert_eq!(disk_bytes(&directory), MAX_INVENTORY_ENTRIES as u64);
        let inventory = lock_recover(&cache.inventory);
        assert_eq!(inventory.entries.len(), MAX_INVENTORY_ENTRIES);
        assert_eq!(inventory.recency.len(), MAX_INVENTORY_ENTRIES);
        drop(inventory);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejection_after_free_space_error_forgets_only_the_unlinked_output() {
        let directory = test_directory("rejected-accounting");
        let cache = DerivedImageCache::new();
        let maintenance = Mutex::new(());
        let old = directory.join("old.jpg");
        std::fs::write(&old, [0; 100]).unwrap();
        cache
            .maintain(&maintenance, &directory, 100, 30, 0)
            .unwrap();
        let key = "f".repeat(64);
        let active = cache.activate(&key, &maintenance);
        let path = directory.join(format!("{key}.jpg"));
        std::fs::write(&path, [0; 50]).unwrap();
        let error = maintain_derived_image_cache_with(
            &mut lock_recover(&cache.inventory),
            &directory,
            100,
            30,
            1,
            &HashSet::from([key]),
            (
                |_: &Path| Err(std::io::Error::other("statvfs failed")),
                |path: &Path| std::fs::symlink_metadata(path),
            ),
        )
        .unwrap_err();
        assert!(error.to_string().contains("statvfs failed"));
        cache.reject(&maintenance, &path).unwrap();
        drop(active);
        let report = cache
            .maintain(&maintenance, &directory, 100, 30, 0)
            .unwrap();
        assert!(report.limits_satisfied);
        assert_eq!(report.bytes, disk_bytes(&directory));
        assert_eq!(report.bytes, 100);
        assert!(
            old.exists(),
            "phantom output bytes must not evict valid older files"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn publication_accounting_and_eviction_match_files_without_repeated_scans() {
        let directory = test_directory("incremental");
        let cache = DerivedImageCache::new();
        let maintenance = Mutex::new(());
        for index in 0..100 {
            std::fs::write(directory.join(format!("{index:064x}.jpg")), [0; 100]).unwrap();
        }
        cache
            .maintain(&maintenance, &directory, 20_000, 30, 0)
            .unwrap();
        let before = cache.work.snapshot();
        let key = "a".repeat(64);
        let active = cache.activate(&key, &maintenance);
        cache
            .maintain(&maintenance, &directory, 20_000, 30, 0)
            .unwrap();
        let output = directory.join(format!("{key}.jpg"));
        std::fs::write(&output, [0; 200]).unwrap();
        let report = cache
            .maintain(&maintenance, &directory, 10_000, 30, 0)
            .unwrap();
        assert!(report.limits_satisfied);
        assert_eq!(report.bytes, 10_000);
        assert_eq!(disk_bytes(&directory), 10_000);
        assert!(output.is_file(), "the new active output is protected");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 99);
        assert_eq!(
            cache.work.snapshot()[0],
            before[0],
            "cold misses must not rescan the directory"
        );
        assert!(
            cache.work.snapshot()[1] - before[1] <= 6,
            "only active files and eviction candidates need metadata"
        );
        // Rejected oversized final files stay accounted while protected.
        std::fs::write(&output, [0; 20_000]).unwrap();
        let report = cache
            .maintain(&maintenance, &directory, 10_000, 30, 0)
            .unwrap();
        assert!(!report.limits_satisfied);
        assert_eq!(report.bytes, disk_bytes(&directory));
        assert_eq!(report.bytes, 20_000);
        std::fs::remove_file(&output).unwrap();
        assert_eq!(
            cache
                .maintain(&maintenance, &directory, 10_000, 30, 0)
                .unwrap()
                .bytes,
            0
        );
        drop(active);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn external_changes_reconcile_and_recently_read_files_are_not_old_victims() {
        let directory = test_directory("external");
        let cache = DerivedImageCache::new();
        let maintenance = Mutex::new(());
        let old = directory.join("old.jpg");
        let next = directory.join("next.jpg");
        for (path, seconds) in [(&old, 100), (&next, 200)] {
            std::fs::write(path, [0; 100]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap();
        }
        cache
            .maintain(&maintenance, &directory, 200, 36_500, 0)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
        let report = cache
            .maintain(&maintenance, &directory, 100, 36_500, 0)
            .unwrap();
        assert_eq!(report.bytes, 100);
        assert!(old.is_file());
        assert!(
            !next.exists(),
            "a recently read candidate must move behind an older file"
        );
        std::fs::remove_file(&old).unwrap();
        std::fs::write(&next, [0; 50]).unwrap();
        let leftover = directory.join(".abandoned.jpg.1.tmp.jpg");
        std::fs::write(&leftover, [0; 20]).unwrap();
        lock_recover(&cache.inventory).swept = Some(Instant::now() - RECONCILE_INTERVAL);
        let report = cache
            .maintain(&maintenance, &directory, 100, 30, 0)
            .unwrap();
        assert_eq!(report.bytes, disk_bytes(&directory));
        assert_eq!(report.bytes, 50);
        assert!(!leftover.exists());
        assert_eq!(cache.work.snapshot()[0], 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn active_final_and_atomic_temporary_are_protected_until_guard_drop() {
        let directory = test_directory("active");
        let cache = DerivedImageCache::new();
        let maintenance = Mutex::new(());
        let key = "a".repeat(64);
        let final_path = directory.join(format!("{key}.jpg"));
        let temporary = directory.join(format!(".{key}.jpg.7-9.tmp.jpg"));
        std::fs::write(&final_path, vec![1u8; 64]).unwrap();
        std::fs::write(&temporary, vec![2u8; 64]).unwrap();

        let active = cache.activate(&key, &maintenance);
        let report = cache
            .maintain(&maintenance, &directory, 0, 36_500, 0)
            .unwrap();
        assert!(
            final_path.exists(),
            "active final output must survive quota pressure"
        );
        assert!(
            temporary.exists(),
            "active temporary output must survive quota pressure"
        );
        assert!(!report.limits_satisfied);

        drop(active);
        let report = cache
            .maintain(&maintenance, &directory, 0, 36_500, 0)
            .unwrap();
        assert!(report.limits_satisfied);
        assert!(!final_path.exists());
        assert!(!temporary.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn minimum_free_space_is_rechecked_after_each_unlink() {
        let directory = test_directory("free-space");
        let first = directory.join(format!("{}.jpg", "a".repeat(64)));
        let second = directory.join(format!("{}.jpg", "b".repeat(64)));
        std::fs::write(&first, vec![0u8; 100]).unwrap();
        std::fs::write(&second, vec![0u8; 100]).unwrap();
        let mut readings = VecDeque::from([0u64, 0, 100]);

        let report = maintain_derived_image_cache_with(
            &mut Inventory::default(),
            &directory,
            u64::MAX,
            36_500,
            100,
            &HashSet::new(),
            (
                |_: &Path| {
                    readings
                        .pop_front()
                        .ok_or_else(|| std::io::Error::other("unexpected free-space read"))
                },
                |path: &Path| std::fs::symlink_metadata(path),
            ),
        )
        .unwrap();

        assert!(report.limits_satisfied);
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(readings.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn free_space_errors_fail_closed() {
        let directory = test_directory("stat-error");
        let image = directory.join(format!("{}.jpg", "c".repeat(64)));
        std::fs::write(&image, vec![0u8; 100]).unwrap();
        let error = maintain_derived_image_cache_with(
            &mut Inventory::default(),
            &directory,
            u64::MAX,
            36_500,
            100,
            &HashSet::new(),
            (
                |_: &Path| Err(std::io::Error::other("statvfs failed")),
                |path: &Path| std::fs::symlink_metadata(path),
            ),
        )
        .unwrap_err();
        assert!(error.to_string().contains("statvfs failed"));
        assert!(image.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn metadata_errors_fail_closed() {
        let directory = test_directory("metadata-error");
        let image = directory.join(format!("{}.jpg", "d".repeat(64)));
        std::fs::write(&image, vec![0u8; 100]).unwrap();
        let error = maintain_derived_image_cache_with(
            &mut Inventory::default(),
            &directory,
            u64::MAX,
            36_500,
            0,
            &HashSet::new(),
            (
                |_: &Path| Ok(u64::MAX),
                |_: &Path| Err(std::io::Error::other("metadata failed")),
            ),
        )
        .unwrap_err();
        assert!(error.to_string().contains("metadata failed"));
        assert!(image.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_jpeg_names_remain_accounted_and_evictable() {
        use std::os::unix::ffi::OsStringExt;

        let directory = test_directory("non-utf8");
        let image = directory.join(std::ffi::OsString::from_vec(b"cache-\xff.jpg".to_vec()));
        std::fs::write(&image, vec![0u8; 100]).unwrap();
        let report = maintain_derived_image_cache_with(
            &mut Inventory::default(),
            &directory,
            0,
            36_500,
            0,
            &HashSet::new(),
            (
                |_: &Path| Ok(u64::MAX),
                |path: &Path| std::fs::symlink_metadata(path),
            ),
        )
        .unwrap();
        assert!(report.limits_satisfied);
        assert!(!image.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_maintenance_is_serialized() {
        let directory = test_directory("concurrent");
        for index in 0..8 {
            std::fs::write(
                directory.join(format!("{:064x}.jpg", index)),
                vec![index as u8; 100],
            )
            .unwrap();
        }
        let cache = Arc::new(DerivedImageCache::new());
        let maintenance = Arc::new(Mutex::new(()));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let maintenance = Arc::clone(&maintenance);
            let directory = directory.clone();
            workers.push(std::thread::spawn(move || {
                cache.maintain(&maintenance, &directory, 100, 36_500, 0)
            }));
        }
        for worker in workers {
            assert!(worker.join().unwrap().unwrap().limits_satisfied);
        }
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
