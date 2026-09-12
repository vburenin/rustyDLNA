import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";

import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

const execFileAsync = promisify(execFile);
let playingMedia;

async function captionPlaybackMedia() {
  playingMedia ||= (async () => {
    // The tiny catalog fixture can reach its loop boundary during assertions.
    // Give caption failure/retry a continuous, indexed decoded video instead.
    const directory = await mkdtemp(join(tmpdir(), "rustydlna-caption-playback-"));
    try {
      const path = join(directory, "captions.mp4");
      await execFileAsync("ffmpeg", [
        "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=15",
        "-t", "30", "-an", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
        "-movflags", "+faststart", path,
      ], { timeout: 15_000, maxBuffer: 1024 * 1024 });
      return await readFile(path);
    } finally { await rm(directory, { recursive: true, force: true }); }
  })();
  return playingMedia;
}

const timelineVtt = `WEBVTT

opening
00:00:00.000 --> 00:00:00.350
Opening scene

crossing
00:01:29.500 --> 00:01:30.150 align:start position:20%
Crossing the source start

two-minutes
00:02:00.000 --> 00:02:00.350
Scene at two minutes

`;

async function captionFixture(page, { mode = "direct", playing = false } = {}) {
  await page.addInitScript(({ mode, playing }) => {
    localStorage.setItem("rustydlna.stream", mode);
    localStorage.setItem("rustydlna.loop", "true");
    if (!playing) HTMLMediaElement.prototype.play = () => Promise.resolve();
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function fixtureCanPlayType(contentType) {
      return String(contentType).includes("mpegurl") ? "" : canPlayType.call(this, contentType);
    };
    if (typeof MediaSource === "function") {
      Object.defineProperty(MediaSource, "isTypeSupported", { configurable: true, value: () => false });
    }
  }, { mode, playing });
  const media = playing ? await captionPlaybackMedia()
    : await readFile(new URL("../testdata/library/video/tagged.mp4", import.meta.url));
  const requests = { media: [], cancellations: [] };
  await page.route("**/web/media/*.mp4?**", async (route) => {
    requests.media.push(route.request().url());
    const range = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || "");
    const start = range ? Number(range[1]) : 0;
    const end = range?.[2] ? Math.min(Number(range[2]), media.length - 1) : media.length - 1;
    await route.fulfill({
      status: range ? 206 : 200,
      contentType: "video/mp4",
      headers: {
        "accept-ranges": "bytes",
        ...(range ? { "content-range": `bytes ${start}-${end}/${media.length}` } : {}),
      },
      body: media.subarray(start, end + 1),
    });
  });
  page.on("request", (request) => {
    if (request.method() === "DELETE" && request.url().includes("/api/web/transcode/")) {
      requests.cancellations.push(request.url());
    }
  });
  const response = await page.request.get("/api/web/library?view=library&kind=video&q=&sort=title&offset=0&limit=60");
  const payload = await response.json();
  const item = payload.entries.find((entry) => entry.title === "tagged");
  expect(item).toBeTruthy();
  await page.route("**/api/web/item/**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    if (String(payload.item?.id) === String(item.id)) {
      Object.assign(payload.item, {
        duration_seconds: 600, duration: "0:10:00.000", stream_metadata_complete: true,
        captions: [
          { index: 0, browser_supported: true, label: "English", language: "en", url: "/caption-controls.vtt" },
          { index: 1, browser_supported: false, label: "Unsupported", source_format: "sub" },
          { index: 2, browser_supported: true, label: "French", language: "fr", url: "/caption-controls-fr.vtt" },
        ],
      });
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/caption-controls-fr.vtt", (route) => route.fulfill({ contentType: "text/vtt", body: timelineVtt }));
  return { item, requests };
}

async function openCaptions(page) {
  const button = page.locator("#captions-button");
  await expect(button).toBeEnabled();
  await page.locator("#player-stage").scrollIntoViewIfNeeded();
  await page.locator("#player-stage").hover();
  await button.focus();
  await expect(button).toBeFocused();
  await button.press("Enter");
  await expect(page.locator("#caption-menu")).toBeVisible();
}

async function expectCue(page, expected) {
  const track = page.locator('#video-player track[data-caption-index="0"]');
  await expect.poll(() => track.evaluate((node) => node.track.mode)).toBe("showing");
  await expect.poll(() => track.evaluate((node) => {
    const cue = node.track.cues?.[0];
    return cue && {
      id: cue.id, start: cue.startTime, end: Math.round(cue.endTime * 1000) / 1000,
      text: cue.getCueAsHTML().textContent,
    };
  })).toEqual(expected);
}

