#!/bin/sh
# Repeatedly exercises scan/watch, Browse/Search, GENA, media/remux, shutdown,
# and restart through the mandatory socket E2E suite. Default duration is 24h.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
SOAK_SECONDS=${SOAK_SECONDS:-86400}
SOAK_MAX_PROCESSES=${SOAK_MAX_PROCESSES:-64}
SOAK_MAX_RSS_KB=${SOAK_MAX_RSS_KB:-1048576}
SOAK_MAX_THREADS=${SOAK_MAX_THREADS:-512}
SOAK_MAX_FDS=${SOAK_MAX_FDS:-4096}
SOAK_MAX_DB_BYTES=${SOAK_MAX_DB_BYTES:-536870912}
SOAK_MAX_CACHE_BYTES=${SOAK_MAX_CACHE_BYTES:-2147483648}
SOAK_PROGRESS_EVERY=${SOAK_PROGRESS_EVERY:-100}
HTTP_PORT=${SOAK_HTTP_PORT:-18300}
SSDP_PORT=${SOAK_SSDP_PORT:-12000}
REPORT=${SOAK_REPORT:-"$ROOT/soak-report.tsv"}
TOOLCHAIN=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$ROOT/rust-toolchain.toml")

case "$SOAK_SECONDS:$SOAK_MAX_PROCESSES:$SOAK_MAX_RSS_KB:$SOAK_MAX_THREADS:$SOAK_MAX_FDS:$SOAK_MAX_DB_BYTES:$SOAK_MAX_CACHE_BYTES:$SOAK_PROGRESS_EVERY:$HTTP_PORT:$SSDP_PORT" in
    *[!0-9:]*) echo "soak settings must be positive integers" >&2; exit 2 ;;
esac
for limit in "$SOAK_SECONDS" "$SOAK_MAX_PROCESSES" "$SOAK_MAX_RSS_KB" \
    "$SOAK_MAX_THREADS" "$SOAK_MAX_FDS" "$SOAK_MAX_DB_BYTES" \
    "$SOAK_MAX_CACHE_BYTES" "$SOAK_PROGRESS_EVERY" "$HTTP_PORT" "$SSDP_PORT"; do
    [ "$limit" -gt 0 ] || exit 2
done
if [ "$HTTP_PORT" -gt 65535 ] || [ "$SSDP_PORT" -gt 65535 ]; then
    echo "soak ports must be in the range 1..65535" >&2
    exit 2
fi

TMP_BASE=${TMPDIR:-/tmp}
TMP=$(mktemp -d "$TMP_BASE/rusty-dlna-soak.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM
CYCLE=0
MAX_RSS=0
MAX_THREADS=0
MAX_FDS=0
MAX_PROCESSES=0
MAX_DB_BYTES=0
MAX_CACHE_BYTES=0
BASE_TREES=$(find "$TMP_BASE" -maxdepth 1 -type d -name 'rusty-dlna-e2e-*' 2>/dev/null | wc -l)
cd "$ROOT"

# Bind a persisted report to the exact tracked and untracked source content
# under test. Ignored build outputs and the report itself are intentionally
# excluded from this fingerprint.
source_fingerprint() {
    {
        git rev-parse HEAD
        git diff --binary --no-ext-diff HEAD --
        git ls-files --others --exclude-standard | LC_ALL=C sort | while IFS= read -r source_path; do
            sha256sum -- "$source_path"
        done
    } | sha256sum | awk '{ print $1 }'
}
SOURCE_FINGERPRINT=$(source_fingerprint)
GIT_HEAD=$(git rev-parse HEAD)
STARTED_UTC=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
printf '# started_utc\t%s\n# git_head\t%s\n# source_fingerprint_sha256\t%s\n# toolchain\t%s\n' \
    "$STARTED_UTC" "$GIT_HEAD" "$SOURCE_FINGERPRINT" "$TOOLCHAIN" >"$REPORT"
printf '# limits\tseconds=%s processes=%s threads=%s fds=%s rss_kb=%s db_bytes=%s cache_bytes=%s\n' \
    "$SOAK_SECONDS" "$SOAK_MAX_PROCESSES" "$SOAK_MAX_THREADS" "$SOAK_MAX_FDS" \
    "$SOAK_MAX_RSS_KB" "$SOAK_MAX_DB_BYTES" "$SOAK_MAX_CACHE_BYTES" >>"$REPORT"
printf 'cycle\telapsed_seconds\tmax_processes\tmax_threads\tmax_fds\tmax_rss_kb\tmax_db_bytes\tmax_cache_bytes\n' >>"$REPORT"

run_cargo() {
    if command -v rustup >/dev/null 2>&1; then
        RUSTC=$(rustup which --toolchain "$TOOLCHAIN" rustc) \
        RUSTDOC=$(rustup which --toolchain "$TOOLCHAIN" rustdoc) \
            rustup run "$TOOLCHAIN" cargo "$@"
    else
        cargo "$@"
    fi
}

run_cargo test --locked -p rusty-dlna --test listen_e2e --no-run
START=$(date +%s)
DEADLINE=$((START + SOAK_SECONDS))

# Print the root process and every currently live descendant. Linux exposes
# this relationship without requiring ps parsing or elevated permissions.
process_tree() {
    tree_pid=$1
    [ -d "/proc/$tree_pid" ] || return 0
    printf '%s\n' "$tree_pid"
    children=$(sed -n '1p' "/proc/$tree_pid/task/$tree_pid/children" 2>/dev/null || true)
    for child in $children; do
        process_tree "$child"
    done
}

tree_storage_bytes() {
    storage_kind=$1
    case "$storage_kind" in
        db) storage_pattern='database/*' ;;
        cache) storage_pattern='cache/*' ;;
        *) return 2 ;;
    esac
    find "$TMP_BASE" -maxdepth 5 -type f \
        -path "$TMP_BASE/rusty-dlna-e2e-*"'/'"$storage_pattern" \
        -printf '%s\n' 2>/dev/null | awk '{ total += $1 } END { print total + 0 }'
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
    db_bytes=$(tree_storage_bytes db)
    cache_bytes=$(tree_storage_bytes cache)
    printf '%s %s %s %s %s %s\n' \
        "$processes" "$threads" "$fds" "$rss" "$db_bytes" "$cache_bytes"
}

