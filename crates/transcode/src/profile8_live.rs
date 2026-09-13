//! A bounded pipe connects the timestamped video wrapper, per-fragment RPU
//! conversion, and final audio/video mux. Every process remains supervised;
//! a failed stage, quota rejection, deadline or cancellation stops all stages.

use super::*;
use rusty_dlna_helper::{CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome};
use std::fs::File;
use std::ops::ControlFlow;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct StopOnDrop<'a>(&'a AtomicBool);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn pipe() -> Result<(UnixStream, File), RemuxP8Error> {
    let (parent, child) = UnixStream::pair().map_err(|error| error.to_string())?;
    parent
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| error.to_string())?;
    parent
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| error.to_string())?;
    Ok((parent, File::from(OwnedFd::from(child))))
}

fn helper(
    tool: &VerifiedExecutable,
    args: &[OsString],
    inherited: &[(&File, i32)],
    deadline: Instant,
    mut check: impl FnMut() -> Result<(), RemuxP8Error>,
) -> Result<(), RemuxP8Error> {
    tool.verify_current("ffmpeg")
        .map_err(RemuxP8Error::Pipeline)?;
    let mut command = tool.command();
    command.args(args);
    let mut runner = SupervisedCommand::new(&mut command)
        .capture_stderr(CaptureConfig::new(64 * 1024, CaptureRetention::Tail));
    for (file, fd) in inherited {
        runner = runner
            .inherit_file_at(file, *fd)
            .map_err(|error| error.to_string())?;
    }
    runner = tool
        .inherit_for_execution(runner)
        .map_err(|error| error.to_string())?;
    match runner
        .run_until(deadline, Duration::from_millis(20), || match check() {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        })
        .map_err(|error| error.to_string())?
    {
        SupervisedOutcome::Exited(output) if output.status.success() => Ok(()),
        SupervisedOutcome::Exited(output) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            let lower = detail.to_ascii_lowercase();
            if lower.contains("no space left on device") || lower.contains("disk quota exceeded") {
                return Err(RemuxP8Error::Observer(
                    "Profile-8 cache storage limit reached".into(),
                ));
            }
            Err(RemuxP8Error::Pipeline(format!(
                "Profile-8 FFmpeg {}: {detail}",
                output.status
            )))
        }
        SupervisedOutcome::NotStarted { reason } | SupervisedOutcome::Stopped { reason, .. } => {
            Err(reason)
        }
        SupervisedOutcome::Deadline { .. } => Err(RemuxP8Error::Deadline(
            "Profile-8 streaming deadline".into(),
        )),
    }
}

