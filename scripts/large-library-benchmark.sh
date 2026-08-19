#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

FILES=${RUSTY_DLNA_BENCH_FILES:-50000}
ALIASES=${RUSTY_DLNA_BENCH_ALIASES:-5000}
REQUESTS=${RUSTY_DLNA_BENCH_REQUESTS:-200}
SCAN_WORKERS=${RUSTY_DLNA_BENCH_SCAN_WORKERS:-16}
HTTP_PORT=${RUSTY_DLNA_BENCH_HTTP_PORT:-18240}
SSDP_PORT=${RUSTY_DLNA_BENCH_SSDP_PORT:-11940}
TIMEOUT_SECS=${RUSTY_DLNA_BENCH_TIMEOUT_SECS:-7200}
KEEP_WORKDIR=${RUSTY_DLNA_BENCH_KEEP_WORKDIR:-0}
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUTPUT=${RUSTY_DLNA_BENCH_OUTPUT:-$ROOT/benchmark-results/large-library-$STAMP.json}
TOOLCHAIN=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml)

case "$FILES:$ALIASES:$REQUESTS:$SCAN_WORKERS" in
    *[!0-9:]*|0:*|*:0:*|*:*:0:*|*:*:*:0)
        echo "benchmark counts and worker count must be positive integers" >&2
        exit 2
        ;;
esac
if ((ALIASES > FILES)); then
    echo "RUSTY_DLNA_BENCH_ALIASES cannot exceed RUSTY_DLNA_BENCH_FILES" >&2
    exit 2
fi
for command in curl python3 rustup; do
    command -v "$command" >/dev/null || {
        echo "required command is missing: $command" >&2
        exit 2
    }
done
[[ -r /proc/self/status ]] || {
    echo "the large-library benchmark requires Linux /proc metrics" >&2
    exit 2
}

RUN_DIR=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-large-library.XXXXXX")
LIBRARY=$RUN_DIR/library
CACHE=$RUN_DIR/cache
DATABASE=$RUN_DIR/database
CONFIG=$RUN_DIR/benchmark.toml
STATUS_JSON=$RUN_DIR/status.json
LATENCY_JSON=$RUN_DIR/latency.json
RECONCILE_JSON=$RUN_DIR/reconcile.json
RECONCILE_WARMUP_JSON=$RUN_DIR/reconcile-warmup.json
UPDATE_JSON=$RUN_DIR/update.json
DAEMON_LOG=$RUN_DIR/daemon.log
DAEMON_PID=

cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    if [[ "$KEEP_WORKDIR" != 1 ]]; then
        rm -rf -- "$RUN_DIR"
    else
        echo "benchmark work directory retained: $RUN_DIR" >&2
    fi
}
trap cleanup EXIT INT TERM

run_cargo() {
    RUSTC=$(rustup which --toolchain "$TOOLCHAIN" rustc) \
    RUSTDOC=$(rustup which --toolchain "$TOOLCHAIN" rustdoc) \
        rustup run "$TOOLCHAIN" cargo "$@"
}

echo "building release benchmark binaries" >&2
run_cargo build --release --locked -p rusty-dlna
run_cargo build --release --locked -p rusty-dlna-scan \
    --example generate_large_library --example benchmark_reconcile

mkdir -p "$LIBRARY" "$CACHE" "$DATABASE" "$(dirname "$OUTPUT")"
echo "generating $FILES physical files plus $ALIASES hard-link and symlink aliases" >&2
target/release/examples/generate_large_library "$LIBRARY" "$FILES" "$ALIASES"

python3 - "$CONFIG" "$LIBRARY" "$CACHE" "$DATABASE" "$SCAN_WORKERS" <<'PY'
import pathlib
import sys

config, library, cache, database, workers = sys.argv[1:]
pathlib.Path(config).write_text(
    f'''friendly_name = "rustyDLNA 50k benchmark"
media_dir = ["V,{library}"]
cache_dir = "{cache}"
db_dir = "{database}"
listen_ip = "127.0.0.1"
advertise_ip = "127.0.0.1"
thumbnails = false
subtitles = false
scan_workers = {workers}
helper_max_jobs = {workers}
helper_queue_capacity = 4096
rescan_secs = 0
''',
    encoding="utf-8",
)
PY

