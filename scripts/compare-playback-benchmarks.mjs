#!/usr/bin/env node
// Offline comparison uses the same validation and gate as a generated run.
import { readFile, writeFile } from "node:fs/promises";
import { compareReports, comparisonExitCode } from "./playback-benchmark-summary.mjs";

try {
  const args = {};
  const accepted = new Set(["before", "after", "output", "exclusions", "median-percent", "median-ms", "p95-percent", "p95-ms"]);
  for (const argument of process.argv.slice(2)) {
    if (["--gate", "--help"].includes(argument)) { args[argument.slice(2)] = true; continue; }
    const match = /^--([^=]+)=(.+)$/.exec(argument);
    if (!match || !accepted.has(match[1]) || Object.hasOwn(args, match[1])) throw new Error(`Invalid comparison option: ${argument}`);
    args[match[1]] = match[2];
  }
  if (args.help) {
    console.log("node scripts/compare-playback-benchmarks.mjs --before=/tmp/before.json --after=/tmp/after.json [--output=/tmp/comparison.json] [--gate] [--exclusions=/tmp/exclusions.json] [--median-percent=25 --median-ms=50 --p95-percent=30 --p95-ms=100]\nReport-only returns 0 after a comparison, including rejected evidence. --gate returns 1 for regression, 2 for incomplete/invalid/noncomparable evidence. Invalid CLI/JSON always returns 2. Exclusions map conditional recipe/workload keys to explicit reasons; missing trials and failures cannot be excluded.");
  } else {
    if (!args.before || !args.after) throw new Error("Both --before and --after are required; see --help");
    const json = async (path) => JSON.parse(await readFile(path, "utf8"));
    const thresholds = Object.fromEntries(["median-percent", "median-ms", "p95-percent", "p95-ms"]
      .filter((key) => args[key] !== undefined).map((key) => [key.replace("-", "_"), Number(args[key])]));
    const comparison = compareReports(await json(args.before), await json(args.after), thresholds,
      args.exclusions ? await json(args.exclusions) : {});
    const output = `${JSON.stringify(comparison, null, 2)}\n`;
    if (args.output) await writeFile(args.output, output);
    process.stdout.write(output);
    if (args.gate) process.exitCode = comparisonExitCode(comparison);
  }
} catch (error) {
  console.error(`Benchmark comparison failed: ${error.message}`);
  process.exitCode = 2;
}
