import test from "node:test";
import assert from "node:assert/strict";
import { reusableMediaSourceSeek, retainedMediaSourceBytes, parseHlsMediaPlaylist } from "./core.js";
import { fetchResource, MEDIA_SOURCE_RESOURCE_MAX_BYTES } from "./media-source.js";
import { PreviewCache, PREVIEW_RETAINED_MAX_BYTES } from "./preview-cache.js";
import { PlaybackTiming, playbackTimingSnapshot } from "./playback-timing.js";

const context = { ranges: [{ start: 3, end: 14 }], segments: [
  { start: 3, end: 6, decodeStart: 3, bytes: 10 },
  { start: 6, end: 10, decodeStart: 3, bytes: 20 },
  { start: 10, end: 14, decodeStart: 10, bytes: 15 },
], copiedVideo: true, duration: 20 };

test("copied buffered seeks require retained GOP prerequisites and reorder margin", () => {
  assert.equal(reusableMediaSourceSeek({ ...context, target: 8 }), true);
  assert.equal(reusableMediaSourceSeek({ ...context, target: 12 }), true);
  assert.equal(reusableMediaSourceSeek({ ...context, target: 12.01 }), false);
  assert.equal(reusableMediaSourceSeek({ ...context, target: 8, ranges: [{ start: 4, end: 14 }] }), false);
  assert.equal(reusableMediaSourceSeek({ ...context, target: 8, ranges: [{ start: 3, end: 7 }, { start: 7.1, end: 14 }] }), false);
  assert.equal(reusableMediaSourceSeek({ ...context, target: 8, segments: [{ start: 3, end: 14 }] }), false);
  assert.equal(reusableMediaSourceSeek({ ...context, target: 13.5, duration: 14 }), true);
  assert.equal(reusableMediaSourceSeek({ ...context, target: -1 }), false);
});

test("buffer estimates charge burst fragments until their entire range is gone", () => {
  assert.equal(retainedMediaSourceBytes(context.segments, [{ start: 5.999, end: 10.001 }]), 45);
  assert.equal(retainedMediaSourceBytes(context.segments, [{ start: 6, end: 10 }]), 20);
  assert.equal(retainedMediaSourceBytes(context.segments, []), 0);
});

test("MSE playlist random-access timing stays numeric, bounded and local", () => {
  const playlist = '#EXTM3U\n#EXT-X-MAP:URI="/web/media/1.mp4?delivery=mse_init&hls_offset=0&hls_length=1"\n#EXT-X-RUSTY-TIMING:6.000000,3.000000\n#EXTINF:1,\n/web/media/1.m4s?delivery=mse_segment&hls_offset=1&hls_length=1\n#EXT-X-ENDLIST\n';
  const parse = (text) => parseHlsMediaPlaylist(text, "http://localhost/web/media/1.m3u8?delivery=mse");
  assert.deepEqual(parse(playlist).segments.map(({ start, decodeStart, duration }) => ({ start, decodeStart, duration })), [{ start: 6, decodeStart: 3, duration: 1 }]);
  for (const invalid of ["6,7", "NaN,0", "6,-1", "Infinity,0", "6,0,1"]) {
    assert.equal(parse(playlist.replace("6.000000,3.000000", invalid)), null);
  }
});

test("streaming MSE bodies below, at and above 32 MiB stay bounded with or without length", async () => {
  const previousWindow = globalThis.window;
  const previousFetch = globalThis.fetch;
  globalThis.window = globalThis;
  try {
    for (const declared of [false, true]) {
      for (const delta of [-1, 0, 1]) {
        let cancelled = false;
        let delivered = 0;
        const length = MEDIA_SOURCE_RESOURCE_MAX_BYTES + delta;
        globalThis.fetch = async () => new Response(new ReadableStream({
          pull(stream) {
            if (delivered === length) { if (delta <= 0) stream.close(); return; }
            const count = Math.min(1024 * 1024, length - delivered);
            delivered += count;
            stream.enqueue(new Uint8Array(count));
          }, cancel() { cancelled = true; },
        }), declared ? { headers: { "content-length": String(length) } } : {});
        const operation = fetchResource("http://localhost/fragment", new AbortController().signal);
        if (delta > 0) {
          await assert.rejects(operation, { code: "resource_limit" });
          if (!declared) assert.equal(cancelled, true);
          assert.ok(delivered <= MEDIA_SOURCE_RESOURCE_MAX_BYTES + 1);
        } else assert.equal((await operation).byteLength, length);
      }
    }
  } finally { globalThis.fetch = previousFetch; globalThis.window = previousWindow; }
});

