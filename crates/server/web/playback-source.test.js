import test from "node:test";
import assert from "node:assert/strict";
import { PlaybackSource } from "./playback-source.js";

test("cancelling a source removes listeners and prevents even queued timer callbacks", (t) => {
  const timers = new Map();
  const removed = [];
  const previousWindow = globalThis.window;
  globalThis.window = {
    setTimeout(callback) { const id = timers.size + 1; timers.set(id, callback); return id; },
    clearTimeout(id) { removed.push(id); },
  };
  t.after(() => { globalThis.window = previousWindow; });
  const player = new EventTarget();
  const source = new PlaybackSource({ player });
  let updates = 0;
  source.listen("timeupdate", () => { updates += 1; });
  source.setTimer("poll", () => { updates += 10; }, 10);
  source.setTimer("startup", () => { updates += 100; }, 10);
  player.dispatchEvent(new Event("timeupdate"));
  source.cancel();
  player.dispatchEvent(new Event("timeupdate"));
  for (const callback of timers.values()) callback();
  source.setTimer("poll", () => { updates += 1000; }, 10);
  assert.equal(updates, 1);
  assert.equal(source.signal.aborted, true);
  assert.deepEqual(removed, [1, 2]);
  assert.equal(timers.size, 2);
});

test("overlapping source failures share recovery and cancellation rejects late work", async () => {
  const source = new PlaybackSource({ player: new EventTarget() });
  let release;
  let recoveries = 0;
  const recovery = () => {
    recoveries += 1;
    return new Promise((resolve) => { release = resolve; });
  };
  const first = source.recover(recovery);
  assert.equal(source.recover(recovery), first);
  await Promise.resolve();
  assert.equal(recoveries, 1);
  release();
  await first;
  const obsolete = source.recover(recovery);
  source.cancel();
  await obsolete;
  await source.recover(recovery);
  assert.equal(recoveries, 1);
});
