import { execFile } from "node:child_process";
import { promisify } from "node:util";
import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

const execFileAsync = promisify(execFile);
let media;
test.beforeAll(async () => {
  ({ stdout: media } = await execFileAsync("ffmpeg", [
    "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=12",
    "-t", "40", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
    "-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", "pipe:1",
  ], { encoding: null, timeout: 15_000, maxBuffer: 4 * 1024 * 1024 }));
});

async function prepare(page, { fakePip = true } = {}) {
  const cancellations = [];
  await page.addInitScript(({ fakePip }) => {
    localStorage.setItem("rustydlna.stream", "compat");
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function (type) {
      return String(type).includes("mpegurl") ? "" : canPlayType.call(this, type);
    };
    if (globalThis.MediaSource) Object.defineProperty(MediaSource, "isTypeSupported", { value: () => false });
    if (!fakePip) return;
    let active = null;
    const pending = [];
    const defer = (kind) => new Promise((resolve, reject) => pending.push({ kind, resolve, reject }));
    Object.defineProperty(document, "pictureInPictureEnabled", { configurable: true, value: true });
    Object.defineProperty(document, "pictureInPictureElement", { configurable: true, get: () => active });
    HTMLVideoElement.prototype.requestPictureInPicture = () => defer("enter");
    document.exitPictureInPicture = () => defer("exit");
    window.__pip = {
      count: () => pending.length,
      reject() { pending.shift().reject(new DOMException("sensitive diagnostic ".repeat(500), "NotAllowedError")); },
      resolve() {
        const request = pending.shift();
        active = request.kind === "enter" ? document.querySelector("#video-player") : null;
        document.querySelector("#video-player").dispatchEvent(new Event(
          request.kind === "enter" ? "enterpictureinpicture" : "leavepictureinpicture",
        ));
        request.resolve({});
      },
    };
  }, { fakePip });
  await page.route("**/web/media/*.mp4?**", (route) => {
    const range = /^bytes=(\d+)-(\d*)$/.exec(route.request().headers().range || "");
    const start = range ? Number(range[1]) : 0;
    const end = range?.[2] ? Math.min(Number(range[2]), media.length - 1) : media.length - 1;
    return route.fulfill({
      status: range ? 206 : 200, contentType: "video/mp4",
      headers: { "accept-ranges": "bytes", ...(range ? { "content-range": `bytes ${start}-${end}/${media.length}` } : {}) },
      body: media.subarray(start, end + 1),
    });
  });
  await page.route("**/api/web/transcode/**", (route) => {
    if (route.request().method() === "DELETE") cancellations.push(route.request().url());
    return route.fulfill({ json: { schema_version: 2, state: "ready", complete: true, output_duration: 40 } });
  });
  await page.goto("/");
  await page.getByRole("tab", { name: "Videos" }).click();
  await page.getByRole("button", { name: /^Play tagged\b/ }).click();
  const video = page.locator("#video-player");
  await expect.poll(() => video.evaluate((node) => node.currentTime)).toBeGreaterThan(0.2);
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
  await video.evaluate((node) => {
    window.__mediaReloads = 0;
    node.addEventListener("loadstart", () => { window.__mediaReloads += 1; });
  });
  return { video, cancellations };
}

async function revealControls(page) {
  const stage = page.locator("#player-stage");
  await stage.scrollIntoViewIfNeeded();
  await stage.hover();
  await expect(page.locator("#playback-controls")).toBeVisible();
}

async function clickPip(page) {
  await revealControls(page);
  const button = page.locator("#pip-button");
  if (await button.isVisible()) {
    await button.focus();
    await expect(button).toBeFocused();
    await button.press("Enter");
  } else {
    // The compact phone toolbar omits PiP. Exercise the same optional API
    // handler there while focus remains on its visible transport control.
    await page.locator("#play-button").focus();
    await button.evaluate((node) => node.click());
  }
}

