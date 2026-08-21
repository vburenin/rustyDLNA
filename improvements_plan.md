# rustyDLNA implementation audit and improvement plan

Audit date: 2026-08-18
Audited rustyDLNA base commit: `ff5397a` plus the current remediation worktree

This is an evidence-based implementation inventory and release plan, not a list
of aspirations. Checked items were found in code and/or exercised during this
audit. Unchecked items are missing, incomplete, or need stronger proof. A parent
item should be checked only after every acceptance checkbox below it is checked.

## Release verdict

- [x] The project is a substantial working DLNA server, not a scaffold: scanning,
  SQLite persistence, SSDP, HTTP, SOAP/DIDL, GENA, media delivery, client quirks,
  and opt-in remux/transcode are all implemented.
- [x] The normal non-interactive quality gate passes: formatting, strict Clippy,
  385 workspace tests, documentation tests, command smoke tests, required socket
  E2E, and Compose isolation.
- [ ] **Release-ready.** Do not publish until all P0 items are closed. The current
  implementation blockers have been fixed. The remaining P0 evidence is a green
  execution of the final tagged digest on both `linux/amd64` and `linux/arm64`,
  plus a recorded partial-release rollback rehearsal. This host can execute amd64
  only; the release workflow now makes both runtime smokes mandatory before
  publication.

## Audit evidence

- [x] Read the current compatibility, distribution, protocol-contract,
  transcode, test, container, CI, release, and soak material.
- [x] Audited the Rust subsystems for configuration, discovery, HTTP, SOAP,
  eventing, scanning, metadata, playlists, monitoring, image handling, renderer
  behavior, and database/container generation.
- [x] Ran `./scripts/check.sh` non-interactively: all checks passed.
- [x] Ran `cargo audit --deny warnings`: 137 dependencies scanned with no known
  vulnerability finding.
- [x] Ran `cargo deny check`: advisories, bans, licenses, and sources passed. It
  reports duplicate-version warnings for `hashbrown`, `shlex`, and `syn`.
- [x] Ran `cargo llvm-cov --workspace --json --summary-only --fail-under-lines 80`:
  81.61% line, 79.12% region, and 80.24% function coverage after moving inline
  tests out of production files; the configured 80% line floor passes.
- [x] Ran `scripts/compose-smoke.sh` against the versioned release-local image:
  scan/probe, SOAP Browse, byte-identical original GET, thumbnail, resize, AC-3
  remux, helper tools, health, OCI version, and graceful stop passed.
- [x] Ran `scripts/ssdp-netns-e2e.sh` in two Linux network namespaces: multicast
  membership and subnet-correct reply source/`LOCATION` passed.
- [x] Inspected the live host-network container: it was healthy, ran as uid/gid
  10001 with all capabilities dropped and no-new-privileges, and reported a
  healthy SQLite database and active watcher.
- [x] Ran a fresh release build and inspected it with `ldd`; it dynamically links
  the runner's FFmpeg libraries and their large system dependency closure.
- [x] Confirmed from the official Rust release channel that 1.97.1 is the latest
  stable release for this audit. The project toolchain already pinned 1.97.1; the
  stale Cargo/README/CI claim of Rust 1.83 was removed during the audit.
- [x] Before remediation, reproduced `rusty-dlna --version` being rejected even
  though the distribution verification procedure required it.
- [x] Before remediation, reproduced the scanner test suite hanging when attached
  to a PTY because scanner FFmpeg/FFprobe commands inherited stdin.
- [x] Confirmed that the ignored local soak report is partial and belongs to an
  older commit: it contains 241 cycles over 2,282 seconds and no final pass record.

## Remediation verification

- [x] Rust 1.97.1 is exact and consistent in `rust-toolchain.toml`, Cargo
  `rust-version`, Docker, CI, release CI, README, and the release contract.
- [x] `./scripts/check.sh` passes with strict Clippy and 385 tests on the current
  worktree, including PTY, SOAP persistence, descriptor-race, overload, CLI, and
  socket E2E regressions.
- [x] `cargo audit --deny warnings`, `cargo deny check`, and the 80% coverage gate
  pass; only the documented duplicate-version warnings remain.
- [x] All seven parser fuzz targets pass their pinned ten-second smoke runs.
- [x] All seven tracked fuzz seeds replay under AddressSanitizer, the complete
  LeakSanitizer target matrix builds, and a LeakSanitizer seed replay passes.
