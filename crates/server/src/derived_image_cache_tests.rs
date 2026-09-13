//! Disposable real JPEG generation alongside a real video producer.
use super::*;

#[test]
fn shutdown_during_image_generation_reaps_helper_and_discards_output() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let mut app = testdata_app();
    app.cfg.cache_min_free_mb = 0;
    app.cfg.derived_image_max_dimension = 8192;
    app.cfg.derived_image_max_pixels = 8192 * 8192;
    app.cfg.derived_image_timeout_secs = 10;
    let image = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.mime == "image/jpeg")
        .unwrap()
        .detail_id;
    let request = req(&get(
        &format!("/Resized/{image}.jpg?width=8192,height=8192"),
        "Test/1.0",
    ));
    let directory = app.cache_dir.join("derived-images");
    let (thread_sent, thread_received) = std::sync::mpsc::channel();
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        struct CancelOnDrop<'a>(&'a CancellationToken);
        impl Drop for CancelOnDrop<'_> {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel = CancelOnDrop(&app.scan_cfg.cancellation);
        scope.spawn(|| {
            thread_sent
                .send(std::fs::read_link("/proc/thread-self").unwrap())
                .unwrap();
            sent.send(app.handle(&request)).unwrap();
        });
        let children = Path::new("/proc")
            .join(
                thread_received
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
            )
            .join("children");
        // Observe this handler's actual FFmpeg child and private JPEG target,
        // after image probing. A helper permit alone could still mean probing
        // or pre-spawn work and would not prove active generation cancellation.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (child, temporary) = loop {
            let candidate = std::fs::read_to_string(&children)
                .unwrap_or_default()
                .split_whitespace()
                .find_map(|pid| {
                    let process = PathBuf::from(format!("/proc/{pid}"));
                    let command = std::fs::read(process.join("cmdline")).ok()?;
                    let temporary = command.split(|byte| *byte == 0).find(|arg| {
                        arg.starts_with(directory.as_os_str().as_bytes())
                            && arg.ends_with(b".tmp.jpg")
                    })?;
                    Some((
                        process,
                        PathBuf::from(std::ffi::OsString::from_vec(temporary.to_vec())),
                    ))
                });
            if let Some(child) = candidate {
                break child;
            }
            assert!(Instant::now() < deadline, "image FFmpeg was not observed");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(app.helpers.metrics().active, 1);
        // Inject a partial at the actual owned output boundary, rather than
        // depending on the short interval between JPEG write and rename.
        // The helper is real; these bytes deliberately model an interrupted
        // output and make the unlink assertion sensitive to cleanup failures.
        std::fs::write(&temporary, b"interrupted JPEG output").unwrap();
        assert!(temporary.is_file());
        assert!(child.exists());
        app.scan_cfg.cancellation.cancel();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .status,
            404
        );
        assert!(!child.exists(), "the image child must be reaped");
        assert!(!temporary.exists(), "known partial output must be removed");
        assert_eq!(app.helpers.metrics().active, 0);
    });
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    assert_eq!(
        app.derived_images
            .maintain(&app.cache_maintenance, &directory, u64::MAX, 30, 0)
            .unwrap()
            .bytes,
        0
    );
    // Restart against the same disposable cache with a fresh cancellation
    // token; cancellation must not leave a poisoned key or reusable partial.
    let fresh = App::from_config(
        Config {
            media_dir: app.cfg.media_dir.clone(),
            cache_dir: Some(app.cache_dir.display().to_string()),
            db_dir: app.cfg.db_dir.clone(),
            rescan_secs: 0,
            cache_min_free_mb: 0,
            derived_image_max_dimension: 8192,
            derived_image_max_pixels: 8192 * 8192,
            ..Config::default()
        },
        18200,
        11900,
        &workspace(),
    );
    *write_recover(&fresh.catalog) = read_recover(&app.catalog).clone();
    let response = fresh.handle(&request);
    assert_eq!(response.status, 200);
    assert!(response.body.starts_with(&[0xff, 0xd8]));
    let files = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read(&files[0]).unwrap(), response.body);
}

