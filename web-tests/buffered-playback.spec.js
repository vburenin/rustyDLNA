import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { expect, test } from "@playwright/test";

const execFileAsync = promisify(execFile);
const contentType = 'video/mp4; codecs="avc1.4d400b,mp4a.40.2"';
const item = { entry_type: "media", id: "1", title: "Buffered GOP fixture", kind: "video", mime: "video/mp4",
  source_url: "/web/media/1.mp4?mode=direct", fallback_url: "/web/media/1.mp4", duration_seconds: 600,
  stream_metadata_complete: true, audio_tracks: [{ index: 0, codec: "aac", channels: 2 }],
  captions: [], chapters: [], video_codec: "h264", audio_codec: "aac", width: 160, height: 90,
  video_content_type: 'video/mp4; codecs="avc1.4d400b"', audio_content_type: 'audio/mp4; codecs="mp4a.40.2"' };

async function fixture(page, { preview = false } = {}) {
  const { stdout: bytes } = await execFileAsync("ffmpeg", ["-nostdin", "-v", "error", "-f", "lavfi", "-i",
    "testsrc2=size=160x90:rate=25", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "20",
    "-c:a", "aac", "-ac", "2", "-c:v", "libx264", "-profile:v", "main", "-level:v", "1.1", "-preset", "veryfast",
    "-bf", "2", "-g", "125", "-keyint_min", "125", "-sc_threshold", "0", "-frag_duration", "1000000",
    "-movflags", "frag_keyframe+empty_moov+delay_moov+default_base_moof", "-f", "mp4", "pipe:1"],
  { encoding: null, timeout: 10000, maxBuffer: 2 * 1024 * 1024 });
  const offsets = [];
  for (let offset = 0; offset + 8 <= bytes.length;) {
    const size = bytes.readUInt32BE(offset);
    if (size < 8 || offset + size > bytes.length) throw new Error("Invalid generated MP4");
    if (bytes.toString("ascii", offset + 4, offset + 8) === "moof") offsets.push(offset);
    offset += size;
  }
  const init = bytes.subarray(0, offsets[0]);
  const fragments = offsets.map((offset, index) => bytes.subarray(offset, offsets[index + 1] ?? bytes.length));
  const entry = { ...item, ...(preview ? { preview_url: "/api/web/preview/1" } : {}) };
  const capabilities = { transcoding: true, mse_resource_max_bytes: 33554432,
    quality_profiles: [{ id: "auto", label: "Auto" }],
    video_outputs: [{ id: "h264_sdr", video_content_type: contentType, mse_content_type: contentType }] };
  await page.route("**/api/web/library?**", (route) => route.fulfill({ json: { schema_version: 2, server_name: "Buffered",
    root_folder_id: "0", capabilities, library_state: "ready", entries: [entry], total: 1, offset: 0,
    limit: 200, generation: 1, has_more: false } }));
  await page.route("**/api/web/item/*", (route) => route.fulfill({ json: { schema_version: 2, item: entry, generation: 1,
    audio_tracks: entry.audio_tracks, chapters: [] } }));
  const cancelled = [];
  let producerState = "producing";
  await page.route("**/api/web/transcode/*", (route) => {
    if (route.request().method() === "DELETE") cancelled.push(route.request().url());
    return route.fulfill({ json: { schema_version: 2, state: producerState } });
  });
  const requests = [];
  await page.route("**/web/media/*", async (route) => {
    const url = new URL(route.request().url()); requests.push(url);
    if (url.pathname.endsWith(".m3u8")) {
      const resource = (suffix, delivery, index, length) => {
        const target = new URL(url); target.pathname = `/web/media/1.${suffix}`;
        target.searchParams.set("delivery", delivery); target.searchParams.set("hls_offset", String(index));
        target.searchParams.set("hls_length", String(length)); return target.href;
      };
      return route.fulfill({ body: '#EXTM3U\n#EXT-X-MAP:URI="' + resource("mp4", "mse_init", 0, init.length) + '"\n'
        + fragments.map((fragment, index) => `#EXT-X-RUSTY-TIMING:${index},${Math.floor(index / 5) * 5}\n#EXTINF:1,\n${resource("m4s", "mse_segment", index, fragment.length)}\n`).join("")
        + "#EXT-X-ENDLIST\n" });
    }
    return route.fulfill({ contentType: "video/mp4", body: url.pathname.endsWith(".mp4") ? init : fragments[Number(url.searchParams.get("hls_offset"))] });
  });
  await page.addInitScript(() => {
    localStorage.setItem("rustydlna.stream", "compat"); localStorage.setItem("rustydlna.muted", "true");
    Object.defineProperty(navigator, "userAgent", { configurable: true,
      value: "Mozilla/5.0 (Linux; Android 10) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36" });
    Object.defineProperty(navigator, "mediaCapabilities", { configurable: true,
      value: { decodingInfo: async () => ({ supported: true, smooth: true, powerEfficient: true }) } });
  });
  return { requests, cancelled, expire() { producerState = "idle"; } };
}

