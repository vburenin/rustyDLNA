# rustyDLNA contributor guide

This file applies to the entire repository. rustyDLNA is a standalone DLNA / UPnP
media server. Describe protocol behavior, object IDs, renderer handling, and
compatibility decisions as rustyDLNA's own contract.

## Project index

### Workspace crates

- `crates/protocol`: Stable wire constants and pure protocol behavior: renderer
  profiles, media formats, object IDs, paths, dates, HTTP persistence, SOAP
  names, and SSDP service types. Changes here can affect every client.
- `crates/ssdp`: SSDP packet parsing and generation. Socket ownership and
  interface lifecycle remain in the server crate.
- `crates/http`: HTTP request parsing, routing helpers, device/service XML,
  byte ranges, DLNA headers, and media response types.
- `crates/soap`: SOAP request parsing, ContentDirectory behavior, DIDL output,
  search/filter/sort handling, faults, and protocol-info output.
- `crates/scan`: Filesystem scanning and watching, root confinement, SQLite
  persistence, metadata probing, NFO, artwork, captions, playlists, inode and
  symlink alias handling, and the global media-helper gate.
- `crates/transcode`: Codec/HDR models, remap policy, FFmpeg argument builders,
  cache identities, browser quality profiles, and controlled helper execution.
  Policy belongs here; HTTP/job orchestration does not.
- `crates/server`: Configuration, listener lifecycle, catalog publication,
  SOAP/HTTP dispatch, GENA, status/metrics, remux jobs, and the embedded browser
  application. `src/main.rs` is only the CLI/bootstrap entry point.

### Embedded browser application

- `crates/server/web/index.html`: Accessible static structure.
- `crates/server/web/app.css`: Responsive and fullscreen presentation.
- `crates/server/web/app.js`: Application bootstrap and event wiring.
- `crates/server/web/api.js`: Typed same-origin API access and error mapping.
- `crates/server/web/core.js`: Pure capability, time, queue, and navigation
  helpers. Keep it browser-independent and cover it with `core.test.js`.
- `crates/server/web/library.js`: Library loading, paging, search, and history.
- `crates/server/web/player.js`: Playback session, source negotiation, controls,
  seeking, recovery, captions, tracks, and Media Session integration.
- `crates/server/web/preferences.js`: Browser-local settings and progress.
- `crates/server/web/store.js`: State transitions. Stale session events must not
  mutate a newer playback session.
- `web-tests/player.spec.js`: Playwright behavior, browser compatibility,
  responsiveness, and accessibility coverage.

The browser assets are embedded with `include_str!`; there is no production
frontend build or runtime asset directory.

### Configuration, deployment, and validation

- `rusty-dlna.toml`: Shipped defaults.
- `rusty-dlna.live.toml.example`, `.env.example`, and
  `docker-compose.override.yaml.example`: User-owned configuration templates.
- `docker-compose.yaml`: Live host-network deployment. SSDP multicast does not
  work through an ordinary bridge/NAT setup.
- `docker-compose.test.yaml`, `docker-compose.smoke.yaml`: Isolated test setups.
- `Dockerfile`: Release runtime and pinned media-tool dependencies.
- `scripts/check.sh`: Canonical local quality gate.
- `scripts/compose-smoke.sh`: Container-level scan, browse, media, and shutdown
  smoke coverage.
- `scripts/ssdp-netns-e2e.sh`, `scripts/host-network-e2e.sh`: Privileged network
  behavior tests.
- `scripts/helper-load.sh`, `scripts/soak.sh`, and
  `scripts/large-library-benchmark.sh`: Resource-bound and scale validation.
- `testdata/`: Checksum-locked media, metadata, protocol, and database fixtures.
- `fuzz/`: Parser fuzz targets, seeds, and promoted regressions.

### Authoritative documentation

