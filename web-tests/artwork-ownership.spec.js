import { createServer } from "node:http";
import { expect, test } from "./diagnostics.js";

const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="600" height="900"><rect width="600" height="900" fill="navy"/></svg>';

// Same-origin sockets, including real incomplete image responses: route.fulfill
// cannot demonstrate that removing src actually closes browser requests.
async function artworkServer(request) {
  const template = await (await request.get("/api/web/library?view=library&kind=video")).json();
  const base = template.entries.find((entry) => entry.entry_type === "media");
  const records = [];
  const held = new Map();
  let maximum = 0;
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    if (url.pathname.startsWith("/ownership-art/")) {
      const record = { url: url.pathname, closed: false, complete: false };
      records.push(record);
      maximum = Math.max(maximum, records.filter((entry) => !entry.closed).length);
      res.on("close", () => { record.closed = true; held.delete(record.url); });
      res.writeHead(200, { "Content-Type": "image/svg+xml", "Cache-Control": "no-store" });
      const complete = () => { record.complete = true; res.end(svg.slice(4)); };
      res.write(svg.slice(0, 4));
      if (records.length <= 4) held.set(record.url, { res, complete });
      else complete();
      return;
    }
    if (url.pathname === "/api/web/library" && url.searchParams.get("kind") === "video") {
      const entries = Array.from({ length: 80 }, (_, index) => ({ ...base,
        id: String(90000 + index), title: `Ownership ${index}`, collection: null,
        art_url: `/ownership-art/${index}.svg`,
      }));
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ...template, entries, offset: 0, limit: 200, total: 80, has_more: false }));
      return;
    }
    try {
      const upstream = await fetch(`http://127.0.0.1:18201${req.url}`);
      const headers = Object.fromEntries(upstream.headers);
      delete headers["transfer-encoding"];
      delete headers["content-encoding"];
      res.writeHead(upstream.status, headers);
      res.end(Buffer.from(await upstream.arrayBuffer()));
    } catch { res.destroy(); }
  });
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  return {
    url: `http://127.0.0.1:${server.address().port}/?view=video`, records, held,
    get maximum() { return maximum; },
    async close() {
      server.closeAllConnections();
      await new Promise((done) => server.close(done));
    },
  };
}

