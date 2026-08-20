const PREFIX = "rustydlna.";
const PROGRESS_KEY = `${PREFIX}webProgress.v1`;

function read(key, fallback) {
  try {
    const value = localStorage.getItem(`${PREFIX}${key}`);
    return value === null ? fallback : value;
  } catch (_) {
    return fallback;
  }
}

function write(key, value) {
  try {
    localStorage.setItem(`${PREFIX}${key}`, String(value));
    return true;
  } catch (_) {
    return false;
  }
}

export function loadPreferences() {
  const rate = Number(read("rate", "1"));
  const volume = Number(read("volume", "100"));
  const streamMode = read("stream", "auto");
  return {
    rate: [0.75, 1, 1.25, 1.5, 2].includes(rate) ? rate : 1,
    volume: Number.isFinite(volume) ? Math.max(0, Math.min(100, volume)) : 100,
    streamMode: ["auto", "direct", "compat"].includes(streamMode) ? streamMode : "auto",
    quality: ["auto", "full_hd", "data_saver"].includes(read("quality", "auto")) ? read("quality", "auto") : "auto",
    muted: read("muted", "false") === "true",
    loop: read("loop", "false") === "true",
    fill: read("fill", "false") === "true",
    autoplay: read("autoplay", "false") === "true",
    caption: read("caption", "off"),
    captionSize: ["normal", "large", "extra_large"].includes(read("captionSize", "normal")) ? read("captionSize", "normal") : "normal",
    captionBackground: ["translucent", "solid"].includes(read("captionBackground", "translucent")) ? read("captionBackground", "translucent") : "translucent",
  };
}

export function savePreference(name, value) {
  return write(name, value);
}

function readProgressMap() {
  try {
    const parsed = JSON.parse(localStorage.getItem(PROGRESS_KEY) || "{}");
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch (_) {
    return {};
  }
}

function writeProgressMap(progress) {
  try {
    localStorage.setItem(PROGRESS_KEY, JSON.stringify(progress));
    return true;
  } catch (_) {
    return false;
  }
}

export function progressFor(itemId) {
  const entry = readProgressMap()[String(itemId)];
  const position = Number(entry?.position);
  return Number.isFinite(position) && position > 0 ? position : 0;
}

export function progressDetails(itemId) {
  const entry = readProgressMap()[String(itemId)];
  const position = Number(entry?.position);
  const duration = Number(entry?.duration);
  const updated = Number(entry?.updated);
  return {
    position: Number.isFinite(position) && position > 0 ? position : 0,
    duration: Number.isFinite(duration) && duration > 0 ? duration : 0,
    updated: Number.isFinite(updated) ? updated : 0,
  };
}

export function saveProgress(itemId, position, duration) {
  if (itemId === null || itemId === undefined) return false;
  const progress = readProgressMap();
  if (!(position > 0) || !(duration > 0)) {
    delete progress[String(itemId)];
  } else {
    progress[String(itemId)] = { position: Math.floor(position), duration: Math.floor(duration), updated: Date.now() };
  }
  const entries = Object.entries(progress);
  if (entries.length > 500) {
    entries.sort((left, right) => Number(right[1]?.updated || 0) - Number(left[1]?.updated || 0));
    for (const [key] of entries.slice(500)) delete progress[key];
  }
  return writeProgressMap(progress);
}

export function clearProgress(itemId) {
  return saveProgress(itemId, 0, 0);
}

export function createProgressWriter(writeNow, interval = 5000) {
  let timer = null;
  let pending = false;
  const flush = () => {
    if (timer !== null) window.clearTimeout(timer);
    timer = null;
    if (!pending) return;
    pending = false;
    writeNow();
  };
  return {
    schedule() {
      pending = true;
      if (timer === null) timer = window.setTimeout(flush, interval);
    },
    flush,
    cancel() {
      if (timer !== null) window.clearTimeout(timer);
      timer = null;
      pending = false;
    },
  };
}
