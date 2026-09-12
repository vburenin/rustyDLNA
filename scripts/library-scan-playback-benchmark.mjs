#!/usr/bin/env node
// Presented-frame startup while a private scanner stage admits generated files.
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { cpus, loadavg, release, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const options = {};
for (const argument of process.argv.slice(2)) {
  if (argument === "--help") { options.help = true; continue; }
  const match = /^--(binary|output|samples|files|port|ssdp-port|scan-delay-ms)=(.+)$/.exec(argument);
  if (!match) throw new Error(`Unknown benchmark option: ${argument}`);
  options[match[1]] = match[2];
}
if (options.help) {
  console.log("node scripts/library-scan-playback-benchmark.mjs --binary=target/release/rusty-dlna --output=/tmp/scan-playback.json [--samples=5 --files=4000 --port=18244 --ssdp-port=11944 --scan-delay-ms=500]\nGenerates an H264/AAC MP4 and seeds its catalog, then adds a flat directory of deterministic scan fixtures. Each trial starts a fresh server from the seed database and browser context. Uses existing playbackTimingSnapshot presented-frame instrumentation. Reports observed scan overlap, CPU/RSS and every sample; no tail percentiles.");
  process.exit(0);
}
const samples = Number(options.samples || 5);
const files = Number(options.files || 4000);
const port = Number(options.port || 18244);
const ssdpPort = Number(options["ssdp-port"] || 11944);
const scanDelayMs = Number(options["scan-delay-ms"] ?? 500);
if (!Number.isInteger(samples) || samples < 1 || samples > 20
  || !Number.isInteger(files) || files < 1000 || files > 50000
  || !Number.isInteger(scanDelayMs) || scanDelayMs < 0 || scanDelayMs > 10000
  || ![port, ssdpPort].every((value) => Number.isInteger(value) && value >= 1024 && value <= 65535)
  || [8200, 18201, 18240].includes(port) || [1900, 11901, 11940].includes(ssdpPort)) {
  throw new Error("Use 1-20 samples, 1000-50000 files, and isolated valid ports");
}
const binary = resolve(options.binary || join(root, "target/release/rusty-dlna"));
const run = await mkdtemp(join(tmpdir(), "rustydlna-scan-playback-"));
const output = resolve(options.output || join(tmpdir(), `rustydlna-scan-playback-${Date.now()}.json`));
const library = join(run, "library");
const base = `http://127.0.0.1:${port}`;
const pause = (ms) => new Promise((done) => setTimeout(done, ms));
const command = (name, args) => execFileSync(name, args, { encoding: "utf8", timeout: 120000, maxBuffer: 4 * 1024 * 1024 }).trim();
const clockTicks = Number(command("getconf", ["CLK_TCK"]));
const hashFile = async (path) => {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
};
const report = {
  schema: 1, started: new Date().toISOString(), binary, binary_sha256: await hashFile(binary),
  environment: { rust: command("rustc", ["--version"]), node: process.version,
    ffmpeg: command("ffmpeg", ["-version"]).split("\n")[0], cpu: cpus()[0]?.model,
    logical_cpus: cpus().length, kernel: release(), host_load_average_start: loadavg() },
  workload: { samples, files, shape: "flat", scanner_workers: 16, scan_delay_ms: scanDelayMs, http_port: port, ssdp_port: ssdpPort,
    playback: "Generated 640x360 24fps H264/AAC MP4; original delivery; linked item startup",
    cache_conditions: "Fresh server/database stage and browser context each trial. Identical seed catalog has one playable item; scan fixtures are warm in OS cache after generation. No privileged page-cache drop." },
  limitations: ["Shared developer host; unrelated work is recorded by load average and is not controlled.",
    "CPU and RSS are the server process only; helper/browser CPU and aggregate descendant memory are not included.",
    "Peak RSS is the kernel process high-water mark through startup; CPU window starts immediately before navigation and ends after the first-frame observation.",
    "Scan overlap requires an active scanner phase both before navigation and at the presented-frame observation. Trials failing this check remain in the report and are excluded from the overlap summary.",
    "This exercises playback from an existing published catalog during a private initial scan, not playback of a not-yet-published new item. The scanner is cancelled and reaped after each frame; scan-completion latency is measured by the separate large-library benchmark."],
  trials: [],
};
let server;
let browser;
let logs = "";
async function configFor(label, database) {
  const config = join(run, `${label}.toml`);
  await writeFile(config, `friendly_name = "scan playback benchmark"\nmedia_dir = [${JSON.stringify(library)}]\ncache_dir = ${JSON.stringify(join(run, `${label}-cache`))}\ndb_dir = ${JSON.stringify(database)}\nlisten_ip = "127.0.0.1"\nadvertise_ip = "127.0.0.1"\nthumbnails = false\nsubtitles = false\nscan_workers = 16\nhelper_max_jobs = 16\nhelper_queue_capacity = 4096\nrescan_secs = 0\n`);
  return config;
}
async function start(config) {
  logs = "";
  server = spawn(binary, ["--config", config, "-p", String(port)], {
    detached: true, stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, RUSTY_DLNA_SSDP_PORT: String(ssdpPort), RUST_LOG: "rusty_dlna=info" },
  });
  for (const stream of [server.stdout, server.stderr]) stream.on("data", (chunk) => {
    logs = (logs + chunk.toString()).slice(-256 * 1024);
  });
  server.on("error", (error) => { logs += `\n${error.message}`; });
  for (let attempt = 0; attempt < 1200; attempt++) {
    if (server.exitCode !== null) throw new Error(`Server exited during startup: ${logs.slice(-4000)}`);
    try { const status = await serverStatus(); if (status.catalog?.physical_inodes >= 1) return status; }
    catch { /* The listener or initial stored catalog is not ready yet. */ }
    await pause(50);
  }
  throw new Error(`Server startup timed out: ${logs.slice(-4000)}`);
}
async function stop() {
  if (!server || server.exitCode !== null) return;
  const child = server;
  const exited = new Promise((done) => child.once("exit", done));
  process.kill(-child.pid, "SIGTERM");
  const escalation = setTimeout(() => { try { process.kill(-child.pid, "SIGKILL"); } catch { /* Reaped. */ } }, 15000);
  await exited;
  clearTimeout(escalation);
  server = null;
}
async function serverStatus() {
  const response = await fetch(`${base}/api/status`, { signal: AbortSignal.timeout(2000) });
  if (!response.ok) throw new Error(`Status HTTP ${response.status}`);
  return response.json();
}
async function processSample() {
  const [stat, status] = await Promise.all([
    readFile(`/proc/${server.pid}/stat`, "utf8"), readFile(`/proc/${server.pid}/status`, "utf8"),
  ]);
  const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
  return { cpu_ms: (Number(fields[11]) + Number(fields[12])) * 1000 / clockTicks,
    rss_bytes: Number(status.match(/^VmRSS:\s+(\d+)/m)?.[1] || 0) * 1024,
    peak_rss_bytes: Number(status.match(/^VmHWM:\s+(\d+)/m)?.[1] || 0) * 1024 };
}
function active(status) { return ["initializing", "periodic-reconcile", "publishing"].includes(status.scanner?.phase); }
function summary(values) {
  if (!values.length) return { count: 0 };
  const sorted = [...values].sort((left, right) => left - right);
  const mean = values.reduce((sum, value) => sum + value, 0) / values.length;
  return { count: values.length, samples_ms: values, median_ms: (sorted[Math.floor((sorted.length - 1) / 2)] + sorted[Math.floor(sorted.length / 2)]) / 2,
    mean_ms: mean, min_ms: sorted[0], max_ms: sorted.at(-1),
    sample_stdev_ms: values.length > 1 ? Math.sqrt(values.reduce((sum, value) => sum + (value - mean) ** 2, 0) / (values.length - 1)) : null };
}
try {
  await mkdir(library);
  const playable = join(library, "playback.mp4");
  execFileSync("ffmpeg", ["-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=11", "-t", "12", "-c:v", "libx264", "-threads", "2", "-preset", "ultrafast", "-crf", "20", "-pix_fmt", "yuv420p", "-g", "48", "-c:a", "aac", "-ac", "2", "-fflags", "+bitexact", "-flags:v", "+bitexact", "-flags:a", "+bitexact", "-movflags", "+faststart", playable], { timeout: 120000, stdio: ["ignore", "ignore", "pipe"] });
  const template = join(root, "testdata/library/video/movie.mkv");
  report.workload.playback_sha256 = await hashFile(playable);
  report.workload.scan_fixture_sha256 = await hashFile(template);
  const seed = join(run, "seed-db");
  const initial = await start(await configFor("seed", seed));
  let status = initial;
  for (let attempt = 0; status.scanner?.phase !== "watching" && attempt < 1200; attempt++) {
    await pause(50); status = await serverStatus();
  }
  if (status.scanner?.phase !== "watching") throw new Error("Seed scanner did not finish");
  const page = await fetch(`${base}/api/web/library?view=library&kind=video&limit=10`).then((response) => response.json());
  const item = page.entries.find((entry) => entry.file_name === "playback.mp4");
  if (!item) throw new Error("Playable seed item missing");
  await stop();
  await mkdir(join(library, "scan"));
  for (let start = 0; start < files; start += 32) {
    await Promise.all(Array.from({ length: Math.min(32, files - start) }, (_, offset) => {
      const index = start + offset;
      return copyFile(template, join(library, "scan", `media-${String(index).padStart(8, "0")}.mkv`));
    }));
  }
  browser = await chromium.launch({ headless: true, args: ["--autoplay-policy=no-user-gesture-required"] });
  report.environment.browser = browser.version();
  for (let trial = 0; trial < samples; trial++) {
    const database = join(run, `trial-${trial}-db`);
    await mkdir(database);
    await copyFile(join(seed, "files.db"), join(database, "files.db"));
    await start(await configFor(`trial-${trial}`, database));
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
    const page = await context.newPage();
    await page.addInitScript(() => {
      localStorage.setItem("rustydlna.stream", "original");
      localStorage.setItem("rustydlna.muted", "true");
      window.__scanPlaybackFrames = [];
      addEventListener("rustydlna-playback-timing", (event) => {
        if (window.__scanPlaybackFrames.length < 16) window.__scanPlaybackFrames.push(event.detail);
      });
    });
    await pause(scanDelayMs);
    const beforeStatus = await serverStatus();
    const before = await processSample();
    const started = performance.now();
    let outcome;
    try {
      await page.goto(`${base}/?view=video&item=${encodeURIComponent(item.id)}`, { waitUntil: "domcontentloaded" });
      await page.waitForFunction(() => window.__scanPlaybackFrames.some((frame) => frame.kind === "selection"), null, { timeout: 30000 });
      outcome = await page.evaluate(async () => {
        const timing = await import("/web/playback-timing.js");
        return timing.playbackTimingSnapshot().records.find((record) => record.kind === "selection");
      });
    } catch (error) {
      outcome = { error: error.message, player: await page.locator("#player-stage").innerText().catch(() => "unavailable") };
    }
    const wall = performance.now() - started;
    const after = await processSample();
    const afterStatus = await serverStatus();
    const record = { trial, timing: outcome, navigation_to_observation_ms: wall,
      scan_overlap: active(beforeStatus) && active(afterStatus),
      scanner_before: beforeStatus.scanner, scanner_after: afterStatus.scanner,
      server_cpu_ms: after.cpu_ms - before.cpu_ms, server_cpu_since_launch_ms: after.cpu_ms,
      server_rss_bytes: after.rss_bytes,
      server_peak_rss_bytes: after.peak_rss_bytes, host_load_average: loadavg() };
    report.trials.push(record);
    console.log(JSON.stringify(record));
    await context.close();
    await stop();
  }
  const valid = report.trials.filter((trial) => trial.scan_overlap && !trial.timing.error && !trial.timing.estimated);
  report.presented_frame_under_scan = summary(valid.map((trial) => trial.timing.duration_ms));
  report.overlap_trials = valid.length;
  report.failed_trials = report.trials.filter((trial) => trial.timing.error).length;
  report.completed = new Date().toISOString();
  if (valid.length !== samples) process.exitCode = 1;
} catch (error) {
  report.error = error.stack;
  report.server_log_tail = logs.slice(-8000);
  process.exitCode = 1;
} finally {
  await browser?.close();
  await stop();
  await mkdir(dirname(output), { recursive: true });
  await writeFile(output, JSON.stringify(report, null, 2) + "\n");
  await rm(run, { recursive: true, force: true });
  console.log(`Report: ${output}`);
}
