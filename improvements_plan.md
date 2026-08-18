# rustyDLNA implementation audit and improvement plan

Audit date: 2026-08-18.

This audit compares the current rustyDLNA working tree with the MiniDLNA
1.3.3-derived reference in `/home/vlad/workspace/minidlna`, the locked wire
notes in `replica.md`, the project documentation, and the tests. The working
tree already contained uncommitted protocol, scanner, SOAP, SSDP, and E2E
changes; those changes were inspected as part of the current implementation
and were not reverted.

Checkbox meaning:

- [x] Present in the current source and/or demonstrated by a passing test.
- [ ] Missing, incomplete, misleading, or not yet demonstrated at the stated
      quality level.

The project has a broad, working implementation, but it should not yet be
called production-quality. Complete all P0 items and the relevant P1 parity
items before a stable release.

## Verification baseline

- [x] `cargo test --workspace` passes when Cargo, rustdoc, and the other tools
      are taken from the selected Rust toolchain: 231 unit/integration test
      functions passed, with no failed doctests.
- [x] The seven conditional socket E2E tests execute and pass with
      `RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900` and
      `--test-threads=1`; the default workspace run otherwise reports these
      tests as successful after their early-return skip path.
- [x] `cargo run -p rusty-dlna -- --check` exits successfully for the default
      configuration.
- [x] `scripts/assert-isolation.sh` passes: the test compose project has no
      host network and publishes neither live port 8200 nor 1900.
- [x] `docker compose -f docker-compose.yaml config` parses with the local
      environment.
- [ ] `cargo fmt --all -- --check` fails on 23 Rust files with the current
      stable toolchain. Apply `cargo fmt --all`, review the mechanical diff,
      and make this a required CI gate.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
      fails. The first strict failure is `needless_lifetimes` in
      `crates/protocol/src/ssdp.rs`; a non-strict run also reports production
      and test warnings including dead code, overly complex tuple APIs, large
      enum variants, needless matches, and avoidable allocations. Resolve or
      narrowly justify every warning.
- [ ] The normal test output is not clean: deliberately invalid media fixtures
      cause many libav/ffmpeg errors, and tests have accumulated roughly 960
      ignored directories under `testdata/cache`. Replace fake codec fixtures
      where probing is expected, capture expected child-process stderr, and use
      RAII temporary directories outside the repository.
- [ ] No automated CI workflow currently enforces format, lint, tests, real
      socket E2E, container build, or dependency policy.

## Implemented inventory

### Project structure and runtime

- [x] The repository is a seven-crate Rust workspace with clear protocol,
      SSDP, SOAP, HTTP, scanner/database, transcoding, and server boundaries.
- [x] A foreground `rusty-dlna` binary loads TOML, supports `--config`,
      `--port`, and `--check`, initializes structured `tracing`, and creates a
      multithreaded Tokio runtime.
- [x] TCP HTTP and UDP SSDP listeners are wired into one process, with SIGINT
      and SIGTERM handling and SSDP byebye emission on the normal shutdown
      path.
- [x] Library loading starts from SQLite and scanning/inotify reconciliation
      runs on background OS threads rather than delaying the initial bind.
- [x] HTTP and SSDP test ports are isolated from the production 8200/1900
      ports, with a dedicated bridge-only test compose file.

### SSDP and client identification

- [x] Six MiniDLNA-compatible service/device variants are generated for alive,
      byebye, and `ssdp:all` replies, including the intentionally different
      header spacing documented in `replica.md`.
- [x] M-SEARCH parsing validates HTTP/1.1, quoted `MAN`, `MX`, and non-empty
      `ST`; specific known service requests return one reply.
- [x] Alive announcements are duplicated with jitter, periodic announcements
      are scheduled, and byebye packets omit `LOCATION`.
- [x] Configured interface names or IPv4 addresses are resolved and used for
      multicast group membership.
- [x] The MiniDLNA-derived ordered client matrix and client flags cover Kodi,
      Samsung variants, Xbox, Sony, Toshiba, Cast/CrKey, generic DLNA, and
      related MIME/title quirks.
- [x] A 25-entry IPv4 client cache retains more-specific identification over a
      later generic user agent and implements the inherited expiry behavior.
- [x] Selected renderer NOTIFY packets can pre-populate client identity by
      fetching a renderer description.

### HTTP, descriptions, and media resources

- [x] Root device XML advertises MediaServer:1, ContentDirectory,
      ConnectionManager, the Microsoft registrar, presentation URL, icons,
      Samsung DCM10 fields, and Xbox-specific identity changes.
- [x] ContentDirectory, ConnectionManager, and registrar SCPD documents expose
      the actions and state variables required by the implemented handlers.
- [x] The server routes root/presentation/status pages, descriptions, SOAP,
      GENA, original media, transcode media, album art, thumbnails, resized
      images, captions, and icons.
- [x] The inherited dotted-IPv4 Host policy, DLNA header validation,
      TimeSeek/PlaySpeed checks, image transfer-mode checks, and route-specific
      persistence rules are implemented.
- [x] Original media supports full GET and one byte range, returns 400/416 for
      invalid or unsatisfiable ranges, advertises `Accept-Ranges`, and streams
      responses larger than 8 MiB with Tokio file I/O.
