import { WebApi } from "./api.js";
import { LAYOUT_MODES, navigationFromUrl, navigationUrl } from "./core.js";
import { LibraryController } from "./library.js";
import { PlaybackController } from "./player.js";
import { loadPreferences } from "./preferences.js";
import { initialState, Store } from "./store.js";

function required(id) {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Required player element #${id} is missing`);
  return element;
}

const dom = {
  serverName: required("server-name"),
  serverState: required("server-state"),
  libraryCount: required("library-count"),
  libraryRetryTop: required("library-retry-top"),
  appMain: required("app-main"),
  layoutBrowse: required("layout-browse"),
  layoutWatch: required("layout-watch"),
  playerPanel: required("player-panel"),
  playerStage: required("player-stage"),
  closePlayerButton: required("close-player-button"),
  video: required("video-player"),
  videoFrameHold: required("video-frame-hold"),
  audio: required("audio-player"),
  audioStage: required("audio-stage"),
  audioArt: required("audio-art"),
  playerEmpty: required("player-empty"),
  playerEmptyText: required("player-empty-text"),
  stageProgress: required("stage-progress"),
  stageProgressLabel: required("stage-progress-label"),
  seekGestureFeedback: required("seek-gesture-feedback"),
  resumePrompt: required("resume-prompt"),
  resumeTime: required("resume-time"),
  resumeButton: required("resume-button"),
  startOverButton: required("start-over-button"),
  playbackControls: required("playback-controls"),
  streamInfoButton: required("stream-info-button"),
  streamInfoDialog: required("stream-info-dialog"),
  streamInfoSummary: required("stream-info-summary"),
  sourceStreamFacts: required("source-stream-facts"),
  outputStreamFacts: required("output-stream-facts"),
  timeline: required("timeline"),
  timelineStatus: required("timeline-status"),
  timelineCurrent: required("timeline-current"),
  timelineDuration: required("timeline-duration"),
  chapterMarkers: required("chapter-markers"),
  previousButton: required("previous-button"),
  nextButton: required("next-button"),
  playButton: required("play-button"),
  muteButton: required("mute-button"),
  captionsButton: required("captions-button"),
  loopButton: required("loop-button"),
  fitButton: required("fit-button"),
  pipButton: required("pip-button"),
  fullscreenButton: required("fullscreen-button"),
  shortcutHelpButton: required("shortcut-help-button"),
  shortcutDialog: required("shortcut-dialog"),
  itemDetailsDialog: required("item-details-dialog"),
  itemDetailsTitle: required("item-details-title"),
  itemDetailsAbout: required("item-details-about"),
  itemDetailsSummary: required("item-details-summary"),
  itemDetailsPlot: required("item-details-plot"),
  itemDetailsPlotText: required("item-details-plot-text"),
  itemDetailsFacts: required("item-details-facts"),
  itemDetailsDownload: required("item-details-download"),
  volumeControl: required("volume-control"),
  volumeValue: required("volume-value"),
  speedControl: required("speed-control"),
  audioTrackControl: required("audio-track-control"),
  audioTrackControls: required("audio-track-controls"),
  audioTrackStatus: required("audio-track-status"),
  audioTrackRetry: required("audio-track-retry"),
  chapterControl: required("chapter-control"),
  chapterControls: required("chapter-controls"),
  captionMenu: required("caption-menu"),
  captionChoices: required("caption-choices"),
  advancedPlaybackButton: required("advanced-playback-button"),
  advancedPlaybackDialog: required("advanced-playback-dialog"),
  qualityMenuButton: required("quality-menu-button"),
  qualityDialog: required("quality-dialog"),
  qualityChoices: required("quality-choices"),
  streamControls: required("stream-controls"),
  qualityControl: required("quality-control"),
  captionSizeControl: required("caption-size-control"),
  captionBackgroundControl: required("caption-background-control"),
  autoplayControl: required("autoplay-control"),
  nowPlaying: required("now-playing"),
  showPlayer: required("show-player"),
  nowPlayingTitle: required("now-playing-title"),
  nowPlayingMeta: required("now-playing-meta"),
  playbackMode: required("playback-mode"),
  modeLabel: required("mode-label"),
  queuePosition: required("queue-position"),
  playerMessage: required("player-message"),
  playerMessageText: required("player-message-text"),
  playerRetry: required("player-retry"),
  tryCompatible: required("try-compatible"),
  playOriginal: required("play-original"),
  returnLibrary: required("return-library"),
  technicalDetails: required("technical-details"),
  technicalMessage: required("technical-message"),
  playbackLive: required("playback-live"),
  searchInput: required("search-input"),
  sortControl: required("sort-control"),
  tabs: [...document.querySelectorAll('[role="tab"]')],
  breadcrumbs: required("breadcrumbs"),
  resultsSummary: required("results-summary"),
  libraryLive: required("library-live"),
  libraryPanel: required("library-panel"),
  grid: required("media-grid"),
  libraryEmpty: required("library-empty"),
  libraryEmptyTitle: required("library-empty-title"),
  libraryEmptyDetail: required("library-empty-detail"),
  libraryRetry: required("library-retry"),
  loading: required("loading"),
  loadingLabel: required("loading-label"),
  loadMoreSentinel: required("load-more-sentinel"),
};

