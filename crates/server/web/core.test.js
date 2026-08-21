import test from "node:test";
import assert from "node:assert/strict";

import {
  audioDecodingConfiguration,
  audioTrackLabel,
  chooseSource,
  compatibleSegmentStart,
  clockLabel,
  durationSeconds,
  navigationFromUrl,
  navigationUrl,
  negotiateCompatibleStreams,
  queueNeighbor,
  resumePosition,
  seekTarget,
  timelineValueText,
  videoDecodingConfiguration,
} from "./core.js";
import { initialState, Store } from "./store.js";

test("time conversion and seek bounds keep the real end position", () => {
  assert.equal(durationSeconds("1:02:03.5"), 3723.5);
  assert.equal(clockLabel(3723.9), "1:02:03");
  assert.equal(seekTarget(100, 100), 100);
  assert.equal(seekTarget(101, 100), 100);
  assert.equal(timelineValueText(100, 100), "1:40 of 1:40");
});

test("compatible seeks share a nearby ephemeral segment", () => {
  assert.equal(compatibleSegmentStart(0), 0);
  assert.equal(compatibleSegmentStart(127.9), 120);
  assert.equal(compatibleSegmentStart(129), 120);
  assert.equal(compatibleSegmentStart(130), 130);
});

test("resume thresholds discard trivial and near-end positions", () => {
  assert.equal(resumePosition(12, 3600), 0);
  assert.equal(resumePosition(120, 3600), 120);
  assert.equal(resumePosition(3540, 3600), 0);
  assert.equal(resumePosition(500, 0), 0);
});

test("source choice is explicit about forced and unavailable modes", () => {
  assert.deepEqual(chooseSource({ requestedMode: "auto", directSupport: true, transcoding: true }), {
    mode: "direct", reason: "browser_supported",
  });
  assert.deepEqual(chooseSource({ requestedMode: "auto", directSupport: false, transcoding: true }), {
    mode: "compatible", reason: "browser_support_uncertain",
  });
  assert.deepEqual(chooseSource({ requestedMode: "compat", directSupport: false, transcoding: false }), {
    mode: "direct", reason: "transcoding_disabled",
  });
});

test("compatible playback negotiates video and audio independently", async () => {
  const item = {
    kind: "video",
    video_content_type: 'video/mp4; codecs="hvc1.1.6.L120.B0"',
    width: 3840,
    height: 2160,
    bitrate: 2_000_000,
    frame_rate: "24000/1001",
    hdr: "hdr10",
    sample_rate: 48_000,
  };
  const ac3 = { codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6 };
  assert.deepEqual(videoDecodingConfiguration(item), {
    type: "file",
    video: {
      contentType: item.video_content_type,
      width: 3840,
      height: 2160,
      bitrate: 16_000_000,
      framerate: 24000 / 1001,
      hdrMetadataType: "smpteSt2086",
      colorGamut: "bt2020",
      transferFunction: "pq",
    },
  });
  assert.equal(audioDecodingConfiguration(item, ac3).audio.channels, "6");

  const queried = [];
  const negotiated = await negotiateCompatibleStreams({
    item,
    track: ac3,
    quality: "auto",
    canPlayType: (contentType) => contentType.includes("ac-3") ? "" : "probably",
    decodingInfo: async (configuration) => {
      queried.push(configuration);
      return { supported: Boolean(configuration.video) };
    },
  });
  assert.deepEqual(negotiated, {
    video: "copy",
    audio: "transcode",
    videoContentType: item.video_content_type,
    audioContentType: ac3.content_type,
    videoProbe: { supported: true, canPlayType: "probably", mediaCapabilities: "supported" },
    audioProbe: { supported: false, canPlayType: "unsupported", mediaCapabilities: "unsupported" },
  });
  assert.equal(queried.length, 2);

  const resized = await negotiateCompatibleStreams({
    item,
    track: { ...ac3, codec: "aac", content_type: 'audio/mp4; codecs="mp4a.40.2"' },
    quality: "full_hd",
    canPlayType: () => "probably",
  });
  assert.equal(resized.video, "transcode");
  assert.equal(resized.audio, "copy");

  const repaired = await negotiateCompatibleStreams({
    item: { ...item, video_repair_required: true },
    track: ac3,
    quality: "auto",
    canPlayType: () => "probably",
  });
  assert.equal(repaired.video, "repair");

  const disagreeingApis = await negotiateCompatibleStreams({
    item,
    track: ac3,
    quality: "auto",
    canPlayType: (contentType) => contentType.includes("hvc1.") ? "probably" : "",
    decodingInfo: async () => ({ supported: false }),
  });
  assert.equal(disagreeingApis.video, "copy");
  assert.deepEqual(disagreeingApis.videoProbe, {
    supported: true,
    canPlayType: "probably",
    mediaCapabilities: "unsupported",
  });
});

