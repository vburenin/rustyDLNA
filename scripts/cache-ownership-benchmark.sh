#!/bin/sh
# Measure first-migration and normal-restart ownership cost on a large cache.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
IMAGE=${RUSTY_DLNA_BENCH_IMAGE:-rusty-dlna:local}
FILE_COUNT=${RUSTY_DLNA_BENCH_FILES:-50000}
MAX_WARM_MS=${RUSTY_DLNA_BENCH_MAX_WARM_MS:-2000}
VOLUME="rusty-dlna-ownership-bench-$$"

case "$FILE_COUNT:$MAX_WARM_MS" in
    *[!0-9:]*|0:*|*:0)
        echo "RUSTY_DLNA_BENCH_FILES and RUSTY_DLNA_BENCH_MAX_WARM_MS must be positive integers" >&2
        exit 2
        ;;
esac
command -v docker >/dev/null
docker image inspect "$IMAGE" >/dev/null
docker volume create "$VOLUME" >/dev/null
cleanup() {
    docker volume rm --force "$VOLUME" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

docker run --rm --user 0:0 --entrypoint /bin/sh \
    --mount "type=volume,src=$VOLUME,dst=/var/cache/rusty-dlna" \
    "$IMAGE" -euc '
        count=$1
        root=/var/cache/rusty-dlna
        mkdir -p "$root/derived-images" "$root/art"
        i=0
        while [ "$i" -lt "$count" ]; do
            case $((i % 3)) in
                0) file="$root/derived-images/$i.jpg" ;;
                1) file="$root/art/thumb-$i.jpg" ;;
                2) file="$root/$i.mp4" ;;
            esac
            : >"$file"
            i=$((i + 1))
        done
    ' sh "$FILE_COUNT"

run_init() {
    docker run --rm --user 0:0 \
        --entrypoint /usr/local/libexec/rusty-dlna-cache-volume-init \
        --mount "type=volume,src=$VOLUME,dst=/var/cache/rusty-dlna" \
        "$IMAGE"
}

start=$(date +%s%N)
run_init
cold_ms=$((($(date +%s%N) - start) / 1000000))
start=$(date +%s%N)
warm_output=$(run_init)
warm_ms=$((($(date +%s%N) - start) / 1000000))

printf '%s\n' "$warm_output" | grep -Fq 'recursive scan skipped'
test "$warm_ms" -le "$MAX_WARM_MS" || {
    echo "warm ownership initialization took ${warm_ms}ms (limit ${MAX_WARM_MS}ms)" >&2
    exit 1
}
docker run --rm --user 0:0 --entrypoint /bin/sh \
    --mount "type=volume,src=$VOLUME,dst=/var/cache/rusty-dlna" \
    "$IMAGE" -euc ': >/var/cache/rusty-dlna/externally-copied.mp4'
docker run --rm --user 0:0 --env RUSTY_DLNA_REPAIR_OWNERSHIP=1 \
    --entrypoint /usr/local/libexec/rusty-dlna-cache-volume-init \
    --mount "type=volume,src=$VOLUME,dst=/var/cache/rusty-dlna" \
    "$IMAGE" >/dev/null
owner=$(docker run --rm --user 0:0 --entrypoint stat \
    --mount "type=volume,src=$VOLUME,dst=/var/cache/rusty-dlna" \
    "$IMAGE" -c '%u:%g' /var/cache/rusty-dlna/externally-copied.mp4)
test "$owner" = 10001:10001
printf 'files\tfirst_migration_ms\tmarked_restart_ms\tmarked_restart_limit_ms\n'
printf '%s\t%s\t%s\t%s\n' "$FILE_COUNT" "$cold_ms" "$warm_ms" "$MAX_WARM_MS"
printf 'benchmark image: %s\n' "$IMAGE"
printf 'implementation: %s\n' "$ROOT/scripts/cache-volume-init.sh"
