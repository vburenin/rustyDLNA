import { expect, test } from "@playwright/test";

// Exercise the real transport in every browser, with deterministic event and
// stream faults independent of OS codec support. Player recovery/decoding is
// covered separately with actual fragmented MP4 in player.spec.js.
for (const phase of ["sourceopen", "playlist headers", "init headers", "fragment headers",
  "playlist body", "init body", "fragment body", "append", "remove", "oversized playlist",
  "absolute body"]) {
  test(`MSE bounds ${phase} and cleans pending operations`, async ({ page }) => {
    await page.goto("/");
    await page.clock.install();
    await page.evaluate(async (phase) => {
      const { pumpMediaSource } = await import("/web/media-source.js");
      const controller = new AbortController();
      const tracked = [];
      class TrackedTarget extends EventTarget {
        listeners = new Map();
        constructor() { super(); tracked.push(this); }
        addEventListener(type, callback, options) {
          if (!this.listeners.has(type)) this.listeners.set(type, new Set());
          this.listeners.get(type).add(callback);
          super.addEventListener(type, callback, options);
        }
        removeEventListener(type, callback, options) {
          this.listeners.get(type)?.delete(callback);
          super.removeEventListener(type, callback, options);
        }
      }
      const state = window.__mseDeadline = { reached: false, result: null, aborted: 0, cancelled: 0, listeners: null };
      const sourceBuffer = new TrackedTarget();
      Object.assign(sourceBuffer, {
        updating: false,
        buffered: { length: phase === "remove" ? 1 : 0, start: () => 0, end: () => 7 },
        appendBuffer() {
          this.updating = true;
          if (phase === "append") { state.reached = true; return; }
          queueMicrotask(() => { this.updating = false; this.dispatchEvent(new Event("updateend")); });
        },
        remove() { this.updating = true; state.reached = true; },
      });
      const mediaSource = new TrackedTarget();
      Object.assign(mediaSource, { readyState: phase === "sourceopen" ? "closed" : "open",
        addSourceBuffer: () => sourceBuffer, endOfStream() {} });
      const player = new TrackedTarget();
      Object.assign(player, { paused: false, currentTime: phase === "remove" ? 6 : 0 });
      const originalFetch = window.fetch;
      window.fetch = async (url, options) => {
        const resource = { mse: "playlist", mse_init: "init", mse_segment: "fragment" }[new URL(url).searchParams.get("delivery")];
        options.signal.addEventListener("abort", () => { state.aborted += 1; }, { once: true });
        if (phase === `${resource} headers`) {
          state.reached = true;
          return new Promise(() => {});
        }
        if (phase === `${resource} body` || (phase === "absolute body" && resource === "fragment")) {
          state.reached = true;
          return new Response(new ReadableStream({
            start(stream) {
              stream.enqueue(new Uint8Array([1]));
              if (phase === "absolute body") {
                this.timer = setInterval(() => stream.enqueue(new Uint8Array([1])), 5_000);
              }
            },
            cancel() { clearInterval(this.timer); state.cancelled += 1; },
          }));
        }
        if (phase === "oversized playlist") {
          state.reached = true;
          return new Response(new ReadableStream({ start(stream) {
            stream.enqueue(new Uint8Array(4 * 1024 * 1024));
            stream.enqueue(new Uint8Array(1));
          }, cancel() { state.cancelled += 1; } }));
        }
        return new Response(resource === "playlist"
          ? '#EXTM3U\n#EXT-X-MAP:URI="/web/media/1.mp4?delivery=mse_init&hls_offset=0&hls_length=1"\n#EXTINF:1,\n/web/media/1.m4s?delivery=mse_segment&hls_offset=1&hls_length=1\n#EXT-X-ENDLIST\n'
          : new Uint8Array([1]));
      };
      if (phase === "sourceopen") state.reached = true;
      void pumpMediaSource({ player, mediaSource, playlistUrl: `${location.origin}/web/media/1.m3u8?delivery=mse`,
        contentType: "video/mp4", signal: controller.signal, reportStartup() {} })
        .then(() => { state.result = "unexpected success"; }, (error) => { state.result = error.message; })
        .finally(() => {
          controller.abort();
          window.fetch = originalFetch;
          state.listeners = tracked.reduce((sum, target) => sum
            + [...target.listeners.values()].reduce((n, listeners) => n + listeners.size, 0), 0);
        });
    }, phase);
    await expect.poll(() => page.evaluate(() => window.__mseDeadline.reached)).toBe(true);
    const budget = phase === "playlist headers" || phase === "absolute body" ? 120_001
      : phase.endsWith("headers") ? 30_001 : phase.endsWith("body") ? 15_001 : 20_001;
    await page.clock.runFor(budget);
    await expect.poll(() => page.evaluate(() => window.__mseDeadline.result)).toMatch(
      phase === "oversized playlist" ? /too large/ : /timed out/,
    );
    await expect.poll(() => page.evaluate(() => window.__mseDeadline.listeners)).toBe(0);
    if (phase.includes("headers") || phase.includes("body") || phase === "oversized playlist") {
      await expect.poll(() => page.evaluate(() => window.__mseDeadline.aborted)).toBeGreaterThan(0);
    }
    if (phase.includes("body") || phase === "oversized playlist") {
      await expect.poll(() => page.evaluate(() => window.__mseDeadline.cancelled)).toBe(1);
    }
  });
}

