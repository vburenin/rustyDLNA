export const PLAYBACK_STATES = Object.freeze([
  "idle", "loading", "waiting", "playing", "paused", "seeking", "ended", "error",
]);

export function playbackControlLabel(status, intent) {
  if (status === "ended") return "Replay";
  if (status === "playing"
    || (intent === "playing" && ["loading", "waiting", "seeking"].includes(status))) {
    return "Pause";
  }
  return "Play";
}

export const STREAM_MODES = Object.freeze({
  AUTO: "auto",
  ORIGINAL: "direct",
  COMPATIBLE: "compat",
});

export const SOURCE_MODES = Object.freeze({
  ORIGINAL: "direct",
  COMPATIBLE: "compatible",
});

export const LAYOUT_MODES = Object.freeze({
  BROWSE: "browse",
  WATCH: "watch",
});

const MAX_QUALITY_PROFILE_ID_LENGTH = 64;
const MAX_DETAIL_ID = "9223372036854775807";
const MEDIA_CAPABILITIES_TIMEOUT_MS = 1_000;

export function clamp(value, minimum, maximum) {
  return Math.min(maximum, Math.max(minimum, value));
}

export function durationSeconds(value) {
  if (!value) return 0;
  const exact = Number(value);
  if (Number.isFinite(exact) && exact > 0) return exact;
  const parts = String(value).split(":").map(Number);
  if (parts.length !== 3 || parts.some((part) => !Number.isFinite(part))) return 0;
  return parts[0] * 3600 + parts[1] * 60 + parts[2];
}

export function itemDuration(item, mediaDuration = 0) {
  const catalog = durationSeconds(item?.duration_seconds || item?.duration);
  if (catalog > 0) return catalog;
  return Number.isFinite(mediaDuration) && mediaDuration > 0 ? mediaDuration : 0;
}

export function bufferedRangeSecondsAhead(ranges, currentTime, startTolerance = 1) {
  const current = Number(currentTime);
  const tolerance = Math.max(0, Number(startTolerance) || 0);
  if (!Number.isFinite(current) || current < 0 || !Array.isArray(ranges)) return 0;
  for (const range of ranges) {
    const start = Number(range?.start);
    const end = Number(range?.end);
    if (!Number.isFinite(start) || !Number.isFinite(end) || !(end >= current)) continue;
    // A freshly appended fMP4 can begin a fraction of a second after zero due
    // to audio priming or reordered video. Count that near-future range while
    // the media clock is still at zero so the MSE pump cannot race past its
    // buffer limit before playback advances into the range.
    if (start <= current
      || (current <= tolerance && start <= current + tolerance)) {
      return Math.max(0, end - current);
    }
  }
  return 0;
}

