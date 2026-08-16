#!/bin/sh
# Fail if rustyDLNA *tests* are configured to steal the live daemon's
# TCP 8200 / UDP 1900. After cutover those ports are rustyDLNA itself.
# Safe to run anytime.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"

fail() { echo "ISOLATION FAIL: $*" >&2; exit 1; }
ok() { echo "OK: $*"; }

# Test compose must never request host networking or live ports.
# docker-compose.yaml (live daemon) *does* use host network — that is
# the MiniDLNA-shaped production stack, not this check.
if grep -nE '^[^#]*network_mode:[[:space:]]*host' docker-compose.test.yaml 2>/dev/null; then
	fail "test compose requests network_mode: host"
fi
ok "no host network in test compose"

if grep -nE '^[^#]*["[:space:]]8200:|^[^#]*["[:space:]]1900:' docker-compose.test.yaml 2>/dev/null; then
	fail "test compose publishes 8200 or 1900"
fi
ok "test compose does not publish 8200/1900"

# Live :8200 may be rustyDLNA (host process or rusty-dlna container).
# Only the *test* container is forbidden from host networking.
if command -v docker >/dev/null 2>&1; then
	if docker ps --filter name=rusty-dlna-test --format '{{.Names}}' | grep -q .; then
		for id in $(docker ps --filter name=rusty-dlna-test --format '{{.ID}}'); do
			mode=$(docker inspect "$id" --format '{{.HostConfig.NetworkMode}}')
			[ "$mode" = "host" ] && fail "rusty-dlna-test container $id uses host network"
		done
	fi
	ok "no rusty-dlna-test container on host network"
fi

ok "isolation holds"