test("caption Space and Arrow selection retain radio nodes, keyboard focus and visible controls", async ({ page }) => {
  const { item } = await captionFixture(page);
  await page.route("**/caption-controls.vtt", (route) => route.fulfill({ contentType: "text/vtt", body: timelineVtt }));
  await page.goto(`/?view=video&item=${item.id}`);
  await openCaptions(page);
  const english = page.getByRole("radio", { name: "English", exact: true });
  const french = page.getByRole("radio", { name: "French", exact: true });
  await english.evaluate((radio) => { window.__englishRadio = radio; });
  await english.focus();
  await page.keyboard.press("Space");
  await expect(english).toBeChecked();
  await expect(english).toBeFocused();
  expect(await english.evaluate((radio) => radio === window.__englishRadio)).toBe(true);
  await expect(page.locator("#captions-button")).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator("#captions-button")).toHaveAttribute("aria-expanded", "true");
  await page.keyboard.press("ArrowDown");
  await expect(french).toBeChecked();
  await expect(french).toBeFocused();
  await page.keyboard.press("ArrowUp");
  await expect(english).toBeChecked();
  await expect(english).toBeFocused();
  await page.locator("#player-stage").dispatchEvent("pointerleave");
  await expect(page.locator("#player-stage")).toHaveClass(/controls-visible/);
  expect(await english.evaluate((radio) => {
    const style = getComputedStyle(radio);
    return { style: style.outlineStyle, width: style.outlineWidth };
  })).toEqual({ style: "solid", width: "3px" });
  await page.keyboard.press("ArrowUp");
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeChecked();
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeFocused();
  await expect(page.locator("#captions-button")).toHaveAttribute("aria-pressed", "false");
});

test("long caption lists stay inside the player with first and last choices reachable", async ({ page, request }) => {
  await page.addInitScript(() => { HTMLMediaElement.prototype.play = () => Promise.resolve(); });
  const response = await request.get("/api/web/library?view=library&kind=video&q=&sort=title&offset=0&limit=60");
  const { entries } = await response.json();
  const item = entries.find((entry) => entry.captions?.some((caption) => caption.language === "zza"));
  expect(item.captions.length).toBeGreaterThan(20);
  await page.goto(`/?view=video&item=${item.id}`);
  await openCaptions(page);
  const bounds = await page.locator("#caption-menu").evaluate((menu) => {
    const rect = menu.getBoundingClientRect();
    const stage = document.querySelector("#player-stage").getBoundingClientRect();
    return { top: rect.top - stage.top, bottom: stage.bottom - rect.bottom, scrolls: menu.scrollHeight > menu.clientHeight };
  });
  expect(bounds.top).toBeGreaterThanOrEqual(0);
  expect(bounds.bottom).toBeGreaterThanOrEqual(0);
  expect(bounds.scrolls).toBe(true);
  // Native pointer actionability catches clipping and header interception.
  const first = page.locator('input[name="caption-choice"][value="0"]');
  await first.check();
  await expect(first).toBeFocused();
  const lastCaption = item.captions.find((caption) => caption.language === "zzt");
  const last = page.locator(`input[name="caption-choice"][value="${lastCaption.index}"]`);
  await last.check();
  await expect(last).toBeFocused();
  await first.check();
  await expect(first).toBeFocused();
  await expect(page.locator("#player-stage")).toHaveClass(/controls-visible/);
});

