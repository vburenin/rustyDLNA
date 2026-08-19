#!/bin/sh
# Build and exercise the production image without host networking or ports.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-smoke.XXXXXX")
export RUSTY_DLNA_SMOKE_MEDIA="$TMP/media"
export RUSTY_DLNA_SMOKE_CACHE="$TMP/cache"
export RUSTY_DLNA_SMOKE_CONFIG="$TMP/rusty-dlna.toml"
COMPOSE="docker compose -f $ROOT/docker-compose.smoke.yaml"

cleanup() {
    $COMPOSE down --remove-orphans >/dev/null 2>&1 || true
    rm -rf -- "$TMP"
}
trap cleanup EXIT INT TERM

mkdir -p "$RUSTY_DLNA_SMOKE_MEDIA" "$RUSTY_DLNA_SMOKE_CACHE"
chmod 0755 "$TMP" "$RUSTY_DLNA_SMOKE_MEDIA"
chmod 0777 "$RUSTY_DLNA_SMOKE_CACHE"
cp "$ROOT/testdata/library/video/movie.mkv" "$RUSTY_DLNA_SMOKE_MEDIA/movie.mkv"
printf '%s\n' \
    'friendly_name = "rustyDLNA smoke"' \
    'media_dir = ["V,/storage/video"]' \
    'listen_ip = "0.0.0.0"' \
    'cache_dir = "/var/cache/rusty-dlna"' \
    'rescan_secs = 1' \
    >"$RUSTY_DLNA_SMOKE_CONFIG"
chmod 0644 "$RUSTY_DLNA_SMOKE_CONFIG"

cd "$ROOT"
$COMPOSE up --build --detach --wait
$COMPOSE exec -T rusty-dlna-smoke \
    curl --fail --silent http://127.0.0.1:18200/rootDesc.xml \
    | grep -q '<friendlyName>rustyDLNA smoke</friendlyName>'
$COMPOSE exec -T rusty-dlna-smoke \
    sh -c "curl --fail --silent http://127.0.0.1:18200/health | grep -q '\"status\"' && curl --fail --silent http://127.0.0.1:18200/status | grep -Eq '<td>Video</td><td>[1-9]'"
$COMPOSE kill --signal SIGTERM rusty-dlna-smoke

tries=0
while $COMPOSE ps --status running --quiet | grep -q .; do
    tries=$((tries + 1))
    [ "$tries" -lt 20 ] || { echo "smoke daemon did not stop" >&2; exit 1; }
    sleep 1
done
$COMPOSE logs --no-color rusty-dlna-smoke | grep -q 'SSDP byebye sent'
echo "compose smoke OK"
