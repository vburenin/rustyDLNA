//! Opt-in P08 transfer experiment. Never selected by the daemon.
//! cargo run --locked -p rusty-dlna --example gpu_graph -- [samples] [seconds]
//! JSON lines on stdout; disposable generated media is removed on exit.

use rusty_dlna_helper::{
    CancellationToken, CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome,
};
use rusty_dlna_transcode::{
    AudioAction, AudioCodec, BrowserEncodingPreset, BrowserOutputOptions, BrowserQuality, Decision,
    HardwareDecode, HdrKind, RecodeAction, TranscodePlan, VideoCodec,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static CANCEL: std::sync::LazyLock<CancellationToken> =
    std::sync::LazyLock::new(CancellationToken::default);
const DEADLINE: Duration = Duration::from_secs(180);

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn strings(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

// Every helper is serial except the explicitly bounded optional GPU observer.
// Both use null stdin, private process groups, bounded capture, deadlines and
// complete reaping through the same supervisor used by media jobs.
fn run_helper(
    program: &str,
    args: &[OsString],
    output: Option<&Path>,
    stop: Option<&CancellationToken>,
) -> Result<Value, String> {
    let started = Instant::now();
    let mut command = Command::new(program);
    command.args(args);
    let mut cpu_ticks = 0u64;
    let mut rss = 0u64;
    let mut threads = 0u64;
    let mut read_bytes = 0u64;
    let mut write_bytes = 0u64;
    let mut first_bytes = None;
    let mut observations = 0;
    let result = SupervisedCommand::new(&mut command)
        .capture_stdout(CaptureConfig::new(2 * 1024 * 1024, CaptureRetention::Tail))
        .capture_stderr(CaptureConfig::new(64 * 1024, CaptureRetention::Tail))
        .run_until(started + DEADLINE, Duration::from_millis(20), || {
            if CANCEL.is_cancelled() || stop.is_some_and(CancellationToken::is_cancelled) {
                return ControlFlow::Break("cancelled");
            }
            if output.is_some_and(|path| {
                path.metadata()
                    .is_ok_and(|meta| meta.len() > 512 * 1024 * 1024)
            }) {
                return ControlFlow::Break("512 MiB output limit");
            }
            observations += 1;
            if first_bytes.is_none()
                && output.is_some_and(|path| path.metadata().is_ok_and(|meta| meta.len() >= 16384))
            {
                first_bytes = Some(started.elapsed().as_secs_f64() * 1000.0);
            }
            // This thread's children only; excludes the GPU observation thread.
            if let Ok(children) = fs::read_to_string("/proc/thread-self/children") {
                for pid in children.split_whitespace() {
                    if !pid.bytes().all(|b| b.is_ascii_digit()) {
                        continue;
                    }
                    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                        if let Some((_, tail)) = stat.rsplit_once(") ") {
                            let values: Vec<_> = tail.split_whitespace().collect();
                            let number = |i: usize| {
                                values
                                    .get(i)
                                    .and_then(|s| s.parse::<u64>().ok())
                                    .unwrap_or(0)
                            };
                            cpu_ticks = cpu_ticks.max(number(11).saturating_add(number(12)));
                            threads = threads.max(number(17));
                        }
                    }
                    if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
                        if let Some(kb) = status.lines().find_map(|line| {
                            line.strip_prefix("VmRSS:")?
                                .split_whitespace()
                                .next()?
                                .parse::<u64>()
                                .ok()
                        }) {
                            rss = rss.max(kb);
                        }
                    }
                    if let Ok(io) = fs::read_to_string(format!("/proc/{pid}/io")) {
                        let number = |key: &str| {
                            io.lines()
                                .find_map(|line| line.strip_prefix(key)?.trim().parse::<u64>().ok())
                                .unwrap_or(0)
                        };
                        read_bytes = read_bytes.max(number("read_bytes:"));
                        write_bytes = write_bytes.max(number("write_bytes:"));
                    }
                }
            }
            ControlFlow::Continue(())
        })
        .map_err(|error| error.to_string())?;
    let (status, stdout, stderr) = match result {
        SupervisedOutcome::Exited(result) => {
            (result.status.to_string(), result.stdout, result.stderr)
        }
        SupervisedOutcome::Deadline { stdout, stderr } => ("deadline".into(), stdout, stderr),
        SupervisedOutcome::Stopped {
            reason,
            stdout,
            stderr,
        } => (reason.into(), stdout, stderr),
        SupervisedOutcome::NotStarted { reason } => (reason.into(), Vec::new(), Vec::new()),
    };
    Ok(
        json!({"program":program, "argv":args.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>(), "status":status,
        "wall_ms":started.elapsed().as_secs_f64()*1000.0, "first_16k_ms":first_bytes, "cpu_ticks_sampled":cpu_ticks,
        "peak_rss_kib_sampled":rss, "peak_threads_sampled":threads, "read_bytes_sampled":read_bytes, "write_bytes_sampled":write_bytes,
        "observations":observations, "output_bytes":output.and_then(|p| p.metadata().ok()).map(|m| m.len()),
        "stdout":String::from_utf8_lossy(&stdout), "stderr":String::from_utf8_lossy(&stderr)}),
    )
}

