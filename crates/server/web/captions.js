import { captionCueWindow } from "./core.js";

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

  attach(captions, { segmentOffset, signal }) {
    this.clear();
    if (signal.aborted) return;
    const source = { signal, segmentOffset, tracks: [], cleanup: null };
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
      const entry = { caption, node: null, ready: false, loading: false, failed: false, cleanup: null };
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
    node.src = caption.url;
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
    };
    const failed = () => {
      if (!current() || !this.#isSelected(entry) || entry.ready || entry.failed) return;
      entry.failed = true;
      entry.loading = false;
      node.track.mode = "disabled";
      this.#dom.captionMenu.hidden = false;
      this.#dom.captionsButton.setAttribute("aria-expanded", "true");
      this.#dom.playerStage.classList.add("controls-visible");
      this.#renderError();
    };
    entry.cleanup = () => {
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
      if (!selected && (entry.loading || entry.failed)) {
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