async function seek(page, time) {
  await page.locator("#timeline").evaluate((timeline, value) => {
    timeline.value = String(value); timeline.dispatchEvent(new Event("input")); timeline.dispatchEvent(new Event("change"));
  }, time);
}

for (const paused of [true, false]) {
  test(`buffered copied B-frame seeks reuse the source at nonzero offset while ${paused ? "paused" : "playing"}`, async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "Real copied H.264 MSE decoding uses desktop and mobile Chromium.");
    const observed = await fixture(page);
    await page.goto("/?view=video&item=1&t=43");
    await expect.poll(() => page.locator("video").evaluate((video) => video.buffered.length ? video.buffered.end(video.buffered.length - 1) : 0)).toBeGreaterThan(10);
    if (paused) await page.locator("video").evaluate((video) => video.pause());
    await page.locator("#loop-button").evaluate((button) => button.click());
    await page.locator("video").evaluate((video) => { video.playbackRate = 1.5; video.volume = 0.35; });
    const originalSource = await page.locator("video").getAttribute("src");
    const originalInits = observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init").length;
    const originalDeletes = observed.cancelled.length;
    for (const target of [47, 44, 46]) {
      await seek(page, target);
      await expect.poll(() => page.locator("video").evaluate((video) => video.seeking)).toBe(false);
      await expect.poll(() => page.locator("video").evaluate((video) => video.currentTime)).toBeGreaterThanOrEqual(target - 40);
      expect(await page.locator("video").evaluate((video) => video.currentTime)).toBeLessThan(target - 40 + (paused ? 0.05 : 1));
      expect(await page.locator("video").getAttribute("src")).toBe(originalSource);
      expect(await page.locator("video").evaluate((video) => video.paused)).toBe(paused);
      expect(await page.locator("video").evaluate((video) => video.playbackRate)).toBe(1.5);
      expect(await page.locator("video").evaluate((video) => video.volume)).toBeCloseTo(0.35);
      await expect(page.locator("#loop-button")).toHaveAttribute("aria-pressed", "true");
    }
    expect(observed.cancelled).toHaveLength(originalDeletes);
    expect(observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init")).toHaveLength(originalInits);
    const timings = await page.evaluate(async () => (await import("/web/playback-timing.js")).playbackTimingSnapshot());
    expect(timings.records.some((record) => record.kind === "seek_buffered" && !record.estimated)).toBe(true);
    await seek(page, 97);
    await expect.poll(() => page.locator("video").getAttribute("src")).not.toBe(originalSource);
    await expect.poll(() => observed.cancelled.length).toBeGreaterThan(originalDeletes);
  });
}