START_NS=$(python3 -c 'import time; print(time.time_ns())')
RUSTY_DLNA_HTTP_PORT=$HTTP_PORT RUSTY_DLNA_SSDP_PORT=$SSDP_PORT \
RUST_LOG=rusty_dlna=info \
    target/release/rusty-dlna --config "$CONFIG" >"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!

echo "waiting for cold scan publication" >&2
DEADLINE=$((SECONDS + TIMEOUT_SECS))
while ((SECONDS < DEADLINE)); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        tail -100 "$DAEMON_LOG" >&2
        echo "daemon exited before cold scan completed" >&2
        exit 1
    fi
    if curl --silent --show-error --fail --max-time 30 \
        "http://127.0.0.1:$HTTP_PORT/api/status" >"$STATUS_JSON.tmp" 2>/dev/null; then
        mv "$STATUS_JSON.tmp" "$STATUS_JSON"
        if python3 - "$STATUS_JSON" "$FILES" "$ALIASES" <<'PY'
import json
import sys

status = json.load(open(sys.argv[1], encoding="utf-8"))
files = int(sys.argv[2])
aliases = int(sys.argv[3]) * 2
catalog = status.get("catalog") or {}
scanner = status.get("scanner") or {}
ready = (
    scanner.get("phase") == "watching"
    and scanner.get("last_success_unix") is not None
    and scanner.get("last_error") is None
    and catalog.get("physical_inodes") == files
    and catalog.get("path_aliases", 0) >= aliases
)
raise SystemExit(0 if ready else 1)
PY
        then
            break
        fi
    fi
    sleep 0.5
done
if ((SECONDS >= DEADLINE)); then
    tail -100 "$DAEMON_LOG" >&2
    echo "cold scan exceeded ${TIMEOUT_SECS}s" >&2
    exit 1
fi
END_NS=$(python3 -c 'import time; print(time.time_ns())')
COLD_WALL_MS=$(((END_NS - START_NS) / 1000000))

read -r CPU_TICKS < <(awk '{print $14 + $15}' "/proc/$DAEMON_PID/stat")
CLK_TCK=$(getconf CLK_TCK)
RSS_KB=$(awk '/^VmRSS:/ {print $2}' "/proc/$DAEMON_PID/status")
HWM_KB=$(awk '/^VmHWM:/ {print $2}' "/proc/$DAEMON_PID/status")
OPEN_FDS=$(find "/proc/$DAEMON_PID/fd" -mindepth 1 -maxdepth 1 -printf . 2>/dev/null | wc -c)
SQLITE_BYTES=$(find "$DATABASE" -maxdepth 1 -type f -name 'files.db*' -printf '%s\n' | awk '{sum += $1} END {print sum + 0}')

echo "measuring unchanged reconciliation" >&2
target/release/examples/benchmark_reconcile "$LIBRARY" "$DATABASE/files.db" >"$RECONCILE_WARMUP_JSON"
target/release/examples/benchmark_reconcile "$LIBRARY" "$DATABASE/files.db" >"$RECONCILE_JSON"

echo "measuring Browse/Search latency ($REQUESTS requests each)" >&2
python3 scripts/large-library-http-benchmark.py --port "$HTTP_PORT" latency \
    --requests "$REQUESTS" >"$LATENCY_JSON"

echo "measuring targeted inotify update-to-Browse latency" >&2
python3 scripts/large-library-http-benchmark.py --port "$HTTP_PORT" wait-for-total \
    --expected "$((FILES + 1))" --timeout 300 \
    --create "$LIBRARY/physical/update-latency.mkv" >"$UPDATE_JSON"

GIT_REVISION=$(git rev-parse HEAD)
if git diff --quiet && git diff --cached --quiet; then
    GIT_DIRTY=false
