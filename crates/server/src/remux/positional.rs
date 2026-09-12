//! Bounded delivery from an already validated, generation-pinned descriptor.
//! `pread` leaves the shared Unix open-file cursor untouched, including when
//! another owner indexes the file or a producer publishes it by rename.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

// One batch is owned by the socket task or its single blocking read. There is
// no read-ahead queue and no blocking worker held while a slow socket drains.
pub(super) const READ_BYTES: usize = 256 * 1024;

pub(super) fn read_chunk(
    file: &File,
    buf: &mut [u8],
    offset: u64,
    end: Option<u64>,
) -> io::Result<usize> {
    let remaining = end.map_or(buf.len() as u64, |end| {
        if offset > end {
            0
        } else {
            end.saturating_sub(offset).saturating_add(1)
        }
    });
    let count = remaining.min(buf.len() as u64) as usize;
    // Bound interruption retries so cancellation can regain control even when
    // this process receives a sustained signal stream.
    for _ in 0..16 {
        match file.read_at(&mut buf[..count], offset) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "positioned output read repeatedly interrupted",
    ))
}

#[cfg(test)]
mod tests;
