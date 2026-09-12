# Docs index — rustyDLNA

- [Architecture](./architecture.md)
- [ADRs](./adr/) — start from `0001-template.md`
- [Plans](./plans/) — use `_template.md` as an issue outline; keep future work in issues

## Product and contributor guides

- [README](../README.md) — purpose, supported hosts, and quick start
- [Compatibility](./COMPATIBILITY.md) — supported surface and exclusions
- [Protocol contract](./PROTOCOL_CONTRACT.md) — wire and catalog invariants
- [Web player](./WEB_PLAYER.md) — browser UI, API, playback, and recovery
- [Transcode](./TRANSCODE.md) — media policy, FFmpeg, HDR, and GPU setup
- [Operations](./OPERATIONS.md) — storage, health, recovery, and native service
- [Distribution](./DISTRIBUTION.md) — releases, reproducibility, and source obligations
- [Large-library benchmark](./LARGE_LIBRARY_BENCHMARK.md) — scale validation
- [Library tools](../contrib/library/README.md) — separate operator programs
- [Fixtures](../testdata/README.md) — provenance and checksums

## Stack and command reference

Project: **rustyDLNA**, a standalone DLNA / UPnP server for a local media library.
The Cargo workspace has eight Rust crates, embedded HTML/CSS/JavaScript, and
Node/Playwright web tests. Python and shell provide operator and validation tools.
Cargo uses `Cargo.lock`; npm uses `package-lock.json`. The Python tools use only
the standard library; no Python package installation is configured or needed.
Sources: `Cargo.toml`, `package.json`, `README.md`, and
`contrib/library/requirements.txt` at the repository root.

All commands below run from the **repository root**. The agent wrapper invokes
`scripts/check.sh` once; do not repeat its constituent checks in that wrapper.

| Check | Command | Source |
| --- | --- | --- |
| Combined verification | `./scripts/agent-verify.sh` → `scripts/check.sh` | `AGENTS.md`, `scripts/check.sh`, `.github/workflows/ci.yml` |
| Rust pin consistency | `python3 scripts/rust-pins.py check` | `scripts/check.sh`, `scripts/release-contract.sh` |
| Rust formatting | `cargo fmt --all -- --check` | `AGENTS.md`, `scripts/check.sh` |
| Rust lint and type checking | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | `AGENTS.md`, `scripts/check.sh` |
| Rust tests | `cargo test --workspace --locked` | `AGENTS.md`, `scripts/check.sh` |
| Web unit tests | `npm run test:web-unit` | `package.json`, `scripts/check.sh` |
| Python tests | `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s contrib/library/tests -p 'test_*.py'` and the same command with `-s scripts/tests` | `scripts/check.sh` |
| Rust documentation | `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked` | `scripts/check.sh` |
| Build (optional, separate) | `cargo build --locked -p rusty-dlna` | `.github/workflows/ci.yml`, privileged SSDP job |
| Full web suite (additional CI check) | `npm run test:web` | `package.json`, `.github/workflows/ci.yml`, browser job |

Rust type checking is covered by Clippy; no separate typecheck task is declared.
No standalone JavaScript or Python lint/typecheck commands are declared; update
this reference if they are added. Python AST and shell syntax checks are already
part of `scripts/check.sh`. The embedded browser assets have no production
frontend build. Network, Docker, GPU, fuzz, soak, and scale checks follow the
scope rules in `AGENTS.md`.

## Runtime and CI prerequisites

Environment provisioning is separate from verification; the agent wrapper does
not install dependencies.

- Linux amd64 or arm64 is required by the server (`README.md`). The existing
  quality workflow uses Ubuntu 24.04; `scripts/check.sh` also uses GNU utilities.
- Rust **1.98.1**, edition 2021, with rustfmt and Clippy is declared in
  `rust-toolchain.toml` and `Cargo.toml`. CI installs it with
  `rustup toolchain install 1.98.1 --profile minimal --component rustfmt,clippy`
  and selects it with `rustup default 1.98.1`. Cargo test/doc/run commands in
  the quality gate use `--locked` and the workspace lockfile.
- Node.js **20 or newer** with npm is required by `AGENTS.md` and the gate;
  existing CI pins **22.19.0**. Web unit tests need no npm dependency install.
  The browser job uses `npm ci` and
  `npx playwright install --with-deps chromium firefox webkit` before
  `npm run test:web`.
- Python **3.10 or newer** is required by the library tools. No PyPI packages
  are needed (`contrib/library/requirements.txt`).
- The CI quality job installs `clang libavformat-dev ffmpeg curl` through apt;
  Python, shell tools, and GNU utilities must also be available. Operator tools
  use FFmpeg and FFprobe. Additional CI jobs declare their own dependencies.

The workflows are authoritative. The quality job
runs `scripts/check.sh` directly; it does not call `scripts/agent-verify.sh`.

## For agents

Canonical contract: [AGENTS.md](../AGENTS.md).
Verify from the repository root: `./scripts/agent-verify.sh`.
Report missing prerequisites, failures, and skipped checks explicitly.
Do not add dated implementation checklists or completed audit reports as
permanent documentation; put future work in an issue.
