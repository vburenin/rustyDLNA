#!/usr/bin/env node
// One daemon/cache across all cycles. All mutable media lives in a private tree.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { copyFile, mkdir, mkdtemp, readFile, readdir, rename, rm, stat, writeFile } from "node:fs/promises";
import { cpus, freemem, loadavg, release, tmpdir, totalmem } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";
import { evaluateRun, ownedProcesses, sameProcess } from "./persistent-soak-summary.mjs";
import { runCommand } from "./persistent-soak-process.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const defaults = { seconds: 3600, "warmup-seconds": 150, "sample-ms": 1000, "query-concurrency": 2,
  "queries-per-cycle": 16, "cache-mb": 24, port: 18440, "ssdp-port": 12440,
  "max-rss-mb": 512, "max-threads": 256, "max-fds": 512, "max-children": 16,
  "max-tree-rss-mb": 2048, "max-db-mb": 128, "max-cache-mb": 48,
  "max-rss-growth-mb": 64, "max-fd-growth": 16, "max-thread-growth": 8,
  "max-query-ms": 5000, "max-playback-ms": 30000, "max-shutdown-ms": 20000 };
const options = { ...defaults };
for (const arg of process.argv.slice(2)) {
  if (arg === "--help") {
    console.log(`node scripts/persistent-soak.mjs --binary=target/debug/rusty-dlna --output=/tmp/persistent-soak.json [--option=value]\nDefaults: ${JSON.stringify(defaults)}\nLinux, Node >=20, locked Playwright Chromium, FFmpeg/FFprobe and built server required. Each cycle scans temporary media, decodes cold/warm compatible playback and a seek, verifies cache reuse, interrupts/reconnects a live producer then explicitly cancels it. Query/artwork traffic overlaps playback. Minimum three cycles wholly after warm-up and observed eviction required. No retries/skips. Runtime directory is removed after shutdown; report, samples JSONL, browser cycle events and bounded server log remain beside output. A cycle may finish after --seconds (bounded operations); this is finite evidence, not indefinite stability.`);
    process.exit(0);
  }
  const match = /^--([a-z-]+)=(.+)$/.exec(arg);
  if (!match || (!(match[1] in defaults) && !["binary", "output"].includes(match[1]))) throw new Error(`Unknown option: ${arg}`);
  options[match[1]] = match[1] in defaults ? Number(match[2]) : match[2];
}
for (const key of Object.keys(defaults)) {
  if (!Number.isSafeInteger(options[key]) || options[key] < (key === "warmup-seconds" ? 0 : 1)) throw new Error(`Invalid --${key}`);
}
if (options.seconds < 60 || options.seconds > 86400 || options["warmup-seconds"] >= options.seconds
  || options["sample-ms"] < 250 || options["sample-ms"] > 10000
  || options["query-concurrency"] > 8 || options["queries-per-cycle"] > 256
  || options["cache-mb"] < 8 || options["cache-mb"] > 128
  || ![options.port, options["ssdp-port"]].every((port) => port >= 1024 && port <= 65535)
  || [8200, 18201, 18300].includes(options.port) || [1900, 11901, 12000].includes(options["ssdp-port"])) {
  throw new Error("Use 60–86400 seconds, shorter warm-up, 250–10000ms samples, 1–8 query workers, 1–256 queries, 8–128MiB cache and isolated test ports");
}
const binary = resolve(options.binary || join(root, "target/debug/rusty-dlna"));
const output = resolve(options.output || join(tmpdir(), `rustydlna-persistent-${Date.now()}.json`));
const run = await mkdtemp(join(tmpdir(), "rustydlna-persistent-"));
const library = join(run, "library");
const cache = join(run, "cache");
const base = `http://127.0.0.1:${options.port}`;
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
const lifecycle = new AbortController();
const command = (program, args) => runCommand(program, args, { signal: lifecycle.signal });
const fileHash = async (path) => {
  const hash = createHash("sha256");
  for await (const part of createReadStream(path)) hash.update(part);
  return hash.digest("hex");
};
const report = { schema: 1, started: new Date().toISOString(), configuration: options, binary,
  runtime_directory: run, records: [], peaks: {}, failures: [],
  artifacts: { samples: `${output}.samples.jsonl`, server_log: `${output}.server.log` },
  limitations: ["Finite run on a shared host; no indefinite stability, cold-storage, GPU/HDR, or full browser-matrix claim.",
    "Generated 640x360 MPEG2/FLAC source; Chromium decodes actual H264/AAC MSE output. One playback viewer and configurable bounded query workers.",
    "Memory/FD/thread growth compares idle first/last thirds of cycles wholly after warm-up; busy peaks use a separate sampler. Linear slopes are descriptive, not extrapolated leak predictions.",
    "The daemon and cache are never restarted or manually purged during the workload. Temporary catalog files are replaced/deleted to keep the active library bounded; outputs disappear through normal cache eviction.",
    "Resource samples can miss short-lived helpers. Shutdown also checks every captured PID/start identity, including helpers that create private process groups.",
    "OS page cache is warm after generation; sampler and filesystem accounting overhead are included. No privileged cache drop." ] };