else
    GIT_DIRTY=true
fi
KERNEL=$(uname -srmo)
CPU_MODEL=$(awk -F: '/model name/ {sub(/^[[:space:]]+/, "", $2); print $2; exit}' /proc/cpuinfo)
RUSTC_VERSION=$(rustup run "$TOOLCHAIN" rustc --version)

BENCH_FILES=$FILES BENCH_ALIASES=$ALIASES BENCH_REQUESTS=$REQUESTS \
BENCH_WORKERS=$SCAN_WORKERS BENCH_COLD_MS=$COLD_WALL_MS \
BENCH_CPU_TICKS=$CPU_TICKS BENCH_CLK_TCK=$CLK_TCK BENCH_RSS_KB=$RSS_KB \
BENCH_HWM_KB=$HWM_KB BENCH_OPEN_FDS=$OPEN_FDS BENCH_SQLITE_BYTES=$SQLITE_BYTES \
BENCH_STATUS=$STATUS_JSON BENCH_LATENCY=$LATENCY_JSON \
BENCH_RECONCILE=$RECONCILE_JSON BENCH_RECONCILE_WARMUP=$RECONCILE_WARMUP_JSON \
BENCH_UPDATE=$UPDATE_JSON \
BENCH_REVISION=$GIT_REVISION BENCH_DIRTY=$GIT_DIRTY BENCH_KERNEL=$KERNEL \
BENCH_CPU_MODEL=$CPU_MODEL BENCH_RUSTC=$RUSTC_VERSION \
BENCH_RECORDED_UTC=$STAMP \
python3 - <<'PY' >"$OUTPUT"
import json
import os

def load(name):
    with open(os.environ[name], encoding="utf-8") as source:
        return json.load(source)

status = load("BENCH_STATUS")
report = {
    "schema": 1,
    "recorded_utc": os.environ["BENCH_RECORDED_UTC"],
    "fixture": {
        "physical_files": int(os.environ["BENCH_FILES"]),
        "hardlink_aliases": int(os.environ["BENCH_ALIASES"]),
        "symlink_aliases": int(os.environ["BENCH_ALIASES"]),
        "media_template": "testdata/library/video/movie.mkv",
    },
    "build": {
        "git_revision": os.environ["BENCH_REVISION"],
        "git_dirty": os.environ["BENCH_DIRTY"] == "true",
        "rustc": os.environ["BENCH_RUSTC"],
        "profile": "release",
    },
    "host": {
        "kernel": os.environ["BENCH_KERNEL"],
        "cpu_model": os.environ["BENCH_CPU_MODEL"],
        "logical_cpus": os.cpu_count(),
        "memory_bytes": os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES"),
    },
    "settings": {
        "scan_workers": int(os.environ["BENCH_WORKERS"]),
        "requests_per_action": int(os.environ["BENCH_REQUESTS"]),
        "thumbnails": False,
    },
    "cold_scan": {
        "wall_ms": int(os.environ["BENCH_COLD_MS"]),
        "process_cpu_seconds": round(
            int(os.environ["BENCH_CPU_TICKS"]) / int(os.environ["BENCH_CLK_TCK"]), 3
        ),
        "rss_bytes": int(os.environ["BENCH_RSS_KB"]) * 1024,
        "peak_rss_bytes": int(os.environ["BENCH_HWM_KB"]) * 1024,
        "open_fds": int(os.environ["BENCH_OPEN_FDS"]),
        "sqlite_bytes": int(os.environ["BENCH_SQLITE_BYTES"]),
    },
    "reconcile": load("BENCH_RECONCILE"),
    "reconcile_warmup": load("BENCH_RECONCILE_WARMUP"),
    "request_latency": load("BENCH_LATENCY"),
    "update": load("BENCH_UPDATE"),
    "catalog": status["catalog"],
}
print(json.dumps(report, indent=2, sort_keys=True))
PY

python3 -m json.tool "$OUTPUT" >/dev/null
echo "large-library benchmark complete: $OUTPUT" >&2
cat "$OUTPUT"