- [x] `scripts/helper-load.sh` sends 24 distinct resize/remux requests and passes
  the configured process, thread, FD, RSS, cache, and latency ceilings while
  observing both HTTP 200 and bounded HTTP 503 overload outcomes.
- [x] `scripts/release-contract.sh v0.1.0` accepts the matching package/image
  version and OCI revision labels, while a mismatched `v0.1.1` is rejected.
- [x] The privileged two-namespace SSDP test passes with reply source and
  `LOCATION` matching each receiving interface.
- [x] Deleted obsolete design and checklist documents, copied sources, and local
  specification PDFs are not silently restored. Broken test/build references were replaced
  with self-contained executable compatibility contracts.

## Implemented functionality

### Workspace, runtime, and configuration

- [x] The code is separated into seven workspace crates: protocol, SSDP, SOAP,
  HTTP, scan/database, transcode policy, and the server binary.
- [x] One Tokio-based daemon owns HTTP, SSDP, GENA, scanner/watch workers, remux
  jobs, signals, and orderly shutdown instead of using a fork-per-request model.
- [x] TOML configuration uses strict unknown-field rejection, validation, and
  config-file-relative path resolution.
- [x] `--config`, `--check`, `--print-effective-config`, `rescan`, `database
  check`, and `database rebuild` operational paths are implemented and smoke-tested.
- [x] A stable random UUID is persisted below the cache directory when one is not
  configured; configured identities are validated.
- [x] Advertised/listen addresses and one or more interfaces can be selected, with
  ambiguity rejected rather than silently advertising an unusable address.
- [x] Multiple typed media roots, root identities, media masks, hidden-file policy,
  exclusion rules, artwork templates, scan worker bounds, deadlines, and recent
  item policy are configurable.
- [x] SIGINT/SIGTERM trigger shutdown, SSDP byebye, listener cleanup, watcher stop,
  and child-process cleanup.

### SSDP, discovery, and renderer identification

- [x] IPv4 SSDP alive/byebye and M-SEARCH responses cover root device, UUID,
  MediaServer device, ContentDirectory, ConnectionManager, and Microsoft registrar.
- [x] M-SEARCH parsing, `MAN`/`MX`/`ST` handling, response jitter, version matching,
  and malformed-packet rejection are tested.
- [x] Multi-interface sockets join and announce on each selected address and reply
  from the subnet-facing source with a matching HTTP `LOCATION`.
- [x] Renderer profiles define behavior for Kodi, Samsung,
  Xbox, Sony, Toshiba, Cast, and generic clients.
- [x] Client identity combines HTTP headers, SSDP data, address specificity, ARP
  information, TTL expiry, and cached renderer descriptions.
- [x] Renderer-description fetching is protected by subnet policy, body/deadline
  caps, concurrency limits, rate limiting, and duplicate suppression.

### HTTP and DLNA transport

- [x] The server has bounded connection counts, request/header/body limits,
  keep-alive request caps, header/body/write timeouts, and strict HTTP/1.0/1.1 parsing.
- [x] Malformed request lines/headers, conflicting content lengths, unsupported
  transfer encodings, and request-smuggling forms are rejected.
- [x] Leftover pipelined bytes are retained between requests.
- [x] The dotted-IPv4 `Host` policy and port validation are implemented.
- [x] Root/device descriptions and all three SCPDs are generated, XML-escaped,
  parse-tested, and served with real PNG/JPEG icons.
- [x] Original media supports GET, HEAD, full-body delivery, single byte ranges,
  206/416 semantics, large-file streaming, and DLNA response headers.
- [x] Media and transcode resources intentionally close the connection according
  to the rustyDLNA contract for `/MediaItems/`.
- [x] Album art, captions, thumbnails/resized JPEGs, presentation/status HTML,
  `/api/status`, and `/health` routes exist.
- [x] DLNA request checks cover time-seek/play-speed without ranges, transfer mode,
  real-time/interactive combinations, and client-specific exceptions.

### SOAP, DIDL, virtual containers, and eventing

- [x] SOAP action/header parsing, XML argument parsing, invalid-action/argument/
  object/search/sort faults, and a 2 MiB response ceiling are implemented.
- [x] ContentDirectory implements Browse metadata/children, Search, filtering,
  sorting, pagination, Search/Sort capabilities, and SystemUpdateID.
