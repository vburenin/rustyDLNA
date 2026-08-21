import {
  apiErrorCategory,
  audioTrackLabel,
  chooseSource,
  clockLabel,
  compatibleSegmentStart,
  itemDuration,
  mediaDetails,
  negotiateCompatibleStreams,
  playbackError,
  queueNeighbor,
  resumePosition,
  seekTarget,
  sourceMime,
  timelineValueText,
} from "./core.js";
import {
  clearProgress,
  createProgressWriter,
  progressFor,
  savePreference,
  saveProgress,
} from "./preferences.js";

const CONTROLS_IDLE_MS = 3000;

function codecLabel(value) {
  const codec = String(value || "unknown").split(",")[0].trim().toLowerCase();
  return {
    aac: "AAC", ac3: "AC-3", eac3: "E-AC-3", dts: "DTS", flac: "FLAC",
    h264: "H.264", avc: "H.264", hevc: "HEVC", h265: "HEVC", mp3: "MP3",
    opus: "Opus", truehd: "TrueHD", vorbis: "Vorbis",
  }[codec] || codec.toUpperCase();
}

function containerLabel(item) {
  const container = String(item?.container || item?.ext || item?.mime || "unknown").toLowerCase();
  return { matroska: "Matroska", mkv: "Matroska", mp4: "MP4", mov: "QuickTime", webm: "WebM", "mpeg-ts": "MPEG-TS" }[container]
    || container.toUpperCase();
}

function videoLevelLabel(level) {
  const numeric = Number(level);
  if (!(numeric > 0)) return "";
  return numeric < 100 ? `Level ${Math.floor(numeric / 10)}.${numeric % 10}` : `Level ${numeric}`;
}

function replaceFacts(list, facts) {
  list.replaceChildren(...facts.filter(([, value]) => value).map(([name, value]) => {
    const row = document.createElement("div");
    const term = document.createElement("dt");
    const description = document.createElement("dd");
    term.textContent = name;
    description.textContent = value;
    row.append(term, description);
    return row;
  }));
}

function capabilityProbeLabel(contentType, probe) {
  if (!contentType) return "No server-approved stream-copy candidate";
  return [
    contentType,
    `canPlayType: ${probe?.canPlayType || "not tested"}`,
    `MediaCapabilities: ${probe?.mediaCapabilities || "not tested"}`,
  ].join(" · ");
}

export class PlaybackController {
  #store;
  #api;
  #dom;
  #session = 0;
  #sourceController = null;
  #seekTimer = null;
  #controlsTimer = null;
  #statusTimer = null;
  #announceTimer = null;
  #wakeLock = null;
  #captionRenderKey = "";
  #audioRenderKey = "";
  #chapterRenderKey = "";
  #capabilityCache = new Map();
  #progressWriter;

  constructor({ store, api, dom }) {
    this.#store = store;
    this.#api = api;
    this.#dom = dom;
    this.#progressWriter = createProgressWriter(() => this.#writeProgress());
    this.#bindControls();
    this.#applyInitialPreferences();
    this.#store.subscribe(() => this.render());
    this.render();
  }

