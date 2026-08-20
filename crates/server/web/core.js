export const PLAYBACK_STATES = Object.freeze([
  "idle", "loading", "waiting", "playing", "paused", "seeking", "ended", "error",
]);

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

export function seekTarget(value, duration) {
  const numeric = Math.max(0, Number(value) || 0);
  return duration > 0 ? Math.min(numeric, duration) : numeric;
}

export function compatibleSegmentStart(value, bucketSeconds = 10) {
  const target = Math.max(0, Math.floor(Number(value) || 0));
  const bucket = Math.max(1, Math.floor(Number(bucketSeconds) || 1));
  return Math.floor(target / bucket) * bucket;
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

export function chooseSource({ requestedMode, directSupport, transcoding }) {
  if (requestedMode === "direct") return { mode: "direct", reason: "forced_original" };
  if (requestedMode === "compat") {
    return transcoding
      ? { mode: "compatible", reason: "forced_compatible" }
      : { mode: "direct", reason: "transcoding_disabled" };
  }
  if (directSupport) return { mode: "direct", reason: "browser_supported" };
  return transcoding
    ? { mode: "compatible", reason: "browser_support_uncertain" }
    : { mode: "direct", reason: "transcoding_disabled" };
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

export function navigationFromUrl(href) {
  const url = new URL(href);
  const folder = url.searchParams.get("folder");
  const view = url.searchParams.get("view");
  const kind = ["all", "video", "audio"].includes(view) ? view : "all";
  const rawItem = url.searchParams.get("item");
  const rawStart = url.searchParams.get("t");
  return {
    view: folder ? "folders" : view === "continue" ? "continue" : (["all", "video", "audio"].includes(view) ? "library" : "folders"),
    folder: folder || null,
    kind: folder ? "all" : kind,
    query: (url.searchParams.get("q") || "").slice(0, 256),
    sort: ["title", "date_desc", "episode"].includes(url.searchParams.get("sort"))
      ? url.searchParams.get("sort") : "title",
    itemId: rawItem && /^\d+$/.test(rawItem) ? rawItem : null,
    start: rawStart && /^\d+(?:\.\d+)?$/.test(rawStart) ? Math.max(0, Number(rawStart)) : 0,
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
  return `${url.pathname}${url.search}`;
}

const ERROR_MAP = Object.freeze({
  media_missing: ["This media file is no longer available.", ["retry", "return_to_library"]],
  unsupported_direct: ["Your browser cannot play the original file.", ["try_compatible"]],
  transcode_disabled: ["Compatible playback is disabled on this server.", ["play_original"]],
  transcode_busy: ["The server is preparing other media. Try again shortly.", ["retry"]],
  transcode_failed: ["The server could not prepare this title.", ["retry", "play_original"]],
  transcode_cancelled: ["Preparing this title was cancelled.", ["retry", "play_original"]],
  network: ["The server connection was interrupted.", ["retry"]],
  offline: ["You appear to be offline.", ["retry"]],
  browser_policy: ["Playback is ready. Press Play to begin.", ["play"]],
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
