import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { summarizeRecords } from "../playback-benchmark-summary.mjs";
import { expectedAndroidOutput, expectedAudioTrack, assertExpectedRequest, assertExpectedVideo } from "../playback-benchmark-policy.mjs";
import { waitForCompletedArtifact } from "../playback-benchmark-artifact.mjs";

test("publication between the cache scan and producer completion is observed before declaring failure", async () => {
  const cache = mkdtempSync(join(tmpdir(), "rustydlna-publication-test-"));
  const output = join(cache, "7-web-control.mp4");
  let observations = 0;
  try {
    const result = await waitForCompletedArtifact(cache, { id: 7 }, async () => {
      // Synchronize the real filesystem publication after this iteration's
      // directory read, before the external status reports completion.
      if (++observations === 7) {
        writeFileSync(output, "completed payload");
        writeFileSync(`${output}.src`, "validated stamp");
      }
      return { transcode: { active: observations < 7 ? 1 : 0 } };
    }, async () => []);
    assert.equal(result, output);
  } finally { rmSync(cache, { recursive: true, force: true }); }
});

test("a stopped producer still fails when output is absent, unstamped or still partial", async () => {
  for (const state of ["absent", "unstamped", "partial"]) {
    const cache = mkdtempSync(join(tmpdir(), "rustydlna-publication-test-"));
    const output = join(cache, "7-web-control.mp4");
    try {
      if (state !== "absent") writeFileSync(output, "payload");
      if (state === "partial") {
        writeFileSync(`${output}.src`, "stamp");
        writeFileSync(`${output}.part`, "unfinished");
      }
      await assert.rejects(waitForCompletedArtifact(cache, { id: 7 },
        async () => ({ transcode: { active: 0 } }), async () => []), /without a completed output and validation stamp/);
    } finally { rmSync(cache, { recursive: true, force: true }); }
  }
});

test("external audio oracle follows English/default dispositions and audio ordinals independently", () => {
  const streams = [
    { codec_type: "video", codec_name: "h264" },
    { codec_type: "audio", codec_name: "flac", disposition: { default: 1 }, tags: { language: "jpn" } },
    { codec_type: "audio", codec_name: "aac", tags: { language: "eng" } },
  ];
  assert.equal(expectedAudioTrack(streams).ordinal, 1);
  assert.equal(expectedAudioTrack(streams).stream.codec_name, "aac");
  streams[2].tags.language = "fr";
  assert.equal(expectedAudioTrack(streams).ordinal, 0);
  streams[1].disposition.default = 0;
  assert.equal(expectedAudioTrack(streams).ordinal, 1);
});

test("independent Android oracle accepts the documented recipes and rejects altered requests/output", () => {
  const profiles = [
    { id: "auto", max_width: 3840, max_height: 2160 },
    { id: "full_hd", max_width: 1920, max_height: 1080 },
    { id: "data_saver", max_width: 1280, max_height: 720, automatic_fallback: true },
    { id: "low_360", max_width: 640, max_height: 360 },
  ];
  const sourceVideo = { width: 640, height: 360, r_frame_rate: "24/1" };
  for (const [recipe, quality, video_mode, audio_mode] of [
    ["copy", "auto", "copy", "copy"], ["audio", "auto", "copy", "transcode"],
    ["video", "data_saver", "transcode", "copy"], ["both", "data_saver", "transcode", "transcode"],
  ]) {
    const expected = expectedAndroidOutput({ recipe, preference: "auto", sourceVideo, profiles });
    assert.equal(expected.quality, quality);
    const request = { quality, video_mode, audio_mode, video_output: "h264_sdr" };
    assert.doesNotThrow(() => assertExpectedRequest(expected, request, "balanced"));
    assert.throws(() => assertExpectedRequest(expected, { ...request, quality: "low_360" }, "balanced"), /Expected Android quality/);
    assert.throws(() => assertExpectedRequest(expected, { ...request, audio_mode: audio_mode === "copy" ? "transcode" : "copy" }, "balanced"), /Expected Android audio_mode/);
    if (video_mode === "transcode") {
      const video = { ...sourceVideo, codec_name: "h264", pix_fmt: "yuv420p", profile: "Constrained Baseline" };
      assert.doesNotThrow(() => assertExpectedVideo(expected, video, sourceVideo));
      assert.throws(() => assertExpectedVideo(expected, { ...video, width: 320 }, sourceVideo), /independent quality envelope/);
      for (const width of [undefined, null, "bad", 0, Infinity]) {
        assert.throws(() => assertExpectedVideo(expected, { ...video, width }, sourceVideo), /independent quality envelope/);
      }
      assert.throws(() => assertExpectedRequest(expected, { ...request, quality: "auto" }, "balanced"), /Expected Android quality/);
    }
  }
  assert.equal(expectedAndroidOutput({ recipe: "video", preference: "full_hd", sourceVideo, profiles }).quality, "low_360");
  assert.equal(expectedAndroidOutput({ recipe: "video", preference: "full_hd", sourceVideo: { width: 1920, height: 1080 }, profiles }).quality, "full_hd");
});

