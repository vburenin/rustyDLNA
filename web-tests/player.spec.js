import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

const compatibleFixture = fileURLToPath(new URL("../testdata/library/video/tagged.mp4", import.meta.url));

async function openLibrary(page) {
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto("/");
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
  await expect(page.locator("#loading")).toBeHidden();
  return errors;
}

async function openVideoView(page) {
  await page.getByRole("tab", { name: "Videos" }).click();
  await expect(page.getByRole("tabpanel")).toHaveAttribute("aria-labelledby", "tab-video");
  await expect(page.locator(".media-card.video").first()).toBeVisible();
}

async function selectTaggedVideo(page) {
  await openVideoView(page);
  const poster = page.locator(".media-card.video .art", { has: page.locator("img") }).first();
  await expect(poster).toBeVisible();
  const posterLayout = await poster.evaluate((art) => {
    const bounds = art.getBoundingClientRect();
    return {
      objectFit: getComputedStyle(art.querySelector("img")).objectFit,
      width: bounds.width,
      ratio: bounds.height / bounds.width,
    };
  });
  expect(posterLayout.objectFit).toBe("contain");
  expect(posterLayout.width).toBeGreaterThan(100);
  expect(posterLayout.ratio).toBeGreaterThan(1.45);
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect(page.locator("#now-playing-title")).toHaveText("tagged");
}

async function usePreference(page, name, value) {
  await page.addInitScript(({ name, value }) => {
    localStorage.setItem(`rustydlna.${name}`, String(value));
  }, { name, value });
}

async function serveFixtureMedia(page, onRequest = () => {}) {
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", async (route) => {
    onRequest(new URL(route.request().url()));
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });
}

async function showPlayerControls(page) {
  const stage = page.locator("#player-stage");
  await stage.scrollIntoViewIfNeeded();
  await stage.hover();
  await expect(page.locator("#playback-controls")).toBeVisible();
}

async function openAdvancedPlayback(page) {
  await showPlayerControls(page);
  await page.locator("#advanced-playback-button").click();
  await expect(page.locator("#advanced-playback-dialog")).toBeVisible();
}

test("library tabs, player scoping, and overlay controls work", async ({ page }) => {
  const errors = await openLibrary(page);
  const folders = page.getByRole("tab", { name: "Folders" });
  await folders.focus();
  await folders.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "All media" })).toHaveAttribute("aria-selected", "true");
  await openVideoView(page);
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();

  const stage = page.locator("#player-stage");
  for (const selector of ["#timeline", "#volume-control", "#stream-info-button", "#captions-button", "#audio-track-controls", "#fullscreen-button"]) {
    await expect(stage.locator(selector)).toHaveCount(1);
  }
  await expect(stage.locator("[data-seek]")).toHaveCount(0);
  const iconOffsets = await stage.locator("button:has(.button-icon)").evaluateAll((buttons) => buttons.map((button) => {
    const control = button.getBoundingClientRect();
    const icon = button.querySelector(".button-icon").getBoundingClientRect();
    return {
      x: Math.abs(icon.left + icon.width / 2 - (control.left + control.width / 2)),
      y: Math.abs(icon.top + icon.height / 2 - (control.top + control.height / 2)),
    };
  }));
  expect(iconOffsets.length).toBeGreaterThan(0);
  expect(iconOffsets.every(({ x, y }) => x <= 0.5 && y <= 0.5)).toBe(true);
  const volumeControl = page.locator(".volume-control");
  if (await volumeControl.isVisible()) {
    const volumeWidth = await volumeControl.evaluate((control) => control.getBoundingClientRect().width);
    const speakerGap = await page.evaluate(() => {
      const speaker = document.querySelector("#mute-button").getBoundingClientRect();
      const volume = document.querySelector(".volume-control").getBoundingClientRect();
      return volume.left - speaker.right;
    });
    expect(speakerGap).toBeGreaterThan(0);
    await showPlayerControls(page);
    await volumeControl.hover({ force: true });
    expect(await volumeControl.evaluate((control) => control.getBoundingClientRect().width)).toBe(volumeWidth);
  }
  await showPlayerControls(page);
  await page.locator("#play-button").hover({ force: true });
  const playHoverStyle = await page.locator("#play-button").evaluate((button) => {
    const style = getComputedStyle(button);
    return { backgroundColor: style.backgroundColor, color: style.color };
  });
  expect(playHoverStyle.color).toBe("rgb(255, 255, 255)");
  expect(playHoverStyle.backgroundColor).not.toBe("rgba(0, 0, 0, 0)");
  await expect(page.locator(".topbar #now-playing-title")).toHaveText("tagged");
  const layout = await page.evaluate(() => {
    const media = document.querySelector(".media-viewport").getBoundingClientRect();
    const controlSurface = document.querySelector("#playback-controls");
    const controls = controlSurface.getBoundingClientRect();
    const separated = media.right <= controls.left
      || controls.right <= media.left
      || media.bottom <= controls.top
      || controls.bottom <= media.top;
    const filters = document.querySelector(".filters");
    return {
      separated,
      controlsOverflow: controlSurface.scrollHeight > controlSurface.clientHeight,
      filtersOverflow: filters.scrollWidth > filters.clientWidth,
      pageOverflow: document.documentElement.scrollWidth > document.documentElement.clientWidth,
    };
  });
  expect(layout).toEqual({ separated: false, controlsOverflow: false, filtersOverflow: false, pageOverflow: false });
  expect(await page.locator("#speed-control").evaluate((select) => select.getBoundingClientRect().width)).toBeLessThanOrEqual(90);
  await openAdvancedPlayback(page);
  const advancedLayout = await page.locator("#advanced-playback-dialog").evaluate((dialog) => ({
    overflow: dialog.scrollHeight > dialog.clientHeight,
    removedCopy: !dialog.textContent.includes("Compatible playback uses server CPU/GPU"),
  }));
  expect(advancedLayout).toEqual({ overflow: false, removedCopy: true });
  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  expect(await page.locator("#video-player").evaluate((video) => video.controls)).toBe(false);
  expect(await page.evaluate(() => document.body.dispatchEvent(new KeyboardEvent("keydown", {
    key: "f", ctrlKey: true, bubbles: true, cancelable: true,
  })))).toBe(true);
  expect(await page.evaluate(() => document.body.dispatchEvent(new KeyboardEvent("keydown", {
    key: "ArrowDown", bubbles: true, cancelable: true,
  })))).toBe(true);
  await stage.focus();
  expect(await page.evaluate(() => document.querySelector("#player-stage").dispatchEvent(new KeyboardEvent("keydown", {
    key: " ", bubbles: true, cancelable: true,
  })))).toBe(false);
  expect(errors).toEqual([]);
});

