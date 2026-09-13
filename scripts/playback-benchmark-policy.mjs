// Independent benchmark oracle for docs/WEB_PLAYER.md's Android MSE contract.
// Do not import the application's source/quality decision functions here: a
// policy regression in those functions must still fail output validation.
const envelopes = {
  auto: [3840, 2160], uhd_high: [3840, 2160], uhd_optimized: [3840, 2160],
  full_hd: [1920, 1080], data_saver: [1280, 720], sd_480: [854, 480], low_360: [640, 360],
};

export function expectedAudioTrack(streams) {
  const tracks = streams.filter((stream) => stream.codec_type === "audio");
  const english = (stream) => /^(en|eng)([-_].*)?$|^english$/i.test(String(stream.tags?.language || "").trim());
  const selected = tracks.find((stream) => stream.disposition?.default === 1 && english(stream))
    || tracks.find(english) || tracks.find((stream) => stream.disposition?.default === 1)
    || tracks.find((stream) => ["aac", "ac3", "eac3", "mp3"].includes(stream.codec_name)) || tracks[0];
  if (!selected) throw new Error("No audio track for the benchmark recipe");
  return { stream: selected, ordinal: tracks.indexOf(selected) };
}

export function expectedAndroidOutput({ recipe, preference, sourceVideo, sourceAudio, profiles, copySupport, hdrSupport = false, audioSupport = true }) {
  const envelope = envelopes[preference];
  if (!envelope || !(sourceVideo?.width > 0 && sourceVideo?.height > 0)) throw new Error("Source dimensions/quality unavailable for independent benchmark policy");
  if (!Array.isArray(profiles) || !profiles.some((profile) => profile.id === preference)) throw new Error("Requested quality is not advertised");
  let quality = preference;
  if (preference !== "auto" && envelope[0] >= sourceVideo.width && envelope[1] >= sourceVideo.height) {
    // Explicit preferences cap at the smallest advertised source-preserving
    // envelope. Keep the preference when several presets share that envelope.
    const fitting = Object.entries(envelopes).filter(([id, [width, height]]) => id !== "auto"
      && profiles.some((profile) => profile.id === id) && width >= sourceVideo.width && height >= sourceVideo.height)
      .sort((a, b) => a[1][0] * a[1][1] - b[1][0] * b[1][1]);
    if (fitting[0] && fitting[0][1][0] * fitting[0][1][1] < envelope[0] * envelope[1]) quality = fitting[0][0];
  }
  const videoCopy = preference === "auto" && (recipe === "external" ? copySupport === true : ["copy", "audio"].includes(recipe));
  const audioCopy = recipe === "external" ? sourceAudio?.codec_name === "aac" && audioSupport : ["copy", "video"].includes(recipe);
  if (preference === "auto" && !videoCopy && !hdrSupport) {
    // The shipped portable Android Auto profile is 720p, including sources
    // smaller than 720p. No second explicit-preference cap follows this step.
    const mobile = profiles.find((profile) => profile.id === "data_saver");
    if (mobile?.automatic_fallback !== true) throw new Error("Documented Android 720p automatic fallback is unavailable");
    quality = "data_saver";
  }
  const [width, height] = envelopes[quality];
  const advertised = profiles.find((profile) => profile.id === quality);
  if (advertised?.max_width !== width || advertised?.max_height !== height) throw new Error(`Advertised ${quality} envelope differs from documented policy`);
  return { quality, video_mode: videoCopy ? "copy" : "transcode", audio_mode: audioCopy ? "copy" : "transcode",
    video_output: videoCopy ? null : hdrSupport ? "hevc_hdr10" : "h264_sdr", max_width: width, max_height: height };
}

export function assertExpectedRequest(expected, requested, preset) {
  for (const key of ["quality", "video_mode", "audio_mode"]) {
    if ((requested[key] ?? (key === "quality" ? "auto" : null)) !== expected[key]) {
      throw new Error(`Expected Android ${key}=${expected[key]}, received ${JSON.stringify(requested)}`);
    }
  }
  if (expected.video_mode === "transcode"
    && ((requested.encoding_preset || "balanced") !== preset || requested.video_output !== expected.video_output)) {
    throw new Error(`Expected Android ${expected.video_output} with preset ${preset}, received ${JSON.stringify(requested)}`);
  }
}

export function assertExpectedVideo(expected, video, sourceVideo) {
  if (expected.video_mode === "copy") return; // Decoded hashes and source dimensions are checked by the caller.
  const scale = Math.min(1, expected.max_width / sourceVideo.width, expected.max_height / sourceVideo.height);
  const rate = (stream) => { const [n, d = 1] = String(stream.r_frame_rate).split("/").map(Number); return n / d; };
  const hdr = expected.video_output === "hevc_hdr10";
  if (![video?.width, video?.height, sourceVideo?.width, sourceVideo?.height, rate(video || {}), rate(sourceVideo || {})]
    .every((value) => Number.isFinite(value) && value > 0)
    || video?.codec_name !== (hdr ? "hevc" : "h264") || video.pix_fmt !== (hdr ? "yuv420p10le" : "yuv420p")
    || (hdr && (video.color_transfer !== "smpte2084" || video.color_primaries !== "bt2020"))
    || Math.abs(video.width - sourceVideo.width * scale) > 2 || Math.abs(video.height - sourceVideo.height * scale) > 2
    || video.width > sourceVideo.width || video.height > sourceVideo.height
    || !Number.isFinite(rate(video)) || Math.abs(rate(video) - Math.min(rate(sourceVideo), 30)) > 0.02
    || (!hdr && ["data_saver", "sd_480", "low_360"].includes(expected.quality) && !String(video.profile).includes("Baseline"))) {
    throw new Error(`Encoded video differs from the independent quality envelope: ${JSON.stringify(video)}`);
  }
}