- [x] ConnectionManager protocol info/current connection actions and the Microsoft
  MediaReceiverRegistrar actions are implemented.
- [x] Samsung feature-list/bookmark behavior and Kodi `UpdateObject`, resume
  position, and play-count persistence are implemented.
- [x] DIDL includes client-specific class/MIME quirks, protocolInfo, dates,
  metadata, reference IDs, subtitles, artwork, resource dimensions/duration, and
  XML-safe escaping.
- [x] Stable rustyDLNA object IDs and virtual views exist for music,
  video, pictures, folders, playlists, recent items, artists, albums, genres,
  composers, contributors, ratings, cameras, series, seasons, and actors.
- [x] GENA subscription/renewal/unsubscription, UUID SIDs, bounded timeouts,
  asynchronous notification workers, queue limits, failure pruning, and metrics
  are implemented.

### Scanner, metadata, database, and filesystem monitoring

- [x] SQLite schema creation/migration, foreign keys, constraints, transactions,
  a connection pool, `quick_check`, corruption backup/recovery, and rebuild tooling
  are implemented.
- [x] Scan, Browse/Search, and HTTP delivery use a shared media format table rather
  than unrelated extension lists.
- [x] Media probing covers the declared audio/video formats; tagged FLAC/MP3,
  JPEG EXIF, NFO XML, playlists, subtitles, and folder/sidecar artwork are parsed.
- [x] NFO and playlist parsing are bounded and reject paths outside allowed roots.
- [x] JPEG metadata includes orientation, dates, camera, rating, and album; derived
  images account for EXIF orientation.
- [x] Hard links and symlink aliases are grouped by device/inode so probing and
  generated artwork can be reused while folder aliases retain stable identities.
- [x] Non-UTF-8 Unix paths have reversible database storage instead of lossy
  identity conversion.
- [x] Hidden entries, exclusions, wide-link policy, samples, unfinished files,
  junk, sidecars, and generated artifacts have explicit admission rules.
- [x] Scan preparation is parallel and bounded by `scan_workers`; SQLite publication
  remains single-threaded and deterministic.
- [x] Linux inotify events are batched and reconciled, including create/change,
  rename, delete, overflow, sidecar, artwork, subtitle, and playlist changes.
- [x] Bookmark retention and periodic full reconciliation are implemented.

### Remap, remux, and transcode

- [x] Transcoding is opt-in. The original resource remains the default and ordered
  remap rules match both source metadata and renderer profile.
- [x] Configuration validates container/codec/audio choices instead of passing
  arbitrary encoder strings directly to FFmpeg.
- [x] Encoded output uses a growing disk-backed fragmented-MP4 job cache with
  shared readers, stable source/plan identities, and range-aware reads.
- [x] `remux-p8` integrates `dovi_tool` when available and has an explicitly
  documented HDR10 fallback.
- [x] FFmpeg/FFprobe/dovi processes have deadlines, bounded stderr, cancellation,
  reaping, exit validation, and output probing in the remux implementation.
- [x] Cache admission/cleanup covers age, quota, minimum-free-space, invalidation,
  and disconnect policy.

### Security, operations, delivery, and project hygiene

- [x] The production image starts directly as uid/gid 10001, drops all Linux
  capabilities, enables no-new-privileges, and uses a read-only media mount model.
- [x] Production Compose uses host networking for SSDP; test/smoke Compose is an
  isolated bridge and does not publish production ports.
- [x] Base images/actions/toolchains are pinned; Debian packages use a snapshot;
  dovi_tool archives have checksums; license and notice files are shipped.
- [x] CI includes formatting/Clippy/tests/docs/E2E, pinned-stable Docker smoke, Trivy,
  audit/deny, coverage, and bounded parser fuzz jobs.
- [x] The release workflow builds an SBOM, checksums and provenance, publishes a
  multi-architecture image, scans it, signs its digest, and creates a draft release.
- [x] Fixtures and oracle material are checksum-locked, and fuzz targets cover
  HTTP, ranges, SSDP, SOAP XML, NFO, URL IDs, and sidecars.
- [x] Protocol behavior, supported scope, deployment, distribution, and
  transcode limitations are documented in the remaining project files.

## P0 — must fix before a public release

### P0-01: Add trustworthy program and artifact versioning

- [x] Enable Clap version metadata so `rusty-dlna --version` exits successfully and
  reports the package version plus, if desired, short VCS revision/build version.