for (const offscreen of [false, true]) {
  test(`real hung posters release slots after ${offscreen ? "offscreen grace" : "ownership deadline"}`, async ({ page, request }) => {
    const server = await artworkServer(request);
    try {
      await page.emulateMedia({ reducedMotion: "reduce" });
      await page.addInitScript(() => {
        window.__artworkBlobs = { live: new Map(), maximum: 0 };
        const create = URL.createObjectURL.bind(URL);
        const revoke = URL.revokeObjectURL.bind(URL);
        URL.createObjectURL = (blob) => {
          const url = create(blob);
          window.__artworkBlobs.live.set(url, blob.size);
          window.__artworkBlobs.maximum = Math.max(window.__artworkBlobs.maximum, window.__artworkBlobs.live.size);
          return url;
        };
        URL.revokeObjectURL = (url) => { window.__artworkBlobs.live.delete(url); revoke(url); };
      });
      await page.clock.install();
      await page.goto(server.url, { waitUntil: "domcontentloaded" });
      await expect(page.locator(".media-card")).toHaveCount(80);
      await expect.poll(() => server.held.size).toBe(4);
      const original = server.records.slice();
      await page.evaluate((urls) => {
        window.__oldArtwork = urls.map((url) => document.querySelector(`[data-media-id="${90000 + Number(url.match(/(\d+)\.svg/)[1])}"] img`));
      }, original.map(({ url }) => url));
      const target = offscreen ? page.locator(".media-card .card-button").last() : page.locator(".media-card .card-button").first();
      await target.focus();
      if (offscreen) await target.scrollIntoViewIfNeeded();
      await page.clock.runFor(100); // Deliver the scroll/animation-frame admission update.
      const geometry = await target.evaluate((button) => ({ top: scrollY, height: document.documentElement.scrollHeight,
        target: button.getBoundingClientRect().top }));
      await page.clock.fastForward(offscreen ? 5100 : 60100);
      await expect.poll(() => original.map(({ url, closed }) => ({ url, closed })))
        .toEqual(original.map(({ url }) => ({ url, closed: true })));
      expect(original.every((record) => !record.complete)).toBe(true);
      const healthy = offscreen ? page.locator(".media-card img").last() : page.locator(".media-card img:not(.failed)[src]").first();
      await expect.poll(() => healthy.evaluate((image) => image.complete && image.naturalWidth > 0)).toBe(true);
      expect(server.maximum).toBeLessThanOrEqual(4);
      // Late callbacks on expired elements must neither mark healthy images
      // failed nor release somebody else's slot. Expiry is not an auto retry.
      await page.evaluate(() => {
        for (const image of window.__oldArtwork) {
          image.dispatchEvent(new Event("load")); image.dispatchEvent(new Event("error"));
        }
      });
      await page.clock.fastForward(120000);
      expect(server.maximum).toBeLessThanOrEqual(4);
      expect(await page.evaluate(() => window.__oldArtwork.every((image) => image.classList.contains("failed") && !image.hasAttribute("src")))).toBe(true);
      for (const record of original) expect(server.records.filter((entry) => entry.url === record.url)).toHaveLength(1);
      await expect(target).toBeFocused();
      expect(await target.evaluate((button) => ({ top: scrollY, height: document.documentElement.scrollHeight,
        target: button.getBoundingClientRect().top }))).toEqual(geometry);
      await expect(healthy).not.toHaveClass(/failed/);
      await expect.poll(() => page.evaluate(() => window.__artworkBlobs.live.size)).toBe(0);
      expect(await page.evaluate(() => window.__artworkBlobs.maximum)).toBeLessThanOrEqual(4);
      // Reloading the view is an explicit retry. The previously hung URLs now
      // complete and must decode normally, with no persistent failure cache.
      await page.reload({ waitUntil: "domcontentloaded" });
      await page.locator(".media-card .card-button").first().focus();
      for (const { url } of original) {
        const id = 90000 + Number(url.match(/(\d+)\.svg/)[1]);
        await expect.poll(() => page.locator(`[data-media-id="${id}"] img`).evaluate((image) => image.complete && image.naturalWidth > 0)).toBe(true);
        expect(server.records.filter((entry) => entry.url === url)).toHaveLength(2);
      }
    } finally { await page.goto("about:blank"); await server.close(); }
  });
}

test("healthy slow posters keep ownership through scrolling and view replacement closes old sockets", async ({ page, request, isMobile }) => {
  const server = await artworkServer(request);
  try {
    await page.clock.install();
    await page.goto(server.url, { waitUntil: "domcontentloaded" });
    await expect.poll(() => server.held.size).toBe(4);
    const original = server.records.slice();
    const slowUrl = original[0].url;
    const slow = page.locator(`[data-media-id="${90000 + Number(slowUrl.match(/(\d+)\.svg/)[1])}"] img`);
    for (let index = 0; index < 3; index++) {
      server.held.get(slowUrl).res.write(" "); // Actual body progress; never restart this request.
      await page.locator(".media-card .card-button").last().focus();
      await page.clock.runFor(100);
      await page.clock.fastForward(1000);
      await slow.locator("..").locator("..").focus();
      await slow.scrollIntoViewIfNeeded();
      await page.clock.runFor(100);
    }
    expect(original.every((record) => !record.closed)).toBe(true);
    server.held.get(slowUrl).complete();
    await expect.poll(() => slow.evaluate((image) => image.complete && image.naturalWidth > 0)).toBe(true);
    expect(server.records.filter((entry) => entry.url === slowUrl)).toHaveLength(1);
    const tab = page.getByRole("tab", { name: "Audio", exact: true });
    await tab.scrollIntoViewIfNeeded();
    if (isMobile) await tab.tap();
    else { await tab.focus(); await page.keyboard.press("Enter"); }
    await expect(tab).toHaveAttribute("aria-selected", "true");
    await expect.poll(() => original.every((record) => record.closed)).toBe(true);
    await expect(page.locator(".media-card.audio").first()).toBeVisible();
    expect(server.maximum).toBeLessThanOrEqual(4);
  } finally { await page.goto("about:blank"); await server.close(); }
});