test("a changed caption list preserves the focused choice or moves to the selected available choice", async ({ page }) => {
  await page.route("**/caption-controller-test", (route) => route.fulfill({
    contentType: "text/html",
    body: `<main id="player-stage"><video id="video"></video><button id="button">Captions</button>
      <div id="menu"><fieldset><legend>Captions</legend><div id="choices"></div></fieldset>
      <div id="error" hidden><p id="message"></p><button id="retry">Retry</button><button id="off">Off</button></div></div></main>`,
  }));
  await page.goto("/caption-controller-test");
  await page.evaluate(async () => {
    const { CaptionController } = await import("/web/captions.js");
    const captions = [
      { index: 0, label: "English", browser_supported: true },
      { index: 2, label: "French", browser_supported: true },
    ];
    const state = { playback: { sessionId: 1, selectedCaption: "off", item: { id: 1, kind: "video", captions } } };
    const store = { getState: () => state, dispatch(action) { Object.assign(state.playback, action.values); controller.render(); } };
    const dom = Object.fromEntries(Object.entries({
      playerStage: "player-stage", video: "video", captionsButton: "button", captionMenu: "menu", captionChoices: "choices",
      captionError: "error", captionErrorMessage: "message", captionRetry: "retry", captionOff: "off",
    }).map(([key, id]) => [key, document.getElementById(id)]));
    const controller = new CaptionController({ store, dom });
    window.__captions = { state, controller };
    controller.render();
  });
  const french = page.getByRole("radio", { name: "French", exact: true });
  await page.getByRole("radio", { name: "English", exact: true }).check();
  await french.focus();
  await page.evaluate(() => {
    window.__captions.state.playback.item.captions.reverse();
    window.__captions.controller.render();
  });
  await expect(french).toBeFocused();
  await page.evaluate(() => {
    window.__captions.state.playback.item.captions = [{ index: 0, label: "English (updated)", browser_supported: true }];
    window.__captions.controller.render();
  });
  const english = page.getByRole("radio", { name: "English (updated)", exact: true });
  await expect(english).toBeFocused();
  await expect(english).toBeChecked();
  await page.evaluate(() => {
    window.__captions.state.playback.item.captions[0].browser_supported = false;
    window.__captions.state.playback.item.captions[0].source_format = "sub";
    window.__captions.controller.render();
  });
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeFocused();
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeChecked();
  await expect(page.getByRole("radio", { name: /English \(updated\).*not supported/ })).toBeDisabled();
  await page.evaluate(() => {
    window.__captions.state.playback.item.captions[0].browser_supported = true;
    window.__captions.controller.render();
  });
  await english.check();
  await english.focus();
  await page.evaluate(() => {
    window.__captions.state.playback.item.captions = [];
    window.__captions.controller.render();
  });
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeFocused();
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeChecked();
});

test("caption failure and retry leave decoded playback healthy and expose accessible Off actions", async ({ page }) => {
  const { item, requests } = await captionFixture(page, { mode: "compat", playing: true });
  let fail = true;
  let attempts = 0;
  await page.route("**/caption-controls.vtt", (route) => {
    attempts += 1;
    return route.fulfill(fail
      ? { status: 422, contentType: "application/json", body: '{"error":"private diagnostics must not reach the menu"}' }
      : { contentType: "text/vtt", body: timelineVtt });
  });
  await page.goto(`/?view=video&item=${item.id}`);
  const video = page.locator("#video-player");
  await video.evaluate((video) => video.play());
  await expect.poll(() => video.evaluate((video) => video.getVideoPlaybackQuality?.().totalVideoFrames || 0)).toBeGreaterThan(1);
  const source = await video.evaluate((video) => video.src);
  await openCaptions(page);
  await page.getByRole("radio", { name: "English", exact: true }).check();
  await expect(page.locator("#caption-error-message")).toHaveText("Captions could not load. Try again or turn captions off.");
  await expect(page.locator("#caption-error-message")).toHaveAttribute("role", "status");
  await expect(page.locator("#player-retry")).toBeHidden();
  await expect(page.locator("#player-message")).not.toContainText("Playback could not continue");
  expect((await new AxeBuilder({ page }).include("#player-stage").analyze()).violations).toEqual([]);
  const targets = await page.locator(".caption-error-actions button").evaluateAll((buttons) => buttons.map((button) => {
    const rect = button.getBoundingClientRect();
    return { width: rect.width, height: rect.height };
  }));
  for (const target of targets) {
    expect(target.width).toBeGreaterThanOrEqual(43.99);
    expect(target.height).toBeGreaterThanOrEqual(43.99);
  }
  await page.locator('#video-player track[data-caption-index="0"]').evaluate((track) => { window.__failedCaption = track; });
  fail = false;
  await page.getByRole("button", { name: "Retry captions", exact: true }).click();
  await expect(page.getByRole("radio", { name: "English", exact: true })).toBeFocused();
  await expectCue(page, { id: "opening", start: 0, end: 0.35, text: "Opening scene" });
  await expect(page.locator("#caption-error")).toBeHidden();
  expect(attempts).toBe(2);
  await page.evaluate(() => {
    window.__failedCaption.dispatchEvent(new Event("error"));
    window.__failedCaption.dispatchEvent(new Event("load"));
  });
  await expect(page.locator("#caption-error")).toBeHidden();
  expect(await video.evaluate((video) => ({ src: video.src, paused: video.paused, error: video.error?.code ?? null })))
    .toEqual({ src: source, paused: false, error: null });
  expect(new Set(requests.media)).toEqual(new Set([source]));
  expect(requests.cancellations).toEqual([]);
});

