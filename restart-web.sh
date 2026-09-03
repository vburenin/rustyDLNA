#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
COMPOSE_FILE="$ROOT/docker-compose.web.yaml"
SERVICE=rusty-web
CONTAINER=rusty-web
START_TIMEOUT=${RUSTY_WEB_START_TIMEOUT:-60}

usage() {
	cat <<'EOF'
Usage: ./restart-web.sh

Rebuild and recreate only the browser gateway, wait for its healthcheck, run
the browser/DLNA route-isolation smoke test, and print its Compose status.
The rusty-dlna container is not restarted.

Environment:
  RUSTY_WEB_BIND_IP       Published bind address (default 127.0.0.1)
  RUSTY_WEB_PORT          Published TCP port (default 8201)
  RUSTY_WEB_START_TIMEOUT Health wait in seconds (default 60)
EOF
}

case ${1:-} in
	-h|--help)
		usage
		exit 0
		;;
	"") ;;
	*)
		usage >&2
		exit 2
		;;
esac

case $START_TIMEOUT in
	''|*[!0-9]*|0)
		echo "RUSTY_WEB_START_TIMEOUT must be a positive whole number" >&2
		exit 2
		;;
esac

cd "$ROOT"
docker compose -f "$COMPOSE_FILE" config --quiet
docker compose -f "$COMPOSE_FILE" up -d --build --force-recreate "$SERVICE"

deadline=$((SECONDS + START_TIMEOUT))
while :; do
	health=$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}missing{{end}}' "$CONTAINER" 2>/dev/null || true)
	case $health in
		healthy) break ;;
		unhealthy|missing)
			docker compose -f "$COMPOSE_FILE" logs --no-color "$SERVICE" >&2 || true
			echo "rusty-web healthcheck failed: $health" >&2
			exit 1
			;;
	esac
	if (( SECONDS >= deadline )); then
		docker compose -f "$COMPOSE_FILE" logs --no-color "$SERVICE" >&2 || true
		echo "rusty-web did not become healthy within ${START_TIMEOUT}s" >&2
		exit 1
	fi
	sleep 1
done

published=$(docker compose -f "$COMPOSE_FILE" port "$SERVICE" 8080 | head -n 1)
if [[ -z $published ]]; then
	echo "rusty-web has no published TCP port" >&2
	exit 1
fi
"$ROOT/scripts/web-gateway-smoke.sh" "http://$published"
docker compose -f "$COMPOSE_FILE" ps "$SERVICE"