- [x] Sidecar reads are canonicalized beneath media/cache roots and capped at
      16 MiB.
- [x] Album-art IDs and URLs, Xbox `?albumArt=true`, thumbnail lookup,
      on-demand JPEG resize, DLNA JPEG profiles, and caption URLs/MIME types
      are implemented.

### SOAP, DIDL, compatibility behavior, and events

- [x] SOAP action dispatch covers Browse, Search, capability/update queries,
      ConnectionManager queries, registrar actions, QueryStateVariable,
      `X_GetFeatureList`, `X_SetBookmark`, and `UpdateObject`.
- [x] BrowseMetadata and BrowseDirectChildren work for physical and virtual
      containers, including pagination, default ordering, selected sort keys,
      magic Samsung/Xbox IDs, reference IDs, child counts, and update IDs.
- [x] Search currently supports `contains`, `=`, `derivedfrom`, `exists`, AND,
      OR, and a defined property subset; invalid DLNA sort criteria return 709.
- [x] DIDL emission escapes catalog data and includes titles, dates,
      descriptions, creator/artist/actor/album/genre, track/episode fields,
      resources, artwork, captions, bookmarks, play count, and vendor fields
      when the corresponding implementation path supplies them.
- [x] Client-specific resource MIME remaps, title hacks, DLNA profile omission,
      caption behavior, extra CI=1 resources, and transcode-first ordering are
      represented in handler tests.
- [x] Bookmarks and play counts persist in SQLite and are exposed through
      Samsung/Kodi-compatible actions and DIDL fields.
- [x] GENA supports subscribe, renew, unsubscribe, peer-IP callback checks,
      subscriber expiry, initial notifications, and ContentDirectory update
      notifications with sequence numbers.

### Scanner, metadata, database, and virtual library

- [x] Media extension recognition, junk/sample/unfinished filters, hidden-file
      skipping, directory exclusions, NFO/caption/artwork sidecars, optical-disc
      structure avoidance, hard-link reuse, and directory-symlink alias views
      are present.
- [x] libav probing records container, video/audio/subtitle codecs, duration,
      bitrate, resolution, sample rate, channels, HDR/Dolby Vision information,
      and selected embedded artwork.
- [x] NFO handling reads movie/episode/show title, plot, genre, studio/creator,
      show/season/episode, and inherited `tvshow.nfo` metadata with a 64 KiB
      input cap.
- [x] Sidecar and embedded artwork can be persisted/reused, thumbnails can be
      generated, and inode aliases copy stream, NFO, caption, and artwork data.
- [x] SQLite stores objects, details, album art, captions, bookmarks,
      playlists (schema only), settings, and additional stream columns, using
      WAL and a busy timeout.
- [x] Stable MiniDLNA-style object roots and Browse Folders, Music, Video,
      Pictures, All, Recently Added, Series, Genre, Actor, Artist, Album, Date,
      Camera, rating, composer, and playlist container IDs are seeded.
- [x] Video Series/Season and Genre aliases are populated from NFO data, and
      audio/image virtual aliases are populated when their required fields are
      present.
- [x] Startup repair, inotify batching, rename/delete handling, overflow
      reconciliation, periodic reconciliation, and persisted SystemUpdateID
      updates are implemented and tested for multiple filesystem scenarios.

### Transcode and remux

- [x] Ordered `[[remap]]` rules match client/software and source traits and
      select original, Profile-8 remux, HDR10 encode, or audio conversion.
- [x] Source metadata is translated into the transcode decision model, and
      ffmpeg argument builders cover fragmented MP4, HDR color flags, audio
      mapping, and finished-file output.
- [x] A job gate limits concurrent titles and concurrent GETs can attach to one
      background growing-file job.
- [x] Growing `.part` output is served after an initial fragment, finished
      output supports normal byte ranges, and source size/mtime stamps provide
      basic invalidation.
- [x] `remux-p8` has a dovi_tool pipeline and falls back to HDR10 encoding when
      conversion is unavailable or fails.
- [x] No remap defaults to serving the original resource, and DIDL only adds a
      transcode resource after a matching decision.

### Packaging and documentation

- [x] A multistage Debian Dockerfile builds the release binary and installs
      runtime ffmpeg dependencies.
- [x] Production compose uses host networking for SSDP while test compose is
      isolated.
- [x] `README.md`, architecture, inherited behavior, transcode, protocol
      specification, oracle snippets, and a phase checklist document the
      intended system and test isolation rules.
- [x] The GPL-2.0-only license is declared in Cargo metadata and a LICENSE file
      is present.

## P0 — release blockers and correctness/security fixes

### P0-01: make default identity and network advertisement safe

- [ ] Replace the shared fallback UUID
      `uuid:00000000-0000-4000-8000-000000000001` with a generated UUID that
      is persisted in the database/cache on first run; validate configured
      UUID syntax and normalize a missing `uuid:` prefix in one place.
- [ ] When listening on `0.0.0.0` and no `advertise_ip` is supplied, select the
      usable IPv4 address for the configured interface or route instead of
      advertising `127.0.0.1`; fail startup with a clear message if the choice
      is ambiguous.
- [ ] Validate that `advertise_ip` is assigned to an enabled interface and is
      neither unspecified nor loopback for a live port-1900 deployment, while
      retaining an explicit test-mode exception for loopback ports.
