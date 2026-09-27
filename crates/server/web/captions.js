import { captionCueWindow, captionWindowStarts, parseWebVttCues } from "./core.js";

const CAPTION_WINDOW_RETRIES = 3;
let emptyCaptionsUrl = "";

// Streaming tracks start empty and receive cues window by window, so the
// selected caption shows without extracting the complete track first.
function emptyCaptions() {
  emptyCaptionsUrl ||= URL.createObjectURL(new Blob(["WEBVTT\n\n"], { type: "text/vtt" }));
  return emptyCaptionsUrl;
}

function applyCueSettings(cue, settings) {
  for (const setting of settings.split(" ")) {
    const [name, value = ""] = setting.split(":");
    const [amount, alignment] = value.split(",");
    const percent = amount.endsWith("%") ? Number.parseFloat(amount) : null;
    try {
      if (name === "vertical") cue.vertical = value;
      else if (name === "align") cue.align = value;
      else if (name === "size" && percent !== null) cue.size = percent;
      else if (name === "position" && percent !== null) {
        cue.position = percent;
        if (alignment) cue.positionAlign = alignment;
      } else if (name === "line" && amount) {
        cue.snapToLines = percent === null;
        cue.line = percent ?? Number.parseInt(amount, 10);
        if (alignment) cue.lineAlign = alignment;
      }
    } catch {
      // An unsupported setting keeps the browser default for that cue.
    }
  }
}

function abortableDelay(milliseconds, signal) {
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", done);
      resolve();
    };
    const timer = setTimeout(done, milliseconds);
    signal.addEventListener("abort", done, { once: true });
  });
}

// Own the caption DOM and its source lifetime. The player supplies the local
// timeline origin; this component never negotiates or restarts media sources.
export class CaptionController {
  #store;
  #dom;
  #source = null;
  #renderKey = "";

  constructor({ store, dom }) {
    this.#store = store;
    this.#dom = dom;
    dom.captionsButton.addEventListener("click", () => {
      const open = dom.captionMenu.hidden;
      dom.captionMenu.hidden = !open;
      dom.captionsButton.setAttribute("aria-expanded", String(open));
      if (open) dom.captionChoices.querySelector("input:checked")?.focus();
    });
    dom.captionRetry.addEventListener("click", () => this.#retry());
    dom.captionOff.addEventListener("click", () => {
      this.#select("off");
      this.#focusChoice("off");
    });
    document.addEventListener("pointerdown", (event) => {
      if (!dom.captionMenu.hidden
        && !dom.captionMenu.contains(event.target)
        && !dom.captionsButton.contains(event.target)) this.closeMenu();
    });
  }

  closeMenu({ restoreFocus = false } = {}) {
    this.#dom.captionMenu.hidden = true;
    this.#dom.captionsButton.setAttribute("aria-expanded", "false");
    if (restoreFocus) this.#dom.captionsButton.focus();
  }

  attach(captions, { segmentOffset, signal, globalTime = null }) {
    this.clear();
    if (signal.aborted) return;
    const source = { signal, segmentOffset, globalTime, tracks: [], cleanup: null };
    source.cleanup = () => {
      signal.removeEventListener("abort", source.cleanup);
      for (const entry of source.tracks) this.#removeTrack(entry);
      if (this.#source === source) {
        this.#source = null;
        this.#renderError();
      }
    };
    this.#source = source;
    signal.addEventListener("abort", source.cleanup, { once: true });

    for (const caption of captions) {
      if (!caption.browser_supported || !caption.url) continue;
      const entry = { caption, node: null, ready: false, loading: false, failed: false, cleanup: null, windows: null };
      source.tracks.push(entry);
      this.#createTrack(source, entry);
    }
    this.#applySelection();
  }

  clear() {
    this.#source?.cleanup();
  }

  #removeTrack(entry) {
    entry.cleanup?.();
    entry.cleanup = null;
    entry.node = null;
    entry.ready = false;
    entry.loading = false;
  }