test("folder history and a pending search cannot leak into navigation", async ({ page }) => {
  await openLibrary(page);
  await page.getByRole("tab", { name: "All media" }).click();
  await page.locator("#search-input").fill("tagged");
  await page.getByRole("tab", { name: "Folders" }).click();
  await page.waitForTimeout(350);
  expect(new URL(page.url()).searchParams.has("q")).toBe(false);
  await expect(page.locator("#search-input")).toHaveValue("");

  await page.getByRole("button", { name: /^Open library,/ }).click();
  await page.getByRole("button", { name: /^Open video,/ }).click();
  await expect(page).toHaveURL(/folder=/);
  await expect(page.locator(".media-card.video").first()).toBeVisible();
  await page.goBack();
  await expect(page.getByRole("button", { name: /^Open video,/ })).toBeVisible();
  await expect(page.getByRole("tab", { name: "Folders" })).toHaveAttribute("aria-selected", "true");
});

test("Continue watching survives reload and supports clearing progress", async ({ page }) => {
  await page.route("**/api/web/library?*", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media") {
        entry.duration_seconds = 600;
        entry.duration = "0:10:00.000";
      }
    }
    await route.fulfill({ response, json: payload });
  });
  await page.goto("/?view=video");
  const first = page.locator("[data-media-id]").first();
  const itemId = await first.getAttribute("data-media-id");
  const title = await first.locator(".card-title").textContent();
  await page.evaluate(({ itemId }) => {
    localStorage.setItem("rustydlna.webProgress.v1", JSON.stringify({
      [itemId]: { position: 120, duration: 600, updated: Date.now() },
    }));
  }, { itemId });
  await page.reload();
  await page.getByRole("tab", { name: "Continue watching" }).click();
  await expect(page.locator(`[data-media-id="${itemId}"] .card-title`)).toHaveText(title);
  await page.getByRole("button", { name: `Clear progress for ${title}` }).click();
  await expect(page.locator(`[data-media-id="${itemId}"]`)).toHaveCount(0);
  await expect(page.locator("#library-empty-title")).toHaveText("No media found");
});

test("item details and validated deep links remain usable", async ({ page }) => {
  await page.goto("/?view=video");
  const first = page.locator("[data-media-id]").first();
  const itemId = await first.getAttribute("data-media-id");
  const title = (await first.locator(".card-title").textContent()).trim();
  await page.getByRole("button", { name: `Details for ${title}` }).click();
  await expect(page.locator("#item-details-dialog")).toBeVisible();
  await expect(page.locator("#item-details-title")).toHaveText(title);
  await page.getByRole("button", { name: "Close item details" }).click();

  await page.goto(`/?view=video&item=${itemId}&t=5`);
  await expect(page.locator("#now-playing-title")).toHaveText(title);
  await expect(page).toHaveURL(new RegExp(`item=${itemId}`));

  await page.goto("/?view=video&item=999999999");
  await expect(page.locator("#player-empty-text")).toContainText("linked title is not available");
});

test("missing catalog duration, artwork, and audio degrade deliberately", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media" && entry.file_name === "tagged.mp4") {
        entry.duration = null;
        entry.duration_seconds = null;
        entry.art_url = null;
        entry.audio_tracks = [];
        entry.stream_metadata_complete = true;
      }
    }
    await route.fulfill({ response, json: payload });
  });
  await serveFixtureMedia(page);
  await page.goto("/?view=video");
  const card = page.locator(".media-card.video", { has: page.getByRole("button", { name: /^Play tagged\b/ }) });
  await expect(card.locator(".art img")).toHaveCount(0);
  await expect(card.locator(".art-fallback")).toBeVisible();
  await card.locator(".card-button").click();
  await expect(page.locator("#audio-track-control")).toBeHidden();
  await expect.poll(async () => Number(await page.locator("#timeline").getAttribute("max"))).toBeGreaterThan(0);
});