test("MSE replacement aborts a held body without waiting for its deadline", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const { pumpMediaSource } = await import("/web/media-source.js");
    const controller = new AbortController();
    const originalFetch = window.fetch;
    let cancelled = false;
    window.fetch = async () => new Response(new ReadableStream({
      start() { window.setTimeout(() => controller.abort(), 0); },
      cancel() { cancelled = true; },
    }));
    const mediaSource = Object.assign(new EventTarget(), { readyState: "open", addSourceBuffer: () => ({}) });
    try {
      await pumpMediaSource({ mediaSource, player: { paused: false }, playlistUrl: `${location.origin}/web/media/1.m3u8?delivery=mse`,
        contentType: "video/mp4", signal: controller.signal, reportStartup() {} });
      return { name: "success", cancelled };
    } catch (error) { return { name: error.name, cancelled }; }
    finally { window.fetch = originalFetch; }
  });
  expect(result).toEqual({ name: "AbortError", cancelled: true });
});

async function installPlayerFault(page, { fault, hevc = false, persistent = false }) {
  await page.addInitScript(({ fault, persistent, hevc }) => {
    localStorage.setItem("rustydlna.stream", "compat");
    localStorage.setItem("rustydlna.quality", hevc ? "auto" : "data_saver");
    Object.defineProperty(navigator, "userAgent", { configurable: true,
      value: "Mozilla/5.0 (Linux; Android 10) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36" });
    Object.defineProperty(navigator, "mediaCapabilities", { configurable: true,
      value: { decodingInfo: async () => ({ supported: true, smooth: true, powerEfficient: true }) } });
    HTMLMediaElement.prototype.canPlayType = (type) => /mpegurl|ac-3/.test(type) ? "" : "probably";
    const state = window.__msePlayerFault = { generations: 0, requests: [], cancelled: [], aborted: [],
      reached: false, status: "ready", produced: 1, ready: 0, time: 0, seeking: false, paused: true };
    const objects = new Map();
    const sources = new WeakMap();
    class FakeMediaSource extends EventTarget {
      static isTypeSupported() { return true; }
      readyState = "closed";
      generation = ++state.generations;
      addSourceBuffer() {
        const generation = this.generation;
        const buffer = new EventTarget();
        let appends = 0;
        Object.assign(buffer, { buffered: { length: 0, start: () => 0, end: () => 2 },
          remove() { queueMicrotask(() => buffer.dispatchEvent(new Event("updateend"))); },
          appendBuffer() {
            appends += 1;
            const broken = persistent || generation === 1;
            if (broken && fault === "append") { state.reached = true; return; }
            queueMicrotask(() => {
              buffer.dispatchEvent(new Event("updateend"));
              if (appends < 2) return;
              buffer.buffered.length = 1;
              if (broken && fault === "first frame") { state.reached = true; return; }
              const player = document.querySelector("#video-player");
              state.ready = 3;
              state.time = 0.5;
              player.dispatchEvent(new Event("loadeddata"));
              player.dispatchEvent(new Event("canplay"));
              player.dispatchEvent(new Event("timeupdate"));
              if (fault === "playback progress") state.reached = true;
            });
          },
        });
        return buffer;
      }
      endOfStream() {}
    }
    globalThis.MediaSource = FakeMediaSource;
    const createObjectURL = URL.createObjectURL;
    URL.createObjectURL = (object) => {
      if (!(object instanceof FakeMediaSource)) return createObjectURL(object);
      const url = `blob:${location.origin}/mse-test-${object.generation}`;
      objects.set(url, object);
      return url;
    };
    HTMLMediaElement.prototype.load = function () {
      state.ready = 0;
      state.time = 0;
      const source = objects.get(sources.get(this));
      if (source) queueMicrotask(() => {
        if ((persistent || source.generation === 1) && fault === "sourceopen") { state.reached = true; return; }
        source.readyState = "open";
        source.dispatchEvent(new Event("sourceopen"));
      });
    };
    HTMLMediaElement.prototype.play = function () {
      state.paused = false;
      this.dispatchEvent(new Event("play"));
      if (state.ready >= 2) this.dispatchEvent(new Event("playing"));
      return Promise.resolve();
    };
    HTMLMediaElement.prototype.pause = function () { state.paused = true; this.dispatchEvent(new Event("pause")); };
    for (const [name, get, set] of [
      ["src", function () { return sources.get(this) || ""; }, function (value) { sources.set(this, value); }],
      ["currentTime", () => state.time, (value) => { state.time = value; }],
      ["readyState", () => state.ready], ["paused", () => state.paused], ["seeking", () => state.seeking],
      ["duration", () => 600], ["videoWidth", () => 0],
      ["buffered", () => ({ length: state.ready ? 1 : 0, start: () => 0, end: () => 600 })],
    ]) Object.defineProperty(HTMLMediaElement.prototype, name, { configurable: true, get, set });
    const getAttribute = Element.prototype.getAttribute;
    const removeAttribute = Element.prototype.removeAttribute;
    HTMLMediaElement.prototype.getAttribute = function (name) {
      return name === "src" ? sources.get(this) || null : getAttribute.call(this, name);
    };
    HTMLMediaElement.prototype.removeAttribute = function (name) {
      if (name === "src") sources.delete(this);
      return removeAttribute.call(this, name);
    };
    const originalFetch = window.fetch;
    window.fetch = async (input, options) => {
      const url = new URL(input, location.href);
      if (url.pathname.startsWith("/api/web/transcode/")) {
        if (options?.method === "DELETE") state.cancelled.push(url.searchParams.get("request"));
        return new Response(JSON.stringify({ schema_version: 2, state: state.status,
          produced_seconds: state.produced, retry_after_seconds: 0.25 }), { headers: { "Content-Type": "application/json" } });
      }
      if (!url.pathname.startsWith("/web/media/")) return originalFetch(input, options);
      const delivery = url.searchParams.get("delivery");
      if (delivery === "mse") state.requests.push(url.searchParams.get("request"));
      if ((persistent || state.generations === 1) && fault === "playlist headers" && delivery === "mse") {
        state.reached = true;
        options.signal.addEventListener("abort", () => state.aborted.push(url.searchParams.get("request")), { once: true });
        await new Promise((resolve) => { state.releaseHeaders = resolve; });
      }
      if (delivery !== "mse") return new Response(new Uint8Array([1]));
      const init = new URL(url); init.pathname = init.pathname.replace(/\.m3u8$/, ".mp4");
      init.searchParams.set("delivery", "mse_init"); init.searchParams.set("hls_offset", "0"); init.searchParams.set("hls_length", "1");
      const fragment = new URL(init); fragment.pathname = fragment.pathname.replace(/\.mp4$/, ".m4s");
      fragment.searchParams.set("delivery", "mse_segment"); fragment.searchParams.set("hls_offset", "1");
      return new Response(`#EXTM3U\n#EXT-X-MAP:URI="${init}"\n#EXTINF:2,\n${fragment}\n#EXT-X-ENDLIST\n`);
    };
  }, { fault, persistent, hevc });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const item of payload.entries || []) {
      if (item.entry_type !== "media" || item.kind !== "video") continue;
      Object.assign(item, { duration_seconds: 600, duration: "0:10:00.000", stream_metadata_complete: true,
        video_codec: hevc ? "hevc" : "other", codec_string: hevc ? "hvc1.1.6.L120.B0,ac-3" : "other,ac-3",
        video_repair_required: false, video_content_type: hevc ? 'video/mp4; codecs="hvc1.1.6.L120.B0"' : null,
        audio_codec: "ac3", audio_tracks: [{ index: 0, codec: "ac3", channels: 6, default: true,
          content_type: 'audio/mp4; codecs="ac-3"' }] });
    }
    await route.fulfill({ response, json: payload });
  });
  await page.goto("/");
  await expect(page.locator("#loading")).toBeHidden();
  await page.clock.install();
  await page.getByRole("tab", { name: "Videos" }).click();
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect.poll(() => page.evaluate(() => window.__msePlayerFault.reached)).toBe(true);
}

