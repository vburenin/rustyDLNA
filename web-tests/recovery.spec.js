import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { expect, test } from "./diagnostics.js";

const execFileAsync = promisify(execFile);
const item = {
  entry_type: "media", id: "1", title: "Recovery fixture", kind: "video",
  mime: "video/mp4", source_url: "/media/1.mp4", fallback_url: "/web/media/1.mp4",
  duration_seconds: 600, stream_metadata_complete: true, audio_tracks: [],
  captions: [], chapters: [], video_codec: "h264", width: 160, height: 90,
};

async function syntheticLibrary(page, entry = item, entries = [entry]) {
  const contentType = entry.audio_tracks.length
    ? 'video/mp4; codecs="avc1.42C00A,mp4a.40.2"' : 'video/mp4; codecs="avc1.42C00A"';
  const capabilities = {
    transcoding: true, quality_profiles: [{ id: "auto", label: "Auto" }],
    video_outputs: [{ id: "h264_sdr", video_content_type: contentType, mse_content_type: contentType }],
  };
  await page.route("**/api/web/library?**", (route) => route.fulfill({ json: {
    schema_version: 2, server_name: "Recovery", root_folder_id: "0", capabilities,
    library_state: "ready", entries, total: entries.length, offset: 0, limit: 200, generation: 1, has_more: false,
  } }));
  await page.route("**/api/web/item/*", (route) => route.fulfill({ json: {
    schema_version: 2, generation: 1, id: entry.id, item: entry, audio_tracks: entry.audio_tracks, chapters: [],
  } }));
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({ json: {
    schema_version: 2, state: "producing", retry_after_seconds: null,
  } }));
  await page.addInitScript(() => localStorage.setItem("rustydlna.muted", "true"));
}

