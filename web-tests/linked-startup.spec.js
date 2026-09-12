import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { appendFile } from "node:fs/promises";
import { cpus, platform, release, totalmem } from "node:os";
import { promisify } from "node:util";
import { expect, test } from "@playwright/test";

const execFileAsync = promisify(execFile);
let fixture;
let ffmpegVersion;

test.beforeAll(async () => {
  const { stdout } = await execFileAsync("ffmpeg", [
    "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=24",
    "-t", "3", "-an", "-c:v", "libx264", "-threads", "1", "-profile:v", "baseline",
    "-pix_fmt", "yuv420p", "-movflags", "frag_keyframe+empty_moov+default_base_moof",
    "-f", "mp4", "pipe:1",
  ], { encoding: null, timeout: 10_000, maxBuffer: 1024 * 1024 });
  fixture = stdout;
  ffmpegVersion = (await execFileAsync("ffmpeg", ["-version"], { timeout: 5_000, maxBuffer: 64 * 1024 }))
    .stdout.split("\n")[0];
});

async function linkedLibrary(page, request, { total = 400, heldOffset = 200, delay = null, laterError = false } = {}) {
  const response = await request.get("/api/web/library?view=library&kind=video");
  expect(response.ok()).toBe(true);
  const template = await response.json();
  const item = template.entries.find((entry) => entry.title === "tagged");
  expect(item).toBeTruthy();
  let release;
  let heldStarted;
  let mediaRequested = false;
  const held = new Promise((resolve) => { release = resolve; });
  const started = new Promise((resolve) => { heldStarted = resolve; });
  await page.addInitScript(() => {
    localStorage.setItem("rustydlna.stream", "direct");
    window.__linkedTiming = { firstFrame: null, released: null };
    const observer = new MutationObserver(() => {
      const video = document.getElementById("video-player");
      if (!video) return;
      observer.disconnect();
      video.muted = true;
      video.requestVideoFrameCallback(() => { window.__linkedTiming.firstFrame = performance.now(); });
    });
    observer.observe(document, { childList: true, subtree: true });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const offset = Number(new URL(route.request().url()).searchParams.get("offset"));
    if (offset === heldOffset) {
      heldStarted();
      if (delay !== null) {
        await new Promise((resolve) => setTimeout(resolve, delay));
        await page.evaluate(() => { window.__linkedTiming.released = performance.now(); });
      } else await held;
      if (laterError) {
        await route.fulfill({ status: 503, json: {
          schema_version: 2, error: { code: "library_unavailable", message: "Unavailable", recoverable: true },
        } });
        return;
      }
    }
    await route.fulfill({ json: {
      ...template, total, offset, limit: 200, has_more: offset + 200 < total,
      entries: Array.from({ length: Math.min(200, total - offset) }, (_, index) => ({
        ...item,
        id: offset + index === 0 ? item.id : String(100_000 + offset + index),
        title: offset + index === 0 ? item.title : `Generated card ${offset + index}`,
        collection: null, art_url: null,
      })),
    } });
  });
  await page.route("**/web/media/**", async (route) => {
    mediaRequested = true;
    await route.fulfill({ contentType: "video/mp4", body: fixture });
  });
  return { item, template, started, release, mediaRequested: () => mediaRequested };
}

for (const [total, heldOffset] of [[400, 200], [10_000, 9800]]) {
  test(`linked playback starts while page ${heldOffset / 200 + 1} of ${total / 200} is held`, async ({ page, request }) => {
    test.setTimeout(60_000);
    const library = await linkedLibrary(page, request, { total, heldOffset });
    try {
      await page.goto(`/?view=video&item=${library.item.id}`);
      await library.started;
      await expect.poll(library.mediaRequested).toBe(true);
      await expect.poll(() => page.evaluate(() => window.__linkedTiming.firstFrame)).not.toBeNull();
      await expect(page.locator("#loading")).toBeVisible();
      await expect(page.locator("#queue-position")).toHaveText("Item 1 of 1");
      await expect(page.locator("#next-button")).toBeDisabled();
    } finally {
      library.release();
    }
    // Startup above must finish before the held response. Rendering all 10k
    // cards after release is a separate workload: WebKit tracing can spend
    // more than 7.5 seconds settling that DOM even after the response arrives.
    await expect(page.locator("#loading")).toBeHidden({ timeout: total >= 10_000 ? 30_000 : 7500 });
    await expect(page.locator("#queue-position")).toHaveText("Item 1 of 1");
    await page.locator(".card-button").first().click();
    await expect(page.locator("#queue-position")).toHaveText(`Item 1 of ${total}`);
  });
}