- [ ] Add startup tests for persisted UUID reuse, two independent cache dirs,
      interface selection, an invalid interface, and the no-LAN-address error.
- [ ] Update `--check` so it runs the same effective-config/network validation
      as startup instead of testing hard-coded loopback values.

### P0-02: honor the transcode configuration contract

- [ ] Gate all transcode DIDL resources and `/Transcode/` jobs on
      `transcode.enable`; when disabled, matching remap rows must still serve
      only the original and must not start external processes.
- [ ] Apply `transcode.encoder` as the default encoder when a remap does not set
      one, or remove the setting if per-rule encoders are the only supported
      contract. Ensure docs, config examples, status, and code say the same
      thing.
- [ ] Preserve each selected plan's `audio_out` in the Profile-8 pipeline;
      remove the current hard-coded `ToAac` replacement.
- [ ] Validate action/encoder/audio combinations and required binaries during
      `--check`, including actionable errors for ffmpeg, ffprobe, dovi_tool,
      hardware encoders, and unsupported copy/codec combinations.
- [ ] Add handler/E2E tests proving enable=false blocks transcode, the global
      default encoder is used, per-rule override wins, and Profile-8 honors
      copy/AC-3/AAC choices.

### P0-03: fix remux job identity, cleanup, and stale-cache reuse

- [ ] Key `App::remuxes` by a complete immutable job key (detail/source
      identity, action, encoder, audio selection, normalized arguments, and
      output format), not only `detail_id`, so incompatible clients/plans never
      attach to each other's output.
- [ ] Remove jobs from the map on every terminal state. The current success
      path sets `done=true` and then removes only when `!done`, leaving completed
      jobs permanently resident.
- [ ] Make cache invalidation and map replacement atomic. A source change must
      not delete the destination and then attach to a completed job whose file
      has disappeared.
- [ ] Include source identity and a plan/tool-version hash in cache names or
      stamps; seconds-resolution mtime plus size is insufficient after config
      changes or same-size replacements.
- [ ] Use an explicit state machine (`Starting`, `Growing`, `Complete`,
      `Failed`, `Cancelled`) and notify waiters instead of polling shared files
      and atomics.
- [ ] Add concurrency tests for different plans on one detail, successful map
      cleanup, failure cleanup/retry, source replacement after completion,
      process-spawn failure, and simultaneous stale-cache requests.

### P0-04: preserve media-root identity and per-root type filters

- [ ] Replace `ScanConfig { media_dirs, types }` with root records containing a
      canonical path, configured path, stable root key, display title, and that
      root's A/V/P mask. A global union currently allows every selected type in
      every root.
- [ ] Make `media_rel_key` root-qualified. Two roots containing the same
      relative name must not overwrite, deduplicate, relocate, or delete one
      another during monitor reconciliation.
- [ ] Stop locating a root by the first matching path component name; use the
      selected root record and an explicit persisted relocation mapping for
      host-path/container-path rebasing.
- [ ] Define behavior for nested, duplicate, missing, and same-basename roots
      and reject ambiguous configurations before scanning.
- [ ] Add tests with `V,/video` and `A,/audio` that put an audio file in the
      video root and a video file in the audio root, plus identical
      `Show/episode.*` relative paths in both roots and a restart/reconcile.

### P0-05: correct media MIME/class detection and metadata source type

- [ ] Provide a complete table for every recognized MiniDLNA extension. At
      present only MKV, MP4/M4V, AVI, MOV, TS/M2TS, MP3, FLAC, JPEG, and PNG
      have meaningful `mime_and_class` entries; other accepted audio/video
      formats fall back to `application/octet-stream` and video class.
- [ ] Resolve ambiguous MP4, ASF, and 3GP files from actual stream types rather
      than extension order; an audio-only MP4 must become
      `item.audioItem.musicTrack` and `audio/mp4`.
- [ ] Keep library admission, database MIME/class, DIDL resource MIME, HTTP
      Content-Type, and `GetProtocolInfo` capabilities generated from one
      canonical media-format mapping.
- [ ] Do not run video-oriented libav interpretation on a JPEG in a way that
      records it as an MKV/other-video probe; add explicit image probing.
- [ ] Add small valid fixtures and table-driven scan/Browse/GET tests for every
      supported extension, including AAC, M4A, WAV, OGG, WMA/ASF, DSF/DFF,
      MPEG/VOB/WMV/FLV/WebM/RM/RMVB, 3GP, and audio-only MP4.

### P0-06: fix HEAD behavior for streamed original files

- [ ] Clear or suppress `HttpResponse.file_range` for HEAD. Currently the
      handler clears only the in-memory body, so an original response larger
      than 8 MiB still streams file bytes after its headers.
- [ ] Ensure HEAD never reads a small media body into memory and decide whether
      HEAD on an uncached transcode may start an expensive job; preferably
      return metadata without launching ffmpeg.
- [ ] Test small and large originals, ranged HEAD, finished transcode, growing
      transcode, art, caption, and error HEAD responses by checking that zero
      bytes follow the header terminator while GET behavior remains unchanged.

### P0-07: harden the HTTP parser and bound connections

