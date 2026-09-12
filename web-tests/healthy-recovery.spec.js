import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { expect, test } from "@playwright/test";

const execFileAsync = promisify(execFile);
const fixtures = new Map();

test.afterEach(async ({ page }, testInfo) => {
  if (testInfo.status === testInfo.expectedStatus) return;
  const evidence = await page.evaluate(() => ({ ...window.__healthyRecovery,
    media: [...document.querySelectorAll("video")].map((video) => ({ time: video.currentTime,
      paused: video.paused, seeking: video.seeking, readyState: video.readyState, error: video.error?.code,
      src: video.currentSrc, duration: video.duration })),
  })).catch(() => null);
  await testInfo.attach("decoded-recovery-evidence", { contentType: "application/json", body: JSON.stringify(evidence, null, 2) });
});

async function decodedVideo(fragmented) {
  if (fixtures.has(fragmented)) return fixtures.get(fragmented);
  // Bounded generated media uses memory or a cleaned temporary directory;
  // checksum-locked fixtures and the operator's library stay unchanged.
  const args = [
    "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=15",
    "-t", "90", "-an", "-c:v", "libx264", "-profile:v", "baseline", "-level:v", "1.0",
    "-preset", "ultrafast", "-tune", "zerolatency", "-g", "15", "-keyint_min", "15", "-sc_threshold", "0",
  ];
  const fixture = (async () => {
    if (fragmented) {
      const { stdout } = await execFileAsync("ffmpeg", [...args,
        "-movflags", "frag_keyframe+empty_moov+delay_moov+default_base_moof", "-f", "mp4", "pipe:1",
      ], { encoding: null, timeout: 15_000, maxBuffer: 4 * 1024 * 1024 });
      return stdout;
    }
    const directory = await mkdtemp(join(tmpdir(), "rustydlna-healthy-video-"));
    try {
      const path = join(directory, "healthy.mp4");
      await execFileAsync("ffmpeg", [...args, "-movflags", "+faststart", path], { timeout: 15_000, maxBuffer: 1024 * 1024 });
      return await readFile(path);
    } finally { await rm(directory, { recursive: true, force: true }); }
  })();
  fixtures.set(fragmented, fixture);
  return fixture;
}

