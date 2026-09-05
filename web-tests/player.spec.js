import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

const compatibleFixture = fileURLToPath(new URL("../testdata/library/video/tagged.mp4", import.meta.url));
const execFileAsync = promisify(execFile);

async function fragmentedCompatibleFixture({ profile = "baseline", level = "3.1" } = {}) {
  const { stdout } = await execFileAsync("ffmpeg", [
    "-nostdin", "-v", "error", "-i", compatibleFixture,
    "-map", "0:v:0", "-map", "0:a:0",
    "-c:v", "libx264", "-profile:v", profile, "-level:v", level, "-bf", "0", "-pix_fmt", "yuv420p",
    "-c:a", "aac", "-ac", "2",
    "-movflags", "frag_keyframe+empty_moov+delay_moov+default_base_moof",
    "-f", "mp4", "pipe:1",
  ], { encoding: null, timeout: 5_000, maxBuffer: 1024 * 1024 });
  return stdout;
}

function fragmentedMp4Layout(bytes) {
  let offset = 0;
  let initEnd = 0;
  while (offset + 8 <= bytes.byteLength) {
    const view = new DataView(bytes.buffer, bytes.byteOffset + offset, Math.min(16, bytes.byteLength - offset));
    const size32 = view.getUint32(0);
    const type = String.fromCharCode(...bytes.subarray(offset + 4, offset + 8));
    const size = size32 === 1 ? Number(view.getBigUint64(8)) : size32 || bytes.byteLength - offset;
    if (!Number.isSafeInteger(size) || size < 8 || offset + size > bytes.byteLength) break;
    offset += size;
    if (type === "moov") initEnd = offset;
  }
  if (!(initEnd > 0 && initEnd < bytes.byteLength)) throw new Error("fixture is not fragmented MP4");
  return { initEnd };
}

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

async function disableFragmentedDelivery(page) {
  await page.addInitScript(() => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function fixtureCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "";
      return canPlayType.call(this, contentType);
    };
    if (typeof globalThis.MediaSource !== "function") return;
    Object.defineProperty(globalThis.MediaSource, "isTypeSupported", {
      configurable: true,
      value: () => false,
    });
  });
}

async function serveFixtureMedia(page, onRequest = () => {}) {
  await disableFragmentedDelivery(page);
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", async (route) => {
    onRequest(new URL(route.request().url()));
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });
}

async function installDeferredDecodingInfo(page) {
  await page.addInitScript(() => {
    const setTimeout = window.setTimeout.bind(window);
    window.setTimeout = (callback, delay = 0, ...args) => setTimeout(
      callback,
      delay === 1_000 ? 30_000 : delay,
      ...args,
    );
    const pending = [];
    let released = false;
    const supported = { supported: true, smooth: true, powerEfficient: true };
    Object.defineProperty(HTMLMediaElement.prototype, "canPlayType", {
      configurable: true,
      value: () => "probably",
    });
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: {
        decodingInfo: (configuration) => released
          ? Promise.resolve(supported)
          : new Promise((resolve) => pending.push({ configuration, resolve })),
      },
    });
    window.__decodingRace = {
      count: () => pending.length,
      release() {
        released = true;
        for (const request of pending.splice(0)) request.resolve(supported);
      },
    };
  });
}

async function installHevcHlsTrial(page, { appleMobile = false, hdr = false } = {}) {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(({ appleMobile }) => {
    Object.defineProperty(navigator, "userAgent", { configurable: true, value: appleMobile
      ? "Mozilla/5.0 (iPad; CPU OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1"
      : "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15" });
    const sources = new WeakMap();
    const positions = new WeakMap();
    window.__hlsTrial = { sources: [], playCalls: 0 };
    HTMLMediaElement.prototype.canPlayType = (type) => String(type).includes("mpegurl") ? "maybe" : "";
    HTMLMediaElement.prototype.play = () => { window.__hlsTrial.playCalls += 1; return Promise.resolve(); };
    HTMLMediaElement.prototype.pause = () => {};
    HTMLMediaElement.prototype.load = function () { positions.set(this, 0); };
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true, get() { return sources.has(this) ? new URL(sources.get(this), document.baseURI).href : ""; },
      set(value) { sources.set(this, value); window.__hlsTrial.sources.push(new URL(value, document.baseURI).href); },
    });
    Object.defineProperty(HTMLMediaElement.prototype, "currentTime", {
      configurable: true, get() { return positions.get(this) || 0; }, set(value) { positions.set(this, value); },
    });
    const getAttribute = Element.prototype.getAttribute;
    const removeAttribute = Element.prototype.removeAttribute;
    HTMLMediaElement.prototype.getAttribute = function (name) {
      return name === "src" ? sources.get(this) || null : getAttribute.call(this, name);
    };
    HTMLMediaElement.prototype.removeAttribute = function (name) {
      if (name === "src") sources.delete(this);
      return removeAttribute.call(this, name);
    };
    // Desktop Safari may advertise MSE too; the trial must still stay native HLS.
    if (globalThis.MediaSource) Object.defineProperty(MediaSource, "isTypeSupported", { configurable: true, value: () => true });
    Object.defineProperty(navigator, "mediaCapabilities", { configurable: true, value: {
      decodingInfo: () => { throw new Error("Native HLS must not wait on a capability promise"); },
    } });
  }, { appleMobile });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    if (hdr) payload.capabilities.video_outputs = [{
      id: "hevc_hdr10", video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
    }];
    for (const item of payload.entries || []) {
      if (item.entry_type !== "media" || item.title !== "tagged") continue;
      Object.assign(item, {
        video_codec: "hevc", video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
        video_repair_required: false, width: 3840, height: 2160, resolution: "3840×2160",
        duration_seconds: 600, duration: "0:10:00.000", stream_metadata_complete: true,
        bit_depth: hdr ? 10 : 8, hdr: hdr ? "hdr10" : "sdr",
      });
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    json: { schema_version: 2, state: "producing", retry_after_seconds: null },
  }));
}

async function deferDeepLinkEnrichment(page, itemId, { failAfterRelease = false } = {}) {
  let markStarted;
  const started = new Promise((resolve) => { markStarted = resolve; });
  let markFailureAttempted;
  const failureAttempted = new Promise((resolve) => { markFailureAttempted = resolve; });
  let releaseRequest;
  const release = new Promise((resolve) => { releaseRequest = resolve; });
  await page.route(`**/api/web/item/${itemId}*`, async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("enrich") === "1") {
      markStarted();
      await release;
      try {
        if (failAfterRelease) {
          markFailureAttempted();
          await route.fulfill({
            status: 503,
            contentType: "application/json",
            body: JSON.stringify({
              schema_version: 2,
              error: {
                code: "transcode_busy",
                message: "busy",
                recoverable: true,
                action: "retry_item",
              },
            }),
          });
        } else {
          await route.fallback();
        }
      } catch (_) {
        // The navigation under test deliberately aborts this request.
      }
      return;
    }
    const response = await route.fetch();
    const payload = await response.json();
    payload.item.stream_metadata_complete = false;
    await route.fulfill({ response, json: payload });
  });
  return { started, failureAttempted, release: releaseRequest };
}

async function showPlayerControls(page) {
  const stage = page.locator("#player-stage");
  await stage.scrollIntoViewIfNeeded();
  await stage.hover();
  await expect(page.locator("#playback-controls")).toBeVisible();
}

async function openAdvancedPlayback(page) {
  await showPlayerControls(page);
  const button = page.locator("#advanced-playback-button");
  await button.focus();
  await expect(button).toBeFocused();
  await button.press("Enter");
  await expect(page.locator("#advanced-playback-dialog")).toBeVisible();
}

async function undersizedTouchTargets(page, selector) {
  return page.locator(selector).evaluateAll((elements) => elements
    .map((element) => ({
      id: element.id,
      text: element.textContent.trim(),
      width: element.getBoundingClientRect().width,
      height: element.getBoundingClientRect().height,
    }))
    // Device-scale rounding can report a nominal 44 CSS pixels a few
    // millionths of a pixel below 44.
    .filter((box) => box.width < 43.99 || box.height < 43.99));
}

async function playerToolbarOverlap(page) {
  return page.evaluate(() => {
    const times = document.querySelector(".timeline-times");
    const timeBounds = times.getBoundingClientRect();
    const overlapping = [...document.querySelectorAll("#playback-controls button")]
      .filter((button) => {
        const style = getComputedStyle(button);
        return style.display !== "none" && style.visibility !== "hidden";
      })
      .map((button) => {
        const bounds = button.getBoundingClientRect();
        const hit = bounds.left < timeBounds.right
          && bounds.right > timeBounds.left
          && bounds.top < timeBounds.bottom
          && bounds.bottom > timeBounds.top;
        return hit ? button.id : null;
      })
      .filter(Boolean);
    return {
      overlapping,
      time: [timeBounds.left, timeBounds.right, timeBounds.top, timeBounds.bottom],
    };
  });
}

async function installDeferredDisplayModeRequests(page) {
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

    const pending = { fullscreen: [], pip: [] };
    const defer = (kind) => new Promise((resolve, reject) => pending[kind].push({ resolve, reject }));
    Element.prototype.requestFullscreen = () => defer("fullscreen");
    Object.defineProperty(document, "pictureInPictureEnabled", {
      configurable: true,
      value: true,
    });
    let pictureInPictureElement = null;
    Object.defineProperty(document, "pictureInPictureElement", {
      configurable: true,
      get: () => pictureInPictureElement,
    });
    HTMLVideoElement.prototype.requestPictureInPicture = () => defer("pip");
    window.__displayModeTest = {
      pending: (kind) => pending[kind].length,
      reject(kind) {
        const deferred = pending[kind].shift();
        if (!deferred) throw new Error(`No pending ${kind} request`);
        deferred.reject(new DOMException(`${kind} request denied`, "NotAllowedError"));
      },
      resolve(kind) {
        const deferred = pending[kind].shift();
        if (!deferred) throw new Error(`No pending ${kind} request`);
        deferred.resolve({});
      },
      setPictureInPicture(active) {
        pictureInPictureElement = active ? document.querySelector("#video-player") : null;
      },
    };
  });
}

async function installIphoneUserAgent(page) {
  await page.addInitScript(() => {
    Object.defineProperty(Navigator.prototype, "userAgent", {
      configurable: true,
      get: () => "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1",
    });
  });
}

async function installAndroidVisualViewport(page, initialViewport) {
  await page.addInitScript((values) => {
    const visualViewport = new EventTarget();
    Object.assign(visualViewport, {
      offsetLeft: 0,
      offsetTop: 0,
      scale: 1,
      ...values,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: visualViewport,
    });
    window.__visualViewportTest = {
      resizeViewport(next) {
        Object.assign(visualViewport, next);
        visualViewport.dispatchEvent(new Event("resize"));
      },
    };
  }, initialViewport);
}

async function installIphoneVideoFullscreen(page) {
  await installIphoneUserAgent(page);
  await page.addInitScript(() => {
    const visualViewport = new EventTarget();
    Object.assign(visualViewport, {
      offsetLeft: 11,
      offsetTop: 17,
      width: 900,
      height: 500,
      scale: 1,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: visualViewport,
    });
    Object.defineProperty(Document.prototype, "fullscreenEnabled", {
      configurable: true,
      get: () => true,
    });
    let stageEnters = 0;
    Object.defineProperty(Element.prototype, "requestFullscreen", {
      configurable: true,
      value() {
        stageEnters += 1;
        return Promise.resolve();
      },
    });
    const active = new WeakSet();
    let enters = 0;
    let exits = 0;
    Object.defineProperty(HTMLVideoElement.prototype, "webkitDisplayingFullscreen", {
      configurable: true,
      get() { return active.has(this); },
    });
    Object.defineProperty(HTMLVideoElement.prototype, "webkitSupportsFullscreen", {
      configurable: true,
      get: () => true,
    });
    HTMLVideoElement.prototype.webkitEnterFullscreen = function webkitEnterFullscreen() {
      enters += 1;
      active.add(this);
      this.dispatchEvent(new Event("webkitbeginfullscreen"));
    };
    HTMLVideoElement.prototype.webkitExitFullscreen = function webkitExitFullscreen() {
      exits += 1;
      active.delete(this);
      this.dispatchEvent(new Event("webkitendfullscreen"));
    };
    window.__iphoneFullscreenTest = {
      counts: () => ({ enters, exits, stageEnters }),
      resizeViewport(values) {
        Object.assign(visualViewport, values);
        visualViewport.dispatchEvent(new Event("resize"));
      },
    };
  });
}

async function installDeferredWakeLock(page) {
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

    let fullscreenElement = null;
    Object.defineProperty(Document.prototype, "fullscreenElement", {
      configurable: true,
      get: () => fullscreenElement,
    });
    Element.prototype.requestFullscreen = function requestFullscreen() {
      fullscreenElement = this;
      document.dispatchEvent(new Event("fullscreenchange"));
      return Promise.resolve();
    };
    document.exitFullscreen = () => {
      fullscreenElement = null;
      document.dispatchEvent(new Event("fullscreenchange"));
      return Promise.resolve();
    };

    const pending = [];
    let current = null;
    let requests = 0;
    let releases = 0;
    Object.defineProperty(navigator, "wakeLock", {
      configurable: true,
      value: {
        request(type) {
          if (type !== "screen") throw new Error(`Unexpected wake-lock type: ${type}`);
          requests += 1;
          return new Promise((resolve, reject) => pending.push({ resolve, reject }));
        },
      },
    });
    window.__wakeLockTest = {
      counts: () => ({ requests, releases, pending: pending.length }),
      resolveNext() {
        const deferred = pending.shift();
        if (!deferred) throw new Error("No pending wake-lock request");
        const sentinel = new EventTarget();
        current = sentinel;
        sentinel.released = false;
        sentinel.release = async () => {
          if (sentinel.released) return;
          sentinel.released = true;
          releases += 1;
          if (current === sentinel) current = null;
          sentinel.dispatchEvent(new Event("release"));
        };
        deferred.resolve(sentinel);
      },
      rejectNext() {
        const deferred = pending.shift();
        if (!deferred) throw new Error("No pending wake-lock request");
        deferred.reject(new DOMException("Wake lock denied", "NotAllowedError"));
      },
      releaseCurrent() {
        if (!current) throw new Error("No active wake lock");
        return current.release();
      },
    };
  });
}

async function startDeferredWakeLockRequest(page) {
  await openLibrary(page);
  await selectTaggedVideo(page);
  await page.locator("#fullscreen-button").evaluate((button) => button.click());
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe("player-stage");
  await page.locator("#video-player").dispatchEvent("playing");
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().requests)).toBe(1);
}

test("folder artwork matches movie poster dimensions across screen sizes", async ({ page }) => {
  await openLibrary(page);
  for (const width of [300, 390, 1280]) {
    await page.setViewportSize({ width, height: 900 });
    await page.getByRole("tab", { name: "Folders", exact: true }).click();
    const folder = page.locator(".media-card.folder .art").first();
    await expect(folder).toBeVisible();
    const folderSize = await folder.evaluate((art) => {
      const bounds = art.getBoundingClientRect();
      const icon = art.querySelector(".folder-icon").getBoundingClientRect();
      return { width: bounds.width, height: bounds.height, iconRatio: icon.width / icon.height };
    });
    expect(folderSize.iconRatio).toBeCloseTo(2, 1);
    await expect(folder.locator(".folder-count")).toBeVisible();
    await openVideoView(page);
    const posterSize = await page.locator(".media-card.video .art").first().evaluate((art) => {
      const bounds = art.getBoundingClientRect();
      return { width: bounds.width, height: bounds.height };
    });
    expect(Math.abs(folderSize.width - posterSize.width)).toBeLessThanOrEqual(1);
    expect(Math.abs(folderSize.height - posterSize.height)).toBeLessThanOrEqual(1);
    expect(folderSize.height / folderSize.width).toBeCloseTo(1.5, 2);
  }
});

test("long item details and settings fit narrow screens without horizontal scrolling", async ({ page }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await openVideoView(page);
  const details = page.getByRole("button", { name: /^Details for Movie/ }).first();
  for (const viewport of [{ width: 300, height: 640 }, { width: 390, height: 320 }]) {
    await page.setViewportSize(viewport);
    await details.click();
    const dialog = page.locator("#item-details-dialog");
    await expect(dialog).toBeVisible();
    const dimensions = await dialog.evaluate((element) => ({
      width: element.clientWidth, contentWidth: element.scrollWidth,
      titleLength: element.querySelector("h2").textContent.length,
    }));
    expect(dimensions.titleLength).toBeGreaterThan(35);
    expect(dimensions.contentWidth).toBeLessThanOrEqual(dimensions.width + 1);
    await dialog.evaluate((element) => { element.scrollTop = element.scrollHeight; });
    await dialog.getByRole("button", { name: "Close item details" }).click();
  }
  await page.setViewportSize({ width: 300, height: 640 });
  await selectTaggedVideo(page);
  await openAdvancedPlayback(page);
  const settings = page.locator("#advanced-playback-dialog");
  expect(await settings.evaluate((dialog) => dialog.scrollWidth <= dialog.clientWidth + 1)).toBe(true);
  await page.locator("#loop-button").click();
  await expect(page.locator("#loop-button")).toHaveAttribute("aria-pressed", "true");
  await page.locator("#loop-button").focus();
  await page.keyboard.press("Tab");
  await page.keyboard.press("Shift+Tab");
  expect(await page.locator("#loop-button").evaluate((button) => getComputedStyle(button).outlineWidth)).toBe("3px");
  const toggleColors = await settings.evaluate((dialog) => ["#loop-button", "#fit-button"]
    .map((selector) => getComputedStyle(dialog.querySelector(selector)).backgroundColor));
  expect(toggleColors[0]).not.toBe(toggleColors[1]);
  await page.locator("#fit-button").click();
  await expect(page.locator("#fit-button")).toHaveText("Fill frame");
  await expect(page.locator("#fit-button")).toHaveAttribute("aria-pressed", "true");
  await settings.getByRole("button", { name: "Close playback settings" }).click();
});

test("modal keyboard input never controls playback behind the dialog", async ({ page }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  for (const [buttonId, dialogId] of [
    ["quality-menu-button", "quality-dialog"],
    ["advanced-playback-button", "advanced-playback-dialog"],
    ["stream-info-button", "stream-info-dialog"],
  ]) {
    await page.locator(`#${buttonId}`).evaluate((button) => button.click());
    const dialog = page.locator(`#${dialogId}`);
    await expect(dialog).toBeVisible();
    const unhandled = await dialog.evaluate((element) => {
      element.tabIndex = -1;
      element.focus();
      return element.dispatchEvent(new KeyboardEvent("keydown", { key: "m", bubbles: true, cancelable: true }));
    });
    expect(unhandled).toBe(true);
    await expect(page.locator("#mute-button")).toHaveAttribute("aria-pressed", "false");
    await dialog.locator(".dialog-close").focus();
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(page.locator("#now-playing-title")).toHaveText("tagged");
    await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "watch");
  }
});

test("empty search offers a focused reset without changing the view or playback", async ({ page }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  const source = await page.locator("#video-player").getAttribute("src");
  await page.locator("#layout-browse").click();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator("#search-input").fill("unmatched".repeat(12));
  await expect(page.locator("#library-empty-title")).toContainText("No results for");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.getByRole("button", { name: "Clear search", exact: true }).click();
  await expect(page.locator("#search-input")).toBeFocused();
  await expect(page.locator("#search-input")).toHaveValue("");
  await expect(page.locator(".media-card.video").first()).toBeVisible();
  await expect(page.locator("#tab-video")).toHaveAttribute("aria-selected", "true");
  await expect(page.locator("#video-player")).toHaveAttribute("src", source);
  await expect(page.locator("#library-clear-search")).toBeHidden();
  await page.getByRole("tab", { name: "Continue watching" }).click();
  await expect(page.locator("#library-empty-title")).toHaveText("Nothing to continue yet");
  await expect(page.locator("#library-empty-detail")).toContainText("on this browser");
});

for (const display of ["expanded", "fullscreen"]) {
  test(`recovery actions stay reachable in ${display} playback and close clears the error`, async ({ page, browserName, isMobile }) => {
    test.skip(display === "fullscreen" && (isMobile || browserName === "webkit" && process.platform === "linux"), "native element fullscreen is unavailable on this project");
    await usePreference(page, "stream", "direct");
    if (display === "expanded") await installIphoneUserAgent(page);
    await page.addInitScript(() => {
      const sources = new WeakMap();
      HTMLMediaElement.prototype.load = () => {};
      HTMLMediaElement.prototype.pause = () => {};
      HTMLMediaElement.prototype.play = () => Promise.resolve();
      Object.defineProperty(HTMLMediaElement.prototype, "src", {
        configurable: true,
        get() { return sources.get(this) || ""; },
        set(value) { sources.set(this, new URL(value, document.baseURI).href); },
      });
    });
    await openLibrary(page);
    await selectTaggedVideo(page);
    await showPlayerControls(page);
    await page.locator("#fullscreen-button").click();
    await expect(page.locator("#fullscreen-button")).toHaveAttribute("aria-pressed", "true");
    await page.locator("#video-player").dispatchEvent("error");
    const message = page.locator("#player-stage > #player-message");
    await expect(message).toBeVisible();
    await expect(message).toContainText("cannot play the original file");
    const action = page.locator("#try-compatible");
    await expect(action).toBeVisible();
    expect(await action.evaluate((button) => {
      const box = button.getBoundingClientRect();
      return button.contains(document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2));
    })).toBe(true);
    await page.locator("#close-player-button").click();
    await page.locator("#layout-watch").click();
    await expect(page.locator("#player-empty")).toBeVisible();
    await expect(page.locator("#stage-progress")).toBeHidden();
    await expect(page.locator("#player-message")).toBeHidden();
    await expect(page.locator("#player-panel > #player-message")).toHaveCount(1);
  });
}

test("closing during loading leaves a clean empty player", async ({ page }) => {
  await installDeferredWakeLock(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#stage-progress")).toBeVisible();
  await showPlayerControls(page);
  await page.locator("#close-player-button").click();
  await page.locator("#layout-watch").click();
  await expect(page.locator("#player-empty")).toBeVisible();
  await expect(page.locator("#stage-progress")).toBeHidden();
});

test("library tabs, player scoping, and overlay controls work", async ({ page }) => {
  const errors = await openLibrary(page);
  const folders = page.getByRole("tab", { name: "Folders" });
  await folders.focus();
  await folders.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "All media" })).toHaveAttribute("aria-selected", "true");
  await openVideoView(page);
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();

  const stage = page.locator("#player-stage");
  for (const selector of ["#close-player-button", "#timeline", "#volume-control", "#stream-info-button", "#captions-button", "#audio-track-controls", "#fullscreen-button"]) {
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
  await expect(page.locator("#quality-menu-button")).toHaveText("Auto");
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

test("top bar stays compact on desktop and preserves touch targets", async ({ page, isMobile }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  const topbar = page.locator(".topbar");
  const checkHeight = async () => {
    const height = await topbar.evaluate((header) => header.getBoundingClientRect().height);
    if (isMobile) expect(height).toBeGreaterThanOrEqual(44);
    else expect(height).toBe(32);
  };
  await checkHeight();
  await selectTaggedVideo(page);
  for (const width of [300, 760, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await page.evaluate(() => window.scrollTo(0, 0));
    await checkHeight();
    await expect.poll(async () => {
      await topbar.scrollIntoViewIfNeeded();
      const controls = await topbar.locator("button, a").evaluateAll((elements) => elements
        .filter((element) => element.checkVisibility()).map((element) => {
          const rect = element.getBoundingClientRect();
          const hit = document.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2);
          return { height: rect.height, reachable: element === hit || element.contains(hit) };
        }));
      return controls.every((control) => control.height >= (isMobile ? 44 : 28) && control.reachable);
    }).toBe(true);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  }
  await page.locator("#layout-browse").focus();
  await page.keyboard.press("Enter");
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "browse");
  await expect(page.locator("#layout-browse")).toBeFocused();
  if (!isMobile) {
    expect(await page.locator("#player-panel").evaluate((panel) => getComputedStyle(panel).top)).toBe("48px");
  }
});

test("compact settings keep two-column controls readable and touch-sized", async ({ page }, testInfo) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  await openAdvancedPlayback(page);
  const dialog = page.locator("#advanced-playback-dialog");
  for (const width of [300, 412, 1280]) {
    await page.setViewportSize({ width, height: 900 });
    const layout = await dialog.evaluate((element) => {
      const grid = element.querySelector(".advanced-selects");
      const controls = [...grid.querySelectorAll("select")].filter((control) => control.checkVisibility());
      return {
        overflow: element.scrollWidth > element.clientWidth,
        dimensions: [element.clientWidth, element.scrollWidth],
        spilling: [...element.querySelectorAll("*")].filter((child) => child.checkVisibility() && child.scrollWidth > child.clientWidth + 1)
          .map((child) => [child.id || child.tagName, child.getAttribute("for"), child.clientWidth, child.scrollWidth, child.textContent.slice(0, 80)]),
        overflowing: [...element.querySelectorAll("*")].filter((child) => child.checkVisibility()
          && child.getBoundingClientRect().right > element.getBoundingClientRect().right)
          .map((child) => child.id || child.className || child.tagName),
        columns: getComputedStyle(grid).gridTemplateColumns.split(" ").length,
        touchSized: controls.every((control) => control.getBoundingClientRect().height >= 44),
        fits: controls.every((control) => control.getBoundingClientRect().right <= element.getBoundingClientRect().right),
        height: element.getBoundingClientRect().height,
      };
    });
    expect(layout.overflow, JSON.stringify(layout)).toBe(false);
    expect(layout.columns).toBe(2);
    expect(layout.touchSized).toBe(true);
    expect(layout.fits).toBe(true);
    if (width === 1280) expect(layout.height).toBeLessThan(650);
    if (testInfo.project.name === "chromium") {
      const path = testInfo.outputPath(`compact-settings-${width}.png`);
      await dialog.screenshot({ path });
      await testInfo.attach(`compact-settings-${width}`, { path, contentType: "image/png" });
    }
  }
  const automatic = dialog.getByRole("radio", { name: /^Automatic/ });
  for (const [value, name, hint, tooltip] of [
    ["auto", "Automatic", "Prefer original; convert when needed", /Prefer the original file/],
    ["direct", "Original only", "No conversion; may not play", /without server conversion or automatic fallback/],
    ["compat", "Prepared streaming", "Preserve compatible streams", /does not force video re-encoding/],
  ]) {
    const radio = dialog.locator(`input[name="stream-mode"][value="${value}"]`);
    await expect(radio).toHaveAccessibleName(new RegExp(`^${name}`));
    await expect(radio).toHaveAccessibleDescription(hint);
    const card = radio.locator("..");
    await card.hover();
    await expect(card).toHaveAttribute("title", tooltip);
    await expect(automatic).toBeChecked();
  }
  await automatic.focus();
  await automatic.press("ArrowRight");
  const original = dialog.getByRole("radio", { name: /^Original / });
  await expect(original).toBeChecked();
  expect(await original.evaluate((input) => getComputedStyle(input.closest("label")).outlineWidth)).toBe("3px");
  expect(await original.evaluate((input) => input.closest("label").getBoundingClientRect().height)).toBeGreaterThanOrEqual(44);
  if (testInfo.project.name === "chromium") {
    await page.emulateMedia({ forcedColors: "active" });
    expect(await original.evaluate((input) => getComputedStyle(input).opacity)).toBe("1");
    expect(await page.locator("#quality-control").evaluate((select) => getComputedStyle(select).appearance)).toBe("auto");
  }
  const prepared = dialog.getByRole("radio", { name: /^Prepared streaming/ });
  await prepared.check();
  await expect(prepared).toBeChecked();
});