test("100 rapid scrub targets keep one fetch and only the latest pending target", async () => {
  let active = 0;
  let maximum = 0;
  const fetched = [];
  const cache = new PreviewCache(async (url, { signal }) => {
    active += 1; maximum = Math.max(maximum, active); fetched.push(url);
    try {
      await new Promise((resolve, reject) => {
        const timer = setTimeout(resolve, 2);
        signal.addEventListener("abort", () => { clearTimeout(timer); reject(new DOMException("replaced", "AbortError")); }, { once: true });
      });
      return url;
    } finally { active -= 1; }
  }, async () => ({ width: 2880, height: 3780, close() {} }));
  const requests = Array.from({ length: 100 }, (_, index) => cache.request(String(index)).catch(() => null));
  assert.equal(cache.snapshot().active, 1);
  assert.equal(cache.snapshot().pendingTargets, 1);
  await Promise.all(requests);
  assert.equal(maximum, 1);
  assert.deepEqual(fetched, ["0", "99"]);
  assert.equal(cache.snapshot().retained, 1);
  assert.ok(cache.snapshot().retainedBytes <= PREVIEW_RETAINED_MAX_BYTES);
  cache.cancel();
  assert.equal(cache.snapshot().retainedBytes, 0);
});

test("preview preloading deduplicates against an active scrub and has bounded retained bytes", async () => {
  let calls = 0;
  const cache = new PreviewCache(async () => { calls += 1; return null; }, async () => ({ width: 2880, height: 3780, close() {} }));
  const first = cache.request("one");
  await Promise.all([first, cache.request("one")]);
  assert.equal(calls, 1);
  await cache.request("two");
  assert.equal(cache.snapshot().retained, 1);
  cache.preload(Array.from({ length: 100 }, (_, index) => String(index)));
  assert.ok(cache.snapshot().pendingSpeculative <= 8);
  cache.cancel();
});

test("browser timings retain 64 sanitized records and fixed-cardinality histograms", () => {
  const source = { active: true, item: { title: "secret", path: "/secret" }, plan: {
    sourceMode: "original", outputQuality: "auto", streamNegotiation: null,
  } };
  for (let index = 0; index < 100; index += 1) {
    const timing = new PlaybackTiming("selection");
    timing.mark("negotiation_complete"); timing.mark("/secret"); timing.finish(source); timing.finish(source);
  }
  const snapshot = playbackTimingSnapshot();
  assert.equal(snapshot.records.length, 64);
  assert.equal(snapshot.histograms["selection:presented"].count, 100);
  assert.equal(snapshot.histograms["selection:presented"].buckets.length, 11);
  assert.equal(JSON.stringify(snapshot).includes("secret"), false);
});

test("paused seek timing preserves the frame observation before later confirmation exactly once", () => {
  const source = { active: true, plan: { sourceMode: "compatible", mediaSourceDelivery: true } };
  const timing = new PlaybackTiming("seek_restarted", performance.now() - 5);
  const observedAt = timing.startedAt + 2;
  timing.finish(source, false, Infinity);
  assert.equal(timing.finished, false);
  timing.finish(source, false, observedAt);
  timing.finish(source, false, observedAt + 1);
  const snapshot = playbackTimingSnapshot();
  assert.equal(snapshot.histograms["seek_restarted:presented"].count, 1);
  assert.equal(snapshot.records.at(-1).duration_ms, 2);
  assert.equal(snapshot.records.at(-1).stages.first_presented_frame, 2);
  assert.equal(snapshot.records.at(-1).estimated, false);
});