- [x] Validate that a `vX.Y.Z` tag equals the Cargo package version; reject arbitrary
  `v*` strings before any image or release mutation.
- [x] Add tests for binary/image `--version`, OCI version/revision labels, and the
  release tag so they cannot disagree.
- [x] Update the current `0.1.0` version and changelog intentionally before the first
  release rather than deriving a misleading version only in image metadata.

### P0-02: Fix the non-portable standalone Linux archive

- [x] Choose and document one supportable artifact contract: container-only;
  a self-contained/static or bundled FFmpeg build with reviewed licensing; or a
  DEB/RPM-style package declaring exact shared-library dependencies. Do not publish
  the current bare runner-linked ELF as a generic `linux-x86_64` binary.
- [x] Remove the unused high-level `ffmpeg-next` dependency from `crates/scan` and
  configure `ffmpeg-sys-next` with the minimum required features. Reinspect `ldd`,
  SBOM, image contents, and licenses after narrowing the dependency graph.
- [x] Test the chosen artifact in a clean target environment that contains only its
  documented runtime prerequisites; exercise scan/probe, Browse, original GET,
  thumbnail generation, and remux, not merely process startup.
- [x] Identify OS/architecture through the OCI manifest and sign/attest the tested
  multi-architecture digest; no unsupported bare ELF archive is published.

### P0-03: Make releases transactional and quality-gated

- [x] Refactor the tag workflow so formatting/Clippy/tests/docs, pinned stable Rust,
  dependency policy, artifact smoke, container smoke, and tag validation finish
  before pushing mutable tags, creating attestations, or publishing a release.
- [x] Build once per artifact and promote the tested digest; avoid independently
  rebuilding something different after validation.
- [x] Do not push broad major/minor image tags until the digest has passed Trivy and
  runtime smoke. Stage by immutable SHA/tag digest first, then promote and sign.
- [ ] Record a successful tagged-workflow runtime smoke of both `linux/amd64` and
  `linux/arm64`, including FFmpeg/dovi_tool, scan, Browse, original media, image
  derivation, remux, and shutdown. The workflow enforces this; amd64 passes locally,
  but this host exposes no arm64/QEMU runtime.
- [x] Document failure/rollback behavior for already-pushed immutable staging
  images, unpromoted release tags, and draft GitHub releases.
- [ ] Rehearse the documented failure/rollback procedure against a disposable
  registry/repository after an immutable staging digest has been pushed. Verify no
  release/minor tags are promoted, the draft remains unpublished, and the staging
  digest can be retained for forensics or explicitly removed.

### P0-04: Prevent FFmpeg/FFprobe from reading daemon or test stdin

- [x] Centralize scanner external-command construction and always set
  `.stdin(Stdio::null())`; add FFmpeg/FFprobe `-nostdin` where supported. Cover
  probing, thumbnails, filmstrips, artwork, and test fixture helpers.
- [x] Audit configuration-validation and server helper invocations for the same
  inherited-stdin problem; apply one shared rule to every noninteractive child.
- [x] Add a PTY regression test that keeps terminal stdin open and proves scan/test
  completion within the configured deadline. Preserve the existing non-TTY tests.

### P0-05: Restore SOAP keep-alive compatibility

- [x] Stop treating `HttpRoute::Soap` like media in `persist_for_route` and the
  server's response post-processing. Successful SOAP should follow the HTTP/1.0
  and HTTP/1.1 persist rule, the 100-request cap, `Connection` tokens, and the
  defined chunked-request close rule.
- [x] Determine fault persistence from rustyDLNA's protocol contract and encode it explicitly;
  do not let the current unconditional SOAP close hide whether individual fault
  constructors have the right behavior.
- [x] Add one-socket tests with two pipelined/sequential SOAP requests, HTTP/1.0
  keep-alive, HTTP/1.1 default persistence, explicit close, chunked input, success,
  and representative 401/402/701 faults.
- [x] Keep `/MediaItems/` and `/Transcode/` nonpersistent and verify that restoring
  SOAP persistence does not change streaming-resource behavior.

### P0-06: Remove check-then-open path races

- [x] Replace canonicalize/check followed by a later pathname open with a rooted,
  descriptor-based open. On Linux prefer `openat2` with `RESOLVE_BENEATH` and
  `RESOLVE_NO_MAGICLINKS`, or an equally strong component walk plus `fstat`.