test("a later library page failure does not prevent linked playback or replace its singleton queue", async ({ page, request }) => {
  const library = await linkedLibrary(page, request, { laterError: true });
  await page.goto(`/?view=video&item=${library.item.id}`);
  await library.started;
  await expect.poll(library.mediaRequested).toBe(true);
  library.release();
  await expect(page.locator("#library-empty-title")).toHaveText("Could not load the library");
  await expect(page.locator("#now-playing-title")).toHaveText("tagged");
  await expect.poll(() => page.evaluate(() => window.__linkedTiming.firstFrame)).not.toBeNull();
  await expect(page.locator("#queue-position")).toHaveText("Item 1 of 1");
});

test("linked metadata is rechecked against first-page generation before playback", async ({ page, request }) => {
  const library = await linkedLibrary(page, request);
  let releaseItem;
  let checkingGeneration;
  const held = new Promise((resolve) => { releaseItem = resolve; });
  const rechecked = new Promise((resolve) => { checkingGeneration = resolve; });
  await page.route(`**/api/web/item/${library.item.id}*`, async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.has("generation")) {
      expect(url.searchParams.get("generation")).toBe(String(library.template.generation));
      checkingGeneration();
      await held;
      await route.fallback();
    } else {
      const response = await route.fetch();
      const payload = await response.json();
      await route.fulfill({ response, json: { ...payload, generation: library.template.generation - 1 } });
    }
  });
  try {
    await page.goto(`/?view=video&item=${library.item.id}`);
    await rechecked;
    expect(library.mediaRequested()).toBe(false);
    releaseItem();
    await expect.poll(library.mediaRequested).toBe(true);
  } finally {
    releaseItem();
    library.release();
  }
});

test("new navigation aborts a linked generation recheck before it can start media", async ({ page, request }) => {
  const library = await linkedLibrary(page, request);
  let releaseItem;
  let checkingGeneration;
  const held = new Promise((resolve) => { releaseItem = resolve; });
  const rechecked = new Promise((resolve) => { checkingGeneration = resolve; });
  await page.route(`**/api/web/item/${library.item.id}*`, async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    if (new URL(route.request().url()).searchParams.has("generation")) {
      checkingGeneration();
      await held;
    } else payload.generation = library.template.generation - 1;
    await route.fulfill({ response, json: payload });
  });
  try {
    await page.goto(`/?view=video&item=${library.item.id}`);
    await rechecked;
    await page.getByRole("tab", { name: "Audio", exact: true }).click();
    releaseItem();
    library.release();
    await expect(page.locator("#loading")).toBeHidden();
    await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
    expect(library.mediaRequested()).toBe(false);
    expect(new URL(page.url()).searchParams.has("item")).toBe(false);
  } finally {
    releaseItem();
    library.release();
  }
});

for (const responseKind of ["catalog-precondition", "mismatched-response"]) {
  test(`linked enrichment rejects a server catalog change (${responseKind})`, async ({ page, request }) => {
    const library = await linkedLibrary(page, request);
    let requestedGeneration;
    await page.route(`**/api/web/item/${library.item.id}*`, async (route) => {
      const url = new URL(route.request().url());
      if (url.searchParams.get("enrich") !== "1") {
        await route.fulfill({ json: { schema_version: 2, generation: library.template.generation,
          item: { ...library.item, stream_metadata_complete: false }, chapters: [] } });
        return;
      }
      requestedGeneration = url.searchParams.get("generation");
      // The browser still has the original first page. Simulate a server-only
      // publication between its plain item request and enriched metadata.
      if (responseKind === "catalog-precondition" && requestedGeneration !== null) {
        await route.fulfill({ status: 409, json: { schema_version: 2, error: {
          code: "catalog_changed", message: "Library changed", recoverable: true,
        } } });
      } else {
        await route.fulfill({ json: { schema_version: 2, generation: library.template.generation + 1,
          item: { ...library.item, title: "Stale enriched title", stream_metadata_complete: true },
          audio_tracks: [], chapters: [] } });
      }
    });
    try {
      await page.goto(`/?view=video&item=${library.item.id}`);
      await expect(page.locator("#player-empty-text")).toHaveText("This linked title is not available. Choose another item.");
      expect(requestedGeneration).toBe(String(library.template.generation));
      expect(library.mediaRequested()).toBe(false);
      await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
    } finally {
      library.release();
    }
  });
}

