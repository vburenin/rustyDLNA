import test from "node:test";
import assert from "node:assert/strict";

import {
  aiUpscaleAvailable,
  aiUpscaleQualityAvailable,
  automaticCompatibleRecoveryProfile,
  audioDecodingConfiguration,
  audioTrackLabel,
  bufferedRangeSecondsAhead,
  chooseSource,
  compatibleSegmentStart,
  clockLabel,
  compatibleVideoDimensions,
  directSourceSupported,
  doubleTapSeekDelta,
  durationSeconds,
  fullscreenAction,
  hdrDisplaySupport,
  hdrVideoOutputCandidate,
  mediaMatchesQuery,
  nativeHlsQualityProfile,
  nativeHlsHevcCopyEligible,
  encodingPreset,
  navigationFromUrl,
  navigationUrl,
  negotiateCompatibleStreams,
  originalAudioTrackIndex,
  originalDownloadUrl,
  playbackControlLabel,
  playbackProcessing,
  primaryVideoCodec,
  queueNeighbor,
  reconcileQualityPreference,
  resumePosition,
  saferCompatibleQualityProfile,
  selectedAudioRequiresCompatible,
  isAndroidDevice,
  parseHlsMediaPlaylist,
  isAppleMobileDevice,
  isApplePhoneDevice,
  isSafariBrowser,
  seekTarget,
  sourceApplicableQualityProfiles,
  sourceAwareQualityProfileLabel,
  sourceBoundedQualityProfile,
  SOURCE_MODES,
  STREAM_MODES,
  timelineValueText,
  trickplayFrame,
  trickplayPreloadUrls,
  validDetailId,
  validQualityProfileId,
  videoDecodingConfiguration,
  videoOutputConfiguration,
} from "./core.js";
import { initialState, Store } from "./store.js";
import { loadPreferences, progressDetails, progressSnapshot } from "./preferences.js";

test("processing labels describe actual streams rather than the selected playback policy", () => {
  const playback = { sourceMode: SOURCE_MODES.COMPATIBLE, item: { kind: "video", audio_codec: "aac" } };
  assert.equal(playbackProcessing(playback).label, "Prepared streaming");
  for (const [video, audio, label] of [
    ["copy", "copy", "Repackaging"], ["copy", "transcode", "Converting audio"],
    ["transcode", "copy", "Re-encoding video"], ["transcode", "transcode", "Re-encoding video"],
    ["repair", "copy", "Re-encoding video"],
  ]) {
    assert.equal(playbackProcessing({ ...playback, streamNegotiation: { video, audio } }).label, label);
  }
  const encoded = { ...playback, streamNegotiation: { video: "transcode", audio: "transcode" } };
  assert.match(playbackProcessing(encoded).description, /Audio is also converted/);
  assert.equal(playbackProcessing({ ...encoded, sourceMode: SOURCE_MODES.ORIGINAL }).label, "Original file");
  assert.equal(playbackProcessing({ ...encoded, item: { kind: "audio" } }).label, "Converting audio");
  assert.equal(playbackProcessing({ ...playback, item: { kind: "audio" }, streamNegotiation: { video: "transcode", audio: "copy" } }).label, "Repackaging");
  assert.equal(playbackProcessing({ ...playback, item: { kind: "video" }, streamNegotiation: { video: "copy", audio: "transcode" } }).label, "Repackaging");
});

test("original downloads use only the advertised same-origin download route", () => {
  assert.equal(originalDownloadUrl({ download_url: "/web/download/9" }), "/web/download/9");
  for (const download_url of [null, "", 9, "https://example.com/web/download/9", "//example.com/web/download/9", "/MediaItems/9"]) {
    assert.equal(originalDownloadUrl({ download_url }), null);
  }
  assert.equal(originalDownloadUrl(null), null);
});

test("time conversion and seek bounds keep the real end position", () => {
  assert.equal(durationSeconds("1:02:03.5"), 3723.5);
  assert.equal(clockLabel(3723.9), "1:02:03");
  assert.equal(seekTarget(100, 100), 100);
  assert.equal(seekTarget(101, 100), 100);
  assert.equal(timelineValueText(100, 100), "1:40 of 1:40");
});

test("Media Source buffer accounting includes a small initial timestamp gap", () => {
  assert.equal(bufferedRangeSecondsAhead([{ start: 0.125, end: 31.125 }], 0), 31.125);
  assert.equal(bufferedRangeSecondsAhead([{ start: 10, end: 40 }], 0), 0);
  assert.equal(bufferedRangeSecondsAhead([{ start: 10, end: 40 }], 20), 20);
  assert.equal(bufferedRangeSecondsAhead([{ start: 10.5, end: 40 }], 10), 0);
  assert.equal(bufferedRangeSecondsAhead([{ start: 0, end: 10 }], 11), 0);
});

test("playback control preserves playing intent through transient states", () => {
  for (const status of ["loading", "waiting", "seeking", "playing"]) {
    assert.equal(playbackControlLabel(status, "playing"), "Pause");
  }
  for (const status of ["loading", "waiting", "seeking", "paused", "error"]) {
    assert.equal(playbackControlLabel(status, "paused"), "Play");
  }
  assert.equal(playbackControlLabel("paused", "playing"), "Play");
  assert.equal(playbackControlLabel("ended", "playing"), "Replay");
});

test("touch double taps seek thirty seconds only when both taps stay on the same side", () => {
  const base = { firstY: 100, secondY: 110, viewportLeft: 20, viewportWidth: 400 };
  assert.equal(doubleTapSeekDelta({ ...base, firstX: 330, secondX: 350 }), 30);
  assert.equal(doubleTapSeekDelta({ ...base, firstX: 100, secondX: 80 }), -30);
  assert.equal(doubleTapSeekDelta({ ...base, firstX: 200, secondX: 240 }), 0);
  assert.equal(doubleTapSeekDelta({ ...base, firstX: 300, secondX: 410 }), 30);
  assert.equal(doubleTapSeekDelta({ ...base, firstX: 300, secondX: 410, maximumDistance: 96 }), 0);
  assert.equal(doubleTapSeekDelta({ ...base, firstX: 300, secondX: 310, viewportWidth: 0 }), 0);
});

