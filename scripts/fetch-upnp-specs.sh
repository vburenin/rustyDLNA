#!/usr/bin/env bash
# Re-download the official UPnP Forum PDFs into docs/specs/.
# Does not fetch IEC 62481 (DLNA Guidelines); those are not redistributable.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/docs/specs"
mkdir -p "$dest"
cd "$dest"

base=https://upnp.org/specs
urls=(
  "$base/arch/UPnP-arch-DeviceArchitecture-v1.0-20060720.pdf"
  "$base/arch/UPnP-arch-DeviceArchitecture-v1.1.pdf"
  "$base/av/UPnP-av-AVArchitecture-v1.pdf"
  "$base/av/UPnP-av-AVArchitecture-v2-20101231.pdf"
  "$base/av/UPnP-av-MediaServer-v1-Device.pdf"
  "$base/av/UPnP-av-ContentDirectory-v1-Service.pdf"
  "$base/av/UPnP-av-ConnectionManager-v1-Service.pdf"
  "$base/av/UPnP-av-ContentDirectory-v3-Service.pdf"
  "$base/av/UPnP-av-ContentDirectory-v4-Service-20101231.pdf"
  "$base/av/UPnP-av-ConnectionManager-v2-Service.pdf"
  "$base/av/UPnP-av-AVTransport-v1-Service.pdf"
)

for u in "${urls[@]}"; do
  name=$(basename "$u")
  echo "GET $u"
  curl -fsSL --retry 3 --retry-delay 1 -A "rustyDLNA-spec-fetch/0.1" -o "$name" "$u"
done

if [[ -f SHA256SUMS ]]; then
  sha256sum -c SHA256SUMS
fi
