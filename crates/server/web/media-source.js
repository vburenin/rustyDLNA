// Abort-scoped Media Source transport and bounded buffering.
// Source selection, playback intent, and recovery belong to the player.
import { bufferedSeekTarget, bufferedRangeSecondsAhead, parseHlsMediaPlaylist } from "./core.js";

// Copied UHD fragments can exceed 10 MB per second. Keep the total window
// below Chromium's practical SourceBuffer quota instead of treating every
// codec and bitrate like a small mobile rendition. Seeks start a new
// generation, so only a short backward window is useful here.
const MEDIA_SOURCE_BUFFER_AHEAD_SECONDS = 10;
const MEDIA_SOURCE_RETAIN_BEHIND_SECONDS = 5;
const MEDIA_SOURCE_PLAYLIST_POLL_MS = 500;
const MEDIA_SOURCE_EVENT_TIMEOUT_MS = 20_000;
const MEDIA_SOURCE_BODY_PROGRESS_MS = 15_000;

function timeoutError(phase) {
  return new Error(`Media Source ${phase} timed out.`);
}

// Race the operation as well as aborting fetch: a body implementation must not
// be able to hold source replacement or recovery hostage by ignoring abort.
function withAbort(promise, signal) {
  if (signal.aborted) {
    void Promise.resolve(promise).catch(() => {});
    return Promise.reject(signal.reason || abortedError());
  }
  return new Promise((resolve, reject) => {
    const abort = () => { cleanup(); reject(signal.reason || abortedError()); };
    const cleanup = () => signal.removeEventListener("abort", abort);
    signal.addEventListener("abort", abort, { once: true });
    Promise.resolve(promise).then(
      (value) => { cleanup(); resolve(value); },
      (error) => { cleanup(); reject(error); },
    );
  });
}

