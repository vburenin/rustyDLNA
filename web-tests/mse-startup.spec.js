import { createServer } from "node:http";
import { createSocket } from "node:dgram";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, mkdir, writeFile, copyFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { expect, test } from "./diagnostics.js";

const execute = promisify(execFile);
async function port(udp = false) {
  const socket = udp ? createSocket("udp4") : createServer();
  await new Promise((done) => udp ? socket.bind(0, "127.0.0.1", done) : socket.listen(0, "127.0.0.1", done));
  const value = socket.address().port;
  await new Promise((done) => socket.close(done));
  return value;
}

// Actual Rust generation/playlist/range responses and real browser sockets.
// Faults are applied only at the HTTP boundary in disposable storage.
async function fixture({ startupOverlap = true } = {}) {
  const root = await mkdtemp(join(tmpdir(), "rustydlna-mse-startup-"));
  await mkdir(join(root, "library"));
  await execute("ffmpeg", ["-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25",
    "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=39", "-t", "40",
    "-c:v", "libx264", "-preset", "ultrafast", "-threads", "2", "-g", "50", "-pix_fmt", "yuv420p",
    "-c:a", "aac", "-ac", "2", join(root, "library", "first.mp4")], { timeout: 15000, maxBuffer: 65536 });
  await copyFile(join(root, "library", "first.mp4"), join(root, "library", "second.mp4"));
  const backend = await port();
  const ssdp = await port(true);
  const config = join(root, "server.toml");
  await writeFile(config, 'media_dir = ["library"]\ncache_dir = "cache"\ndb_dir = "db"\nadvertise_ip = "127.0.0.1"\nrescan_secs = 0\n[transcode]\nenable = true\nencoder = "libx264"\nmax_jobs = 1\n');
  const child = spawn(resolve(process.env.RUSTY_DLNA_PLAYWRIGHT_SERVER_PROGRAM || "target/debug/rusty-dlna"), ["-c", config, "-p", String(backend)], {
    env: { ...process.env, RUSTY_DLNA_SSDP_PORT: String(ssdp) }, detached: true, stdio: ["ignore", "pipe", "pipe"],
  });
  let logs = "";
  for (const stream of [child.stdout, child.stderr]) stream.on("data", (data) => { logs = (logs + data).slice(-65536); });
  const exited = new Promise((done) => child.on("exit", done));
  let hold = true;
  const records = [];
  const held = [];
  let maximum = 0;
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, `http://127.0.0.1:${backend}`);
    if (startupOverlap && url.pathname === "/web/media-source.js") {
      // Explicitly opt this isolated experiment into the production transport's
      // option. The shipped player keeps its ordinary serial default.
      res.writeHead(200, { "Content-Type": "text/javascript", "Cache-Control": "no-store" });
      res.end('export * from "./mse-startup-original.js"; import { pumpMediaSource as pump } from "./mse-startup-original.js"; export const pumpMediaSource = options => pump({ ...options, startupOverlap: true });');
      return;
    }
    if (url.pathname === "/web/mse-startup-original.js") url.pathname = "/web/media-source.js";
    const delivery = url.searchParams.get("delivery");
    const controller = new AbortController();
    const record = ["mse_init", "mse_segment"].includes(delivery)
      ? { delivery, url: url.href, request: url.searchParams.get("request"), closed: false, sent: 0, expected: Number(url.searchParams.get("hls_length")) } : null;
    if (record) {
      records.push(record);
      maximum = Math.max(maximum, records.filter((entry) => !entry.closed).length);
    }
    res.on("close", () => { if (record) record.closed = true; controller.abort(); });
    try {
      const upstream = await fetch(url, { method: req.method, signal: controller.signal });
      const body = Buffer.from(await upstream.arrayBuffer());
      const headers = Object.fromEntries(upstream.headers);
      delete headers["transfer-encoding"]; delete headers["content-encoding"];
      const send = (fault) => {
        if (res.destroyed) { if (record) record.late = true; return; }
        if (fault === "http") { res.writeHead(503, { "content-length": "0" }); res.end(); return; }
        if (fault === "range") headers["content-range"] = `bytes 1-${body.length}/${body.length + 1}`;
        const bytes = fault === "malformed" ? Buffer.alloc(body.length)
          : fault === "partial" ? body.subarray(0, body.length - 1) : body;
        res.writeHead(upstream.status, headers);
        res.end(bytes);
        if (record) record.sent = bytes.length;
      };
      if (record && hold) held.push({ record, send });
      else send();
    } catch { res.destroy(); }
  });
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  const close = async () => {
    server.closeAllConnections();
    await new Promise((done) => server.close(done));
    if (child.exitCode === null && child.signalCode === null) process.kill(-child.pid, "SIGTERM");
    let timer;
    try {
      await Promise.race([exited, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`Fixture server failed to stop: ${logs}`)), 5000); })]);
    } catch (error) {
      if (child.exitCode === null && child.signalCode === null) process.kill(-child.pid, "SIGKILL");
      await exited;
      await writeFile(join(root, "server.log"), logs);
      throw error;
    } finally { clearTimeout(timer); }
    await rm(root, { recursive: true, force: true });
  };
  try {
    await expect.poll(async () => {
      const response = await fetch(`http://127.0.0.1:${backend}/api/web/library?view=library&kind=video`).catch(() => null);
      return response?.ok ? (await response.json()).entries?.filter((entry) => entry.entry_type === "media").length : 0;
    }).toBe(2);
  } catch (error) { await close(); throw error; }
  return { url: `http://127.0.0.1:${server.address().port}/?view=video`, records, held,
    get maximum() { return maximum; },
    release(faultDelivery, fault) {
      hold = false;
      for (const item of held.splice(0)) item.send(item.record.delivery === faultDelivery ? fault : undefined);
    },
    fail(delivery, fault) {
      hold = false;
      held.find((item) => item.record.delivery === delivery).send(fault);
    },
    completeOne(delivery) { held.find((item) => item.record.delivery === delivery).send(); },
    close,
  };
}

