//! Opt-in measurements of the production Profile-8 stages; no serving policy changes.
//! cargo run -p rusty-dlna --example profile8_stages -- SOURCE [samples=5] [keep]
//! JSON lines go to stdout. Every sample uses disposable, uncached output.

use rusty_dlna_helper::{CancellationToken, HelperGate};
use rusty_dlna_transcode::{
    run_remux_p8_with_toolchain_stage_observed, transcode_cache_identity_file_controlled,
    AudioAction, Decision, RecodeAction, RemuxP8Input, RemuxP8StageStatus, ToolQueryControl,
    TranscodePlan,
};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SOURCE_LIMIT: u64 = 256 * 1024 * 1024;
const OUTPUT_LIMIT: u64 = 512 * 1024 * 1024;
const JOB_SECONDS: u64 = 120;

struct Scratch(PathBuf, bool);

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.1 {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn run(cancelled: CancellationToken) -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|value| value == "--help") {
        println!("cargo run -p rusty-dlna --example profile8_stages -- SOURCE [samples=5,1..20] [keep]\nMeasures the actual sequential Profile-8 pipeline with fresh disposable output and the installed ffmpeg, ffprobe and DOVI_TOOL. Source <=256 MiB, staging <=512 MiB, one admitted helper, 120-second absolute job deadline. Outputs JSON lines; never publishes a reusable cache. Optional keep retains successful output in the reported temporary directory for external pixel/timing validation. Stage completion is not final server validation or a presented video frame. SIGINT/SIGTERM cancel and reap the active helper.");
        return Ok(());
    }
    if args.is_empty() || args.len() > 3 || args.get(2).is_some_and(|value| value != "keep") {
        return Err("expected SOURCE and optional sample count".into());
    }
    let source = PathBuf::from(&args[0]);
    let samples = args
        .get(1)
        .map(|value| value.to_str().unwrap_or("").parse::<usize>())
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or(5);
    if !(1..=20).contains(&samples) {
        return Err("sample count must be 1..20".into());
    }
    let file = fs::File::open(&source).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > SOURCE_LIMIT {
        return Err("source must be a regular file no larger than 256 MiB".into());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let mut scratch = Scratch(
        std::env::temp_dir().join(format!(
            "rustydlna-profile8-stages-{}-{stamp}",
            std::process::id()
        )),
        false,
    );
    fs::create_dir(&scratch.0).map_err(|error| error.to_string())?;
    let helpers = Arc::new(HelperGate::new(1, 1));
    let plan = TranscodePlan {
        decision: Decision::Recode,
        action: RecodeAction::RemuxP8,
        video_encoder: "copy".into(),
        audio: AudioAction::ToAc3,
        container: "mp4",
        ..TranscodePlan::default()
    };
    println!(
        "{}",
        json!({"kind":"environment", "source":source.to_string_lossy(), "source_bytes":metadata.len(),
            "samples":samples, "output_cache":"fresh each sample", "os_page_cache":"uncontrolled; no eviction",
            "tool_cache":"first query then warm in this process", "helper_limit":1,
            "staging_byte_limit":OUTPUT_LIMIT, "deadline_seconds":JOB_SECONDS,
            "video":"copy; dovi_tool mode 2 discards enhancement layer", "audio":"first ordinal to ac3 640k",
            "cgroup":fs::read_to_string("/proc/self/cgroup").ok()})
    );
    for sample in 0..samples {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(JOB_SECONDS);
        let identity = transcode_cache_identity_file_controlled(
            &file,
            &source,
            &plan,
            true,
            ToolQueryControl::new(&helpers, &cancelled, Duration::from_secs(10)),
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "cannot determine source identity".to_string())?;
        let toolchain = identity
            .profile8_toolchain()
            .ok_or_else(|| "missing Profile-8 toolchain".to_string())?;
        println!(
            "{}",
            json!({"kind":"preparation", "sample":sample, "elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
                "ffmpeg":toolchain.ffmpeg().fingerprint(), "ffprobe":toolchain.ffprobe().fingerprint(),
                "dovi_tool":toolchain.dovi_tool().fingerprint()})
        );
        let _permit = helpers
            .acquire_timeout_cancelled(Duration::from_secs(10), &cancelled)
            .map_err(|error| error.to_string())?;
        let part = scratch.0.join(format!("sample-{sample}.mp4.part"));
        let result = run_remux_p8_with_toolchain_stage_observed(
            toolchain,
            RemuxP8Input::OpenFile {
                file: &file,
                identity_path: &source,
            },
            &part,
            &plan,
            deadline,
            cancelled.as_atomic(),
            |event| {
                let bytes = fs::read_dir(&scratch.0)
                    .map_err(|error| error.to_string())?
                    .try_fold(0_u64, |total, entry| {
                        let size = entry?.metadata()?.len();
                        Ok::<_, std::io::Error>(total.saturating_add(size))
                    })
                    .map_err(|error| error.to_string())?;
                if bytes > OUTPUT_LIMIT {
                    return Err("experiment staging byte limit exceeded".into());
                }
                if event.status != RemuxP8StageStatus::Progress {
                    println!(
                        "{}",
                        json!({"kind":"stage", "sample":sample,
                            "stage":format!("{:?}",event.stage), "status":format!("{:?}",event.status),
                            "elapsed_ms":event.elapsed.as_secs_f64()*1000.0,
                            "job_elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
                            "input_bytes":event.input_bytes,"output_bytes":event.output_bytes,
                            "io":event.io.map(|io| json!({"basis":format!("{:?}",io.basis),"complete":io.is_complete(),
                                "read_bytes":io.read_bytes,"written_bytes":io.written_bytes,
                                "storage_read_bytes":io.storage_read_bytes,
                                "storage_written_bytes":io.storage_written_bytes}))})
                    );
                }
                Ok(())
            },
        );
        println!(
            "{}",
            json!({"kind":"sample", "sample":sample,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
                "output":part.to_string_lossy(), "retained_on_success":args.get(2).is_some(),
                "result":result.as_ref().map(|()| "pipeline_finished").map_err(ToString::to_string)})
        );
        result.map_err(|error| error.to_string())?;
        if args.get(2).is_none() {
            fs::remove_file(part).map_err(|error| error.to_string())?;
        }
    }
    scratch.1 = args.get(2).is_some();
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let cancelled = CancellationToken::default();
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let signal_token = cancelled.clone();
    let signals = tokio::spawn(async move {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        signal_token.cancel();
    });
    let result = tokio::task::spawn_blocking(move || run(cancelled))
        .await
        .map_err(|error| error.to_string())?;
    signals.abort();
    result
}
