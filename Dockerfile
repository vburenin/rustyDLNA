# Production rustyDLNA image. SSDP needs host networking at *run* time
# (see docker-compose.yaml). This file must not request host network —
# docker-compose.test.yaml stays on a bridge with no published 8200/1900.

FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS build

ARG TARGETARCH
ARG DOVI_TOOL_VERSION=2.3.3
ARG DEBIAN_SNAPSHOT=20260801T000000Z
ARG SOURCE_DATE_EPOCH=0

ENV SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH

RUN printf '%s\n' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}" \
      'Suites: bookworm bookworm-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}" \
      'Suites: bookworm-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/debian.sources

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        clang \
        curl \
        ca-certificates \
        libavformat-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# The digest-pinned rust:1.97.1 image is the build toolchain. Do not copy the
# host rust-toolchain file here: its rustfmt/clippy component request would
# make a release build contact rustup and weaken offline reproducibility.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY assets ./assets

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release -p rusty-dlna \
    && cp target/release/rusty-dlna /tmp/rusty-dlna \
    && strip /tmp/rusty-dlna

# Official immutable quietvoid release assets, pinned and checksum verified.
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) dovi_arch=x86_64; dovi_sha=5dae82cb2becd3b9fd726127f936a8d32635e60746d16238fdfded12aa05988c ;; \
      arm64) dovi_arch=aarch64; dovi_sha=daf538c275f4e702219ce8eb61db28382193ac9d0126e1ef4185a88303af4485 ;; \
      *) echo "unsupported dovi_tool architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    archive="dovi_tool-${DOVI_TOOL_VERSION}-${dovi_arch}-unknown-linux-musl.tar.gz"; \
    curl -fsSLo "/tmp/$archive" \
      "https://github.com/quietvoid/dovi_tool/releases/download/${DOVI_TOOL_VERSION}/$archive"; \
    echo "$dovi_sha  /tmp/$archive" | sha256sum -c -; \
    mkdir -p /opt/dovi/bin; \
    tar -xzf "/tmp/$archive" -C /opt/dovi/bin; \
    chmod 0755 /opt/dovi/bin/dovi_tool

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241

# bookworm-slim does not ship a CA bundle. Bootstrap it from the already
# digest-pinned build image before switching APT to the HTTPS snapshot.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

ARG DEBIAN_SNAPSHOT=20260801T000000Z
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

RUN printf '%s\n' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}" \
      'Suites: bookworm bookworm-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}" \
      'Suites: bookworm-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/debian.sources

RUN apt-get update && apt-get install -y --no-install-recommends \
        ffmpeg \
        ca-certificates \
        curl \
        tzdata \
    && ln -snf /usr/share/zoneinfo/$TZ /etc/localtime \
    && echo $TZ > /etc/timezone \
    && mkdir -p /var/cache/rusty-dlna /storage/video \
    && groupadd --gid 10001 rusty-dlna \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin rusty-dlna \
    && chown -R 10001:10001 /var/cache/rusty-dlna \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /tmp/rusty-dlna /usr/local/bin/rusty-dlna
COPY --from=build /opt/dovi/bin/ /usr/local/bin/
COPY rusty-dlna.toml /etc/rusty-dlna.toml
COPY LICENSE /usr/share/doc/rusty-dlna/LICENSE
COPY THIRD_PARTY_NOTICES.md /usr/share/doc/rusty-dlna/THIRD_PARTY_NOTICES.md

# HTTP descriptions/SOAP/media. SSDP is UDP/1900 (host network at run time).
EXPOSE 8200/tcp 1900/udp

USER 10001:10001

# Foreground (container PID 1). Config is bind-mounted over this default
# from compose when RUSTY_DLNA_CONF / override is set.
CMD ["rusty-dlna", "--config", "/etc/rusty-dlna.toml"]
