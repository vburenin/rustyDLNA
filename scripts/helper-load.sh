#!/bin/sh
# Exercise the daemon-wide helper gate with distinct image and remux cache keys.
# The limits below are deliberately conservative enough for shared CI runners,
# while still detecting unbounded helper processes, descriptors, or cache growth.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
TOOLCHAIN=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$ROOT/rust-toolchain.toml")
HTTP_PORT=${HELPER_LOAD_HTTP_PORT:-18420}
SSDP_PORT=${HELPER_LOAD_SSDP_PORT:-12120}
MAX_RSS_GROWTH_KB=${HELPER_LOAD_MAX_RSS_GROWTH_KB:-786432}
MAX_THREAD_GROWTH=${HELPER_LOAD_MAX_THREAD_GROWTH:-256}
MAX_FD_GROWTH=${HELPER_LOAD_MAX_FD_GROWTH:-192}
MAX_CACHE_BYTES=${HELPER_LOAD_MAX_CACHE_BYTES:-536870912}
MAX_LATENCY_SECONDS=${HELPER_LOAD_MAX_LATENCY_SECONDS:-31}

case "$HTTP_PORT:$SSDP_PORT:$MAX_RSS_GROWTH_KB:$MAX_THREAD_GROWTH:$MAX_FD_GROWTH:$MAX_CACHE_BYTES:$MAX_LATENCY_SECONDS" in
    *[!0-9:]*) echo "helper-load settings must be positive integers" >&2; exit 2 ;;
esac

for command_name in curl ffmpeg python3; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "helper-load requires $command_name" >&2
        exit 2
    }
done

TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-helper-load.XXXXXX")
SERVER_PID=
MONITOR_PID=
cleanup() {
    status=$?
    if [ -n "$MONITOR_PID" ]; then
        kill "$MONITOR_PID" >/dev/null 2>&1 || true
        wait "$MONITOR_PID" 2>/dev/null || true
    fi
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [ "$status" -ne 0 ] && [ -f "$TMP/server.log" ]; then
        tail -n 120 "$TMP/server.log" >&2 || true
    fi
    rm -rf -- "$TMP"
    exit "$status"
}
trap cleanup EXIT INT TERM

mkdir -p "$TMP/media" "$TMP/cache"
ffmpeg -hide_banner -loglevel error -nostdin -y \
    -f lavfi -i 'testsrc2=size=3840x2160:rate=1' -frames:v 1 \
    "$TMP/media/load-image-00.jpg"
index=1
while [ "$index" -le 7 ]; do
    cp "$TMP/media/load-image-00.jpg" "$TMP/media/load-image-0$index.jpg"
    index=$((index + 1))
done
index=0
while [ "$index" -le 7 ]; do
    cp "$ROOT/testdata/library/video/movie.mkv" "$TMP/media/load-video-0$index.mkv"
    index=$((index + 1))
done

{
    printf 'friendly_name = "rustyDLNA helper load"\n'
    printf 'media_dir = ["%s"]\n' "$TMP/media"
    printf 'listen_ip = "127.0.0.1"\n'
    printf 'advertise_ip = "127.0.0.1"\n'
    printf 'cache_dir = "%s/cache"\n' "$TMP"
    printf 'db_dir = "%s/cache"\n' "$TMP"
    printf 'thumbnails = false\n'
    printf 'rescan_secs = 0\n'
    printf 'scan_workers = 1\n'
    printf 'helper_max_jobs = 1\n'
    printf 'helper_queue_capacity = 2\n'
    printf 'helper_queue_timeout_secs = 1\n'
    printf 'derived_image_cache_mb = 256\n'
    printf 'cache_min_free_mb = 0\n'
    printf 'max_connections = 64\n'
    printf '[transcode]\n'
    printf 'enable = true\n'
    printf 'encoder = "libx264"\n'
    printf 'max_jobs = 4\n'
    printf 'cache_max_mb = 256\n'
    printf '[[remap]]\n'
    printf 'name = "helper-load-audio"\n'
    printf 'client = "CrKey"\n'
    printf 'action = "audio-ac3"\n'
    printf 'encoder = "copy"\n'
} >"$TMP/config.toml"

if command -v rustup >/dev/null 2>&1; then
    RUSTC=$(rustup which --toolchain "$TOOLCHAIN" rustc) \
    RUSTDOC=$(rustup which --toolchain "$TOOLCHAIN" rustdoc) \
        rustup run "$TOOLCHAIN" cargo build --locked -p rusty-dlna \
        --manifest-path "$ROOT/Cargo.toml"
else
    cargo build --locked -p rusty-dlna --manifest-path "$ROOT/Cargo.toml"
fi

RUSTY_DLNA_HTTP_PORT=$HTTP_PORT RUSTY_DLNA_SSDP_PORT=$SSDP_PORT \
    RUST_LOG=info "$ROOT/target/debug/rusty-dlna" \
    --config "$TMP/config.toml" >"$TMP/server.log" 2>&1 &
