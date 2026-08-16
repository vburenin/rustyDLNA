#!/bin/sh
# Fail if rustyDLNA is configured or running in a way that can steal
# MiniDLNA's TCP 8200 / UDP 1900. Safe to run anytime.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

fail() { echo "ISOLATION FAIL: $*" >&2; exit 1; }
ok() { echo "OK: $*"; }

# Compose must never request host networking or live ports.
if grep -nE '^[^#]*network_mode:[[:space:]]*host' docker-compose.test.yaml Dockerfile 2>/dev/null; then
	fail "test compose/Dockerfile requests network_mode: host"
fi
ok "no host network in test files"

if grep -nE '^[^#]*["[:space:]]8200:|^[^#]*["[:space:]]1900:' docker-compose.test.yaml 2>/dev/null; then
	fail "test compose publishes 8200 or 1900"
fi
ok "test compose does not publish 8200/1900"

# Live MiniDLNA must still be the thing on 8200.
hdr=$(curl -sI --max-time 3 http://127.0.0.1:8200/ | tr -d '\r' || true)
echo "$hdr" | grep -q 'HTTP/1.1 200' || fail "nothing healthy on :8200"
echo "$hdr" | grep -q 'MiniDLNA/' || fail ":8200 is not MiniDLNA (got: $hdr)"
echo "$hdr" | grep -q 'rustyDLNA' && fail ":8200 already serves rustyDLNA"
ok "host :8200 is MiniDLNA"

if command -v docker >/dev/null 2>&1; then
	st=$(docker inspect minidlna --format '{{.State.Status}} {{.HostConfig.NetworkMode}}' 2>/dev/null || true)
	echo "$st" | grep -q '^running host$' || fail "minidlna container not running host-network (got: '$st')"
	ok "minidlna container running (host network)"

	# No leftover rusty test container on host network
	if docker ps --format '{{.Names}} {{.ID}}' | grep -q '^rusty-dlna'; then
		for id in $(docker ps --filter name=rusty-dlna --format '{{.ID}}'); do
			mode=$(docker inspect "$id" --format '{{.HostConfig.NetworkMode}}')
			[ "$mode" = "host" ] && fail "rusty-dlna container $id uses host network"
		done
	fi
	ok "no rusty-dlna container on host network"
fi

ok "isolation holds"