async function setup(page, { producerState = () => "producing", fragmented = true } = {}) {
  const bytes = await decodedVideo(fragmented);
  const item = {
    entry_type: "media", id: "1", title: "Healthy recovery fixture", kind: "video",
    mime: "video/mp4", source_url: "/media/1.mp4", fallback_url: "/web/media/1.mp4",
    duration_seconds: 600, stream_metadata_complete: true, audio_tracks: [],
    captions: [], chapters: [], video_codec: "h264", width: 160, height: 90,
  };
  await page.addInitScript(() => {
    localStorage.setItem("rustydlna.stream", "compat");
    localStorage.setItem("rustydlna.muted", "true");
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function fixtureDelivery(contentType) {
      return String(contentType).includes("mpegurl") ? "" : canPlayType.call(this, contentType);
    };
    if (globalThis.MediaSource) Object.defineProperty(MediaSource, "isTypeSupported", { value: () => false });
  });
  await page.route("**/api/web/library?**", (route) => route.fulfill({ json: {
    schema_version: 2, server_name: "Recovery", root_folder_id: "0",
    capabilities: { transcoding: true, quality_profiles: [{ id: "auto", label: "Auto" }] },
    library_state: "ready", entries: [item], total: 1, offset: 0, limit: 200, generation: 1, has_more: false,
  } }));
  await page.route("**/api/web/item/*", (route) => route.fulfill({ json: {
    schema_version: 2, generation: 1, id: item.id, item, audio_tracks: [], chapters: [],
  } }));
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({ json: {
    schema_version: 2, state: producerState(), retry_after_seconds: 0.01,
  } }));
  const generations = new Set();
  await page.route("**/web/media/*.mp4?**", (route) => {
    generations.add(new URL(route.request().url()).searchParams.get("request"));
    const range = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || "");
    const start = range ? Number(range[1]) : 0;
    const end = range?.[2] ? Math.min(Number(range[2]), bytes.length - 1) : bytes.length - 1;
    return route.fulfill({ status: range ? 206 : 200, contentType: "video/mp4",
      headers: { "Accept-Ranges": "bytes", ...(range ? { "Content-Range": `bytes ${start}-${end}/${bytes.length}` } : {}) },
      body: bytes.subarray(start, end + 1) });
  });
  await page.goto("/?view=video");
  await page.evaluate(async () => {
    const { PlaybackSource } = await import("/web/playback-source.js");
    const watch = PlaybackSource.prototype.watchHealthyProgress;
    window.__healthyRecovery = { renewals: [], starts: [], events: [] };
    const video = document.querySelector("video");
    for (const name of ["playing", "waiting", "seeking", "seeked", "pause", "error", "ended", "loadedmetadata", "durationchange"]) video.addEventListener(name, (event) => {
      window.__healthyRecovery.events.push({ name, time: video.currentTime, paused: video.paused,
        seeking: video.seeking, readyState: video.readyState, now: performance.now(),
        ended: video.ended, duration: video.duration, trusted: event.isTrusted,
        session: window.__healthyRecovery.starts.at(-1)?.session,
        buffered: Array.from({ length: video.buffered.length }, (_, index) => [video.buffered.start(index), video.buffered.end(index)]),
      });
      if (window.__healthyRecovery.events.length > 64) window.__healthyRecovery.events.shift();
    });
    PlaybackSource.prototype.watchHealthyProgress = function observedWatch(options) {
      window.__healthyRecovery.starts.push({ session: this.sessionId, now: performance.now() });
      window.__healthyRecovery.renewCurrent = () => {
        this.mediaSourceRetry = true;
        this.reloads = 1;
        const before = { attachmentSpent: this.mediaSourceRetry, reloads: this.reloads,
          negotiation: structuredClone(this.plan.streamNegotiation), quality: this.plan.outputQuality };
        options.onHealthy();
        return { before, after: { attachmentSpent: this.mediaSourceRetry, reloads: this.reloads,
          negotiation: this.plan.streamNegotiation, quality: this.plan.outputQuality } };
      };
      return watch.call(this, { ...options, onHealthy: () => {
        window.__healthyRecovery.renewals.push({ session: this.sessionId, now: performance.now(),
          time: this.player.currentTime, frames: this.player.getVideoPlaybackQuality?.().totalVideoFrames });
        options.onHealthy();
      } });
    };
  });
  await page.getByRole("button", { name: "Play Healthy recovery fixture" }).click();
  return generations;
}

async function decodedBurst(page) {
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  await page.locator("video").evaluate((video) => new Promise((resolve) => {
    const initial = video.currentTime;
    const advance = () => {
      if (video.currentTime >= initial + 0.4 && !video.paused && video.readyState >= 2) {
        video.removeEventListener("timeupdate", advance);
        resolve();
      }
    };
    video.addEventListener("timeupdate", advance);
  }));
}

async function failConnection(page, generations, expected) {
  await decodedBurst(page);
  await page.locator("video").evaluate((video) => video.dispatchEvent(new Event("error")));
  await expect.poll(() => generations.size).toBe(expected);
}

test("brief real decoded recoveries still exhaust the three generation retries", async ({ page }) => {
  const generations = await setup(page);
  for (const expected of [2, 3, 4]) await failConnection(page, generations, expected);
  await decodedBurst(page);
  await page.locator("video").evaluate((video) => video.dispatchEvent(new Event("error")));
  await expect(page.locator("#player-message-text")).toContainText("could not prepare this title");
  expect(generations.size).toBe(4);
  expect(await page.evaluate(() => window.__healthyRecovery.renewals)).toEqual([]);
});

test("native prepared-stream seek completion preserves paused intent and settles readiness", async ({ page }) => {
  const generations = await setup(page);
  await decodedBurst(page);
  await page.locator("video").evaluate((video) => video.pause());
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await page.locator("video").evaluate((video) => { video.currentTime = 3; });
  await expect.poll(() => page.locator("video").evaluate((video) => !video.seeking && video.currentTime >= 3)).toBe(true);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await expect(page.locator("#player-message")).toBeHidden();
  expect(await page.locator("video").evaluate((video) => video.paused)).toBe(true);
  expect(generations.size).toBe(1);
  expect(await page.evaluate(() => window.__healthyRecovery.renewals)).toEqual([]);
});

