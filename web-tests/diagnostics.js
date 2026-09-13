import { test as base, expect } from "@playwright/test";

// Opt-in observations shared by every project. Keep diagnostic state bounded;
// do not change media methods, application timers, or readiness conditions.
export const test = base.extend({
  page: async ({ page, browser }, use, testInfo) => {
    if (!process.env.RUSTY_DLNA_BROWSER_EVIDENCE) return use(page);
    const network = [];
    const pending = new Map();
    let droppedNetwork = 0;
    let truncated = false;
    const text = (value) => {
      if (value === undefined) return undefined;
      const string = String(value);
      if (string.length > 4000) truncated = true;
      return string.slice(0, 4000);
    };
    page.on("request", (request) => {
      if (!["document", "script", "stylesheet"].includes(request.resourceType())
        && !/\/api\/|\/web\/media\//.test(request.url())) return;
      if (network.length >= 500) { droppedNetwork++; return; }
      const record = { url: text(request.url()), resourceType: request.resourceType(),
        timing: request.timing(), outcome: "pending" };
      network.push(record);
      pending.set(request, record);
    });
    const finish = (request, outcome) => {
      const record = pending.get(request);
      if (!record) return;
      Object.assign(record, { timing: request.timing(), outcome,
        ...(outcome === "failed" ? { error: text(request.failure()?.errorText) } : {}) });
      pending.delete(request);
    };
    page.on("requestfinished", (request) => finish(request, "finished"));
    page.on("requestfailed", (request) => finish(request, "failed"));
    await page.addInitScript(() => {
      const events = [];
      const text = (value) => {
        if (value === undefined || value === null) return value;
        const string = String(value);
        if (string.length > 4000) window.__browserEvidence.truncated = true;
        return string.slice(0, 4000);
      };
      const record = (kind, value = {}) => {
        if (events.length < 2000) events.push({ at: performance.now(), kind, ...value });
        else window.__browserEvidence.droppedEvents++;
      };
      window.__browserEvidence = { events, droppedEvents: 0, truncated: false, longTasksSupported:
        PerformanceObserver.supportedEntryTypes.includes("longtask") };
      const focusState = () => {
        const active = document.activeElement;
        const controls = document.getElementById("playback-controls");
        const style = controls && getComputedStyle(controls);
        return { active: text(active?.id || active?.tagName), focusVisible: active?.matches(":focus-visible"),
          documentFocused: document.hasFocus(), stageClass: text(document.getElementById("player-stage")?.className),
          controlsVisibility: style?.visibility, controlsOpacity: style?.opacity };
      };
      for (const name of ["focusin", "focusout", "pointerenter", "pointerleave", "keydown"]) {
        document.addEventListener(name, (event) => {
          if (!(event.target instanceof Element)
            || !event.target.closest("#player-stage, #layout-watch, #layout-browse")) return;
          record(name, { target: text(event.target.id || event.target.tagName),
            related: text(event.relatedTarget?.id || event.relatedTarget?.tagName), key: text(event.key), ...focusState() });
        }, true);
      }
      if (window.__browserEvidence.longTasksSupported) new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) record("longtask", { start: entry.startTime, duration: entry.duration });
      }).observe({ type: "longtask", buffered: true });
      for (const name of ["loadedmetadata", "loadeddata", "canplay", "playing", "pause", "waiting", "stalled", "seeking", "seeked", "ended", "error", "emptied"]) {
        document.addEventListener(name, (event) => {
          const media = event.target;
          if (!(media instanceof HTMLMediaElement)) return;
          record(name, { currentTime: media.currentTime, readyState: media.readyState,
            paused: media.paused, error: media.error?.code, source: text(media.currentSrc) });
        }, true);
      }
      document.addEventListener("DOMContentLoaded", () => {
        record("domcontentloaded");
        for (const id of ["server-state", "loading", "library-panel", "player-stage"]) {
          const element = document.getElementById(id);
          if (element) new MutationObserver(() => record("state", {
            id, state: text(element.dataset.state), busy: text(element.getAttribute("aria-busy")), hidden: element.hidden,
            ...(id === "player-stage" ? focusState() : {}),
          })).observe(element, { attributes: true, attributeFilter: ["data-state", "aria-busy", "hidden", ...(id === "player-stage" ? ["class"] : [])] });
        }
      }, { once: true });
      window.addEventListener("load", () => record("load"), { once: true });
    });
    await use(page);
    let documentEvidence;
    try {
      documentEvidence = await page.evaluate(() => {
        const navigation = performance.getEntriesByType("navigation").slice(0, 16).map((entry) => {
          const value = entry.toJSON();
          if (value.name.length > 4000 && window.__browserEvidence) window.__browserEvidence.truncated = true;
          value.name = value.name.slice(0, 4000);
          return value;
        });
        return { ...window.__browserEvidence, navigation };
      });
    } catch (error) { documentEvidence = { unavailable: text(error.message) }; }
    // Context teardown can abort a request only after this fixture attaches its
    // evidence. Preserve requests still waiting for headers/body at that point.
    for (const [request, record] of pending) record.timing = request.timing();
    await testInfo.attach("browser-evidence", { contentType: "application/json", body: Buffer.from(JSON.stringify({
      browser: text(browser.version()), project: text(testInfo.project.name), network, droppedNetwork,
      document: documentEvidence, truncated: truncated || Boolean(documentEvidence.truncated),
    })) });
  },
});

export { expect };