- [ ] Reject malformed request lines/versions, whitespace before header colons,
      invalid field names/values, obsolete folding, invalid Content-Length,
      conflicting duplicate Content-Length, and Transfer-Encoding plus
      Content-Length. Do not silently treat an invalid length as zero.
- [ ] Either implement chunked request decoding correctly or reject every
      Transfer-Encoding with 400/501 before dispatch; current routing only uses
      it to disable persistence.
- [ ] Preserve unread bytes between requests or force close whenever a read
      contains pipelined bytes. The current loop truncates the body and drops a
      following pipelined request.
- [ ] Add header-read, body-read, keep-alive idle, and write timeouts to stop
      slowloris connections; cap simultaneous TCP connections with a
      configurable semaphore analogous to MiniDLNA `max_connections`.
- [ ] Reduce the 256 MiB request-body limit to a protocol-appropriate default
      and make it configurable with a hard upper bound. Avoid allocating based
      solely on an untrusted length.
- [ ] Move mature parsing to a maintained HTTP implementation if exact dialect
      headers can be retained; otherwise fuzz the custom parser and add raw
      smuggling/pipelining regression tests.

### P0-08: close the SSDP renderer-description SSRF and amplification paths

- [ ] Pass the UDP sender address into renderer NOTIFY processing and require
      the `LOCATION` host to equal that sender before connecting. Reject
      loopback, unspecified, multicast, broadcast, and off-link targets unless
      an explicit trusted policy allows them.
- [ ] Bound the renderer-description response by bytes and header/body time,
      validate HTTP status/content type, and stop using unbounded
      `read_to_string`.
- [ ] Rate-limit and deduplicate renderer description fetches by sender and
      USN/location; use a bounded worker pool rather than one `spawn_blocking`
      task per matching datagram.
- [ ] Schedule M-SEARCH jitter/replies outside the receive loop and rate-limit
      replies per source. Sleeping in the receive loop and sending every reply
      twice makes request floods block discovery and amplify traffic.
- [ ] Clamp/validate MX according to the intended dialect and add adversarial
      UDP tests for spoofed LOCATION, floods, oversized descriptions, and slow
      endpoints.

### P0-09: make scanner/database updates transactional and observable

- [ ] Return `Result` from scan, monitor, repair, metadata, art, and probe
      mutation paths instead of discarding database/I/O errors with `let _ =`.
- [ ] Use `rusqlite::Transaction` RAII for each coherent catalog update; roll
      back on any failed insert/update/delete and publish a new in-memory
      Catalog/SystemUpdateID only after a successful commit.
- [ ] Replace production `expect("open files.db")` calls with propagated errors
      and a defined retry/read-only/degraded policy so a background thread
      cannot silently die or panic the process.
- [ ] Enable foreign keys, add constraints/unique indexes where appropriate
      (`SETTINGS.KEY`, `ALBUM_ART.PATH`, relationship rows), set a schema
      version, and implement ordered, testable migrations rather than ignoring
      arbitrary ALTER errors.
- [ ] Run integrity checks at startup, define corruption recovery/backup, and
      test injected busy/full/I/O failures and interrupted migrations.

### P0-10: prevent scanner traversal outside configured roots

- [ ] Default to MiniDLNA-compatible `wide_links=false`: before descending a
      directory symlink, canonicalize its target and require it to remain under
      that root. The HTTP jail currently rejects the eventual GET, but the
      scanner can still enumerate and store outside content.
- [ ] If outside-root aliases are a desired product feature, add an explicit
      `wide_links=true` option with a security warning and treat every allowed
      canonical target as a serving root.
- [ ] Use one root/jail policy for scanning, metadata probing, artwork,
      captions, resizing, and original media rather than checking only at
      response time.
- [ ] Add directory-symlink escape, loop, retarget, broken-link, and
      inside-root alias tests for initial scan and inotify reconciliation.

### P0-11: escape device XML and repair protocol-info output

- [ ] XML-escape every configurable/generated value in `gen_root_desc`,
      including friendly name, manufacturer/model fields, serial, UUID, and
      presentation URL. Add tests with `&`, `<`, `>`, quotes, and non-ASCII.
- [ ] Fix the missing separators among the final OGG, RealMedia,
      RealMedia-VBR, and WebM entries in `PROTOCOL_INFO_SOURCE`; current string
      concatenation produces one invalid combined protocol-info entry.
- [ ] Generate or parse the protocol-info list structurally and test every
      entry against the reference macro so missing commas cannot recur.
- [ ] Add XML parser validation for every generated description/SCPD/SOAP/DIDL
      document, not only substring assertions.

### P0-12: bound Browse/Search response work

- [ ] Enforce a maximum SOAP response size comparable to MiniDLNA's 2 MiB
      guard, with deterministic truncation/pagination semantics and correct
      `NumberReturned`/`TotalMatches`.
- [ ] Cap server-side `RequestedCount=0` work or stream/encode results without
      building multiple full XML strings in memory.
- [ ] Avoid cloning every search hit before pagination and move indexed search,
      sort, and count work into SQLite or another bounded query layer.
- [ ] Add large-catalog benchmarks and tests for response cap boundaries,
      huge starting indexes, zero requested count, and clients disconnecting
      during response generation.

### P0-13: fix production container defaults