test("fullscreen prefers the expanded player when requested and otherwise chooses a browser API", () => {
  assert.equal(fullscreenAction({
    preferExpandedPlayer: true,
    stageSupported: true,
    nativeVideoSupported: true,
    videoSelected: true,
  }), "enter_expanded_player");
  assert.equal(fullscreenAction({ expandedPlayerActive: true }), "exit_expanded_player");
  assert.equal(fullscreenAction({ stageSupported: true, nativeVideoSupported: true, videoSelected: true }), "enter_stage");
  assert.equal(fullscreenAction({ nativeVideoSupported: true, videoSelected: true }), "enter_native_video");
  assert.equal(fullscreenAction({ nativeVideoSupported: true, videoSelected: false }), "unavailable");
  assert.equal(fullscreenAction({ stageActive: true, nativeVideoActive: true }), "exit_stage");
  assert.equal(fullscreenAction({ nativeVideoActive: true }), "exit_native_video");
});

test("trick-play selects the nearest bounded sprite frame", () => {
  const manifest = {
    schema_version: 2,
    available: true,
    interval_seconds: 3,
    frame_count: 105,
    frame_width: 960,
    frame_height: 540,
    columns: 3,
    rows: 7,
    sheet_urls: [
      "/web/preview/7/rev/0.jpg",
      "/web/preview/7/rev/1.jpg",
      "/web/preview/7/rev/2.jpg",
      "/web/preview/7/rev/3.jpg",
      "/web/preview/7/rev/4.jpg",
    ],
  };
  assert.deepEqual(trickplayFrame(manifest, 302), {
    frameIndex: 101,
    sheetIndex: 4,
    column: 2,
    row: 5,
    url: "/web/preview/7/rev/4.jpg",
  });
  assert.equal(trickplayFrame(manifest, 99_999).frameIndex, 104);
  assert.equal(trickplayFrame({ ...manifest, schema_version: 1 }, 10), null);
  assert.equal(trickplayFrame({ ...manifest, interval_seconds: 0 }, 10), null);
  assert.equal(trickplayFrame({ ...manifest, frame_count: 2401 }, 10), null);
  assert.equal(trickplayFrame({ ...manifest, frame_width: 4096 }, 10), null);
  assert.equal(trickplayFrame({ ...manifest, sheet_urls: ["https://example.test/x"] }, 10), null);

  const portrait = {
    ...manifest,
    interval_seconds: 5,
    frame_count: 1440,
    frame_height: 1706,
    columns: 3,
    rows: 2,
    sheet_urls: Array.from(
      { length: 240 },
      (_, index) => `/web/preview/7/rev/${index}.jpg`,
    ),
  };
  assert.deepEqual(trickplayFrame(portrait, 7199), {
    frameIndex: 1439,
    sheetIndex: 239,
    column: 2,
    row: 1,
    url: "/web/preview/7/rev/239.jpg",
  });
});

