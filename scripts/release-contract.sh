#!/bin/sh
# Validate the version/toolchain contract before a release mutates registries.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
TAG=${1:-${GITHUB_REF_NAME:-}}
VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
RUST_VERSION=$(sed -n 's/^rust-version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
TOOLCHAIN=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$ROOT/rust-toolchain.toml" | head -n 1)

[ -n "$VERSION" ] || { echo "workspace package version is missing" >&2; exit 1; }
[ -n "$TAG" ] || { echo "usage: $0 vX.Y.Z" >&2; exit 2; }
printf '%s\n' "$VERSION" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || {
    echo "workspace version is not a release SemVer: $VERSION" >&2
    exit 1
}
[ "$TAG" = "v$VERSION" ] || {
    echo "release tag $TAG does not match workspace version v$VERSION" >&2
    exit 1
}
[ "$RUST_VERSION" = "$TOOLCHAIN" ] || {
    echo "Cargo rust-version $RUST_VERSION does not match toolchain $TOOLCHAIN" >&2
    exit 1
}
grep -Fq "FROM rust:${TOOLCHAIN}-" "$ROOT/Dockerfile" || {
    echo "Docker build image does not use Rust $TOOLCHAIN" >&2
    exit 1
}
for file in \
    .github/workflows/ci.yml \
    .github/workflows/release.yml \
    .github/workflows/soak.yml \
    docs/DISTRIBUTION.md
do
    grep -Fq "$TOOLCHAIN" "$ROOT/$file" || {
        echo "$file does not use or document Rust $TOOLCHAIN" >&2
        exit 1
    }
done
grep -Fq "## $VERSION -" "$ROOT/CHANGELOG.md" || {
    echo "CHANGELOG.md has no section for $VERSION" >&2
    exit 1
}

if [ -n "${RELEASE_IMAGE:-}" ]; then
    IMAGE_VERSION=$(docker image inspect --format '{{ index .Config.Labels "org.opencontainers.image.version" }}' "$RELEASE_IMAGE")
    [ "$IMAGE_VERSION" = "$TAG" ] || {
        echo "image version label $IMAGE_VERSION does not match $TAG" >&2
        exit 1
    }
    if [ -n "${RELEASE_REVISION:-}" ]; then
        IMAGE_REVISION=$(docker image inspect --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' "$RELEASE_IMAGE")
        [ "$IMAGE_REVISION" = "$RELEASE_REVISION" ] || {
            echo "image revision $IMAGE_REVISION does not match $RELEASE_REVISION" >&2
            exit 1
        }
    fi
    ACTUAL=$(docker run --rm --entrypoint rusty-dlna "$RELEASE_IMAGE" --version)
    [ "$ACTUAL" = "rusty-dlna $VERSION" ] || {
        echo "image binary version is inconsistent: $ACTUAL" >&2
        exit 1
    }
fi

printf 'release contract OK: tag=%s version=%s rust=%s\n' "$TAG" "$VERSION" "$TOOLCHAIN"
