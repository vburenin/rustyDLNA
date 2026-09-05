// One source owns its media listeners, polling/startup timers and in-flight
// recovery. Cancelling it makes every queued callback harmless. User seek and
// retry delays belong to the controller because they outlive the old source.
export class PlaybackSource {
  #controller = new AbortController();
  #timers = new Map();
  #reported = new Set();
  #recovery = null;

  constructor(context) {
    Object.assign(this, context);
    this.reloads = 0;
  }

  get signal() { return this.#controller.signal; }
  get active() { return !this.signal.aborted; }

  listen(name, handler) {
    this.player.addEventListener(name, (...args) => {
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

  recover(callback) {
    if (!this.active) return Promise.resolve();
    // Defer invocation so duplicate ended/error/append callbacks all observe
    // the same promise even if recovery immediately starts another source.
    this.#recovery ||= Promise.resolve().then(() => this.active && callback())
      .finally(() => { this.#recovery = null; });
    return this.#recovery;
  }

  cancel() {
    this.#controller.abort();
    for (const name of this.#timers.keys()) this.clearTimer(name);
  }
}
