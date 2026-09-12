// Abort-scoped Media Source transport and bounded buffering.
// Source selection, playback intent, and recovery belong to the player.
import { bufferedSeekTarget, bufferedRangeSecondsAhead, fallbackMediaSourceType, parseHlsMediaPlaylist, reusableMediaSourceSeek, retainedMediaSourceBytes } from "./core.js";

// Copied UHD fragments can exceed 10 MB per second. Keep the total window
// below Chromium's practical SourceBuffer quota instead of treating every
// codec and bitrate like a small mobile rendition. Seeks start a new
// generation for distant seeks; retain a useful bounded nearby-seek window.
export const MEDIA_SOURCE_RESOURCE_MAX_BYTES = 32 * 1024 * 1024;
export const MEDIA_SOURCE_BUFFER_MAX_BYTES = 96 * 1024 * 1024;
const MEDIA_SOURCE_BUFFER_AHEAD_SECONDS = 10;

export class MediaSourceResourceError extends Error {
  constructor(message) { super(message); this.name = "MediaSourceResourceError"; this.code = "resource_limit"; }
}
const MEDIA_SOURCE_RETAIN_BEHIND_SECONDS = 5;
const MEDIA_SOURCE_PLAYLIST_POLL_MS = 500;
const MEDIA_SOURCE_EVENT_TIMEOUT_MS = 20_000;
const MEDIA_SOURCE_BODY_PROGRESS_MS = 15_000;
const MEDIA_SOURCE_SEEK_SETTLE_MS = 100;

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

