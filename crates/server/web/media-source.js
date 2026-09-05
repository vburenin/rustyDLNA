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
  await waitForMediaEvent(mediaSource, "sourceopen", signal, "sourceclose");
  if (signal.aborted || mediaSource.readyState !== "open") throw abortedError();
  const sourceBuffer = mediaSource.addSourceBuffer(contentType);
  sourceBuffer.mode = "segments";
  const appended = new Set();
  let initAppended = false;
  let playlistReported = false;
  const needsSeekData = () => {
    const target = pendingSeek();
    return target !== null && !bufferedSeekTarget(bufferedRanges(sourceBuffer), target);
  };

  while (!signal.aborted) {
    if (appended.size > 0 && player.paused && !needsSeekData()) {
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
      // server bucket. Stop as soon as its target is buffered, then wait for
      // playback to resume; ordinary paused starts still fetch one fragment.
      if (appended.size > 0 && player.paused && !needsSeekData()) {
        await waitForMediaSourcePlayback(player, signal);
      }
      while (bufferedSecondsAhead(sourceBuffer, player.currentTime)
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
      if (needsSeekData()) throw new Error("Media Source ended before the requested seek position.");
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
