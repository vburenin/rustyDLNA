#!/usr/bin/env node
// Generated-media, real-browser benchmark. Reports and fixtures stay outside Git.
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { mkdtemp, mkdir, readFile, writeFile, readdir, stat, rm, copyFile, open } from "node:fs/promises";
import { tmpdir, cpus, totalmem, freemem, loadavg, release } from "node:os";
import { resolve, join, dirname, extname, basename } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";
import { summarizeRecords, compareReports } from "./playback-benchmark-summary.mjs";
import { sourceBoundedQualityProfile } from "../crates/server/web/core.js";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = Object.fromEntries(process.argv.slice(2).map((arg) => {
  const [key, ...value] = arg.replace(/^--/, "").split("=");
  return [key, value.join("=") || true];
}));
const acceptedArguments = new Set(["help", "binary", "output", "samples", "recipes", "concurrency", "size", "fps", "duration", "rate", "sustain-seconds", "compare", "median-percent", "median-ms", "p95-percent", "p95-ms", "fixture", "tier", "encoder", "port", "build-profile", "quality", "encoding-preset"]);
if (Object.entries(args).some(([key, value]) => !acceptedArguments.has(key) || (key !== "help" && value === true))) {
  throw new Error("Unknown benchmark option or missing --option=value; see --help");
}
if (args.help) {
  console.log("Preset experiments: --encoding-preset=balanced|fast_start|maximum_speed --quality=auto|uhd_high|uhd_optimized|full_hd|data_saver|sd_480|low_360. Quality follows the browser's source bounds; each validation records the requested preference and effective quality separately. Compare graph changes within the same preset; different presets can change quality and are not accepted by --compare as equivalent workloads.");
  console.log("node scripts/playback-benchmark.mjs --binary=target/debug/rusty-dlna --output=/tmp/playback.json --samples=10 --recipes=copy,audio,video,both --concurrency=1 --size=1280x720 --fps=24 --duration=40 --rate=1 --sustain-seconds=2 [--build-profile=debug|release|unknown] [--compare=/tmp/baseline.json] [--median-percent=25 --median-ms=50 --p95-percent=30 --p95-ms=100]\nOptional existing hardware/media tiers: --fixture=/path/to/35-600s-clip.mkv --tier=hdr10 --encoder=h264_nvenc (copies supplied media into the temporary library; maximum 8 GiB). Available encoders: libx264, h264_nvenc. CPU default generates SDR fixtures.");
  process.exit(0);
}
const samples = Number(args.samples || 10);
const concurrency = Number(args.concurrency || 1);
const duration = Number(args.duration || 40);
const fps = Number(args.fps || 24);
const rate = Number(args.rate || 1);
const size = String(args.size || "1280x720");
const recipes = args.fixture ? ["external"] : String(args.recipes || "copy").split(",");
const encoder = String(args.encoder || "libx264");
const requestedQuality = String(args.quality || "auto");
const encodingPreset = String(args["encoding-preset"] || "balanced");
const buildProfile = String(args["build-profile"] || "unknown");
const sustainSeconds = Number(args["sustain-seconds"] || 2);
if (!Number.isInteger(samples) || samples < 1 || samples > 1000
  || ![1, 2, 4].includes(concurrency) || ![24, 30, 60].includes(fps)
  || ![1, 2].includes(rate) || !/^\d{2,4}x\d{2,4}$/.test(size)
  || !Number.isFinite(duration) || duration < 35 || duration > 600
  || !Number.isFinite(sustainSeconds) || sustainSeconds < 0.5 || sustainSeconds > 30
  || !["libx264", "h264_nvenc"].includes(encoder) || !["debug", "release", "unknown"].includes(buildProfile)
  || !["auto", "uhd_high", "uhd_optimized", "full_hd", "data_saver", "sd_480", "low_360"].includes(requestedQuality)
  || !["balanced", "fast_start", "maximum_speed"].includes(encodingPreset)
  || !recipes.every((r) => ["copy", "audio", "video", "both", ...(args.fixture ? ["external"] : [])].includes(r)) || new Set(recipes).size !== recipes.length) {
  throw new Error("Invalid bounded benchmark options; see --help");
}
const run = await mkdtemp(join(tmpdir(), "rustydlna-playback-bench-"));
const binary = resolve(args.binary || join(root, "target/debug/rusty-dlna"));
const output = resolve(args.output || join(run, "report.json"));
const command = (name, argv) => {
  try { return execFileSync(name, argv, { encoding: "utf8", timeout: 120_000, maxBuffer: 4 * 1024 * 1024 }).trim(); }
  catch (error) { return `unavailable: ${error.message}`; }
};
const optional = async (path) => readFile(path, "utf8").then((s) => s.trim()).catch(() => "unavailable");
async function cgroupLimits() {
  const membership = await optional("/proc/self/cgroup");
  const relative = membership.split("\n").find((line) => line.startsWith("0::"))?.slice(3);
  const hierarchy = [];
  if (relative?.startsWith("/")) {
    const mount = "/sys/fs/cgroup";
    let current = resolve(mount, `.${relative}`);
    for (let depth = 0; depth < 32 && (current === mount || current.startsWith(`${mount}/`)); depth += 1) {
      hierarchy.push({ cpu_max: await optional(join(current, "cpu.max")),
        memory_max: await optional(join(current, "memory.max")),
        cpuset_cpus_effective: await optional(join(current, "cpuset.cpus.effective")) });
      if (current === mount) break;
      current = dirname(current);
    }
  }
  const cpu = hierarchy.map((level) => level.cpu_max).filter((value) => value !== "unavailable");
  const memory = hierarchy.map((level) => level.memory_max).filter((value) => value !== "unavailable");
  const quotas = cpu.filter((value) => !value.startsWith("max ")).map((value) => {
    const [quota, period] = value.split(/\s+/).map(Number);
    return quota / period;
  }).filter(Number.isFinite);
  const memoryBytes = memory.filter((value) => value !== "max").map(Number).filter(Number.isFinite);
  return { cgroup_ancestor_limits: hierarchy,
    cpu_max: quotas.length ? Math.min(...quotas) : cpu.length ? "unlimited" : "unavailable",
    memory_max: memoryBytes.length ? Math.min(...memoryBytes) : memory.length ? "unlimited" : "unavailable",
    cgroup_limit_units: "Effective minimum CPU cores and memory bytes across current cgroup plus up to 31 parents; ancestor values run current-to-root. Missing controller files are recorded as unavailable." };
}
const clockTicks = Number(command("getconf", ["CLK_TCK"]));
const pageSize = Number(command("getconf", ["PAGESIZE"]));
const ffmpegIdentity = await stat(command("which", ["ffmpeg"]));
if (!(clockTicks > 0 && pageSize > 0)) throw new Error("Linux process accounting constants unavailable");
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
async function fileSha256(path) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}
const report = {
  schema: 2, started: new Date().toISOString(), binary, binary_sha256: await fileSha256(binary), root_commit: command("git", ["-C", root, "rev-parse", "HEAD"]),
  dirty: command("git", ["-C", root, "status", "--porcelain"]),
  environment: {
    rust: command("rustc", ["--version"]), node: process.version,
    ffmpeg: command("ffmpeg", ["-version"]), ffprobe: command("ffprobe", ["-version"]),
    kernel: release(), cpu: cpus()[0]?.model, logical_cpus: cpus().length, memory_bytes: totalmem(),
    cpu_reported_mhz: cpus().map((cpu) => cpu.speed),
    free_memory_bytes_start: freemem(), host_load_average_start: loadavg(),
    cpu_clock_ticks_per_second: clockTicks, page_size_bytes: pageSize,
    ffmpeg_file_identity: { device: ffmpegIdentity.dev, inode: ffmpegIdentity.ino },
    ...await cgroupLimits(),
    process_affinity_cpus: (await optional("/proc/self/status")).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1] ?? "unavailable",
    filesystem: command("findmnt", ["-T", run, "-o", "SOURCE,FSTYPE,OPTIONS", "-n"]),
    gpu_inventory: command("nvidia-smi", ["--query-gpu=name,driver_version,memory.total", "--format=csv,noheader"]),
    cache_conditions: "Generated temporary fixtures; OS page cache warm from generation; no privileged cache drop. Cold means empty derived-output cache and freshly started server/tool cache; warm means structurally completed derived output. Fresh browser context per startup.",
    unmeasured_tiers: ["Dedicated GPU decode/encode/compute/VRAM telemetry", "P7/P8 conversion", "HDR10/HLG/Dolby Vision presentation", "Physical device rendering", "Cold storage/page cache", "Concurrent scanner/artwork workload"],
    unavailable_tiers: [],
  },
  configuration: { samples, concurrency, duration, fps, rate, size, recipes, encoder, build_profile: buildProfile, tier: String(args.tier || (args.fixture ? "external" : "cpu-sdr")), sustain_seconds: sustainSeconds, quality: requestedQuality, encoding_preset: encodingPreset, delivery: "MSE (Android Chromium UA; native HLS capability disabled; actual resource requests verified)" },
  limitations: ["Generated audio ends one second before video. An earlier audio-longest fixture exposed a selected-track-tail indexing failure; this CPU subset does not cover that separate case.", "Active attachment/cancellation are unavailable when a producer finishes before it can be observed; completed-job cleanup is never counted as cancellation.", "Process samples include harness, browser, server, and live descendants; short-lived children between samples can be missed.", "Shared developer host: unrelated workloads are not stopped. Host load averages and CPU frequency are recorded; they do not establish controlled hardware isolation.", "External --fixture/--tier and existing --encoder settings exercise normal browser negotiation, not an artificial forced GPU, Dolby Vision, or P7/P8 pipeline. Requested tier names are descriptive; actual output probe and browser recipe determine what ran."],
  measurement_windows: {
    resource_sampling: "sampler_wall_ms is cumulative time spent in processSnapshot sampling, not the resource window duration. Samples can overlap the operation and impose observer overhead; do not subtract this value from browser latency or wall_ms.",
    startup_latency: "Browser monotonic immediately before selection through the first presented frame. Includes negotiation and media delivery, excludes page/context/library preparation.",
    startup_resources: "Harness monotonic from context/page creation through all concurrent first frames; includes page and library preparation plus process sampling. Shared trial values must not be summed across viewers.",
    seek_latency: "Browser monotonic from timeline change dispatch through the first presented frame at the target; seeked settlement recorded separately.",
    sustained_resources: "Separate wall window after startup/validation or seek workloads. Fixture generation and FFprobe/frame-hash validation are excluded from measured windows.",
  },
  fixtures: [], records: [], validations: [], summaries: {},
};
if (report.environment.gpu_inventory.startsWith("unavailable:")) {
  report.environment.unavailable_tiers.push("NVIDIA inventory query unavailable; see gpu_inventory diagnostic. This does not prove all GPU hardware is absent.");
}
await mkdir(join(run, "library"));
for (const recipe of recipes) {
  if (recipe === "external") {
    const source = resolve(args.fixture);
    const metadata = await stat(source);
    if (!metadata.isFile() || metadata.size > 8 * 1024 ** 3) throw new Error("External fixture must be a regular file of at most 8 GiB");
    const extension = extname(source).toLowerCase();
    if (![".mkv", ".mp4", ".m4v", ".ts", ".m2ts"].includes(extension)) throw new Error("External tiers accept Matroska, MP4, or MPEG-TS fixtures");
    const path = join(run, "library", `external${extension}`);
    await copyFile(source, path);
    const probe = JSON.parse(command("ffprobe", ["-v", "error", "-protocol_whitelist", "file,pipe", "-format_whitelist", "matroska,mov,mpegts", "-show_streams", "-show_format", "-of", "json", path]));
    if (!(Number(probe.format.duration) >= 35 && Number(probe.format.duration) <= 600)) throw new Error("External fixture must contain 35-600 seconds for bounded seek workloads");
    report.fixtures.push({ recipe, path, supplied_fixture: true, sha256: await fileSha256(path), probe });
    report.configuration.external_fixture = { sha256: report.fixtures[0].sha256, duration: probe.format.duration,
      streams: probe.streams.map(({ codec_name, codec_type, width, height, pix_fmt, r_frame_rate, color_transfer, color_primaries }) =>
        ({ codec_name, codec_type, width, height, pix_fmt, r_frame_rate, color_transfer, color_primaries })) };
    continue;
  }
  const video = ["video", "both"].includes(recipe) ? "mpeg2video" : "libx264";
  const audio = ["audio", "both"].includes(recipe) ? "flac" : "aac";
  const path = join(run, "library", `${recipe}.mkv`);
  const argv = ["-nostdin", "-v", "error", "-f", "lavfi", "-i", `testsrc2=size=${size}:rate=${fps}`,
    "-f", "lavfi", "-i", `sine=frequency=440:sample_rate=48000:duration=${duration - 1}`, "-t", String(duration),
    "-c:v", video, "-threads", "2", "-g", String(fps * 2), "-bf", "2", "-pix_fmt", "yuv420p",
    ...(video === "libx264" ? ["-preset", "ultrafast", "-crf", "20"] : ["-q:v", "3"]),
    "-c:a", audio, "-ac", "2", "-fflags", "+bitexact", "-flags:v", "+bitexact", "-flags:a", "+bitexact", path];
  execFileSync("ffmpeg", argv, { timeout: 120_000, stdio: ["ignore", "ignore", "pipe"] });
  // Original uses a browser-native MP4 with the same generated copy streams.
  if (recipe === "copy") execFileSync("ffmpeg", ["-nostdin", "-v", "error", "-i", path, "-c", "copy", "-movflags", "+faststart", join(run, "library", "original.mp4")], { timeout: 30_000 });
  report.fixtures.push({ recipe, path, sha256: await fileSha256(path), generation: ["ffmpeg", ...argv.map((s) => s === path ? "<temporary fixture>" : s)], probe: JSON.parse(command("ffprobe", ["-v", "error", "-show_streams", "-show_format", "-of", "json", path])) });
}
const port = Number(args.port || 18211);
if (!Number.isInteger(port) || port < 1024 || port > 64535 || [8200, 18201].includes(port)) throw new Error("Choose an isolated benchmark port");
const base = `http://127.0.0.1:${port}`;
const config = join(run, "server.toml");
await writeFile(config, `friendly_name = "playback-benchmark"\nmedia_dir = ["library"]\ncache_dir = "cache"\ndb_dir = "db"\nadvertise_ip = "127.0.0.1"\n[transcode]\nenable = true\nencoder = "${encoder}"\nmax_jobs = 4\n[web]\nencoder = "${encoder}"\n`);
let server;
let browser;
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
const serverLogs = [];
let serverLogBytes = 0;
async function startServer() {
  server = spawn(binary, ["-c", config, "-p", String(port)], { env: { ...process.env, RUSTY_DLNA_SSDP_PORT: String(port + 1000) }, stdio: ["ignore", "pipe", "pipe"], detached: true });
  for (const stream of [server.stdout, server.stderr]) stream.on("data", (chunk) => {
    const value = chunk.toString();
    serverLogs.push(value); serverLogBytes += Buffer.byteLength(value);
    while (serverLogBytes > 4 * 1024 * 1024) serverLogBytes -= Buffer.byteLength(serverLogs.shift());
  });
  for (let i = 0; i < 600; i++) {
    if (server.exitCode !== null) throw new Error(`Server exited: ${serverLogs.slice(-5).join("")}`);
    try {
      const response = await fetch(`${base}/api/web/library?view=library&kind=video&limit=200`);
      if (response.ok) { const data = await response.json(); if (data.entries?.length) return data; }
    } catch { /* Listener starting. */ }
    await sleep(50);
  }
  throw new Error("Server catalog readiness deadline");
}
async function stopServer() {
  if (!server || server.exitCode !== null) return;
  const child = server;
  const exited = new Promise((done) => child.once("exit", done));
  process.kill(-child.pid, "SIGTERM");
  const escalation = setTimeout(() => { try { process.kill(-child.pid, "SIGKILL"); } catch { /* Already reaped. */ } }, 20_000);
  await exited;
  clearTimeout(escalation);
}
async function processSnapshot() {
  const processes = [];
  for (const name of await readdir("/proc")) {
    if (!/^\d+$/.test(name)) continue;
    try {
      const data = await readFile(`/proc/${name}/stat`, "utf8");
      const fields = data.slice(data.lastIndexOf(")") + 2).split(" ");
      processes.push({ pid: Number(name), parent: Number(fields[1]), started: fields[19], name: data.slice(data.indexOf("(") + 1, data.lastIndexOf(")")), state: fields[0], ticks: Number(fields[11]) + Number(fields[12]), rss: Number(fields[21]) * pageSize, threads: Number(fields[17]) });
    } catch { /* Process exited between enumeration and stat. */ }
  }
  const owned = new Set([process.pid]);
  const serverOwned = new Set(server ? [server.pid] : []);
  for (let i = 0; i < 8; i++) for (const p of processes) if (owned.has(p.parent)) owned.add(p.pid);
  for (let i = 0; i < 8; i++) for (const p of processes) if (serverOwned.has(p.parent)) serverOwned.add(p.pid);
  const result = [];
  for (const p of processes.filter((p) => owned.has(p.pid))) {
    p.role = p.pid === process.pid ? "harness" : serverOwned.has(p.pid) ? "server_and_helpers" : "browser";
    const io = await optional(`/proc/${p.pid}/io`);
    p.read = Number(io.match(/^read_bytes: (\d+)/m)?.[1] || 0);
    p.write = Number(io.match(/^write_bytes: (\d+)/m)?.[1] || 0);
    result.push(p);
  }
  return result;
}
async function measured(callback) {
  const previous = new Map((await processSnapshot()).map((p) => [p.pid, p]));
  const resource = { cpu_ticks: 0, cpu_ticks_by_role: { harness: 0, server_and_helpers: 0, browser: 0 },
    peak_rss_bytes: 0, peak_threads: 0, read_bytes: 0, write_bytes: 0,
    peak_rss_bytes_by_role: { harness: 0, server_and_helpers: 0, browser: 0 },
    peak_threads_by_role: { harness: 0, server_and_helpers: 0, browser: 0 },
    read_bytes_by_role: { harness: 0, server_and_helpers: 0, browser: 0 },
    write_bytes_by_role: { harness: 0, server_and_helpers: 0, browser: 0 },
    sample_count: 0, interval_ms: 100, sampler_wall_ms: 0,
    scope: "harness + browser + server + live descendants; short-lived processes between samples may be missed" };
  let stopped = false;
  async function sample() {
    const samplingStarted = performance.now();
    const values = await processSnapshot();
    resource.sample_count++;
    resource.peak_rss_bytes = Math.max(resource.peak_rss_bytes, values.reduce((n, p) => n + p.rss, 0));
    resource.peak_threads = Math.max(resource.peak_threads, values.reduce((n, p) => n + p.threads, 0));
    for (const role of Object.keys(resource.cpu_ticks_by_role)) {
      const matching = values.filter((p) => p.role === role);
      resource.peak_rss_bytes_by_role[role] = Math.max(resource.peak_rss_bytes_by_role[role], matching.reduce((n, p) => n + p.rss, 0));
      resource.peak_threads_by_role[role] = Math.max(resource.peak_threads_by_role[role], matching.reduce((n, p) => n + p.threads, 0));
    }
    for (const p of values) {
      const last = previous.get(p.pid);
      const old = last?.started === p.started ? last : null;
      resource.cpu_ticks_by_role[p.role] += Math.max(0, p.ticks - (old?.ticks || 0));
      resource.read_bytes_by_role[p.role] += Math.max(0, p.read - (old?.read || 0));
      resource.write_bytes_by_role[p.role] += Math.max(0, p.write - (old?.write || 0));
      for (const [field, key] of [["ticks", "cpu_ticks"], ["read", "read_bytes"], ["write", "write_bytes"]]) resource[key] += Math.max(0, p[field] - (old?.[field] || 0));
      previous.set(p.pid, p);
    }
    resource.sampler_wall_ms += performance.now() - samplingStarted;
  }
  const polling = (async () => { while (!stopped) { await sample(); await sleep(100); } })();
  const started = performance.now();
  try { return { value: await callback(), wall_ms: performance.now() - started, resource }; }
  finally {
    stopped = true; await polling; await sample(); resource.cpu_seconds = resource.cpu_ticks / clockTicks;
    resource.cpu_seconds_by_role = Object.fromEntries(Object.entries(resource.cpu_ticks_by_role).map(([key, value]) => [key, value / clockTicks]));
  }
}
async function openPlayback(item, mode = "compat", { awaitFrame = true, beforeMediaRequest = null } = {}) {
  const context = await browser.newContext({ userAgent: "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36" });
  const page = await context.newPage();
  const requests = [];
  const errors = [];
  page.on("pageerror", (error) => { if (errors.length < 16) errors.push(error.message); });
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/web/media/") && requests.length < 1024) {
      const recipe = Object.fromEntries(url.searchParams);
      // Request/session IDs are internal correlation only, never dimensions.
      const generation = { request: recipe.request, session: recipe.session };
      delete recipe.request; delete recipe.session;
      requests.push({ at: performance.now(), method: request.method(), recipe, generation });
    }
  });
  if (beforeMediaRequest) {
    let first = true;
    await page.route("**/web/media/*", async (route) => {
      if (first) { first = false; await beforeMediaRequest(); }
      await route.continue();
    });
  }
  await page.addInitScript(({ mode, rate, quality, encodingPreset }) => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function(type) {
      return String(type).includes("mpegurl") ? "" : canPlayType.call(this, type);
    };
    localStorage.setItem("rustydlna.stream", mode);
    localStorage.setItem("rustydlna.rate", String(rate));
    localStorage.setItem("rustydlna.quality", quality);
    localStorage.setItem("rustydlna.encodingPreset", encodingPreset);
    const state = window.__playbackBench = { start: null, first: null, frames: 0, stages: [], records: [], sourceOffset: 0 };
    const fetchMedia = window.fetch;
    window.fetch = function(input, options) {
      const url = new URL(input instanceof Request ? input.url : String(input), location.href);
      if (url.pathname.startsWith("/web/media/") && url.searchParams.get("delivery") === "mse") state.sourceOffset = Number(url.searchParams.get("start") || 0);
      return fetchMedia.call(this, input, options);
    };
    addEventListener("rustydlna-playback-timing", (e) => { if (state.records.length < 64) state.records.push(e.detail); });
    addEventListener("DOMContentLoaded", () => {
      const video = document.querySelector("video");
      if (!video?.requestVideoFrameCallback) { state.unavailable = "requestVideoFrameCallback"; return; }
      const frame = (now, metadata) => {
        state.frames++;
        state.mediaTime = metadata.mediaTime;
        if (state.seeking && state.first === null) {
          state.seek_frames ||= [];
          if (state.seek_frames.length < 128) state.seek_frames.push({ ms: now - state.start, media_time: metadata.mediaTime,
            source_offset: state.sourceOffset, current_time: video.currentTime, ready_state: video.readyState, seeking: video.seeking });
        }
        if (state.start !== null && state.first === null && (!state.seeking
          || (Math.abs(metadata.mediaTime + state.sourceOffset - state.target) < 0.3))) {
          state.first = now - state.start;
          state.rate_at_first_frame = { playback_rate: video.playbackRate, default_playback_rate: video.defaultPlaybackRate,
            preference_rate: Number(localStorage.getItem("rustydlna.rate") || 1),
            control_rate: Number(document.querySelector("#speed-control")?.value) };
          state.seeking_at_first_frame = video.seeking;
        }
        video.requestVideoFrameCallback(frame);
      };
      video.requestVideoFrameCallback(frame);
      for (const event of ["loadstart", "loadedmetadata", "loadeddata", "canplay", "playing", "seeking", "seeked", "waiting", "error"]) video.addEventListener(event, () => { if (state.stages.length < 128) state.stages.push({ event, ms: performance.now() - state.start, media: video.currentTime }); });
    });
  }, { mode, rate, quality: requestedQuality, encodingPreset });
  await page.goto(`${base}/?view=video`);
  await page.waitForFunction(() => document.querySelectorAll(".media-card").length > 0);
  await page.evaluate((id) => {
    const button = [...document.querySelectorAll(".media-card")].find((card) => card.dataset.mediaId === String(id))?.querySelector("button");
    window.__playbackBench.start = performance.now();
    if (button) button.click();
    else [...document.querySelectorAll("button")].find((button) => button.getAttribute("aria-label")?.startsWith(`Play ${id}`))?.click();
  }, item.id);
  const playback = { page, context, requests, errors, first: null };
  if (awaitFrame) {
    try { await page.waitForFunction(() => window.__playbackBench.first !== null, null, { timeout: 45_000 }); }
    catch (error) { error.stack += ` diagnostics=${JSON.stringify(await diagnostics(playback))}`; throw error; }
    playback.first = await page.evaluate(() => structuredClone(window.__playbackBench));
    verifyDelivery(playback, mode);
    verifyRate(playback.first.rate_at_first_frame, "first presented frame");
  }
  return playback;
}
async function seek(playback, target, paused) {
  await playback.page.evaluate((paused) => {
    const video = document.querySelector("video");
    if (paused !== video.paused) document.querySelector("#play-button")?.click();
  }, paused);
  await playback.page.waitForFunction((paused) => {
    const video = document.querySelector("video");
    return video.paused === paused && document.querySelector("#play-button").getAttribute("aria-label") === (paused ? "Play" : "Pause");
  }, paused);
  await playback.page.evaluate((target) => {
    const state = window.__playbackBench;
    state.start = performance.now(); state.first = null; state.seeking = true; state.target = target; state.stages = []; state.seek_frames = [];
    const timeline = document.querySelector("#timeline");
    timeline.value = String(target);
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  }, target);
  try { await playback.page.waitForFunction(() => window.__playbackBench.first !== null, null, { timeout: 45_000 }); }
  catch (error) { error.stack += ` target=${target} paused=${paused} diagnostics=${JSON.stringify(await diagnostics(playback))}`; throw error; }
  await playback.page.waitForFunction(() => !document.querySelector("video").seeking);
  const value = await playback.page.evaluate(() => ({ ...window.__playbackBench,
    seek_settled_ms: performance.now() - window.__playbackBench.start,
    currentTime: document.querySelector("video").currentTime, paused: document.querySelector("video").paused,
    rate_at_completion: { playback_rate: document.querySelector("video").playbackRate,
      default_playback_rate: document.querySelector("video").defaultPlaybackRate,
      preference_rate: Number(localStorage.getItem("rustydlna.rate") || 1),
      control_rate: Number(document.querySelector("#speed-control")?.value) } }));
  if (value.paused !== paused) throw new Error(`Seek changed playback intent: ${JSON.stringify(value)}`);
  verifyRate(value.rate_at_first_frame, "seek presented frame");
  verifyRate(value.rate_at_completion, "seek completion");
  return value;
}
async function cacheBytes() {
  let bytes = 0;
  for (const name of await readdir(join(run, "cache")).catch(() => [])) { const s = await stat(join(run, "cache", name)); if (s.isFile()) bytes += s.size; }
  return bytes;
}

