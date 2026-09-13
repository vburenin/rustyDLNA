//! Original-file ranges retain the validated inode and an independent offset.
use crate::{App, CancellationToken};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::Duration;

// Keep the established write granularity/deadline even when experimenting with
// larger reads. A slow progressing connection must not need four times the rate.
const WRITE_BYTES: usize = 64 * 1024;
const READ_BYTES: usize = 256 * 1024;

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) async fn stream(
    app: &App,
    socket: &mut tokio::net::TcpStream,
    file: File,
    start: u64,
    end: u64,
) -> io::Result<()> {
    stream_with_read(app, socket, file, start, end, File::read_at).await
}

// The read boundary is injectable in tests so a cold/stalled disk operation can
// be held with explicit synchronization while real sockets continue to run.
pub(super) async fn stream_with_read<R>(
    app: &App,
    socket: &mut tokio::net::TcpStream,
    file: File,
    start: u64,
    end: u64,
    read: R,
) -> io::Result<()>
where
    R: Fn(&File, &mut [u8], u64) -> io::Result<usize> + Send + Sync + Clone + 'static,
{
    let cancelled = CancelOnDrop(CancellationToken::default());
    let transfer = async {
        let mut left = end
            .checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid original range"))?;
        let file = Arc::new(file);
        let mut offset = start;
        let mut buffer = vec![0; READ_BYTES];
        while left > 0 {
            let limit = if offset == start {
                WRITE_BYTES
            } else {
                buffer.len()
            };
            let length = left.min(limit as u64) as usize;
            let operation = async {
                let permit = app
                    .original_reads
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(io::Error::other)?;
                let file = file.clone();
                let read = read.clone();
                let cancellation = cancelled.0.clone();
                tokio::task::spawn_blocking(move || {
                    // A timed-out/aborted connection cannot release admission
                    // while its uninterruptible kernel read is still running.
                    // No worker is retained while the socket is backpressured.
                    let _permit = permit;
                    for _ in 0..16 {
                        if cancellation.is_cancelled() {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "original read cancelled",
                            ));
                        }
                        match read(&file, &mut buffer[..length], offset) {
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            result => return result.map(|got| (buffer, got)),
                        }
                    }
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "original read repeatedly interrupted",
                    ))
                })
                .await
                .map_err(io::Error::other)?
            };
            let (returned, got) =
                tokio::time::timeout(Duration::from_secs(app.cfg.write_timeout_secs), operation)
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "original read timeout")
                    })??;
            buffer = returned;
            if got == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "original file ended before its promised range",
                ));
            }
            for bytes in buffer[..got].chunks(WRITE_BYTES) {
                crate::socket_write_all(app, socket, bytes).await?;
            }
            left -= got as u64;
            if left > 0 {
                offset = offset.checked_add(got as u64).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "original range offset overflow",
                    )
                })?;
            }
        }
        Ok(())
    };
    let shutdown = async {
        loop {
            if app.scan_control.cancellation.is_cancelled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        result = transfer => result,
        () = shutdown => Err(io::Error::new(io::ErrorKind::Interrupted, "original delivery cancelled")),
    }
}