test("stream information downloads the current original without restarting playback", async ({ page }, testInfo) => {
  await installHevcHlsTrial(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  const sources = await page.evaluate(() => [...window.__hlsTrial.sources]);
  await page.locator("#stream-info-button").evaluate((button) => button.click());
  const dialog = page.locator("#stream-info-dialog");
  const link = dialog.getByRole("link", { name: /^Download original file / });
  await expect(link).toBeVisible();
  await expect(link).toHaveAttribute("href", /^\/web\/download\/\d+$/);
  const expectedFile = await link.getAttribute("download");
  const [download] = await Promise.all([page.waitForEvent("download"), link.click()]);
  expect(download.suggestedFilename()).toBe(expectedFile);
  await download.cancel();
  expect(await page.evaluate(() => window.__hlsTrial.sources)).toEqual(sources);
  await expect(dialog).toBeVisible();
  await expect(page.locator("#stream-diagnostic-facts")).toBeHidden();
  if (testInfo.project.name === "chromium") {
    const path = testInfo.outputPath("compact-stream-info.png");
    await dialog.screenshot({ path });
    await testInfo.attach("compact-stream-info", { path, contentType: "image/png" });
  }
  const summary = page.locator("#stream-diagnostics summary");
  await summary.focus();
  await summary.press("Enter");
  await expect(page.locator("#stream-diagnostic-facts")).toBeVisible();
  await summary.press("Enter");
  await expect(page.locator("#stream-diagnostic-facts")).toBeHidden();
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await page.getByRole("tab", { name: "Audio", exact: true }).click();
  await page.locator("[data-media-id]").first().getByRole("button", { name: /^Play / }).click();
  await page.locator("#stream-info-button").evaluate((button) => button.click());
  await expect(page.locator("#stream-info-download")).toBeHidden();
  await expect(page.locator("#stream-info-download")).not.toHaveAttribute("href");
});

test("scrollable player dialogs keep their X close action pinned", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 320 });
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  const expectPinnedClose = async (dialogSelector, accessibleName) => {
    const dialog = page.locator(dialogSelector);
    await expect(dialog).toBeVisible();
    expect(await dialog.evaluate((element) => element.scrollHeight > element.clientHeight)).toBe(true);
    await dialog.evaluate((element) => { element.scrollTop = element.scrollHeight; });
    const close = dialog.getByRole("button", { name: accessibleName });
    await expect(close.locator("svg")).toHaveCount(1);
    await expect(close).toBeVisible();
    const placement = await close.evaluate((button) => {
      const control = button.getBoundingClientRect();
      const icon = button.querySelector("svg").getBoundingClientRect();
      const dialogBounds = button.closest("dialog").getBoundingClientRect();
      return {
        insideTop: control.top >= dialogBounds.top,
        insideBottom: control.bottom <= dialogBounds.bottom,
        centerX: Math.abs(icon.left + icon.width / 2 - (control.left + control.width / 2)),
        centerY: Math.abs(icon.top + icon.height / 2 - (control.top + control.height / 2)),
      };
    });
    expect(placement.insideTop).toBe(true);
    expect(placement.insideBottom).toBe(true);
    expect(placement.centerX).toBeLessThanOrEqual(0.5);
    expect(placement.centerY).toBeLessThanOrEqual(0.5);
    await close.click();
    await expect(dialog).toBeHidden();
  };

  await openAdvancedPlayback(page);
  await expectPinnedClose("#advanced-playback-dialog", "Close playback settings");
  await page.locator("#stream-info-button").evaluate((button) => button.click());
  await expectPinnedClose("#stream-info-dialog", "Close stream information");
});

test("Browse and Watch switch the full library without restarting playback", async ({ page }) => {
  await page.setViewportSize({ width: 1600, height: 900 });
  await page.addInitScript(() => {
    const load = HTMLMediaElement.prototype.load;
    let loadCalls = 0;
    HTMLMediaElement.prototype.load = function countedLoad() {
      loadCalls += 1;
      return load.call(this);
    };
    window.__mediaLoadCalls = () => loadCalls;
  });
  await serveFixtureMedia(page);
  await openLibrary(page);

  const main = page.locator("#app-main");
  const playerPanel = page.locator("#player-panel");
  const libraryPanel = page.locator(".library");
  const browseButton = page.locator("#layout-browse");
  const watchButton = page.locator("#layout-watch");
  await expect(main).toHaveAttribute("data-layout", "browse");
  await expect(browseButton).toHaveAttribute("aria-pressed", "true");
  await expect(watchButton).toHaveAttribute("aria-pressed", "false");
  await expect(playerPanel).toBeHidden();
  const browseWidth = await libraryPanel.evaluate((element) => element.getBoundingClientRect().width);
  const browseColumns = await page.locator("#media-grid").evaluate((grid) => (
    getComputedStyle(grid).gridTemplateColumns.split(" ").filter(Boolean).length
  ));
  expect(browseColumns).toBeGreaterThanOrEqual(6);

  await openVideoView(page);
  await page.locator("#search-input").fill("tagged");
  await expect(page.getByRole("button", { name: /^Play tagged\b/ })).toBeVisible();
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect(main).toHaveAttribute("data-layout", "watch");
  await expect(playerPanel).toBeVisible();
  await expect(watchButton).toHaveAttribute("aria-pressed", "true");
  const watchWidth = await libraryPanel.evaluate((element) => element.getBoundingClientRect().width);
  expect(browseWidth).toBeGreaterThan(watchWidth * 2);

  const source = await page.locator("#video-player").getAttribute("src");
  const loads = await page.evaluate(() => window.__mediaLoadCalls());
  await browseButton.click();
  await expect(main).toHaveAttribute("data-layout", "browse");
  await expect(playerPanel).toBeHidden();
  await expect(browseButton).toBeFocused();
  await expect(page.locator("#search-input")).toHaveValue("tagged");
  await expect(page).toHaveURL(/layout=browse/);
  expect(await page.locator("#video-player").getAttribute("src")).toBe(source);
  expect(await page.evaluate(() => window.__mediaLoadCalls())).toBe(loads);

  const nowPlaying = page.locator("#show-player");
  await expect(nowPlaying).toHaveAttribute("aria-label", "Show player for tagged");
  await nowPlaying.click();
  await expect(main).toHaveAttribute("data-layout", "watch");
  await expect(playerPanel).toBeVisible();
  await expect(page.locator("#player-stage")).toBeFocused();
  expect(await page.locator("#video-player").getAttribute("src")).toBe(source);
  expect(await page.evaluate(() => window.__mediaLoadCalls())).toBe(loads);
});

test("Close player stops playback and returns to the library", async ({ page }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await openVideoView(page);
  await page.locator("#search-input").fill("tagged");
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "watch");
  await expect(page.locator("#player-panel")).toBeVisible();
  await expect(page).toHaveURL(/item=/);
  const source = await page.locator("#video-player").getAttribute("src");
  expect(source).toBeTruthy();
  await expect(page.locator(".media-card.playing")).toHaveCount(1);

  const closeButton = page.locator("#close-player-button");
  await expect(closeButton).toBeVisible();
  const closeBox = await closeButton.boundingBox();
  expect(closeBox.width).toBeGreaterThanOrEqual(43.99);
  expect(closeBox.height).toBeGreaterThanOrEqual(43.99);
  await closeButton.click();

  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "browse");
  await expect(page.locator("#player-panel")).toBeHidden();
  await expect(page.locator("#library-panel")).toBeFocused();
  await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
  await expect(page.locator("#now-playing")).toBeHidden();
  await expect(page.locator(".media-card.playing")).toHaveCount(0);
  expect(new URL(page.url()).searchParams.has("item")).toBe(false);
  expect(new URL(page.url()).searchParams.get("layout")).toBeNull();
  expect(await page.locator("#video-player").getAttribute("src")).toBeFalsy();

  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "watch");
  await expect(page.locator("#now-playing-title")).toHaveText("tagged");
  await expect(page.locator("#player-panel")).toBeVisible();

  await page.locator("#player-stage").focus();
  await page.keyboard.press("Escape");
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "browse");
  await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
});

test("Close player stops immediately while picture-in-picture exit is pending", async ({ page }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video");
  const secondTitle = await cards.nth(1).locator(".card-title").textContent();
  await cards.first().locator(".card-button").click();
  await page.evaluate(() => {
    Object.defineProperty(document, "pictureInPictureElement", {
      configurable: true, get: () => document.getElementById("video-player"),
    });
    document.exitPictureInPicture = () => new Promise((resolve) => {
      window.__finishPiPExit = () => {
        Object.defineProperty(document, "pictureInPictureElement", { configurable: true, value: null });
        resolve();
      };
    });
  });
  await page.locator("#close-player-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
  expect(await page.locator("#video-player").getAttribute("src")).toBeFalsy();
  await cards.nth(1).locator(".card-button").click();
  await page.evaluate(() => window.__finishPiPExit());
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "watch");
});

test("linked item details start loading alongside the library", async ({ page, request }) => {
  const response = await request.get("/api/web/library?view=library&kind=video");
  expect(response.ok()).toBe(true);
  const payload = await response.json();
  const item = payload.entries.find((entry) => entry.title === "tagged");
  expect(item).toBeTruthy();
  let releaseLibrary;
  const held = new Promise((resolve) => { releaseLibrary = resolve; });
  let itemRequested = false;
  await page.route("**/api/web/library?**", async (route) => {
    await held;
    await route.fallback();
  });
  await page.route(`**/api/web/item/${item.id}*`, async (route) => {
    itemRequested = true;
    await route.fallback();
  });
  await serveFixtureMedia(page);
  try {
    await page.goto(`/?view=video&item=${item.id}`);
    await expect.poll(() => itemRequested).toBe(true);
    expect(await page.locator("#video-player").getAttribute("src")).toBeFalsy();
  } finally {
    releaseLibrary();
  }
  await expect(page.locator("#now-playing-title")).toHaveText("tagged");
  await expect(page.locator("#video-player")).toHaveAttribute("src", /web\/media/);
});

test("queue selection updates the shared URL, page title, and current library card", async ({ page }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video");
  const secondId = await cards.nth(1).getAttribute("data-media-id");
  const secondTitle = await cards.nth(1).locator(".card-title").textContent();
  await cards.first().locator(".card-button").click();
  await expect(page.locator("#next-button")).toBeEnabled();
  await page.locator("#next-button").evaluate((button) => button.click());
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect.poll(() => new URL(page.url()).searchParams.get("item")).toBe(secondId);
  await expect(page).toHaveTitle(`${secondTitle} · rustyDLNA-web-test`);
  await expect(page.locator(".media-card.playing")).toHaveAttribute("data-media-id", secondId);
  await page.getByRole("tab", { name: "Folders" }).click();
  expect(new URL(page.url()).searchParams.get("item")).toBe(secondId);
});

test("playback clock updates preserve stream-information text and nodes", async ({ page }) => {
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  const video = page.locator("#video-player");
  await expect.poll(() => video.evaluate((player) => player.readyState)).toBeGreaterThanOrEqual(2);
  await video.evaluate((player) => player.pause());
  await page.locator("#stream-info-button").evaluate((button) => button.click());
  await expect(page.locator("#source-stream-facts")).toContainText("H.264");
  const unchanged = await page.evaluate(() => {
    const facts = document.getElementById("source-stream-facts");
    const row = facts.firstElementChild;
    const player = document.getElementById("video-player");
    for (let index = 0; index < 10; index += 1) player.dispatchEvent(new Event("timeupdate"));
    return facts.firstElementChild === row;
  });
  expect(unchanged).toBe(true);
});

test("explicit layout URLs survive reload without adding history entries", async ({ page }) => {
  await page.goto("/?view=video&layout=watch");
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "watch");
  await expect(page.locator("#player-panel")).toBeVisible();
  const historyLength = await page.evaluate(() => history.length);

  await page.locator("#layout-browse").click();
  await expect(page).toHaveURL(/\?view=video$/);
  await expect(page.locator("#player-panel")).toBeHidden();
  await page.locator("#layout-watch").click();
  await expect(page).toHaveURL(/\?view=video&layout=watch$/);
  expect(await page.evaluate(() => history.length)).toBe(historyLength);

  await page.reload();
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "watch");
  await expect(page.locator("#layout-watch")).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator("#player-panel")).toBeVisible();
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
  const exactId = "9007199254740993";
  await page.addInitScript(() => {
    const getItem = Storage.prototype.getItem;
    window.__progressStorageReads = 0;
    Storage.prototype.getItem = function countedGetItem(key) {
      if (key === "rustydlna.webProgress.v1") window.__progressStorageReads += 1;
      return getItem.call(this, key);
    };
  });
  const libraryRequests = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === "/api/web/library") libraryRequests.push(url);
  });
  await page.route("**/api/web/library?*", async (route) => {
    const url = new URL(route.request().url());
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media") {
        entry.duration_seconds = 600;
        entry.duration = "0:10:00.000";
      }
    }
    if (url.searchParams.get("view") === "continue"
      && url.searchParams.get("ids")?.split(",").includes(exactId)
      && payload.entries.length > 0) {
      payload.entries.unshift({
        ...payload.entries[0],
        id: exactId,
        title: "Exact i64 ID",
        file_name: "exact-i64-id.mp4",
      });
    }
    await route.fulfill({ response, json: payload });
  });
  await page.goto("/?view=video");
  const first = page.locator("[data-media-id]").first();
  const itemId = await first.getAttribute("data-media-id");
  const title = await first.locator(".card-title").textContent();
  const expectedIds = await page.evaluate(({ exactId, itemId }) => {
    const now = Date.now();
    const progress = {
      [exactId]: { position: 125, duration: 600, updated: now + 1 },
      [itemId]: { position: 120, duration: 600, updated: now },
    };
    const ids = [exactId, String(itemId)];
    for (let index = 0; index < 498; index += 1) {
      const id = String(8_000_000 + index);
      progress[id] = { position: 120, duration: 600, updated: now - index - 1 };
      ids.push(id);
    }
    localStorage.setItem("rustydlna.webProgress.v1", JSON.stringify(progress));
    return ids;
  }, { exactId, itemId });
  await page.reload();
  const requestsBeforeContinue = libraryRequests.length;
  const readsBeforeContinue = await page.evaluate(() => window.__progressStorageReads);
  await page.getByRole("tab", { name: "Continue watching" }).click();
  await expect(page.locator(`[data-media-id="${exactId}"] .card-title`)).toHaveText("Exact i64 ID");
  await expect(page.locator(`[data-media-id="${exactId}"] .progress-actions`)).toContainText("2:05 watched");
  await expect(page.locator(`[data-media-id="${itemId}"] .card-title`)).toHaveText(title);
  const continueRequests = libraryRequests.slice(requestsBeforeContinue);
  expect(continueRequests).toHaveLength(5);
  expect(continueRequests.every((url) => url.searchParams.get("view") === "continue")).toBe(true);
  expect(continueRequests.every((url) => !url.searchParams.has("offset"))).toBe(true);
  const requestedIds = continueRequests.flatMap((url) => url.searchParams.get("ids").split(","));
  expect(requestedIds).toEqual(expectedIds);
  expect(continueRequests.every((url) => url.searchParams.get("ids").split(",").length <= 100)).toBe(true);
  expect(continueRequests[0].searchParams.has("generation")).toBe(false);
  const pinnedGeneration = continueRequests[1].searchParams.get("generation");
  expect(pinnedGeneration).toMatch(/^\d+$/);
  expect(continueRequests.slice(1).every((url) => url.searchParams.get("generation") === pinnedGeneration)).toBe(true);
  expect(await page.evaluate(() => window.__progressStorageReads)).toBe(readsBeforeContinue + 1);

  await page.locator("#search-input").fill("definitely absent from every title");
  await expect(page.locator(`[data-media-id="${itemId}"]`)).toHaveCount(0);
  await expect(page.locator("#library-empty-title")).toContainText("No results for");
  await page.locator("#search-input").fill("");
  await expect(page.locator(`[data-media-id="${itemId}"] .card-title`)).toHaveText(title);
  await page.getByRole("button", { name: `Clear progress for ${title}` }).click();
  await expect(page.locator(`[data-media-id="${itemId}"]`)).toHaveCount(0);
  await expect(page.locator("#library-empty-title")).toHaveText("Nothing to continue yet");
});

test("navigating away aborts a later Continue Watching batch", async ({ page }) => {
  await page.addInitScript(() => {
    const progress = {};
    for (let index = 0; index < 101; index += 1) {
      progress[String(8_100_000 + index)] = {
        position: 120,
        duration: 600,
        updated: 10_000 - index,
      };
    }
    localStorage.setItem("rustydlna.webProgress.v1", JSON.stringify(progress));
  });
  let continueBatch = 0;
  let markSecondBatch;
  const secondBatchStarted = new Promise((resolve) => { markSecondBatch = resolve; });
  let releaseSecondBatch;
  const secondBatchRelease = new Promise((resolve) => { releaseSecondBatch = resolve; });
  await page.route("**/api/web/library?*", async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("view") !== "continue") return route.fallback();
    continueBatch += 1;
    if (continueBatch === 2) {
      markSecondBatch();
      await secondBatchRelease;
    }
    try {
      await route.fallback();
    } catch (_) {
      // Navigating away deliberately aborts the delayed request.
    }
  });

  await page.goto("/?view=continue");
  await secondBatchStarted;
  await page.getByRole("tab", { name: "Videos" }).click();
  releaseSecondBatch();
  await expect(page.getByRole("tab", { name: "Videos" })).toHaveAttribute("aria-selected", "true");
  await expect(page.locator(".media-card.video").first()).toBeVisible();
  await expect(page.locator("#library-empty-title")).not.toHaveText("Could not load the library");
});

test("item details and validated deep links remain usable", async ({ page }) => {
  await page.goto("/?view=video");
  const first = page.locator("[data-media-id]").first();
  const itemId = await first.getAttribute("data-media-id");
  const title = (await first.locator(".card-title").textContent()).trim();
  await page.getByRole("button", { name: `Details for ${title}` }).click();
  await expect(page.locator("#item-details-dialog")).toBeVisible();
  await expect(page.locator("#item-details-title")).toHaveText(title);
  const detailsClose = page.getByRole("button", { name: "Close item details" });
  await expect(detailsClose.locator("svg")).toHaveCount(1);
  const closeAlignment = await detailsClose.evaluate((button) => {
    const control = button.getBoundingClientRect();
    const icon = button.querySelector("svg").getBoundingClientRect();
    const dialog = button.closest("dialog").getBoundingClientRect();
    return {
      centerX: Math.abs(icon.left + icon.width / 2 - (control.left + control.width / 2)),
      centerY: Math.abs(icon.top + icon.height / 2 - (control.top + control.height / 2)),
      rightInset: dialog.right - control.right,
      topInset: control.top - dialog.top,
    };
  });
  expect(closeAlignment.centerX).toBeLessThanOrEqual(0.5);
  expect(closeAlignment.centerY).toBeLessThanOrEqual(0.5);
  expect(closeAlignment.rightInset).toBeLessThanOrEqual(24);
  expect(closeAlignment.topInset).toBeLessThanOrEqual(24);
  const downloadLink = page.getByRole("link", { name: /^Download original file / });
  await expect(downloadLink).toBeVisible();
  await expect(downloadLink).toHaveAttribute("href", new RegExp(`^/web/download/${itemId}$`));
  const expectedFileName = await downloadLink.getAttribute("download");
  const [download] = await Promise.all([
    page.waitForEvent("download"),
    downloadLink.click(),
  ]);
  expect(download.url()).toMatch(new RegExp(`/web/download/${itemId}$`));
  expect(download.suggestedFilename()).toBe(expectedFileName);
  await download.cancel();
  await page.getByRole("button", { name: "Close item details" }).click();

  await page.getByRole("tab", { name: "Audio" }).click();
  const firstAudio = page.locator("[data-media-id]").first();
  const audioTitle = (await firstAudio.locator(".card-title").textContent()).trim();
  await firstAudio.getByRole("button", { name: `Details for ${audioTitle}` }).click();
  await expect(page.locator("#item-details-download")).toBeHidden();
  await page.getByRole("button", { name: "Close item details" }).click();

  await page.goto(`/?view=video&item=${itemId}&t=5`);
  await expect(page.locator("#now-playing-title")).toHaveText(title);
  await expect(page).toHaveURL(new RegExp(`item=${itemId}`));

  await page.goto("/?view=video&item=999999999");
  await expect(page.locator("#player-empty-text")).toContainText("linked title is not available");
});

test("movie details keep the full plot behind an explicit spoiler disclosure", async ({ page }) => {
  await page.goto("/?view=video");
  const movie = page.locator("[data-media-id]").filter({ hasText: "Fixture Movie" });
  await movie.getByRole("button", { name: "Details for Fixture Movie" }).click();

  await expect(page.locator("#item-details-about")).toBeVisible();
  await expect(page.locator("#item-details-summary")).toHaveText("A tiny spoiler-free fixture.");
  await expect(page.locator("#item-details-plot-text")).toBeHidden();
  await page.getByText("Reveal full plot (spoilers)", { exact: true }).click();
  await expect(page.locator("#item-details-plot-text")).toBeVisible();
  await expect(page.locator("#item-details-plot-text")).toHaveText(
    "A full fixture plot in which the ending is revealed.",
  );

  await page.getByRole("button", { name: "Close item details" }).click();
  await movie.getByRole("button", { name: "Details for Fixture Movie" }).click();
  await expect(page.locator("#item-details-plot-text")).toBeHidden();
});

test("a popstate without an item cancels a delayed deep-link selection", async ({ page }) => {
  await page.goto("/?view=video");
  const itemId = await page.locator("[data-media-id]").first().getAttribute("data-media-id");
  const deferred = await deferDeepLinkEnrichment(page, itemId);
  const mediaRequests = [];
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.startsWith("/web/media/")) mediaRequests.push(request.url());
  });

  await page.goto(`/?view=video&item=${itemId}&t=5`);
  await deferred.started;
  await page.evaluate(() => {
    history.pushState({}, "", "/?view=video");
    window.dispatchEvent(new PopStateEvent("popstate"));
  });
  await expect(page.locator("#loading")).toBeHidden();
  deferred.release();
  await page.waitForTimeout(100);

  await expect(page.locator("#player-stage")).not.toHaveClass(/has-media/);
  await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
  await expect(page.locator("#player-empty-text")).toHaveText("Choose something from your library");
  await expect(page.locator("#player-message")).toBeHidden();
  expect(await page.evaluate(() => document.activeElement?.id)).not.toBe("player-stage");
  expect(mediaRequests).toEqual([]);
});

test("tab navigation clears a delayed initial deep link", async ({ page }) => {
  await page.goto("/?view=video");
  const itemId = await page.locator("[data-media-id]").first().getAttribute("data-media-id");
  const deferred = await deferDeepLinkEnrichment(page, itemId);
  const mediaRequests = [];
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.startsWith("/web/media/")) mediaRequests.push(request.url());
  });

  await page.goto(`/?view=video&item=${itemId}&t=5`);
  await deferred.started;
  await page.getByRole("tab", { name: "Audio" }).click();
  await expect(page.getByRole("tab", { name: "Audio" })).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/\?view=audio$/);
  deferred.release();
  await page.waitForTimeout(100);

  await expect(page.locator("#player-stage")).not.toHaveClass(/has-media/);
  await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
  await expect(page.locator("#player-empty-text")).toHaveText("Choose something from your library");
  await expect(page.locator("#player-message")).toBeHidden();
  expect(await page.evaluate(() => document.activeElement?.id)).not.toBe("player-stage");
  expect(mediaRequests).toEqual([]);
});

test("ordinary tab navigation preserves an already committed linked title", async ({ page }) => {
  await page.goto("/?view=video");
  const card = page.locator("[data-media-id]").first();
  const itemId = await card.getAttribute("data-media-id");
  const title = (await card.locator(".card-title").textContent()).trim();

  await page.goto(`/?view=video&item=${itemId}&t=5`);
  await expect(page.locator("#now-playing-title")).toHaveText(title);
  await page.getByRole("tab", { name: "Audio" }).click();

  await expect(page.getByRole("tab", { name: "Audio" })).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(new RegExp(`\\?view=audio&item=${itemId}&t=5$`));
  await expect(page.locator("#now-playing-title")).toHaveText(title);
});

test("a current deep-link enrichment failure remains playable and retryable", async ({ page }) => {
  await page.goto("/?view=video");
  const card = page.locator("[data-media-id]").first();
  const itemId = await card.getAttribute("data-media-id");
  const title = (await card.locator(".card-title").textContent()).trim();
  let enrichmentAttempts = 0;
  await page.route(`**/api/web/item/${itemId}*`, async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("enrich") === "1") {
      enrichmentAttempts += 1;
      if (enrichmentAttempts === 1) {
        await route.fulfill({
          status: 503,
          contentType: "application/json",
          body: JSON.stringify({
            schema_version: 2,
            error: {
              code: "transcode_busy",
              message: "busy",
              recoverable: true,
              action: "retry_item",
            },
          }),
        });
        return;
      }
      await route.fallback();
      return;
    }
    const response = await route.fetch();
    const payload = await response.json();
    payload.item.stream_metadata_complete = false;
    await route.fulfill({ response, json: payload });
  });
  await serveFixtureMedia(page);

  await page.goto(`/?view=video&item=${itemId}&t=5`);
  await expect(page.locator("#now-playing-title")).toHaveText(title);
  await expect(page).toHaveURL(new RegExp(`item=${itemId}`));
  await expect(page.locator("#video-player")).toHaveAttribute("src", /web\/media\//);
  await expect(page.locator("#audio-track-status")).toContainText("unavailable");
  await openAdvancedPlayback(page);
  await expect(page.locator("#audio-track-retry")).toBeVisible();
  await page.locator("#audio-track-retry").click();
  await expect(page.locator("#audio-track-retry")).toBeHidden();
  await expect(page.locator("#audio-track-status")).toBeHidden();
  expect(enrichmentAttempts).toBe(2);
});

test("a newer deep link wins when the previous enrichment later fails", async ({ page }) => {
  await page.goto("/?view=video");
  const cards = page.locator("[data-media-id]");
  const firstId = await cards.nth(0).getAttribute("data-media-id");
  const secondId = await cards.nth(1).getAttribute("data-media-id");
  const secondTitle = (await cards.nth(1).locator(".card-title").textContent()).trim();
  const deferred = await deferDeepLinkEnrichment(page, firstId, { failAfterRelease: true });
  const firstMediaRequests = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/web/media/") && url.pathname.includes(`/${firstId}.`)) {
      firstMediaRequests.push(request.url());
    }
  });

  await page.goto(`/?view=video&item=${firstId}&t=5`);
  await deferred.started;
  await page.evaluate((itemId) => {
    history.pushState({}, "", `/?view=video&item=${itemId}`);
    window.dispatchEvent(new PopStateEvent("popstate"));
  }, secondId);
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  deferred.release();
  await deferred.failureAttempted;
  await page.waitForTimeout(100);

  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#player-message")).toBeHidden();
  await openAdvancedPlayback(page);
  await expect(page.locator("#audio-track-status")).toBeHidden();
  await expect(page.locator("#audio-track-retry")).toBeHidden();
  expect(firstMediaRequests).toEqual([]);
});

test("a card selection supersedes a linked title that is still enriching", async ({ page }) => {
  await page.goto("/?view=video");
  const cards = page.locator("[data-media-id]");
  const firstId = await cards.nth(0).getAttribute("data-media-id");
  const secondTitle = (await cards.nth(1).locator(".card-title").textContent()).trim();
  const deferred = await deferDeepLinkEnrichment(page, firstId);
  const firstMediaRequests = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/web/media/") && url.pathname.includes(`/${firstId}.`)) {
      firstMediaRequests.push(request.url());
    }
  });

  await page.goto(`/?view=video&item=${firstId}&t=5`);
  await deferred.started;
  await page.locator("[data-media-id]").nth(1).locator(".card-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  deferred.release();
  await page.waitForTimeout(100);

  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#player-message")).toBeHidden();
  expect(firstMediaRequests).toEqual([]);
});

test("missing catalog duration, artwork, and audio degrade deliberately", async ({ page, request }) => {
  await usePreference(page, "stream", "direct");
  const response = await request.get("/api/web/library?view=library&kind=video&q=&sort=title&offset=0&limit=24");
  expect(response.ok()).toBe(true);
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
  await page.route("**/api/web/library?**", (route) => route.fulfill({ json: payload }));
  await serveFixtureMedia(page);
  await page.goto("/?view=video");
  const card = page.locator(".media-card.video", { has: page.getByRole("button", { name: /^Play tagged\b/ }) });
  await expect(card).toBeVisible();
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
        body: JSON.stringify({ schema_version: 2, error: { code: "library_unavailable", message: "raw helper output", recoverable: true, action: "retry_library" } }),
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

test("a version-mismatched JSON error cannot masquerade as a current API error", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await serveFixtureMedia(page);
  await page.route("**/api/web/item/*", (route) => route.fulfill({
    status: 503,
    contentType: "application/json",
    body: JSON.stringify({
      schema_version: 1,
      error: {
        code: "media_missing",
        message: "stale schema claims the file is missing",
        recoverable: true,
        action: "return_to_library",
      },
    }),
  }));

  await openLibrary(page);
  await selectTaggedVideo(page);
  await page.locator("#video-player").dispatchEvent("error");

  await expect(page.locator("#player-message-text")).toContainText("cannot play the original file");
  await expect(page.locator("#player-message-text")).not.toContainText("no longer available");
  await expect(page.locator("#technical-message")).not.toContainText("stale schema claims");
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
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.locator("#playback-live")).toHaveText("Playback is ready. Press Play to begin.");
  await expect(page.locator("#player-stage")).toHaveClass(/awaiting-play/);
  await page.locator("#play-button").click();
  await expect(page.locator("#player-stage")).not.toHaveClass(/awaiting-play/);
  await expect(page.locator("#player-message")).toBeHidden();
});