async function diagnostics(playback) {
  return playback.page.evaluate(() => ({
    state: window.__playbackBench, message: document.querySelector("#player-message-text")?.textContent,
    source: document.querySelector("video").currentSrc, time: document.querySelector("video").currentTime,
    paused: document.querySelector("video").paused,
  })).then((value) => ({ ...value, errors: playback.errors }));
}

function verifyRate(actual, phase) {
  if (actual?.playback_rate !== rate || actual.preference_rate !== rate) {
    throw new Error(`Requested playback rate ${rate} was not retained at ${phase}: ${JSON.stringify(actual)}`);
  }
}

function verifyDelivery(playback, mode) {
  const actual = playback.requests.filter((request) => request.recipe.delivery === "mse_segment");
  if (mode === "compat" && !actual.length) throw new Error("Compatible workload did not actually decode MSE resources");
  if (mode === "direct" && !playback.requests.some((request) => request.recipe.mode === "direct")) {
    throw new Error("Original workload did not request the original source");
  }
  if (playback.errors.length) throw new Error(`Browser errors: ${playback.errors.join("; ")}`);
}

async function serverStatus() {
  const response = await fetch(`${base}/api/status`, { signal: AbortSignal.timeout(5000) });
  if (!response.ok) throw new Error(`Status HTTP ${response.status}`);
  return response.json();
}