  #createTrack(source, entry) {
    const { caption } = entry;
    const node = document.createElement("track");
    node.kind = "subtitles";
    node.label = caption.label;
    node.srclang = caption.language || "und";
    node.src = caption.streaming_url ? emptyCaptions() : caption.url;
    node.dataset.captionIndex = String(caption.index);
    entry.node = node;
    const current = () => this.#source === source && !source.signal.aborted && entry.node === node;
    const loaded = () => {
      if (!current() || entry.ready || entry.failed) return;
      // Mutate the browser's parsed cues, preserving styling, positioning,
      // and identifiers. Each attempt has fresh nodes, so offsets never add.
      for (const cue of [...(node.track.cues || [])]) {
        const window = captionCueWindow(cue.startTime, cue.endTime, source.segmentOffset);
        if (!window) node.track.removeCue(cue);
        else {
          cue.startTime = window.start;
          cue.endTime = window.end;
        }
      }
      entry.ready = true;
      entry.loading = false;
      this.#applySelection();
      if (caption.streaming_url) this.#startWindows(source, entry);
    };
    const failed = () => {
      if (!current() || !this.#isSelected(entry) || entry.ready || entry.failed) return;
      this.#fail(entry);
    };
    entry.cleanup = () => {
      entry.windows?.controller.abort();
      entry.windows = null;
      node.removeEventListener("load", loaded);
      node.removeEventListener("error", failed);
      node.track.mode = "disabled";
      // Invalidate the native track fetch as well as its queued callbacks.
      node.removeAttribute("src");
      node.remove();
    };
    node.addEventListener("load", loaded);
    node.addEventListener("error", failed);
    this.#dom.video.append(node);
  }