async function instrument(page, { serial = false, holdInitCompletion = false } = {}) {
  await page.addInitScript(({ serial, holdInitCompletion }) => {
    localStorage.setItem("rustydlna.stream", "compat"); localStorage.setItem("rustydlna.muted", "true");
    Object.defineProperty(navigator, "userAgent", { configurable: true, value: "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/151.0.0.0 Mobile Safari/537.36" });
    Object.defineProperty(navigator, "connection", { configurable: true, value: { effectiveType: "4g", downlink: serial ? 1 : 20, saveData: false } });
    const canPlay = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function(type) { return String(type).includes("mpegurl") ? "" : canPlay.call(this, type); };
    const state = window.__mseStartup = { buffers: 0, appends: [], frames: 0, lastFrame: null };
    const controllers = new WeakMap();
    if (holdInitCompletion) {
      const NativeController = AbortController;
      window.AbortController = class extends NativeController {
        constructor() { super(); controllers.set(this.signal, this); }
      };
      const listen = AbortSignal.prototype.addEventListener;
      AbortSignal.prototype.addEventListener = function(type, callback, options) {
        if (type === "abort" && state.awaitingInitAbort) {
          state.awaitingInitAbort = false;
          state.initOwner = this;
        }
        return listen.call(this, type, callback, options);
      };
    }
    const add = MediaSource.prototype.addSourceBuffer;
    const append = SourceBuffer.prototype.appendBuffer;
    const owners = new WeakMap();
    MediaSource.prototype.addSourceBuffer = function(type) {
      const buffer = add.call(this, type); owners.set(buffer, ++state.buffers);
      if (holdInitCompletion && state.buffers === 1) {
        const addEvent = buffer.addEventListener.bind(buffer);
        const removeEvent = buffer.removeEventListener.bind(buffer);
        const callbacks = new Map();
        buffer.addEventListener = (type, callback, options) => {
          if (type !== "updateend") return addEvent(type, callback, options);
          state.awaitingInitAbort = true;
          const delayed = (event) => {
            state.initCompletion = () => {
              // Settle init first, then cancel before its continuation runs.
              // Keep the real buffer attached so a stale append cannot be
              // hidden by the browser rejecting access to a removed buffer.
              callback.call(buffer, event);
              controllers.get(state.initOwner).abort();
            };
          };
          callbacks.set(callback, delayed); addEvent(type, delayed, options);
        };
        buffer.removeEventListener = (type, callback, options) => {
          removeEvent(type, callbacks.get(callback) || callback, options); callbacks.delete(callback);
        };
      }
      return buffer;
    };
    SourceBuffer.prototype.appendBuffer = function(bytes) {
      state.appends.push({ owner: owners.get(this), latest: state.buffers, bytes: bytes.byteLength,
        type: String.fromCharCode(...new Uint8Array(bytes.buffer || bytes, bytes.byteOffset || 0, 8).slice(4, 8)) });
      return append.call(this, bytes);
    };
    addEventListener("DOMContentLoaded", () => {
      const video = document.querySelector("video");
      const frame = (_now, metadata) => { state.frames++; state.lastFrame = metadata.mediaTime; video.requestVideoFrameCallback(frame); };
      video.requestVideoFrameCallback(frame);
    });
  }, { serial, holdInitCompletion });
}