for (const paused of [false, true]) {
  test(`picture-in-picture denial preserves healthy ${paused ? "paused" : "playing"} media`, async ({ page }) => {
    const { video, cancellations } = await prepare(page);
    if (paused) {
      await revealControls(page);
      const play = page.locator("#play-button");
      await play.focus();
      await expect(play).toBeFocused();
      await play.press("Enter");
      await expect.poll(() => video.evaluate((node) => node.paused)).toBe(true);
    }
    const before = await video.evaluate((node) => ({ src: node.src, time: node.currentTime, rate: node.playbackRate, volume: node.volume, muted: node.muted }));
    const cancellationsBefore = cancellations.length;
    await clickPip(page);
    await expect.poll(() => page.evaluate(() => window.__pip.count())).toBe(1);
    await page.evaluate(() => window.__pip.reject());
    await expect(page.locator("#player-message")).toHaveAttribute("role", "status");
    await expect(page.locator("#player-message-text")).toHaveText("Picture in picture is unavailable. You can keep watching in the player.");
    await expect(page.locator("#player-retry")).toBeHidden();
    await expect(page.locator("#technical-details")).toBeHidden();
    await expect(page.locator("#play-button")).toHaveAttribute("aria-label", paused ? "Play" : "Pause");
    await expect(page.locator(await page.locator("#pip-button").isVisible() ? "#pip-button" : "#play-button")).toBeFocused();
    await expect(page.locator("#player-stage")).toHaveClass(/controls-visible/);
    const after = await video.evaluate((node) => ({ src: node.src, time: node.currentTime, rate: node.playbackRate, volume: node.volume, muted: node.muted, paused: node.paused }));
    expect(after.src).toBe(before.src);
    expect(after.paused).toBe(paused);
    expect(after.rate).toBe(before.rate);
    expect(after.volume).toBe(before.volume);
    expect(after.muted).toBe(before.muted);
    if (paused) expect(after.time).toBeCloseTo(before.time, 2);
    else await expect.poll(() => video.evaluate((node) => node.currentTime)).toBeGreaterThan(before.time + 0.2);
    expect(await page.evaluate(() => window.__mediaReloads)).toBe(0);
    expect(cancellations.length).toBe(cancellationsBefore);
    expect((await new AxeBuilder({ page }).include("#player-panel").analyze()).violations).toEqual([]);
  });
}

test("picture-in-picture serializes entry/exit, reports exit denial and clears notice after success", async ({ page }) => {
  const { video, cancellations } = await prepare(page);
  const src = await video.getAttribute("src");
  await clickPip(page);
  await clickPip(page);
  expect(await page.evaluate(() => window.__pip.count())).toBe(1);
  await page.evaluate(() => window.__pip.resolve());
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "true");
  // A delayed leave event cannot undo the authoritative active PiP element.
  await video.dispatchEvent("leavepictureinpicture");
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "true");
  await clickPip(page);
  await page.evaluate(() => window.__pip.reject());
  await expect(page.locator("#player-message-text")).toHaveText("Picture in picture could not close. Try closing its window.");
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "true");
  await clickPip(page);
  await page.evaluate(() => window.__pip.resolve());
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(video).toHaveAttribute("src", src);
  expect(await page.evaluate(() => window.__mediaReloads)).toBe(0);
  expect(cancellations).toEqual([]);
});

test("late PiP rejection after a same-title source replacement leaves the new source healthy", async ({ page }) => {
  const { video } = await prepare(page);
  const src = await video.getAttribute("src");
  await clickPip(page);
  await page.locator("#advanced-playback-button").click();
  await page.locator("#quality-control").selectOption("low_360");
  await expect(video).not.toHaveAttribute("src", src);
  await expect.poll(() => video.evaluate((node) => node.currentTime)).toBeGreaterThan(0.2);
  await page.evaluate(() => window.__pip.reject());
  await expect(page.locator("#player-message")).toBeHidden();
  await expect(page.locator("#player-stage")).toHaveClass(/is-playing/);
});

test("native picture-in-picture enters and exits where the browser exposes the API", async ({ page }) => {
  const { video } = await prepare(page, { fakePip: false });
  test.skip(!await page.evaluate(() => document.pictureInPictureEnabled
    && typeof document.querySelector("#video-player").requestPictureInPicture === "function"), "This browser does not expose the element Picture-in-Picture API");
  test.skip(!await page.locator("#pip-button").isVisible(), "The compact phone toolbar omits the Picture-in-Picture control");
  const src = await video.getAttribute("src");
  await clickPip(page);
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "true");
  await clickPip(page);
  await expect(page.locator("#pip-button")).toHaveAttribute("aria-pressed", "false");
  await expect(video).toHaveAttribute("src", src);
  await expect(page.locator("#player-message")).toBeHidden();
});