// Controlled ten-trial reports with exact 100-ms observations. This fixture is
// intentionally independent of the requiredWorkloads/validateReport decisions.
function report(latency = 100, samples = 10, concurrency = 1) {
  const result = {
    schema: 2, finished: "2026-09-13T00:00:00Z",
    configuration: { samples, concurrency, duration: 40, fps: 24, rate: 1, size: "1280x720", recipes: ["copy"],
      encoder: "libx264", build_profile: "release", tier: "cpu-sdr", sustain_seconds: 2, quality: "auto", encoding_preset: "balanced",
      delivery: "MSE Android", network: { latency_ms: 0, kbps: 0, scope: "per-viewer CDP aggregate" } },
    environment: { cpu: "controlled CPU", logical_cpus: 4, cpu_max: "unlimited", memory_max: "unlimited", memory_bytes: 1024 ** 3,
      cgroup_ancestor_limits: [], process_affinity_cpus: "0-3", browser: "151", ffmpeg: "8.0", ffprobe: "8.0", rust: "1.98.1", node: "22.19.0",
      kernel: "fixed", filesystem: "tmpfs", cache_conditions: "warm OS cache",
      runtime: { platform: { system: "Linux", release: "fixed", machine: "x86_64", python: "3.12", os_release: "controlled Linux" },
        tools: { libav_development: { status: "ok", output: "libav versions" },
          server_linked_libraries: { status: "ok", output: "libavformat.so.61 => /lib/libavformat.so.61 (0xabc)\nlibc.so.6 => /lib/libc.so.6 (0x123)" },
          packages: { status: "ok", output: "libavformat61\t8.0\tam64\tffmpeg\t8.0\tii" } },
        files: { ffmpeg: { sha256: "a".repeat(64) }, ffprobe: { sha256: "b".repeat(64) } } } },
    fixtures: [{ recipe: "copy", sha256: "c".repeat(64) }], validations: [], records: [],
  };
  for (let sample = 0; sample < samples; sample++) {
    result.validations.push({ id: sample, recipe: "copy", sample,
      requested: { quality: "auto", video_mode: "copy", audio_mode: "copy" },
      output_probe: { streams: [{ codec_type: "video", codec_name: "h264", width: 1280, height: 720, pix_fmt: "yuv420p", r_frame_rate: "24/1" },
        { codec_type: "audio", codec_name: "aac", channels: 2, sample_rate: "48000" }] }, output_bytes: 10000, stamp_bytes: 100,
      quality: { copied_video_frame_hashes_match: true, decoded_frames_compared: 48, decoded_hash_sha256: "d".repeat(64) } });
    for (const workload of ["original", "cold", "warm", "active-attachment", "near", "near-paused", "restart", "cancellation"]) {
      const startup = ["original", "cold", "warm"].includes(workload);
      for (let viewer = 0; viewer < (startup ? concurrency : 1); viewer++) {
        const metric = workload === "cancellation" ? "cancellation_ms"
          : ["near", "near-paused", "restart"].includes(workload) ? "seek_to_frame_ms" : "selection_to_frame_ms";
        const rate = { playback_rate: 1, preference_rate: 1 };
        result.records.push({ recipe: "copy", workload, sample, ...(startup ? { viewer } : {}), [metric]: latency,
          browser: { frames: 1, rate_at_first_frame: rate, rate_at_completion: rate },
          ...(startup ? { sustained: { progression_within_tolerance: true, presented_frames: 48, actual_rate_start: rate, actual_rate_end: rate } } : {}),
          ...(["cold", "warm"].includes(workload) ? { validation_id: sample } : {}),
          ...(workload === "cancellation" ? { helpers_reaped: true } : {}),
          ...(workload === "active-attachment" ? { active_helpers_at_dispatch: 1 } : {}) });
      }
    }
  }
  return resummarize(result);
}
function resummarize(value) { value.summaries = summarizeRecords(value.records); return value; }
function run(before, after, options = [], exclusions) {
  const evidence = process.env.RUSTY_DLNA_BENCHMARK_TEST_EVIDENCE;
  if (evidence) mkdirSync(evidence, { recursive: true });
  const directory = mkdtempSync(join(evidence || tmpdir(), "rustydlna-comparison-test-"));
  try {
    for (const [name, value] of Object.entries({ before, after, exclusions })) {
      if (value !== undefined) writeFileSync(join(directory, `${name}.json`), JSON.stringify(value));
    }
    const argv = ["scripts/compare-playback-benchmarks.mjs",
      `--before=${join(directory, "before.json")}`, `--after=${join(directory, "after.json")}`,
      ...(exclusions ? [`--exclusions=${join(directory, "exclusions.json")}`] : []), ...options];
    const child = spawnSync(process.execPath, argv, { encoding: "utf8", timeout: 10000 });
    if (evidence) writeFileSync(join(directory, "cli-result.json"), JSON.stringify({ argv, status: child.status, stdout: child.stdout, stderr: child.stderr }, null, 2));
    assert.ifError(child.error);
    return { code: child.status, result: child.stdout ? JSON.parse(child.stdout) : null, error: child.stderr };
  } finally { if (!evidence) rmSync(directory, { recursive: true, force: true }); }
}