test("compatible audio-only playback copies supported codecs and survives capability API errors", async () => {
  const item = { kind: "audio", channels: 2, sample_rate: 48_000 };
  const track = { codec: "aac", content_type: 'audio/mp4; codecs="mp4a.40.2"', channels: 2 };
  let probes = 0;
  const negotiated = await negotiateCompatibleStreams({
    item,
    track,
    quality: "auto",
    canPlayType: (contentType) => contentType === track.content_type ? "probably" : "",
    decodingInfo: async () => {
      probes += 1;
      throw new Error("MediaCapabilities unavailable for this codec");
    },
  });
  assert.deepEqual(negotiated, {
    video: "transcode",
    audio: "copy",
    videoContentType: null,
    audioContentType: track.content_type,
    videoProbe: { supported: false, canPlayType: "not tested", mediaCapabilities: "not tested" },
    audioProbe: { supported: true, canPlayType: "probably", mediaCapabilities: "error" },
  });
  assert.equal(probes, 1);
});

test("queue navigation is stable by item ID", () => {
  const queue = [{ id: 3 }, { id: 8 }, { id: 13 }];
  assert.equal(queueNeighbor(queue, 8, -1).id, 3);
  assert.equal(queueNeighbor(queue, "8", 1).id, 13);
  assert.equal(queueNeighbor(queue, 13, 1), null);
});

test("track labels include language, title, codec, and channel layout", () => {
  assert.equal(
    audioTrackLabel({ index: 1, language: "eng", title: "Commentary", default: true, codec: "ac3", channels: 6 }),
    "ENG · Commentary · Default · AC-3 5.1",
  );
});

test("URL state round-trips folder, search, sort, and flat views", () => {
  const folder = navigationFromUrl("http://server/?folder=abc&q=film&sort=date_desc");
  assert.deepEqual(folder, { view: "folders", folder: "abc", kind: "all", query: "film", sort: "date_desc", itemId: null, start: 0 });
  assert.equal(navigationUrl("http://server/", folder, "root"), "/?folder=abc&q=film&sort=date_desc");
  const flat = navigationFromUrl("http://server/?view=audio");
  assert.equal(flat.view, "library");
  assert.equal(flat.kind, "audio");
  const continuing = navigationFromUrl("http://server/?view=continue");
  assert.equal(continuing.view, "continue");
  assert.equal(navigationUrl("http://server/", continuing, "root"), "/?view=continue");
  const deep = navigationFromUrl("http://server/?view=video&item=42&t=90");
  assert.equal(deep.itemId, "42");
  assert.equal(deep.start, 90);
  assert.equal(navigationUrl("http://server/", deep, "root"), "/?view=video&item=42&t=90");
});

test("stale media events cannot mutate a newer playback session", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "auto", quality: "auto", muted: false, loop: false, fill: false, autoplay: false, caption: "off" };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  const item = { id: 1, default_audio_index: 0, audio_tracks: [] };
  store.dispatch({ type: "PLAYBACK_SELECT", sessionId: 4, item, duration: 100 });
  store.dispatch({ type: "PLAYBACK_SOURCE", sessionId: 5, sourceMode: "direct", sourceReason: "test", segmentOffset: 0, start: 0, intent: "playing" });
  store.dispatch({ type: "PLAYBACK_STATUS", sessionId: 4, status: "error", error: { message: "stale" } });
  assert.equal(store.getState().playback.sessionId, 5);
  assert.equal(store.getState().playback.status, "loading");
  assert.equal(store.getState().playback.error, null);
});