for (const delivery of ["mse_init", "mse_segment"]) {
  for (const fault of ["http", "partial", "range", "malformed"]) {
    test(`real startup ${delivery} ${fault} failure retains bounded requests and healthy recovery`, async ({ page }) => {
      const server = await fixture();
      try {
        await instrument(page);
        await page.goto(server.url);
        await page.getByRole("button", { name: /^Play first\b/ }).click();
        await expect.poll(() => server.held.length).toBe(2);
        const old = server.held.slice();
        if (fault === "malformed") server.release(delivery, fault);
        else server.fail(delivery, fault);
        await expect.poll(() => old.every(({ record }) => record.closed)).toBe(true);
        if (fault !== "malformed") expect(old.find(({ record }) => record.delivery !== delivery).record.sent).toBe(0);
        // Recovery may offer an explicit Retry after exhausting its portable
        // attempt. Either route must return to actual decoded frames.
        await expect.poll(async () => (await page.evaluate(() => window.__mseStartup.frames)) > 2
          || await page.locator("#player-retry").isVisible()).toBe(true);
        if (await page.locator("#player-retry").isVisible()) await page.locator("#player-retry").click();
        await expect.poll(() => page.evaluate(() => window.__mseStartup.frames)).toBeGreaterThan(2);
        expect(await page.evaluate(() => window.__mseStartup.appends.every((entry) => entry.owner === entry.latest))).toBe(true);
        expect(new Set(server.records.map((record) => record.request)).size).toBeLessThanOrEqual(3);
        expect(server.maximum).toBeLessThanOrEqual(2);
      } finally { await page.goto("about:blank"); await server.close(); }
    });
  }
}

test("late init completion cannot append a prefetched fragment after source cancellation", async ({ page }) => {
  const server = await fixture();
  try {
    await instrument(page, { holdInitCompletion: true });
    await page.goto(server.url);
    await page.getByRole("button", { name: /^Play first\b/ }).click();
    await expect.poll(() => server.held.length).toBe(2);
    server.release();
    await expect.poll(() => page.evaluate(() => typeof window.__mseStartup.initCompletion)).toBe("function");
    expect(await page.evaluate(() => window.__mseStartup.appends.length)).toBe(1);
    await page.evaluate(async () => {
      window.__mseStartup.initCompletion();
      await new Promise(requestAnimationFrame);
    });
    expect(await page.evaluate(() => window.__mseStartup.appends.filter((entry) => entry.owner === 1).map((entry) => entry.type))).toEqual(["ftyp"]);
    await page.getByRole("button", { name: /^Play second\b/ }).click();
    await expect.poll(() => page.evaluate(() => window.__mseStartup.frames)).toBeGreaterThan(2);
    expect(await page.locator("#now-playing-title").textContent()).toBe("second");
  } finally { await page.goto("about:blank"); await server.close(); }
});