test("library failure is plain-language and recoverable", async ({ page }) => {
  let requests = 0;
  await page.route("**/api/web/library?**", async (route) => {
    requests += 1;
    if (requests === 1) {
      await route.fulfill({
        status: 503,
        contentType: "application/json",
        body: JSON.stringify({ schema_version: 1, error: { code: "library_unavailable", message: "raw helper output", recoverable: true, action: "retry_library" } }),
      });
    } else {
      await route.fallback();
    }
  });
  await page.goto("/");
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", "error");
  await expect(page.locator("#library-empty-detail")).toHaveText(/(?:Check the server connection|You appear to be offline)/);
  await expect(page.locator("#library-empty-detail")).not.toContainText("raw helper output");
  await page.locator("#library-retry").click();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
});

test("autoplay rejection leaves a clear Play affordance", async ({ page }) => {
  await page.addInitScript(() => {
    const original = HTMLMediaElement.prototype.play;
    let rejected = false;
    HTMLMediaElement.prototype.play = function patchedPlay() {
      if (!rejected) {
        rejected = true;
        return Promise.reject(new DOMException("User activation required", "NotAllowedError"));
      }
      return original.call(this);
    };
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await expect(page.locator("#player-message-text")).toContainText("Press Play");
  await showPlayerControls(page);
  await page.locator("#play-button").click();
});

test("forced Original and Compatible modes select the requested typed source", async ({ page }) => {
  const requests = [];
  await usePreference(page, "stream", "direct");
  await serveFixtureMedia(page, (url) => requests.push(url));
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#mode-label")).toHaveText("Original file");
  expect(requests.some((url) => url.searchParams.get("mode") === "direct"
    && url.searchParams.get("reason") === "forced_original")).toBe(true);

  await openAdvancedPlayback(page);
  expect(await page.locator("#quality-control option").evaluateAll((options) => options.map((option) => ({
    value: option.value,
    label: option.textContent,
  })))).toEqual([
    { value: "auto", label: "Auto · up to 4K" },
    { value: "full_hd", label: "1080p" },
    { value: "data_saver", label: "720p" },
  ]);
  await page.locator('input[name="stream-mode"][value="compat"]').check();
  await expect(page.locator("#mode-label")).toHaveText("Compatible playback");
  await expect.poll(() => requests.some((url) => url.searchParams.get("mode") === "compatible")).toBe(true);
  const compatible = requests.findLast((url) => url.searchParams.get("mode") === "compatible");
  expect(compatible?.searchParams.get("reason")).toBe("forced_compatible");
  expect(compatible?.searchParams.get("quality")).toBe("auto");
  expect(compatible?.searchParams.get("request")).toMatch(/^\d+$/);
  await page.locator("#quality-control").selectOption("full_hd");
  await expect.poll(() => requests.some((url) => url.searchParams.get("quality") === "full_hd")).toBe(true);
});

test("compatible startup status is not duplicated and stream info explains an audio-only transcode", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    const original = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function negotiatedCanPlayType(contentType) {
      if (String(contentType).includes("ac-3")) return "";
      if (String(contentType).includes("hvc1.")) return "probably";
      return original.call(this, contentType);
    };
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: {
        decodingInfo: async (configuration) => ({
          supported: Boolean(configuration.video),
          smooth: true,
          powerEfficient: true,
        }),
      },
    });
  });
  const fixture = await readFile(compatibleFixture);
  let compatibleRequest = null;
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "hevc";
      entry.codec_string = "hvc1.1.6.L120.B0,ac-3";
      entry.video_content_type = 'video/mp4; codecs="hvc1.1.6.L120.B0"';
      entry.video_profile = "Main 10";
      entry.video_level = 120;
      entry.pixel_format = "yuv420p10le";
      entry.bit_depth = 10;
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, language: "eng", title: "Main", default: true }];
      entry.stream_metadata_complete = true;
      entry.compatible_video_encoder = "libx264";
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 1, item_id: 9, request_id: 1, state: "starting", retry_after_seconds: 1 }),
  }));
  await page.route("**/web/media/*.mp4?**", async (route) => {
    compatibleRequest = new URL(route.request().url());
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#stage-progress-label")).toHaveText("Starting compatible playback…");
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.getByText("Starting compatible playback…", { exact: true })).toHaveCount(1);
  expect(compatibleRequest?.searchParams.get("video_mode")).toBe("copy");
  expect(compatibleRequest?.searchParams.get("audio_mode")).toBe("transcode");

  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#stream-info-dialog")).toBeVisible();
  await expect(page.locator("#source-stream-facts")).toContainText("HEVC");
  await expect(page.locator("#source-stream-facts")).toContainText("AC-3");
  await expect(page.locator("#stream-info-summary")).toContainText("video bitstream is copied unchanged");
  await expect(page.locator("#output-stream-facts")).toContainText("no video re-encode");
  await expect(page.locator("#output-stream-facts")).toContainText("AAC");
  await expect(page.locator("#output-stream-facts")).toContainText("canPlayType: probably");
  await expect(page.locator("#output-stream-facts")).toContainText("MediaCapabilities: supported");
});