async function producerArguments(pid) {
  const file = await open(`/proc/${pid}/cmdline`, "r").catch(() => null);
  if (!file) return [];
  try {
    const buffer = Buffer.alloc(65_537);
    const { bytesRead } = await file.read(buffer, 0, buffer.length, 0);
    if (bytesRead > 65_536 || !bytesRead || buffer[bytesRead - 1] !== 0) return [];
    return buffer.subarray(0, bytesRead - 1).toString("utf8").split("\0");
  } catch { return []; } finally { await file.close(); }
}

async function producers(item) {
  const values = await processSnapshot();
  const owned = new Set([server.pid]);
  for (let i = 0; i < 8; i++) for (const value of values) if (owned.has(value.parent)) owned.add(value.pid);
  const matched = [];
  for (const value of values.filter((value) => owned.has(value.pid))) {
    // Verified helpers execute /proc/self/fd/4, so their comm can be "4".
    // Match the executable inode, confined to this benchmark server ancestry.
    const executable = await stat(`/proc/${value.pid}/exe`).catch(() => null);
    if (executable?.dev !== ffmpegIdentity.dev || executable.ino !== ffmpegIdentity.ino) continue;
    // Artwork can also use FFmpeg. Match this selected compatible artifact,
    // without retaining command lines or treating paths as metric dimensions.
    const arguments_ = await producerArguments(value.pid);
    const output = arguments_.at(-1);
    if (output && dirname(output) === join(run, "cache")
      && basename(output).startsWith(`${item.id}-web-`) && output.endsWith(".mp4.part")) {
      const option = (flag) => {
        const index = arguments_.indexOf(flag);
        const argument = index >= 0 ? arguments_[index + 1] : null;
        return argument && /^[a-z0-9_]{1,64}$/.test(argument) ? argument : null;
      };
      value.recipe = { video_encoder: option("-c:v"), audio_encoder: option("-c:a"), hardware_decode: option("-hwaccel") };
      matched.push(value);
    }
  }
  return matched;
}

