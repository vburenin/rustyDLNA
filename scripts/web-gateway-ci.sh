#!/bin/sh
# Build the browser gateway image, validate its nginx configuration, and run
# the gateway smoke (route isolation plus the native client's HEAD, Range, and
# DELETE methods) against a disposable rustyDLNA backend.
#
# web-gateway/nginx.conf deliberately proxies to host.docker.internal:8200.
# The backend container answers under that name on a private bridge network,
# so its TCP 8200 exists only in that container's network namespace: no host
# port 8200 or 1900 is bound, and the shipped gateway configuration is used
# unmodified. Only the gateway publishes an ephemeral 127.0.0.1 port.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
BACKEND_IMAGE=${RUSTY_DLNA_GATEWAY_BACKEND_IMAGE:?set to a built rustyDLNA image}
GATEWAY_IMAGE=${RUSTY_WEB_IMAGE:-rusty-web:ci}
NAME="rusty-web-ci-$$"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-web-ci.XXXXXX")
STARTED=""

cleanup() {
	status=$?
	trap - EXIT INT TERM
	if [ "$status" -ne 0 ] && [ -n "$STARTED" ]; then
		docker logs "$NAME-backend" >&2 2>&1 || true
		docker logs "$NAME-gateway" >&2 2>&1 || true
	fi
	docker rm --force "$NAME-gateway" "$NAME-backend" >/dev/null 2>&1 || true
	docker network rm "$NAME" >/dev/null 2>&1 || true
	rm -rf -- "$TMP"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker image inspect "$BACKEND_IMAGE" >/dev/null
if [ -z "${RUSTY_WEB_IMAGE:-}" ]; then
	docker build --pull --file "$ROOT/Dockerfile.web" --tag "$GATEWAY_IMAGE" "$ROOT"
fi

echo "gateway: nginx -t"
# The upstream name must resolve for the syntax check; nothing is contacted.
docker run --rm --read-only \
	--tmpfs /tmp:rw,noexec,nosuid,nodev,size=16m,uid=101,gid=101,mode=0700 \
	--add-host host.docker.internal:127.0.0.1 \
	"$GATEWAY_IMAGE" -t

mkdir -p "$TMP/media"
cp "$ROOT/testdata/library/video/tagged.mp4" "$TMP/media/contract.mp4"
chmod 0755 "$TMP" "$TMP/media"
chmod 0644 "$TMP/media/contract.mp4"
printf '%s\n' \
	'friendly_name = "rustyDLNA gateway CI"' \
	'media_dir = ["V,/storage/video"]' \
	'listen_ip = "0.0.0.0"' \
	'cache_dir = "/var/cache/rusty-dlna"' \
	'rescan_secs = 0' \
	>"$TMP/rusty-dlna.toml"
chmod 0644 "$TMP/rusty-dlna.toml"

docker network create "$NAME" >/dev/null
STARTED=1
docker run --detach --name "$NAME-backend" \
	--network "$NAME" --network-alias host.docker.internal \
	--env RUSTY_DLNA_HTTP_PORT=8200 --env RUSTY_DLNA_SSDP_PORT=11900 \
	--mount "type=bind,src=$TMP/rusty-dlna.toml,dst=/etc/rusty-dlna.toml,readonly" \
	--mount "type=bind,src=$TMP/media,dst=/storage/video,readonly" \
	--tmpfs /var/cache/rusty-dlna:rw,uid=10001,gid=10001,mode=0750 \
	--cap-drop ALL --security-opt no-new-privileges:true \
	"$BACKEND_IMAGE" >/dev/null
# Matches docker-compose.web.yaml apart from the ephemeral loopback port.
docker run --detach --name "$NAME-gateway" --network "$NAME" \
	--read-only \
	--tmpfs /tmp:rw,noexec,nosuid,nodev,size=16m,uid=101,gid=101,mode=0700 \
	--cap-drop ALL --security-opt no-new-privileges:true \
	--publish 127.0.0.1::8080 \
	"$GATEWAY_IMAGE" >/dev/null
published=$(docker port "$NAME-gateway" 8080/tcp | head -n 1)
test -n "$published"
base="http://$published"

tries=0
until curl --silent --fail --max-time 2 "$base/api/web/library?view=library&kind=video" |
	grep -q '"file_name":"contract.mp4"'; do
	tries=$((tries + 1))
	[ "$tries" -lt 60 ] || {
		echo "gateway: backend library did not become ready" >&2
		exit 1
	}
	sleep 1
done

"$ROOT/scripts/web-gateway-smoke.sh" "$base" contract.mp4
echo "gateway CI OK"
