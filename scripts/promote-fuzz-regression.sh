#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
TARGET=${1:-}
ARTIFACT=${2:-}
SANITIZER=${3:-address}
FUZZ_RUST_TOOLCHAIN=${FUZZ_RUST_TOOLCHAIN:-nightly-2026-08-01}

case "$TARGET" in
	http_request|http_range|ssdp|soap_xml|nfo|url_ids|sidecars) ;;
	*) echo "usage: $0 <target> <artifact> [address|leak|memory|thread|none]" >&2; exit 2 ;;
esac
case "$SANITIZER" in
	address|leak|memory|thread|none) ;;
	*) echo "unsupported sanitizer: $SANITIZER" >&2; exit 2 ;;
esac
test -f "$ARTIFACT" || { echo "artifact is not a regular file: $ARTIFACT" >&2; exit 2; }

ARTIFACT=$(CDPATH= cd -- "$(dirname "$ARTIFACT")" && pwd)/$(basename "$ARTIFACT")
ARTIFACT_DIR="$ROOT/fuzz/artifacts/$TARGET"
MARKER=$(mktemp "${TMPDIR:-/tmp}/rusty-dlna-fuzz-promotion.XXXXXX")
trap 'rm -f "$MARKER"' EXIT INT TERM

cd "$ROOT"
cargo "+$FUZZ_RUST_TOOLCHAIN" fuzz tmin --sanitizer "$SANITIZER" \
	--runs 255 "$TARGET" "$ARTIFACT"

MINIMIZED=$(find "$ARTIFACT_DIR" -maxdepth 1 -type f -name 'minimized-from-*' \
	-newer "$MARKER" -printf '%T@ %p\n' | sort -nr | sed -n '1s/^[^ ]* //p')
test -n "$MINIMIZED" || { echo "cargo-fuzz did not produce a minimized artifact" >&2; exit 1; }

DIGEST=$(sha256sum "$MINIMIZED" | awk '{print $1}')
DEST_DIR="$ROOT/fuzz/regressions/$TARGET"
DEST="$DEST_DIR/$DIGEST"
mkdir -p "$DEST_DIR"
if test -e "$DEST"; then
	cmp "$MINIMIZED" "$DEST"
else
	cp "$MINIMIZED" "$DEST"
fi
printf 'promoted %s\n' "$DEST"
printf 'verify with: cargo +%s fuzz run --sanitizer %s %s %s -- -runs=1 -timeout=5\n' \
	"$FUZZ_RUST_TOOLCHAIN" "$SANITIZER" "$TARGET" "$DEST"
