//! Opt-in Linux evidence for EVENT history. This does not certify Safari playback.
use super::*;
use rusty_dlna_helper::{CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome};
use rusty_dlna_transcode::{
    AudioAction, AudioCodec, BrowserEncodingPreset, BrowserOutputOptions, BrowserQuality, Decision,
    HardwareDecode, HdrKind, RecodeAction, TranscodePlan, VideoCodec,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn run(command: &mut Command, log: &mut File) -> Vec<u8> {
    writeln!(log, "{}", json!({"program":command.get_program().to_string_lossy(), "args":command.get_args().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>()})).unwrap();
    let result = SupervisedCommand::new(command)
        .capture_stdout(CaptureConfig::new(16 * 1024 * 1024, CaptureRetention::Head))
        .capture_stderr(CaptureConfig::new(64 * 1024, CaptureRetention::Tail))
        .run_until(
            Instant::now() + Duration::from_secs(180),
            Duration::from_millis(20),
            || std::ops::ControlFlow::<()>::Continue(()),
        )
        .unwrap();
    match result {
        SupervisedOutcome::Exited(result) => {
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            result.stdout
        }
        other => panic!("native history helper did not complete: {other:?}"),
    }
}

fn digest(path: &Path) -> String {
    let mut file = File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let count = file.read(&mut bytes).unwrap();
        if count == 0 {
            break;
        }
        hasher.update(&bytes[..count]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
#[ignore = "release long variable-GOP copy/index/decoder evidence; requires an owned RUSTY_DLNA_NATIVE_EVIDENCE directory"]
fn measure_native_variable_gop_history() {
    let directory = PathBuf::from(
        std::env::var_os("RUSTY_DLNA_NATIVE_EVIDENCE").expect("new evidence directory"),
    );
    std::fs::create_dir(&directory).expect("evidence directory must not already exist");
    let mut log = File::create(directory.join("commands.jsonl")).unwrap();
    let version = run(Command::new("ffmpeg").arg("-version"), &mut log);
    std::fs::write(directory.join("ffmpeg-version.txt"), version).unwrap();
    let mut records = File::create(directory.join("measurements.jsonl")).unwrap();
    for seconds in [7200, 28_800] {
        let source = directory.join(format!("source-{seconds}.mp4"));
        let output = directory.join(format!("copy-{seconds}.mp4"));
        // A small decodable frame keeps the history experiment affordable. Early
        // two-second GOPs become twelve-second GOPs in the last quarter.
        let force = format!("expr:if(isnan(prev_forced_t),1,if(lte(t,{}),gte(t,prev_forced_t+2),gte(t,prev_forced_t+12)))", seconds * 3 / 4);
        run(
            Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=128x72:rate=2",
                    "-t",
                    &seconds.to_string(),
                    "-an",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "1",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "24",
                    "-keyint_min",
                    "1",
                    "-sc_threshold",
                    "0",
                    "-bf",
                    "0",
                    "-force_key_frames",
                    &force,
                ])
                .arg(&source),
            &mut log,
        );
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            rule: None,
            keep_hdr10: false,
            drop_dolby_vision: false,
            video_encoder: "copy".into(),
            hardware_decode: HardwareDecode::None,
            audio: AudioAction::Copy,
            container: "mp4",
            audio_index: 0,
            browser_quality: Some(BrowserQuality::Auto),
            browser_ai_upscale: None,
            download_audio: None,
        };
        let mut args = rusty_dlna_transcode::browser_ffmpeg_os_args(
            &source,
            &output,
            &plan,
            BrowserOutputOptions {
                encoding_preset: BrowserEncodingPreset::Balanced,
                source_video: Some(VideoCodec::H264),
                selected_audio: AudioCodec::Other,
                source_hdr: HdrKind::Sdr,
                start_seconds: 0,
                // Copy-only HLS already omits pacing in production. Exercise the
                // native recipe without overriding any generated arguments.
                hls: true,
            },
        );
        assert_eq!(args.remove(0), "ffmpeg");
        assert!(!args.iter().any(|arg| arg == "-readrate"));
        run(Command::new("ffmpeg").args(args), &mut log);
        let mut hashes = Vec::new();
        for path in [&source, &output] {
            hashes.push(run(
                Command::new("ffmpeg")
                    .args(["-nostdin", "-v", "error", "-threads", "1", "-i"])
                    .arg(path)
                    .args(["-map", "0:v:0", "-f", "hash", "-hash", "sha256", "-"]),
                &mut log,
            ));
        }
        assert_eq!(hashes[0], hashes[1], "copied video changed decoded frames");
        std::fs::write(
            directory.join(format!("decoded-{seconds}.sha256")),
            &hashes[0],
        )
        .unwrap();
        let oracle = run(
            Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_packets",
                    "-show_entries",
                    "packet=pts_time,flags",
                    "-of",
                    "json",
                ])
                .arg(&output),
            &mut log,
        );
        std::fs::write(directory.join(format!("packets-{seconds}.json")), &oracle).unwrap();
        let oracle: serde_json::Value = serde_json::from_slice(&oracle).unwrap();
        let mut boundaries: Vec<f64> = oracle["packets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|packet| packet["flags"].as_str().unwrap().contains('K'))
            .map(|packet| packet["pts_time"].as_str().unwrap().parse().unwrap())
            .collect();
        boundaries.push(seconds as f64);
        let expected: Vec<f64> = boundaries
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .collect();
        assert_eq!(expected.first(), Some(&2.0));
        assert_eq!(expected.last(), Some(&12.0));
        let file = File::open(&output).unwrap();
        let expectation = rusty_dlna_http::RemuxOutputExpectation {
            video_codec: Some("h264".into()),
            audio_codecs: Vec::new(),
            duration_seconds: Some(seconds as f64),
            seek_seconds: 0.0,
            video_copy: true,
        };
        validate_finished(
            &file,
            &expectation,
            Instant::now() + Duration::from_secs(30),
            &AtomicBool::new(false),
        )
        .unwrap();
        let uri = "/web/media/9100010.m4s?mode=compatible&quality=auto&video_mode=copy&audio_mode=copy&delivery=hls_segment&session=1&request=2";
        for trial in 0..10 {
            let mut index = Index::default();
            let started = Instant::now();
            index.update_file(&file, false).unwrap();
            let parse_ns = started.elapsed().as_nanos();
            index.update_file(&file, true).unwrap();
            let started = Instant::now();
            let mut reopened = Index::default();
            reopened.update_file(&file, true).unwrap();
            let reuse_ns = started.elapsed().as_nanos();
            let started = Instant::now();
            let view = reopened.playlist_view_for(false, Some((1, 2))).unwrap();
            let view_ns = started.elapsed().as_nanos();
            let started = Instant::now();
            let text = view.render(uri, uri).unwrap();
            let render_ns = started.elapsed().as_nanos();
            let actual: Vec<f64> = text
                .lines()
                .filter_map(|line| line.strip_prefix("#EXTINF:"))
                .map(|duration| duration.trim_end_matches(',').parse().unwrap())
                .collect();
            assert_eq!(
                actual.len(),
                expected.len(),
                "EVENT omitted earlier seek history"
            );
            for (actual, expected) in actual.iter().zip(&expected) {
                assert!((actual - expected).abs() < 0.001);
            }
            assert!(text.contains("#EXT-X-TARGETDURATION:12\n"));
            assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
            assert!(text.ends_with("#EXT-X-ENDLIST\n"));
            if trial == 0 {
                std::fs::write(directory.join(format!("event-{seconds}.m3u8")), &text).unwrap();
            }
            writeln!(records, "{}", json!({"seconds":seconds, "trial":trial, "source_sha256":digest(&source), "output_sha256":digest(&output), "segments":actual.len(), "playlist_bytes":text.len(), "parse_ns":parse_ns, "completed_reuse_ns":reuse_ns, "view_ns":view_ns, "format_ns":render_ns, "retained_index_bytes":index.retained_bytes(), "target_duration":12, "native_client":null, "limitations":"Linux metadata/FFmpeg decoder experiment; accelerated 128x72 2fps video-only fixture; no native polling, seek, reconnect or daemon-restart evidence"})).unwrap();
        }
    }
}
