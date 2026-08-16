#!/bin/sh
# Run rustyDLNA tests in an isolated bridge container, then prove the
# live MiniDLNA on :8200 is unchanged. Never uses host networking.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

snap() {
	curl -sI --max-time 3 http://127.0.0.1:8200/ | tr -d '\r' | grep -i '^Server:' || true
}

echo "== isolation before tests =="
"$ROOT/scripts/assert-isolation.sh"
before=$(snap)
echo "MiniDLNA before: $before"

echo "== container tests (bridge, no published ports) =="
# --rm so nothing is left running. No -p. Default project network = bridge.
docker compose -f docker-compose.test.yaml run --rm --no-deps rusty-dlna-test

echo "== isolation after tests =="
"$ROOT/scripts/assert-isolation.sh"
after=$(snap)
echo "MiniDLNA after:  $after"

[ -n "$before" ] && [ "$before" = "$after" ] || {
	echo "FAIL: MiniDLNA Server header changed" >&2
	echo "  before: $before" >&2
	echo "  after:  $after" >&2
	exit 1
}

# Test container must not still be running
if docker ps --filter name=rusty-dlna-test --format '{{.Names}}' | grep -q .; then
	echo "FAIL: rusty-dlna-test still running" >&2
	exit 1
fi

echo "PROVE OK (MiniDLNA untouched: $after)"
