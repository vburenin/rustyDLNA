//! Bounded source facts for completed-output coverage decisions and for the
//! output origin of a copied-video seek.
//!
//! The catalog records only a container duration. A container can run past
//! the streams a job selected (another audio or subtitle stream, or a trailing
//! gap), and a copied seek starts at whatever keyframe precedes it. When the
//! catalog alone rejects an output, these demux-only FFprobe reads of the
//! rooted source descriptor establish what the selected streams really cover.
use super::hls::SourceEvidence;
use rusty_dlna_helper::{
    CaptureConfig, CaptureOverflow, CaptureReadError, CaptureRetention, SupervisedCommand,
    SupervisedOutcome, SupervisionError,
};
use rusty_dlna_http::RemuxOutputExpectation;
use std::fs::File;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Packets are read from this far before the output's end, so a truncated
/// output always overlaps the window and shows the stream continuing.
const END_WINDOW_LEAD_SECONDS: f64 = 30.0;
/// A selected stream still producing packets this far past the output's end
/// proves the output truncated; reading further cannot change the verdict.
const END_WINDOW_TRAIL_SECONDS: f64 = 60.0;
/// Longest copied-video keyframe distance searched before a seek.
const KEYFRAME_SEARCH_SECONDS: f64 = 300.0;
/// Complete records only: more output than this is an error, never a
/// silently partial packet list. A 90-second window of a 22-stream
/// Blu-ray remux is about 0.6 MiB.
const CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROBE_DURATION: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Packet {
    stream: Option<usize>,
    start: f64,
    end: f64,
    keyframe: bool,
}

