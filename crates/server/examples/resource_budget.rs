//! Opt-in P07 experiment; no daemon configuration or production policy changes.
//! cargo run -p rusty-dlna --example resource_budget -- [samples] [cpu-budget] [gpu]
//! Writes TSV to stdout and removes its generated media on exit.
#[path = "resource_budget/policy.rs"]
mod policy;

use policy::{Request, Scheduler, Viewer, Work};
use rusty_dlna_helper::{
    CaptureConfig, CaptureRetention, HelperGate, SupervisedCommand, SupervisedOutcome,
};
use rusty_dlna_transcode::{
    AudioAction, AudioCodec, BrowserEncodingPreset, BrowserOutputOptions, BrowserQuality,
    HardwareDecode, HdrKind, RecodeAction, TranscodePlan, VideoCodec,
};
use std::ffi::OsString;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static CANCELLED: std::sync::LazyLock<rusty_dlna_helper::CancellationToken> =
    std::sync::LazyLock::new(rusty_dlna_helper::CancellationToken::default);

const GLOBAL_HELPERS: usize = 4;
const MEDIA_SECONDS: usize = 40;
const QUEUE_DEADLINE_MS: u64 = 10_000;
const HELPER_DEADLINE_SECONDS: u64 = 90;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Resources {
    ticks: u64,
    threads: usize,
    rss_kb: u64,
    first_output_ms: Option<u128>,
    samples: usize,
}

impl Resources {
    fn sample(&mut self, output: &Path, started: Instant) {
        self.samples += 1;
        if self.first_output_ms.is_none()
            && output.metadata().is_ok_and(|meta| meta.len() >= 16 * 1024)
        {
            self.first_output_ms = Some(started.elapsed().as_millis());
        }
        // Only this supervision thread's children; simultaneous workers are not
        // attributed to one another. Exited/short-lived helpers may be missed.
        if let Ok(children) = fs::read_to_string("/proc/thread-self/children") {
            for pid in children.split_whitespace() {
                if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
                    continue;
                }
                if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                    if let Some((_, tail)) = stat.rsplit_once(") ") {
                        let values: Vec<_> = tail.split_whitespace().collect();
                        let number = |index: usize| {
                            values
                                .get(index)
                                .and_then(|value| value.parse::<u64>().ok())
                                .unwrap_or(0)
                        };
                        self.ticks = self.ticks.max(number(11).saturating_add(number(12)));
                        self.threads = self.threads.max(number(17) as usize);
                    }
                }
                if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
                    if let Some(kb) = status.lines().find_map(|line| {
                        line.strip_prefix("VmRSS:")
                            .and_then(|value| value.split_whitespace().next()?.parse::<u64>().ok())
                    }) {
                        self.rss_kb = self.rss_kb.max(kb);
                    }
                }
            }
        }
    }
}

