//! Real media regression coverage for the Profile-8 packet/timeline contract.

use super::*;
use rusty_dlna_helper::{CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome};
use std::ffi::OsString;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch(label: &str) -> Scratch {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = Scratch(std::env::temp_dir().join(format!(
        "rustydlna-p8-{label}-{}-{stamp}",
        std::process::id()
    )));
    fs::create_dir(&dir.0).unwrap();
    dir
}

fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn supervised(program: &Path, args: &[OsString]) -> Result<Vec<u8>, String> {
    let mut command = Command::new(program);
    command.args(args);
    let outcome = SupervisedCommand::new(&mut command)
        .capture_stdout(CaptureConfig::new(4 * 1024 * 1024, CaptureRetention::Head))
        .capture_stderr(CaptureConfig::new(16 * 1024, CaptureRetention::Tail))
        .run_until(
            Instant::now() + Duration::from_secs(30),
            Duration::from_millis(20),
            || ControlFlow::<()>::Continue(()),
        )
        .map_err(|error| error.to_string())?;
    match outcome {
        SupervisedOutcome::Exited(output) if output.status.success() => Ok(output.stdout),
        SupervisedOutcome::Exited(output) => Err(format!(
            "{program:?}: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )),
        _ => Err(format!("{program:?}: media regression helper deadline")),
    }
}

fn ffmpeg(args: &[OsString]) -> Vec<u8> {
    let mut flags = strings(&["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    flags.extend_from_slice(args);
    supervised(Path::new("ffmpeg"), &flags).unwrap()
}

fn available() -> Option<PathBuf> {
    let Some(dovi) = dovi_tool_path() else {
        eprintln!("skip Profile-8 packet/pixel regression: dovi_tool unavailable");
        return None;
    };
    for tool in [Path::new("ffmpeg"), Path::new("ffprobe")] {
        if supervised(tool, &strings(&["-version"])).is_err() {
            eprintln!("skip Profile-8 packet/pixel regression: {tool:?} unavailable");
            return None;
        }
    }
    supervised(&dovi, &strings(&["--version"])).unwrap();
    Some(dovi)
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/library/video/dvp7.mkv")
}

fn packets(path: &Path, stream: &str) -> Vec<String> {
    let mut args = strings(&[
        "-v",
        "error",
        "-select_streams",
        stream,
        "-show_packets",
        "-show_entries",
        "packet=pts_time,dts_time,duration_time",
        "-of",
        "csv=p=0",
    ]);
    args.push(path.into());
    String::from_utf8(supervised(Path::new("ffprobe"), &args).unwrap())
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn pixels(path: &Path, seek: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(seek) = seek {
        args.extend(strings(&["-ss", seek]));
    }
    args.extend(strings(&["-threads", "2", "-i"]));
    args.push(path.into());
    args.extend(strings(&[
        "-map",
        "0:v:0",
        "-an",
        "-fps_mode",
        "passthrough",
    ]));
    if seek.is_some() {
        args.extend(strings(&["-frames:v", "3"]));
    }
    args.extend(strings(&["-f", "framemd5", "-"]));
    String::from_utf8(ffmpeg(&args))
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            line.rsplit_once(',')
                .map(|(_, hash)| hash.trim().to_owned())
        })
        .collect()
}

fn extracted_rpu(source: &Path, stem: &Path, dovi: &Path, convert: bool) -> Vec<u8> {
    let raw = stem.with_extension("hevc");
    let rpu = stem.with_extension("rpu");
    let mut args = strings(&["-i"]);
    args.push(source.into());
    args.extend(strings(&[
        "-map",
        "0:v:0",
        "-c:v",
        "copy",
        "-bsf:v",
        "hevc_mp4toannexb",
        "-an",
        "-f",
        "hevc",
    ]));
    args.push(raw.into());
    ffmpeg(&args);
    let mut args = if convert {
        strings(&["-m", "2"])
    } else {
        Vec::new()
    };
    args.extend(strings(&["extract-rpu", "-i"]));
    args.push(stem.with_extension("hevc").into());
    args.push("-o".into());
    args.push(rpu.clone().into());
    supervised(dovi, &args).unwrap();
    fs::read(rpu).unwrap()
}

fn verify_conversion(source: &Path, dir: &Path, dovi: &Path, audio_index: usize) {
    verify_conversion_mode(source, dir, dovi, audio_index, false);
}

fn verify_conversion_mode(
    source: &Path,
    dir: &Path,
    dovi: &Path,
    audio_index: usize,
    streaming: bool,
) {
    let converted = dir.join("converted.mp4.part");
    let reference = dir.join("reference.mp4");
    let source_file = fs::File::open(source).unwrap();
    let plan = TranscodePlan {
        decision: Decision::Recode,
        action: RecodeAction::RemuxP8,
        video_encoder: "copy".into(),
        audio: AudioAction::ToAc3,
        audio_index,
        container: "mp4",
        ..TranscodePlan::default()
    };
    if streaming {
        let toolchain = Profile8ToolchainSnapshot::query_paths(
            Path::new("ffmpeg"),
            Path::new("ffprobe"),
            dovi,
            None,
        )
        .unwrap();
        run_remux_p8_streaming_with_toolchain(
            &toolchain,
            RemuxP8Input::OpenFile {
                file: &source_file,
                identity_path: source,
            },
            &converted,
            &plan,
            Instant::now() + Duration::from_secs(60),
            &AtomicBool::new(false),
            &mut |_| Ok(()),
        )
        .unwrap();
    } else {
        run_remux_p8_file_controlled(
            &source_file,
            source,
            &converted,
            &plan,
            Instant::now() + Duration::from_secs(60),
            &AtomicBool::new(false),
        )
        .unwrap();
    }

    // A direct remux defines FFmpeg's common offset and encoder priming. Compare
    // each track separately because harmless packet interleaving can differ.
    let mut args = strings(&["-copyts", "-i"]);
    args.push(source.into());
    args.extend(strings(&[
        "-map",
        "0:v:0",
        "-map",
        &format!("0:a:{audio_index}"),
        "-c:v",
        "copy",
        "-tag:v",
        "hvc1",
        "-strict",
        "unofficial",
        "-c:a",
        "ac3",
        "-b:a",
        "640k",
    ]));
    args.extend(live_frag_os_tail(reference.as_os_str()));
    ffmpeg(&args);
    for stream in ["v:0", "a:0"] {
        let expected = packets(&reference, stream);
        assert!(!expected.is_empty(), "reference {stream} has no packets");
        assert_eq!(packets(&converted, stream), expected, "{stream} timing");
    }
    let audio_hash = |path: &Path| {
        let mut args = strings(&["-i"]);
        args.push(path.into());
        args.extend(strings(&["-map", "0:a:0", "-vn", "-f", "md5", "-"]));
        ffmpeg(&args)
    };
    assert_eq!(
        audio_hash(&converted),
        audio_hash(&reference),
        "selected audio must decode to the same samples as the direct reference"
    );
    let source_pixels = pixels(source, None);
    assert_eq!(source_pixels.len(), 259, "genuine fixture frame count");
    assert_eq!(
        pixels(&converted, None),
        source_pixels,
        "HDR base-layer pixels"
    );
    for seek in ["0.2", "5.0", "9.5"] {
        let expected = pixels(&reference, Some(seek));
        assert!(!expected.is_empty(), "seek {seek} decoded no frames");
        assert_eq!(pixels(&converted, Some(seek)), expected, "seek {seek}");
    }
    assert_eq!(
        extracted_rpu(&converted, &dir.join("actual"), dovi, false),
        extracted_rpu(source, &dir.join("expected"), dovi, true),
        "RPU bytes must match dovi_tool's explicit mode-2 conversion"
    );
    let mut args = strings(&["-v", "error", "-show_streams", "-of", "json"]);
    args.push(converted.into());
    let probe = String::from_utf8(supervised(Path::new("ffprobe"), &args).unwrap()).unwrap();
    for field in [
        "\"codec_tag_string\": \"hvc1\"",
        "\"pix_fmt\": \"yuv420p10le\"",
        "\"color_range\": \"tv\"",
        "\"color_space\": \"bt2020nc\"",
        "\"color_transfer\": \"smpte2084\"",
        "\"color_primaries\": \"bt2020\"",
        "\"dv_profile\": 8",
        "\"rpu_present_flag\": 1",
        "\"el_present_flag\": 0",
        "\"bl_present_flag\": 1",
        "\"dv_bl_signal_compatibility_id\": 1",
    ] {
        assert!(probe.contains(field), "missing {field}: {probe}");
    }
}

#[test]
fn streaming_fragment_rewrite_preserves_genuine_video_packets_and_pixels() {
    if supervised(Path::new("ffmpeg"), &strings(&["-version"])).is_err() {
        eprintln!("skip streaming fragment media regression: FFmpeg unavailable");
        return;
    }
    let dir = scratch("fragment-rewrite");
    let input = dir.0.join("input.mp4");
    let output = dir.0.join("output.mp4");
    let mut args = strings(&["-copyts", "-i"]);
    args.push(fixture().into());
    args.extend(strings(&[
        "-map",
        "0:v:0",
        "-c:v",
        "copy",
        "-tag:v",
        "hvc1",
        "-strict",
        "unofficial",
        "-an",
        "-map_chapters",
        "-1",
        "-avoid_negative_ts",
        "disabled",
        "-movflags",
        "frag_keyframe+empty_moov+delay_moov+default_base_moof",
        "-frag_duration",
        "1000000",
        "-f",
        "mp4",
    ]));
    args.push(input.clone().into());
    ffmpeg(&args);
    super::profile8_stream::rewrite(
        &mut fs::File::open(&input).unwrap(),
        &mut fs::File::create(&output).unwrap(),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(packets(&output, "v:0"), packets(&input, "v:0"));
    assert_eq!(pixels(&output, None), pixels(&input, None));
    let bytes = fs::read(&input).unwrap();
    for tag in [b"tfhd", b"trun"] {
        let mut malformed = bytes.clone();
        let offset = malformed.windows(4).position(|part| part == tag).unwrap();
        // Change the track ID or the trun data offset, respectively.
        let field = offset + if tag == b"tfhd" { 8 } else { 12 };
        malformed[field..field + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        let mut converted = Vec::new();
        assert!(super::profile8_stream::rewrite(
            &mut malformed.as_slice(),
            &mut converted,
            &mut || Ok(())
        )
        .is_err());
        assert!(!converted.windows(4).any(|part| part == b"moof"));
    }
    // Withhold the rest of the source after two fragments. The converter
    // must have delivered those complete fragments before waiting for EOF.
    struct Withheld<'a> {
        bytes: &'a [u8],
        at: usize,
        limit: usize,
        blocked: &'a AtomicBool,
    }
    impl std::io::Read for Withheld<'_> {
        fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
            if self.at == self.limit {
                self.blocked
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let size = target.len().min(self.limit - self.at);
            target[..size].copy_from_slice(&self.bytes[self.at..self.at + size]);
            self.at += size;
            Ok(size)
        }
    }
    let top = super::profile8_rewrite::children(&bytes, 0, bytes.len()).unwrap();
    let limit = top
        .iter()
        .filter(|item| item.kind == *b"mdat")
        .nth(1)
        .unwrap()
        .end;
    let blocked = AtomicBool::new(false);
    let mut reader = Withheld {
        bytes: &bytes,
        at: 0,
        limit,
        blocked: &blocked,
    };
    let mut partial = Vec::new();
    let error = super::profile8_stream::rewrite(&mut reader, &mut partial, &mut || {
        if blocked.load(std::sync::atomic::Ordering::Acquire) {
            Err(RemuxP8Error::Cancelled("test source withheld".into()))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(matches!(error, RemuxP8Error::Cancelled(_)));
    let boxes = super::profile8_rewrite::children(&partial, 0, partial.len()).unwrap();
    assert_eq!(boxes.iter().filter(|item| item.kind == *b"mdat").count(), 2);
    let mut args = strings(&["-v", "error", "-show_streams", "-of", "json"]);
    args.push(output.clone().into());
    let probe = String::from_utf8(supervised(Path::new("ffprobe"), &args).unwrap()).unwrap();
    assert!(probe.contains("\"dv_profile\": 8"), "{probe}");
    assert!(
        probe.contains("\"dv_bl_signal_compatibility_id\": 1"),
        "{probe}"
    );
}

#[test]
fn streaming_profile8_preserves_genuine_packet_timing_pixels_and_rpu() {
    let Some(dovi) = available() else { return };
    let dir = scratch("streaming-timing");
    verify_conversion_mode(&fixture(), &dir.0, &dovi, 0, true);
}

#[test]
fn streaming_profile8_cancellation_pressure_and_deadline_reap_both_helpers() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    let Some(dovi) = available() else { return };
    let dir = scratch("streaming-stop");
    let fragments = dir.0.join("fragments.mp4");
    // Repeat the tiny low-bitrate fixture past FFmpeg's input read-ahead,
    // then hold the producer open to test shutdown during active delivery.
    let mut args = strings(&["-copyts", "-stream_loop", "4", "-i"]);
    args.push(fixture().into());
    args.extend(strings(&[
        "-map",
        "0:v:0",
        "-c:v",
        "copy",
        "-tag:v",
        "hvc1",
        "-strict",
        "unofficial",
        "-an",
        "-map_chapters",
        "-1",
        "-avoid_negative_ts",
        "disabled",
        "-movflags",
        "frag_keyframe+empty_moov+delay_moov+default_base_moof",
        "-frag_duration",
        "1000000",
        "-f",
        "mp4",
    ]));
    args.push(fragments.clone().into());
    ffmpeg(&args);
    let wrapper = dir.0.join("ffmpeg-wrapper");
    // All substituted paths are generated ASCII-only private test paths.
    fs::write(
        &wrapper,
        format!(
            r#"#!/bin/sh
if [ "$1" = -version ]; then exec ffmpeg "$@"; fi
case "$*" in
  *pipe:7*)
    echo $$ > '{dir}/wrap.pid'
    cat '{fragments}' >&7
    sleep 30 &
    echo $! > '{dir}/sleep.pid'
    wait
    ;;
  *)
    echo $$ > '{dir}/mux.pid'
    exec ffmpeg "$@"
    ;;
esac
"#,
            dir = dir.0.display(),
            fragments = fragments.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let toolchain =
        Profile8ToolchainSnapshot::query_paths(&wrapper, Path::new("ffprobe"), &dovi, None)
            .unwrap();
    let plan = TranscodePlan {
        audio: AudioAction::ToAac,
        ..TranscodePlan::default()
    };
    for mode in ["cancel", "pressure", "deadline"] {
        let part = dir.0.join(format!("{mode}.mp4.part"));
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        let error = run_remux_p8_streaming_with_toolchain(
            &toolchain,
            RemuxP8Input::Path(&fixture()),
            &part,
            &plan,
            started + Duration::from_secs(2),
            &cancelled,
            &mut |_| {
                if part.metadata().is_ok_and(|m| m.len() > 1024) {
                    if mode == "cancel" {
                        cancelled.store(true, Ordering::Release);
                    }
                    if mode == "pressure" {
                        return Err("test cache pressure".into());
                    }
                }
                Ok(())
            },
        )
        .unwrap_err();
        match mode {
            "cancel" => assert!(matches!(error, RemuxP8Error::Cancelled(_)), "{error}"),
            "pressure" => assert!(matches!(error, RemuxP8Error::Observer(_)), "{error}"),
            _ => assert!(matches!(error, RemuxP8Error::Deadline(_)), "{error}"),
        }
        assert!(started.elapsed() < Duration::from_secs(4));
        for name in ["wrap.pid", "mux.pid", "sleep.pid"] {
            let pid = fs::read_to_string(dir.0.join(name)).unwrap();
            let process = PathBuf::from(format!("/proc/{}", pid.trim()));
            let deadline = Instant::now() + Duration::from_secs(1);
            while process.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!process.exists(), "{name} was not reaped");
        }
    }
}

#[test]
fn genuine_mel_preserves_packet_timing_pixels_and_converted_rpu() {
    let Some(dovi) = available() else { return };
    let dir = scratch("cfr-timing");
    verify_conversion(&fixture(), &dir.0, &dovi, 0);
}

#[test]
fn genuine_mel_vfr_preserves_timing_and_selected_offset_audio() {
    let Some(dovi) = available() else { return };
    let dir = scratch("vfr-timing");
    let source = dir.0.join("vfr.mkv");
    let mut args = strings(&["-i"]);
    args.push(fixture().into());
    args.extend(strings(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=14",
        "-itsoffset",
        "0.125",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=880:sample_rate=48000:duration=14",
        "-map",
        "0:v:0",
        "-map",
        "1:a:0",
        "-map",
        "2:a:0",
        "-c:v",
        "copy",
        "-bsf:v",
        "setts=pts=PTS+floor(PTS*TB/0.4)*0.04/TB",
        "-c:a",
        "aac",
        "-b:a",
        "96k",
    ]));
    args.push(source.clone().into());
    ffmpeg(&args);
    let original = packets(&fixture(), "v:0");
    let retimed = packets(&source, "v:0");
    assert_eq!(original.len(), retimed.len());
    assert_ne!(
        original, retimed,
        "VFR fixture must change actual packet timing"
    );
    verify_conversion(&source, &dir.0, &dovi, 1);
    verify_conversion_mode(&source, &dir.0, &dovi, 1, true);
}
