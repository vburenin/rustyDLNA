#!/bin/sh
# Update every explicit Rust toolchain pin as one reviewable change.
set -eu

ROOT=${RUSTY_DLNA_UPDATE_ROOT:-$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)}
VERSION=${1:-}
IMAGE_DIGEST=${2:-}

case "$VERSION" in
    ''|*[!0-9.]*|.*|*..*|*.)
        echo "usage: $0 X.Y.Z sha256:HEX" >&2
        exit 2
        ;;
esac
test "$(printf '%s' "$VERSION" | tr -cd '.' | wc -c)" -eq 2 || {
    echo "Rust version must have the form X.Y.Z: $VERSION" >&2
    exit 2
}
printf '%s\n' "$IMAGE_DIGEST" | grep -Eq '^sha256:[0-9a-f]{64}$' || {
    echo "Rust image digest must have the form sha256:<64 lowercase hex digits>" >&2
    exit 2
}

CURRENT=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$ROOT/rust-toolchain.toml")
CARGO_VERSION=$(sed -n 's/^rust-version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml")
test -n "$CURRENT" || { echo "rust-toolchain.toml has no channel" >&2; exit 1; }
test "$CURRENT" = "$CARGO_VERSION" || {
    echo "Cargo rust-version $CARGO_VERSION does not match toolchain $CURRENT" >&2
    exit 1
}
test "$CURRENT" != "$VERSION" || exit 0

FILES='
rust-toolchain.toml
Cargo.toml
Dockerfile
.github/workflows/ci.yml
.github/workflows/release.yml
.github/workflows/soak.yml
docs/DISTRIBUTION.md
improvements_plan.md
'
for relative in $FILES; do
    grep -Fq "$CURRENT" "$ROOT/$relative" || {
        echo "$relative does not contain the current Rust pin $CURRENT" >&2
        exit 1
    }
done

for relative in $FILES; do
    OLD_VERSION=$CURRENT NEW_VERSION=$VERSION perl -pi -e \
        's/\Q$ENV{OLD_VERSION}\E/$ENV{NEW_VERSION}/g' "$ROOT/$relative"
done

NEW_FROM="FROM rust:${VERSION}-bookworm@${IMAGE_DIGEST} AS build"
NEW_FROM=$NEW_FROM perl -pi -e \
    's/^FROM rust:[0-9]+\.[0-9]+\.[0-9]+-bookworm\@sha256:[0-9a-f]{64} AS build$/$ENV{NEW_FROM}/' \
    "$ROOT/Dockerfile"
grep -Fqx "$NEW_FROM" "$ROOT/Dockerfile" || {
    echo "failed to set the pinned Rust builder image" >&2
    exit 1
}

PACKAGE_VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
"$ROOT/scripts/release-contract.sh" "v$PACKAGE_VERSION"
printf 'updated Rust %s -> %s (%s)\n' "$CURRENT" "$VERSION" "$IMAGE_DIGEST"
