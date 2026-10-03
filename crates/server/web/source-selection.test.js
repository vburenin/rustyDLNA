import assert from "node:assert/strict";
import test from "node:test";
import { SourceSelector } from "./source-selection.js";
import { initialState } from "./store.js";

// These fixed expectations describe Android's published source policy. The
// external boundaries advertise codec support; they never return a source plan.
test("Android MSE independently copies streams, preserves Auto copies, and selects portable 720p only for Auto encodes", async (t) => {
  const saved = Object.fromEntries(["navigator", "MediaSource"].map((key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  t.after(() => {
    for (const [key, descriptor] of Object.entries(saved)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else delete globalThis[key];
    }
  });
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: {
    userAgent: "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Mobile Safari/537.36",
  } });
  let rejectCopiedType = false;
  globalThis.MediaSource = class {
    static isTypeSupported(type) { return type.includes("avc1.") && !(rejectCopiedType && type.includes("avc1.640028")); }
  };
  const profiles = [
    { id: "auto", max_width: 3840, max_height: 2160, max_video_kbps: 25000, audio_kbps: 192 },
    { id: "full_hd", max_width: 1920, max_height: 1080, max_video_kbps: 8000, audio_kbps: 192 },
    { id: "data_saver", max_width: 1280, max_height: 720, max_video_kbps: 3000, audio_kbps: 128, automatic_fallback: true },
    { id: "low_360", max_width: 640, max_height: 360, max_video_kbps: 800, audio_kbps: 96 },
  ];
  for (const [video, audio, preference, width, expectedVideo, expectedAudio, expectedQuality] of [
    ["h264", "aac", "auto", 640, "copy", "copy", "auto"],
    ["h264", "flac", "auto", 640, "copy", "transcode", "auto"],
    ["mpeg2video", "aac", "auto", 640, "transcode", "copy", "data_saver"],
    ["mpeg2video", "flac", "auto", 640, "transcode", "transcode", "data_saver"],
    ["mpeg2video", "aac", "full_hd", 1920, "transcode", "copy", "full_hd"],
    ["mpeg2video", "aac", "full_hd", 640, "transcode", "copy", "low_360"],
    ["h264", "aac", "data_saver", 1920, "transcode", "copy", "data_saver"],
  ]) {
    const item = { kind: "video", width, height: width * 9 / 16, video_codec: video, audio_codec: audio,
      video_content_type: video === "h264" ? 'video/mp4; codecs="avc1.640028"' : null,
      frame_rate: "24/1", hdr: "sdr", bit_depth: 8 };
    const state = initialState({}, { streamMode: "compat", quality: preference, encodingPreset: "balanced" });
    state.server.capabilities = { transcoding: true, quality_profiles: profiles, video_outputs: [] };
    state.playback.audioTracks = [{ index: 0, codec: audio, content_type: audio === "aac" ? 'audio/mp4; codecs="mp4a.40.2"' : 'audio/flac' }];
    const player = { canPlayType: (type) => /avc1\.|mp4a\./.test(type) ? "probably" : "" };
    const selector = new SourceSelector();
    const resolved = await selector.resolve(selector.prepare(item, state, player, {}), item, state, player);
    assert.equal(resolved.streamNegotiation.video, expectedVideo);
    assert.equal(resolved.streamNegotiation.audio, expectedAudio);
    assert.equal(resolved.outputQuality, expectedQuality);
    assert.equal(resolved.mediaSourceDelivery, true);
    assert.equal(state.preferences.quality, preference);
    if (video === "h264" && preference === "auto") {
      // A media-element claim alone cannot validate the exact copied MSE type.
      rejectCopiedType = true;
      const fallback = await selector.resolve(selector.prepare(item, state, player, {}), item, state, player);
      assert.equal(fallback.streamNegotiation.video, "transcode");
      assert.equal(fallback.outputQuality, "data_saver");
      rejectCopiedType = false;
    }
  }
});

test("compatible audio never uses the video-typed Media Source delivery", async (t) => {
  const saved = Object.fromEntries(["navigator", "MediaSource"].map((key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  t.after(() => {
    for (const [key, descriptor] of Object.entries(saved)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else delete globalThis[key];
    }
  });
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: {
    userAgent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36",
  } });
  const accepted = [];
  globalThis.MediaSource = class {
    static isTypeSupported(type) { accepted.push(type); return true; }
  };
  const state = initialState({}, { streamMode: "compat", quality: "auto", encodingPreset: "balanced" });
  state.server.capabilities = {
    transcoding: true,
    quality_profiles: [{ id: "auto", max_width: 3840, max_height: 2160, max_video_kbps: 25000, audio_kbps: 192 }],
    video_outputs: [{ id: "h264_sdr", mse_content_type: 'video/mp4; codecs="avc1.640033,mp4a.40.2"' }],
  };
  const player = { canPlayType: (type) => /mp4a\.|avc1\./.test(type) ? "probably" : "" };
  const selector = new SourceSelector();
  const resolve = (item) => selector.resolve(selector.prepare(item, state, player, {}), item, state, player);

  const audio = await resolve({ kind: "audio", audio_codec: "alac", duration_seconds: 240 });
  assert.equal(audio.sourceMode, "compatible");
  assert.equal(audio.mediaSourceDelivery, false);
  assert.equal(audio.mediaSourceType, null);
  assert.deepEqual(accepted, []);

  // Control: the same capabilities still select Media Source for video.
  const video = await resolve({ kind: "video", width: 1920, height: 1080, video_codec: "mpeg2video",
    audio_codec: "ac3", frame_rate: "24/1", hdr: "sdr", bit_depth: 8 });
  assert.equal(video.mediaSourceDelivery, true);
  assert.equal(video.mediaSourceType, 'video/mp4; codecs="avc1.640033,mp4a.40.2"');
});