- `README.md`: Product overview, supported use, and quick start.
- `docs/COMPATIBILITY.md`: Supported product surface and intentional exclusions.
- `docs/PROTOCOL_CONTRACT.md`: Subtle protocol and media-library invariants.
- `docs/WEB_PLAYER.md`: Browser UI, API, playback, and recovery behavior.
- `docs/TRANSCODE.md`: Remap policy, FFmpeg behavior, HDR, and GPU setup.
- `docs/OPERATIONS.md`: Storage, health, alerts, recovery, and native service use.
- `docs/DISTRIBUTION.md`: Release, reproducibility, and source obligations.
- `docs/LARGE_LIBRARY_BENCHMARK.md`: Reproducible scale workload and baseline.
- `CHANGELOG.md`, `LICENSE`, `THIRD_PARTY_NOTICES.md`, and `testdata/README.md`:
  release history and licensing requirements.

Do not add dated implementation checklists or completed audit reports as
permanent documentation. Put current behavior in the relevant guide and future
work in an issue.

## Coding rules

### General

- Preserve unrelated user changes. Inspect `git status` before editing and do
  not rewrite, discard, or commit changes outside the requested scope.
- Do not commit or push unless explicitly requested. Use focused commits with a
  descriptive imperative subject and a body when the change needs context.
- Keep media roots read-only. Never modify a user's media library or external
  storage as part of ordinary development or tests unless explicitly asked.
- Keep generated output out of Git: `target/`, `fuzz/target/`, `node_modules/`,
  Playwright reports, caches, databases, and local configuration stay untracked.
- Prefer the smallest change that owns the behavior at the right layer. Avoid
  duplicating protocol constants, codec tables, route parsing, or policy across
  crates.
- New dependencies must have a concrete need, a compatible license, bounded
  behavior on untrusted input, and corresponding lockfile/notice review.

### Rust

- Use the pinned Rust `1.97.1`, edition 2021, and the workspace dependency table.
- Code must pass formatting and Clippy with warnings denied. Avoid broad lint
  allowances; explain any narrow exception next to it.
- Prefer typed enums and explicit state transitions over stringly typed flags.
  Use checked or saturating arithmetic for media sizes, timestamps, offsets,
  quotas, and protocol values.
- Recover poisoned shared state through the repository's existing helpers when
  continued service is safe. Do not add panics to request, scanner, watcher, or
  background-job paths.
- Keep blocking filesystem, SQLite, probe, and FFmpeg work out of asynchronous
  listener tasks. Respect existing gates, queue bounds, cancellation tokens,
  deadlines, and shutdown budgets.
- Any new `unsafe` block requires a local safety explanation and focused tests.
  Do not widen inherited file descriptors or process-group scope.
- Preserve non-UTF-8 Unix paths by using `Path`, `OsStr`, and `OsString`; do not
  round-trip filesystem identity through lossy strings.

### Protocol and HTTP

- Treat object IDs, URL paths, SOAP action/fault codes, XML namespaces, service
  descriptions, protocol-info entries, renderer ordering, MIME mappings, and
  HTTP connection behavior as compatibility-sensitive.
- Put shared wire literals in `crates/protocol`; ensure scanner admission,
  DIDL/protocol-info advertisement, and HTTP serving agree on every media type.
- Escape all XML and HTML values. Keep parsers bounded and reject ambiguous
  framing, duplicate conflicting parameters, malformed ranges, and unsupported
  transfer encodings.
- Preserve the dotted-IPv4 host validation and callback/location confinement.
  Network input and renderer descriptions are untrusted.
- Do not change a renderer-specific workaround without a regression test naming
  the affected client and wire behavior.

### Scanner and database

- Open media and sidecars through the configured-root confinement helpers.
  Symlink retargeting must never allow reads outside allowed roots.
- A physical inode may have multiple browseable paths. Probe/artwork work is
  shared, metadata updates reach every alias, and deleting one alias must not
  delete a surviving path.
- Scanner publication must be deterministic and transactional. A failed or
  cancelled scan cannot expose a partial catalog or partially migrated schema.
- Watcher updates must converge with a full reconciliation. Cover create,
  replace, rename, delete, overflow, and sidecar-only changes.
- Keep fixture inputs deterministic and bounded. If fixture bytes intentionally
  change, update `testdata/SHA256SUMS`, `testdata/REQUIRED_FILES`, provenance,
  and the assertions together.

