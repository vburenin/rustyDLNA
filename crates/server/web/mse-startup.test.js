import test from "node:test";
import assert from "node:assert/strict";
import { mediaSourceStartupBudget, parseHlsMediaPlaylist } from "./core.js";
import { fetchResource, fetchStartupResources, MEDIA_SOURCE_RESOURCE_MAX_BYTES } from "./media-source.js";

const base = "http://localhost/web/media/42.m3u8?delivery=mse&request=7&session=9&start=30";
const text = '#EXTM3U\n#EXT-X-MAP:URI="/web/media/42.mp4?delivery=mse_init&request=7&session=9&start=30&hls_offset=0&hls_length=2"\n#EXTINF:2,\n/web/media/42.m4s?delivery=mse_segment&request=7&session=9&start=30&hls_offset=2&hls_length=3\n#EXT-X-ENDLIST\n';
const playlist = parseHlsMediaPlaylist(text, base);
const budget = { initBytes: 2, mediaBytes: 3, totalBytes: 5 };

test("startup resources retain title, source generation, recipe and exact numeric ranges", () => {
  assert.ok(playlist);
  for (const [before, after] of [["42.m4s", "43.m4s"], ["request=7", "request=8"],
    ["session=9", "session=10"], ["start=30", "start=0"], ["request=7", "request=7&request=7"],
    ["request=7", "request=7&quality=sd_480"], ["request=7", "request=7&mode=compat"],
    ["hls_offset=2", "hls_offset=9007199254740991"], ["hls_length=3", "hls_length=9007199254740992"]]) {
    assert.equal(parseHlsMediaPlaylist(text.replace(before, after), base), null, after);
  }
});

test("startup overlap shares one resource budget and defers to slow-link/save-data hints", () => {
  assert.deepEqual(mediaSourceStartupBudget(playlist, 5, 5), budget);
  assert.deepEqual(mediaSourceStartupBudget(playlist, 5, 5, { effectiveType: "4g", downlink: 3 }), budget);
  for (const connection of [{ saveData: true }, { effectiveType: "3g" }, { effectiveType: "2g" }, { downlink: 2 }, { downlink: 0.5 }]) {
    assert.equal(mediaSourceStartupBudget(playlist, 5, 5, connection), null);
  }
  assert.equal(mediaSourceStartupBudget(playlist, 4, 5), null);
  assert.equal(mediaSourceStartupBudget(playlist, 5, 4), null);
  assert.equal(mediaSourceStartupBudget({ ...playlist, segments: [] }, 5, 5), null);
  const hugeInit = { ...playlist, initUrl: playlist.initUrl.replace("hls_length=2", "hls_length=65537") };
  assert.equal(mediaSourceStartupBudget(hugeInit, 1e6, 1e6), null);
});

function mockNetwork(t, implementation) {
  const old = { fetch: globalThis.fetch, window: globalThis.window };
  globalThis.window = globalThis;
  globalThis.fetch = implementation;
  t.after(() => { globalThis.fetch = old.fetch; globalThis.window = old.window; });
}
const tick = () => new Promise((resolve) => setImmediate(resolve));

test("init and first fragment overlap, retain exact bytes, and start no third fetch", async (t) => {
  const requests = [];
  const held = [];
  mockNetwork(t, async (url, { signal }) => {
    requests.push({ url, signal });
    return new Response(new ReadableStream({ start(controller) { held.push(controller); } }));
  });
  const stages = [];
  let finished = false;
  const operation = fetchStartupResources(playlist, budget, new AbortController().signal, (stage) => stages.push(stage))
    .then((value) => { finished = true; return value; });
  await tick();
  assert.equal(requests.length, 2);
  held[1].enqueue(Uint8Array.of(3, 4, 5)); held[1].close();
  await tick();
  assert.equal(finished, false);
  assert.deepEqual(stages, ["mse_first_fragment_fetched"]);
  held[0].enqueue(Uint8Array.of(1, 2)); held[0].close();
  assert.deepEqual(await operation, [Uint8Array.of(1, 2), Uint8Array.of(3, 4, 5)]);
  assert.equal(requests.length, 2);
  assert.deepEqual(stages, ["mse_first_fragment_fetched", "mse_init_fetched"]);
});

for (const failed of [0, 1]) {
  test(`failed startup resource ${failed} cancels its actual pending sibling body and permits a healthy retry`, async (t) => {
    let calls = 0;
    let cancelled = 0;
    mockNetwork(t, async () => {
      const index = calls++;
      return index === failed ? new Response("failed", { status: 503 })
        : new Response(new ReadableStream({ cancel() { cancelled++; } }));
    });
    await assert.rejects(fetchStartupResources(playlist, budget, new AbortController().signal), /HTTP 503/);
    await tick();
    assert.equal(calls, 2);
    assert.equal(cancelled, 1);
    globalThis.fetch = async (url) => new Response(new Uint8Array(url === playlist.initUrl ? [1, 2] : [3, 4, 5]));
    assert.deepEqual(await fetchStartupResources(playlist, budget, new AbortController().signal),
      [Uint8Array.of(1, 2), Uint8Array.of(3, 4, 5)]);
  });
}

test("supersession aborts both startup bodies and rejects without returning stale bytes", async (t) => {
  const signals = [];
  let cancelled = 0;
  mockNetwork(t, async (_url, { signal }) => {
    signals.push(signal);
    return new Response(new ReadableStream({ cancel() { cancelled++; } }));
  });
  const abort = new AbortController();
  const operation = fetchStartupResources(playlist, budget, abort.signal);
  await tick();
  abort.abort();
  await assert.rejects(operation, { name: "AbortError" });
  await tick();
  assert.equal(signals.length, 2);
  assert.ok(signals.every((signal) => signal.aborted));
  assert.equal(cancelled, 2);
});

test("partial, overlong and range-mismatched responses cannot masquerade as complete resources", async (t) => {
  const abort = new AbortController();
  mockNetwork(t, async () => new Response());
  for (const [bytes, init] of [[1, {}], [3, {}], [2, { status: 206 }],
    [2, { headers: { "content-range": "bytes 1-2/4" } }], [2, { headers: { "content-length": "3" } }]]) {
    globalThis.fetch = async () => new Response(new Uint8Array(bytes), init);
    await assert.rejects(fetchResource(playlist.initUrl, abort.signal, { expectedBytes: 2 }), /requested range/);
  }
  globalThis.fetch = async () => new Response(Uint8Array.of(7, 8));
  assert.deepEqual(await fetchResource(playlist.initUrl, abort.signal, { expectedBytes: 2 }), Uint8Array.of(7, 8));
});

test("an invalid aggregate reservation fails before either startup request", async (t) => {
  let calls = 0;
  mockNetwork(t, async () => { calls++; return new Response(Uint8Array.of(1)); });
  for (const invalid of [{ ...budget, totalBytes: 4 },
    { initBytes: 1, mediaBytes: MEDIA_SOURCE_RESOURCE_MAX_BYTES, totalBytes: MEDIA_SOURCE_RESOURCE_MAX_BYTES + 1 }]) {
    await assert.rejects(fetchStartupResources(playlist, invalid, new AbortController().signal), { code: "resource_limit" });
  }
  assert.equal(calls, 0);
});
