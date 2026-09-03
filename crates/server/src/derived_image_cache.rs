//! Bounded, concurrency-safe ownership for on-demand JPEG cache files.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::{available_filesystem_bytes, lock_recover};

const DERIVED_IMAGE_STRIPES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DerivedCacheMaintenance {
    pub(crate) bytes: u64,
    pub(crate) quota_satisfied: bool,
    pub(crate) limits_satisfied: bool,
}

pub(crate) struct DerivedImageCache {
    stripes: Vec<Mutex<()>>,
    active: Mutex<HashMap<String, usize>>,
}

impl DerivedImageCache {
    pub(crate) fn new() -> Self {
        Self {
            stripes: (0..DERIVED_IMAGE_STRIPES).map(|_| Mutex::new(())).collect(),
            active: Mutex::new(HashMap::new()),
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
        let _maintenance = lock_recover(maintenance);
        let protected = lock_recover(&self.active)
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        maintain_derived_image_cache_with(
            directory,
            quota_bytes,
            max_age_days,
            minimum_free_bytes,
            &protected,
            available_filesystem_bytes,
            |entry| entry.metadata(),
        )
    }

    pub(crate) fn maintain_startup(
        directory: &Path,
        quota_bytes: u64,
        max_age_days: u32,
        minimum_free_bytes: u64,
    ) -> std::io::Result<DerivedCacheMaintenance> {
        maintain_derived_image_cache_with(
            directory,
            quota_bytes,
            max_age_days,
            minimum_free_bytes,
            &HashSet::new(),
            available_filesystem_bytes,
            |entry| entry.metadata(),
        )
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

fn maintain_derived_image_cache_with<F, M>(
    directory: &Path,
    quota_bytes: u64,
    max_age_days: u32,
    minimum_free_bytes: u64,
    protected: &HashSet<String>,
    mut available_bytes: F,
    mut entry_metadata: M,
) -> std::io::Result<DerivedCacheMaintenance>
where
    F: FnMut(&Path) -> std::io::Result<u64>,
    M: FnMut(&std::fs::DirEntry) -> std::io::Result<std::fs::Metadata>,
{
    std::fs::create_dir_all(directory)?;
    let now = SystemTime::now();
    let max_age = Duration::from_secs(u64::from(max_age_days).saturating_mul(86_400));
    let mut entries = Vec::new();
    let mut total = 0u64;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry_metadata(&entry) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let name = path.file_name().and_then(|value| value.to_str());
        let protected = name.is_some_and(|name| artifact_is_protected(name, protected));
        if name.is_some_and(is_atomic_temporary) {
            if !protected {
                std::fs::remove_file(path)?;
            }
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("jpg") {
            continue;
        }
        let used = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if !protected && now.duration_since(used).unwrap_or_default() > max_age {
            std::fs::remove_file(path)?;
            continue;
        }
        total = total.saturating_add(metadata.len());
        if !protected {
            entries.push((used, metadata.len(), path));
        }
    }
    entries.sort_by_key(|entry| entry.0);
    let mut quota_reclaim = total.saturating_sub(quota_bytes);
    let mut free_shortfall = if minimum_free_bytes == 0 {
        0
    } else {
        minimum_free_bytes.saturating_sub(available_bytes(directory)?)
    };
    for (_, bytes, path) in entries {
        if quota_reclaim == 0 && free_shortfall == 0 {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                quota_reclaim = quota_reclaim.saturating_sub(bytes);
                total = total.saturating_sub(bytes);
                if free_shortfall > 0 {
                    free_shortfall = minimum_free_bytes.saturating_sub(available_bytes(directory)?);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                quota_reclaim = quota_reclaim.saturating_sub(bytes);
                total = total.saturating_sub(bytes);
                if free_shortfall > 0 {
                    free_shortfall = minimum_free_bytes.saturating_sub(available_bytes(directory)?);
                }
            }
            Err(_) => {}
        }
    }
    let quota_satisfied = quota_reclaim == 0;
    Ok(DerivedCacheMaintenance {
        bytes: total,
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
        directory,
        quota_bytes,
        max_age_days,
        minimum_free_bytes,
        &HashSet::new(),
        available_filesystem_bytes,
        |entry| entry.metadata(),
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
        assert!(!report.limits_satisfied);
        assert!(final_path.exists());
        assert!(temporary.exists());

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
            &directory,
            u64::MAX,
            36_500,
            100,
            &HashSet::new(),
            |_| {
                readings
                    .pop_front()
                    .ok_or_else(|| std::io::Error::other("unexpected free-space read"))
            },
            |entry| entry.metadata(),
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
            &directory,
            u64::MAX,
            36_500,
            100,
            &HashSet::new(),
            |_| Err(std::io::Error::other("statvfs failed")),
            |entry| entry.metadata(),
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
            &directory,
            u64::MAX,
            36_500,
            0,
            &HashSet::new(),
            |_| Ok(u64::MAX),
            |_| Err(std::io::Error::other("metadata failed")),
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
            &directory,
            0,
            36_500,
            0,
            &HashSet::new(),
            |_| Ok(u64::MAX),
            |entry| entry.metadata(),
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
