//! `Range: bytes=` as the dialect understands it.

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
pub fn read_file_range(
    path: &std::path::Path,
    start: u64,
    end: u64,
) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let len = (end.saturating_sub(start).saturating_add(1)) as usize;
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
        assert_eq!(r, ByteRange { start: 100, end: 999 });
        let r = parse_byte_range("bytes=-10", 1000).unwrap().unwrap();
        assert_eq!(r, ByteRange { start: 990, end: 999 });
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
}
