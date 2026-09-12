use super::*;
use std::hint::black_box;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Deliberately metadata-only, one one-second random-access fragment per entry.
/// Decoder validation is covered separately by the optional media benchmark.
fn synthetic(seconds: usize) -> Vec<u8> {
    let fixture = tests::fixture();
    let first = fixture.windows(4).position(|name| name == b"moof").unwrap() - 4;
    let second = fixture
        .windows(4)
        .enumerate()
        .filter(|(_, name)| *name == b"moof")
        .nth(1)
        .unwrap()
        .0
        - 4;
    let fragment = &fixture[first..second];
    let tfdt = fragment
        .windows(4)
        .position(|name| name == b"tfdt")
        .unwrap()
        + 8;
    let mut bytes = fixture[..first].to_vec();
    for second in 0..seconds {
        let start = bytes.len();
        bytes.extend_from_slice(fragment);
        bytes[start + tfdt..start + tfdt + 4]
            .copy_from_slice(&(second as u32 * 1000).to_be_bytes());
    }
    bytes
}

#[test]
fn long_title_views_retain_history_timing_and_bound_mse_pages() {
    let dir = tests::TempDir::new("long-shapes");
    for seconds in [600, 7200, 28_800] {
        let path = dir.path().join(format!("{seconds}.mp4"));
        std::fs::write(&path, synthetic(seconds)).unwrap();
        let file = File::open(path).unwrap();
        let mut index = Index::default();
        index.update_file(&file, false).unwrap();
        let old = index.playlist_view(true).unwrap();
        index.update_file(&file, true).unwrap();
        assert_eq!(index.produced_duration_seconds(), Some(seconds as f64));
        assert_eq!(index.segment_time, seconds as f64);
        let old_text = old.render("init", "segment").unwrap();
        assert_eq!(old_text.matches("#EXTINF:").count(), seconds);
        assert!(!old_text.contains("#EXT-X-ENDLIST"));
        let text = index
            .playlist_view(true)
            .unwrap()
            .render("init", "segment")
            .unwrap();
        assert_eq!(text.matches("#EXTINF:").count(), seconds);
        assert!(text.contains("#EXT-X-TARGETDURATION:1\n"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
        for after in [0, seconds / 2, seconds - 1, seconds] {
            let page = index
                .mse_playlist_view(after)
                .unwrap()
                .render("init", "segment")
                .unwrap();
            assert_eq!(page.matches("#EXTINF:").count(), (seconds - after).min(256));
            assert!(page.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{after}\n")));
            if after < seconds {
                assert!(page.contains(&format!(
                    "#EXT-X-RUSTY-TIMING:{:.6},{:.6}\n",
                    after as f64, after as f64
                )));
            }
        }
    }
}

#[test]
fn native_target_is_frozen_and_later_larger_copied_gop_requires_restart() {
    let mut index = Index {
        init_end: Some(32),
        ..Index::default()
    };
    let mut offset = 32;
    for (seconds, random_access) in [(2.1, true), (0.2, false), (1.0, true)] {
        index
            .push_fragment(
                Fragment {
                    offset,
                    duration: seconds,
                    random_access,
                },
                offset + 16,
            )
            .unwrap();
        offset += 16;
    }
    let first = index.playlist_view(false).unwrap();
    assert!(first
        .render("init", "segment")
        .unwrap()
        .contains("#EXT-X-TARGETDURATION:2\n"));
    index
        .push_fragment(
            Fragment {
                offset,
                duration: 2.49,
                random_access: true,
            },
            offset + 16,
        )
        .unwrap();
    offset += 16;
    assert!(index
        .playlist_view(false)
        .unwrap()
        .render("init", "segment")
        .unwrap()
        .contains("#EXT-X-TARGETDURATION:2\n"));
    index
        .push_fragment(
            Fragment {
                offset,
                duration: 3.0,
                random_access: true,
            },
            offset + 16,
        )
        .unwrap();
    offset += 16;
    assert!(
        index.playlist_view(false).is_ok(),
        "2.49 rounds within the original target"
    );
    index
        .push_fragment(
            Fragment {
                offset,
                duration: 1.0,
                random_access: true,
            },
            offset + 16,
        )
        .unwrap();
    assert!(index
        .playlist_view(false)
        .unwrap_err()
        .contains("restart required"));
    assert!(first
        .render("init", "segment")
        .unwrap()
        .contains("#EXT-X-TARGETDURATION:2\n"));
    // A fresh generation with a complete index knows the entire GOP maximum.
    assert!(index
        .playlist_view_for(false, Some((2, 2)))
        .unwrap()
        .render("init", "segment")
        .unwrap()
        .contains("#EXT-X-TARGETDURATION:3\n"));
}

#[test]
fn native_generation_targets_are_bounded_and_scoped_to_request_owners() {
    let mut index = Index {
        init_end: Some(32),
        ..Index::default()
    };
    index.push_segment(Segment {
        offset: 32,
        length: 16,
        duration: 1.0,
    });
    index.playlist_view_for(false, Some((1, 10))).unwrap();
    index.push_segment(Segment {
        offset: 48,
        length: 16,
        duration: 3.0,
    });
    assert!(index.playlist_view_for(false, Some((1, 10))).is_err());
    index.finalized = true;
    let new = index.playlist_view_for(false, Some((1, 11))).unwrap();
    assert!(new
        .render("init", "segment")
        .unwrap()
        .contains("#EXT-X-TARGETDURATION:3\n"));
    assert!(index.playlist_view_for(false, Some((1, 10))).is_err());
    index.forget_generation(1, 10);
    assert_eq!(index.native_targets.len(), 1);
    for request in 0..(2 * (super::super::MAX_WEB_PLAYBACK_SESSIONS + 1)) as u64 {
        let result = index.playlist_view_for(false, Some((2, request)));
        if result.is_err() {
            assert_eq!(
                index.native_targets.len(),
                2 * (super::super::MAX_WEB_PLAYBACK_SESSIONS + 1)
            );
        }
    }
    assert!(index.playlist_view_for(false, Some((3, u64::MAX))).is_err());
}

#[test]
fn removing_one_session_preserves_another_sessions_same_numbered_generation() {
    let mut index = Index {
        init_end: Some(32),
        ..Index::default()
    };
    index.push_segment(Segment {
        offset: 32,
        length: 16,
        duration: 1.0,
    });
    index.playlist_view_for(false, Some((1, 10))).unwrap();
    index.playlist_view_for(false, Some((2, 10))).unwrap();
    index.push_segment(Segment {
        offset: 48,
        length: 16,
        duration: 3.0,
    });
    index.forget_generation(1, 10);
    assert!(index
        .playlist_view_for(false, Some((2, 10)))
        .unwrap_err()
        .contains("restart required"));
    assert!(index
        .playlist_view_for(false, Some((1, 11)))
        .unwrap()
        .render("init", "segment")
        .unwrap()
        .contains("#EXT-X-TARGETDURATION:3\n"));
}

#[tokio::test]
async fn same_completed_job_accepts_new_hls_generation_after_longer_copied_gop() {
    use super::super::tests::{growing_test_job, test_app};
    use super::super::{serve_fragment_playlist, RemuxJob, RemuxState};
    use tokio::io::AsyncReadExt;

    async fn serve(
        app: Arc<crate::App>,
        job: Arc<RemuxJob>,
        request_id: u64,
    ) -> (Result<(), String>, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = rusty_dlna_http::HttpRequest::parse_headers(&format!(
            "GET /web/media/42.m3u8?delivery=hls&session=1&request={request_id} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        )).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            serve_fragment_playlist(&app, &mut socket, &request, &job, false, false)
                .await
                .map_err(|error| error.to_string())
        });
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        (server.await.unwrap(), String::from_utf8(response).unwrap())
    }

    let dir = tests::TempDir::new("native-generation-wire");
    let app = test_app(dir.path(), 1);
    let mut bytes = synthetic(6);
    let runs = bytes
        .windows(4)
        .enumerate()
        .filter_map(|(offset, name)| (name == b"trun").then_some(offset))
        .collect::<Vec<_>>();
    // Random-access groups have durations2,3,1seconds, while movie fragments
    // retain one-second timings and the copied stream's decoder dependencies.
    for index in [1, 3, 4] {
        bytes[runs[index] + 12..runs[index] + 16].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    }
    let split = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, name)| *name == b"moof")
        .nth(3)
        .unwrap()
        .0
        - 4;
    let job = growing_test_job(dir.path(), 42, &bytes[..split]);
    let (result, text) = serve(app.clone(), job.clone(), 10).await;
    result.unwrap();
    assert!(text.contains("#EXT-X-TARGETDURATION:2\n"));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&job.part)
        .unwrap()
        .write_all(&bytes[split..])
        .unwrap();
    std::fs::rename(&job.part, &job.dest).unwrap();
    job.transition(RemuxState::Complete);
    assert!(serve(app.clone(), job.clone(), 10)
        .await
        .0
        .unwrap_err()
        .contains("restart required"));
    let (result, text) = serve(app.clone(), job.clone(), 11).await;
    result.unwrap();
    assert!(text.contains("#EXT-X-TARGETDURATION:3\n"));
    assert_eq!(text.matches("#EXTINF:").count(), 3);
    assert!(text.ends_with("#EXT-X-ENDLIST\n"));
    assert!(
        serve(app, job, 10).await.0.is_err(),
        "the old generation remains frozen"
    );
}