for (const fault of ["playlist headers", "sourceopen", "append", "first frame", "playback progress"]) {
  test(`MSE player recovers a stalled ${fault} through its shared retry`, async ({ page }) => {
    await installPlayerFault(page, { fault });
    await page.locator("#video-player").evaluate((player) => {
      player.playbackRate = 1.5;
      player.volume = 0.35;
      player.muted = true;
    });
    if (fault === "playback progress") await page.clock.runFor(1_000);
    await page.clock.fastForward(fault === "playlist headers" ? 121_000 : 21_000);
    await expect.poll(() => page.evaluate(() => window.__msePlayerFault.cancelled.length)).toBe(1);
    await page.clock.fastForward(1_000);
    await expect.poll(() => page.evaluate(() => window.__msePlayerFault.generations)).toBe(2);
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
    await expect.poll(() => page.locator("#video-player").evaluate((player) => player.readyState)).toBe(3);
    expect(await page.locator("#video-player").evaluate((player) => [player.playbackRate, player.volume, player.muted]))
      .toEqual([1.5, 0.35, true]);
    if (fault === "playlist headers") {
      await expect.poll(() => page.evaluate(() => window.__msePlayerFault.aborted.length)).toBe(1);
    }
    await expect(page.locator("#player-message[role=alert]")).toBeHidden();
  });
}