test("bursty MSE buffering stops at its byte budget before fetching another fragment", async () => {
  const { pumpMediaSource } = await import("./media-source.js");
  const old = { window: globalThis.window, fetch: globalThis.fetch, media: globalThis.HTMLMediaElement };
  globalThis.window = globalThis;
  globalThis.HTMLMediaElement = { HAVE_FUTURE_DATA: 3 };
  const abort = new AbortController();
  let fetched = 0;
  let end = 0;
  let appends = 0;
  let controller;
  const sourceBuffer = Object.assign(new EventTarget(), {
    updating: false, buffered: { get length() { return end > 0 ? 1 : 0; }, start: () => 0, end: () => end },
    appendBuffer() { if (appends++ > 0) end += 1; queueMicrotask(() => this.dispatchEvent(new Event("updateend"))); },
  });
  const player = Object.assign(new EventTarget(), { paused: false, currentTime: 0, readyState: 3 });
  const mediaSource = { readyState: "open", addSourceBuffer: () => sourceBuffer, endOfStream() {} };
  globalThis.fetch = async (url) => {
    if (new URL(url).searchParams.get("delivery") === "mse") return new Response('#EXTM3U\n#EXT-X-MAP:URI="/web/media/1.mp4?delivery=mse_init&hls_offset=0&hls_length=1"\n'
      + Array.from({ length: 4 }, (_, index) => `#EXT-X-RUSTY-TIMING:${index},${index}\n#EXTINF:1,\n/web/media/1.m4s?delivery=mse_segment&hls_offset=${index + 1}&hls_length=32\n`).join("") + '#EXT-X-ENDLIST\n');
    if (new URL(url).searchParams.get("delivery") === "mse_segment") fetched += 1;
    return new Response(new Uint8Array(new URL(url).searchParams.get("delivery") === "mse_init" ? 1 : 32));
  };
  try {
    const pump = pumpMediaSource({ player, mediaSource, signal: abort.signal, playlistUrl: "http://localhost/web/media/1.m3u8?delivery=mse",
      contentType: "video/mp4", reportStartup() {}, resourceMaxBytes: 32, bufferMaxBytes: 97,
      onController(value) { controller = value; } });
    await new Promise((resolve) => setTimeout(resolve, 30));
    assert.equal(fetched, 3);
    assert.equal(controller.snapshot().bufferedBytes, 97);
    abort.abort();
    await assert.rejects(pump, { name: "AbortError" });
  } finally { globalThis.window = old.window; globalThis.fetch = old.fetch; globalThis.HTMLMediaElement = old.media; }
});

test("quota recovery prunes a whole copied GOP during a paused distant seek", async () => {
  const { pumpMediaSource } = await import("./media-source.js");
  const old = { window: globalThis.window, fetch: globalThis.fetch, media: globalThis.HTMLMediaElement };
  globalThis.window = globalThis; globalThis.HTMLMediaElement = { HAVE_FUTURE_DATA: 3 };
  let start = 0;
  let end = 0;
  let attempts = 0;
  let quota = true;
  const removals = [];
  const sourceBuffer = Object.assign(new EventTarget(), { updating: false,
    buffered: { get length() { return end > start ? 1 : 0; }, start: () => start, end: () => end },
    appendBuffer() {
      attempts += 1;
      if (attempts === 4 && quota) { quota = false; throw new DOMException("full", "QuotaExceededError"); }
      if (attempts > 1) end += 3;
      queueMicrotask(() => this.dispatchEvent(new Event("updateend")));
    },
    remove(from, to) { removals.push([from, to]); start = to; queueMicrotask(() => this.dispatchEvent(new Event("updateend"))); },
  });
  const player = Object.assign(new EventTarget(), { paused: true, currentTime: 0, readyState: 2, seeking: true });
  const mediaSource = { readyState: "open", addSourceBuffer: () => sourceBuffer, endOfStream() {} };
  globalThis.fetch = async (url) => new Response(new URL(url).searchParams.get("delivery") === "mse"
    ? '#EXTM3U\n#EXT-X-MAP:URI="/web/media/1.mp4?delivery=mse_init&hls_offset=0&hls_length=1"\n'
      + [0, 3, 6].map((time) => `#EXT-X-RUSTY-TIMING:${time},${time}\n#EXTINF:3,\n/web/media/1.m4s?delivery=mse_segment&hls_offset=${time + 1}&hls_length=1\n`).join("") + '#EXT-X-ENDLIST\n'
    : new Uint8Array([1]));
  try {
    await pumpMediaSource({ player, mediaSource, signal: new AbortController().signal,
      playlistUrl: "http://localhost/web/media/1.m3u8?delivery=mse", contentType: "video/mp4",
      reportStartup() {}, copiedVideo: true, pendingSeek: () => 8 });
    assert.equal(quota, false);
    assert.ok(removals.some(([, to]) => to === 3));
    assert.equal(attempts, 5);
  } finally { globalThis.window = old.window; globalThis.fetch = old.fetch; globalThis.HTMLMediaElement = old.media; }
});

