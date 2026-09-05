import {
  chooseSource, directSourceSupported, selectedAudioRequiresCompatible,
  sourceBoundedQualityProfile, nativeHlsHevcCopyEligible, nativeHlsQualityProfile,
  encodingPreset, isAndroidDevice, isAppleMobileDevice, isSafariBrowser,
  primaryVideoCodec, hdrDisplaySupport, hdrVideoOutputCandidate,
  negotiateCompatibleStreams, saferCompatibleQualityProfile, SOURCE_MODES,
} from "./core.js";

const MAX_MEDIA_CAPABILITY_CACHE_ENTRIES = 64;
const ANDROID_MEDIA_SOURCE_TYPES = Object.freeze([
  'video/mp4; codecs="avc1.42c01f,mp4a.40.2"',
  'video/mp4; codecs="avc1.42e01f,mp4a.40.2"',
]);

export function supportsNativeHlsDelivery(player) {
  if (!isAppleMobileDevice(navigator) && !isSafariBrowser(navigator)) return false;
  try {
    return player.canPlayType("application/vnd.apple.mpegurl") !== "";
  } catch (_) {
    return false;
  }
}

export function androidMediaSourceType() {
  if (!isAndroidDevice(navigator)
    || typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  return ANDROID_MEDIA_SOURCE_TYPES.find((contentType) => {
    try {
      return globalThis.MediaSource.isTypeSupported(contentType);
    } catch (_) {
      return false;
    }
  }) || null;
}

function advertisedMediaSourceType(videoOutputs, outputId) {
  if (typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  const contentType = videoOutputs?.find((output) => output?.id === outputId)?.mse_content_type;
  if (!contentType) return null;
  try {
    return globalThis.MediaSource.isTypeSupported(contentType) ? contentType : null;
  } catch (_) {
    return null;
  }
}

export function copiedHevcHdrEncodingFallbackType(capabilities, streamNegotiation) {
  if (streamNegotiation?.video !== "copy"
    || !streamNegotiation?.outputVideoProbe?.supported
    || !streamNegotiation?.outputVideoContentType) return null;
  return advertisedMediaSourceType(capabilities?.video_outputs, "hevc_hdr10");
}

export function displayHdrSupport() {
  return hdrDisplaySupport(typeof globalThis.matchMedia === "function"
    ? (query) => globalThis.matchMedia(query)
    : null);
}

export function nativeHlsVideoOutput(item, capabilities) {
  const candidate = hdrVideoOutputCandidate(
    item,
    capabilities?.video_outputs,
  );
  if (!candidate) return "h264_sdr";
  // Safari can return an empty canPlayType result for both exact Main-10 and
  // generic hvc1 strings even though AVFoundation accepts that codec in native
  // HLS. This path is already restricted to native Apple HLS, so treat the
  // capability API as advisory and let a real media error trigger the bounded,
  // same-quality H.264 SDR recovery.
  return candidate.id;
}

function copiedAndroidMediaSourceType(item) {
  if (!isAndroidDevice(navigator)
    || !["h264", "hevc"].includes(primaryVideoCodec(item?.video_codec))
    || typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  const match = /^video\/mp4\s*;\s*codecs\s*=\s*"([^"]+)"$/i.exec(String(item?.video_content_type || ""));
  const videoCodec = match?.[1]?.split(",", 1)[0]?.trim();
  if (!/^(?:avc1|hvc1)\./i.test(videoCodec || "")) return null;
  const contentType = `video/mp4; codecs="${videoCodec},mp4a.40.2"`;
  try {
    return globalThis.MediaSource.isTypeSupported(contentType) ? contentType : null;
  } catch (_) {
    return null;
  }
}

function copiedHevcMediaSourceType(item, streamNegotiation) {
  if (isAndroidDevice(navigator)
    || isAppleMobileDevice(navigator)
    || streamNegotiation?.video !== "copy"
    || streamNegotiation?.audio !== "transcode"
    || typeof globalThis.MediaSource !== "function"
    || typeof globalThis.MediaSource.isTypeSupported !== "function") return null;
  const match = /^video\/mp4\s*;\s*codecs\s*=\s*"([^"]+)"$/i.exec(String(item?.video_content_type || ""));
  const videoCodec = match?.[1]?.split(",", 1)[0]?.trim();
  if (!/^hvc1\./i.test(videoCodec || "")) return null;
  const contentType = `video/mp4; codecs="${videoCodec},mp4a.40.2"`;
  try {
    return globalThis.MediaSource.isTypeSupported(contentType) ? contentType : null;
  } catch (_) {
    return null;
  }
}