#[test]
fn failed_image_helper_leaves_no_published_or_accounted_output_and_retry_succeeds() {
    let mut app = testdata_app();
    app.cfg.cache_min_free_mb = 0;
    let image = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.mime == "image/jpeg")
        .unwrap()
        .detail_id;
    app.cfg.derived_image_max_dimension = 8192;
    app.cfg.derived_image_max_pixels = 8192 * 8192;
    let request = req(&get(
        &format!("/Resized/{image}.jpg?width=8192,height=8192"),
        "Test/1.0",
    ));
    let memory = app.cfg.derived_image_memory_mb;
    // Fault at the real helper boundary: probing succeeds, but the scaled
    // frame needs an allocation larger than FFmpeg's 16 MiB allocation cap.
    app.cfg.derived_image_memory_mb = 16;
    let before = app.helpers.metrics().admitted_total;
    assert_eq!(app.handle(&request).status, 404);
    assert_eq!(app.helpers.metrics().admitted_total - before, 1);
    assert_eq!(app.helpers.metrics().active, 0);
    let directory = app.cache_dir.join("derived-images");
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    assert_eq!(
        app.derived_images
            .maintain(&app.cache_maintenance, &directory, u64::MAX, 30, 0)
            .unwrap()
            .bytes,
        0
    );
    app.cfg.derived_image_memory_mb = memory;
    let response = app.handle(&request);
    assert_eq!(response.status, 200);
    assert!(response.body.starts_with(&[0xff, 0xd8]));
    let files = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read(&files[0]).unwrap(), response.body);
}

