#!/usr/bin/env bash
# Exercise real multicast discovery from a host-network container on a disposable VM.
set -Eeuo pipefail

for tool in docker ip python3 curl; do
  command -v "$tool" >/dev/null || {
    echo "host-network-e2e: missing $tool" >&2
    exit 77
  }
done

IMAGE=${RUSTY_DLNA_HOST_IMAGE:-rusty-dlna:local}
HTTP_PORT=${RUSTY_DLNA_HOST_HTTP_PORT:-18240}
SSDP_PORT=${RUSTY_DLNA_HOST_SSDP_PORT:-11940}
DIAGNOSTICS=${RUSTY_DLNA_HOST_DIAGNOSTICS:-}
NAME="rusty-dlna-host-e2e-$$"
TMP_DIR=$(mktemp -d -t rusty-dlna-host.XXXXXX)
CAPTURE_PID=""
STARTED=0

case "$HTTP_PORT:$SSDP_PORT" in
  *[!0-9:]*|0:*|*:0)
    echo "host-network-e2e: ports must be positive integers" >&2
    exit 2
    ;;
esac
docker image inspect "$IMAGE" >/dev/null
route=$(ip -4 route get 239.255.255.250)
interface=$(printf '%s\n' "$route" | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n 1)
address=$(printf '%s\n' "$route" | sed -n 's/.* src \([^ ]*\).*/\1/p' | head -n 1)
test -n "$interface"
test -n "$address"

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ -n $CAPTURE_PID ]]; then
    kill -TERM "$CAPTURE_PID" 2>/dev/null || true
    wait "$CAPTURE_PID" 2>/dev/null || true
  fi
  if [[ $STARTED -eq 1 ]]; then
    docker stop --time 30 "$NAME" >/dev/null 2>&1 || true
    docker logs "$NAME" >"${TMP_DIR}/daemon.log" 2>&1 || true
    docker inspect "$NAME" >"${TMP_DIR}/container-inspect.json" 2>&1 || true
    docker rm --force "$NAME" >/dev/null 2>&1 || true
  fi
  if [[ $status -ne 0 ]]; then
    ip -details address show >"${TMP_DIR}/host-addresses.log" 2>&1 || true
    ip maddress show >"${TMP_DIR}/host-multicast.log" 2>&1 || true
    ss -lunp >"${TMP_DIR}/host-sockets.log" 2>&1 || true
    if [[ -n $DIAGNOSTICS ]]; then
      mkdir -p "$DIAGNOSTICS"
      cp -a "${TMP_DIR}/." "$DIAGNOSTICS/"
      chmod -R a+rX "$DIAGNOSTICS"
      echo "host-network-e2e: failure diagnostics saved to $DIAGNOSTICS" >&2
    fi
    for log in "${TMP_DIR}/probe.log" "${TMP_DIR}/daemon.log"; do
      [[ -f $log ]] && { echo "--- $log" >&2; tail -200 "$log" >&2; }
    done
  fi
  rm -rf -- "$TMP_DIR"
  exit "$status"
}
trap cleanup EXIT INT TERM

mkdir -p "${TMP_DIR}/media"
printf '%s\n' \
  'friendly_name = "rustyDLNA-host-network-e2e"' \
  'listen_ip = "0.0.0.0"' \
  'advertise_ip = "'"$address"'"' \
  'network_interface = ["'"$interface"'"]' \
  'media_dir = ["V,/storage/video"]' \
  'cache_dir = "/var/cache/rusty-dlna"' \
  'db_dir = "/var/cache/rusty-dlna"' \
  'rescan_secs = 0' \
  >"${TMP_DIR}/config.toml"

if [[ ${EUID} -eq 0 ]] && command -v tcpdump >/dev/null; then
  tcpdump -U -n -i "$interface" "udp port $SSDP_PORT" \
    -w "${TMP_DIR}/host-ssdp.pcap" >"${TMP_DIR}/tcpdump.log" 2>&1 &
  CAPTURE_PID=$!
fi

docker run --detach --name "$NAME" --network host \
  --env "RUSTY_DLNA_HTTP_PORT=$HTTP_PORT" \
  --env "RUSTY_DLNA_SSDP_PORT=$SSDP_PORT" \
  --mount "type=bind,src=${TMP_DIR}/config.toml,dst=/etc/rusty-dlna.toml,readonly" \
  --mount "type=bind,src=${TMP_DIR}/media,dst=/storage/video,readonly" \
  --tmpfs /var/cache/rusty-dlna:rw,uid=10001,gid=10001,mode=0750 \
  "$IMAGE" >/dev/null
STARTED=1

for _ in $(seq 1 100); do
  if curl --silent --fail --max-time 1 "http://127.0.0.1:${HTTP_PORT}/rootDesc.xml" >/dev/null; then
    break
  fi
  sleep 0.1
done
curl --silent --fail --max-time 1 "http://127.0.0.1:${HTTP_PORT}/rootDesc.xml" >/dev/null

probe='import socket,sys
local,http_port,ssdp_port=sys.argv[1],int(sys.argv[2]),int(sys.argv[3])
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM,socket.IPPROTO_UDP)
s.setsockopt(socket.IPPROTO_IP,socket.IP_MULTICAST_IF,socket.inet_aton(local))
s.bind((local,0)); s.settimeout(5)
q=b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:%d\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: upnp:rootdevice\r\n\r\n"%ssdp_port
s.sendto(q,("239.255.255.250",ssdp_port))
data,peer=s.recvfrom(8192); text=data.decode("ascii")
assert peer[0]==local,(peer,text)
assert "LOCATION: http://%s:%d/rootDesc.xml"%(local,http_port) in text,text
print("peer=%s:%s"%peer)
print(text)'
python3 -c "$probe" "$address" "$HTTP_PORT" "$SSDP_PORT" >"${TMP_DIR}/probe.log" 2>&1

docker stop --time 30 "$NAME" >/dev/null
docker logs "$NAME" >"${TMP_DIR}/daemon.log" 2>&1
docker inspect "$NAME" >"${TMP_DIR}/container-inspect.json" 2>&1
STARTED=0
docker rm "$NAME" >/dev/null
grep -Fq 'SSDP M-SEARCH reply' "${TMP_DIR}/daemon.log"
echo "host-network-e2e: multicast reply source and LOCATION matched $address on ports $SSDP_PORT/$HTTP_PORT"