for (const operation of ["append", "prune", "abort", "deadline"]) {
  test(`a nearby seek waits for owned buffer work and rechecks after ${operation}`, async () => {
    const { pumpMediaSource } = await import("./media-source.js");
    const old = { window: globalThis.window, fetch: globalThis.fetch, media: globalThis.HTMLMediaElement };
    globalThis.window = globalThis; globalThis.HTMLMediaElement = { HAVE_FUTURE_DATA: 3 };
    const abort = new AbortController();
    let start = 0;
    let end = 0;
    let appends = 0;
    let controller;
    let release;
    let held;
    const operationStarted = new Promise((resolve) => { held = resolve; });
    const sourceBuffer = Object.assign(new EventTarget(), { updating: false,
      buffered: { get length() { return end > start ? 1 : 0; }, start: () => start, end: () => end },
      appendBuffer() {
        const index = appends++;
        this.updating = true;
        const complete = () => {
          if (index > 0) end += 4;
          this.updating = false; this.dispatchEvent(new Event("updateend"));
        };
        if (index === 4 && operation !== "prune") { release = complete; held(); }
        else queueMicrotask(complete);
      },
      remove(from, to) {
        this.updating = true;
        release = () => { start = to; this.updating = false; this.dispatchEvent(new Event("updateend")); };
        held();
      },
    });
    const player = Object.assign(new EventTarget(), { paused: false, currentTime: 0, readyState: 3 });
    const mediaSource = { readyState: "open", addSourceBuffer: () => sourceBuffer, endOfStream() {} };
    globalThis.fetch = async (url) => new Response(new URL(url).searchParams.get("delivery") === "mse"
      ? '#EXTM3U\n#EXT-X-MAP:URI="/web/media/1.mp4?delivery=mse_init&hls_offset=0&hls_length=1"\n'
        + [0, 4, 8, 12, 16].map((time) => `#EXT-X-RUSTY-TIMING:${time},${time}\n#EXTINF:4,\n/web/media/1.m4s?delivery=mse_segment&hls_offset=${time + 1}&hls_length=1\n`).join("") + '#EXT-X-ENDLIST\n'
      : new Uint8Array([1]));
    const pump = pumpMediaSource({ player, mediaSource, signal: abort.signal,
      playlistUrl: "http://localhost/web/media/1.m3u8?delivery=mse", contentType: "video/mp4", copiedVideo: true,
      reportStartup() {}, onController(value) { controller = value; },
      onBuffered() { if (end >= 12) player.currentTime = operation === "prune" && end >= 16 ? 14 : 5; } });
    try {
      await operationStarted;
      assert.equal(controller.canSeek(4), false);
      const superseded = controller.waitForSeek(4);
      const latest = controller.waitForSeek(6);
      assert.ok(superseded instanceof Promise);
      assert.ok(latest instanceof Promise);
      await superseded;
      player.paused = true;
      if (operation === "abort") abort.abort();
      else if (operation === "deadline") {
        await latest;
        assert.equal(sourceBuffer.updating, true);
        assert.equal(controller.canSeek(6), false);
        release();
      } else release();
      await latest;
      assert.equal(controller.canSeek(6), operation === "append" || operation === "deadline");
      abort.abort();
      await assert.rejects(pump, { name: "AbortError" });
    } finally { abort.abort(); globalThis.window = old.window; globalThis.fetch = old.fetch; globalThis.HTMLMediaElement = old.media; }
  });
}

test("preview bodies without a declared length stop at 16 MiB and stale reads abort immediately", async () => {
  const { WebApi } = await import("./api.js");
  const old = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = globalThis;
  let cancellations = 0;
  const api = new WebApi();
  try {
    globalThis.fetch = async () => new Response(new ReadableStream({ start(stream) {
      stream.enqueue(new Uint8Array(16 * 1024 * 1024)); stream.enqueue(new Uint8Array(1));
    }, cancel() { cancellations += 1; } }));
    await assert.rejects(api.previewSheet("http://localhost/image"), /resource budget/);
    assert.equal(cancellations, 2); // bounded cache retry and reload attempt
    const abort = new AbortController();
    globalThis.fetch = async () => new Response(new ReadableStream({ start() { setTimeout(() => abort.abort(), 0); },
      cancel() { cancellations += 1; } }));
    await assert.rejects(api.previewSheet("http://localhost/held", { signal: abort.signal }), { name: "AbortError" });
    assert.equal(cancellations, 3);
  } finally { globalThis.window = old.window; globalThis.fetch = old.fetch; }
});

