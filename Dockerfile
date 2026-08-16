# Test/build image only. Never `network_mode: host`.
# Do not publish 8200 or 1900 — those belong to the live MiniDLNA container.
FROM rust:bookworm

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY replica.md rusty-dlna.toml ./
COPY docs ./docs

ENV CARGO_TERM_COLOR=always \
    RUSTY_DLNA_HTTP_PORT=18200 \
    RUSTY_DLNA_SSDP_PORT=11900

# Default command is unit tests + dialect check. No sockets on 8200/1900.
CMD ["sh", "-c", "cargo test --workspace && cargo run -p rusty-dlna -- --check"]