  #fail(entry) {
    entry.failed = true;
    entry.loading = false;
    entry.windows?.controller.abort();
    entry.windows = null;
    entry.node.track.mode = "disabled";
    this.#dom.captionMenu.hidden = false;
    this.#dom.captionsButton.setAttribute("aria-expanded", "true");
    this.#dom.playerStage.classList.add("controls-visible");
    this.#renderError();
  }

  #globalTime(source) {
    const time = source.globalTime?.();
    return Number.isFinite(time) ? time : source.segmentOffset + (this.#dom.video.currentTime || 0);
  }

  #startWindows(source, entry) {
    const controller = new AbortController();
    const windows = { controller, loaded: new Set(), pending: false, seen: new Set() };
    entry.windows = windows;
    const update = () => this.#loadNextWindow(source, entry, windows);
    for (const type of ["timeupdate", "seeking", "loadedmetadata"]) {
      this.#dom.video.addEventListener(type, update, { signal: controller.signal });
    }
    update();
  }

  #loadNextWindow(source, entry, windows) {
    if (windows.pending || windows.controller.signal.aborted || entry.windows !== windows) return;
    const duration = Number(this.#store.getState().playback.item?.duration_seconds) || Infinity;
    const start = captionWindowStarts(this.#globalTime(source), duration).find((start) => !windows.loaded.has(start));
    if (start === undefined) return;
    windows.pending = true;
    this.#fetchWindow(source, entry, windows, start).catch(() => {
      if (entry.windows === windows && this.#isSelected(entry)) this.#fail(entry);
    }).finally(() => {
      windows.pending = false;
      this.#loadNextWindow(source, entry, windows);
    });
  }

  async #fetchWindow(source, entry, windows, start) {
    const { signal } = windows.controller;
    const url = new URL(entry.caption.streaming_url, document.baseURI);
    url.searchParams.set("start", String(start));
    let text = null;
    for (let attempt = 0; text === null; attempt += 1) {
      let status = 0;
      try {
        const response = await fetch(url, { signal, headers: { Accept: "text/vtt" } });
        status = response.status;
        if (response.ok) text = await response.text();
      } catch {
        // Network failures are retried below; aborts end this window.
      }
      if (signal.aborted || entry.windows !== windows) return;
      if (text !== null) break;
      // Busy helpers and timeouts are transient; other statuses are final.
      if (![0, 503, 504].includes(status) || attempt >= CAPTION_WINDOW_RETRIES) {
        if (this.#isSelected(entry)) this.#fail(entry);
        return;
      }
      await abortableDelay(1000 * (attempt + 1), signal);
      if (signal.aborted) return;
    }
    const { track } = entry.node;
    // Windows overlap; cues keep absolute times, so the key is stable.
    for (const cue of parseWebVttCues(text)) {
      const key = `${cue.start}|${cue.end}|${cue.text}`;
      if (windows.seen.has(key)) continue;
      windows.seen.add(key);
      const window = captionCueWindow(cue.start, cue.end, source.segmentOffset);
      if (!window) continue;
      const added = new VTTCue(window.start, window.end, cue.text);
      added.id = cue.id;
      applyCueSettings(added, cue.settings);
      track.addCue(added);
    }
    windows.loaded.add(start);
  }

  #isSelected(entry) {
    const value = this.#store.getState().playback.selectedCaption;
    return value !== "off" && String(entry.caption.index) === String(value);
  }

  #retry() {
    const source = this.#source;
    if (!source || source.signal.aborted) return;
    const entry = source.tracks.find((entry) => this.#isSelected(entry));
    if (!entry?.failed) return;
    this.#removeTrack(entry);
    entry.failed = false;
    this.#createTrack(source, entry);
    this.#applySelection();
  }

  #select(value) {
    const sessionId = this.#store.getState().playback.sessionId;
    this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { selectedCaption: value } });
    this.#applySelection();
  }

  #applySelection() {
    if (!this.#source || this.#source.signal.aborted) return;
    for (const track of this.#dom.video.textTracks || []) track.mode = "disabled";
    for (const entry of this.#source.tracks) {
      const selected = this.#isSelected(entry);
      if (!selected && (entry.loading || entry.failed || entry.windows)) {
        this.#removeTrack(entry);
        entry.failed = false;
      }
      if (selected && !entry.node) this.#createTrack(this.#source, entry);
      // Hidden triggers loading without displaying cues on the wrong timeline.
      // The load callback rebases them before making the selected track visible.
      if (entry.node) {
        entry.loading = selected && !entry.ready && !entry.failed;
        entry.node.track.mode = selected && !entry.failed ? (entry.ready ? "showing" : "hidden") : "disabled";
      }
    }
    this.#renderError();
  }

  #focusChoice(value) {
    const radios = [...this.#dom.captionChoices.querySelectorAll("input")];
    const target = radios.find((radio) => !radio.disabled && radio.value === String(value))
      || radios.find((radio) => !radio.disabled && radio.checked)
      || radios.find((radio) => !radio.disabled);
    target?.focus();
  }

  #renderError() {
    const failed = Boolean(this.#source?.tracks.some((entry) => this.#isSelected(entry) && entry.failed));
    if (!failed && this.#dom.captionError.contains(document.activeElement)) {
      this.#focusChoice(this.#store.getState().playback.selectedCaption);
    }
    this.#dom.captionError.hidden = !failed;
    const message = failed ? "Captions could not load. Try again or turn captions off." : "";
    if (this.#dom.captionErrorMessage.textContent !== message) this.#dom.captionErrorMessage.textContent = message;
  }

  render() {
    let { playback } = this.#store.getState();
    const captions = playback.item?.captions || [];
    if (playback.selectedCaption !== "off"
      && !captions.some((caption) => caption.browser_supported && String(caption.index) === String(playback.selectedCaption))) {
      // Metadata can remove or disable a selected choice. Dispatch only while
      // invalid, so the synchronous store subscriber can safely render again.
      this.#select("off");
      playback = this.#store.getState().playback;
    }
    const key = JSON.stringify([playback.item?.id, captions.map((caption) => [caption.index, caption.label, caption.browser_supported, caption.source_format])]);
    this.#dom.captionsButton.disabled = playback.item?.kind !== "video" || captions.length === 0;
    this.#dom.captionsButton.setAttribute("aria-pressed", String(playback.selectedCaption !== "off"));
    const focused = this.#dom.captionChoices.contains(document.activeElement) ? document.activeElement.value : null;
    if (key !== this.#renderKey) {
      this.#renderKey = key;
      this.#dom.captionChoices.replaceChildren();
      const choices = [{ index: "off", label: "Off", browser_supported: true }, ...captions];
      for (const caption of choices) {
        const label = document.createElement("label");
        const radio = document.createElement("input");
        radio.type = "radio";
        radio.name = "caption-choice";
        radio.value = String(caption.index);
        radio.disabled = !caption.browser_supported;
        radio.addEventListener("change", () => this.#select(radio.value));
        label.append(radio, document.createTextNode(caption.browser_supported ? caption.label : `${caption.label} (${caption.source_format?.toUpperCase()} is not supported in browsers)`));
        this.#dom.captionChoices.append(label);
      }
    }
    for (const radio of this.#dom.captionChoices.querySelectorAll("input")) {
      radio.checked = radio.value === String(playback.selectedCaption);
    }
    if (focused !== null && !this.#dom.captionChoices.contains(document.activeElement)) this.#focusChoice(focused);
    this.#renderError();
  }
}
