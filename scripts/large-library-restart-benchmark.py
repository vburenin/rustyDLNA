#!/usr/bin/env python3
"""Measure catalog restoration separately from filesystem reconciliation.

Use a retained disposable large-library-benchmark.sh configuration. The normal
--database-check startup restores the catalog, then checks SQLite and exits;
it does not start listeners, scan, or watch the library. GNU time reports each
child's CPU and peak RSS (not the Python driver's historical high-water mark).
"""

import argparse
import hashlib
import json
import os
import pathlib
import platform
import signal
import statistics
import subprocess
import tempfile
import time


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--config", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--samples", type=int, default=10)
    args = parser.parse_args()
    if not 1 <= args.samples <= 100:
        parser.error("samples must be between 1 and 100")
    samples = []
    with tempfile.TemporaryDirectory(prefix="rusty-dlna-restart-") as directory:
        metrics = pathlib.Path(directory) / "time.json"
        for _ in range(args.samples):
            started = time.perf_counter()
            with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
                child = subprocess.Popen([
                    "/usr/bin/time", "-o", str(metrics), "-f",
                    '{"user_seconds":%U,"system_seconds":%S,"peak_rss_kib":%M}',
                    str(args.binary.resolve()), "--config", str(args.config.resolve()),
                    "--database-check",
                ], stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                    start_new_session=True,
                    env={**os.environ, "RUST_LOG": "warn", "RUSTY_DLNA_HTTP_PORT": "18249", "RUSTY_DLNA_SSDP_PORT": "11949"})
                try:
                    child.wait(timeout=1800)
                finally:
                    if child.poll() is None:
                        os.killpg(child.pid, signal.SIGTERM)
                        try:
                            child.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            os.killpg(child.pid, signal.SIGKILL)
                            child.wait()
                stderr.seek(max(0, stderr.seek(0, os.SEEK_END) - 2000))
                diagnostics = stderr.read(2000).decode(errors="replace")
                stdout.seek(max(0, stdout.seek(0, os.SEEK_END) - 2000))
                completed = b"database OK:" in stdout.read(2000)
                if child.returncode:
                    raise RuntimeError(f"startup failed ({child.returncode}): {diagnostics}")
            sample = json.loads(metrics.read_text())
            sample["wall_ms"] = (time.perf_counter() - started) * 1000
            if not completed:
                raise RuntimeError("database check did not complete")
            samples.append(sample)
    wall = [sample["wall_ms"] for sample in samples]
    report = {
        "schema": 1, "workload": "startup catalog restoration plus SQLite quick_check; no scan or listeners",
        "binary_sha256": digest(args.binary),
        "config_sha256": digest(args.config),
        "host": platform.platform(), "samples": samples,
        "wall_median_ms": statistics.median(wall),
        "wall_sample_sd_ms": statistics.stdev(wall) if len(wall) > 1 else None,
        "limitations": "OS page cache is warm. CPU uses GNU time 10ms precision. Small samples do not establish tail latency.",
    }
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
