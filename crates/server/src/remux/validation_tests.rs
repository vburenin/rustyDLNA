use super::tests::{job_spec, temp_dir, test_app, wait_for_terminal_cleanup};
use super::*;
use rusty_dlna_http::RemuxOutputExpectation;

fn expected(seconds: f64) -> RemuxOutputExpectation {
    RemuxOutputExpectation {
        video_codec: Some("h264".into()),
        audio_codecs: vec!["aac".into()],
        duration_seconds: Some(seconds),
        seek_seconds: 0.0,
        video_copy: false,
    }
}

pub(super) fn generate(path: &Path, seconds: &str, video: &str, audio: bool) {
    generate_with_audio(path, seconds, video, audio.then_some("aac"));
}

fn generate_with_audio(path: &Path, seconds: &str, video: &str, audio: Option<&str>) {
    generate_with_durations(path, seconds, video, audio, None);
}

fn generate_with_durations(
    path: &Path,
    seconds: &str,
    video: &str,
    audio: Option<&str>,
    durations: Option<(&str, &str)>,
) {
    use rusty_dlna_helper::{SupervisedCommand, SupervisedOutcome};
    let mut command = std::process::Command::new("ffmpeg");
    let video_source = format!(
        "testsrc2=size=64x64:rate=24{}",
        durations
            .map(|(video, _)| format!(":duration={video}"))
            .unwrap_or_default()
    );
    let audio_source = format!(
        "sine=frequency=440:sample_rate=48000{}",
        durations
            .map(|(_, audio)| format!(":duration={audio}"))
            .unwrap_or_default()
    );
    command.args([
        "-nostdin",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        &video_source,
    ]);
    if audio.is_some() {
        command.args(["-f", "lavfi", "-i", &audio_source]);
    }
    command.args([
        "-t",
        seconds,
        "-c:v",
        video,
        "-threads",
        "1",
        "-g",
        "24",
        "-c:a",
        audio.unwrap_or("aac"),
    ]);
    if video == "libx265" {
        command.args([
            "-x265-params",
            "pools=none:frame-threads=1:log-level=error",
            "-pix_fmt",
            "yuv420p10le",
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
            "-colorspace",
            "bt2020nc",
            "-tag:v",
            "hvc1",
        ]);
    }
    if video == "libaom-av1" {
        command.args(["-cpu-used", "8"]);
    }
    if audio == Some("dca") {
        command.args(["-strict", "-2"]);
    }
    command
        .args([
            "-movflags",
            if matches!(audio, Some("ac3" | "eac3")) {
                "+frag_keyframe+empty_moov+default_base_moof+delay_moov"
            } else {
                "+frag_keyframe+empty_moov+default_base_moof"
            },
            "-f",
            "mp4",
            "-y",
        ])
        .arg(path);
    let result = SupervisedCommand::new(&mut command)
        .capture_stderr(rusty_dlna_helper::CaptureConfig::new(
            65536,
            rusty_dlna_helper::CaptureRetention::Tail,
        ))
        .run_until(Instant::now() + Duration::from_secs(20), POLL, || {
            std::ops::ControlFlow::<()>::Continue(())
        })
        .unwrap();
    match result {
        SupervisedOutcome::Exited(output) => assert!(
            output.status.success(),
            "fixture generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        other => panic!("fixture generation failed: {other:?}"),
    }
}

fn top_boxes(bytes: &[u8]) -> Vec<(usize, usize, [u8; 4])> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = bytes[offset + 4..offset + 8].try_into().unwrap();
        result.push((offset, size, kind));
        offset += size;
    }
    result
}

fn publish(
    dir: &Path,
    key: &str,
    bytes: &[u8],
    expectation: RemuxOutputExpectation,
    hook: Option<PublicationTestHook>,
) -> (Arc<App>, Arc<RemuxJob>) {
    let app = test_app(dir, 1);
    let fixture = dir.join(format!("{key}-fixture.mp4"));
    std::fs::write(&fixture, bytes).unwrap();
    let mut spec = job_spec(dir, key, Vec::new());
    spec.output_expectation = Some(expectation);
    spec.args = vec![
        "cp".into(),
        fixture.into_os_string(),
        cache_part(&spec.dest).into_os_string(),
    ];
    if let Some(hook) = hook {
        crate::lock_recover(publication_test_hooks()).insert(cache_part(&spec.dest), hook);
    }
    let job = attach(app.clone(), spec).unwrap();
    wait_for_terminal_cleanup(&app, &job);
    (app, job)
}

fn assert_rejected(
    dir: &Path,
    key: &str,
    bytes: &[u8],
    expectation: RemuxOutputExpectation,
    hook: Option<PublicationTestHook>,
) {
    let (_app, job) = publish(dir, key, bytes, expectation, hook);
    assert!(
        matches!(job.state(), RemuxState::Failed(_) | RemuxState::Cancelled),
        "{key}: {:?}",
        job.state()
    );
    assert!(!job.dest.exists(), "{key} published final bytes");
    assert!(
        !rusty_dlna_transcode::cache_stamp_path(&job.dest).exists(),
        "{key} published a reusable stamp"
    );
}