async function nativeFragments(page, beforeFragment = async () => {}, { audio = false } = {}) {
  // Generate bounded test media without modifying checksum-locked fixtures.
  // Twelve independent one-second fragments make a seven-second local seek
  // impossible until several real fragments have reached the decoder.
  const { stdout: bytes } = await execFileAsync("ffmpeg", [
    "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25",
    ...(audio ? ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"] : []),
    "-t", "12", ...(audio ? ["-c:a", "aac", "-ac", "2"] : ["-an"]),
    "-c:v", "libx264", "-profile:v", "baseline", "-level:v", "1.0",
    "-preset", "ultrafast", "-tune", "zerolatency", "-g", "25", "-keyint_min", "25", "-sc_threshold", "0",
    "-movflags", "frag_keyframe+empty_moov+delay_moov+default_base_moof", "-f", "mp4", "pipe:1",
  ], { encoding: null, timeout: 10_000, maxBuffer: 1024 * 1024 });
  const offsets = [];
  for (let offset = 0; offset + 8 <= bytes.length;) {
    const size = bytes.readUInt32BE(offset);
    if (size < 8 || offset + size > bytes.length) throw new Error("Invalid synthetic MP4 box");
    if (bytes.toString("ascii", offset + 4, offset + 8) === "moof") offsets.push(offset);
    offset += size;
  }
  expect(offsets).toHaveLength(12);
  const init = bytes.subarray(0, offsets[0]);
  const fragments = offsets.map((offset, index) => bytes.subarray(offset, offsets[index + 1] ?? bytes.length));
  const requests = [];
  await page.route("**/web/media/*", async (route) => {
    const url = new URL(route.request().url());
    requests.push(url);
    if (url.pathname.endsWith(".m3u8")) {
      const resource = (suffix, delivery, index) => {
        const result = new URL(url);
        result.pathname = `/web/media/1.${suffix}`;
        result.searchParams.set("delivery", delivery);
        result.searchParams.set("hls_offset", String(delivery === "mse_init" ? 0 : offsets[index]));
        result.searchParams.set("hls_length", String(delivery === "mse_init" ? init.length : fragments[index].length));
        return result.href;
      };
      return route.fulfill({ body: `#EXTM3U\n#EXT-X-MAP:URI="${resource("mp4", "mse_init", 0)}"\n`
        + fragments.map((_, index) => `#EXTINF:1,\n${resource("m4s", "mse_segment", index)}\n`).join("")
        + "#EXT-X-ENDLIST\n" });
    }
    await beforeFragment(url);
    return route.fulfill({ contentType: "video/mp4", body: url.pathname.endsWith(".mp4")
      ? init : fragments[offsets.indexOf(Number(url.searchParams.get("hls_offset")))] });
  });
  await page.addInitScript(() => {
    localStorage.setItem("rustydlna.stream", "compat");
    window.__nativeStarts = [];
    document.addEventListener("playing", (event) => {
      if (event.target instanceof HTMLMediaElement) {
        window.__nativeStarts.push({ source: event.target.src, time: event.target.currentTime });
      }
    }, true);
  });
  return requests;
}

for (const target of [48.99, 6369]) {
  for (const paused of [false, true]) {
    test(`native Media Source finishes an AAC-primed seek to ${target} while ${paused ? "paused" : "playing"}`, async ({ page, browserName }) => {
      test.skip(browserName !== "chromium", "Native fragmented MP4 regression runs in Chromium.");
      await syntheticLibrary(page, { ...item, duration_seconds: 8294, audio_codec: "aac", audio_tracks: [{ index: 0, codec: "aac", channels: 2 }] });
      await nativeFragments(page, undefined, { audio: true });
      await selectFixture(page);
      if (paused) await page.locator("#play-button").evaluate((button) => button.click());
      await seek(page, target);
      await exactNativeTime(page, target, paused);
      await expect.poll(() => page.locator("video").evaluate((video) => (
        !video.seeking && video.readyState >= HTMLMediaElement.HAVE_FUTURE_DATA
      ))).toBe(true);
      if (!paused) {
        await expect.poll(() => page.locator("video").evaluate((video) => video.currentTime)).toBeGreaterThan(target % 10 + 0.1);
      }
    });
  }
}

async function selectFixture(page) {
  await page.goto("/?view=video");
  await page.getByRole("button", { name: "Play Recovery fixture" }).click();
  await expect.poll(() => page.locator("video").evaluate((video) => video.readyState)).toBeGreaterThanOrEqual(3);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
}

async function seek(page, target) {
  await page.locator("#timeline").evaluate((timeline, value) => {
    timeline.value = String(value);
    timeline.dispatchEvent(new Event("input"));
    timeline.dispatchEvent(new Event("change"));
  }, target);
}

async function exactNativeTime(page, global, paused) {
  const local = global % 10;
  await expect.poll(() => page.locator("video").evaluate((video) => video.currentTime)).toBeGreaterThanOrEqual(local);
  const actual = await page.locator("video").evaluate((video) => ({ time: video.currentTime, paused: video.paused }));
  expect(actual.time).toBeLessThan(local + (paused ? 0.05 : 2));
  expect(actual.paused).toBe(paused);
  if (!paused) {
    await expect.poll(() => page.locator("video").evaluate((video) => (
      window.__nativeStarts.find((entry) => entry.source === video.src)?.time ?? -1
    ))).toBeGreaterThanOrEqual(local);
  }
  await expect.poll(async () => Math.abs(Number(await page.locator("#timeline").inputValue())
    - (global - local + await page.locator("video").evaluate((video) => video.currentTime)))).toBeLessThan(0.4);
}

for (const paused of [true, false]) {
  test(`native Media Source fulfills an exact seek while ${paused ? "paused" : "playing"}`, async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "Native fragmented MP4 decoding regression runs in desktop and mobile Chromium.");
    await syntheticLibrary(page);
    const requests = await nativeFragments(page);
    await selectFixture(page);
    if (paused) {
      await page.locator("#play-button").evaluate((button) => button.click());
      await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    }
    await seek(page, 47);
    await exactNativeTime(page, 47, paused);
    if (paused) {
      const fetched = requests.filter((url) => url.searchParams.get("start") === "40");
      // The decoder may need one more fragment to settle its asynchronous seek.
      const fragments = fetched.filter((url) => url.searchParams.get("delivery") === "mse_segment");
      expect(fragments.length).toBeGreaterThanOrEqual(8);
      expect(fragments.length).toBeLessThanOrEqual(9);
      const count = requests.length;
      await page.waitForTimeout(600);
      expect(requests).toHaveLength(count);
      await exactNativeTime(page, 47, true);
    }
  });
}

for (const start of ["deep link", "saved resume"]) {
  test(`native Media Source applies an exact ${start} before playing`, async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "Native fragmented MP4 decoding regression runs in desktop and mobile Chromium.");
    await syntheticLibrary(page);
    await nativeFragments(page);
    if (start === "saved resume") {
      await page.addInitScript(() => localStorage.setItem("rustydlna.webProgress.v1", JSON.stringify({
        1: { position: 47, duration: 600, updated: Date.now() },
      })));
      await page.goto("/?view=video");
      await page.getByRole("button", { name: "Play Recovery fixture" }).click();
      await page.locator("#resume-button").click();
    } else {
      await page.goto("/?view=video&item=1&t=47");
    }
    await exactNativeTime(page, 47, false);
  });
}