if (dom.tabs.length !== 5) {
  throw new Error("The embedded player document is incomplete");
}

const store = new Store(initialState(navigationFromUrl(window.location.href), loadPreferences()));
const api = new WebApi();
const layoutScroll = { [LAYOUT_MODES.BROWSE]: 0, [LAYOUT_MODES.WATCH]: 0 };
const landscapeTouch = window.matchMedia("(orientation: landscape) and (pointer: coarse) and (max-height: 520px)");
const landscapeWatchHeightProperty = "--landscape-watch-content-height";
let scrollFrame = null;
let landscapeViewportFrame = null;

function updateLandscapeWatchViewport() {
  if (!landscapeTouch.matches
    || store.getState().navigation.layout !== LAYOUT_MODES.WATCH) {
    document.documentElement.style.removeProperty(landscapeWatchHeightProperty);
    return;
  }
  const viewportHeight = window.visualViewport?.height ?? window.innerHeight;
  const topbarHeight = document.querySelector(".topbar")?.getBoundingClientRect().height ?? 0;
  const contentHeight = viewportHeight - topbarHeight;
  if (Number.isFinite(contentHeight) && contentHeight > 0) {
    document.documentElement.style.setProperty(landscapeWatchHeightProperty, `${contentHeight}px`);
  }
}

function scheduleLandscapeWatchViewport() {
  if (landscapeViewportFrame !== null) return;
  landscapeViewportFrame = window.requestAnimationFrame(() => {
    landscapeViewportFrame = null;
    updateLandscapeWatchViewport();
  });
}

function alignLandscapeWatch() {
  if (!landscapeTouch.matches
    || store.getState().navigation.layout !== LAYOUT_MODES.WATCH) return;
  updateLandscapeWatchViewport();
  if (scrollFrame !== null) window.cancelAnimationFrame(scrollFrame);
  const align = () => {
    if (!landscapeTouch.matches
      || store.getState().navigation.layout !== LAYOUT_MODES.WATCH) return;
    updateLandscapeWatchViewport();
    window.scrollTo({ top: 0, left: 0, behavior: "auto" });
  };
  scrollFrame = window.requestAnimationFrame(() => {
    scrollFrame = null;
    align();
    window.requestAnimationFrame(align);
  });
}

if (typeof landscapeTouch.addEventListener === "function") {
  landscapeTouch.addEventListener("change", (event) => {
    if (event.matches) alignLandscapeWatch();
    else updateLandscapeWatchViewport();
  });
} else {
  landscapeTouch.addListener((event) => {
    if (event.matches) alignLandscapeWatch();
    else updateLandscapeWatchViewport();
  });
}
window.addEventListener("resize", scheduleLandscapeWatchViewport, { passive: true });
window.visualViewport?.addEventListener("resize", scheduleLandscapeWatchViewport, { passive: true });

function renderLayout(state = store.getState()) {
  const layout = state.navigation.layout;
  const browsing = layout === LAYOUT_MODES.BROWSE;
  dom.appMain.dataset.layout = layout;
  dom.playerPanel.hidden = browsing;
  dom.layoutBrowse.setAttribute("aria-pressed", String(browsing));
  dom.layoutWatch.setAttribute("aria-pressed", String(!browsing));
  const title = state.playback.item?.title;
  dom.showPlayer.setAttribute(
    "aria-label",
    `${browsing ? "Show" : "Focus"} player${title ? ` for ${title}` : ""}`,
  );
}