- [x] Pass the validated open file handle through metadata, range, caption/artwork,
  resized-image, probe, and streaming code. Generate `Content-Length`/range data
  from that same handle so replacement/truncation cannot desynchronize headers and
  bytes.
- [x] Enforce sidecar and small-file limits while reading (for example `take(limit
  + 1)`), not only from pre-open metadata, so a growing/replaced file cannot bypass
  the cap.
- [x] Add adversarial tests that repeatedly retarget a symlink between an allowed
  media file and an outside secret during GET, scan, caption/artwork, and resize.
  No outside byte or metadata may be returned or persisted.
- [x] Define and test the fallback on platforms without `openat2`, or explicitly
  declare Linux as the only supported native runtime.

### P0-07: Bound all expensive media-helper work globally

- [x] Add a daemon-wide semaphore/queue for FFmpeg, FFprobe, image derivation, and
  related helpers. `scan_workers` only bounds scan preparation; many unique
  `/Resized/` or probe requests can otherwise consume up to the HTTP connection cap.
- [x] Reserve/cache-check storage before spawning work and reject overload with a
  stable HTTP outcome rather than accumulating processes and partial cache files.
- [x] Export active/queued/rejected/timed-out helper metrics and test fairness so a
  thumbnail storm cannot starve scan, health, SOAP, or an already-running remux.
- [x] Add a load test using many distinct resize/remux keys and assert process,
  thread, FD, RSS, cache, and latency ceilings.

## P1 — high-priority quality and scale work

### P1-01: Prove and improve large-library behavior

- [x] Add reproducible benchmarks at 50k physical files with hard-link and
  symlink aliases. Record cold scan time, reconcile time, Browse/Search p50/p95/p99,
  SQLite size, RSS, CPU, open FDs, and update latency.
- [x] Remove full-library cloning/replacement from request and publish paths. Prefer
  SQL-primary pagination and incremental catalog deltas, with bounded cached
  projections for hot virtual views.
- [x] Stream scan discovery/preparation into a bounded ordered publisher instead of
  retaining multiple full file/group/prepared collections at once.
- [x] Make full-reconcile cadence adaptive/configurable for large roots and retain
  targeted inotify updates without walking every root unnecessarily.
- [x] Clarify physical inode, path alias, media record, container, and total object
  counts in status output; current live counts can otherwise look contradictory.

Reference evidence is recorded in `docs/LARGE_LIBRARY_BENCHMARK.md`. On the
2026-08-19 50k-inode/10k-alias run, steady reconciliation took 11.590 seconds,
Browse/Search p95 were 0.411/0.425 ms, targeted write-to-visible latency was
719.655 ms, peak RSS was 490.125 MiB, SQLite used 115.853 MiB, and the daemon
held 21 file descriptors.

### P1-02: Make scan/watch shutdown reliably bounded

- [x] Propagate a cancellation token into directory walking, metadata preparation,
  libav, FFmpeg/FFprobe waits, and SQLite publication. A watcher join must not wait
  for the full helper deadline during daemon shutdown.
- [x] Define a graceful-stop budget, send terminate then kill/reap at its deadline,
  checkpoint or discard partial scan state transactionally, and still send byebye.
- [x] Add SIGTERM tests while a deliberately stuck probe, thumbnail, database
  transaction, inotify overflow reconcile, and remux are active; assert no child,
  temp file, lock, or corrupt transaction survives.

### P1-03: Strengthen health and observability semantics

- [x] Make listener health reflect actual accept-loop/task state rather than a
  hard-coded boolean; include scanner/watch, notification workers, helper queue,
  remux supervisor, DB pool, and last successful reconcile freshness.
- [x] Avoid running an uncached SQLite `quick_check` on every orchestrator health
  poll. Run it at startup/periodically or cache a bounded check result and expose
  its age.
- [x] Decide whether `degraded` should return 200 or 503 and align the Docker
  healthcheck, documentation, alerting, and restart behavior with that decision.
- [x] Add structured counters/histograms for HTTP routes/status, SOAP actions/faults,
  Browse/Search latency, scan backlog, dropped inotify events, GENA failures,
  helper saturation, transcode cache, and shutdown duration.

### P1-04: Add genuine advanced-media fixtures

- [x] Replace the synthetic Dolby Vision probe sidecar/copy fixture with a tiny,
  redistributable genuine DV Profile 7 sample, or generate one reproducibly in CI
  from legally usable sources.
