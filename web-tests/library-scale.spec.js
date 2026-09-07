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