SERVER_PID=$!

tries=0
while :; do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "helper-load daemon exited during startup" >&2
        exit 1
    fi
    detail_count=$(python3 - "$TMP/cache/files.db" <<'PY' 2>/dev/null || true
import sqlite3
import sys

try:
    with sqlite3.connect(sys.argv[1]) as db:
        print(db.execute("SELECT COUNT(*) FROM DETAILS").fetchone()[0])
except (OSError, sqlite3.Error):
    pass
PY
)
    if [ "${detail_count:-0}" -ge 16 ] \
        && curl --fail --silent --max-time 2 "http://127.0.0.1:$HTTP_PORT/health" >/dev/null; then
        break
    fi
    tries=$((tries + 1))
    [ "$tries" -lt 120 ] || { echo "helper-load scan did not become ready" >&2; exit 1; }
    sleep 0.1
done

python3 - "$TMP/cache/files.db" "$TMP/image-ids" "$TMP/video-ids" <<'PY'
import sqlite3
import sys

with sqlite3.connect(sys.argv[1]) as db:
    rows = list(db.execute("SELECT ID, MIME FROM DETAILS ORDER BY ID"))
with open(sys.argv[2], "w", encoding="ascii") as images, open(
    sys.argv[3], "w", encoding="ascii"
) as videos:
    for detail_id, mime in rows:
        if mime.startswith("image/"):
            images.write(f"{detail_id}\n")
        elif mime.startswith("video/"):
            videos.write(f"{detail_id}\n")
PY
test "$(wc -l <"$TMP/image-ids")" -eq 8
test "$(wc -l <"$TMP/video-ids")" -eq 8

process_tree() {
    tree_pid=$1
    [ -d "/proc/$tree_pid" ] || return 0
    printf '%s\n' "$tree_pid"
    children=$(sed -n '1p' "/proc/$tree_pid/task/$tree_pid/children" 2>/dev/null || true)
    for child in $children; do
        process_tree "$child"
    done
}

