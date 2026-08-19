#!/bin/sh
# Initialize a persistent cache volume once; normal restarts take the O(1) path.
set -eu

CACHE_ROOT=${1:-/var/cache/rusty-dlna}
REPAIR=${RUSTY_DLNA_REPAIR_OWNERSHIP:-0}
OWNER_UID=10001
OWNER_GID=10001
MARKER="$CACHE_ROOT/.rusty-dlna-ownership-v1"
EXPECTED="version=1 uid=$OWNER_UID gid=$OWNER_GID"

case "$REPAIR" in
    0|1) ;;
    *)
        echo "RUSTY_DLNA_REPAIR_OWNERSHIP must be 0 or 1" >&2
        exit 2
        ;;
esac
test -d "$CACHE_ROOT" || {
    echo "cache root is not a directory: $CACHE_ROOT" >&2
    exit 1
}

ROOT_OWNER=$(stat -c '%u:%g' "$CACHE_ROOT")
MARKER_VALUE=$(cat "$MARKER" 2>/dev/null || true)
if [ "$REPAIR" = 1 ] || [ "$ROOT_OWNER" != "$OWNER_UID:$OWNER_GID" ] || [ "$MARKER_VALUE" != "$EXPECTED" ]; then
    find "$CACHE_ROOT" -xdev \( ! -uid "$OWNER_UID" -o ! -gid "$OWNER_GID" \) \
        -exec chown "$OWNER_UID:$OWNER_GID" {} +
    chmod 0750 "$CACHE_ROOT"
    temporary="$MARKER.tmp.$$"
    umask 027
    printf '%s\n' "$EXPECTED" >"$temporary"
    chown "$OWNER_UID:$OWNER_GID" "$temporary"
    mv -f "$temporary" "$MARKER"
    echo "cache ownership initialized: $CACHE_ROOT"
else
    echo "cache ownership marker valid; recursive scan skipped: $CACHE_ROOT"
fi
