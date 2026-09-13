"""Behavioral checks for benchmark conclusions, without browser dependencies."""
import pathlib
import shutil
import subprocess
import unittest


class PlaybackBenchmarkSummaryTests(unittest.TestCase):
    def test_unavailable_and_correlated_samples_cannot_support_tail_claims(self):
        if not shutil.which("node"):
            self.skipTest("Node is required for the benchmark report calculations")
        root = pathlib.Path(__file__).resolve().parents[2]
        code = """
import assert from 'node:assert/strict';
import { summarizeRecords, compareReports } from './scripts/playback-benchmark-summary.mjs';
const records = Array.from({length: 120}, (_, n) => ({recipe:'copy', workload:'warm', sample:Math.floor(n / 4), selection_to_frame_ms:100}));
records.push({recipe:'copy', workload:'cancellation', sample:0, available:false, reason:'producer completed'});
const summary = summarizeRecords(records);
assert.equal(summary['copy/warm'].n, 120);
assert.equal(summary['copy/warm'].independent_trials, 30);
assert.equal(summary['copy/warm'].tail_sample_threshold_met, false);
assert.equal(summary['copy/cancellation'].n, 0);
assert.equal(summary['copy/cancellation'].p50, null);
const before = {configuration:{rate:1}, environment:{cpu:'a'}, fixtures:[{recipe:'copy',sha256:'same'}], validations:[{recipe:'copy',output_probe:{streams:[]}}], summaries:summary};
const after = {...before, configuration:{rate:2}};
assert.equal(compareReports(before, after).comparable, false);
assert.deepEqual(compareReports(before, after).workloads, {});
const slower = { ...before, summaries: summarizeRecords(records.map(r => ({...r, selection_to_frame_ms:300}))) };
assert.equal(compareReports(before, slower).workloads['copy/warm'].repeat_recommended, true);
assert.equal(compareReports(before, slower).workloads['copy/warm'].tail_comparison_available, false);
assert.equal(compareReports(before, {...before, fixtures:[{recipe:'copy',sha256:'different'}]}).comparable, false);
const validated = {...before, validations:[{recipe:'copy', requested:{quality:'auto',video_mode:'copy',audio_mode:'copy'}, output_probe:{streams:[{codec_type:'video',codec_name:'h264',width:1280,height:720,pix_fmt:'yuv420p'}]}, quality:{decoded_hash_sha256:'frames'}}]};
const changedOutput = structuredClone(validated);
changedOutput.validations[0].output_probe.streams[0].width = 640;
assert.equal(compareReports(validated, changedOutput).comparable, false);
const changedPreset = structuredClone(validated);
changedPreset.configuration.encoding_preset = 'maximum_speed';
assert.equal(compareReports(validated, changedPreset).comparable, false);
const changedNetwork = structuredClone(validated);
changedNetwork.configuration.network = {latency_ms:100, kbps:2000, scope:'per-viewer CDP aggregate'};
assert.equal(compareReports(validated, changedNetwork).comparable, false);
const changedRecipePreset = structuredClone(validated);
changedRecipePreset.validations[0].requested.encoding_preset = 'fast_start';
assert.equal(compareReports(validated, changedRecipePreset).comparable, false);
const starved = {...validated, records:[{sustained:{progression_within_tolerance:false}}]};
assert.equal(compareReports(starved, validated).comparable, false);
assert.deepEqual(compareReports(starved, validated).workloads, {});
const resetRate = {...validated, records:[{browser:{rate_at_first_frame:{playback_rate:2,preference_rate:1}}}]};
assert.equal(compareReports(validated, resetRate).comparable, false);
const oneHundred = summarizeRecords(Array.from({length:100}, (_, sample) => ({recipe:'copy', workload:'warm', sample, selection_to_frame_ms:100})));
assert.equal(oneHundred['copy/warm'].tail_sample_threshold_met, false);
const oneThousand = summarizeRecords(Array.from({length:1000}, (_, sample) => ({recipe:'copy', workload:'warm', sample, selection_to_frame_ms:100})));
assert.equal(oneThousand['copy/warm'].tail_sample_threshold_met, true);
const lowSamples = {...before, summaries: summarizeRecords(records.slice(0, 4))};
assert.equal(compareReports(lowSamples, lowSamples).workloads['copy/warm'].sufficient_trials, false);
assert.equal(compareReports(lowSamples, lowSamples).workloads['copy/warm'].median_regression, null);
"""
        subprocess.run(["node", "--input-type=module", "-e", code], cwd=root, check=True, timeout=10)


if __name__ == "__main__":
    unittest.main()