#[test]
fn same_key_requests_generate_once_and_external_deletion_retries_safely() {
    let mut app = testdata_app();
    app.cfg.cache_min_free_mb = 0;
    app.helpers = Arc::new(HelperGate::new(4, 8));
    let image = read_recover(&app.catalog)
        .items
        .values()
        .find(|item| item.mime == "image/jpeg")
        .unwrap()
        .detail_id;
    let before = app.helpers.metrics().admitted_total;
    let barrier = std::sync::Barrier::new(4);
    let responses = std::thread::scope(|scope| {
        let workers = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    app.handle(&req(&get(
                        &format!("/Resized/{image}.jpg?width=160,height=160"),
                        "Test/1.0",
                    )))
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(responses
        .iter()
        .all(|response| response.status == 200 && response.body == responses[0].body));
    assert_eq!(app.helpers.metrics().admitted_total - before, 1);
    let directory = app.cache_dir.join("derived-images");
    let files = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read(&files[0]).unwrap(), responses[0].body);
    std::fs::remove_file(&files[0]).unwrap();
    let retry = app.handle(&req(&get(
        &format!("/Resized/{image}.jpg?width=160,height=160"),
        "Test/1.0",
    )));
    assert_eq!(retry.status, 200);
    assert_eq!(retry.body, responses[0].body);
    assert_eq!(app.helpers.metrics().admitted_total - before, 2);
    app.cfg.derived_image_cache_mb = 0;
    let rejected = app.handle(&req(&get(
        &format!("/Resized/{image}.jpg?width=171,height=171"),
        "Test/1.0",
    )));
    assert_eq!(rejected.status, 507);
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    assert_eq!(
        app.derived_images
            .maintain(&app.cache_maintenance, &directory, 0, 30, 0)
            .unwrap()
            .bytes,
        0
    );
    app.cfg.derived_image_cache_mb = 1;
    app.scan_cfg.cancellation.cancel();
    let cancelled = app.handle(&req(&get(
        &format!("/Resized/{image}.jpg?width=171,height=171"),
        "Test/1.0",
    )));
    assert_eq!(cancelled.status, 503);
    assert_eq!(app.helpers.metrics().active, 0);
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "100/1k/10k JPEG cache experiment, ten independent trials per size"]
async fn derived_image_cache_scale_benchmark() {
    let resident_bytes = || {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            * 1024
    };
    for entries in [100, 1_000, 10_000] {
        for trial in 0..10 {
            let mut app = testdata_app();
            app.cfg.cache_min_free_mb = 0;
            app.helpers = Arc::new(HelperGate::new(4, 8));
            let (image, video) = {
                let catalog = read_recover(&app.catalog);
                let image = catalog
                    .items
                    .values()
                    .find(|item| item.mime == "image/jpeg")
                    .unwrap()
                    .detail_id;
                let video = catalog
                    .items
                    .values()
                    .find(|item| item.path.ends_with("tagged.mp4"))
                    .unwrap()
                    .detail_id;
                (image, video)
            };
            let seed = app.handle(&req(&get(
                &format!("/Resized/{image}.jpg?width=160,height=160"),
                "Benchmark/1.0",
            )));
            assert_eq!(seed.status, 200);
            assert!(seed.body.starts_with(&[0xff, 0xd8]));
            let directory = app.cache_dir.join("derived-images");
            // Distinct regular files containing a real generated, decodable
            // JPEG; no sparse/zero-byte stand-ins for the cache inventory.
            for index in 0..entries {
                std::fs::write(directory.join(format!("{index:064x}.jpg")), &seed.body).unwrap();
            }
            // Simulate a populated cache at restart, outside measured misses.
            app.derived_images = derived_image_cache::DerivedImageCache::new();
            let rss_before_inventory = resident_bytes();
            app.derived_images
                .maintain(&app.cache_maintenance, &directory, u64::MAX, 30, 0)
                .unwrap();
            let rss_after_inventory = resident_bytes();
            let before = app.derived_images.work.snapshot();
            let spec = app
                .handle(&req(&get(
                    &format!(
                        "/web/media/{video}.mp4?mode=compatible&video_mode=copy&audio_mode=copy"
                    ),
                    "Benchmark/1.0",
                )))
                .remux_job
                .unwrap();
            let app = Arc::new(app);
            let barrier = Arc::new(std::sync::Barrier::new(5));
            let mut posters = Vec::new();
            for size in 171..175 {
                let app = app.clone();
                let barrier = barrier.clone();
                posters.push(tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    let started = std::time::Instant::now();
                    let response = app.handle(&req(&get(
                        &format!("/Resized/{image}.jpg?width={size},height={size}"),
                        "Benchmark/1.0",
                    )));
                    assert_eq!(response.status, 200);
                    assert!(response.body.starts_with(&[0xff, 0xd8]));
                    (
                        started.elapsed().as_secs_f64() * 1000.0,
                        response.body.len(),
                    )
                }));
            }
            let video_app = app.clone();
            let (job, started) = tokio::task::spawn_blocking(move || {
                barrier.wait();
                let started = std::time::Instant::now();
                (remux::attach(video_app, spec).unwrap(), started)
            })
            .await
            .unwrap();
            let ready = remux::wait_ready(&job).await.unwrap();
            let ready_ms = started.elapsed().as_secs_f64() * 1000.0;
            assert!(
                ready.is_file(),
                "image maintenance must preserve video output"
            );
            let mut image_results = Vec::new();
            for poster in posters {
                image_results.push(poster.await.unwrap());
            }
            let after = app.derived_images.work.snapshot();
            let files = std::fs::read_dir(&directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            let disk_bytes = files
                .iter()
                .map(|path| std::fs::metadata(path).unwrap().len())
                .sum::<u64>();
            assert_eq!(files.len(), entries + 5);
            assert_eq!(
                disk_bytes,
                (entries as u64 + 1) * seed.body.len() as u64
                    + image_results
                        .iter()
                        .map(|(_, bytes)| *bytes as u64)
                        .sum::<u64>()
            );
            let report = app
                .derived_images
                .maintain(&app.cache_maintenance, &directory, u64::MAX, 30, 0)
                .unwrap();
            assert_eq!(
                report.bytes, disk_bytes,
                "independent file sum must match accounting"
            );
            tokio::time::timeout(Duration::from_secs(15), async {
                while *lock_recover(&job.state) != remux::RemuxState::Complete {
                    assert!(!matches!(
                        *lock_recover(&job.state),
                        remux::RemuxState::Failed(_) | remux::RemuxState::Cancelled
                    ));
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let decoded = rusty_dlna_helper::SupervisedCommand::new(
                std::process::Command::new("ffmpeg")
                    .args(["-nostdin", "-v", "error", "-i"])
                    .arg(&job.dest)
                    .args(["-f", "null", "-"]),
            )
            .run_until(
                Instant::now() + Duration::from_secs(30),
                Duration::from_millis(20),
                || std::ops::ControlFlow::<()>::Continue(()),
            )
            .unwrap();
            let rusty_dlna_helper::SupervisedOutcome::Exited(decoded) = decoded else {
                panic!("video decode timed out")
            };
            assert!(
                decoded.status.success(),
                "video remains decodable after concurrent image maintenance"
            );
            println!(
                "image_cache_trial {}",
                serde_json::json!({
                    "entries": entries, "trial": trial, "posters": image_results, "video_ready_ms": ready_ms,
                    "scans": after[0] - before[0], "stats": after[1] - before[1], "lock_wait_nanos": after[2] - before[2],
                    "disk_bytes": disk_bytes, "files": files.len(), "video_bytes": std::fs::metadata(&job.dest).unwrap().len(),
                    "rss_before_inventory": rss_before_inventory, "rss_after_inventory": rss_after_inventory,
                    "rss_after_work": resident_bytes(),
                    "conditions": "release; generated JPEG copies; warm OS cache; four cold posters plus copy/copy video; ready bytes is not a presented frame"
                })
            );
            remux::cancel_all(&app);
            remux::wait_for_shutdown(&app, Duration::from_secs(5)).await;
        }
    }
}