test("rapid buffered direction changes settle the latest paused target and end still detaches", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Real copied H.264 MSE decoding uses Chromium.");
  const observed = await fixture(page);
  await page.goto("/?view=video&item=1&t=43");
  await expect.poll(() => page.locator("video").evaluate((video) => video.buffered.length ? video.buffered.end(video.buffered.length - 1) : 0)).toBeGreaterThan(10);
  await page.locator("video").evaluate((video) => video.pause());
  const source = await page.locator("video").getAttribute("src");
  await page.locator("#timeline").evaluate((timeline) => {
    for (const value of [47, 44, 46, 43, 47]) {
      timeline.value = String(value); timeline.dispatchEvent(new Event("input")); timeline.dispatchEvent(new Event("change"));
    }
  });
  await expect.poll(() => page.locator("video").evaluate((video) => !video.seeking && Math.abs(video.currentTime - 7) < 0.05)).toBe(true);
  expect(await page.locator("video").getAttribute("src")).toBe(source);
  const count = observed.cancelled.length;
  await seek(page, 600);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Replay");
  await expect.poll(() => observed.cancelled.length).toBeGreaterThan(count);
});

test("preview sheets wait for a presented frame and buffer while active scrubbing bypasses the gate", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Real copied H.264 MSE decoding uses Chromium.");
  await fixture(page, { preview: true });
  await page.route("**/api/web/preview/1", (route) => route.fulfill({ json: { schema_version: 2, available: true,
    frame_width: 160, frame_height: 90, columns: 1, rows: 1, frame_count: 100, interval_seconds: 6,
    sheet_urls: Array.from({ length: 100 }, (_, index) => `/web/preview/1/hash/${index}.jpg`) } }));
  const sheets = [];
  await page.route("**/web/preview/1/hash/*.jpg", async (route) => { sheets.push(route.request().url()); await route.abort(); });
  await page.addInitScript(() => { HTMLMediaElement.prototype.play = async () => {}; });
  await page.goto("/?view=video&item=1");
  await expect.poll(() => page.locator("video").evaluate((video) => video.readyState)).toBeGreaterThanOrEqual(2);
  expect(sheets).toHaveLength(0);
  await page.locator("#timeline").evaluate((timeline) => { timeline.value = "60"; timeline.dispatchEvent(new Event("input")); });
  await expect.poll(() => sheets.length).toBeGreaterThan(0);
  expect(sheets.every((url) => url.endsWith("/10.jpg"))).toBe(true);
});

test("an evicted copied GOP takes the restart path", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Real copied H.264 MSE decoding uses Chromium.");
  const observed = await fixture(page);
  await page.addInitScript(() => {
    const add = MediaSource.prototype.addSourceBuffer;
    MediaSource.prototype.addSourceBuffer = function (...args) {
      const buffer = add.apply(this, args); window.__bufferForEviction = buffer; return buffer;
    };
  });
  await page.goto("/?view=video&item=1&t=43");
  await expect.poll(() => page.locator("video").evaluate((video) => video.buffered.length ? video.buffered.end(video.buffered.length - 1) : 0)).toBeGreaterThan(10);
  await page.locator("video").evaluate((video) => video.pause());
  await expect.poll(() => page.evaluate(() => window.__bufferForEviction.updating)).toBe(false);
  const source = await page.locator("video").getAttribute("src");
  await page.evaluate(() => new Promise((resolve) => {
    window.__bufferForEviction.addEventListener("updateend", resolve, { once: true });
    window.__bufferForEviction.remove(0, 5);
  }));
  await seek(page, 44);
  await expect.poll(() => page.locator("video").getAttribute("src")).not.toBe(source);
  // Same ten-second bucket can attach its existing server output, but must
  // reacquire initialization and decoder prerequisites after browser eviction.
  await expect.poll(() => observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init").length).toBe(2);
});

test("a resource limit is reported without starting a lossy codec fallback", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Real MSE delivery negotiation uses Chromium.");
  const observed = await fixture(page);
  await page.route("**/web/media/*.m4s?**", (route) => route.fulfill({ status: 413,
    contentType: "application/json", body: JSON.stringify({ schema_version: 2, error: { code: "resource_limit" } }) }));
  await page.goto("/?view=video&item=1&t=43");
  await expect(page.locator("#player-panel")).toContainText("This stream exceeds the available playback memory.");
  await page.waitForTimeout(500);
  expect(observed.requests.filter((url) => url.pathname.endsWith(".m3u8"))).toHaveLength(1);
  expect(observed.requests.some((url) => url.searchParams.get("video_mode") === "transcode")).toBe(false);
});