sample_resources() {
    root_pid=$1
    processes=0
    threads=0
    fds=0
    rss=0
    for sample_pid in $(process_tree "$root_pid"); do
        [ -r "/proc/$sample_pid/status" ] || continue
        processes=$((processes + 1))
        value=$(awk '/^Threads:/ { print $2; exit }' "/proc/$sample_pid/status" 2>/dev/null || true)
        threads=$((threads + ${value:-0}))
        value=$(find "/proc/$sample_pid/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
        fds=$((fds + value))
        value=$(awk '/^VmRSS:/ { print $2; exit }' "/proc/$sample_pid/status" 2>/dev/null || true)
        rss=$((rss + ${value:-0}))
    done
    printf '%s %s %s %s\n' "$processes" "$threads" "$fds" "$rss"
}

monitor_resources() {
    monitored_pid=$1
    metrics_file=$2
    max_processes=0
    max_threads=0
    max_fds=0
    max_rss=0
    while kill -0 "$monitored_pid" 2>/dev/null; do
        sample_line=$(sample_resources "$monitored_pid")
        read -r processes threads fds rss <<EOF
$sample_line
EOF
        [ "$processes" -le "$max_processes" ] || max_processes=$processes
        [ "$threads" -le "$max_threads" ] || max_threads=$threads
        [ "$fds" -le "$max_fds" ] || max_fds=$fds
        [ "$rss" -le "$max_rss" ] || max_rss=$rss
        printf '%s %s %s %s\n' "$max_processes" "$max_threads" "$max_fds" "$max_rss" >"$metrics_file"
        sleep 0.05
    done
}

baseline_line=$(sample_resources "$SERVER_PID")
read -r BASE_PROCESSES BASE_THREADS BASE_FDS BASE_RSS <<EOF
$baseline_line
EOF
monitor_resources "$SERVER_PID" "$TMP/metrics" &
MONITOR_PID=$!

request=0
REQUEST_PIDS=
while IFS= read -r detail_id; do
    request=$((request + 1))
    dimension=$((900 + request * 17))
    (
        result=$(curl --silent --show-error --max-time "$MAX_LATENCY_SECONDS" \
            --output /dev/null --write-out '%{http_code}\t%{time_total}' \
            "http://127.0.0.1:$HTTP_PORT/Resized/$detail_id.jpg?width=$dimension,height=$dimension" \
            || printf '000\t%s' "$MAX_LATENCY_SECONDS")
        printf 'resize\t%s\n' "$result" >"$TMP/result-$request"
    ) &
    REQUEST_PIDS="$REQUEST_PIDS $!"
    request=$((request + 1))
    dimension=$((900 + request * 17))
    (
        result=$(curl --silent --show-error --max-time "$MAX_LATENCY_SECONDS" \
            --output /dev/null --write-out '%{http_code}\t%{time_total}' \
            "http://127.0.0.1:$HTTP_PORT/Resized/$detail_id.jpg?width=$dimension,height=$dimension" \
            || printf '000\t%s' "$MAX_LATENCY_SECONDS")
        printf 'resize\t%s\n' "$result" >"$TMP/result-$request"
    ) &
    REQUEST_PIDS="$REQUEST_PIDS $!"
done <"$TMP/image-ids"
while IFS= read -r detail_id; do
    request=$((request + 1))
    (
        result=$(curl --silent --show-error --max-time "$MAX_LATENCY_SECONDS" \
            --header 'User-Agent: CrKey/1.54' --output /dev/null \
            --write-out '%{http_code}\t%{time_total}' \
            "http://127.0.0.1:$HTTP_PORT/Transcode/$detail_id.mp4" \
            || printf '000\t%s' "$MAX_LATENCY_SECONDS")
        printf 'remux\t%s\n' "$result" >"$TMP/result-$request"
    ) &
    REQUEST_PIDS="$REQUEST_PIDS $!"
done <"$TMP/video-ids"

for request_pid in $REQUEST_PIDS; do
    wait "$request_pid"
done
kill "$MONITOR_PID" 2>/dev/null || true
wait "$MONITOR_PID" 2>/dev/null || true
MONITOR_PID=
cat "$TMP"/result-* >"$TMP/results"

if awk -F '\t' '$2 != 200 && $2 != 503 { bad = 1 } END { exit bad }' "$TMP/results"; then
    :
else
    echo "helper-load received an unexpected HTTP status" >&2
    cat "$TMP/results" >&2
    exit 1
fi
grep -q '^resize[[:space:]]' "$TMP/results"
grep -q '^remux[[:space:]]' "$TMP/results"
grep -q "$(printf '\t')200$(printf '\t')" "$TMP/results"
grep -q "$(printf '\t')503$(printf '\t')" "$TMP/results"

MAX_LATENCY=$(awk -F '\t' 'BEGIN { max = 0 } $3 > max { max = $3 } END { print max }' "$TMP/results")
awk -v actual="$MAX_LATENCY" -v limit="$MAX_LATENCY_SECONDS" 'BEGIN { exit !(actual <= limit) }'

read -r MAX_PROCESSES MAX_THREADS MAX_FDS MAX_RSS <"$TMP/metrics"
[ "$MAX_PROCESSES" -le 2 ] || {
    echo "helper process limit exceeded: $MAX_PROCESSES > 2" >&2
    exit 1
}
[ "$MAX_THREADS" -le "$((BASE_THREADS + MAX_THREAD_GROWTH))" ] || {
    echo "helper thread growth exceeded: $MAX_THREADS > $((BASE_THREADS + MAX_THREAD_GROWTH))" >&2
    exit 1
}
[ "$MAX_FDS" -le "$((BASE_FDS + MAX_FD_GROWTH))" ] || {
    echo "helper FD growth exceeded: $MAX_FDS > $((BASE_FDS + MAX_FD_GROWTH))" >&2
    exit 1
}
[ "$MAX_RSS" -le "$((BASE_RSS + MAX_RSS_GROWTH_KB))" ] || {
    echo "helper RSS growth exceeded: ${MAX_RSS}KiB > $((BASE_RSS + MAX_RSS_GROWTH_KB))KiB" >&2
    exit 1
}
CACHE_BYTES=$(find "$TMP/cache" -type f -printf '%s\n' | awk '{ total += $1 } END { print total + 0 }')
[ "$CACHE_BYTES" -le "$MAX_CACHE_BYTES" ] || {
    echo "helper cache limit exceeded: $CACHE_BYTES > $MAX_CACHE_BYTES bytes" >&2
    exit 1
}

curl --fail --silent --max-time 3 "http://127.0.0.1:$HTTP_PORT/api/status" >"$TMP/status.json"
python3 - "$TMP/status.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as status_file:
    helpers = json.load(status_file)["helpers"]
assert helpers["active"] == 0, helpers
assert helpers["queued"] == 0, helpers
assert helpers["max_active"] == 1, helpers
assert helpers["queue_capacity"] == 2, helpers
assert helpers["rejected_total"] + helpers["timed_out_total"] > 0, helpers
PY

kill -TERM "$SERVER_PID"
wait "$SERVER_PID"
SERVER_PID=
grep -q 'SSDP byebye sent' "$TMP/server.log"
printf 'helper load OK: requests=%s processes=%s threads=%s fds=%s rss_kb=%s cache_bytes=%s max_latency=%ss\n' \
    "$request" "$MAX_PROCESSES" "$MAX_THREADS" "$MAX_FDS" "$MAX_RSS" "$CACHE_BYTES" "$MAX_LATENCY"