let server, browser, context, page, sampleStream;
let logs = "", stopping = false, monitor, monitorFailure, workloadStart, queryStop = false;
const observed = new Map();
let pageSize, clockTicks;
const interrupted = (signal) => {
  const error = new Error(`Interrupted by ${signal}`);
  monitorFailure = error; lifecycle.abort(error);
  // Interrupt an outstanding browser wait so the common finally path owns cleanup.
  void page?.close().catch(() => {});
};
process.on("SIGTERM", interrupted);
process.on("SIGINT", interrupted);
const elapsed = () => (performance.now() - workloadStart) / 1000;
const assert = (condition, message) => { if (!condition) throw new Error(message); };
function errorDetails(error) {
  const details = [];
  for (let depth = 0; error && depth < 4; depth++, error = error.cause) {
    details.push(`${error.stack || String(error)}${error.code ? ` [${error.code}]` : ""}`.slice(0, 16384));
  }
  return details.join("\nCaused by: ");
}
async function until(check, timeout, label) {
  const deadline = performance.now() + timeout;
  do {
    if (monitorFailure) throw monitorFailure;
    if (server && (server.exitCode !== null || server.signalCode !== null)) throw new Error(`Daemon exited: ${logs.slice(-2000)}`);
    const result = await check();
    if (result) return result;
    await sleep(50);
  } while (performance.now() < deadline);
  throw new Error(`${label} deadline (${timeout}ms)`);
}
async function json(path) {
  try {
    const response = await fetch(`${base}${path}`, { signal: AbortSignal.timeout(options["max-query-ms"]) });
    assert(response.ok, `${path}: HTTP ${response.status}`);
    return await response.json();
  } catch (error) { throw new Error(`GET ${path} failed`, { cause: error }); }
}
const status = () => json("/api/status");
const list = () => json("/api/web/library?view=library&kind=video&limit=100");
async function processes() {
  const rows = [];
  for (const pid of await readdir("/proc")) {
    if (!/^\d+$/.test(pid)) continue;
    try {
      const raw = await readFile(`/proc/${pid}/stat`, "utf8");
      const fields = raw.slice(raw.lastIndexOf(")") + 2).split(" ");
      rows.push({ pid: Number(pid), parent: Number(fields[1]), state: fields[0], started: fields[19],
        rss_bytes: Number(fields[21]) * pageSize, threads: Number(fields[17]),
        cpu_seconds: (Number(fields[11]) + Number(fields[12])) / clockTicks });
    } catch (error) { if (!["ENOENT", "ESRCH"].includes(error.code)) throw error; }
  }
  return rows;
}
async function storage(directory) {
  let bytes = 0, media = 0, parts = 0, stamps = 0;
  for (const entry of await readdir(directory, { withFileTypes: true }).catch((error) => { if (error.code === "ENOENT") return []; throw error; })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) { bytes += (await storage(path)).bytes; continue; }
    const metadata = await stat(path).catch((error) => { if (error.code === "ENOENT") return null; throw error; });
    if (!metadata?.isFile()) continue;
    bytes += metadata.size;
    if (/^\d+-web-.*\.mp4$/.test(entry.name)) media += metadata.size;
    if (/^\d+-web-.*\.mp4\.part/.test(entry.name)) parts += metadata.size;
    if (/^\d+-web-.*\.mp4\.src$/.test(entry.name)) stamps += metadata.size;
  }
  return { bytes, media_bytes: media, intermediate_bytes: parts, stamp_bytes: stamps };
}
async function sample(idle = false) {
  const started = performance.now();
  const rows = await processes();
  const owned = ownedProcesses(rows, [process.pid]);
  const daemon = ownedProcesses(rows, [server.pid]);
  for (const row of owned.filter((row) => row.pid !== process.pid)) observed.set(`${row.pid}:${row.started}`, row);
  const own = daemon.find((row) => row.pid === server.pid);
  assert(own, "Persistent daemon PID disappeared");
  const [fds, disk, db, serverStatus] = await Promise.all([
    readdir(`/proc/${server.pid}/fd`), storage(cache), storage(join(run, "db")), status(),
  ]);
  const value = { elapsed_seconds: elapsed(), idle, server_pid: server.pid, server_started: own.started,
    rss_bytes: own.rss_bytes, threads: own.threads, fds: fds.length, cpu_seconds: own.cpu_seconds,
    children: daemon.length - 1, server_tree_rss_bytes: daemon.reduce((n, row) => n + row.rss_bytes, 0),
    server_tree_threads: daemon.reduce((n, row) => n + row.threads, 0),
    browser_and_harness_rss_bytes: owned.filter((row) => !daemon.includes(row)).reduce((n, row) => n + row.rss_bytes, 0),
    db_bytes: db.bytes, disk_cache_bytes: disk.bytes, completed_cache_bytes: disk.media_bytes,
    intermediate_cache_bytes: disk.intermediate_bytes, stamp_bytes: disk.stamp_bytes,
    accounted_cache_bytes: serverStatus.transcode.cache_bytes,
    active_jobs: serverStatus.transcode.active, active_helpers: serverStatus.helpers.active,
    scanner: serverStatus.scanner, transcode: serverStatus.transcode,
    host_load: loadavg(), host_free_bytes: freemem(),
    host_memory_pressure: await readFile("/proc/pressure/memory", "utf8").catch(() => "unavailable"),
    host_cpu_pressure: await readFile("/proc/pressure/cpu", "utf8").catch(() => "unavailable") };
  value.sampler_ms = performance.now() - started;
  const bounds = { rss_bytes: options["max-rss-mb"] * 1048576, threads: options["max-threads"],
    fds: options["max-fds"], children: options["max-children"], server_tree_rss_bytes: options["max-tree-rss-mb"] * 1048576,
    db_bytes: options["max-db-mb"] * 1048576, disk_cache_bytes: options["max-cache-mb"] * 1048576 };
  for (const [key, bound] of Object.entries(bounds)) {
    report.peaks[key] = Math.max(report.peaks[key] || 0, value[key]);
    assert(value[key] <= bound, `${key}=${value[key]} exceeds ${bound}`);
  }
  sampleStream.write(`${JSON.stringify(value)}\n`);
  return value;
}
async function artifact(id) {
  for (const name of await readdir(cache)) {
    if (!name.startsWith(`${id}-web-`) || !name.endsWith(".mp4")) continue;
    const path = join(cache, name);
    const stamp = await stat(`${path}.src`).catch(() => null);
    if (stamp?.size && (await stat(path).catch(() => null))?.size) return path;
  }
  return null;
}
async function idle() {
  await until(async () => { const s = await status(); return s.transcode.active === 0 && s.transcode.queued === 0 && s.helpers.active === 0; }, 30000, "Helpers idle");
  return until(async () => {
    const value = await sample(true);
    return value.children === 0 && value.intermediate_cache_bytes === 0
      && value.accounted_cache_bytes === value.completed_cache_bytes ? value : false;
  }, 10000, "Idle helper reaping and cache accounting");
}
async function play(item, record, label) {
  const requested = [];
  const listener = (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/web/media/") && requested.length < 128) requested.push(Object.fromEntries(url.searchParams));
  };
  page.on("request", listener);
  const started = performance.now();
  try {
    await page.goto(`${base}/?view=video&item=${item.id}`, { waitUntil: "domcontentloaded", timeout: options["max-playback-ms"] });
    await page.waitForFunction(() => window.__persistent?.frames >= 3, null, { timeout: options["max-playback-ms"] });
    assert(requested.some((request) => request.delivery === "mse_segment"), "Playback did not decode actual compatible MSE segments");
    const snapshot = await page.evaluate(() => structuredClone(window.__persistent));
    record.browser.push({ label, startup_ms: performance.now() - started, ...snapshot, requests: requested });
    assert(snapshot.errors.length === 0, `Browser errors: ${snapshot.errors.join("; ")}`);
    record.playbacks++;
  } finally { page.off("request", listener); }
}
async function closePlayer() {
  await page.locator("#player-stage").hover();
  await page.locator("#close-player-button").click();
  await page.waitForFunction(() => document.querySelector("video").paused);
}
async function seek(record) {
  await page.evaluate(() => {
    const state = window.__persistent;
    state.target = 12; state.seekFrame = null; state.seekStart = performance.now();
    const timeline = document.querySelector("#timeline");
    timeline.value = "12"; timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await page.waitForFunction(() => window.__persistent.seekFrame !== null && !document.querySelector("video").seeking, null,
    { timeout: options["max-playback-ms"] });
  record.seek = await page.evaluate(() => structuredClone(window.__persistent));
  assert(record.seek.errors.length === 0, `Browser errors during seek: ${record.seek.errors.join("; ")}`);
  record.seeks++;
}
async function queryLoad(record) {
  const paths = ["view=library&kind=video&sort=title&limit=2", "view=library&kind=video&q=cycle&limit=4",
    "view=folders&kind=all&limit=4", "view=library&kind=video&sort=title&offset=1&limit=2"];
  let next = 0;
  await Promise.all(Array.from({ length: options["query-concurrency"] }, async () => {
    while (!queryStop) {
      const index = next++;
      if (index >= options["queries-per-cycle"]) return;
      const start = performance.now();
      await json(`/api/web/library?${paths[index % paths.length]}`);
      record.query_ms.push(performance.now() - start); record.queries++;
      const response = await fetch(`${base}${record.art_url}`, { signal: AbortSignal.timeout(options["max-query-ms"]) });
      assert(response.ok && response.headers.get("content-type")?.startsWith("image/"), `Artwork HTTP ${response.status}`);
      assert((await response.arrayBuffer()).byteLength > 0, "Empty artwork");
      record.artwork++;
      await sleep(100);
    }
  }));
}
async function reconnectAndCancel(item, record) {
  const before = await status();
  const params = new URLSearchParams({ mode: "compatible", video_mode: "transcode", audio_mode: "transcode",
    quality: "auto", request: String(record.cycle + 1), session: "900001" });
  const url = `${base}/web/media/${item.id}.mp4?${params}`;
  const readers = [];
  const start = performance.now();
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(30000) });
    assert(response.ok, `Initial reconnect source HTTP ${response.status}`);
    const reader = response.body.getReader(); readers.push(reader);
    assert(!(await reader.read()).done, "Initial reconnect source had no bytes");
    const producer = [];
    for (const row of ownedProcesses(await processes(), [server.pid]).filter((row) => row.pid !== server.pid)) {
      const args = (await readFile(`/proc/${row.pid}/cmdline`, "utf8").catch(() => "")).split("\0");
      if (args.some((arg) => arg.startsWith(join(cache, `${item.id}-web-`)) && arg.endsWith(".mp4.part"))) producer.push(row);
    }
    assert(producer.length > 0 && (await status()).transcode.active > 0, "Reconnect requires a live producer, but it already completed");
    await reader.cancel();
    const reconnected = await fetch(url, { signal: AbortSignal.timeout(30000) });
    assert(reconnected.ok, `Reconnected source HTTP ${reconnected.status}`);
    const second = reconnected.body.getReader(); readers.push(second);
    assert(!(await second.read()).done, "Reconnected source had no bytes");
    const current = await processes();
    assert(producer.some((old) => current.some((row) => sameProcess(row, old))), "Reconnect did not retain the live helper identity");
    assert((await status()).transcode.active > 0, "Producer completed before reconnect cancellation observation");
    record.reconnects++;
    await second.cancel();
    const cancelledAt = performance.now();
    const control = new URLSearchParams({ request: params.get("request"), session: params.get("session") });
    const cancellation = await fetch(`${base}/api/web/transcode/${item.id}?${control}`, { method: "DELETE", signal: AbortSignal.timeout(5000) });
    assert(cancellation.ok, `Cancellation HTTP ${cancellation.status}`);
    await until(async () => {
      const [rows, state] = await Promise.all([processes(), status()]);
      return !producer.some((old) => rows.some((row) => sameProcess(row, old)))
        && state.transcode.active === 0 && state.transcode.cancelled_total > before.transcode.cancelled_total;
    }, 10000, "Explicit cancellation and producer reaping");
    record.cancellations++;
    record.reconnect = { milliseconds: performance.now() - start, cancellation_ms: performance.now() - cancelledAt,
      helpers: producer, same_live_producer: true, reaped: true };
  } finally { await Promise.allSettled(readers.map((reader) => reader.cancel())); }
}
async function shutdown() {
  stopping = true;
  await monitor;
  const cleanup = report.cleanup = { escalated: false, leaked_processes: [] };
  // Capture immediately before shutdown as well as during periodic sampling.
  if (server?.pid) for (const row of ownedProcesses(await processes(), [process.pid]).filter((row) => row.pid !== process.pid)) observed.set(`${row.pid}:${row.started}`, row);
  await browser?.close();
  if (server?.pid && server.exitCode === null && server.signalCode === null) {
    const started = performance.now();
    const exited = new Promise((done) => server.once("exit", done));
    server.kill("SIGTERM");
    const timer = setTimeout(() => { cleanup.escalated = true; server.kill("SIGKILL"); }, options["max-shutdown-ms"]);
    await exited; clearTimeout(timer);
    cleanup.shutdown_ms = performance.now() - started;
  }
  const deadline = performance.now() + 5000;
  do {
    const current = await processes();
    cleanup.leaked_processes = [...observed.values()].filter((old) => current.some((row) => sameProcess(row, old)));
    if (!cleanup.leaked_processes.length) break;
    await sleep(50);
  } while (performance.now() < deadline);
  cleanup.server_exit_code = server?.exitCode ?? null;
  cleanup.server_signal = server?.signalCode ?? null;
  if (cleanup.escalated) report.failures.push("Daemon needed shutdown escalation");
  if (server && server.exitCode !== 0) report.failures.push(`Daemon did not exit cleanly: ${server.exitCode}/${server.signalCode}`);
  if (cleanup.leaked_processes.length) {
    report.failures.push(`${cleanup.leaked_processes.length} owned processes remained after shutdown`);
    for (const old of cleanup.leaked_processes) {
      const current = await processes();
      if (current.some((row) => sameProcess(row, old))) { try { process.kill(old.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; } }
    }
  }
  await writeFile(report.artifacts.server_log, logs);
  await rm(run, { recursive: true, force: true });
  cleanup.temporary_tree_removed = true;
}
try {
  pageSize = Number(await command("getconf", ["PAGESIZE"]));
  clockTicks = Number(await command("getconf", ["CLK_TCK"]));
  report.binary_sha256 = await fileHash(binary);
  report.environment = { node: process.version, kernel: release(), cpu: cpus()[0]?.model,
    logical_cpus: cpus().length, memory_bytes: totalmem(), host_load_start: loadavg(),
    runtime: JSON.parse(await command("python3", [join(root, "scripts/runtime-evidence.py"), "--binary", binary])) };
  report.source = { commit: await command("git", ["-C", root, "rev-parse", "HEAD"]),
    dirty: await command("git", ["-C", root, "status", "--porcelain"]),
    diff_sha256: createHash("sha256").update(await command("git", ["-C", root, "diff", "--binary", "HEAD"])).digest("hex") };
  report.source.harness_sha256 = Object.fromEntries(await Promise.all([
    "scripts/persistent-soak.mjs", "scripts/persistent-soak-summary.mjs", "scripts/persistent-soak-process.mjs", "scripts/runtime-evidence.py",
  ].map(async (path) => [path, await fileHash(join(root, path))])));
  await mkdir(library);
  const generate = (path, duration) => command("ffmpeg", ["-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24",
    "-f", "lavfi", "-i", `sine=frequency=440:sample_rate=48000:duration=${duration - 1}`, "-t", String(duration),
    "-c:v", "mpeg2video", "-threads", "2", "-g", "48", "-q:v", "3", "-pix_fmt", "yuv420p", "-c:a", "flac", "-ac", "2", path]);
  const template = join(run, "template.mkv");
  await generate(template, 40); await generate(join(library, "cancellation.mkv"), 240);
  await command("ffmpeg", ["-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=c=blue:size=96x144", "-frames:v", "1", "-threads", "1", join(library, "poster.jpg")]);
  report.fixtures = { playback_sha256: await fileHash(template), cancellation_sha256: await fileHash(join(library, "cancellation.mkv")),
    probe: JSON.parse(await command("ffprobe", ["-v", "error", "-show_streams", "-show_format", "-of", "json", template])) };
  const config = join(run, "server.toml");
  await writeFile(config, `friendly_name = "persistent soak"\nmedia_dir = ["library"]\ncache_dir = "cache"\ndb_dir = "db"\nlisten_ip = "127.0.0.1"\nadvertise_ip = "127.0.0.1"\nrescan_secs = 2\nscan_workers = 2\nhelper_max_jobs = 4\ncache_min_free_mb = 0\n[transcode]\nenable = true\nencoder = "libx264"\nmax_jobs = 2\ncache_max_mb = ${options["cache-mb"]}\n[web]\nencoder = "libx264"\n`);
  server = spawn(binary, ["-c", config, "-p", String(options.port)], { detached: true, stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, RUSTY_DLNA_SSDP_PORT: String(options["ssdp-port"]) } });
  server.on("error", (error) => { monitorFailure = error; });
  for (const stream of [server.stdout, server.stderr]) stream.on("data", (part) => { logs = (logs + part).slice(-4 * 1024 * 1024); });
  await until(async () => { try { return (await list()).entries.find((item) => item.file_name === "cancellation.mkv"); } catch { return false; } }, 60000, "Initial catalog");
  report.server_pid = server.pid;
  report.loaded_binary_sha256 = await fileHash(`/proc/${server.pid}/exe`);
  assert(report.loaded_binary_sha256 === report.binary_sha256, "Server executable changed between evidence capture and launch");
  browser = await chromium.launch({ headless: true, args: ["--autoplay-policy=no-user-gesture-required"] });
  report.environment.browser = browser.version();
  context = await browser.newContext({ userAgent: "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36" });
  await context.addInitScript(() => {
    localStorage.setItem("rustydlna.stream", "compat"); localStorage.setItem("rustydlna.muted", "true");
    const canPlay = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function(type) { return String(type).includes("mpegurl") ? "" : canPlay.call(this, type); };
    const state = window.__persistent = { frames: 0, events: [], errors: [], offset: 0, timings: [] };
    const originalFetch = fetch;
    window.fetch = function(input, options) {
      const url = new URL(input instanceof Request ? input.url : String(input), location.href);
      if (url.pathname.startsWith("/web/media/") && url.searchParams.get("delivery") === "mse") state.offset = Number(url.searchParams.get("start") || 0);
      return originalFetch.call(this, input, options);
    };
    addEventListener("error", (event) => { if (state.errors.length < 16) state.errors.push(event.message); });
    addEventListener("rustydlna-playback-timing", (event) => { if (state.timings.length < 32) state.timings.push(event.detail); });
    addEventListener("DOMContentLoaded", () => {
      const video = document.querySelector("video");
      const frame = (now, metadata) => {
        state.frames++; state.media_time = metadata.mediaTime;
        if (state.target !== undefined && state.seekFrame === null && Math.abs(metadata.mediaTime + state.offset - state.target) < 0.5) state.seekFrame = { ms: now - state.seekStart, media_time: metadata.mediaTime, offset: state.offset };
        video.requestVideoFrameCallback(frame);
      };
      video.requestVideoFrameCallback(frame);
      for (const event of ["playing", "waiting", "error", "seeking", "seeked", "ended"]) video.addEventListener(event, () => {
        if (state.events.length < 128) state.events.push({ event, ms: performance.now(), time: video.currentTime });
        if (event === "error" && state.errors.length < 16) state.errors.push(`Media error ${video.error?.code}`);
      });
    });
  });
  page = await context.newPage();
  workloadStart = performance.now();
  sampleStream = createWriteStream(report.artifacts.samples, { flags: "wx" });
  sampleStream.on("error", (error) => { monitorFailure = error; });
  monitor = (async () => { while (!stopping) { await sample(); await sleep(options["sample-ms"]); } })().catch((error) => { monitorFailure = error; });
  let previousFile;
  while (elapsed() < options.seconds) {
    const cycleStart = elapsed();
    const before = await status();
    const record = { cycle: report.records.length, phase: cycleStart < options["warmup-seconds"] ? "warmup" : "steady",
      started_seconds: cycleStart, playbacks: 0, seeks: 0, cancellations: 0, reconnects: 0, cache_reuses: 0,
      evictions: 0, scans: 0, artwork: 0, queries: 0, query_ms: [], browser: [] };
    report.records.push(record);
    const path = join(library, `cycle-${record.cycle}.mkv`);
    await copyFile(template, join(run, "arrival.mkv"));
    await rename(join(run, "arrival.mkv"), path);
    if (previousFile) await rm(previousFile);
    previousFile = path;
    const item = await until(async () => (await list()).entries.find((item) => item.file_name === `cycle-${record.cycle}.mkv`), 30000, "Watcher publication");
    record.scans++; record.scan_ms = (elapsed() - cycleStart) * 1000;
    record.art_url = item.art_url;
    assert(record.art_url, "Generated poster was not advertised");
    const traffic = queryLoad(record);
    // Attach immediately: a query failure stays handled while playback is awaited.
    let trafficError; const observedTraffic = traffic.catch((error) => { trafficError = error; });
    await play(item, record, "cold");
    const completed = await until(() => artifact(item.id), 60000, "Completed validated cache output");
    const probe = JSON.parse(await command("ffprobe", ["-v", "error", "-show_streams", "-show_format", "-of", "json", completed]));
    assert(probe.streams.some((stream) => stream.codec_name === "h264") && probe.streams.some((stream) => stream.codec_name === "aac")
      && Number(probe.format.duration) >= 38.5, "Completed output codec/duration validation failed");
    record.output = { bytes: (await stat(completed)).size, sha256: await fileHash(completed), probe };
    await closePlayer();
    const reuseBefore = (await status()).transcode.web_player.cache_reuses_total;
    await play(item, record, "warm");
    record.cache_reuses = (await status()).transcode.web_player.cache_reuses_total - reuseBefore;
    assert(record.cache_reuses > 0, "Warm playback did not reuse completed cache");
    await seek(record); await closePlayer();
    await observedTraffic; if (trafficError) throw trafficError;
    const cancellation = (await list()).entries.find((entry) => entry.file_name === "cancellation.mkv");
    await idle();
    await reconnectAndCancel(cancellation, record);
    const settled = await idle();
    Object.assign(record, { elapsed_seconds: elapsed(), rss_bytes: settled.rss_bytes, fds: settled.fds, threads: settled.threads,
      cache_bytes: settled.accounted_cache_bytes, db_bytes: settled.db_bytes, children: settled.children,
      evictions: settled.transcode.cache_evicted_files_total - before.transcode.cache_evicted_files_total,
      scanner_batches: settled.scanner.batches - before.scanner.batches,
      full_reconciles: settled.scanner.full_reconciles - before.scanner.full_reconciles });
    assert(settled.transcode.failed_total === 0, "Server reported a failed transcode");
    assert(settled.transcode.cache_maintenance_failures_total === 0, "Server reported failed cache maintenance");
    if (monitorFailure) throw monitorFailure;
    await writeFile(output, JSON.stringify(report, null, 2));
    console.log(`persistent soak cycle=${record.cycle} phase=${record.phase} seconds=${elapsed().toFixed(1)} rss=${record.rss_bytes} fds=${record.fds} cache=${record.cache_bytes} evictions=${record.evictions}`);
  }
  report.workload_seconds = elapsed();
  report.summary = evaluateRun(report.records, options["warmup-seconds"], { rss_bytes: options["max-rss-growth-mb"] * 1048576,
    fds: options["max-fd-growth"], threads: options["max-thread-growth"] });
  report.failures.push(...report.summary.failures);
} catch (error) {
  report.failures.push(errorDetails(error));
  if (page && !page.isClosed()) report.browser_failure = await page.evaluate(() => window.__persistent).catch(() => null);
} finally {
  queryStop = true;
  try { await shutdown(); } catch (error) { report.failures.push(`Cleanup: ${errorDetails(error)}`); }
  if (monitorFailure && !report.failures.includes(errorDetails(monitorFailure))) report.failures.push(errorDetails(monitorFailure));
  if (sampleStream) await new Promise((done) => sampleStream.end(done));
  report.result = report.failures.length ? "fail" : "pass";
  report.finished = new Date().toISOString();
  report.host_load_end = loadavg();
  await writeFile(output, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ output, result: report.result, cycles: report.records.length, summary: report.summary, failures: report.failures }));
  if (report.failures.length) process.exitCode = 1;
}