test("caption retries preserve compatible offsets, and Off or source replacement rejects late failures", async ({ page }) => {
  const { item } = await captionFixture(page, { mode: "compat" });
  let fail = true;
  await page.route("**/caption-controls.vtt", (route) => route.fulfill(fail
    ? { status: 503, contentType: "text/plain", body: "temporarily unavailable" }
    : { contentType: "text/vtt", body: timelineVtt }));
  await page.goto(`/?view=video&item=${item.id}&t=90`);
  await openCaptions(page);
  await page.getByRole("radio", { name: "English", exact: true }).check();
  await expect(page.locator("#caption-error")).toBeVisible();
  const source = await page.locator("#video-player").evaluate((video) => video.src);
  fail = false;
  await page.getByRole("button", { name: "Retry captions", exact: true }).click();
  await expectCue(page, { id: "crossing", start: 0, end: 0.15, text: "Crossing the source start" });
  expect(await page.locator("#video-player").evaluate((video) => video.src)).toBe(source);
  await page.locator('#video-player track[data-caption-index="0"]').evaluate((track) => { window.__priorSourceCaption = track; });
  fail = true;
  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "120";
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect.poll(() => page.locator("#video-player").evaluate((video) => new URL(video.src || document.baseURI).searchParams.get("start"))).toBe("120");
  await expect(page.getByRole("radio", { name: "English", exact: true })).toBeChecked();
  await expect(page.locator("#caption-error")).toBeVisible();
  await page.locator('#video-player track[data-caption-index="0"]').evaluate((track) => { window.__offCaption = track; });
  await page.getByRole("button", { name: "Turn captions off", exact: true }).click();
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeFocused();
  await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeChecked();
  await expect(page.locator("#captions-button")).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator("#caption-error")).toBeHidden();
  await page.evaluate(() => {
    window.__priorSourceCaption.dispatchEvent(new Event("load"));
    window.__priorSourceCaption.dispatchEvent(new Event("error"));
    window.__offCaption.dispatchEvent(new Event("error"));
  });
  await expect(page.locator("#caption-error")).toBeHidden();
  expect(await page.evaluate(() => window.__priorSourceCaption.isConnected)).toBe(false);
  fail = false;
  await page.getByRole("radio", { name: "French", exact: true }).check();
  await page.evaluate(() => window.__offCaption.dispatchEvent(new Event("error")));
  await expect(page.getByRole("radio", { name: "French", exact: true })).toBeChecked();
  await expect(page.locator("#caption-error")).toBeHidden();
});

test("turning captions off detaches an unfinished track before its late load or error", async ({ page }) => {
  const { item } = await captionFixture(page);
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  let requested = false;
  await page.route("**/caption-controls.vtt", async (route) => {
    requested = true;
    await pending;
    await route.fulfill({ contentType: "text/vtt", body: timelineVtt }).catch(() => {});
  });
  try {
    await page.goto(`/?view=video&item=${item.id}`);
    await openCaptions(page);
    await page.getByRole("radio", { name: "English", exact: true }).check();
    await expect.poll(() => requested).toBe(true);
    await page.locator('#video-player track[data-caption-index="0"]').evaluate((track) => { window.__pendingCaption = track; });
    await page.getByRole("radio", { name: "Off", exact: true }).check();
    expect(await page.evaluate(() => ({ connected: window.__pendingCaption.isConnected, src: window.__pendingCaption.getAttribute("src") })))
      .toEqual({ connected: false, src: null });
    release();
    await page.evaluate(() => {
      window.__pendingCaption.dispatchEvent(new Event("load"));
      window.__pendingCaption.dispatchEvent(new Event("error"));
    });
    await expect(page.locator("#caption-error")).toBeHidden();
    await expect(page.getByRole("radio", { name: "Off", exact: true })).toBeChecked();
    await page.getByRole("radio", { name: "English", exact: true }).check();
    await expectCue(page, { id: "opening", start: 0, end: 0.35, text: "Opening scene" });
  } finally {
    release();
  }
});