test("server timing reports skip out-of-range durations without clipping browser latency", async () => {
  const { WebApi } = await import("./api.js");
  const original = globalThis.fetch;
  const reported = [];
  globalThis.fetch = async (url) => { reported.push(new URL(url, "http://localhost").searchParams.get("elapsed_ms"));
    return new Response(JSON.stringify({ schema_version: 2 })); };
  try {
    const api = new WebApi();
    for (const elapsed of [0, 119999.5, 120000, 120000.1, -1, NaN, Infinity]) {
      await api.reportTranscodeStartup("1", 1, 1, "selection_to_frame", null, elapsed);
    }
    assert.deepEqual(reported, ["0", "120000", "120000"]);
  } finally { globalThis.fetch = original; }
});

test("speculative previews never duplicate an active or pending scrub request", async () => {
  const calls = [];
  const completions = new Map();
  const cache = new PreviewCache(async (url, { signal }) => {
    calls.push(url);
    await new Promise((resolve, reject) => {
      completions.set(url, resolve);
      signal.addEventListener("abort", () => reject(new DOMException("replaced", "AbortError")), { once: true });
    });
    return null;
  }, async () => ({ width: 100, height: 100, close() {} }));
  const first = cache.request("one");
  cache.preload(["one", "two", "one"]);
  assert.equal(cache.snapshot().pendingSpeculative, 1);
  completions.get("one")();
  await first;
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(calls, ["one", "two"]);
  const third = cache.request("three");
  cache.preload(["one", "two", "three", "four"]);
  assert.equal(cache.snapshot().pendingSpeculative, 1);
  await new Promise((resolve) => setImmediate(resolve));
  completions.get("three")();
  await third;
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(calls, ["one", "two", "three", "four"]);
  cache.cancel();
});

test("preview JPEG headers reject oversized dimensions before browser decoding", async () => {
  const { previewJpegDimensions, PREVIEW_HEADER_MAX_BYTES } = await import("./preview-cache.js");
  const header = (width, height) => new Uint8Array([0xff, 0xd8, 0xff, 0xc0, 0, 11, 8,
    height >> 8, height & 255, width >> 8, width & 255, 1, 1, 0x11, 0,
    0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0]);
  for (const [width, height] of [[160, 90], [2880, 3780], [3000, 4000]]) {
    assert.deepEqual(previewJpegDimensions(header(width, height)), { width, height });
  }
  for (const [width, height] of [[3001, 4000], [65535, 65535], [0, 1], [4097, 1]]) {
    assert.equal(previewJpegDimensions(header(width, height)), null);
  }
  const hugeHeader = new Uint8Array(PREVIEW_HEADER_MAX_BYTES + 64);
  hugeHeader.set([0xff, 0xd8]);
  for (let offset = 2; offset + 65537 < hugeHeader.length; offset += 65537) {
    hugeHeader.set([0xff, 0xe1, 0xff, 0xff], offset);
  }
  hugeHeader.set(header(160, 90).slice(2), PREVIEW_HEADER_MAX_BYTES);
  assert.equal(previewJpegDimensions(hugeHeader), null);
  for (const invalid of [new Uint8Array(), header(160, 90).slice(0, -1),
    new Uint8Array([0xff, 0xd8, 0xff, 0xda, 0, 2]), new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0, 1])]) {
    assert.equal(previewJpegDimensions(invalid), null);
  }
  const contradictory = new Uint8Array([...header(160, 90).slice(0, -10), ...header(65535, 65535).slice(2)]);
  assert.equal(previewJpegDimensions(contradictory), null);
  const originalImage = globalThis.Image;
  let decoderStarts = 0;
  globalThis.Image = class { constructor() { decoderStarts += 1; } };
  const cache = new PreviewCache(async () => new Blob([header(65535, 65535)], { type: "image/jpeg" }));
  try {
    await assert.rejects(cache.request("oversized"), /dimensions/);
    assert.equal(decoderStarts, 0);
  } finally { cache.cancel(); globalThis.Image = originalImage; }
  const progressive = header(160, 90); progressive[3] = 0xc2;
  assert.deepEqual(previewJpegDimensions(progressive), { width: 160, height: 90 });
});

test("preview reservation evicts retained bitmaps before starting a large decode", async () => {
  let cache;
  const duringDecode = [];
  cache = new PreviewCache(async () => null, async (_, signal, reserve) => {
    reserve(48 * 1024 * 1024);
    duringDecode.push(cache.snapshot().retainedBytes);
    return { width: 3000, height: 4000, retainedBytes: 48 * 1024 * 1024, close() {} };
  });
  await cache.request("one");
  await cache.request("two");
  assert.deepEqual(duringDecode, [0, 0]);
  cache.cancel();
});