  async select(item, { preserveQueue = false, startAt = 0 } = {}) {
    if (!preserveQueue) {
      this.#store.dispatch({ type: "QUEUE_SUCCESS", entries: [item], generation: null });
    }
    this.#cancelSource();
    const sessionId = ++this.#session;
    this.#dom.resumePrompt.hidden = true;
    this.#store.dispatch({ type: "PLAYBACK_SELECT", sessionId, item, duration: itemDuration(item) });
    this.#bringPlayerIntoView();
    this.#showControls();
    const enriched = await this.#enrichAudioTracks();
    if (sessionId !== this.#store.getState().playback.sessionId) return;
    item = enriched || item;
    this.#updateMediaSessionMetadata(item);
    const duration = itemDuration(item);
    const linkedStart = seekTarget(startAt, duration);
    const resumeAt = linkedStart > 0 ? 0 : resumePosition(progressFor(item.id), duration);
    if (linkedStart > 0) {
      this.#loadSource(item, { start: linkedStart, intent: "playing", messageKind: "deep_link" });
      return;
    }
    if (resumeAt > 0) {
      this.#dom.resumeTime.textContent = `Resume at ${clockLabel(resumeAt)}`;
      this.#dom.resumePrompt.hidden = false;
      this.#dom.resumeButton.onclick = () => {
        this.#dom.resumePrompt.hidden = true;
        this.#loadSource(item, { start: resumeAt, intent: "playing", messageKind: "resume" });
      };
      this.#dom.startOverButton.onclick = () => {
        clearProgress(item.id);
        this.#dom.resumePrompt.hidden = true;
        this.#loadSource(item, { start: 0, intent: "playing" });
      };
    } else {
      this.#loadSource(item, { start: 0, intent: "playing" });
    }
  }

  activePlayer() {
    return this.#store.getState().playback.item?.kind === "audio" ? this.#dom.audio : this.#dom.video;
  }

  globalTime() {
    const playback = this.#store.getState().playback;
    const player = this.activePlayer();
    const local = Number.isFinite(player?.currentTime) ? player.currentTime : 0;
    return playback.sourceMode === "compatible" ? playback.segmentOffset + local : local;
  }

  async togglePlay() {
    const { playback } = this.#store.getState();
    if (!playback.item) return;
    if (playback.status === "ended") {
      if (playback.sourceMode === "compatible") {
        this.#loadSource(playback.item, { start: 0, intent: "playing", forceSourceMode: "compatible" });
        return;
      }
      this.seekTo(0);
      this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId: playback.sessionId, status: "paused", intent: "playing", message: null });
      await this.#attemptPlay(playback.sessionId, this.activePlayer());
      return;
    }
    const player = this.activePlayer();
    if (player.paused) await this.#attemptPlay(playback.sessionId, player);
    else player.pause();
  }

  seekTo(value) {
    const state = this.#store.getState();
    const { playback } = state;
    if (!playback.item || !(playback.duration > 0)) return;
    const target = seekTarget(value, playback.duration);
    if (target >= playback.duration) {
      this.#cancelSeekTimer();
      this.activePlayer().pause();
      this.#store.dispatch({
        type: "PLAYBACK_TIME", sessionId: playback.sessionId,
        currentTime: playback.duration, duration: playback.duration,
      });
      this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId: playback.sessionId, status: "ended", intent: "paused", message: null });
      clearProgress(playback.item.id);
      return;
    }
    if (playback.sourceMode !== "compatible") {
      this.activePlayer().currentTime = target;
      this.#store.dispatch({ type: "PLAYBACK_TIME", sessionId: playback.sessionId, currentTime: target, duration: playback.duration });
      // Persist explicit seeks even while paused; a media engine may not emit a
      // timeupdate before the user chooses another title or closes the page.
      this.#progressWriter.schedule();
      return;
    }
    this.#cancelSeekTimer();
    const intent = this.activePlayer().paused ? "paused" : "playing";
    this.#cancelSource({ keepElement: false });
    this.#store.dispatch({
      type: "PLAYBACK_STATUS", sessionId: playback.sessionId, status: "seeking", intent,
      message: `Starting at ${clockLabel(target)}…`,
    });
    this.#seekTimer = window.setTimeout(() => {
      this.#seekTimer = null;
      this.#loadSource(playback.item, {
        start: target,
        intent,
        forceSourceMode: "compatible",
        message: `Starting at ${clockLabel(target)}…`,
        messageKind: "seek",
      });
    }, 140);
  }

  playRelative(delta) {
    const state = this.#store.getState();
    const next = queueNeighbor(state.queue.entries, state.playback.item?.id, delta);
    if (next) this.select(next, { preserveQueue: true });
  }

  playChapterRelative(delta) {
    const { playback } = this.#store.getState();
    const chapters = playback.chapters || [];
    if (!chapters.length) {
      this.playRelative(delta);
      return;
    }
    const currentIndex = Math.max(0, chapters.findLastIndex((chapter) => chapter.start_seconds <= playback.currentTime));
    if (delta < 0 && playback.currentTime > chapters[currentIndex].start_seconds + 3) {
      this.seekTo(chapters[currentIndex].start_seconds);
      return;
    }
    const chapter = chapters[currentIndex + delta];
    if (chapter) this.seekTo(chapter.start_seconds);
    else this.playRelative(delta);
  }

  async toggleFullscreen() {
    try {
      if (document.fullscreenElement) await document.exitFullscreen();
      else await this.#dom.playerStage.requestFullscreen();
    } catch (error) {
      const sessionId = this.#store.getState().playback.sessionId;
      this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("unknown", error?.message || "Fullscreen request failed") });
    }
  }

  render() {
    const state = this.#store.getState();
    const { playback, preferences, queue, server } = state;
    const item = playback.item;
    this.#dom.playerStage.classList.toggle("has-media", Boolean(item));
    this.#dom.playerStage.classList.toggle("has-video", item?.kind === "video");
    this.#dom.playerStage.classList.toggle("is-playing", playback.status === "playing");
    this.#dom.playerEmpty.hidden = Boolean(item);
    this.#dom.nowPlaying.hidden = !item;
    this.#dom.playbackControls.hidden = !item || !this.#dom.resumePrompt.hidden;
    if (!item) return;

    document.title = `${item.title} · ${server.name}`;
    this.#dom.nowPlayingTitle.textContent = item.title;
    this.#dom.nowPlayingMeta.textContent = [mediaDetails(item), item.file_name !== item.title ? item.file_name : ""].filter(Boolean).join(" · ");
    this.#dom.video.hidden = item.kind !== "video";
    this.#dom.audioStage.hidden = item.kind !== "audio";
    if (item.kind === "audio" && item.art_url) {
      if (this.#dom.audioArt.src !== new URL(item.art_url, window.location.href).href) this.#dom.audioArt.src = item.art_url;
      this.#dom.audioArt.hidden = false;
    } else {
      this.#dom.audioArt.hidden = true;
      this.#dom.audioArt.removeAttribute("src");
    }
    this.#dom.video.classList.toggle("fill", preferences.fill);
    this.#dom.video.dataset.captionSize = preferences.captionSize;
    this.#dom.video.dataset.captionBackground = preferences.captionBackground;
    this.#dom.captionSizeControl.value = preferences.captionSize;
    this.#dom.captionBackgroundControl.value = preferences.captionBackground;
    this.#dom.autoplayControl.checked = preferences.autoplay;

    const busy = ["loading", "waiting", "seeking"].includes(playback.status);
    this.#dom.stageProgress.hidden = !busy;
    this.#dom.stageProgressLabel.textContent = playback.message || {
      loading: playback.sourceMode === "compatible" ? "Preparing media" : "Loading media",
      waiting: "Buffering",
      seeking: "Seeking",
    }[playback.status] || "Loading media";

    const playing = playback.status === "playing";
    this.#dom.playButton.textContent = playing ? "Pause" : playback.status === "ended" ? "Replay" : "Play";
    this.#dom.playButton.setAttribute("aria-label", playing ? "Pause" : playback.status === "ended" ? "Replay" : "Play");
    this.#dom.muteButton.textContent = preferences.muted ? "Unmute" : "Mute";
    this.#dom.muteButton.setAttribute("aria-label", preferences.muted ? "Unmute" : "Mute");
    this.#dom.muteButton.setAttribute("aria-pressed", String(preferences.muted));
    this.#dom.loopButton.setAttribute("aria-pressed", String(preferences.loop));
    this.#dom.loopButton.setAttribute("aria-label", preferences.loop ? "Turn loop off" : "Turn loop on");
    this.#dom.fitButton.disabled = item.kind !== "video";
    this.#dom.fitButton.setAttribute("aria-pressed", String(preferences.fill));
    this.#dom.fitButton.textContent = preferences.fill ? "Fit" : "Fill";
    this.#dom.fitButton.setAttribute("aria-label", preferences.fill ? "Fit entire video in frame" : "Fill video frame");
    this.#dom.pipButton.disabled = item.kind !== "video" || !document.pictureInPictureEnabled;
    this.#dom.pipButton.setAttribute("aria-pressed", String(playback.pip));
    this.#dom.pipButton.setAttribute("aria-label", playback.pip ? "Exit picture in picture" : "Enter picture in picture");
    this.#dom.fullscreenButton.setAttribute("aria-pressed", String(playback.fullscreen));
    this.#dom.fullscreenButton.textContent = playback.fullscreen ? "Exit" : "Full";
    this.#dom.fullscreenButton.setAttribute("aria-label", playback.fullscreen ? "Exit full screen" : "Enter full screen");

    this.#dom.volumeControl.value = String(preferences.volume);
    this.#dom.volumeControl.style.setProperty("--volume-level", `${preferences.muted ? 0 : preferences.volume}%`);
    this.#dom.volumeValue.textContent = preferences.muted ? "Muted" : `${preferences.volume}%`;
    this.#dom.speedControl.value = String(preferences.rate);
    for (const radio of this.#dom.streamControls.querySelectorAll("input[name=stream-mode]")) {
      radio.checked = radio.value === preferences.streamMode;
      radio.disabled = radio.value === "compat" && !server.capabilities.transcoding;
    }
    this.#renderQualityProfiles();

    const current = playback.previewTime ?? playback.currentTime;
    this.#dom.timeline.max = String(playback.duration || 0);
    this.#dom.timeline.value = String(Math.min(current, playback.duration || current));
    const timelineProgress = playback.duration > 0 ? Math.min(100, Math.max(0, (current / playback.duration) * 100)) : 0;
    this.#dom.timeline.style.setProperty("--timeline-progress", `${timelineProgress}%`);
    this.#dom.timeline.disabled = !(playback.duration > 0);
    this.#dom.timeline.setAttribute("aria-valuetext", timelineValueText(current, playback.duration));
    this.#dom.timeline.setAttribute("aria-busy", String(playback.status === "seeking"));
    this.#dom.timelineStatus.textContent = playback.status === "seeking"
      ? `${timelineValueText(current, playback.duration)}. Starting a compatible stream.`
      : timelineValueText(current, playback.duration);
    this.#dom.timelineCurrent.textContent = clockLabel(current);
    this.#dom.timelineDuration.textContent = playback.duration > 0 ? clockLabel(playback.duration) : "Unknown";

    const previous = queueNeighbor(queue.entries, item.id, -1);
    const next = queueNeighbor(queue.entries, item.id, 1);
    const queueIndex = queue.entries.findIndex((entry) => String(entry.id) === String(item.id));
    this.#dom.queuePosition.textContent = queueIndex < 0 ? "" : queue.status === "loading"
      ? `Item ${queueIndex + 1} · loading queue…`
      : `Item ${queueIndex + 1} of ${queue.entries.length}`;
    this.#dom.previousButton.disabled = !previous;
    this.#dom.nextButton.disabled = !next;
    this.#dom.previousButton.title = previous ? `Previous: ${previous.title}` : queue.status === "loading" ? "Loading the rest of the queue" : "No previous item in this queue";
    this.#dom.nextButton.title = next ? `Next: ${next.title}` : queue.status === "loading" ? "Loading the rest of the queue" : "No next item in this queue";
    this.#dom.previousButton.setAttribute("aria-label", previous
      ? `Previous item: ${previous.title}`
      : queue.status === "loading" ? "Previous item unavailable while the queue loads" : "Previous item unavailable at the start of this queue");
    this.#dom.nextButton.setAttribute("aria-label", next
      ? `Next item: ${next.title}`
      : queue.status === "loading" ? "Next item unavailable while the queue loads" : "Next item unavailable at the end of this queue");

    this.#dom.playbackMode.hidden = !playback.sourceMode;
    this.#dom.playbackMode.classList.toggle("compat", playback.sourceMode === "compatible");
    this.#dom.modeLabel.textContent = playback.sourceMode === "compatible" ? "Compatible playback" : "Original file";
    this.#renderAudioTracks();
    this.#renderChapters();
    this.#renderCaptions();
    this.#renderStreamInfo();
    this.#renderMessage();
  }

  async #loadSource(item, {
    start = 0,
    intent = "paused",
    forceSourceMode = null,
    forceStreamNegotiation = null,
    message = null,
    messageKind = null,
  } = {}) {
    this.#cancelSource();
    const state = this.#store.getState();
    const requestedMode = forceSourceMode || state.preferences.streamMode;
    const player = item.kind === "audio" ? this.#dom.audio : this.#dom.video;
    // Browser capability is authoritative. Server hints inform the UI but do
    // not veto FLAC/WAV/HEVC support that this particular browser advertises.
    const directSupport = Boolean(player.canPlayType(sourceMime(item)));
    const selected = forceSourceMode
      ? { mode: forceSourceMode, reason: `forced_${forceSourceMode}` }
      : chooseSource({ requestedMode, directSupport, transcoding: state.server.capabilities.transcoding });
    if (requestedMode === "compat" && !state.server.capabilities.transcoding) {
      const sessionId = ++this.#session;
      this.#store.dispatch({ type: "PLAYBACK_SOURCE", sessionId, sourceMode: "direct", sourceReason: "transcoding_disabled", segmentOffset: 0, start, intent });
      this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("transcode_disabled") });
      return;
    }
    const sessionId = ++this.#session;
    const controller = new AbortController();
    this.#sourceController = controller;
    const sourceMode = selected.mode;
    const segmentOffset = sourceMode === "compatible" ? compatibleSegmentStart(start) : 0;
    const sourceMessage = message || (sourceMode === "compatible" ? "Preparing compatible playback…" : null);
    this.#store.dispatch({
      type: "PLAYBACK_SOURCE",
      sessionId,
      sourceMode,
      sourceReason: selected.reason,
      segmentOffset,
      start,
      intent,
      message: sourceMessage,
    });

    const inactive = item.kind === "audio" ? this.#dom.video : this.#dom.audio;
    this.#resetMediaElement(inactive);
    this.#resetMediaElement(player);
    this.#attachCaptions(item);
    player.playbackRate = state.preferences.rate;
    player.volume = state.preferences.volume / 100;
    player.muted = state.preferences.muted;
    player.loop = state.preferences.loop;
    const valid = () => this.#store.getState().playback.sessionId === sessionId && !controller.signal.aborted;
    let streamNegotiation = null;
    if (sourceMode === "compatible") {
      if (forceStreamNegotiation) {
        streamNegotiation = forceStreamNegotiation;
      } else {
        const playback = this.#store.getState().playback;
        const selectedTrack = playback.audioTracks.find((track) => Number(track.index) === Number(playback.selectedAudio));
        const mediaCapabilities = navigator.mediaCapabilities;
        streamNegotiation = await negotiateCompatibleStreams({
          item,
          track: selectedTrack,
          quality: state.preferences.quality,
          canPlayType: (contentType) => player.canPlayType(contentType),
          decodingInfo: typeof mediaCapabilities?.decodingInfo === "function"
            ? (configuration) => this.#decodingInfo(configuration)
            : null,
        });
      }
      if (!valid()) return;
      this.#store.dispatch({ type: "PLAYBACK_AUX", values: { streamNegotiation } });
    }
    const status = (next, values = {}) => {
      if (valid()) this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId, status: next, ...values });
    };
    const listen = (name, handler) => player.addEventListener(name, handler, { signal: controller.signal });
    listen("loadstart", () => status("loading"));
    listen("waiting", () => status("waiting", { message: this.#store.getState().playback.message || "Buffering…" }));
    listen("stalled", () => status("waiting", { message: "The connection stalled. Buffering…" }));
    listen("seeking", () => { if (sourceMode === "direct") status("seeking", { message: "Seeking…" }); });
    listen("seeked", () => { if (sourceMode === "direct") status(player.paused ? "paused" : "playing", { message: null }); });
    listen("loadedmetadata", () => {
      if (!valid()) return;
      if (sourceMode === "direct" && start > 0) {
        try { player.currentTime = Math.min(start, Number.isFinite(player.duration) ? player.duration : start); } catch (_) { /* canplay retries naturally */ }
      } else if (sourceMode === "compatible" && start > segmentOffset) {
        try { player.currentTime = start - segmentOffset; } catch (_) { /* canplay retries naturally */ }
      }
      const duration = itemDuration(item, player.duration);
      this.#store.dispatch({ type: "PLAYBACK_TIME", sessionId, currentTime: start, duration });
      if (messageKind === "seek") status("loading", { message: null });
    });
    listen("durationchange", () => {
      if (!valid()) return;
      this.#store.dispatch({ type: "PLAYBACK_TIME", sessionId, currentTime: this.globalTime(), duration: itemDuration(item, player.duration) });
    });
    listen("canplay", async () => {
      if (!valid()) return;
      status("paused", { message: null });
      if (intent === "playing") await this.#attemptPlay(sessionId, player);
    });
    listen("playing", () => {
      status("playing", { intent: "playing", message: null });
      this.#announce("Playing");
      this.#updateWakeLock();
    });
    listen("pause", () => {
      if (!valid() || player.ended) return;
      const currentStatus = this.#store.getState().playback.status;
      if (!["loading", "waiting", "seeking", "error"].includes(currentStatus)) status("paused", { intent: "paused" });
      this.#progressWriter.flush();
      this.#updateWakeLock();
    });
    listen("timeupdate", () => {
      if (!valid()) return;
      const global = sourceMode === "compatible" ? segmentOffset + player.currentTime : player.currentTime;
      this.#store.dispatch({ type: "PLAYBACK_TIME", sessionId, currentTime: global, duration: itemDuration(item, player.duration) });
      // A seek while paused is still valuable resume state, and some engines do
      // not expose a mutable `paused` value until real media has decoded.
      this.#progressWriter.schedule();
      this.#updateMediaSessionPosition();
    });
    listen("ratechange", () => {
      if (!valid() || !Number.isFinite(player.playbackRate)) return;
      this.#setPreference("rate", player.playbackRate);
    });
    listen("volumechange", () => {
      if (!valid()) return;
      this.#setPreference("volume", Math.round(player.volume * 100));
      this.#setPreference("muted", player.muted);
    });
    listen("ended", () => {
      if (!valid()) return;
      const duration = this.#store.getState().playback.duration;
      this.#store.dispatch({ type: "PLAYBACK_TIME", sessionId, currentTime: duration, duration });
      status("ended", { intent: "paused", message: null });
      clearProgress(item.id);
      this.#announce("Playback ended");
      this.#updateWakeLock();
      if (this.#store.getState().preferences.autoplay) this.playRelative(1);
    });
    listen("error", () => this.#handleMediaError(sessionId, item, sourceMode, start, intent, streamNegotiation));

    const params = new URLSearchParams();
    let sourceUrl = item.source_url;
    if (sourceMode === "compatible") {
      params.set("mode", "compatible");
      params.set("audio", String(this.#store.getState().playback.selectedAudio));
      params.set("start", String(segmentOffset));
      params.set("quality", this.#store.getState().preferences.quality);
      params.set("video_mode", streamNegotiation.video);
      params.set("audio_mode", streamNegotiation.audio);
      params.set("reason", selected.reason);
      params.set("request", String(sessionId));
      sourceUrl = `${item.fallback_url}${item.fallback_url.includes("?") ? "&" : "?"}${params}`;
    } else {
      params.set("reason", selected.reason);
      params.set("request", String(sessionId));
      sourceUrl = `${item.source_url}${item.source_url.includes("?") ? "&" : "?"}${params}`;
    }
    player.src = sourceUrl;
    player.load();
    if (sourceMode === "compatible") this.#pollTranscodeState(item.id, sessionId, controller.signal);
  }

  #decodingInfo(configuration) {
    const key = JSON.stringify(configuration);
    if (!this.#capabilityCache.has(key)) {
      const result = navigator.mediaCapabilities.decodingInfo(configuration).catch((error) => {
        this.#capabilityCache.delete(key);
        throw error;
      });
      this.#capabilityCache.set(key, result);
    }
    return this.#capabilityCache.get(key);
  }

  #pollTranscodeState(itemId, sessionId, signal) {
    const poll = async () => {
      if (signal.aborted || sessionId !== this.#store.getState().playback.sessionId) return;
      if (!["loading", "waiting", "seeking"].includes(this.#store.getState().playback.status)) return;
      try {
        const payload = await this.#api.transcodeStatus(itemId, sessionId, signal);
        const message = {
          queued: "Waiting for a transcode slot…",
          starting: "Starting compatible playback…",
          producing: "Preparing video…",
        }[payload.state];
        if (message && ["loading", "waiting", "seeking"].includes(this.#store.getState().playback.status)) {
          this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId, status: "loading", message });
        } else if (payload.state === "failed") {
          this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("transcode_failed", "The transcode producer failed.") });
          return;
        } else if (payload.state === "cancelled") {
          this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("transcode_cancelled") });
          return;
        }
      } catch (error) {
        if (error?.name === "AbortError") return;
        const category = apiErrorCategory(error);
        if (["media_missing", "transcode_busy", "transcode_failed", "transcode_cancelled", "offline", "network"].includes(category)) {
          this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError(category, error?.technical || "") });
          return;
        }
      }
      this.#statusTimer = window.setTimeout(poll, 500);
    };
    poll();
  }

  async #attemptPlay(sessionId, player) {
    try {
      await player.play();
    } catch (error) {
      if (sessionId !== this.#store.getState().playback.sessionId) return;
      if (error?.name === "NotAllowedError") {
        this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId, status: "paused", intent: "paused", message: "Playback is ready. Press Play to begin." });
        this.#announce("Playback is ready. Press Play to begin.");
      } else if (error?.name !== "AbortError") {
        this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("unknown", error?.message || "play() failed") });
      }
    }
  }

  async #handleMediaError(sessionId, item, sourceMode, start, intent, streamNegotiation) {
    if (sessionId !== this.#store.getState().playback.sessionId) return;
    const preferences = this.#store.getState().preferences;
    const capabilities = this.#store.getState().server.capabilities;
    const mediaCode = this.activePlayer().error?.code;
    if (sourceMode === "direct" && preferences.streamMode === "auto" && capabilities.transcoding) {
      this.#announce("Original playback failed. Switching to compatible playback.");
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: "compatible",
        message: "Switching to compatible playback…",
        messageKind: "fallback",
      });
      return;
    }
    // Capability APIs are advisory. If a copied HEVC or audio stream reaches
    // the browser but fails decoding, retry once with the portable H.264/AAC
    // path before presenting a terminal MediaError.
    if (sourceMode === "compatible"
      && mediaCode === 3
      && (streamNegotiation?.video === "copy" || streamNegotiation?.audio === "copy")) {
      const portableNegotiation = {
        ...streamNegotiation,
        video: "transcode",
        audio: "transcode",
      };
      this.#announce("The copied stream could not be decoded. Trying portable compatible playback.");
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: "compatible",
        forceStreamNegotiation: portableNegotiation,
        message: "Trying portable compatible playback…",
        messageKind: "fallback",
      });
      return;
    }
    const sourceReason = this.#store.getState().playback.sourceReason;
    let code = sourceMode === "direct"
      ? (!capabilities.transcoding && sourceReason === "transcoding_disabled" ? "transcode_disabled" : "unsupported_direct")
      : "transcode_failed";
    // navigator.onLine is only a hint and WebKit can leave it false after an
    // intentionally aborted request. Ask the typed API for the real cause.
    if (sourceMode === "compatible") {
      try {
        const payload = await this.#api.transcodeStatus(item.id, sessionId);
        if (sessionId !== this.#store.getState().playback.sessionId) return;
        if (payload.state === "queued") code = "transcode_busy";
        else if (payload.state === "cancelled") code = "transcode_cancelled";
        else if (payload.state === "failed") code = "transcode_failed";
      } catch (error) {
        if (sessionId !== this.#store.getState().playback.sessionId) return;
        const category = apiErrorCategory(error);
        if (category !== "unknown") code = category;
      }
    } else if (sourceMode === "direct") {
      try {
        await this.#api.item(item.id);
      } catch (error) {
        if (sessionId !== this.#store.getState().playback.sessionId) return;
        const category = apiErrorCategory(error);
        if (["media_missing", "offline", "network"].includes(category)) code = category;
      }
    }
    if (sessionId !== this.#store.getState().playback.sessionId) return;
    this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError(code, mediaCode ? `MediaError code ${mediaCode}` : "Media element error") });
  }

  async #enrichAudioTracks() {
    const state = this.#store.getState();
    const { item, sessionId, audioTracks } = state.playback;
    if (!item) return null;
    if (state.playback.audioTracksStatus === "loading") return null;
    if (state.playback.audioTracksStatus === "ready") return item;
    if (item.stream_metadata_complete) {
      this.#store.dispatch({ type: "AUDIO_TRACKS_SUCCESS", sessionId, item, tracks: audioTracks, chapters: item.chapters || [] });
      return item;
    }
    this.#store.dispatch({ type: "AUDIO_TRACKS_LOADING" });
    try {
      const payload = await this.#api.item(item.id, { enrich: true });
      const tracks = payload.audio_tracks || audioTracks;
      const chapters = payload.chapters || item.chapters || [];
      const enriched = {
        ...item,
        ...(payload.item || {}),
        audio_tracks: tracks,
        chapters,
        stream_metadata_complete: true,
      };
      this.#store.dispatch({ type: "AUDIO_TRACKS_SUCCESS", sessionId, item: enriched, tracks, chapters });
      return enriched;
    } catch (error) {
      if (error?.name === "AbortError") return null;
      this.#store.dispatch({ type: "AUDIO_TRACKS_ERROR", sessionId, error });
      return item;
    }
  }

  #selectAudioTrack(index) {
    const state = this.#store.getState();
    const { playback } = state;
    if (!playback.item || index === playback.selectedAudio) return;
    this.#store.dispatch({ type: "PLAYBACK_AUX", values: { selectedAudio: index } });
    const start = this.globalTime();
    const intent = this.activePlayer().paused ? "paused" : "playing";
    this.#loadSource(playback.item, {
      start,
      intent,
      forceSourceMode: "compatible",
      message: "Changing audio track…",
      messageKind: "audio",
    });
  }

  #selectCaption(value) {
    this.#setPreference("caption", value);
    this.#store.dispatch({ type: "PLAYBACK_AUX", values: { selectedCaption: value } });
    this.#applyCaptionMode(value);
  }

  #attachCaptions(item) {
    for (const old of this.#dom.video.querySelectorAll("track")) old.remove();
    for (const caption of item.captions || []) {
      if (!caption.browser_supported || !caption.url) continue;
      const track = document.createElement("track");
      track.kind = "subtitles";
      track.label = caption.label;
      track.srclang = caption.language || "und";
      track.src = caption.url;
      track.dataset.captionIndex = String(caption.index);
      this.#dom.video.append(track);
    }
    window.setTimeout(() => this.#applyCaptionMode(this.#store.getState().playback.selectedCaption), 0);
  }

  #applyCaptionMode(value) {
    for (const track of this.#dom.video.textTracks || []) {
      track.mode = "disabled";
    }
    const nodes = [...this.#dom.video.querySelectorAll("track")];
    nodes.forEach((node, index) => {
      const selected = value !== "off" && node.dataset.captionIndex === String(value);
      if (this.#dom.video.textTracks[index]) this.#dom.video.textTracks[index].mode = selected ? "showing" : "disabled";
    });
  }

  #renderAudioTracks() {
    const { playback } = this.#store.getState();
    const key = `${playback.item?.id}:${playback.audioTracksStatus}:${playback.audioTracks.map((track) => `${track.index}-${track.language}-${track.title}`).join("|")}`;
    if (key !== this.#audioRenderKey) {
      this.#audioRenderKey = key;
      this.#dom.audioTrackControls.replaceChildren();
      for (const track of playback.audioTracks) {
        const option = document.createElement("option");
        option.value = String(track.index);
        option.textContent = audioTrackLabel(track);
        this.#dom.audioTrackControls.append(option);
      }
    }
    this.#dom.audioTrackControl.hidden = playback.audioTracks.length < 2;
    this.#dom.audioTrackControls.value = String(playback.selectedAudio);
    this.#dom.audioTrackControls.disabled = playback.audioTracksStatus === "loading";
    this.#dom.audioTrackRetry.hidden = playback.audioTracksStatus !== "error";
    this.#dom.audioTrackStatus.hidden = !["loading", "error"].includes(playback.audioTracksStatus);
    this.#dom.audioTrackStatus.textContent = playback.audioTracksStatus === "loading"
      ? "Loading audio tracks…"
      : playback.audioTracksStatus === "error" ? "Audio-track details are unavailable." : "";
  }

  #renderChapters() {
    const { playback } = this.#store.getState();
    const chapters = playback.chapters || [];
    const key = chapters.map((chapter) => `${chapter.index}:${chapter.start_seconds}:${chapter.title}`).join("|");
    this.#dom.chapterControl.hidden = chapters.length === 0;
    const currentChapter = chapters.filter((chapter) => chapter.start_seconds <= playback.currentTime).at(-1);
    if (key === this.#chapterRenderKey) {
      if (currentChapter) this.#dom.chapterControls.value = String(currentChapter.start_seconds);
      return;
    }
    this.#chapterRenderKey = key;
    this.#dom.chapterControls.replaceChildren();
    this.#dom.chapterMarkers.replaceChildren();
    for (const chapter of chapters) {
      const option = document.createElement("option");
      option.value = String(chapter.start_seconds);
      option.textContent = `${chapter.title} · ${clockLabel(chapter.start_seconds)}`;
      this.#dom.chapterControls.append(option);
      const marker = document.createElement("option");
      marker.value = String(chapter.start_seconds);
      marker.label = chapter.title;
      this.#dom.chapterMarkers.append(marker);
    }
    if (currentChapter) this.#dom.chapterControls.value = String(currentChapter.start_seconds);
  }

  #renderCaptions() {
    const { playback } = this.#store.getState();
    const captions = playback.item?.captions || [];
    const key = `${playback.item?.id}:${playback.selectedCaption}:${captions.map((caption) => `${caption.index}-${caption.browser_supported}`).join("|")}`;
    this.#dom.captionsButton.disabled = playback.item?.kind !== "video" || captions.length === 0;
    this.#dom.captionsButton.setAttribute("aria-pressed", String(playback.selectedCaption !== "off"));
    if (key === this.#captionRenderKey) return;
    this.#captionRenderKey = key;
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
      radio.addEventListener("change", () => this.#selectCaption(radio.value));
      label.append(radio, document.createTextNode(caption.browser_supported ? caption.label : `${caption.label} (${caption.source_format?.toUpperCase()} is not supported in browsers)`));
      this.#dom.captionChoices.append(label);
    }
  }

  #renderQualityProfiles() {
    const state = this.#store.getState();
    const profiles = state.server.capabilities.quality_profiles || [];
    const key = profiles.map((profile) => profile.id).join("|");
    if (this.#dom.qualityControl.dataset.profiles !== key) {
      this.#dom.qualityControl.dataset.profiles = key;
      this.#dom.qualityControl.replaceChildren();
      for (const profile of profiles) {
        const option = document.createElement("option");
        option.value = profile.id;
        option.textContent = profile.label;
        this.#dom.qualityControl.append(option);
      }
    }
    this.#dom.qualityControl.value = state.preferences.quality;
    this.#dom.qualityControl.disabled = !state.server.capabilities.transcoding;
  }

  #renderStreamInfo() {
    const { playback, preferences, server } = this.#store.getState();
    const item = playback.item;
    if (!item) return;
    const selectedTrack = playback.audioTracks.find((track) => Number(track.index) === Number(playback.selectedAudio));
    const sourceAudio = selectedTrack
      ? audioTrackLabel(selectedTrack)
      : [codecLabel(item.audio_codec), item.audio_layout || (item.channels ? `${item.channels}ch` : "")].filter(Boolean).join(" · ");
    const sourceVideo = item.kind === "video" ? [
      codecLabel(item.video_codec),
      item.video_profile,
      videoLevelLabel(item.video_level),
      item.bit_depth ? `${item.bit_depth}-bit` : "",
      item.pixel_format,
      item.resolution || (item.width && item.height ? `${item.width}×${item.height}` : ""),
      item.frame_rate ? `${item.frame_rate} fps` : "",
    ].filter(Boolean).join(" · ") : "None";
    replaceFacts(this.#dom.sourceStreamFacts, [
      ["Container", containerLabel(item)],
      ["Video", sourceVideo],
      ["Audio", sourceAudio],
      ["Frame timing", item.video_repair_required ? "Malformed display-order timestamps detected" : ""],
    ]);

    if (playback.sourceMode !== "compatible") {
      this.#dom.streamInfoSummary.textContent = "The browser is consuming the original file directly; the server is not transcoding it.";
      replaceFacts(this.#dom.outputStreamFacts, [
        ["Container", `${containerLabel(item)} · unchanged`],
        ["Video", item.kind === "video" ? `${sourceVideo} · unchanged` : "None"],
        ["Audio", `${sourceAudio} · unchanged`],
      ]);
      return;
    }

    const profile = (server.capabilities.quality_profiles || []).find((entry) => entry.id === preferences.quality)
      || (server.capabilities.quality_profiles || [])[0];
    const negotiation = playback.streamNegotiation;
    if (!negotiation) {
      this.#dom.streamInfoSummary.textContent = "The browser is checking the source video and audio codecs independently.";
      replaceFacts(this.#dom.outputStreamFacts, [
        ["Container", "Fragmented MP4"],
        ["Video", item.kind === "video" ? "Checking browser support…" : "None"],
        ["Audio", "Checking browser support…"],
      ]);
      return;
    }
    const copiesVideo = item.kind === "video" && negotiation.video === "copy";
    const repairsVideo = item.kind === "video" && negotiation.video === "repair";
    const copiesAudio = negotiation.audio === "copy";
    const repairHevc = repairsVideo && item.repair_video_encoder === "hevc_nvenc";
    const repairPreservesHdr = repairHevc && ["hdr10", "dv-p8"].includes(String(item.hdr || "").toLowerCase());
    const encodedVideo = repairHevc && profile
      ? [
        "HEVC (hevc_nvenc)",
        "Main 10 profile",
        "Level 5.1",
        "p010le",
        `up to ${profile.max_width}×${profile.max_height} at ${profile.max_fps} fps`,
        `${profile.max_video_kbps} kbps maximum`,
        repairPreservesHdr ? "HDR10 preserved" : "",
      ].filter(Boolean).join(" · ")
      : profile
      ? [
        `H.264 (${item.compatible_video_encoder || "server encoder"})`,
        `${profile.h264_profile} profile`,
        `Level ${profile.h264_level}`,
        profile.pixel_format,
        `up to ${profile.max_width}×${profile.max_height} at ${profile.max_fps} fps`,
        `${profile.max_video_kbps} kbps maximum`,
      ].join(" · ")
      : `H.264 (${item.compatible_video_encoder || "server encoder"})`;
    const outputAudio = profile
      ? `AAC · stereo · ${profile.audio_kbps} kbps`
      : "AAC · stereo";
    this.#dom.streamInfoSummary.textContent = repairsVideo
      ? `The source file has malformed display-order timestamps. The server is re-encoding the video to restore stable frame order${repairHevc ? ` while preserving HEVC${repairPreservesHdr ? " and HDR10" : ""}` : ""}${copiesAudio ? "; audio is copied unchanged" : "; audio is converted to AAC"}.`
      : copiesVideo && copiesAudio
      ? "The browser supports both codecs; the server is remuxing them without transcoding."
      : copiesVideo
        ? "The original video bitstream is copied unchanged; only the audio is transcoded for browser compatibility."
        : copiesAudio
          ? "The original audio bitstream is copied unchanged; only the video is transcoded for browser compatibility."
          : "The server is producing browser-compatible H.264 video and AAC audio.";
    replaceFacts(this.#dom.outputStreamFacts, [
      ["Container", "Fragmented MP4"],
      ["Video", item.kind === "video" ? (copiesVideo
        ? `${sourceVideo} · copied unchanged (no video re-encode)`
        : repairsVideo ? `${sourceVideo} → ${encodedVideo} · frame order repaired` : encodedVideo) : "None"],
      ["Audio", copiesAudio ? `${sourceAudio} · copied unchanged (no audio re-encode)` : `${sourceAudio} → ${outputAudio}`],
      ["Browser video probe", item.kind === "video" ? capabilityProbeLabel(negotiation.videoContentType, negotiation.videoProbe) : "Not applicable"],
      ["Browser audio probe", capabilityProbeLabel(negotiation.audioContentType, negotiation.audioProbe)],
    ]);
  }

  #renderMessage() {
    const { playback } = this.#store.getState();
    const error = playback.error;
    const transient = !error && ["loading", "waiting", "seeking"].includes(playback.status);
    const text = error?.message || (transient ? null : playback.message);
    this.#dom.playerMessage.hidden = !text;
    if (!text) {
      this.#dom.playerMessageText.textContent = "";
      this.#dom.technicalMessage.textContent = "";
      return;
    }
    this.#dom.playerMessage.setAttribute("role", error ? "alert" : "status");
    this.#dom.playerMessageText.textContent = text;
    const actions = error?.actions || [];
    this.#dom.playerRetry.hidden = !actions.includes("retry");
    this.#dom.tryCompatible.hidden = !actions.includes("try_compatible");
    this.#dom.playOriginal.hidden = !actions.includes("play_original");
    this.#dom.returnLibrary.hidden = !actions.includes("return_to_library");
    this.#dom.technicalDetails.hidden = !error?.technical;
    this.#dom.technicalMessage.textContent = error?.technical || "";
  }

  #bindControls() {
    this.#dom.playButton.addEventListener("click", () => this.togglePlay());
    this.#dom.streamInfoButton.addEventListener("click", (event) => {
      const pointerActivated = event.detail > 0;
      this.#renderStreamInfo();
      if (pointerActivated) {
        this.#dom.streamInfoDialog.addEventListener("close", () => {
          window.setTimeout(() => {
            if (document.activeElement === this.#dom.streamInfoButton) this.#dom.streamInfoButton.blur();
          }, 0);
        }, { once: true });
      }
      this.#dom.streamInfoDialog.showModal();
    });
    this.#dom.previousButton.addEventListener("click", () => this.playRelative(-1));
    this.#dom.nextButton.addEventListener("click", () => this.playRelative(1));
    for (const button of this.#dom.seekButtons) {
      button.addEventListener("click", () => this.seekTo(this.globalTime() + Number(button.dataset.seek)));
    }
    this.#dom.timeline.addEventListener("input", () => {
      this.#store.dispatch({ type: "PLAYBACK_PREVIEW", value: Number(this.#dom.timeline.value) });
    });
    this.#dom.timeline.addEventListener("change", () => {
      const target = Number(this.#dom.timeline.value);
      this.#store.dispatch({ type: "PLAYBACK_PREVIEW", value: null });
      this.seekTo(target);
    });
    this.#dom.timeline.addEventListener("blur", () => this.#store.dispatch({ type: "PLAYBACK_PREVIEW", value: null }));
    this.#dom.muteButton.addEventListener("click", () => {
      const muted = !this.#store.getState().preferences.muted;
      this.#setPreference("muted", muted);
      this.#dom.video.muted = muted;
      this.#dom.audio.muted = muted;
    });
    this.#dom.volumeControl.addEventListener("input", () => {
      const volume = Number(this.#dom.volumeControl.value);
      this.#setPreference("volume", volume);
      this.#setPreference("muted", volume === 0);
      for (const player of [this.#dom.video, this.#dom.audio]) {
        player.volume = volume / 100;
        player.muted = volume === 0;
      }
    });
    this.#dom.speedControl.addEventListener("change", () => {
      const rate = Number(this.#dom.speedControl.value);
      this.#setPreference("rate", rate);
      this.#dom.video.playbackRate = rate;
      this.#dom.audio.playbackRate = rate;
    });
    this.#dom.loopButton.addEventListener("click", () => {
      const loop = !this.#store.getState().preferences.loop;
      this.#setPreference("loop", loop);
      this.#dom.video.loop = loop;
      this.#dom.audio.loop = loop;
    });
    this.#dom.fitButton.addEventListener("click", () => this.#setPreference("fill", !this.#store.getState().preferences.fill));
    this.#dom.pipButton.addEventListener("click", async () => {
      try {
        if (document.pictureInPictureElement) await document.exitPictureInPicture();
        else await this.#dom.video.requestPictureInPicture();
      } catch (error) {
        const sessionId = this.#store.getState().playback.sessionId;
        this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("unknown", error?.message || "PiP request failed") });
      }
    });
    this.#dom.video.addEventListener("enterpictureinpicture", () => this.#store.dispatch({ type: "PLAYBACK_AUX", values: { pip: true } }));
    this.#dom.video.addEventListener("leavepictureinpicture", () => this.#store.dispatch({ type: "PLAYBACK_AUX", values: { pip: false } }));
    this.#dom.fullscreenButton.addEventListener("click", (event) => {
      this.toggleFullscreen();
      // Pointer activation leaves the button focused in desktop browsers. That
      // focus is incidental and must not pin the controls open indefinitely;
      // keyboard activation keeps focus so the controls remain accessible.
      if (event.detail > 0) event.currentTarget.blur();
    });
    document.addEventListener("fullscreenchange", () => {
      this.#store.dispatch({ type: "PLAYBACK_AUX", values: { fullscreen: document.fullscreenElement === this.#dom.playerStage } });
      this.#showControls();
      this.#updateWakeLock();
    });
    this.#dom.captionsButton.addEventListener("click", () => {
      const open = this.#dom.captionMenu.hidden;
      this.#dom.captionMenu.hidden = !open;
      this.#dom.captionsButton.setAttribute("aria-expanded", String(open));
      if (open) this.#dom.captionChoices.querySelector("input:checked")?.focus();
    });
    this.#dom.audioTrackControls.addEventListener("change", () => this.#selectAudioTrack(Number(this.#dom.audioTrackControls.value)));
    this.#dom.audioTrackRetry.addEventListener("click", async () => {
      const enriched = await this.#enrichAudioTracks();
      const playback = this.#store.getState().playback;
      if (enriched && playback.sourceMode === "compatible") {
        this.#loadSource(enriched, {
          start: this.globalTime(),
          intent: this.activePlayer().paused ? "paused" : "playing",
          forceSourceMode: "compatible",
          message: "Applying stream details…",
        });
      }
    });
    this.#dom.chapterControls.addEventListener("change", () => this.seekTo(Number(this.#dom.chapterControls.value)));
    this.#dom.advancedPlaybackButton.addEventListener("click", () => this.#dom.advancedPlaybackDialog.showModal());
    this.#dom.streamControls.addEventListener("change", (event) => {
      if (!(event.target instanceof HTMLInputElement)) return;
      this.#setPreference("streamMode", event.target.value, "stream");
      const playback = this.#store.getState().playback;
      if (playback.item) this.#loadSource(playback.item, { start: this.globalTime(), intent: this.activePlayer().paused ? "paused" : "playing" });
    });
    this.#dom.qualityControl.addEventListener("change", () => {
      this.#setPreference("quality", this.#dom.qualityControl.value);
      const playback = this.#store.getState().playback;
      if (playback.item && playback.sourceMode === "compatible") this.#loadSource(playback.item, { start: this.globalTime(), intent: this.activePlayer().paused ? "paused" : "playing", forceSourceMode: "compatible", message: "Changing compatible quality…" });
    });
    this.#dom.captionSizeControl.addEventListener("change", () => this.#setPreference("captionSize", this.#dom.captionSizeControl.value));
    this.#dom.captionBackgroundControl.addEventListener("change", () => this.#setPreference("captionBackground", this.#dom.captionBackgroundControl.value));
    this.#dom.autoplayControl.addEventListener("change", () => this.#setPreference("autoplay", this.#dom.autoplayControl.checked));
    this.#dom.shortcutHelpButton.addEventListener("click", () => this.#dom.shortcutDialog.showModal());
    this.#dom.playerRetry.addEventListener("click", () => {
      const playback = this.#store.getState().playback;
      if (playback.item) this.#loadSource(playback.item, { start: playback.currentTime, intent: playback.intent, forceSourceMode: playback.sourceMode });
    });
    this.#dom.tryCompatible.addEventListener("click", () => {
      const playback = this.#store.getState().playback;
      if (playback.item) this.#loadSource(playback.item, { start: playback.currentTime, intent: "playing", forceSourceMode: "compatible", message: "Preparing compatible playback…" });
    });
    this.#dom.playOriginal.addEventListener("click", () => {
      const playback = this.#store.getState().playback;
      if (playback.item) this.#loadSource(playback.item, { start: playback.currentTime, intent: "playing", forceSourceMode: "direct" });
    });
    this.#dom.returnLibrary.addEventListener("click", () => this.#dom.libraryPanel.focus());
    this.#dom.playerStage.addEventListener("click", (event) => {
      if (event.target === this.#dom.video) this.togglePlay();
    });
    for (const eventName of ["pointerenter", "pointermove", "pointerdown", "touchstart", "focusin"]) {
      this.#dom.playerStage.addEventListener(eventName, () => this.#showControls(), { passive: true });
    }
    this.#dom.playerStage.addEventListener("pointerleave", () => this.#hideControls());
    this.#dom.playerStage.addEventListener("focusout", () => {
      window.setTimeout(() => {
        if (this.#controlsHaveKeyboardFocus()) return;
        if (this.#dom.playerStage.matches(":hover")) this.#showControls();
        else this.#hideControls();
      }, 0);
    });
    document.addEventListener("keydown", (event) => this.#handleShortcut(event));
    document.addEventListener("visibilitychange", () => this.#updateWakeLock());
    window.addEventListener("pagehide", () => this.#progressWriter.flush());
    this.#installMediaSessionHandlers();
  }

  #handleShortcut(event) {
    if (event.key === "Escape" && document.fullscreenElement === this.#dom.playerStage) {
      document.exitFullscreen().catch(() => {});
      return;
    }
    const target = event.target;
    const editable = target instanceof HTMLElement
      && (target.isContentEditable || ["INPUT", "SELECT", "TEXTAREA", "BUTTON"].includes(target.tagName));
    const scoped = document.fullscreenElement === this.#dom.playerStage
      || this.#dom.playerStage.matches(":hover")
      || this.#dom.playerStage.contains(document.activeElement);
    if (!scoped || editable || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
    const key = event.key.toLowerCase();
    if ([" ", "k"].includes(key)) { event.preventDefault(); this.togglePlay(); }
    else if (["arrowleft", "j"].includes(key)) { event.preventDefault(); this.seekTo(this.globalTime() - 10); }
    else if (["arrowright", "l"].includes(key)) { event.preventDefault(); this.seekTo(this.globalTime() + 10); }
    else if (key === "m") { event.preventDefault(); this.#dom.muteButton.click(); }
    else if (key === "f") { event.preventDefault(); this.toggleFullscreen(); }
    else if (key === "?") { event.preventDefault(); this.#dom.shortcutDialog.showModal(); }
  }

  #showControls() {
    this.#dom.playerStage.classList.add("controls-visible");
    if (this.#controlsTimer !== null) window.clearTimeout(this.#controlsTimer);
    this.#controlsTimer = window.setTimeout(() => {
      this.#controlsTimer = null;
      if (!this.#controlsArePinned()) {
        this.#dom.playerStage.classList.remove("controls-visible");
      }
    }, CONTROLS_IDLE_MS);
  }

  #hideControls() {
    if (this.#controlsTimer !== null) window.clearTimeout(this.#controlsTimer);
    this.#controlsTimer = null;
    if (!this.#controlsArePinned()) {
      this.#dom.playerStage.classList.remove("controls-visible");
    }
  }

  #controlsArePinned() {
    return !this.#dom.captionMenu.hidden || this.#controlsHaveKeyboardFocus();
  }

  #controlsHaveKeyboardFocus() {
    const focused = document.activeElement;
    return this.#dom.playbackControls.contains(focused) && focused.matches(":focus-visible");
  }

  #applyInitialPreferences() {
    const preferences = this.#store.getState().preferences;
    for (const player of [this.#dom.video, this.#dom.audio]) {
      player.controls = false;
      player.playbackRate = preferences.rate;
      player.volume = preferences.volume / 100;
      player.muted = preferences.muted;
      player.loop = preferences.loop;
    }
  }

  #setPreference(name, value, storageName = name) {
    if (this.#store.getState().preferences[name] === value) return;
    savePreference(storageName, value);
    this.#store.dispatch({ type: "PREFERENCE", name, value });
  }

  #cancelSeekTimer() {
    if (this.#seekTimer !== null) window.clearTimeout(this.#seekTimer);
    this.#seekTimer = null;
  }

  #cancelSource({ keepElement = true } = {}) {
    this.#progressWriter.flush();
    this.#sourceController?.abort();
    this.#sourceController = null;
    this.#api.abortItem();
    if (this.#statusTimer !== null) window.clearTimeout(this.#statusTimer);
    this.#statusTimer = null;
    this.#cancelSeekTimer();
    if (this.#announceTimer !== null) window.clearTimeout(this.#announceTimer);
    this.#announceTimer = null;
    if (!keepElement) this.#resetMediaElement(this.activePlayer());
  }

  #resetMediaElement(player) {
    if (!player) return;
    player.pause();
    player.removeAttribute("src");
    player.load();
  }

  #writeProgress() {
    const { playback } = this.#store.getState();
    if (!playback.item || !(playback.duration > 0)) return;
    const resumable = resumePosition(playback.currentTime, playback.duration);
    if (resumable > 0) saveProgress(playback.item.id, resumable, playback.duration);
    else clearProgress(playback.item.id);
  }

  #bringPlayerIntoView() {
    const rect = this.#dom.playerPanel.getBoundingClientRect();
    const visible = rect.top >= 0 && rect.bottom <= window.innerHeight;
    if (visible) return;
    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    this.#dom.playerPanel.scrollIntoView({ behavior: reduced ? "auto" : "smooth", block: "nearest" });
  }

  #announce(message) {
    const sessionId = this.#store.getState().playback.sessionId;
    if (this.#announceTimer !== null) window.clearTimeout(this.#announceTimer);
    this.#dom.playbackLive.textContent = "";
    this.#announceTimer = window.setTimeout(() => {
      this.#announceTimer = null;
      if (sessionId === this.#store.getState().playback.sessionId) {
        this.#dom.playbackLive.textContent = message;
      }
    }, 0);
  }

  #updateMediaSessionMetadata(item) {
    if (!("mediaSession" in navigator)) return;
    navigator.mediaSession.metadata = new MediaMetadata({
      title: item.title,
      artist: item.artist || "",
      album: item.album || "",
      artwork: item.art_url ? [{ src: new URL(item.art_url, window.location.href).href }] : [],
    });
  }

  #updateMediaSessionPosition() {
    if (!("mediaSession" in navigator) || !("setPositionState" in navigator.mediaSession)) return;
    const playback = this.#store.getState().playback;
    if (!(playback.duration > 0) || playback.currentTime > playback.duration) return;
    try {
      navigator.mediaSession.setPositionState({ duration: playback.duration, playbackRate: this.activePlayer().playbackRate, position: Math.max(0, playback.currentTime) });
    } catch (_) { /* Browsers can reject while metadata is changing. */ }
  }

  #installMediaSessionHandlers() {
    if (!("mediaSession" in navigator)) return;
    const handlers = {
      play: () => { if (this.activePlayer()?.paused) this.togglePlay(); },
      pause: () => { if (this.activePlayer() && !this.activePlayer().paused) this.activePlayer().pause(); },
      seekbackward: (details) => this.seekTo(this.globalTime() - (details.seekOffset || 10)),
      seekforward: (details) => this.seekTo(this.globalTime() + (details.seekOffset || 10)),
      seekto: (details) => this.seekTo(details.seekTime || 0),
      previoustrack: () => this.playChapterRelative(-1),
      nexttrack: () => this.playChapterRelative(1),
    };
    for (const [action, handler] of Object.entries(handlers)) {
      try { navigator.mediaSession.setActionHandler(action, handler); } catch (_) { /* Optional action. */ }
    }
  }

  async #updateWakeLock() {
    const state = this.#store.getState();
    const shouldHold = state.playback.status === "playing"
      && state.playback.item?.kind === "video"
      && state.playback.fullscreen
      && document.visibilityState === "visible";
    if (!shouldHold) {
      try { await this.#wakeLock?.release(); } catch (_) { /* Already released. */ }
      this.#wakeLock = null;
      return;
    }
    if (this.#wakeLock || !("wakeLock" in navigator)) return;
    try {
      this.#wakeLock = await navigator.wakeLock.request("screen");
      this.#wakeLock.addEventListener("release", () => { this.#wakeLock = null; }, { once: true });
    } catch (_) {
      // Denial is normal (battery policy, permissions, or unsupported context).
    }
  }
}