test("actual CLI gates the complete matrix; thresholds detect a controlled slowdown", () => {
  const before = report();
  const passed = run(before, report(), ["--gate"]);
  assert.equal(passed.code, 0);
  assert.equal(passed.result.classification, "passed");
  assert.equal(Object.keys(passed.result.workloads).length, 8);
  const options = ["--median-percent=25", "--median-ms=50", "--p95-percent=30", "--p95-ms=100"];
  const slower = run(before, report(300), ["--gate", ...options]);
  assert.equal(slower.code, 1);
  assert.equal(slower.result.classification, "regression");
  assert.equal(slower.result.comparable, true);
  assert.equal(slower.result.workloads["copy/warm"].delta_ms, 200);
  assert.equal(slower.result.workloads["copy/warm"].median_regression_budget_ms, 50);
  assert.equal(slower.result.workloads["copy/warm"].observed_p95_regression, true);
  assert.equal(slower.result.workloads["copy/warm"].p99_gate, null);
  assert.equal(run(before, report(300), options).code, 0);
  assert.equal(run(before, report(120), ["--gate", "--median-percent=0", "--median-ms=0"]).code, 1);
});

test("empty reports, missing workloads/trials/viewers and stale summaries fail in either arm", () => {
  const changes = [
    (r) => { r.records = []; r.summaries = {}; },
    (r) => { r.records = r.records.filter((v) => v.workload !== "warm"); resummarize(r); },
    (r) => { r.records.pop(); resummarize(r); },
    (r) => { r.records = r.records.filter((v) => v.viewer !== 1); resummarize(r); },
    (r) => { r.summaries = {}; },
    (r) => { r.summaries["copy/warm"].p50 = 1; },
    (r) => { r.validations.pop(); },
    (r) => { r.records.push(r.records[0]); resummarize(r); },
    (r) => { r.fixtures = []; },
    (r) => { delete r.finished; },
    (r) => { delete r.records[0].browser.rate_at_first_frame; },
    (r) => { delete r.records[0].sustained.actual_rate_end; },
    (r) => { delete r.records[0].browser.frames; },
  ];
  for (const change of changes) for (const arm of ["before", "after"]) {
    const pair = { before: report(100, 10, 2), after: report(100, 10, 2) };
    change(pair[arm]);
    const rejected = run(pair.before, pair.after, ["--gate"]);
    assert.equal(rejected.code, 2, `${arm}: ${change}`);
    assert.equal(rejected.result.comparable, false);
    assert.ok(rejected.result.issues.some((issue) => issue.arm === arm));
  }
  assert.equal(run({}, {}, ["--gate"]).code, 2);
});