async function completedArtifact(item) {
  const names = await readdir(join(run, "cache")).catch(() => []);
  for (const name of names) {
    if (!name.startsWith(`${item.id}-web-`) || !name.endsWith(".mp4")) continue;
    const path = join(run, "cache", name);
    const [media, stamp, part] = await Promise.all([
      stat(path).catch(() => null), stat(`${path}.src`).catch(() => null), stat(`${path}.part`).catch(() => null),
    ]);
    if (media?.isFile() && media.size > 0 && stamp?.isFile() && stamp.size > 0 && !part) return path;
  }
  return null;
}

async function waitForCompletedArtifact(item) {
  for (let attempt = 0; attempt < 1200; attempt++) {
    const artifact = await completedArtifact(item);
    if (artifact) return artifact;
    const status = await serverStatus();
    if (attempt > 5 && status.transcode?.active === 0 && !(await producers(item)).length) {
      throw new Error("Producer finished without a completed output and validation stamp");
    }
    await sleep(100);
  }
  throw new Error("Completed and stamped cache deadline");
}

function decodedHashes(path) {
  const value = execFileSync("ffmpeg", ["-nostdin", "-v", "error", "-threads", "2", "-i", path,
    "-map", "0:v:0", "-an", "-frames:v", String(Math.min(120, fps * 2)), "-fps_mode", "passthrough",
    "-threads", "2", "-f", "framemd5", "pipe:1"], { encoding: "utf8", timeout: 120_000, maxBuffer: 1024 * 1024 });
  return value.split("\n").filter((line) => line && !line.startsWith("#")).map((line) => line.split(",").at(-1).trim());
}