#[test]
fn profile8_decodable_fragments_do_not_prove_source_timing_coverage() {
    let dir = temp_dir("profile8-retimed-fragments");
    let path = dir.join("shortened.mp4");
    // Model the measured raw-HEVC timestamp loss with generated media: the
    // video was rebuilt to CFR while a short original audio track remained.
    // This tests the existing completed validator, independently of dovi_tool.
    generate_with_durations(&path, "3", "libx264", Some("aac"), Some(("3", "0.4")));
    let file = std::fs::File::open(&path).unwrap();
    let mut index = hls::Index::default();
    index.update_file(&file, false).unwrap();
    assert!(
        index.has_playable_segment(),
        "a prefix can be playable despite lost source timing"
    );
    let error = hls::validate_finished(
        &file,
        &expected(4.0),
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(error.contains("output tracks cover"), "{error}");
    assert_rejected(
        &dir,
        "retimed",
        &std::fs::read(&path).unwrap(),
        expected(4.0),
        None,
    );
}

#[test]
fn completed_publication_validates_every_track_extent_and_timeline() {
    let dir = temp_dir("completed-validation");
    let fixture = dir.join("valid.mp4");
    generate(&fixture, "4", "libx264", true);
    let bytes = std::fs::read(&fixture).unwrap();
    assert_rejected(&dir, "invalid-mp4", b"not an mp4", expected(4.0), None);
    let (_app, valid) = publish(&dir, "valid-output", &bytes, expected(4.0), None);
    assert_eq!(valid.state(), RemuxState::Complete);
    assert!(cache_is_fresh_for_key(&valid.dest, "valid-output"));
    let top = top_boxes(&bytes);
    let last_mdat = top.iter().rev().find(|entry| &entry.2 == b"mdat").unwrap();
    assert_rejected(
        &dir,
        "truncated-mdat",
        &bytes[..last_mdat.0 + last_mdat.1 - 12],
        expected(4.0),
        None,
    );
    let first_moof = top.iter().find(|entry| &entry.2 == b"moof").unwrap().0;
    assert_rejected(&dir, "init-only", &bytes[..first_moof], expected(4.0), None);
    let last_moof = top
        .iter()
        .rev()
        .find(|entry| &entry.2 == b"moof")
        .unwrap()
        .0;
    assert_rejected(
        &dir,
        "missing-tail",
        &bytes[..last_moof],
        expected(4.0),
        None,
    );
    let mut too_many = bytes.clone();
    let trun = bytes.windows(4).position(|kind| kind == b"trun").unwrap();
    too_many[trun + 8..trun + 12].copy_from_slice(&32_000_001_u32.to_be_bytes());
    assert_rejected(&dir, "sample-budget", &too_many, expected(4.0), None);
    let mut no_sps = bytes.clone();
    let avcc = bytes.windows(4).position(|kind| kind == b"avcC").unwrap();
    no_sps[avcc + 9] = 0;
    assert_rejected(&dir, "missing-codec-init", &no_sps, expected(4.0), None);
    for kind in [b"mvhd", b"tkhd", b"mdhd"] {
        let mut invalid_version = bytes.clone();
        let index = bytes.windows(4).position(|entry| entry == kind).unwrap();
        invalid_version[index + 4] = 2;
        assert_rejected(
            &dir,
            &format!("unsupported-{}", String::from_utf8_lossy(kind)),
            &invalid_version,
            expected(4.0),
            None,
        );
    }
    let mvhd = bytes.windows(4).position(|kind| kind == b"mvhd").unwrap();
    let mut zero_timescale = bytes.clone();
    zero_timescale[mvhd + 16..mvhd + 20].fill(0);
    assert_rejected(
        &dir,
        "zero-movie-timescale",
        &zero_timescale,
        expected(4.0),
        None,
    );
    // Keep every parent boundary consistent while replacing mvhd with an empty box.
    let mvhd_size = u32::from_be_bytes(bytes[mvhd - 4..mvhd].try_into().unwrap()) as usize;
    let moov = top.iter().find(|entry| &entry.2 == b"moov").unwrap();
    let mut empty_header = bytes.clone();
    empty_header.drain(mvhd + 4..mvhd - 4 + mvhd_size);
    empty_header[mvhd - 4..mvhd].copy_from_slice(&8_u32.to_be_bytes());
    empty_header[moov.0..moov.0 + 4]
        .copy_from_slice(&((moov.1 - mvhd_size + 8) as u32).to_be_bytes());
    assert_rejected(
        &dir,
        "empty-movie-header",
        &empty_header,
        expected(4.0),
        None,
    );
    let mut wrong = expected(4.0);
    wrong.video_codec = Some("hevc".into());
    assert_rejected(&dir, "wrong-codec", &bytes, wrong, None);
    let mut missing = expected(4.0);
    missing.audio_codecs.push("aac".into());
    assert_rejected(&dir, "missing-track", &bytes, missing, None);
    // The second traf is audio: corrupt its data offset while the selected video
    // track and all enclosing box sizes remain valid.
    let mut extent = bytes.clone();
    let audio_trun = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, kind)| *kind == b"trun")
        .nth(1)
        .unwrap()
        .0;
    extent[audio_trun + 12..audio_trun + 16].copy_from_slice(&0_i32.to_be_bytes());
    assert_rejected(&dir, "audio-extent", &extent, expected(4.0), None);
    let mut timeline = bytes.clone();
    let audio_tfdt = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, kind)| *kind == b"tfdt")
        .nth(1)
        .unwrap()
        .0;
    timeline[audio_tfdt + 8..audio_tfdt + 16].copy_from_slice(&480_000_u64.to_be_bytes());
    assert_rejected(&dir, "audio-timeline", &timeline, expected(4.0), None);
}

#[test]
fn completed_publication_rejects_mutation_replacement_cancellation_and_expired_budget() {
    let dir = temp_dir("completed-publication-races");
    let fixture = dir.join("valid.mp4");
    generate(&fixture, "0.2", "libx264", true);
    let bytes = std::fs::read(&fixture).unwrap();
    assert_rejected(
        &dir,
        "in-place-mutation",
        &bytes,
        expected(0.2),
        Some(|job| {
            use std::io::{Seek, SeekFrom, Write};
            let modified = std::fs::metadata(&job.part).unwrap().modified().unwrap();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&job.part)
                .unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(b"evil").unwrap();
            file.set_modified(modified).unwrap();
        }),
    );
    assert_rejected(
        &dir,
        "pathname-replacement",
        &bytes,
        expected(0.2),
        Some(|job| {
            let replacement = job.part.with_extension("replacement");
            std::fs::write(&replacement, std::fs::read(&job.part).unwrap()).unwrap();
            std::fs::rename(replacement, &job.part).unwrap();
        }),
    );
    let (_app, failed_stamp) = publish(
        &dir,
        "stamp-failure",
        &bytes,
        expected(0.2),
        Some(|job| {
            std::fs::create_dir(rusty_dlna_transcode::cache_stamp_path(&job.dest)).unwrap();
        }),
    );
    assert!(matches!(failed_stamp.state(), RemuxState::Failed(_)));
    assert!(!failed_stamp.dest.exists());
    assert!(!cache_is_fresh_for_key(&failed_stamp.dest, "stamp-failure"));
    assert_rejected(
        &dir,
        "cancelled",
        &bytes,
        expected(0.2),
        Some(RemuxJob::cancel),
    );
    let app = test_app(&dir, 1);
    let mut job = super::tests::growing_test_job(&dir, 123, &bytes);
    Arc::get_mut(&mut job).unwrap().started =
        Instant::now() - Duration::from_secs(app.cfg.transcode.max_runtime_secs + 1);
    let spec = job_spec(&dir, "expired", Vec::new());
    finalize_remux(
        &app,
        &job,
        &spec,
        Duration::from_secs(10),
        &Some(expected(0.2)),
        true,
    );
    assert!(matches!(job.state(), RemuxState::Failed(_)));
    assert!(!job.dest.exists());
}

