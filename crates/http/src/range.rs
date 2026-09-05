//! rustyDLNA handling for `Range: bytes=`.

/// An opaque validator for a completed, opened original file. Change timestamps
/// and physical identity prevent a same-size replacement from reusing a range
/// belonging to an older file. This does not validate a growing media output.
pub fn original_file_etag(metadata: &std::fs::Metadata) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    metadata.len().hash(&mut hash);
    metadata.modified().ok()?.hash(&mut hash);
    metadata.created().ok().hash(&mut hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.dev().hash(&mut hash);
        metadata.ino().hash(&mut hash);
        metadata.ctime().hash(&mut hash);
        metadata.ctime_nsec().hash(&mut hash);
    }
    Some(format!("\"original-{:016x}\"", hash.finish()))
}

/// A finalized cache artifact is immutable until its completion stamp is
/// rewritten. Cache eviction touches the media mtime on reads, so its validator
/// uses the stamp's change identity plus the artifact's physical identity.
pub fn completed_cache_etag(
    metadata: &std::fs::Metadata,
    stamp: &std::fs::Metadata,
) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    original_file_etag(stamp)?.hash(&mut hash);
    metadata.len().hash(&mut hash);
    metadata.created().ok().hash(&mut hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.dev().hash(&mut hash);
        metadata.ino().hash(&mut hash);
    }
    Some(format!("\"completed-{:016x}\"", hash.finish()))
}

