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
    const source = { signal, tracks: [], cleanup: null };
    const current = () => this.#source === source && !signal.aborted;
    source.cleanup = () => {
      signal.removeEventListener("abort", source.cleanup);
      for (const { node, loaded } of source.tracks) {
        node.removeEventListener("load", loaded);
        node.track.mode = "disabled";
        node.remove();
      }
      if (this.#source === source) this.#source = null;
    };
    this.#source = source;
    signal.addEventListener("abort", source.cleanup, { once: true });

    for (const caption of captions) {
      if (!caption.browser_supported || !caption.url) continue;
      const node = document.createElement("track");
      node.kind = "subtitles";
      node.label = caption.label;
      node.srclang = caption.language || "und";
      node.src = caption.url;
      node.dataset.captionIndex = String(caption.index);
      const entry = { node, ready: false, loaded: null };
      entry.loaded = () => {
        if (!current() || entry.ready) return;
        // Mutate the browser's parsed cues, preserving styling, positioning,
        // and identifiers. Each source has fresh nodes, so offsets never add.
        for (const cue of [...(node.track.cues || [])]) {
          const window = captionCueWindow(cue.startTime, cue.endTime, segmentOffset);
          if (!window) node.track.removeCue(cue);
          else {
            cue.startTime = window.start;
            cue.endTime = window.end;
          }
        }
        entry.ready = true;
        this.#applySelection();
      };
      source.tracks.push(entry);
      node.addEventListener("load", entry.loaded);
      this.#dom.video.append(node);
    }
    this.#applySelection();
  }

  clear() {
    this.#source?.cleanup();
  }

  #select(value) {
    const sessionId = this.#store.getState().playback.sessionId;
    this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { selectedCaption: value } });
    this.#applySelection();
  }

  #applySelection() {
    if (!this.#source || this.#source.signal.aborted) return;
    const value = this.#store.getState().playback.selectedCaption;
    for (const track of this.#dom.video.textTracks || []) track.mode = "disabled";
    for (const { node, ready } of this.#source.tracks) {
      const selected = value !== "off" && node.dataset.captionIndex === String(value);
      // Hidden triggers loading without displaying cues on the wrong timeline.
      // The load callback rebases them before making the selected track visible.
      node.track.mode = selected ? (ready ? "showing" : "hidden") : "disabled";
    }
  }

  render() {
    const { playback } = this.#store.getState();
    const captions = playback.item?.captions || [];
    const key = `${playback.item?.id}:${playback.selectedCaption}:${captions.map((caption) => `${caption.index}-${caption.browser_supported}`).join("|")}`;
    this.#dom.captionsButton.disabled = playback.item?.kind !== "video" || captions.length === 0;
    this.#dom.captionsButton.setAttribute("aria-pressed", String(playback.selectedCaption !== "off"));
    if (key === this.#renderKey) return;
    this.#renderKey = key;
    this.#dom.captionChoices.replaceChildren();
    const choices = [{ index: "off", label: "Off", browser_supported: true }, ...captions];
    for (const caption of choices) {
      const label = document.createElement("label");
      const radio = document.createElement("input");
      radio.type = "radio";
      radio.name = "caption-choice";
      radio.value = String(caption.index);
      radio.checked = String(caption.index) === String(playback.selectedCaption);
      radio.disabled = !caption.browser_supported;
      radio.addEventListener("change", () => this.#select(radio.value));
      label.append(radio, document.createTextNode(caption.browser_supported ? caption.label : `${caption.label} (${caption.source_format?.toUpperCase()} is not supported in browsers)`));
      this.#dom.captionChoices.append(label);
    }
  }
}