test("malformed HEVC timing selects HDR-preserving frame-order repair", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    const original = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function repairCanPlayType(contentType) {
      if (String(contentType).includes("hvc1.")) return "probably";
      return original.call(this, contentType);
    };
  });
  let compatibleRequest = null;
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "hevc";
      entry.codec_string = "hvc1.2.4.H153.90,mp4a.40.2";
      entry.video_content_type = 'video/mp4; codecs="hvc1.2.4.H153.90"';
      entry.video_profile = "Main 10";
      entry.video_level = 153;
      entry.pixel_format = "yuv420p10le";
      entry.bit_depth = 10;
      entry.hdr = "hdr10";
      entry.video_timestamp_mode = "broken-reordered";
      entry.video_repair_required = true;
      entry.repair_video_encoder = "hevc_nvenc";
      entry.audio_codec = "aac";
      entry.audio_tracks = [{ index: 0, codec: "aac", content_type: 'audio/mp4; codecs="mp4a.40.2"', channels: 2, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 1, item_id: 9, request_id: 1, state: "starting", retry_after_seconds: 1 }),
  }));
  await serveFixtureMedia(page, (url) => { compatibleRequest = url; });

  await openLibrary(page);
  await selectTaggedVideo(page);
  expect(compatibleRequest?.searchParams.get("video_mode")).toBe("repair");
  expect(compatibleRequest?.searchParams.get("audio_mode")).toBe("copy");

  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#source-stream-facts")).toContainText("Malformed display-order timestamps detected");
  await expect(page.locator("#stream-info-summary")).toContainText("restore stable frame order while preserving HEVC and HDR10");
  await expect(page.locator("#output-stream-facts")).toContainText("HEVC (hevc_nvenc)");
  await expect(page.locator("#output-stream-facts")).toContainText("frame order repaired");
});

test("a copied codec decode error retries once with portable H.264 and AAC", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    const original = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function negotiatedCanPlayType(contentType) {
      if (String(contentType).includes("hvc1.")) return "probably";
      if (String(contentType).includes("ac-3")) return "probably";
      return original.call(this, contentType);
    };
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "hevc";
      entry.codec_string = "hvc1.2.4.L150.90,ac-3";
      entry.video_content_type = 'video/mp4; codecs="hvc1.2.4.L150.90"';
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 1, item_id: 9, request_id: 1, state: "producing", retry_after_seconds: 1 }),
  }));
  const fixture = await readFile(compatibleFixture);
  const requests = [];
  await page.route("**/web/media/*.mp4?**", async (route) => {
    requests.push(new URL(route.request().url()));
    if (requests.length > 1) await new Promise((resolve) => setTimeout(resolve, 750));
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.length).toBe(1);
  expect(requests[0].searchParams.get("video_mode")).toBe("copy");
  expect(requests[0].searchParams.get("audio_mode")).toBe("copy");
  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 3 } });
    video.dispatchEvent(new Event("error"));
  });

  await expect.poll(() => requests.length).toBe(2);
  expect(requests[1].searchParams.get("video_mode")).toBe("transcode");
  expect(requests[1].searchParams.get("audio_mode")).toBe("transcode");
  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#stream-info-summary")).toContainText("H.264 video and AAC audio");
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("disabled, busy, and failed compatible playback recover appropriately", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  let transcoding = false;
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.transcoding = transcoding;
    await route.fulfill({ response, json: payload });
  });
  await page.goto("/?view=video");
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect(page.locator("#player-message-text")).toContainText("disabled on this server");
  await expect(page.locator("#play-original")).toBeVisible();

  transcoding = true;
  let status = "queued";
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 1, item_id: 9, request_id: 1, state: status, retry_after_seconds: status === "queued" ? 1 : null }),
  }));
  const fixture = await readFile(compatibleFixture);
  let mediaRequests = 0;
  await page.route("**/web/media/*.mp4?**", (route) => {
    mediaRequests += 1;
    return status === "queued" && mediaRequests > 1
      ? route.fulfill({ status: 200, contentType: "video/mp4", body: fixture })
      : route.abort("failed");
  });
  await page.reload();
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect.poll(() => mediaRequests).toBeGreaterThan(1);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();

  status = "failed";
  mediaRequests = 0;
  await page.reload();
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect(page.locator("#player-message-text")).toContainText("could not prepare this title");
  await expect(page.locator("#play-original")).toBeVisible();
});

test("a missing direct file is not mislabeled as unsupported", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await openLibrary(page);
  await openVideoView(page);
  const card = page.locator(".media-card.video").first();
  const itemId = await card.getAttribute("data-media-id");
  await page.route(`**/api/web/item/${itemId}`, (route) => route.fulfill({
    status: 404,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 1, error: { code: "media_missing", message: "raw path", recoverable: true, action: "return_to_library" } }),
  }));
  await page.route("**/web/media/*.mp4?**", (route) => route.abort("failed"));
  await card.locator(".card-button").click();
  await expect(page.locator("#player-message-text")).toHaveText("This media file is no longer available.");
  await expect(page.locator("#technical-message")).not.toContainText("raw path");
});