/// If-Range requires a matching strong validator. Unknown dates, weak tags,
/// and changed files fall back to a full response, never a mixed partial file.
pub fn if_range_matches(if_range: Option<&str>, etag: Option<&str>) -> bool {
    match if_range {
        None => true,
        Some(value) => etag.is_some_and(|etag| value.trim() == etag && !etag.starts_with("W/")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    /// Inclusive start.
    pub start: u64,
    /// Inclusive end.
    pub end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    /// Malformed header → HTTP 400.
    Invalid,
    /// Start past EOF → HTTP 416.
    Unsatisfiable,
}

/// Parse `Range`. `None` means no range (full file).
pub fn parse_byte_range(value: &str, size: u64) -> Result<Option<ByteRange>, RangeError> {
    let v = value.trim();
    let Some(spec) = v
        .strip_prefix("bytes=")
        .or_else(|| v.strip_prefix("BYTES="))
    else {
        return Err(RangeError::Invalid);
    };
    let spec = spec.trim();
    if spec.is_empty() || spec.contains(',') {
        return Err(RangeError::Invalid);
    }
    if size == 0 {
        return Err(RangeError::Unsatisfiable);
    }
    if let Some(suffix) = spec.strip_prefix('-') {
        let n: u64 = suffix.parse().map_err(|_| RangeError::Invalid)?;
        if n == 0 {
            return Err(RangeError::Invalid);
        }
        let n = n.min(size);
        return Ok(Some(ByteRange {
            start: size - n,
            end: size - 1,
        }));
    }
    let (a, b) = spec.split_once('-').ok_or(RangeError::Invalid)?;
    let start: u64 = a.parse().map_err(|_| RangeError::Invalid)?;
    if start >= size {
        return Err(RangeError::Unsatisfiable);
    }
    let end = if b.is_empty() {
        size - 1
    } else {
        let e: u64 = b.parse().map_err(|_| RangeError::Invalid)?;
        if e < start {
            return Err(RangeError::Invalid);
        }
        e.min(size - 1)
    };
    Ok(Some(ByteRange { start, end }))
}

/// Range when the total size is not known yet (growing remux).
/// `end = None` means “from start to whatever is produced.”
pub fn parse_open_range(value: &str) -> Result<(u64, Option<u64>), RangeError> {
    let v = value.trim();
    let Some(spec) = v
        .strip_prefix("bytes=")
        .or_else(|| v.strip_prefix("BYTES="))
    else {
        return Err(RangeError::Invalid);
    };
    let spec = spec.trim();
    if spec.is_empty() || spec.contains(',') || spec.starts_with('-') {
        return Err(RangeError::Invalid);
    }
    let (a, b) = spec.split_once('-').ok_or(RangeError::Invalid)?;
    let start: u64 = a.parse().map_err(|_| RangeError::Invalid)?;
    if b.is_empty() {
        return Ok((start, None));
    }
    let end: u64 = b.parse().map_err(|_| RangeError::Invalid)?;
    if end < start {
        return Err(RangeError::Invalid);
    }
    Ok((start, Some(end)))
}

pub fn range_len(r: ByteRange) -> u64 {
    r.end.saturating_sub(r.start).saturating_add(1)
}

/// Read `[start, end]` inclusive from `path`. This is the GET body path.
pub fn read_file_range(path: &std::path::Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let len = usize::try_from(end.saturating_sub(start).saturating_add(1)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "requested range does not fit in memory",
        )
    })?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let r = parse_byte_range("bytes=0-99", 1000).unwrap().unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 99 });
        let r = parse_byte_range("bytes=100-", 1000).unwrap().unwrap();
        assert_eq!(
            r,
            ByteRange {
                start: 100,
                end: 999
            }
        );
        let r = parse_byte_range("bytes=-10", 1000).unwrap().unwrap();
        assert_eq!(
            r,
            ByteRange {
                start: 990,
                end: 999
            }
        );
        assert_eq!(
            parse_byte_range("bytes=1000-2000", 1000),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_byte_range("bytes=abc", 1000),
            Err(RangeError::Invalid)
        );
        assert_eq!(
            parse_byte_range("bytes=50-10", 1000),
            Err(RangeError::Invalid)
        );
        assert_eq!(parse_open_range("bytes=0-1").unwrap(), (0, Some(1)));
        assert_eq!(parse_open_range("bytes=100-").unwrap(), (100, None));
        assert_eq!(parse_open_range("bytes=50-10"), Err(RangeError::Invalid));
    }

    #[test]
    fn range_arithmetic_properties_hold_exhaustively_for_small_files() {
        for size in 1_u64..=128 {
            for start in 0..size {
                for requested_end in start..=size.saturating_add(8) {
                    let raw = format!("bytes={start}-{requested_end}");
                    let range = parse_byte_range(&raw, size).unwrap().unwrap();
                    assert_eq!(range.start, start);
                    assert_eq!(range.end, requested_end.min(size - 1));
                    assert!(range.start <= range.end);
                    assert!(range.end < size);
                    assert_eq!(range_len(range), range.end - range.start + 1);
                }

                let open = parse_byte_range(&format!("bytes={start}-"), size)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    open,
                    ByteRange {
                        start,
                        end: size - 1
                    }
                );
                assert_eq!(range_len(open), size - start);
            }

            for suffix in 1..=size.saturating_add(8) {
                let range = parse_byte_range(&format!("bytes=-{suffix}"), size)
                    .unwrap()
                    .unwrap();
                assert_eq!(range.end, size - 1);
                assert_eq!(range_len(range), suffix.min(size));
            }
        }
    }

    #[test]
    fn original_validator_detects_equal_size_and_mtime_replacement() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rustydlna-validator-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join("source");
        let replacement = directory.join("replacement");
        std::fs::write(&source, b"old-bytes").unwrap();
        let original = std::fs::metadata(&source).unwrap();
        let tag = original_file_etag(&original).unwrap();
        assert_eq!(
            original_file_etag(&std::fs::metadata(&source).unwrap()),
            Some(tag.clone())
        );
        assert!(if_range_matches(Some(&tag), Some(&tag)));
        assert!(if_range_matches(None, Some(&tag)));
        assert!(!if_range_matches(Some(&format!("W/{tag}")), Some(&tag)));
        assert!(!if_range_matches(
            Some("Wed, 01 Jan 2020 00:00:00 GMT"),
            Some(&tag)
        ));
        std::fs::write(&replacement, b"new-bytes").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(original.modified().unwrap())
            .unwrap();
        std::fs::rename(&replacement, &source).unwrap();
        let changed = original_file_etag(&std::fs::metadata(&source).unwrap()).unwrap();
        assert_ne!(changed, tag);
        assert!(!if_range_matches(Some(&tag), Some(&changed)));
        let stamp_file = directory.join("completion-stamp");
        std::fs::write(&stamp_file, b"synthetic-cache-key").unwrap();
        let stamp_metadata = std::fs::metadata(&stamp_file).unwrap();
        let cache_tag =
            completed_cache_etag(&std::fs::metadata(&source).unwrap(), &stamp_metadata).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
            .unwrap();
        assert_eq!(
            completed_cache_etag(&std::fs::metadata(&source).unwrap(), &stamp_metadata),
            Some(cache_tag.clone()),
            "Cache recency touches must not invalidate preserved download bytes"
        );
        std::fs::write(&stamp_file, b"new-completed-cache-key").unwrap();
        assert_ne!(
            completed_cache_etag(
                &std::fs::metadata(&source).unwrap(),
                &std::fs::metadata(&stamp_file).unwrap()
            ),
            Some(cache_tag)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
