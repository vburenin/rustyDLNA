import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";

import { expect, test } from "./diagnostics.js";
import { captionConversionFixtures } from "./caption-conversion-fixtures.mjs";

const execFileAsync = promisify(execFile);

async function indexedCaptionUrls(request) {
  let captions;
  await expect.poll(async () => {
    const response = await request.get("/api/web/library?view=library&kind=video&q=&sort=title&offset=0&limit=60");
    if (!response.ok()) return false;
    const payload = await response.json();
    captions = payload.entries.find((entry) => entry.captions?.some((caption) => caption.language === "zza"))?.captions;
    return captionConversionFixtures.every((fixture) => captions?.some((caption) => caption.language === fixture.language));
  }).toBe(true);
  return new Map(captions.map((caption) => [caption.language, caption.url]));
}

test("actual caption conversion produces browser cue times, displayed text, and SAMI gaps", async ({ page, request }) => {
  const urls = await indexedCaptionUrls(request);
  const directory = await mkdtemp(join(tmpdir(), "rustydlna-caption-media-"));
  let media;
  try {
    const path = join(directory, "caption.mp4");
    await execFileAsync("ffmpeg", [
      "-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=black:size=160x90:rate=5",
      "-t", "8", "-an", "-c:v", "libx264", "-preset", "ultrafast", "-profile:v", "baseline", "-pix_fmt", "yuv420p",
      "-movflags", "faststart", path,
    ], { timeout: 10_000, maxBuffer: 1024 * 1024 });
    media = await readFile(path);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
  await page.route("**/caption-conversion-media.mp4", (route) => {
    const range = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || "");
    const start = range ? Number(range[1]) : 0;
    const end = range?.[2] ? Math.min(Number(range[2]), media.length - 1) : media.length - 1;
    return route.fulfill({
      status: range ? 206 : 200,
      contentType: "video/mp4",
      headers: {
        "Accept-Ranges": "bytes",
        ...(range ? { "Content-Range": `bytes ${start}-${end}/${media.length}` } : {}),
      },
      body: media.subarray(start, end + 1),
    });
  });
  await page.goto("/");
  await page.setContent('<video id="caption-video" muted playsinline preload="auto" src="/caption-conversion-media.mp4"></video><div id="caption-display"></div>');
  await expect.poll(() => page.locator("video").evaluate((video) => video.readyState)).toBeGreaterThanOrEqual(2);

  for (const fixture of captionConversionFixtures.filter((fixture) => !fixture.error)) {
    await test.step(fixture.name, async () => {
      const response = await request.get(urls.get(fixture.language));
      expect(response.status()).toBe(200);
      expect(response.headers()["content-type"]).toContain("text/vtt");
      const cues = await page.evaluate(async (url) => {
        const video = document.querySelector("video");
        video.querySelectorAll("track").forEach((track) => track.remove());
        document.querySelector("#caption-display").replaceChildren();
        const element = document.createElement("track");
        element.kind = "subtitles";
        element.src = url;
        const loaded = new Promise((resolve, reject) => {
          const timer = setTimeout(() => reject(new Error("caption track load timed out")), 7_000);
          element.addEventListener("load", () => { clearTimeout(timer); resolve(); }, { once: true });
          element.addEventListener("error", () => { clearTimeout(timer); reject(new Error("caption track failed")); }, { once: true });
        });
        video.append(element);
        element.track.mode = "showing";
        element.track.addEventListener("cuechange", () => {
          if (!element.isConnected) return;
          const display = document.querySelector("#caption-display");
          display.replaceChildren();
          for (const cue of element.track.activeCues || []) display.append(cue.getCueAsHTML());
        });
        await loaded;
        return Array.from(element.track.cues || [], (cue) => {
          const html = cue.getCueAsHTML();
          return { start: cue.startTime, end: cue.endTime, text: html.textContent, id: cue.id, bold: html.querySelector("b")?.textContent, line: cue.line, position: cue.position };
        });
      }, urls.get(fixture.language));
      expect(cues).toHaveLength(fixture.cues.length);
      for (const [index, expected] of fixture.cues.entries()) expect(cues[index]).toMatchObject(expected);
      const active = fixture.active ?? (fixture.cues.length ? [[(fixture.cues[0].start + fixture.cues[0].end) / 2, fixture.cues[0].text]] : []);
      for (const [time, text] of active) {
        await page.locator("video").evaluate((video, time) => { video.currentTime = time; }, time);
        await expect.poll(() => page.locator("video").evaluate((video) => video.currentTime)).toBeCloseTo(time, 2);
        await expect.poll(() => page.locator("#caption-display").textContent()).toBe(text);
      }
    });
  }
});

test("actual caption conversion rejects malformed signatures and timings with bounded errors", async ({ request }) => {
  const urls = await indexedCaptionUrls(request);
  for (const fixture of captionConversionFixtures.filter((fixture) => fixture.error)) {
    await test.step(fixture.name, async () => {
      const response = await request.get(urls.get(fixture.language));
      expect(response.status()).toBe(422);
      const text = await response.text();
      expect(text.length).toBeLessThan(512);
      const payload = JSON.parse(text);
      expect(payload.error.code).toBe(fixture.error);
      expect(payload.error.message).toBe("The caption file is malformed.");
      expect(text).not.toContain("/tmp/");
    });
  }
});