test("forced Original and Compatible modes select the requested typed source", async ({ page }) => {
  const requests = [];
  await usePreference(page, "stream", "direct");
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.width = 3840;
      entry.height = 2160;
      entry.resolution = "3840×2160";
    }
    await route.fulfill({ response, json: payload });
  });
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
    { value: "uhd_high", label: "4K High · 25 Mbps" },
    { value: "uhd_optimized", label: "4K Optimized · 16 Mbps" },
    { value: "full_hd", label: "1080p · 8 Mbps" },
    { value: "data_saver", label: "720p · 3 Mbps" },
    { value: "sd_480", label: "480p · 1.5 Mbps" },
    { value: "low_360", label: "360p · 0.8 Mbps" },
  ]);
  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  await showPlayerControls(page);
  await page.locator("#quality-menu-button").click();
  await expect(page.locator("#quality-dialog")).toBeVisible();
  expect(await page.locator('#quality-choices input[name="quality-choice"]').evaluateAll((inputs) => inputs.map((input) => ({
    value: input.value,
    label: input.parentElement.textContent,
    disabled: input.disabled,
  })))).toEqual([
    { value: "auto", label: "Auto · up to 4K", disabled: false },
    { value: "uhd_high", label: "4K High · 25 Mbps", disabled: false },
    { value: "uhd_optimized", label: "4K Optimized · 16 Mbps", disabled: false },
    { value: "full_hd", label: "1080p · 8 Mbps", disabled: false },
    { value: "data_saver", label: "720p · 3 Mbps", disabled: false },
    { value: "sd_480", label: "480p · 1.5 Mbps", disabled: false },
    { value: "low_360", label: "360p · 0.8 Mbps", disabled: false },
  ]);
  await page.locator('#quality-choices input[value="low_360"]').check();
  await expect.poll(() => requests.some((url) => url.searchParams.get("quality") === "low_360")).toBe(true);
  await showPlayerControls(page);
  await page.locator("#quality-menu-button").click();
  await page.locator('#quality-choices input[value="uhd_high"]').check();
  await expect(page.locator("#quality-menu-button")).toHaveText("4K High");
  await expect(page.locator('input[name="stream-mode"][value="compat"]')).toBeChecked();
  await expect(page.locator("#mode-label")).toHaveText("Re-encoding video");
  await expect.poll(() => requests.some((url) => url.searchParams.get("mode") === "compatible")).toBe(true);
  const compatible = requests.findLast((url) => url.searchParams.get("mode") === "compatible");
  expect(compatible?.searchParams.get("reason")).toBe("forced_compatible");
  expect(compatible?.searchParams.get("quality")).toBe("uhd_high");
  expect(compatible?.searchParams.get("video_output")).toBe("h264_sdr");
  expect(compatible?.searchParams.get("request")).toMatch(/^\d+$/);
  await showPlayerControls(page);
  await page.locator("#quality-menu-button").click();
  await page.locator('#quality-choices input[value="full_hd"]').check();
  await expect.poll(() => requests.some((url) => url.searchParams.get("quality") === "full_hd")).toBe(true);
});

test("quality choices and requests stop at the active video's source resolution", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "uhd_high");
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));

  await openLibrary(page);
  await selectTaggedVideo(page);
  await openAdvancedPlayback(page);
  expect(await page.locator("#quality-control option").evaluateAll((options) => options.map((option) => ({
    value: option.value,
    label: option.textContent,
  })))).toEqual([
    { value: "auto", label: "Auto · up to 32×24" },
    { value: "low_360", label: "Source 32×24 · 0.8 Mbps" },
  ]);
  await expect(page.locator("#quality-control")).toHaveValue("low_360");
  await expect.poll(() => requests.findLast((url) => url.searchParams.get("mode") === "compatible")?.searchParams.get("quality"))
    .toBe("low_360");
  expect(await page.evaluate(() => localStorage.getItem("rustydlna.quality"))).toBe("uhd_high");

  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  await expect(page.locator("#output-stream-facts")).toContainText("no larger than 32×24 (never upscaled)");
});

test("an eligible measured SDR model exposes and labels only the bounded AI quality", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "full_hd");
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.ai_upscale = {
      label: "AI upscale",
      max_scale: 2,
      sdr_only: true,
      bit_depth: 8,
      profiles: [{
        name: "fsrcnnx-8",
        max_source_width: 1920,
        max_source_height: 1080,
        max_source_pixels_per_second: 70_000_000,
      }],
    };
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.width = 1280;
      entry.height = 720;
      entry.resolution = "1280×720";
      entry.frame_rate = "30000/1001";
      entry.hdr = "sdr";
      entry.bit_depth = 8;
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await openAdvancedPlayback(page);
  expect(await page.locator("#quality-control option").evaluateAll((options) => options.map((option) => ({
    value: option.value,
    label: option.textContent,
  })))).toEqual([
    { value: "auto", label: "Auto · up to 1280×720" },
    { value: "full_hd", label: "1080p · 8 Mbps · AI upscale" },
    { value: "data_saver", label: "720p · 3 Mbps" },
    { value: "sd_480", label: "480p · 1.5 Mbps" },
    { value: "low_360", label: "360p · 0.8 Mbps" },
  ]);
  await expect(page.locator("#quality-control")).toHaveValue("full_hd");
  await expect.poll(() => requests.findLast((url) => url.searchParams.get("mode") === "compatible")?.searchParams.get("quality"))
    .toBe("full_hd");
  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  await expect(page.locator("#output-stream-facts")).toContainText("1920×1080 (AI upscaled)");
});

test("desktop Chrome decodes AI-upscaled H.264 through Media Source fragments", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "Media Source delivery selection belongs to desktop Chromium");
  const fixture = await fragmentedCompatibleFixture({ profile: "high", level: "5.1" });
  const { initEnd } = fragmentedMp4Layout(fixture);
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "uhd_high");
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.ai_upscale = {
      label: "AI upscale",
      max_scale: 2,
      sdr_only: true,
      bit_depth: 8,
      profiles: [{
        name: "fsrcnnx-8",
        max_source_width: 1920,
        max_source_height: 1080,
        max_source_pixels_per_second: 70_000_000,
      }],
    };
    payload.capabilities.video_outputs = [
      ...(payload.capabilities.video_outputs || []).filter((output) => output.id !== "h264_sdr"),
      {
        id: "h264_sdr",
        label: "H.264 SDR",
        codec: "avc1.640033",
        video_content_type: 'video/mp4; codecs="avc1.640033"',
        mse_content_type: 'video/mp4; codecs="avc1.640033,mp4a.40.2"',
        dynamic_range: "sdr",
        bit_depth: 8,
      },
    ];
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.width = 1920;
      entry.height = 1080;
      entry.resolution = "1920×1080";
      entry.frame_rate = "24000/1001";
      entry.video_codec = "h264";
      entry.video_content_type = 'video/mp4; codecs="avc1.640028"';
      entry.codec_string = "avc1.640028,mp4a.40.2";
      entry.hdr = "sdr";
      entry.bit_depth = 8;
      entry.audio_codec = "aac";
      entry.audio_tracks = [{
        index: 0,
        codec: "aac",
        content_type: 'audio/mp4; codecs="mp4a.40.2"',
        channels: 2,
        default: true,
      }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  const startupEvents = [];
  await page.route("**/api/web/transcode/*", (route) => {
    const request = route.request();
    if (request.method() === "POST") {
      startupEvents.push(new URL(request.url()).searchParams.get("event"));
    }
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
    });
  });
  const requests = [];
  await page.route("**/web/media/*?**", async (route) => {
    const url = new URL(route.request().url());
    const delivery = url.searchParams.get("delivery");
    requests.push(url);
    if (delivery === "mse") {
      const initUrl = new URL(url);
      initUrl.pathname = initUrl.pathname.replace(/\.m3u8$/, ".mp4");
      initUrl.searchParams.set("delivery", "mse_init");
      initUrl.searchParams.set("hls_offset", "0");
      initUrl.searchParams.set("hls_length", String(initEnd));
      const segmentUrl = new URL(initUrl);
      segmentUrl.pathname = segmentUrl.pathname.replace(/\.mp4$/, ".m4s");
      segmentUrl.searchParams.set("delivery", "mse_segment");
      segmentUrl.searchParams.set("hls_offset", String(initEnd));
      segmentUrl.searchParams.set("hls_length", String(fixture.byteLength - initEnd));
      await route.fulfill({
        status: 200,
        contentType: "application/vnd.apple.mpegurl",
        body: [
          "#EXTM3U",
          "#EXT-X-VERSION:7",
          `#EXT-X-MAP:URI="${initUrl.pathname}?${initUrl.searchParams}"`,
          "#EXTINF:2.000000,",
          `${segmentUrl.pathname}?${segmentUrl.searchParams}`,
          "",
        ].join("\n"),
      });
      return;
    }
    const start = Number(url.searchParams.get("hls_offset"));
    const length = Number(url.searchParams.get("hls_length"));
    const body = fixture.subarray(start, start + length);
    await route.fulfill({
      status: 200,
      headers: {
        "Content-Length": String(body.byteLength),
        "Content-Type": delivery === "mse_init" ? "video/mp4" : "video/iso.segment",
      },
      body,
    });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.some((url) => url.searchParams.get("delivery") === "mse")).toBe(true);
  const playlistRequest = requests.find((url) => url.searchParams.get("delivery") === "mse");
  expect(playlistRequest.pathname).toMatch(/\.m3u8$/);
  expect(playlistRequest.searchParams.get("quality")).toBe("uhd_high");
  expect(playlistRequest.searchParams.get("video_mode")).toBe("transcode");
  expect(playlistRequest.searchParams.get("video_output")).toBe("h264_sdr");
  await expect.poll(() => requests.some((url) => url.searchParams.get("delivery") === "mse_init")).toBe(true);
  await expect.poll(() => requests.some((url) => url.searchParams.get("delivery") === "mse_segment")).toBe(true);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.videoWidth > 0)).toBe(true);
  await expect.poll(() => startupEvents).toEqual(expect.arrayContaining([
    "mse_playlist_received",
    "mse_init_fetched",
    "mse_init_appended",
    "mse_first_fragment_fetched",
    "mse_first_fragment_appended",
  ]));
  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#output-stream-facts")).toContainText("3840×2160 (AI upscaled)");
  await expect(page.locator("#output-stream-facts")).toContainText("Media Source · fragmented MP4");
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
  await page.locator("#video-player").evaluate((video) => video.pause());
});

test("Auto converts MPEG-4 Part 2 when the browser only claims broad MP4 support", async ({ page }) => {
  await usePreference(page, "stream", "auto");
  await page.addInitScript(() => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function broadMp4Support(contentType) {
      if (String(contentType) === "video/mp4") return "maybe";
      return canPlayType.call(this, contentType);
    };
  });
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "mpeg4";
      entry.codec_string = null;
      entry.video_content_type = null;
      entry.transcode_likely = true;
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);

  await expect.poll(() => requests.length).toBeGreaterThan(0);
  expect(requests.every((url) => url.searchParams.get("mode") !== "direct")).toBe(true);
  expect(requests[0].searchParams.get("mode")).toBe("compatible");
  expect(requests[0].searchParams.get("reason")).toBe("browser_support_uncertain");
  expect(requests[0].searchParams.get("video_mode")).toBe("transcode");
  expect(requests[0].searchParams.get("audio_mode")).toBe("copy");
  await expect(page.locator("#mode-label")).toHaveText("Re-encoding video");
  await expect(page.locator("#playback-mode")).toHaveAttribute("title", /Audio is copied unchanged/);
});

test("compatible canplay reports one generation-scoped startup timing event", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await serveFixtureMedia(page);
  const reports = [];
  page.on("request", (request) => {
    if (request.method() === "POST" && request.url().includes("/api/web/transcode/")) {
      const url = new URL(request.url());
      if (url.searchParams.get("event") === "canplay") reports.push(url);
    }
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => reports.length).toBe(1);
  expect(reports[0].searchParams.get("event")).toBe("canplay");
  expect(reports[0].searchParams.get("request")).toMatch(/^\d+$/);
  expect(reports[0].searchParams.get("session")).toMatch(/^\d+$/);

  await page.locator("#video-player").dispatchEvent("canplay");
  await page.locator("#video-player").dispatchEvent("canplay");
  await page.waitForTimeout(50);
  expect(reports).toHaveLength(1);
});

test("quality preferences follow advertised bounded opaque profile IDs", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    if (sessionStorage.getItem("quality-seeded")) return;
    sessionStorage.setItem("quality-seeded", "true");
    localStorage.setItem("rustydlna.quality", "future.4k-v2");
  });
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    const template = payload.capabilities.quality_profiles[0];
    payload.capabilities.quality_profiles.push({
      ...template,
      id: "future.4k-v2",
      label: "Future 4K",
    });
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.width = 3840;
      entry.height = 2160;
      entry.resolution = "3840×2160";
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await openAdvancedPlayback(page);
  await expect(page.locator("#quality-control")).toHaveValue("future.4k-v2");
  await expect(page.locator('#quality-control option[value="future.4k-v2"]')).toHaveText("Future 4K");
  await expect.poll(() => requests.findLast((url) => url.searchParams.get("mode") === "compatible")?.searchParams.get("quality"))
    .toBe("future.4k-v2");

  await page.evaluate(() => localStorage.setItem("rustydlna.quality", "removed-profile"));
  await page.reload();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
  await expect.poll(() => page.evaluate(() => localStorage.getItem("rustydlna.quality"))).toBe("auto");
});

for (const control of ["toolbar", "Media Session"]) {
  test(`Pause during deferred negotiation survives attaching the compatible source (${control})`, async ({ page }) => {
    await usePreference(page, "stream", "compat");
    await installDeferredDecodingInfo(page);
    await serveFixtureMedia(page);
    await page.addInitScript(() => {
      window.__playCalls = 0;
      HTMLMediaElement.prototype.play = () => { window.__playCalls += 1; return Promise.resolve(); };
      window.__mediaActions = {};
      Object.defineProperty(navigator, "mediaSession", {
        configurable: true,
        value: { setActionHandler: (action, handler) => { window.__mediaActions[action] = handler; } },
      });
    });
    await openLibrary(page);
    await selectTaggedVideo(page);
    await expect.poll(() => page.evaluate(() => window.__decodingRace.count())).toBeGreaterThan(0);
    if (control === "toolbar") {
      await showPlayerControls(page);
      await page.locator("#play-button").click();
    } else {
      await page.evaluate(() => {
        window.__mediaActions.play();
        window.__mediaActions.pause();
      });
    }
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    await page.evaluate(() => window.__decodingRace.release());
    await expect(page.locator("#video-player")).toHaveAttribute("src", /mode=compatible/);
    await page.locator("#video-player").dispatchEvent("canplay");
    expect(await page.evaluate(() => window.__playCalls)).toBe(0);
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  });
}

test("changing quality during negotiation preserves playing intent", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await installDeferredDecodingInfo(page);
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__decodingRace.count())).toBeGreaterThan(0);
  await page.locator("#quality-control").evaluate((control) => {
    control.value = [...control.options].find((option) => option.value !== "auto").value;
    control.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
  await page.evaluate(() => window.__decodingRace.release());
  const quality = await page.locator("#quality-control").inputValue();
  await expect.poll(async () => new URL(await page.locator("#video-player").evaluate((video) => video.src)).searchParams.get("quality"))
    .toBe(quality);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
});

test("a delayed startup status failure cannot interrupt playback that already started", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const fetchOriginal = window.fetch.bind(window);
    window.fetch = (input, options) => {
      if (String(input).includes("/api/web/transcode/") && !options?.method) {
        return new Promise((resolve, reject) => {
          window.__failStartupStatus = () => reject(new TypeError("connection lost"));
        });
      }
      return fetchOriginal(input, options);
    };
    const sources = new WeakMap();
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) { sources.set(this, value); },
    });
    HTMLMediaElement.prototype.load = () => {};
    HTMLMediaElement.prototype.play = () => Promise.resolve();
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => typeof window.__failStartupStatus)).toBe("function");
  await page.locator("#video-player").dispatchEvent("playing");
  await page.evaluate(() => window.__failStartupStatus());
  await page.waitForTimeout(0);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
  await expect(page.locator("#player-message")).toBeHidden();
});

test("Pause while a failed stream awaits producer status survives codec recovery", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const fetchOriginal = window.fetch.bind(window);
    const pending = [];
    let defer = false;
    const statusResponse = () => new Response(JSON.stringify({ schema_version: 2, state: "producing" }));
    window.fetch = (input, options) => {
      if (String(input).includes("/api/web/transcode/") && !options?.method) {
        return defer ? new Promise((resolve) => pending.push(resolve)) : Promise.resolve(statusResponse());
      }
      return fetchOriginal(input, options);
    };
    window.__recoveryStatus = {
      defer() { defer = true; },
      count() { return pending.length; },
      release() { defer = false; for (const resolve of pending.splice(0)) resolve(statusResponse()); },
    };
    const sources = new WeakMap();
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) { sources.set(this, value); },
    });
    HTMLMediaElement.prototype.load = () => {};
    HTMLMediaElement.prototype.play = () => Promise.resolve();
    HTMLMediaElement.prototype.canPlayType = (type) => String(type).includes("mpegurl") ? "" : "probably";
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  const video = page.locator("#video-player");
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("video_mode=copy");
  await video.evaluate((player) => {
    window.__recoveryStatus.defer();
    Object.defineProperty(player, "error", { configurable: true, value: { code: 3 } });
    player.dispatchEvent(new Event("error"));
  });
  await expect.poll(() => page.evaluate(() => window.__recoveryStatus.count())).toBeGreaterThan(0);
  await showPlayerControls(page);
  await page.locator("#play-button").click();
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await page.evaluate(() => window.__recoveryStatus.release());
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("video_mode=transcode");
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
});

test("a transcoding capability change invalidates deferred Compatible negotiation", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await installDeferredDecodingInfo(page);
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  let transcoding = true;
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.transcoding = transcoding;
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__decodingRace.count())).toBeGreaterThan(0);

  transcoding = false;
  await page.getByRole("tab", { name: "Folders" }).click();
  await expect(page.locator('input[name="stream-mode"][value="compat"]')).toBeDisabled();
  await page.evaluate(() => window.__decodingRace.release());

  await expect(page.locator("#player-message-text")).toContainText("disabled on this server");
  expect(requests).toHaveLength(0);
});

test("a removed selected quality restarts deferred negotiation before requesting media", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "full_hd");
  await installDeferredDecodingInfo(page);
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  let removeSelectedQuality = false;
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    if (removeSelectedQuality) {
      payload.capabilities.quality_profiles = payload.capabilities.quality_profiles
        .filter((profile) => profile.id !== "full_hd");
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__decodingRace.count())).toBeGreaterThan(0);

  removeSelectedQuality = true;
  await page.getByRole("tab", { name: "Folders" }).click();
  await expect.poll(() => page.evaluate(() => localStorage.getItem("rustydlna.quality"))).toBe("auto");
  await page.evaluate(() => window.__decodingRace.release());

  await expect.poll(() => requests.length).toBe(1);
  expect(requests[0].searchParams.get("quality")).toBe("auto");
  expect(requests[0].searchParams.get("video_mode")).toBe("copy");
});

test("a timed-out Media Capabilities probe is retried for a later selection", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    let recovered = false;
    let calls = 0;
    Object.defineProperty(HTMLMediaElement.prototype, "canPlayType", {
      configurable: true,
      value: () => "probably",
    });
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: {
        decodingInfo: () => {
          calls += 1;
          return recovered
            ? Promise.resolve({ supported: true, smooth: true, powerEfficient: true })
            : new Promise(() => {});
        },
      },
    });
    window.__capabilityProbeRetry = {
      calls: () => calls,
      recover: () => { recovered = true; },
    };
  });
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.length).toBe(1);
  const firstProbeCount = await page.evaluate(() => window.__capabilityProbeRetry.calls());
  expect(firstProbeCount).toBeGreaterThan(0);

  await page.evaluate(() => window.__capabilityProbeRetry.recover());
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__capabilityProbeRetry.calls()))
    .toBeGreaterThan(firstProbeCount);
  await expect.poll(() => requests.length).toBe(2);
});

test("empty advertised quality profiles reset to Auto while a missing legacy field preserves the choice", async ({ page }) => {
  await page.addInitScript(() => {
    if (sessionStorage.getItem("legacy-quality-seeded")) return;
    sessionStorage.setItem("legacy-quality-seeded", "true");
    localStorage.setItem("rustydlna.quality", "future.4k-v2");
  });
  let profileMode = "missing";
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    if (profileMode === "missing") delete payload.capabilities.quality_profiles;
    else payload.capabilities.quality_profiles = [];
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await expect.poll(() => page.evaluate(() => localStorage.getItem("rustydlna.quality"))).toBe("future.4k-v2");

  profileMode = "empty";
  await page.reload();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
  await expect.poll(() => page.evaluate(() => localStorage.getItem("rustydlna.quality"))).toBe("auto");
});

test("compatible startup status is not duplicated and stream info explains an audio-only transcode", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chromium", "portrait phones intentionally hide the stream-information control");
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
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
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "starting", retry_after_seconds: 1 }),
  }));
  await page.route("**/web/media/*.mp4?**", async (route) => {
    compatibleRequest = new URL(route.request().url());
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#stage-progress-label")).toHaveText("Starting prepared stream…");
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.getByText("Starting prepared stream…", { exact: true })).toHaveCount(1);
  await expect.poll(() => compatibleRequest?.searchParams.get("video_mode")).toBe("copy");
  expect(compatibleRequest.searchParams.get("audio_mode")).toBe("transcode");

  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#stream-info-dialog")).toBeVisible();
  await expect(page.locator("#source-stream-facts")).toContainText("HEVC");
  await expect(page.locator("#source-stream-facts")).toContainText("AC-3");
  await expect(page.locator("#stream-info-summary")).toContainText("video bitstream is copied unchanged");
  await expect(page.locator("#mode-label")).toHaveText("Converting audio");
  await expect(page.locator("#playback-mode")).toHaveAttribute("title", /video is copied unchanged/);
  await expect(page.locator("#output-stream-facts")).toContainText("no video re-encode");
  await expect(page.locator("#output-stream-facts")).toContainText("AAC");
  await expect(page.locator("#stream-diagnostic-facts")).toBeHidden();
  await page.getByText("Browser diagnostics", { exact: true }).click();
  await expect(page.locator("#stream-diagnostic-facts")).toBeVisible();
  await expect(page.locator("#stream-diagnostic-facts")).toContainText("canPlayType: probably");
  await expect(page.locator("#stream-diagnostic-facts")).toContainText("MediaCapabilities: supported");
});

test("desktop Chrome tries encoded HDR before SDR when copied-HEVC Media Source rejects its SourceBuffer", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "Media Source delivery selection belongs to desktop Chromium");
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
    Object.defineProperty(MediaSource, "isTypeSupported", {
      configurable: true,
      value: (contentType) => [
        'video/mp4; codecs="hvc1.2.4.L150.90,mp4a.40.2"',
        'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
      ].includes(String(contentType)),
    });
    const addSourceBuffer = MediaSource.prototype.addSourceBuffer;
    let copiedHevcAttempts = 0;
    const attemptedTypes = [];
    MediaSource.prototype.addSourceBuffer = function observedAddSourceBuffer(contentType) {
      attemptedTypes.push(String(contentType));
      if (String(contentType).includes("hvc1.2.4.L150.90")) copiedHevcAttempts += 1;
      return addSourceBuffer.call(this, contentType);
    };
    window.__copiedHevcSourceBufferAttempts = () => copiedHevcAttempts;
    window.__hevcSourceBufferTypes = () => [...attemptedTypes];
    const androidPlaybackMessages = new Set();
    window.__androidPlaybackMessages = () => [...androidPlaybackMessages];
    document.addEventListener("DOMContentLoaded", () => {
      const recordVisibleAndroidMessage = () => {
        for (const element of document.querySelectorAll("#stage-progress-label, #player-message")) {
          if (element.textContent.includes("Android")) androidPlaybackMessages.add(element.textContent);
        }
      };
      new MutationObserver(recordVisibleAndroidMessage).observe(document.body, {
        childList: true,
        subtree: true,
        characterData: true,
      });
      recordVisibleAndroidMessage();
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.video_outputs = [
      ...(payload.capabilities.video_outputs || []).filter((output) => output.id !== "hevc_hdr10"),
      {
        id: "hevc_hdr10",
        label: "HEVC Main 10 · HDR10",
        codec: "hvc1.2.4.L153.B0",
        video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
        mse_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
        dynamic_range: "hdr",
        bit_depth: 10,
        color_gamut: "rec2020",
        transfer_function: "pq",
      },
    ];
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "hevc";
      entry.codec_string = "hvc1.2.4.L150.90,ac-3";
      entry.video_content_type = 'video/mp4; codecs="hvc1.2.4.L150.90"';
      entry.video_profile = "Main 10";
      entry.video_level = 150;
      entry.pixel_format = "yuv420p10le";
      entry.bit_depth = 10;
      entry.hdr = "hdr10";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: 1 }),
  }));
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    await route.fulfill({
      status: 200,
      contentType: "video/mp4",
      body: fixture,
    });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__copiedHevcSourceBufferAttempts())).toBe(2);
  await expect.poll(() => page.evaluate(() => window.__hevcSourceBufferTypes())).toContain(
    'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
  );
  await expect.poll(() => page.evaluate(() => window.__androidPlaybackMessages())).toEqual([]);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.src)).toContain("video_mode=transcode");
  const recovered = new URL(await page.locator("#video-player").evaluate((video) => video.src));
  expect(recovered.searchParams.get("video_output")).toBe("h264_sdr");
  expect(recovered.searchParams.get("audio_mode")).toBe("transcode");
  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#output-stream-facts")).toContainText("H.264");
  await expect(page.locator("#output-stream-facts")).toContainText("AAC");
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("desktop Chrome sends an encoded 4K HDR rendition through Media Source", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "Media Source delivery selection belongs to desktop Chromium");
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "uhd_high");
  const cancellations = [];
  page.on("request", (request) => {
    if (request.method() === "DELETE" && request.url().includes("/api/web/transcode/")) {
      cancellations.push(new URL(request.url()));
    }
  });
  await page.addInitScript(() => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function hdrCanPlayType(contentType) {
      if (String(contentType).includes("hvc1.2.4.L153.B0")) return "probably";
      return canPlayType.call(this, contentType);
    };
    const matchMedia = globalThis.matchMedia.bind(globalThis);
    globalThis.matchMedia = (query) => String(query).includes("dynamic-range: high")
      ? { matches: true, media: query, addEventListener() {}, removeEventListener() {} }
      : matchMedia(query);
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: {
        decodingInfo: async (configuration) => ({
          supported: String(configuration?.video?.contentType || "").includes("hvc1.2.4.L153.B0"),
          smooth: true,
          powerEfficient: true,
        }),
      },
    });
    const hdrType = 'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"';
    Object.defineProperty(MediaSource, "isTypeSupported", {
      configurable: true,
      value: (contentType) => String(contentType) === hdrType,
    });
    const attemptedTypes = [];
    MediaSource.prototype.addSourceBuffer = function rejectTestHdrSourceBuffer(contentType) {
      attemptedTypes.push(String(contentType));
      throw new DOMException("Injected SourceBuffer rejection", "NotSupportedError");
    };
    window.__encodedHdrSourceBufferTypes = () => [...attemptedTypes];
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.video_outputs = [
      ...(payload.capabilities.video_outputs || []).filter((output) => output.id !== "hevc_hdr10"),
      {
        id: "hevc_hdr10",
        label: "HEVC Main 10 · HDR10",
        codec: "hvc1.2.4.L153.B0",
        video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
        mse_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
        dynamic_range: "hdr",
        bit_depth: 10,
        color_gamut: "rec2020",
        transfer_function: "pq",
      },
    ];
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.duration_seconds = 600;
      entry.duration = "0:10:00.000";
      entry.video_codec = "hevc";
      entry.hdr = "hdr10";
      entry.bit_depth = 10;
      entry.width = 3840;
      entry.height = 2160;
      entry.frame_rate = "24000/1001";
      entry.audio_codec = "aac";
      entry.audio_tracks = [{ index: 0, codec: "aac", content_type: 'audio/mp4; codecs="mp4a.40.2"', channels: 2, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
  }));
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", (route) => route.fulfill({
    status: 200,
    contentType: "video/mp4",
    body: fixture,
  }));

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__encodedHdrSourceBufferTypes())).toEqual([
    'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
    'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
  ]);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.src)).toContain("video_output=h264_sdr");
  const recovered = new URL(await page.locator("#video-player").evaluate((video) => video.src));
  expect(recovered.searchParams.get("quality")).toBe("uhd_high");
  // Reopening the same encoded-HDR plan must adopt its producer. Only the
  // subsequent plan change to H.264 cancels the previous generation.
  expect(cancellations).toHaveLength(1);
});

test("MPEG-4 Part 2 timing damage requests normal transcoding instead of unsupported repair", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function withoutAc3(contentType) {
      if (String(contentType).includes("ac-3")) return "";
      return canPlayType.call(this, contentType);
    };
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: { decodingInfo: async () => ({ supported: false }) },
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.container = "avi";
      entry.mime = "video/x-msvideo";
      entry.ext = "avi";
      entry.video_codec = "mpeg4";
      entry.video_profile = "Advanced Simple Profile";
      entry.video_content_type = null;
      entry.video_timestamp_mode = "broken-reordered";
      entry.video_repair_required = true;
      entry.repair_video_encoder = "h264_nvenc";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  let compatibleRequest = null;
  await serveFixtureMedia(page, (url) => { compatibleRequest = url; });

  await openLibrary(page);
  await selectTaggedVideo(page);
  expect(compatibleRequest?.searchParams.get("video_mode")).toBe("transcode");
  expect(compatibleRequest?.searchParams.get("audio_mode")).toBe("transcode");
});