/// `audio_index` is the selected `0:a:N` stream for single-audio output;
/// multi-track output is judged against every audio stream, which can only
/// make the expectation stricter.
pub(super) fn gather(
    source: &File,
    expected: &RemuxOutputExpectation,
    output_end: f64,
    audio_index: usize,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<SourceEvidence, String> {
    let cancelled = &[cancelled];
    let (start_time, streams) = streams(source, deadline, cancelled)?;
    // Ordinals follow FFmpeg's `0:v:0` / `0:a:N` stream specifiers.
    let of_type = |kind: &str| {
        streams
            .iter()
            .filter(|(_, codec_type)| codec_type == kind)
            .map(|(index, _)| *index)
            .collect::<Vec<_>>()
    };
    let mut selected = Vec::new();
    if expected.video_codec.is_some() {
        selected.extend(of_type("video").first().copied());
    }
    let audio = of_type("audio");
    match expected.audio_codecs.len() {
        0 => {}
        1 => selected.extend(audio.get(audio_index).copied()),
        _ => selected.extend(audio),
    }
    if selected.is_empty() {
        return Err("selected source streams not found".into());
    }
    let seek = expected.seek_seconds.max(0.0);
    let window = (start_time + seek + output_end - END_WINDOW_LEAD_SECONDS).max(0.0);
    let span = END_WINDOW_LEAD_SECONDS + END_WINDOW_TRAIL_SECONDS;
    // Reading every stream lets the demuxer seek by its default (video)
    // index; restricting the read to an audio stream can force a linear scan
    // of a container whose seek index covers only video.
    let window_packets = packets(
        source,
        None,
        &format!("{window:.6}%+{span:.6}"),
        deadline,
        cancelled,
    )?;
    let selected_end = selected
        .iter()
        .map(|stream| {
            window_packets
                .iter()
                .filter(|packet| packet.stream == Some(*stream))
                .map(|packet| packet.end)
                // No packet in the window: the stream ended before it.
                .fold(window, f64::max)
        })
        .fold(window, f64::max);
    let keyframe_before_seek =
        if expected.video_copy && seek > 0.0 && expected.video_codec.is_some() {
            let target = start_time + seek;
            let from = (target - KEYFRAME_SEARCH_SECONDS).max(0.0);
            packets(
                source,
                Some("v:0"),
                &format!("{from:.6}%{:.6}", target + 0.1),
                deadline,
                cancelled,
            )?
            .iter()
            .filter(|packet| packet.keyframe && packet.start <= target + 0.001)
            .map(|packet| packet.start)
            .fold(None, |latest: Option<f64>, start| {
                Some(latest.map_or(start, |known| known.max(start)))
            })
            .map(|keyframe| keyframe - start_time)
        } else {
            None
        };
    Ok(SourceEvidence {
        selected_end: Some(selected_end - start_time),
        keyframe_before_seek,
    })
}

/// A keyframe landing found this far past the requested seek is not the
/// backward seek FFmpeg performs; the origin is then left unknown.
const LANDING_LEAD_TOLERANCE_SECONDS: f64 = 1.0;
/// FFmpeg's input seek moves this much earlier when a video stream has
/// decoder delay and the demuxer does not seek by presentation time.
const DTS_SEEK_HEURISTIC_SECONDS: f64 = 3.0 / 23.0;

/// Source time, relative to the container start, of the keyframe a copied
/// video `-ss <seek>` input seek begins at. `-avoid_negative_ts make_zero`
/// moves that keyframe to output time zero, so it is the output's origin.
///
/// This repeats FFmpeg's own demuxer seek with demux-only FFprobe reads of the
/// rooted descriptor: one stream-information read, then one video packet.
/// Index-seeking demuxers (Matroska, MP4) land on a keyframe. A generic
/// timestamp seek (MPEG-TS, M2TS and similar) can land between keyframes;
/// FFmpeg then still emits every other selected stream, such as copied or
/// encoded audio, from the landing while copied video waits for its next
/// keyframe, so output zero depends on which streams are mapped. Such a
/// landing returns `None`, as does any landing that cannot be established.
///
/// The origin is the keyframe's presentation time. Output zero is the
/// earliest timestamp across the muxed streams, so B-frame decode lead and
/// audio interleaved around the keyframe can place it a fraction of a second
/// earlier; the origin is accurate to well under one second, not one frame.
///
/// Any one of the `cancelled` flags stops the probe and reaps FFprobe.
pub(super) fn copied_seek_origin(
    source: &File,
    seek: f64,
    deadline: Instant,
    cancelled: &[&AtomicBool],
) -> Result<Option<f64>, String> {
    if !seek.is_finite() || seek <= 0.0 {
        return Ok(Some(0.0));
    }
    let output = ffprobe(
        source,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "format=format_name,start_time:stream=has_b_frames",
            "-of",
            "compact=p=0",
        ],
        deadline,
        cancelled,
    )?;
    let mut start_time = 0.0;
    let mut seeks_to_pts = false;
    let mut video_delay = false;
    for line in output.lines() {
        if let Some(value) = field(line, "start_time") {
            start_time = value;
        }
        if let Some(name) = line
            .split('|')
            .find_map(|pair| pair.strip_prefix("format_name="))
        {
            seeks_to_pts = demuxer_seeks_to_pts(name);
        }
        video_delay |= field(line, "has_b_frames").is_some_and(|delay| delay > 0.0);
    }
    let target = copied_seek_target(start_time, seek, seeks_to_pts, video_delay);
    let first = packets(
        source,
        Some("v:0"),
        &format!("{target:.6}%+#1"),
        deadline,
        cancelled,
    )?;
    Ok(first
        .first()
        .filter(|packet| packet.keyframe)
        .map(|keyframe| (keyframe.start - start_time).max(0.0))
        .filter(|origin| {
            origin.is_finite()
                && *origin >= seek - KEYFRAME_SEARCH_SECONDS
                && *origin <= seek + LANDING_LEAD_TOLERANCE_SECONDS
        }))
}

/// FFmpeg's MOV/MP4 demuxer is the admitted container family that declares
/// `AVFMT_SEEK_TO_PTS`; Matroska, MPEG-TS and others seek by decode time.
fn demuxer_seeks_to_pts(format_name: &str) -> bool {
    format_name.split(',').any(|name| name == "mov")
}

/// The absolute timestamp FFmpeg passes to its demuxer for `-ss <seek>`.
fn copied_seek_target(start_time: f64, seek: f64, seeks_to_pts: bool, video_delay: bool) -> f64 {
    let heuristic = if video_delay && !seeks_to_pts {
        DTS_SEEK_HEURISTIC_SECONDS
    } else {
        0.0
    };
    start_time + seek - heuristic
}

/// The source's start time and its `(index, codec_type)` streams in order.
fn streams(
    source: &File,
    deadline: Instant,
    cancelled: &[&AtomicBool],
) -> Result<(f64, Vec<(usize, String)>), String> {
    let output = ffprobe(
        source,
        &[
            "-show_entries",
            "stream=index,codec_type:format=start_time",
            "-of",
            "compact=p=0",
        ],
        deadline,
        cancelled,
    )?;
    let mut start_time = 0.0;
    let mut streams = Vec::new();
    for line in output.lines() {
        if let Some(value) = field(line, "start_time") {
            start_time = value;
        }
        let index = line
            .split('|')
            .find_map(|pair| pair.strip_prefix("index="))
            .and_then(|value| value.parse::<usize>().ok());
        let codec_type = line
            .split('|')
            .find_map(|pair| pair.strip_prefix("codec_type="));
        if let (Some(index), Some(codec_type)) = (index, codec_type) {
            streams.push((index, codec_type.to_owned()));
        }
    }
    Ok((start_time, streams))
}