#[test]
fn helper_signal_after_valid_output_never_publishes_and_releases_admission() {
    let dir = temp_dir("crashed-valid-output");
    let fixture = dir.join("valid.mp4");
    generate(&fixture, "0.2", "libx264", true);
    let app = test_app(&dir, 1);
    let pid_file = dir.join("helper.pid");
    let mut spec = job_spec(&dir, "crashed", Vec::new());
    spec.output_expectation = Some(expected(0.2));
    spec.args = vec![
        "sh".into(),
        "-c".into(),
        "ulimit -c 0; printf '%s' $$ > \"$3\"; cp \"$1\" \"$2\"; kill -SEGV $$".into(),
        "crash-after-output".into(),
        fixture.clone().into_os_string(),
        cache_part(&spec.dest).into_os_string(),
        pid_file.clone().into_os_string(),
    ];
    let job = attach(app.clone(), spec).unwrap();
    wait_for_terminal_cleanup(&app, &job);
    let RemuxState::Failed(error) = job.state() else {
        panic!("crashed helper did not fail the job: {:?}", job.state());
    };
    assert!(error.contains("signal: 11"), "{error}");
    assert!(!job.dest.exists());
    assert!(!job.part.exists());
    assert!(!rusty_dlna_transcode::cache_stamp_path(&job.dest).exists());
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    assert!(
        !Path::new("/proc").join(pid.trim()).exists(),
        "helper leader was not reaped"
    );
    assert_eq!(app.helpers.metrics().active, 0);

    // Successful work on the same app proves a helper crash did not exhaust
    // server admission or turn valid structural output into a reusable failure.
    let mut next = job_spec(&dir, "after-crash", Vec::new());
    next.output_expectation = Some(expected(0.2));
    next.args = vec![
        "cp".into(),
        fixture.into_os_string(),
        cache_part(&next.dest).into_os_string(),
    ];
    let completed = attach(app.clone(), next).unwrap();
    wait_for_terminal_cleanup(&app, &completed);
    assert_eq!(completed.state(), RemuxState::Complete);
    assert!(cache_is_fresh_for_key(&completed.dest, "after-crash"));
}

#[test]
fn completed_publication_accepts_short_media_hdr_and_mixed_copy_encode() {
    let dir = temp_dir("completed-valid-media");
    for (name, seconds, codec, audio) in [
        ("short-av", "0.2", "libx264", true),
        ("hdr", "1", "libx265", true),
        ("video-only", "0.2", "libx264", false),
    ] {
        let fixture = dir.join(format!("{name}.mp4"));
        generate(&fixture, seconds, codec, audio);
        let mut contract = expected(seconds.parse().unwrap());
        if codec == "libx265" {
            contract.video_codec = Some("hevc".into());
        }
        if !audio {
            contract.audio_codecs.clear();
        }
        let (_app, job) = publish(&dir, name, &std::fs::read(fixture).unwrap(), contract, None);
        assert_eq!(job.state(), RemuxState::Complete, "{name}");
    }
    for (name, encoder, codec) in [
        ("mp3", "libmp3lame", "mp3"),
        ("ac3", "ac3", "ac3"),
        ("eac3", "eac3", "eac3"),
        ("dts", "dca", "dts"),
    ] {
        let fixture = dir.join(format!("{name}-source.mp4"));
        generate_with_audio(&fixture, "1", "libx264", Some(encoder));
        let mut contract = expected(1.0);
        contract.audio_codecs = vec![codec.into()];
        let (_app, job) = publish(&dir, name, &std::fs::read(fixture).unwrap(), contract, None);
        assert_eq!(job.state(), RemuxState::Complete, "{name}");
    }
    let source = dir.join("mixed-source.mp4");
    generate(&source, "2", "libx264", true);
    for (name, video, audio) in [
        ("copy-video", "copy", "aac"),
        ("copy-audio", "libx264", "copy"),
        ("audio-only", "none", "copy"),
    ] {
        let app = test_app(&dir, 1);
        let mut spec = job_spec(&dir, name, Vec::new());
        let mut contract = expected(2.0);
        contract.video_copy = video == "copy";
        if video == "none" {
            contract.video_codec = None;
            contract.duration_seconds = Some(0.2);
        }
        spec.output_expectation = Some(contract);
        let source_file = std::fs::File::open(&source).unwrap();
        let identity = rusty_dlna_transcode::transcode_cache_identity_file_controlled(
            &source_file,
            &source,
            &TranscodePlan::default(),
            false,
            rusty_dlna_transcode::ToolQueryControl::new(
                &app.helpers,
                &app.scan_cfg.cancellation,
                Duration::from_secs(5),
            ),
        )
        .unwrap()
        .unwrap();
        spec.verified_ffmpeg = Some(identity.ffmpeg().clone());
        spec.source_file = Some(Arc::new(source_file));
        spec.args = ["ffmpeg", "-nostdin", "-v", "error", "-i"]
            .into_iter()
            .map(Into::into)
            .collect();
        spec.args.push(source.as_os_str().to_owned());
        if video == "none" {
            spec.args.extend(["-vn".into(), "-t".into(), "0.2".into()]);
        } else {
            spec.args.extend(["-c:v".into(), video.into()]);
        }
        spec.args.extend([
            "-c:a".into(),
            audio.into(),
            "-threads".into(),
            "1".into(),
            "-movflags".into(),
            "+frag_keyframe+empty_moov+default_base_moof".into(),
            "-f".into(),
            "mp4".into(),
            "-y".into(),
            cache_part(&spec.dest).into_os_string(),
        ]);
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete, "{name}");
    }
}

