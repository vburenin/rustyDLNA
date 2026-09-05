import { PLAYBACK_STATES, validQualityProfileId } from "./core.js";

function negotiationCapabilityKey(capabilities) {
  const profiles = Array.isArray(capabilities?.quality_profiles)
    ? capabilities.quality_profiles
      .filter((profile) => validQualityProfileId(profile?.id))
      .map((profile) => [
        profile.id,
        profile.max_width,
        profile.max_height,
        profile.max_fps,
        profile.max_video_kbps,
        profile.automatic_fallback === true,
      ])
    : null;
  const videoOutputs = Array.isArray(capabilities?.video_outputs)
    ? capabilities.video_outputs.map((output) => [
      output?.id,
      output?.video_content_type,
      output?.mse_content_type,
      output?.dynamic_range,
      output?.hdr_metadata_type,
      output?.color_gamut,
      output?.transfer_function,
    ])
    : null;
  return JSON.stringify({
    transcoding: capabilities?.transcoding === true,
    profiles,
    videoOutputs,
  });
}

export function initialState(navigation, preferences) {
  return {
    navigation: { ...navigation },
    server: {
      name: "rustyDLNA",
      rootFolderId: null,
      capabilities: {
        transcoding: false,
        captions: false,
        quality_profiles: [],
        video_outputs: [],
        ai_upscale: null,
      },
      negotiationEpoch: 0,
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
    queue: { entries: [], status: "idle", error: null, generation: null, requestId: 0 },
    playback: {
      sessionId: 0,
      status: "idle",
      sourceMode: null,
      sourceReason: null,
      outputQuality: null,
      encodingPreset: "balanced",
      nativeHlsDelivery: false,
      mediaSourceDelivery: false,
      autoplayBlocked: false,
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
      streamNegotiation: null,
      chapters: [],
      selectedCaption: "off",
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
      const capabilities = action.payload.capabilities || {};
      const negotiationChanged = negotiationCapabilityKey(capabilities)
        !== negotiationCapabilityKey(state.server.capabilities);
      return {
        ...state,
        server: {
          ...state.server,
          name: action.payload.server_name,
          rootFolderId: action.payload.root_folder_id,
          capabilities,
          negotiationEpoch: state.server.negotiationEpoch + Number(negotiationChanged),
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
      if (action.requestId <= state.queue.requestId) return state;
      return {
        ...state,
        queue: {
          entries: action.entries,
          status: "loading",
          error: null,
          generation: action.generation,
          requestId: action.requestId,
        },
      };
    case "QUEUE_SUCCESS":
      if (action.requestId !== state.queue.requestId) return state;
      return { ...state, queue: { ...state.queue, entries: action.entries, status: "ready", error: null, generation: action.generation } };
    case "QUEUE_ERROR":
      if (action.requestId !== state.queue.requestId) return state;
      return { ...state, queue: { ...state.queue, status: "error", error: action.error } };
    case "QUEUE_REPLACE":
      return {
        ...state,
        queue: {
          entries: action.entries,
          status: "ready",
          error: null,
          generation: action.generation,
          requestId: state.queue.requestId + 1,
        },
      };
    case "PLAYBACK_SELECT":
      return {
        ...state,
        playback: {
          ...state.playback,
          sessionId: action.sessionId,
          status: "idle",
          sourceMode: null,
          sourceReason: null,
          outputQuality: null,
          nativeHlsDelivery: false,
          mediaSourceDelivery: false,
          autoplayBlocked: false,
          item: action.item,
          intent: "paused",
          segmentOffset: 0,
          currentTime: 0,
          duration: action.duration || 0,
          previewTime: null,
          message: null,
          error: null,
          selectedAudio: action.item.default_audio_index ?? 0,
          audioTracks: action.item.audio_tracks || [],
          audioTracksStatus: "idle",
          streamNegotiation: null,
          chapters: action.item.chapters || [],
          selectedCaption: "off",
          pip: false,
        },
      };
    case "PLAYBACK_CLEAR":
      if (action.sessionId < state.playback.sessionId) return state;
      return {
        ...state,
        queue: {
          entries: [],
          status: "idle",
          error: null,
          generation: null,
          requestId: state.queue.requestId + 1,
        },
        playback: {
          ...state.playback,
          sessionId: action.sessionId,
          status: "idle",
          sourceMode: null,
          sourceReason: null,
          outputQuality: null,
          nativeHlsDelivery: false,
          mediaSourceDelivery: false,
          autoplayBlocked: false,
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
          streamNegotiation: null,
          chapters: [],
          selectedCaption: "off",
          pip: false,
          fullscreen: false,
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
          outputQuality: action.outputQuality || null,
          encodingPreset: action.encodingPreset || "balanced",
          nativeHlsDelivery: action.nativeHlsDelivery === true,
          mediaSourceDelivery: action.mediaSourceDelivery === true,
          autoplayBlocked: false,
          intent: action.intent,
          segmentOffset: action.segmentOffset,
          currentTime: action.start,
          previewTime: null,
          streamNegotiation: null,
          message: action.message || null,
          error: null,
          ...(action.pip !== undefined ? { pip: action.pip } : {}),
        },
      };
    case "PLAYBACK_STATUS":
      if (action.sessionId !== state.playback.sessionId || !PLAYBACK_STATES.includes(action.status)) return state;
      return {
        ...state,
        playback: {
          ...state.playback,
          status: action.status,
          ...(action.autoplayBlocked !== undefined
            ? { autoplayBlocked: action.autoplayBlocked === true }
            : {}),
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
      if (action.sessionId !== state.playback.sessionId) return state;
      return { ...state, playback: { ...state.playback, previewTime: action.value } };
    case "PLAYBACK_ERROR":
      if (action.sessionId !== state.playback.sessionId) return state;
      return {
        ...state,
        playback: {
          ...state.playback,
          status: "error",
          autoplayBlocked: false,
          error: action.error,
          message: null,
        },
      };
    case "PLAYBACK_AUX":
      if (action.sessionId !== state.playback.sessionId) return state;
      return { ...state, playback: { ...state.playback, ...action.values } };
    case "AUDIO_TRACKS_LOADING":
      if (action.sessionId !== state.playback.sessionId) return state;
      return { ...state, playback: { ...state.playback, audioTracksStatus: "loading" } };
    case "AUDIO_TRACKS_SUCCESS":
      if (action.sessionId !== state.playback.sessionId) return state;
      return {
        ...state,
        playback: {
          ...state.playback,
          item: action.item ? { ...state.playback.item, ...action.item } : state.playback.item,
          selectedAudio: state.playback.selectedAudio
            === (state.playback.item?.default_audio_index ?? 0)
            ? action.item?.default_audio_index ?? state.playback.selectedAudio
            : state.playback.selectedAudio,
          audioTracks: action.tracks,
          chapters: action.chapters || state.playback.chapters,
          audioTracksStatus: "ready",
        },
      };
    case "AUDIO_TRACKS_ERROR":
      if (action.sessionId !== state.playback.sessionId) return state;
      return { ...state, playback: { ...state.playback, audioTracksStatus: "error" } };
    case "PREFERENCE":
      return {
        ...state,
        server: action.name === "quality" && action.value !== state.preferences.quality
          ? { ...state.server, negotiationEpoch: state.server.negotiationEpoch + 1 }
          : state.server,
        preferences: { ...state.preferences, [action.name]: action.value },
      };
    default:
      return state;
  }
}