test("library retry cannot select held metadata from the previous capability generation", async ({ page, request }) => {
  const response = await request.get("/api/web/library?view=library&kind=video");
  const library = await response.json();
  const item = library.entries.find((entry) => entry.title === "tagged");
  let generation = library.generation;
  let firstPage = true;
  let releaseOldItem;
  let releaseCurrentItem;
  let currentItemRequested;
  const oldItem = new Promise((resolve) => { releaseOldItem = resolve; });
  const currentItem = new Promise((resolve) => { releaseCurrentItem = resolve; });
  const recheck = new Promise((resolve) => { currentItemRequested = resolve; });
  let mediaRequests = 0;
  await page.route("**/api/web/library?**", async (route) => {
    const offset = Number(new URL(route.request().url()).searchParams.get("offset"));
    if (offset > 0) return route.fulfill({ status: 503, json: {
      schema_version: 2, error: { code: "library_unavailable", message: "Unavailable", recoverable: true },
    } });
    if (!firstPage) generation += 1;
    const total = firstPage ? 400 : 1;
    firstPage = false;
    await route.fulfill({ json: { ...library, generation, total, offset: 0, limit: 200,
      has_more: total > 200,
      entries: Array.from({ length: Math.min(200, total) }, (_, index) => ({ ...item, id: String(Number(item.id) + index), art_url: null })),
    } });
  });
  await page.route(`**/api/web/item/${item.id}*`, async (route) => {
    const requested = new URL(route.request().url()).searchParams.get("generation");
    const snapshot = requested === null ? library.generation : Number(requested);
    if (requested === null) await oldItem;
    else {
      expect(Number(requested)).toBe(generation);
      currentItemRequested();
      await currentItem;
    }
    await route.fulfill({ json: { schema_version: 2, generation: snapshot, item, chapters: [] } });
  });
  await page.route("**/web/media/**", async (route) => {
    mediaRequests += 1;
    await route.fulfill({ contentType: "video/mp4", body: fixture });
  });
  await page.addInitScript(() => localStorage.setItem("rustydlna.stream", "direct"));
  try {
    await page.goto(`/?view=video&item=${item.id}`);
    await expect(page.locator("#library-empty-title")).toHaveText("Could not load the library");
    await page.locator("#library-retry").click();
    await expect(page.locator("#loading")).toBeHidden();
    releaseOldItem();
    await recheck;
    expect(mediaRequests).toBe(0);
    await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
    releaseCurrentItem();
    await expect.poll(() => mediaRequests).toBeGreaterThan(0);
    await expect(page.locator("#now-playing-title")).toHaveText("tagged");
  } finally {
    releaseOldItem();
    releaseCurrentItem();
  }
});

// Opt-in controlled-latency measurements use the same generated bytes and
// browser clock before/after. Each JSONL record is a sample, not a percentile.
if (process.env.RUSTY_DLNA_LINK_BENCHMARK_REPORT) {
  for (const [total, heldOffset] of [[400, 200], [10_000, 9800]]) {
    test(`measure linked startup for ${total} cards`, async ({ page, request, browser }, testInfo) => {
      test.setTimeout(60_000);
      const library = await linkedLibrary(page, request, { total, heldOffset, delay: 600 });
      await page.goto(`/?view=video&item=${library.item.id}`);
      await expect.poll(() => page.evaluate(() => window.__linkedTiming.firstFrame), { timeout: 45_000 }).not.toBeNull();
      await expect(page.locator("#loading")).toBeHidden({ timeout: total >= 10_000 ? 30_000 : 7500 });
      const timing = await page.evaluate(() => ({
        ...window.__linkedTiming,
        mediaRequest: performance.getEntriesByType("resource")
          .find((entry) => entry.name.includes("/web/media/"))?.startTime ?? null,
      }));
      await appendFile(process.env.RUSTY_DLNA_LINK_BENCHMARK_REPORT, `${JSON.stringify({
        browser: testInfo.project.name, total, heldOffset, pageDelayMs: 600,
        browserVersion: browser.version(), nodeVersion: process.version, ffmpegVersion,
        hardware: { cpu: cpus()[0]?.model, logicalCpus: cpus().length, memoryBytes: totalmem(),
          os: `${platform()} ${release()}` },
        conditions: "Fresh browser context; generated metadata and media served from memory; host caches retained",
        fixtureSha256: createHash("sha256").update(fixture).digest("hex"),
        fixtureBytes: fixture.byteLength, recipe: "generated SDR H264 baseline 320x180 24fps no audio",
        ...timing,
      })}\n`);
    });
  }
}
