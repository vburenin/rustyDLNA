#!/usr/bin/env python3
"""Release production-request/socket benchmark; generated files, loopback only.

Build with cargo test --release --locked -p rusty-dlna --lib --no-run, then
pass the test executable as --arm name=/absolute/path. Copy executables before
rebuilding another arm. This measures transport bytes, not decoded playback or
WAN capacity. Cold means verified eviction from this file's Linux page cache;
device/controller caches are neither flushed nor claimed cold.
"""

import argparse
import concurrent.futures
import ctypes
import hashlib
import http.client
import json
import mmap
import os
from pathlib import Path
import platform
import random
import selectors
import signal
import statistics
import subprocess
import threading
import time


CAP = 8 * 1024 * 1024
TEST = "tests::original_delivery::original_delivery_benchmark_server"


def file_digest(path):
    with path.open("rb") as source:
        digest = hashlib.sha256()
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
        return digest.hexdigest()


def resident_bytes(path):
    """Inspect residency without touching mapped file pages (Linux mincore)."""
    size = path.stat().st_size
    with path.open("rb") as source, mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_COPY) as mapping:
        anchor = ctypes.c_char.from_buffer(mapping)
        pages = (size + mmap.PAGESIZE - 1) // mmap.PAGESIZE
        vector = (ctypes.c_ubyte * pages)()
        libc = ctypes.CDLL(None, use_errno=True)
        libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
        libc.mincore.restype = ctypes.c_int
        result = libc.mincore(ctypes.addressof(anchor), size, vector)
        del anchor
        if result:
            raise OSError(ctypes.get_errno(), "mincore failed")
        return min(size, sum(value & 1 for value in vector) * mmap.PAGESIZE)