test("trick-play speculative caching is evenly bounded", () => {
  const urls = Array.from({ length: 115 }, (_, index) => `/sheet/${index}.jpg`);
  const selected = trickplayPreloadUrls(urls);
  assert.equal(selected.length, 8);
  assert.equal(selected[0], urls[0]);
  assert.equal(selected.at(-1), urls.at(-1));
  assert.equal(new Set(selected).size, selected.length);
  assert.deepEqual(trickplayPreloadUrls(urls.slice(0, 8)), urls.slice(0, 8));
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

test("progress snapshots parse browser storage once for repeated lookups", () => {
  let reads = 0;
  globalThis.localStorage = {
    getItem(key) {
      reads += 1;
      assert.equal(key, "rustydlna.webProgress.v1");
      return JSON.stringify({
        7: { position: "120", duration: 600, updated: 42 },
        bad: { position: "nope", duration: -1, updated: null },
      });
    },
  };
  try {
    const snapshot = progressSnapshot();
    assert.deepEqual(progressDetails(7, snapshot), { position: 120, duration: 600, updated: 42 });
    assert.deepEqual(progressDetails("bad", snapshot), { position: 0, duration: 0, updated: 0 });
    assert.deepEqual(progressDetails("missing", snapshot), { position: 0, duration: 0, updated: 0 });
    assert.equal(reads, 1);
  } finally {
    delete globalThis.localStorage;
  }
});

test("Continue Watching search matches the same displayed media fields", () => {
  const item = {
    file_name: "The.File.2026.mkv",
    title: "A Display Title",
    artist: "Example Artist",
    album_artist: "Album Ensemble",
    album: "Season One",
  };
  for (const query of ["", "the.file", "DISPLAY", "example", "ensemble", "season one"]) {
    assert.equal(mediaMatchesQuery(item, query), true, query);
  }
  assert.equal(mediaMatchesQuery(item, "missing title"), false);
});

test("source choice is explicit about forced and unavailable modes", () => {
  assert.deepEqual(chooseSource({ requestedMode: STREAM_MODES.AUTO, directSupport: true, transcoding: true }), {
    mode: SOURCE_MODES.ORIGINAL, reason: "browser_supported",
  });
  assert.deepEqual(chooseSource({ requestedMode: STREAM_MODES.AUTO, directSupport: false, transcoding: true }), {
    mode: SOURCE_MODES.COMPATIBLE, reason: "browser_support_uncertain",
  });
  assert.deepEqual(chooseSource({
    requestedMode: STREAM_MODES.AUTO,
    directSupport: true,
    transcoding: true,
    requiresCompatibleAudio: true,
  }), {
    mode: SOURCE_MODES.COMPATIBLE, reason: "preferred_audio",
  });
  assert.deepEqual(chooseSource({
    requestedMode: STREAM_MODES.AUTO,
    directSupport: true,
    transcoding: false,
    requiresCompatibleAudio: true,
  }), {
    mode: SOURCE_MODES.ORIGINAL, reason: "transcoding_disabled",
  });
  const unavailable = {
    mode: SOURCE_MODES.ORIGINAL,
    reason: "transcoding_disabled",
    blocked: "transcode_disabled",
  };
  assert.deepEqual(chooseSource({ requestedMode: STREAM_MODES.COMPATIBLE, directSupport: false, transcoding: false }), unavailable);
  assert.deepEqual(chooseSource({
    requestedMode: STREAM_MODES.ORIGINAL,
    forcedMode: SOURCE_MODES.COMPATIBLE,
    directSupport: true,
    transcoding: false,
  }), unavailable);
  assert.deepEqual(chooseSource({
    requestedMode: STREAM_MODES.COMPATIBLE,
    forcedMode: SOURCE_MODES.ORIGINAL,
    directSupport: false,
    transcoding: true,
    requiresCompatibleAudio: true,
  }), {
    mode: SOURCE_MODES.ORIGINAL,
    reason: "forced_original",
  });
});

test("direct support requires exact codecs for a server-identified compatibility video", () => {
  const broadMp4Probe = [];
  const mpeg4Part2 = {
    kind: "video",
    mime: "video/mp4",
    codec_string: null,
    transcode_likely: true,
  };
  assert.equal(directSourceSupported(mpeg4Part2, (contentType) => {
    broadMp4Probe.push(contentType);
    return "maybe";
  }), false);
  assert.deepEqual(broadMp4Probe, [], "container-only support must not override indexed video policy");

  assert.equal(directSourceSupported({
    ...mpeg4Part2,
    video_codec: "hevc",
    codec_string: "hvc1.2.4.L120.B0,mp4a.40.2",
  }, (contentType) => contentType.includes("hvc1") ? "probably" : ""), true);

  assert.equal(directSourceSupported({
    kind: "video",
    mime: "video/webm",
    codec_string: null,
    transcode_likely: false,
  }, (contentType) => contentType === "video/webm" ? "maybe" : ""), true);
});

test("audio selection distinguishes the preferred track from the original file default", () => {
  const tracks = [
    { index: 3, language: "jpn", default: true },
    { index: 7, language: "eng", default: false },
  ];
  assert.equal(originalAudioTrackIndex(tracks), 3);
  assert.equal(selectedAudioRequiresCompatible(tracks, 7), true);
  assert.equal(selectedAudioRequiresCompatible(tracks, 3), false);
  assert.equal(selectedAudioRequiresCompatible(tracks, 99), false);
  assert.equal(originalAudioTrackIndex([{ index: 4 }, { index: 9 }]), 4);
  assert.equal(originalAudioTrackIndex([]), 0);
});

test("quality preferences use bounded opaque IDs and follow advertised profiles", () => {
  const profiles = [{ id: "auto" }, { id: "future.4k-v2" }];
  assert.equal(validQualityProfileId("future.4k-v2"), true);
  assert.equal(validQualityProfileId(""), false);
  assert.equal(validQualityProfileId(`x${"y".repeat(64)}`), false);
  assert.equal(validQualityProfileId("bad\nvalue"), false);
  assert.equal(reconcileQualityPreference("future.4k-v2", profiles), "future.4k-v2");
  assert.equal(reconcileQualityPreference("removed", profiles), "auto");
  assert.equal(reconcileQualityPreference("removed", [{ id: "server-default" }]), "server-default");
  assert.equal(reconcileQualityPreference("future.4k-v2", []), "auto");
  assert.equal(reconcileQualityPreference("future.4k-v2", undefined), "future.4k-v2");

  globalThis.localStorage = {
    getItem(key) {
      return key === "rustydlna.quality" ? "future.4k-v2" : null;
    },
  };
  try {
    assert.equal(loadPreferences().quality, "future.4k-v2");
  } finally {
    delete globalThis.localStorage;
  }
});

test("native HLS HEVC copying is opt-in, Auto-only, and respects server eligibility", () => {
  const item = { kind: "video", video_codec: "hevc", video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"' };
  assert.equal(nativeHlsHevcCopyEligible(item, "auto", true), true);
  for (const [candidate, quality, enabled] of [
    [item, "auto", false], [item, "auto", "true"], [item, "full_hd", true],
    [{ ...item, kind: "audio" }, "auto", true],
    [{ ...item, video_codec: "h264" }, "auto", true],
    [{ ...item, video_content_type: null }, "auto", true],
    [{ ...item, video_repair_required: true }, "auto", true],
    [null, "auto", true],
  ]) assert.equal(nativeHlsHevcCopyEligible(candidate, quality, enabled), false);
});

test("encoding presets are validated, persisted, and gated by server support", () => {
  for (const id of ["balanced", "fast_start", "maximum_speed"]) {
    assert.equal(encodingPreset(id), id);
    assert.equal(encodingPreset(id, []), "balanced");
    assert.equal(encodingPreset(id, [{ id }]), id);
  }
  assert.equal(encodingPreset("unknown"), "balanced");
  try {
    for (const [stored, expected] of [[null, "balanced"], ["fast_start", "fast_start"], ["maximum_speed", "maximum_speed"], ["unknown", "balanced"]]) {
      globalThis.localStorage = { getItem: (key) => key === "rustydlna.encodingPreset" ? stored : null };
      assert.equal(loadPreferences().encodingPreset, expected);
    }
    globalThis.localStorage = { getItem: () => { throw new Error("blocked"); } };
    assert.equal(loadPreferences().encodingPreset, "balanced");
  } finally { delete globalThis.localStorage; }
});

test("HEVC HLS preference defaults off and handles unavailable storage", () => {
  try {
    for (const [stored, expected] of [[null, false], ["true", true], ["false", false], ["invalid", false]]) {
      globalThis.localStorage = { getItem: (key) => key === "rustydlna.hevcHlsCopy" ? stored : null };
      assert.equal(loadPreferences().hevcHlsCopy, expected);
    }
    globalThis.localStorage = { getItem: () => { throw new Error("blocked"); } };
    assert.equal(loadPreferences().hevcHlsCopy, false);
  } finally {
    delete globalThis.localStorage;
  }
});

test("compatible recovery selects the safest lower advertised quality without assuming profile IDs", () => {
  const profiles = [
    { id: "server-default", max_width: 3840, max_height: 2160, max_fps: 30, expected_bandwidth_kbps: 25_448 },
    { id: "medium", max_width: 1920, max_height: 1080, max_fps: 30, expected_bandwidth_kbps: 8_448 },
    { id: "phone-safe", max_width: 1280, max_height: 720, max_fps: 30, expected_bandwidth_kbps: 3_384 },
  ];
  assert.equal(saferCompatibleQualityProfile(profiles, "server-default"), "phone-safe");
  assert.equal(saferCompatibleQualityProfile(profiles, "medium"), "phone-safe");
  assert.equal(saferCompatibleQualityProfile(profiles, "phone-safe"), null);
  assert.equal(saferCompatibleQualityProfile([{ id: "unmeasured" }], "unmeasured"), null);
  assert.equal(saferCompatibleQualityProfile(undefined, "server-default"), null);

  const explicitLadder = [
    { ...profiles[0], id: "uhd" },
    { ...profiles[2], id: "mobile", automatic_fallback: true },
    { id: "low", max_width: 640, max_height: 360, max_fps: 30, expected_bandwidth_kbps: 1_152 },
  ];
  assert.equal(saferCompatibleQualityProfile(explicitLadder, "uhd"), "mobile");
  assert.equal(saferCompatibleQualityProfile(explicitLadder, "mobile"), null);
  assert.equal(saferCompatibleQualityProfile(explicitLadder, "low"), null);
  assert.equal(nativeHlsQualityProfile(explicitLadder, "auto", false), "auto");
  assert.equal(nativeHlsQualityProfile([
    { ...profiles[0], id: "auto" },
    profiles[2],
  ], "auto", true), "phone-safe");
  assert.equal(nativeHlsQualityProfile(explicitLadder, "uhd", true), "uhd");
  assert.equal(automaticCompatibleRecoveryProfile(explicitLadder, "uhd", "auto"), "mobile");
  assert.equal(automaticCompatibleRecoveryProfile(explicitLadder, "uhd", "uhd"), null);
});

test("quality choices stop at the source resolution without changing lower choices", () => {
  const profiles = [
    { id: "auto", max_width: 3840, max_height: 2160 },
    { id: "uhd-high", max_width: 3840, max_height: 2160 },
    { id: "uhd-small", max_width: 3840, max_height: 2160 },
    { id: "full-hd", max_width: 1920, max_height: 1080 },
    { id: "hd", max_width: 1280, max_height: 720 },
    { id: "sd", max_width: 854, max_height: 480 },
    { id: "low", label: "360p · 0.8 Mbps", max_width: 640, max_height: 360, max_video_kbps: 800 },
  ];
  const fullHd = { kind: "video", width: 1920, height: 1080 };
  assert.deepEqual(
    sourceApplicableQualityProfiles(profiles, fullHd).map((profile) => profile.id),
    ["auto", "full-hd", "hd", "sd", "low"],
  );
  assert.equal(sourceBoundedQualityProfile(profiles, "uhd-high", fullHd), "full-hd");
  assert.equal(sourceBoundedQualityProfile(profiles, "full-hd", fullHd), "full-hd");
  assert.equal(sourceBoundedQualityProfile(profiles, "hd", fullHd), "hd");
  assert.equal(sourceBoundedQualityProfile(profiles, "auto", fullHd), "auto");

  const uhd = { kind: "video", width: 3840, height: 2160 };
  assert.deepEqual(
    sourceApplicableQualityProfiles(profiles, uhd).map((profile) => profile.id),
    profiles.map((profile) => profile.id),
  );
  assert.equal(sourceBoundedQualityProfile(profiles, "uhd-small", uhd), "uhd-small");

  const tiny = { kind: "video", width: 32, height: 24 };
  assert.deepEqual(
    sourceApplicableQualityProfiles(profiles, tiny).map((profile) => profile.id),
    ["auto", "low"],
  );
  assert.equal(sourceBoundedQualityProfile(profiles, "uhd-high", tiny), "low");
  assert.equal(sourceAwareQualityProfileLabel(profiles, profiles[0], tiny), "Auto · up to 32×24");
  assert.equal(sourceAwareQualityProfileLabel(profiles, profiles.at(-1), tiny), "Source 32×24 · 0.8 Mbps");
  assert.deepEqual(compatibleVideoDimensions(tiny, profiles[1]), { width: 32, height: 24 });
});

test("AI quality choices require a measured 8-bit SDR envelope and stay within 2x", () => {
  const profiles = [
    { id: "auto", label: "Auto", max_width: 3840, max_height: 2160 },
    { id: "uhd", label: "4K", max_width: 3840, max_height: 2160 },
    { id: "full-hd", label: "1080p · 8 Mbps", max_width: 1920, max_height: 1080 },
    { id: "hd", label: "720p", max_width: 1280, max_height: 720 },
    { id: "sd", label: "480p", max_width: 854, max_height: 480 },
  ];
  const capability = {
    bit_depth: 8,
    max_scale: 2,
    profiles: [{
      name: "quality",
      max_source_width: 1920,
      max_source_height: 1080,
      max_source_pixels_per_second: 52_000_000,
    }],
  };
  const sdr = {
    kind: "video", width: 1280, height: 720, frame_rate: "24000/1001", hdr: "sdr", bit_depth: 8,
  };
  assert.equal(aiUpscaleAvailable(capability, sdr), true);
  assert.equal(aiUpscaleQualityAvailable(capability, sdr, profiles[2]), true);
  assert.equal(aiUpscaleQualityAvailable(capability, sdr, profiles[1]), false);
  assert.deepEqual(
    sourceApplicableQualityProfiles(profiles, sdr, capability).map((profile) => profile.id),
    ["auto", "full-hd", "hd", "sd"],
  );
  assert.equal(sourceBoundedQualityProfile(profiles, "full-hd", sdr, capability), "full-hd");
  assert.equal(sourceBoundedQualityProfile(profiles, "uhd", sdr, capability), "hd");
  assert.equal(
    sourceAwareQualityProfileLabel(profiles, profiles[2], sdr, capability),
    "1080p · 8 Mbps · AI upscale",
  );
  assert.deepEqual(
    compatibleVideoDimensions(sdr, profiles[2], capability),
    { width: 1920, height: 1080 },
  );
  assert.equal(aiUpscaleAvailable(capability, { ...sdr, hdr: "hdr10" }), false);
  assert.equal(aiUpscaleAvailable(capability, { ...sdr, hdr: "unknown" }), false);
  assert.equal(aiUpscaleAvailable(capability, { ...sdr, bit_depth: 10 }), false);
  assert.equal(aiUpscaleAvailable(capability, { ...sdr, frame_rate: "60/1" }), false);
  assert.equal(aiUpscaleAvailable(capability, { ...sdr, frame_rate: "24/1/2" }), false);
});

test("HDR output capability uses the exact advertised codec and treats display range as advisory", () => {
  assert.equal(hdrDisplaySupport((query) => ({ matches: query.includes(": high") })), true);
  assert.equal(hdrDisplaySupport((query) => ({ matches: query.includes(": standard") })), false);
  assert.equal(hdrDisplaySupport(null), null);

  const item = {
    kind: "video", video_codec: "hevc,other", hdr: "dv-p7", bit_depth: 10,
    width: 3840, height: 2160, frame_rate: "24000/1001",
  };
  const output = {
    id: "hevc_hdr10",
    video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
    color_gamut: "rec2020",
    transfer_function: "pq",
  };
  const profile = { max_width: 1920, max_height: 1080, max_fps: 30, max_video_kbps: 8_000 };
  assert.equal(primaryVideoCodec(item.video_codec), "hevc");
  assert.equal(hdrVideoOutputCandidate(item, [output]), output);
  assert.equal(hdrVideoOutputCandidate({ ...item, hdr: "dv-p5" }, [output]), null);
  assert.deepEqual(videoOutputConfiguration(output, profile, item), {
    type: "file",
    video: {
      contentType: output.video_content_type,
      width: 1920,
      height: 1080,
      bitrate: 8_000_000,
      framerate: 24000 / 1001,
      colorGamut: "rec2020",
      transferFunction: "pq",
    },
  });
});

test("Apple mobile detection includes iPad desktop mode without matching a Mac", () => {
  assert.equal(isAppleMobileDevice({
    userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1",
    platform: "iPhone",
    maxTouchPoints: 5,
  }), true);
  assert.equal(isAppleMobileDevice({
    userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1",
    platform: "MacIntel",
    maxTouchPoints: 5,
  }), true);
  assert.equal(isAppleMobileDevice({
    userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15",
    platform: "MacIntel",
    maxTouchPoints: 0,
  }), false);
  assert.equal(isAppleMobileDevice({
    userAgent: "Mozilla/5.0 (Linux; Android 15) AppleWebKit/537.36 Chrome/128 Mobile Safari/537.36",
    platform: "Linux armv8l",
    maxTouchPoints: 5,
  }), false);
});

test("Safari detection includes macOS Safari without matching Chromium", () => {
  assert.equal(isSafariBrowser({
    userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 Version/18.0 Safari/605.1.15",
  }), true);
  assert.equal(isSafariBrowser({
    userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/537.36 Chrome/128.0 Safari/537.36",
  }), false);
  assert.equal(isSafariBrowser({
    userAgent: "Mozilla/5.0 (Linux; Android 15) AppleWebKit/537.36 Chrome/128 Mobile Safari/537.36",
  }), false);
});

test("Apple phone detection selects expanded playback without matching iPad", () => {
  assert.equal(isApplePhoneDevice({
    userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1",
  }), true);
  assert.equal(isApplePhoneDevice({
    userAgent: "Mozilla/5.0 (iPad; CPU OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1",
  }), false);
  assert.equal(isApplePhoneDevice({
    userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1",
  }), false);
});

test("Android detection is limited to Android user agents", () => {
  assert.equal(isAndroidDevice({
    userAgent: "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36",
  }), true);
  assert.equal(isAndroidDevice({
    userAgent: "Mozilla/5.0 (Linux; U; en-US) AppleWebKit/537.36 Chrome/134.0.0.0 Mobile Safari/537.36",
  }), false);
  assert.equal(isAndroidDevice({
    userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1",
  }), false);
});

test("HLS media playlists expose only confined fixed fragmented-MP4 resources", () => {
  const playlist = [
    "#EXTM3U",
    '#EXT-X-MAP:URI="/web/media/42.mp4?request=7&delivery=hls_init&hls_offset=0&hls_length=1024"',
    "#EXTINF:2.000000,",
    "/web/media/42.m4s?request=7&delivery=hls_segment&hls_offset=1024&hls_length=4096",
    "#EXT-X-ENDLIST",
    "",
  ].join("\n");
  assert.deepEqual(parseHlsMediaPlaylist(playlist, "https://movies.example/web/media/42.m3u8"), {
    initUrl: "https://movies.example/web/media/42.mp4?request=7&delivery=hls_init&hls_offset=0&hls_length=1024",
    segmentUrls: ["https://movies.example/web/media/42.m4s?request=7&delivery=hls_segment&hls_offset=1024&hls_length=4096"],
    ended: true,
  });
  assert.equal(parseHlsMediaPlaylist(playlist.replace("/web/media/42.m4s", "https://evil.example/video.m4s"), "https://movies.example/playlist.m3u8"), null);
  assert.equal(parseHlsMediaPlaylist(playlist.replace("hls_length=4096", "hls_length=0"), "https://movies.example/playlist.m3u8"), null);
  assert.equal(parseHlsMediaPlaylist(playlist.replace("#EXTINF:2.000000,\n", ""), "https://movies.example/playlist.m3u8"), null);

  const msePlaylist = playlist
    .replace("delivery=hls_init", "delivery=mse_init")
    .replace("delivery=hls_segment", "delivery=mse_segment");
  assert.deepEqual(
    parseHlsMediaPlaylist(msePlaylist, "https://movies.example/web/media/42.m3u8?delivery=mse"),
    {
      initUrl: "https://movies.example/web/media/42.mp4?request=7&delivery=mse_init&hls_offset=0&hls_length=1024",
      segmentUrls: ["https://movies.example/web/media/42.m4s?request=7&delivery=mse_segment&hls_offset=1024&hls_length=4096"],
      ended: true,
    },
  );
  assert.equal(
    parseHlsMediaPlaylist(msePlaylist, "https://movies.example/web/media/42.m3u8?delivery=hls"),
    null,
  );

  const exhaustedMsePlaylist = [
    "#EXTM3U",
    "#EXT-X-MEDIA-SEQUENCE:3",
    '#EXT-X-MAP:URI="/web/media/42.mp4?request=7&delivery=mse_init&hls_offset=0&hls_length=1024"',
    "#EXT-X-ENDLIST",
    "",
  ].join("\n");
  assert.deepEqual(
    parseHlsMediaPlaylist(
      exhaustedMsePlaylist,
      "https://movies.example/web/media/42.m3u8?delivery=mse&mse_after=3",
    ),
    {
      initUrl: "https://movies.example/web/media/42.mp4?request=7&delivery=mse_init&hls_offset=0&hls_length=1024",
      segmentUrls: [],
      ended: true,
    },
  );
  assert.equal(
    parseHlsMediaPlaylist(exhaustedMsePlaylist, "https://movies.example/web/media/42.m3u8?delivery=hls"),
    null,
  );
});

test("detail IDs are canonical positive i64 decimal strings without safe-integer truncation", () => {
  for (const value of ["1", "9007199254740993", "9223372036854775807"]) {
    assert.equal(validDetailId(value), true, value);
  }
  for (const value of [0, "", "0", "01", "+1", "-1", "1.0", "9223372036854775808"]) {
    assert.equal(validDetailId(value), false, String(value));
  }
});

test("capability and quality changes advance the playback negotiation epoch", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "compat", quality: "auto", muted: false, loop: false, fill: false, autoplay: false };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  const publish = (requestId, capabilities) => {
    store.dispatch({ type: "LIBRARY_LOADING", requestId, append: false });
    store.dispatch({
      type: "LIBRARY_SUCCESS",
      requestId,
      append: false,
      payload: {
        server_name: "test",
        root_folder_id: "root",
        library_state: "empty",
        capabilities,
        entries: [],
        breadcrumbs: [],
        total: 0,
        offset: 0,
        has_more: false,
        generation: 1,
      },
    });
  };
  publish(1, { transcoding: true, quality_profiles: [{ id: "auto" }, { id: "full_hd" }] });
  assert.equal(store.getState().server.negotiationEpoch, 1);
  publish(2, { transcoding: true, quality_profiles: [{ id: "auto" }, { id: "full_hd" }] });
  assert.equal(store.getState().server.negotiationEpoch, 1);
  publish(3, {
    transcoding: true,
    quality_profiles: [{ id: "auto" }, { id: "full_hd" }],
    video_outputs: [{ id: "hevc_hdr10", video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"' }],
  });
  assert.equal(store.getState().server.negotiationEpoch, 2);
  store.dispatch({ type: "PREFERENCE", name: "quality", value: "full_hd" });
  assert.equal(store.getState().server.negotiationEpoch, 3);
  publish(4, { transcoding: false, quality_profiles: [{ id: "auto" }] });
  assert.equal(store.getState().server.negotiationEpoch, 4);
});

test("compatible playback negotiates video and audio independently", async () => {
  const item = {
    kind: "video",
    video_codec: "hevc",
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
    videoOutput: "h264_sdr",
    hdrDisplay: null,
    videoContentType: item.video_content_type,
    audioContentType: ac3.content_type,
    outputVideoContentType: null,
    videoProbe: { supported: true, canPlayType: "probably", mediaCapabilities: "supported" },
    audioProbe: { supported: false, canPlayType: "unsupported", mediaCapabilities: "unsupported" },
    outputVideoProbe: { supported: false, canPlayType: "not tested", mediaCapabilities: "not tested" },
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

  const hdrOutput = {
    id: "hevc_hdr10",
    video_content_type: 'video/mp4; codecs="hvc1.2.4.L153.B0"',
  };
  const hdrTranscode = await negotiateCompatibleStreams({
    item: { ...item, bit_depth: 10 },
    track: ac3,
    quality: "full_hd",
    qualityProfile: { max_width: 1920, max_height: 1080, max_fps: 30, max_video_kbps: 8_000 },
    videoOutputs: [hdrOutput],
    // Safari may report standard range even while its HEVC decoder accepts
    // HDR and can tone-map it for the current display.
    hdrDisplay: false,
    canPlayType: (contentType) => contentType.includes("hvc1") ? "probably" : "",
    decodingInfo: async (configuration) => ({ supported: Boolean(configuration.video) }),
  });
  assert.equal(hdrTranscode.videoOutput, "hevc_hdr10");
  assert.equal(hdrTranscode.hdrDisplay, false);
  assert.equal(hdrTranscode.outputVideoContentType, hdrOutput.video_content_type);
  assert.equal(hdrTranscode.outputVideoProbe.supported, true);

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

test("portable frame-order repair does not require a source video capability probe", async () => {
  const track = { codec: "aac", content_type: 'audio/mp4; codecs="mp4a.40.2"', channels: 2 };
  for (const [hdr, repairEncoder] of [["dv-p5", "libx264"], ["dv-p7", "h264_nvenc"]]) {
    const probed = [];
    const negotiated = await negotiateCompatibleStreams({
      item: {
        kind: "video",
        video_codec: "h264",
        hdr,
        video_content_type: null,
        video_repair_required: true,
        repair_video_encoder: repairEncoder,
      },
      track,
      quality: "auto",
      canPlayType: (contentType) => {
        probed.push(contentType);
        return contentType === track.content_type ? "probably" : "";
      },
    });

    assert.equal(negotiated.video, "repair", `${hdr} ${repairEncoder}`);
    assert.deepEqual(negotiated.videoProbe, {
      supported: false,
      canPlayType: "not tested",
      mediaCapabilities: "not tested",
    });
    assert.deepEqual(probed, [track.content_type]);

    const resized = await negotiateCompatibleStreams({
      item: {
        kind: "video",
        video_codec: "h264",
        hdr,
        video_content_type: null,
        video_repair_required: true,
        repair_video_encoder: repairEncoder,
      },
      track,
      quality: "full_hd",
      canPlayType: () => "probably",
    });
    assert.equal(resized.video, "transcode", `explicit quality for ${hdr}`);
  }
});

test("HEVC frame-order repair remains browser capability gated", async () => {
  const track = { codec: "aac", content_type: 'audio/mp4; codecs="mp4a.40.2"', channels: 2 };
  for (const hdr of ["hdr10", "dv-p8"]) {
    const item = {
      kind: "video",
      video_codec: "hevc",
      hdr,
      video_content_type: 'video/mp4; codecs="hvc1.2.4.H153.90"',
      video_repair_required: true,
      repair_video_encoder: "hevc_nvenc",
    };
    const unsupported = await negotiateCompatibleStreams({
      item,
      track,
      quality: "auto",
      canPlayType: (contentType) => contentType === track.content_type ? "probably" : "",
      decodingInfo: async () => ({ supported: false }),
    });
    assert.equal(unsupported.video, "transcode", `unsupported ${hdr}`);

    const supported = await negotiateCompatibleStreams({
      item,
      track,
      quality: "auto",
      canPlayType: (contentType) => contentType.includes("hvc1") ? "probably" : "",
      decodingInfo: async () => ({ supported: false }),
    });
    assert.equal(supported.video, "repair", `supported ${hdr}`);
  }
});

test("non-H.264/HEVC timestamp damage uses normal transcoding instead of repair mode", async () => {
  const negotiated = await negotiateCompatibleStreams({
    item: {
      kind: "video",
      video_codec: "mpeg4",
      video_content_type: null,
      video_repair_required: true,
      repair_video_encoder: "h264_nvenc",
    },
    track: { codec: "ac3", content_type: 'audio/mp4; codecs="ac-3"', channels: 6 },
    quality: "auto",
    canPlayType: () => "",
  });
  assert.equal(negotiated.video, "transcode");
  assert.equal(negotiated.audio, "transcode");
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
    videoOutput: "h264_sdr",
    hdrDisplay: null,
    videoContentType: null,
    audioContentType: track.content_type,
    outputVideoContentType: null,
    videoProbe: { supported: false, canPlayType: "not tested", mediaCapabilities: "not tested" },
    audioProbe: { supported: true, canPlayType: "probably", mediaCapabilities: "error" },
    outputVideoProbe: { supported: false, canPlayType: "not tested", mediaCapabilities: "not tested" },
  });
  assert.equal(probes, 1);
});

test("compatible negotiation does not wait indefinitely for Media Capabilities", async () => {
  const item = {
    kind: "video",
    video_codec: "h264",
    video_content_type: 'video/mp4; codecs="avc1.640028"',
    width: 1920,
    height: 1080,
    bitrate: 1_000_000,
    frame_rate: "30/1",
  };
  const track = {
    codec: "ac3",
    content_type: 'audio/mp4; codecs="ac-3"',
    channels: 6,
  };
  const negotiated = await negotiateCompatibleStreams({
    item,
    track,
    quality: "auto",
    canPlayType: (contentType) => contentType.includes("avc1") ? "probably" : "",
    decodingInfo: () => new Promise(() => {}),
    decodingInfoTimeoutMs: 10,
  });

  assert.equal(negotiated.video, "copy");
  assert.equal(negotiated.audio, "transcode");
  assert.equal(negotiated.videoProbe.mediaCapabilities, "timed out");
  assert.equal(negotiated.audioProbe.mediaCapabilities, "timed out");
});

test("queue navigation is stable by item ID", () => {
  const queue = [{ id: "3" }, { id: "8" }, { id: "13" }];
  assert.equal(queueNeighbor(queue, "8", -1).id, "3");
  assert.equal(queueNeighbor(queue, "8", 1).id, "13");
  assert.equal(queueNeighbor(queue, "13", 1), null);
});

test("track labels include language, title, codec, and channel layout", () => {
  assert.equal(
    audioTrackLabel({ index: 1, language: "eng", title: "Commentary", default: true, codec: "ac3", channels: 6 }),
    "ENG · Commentary · Default · AC-3 5.1",
  );
});

test("audio enrichment adopts the newly discovered preferred track", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "auto", quality: "auto", muted: false, loop: false, fill: false, autoplay: false, caption: "off" };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  const item = { id: "1", default_audio_index: 0, audio_tracks: [] };
  store.dispatch({ type: "PLAYBACK_SELECT", sessionId: 4, item, duration: 100 });
  store.dispatch({ type: "AUDIO_TRACKS_LOADING", sessionId: 4 });
  store.dispatch({
    type: "AUDIO_TRACKS_SUCCESS",
    sessionId: 4,
    item: { ...item, default_audio_index: 2 },
    tracks: [{ index: 0, default: true }, { index: 2, language: "eng" }],
    chapters: [],
  });
  assert.equal(store.getState().playback.selectedAudio, 2);

  store.dispatch({ type: "PLAYBACK_AUX", sessionId: 4, values: { selectedAudio: 0 } });
  store.dispatch({
    type: "AUDIO_TRACKS_SUCCESS",
    sessionId: 4,
    item: { ...item, default_audio_index: 3 },
    tracks: [{ index: 0, default: true }, { index: 3, language: "eng" }],
    chapters: [],
  });
  assert.equal(store.getState().playback.selectedAudio, 0, "an explicit selection is preserved");
});

test("URL state round-trips folder, search, sort, and flat views", () => {
  const folder = navigationFromUrl("http://server/?folder=abc&q=film&sort=date_desc");
  assert.deepEqual(folder, { view: "folders", folder: "abc", kind: "all", query: "film", sort: "date_desc", itemId: null, start: 0, layout: "browse" });
  assert.equal(navigationUrl("http://server/", folder, "root"), "/?folder=abc&q=film&sort=date_desc");
  assert.equal(
    navigationUrl("http://server/", { ...folder, layout: undefined }, "root"),
    "/?folder=abc&q=film&sort=date_desc",
  );
  const flat = navigationFromUrl("http://server/?view=audio");
  assert.equal(flat.view, "library");
  assert.equal(flat.kind, "audio");
  const continuing = navigationFromUrl("http://server/?view=continue");
  assert.equal(continuing.view, "continue");
  assert.equal(navigationUrl("http://server/", continuing, "root"), "/?view=continue");
  const deep = navigationFromUrl("http://server/?view=video&item=42&t=90");
  assert.equal(deep.itemId, "42");
  assert.equal(deep.start, 90);
  assert.equal(deep.layout, "watch");
  assert.equal(navigationUrl("http://server/", deep, "root"), "/?view=video&item=42&t=90");

  const browsingPlayback = navigationFromUrl("http://server/?view=video&item=42&layout=browse");
  assert.equal(browsingPlayback.layout, "browse");
  assert.equal(
    navigationUrl("http://server/", browsingPlayback, "root"),
    "/?view=video&item=42&layout=browse",
  );
  const emptyWatch = navigationFromUrl("http://server/?layout=watch");
  assert.equal(emptyWatch.layout, "watch");
  assert.equal(navigationUrl("http://server/", emptyWatch, "root"), "/?layout=watch");
  assert.equal(navigationFromUrl("http://server/?layout=unknown").layout, "browse");
});

test("clearing playback drops the title and ignores stale session events", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "auto", quality: "auto", muted: false, loop: false, fill: false, autoplay: false, caption: "off" };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  const item = { id: "1", default_audio_index: 0, audio_tracks: [] };
  store.dispatch({ type: "PLAYBACK_SELECT", sessionId: 4, item, duration: 100 });
  store.dispatch({ type: "QUEUE_REPLACE", entries: [item], generation: 7 });
  store.dispatch({ type: "PLAYBACK_CLEAR", sessionId: 5 });
  assert.equal(store.getState().playback.item, null);
  assert.equal(store.getState().playback.sessionId, 5);
  assert.equal(store.getState().playback.status, "idle");
  assert.deepEqual(store.getState().queue.entries, []);
  store.dispatch({ type: "PLAYBACK_STATUS", sessionId: 4, status: "playing" });
  store.dispatch({ type: "QUEUE_SUCCESS", requestId: store.getState().queue.requestId - 1, entries: [item], generation: 7 });
  assert.equal(store.getState().playback.status, "idle");
  assert.deepEqual(store.getState().queue.entries, []);
});

