import { decodedProgressWindow, HEALTHY_DECODED_PROGRESS_MS } from "./core.js";

// One source owns its media listeners, polling/startup timers and in-flight
// recovery. Cancelling it makes every queued callback harmless. User seek and
// retry delays belong to the controller because they outlive the old source.
export class PlaybackSource {
  #controller = new AbortController();
  #timers = new Map();
  #reported = new Set();
  #recovery = null;
  #healthyProgress = null;
  #frameCallback = null;
  #healthyWatch = null;

  constructor(context) {
    Object.assign(this, context);
    this.reloads = 0;
  }

  get signal() { return this.#controller.signal; }
  get active() { return !this.signal.aborted; }

  listen(name, handler, target = this.player) {
    target.addEventListener(name, (...args) => {
      if (this.active) handler(...args);
    }, { signal: this.signal });
  }

  hasTimer(name) { return this.#timers.has(name); }

  clearTimer(name) {
    if (this.#timers.has(name)) window.clearTimeout(this.#timers.get(name));
    this.#timers.delete(name);
  }

  setTimer(name, callback, delay) {
    this.clearTimer(name);
    if (!this.active) return;
    this.#timers.set(name, window.setTimeout(() => {
      this.#timers.delete(name);
      if (this.active) callback();
    }, delay));
  }

  reportOnce(event, report) {
    if (!this.active || this.#reported.has(event)) return;
    this.#reported.add(event);
    void report(event).catch(() => { /* Telemetry never changes playback. */ });
  }

  watchHealthyProgress({ isPlaying, onHealthy, visibility = document, now = () => performance.now() }) {
    const player = this.player;
    const presentedFrames = this.item.kind === "video" && typeof player.requestVideoFrameCallback === "function";
    const cancelFrame = player.cancelVideoFrameCallback?.bind(player);
    let lastSampleAt = -Infinity;
    let lastFrames = null;
    let watching = false;
    let epoch = 0;
    const reset = () => { this.#healthyProgress = null; lastSampleAt = -Infinity; lastFrames = null; };
    const eligible = () => this.active && !this.#recovery && isPlaying() && visibility.visibilityState === "visible"
      && !player.paused && !player.ended && !player.seeking && !player.error && player.readyState >= 2;
    const stop = () => {
      watching = false;
      epoch += 1;
      reset();
      if (this.#frameCallback !== null) cancelFrame?.(this.#frameCallback);
      this.#frameCallback = null;
      this.clearTimer("healthy-progress");
    };
    const observe = (mediaTime, frames = null) => {
      const time = now();
      if (time - lastSampleAt < 250) return;
      lastSampleAt = time;
      const hasDecoded = frames === null || (Number.isFinite(frames) && frames > lastFrames);
      this.#healthyProgress = decodedProgressWindow(this.#healthyProgress, {
        now: time, mediaTime, rate: player.playbackRate, eligible: hasDecoded,
      });
      lastFrames = frames;
      if (this.#healthyProgress?.healthyMs >= HEALTHY_DECODED_PROGRESS_MS) {
        // A renewed allowance needs another complete healthy interval before
        // renewal again; counters and progress remain source/session owned.
        reset();
        onHealthy();
      }
    };
    const current = (token) => {
      if (!watching || token !== epoch) return false;
      if (eligible()) return true;
      stop();
      return false;
    };
    const nextFrame = (token) => {
      if (!current(token)) return;
      this.#frameCallback = player.requestVideoFrameCallback((_, frame) => {
        // A canceled observation may already be queued. It cannot clear a
        // newer observer's handle or compete with a paused seek's only frame.
        if (!watching || token !== epoch || !this.active) return;
        this.#frameCallback = null;
        if (!current(token)) return;
        observe(frame.mediaTime);
        nextFrame(token);
      });
    };
    const tick = (token) => {
      if (!current(token)) return;
      if (this.item.kind === "audio") observe(player.currentTime);
      else {
        const quality = player.getVideoPlaybackQuality?.();
        const frames = quality ? quality.totalVideoFrames - quality.droppedVideoFrames
          : player.webkitDecodedFrameCount ?? player.mozPresentedFrames;
        // Without frame evidence, keep the finite allowance. A running
        // video clock alone cannot establish a working video decoder.
        observe(player.currentTime, Number.isFinite(frames) ? frames : NaN);
      }
      this.setTimer("healthy-progress", () => tick(token), 250);
    };
    const resume = () => {
      if (watching || !eligible()) return;
      watching = true;
      if (presentedFrames) nextFrame(epoch);
      else tick(epoch);
    };
    this.#healthyWatch = { stop, resume };
    for (const name of ["waiting", "seeking", "emptied", "ended", "error"]) this.listen(name, stop);
    this.listen("pause", () => {
      stop();
      // Native pause events can arrive after playback has already resumed.
      if (!player.paused) resume();
    });
    for (const name of ["playing", "canplay", "seeked"]) this.listen(name, resume);
    this.listen("ratechange", () => { stop(); resume(); });
    this.listen("visibilitychange", () => { stop(); resume(); }, visibility);
    resume();
  }

  recover(callback) {
    if (!this.active) return Promise.resolve();
    this.#healthyWatch?.stop();
    // Defer invocation so duplicate ended/error/append callbacks all observe
    // the same promise even if recovery immediately starts another source.
    this.#recovery ||= Promise.resolve().then(() => this.active && callback())
      .finally(() => { this.#recovery = null; this.#healthyWatch?.resume(); });
    return this.#recovery;
  }

  cancel() {
    this.#controller.abort();
    this.#healthyWatch?.stop();
    for (const name of this.#timers.keys()) this.clearTimer(name);
  }
}