#[test]
fn manifest_output_budget_is_checked_before_formatting_unbounded_uris() {
    let dir = tests::TempDir::new("manifest-budget");
    let path = dir.path().join("stream.mp4");
    std::fs::write(&path, synthetic(600)).unwrap();
    let mut index = Index::default();
    index.update(&path, true).unwrap();
    let native = index.playlist_view(false).unwrap();
    assert!(native.render("init", &"x".repeat(200)).is_ok());
    assert!(native
        .render("init", &"x".repeat(65536))
        .unwrap_err()
        .starts_with("resource_limit:"));
    let mse = index.mse_playlist_view(0).unwrap();
    assert!(mse
        .render("init", &"x".repeat(65536))
        .unwrap_err()
        .starts_with("resource_limit:"));
}

#[test]
fn completed_reuse_is_pinned_and_rejects_replacement_truncation_and_corruption() {
    let dir = tests::TempDir::new("completed-reuse");
    let path = dir.path().join("stream.mp4");
    let old_bytes = synthetic(600);
    std::fs::write(&path, &old_bytes).unwrap();
    let pinned = File::open(&path).unwrap();
    let mut first = Index::default();
    first.update_file(&pinned, true).unwrap();
    let mut warm = Index::default();
    warm.update_file(&pinned, true).unwrap();
    assert_eq!(
        first.playlist("init", "segment").unwrap(),
        warm.playlist("init", "segment").unwrap()
    );
    let replacement = dir.path().join("replacement.mp4");
    std::fs::write(&replacement, synthetic(720)).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    // Unlink/rename changes ctime without changing the pinned output bytes.
    first.update_file(&pinned, true).unwrap();
    // A fresh attachment reparses if strict cache identity no longer matches.
    let mut old = Index::default();
    old.update_file(&pinned, true).unwrap();
    assert_eq!(old.produced_duration_seconds(), Some(600.0));
    let mut newer = Index::default();
    newer.update(&path, true).unwrap();
    assert_eq!(newer.produced_duration_seconds(), Some(720.0));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(32)
        .unwrap();
    assert!(newer.update(&path, true).unwrap_err().contains("changed"));
    assert!(newer.update(&path, true).is_err());
    let mut bad = synthetic(3);
    bad.pop();
    std::fs::write(&path, &bad).unwrap();
    for _ in 0..2 {
        assert!(Index::default()
            .update(&path, true)
            .unwrap_err()
            .contains("ends inside"));
    }
}