test("a dropped compatible-media connection retries a healthy producer", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  const fixture = await readFile(compatibleFixture);
  const requests = [];
  const cancellations = [];
  page.on("request", (request) => {
    if (request.method() === "DELETE" && request.url().includes("/api/web/transcode/")) {
      cancellations.push(new URL(request.url()));
    }
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 1, item_id: 9, request_id: 1, state: "producing", retry_after_seconds: null }),
  }));
  await page.route("**/web/media/*.mp4?**", async (route) => {
    requests.push(new URL(route.request().url()));
    if (requests.length === 1) return route.abort("failed");
    return route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);

  await expect.poll(() => requests.length).toBeGreaterThan(1);
  await expect.poll(() => cancellations.length).toBeGreaterThan(0);
  expect(requests[1].searchParams.get("session")).toBe(requests[0].searchParams.get("session"));
  expect(Number(requests[1].searchParams.get("request"))).toBeGreaterThan(Number(requests[0].searchParams.get("request")));
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("rapid item switching suppresses an older media failure", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  const fixture = await readFile(compatibleFixture);
  let mediaRequest = 0;
  await page.route("**/web/media/*.mp4?**", async (route) => {
    mediaRequest += 1;
    if (mediaRequest === 1) {
      await new Promise((resolve) => setTimeout(resolve, 350));
      await route.abort("failed");
    } else {
      await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
    }
  });
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video .card-button");
  await cards.nth(0).click();
  await cards.nth(1).click();
  const current = await page.locator("#now-playing-title").textContent();
  await page.waitForTimeout(500);
  await expect(page.locator("#now-playing-title")).toHaveText(current);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("repeated compatible seeks coalesce and audio switching preserves global time", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  const requests = [];
  const cancellations = [];
  page.on("request", (request) => {
    if (request.method() === "DELETE" && request.url().includes("/api/web/transcode/")) {
      cancellations.push(new URL(request.url()));
    }
  });
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media") continue;
      entry.duration_seconds = 600;
      entry.duration = "0:10:00.000";
      entry.audio_tracks = [
        { index: 0, codec: "aac", channels: 2, language: "eng", title: "Main", default: true },
        { index: 1, codec: "ac3", channels: 6, language: "fra", title: "Dub", default: false },
      ];
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/item/*", async (route) => {
    const response = await route.fetch();
    if (!response.ok()) return route.fulfill({ response });
    const payload = await response.json();
    payload.audio_tracks = [
      { index: 0, codec: "aac", channels: 2, language: "eng", title: "Main", default: true },
      { index: 1, codec: "ac3", channels: 6, language: "fra", title: "Dub", default: false },
    ];
    await route.fulfill({ response, json: payload });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#timeline")).toHaveAttribute("max", "600");
  for (const value of [12, 18, 27]) {
    await page.locator("#timeline").evaluate((timeline, next) => {
      timeline.value = String(next);
      timeline.dispatchEvent(new Event("input", { bubbles: true }));
      timeline.dispatchEvent(new Event("change", { bubbles: true }));
    }, value);
  }
  await expect.poll(() => requests.filter((url) => url.searchParams.get("start") !== "0").length).toBe(1);
  await expect.poll(() => cancellations.length).toBeGreaterThan(0);
  expect(cancellations[0].searchParams.get("request")).toMatch(/^\d+$/);
  expect(cancellations[0].searchParams.get("session")).toMatch(/^\d+$/);
  expect(requests.findLast((url) => url.searchParams.get("start") !== "0")?.searchParams.get("start")).toBe("20");
  await openAdvancedPlayback(page);
  await page.locator("#audio-track-controls").selectOption("1");
  await expect.poll(() => requests.findLast((url) => url.searchParams.get("audio") === "1")?.searchParams.get("start")).toBe("20");
  const playbackSessions = new Set(requests
    .filter((url) => url.searchParams.get("mode") === "compatible")
    .map((url) => url.searchParams.get("session")));
  expect(playbackSessions.size).toBe(1);
  expect([...playbackSessions][0]).toMatch(/^\d+$/);
  expect(await page.locator("#audio-track-controls").evaluate((select) => select.getBoundingClientRect().width)).toBeLessThanOrEqual(145);
  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  await expect(page.locator("#mode-label")).toHaveText("Compatible playback");
});

test("repeated compatible keyboard seeks preserve playing intent", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    window.__playCalls = 0;
    HTMLMediaElement.prototype.play = function play() {
      window.__playCalls += 1;
      this.dispatchEvent(new Event("playing"));
      return Promise.resolve();
    };
  });
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media") continue;
      entry.duration_seconds = 600;
      entry.duration = "0:10:00.000";
    }
    await route.fulfill({ response, json: payload });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__playCalls)).toBeGreaterThan(0);
  const initialPlayCalls = await page.evaluate(() => window.__playCalls);
  await page.locator("#player-stage").focus();
  await page.evaluate(async () => {
    for (let press = 0; press < 6; press += 1) {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  });
  await expect.poll(() => requests.findLast((url) => url.searchParams.get("start") !== "0")?.searchParams.get("start")).toBe("60");
  expect(requests.filter((url) => url.searchParams.get("start") !== "0")).toHaveLength(1);
  await expect.poll(() => page.evaluate(() => window.__playCalls)).toBeGreaterThan(initialPlayCalls);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
});

test("captions convert to WebVTT and survive a compatible source restart", async ({ page }) => {
  await serveFixtureMedia(page);
  await page.goto("/?view=video");
  const captioned = await page.evaluate(async () => {
    const response = await fetch("/api/web/library?view=library&kind=video&q=&sort=title&offset=0&limit=60");
    const payload = await response.json();
    return payload.entries.find((entry) => entry.entry_type === "media" && entry.captions?.some((caption) => caption.browser_supported));
  });
  expect(captioned).toBeTruthy();
  await page.locator(`[data-media-id="${captioned.id}"] .card-button`).click();
  await showPlayerControls(page);
  await page.locator("#captions-button").click();
  const caption = captioned.captions.find((entry) => entry.browser_supported);
  await page.locator(`input[name="caption-choice"][value="${caption.index}"]`).check();
  await expect(page.locator("#captions-button")).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator(`#video-player track[data-caption-index="${caption.index}"]`)).toHaveCount(1);
  const captionResponse = await page.request.get(caption.url);
  expect(captionResponse.ok()).toBe(true);
  expect(await captionResponse.text()).toMatch(/^WEBVTT\n\n/);

  await openAdvancedPlayback(page);
  await page.locator('input[name="stream-mode"][value="compat"]').check();
  await expect(page.locator(`input[name="caption-choice"][value="${caption.index}"]`)).toBeChecked();
  await expect(page.locator(`#video-player track[data-caption-index="${caption.index}"]`)).toHaveCount(1);
});

test("resume offers Start over and blocked browser storage remains nonfatal", async ({ page }) => {
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media") {
        entry.duration_seconds = 600;
        entry.duration = "0:10:00.000";
      }
    }
    await route.fulfill({ response, json: payload });
  });
  await serveFixtureMedia(page);
  await page.goto("/?view=video");
  const card = page.locator(".media-card.video").first();
  const itemId = await card.getAttribute("data-media-id");
  await page.evaluate(({ itemId }) => localStorage.setItem("rustydlna.webProgress.v1", JSON.stringify({
    [itemId]: { position: 125, duration: 600, updated: Date.now() },
  })), { itemId });
  await card.locator(".card-button").click();
  await expect(page.locator("#resume-prompt")).toBeVisible();
  await expect(page.locator("#resume-time")).toHaveText("Resume at 2:05");
  await page.locator("#start-over-button").click();
  await expect(page.locator("#resume-prompt")).toBeHidden();
  expect(await page.evaluate(({ itemId }) => JSON.parse(localStorage.getItem("rustydlna.webProgress.v1") || "{}")[itemId], { itemId })).toBeUndefined();

  const blocked = await page.context().newPage();
  const errors = [];
  blocked.on("pageerror", (error) => errors.push(error.message));
  await blocked.addInitScript(() => {
    Storage.prototype.getItem = () => { throw new DOMException("blocked", "SecurityError"); };
    Storage.prototype.setItem = () => { throw new DOMException("blocked", "SecurityError"); };
    Storage.prototype.removeItem = () => { throw new DOMException("blocked", "SecurityError"); };
  });
  await blocked.goto("/?view=video");
  await expect(blocked.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
  await blocked.getByRole("button", { name: /^Play tagged\b/ }).click();
  expect(errors).toEqual([]);
  await blocked.close();
});

test("switching titles flushes progress to the title that was playing", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await page.addInitScript(() => {
    const sources = new WeakMap();
    HTMLMediaElement.prototype.play = () => Promise.resolve();
    HTMLMediaElement.prototype.pause = () => {};
    // This test exercises controller persistence, not native decoding. The
    // checked-in MP4 is sub-second while the mocked catalog duration is 10m,
    // so loading it would legitimately clamp the synthetic 2:05 seek.
    HTMLMediaElement.prototype.load = () => {};
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) { sources.set(this, new URL(value, document.baseURI).href); },
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media") {
        entry.duration_seconds = 600;
        entry.duration = "0:10:00.000";
      }
    }
    await route.fulfill({ response, json: payload });
  });
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video .card-button");
  const firstId = await cards.nth(0).locator("xpath=..").getAttribute("data-media-id");
  const secondId = await cards.nth(1).locator("xpath=..").getAttribute("data-media-id");
  await cards.nth(0).click();
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.src)).toMatch(/web\/media\//);
  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "125";
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await cards.nth(1).click();
  const progress = await page.evaluate(() => JSON.parse(localStorage.getItem("rustydlna.webProgress.v1") || "{}"));
  expect(progress[firstId].position).toBe(125);
  expect(progress[secondId]).toBeUndefined();
});

test("legacy stream metadata exposes loading and retryable enrichment", async ({ page }) => {
  let selectedId = null;
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    const media = (payload.entries || []).find((entry) => entry.entry_type === "media" && entry.kind === "video");
    if (media) {
      selectedId = String(media.id);
      media.audio_tracks = [];
      media.stream_metadata_complete = false;
    }
    await route.fulfill({ response, json: payload });
  });
  let enrichmentAttempts = 0;
  await page.route("**/api/web/item/*?enrich=1", async (route) => {
    enrichmentAttempts += 1;
    if (enrichmentAttempts === 1) {
      await route.fulfill({
        status: 503,
        contentType: "application/json",
        body: JSON.stringify({ schema_version: 1, error: { code: "transcode_busy", message: "busy", recoverable: true, action: "retry_item" } }),
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        schema_version: 1,
        id: Number(selectedId),
        item: {},
        audio_tracks: [{ index: 0, codec: "aac", channels: 2, language: "eng", title: "Main", default: true }],
        chapters: [],
      }),
    });
  });
  await serveFixtureMedia(page);
  await openLibrary(page);
  await openVideoView(page);
  await page.locator(`[data-media-id="${selectedId}"] .card-button`).click();
  await expect(page.locator("#audio-track-status")).toContainText("unavailable");
  await openAdvancedPlayback(page);
  await expect(page.locator("#audio-track-retry")).toBeVisible();
  await page.locator("#audio-track-retry").click();
  await expect(page.locator("#audio-track-retry")).toBeHidden();
  await expect(page.locator("#audio-track-status")).toBeHidden();
  expect(enrichmentAttempts).toBe(2);
});

test("queue snapshot crosses pagination, auto-advances, and survives navigation", async ({ page }) => {
  await usePreference(page, "autoplay", "true");
  await serveFixtureMedia(page);
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 1, item_id: 7, request_id: 1, state: "complete", retry_after_seconds: null }),
  }));
  let firstPayload = null;
  await page.route("**/api/web/library?**", async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("view") !== "library" || url.searchParams.get("kind") !== "video") {
      return route.fallback();
    }
    const offset = Number(url.searchParams.get("offset") || 0);
    if (!firstPayload) {
      const response = await route.fetch();
      firstPayload = await response.json();
    }
    const base = firstPayload.entries.find((entry) => entry.entry_type === "media");
    const make = (index) => ({
      ...base,
      id: Number(base.id) + 10_000 + index,
      title: `Queue ${index}`,
      file_name: `queue-${index}.mp4`,
      duration_seconds: 600,
      duration: "0:10:00.000",
    });
    const entries = offset === 0
      ? Array.from({ length: 60 }, (_, index) => make(index + 1))
      : offset === 60 ? [make(61)] : [];
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ ...firstPayload, offset, limit: 60, total: 61, has_more: offset + entries.length < 61, entries }),
    });
  });
  await page.goto("/?view=video");
  await page.getByRole("button", { name: /^Play Queue 60\b/ }).click();
  await expect(page.locator("#next-button")).toHaveAttribute("title", "Next: Queue 61");
  await page.locator("#video-player").dispatchEvent("ended");
  await expect(page.locator("#now-playing-title")).toHaveText("Queue 61");
  await page.getByRole("tab", { name: "Folders" }).click();
  await showPlayerControls(page);
  await page.locator("#previous-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText("Queue 60");
});