// Synchronous native-HLS selection preserves Safari's user activation. Only
// non-native codec negotiation may return a promise; it never mutates UI state.
export class SourceSelector {
  #cache = new Map();

  prepare(item, state, player, { forceSourceMode, forceQuality, forceStreamNegotiation }) {
    const requestedMode = state.preferences.streamMode;
    // Exact browser capability results remain authoritative for codecs such as
    // HEVC. A broad container-only answer cannot validate an indexed video
    // codec that the server already knows normally needs conversion.
    const directSupport = directSourceSupported(
      item,
      (contentType) => player.canPlayType(contentType),
    );
    const selected = chooseSource({
      requestedMode,
      forcedMode: forceSourceMode,
      directSupport,
      transcoding: state.server.capabilities.transcoding,
      requiresCompatibleAudio: selectedAudioRequiresCompatible(
        state.playback.audioTracks,
        state.playback.selectedAudio,
      ),
    });
    const sourceMode = selected.mode;
    const advertisedProfiles = state.server.capabilities.quality_profiles || [];
    const selectedEncodingPreset = encodingPreset(
      state.preferences.encodingPreset, state.server.capabilities.encoding_presets || [],
    );
    const requestedOutputQuality = sourceMode === SOURCE_MODES.COMPATIBLE
      && forceQuality
      && advertisedProfiles.some((profile) => profile?.id === forceQuality)
      ? forceQuality
      : sourceMode === SOURCE_MODES.COMPATIBLE ? state.preferences.quality : null;
    const preferredOutputQuality = sourceMode === SOURCE_MODES.COMPATIBLE
      ? sourceBoundedQualityProfile(
        advertisedProfiles,
        requestedOutputQuality,
        item,
        state.server.capabilities.ai_upscale,
      )
      : null;
    const nativeHlsDelivery = sourceMode === SOURCE_MODES.COMPATIBLE
      && item.kind === "video"
      && supportsNativeHlsDelivery(player);
    const copyNativeHlsHevc = nativeHlsDelivery && !forceStreamNegotiation
      && nativeHlsHevcCopyEligible(item, preferredOutputQuality, state.preferences.hevcHlsCopy);
    const androidTranscodeEligible = sourceMode === SOURCE_MODES.COMPATIBLE
      && item.kind === "video"
      && isAndroidDevice(navigator);
    const androidMediaSourceSupport = androidTranscodeEligible ? androidMediaSourceType() : null;
    const outputQuality = nativeHlsDelivery && !copyNativeHlsHevc
      && forceStreamNegotiation?.video !== "copy"
      ? nativeHlsQualityProfile(
        advertisedProfiles,
        preferredOutputQuality,
        isAppleMobileDevice(navigator),
      )
      : preferredOutputQuality;
    const streamNegotiation = sourceMode !== SOURCE_MODES.COMPATIBLE ? null
      : forceStreamNegotiation || (nativeHlsDelivery ? {
        video: copyNativeHlsHevc ? "copy" : "transcode",
        audio: "transcode",
        videoOutput: copyNativeHlsHevc ? null : nativeHlsVideoOutput(item, state.server.capabilities),
        hdrDisplay: displayHdrSupport(),
      } : null);
    return {
      sourceMode, sourceReason: selected.reason, blocked: selected.blocked,
      outputQuality, encodingPreset: selectedEncodingPreset, streamNegotiation,
      nativeHlsDelivery, mediaSourceDelivery: false, mediaSourceType: null,
      androidTranscodeEligible, androidMediaSourceSupport,
    };
  }