fn supervised(program: &str, args: &[OsString], output: &Path) -> Result<Resources, String> {
    let mut command = Command::new(program);
    command.args(args);
    let started = Instant::now();
    let mut resources = Resources::default();
    let result = SupervisedCommand::new(&mut command)
        .capture_stdout(CaptureConfig::new(64 * 1024, CaptureRetention::Tail))
        .capture_stderr(CaptureConfig::new(16 * 1024, CaptureRetention::Tail))
        .run_until(
            started + Duration::from_secs(HELPER_DEADLINE_SECONDS),
            Duration::from_millis(20),
            || {
                if CANCELLED.is_cancelled() {
                    return ControlFlow::Break(());
                }
                resources.sample(output, started);
                ControlFlow::<()>::Continue(())
            },
        )
        .map_err(|error| error.to_string())?;
    match result {
        SupervisedOutcome::Exited(result) if result.status.success() => Ok(resources),
        SupervisedOutcome::Exited(result) => Err(format!(
            "{program}: {}: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        )),
        _ => Err(format!("{program}: deadline or cancellation")),
    }
}

fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn generated_fixture(path: &Path) -> Result<(), String> {
    let mut args = strings(&[
        "-nostdin",
        "-hide_banner",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=24",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        &MEDIA_SECONDS.to_string(),
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-threads:v",
        "2",
        "-g",
        "48",
        "-bf",
        "2",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-threads:a",
        "1",
    ]);
    args.push(path.into());
    supervised("ffmpeg", &args, path).map(|_| ())
}

fn job_args(
    work: Work,
    input: &Path,
    output: &Path,
    experimental: bool,
) -> (&'static str, Vec<OsString>) {
    if work == Work::Probe {
        let mut args = strings(&["-v", "error", "-show_streams", "-show_format"]);
        args.push(input.into());
        return ("ffprobe", args);
    }
    if work == Work::Artwork {
        let mut args = strings(&["-nostdin", "-v", "error"]);
        if experimental {
            args.extend(strings(&["-threads", "1", "-filter_threads", "1"]));
        }
        args.extend(strings(&["-i"]));
        args.push(input.into());
        args.extend(strings(&["-frames:v", "1", "-vf", "scale=320:-2"]));
        if experimental {
            args.extend(strings(&["-threads:v", "1"]));
        }
        args.push(output.into());
        return ("ffmpeg", args);
    }
    let plan = TranscodePlan {
        decision: rusty_dlna_transcode::Decision::Recode,
        action: RecodeAction::Browser,
        rule: None,
        keep_hdr10: false,
        drop_dolby_vision: false,
        video_encoder: if matches!(work, Work::Copy | Work::Audio) {
            "copy"
        } else if work == Work::Gpu {
            "h264_nvenc"
        } else {
            "libx264"
        }
        .into(),
        hardware_decode: HardwareDecode::None,
        audio: if matches!(work, Work::Copy | Work::Video) {
            AudioAction::Copy
        } else {
            AudioAction::ToAac
        },
        container: "mp4",
        audio_index: 0,
        browser_quality: Some(BrowserQuality::Low360),
        browser_ai_upscale: None,
        download_audio: None,
    };
    let mut args = rusty_dlna_transcode::browser_ffmpeg_os_args(
        input,
        output,
        &plan,
        BrowserOutputOptions {
            encoding_preset: BrowserEncodingPreset::Balanced,
            source_video: Some(VideoCodec::H264),
            selected_audio: AudioCodec::Aac,
            source_hdr: HdrKind::Sdr,
            start_seconds: 0,
            // Throughput experiment; pacing is measured separately. Both arms use
            // this same existing raw-MP4 recipe and identical quality settings.
            hls: false,
        },
    );
    // The production builder includes argv[0]; Command already selects it.
    if args.first().is_some_and(|argument| argument == "ffmpeg") {
        args.remove(0);
    }
    if experimental {
        let input_position = args
            .iter()
            .position(|arg| arg == "-i")
            .expect("builder input");
        args.splice(
            input_position..input_position,
            strings(&[
                "-threads",
                "1",
                "-filter_threads",
                "1",
                "-filter_complex_threads",
                "1",
            ]),
        );
        let output_position = args.len() - 1;
        args.splice(
            output_position..output_position,
            strings(&["-threads:v", "1", "-threads:a", "1"]),
        );
    }
    ("ffmpeg", args)
}

#[derive(Clone, Copy)]
enum Arm {
    Automatic,
    Threads,
    Resources,
}
impl Arm {
    fn name(self) -> &'static str {
        match self {
            Self::Automatic => "fifo-auto-threads",
            Self::Threads => "fifo-bounded-threads",
            Self::Resources => "resource-bounded-threads",
        }
    }
    fn bounded(self) -> bool {
        !matches!(self, Self::Automatic)
    }
}

fn run_trial(root: &Path, sample: usize, mode: Arm, cpus: usize, gpu: bool) -> Result<(), String> {
    let arm = mode.name();
    let started = Instant::now();
    let mut scheduler = Scheduler::new(GLOBAL_HELPERS, cpus, matches!(mode, Arm::Resources));
    let gate = Arc::new(HelperGate::new(GLOBAL_HELPERS, 0));
    let mut work = vec![
        Work::Probe,
        Work::Artwork,
        Work::Both,
        Work::Video,
        Work::Audio,
        Work::Copy,
        Work::Probe,
        Work::Artwork,
    ];
    if gpu {
        work.push(Work::Gpu);
    }
    for (id, work) in work.iter().copied().enumerate() {
        scheduler.enqueue(Request {
            id,
            work,
            queued_ms: 0,
        })?;
    }
    let (send, receive) = mpsc::channel();
    let mut workers = Vec::new();
    let mut finished = 0;
    let mut outputs = Vec::new();
    let mut errors = Vec::new();
    while finished < work.len() {
        let now = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        for expired in scheduler.expire(
            now,
            if CANCELLED.is_cancelled() {
                0
            } else {
                QUEUE_DEADLINE_MS
            },
        ) {
            errors.push(format!("{arm} {:?} queue deadline", expired.work));
            finished += 1;
        }
        while let Some(request) = scheduler.next(now) {
            let permit = match gate.try_acquire() {
                Ok(permit) => permit,
                Err(error) => {
                    scheduler.release(request.id);
                    finished += 1;
                    errors.push(error.to_string());
                    continue;
                }
            };
            let sender = send.clone();
            let extension = if request.work == Work::Artwork {
                "jpg"
            } else {
                "mp4"
            };
            let output = root.join(format!("{sample}-{arm}-{}.{}", request.id, extension));
            if request.work != Work::Probe {
                outputs.push(output.clone());
            }
            let (program, args) = job_args(
                request.work,
                &root.join("input.mkv"),
                &output,
                mode.bounded(),
            );
            // Exact command and recipe are retained separately from timed rows.
            eprintln!(
                "recipe sample={sample} arm={arm} work={:?} program={program} args={args:?}",
                request.work
            );
            let worker = std::thread::Builder::new()
                .name(format!("resource-{}", request.id))
                .spawn(move || {
                    let begin = Instant::now();
                    // SupervisedCommand reaps on unwind. Always notify the scheduler
                    // so a failed observer cannot leave a phantom running request.
                    let result = std::panic::catch_unwind(|| supervised(program, &args, &output))
                        .unwrap_or_else(|_| Err("resource observer panicked".into()));
                    drop(permit);
                    let _ = sender.send((request, now, begin.elapsed(), result));
                });
            match worker {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    scheduler.release(request.id);
                    finished += 1;
                    errors.push(format!("spawn resource worker: {error}"));
                }
            }
        }
        if let Ok((request, wait, elapsed, result)) =
            receive.recv_timeout(Duration::from_millis(20))
        {
            scheduler.release(request.id);
            finished += 1;
            match result {
                Ok(resources) => println!(
                    "{sample}\t{arm}\t{:?}\t{wait}\t{}\t{}\t{}\t{}\t{}\t{}",
                    request.work,
                    elapsed.as_millis(),
                    resources
                        .first_output_ms
                        .map_or_else(|| "unobserved".into(), |value| value.to_string()),
                    resources.ticks,
                    resources.threads,
                    resources.rss_kb,
                    resources.samples
                ),
                Err(error) => errors.push(error),
            }
        }
    }
    for worker in workers {
        if worker.join().is_err() {
            errors.push("resource worker panicked".into());
        }
    }
    assert_eq!(gate.metrics().active, 0);
    // Decoding verification is outside the timing window, on every produced
    // artifact. It is not a quality equivalence metric or a cache validator.
    for output in outputs {
        if CANCELLED.is_cancelled() {
            return Err("resource experiment cancelled".into());
        }
        if output.is_file() {
            let mut args = strings(&["-nostdin", "-v", "error", "-xerror", "-threads", "1", "-i"]);
            args.push(output.into());
            args.extend(strings(&["-threads", "1", "-f", "null", "-"]));
            if let Err(error) = supervised("ffmpeg", &args, Path::new("")) {
                errors.push(error);
            }
        } else {
            errors.push(format!("missing generated output: {}", output.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() > 3 || args.first().is_some_and(|arg| arg == "--help") {
        println!("cargo run -p rusty-dlna --example resource_budget -- [samples=5,1..20] [cpu-budget=8,1..64] [gpu]\nOpt-in generated SDR CPU experiment: four helper ceiling, 10-second queue, 90-second child deadline. Three arms isolate thread pools from admission: FIFO/automatic, FIFO/bounded, reserved interactive/resource admission/bounded. Optional gpu adds H264 NVENC with software decoding as a separate workload; AI execution is not covered. No daemon defaults change.");
        return Ok(());
    }
    let gpu = match args.get(2).map(String::as_str) {
        None => false,
        Some("gpu") => true,
        _ => return Err("third argument must be gpu".into()),
    };
    let samples = args
        .first()
        .map_or(Ok(5), |arg| arg.parse::<usize>())
        .map_err(|error| error.to_string())?;
    let requested_cpus = args
        .get(1)
        .map_or(Ok(8), |arg| arg.parse::<usize>())
        .map_err(|error| error.to_string())?;
    if !(1..=20).contains(&samples) || !(1..=64).contains(&requested_cpus) {
        return Err("out-of-range experiment arguments".into());
    }
    // Rust's available_parallelism is advisory and includes Linux affinity and
    // supported cgroup quota discovery; retain raw membership for audit. Missing
    // controller visibility cannot certify host/container resource enforcement.
    let detected = std::thread::available_parallelism()
        .map(usize::from)
        .map_err(|error| error.to_string())?;
    let cpus = requested_cpus.min(detected);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "rustydlna-resource-budget-{}-{stamp}",
        std::process::id()
    )));
    fs::create_dir(&scratch.0).map_err(|error| error.to_string())?;
    println!("# generated 640x360 24fps SDR H264/AAC; duration={MEDIA_SECONDS}; warm local page cache; helpers={GLOBAL_HELPERS}; requested_cpu_tokens={requested_cpus}; detected_parallelism={detected}; cpu_tokens={cpus}");
    println!("# CPU ticks and peak RSS/threads sampled per child at ~20ms; misses short-lived work, lower bounds; no CPU/GPU enforcement implied. Thread-pool changes can alter encoded bytes; quality equivalence is not claimed.");
    eprintln!(
        "cgroup={:?}; affinity={:?}",
        fs::read_to_string("/proc/self/cgroup"),
        fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|text| text
                .lines()
                .find(|line| line.starts_with("Cpus_allowed_list:"))
                .map(str::to_owned))
    );
    println!("# device models only: GPU cpu tokens={}, upscale cpu tokens={}; GPU execution enabled={gpu}; upscale execution not exercised", Work::Gpu.cpu(cpus), Work::Upscale.cpu(cpus));
    generated_fixture(&scratch.0.join("input.mkv"))?;
    println!("sample\tarm\twork\tqueue_ms\thelper_ms\tfirst_16k_ms\tcpu_ticks\tpeak_threads\tpeak_rss_kb\tobservations");
    for sample in 0..samples {
        // Rotating order reduces systematic thermal/order bias.
        for index in 0..3 {
            run_trial(
                &scratch.0,
                sample,
                [Arm::Automatic, Arm::Threads, Arm::Resources][(sample + index) % 3],
                cpus,
                gpu,
            )?;
        }
    }
    let viewer = Viewer {
        position: 120.0,
        rate: 2.0,
        paused: false,
        lease_end: 30.0,
    };
    println!("# pacing metadata model: fixed 1x loses a 30s lead after 30s at 2x; demand end={}; all paused end={}; no real-helper pacing or native Safari validation", policy::desired_end(&[viewer], 0.0, 150.0, false), policy::desired_end(&[Viewer { paused: true, ..viewer }], 0.0, 150.0, false));
    Ok(())
}

// Signal handlers run on the async thread while all process/filesystem work
// stays on the blocking worker. Cancellation reaches every owned helper and
// the scheduler joins its workers before Scratch removes generated artifacts.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let signals = tokio::spawn(async move {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
        CANCELLED.cancel();
    });
    let result = tokio::task::spawn_blocking(run)
        .await
        .map_err(|error| error.to_string())?;
    signals.abort();
    result
}