test("a premature copied Compatible end resumes with portable codecs", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "the premature native end was observed in Chromium");
  await usePreference(page, "stream", "compat");
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
    HTMLMediaElement.prototype.canPlayType = (contentType) => {
      if (String(contentType).includes("ac-3")) return "";
      if (String(contentType).includes("hvc1.")) return "probably";
      return "probably";
    };
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: { decodingInfo: async (configuration) => ({ supported: Boolean(configuration.video) }) },
    });
    Object.defineProperty(MediaSource, "isTypeSupported", {
      configurable: true,
      value: () => false,
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.duration_seconds = 600;
      entry.duration = "0:10:00.000";
      entry.video_codec = "hevc";
      entry.codec_string = "hvc1.2.4.L150.90,ac-3";
      entry.video_content_type = 'video/mp4; codecs="hvc1.2.4.L150.90"';
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  const video = page.locator("#video-player");
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("video_mode=copy");
  await video.evaluate((player) => {
    Object.defineProperty(player, "currentTime", { configurable: true, value: 37 });
    player.dispatchEvent(new Event("ended"));
  });
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("video_mode=transcode");
  const recovered = new URL(await video.evaluate((player) => player.src));
  expect(recovered.searchParams.get("audio_mode")).toBe("transcode");
  expect(recovered.searchParams.get("start")).toBe("30");
  await expect(page.locator("#timeline")).toHaveValue("37");
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
});

test("a premature encoded HEVC end resumes at the same 4K quality with portable codecs", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "the premature native end was observed in desktop Chromium");
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "uhd_high");
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
    HTMLMediaElement.prototype.canPlayType = (contentType) => (
      String(contentType).includes("hvc1.2.4.L153.B0") ? "probably" : "probably"
    );
    const matchMedia = globalThis.matchMedia.bind(globalThis);
    globalThis.matchMedia = (query) => String(query).includes("dynamic-range: high")
      ? { matches: true, media: query, addEventListener() {}, removeEventListener() {} }
      : matchMedia(query);
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: { decodingInfo: async (configuration) => ({
        supported: Boolean(configuration.video),
        smooth: true,
        powerEfficient: true,
      }) },
    });
    Object.defineProperty(MediaSource, "isTypeSupported", {
      configurable: true,
      value: () => false,
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.video_outputs = [
      ...(payload.capabilities.video_outputs || []).filter((output) => output.id !== "hevc_hdr10"),
      {
        id: "hevc_hdr10",
        label: "HEVC Main 10 · HDR10",
        codec: "hvc1.2.4.L153.B0",
        video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
        mse_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
        dynamic_range: "hdr",
        bit_depth: 10,
        color_gamut: "rec2020",
        transfer_function: "pq",
      },
    ];
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.duration_seconds = 600;
      entry.duration = "0:10:00.000";
      entry.video_codec = "hevc";
      entry.hdr = "hdr10";
      entry.bit_depth = 10;
      entry.width = 3840;
      entry.height = 2160;
      entry.frame_rate = "24000/1001";
      entry.audio_codec = "aac";
      entry.audio_tracks = [{ index: 0, codec: "aac", content_type: 'audio/mp4; codecs="mp4a.40.2"', channels: 2, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  const video = page.locator("#video-player");
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("video_output=hevc_hdr10");
  await video.evaluate((player) => {
    Object.defineProperty(player, "currentTime", { configurable: true, value: 37 });
    player.dispatchEvent(new Event("ended"));
  });
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("video_output=h264_sdr");
  const recovered = new URL(await video.evaluate((player) => player.src));
  expect(recovered.searchParams.get("video_mode")).toBe("transcode");
  expect(recovered.searchParams.get("audio_mode")).toBe("transcode");
  expect(recovered.searchParams.get("quality")).toBe("uhd_high");
  expect(recovered.searchParams.get("start")).toBe("30");
  await expect(page.locator("#timeline")).toHaveValue("37");
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
});

test("malformed HEVC timing selects HDR-preserving frame-order repair", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chromium", "portrait phones intentionally hide the stream-information control");
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
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "starting", retry_after_seconds: 1 }),
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

test("an unsupported copied rendition retries portable codecs then a mobile-safe quality", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chromium", "portrait phones intentionally hide the stream-information control");
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
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
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: 1 }),
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
    Object.defineProperty(video, "error", { configurable: true, value: { code: 4 } });
    video.dispatchEvent(new Event("error"));
  });

  await expect.poll(() => requests.length).toBe(2);
  expect(requests[1].searchParams.get("video_mode")).toBe("transcode");
  expect(requests[1].searchParams.get("audio_mode")).toBe("transcode");
  expect(requests[1].searchParams.get("quality")).toBe("auto");
  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 4 } });
    video.dispatchEvent(new Event("error"));
  });

  await expect.poll(() => requests.length).toBe(3);
  expect(requests[2].searchParams.get("video_mode")).toBe("transcode");
  expect(requests[2].searchParams.get("audio_mode")).toBe("transcode");
  expect(requests[2].searchParams.get("quality")).toBe("low_360");
  await expect(page.locator("#quality-control")).toHaveValue("auto");
  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#stream-info-summary")).toContainText("H.264 video and AAC audio");
  await expect(page.locator("#output-stream-facts")).toContainText("Source 32×24 · 0.8 Mbps (automatic recovery)");
  await expect(page.locator("#output-stream-facts")).toContainText("no larger than 32×24 (never upscaled)");
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();

  await page.getByRole("button", { name: "Close stream information" }).click();
  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 4 } });
    video.dispatchEvent(new Event("error"));
  });
  await expect(page.locator("#player-message[role=alert]")).toBeVisible();
  await page.waitForTimeout(1_250);
  expect(requests).toHaveLength(3);
});

test("HDR transcode is capability gated and retries the same quality as SDR", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "exact desktop codec probing is covered in Chromium");
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "full_hd");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function hdrCanPlayType(contentType) {
      if (String(contentType).includes("hvc1.2.4.L153.B0")) return "probably";
      return canPlayType.call(this, contentType);
    };
    const matchMedia = globalThis.matchMedia.bind(globalThis);
    globalThis.matchMedia = (query) => String(query).includes("dynamic-range: high")
      ? { matches: true, media: query, addEventListener() {}, removeEventListener() {} }
      : matchMedia(query);
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: {
        decodingInfo: async (configuration) => ({
          supported: String(configuration?.video?.contentType || "").includes("hvc1.2.4.L153.B0"),
          smooth: true,
          powerEfficient: true,
        }),
      },
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.video_outputs = [
      ...(payload.capabilities.video_outputs || []),
      {
        id: "hevc_hdr10",
        label: "HEVC Main 10 · HDR10",
        codec: "hvc1.2.4.L153.B0",
        video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
        mse_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
        dynamic_range: "hdr",
        bit_depth: 10,
        color_gamut: "rec2020",
        transfer_function: "pq",
      },
    ];
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "hevc,other";
      entry.hdr = "dv-p7";
      entry.bit_depth = 10;
      entry.width = 3840;
      entry.height = 2160;
      entry.frame_rate = "24000/1001";
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: 1 }),
  }));
  const fixture = await readFile(compatibleFixture);
  const requests = [];
  await page.route("**/web/media/*.mp4?**", async (route) => {
    requests.push(new URL(route.request().url()));
    if (requests.length > 1) await new Promise((resolve) => setTimeout(resolve, 500));
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.length).toBe(1);
  expect(requests[0].searchParams.get("video_output")).toBe("hevc_hdr10");
  expect(requests[0].searchParams.get("quality")).toBe("full_hd");

  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 4 } });
    video.dispatchEvent(new Event("error"));
  });
  await expect.poll(() => requests.length).toBe(2);
  expect(requests[1].searchParams.get("video_output")).toBe("h264_sdr");
  expect(requests[1].searchParams.get("quality")).toBe("full_hd");
  await page.waitForTimeout(600);
  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 4 } });
    video.dispatchEvent(new Event("error"));
  });
  await expect(page.locator("#player-message[role=alert]")).toBeVisible();
  await page.waitForTimeout(750);
  expect(requests).toHaveLength(2);
});

test("encoding presets preserve position, intent, HDR and quality through seeks and recovery", async ({ page }) => {
  await installHevcHlsTrial(page, { hdr: true });
  await usePreference(page, "rate", "1.5");
  await openLibrary(page);
  await selectTaggedVideo(page);
  const source = () => page.evaluate(() => window.__hlsTrial.sources.at(-1));
  expect(new URL(await source()).searchParams.has("encoding_preset")).toBe(false);
  await showPlayerControls(page);
  await page.locator("#play-button").click();
  await page.locator("#video-player").evaluate((video) => {
    video.currentTime = 127;
    video.dispatchEvent(new Event("timeupdate"));
  });
  await openAdvancedPlayback(page);
  const control = page.getByRole("combobox", { name: "Encoding preset", exact: true });
  await expect(control).toHaveValue("balanced");
  const before = new URL(await source());
  for (const preset of ["fast_start", "maximum_speed"]) {
    await control.selectOption(preset);
    await expect.poll(async () => new URL(await source()).searchParams.get("encoding_preset")).toBe(preset);
    const current = new URL(await source());
    for (const key of ["quality", "video_output", "audio", "delivery"]) {
      expect(current.searchParams.get(key)).toBe(before.searchParams.get(key));
    }
    expect(current.searchParams.get("start")).toBe("120");
    await expect(page.locator("#timeline")).toHaveValue("127");
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    expect(await page.locator("#video-player").evaluate((video) => video.playbackRate)).toBe(1.5);
  }
  await page.locator("#quality-control").selectOption("full_hd");
  await expect.poll(async () => new URL(await source()).searchParams.get("quality")).toBe("full_hd");
  await page.getByRole("button", { name: "Close playback settings" }).click();
  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "247";
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect.poll(async () => new URL(await source()).searchParams.get("start")).toBe("240");
  expect(new URL(await source()).searchParams.get("encoding_preset")).toBe("maximum_speed");
  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 3 } });
    video.dispatchEvent(new Event("error"));
  });
  await expect.poll(async () => new URL(await source()).searchParams.get("video_output")).toBe("h264_sdr");
  expect(new URL(await source()).searchParams.get("encoding_preset")).toBe("maximum_speed");
  await page.locator("#stream-info-button").evaluate((button) => button.click());
  await expect(page.locator("#output-stream-facts")).toContainText("Maximum speed");
  await page.goto("/");
  await selectTaggedVideo(page);
  await page.locator("#start-over-button").click();
  await openAdvancedPlayback(page);
  await expect(control).toHaveValue("maximum_speed");
});

for (const mode of ["direct", "copy"]) {
  test(`encoding presets do not restart ${mode} video`, async ({ page }) => {
    await installHevcHlsTrial(page);
    if (mode === "direct") await usePreference(page, "stream", "direct");
    else await usePreference(page, "hevcHlsCopy", "true");
    await openLibrary(page);
    await selectTaggedVideo(page);
    const before = await page.evaluate(() => [...window.__hlsTrial.sources]);
    await openAdvancedPlayback(page);
    await page.getByRole("combobox", { name: "Encoding preset", exact: true }).selectOption("fast_start");
    expect(await page.evaluate(() => window.__hlsTrial.sources)).toEqual(before);
    expect(await page.evaluate(() => localStorage.getItem("rustydlna.encodingPreset"))).toBe("fast_start");
    await page.getByRole("button", { name: "Close playback settings" }).click();
    if (mode === "copy") {
      await page.locator("#timeline").evaluate((timeline) => {
        timeline.value = "127";
        timeline.dispatchEvent(new Event("input", { bubbles: true }));
        timeline.dispatchEvent(new Event("change", { bubbles: true }));
      });
      await expect.poll(() => page.evaluate(() => window.__hlsTrial.sources.length)).toBe(before.length + 1);
      const url = new URL(await page.evaluate(() => window.__hlsTrial.sources.at(-1)));
      expect(url.searchParams.get("video_mode")).toBe("copy");
      expect(url.searchParams.has("encoding_preset")).toBe(false);
    }
  });
}

test("an older server hides encoding presets and retains Balanced requests", async ({ page }) => {
  await installHevcHlsTrial(page);
  await usePreference(page, "encodingPreset", "maximum_speed");
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    delete payload.capabilities.encoding_presets;
    await route.fulfill({ response, json: payload });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await openAdvancedPlayback(page);
  await expect(page.locator("#encoding-preset-option")).toBeHidden();
  expect(new URL(await page.evaluate(() => window.__hlsTrial.sources.at(-1))).searchParams.has("encoding_preset")).toBe(false);
});

for (const appleMobile of [false, true]) {
  test(`HEVC HLS toggle preserves intent, position, and source quality on ${appleMobile ? "iPad" : "desktop Safari"}`, async ({ page }) => {
    await installHevcHlsTrial(page, { appleMobile });
    const errors = await openLibrary(page);
    await selectTaggedVideo(page);
    const source = () => page.evaluate(() => window.__hlsTrial.sources.at(-1));
    expect(new URL(await source()).searchParams.get("video_mode")).toBe("transcode");
    await showPlayerControls(page);
    await page.locator("#play-button").click();
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    await page.locator("#video-player").evaluate((video) => {
      video.currentTime = 127;
      video.dispatchEvent(new Event("timeupdate"));
    });
    const playCalls = await page.evaluate(() => window.__hlsTrial.playCalls);
    await openAdvancedPlayback(page);
    const toggle = page.getByRole("checkbox", { name: "Try original HEVC with HLS" });
    await expect(toggle).not.toBeChecked();
    await toggle.check();
    await expect.poll(async () => new URL(await source()).searchParams.get("video_mode")).toBe("copy");
    const copied = new URL(await source());
    expect(copied.pathname).toMatch(/\.m3u8$/);
    expect(Object.fromEntries(["delivery", "video_mode", "audio_mode", "quality", "start"]
      .map((key) => [key, copied.searchParams.get(key)]))).toEqual({
      delivery: "hls", video_mode: "copy", audio_mode: "transcode", quality: "auto", start: "120",
    });
    expect(copied.searchParams.has("video_output")).toBe(false);
    await expect(page.locator("#timeline")).toHaveValue("127");
    expect(await page.evaluate(() => window.__hlsTrial.playCalls)).toBe(playCalls);
    expect(await page.evaluate(() => localStorage.getItem("rustydlna.hevcHlsCopy"))).toBe("true");
    await page.getByRole("button", { name: "Close playback settings" }).click();
    await page.locator("#timeline").evaluate((timeline) => {
      timeline.value = "247";
      timeline.dispatchEvent(new Event("input", { bubbles: true }));
      timeline.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await expect.poll(async () => new URL(await source()).searchParams.get("start")).toBe("240");
    expect(new URL(await source()).searchParams.get("quality")).toBe("auto");
    expect(new URL(await source()).searchParams.get("video_mode")).toBe("copy");
    await page.locator("#stream-info-button").evaluate((button) => button.click());
    await expect(page.locator("#output-stream-facts")).toContainText("copied unchanged");
    await expect(page.locator("#output-stream-facts")).toContainText("Native HLS");
    await page.getByRole("button", { name: "Close stream information" }).click();
    await openAdvancedPlayback(page);
    await toggle.uncheck();
    await expect.poll(async () => new URL(await source()).searchParams.get("video_mode")).toBe("transcode");
    await expect(page.locator("#timeline")).toHaveValue("247");
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    await toggle.check();
    await page.locator("#quality-control").selectOption("full_hd");
    await expect.poll(async () => new URL(await source()).searchParams.get("quality")).toBe("full_hd");
    expect(new URL(await source()).searchParams.get("video_mode")).toBe("transcode");
    await page.goto("/");
    await selectTaggedVideo(page);
    await page.locator("#start-over-button").click();
    await openAdvancedPlayback(page);
    await expect(toggle).toBeChecked();
    expect(new URL(await source()).searchParams.get("quality")).toBe("full_hd");
    expect(errors).toEqual([]);
  });
}

for (const failure of ["decode", "startup", "producer"]) {
  test(`HEVC HLS trial falls back once on ${failure} failure and keeps the recovered seek plan`, async ({ page }) => {
    await installHevcHlsTrial(page, { appleMobile: true, hdr: failure === "decode" });
    await usePreference(page, "hevcHlsCopy", "true");
    await page.clock.install();
    await openLibrary(page);
    await selectTaggedVideo(page);
    const source = () => page.evaluate(() => window.__hlsTrial.sources.at(-1));
    expect(new URL(await source()).searchParams.get("video_mode")).toBe("copy");
    await showPlayerControls(page);
    await page.locator("#play-button").click();
    await page.locator("#video-player").evaluate((video) => {
      video.currentTime = 127;
      video.dispatchEvent(new Event("timeupdate"));
    });
    if (failure === "decode") {
      await page.locator("#video-player").evaluate((video) => {
        Object.defineProperty(video, "error", { configurable: true, value: { code: 3 } });
        video.dispatchEvent(new Event("error"));
      });
    } else if (failure === "startup") {
      await expect(page.locator("#stage-progress-label")).toHaveText("Preparing video…");
      await page.clock.runFor(12_500);
    } else {
      await page.route("**/api/web/transcode/*", (route) => route.fulfill({ json: { schema_version: 2, state: "failed" } }));
      await page.clock.runFor(600);
    }
    await expect.poll(async () => new URL(await source()).searchParams.get("video_mode")).toBe("transcode");
    expect(new URL(await source()).searchParams.get("delivery")).toBe("hls");
    expect(new URL(await source()).searchParams.get("video_output")).toBe(failure === "decode" ? "hevc_hdr10" : "h264_sdr");
    expect(new URL(await source()).searchParams.get("start")).toBe("120");
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    expect(await page.evaluate(() => window.__hlsTrial.sources.length)).toBe(2);
    // A healthy recovered producer must not cause another HEVC-copy trial.
    await page.route("**/api/web/transcode/*", (route) => route.fulfill({ json: { schema_version: 2, state: "producing" } }));
    await page.locator("#timeline").evaluate((timeline) => {
      timeline.value = "247";
      timeline.dispatchEvent(new Event("input", { bubbles: true }));
      timeline.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await page.clock.runFor(450);
    await expect.poll(async () => new URL(await source()).searchParams.get("start")).toBe("240");
    expect(new URL(await source()).searchParams.get("video_mode")).toBe("transcode");
    expect(await page.evaluate(() => localStorage.getItem("rustydlna.hevcHlsCopy"))).toBe("true");
    if (failure === "decode") {
      await page.locator("#video-player").dispatchEvent("error");
      await expect.poll(async () => new URL(await source()).searchParams.get("video_output")).toBe("h264_sdr");
      expect(new URL(await source()).searchParams.get("video_mode")).toBe("transcode");
    }
  });
}

test("HEVC HLS toggle leaves Original playback untouched", async ({ page }) => {
  await installHevcHlsTrial(page);
  await usePreference(page, "stream", "direct");
  await openLibrary(page);
  await selectTaggedVideo(page);
  const source = await page.evaluate(() => window.__hlsTrial.sources.at(-1));
  await openAdvancedPlayback(page);
  await page.getByRole("checkbox", { name: "Try original HEVC with HLS" }).check();
  expect(await page.evaluate(() => window.__hlsTrial.sources)).toEqual([source]);
  await expect(page.locator("#mode-label")).toHaveText("Original file");
});

test("desktop Safari keeps Auto quality for native HLS", async ({ page, browserName }) => {
  test.skip(browserName !== "webkit", "native HLS is a WebKit compatibility path");
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "auto");
  await page.addInitScript(() => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function desktopSafariCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "maybe";
      return canPlayType.call(this, contentType);
    };
  });
  await page.route("**/web/media/*?**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.endsWith(".m3u8")) {
      await route.fulfill({
        status: 200,
        contentType: "application/vnd.apple.mpegurl",
        body: "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-ENDLIST\n",
      });
      return;
    }
    await route.abort();
  });
  const mediaRequest = page.waitForRequest((request) => /\/web\/media\/\d+\.m3u8\?/.test(request.url()));

  await openLibrary(page);
  await selectTaggedVideo(page);
  const url = new URL((await mediaRequest).url());

  expect(url.searchParams.get("delivery")).toBe("hls");
  expect(url.searchParams.get("quality")).toBe("auto");
});

test("iPad WebKit selects native HLS compatible delivery", async ({ page, browserName }) => {
  test.skip(browserName !== "webkit", "native HLS is an Apple mobile compatibility path");
  const fixture = await fragmentedCompatibleFixture();
  const { initEnd } = fragmentedMp4Layout(fixture);
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1",
    });
    Object.defineProperty(navigator, "platform", { configurable: true, value: "MacIntel" });
    Object.defineProperty(navigator, "maxTouchPoints", { configurable: true, value: 5 });
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function appleMobileCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "maybe";
      if (String(contentType).includes("hvc1")) return "";
      return canPlayType.call(this, contentType);
    };
    const matchMedia = globalThis.matchMedia.bind(globalThis);
    globalThis.matchMedia = (query) => {
      if (String(query).includes("dynamic-range: high")) return { matches: false, media: query };
      if (String(query).includes("dynamic-range: standard")) return { matches: true, media: query };
      return matchMedia(query);
    };
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.video_outputs = [
      ...(payload.capabilities.video_outputs || []),
      {
        id: "hevc_hdr10",
        label: "HEVC Main 10 · HDR10",
        codec: "hvc1.2.4.L153.B0",
        video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
        mse_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0,mp4a.40.2"',
        dynamic_range: "hdr",
        bit_depth: 10,
        color_gamut: "rec2020",
        transfer_function: "pq",
      },
    ];
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "hevc";
      entry.hdr = "hdr10";
      entry.bit_depth = 10;
    }
    await route.fulfill({ response, json: payload });
  });
  const requests = [];
  await page.route("**/web/media/*?**", async (route) => {
    const url = new URL(route.request().url());
    const delivery = url.searchParams.get("delivery");
    if (delivery === "hls") {
      const initUrl = new URL(url);
      initUrl.pathname = initUrl.pathname.replace(/\.m3u8$/, ".mp4");
      initUrl.searchParams.set("delivery", "hls_init");
      initUrl.searchParams.set("hls_offset", "0");
      initUrl.searchParams.set("hls_length", String(initEnd));
      const segmentUrl = new URL(initUrl);
      segmentUrl.pathname = segmentUrl.pathname.replace(/\.mp4$/, ".m4s");
      segmentUrl.searchParams.set("delivery", "hls_segment");
      segmentUrl.searchParams.set("hls_offset", String(initEnd));
      segmentUrl.searchParams.set("hls_length", String(fixture.byteLength - initEnd));
      const playlist = [
        "#EXTM3U",
        "#EXT-X-VERSION:7",
        "#EXT-X-TARGETDURATION:3",
        "#EXT-X-MEDIA-SEQUENCE:0",
        "#EXT-X-PLAYLIST-TYPE:VOD",
        "#EXT-X-INDEPENDENT-SEGMENTS",
        `#EXT-X-MAP:URI="${initUrl.pathname}?${initUrl.searchParams}"`,
        "#EXTINF:2.000000,",
        `${segmentUrl.pathname}?${segmentUrl.searchParams}`,
        "#EXT-X-ENDLIST",
        "",
      ].join("\n");
      await route.fulfill({ status: 200, contentType: "application/vnd.apple.mpegurl", body: playlist });
      return;
    }
    if (!["hls_init", "hls_segment"].includes(delivery)) {
      await route.fulfill({ status: 400, body: "invalid delivery" });
      return;
    }
    const start = Number(url.searchParams.get("hls_offset"));
    const length = Number(url.searchParams.get("hls_length"));
    const body = fixture.subarray(start, start + length);
    await route.fulfill({
      status: 200,
      headers: {
        "Accept-Ranges": "bytes",
        "Content-Length": String(body.byteLength),
        "Content-Type": delivery === "hls_init" ? "video/mp4" : "video/iso.segment",
      },
      body,
    });
  });
  page.on("request", (request) => {
    if (/\/web\/media\/\d+\.(?:mp4|m3u8|m4s)\?/.test(request.url())) {
      requests.push({
        url: new URL(request.url()),
        range: request.headers().range || null,
        resourceType: request.resourceType(),
      });
    }
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.length).toBeGreaterThan(0);
  expect(requests[0].range).toBeNull();
  expect(requests[0].url.pathname).toMatch(/\.m3u8$/);
  expect(requests[0].url.searchParams.get("delivery")).toBe("hls");
  expect(requests[0].url.searchParams.get("quality")).toBe("data_saver");
  expect(requests[0].url.searchParams.get("video_mode")).toBe("transcode");
  expect(requests[0].url.searchParams.get("video_output")).toBe("hevc_hdr10");
  expect(requests[0].url.searchParams.get("audio_mode")).toBe("transcode");
  expect(await page.locator("#video-player").evaluate((video) => video.disableRemotePlayback)).toBe(false);
  await showPlayerControls(page);
  await page.locator("#stream-info-button").click();
  await expect(page.locator("#output-stream-facts")).toContainText("Native HLS · fragmented MP4");
  await expect(page.locator("#output-stream-facts")).toContainText("Source 32×24 · 0.8 Mbps (automatic recovery)");
});

test("iPad WebKit reattaches a resumed native HLS source that decodes no data", async ({ page, browserName }) => {
  test.skip(browserName !== "webkit", "native HLS recovery belongs to Apple WebKit");
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1",
    });
    Object.defineProperty(navigator, "platform", { configurable: true, value: "MacIntel" });
    Object.defineProperty(navigator, "maxTouchPoints", { configurable: true, value: 5 });
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    const nativeLoad = HTMLMediaElement.prototype.load;
    const nativeReadyState = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, "readyState");
    const nativeGetAttribute = Element.prototype.getAttribute;
    const nativeRemoveAttribute = Element.prototype.removeAttribute;
    const nativeSetTimeout = window.setTimeout.bind(window);
    const sources = new WeakMap();
    window.__nativeHlsLoads = [];
    window.__nativeHlsCanDecode = false;
    HTMLMediaElement.prototype.canPlayType = function nativeHlsCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "maybe";
      return canPlayType.call(this, contentType);
    };
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) { sources.set(this, String(value)); },
    });
    HTMLMediaElement.prototype.getAttribute = function mediaAttribute(name) {
      if (String(name).toLowerCase() === "src") return sources.get(this) || null;
      return nativeGetAttribute.call(this, name);
    };
    HTMLMediaElement.prototype.removeAttribute = function removeMediaAttribute(name) {
      if (String(name).toLowerCase() === "src") sources.delete(this);
      else nativeRemoveAttribute.call(this, name);
    };
    Object.defineProperty(HTMLMediaElement.prototype, "readyState", {
      configurable: true,
      get() {
        const source = this.getAttribute("src") || "";
        if (source.includes("delivery=hls") && !window.__nativeHlsCanDecode) return 0;
        return nativeReadyState.get.call(this);
      },
    });
    window.setTimeout = (callback, delay = 0, ...args) => nativeSetTimeout(
      callback,
      delay === 12_000 ? 25 : delay,
      ...args,
    );
    HTMLMediaElement.prototype.load = function controlledNativeHlsLoad() {
      const source = this.getAttribute("src") || "";
      if (!source.includes("delivery=hls")) return nativeLoad.call(this);
      window.__nativeHlsLoads.push(source);
      this.dispatchEvent(new Event("loadstart"));
      if (window.__nativeHlsLoads.length > 1) {
        window.__nativeHlsCanDecode = true;
        nativeSetTimeout(() => {
          this.dispatchEvent(new Event("loadedmetadata"));
          this.dispatchEvent(new Event("loadeddata"));
          this.dispatchEvent(new Event("canplay"));
        }, 0);
      }
    };
    HTMLMediaElement.prototype.play = function playRecoveredNativeHls() {
      this.dispatchEvent(new Event("play"));
      if (window.__nativeHlsCanDecode) this.dispatchEvent(new Event("playing"));
      return Promise.resolve();
    };
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
  }));
  await openLibrary(page);
  await selectTaggedVideo(page);

  await expect.poll(() => page.evaluate(() => window.__nativeHlsLoads.length)).toBe(2);
  const loads = await page.evaluate(() => window.__nativeHlsLoads);
  expect(new Set(loads).size).toBe(1);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("iPad sleep restarts native HLS at the saved position on Play", async ({ page, browserName }) => {
  test.skip(browserName !== "webkit", "native HLS suspension recovery belongs to Apple WebKit");
  await usePreference(page, "stream", "compat");
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media" && entry.file_name === "tagged.mp4") {
        entry.duration = 7_200;
        entry.duration_seconds = 7_200;
      }
    }
    await route.fulfill({ response, json: payload });
  });
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1",
    });
    Object.defineProperty(navigator, "platform", { configurable: true, value: "MacIntel" });
    Object.defineProperty(navigator, "maxTouchPoints", { configurable: true, value: 5 });
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    const nativeGetAttribute = Element.prototype.getAttribute;
    const nativeRemoveAttribute = Element.prototype.removeAttribute;
    const sources = new WeakMap();
    const paused = new WeakMap();
    const positions = new WeakMap();
    let visibilityState = "visible";
    window.__nativeHlsSleep = {
      events: [],
      loads: [],
      sleepAt(player, currentTime) {
        positions.set(player, currentTime);
        player.dispatchEvent(new Event("timeupdate"));
        visibilityState = "hidden";
        document.dispatchEvent(new Event("visibilitychange"));
        paused.set(player, true);
        player.dispatchEvent(new Event("pause"));
        visibilityState = "visible";
        document.dispatchEvent(new Event("visibilitychange"));
      },
    };
    Object.defineProperty(Document.prototype, "visibilityState", {
      configurable: true,
      get: () => visibilityState,
    });
    Object.defineProperty(HTMLMediaElement.prototype, "paused", {
      configurable: true,
      get() { return paused.get(this) ?? true; },
    });
    Object.defineProperty(HTMLMediaElement.prototype, "currentTime", {
      configurable: true,
      get() { return positions.get(this) ?? 0; },
      set(value) { positions.set(this, Number(value)); },
    });
    Object.defineProperty(HTMLMediaElement.prototype, "duration", {
      configurable: true,
      get: () => 7_200,
    });
    Object.defineProperty(HTMLMediaElement.prototype, "readyState", {
      configurable: true,
      get: () => 4,
    });
    HTMLMediaElement.prototype.canPlayType = function nativeHlsCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "maybe";
      return canPlayType.call(this, contentType);
    };
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) { sources.set(this, String(value)); },
    });
    HTMLMediaElement.prototype.getAttribute = function mediaAttribute(name) {
      if (String(name).toLowerCase() === "src") return sources.get(this) || null;
      return nativeGetAttribute.call(this, name);
    };
    HTMLMediaElement.prototype.removeAttribute = function removeMediaAttribute(name) {
      if (String(name).toLowerCase() === "src") sources.delete(this);
      else nativeRemoveAttribute.call(this, name);
    };
    HTMLMediaElement.prototype.pause = function pauseNativeHls() {
      paused.set(this, true);
      this.dispatchEvent(new Event("pause"));
    };
    HTMLMediaElement.prototype.load = function loadNativeHls() {
      const source = this.getAttribute("src") || "";
      if (!source.includes("delivery=hls")) return;
      paused.set(this, true);
      window.__nativeHlsSleep.loads.push(source);
      window.__nativeHlsSleep.events.push("load");
      this.dispatchEvent(new Event("loadstart"));
      window.setTimeout(() => {
        this.dispatchEvent(new Event("loadedmetadata"));
        this.dispatchEvent(new Event("loadeddata"));
        window.__nativeHlsSleep.events.push("canplay");
        this.dispatchEvent(new Event("canplay"));
      }, 0);
    };
    HTMLMediaElement.prototype.play = function playNativeHls() {
      paused.set(this, false);
      window.__nativeHlsSleep.events.push("play");
      this.dispatchEvent(new Event("play"));
      window.setTimeout(() => {
        window.__nativeHlsSleep.events.push("playing");
        this.dispatchEvent(new Event("playing"));
      }, 0);
      return Promise.resolve();
    };
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
  }));
  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__nativeHlsSleep.loads.length)).toBe(1);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  const startupEvents = await page.evaluate(() => window.__nativeHlsSleep.events);
  expect(startupEvents.indexOf("play")).toBeLessThan(startupEvents.indexOf("canplay"));
  await page.locator("#video-player").dispatchEvent("canplay");
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);

  await page.locator("#video-player").evaluate((player) => window.__nativeHlsSleep.sleepAt(player, 4_270));
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await page.locator("#play-button").evaluate((button) => button.click());

  await expect.poll(() => page.evaluate(() => window.__nativeHlsSleep.loads.length)).toBe(2);
  const loads = await page.evaluate(() => window.__nativeHlsSleep.loads.map((source) => (
    Object.fromEntries(new URL(source, document.baseURI).searchParams)
  )));
  expect(loads.map((params) => params.start)).toEqual(["0", "4270"]);
  expect(loads[1].request).not.toBe(loads[0].request);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);

  await page.locator("#video-player").evaluate((player) => {
    window.__nativeHlsSleep.sleepAt(player, 80);
    return player.play();
  });
  await expect.poll(() => page.evaluate(() => window.__nativeHlsSleep.loads.length)).toBe(3);
  const nativeResume = await page.evaluate(() => Object.fromEntries(
    new URL(window.__nativeHlsSleep.loads[2], document.baseURI).searchParams,
  ));
  expect(nativeResume.start).toBe("4350");
  expect(nativeResume.request).not.toBe(loads[1].request);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("desktop Safari selects server-generated native HLS", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "webkit", "native HLS selection belongs to WebKit");
  test.setTimeout(45_000);
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "data_saver");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15",
    });
    Object.defineProperty(navigator, "platform", { configurable: true, value: "MacIntel" });
    Object.defineProperty(navigator, "maxTouchPoints", { configurable: true, value: 0 });
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function safariCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "maybe";
      return canPlayType.call(this, contentType);
    };
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.currentSrc), {
    timeout: 30_000,
  }).toContain("delivery=hls");
  const sourceUrl = await page.locator("#video-player").evaluate((video) => video.currentSrc);
  const source = new URL(sourceUrl);
  expect(source.pathname).toMatch(/\.m3u8$/);
  expect(source.searchParams.get("video_mode")).toBe("transcode");
  expect(source.searchParams.get("audio_mode")).toBe("transcode");
  expect(source.searchParams.get("quality")).toBe("low_360");
});

