#!/bin/sh
# Resolve compiler metadata, then update the complete checked Rust pin inventory.
set -eu
ROOT=${RUSTY_DLNA_UPDATE_ROOT:-$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)}
exec python3 "$ROOT/scripts/rust-pins.py" update "$@"
