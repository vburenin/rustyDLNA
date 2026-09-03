import {
  aiUpscaleQualityAvailable,
  apiErrorCategory,
  automaticCompatibleRecoveryProfile,
  audioTrackLabel,
  bufferedRangeSecondsAhead,
  chooseSource,
  clockLabel,
  compatibleVideoDimensions,
  compatibleSegmentStart,
  directSourceSupported,
  doubleTapSeekDelta,
  fullscreenAction,
  hdrDisplaySupport,
  hdrVideoOutputCandidate,
  isAndroidDevice,
  isApplePhoneDevice,
  itemDuration,
  isAppleMobileDevice,
  isSafariBrowser,
  mediaDetails,
  nativeHlsQualityProfile,
  negotiateCompatibleStreams,
  parseHlsMediaPlaylist,
  playbackControlLabel,
  playbackError,
  primaryVideoCodec,
  queueNeighbor,
  resumePosition,
  saferCompatibleQualityProfile,
  selectedAudioRequiresCompatible,
  seekTarget,
  sourceApplicableQualityProfiles,
  sourceAwareQualityProfileLabel,
  sourceBoundedQualityProfile,
  SOURCE_MODES,
  STREAM_MODES,
  timelineValueText,
  trickplayFrame,
  trickplayPreloadUrls,
  validQualityProfileId,
} from "./core.js";
import {
  clearProgress,
  createProgressWriter,
  progressFor,
  savePreference,
  saveProgress,
} from "./preferences.js";

const CONTROLS_IDLE_MS = 3000;
const TOUCH_CONTROLS_IDLE_MS = 5000;
const DOUBLE_TAP_WINDOW_MS = 500;
const TOUCH_TAP_MAX_DURATION_MS = 450;
const TOUCH_TAP_MAX_MOVE_PX = 28;
const SYNTHETIC_TOUCH_CLICK_GRACE_MS = 1_000;
const SEEK_GESTURE_FEEDBACK_MS = 900;
const COMPATIBLE_SEEK_DEBOUNCE_MS = 400;
const TRANSCODE_PREPARING_POLL_MS = 500;
const TRANSCODE_ACTIVE_POLL_MS = 10_000;
const ORIGINAL_BUFFER_STALL_MS = 12_000;
const COMPATIBLE_STARTUP_STALL_MS = 8_000;
const NATIVE_HLS_STARTUP_STALL_MS = 12_000;
const MAX_COMPATIBLE_SOURCE_RELOADS = 1;
const MAX_AUTOMATIC_TRANSCODE_RETRIES = 3;
const TRANSCODE_BUSY_RETRY_WINDOW_MS = 5 * 60 * 1_000;
const MAX_HELD_VIDEO_FRAME_PIXELS = 4_194_304;
const MAX_DECODED_TRICKPLAY_SHEETS = 2;
// Copied UHD fragments can exceed 10 MB per second. Keep the total window
// below Chromium's practical SourceBuffer quota instead of treating every
// codec and bitrate like a small mobile rendition. Seeks start a new
// generation, so only a short backward window is useful here.
const MEDIA_SOURCE_BUFFER_AHEAD_SECONDS = 10;
const MEDIA_SOURCE_RETAIN_BEHIND_SECONDS = 5;
const MEDIA_SOURCE_PLAYLIST_POLL_MS = 500;
const MAX_MEDIA_CAPABILITY_CACHE_ENTRIES = 64;
const ANDROID_MEDIA_SOURCE_TYPES = Object.freeze([
  'video/mp4; codecs="avc1.42c01f,mp4a.40.2"',
  'video/mp4; codecs="avc1.42e01f,mp4a.40.2"',
]);

function supportsNativeHlsDelivery(player) {
  if (!isAppleMobileDevice(navigator) && !isSafariBrowser(navigator)) return false;
  try {
    return player.canPlayType("application/vnd.apple.mpegurl") !== "";
  } catch (_) {
    return false;
  }
}

function androidMediaSourceType() {
  if (!isAndroidDevice(navigator)
    || typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  return ANDROID_MEDIA_SOURCE_TYPES.find((contentType) => {
    try {
      return globalThis.MediaSource.isTypeSupported(contentType);
    } catch (_) {
      return false;
    }
  }) || null;
}

function advertisedMediaSourceType(videoOutputs, outputId) {
  if (typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  const contentType = videoOutputs?.find((output) => output?.id === outputId)?.mse_content_type;
  if (!contentType) return null;
  try {
    return globalThis.MediaSource.isTypeSupported(contentType) ? contentType : null;
  } catch (_) {
    return null;
  }
}

function copiedHevcHdrEncodingFallbackType(capabilities, streamNegotiation) {
  if (streamNegotiation?.video !== "copy"
    || !streamNegotiation?.outputVideoProbe?.supported
    || !streamNegotiation?.outputVideoContentType) return null;
  return advertisedMediaSourceType(capabilities?.video_outputs, "hevc_hdr10");
}

function displayHdrSupport() {
  return hdrDisplaySupport(typeof globalThis.matchMedia === "function"
    ? (query) => globalThis.matchMedia(query)
    : null);
}

function nativeHlsVideoOutput(item, capabilities) {
  const candidate = hdrVideoOutputCandidate(
    item,
    capabilities?.video_outputs,
  );
  if (!candidate) return "h264_sdr";
  // Safari can return an empty canPlayType result for both exact Main-10 and
  // generic hvc1 strings even though AVFoundation accepts that codec in native
  // HLS. This path is already restricted to native Apple HLS, so treat the
  // capability API as advisory and let a real media error trigger the bounded,
  // same-quality H.264 SDR recovery.
  return candidate.id;
}

function copiedAndroidMediaSourceType(item) {
  if (!isAndroidDevice(navigator)
    || !["h264", "hevc"].includes(primaryVideoCodec(item?.video_codec))
    || typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  const match = /^video\/mp4\s*;\s*codecs\s*=\s*"([^"]+)"$/i.exec(String(item?.video_content_type || ""));
  const videoCodec = match?.[1]?.split(",", 1)[0]?.trim();
  if (!/^(?:avc1|hvc1)\./i.test(videoCodec || "")) return null;
  const contentType = `video/mp4; codecs="${videoCodec},mp4a.40.2"`;
  try {
    return globalThis.MediaSource.isTypeSupported(contentType) ? contentType : null;
  } catch (_) {
    return null;
  }
}

function copiedHevcMediaSourceType(item, streamNegotiation) {
  if (isAndroidDevice(navigator)
    || isAppleMobileDevice(navigator)
    || streamNegotiation?.video !== "copy"
    || streamNegotiation?.audio !== "transcode"
    || typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  const match = /^video\/mp4\s*;\s*codecs\s*=\s*"([^"]+)"$/i.exec(String(item?.video_content_type || ""));
  const videoCodec = match?.[1]?.split(",", 1)[0]?.trim();
  if (!/^hvc1\./i.test(videoCodec || "")) return null;
  const contentType = `video/mp4; codecs="${videoCodec},mp4a.40.2"`;
  try {
    return globalThis.MediaSource.isTypeSupported(contentType) ? contentType : null;
  } catch (_) {
    return null;
  }
}

function abortedError() {
  return new DOMException("Playback source was replaced.", "AbortError");
}

function abortableDelay(milliseconds, signal) {
  if (signal.aborted) return Promise.reject(abortedError());
  return new Promise((resolve, reject) => {
    const timer = window.setTimeout(done, milliseconds);
    signal.addEventListener("abort", abort, { once: true });
    function cleanup() {
      window.clearTimeout(timer);
      signal.removeEventListener("abort", abort);
    }
    function done() {
      cleanup();
      resolve();
    }
    function abort() {
      cleanup();
      reject(abortedError());
    }
  });
}

function waitForMediaEvent(target, eventName, signal, errorEvent = "error") {
  if (signal.aborted) return Promise.reject(abortedError());
  return new Promise((resolve, reject) => {
    target.addEventListener(eventName, done, { once: true });
    if (errorEvent) target.addEventListener(errorEvent, failed, { once: true });
    signal.addEventListener("abort", abort, { once: true });
    function cleanup() {
      target.removeEventListener(eventName, done);
      if (errorEvent) target.removeEventListener(errorEvent, failed);
      signal.removeEventListener("abort", abort);
    }
    function done() {
      cleanup();
      resolve();
    }
    function failed() {
      cleanup();
      reject(new Error(`Media Source ${errorEvent}`));
    }
    function abort() {
      cleanup();
      reject(abortedError());
    }
  });
}

function sourceBufferOperation(sourceBuffer, operation, signal) {
  if (signal.aborted) return Promise.reject(abortedError());
  return new Promise((resolve, reject) => {
    sourceBuffer.addEventListener("updateend", done, { once: true });
    sourceBuffer.addEventListener("error", failed, { once: true });
    signal.addEventListener("abort", abort, { once: true });
    function cleanup() {
      sourceBuffer.removeEventListener("updateend", done);
      sourceBuffer.removeEventListener("error", failed);
      signal.removeEventListener("abort", abort);
    }
    function done() {
      cleanup();
      resolve();
    }
    function failed() {
      cleanup();
      reject(new Error("Media Source buffer rejected a fragment."));
    }
    function abort() {
      cleanup();
      reject(abortedError());
    }
    try {
      operation();
    } catch (error) {
      cleanup();
      reject(error);
    }
  });
}

function bufferedSecondsAhead(sourceBuffer, currentTime) {
  const ranges = [];
  for (let index = 0; index < sourceBuffer.buffered.length; index += 1) {
    ranges.push({
      start: sourceBuffer.buffered.start(index),
      end: sourceBuffer.buffered.end(index),
    });
  }
  return bufferedRangeSecondsAhead(ranges, currentTime);
}

function waitForMediaSourcePlayback(player, signal) {
  if (!player.paused) return Promise.resolve();
  return new Promise((resolve, reject) => {
    player.addEventListener("play", resumed, { once: true });
    signal.addEventListener("abort", aborted, { once: true });
    function cleanup() {
      player.removeEventListener("play", resumed);
      signal.removeEventListener("abort", aborted);
    }
    function resumed() {
      cleanup();
      resolve();
    }
    function aborted() {
      cleanup();
      reject(abortedError());
    }
  });
}

function currentFullscreenElement() {
  return document.fullscreenElement
    || document.webkitFullscreenElement
    || document.webkitCurrentFullScreenElement
    || null;
}

function stageFullscreenRequest(stage) {
  return stage.requestFullscreen
    || stage.webkitRequestFullscreen
    || stage.webkitRequestFullScreen
    || null;
}

function stageFullscreenExit() {
  return document.exitFullscreen
    || document.webkitExitFullscreen
    || document.webkitCancelFullScreen
    || null;
}

function stageFullscreenSupported(stage) {
  if (typeof stageFullscreenRequest(stage) !== "function") return false;
  const flags = [document.fullscreenEnabled, document.webkitFullscreenEnabled]
    .filter((value) => typeof value === "boolean");
  return flags.length === 0 || flags.some(Boolean);
}

function nativeVideoFullscreenRequest(video) {
  return video.webkitEnterFullscreen || video.webkitEnterFullScreen || null;
}

function nativeVideoFullscreenExit(video) {
  return video.webkitExitFullscreen || video.webkitExitFullScreen || null;
}

function nativeVideoFullscreenActive(video) {
  return video.webkitDisplayingFullscreen === true
    || video.webkitPresentationMode === "fullscreen";
}

