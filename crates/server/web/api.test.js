import test from "node:test";
import assert from "node:assert/strict";
import { WebApi } from "./api.js";

const navigation = { view: "library", kind: "video", query: "", sort: "title" };
const response = (body) => new Response(JSON.stringify(body), {
  headers: { "Content-Type": "application/json" },
});
const page = (offset, total, limit = 200) => ({
  schema_version: 2,
  generation: 17,
  offset,
  limit,
  total,
  has_more: offset + limit < total,
  entries: Array.from({ length: Math.min(limit, total - offset) }, (_, index) => ({
    id: String(offset + index + 1), entry_type: "media",
  })),
});

test("a 10,000-item snapshot stays ordered with at most four requests in flight", async (t) => {
  let active = 0;
  let maximum = 0;
  const offsets = [];
  t.mock.method(globalThis, "fetch", async (input, { signal }) => {
    const params = new URL(input, "http://localhost").searchParams;
    const offset = Number(params.get("offset"));
    offsets.push(offset);
    assert.equal(params.get("limit"), "200");
    assert.equal(params.get("generation"), offset === 0 ? null : "17");
    assert.equal(signal.aborted, false);
    maximum = Math.max(maximum, ++active);
    // Later pages can finish before earlier pages.
    await new Promise((resolve) => setTimeout(resolve, 4 - (offset / 200) % 4));
    active -= 1;
    return response(page(offset, 10_000));
  });
  const snapshot = await new WebApi().librarySnapshot(navigation);
  assert.equal(maximum, 4);
  assert.equal(offsets.length, 50);
  assert.equal(new Set(offsets).size, 50);
  assert.deepEqual(snapshot.entries.map((entry) => entry.id),
    Array.from({ length: 10_000 }, (_, index) => String(index + 1)));
  assert.equal(snapshot.has_more, false);
  assert.equal(snapshot.total, 10_000);
  assert.equal(snapshot.generation, 17);
});

test("empty and short final pages complete without an extra request", async (t) => {
  for (const total of [0, 1, 200, 201]) {
    await t.test(String(total), async (t) => {
      const offsets = [];
      t.mock.method(globalThis, "fetch", async (input) => {
        const offset = Number(new URL(input, "http://localhost").searchParams.get("offset"));
        offsets.push(offset);
        return response(page(offset, total));
      });
      const snapshot = await new WebApi().librarySnapshot(navigation);
      assert.equal(snapshot.entries.length, total);
      assert.equal(snapshot.has_more, false);
      assert.deepEqual(offsets, total > 200 ? [0, 200] : [0]);
    });
  }
});

test("incomplete pages cannot silently publish a partial snapshot", async (t) => {
  for (const patch of [
    { entries: [] }, { offset: 1 }, { limit: 0 }, { limit: 201 },
    { total: -1 }, { total: 1.5 }, { has_more: false },
  ]) {
    await t.test(JSON.stringify(patch), async (t) => {
      t.mock.method(globalThis, "fetch", async () => response({ ...page(0, 201), ...patch }));
      await assert.rejects(new WebApi().librarySnapshot(navigation), { code: "invalid_page" });
    });
  }
});

test("a changed catalog aborts the remaining snapshot requests", async (t) => {
  const signals = [];
  let releaseChange;
  const changed = new Promise((resolve) => { releaseChange = resolve; });
  t.mock.method(globalThis, "fetch", async (input, { signal }) => {
    const offset = Number(new URL(input, "http://localhost").searchParams.get("offset"));
    if (offset === 0) return response(page(0, 1000));
    signals.push(signal);
    if (offset === 200) {
      await changed;
      return response({ ...page(offset, 1000), generation: 18 });
    }
    if (signals.length === 4) releaseChange();
    return new Promise((_, reject) => signal.addEventListener("abort", () => reject(signal.reason), { once: true }));
  });
  await assert.rejects(new WebApi().librarySnapshot(navigation), { code: "catalog_changed" });
  assert.equal(signals.length, 4);
  assert.ok(signals.every((signal) => signal.aborted));
});

test("replacing a snapshot rejects late pages even when fetch ignores cancellation", async (t) => {
  let releaseOld;
  let tailStarted;
  const tail = new Promise((resolve) => { tailStarted = resolve; });
  const held = new Promise((resolve) => { releaseOld = resolve; });
  t.mock.method(globalThis, "fetch", async (input) => {
    const params = new URL(input, "http://localhost").searchParams;
    if (params.get("kind") === "audio") return response(page(0, 1));
    const offset = Number(params.get("offset"));
    if (offset > 0) {
      tailStarted();
      await held;
    }
    return response(page(offset, 201));
  });
  const api = new WebApi();
  const old = api.librarySnapshot(navigation);
  const rejected = assert.rejects(old, { name: "AbortError" });
  await tail;
  const current = await api.librarySnapshot({ ...navigation, kind: "audio" });
  releaseOld();
  await rejected;
  assert.equal(current.entries.length, 1);
});
