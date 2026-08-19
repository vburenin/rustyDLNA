#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR"

usage() {
    cat <<'EOF'
Usage: ./restart.sh [--clean]

Rebuild and restart the live rusty-dlna container.

  --clean   Discard the cache volume and start with an empty catalog.
            Removes files.db, artwork, derived images, transcode cache,
            Kodi bookmarks, and the persisted UUID file. The configured
            name (RUSTY_DLNA_CACHE_VOLUME) is kept; Compose recreates a
            labeled empty volume. Bind-mounted caches are refused.
            A uuid= in the mounted TOML is kept.

  -h, --help   Show this help.
EOF
}

CLEAN=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --clean)
            CLEAN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

CACHE_VOLUME=${RUSTY_DLNA_CACHE_VOLUME:-rusty-dlna-cache}
START_TIMEOUT=${RUSTY_DLNA_START_TIMEOUT:-120}
IMAGE=rusty-dlna:local
CONTAINER=rusty-dlna
CACHE_DEST=/var/cache/rusty-dlna

if [[ ! "$CACHE_VOLUME" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]]; then
    echo "invalid RUSTY_DLNA_CACHE_VOLUME: $CACHE_VOLUME" >&2
    exit 2
fi
if [[ ! "$START_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
    echo "RUSTY_DLNA_START_TIMEOUT must be a positive integer" >&2
    exit 2
fi

cache_mount_field() {
    local field=$1
    docker inspect \
        --format "{{range .Mounts}}{{if eq .Destination \"$CACHE_DEST\"}}{{.$field}}{{end}}{{end}}" \
        "$CONTAINER" 2>/dev/null || true
}

docker compose config --quiet
docker compose build

# Stop before touching the persistent catalog. This is a no-op on first use.
docker compose stop --timeout 30 rusty-dlna

if [[ "$CLEAN" -eq 1 ]]; then
    if docker inspect "$CONTAINER" >/dev/null 2>&1; then
        mount_type=$(cache_mount_field Type)
        mount_name=$(cache_mount_field Name)
        if [[ "$mount_type" != volume || "$mount_name" != "$CACHE_VOLUME" ]]; then
            echo "refusing --clean: $CACHE_DEST is ${mount_type:-unmounted}${mount_name:+ $mount_name}, not Docker volume $CACHE_VOLUME" >&2
            exit 2
        fi
        # A stopped container still holds the volume. Remove it before
        # `volume rm`; force is non-interactive, not "in use".
        docker compose rm --force rusty-dlna
    fi
    if docker volume inspect "$CACHE_VOLUME" >/dev/null 2>&1; then
        echo "discarding cache volume $CACHE_VOLUME (catalog, UUID file, bookmarks, transcode cache)"
        docker volume rm "$CACHE_VOLUME"
    else
        echo "cache volume $CACHE_VOLUME does not exist; Compose will create an empty one"
    fi
fi

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