test("healthy renewal restores HEVC attachment recovery while retaining codec, quality and startup limits", async ({ page }) => {
  const generations = await setup(page);
  await decodedBurst(page);
  const before = await page.locator("video").evaluate((video) => ({ src: video.src, time: video.currentTime, paused: video.paused }));
  const result = await page.evaluate(async () => {
    // Control only the health notification here. Real decoded timing is
    // exercised separately; no HEVC hardware decoder is claimed by this test.
    const budgets = window.__healthyRecovery.renewCurrent();
    const { compatibleDecodeRecovery } = await import("/web/core.js");
    const context = {
      item: { kind: "video" }, negotiation: { video: "transcode", audio: "transcode", videoOutput: "hevc_hdr10" },
      mediaCode: 3, producerState: "producing", mediaSourceDelivery: true,
      hdrEncodingSupported: false, androidMediaSourceSupported: false,
      profiles: [], quality: "uhd_high", preferredQuality: "uhd_high",
    };
    return { budgets,
      spent: compatibleDecodeRecovery({ ...context, mediaSourceRetry: budgets.before.attachmentSpent }),
      renewed: compatibleDecodeRecovery({ ...context, mediaSourceRetry: budgets.after.attachmentSpent }),
    };
  });
  expect(result.budgets.after).toEqual({ ...result.budgets.before, attachmentSpent: false });
  expect(result.spent.streamNegotiation.videoOutput).toBe("h264_sdr");
  expect(result.renewed.streamNegotiation).toEqual({ video: "transcode", audio: "transcode", videoOutput: "hevc_hdr10" });
  expect(result.renewed).toMatchObject({ mediaSourceRetry: true, preservePreviousTranscode: true, quality: "uhd_high" });
  expect(generations.size).toBe(1);
  await expect.poll(() => page.locator("video").evaluate((video) => video.src)).toBe(before.src);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  expect(await page.locator("video").evaluate((video) => video.paused)).toBe(before.paused);
  await expect.poll(() => page.locator("video").evaluate((video) => video.currentTime)).toBeGreaterThan(before.time + 0.2);
});

test("confirmed admission renews queue waiting without renewing spent generation retries", async ({ page }) => {
  let state = "producing";
  const generations = await setup(page, { producerState: () => state });
  await page.evaluate(() => { Date.now = () => 10_000; });
  await failConnection(page, generations, 2);
  state = "queued";
  await failConnection(page, generations, 3);
  await page.evaluate(() => { Date.now = () => 309_999; });
  await failConnection(page, generations, 4);
  state = "ready";
  // The recovery status query proves this generation was admitted. It may
  // start a new generation but must retain the already spent failure retry.
  await failConnection(page, generations, 5);
  state = "queued";
  await page.evaluate(() => { Date.now = () => 310_001; });
  await failConnection(page, generations, 6);
  state = "producing";
  await failConnection(page, generations, 7);
  await decodedBurst(page);
  await page.locator("video").evaluate((video) => video.dispatchEvent(new Event("error")));
  await expect(page.locator("#player-message-text")).toContainText("could not prepare this title");
  expect(generations.size).toBe(7);
  expect(await page.evaluate(() => window.__healthyRecovery.renewals)).toEqual([]);
});

test("sustained real decoded playback renews retries for a later independent outage", async ({ page }) => {
  test.setTimeout(90_000);
  // A finalized index keeps the three deliberately injected outages distinct:
  // Firefox can clamp a native fMP4 seek to the temporarily parsed tail and
  // emit an additional early ended. Native fragmented seek coverage remains
  // in the cases above and the delivery suites.
  const generations = await setup(page, { fragmented: false });
  for (const expected of [2, 3]) await failConnection(page, generations, expected);
  await decodedBurst(page);
  await expect.poll(() => page.evaluate(() => window.__healthyRecovery.renewals.length), {
    timeout: 45_000, intervals: [250, 500, 1_000],
  }).toBe(1);
  const evidence = await page.evaluate(() => window.__healthyRecovery);
  const renewal = evidence.renewals[0];
  expect(renewal.now - evidence.starts.find((entry) => entry.session === renewal.session).now).toBeGreaterThanOrEqual(30_000);
  expect(renewal.time).toBeGreaterThan(30);
  if (Number.isFinite(renewal.frames)) expect(renewal.frames).toBeGreaterThan(30);
  for (const expected of [4, 5, 6]) await failConnection(page, generations, expected);
  await decodedBurst(page);
  await page.locator("video").evaluate((video) => video.dispatchEvent(new Event("error")));
  await expect(page.locator("#player-message-text")).toContainText("could not prepare this title");
  expect(generations.size).toBe(6);
  expect(await page.evaluate(() => window.__healthyRecovery.renewals.length)).toBe(1);
});