test("desktop Safari Auto lowers quality when a supported 4K HEVC original never starts", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "webkit", "4K HEVC startup recovery belongs to desktop Safari");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15",
    });
    Object.defineProperty(navigator, "platform", { configurable: true, value: "MacIntel" });
    Object.defineProperty(navigator, "maxTouchPoints", { configurable: true, value: 0 });
    const sources = new WeakMap();
    window.__safariStalledSources = [];
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) {
        const source = new URL(value, document.baseURI).href;
        sources.set(this, source);
        if (this instanceof HTMLVideoElement) window.__safariStalledSources.push(source);
      },
    });
    HTMLMediaElement.prototype.load = () => {};
    HTMLMediaElement.prototype.pause = () => {};
    HTMLMediaElement.prototype.play = () => Promise.resolve();
    HTMLMediaElement.prototype.canPlayType = function safariHevcCanPlayType(contentType) {
      const value = String(contentType);
      if (value.includes("mpegurl")) return "maybe";
      if (value.includes("hvc1.")) return "probably";
      if (value.includes("mp4a.40.2")) return "probably";
      return "";
    };
    const nativeSetTimeout = window.setTimeout.bind(window);
    let acceleratedOriginalStall = false;
    window.setTimeout = (callback, delay = 0, ...args) => {
      const accelerated = delay === 12_000 && !acceleratedOriginalStall;
      if (accelerated) acceleratedOriginalStall = true;
      return nativeSetTimeout(callback, accelerated ? 40 : delay, ...args);
    };
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media") continue;
      entry.mime = "video/mp4";
      entry.codec_string = "hvc1.2.4.H153.90,mp4a.40.2";
      entry.video_content_type = 'video/mp4; codecs="hvc1.2.4.H153.90"';
      entry.video_codec = "hevc";
      entry.width = 3840;
      entry.height = 2160;
      entry.resolution = "3840×2160";
      entry.bit_depth = 10;
      entry.hdr = "hdr10";
      entry.transcode_likely = false;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
  }));

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.evaluate(() => window.__safariStalledSources.length))
    .toBeGreaterThanOrEqual(2);
  const sources = await page.evaluate(() => window.__safariStalledSources);
  const original = sources.map((source) => new URL(source))
    .find((source) => source.searchParams.get("reason") === "browser_supported");
  const compatible = sources.map((source) => new URL(source))
    .find((source) => source.searchParams.get("mode") === "compatible");
  expect(original).toBeTruthy();
  expect(compatible).toBeTruthy();
  expect(compatible.searchParams.get("delivery")).toBe("hls");
  expect(compatible.searchParams.get("video_mode")).toBe("transcode");
  expect(compatible.searchParams.get("audio_mode")).toBe("transcode");
  expect(compatible.searchParams.get("quality")).toBe("data_saver");
});

test("Android Chrome decodes server-generated Media Source output", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "mobile-chromium", "real Media Source decoding belongs to Android Chrome");
  test.setTimeout(45_000);
  await usePreference(page, "stream", "compat");

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.src), {
    timeout: 30_000,
  }).toMatch(/^blob:/);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.readyState), {
    timeout: 30_000,
  }).toBeGreaterThanOrEqual(2);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.videoWidth), {
    timeout: 30_000,
  }).toBeGreaterThan(0);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.currentTime), {
    timeout: 30_000,
  }).toBeGreaterThan(0);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("Android sends required video transcodes directly through Media Source", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Android delivery selection belongs to mobile Chromium");
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36",
    });
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function androidCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "maybe";
      if (String(contentType).includes("ac-3")) return "";
      return canPlayType.call(this, contentType);
    };
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "other";
      entry.video_profile = "Advanced";
      entry.video_content_type = null;
      entry.codec_string = "other,ac-3";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "starting", retry_after_seconds: 1 }),
  }));
  const requests = [];
  await page.route("**/web/media/*?**", async (route) => {
    requests.push(new URL(route.request().url()));
    await route.fulfill({
      status: 200,
      contentType: "application/vnd.apple.mpegurl",
      body: "#EXTM3U\n",
    });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.length).toBeGreaterThan(0);
  expect(requests[0].pathname).toMatch(/\.m3u8$/);
  expect(requests[0].searchParams.get("delivery")).toBe("mse");
  expect(requests[0].searchParams.get("quality")).toBe("data_saver");
  expect(requests[0].searchParams.get("video_mode")).toBe("transcode");
  expect(requests[0].searchParams.get("audio_mode")).toBe("transcode");
  await expect(page.locator("#output-stream-facts")).toContainText("Media Source · fragmented MP4");
  await expect(page.locator("#output-stream-facts")).toContainText("720p · 3 Mbps (automatic recovery)");
});

test("Android decodes required video transcodes through Media Source fragments", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Media Source fallback belongs to Android Chromium");
  const fixture = await fragmentedCompatibleFixture();
  const { initEnd } = fragmentedMp4Layout(fixture);
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36",
    });
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function androidCanPlayType(contentType) {
      if (String(contentType).includes("ac-3")) return "";
      return canPlayType.call(this, contentType);
    };
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "other";
      entry.video_profile = "Advanced";
      entry.video_content_type = null;
      entry.codec_string = "other,ac-3";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  const startupEvents = [];
  await page.route("**/api/web/transcode/*", (route) => {
    const request = route.request();
    if (request.method() === "POST") {
      startupEvents.push(new URL(request.url()).searchParams.get("event"));
    }
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
    });
  });
  const requests = [];
  await page.route("**/web/media/*?**", async (route) => {
    const url = new URL(route.request().url());
    const delivery = url.searchParams.get("delivery");
    requests.push(url);
    if (delivery === "mse") {
      const initUrl = new URL(url);
      initUrl.pathname = initUrl.pathname.replace(/\.m3u8$/, ".mp4");
      initUrl.searchParams.set("delivery", "mse_init");
      initUrl.searchParams.set("hls_offset", "0");
      initUrl.searchParams.set("hls_length", String(initEnd));
      const segmentUrl = new URL(initUrl);
      segmentUrl.pathname = segmentUrl.pathname.replace(/\.mp4$/, ".m4s");
      segmentUrl.searchParams.set("delivery", "mse_segment");
      segmentUrl.searchParams.set("hls_offset", String(initEnd));
      segmentUrl.searchParams.set("hls_length", String(fixture.byteLength - initEnd));
      await route.fulfill({
        status: 200,
        contentType: "application/vnd.apple.mpegurl",
        body: [
          "#EXTM3U",
          "#EXT-X-VERSION:7",
          `#EXT-X-MAP:URI="${initUrl.pathname}?${initUrl.searchParams}"`,
          "#EXTINF:2.000000,",
          `${segmentUrl.pathname}?${segmentUrl.searchParams}`,
          "",
        ].join("\n"),
      });
      return;
    }
    const start = Number(url.searchParams.get("hls_offset"));
    const length = Number(url.searchParams.get("hls_length"));
    const body = fixture.subarray(start, start + length);
    await route.fulfill({
      status: 200,
      headers: {
        "Content-Length": String(body.byteLength),
        "Content-Type": delivery === "mse_init" ? "video/mp4" : "video/iso.segment",
      },
      body,
    });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.some((url) => url.searchParams.get("delivery") === "mse")).toBe(true);
  await expect.poll(() => requests.some((url) => url.searchParams.get("delivery") === "mse_init")).toBe(true);
  await expect.poll(() => requests.some((url) => url.searchParams.get("delivery") === "mse_segment")).toBe(true);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.videoWidth > 0)).toBe(true);
  await expect.poll(() => startupEvents).toEqual(expect.arrayContaining([
    "mse_playlist_received",
    "mse_init_fetched",
    "mse_init_appended",
    "mse_first_fragment_fetched",
    "mse_first_fragment_appended",
  ]));
  await page.locator("#video-player").evaluate((video) => video.pause());
  await page.waitForTimeout(700);
  const pausedPlaylistRequests = requests.filter((url) => url.searchParams.get("delivery") === "mse").length;
  await page.waitForTimeout(1_100);
  expect(requests.filter((url) => url.searchParams.get("delivery") === "mse")).toHaveLength(pausedPlaylistRequests);
  await expect(page.locator("#output-stream-facts")).toContainText("Media Source · fragmented MP4");
  await expect(page.locator("#output-stream-facts")).toContainText("720p · 3 Mbps (automatic recovery)");
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("Android Media Source retries a busy playlist generation", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Media Source recovery belongs to Android Chromium");
  const fixture = await fragmentedCompatibleFixture();
  const { initEnd } = fragmentedMp4Layout(fixture);
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36",
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "other";
      entry.video_content_type = null;
      entry.codec_string = "other,ac-3";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  let playlistAttempts = 0;
  await page.route("**/api/web/transcode/*", (route) => {
    const deleting = route.request().method() === "DELETE";
    const queued = !deleting && playlistAttempts <= 1;
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        schema_version: 2,
        item_id: "9",
        request_id: 1,
        state: deleting ? "cancelled" : queued ? "queued" : "producing",
        retry_after_seconds: queued ? 0.01 : null,
      }),
    });
  });
  const requests = [];
  await page.route("**/web/media/*?**", async (route) => {
    const url = new URL(route.request().url());
    const delivery = url.searchParams.get("delivery");
    requests.push(url);
    if (delivery === "mse") {
      playlistAttempts += 1;
      if (playlistAttempts === 1) {
        await route.fulfill({ status: 503, body: "busy" });
        return;
      }
      const initUrl = new URL(url);
      initUrl.pathname = initUrl.pathname.replace(/\.m3u8$/, ".mp4");
      initUrl.searchParams.set("delivery", "mse_init");
      initUrl.searchParams.set("hls_offset", "0");
      initUrl.searchParams.set("hls_length", String(initEnd));
      const segmentUrl = new URL(initUrl);
      segmentUrl.pathname = segmentUrl.pathname.replace(/\.mp4$/, ".m4s");
      segmentUrl.searchParams.set("delivery", "mse_segment");
      segmentUrl.searchParams.set("hls_offset", String(initEnd));
      segmentUrl.searchParams.set("hls_length", String(fixture.byteLength - initEnd));
      await route.fulfill({
        status: 200,
        contentType: "application/vnd.apple.mpegurl",
        body: [
          "#EXTM3U",
          "#EXT-X-VERSION:7",
          `#EXT-X-MAP:URI="${initUrl.pathname}?${initUrl.searchParams}"`,
          "#EXTINF:2.000000,",
          `${segmentUrl.pathname}?${segmentUrl.searchParams}`,
          "#EXT-X-ENDLIST",
          "",
        ].join("\n"),
      });
      return;
    }
    const start = Number(url.searchParams.get("hls_offset"));
    const length = Number(url.searchParams.get("hls_length"));
    const body = fixture.subarray(start, start + length);
    await route.fulfill({
      status: 200,
      headers: {
        "Content-Length": String(body.byteLength),
        "Content-Type": delivery === "mse_init" ? "video/mp4" : "video/iso.segment",
      },
      body,
    });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.filter((url) => url.searchParams.get("delivery") === "mse").length)
    .toBeGreaterThan(1);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.videoWidth > 0)).toBe(true);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("Android sends compatible copied video through Media Source", async ({ page, browserName }) => {
  test.skip(browserName !== "chromium", "Android delivery selection belongs to mobile Chromium");
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36",
    });
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function androidCanPlayType(contentType) {
      if (String(contentType).includes("mpegurl")) return "maybe";
      if (String(contentType).includes("avc1.")) return "probably";
      if (String(contentType).includes("ac-3")) return "";
      return canPlayType.call(this, contentType);
    };
    const isTypeSupported = MediaSource.isTypeSupported.bind(MediaSource);
    Object.defineProperty(MediaSource, "isTypeSupported", {
      configurable: true,
      value: (contentType) => String(contentType).includes("avc1.640028,mp4a.40.2")
        || isTypeSupported(contentType),
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "h264";
      entry.video_content_type = 'video/mp4; codecs="avc1.640028"';
      entry.codec_string = "avc1.640028,ac-3";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "starting", retry_after_seconds: 1 }),
  }));
  const requests = [];
  await page.route("**/web/media/*?**", async (route) => {
    requests.push(new URL(route.request().url()));
    await route.fulfill({ status: 503, body: "capture request" });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.length).toBeGreaterThan(0);
  expect(requests[0].pathname).toMatch(/\.m3u8$/);
  expect(requests[0].searchParams.get("delivery")).toBe("mse");
  expect(requests[0].searchParams.get("quality")).toBe("auto");
  expect(requests[0].searchParams.get("video_mode")).toBe("copy");
  expect(requests[0].searchParams.get("audio_mode")).toBe("transcode");
});

test("native HLS and Media Source playlists expose fixed fragmented MP4 resources", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "chromium", "exercise the shared server pipeline once");
  const libraryResponse = await page.request.get("/api/web/library?view=library&kind=video&q=tagged&limit=10");
  expect(libraryResponse.ok()).toBe(true);
  const library = await libraryResponse.json();
  const item = library.entries.find((entry) => entry.entry_type === "media" && entry.title === "tagged");
  expect(item).toBeTruthy();
  const params = new URLSearchParams({
    mode: "compatible",
    audio: "0",
    start: "0",
    quality: "data_saver",
    video_mode: "transcode",
    audio_mode: "transcode",
    reason: "test_hls",
    request: "7101",
    session: "8101",
    delivery: "hls",
  });
  const playlistUrl = item.fallback_url.replace(/\.mp4$/, ".m3u8");
  let playlistResponse;
  await expect.poll(async () => {
    playlistResponse = await page.request.get(`${playlistUrl}?${params}`, { timeout: 30_000 });
    return playlistResponse.status();
  }, { timeout: 30_000 }).toBe(200);
  expect(playlistResponse.headers()["content-type"]).toContain("application/vnd.apple.mpegurl");
  const playlist = await playlistResponse.text();
  expect(playlist).toContain("#EXT-X-PLAYLIST-TYPE:EVENT");
  expect(playlist).toContain("#EXT-X-INDEPENDENT-SEGMENTS");

  const map = playlist.match(/#EXT-X-MAP:URI="([^"]+)"/);
  const segment = playlist.match(/#EXTINF:[^\n]+\n([^\n]+)/);
  expect(map).toBeTruthy();
  expect(segment).toBeTruthy();
  const initUrl = new URL(map[1], "http://localhost");
  expect(initUrl.pathname).toMatch(/\.mp4$/);
  expect(initUrl.searchParams.get("delivery")).toBe("hls_init");
  const initResponse = await page.request.get(map[1]);
  expect(initResponse.status()).toBe(200);
  const init = await initResponse.body();
  expect(init.byteLength).toBe(Number(initUrl.searchParams.get("hls_length")));
  expect(init.subarray(4, 8).toString("ascii")).toBe("ftyp");
  expect(init.includes(Buffer.from("moov"))).toBe(true);

  const segmentUrl = new URL(segment[1], "http://localhost");
  expect(segmentUrl.pathname).toMatch(/\.m4s$/);
  expect(segmentUrl.searchParams.get("delivery")).toBe("hls_segment");
  const segmentResponse = await page.request.get(segment[1]);
  expect(segmentResponse.status()).toBe(200);
  expect(segmentResponse.headers()["content-type"]).toContain("video/iso.segment");
  const segmentBytes = await segmentResponse.body();
  expect(segmentBytes.byteLength).toBe(Number(segmentUrl.searchParams.get("hls_length")));
  expect(segmentBytes.subarray(4, 8).toString("ascii")).toBe("moof");
  expect(segmentBytes.includes(Buffer.from("mdat"))).toBe(true);

  params.set("reason", "test_mse_delta");
  params.set("request", "7201");
  params.set("session", "8201");
  params.set("delivery", "mse");
  params.set("mse_after", "0");
  const initialResponse = await page.request.get(`${playlistUrl}?${params}`, { timeout: 30_000 });
  expect(initialResponse.status()).toBe(200);
  const initial = await initialResponse.text();
  const initialSegments = [...initial.matchAll(/#EXTINF:[^\n]+\n([^\n]+)/g)].map((match) => match[1]);
  expect(initialSegments.length).toBeGreaterThan(0);
  expect(initial).toContain("#EXT-X-MEDIA-SEQUENCE:0");
  expect(initialSegments[0]).not.toContain("mse_after=");

  params.set("mse_after", "1");
  const deltaResponse = await page.request.get(`${playlistUrl}?${params}`, { timeout: 30_000 });
  expect(deltaResponse.status()).toBe(200);
  const delta = await deltaResponse.text();
  expect(delta).toContain("#EXT-X-MEDIA-SEQUENCE:1");
  expect(delta).not.toContain(initialSegments[0]);
  expect(delta).not.toContain("mse_after=");
});

test("an advisory HEVC repair decode error retries with portable video and audio", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const original = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function negotiatedCanPlayType(contentType) {
      if (String(contentType).includes("hvc1.")) return "probably";
      if (String(contentType).includes("ac-3")) return "";
      return original.call(this, contentType);
    };
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: {
        decodingInfo: async (configuration) => ({ supported: Boolean(configuration.video) }),
      },
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "hevc";
      entry.codec_string = "hvc1.2.4.H153.90,ac-3";
      entry.video_content_type = 'video/mp4; codecs="hvc1.2.4.H153.90"';
      entry.video_profile = "Main 10";
      entry.bit_depth = 10;
      entry.hdr = "hdr10";
      entry.video_timestamp_mode = "broken-reordered";
      entry.video_repair_required = true;
      entry.repair_video_encoder = "hevc_nvenc";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: 1 }),
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
  expect(requests[0].searchParams.get("video_mode")).toBe("repair");
  expect(requests[0].searchParams.get("audio_mode")).toBe("transcode");
  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 3 } });
    video.dispatchEvent(new Event("error"));
  });

  await expect.poll(() => requests.length).toBe(2);
  expect(requests[1].searchParams.get("video_mode")).toBe("transcode");
  expect(requests[1].searchParams.get("audio_mode")).toBe("transcode");
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("disabled, busy, and failed compatible playback recover appropriately", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
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
  const fixture = await readFile(compatibleFixture);
  let mediaRequests = 0;
  const busyMediaAttempts = 5;
  await page.route("**/api/web/transcode/*", (route) => {
    const state = status === "queued" && mediaRequests > busyMediaAttempts ? "producing" : status;
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state, retry_after_seconds: state === "queued" ? 0.01 : null }),
    });
  });
  await page.route("**/web/media/*.mp4?**", (route) => {
    mediaRequests += 1;
    return status === "queued" && mediaRequests > busyMediaAttempts
      ? route.fulfill({ status: 200, contentType: "video/mp4", body: fixture })
      : route.abort("failed");
  });
  await page.reload();
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect.poll(() => mediaRequests).toBeGreaterThan(busyMediaAttempts);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();

  status = "failed";
  mediaRequests = 0;
  await page.reload();
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  await expect(page.locator("#player-message-text")).toContainText("could not prepare this title");
  await expect(page.locator("#play-original")).toBeVisible();
});

test("an unsupported-source MediaError still waits when the producer is queued", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await usePreference(page, "quality", "data_saver");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "userAgent", {
      configurable: true,
      value: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.video_codec = "other";
      entry.video_content_type = null;
      entry.codec_string = "other,ac-3";
      entry.audio_codec = "ac3";
      entry.audio_tracks = [{ index: 0, codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6, default: true }];
      entry.stream_metadata_complete = true;
    }
    await route.fulfill({ response, json: payload });
  });
  let mediaRequests = 0;
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({
      schema_version: 2,
      item_id: "9",
      request_id: 1,
      state: mediaRequests <= 1 ? "queued" : "producing",
      retry_after_seconds: 0.01,
    }),
  }));
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", async (route) => {
    mediaRequests += 1;
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => mediaRequests).toBe(1);
  await page.locator("#video-player").evaluate((video) => {
    Object.defineProperty(video, "error", { configurable: true, value: { code: 4 } });
    video.dispatchEvent(new Event("error"));
    delete video.error;
  });

  await expect.poll(() => mediaRequests).toBeGreaterThan(1);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("Auto playback selects tagged English audio before the file default", async ({ page }) => {
  await usePreference(page, "stream", "auto");
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.stream_metadata_complete = true;
      entry.default_audio_index = 1;
      entry.audio_tracks = [
        { index: 0, codec: "aac", channels: 2, language: "jpn", title: "Main", default: true },
        { index: 1, codec: "ac3", channels: 6, language: "eng", title: "English", default: false },
      ];
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => requests.findLast((url) => url.searchParams.get("reason") === "preferred_audio")?.searchParams.get("mode")).toBe("compatible");
  const preferredRequest = requests.findLast((url) => url.searchParams.get("reason") === "preferred_audio");
  expect(preferredRequest.searchParams.get("audio")).toBe("1");
  await expect(page.locator("#mode-label")).toHaveText(/^(Repackaging|Converting audio|Re-encoding video)$/);
});

test("disabled transcoding blocks forced recovery and audio-track switching", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    payload.capabilities.transcoding = false;
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.stream_metadata_complete = true;
      entry.default_audio_index = 0;
      entry.audio_tracks = [
        { index: 0, codec: "aac", channels: 2, language: "eng", title: "Main", default: true },
        { index: 1, codec: "ac3", channels: 6, language: "fra", title: "Dub", default: false },
      ];
    }
    await route.fulfill({ response, json: payload });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await openAdvancedPlayback(page);
  await expect(page.locator("#audio-track-controls")).toBeVisible();
  await expect(page.locator("#audio-track-controls")).toBeDisabled();
  await expect(page.locator("#audio-track-status")).toContainText("Prepared streaming, which is disabled");
  await page.locator("#audio-track-controls").evaluate((select) => {
    select.value = "1";
    select.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await page.waitForTimeout(50);
  expect(requests.filter((url) => url.searchParams.get("mode") === "compatible")).toHaveLength(0);
  await page.locator('#advanced-playback-dialog button[value="close"]').click();

  await page.locator("#video-player").dispatchEvent("error");
  await expect(page.locator("#player-message-text")).toContainText("cannot play the original file");
  await expect(page.locator("#try-compatible")).toBeHidden();
  await expect(page.locator("#return-library")).toBeVisible();
  await page.locator("#return-library").click();
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "browse");
  await expect(page.locator("#player-panel")).toBeHidden();
  await expect(page.locator("#library-panel")).toBeFocused();
  await page.locator("#try-compatible").evaluate((button) => button.click());
  await page.waitForTimeout(50);
  expect(requests.filter((url) => url.searchParams.get("mode") === "compatible")).toHaveLength(0);
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
    body: JSON.stringify({ schema_version: 2, error: { code: "media_missing", message: "raw path", recoverable: true, action: "return_to_library" } }),
  }));
  await page.route("**/web/media/*.mp4?**", (route) => route.abort("failed"));
  await card.locator(".card-button").click();
  await expect(page.locator("#player-message-text")).toHaveText("This media file is no longer available.");
  await expect(page.locator("#technical-message")).not.toContainText("raw path");
});

test("a dropped compatible-media connection retries a healthy producer", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
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
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
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

test("a prepared compatible stream reattaches when Chromium decodes no data", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const nativeLoad = HTMLMediaElement.prototype.load;
    const nativeReadyState = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, "readyState");
    const nativeSetTimeout = window.setTimeout.bind(window);
    window.__compatibleLoads = [];
    window.__compatibleCanDecode = false;
    Object.defineProperty(HTMLMediaElement.prototype, "readyState", {
      configurable: true,
      get() {
        const source = this.getAttribute("src") || "";
        if (source.includes("/web/media/")
          && source.includes("mode=compatible")
          && !window.__compatibleCanDecode) return 0;
        return nativeReadyState.get.call(this);
      },
    });
    for (const name of ["loadedmetadata", "loadeddata", "canplay", "playing", "error"]) {
      document.addEventListener(name, (event) => {
        const source = event.target?.getAttribute?.("src") || "";
        if (source.includes("/web/media/")
          && source.includes("mode=compatible")
          && !window.__compatibleCanDecode) event.stopImmediatePropagation();
      }, true);
    }
    window.setTimeout = (callback, delay = 0, ...args) => nativeSetTimeout(
      callback,
      delay === 8_000 ? 25 : delay,
      ...args,
    );
    HTMLMediaElement.prototype.load = function controlledCompatibleLoad() {
      const source = this.getAttribute("src") || "";
      if (!source.includes("/web/media/") || !source.includes("mode=compatible")) {
        return nativeLoad.call(this);
      }
      window.__compatibleLoads.push(source);
      this.dispatchEvent(new Event("loadstart"));
      if (window.__compatibleLoads.length > 1) {
        window.__compatibleCanDecode = true;
        nativeSetTimeout(() => {
          this.dispatchEvent(new Event("loadedmetadata"));
          this.dispatchEvent(new Event("loadeddata"));
          this.dispatchEvent(new Event("canplay"));
        }, 0);
      }
    };
    HTMLMediaElement.prototype.play = function playPreparedStream() {
      this.dispatchEvent(new Event("playing"));
      return Promise.resolve();
    };
  });
  const cancellations = [];
  page.on("request", (request) => {
    if (request.method() === "DELETE" && request.url().includes("/api/web/transcode/")) {
      cancellations.push(request.url());
    }
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
  }));

  await openLibrary(page);
  await selectTaggedVideo(page);

  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  const loads = await page.evaluate(() => window.__compatibleLoads);
  expect(loads).toHaveLength(2);
  expect(new Set(loads).size).toBe(1);
  expect(cancellations).toEqual([]);
  await expect(page.locator("#player-message[role=alert]")).toBeHidden();
});