monitor_resources() {
    monitored_pid=$1
    metrics_file=$2
    cycle_processes=0
    cycle_threads=0
    cycle_fds=0
    cycle_rss=0
    cycle_db=0
    cycle_cache=0
    while kill -0 "$monitored_pid" 2>/dev/null; do
        sample_line=$(sample_resources "$monitored_pid")
        read -r sampled_processes sampled_threads sampled_fds sampled_rss sampled_db sampled_cache <<EOF
$sample_line
EOF
        [ "$sampled_processes" -le "$cycle_processes" ] || cycle_processes=$sampled_processes
        [ "$sampled_threads" -le "$cycle_threads" ] || cycle_threads=$sampled_threads
        [ "$sampled_fds" -le "$cycle_fds" ] || cycle_fds=$sampled_fds
        [ "$sampled_rss" -le "$cycle_rss" ] || cycle_rss=$sampled_rss
        [ "$sampled_db" -le "$cycle_db" ] || cycle_db=$sampled_db
        [ "$sampled_cache" -le "$cycle_cache" ] || cycle_cache=$sampled_cache
        printf '%s %s %s %s %s %s\n' "$cycle_processes" "$cycle_threads" \
            "$cycle_fds" "$cycle_rss" "$cycle_db" "$cycle_cache" >"$metrics_file"
        sleep 1
    done
}

while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    CYCLE=$((CYCLE + 1))
    CYCLE_LOG="$TMP/cycle.log"
    if command -v rustup >/dev/null 2>&1; then
        RUSTC_BIN=$(rustup which --toolchain "$TOOLCHAIN" rustc)
        RUSTDOC_BIN=$(rustup which --toolchain "$TOOLCHAIN" rustdoc)
        env RUSTY_DLNA_REQUIRE_E2E=1 \
            RUSTY_DLNA_HTTP_PORT="$HTTP_PORT" \
            RUSTY_DLNA_SSDP_PORT="$SSDP_PORT" \
            RUSTC="$RUSTC_BIN" RUSTDOC="$RUSTDOC_BIN" \
            rustup run "$TOOLCHAIN" cargo test --locked -p rusty-dlna \
                --test listen_e2e -- --test-threads=1 >"$CYCLE_LOG" 2>&1 &
    else
        env RUSTY_DLNA_REQUIRE_E2E=1 \
            RUSTY_DLNA_HTTP_PORT="$HTTP_PORT" \
            RUSTY_DLNA_SSDP_PORT="$SSDP_PORT" \
            cargo test --locked -p rusty-dlna --test listen_e2e -- \
                --test-threads=1 >"$CYCLE_LOG" 2>&1 &
    fi
    CYCLE_PID=$!
    METRICS="$TMP/metrics-$CYCLE"
    monitor_resources "$CYCLE_PID" "$METRICS" &
    MONITOR_PID=$!
    if wait "$CYCLE_PID"; then
        CYCLE_STATUS=0
    else
        CYCLE_STATUS=$?
    fi
    kill "$MONITOR_PID" 2>/dev/null || true
    wait "$MONITOR_PID" 2>/dev/null || true
    if [ "$CYCLE_STATUS" -ne 0 ]; then
        cat "$CYCLE_LOG" >&2
        exit "$CYCLE_STATUS"
    fi
    [ -s "$METRICS" ] || { echo "soak resource sampler produced no metrics" >&2; exit 1; }
    metrics_line=$(sed -n '$p' "$METRICS")
    read -r CYCLE_PROCESSES CYCLE_THREADS CYCLE_FDS CYCLE_RSS CYCLE_DB CYCLE_CACHE <<EOF
