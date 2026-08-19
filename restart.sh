#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR"

CACHE_VOLUME=${RUSTY_DLNA_CACHE_VOLUME:-rusty-dlna-cache}
START_TIMEOUT=${RUSTY_DLNA_START_TIMEOUT:-120}
IMAGE=rusty-dlna:local

if [[ ! "$CACHE_VOLUME" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]]; then
    echo "invalid RUSTY_DLNA_CACHE_VOLUME: $CACHE_VOLUME" >&2
    exit 2
fi
if [[ ! "$START_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
    echo "RUSTY_DLNA_START_TIMEOUT must be a positive integer" >&2
    exit 2
fi

docker compose config --quiet
docker compose build

# Stop before touching the persistent catalog. This is a no-op on first use.
docker compose stop --timeout 30 rusty-dlna
# `compose create` provisions and labels the named volume without starting the
# daemon. Avoid `docker volume create` here: an unlabeled volume makes Compose
# warn on every later deployment even though the data is otherwise reusable.
docker compose create rusty-dlna

# Older deployments may have populated the volume as root. Repair it with a
# one-shot root container, then prove the image's normal uid/gid can write.
docker run --rm \
    --user 0:0 \
    --entrypoint /bin/sh \
    --mount "type=volume,src=$CACHE_VOLUME,dst=/var/cache/rusty-dlna" \
    "$IMAGE" -euc '
        chown -R 10001:10001 /var/cache/rusty-dlna
        chmod 0750 /var/cache/rusty-dlna
    '
docker run --rm \
    --entrypoint /bin/sh \
    --mount "type=volume,src=$CACHE_VOLUME,dst=/var/cache/rusty-dlna" \
    "$IMAGE" -euc '
        probe=/var/cache/rusty-dlna/.write-probe-$$
        : >"$probe"
        rm -f -- "$probe"
    '

docker compose up --detach --remove-orphans --wait \
    --wait-timeout "$START_TIMEOUT"
docker compose ps