fn run(
    toolchain: &Profile8ToolchainSnapshot,
    source: &File,
    output: &Path,
    plan: &TranscodePlan,
    control: &mut RemuxP8Control<'_>,
) -> Result<(), RemuxP8Error> {
    let source_video = reopen_media_input(source).map_err(|error| error.to_string())?;
    let source_audio = reopen_media_input(source).map_err(|error| error.to_string())?;
    let (mut video_read, video_write) = pipe()?;
    let (mut converted_write, converted_read) = pipe()?;
    let mut wrap: Vec<OsString> = [
        "-hide_banner",
        "-nostats",
        "-nostdin",
        "-copyts",
        "-i",
        "/proc/self/fd/3",
        "-map",
        "0:v:0",
        "-c:v",
        "copy",
        "-tag:v",
        "hvc1",
        "-an",
        "-map_chapters",
        "-1",
        "-strict",
        "unofficial",
        "-avoid_negative_ts",
        "disabled",
        "-movflags",
        "frag_keyframe+empty_moov+delay_moov+default_base_moof",
        "-frag_duration",
        "1000000",
        "-flush_packets",
        "1",
        "-f",
        "mp4",
        "pipe:7",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    use_inherited_media_input(&mut wrap, 0, 6)?;
    let mut mux: Vec<OsString> = [
        "-hide_banner",
        "-nostats",
        "-nostdin",
        "-y",
        "-copyts",
        "-protocol_whitelist",
        "pipe",
        "-format_whitelist",
        "mov",
        "-f",
        "mp4",
        "-probesize",
        "1048576",
        "-analyzeduration",
        "1000000",
        "-i",
        "pipe:6",
        "-i",
        "/proc/self/fd/3",
        "-map",
        "0:v:0",
        "-map",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    mux.push(format!("1:a:{}?", plan.audio_index).into());
    mux.extend(["-c:v", "copy", "-tag:v", "hvc1", "-strict", "unofficial"].map(Into::into));
    match plan.audio {
        AudioAction::Copy => mux.extend(["-c:a", "copy"].map(Into::into)),
        AudioAction::ToAc3 => mux.extend(["-c:a", "ac3", "-b:a", "640k"].map(Into::into)),
        AudioAction::ToAac => mux.extend(["-c:a", "aac", "-b:a", "256k"].map(Into::into)),
    }
    mux.extend(live_frag_os_tail(output.as_os_str()));
    use_inherited_media_input(&mut mux, 1, 7)?;
    let stop = AtomicBool::new(false);
    let cancelled = control.cancelled;
    let deadline = control.deadline;
    let stage_check = || {
        if cancelled.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
            Err(RemuxP8Error::Cancelled("Profile-8 stream stopped".into()))
        } else if Instant::now() >= deadline {
            Err(RemuxP8Error::Deadline("Profile-8 stream deadline".into()))
        } else {
            Ok(())
        }
    };
    std::thread::scope(|scope| {
        // Dropped before scoped joins, including thread creation failure.
        let stop_on_exit = StopOnDrop(&stop);
        let wrap_thread = std::thread::Builder::new()
            .name("profile8-wrap".into())
            .spawn_scoped(scope, || {
                let result = helper(
                    toolchain.ffmpeg(),
                    &wrap,
                    &[(&source_video, 6), (&video_write, 7)],
                    deadline,
                    stage_check,
                );
                // These descriptors must close before the converter can see EOF.
                drop(video_write);
                if result.is_err() {
                    stop.store(true, Ordering::Release);
                }
                result
            })
            .map_err(|error| error.to_string())?;
        let rewrite_thread = std::thread::Builder::new()
            .name("profile8-rpu".into())
            .spawn_scoped(scope, || {
                // A third-party parser panic must close the pipe and stop helpers,
                // rather than leave the final mux blocked waiting for more bytes.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    super::profile8_stream::rewrite(&mut video_read, &mut converted_write, &mut {
                        stage_check
                    })
                }))
                .unwrap_or_else(|_| {
                    Err(RemuxP8Error::Pipeline("Profile-8 RPU parser failed".into()))
                });
                drop(video_read);
                drop(converted_write);
                if result.is_err() {
                    stop.store(true, Ordering::Release);
                }
                result
            })
            .map_err(|error| error.to_string())?;
        let result = helper(
            toolchain.ffmpeg(),
            &mux,
            &[(&converted_read, 6), (&source_audio, 7)],
            deadline,
            || {
                control
                    .check("Profile-8 streaming mux")
                    .map_err(RemuxP8Error::from)?;
                stage_check()
            },
        );
        drop(converted_read);
        if result.is_err() {
            stop.store(true, Ordering::Release);
        }
        let rewrite = rewrite_thread
            .join()
            .map_err(|_| RemuxP8Error::Pipeline("Profile-8 converter thread failed".into()))?;
        let wrapped = wrap_thread
            .join()
            .map_err(|_| RemuxP8Error::Pipeline("Profile-8 wrapper thread failed".into()))?;
        drop(stop_on_exit);
        // Retain the initiating error, especially quota/cancellation/deadline,
        // rather than a secondary broken-pipe or peer-stage cancellation.
        if cancelled.load(Ordering::Acquire) {
            return Err(RemuxP8Error::Cancelled("Profile-8 stream cancelled".into()));
        }
        if matches!(
            &result,
            Err(RemuxP8Error::Observer(_) | RemuxP8Error::Deadline(_))
        ) {
            return result;
        }
        if matches!(&rewrite, Err(RemuxP8Error::Pipeline(_))) {
            return rewrite;
        }
        if matches!(&wrapped, Err(RemuxP8Error::Pipeline(_))) {
            return wrapped;
        }
        result?;
        rewrite?;
        wrapped
    })
}

/// Convert Profile 7 in bounded timestamped fragments and publish only through
/// the server's existing final-mux readiness/validation boundary.
pub fn run_remux_p8_streaming_with_toolchain(
    toolchain: &Profile8ToolchainSnapshot,
    input: RemuxP8Input<'_>,
    dest_part: &Path,
    plan: &TranscodePlan,
    deadline: Instant,
    cancelled: &AtomicBool,
    observer: &mut dyn FnMut(RemuxP8StageEvent) -> Result<(), String>,
) -> Result<(), RemuxP8Error> {
    let source = match input {
        RemuxP8Input::Path(path) => File::open(path),
        RemuxP8Input::OpenFile { file, .. } => reopen_media_input(file),
    }
    .map_err(|error| error.to_string())?;
    if !source
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Profile-8 source must be a regular file".into());
    }
    let mut stages = Profile8StageRunner {
        deadline,
        cancelled,
        observer,
        progress_interval: Duration::from_millis(100),
    };
    stages.run(
        RemuxP8Stage::FinalMux,
        source.metadata().ok().map(|m| m.len()),
        Some(dest_part),
        |control| run(toolchain, &source, dest_part, plan, control),
    )
}