const sourceFrameHashes = new Map();
async function validateOutput(recipe, artifact, requested, sample, { item, capabilities }) {
  const fixture = report.fixtures.find((entry) => entry.recipe === recipe);
  const probe = JSON.parse(command("ffprobe", ["-v", "error", "-show_streams", "-show_format", "-of", "json", artifact]));
  const video = probe.streams.find((stream) => stream.codec_type === "video");
  const audio = probe.streams.find((stream) => stream.codec_type === "audio");
  const sourceVideo = fixture.probe.streams.find((stream) => stream.codec_type === "video");
  const videoCopy = requested.video_mode === "copy";
  if (!Array.isArray(capabilities?.quality_profiles) || !capabilities.quality_profiles.length) {
    throw new Error("Advertised quality profiles unavailable for output validation");
  }
  const effectiveQuality = sourceBoundedQualityProfile(
    capabilities.quality_profiles, requestedQuality, item, capabilities.ai_upscale,
  );
  if ((requested.quality || "auto") !== effectiveQuality
    || (!videoCopy && (requested.encoding_preset || "balanced") !== encodingPreset)) {
    throw new Error(`Expected effective quality ${effectiveQuality} from preference ${requestedQuality}, with preset ${encodingPreset}: ${JSON.stringify(requested)}`);
  }
  const expectedVideoCopy = effectiveQuality === "auto" && ["copy", "audio"].includes(recipe);
  if (recipe !== "external" && (videoCopy !== expectedVideoCopy || requested.audio_mode !== (["copy", "video"].includes(recipe) ? "copy" : "transcode"))) {
    throw new Error(`Requested recipe differs from ${recipe}: ${JSON.stringify(requested)}`);
  }
  if ((recipe !== "external" && (video?.codec_name !== "h264" || audio?.codec_name !== "aac"))
    || !video || Number(probe.format.duration) < Math.min(duration, Number(fixture.probe.format.duration)) - 1.5) {
    throw new Error(`Output validation failed: ${JSON.stringify(probe)}`);
  }
  const outputFrames = decodedHashes(artifact);
  if (!outputFrames.length) throw new Error(`Completed ${recipe} output has no decodable video frames`);
  let quality;
  if (videoCopy) {
    if (!sourceFrameHashes.has(recipe)) sourceFrameHashes.set(recipe, decodedHashes(fixture.path));
    const source = sourceFrameHashes.get(recipe);
    const output = outputFrames;
    if (!source.length || JSON.stringify(source) !== JSON.stringify(output)
      || video.width !== sourceVideo.width || video.height !== sourceVideo.height || video.pix_fmt !== sourceVideo.pix_fmt) {
      throw new Error(`Copied video changed decoded frames, size, or pixel format for ${recipe}`);
    }
    quality = { copied_video_frame_hashes_match: true, decoded_frames_compared: source.length,
      decoded_hash_sha256: sha256(source.join("\n")), scope: "First min(120, 2 * configured fps) decoded frames; external source frame rate may differ. No full-movie identity claim." };
  } else quality = { copied_video_frame_hashes_match: null,
    decoded_frames_sampled: outputFrames.length, encoded_output_decoded_hash_sha256: sha256(outputFrames.join("\n")),
    scope: "Requested profile video encode; bounded output decoded-frame hashes can verify identical before/after frames. Output codec, dimensions, pixel format, frame rate, bitrate, and color metadata are recorded. No perceptual quality score or full-movie equality inferred." };
  const validation = { id: report.validations.length, recipe, sample, requested, output_probe: probe, output_bytes: (await stat(artifact)).size,
    quality_selection: { requested: requestedQuality, effective: effectiveQuality },
    stamp_bytes: (await stat(`${artifact}.src`)).size, quality, outside_latency_measurements: true };
  report.validations.push(validation);
  return validation.id;
}

