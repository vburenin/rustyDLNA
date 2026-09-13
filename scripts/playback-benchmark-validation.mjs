// Report integrity is checked before performance conclusions. Expectations come
// from the harness workload contract, never from whichever summaries survived.
export const CONFIGURATION_KEYS = ["samples", "concurrency", "duration", "fps", "rate", "size", "recipes", "encoder", "build_profile", "tier", "sustain_seconds", "quality", "encoding_preset", "delivery", "network"];
export const ENVIRONMENT_KEYS = ["cpu", "logical_cpus", "cpu_max", "memory_max", "memory_bytes", "cgroup_ancestor_limits", "process_affinity_cpus", "browser", "ffmpeg", "ffprobe", "rust", "node", "kernel", "filesystem", "cache_conditions"];
const conditional = new Set(["active-attachment", "cancellation"]);
export const conditionalWorkload = (key) => conditional.has(key.split("/")[1]);
const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const present = (value) => value !== undefined && value !== null && value !== "";
const positiveInteger = (value) => Number.isSafeInteger(value) && value > 0;
const sha256 = (value) => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
const qualities = ["auto", "uhd_high", "uhd_optimized", "full_hd", "data_saver", "sd_480", "low_360"];
const positiveRate = (value) => typeof value === "string" && /^[1-9]\d*\/[1-9]\d*$/.test(value)
  && value.split("/").map(Number).every(positiveInteger);
const unavailable = (value) => typeof value === "string" && /^unavailable\b/.test(value);
export const usesGpu = (report) => report?.configuration?.encoder === "h264_nvenc";
export const recordLatency = (record) => record.workload === "cancellation" ? record.cancellation_ms
  : ["near", "near-paused", "restart"].includes(record.workload) ? record.seek_to_frame_ms : record.selection_to_frame_ms;

export function requiredWorkloads(configuration) {
  return (Array.isArray(configuration?.recipes) ? configuration.recipes : []).flatMap((recipe) =>
    [...(recipe === "copy" ? ["original"] : []), "cold", "warm", "active-attachment", "near", "near-paused", "restart", "cancellation"]
      .map((workload) => `${recipe}/${workload}`));
}

// Ignore collection timestamps, ASLR addresses and the candidate executable's
// hash. The candidate is allowed to change; its media/runtime environment isn't.
export function runtimeIdentity(environment) {
  const runtime = environment?.runtime;
  return {
    platform: Object.fromEntries(["system", "release", "machine", "python", "os_release"].map((key) => [key, runtime?.platform?.[key]])),
    libraries: runtime?.tools?.libav_development?.output,
    linked_libraries: runtime?.tools?.server_linked_libraries?.output?.replace(/\s*\(0x[0-9a-f]+\)/gi, "")
      .split("\n").map((line) => line.trim()).filter(Boolean).sort(),
    media_packages: runtime?.tools?.packages?.output?.split("\n")
      .filter((line) => /^(ffmpeg|libav(codec|format|util|filter|device)|libsw(scale|resample)|libpostproc|libx26[45]|libvpx|libaom|libdav1d|libplacebo|libass|libc6|libstdc\+\+)/.test(line)).sort(),
    ffmpeg: runtime?.files?.ffmpeg?.sha256,
    ffprobe: runtime?.files?.ffprobe?.sha256,
  };
}