test("a delayed initial frame callback completes selection timing and enables previews", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Actual H.264 MSE presentation uses desktop and mobile Chromium.");
  await fixture(page, { preview: true });
  await page.addInitScript(() => {
    const request = HTMLVideoElement.prototype.requestVideoFrameCallback;
    HTMLVideoElement.prototype.requestVideoFrameCallback = function (callback) {
      return request.call(this, (now, frame) => callback(now, { ...frame, mediaTime: Math.max(1, frame.mediaTime) }));
    };
  });
  const { stdout: sheet } = await execFileAsync("ffmpeg", ["-nostdin", "-v", "error", "-f", "lavfi", "-i",
    "color=c=blue:s=160x90", "-frames:v", "1", "-threads", "1", "-c:v", "mjpeg", "-f", "image2pipe", "pipe:1"],
  { encoding: null, timeout: 10_000, maxBuffer: 1024 * 1024 });
  let manifests = 0;
  let sheets = 0;
  await page.route("**/api/web/preview/1", (route) => {
    manifests += 1;
    return route.fulfill({ json: { schema_version: 2, available: true, frame_width: 160, frame_height: 90,
      columns: 1, rows: 1, frame_count: 1, interval_seconds: 6, sheet_urls: ["/web/preview/1/hash/0.jpg"] } });
  });
  await page.route("**/web/preview/1/hash/0.jpg", (route) => {
    sheets += 1;
    return route.fulfill({ contentType: "image/jpeg", body: sheet });
  });
  await page.goto("/?view=video&item=1");
  await expect.poll(() => page.evaluate(async () => (await import("/web/playback-timing.js")).playbackTimingSnapshot()
    .records.some((record) => record.kind === "selection" && !record.estimated))).toBe(true);
  await expect.poll(() => manifests).toBe(1);
  await expect.poll(() => sheets).toBe(1);
});

for (const restarted of [false, true]) {
  test(`a paused ${restarted ? "restarted" : "buffered"} seek retains its only frame before seeked`, async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "Actual copied H.264 MSE buffering uses desktop and mobile Chromium.");
    await fixture(page);
    await page.goto("/?view=video&item=1&t=43");
    await expect.poll(() => page.locator("video").evaluate((video) => video.buffered.length
      ? video.buffered.end(video.buffered.length - 1) : 0)).toBeGreaterThan(10);
    await expect.poll(() => page.evaluate(async () => (await import("/web/playback-timing.js")).playbackTimingSnapshot()
      .records.some((record) => record.kind === "selection" && !record.estimated))).toBe(true);
    await page.locator("video").evaluate((video) => {
      video.pause();
      // Deterministically model an engine that presents the target frame
      // while seeking, then stays paused without another frame callback.
      Object.defineProperty(video, "seeking", { configurable: true, get: () => true });
      window.__seekFrames = { requests: 0, callback: null };
      video.requestVideoFrameCallback = (callback) => {
        window.__seekFrames.requests += 1;
        window.__seekFrames.callback = callback;
        return window.__seekFrames.requests;
      };
      video.cancelVideoFrameCallback = () => { window.__seekFrames.callback = null; };
    });
    await page.locator("#timeline").evaluate((timeline, target) => {
      window.__seekFrames.startedBefore = performance.now();
      timeline.value = String(target);
      timeline.dispatchEvent(new Event("input"));
      timeline.dispatchEvent(new Event("change"));
      window.__seekFrames.startedAfter = performance.now();
    }, restarted ? 97 : 47);
    await expect.poll(() => page.locator("video").evaluate((video) => video.readyState >= 2
      && Math.abs(video.currentTime - 7) < 0.05 && typeof window.__seekFrames.callback === "function")).toBe(true);
    await page.locator("video").evaluate((video) => {
      const frames = window.__seekFrames;
      const callback = frames.callback;
      frames.callback = null;
      frames.observedBefore = performance.now();
      callback(frames.observedBefore, { mediaTime: video.currentTime });
      frames.observedAfter = performance.now();
    });
    await page.waitForTimeout(125);
    const result = await page.locator("video").evaluate(async (video) => {
      const { playbackTimingSnapshot } = await import("/web/playback-timing.js");
      const beforeConfirmation = playbackTimingSnapshot().records.filter((record) => record.kind.startsWith("seek_"));
      delete video.seeking;
      if (video.seeking || !video.paused) throw new Error("The native seek must have settled while paused.");
      const confirmedAt = performance.now();
      video.dispatchEvent(new Event("seeked"));
      video.dispatchEvent(new Event("seeked"));
      return { ...window.__seekFrames, callback: undefined, confirmedAt, beforeConfirmation,
        records: playbackTimingSnapshot().records.filter((record) => record.kind.startsWith("seek_")) };
    });
    expect(result.beforeConfirmation).toHaveLength(0);
    expect(result.requests).toBe(1);
    expect(result.records).toHaveLength(1);
    expect(result.records[0].kind).toBe(restarted ? "seek_restarted" : "seek_buffered");
    expect(result.records[0].estimated).toBe(false);
    expect(result.records[0].duration_ms).toBeGreaterThanOrEqual(result.observedBefore - result.startedAfter);
    expect(result.records[0].duration_ms).toBeLessThanOrEqual(result.observedAfter - result.startedBefore);
    expect(result.records[0].duration_ms).toBeLessThan(result.confirmedAt - result.startedAfter - 100);
  });
}