test("native Media Source discards an exact seek when a newer source replaces it", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Native fragmented MP4 decoding regression runs in desktop and mobile Chromium.");
  await syntheticLibrary(page);
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  let held = false;
  await nativeFragments(page, async (url) => {
    if (url.searchParams.get("start") === "40") { held = true; await gate; }
  });
  await selectFixture(page);
  await page.locator("#play-button").evaluate((button) => button.click());
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await seek(page, 47);
  await expect.poll(() => held).toBe(true);
  await seek(page, 67);
  await exactNativeTime(page, 67, true);
  release();
  await page.waitForTimeout(300);
  await exactNativeTime(page, 67, true);
});

for (const { mode, explicit = false } of [
  { mode: "auto" }, { mode: "compat" }, { mode: "original" },
  { mode: "auto", explicit: true }, { mode: "compat", explicit: true },
]) {
  test(`metadata retry applies or accurately reports enriched audio in ${mode}${explicit ? " with an explicit default choice" : ""}`, async ({ page }) => {
    const tracks = [{ index: 3, codec: "aac", language: "spa", default: true }, { index: 7, codec: "aac", language: "eng" }];
    const entry = { ...item, stream_metadata_complete: false, default_audio_index: 3, audio_tracks: explicit ? tracks : [] };
    await syntheticLibrary(page, entry);
    await page.addInitScript((stream) => {
      localStorage.setItem("rustydlna.stream", stream);
      window.MediaSource = undefined;
      HTMLMediaElement.prototype.canPlayType = (type) => type.includes("mpegurl") ? "" : "probably";
      const nativeLoad = HTMLMediaElement.prototype.load;
      HTMLMediaElement.prototype.load = function loadFixture() {
        Object.defineProperty(this, "currentTime", { configurable: true, writable: true, value: 0 });
        nativeLoad.call(this);
      };
      HTMLMediaElement.prototype.play = function playFixture() {
        Object.defineProperty(this, "paused", { configurable: true, value: false });
        this.dispatchEvent(new Event("playing"));
        return Promise.resolve();
      };
      HTMLMediaElement.prototype.pause = function pauseFixture() {
        Object.defineProperty(this, "paused", { configurable: true, value: true });
        this.dispatchEvent(new Event("pause"));
      };
    }, mode === "original" ? "direct" : mode);
    let attempts = 0;
    await page.route("**/api/web/item/*?enrich=1", (route) => {
      attempts += 1;
      if (attempts === 1) return route.fulfill({ status: 503, json: {
        schema_version: 2, error: { code: "transcode_busy", message: "busy", recoverable: true, action: "retry_item" },
      } });
      return route.fulfill({ json: {
        schema_version: 2, item: { ...entry, default_audio_index: 7 },
        audio_tracks: [{ index: 7, codec: "aac", language: "eng" }, { index: 3, codec: "aac", language: "spa", default: true }], chapters: [],
      } });
    });
    const sources = [];
    const fixture = await readFile(new URL("../testdata/library/video/tagged.mp4", import.meta.url));
    await page.route(/\/(?:web\/)?media\/1\.mp4\?/, (route) => {
      sources.push(new URL(route.request().url()));
      return route.fulfill({ contentType: "video/mp4", body: fixture });
    });
    await page.goto("/?view=video");
    await page.getByRole("button", { name: "Play Recovery fixture" }).click();
    await expect.poll(() => sources.length).toBe(1);
    await expect.poll(() => page.locator("video").evaluate((video) => video.readyState)).toBeGreaterThanOrEqual(3);
    await page.locator("video").evaluate((video) => {
      video.currentTime = 47;
      video.dispatchEvent(new Event("timeupdate"));
    });
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
    await page.locator("#play-button").evaluate((button) => button.click());
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    await page.locator("#advanced-playback-button").evaluate((button) => button.click());
    if (explicit) await page.locator("#audio-track-controls").selectOption("3");
    await page.locator("#audio-track-retry").click();
    await expect(page.locator("#audio-track-retry")).toBeHidden();
    await expect(page.locator("#audio-track-controls")).toHaveValue(mode === "original" || explicit ? "3" : "7");
    if (mode === "original") {
      expect(sources).toHaveLength(1);
    } else {
      await expect.poll(() => sources.length).toBe(2);
      const prepared = mode === "compat" || !explicit;
      expect(sources[1].pathname).toBe(prepared ? "/web/media/1.mp4" : "/media/1.mp4");
      expect(sources[1].searchParams.get("audio")).toBe(prepared ? explicit ? "3" : "7" : null);
      expect(sources[1].searchParams.get("start")).toBe(prepared ? "40" : null);
      expect(Number(sources[1].searchParams.get("request"))).toBeGreaterThan(Number(sources[0].searchParams.get("request")));
      if (mode === "compat") expect(sources[1].searchParams.get("session")).toBe(sources[0].searchParams.get("session"));
    }
    expect(await page.locator("video").evaluate((video) => video.paused)).toBe(true);
    await expect(page.locator("#timeline")).toHaveValue("47");
    expect(attempts).toBe(2);
  });
}