function setLayout(layout, { history = "replace", restoreScroll = true } = {}) {
  if (!Object.values(LAYOUT_MODES).includes(layout)) return false;
  const state = store.getState();
  const current = state.navigation.layout;
  if (current === layout) return false;
  layoutScroll[current] = window.scrollY;
  store.dispatch({ type: "NAVIGATE", navigation: { layout } });
  if (history !== "none") {
    const target = navigationUrl(
      window.location.href,
      store.getState().navigation,
      store.getState().server.rootFolderId,
    );
    window.history.replaceState({}, "", target);
  }
  if (scrollFrame !== null) window.cancelAnimationFrame(scrollFrame);
  if (layout === LAYOUT_MODES.WATCH && landscapeTouch.matches) {
    alignLandscapeWatch();
  } else if (restoreScroll) {
    updateLandscapeWatchViewport();
    scrollFrame = window.requestAnimationFrame(() => {
      scrollFrame = null;
      window.scrollTo({ top: layoutScroll[layout], left: 0, behavior: "auto" });
    });
  } else {
    updateLandscapeWatchViewport();
  }
  return true;
}

function focusLibrary() {
  setLayout(LAYOUT_MODES.BROWSE);
  window.requestAnimationFrame(() => dom.libraryPanel.focus({ preventScroll: true }));
}

function focusPlayer() {
  setLayout(LAYOUT_MODES.WATCH);
  window.requestAnimationFrame(() => dom.playerStage.focus({ preventScroll: true }));
}

function closePlaybackToLibrary() {
  store.dispatch({ type: "NAVIGATE", navigation: { itemId: null, start: 0 } });
  library?.markCurrent(null);
  if (!setLayout(LAYOUT_MODES.BROWSE)) {
    const state = store.getState();
    window.history.replaceState(
      {},
      "",
      navigationUrl(window.location.href, state.navigation, state.server.rootFolderId),
    );
  }
  window.requestAnimationFrame(() => dom.libraryPanel.focus({ preventScroll: true }));
}

renderLayout();
if (landscapeTouch.matches && store.getState().navigation.layout === LAYOUT_MODES.WATCH) {
  alignLandscapeWatch();
} else {
  updateLandscapeWatchViewport();
}
const player = new PlaybackController({
  store,
  api,
  dom,
  onReturnLibrary: focusLibrary,
  onClosePlayback: closePlaybackToLibrary,
});
store.subscribe((state) => renderLayout(state));

let navigationEpoch = 0;
let navigationController = null;

function supersedePendingNavigation() {
  const pendingController = navigationController;
  navigationEpoch += 1;
  pendingController?.abort();
  navigationController = null;
  if (!pendingController) return;
  const committedItem = store.getState().playback.item;
  store.dispatch({
    type: "NAVIGATE",
    navigation: {
      itemId: committedItem ? String(committedItem.id) : null,
      start: 0,
      ...(committedItem ? {} : { layout: LAYOUT_MODES.BROWSE }),
    },
  });
}

let library;
library = new LibraryController({
  store,
  api,
  dom,
  onNavigate: supersedePendingNavigation,
  onSelect: (item, options) => {
    setLayout(LAYOUT_MODES.WATCH, { restoreScroll: false });
    return player.select(item, options);
  },
});

dom.layoutBrowse.addEventListener("click", () => setLayout(LAYOUT_MODES.BROWSE));
dom.layoutWatch.addEventListener("click", () => setLayout(LAYOUT_MODES.WATCH));
dom.showPlayer.addEventListener("click", focusPlayer);

function navigationIsCurrent(epoch, controller) {
  return epoch === navigationEpoch && !controller.signal.aborted;
}

async function applyNavigation(navigation, { initial = false } = {}) {
  const epoch = ++navigationEpoch;
  navigationController?.abort();
  const controller = new AbortController();
  navigationController = controller;
  try {
    if (initial) {
      await library.start();
    } else {
      library.cancelPendingSearch();
      await library.navigate(navigation, {
        history: "none",
        focusAfterLoad: true,
        supersedePending: false,
      });
    }
    if (!navigationIsCurrent(epoch, controller) || !navigation.itemId) return;
    const payload = await api.item(navigation.itemId, { signal: controller.signal });
    if (!navigationIsCurrent(epoch, controller)) return;
    payload.item.chapters = payload.chapters || [];
    await player.select(payload.item, { startAt: navigation.start, signal: controller.signal });
    if (!navigationIsCurrent(epoch, controller)) return;
  } catch (_) {
    if (!navigationIsCurrent(epoch, controller)) return;
    dom.playerEmptyText.textContent = "This linked title is not available. Choose another item.";
    dom.playerStage.focus({ preventScroll: true });
  } finally {
    if (epoch === navigationEpoch) navigationController = null;
  }
}

window.addEventListener("popstate", () => {
  void applyNavigation(navigationFromUrl(window.location.href));
});

void applyNavigation(store.getState().navigation, { initial: true });
