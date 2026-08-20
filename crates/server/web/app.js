(() => {
  "use strict";

  const ROOT_FOLDER = "64";

  function stored(name, fallback) {
    try {
      return localStorage.getItem(`rustydlna.${name}`) || fallback;
    } catch (_) {
      return fallback;
    }
  }

  function store(name, value) {
    try {
      localStorage.setItem(`rustydlna.${name}`, String(value));
    } catch (_) {
      // Playback must remain usable when storage is blocked.
    }
  }

  const savedRate = Number(stored("rate", "1"));
  const savedVolume = Number(stored("volume", "100"));
  const savedMode = stored("stream", "auto");
  const state = {
    view: "folders",
    folder: ROOT_FOLDER,
    kind: "all",
    query: "",
    offset: 0,
    limit: 60,
    total: 0,
    entries: [],
    loading: false,
    request: 0,
    current: null,
    selectedAudio: 0,
    itemRequest: 0,
    segmentOffset: 0,
    scrubbing: false,
    usedFallback: false,
    transcoding: false,
    rate: [0.75, 1, 1.25, 1.5, 2].includes(savedRate) ? savedRate : 1,
    volume: Number.isFinite(savedVolume) ? Math.max(0, Math.min(200, savedVolume)) : 100,
    playMode: ["auto", "direct", "compat"].includes(savedMode) ? savedMode : "auto",
    muted: stored("muted", "false") === "true",
    loop: stored("loop", "false") === "true",
    fill: stored("fill", "false") === "true",
  };

  const el = (id) => document.getElementById(id);
  const grid = el("media-grid");
  const empty = el("library-empty");
  const loading = el("loading");
  const loadMore = el("load-more");
  const video = el("video-player");
  const audio = el("audio-player");
  const audioStage = el("audio-stage");
  const playerStage = el("player-stage");
  const fullscreenButton = el("fullscreen-button");
  const playerEmpty = el("player-empty");
  const message = el("player-message");
  const breadcrumbs = el("breadcrumbs");
  const searchInput = el("search-input");
  const playerPanel = document.querySelector(".player-panel");
  const library = document.querySelector(".library");
  let boostContext = null;
  const boostGains = new Map();
  let fullscreenHideTimer = null;

  function hideFullscreenControl() {
    if (fullscreenHideTimer !== null) window.clearTimeout(fullscreenHideTimer);
    fullscreenHideTimer = null;
    fullscreenButton.hidden = true;
  }

  function showFullscreenControl() {
    if (state.current?.kind !== "video") return;
    if (fullscreenHideTimer !== null) window.clearTimeout(fullscreenHideTimer);
    fullscreenButton.hidden = false;
    fullscreenHideTimer = window.setTimeout(hideFullscreenControl, 5000);
  }

  playerStage.addEventListener("mousemove", showFullscreenControl);

  function syncLibraryPanelHeight() {
    library.style.setProperty("--player-panel-height", `${Math.ceil(playerPanel.getBoundingClientRect().height)}px`);
  }

  if ("ResizeObserver" in window) {
    new ResizeObserver(syncLibraryPanelHeight).observe(playerPanel);
  } else {
    window.addEventListener("resize", syncLibraryPanelHeight);
  }
  syncLibraryPanelHeight();

  function durationLabel(value) {
    if (!value) return "";
    return value.replace(/^0:/, "").replace(/\.\d+$/, "");
  }

  function durationSeconds(value) {
    if (!value) return 0;
    const parts = value.split(":").map(Number);
    if (parts.length !== 3 || parts.some((part) => !Number.isFinite(part))) return 0;
    return parts[0] * 3600 + parts[1] * 60 + parts[2];
  }

  function itemDurationSeconds(item) {
    const exact = Number(item?.duration_seconds);
    return Number.isFinite(exact) && exact > 0 ? exact : durationSeconds(item?.duration);
  }

  function clockLabel(value) {
    const total = Math.max(0, Math.floor(value || 0));
    const hours = Math.floor(total / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    const seconds = total % 60;
    return hours > 0
      ? `${hours}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`
      : `${minutes}:${String(seconds).padStart(2, "0")}`;
  }

  function detailLabel(item) {
    const parts = [];
    if (item.kind === "audio") {
      if (item.artist) parts.push(item.artist);
      if (item.album) parts.push(item.album);
    } else {
      if (item.date) parts.push(item.date.slice(0, 4));
      if (item.resolution) parts.push(item.resolution);
    }
    if (item.duration) parts.push(durationLabel(item.duration));
    return parts.filter(Boolean).join(" · ") || item.mime;
  }

  function sourceType(item) {
    const v = (item.video_codec || "").toLowerCase();
    const a = (item.audio_codec || "").toLowerCase();
    const container = (item.container || "").toLowerCase();
    if (item.kind === "audio") {
      if (item.mime === "audio/mpeg") return "audio/mpeg";
      if (item.mime === "audio/aac") return "audio/aac";
      if (item.mime === "audio/mp4" && a === "aac") return 'audio/mp4; codecs="mp4a.40.2"';
      if (item.mime.includes("wav")) return 'audio/wav; codecs="1"';
      if (item.mime.includes("flac")) return "audio/flac";
      return item.mime;
    }
    if (container === "mp4" && v === "h264" && (a === "aac" || !a)) {
      return 'video/mp4; codecs="avc1.42E01E, mp4a.40.2"';
    }
    if (container === "mp4" && v === "hevc" && (a === "aac" || !a)) {
      return 'video/mp4; codecs="hvc1, mp4a.40.2"';
    }
    if (container === "webm") return "video/webm";
    return item.mime;
  }

  function activePlayer() {
    return state.current?.kind === "audio" ? audio : video;
  }

  function canPlayDirect(item) {
    // A generic `video/mp4` "maybe" is not evidence that the browser can
    // decode the file's actual video and audio codecs. Trust the server's
    // conservative container/codec classification before asking the browser.
    if (item.transcode_likely) return false;
    const player = item.kind === "video" ? video : audio;
    const result = player.canPlayType(sourceType(item));
    return result === "probably" || result === "maybe";
  }

  function compatibilityUrl(item) {
    const separator = item.fallback_url.includes("?") ? "&" : "?";
    const start = Math.max(0, Math.floor(state.segmentOffset));
    return `${item.fallback_url}${separator}audio=${state.selectedAudio}&start=${start}`;
  }

  function audioTrackLabel(track) {
    const parts = [`${track.index + 1}`];
    if (track.language) parts.push(track.language.toUpperCase());
    if (track.title) parts.push(track.title);
    const codec = (track.codec || "audio").replace("eac3", "E-AC-3").replace("ac3", "AC-3").toUpperCase();
    const channels = track.channels === 8 ? "7.1" : track.channels === 6 ? "5.1" : track.channels > 0 ? `${track.channels}ch` : "";
    parts.push([codec, channels].filter(Boolean).join(" "));
    return parts.join(" · ");
  }

  function renderAudioTracks(tracks) {
    const control = el("audio-track-control");
    const choices = el("audio-track-controls");
    choices.replaceChildren();
    control.hidden = tracks.length < 2;
    for (const track of tracks) {
      const option = document.createElement("option");
      option.value = String(track.index);
      option.textContent = audioTrackLabel(track);
      choices.append(option);
    }
    choices.value = String(state.selectedAudio);
  }

  async function enrichAudioTracks(item) {
    if (item.audio_tracks_loaded || (item.audio_tracks || []).length < 2) return;
    item.audio_tracks_loaded = true;
    const request = ++state.itemRequest;
    try {
      const response = await fetch(`/api/web/item/${item.id}`, { headers: { Accept: "application/json" } });
      if (!response.ok) throw new Error(`stream request returned ${response.status}`);
      const payload = await response.json();
      if (request !== state.itemRequest || state.current?.id !== item.id) return;
      if (payload.audio_tracks?.length) item.audio_tracks = payload.audio_tracks;
      renderAudioTracks(item.audio_tracks || []);
    } catch (_) {
      // The persisted codec/channel buttons remain usable if probing fails.
    }
  }

  function showMode(transcoded, supportUncertain = false) {
    const mode = el("playback-mode");
    mode.hidden = false;
    mode.classList.toggle("transcode", transcoded);
    el("mode-label").textContent = transcoded
      ? "Browser compatibility transcode"
      : supportUncertain ? "Direct play · support uncertain" : "Direct play";
  }

  function setBoostGain() {
    const gain = Math.max(1, state.volume / 100);
    for (const node of boostGains.values()) {
      node.gain.setValueAtTime(gain, boostContext.currentTime);
    }
  }

  function ensureAudioBoost() {
    const AudioContextClass = window.AudioContext || window.webkitAudioContext;
    if (!AudioContextClass) return false;
    if (!boostContext) {
      try {
        boostContext = new AudioContextClass();
        for (const player of [video, audio]) {
          const source = boostContext.createMediaElementSource(player);
          const gain = boostContext.createGain();
          source.connect(gain).connect(boostContext.destination);
          boostGains.set(player, gain);
        }
      } catch (_) {
        boostContext = null;
        boostGains.clear();
        return false;
      }
    }
    if (boostContext.state === "suspended") boostContext.resume().catch(() => {});
    setBoostGain();
    return true;
  }

  function applyVolume(enableBoost = false) {
    const baseVolume = Math.min(100, state.volume) / 100;
    video.volume = baseVolume;
    audio.volume = baseVolume;
    if (enableBoost && state.volume > 100) ensureAudioBoost();
    if (boostContext) setBoostGain();
  }

  function applyPlaybackPreferences(player) {
    player.playbackRate = state.rate;
    player.defaultPlaybackRate = state.rate;
    player.muted = state.muted;
    player.volume = Math.min(100, state.volume) / 100;
    player.loop = state.loop;
    video.classList.toggle("fill", state.fill);
  }

  function stopPlayers() {
    video.pause();
    audio.pause();
    video.removeAttribute("src");
    audio.removeAttribute("src");
    video.controls = true;
    audio.controls = true;
    video.load();
    audio.load();
  }

  function loadedAt(player, resumeAt) {
    if (!Number.isFinite(resumeAt) || resumeAt <= 0) return;
    try {
      player.currentTime = Math.min(resumeAt, Number.isFinite(player.duration) ? player.duration : resumeAt);
    } catch (_) {
      // A growing compatibility stream may not be seekable immediately.
    }
  }

  function globalCurrentTime() {
    if (!state.current) return 0;
    const current = activePlayer().currentTime || 0;
    return state.usedFallback ? state.segmentOffset + current : current;
  }

  function syncTimeline(preview = null) {
    const timeline = el("timeline");
    const duration = itemDurationSeconds(state.current);
    const current = preview ?? globalCurrentTime();
    timeline.max = String(Math.max(0, Math.floor(duration)));
    if (preview === null && !state.scrubbing) {
      timeline.value = String(Math.min(duration, Math.max(0, current)));
    }
    timeline.disabled = !state.current || duration <= 0;
    el("timeline-current").textContent = clockLabel(current);
    el("timeline-duration").textContent = clockLabel(duration);
  }

  function seekTo(value) {
    if (!state.current) return;
    const duration = itemDurationSeconds(state.current);
    const target = Math.max(0, Math.min(value, Math.max(0, duration - 1)));
    const player = activePlayer();
    if (!state.usedFallback) {
      player.currentTime = target;
      syncTimeline();
      return;
    }
    const localTarget = target - state.segmentOffset;
    let locallySeekable = false;
    for (let index = 0; index < player.seekable.length; index += 1) {
      if (localTarget >= player.seekable.start(index) && localTarget <= player.seekable.end(index)) {
        locallySeekable = true;
        break;
      }
    }
    if (locallySeekable) {
      player.currentTime = localTarget;
      syncTimeline();
      return;
    }
    const remainPaused = player.paused;
    start(state.current, true, target, remainPaused, target);
    const timelineStatus = el("timeline-status");
    timelineStatus.textContent = `Starting at ${clockLabel(target)}…`;
    timelineStatus.hidden = false;
    activePlayer().addEventListener("playing", () => {
      timelineStatus.hidden = true;
    }, { once: true });
  }

  function start(item, forceFallback = false, resumeAt = 0, remainPaused = false, transcodeStart = null) {
    const sameItem = state.current?.id === item.id;
    stopPlayers();
    if (!sameItem) state.selectedAudio = item.default_audio_index || 0;
    state.current = item;
    hideFullscreenControl();
    message.hidden = true;
    el("timeline-status").hidden = true;
    playerEmpty.hidden = true;
    el("playback-controls").hidden = false;
    el("now-playing-title").textContent = item.title;
    el("now-playing-meta").textContent = detailLabel(item);

    const directSupported = canPlayDirect(item);
    const compatibilityRequested = forceFallback || state.playMode === "compat";
    const automaticFallback = state.playMode === "auto" && !directSupported;
    const useFallback = state.transcoding && (compatibilityRequested || automaticFallback);
    const forcedUncertainDirect = state.playMode === "direct" && !directSupported;
    state.segmentOffset = useFallback && transcodeStart !== null
      ? Math.max(0, Math.floor(transcodeStart))
      : 0;
    const source = useFallback ? compatibilityUrl(item) : item.source_url;
    state.usedFallback = useFallback;
    showMode(useFallback, forcedUncertainDirect);

    if ((compatibilityRequested || automaticFallback) && !state.transcoding) {
      message.textContent = "Compatibility playback needs [transcode].enable = true; trying the original file.";
      message.hidden = false;
    }

    const player = item.kind === "video" ? video : audio;
    if (item.kind === "video") {
      audioStage.hidden = true;
      video.classList.add("active");
      video.poster = item.art_url || "";
    } else {
      video.classList.remove("active");
      audioStage.hidden = false;
      el("audio-art").src = item.art_url || "";
    }
    // A growing fragmented MP4 exposes only its current segment duration to
    // native controls. Hide that misleading short seek bar and use the
    // catalog-backed full-duration timeline above for compatibility streams.
    player.controls = !useFallback;
    player.src = source;
    applyPlaybackPreferences(player);
    if (state.volume > 100) ensureAudioBoost();
    const localResume = useFallback ? Math.max(0, resumeAt - state.segmentOffset) : resumeAt;
    player.addEventListener("loadedmetadata", () => loadedAt(player, localResume), { once: true });
    player.load();
    if (!remainPaused) player.play().catch(() => {});
    renderAudioTracks(item.audio_tracks || []);
    enrichAudioTracks(item);
    markCurrentCard();
    syncControls();
    syncTimeline();
    el("player-stage").scrollIntoView({ behavior: "smooth", block: "center" });
  }

  function playbackError(event) {
    const player = event.currentTarget;
    if (!state.current || !player.currentSrc) return;
    const canFallback = state.transcoding && !state.usedFallback && state.playMode !== "direct";
    if (!canFallback || player.currentSrc.includes("/web/media/")) {
      message.textContent = "Playback failed. Try Compatible mode, or check that the source file is available.";
      message.hidden = false;
      return;
    }
    const resumeAt = globalCurrentTime();
    message.textContent = "Direct playback failed; switching to a browser-compatible stream…";
    message.hidden = false;
    start(state.current, true, resumeAt, false, resumeAt);
  }

  function playableEntries() {
    return state.entries.filter((entry) => entry.entry_type !== "folder");
  }

  function playRelative(delta) {
    const entries = playableEntries();
    const currentIndex = entries.findIndex((item) => item.id === state.current?.id);
    const next = entries[currentIndex + delta];
    if (next) start(next);
  }

  function selectAudioTrack(index) {
    if (!state.current || index === state.selectedAudio) return;
    if (!state.transcoding) {
      message.textContent = "Audio-track selection needs compatibility transcoding to AAC.";
      message.hidden = false;
      return;
    }
    const player = activePlayer();
    const resumeAt = globalCurrentTime();
    const remainPaused = player.paused;
    state.selectedAudio = index;
    state.playMode = "compat";
    store("stream", state.playMode);
    renderAudioTracks(state.current.audio_tracks || []);
    start(state.current, false, resumeAt, remainPaused, resumeAt);
  }

  function markCurrentCard() {
    document.querySelectorAll(".media-card.playing").forEach((card) => card.classList.remove("playing"));
    if (!state.current) return;
    const selected = grid.querySelector(`[data-media-id="${state.current.id}"]`);
    if (selected) selected.classList.add("playing");
  }

  function syncControls() {
    const player = state.current ? activePlayer() : null;
    el("playback-controls").hidden = !state.current;
    el("play-button").textContent = player?.paused === false ? "Pause" : "Play";
    el("mute-button").textContent = state.muted ? "Unmute" : "Mute";
    el("mute-button").setAttribute("aria-pressed", String(state.muted));
    el("volume-control").value = String(state.volume);
    el("volume-value").textContent = state.muted ? "Muted" : `${state.volume}%`;
    el("loop-button").classList.toggle("active", state.loop);
    el("loop-button").setAttribute("aria-pressed", String(state.loop));
    el("fit-button").classList.toggle("active", state.fill);
    el("fit-button").setAttribute("aria-pressed", String(state.fill));
    document.querySelectorAll("[data-rate]").forEach((button) => {
      button.classList.toggle("active", Number(button.dataset.rate) === state.rate);
    });
    document.querySelectorAll("[data-play-mode]").forEach((button) => {
      button.classList.toggle("active", button.dataset.playMode === state.playMode);
      button.disabled = button.dataset.playMode === "compat" && !state.transcoding;
    });
    el("audio-track-controls").value = String(state.selectedAudio);
    const entries = playableEntries();
    const index = entries.findIndex((item) => item.id === state.current?.id);
    el("previous-button").disabled = index <= 0;
    el("next-button").disabled = index < 0 || index >= entries.length - 1;
    const isVideo = state.current?.kind === "video";
    el("fit-button").disabled = !isVideo;
    el("pip-button").disabled = !isVideo || !document.pictureInPictureEnabled;
    if (!isVideo) hideFullscreenControl();
    fullscreenButton.textContent = document.fullscreenElement ? "Exit full screen" : "Full screen";
    fullscreenButton.setAttribute(
      "aria-label",
      document.fullscreenElement ? "Exit full screen" : "Enter full screen",
    );
    syncTimeline();
  }

  video.addEventListener("error", playbackError);
  audio.addEventListener("error", playbackError);
  [video, audio].forEach((player) => {
    player.addEventListener("play", syncControls);
    player.addEventListener("pause", syncControls);
    player.addEventListener("volumechange", () => {
      const expectedVolume = Math.min(100, state.volume) / 100;
      if (Math.abs(player.volume - expectedVolume) > 0.01) {
        state.volume = Math.round(player.volume * 100);
        store("volume", state.volume);
        if (boostContext) setBoostGain();
      }
      if (player.muted !== state.muted) {
        state.muted = player.muted;
        store("muted", state.muted);
      }
      syncControls();
    });
    player.addEventListener("ratechange", syncControls);
    player.addEventListener("timeupdate", () => {
      if (!state.scrubbing) syncTimeline();
    });
    player.addEventListener("durationchange", () => syncTimeline());
  });

  function folderCard(folder) {
    const article = document.createElement("article");
    article.className = "media-card folder";
    const childCount = Number(folder.child_count ?? 0);
    const childLabel = `${childCount} ${childCount === 1 ? "item" : "items"}`;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "card-button";
    button.title = `Open ${folder.title}`;
    button.setAttribute("aria-label", `Open ${folder.title}, ${childLabel}`);
    button.addEventListener("click", () => navigateFolder(folder.id));
    const art = document.createElement("span");
    art.className = "art folder-art";
    const icon = document.createElement("span");
    icon.className = "folder-icon";
    icon.setAttribute("aria-hidden", "true");
    const count = document.createElement("span");
    count.className = "folder-count";
    count.textContent = childCount > 999 ? "999+" : String(childCount);
    count.title = childLabel;
    icon.append(count);
    art.append(icon);
    const title = document.createElement("span");
    title.className = "card-title";
    title.textContent = folder.title;
    button.append(art, title);
    article.append(button);
    return article;
  }

  function mediaCard(item) {
    const article = document.createElement("article");
    article.className = `media-card ${item.kind}`;
    article.dataset.mediaId = String(item.id);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "card-button";
    button.setAttribute("aria-label", `Play ${item.title}`);
    button.addEventListener("click", () => start(item));

    const art = document.createElement("span");
    art.className = "art";
    if (item.art_url) {
      const image = document.createElement("img");
      image.loading = "lazy";
      image.alt = "";
      image.src = item.art_url;
      image.addEventListener("error", () => image.classList.add("failed"));
      art.append(image);
    }
    const play = document.createElement("span");
    play.className = "card-play";
    play.setAttribute("aria-hidden", "true");
    art.append(play);
    const fullTitle = document.createElement("span");
    fullTitle.className = "full-card-title";
    fullTitle.textContent = item.title;
    fullTitle.setAttribute("aria-hidden", "true");
    art.append(fullTitle);
    if (item.transcode_likely) {
      const badge = document.createElement("span");
      badge.className = "transcode-badge";
      badge.textContent = "COMPAT";
      badge.title = "May use browser compatibility transcoding";
      art.append(badge);
    }

    const title = document.createElement("span");
    title.className = "card-title";
    title.textContent = item.title;
    const meta = document.createElement("span");
    meta.className = "card-meta";
    const primary = document.createElement("span");
    primary.textContent = item.kind === "audio" ? (item.artist || item.album || "Audio") : (item.date ? item.date.slice(0, 4) : "Video");
    const dot = document.createElement("i");
    const secondary = document.createElement("span");
    secondary.textContent = durationLabel(item.duration) || item.ext.toUpperCase();
    meta.append(primary, dot, secondary);
    button.append(art, title, meta);
    article.append(button);
    return article;
  }

  function card(entry) {
    return entry.entry_type === "folder" ? folderCard(entry) : mediaCard(entry);
  }

  function navigationFromUrl() {
    const params = new URL(window.location.href).searchParams;
    const folder = params.get("folder");
    const view = params.get("view");
    if (folder) {
      state.view = "folders";
      state.folder = folder;
      state.kind = "all";
    } else if (["all", "video", "audio"].includes(view)) {
      state.view = "library";
      state.folder = ROOT_FOLDER;
      state.kind = view;
    } else {
      state.view = "folders";
      state.folder = ROOT_FOLDER;
      state.kind = "all";
    }
  }

  function writeNavigation(replace = false) {
    const url = new URL(window.location.href);
    url.search = "";
    if (state.view === "folders" && state.folder !== ROOT_FOLDER) {
      url.searchParams.set("folder", state.folder);
    } else if (state.view === "library") {
      url.searchParams.set("view", state.kind);
    }
    history[replace ? "replaceState" : "pushState"]({}, "", `${url.pathname}${url.search}`);
  }

  function syncViewButtons() {
    document.querySelectorAll(".filters button").forEach((button) => {
      const selected = button.dataset.view === state.view
        && (state.view === "folders" || button.dataset.kind === state.kind);
      button.classList.toggle("active", selected);
      button.setAttribute("aria-selected", String(selected));
    });
  }

  function navigateFolder(folder) {
    state.view = "folders";
    state.folder = folder;
    state.kind = "all";
    state.query = "";
    searchInput.value = "";
    syncViewButtons();
    writeNavigation();
    load(true);
  }

  function renderBreadcrumbs(items) {
    breadcrumbs.replaceChildren();
    breadcrumbs.hidden = state.view !== "folders";
    if (breadcrumbs.hidden) return;
    items.forEach((item, index) => {
      if (index > 0) {
        const separator = document.createElement("span");
        separator.textContent = "/";
        separator.setAttribute("aria-hidden", "true");
        breadcrumbs.append(separator);
      }
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = item.title;
      button.disabled = index === items.length - 1;
      button.addEventListener("click", () => navigateFolder(item.id));
      breadcrumbs.append(button);
    });
  }

  async function load(reset = false) {
    if (state.loading && !reset) return;
    if (reset) {
      state.offset = 0;
      state.entries = [];
      grid.replaceChildren();
    }
    const request = ++state.request;
    state.loading = true;
    loading.hidden = false;
    empty.hidden = true;
    loadMore.hidden = true;
    const params = new URLSearchParams({
      view: state.view,
      kind: state.kind,
      q: state.query,
      offset: String(state.offset),
      limit: String(state.limit),
    });
    if (state.view === "folders") params.set("folder", state.folder);
    try {
      const response = await fetch(`/api/web/library?${params}`, { headers: { Accept: "application/json" } });
      if (!response.ok) throw new Error(`library request returned ${response.status}`);
      const payload = await response.json();
      if (request !== state.request) return;
      document.title = `${payload.server_name} Player`;
      el("server-name").textContent = payload.server_name;
      state.transcoding = payload.transcoding_enabled;
      state.total = payload.total;
      const entries = payload.entries || payload.items || [];
      state.entries.push(...entries);
      for (const entry of entries) grid.append(card(entry));
      state.offset += entries.length;
      const noun = state.view === "folders" ? "entries" : (payload.total === 1 ? "item" : "items");
      el("library-count").textContent = `${payload.total} ${noun}`;
      searchInput.placeholder = state.view === "folders"
        ? "Filter this folder…"
        : "Search titles, artists, albums…";
      renderBreadcrumbs(payload.breadcrumbs || []);
      empty.querySelector("p").textContent = "No media found";
      empty.querySelector("small").textContent = "Try another search or wait for the library scan to finish.";
      empty.hidden = state.total !== 0;
      loadMore.hidden = state.offset >= state.total;
      markCurrentCard();
      syncControls();
    } catch (error) {
      if (request !== state.request) return;
      empty.hidden = false;
      empty.querySelector("p").textContent = "Could not load the library";
      empty.querySelector("small").textContent = error.message;
    } finally {
      if (request === state.request) {
        state.loading = false;
        loading.hidden = true;
      }
    }
  }

  let searchTimer;
  searchInput.addEventListener("input", (event) => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      state.query = event.target.value.trim();
      load(true);
    }, 220);
  });

  document.querySelectorAll(".filters button").forEach((button) => {
    button.addEventListener("click", () => {
      state.view = button.dataset.view;
      state.kind = button.dataset.kind;
      state.folder = ROOT_FOLDER;
      state.query = "";
      searchInput.value = "";
      syncViewButtons();
      writeNavigation();
      load(true);
    });
  });

  document.querySelectorAll("[data-seek]").forEach((button) => {
    button.addEventListener("click", () => {
      if (!state.current) return;
      seekTo(globalCurrentTime() + Number(button.dataset.seek));
    });
  });

  document.querySelectorAll("[data-rate]").forEach((button) => {
    button.addEventListener("click", () => {
      state.rate = Number(button.dataset.rate);
      store("rate", state.rate);
      video.playbackRate = state.rate;
      audio.playbackRate = state.rate;
      syncControls();
    });
  });

  document.querySelectorAll("[data-play-mode]").forEach((button) => {
    button.addEventListener("click", () => {
      const nextMode = button.dataset.playMode;
      if (nextMode === state.playMode) return;
      const player = state.current ? activePlayer() : null;
      const resumeAt = state.current ? globalCurrentTime() : 0;
      const remainPaused = player?.paused ?? true;
      state.playMode = nextMode;
      store("stream", state.playMode);
      syncControls();
      if (state.current) {
        const transcodeStart = nextMode === "compat" ? resumeAt : null;
        start(state.current, false, resumeAt, remainPaused, transcodeStart);
      }
    });
  });

  el("play-button").addEventListener("click", () => {
    if (!state.current) return;
    const player = activePlayer();
    if (player.paused) player.play().catch(() => {});
    else player.pause();
  });
  el("previous-button").addEventListener("click", () => playRelative(-1));
  el("next-button").addEventListener("click", () => playRelative(1));
  el("mute-button").addEventListener("click", () => {
    state.muted = !state.muted;
    if (!state.muted && state.volume === 0) {
      state.volume = 100;
      store("volume", state.volume);
      applyVolume(false);
    }
    store("muted", state.muted);
    video.muted = state.muted;
    audio.muted = state.muted;
    syncControls();
  });
  el("volume-control").addEventListener("input", (event) => {
    state.volume = Number(event.target.value);
    state.muted = state.volume === 0;
    store("volume", state.volume);
    store("muted", state.muted);
    video.muted = state.muted;
    audio.muted = state.muted;
    applyVolume(true);
    syncControls();
  });
  el("loop-button").addEventListener("click", () => {
    state.loop = !state.loop;
    store("loop", state.loop);
    video.loop = state.loop;
    audio.loop = state.loop;
    syncControls();
  });
  el("fit-button").addEventListener("click", () => {
    state.fill = !state.fill;
    store("fill", state.fill);
    video.classList.toggle("fill", state.fill);
    syncControls();
  });
  el("pip-button").addEventListener("click", async () => {
    if (state.current?.kind !== "video") return;
    try {
      if (document.pictureInPictureElement) await document.exitPictureInPicture();
      else await video.requestPictureInPicture();
    } catch (_) {
      message.textContent = "Picture-in-picture is not available for this stream.";
      message.hidden = false;
    }
  });
  async function toggleFullscreen() {
    try {
      if (document.fullscreenElement) await document.exitFullscreen();
      else await playerStage.requestFullscreen();
    } catch (_) {
      message.textContent = "Fullscreen is not available in this browser.";
      message.hidden = false;
    }
  }
  el("fullscreen-button").addEventListener("click", toggleFullscreen);
  document.addEventListener("fullscreenchange", syncControls);
  el("timeline").addEventListener("input", (event) => {
    state.scrubbing = true;
    syncTimeline(Number(event.target.value));
  });
  el("timeline").addEventListener("change", (event) => {
    const target = Number(event.target.value);
    state.scrubbing = false;
    seekTo(target);
  });
  el("timeline").addEventListener("blur", () => {
    state.scrubbing = false;
    syncTimeline();
  });
  window.addEventListener("keydown", (event) => {
    const target = event.target;
    const isEditable = target instanceof HTMLElement
      && (target.isContentEditable || ["INPUT", "SELECT", "TEXTAREA", "BUTTON"].includes(target.tagName));
    if (!state.current || event.defaultPrevented || event.repeat || isEditable) return;
    if (state.current.kind === "video"
      && event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey
      && event.key.toLowerCase() === "f") {
      event.preventDefault();
      event.stopPropagation();
      toggleFullscreen();
      return;
    }
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const ordinarySteps = {
      ArrowLeft: -10,
      ArrowRight: 10,
      ArrowUp: 30,
      ArrowDown: -30,
    };
    const shiftedSteps = {
      ArrowLeft: -60,
      ArrowRight: 60,
      ArrowUp: 300,
      ArrowDown: -300,
    };
    const delta = (event.shiftKey ? shiftedSteps : ordinarySteps)[event.key];
    if (delta === undefined) return;
    event.preventDefault();
    event.stopPropagation();
    seekTo(globalCurrentTime() + delta);
  }, { capture: true });
  el("audio-track-controls").addEventListener("change", (event) => {
    selectAudioTrack(Number(event.target.value));
  });
  video.addEventListener("click", () => {
    if (video.controls || state.current?.kind !== "video") return;
    if (video.paused) video.play().catch(() => {});
    else video.pause();
  });
  loadMore.addEventListener("click", () => load(false));
  window.addEventListener("popstate", () => {
    navigationFromUrl();
    state.query = "";
    searchInput.value = "";
    syncViewButtons();
    load(true);
  });

  navigationFromUrl();
  syncViewButtons();
  applyPlaybackPreferences(video);
  applyPlaybackPreferences(audio);
  applyVolume(false);
  syncControls();
  load(true);
})();