  resolve(plan, item, state, player) {
    if (plan.sourceMode !== SOURCE_MODES.COMPATIBLE) return plan;
    if (plan.streamNegotiation) return this.#delivery(plan, item, state);
    const selectedTrack = state.playback.audioTracks
      .find((track) => Number(track.index) === Number(state.playback.selectedAudio));
    const capabilities = state.server.capabilities;
    return negotiateCompatibleStreams({
      item,
      track: selectedTrack,
      quality: plan.outputQuality,
      qualityProfile: capabilities.quality_profiles?.find((profile) => profile?.id === plan.outputQuality),
      videoOutputs: capabilities.video_outputs || [],
      aiUpscale: capabilities.ai_upscale,
      hdrDisplay: displayHdrSupport(),
      canPlayType: (contentType) => player.canPlayType(contentType),
      decodingInfo: typeof navigator.mediaCapabilities?.decodingInfo === "function"
        ? (configuration) => this.#decodingInfo(configuration) : null,
    }).then((streamNegotiation) => this.#delivery({ ...plan, streamNegotiation }, item, state));
  }

  #delivery(plan, item, state) {
    const { nativeHlsDelivery, androidTranscodeEligible, androidMediaSourceSupport } = plan;
    const advertisedProfiles = state.server.capabilities.quality_profiles || [];
    let { streamNegotiation, outputQuality, mediaSourceType, mediaSourceDelivery } = plan;
    if (androidTranscodeEligible) {
      const copiedMediaSourceSupport = streamNegotiation?.video === "copy"
        ? copiedAndroidMediaSourceType(item)
        : null;
      const selectedTrack = state.playback.audioTracks
        .find((track) => Number(track.index) === Number(state.playback.selectedAudio));
      const copiedAac = streamNegotiation?.audio === "copy"
        && String(selectedTrack?.codec || item.audio_codec || "").toLowerCase() === "aac";
      const advertisedHdrMediaSourceSupport = streamNegotiation?.videoOutput === "hevc_hdr10"
        ? advertisedMediaSourceType(
          state.server.capabilities.video_outputs,
          streamNegotiation.videoOutput,
        )
        : null;
      if (streamNegotiation?.videoOutput === "hevc_hdr10"
        && !advertisedHdrMediaSourceSupport) {
        streamNegotiation = {
          ...streamNegotiation,
          video: "transcode",
          videoOutput: "h264_sdr",
        };
      }
      mediaSourceType = copiedMediaSourceSupport
        || advertisedHdrMediaSourceSupport
        || androidMediaSourceSupport;
      if (mediaSourceType) {
        // Android's native loader can leave growing fragmented MP4 attached
        // without ever decoding it. Use finite MSE resources for every
        // compatible video. Preserve supported H.264/HEVC video and AAC;
        // otherwise select the portable H.264/AAC pair before requesting.
        streamNegotiation = {
          ...streamNegotiation,
          video: copiedMediaSourceSupport ? "copy" : "transcode",
          audio: copiedAac ? "copy" : "transcode",
          videoOutput: advertisedHdrMediaSourceSupport ? "hevc_hdr10" : "h264_sdr",
        };
        if (!copiedMediaSourceSupport
          && !advertisedHdrMediaSourceSupport
          && outputQuality === "auto") {
          outputQuality = saferCompatibleQualityProfile(advertisedProfiles, outputQuality)
            || outputQuality;
        }
        mediaSourceDelivery = true;
      }
    }
    const copiedHevcMediaSourceSupport = !nativeHlsDelivery && copiedHevcMediaSourceType(item, streamNegotiation);
    if (copiedHevcMediaSourceSupport) {
      mediaSourceType = copiedHevcMediaSourceSupport;
      mediaSourceDelivery = true;
    }
    const encodedMediaSourceSupport = !nativeHlsDelivery
      && streamNegotiation?.video === "transcode"
      && ["h264_sdr", "hevc_hdr10"].includes(streamNegotiation?.videoOutput)
      ? advertisedMediaSourceType(
        state.server.capabilities.video_outputs,
        streamNegotiation.videoOutput,
      )
      : null;
    if (encodedMediaSourceSupport) {
      // A native Chromium media loader can treat the currently available
      // tail of a growing fragmented MP4 as EOF after a compatible seek.
      // Feed encoded output through fixed complete fragments whenever the
      // browser accepts its exact SourceBuffer type. This also bounds the
      // amount of output produced ahead of playback; a browser without that
      // exact support retains the portable native MP4 fallback.
      mediaSourceType = encodedMediaSourceSupport;
      mediaSourceDelivery = true;
    }
    return { ...plan, streamNegotiation, outputQuality, mediaSourceType, mediaSourceDelivery };
  }

  #decodingInfo(configuration) {
    const key = JSON.stringify(configuration);
    if (this.#cache.has(key)) {
      const cached = this.#cache.get(key);
      // Refresh insertion order so the bounded map behaves as a small LRU.
      this.#cache.delete(key);
      this.#cache.set(key, cached);
      return Promise.resolve(cached);
    }
    // Cache only completed probes. A browser promise that never settles is
    // timed out by negotiation and must not poison every later title with the
    // same codec configuration.
    return navigator.mediaCapabilities.decodingInfo(configuration).then((result) => {
      while (this.#cache.size >= MAX_MEDIA_CAPABILITY_CACHE_ENTRIES) {
        this.#cache.delete(this.#cache.keys().next().value);
      }
      this.#cache.set(key, result);
      return result;
    });
  }
}
