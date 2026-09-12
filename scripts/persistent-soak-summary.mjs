// Pure report decisions: warm-up, workload failures, and idle resource trends.
export function trend(points, key) {
  const values = points;
  if (values.some((point, index) => !Number.isFinite(point[key]) || !Number.isFinite(point.elapsed_seconds)
    || (index > 0 && point.elapsed_seconds <= values[index - 1].elapsed_seconds))) {
    return { samples: values.length, available: false, reason: "Missing/nonfinite resource or nonincreasing sample time" };
  }
  if (values.length < 3) return { samples: values.length, available: false };
  const median = (items) => {
    const sorted = [...items].sort((a, b) => a - b);
    return (sorted[Math.floor((sorted.length - 1) / 2)] + sorted[Math.floor(sorted.length / 2)]) / 2;
  };
  const width = Math.max(1, Math.floor(values.length / 3));
  const meanX = values.reduce((sum, point) => sum + point.elapsed_seconds, 0) / values.length;
  const meanY = values.reduce((sum, point) => sum + point[key], 0) / values.length;
  const denominator = values.reduce((sum, point) => sum + (point.elapsed_seconds - meanX) ** 2, 0);
  const numerator = values.reduce((sum, point) => sum + (point.elapsed_seconds - meanX) * (point[key] - meanY), 0);
  const first = median(values.slice(0, width).map((point) => point[key]));
  const last = median(values.slice(-width).map((point) => point[key]));
  return { samples: values.length, available: denominator > 0,
    span_seconds: values.at(-1).elapsed_seconds - values[0].elapsed_seconds,
    first_third_median: first, last_third_median: last, growth: last - first,
    slope_per_hour: denominator > 0 ? numerator / denominator * 3600 : null,
    peak: Math.max(...values.map((point) => point[key])) };
}

export function evaluateRun(cycles, warmupSeconds, growthLimits) {
  const steady = cycles.filter((cycle) => cycle.phase === "steady" && cycle.elapsed_seconds >= warmupSeconds);
  const trends = Object.fromEntries(Object.keys(growthLimits).map((key) => [key, trend(steady, key)]));
  const failures = [];
  if (cycles.some((cycle) => cycle.phase === "steady" && (!Number.isFinite(cycle.elapsed_seconds) || cycle.elapsed_seconds < warmupSeconds))) {
    failures.push("Invalid steady-state elapsed time");
  }
  if (steady.length < 3) failures.push(`Only ${steady.length} steady-state cycles; at least three are required`);
  for (const [key, limit] of Object.entries(growthLimits)) {
    if (!trends[key].available) failures.push(`No usable ${key} steady-state trend evidence`);
    if (trends[key].available && trends[key].growth > limit) failures.push(`${key} idle growth ${trends[key].growth} exceeds ${limit}`);
  }
  for (const field of ["playbacks", "seeks", "cancellations", "reconnects", "cache_reuses", "evictions", "scans", "artwork", "queries"]) {
    if (!steady.some((cycle) => cycle[field] > 0)) failures.push(`No verified steady-state ${field}`);
  }
  return { steady_cycles: steady.length, warmup_cycles: cycles.length - steady.length, trends, failures };
}

export function ownedProcesses(snapshot, roots) {
  const owned = new Set(roots);
  let changed = true;
  while (changed) {
    changed = false;
    for (const process of snapshot) if (owned.has(process.parent) && !owned.has(process.pid)) {
      owned.add(process.pid); changed = true;
    }
  }
  return snapshot.filter((process) => owned.has(process.pid));
}

export function sameProcess(current, previous) {
  return current.pid === previous.pid && current.started === previous.started;
}