export function clockLabel(value) {
  const total = Math.max(0, Math.floor(Number(value) || 0));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`
    : `${minutes}:${String(seconds).padStart(2, "0")}`;
}

export function timelineValueText(current, duration) {
  return duration > 0
    ? `${clockLabel(current)} of ${clockLabel(duration)}`
    : clockLabel(current);
}

export function trickplayFrame(manifest, seconds) {
  const schemaVersion = Number(manifest?.schema_version);
  const interval = Number(manifest?.interval_seconds);
  const frameCount = Number(manifest?.frame_count);
  const columns = Number(manifest?.columns);
  const rows = Number(manifest?.rows);
  const width = Number(manifest?.frame_width);
  const height = Number(manifest?.frame_height);
  const urls = manifest?.sheet_urls;
  const framePixels = width * height;
  const sheetWidth = width * columns;
  const sheetHeight = height * rows;
  const sheetPixels = sheetWidth * sheetHeight;
  if (schemaVersion !== 2 || manifest?.available !== true
    || !Number.isInteger(interval) || interval < 1
    || !Number.isInteger(frameCount) || frameCount < 1 || frameCount > 2400
    || !Number.isInteger(width) || width < 16 || width > 4096
    || !Number.isInteger(height) || height < 16 || height > 4096
    || !Number.isInteger(columns) || columns < 1 || columns > 10
    || !Number.isInteger(rows) || rows < 1 || rows > 10
    || framePixels > 4_194_304 || sheetWidth > 4096 || sheetHeight > 4096
    || sheetPixels > 12_000_000
    || !Array.isArray(urls) || urls.length !== Math.ceil(frameCount / (columns * rows))
    || urls.length > 256
    || !Number.isFinite(Number(seconds))) return null;
  const framesPerSheet = columns * rows;
  const frameIndex = Math.min(frameCount - 1, Math.max(0, Math.round(Number(seconds) / interval)));
  const sheetIndex = Math.floor(frameIndex / framesPerSheet);
  const slot = frameIndex % framesPerSheet;
  const url = urls[sheetIndex];
  if (typeof url !== "string" || !url.startsWith("/web/preview/")) return null;
  return {
    frameIndex,
    sheetIndex,
    column: slot % columns,
    row: Math.floor(slot / columns),
    url,
  };
}

export function trickplayPreloadUrls(urls) {
  if (!Array.isArray(urls)) return [];
  const limit = 8;
  if (urls.length <= limit) return [...urls];
  return Array.from({ length: limit }, (_, index) => (
    urls[Math.floor(index * (urls.length - 1) / (limit - 1))]
  ));
}

export function seekTarget(value, duration) {
  const numeric = Math.max(0, Number(value) || 0);
  return duration > 0 ? Math.min(numeric, duration) : numeric;
}

export function doubleTapSeekDelta({
  firstX,
  firstY,
  secondX,
  secondY,
  viewportLeft = 0,
  viewportWidth,
  maximumDistance = 160,
  seconds = 30,
} = {}) {
  const values = [firstX, firstY, secondX, secondY, viewportLeft, viewportWidth, maximumDistance, seconds];
  if (!values.every(Number.isFinite) || !(viewportWidth > 0) || !(maximumDistance >= 0) || !(seconds > 0)) return 0;
  if (Math.hypot(secondX - firstX, secondY - firstY) > maximumDistance) return 0;
  const midpoint = viewportLeft + viewportWidth / 2;
  if (firstX < midpoint && secondX < midpoint) return -seconds;
  if (firstX >= midpoint && secondX >= midpoint) return seconds;
  return 0;
}

// Caption files use global title time; restarted compatible sources use a
// local timeline. Drop expired cues and clip cues crossing the source start.
export function captionCueWindow(start, end, segmentOffset = 0) {
  if (![start, end, segmentOffset].every(Number.isFinite)
    || start < 0 || end <= start || segmentOffset < 0 || end <= segmentOffset) return null;
  return { start: Math.max(0, start - segmentOffset), end: end - segmentOffset };
}

export function compatibleSegmentStart(value, bucketSeconds = 10) {
  const target = Math.max(0, Math.floor(Number(value) || 0));
  const bucket = Math.max(1, Math.floor(Number(bucketSeconds) || 1));
  return Math.floor(target / bucket) * bucket;
}

export function fullscreenAction({
  stageActive = false,
  nativeVideoActive = false,
  expandedPlayerActive = false,
  preferExpandedPlayer = false,
  stageSupported = false,
  nativeVideoSupported = false,
  videoSelected = false,
} = {}) {
  if (stageActive) return "exit_stage";
  if (nativeVideoActive) return "exit_native_video";
  if (expandedPlayerActive) return "exit_expanded_player";
  if (videoSelected && preferExpandedPlayer) return "enter_expanded_player";
  if (stageSupported) return "enter_stage";
  if (videoSelected && nativeVideoSupported) return "enter_native_video";
  return "unavailable";
}

export function resumePosition(position, duration) {
  const current = Number(position);
  if (!Number.isFinite(current) || current < 30 || !(duration > 0)) return 0;
  const remaining = duration - current;
  if (remaining <= 120 || current / duration >= 0.95) return 0;
  return Math.min(current, duration);
}

export function sourceMime(item) {
  if (item?.codec_string) return `${item.mime}; codecs="${item.codec_string}"`;
  return item?.mime || "";
}

export function directSourceSupported(item, canPlayType) {
  const hasExactCodecs = typeof item?.codec_string === "string"
    && item.codec_string.trim() !== "";
  // A container-only query is too broad when the server has already identified
  // a video that normally needs conversion. In particular, Chromium reports
  // MP4 support without proving it can decode an MPEG-4 Part 2 video stream.
  if (item?.kind === "video" && item.transcode_likely === true && !hasExactCodecs) return false;
  if (typeof canPlayType !== "function") return false;
  try {
    return Boolean(canPlayType(sourceMime(item)));
  } catch (_) {
    return false;
  }
}

export function originalAudioTrackIndex(tracks) {
  if (!Array.isArray(tracks) || tracks.length === 0) return 0;
  const original = tracks.find((track) => track?.default === true) || tracks[0];
  const index = Number(original?.index);
  return Number.isInteger(index) && index >= 0 ? index : 0;
}

export function selectedAudioRequiresCompatible(tracks, selectedAudio) {
  if (!Array.isArray(tracks) || tracks.length < 2) return false;
  const selected = tracks.find((track) => Number(track?.index) === Number(selectedAudio));
  return Boolean(selected) && Number(selected.index) !== originalAudioTrackIndex(tracks);
}

export function chooseSource({
  requestedMode,
  forcedMode = null,
  directSupport,
  transcoding,
  requiresCompatibleAudio = false,
}) {
  if (forcedMode === SOURCE_MODES.ORIGINAL) {
    return { mode: SOURCE_MODES.ORIGINAL, reason: "forced_original" };
  }
  if (forcedMode === SOURCE_MODES.COMPATIBLE) {
    return transcoding
      ? { mode: SOURCE_MODES.COMPATIBLE, reason: "forced_compatible" }
      : { mode: SOURCE_MODES.ORIGINAL, reason: "transcoding_disabled", blocked: "transcode_disabled" };
  }
  if (requestedMode === STREAM_MODES.ORIGINAL) {
    return { mode: SOURCE_MODES.ORIGINAL, reason: "forced_original" };
  }
  if (requestedMode === STREAM_MODES.COMPATIBLE) {
    return transcoding
      ? { mode: SOURCE_MODES.COMPATIBLE, reason: "forced_compatible" }
      : { mode: SOURCE_MODES.ORIGINAL, reason: "transcoding_disabled", blocked: "transcode_disabled" };
  }
  if (requiresCompatibleAudio) {
    return transcoding
      ? { mode: SOURCE_MODES.COMPATIBLE, reason: "preferred_audio" }
      : { mode: SOURCE_MODES.ORIGINAL, reason: "transcoding_disabled" };
  }
  if (directSupport) return { mode: SOURCE_MODES.ORIGINAL, reason: "browser_supported" };
  return transcoding
    ? { mode: SOURCE_MODES.COMPATIBLE, reason: "browser_support_uncertain" }
    : { mode: SOURCE_MODES.ORIGINAL, reason: "transcoding_disabled" };
}

export function validQualityProfileId(value) {
  return typeof value === "string"
    && value.length > 0
    && value.length <= MAX_QUALITY_PROFILE_ID_LENGTH
    && value.trim() === value
    && !/[\u0000-\u001f\u007f]/.test(value);
}

export function validDetailId(value) {
  if (typeof value !== "string" || !/^[1-9]\d*$/.test(value)) return false;
  return value.length < MAX_DETAIL_ID.length
    || (value.length === MAX_DETAIL_ID.length && value <= MAX_DETAIL_ID);
}

export function reconcileQualityPreference(preferred, profiles) {
  if (!Array.isArray(profiles)) return validQualityProfileId(preferred) ? preferred : "auto";
  const ids = profiles.map((profile) => profile?.id).filter(validQualityProfileId);
  if (ids.includes(preferred)) return preferred;
  if (ids.includes("auto")) return "auto";
  if (ids.length > 0) return ids[0];
  return "auto";
}

function qualitySafety(profile) {
  const bandwidth = Number(profile?.expected_bandwidth_kbps)
    || Number(profile?.max_video_kbps) + Number(profile?.audio_kbps);
  const pixels = Number(profile?.max_width) * Number(profile?.max_height);
  if (!(bandwidth > 0) || !(pixels > 0)) return null;
  return [bandwidth, pixels, Number(profile?.max_fps) || Number.MAX_SAFE_INTEGER];
}

function compareQualitySafety(left, right) {
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) return left[index] - right[index];
  }
  return 0;
}

export function saferCompatibleQualityProfile(profiles, currentId) {
  if (!Array.isArray(profiles)) return null;
  const measured = profiles
    .filter((profile) => validQualityProfileId(profile?.id))
    .map((profile) => ({
      id: profile.id,
      safety: qualitySafety(profile),
      automaticFallback: profile.automatic_fallback === true,
    }))
    .filter((profile) => profile.safety);
  const current = measured.find((profile) => profile.id === currentId);
  if (!current) return null;
  const automatic = measured.filter((profile) => profile.automaticFallback);
  const candidates = automatic.length > 0 ? automatic : measured;
  const safest = candidates
    .filter((profile) => profile.id !== currentId)
    .sort((left, right) => compareQualitySafety(left.safety, right.safety)
      || left.id.localeCompare(right.id))[0];
  return safest && compareQualitySafety(safest.safety, current.safety) < 0
    ? safest.id
    : null;
}

export function nativeHlsQualityProfile(profiles, preferredId, appleMobile) {
  if (!appleMobile || preferredId !== "auto") return preferredId;
  return saferCompatibleQualityProfile(profiles, preferredId) || preferredId;
}

export function encodingPreset(value, advertised = null) {
  return ["balanced", "fast_start", "maximum_speed"].includes(value)
    && (advertised === null || advertised.some((preset) => preset?.id === value))
    ? value : "balanced";
}

export function originalDownloadUrl(item) {
  return typeof item?.download_url === "string" && item.download_url.startsWith("/web/download/")
    ? item.download_url : null;
}

export function playbackProcessing(playback) {
  if (playback.sourceMode === SOURCE_MODES.ORIGINAL) {
    return { label: "Original file", description: "Playing the original file without server conversion." };
  }
  const negotiation = playback.streamNegotiation;
  if (!negotiation || playback.sourceMode !== SOURCE_MODES.COMPATIBLE) {
    return { label: "Prepared streaming", description: "Checking which streams can be copied without re-encoding." };
  }
  const hasVideo = playback.item?.kind === "video";
  const hasAudio = playback.item?.kind === "audio" || Boolean(playback.item?.audio_codec
    || playback.audioTracks?.length || playback.item?.audio_tracks?.length);
  const convertsAudio = hasAudio && negotiation.audio !== "copy";
  if (hasVideo && negotiation.video !== "copy") {
    return {
      label: "Re-encoding video",
      description: `${negotiation.video === "repair" ? "Re-encoding video to repair frame timing." : "Re-encoding video for the browser or selected quality."} ${convertsAudio ? "Audio is also converted." : hasAudio ? "Audio is copied unchanged." : "The source has no audio."}`,
    };
  }
  if (convertsAudio) {
    return { label: "Converting audio", description: hasVideo
      ? "Converting audio; video is copied unchanged without video quality loss."
      : "Converting audio for browser playback. The source has no video." };
  }
  return { label: "Repackaging", description: "Copying the source streams into a streaming container without re-encoding or quality loss." };
}

export function nativeHlsHevcCopyEligible(item, quality, enabled) {
  // The server only advertises a video content type for remux-compatible
  // codecs/HDR. Keep that policy authoritative rather than duplicating it.
  return enabled === true && quality === "auto" && item?.kind === "video"
    && primaryVideoCodec(item.video_codec) === "hevc"
    && Boolean(item.video_content_type) && !item.video_repair_required;
}

export function automaticCompatibleRecoveryProfile(profiles, currentId, preferredId) {
  return preferredId === "auto"
    ? saferCompatibleQualityProfile(profiles, currentId)
    : null;
}

export function isAppleMobileDevice({ userAgent = "", platform = "", maxTouchPoints = 0 } = {}) {
  if (/\b(?:iPad|iPhone|iPod)\b/i.test(String(userAgent))) return true;
  return /\bMacintosh\b/i.test(String(userAgent))
    && String(platform) === "MacIntel"
    && Number(maxTouchPoints) > 1;
}

export function isSafariBrowser({ userAgent = "" } = {}) {
  const value = String(userAgent);
  return /\bAppleWebKit\//i.test(value)
    && /\bSafari\//i.test(value)
    && !/\b(?:Android|Chrome|Chromium|CriOS|Edg|EdgiOS|FxiOS|OPiOS)\b/i.test(value);
}

export function isApplePhoneDevice({ userAgent = "" } = {}) {
  return /\b(?:iPhone|iPod)\b/i.test(String(userAgent));
}

export function isAndroidDevice({ userAgent = "" } = {}) {
  return /\bAndroid\b/i.test(String(userAgent));
}

export function parseHlsMediaPlaylist(value, baseHref) {
  if (typeof value !== "string" || value.length === 0 || value.length > 4 * 1024 * 1024) return null;
  let base;
  try {
    base = new URL(baseHref);
  } catch (_) {
    return null;
  }
  const mediaSourceDelivery = base.searchParams.get("delivery") === "mse";
  const initDelivery = mediaSourceDelivery ? "mse_init" : "hls_init";
  const segmentDelivery = mediaSourceDelivery ? "mse_segment" : "hls_segment";
  const lines = value.split(/\r?\n/).map((line) => line.trim());
  if (lines[0] !== "#EXTM3U" || lines.length > 40_000) return null;

  const confinedResource = (raw, delivery, extension) => {
    let url;
    try {
      url = new URL(raw, base);
    } catch (_) {
      return null;
    }
    const offsets = url.searchParams.getAll("hls_offset");
    const lengths = url.searchParams.getAll("hls_length");
    if (url.origin !== base.origin
      || !new RegExp(`^/web/media/[1-9]\\d*\\.${extension}$`).test(url.pathname)
      || url.searchParams.getAll("delivery").length !== 1
      || url.searchParams.get("delivery") !== delivery
      || offsets.length !== 1 || !/^\d+$/.test(offsets[0])
      || lengths.length !== 1 || !/^[1-9]\d*$/.test(lengths[0])) return null;
    return url.href;
  };

  let initUrl = null;
  let expectsSegment = false;
  const segmentUrls = [];
  for (const line of lines.slice(1)) {
    if (line.startsWith("#EXT-X-MAP:")) {
      if (initUrl !== null) return null;
      const match = line.match(/^#EXT-X-MAP:URI="([^"]+)"$/);
      initUrl = match ? confinedResource(match[1], initDelivery, "mp4") : null;
      if (!initUrl) return null;
    } else if (line.startsWith("#EXTINF:")) {
      if (expectsSegment) return null;
      expectsSegment = true;
    } else if (line && !line.startsWith("#")) {
      if (!expectsSegment || segmentUrls.length >= 20_000) return null;
      const segment = confinedResource(line, segmentDelivery, "m4s");
      if (!segment) return null;
      segmentUrls.push(segment);
      expectsSegment = false;
    }
  }
  const ended = lines.includes("#EXT-X-ENDLIST");
  if (!initUrl
    || expectsSegment
    || (segmentUrls.length === 0 && !(mediaSourceDelivery && ended))) return null;
  return {
    initUrl,
    segmentUrls,
    ended,
  };
}

function positiveNumber(value, fallback) {
  const numeric = Number(value);
  return Number.isFinite(numeric) && numeric > 0 ? numeric : fallback;
}

function parsedFrameRate(value) {
  const [numerator, denominator = "1"] = String(value || "").split("/");
  return positiveNumber(Number(numerator) / positiveNumber(denominator, 1), 30);
}

export function hdrDisplaySupport(matchMedia = globalThis.matchMedia) {
  if (typeof matchMedia !== "function") return null;
  for (const prefix of ["video-dynamic-range", "dynamic-range"]) {
    try {
      if (matchMedia(`(${prefix}: high)`).matches) return true;
      if (matchMedia(`(${prefix}: standard)`).matches) return false;
    } catch (_) {
      // An unsupported media feature remains unknown rather than disabling HDR.
    }
  }
  return null;
}

export function primaryVideoCodec(value) {
  return String(value || "").split(",", 1)[0].trim().toLowerCase();
}

function hdr10OutputSource(item) {
  return item?.kind === "video"
    && primaryVideoCodec(item.video_codec) === "hevc"
    && Number(item.bit_depth) > 8
    && ["hdr10", "dv-p7", "dv-p8"].includes(String(item.hdr || "").toLowerCase());
}

export function hdrVideoOutputCandidate(item, videoOutputs) {
  if (!hdr10OutputSource(item) || !Array.isArray(videoOutputs)) return null;
  const output = videoOutputs.find((candidate) => candidate?.id === "hevc_hdr10");
  return output?.video_content_type ? output : null;
}

function qualityEnvelope(profile) {
  const width = Number(profile?.max_width);
  const height = Number(profile?.max_height);
  return Number.isFinite(width) && width > 0 && Number.isFinite(height) && height > 0
    ? { width, height }
    : null;
}

function sourceDimensions(item) {
  const width = Number(item?.width);
  const height = Number(item?.height);
  return item?.kind === "video"
    && Number.isFinite(width) && width > 0
    && Number.isFinite(height) && height > 0
    ? { width, height }
    : null;
}

function strictFrameRate(value) {
  const parts = String(value || "").split("/");
  if (parts.length > 2) return null;
  const [numerator, denominator = "1"] = parts;
  const rate = Number(numerator) / Number(denominator);
  return Number.isFinite(rate) && rate > 0 ? rate : null;
}

export function aiUpscaleAvailable(capability, item) {
  const source = sourceDimensions(item);
  const frameRate = strictFrameRate(item?.frame_rate);
  if (!source || frameRate === null
    || String(item?.hdr || "").toLowerCase() !== "sdr"
    || Number(item?.bit_depth) !== Number(capability?.bit_depth || 8)
    || !Array.isArray(capability?.profiles)) return false;
  const pixelRate = source.width * source.height * frameRate;
  return capability.profiles.some((profile) => source.width <= Number(profile?.max_source_width)
    && source.height <= Number(profile?.max_source_height)
    && pixelRate <= Number(profile?.max_source_pixels_per_second));
}

export function aiUpscaleQualityAvailable(capability, item, profile) {
  const source = sourceDimensions(item);
  const envelope = qualityEnvelope(profile);
  const maxScale = positiveNumber(capability?.max_scale, 2);
  return aiUpscaleAvailable(capability, item)
    && source
    && envelope
    && envelope.width > source.width
    && envelope.height > source.height
    && (envelope.width <= source.width * maxScale
      || envelope.height <= source.height * maxScale);
}

function minimumSourcePreservingProfiles(profiles, item) {
  const source = sourceDimensions(item);
  if (!source || !Array.isArray(profiles)) return [];
  const preserving = profiles
    .map((profile) => ({ profile, envelope: qualityEnvelope(profile) }))
    .filter(({ profile, envelope }) => profile?.id !== "auto"
      && envelope
      && envelope.width >= source.width
      && envelope.height >= source.height);
  return preserving.filter(({ envelope }, index) => !preserving.some(({ envelope: candidate }, candidateIndex) => (
    candidateIndex !== index
    && candidate.width <= envelope.width
    && candidate.height <= envelope.height
    && (candidate.width < envelope.width || candidate.height < envelope.height)
  )));
}

export function sourceApplicableQualityProfiles(profiles, item, aiUpscale = null) {
  if (!Array.isArray(profiles)) return [];
  const source = sourceDimensions(item);
  if (!source) return profiles;
  const minimum = new Set(minimumSourcePreservingProfiles(profiles, item).map(({ profile }) => profile));
  return profiles.filter((profile) => {
    if (profile?.id === "auto" || minimum.has(profile)
      || aiUpscaleQualityAvailable(aiUpscale, item, profile)) return true;
    const envelope = qualityEnvelope(profile);
    return !envelope || envelope.width < source.width || envelope.height < source.height;
  });
}

export function sourceBoundedQualityProfile(profiles, preferredId, item, aiUpscale = null) {
  if (!Array.isArray(profiles) || preferredId === "auto") return preferredId;
  const preferred = profiles.find((profile) => profile?.id === preferredId);
  const source = sourceDimensions(item);
  const envelope = qualityEnvelope(preferred);
  if (!preferred || !source || !envelope
    || envelope.width < source.width || envelope.height < source.height) return preferredId;
  if (aiUpscaleQualityAvailable(aiUpscale, item, preferred)) return preferredId;
  const minimum = minimumSourcePreservingProfiles(profiles, item);
  if (minimum.some(({ profile }) => profile === preferred)) return preferredId;
  return minimum[0]?.profile?.id || preferredId;
}

export function sourceAwareQualityProfileLabel(profiles, profile, item, aiUpscale = null) {
  const source = sourceDimensions(item);
  const envelope = qualityEnvelope(profile);
  if (source && envelope && profile?.id === "auto"
    && (envelope.width > source.width || envelope.height > source.height)) {
    return `Auto · up to ${Math.min(source.width, envelope.width)}×${Math.min(source.height, envelope.height)}`;
  }
  if (aiUpscaleQualityAvailable(aiUpscale, item, profile)) {
    return `${String(profile?.label || "")} · AI upscale`;
  }
  const isMinimumPreserving = minimumSourcePreservingProfiles(profiles, item)
    .some(({ profile: candidate }) => candidate === profile);
  if (!source || !envelope || !isMinimumPreserving
    || (envelope.width === source.width && envelope.height === source.height)) {
    return String(profile?.label || "");
  }
  const maxVideoKbps = Number(profile?.max_video_kbps);
  const bitrate = Number.isFinite(maxVideoKbps) && maxVideoKbps > 0
    ? ` · ${Number((maxVideoKbps / 1000).toFixed(2))} Mbps`
    : "";
  return `Source ${source.width}×${source.height}${bitrate}`;
}

export function compatibleVideoDimensions(item, profile, aiUpscale = null) {
  const maxWidth = positiveNumber(profile?.max_width, 3840);
  const maxHeight = positiveNumber(profile?.max_height, 2160);
  if (aiUpscaleQualityAvailable(aiUpscale, item, profile)) {
    const source = sourceDimensions(item);
    const scale = Math.min(maxWidth / source.width, maxHeight / source.height);
    return {
      width: Math.max(2, Math.floor((source.width * scale) / 2) * 2),
      height: Math.max(2, Math.floor((source.height * scale) / 2) * 2),
    };
  }
  return {
    width: Math.min(positiveNumber(item?.width, maxWidth), maxWidth),
    height: Math.min(positiveNumber(item?.height, maxHeight), maxHeight),
  };
}

export function videoOutputConfiguration(output, profile, item, aiUpscale = null) {
  if (!output?.video_content_type) return null;
  const dimensions = compatibleVideoDimensions(item, profile, aiUpscale);
  const video = {
    contentType: output.video_content_type,
    width: dimensions.width,
    height: dimensions.height,
    bitrate: positiveNumber(profile?.max_video_kbps, 25_000) * 1_000,
    framerate: Math.min(parsedFrameRate(item?.frame_rate), positiveNumber(profile?.max_fps, 30)),
  };
  if (output.hdr_metadata_type) video.hdrMetadataType = output.hdr_metadata_type;
  if (output.color_gamut) video.colorGamut = output.color_gamut;
  if (output.transfer_function) video.transferFunction = output.transfer_function;
  return { type: "file", video };
}

export function videoDecodingConfiguration(item) {
  if (item?.kind !== "video" || !item.video_content_type) return null;
  const video = {
    contentType: item.video_content_type,
    width: positiveNumber(item.width, 1920),
    height: positiveNumber(item.height, 1080),
    bitrate: positiveNumber(Number(item.bitrate) * 8, 8_000_000),
    framerate: parsedFrameRate(item.frame_rate),
  };
  if (String(item.hdr).toLowerCase() === "hdr10") {
    video.hdrMetadataType = "smpteSt2086";
    video.colorGamut = "bt2020";
    video.transferFunction = "pq";
  }
  return { type: "file", video };
}

export function audioDecodingConfiguration(item, track) {
  if (!track?.content_type) return null;
  const codec = String(track.codec || "").toLowerCase();
  const bitrate = { aac: 320_000, ac3: 640_000, eac3: 1_536_000, mp3: 320_000 }[codec] || 320_000;
  return {
    type: "file",
    audio: {
      contentType: track.content_type,
      channels: String(positiveNumber(track.channels || item?.channels, 2)),
      bitrate,
      samplerate: positiveNumber(item?.sample_rate, 48_000),
    },
  };
}

async function supportsConfiguration(configuration, canPlayType, decodingInfo, decodingInfoTimeoutMs) {
  if (!configuration) {
    return {
      supported: false,
      canPlayType: "not tested",
      mediaCapabilities: "not tested",
    };
  }
  const contentType = configuration.video?.contentType || configuration.audio?.contentType;
  let canPlayTypeResult = "";
  try {
    canPlayTypeResult = String(canPlayType(contentType) || "");
  } catch (_) {
    // An invalid or overly specific candidate is simply not supported.
  }
  const basicSupport = Boolean(canPlayTypeResult);
  if (!decodingInfo) {
    return {
      supported: basicSupport,
      canPlayType: canPlayTypeResult || "unsupported",
      mediaCapabilities: "unavailable",
    };
  }
  try {
    let timeout = null;
    const query = Promise.resolve().then(() => decodingInfo(configuration));
    const timeoutMs = Number(decodingInfoTimeoutMs);
    const capabilities = Number.isFinite(timeoutMs) && timeoutMs > 0
      ? await Promise.race([
        query,
        new Promise((_, reject) => {
          timeout = globalThis.setTimeout(() => {
            const error = new Error("Media Capabilities probe timed out");
            error.name = "TimeoutError";
            reject(error);
          }, timeoutMs);
        }),
      ]).finally(() => globalThis.clearTimeout(timeout))
      : await query;
    const capabilitiesSupport = Boolean(capabilities.supported);
    return {
      // In practice, HEVC-capable browsers can disagree between these two
      // APIs. Either positive answer is sufficient to try stream copy.
      supported: capabilitiesSupport || basicSupport,
      canPlayType: canPlayTypeResult || "unsupported",
      mediaCapabilities: capabilitiesSupport ? "supported" : "unsupported",
    };
  } catch (error) {
    return {
      supported: basicSupport,
      canPlayType: canPlayTypeResult || "unsupported",
      mediaCapabilities: error?.name === "TimeoutError" ? "timed out" : "error",
    };
  }
}

export async function negotiateCompatibleStreams({
  item,
  track,
  quality,
  qualityProfile = null,
  videoOutputs = [],
  aiUpscale = null,
  hdrDisplay = null,
  canPlayType,
  decodingInfo = null,
  decodingInfoTimeoutMs = MEDIA_CAPABILITIES_TIMEOUT_MS,
}) {
  const videoConfiguration = quality === "auto" ? videoDecodingConfiguration(item) : null;
  const audioConfiguration = audioDecodingConfiguration(item, track);
  const hdrOutput = hdrVideoOutputCandidate(item, videoOutputs);
  const outputConfiguration = videoOutputConfiguration(hdrOutput, qualityProfile, item, aiUpscale);
  const [videoProbe, audioProbe, outputVideoProbe] = await Promise.all([
    supportsConfiguration(videoConfiguration, canPlayType, decodingInfo, decodingInfoTimeoutMs),
    supportsConfiguration(audioConfiguration, canPlayType, decodingInfo, decodingInfoTimeoutMs),
    supportsConfiguration(outputConfiguration, canPlayType, decodingInfo, decodingInfoTimeoutMs),
  ]);
  const repairEncoder = String(item?.repair_video_encoder || "").toLowerCase();
  const repairableSource = ["h264", "hevc"].includes(primaryVideoCodec(item?.video_codec));
  const portableRepair = repairableSource
    && item?.video_repair_required
    && ["libx264", "h264_nvenc"].includes(repairEncoder);
  const video = item?.kind === "video" && quality === "auto"
      ? (item.video_repair_required
        ? (repairableSource && (portableRepair || videoProbe.supported) ? "repair" : "transcode")
        : (videoProbe.supported ? "copy" : "transcode"))
      : "transcode";
  return {
    video,
    audio: audioProbe.supported ? "copy" : "transcode",
    videoOutput: video === "transcode" && outputVideoProbe.supported ? "hevc_hdr10" : "h264_sdr",
    hdrDisplay,
    videoContentType: videoConfiguration?.video.contentType || null,
    audioContentType: audioConfiguration?.audio.contentType || null,
    outputVideoContentType: outputConfiguration?.video.contentType || null,
    videoProbe,
    audioProbe,
    outputVideoProbe,
  };
}

export function queuePosition(queue, itemId) {
  return queue.findIndex((item) => String(item.id) === String(itemId));
}

export function queueNeighbor(queue, itemId, delta) {
  const index = queuePosition(queue, itemId);
  return index >= 0 ? queue[index + delta] || null : null;
}

export function audioTrackLabel(track) {
  const parts = [];
  if (track.language) parts.push(String(track.language).toUpperCase());
  if (track.title) parts.push(track.title);
  if (track.default) parts.push("Default");
  const codec = String(track.codec || "audio")
    .replace("eac3", "E-AC-3")
    .replace("ac3", "AC-3")
    .toUpperCase();
  const channels = track.channels === 8 ? "7.1" : track.channels === 6 ? "5.1"
    : track.channels > 0 ? `${track.channels}ch` : "";
  parts.push([codec, channels].filter(Boolean).join(" "));
  return parts.filter(Boolean).join(" · ") || `Track ${Number(track.index) + 1}`;
}

export function mediaDetails(item) {
  const parts = [];
  if (item?.kind === "audio") {
    if (item.artist) parts.push(item.artist);
    if (item.album) parts.push(item.album);
  } else {
    if (item?.date) parts.push(String(item.date).slice(0, 4));
    if (item?.resolution) parts.push(item.resolution);
  }
  if (itemDuration(item) > 0) parts.push(clockLabel(itemDuration(item)));
  return parts.join(" · ");
}

export function mediaMatchesQuery(item, query) {
  const normalized = String(query || "").trim().toLowerCase();
  if (!normalized) return true;
  return [item?.file_name, item?.title, item?.artist, item?.album_artist, item?.album]
    .some((value) => String(value || "").toLowerCase().includes(normalized));
}

export function navigationFromUrl(href) {
  const url = new URL(href);
  const folder = url.searchParams.get("folder");
  const view = url.searchParams.get("view");
  const kind = ["all", "video", "audio"].includes(view) ? view : "all";
  const rawItem = url.searchParams.get("item");
  const rawStart = url.searchParams.get("t");
  const itemId = rawItem && /^\d+$/.test(rawItem) ? rawItem : null;
  const requestedLayout = url.searchParams.get("layout");
  return {
    view: folder ? "folders" : view === "continue" ? "continue" : (["all", "video", "audio"].includes(view) ? "library" : "folders"),
    folder: folder || null,
    kind: folder ? "all" : kind,
    query: (url.searchParams.get("q") || "").slice(0, 256),
    sort: ["title", "date_desc", "episode"].includes(url.searchParams.get("sort"))
      ? url.searchParams.get("sort") : "title",
    itemId,
    start: rawStart && /^\d+(?:\.\d+)?$/.test(rawStart) ? Math.max(0, Number(rawStart)) : 0,
    layout: Object.values(LAYOUT_MODES).includes(requestedLayout)
      ? requestedLayout
      : itemId ? LAYOUT_MODES.WATCH : LAYOUT_MODES.BROWSE,
  };
}

export function navigationUrl(href, navigation, rootFolderId) {
  const url = new URL(href);
  url.search = "";
  if (navigation.view === "folders" && navigation.folder && navigation.folder !== rootFolderId) {
    url.searchParams.set("folder", navigation.folder);
  } else if (navigation.view === "library") {
    url.searchParams.set("view", navigation.kind);
  } else if (navigation.view === "continue") {
    url.searchParams.set("view", "continue");
  }
  if (navigation.query) url.searchParams.set("q", navigation.query);
  if (navigation.sort !== "title") url.searchParams.set("sort", navigation.sort);
  if (navigation.itemId !== null && navigation.itemId !== undefined) {
    url.searchParams.set("item", String(navigation.itemId));
    if (navigation.start > 0) url.searchParams.set("t", String(Math.floor(navigation.start)));
  }
  const implicitLayout = navigation.itemId ? LAYOUT_MODES.WATCH : LAYOUT_MODES.BROWSE;
  const layout = Object.values(LAYOUT_MODES).includes(navigation.layout)
    ? navigation.layout
    : implicitLayout;
  if (layout !== implicitLayout) url.searchParams.set("layout", layout);
  return `${url.pathname}${url.search}`;
}

const ERROR_MAP = Object.freeze({
  media_missing: ["This media file is no longer available.", ["retry", "return_to_library"]],
  unsupported_direct: ["Your browser cannot play the original file.", ["try_compatible"]],
  transcode_disabled: ["Prepared streaming is disabled on this server.", ["play_original"]],
  transcode_busy: ["The server is preparing other media. Try again shortly.", ["retry"]],
  transcode_failed: ["The server could not prepare this title.", ["retry", "play_original"]],
  transcode_cancelled: ["Preparing this title was cancelled.", ["retry", "play_original"]],
  network: ["The server connection was interrupted.", ["retry"]],
  offline: ["You appear to be offline.", ["retry"]],
  unknown: ["Playback could not continue.", ["retry", "try_compatible"]],
});

export function playbackError(code, technical = "") {
  const [message, actions] = ERROR_MAP[code] || ERROR_MAP.unknown;
  return { code, message, actions: [...actions], technical };
}

export function apiErrorCategory(error) {
  if (error?.code === "media_missing") return "media_missing";
  if (error?.code === "transcode_disabled") return "transcode_disabled";
  if (error?.code === "transcode_busy" || error?.status === 503) return "transcode_busy";
  if (error?.code === "transcode_cancelled") return "transcode_cancelled";
  if (error?.code === "transcode_failed") return "transcode_failed";
  if (typeof navigator !== "undefined" && !navigator.onLine) return "offline";
  if (error?.name === "TypeError" || error?.status === 0) return "network";
  return "unknown";
}
