import { expect, test } from "@playwright/test";

// Full DOM trace snapshots dominate this 10,000-card workload in WebKit.
test.use({ trace: "off" });

test("10,000 movie cards reserve stable offscreen space and load posters on demand", async ({ page }) => {
  test.setTimeout(60_000);
  let initial = null;
  let metadataRequests = 0;
  const posterRequests = [];
  await page.route("**/scale-poster/**", async (route) => {
    posterRequests.push(route.request().url());
    await route.fulfill({ contentType: "image/svg+xml", body:
      '<svg xmlns="http://www.w3.org/2000/svg" width="600" height="900"><rect width="600" height="900" fill="navy"/></svg>',
    });
  });
  await page.route("**/api/web/library?**", async (route) => {
    const params = new URL(route.request().url()).searchParams;
    if (params.get("kind") !== "video") return route.fallback();
    metadataRequests += 1;
    if (!initial) initial = await (await route.fetch()).json();
    const offset = Number(params.get("offset"));
    const limit = Number(params.get("limit"));
    const base = initial.entries.find((entry) => entry.entry_type === "media");
    const entries = Array.from({ length: Math.min(limit, 10000 - offset) }, (_, index) => ({
      ...base, id: String(100000 + offset + index), title: `Large library ${offset + index + 1}`,
      collection: offset + index >= 5000 && offset + index < 5100
        ? { id: "scale-collection", title: "Large collection", sequence: offset + index - 4999 } : null,
      art_url: `/scale-poster/${offset + index}.svg`,
    }));
    await route.fulfill({ json: {
      ...initial, entries, offset, limit, total: 10000, has_more: offset + entries.length < 10000,
    } });
  });
  await page.goto("/?view=video", { waitUntil: "domcontentloaded" });
  await expect(page.locator(".media-card.video")).toHaveCount(10000, { timeout: 20_000 });
  await expect(page.locator(".media-chunk.sized").first()).toBeAttached();
  expect(metadataRequests).toBe(50);
  const firstWidth = await page.locator(".media-card").first().evaluate((card) => card.getBoundingClientRect().width);
  expect(firstWidth).toBeGreaterThan(100);
  expect(firstWidth).toBeLessThan(400);
  const height = await page.evaluate(() => document.documentElement.scrollHeight);
  const last = page.locator('[data-media-id="109999"]');
  await expect(last.locator("img")).not.toHaveAttribute("src");
  // Scroll the reserved batch: WebKit cannot scroll a skipped descendant's
  // empty box into view, whereas the batch always has its measured geometry.
  await last.evaluate((card) => card.closest(".media-chunk").scrollIntoView({ block: "end" }));
  await expect.poll(() => last.locator("img").evaluate((image) => image.complete && image.naturalWidth > 0)).toBe(true);
  await last.locator(".card-button").focus();
  await expect(last.locator(".card-button")).toBeFocused();
  expect(await last.evaluate((card) => getComputedStyle(card.closest(".media-chunk")).contentVisibility)).toBe("visible");
  expect(await page.evaluate(() => document.documentElement.scrollHeight)).toBe(height);
  const collected = page.locator('[data-media-id="105050"]');
  await collected.evaluate((card) => card.closest(".media-chunk").scrollIntoView({ block: "center" }));
  await collected.scrollIntoViewIfNeeded();
  await expect.poll(() => collected.locator("img").evaluate((image) => image.complete && image.naturalWidth > 0)).toBe(true);
  expect(await page.evaluate(() => document.documentElement.scrollHeight)).toBe(height);
  expect(posterRequests.length).toBeLessThan(200);
  expect(metadataRequests).toBe(50);
  // Changing the library width remeasures its reserved spaces.
  await page.setViewportSize({ width: 820, height: 1000 });
  await last.scrollIntoViewIfNeeded();
  await expect(last).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test("navigation cancels private card batches without publishing a partial list", async ({ page, request }) => {
  const template = await (await request.get("/api/web/library?view=library&kind=video")).json();
  const base = template.entries.find((entry) => entry.entry_type === "media");
  await page.addInitScript(() => {
    const create = document.createElement.bind(document);
    window.__cardBuild = { count: 0, atNavigation: null };
    document.createElement = (...args) => {
      const element = create(...args);
      if (args[0] === "article" && ++window.__cardBuild.count === 64) {
        setTimeout(() => {
          window.__cardBuild.atNavigation = document.querySelectorAll(".media-card.video").length;
          document.querySelector('[data-kind="audio"]').click();
        }, 0);
      }
      return element;
    };
  });
  await page.route("**/api/web/library?**", async (route) => {
    const params = new URL(route.request().url()).searchParams;
    if (params.get("kind") !== "video") return route.fallback();
    const offset = Number(params.get("offset"));
    await route.fulfill({ json: { ...template, offset, limit: 200, total: 1000, has_more: offset + 200 < 1000,
      entries: Array.from({ length: 200 }, (_, index) => ({
        ...base, id: String(100000 + offset + index), title: `Cancelled card ${offset + index}`, art_url: null,
      })),
    } });
  });
  await page.goto("/?view=video");
  await expect(page.getByRole("tab", { name: "Audio", exact: true })).toHaveAttribute("aria-selected", "true");
  await expect(page.locator("#library-panel")).toHaveAttribute("aria-busy", "false");
  await expect(page.locator(".media-card.audio").first()).toBeVisible();
  expect(await page.evaluate(() => window.__cardBuild.atNavigation)).toBe(0);
  expect(await page.evaluate(() => window.__cardBuild.count)).toBeLessThan(200);
  await expect(page.locator(".media-card.video")).toHaveCount(0);
});

test("chunk reservations match heterogeneous card layouts and reuse an unchanged width", async ({ page, request }) => {
  const template = await (await request.get("/api/web/library?view=library&kind=video")).json();
  const base = template.entries.find((entry) => entry.entry_type === "media");
  await page.addInitScript(() => {
    window.__chunkMeasurements = 0;
    const measure = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function () {
      if (this.classList.contains("media-chunk") && this.style.contentVisibility === "visible") window.__chunkMeasurements += 1;
      return measure.call(this);
    };
  });
  await page.route("**/api/web/library?**", async (route) => {
    const params = new URL(route.request().url()).searchParams;
    if (params.get("kind") !== "video") return route.fallback();
    const offset = Number(params.get("offset"));
    await route.fulfill({ json: { ...template, offset, limit: 200, total: 1000, has_more: offset + 200 < 1000,
      entries: Array.from({ length: 200 }, (_, index) => ({
        ...base, id: String(100000 + offset + index), title: `A varying title ${index} ${"long ".repeat(index % 10)}`,
        kind: index % 4 === 0 ? "audio" : "video", artist: "A long artist name ".repeat(index % 3),
        album: "Album", file_name: index % 3 ? "file.mp4" : null, art_url: null,
        collection: offset >= 400 && offset < 600 ? { id: "varied", title: "Varied collection" } : null,
      })),
    } });
  });
  await page.goto("/?view=video");
  await expect(page.locator(".media-card")).toHaveCount(1000);
  const checkHeights = async () => {
    const mismatches = await page.evaluate(() => {
      const chunks = [...document.querySelectorAll(".media-chunk")];
      for (const chunk of chunks) chunk.style.contentVisibility = "visible";
      const mismatches = chunks.map((chunk) => ({ measured: chunk.getBoundingClientRect().height,
        reserved: parseFloat(chunk.style.getPropertyValue("--chunk-height")) }))
        .filter(({ measured, reserved }) => Math.abs(measured - reserved) > 1);
      for (const chunk of chunks) chunk.style.removeProperty("content-visibility");
      return mismatches;
    });
    expect(mismatches).toEqual([]);
  };
  await checkHeights();
  const focusedCard = page.locator(".media-card .card-button").nth(850);
  await focusedCard.focus();
  const original = page.viewportSize();
  await page.setViewportSize({ width: original.width === 820 ? 1000 : 820, height: original.height });
  await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  await expect(focusedCard).toBeFocused();
  await checkHeights();
  await page.evaluate(() => { window.__chunkMeasurements = 0; });
  await page.setViewportSize(original);
  await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  await expect(focusedCard).toBeFocused();
  expect(await page.evaluate(() => window.__chunkMeasurements)).toBe(0);
  await checkHeights();
});

test("clock events update time and chapters without repainting static playback controls", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("rustydlna.stream", "direct"));
  await page.route("**/api/web/library?**", async (route) => {
    const payload = await (await route.fetch()).json();
    for (const item of payload.entries) {
      if (item.title === "tagged") item.chapters = [
        { index: 0, start_seconds: 0, title: "First" },
        { index: 1, start_seconds: 1, title: "Second" },
      ];
    }
    await route.fulfill({ json: payload });
  });
  await page.goto("/?view=video");
  await page.getByRole("button", { name: /^Play tagged\./ }).click();
  await expect(page.locator("#video-player")).toHaveAttribute("src", /\/web\/media\//);
  const result = await page.evaluate(() => {
    const video = document.querySelector("#video-player");
    Object.defineProperty(video, "currentTime", { configurable: true, get: () => 1.25 });
    const observer = new MutationObserver(() => {});
    for (const id of ["queue-position", "now-playing-title", "previous-button", "next-button", "stream-controls"]) {
      const element = document.getElementById(id);
      if (element) observer.observe(element, { subtree: true, childList: true, attributes: true, characterData: true });
    }
    video.dispatchEvent(new Event("timeupdate"));
    const mutations = observer.takeRecords().length;
    observer.disconnect();
    return { mutations, time: document.querySelector("#timeline-current").textContent,
      chapter: document.querySelector("#chapter-controls").value };
  });
  expect(result.mutations).toBe(0);
  expect(result.time).toBe("0:01");
  expect(result.chapter).toBe("1");
});