#[test]
fn growing_index_keeps_partial_tail_and_rejects_truncation_without_resetting_generation() {
    let dir = tests::TempDir::new("incomplete-tail");
    let path = dir.path().join("stream.part");
    let bytes = synthetic(3);
    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    let mut index = Index::default();
    index.update(&path, false).unwrap();
    assert_eq!(index.produced_duration_seconds(), Some(2.0));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&bytes[bytes.len() - 1..])
        .unwrap();
    index.update(&path, false).unwrap();
    assert_eq!(index.produced_duration_seconds(), Some(3.0));
    std::fs::write(&path, &bytes[..64]).unwrap();
    assert!(index
        .update(&path, false)
        .unwrap_err()
        .contains("truncated"));
    assert_eq!(index.produced_duration_seconds(), Some(3.0));
}

#[test]
fn parser_rejects_metadata_and_fragment_budget_exhaustion() {
    let dir = tests::TempDir::new("index-budget");
    let path = dir.path().join("stream.mp4");
    std::fs::write(&path, synthetic(1)).unwrap();
    for index in [
        Index {
            scanned_boxes: MAX_INDEX_BOXES,
            ..Index::default()
        },
        Index {
            metadata_bytes: MAX_INDEX_METADATA_BYTES,
            ..Index::default()
        },
        Index {
            indexed_samples: MAX_INDEX_SAMPLES,
            ..Index::default()
        },
    ] {
        let mut index = index;
        assert!(index.update(&path, true).unwrap_err().contains("budget"));
    }
    let mut index = Index {
        fragments: (0..MAX_INDEX_FRAGMENTS)
            .map(|_| Segment {
                offset: 32,
                length: 16,
                duration: 1.0,
            })
            .collect(),
        ..Index::default()
    };
    assert!(index
        .push_fragment(
            Fragment {
                offset: 32,
                duration: 1.0,
                random_access: true
            },
            48
        )
        .unwrap_err()
        .contains("fragment budget"));
}

fn summary(samples: &mut [f64]) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    (
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1],
    )
}