export function validateReport(report) {
  const issues = [];
  const issue = (kind, detail) => issues.push({ kind, detail });
  if (!object(report)) return { issues: [{ kind: "invalid_report", detail: "Report must be an object" }], keys: [] };
  if (report.schema !== 2) issue("invalid_report", "Expected report schema 2");
  const config = object(report.configuration) ? report.configuration : {};
  for (const key of CONFIGURATION_KEYS) if (!present(config[key])) issue("invalid_report", `Missing configuration.${key}`);
  const recipes = config.recipes;
  const validMatrix = Array.isArray(recipes) && recipes.length > 0 && recipes.length <= 4
    && recipes.every((recipe) => ["copy", "audio", "video", "both", "external"].includes(recipe))
    && new Set(recipes).size === recipes.length && (!recipes.includes("external") || recipes.length === 1)
    && Number.isInteger(config.samples) && config.samples >= 1 && config.samples <= 1000
    && [1, 2, 4].includes(config.concurrency);
  if (!validMatrix) issue("invalid_report", "Invalid recipe/sample/concurrency matrix");
  if (!Number.isFinite(config.duration) || config.duration < 35 || config.duration > 600
    || ![24, 30, 60].includes(config.fps) || ![1, 2].includes(config.rate) || !/^\d{2,4}x\d{2,4}$/.test(config.size)
    || !["libx264", "h264_nvenc"].includes(config.encoder) || !["debug", "release", "unknown"].includes(config.build_profile)
    || !qualities.includes(config.quality)
    || !["balanced", "fast_start", "maximum_speed"].includes(config.encoding_preset)
    || !Number.isFinite(config.sustain_seconds) || config.sustain_seconds < 0.5 || config.sustain_seconds > 30
    || !Number.isFinite(config.network?.latency_ms) || config.network.latency_ms < 0 || config.network.latency_ms > 2000
    || !Number.isFinite(config.network?.kbps) || config.network.kbps < 0 || config.network.kbps > 1000000
    || config.network?.scope !== "per-viewer CDP aggregate") issue("invalid_report", "Invalid bounded workload configuration");
  const keys = validMatrix ? requiredWorkloads(config) : [];
  for (const key of ENVIRONMENT_KEYS) {
    if (!present(report.environment?.[key])) issue("invalid_report", `Missing environment.${key}`);
    else if (unavailable(report.environment[key])) issue("unavailable_environment", `Unavailable environment.${key}`);
  }
  const runtime = runtimeIdentity(report.environment);
  for (const [key, value] of Object.entries({ ...runtime.platform, libraries: runtime.libraries, ffmpeg: runtime.ffmpeg, ffprobe: runtime.ffprobe })) {
    if (!present(value) || (key === "os_release" && typeof value !== "string")) issue("invalid_report", `Missing runtime identity: ${key}`);
  }
  for (const key of ["libav_development", "server_linked_libraries", "packages"]) {
    if (report.environment?.runtime?.tools?.[key]?.status !== "ok") issue("unavailable_environment", `Runtime collection did not succeed: ${key}`);
  }
  if (!runtime.linked_libraries?.length || !runtime.media_packages?.length) issue("missing_evidence", "Missing linked media runtime identity");
  if (usesGpu(report)) {
    if (!present(report.environment?.gpu_inventory) || report.environment.gpu_inventory.startsWith("unavailable:")) {
      issue("unavailable_environment", "Device inventory unavailable for selected tier");
    }
  }
  if (report.failure) issue("failed_run", String(report.failure));
  if (!present(report.finished)) issue("incomplete_run", "Run has no completion timestamp");
  for (const field of ["fixtures", "records", "validations"]) {
    if (!Array.isArray(report[field]) || !report[field].length) issue("missing_evidence", `Empty or missing ${field}`);
  }
  if (!object(report.summaries) || !Object.keys(report.summaries).length) issue("missing_evidence", "Empty or missing summaries");
  if (!validMatrix) return { issues, keys };
  const fixtures = Array.isArray(report.fixtures) ? report.fixtures : [];
  for (const recipe of recipes) {
    const matches = fixtures.filter((entry) => entry?.recipe === recipe);
    if (matches.length !== 1 || !/^[a-f0-9]{64}$/.test(matches[0]?.sha256)) issue("missing_evidence", `Expected one hashed fixture for ${recipe}`);
  }
  if (fixtures.some((entry) => !recipes.includes(entry?.recipe))) issue("invalid_report", "Unexpected fixture recipe");
  const validations = Array.isArray(report.validations) ? report.validations : [];
  const validationIds = new Map();
  for (const validation of validations) {
    if (!object(validation) || !recipes.includes(validation.recipe) || !Number.isInteger(validation.sample)
      || validation.sample < 0 || validation.sample >= config.samples || !Number.isInteger(validation.id)
      || validationIds.has(validation.id)) {
      issue("invalid_report", "Invalid or duplicate output validation identity");
      continue;
    }
    validationIds.set(validation.id, validation);
    const streams = validation.output_probe?.streams;
    const video = Array.isArray(streams) ? streams.find((stream) => stream?.codec_type === "video") : null;
    const audio = Array.isArray(streams) ? streams.find((stream) => stream?.codec_type === "audio") : null;
    if (!video?.codec_name || !positiveInteger(video.width) || !positiveInteger(video.height)
      || typeof video.pix_fmt !== "string" || !video.pix_fmt.trim() || !positiveRate(video.r_frame_rate)
      || !audio?.codec_name || !positiveInteger(audio.channels)
      || typeof audio.sample_rate !== "string" || !/^[1-9]\d*$/.test(audio.sample_rate) || !positiveInteger(Number(audio.sample_rate))
      || !qualities.includes(validation.requested?.quality)
      || !["copy", "transcode"].includes(validation.requested?.video_mode)
      || !["copy", "transcode"].includes(validation.requested?.audio_mode)
      || (validation.requested?.video_mode === "transcode" && !["h264_sdr", "hevc_hdr10"].includes(validation.requested?.video_output))
      || !positiveInteger(validation.output_bytes) || !positiveInteger(validation.stamp_bytes)) {
      issue("missing_evidence", `Incomplete output validation ${validation.recipe}/${validation.sample}`);
    }
    const copied = validation.requested?.video_mode === "copy";
    if (copied ? validation.quality?.copied_video_frame_hashes_match !== true
      || !positiveInteger(validation.quality?.decoded_frames_compared) || !sha256(validation.quality?.decoded_hash_sha256)
      : !positiveInteger(validation.quality?.decoded_frames_sampled) || !sha256(validation.quality?.encoded_output_decoded_hash_sha256)) {
      issue("missing_evidence", `Missing decoded-frame proof ${validation.recipe}/${validation.sample}`);
    }
  }
  for (const recipe of recipes) for (let sample = 0; sample < config.samples; sample++) {
    if (validations.filter((entry) => entry?.recipe === recipe && entry.sample === sample).length !== 1) {
      issue("missing_evidence", `Expected one output validation for ${recipe}/${sample}`);
    }
  }
  const records = Array.isArray(report.records) ? report.records : [];
  const identities = new Set();
  for (const record of records) {
    if (!object(record)) { issue("invalid_report", "Record must be an object"); continue; }
    const key = `${record.recipe}/${record.workload}`;
    const viewers = ["original", "cold", "warm"].includes(record.workload) ? config.concurrency : 1;
    const viewer = record.viewer ?? 0;
    const identity = `${key}/${record.sample}/${viewer}`;
    if (!keys.includes(key) || !Number.isInteger(record.sample) || record.sample < 0 || record.sample >= config.samples
      || !Number.isInteger(viewer) || viewer < 0 || viewer >= viewers || identities.has(identity)) {
      issue("invalid_report", `Unexpected or duplicate trial ${identity}`);
      continue;
    }
    identities.add(identity);
    if (record.available === false) {
      if (!conditional.has(record.workload) || typeof record.reason !== "string" || !record.reason.trim()) {
        issue("invalid_report", `Unavailable trial lacks an allowed workload and reason: ${identity}`);
      }
      continue;
    }
    if (!Number.isFinite(recordLatency(record)) || recordLatency(record) < 0) issue("missing_evidence", `Missing latency: ${identity}`);
    if (record.workload !== "cancellation" && (!(record.browser?.frames > 0) || !object(record.browser?.rate_at_first_frame))) {
      issue("missing_evidence", `Missing presented-frame/rate observation: ${identity}`);
    }
    if (["near", "near-paused", "restart"].includes(record.workload) && !object(record.browser?.rate_at_completion)) {
      issue("missing_evidence", `Missing completed-seek rate: ${identity}`);
    }
    if (["original", "cold", "warm"].includes(record.workload) && record.sustained?.progression_within_tolerance !== true) {
      issue(record.sustained?.progression_within_tolerance === false ? "failed_run" : "missing_evidence", `Sustained progression: ${identity}`);
    }
    if (["original", "cold", "warm"].includes(record.workload)
      && (!(record.sustained?.presented_frames > 0) || !object(record.sustained?.actual_rate_start) || !object(record.sustained?.actual_rate_end))) {
      issue("missing_evidence", `Missing sustained-frame/rate observations: ${identity}`);
    }
    if (["cold", "warm"].includes(record.workload)) {
      const validation = validationIds.get(record.validation_id);
      if (validation?.recipe !== record.recipe || validation?.sample !== record.sample) issue("missing_evidence", `Missing matching output validation: ${identity}`);
    }
    if (record.workload === "cancellation" && record.helpers_reaped !== true) issue("missing_evidence", `Helper reaping unverified: ${identity}`);
    if (record.workload === "active-attachment" && !(record.active_helpers_at_dispatch > 0)) issue("missing_evidence", `Active attachment unverified: ${identity}`);
    if ([record.browser?.rate_at_first_frame, record.browser?.rate_at_completion,
      record.sustained?.actual_rate_start, record.sustained?.actual_rate_end]
      .some((rate) => rate && (rate.playback_rate !== config.rate || rate.preference_rate !== config.rate))) {
      issue("failed_run", `Actual playback rate differs: ${identity}`);
    }
  }
  for (const key of keys) for (let sample = 0; sample < config.samples; sample++) {
    const viewers = ["original", "cold", "warm"].includes(key.split("/")[1]) ? config.concurrency : 1;
    for (let viewer = 0; viewer < viewers; viewer++) {
      if (!identities.has(`${key}/${sample}/${viewer}`)) issue("missing_evidence", `Missing trial ${key}/${sample}/${viewer}`);
    }
  }
  return { issues, keys };
}
