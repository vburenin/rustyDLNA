import { WebApi } from "./api.js";
import { navigationFromUrl } from "./core.js";
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
  playerPanel: document.querySelector(".player-panel"),
  playerStage: required("player-stage"),
  video: required("video-player"),
  audio: required("audio-player"),
  audioStage: required("audio-stage"),
  audioArt: required("audio-art"),
  playerEmpty: required("player-empty"),
  playerEmptyText: required("player-empty-text"),
  stageProgress: required("stage-progress"),
  stageProgressLabel: required("stage-progress-label"),
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
  seekButtons: [...document.querySelectorAll("[data-seek]")],
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
  itemDetailsSummary: required("item-details-summary"),
  itemDetailsFacts: required("item-details-facts"),
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
  streamControls: required("stream-controls"),
  qualityControl: required("quality-control"),
  captionSizeControl: required("caption-size-control"),
  captionBackgroundControl: required("caption-background-control"),
  autoplayControl: required("autoplay-control"),
  nowPlaying: required("now-playing"),
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
  libraryPanel: required("library-panel"),
  grid: required("media-grid"),
  libraryEmpty: required("library-empty"),
  libraryEmptyTitle: required("library-empty-title"),
  libraryEmptyDetail: required("library-empty-detail"),
  libraryRetry: required("library-retry"),
  loading: required("loading"),
  loadMore: required("load-more"),
};

if (!dom.playerPanel || dom.tabs.length !== 5 || dom.seekButtons.length !== 2) {
  throw new Error("The embedded player document is incomplete");
}

const store = new Store(initialState(navigationFromUrl(window.location.href), loadPreferences()));
const api = new WebApi();
const player = new PlaybackController({ store, api, dom });
const library = new LibraryController({ store, api, dom, onSelect: (item, options) => player.select(item, options) });

window.addEventListener("popstate", () => {
  library.cancelPendingSearch();
  library.navigate(navigationFromUrl(window.location.href), { history: "none", focusAfterLoad: true });
});

const initialNavigation = store.getState().navigation;
library.start().then(async () => {
  if (!initialNavigation.itemId) return;
  try {
    const payload = await api.item(initialNavigation.itemId);
    payload.item.chapters = payload.chapters || [];
    player.select(payload.item, { startAt: initialNavigation.start });
  } catch (_) {
    dom.playerEmptyText.textContent = "This linked title is not available. Choose another item.";
    dom.playerStage.focus({ preventScroll: true });
  }
});
