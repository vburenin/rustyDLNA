import { PLAYBACK_STATES } from "./core.js";

export function initialState(navigation, preferences) {
  return {
    navigation: { ...navigation },
    server: {
      name: "rustyDLNA",
      rootFolderId: null,
      capabilities: { transcoding: false, captions: false, quality_profiles: [] },
      state: "connecting",
    },
    library: {
      status: "idle",
      entries: [],
      breadcrumbs: [],
      total: 0,
      offset: 0,
      hasMore: false,
      generation: null,
      error: null,
      requestId: 0,
    },
    queue: { entries: [], status: "idle", error: null, generation: null },
    playback: {
      sessionId: 0,
      status: "idle",
      sourceMode: null,
      sourceReason: null,
      item: null,
      intent: "paused",
      segmentOffset: 0,
      currentTime: 0,
      duration: 0,
      previewTime: null,
      message: null,
      error: null,
      selectedAudio: 0,
      audioTracks: [],
      audioTracksStatus: "idle",
      chapters: [],
      selectedCaption: preferences.caption,
      pip: false,
      fullscreen: false,
    },
    preferences: { ...preferences },
  };
}

export class Store {
  #state;
  #listeners = new Set();

  constructor(state) {
    this.#state = state;
  }

  getState() {
    return this.#state;
  }

  subscribe(listener) {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  dispatch(action) {
    const next = reduce(this.#state, action);
    if (next === this.#state) return;
    this.#state = next;
    for (const listener of this.#listeners) listener(next, action);
  }
}

function reduce(state, action) {
  switch (action.type) {
    case "NAVIGATE":
      return { ...state, navigation: { ...state.navigation, ...action.navigation } };
    case "LIBRARY_LOADING":
      return {
        ...state,
        server: { ...state.server, state: state.server.state === "ready" ? "ready" : "connecting" },
        library: {
          ...state.library,
          status: action.append ? "loading_more" : "loading",
          error: null,
          requestId: action.requestId,
          ...(action.append ? {} : { entries: [], offset: 0, hasMore: false }),
        },
      };
    case "LIBRARY_SUCCESS": {
      if (action.requestId !== state.library.requestId) return state;
      const entries = action.append ? [...state.library.entries, ...action.payload.entries] : action.payload.entries;
      return {
        ...state,
        server: {
          ...state.server,
          name: action.payload.server_name,
          rootFolderId: action.payload.root_folder_id,
          capabilities: action.payload.capabilities,
          state: action.payload.library_state === "empty" ? "empty" : "ready",
        },
        library: {
          ...state.library,
          status: "ready",
          entries,
          breadcrumbs: action.payload.breadcrumbs || [],
          total: action.payload.total,
          offset: action.payload.offset + action.payload.entries.length,
          hasMore: action.payload.has_more,
          generation: action.payload.generation,
          error: null,
        },
      };
    }
    case "LIBRARY_ERROR":
      if (action.requestId !== state.library.requestId) return state;
      return {
        ...state,
        server: { ...state.server, state: "error" },
        library: { ...state.library, status: "error", error: action.error },
      };
    case "LIBRARY_REMOVE_ENTRY": {
      const entries = state.library.entries.filter((entry) => String(entry.id) !== String(action.id));
      return {
        ...state,
        library: {
          ...state.library,
          entries,
          total: Math.max(0, state.library.total - (entries.length < state.library.entries.length ? 1 : 0)),
        },
      };
    }
    case "QUEUE_LOADING":
      return { ...state, queue: { entries: action.entries, status: "loading", error: null, generation: action.generation } };
    case "QUEUE_SUCCESS":
      return { ...state, queue: { entries: action.entries, status: "ready", error: null, generation: action.generation } };
    case "QUEUE_ERROR":
      return { ...state, queue: { ...state.queue, status: "error", error: action.error } };
    case "PLAYBACK_SELECT":
      return {
        ...state,
        playback: {
          ...state.playback,
          sessionId: action.sessionId,
          status: "idle",
          sourceMode: null,
          sourceReason: null,
          item: action.item,
          intent: "paused",
          segmentOffset: 0,
          currentTime: 0,
          duration: action.duration || 0,
          previewTime: null,
          message: null,
          error: null,
          selectedAudio: action.item.default_audio_index || 0,
          audioTracks: action.item.audio_tracks || [],
          audioTracksStatus: "idle",
          chapters: action.item.chapters || [],
          selectedCaption: state.preferences.caption,
        },
      };
    case "PLAYBACK_SOURCE":
      if (action.sessionId < state.playback.sessionId) return state;
      return {
        ...state,
        playback: {
          ...state.playback,
          sessionId: action.sessionId,
          status: "loading",
          sourceMode: action.sourceMode,
          sourceReason: action.sourceReason,
          intent: action.intent,
          segmentOffset: action.segmentOffset,
          currentTime: action.start,
          previewTime: null,
          message: action.message || null,
          error: null,
        },
      };
    case "PLAYBACK_STATUS":
      if (action.sessionId !== state.playback.sessionId || !PLAYBACK_STATES.includes(action.status)) return state;
      return {
        ...state,
        playback: {
          ...state.playback,
          status: action.status,
          ...(action.intent ? { intent: action.intent } : {}),
          ...(action.message !== undefined ? { message: action.message } : {}),
          ...(action.error !== undefined ? { error: action.error } : {}),
        },
      };
    case "PLAYBACK_TIME":
      if (action.sessionId !== state.playback.sessionId) return state;
      return {
        ...state,
        playback: {
          ...state.playback,
          currentTime: action.currentTime,
          duration: action.duration || state.playback.duration,
        },
      };
    case "PLAYBACK_PREVIEW":
      return { ...state, playback: { ...state.playback, previewTime: action.value } };
    case "PLAYBACK_ERROR":
      if (action.sessionId !== state.playback.sessionId) return state;
      return { ...state, playback: { ...state.playback, status: "error", error: action.error, message: null } };
    case "PLAYBACK_AUX":
      return { ...state, playback: { ...state.playback, ...action.values } };
    case "AUDIO_TRACKS_LOADING":
      return { ...state, playback: { ...state.playback, audioTracksStatus: "loading" } };
    case "AUDIO_TRACKS_SUCCESS":
      if (action.sessionId !== state.playback.sessionId) return state;
      return { ...state, playback: { ...state.playback, audioTracks: action.tracks, chapters: action.chapters || state.playback.chapters, audioTracksStatus: "ready" } };
    case "AUDIO_TRACKS_ERROR":
      if (action.sessionId !== state.playback.sessionId) return state;
      return { ...state, playback: { ...state.playback, audioTracksStatus: "error" } };
    case "PREFERENCE":
      return { ...state, preferences: { ...state.preferences, [action.name]: action.value } };
    default:
      return state;
  }
}