async function fetchResource(url, signal, { playlist = false } = {}) {
  if (signal.aborted) throw abortedError();
  const controller = new AbortController();
  const abort = () => controller.abort(abortedError());
  signal.addEventListener("abort", abort, { once: true });
  const limit = (playlist ? 4 : 32) * 1024 * 1024;
  // Playlist headers may wait for helper admission, preparation, and the first
  // complete fragment. Subsequent finite-resource requests need less grace.
  let progressTimer = window.setTimeout(() => controller.abort(timeoutError("headers")), playlist ? 120_000 : 30_000);
  const absoluteTimer = window.setTimeout(() => controller.abort(timeoutError("request")), playlist ? 180_000 : 120_000);
  let reader;
  let completed = false;
  try {
    const response = await withAbort(fetch(url, {
      cache: "no-store", credentials: "same-origin", signal: controller.signal,
    }), controller.signal);
    if (!response.ok) throw new Error(`Media Source resource returned HTTP ${response.status}.`);
    if (Number(response.headers.get("content-length")) > limit) throw new Error("Media Source resource is too large.");
    if (!response.body) throw new Error("Media Source resource has no body.");
    reader = response.body.getReader();
    const chunks = [];
    let length = 0;
    let reads = 0;
    const resetProgress = () => {
      window.clearTimeout(progressTimer);
      progressTimer = window.setTimeout(() => controller.abort(timeoutError("body progress")), MEDIA_SOURCE_BODY_PROGRESS_MS);
    };
    resetProgress();
    while (true) {
      const { value, done } = await withAbort(reader.read(), controller.signal);
      if (done) break;
      if (++reads > 65_536) throw new Error("Media Source resource has too many chunks.");
      if (value.byteLength) resetProgress();
      length += value.byteLength;
      if (length > limit) throw new Error("Media Source resource is too large.");
      if (value.byteLength) chunks.push(value);
    }
    if (length === 0) throw new Error("Media Source resource is empty.");
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    completed = true;
    return bytes;
  } finally {
    window.clearTimeout(progressTimer);
    window.clearTimeout(absoluteTimer);
    signal.removeEventListener("abort", abort);
    if (!completed) {
      controller.abort();
      void reader?.cancel().catch(() => {});
    }
    // A cancelled stream may still have a pending read; cancellation settles it
    // without delaying the already bounded operation's failure.
    try { reader?.releaseLock(); } catch (_) { /* Pending abort completion. */ }
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
    const timer = window.setTimeout(() => {
      cleanup();
      reject(timeoutError(eventName));
    }, MEDIA_SOURCE_EVENT_TIMEOUT_MS);
    function cleanup() {
      window.clearTimeout(timer);
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
    const timer = window.setTimeout(() => {
      cleanup();
      reject(timeoutError("buffer update"));
    }, MEDIA_SOURCE_EVENT_TIMEOUT_MS);
    function cleanup() {
      window.clearTimeout(timer);
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

function bufferedRanges(sourceBuffer) {
  const ranges = [];
  for (let index = 0; index < sourceBuffer.buffered.length; index += 1) {
    ranges.push({
      start: sourceBuffer.buffered.start(index),
      end: sourceBuffer.buffered.end(index),
    });
  }
  return ranges;
}

function bufferedSecondsAhead(sourceBuffer, currentTime) {
  return bufferedRangeSecondsAhead(bufferedRanges(sourceBuffer), currentTime);
}

function waitForMediaSourcePlayback(player, signal) {
  if (signal.aborted) return Promise.reject(abortedError());
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

export async function pumpMediaSource({
  player,
  mediaSource,
  playlistUrl,
  contentType,
  signal,
  reportStartup,
  pendingSeek = () => null,
  onBuffered = () => {},
}) {
  if (mediaSource.readyState !== "open") {
    await waitForMediaEvent(mediaSource, "sourceopen", signal, "sourceclose");
  }
  if (signal.aborted || mediaSource.readyState !== "open") throw abortedError();
  const sourceBuffer = mediaSource.addSourceBuffer(contentType);
  sourceBuffer.mode = "segments";
  const appended = new Set();
  let initAppended = false;
  let playlistReported = false;
  const needsSeekData = () => {
    const target = pendingSeek();
    // A target can be buffered while the decoder still needs the next audio
    // packet or video frame to finish seeking. Playback waits for that seek,
    // so waiting for play here would deadlock both sides of the handoff.
    return target !== null && (!bufferedSeekTarget(bufferedRanges(sourceBuffer), target)
      || player.seeking || player.readyState < HTMLMediaElement.HAVE_FUTURE_DATA);
  };

  while (!signal.aborted) {
    if (appended.size > 0 && player.paused && !needsSeekData()) {
      await waitForMediaSourcePlayback(player, signal);
    }
    const requestUrl = new URL(playlistUrl);
    requestUrl.searchParams.set("mse_after", String(appended.size));
    const bytes = await fetchResource(requestUrl, signal, { playlist: true });
    const playlist = parseHlsMediaPlaylist(new TextDecoder().decode(bytes), requestUrl.href);
    if (!playlist) throw new Error("Media Source playlist is invalid.");
    if (!playlistReported) {
      playlistReported = true;
      reportStartup("mse_playlist_received");
    }

    if (!initAppended) {
      await appendMediaSourceResource(
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
      // A paused exact seek may need several fragments within its ten-second
      // server bucket. Stop as soon as its seek can complete, then wait for
      // playback to resume; ordinary paused starts still fetch one fragment.
      if (appended.size > 0 && player.paused && !needsSeekData()) {
        await waitForMediaSourcePlayback(player, signal);
      }
      while (!needsSeekData() && bufferedSecondsAhead(sourceBuffer, player.currentTime)
        >= MEDIA_SOURCE_BUFFER_AHEAD_SECONDS) {
        await abortableDelay(250, signal);
      }
      const firstFragment = appended.size === 0;
      await appendMediaSourceResource(
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
      onBuffered();
      await pruneMediaSourceBuffer(sourceBuffer, player.currentTime, signal);
    }

    if (playlist.ended) {
      const target = pendingSeek();
      if (target !== null && !bufferedSeekTarget(bufferedRanges(sourceBuffer), target)) {
        throw new Error("Media Source ended before the requested seek position.");
      }
      if (sourceBuffer.updating) await waitForMediaEvent(sourceBuffer, "updateend", signal);
      if (mediaSource.readyState === "open") mediaSource.endOfStream();
      return;
    }
    if (appended.size > 0 && player.paused && !needsSeekData()) {
      await waitForMediaSourcePlayback(player, signal);
    }
    if (!appendedNewSegment) await abortableDelay(MEDIA_SOURCE_PLAYLIST_POLL_MS, signal);
  }
}

async function appendMediaSourceResource(sourceBuffer, url, player, signal, observers = {}) {
  const bytes = await fetchResource(url, signal);
  observers.onFetched?.();
  try {
    await sourceBufferOperation(sourceBuffer, () => sourceBuffer.appendBuffer(bytes), signal);
  } catch (error) {
    if (error?.name !== "QuotaExceededError") throw error;
    await pruneMediaSourceBuffer(sourceBuffer, player.currentTime, signal, true);
    await sourceBufferOperation(sourceBuffer, () => sourceBuffer.appendBuffer(bytes), signal);
  }
  observers.onAppended?.();
}

async function pruneMediaSourceBuffer(sourceBuffer, currentTime, signal, required = false) {
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
