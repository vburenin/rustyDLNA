#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
  echo "ssdp-netns-e2e: root/CAP_NET_ADMIN is required" >&2
  exit 77
fi
for tool in ip python3 curl; do
  command -v "${tool}" >/dev/null || {
    echo "ssdp-netns-e2e: missing ${tool}" >&2
    exit 77
  }
done

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary=${RUSTY_DLNA_BIN:-"${repo_dir}/target/debug/rusty-dlna"}
if [[ ! -x ${binary} ]]; then
  cargo build -p rusty-dlna --manifest-path "${repo_dir}/Cargo.toml"
fi

suffix=$$
server_ns="rdls${suffix}"
client_a_ns="rdla${suffix}"
client_b_ns="rdlb${suffix}"
server_a="sa${suffix}"
server_b="sb${suffix}"
client_a="ca${suffix}"
client_b="cb${suffix}"
tmp_dir=$(mktemp -d -t rusty-dlna-netns.XXXXXX)
daemon_pid=""

cleanup() {
  if [[ -n ${daemon_pid} ]]; then
    kill -TERM "${daemon_pid}" 2>/dev/null || true
    wait "${daemon_pid}" 2>/dev/null || true
  fi
  ip netns del "${server_ns}" 2>/dev/null || true
  ip netns del "${client_a_ns}" 2>/dev/null || true
  ip netns del "${client_b_ns}" 2>/dev/null || true
  rm -rf -- "${tmp_dir}"
}
trap cleanup EXIT INT TERM

ip netns add "${server_ns}"
ip netns add "${client_a_ns}"
ip netns add "${client_b_ns}"
ip link add "${server_a}" type veth peer name "${client_a}"
ip link add "${server_b}" type veth peer name "${client_b}"
ip link set "${server_a}" netns "${server_ns}"
ip link set "${server_b}" netns "${server_ns}"
ip link set "${client_a}" netns "${client_a_ns}"
ip link set "${client_b}" netns "${client_b_ns}"

ip -n "${server_ns}" link set lo up
ip -n "${server_ns}" address add 10.201.1.1/24 dev "${server_a}"
ip -n "${server_ns}" address add 10.202.1.1/24 dev "${server_b}"
ip -n "${server_ns}" link set "${server_a}" up
ip -n "${server_ns}" link set "${server_b}" up
ip -n "${client_a_ns}" link set lo up
ip -n "${client_a_ns}" address add 10.201.1.2/24 dev "${client_a}"
ip -n "${client_a_ns}" link set "${client_a}" up
ip -n "${client_a_ns}" route add 239.0.0.0/8 dev "${client_a}"
ip -n "${client_b_ns}" link set lo up
ip -n "${client_b_ns}" address add 10.202.1.2/24 dev "${client_b}"
ip -n "${client_b_ns}" link set "${client_b}" up
ip -n "${client_b_ns}" route add 239.0.0.0/8 dev "${client_b}"

mkdir -p "${tmp_dir}/cache" "${tmp_dir}/media"
printf '%s\n' \
  'friendly_name = "rustyDLNA-netns"' \
  'listen_ip = "0.0.0.0"' \
  'network_interface = ["'"${server_a}"'", "'"${server_b}"'"]' \
  'media_dir = ["V,'"${tmp_dir}"'/media"]' \
  'cache_dir = "'"${tmp_dir}"'/cache"' \
  'rescan_secs = 0' \
  >"${tmp_dir}/config.toml"

ip netns exec "${server_ns}" "${binary}" --config "${tmp_dir}/config.toml" \
  >"${tmp_dir}/daemon.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 100); do
  if ip netns exec "${server_ns}" curl --silent --fail --max-time 1 \
    http://127.0.0.1:8200/rootDesc.xml >/dev/null; then
    break
  fi
  sleep 0.1
done
ip netns exec "${server_ns}" curl --silent --fail --max-time 1 \
  http://127.0.0.1:8200/rootDesc.xml >/dev/null

probe='import socket,sys
local,expected=sys.argv[1:3]
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM,socket.IPPROTO_UDP)
s.setsockopt(socket.IPPROTO_IP,socket.IP_MULTICAST_IF,socket.inet_aton(local))
s.bind((local,0)); s.settimeout(5)
q=b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: upnp:rootdevice\r\n\r\n"
s.sendto(q,("239.255.255.250",1900))
data,peer=s.recvfrom(8192); text=data.decode("ascii")
assert peer[0]==expected,(peer,text)
assert "LOCATION: http://%s:8200/rootDesc.xml"%expected in text,text'

ip netns exec "${client_a_ns}" python3 -c "${probe}" 10.201.1.2 10.201.1.1
ip netns exec "${client_b_ns}" python3 -c "${probe}" 10.202.1.2 10.202.1.1
echo "ssdp-netns-e2e: both interfaces replied with matching source and LOCATION"