test("active compatible playback keeps renewing its transcode generation", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    const nativeSetTimeout = window.setTimeout.bind(window);
    window.setTimeout = (callback, delay = 0, ...args) => nativeSetTimeout(
      callback,
      delay === 10_000 ? 25 : delay,
      ...args,
    );
    HTMLMediaElement.prototype.play = function playWithoutEnding() {
      this.dispatchEvent(new Event("playing"));
      return Promise.resolve();
    };
  });
  const statusGets = [];
  await page.route("**/api/web/transcode/*", (route) => {
    const url = new URL(route.request().url());
    if (route.request().method() === "GET") statusGets.push(url);
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        schema_version: 2,
        item_id: "9",
        request_id: Number(url.searchParams.get("request")),
        state: route.request().method() === "DELETE" ? "cancelled" : "producing",
        retry_after_seconds: null,
      }),
    });
  });
  await serveFixtureMedia(page);

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  await expect.poll(() => statusGets.length).toBeGreaterThan(2);
  const sessions = new Set(statusGets.map((url) => url.searchParams.get("session")));
  const requests = new Set(statusGets.map((url) => url.searchParams.get("request")));
  expect(sessions.size).toBe(1);
  expect(requests.size).toBe(1);
});

test("brief playback cannot reset the compatible retry budget", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    window.__briefPlayEvents = 0;
    HTMLMediaElement.prototype.play = function playBriefly() {
      window.__briefPlayEvents += 1;
      this.dispatchEvent(new Event("playing"));
      window.setTimeout(() => this.dispatchEvent(new Event("error")), 25);
      return Promise.resolve();
    };
  });
  const fixture = await readFile(compatibleFixture);
  const generations = new Map();
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: 0.01 }),
  }));
  await page.route("**/web/media/*.mp4?**", (route) => {
    const url = new URL(route.request().url());
    generations.set(url.searchParams.get("request"), url.searchParams.get("session"));
    return route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);

  await expect(page.locator("#player-message-text")).toContainText("could not prepare this title");
  expect(generations.size).toBe(4);
  const requestIds = [...generations.keys()].map(Number);
  expect(new Set(generations.values()).size).toBe(1);
  expect(requestIds.every((requestId, index) => index === 0 || requestId > requestIds[index - 1])).toBe(true);
  expect(await page.evaluate(() => window.__briefPlayEvents)).toBeGreaterThanOrEqual(4);
  await page.waitForTimeout(750);
  expect(generations.size).toBe(4);
});

test("rapid item switching suppresses an older media failure", async ({ page }) => {
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
  await page.addInitScript(() => {
    // The media fixture is only 0.4 seconds long while this test expands its
    // catalog duration to ten minutes. Keep decoding available without
    // allowing an unrelated early-ended recovery to change the stream plan.
    HTMLMediaElement.prototype.play = () => Promise.resolve();
    window.__seekCapabilityProbes = 0;
    Object.defineProperty(navigator, "mediaCapabilities", {
      configurable: true,
      value: {
        decodingInfo: async () => {
          window.__seekCapabilityProbes += 1;
          return { supported: false, smooth: false, powerEfficient: false };
        },
      },
    });
  });
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
  await expect.poll(() => page.locator("#video-player").evaluate((video) => (
    video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA
  ))).toBe(true);
  const initialRequest = new URL(await page.locator("#video-player").evaluate((video) => (
    video.currentSrc || video.src
  )));
  const initialCapabilityProbes = await page.evaluate(() => window.__seekCapabilityProbes);
  expect(initialCapabilityProbes).toBeGreaterThan(0);
  await expect(page.locator("#timeline")).toHaveAttribute("max", "600");
  await page.locator("#timeline").evaluate((timeline, values) => {
    for (const next of values) {
      timeline.value = String(next);
      timeline.dispatchEvent(new Event("input", { bubbles: true }));
      timeline.dispatchEvent(new Event("change", { bubbles: true }));
    }
  }, [12, 18, 27]);
  await expect.poll(() => [...new Set(requests
    .map((url) => url.searchParams.get("start"))
    .filter((start) => start !== "0"))]).toEqual(["20"]);
  await expect.poll(() => cancellations.length).toBeGreaterThan(0);
  expect(cancellations[0].searchParams.get("request")).toMatch(/^\d+$/);
  expect(cancellations[0].searchParams.get("session")).toMatch(/^\d+$/);
  const seekRequest = requests.find((url) => url.searchParams.get("start") !== "0");
  expect(seekRequest?.searchParams.get("start")).toBe("20");
  for (const parameter of ["quality", "video_mode", "audio_mode"]) {
    expect(seekRequest?.searchParams.get(parameter)).toBe(initialRequest?.searchParams.get(parameter));
  }
  expect(await page.evaluate(() => window.__seekCapabilityProbes)).toBe(initialCapabilityProbes);
  await openAdvancedPlayback(page);
  await page.locator("#audio-track-controls").selectOption("1");
  await expect.poll(() => requests.findLast((url) => url.searchParams.get("audio") === "1")?.searchParams.get("start")).toBe("20");
  const playbackSessions = new Set(requests
    .filter((url) => url.searchParams.get("mode") === "compatible")
    .map((url) => url.searchParams.get("session")));
  expect(playbackSessions.size).toBe(1);
  expect([...playbackSessions][0]).toMatch(/^\d+$/);
  const audioLayout = await page.locator("#audio-track-controls").evaluate((select) => ({
    width: select.getBoundingClientRect().width,
    columnWidth: select.closest("label").getBoundingClientRect().width,
    speedWidth: document.getElementById("speed-control").getBoundingClientRect().width,
  }));
  expect(audioLayout.width).toBeLessThanOrEqual(audioLayout.columnWidth);
  expect(Math.abs(audioLayout.width - audioLayout.speedWidth)).toBeLessThanOrEqual(1);
  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  await expect(page.locator("#mode-label")).toHaveText(/^(Repackaging|Converting audio|Re-encoding video)$/);
});

test("compatible seeking holds the last video frame until replacement data is ready", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const drawImage = CanvasRenderingContext2D.prototype.drawImage;
    window.__heldFrameDraws = 0;
    CanvasRenderingContext2D.prototype.drawImage = function heldFrameDraw(...args) {
      window.__heldFrameDraws += 1;
      return drawImage.apply(this, args);
    };
  });
  const fixture = await readFile(compatibleFixture);
  let markSeekRequested;
  const seekRequested = new Promise((resolve) => { markSeekRequested = resolve; });
  let releaseSeekResponse;
  const seekResponse = new Promise((resolve) => { releaseSeekResponse = resolve; });
  await page.route("**/web/media/*.mp4?**", async (route) => {
    const start = new URL(route.request().url()).searchParams.get("start");
    if (start !== "0") {
      markSeekRequested();
      await seekResponse;
    }
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });
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
  await expect.poll(() => page.locator("#video-player").evaluate((video) => (
    video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA
      && video.videoWidth > 0
      && video.videoHeight > 0
  ))).toBe(true);
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");

  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "27";
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
  const heldFrame = page.locator("#video-frame-hold");
  await expect(heldFrame).toBeVisible();
  const heldBitmap = await heldFrame.evaluate((canvas) => ({
    width: canvas.width,
    height: canvas.height,
    alpha: canvas.getContext("2d").getImageData(
      Math.floor(canvas.width / 2),
      Math.floor(canvas.height / 2),
      1,
      1,
    ).data[3],
  }));
  expect(heldBitmap.width * heldBitmap.height).toBeLessThanOrEqual(4_194_304);
  expect(heldBitmap.alpha).toBe(255);
  expect(await page.evaluate(() => window.__heldFrameDraws)).toBe(1);

  await seekRequested;
  await expect(heldFrame).toBeVisible();
  await page.locator("#play-button").click();
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  releaseSeekResponse();
  await expect(heldFrame).toBeHidden();
  await expect.poll(() => page.locator("#video-player").evaluate((video) => (
    video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA
  ))).toBe(true);
  // The 0.4-second fixture can reach its natural end after the delayed seek
  // response even though this test advertises a ten-minute catalog duration.
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", /^(Play|Replay)$/);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => video.paused)).toBe(true);
});

test("timeline scrubbing shows the nearest sprite until replacement video is ready", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    // This fixture is 0.4 seconds long while the catalog duration is expanded
    // to ten minutes below. Keep its decode path active without letting an
    // unrelated early-ended recovery race the seek being tested.
    HTMLMediaElement.prototype.play = function play() {
      return Promise.resolve();
    };
  });
  const fixture = await readFile(compatibleFixture);
  let markSeekRequested;
  const seekRequested = new Promise((resolve) => { markSeekRequested = resolve; });
  let releaseSeekResponse;
  const seekResponse = new Promise((resolve) => { releaseSeekResponse = resolve; });
  await page.route("**/web/media/*.mp4?**", async (route) => {
    const start = new URL(route.request().url()).searchParams.get("start");
    if (start !== "0") {
      markSeekRequested();
      await seekResponse;
    }
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type !== "media" || entry.kind !== "video") continue;
      entry.duration_seconds = 600;
      entry.duration = "0:10:00.000";
      entry.preview_url = `/api/web/preview/${entry.id}`;
    }
    await route.fulfill({ response, json: payload });
  });
  let previewRequests = 0;
  await page.route("**/api/web/preview/*", async (route) => {
    previewRequests += 1;
    const itemId = new URL(route.request().url()).pathname.split("/").pop();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        schema_version: 2,
        item_id: String(itemId),
        available: true,
        duration_seconds: 600,
        interval_seconds: 3,
        frame_width: 960,
        frame_height: 540,
        columns: 3,
        rows: 7,
        frame_count: 200,
        sheet_urls: Array.from(
          { length: 10 },
          (_, index) => `/web/preview/${itemId}/0123456789abcdef/${index}.jpg`,
        ),
      }),
    });
  });
  let selectedSheetAttempts = 0;
  await page.route("**/web/preview/*/*/*.jpg", async (route) => {
    const blue = new URL(route.request().url()).pathname.endsWith("/4.jpg");
    if (blue && selectedSheetAttempts++ === 0) {
      await route.fulfill({ status: 502, contentType: "text/plain", body: "temporary failure" });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "image/svg+xml",
      body: `<svg xmlns="http://www.w3.org/2000/svg" width="2880" height="3780"><rect width="2880" height="3780" fill="${blue ? "#0000ff" : "#ff0000"}"/></svg>`,
    });
  });

  await openLibrary(page);
  await selectTaggedVideo(page);
  await expect.poll(() => previewRequests).toBe(1);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => (
    video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA
  ))).toBe(true);
  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "303";
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
  });
  const heldFrame = page.locator("#video-frame-hold");
  await expect.poll(() => heldFrame.evaluate((canvas) => canvas.width)).toBe(960);
  expect(selectedSheetAttempts).toBeGreaterThanOrEqual(2);
  expect(await heldFrame.evaluate((canvas) => {
    const pixel = canvas.getContext("2d").getImageData(480, 270, 1, 1).data;
    return [...pixel];
  })).toEqual([0, 0, 255, 255]);

  await page.locator("#timeline").evaluate((timeline) => {
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await seekRequested;
  await expect(heldFrame).toBeVisible();
  expect(await heldFrame.evaluate((canvas) => canvas.width)).toBe(960);
  releaseSeekResponse();
  await expect(heldFrame).toBeHidden();
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

test("keyboard seeking continues from an exact dragged target while compatible media reloads", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const positions = new WeakMap();
    const sources = new WeakMap();
    window.__dragSeekSources = [];
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) {
        const source = new URL(value, document.baseURI).href;
        sources.set(this, source);
        if (this instanceof HTMLVideoElement) window.__dragSeekSources.push(source);
      },
    });
    Object.defineProperty(HTMLMediaElement.prototype, "currentTime", {
      configurable: true,
      get() { return positions.get(this) || 0; },
      set(value) { positions.set(this, Number(value)); },
    });
    HTMLMediaElement.prototype.load = function load() {
      if (!this.hasAttribute("src")) positions.set(this, 0);
    };
    HTMLMediaElement.prototype.pause = () => {};
    HTMLMediaElement.prototype.play = () => Promise.resolve();
  });
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "9", request_id: 1, state: "producing", retry_after_seconds: null }),
  }));
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
  await expect(page.locator("#timeline")).toHaveAttribute("max", "600");
  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "127";
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect.poll(() => page.evaluate(() => window.__dragSeekSources.some((source) => (
    new URL(source).searchParams.get("start") === "120"
  )))).toBe(true);

  await page.locator("#player-stage").focus();
  await page.evaluate(() => {
    document.dispatchEvent(new KeyboardEvent("keydown", {
      key: "ArrowRight",
      bubbles: true,
      cancelable: true,
    }));
  });
  await expect(page.locator("#timeline")).toHaveValue("137");
});

test("caption Escape closes the popup before expanded playback and restores focus", async ({ page }) => {
  await installIphoneUserAgent(page);
  await serveFixtureMedia(page);
  await openLibrary(page);
  await openVideoView(page);
  const response = await page.request.get("/api/web/library?view=library&kind=video&q=&sort=title&offset=0&limit=60");
  const payload = await response.json();
  const captioned = payload.entries.find((entry) => entry.entry_type === "media"
    && entry.captions?.some((caption) => caption.browser_supported));
  expect(captioned).toBeTruthy();
  await page.locator(`[data-media-id="${captioned.id}"] .card-button`).click();
  await showPlayerControls(page);
  await page.locator("#fullscreen-button").click();
  await expect(page.locator("#player-stage")).toHaveClass(/expanded-player/);
  const captions = page.locator("#captions-button");
  await captions.click();
  await expect(page.locator("#caption-menu")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.locator("#caption-menu")).toBeHidden();
  await expect(captions).toBeFocused();
  await expect(captions).toHaveAttribute("aria-expanded", "false");
  await expect(page.locator("#player-stage")).toHaveClass(/expanded-player/);
  await captions.click();
  await expect(page.locator("#caption-menu")).toBeVisible();
  await page.locator("#player-stage").dispatchEvent("pointerdown");
  await expect(page.locator("#caption-menu")).toBeHidden();
  await expect(captions).toHaveAttribute("aria-expanded", "false");
});

test("captions survive source restarts but reset for a different title", async ({ page }) => {
  await usePreference(page, "caption", "legacy-index");
  await serveFixtureMedia(page);
  await page.goto("/?view=video");
  const { captioned, other } = await page.evaluate(async () => {
    const response = await fetch("/api/web/library?view=library&kind=video&q=&sort=title&offset=0&limit=60");
    const payload = await response.json();
    const media = payload.entries.filter((entry) => entry.entry_type === "media");
    const withCaptions = media.find((entry) => entry.captions?.some((caption) => caption.browser_supported));
    return {
      captioned: withCaptions,
      other: media.find((entry) => String(entry.id) !== String(withCaptions?.id)),
    };
  });
  expect(captioned).toBeTruthy();
  expect(other).toBeTruthy();
  await page.locator(`[data-media-id="${captioned.id}"] .card-button`).click();
  await showPlayerControls(page);
  await expect(page.locator("#captions-button")).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator('input[name="caption-choice"][value="off"]')).toBeChecked();
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
  expect(await page.evaluate(() => localStorage.getItem("rustydlna.caption"))).toBe("legacy-index");
  await page.locator('#advanced-playback-dialog button[value="close"]').click();

  await page.locator(`[data-media-id="${other.id}"] .card-button`).click();
  await showPlayerControls(page);
  await expect(page.locator("#captions-button")).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator('input[name="caption-choice"][value="off"]')).toBeChecked();
});

// Real text tracks and media seeks exercise cue timing, not just menu state.
async function captionTimelineFixture(page) {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    // Keep the short media fixture paused while we inspect its real cue clock.
    HTMLMediaElement.prototype.play = () => Promise.resolve();
  });
  await disableFragmentedDelivery(page);
  const fixture = await readFile(compatibleFixture);
  await page.route("**/web/media/*.mp4?**", async (route) => {
    const range = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || "");
    const start = range ? Number(range[1]) : 0;
    const end = range?.[2] ? Math.min(Number(range[2]), fixture.length - 1) : fixture.length - 1;
    await route.fulfill({
      status: range ? 206 : 200,
      contentType: "video/mp4",
      headers: {
        "accept-ranges": "bytes",
        ...(range ? { "content-range": `bytes ${start}-${end}/${fixture.length}` } : {}),
      },
      body: fixture.subarray(start, end + 1),
    });
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
        captions: [{ index: 0, browser_supported: true, label: "Timeline", language: "en", url: "/review-captions.vtt" }],
      });
    }
    await route.fulfill({ response, json: payload });
  });
  return item;
}

const captionTimelineVtt = `WEBVTT

opening
00:00:00.000 --> 00:00:00.350
Opening scene

crossing
00:01:29.500 --> 00:01:30.150 align:start position:20%
Crossing the source start

ninety
00:01:30.150 --> 00:01:30.350
Scene at ninety seconds

two-minutes
00:02:00.000 --> 00:02:00.350
Scene at two minutes

`;

async function expectCaptionAt(page, time, text) {
  const video = page.locator("#video-player");
  await expect.poll(() => video.evaluate((video) => video.readyState)).toBeGreaterThanOrEqual(2);
  await expect.poll(() => video.evaluate((video) => video.textTracks[0]?.mode)).toBe("showing");
  await video.evaluate((video, time) => { video.pause(); video.currentTime = time; }, time);
  await expect.poll(() => video.evaluate((video) => video.currentTime)).toBeCloseTo(time, 2);
  await expect.poll(() => video.evaluate((video) => [...(video.textTracks[0]?.activeCues || [])].map((cue) => cue.text)))
    .toEqual([text]);
}

async function seekCaptionTimeline(page, time) {
  await page.locator("#timeline").evaluate((timeline, time) => {
    timeline.value = String(time);
    timeline.dispatchEvent(new Event("input", { bubbles: true }));
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  }, time);
  await expect.poll(() => page.locator("#video-player").evaluate((video) => (
    new URL(video.src || document.baseURI).searchParams.get("start")
  ))).toBe(String(time));
}

for (const start of ["deep link", "saved resume"]) {
  test(`caption cues stay aligned through ${start}, repeated seeks, and original playback`, async ({ page }) => {
    const item = await captionTimelineFixture(page);
    await page.route("**/review-captions.vtt", (route) => route.fulfill({
      contentType: "text/vtt", body: captionTimelineVtt,
    }));
    if (start === "saved resume") {
      await page.addInitScript((id) => localStorage.setItem("rustydlna.webProgress.v1", JSON.stringify({
        [id]: { position: 90, duration: 600, updated: Date.now() },
      })), item.id);
    }
    await page.goto(`/?view=video&item=${item.id}${start === "deep link" ? "&t=90" : ""}`);
    if (start === "saved resume") await page.locator("#resume-button").click();
    await showPlayerControls(page);
    await page.locator("#captions-button").click();
    await page.locator('input[name="caption-choice"][value="0"]').check();
    await expectCaptionAt(page, 0.05, "Crossing the source start");
    expect(await page.locator("#video-player").evaluate((video) => {
      const cue = video.textTracks[0].cues[0];
      return { id: cue.id, start: cue.startTime, end: Math.round(cue.endTime * 1000) / 1000, align: cue.align, position: cue.position };
    })).toEqual({ id: "crossing", start: 0, end: 0.15, align: "start", position: 20 });
    await expectCaptionAt(page, 0.25, "Scene at ninety seconds");
    await seekCaptionTimeline(page, 120);
    await expectCaptionAt(page, 0.05, "Scene at two minutes");
    await seekCaptionTimeline(page, 90);
    await expectCaptionAt(page, 0.25, "Scene at ninety seconds");
    await seekCaptionTimeline(page, 0);
    await expectCaptionAt(page, 0.05, "Opening scene");
    await openAdvancedPlayback(page);
    await page.locator('input[name="stream-mode"][value="direct"]').check();
    await expect.poll(() => page.locator("#video-player").evaluate((video) => new URL(video.src || document.baseURI).searchParams.get("mode")))
      .toBe("direct");
    await expectCaptionAt(page, 0.05, "Opening scene");
  });
}

test("a caption load superseded by a seek cannot apply its old offset", async ({ page }) => {
  const item = await captionTimelineFixture(page);
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  let requested = false;
  await page.route("**/review-captions.vtt", async (route) => {
    requested = true;
    await pending;
    await route.fulfill({ contentType: "text/vtt", body: captionTimelineVtt }).catch(() => {});
  });
  try {
    await page.goto(`/?view=video&item=${item.id}&t=90`);
    await showPlayerControls(page);
    await page.locator("#captions-button").click();
    await page.locator('input[name="caption-choice"][value="0"]').check();
    await expect.poll(() => requested).toBe(true);
    await page.locator("#video-player track").evaluate((track) => { window.__oldCaption = track; });
    await seekCaptionTimeline(page, 120);
    release();
    await expectCaptionAt(page, 0.05, "Scene at two minutes");
    await page.evaluate(() => window.__oldCaption.dispatchEvent(new Event("load")));
    await expectCaptionAt(page, 0.05, "Scene at two minutes");
    expect(await page.evaluate(() => window.__oldCaption.isConnected)).toBe(false);
  } finally {
    release();
  }
});

test("resume offers Start over and blocked browser storage remains nonfatal", async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 480 });
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
  await expect(page.locator("#playback-controls")).toBeHidden();
  await page.locator("#resume-prompt").scrollIntoViewIfNeeded();
  expect(await undersizedTouchTargets(page, "#resume-prompt button:visible")).toEqual([]);
  expect(await page.locator("#resume-prompt button").evaluateAll((buttons) => buttons.every((button) => {
    const bounds = button.getBoundingClientRect();
    const hit = document.elementFromPoint(bounds.left + bounds.width / 2, bounds.top + bounds.height / 2);
    return hit === button || button.contains(hit);
  }))).toBe(true);
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
        body: JSON.stringify({ schema_version: 2, error: { code: "transcode_busy", message: "busy", recoverable: true, action: "retry_item" } }),
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        schema_version: 2,
        id: String(selectedId),
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

test("a delayed non-Abort enrichment failure cannot reload an old title after abort is ignored", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await page.addInitScript(() => {
    const nativeFetch = window.fetch.bind(window);
    let releaseRetry;
    const retryGate = new Promise((resolve) => { releaseRetry = resolve; });
    let attempts = 0;
    let retryStarted = false;
    let retrySettled = false;
    const failedEnrichment = () => new Response(JSON.stringify({
      schema_version: 2,
      error: {
        code: "transcode_busy",
        message: "busy",
        recoverable: true,
        action: "retry_item",
      },
    }), { status: 503, headers: { "Content-Type": "application/json" } });
    window.fetch = async (input, options = {}) => {
      const url = new URL(typeof input === "string" ? input : input.url, document.baseURI);
      if (url.pathname.startsWith("/api/web/item/") && url.searchParams.get("enrich") === "1") {
        attempts += 1;
        if (attempts === 1) return failedEnrichment();
        if (attempts === 2) {
          // Deliberately do not forward options.signal: the old-title request
          // completes with a normal API error even after abortItem() runs.
          retryStarted = true;
          await retryGate;
          retrySettled = true;
          return failedEnrichment();
        }
      }
      return nativeFetch(input, options);
    };
    window.__enrichmentRace = {
      started: () => retryStarted,
      settled: () => retrySettled,
      release: () => releaseRetry(),
    };
  });
  let firstId = null;
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    const media = (payload.entries || []).filter((entry) => entry.entry_type === "media" && entry.kind === "video");
    if (media.length > 0 && firstId === null) firstId = String(media[0].id);
    for (const entry of media) {
      if (String(entry.id) === firstId) entry.stream_metadata_complete = false;
    }
    await route.fulfill({ response, json: payload });
  });
  const requests = [];
  await serveFixtureMedia(page, (url) => requests.push(url));

  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video");
  const secondId = await cards.nth(1).getAttribute("data-media-id");
  const secondTitle = (await cards.nth(1).locator(".card-title").textContent()).trim();
  await cards.nth(0).locator(".card-button").click();
  await openAdvancedPlayback(page);
  await expect(page.locator("#audio-track-retry")).toBeVisible();
  await page.locator("#audio-track-retry").click();
  await expect.poll(() => page.evaluate(() => window.__enrichmentRace.started())).toBe(true);
  await page.locator('#advanced-playback-dialog button[value="close"]').click();

  await cards.nth(1).locator(".card-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect.poll(() => requests.some((url) => url.pathname.includes(`/${secondId}.`))).toBe(true);
  const oldRequests = requests.filter((url) => url.pathname.includes(`/${firstId}.`)).length;

  await page.evaluate(() => window.__enrichmentRace.release());
  await expect.poll(() => page.evaluate(() => window.__enrichmentRace.settled())).toBe(true);
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));

  expect(requests.filter((url) => url.pathname.includes(`/${firstId}.`))).toHaveLength(oldRequests);
  await expect(page.locator("#video-player")).toHaveAttribute("src", new RegExp(`/web/media/${secondId}\\.`));
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
});

test("selecting a title clears prior media while enrichment is pending", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  const selectedIds = [];
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    const media = (payload.entries || []).filter((entry) => entry.entry_type === "media" && entry.kind === "video");
    if (selectedIds.length === 0 && media.length >= 2) selectedIds.push(String(media[0].id), String(media[1].id));
    for (const entry of media) {
      if (String(entry.id) === selectedIds[0]) entry.stream_metadata_complete = true;
      if (String(entry.id) === selectedIds[1]) entry.stream_metadata_complete = false;
    }
    await route.fulfill({ response, json: payload });
  });
  let enrichmentStarted = false;
  let releaseEnrichment;
  const enrichmentGate = new Promise((resolve) => { releaseEnrichment = resolve; });
  await page.route("**/api/web/item/*?enrich=1", async (route) => {
    enrichmentStarted = true;
    await enrichmentGate;
    await route.continue();
  });
  await serveFixtureMedia(page);
  await page.goto("/?view=video");
  await expect(page.locator(".media-card.video").nth(1)).toBeVisible();
  expect(selectedIds).toHaveLength(2);

  await page.locator(`[data-media-id="${selectedIds[0]}"] .card-button`).click();
  await expect.poll(() => page.locator("#video-player").getAttribute("src")).toMatch(/web\/media\//);
  await page.locator(`[data-media-id="${selectedIds[1]}"] .card-button`).click();
  await expect.poll(() => enrichmentStarted).toBe(true);
  expect(await page.locator("#video-player").evaluate((video) => ({
    paused: video.paused,
    source: video.getAttribute("src"),
  }))).toEqual({ paused: true, source: null });

  releaseEnrichment();
  await expect.poll(() => page.locator("#video-player").getAttribute("src")).toMatch(/web\/media\//);
});

test("queue snapshot crosses pagination, auto-advances, and survives navigation", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chromium", "portrait phones intentionally hide the previous-item control");
  // Queue transitions should not race an in-flight smooth scroll to the player.
  await page.emulateMedia({ reducedMotion: "reduce" });
  await usePreference(page, "autoplay", "true");
  await serveFixtureMedia(page);
  await page.route("**/api/web/transcode/*", (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ schema_version: 2, item_id: "7", request_id: 1, state: "complete", retry_after_seconds: null }),
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
      id: String(BigInt(base.id) + 10_000n + BigInt(index)),
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
  // Finish replacing the large grid before positioning the player controls.
  await expect(page.locator("#loading")).toBeHidden();
  await expect(page.locator(".media-card.folder").first()).toBeVisible();
  await showPlayerControls(page);
  await page.locator("#previous-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText("Queue 60");
});

