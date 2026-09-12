import test from "node:test";
import assert from "node:assert/strict";
import { PlaybackSource } from "./playback-source.js";
import { healthyCompatibleRecovery, initialCompatibleRecovery, nextCompatibleRetry } from "./core.js";

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

const healthyTestCleanup = new WeakMap();

function healthySource(t, { kind = "video", frameCallbacks = true, frameCounter = true, initiallyPaused = false } = {}) {
  let now = 0;
  let frames = 0;
  let callbackId = 0;
  const callbacks = new Map();
  const timers = new Map();
  if (!healthyTestCleanup.has(t)) {
    const previousWindow = globalThis.window;
    const sources = [];
    healthyTestCleanup.set(t, sources);
    t.after(() => {
      for (const source of sources) source.cancel();
      globalThis.window = previousWindow;
    });
  }
  globalThis.window = {
    setTimeout(callback) { const id = ++callbackId; timers.set(id, callback); return id; },
    clearTimeout(id) { timers.delete(id); },
  };
  const player = Object.assign(new EventTarget(), {
    paused: initiallyPaused, ended: false, seeking: false, readyState: 4, error: null,
    currentTime: 0, playbackRate: 1,
  });
  if (frameCallbacks) {
    player.requestVideoFrameCallback = (callback) => { const id = ++callbackId; callbacks.set(id, callback); return id; };
    player.cancelVideoFrameCallback = (id) => { callbacks.delete(id); };
  }
  if (frameCounter) player.getVideoPlaybackQuality = () => ({ totalVideoFrames: frames, droppedVideoFrames: 0 });
  const visibility = Object.assign(new EventTarget(), { visibilityState: "visible" });
  const source = new PlaybackSource({ player, item: { kind } });
  healthyTestCleanup.get(t).push(source);
  let renewals = 0;
  let playing = !initiallyPaused;
  source.watchHealthyProgress({ visibility, now: () => now,
    isPlaying: () => playing, onHealthy: () => { renewals += 1; } });
  const sample = (elapsed = 1_000, mediaSeconds = elapsed / 1_000 * player.playbackRate, newFrames = 30) => {
    now += elapsed;
    player.currentTime += mediaSeconds;
    frames += newFrames;
    if (frameCallbacks && kind === "video") {
      const entries = [...callbacks];
      callbacks.clear();
      for (const [, callback] of entries) callback(now, { mediaTime: player.currentTime });
    } else {
      const entries = [...timers];
      timers.clear();
      for (const [, callback] of entries) callback();
    }
  };
  const run = (seconds) => { for (let second = 0; second < seconds; second += 1) sample(); };
  return { source, player, visibility, sample, run, callbacks, timers,
    renewals: () => renewals, requests: () => callbackId, setPlaying: (value) => { playing = value; } };
}

test("only continuous decoded frames renew the allowance, with exact interval boundaries", (t) => {
  const fixture = healthySource(t);
  fixture.player.dispatchEvent(new Event("playing"));
  fixture.player.dispatchEvent(new Event("canplay"));
  fixture.sample(0);
  fixture.run(29);
  fixture.sample(999);
  assert.equal(fixture.renewals(), 0);
  // Observations are coalesced to at most four per second, so the boundary
  // is evaluated by the next newly decoded sample rather than a bare timer.
  fixture.sample(251);
  assert.equal(fixture.renewals(), 1);
  fixture.run(29);
  assert.equal(fixture.renewals(), 1);
});

for (const event of ["pause", "waiting", "seeking", "ratechange", "visibilitychange"]) {
  test(`${event} starts a new decoded health interval without renewing retries`, (t) => {
    const fixture = healthySource(t);
    fixture.sample(0);
    fixture.run(29);
    (event === "visibilitychange" ? fixture.visibility : fixture.player).dispatchEvent(new Event(event));
    fixture.player.dispatchEvent(new Event("playing"));
    fixture.run(30);
    assert.equal(fixture.renewals(), 0);
    fixture.sample();
    assert.equal(fixture.renewals(), 1);
  });
}

test("hidden playback, paused intent, sparse callbacks and discontinuous offsets cannot accrue health", (t) => {
  const fixture = healthySource(t);
  fixture.visibility.visibilityState = "hidden";
  fixture.run(35);
  fixture.visibility.visibilityState = "visible";
  fixture.setPlaying(false);
  fixture.run(35);
  fixture.setPlaying(true);
  fixture.player.dispatchEvent(new Event("playing"));
  fixture.run(29);
  fixture.sample(3_000);
  fixture.run(29);
  fixture.sample(1_000, 100);
  fixture.run(29);
  assert.equal(fixture.renewals(), 0);
  fixture.sample();
  assert.equal(fixture.renewals(), 1);
});

test("a paused seek retains its only presentation callback without a health observer", (t) => {
  const fixture = healthySource(t, { initiallyPaused: true });
  assert.equal(fixture.callbacks.size, 0);
  let presented = 0;
  fixture.player.requestVideoFrameCallback(() => { presented += 1; });
  fixture.player.seeking = true;
  fixture.player.dispatchEvent(new Event("seeking"));
  fixture.player.seeking = false;
  fixture.player.dispatchEvent(new Event("seeked"));
  fixture.player.dispatchEvent(new Event("canplay"));
  assert.equal(fixture.requests(), 1);
  fixture.sample();
  assert.equal(presented, 1);
  assert.equal(fixture.callbacks.size, 0);
  assert.equal(fixture.renewals(), 0);
  fixture.player.paused = false;
  fixture.setPlaying(true);
  fixture.player.dispatchEvent(new Event("playing"));
  assert.equal(fixture.callbacks.size, 1);
  fixture.run(31);
  assert.equal(fixture.renewals(), 1);
});