$metrics_line
EOF
    [ "$CYCLE_PROCESSES" -le "$SOAK_MAX_PROCESSES" ] || {
        echo "process limit exceeded: $CYCLE_PROCESSES > $SOAK_MAX_PROCESSES" >&2; exit 1;
    }
    [ "$CYCLE_THREADS" -le "$SOAK_MAX_THREADS" ] || {
        echo "thread limit exceeded: $CYCLE_THREADS > $SOAK_MAX_THREADS" >&2; exit 1;
    }
    [ "$CYCLE_FDS" -le "$SOAK_MAX_FDS" ] || {
        echo "file-descriptor limit exceeded: $CYCLE_FDS > $SOAK_MAX_FDS" >&2; exit 1;
    }
    [ "$CYCLE_RSS" -le "$SOAK_MAX_RSS_KB" ] || {
        echo "RSS limit exceeded: ${CYCLE_RSS}KiB > ${SOAK_MAX_RSS_KB}KiB" >&2; exit 1;
    }
    [ "$CYCLE_DB" -le "$SOAK_MAX_DB_BYTES" ] || {
        echo "database limit exceeded: $CYCLE_DB > $SOAK_MAX_DB_BYTES bytes" >&2; exit 1;
    }
    [ "$CYCLE_CACHE" -le "$SOAK_MAX_CACHE_BYTES" ] || {
        echo "cache limit exceeded: $CYCLE_CACHE > $SOAK_MAX_CACHE_BYTES bytes" >&2; exit 1;
    }
    [ "$CYCLE_PROCESSES" -le "$MAX_PROCESSES" ] || MAX_PROCESSES=$CYCLE_PROCESSES
    [ "$CYCLE_THREADS" -le "$MAX_THREADS" ] || MAX_THREADS=$CYCLE_THREADS
    [ "$CYCLE_FDS" -le "$MAX_FDS" ] || MAX_FDS=$CYCLE_FDS
    [ "$CYCLE_RSS" -le "$MAX_RSS" ] || MAX_RSS=$CYCLE_RSS
    [ "$CYCLE_DB" -le "$MAX_DB_BYTES" ] || MAX_DB_BYTES=$CYCLE_DB
    [ "$CYCLE_CACHE" -le "$MAX_CACHE_BYTES" ] || MAX_CACHE_BYTES=$CYCLE_CACHE
    NOW=$(date +%s)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$CYCLE" "$((NOW - START))" \
        "$CYCLE_PROCESSES" "$CYCLE_THREADS" "$CYCLE_FDS" "$CYCLE_RSS" \
        "$CYCLE_DB" "$CYCLE_CACHE" >>"$REPORT"

    CURRENT_TREES=$(find "$TMP_BASE" -maxdepth 1 -type d -name 'rusty-dlna-e2e-*' 2>/dev/null | wc -l)
    [ "$CURRENT_TREES" -le "$BASE_TREES" ] || {
        echo "E2E temporary tree leaked after cycle $CYCLE" >&2
        exit 1
    }
    CURRENT_SOURCE_FINGERPRINT=$(source_fingerprint)
    [ "$CURRENT_SOURCE_FINGERPRINT" = "$SOURCE_FINGERPRINT" ] || {
        echo "source tree changed during soak cycle $CYCLE" >&2
        exit 1
    }
    if [ "$((CYCLE % SOAK_PROGRESS_EVERY))" -eq 0 ]; then
        echo "soak progress: cycles=$CYCLE elapsed_seconds=$((NOW - START)) rss_kb=$MAX_RSS"
    fi
done

FINISHED_UTC=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
printf '# result\tpass\n# finished_utc\t%s\n# completed_cycles\t%s\n# max_processes\t%s\n# max_threads\t%s\n# max_fds\t%s\n# max_rss_kb\t%s\n# max_db_bytes\t%s\n# max_cache_bytes\t%s\n# duration_seconds\t%s\n' \
    "$FINISHED_UTC" "$CYCLE" "$MAX_PROCESSES" "$MAX_THREADS" "$MAX_FDS" "$MAX_RSS" \
    "$MAX_DB_BYTES" "$MAX_CACHE_BYTES" "$(($(date +%s) - START))" >>"$REPORT"
echo "soak OK: cycles=$CYCLE processes=$MAX_PROCESSES threads=$MAX_THREADS fds=$MAX_FDS rss_kb=$MAX_RSS db_bytes=$MAX_DB_BYTES cache_bytes=$MAX_CACHE_BYTES report=$REPORT"