test("stale media events cannot mutate a newer playback session", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "auto", quality: "auto", muted: false, loop: false, fill: false, autoplay: false, caption: "off" };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  const item = { id: "1", default_audio_index: 0, audio_tracks: [] };
  store.dispatch({ type: "PLAYBACK_SELECT", sessionId: 4, item, duration: 100 });
  store.dispatch({ type: "PLAYBACK_SOURCE", sessionId: 5, sourceMode: "direct", sourceReason: "test", segmentOffset: 0, start: 0, intent: "playing" });
  store.dispatch({ type: "PLAYBACK_STATUS", sessionId: 4, status: "error", error: { message: "stale" } });
  store.dispatch({ type: "PLAYBACK_AUX", sessionId: 4, values: { pip: true, selectedAudio: 8 } });
  store.dispatch({ type: "PLAYBACK_PREVIEW", sessionId: 4, value: 42 });
  assert.equal(store.getState().playback.sessionId, 5);
  assert.equal(store.getState().playback.status, "loading");
  assert.equal(store.getState().playback.error, null);
  assert.equal(store.getState().playback.pip, false);
  assert.equal(store.getState().playback.selectedAudio, 0);
  assert.equal(store.getState().playback.previewTime, null);
});

test("transient playback status preserves an autoplay block until playback succeeds", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "auto", quality: "auto", muted: false, loop: false, fill: false, autoplay: false, caption: "off" };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  const item = { id: "1", default_audio_index: 0, audio_tracks: [] };
  store.dispatch({ type: "PLAYBACK_SELECT", sessionId: 4, item, duration: 100 });
  store.dispatch({ type: "PLAYBACK_SOURCE", sessionId: 5, sourceMode: "direct", sourceReason: "test", segmentOffset: 0, start: 0, intent: "playing" });
  store.dispatch({ type: "PLAYBACK_STATUS", sessionId: 5, status: "paused", intent: "paused", autoplayBlocked: true });
  store.dispatch({ type: "PLAYBACK_STATUS", sessionId: 5, status: "waiting", message: "Buffering" });
  assert.equal(store.getState().playback.autoplayBlocked, true);
  store.dispatch({ type: "PLAYBACK_STATUS", sessionId: 5, status: "playing", intent: "playing", autoplayBlocked: false });
  assert.equal(store.getState().playback.autoplayBlocked, false);
});