test("required key union and environment/output differences are noncomparable", () => {
  const changes = [
    (r) => { r.configuration.recipes = ["audio"]; },
    (r) => { r.configuration.network.kbps = 2000; },
    (r) => { r.configuration.encoding_preset = "maximum_speed"; },
    (r) => { r.environment.kernel = "other kernel"; },
    (r) => { r.environment.node = "different runtime"; },
    (r) => { r.environment.runtime.platform.os_release = "different OS"; },
    (r) => { r.environment.runtime.files.ffmpeg.sha256 = "different"; },
    (r) => { r.environment.runtime.tools.server_linked_libraries.output = "libavformat.so.62 => /custom/libavformat.so.62 (0xabc)"; },
    (r) => { r.environment.runtime.tools.packages.output = "libavformat61\t8.1\tam64\tffmpeg\t8.1\tii"; },
    (r) => { r.fixtures[0].sha256 = "f".repeat(64); },
    (r) => { r.validations[0].requested.quality = "low_360"; },
    (r) => { r.validations[0].requested.audio_mode = "transcode"; },
    (r) => { r.validations[0].output_probe.streams[0].width = 640; },
  ];
  for (const change of changes) {
    const after = report(); change(after);
    const rejected = run(report(), after, ["--gate"]);
    assert.equal(rejected.code, 2, String(change));
    assert.ok(rejected.result.mismatches.length > 0);
    assert.equal(rejected.result.comparable, false);
  }
  const after = report(); after.configuration.recipes = ["audio"];
  const rejected = run(report(), after);
  assert.ok(Object.hasOwn(rejected.result.workloads, "audio/cold"));
  assert.ok(Object.hasOwn(rejected.result.workloads, "copy/cold"));
});

test("malformed configurations and failed runtime collection cannot match themselves into a pass", () => {
  for (const mutate of [
    (r) => { r.configuration.duration = -1; },
    (r) => { r.configuration.fps = "nonsense"; },
    (r) => { r.configuration.network = {}; },
    (r) => { r.environment.cpu_max = "unavailable"; },
    (r) => { r.environment.runtime.tools.libav_development = { status: "failed", output: "Library not found" }; },
    (r) => { r.environment.runtime.tools.server_linked_libraries.status = "timeout"; },
  ]) {
    const invalid = report(); mutate(invalid);
    assert.equal(run(invalid, invalid, ["--gate"]).code, 2, String(mutate));
  }
  const before = report(); const after = report();
  after.environment.runtime.tools.server_linked_libraries.output = after.environment.runtime.tools.server_linked_libraries.output.replaceAll("0xabc", "0xdef");
  assert.equal(run(before, after, ["--gate"]).code, 0, "ASLR is not library identity");
  const cpu = report(); cpu.configuration.tier = "external-sdr"; cpu.environment.gpu_inventory = "unavailable: nvidia-smi not installed";
  assert.equal(run(cpu, cpu, ["--gate"]).code, 0, "CPU media tiers do not require NVIDIA");
  cpu.configuration.encoder = "h264_nvenc";
  assert.equal(run(cpu, cpu, ["--gate"]).code, 2, "Selected NVIDIA output requires device identity");
});

test("matching incomplete quality evidence fails the actual CLI in both arms", () => {
  for (const mutate of [
    (v) => { delete v.requested.quality; },
    (v) => { delete v.output_probe.streams[0].width; },
    (v) => { v.output_probe.streams[0].height = 0; },
    (v) => { delete v.output_probe.streams[0].pix_fmt; },
    (v) => { v.output_probe.streams[0].r_frame_rate = "0/0"; },
    (v) => { delete v.output_probe.streams[1].channels; },
    (v) => { v.output_probe.streams[1].sample_rate = "unknown"; },
    (v) => { v.quality.decoded_hash_sha256 = "x"; },
    (v) => { v.quality.decoded_frames_compared = 0.5; },
    (v) => { v.requested.video_mode = "transcode"; v.quality = { decoded_frames_sampled: 48, encoded_output_decoded_hash_sha256: "x" }; },
  ]) {
    const invalid = report();
    invalid.validations.forEach(mutate);
    const rejected = run(invalid, invalid, ["--gate"]);
    assert.equal(rejected.code, 2, String(mutate));
    for (const arm of ["before", "after"]) {
      assert.ok(rejected.result.issues.some((issue) => issue.arm === arm && issue.kind === "missing_evidence"));
    }
    assert.equal(run(invalid, invalid).code, 0, "report-only retains malformed evidence diagnostics");
  }
});

test("mixed quality distributions are rejected even when both arms contain the same set of qualities", () => {
  const before = report(); const after = report();
  before.validations[0].requested.quality = "low_360";
  after.validations.slice(1).forEach((validation) => { validation.requested.quality = "low_360"; });
  const rejected = run(before, after, ["--gate"]);
  assert.equal(rejected.code, 2);
  assert.ok(rejected.result.issues.some((issue) => issue.detail.includes("varies between copy trials")));
});