for (const entryPoint of ["seek", "deep link"]) {
  test(`native Media Source end boundary settles without a replacement from ${entryPoint}`, async ({ page, browserName }) => {
    test.skip(browserName !== "chromium", "Native fragmented MP4 decoding regression runs in desktop and mobile Chromium.");
    await syntheticLibrary(page, { ...item, duration_seconds: 12 });
    const requests = await nativeFragments(page);
    if (entryPoint === "seek") {
      await selectFixture(page);
      const count = requests.filter((url) => url.pathname.endsWith(".m3u8")).length;
      await seek(page, 12);
      await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Replay");
      expect(requests.filter((url) => url.pathname.endsWith(".m3u8"))).toHaveLength(count);
    } else {
      await page.goto("/?view=video&item=1&t=12");
      await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Replay");
      expect(requests).toHaveLength(0);
    }
    await expect(page.locator("#timeline")).toHaveValue("12");
    expect(await page.locator("video").evaluate((video) => video.paused)).toBe(true);
    await page.locator("#play-button").evaluate((button) => button.click());
    await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
    expect(await page.locator("video").evaluate((video) => video.currentTime)).toBeLessThan(2);
  });
}

async function originalVideoFixture() {
  const directory = await mkdtemp(join(tmpdir(), "rustydlna-original-seek-"));
  try {
    const path = join(directory, "original.mp4");
    await execFileAsync("ffmpeg", [
      "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25",
      "-t", "12", "-an", "-c:v", "libx264", "-profile:v", "baseline", "-g", "25",
      "-movflags", "+faststart", path,
    ], { timeout: 10_000, maxBuffer: 1024 * 1024 });
    return await readFile(path);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

for (const entryPoint of ["seek", "deep link"]) {
  test(`Original playback seeks backward from an explicit end reached by ${entryPoint}`, async ({ page }) => {
    await syntheticLibrary(page, { ...item, duration_seconds: 12 });
    const media = await originalVideoFixture();
    await page.route("**/media/1.mp4?**", (route) => {
      const range = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || "");
      const start = range ? Number(range[1]) : 0;
      const end = range?.[2] ? Math.min(Number(range[2]), media.length - 1) : media.length - 1;
      return route.fulfill({
        status: range ? 206 : 200, contentType: "video/mp4", body: media.subarray(start, end + 1),
        headers: { "Accept-Ranges": "bytes", ...(range ? { "Content-Range": `bytes ${start}-${end}/${media.length}` } : {}) },
      });
    });
    await page.addInitScript(() => localStorage.setItem("rustydlna.stream", "direct"));
    if (entryPoint === "seek") {
      await selectFixture(page);
      await seek(page, 12);
    } else {
      await page.goto("/?view=video&item=1&t=12");
    }
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Replay");
    await seek(page, 5);
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    await expect.poll(() => page.locator("video").evaluate((video) => video.currentTime)).toBeCloseTo(5, 1);
    expect(await page.locator("video").evaluate((video) => video.paused)).toBe(true);
    await page.locator("#play-button").evaluate((button) => button.click());
    await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
    await expect.poll(async () => Number(await page.locator("#timeline").inputValue())).toBeGreaterThanOrEqual(5);
    await expect.poll(() => page.locator("video").evaluate((video) => video.currentTime)).toBeGreaterThan(5.1);
    // A second pause exercises the replacement source's event listeners.
    await page.locator("#play-button").evaluate((button) => button.click());
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  });
}

for (const producerState of ["failed", "producing"]) {
  test(`a truncated portable stream with a ${producerState} producer preserves progress and stops without advancing the queue`, async ({ page }, testInfo) => {
    await syntheticLibrary(page, item, [item, { ...item, id: "2", title: "Next fixture" }]);
    await page.addInitScript(() => {
      const events = window.__recoveryMediaEvents = [];
      const record = (type, video, detail = null) => {
        events.push({ type, detail, at: performance.now(), time: video.currentTime, paused: video.paused,
          ended: video.ended, ready: video.readyState, seeking: video.seeking,
          request: video.src ? new URL(video.src).searchParams.get("request") : null });
        if (events.length > 128) events.shift();
      };
      for (const type of ["play", "playing", "pause", "ended", "loadedmetadata", "canplay", "error", "seeked"]) {
        document.addEventListener(type, (event) => {
          if (event.target instanceof HTMLMediaElement) record(type, event.target, event.isTrusted);
        }, true);
      }
      const play = HTMLMediaElement.prototype.play;
      HTMLMediaElement.prototype.play = function (...args) {
        record("play-call", this);
        return play.apply(this, args).then((value) => { record("play-resolved", this); return value; },
          (error) => { record("play-rejected", this, error.name); throw error; });
      };
      localStorage.setItem("rustydlna.stream", "compat");
      localStorage.setItem("rustydlna.autoplay", "true");
      localStorage.setItem("rustydlna.webProgress.v1", JSON.stringify({
        1: { position: 40, duration: 600, updated: Date.now() },
      }));
      HTMLMediaElement.prototype.canPlayType = () => "";
      if (globalThis.MediaSource) Object.defineProperty(MediaSource, "isTypeSupported", { value: () => false });
      Object.defineProperty(navigator, "mediaCapabilities", { value: {
        decodingInfo: async () => ({ supported: false, smooth: false, powerEfficient: false }),
      } });
    });
    let checksAfterEnd = 0;
    const initialQueries = new Set();
    await page.route("**/api/web/transcode/*", async (route) => {
      const requestId = new URL(route.request().url()).searchParams.get("request");
      if (route.request().method() === "GET" && !initialQueries.has(requestId)) {
        initialQueries.add(requestId);
        return route.fulfill({ json: { schema_version: 2, state: "producing" } });
      }
      const ended = route.request().method() === "GET"
        && await page.locator("video").evaluate((video) => video.ended);
      if (ended) {
        checksAfterEnd += 1;
        // Duplicate media failures while the producer query is pending must
        // share recovery, without consuming retries before a new source loads.
        await page.locator("video").evaluate((video) => {
          video.dispatchEvent(new Event("error"));
          video.dispatchEvent(new Event("ended"));
        });
      }
      await route.fulfill({ json: { schema_version: 2, state: ended ? producerState : "producing" } });
    });
    const media = await readFile(new URL("../testdata/library/video/tagged.mp4", import.meta.url));
    const requests = [];
    await page.route("**/web/media/*", async (route) => {
      requests.push(new URL(route.request().url()));
      await route.fulfill({ contentType: "video/mp4", body: media });
    });
    await page.goto("/?view=video");
    await page.getByRole("button", { name: "Play Recovery fixture" }).click();
    await expect(page.locator("#queue-position")).toHaveText("Item 1 of 2");
    await page.locator("#resume-button").click();
    await expect(page.locator("#player-message")).toContainText("The server could not prepare this title.").catch(async (error) => {
      await testInfo.attach("recovery-media-events", { contentType: "application/json",
        body: JSON.stringify(await page.evaluate(() => window.__recoveryMediaEvents), null, 2) });
      throw error;
    });
    const expectedGenerations = producerState === "failed" ? 1 : 4;
    expect(checksAfterEnd).toBeGreaterThan(0);
    // Native loaders may request a URL several times (for example for ranges).
    // Count prepared generations, whose retry budget is shared across errors.
    expect(new Set(requests.map((url) => url.searchParams.get("request"))).size).toBe(expectedGenerations);
    expect(requests[0].searchParams.get("video_mode")).toBe("transcode");
    expect(requests[0].searchParams.get("audio_mode")).toBe("transcode");
    await expect(page.locator("#timeline")).not.toHaveValue("600");
    await expect(page.locator("#queue-position")).toHaveText("Item 1 of 2");
    await expect(page.locator("#now-playing-title")).toHaveText("Recovery fixture");
    const progress = await page.evaluate(() => JSON.parse(localStorage.getItem("rustydlna.webProgress.v1") || "{}")[1]);
    expect(progress?.position).toBe(40);
    expect(progress?.duration).toBe(600);
  });
}