test("stale queue completions cannot replace a newer queue epoch", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "auto", quality: "auto", muted: false, loop: false, fill: false, autoplay: false };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  store.dispatch({ type: "QUEUE_LOADING", requestId: 1, entries: [{ id: "old-head" }], generation: 10 });
  store.dispatch({ type: "QUEUE_LOADING", requestId: 2, entries: [{ id: "new-head" }], generation: 11 });
  store.dispatch({ type: "QUEUE_SUCCESS", requestId: 1, entries: [{ id: "old-tail" }], generation: 10 });
  store.dispatch({ type: "QUEUE_ERROR", requestId: 1, error: new Error("stale") });
  assert.deepEqual(store.getState().queue.entries, [{ id: "new-head" }]);
  assert.equal(store.getState().queue.status, "loading");
  assert.equal(store.getState().queue.error, null);
  store.dispatch({ type: "QUEUE_SUCCESS", requestId: 2, entries: [{ id: "new-tail" }], generation: 11 });
  assert.deepEqual(store.getState().queue.entries, [{ id: "new-tail" }]);
  assert.equal(store.getState().queue.status, "ready");

  store.dispatch({ type: "QUEUE_LOADING", requestId: 3, entries: [{ id: "pending" }], generation: 12 });
  store.dispatch({ type: "QUEUE_REPLACE", entries: [{ id: "linked" }], generation: null });
  store.dispatch({ type: "QUEUE_SUCCESS", requestId: 3, entries: [{ id: "late" }], generation: 12 });
  assert.deepEqual(store.getState().queue.entries, [{ id: "linked" }]);
});

