#!/bin/sh
# Run rustyDLNA tests in an isolated bridge container. Never uses host
# networking or publishes 8200/1900. Live :8200 may be rustyDLNA.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

echo "== isolation before tests =="
"$ROOT/scripts/assert-isolation.sh"

echo "== container tests (bridge, no published ports) =="
# --rm so nothing is left running. No -p. Default project network = bridge.
docker compose -f docker-compose.test.yaml run --rm --no-deps rusty-dlna-test

echo "== isolation after tests =="
"$ROOT/scripts/assert-isolation.sh"

# Test container must not still be running
if docker ps --filter name=rusty-dlna-test --format '{{.Names}}' | grep -q .; then
	echo "FAIL: rusty-dlna-test still running" >&2
	exit 1
fi

echo "PROVE OK (test compose isolated; live :8200 not published)"