for (const event of ["pause", "waiting", "seeking", "visibilitychange"]) {
  test(`${event} cancels frame observation and stale callbacks cannot re-arm it`, (t) => {
    const fixture = healthySource(t);
    fixture.sample(0);
    fixture.run(29);
    const stale = [...fixture.callbacks.values()];
    if (event === "pause") fixture.player.paused = true;
    if (event === "seeking") fixture.player.seeking = true;
    if (event === "visibilitychange") fixture.visibility.visibilityState = "hidden";
    fixture.setPlaying(false);
    (event === "visibilitychange" ? fixture.visibility : fixture.player).dispatchEvent(new Event(event));
    assert.equal(fixture.callbacks.size, 0);
    for (const callback of stale) callback(30_000, { mediaTime: 30 });
    assert.equal(fixture.callbacks.size, 0);
    assert.equal(fixture.renewals(), 0);
    fixture.player.paused = false;
    fixture.player.seeking = false;
    fixture.visibility.visibilityState = "visible";
    fixture.setPlaying(true);
    (event === "visibilitychange" ? fixture.visibility : fixture.player)
      .dispatchEvent(new Event(event === "visibilitychange" ? event : event === "seeking" ? "seeked" : "playing"));
    const resumed = [...fixture.callbacks];
    assert.equal(resumed.length, 1);
    for (const callback of stale) callback(31_000, { mediaTime: 31 });
    assert.deepEqual([...fixture.callbacks], resumed);
    fixture.run(30);
    assert.equal(fixture.renewals(), 0);
    fixture.sample();
    assert.equal(fixture.renewals(), 1);
  });
}

test("paused audio cancels the health timer until playing resumes", (t) => {
  const fixture = healthySource(t, { kind: "audio", frameCallbacks: false });
  fixture.run(29);
  const queued = [...fixture.timers.values()];
  fixture.player.paused = true;
  fixture.player.dispatchEvent(new Event("pause"));
  assert.equal(fixture.source.hasTimer("healthy-progress"), false);
  assert.equal(fixture.timers.size, 0);
  for (const callback of queued) callback();
  fixture.player.dispatchEvent(new Event("canplay"));
  assert.equal(fixture.source.hasTimer("healthy-progress"), false);
  assert.equal(fixture.renewals(), 0);
  fixture.player.paused = false;
  fixture.player.dispatchEvent(new Event("playing"));
  fixture.run(30);
  assert.equal(fixture.renewals(), 1);
});

test("a source failure discards prior health and in-flight recovery cannot renew it", async (t) => {
  const fixture = healthySource(t);
  fixture.sample(0);
  fixture.run(29);
  let release;
  const recovery = fixture.source.recover(() => new Promise((resolve) => { release = resolve; }));
  await Promise.resolve();
  fixture.run(35);
  assert.equal(fixture.renewals(), 0);
  release();
  await recovery;
  fixture.run(30);
  assert.equal(fixture.renewals(), 0);
  fixture.sample();
  assert.equal(fixture.renewals(), 1);
});

test("cancelled frame callbacks cannot renew a replacement session's exhausted allowance", (t) => {
  const fixture = healthySource(t);
  fixture.sample(0);
  fixture.run(29);
  const staleCallbacks = [...fixture.callbacks.values()];
  fixture.source.cancel();
  const replacement = healthySource(t);
  replacement.sample(0);
  replacement.run(29);
  for (const callback of staleCallbacks) callback(30_000, { mediaTime: 30 });
  assert.equal(fixture.callbacks.size, 0);
  fixture.player.dispatchEvent(new Event("playing"));
  fixture.visibility.dispatchEvent(new Event("visibilitychange"));
  fixture.run(35);
  assert.equal(fixture.renewals(), 0);
  assert.equal(replacement.renewals(), 0);
  replacement.sample();
  assert.equal(replacement.renewals(), 1);
});

for (const kind of ["video", "audio"]) {
  test(`${kind} health fallback requires sustained decoded progress`, (t) => {
    const fixture = healthySource(t, { kind, frameCallbacks: false });
    fixture.run(30);
    assert.equal(fixture.renewals(), kind === "audio" ? 1 : 0);
    if (kind === "video") {
      fixture.sample();
      assert.equal(fixture.renewals(), 1);
      fixture.run(29);
      fixture.sample(1_000, 1, 0);
      fixture.run(29);
      assert.equal(fixture.renewals(), 1, "a clock without a new decoded frame does not renew");
    }
  });
}

test("video without presented or decoded frame evidence retains finite recovery", (t) => {
  const fixture = healthySource(t, { frameCallbacks: false, frameCounter: false });
  fixture.run(90);
  assert.equal(fixture.renewals(), 0);
});

test("brief decoded recoveries exhaust retries while separated healthy outages renew them", (t) => {
  let recovery = initialCompatibleRecovery();
  for (let sessionId = 1; sessionId <= 4; sessionId += 1) {
    const fixture = healthySource(t);
    fixture.sample(0);
    fixture.run(sessionId === 3 ? 30 : 5);
    if (fixture.renewals()) recovery = healthyCompatibleRecovery(recovery);
    fixture.source.cancel();
    recovery = nextCompatibleRetry(recovery, { sessionId, now: sessionId * 10_000 });
    assert.notEqual(recovery, null);
    recovery = { ...recovery, pendingSession: null };
  }
  assert.equal(recovery.retries, 2);
  recovery = nextCompatibleRetry(recovery, { sessionId: 5, now: 50_000 });
  assert.equal(nextCompatibleRetry({ ...recovery, pendingSession: null }, { sessionId: 6, now: 60_000 }), null);
});
