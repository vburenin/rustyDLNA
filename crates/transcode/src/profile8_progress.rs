//! Fixed-size Profile-8 stage diagnostics. File lengths and actual I/O are
//! deliberately distinct: signaling usually touches only the MP4 metadata tail.

use std::cell::Cell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemuxP8Stage {
    Probe,
    Extraction,
    Conversion,
    Wrapping,
    PacketRewrite,
    Signaling,
    FinalMux,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemuxP8StageStatus {
    Started,
    Progress,
    Succeeded,
    Failed,
    Cancelled,
    Deadline,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemuxP8IoBasis {
    /// Linux child rchar/wchar, including tool/configuration/diagnostic I/O.
    /// Best effort samples, never an exact total. Proc access may disappear
    /// during exec or before reaping, so even successful short stages can
    /// retain only a startup sample with zero writes. A stopped child can also
    /// do more I/O during termination. Inspect [`RemuxP8StageIo::is_complete`].
    ProcessCounters,
    /// Bytes returned by successful application read/write calls. This does
    /// not measure physical storage traffic or count file-length extension.
    ApplicationCounters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemuxP8StageIo {
    pub read_bytes: u64,
    pub written_bytes: u64,
    /// Linux storage counters; cache hits need not cause physical reads and
    /// delayed writeback means this is not a storage-device measurement.
    pub storage_read_bytes: Option<u64>,
    pub storage_written_bytes: Option<u64>,
    pub basis: RemuxP8IoBasis,
}

impl RemuxP8StageIo {
    /// Whether these counters cover all completed application operations up
    /// to the event. Process samples are always conservatively incomplete,
    /// including at Succeeded: a pre-reap proc read may be denied by the OS.
    /// This flag does not turn storage counters into device measurements.
    pub const fn is_complete(self) -> bool {
        matches!(self.basis, RemuxP8IoBasis::ApplicationCounters)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemuxP8StageEvent {
    pub stage: RemuxP8Stage,
    pub status: RemuxP8StageStatus,
    pub elapsed: Duration,
    /// Logical input file lengths, summed for the two final-mux inputs.
    pub input_bytes: Option<u64>,
    /// Current logical output file length, not bytes physically written.
    pub output_bytes: Option<u64>,
    pub io: Option<RemuxP8StageIo>,
}

pub(super) type IoCell = Cell<Option<RemuxP8StageIo>>;

/// Cache exhaustion must stop the job, including races after the last quota
/// observation; it must not select an additional HDR10 fallback producer.
pub(super) fn staging_io_error(operation: &str, error: std::io::Error) -> super::RemuxP8Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
    ) {
        super::RemuxP8Error::Observer(format!("{operation}: cache storage limit reached"))
    } else {
        super::RemuxP8Error::Pipeline(format!("{operation}: {error}"))
    }
}

#[cfg(target_os = "linux")]
pub(super) fn sample_process_io(pid: u32) -> Option<RemuxP8StageIo> {
    let file = std::fs::File::open(format!("/proc/{pid}/io")).ok()?;
    let mut bytes = String::new();
    file.take(4096).read_to_string(&mut bytes).ok()?;
    parse_process_io(&bytes)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sample_process_io(_pid: u32) -> Option<RemuxP8StageIo> {
    None
}

#[cfg(any(test, target_os = "linux"))]
fn parse_process_io(bytes: &str) -> Option<RemuxP8StageIo> {
    let value = |key: &str| {
        bytes.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name == key)
                .then(|| value.trim().parse::<u64>().ok())
                .flatten()
        })
    };
    Some(RemuxP8StageIo {
        read_bytes: value("rchar")?,
        written_bytes: value("wchar")?,
        storage_read_bytes: value("read_bytes"),
        storage_written_bytes: value("write_bytes"),
        basis: RemuxP8IoBasis::ProcessCounters,
    })
}

/// Count completed application operations, including partial reads/writes
/// before an I/O error. Seeking and sparse extension do not imply copying.
pub(super) struct SignalingFile<'a> {
    pub file: &'a mut std::fs::File,
    pub io: Option<&'a IoCell>,
}

impl Read for SignalingFile<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let read = self.file.read(bytes)?;
        if let Some(io) = self.io {
            if let Some(mut sample) = io.get() {
                sample.read_bytes = sample.read_bytes.saturating_add(read as u64);
                io.set(Some(sample));
            }
        }
        Ok(read)
    }
}

impl Write for SignalingFile<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(bytes)?;
        if let Some(io) = self.io {
            if let Some(mut sample) = io.get() {
                sample.written_bytes = sample.written_bytes.saturating_add(written as u64);
                io.set(Some(sample));
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Seek for SignalingFile<'_> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_storage_is_an_admission_failure_not_a_quality_fallback() {
        for kind in [
            std::io::ErrorKind::StorageFull,
            std::io::ErrorKind::QuotaExceeded,
        ] {
            assert!(matches!(
                staging_io_error("write staging", kind.into()),
                crate::RemuxP8Error::Observer(_)
            ));
        }
        assert!(matches!(
            staging_io_error("read staging", std::io::ErrorKind::UnexpectedEof.into()),
            crate::RemuxP8Error::Pipeline(_)
        ));
    }

    #[test]
    fn process_counters_distinguish_cached_io_and_reject_missing_counts() {
        let io =
            parse_process_io("rchar: 1048576\nwchar: 32768\nread_bytes: 0\nwrite_bytes: 4096\n")
                .unwrap();
        assert_eq!(io.read_bytes, 1048576);
        assert_eq!(io.storage_read_bytes, Some(0));
        assert_eq!(io.written_bytes, 32768);
        assert_eq!(io.storage_written_bytes, Some(4096));
        assert!(!io.is_complete());
        assert!(parse_process_io("rchar: 1\nwchar: truncated\n").is_none());
        assert!(parse_process_io("rchar: 18446744073709551616\nwchar: 0\n").is_none());
    }
}