test("a generation change during Load more is recoverable without duplicate cards", async ({ page }) => {
  let generationChanged = false;
  await page.route("**/api/web/library?**", async (route) => {
    const url = new URL(route.request().url());
    const offset = Number(url.searchParams.get("offset") || 0);
    if (offset > 0 && !generationChanged) {
      generationChanged = true;
      return route.fulfill({
        status: 409,
        contentType: "application/json",
        body: JSON.stringify({ schema_version: 1, error: { code: "catalog_changed", message: "changed", recoverable: true, action: "retry_library" } }),
      });
    }
    const response = await route.fetch();
    const payload = await response.json();
    if (!generationChanged && offset === 0) {
      payload.total += 1;
      payload.has_more = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.goto("/?view=video");
  const before = await page.locator("[data-media-id]").count();
  await page.locator("#load-more").click();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", "error");
  expect(await page.locator("[data-media-id]").count()).toBe(before);
  await page.locator("#library-retry").click();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
  const ids = await page.locator("[data-media-id]").evaluateAll((cards) => cards.map((card) => card.dataset.mediaId));
  expect(new Set(ids).size).toBe(ids.length);
});

test("end state keeps the real duration and Replay starts from zero", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await page.addInitScript(() => {
    const sources = new WeakMap();
    HTMLMediaElement.prototype.play = () => Promise.resolve();
    HTMLMediaElement.prototype.pause = () => {};
    HTMLMediaElement.prototype.load = () => {};
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) { sources.set(this, new URL(value, document.baseURI).href); },
    });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  const duration = await page.locator("#timeline").getAttribute("max");
  await page.locator("#video-player").dispatchEvent("ended");
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Replay");
  await expect(page.locator("#timeline")).toHaveValue(duration);
  await showPlayerControls(page);
  await page.locator("#play-button").click();
  await expect(page.locator("#timeline")).toHaveValue("0");
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
});

test("reduced motion, 200% zoom, and focus restoration remain usable", async ({ page }) => {
  await page.addInitScript(() => {
    const original = window.matchMedia.bind(window);
    window.matchMedia = (query) => query.includes("prefers-reduced-motion")
      ? { matches: true, media: query, onchange: null, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return true; } }
      : original(query);
    window.__scrollOptions = [];
    Element.prototype.scrollIntoView = function scrollIntoView(options) { window.__scrollOptions.push(options); };
  });
  await page.setViewportSize({ width: 600, height: 700 });
  await openLibrary(page);
  await openVideoView(page);
  await page.locator(".media-card.video").last().scrollIntoViewIfNeeded();
  await page.locator(".media-card.video .card-button").last().click();
  await expect.poll(() => page.evaluate(() => window.__scrollOptions.length)).toBeGreaterThan(0);
  expect(await page.evaluate(() => window.__scrollOptions.at(-1)?.behavior)).toBe("auto");
  await page.evaluate(() => { document.documentElement.style.zoom = "2"; });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);
  await page.getByRole("tab", { name: "Audio" }).click();
  await expect(page.locator("#library-panel")).toBeFocused();
});