test("captions reset per title but survive playback source restarts", () => {
  const preferences = { rate: 1, volume: 100, streamMode: "auto", quality: "auto", muted: false, loop: false, fill: false, autoplay: false, caption: "legacy-index" };
  const store = new Store(initialState({ view: "folders", folder: null, kind: "all", query: "", sort: "title" }, preferences));
  const first = { id: "1", default_audio_index: 0, audio_tracks: [] };
  const second = { id: "2", default_audio_index: 0, audio_tracks: [] };

  assert.equal(store.getState().playback.selectedCaption, "off");
  store.dispatch({ type: "PLAYBACK_SELECT", sessionId: 1, item: first, duration: 100 });
  assert.equal(store.getState().playback.selectedCaption, "off");
  store.dispatch({ type: "PLAYBACK_AUX", sessionId: 1, values: { selectedCaption: "3" } });
  store.dispatch({ type: "PLAYBACK_SOURCE", sessionId: 2, sourceMode: "compatible", sourceReason: "test", segmentOffset: 0, start: 0, intent: "playing" });
  assert.equal(store.getState().playback.selectedCaption, "3");

  store.dispatch({ type: "PLAYBACK_SELECT", sessionId: 3, item: second, duration: 100 });
  assert.equal(store.getState().playback.selectedCaption, "off");
});