for (const { name, startupOverlap, serial, requests } of [
  { name: "production serial default", startupOverlap: false, serial: false, requests: 1 },
  { name: "experimental overlap", startupOverlap: true, serial: false, requests: 2 },
  { name: "slow-link serial control", startupOverlap: true, serial: true, requests: 1 },
]) {
  test(`real server MSE startup ${name} presents frames in append order`, async ({ page }) => {
    const server = await fixture({ startupOverlap });
    try {
      await instrument(page, { serial });
      await page.goto(server.url);
      await page.getByRole("button", { name: /^Play first\b/ }).click();
      await expect.poll(() => server.held.length).toBe(requests);
      expect(await page.evaluate(() => window.__mseStartup.appends)).toEqual([]);
      expect(server.held.reduce((sum, item) => sum + item.record.expected, 0)).toBeLessThanOrEqual(32 * 1024 * 1024);
      server.release();
      await expect.poll(() => page.evaluate(() => window.__mseStartup.frames)).toBeGreaterThan(2);
      const appends = await page.evaluate(() => window.__mseStartup.appends);
      expect(appends[0].type).toBe("ftyp"); expect(appends[1].type).toBe("moof");
      expect(appends.every((entry) => entry.owner === entry.latest)).toBe(true);
      expect(server.maximum).toBeLessThanOrEqual(requests);
    } finally { await page.goto("about:blank"); await server.close(); }
  });
}

for (const action of ["close", "seek", "title"]) {
  test(`real overlapping MSE requests abort on ${action} and a healthy replacement presents frames`, async ({ page }) => {
    const server = await fixture();
    try {
      await instrument(page);
      await page.goto(server.url);
      await page.getByRole("button", { name: /^Play first\b/ }).click();
      await expect.poll(() => server.held.length).toBe(2);
      const old = server.held.slice();
      if (action === "close") await page.locator("#close-player-button").click();
      else if (action === "title") await page.getByRole("button", { name: /^Play second\b/ }).click();
      else await page.locator("#timeline").evaluate((timeline) => { timeline.value = "30"; timeline.dispatchEvent(new Event("change", { bubbles: true })); });
      await expect.poll(() => old.every(({ record }) => record.closed)).toBe(true);
      expect(old.every(({ record }) => record.sent === 0)).toBe(true);
      if (action === "close") await page.getByRole("button", { name: /^Play first\b/ }).click();
      await expect.poll(() => server.held.length).toBe(4);
      expect(new Set(server.held.map(({ record }) => record.request)).size).toBe(2);
      server.release(); // Includes late completions of the cancelled sockets.
      await expect.poll(() => page.evaluate(() => window.__mseStartup.frames)).toBeGreaterThan(2);
      expect(await page.evaluate(() => window.__mseStartup.appends.every((entry) => entry.owner === entry.latest))).toBe(true);
      if (action === "seek") expect(await page.evaluate(() => window.__mseStartup.lastFrame)).toBeLessThan(10);
      expect(server.maximum).toBeLessThanOrEqual(2);
    } finally { await page.goto("about:blank"); await server.close(); }
  });
}

for (const completed of ["mse_init", "mse_segment"]) {
  test(`cancelling after ${completed} download bounds discarded prefetch bytes and permits healthy playback`, async ({ page }) => {
    const server = await fixture();
    try {
      await instrument(page);
      await page.goto(server.url);
      await page.getByRole("button", { name: /^Play first\b/ }).click();
      await expect.poll(() => server.held.length).toBe(2);
      const old = server.held.slice();
      server.completeOne(completed);
      const downloaded = old.find(({ record }) => record.delivery === completed).record;
      await expect.poll(() => downloaded.closed).toBe(true);
      expect(downloaded.sent).toBe(downloaded.expected);
      expect(downloaded.sent).toBeLessThanOrEqual(32 * 1024 * 1024);
      expect(await page.evaluate(() => window.__mseStartup.appends)).toEqual([]);
      await page.locator("#close-player-button").click();
      await expect.poll(() => old.every(({ record }) => record.closed)).toBe(true);
      expect(old.find(({ record }) => record.delivery !== completed).record.sent).toBe(0);
      await page.getByRole("button", { name: /^Play second\b/ }).click();
      await expect.poll(() => server.held.length).toBe(4);
      server.release();
      await expect.poll(() => page.evaluate(() => window.__mseStartup.frames)).toBeGreaterThan(2);
      expect(await page.evaluate(() => window.__mseStartup.appends.every((entry) => entry.owner === 2))).toBe(true);
      expect(server.maximum).toBeLessThanOrEqual(2);
    } finally { await page.goto("about:blank"); await server.close(); }
  });
}
