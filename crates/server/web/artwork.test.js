import test from "node:test";
import assert from "node:assert/strict";
import { ARTWORK_MAX_BYTES, fetchArtwork } from "./artwork.js";

globalThis.location = new URL("http://localhost/");

test("artwork bytes and type survive chunked transport", async (t) => {
  t.mock.method(globalThis, "fetch", async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new Uint8Array([0xff, 0xd8])); controller.enqueue(new Uint8Array([0xff, 0xd9])); controller.close(); },
  }), { headers: { "Content-Type": "image/jpeg" } }));
  const blob = await fetchArtwork("/art.jpg", new AbortController().signal);
  assert.equal(blob.type, "image/jpeg");
  assert.deepEqual(new Uint8Array(await blob.arrayBuffer()), new Uint8Array([0xff, 0xd8, 0xff, 0xd9]));
});

test("oversized artwork closes both declared and unadvertised bodies", async (t) => {
  for (const declared of [true, false]) await t.test(String(declared), async (t) => {
    let cancelled = false;
    let pulls = 0;
    t.mock.method(globalThis, "fetch", async () => new Response(new ReadableStream({
      pull(controller) { pulls++; controller.enqueue(new Uint8Array(1024 * 1024)); },
      cancel() { cancelled = true; },
    }), { headers: declared ? { "Content-Length": String(ARTWORK_MAX_BYTES + 1) } : {} }));
    await assert.rejects(fetchArtwork("/art.jpg", new AbortController().signal), /too large/);
    assert.equal(cancelled, true);
    assert.ok(pulls <= 18, `bounded chunk reads: ${pulls}`);
  });
});

test("the server's full 16 MiB sidecar limit remains accepted", async (t) => {
  const bytes = new Uint8Array(16 * 1024 * 1024);
  bytes[0] = 0xff; bytes[bytes.length - 1] = 0xd9;
  t.mock.method(globalThis, "fetch", async () => new Response(bytes, { headers: { "Content-Length": String(bytes.length) } }));
  const blob = await fetchArtwork("/large.jpg", new AbortController().signal);
  assert.equal(blob.size, 16 * 1024 * 1024);
  const actual = new Uint8Array(await blob.arrayBuffer());
  assert.equal(actual[0], 0xff);
  assert.equal(actual.at(-1), 0xd9);
});

test("empty, failed, and foreign artwork never becomes a displayable blob", async (t) => {
  for (const [source, status, body] of [["/art.jpg", 200, ""], ["/art.jpg", 404, "missing"], ["https://foreign.invalid/art.jpg", 200, "bytes"]]) {
    let fetched = false;
    t.mock.method(globalThis, "fetch", async () => { fetched = true; return new Response(body, { status }); });
    await assert.rejects(fetchArtwork(source, new AbortController().signal));
    assert.equal(fetched, source.startsWith("/"));
  }
});