#[test]
fn completed_validation_work_is_bounded_and_measured_without_reading_payloads() {
    let dir = temp_dir("validation-cost");
    let fixture = dir.join("cost.mp4");
    generate(&fixture, "60", "libx264", true);
    let file = std::fs::File::open(&fixture).unwrap();
    let started = Instant::now();
    let stats = hls::validate_finished(
        &file,
        &expected(60.0),
        started + Duration::from_secs(2),
        &AtomicBool::new(false),
    )
    .unwrap();
    eprintln!(
        "60s completed validation: {:?}, {} metadata bytes, {} samples, {} boxes, {} total bytes",
        started.elapsed(),
        stats.metadata_bytes,
        stats.samples,
        stats.boxes,
        file.metadata().unwrap().len()
    );
    assert!(stats.metadata_bytes < file.metadata().unwrap().len() / 2);
    assert!(hls::validate_finished(
        &file,
        &expected(60.0),
        Instant::now(),
        &AtomicBool::new(false)
    )
    .is_err());
    assert!(hls::validate_finished(
        &file,
        &expected(60.0),
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(true)
    )
    .is_err());
}

#[test]
fn completed_publication_accepts_real_browser_mixed_seek_recipes() {
    use rusty_dlna_transcode::{
        AudioAction, AudioCodec, BrowserEncodingPreset, Decision, HdrKind, VideoCodec,
    };
    let dir = temp_dir("validated-mixed-seek");
    let source = dir.join("source.mp4");
    generate(&source, "8", "libx264", true);
    for (name, video, audio) in [
        ("seek-copy-video", "copy", AudioAction::ToAac),
        ("seek-copy-audio", "libx264", AudioAction::Copy),
    ] {
        let app = test_app(&dir, 1);
        let source_file = std::fs::File::open(&source).unwrap();
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::Browser,
            video_encoder: video.into(),
            audio,
            container: "mp4",
            ..TranscodePlan::default()
        };
        let identity = rusty_dlna_transcode::transcode_cache_identity_file_controlled(
            &source_file,
            &source,
            &plan,
            false,
            rusty_dlna_transcode::ToolQueryControl::new(
                &app.helpers,
                &app.scan_cfg.cancellation,
                Duration::from_secs(5),
            ),
        )
        .unwrap()
        .unwrap();
        let mut spec = job_spec(&dir, name, Vec::new());
        spec.source_file = Some(Arc::new(source_file));
        spec.verified_ffmpeg = Some(identity.ffmpeg().clone());
        let mut contract = expected(8.0);
        contract.seek_seconds = 3.0;
        contract.video_copy = video == "copy";
        spec.output_expectation = Some(contract);
        spec.args = rusty_dlna_transcode::browser_ffmpeg_os_args_for_verified_ffmpeg(
            &source,
            &cache_part(&spec.dest),
            &plan,
            BrowserOutputOptions {
                encoding_preset: BrowserEncodingPreset::Balanced,
                source_video: Some(VideoCodec::H264),
                selected_audio: AudioCodec::Aac,
                source_hdr: HdrKind::Sdr,
                start_seconds: 3,
                hls: true,
            },
            identity.ffmpeg(),
        );
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete, "{name}");
    }
}

#[test]
fn completed_publication_preserves_unequal_source_track_durations() {
    let dir = temp_dir("unequal-source-track-durations");
    for (name, video, audio) in [("short-audio", "4", "0.4"), ("short-video", "0.4", "4")] {
        let fixture = dir.join(format!("{name}.mp4"));
        generate_with_durations(&fixture, "4", "libx264", Some("aac"), Some((video, audio)));
        let (_app, job) = publish(
            &dir,
            name,
            &std::fs::read(fixture).unwrap(),
            expected(4.0),
            None,
        );
        assert_eq!(job.state(), RemuxState::Complete, "{name}");
    }
    // The real Dolby Vision fixture deliberately has 10.803s video and 0.4s
    // TrueHD audio. Exercise its copy-video/encode-audio producer and publication.
    use rusty_dlna_transcode::{
        AudioAction, AudioCodec, BrowserEncodingPreset, HdrKind, VideoCodec,
    };
    let fixture = dir.join("dvp7.mkv");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/library/video/dvp7.mkv"),
        &fixture,
    )
    .unwrap();
    let app = test_app(&dir, 1);
    let file = std::fs::File::open(&fixture).unwrap();
    let plan = TranscodePlan {
        action: RecodeAction::Browser,
        video_encoder: "copy".into(),
        audio: AudioAction::ToAac,
        container: "mp4",
        ..TranscodePlan::default()
    };
    let identity = rusty_dlna_transcode::transcode_cache_identity_file_controlled(
        &file,
        &fixture,
        &plan,
        false,
        rusty_dlna_transcode::ToolQueryControl::new(
            &app.helpers,
            &app.scan_cfg.cancellation,
            Duration::from_secs(5),
        ),
    )
    .unwrap()
    .unwrap();
    let mut spec = job_spec(&dir, "unequal-dvp7", Vec::new());
    spec.source_file = Some(Arc::new(file));
    spec.verified_ffmpeg = Some(identity.ffmpeg().clone());
    let mut expectation = expected(10.803);
    expectation.video_codec = Some("hevc".into());
    expectation.video_copy = true;
    spec.output_expectation = Some(expectation);
    spec.args = rusty_dlna_transcode::browser_ffmpeg_os_args_for_verified_ffmpeg(
        &fixture,
        &cache_part(&spec.dest),
        &plan,
        BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::Hevc),
            selected_audio: AudioCodec::TrueHd,
            source_hdr: HdrKind::DolbyVisionProfile7,
            start_seconds: 0,
            hls: false,
        },
        identity.ffmpeg(),
    );
    let job = attach(app.clone(), spec).unwrap();
    wait_for_terminal_cleanup(&app, &job);
    assert_eq!(job.state(), RemuxState::Complete);
    assert!(cache_is_fresh_for_key(&job.dest, "unequal-dvp7"));
}

