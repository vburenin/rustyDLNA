// Dependency-free report calculations, also used by the Python quality gate.
export function summarizeRecords(records) {
  const summaries = {};
  for (const record of records) {
    const key = `${record.recipe}/${record.workload}`;
    const summary = summaries[key] ||= { values: [], trials: new Set(), unavailable: 0 };
    const value = record.selection_to_frame_ms ?? record.seek_to_frame_ms ?? record.cancellation_ms;
    if (record.available === false || !Number.isFinite(value) || value < 0) {
      summary.unavailable += 1;
      continue;
    }
    summary.values.push(value);
    summary.trials.add(record.sample);
  }
  for (const summary of Object.values(summaries)) {
    const values = summary.values.sort((a, b) => a - b);
    const quantile = (p) => {
      if (!values.length) return null;
      const position = (values.length - 1) * p;
      return values[Math.floor(position)] + (values[Math.ceil(position)] - values[Math.floor(position)]) * (position % 1);
    };
    const mean = values.length ? values.reduce((a, b) => a + b, 0) / values.length : null;
    Object.assign(summary, {
      n: values.length, independent_trials: summary.trials.size,
      p50: quantile(0.5), p95: quantile(0.95), p99: quantile(0.99), mean,
      standard_deviation: values.length > 1 ? Math.sqrt(values.reduce((n, v) => n + (v - mean) ** 2, 0) / (values.length - 1)) : null,
      min: values[0] ?? null, max: values.at(-1) ?? null,
      tail_sample_threshold_met: summary.trials.size >= 1000,
      note: summary.trials.size < 1000
        ? "Small-sample p95/p99 are descriptive, not reliable tails. Concurrent viewers within one trial are correlated."
        : "Engineering sample threshold met; this does not establish statistical confidence. Concurrent viewers within one trial are correlated.",
    });
    delete summary.trials;
  }
  return summaries;
}

export function compareReports(before, after, thresholds = {}) {
  thresholds = { median_percent: 25, median_ms: 50, p95_percent: 30, p95_ms: 100, ...thresholds };
  if (!Object.values(thresholds).every((value) => Number.isFinite(value) && value >= 0)) throw new Error("Invalid comparison thresholds");
  const comparedKeys = ["concurrency", "duration", "fps", "rate", "size", "recipes", "encoder", "build_profile", "tier", "external_fixture", "sustain_seconds", "quality", "encoding_preset", "delivery", "network"];
  const mismatches = comparedKeys.filter((key) => JSON.stringify(before.configuration?.[key]) !== JSON.stringify(after.configuration?.[key]));
  for (const key of ["cpu", "logical_cpus", "cpu_max", "memory_max", "cgroup_ancestor_limits", "process_affinity_cpus", "browser", "ffmpeg", "filesystem", "cache_conditions"]) {
    if (JSON.stringify(before.environment?.[key]) !== JSON.stringify(after.environment?.[key])) mismatches.push(`environment.${key}`);
  }
  const fixtureKeys = (report) => report.fixtures?.map(({ recipe, sha256 }) => ({ recipe, sha256 })).sort((a, b) => a.recipe.localeCompare(b.recipe));
  const beforeFixtures = fixtureKeys(before);
  const afterFixtures = fixtureKeys(after);
  if (!beforeFixtures?.length || !afterFixtures?.length || beforeFixtures.some((value) => !value.sha256)
    || JSON.stringify(beforeFixtures) !== JSON.stringify(afterFixtures)) mismatches.push("fixture bytes");
  const qualityKeys = (report) => [...new Set((report.validations || []).map((validation) => JSON.stringify({
    recipe: validation.recipe,
    requested: Object.fromEntries(["quality", "video_mode", "audio_mode", "video_output", "encoding_preset"]
      .map((key) => [key, validation.requested?.[key] ?? null])),
    streams: validation.output_probe?.streams.map((stream) => Object.fromEntries([
      "codec_type", "codec_name", "profile", "level", "width", "height", "pix_fmt", "r_frame_rate",
      "color_range", "color_space", "color_transfer", "color_primaries", "channels", "sample_rate",
    ].map((key) => [key, stream[key] ?? null]))),
    copied_frame_hash: validation.quality?.decoded_hash_sha256 ?? null,
    encoded_frame_hash: validation.quality?.encoded_output_decoded_hash_sha256 ?? null,
  })))].sort();
  const beforeQuality = qualityKeys(before);
  const afterQuality = qualityKeys(after);
  if (!beforeQuality.length || !afterQuality.length
    || JSON.stringify(beforeQuality) !== JSON.stringify(afterQuality)) mismatches.push("actual output recipe/quality");
  if (before.failure || after.failure) mismatches.push("a run failed");
  for (const [name, report] of [["before", before], ["after", after]]) {
    if (report.records?.some((record) => record.sustained?.progression_within_tolerance === false)) {
      mismatches.push(`${name} sustained progression failed`);
    }
    if (report.records?.some((record) => [record.browser?.rate_at_first_frame, record.browser?.rate_at_completion,
      record.sustained?.actual_rate_start, record.sustained?.actual_rate_end]
      .some((rate) => rate && (rate.playback_rate !== report.configuration.rate || rate.preference_rate !== report.configuration.rate)))) {
      mismatches.push(`${name} actual playback rate`);
    }
  }
  const workloads = {};
  if (!mismatches.length) for (const [key, current] of Object.entries(after.summaries)) {
    const baseline = before.summaries?.[key];
    if (!baseline?.n || !current.n) continue;
    const enoughTrials = baseline.independent_trials >= 10 && current.independent_trials >= 10;
    const budget = Math.max(thresholds.median_ms, baseline.p50 * thresholds.median_percent / 100);
    const p95Budget = Math.max(thresholds.p95_ms, baseline.p95 * thresholds.p95_percent / 100);
    workloads[key] = {
      before_p50_ms: baseline.p50, after_p50_ms: current.p50,
      delta_ms: current.p50 - baseline.p50, median_regression_budget_ms: budget,
      sufficient_trials: enoughTrials,
      observed_p95_regression_budget_ms: p95Budget,
      median_regression: enoughTrials ? current.p50 - baseline.p50 > budget : null,
      observed_p95_regression: enoughTrials ? current.p95 - baseline.p95 > p95Budget : null,
      p99_gate: baseline.tail_sample_threshold_met && current.tail_sample_threshold_met ? {
        before_ms: baseline.p99, after_ms: current.p99,
        regression: current.p99 - baseline.p99 > Math.max(thresholds.p95_ms, baseline.p99 * thresholds.p95_percent / 100),
      } : null,
      repeat_recommended: !enoughTrials || current.p50 - baseline.p50 > budget || current.p95 - baseline.p95 > p95Budget,
      tail_comparison_available: baseline.tail_sample_threshold_met && current.tail_sample_threshold_met,
    };
  }
  return { comparable: !mismatches.length, mismatches, thresholds, workloads,
    policy: "Configurable engineering guard bands, not statistical confidence: median increase must exceed both relative and absolute limits; observed p95 uses its own pair. Fewer than 10 independent trials is insufficient for gating; p99 gating is disabled below 1000 trials in either run." };
}