test("queued pause events cannot change intent after playback resumes", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Actual H.264 MSE playback uses desktop and mobile Chromium.");
  await fixture(page);
  await page.goto("/?view=video&item=1&t=43");
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  await page.locator("video").evaluate((video) => {
    if (video.paused) throw new Error("Fixture must actually be playing before a queued pause arrives.");
    video.dispatchEvent(new Event("pause"));
  });
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  await seek(page, 97);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  await expect.poll(() => page.locator("video").evaluate((video) => video.paused)).toBe(false);
});

for (const relative of [false, true]) {
  test(`an owned background append preserves ${relative ? "rapid relative" : "nearby"} buffered seeks`, async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "Actual H.264 MSE buffering uses desktop and mobile Chromium.");
    const observed = await fixture(page);
    await page.addInitScript(() => {
      window.__mediaActions = {};
      const action = navigator.mediaSession.setActionHandler.bind(navigator.mediaSession);
      navigator.mediaSession.setActionHandler = (name, callback) => { window.__mediaActions[name] = callback; action(name, callback); };
      const add = MediaSource.prototype.addSourceBuffer;
      MediaSource.prototype.addSourceBuffer = function (...args) {
        const buffer = add.apply(this, args);
        const append = buffer.appendBuffer.bind(buffer);
        buffer.addEventListener("updateend", (event) => {
          if (window.__heldAppend?.buffer === buffer && !window.__heldAppend.released) {
            event.stopImmediatePropagation(); window.__heldAppend.complete = true;
          }
        });
        buffer.appendBuffer = (bytes) => {
          if (!window.__heldAppend && buffer.buffered.length && buffer.buffered.end(0) >= 9) {
            window.__heldAppend = { buffer, complete: false, released: false };
            Object.defineProperty(buffer, "updating", { configurable: true, get: () => true });
          }
          append(bytes);
        };
        return buffer;
      };
    });
    await page.goto("/?view=video&item=1&t=43");
    await expect.poll(() => page.evaluate(() => window.__heldAppend?.complete)).toBe(true);
    await page.locator("video").evaluate((video) => video.pause());
    const originalSource = await page.locator("video").getAttribute("src");
    const initializations = observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init").length;
    const playlists = observed.requests.filter((url) => url.pathname.endsWith(".m3u8")).length;
    const cancellations = observed.cancelled.length;
    const target = await page.locator("video").evaluate((video, relative) => {
      const expected = relative ? video.currentTime + 2 : 4;
      window.__heldAppend.beforeSeek = video.currentTime;
      setTimeout(() => {
        const held = window.__heldAppend;
        held.timeAtRelease = video.currentTime;
        held.released = true;
        delete held.buffer.updating;
        held.buffer.dispatchEvent(new Event("updateend"));
      }, 40);
      if (relative) {
        window.__mediaActions.seekforward({ seekOffset: 1 });
        window.__mediaActions.seekforward({ seekOffset: 1 });
      } else {
        const timeline = document.querySelector("#timeline");
        timeline.value = "44"; timeline.dispatchEvent(new Event("input")); timeline.dispatchEvent(new Event("change"));
      }
      return expected;
    }, relative);
    await expect.poll(() => page.locator("video").evaluate((video, target) => !video.seeking
      && Math.abs(video.currentTime - target) < 0.05, target)).toBe(true);
    const heldTimes = await page.evaluate(() => [window.__heldAppend.beforeSeek, window.__heldAppend.timeAtRelease]);
    expect(heldTimes[1]).toBeCloseTo(heldTimes[0], 2);
    expect(await page.locator("video").evaluate((video) => video.paused)).toBe(true);
    expect(await page.locator("video").getAttribute("src")).toBe(originalSource);
    expect(observed.cancelled).toHaveLength(cancellations);
    expect(observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init")).toHaveLength(initializations);
    expect(observed.requests.filter((url) => url.pathname.endsWith(".m3u8"))).toHaveLength(playlists);
  });
}