fn packets(
    source: &File,
    selector: Option<&str>,
    interval: &str,
    deadline: Instant,
    cancelled: &[&AtomicBool],
) -> Result<Vec<Packet>, String> {
    let mut arguments = vec!["-read_intervals", interval];
    if let Some(selector) = selector {
        arguments.extend(["-select_streams", selector]);
    }
    arguments.extend([
        "-show_entries",
        "packet=stream_index,pts_time,dts_time,duration_time,flags",
        "-of",
        "compact=p=0",
    ]);
    let output = ffprobe(source, &arguments, deadline, cancelled)?;
    Ok(output.lines().filter_map(parse_packet).collect())
}

fn parse_packet(line: &str) -> Option<Packet> {
    let pts = field(line, "pts_time");
    let dts = field(line, "dts_time");
    let start = match (pts, dts) {
        (Some(pts), Some(dts)) => pts.max(dts),
        (time, None) | (None, time) => time?,
    };
    let duration = field(line, "duration_time").unwrap_or(0.0).max(0.0);
    let flags = line
        .split('|')
        .find_map(|pair| pair.strip_prefix("flags="))
        .unwrap_or("");
    let stream = line
        .split('|')
        .find_map(|pair| pair.strip_prefix("stream_index="))
        .and_then(|value| value.parse::<usize>().ok());
    Some(Packet {
        stream,
        start,
        end: start + duration,
        keyframe: flags.starts_with('K'),
    })
}

fn field(line: &str, name: &str) -> Option<f64> {
    line.split('|')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn ffprobe(
    source: &File,
    arguments: &[&str],
    deadline: Instant,
    cancelled: &[&AtomicBool],
) -> Result<String, String> {
    let deadline = deadline.min(Instant::now() + MAX_PROBE_DURATION);
    let mut command = std::process::Command::new("ffprobe");
    command
        .args(["-hide_banner", "-v", "error"])
        .args(arguments)
        .arg("/proc/self/fd/3");
    let outcome = SupervisedCommand::new(&mut command)
        .capture_stdout(
            CaptureConfig::new(CAPTURE_BYTES, CaptureRetention::Head)
                .overflow(CaptureOverflow::Error)
                .read_error(CaptureReadError::Error),
        )
        .capture_stderr(CaptureConfig::new(4096, CaptureRetention::Head))
        .inherit_file_at(source, 3)
        .map_err(|error| format!("source evidence descriptor: {error}"))?
        .run_until(deadline, POLL, || {
            if cancelled.iter().any(|flag| flag.load(Ordering::Acquire)) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .map_err(|error: SupervisionError| format!("source evidence: {error}"))?;
    match outcome {
        SupervisedOutcome::Exited(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        SupervisedOutcome::Exited(output) => Err(format!(
            "source evidence ffprobe {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        SupervisedOutcome::Deadline { .. } => Err("source evidence deadline exceeded".into()),
        _ => Err("source evidence cancelled".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_packet_records_parse_times_and_keyframes() {
        assert_eq!(
            parse_packet(
                "stream_index=4|pts_time=10.500000|dts_time=10.416667|duration_time=0.041667|flags=K__"
            ),
            Some(Packet {
                stream: Some(4),
                start: 10.5,
                end: 10.541667,
                keyframe: true
            })
        );
        let audio =
            parse_packet("pts_time=N/A|dts_time=3.000000|duration_time=N/A|flags=___").unwrap();
        assert_eq!((audio.start, audio.end, audio.keyframe), (3.0, 3.0, false));
        assert_eq!(parse_packet("start_time=0.000000"), None);
        assert_eq!(field("start_time=1.400000", "start_time"), Some(1.4));
    }

    #[test]
    fn copied_seek_target_mirrors_ffmpeg_demuxer_seek() {
        assert!(demuxer_seeks_to_pts("mov,mp4,m4a,3gp,3g2,mj2"));
        assert!(!demuxer_seeks_to_pts("matroska,webm"));
        assert!(!demuxer_seeks_to_pts("mpegts"));
        assert_eq!(copied_seek_target(0.0, 30.0, true, true), 30.0);
        assert_eq!(copied_seek_target(1.4, 30.0, false, false), 31.4);
        let delayed = copied_seek_target(-0.005, 30.0, false, true);
        assert!((delayed - (30.0 - 0.005 - 3.0 / 23.0)).abs() < 1e-9);
    }
}