- [ ] Remove the second default bind mount collision in
      `docker-compose.yaml`. With an empty environment, Compose resolves the
      optional `RUSTY_DLNA_MEDIA_AT` mount to `/storage/video:/storage/video`
      and replaces the intended `./media:/storage/video` default.
- [ ] Make a fresh documented compose deployment advertise a usable address
      and stable UUID, or fail fast with exact setup instructions; the baked
      config currently combines `listen_ip=0.0.0.0`, no advertise IP, and the
      shared fallback UUID.
- [ ] Run the daemon as an unprivileged UID/GID with writable cache ownership;
      UDP 1900 and TCP 8200 do not require root. Add dropped capabilities,
      `no-new-privileges`, and an init/reaping strategy.
- [ ] Add a healthcheck that verifies the HTTP description and database/scanner
      state without sending LAN multicast.
- [ ] Add a clean-directory compose smoke test that validates mounts, starts
      the service, checks rootDesc/status, scans a fixture, and shuts down with
      byebye.

## P1 — feature completeness and reference parity

### P1-01: extract first-class audio metadata

- [ ] Read title, artist, album artist, album, genre, composer, contributing
      artists, track/disc, date, comment, and embedded cover art from MP3,
      FLAC, Vorbis/Opus, MP4/AAC, WMA, WAV, and DSD metadata using libav or
      format-specific libraries.
- [ ] Define NFO-vs-embedded-vs-filename precedence and update inode aliases
      consistently when tags change.
- [ ] Populate the existing Artist, Album, Genre, Composer, Contributing
      Artists, Album Artist, Rating, and Recently Added trees rather than
      leaving most of them as empty seeded placeholders.
- [ ] Add valid tagged fixtures and parity assertions against the MiniDLNA
      reference database/DIDL for representative formats.

### P1-02: extract first-class image metadata

- [ ] Parse JPEG EXIF date taken, camera make/model, orientation/rotation,
      width/height, comment/title, and embedded thumbnail.
- [ ] Populate Date Taken, Camera, Album, Rating, and Recently Added views from
      real metadata rather than NFO/mtime fallbacks.
- [ ] Apply EXIF orientation in thumbnail/resized output and advertise the true
      resolution/profile.
- [ ] Decide whether PNG/WebP/HEIF library images are a rustyDLNA extension;
      MiniDLNA's own source only recognizes JPEG as a library image.

### P1-03: implement playlist ingestion and views

- [ ] Parse M3U/M3U8 and PLS, normalize relative/absolute entries under allowed
      roots, reject binary/oversized files, and populate the existing
      `PLAYLISTS` schema and Music/Video/Pictures playlist containers.
- [ ] Refresh playlist membership on file/playlist rename, change, and delete;
      keep stable IDs and correct item counts.
- [ ] Add encoding, duplicate, missing-entry, cross-root, and traversal tests
      modeled on the reference `playlist.c` behavior.

### P1-04: replace hand-written XML/NFO parsing

- [ ] Use a bounded streaming XML parser for SOAP and NFO instead of tag
      substring scans. Correctly handle namespaces, attributes, CDATA, numeric
      entities, UTF encodings, repeated tags, nesting, and malformed input.
- [ ] Return the correct 402/708 fault for malformed or unsupported SOAP input
      rather than silently extracting partial values or returning zero hits.
- [ ] Keep the NFO 64 KiB cap, define malformed-file fallback behavior, and add
      a corpus covering Kodi NFO variants and entity/namespace edge cases.

### P1-05: complete SearchCriteria and Filter semantics

- [ ] Replace literal `" or "`/`" and "` splitting with a tokenizer/parser that
      respects quotes and nested parentheses and implements the advertised
      UPnP grammar/operators (`doesNotContain`, `!=`, relational operators,
      boolean precedence, and supported properties).
- [ ] Represent parse failure explicitly and return ContentDirectory 708;
      unknown properties/operators must not be indistinguishable from a valid
      zero-match query.
- [ ] Parse Filter as case-insensitive comma-separated fields and attributes,
      not substring matches.
- [ ] Add filter bits for every optional DIDL field. Currently creator,
      description, artist, actor, album, genre, track/episode, artwork, and
      playback fields can be emitted even when a listed Filter omitted them.
- [ ] Keep required fields and intentional Samsung/Kodi exceptions explicit,
      with golden DIDL tests for `*`, empty, field-only, `res`, and individual
      `res@attribute` requests.

### P1-06: make sidecar updates and deletion correct

- [ ] Match captions only when the filename is exactly the video stem followed
      by the caption extension or a `.` language/variant suffix; `movie2.srt`
      must not attach to `movie.mkv`.
- [ ] On NFO deletion, clear formerly inherited/sidecar metadata back to
      embedded/file defaults and rebuild affected virtual aliases.
- [ ] On poster/art deletion or replacement, clear/reselect `ALBUM_ART`, remove
      stale cached derivatives only when unreferenced, and refresh every inode
      alias.
- [ ] On caption add/change/delete, rebuild caption rows even when media
      size/mtime is unchanged and emit exactly one catalog update.
- [ ] Add inotify and periodic-reconcile tests for all sidecar add/change/delete
      cases and same-stem collision cases.

### P1-07: finish exclusion and scan-policy parity

- [ ] Implement MiniDLNA `exclude_file` basename glob semantics (`*` and `?`),
      with case behavior documented and tested; current behavior is exact
      equality only.