for (const delayed of [false, true]) {
  test(`idle requested before MSE registration cannot expire playback when delivered ${delayed ? "after first frame" : "before the playlist"}`, async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "Actual H.264 MSE buffering uses desktop and mobile Chromium.");
    const observed = await fixture(page);
    let firstStatus;
    let statusStarted;
    const started = new Promise((resolve) => { statusStarted = resolve; });
    let first = true;
    await page.route("**/api/web/transcode/*", async (route) => {
      if (route.request().method() !== "GET") return route.fallback();
      if (!first) return route.fulfill({ json: { schema_version: 2, state: "producing" } });
      first = false; firstStatus = route; statusStarted();
      if (!delayed) await route.fulfill({ json: { schema_version: 2, state: "idle" } });
    });
    await page.route("**/web/media/*.m3u8?**", async (route) => { await started; await route.fallback(); });
    await page.addInitScript(() => {
      const fetch = window.fetch;
      window.__idleResponses = 0;
      window.fetch = async (...args) => {
        const response = await fetch(...args);
        if (String(args[0]).includes("/api/web/transcode/") && (!args[1]?.method || args[1].method === "GET")) {
          if ((await response.clone().json()).state === "idle") window.__idleResponses += 1;
        }
        return response;
      };
    });
    await page.goto("/?view=video&item=1&t=43");
    await expect.poll(() => page.locator("video").evaluate((video) => video.buffered.length
      ? video.buffered.end(video.buffered.length - 1) : 0)).toBeGreaterThan(10);
    await expect.poll(() => page.evaluate(async () => (await import("/web/playback-timing.js")).playbackTimingSnapshot()
      .records.some((record) => record.kind === "selection" && !record.estimated))).toBe(true);
    if (delayed) await firstStatus.fulfill({ json: { schema_version: 2, state: "idle" } });
    await expect.poll(() => page.evaluate(() => window.__idleResponses)).toBe(1);
    await page.locator("video").evaluate((video) => video.pause());
    const originalSource = await page.locator("video").getAttribute("src");
    const initializations = observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init").length;
    for (const target of [44, 46]) {
      await seek(page, target);
      await expect.poll(() => page.locator("video").evaluate((video, target) => !video.seeking
        && Math.abs(video.currentTime - (target - 40)) < 0.05, target)).toBe(true);
    }
    expect(await page.locator("video").getAttribute("src")).toBe(originalSource);
    expect(observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init")).toHaveLength(initializations);
    expect(observed.cancelled).toHaveLength(0);
  });
}

