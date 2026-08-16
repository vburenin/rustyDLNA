#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
export PATH="${CARGO_HOME}/bin:$PATH"
cd "$ROOT"
test -f replica.md
test -f docs/replica.md
test -f docs/ARCHITECTURE.md
test -f docs/TRANSCODE.md
grep -F -q '/rootDesc.xml' replica.md
grep -F -q '239.255.255.250' replica.md
grep -F -q 'FLAG_SKIP_DLNA_PN' replica.md
cargo test --workspace
cargo run -p rusty-dlna -- --check
# Host-side isolation (does not start rusty listeners)
if [ "${SKIP_ISOLATION:-}" != "1" ] && command -v curl >/dev/null 2>&1; then
	"$ROOT/scripts/assert-isolation.sh"
fi
echo "workspace OK"
