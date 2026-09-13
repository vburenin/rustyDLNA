// Dependency-free report calculations, also used by the Python quality gate.
import { CONFIGURATION_KEYS, ENVIRONMENT_KEYS, conditionalWorkload, recordLatency, runtimeIdentity, usesGpu, validateReport } from "./playback-benchmark-validation.mjs";

export function summarizeRecords(records) {
  const summaries = {};
  for (const record of records) {
    const key = `${record.recipe}/${record.workload}`;
    const summary = summaries[key] ||= { values: [], trials: new Set(), unavailable: 0 };
    const value = recordLatency(record);
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

export function compareReports(before, after, thresholds = {}, exclusions = {}) {
  thresholds = { median_percent: 25, median_ms: 50, p95_percent: 30, p95_ms: 100, ...thresholds };
  if (!Object.values(thresholds).every((value) => Number.isFinite(value) && value >= 0)) throw new Error("Invalid comparison thresholds");
  const arms = { before: validateReport(before), after: validateReport(after) };
  const keys = [...new Set([...arms.before.keys, ...arms.after.keys])].sort();
  if (!exclusions || Array.isArray(exclusions) || typeof exclusions !== "object"
    || Object.entries(exclusions).some(([key, reason]) => !keys.includes(key) || !conditionalWorkload(key) || typeof reason !== "string" || !reason.trim())) {
    throw new Error("Exclusions must map required attachment/cancellation workloads to explicit nonempty reasons");
  }
  before ||= {};
  after ||= {};
  const mismatches = [...CONFIGURATION_KEYS.filter((key) => key !== "samples"), "external_fixture"]
    .filter((key) => JSON.stringify(before.configuration?.[key]) !== JSON.stringify(after.configuration?.[key]));
  for (const key of ENVIRONMENT_KEYS) {
    if (JSON.stringify(before.environment?.[key]) !== JSON.stringify(after.environment?.[key])) mismatches.push(`environment.${key}`);
  }
  if (JSON.stringify(runtimeIdentity(before.environment)) !== JSON.stringify(runtimeIdentity(after.environment))) mismatches.push("environment.runtime identity");
  if (usesGpu(before) || usesGpu(after)) {
    if (before.environment?.gpu_inventory !== after.environment?.gpu_inventory) mismatches.push("environment.gpu_inventory");
  }
  const fixtureKeys = (report) => (Array.isArray(report.fixtures) ? report.fixtures : []).filter(Boolean)
    .map(({ recipe, sha256 }) => ({ recipe, sha256 })).sort((a, b) => String(a.recipe).localeCompare(String(b.recipe)));
  const beforeFixtures = fixtureKeys(before);
  const afterFixtures = fixtureKeys(after);
  if (!beforeFixtures?.length || !afterFixtures?.length || beforeFixtures.some((value) => !value.sha256)
    || JSON.stringify(beforeFixtures) !== JSON.stringify(afterFixtures)) mismatches.push("fixture bytes");
  const qualityKeys = (report) => [...new Set((Array.isArray(report.validations) ? report.validations : []).filter(Boolean).map((validation) => JSON.stringify({
    recipe: validation.recipe,
    requested: Object.fromEntries(["quality", "video_mode", "audio_mode", "video_output", "encoding_preset"]
      .map((key) => [key, validation.requested?.[key] ?? null])),
    streams: (Array.isArray(validation.output_probe?.streams) ? validation.output_probe.streams : []).filter(Boolean).map((stream) => Object.fromEntries([
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
  for (const [name, report] of [["before", before], ["after", after]]) {
    for (const recipe of new Set((Array.isArray(report.validations) ? report.validations : []).map((value) => value?.recipe))) {
      if (qualityKeys({ validations: report.validations.filter((value) => value?.recipe === recipe) }).length !== 1) {
        arms[name].issues.push({ kind: "invalid_report", detail: `Output recipe/quality varies between ${recipe} trials` });
      }
    }
    arms[name].summaries = summarizeRecords((Array.isArray(report.records) ? report.records : []).filter((record) => record && typeof record === "object"));
    const actual = arms[name].summaries;
    if (JSON.stringify(Object.keys(actual).sort()) !== JSON.stringify(Object.keys(report.summaries || {}).sort())
      || Object.entries(actual).some(([key, summary]) => Object.entries(summary).some(([metric, value]) =>
        JSON.stringify(value) !== JSON.stringify(report.summaries?.[key]?.[metric])))) {
      arms[name].issues.push({ kind: "invalid_report", detail: "Summaries do not match raw records" });
    }
  }
  const evidenceValid = !arms.before.issues.length && !arms.after.issues.length;
  const workloads = {};
  for (const key of keys) {
    const baseline = arms.before.summaries[key];
    const current = arms.after.summaries[key];
    const counts = (summary) => ({ measurements: summary?.n ?? 0, independent_trials: summary?.independent_trials ?? 0, unavailable: summary?.unavailable ?? 0 });
    const result = workloads[key] = { before: counts(baseline), after: counts(current),
      unavailable_reasons: Object.fromEntries([["before", before], ["after", after]].map(([name, report]) => [name,
        [...new Set((Array.isArray(report.records) ? report.records : []).filter((record) => `${record?.recipe}/${record?.workload}` === key && record?.available === false).map((record) => record.reason))]])) };
    if (!baseline || !current) { result.classification = "missing_workload"; continue; }
    if (!evidenceValid || mismatches.length) { result.classification = "noncomparable"; continue; }
    const enoughTrials = baseline.independent_trials >= 10 && current.independent_trials >= 10;
    if (!baseline.n || !current.n || !enoughTrials) {
      result.classification = !baseline.n || !current.n ? "unavailable" : "insufficient_trials";
      result.sufficient_trials = false;
      if (exclusions[key] && (baseline.unavailable || current.unavailable)) {
        result.exclusion_reason = exclusions[key];
        result.excluded_classification = result.classification;
        result.classification = "excluded_unavailable";
      }
      continue;
    }
    const budget = Math.max(thresholds.median_ms, baseline.p50 * thresholds.median_percent / 100);
    const p95Budget = Math.max(thresholds.p95_ms, baseline.p95 * thresholds.p95_percent / 100);
    Object.assign(result, {
      classification: "compared",
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
    });
    if (result.median_regression || result.observed_p95_regression || result.p99_gate?.regression) result.classification = "regression";
    else if (baseline.unavailable || current.unavailable) {
      // Ten surviving observations do not make the missing observations
      // disappear. Retain measured diagnostics, but require an explicit reason
      // to exclude this conditional workload from the gate.
      result.classification = "unavailable";
      if (exclusions[key]) {
        result.exclusion_reason = exclusions[key];
        result.excluded_classification = "unavailable";
        result.classification = "excluded_unavailable";
      }
    }
  }
  const issues = Object.entries(arms).flatMap(([arm, value]) => value.issues.map((issue) => ({ arm, ...issue })));
  const classifications = Object.values(workloads).map((value) => value.classification);
  const classification = issues.some((issue) => issue.kind === "failed_run") ? "failed_run"
    : issues.length ? "invalid_evidence" : mismatches.length ? "noncomparable"
      : classifications.includes("missing_workload") ? "missing_evidence"
        : classifications.includes("regression") ? "regression"
          : classifications.includes("unavailable") ? "unavailable"
            : classifications.includes("insufficient_trials") ? "insufficient_trials"
              : classifications.includes("compared") ? "passed" : "missing_evidence";
  return { comparable: evidenceValid && !mismatches.length && classifications.every((value) => ["compared", "regression", "excluded_unavailable"].includes(value)) && classifications.some((value) => ["compared", "regression"].includes(value)),
    classification, gate_passed: classification === "passed", issues, mismatches, thresholds, exclusions, workloads,
    policy: "Configurable engineering guard bands, not statistical confidence: median increase must exceed both relative and absolute limits; observed p95 uses its own pair. Fewer than 10 independent trials is insufficient for gating; p99 gating is disabled below 1000 trials in either run." };
}

export function comparisonExitCode(comparison) {
  return comparison.gate_passed ? 0 : comparison.classification === "regression" ? 1 : 2;
}
