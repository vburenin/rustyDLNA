//! Profile-8 progress and the boundary between private staging and delivery.
use super::*;
use rusty_dlna_transcode::{RemuxP8IoBasis, RemuxP8Stage, RemuxP8StageEvent, RemuxP8StageStatus};
use std::collections::VecDeque;

const STAGES: [RemuxP8Stage; 7] = [
    RemuxP8Stage::Probe,
    RemuxP8Stage::Extraction,
    RemuxP8Stage::Conversion,
    RemuxP8Stage::Wrapping,
    RemuxP8Stage::PacketRewrite,
    RemuxP8Stage::Signaling,
    RemuxP8Stage::FinalMux,
];
const MAX_RECORDS: usize = 64;

/// Producer-owned accounting survives eviction from the diagnostic history.
pub(super) struct Token {
    sequence: u64,
    terminal: [bool; 7],
}

fn stage_name(stage: RemuxP8Stage) -> &'static str {
    match stage {
        RemuxP8Stage::Probe => "probe",
        RemuxP8Stage::Extraction => "extraction",
        RemuxP8Stage::Conversion => "conversion",
        RemuxP8Stage::Wrapping => "wrapping",
        RemuxP8Stage::PacketRewrite => "packet_rewrite",
        RemuxP8Stage::Signaling => "signaling",
        RemuxP8Stage::FinalMux => "final_mux",
    }
}

fn status_name(status: RemuxP8StageStatus) -> &'static str {
    match status {
        RemuxP8StageStatus::Started => "started",
        RemuxP8StageStatus::Progress => "running",
        RemuxP8StageStatus::Succeeded => "succeeded",
        RemuxP8StageStatus::Failed => "failed",
        RemuxP8StageStatus::Cancelled => "cancelled",
        RemuxP8StageStatus::Deadline => "deadline",
        RemuxP8StageStatus::Rejected => "rejected",
    }
}

#[derive(Debug)]
struct Record {
    sequence: u64,
    stages: [Option<RemuxP8StageEvent>; 7],
}

#[derive(Debug, Default)]
pub(super) struct Metrics {
    sequence: AtomicU64,
    records: Mutex<VecDeque<Record>>,
    stages: [AtomicDurationMetric; 7],
}

impl Metrics {
    pub(super) fn begin(&self) -> Token {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut records = crate::lock_recover(&self.records);
        if records.len() == MAX_RECORDS {
            records.pop_front();
        }
        records.push_back(Record {
            sequence,
            stages: [None; 7],
        });
        Token {
            sequence,
            terminal: [false; 7],
        }
    }

    pub(super) fn record(&self, token: &mut Token, event: RemuxP8StageEvent) {
        let Some(index) = STAGES.iter().position(|stage| *stage == event.stage) else {
            return;
        };
        let terminal = |status| {
            !matches!(
                status,
                RemuxP8StageStatus::Started | RemuxP8StageStatus::Progress
            )
        };
        let already_terminal = token.terminal[index];
        if already_terminal && !terminal(event.status) {
            return;
        }
        token.terminal[index] |= terminal(event.status);
        let mut records = crate::lock_recover(&self.records);
        if let Some(record) = records
            .iter_mut()
            .find(|record| record.sequence == token.sequence)
        {
            record.stages[index] = Some(event);
        }
        drop(records);
        // A successful-stage callback may itself reject publication/pressure;
        // keep its final status but count that attempt only once.
        if terminal(event.status) && !already_terminal {
            self.stages[index].record(event.elapsed);
        }
    }

    pub(super) fn snapshot(&self) -> serde_json::Value {
        let stages: serde_json::Map<_, _> = STAGES
            .iter()
            .enumerate()
            .map(|(index, stage)| {
                (
                    stage_name(*stage).to_owned(),
                    serde_json::to_value(self.stages[index].snapshot()).unwrap_or_default(),
                )
            })
            .collect();
        let recent: Vec<_> = crate::lock_recover(&self.records).iter().map(|record| {
            let stages: Vec<_> = record.stages.iter().flatten().map(|event| {
                let io = event.io.map(|io| serde_json::json!({
                    "complete": io.is_complete(),
                    "read_bytes": io.read_bytes, "written_bytes": io.written_bytes,
                    "storage_read_bytes": io.storage_read_bytes, "storage_written_bytes": io.storage_written_bytes,
                    "basis": match io.basis { RemuxP8IoBasis::ProcessCounters => "sampled_process_counters", RemuxP8IoBasis::ApplicationCounters => "application_counters" },
                }));
                serde_json::json!({"stage": stage_name(event.stage), "status": status_name(event.status),
                    "elapsed_ms": rusty_dlna_helper::duration_millis_saturating(event.elapsed),
                    "input_file_bytes": event.input_bytes, "output_file_bytes": event.output_bytes, "io": io})
            }).collect();
            serde_json::json!({"sequence": record.sequence, "stages": stages})
        }).collect();
        serde_json::json!({"stages_ms": stages, "recent": recent, "recent_limit": MAX_RECORDS})
    }
}