export async function fetchResource(url, signal, { playlist = false, resourceMaxBytes = MEDIA_SOURCE_RESOURCE_MAX_BYTES, onHeaders } = {}) {
  if (signal.aborted) throw abortedError();
  const controller = new AbortController();
  const abort = () => controller.abort(abortedError());
  signal.addEventListener("abort", abort, { once: true });
  const limit = playlist ? 4 * 1024 * 1024 : Math.min(MEDIA_SOURCE_RESOURCE_MAX_BYTES, resourceMaxBytes);
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
    if (response.status === 413) throw new MediaSourceResourceError("Media Source resource is too large.");
    if (!response.ok) throw new Error(`Media Source resource returned HTTP ${response.status}.`);
    if (Number(response.headers.get("content-length")) > limit) throw new MediaSourceResourceError("Media Source resource is too large.");
    onHeaders?.(response.headers);
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
      if (length > limit) throw new MediaSourceResourceError("Media Source resource is too large.");
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
  onController = () => {},
  copiedVideo = false,
  videoOutputs = [],
  resourceMaxBytes = MEDIA_SOURCE_RESOURCE_MAX_BYTES,
  bufferMaxBytes = MEDIA_SOURCE_BUFFER_MAX_BYTES,
}) {
  if (mediaSource.readyState !== "open") {
    await waitForMediaEvent(mediaSource, "sourceopen", signal, "sourceclose");
  }
  if (signal.aborted || mediaSource.readyState !== "open") throw abortedError();
  resourceMaxBytes = Math.min(MEDIA_SOURCE_RESOURCE_MAX_BYTES, resourceMaxBytes);
  bufferMaxBytes = Math.min(MEDIA_SOURCE_BUFFER_MAX_BYTES, bufferMaxBytes);
  let sourceBuffer = mediaSource.addSourceBuffer(contentType);
  sourceBuffer.mode = "segments";
  const appended = new Set();
  let segments = [];
  let initializationBytes = 0;
  let duration = 0;
  let timelinePriming = 0;
  let ended = false;
  let mutating = false;
  let appending = false;
  let seekWaiter = null;
  const ranges = () => bufferedRanges(sourceBuffer);
  const retainedBytes = () => initializationBytes + retainedMediaSourceBytes(segments, ranges());
  const reconcile = () => {
    const current = ranges();
    segments = segments.filter((segment) => current.some((range) => segment.start < range.end && segment.end > range.start));
  };
  const prune = async (required = false) => {
    mutating = true;
    try {
      await pruneMediaSourceBuffer(sourceBuffer, player.currentTime, signal, required, segments, pendingSeek(), copiedVideo);
      reconcile();
    } finally { mutating = false; seekWaiter?.check(); }
  };
  const appendResource = async (...args) => {
    appending = true;
    try { return await appendMediaSourceResource(...args); }
    finally { appending = false; }
  };
  const hasSeekData = (target) => !signal.aborted && ["open", "ended"].includes(mediaSource.readyState)
    && reusableMediaSourceSeek({ ranges: ranges(), segments, target, copiedVideo, duration: ended ? duration : Infinity });
  const canSeek = (target) => !mutating && !sourceBuffer.updating && hasSeekData(target);
  onController({
    canSeek,
    waitForSeek(target) {
      seekWaiter?.finish();
      // Existing decoder prerequisites can be temporarily busy while a later
      // fragment appends. Wait only for our owned operation, then let the
      // player recheck source ownership and the actual post-eviction ranges.
      if (canSeek(target) || !(appending || mutating) || !hasSeekData(target)) return null;
      return new Promise((resolve) => {
        const finish = () => {
          window.clearTimeout(timer);
          signal.removeEventListener("abort", finish);
          if (seekWaiter?.finish === finish) seekWaiter = null;
          resolve();
        };
        const timer = window.setTimeout(finish, MEDIA_SOURCE_SEEK_SETTLE_MS);
        signal.addEventListener("abort", finish, { once: true });
        seekWaiter = { finish, check: () => { if (!mutating && !sourceBuffer.updating) finish(); } };
      });
    },
    snapshot() { return { bufferedBytes: retainedBytes(), bufferMaxBytes,
      resourceMaxBytes, secondsAhead: bufferedSecondsAhead(sourceBuffer, player.currentTime) }; },
  });
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
    let fallbackVideoOutput = null;
    let fallbackAudioCodec = null;
    const bytes = await fetchResource(requestUrl, signal, { playlist: true, onHeaders: (headers) => {
      fallbackVideoOutput = headers.get("X-Rusty-Video-Output");
      fallbackAudioCodec = headers.get("X-Rusty-Audio-Codec");
    } });
    if (signal.aborted || mediaSource.readyState !== "open") throw abortedError();
    const playlist = parseHlsMediaPlaylist(new TextDecoder().decode(bytes), requestUrl.href);
    if (!playlist) throw new Error("Media Source playlist is invalid.");
    if (!playlistReported) {
      playlistReported = true;
      reportStartup("mse_playlist_received");
    }

    if (!initAppended) {
      // The playlist pins the producer's actual bytes. Replace the still-empty
      // buffer before init append when a permitted fallback changed codecs.
      // This also handles warm fallback reuse without a new playback session.
      const actualType = fallbackMediaSourceType(contentType, fallbackVideoOutput, fallbackAudioCodec, videoOutputs);
      if (!actualType) throw new Error("Media Source fallback recipe is unsupported.");
      if (actualType !== contentType) {
        mediaSource.removeSourceBuffer(sourceBuffer);
        sourceBuffer = mediaSource.addSourceBuffer(actualType);
        sourceBuffer.mode = "segments";
      }
      if (fallbackVideoOutput) copiedVideo = false;
      initializationBytes = await appendResource(
        sourceBuffer,
        playlist.initUrl,
        player,
        signal,
        {
          resourceMaxBytes, prune,
          onFetched: () => reportStartup("mse_init_fetched"),
          onAppended: () => reportStartup("mse_init_appended"),
        },
      );
      initAppended = true;
    }

    let appendedNewSegment = false;
    for (const segment of playlist.segments) {
      const segmentUrl = segment.url;
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
      const advertisedBytes = Number(new URL(segmentUrl).searchParams.get("hls_length"));
      if (advertisedBytes > resourceMaxBytes) throw new MediaSourceResourceError("Media Source resource is too large.");
      while (retainedBytes() + advertisedBytes > bufferMaxBytes) {
        await prune();
        if (retainedBytes() + advertisedBytes <= bufferMaxBytes) break;
        if (needsSeekData()) {
          await prune(true);
          if (retainedBytes() + advertisedBytes > bufferMaxBytes) {
            throw new MediaSourceResourceError("The requested seek needs more decoder memory than is available.");
          }
        } else {
          if (!player.paused && bufferedSecondsAhead(sourceBuffer, player.currentTime) < 0.25) {
            throw new MediaSourceResourceError("The copied decoder group exceeds the playback memory budget.");
          }
          await abortableDelay(250, signal);
        }
      }
      const firstFragment = appended.size === 0;
      const byteLength = await appendResource(
        sourceBuffer,
        segmentUrl,
        player,
        signal,
        {
          resourceMaxBytes, prune,
          onFetched: firstFragment ? () => reportStartup("mse_first_fragment_fetched") : undefined,
          onAppended: firstFragment ? () => reportStartup("mse_first_fragment_appended") : undefined,
        },
      );
      if (firstFragment && sourceBuffer.buffered.length > 0) {
        // MP4 decode timestamps begin at zero, but AAC priming/B-frame
        // composition can shift the first presentation timestamp slightly.
        // Learn that origin once; eviction must never redefine it.
        timelinePriming = Math.max(0, Math.min(2, sourceBuffer.buffered.start(0) - (segment.start ?? 0)));
      }
      const start = segment.start === undefined ? duration : segment.start + timelinePriming;
      const end = start + segment.duration;
      // Old playlists remain playable but copied-video seek reuse requires the
      // server's explicit random-access prerequisite. Encoded output has IDRs.
      segments.push({ start, end, decodeStart: segment.decodeStart === undefined ? (copiedVideo ? null : start) : segment.decodeStart + timelinePriming, bytes: byteLength });
      duration = Math.max(duration, end);
      appended.add(segmentUrl);
      appendedNewSegment = true;
      onBuffered();
      await prune();
    }

    if (playlist.ended) {
      ended = true;
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
  const bytes = await fetchResource(url, signal, { resourceMaxBytes: observers.resourceMaxBytes });
  observers.onFetched?.();
  const deadline = performance.now() + MEDIA_SOURCE_EVENT_TIMEOUT_MS;
  while (true) {
    try {
      await sourceBufferOperation(sourceBuffer, () => sourceBuffer.appendBuffer(bytes), signal);
      break;
    } catch (error) {
      if (error?.name !== "QuotaExceededError") throw error;
      if (performance.now() >= deadline) throw new MediaSourceResourceError("Media Source decoder memory is full.");
      try { await observers.prune(true); }
      catch (pruneError) {
        if (pruneError?.code !== "resource_limit") throw pruneError;
        // Let decoded playback release an old GOP before retrying. Paused
        // seeks already prune toward their target and cannot make progress by
        // waiting for playback the user did not request.
        if (player.paused) throw pruneError;
      }
      await abortableDelay(250, signal);
    }
  }
  observers.onAppended?.();
  return bytes.byteLength;
}

async function pruneMediaSourceBuffer(sourceBuffer, currentTime, signal, required = false, segments = [], target = null, copiedVideo = false) {
  const ranges = bufferedRanges(sourceBuffer);
  const frontier = ranges.at(-1)?.end ?? 0;
  // A paused distant seek can discard old GOPs as it advances toward its
  // target. Round removal down to a known RAP so it never cuts prerequisites.
  const position = target === null ? currentTime : Math.min(target, frontier);
  const desired = Math.max(0, position - (required ? 2 : MEDIA_SOURCE_RETAIN_BEHIND_SECONDS));
  const points = segments.map((segment) => segment.decodeStart).filter((value) => Number.isFinite(value) && value <= desired);
  const knownBoundary = points.length ? Math.max(...points) : 0;
  const removeEnd = copiedVideo ? knownBoundary : Math.max(knownBoundary, Math.floor(desired));
  if (!(removeEnd > 0) || sourceBuffer.buffered.length === 0) {
    if (required) throw new MediaSourceResourceError("Media Source decoder memory is full.");
    return;
  }
  const removeStart = sourceBuffer.buffered.start(0);
  if (!(removeEnd > removeStart)) {
    if (required) throw new MediaSourceResourceError("Media Source decoder memory is full.");
    return;
  }
  await sourceBufferOperation(
    sourceBuffer,
    () => sourceBuffer.remove(removeStart, removeEnd),
    signal,
  );
}
