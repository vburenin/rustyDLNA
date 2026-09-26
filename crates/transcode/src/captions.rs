//! Bounded extraction of an embedded text track from a confined media file.

use rusty_dlna_helper::{
    CancellationToken, CaptureConfig, CaptureOverflow, CaptureReadError, CaptureRetention,
    SupervisedCommand, SupervisedOutcome,
};
use std::{
    fs::File,
    io,
    ops::ControlFlow,
    process::Command,
    time::{Duration, Instant},
};

/// Supported embedded text codecs. Bitmap subtitles require a different renderer.
pub fn embedded_caption_supported(codec: &str) -> bool {
    matches!(
        codec,
        "subrip" | "ass" | "ssa" | "webvtt" | "mov_text" | "text"
    )
}

/// Extract one absolute stream index without decoding or rewriting the video.
/// The caller owns helper admission and has verified this is a text subtitle.
pub fn extract_embedded_webvtt(
    source: &File,
    stream: usize,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> io::Result<Vec<u8>> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-max_alloc",
            "67108864",
            "-threads",
            "1",
            "-copyts",
            "-start_at_zero",
        ])
        .args(rusty_dlna_protocol::media_input::inherited_media_input_options(3))
        .args([
            "-i",
            "fd:",
            "-map",
            &format!("0:{stream}"),
            "-vn",
            "-an",
            "-dn",
            "-c:s",
            "webvtt",
            "-f",
            "webvtt",
            "pipe:1",
        ]);
    let outcome = SupervisedCommand::new(&mut command)
        .inherit_file_at(source, 3)?
        .capture_stdout(
            CaptureConfig::new(5 * 1024 * 1024, CaptureRetention::Head)
                .overflow(CaptureOverflow::Error)
                .read_error(CaptureReadError::Error),
        )
        .capture_stderr(CaptureConfig::new(4096, CaptureRetention::Tail))
        .run_until(Instant::now() + timeout, Duration::from_millis(50), || {
            if cancellation.is_cancelled() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .map_err(|_| io::Error::other("Subtitle extraction failed or exceeded its size limit"))?;
    match outcome {
        SupervisedOutcome::Exited(output) if output.status.success() => Ok(output.stdout),
        SupervisedOutcome::Deadline { .. } => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Subtitle extraction timed out",
        )),
        SupervisedOutcome::NotStarted { .. } | SupervisedOutcome::Stopped { .. } => Err(
            io::Error::new(io::ErrorKind::Interrupted, "Subtitle extraction cancelled"),
        ),
        _ => Err(io::Error::other(
            "This subtitle could not be converted to text",
        )),
    }
}