test("a stale queue page cannot replace a newer queue snapshot", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "mobile-chromium", "the browser-independent queue race is covered by the desktop projects");
  await page.addInitScript(() => {
    Object.defineProperty(window, "IntersectionObserver", {
      configurable: true,
      value: class {
        observe() {}
        unobserve() {}
        disconnect() {}
      },
    });
    const nativeFetch = window.fetch.bind(window);
    let releaseFirst;
    const firstGate = new Promise((resolve) => { releaseFirst = resolve; });
    let tailRequests = 0;
    let firstResponseSettled = false;
    window.__queueRace = {
      count: () => tailRequests,
      releaseFirst: () => releaseFirst(),
      firstResponseSettled: () => firstResponseSettled,
    };
    window.fetch = async (input, options = {}) => {
      const url = new URL(typeof input === "string" ? input : input.url, document.baseURI);
      if (url.pathname === "/api/web/library"
        && url.searchParams.get("offset") === "60"
        && url.searchParams.get("limit") !== "200") {
        // Clicking the last card can also trigger ordinary infinite paging.
        // Hold it so both selections must take an incomplete queue snapshot.
        await firstGate;
      }
      if (url.pathname === "/api/web/library"
        && url.searchParams.get("offset") === "60"
        && url.searchParams.get("limit") === "200") {
        // Deliberately ignore AbortSignal so the controller/epoch guards, rather
        // than fetch cancellation, must reject the stale completion.
        const response = await nativeFetch(input, { ...options, signal: undefined });
        const payload = await response.json();
        const requestOrdinal = ++tailRequests;
        payload.entries[0].title = requestOrdinal === 1 ? "Stale queue tail" : "Current queue tail";
        payload.entries[0].file_name = `${payload.entries[0].title}.mp4`;
        if (requestOrdinal === 1) await firstGate;
        const wrappedResponse = new Response(JSON.stringify(payload), {
          status: response.status,
          headers: { "Content-Type": "application/json" },
        });
        if (requestOrdinal === 1) {
          return Promise.resolve(wrappedResponse).finally(() => { firstResponseSettled = true; });
        }
        return wrappedResponse;
      }
      return nativeFetch(input, options);
    };
  });
  await serveFixtureMedia(page);
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
      id: String(BigInt(base.id) + 20_000n + BigInt(index)),
      title: `Queue race ${index}`,
      file_name: `queue-race-${index}.mp4`,
    });
    const entries = offset === 0
      ? Array.from({ length: 60 }, (_, index) => make(index + 1))
      : offset === 60 ? [make(61)] : [];
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        ...firstPayload,
        offset,
        limit: 60,
        total: 61,
        has_more: offset + entries.length < 61,
        entries,
      }),
    });
  });

  await page.goto("/?view=video");
  const last = page.getByRole("button", { name: /^Play Queue race 60\b/ });
  await last.click();
  await expect.poll(() => page.evaluate(() => window.__queueRace.count())).toBe(1);
  await last.click();
  await expect.poll(() => page.evaluate(() => window.__queueRace.count())).toBe(2);
  await expect(page.locator("#next-button")).toHaveAttribute("title", "Next: Current queue tail");

  await page.evaluate(() => window.__queueRace.releaseFirst());
  await expect.poll(() => page.evaluate(() => window.__queueRace.firstResponseSettled())).toBe(true);
  await expect(page.locator("#next-button")).toHaveAttribute("title", "Next: Current queue tail");
});

test("already-complete broken artwork shows the fallback and releases its loading slots", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 1000 });
  await page.addInitScript(() => {
    // Model a cached failure whose complete flag precedes its queued error event.
    const complete = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, "complete").get;
    Object.defineProperty(HTMLImageElement.prototype, "complete", {
      configurable: true,
      get() { return this.getAttribute("src")?.startsWith("/Thumbnails/") ? true : complete.call(this); },
    });
  });
  let releaseArtwork;
  const held = new Promise((resolve) => { releaseArtwork = resolve; });
  await page.route("**/Thumbnails/**", async (route) => {
    await held;
    await route.fulfill({ status: 404 }).catch(() => {});
  });
  try {
    await openLibrary(page);
    await openVideoView(page);
    const failedImages = page.locator('#media-grid img[src^="/Thumbnails/"].failed');
    await expect.poll(() => failedImages.count()).toBeGreaterThan(3);
    await expect(failedImages.first()).toBeHidden();
    expect(await failedImages.evaluateAll((images) => images.every((image) => image.naturalWidth === 0))).toBe(true);
  } finally {
    releaseArtwork();
  }
});

test("slow artwork from an abandoned view cannot block the current view", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 1000 });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    const view = new URL(route.request().url()).searchParams.get("kind");
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media") entry.art_url = `/review-art/${view}/${entry.id}.jpg`;
    }
    await route.fulfill({ response, json: payload });
  });
  let releaseArtwork;
  const held = new Promise((resolve) => { releaseArtwork = resolve; });
  const requested = [];
  await page.route("**/review-art/**", async (route) => {
    requested.push(route.request().url());
    await held;
    await route.fulfill({ status: 404 }).catch(() => {});
  });
  try {
    await openLibrary(page);
    await openVideoView(page);
    await expect.poll(() => requested.filter((url) => url.includes("/video/")).length).toBe(4);
    await page.getByRole("tab", { name: "Audio", exact: true }).click();
    await expect.poll(() => requested.filter((url) => url.includes("/audio/")).length).toBeGreaterThan(0);
  } finally {
    releaseArtwork();
  }
});

test("infinite scroll loads each bounded page once and stops at the catalog end", async ({ page }) => {
  let firstPayload = null;
  const requestedOffsets = [];
  let artworkActive = 0;
  let artworkMaximum = 0;
  let artworkRequests = 0;
  let releaseArtwork;
  const artworkGate = new Promise((resolve) => { releaseArtwork = resolve; });
  await page.route(/\/(?:AlbumArt|Resized|Thumbnails)\//, async (route) => {
    artworkActive += 1;
    artworkRequests += 1;
    artworkMaximum = Math.max(artworkMaximum, artworkActive);
    await artworkGate;
    await route.fulfill({ status: 404, contentType: "image/jpeg", body: "" });
    artworkActive -= 1;
  });
  await page.route("**/api/web/library?**", async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("view") !== "library" || url.searchParams.get("kind") !== "video") {
      return route.fallback();
    }
    const offset = Number(url.searchParams.get("offset") || 0);
    requestedOffsets.push(offset);
    if (!firstPayload) {
      const response = await route.fetch();
      firstPayload = await response.json();
    }
    const base = firstPayload.entries.find((entry) => entry.entry_type === "media");
    const make = (index) => ({
      ...base,
      id: String(BigInt(base.id) + 30_000n + BigInt(index)),
      title: `Infinite ${index}`,
      file_name: `infinite-${index}.mp4`,
      art_url: `/AlbumArt/infinite-${index}.jpg`,
    });
    const pageSize = offset === 48 ? 1 : 24;
    const entries = offset <= 48
      ? Array.from({ length: pageSize }, (_, index) => make(offset + index + 1))
      : [];
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        ...firstPayload,
        offset,
        limit: 24,
        total: 49,
        has_more: offset + entries.length < 49,
        entries,
      }),
    });
  });

  await page.goto("/?view=video", { waitUntil: "domcontentloaded" });
  await expect(page.locator("[data-media-id]")).toHaveCount(24);
  await expect(page.locator("#load-more")).toHaveCount(0);
  const sentinel = page.locator("#load-more-sentinel");
  await expect(sentinel).toBeAttached();
  const firstArtwork = page.locator(".media-card img").first();
  // The application admits nearby images itself; offscreen images must stay
  // unrequested regardless of the browser's native lazy-loading heuristic.
  expect(await page.locator('.media-card img[src]').count()).toBeLessThanOrEqual(4);
  await expect(page.locator('.media-card img').last()).not.toHaveAttribute("src");
  await expect(firstArtwork).toHaveAttribute("decoding", "async");
  await expect(firstArtwork).toHaveAttribute("fetchpriority", "low");

  const lastInitialCard = page.getByRole("button", { name: /^Play Infinite 24\b/ });
  await lastInitialCard.evaluate((card) => {
    document.getElementById("load-more-sentinel").scrollIntoView({ block: "end" });
    card.focus({ preventScroll: true });
  });
  await expect(page.locator("[data-media-id]")).toHaveCount(48);
  await expect(lastInitialCard).toBeFocused();
  await expect.poll(() => lastInitialCard.evaluate((card) => {
    const bounds = card.getBoundingClientRect();
    return bounds.bottom > 0 && bounds.top < window.innerHeight;
  })).toBe(true);
  expect(requestedOffsets).toEqual([0, 24]);

  await lastInitialCard.scrollIntoViewIfNeeded();
  await page.mouse.wheel(0, 1);
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.evaluate(() => document.getElementById("load-more-sentinel").scrollIntoView({ block: "end" }));
  await expect(page.locator("[data-media-id]")).toHaveCount(49);
  await expect(sentinel).toBeHidden();
  expect(requestedOffsets).toEqual([0, 24, 48]);
  await expect.poll(() => artworkRequests).toBe(4);
  expect(artworkMaximum).toBeLessThanOrEqual(4);
  releaseArtwork();
  await page.waitForTimeout(100);
  await expect.poll(() => artworkActive).toBe(0);
});

test("paging continues when WebKit omits the sentinel exit after appending cards", async ({ page }) => {
  await page.addInitScript(() => {
    let pagingCallback = null;
    let sentinel = null;
    window.__sparsePagingObserver = {
      intersect() {
        pagingCallback?.([{ target: sentinel, isIntersecting: true }]);
      },
    };
    Object.defineProperty(window, "IntersectionObserver", {
      configurable: true,
      value: class SparseIntersectionObserver {
        constructor(nextCallback, options = {}) {
          this.paging = options.rootMargin === "800px 0px";
          if (this.paging) pagingCallback = nextCallback;
        }

        observe(target) {
          if (this.paging && target.id === "load-more-sentinel") sentinel = target;
        }

        unobserve() {}
        disconnect() {}
      },
    });
  });
  let firstPayload = null;
  const requestedOffsets = [];
  await page.route("**/api/web/library?**", async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("view") !== "library" || url.searchParams.get("kind") !== "video") {
      return route.fallback();
    }
    const offset = Number(url.searchParams.get("offset") || 0);
    requestedOffsets.push(offset);
    if (!firstPayload) {
      const response = await route.fetch();
      firstPayload = await response.json();
    }
    const base = firstPayload.entries.find((entry) => entry.entry_type === "media");
    const count = offset === 48 ? 1 : 24;
    const entries = Array.from({ length: count }, (_, index) => ({
      ...base,
      id: String(BigInt(base.id) + 50_000n + BigInt(offset + index)),
      title: `Sparse observer ${offset + index + 1}`,
      art_url: null,
    }));
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        ...firstPayload,
        offset,
        limit: 24,
        total: 49,
        has_more: offset + entries.length < 49,
        entries,
      }),
    });
  });

  await page.goto("/?view=video", { waitUntil: "domcontentloaded" });
  await expect(page.locator("[data-media-id]")).toHaveCount(24);
  await page.locator("#load-more-sentinel").evaluate((sentinel) => {
    sentinel.scrollIntoView({ block: "end" });
    window.__sparsePagingObserver.intersect();
  });
  await expect(page.locator("[data-media-id]")).toHaveCount(48);
  expect(requestedOffsets).toEqual([0, 24]);
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.locator("#load-more-sentinel").evaluate((sentinel) => {
    sentinel.scrollIntoView({ block: "end" });
    window.dispatchEvent(new WheelEvent("wheel", { deltaY: 1 }));
  });
  await expect(page.locator("[data-media-id]")).toHaveCount(49);
  expect(requestedOffsets).toEqual([0, 24, 48]);
});

test("a generation change during infinite scroll is recoverable without duplicate cards", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(window, "IntersectionObserver", {
      configurable: true,
      value: undefined,
    });
  });
  let generationChanged = false;
  let markGenerationChangeRequested;
  const generationChangeRequested = new Promise((resolve) => { markGenerationChangeRequested = resolve; });
  let releaseGenerationChange;
  const generationChangeGate = new Promise((resolve) => { releaseGenerationChange = resolve; });
  await page.route("**/api/web/library?**", async (route) => {
    const url = new URL(route.request().url());
    const offset = Number(url.searchParams.get("offset") || 0);
    if (offset > 0 && !generationChanged) {
      generationChanged = true;
      markGenerationChangeRequested();
      await generationChangeGate;
      return route.fulfill({
        status: 409,
        contentType: "application/json",
        body: JSON.stringify({ schema_version: 2, error: { code: "catalog_changed", message: "changed", recoverable: true, action: "retry_library" } }),
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
  await generationChangeRequested;
  await expect.poll(() => page.locator("[data-media-id]").count()).toBeGreaterThan(0);
  const before = await page.locator("[data-media-id]").count();
  releaseGenerationChange();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", "error");
  expect(await page.locator("[data-media-id]").count()).toBe(before);
  await page.locator("#library-retry").click();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
  const ids = await page.locator("[data-media-id]").evaluateAll((cards) => cards.map((card) => card.dataset.mediaId));
  expect(new Set(ids).size).toBe(ids.length);
});

test("Compatible loop restarts the whole title after a seek and takes precedence over auto-advance", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await usePreference(page, "autoplay", "true");
  await disableFragmentedDelivery(page);
  await page.addInitScript(() => {
    const sources = new WeakMap();
    Object.defineProperty(HTMLMediaElement.prototype, "src", {
      configurable: true,
      get() { return sources.get(this) || ""; },
      set(value) { sources.set(this, value); },
    });
    HTMLMediaElement.prototype.load = () => {};
    HTMLMediaElement.prototype.play = () => Promise.resolve();
  });
  await page.route("**/api/web/library?**", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    for (const entry of payload.entries || []) {
      if (entry.entry_type === "media") entry.duration_seconds = 600;
    }
    await route.fulfill({ response, json: payload });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  const video = page.locator("#video-player");
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("start=0");
  await page.locator("#timeline").evaluate((timeline) => {
    timeline.value = "25";
    timeline.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("start=20");
  await page.locator("#loop-button").evaluate((button) => button.click());
  await expect(page.locator("#loop-button")).toHaveAttribute("aria-pressed", "true");
  expect(await video.evaluate((player) => player.loop)).toBe(false);
  await video.evaluate((player) => {
    Object.defineProperty(player, "currentTime", { configurable: true, value: 580 });
    player.dispatchEvent(new Event("ended"));
  });
  await expect.poll(() => video.evaluate((player) => player.src)).toContain("start=0");
  await expect(page.locator("#now-playing-title")).toHaveText("tagged");
  await expect(page.locator("#timeline")).toHaveValue("0");
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
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
  await expect(page.locator("#mode-label")).toHaveText(/^(Repackaging|Converting audio|Re-encoding video)$/);
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.locator("#technical-message")).not.toContainText("MediaItems");
});

test("compatible seek while paused clears its busy description on metadata", async ({ page }) => {
  await serveFixtureMedia(page);
  let releaseMedia;
  const held = new Promise((resolve) => { releaseMedia = resolve; });
  let requests = 0;
  await page.route("**/web/media/*.mp4?**", async (route) => {
    requests += 1;
    await held;
    await route.fallback();
  });
  await openLibrary(page);
  await openVideoView(page);
  try {
    await page.getByRole("button", { name: /^Play dvp7\b/ }).first().click();
    await expect(page.locator("#timeline")).toHaveAttribute("max", "10");
    await expect.poll(() => requests).toBe(1);
    // Pause before the tiny fixture can finish and turn the action into Replay.
    await showPlayerControls(page);
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Pause");
    await page.locator("#play-button").click();
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
    await page.locator("#timeline").evaluate((timeline) => {
      timeline.value = "5";
      timeline.dispatchEvent(new Event("input", { bubbles: true }));
      timeline.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await expect(page.locator("#timeline")).toHaveAttribute("aria-busy", "true");
    await expect.poll(() => requests).toBe(2);
  } finally {
    releaseMedia();
  }
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await expect(page.locator("#timeline")).toHaveAttribute("aria-busy", "false");
  await expect(page.locator("#timeline-status")).not.toContainText("Starting a prepared stream");
});

test("iPhone expands the in-page player instead of surrendering controls to native fullscreen", async ({ page }) => {
  await installIphoneVideoFullscreen(page);
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  const button = page.locator("#fullscreen-button");
  await page.locator("#player-stage").dispatchEvent("pointerdown", {
    pointerType: "touch",
    isPrimary: true,
  });
  await expect(button).toBeEnabled();
  await button.evaluate((element) => element.click());
  await expect.poll(() => page.evaluate(() => window.__iphoneFullscreenTest.counts()))
    .toEqual({ enters: 0, exits: 0, stageEnters: 0 });
  await expect(page.locator("#player-stage")).toHaveClass(/expanded-player/);
  await expect(page.locator("body")).toHaveClass(/player-expanded/);
  await expect(button).toHaveAttribute("aria-label", "Exit expanded player");
  await expect(button).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator("#timeline")).toBeVisible();
  expect(await page.locator("#player-stage").evaluate((stage) => {
    const bounds = stage.getBoundingClientRect();
    return Math.abs(bounds.top - visualViewport.offsetTop) < 1
      && Math.abs(bounds.left - visualViewport.offsetLeft) < 1
      && Math.abs(bounds.width - visualViewport.width) < 1
      && Math.abs(bounds.height - visualViewport.height) < 1;
  })).toBe(true);
  await page.evaluate(() => window.__iphoneFullscreenTest.resizeViewport({
    offsetLeft: 7,
    offsetTop: 31,
    width: 880,
    height: 420,
  }));
  await expect.poll(() => page.locator("#player-stage").evaluate((stage) => {
    const bounds = stage.getBoundingClientRect();
    return [bounds.left, bounds.top, bounds.width, bounds.height].map(Math.round);
  })).toEqual([7, 31, 880, 420]);
  await expect(page.locator("#player-message")).toBeHidden();

  await button.evaluate((element) => element.click());
  await expect.poll(() => page.evaluate(() => window.__iphoneFullscreenTest.counts()))
    .toEqual({ enters: 0, exits: 0, stageEnters: 0 });
  await expect(page.locator("#player-stage")).not.toHaveClass(/expanded-player/);
  await expect(page.locator("body")).not.toHaveClass(/player-expanded/);
  await expect(button).toHaveAttribute("aria-label", "Expand player");
  await expect(button).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator("#player-message")).toBeHidden();
});

test("iPhone expanded playback holds and releases the screen wake lock", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await installIphoneUserAgent(page);
  await installDeferredWakeLock(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  const button = page.locator("#fullscreen-button");
  await expect(button).toHaveAttribute("aria-label", "Expand player");
  await button.evaluate((control) => control.click());
  await expect(page.locator("#player-stage")).toHaveClass(/expanded-player/);
  await page.locator("#video-player").dispatchEvent("playing");
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().requests)).toBe(1);
  await page.evaluate(() => window.__wakeLockTest.resolveNext());
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().pending)).toBe(0);

  await button.evaluate((control) => control.click());
  await expect(page.locator("#player-stage")).not.toHaveClass(/expanded-player/);
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().releases)).toBe(1);
});

test("fullscreen controls remain reachable and Escape exits", async ({ page, browserName, isMobile }) => {
  test.skip(browserName === "webkit" && process.platform === "linux", "WebKitGTK headless does not expose the Fullscreen API");
  test.skip(isMobile, "desktop fullscreen control surface");
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
  await expect(page.locator("#mode-label")).toHaveText(/^(Repackaging|Converting audio|Re-encoding video)$/);
  await page.locator('#advanced-playback-dialog button[value="close"]').click();
  await showPlayerControls(page);
  await page.locator("#fullscreen-button").click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe("player-stage");
  await expect(page.locator("#captions-button")).toBeVisible();
  await expect(page.locator("#volume-control")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe(null);
});

test("pointer fullscreen releases Safari-style incidental focus and locks page scroll", async ({ page }) => {
  await page.addInitScript(() => {
    const nativeSetTimeout = window.setTimeout.bind(window);
    window.setTimeout = (callback, delay = 0, ...args) => nativeSetTimeout(
      callback,
      delay === 3_000 ? 250 : delay,
      ...args,
    );
    let fullscreenElement = null;
    Object.defineProperty(document, "fullscreenEnabled", { configurable: true, value: true });
    Object.defineProperty(document, "fullscreenElement", {
      configurable: true,
      get: () => fullscreenElement,
    });
    Object.defineProperty(Element.prototype, "requestFullscreen", {
      configurable: true,
      value() {
        fullscreenElement = this;
        if (this instanceof HTMLElement) this.focus();
        document.dispatchEvent(new Event("fullscreenchange"));
        return Promise.resolve();
      },
    });
    Object.defineProperty(document, "exitFullscreen", {
      configurable: true,
      value() {
        fullscreenElement = null;
        document.dispatchEvent(new Event("fullscreenchange"));
        return Promise.resolve();
      },
    });
  });
  await openLibrary(page);
  await selectTaggedVideo(page);
  await showPlayerControls(page);
  await page.locator("#fullscreen-button").evaluate((button) => {
    button.dispatchEvent(new MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      detail: 1,
    }));
  });
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null))
    .toBe("player-stage");
  await expect(page.locator("html")).toHaveClass(/player-expanded/);
  await expect(page.locator("body")).toHaveClass(/player-expanded/);
  await expect.poll(() => page.evaluate(() => document.activeElement?.id || ""))
    .not.toBe("player-stage");
  await expect(page.locator("#player-stage")).not.toHaveClass(/controls-visible/);
  expect(await page.locator("body").evaluate((body) => getComputedStyle(body).overflow)).toBe("hidden");

  await page.keyboard.press("Escape");
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe(null);
  await expect(page.locator("html")).not.toHaveClass(/player-expanded/);
  await expect(page.locator("body")).not.toHaveClass(/player-expanded/);
});

test("a delayed fullscreen rejection cannot fail a newer playback session", async ({ page }) => {
  await installDeferredDisplayModeRequests(page);
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video");
  const secondTitle = (await cards.nth(1).locator(".card-title").textContent()).trim();
  await cards.nth(0).locator(".card-button").click();
  await showPlayerControls(page);
  await page.locator("#fullscreen-button").evaluate((button) => button.click());
  await expect.poll(() => page.evaluate(() => window.__displayModeTest.pending("fullscreen"))).toBe(1);

  await cards.nth(1).locator(".card-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#player-message")).toBeHidden();
  await page.evaluate(() => window.__displayModeTest.reject("fullscreen"));
  await page.waitForTimeout(0);

  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.locator("#player-message-text")).toHaveText("");
});

test("a delayed picture-in-picture rejection cannot fail a newer playback session", async ({ page }) => {
  await installDeferredDisplayModeRequests(page);
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video");
  const secondTitle = (await cards.nth(1).locator(".card-title").textContent()).trim();
  await cards.nth(0).locator(".card-button").click();
  await showPlayerControls(page);
  await expect(page.locator("#pip-button")).toBeEnabled();
  await page.locator("#pip-button").evaluate((button) => button.click());
  await expect.poll(() => page.evaluate(() => window.__displayModeTest.pending("pip"))).toBe(1);

  await cards.nth(1).locator(".card-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#player-message")).toBeHidden();
  await page.evaluate(() => window.__displayModeTest.reject("pip"));
  await page.waitForTimeout(0);

  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.locator("#player-message-text")).toHaveText("");
});

test("late picture-in-picture events cannot mutate a newer playback session", async ({ page }) => {
  await installDeferredDisplayModeRequests(page);
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video");
  const secondTitle = (await cards.nth(1).locator(".card-title").textContent()).trim();
  await cards.nth(0).locator(".card-button").click();
  await showPlayerControls(page);
  await page.locator("#pip-button").evaluate((button) => button.click());
  await expect.poll(() => page.evaluate(() => window.__displayModeTest.pending("pip"))).toBe(1);

  await cards.nth(1).locator(".card-button").click();
  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await page.locator("#video-player").dispatchEvent("enterpictureinpicture");
  await page.evaluate(() => window.__displayModeTest.resolve("pip"));
  await page.locator("#video-player").dispatchEvent("leavepictureinpicture");
  await page.waitForTimeout(0);

  await expect(page.locator("#now-playing-title")).toHaveText(secondTitle);
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator("#player-message")).toBeHidden();
});

test("pending picture-in-picture follows a same-title Compatible source restart", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await installDeferredDisplayModeRequests(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  await showPlayerControls(page);
  const source = page.locator("#video-player");
  await expect.poll(() => source.evaluate((video) => video.src)).toMatch(/request=/);
  const firstRequest = new URL(await source.evaluate((video) => video.src)).searchParams.get("request");

  await page.locator("#pip-button").evaluate((button) => button.click());
  await expect.poll(() => page.evaluate(() => window.__displayModeTest.pending("pip"))).toBe(1);
  await openAdvancedPlayback(page);
  await page.locator("#quality-control").selectOption("low_360");
  await expect.poll(async () => new URL(await source.evaluate((video) => video.src)).searchParams.get("request"))
    .not.toBe(firstRequest);

  await page.evaluate(() => window.__displayModeTest.setPictureInPicture(true));
  await source.dispatchEvent("enterpictureinpicture");
  await page.evaluate(() => window.__displayModeTest.resolve("pip"));
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "true");

  await page.evaluate(() => window.__displayModeTest.setPictureInPicture(false));
  await source.dispatchEvent("leavepictureinpicture");
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "false");
});

test("active picture-in-picture follows a same-title Compatible source restart", async ({ page }) => {
  await usePreference(page, "stream", "compat");
  await disableFragmentedDelivery(page);
  await installDeferredDisplayModeRequests(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  await showPlayerControls(page);
  const source = page.locator("#video-player");
  await expect.poll(() => source.evaluate((video) => video.src)).toMatch(/request=/);

  await page.locator("#pip-button").evaluate((button) => button.click());
  await expect.poll(() => page.evaluate(() => window.__displayModeTest.pending("pip"))).toBe(1);
  await page.evaluate(() => window.__displayModeTest.setPictureInPicture(true));
  await source.dispatchEvent("enterpictureinpicture");
  await page.evaluate(() => window.__displayModeTest.resolve("pip"));
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "true");
  const firstRequest = new URL(await source.evaluate((video) => video.src)).searchParams.get("request");

  await openAdvancedPlayback(page);
  await page.locator("#quality-control").selectOption("low_360");
  await expect.poll(async () => new URL(await source.evaluate((video) => video.src)).searchParams.get("request"))
    .not.toBe(firstRequest);
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "true");

  await page.evaluate(() => window.__displayModeTest.setPictureInPicture(false));
  await source.dispatchEvent("leavepictureinpicture");
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "false");
});

test("a wake lock acquired after pause is released without reacquiring", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await installDeferredWakeLock(page);
  await startDeferredWakeLockRequest(page);

  await page.locator("#video-player").dispatchEvent("pause");
  await expect(page.locator("#play-button")).toHaveAttribute("aria-label", "Play");
  await page.evaluate(() => window.__wakeLockTest.resolveNext());
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().releases)).toBe(1);
  await page.waitForTimeout(100);
  expect(await page.evaluate(() => window.__wakeLockTest.counts())).toEqual({ requests: 1, releases: 1, pending: 0 });
});

test("a wake lock acquired after a media error is released without reacquiring", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await installDeferredWakeLock(page);
  await startDeferredWakeLockRequest(page);

  await page.locator("#video-player").dispatchEvent("error");
  await page.evaluate(() => window.__wakeLockTest.resolveNext());
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().releases)).toBe(1);
  await expect(page.locator("#player-message[role=alert]")).toBeVisible();
  await page.waitForTimeout(100);
  expect(await page.evaluate(() => window.__wakeLockTest.counts())).toEqual({ requests: 1, releases: 1, pending: 0 });
});

test("a denied wake lock is not retried by playback updates until eligibility resets", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await installDeferredWakeLock(page);
  await startDeferredWakeLockRequest(page);

  await page.evaluate(() => window.__wakeLockTest.rejectNext());
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().pending)).toBe(0);
  for (let index = 0; index < 5; index += 1) {
    await page.locator("#video-player").dispatchEvent("timeupdate");
  }
  await page.waitForTimeout(100);
  expect(await page.evaluate(() => window.__wakeLockTest.counts())).toEqual({ requests: 1, releases: 0, pending: 0 });

  await page.locator("#video-player").dispatchEvent("pause");
  await page.locator("#video-player").dispatchEvent("playing");
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().requests)).toBe(2);
});

test("an externally released wake lock waits for eligibility to reset before retrying", async ({ page }) => {
  await usePreference(page, "stream", "direct");
  await installDeferredWakeLock(page);
  await startDeferredWakeLockRequest(page);

  await page.evaluate(() => window.__wakeLockTest.resolveNext());
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().pending)).toBe(0);
  await page.evaluate(() => window.__wakeLockTest.releaseCurrent());
  for (let index = 0; index < 5; index += 1) {
    await page.locator("#video-player").dispatchEvent("timeupdate");
  }
  await page.waitForTimeout(100);
  expect(await page.evaluate(() => window.__wakeLockTest.counts())).toEqual({ requests: 1, releases: 1, pending: 0 });

  await page.locator("#video-player").dispatchEvent("pause");
  await page.locator("#video-player").dispatchEvent("playing");
  await expect.poll(() => page.evaluate(() => window.__wakeLockTest.counts().requests)).toBe(2);
});

test("automated accessibility scan has no serious violations", async ({ page }) => {
  await openLibrary(page);
  await openVideoView(page);
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21aa"]).analyze();
  const serious = results.violations.filter((violation) => ["serious", "critical"].includes(violation.impact));
  expect(serious).toEqual([]);
});

