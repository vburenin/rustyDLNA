#!/usr/bin/env node
// Run against an isolated scripts/playwright-server.sh server. Reports stay outside Git.
import { chromium } from "@playwright/test";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { cpus, release, totalmem } from "node:os";
import { resolve, join } from "node:path";

const args = Object.fromEntries(process.argv.slice(2).map((arg) => {
  const [key, ...value] = arg.replace(/^--/, "").split("=");
  return [key, value.join("=") || true];
}));
if (args.help) {
  console.log("Usage: node scripts/library-browser-benchmark.mjs --url=http://127.0.0.1:18201 --output=/tmp/library-before.json [--samples=5] [--cards=10000] [--cpu-rate=4] [--assets=crates/server/web] [--diagnose-retention] [--heap-snapshot=/tmp/library.heapsnapshot]");
  process.exit(0);
}
const url = new URL(args.url || "http://127.0.0.1:18201");
if (!["127.0.0.1", "localhost"].includes(url.hostname) || ["", "8200"].includes(url.port)) {
  throw new Error("Use an isolated loopback test server and explicit non-live port.");
}
const samples = Number(args.samples || 5);
const cards = Number(args.cards || 10000);
const cpuRate = Number(args["cpu-rate"] || 4);
if (!Number.isInteger(samples) || samples < 1 || samples > 20 || !Number.isInteger(cards)
  || cards < 500 || cards > 50000 || !Number.isFinite(cpuRate) || cpuRate < 1 || cpuRate > 8) {
  throw new Error("Bounded workload requires 1–20 samples, 500–50000 cards and 1–8 CPU rate.");
}
const output = resolve(String(args.output || "/tmp/rustydlna-library-browser.json"));
const browser = await chromium.launch({ headless: true });
const report = {
  environment: { node: process.version, browser: browser.version(), cpu: cpus()[0]?.model,
    logicalCpus: cpus().length, memoryBytes: totalmem(), kernel: release() },
  workload: { samples, cards, cpuRate, assets: args.assets || "embedded", url: url.href,
    note: "CPU throttling simulates constrained execution; not a physical mobile-device result. New context per sample; five list/title cycles per context. CDP heap after explicit GC. Readiness predicates return booleans so Playwright does not retain element handles. Timer lag measures main-thread scheduling, not trusted user-event latency." },
  samples: [],
};
try {
  for (let sample = 0; sample < samples; sample += 1) {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
    const page = await context.newPage();
    const session = await context.newCDPSession(page);
    await session.send("Performance.enable");
    await session.send("Emulation.setCPUThrottlingRate", { rate: cpuRate });
    if (args.assets) await page.route("**/web/*", async (route) => {
      const name = new URL(route.request().url()).pathname.slice("/web/".length);
      if (!/^[a-z-]+\.(js|css)$/.test(name)) return route.fallback();
      await route.fulfill({ body: await readFile(join(resolve(String(args.assets)), name)),
        contentType: name.endsWith(".js") ? "text/javascript" : "text/css" });
    });
    const template = await (await context.request.get(new URL("/api/web/library?view=library&kind=video", url).href)).json();
    const item = template.entries.find((entry) => entry.title === "tagged");
    if (!item) throw new Error("Expected deterministic tagged fixture from playwright-server.sh");
    await page.route("**/api/web/library?**", async (route) => {
      const params = new URL(route.request().url()).searchParams;
      if (params.get("kind") !== "video") return route.fallback();
      const offset = Number(params.get("offset"));
      const limit = Number(params.get("limit"));
      await route.fulfill({ json: { ...template, offset, limit, total: cards, has_more: offset + limit < cards,
        entries: Array.from({ length: Math.min(limit, cards - offset) }, (_, index) => ({
          ...item, id: String(100000 + offset + index), title: `Large library ${offset + index + 1}`,
          collection: offset + index >= cards / 2 && offset + index < cards / 2 + 100
            ? { id: "scale-collection", title: "Large collection", sequence: index } : null,
          art_url: null,
        })),
      } });
    });
    await page.addInitScript(() => {
      localStorage.setItem("rustydlna.stream", "direct");
      window.__scale = { tasks: [], lag: [], mutations: 0 };
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) window.__scale.tasks.push({ start: entry.startTime, duration: entry.duration });
      }).observe({ type: "longtask", buffered: true });
      let previous = performance.now();
      setInterval(() => {
        const now = performance.now();
        window.__scale.lag.push({ start: previous, duration: Math.max(0, now - previous - 16) });
        previous = now;
      }, 16);
    });
    const metrics = async () => Object.fromEntries((await session.send("Performance.getMetrics")).metrics.map(({ name, value }) => [name, value]));
    const measure = async (action) => {
      const before = await metrics();
      const start = await page.evaluate(() => performance.now());
      await action();
      await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
      const after = await metrics();
      return { ...(await page.evaluate((since) => {
        const tasks = window.__scale.tasks.filter((entry) => entry.start >= since);
        return { elapsedMs: performance.now() - since, longTasks: tasks.length,
          maxLongTaskMs: Math.max(0, ...tasks.map((entry) => entry.duration)),
          totalLongTaskMs: tasks.reduce((total, entry) => total + entry.duration, 0),
          maxTimerLagMs: Math.max(0, ...window.__scale.lag.filter((entry) => entry.start >= since).map((entry) => entry.duration)) };
      }, start)), layoutMs: (after.LayoutDuration - before.LayoutDuration) * 1000,
      scriptMs: (after.ScriptDuration - before.ScriptDuration) * 1000,
      taskMs: (after.TaskDuration - before.TaskDuration) * 1000 };
    };
    await page.goto(url.href, { waitUntil: "networkidle" });
    const ready = () => page.waitForFunction((count) => document.querySelectorAll(".media-card.video").length === count
      && Boolean(document.querySelector(".media-chunk.sized")), cards, { timeout: 60000 });
    const initial = await measure(async () => { await page.getByRole("tab", { name: "Videos" }).click(); await ready(); });
    const resize = await measure(async () => { await page.setViewportSize({ width: 820, height: 900 }); });
    const repeatResize = await measure(async () => { await page.setViewportSize({ width: 1280, height: 900 }); });
    const retained = [];
    let tick;
    for (let cycle = 0; cycle < 5; cycle += 1) {
      await page.evaluate(() => [...document.querySelectorAll(".media-card.video .card-button")].at(-1).click());
      await page.waitForFunction(() => document.querySelector("#video-player").getAttribute("src"));
      if (!tick) tick = await page.evaluate(() => {
        const video = document.querySelector("#video-player");
        const observer = new MutationObserver(() => {});
        observer.observe(document.querySelector("#player-stage"), { subtree: true, attributes: true, childList: true, characterData: true });
        const start = performance.now();
        for (let index = 0; index < 200; index += 1) video.dispatchEvent(new Event("timeupdate"));
        const result = { events: 200, elapsedMs: performance.now() - start, mutations: observer.takeRecords().length };
        observer.disconnect();
        return result;
      });
      await page.getByRole("button", { name: "Close player", exact: true }).evaluate((button) => button.click());
      await page.getByRole("tab", { name: "Audio", exact: true }).click();
      await page.waitForFunction(() => !document.querySelector(".media-card.video") && document.querySelector("#library-panel")?.getAttribute("aria-busy") === "false");
      await page.getByRole("tab", { name: "Videos", exact: true }).click();
      await ready();
      await session.send("HeapProfiler.collectGarbage");
      const memory = { cycle, ...(await session.send("Memory.getDOMCounters")),
        heapBytes: (await metrics()).JSHeapUsedSize };
      if (args["diagnose-retention"]) {
        await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
        await session.send("HeapProfiler.collectGarbage");
        memory.settled = { ...(await session.send("Memory.getDOMCounters")),
          heapBytes: (await metrics()).JSHeapUsedSize };
        await session.send("DOM.enable");
        const detached = await session.send("DOM.getDetachedDomNodes");
        memory.detached = detached.detachedNodes.map(({ treeNode, retainedNodeIds }) => ({
          name: treeNode.nodeName, attributes: treeNode.attributes, children: treeNode.childNodeCount,
          retained: retainedNodeIds.length,
        }));
        const media = await session.send("Runtime.evaluate", { expression: 'document.querySelector("#video-player")' });
        memory.mediaListeners = (await session.send("DOMDebugger.getEventListeners", {
          objectId: media.result.objectId,
        })).listeners.map((listener) => listener.type);
        await session.send("Runtime.releaseObject", { objectId: media.result.objectId });
      }
      retained.push(memory);
    }
    report.samples.push({ initial, resize, repeatResize, tick, retained });
    console.log(JSON.stringify({ sample, ...report.samples.at(-1) }));
    if (args["heap-snapshot"]) {
      const chunks = [];
      session.on("HeapProfiler.addHeapSnapshotChunk", ({ chunk }) => chunks.push(chunk));
      await session.send("HeapProfiler.takeHeapSnapshot", { reportProgress: false });
      await writeFile(String(args["heap-snapshot"]), chunks.join(""));
    }
    await context.close();
  }
} finally {
  await browser.close();
  await mkdir(resolve(output, ".."), { recursive: true });
  await writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
}