- [ ] Make hidden-file/directory skipping an explicit documented option or
      parity rule rather than an unconditional undocumented filter.
- [ ] Add configurable `album_art_names`, subtitle enable/disable, thumbnail
      generation/width/quality/filmstrip, and `wide_links` settings where those
      reference options remain in product scope.
- [ ] Add limits/timeouts or libav interrupt callbacks for viability probes,
      metadata probing, thumbnail extraction, and external commands so corrupt
      or remote files cannot hang scanning indefinitely.
- [ ] Track nanosecond mtime or another replacement signature so an in-place
      same-size/same-second rewrite is not missed.

### P1-08: define and implement Recently Added semantics

- [ ] Decide whether rustyDLNA intentionally uses 200 items with no time window
      or should match the reference's 50-item/approximately-90-day behavior.
- [ ] Make the chosen limit/window configurable if both deployed behaviors are
      needed, document whether file mtime or metadata date controls it, and
      apply it consistently to audio, video, and pictures.
- [ ] Add deterministic boundary, alias-deduplication, restart, and clock-skew
      tests; update docs that currently present divergence as completion.

### P1-09: complete multi-interface SSDP

- [ ] Model each active interface with its own IP, receive membership, send
      socket, multicast interface, LOCATION URL, and unicast reply source.
      Current code joins multiple memberships but still advertises and sends
      through one global address/interface.
- [ ] Send alive/byebye on each selected interface and choose the reply
      LOCATION from the interface that received the M-SEARCH.
- [ ] Support multiple IPv4 addresses per named interface or reject ambiguity,
      and react predictably to interface/address changes.
- [ ] Add network-namespace integration tests with two interfaces proving no
      cross-interface LOCATION/source mismatch.

### P1-10: make GENA delivery bounded and reliable

- [ ] Replace one OS thread per notification with a bounded delivery queue and
      worker set; 500 subscribers must not produce 500 simultaneous threads on
      every scan update.
- [ ] Use a standard random UUID generator for SIDs and test collision-free
      creation under concurrency.
- [ ] Clamp subscription timeouts, handle `Second-infinite` deliberately, prune
      repeatedly failing subscribers, and record delivery failure metrics.
- [ ] Validate callback HTTP responses sufficiently to distinguish success,
      retryable failure, and permanent failure without blocking catalog locks.

### P1-11: manage derived-image cache correctness

- [ ] Include source/art identity and mtime/content stamp in resized/thumbnail
      cache keys so a replaced image does not serve an old derivative.
- [ ] Write derivatives to unique temporary files and atomically rename them;
      coordinate concurrent requests for the same size.
- [ ] Add configurable maximum dimensions, pixel/decoder memory limits,
      quality, cache quota, LRU/age eviction, and free-space protection.
- [ ] Replace the 1x1 placeholder icon byte arrays with real 48x48 and 120x120
      PNG/JPEG assets matching rootDesc, and verify dimensions in tests.

### P1-12: supervise transcode processes and storage

- [ ] Add process timeouts, cancellation tokens, shutdown coordination, and
      explicit policy for continuing after the last client disconnect. Ensure
      child processes are terminated/reaped on shutdown and failed jobs cannot
      become orphans.
- [ ] Stream/bound stderr instead of `Command::output`, and make tail extraction
      UTF-8 boundary-safe; the current byte slice can panic on non-ASCII text.
- [ ] Persist real stream descriptors/indexes and choose the actual audio
      stream index. Selecting a position from a de-duplicated codec CSV can map
      the wrong stream when codecs repeat.
- [ ] Add cache quotas, minimum-free-space checks, LRU/age cleanup, startup
      cleanup for `.part`/raw-HEVC/intermediate files, and metrics for job/cache
      size.
- [ ] Verify finished outputs with ffprobe before rename and quarantine/delete
      corrupt results.
- [ ] Either implement preservation of mastering-display/MaxCLL metadata or
      remove the unsupported claim from `docs/TRANSCODE.md`; validate HDR10 and
      Profile-8 output metadata with real fixtures.
- [ ] Remove or integrate dead/duplicated transcode APIs such as the old live
      pipe/cache helpers so documentation and tests exercise only the shipped
      path.

### P1-13: improve concurrency and large-library behavior

- [ ] Move blocking SQLite opens/queries, directory reads, image scaling,
      process setup, and other synchronous work off Tokio request tasks.
- [ ] Use a centralized database actor/pool and prepared statements instead of
      repeatedly opening SQLite for bookmarks/update IDs and scanning the full
      cloned in-memory Catalog for every search.
- [ ] Avoid cloning 600+ byte `MediaItem` values for routine browse/search;
      measure memory for 10k, 100k, and 1M objects and introduce shared/compact
      representations where justified.
- [ ] Replace fixed five-second full reconciliation after common inotify bursts
      with targeted updates plus a bounded fallback, and expose watch coverage,
      overflow, scan duration, and last error.
- [ ] Coordinate periodic scan, inotify scan, metadata backfill, and shutdown
      with owned tasks/cancellation instead of detached infinite OS threads.

### P1-14: make status and health operationally useful

- [ ] Replace the unused `scan.status` file read with real shared scan state:
      phase, current path/count, start/end time, duration, last success/error,
      inotify watch count, and next reconciliation.