async function sustainedPlayback(player) {
  const start = await player.page.evaluate(() => ({ wall: performance.now(), media: document.querySelector("video").currentTime,
    frames: window.__playbackBench.frames, ended: document.querySelector("video").ended,
    actual_rate: { playback_rate: document.querySelector("video").playbackRate,
      default_playback_rate: document.querySelector("video").defaultPlaybackRate,
      preference_rate: Number(localStorage.getItem("rustydlna.rate") || 1),
      control_rate: Number(document.querySelector("#speed-control")?.value) },
    dropped: document.querySelector("video").getVideoPlaybackQuality?.().droppedVideoFrames ?? null }));
  verifyRate(start.actual_rate, "sustained window start");
  await sleep(sustainSeconds * 1000);
  const end = await player.page.evaluate(() => ({ wall: performance.now(), media: document.querySelector("video").currentTime,
    frames: window.__playbackBench.frames, ended: document.querySelector("video").ended,
    actual_rate: { playback_rate: document.querySelector("video").playbackRate,
      default_playback_rate: document.querySelector("video").defaultPlaybackRate,
      preference_rate: Number(localStorage.getItem("rustydlna.rate") || 1),
      control_rate: Number(document.querySelector("#speed-control")?.value) },
    dropped: document.querySelector("video").getVideoPlaybackQuality?.().droppedVideoFrames ?? null }));
  verifyRate(end.actual_rate, "sustained window end");
  return { actual_rate_start: start.actual_rate, actual_rate_end: end.actual_rate,
    wall_seconds: (end.wall - start.wall) / 1000, media_seconds: end.media - start.media,
    media_seconds_per_wall_second: (end.media - start.media) * 1000 / (end.wall - start.wall),
    presented_frames: end.frames - start.frames, requested_rate: rate, ended_at_start: start.ended, ended_at_end: end.ended,
    progression_within_tolerance: end.ended ? null : end.frames > start.frames
      && (end.media - start.media) * 1000 / (end.wall - start.wall) >= rate * 0.8,
    progression_policy: "At least one presented frame and 80% of requested media/wall rate; endpoint-limited runs are inconclusive. This is a screening threshold, not a quality score.",
    dropped_frames: end.dropped === null || start.dropped === null ? null : end.dropped - start.dropped };
}

