//! Policy-neutral bounded reads for already-authorized descriptors.

use std::io::{self, Read};

/// Failure to consume a stream within its caller-owned memory budget.
#[derive(Debug, thiserror::Error)]
pub enum BoundedReadError {
    #[error("read bounded stream: {0}")]
    Io(#[from] io::Error),
    #[error("stream exceeded the {limit}-byte limit")]
    LimitExceeded { limit: usize },
}

/// Read at most `limit` bytes and reject a stream with any additional byte.
///
/// The caller remains responsible for opening and authorizing the descriptor.
/// This function deliberately reads no more than `limit + 1` bytes, avoids an
/// eager allocation proportional to an untrusted limit, and distinguishes an
/// I/O failure from a complete but oversized stream.
pub fn read_to_end_bounded(
    mut reader: impl Read,
    limit: usize,
) -> Result<Vec<u8>, BoundedReadError> {
    let read_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    reader.by_ref().take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(BoundedReadError::LimitExceeded { limit });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_limit_and_short_streams_are_returned() {
        assert_eq!(read_to_end_bounded(&b"abc"[..], 3).unwrap(), b"abc");
        assert_eq!(read_to_end_bounded(&b"ab"[..], 3).unwrap(), b"ab");
        assert_eq!(read_to_end_bounded(&b""[..], 0).unwrap(), b"");
    }

    #[test]
    fn one_extra_byte_is_a_distinct_bounded_failure() {
        assert!(matches!(
            read_to_end_bounded(&b"abcd"[..], 3),
            Err(BoundedReadError::LimitExceeded { limit: 3 })
        ));
        assert!(matches!(
            read_to_end_bounded(&b"x"[..], 0),
            Err(BoundedReadError::LimitExceeded { limit: 0 })
        ));
    }

    #[test]
    fn underlying_io_errors_are_preserved() {
        struct FailingReader;

        impl Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            }
        }

        let error = read_to_end_bounded(FailingReader, 8).unwrap_err();
        assert!(matches!(
            error,
            BoundedReadError::Io(ref source)
                if source.kind() == io::ErrorKind::PermissionDenied
        ));
    }
}