- [ ] After remux-map cleanup is fixed, expose truly active/queued/failed job
      counts, cache bytes, and job age instead of completed entries mislabeled
      as active.
- [ ] Add a machine-readable health/status endpoint with no sensitive absolute
      paths by default; keep the presentation HTML escaped and lightweight.
- [ ] Define healthy/degraded/unhealthy conditions for listener, DB integrity,
      scanner freshness, required tools, and free cache space.

### P1-15: preserve non-UTF-8 paths correctly

- [ ] Stop using lossy UTF-8 strings as database identity keys. Store raw Unix
      path bytes or an unambiguous reversible encoding while retaining a safe
      display string for DIDL/logs.
- [ ] Ensure two distinct non-UTF-8 paths cannot collide, and test scan,
      restart, rename, sidecar lookup, and GET on such filenames.

## P2 — maintainability, test quality, delivery, and documentation

### P2-01: establish mandatory local and CI quality gates

- [ ] Pin an exact supported Rust toolchain instead of floating `stable`; test
      the declared MSRV 1.80 separately from the release toolchain.
- [ ] Extend `scripts/check.sh` to run fmt check, strict Clippy, workspace tests,
      explicitly enabled socket E2E, doc build, config validation, and
      documentation consistency. Do not rely on `/usr/local` rustup/cargo paths.
- [ ] Add CI jobs for Linux build/test, MSRV, current pinned toolchain, real
      E2E ports, Docker build/smoke, and compose isolation.
- [ ] Make conditional E2E tests use ignored tests or explicit failure when a
      requested E2E job lacks its environment; a green early return must not be
      confused with exercised networking.
- [ ] Add coverage reporting and set a meaningful floor for parser, protocol,
      scanner migration, and error paths.

### P2-02: improve test fixtures and hygiene

- [ ] Generate or check in tiny valid FLAC, MP3, MP4/MKV, JPEG/EXIF, subtitle,
      NFO, and playlist fixtures with documented provenance/license; do not
      repeatedly probe text pretending to be media unless rejection is the
      subject of that test.
- [ ] Use a tempfile-style RAII helper for every per-test DB/cache tree and
      delete on success and panic; stop accumulating ignored directories inside
      the repository.
- [ ] Avoid sharing `testdata/cache/files.db` between parallel unit/E2E tests;
      each server process needs an isolated cache to prevent races and state
      leakage.
- [ ] Decide whether the tracked `testdata/testdata/cache/files.db` is an
      intentional migration fixture. If so, rename/version/document it and
      test its checksum; otherwise remove it from source control.
- [ ] Capture expected ffmpeg/libav failure output and assert structured errors
      so successful test logs remain readable.

### P2-03: add protocol and parser robustness testing

- [ ] Add fuzz targets for HTTP request/range parsing, SSDP parsing, SOAP/XML,
      SearchCriteria, Filter, NFO, URL/ID parsing, and caption/art matching.
- [ ] Add property tests for range arithmetic, XML escaping/unescaping,
      pagination totals, sort stability, object ID uniqueness, and path/root
      normalization.
- [ ] Expand differential tests against the MiniDLNA reference from a few
      constants to normalized rootDesc/SCPD, SSDP variants, SOAP faults,
      protocol-info entries, DIDL fields, media classification, and database
      virtual views.
- [ ] Add malformed, oversized, slow, and concurrent client integration tests,
      plus long-running scan/remux/restart soak tests.

### P2-04: simplify code ownership and unsafe boundaries

- [ ] Split the roughly 5k-line server and scanner modules into focused
      components (config, catalog query, DIDL mapping, media serving, SSDP
      runtime, scan coordinator, sidecars, virtual views) with narrow APIs.
- [ ] Replace long positional tuples/argument lists with named structs and
      remove dead code such as `Catalog::next_child_id`; use this refactor to
      clear Clippy complexity and large-variant warnings.
- [ ] Add `// SAFETY:` invariants and focused wrappers/tests for every libav,
      getifaddrs, poll, and signal unsafe block; keep raw pointers out of normal
      catalog logic.
- [ ] Replace broad boxed/string errors and lock `expect` calls with typed
      errors and poison recovery/shutdown policy. Keep error context with path,
      operation, and source.
- [ ] Add module/API documentation and `cargo doc` warnings-as-errors once the
      public surface is intentional.

### P2-05: make dependencies and images reproducible

- [ ] Pin Docker base images by digest and pin the dovi_tool version. Do not
      silently turn a failed `cargo install dovi_tool` into a successful image;
      either require it or build an explicit no-dovi variant.
- [ ] Use locked dependency installs and cache-friendly manifest-first Docker
      layers without copying generated/test cache data into build context.
- [ ] Add `cargo audit`/RustSec, `cargo deny` license/source/advisory checks,
      dependency update automation, SBOM generation, and container vulnerability
      scanning.
- [ ] Document the GPL implications of ffmpeg/dovi_tool build choices and
      verify the distributable image contains required notices/source offers.
- [ ] Produce versioned, signed release artifacts/images with changelog and
      reproducible build metadata instead of only `rusty-dlna:local`.

### P2-06: reconcile documentation with shipped behavior