- [x] Prove both dovi_tool conversion and HDR10 fallback end-to-end by probing the
  produced fMP4 and checking codec/profile/color/DV metadata and playable fragments.
- [x] Add real TrueHD/audio-remap, HDR10 mastering metadata, truncated/corrupt,
  oversized metadata, and unusual stream-layout fixtures.
- [x] Either implement mastering-display/MaxCLL preservation or keep the current
  limitation prominent in generated resource policy and user documentation.

The tracked `dvp7.mkv` is now deterministically rebuilt from checksum-pinned
MIT-licensed `dovi_tool` assets and contains genuine Profile 7 MEL BL+EL+RPU
plus TrueHD, with no probe sidecar. Docker CI byte-compares a regeneration.
The production-image smoke probes and decodes both the signaled Profile 8
fMP4/RPU path and the no-dovi_tool High-10 HDR10 fallback. Generated lavfi
fixtures cover mastering-display/MaxCLL, oversized metadata, malformed inputs,
and unusual stream ordinals; exact mastering metadata preservation remains an
explicitly documented non-claim.

### P1-05: Put privileged and architecture-specific behavior in CI

- [x] Run the two-network-namespace SSDP test in an isolated privileged CI job and
  retain packet/assertion diagnostics on failure.
- [x] Add real or emulated arm64 runtime smoke for the final image/artifact, including
  scan, libav ABI loading, FFmpeg, dovi_tool, HTTP, and graceful shutdown.
- [x] Exercise host-network discovery in a disposable CI VM where feasible; keep it
  isolated from port 1900/8200 on any real deployment.

The dedicated `privileged-ssdp-netns` job runs the two-interface namespace
test and uploads packet, address, socket, daemon, and probe diagnostics on
failure. The Docker smoke job uses alternate ports on its disposable runner and
uploads equivalent host-network diagnostics. Both scripts also pass locally;
the host-network test was verified with a freshly built current image on a NIC
with two addresses, with reply source and `LOCATION` selecting the configured
primary address.

### P1-06: Improve source and dependency hygiene

- [x] Automate detection of new stable Rust releases and open a single update that
  changes `rust-toolchain.toml`, `rust-version`, CI/release builders, and docs
  together. Keep builds exactly pinned while ensuring the pin does not become stale.
- [x] Rewrite `scripts/fixture-exif.rs` as ordinary UTF-8 text with escaped byte
  literals; it currently contains two literal NUL bytes and is detected as `data`.
- [x] Make fixture setup fail if tracked fixtures are absent instead of creating or
  rewriting files in `testdata/library`; tests should mutate only temporary copies.
- [x] Add an unused-dependency check (`cargo machete` or equivalent) and keep
  FFmpeg features minimal. Review whether duplicate `hashbrown`, `shlex`, and `syn`
  versions can be consolidated without forcing risky upgrades.
- [x] Split the former monolithic scan and server libraries
  into bounded modules around DB, catalog, metadata, HTTP application, GENA, and
  lifecycle ownership; enable `missing_docs` selectively for public APIs.
- [x] Raise targeted coverage in `main`, scan/watch, config, catalog query, and remux
  failure/cancellation paths rather than relying only on the aggregate 80% floor.
- [x] Run longer scheduled fuzzing with persisted corpora, sanitizer variants, crash
  artifact retention, minimization, and regression promotion; ten seconds per target
  is only a build/smoke check.

The scan root is now 5,421 lines, with catalog (916), metadata (206), DB (3,702),
probe (1,588), watch (1,036), NFO, playlist, and a separate 4,308-line test module.
The server root is 684 lines, with HTTP application (2,660), lifecycle (1,551),
GENA (852), config, catalog query, metrics, status, remux, and a separate 4,944-line
test module. `missing_docs` is enabled in the new catalog, metadata, HTTP application,
and lifecycle modules; documented exported APIs and explicit legacy-surface exceptions
keep the strict warning gate actionable.

The coverage job now emits LLVM JSON and enforces independent line/function floors
for `main`, scan/watch, config, catalog query, and remux. The measured line/function
coverage is 85.27/62.50%, 65.18/66.67%, 73.14/73.61%, 72.04/86.05%, and
74.54/80.91%, respectively; new CLI lifecycle tests raised `main` from 28.68% to
85.27% and config from 69.59% to 73.14%.