function playbackSessionSeed() {
  if (globalThis.crypto?.getRandomValues) {
    const words = globalThis.crypto.getRandomValues(new Uint32Array(2));
    return (words[0] & 0x000f_ffff) * 4_294_967_296 + words[1] + 1;
  }
  return Date.now() * 1_024 + Math.floor(Math.random() * 1_024);
}

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
  #playbackSession = 0;
  #sourceController = null;
  #mediaSourceObjectUrl = null;
  #trickplayController = null;
  #trickplayManifest = null;
  #trickplayImages = new Map();
  #trickplayTarget = null;
  #trickplayPreloadStarted = false;
  #seekTimer = null;
  #controlsTimer = null;
  #touchControlsUntil = 0;
  #touchTapStart = null;
  #pendingTouchTap = null;
  #touchTapTimer = null;
  #suppressVideoClickUntil = 0;
  #seekGestureFeedbackTimer = null;
  #statusTimer = null;
  #startupTimer = null;
  #canplayReportedSession = null;
  #playingReportedSession = null;
  #announceTimer = null;
  #announcementKey = "";
  #wakeLock = null;
  #wakeLockRequest = null;
  #wakeLockGeneration = 0;
  #wakeLockDeniedGeneration = null;
  #wakeLockDesired = false;
  #wakeLockBlockedSession = null;
  #pipRequestSession = null;
  #pipRequestToken = 0;
  #pipActiveSession = null;
  #fullscreenRequestSession = null;
  #fullscreenRequestKind = null;
  #fullscreenRequestPointerActivated = false;
  #fullscreenRequestToken = 0;
  #fullscreenActiveSession = null;
  #fullscreenActiveKind = null;
  #displayViewportFrame = null;
  #captionRenderKey = "";
  #audioRenderKey = "";
  #chapterRenderKey = "";
  #capabilityCache = new Map();
  #automaticTranscodeRetries = 0;
  #pendingCompatibleRetrySession = null;
  #transcodeBusyStartedAt = null;
  #transcodeBusyRetries = 0;
  #compatibleSourceReloads = 0;
  #nativeHlsSuspendedSession = null;
  #progressWriter;
  #onReturnLibrary;
  #onClosePlayback;

  constructor({
    store,
    api,
    dom,
    onReturnLibrary = () => dom.libraryPanel.focus(),
    onClosePlayback = null,
  }) {
    this.#store = store;
    this.#api = api;
    this.#dom = dom;
    this.#session = playbackSessionSeed();
    this.#playbackSession = playbackSessionSeed();
    this.#progressWriter = createProgressWriter(() => this.#writeProgress());
    this.#onReturnLibrary = onReturnLibrary;
    this.#onClosePlayback = onClosePlayback || onReturnLibrary;
    this.#bindControls();
    this.#applyInitialPreferences();
    this.#store.subscribe(() => {
      this.render();
      this.#updateWakeLock();
    });
    this.render();
  }

  async select(item, { preserveQueue = false, startAt = 0, signal = null } = {}) {
    if (signal?.aborted) return;
    let preparationError = null;
    if (signal) {
      const prepared = await this.#prepareSelection(item, signal);
      if (signal.aborted) return;
      item = prepared.item;
      preparationError = prepared.error;
    }
    this.#resetAutomaticTranscodeRecovery();
    if (!preserveQueue) {
      this.#store.dispatch({ type: "QUEUE_REPLACE", entries: [item], generation: null });
    }
    this.#cancelTrickplay();
    this.#resetTouchGestures();
    this.#releaseHeldVideoFrame();
    this.#cancelSource({ keepElement: false });
    this.#pipRequestSession = null;
    this.#pipRequestToken += 1;
    this.#pipActiveSession = null;
    this.#fullscreenRequestSession = null;
    this.#fullscreenRequestKind = null;
    this.#fullscreenRequestPointerActivated = false;
    this.#fullscreenRequestToken += 1;
    this.#playbackSession = playbackSessionSeed();
    const sessionId = ++this.#session;
    this.#dom.resumePrompt.hidden = true;
    this.#store.dispatch({ type: "PLAYBACK_SELECT", sessionId, item, duration: itemDuration(item) });
    this.#adoptActiveFullscreen(sessionId);
    this.#loadTrickplay(item);
    if (preparationError) {
      this.#store.dispatch({ type: "AUDIO_TRACKS_ERROR", sessionId, error: preparationError });
    }
    this.#bringPlayerIntoView();
    this.#showControls();
    const enriched = signal ? item : await this.#enrichAudioTracks();
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
      this.render();
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
    if (playback.sourceMode === SOURCE_MODES.COMPATIBLE) {
      if (playback.status === "seeking") return playback.currentTime;
      const exactStartWithinSegment = playback.currentTime - playback.segmentOffset;
      if (["loading", "waiting"].includes(playback.status)
        && exactStartWithinSegment > 0
        && local === 0) {
        // A replacement source starts at a segment boundary. Until metadata
        // makes its intra-segment offset seekable, retain the exact requested
        // time for another keyboard or Media Session seek.
        return playback.currentTime;
      }
    }
    return playback.sourceMode === SOURCE_MODES.COMPATIBLE ? playback.segmentOffset + local : local;
  }

  async togglePlay() {
    const { playback } = this.#store.getState();
    if (!playback.item) return;
    if (playback.status === "ended") {
      if (playback.sourceMode === SOURCE_MODES.COMPATIBLE) {
        this.#loadSource(playback.item, {
          start: 0,
          intent: "playing",
          forceSourceMode: SOURCE_MODES.COMPATIBLE,
          forceAndroidMediaSource: playback.mediaSourceDelivery,
        });
        return;
      }
      this.seekTo(0);
      this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId: playback.sessionId, status: "paused", intent: "playing", message: null });
      await this.#attemptPlay(playback.sessionId, this.activePlayer());
      return;
    }
    const player = this.activePlayer();
    if (playbackControlLabel(playback.status, playback.intent) === "Pause") {
      if (["loading", "waiting", "seeking"].includes(playback.status)) {
        this.#store.dispatch({
          type: "PLAYBACK_STATUS",
          sessionId: playback.sessionId,
          status: playback.status,
          intent: "paused",
        });
      }
      player.pause();
      return;
    }
    if (this.#restartSuspendedNativeHls(playback)) return;
    this.#store.dispatch({
      type: "PLAYBACK_STATUS",
      sessionId: playback.sessionId,
      status: playback.status,
      intent: "playing",
    });
    await this.#attemptPlay(playback.sessionId, player);
  }

  seekTo(value) {
    const state = this.#store.getState();
    const { playback } = state;
    if (!playback.item || !(playback.duration > 0)) return;
    const target = seekTarget(value, playback.duration);
    this.#resetAutomaticTranscodeRecovery();
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
    if (playback.sourceMode !== SOURCE_MODES.COMPATIBLE) {
      if (Math.abs(this.globalTime() - target) > 0.05) this.#holdVideoFrame();
      this.activePlayer().currentTime = target;
      this.#store.dispatch({ type: "PLAYBACK_TIME", sessionId: playback.sessionId, currentTime: target, duration: playback.duration });
      // Persist explicit seeks even while paused; a media engine may not emit a
      // timeupdate before the user chooses another title or closes the page.
      this.#progressWriter.schedule();
      return;
    }
    this.#holdVideoFrame();
    this.#cancelSeekTimer();
    const intent = playback.intent;
    this.#store.dispatch({
      type: "PLAYBACK_TIME", sessionId: playback.sessionId,
      currentTime: target, duration: playback.duration,
    });
    this.#store.dispatch({
      type: "PLAYBACK_STATUS", sessionId: playback.sessionId, status: "seeking", intent,
      message: `Starting at ${clockLabel(target)}…`,
    });
    this.#cancelSource({
      keepElement: false,
      cancelTranscode: compatibleSegmentStart(target) !== playback.segmentOffset,
    });
    this.#seekTimer = window.setTimeout(() => {
      this.#seekTimer = null;
      const latest = this.#store.getState().playback;
      if (latest.sessionId !== playback.sessionId) return;
      this.#loadSource(playback.item, {
        start: target,
        intent: latest.intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: playback.streamNegotiation,
        forceQuality: playback.outputQuality,
        forceAndroidMediaSource: playback.mediaSourceDelivery,
        message: `Starting at ${clockLabel(target)}…`,
        messageKind: "seek",
      });
    }, COMPATIBLE_SEEK_DEBOUNCE_MS);
  }

  #resetTouchGestures() {
    if (this.#touchTapTimer !== null) window.clearTimeout(this.#touchTapTimer);
    if (this.#seekGestureFeedbackTimer !== null) window.clearTimeout(this.#seekGestureFeedbackTimer);
    this.#touchTapStart = null;
    this.#pendingTouchTap = null;
    this.#touchTapTimer = null;
    this.#seekGestureFeedbackTimer = null;
    this.#dom.seekGestureFeedback.hidden = true;
  }

  #expirePendingTouchTap() {
    if (this.#touchTapTimer !== null) window.clearTimeout(this.#touchTapTimer);
    this.#pendingTouchTap = null;
    this.#touchTapTimer = null;
  }

  #scheduleSingleTouchTap(tap) {
    this.#pendingTouchTap = tap;
    this.#touchTapTimer = window.setTimeout(() => this.#expirePendingTouchTap(), DOUBLE_TAP_WINDOW_MS);
  }

  #showSeekGestureFeedback(delta) {
    if (this.#seekGestureFeedbackTimer !== null) window.clearTimeout(this.#seekGestureFeedbackTimer);
    this.#dom.seekGestureFeedback.dataset.direction = delta < 0 ? "backward" : "forward";
    this.#dom.seekGestureFeedback.textContent = `${delta < 0 ? "−" : "+"}${Math.abs(delta)}s`;
    this.#dom.seekGestureFeedback.hidden = false;
    this.#seekGestureFeedbackTimer = window.setTimeout(() => {
      this.#seekGestureFeedbackTimer = null;
      this.#dom.seekGestureFeedback.hidden = true;
    }, SEEK_GESTURE_FEEDBACK_MS);
  }

  #handleVideoTouchPointerDown(event) {
    const videoSurface = event.target === this.#dom.video
      || event.target === this.#dom.video.parentElement
      || event.target === this.#dom.playbackControls
      || event.target === this.#dom.playerStage;
    // The first tap reveals the full-stage control overlay. Android therefore
    // hit-tests the second tap against that overlay instead of the video even
    // though the user tapped the same visible picture. Treat only the empty
    // overlay itself as video surface; its interactive descendants remain
    // ordinary controls.
    if (event.pointerType !== "touch" || !event.isPrimary) return;
    if (!videoSurface) {
      this.#touchTapStart = null;
      this.#expirePendingTouchTap();
      return;
    }
    this.#touchTapStart = {
      pointerId: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      startedAt: performance.now(),
    };
  }

  #handleVideoTouchPointerUp(event) {
    const start = this.#touchTapStart;
    if (!start || event.pointerType !== "touch" || event.pointerId !== start.pointerId) return;
    this.#touchTapStart = null;
    const now = performance.now();
    if (now - start.startedAt > TOUCH_TAP_MAX_DURATION_MS
      || Math.hypot(event.clientX - start.x, event.clientY - start.y) > TOUCH_TAP_MAX_MOVE_PX) return;
    event.preventDefault();
    this.#suppressVideoClickUntil = now + SYNTHETIC_TOUCH_CLICK_GRACE_MS;
    const sessionId = this.#store.getState().playback.sessionId;
    const tap = { sessionId, x: event.clientX, y: event.clientY, at: now };
    const pending = this.#pendingTouchTap;
    const bounds = this.#dom.video.getBoundingClientRect();
    const delta = pending
      && pending.sessionId === sessionId
      && now - pending.at <= DOUBLE_TAP_WINDOW_MS
      ? doubleTapSeekDelta({
        firstX: pending.x,
        firstY: pending.y,
        secondX: tap.x,
        secondY: tap.y,
        viewportLeft: bounds.left,
        viewportWidth: bounds.width,
      })
      : 0;
    if (delta !== 0) {
      if (this.#touchTapTimer !== null) window.clearTimeout(this.#touchTapTimer);
      this.#pendingTouchTap = null;
      this.#touchTapTimer = null;
      const playback = this.#store.getState().playback;
      if (playback.duration > 0) {
        const current = this.globalTime();
        const target = seekTarget(current + delta, playback.duration);
        if (Math.abs(target - current) > 0.05) {
          this.seekTo(target);
          this.#showSeekGestureFeedback(delta);
        }
      }
      return;
    }
    if (pending) this.#expirePendingTouchTap();
    this.#scheduleSingleTouchTap(tap);
  }

  playRelative(delta) {
    const state = this.#store.getState();
    const next = queueNeighbor(state.queue.entries, state.playback.item?.id, delta);
    if (next) this.select(next, { preserveQueue: true });
  }

  async closePlayback() {
    const playback = this.#store.getState().playback;
    const fullscreenActionName = this.#fullscreenAction();
    this.#dom.captionMenu.hidden = true;
    this.#dom.captionsButton.setAttribute("aria-expanded", "false");
    for (const dialog of [
      this.#dom.advancedPlaybackDialog,
      this.#dom.streamInfoDialog,
      this.#dom.shortcutDialog,
    ]) {
      if (dialog.open) dialog.close();
    }
    this.#dom.resumePrompt.hidden = true;
    if (fullscreenActionName.startsWith("exit_")) {
      await this.toggleFullscreen();
    }
    if (document.pictureInPictureElement === this.#dom.video) {
      try { await document.exitPictureInPicture(); } catch (_) { /* Closing still proceeds. */ }
    }
    if (!playback.item) {
      this.#onClosePlayback();
      return;
    }
    this.#resetAutomaticTranscodeRecovery();
    this.#cancelTrickplay();
    this.#resetTouchGestures();
    this.#releaseHeldVideoFrame();
    this.#cancelSource({ keepElement: false });
    this.#nativeHlsSuspendedSession = null;
    this.#pipRequestSession = null;
    this.#pipRequestToken += 1;
    this.#pipActiveSession = null;
    this.#fullscreenRequestSession = null;
    this.#fullscreenRequestKind = null;
    this.#fullscreenRequestPointerActivated = false;
    this.#fullscreenRequestToken += 1;
    this.#playbackSession = playbackSessionSeed();
    const sessionId = ++this.#session;
    this.#store.dispatch({ type: "PLAYBACK_CLEAR", sessionId });
    if ("mediaSession" in navigator) navigator.mediaSession.metadata = null;
    this.#onClosePlayback();
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

  #fullscreenAction() {
    const videoSelected = this.#store.getState().playback.item?.kind === "video";
    const nativeVideoSupported = typeof nativeVideoFullscreenRequest(this.#dom.video) === "function";
    const preferExpandedPlayer = videoSelected && isApplePhoneDevice(navigator);
    return fullscreenAction({
      stageActive: currentFullscreenElement() === this.#dom.playerStage,
      nativeVideoActive: nativeVideoFullscreenActive(this.#dom.video),
      expandedPlayerActive: this.#dom.playerStage.classList.contains("expanded-player"),
      preferExpandedPlayer,
      stageSupported: !preferExpandedPlayer && stageFullscreenSupported(this.#dom.playerStage),
      nativeVideoSupported,
      videoSelected,
    });
  }

  #setPageScrollLocked(active) {
    document.documentElement.classList.toggle("player-expanded", active);
    document.body.classList.toggle("player-expanded", active);
  }

  #setExpandedPlayer(active) {
    this.#dom.playerStage.classList.toggle("expanded-player", active);
    this.#setPageScrollLocked(active || currentFullscreenElement() === this.#dom.playerStage);
    if (active) {
      this.#updateDisplayViewport();
    } else if (currentFullscreenElement() !== this.#dom.playerStage) {
      this.#clearDisplayViewport();
    }
  }

  #usesMeasuredDisplayViewport() {
    return currentFullscreenElement() === this.#dom.playerStage
      || this.#dom.playerStage.classList.contains("expanded-player");
  }

  #clearDisplayViewport() {
    if (this.#displayViewportFrame !== null) cancelAnimationFrame(this.#displayViewportFrame);
    this.#displayViewportFrame = null;
    for (const name of ["--player-viewport-left", "--player-viewport-top", "--player-viewport-width", "--player-viewport-height"]) {
      this.#dom.playerStage.style.removeProperty(name);
    }
  }

  #scheduleDisplayViewport() {
    if (!this.#usesMeasuredDisplayViewport() || this.#displayViewportFrame !== null) return;
    this.#displayViewportFrame = requestAnimationFrame(() => {
      this.#displayViewportFrame = null;
      this.#updateDisplayViewport();
    });
  }

  #updateDisplayViewport() {
    if (!this.#usesMeasuredDisplayViewport()) return;
    const viewport = window.visualViewport;
    const values = {
      "--player-viewport-left": viewport?.offsetLeft ?? 0,
      "--player-viewport-top": viewport?.offsetTop ?? 0,
      "--player-viewport-width": viewport?.width ?? document.documentElement.clientWidth,
      "--player-viewport-height": viewport?.height ?? window.innerHeight,
    };
    for (const [name, value] of Object.entries(values)) {
      const valid = name.endsWith("width") || name.endsWith("height") ? value > 0 : value >= 0;
      if (Number.isFinite(value) && valid) this.#dom.playerStage.style.setProperty(name, `${value}px`);
    }
  }

  #adoptActiveFullscreen(sessionId) {
    let kind = null;
    if (currentFullscreenElement() === this.#dom.playerStage) kind = "stage";
    else if (nativeVideoFullscreenActive(this.#dom.video)) kind = "native_video";
    else if (this.#dom.playerStage.classList.contains("expanded-player")) kind = "expanded_player";
    this.#fullscreenActiveSession = kind ? sessionId : null;
    this.#fullscreenActiveKind = kind;
    if (kind) {
      if (kind === "stage") this.#setPageScrollLocked(true);
      if (kind === "stage" || kind === "expanded_player") this.#updateDisplayViewport();
      this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { fullscreen: true } });
    }
  }

  #fullscreenEntered(kind) {
    if (this.#fullscreenRequestKind !== kind || this.#fullscreenRequestSession === null) return;
    const sessionId = this.#fullscreenRequestSession;
    const pointerActivated = this.#fullscreenRequestPointerActivated;
    this.#fullscreenRequestSession = null;
    this.#fullscreenRequestKind = null;
    this.#fullscreenRequestPointerActivated = false;
    this.#fullscreenActiveSession = sessionId;
    this.#fullscreenActiveKind = kind;
    if (kind === "stage") this.#setPageScrollLocked(true);
    if (kind === "stage" || kind === "expanded_player") this.#updateDisplayViewport();
    this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { fullscreen: true } });
    const focused = document.activeElement;
    if (pointerActivated
      && focused instanceof HTMLElement
      && this.#dom.playerStage.contains(focused)) {
      // Safari can move focus from a pointer-activated button to the new
      // fullscreen element and mark it focus-visible. That incidental focus
      // must not pin the complete control plane over the video indefinitely.
      focused.blur();
    }
    this.#showControls();
  }

  #fullscreenExited(kind) {
    if (this.#fullscreenActiveKind !== kind || this.#fullscreenActiveSession === null) return;
    const sessionId = this.#fullscreenActiveSession;
    this.#fullscreenActiveSession = null;
    this.#fullscreenActiveKind = null;
    if (kind === "stage" && !this.#dom.playerStage.classList.contains("expanded-player")) {
      this.#clearDisplayViewport();
      this.#setPageScrollLocked(false);
    }
    this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { fullscreen: false } });
    this.#showControls();
  }

  async toggleFullscreen({ pointerActivated = false } = {}) {
    const sessionId = this.#store.getState().playback.sessionId;
    const action = this.#fullscreenAction();
    if (action === "unavailable") {
      this.#dom.fullscreenButton.disabled = true;
      this.#dom.fullscreenButton.title = "Full screen is not available in this browser.";
      this.#dom.fullscreenButton.setAttribute("aria-label", "Full screen unavailable");
      return;
    }
    const requestToken = ++this.#fullscreenRequestToken;
    try {
      if (action === "exit_stage") {
        const exit = stageFullscreenExit();
        if (typeof exit === "function") await Promise.resolve(exit.call(document));
      } else if (action === "exit_native_video") {
        const exit = nativeVideoFullscreenExit(this.#dom.video);
        if (typeof exit === "function") await Promise.resolve(exit.call(this.#dom.video));
      } else if (action === "exit_expanded_player") {
        this.#setExpandedPlayer(false);
        this.#fullscreenExited("expanded_player");
      } else if (action === "enter_expanded_player") {
        this.#fullscreenRequestSession = sessionId;
        this.#fullscreenRequestKind = "expanded_player";
        this.#setExpandedPlayer(true);
        this.#fullscreenEntered("expanded_player");
      } else {
        const kind = action === "enter_stage" ? "stage" : "native_video";
        const request = kind === "stage"
          ? stageFullscreenRequest(this.#dom.playerStage)
          : nativeVideoFullscreenRequest(this.#dom.video);
        this.#fullscreenRequestSession = sessionId;
        this.#fullscreenRequestKind = kind;
        this.#fullscreenRequestPointerActivated = pointerActivated;
        const target = kind === "stage" ? this.#dom.playerStage : this.#dom.video;
        await Promise.resolve(request.call(target));
        if (requestToken !== this.#fullscreenRequestToken) return;
        const active = kind === "stage"
          ? currentFullscreenElement() === this.#dom.playerStage
          : nativeVideoFullscreenActive(this.#dom.video);
        if (active) this.#fullscreenEntered(kind);
      }
    } catch (_) {
      if (requestToken === this.#fullscreenRequestToken) {
        this.#fullscreenRequestSession = null;
        this.#fullscreenRequestKind = null;
        this.#fullscreenRequestPointerActivated = false;
      }
      if (sessionId !== this.#store.getState().playback.sessionId) return;
      this.#dom.fullscreenButton.title = "Full screen could not be opened.";
      this.#dom.fullscreenButton.setAttribute("aria-label", "Full screen could not be opened");
    }
  }

  render() {
    const state = this.#store.getState();
    const { playback, preferences, queue, server } = state;
    const item = playback.item;
    this.#syncPlaybackAnnouncement(playback);
    this.#dom.playerStage.classList.toggle("has-media", Boolean(item));
    this.#dom.playerStage.classList.toggle("has-video", item?.kind === "video");
    this.#dom.playerStage.classList.toggle("is-playing", playback.status === "playing");
    this.#dom.playerStage.classList.toggle("awaiting-play", playback.autoplayBlocked);
    this.#dom.playerEmpty.hidden = Boolean(item);
    this.#dom.nowPlaying.hidden = !item;
    this.#dom.closePlayerButton.hidden = !item;
    this.#dom.playbackControls.hidden = !item || !this.#dom.resumePrompt.hidden;
    if (!item) {
      this.#dom.nowPlayingTitle.textContent = "Nothing selected";
      this.#dom.nowPlayingMeta.textContent = "";
      document.title = `${server.name} · Library`;
      return;
    }

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
    this.#dom.videoFrameHold.classList.toggle("fill", preferences.fill);
    this.#dom.video.dataset.captionSize = preferences.captionSize;
    this.#dom.video.dataset.captionBackground = preferences.captionBackground;
    this.#dom.captionSizeControl.value = preferences.captionSize;
    this.#dom.captionBackgroundControl.value = preferences.captionBackground;
    this.#dom.autoplayControl.checked = preferences.autoplay;

    const busy = ["loading", "waiting", "seeking"].includes(playback.status);
    this.#dom.stageProgress.hidden = !busy;
    this.#dom.stageProgressLabel.textContent = playback.message || {
      loading: playback.sourceMode === SOURCE_MODES.COMPATIBLE ? "Preparing media" : "Loading media",
      waiting: "Buffering",
      seeking: "Seeking",
    }[playback.status] || "Loading media";

    this.#dom.playButton.setAttribute(
      "aria-label",
      playbackControlLabel(playback.status, playback.intent),
    );
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
    const fullscreenActionName = this.#fullscreenAction();
    const fullscreenAvailable = fullscreenActionName !== "unavailable";
    const expandedPlayer = ["enter_expanded_player", "exit_expanded_player"].includes(fullscreenActionName);
    this.#dom.fullscreenButton.disabled = !fullscreenAvailable;
    this.#dom.fullscreenButton.title = fullscreenAvailable
      ? expandedPlayer ? "Fill the visible screen while keeping rustyDLNA controls." : ""
      : "Full screen is not available in this browser.";
    this.#dom.fullscreenButton.setAttribute("aria-pressed", String(playback.fullscreen));
    this.#dom.fullscreenButton.setAttribute(
      "aria-label",
      fullscreenAvailable
        ? expandedPlayer
          ? (playback.fullscreen ? "Exit expanded player" : "Expand player")
          : (playback.fullscreen ? "Exit full screen" : "Enter full screen")
        : "Full screen unavailable",
    );

    this.#dom.volumeControl.value = String(preferences.volume);
    this.#dom.volumeControl.style.setProperty("--volume-level", `${preferences.muted ? 0 : preferences.volume}%`);
    this.#dom.volumeValue.textContent = preferences.muted ? "Muted" : `${preferences.volume}%`;
    this.#dom.speedControl.value = String(preferences.rate);
    for (const radio of this.#dom.streamControls.querySelectorAll("input[name=stream-mode]")) {
      radio.checked = radio.value === preferences.streamMode;
      radio.disabled = radio.value === STREAM_MODES.COMPATIBLE && !server.capabilities.transcoding;
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
    this.#dom.playbackMode.classList.toggle("compat", playback.sourceMode === SOURCE_MODES.COMPATIBLE);
    this.#dom.modeLabel.textContent = playback.sourceMode === SOURCE_MODES.COMPATIBLE ? "Compatible playback" : "Original file";
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
    forceQuality = null,
    forceAndroidMediaSource = false,
    mediaSourceRetry = false,
    preservePreviousTranscode = false,
    message = null,
    messageKind = null,
  } = {}) {
    this.#holdVideoFrame();
    this.#cancelSource({ cancelTranscode: !preservePreviousTranscode });
    this.#nativeHlsSuspendedSession = null;
    const state = this.#store.getState();
    const negotiationEpoch = state.server.negotiationEpoch;
    const requestedMode = state.preferences.streamMode;
    const player = item.kind === "audio" ? this.#dom.audio : this.#dom.video;
    // Exact browser capability results remain authoritative for codecs such as
    // HEVC. A broad container-only answer cannot validate an indexed video
    // codec that the server already knows normally needs conversion.
    const directSupport = directSourceSupported(
      item,
      (contentType) => player.canPlayType(contentType),
    );
    const selected = chooseSource({
      requestedMode,
      forcedMode: forceSourceMode,
      directSupport,
      transcoding: state.server.capabilities.transcoding,
      requiresCompatibleAudio: selectedAudioRequiresCompatible(
        state.playback.audioTracks,
        state.playback.selectedAudio,
      ),
    });
    if (selected.blocked) {
      const sessionId = ++this.#session;
      const pip = this.#rebindPiPSourceSession(item, sessionId);
      this.#store.dispatch({
        type: "PLAYBACK_SOURCE",
        sessionId,
        sourceMode: selected.mode,
        sourceReason: selected.reason,
        segmentOffset: 0,
        start,
        intent,
        pip,
      });
      this.#resetMediaElement(player);
      this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError(selected.blocked) });
      return;
    }
    const sessionId = ++this.#session;
    const pip = this.#rebindPiPSourceSession(item, sessionId);
    const playbackSessionId = this.#playbackSession;
    const controller = new AbortController();
    this.#sourceController = controller;
    const sourceMode = selected.mode;
    const advertisedProfiles = state.server.capabilities.quality_profiles || [];
    const requestedOutputQuality = sourceMode === SOURCE_MODES.COMPATIBLE
      && forceQuality
      && advertisedProfiles.some((profile) => profile?.id === forceQuality)
      ? forceQuality
      : sourceMode === SOURCE_MODES.COMPATIBLE ? state.preferences.quality : null;
    const preferredOutputQuality = sourceMode === SOURCE_MODES.COMPATIBLE
      ? sourceBoundedQualityProfile(
        advertisedProfiles,
        requestedOutputQuality,
        item,
        state.server.capabilities.ai_upscale,
      )
      : null;
    const nativeHlsAvailable = sourceMode === SOURCE_MODES.COMPATIBLE
      && item.kind === "video"
      && supportsNativeHlsDelivery(player);
    let nativeHlsDelivery = nativeHlsAvailable;
    const androidTranscodeEligible = sourceMode === SOURCE_MODES.COMPATIBLE
      && item.kind === "video"
      && isAndroidDevice(navigator);
    const androidMediaSourceSupport = androidTranscodeEligible ? androidMediaSourceType() : null;
    let mediaSourceType = forceAndroidMediaSource ? androidMediaSourceSupport : null;
    let mediaSourceDelivery = false;
    // A forced Android MSE retry does not itself require a lower-quality
    // encode: a supported copied H.264/HEVC stream must remain at Auto so the
    // server can honor video_mode=copy. The Android negotiation below lowers
    // quality only when it actually switches video to the portable encoder.
    let outputQuality = nativeHlsDelivery
      ? nativeHlsQualityProfile(
        advertisedProfiles,
        preferredOutputQuality,
        isAppleMobileDevice(navigator),
      )
      : preferredOutputQuality;
    const segmentOffset = sourceMode === SOURCE_MODES.COMPATIBLE ? compatibleSegmentStart(start) : 0;
    const sourceMessage = message || (nativeHlsDelivery
      ? "Preparing Safari stream…"
      : sourceMode === SOURCE_MODES.COMPATIBLE ? "Preparing compatible playback…" : null);
    this.#store.dispatch({
      type: "PLAYBACK_SOURCE",
      sessionId,
      sourceMode,
      sourceReason: selected.reason,
      outputQuality,
      nativeHlsDelivery,
      mediaSourceDelivery,
      segmentOffset,
      start,
      intent,
      message: sourceMessage,
      pip,
    });

    const inactive = item.kind === "audio" ? this.#dom.video : this.#dom.audio;
    this.#resetMediaElement(inactive);
    this.#resetMediaElement(player);
    this.#attachCaptions(item);
    player.playbackRate = state.preferences.rate;
    player.volume = state.preferences.volume / 100;
    player.muted = state.preferences.muted;
    player.loop = state.preferences.loop;
    player.disableRemotePlayback = false;
    player.removeAttribute("disableremoteplayback");
    const valid = () => this.#store.getState().playback.sessionId === sessionId && !controller.signal.aborted;
    let streamNegotiation = null;
    if (sourceMode === SOURCE_MODES.COMPATIBLE) {
      if (nativeHlsDelivery) {
        // Native Apple HLS uses only synchronous codec/display checks so an
        // advisory promise cannot consume transient playback activation before
        // AVFoundation has attached the playlist URL. A forced negotiation is
        // an active recovery decision and must not be replaced by another HDR
        // capability check after the rendition has already failed to decode.
        streamNegotiation = forceStreamNegotiation || {
          video: "transcode",
          audio: "transcode",
          videoOutput: nativeHlsVideoOutput(item, state.server.capabilities),
          hdrDisplay: displayHdrSupport(),
        };
      } else if (forceStreamNegotiation) {
        streamNegotiation = forceStreamNegotiation;
      } else {
        const playback = this.#store.getState().playback;
        const selectedTrack = playback.audioTracks.find((track) => Number(track.index) === Number(playback.selectedAudio));
        const mediaCapabilities = navigator.mediaCapabilities;
        streamNegotiation = await negotiateCompatibleStreams({
          item,
          track: selectedTrack,
          quality: outputQuality,
          qualityProfile: advertisedProfiles.find((profile) => profile?.id === outputQuality),
          videoOutputs: state.server.capabilities.video_outputs || [],
          aiUpscale: state.server.capabilities.ai_upscale,
          hdrDisplay: displayHdrSupport(),
          canPlayType: (contentType) => player.canPlayType(contentType),
          decodingInfo: typeof mediaCapabilities?.decodingInfo === "function"
            ? (configuration) => this.#decodingInfo(configuration)
            : null,
        });
      }
      if (!valid()) return;
      if (this.#store.getState().server.negotiationEpoch !== negotiationEpoch) {
        return this.#loadSource(item, {
          start,
          intent,
          forceSourceMode,
          forceQuality,
          forceAndroidMediaSource,
          mediaSourceRetry,
          preservePreviousTranscode,
          message,
          messageKind,
        });
      }
      if (androidTranscodeEligible) {
        const copiedMediaSourceSupport = streamNegotiation?.video === "copy"
          ? copiedAndroidMediaSourceType(item)
          : null;
        const selectedTrack = this.#store.getState().playback.audioTracks
          .find((track) => Number(track.index) === Number(this.#store.getState().playback.selectedAudio));
        const copiedAac = streamNegotiation?.audio === "copy"
          && String(selectedTrack?.codec || item.audio_codec || "").toLowerCase() === "aac";
        const advertisedHdrMediaSourceSupport = streamNegotiation?.videoOutput === "hevc_hdr10"
          ? advertisedMediaSourceType(
            state.server.capabilities.video_outputs,
            streamNegotiation.videoOutput,
          )
          : null;
        if (streamNegotiation?.videoOutput === "hevc_hdr10"
          && !advertisedHdrMediaSourceSupport) {
          streamNegotiation = {
            ...streamNegotiation,
            video: "transcode",
            videoOutput: "h264_sdr",
          };
        }
        mediaSourceType = copiedMediaSourceSupport
          || advertisedHdrMediaSourceSupport
          || androidMediaSourceSupport;
        if (mediaSourceType) {
          // Android's native loader can leave growing fragmented MP4 attached
          // without ever decoding it. Use finite MSE resources for every
          // compatible video. Preserve supported H.264/HEVC video and AAC;
          // otherwise select the portable H.264/AAC pair before requesting.
          streamNegotiation = {
            ...streamNegotiation,
            video: copiedMediaSourceSupport ? "copy" : "transcode",
            audio: copiedAac ? "copy" : "transcode",
            videoOutput: advertisedHdrMediaSourceSupport ? "hevc_hdr10" : "h264_sdr",
          };
          if (!copiedMediaSourceSupport
            && !advertisedHdrMediaSourceSupport
            && outputQuality === "auto") {
            outputQuality = saferCompatibleQualityProfile(advertisedProfiles, outputQuality)
              || outputQuality;
          }
          mediaSourceDelivery = true;
        }
      }
      const copiedHevcMediaSourceSupport = copiedHevcMediaSourceType(item, streamNegotiation);
      if (copiedHevcMediaSourceSupport) {
        mediaSourceType = copiedHevcMediaSourceSupport;
        mediaSourceDelivery = true;
      }
      const encodedMediaSourceSupport = !nativeHlsDelivery
        && streamNegotiation?.video === "transcode"
        && ["h264_sdr", "hevc_hdr10"].includes(streamNegotiation?.videoOutput)
        ? advertisedMediaSourceType(
          state.server.capabilities.video_outputs,
          streamNegotiation.videoOutput,
        )
        : null;
      if (encodedMediaSourceSupport) {
        // A native Chromium media loader can treat the currently available
        // tail of a growing fragmented MP4 as EOF after a compatible seek.
        // Feed encoded output through fixed complete fragments whenever the
        // browser accepts its exact SourceBuffer type. This also bounds the
        // amount of output produced ahead of playback; a browser without that
        // exact support retains the portable native MP4 fallback.
        mediaSourceType = encodedMediaSourceSupport;
        mediaSourceDelivery = true;
      }
      this.#store.dispatch({
        type: "PLAYBACK_AUX",
        sessionId,
        values: {
          streamNegotiation,
          nativeHlsDelivery,
          mediaSourceDelivery,
          outputQuality,
          ...(mediaSourceDelivery && !message
            ? {
              message: androidTranscodeEligible
                ? "Preparing reliable Android stream…"
                : "Preparing compatible stream…",
            }
            : {}),
        },
      });
    }
    const status = (next, values = {}) => {
      if (valid()) this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId, status: next, ...values });
    };
    const listen = (name, handler) => player.addEventListener(name, handler, { signal: controller.signal });
    listen("loadstart", () => status("loading"));
    listen("play", () => {
      if (!valid()) return;
      this.#restartSuspendedNativeHls(this.#store.getState().playback);
    });
    listen("waiting", () => {
      status("waiting", { message: this.#store.getState().playback.message || "Buffering…" });
      if (sourceMode === SOURCE_MODES.ORIGINAL
        && requestedMode === STREAM_MODES.AUTO
        && state.server.capabilities.transcoding
        && item.kind === "video") {
        this.#scheduleOriginalBufferRecovery({
          sessionId,
          item,
          start,
          signal: controller.signal,
        });
      }
    });
    listen("seeking", () => { if (sourceMode === SOURCE_MODES.ORIGINAL) status("seeking", { message: "Seeking…" }); });
    listen("seeked", () => {
      if (sourceMode !== SOURCE_MODES.ORIGINAL) return;
      this.#releaseHeldVideoFrame();
      status(player.paused ? "paused" : "playing", { message: null });
    });
    listen("loadedmetadata", () => {
      if (!valid()) return;
      if (sourceMode === SOURCE_MODES.ORIGINAL && start > 0) {
        try { player.currentTime = Math.min(start, Number.isFinite(player.duration) ? player.duration : start); } catch (_) { /* canplay retries naturally */ }
      } else if (sourceMode === SOURCE_MODES.COMPATIBLE && start > segmentOffset) {
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
    listen("loadeddata", () => {
      if (!valid()) return;
      if (sourceMode === SOURCE_MODES.COMPATIBLE) this.#clearStartupTimer();
      this.#releaseHeldVideoFrame();
    });
    listen("canplay", async () => {
      if (!valid()) return;
      if (sourceMode === SOURCE_MODES.COMPATIBLE) this.#clearStartupTimer();
      this.#releaseHeldVideoFrame();
      this.#startTrickplayPreload();
      if (sourceMode === SOURCE_MODES.COMPATIBLE
        && this.#canplayReportedSession !== sessionId) {
        this.#canplayReportedSession = sessionId;
        void this.#api.reportTranscodeCanPlay(
          item.id,
          sessionId,
          playbackSessionId,
          controller.signal,
        ).catch(() => {
          // Startup telemetry is best-effort and never changes playback.
        });
      }
      if (player.paused) {
        const playback = this.#store.getState().playback;
        status("paused", { autoplayBlocked: playback.autoplayBlocked, message: null });
        if (playback.intent === "playing") {
          await this.#attemptPlay(sessionId, player);
        }
      } else {
        // canplay can recur after a buffer interruption. Do not paint an
        // already-running element as paused merely because it recovered data.
        status("playing", { autoplayBlocked: false, intent: "playing", message: null });
      }
    });
    listen("playing", () => {
      if (!valid()) return;
      if (sourceMode === SOURCE_MODES.COMPATIBLE
        && this.#playingReportedSession !== sessionId) {
        this.#playingReportedSession = sessionId;
        void this.#api.reportTranscodeStartup(
          item.id,
          sessionId,
          playbackSessionId,
          "playing",
          controller.signal,
        ).catch(() => {
          // Startup telemetry is best-effort and never changes playback.
        });
      }
      if (document.visibilityState === "visible"
        && this.#nativeHlsSuspendedSession === sessionId) {
        this.#nativeHlsSuspendedSession = null;
      }
      this.#clearStartupTimer();
      this.#releaseHeldVideoFrame();
      this.#startTrickplayPreload();
      status("playing", { autoplayBlocked: false, intent: "playing", message: null });
    });
    listen("pause", () => {
      if (!valid() || player.ended) return;
      const playback = this.#store.getState().playback;
      if (!["loading", "waiting", "seeking", "error"].includes(playback.status)) {
        // Locking a device or backgrounding a browser pauses its media element
        // without changing what the user asked rustyDLNA to do. Preserve that
        // intent so a seek or suspended-source restart continues playback.
        status("paused", {
          autoplayBlocked: playback.autoplayBlocked,
          intent: document.visibilityState === "hidden" ? playback.intent : "paused",
        });
      }
      this.#progressWriter.flush();
    });
    listen("timeupdate", () => {
      if (!valid()) return;
      if (player.currentTime > 0) this.#resetAutomaticTranscodeRecovery();
      const global = sourceMode === SOURCE_MODES.COMPATIBLE ? segmentOffset + player.currentTime : player.currentTime;
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
      const playback = this.#store.getState().playback;
      const duration = playback.duration;
      const currentTime = this.globalTime();
      const endTolerance = Math.max(5, duration * 0.001);
      const prematureCompatibleEnd = sourceMode === SOURCE_MODES.COMPATIBLE
        && duration > 0
        && currentTime + endTolerance < duration;
      const portableCompatibleStream = streamNegotiation?.video === "transcode"
        && streamNegotiation?.audio === "transcode"
        && streamNegotiation?.videoOutput === "h264_sdr";
      if (prematureCompatibleEnd && !portableCompatibleStream) {
        if (mediaSourceDelivery && !mediaSourceRetry) {
          this.#loadSource(item, {
            start: currentTime,
            intent: playback.intent,
            forceSourceMode: SOURCE_MODES.COMPATIBLE,
            forceStreamNegotiation: streamNegotiation,
            forceQuality: outputQuality,
            forceAndroidMediaSource: mediaSourceDelivery,
            mediaSourceRetry: true,
            preservePreviousTranscode: true,
            message: "Reconnecting to the HEVC stream…",
            messageKind: "retry",
          });
          return;
        }
        const hdrEncodingFallback = mediaSourceDelivery
          && copiedHevcHdrEncodingFallbackType(
            state.server.capabilities,
            streamNegotiation,
          );
        this.#loadSource(item, {
          start: currentTime,
          intent: playback.intent,
          forceSourceMode: SOURCE_MODES.COMPATIBLE,
          forceStreamNegotiation: {
            ...streamNegotiation,
            video: "transcode",
            ...(hdrEncodingFallback ? {} : { audio: "transcode" }),
            videoOutput: hdrEncodingFallback ? "hevc_hdr10" : "h264_sdr",
          },
          forceQuality: outputQuality,
          forceAndroidMediaSource: Boolean(hdrEncodingFallback),
          message: hdrEncodingFallback
            ? "Re-encoding the stream to preserve HDR…"
            : "Continuing with portable playback…",
          messageKind: "fallback",
        });
        return;
      }
      this.#store.dispatch({ type: "PLAYBACK_TIME", sessionId, currentTime: duration, duration });
      status("ended", { intent: "paused", message: null });
      clearProgress(item.id);
      if (this.#store.getState().preferences.autoplay) this.playRelative(1);
    });
    listen("error", () => {
      if (!valid()) return;
      this.#invalidateWakeLockSession();
      this.#handleMediaError(
        sessionId,
        item,
        sourceMode,
        start,
        intent,
        streamNegotiation,
        null,
        mediaSourceRetry,
      );
    });

    const params = new URLSearchParams();
    let sourceUrl = item.source_url;
    if (sourceMode === SOURCE_MODES.COMPATIBLE) {
      params.set("mode", SOURCE_MODES.COMPATIBLE);
      params.set("audio", String(this.#store.getState().playback.selectedAudio));
      params.set("start", String(segmentOffset));
      params.set("quality", outputQuality);
      params.set("video_mode", streamNegotiation.video);
      if (streamNegotiation.video === "transcode") {
        params.set("video_output", streamNegotiation.videoOutput || "h264_sdr");
      }
      params.set("audio_mode", streamNegotiation.audio);
      params.set("reason", selected.reason);
      params.set("request", String(sessionId));
      params.set("session", String(playbackSessionId));
      if (nativeHlsDelivery) params.set("delivery", "hls");
      if (mediaSourceDelivery) params.set("delivery", "mse");
      sourceUrl = `${item.fallback_url}${item.fallback_url.includes("?") ? "&" : "?"}${params}`;
      if (nativeHlsDelivery || mediaSourceDelivery) sourceUrl = sourceUrl.replace(/\.mp4(?=\?)/, ".m3u8");
    } else {
      params.set("reason", selected.reason);
      params.set("request", String(sessionId));
      sourceUrl = `${item.source_url}${item.source_url.includes("?") ? "&" : "?"}${params}`;
    }
    this.#compatibleSourceReloads = 0;
    let playerSourceUrl = sourceUrl;
    if (mediaSourceDelivery) {
      playerSourceUrl = this.#startMediaSourceDelivery({
        player,
        playlistUrl: new URL(sourceUrl, window.location.href).href,
        contentType: mediaSourceType,
        sessionId,
        playbackSessionId,
        item,
        start,
        intent,
        streamNegotiation,
        mediaSourceRetry,
        signal: controller.signal,
        valid,
      });
    } else {
      player.src = sourceUrl;
      player.load();
    }
    if (sourceMode === SOURCE_MODES.COMPATIBLE) {
      this.#pollTranscodeState({
        item,
        sessionId,
        playbackSessionId,
        player,
        sourceUrl: playerSourceUrl,
        start,
        intent,
        streamNegotiation,
        nativeHlsDelivery,
        mediaSourceDelivery,
        signal: controller.signal,
      });
    } else if (requestedMode === STREAM_MODES.AUTO
      && state.server.capabilities.transcoding
      && item.kind === "video") {
      this.#scheduleOriginalBufferRecovery({
        sessionId,
        item,
        start,
        signal: controller.signal,
      });
    }
    // A title selection or resume begins in a user event handler. Ask the
    // browser to play while that activation is still available; Safari may
    // reject a first play delayed until canplay even though the user tapped a
    // Play control. The canplay listener retries only while intent remains
    // playing and the media element is still paused.
    if (intent === "playing") void this.#attemptPlay(sessionId, player);
  }

  #startMediaSourceDelivery({
    player,
    playlistUrl,
    contentType,
    sessionId,
    playbackSessionId,
    item,
    start,
    intent,
    streamNegotiation,
    mediaSourceRetry,
    signal,
    valid,
  }) {
    const mediaSource = new MediaSource();
    const objectUrl = URL.createObjectURL(mediaSource);
    this.#mediaSourceObjectUrl = objectUrl;
    player.disableRemotePlayback = true;
    player.src = objectUrl;
    player.load();
    const reportStartup = (event) => {
      void this.#api.reportTranscodeStartup(
        item.id,
        sessionId,
        playbackSessionId,
        event,
        signal,
      ).catch(() => {
        // Startup telemetry is best-effort and never changes playback.
      });
    };
    this.#pumpMediaSource({
      player,
      mediaSource,
      playlistUrl,
      contentType,
      signal,
      reportStartup,
    })
      .catch((error) => {
        if (signal.aborted || error?.name === "AbortError" || !valid()) return;
        // Media Source fetch/append failures need the same producer-status,
        // busy retry, codec fallback, and lower-quality recovery used by a
        // native media-element error. Treat a live producer's append failure
        // as a decode error; queued/cancelled producer state still wins.
        this.#handleMediaError(
          sessionId,
          item,
          SOURCE_MODES.COMPATIBLE,
          start,
          intent,
          streamNegotiation,
          error,
          mediaSourceRetry,
        ).catch((recoveryError) => {
          if (!valid()) return;
          this.#store.dispatch({
            type: "PLAYBACK_ERROR",
            sessionId,
            error: playbackError(
              "transcode_failed",
              recoveryError?.message || error?.message || "Media Source delivery failed",
            ),
          });
        });
      });
    return objectUrl;
  }

  async #pumpMediaSource({
    player,
    mediaSource,
    playlistUrl,
    contentType,
    signal,
    reportStartup,
  }) {
    await waitForMediaEvent(mediaSource, "sourceopen", signal, "sourceclose");
    if (signal.aborted || mediaSource.readyState !== "open") throw abortedError();
    const sourceBuffer = mediaSource.addSourceBuffer(contentType);
    sourceBuffer.mode = "segments";
    const appended = new Set();
    let initAppended = false;
    let playlistReported = false;

    while (!signal.aborted) {
      if (appended.size > 0 && player.paused) {
        await waitForMediaSourcePlayback(player, signal);
      }
      const requestUrl = new URL(playlistUrl);
      requestUrl.searchParams.set("mse_after", String(appended.size));
      const response = await fetch(requestUrl, {
        cache: "no-store",
        credentials: "same-origin",
        signal,
      });
      if (!response.ok) throw new Error(`Media Source playlist returned HTTP ${response.status}.`);
      const contentLength = Number(response.headers.get("content-length"));
      if (contentLength > 4 * 1024 * 1024) throw new Error("Media Source playlist is too large.");
      const playlist = parseHlsMediaPlaylist(await response.text(), requestUrl.href);
      if (!playlist) throw new Error("Media Source playlist is invalid.");
      if (!playlistReported) {
        playlistReported = true;
        reportStartup("mse_playlist_received");
      }

      if (!initAppended) {
        await this.#appendMediaSourceResource(
          sourceBuffer,
          playlist.initUrl,
          player,
          signal,
          {
            onFetched: () => reportStartup("mse_init_fetched"),
            onAppended: () => reportStartup("mse_init_appended"),
          },
        );
        initAppended = true;
      }

      let appendedNewSegment = false;
      for (const segmentUrl of playlist.segmentUrls) {
        if (appended.has(segmentUrl)) continue;
        // One complete fragment is enough to establish the SourceBuffer. Once
        // playback is paused, do not keep polling or downloading against a
        // stationary playback clock; the play event resumes this same pump.
        if (appended.size > 0 && player.paused) {
          await waitForMediaSourcePlayback(player, signal);
        }
        while (bufferedSecondsAhead(sourceBuffer, player.currentTime)
          >= MEDIA_SOURCE_BUFFER_AHEAD_SECONDS) {
          await abortableDelay(250, signal);
        }
        const firstFragment = appended.size === 0;
        await this.#appendMediaSourceResource(
          sourceBuffer,
          segmentUrl,
          player,
          signal,
          firstFragment ? {
            onFetched: () => reportStartup("mse_first_fragment_fetched"),
            onAppended: () => reportStartup("mse_first_fragment_appended"),
          } : undefined,
        );
        appended.add(segmentUrl);
        appendedNewSegment = true;
        await this.#pruneMediaSourceBuffer(sourceBuffer, player.currentTime, signal);
      }

      if (playlist.ended) {
        if (sourceBuffer.updating) await waitForMediaEvent(sourceBuffer, "updateend", signal);
        if (mediaSource.readyState === "open") mediaSource.endOfStream();
        return;
      }
      if (appended.size > 0 && player.paused) {
        await waitForMediaSourcePlayback(player, signal);
      }
      if (!appendedNewSegment) await abortableDelay(MEDIA_SOURCE_PLAYLIST_POLL_MS, signal);
    }
  }

  async #appendMediaSourceResource(sourceBuffer, url, player, signal, observers = {}) {
    const response = await fetch(url, {
      cache: "no-store",
      credentials: "same-origin",
      signal,
    });
    if (!response.ok) throw new Error(`Media Source fragment returned HTTP ${response.status}.`);
    const contentLength = Number(response.headers.get("content-length"));
    if (contentLength > 32 * 1024 * 1024) throw new Error("Media Source fragment is too large.");
    const bytes = await response.arrayBuffer();
    if (bytes.byteLength === 0 || bytes.byteLength > 32 * 1024 * 1024) {
      throw new Error("Media Source fragment has an invalid size.");
    }
    observers.onFetched?.();
    try {
      await sourceBufferOperation(sourceBuffer, () => sourceBuffer.appendBuffer(bytes), signal);
    } catch (error) {
      if (error?.name !== "QuotaExceededError") throw error;
      await this.#pruneMediaSourceBuffer(sourceBuffer, player.currentTime, signal, true);
      await sourceBufferOperation(sourceBuffer, () => sourceBuffer.appendBuffer(bytes), signal);
    }
    observers.onAppended?.();
  }

  async #pruneMediaSourceBuffer(sourceBuffer, currentTime, signal, required = false) {
    const removeEnd = Math.max(0, currentTime - MEDIA_SOURCE_RETAIN_BEHIND_SECONDS);
    if (!(removeEnd > 0) || sourceBuffer.buffered.length === 0) {
      if (required) throw new DOMException("Media Source buffer is full.", "QuotaExceededError");
      return;
    }
    const removeStart = sourceBuffer.buffered.start(0);
    if (!(removeEnd > removeStart)) {
      if (required) throw new DOMException("Media Source buffer is full.", "QuotaExceededError");
      return;
    }
    await sourceBufferOperation(
      sourceBuffer,
      () => sourceBuffer.remove(removeStart, removeEnd),
      signal,
    );
  }

  #rebindPiPSourceSession(item, sessionId) {
    if (item.kind !== "video") {
      this.#pipRequestSession = null;
      this.#pipActiveSession = null;
      return false;
    }
    if (this.#pipRequestSession !== null) this.#pipRequestSession = sessionId;
    const active = document.pictureInPictureElement === this.#dom.video;
    this.#pipActiveSession = active ? sessionId : null;
    return active;
  }

  #decodingInfo(configuration) {
    const key = JSON.stringify(configuration);
    if (this.#capabilityCache.has(key)) {
      const cached = this.#capabilityCache.get(key);
      // Refresh insertion order so the bounded map behaves as a small LRU.
      this.#capabilityCache.delete(key);
      this.#capabilityCache.set(key, cached);
      return Promise.resolve(cached);
    }
    // Cache only completed probes. A browser promise that never settles is
    // timed out by negotiation and must not poison every later title with the
    // same codec configuration.
    return navigator.mediaCapabilities.decodingInfo(configuration).then((result) => {
      while (this.#capabilityCache.size >= MAX_MEDIA_CAPABILITY_CACHE_ENTRIES) {
        this.#capabilityCache.delete(this.#capabilityCache.keys().next().value);
      }
      this.#capabilityCache.set(key, result);
      return result;
    });
  }

  #pollTranscodeState({
    item,
    sessionId,
    playbackSessionId,
    player,
    sourceUrl,
    start,
    intent,
    streamNegotiation,
    nativeHlsDelivery,
    mediaSourceDelivery,
    signal,
  }) {
    const poll = async () => {
      const current = this.#store.getState().playback;
      if (signal.aborted || sessionId !== current.sessionId
        || ["ended", "error"].includes(current.status)) return;
      const preparing = ["loading", "waiting", "seeking"].includes(current.status);
      try {
        const payload = await this.#api.transcodeStatus(
          item.id,
          sessionId,
          playbackSessionId,
          signal,
        );
        const message = {
          queued: "Waiting for a transcode slot…",
          starting: "Starting compatible playback…",
          producing: "Preparing video…",
        }[payload.state];
        const stillPreparing = ["loading", "waiting", "seeking"]
          .includes(this.#store.getState().playback.status);
        if (message && stillPreparing) {
          this.#store.dispatch({ type: "PLAYBACK_STATUS", sessionId, status: "loading", message });
        } else if (payload.state === "failed") {
          this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("transcode_failed", "The transcode producer failed.") });
          return;
        } else if (payload.state === "cancelled") {
          const playback = this.#store.getState().playback;
          if (this.#scheduleCompatibleRetry({
            sessionId,
            item: playback.item,
            start: playback.currentTime,
            intent: playback.intent,
            streamNegotiation: playback.streamNegotiation,
            retryAfterSeconds: payload.retry_after_seconds,
          })) return;
          this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("transcode_cancelled") });
          return;
        }
        if (stillPreparing
          && item.kind === "video"
          && !mediaSourceDelivery
          && ["producing", "ready"].includes(payload.state)) {
          this.#scheduleCompatibleStartupRecovery({
            sessionId,
            item,
            player,
            sourceUrl,
            start,
            streamNegotiation,
            nativeHlsDelivery,
            mediaSourceDelivery,
            signal,
          });
        }
      } catch (error) {
        if (error?.name === "AbortError") return;
        const category = apiErrorCategory(error);
        // Once decoded playback is underway, the media element remains the
        // authority for a connection failure. A missed lease heartbeat alone
        // must not interrupt buffered playback.
        if (preparing && ["media_missing", "transcode_busy", "transcode_failed", "transcode_cancelled", "offline", "network"].includes(category)) {
          this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError(category, error?.technical || "") });
          return;
        }
      }
      const latest = this.#store.getState().playback;
      if (signal.aborted || sessionId !== latest.sessionId
        || ["ended", "error"].includes(latest.status)) return;
      const delay = ["loading", "waiting", "seeking"].includes(latest.status)
        ? TRANSCODE_PREPARING_POLL_MS
        : TRANSCODE_ACTIVE_POLL_MS;
      this.#statusTimer = window.setTimeout(poll, delay);
    };
    poll();
  }

  #scheduleCompatibleStartupRecovery({
    sessionId,
    item,
    player,
    sourceUrl,
    start,
    streamNegotiation,
    nativeHlsDelivery,
    mediaSourceDelivery,
    signal,
  }) {
    if (this.#startupTimer !== null || signal.aborted || player.readyState >= 2) return;
    this.#startupTimer = window.setTimeout(() => {
      this.#startupTimer = null;
      const playback = this.#store.getState().playback;
      if (signal.aborted
        || sessionId !== playback.sessionId
        || !["loading", "waiting", "seeking"].includes(playback.status)
        || player.readyState >= 2
        || player.getAttribute("src") !== sourceUrl) return;
      if (!mediaSourceDelivery
        && item.kind === "video"
        && streamNegotiation?.video === "transcode"
        && androidMediaSourceType()) {
        this.#loadSource(item, {
          start: playback.currentTime || start,
          intent: playback.intent,
          forceSourceMode: SOURCE_MODES.COMPATIBLE,
          forceStreamNegotiation: streamNegotiation,
          forceQuality: playback.outputQuality,
          forceAndroidMediaSource: true,
          message: "Switching to reliable Android playback…",
          messageKind: "fallback",
        });
        return;
      }
      if (this.#compatibleSourceReloads >= MAX_COMPATIBLE_SOURCE_RELOADS) {
        this.#scheduleCompatibleRetry({
          sessionId,
          item,
          start: playback.currentTime || start,
          intent: playback.intent,
          streamNegotiation,
        });
        return;
      }
      this.#compatibleSourceReloads += 1;
      this.#store.dispatch({
        type: "PLAYBACK_STATUS",
        sessionId,
        status: "loading",
        intent: playback.intent,
        message: "Reconnecting to the prepared video…",
      });
      // Mobile Chromium can leave a growing fMP4 reader attached but undecoded,
      // while Safari can retain a native HLS source whose generation expired
      // during a long pause without fetching the replacement fragments. Reopen
      // the same generation first; its accumulated bytes remain reusable. A
      // second stall advances to the bounded fresh-generation retry above.
      player.load();
    }, nativeHlsDelivery ? NATIVE_HLS_STARTUP_STALL_MS : COMPATIBLE_STARTUP_STALL_MS);
  }

  #scheduleOriginalBufferRecovery({ sessionId, item, start, signal }) {
    if (this.#startupTimer !== null || signal.aborted) return;
    this.#startupTimer = window.setTimeout(() => {
      this.#startupTimer = null;
      const state = this.#store.getState();
      const { playback, preferences, server } = state;
      if (signal.aborted
        || sessionId !== playback.sessionId
        || playback.sourceMode !== SOURCE_MODES.ORIGINAL
        || preferences.streamMode !== STREAM_MODES.AUTO
        || !server.capabilities.transcoding
        || playback.intent !== "playing"
        || !["loading", "waiting", "seeking"].includes(playback.status)) return;
      const fallbackQuality = saferCompatibleQualityProfile(
        server.capabilities.quality_profiles,
        preferences.quality,
      ) || preferences.quality;
      this.#loadSource(item, {
        start: playback.currentTime || this.globalTime() || start,
        intent: playback.intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceQuality: fallbackQuality,
        message: "Switching to lower-bandwidth compatible playback…",
        messageKind: "fallback",
      });
    }, ORIGINAL_BUFFER_STALL_MS);
  }

  #clearStartupTimer() {
    if (this.#startupTimer !== null) window.clearTimeout(this.#startupTimer);
    this.#startupTimer = null;
  }

  #markNativeHlsSuspended() {
    const { playback } = this.#store.getState();
    if (playback.nativeHlsDelivery && playback.item?.kind === "video") {
      this.#nativeHlsSuspendedSession = playback.sessionId;
    }
  }

  #restartSuspendedNativeHls(playback) {
    if (document.visibilityState !== "visible"
      || this.#nativeHlsSuspendedSession !== playback.sessionId
      || !playback.nativeHlsDelivery
      || playback.sourceMode !== SOURCE_MODES.COMPATIBLE
      || !playback.item) return false;
    this.#nativeHlsSuspendedSession = null;
    this.#resetAutomaticTranscodeRecovery();
    this.#loadSource(playback.item, {
      start: playback.currentTime,
      intent: "playing",
      forceSourceMode: SOURCE_MODES.COMPATIBLE,
      forceStreamNegotiation: playback.streamNegotiation,
      forceQuality: playback.outputQuality,
      message: "Resuming Safari playback…",
      messageKind: "resume",
    });
    return true;
  }

  #resetAutomaticTranscodeRecovery() {
    this.#automaticTranscodeRetries = 0;
    this.#pendingCompatibleRetrySession = null;
    this.#transcodeBusyStartedAt = null;
    this.#transcodeBusyRetries = 0;
  }

  #scheduleCompatibleRetry({
    sessionId,
    item,
    start,
    intent,
    streamNegotiation,
    retryAfterSeconds = 1,
    busy = false,
  }) {
    if (!item || sessionId !== this.#store.getState().playback.sessionId) return false;
    // A media element can report the same failed source more than once while
    // its producer-status request is in flight. Count and schedule that source
    // only once; otherwise a duplicate callback can consume another retry and
    // cancel the first callback's timer without loading a new generation.
    if (this.#pendingCompatibleRetrySession === sessionId) return true;
    if (busy) {
      const now = Date.now();
      this.#transcodeBusyStartedAt ??= now;
      if (now - this.#transcodeBusyStartedAt >= TRANSCODE_BUSY_RETRY_WINDOW_MS) return false;
      this.#transcodeBusyRetries += 1;
    } else {
      this.#transcodeBusyStartedAt = null;
      this.#transcodeBusyRetries = 0;
      if (this.#automaticTranscodeRetries >= MAX_AUTOMATIC_TRANSCODE_RETRIES) return false;
      this.#automaticTranscodeRetries += 1;
    }
    const target = this.#store.getState().playback.currentTime || start;
    const playback = this.#store.getState().playback;
    const outputQuality = playback.outputQuality;
    const forceAndroidMediaSource = playback.mediaSourceDelivery;
    const requestedDelay = Math.max(250, Number(retryAfterSeconds || 1) * 1_000);
    const delay = busy
      ? Math.min(5_000, Math.max(requestedDelay, 250 * (2 ** Math.min(4, this.#transcodeBusyRetries - 1))))
      : Math.min(2_000, requestedDelay);
    this.#pendingCompatibleRetrySession = sessionId;
    this.#cancelSource({ keepElement: false });
    this.#store.dispatch({
      type: "PLAYBACK_STATUS",
      sessionId,
      status: "loading",
      intent,
      message: busy
        ? "Waiting for a transcode slot…"
        : "The previous stream was abandoned. Retrying compatible playback…",
      error: null,
    });
    this.#statusTimer = window.setTimeout(() => {
      this.#statusTimer = null;
      if (this.#pendingCompatibleRetrySession === sessionId) {
        this.#pendingCompatibleRetrySession = null;
      }
      const latest = this.#store.getState().playback;
      if (sessionId !== latest.sessionId) return;
      this.#loadSource(item, {
        start: target,
        intent: latest.intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: streamNegotiation,
        forceQuality: outputQuality,
        forceAndroidMediaSource,
        message: busy ? "Waiting for a transcode slot…" : "Retrying compatible playback…",
        messageKind: busy ? "queue" : "retry",
      });
    }, delay);
    return true;
  }

  async #attemptPlay(sessionId, player) {
    try {
      await player.play();
    } catch (error) {
      if (sessionId !== this.#store.getState().playback.sessionId) return;
      if (error?.name === "NotAllowedError") {
        this.#store.dispatch({
          type: "PLAYBACK_STATUS",
          sessionId,
          status: "paused",
          intent: "paused",
          autoplayBlocked: true,
          message: null,
        });
      } else if (error?.name !== "AbortError") {
        this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("unknown", error?.message || "play() failed") });
      }
    }
  }

  async #handleMediaError(
    sessionId,
    item,
    sourceMode,
    start,
    intent,
    streamNegotiation,
    deliveryError = null,
    mediaSourceRetry = false,
  ) {
    if (sessionId !== this.#store.getState().playback.sessionId) return;
    const preferences = this.#store.getState().preferences;
    const capabilities = this.#store.getState().server.capabilities;
    const outputQuality = this.#store.getState().playback.outputQuality || preferences.quality;
    const mediaCode = this.activePlayer().error?.code || (deliveryError ? 3 : undefined);
    if (sourceMode === SOURCE_MODES.ORIGINAL
      && preferences.streamMode === STREAM_MODES.AUTO
      && capabilities.transcoding) {
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        message: "Switching to compatible playback…",
        messageKind: "fallback",
      });
      return;
    }
    const sourceReason = this.#store.getState().playback.sourceReason;
    let code = sourceMode === SOURCE_MODES.ORIGINAL
      ? (!capabilities.transcoding && sourceReason === "transcoding_disabled" ? "transcode_disabled" : "unsupported_direct")
      : "transcode_failed";
    let producerState = null;
    // navigator.onLine is only a hint and WebKit can leave it false after an
    // intentionally aborted request. Ask the typed API for the real cause.
    if (sourceMode === SOURCE_MODES.COMPATIBLE) {
      try {
        const payload = await this.#api.transcodeStatus(
          item.id,
          sessionId,
          this.#playbackSession,
        );
        if (sessionId !== this.#store.getState().playback.sessionId) return;
        producerState = payload.state;
        if (["queued", "cancelled", "idle"].includes(payload.state)) {
          const retryableProducerState = payload.state === "queued" || mediaCode !== 4;
          if (retryableProducerState && this.#scheduleCompatibleRetry({
            sessionId,
            item,
            start,
            intent,
            streamNegotiation,
            retryAfterSeconds: payload.retry_after_seconds,
            busy: payload.state === "queued",
          })) return;
          code = payload.state === "queued" ? "transcode_busy" : "transcode_cancelled";
        } else if (payload.state === "failed") code = "transcode_failed";
      } catch (error) {
        if (sessionId !== this.#store.getState().playback.sessionId) return;
        const category = apiErrorCategory(error);
        if (category !== "unknown") code = category;
      }
    } else if (sourceMode === SOURCE_MODES.ORIGINAL) {
      try {
        await this.#api.item(item.id);
      } catch (error) {
        if (sessionId !== this.#store.getState().playback.sessionId) return;
        const category = apiErrorCategory(error);
        if (["media_missing", "offline", "network"].includes(category)) code = category;
      }
    }
    const mediaSourceDelivery = this.#store.getState().playback.mediaSourceDelivery;
    const retryableHevcMediaSource = sourceMode === SOURCE_MODES.COMPATIBLE
      && mediaSourceDelivery
      && !mediaSourceRetry
      && [3, 4].includes(mediaCode)
      && ["producing", "ready"].includes(producerState)
      && (streamNegotiation?.video === "copy"
        || streamNegotiation?.videoOutput === "hevc_hdr10");
    if (retryableHevcMediaSource) {
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: streamNegotiation,
        forceQuality: outputQuality,
        forceAndroidMediaSource: mediaSourceDelivery,
        mediaSourceRetry: true,
        preservePreviousTranscode: true,
        message: "Reconnecting to the HEVC stream…",
        messageKind: "retry",
      });
      return;
    }
    const copiedHevcHdrEncodingFallback = sourceMode === SOURCE_MODES.COMPATIBLE
      && mediaSourceDelivery
      && [3, 4].includes(mediaCode)
      && copiedHevcHdrEncodingFallbackType(capabilities, streamNegotiation);
    if (copiedHevcHdrEncodingFallback) {
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: {
          ...streamNegotiation,
          video: "transcode",
          videoOutput: "hevc_hdr10",
        },
        forceQuality: outputQuality,
        forceAndroidMediaSource: true,
        message: "Re-encoding the stream to preserve HDR…",
        messageKind: "fallback",
      });
      return;
    }
    if (sourceMode === SOURCE_MODES.COMPATIBLE
      && [3, 4].includes(mediaCode)
      && streamNegotiation?.videoOutput === "hevc_hdr10") {
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: {
          ...streamNegotiation,
          video: "transcode",
          audio: "transcode",
          videoOutput: "h264_sdr",
        },
        forceQuality: outputQuality,
        forceAndroidMediaSource: mediaSourceDelivery,
        message: "Switching to SDR-compatible playback…",
        messageKind: "fallback",
      });
      return;
    }
    if (sourceMode === SOURCE_MODES.COMPATIBLE
      && [3, 4].includes(mediaCode)
      && !mediaSourceDelivery
      && item.kind === "video"
      && streamNegotiation?.video === "transcode"
      && androidMediaSourceType()) {
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: streamNegotiation,
        forceQuality: outputQuality,
        forceAndroidMediaSource: true,
        message: "Switching to reliable Android playback…",
        messageKind: "fallback",
      });
      return;
    }
    // Capability APIs are advisory. If copied media or an HEVC repair selected
    // from those APIs reaches a live producer but fails decoding or is rejected
    // as an unsupported source, retry once with portable H.264/AAC before
    // presenting a terminal MediaError.
    const advisoryHevcRepair = streamNegotiation?.video === "repair"
      && String(item?.repair_video_encoder || "").toLowerCase() === "hevc_nvenc";
    if (sourceMode === SOURCE_MODES.COMPATIBLE
      && [3, 4].includes(mediaCode)
      && (streamNegotiation?.video === "copy"
        || streamNegotiation?.audio === "copy"
        || advisoryHevcRepair)) {
      const portableNegotiation = {
        ...streamNegotiation,
        video: "transcode",
        audio: "transcode",
        videoOutput: "h264_sdr",
      };
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: portableNegotiation,
        forceQuality: outputQuality,
        message: "Trying portable compatible playback…",
        messageKind: "fallback",
      });
      return;
    }
    const portableVideo = streamNegotiation?.video === "transcode"
      || (streamNegotiation?.video === "repair"
        && ["libx264", "h264_nvenc"].includes(String(item?.repair_video_encoder || "").toLowerCase()));
    const saferQuality = sourceMode === SOURCE_MODES.COMPATIBLE
      && [3, 4].includes(mediaCode)
      && portableVideo
      && streamNegotiation?.audio === "transcode"
      ? automaticCompatibleRecoveryProfile(
        capabilities.quality_profiles,
        outputQuality,
        preferences.quality,
      )
      : null;
    if (saferQuality) {
      this.#loadSource(item, {
        start: this.globalTime() || start,
        intent,
        forceSourceMode: SOURCE_MODES.COMPATIBLE,
        forceStreamNegotiation: streamNegotiation,
        forceQuality: saferQuality,
        message: "Lowering compatible quality for this device…",
        messageKind: "fallback",
      });
      return;
    }
    // A browser can terminate its media connection even though the server is
    // still producing valid fragments (for example after a socket backpressure
    // timeout). Replace that generation automatically instead of presenting a
    // terminal transcode error. #loadSource assigns the retry a newer
    // generation, which also cancels the abandoned producer server-side.
    if (sourceMode === SOURCE_MODES.COMPATIBLE
      && mediaCode !== 4
      && ["producing", "ready"].includes(producerState)
      && this.#scheduleCompatibleRetry({
        sessionId,
        item,
        start: this.globalTime() || start,
        intent,
        streamNegotiation,
      })) return;
    if (sessionId !== this.#store.getState().playback.sessionId) return;
    const technical = deliveryError?.message
      || (mediaCode ? `MediaError code ${mediaCode}` : "Media element error");
    this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError(code, technical) });
  }

  async #prepareSelection(item, signal) {
    if (item.stream_metadata_complete) return { item, error: null };
    try {
      const payload = await this.#api.item(item.id, { enrich: true, signal });
      const tracks = payload.audio_tracks || item.audio_tracks || [];
      return {
        item: {
          ...item,
          ...(payload.item || {}),
          audio_tracks: tracks,
          chapters: payload.chapters || item.chapters || [],
          stream_metadata_complete: true,
        },
        error: null,
      };
    } catch (error) {
      if (error?.name === "AbortError") return { item, error: null };
      return { item, error };
    }
  }

  async #enrichAudioTracks() {
    const state = this.#store.getState();
    const { item, sessionId, audioTracks } = state.playback;
    const current = () => {
      const playback = this.#store.getState().playback;
      return playback.sessionId === sessionId && String(playback.item?.id) === String(item?.id);
    };
    if (!item) return null;
    if (state.playback.audioTracksStatus === "loading") return null;
    if (state.playback.audioTracksStatus === "ready") return item;
    if (item.stream_metadata_complete) {
      this.#store.dispatch({ type: "AUDIO_TRACKS_SUCCESS", sessionId, item, tracks: audioTracks, chapters: item.chapters || [] });
      return item;
    }
    this.#store.dispatch({ type: "AUDIO_TRACKS_LOADING", sessionId });
    try {
      const payload = await this.#api.item(item.id, { enrich: true });
      if (!current()) return null;
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
      if (!current()) return null;
      if (error?.name === "AbortError") return null;
      this.#store.dispatch({ type: "AUDIO_TRACKS_ERROR", sessionId, error });
      return item;
    }
  }

  #selectAudioTrack(index) {
    const state = this.#store.getState();
    const { playback } = state;
    if (!playback.item || index === playback.selectedAudio || !state.server.capabilities.transcoding) return;
    this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId: playback.sessionId, values: { selectedAudio: index } });
    const start = this.globalTime();
    const intent = this.activePlayer().paused ? "paused" : "playing";
    this.#loadSource(playback.item, {
      start,
      intent,
      forceSourceMode: SOURCE_MODES.COMPATIBLE,
      forceAndroidMediaSource: playback.mediaSourceDelivery,
      message: "Changing audio track…",
      messageKind: "audio",
    });
  }

  #selectCaption(value) {
    const sessionId = this.#store.getState().playback.sessionId;
    this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { selectedCaption: value } });
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
    const { playback, server } = this.#store.getState();
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
    const switchingUnavailable = playback.audioTracks.length > 1 && !server.capabilities.transcoding;
    this.#dom.audioTrackControls.disabled = playback.audioTracksStatus === "loading" || switchingUnavailable;
    this.#dom.audioTrackRetry.hidden = playback.audioTracksStatus !== "error";
    this.#dom.audioTrackStatus.hidden = !switchingUnavailable
      && !["loading", "error"].includes(playback.audioTracksStatus);
    this.#dom.audioTrackStatus.textContent = playback.audioTracksStatus === "loading"
      ? "Loading audio tracks…"
      : playback.audioTracksStatus === "error"
        ? "Audio-track details are unavailable."
        : switchingUnavailable ? "Audio-track switching requires Compatible playback, which is disabled." : "";
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
    const advertisedProfiles = (state.server.capabilities.quality_profiles || [])
      .filter((profile) => validQualityProfileId(profile?.id));
    const aiUpscale = state.server.capabilities.ai_upscale;
    const profiles = sourceApplicableQualityProfiles(
      advertisedProfiles,
      state.playback.item,
      aiUpscale,
    );
    const selectedId = sourceBoundedQualityProfile(
      advertisedProfiles,
      state.preferences.quality,
      state.playback.item,
      aiUpscale,
    );
    const profileLabel = (profile) => sourceAwareQualityProfileLabel(
      advertisedProfiles,
      profile,
      state.playback.item,
      aiUpscale,
    );
    const key = profiles.map((profile) => `${profile.id}:${profileLabel(profile)}`).join("|");
    if (this.#dom.qualityControl.dataset.profiles !== key) {
      this.#dom.qualityControl.dataset.profiles = key;
      this.#dom.qualityControl.replaceChildren();
      this.#dom.qualityChoices.replaceChildren();
      for (const profile of profiles) {
        const option = document.createElement("option");
        option.value = profile.id;
        option.textContent = profileLabel(profile);
        this.#dom.qualityControl.append(option);
        const label = document.createElement("label");
        const radio = document.createElement("input");
        radio.type = "radio";
        radio.name = "quality-choice";
        radio.value = profile.id;
        label.append(radio, document.createTextNode(profileLabel(profile)));
        this.#dom.qualityChoices.append(label);
      }
    }
    this.#dom.qualityControl.value = selectedId;
    const disabled = !state.server.capabilities.transcoding;
    const selected = profiles.find((profile) => profile.id === selectedId);
    const selectedLabel = selected ? profileLabel(selected) : "Auto";
    const shortLabel = selectedLabel.split(" · ")[0];
    this.#dom.qualityControl.disabled = disabled;
    for (const radio of this.#dom.qualityChoices.querySelectorAll('input[name="quality-choice"]')) {
      radio.checked = radio.value === selectedId;
      radio.disabled = disabled;
    }
    this.#dom.qualityMenuButton.disabled = disabled;
    this.#dom.qualityMenuButton.textContent = shortLabel;
    this.#dom.qualityMenuButton.setAttribute("aria-label", `Transcoded quality: ${selectedLabel}`);
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

    if (playback.sourceMode !== SOURCE_MODES.COMPATIBLE) {
      this.#dom.streamInfoSummary.textContent = "The browser is consuming the original file directly; the server is not transcoding it.";
      replaceFacts(this.#dom.outputStreamFacts, [
        ["Container", `${containerLabel(item)} · unchanged`],
        ["Video", item.kind === "video" ? `${sourceVideo} · unchanged` : "None"],
        ["Audio", `${sourceAudio} · unchanged`],
      ]);
      return;
    }

    const profile = (server.capabilities.quality_profiles || []).find((entry) => entry.id === (playback.outputQuality || preferences.quality))
      || (server.capabilities.quality_profiles || [])[0];
    const sourceBoundedPreference = sourceBoundedQualityProfile(
      server.capabilities.quality_profiles || [],
      preferences.quality,
      item,
      server.capabilities.ai_upscale,
    );
    const profileLabel = profile
      ? sourceAwareQualityProfileLabel(
        server.capabilities.quality_profiles || [],
        profile,
        item,
        server.capabilities.ai_upscale,
      )
      : "";
    const negotiation = playback.streamNegotiation;
    if (!negotiation) {
      this.#dom.streamInfoSummary.textContent = "The browser is checking the source video and audio codecs independently.";
      replaceFacts(this.#dom.outputStreamFacts, [
        ["Container", "Fragmented MP4"],
        ["Delivery", playback.nativeHlsDelivery
          ? "Native HLS · fragmented MP4"
          : playback.mediaSourceDelivery ? "Media Source · fragmented MP4" : "Native media loading"],
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
    const transcodesHdr10 = negotiation.video === "transcode"
      && negotiation.videoOutput === "hevc_hdr10";
    const usesAiUpscale = profile
      ? aiUpscaleQualityAvailable(server.capabilities.ai_upscale, item, profile)
      : false;
    const outputDimensions = profile
      ? compatibleVideoDimensions(item, profile, server.capabilities.ai_upscale)
      : null;
    const resolution = outputDimensions
      ? `no larger than ${outputDimensions.width}×${outputDimensions.height}${usesAiUpscale ? " (AI upscaled)" : " (never upscaled)"}`
      : "source resolution (never upscaled)";
    const encodedVideo = (repairHevc || transcodesHdr10) && profile
      ? [
        "HEVC (hevc_nvenc)",
        "Main 10 profile",
        "Level 5.1",
        "p010le",
        `${resolution} at ${profile.max_fps} fps`,
        `${profile.max_video_kbps} kbps maximum`,
        repairPreservesHdr ? "HDR10 preserved" : transcodesHdr10 ? "HDR10 output" : "",
      ].filter(Boolean).join(" · ")
      : profile
      ? [
        `H.264 (${item.compatible_video_encoder || "server encoder"})`,
        `${profile.h264_profile} profile`,
        `Level ${profile.h264_level}`,
        profile.pixel_format,
        `${resolution} at ${profile.max_fps} fps`,
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
          : usesAiUpscale
            ? "The server is AI-upscaling the SDR video and producing browser-compatible H.264 video with AAC audio."
            : transcodesHdr10
              ? "The server is producing browser-compatible HEVC Main 10 HDR10 video and AAC audio."
              : "The server is producing browser-compatible H.264 video and AAC audio in SDR.";
    replaceFacts(this.#dom.outputStreamFacts, [
      ["Container", "Fragmented MP4"],
      ["Delivery", playback.nativeHlsDelivery
        ? "Native HLS · fragmented MP4"
        : playback.mediaSourceDelivery ? "Media Source · fragmented MP4" : "Native media loading"],
      ["Quality", profile ? `${profileLabel}${playback.outputQuality !== preferences.quality
        ? playback.outputQuality === sourceBoundedPreference ? " (source resolution)" : " (automatic recovery)"
        : ""}` : ""],
      ["Video", item.kind === "video" ? (copiesVideo
        ? `${sourceVideo} · copied unchanged (no video re-encode)`
        : repairsVideo ? `${sourceVideo} → ${encodedVideo} · frame order repaired` : encodedVideo) : "None"],
      ["Audio", copiesAudio ? `${sourceAudio} · copied unchanged (no audio re-encode)` : `${sourceAudio} → ${outputAudio}`],
      ["Browser video probe", item.kind === "video" ? capabilityProbeLabel(negotiation.videoContentType, negotiation.videoProbe) : "Not applicable"],
      ["Browser output probe", transcodesHdr10
        ? capabilityProbeLabel(negotiation.outputVideoContentType, negotiation.outputVideoProbe)
        : "Portable H.264 SDR"],
      ["Browser display range", transcodesHdr10
        ? negotiation.hdrDisplay === true
          ? "High dynamic range reported"
          : negotiation.hdrDisplay === false
            ? "Standard range reported · browser tone mapping may apply"
            : "Not reported · playback result is authoritative"
        : "Not used for SDR output"],
      ["Browser audio probe", capabilityProbeLabel(negotiation.audioContentType, negotiation.audioProbe)],
    ]);
  }

  #renderMessage() {
    const { playback, server } = this.#store.getState();
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
    const compatibleAvailable = server.capabilities.transcoding;
    const retryNeedsCompatible = playback.sourceMode === SOURCE_MODES.COMPATIBLE;
    const unavailableCompatibleRecovery = actions.includes("try_compatible") && !compatibleAvailable;
    this.#dom.playerRetry.hidden = !actions.includes("retry")
      || (retryNeedsCompatible && !compatibleAvailable);
    this.#dom.tryCompatible.hidden = !actions.includes("try_compatible") || !compatibleAvailable;
    this.#dom.playOriginal.hidden = !actions.includes("play_original");
    this.#dom.returnLibrary.hidden = !actions.includes("return_to_library")
      && !unavailableCompatibleRecovery;
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
    this.#dom.timeline.addEventListener("input", () => {
      const target = Number(this.#dom.timeline.value);
      const sessionId = this.#store.getState().playback.sessionId;
      this.#store.dispatch({ type: "PLAYBACK_PREVIEW", sessionId, value: target });
      this.#showTrickplayFrame(target);
    });
    this.#dom.timeline.addEventListener("change", () => {
      const target = Number(this.#dom.timeline.value);
      const sessionId = this.#store.getState().playback.sessionId;
      this.#store.dispatch({ type: "PLAYBACK_PREVIEW", sessionId, value: null });
      if (this.#trickplayTarget) this.#trickplayTarget.committed = true;
      this.seekTo(target);
    });
    this.#dom.timeline.addEventListener("blur", () => {
      const sessionId = this.#store.getState().playback.sessionId;
      this.#store.dispatch({ type: "PLAYBACK_PREVIEW", sessionId, value: null });
      if (this.#trickplayTarget && !this.#trickplayTarget.committed) {
        this.#releaseHeldVideoFrame();
      }
    });
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
      const sessionId = this.#store.getState().playback.sessionId;
      const requestToken = ++this.#pipRequestToken;
      try {
        if (document.pictureInPictureElement) await document.exitPictureInPicture();
        else {
          this.#pipRequestSession = sessionId;
          await this.#dom.video.requestPictureInPicture();
          if (this.#pipRequestToken === requestToken
            && this.#pipRequestSession !== null
            && document.pictureInPictureElement !== this.#dom.video) {
            this.#pipRequestSession = null;
          }
        }
      } catch (error) {
        if (this.#pipRequestToken === requestToken) this.#pipRequestSession = null;
        if (sessionId !== this.#store.getState().playback.sessionId) return;
        this.#store.dispatch({ type: "PLAYBACK_ERROR", sessionId, error: playbackError("unknown", error?.message || "PiP request failed") });
      }
    });
    this.#dom.video.addEventListener("enterpictureinpicture", () => {
      const sessionId = this.#pipRequestSession;
      this.#pipRequestSession = null;
      if (sessionId === null) return;
      this.#pipActiveSession = sessionId;
      this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { pip: true } });
    });
    this.#dom.video.addEventListener("leavepictureinpicture", () => {
      const sessionId = this.#pipActiveSession;
      this.#pipActiveSession = null;
      if (sessionId === null) return;
      this.#store.dispatch({ type: "PLAYBACK_AUX", sessionId, values: { pip: false } });
    });
    this.#dom.fullscreenButton.addEventListener("click", (event) => {
      this.toggleFullscreen({ pointerActivated: event.detail > 0 });
      // Pointer activation leaves the button focused in desktop browsers. That
      // focus is incidental and must not pin the controls open indefinitely;
      // keyboard activation keeps focus so the controls remain accessible.
      if (event.detail > 0) event.currentTarget.blur();
    });
    const syncStageFullscreen = () => {
      if (currentFullscreenElement() === this.#dom.playerStage) {
        this.#updateDisplayViewport();
        this.#fullscreenEntered("stage");
      } else {
        if (!this.#dom.playerStage.classList.contains("expanded-player")) {
          this.#clearDisplayViewport();
        }
        this.#fullscreenExited("stage");
      }
    };
    document.addEventListener("fullscreenchange", syncStageFullscreen);
    document.addEventListener("webkitfullscreenchange", syncStageFullscreen);
    this.#dom.video.addEventListener("webkitbeginfullscreen", () => this.#fullscreenEntered("native_video"));
    this.#dom.video.addEventListener("webkitendfullscreen", () => this.#fullscreenExited("native_video"));
    this.#dom.video.addEventListener("webkitpresentationmodechanged", () => {
      if (nativeVideoFullscreenActive(this.#dom.video)) this.#fullscreenEntered("native_video");
      else this.#fullscreenExited("native_video");
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
      if (enriched && playback.sourceMode === SOURCE_MODES.COMPATIBLE) {
        this.#loadSource(enriched, {
          start: this.globalTime(),
          intent: this.activePlayer().paused ? "paused" : "playing",
          forceSourceMode: SOURCE_MODES.COMPATIBLE,
          message: "Applying stream details…",
        });
      }
    });
    this.#dom.chapterControls.addEventListener("change", () => this.seekTo(Number(this.#dom.chapterControls.value)));
    this.#dom.advancedPlaybackButton.addEventListener("click", () => this.#dom.advancedPlaybackDialog.showModal());
    this.#dom.qualityMenuButton.addEventListener("click", () => {
      this.#dom.qualityDialog.showModal();
      this.#dom.qualityChoices.querySelector('input[name="quality-choice"]:checked')?.focus();
    });
    this.#dom.streamControls.addEventListener("change", (event) => {
      if (!(event.target instanceof HTMLInputElement)) return;
      this.#setPreference("streamMode", event.target.value, "stream");
      const playback = this.#store.getState().playback;
      if (playback.item) this.#loadSource(playback.item, { start: this.globalTime(), intent: this.activePlayer().paused ? "paused" : "playing" });
    });
    this.#dom.qualityControl.addEventListener("change", () => this.#selectQuality(this.#dom.qualityControl.value));
    this.#dom.qualityChoices.addEventListener("change", (event) => {
      if (!(event.target instanceof HTMLInputElement) || event.target.name !== "quality-choice") return;
      this.#selectQuality(event.target.value);
      this.#dom.qualityDialog.close();
    });
    this.#dom.captionSizeControl.addEventListener("change", () => this.#setPreference("captionSize", this.#dom.captionSizeControl.value));
    this.#dom.captionBackgroundControl.addEventListener("change", () => this.#setPreference("captionBackground", this.#dom.captionBackgroundControl.value));
    this.#dom.autoplayControl.addEventListener("change", () => this.#setPreference("autoplay", this.#dom.autoplayControl.checked));
    this.#dom.shortcutHelpButton.addEventListener("click", () => this.#dom.shortcutDialog.showModal());
    this.#dom.playerRetry.addEventListener("click", () => {
      const playback = this.#store.getState().playback;
      if (playback.item) {
        this.#resetAutomaticTranscodeRecovery();
        this.#loadSource(playback.item, {
          start: playback.currentTime,
          intent: playback.intent,
          forceSourceMode: playback.sourceMode,
          forceQuality: playback.outputQuality,
          forceAndroidMediaSource: playback.mediaSourceDelivery,
        });
      }
    });
    this.#dom.tryCompatible.addEventListener("click", () => {
      const playback = this.#store.getState().playback;
      if (playback.item) {
        this.#resetAutomaticTranscodeRecovery();
        this.#loadSource(playback.item, { start: playback.currentTime, intent: "playing", forceSourceMode: SOURCE_MODES.COMPATIBLE, message: "Preparing compatible playback…" });
      }
    });
    this.#dom.playOriginal.addEventListener("click", () => {
      const playback = this.#store.getState().playback;
      if (playback.item) this.#loadSource(playback.item, { start: playback.currentTime, intent: "playing", forceSourceMode: SOURCE_MODES.ORIGINAL });
    });
    this.#dom.returnLibrary.addEventListener("click", () => this.#onReturnLibrary());
    this.#dom.closePlayerButton.addEventListener("click", (event) => {
      this.closePlayback();
      if (event.detail > 0) event.currentTarget.blur();
    });
    this.#dom.playerStage.addEventListener("click", (event) => {
      const touchGenerated = event.pointerType === "touch"
        || event.sourceCapabilities?.firesTouchEvents === true
        || performance.now() <= this.#suppressVideoClickUntil;
      if (event.target === this.#dom.video && !touchGenerated) this.togglePlay();
    });
    this.#dom.playerStage.addEventListener("pointerdown", (event) => this.#handleVideoTouchPointerDown(event), { passive: true });
    this.#dom.playerStage.addEventListener("pointerup", (event) => this.#handleVideoTouchPointerUp(event));
    this.#dom.playerStage.addEventListener("pointercancel", (event) => {
      if (event.pointerId === this.#touchTapStart?.pointerId) this.#touchTapStart = null;
    }, { passive: true });
    for (const eventName of ["pointerenter", "pointermove", "pointerdown", "touchstart", "focusin"]) {
      this.#dom.playerStage.addEventListener(eventName, (event) => {
        const touch = event.type === "touchstart" || event.pointerType === "touch";
        this.#showControls(touch ? TOUCH_CONTROLS_IDLE_MS : CONTROLS_IDLE_MS, touch);
      }, { passive: true });
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
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "hidden") this.#markNativeHlsSuspended();
      this.#updateWakeLock();
    });
    window.addEventListener("resize", () => this.#scheduleDisplayViewport(), { passive: true });
    window.visualViewport?.addEventListener("resize", () => this.#scheduleDisplayViewport(), { passive: true });
    window.visualViewport?.addEventListener("scroll", () => this.#scheduleDisplayViewport(), { passive: true });
    window.addEventListener("pagehide", () => {
      this.#markNativeHlsSuspended();
      this.#progressWriter.flush();
    });
    this.#installMediaSessionHandlers();
  }

  #handleShortcut(event) {
    const dialogOpen = this.#dom.advancedPlaybackDialog.open
      || this.#dom.streamInfoDialog.open
      || this.#dom.shortcutDialog.open
      || this.#dom.itemDetailsDialog.open;
    if (event.key === "Escape" && !dialogOpen) {
      if (currentFullscreenElement() === this.#dom.playerStage
        || nativeVideoFullscreenActive(this.#dom.video)
        || this.#dom.playerStage.classList.contains("expanded-player")) {
        this.toggleFullscreen();
        return;
      }
      if (!this.#dom.captionMenu.hidden) {
        this.#dom.captionMenu.hidden = true;
        this.#dom.captionsButton.setAttribute("aria-expanded", "false");
        return;
      }
    }
    const target = event.target;
    const formField = target instanceof HTMLElement
      && (target.isContentEditable || ["INPUT", "SELECT", "TEXTAREA"].includes(target.tagName));
    const onButton = target instanceof HTMLElement && target.tagName === "BUTTON";
    const scoped = currentFullscreenElement() === this.#dom.playerStage
      || this.#dom.playerStage.classList.contains("expanded-player")
      || this.#dom.playerStage.matches(":hover")
      || document.activeElement === this.#dom.playerStage
      || this.#dom.playerStage.contains(document.activeElement);
    if (!scoped || formField || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
    const key = event.key.toLowerCase();
    if (onButton && key !== "escape") return;
    if ([" ", "k"].includes(key)) { event.preventDefault(); this.togglePlay(); }
    else if (["arrowleft", "j"].includes(key)) { event.preventDefault(); this.seekTo(this.globalTime() - 10); }
    else if (["arrowright", "l"].includes(key)) { event.preventDefault(); this.seekTo(this.globalTime() + 10); }
    else if (key === "m") { event.preventDefault(); this.#dom.muteButton.click(); }
    else if (key === "f") { event.preventDefault(); this.toggleFullscreen(); }
    else if (key === "escape") {
      if (dialogOpen) return;
      event.preventDefault();
      this.closePlayback();
    }
    else if (key === "?") { event.preventDefault(); this.#dom.shortcutDialog.showModal(); }
  }

  #showControls(idleMs = CONTROLS_IDLE_MS, touch = false) {
    const now = Date.now();
    if (touch) this.#touchControlsUntil = now + TOUCH_CONTROLS_IDLE_MS;
    this.#dom.playerStage.classList.add("controls-visible");
    if (this.#controlsTimer !== null) window.clearTimeout(this.#controlsTimer);
    const hideAt = Math.max(now + idleMs, this.#touchControlsUntil);
    this.#controlsTimer = window.setTimeout(() => {
      this.#controlsTimer = null;
      this.#touchControlsUntil = 0;
      if (!this.#controlsArePinned()) {
        this.#dom.playerStage.classList.remove("controls-visible");
      }
    }, hideAt - now);
  }

  #hideControls() {
    const touchGraceMs = this.#touchControlsUntil - Date.now();
    if (touchGraceMs > 0) {
      if (this.#controlsTimer !== null) window.clearTimeout(this.#controlsTimer);
      this.#controlsTimer = window.setTimeout(() => {
        this.#controlsTimer = null;
        this.#touchControlsUntil = 0;
        if (!this.#controlsArePinned()) {
          this.#dom.playerStage.classList.remove("controls-visible");
        }
      }, touchGraceMs);
      return;
    }
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
    return (focused === this.#dom.playerStage
      || focused === this.#dom.closePlayerButton
      || this.#dom.playbackControls.contains(focused))
      && focused.matches(":focus-visible");
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

  #selectQuality(quality) {
    this.#setPreference("quality", quality);
    if (quality !== "auto") {
      this.#setPreference("streamMode", STREAM_MODES.COMPATIBLE, "stream");
    }
    const playback = this.#store.getState().playback;
    if (playback.item && (quality !== "auto" || playback.sourceMode === SOURCE_MODES.COMPATIBLE)) this.#loadSource(playback.item, {
      start: this.globalTime(),
      intent: this.activePlayer().paused ? "paused" : "playing",
      forceSourceMode: quality !== "auto" ? SOURCE_MODES.COMPATIBLE : null,
      forceAndroidMediaSource: playback.mediaSourceDelivery,
      message: "Changing transcoded quality…",
    });
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

  #cancelSource({ keepElement = true, cancelTranscode = true } = {}) {
    const playback = this.#store.getState().playback;
    this.#invalidateWakeLockSession();
    const abandonedRequest = cancelTranscode
      && this.#sourceController
      && playback.sourceMode === SOURCE_MODES.COMPATIBLE
      && playback.item
      ? {
        itemId: playback.item.id,
        requestId: playback.sessionId,
        playbackSessionId: this.#playbackSession,
      }
      : null;
    this.#progressWriter.flush();
    this.#sourceController?.abort();
    this.#sourceController = null;
    this.#api.abortItem();
    if (this.#statusTimer !== null) window.clearTimeout(this.#statusTimer);
    this.#statusTimer = null;
    this.#clearStartupTimer();
    this.#cancelSeekTimer();
    if (!keepElement) this.#resetMediaElement(this.activePlayer());
    if (abandonedRequest) {
      this.#api.cancelTranscode(
        abandonedRequest.itemId,
        abandonedRequest.requestId,
        abandonedRequest.playbackSessionId,
      ).catch(() => {});
    }
  }

  #resetMediaElement(player) {
    if (!player) return;
    player.pause();
    player.removeAttribute("src");
    player.load();
    if (player === this.#dom.video && this.#mediaSourceObjectUrl) {
      URL.revokeObjectURL(this.#mediaSourceObjectUrl);
      this.#mediaSourceObjectUrl = null;
    }
  }

  #holdVideoFrame() {
    const playback = this.#store.getState().playback;
    const video = this.#dom.video;
    const canvas = this.#dom.videoFrameHold;
    if (playback.item?.kind !== "video" || !canvas.hidden) return;
    const sourceWidth = Number(video.videoWidth);
    const sourceHeight = Number(video.videoHeight);
    if (!(sourceWidth > 0) || !(sourceHeight > 0)
      || video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA) return;

    const bounds = video.getBoundingClientRect();
    if (!(bounds.width > 0) || !(bounds.height > 0)) return;
    const density = Math.min(2, Math.max(1, Number(window.devicePixelRatio) || 1));
    const fitScale = this.#store.getState().preferences.fill
      ? Math.max(bounds.width / sourceWidth, bounds.height / sourceHeight)
      : Math.min(bounds.width / sourceWidth, bounds.height / sourceHeight);
    let scale = Math.min(1, fitScale * density);
    const pixels = sourceWidth * sourceHeight * scale * scale;
    if (pixels > MAX_HELD_VIDEO_FRAME_PIXELS) {
      scale *= Math.sqrt(MAX_HELD_VIDEO_FRAME_PIXELS / pixels);
    }
    const width = Math.max(1, Math.floor(sourceWidth * scale));
    const height = Math.max(1, Math.floor(sourceHeight * scale));
    try {
      canvas.width = width;
      canvas.height = height;
      const context = canvas.getContext("2d", { alpha: false });
      if (!context) {
        this.#releaseHeldVideoFrame();
        return;
      }
      context.drawImage(video, 0, 0, width, height);
      canvas.hidden = false;
    } catch (_) {
      this.#releaseHeldVideoFrame();
    }
  }

  #cancelTrickplay() {
    this.#trickplayController?.abort();
    this.#trickplayController = null;
    this.#trickplayManifest = null;
    this.#trickplayImages.clear();
    this.#trickplayTarget = null;
    this.#trickplayPreloadStarted = false;
  }

  #loadTrickplay(item) {
    if (item?.kind !== "video" || !item.preview_url) return;
    const controller = new AbortController();
    this.#trickplayController = controller;
    void this.#api.preview(item.preview_url, { signal: controller.signal }).then((manifest) => {
      const playback = this.#store.getState().playback;
      if (controller.signal.aborted || this.#trickplayController !== controller
        || playback.item?.id !== item.id) return;
      if (!trickplayFrame(manifest, 0)) return;
      this.#trickplayManifest = manifest;
      this.#startTrickplayPreload();
      if (playback.previewTime !== null) this.#showTrickplayFrame(playback.previewTime);
    }).catch(() => {
      // Previews are optional. The held decoded frame remains the fallback.
    });
  }

  #startTrickplayPreload() {
    const manifest = this.#trickplayManifest;
    const controller = this.#trickplayController;
    if (!manifest || !controller || this.#trickplayPreloadStarted) return;
    this.#trickplayPreloadStarted = true;
    const urls = trickplayPreloadUrls(manifest.sheet_urls);
    void this.#api.preloadPreviewSheets(urls, { signal: controller.signal }).catch(() => {
      // On-demand image loading still works if speculative caching is denied.
    });
  }

  #trickplayImage(url) {
    const cached = this.#trickplayImages.get(url);
    if (cached) {
      this.#trickplayImages.delete(url);
      this.#trickplayImages.set(url, cached);
      return cached;
    }
    const image = new Image();
    image.decoding = "async";
    const signal = this.#trickplayController?.signal;
    const promise = this.#api.preloadPreviewSheets([url], { signal }).then(() => new Promise((resolve, reject) => {
      image.addEventListener("load", () => resolve(image), { once: true });
      image.addEventListener("error", reject, { once: true });
      image.src = url;
    }));
    const entry = { image, promise };
    this.#trickplayImages.set(url, entry);
    while (this.#trickplayImages.size > MAX_DECODED_TRICKPLAY_SHEETS) {
      const oldest = this.#trickplayImages.keys().next().value;
      if (oldest === url) break;
      this.#trickplayImages.delete(oldest);
    }
    promise.catch(() => {
      if (this.#trickplayImages.get(url) === entry) this.#trickplayImages.delete(url);
    });
    return entry;
  }

  #showTrickplayFrame(seconds) {
    const manifest = this.#trickplayManifest;
    const playback = this.#store.getState().playback;
    const frame = trickplayFrame(manifest, seconds);
    if (!frame || playback.item?.kind !== "video") {
      this.#holdVideoFrame();
      return;
    }
    this.#holdVideoFrame();
    const target = {
      playbackSessionId: this.#playbackSession,
      frameIndex: frame.frameIndex,
      committed: false,
    };
    this.#trickplayTarget = target;
    const entry = this.#trickplayImage(frame.url);
    void entry.promise.then((image) => {
      if (this.#trickplayTarget !== target
        || this.#playbackSession !== target.playbackSessionId) return;
      const canvas = this.#dom.videoFrameHold;
      const width = Number(manifest.frame_width);
      const height = Number(manifest.frame_height);
      if (!(width > 0) || !(height > 0)) return;
      try {
        canvas.width = width;
        canvas.height = height;
        const context = canvas.getContext("2d", { alpha: false });
        if (!context) return;
        context.drawImage(
          image,
          frame.column * width,
          frame.row * height,
          width,
          height,
          0,
          0,
          width,
          height,
        );
        canvas.hidden = false;
      } catch (_) {
        // Keep the last decoded video frame when a sheet cannot be drawn.
      }
    }).catch(() => {});
  }

  #releaseHeldVideoFrame() {
    const canvas = this.#dom.videoFrameHold;
    this.#trickplayTarget = null;
    canvas.hidden = true;
    // Resetting the bitmap releases a potentially display-sized allocation.
    canvas.width = 1;
    canvas.height = 1;
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

  #syncPlaybackAnnouncement(playback) {
    let message = "";
    if (playback.item) {
      if (playback.status === "idle" && playback.audioTracksStatus === "loading") {
        message = "Loading media details.";
      } else if (playback.status === "loading") {
        message = playback.sourceMode === SOURCE_MODES.COMPATIBLE
          ? "Preparing compatible playback."
          : "Loading playback.";
      } else if (playback.status === "waiting") {
        message = "Playback is buffering.";
      } else if (playback.status === "seeking") {
        message = "Seeking.";
      } else if (playback.status === "playing") {
        message = `Playing ${playback.item.title}.`;
      } else if (playback.status === "paused" && playback.intent === "paused" && !playback.message) {
        message = playback.autoplayBlocked
          ? "Playback is ready. Press Play to begin."
          : "Playback paused.";
      } else if (playback.status === "ended") {
        message = "Playback ended.";
      }
    }
    const phase = playback.status === "idle"
      ? `${playback.status}:${playback.audioTracksStatus}`
      : playback.status;
    const key = `${playback.sessionId}:${phase}:${message}`;
    if (key === this.#announcementKey) return;
    this.#announcementKey = key;
    if (this.#announceTimer !== null) window.clearTimeout(this.#announceTimer);
    this.#dom.playbackLive.textContent = "";
    if (!message) {
      this.#announceTimer = null;
      return;
    }
    this.#announceTimer = window.setTimeout(() => {
      this.#announceTimer = null;
      if (key === this.#announcementKey) {
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

  #shouldHoldWakeLock() {
    const state = this.#store.getState();
    return state.playback.sessionId !== this.#wakeLockBlockedSession
      && state.playback.status === "playing"
      && state.playback.item?.kind === "video"
      && state.playback.fullscreen
      && document.visibilityState === "visible";
  }

  #invalidateWakeLockSession() {
    this.#wakeLockBlockedSession = this.#store.getState().playback.sessionId;
    this.#updateWakeLock();
  }

  #updateWakeLock() {
    const shouldHold = this.#shouldHoldWakeLock();
    if (shouldHold !== this.#wakeLockDesired) {
      this.#wakeLockDesired = shouldHold;
      this.#wakeLockGeneration += 1;
    }
    if (!shouldHold) {
      const wakeLock = this.#wakeLock;
      this.#wakeLock = null;
      this.#releaseWakeLock(wakeLock);
      return;
    }
    if (this.#wakeLock
      || this.#wakeLockRequest
      || this.#wakeLockDeniedGeneration === this.#wakeLockGeneration
      || !("wakeLock" in navigator)) return;
    const generation = this.#wakeLockGeneration;
    try {
      const request = Promise.resolve(navigator.wakeLock.request("screen"));
      this.#wakeLockRequest = request;
      this.#settleWakeLockRequest(request, generation);
    } catch (_) {
      // Denial is normal (battery policy, permissions, or unsupported context).
      this.#wakeLockDeniedGeneration = generation;
    }
  }

  async #settleWakeLockRequest(request, generation) {
    let wakeLock = null;
    let denied = false;
    try {
      wakeLock = await request;
    } catch (_) {
      // Denial is normal (battery policy, permissions, or unsupported context).
      denied = true;
    }
    if (this.#wakeLockRequest !== request) {
      await this.#releaseWakeLock(wakeLock);
      return;
    }
    this.#wakeLockRequest = null;
    if ((denied || !wakeLock) && generation === this.#wakeLockGeneration) {
      this.#wakeLockDeniedGeneration = generation;
    }
    const superseded = generation !== this.#wakeLockGeneration;
    if (!wakeLock || superseded || !this.#wakeLockDesired || !this.#shouldHoldWakeLock()) {
      await this.#releaseWakeLock(wakeLock);
      if (superseded && this.#wakeLockDesired && this.#shouldHoldWakeLock()) this.#updateWakeLock();
      return;
    }
    this.#wakeLock = wakeLock;
    wakeLock.addEventListener("release", () => {
      if (this.#wakeLock === wakeLock) {
        this.#wakeLockDeniedGeneration = generation;
        this.#wakeLock = null;
      }
    }, { once: true });
  }

  async #releaseWakeLock(wakeLock) {
    try { await wakeLock?.release(); } catch (_) { /* Already released. */ }
  }
}