test("direct failure stays visible until compatible media is playable", async ({ page }) => {
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", (route) => {
    const mode = new URL(route.request().url()).searchParams.get("mode");
    return mode === "direct"
      ? route.abort("failed")
      : route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#mode-label")).toHaveText("Compatible playback");
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.locator("#technical-message")).not.toContainText("MediaItems");
});

test("compatible seek while paused clears its busy description on metadata", async ({ page }) => {
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", (route) => route.fulfill({ status: 200, contentType: "video/mp4", body: fixture }));
  await openLibrary(page);
  await openVideoView(page);
  await page.getByRole("button", { name: /^Play dvp7\b/ }).first().click();
  await expect(page.locator("#timeline")).toHaveAttribute("max", "10");
  await showPlayerControls(page);
  await page.locator("#play-button").click();
  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "5";
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect(page.locator("#timeline")).toHaveAttribute("aria-busy", "false");
  await expect(page.locator("#timeline-status")).not.toContainText("Starting a compatible stream");
});

test("fullscreen controls remain reachable and Escape exits", async ({ page, browserName }) => {
  test.skip(browserName === "webkit" && process.platform === "linux", "WebKitGTK headless does not expose the Fullscreen API");
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  const stage = page.locator("#player-stage");
  const controls = page.locator("#playback-controls");
  await showPlayerControls(page);
  await page.locator("#fullscreen-button").click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe("player-stage");
  await expect(page.locator("#fullscreen-button")).toBeVisible();
  await expect(page.locator("#stream-info-button")).toBeVisible();
  await stage.dispatchEvent("pointermove");
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#stream-info-dialog")).toBeVisible();
  await page.locator('#stream-info-dialog button[value="close"]').click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe("player-stage");
  await stage.dispatchEvent("pointermove");
  await expect(page.locator("#timeline")).toBeVisible();
  await expect(stage).not.toHaveClass(/controls-visible/, { timeout: 4_000 });
  await expect(controls).toBeHidden();
  await page.keyboard.press("Escape");
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe(null);

  await openAdvancedPlayback(page);
  await page.locator('input[name="stream-mode"][value="compat"]').check();
  await expect(page.locator("#mode-label")).toHaveText("Compatible playback");
  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  await showPlayerControls(page);
  await page.locator("#fullscreen-button").click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe("player-stage");
  await expect(page.locator("#captions-button")).toBeVisible();
  await expect(page.locator("#volume-control")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe(null);
});

test("automated accessibility scan has no serious violations", async ({ page }) => {
  await openLibrary(page);
  await openVideoView(page);
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21aa"]).analyze();
  const serious = results.violations.filter((violation) => ["serious", "critical"].includes(violation.impact));
  expect(serious).toEqual([]);
});

test("mobile layout retains 44px targets without horizontal overflow", async ({ page, isMobile }) => {
  test.skip(!isMobile, "mobile project only");
  await openLibrary(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);
  const undersized = await page.locator("button:visible, select:visible").evaluateAll((elements) => elements
    .map((element) => ({ text: element.textContent.trim(), width: element.getBoundingClientRect().width, height: element.getBoundingClientRect().height }))
    .filter((box) => box.width < 44 || box.height < 44));
  expect(undersized).toEqual([]);

  await selectTaggedVideo(page);
  const title = await page.locator("#now-playing-title").textContent();
  await page.setViewportSize({ width: 915, height: 412 });
  await expect(page.locator("#now-playing-title")).toHaveText(title);
  await expect(page.locator("#fullscreen-button")).toBeVisible();
  await page.setViewportSize({ width: 412, height: 915 });
  await expect(page.locator("#now-playing-title")).toHaveText(title);
});

test("video controls hide after three idle seconds and on pointer leave", async ({ page }) => {
  await openLibrary(page);
  await selectTaggedVideo(page);

  const stage = page.locator("#player-stage");
  const controls = page.locator("#playback-controls");
  await stage.dispatchEvent("pointermove");
  await expect(stage).toHaveClass(/controls-visible/);
  await expect(controls).toBeVisible();

  await page.waitForTimeout(3100);
  await expect(stage).not.toHaveClass(/controls-visible/);
  await expect(controls).toBeHidden();

  await stage.dispatchEvent("pointerenter");
  await expect(controls).toBeVisible();
  await stage.dispatchEvent("pointerleave");
  await expect(controls).toBeHidden();

  await stage.dispatchEvent("pointermove");
  await expect(controls).toBeVisible();
  await page.locator("#play-button").focus();
  await stage.dispatchEvent("pointerleave");
  await expect(controls).toBeVisible();
});