`.github/workflows/fuzz-long.yml` runs every target for ten minutes each week under
both address and leak sanitizers. Tracked seeds initialize a rolling per-target
corpus cache, promoted regressions replay before mutation, failures retain logs and
crashes for 30 days, and cargo-fuzz minimizes crashes before upload.
`scripts/promote-fuzz-regression.sh` minimizes a retained input and installs it under
`fuzz/regressions/<target>/<sha256>` for review and commit with the fix.

### P1-07: Remove avoidable operational scaling costs

- [x] Replace unconditional recursive cache-volume `chown -R` during every restart
  with an initialization marker or ownership mismatch scan; measure startup on a
  large transcode/artwork cache.
- [x] Document cache/database capacity planning and alert thresholds for a large
  deployment, including cleanup failure and minimum-free-space behavior.
- [x] Add a native Linux service example with `User`, `Group`, `NoNewPrivileges`,
  filesystem protections, limits, restart policy, and writable cache/database paths.

## P2 — compatibility expansion and lower-priority work

These are not regressions against the current documented product contract. They
should remain unchecked until deliberately accepted, designed, and tested.

- [ ] **Additional library image formats:** design decoder/resource limits and add
  PNG, WebP, and HEIF/HEIC only with one consistent scan/DIDL/protocol-info/GET
  table, derivatives, malformed-image tests, and representative renderer proof.
- [ ] **IPv6:** specify dual-stack identity, multicast membership, interface scope,
  bracketed Host/`LOCATION`, callback validation, firewall guidance, and dual-stack
  integration tests before accepting IPv6 configuration.
- [ ] **TiVo support:** implement beacon/discovery, TiVo HTTP/query behavior, schema
  fields, capability advertisement, and real-client tests, or continue rejecting
  the unsupported keys explicitly.
- [ ] **Avahi/mDNS:** add only if a concrete non-SSDP consumer requires it; keep it
  optional and prove it cannot advertise inconsistent identities/addresses.
- [ ] **MiniSSDPd integration:** define ownership/failure semantics and test both
  direct-socket and daemon-mediated discovery before exposing a config option.
- [ ] **BSD/macOS native monitoring:** implement kqueue or a tested portable watcher,
  replace Linux `/proc`/inotify assumptions, and publish a platform support matrix.
- [ ] **Time-based seek/trick play:** implement DLNA time-seek/play-speed only with
  precise range mapping, codec/container support rules, and real renderer tests;
  the current explicit 406 behavior is safer than partial support.
- [ ] **Upload/import:** remains outside the current product contract. If ever
  added, require authentication, quotas, atomic writes, filename
  policy, malware/content validation, and rescan integration as a separate design.

## Explicitly accepted current scope

- [x] Linux + IPv4 SSDP/HTTP is the current deployment target; unsupported address
  configuration is rejected instead of partially working.
- [x] JPEG is the supported picture-library format; PNG/WebP/HEIF are not
  silently admitted by extension.
- [x] TiVo, Avahi/mDNS, and MiniSSDPd features are omitted and their configuration
  keys/capabilities are not falsely advertised.
- [x] Media roots are read-only inputs; filesystem upload/import is out of scope.
- [x] Privilege selection belongs to the container/service manager; the daemon does
  not start privileged and then implement its own user switch.
- [x] Logging goes to stderr with `RUST_LOG`; rotation belongs to the runtime.
- [x] Exact Dolby Vision dual-layer preservation and exact mastering-display/MaxCLL
  preservation are not claimed by the current transcode contract.

## Definition of high-quality completion

- [ ] Every P0 parent and acceptance checkbox is closed on the intended release
  commit, and no documented command is known to fail.
- [ ] The normal, PTY, pinned-stable, dependency, coverage, fuzz-smoke, socket E2E,
  multi-interface, artifact, amd64/arm64 image, and security gates are green.
- [ ] A clean-machine user can install the advertised artifact, run `--version`,
  scan the representative library, discover it, Browse/Search it, stream/range a
  file, restart it without identity loss, and follow documented recovery steps.
- [ ] The differential suite has no unexplained wire-contract regressions, and
  every deliberate difference is in the compatibility contract.
- [ ] A 24-hour long-lived-daemon soak and a separate restart/recovery soak pass with
  bounded resources on the exact release commit.
- [ ] Release artifacts are version-consistent, reproducible enough to audit,
  dependency-complete, SBOM-covered, vulnerability-scanned, attested/signed, and
  promoted only after their tested immutable digests pass.