test("MSE first-frame recovery adopts a HEVC producer once, then cancels abandoned and terminal generations", async ({ page }) => {
  await installPlayerFault(page, { fault: "first frame", hevc: true, persistent: true });
  await page.clock.fastForward(21_000);
  await expect.poll(() => page.evaluate(() => window.__msePlayerFault.generations)).toBe(2);
  expect(await page.evaluate(() => window.__msePlayerFault.cancelled)).toEqual([]);
  for (let attempt = 0; attempt < 7; attempt += 1) {
    await page.clock.fastForward(21_000);
    await page.waitForTimeout(100);
    await page.clock.fastForward(1_000);
    if (await page.locator("#player-message[role=alert]").isVisible()) break;
  }
  await expect(page.locator("#player-message[role=alert]")).toBeVisible();
  const state = await page.evaluate(() => window.__msePlayerFault);
  expect(state.generations).toBeLessThanOrEqual(6);
  expect(state.cancelled).not.toContain(state.requests[0]);
  expect(state.cancelled).toContain(state.requests.at(-1));
  const generations = state.generations;
  await page.clock.fastForward(300_000);
  expect(await page.evaluate(() => window.__msePlayerFault.generations)).toBe(generations);
});

test("MSE watchdog preserves deliberate pause and gives an active seek preparation grace", async ({ page }) => {
  await installPlayerFault(page, { fault: "playback progress" });
  await page.locator("#video-player").evaluate((player) => player.pause());
  await page.clock.fastForward(180_000);
  expect(await page.evaluate(() => window.__msePlayerFault.generations)).toBe(1);
  await page.locator("#video-player").evaluate((player) => {
    player.play();
    // The browser owns this transient seek state; fragments can still arrive.
    window.__msePlayerFault.seeking = true;
    player.dispatchEvent(new Event("seeking"));
  });
  await page.clock.fastForward(60_000);
  expect(await page.evaluate(() => window.__msePlayerFault.generations)).toBe(1);
});