### Transcode and media jobs

- The default is the original stream. Remap rules match media traits and
  renderer software; never special-case a title or filesystem path.
- Negotiate video and audio independently. Copy every compatible stream and
  encode only what is required, unless malformed timestamps require explicit
  repair.
- Preserve HDR, bit depth, resolution, and bitrate when possible. Any quality
  loss must be an explicit profile/policy decision visible in stream details.
- Browser output is a growing fragmented MP4. Keep the initial fragment
  playable, timestamps monotonic, copied AAC normalized for MP4, HEVC tagged
  appropriately, and mixed copy/encode seeks aligned from their first packets.
- A change to FFmpeg arguments, stream selection, timestamp handling, or output
  semantics must update the transcode cache identity/revision when old output
  is no longer reusable.
- Pass media through validated file descriptors where established. Never build
  a shell command from media paths or request parameters.
- Every helper must use null stdin, bounded diagnostics, cancellation, a hard
  deadline, a dedicated process group where supported, and complete child
  reaping. A disconnected browser gets only the defined reconnect grace.
- Publish cache files atomically only after successful completion and probing.
  Failed, cancelled, stale, and over-quota intermediates must not be served.

### Browser UI

- Keep one custom control surface for direct and compatible playback, including
  fullscreen. Controls must remain keyboard-, mouse-, and touch-accessible.
- Do not hide focused controls. Preserve visible focus, 44-pixel touch targets,
  safe-area handling, reduced-motion behavior, and accurate ARIA state.
- Scope playback shortcuts to the player or fullscreen and never override
  browser/system shortcuts or editable controls.
- Every source load has a monotonically increasing session ID. Abort old fetches,
  timers, polls, and listeners; stale callbacks must be harmless.
- Preserve playback intent, global time, rate, volume, mute, selected tracks,
  captions, and loop state across compatible-source restarts where applicable.
- Capability APIs are advisory. Keep portable recovery for copied streams that
  the browser advertises but cannot actually decode.
- User messages use plain language and expose technical diagnostics separately.
  Do not leak absolute paths, raw helper commands, or unbounded stderr.
- Keep pure decisions in `core.js`/`store.js` and cover them with Node tests.
  Use Playwright for media events, focus, history, responsive layout, and
  browser-specific behavior; string-presence tests are not behavior tests.

### Configuration and documentation

- Configuration rejects unknown keys. New settings require validation, safe
  defaults, effective-config output, example updates, and positive/negative tests.
- Resolve relative paths against the selected configuration file, not the
  process working directory.
- Never commit `.env`, `rusty-dlna.live.toml`, or
  `docker-compose.override.yaml`.
- Update the relevant authoritative document in the same change when product,
  API, operational, transcode, or release behavior changes.
- rustyDLNA documentation should explain what the project does now. Avoid
  historical source-lineage framing and comparisons that do not help a user or
  maintainer operate the current implementation.

## Verification

Run the narrowest relevant checks while iterating, then expand according to
risk. The canonical full gate is:

```sh
scripts/check.sh
```

Useful focused commands:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --locked
cargo test -p rusty-dlna <test-name> --lib
npm run test:web-unit
npm run test:web-browser
```

Use `cargo test --workspace --lib --bins --tests` when the local toolchain lacks
`rustdoc`. Report that limitation; do not claim the documentation tests passed.

Tests must not bind the live defaults `8200/TCP` or `1900/UDP`. Use the isolated
ports/constants and set `RUSTY_DLNA_REQUIRE_E2E=1` only when intentionally
running the socket end-to-end suite. Run privileged namespace, Docker smoke,
GPU, soak, fuzz, and large-library checks only when the change affects those
surfaces or before a release.

Before handing off a change:

1. Run `git diff --check` and inspect the complete diff.
2. Run focused regression tests for the modified behavior.
3. Run the broadest feasible workspace/web gate and state anything unavailable.
4. Confirm tests did not alter tracked fixtures or create accidental artifacts.
5. Summarize behavior changed, validation performed, and whether changes are
   committed or pushed.