fn success(result: &Value) -> Result<(), String> {
    if result["status"] == "exit status: 0" {
        Ok(())
    } else {
        Err(result.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Graph {
    Software,
    Download,
    Resident,
}
impl Graph {
    fn name(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Download => "cuda_download",
            Self::Resident => "cuda_resident",
        }
    }
}

fn recipe(
    input: &Path,
    output: &Path,
    graph: Graph,
    preset: BrowserEncodingPreset,
) -> Result<Vec<OsString>, String> {
    let plan = TranscodePlan {
        decision: Decision::Recode,
        action: RecodeAction::Browser,
        rule: None,
        keep_hdr10: false,
        drop_dolby_vision: false,
        video_encoder: "h264_nvenc".into(),
        hardware_decode: if graph == Graph::Software {
            HardwareDecode::None
        } else {
            HardwareDecode::Cuda
        },
        audio: AudioAction::Copy,
        container: "mp4",
        audio_index: 0,
        browser_quality: Some(BrowserQuality::DataSaver),
        browser_ai_upscale: None,
        download_audio: None,
    };
    let mut args = rusty_dlna_transcode::browser_ffmpeg_os_args(
        input,
        output,
        &plan,
        BrowserOutputOptions {
            encoding_preset: preset,
            source_video: Some(VideoCodec::Hevc),
            selected_audio: AudioCodec::Aac,
            source_hdr: HdrKind::Sdr,
            start_seconds: 0,
            hls: false,
        },
    );
    if args.first().is_none_or(|arg| arg != "ffmpeg") {
        return Err("production command prefix changed; review experiment".into());
    }
    args.remove(0);
    if graph == Graph::Resident {
        let index = args
            .iter()
            .position(|arg| arg == "-vf")
            .ok_or("missing filter")?
            + 1;
        let filter = args[index].to_str().ok_or("non-UTF-8 generated filter")?;
        let resident = filter
            .strip_suffix(",hwdownload,format=yuv420p")
            .ok_or("production filter changed; review experiment")?;
        args[index] = resident.into();
    }
    Ok(args)
}

fn context_change(scratch: &Path) -> Result<(), String> {
    let mut joined = Vec::new();
    for explicit in [false, true] {
        let path = scratch.join(if explicit {
            "explicit.hevc"
        } else {
            "implicit.hevc"
        });
        let mut args = strings(&[
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=24",
            "-t",
            "2",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "pools=2:frame-threads=2:log-level=error:repeat-headers=1",
            "-g",
            "24",
        ]);
        if explicit {
            args.extend(strings(&[
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
            ]));
        }
        args.push(path.clone().into());
        let result = run_helper("ffmpeg", &args, Some(&path), None)?;
        println!(
            "{}",
            json!({"event":"context_fixture","explicit_color":explicit,"result":result})
        );
        success(&result)?;
        joined.extend(fs::read(path).map_err(|e| e.to_string())?);
    }
    let input = scratch.join("context-change.hevc");
    fs::write(&input, joined).map_err(|e| e.to_string())?;
    println!(
        "{}",
        json!({"event":"context_contract","input":"generated HEVC8; implicit to explicit BT709 SPS at frame48 of96","timing":"Raw test stream forced to24fps; video only; this counterexample does not validate container or A/V timing","expectation":"Download must decode all96frames. Resident failure is recorded as experimental no-go; success alone does not establish broad compatibility."})
    );
    for graph in [Graph::Download, Graph::Resident] {
        let output = scratch.join(format!("{}.mp4", graph.name()));
        let mut args = recipe(&input, &output, graph, BrowserEncodingPreset::Balanced)?;
        let i = args
            .iter()
            .position(|arg| arg == "-i")
            .ok_or("missing input")?;
        args.splice(i..i, strings(&["-r", "24"]));
        let result = run_helper("ffmpeg", &args, Some(&output), None)?;
        println!(
            "{}",
            json!({"event":"context_measurement","graph":graph.name(),"result":result})
        );
        if graph == Graph::Download {
            success(&result)?;
        }
        if success(&result).is_ok() {
            let mut decode = strings(&["-nostdin", "-v", "error", "-i"]);
            decode.push(output.into());
            decode.extend(strings(&["-map", "0:v", "-f", "framemd5", "-"]));
            let result = run_helper("ffmpeg", &decode, None, None)?;
            println!(
                "{}",
                json!({"event":"context_pixels","graph":graph.name(),"result":result})
            );
            success(&result)?;
            let frames = result["stdout"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .count();
            if frames != 96 {
                return Err(format!("{} decoded {frames}/96 frames", graph.name()));
            }
        }
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--help") {
        println!("gpu_graph [samples 1..20, default 5] [generated seconds 2..120, default 8]\ngpu_graph --context-change\nOpt-in HEVC8 SDR/AAC 1920x1080/24 to H264/AAC 1280x720 matrix. Rotates software/CUDA download/CUDA resident and all browser presets. The separate context mode exercises 96 generated HEVC8 frames with a mid-stream color/SPS transition. JSONL report on stdout. Uses FFmpeg on PATH. No daemon defaults change. Temporary fixtures removed. Each helper has a 180s deadline and 512 MiB output ceiling. Run P01 playback-benchmark separately for actual browser first frames.");
        return Ok(());
    }
    if args.len() > 2 {
        return Err("expected samples and seconds; see --help".into());
    }
    let context = args.as_slice() == ["--context-change"];
    let samples: usize = args
        .first()
        .filter(|_| !context)
        .map_or(Ok(5), |s| s.parse())
        .map_err(|e| format!("{e}"))?;
    let seconds: usize = args
        .get(1)
        .map_or(Ok(8), |s| s.parse())
        .map_err(|e| format!("{e}"))?;
    if !(1..=20).contains(&samples) || !(2..=120).contains(&seconds) {
        return Err("out of bounds; see --help".into());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "rustydlna-gpu-graph-{}-{stamp}",
        std::process::id()
    )));
    fs::create_dir(&scratch.0).map_err(|e| e.to_string())?;
    if context {
        return context_change(&scratch.0);
    }
    println!(
        "{}",
        json!({"event":"environment", "samples":samples,"seconds":seconds,"frame_rate":24,"input":"generated HEVC8 SDR/AAC 1920x1080","output":"H264 SDR 1280x720/AAC copied, Data saver 3Mbps cap", "cache":"fresh output every run; source OS page cache warm; no browser/server cache","limits":{"helpers":2,"deadline_seconds":180,"output_bytes":536870912}, "affinity":fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("Cpus_allowed_list:")).map(str::to_owned)),"cgroup":fs::read_to_string("/proc/self/cgroup").ok(),"limitations":["First 16KiB is not a complete fragment or a presented frame.","Whole-device nvidia-smi polled every 100ms includes unrelated clients; utilization uses the driver's own averaging window. PCIe byte attribution needs separate profiling.","Process accounting is a sampled lower bound and excludes short-lived work.","Software scaling uses fast_bilinear; CUDA uses its existing scaling kernel. Decoded-frame hashes and SSIM against the CUDA reference are captured for sample 0 of each graph/preset only. Successful helper exits do not establish frame or quality equivalence; compare the captured measurements separately.","No physical display, HDR, Dolby Vision, changing SPS, VFR, or device-loss acceptance is established by this generated SDR matrix."]})
    );
    for (program, args) in [
        ("ffmpeg", strings(&["-version"])),
        (
            "nvidia-smi",
            strings(&[
                "--query-gpu=name,driver_version,memory.total",
                "--format=csv,noheader",
            ]),
        ),
        ("getconf", strings(&["CLK_TCK"])),
    ] {
        println!(
            "{}",
            json!({"event":"inventory","result":run_helper(program,&args,None,None)?})
        );
    }
    let input = scratch.0.join("input.mkv");
    let mut generate = strings(&[
        "-nostdin",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1920x1080:rate=24",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        &seconds.to_string(),
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        "-x265-params",
        "pools=2:frame-threads=2:log-level=error",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        "-color_range",
        "tv",
        "-colorspace",
        "bt709",
        "-color_trc",
        "bt709",
        "-color_primaries",
        "bt709",
        "-g",
        "24",
        "-c:a",
        "aac",
        "-threads:a",
        "1",
    ]);
    generate.push(input.clone().into());
    let generated = run_helper("ffmpeg", &generate, Some(&input), None)?;
    println!("{}", json!({"event":"fixture","result":generated}));
    success(&generated)?;
    let probe = run_helper(
        "ffprobe",
        &[
            strings(&[
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
            ]),
            vec![input.clone().into()],
        ]
        .concat(),
        None,
        None,
    )?;
    println!("{}", json!({"event":"input_probe","result":probe}));
    success(&probe)?;
    // This lossless reference uses the exact CUDA resize/format path whose
    // download is removed by the resident experiment. Generated media only.
    let reference = scratch.0.join("reference.mkv");
    let mut reference_args = strings(&[
        "-nostdin",
        "-v",
        "error",
        "-hwaccel",
        "cuda",
        "-hwaccel_output_format",
        "cuda",
        "-i",
    ]);
    reference_args.push(input.clone().into());
    reference_args.extend(strings(&[
        "-vf",
        "scale_cuda=w=1280:h=720:format=yuv420p,hwdownload,format=yuv420p",
        "-an",
        "-c:v",
        "ffv1",
        "-threads",
        "2",
    ]));
    reference_args.push(reference.clone().into());
    let generated = run_helper("ffmpeg", &reference_args, Some(&reference), None)?;
    println!("{}", json!({"event":"reference","result":generated}));
    success(&generated)?;
    let mut failures = 0;
    for sample in 0..samples {
        for offset in 0..9 {
            if CANCEL.is_cancelled() {
                return Err("cancelled".into());
            }
            let arm = (sample + offset) % 9;
            let graph = [Graph::Software, Graph::Download, Graph::Resident][arm % 3];
            let preset = BrowserEncodingPreset::ALL[arm / 3];
            let output = scratch.0.join("output.mp4");
            if output.exists() {
                fs::remove_file(&output).map_err(|e| e.to_string())?;
            }
            let args = recipe(&input, &output, graph, preset)?;
            let gpu_stop = CancellationToken::default();
            let (measurement, gpu) = std::thread::scope(|scope| {
                let stop = &gpu_stop;
                let gpu = scope.spawn(move || {
                    run_helper(
                        "nvidia-smi",
                        &strings(&["--query-gpu=timestamp,utilization.gpu,utilization.memory,utilization.encoder,utilization.decoder,memory.used", "--format=csv,noheader,nounits", "-lms", "100"]),
                        None,
                        Some(stop),
                    )
                });
                let measurement = run_helper("ffmpeg", &args, Some(&output), None);
                gpu_stop.cancel();
                (measurement, gpu.join())
            });
            let measurement = measurement?;
            println!(
                "{}",
                json!({"event":"measurement","sample":sample,"graph":graph.name(),"preset":preset.id(),"result":measurement,"gpu":gpu.map_err(|_|"GPU observer panicked".to_string()).and_then(|r|r).unwrap_or_else(|e|json!({"unavailable":e}))})
            );
            if success(&measurement).is_err() {
                failures += 1;
                continue;
            }
            if sample == 0 {
                let mut probe = strings(&[
                    "-v",
                    "error",
                    "-show_streams",
                    "-show_format",
                    "-of",
                    "json",
                ]);
                probe.push(output.clone().into());
                let mut hash = strings(&["-nostdin", "-v", "error", "-i"]);
                hash.push(output.clone().into());
                hash.extend(strings(&["-map", "0:v:0", "-an", "-f", "framemd5", "-"]));
                let mut quality = strings(&["-nostdin", "-v", "info", "-i"]);
                quality.push(output.clone().into());
                quality.extend(strings(&["-i"]));
                quality.push(reference.clone().into());
                quality.extend(strings(&[
                    "-filter_complex",
                    "[0:v]setpts=PTS-STARTPTS[o];[1:v]setpts=PTS-STARTPTS[r];[o][r]ssim",
                    "-an",
                    "-f",
                    "null",
                    "-",
                ]));
                for (kind, program, args) in [
                    ("output_probe", "ffprobe", probe),
                    ("decoded_frames", "ffmpeg", hash),
                    ("ssim_cuda_reference", "ffmpeg", quality),
                ] {
                    let result = run_helper(program, &args, None, None)?;
                    println!(
                        "{}",
                        json!({"event":kind,"graph":graph.name(),"preset":preset.id(),"result":result})
                    );
                    success(&result)?;
                }
            }
        }
    }
    if failures > 0 {
        return Err(format!(
            "{failures} failed graph trials; see JSONL diagnostics"
        ));
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let signals = tokio::spawn(async move {
        tokio::select! {_=interrupt.recv()=>{},_=terminate.recv()=>{}}
        CANCEL.cancel();
    });
    let result = tokio::task::spawn_blocking(run)
        .await
        .map_err(|e| e.to_string())?;
    signals.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resident_changes_only_the_transfer_boundary() {
        for preset in BrowserEncodingPreset::ALL {
            let download = recipe(
                Path::new("in.mkv"),
                Path::new("out.mp4"),
                Graph::Download,
                preset,
            )
            .unwrap();
            let resident = recipe(
                Path::new("in.mkv"),
                Path::new("out.mp4"),
                Graph::Resident,
                preset,
            )
            .unwrap();
            let changed: Vec<_> = download
                .iter()
                .zip(&resident)
                .filter(|(a, b)| a != b)
                .collect();
            assert_eq!(download.len(), resident.len());
            assert_eq!(changed.len(), 1);
            assert_eq!(
                changed[0].0.to_str().unwrap(),
                format!(
                    "{},hwdownload,format=yuv420p",
                    changed[0].1.to_str().unwrap()
                )
            );
        }
    }
    #[test]
    fn cancellation_reaps_the_process_group() {
        let token = CancellationToken::default();
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(80));
                token.cancel();
            });
            let result = run_helper(
                "sh",
                &strings(&["-c", "sleep 60 & wait"]),
                None,
                Some(&token),
            )
            .unwrap();
            assert_eq!(result["status"], "cancelled");
        });
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