for (const phase of ["headers", "body"]) {
  test(`MSE recovery status cannot hang on ${phase}`, async ({ page }) => {
    await page.goto("/");
    await page.clock.install();
    await page.evaluate(async (phase) => {
      const { WebApi } = await import("/web/api.js");
      const original = window.fetch;
      const state = window.__mseStatusDeadline = { reached: false, aborted: false, error: null };
      window.fetch = (_, options) => {
        options.signal.addEventListener("abort", () => { state.aborted = true; }, { once: true });
        state.reached = true;
        return phase === "headers" ? new Promise(() => {})
          : Promise.resolve(new Response(new ReadableStream({ start(stream) {
            stream.enqueue(new TextEncoder().encode('{"schema_version":2,'));
            options.signal.addEventListener("abort", () => stream.error(new DOMException("Aborted", "AbortError")), { once: true });
          } })));
      };
      void new WebApi().transcodeStatus(1, 1, 1).catch((error) => { state.error = error.code; })
        .finally(() => { window.fetch = original; });
    }, phase);
    await expect.poll(() => page.evaluate(() => window.__mseStatusDeadline.reached)).toBe(true);
    await page.clock.fastForward(15_001);
    await expect.poll(() => page.evaluate(() => window.__mseStatusDeadline.error)).toBe("network");
    expect(await page.evaluate(() => window.__mseStatusDeadline.aborted)).toBe(true);
  });
}


test("MSE slow preparation keeps its grace and starts when the first playlist arrives", async ({ page }) => {
  await installPlayerFault(page, { fault: "playlist headers" });
  await page.clock.fastForward(90_000);
  expect(await page.evaluate(() => window.__msePlayerFault.generations)).toBe(1);
  expect(await page.evaluate(() => window.__msePlayerFault.cancelled)).toEqual([]);
  await page.evaluate(() => window.__msePlayerFault.releaseHeaders());
  await expect.poll(() => page.locator("#video-player").evaluate((player) => player.readyState)).toBe(3);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
});

test("MSE gives a new seek its preparation window after long decoded playback", async ({ page }) => {
  await installPlayerFault(page, { fault: "playback progress" });
  await page.clock.runFor(1_000);
  await page.evaluate(() => { window.__msePlayerFault.time = 100; });
  await page.clock.fastForward(360_000);
  expect(await page.evaluate(() => window.__msePlayerFault.generations)).toBe(1);
  await page.evaluate(() => { window.__msePlayerFault.seeking = true; });
  await page.clock.runFor(30_000);
  expect(await page.evaluate(() => window.__msePlayerFault.generations)).toBe(1);
  expect(await page.evaluate(() => window.__msePlayerFault.cancelled)).toEqual([]);
});
