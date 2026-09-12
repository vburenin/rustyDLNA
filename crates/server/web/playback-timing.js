// Browser-local monotonic timings. Server stages use the server's own clock;
// these durations are never computed by subtracting a server timestamp.
const records = [];
const buckets = [50, 100, 250, 500, 1000, 2000, 5000, 10000, 30000, 120000, Infinity];
const histograms = new Map();
const kinds = new Set(["selection", "seek_original", "seek_buffered", "seek_restarted"]);
const stages = new Set(["selection", "metadata_ready", "negotiation_start", "negotiation_complete", "source_request",
  "mse_playlist_received", "mse_init_fetched", "mse_init_appended", "mse_first_fragment_fetched",
  "mse_first_fragment_appended", "canplay", "playing", "first_presented_frame"]);

export function playbackTimingSnapshot() {
  return { records: records.map((record) => structuredClone(record)),
    histograms: Object.fromEntries([...histograms].map(([key, value]) => [key, { ...value, buckets: [...value.buckets] }])),
    bucket_upper_ms: buckets.map((value) => Number.isFinite(value) ? value : null) };
}

export class PlaybackTiming {
  constructor(kind, start = performance.now()) {
    this.kind = kinds.has(kind) ? kind : "selection";
    this.startedAt = Number.isFinite(start) && start >= 0 && start <= performance.now() ? start : performance.now();
    this.stages = {};
    this.finished = false;
  }
  mark(stage) {
    if (!this.finished && stages.has(stage) && this.stages[stage] === undefined) {
      this.stages[stage] = Math.max(0, performance.now() - this.startedAt);
    }
  }
  finish(source, estimated = false, observedAt = performance.now()) {
    if (this.finished || !source.active || !Number.isFinite(observedAt)
      || observedAt < this.startedAt || observedAt > performance.now()) return;
    // A paused seek can present its only frame before seeked confirms the
    // target. Preserve that observation time when confirmation arrives later.
    this.stages.first_presented_frame = observedAt - this.startedAt;
    this.finished = true;
    const plan = source.plan;
    const record = { kind: this.kind, duration_ms: this.stages.first_presented_frame, stages: { ...this.stages },
      estimated, recipe: { source: plan.sourceMode, video: plan.streamNegotiation?.video || "original",
        audio: plan.streamNegotiation?.audio || "original", output: plan.streamNegotiation?.videoOutput || "original",
        quality: plan.outputQuality || "original", delivery: plan.mediaSourceDelivery ? "mse" : plan.nativeHlsDelivery ? "hls" : "mp4" } };
    records.push(record);
    if (records.length > 64) records.shift();
    const key = `${record.kind}:${estimated ? "estimated" : "presented"}`;
    const histogram = histograms.get(key) || { count: 0, sum_ms: 0, max_ms: 0, buckets: buckets.map(() => 0) };
    histogram.count += 1;
    histogram.sum_ms += record.duration_ms;
    histogram.max_ms = Math.max(histogram.max_ms, record.duration_ms);
    buckets.forEach((upper, index) => { if (record.duration_ms <= upper) histogram.buckets[index] += 1; });
    histograms.set(key, histogram);
    globalThis.dispatchEvent?.(new CustomEvent("rustydlna-playback-timing", { detail: structuredClone(record) }));
  }
}