#[test]
fn finished_reads_preserve_validation_reuse_etag_and_cache_recency() {
    use tokio::io::AsyncReadExt;
    let dir = temp_dir("finished-cache-read-reuse");
    let fixture = dir.join("fixture.mp4");
    generate(&fixture, "0.2", "libx264", true);
    let bytes = std::fs::read(&fixture).unwrap();
    let app = test_app(&dir, 1);
    let key = "c".repeat(64);
    let mut spec = job_spec(&dir, &key, Vec::new());
    spec.dest = rusty_dlna_transcode::cache_dest_for_key(&dir, 42, RecodeAction::Hdr10, &key);
    spec.output_expectation = Some(expected(0.2));
    spec.args = vec![
        "cp".into(),
        fixture.into_os_string(),
        cache_part(&spec.dest).into_os_string(),
    ];
    let job = attach(app.clone(), spec.clone()).unwrap();
    wait_for_terminal_cleanup(&app, &job);
    assert_eq!(job.state(), RemuxState::Complete);
    let original = OutputSnapshot::read(&job.dest.metadata().unwrap());
    let stamp_path = rusty_dlna_transcode::cache_stamp_path(&job.dest);
    let stamp_contents = std::fs::read(&stamp_path).unwrap();
    std::fs::File::open(&stamp_path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(10))
        .unwrap();
    let etag_before = rusty_dlna_http::range::completed_cache_etag(
        &job.dest.metadata().unwrap(),
        &stamp_path.metadata().unwrap(),
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for range in [false, true] {
        let extra = if range {
            format!("Range: bytes=8-23\r\nIf-Range: {etag_before}\r\n")
        } else {
            String::new()
        };
        let request = HttpRequest::parse_headers(&format!(
            "GET /Transcode/42.mp4 HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}\r\n"
        ))
        .unwrap();
        let wire = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server_app = app.clone();
            let served = job.clone();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                serve_finished(
                    &server_app,
                    &mut socket,
                    &request,
                    &served,
                    "video/mp4",
                    false,
                )
                .await
                .unwrap();
            });
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            let mut wire = Vec::new();
            client.read_to_end(&mut wire).await.unwrap();
            server.await.unwrap();
            wire
        });
        let boundary = wire
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        let header = std::str::from_utf8(&wire[..boundary]).unwrap();
        assert!(header.starts_with(if range {
            "HTTP/1.1 206"
        } else {
            "HTTP/1.1 200"
        }));
        assert!(header.contains(&format!("ETag: {etag_before}\r\n")));
        assert_eq!(
            &wire[boundary..],
            if range {
                &bytes[8..24]
            } else {
                bytes.as_slice()
            }
        );
        assert_eq!(
            OutputSnapshot::read(&job.dest.metadata().unwrap()),
            original
        );
        assert_eq!(std::fs::read(&stamp_path).unwrap(), stamp_contents);
        assert!(cache_is_fresh_for_key(&job.dest, &key));
    }
    let recent = stamp_path.metadata().unwrap().modified().unwrap();
    assert!(recent > std::time::UNIX_EPOCH + Duration::from_secs(10));
    let cached = attach(app.clone(), spec).unwrap();
    assert!(
        cached.cache_hit,
        "a completed GET must not turn the next attachment into a rebuild"
    );
    assert_eq!(cached.state(), RemuxState::Complete);
    assert_eq!(runtime_status(&app).cache_hits_total, 1);
    // Age only the recency stamp: untouched media must first survive and then
    // expire through the same cache-maintenance path after its last use ages out.
    maintain_transcode_cache(&dir, u64::MAX, 1, 0, &HashSet::new(), false).unwrap();
    assert!(job.dest.exists());
    std::fs::File::open(&stamp_path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    maintain_transcode_cache(&dir, u64::MAX, 1, 0, &HashSet::new(), false).unwrap();
    assert!(!job.dest.exists());
    assert!(!stamp_path.exists());
}

#[test]
fn completed_publication_accepts_the_download_audio_track_limit() {
    let dir = temp_dir("download-track-validation-limit");
    let fixture = dir.join("source.mp4");
    generate(&fixture, "0.2", "libx264", true);
    for count in [32, 33] {
        let app = test_app(&dir, 1);
        let source = std::fs::File::open(&fixture).unwrap();
        let identity = rusty_dlna_transcode::transcode_cache_identity_file_controlled(
            &source,
            &fixture,
            &TranscodePlan::default(),
            false,
            rusty_dlna_transcode::ToolQueryControl::new(
                &app.helpers,
                &app.scan_cfg.cancellation,
                Duration::from_secs(5),
            ),
        )
        .unwrap()
        .unwrap();
        let mut spec = job_spec(&dir, &format!("download-{count}-audio"), Vec::new());
        let mut contract = expected(0.2);
        contract.audio_codecs = vec!["aac".into(); count];
        contract.video_copy = true;
        spec.output_expectation = Some(contract);
        spec.source_file = Some(Arc::new(source));
        spec.verified_ffmpeg = Some(identity.ffmpeg().clone());
        spec.args = vec![
            "ffmpeg".into(),
            "-nostdin".into(),
            "-v".into(),
            "error".into(),
            "-i".into(),
            fixture.as_os_str().to_owned(),
            "-map".into(),
            "0:v:0".into(),
        ];
        for _ in 0..count {
            spec.args.extend(["-map".into(), "0:a:0".into()]);
        }
        spec.args.extend([
            "-c".into(),
            "copy".into(),
            "-movflags".into(),
            "+frag_keyframe+empty_moov+delay_moov+default_base_moof".into(),
            "-f".into(),
            "mp4".into(),
            "-y".into(),
            cache_part(&spec.dest).into_os_string(),
        ]);
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        if count == 32 {
            assert_eq!(
                job.state(),
                RemuxState::Complete,
                "the supported 32-audio-plus-video output must publish"
            );
        } else {
            assert!(matches!(job.state(), RemuxState::Failed(_)));
            assert!(!job.dest.exists());
        }
    }
}