test("failed runs, sustained playback and helper proof cannot be excluded", () => {
  for (const mutate of [
    (r) => { r.failure = "controlled producer failure"; },
    (r) => { r.records[0].sustained.progression_within_tolerance = false; },
    (r) => { r.records[0].browser = { rate_at_first_frame: { playback_rate: 2, preference_rate: 1 } }; },
  ]) {
    const after = report(); mutate(after);
    const rejected = run(report(), after, ["--gate"]);
    assert.equal(rejected.code, 2);
    assert.equal(rejected.result.classification, "failed_run");
  }
  const after = report(); delete after.records.find((v) => v.workload === "cancellation").helpers_reaped;
  assert.equal(run(report(), after, ["--gate"], { "copy/cancellation": "too fast" }).code, 2);
});

test("unavailable attachment/cancellation retain reasons and need explicit exclusions", () => {
  const after = report();
  for (const record of after.records.filter((v) => ["active-attachment", "cancellation"].includes(v.workload))) {
    record.available = false; record.reason = "Producer completed before observation";
    delete record.selection_to_frame_ms; delete record.cancellation_ms;
  }
  resummarize(after);
  const rejected = run(after, after, ["--gate"]);
  assert.equal(rejected.code, 2);
  assert.equal(rejected.result.classification, "unavailable");
  assert.equal(rejected.result.workloads["copy/active-attachment"].before.unavailable, 10);
  assert.deepEqual(rejected.result.workloads["copy/active-attachment"].unavailable_reasons.after, ["Producer completed before observation"]);
  const exclusions = { "copy/active-attachment": "Completed producers cannot attach", "copy/cancellation": "Completed producers cannot cancel" };
  const excluded = run(after, after, ["--gate"], exclusions);
  assert.equal(excluded.code, 0);
  assert.equal(excluded.result.workloads["copy/cancellation"].classification, "excluded_unavailable");
  // Exclusions don't suppress actual measured regressions or absent records.
  assert.equal(run(report(), report(300), ["--gate"], exclusions).code, 1);
  after.records.pop(); resummarize(after);
  assert.equal(run(after, after, ["--gate"], exclusions).code, 2);
  assert.equal(run(report(), report(), ["--gate"], { "copy/warm": "ignore required work" }).code, 2);
});

test("unavailable samples still block with ten surviving trials, and exclusions cannot hide measured regressions", () => {
  for (const arm of ["before", "after"]) {
    const pair = { before: report(100, 20), after: report(100, 20) };
    for (const record of pair[arm].records.filter((r) => r.workload === "active-attachment" && r.sample >= 10)) {
      record.available = false;
      record.reason = "Producer completed before attachment";
      delete record.selection_to_frame_ms;
    }
    resummarize(pair[arm]);
    const rejected = run(pair.before, pair.after, ["--gate"]);
    assert.equal(rejected.code, 2, arm);
    assert.equal(rejected.result.classification, "unavailable");
    assert.equal(rejected.result.workloads["copy/active-attachment"][arm].independent_trials, 10);
    assert.equal(rejected.result.workloads["copy/active-attachment"][arm].unavailable, 10);
    assert.equal(run(pair.before, pair.after).code, 0, "report-only retains incomplete observations");
    const exclusions = { "copy/active-attachment": "Producer completed before half of attachments" };
    assert.equal(run(pair.before, pair.after, ["--gate"], exclusions).code, 0);
    for (const record of pair.after.records.filter((r) => r.workload === "active-attachment" && r.available !== false)) {
      record.selection_to_frame_ms = 300;
    }
    resummarize(pair.after);
    assert.equal(run(pair.before, pair.after, ["--gate"], exclusions).code, 1);
  }
});

test("correlated viewers and fewer than ten trials do not pass; tail minimum remains 1000", () => {
  const rejected = run(report(100, 9, 4), report(100, 9, 4), ["--gate"]);
  assert.equal(rejected.code, 2);
  assert.equal(rejected.result.classification, "insufficient_trials");
  assert.equal(rejected.result.workloads["copy/warm"].before.measurements, 36);
  assert.equal(rejected.result.workloads["copy/warm"].before.independent_trials, 9);
  const records = Array.from({ length: 1000 }, (_, sample) => ({ recipe: "copy", workload: "warm", sample, selection_to_frame_ms: sample }));
  assert.equal(summarizeRecords(records.slice(0, 999))["copy/warm"].tail_sample_threshold_met, false);
  const summary = summarizeRecords(records)["copy/warm"];
  assert.equal(summary.tail_sample_threshold_met, true);
  assert.equal(summary.p50, 499.5);
  assert.equal(summary.p99, 989.01);
});
