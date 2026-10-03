//! Bounded source facts for completed-output coverage decisions.
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

/// The source's start time and its `(index, codec_type)` streams in order.
fn streams(
    source: &File,
    deadline: Instant,
    cancelled: &AtomicBool,
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
    cancelled: &AtomicBool,
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
    cancelled: &AtomicBool,
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
            if cancelled.load(Ordering::Acquire) {
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
}