test("idle requested after owned playlist registration still expires buffered reuse", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Actual H.264 MSE buffering uses desktop and mobile Chromium.");
  const observed = await fixture(page);
  await page.addInitScript(() => {
    const timeout = window.setTimeout;
    window.setTimeout = (callback, milliseconds, ...args) => timeout(callback, milliseconds === 10_000 ? 50 : milliseconds, ...args);
    const fetch = window.fetch;
    window.__idleResponses = 0;
    window.fetch = async (...args) => {
      const response = await fetch(...args);
      if (String(args[0]).includes("/api/web/transcode/") && (!args[1]?.method || args[1].method === "GET")) {
        if ((await response.clone().json()).state === "idle") window.__idleResponses += 1;
      }
      return response;
    };
  });
  await page.goto("/?view=video&item=1&t=43");
  await expect.poll(() => page.locator("video").evaluate((video) => video.buffered.length
    ? video.buffered.end(video.buffered.length - 1) : 0)).toBeGreaterThan(10);
  await page.locator("video").evaluate((video) => video.pause());
  const originalSource = await page.locator("video").getAttribute("src");
  observed.expire();
  await expect.poll(() => page.evaluate(() => window.__idleResponses)).toBeGreaterThan(0);
  await seek(page, 44);
  await expect.poll(() => page.locator("video").getAttribute("src")).not.toBe(originalSource);
  await expect.poll(() => observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init").length).toBe(2);
});

test("saved double speed survives real MSE startup and buffered, paused, and restarted seeks", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Actual H.264 MSE buffering uses desktop and mobile Chromium.");
  const observed = await fixture(page);
  await page.addInitScript(() => localStorage.setItem("rustydlna.rate", "2"));
  await page.goto("/?view=video&item=1&t=43");
  await expect.poll(() => page.evaluate(async () => (await import("/web/playback-timing.js")).playbackTimingSnapshot()
    .records.some((record) => record.kind === "selection" && !record.estimated))).toBe(true);
  const rate = () => page.locator("video").evaluate((video) => ({ actual: video.playbackRate,
    saved: Number(localStorage.getItem("rustydlna.rate")), selected: Number(document.querySelector("#speed-control").value) }));
  expect(await rate()).toEqual({ actual: 2, saved: 2, selected: 2 });
  const progression = await page.locator("video").evaluate((video) => new Promise((resolve) => {
    video.requestVideoFrameCallback((firstAt, first) => {
      const frame = (now, current) => {
        if (now - firstAt < 500) video.requestVideoFrameCallback(frame);
        else resolve((current.mediaTime - first.mediaTime) * 1000 / (now - firstAt));
      };
      video.requestVideoFrameCallback(frame);
    });
  }));
  expect(progression).toBeGreaterThan(1.7);
  expect(progression).toBeLessThan(2.3);
  await expect.poll(() => page.locator("video").evaluate((video) => video.buffered.length
    ? video.buffered.end(video.buffered.length - 1) : 0)).toBeGreaterThan(10);
  const originalSource = await page.locator("video").getAttribute("src");
  await seek(page, 44);
  await expect.poll(() => page.locator("video").evaluate((video) => !video.seeking && video.currentTime >= 4)).toBe(true);
  expect(await rate()).toEqual({ actual: 2, saved: 2, selected: 2 });
  expect(await page.locator("video").getAttribute("src")).toBe(originalSource);
  await page.locator("video").evaluate((video) => video.pause());
  await seek(page, 46);
  await expect.poll(() => page.locator("video").evaluate((video) => !video.seeking && Math.abs(video.currentTime - 6) < 0.05)).toBe(true);
  expect(await page.locator("video").evaluate((video) => video.paused)).toBe(true);
  expect(await rate()).toEqual({ actual: 2, saved: 2, selected: 2 });
  expect(await page.locator("video").getAttribute("src")).toBe(originalSource);
  expect(observed.requests.filter((url) => url.searchParams.get("delivery") === "mse_init")).toHaveLength(1);
  await seek(page, 97);
  await expect.poll(() => page.locator("video").getAttribute("src")).not.toBe(originalSource);
  await expect.poll(() => page.locator("video").evaluate((video) => !video.seeking && Math.abs(video.currentTime - 7) < 0.05)).toBe(true);
  expect(await page.locator("video").evaluate((video) => video.paused)).toBe(true);
  expect(await rate()).toEqual({ actual: 2, saved: 2, selected: 2 });
  await page.locator("#play-button").evaluate((button) => button.click());
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  expect(await rate()).toEqual({ actual: 2, saved: 2, selected: 2 });
});