#[test]
fn completed_publication_preserves_dlna_chapters_and_copy_video_codecs() {
    use rusty_dlna_helper::{SupervisedCommand, SupervisedOutcome};
    use rusty_dlna_transcode::{AudioAction, Decision};
    let dir = temp_dir("dlna-chapter-codec-validation");
    for (name, encoder, codec) in [
        ("chapters", "libx264", "h264"),
        ("vp9", "libvpx-vp9", "vp9"),
        ("av1", "libaom-av1", "av1"),
        ("mpeg2", "mpeg2video", "mpeg2"),
    ] {
        let fixture = dir.join(format!("{name}-source.mp4"));
        generate(&fixture, "0.4", encoder, true);
        let source_path = if name == "chapters" {
            let metadata = dir.join("chapters.ffmetadata");
            std::fs::write(
                &metadata,
                ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=100\nEND=400\ntitle=Opening\n",
            )
            .unwrap();
            let source = dir.join("chaptered-source.mp4");
            let mut command = std::process::Command::new("ffmpeg");
            command
                .args(["-nostdin", "-v", "error", "-i"])
                .arg(&fixture)
                .arg("-i")
                .arg(metadata)
                .args([
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a:0",
                    "-c",
                    "copy",
                    "-map_metadata",
                    "1",
                    "-map_chapters",
                    "1",
                    "-y",
                ])
                .arg(&source);
            let outcome = SupervisedCommand::new(&mut command)
                .capture_stderr(rusty_dlna_helper::CaptureConfig::new(
                    65536,
                    rusty_dlna_helper::CaptureRetention::Tail,
                ))
                .run_until(Instant::now() + Duration::from_secs(20), POLL, || {
                    std::ops::ControlFlow::<()>::Continue(())
                })
                .unwrap();
            match outcome {
                SupervisedOutcome::Exited(output) => assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                ),
                other => panic!("chapter fixture failed: {other:?}"),
            }
            source
        } else {
            fixture
        };
        let app = test_app(&dir, 1);
        let source = std::fs::File::open(&source_path).unwrap();
        let plan = TranscodePlan {
            decision: Decision::Recode,
            action: RecodeAction::AudioAc3,
            video_encoder: "copy".into(),
            audio: AudioAction::ToAc3,
            ..TranscodePlan::default()
        };
        let identity = rusty_dlna_transcode::transcode_cache_identity_file_controlled(
            &source,
            &source_path,
            &plan,
            false,
            rusty_dlna_transcode::ToolQueryControl::new(
                &app.helpers,
                &app.scan_cfg.cancellation,
                Duration::from_secs(5),
            ),
        )
        .unwrap()
        .unwrap();
        let mut spec = job_spec(&dir, name, Vec::new());
        let mut contract = expected(0.4);
        contract.video_codec = Some(codec.into());
        contract.audio_codecs = vec!["ac3".into()];
        contract.video_copy = true;
        spec.output_expectation = Some(contract.clone());
        spec.source_file = Some(Arc::new(source));
        spec.verified_ffmpeg = Some(identity.ffmpeg().clone());
        spec.args =
            rusty_dlna_transcode::ffmpeg_grow_os_args(&source_path, &cache_part(&spec.dest), &plan);
        let job = attach(app.clone(), spec).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete, "{name}");
        let bytes = std::fs::read(&job.dest).unwrap();
        if name == "chapters" {
            let mut unreferenced = bytes.clone();
            let mut references = 0;
            for offset in 0..unreferenced.len() - 4 {
                if &unreferenced[offset..offset + 4] == b"chap" {
                    unreferenced[offset..offset + 4].copy_from_slice(b"free");
                    references += 1;
                }
            }
            assert!(references > 0, "the DLNA recipe must preserve chapters");
            assert_rejected(
                &dir,
                "unreferenced-text",
                &unreferenced,
                contract.clone(),
                None,
            );
            // The last traf is the referenced text track. Its samples receive
            // the same extent checks as AV samples, even though timing is sparse.
            let mut bad_extent = bytes.clone();
            let trun = bad_extent
                .windows(4)
                .rposition(|kind| kind == b"trun")
                .unwrap();
            bad_extent[trun + 12..trun + 16].copy_from_slice(&0_u32.to_be_bytes());
            assert_rejected(&dir, "chapter-extent", &bad_extent, contract, None);
        } else if matches!(name, "vp9" | "av1") {
            let mut bad_config = bytes;
            let kind = if name == "vp9" { b"vpcC" } else { b"av1C" };
            let offset = bad_config
                .windows(4)
                .position(|value| value == kind)
                .unwrap();
            bad_config[offset + 4] = 0;
            assert_rejected(
                &dir,
                &format!("{name}-bad-config"),
                &bad_config,
                contract,
                None,
            );
        }
    }
}