fn measure(path: &Path, label: &str, seconds: usize) {
    const SAMPLES: usize = 9;
    let file = Arc::new(File::open(path).unwrap());
    let uri = "x".repeat(200);
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    let mut render = Vec::new();
    let mut view_time = Vec::new();
    let mut bytes = 0;
    let mut memory = 0;
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let mut index = Index::default();
        index.update_file(&file, false).unwrap();
        cold.push(started.elapsed().as_secs_f64() * 1000.0);
        index.update_file(&file, true).unwrap();
        memory = index.retained_bytes();
        let started = Instant::now();
        let mut reopened = Index::default();
        reopened.update_file(&file, true).unwrap();
        warm.push(started.elapsed().as_secs_f64() * 1000.0);
        let started = Instant::now();
        let view = reopened.playlist_view(false).unwrap();
        view_time.push(started.elapsed().as_secs_f64() * 1000.0);
        let started = Instant::now();
        bytes = black_box(view.render(&uri, &uri).unwrap()).len();
        render.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    // Positional delivery runs concurrently with repeated complete-history
    // formatting from immutable views. Four readers retain one 64 KiB each.
    let mut index = Index::default();
    index.update_file(&file, true).unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let readers = (0..4)
        .map(|_| {
            let file = file.clone();
            let finished = finished.clone();
            std::thread::spawn(move || {
                let length = file.metadata().unwrap().len();
                let mut buffer = vec![0_u8; 64 * 1024];
                let size = buffer.len().min(length as usize);
                let mut times = Vec::new();
                while !finished.load(Ordering::Acquire) && times.len() < 100_000 {
                    let started = Instant::now();
                    file.read_exact_at(&mut buffer[..size], 0).unwrap();
                    times.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                times
            })
        })
        .collect::<Vec<_>>();
    for _ in 0..SAMPLES {
        black_box(
            index
                .playlist_view(false)
                .unwrap()
                .render(&uri, &uri)
                .unwrap(),
        );
    }
    finished.store(true, Ordering::Release);
    let mut reads = readers
        .into_iter()
        .flat_map(|reader| reader.join().unwrap())
        .collect::<Vec<_>>();
    reads.sort_by(f64::total_cmp);
    eprintln!("hls_efficiency label={label} seconds={seconds} samples={SAMPLES} cold_ms={:?} warm_ms={:?} view_ms={:?} render_ms={:?} retained_bytes={memory} manifest_bytes={bytes} bytes_per_minute_at_1hz={} segment_read_samples={} segment_read_p50_ms={} segment_read_p95_ms={}",
        summary(&mut cold), summary(&mut warm), summary(&mut view_time), summary(&mut render), bytes * 60, reads.len(), reads[reads.len()/2], reads[reads.len()*95/100]);
}

#[test]
#[ignore = "opt-in metadata benchmark; generated reports stay outside Git"]
fn benchmark_long_title_metadata() {
    let dir = tests::TempDir::new("metadata-benchmark");
    for seconds in [600, 7200, 28_800] {
        let path = dir.path().join(format!("{seconds}.mp4"));
        std::fs::write(&path, synthetic(seconds)).unwrap();
        measure(&path, "synthetic-not-decodable", seconds);
    }
}

#[test]
#[ignore = "opt-in FFmpeg/decoder long-title benchmark"]
fn benchmark_long_title_decodable_media() {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome,
    };
    let dir = tests::TempDir::new("decodable-benchmark");
    for seconds in [600, 7200, 28_800] {
        let path = dir.path().join(format!("{seconds}.mp4"));
        let mut generate = std::process::Command::new("ffmpeg");
        generate
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=size=32x32:rate=1",
                "-t",
                &seconds.to_string(),
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-preset",
                "ultrafast",
                "-g",
                "1",
                "-movflags",
                "+frag_keyframe+empty_moov+default_base_moof",
                "-f",
                "mp4",
                "-y",
            ])
            .arg(&path);
        let mut decode = std::process::Command::new("ffmpeg");
        decode
            .args(["-nostdin", "-v", "error", "-threads", "1", "-i"])
            .arg(&path)
            .args(["-f", "null", "-"]);
        for mut command in [generate, decode] {
            let outcome = SupervisedCommand::new(&mut command)
                .capture_stderr(CaptureConfig::new(65536, CaptureRetention::Tail))
                .run_until(
                    Instant::now() + Duration::from_secs(120),
                    Duration::from_millis(10),
                    || std::ops::ControlFlow::<()>::Continue(()),
                )
                .unwrap();
            match outcome {
                SupervisedOutcome::Exited(output) => assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                ),
                other => panic!("FFmpeg fixture failed: {other:?}"),
            }
        }
        let file = File::open(&path).unwrap();
        let expected = rusty_dlna_http::RemuxOutputExpectation {
            video_codec: Some("h264".into()),
            audio_codecs: Vec::new(),
            duration_seconds: Some(seconds as f64),
            seek_seconds: 0.0,
            video_copy: false,
        };
        validate_finished(
            &file,
            &expected,
            Instant::now() + Duration::from_secs(30),
            &AtomicBool::new(false),
        )
        .unwrap();
        measure(&path, "decoded-h264-32x32-1fps-video-only", seconds);
    }
}
