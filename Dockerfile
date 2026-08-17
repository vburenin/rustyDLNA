# Production rustyDLNA image. SSDP needs host networking at *run* time
# (see docker-compose.yaml). This file must not request host network —
# docker-compose.test.yaml stays on a bridge with no published 8200/1900.

FROM rust:bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        clang \
        libavformat-dev \
        libavcodec-dev \
        libavutil-dev \
        libavfilter-dev \
        libavdevice-dev \
        libswscale-dev \
        libswresample-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

RUN cargo build --release -p rusty-dlna \
    && strip target/release/rusty-dlna

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    TZ=UTC \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

RUN apt-get update && apt-get install -y --no-install-recommends \
        ffmpeg \
        ca-certificates \
        tzdata \
    && ln -snf /usr/share/zoneinfo/$TZ /etc/localtime \
    && echo $TZ > /etc/timezone \
    && mkdir -p /var/cache/rusty-dlna /storage/video \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/rusty-dlna /usr/local/bin/rusty-dlna
COPY rusty-dlna.toml /etc/rusty-dlna.toml

# HTTP descriptions/SOAP/media. SSDP is UDP/1900 (host network at run time).
EXPOSE 8200/tcp 1900/udp

# Foreground (container PID 1). Config is bind-mounted over this default
# from compose when RUSTY_DLNA_CONF / override is set.
CMD ["rusty-dlna", "--config", "/etc/rusty-dlna.toml"]