def cache_state(path, cold):
    with path.open("rb") as source:
        if cold:
            os.posix_fadvise(source.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
        else:
            while source.read(1024 * 1024):
                pass
    resident = resident_bytes(path)
    if cold and resident:
        raise RuntimeError(f"cold tier unavailable: {resident} file bytes remain resident")
    if not cold and resident != path.stat().st_size:
        raise RuntimeError("warm tier unavailable: file is not fully resident")
    return resident


def proc_snapshot(pid):
    base = Path(f"/proc/{pid}")
    stat = (base / "stat").read_text().rsplit(") ", 1)[1].split()
    io = dict(line.split(": ", 1) for line in (base / "io").read_text().splitlines())
    # Linux's process CPU clock includes every server thread without the 10 ms
    # quantization of /proc/stat. Resolve it through libc, not a guessed clock ID.
    libc = ctypes.CDLL(None, use_errno=True)
    libc.clock_getcpuclockid.argtypes = [ctypes.c_int, ctypes.POINTER(ctypes.c_int)]
    libc.clock_getcpuclockid.restype = ctypes.c_int
    clock_id = ctypes.c_int()
    result = libc.clock_getcpuclockid(pid, ctypes.byref(clock_id))
    if result:
        raise OSError(result, "server process CPU clock unavailable")
    return {
        "cpu_seconds": time.clock_gettime_ns(clock_id.value) / 1e9,
        "rss_bytes": int(stat[21]) * mmap.PAGESIZE,
        "fds": len(list((base / "fd").iterdir())),
        "read_bytes": int(io["read_bytes"]),
        "read_calls": int(io["syscr"]),
        "write_calls": int(io["syscw"]),
    }


def fetch(port, length, expected, rate, barrier):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    try:
        connection.connect()
        barrier.wait(timeout=15)
        started = time.monotonic()
        connection.request("GET", "/MediaItems/9100010.mkv", headers={"Range": f"bytes=0-{length - 1}", "Connection": "keep-alive"})
        response = connection.getresponse()
        if response.status != 206 or response.getheader("Connection").lower() != "close":
            raise RuntimeError(f"unexpected response: {response.status} {response.getheaders()}")
        if int(response.getheader("Content-Length")) != length:
            raise RuntimeError("incorrect Content-Length")
        first = None
        received = 0
        digest = hashlib.sha256()
        while data := response.read(64 * 1024):
            if first is None:
                first = time.monotonic() - started
            digest.update(data)
            received += len(data)
            if rate:
                time.sleep(max(0, started + received / rate - time.monotonic()))
        if received != length or digest.hexdigest() != expected:
            raise RuntimeError(f"independent source digest/length mismatch: {received}/{length}")
        finished = time.monotonic()
        return {"started_monotonic": started, "finished_monotonic": finished, "seconds": finished - started, "first_body_seconds": first, "bytes": received, "sha256": digest.hexdigest()}
    finally:
        connection.close()


def ready(process, log, timeout=30):
    deadline = time.monotonic() + timeout
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        pending = b""
        while time.monotonic() < deadline:
            for key, _ in selector.select(timeout=0.2):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    raise RuntimeError(f"benchmark server exited: {process.poll()}")
                log.write(chunk)
                log.flush()
                pending += chunk
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    if line.startswith(b"DIRECT_BENCH_READY "):
                        return int(line.split()[1])
    raise TimeoutError("benchmark server startup")


def trial(executable, source, length, expected, readers, cold, rate, log_path):
    env = {**os.environ, "RUSTY_DLNA_BENCH_SOURCE": str(source)}
    with log_path.open("xb") as log:
        record = None
        process = subprocess.Popen([str(executable), "--exact", TEST, "--ignored", "--nocapture"], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            port = ready(process, log)
            # Startup may inspect source metadata. Cool only after readiness.
            resident = cache_state(source, cold)
            before = proc_snapshot(process.pid)
            samples = []
            latencies = []
            barrier = threading.Barrier(readers + 1)
            started = time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=readers) as pool:
                futures = [pool.submit(fetch, port, length, expected, rate, barrier) for _ in range(readers)]
                barrier.wait(timeout=15)
                while not all(future.done() for future in futures):
                    samples.append(proc_snapshot(process.pid))
                    ping = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    ping_started = time.monotonic()
                    try:
                        ping.request("GET", "/rootDesc.xml", headers={"Connection": "close"})
                        response = ping.getresponse()
                        if response.status != 200 or b"<root" not in response.read():
                            raise RuntimeError("other connection failed")
                    finally:
                        ping.close()
                    latencies.append(time.monotonic() - ping_started)
                    time.sleep(0.01)
                results = [future.result() for future in futures]
            monitor_elapsed = time.monotonic() - started
            elapsed = max(result["finished_monotonic"] for result in results) - min(result["started_monotonic"] for result in results)
            after = proc_snapshot(process.pid)
            total = sum(result["bytes"] for result in results)
            record = {
                "resident_bytes_before": resident,
                "seconds": elapsed, "monitor_seconds": monitor_elapsed, "bytes": total,
                "mib_per_second": total / elapsed / 1024**2,
                "cpu_seconds": after["cpu_seconds"] - before["cpu_seconds"],
                "cpu_seconds_per_gib": (after["cpu_seconds"] - before["cpu_seconds"]) / (total / 1024**3),
                "read_bytes": after["read_bytes"] - before["read_bytes"],
                "read_calls": after["read_calls"] - before["read_calls"],
                "write_calls": after["write_calls"] - before["write_calls"],
                "rss_peak_sampled_bytes": max(s["rss_bytes"] for s in [before, after, *samples]),
                "fds_peak_sampled": max(s["fds"] for s in [before, after, *samples]),
                "fds_before": before["fds"], "fds_after": after["fds"],
                "listener_seconds": latencies, "readers": results,
            }
            return record
        finally:
            shutdown_started = time.monotonic()
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
            try:
                tail, _ = process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                tail, _ = process.communicate(timeout=5)
                log.write(tail)
                raise RuntimeError("benchmark server did not stop within 10 seconds")
            log.write(tail)
            if process.returncode != 0:
                raise RuntimeError(f"benchmark server failed ({process.returncode}); see {log_path}")
            if record is not None:
                record["test_process_shutdown_seconds"] = time.monotonic() - shutdown_started


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arm", action="append", required=True, help="label=release-test-executable")
    parser.add_argument("--output", type=Path, required=True, help="new owned evidence directory")
    parser.add_argument("--trials", type=int, default=10)
    parser.add_argument("--mib", type=int, default=128)
    parser.add_argument("--readers", default="1,4,16")
    parser.add_argument("--states", default="warm,cold,slow")
    parser.add_argument("--sizes", default="below,above,large")
    parser.add_argument("--slow-mib", type=float, default=8)
    args = parser.parse_args()
    if args.trials < 1 or not 16 <= args.mib <= 1024 or not 0 < args.slow_mib <= 1024:
        parser.error("positive trials, 16..1024 MiB and a bounded positive slow rate required")
    arms = dict(value.split("=", 1) for value in args.arm)
    if len(arms) != len(args.arm):
        parser.error("duplicate arm label")
    readers = [int(value) for value in args.readers.split(",")]
    states = args.states.split(",")
    sizes = args.sizes.split(",")
    if not readers or any(value not in [1, 4, 16] for value in readers) or not set(states) <= {"warm", "cold", "slow"} or not set(sizes) <= {"below", "above", "large"}:
        parser.error("invalid workload")
    arms = {label: Path(executable).resolve(strict=True) for label, executable in arms.items()}
    args.output.mkdir(parents=True, exist_ok=False)
    source = args.output.resolve() / "generated-original.mkv"
    random_source = random.Random(10)
    with source.open("xb") as output:
        for _ in range(args.mib):
            output.write(random_source.randbytes(1024 * 1024))
        output.flush()
        os.fsync(output.fileno())
    lengths = {"below": CAP - 1, "above": CAP + 1, "large": source.stat().st_size}
    digests = {}
    for name, length in lengths.items():
        with source.open("rb") as opened:
            digest = hashlib.sha256()
            left = length
            while left:
                data = opened.read(min(left, 1024 * 1024))
                digest.update(data)
                left -= len(data)
            digests[name] = digest.hexdigest()
    manifest = {
        "schema_version": 1, "command": list(os.sys.argv), "runtime": platform.uname()._asdict(),
        "python": platform.python_version(), "cpu": Path("/proc/cpuinfo").read_text().split("model name", 1)[-1].splitlines()[0],
        "arms": {label: {"executable": str(path), "sha256": file_digest(path)} for label, path in arms.items()},
        "source": {"path": str(source), "bytes": source.stat().st_size, "sha256": digests["large"], "generator": "Python Random(10).randbytes, allocated bytes; transport fixture, not decodable media"},
        "acceptance": "at least 10 independent trials per arm; >=10% median CPU/GiB improvement, <=5% median throughput regression; byte, deadline, cancellation and responsiveness regressions veto promotion",
        "limits": ["loopback; no WAN claim", "cold page cache verified with mincore, underlying device caches uncontrolled", "nanosecond process CPU clock includes listener probes; source hashing runs in client", "RSS/fds sampled; maxima are lower bounds", "at most one source file, concurrent readers share page-cache fills"],
    }
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    workloads = [(sample, count, state, size) for sample in range(args.trials) for count in readers for state in states for size in sizes]
    records = []
    with (args.output / "raw.jsonl").open("x") as raw:
        for sample, count, state, size in workloads:
            ordered = list(arms.items())
            random.Random(sample * 101 + count * 3 + states.index(state) + sizes.index(size)).shuffle(ordered)
            for label, executable in ordered:
                index = len(records)
                try:
                    result = trial(executable, source, lengths[size], digests[size], count, state == "cold", args.slow_mib * 1024**2 if state == "slow" else 0, args.output / f"server-{index}.log")
                except BaseException as error:
                    raw.write(json.dumps({"arm": label, "trial": sample, "reader_count": count, "state": state, "size": size, "status": "failed", "error": str(error)}) + "\n")
                    raw.flush()
                    raise
                record = {"arm": label, "trial": sample, "reader_count": count, "state": state, "size": size, **result}
                raw.write(json.dumps(record) + "\n")
                raw.flush()
                records.append(record)
                print(f"{index + 1}/{len(workloads) * len(arms)} {label} {count} {state} {size}: {result['mib_per_second']:.1f} MiB/s {result['cpu_seconds_per_gib']:.3f} CPU s/GiB", flush=True)
    summary = []
    for label in arms:
        for count in readers:
            for state in states:
                for size in sizes:
                    matching = [record for record in records if (record["arm"], record["reader_count"], record["state"], record["size"]) == (label, count, state, size)]
                    row = {"arm": label, "readers": count, "state": state, "size": size, "samples": len(matching)}
                    for metric in ["mib_per_second", "cpu_seconds_per_gib", "read_calls", "write_calls", "read_bytes", "rss_peak_sampled_bytes", "fds_peak_sampled"]:
                        values = [record[metric] for record in matching]
                        row[metric] = {"median": statistics.median(values), "min": min(values), "max": max(values), "stdev": statistics.stdev(values) if len(values) > 1 else None}
                    summary.append(row)
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