async function bufferedRange(player, target) {
  return player.page.evaluate((target) => {
    const video = document.querySelector("video");
    const offset = window.__playbackBench.sourceOffset;
    const ranges = Array.from({ length: video.buffered.length }, (_, i) => [video.buffered.start(i), video.buffered.end(i)]);
    return { ranges, global_target: target, source_offset: offset, local_target: target - offset,
      candidate: ranges.some(([start, end]) => start <= target - offset - 2.5 && end >= target - offset + 0.5) };
  }, target);
}

async function closePlayers(players) { await Promise.all(players.map((player) => player.context.close())); }

async function measureCancellation(item, recipe, sample) {
  // Start a genuinely cold producer; never classify completed-source close as
  // cancellation. Only temporary derived artifacts are removed between trials.
  await stopServer();
  await rm(join(run, "cache"), { recursive: true, force: true });
  await startServer();
  const before = await serverStatus();
  const player = await openPlayback(item, "compat", { awaitFrame: false });
  let active = [];
  const observationStarted = performance.now();
  while (performance.now() - observationStarted < 5000) {
    active = await producers(item);
    if (active.length && (await serverStatus()).transcode?.active > 0) break;
    await sleep(20);
  }
  if (!active.length) {
    await player.context.close();
    report.records.push({ recipe, workload: "cancellation", sample, available: false,
      reason: "No active FFmpeg producer was observable within five seconds; completed-cache cleanup is not cancellation.",
      output_requests: player.requests });
    return;
  }
  const started = performance.now();
  await player.page.evaluate(() => document.querySelector("#close-player-button").click());
  let observations = 0;
  let maximumPollGap = 0;
  let lastObservation = started;
  while (performance.now() - started < 10_000) {
    // A zombie no longer has /proc/PID/exe but still needs to be reaped.
    const current = await processSnapshot();
    const observedAt = performance.now();
    maximumPollGap = Math.max(maximumPollGap, observedAt - lastObservation);
    lastObservation = observedAt;
    observations += 1;
    const stillOwned = current.some((p) => active.some((old) => p.pid === old.pid && p.started === old.started));
    const status = await serverStatus();
    if (!stillOwned && status.transcode?.active === 0) {
      const cancellationMs = performance.now() - started;
      await player.context.close();
      if (status.transcode.cancelled_total <= before.transcode.cancelled_total) {
        report.records.push({ recipe, workload: "cancellation", sample, available: false,
          reason: "The observed producer completed before the Close player cancellation took ownership" });
        return;
      }
      report.records.push({ recipe, workload: "cancellation", sample, available: true,
        cancellation_ms: cancellationMs, active_helpers_observed: active.length,
        actual_helper_recipes_observed: active.map((helper) => helper.recipe),
        helpers_reaped: true, auxiliary_helpers_active_when_reaped: status.helpers?.active ?? null,
        boundary: "Captured selected compatible MP4 producer PID/start identity reaped and isolated transcode registry inactive; unrelated artwork/probe helpers are not awaited.",
        polling_interval_ms: 25, observations, maximum_poll_gap_ms: maximumPollGap, clock: "harness monotonic from Close player click initiation" });
      return;
    }
    await sleep(25);
  }
  throw new Error("Active cancellation/helper reaping exceeded 10 seconds");
}
try {
  browser = await chromium.launch({ headless: true, args: ["--autoplay-policy=no-user-gesture-required"] });
  report.environment.browser = browser.version();
  let library = await startServer();
  for (const recipe of recipes) {
    for (let sample = 0; sample < samples; sample++) {
      await stopServer();
      await rm(join(run, "cache"), { recursive: true, force: true });
      library = await startServer();
      const item = library.entries.find((item) => item.file_name === basename(report.fixtures.find((fixture) => fixture.recipe === recipe).path));
      if (!item) throw new Error(`Generated ${recipe} fixture not admitted`);
      for (const workload of recipe === "copy" ? ["original", "cold", "warm"] : ["cold", "warm"]) {
        console.log(`${recipe} sample ${sample + 1}/${samples}: ${workload}`);
        const selected = workload === "original" ? library.entries.find((item) => item.title === "original") : item;
        const warmArtifact = workload === "warm" ? await completedArtifact(item) : null;
        if (workload === "warm" && !warmArtifact) throw new Error("Warm workload has no completed, stamped output");
        const beforeStatus = await serverStatus();
        const result = await measured(() => Promise.all(Array.from({ length: concurrency }, () => openPlayback(selected, workload === "original" ? "direct" : "compat"))));
        const players = result.value;
        const afterStatus = await serverStatus();
        const cacheReuses = (afterStatus.transcode?.web_player?.cache_reuses_total || 0) - (beforeStatus.transcode?.web_player?.cache_reuses_total || 0);
        const records = players.map((p, viewer) => ({ recipe, workload, sample, viewer, selection_to_frame_ms: p.first.first,
          browser: p.first, output_requests: structuredClone(p.requests), wall_ms: result.wall_ms,
          resource: result.resource, resource_scope: "Shared concurrent trial; do not sum across viewers",
          cache_bytes: null, warm_stamp_verified: Boolean(warmArtifact), warm_cache_reuse_counter_delta: workload === "warm" ? cacheReuses : null,
          server_before: beforeStatus.transcode, server_after_first_frame: afterStatus.transcode }));
        for (const record of records) { record.cache_bytes = await cacheBytes(); report.records.push(record); }
        if (workload === "warm" && ((await producers(item)).length || cacheReuses < 1)) throw new Error("Warm playback did not verify a completed-cache reuse without FFmpeg");
        if (workload !== "original") {
          if (workload === "cold") {
            if ((await producers(item)).length && !(await completedArtifact(item))) {
              let activeAtDispatch = [];
              const attachment = await measured(() => openPlayback(item, "compat", {
                beforeMediaRequest: async () => { activeAtDispatch = await producers(item); },
              }));
              const attached = attachment.value;
              report.records.push({ recipe, workload: "active-attachment", sample,
                available: activeAtDispatch.length > 0,
                ...(activeAtDispatch.length ? { selection_to_frame_ms: attached.first.first } : { reason: "Producer completed before attachment dispatch" }),
                active_helpers_at_dispatch: activeAtDispatch.length, actual_helper_recipes_observed: activeAtDispatch.map((helper) => helper.recipe), browser: attached.first, output_requests: attached.requests,
                resource: attachment.resource,
                boundary: "FFmpeg observed immediately before releasing the first media request; this instrumentation is included in attachment latency" });
              await attached.context.close();
            } else report.records.push({ recipe, workload: "active-attachment", sample, available: false,
              reason: "Producer had completed before a second viewer could attach" });
          }
          const artifact = await waitForCompletedArtifact(item);
          const requested = players[0].requests.find((request) => request.recipe.delivery === "mse").recipe;
          const validationId = workload === "cold" ? await validateOutput(recipe, artifact, requested, sample, { item, capabilities: library.capabilities })
            : report.validations.findLast((value) => value.recipe === recipe && value.sample === sample).id;
          records.forEach((record) => { record.validation_id = validationId; });
          if (workload === "warm") {
            for (const [seekKind, target, paused] of [["near", 4, false], ["near-paused", 6, true], ["restart", 30, false]]) {
              let buffer = await bufferedRange(players[0], target);
              if (seekKind !== "restart") {
                for (let attempt = 0; attempt < 100 && !buffer.candidate; attempt++) {
                  await sleep(50);
                  buffer = await bufferedRange(players[0], target);
                }
                if (!buffer.candidate) throw new Error(`Nearby seek prerequisites not buffered: ${JSON.stringify(buffer)}`);
              }
              console.log(`${recipe} sample ${sample + 1}/${samples}: ${seekKind}`);
              const beforeRequests = players[0].requests.length;
              const seekResult = await measured(() => seek(players[0], target, paused));
              const newRequests = players[0].requests.slice(beforeRequests);
              report.records.push({ recipe, workload: seekKind, sample, seek_to_frame_ms: seekResult.value.first,
                requested_paused: paused, buffer_before: buffer,
                actual_seek: newRequests.some((request) => request.recipe.delivery === "mse_init") ? "restarted" : "buffered",
                browser: seekResult.value, output_requests: newRequests, requests: newRequests.length, resource: seekResult.resource });
            }
          }
        }
        const sustained = await measured(() => Promise.all(players.map((player) => sustainedPlayback(player))));
        records.forEach((record, index) => { record.sustained = sustained.value[index]; record.sustained_resource = sustained.resource; });
        await closePlayers(players);
      }
      console.log(`${recipe} sample ${sample + 1}/${samples}: cancellation`);
      await measureCancellation(item, recipe, sample);
      await writeFile(output, JSON.stringify(report, null, 2));
      console.log(`${recipe} sample ${sample + 1}/${samples}`);
    }
  }
} catch (error) {
  report.failure = error.stack;
  process.exitCode = 1;
} finally {
  await browser?.close();
  await stopServer();
  report.summaries = summarizeRecords(report.records);
  if (args.compare) report.comparison = compareReports(JSON.parse(await readFile(resolve(args.compare), "utf8")), report, {
    median_percent: Number(args["median-percent"] || 25), median_ms: Number(args["median-ms"] || 50),
    p95_percent: Number(args["p95-percent"] || 30), p95_ms: Number(args["p95-ms"] || 100),
  });
  report.environment.host_load_average_end = loadavg();
  report.environment.free_memory_bytes_end = freemem();
  report.environment.cpu_reported_mhz_end = cpus().map((cpu) => cpu.speed);
  report.finished = new Date().toISOString();
  report.runtime_directory = run;
  await writeFile(output, JSON.stringify(report, null, 2));
  await writeFile(join(run, "server.log"), serverLogs.join(""));
  console.log(JSON.stringify({ output, failure: report.failure, summaries: report.summaries }, null, 2));
}