#[test]
fn effective_fallback_publication_reuses_only_current_stably_unsupported_recipe() {
    let dir = temp_dir("effective-fallback-publication");
    let fixture = dir.join("fallback-fixture.mp4");
    generate(&fixture, "2", "libx264", true);
    let expected_bytes = std::fs::read(&fixture).unwrap();
    for (label, diagnostic, should_reuse) in [
        ("unsupported", "Unknown encoder h264_nvenc", true),
        ("busy", "Resource temporarily unavailable", false),
        ("input", "Invalid data found when processing input", false),
        ("unknown", "Error initializing output stream", false),
    ] {
        let app = test_app(&dir, 1);
        let count = dir.join(format!("{label}-attempts"));
        let mut spec = job_spec(&dir, label, Vec::new());
        spec.output_expectation = Some(expected(2.0));
        spec.args = vec![
            "sh".into(),
            "-c".into(),
            "printf x >> \"$1\"; printf '%s' \"$2\" >&2; exit 1".into(),
            "primary".into(),
            count.as_os_str().to_owned(),
            diagnostic.into(),
        ];
        spec.fallback_args = Some(vec![
            "cp".into(),
            fixture.as_os_str().to_owned(),
            cache_part(&spec.dest).into_os_string(),
        ]);
        let job = attach(app.clone(), spec.clone()).unwrap();
        wait_for_terminal_cleanup(&app, &job);
        assert_eq!(job.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(&job.dest).unwrap(), expected_bytes);
        let actual = crate::lock_recover(&job.effective_recipe).clone().unwrap();
        assert!(cache_is_fresh_for_key(&job.dest, &actual.stamp_key()));
        assert!(!cache_is_fresh_for_key(&job.dest, &spec.cache_key));
        let reused = attach(app.clone(), spec.clone()).unwrap();
        wait_for_terminal_cleanup(&app, &reused);
        assert_eq!(reused.state(), RemuxState::Complete);
        assert_eq!(reused.cache_hit, should_reuse, "{label}");
        assert_eq!(
            std::fs::read(&count).unwrap().len(),
            if should_reuse { 1 } else { 2 }
        );
        if should_reuse {
            // Recency touches cannot renew the one-hour stable-failure preference.
            let old = std::time::SystemTime::now() - Duration::from_secs(3601);
            std::fs::File::open(&reused.dest)
                .unwrap()
                .set_modified(old)
                .unwrap();
            write_cache_stamp_for_key(&reused.dest, &actual.stamp_key()).unwrap();
            assert!(cache_is_fresh_for_key(&reused.dest, &actual.stamp_key()));
            let expired = attach(app.clone(), spec.clone()).unwrap();
            wait_for_terminal_cleanup(&app, &expired);
            assert!(!expired.cache_hit);
            assert_eq!(std::fs::read(&count).unwrap().len(), 2);
            let mut changed_recipe = spec.clone();
            changed_recipe.fallback_args.as_mut().unwrap()[0] = "/bin/cp".into();
            let changed = attach(app.clone(), changed_recipe).unwrap();
            wait_for_terminal_cleanup(&app, &changed);
            assert!(!changed.cache_hit);
            assert_eq!(std::fs::read(&count).unwrap().len(), 3);
        }
        // Removing the currently allowed fallback prevents old output reuse.
        let mut changed = spec.clone();
        changed.fallback_args = None;
        let failed = attach(app.clone(), changed).unwrap();
        wait_for_terminal_cleanup(&app, &failed);
        assert!(matches!(failed.state(), RemuxState::Failed(_)));
        assert!(!failed.dest.exists());
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn invalid_fallback_never_gets_an_effective_validation_stamp() {
    let dir = temp_dir("invalid-effective-fallback");
    let app = test_app(&dir, 1);
    let mut spec = job_spec(
        &dir,
        "invalid-effective-fallback",
        vec![
            "sh".into(),
            "-c".into(),
            "echo 'Unknown encoder' >&2; exit 1".into(),
        ],
    );
    spec.output_expectation = Some(expected(2.0));
    spec.fallback_args = Some(vec![
        "cp".into(),
        spec.src.as_os_str().to_owned(),
        cache_part(&spec.dest).into_os_string(),
    ]);
    let job = attach(app.clone(), spec).unwrap();
    wait_for_terminal_cleanup(&app, &job);
    assert!(matches!(job.state(), RemuxState::Failed(_)));
    assert!(!job.dest.exists());
    assert!(!rusty_dlna_transcode::cache_stamp_path(&job.dest).exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn actual_ffmpeg_fallback_is_validated_reused_and_disclosed_as_software_sdr() {
    use rusty_dlna_transcode::{
        AudioAction, BrowserEncodingPreset, BrowserQuality, HardwareDecode, HdrKind,
        ToolQueryControl, VideoCodec,
    };
    let dir = temp_dir("actual-ffmpeg-fallback");
    let fixture = dir.join("actual-input.mp4");
    generate(&fixture, "2", "libx264", true);
    let app = test_app(&dir, 1);
    let source = Arc::new(std::fs::File::open(&fixture).unwrap());
    let primary_plan = TranscodePlan {
        action: RecodeAction::Browser,
        // Deterministic FFmpeg unsupported-path failure; this exercises real
        // argv/executable/descriptor supervision without depending on a GPU.
        video_encoder: "hevc_rustydlna_unavailable_hardware_encoder".into(),
        audio: AudioAction::ToAac,
        browser_quality: Some(BrowserQuality::Auto),
        hardware_decode: HardwareDecode::None,
        ..TranscodePlan::default()
    };
    let options = BrowserOutputOptions {
        encoding_preset: BrowserEncodingPreset::Balanced,
        source_video: Some(VideoCodec::H264),
        selected_audio: rusty_dlna_transcode::AudioCodec::Aac,
        source_hdr: HdrKind::Sdr,
        start_seconds: 0,
        hls: false,
    };
    let identity = rusty_dlna_transcode::browser_transcode_cache_identity_file_controlled(
        &source,
        &fixture,
        &primary_plan,
        options,
        ToolQueryControl::new(
            &app.helpers,
            &app.scan_cfg.cancellation,
            Duration::from_secs(2),
        ),
    )
    .unwrap()
    .unwrap();
    let mut spec = job_spec(&dir, "actual-ffmpeg-fallback", Vec::new());
    spec.job_key = "web:42:actual-ffmpeg-fallback".into();
    spec.web_session_id = Some(9);
    spec.web_request_id = Some(77);
    spec.cache_key = identity.cache_key().to_owned();
    spec.output_expectation = Some(RemuxOutputExpectation {
        video_codec: Some("hevc".into()),
        ..expected(2.0)
    });
    spec.source_file = Some(source);
    spec.src = fixture;
    spec.verified_ffmpeg = Some(identity.ffmpeg().clone());
    let part = cache_part(&spec.dest);
    let args = |plan: &TranscodePlan| {
        rusty_dlna_transcode::browser_ffmpeg_os_args_for_verified_ffmpeg(
            Path::new("/proc/self/fd/3"),
            &part,
            plan,
            options,
            identity.ffmpeg(),
        )
    };
    spec.args = args(&primary_plan);
    let portable = TranscodePlan {
        video_encoder: "libx264".into(),
        ..primary_plan
    };
    spec.fallback_args = Some(args(&portable));
    let first = attach(app.clone(), spec.clone()).unwrap();
    wait_for_terminal_cleanup(&app, &first);
    assert_eq!(first.state(), RemuxState::Complete);
    let actual = crate::lock_recover(&first.effective_recipe)
        .clone()
        .unwrap();
    assert_eq!(actual.video_encoder, "libx264");
    assert_eq!(actual.audio_encoder, "aac");
    assert_eq!(actual.dynamic_range, "sdr");
    assert_eq!(
        actual.previous_failure,
        Some(fallback::FailureClass::Unsupported)
    );
    assert!(cache_is_fresh_for_key(&first.dest, &actual.stamp_key()));
    assert!(!cache_is_fresh_for_key(&first.dest, &spec.cache_key));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let readers = [(9, 77), (10, 88)]
        .into_iter()
        .map(|(session, request)| {
            let app = app.clone();
            let barrier = barrier.clone();
            let mut spec = spec.clone();
            spec.web_session_id = Some(session);
            spec.web_request_id = Some(request);
            std::thread::spawn(move || {
                barrier.wait();
                attach_for_client(app, spec).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let readers = readers
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert!(Arc::ptr_eq(&readers[0], &readers[1]));
    assert!(readers[0].cache_hit);
    assert!(
        crate::lock_recover(&readers[0].effective_recipe)
            .as_ref()
            .unwrap()
            .cache_reuse
    );
    assert!(web_job_effective_recipe(&app, 42, Some(9), Some(77)).is_some());
    assert!(web_job_effective_recipe(&app, 42, Some(10), Some(88)).is_some());
    assert!(web_job_effective_recipe(&app, 42, Some(9), Some(88)).is_none());
    assert!(web_job_effective_recipe(&app, 42, Some(11), Some(77)).is_none());
    assert_eq!(
        app.remux_metrics
            .performance
            .fallbacks_portable
            .load(Ordering::Relaxed),
        1
    );
    // A warm MSE attachment discloses the effective H.264/AAC recipe before
    // the client fetches or appends initialization bytes for requested HEVC.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let response = runtime.block_on(async {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = rusty_dlna_http::HttpRequest::parse_headers(
            "GET /web/media/42.m3u8?delivery=mse&session=9&request=77 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        ).unwrap();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_remux(&server_app, &mut socket, &request, spec).await.unwrap();
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response)).await.unwrap().unwrap();
        server.await.unwrap();
        String::from_utf8(response).unwrap()
    });
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.contains(&format!(
            "{}: h264_sdr\r\n",
            rusty_dlna_protocol::MSE_VIDEO_OUTPUT_HEADER
        )),
        "{response}"
    );
    assert!(
        response.contains(&format!(
            "{}: aac\r\n",
            rusty_dlna_protocol::MSE_AUDIO_CODEC_HEADER
        )),
        "{response}"
    );
    assert!(response.contains("#EXTM3U"));
    let _ = std::fs::remove_dir_all(dir);
}

fn release_failed_attempt_before_readiness_pin(job: &RemuxJob) {
    let directory = job.part.parent().unwrap();
    std::fs::write(directory.join("release-primary"), b"release").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while (job.state() != RemuxState::Starting || job.part.exists()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(job.state(), RemuxState::Starting);
    assert!(!job.part.exists());
    assert!(crate::lock_recover(&job.output).is_none());
    std::fs::write(directory.join("readiness-handoff"), b"observed").unwrap();
}

#[tokio::test]
async fn readiness_pin_retries_a_replaced_attempt_without_resetting_its_deadline() {
    for expires in [false, true] {
        let dir = temp_dir("readiness-fallback-handoff");
        let app = test_app(&dir, 1);
        let mut spec = job_spec(&dir, "readiness-fallback-handoff", Vec::new());
        spec.args = vec![
            "sh".into(),
            "-c".into(),
            "head -c 32768 /dev/zero > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; exit 1"
                .into(),
            "primary".into(),
            cache_part(&spec.dest).into_os_string(),
            dir.join("release-primary").into_os_string(),
        ];
        spec.fallback_args = Some(vec![
            "sh".into(),
            "-c".into(),
            "while [ ! -f \"$1\" ]; do sleep 0.01; done; cp \"$2\" \"$3\"".into(),
            "fallback".into(),
            dir.join("release-fallback").into_os_string(),
            spec.src.as_os_str().to_owned(),
            cache_part(&spec.dest).into_os_string(),
        ]);
        let job = attach(app.clone(), spec).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while job.state() != RemuxState::Growing {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        crate::lock_recover(readiness_test_hooks()).insert(
            job.part.clone(),
            release_failed_attempt_before_readiness_pin,
        );
        let deadline = Instant::now()
            + if expires {
                Duration::from_millis(250)
            } else {
                FIRST_WAIT
            };
        let waiting_job = job.clone();
        let mut ready = tokio::spawn(async move { wait_ready_until(&waiting_job, deadline).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !dir.join("readiness-handoff").is_file() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        if expires {
            let error = tokio::time::timeout(Duration::from_secs(1), ready)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.contains("remux produced no data"), "{error}");
            assert!(Instant::now() >= deadline);
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut ready)
                    .await
                    .is_err(),
                "replacement is pending, not a missing-output error"
            );
            std::fs::write(dir.join("release-fallback"), b"release").unwrap();
            let path = tokio::time::timeout(Duration::from_secs(3), ready)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(path, job.dest);
            assert!(
                crate::lock_recover(&job.output).is_some(),
                "readiness already pinned the replacement"
            );
        }
        std::fs::write(dir.join("release-fallback"), b"release").unwrap();
        let cleanup_app = app.clone();
        let cleanup_job = job.clone();
        tokio::task::spawn_blocking(move || wait_for_terminal_cleanup(&cleanup_app, &cleanup_job))
            .await
            .unwrap();
        assert_eq!(job.state(), RemuxState::Complete);
        assert_eq!(std::fs::read(&job.dest).unwrap(), b"source bytes");
        let _ = std::fs::remove_dir_all(dir);
    }
}
