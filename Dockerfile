# Production rustyDLNA image. SSDP needs host networking at *run* time
# (see docker-compose.yaml). This file must not request host network —
# docker-compose.test.yaml stays on a bridge with no published 8200/1900.

FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS build

# Build and run against the same FFmpeg 8 ABI. The official Rust toolchain is
# copied into the Ubuntu builder so ffmpeg-sys-next links to libavformat 62,
# rather than to Debian bookworm's incompatible libavformat 59.
FROM ubuntu:resolute-20260811.1@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b AS app-build

COPY --from=build /usr/local/cargo /usr/local/cargo
COPY --from=build /usr/local/rustup /usr/local/rustup

ARG TARGETARCH
ARG DOVI_TOOL_VERSION=2.3.3
ARG FFMPEG_VERSION=7:8.0.1-3ubuntu2
ARG SOURCE_DATE_EPOCH=0

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH \
    SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH \
    DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        clang \
        curl \
        ca-certificates \
        libavformat-dev="$FFMPEG_VERSION" \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# The digest-pinned rust:1.97.1 image is the build toolchain. Do not copy the
# host rust-toolchain file here: its rustfmt/clippy component request would
# make a release build contact rustup and weaken offline reproducibility.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY assets ./assets

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rusty-dlna-target-ffmpeg8,target=/src/target \
    cargo build --locked --release -p rusty-dlna \
    && cp target/release/rusty-dlna /tmp/rusty-dlna \
    && strip /tmp/rusty-dlna

# Official immutable quietvoid release assets, pinned and checksum verified.
# A project-local archive avoids a network dependency; BuildKit's cache keeps
# clean checkouts from downloading the same archive on every source rebuild.
COPY .docker-cache/dovi-tool/ /tmp/dovi-tool-cache/

RUN --mount=type=cache,id=rusty-dlna-dovi-tool,target=/var/cache/dovi-tool,sharing=locked \
    set -eux; \
    case "$TARGETARCH" in \
      amd64) dovi_arch=x86_64; dovi_sha=5dae82cb2becd3b9fd726127f936a8d32635e60746d16238fdfded12aa05988c ;; \
      arm64) dovi_arch=aarch64; dovi_sha=daf538c275f4e702219ce8eb61db28382193ac9d0126e1ef4185a88303af4485 ;; \
      *) echo "unsupported dovi_tool architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    archive="dovi_tool-${DOVI_TOOL_VERSION}-${dovi_arch}-unknown-linux-musl.tar.gz"; \
    local_archive="/tmp/dovi-tool-cache/$archive"; \
    cached_archive="/var/cache/dovi-tool/$archive"; \
    if [ -f "$local_archive" ]; then \
      archive_path="$local_archive"; \
    else \
      archive_path="$cached_archive"; \
      if [ ! -f "$archive_path" ]; then \
        curl -fsSLo "$archive_path.download" \
          "https://github.com/quietvoid/dovi_tool/releases/download/${DOVI_TOOL_VERSION}/$archive"; \
        echo "$dovi_sha  $archive_path.download" | sha256sum -c -; \
        mv "$archive_path.download" "$archive_path"; \
      fi; \
    fi; \
    echo "$dovi_sha  $archive_path" | sha256sum -c -; \
    mkdir -p /opt/dovi/bin; \
    tar -xzf "$archive_path" -C /opt/dovi/bin; \
    chmod 0755 /opt/dovi/bin/dovi_tool

# Byte-for-byte fixture reproduction uses the same FFmpeg decoder/encoder as
# production and the exact muxer that created the checked-in Matroska file.
# Deterministic mkvmerge output is stable only within one muxer version.
FROM ubuntu:resolute-20260811.1@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b AS fixture-tools

ARG FIXTURE_FFMPEG_VERSION=7:8.0.1-3ubuntu2
ARG FIXTURE_MKVTOOLNIX_VERSION=97.0-1build1

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        ffmpeg="$FIXTURE_FFMPEG_VERSION" \
        mkvtoolnix="$FIXTURE_MKVTOOLNIX_VERSION" \
    && rm -rf /var/lib/apt/lists/*

COPY --from=app-build /opt/dovi/bin/dovi_tool /usr/local/bin/dovi_tool

FROM ubuntu:resolute-20260811.1@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b

ARG FFMPEG_VERSION=7:8.0.1-3ubuntu2
ARG BUILD_VERSION=dev
ARG VCS_REF=unknown
ARG BUILD_DATE=1970-01-01T00:00:00Z
ARG SOURCE_DATE_EPOCH=0

LABEL org.opencontainers.image.source="https://github.com/vburenin/rustyDLNA" \
      org.opencontainers.image.licenses="GPL-2.0-only" \
      org.opencontainers.image.title="rustyDLNA" \
      org.opencontainers.image.version="$BUILD_VERSION" \
      org.opencontainers.image.revision="$VCS_REF" \
      org.opencontainers.image.created="$BUILD_DATE"

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=UTC \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH

# The Ubuntu base carries Pebble for managed Rock services. rustyDLNA runs its
# own foreground process and has no Pebble plan, so exclude that unused binary
# from the runtime filesystem and vulnerability surface.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ffmpeg="$FFMPEG_VERSION" \
        ca-certificates \
        curl \
        libegl1 \
        tzdata \
    && ln -snf /usr/share/zoneinfo/$TZ /etc/localtime \
    && echo $TZ > /etc/timezone \
    && mkdir -p /var/cache/rusty-dlna /storage/video \
    && groupadd --gid 10001 rusty-dlna \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin rusty-dlna \
    && chown -R 10001:10001 /var/cache/rusty-dlna \
    && rm -f /usr/bin/pebble \
    && rm -rf /var/lib/apt/lists/*

COPY --from=app-build /tmp/rusty-dlna /usr/local/bin/rusty-dlna
COPY --from=app-build /opt/dovi/bin/ /usr/local/bin/
COPY --chmod=0755 scripts/cache-volume-init.sh /usr/local/libexec/rusty-dlna-cache-volume-init
COPY rusty-dlna.toml /etc/rusty-dlna.toml
COPY LICENSE /usr/share/doc/rusty-dlna/LICENSE
COPY THIRD_PARTY_NOTICES.md /usr/share/doc/rusty-dlna/THIRD_PARTY_NOTICES.md

# HTTP descriptions/SOAP/media. SSDP is UDP/1900 (host network at run time).
EXPOSE 8200/tcp 1900/udp

USER 10001:10001

# Foreground (container PID 1). Config is bind-mounted over this default
# from compose when RUSTY_DLNA_CONF / override is set.
CMD ["rusty-dlna", "--config", "/etc/rusty-dlna.toml"]