- [ ] Remove the README/architecture claims that the shipped transcode path is
      a live stdout pipe or is killed on disconnect; the implementation is a
      background growing-file cache that deliberately survives probes.
- [ ] Correct `docs/TRANSCODE.md` references that still call HTTP output a live
      pipe and verify every HDR/audio/cache statement against generated argv
      and output probes.
- [ ] Update `docs/CHECKLIST.md` entries that overstate completion: per-root
      media types, multi-interface SSDP, full Search/Filter, thumbnail behavior,
      and related phases need links to this audit or narrower claims.
- [ ] Retire or clearly mark the stale singular `improvement_plan.md` as an
      historical phase plan; this file is the current consolidated plan.
- [ ] Keep only one canonical `replica.md` or add a test that root and
      `docs/replica.md` are byte-identical so the two copies cannot drift.
- [ ] Document intentional differences from MiniDLNA and the supported product
      scope (video-only versus first-class music/images) in one compatibility
      matrix.

### P2-07: improve configuration UX and validation

- [ ] Add `#[serde(deny_unknown_fields)]` or an explicit unknown-key warning so
      misspelled settings do not silently disappear.
- [ ] Validate ranges and conflicts for ports, notify/rescan intervals,
      max_jobs, roots, cache/db dirs, exclusions, remap names/actions, and
      interface/address settings before constructing `App`.
- [ ] Resolve `cache_dir` and `db_dir` as separate documented concepts or
      remove one; currently `cache_dir` takes precedence and the database is
      always placed under it, making a simultaneous `db_dir` misleading.
- [ ] Add `--print-effective-config`, `--rescan`, database rebuild/migrate/check
      commands, and secret-free diagnostics suitable for support reports.
- [ ] Make relative paths consistently relative to the config file and add
      tests for CLI/default/container configurations.

## Reference features requiring an explicit product decision

These are present in the MiniDLNA-derived reference but are absent or only
represented by placeholders in rustyDLNA. They should be implemented or marked
as deliberate non-goals; they are not all automatic release blockers for a
video-focused product.

- [ ] Decide whether music and pictures are fully supported products. If yes,
      complete P1-01/P1-02/P1-03 and their client tests; if no, remove empty
      roots/capabilities and document video-only behavior honestly.
- [ ] Decide on TiVo discovery/protocol support (`enable_tivo`,
      `tivo_discovery`); implement isolated compatibility tests or document it
      as unsupported.
- [ ] Decide on Avahi/mDNS integration and MiniSSDPd socket integration; add
      optional features only when deployments require them.
- [ ] Decide on IPv6 HTTP/SSDP support and specify dual-stack LOCATION, Host,
      callback, and interface behavior before implementation.
- [ ] Decide which MiniDLNA configuration options remain compatible:
      `presentation_url`, model name/number, `strict_dlna`, `force_sort_criteria`,
      `merge_media_dirs`, `max_connections`, `stream_buffer_mb`, `user`,
      `log_dir`, and `log_level`.
- [ ] Decide whether the reference privilege-drop behavior is exposed as a
      daemon option in addition to the recommended non-root container.
- [ ] Decide whether filesystem upload/import endpoints are out of scope. They
      are not implemented by the reference either and should not be implied by
      “full parity.”

## Recommended execution order

- [ ] Milestone 1 — correctness core: P0-01 through P0-06, P0-09 through
      P0-11, with regression tests and format/lint green.
- [ ] Milestone 2 — exposed-input and resource safety: P0-07, P0-08, P0-12,
      transcode supervision/cache limits from P1-12, and load/adversarial tests.
- [ ] Milestone 3 — deployability: P0-13, P2-01, P2-02, P2-05, and a clean-host
      compose smoke test.
- [ ] Milestone 4 — declared parity: select the supported product scope, then
      complete the relevant metadata, playlists, Search/Filter, sidecar,
      scan-policy, Recent, and multi-interface P1 work.
- [ ] Milestone 5 — maintainability and release: module/API cleanup,
      differential/fuzz/soak tests, documentation reconciliation, signed
      artifacts, and a release checklist.

## Definition of high quality / stable release

- [ ] All applicable P0 checkboxes are complete with focused regression tests.
- [ ] The product-scope decisions above are written down and no advertised
      root/action/config option is only a nonfunctional placeholder.
- [ ] `cargo fmt --all -- --check`, strict workspace Clippy, workspace tests,
      doctests, explicitly enabled socket E2E, and Docker smoke tests pass in CI
      on every change.
- [ ] HTTP/SOAP/SSDP/XML fuzz targets have a sustained clean run, and resource
      limits are verified under slow/flood/large-catalog tests.
- [ ] Scanner/database fault-injection tests prove no partial catalog publish,
      update-ID lie, or unrecoverable migration on common failures.
- [ ] A representative MiniDLNA differential suite passes for every behavior
      claimed compatible, with intentional differences documented.
- [ ] A 24-hour scan/browse/event/remux soak test shows bounded threads, file
      descriptors, memory, database size, and cache space, with clean shutdown
      and restart.
- [ ] Production runs unprivileged with a stable identity, correct per-interface
      advertisement, bounded cache/connections/jobs, health reporting, and a
      tested backup/upgrade/rollback procedure.
- [ ] README, architecture, transcode guide, configuration examples, and
      compatibility matrix describe the same behavior as the release binary.