/// Inspect final-mux bytes only after the packet rewrite has verified original
/// sample association and retained source timing, and signaling has succeeded.
/// Readiness alone cannot establish those prerequisite properties. Inspection
/// does not pin an unready attempt; an unobserved generation can still fall back.
/// A complete independently playable copied-video segment is required, using
/// the same bounded parser and dependency look-ahead as ordinary fragment delivery.
pub(super) fn observe_final_mux(
    app: &Arc<App>,
    job: &Arc<RemuxJob>,
    monitor: &mut cache_monitor::Monitor,
    kind: cache_monitor::Kind,
) -> Result<bool, String> {
    if job.cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    if matches!(job.state(), RemuxState::Growing) {
        job.notify_growth();
    }
    let Some(observation) = monitor
        .poll(app, job, kind)
        .map_err(|error| format!("transcode cache limits: {error}"))?
    else {
        return Ok(false);
    };
    if job.cancelled.load(Ordering::Acquire)
        || !observation.playable
        || !matches!(job.state(), RemuxState::Preprocessing)
    {
        return Ok(false);
    }
    job.transition(RemuxState::Growing);
    Ok(true)
}

pub(super) fn inspect_final_mux(path: &Path, index: &mut hls::Index) -> Result<bool, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("open Profile-8 final mux: {error}")),
    };
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Profile-8 final mux is not a regular file".into());
    }
    index.update_file(&file, false)?;
    Ok(index.has_playable_segment())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(stage: RemuxP8Stage, status: RemuxP8StageStatus) -> RemuxP8StageEvent {
        RemuxP8StageEvent {
            stage,
            status,
            elapsed: Duration::from_millis(7),
            input_bytes: Some(500),
            output_bytes: Some(100),
            io: None,
        }
    }

    #[test]
    fn progress_has_fixed_records_and_stages_without_paths_or_request_identity() {
        let metrics = Metrics::default();
        for _ in 0..100 {
            let mut sequence = metrics.begin();
            for _ in 0..100 {
                metrics.record(
                    &mut sequence,
                    event(RemuxP8Stage::Conversion, RemuxP8StageStatus::Progress),
                );
            }
            metrics.record(
                &mut sequence,
                event(RemuxP8Stage::Conversion, RemuxP8StageStatus::Succeeded),
            );
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["recent"].as_array().unwrap().len(), MAX_RECORDS);
        assert_eq!(snapshot["stages_ms"]["conversion"]["count"], 100);
        for record in snapshot["recent"].as_array().unwrap() {
            assert_eq!(record["stages"].as_array().unwrap().len(), 1);
            assert_eq!(record["stages"][0]["output_file_bytes"], 100);
            assert!(record["stages"][0]["io"].is_null());
        }
        let text = snapshot.to_string();
        for secret in ["path", "detail_id", "request_id", "stderr"] {
            assert!(!text.contains(secret));
        }
    }

    fn job(dir: &Path) -> RemuxJob {
        RemuxJob {
            detail_id: 42,
            web_request_ids: Mutex::new(HashSet::new()),
            web_sessions: Mutex::new(HashMap::new()),
            web: false,
            web_spec: None,
            cache_hit: false,
            registry_finalized: AtomicBool::new(false),
            producer_finished: AtomicBool::new(false),
            output: Mutex::new(None),
            startup_observations: WebStartupObservations::default(),
            dest: dir.join("out.mp4"),
            part: dir.join("out.mp4.part"),
            state: Mutex::new(RemuxState::Preprocessing),
            changed: tokio::sync::Notify::new(),
            cancelled: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            client_epoch: AtomicU64::new(0),
            disconnect_deadline: Mutex::new(None),
            cacheable: true,
            started: Instant::now(),
            hls_index: Mutex::new(hls::Index::default()),
            effective_recipe: Mutex::new(None),
        }
    }

    #[test]
    fn terminal_accounting_survives_history_eviction_and_rejected_success() {
        let metrics = Metrics::default();
        let mut evicted = metrics.begin();
        for _ in 0..MAX_RECORDS {
            metrics.begin();
        }
        for status in [
            RemuxP8StageStatus::Succeeded,
            RemuxP8StageStatus::Rejected,
            RemuxP8StageStatus::Progress,
        ] {
            metrics.record(&mut evicted, event(RemuxP8Stage::FinalMux, status));
        }
        let mut retained = metrics.begin();
        for status in [
            RemuxP8StageStatus::Succeeded,
            RemuxP8StageStatus::Rejected,
            RemuxP8StageStatus::Progress,
        ] {
            metrics.record(&mut retained, event(RemuxP8Stage::FinalMux, status));
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["stages_ms"]["final_mux"]["count"], 2);
        assert_eq!(
            snapshot["recent"][MAX_RECORDS - 1]["stages"][0]["status"],
            "rejected"
        );
    }

    #[test]
    fn final_mux_requires_complete_copied_segment_and_keeps_publication_private() {
        let dir = super::super::tests::temp_dir("p8-growing-boundary");
        let app = super::super::tests::test_app(&dir, 1);
        let fixture = dir.join("fixture.mp4");
        super::super::validation_tests::generate(&fixture, "8", "libx264", true);
        let bytes = std::fs::read(&fixture).unwrap();
        let mut offset = 0;
        let mut media_ends = Vec::new();
        while offset < bytes.len() {
            let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            if &bytes[offset + 4..offset + 8] == b"mdat" {
                media_ends.push(offset + size);
            }
            offset += size;
        }
        assert!(media_ends.len() >= 3);
        let job = Arc::new(job(&dir));
        let observe = || {
            let mut monitor = cache_monitor::Monitor::new();
            assert!(
                !observe_final_mux(&app, &job, &mut monitor, cache_monitor::Kind::Profile8)
                    .unwrap()
            );
            super::super::tests::wait_until(Duration::from_secs(2), || monitor.result_ready());
            observe_final_mux(&app, &job, &mut monitor, cache_monitor::Kind::Profile8).unwrap()
        };
        // Size alone and incomplete media do not make private bytes ready.
        std::fs::write(&job.part, &bytes[..media_ends[0] - 1]).unwrap();
        assert!(!observe());
        assert_eq!(job.state(), RemuxState::Preprocessing);
        assert!(job.pin_ready_output().unwrap().is_none());
        assert!(crate::lock_recover(&job.output).is_none());
        // Copied fragments retain the established independent-segment look-ahead.
        std::fs::write(&job.part, &bytes[..media_ends[0]]).unwrap();
        assert!(!observe());
        std::fs::write(&job.part, &bytes[..media_ends[2]]).unwrap();
        assert!(observe());
        assert_eq!(job.state(), RemuxState::Growing);
        assert!(!job.dest.exists());
        assert!(!rusty_dlna_transcode::cache_stamp_path(&job.dest).exists());
        assert!(
            crate::lock_recover(&job.output).is_none(),
            "readiness inspection must not pin an unobserved attempt"
        );
        let pinned = job.open_output().unwrap();
        std::fs::write(&job.part, &bytes).unwrap();
        assert!(!observe());
        assert_eq!(pinned.metadata().unwrap().len(), bytes.len() as u64);
        job.cancel();
        assert!(job.pin_ready_output().is_err());
        assert!(!observe_final_mux(
            &app,
            &job,
            &mut cache_monitor::Monitor::new(),
            cache_monitor::Kind::Profile8
        )
        .unwrap());
    }

    fn staged_runner(
        part: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        observer: &mut dyn FnMut(RemuxP8StageEvent) -> Result<(), String>,
    ) -> Result<(), RemuxP8Error> {
        let bytes = std::fs::read(part.with_extension("fixture")).unwrap();
        std::fs::write(part, &bytes).unwrap();
        observer(event(
            RemuxP8Stage::Signaling,
            RemuxP8StageStatus::Succeeded,
        ))
        .map_err(RemuxP8Error::Observer)?;
        std::fs::write(part.with_extension("private"), b"ready").unwrap();
        while !part.with_extension("mux").exists() {
            if cancelled.load(Ordering::Acquire) {
                return Err(RemuxP8Error::Cancelled("test".into()));
            }
            if Instant::now() >= deadline {
                return Err(RemuxP8Error::Deadline("test".into()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        observer(event(RemuxP8Stage::FinalMux, RemuxP8StageStatus::Progress))
            .map_err(RemuxP8Error::Observer)?;
        std::fs::write(part.with_extension("growing"), b"ready").unwrap();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(RemuxP8Error::Cancelled("test".into()));
            }
            if Instant::now() >= deadline {
                return Err(RemuxP8Error::Deadline("test".into()));
            }
            observer(event(RemuxP8Stage::FinalMux, RemuxP8StageStatus::Progress))
                .map_err(RemuxP8Error::Observer)?;
            if let Ok(mode) = std::fs::read_to_string(part.with_extension("finish")) {
                return match mode.as_str() {
                    "failure" => Err(RemuxP8Error::Pipeline("test downstream failure".into())),
                    "truncated" => {
                        // A helper can exit successfully after writing an incomplete tail.
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(part)
                            .unwrap()
                            .set_len(bytes.len() as u64 - 1)
                            .unwrap();
                        Ok(())
                    }
                    "success" => Ok(()),
                    _ => panic!("unknown test mode"),
                };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_marker(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(path.exists(), "stage marker did not arrive");
    }

    #[test]
    fn final_mux_exposure_preserves_failure_cancel_validation_and_atomic_publication() {
        use super::super::tests::{job_spec, temp_dir, test_app, wait_for_terminal_cleanup};
        let root = temp_dir("p8-growing-lifecycle");
        let fixture = root.join("fixture.mp4");
        super::super::validation_tests::generate(&fixture, "8", "libx264", true);
        for mode in ["success", "failure", "truncated", "cancel"] {
            let dir = root.join(mode);
            std::fs::create_dir(&dir).unwrap();
            let app = test_app(&dir, 1);
            let mut spec = job_spec(&dir, "p8", vec!["must-not-run-fallback".into()]);
            spec.remux_p8 = true;
            spec.output_expectation = Some(rusty_dlna_http::RemuxOutputExpectation {
                video_codec: Some("h264".into()),
                audio_codecs: vec!["aac".into()],
                duration_seconds: Some(8.0),
                seek_seconds: 0.0,
                video_copy: false,
            });
            let part = cache_part(&spec.dest);
            std::fs::copy(&fixture, part.with_extension("fixture")).unwrap();
            crate::lock_recover(p8_test_runners()).insert(part.clone(), staged_runner);
            let job = attach(app.clone(), spec).unwrap();
            struct CancelOnDrop(Arc<RemuxJob>);
            impl Drop for CancelOnDrop {
                fn drop(&mut self) {
                    self.0.cancel();
                }
            }
            let _cancel = CancelOnDrop(job.clone());
            wait_marker(&part.with_extension("private"));
            assert_eq!(job.state(), RemuxState::Preprocessing);
            assert!(job.pin_ready_output().unwrap().is_none());
            std::fs::write(part.with_extension("mux"), b"continue").unwrap();
            wait_marker(&part.with_extension("growing"));
            super::super::tests::wait_until(Duration::from_secs(2), || {
                job.state() == RemuxState::Growing
            });
            assert_eq!(job.state(), RemuxState::Growing);
            assert!(!job.dest.exists());
            assert!(!rusty_dlna_transcode::cache_stamp_path(&job.dest).exists());
            let output = job.open_output().unwrap();
            if mode == "cancel" {
                job.cancel();
            } else {
                std::fs::write(part.with_extension("finish"), mode).unwrap();
            }
            wait_for_terminal_cleanup(&app, &job);
            if mode == "success" {
                assert_eq!(job.state(), RemuxState::Complete);
                assert!(job.dest.exists());
                assert!(rusty_dlna_transcode::cache_stamp_path(&job.dest).exists());
                assert_eq!(
                    output.metadata().unwrap().len(),
                    std::fs::metadata(&job.dest).unwrap().len()
                );
            } else {
                assert!(matches!(
                    job.state(),
                    RemuxState::Failed(_) | RemuxState::Cancelled
                ));
                if mode == "failure" {
                    assert_eq!(
                        job.state(),
                        RemuxState::Failed(
                            "Profile-8 output failed after its generation was pinned".into()
                        )
                    );
                }
                assert!(!job.dest.exists());
                assert!(!rusty_dlna_transcode::cache_stamp_path(&job.dest).exists());
                assert!(job.open_output().is_err());
            }
            assert!(!job.part.exists());
            assert_eq!(app.helpers.metrics().active, 0);
            assert_eq!(app.jobs.in_use(), 0);
        }
    }
}