test("asynchronous library and playback states use deduplicated polite live regions", async ({ page }) => {
  // Keep this announcement test in control of playback state. Other behavior
  // tests cover the immediate play request made while title-selection user
  // activation is still available.
  await page.addInitScript(() => {
    HTMLMediaElement.prototype.play = () => Promise.resolve();
  });
  let releaseLibrary;
  const libraryGate = new Promise((resolve) => { releaseLibrary = resolve; });
  let libraryRequestStarted;
  const libraryStarted = new Promise((resolve) => { libraryRequestStarted = resolve; });
  await page.route("**/api/web/library?**", async (route) => {
    libraryRequestStarted();
    await libraryGate;
    await route.fallback();
  });
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await libraryStarted;

  const libraryLive = page.locator("#library-live");
  await expect(libraryLive).toHaveAttribute("role", "status");
  await expect(libraryLive).toHaveAttribute("aria-live", "polite");
  await expect(libraryLive).toHaveAttribute("aria-atomic", "true");
  await expect(libraryLive).toHaveText("Connecting to the library.");
  await expect(page.locator("#loading")).not.toHaveAttribute("role", "status");
  await page.evaluate(() => {
    window.__liveMessages = { library: [], playback: [] };
    for (const [kind, id] of [["library", "library-live"], ["playback", "playback-live"]]) {
      const target = document.getElementById(id);
      new MutationObserver(() => {
        const message = target.textContent.trim();
        if (message) window.__liveMessages[kind].push(message);
      }).observe(target, { childList: true, characterData: true, subtree: true });
    }
  });
  releaseLibrary();
  await expect(page.locator("#server-state")).toHaveAttribute("data-state", /ready|empty/);
  await expect(libraryLive).toHaveText(/^Library ready\./);

  await usePreference(page, "stream", "direct");
  const fixture = await readFile(compatibleFixture);
  let releaseMedia;
  const mediaGate = new Promise((resolve) => { releaseMedia = resolve; });
  await page.route("**/web/media/*.mp4?**", async (route) => {
    await mediaGate;
    await route.fulfill({ status: 200, contentType: "video/mp4", body: fixture });
  });
  await openVideoView(page);
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  const playbackLive = page.locator("#playback-live");
  await expect(playbackLive).toHaveAttribute("role", "status");
  await expect(playbackLive).toHaveAttribute("aria-live", "polite");
  await expect(playbackLive).toHaveAttribute("aria-atomic", "true");
  await expect(playbackLive).toHaveText("Loading playback.");
  await expect(page.locator("#player-message")).toBeHidden();

  for (let index = 0; index < 3; index += 1) {
    await page.locator("#video-player").dispatchEvent("waiting");
  }
  await expect(playbackLive).toHaveText("Playback is buffering.");
  for (let index = 0; index < 3; index += 1) {
    await page.locator("#video-player").dispatchEvent("playing");
  }
  await expect(playbackLive).toHaveText("Playing tagged.");
  for (let index = 0; index < 3; index += 1) {
    await page.locator("#video-player").dispatchEvent("stalled");
  }
  await expect(playbackLive).toHaveText("Playing tagged.");
  await expect(page.locator("#player-message")).toBeHidden();

  const messages = await page.evaluate(() => window.__liveMessages);
  expect(messages.library.filter((message) => message.startsWith("Library ready.")).length).toBeGreaterThan(0);
  expect(new Set(messages.library).size).toBe(messages.library.length);
  expect(messages.playback.filter((message) => message === "Loading playback.")).toHaveLength(1);
  expect(messages.playback.filter((message) => message === "Playback is buffering.")).toHaveLength(1);
  expect(messages.playback.filter((message) => message === "Playing tagged.")).toHaveLength(1);
  releaseMedia();
});

test("300px layout keeps Previous and Next keyboard reachable", async ({ page, isMobile }) => {
  test.skip(isMobile, "narrow-width Previous/Next wrapping is a keyboard layout");
  await page.setViewportSize({ width: 300, height: 700 });
  await serveFixtureMedia(page);
  await openLibrary(page);
  await openVideoView(page);
  const cards = page.locator(".media-card.video .card-button");
  const count = await cards.count();
  expect(count).toBeGreaterThan(2);
  await cards.nth(Math.floor(count / 2)).click();
  await showPlayerControls(page);

  const previous = page.locator("#previous-button");
  const next = page.locator("#next-button");
  await expect(previous).toBeVisible();
  await expect(previous).toBeEnabled();
  await expect(next).toBeVisible();
  await expect(next).toBeEnabled();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);
  for (const control of [previous, next]) {
    const bounds = await control.boundingBox();
    expect(bounds.width).toBeGreaterThanOrEqual(43.99);
    expect(bounds.height).toBeGreaterThanOrEqual(43.99);
    expect(bounds.x).toBeGreaterThanOrEqual(0);
    expect(bounds.x + bounds.width).toBeLessThanOrEqual(300);
  }

  await page.locator("#play-button").focus();
  await page.keyboard.press("Tab");
  await expect(previous).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(next).toBeFocused();
});

test("portrait phone player uses the compact landscape toolbar without overlapping time", async ({ page, isMobile }) => {
  test.skip(!isMobile, "portrait compact toolbar belongs to mobile coverage");
  await page.setViewportSize({ width: 390, height: 844 });
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);
  await page.locator("#player-stage").dispatchEvent("pointerdown", {
    pointerType: "touch",
    isPrimary: true,
  });
  await expect(page.locator("#playback-controls")).toBeVisible();
  await expect(page.locator("#previous-button")).toBeHidden();
  await expect(page.locator("#next-button")).toBeHidden();
  await expect(page.locator("#stream-info-button")).toBeHidden();
  await expect(page.locator("#play-button")).toBeVisible();
  await expect(page.locator("#mute-button")).toBeVisible();
  await expect(page.locator("#close-player-button")).toBeVisible();
  await expect(page.locator("#volume-control")).toBeHidden();
  await expect(page.locator("#timeline-current")).toBeVisible();
  await expect(page.locator("#captions-button")).toBeVisible();
  await expect(page.locator("#fullscreen-button")).toBeVisible();
  expect(await playerToolbarOverlap(page)).toEqual(expect.objectContaining({ overlapping: [] }));
});

test("iPhone expanded portrait keeps time clear of the compact toolbar", async ({ page, isMobile }) => {
  test.skip(!isMobile, "iPhone expanded portrait toolbar belongs to mobile coverage");
  await page.setViewportSize({ width: 390, height: 844 });
  await installIphoneVideoFullscreen(page);
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  const button = page.locator("#fullscreen-button");
  await page.locator("#player-stage").dispatchEvent("pointerdown", {
    pointerType: "touch",
    isPrimary: true,
  });
  await expect(button).toBeEnabled();
  await button.evaluate((element) => element.click());
  await expect(page.locator("#player-stage")).toHaveClass(/expanded-player/);
  await page.evaluate(() => window.__iphoneFullscreenTest.resizeViewport({
    offsetLeft: 0,
    offsetTop: 47,
    width: 390,
    height: 754,
  }));
  await expect.poll(() => page.locator("#player-stage").evaluate((stage) => {
    const bounds = stage.getBoundingClientRect();
    return [Math.round(bounds.width), Math.round(bounds.height)];
  })).toEqual([390, 754]);
  await expect(page.locator("#playback-controls")).toBeVisible();
  await expect(page.locator("#previous-button")).toBeHidden();
  await expect(page.locator("#next-button")).toBeHidden();
  await expect(page.locator("#stream-info-button")).toBeHidden();
  await expect(page.locator("#volume-control")).toBeHidden();
  await expect(page.locator("#mute-button")).toBeVisible();
  await expect(page.locator("#timeline-current")).toBeVisible();
  expect(await playerToolbarOverlap(page)).toEqual(expect.objectContaining({ overlapping: [] }));

  await page.setViewportSize({ width: 844, height: 390 });
  await page.evaluate(() => window.__iphoneFullscreenTest.resizeViewport({
    offsetLeft: 0,
    offsetTop: 0,
    width: 844,
    height: 390,
  }));
  await expect.poll(() => page.locator("#player-stage").evaluate((stage) => {
    const bounds = stage.getBoundingClientRect();
    return [Math.round(bounds.width), Math.round(bounds.height)];
  })).toEqual([844, 390]);
  expect(await page.locator(".volume-control").evaluate((control) => getComputedStyle(control).display))
    .toBe("none");
});

test("Close player leaves iPhone expanded playback for the library", async ({ page, isMobile }) => {
  test.skip(!isMobile, "iPhone expanded close belongs to mobile coverage");
  await page.setViewportSize({ width: 390, height: 844 });
  await installIphoneVideoFullscreen(page);
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  await page.locator("#player-stage").dispatchEvent("pointerdown", {
    pointerType: "touch",
    isPrimary: true,
  });
  await page.locator("#fullscreen-button").evaluate((element) => element.click());
  await expect(page.locator("#player-stage")).toHaveClass(/expanded-player/);
  await expect(page.locator("#close-player-button")).toBeVisible();
  await page.locator("#close-player-button").click();

  await expect(page.locator("#player-stage")).not.toHaveClass(/expanded-player/);
  await expect(page.locator("#app-main")).toHaveAttribute("data-layout", "browse");
  await expect(page.locator("#player-panel")).toBeHidden();
  await expect(page.locator("#now-playing-title")).toHaveText("Nothing selected");
  expect(new URL(page.url()).searchParams.has("item")).toBe(false);
});

test("mobile layout retains 44px targets without horizontal overflow", async ({ page, isMobile }) => {
  test.skip(!isMobile, "mobile project only");
  await openLibrary(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);
  let undersized = await undersizedTouchTargets(page, "button:visible, select:visible");
  expect(undersized).toEqual([]);

  await selectTaggedVideo(page);
  await page.locator("#player-stage").dispatchEvent("pointermove");
  undersized = await undersizedTouchTargets(
    page,
    "#player-stage button:visible, #player-stage select:visible, #player-stage input[type=range]:visible",
  );
  expect(undersized).toEqual([]);

  const title = await page.locator("#now-playing-title").textContent();
  await page.setViewportSize({ width: 915, height: 412 });
  await expect(page.locator("#now-playing-title")).toHaveText(title);
  await expect(page.locator("#fullscreen-button")).toBeVisible();
  await page.locator("#player-stage").dispatchEvent("pointermove");
  undersized = await undersizedTouchTargets(
    page,
    "#player-stage button:visible, #player-stage select:visible, #player-stage input[type=range]:visible",
  );
  expect(undersized).toEqual([]);
  await page.setViewportSize({ width: 412, height: 915 });
  await expect(page.locator("#now-playing-title")).toHaveText(title);
});

test("touch devices hide the volume slider including fullscreen", async ({ page, isMobile }) => {
  test.skip(!isMobile, "hardware volume belongs to mobile coverage");
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  const volumeDisplay = () => page.locator(".volume-control").evaluate((control) => getComputedStyle(control).display);

  expect(await volumeDisplay()).toBe("none");

  await page.setViewportSize({ width: 915, height: 412 });
  expect(await volumeDisplay()).toBe("none");

  await page.locator("#player-stage").dispatchEvent("pointerdown", {
    pointerType: "touch",
    isPrimary: true,
  });
  await expect(page.locator("#fullscreen-button")).toBeVisible();
  await page.locator("#fullscreen-button").click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null))
    .toBe("player-stage");
  expect(await volumeDisplay()).toBe("none");
});

test("Android landscape Watch mode keeps the inline player and library visible", async ({ page, isMobile }) => {
  test.skip(!isMobile, "Android landscape behavior belongs to mobile Chromium");
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  await page.setViewportSize({ width: 915, height: 412 });
  const stage = page.locator("#player-stage");
  await stage.dispatchEvent("pointerdown", { pointerType: "touch", isPrimary: true });
  await expect(page.locator("#playback-controls")).toBeVisible();

  const layout = await page.evaluate(() => {
    const topbar = document.querySelector(".topbar").getBoundingClientRect();
    const layoutSwitch = document.querySelector(".layout-switch").getBoundingClientRect();
    const layoutButtons = [...document.querySelectorAll(".layout-switch button")].map((button) => {
      const bounds = button.getBoundingClientRect();
      return [bounds.width, bounds.height];
    });
    const panel = document.querySelector("#player-panel").getBoundingClientRect();
    const stage = document.querySelector("#player-stage");
    const stageBounds = stage.getBoundingClientRect();
    const library = document.querySelector(".library");
    const libraryBounds = library.getBoundingClientRect();
    const mediaGrid = document.querySelector(".media-grid");
    const controls = ["#layout-browse", "#timeline", "#play-button", "#fullscreen-button"].map((selector) => {
      const control = document.querySelector(selector);
      const bounds = control.getBoundingClientRect();
      const x = bounds.left + bounds.width / 2;
      const y = bounds.top + bounds.height / 2;
      const hit = document.elementFromPoint(x, y);
      return {
        selector,
        bounds: [bounds.left, bounds.top, bounds.right, bounds.bottom],
        reachable: control === hit || control.contains(hit),
      };
    });
    return {
      aspectRatio: getComputedStyle(stage).aspectRatio,
      topbar: [topbar.top, topbar.bottom],
      layoutSwitch: [layoutSwitch.width, layoutSwitch.height],
      layoutButtons,
      topbarPosition: getComputedStyle(document.querySelector(".topbar")).position,
      panel: [panel.left, panel.top, panel.right, panel.bottom],
      stage: [stageBounds.left, stageBounds.top, stageBounds.right, stageBounds.bottom],
      library: [libraryBounds.left, libraryBounds.top, libraryBounds.right, libraryBounds.bottom],
      libraryOverflow: getComputedStyle(library).overflowY,
      mediaColumns: getComputedStyle(mediaGrid).gridTemplateColumns.split(" ").length,
      controls,
    };
  });
  expect(layout.aspectRatio).toBe("16 / 9");
  expect(layout.topbarPosition).toBe("static");
  expect(layout.topbar[0]).toBe(0);
  expect(layout.topbar[1]).toBeGreaterThanOrEqual(35);
  expect(layout.topbar[1]).toBeLessThanOrEqual(37);
  expect(layout.layoutSwitch[0]).toBeLessThan(115);
  expect(layout.layoutSwitch[1]).toBeGreaterThanOrEqual(44);
  expect(layout.layoutButtons.every(([width, height]) => width >= 44 && height >= 44)).toBe(true);
  expect(layout.panel[0]).toBeGreaterThanOrEqual(0);
  expect(layout.panel[1]).toBeGreaterThanOrEqual(layout.topbar[1]);
  expect(layout.panel[2]).toBeLessThan(layout.library[0]);
  expect(layout.panel[3]).toBeLessThanOrEqual(412);
  expect(layout.stage[3] - layout.stage[1]).toBeLessThan(320);
  expect(layout.library[1]).toBeGreaterThanOrEqual(layout.topbar[1]);
  expect(layout.library[2]).toBeLessThanOrEqual(915);
  expect(layout.library[3]).toBeLessThanOrEqual(412);
  expect(layout.libraryOverflow).toBe("auto");
  expect(layout.mediaColumns).toBe(2);
  expect(layout.controls.every(({ bounds, reachable }) => (
    reachable && bounds[0] >= 0 && bounds[1] >= 0
      && bounds[2] <= 915 && bounds[3] <= 412
  ))).toBe(true);

  await page.evaluate(() => {
    window.scrollBy(0, 120);
    window.scrollTo(0, 0);
  });
  await expect.poll(() => stage.evaluate((element) => {
    const bounds = element.getBoundingClientRect();
    return [bounds.left, bounds.top, bounds.right, bounds.bottom];
  })).toEqual(layout.stage);
});

test("Android landscape Watch mode follows the visible viewport instead of a stale viewport unit", async ({ page, isMobile }) => {
  test.skip(!isMobile, "Android landscape behavior belongs to mobile Chromium");
  await installAndroidVisualViewport(page, { width: 915, height: 356 });
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  await page.setViewportSize({ width: 915, height: 412 });
  const visibleBounds = () => page.evaluate(() => {
    const topbar = document.querySelector(".topbar").getBoundingClientRect();
    const main = document.querySelector("main").getBoundingClientRect();
    const library = document.querySelector(".library").getBoundingClientRect();
    const stage = document.querySelector("#player-stage").getBoundingClientRect();
    const viewportHeight = window.visualViewport?.height ?? window.innerHeight;
    return {
      contained: [
        Math.abs(main.top - topbar.bottom) <= 1,
        Math.abs(main.bottom - viewportHeight) <= 1,
        library.bottom <= viewportHeight + 1,
        stage.bottom <= viewportHeight + 1,
      ],
      mainBottom: Math.round(main.bottom),
      stageBottom: Math.round(stage.bottom),
    };
  });
  await expect.poll(async () => (await visibleBounds()).contained).toEqual([true, true, true, true]);
  const initial = await visibleBounds();

  await page.evaluate(() => window.__visualViewportTest.resizeViewport({ height: 338 }));
  await expect.poll(async () => (await visibleBounds()).contained).toEqual([true, true, true, true]);
  const resized = await visibleBounds();
  expect(initial.mainBottom - resized.mainBottom).toBe(18);
  expect(resized.stageBottom).toBeLessThan(initial.stageBottom);

  await page.goto("/?layout=watch");
  await expect.poll(async () => (await visibleBounds()).contained).toEqual([true, true, true, true]);
});

test("Android landscape fullscreen contains video inside the visible viewport", async ({ page, isMobile }) => {
  test.skip(!isMobile, "Android landscape behavior belongs to mobile Chromium");
  await usePreference(page, "stream", "direct");
  await installAndroidVisualViewport(page, {
    width: 840,
    height: 356,
    offsetLeft: 38,
    offsetTop: 12,
  });
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  await page.setViewportSize({ width: 915, height: 412 });
  await page.locator("#fullscreen-button").click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null))
    .toBe("player-stage");

  const layout = await page.locator("#player-stage").evaluate((stage) => {
    const viewport = visualViewport;
    const stageBounds = stage.getBoundingClientRect();
    const mediaBounds = stage.querySelector(".media-viewport").getBoundingClientRect();
    const video = stage.querySelector("video");
    const videoBounds = video.getBoundingClientRect();
    const controlBounds = stage.querySelector(".control-surface").getBoundingClientRect();
    const inside = (bounds) => bounds.left >= viewport.offsetLeft
      && bounds.top >= viewport.offsetTop
      && bounds.right <= viewport.offsetLeft + viewport.width
      && bounds.bottom <= viewport.offsetTop + viewport.height;
    return {
      stage: [stageBounds.left, stageBounds.top, stageBounds.width, stageBounds.height].map(Math.round),
      mediaInside: inside(mediaBounds),
      videoInside: inside(videoBounds),
      controlsInside: inside(controlBounds),
      objectFit: getComputedStyle(video).objectFit,
    };
  });
  expect(layout).toEqual({
    stage: [0, 0, 915, 412],
    mediaInside: true,
    videoInside: true,
    controlsInside: true,
    objectFit: "contain",
  });

  await page.evaluate(() => window.__visualViewportTest.resizeViewport({
    width: 812,
    height: 338,
    offsetLeft: 51,
    offsetTop: 19,
  }));
  await expect.poll(() => page.locator("#player-stage .media-viewport").evaluate((media) => {
    const bounds = media.getBoundingClientRect();
    return [bounds.left, bounds.top, bounds.width, bounds.height].map(Math.round);
  })).toEqual([51, 19, 812, 338]);
});

test("mobile timeline has an enlarged thumb and accepts edge touches", async ({ page, isMobile }) => {
  test.skip(!isMobile, "mobile project only");
  await serveFixtureMedia(page);
  await openLibrary(page);
  await selectTaggedVideo(page);

  const timeline = page.locator("#timeline");
  await page.locator("#player-stage").dispatchEvent("pointerdown", {
    pointerType: "touch",
    isPrimary: true,
  });
  await expect(timeline).toBeVisible();
  await expect.poll(async () => Number(await timeline.getAttribute("max"))).toBeGreaterThan(0);
  const metrics = await timeline.evaluate((control) => ({
    height: control.getBoundingClientRect().height,
    thumbSize: getComputedStyle(control).getPropertyValue("--timeline-thumb-size").trim(),
  }));
  expect(metrics).toEqual({ height: 52, thumbSize: "28px" });

  const bounds = await timeline.boundingBox();
  const maximum = Number(await timeline.getAttribute("max"));
  await timeline.tap({ position: { x: bounds.width * .75, y: 4 } });
  await expect.poll(async () => Number(await timeline.inputValue())).toBeGreaterThan(maximum * .6);
});

test("Android fullscreen double taps seek without accidental play or pause", async ({ page, isMobile }) => {
  test.skip(!isMobile, "Android fullscreen touch behavior belongs to mobile Chromium");
  await usePreference(page, "stream", "direct");
  await serveFixtureMedia(page);
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
  await page.locator("#fullscreen-button").click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id || null)).toBe("player-stage");

  const video = page.locator("#video-player");
  await video.evaluate((player) => {
    let currentTime = 120;
    let paused = false;
    let pauseCalls = 0;
    let playCalls = 0;
    Object.defineProperty(player, "currentTime", {
      configurable: true,
      get: () => currentTime,
      set: (value) => { currentTime = Number(value); },
    });
    Object.defineProperty(player, "paused", {
      configurable: true,
      get: () => paused,
    });
    player.pause = () => {
      pauseCalls += 1;
      paused = true;
      player.dispatchEvent(new Event("pause"));
    };
    player.play = () => {
      playCalls += 1;
      paused = false;
      player.dispatchEvent(new Event("playing"));
      return Promise.resolve();
    };
    window.__touchSeekTest = {
      state: () => ({ currentTime, pauseCalls, playCalls }),
    };
  });
  const bounds = await video.boundingBox();
  const tap = (x) => page.touchscreen.tap(x, bounds.y + bounds.height / 2);
  await page.waitForTimeout(150);
  const settled = await page.evaluate(() => window.__touchSeekTest.state());

  const right = bounds.x + bounds.width * .75;
  await tap(right);
  await page.waitForTimeout(400);
  await tap(right + 100);
  await expect.poll(() => page.evaluate(() => window.__touchSeekTest.state()))
    .toEqual({ ...settled, currentTime: 150 });
  await expect(page.locator("#seek-gesture-feedback")).toHaveText("+30s");
  await expect(page.locator("#seek-gesture-feedback")).toHaveAttribute("data-direction", "forward");

  const left = bounds.x + bounds.width * .25;
  await tap(left);
  await page.waitForTimeout(400);
  await tap(left - 100);
  await expect.poll(() => page.evaluate(() => window.__touchSeekTest.state()))
    .toEqual({ ...settled, currentTime: 120 });
  await expect(page.locator("#seek-gesture-feedback")).toHaveText("−30s");
  await expect(page.locator("#seek-gesture-feedback")).toHaveAttribute("data-direction", "backward");

  await tap(right);
  await page.waitForTimeout(550);
  await expect.poll(() => page.evaluate(() => window.__touchSeekTest.state()))
    .toEqual({ ...settled, currentTime: 120 });
  await expect.poll(() => page.locator("#seek-gesture-feedback").isHidden()).toBe(true);

  await tap(right);
  await page.waitForTimeout(100);
  await page.locator("#play-button").tap();
  const afterControlTap = await page.evaluate(() => window.__touchSeekTest.state());
  await tap(right + 100);
  await page.waitForTimeout(550);
  await expect.poll(() => page.evaluate(() => window.__touchSeekTest.state()))
    .toEqual(afterControlTap);
});

test("video controls hide after three idle seconds and on pointer leave", async ({ page }) => {
  await openLibrary(page);
  await selectTaggedVideo(page);

  const stage = page.locator("#player-stage");
  const controls = page.locator("#playback-controls");
  const closeButton = page.locator("#close-player-button");
  await stage.dispatchEvent("pointermove");
  await expect(stage).toHaveClass(/controls-visible/);
  await expect(controls).toBeVisible();
  await expect(closeButton).toBeVisible();

  await page.waitForTimeout(3100);
  await expect(stage).not.toHaveClass(/controls-visible/);
  await expect(controls).toBeHidden();
  await expect(closeButton).toBeHidden();

  await stage.dispatchEvent("pointerenter");
  await expect(controls).toBeVisible();
  await expect(closeButton).toBeVisible();
  await stage.dispatchEvent("pointerleave");
  await expect(controls).toBeHidden();
  await expect(closeButton).toBeHidden();

  await stage.dispatchEvent("pointermove");
  await expect(controls).toBeVisible();
  await page.locator("#play-button").focus();
  await stage.dispatchEvent("pointerleave");
  await expect(controls).toBeVisible();
});

test("touch controls remain visible for five seconds after pointer leave", async ({ page, isMobile, browserName }) => {
  test.skip(!isMobile && browserName !== "webkit", "touch timing belongs to mobile and WebKit coverage");
  await openLibrary(page);
  await selectTaggedVideo(page);

  const stage = page.locator("#player-stage");
  const controls = page.locator("#playback-controls");
  await page.evaluate(() => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
  await stage.evaluate((element) => {
    document.activeElement?.blur();
    const startedAt = performance.now();
    window.__touchControlsHiddenAfter = null;
    const observer = new MutationObserver(() => {
      if (element.classList.contains("controls-visible")) return;
      window.__touchControlsHiddenAfter = performance.now() - startedAt;
      observer.disconnect();
    });
    observer.observe(element, { attributes: true, attributeFilter: ["class"] });
    for (const type of ["pointerdown", "pointerleave"]) {
      element.dispatchEvent(new PointerEvent(type, {
        bubbles: true,
        pointerType: "touch",
        pointerId: 1,
        isPrimary: true,
      }));
    }
  });
  await expect(stage).toHaveClass(/controls-visible/);
  await expect(controls).toBeVisible();

  await expect.poll(() => page.evaluate(() => window.__touchControlsHiddenAfter), { timeout: 7_000 })
    .toBeGreaterThanOrEqual(4_900);
  await expect(stage).not.toHaveClass(/controls-visible/);
  await expect(controls).toBeHidden();
});

test("stage keyboard focus pins visible controls and keeps Play reachable", async ({ page }) => {
  await openLibrary(page);
  await selectTaggedVideo(page);

  const stage = page.locator("#player-stage");
  const controls = page.locator("#playback-controls");
  await page.evaluate(() => document.activeElement?.blur());
  await page.keyboard.press("Tab");
  await stage.focus();
  await expect(stage).toBeFocused();
  expect(await stage.evaluate((element) => {
    const style = getComputedStyle(element);
    return { style: style.outlineStyle, width: style.outlineWidth };
  })).toEqual({ style: "solid", width: "3px" });

  await page.waitForTimeout(3100);
  await expect(stage).toHaveClass(/controls-visible/);
  await expect(controls).toBeVisible();
  for (let step = 0; step < 8 && await page.evaluate(() => document.activeElement?.id !== "play-button"); step += 1) {
    await page.keyboard.press("Tab");
  }
  await expect(page.locator("#play-button")).toBeFocused();
});


test("movie collection groups continue across pages and end before standalone cards", async ({ page }) => {
  let initial = null;
  await page.route("**/api/web/library?**", async (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("view") !== "library" || url.searchParams.get("kind") !== "video") return route.fallback();
    if (!initial) initial = await (await route.fetch()).json();
    const base = initial.entries.find((entry) => entry.entry_type === "media");
    const offset = Number(url.searchParams.get("offset") || 0);
    const entries = Array.from({ length: 27 }, (_, index) => ({
      ...base,
      id: String(80000 + index),
      title: index === 0 ? "Amber Road" : index === 26 ? "Copper Road" : `Chapter ${index}`,
      file_name: `fictional-${index}.mkv`,
      collection: index > 0 && index < 26 ? { id: "fictional-series", title: "Briar Saga", sequence: index } : null,
      art_url: null,
    })).slice(offset, offset + 24);
    await route.fulfill({ json: {
      ...initial, entries, offset, total: 27, limit: 24, has_more: offset + entries.length < 27,
    } });
  });
  await page.goto("/?view=video");
  await expect(page.locator("[data-media-id]")).toHaveCount(24);
  await expect(page.getByRole("heading", { name: "Briar Saga" })).toHaveCount(1);
  const collection = page.getByRole("region", { name: "Briar Saga" });
  await expect(collection.locator(".media-card")).toHaveCount(23);
  const firstCard = collection.locator(".media-card").first();
  await firstCard.evaluate((card) => { card.dataset.preserved = "yes"; });
  await page.locator("#load-more-sentinel").scrollIntoViewIfNeeded();
  await expect(page.locator("[data-media-id]")).toHaveCount(27);
  await expect(collection.locator(".media-card")).toHaveCount(25);
  await expect(page.getByRole("heading", { name: "Briar Saga" })).toHaveCount(1);
  await expect(firstCard).toHaveAttribute("data-preserved", "yes");
  await expect(collection.locator(".card-title")).toHaveText(Array.from({ length: 25 }, (_, index) => `Chapter ${index + 1}`));
  await expect(page.locator("#media-grid > .media-card .card-title")).toHaveText(["Amber Road", "Copper Road"]);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  const accessibility = await new AxeBuilder({ page }).include("#media-grid").analyze();
  expect(accessibility.violations).toEqual([]);
  const cardWidths = () => page.evaluate(() => ({
    grouped: document.querySelector(".collection-group .media-card").getBoundingClientRect().width,
    standalone: document.querySelector("#media-grid > .media-card").getBoundingClientRect().width,
  }));
  let widths = await cardWidths();
  expect(Math.abs(widths.grouped - widths.standalone)).toBeLessThanOrEqual(1);
  // Browsers without subgrid keep the same poster grid through display: contents.
  await collection.evaluate((section) => { section.style.display = "contents"; });
  widths = await cardWidths();
  expect(Math.abs(widths.grouped - widths.standalone)).toBeLessThanOrEqual(1);
  await page.locator("#sort-control").selectOption("date_desc");
  await expect(page.locator("[data-media-id]")).toHaveCount(24);
  await expect(page.locator(".collection-group")).toHaveCount(0);
});
