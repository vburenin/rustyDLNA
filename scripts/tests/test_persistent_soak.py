"""Decision regressions for persistent-process evidence; no browsers needed."""
import pathlib
import shutil
import subprocess
import unittest


class PersistentSoakTests(unittest.TestCase):
    def test_tool_deadlines_cancellation_and_process_group_cleanup(self):
        if not shutil.which("node"):
            self.skipTest("Node is required for persistent soak process supervision")
        root = pathlib.Path(__file__).resolve().parents[2]
        code = r"""
import assert from 'node:assert/strict';
import {mkdtemp, readFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {runCommand} from './scripts/persistent-soak-process.mjs';
assert.equal(await runCommand('python3', ['-c', 'import sys; print(len(sys.stdin.read()))']), '0');
await assert.rejects(runCommand('python3', ['-c', 'print("x" * 4096)'], {outputLimit:100}), /output bound/);
await assert.rejects(runCommand('python3', ['-c', 'import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)'], {timeout:200,killGrace:50}), /helper deadline/);
const directory = await mkdtemp(join(tmpdir(), 'persistent-soak-process-test-'));
try {
  const marker = join(directory, 'pids');
  const script = `import os,signal,subprocess,sys,time
child = subprocess.Popen(['sleep', '30'])
def stop(signum, frame):
    child.terminate()
    child.wait()
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
open(sys.argv[1], 'w').write(str(os.getpid()) + ' ' + str(child.pid))
time.sleep(30)
`;
  const abort = new AbortController();
  const result = runCommand('python3', ['-c', script, marker], {signal:abort.signal, timeout:5000});
  const rejected = assert.rejects(result, /test cancellation/);
  let pids;
  for (let n = 0; n < 100; n++) {
    pids = await readFile(marker, 'utf8').catch(() => null);
    if (pids) break;
    await new Promise(done => setTimeout(done, 10));
  }
  assert.ok(pids, 'helper published parent/child identities');
  abort.abort(new Error('test cancellation'));
  await rejected;
  for (const pid of pids.split(' ').map(Number)) {
    assert.throws(() => process.kill(pid, 0), {code:'ESRCH'});
  }
} finally { await rm(directory, {recursive:true,force:true}); }
"""
        subprocess.run(["node", "--input-type=module", "-e", code], cwd=root, check=True, timeout=15)

    def test_warmup_growth_coverage_and_process_identity(self):
        if not shutil.which("node"):
            self.skipTest("Node is required for persistent soak report calculations")
        root = pathlib.Path(__file__).resolve().parents[2]
        code = """
import assert from 'node:assert/strict';
import {evaluateRun, trend, ownedProcesses, sameProcess} from './scripts/persistent-soak-summary.mjs';
const steady = Array.from({length:6}, (_, n) => ({phase:'steady', elapsed_seconds:60+n*10,
  rss:100, fds:10, playbacks:2, seeks:1, cancellations:1, reconnects:1, cache_reuses:1,
  evictions:1, scans:1, artwork:2, queries:4}));
const warmup = { ...steady[0], phase:'warmup', elapsed_seconds:61, rss:5000, fds:500 };
assert.deepEqual(evaluateRun([warmup, ...steady], 60, {rss:20, fds:2}).failures, []);
assert.equal(evaluateRun([warmup, ...steady], 60, {rss:20}).warmup_cycles, 1);
assert.equal(evaluateRun(steady.slice(0, 2), 60, {rss:20}).trends.rss.available, false);
assert.match(evaluateRun(steady.slice(0, 2), 60, {rss:20}).failures.join(), /three/);
const leak = steady.map((r, i) => ({...r, rss:100+i*10, fds:10+i}));
assert.equal(trend(leak, 'rss').growth, 40);
assert.equal(trend(leak, 'rss').slope_per_hour, 3600);
assert.equal(evaluateRun(leak, 60, {rss:20, fds:2}).failures.length, 2);
for (const invalid of [undefined, NaN, Infinity]) {
  const missing = steady.map((r, i) => ({...r, rss:i === 3 ? invalid : r.rss}));
  assert.equal(evaluateRun(missing, 60, {rss:20}).trends.rss.available, false);
  assert.ok(evaluateRun(missing, 60, {rss:20}).failures.some(f => f.includes('trend evidence')));
}
for (const invalid of [undefined, NaN, 70, 65]) {
  const malformed = steady.map((r, i) => ({...r, elapsed_seconds:i === 2 ? invalid : r.elapsed_seconds}));
  assert.ok(evaluateRun(malformed, 60, {rss:20}).failures.length > 0);
}
for (const key of ['playbacks','seeks','cancellations','reconnects','cache_reuses','evictions','scans','artwork','queries']) {
  const missing = steady.map(r => ({...r, [key]:0}));
  assert.ok(evaluateRun(missing, 60, {rss:20}).failures.some(f => f.includes(key)));
}
assert.deepEqual(ownedProcesses([{pid:4,parent:3},{pid:3,parent:2},{pid:8,parent:1},{pid:2,parent:1}], [2]).map(r=>r.pid), [4,3,2]);
assert.equal(sameProcess({pid:2,started:'20'}, {pid:2,started:'10'}), false);
assert.equal(sameProcess({pid:2,started:'20'}, {pid:2,started:'20'}), true);
"""
        subprocess.run(["node", "--input-type=module", "-e", code], cwd=root, check=True, timeout=10)


if __name__ == "__main__":
    unittest.main()
