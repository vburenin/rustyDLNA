# Changelog

All notable changes are recorded here. Releases use semantic version tags and
publish a signed, content-addressed OCI image with SBOM and provenance.

## Unreleased

- Added operator library-maintenance tools under `contrib/library/` for NFO,
  posters, generated genre/year/age views, and timeline previews. They take
  `--root` or `RUSTY_DLNA_MEDIA`, keep caches in `<library>/.rusty-library/`,
  and read TMDB/OMDb credentials only from the environment.
- Added separate spoiler-safe movie About text and full plots from Kodi-style
  NFO `<outline>` / `<plot>` metadata. The browser reveals plots only through
  an explicit spoiler disclosure, while DLNA advertises only the safe outline.
- Replaced internal SHA-1 cache identities and scanner fingerprints with
  SHA-256. Generated cache keys now use 64 lowercase hexadecimal characters.
- Fixed Compatible playback for malformed MPEG-4 Part 2 sources by selecting
  normal portable transcoding instead of requesting the H.264/HEVC-only frame
  repair mode. Desktop Chrome Media Source delivery now indexes each complete
  fragmented-MP4 movie fragment independently, reducing copied UHD HEVC startup
  from large keyframe-group downloads while native Apple HLS retains strictly
  independent segments. A paused Media Source player stops fragment-index
  polling and media downloads after its first playable fragment, resuming the
  same pump on Play while retaining only the bounded session heartbeat.
- Routed supported desktop Chrome HEVC-copy/AAC-conversion streams through
  bounded Media Source fragments, avoiding premature end-of-stream from the
  native growing-MP4 loader. Early native end events on copied Compatible
  streams now resume with portable codecs instead of moving progress to the
  end, and mixed copy/encode fragment producers pace after their startup
  buffer.
- Treated browser autoplay rejection as a ready, paused player: the Play
  control remains visible, assistive technology receives a polite prompt, and
  the normal playback-error banner stays hidden.
- Added bounded Chrome-on-Android Media Source delivery for video that requires
  encoding, avoiding the failed native growing-MP4 attempt and its startup
  timeout. It appends confined finite fragments from the existing server job
  using the mobile-safe Constrained Baseline H.264/AAC profile; working copied
  video and the saved quality preference remain unchanged. Fragment encoders
  run unrestricted for a 30-second startup buffer, then pace near playback rate
  instead of saturating the host while racing through the complete title.
- Made Auto H.264 browser transcodes derive their level from the actual output
  instead of declaring every stream as Level 5.1. This gives
  lower-resolution sources accurate decoder signaling while retaining Auto's
  full-resolution, bitrate, and true-4K behavior; a cache revision prevents
  reuse of previously mislabeled streams.
- Recovered Chromium compatible playback when its initial growing-MP4 reader
  remains attached without decoding: the player reopens the same healthy
  generation after a bounded startup stall, preserving accumulated output, then
  falls back to the existing bounded replacement retry if needed.
- Kept the Android landscape Watch player inside the measured visible mobile
  viewport, even when Chrome retains a stale CSS viewport height after
  rotation. Its compact overlaid header and control gradient let the video use
  the complete height with symmetric letterboxing while keeping touch controls
  reachable without scroll-induced vertical jumping.
- Reduced interactive infinite-scroll pages to 24 cards, served prepared
  360x540 posters directly, and bounded near-viewport artwork to four
  low-priority asynchronous requests. Cached derivatives bypass maintenance,
  the paging sentinel rearms after rapid movement or a missed WebKit exit
  callback, and the browser gateway compresses JSON, JavaScript, and CSS
  responses.
- Kept fullscreen controls transient on macOS Safari by releasing WebKit's
  incidental pointer focus, and locked root-page overflow during element
  fullscreen. Keyboard focus still pins the controls for accessibility.
- Based compatible keyboard seeks on the last exact target while replacement
  media is loading instead of restarting from its ten-second segment boundary.
- Made Auto recover an Original video that never starts or remains buffering
  for twelve seconds by preserving its position and selecting the safest
  advertised Compatible quality. Explicit Original mode remains unchanged.
- Kept active Chrome compatible streams alive across reader-free range gaps by
  renewing the browser generation with a bounded status lease; closed or lost
  sessions still expire, and explicit source replacement still cancels at once.
- Added a separate, unprivileged `rusty-web` gateway container with no
  rustyDLNA binary, media/cache mounts, or UDP listener. Its explicit route and
  method allowlist serves the browser player while returning 404 for UPnP
  descriptions, SOAP, GENA, DLNA media/transcode, and icon endpoints.
- Added a Close player control on the video that stops the current title and
  returns to the library. Browse still keeps playback running; Close is the
  way out of a movie from inline Watch, fullscreen, and iPhone expanded
  playback.
- Used the compact landscape Watch toolbar on portrait phones so Previous,
  Next, and stream information leave the control row. The time label no longer
  overlaps transport buttons in vertical playback, including iPhone expanded
  video.
- Hid the in-player volume slider on touch-first devices, including Android
  fullscreen and iPhone expanded playback. Hardware volume and Mute remain;
  iOS cannot change element volume from the page.
- Kept Resume and Start over above the video control surface on small screens,
  hid transient playback controls until the choice is made, and enlarged both
  actions for reliable touch input.
- Added touch double-tap seeking on the video surface: right advances 30
  seconds, left rewinds 30 seconds, and brief directional feedback confirms
  the jump. Android fullscreen accepts a wider, slower tap pair and treats a
  missed pair as control disclosure instead of accidental play/pause.
- Kept the video control surface visible for at least five seconds after touch
  interaction, including mobile browsers that emit pointer-leave immediately
  after a tap, without changing desktop mouse or keyboard behavior. Enlarged
  the timeline thumb and its horizontal drag area on touch-first devices so
  seeking no longer requires hitting a desktop-sized scrubber.
- Made the fullscreen control expand the in-page player across the visible
  iPhone viewport, preserving the custom timeline and playback controls while
  tracking Safari toolbar and rotation changes through Visual Viewport, and
  holding a Screen Wake Lock during active visible playback. The lock releases
  on pause, exit, failure, or page hide. Other browser fullscreen fallbacks
  remain nonfatal when unsupported or rejected.
- Made mobile compatible playback treat browser source-support errors like
  decode errors for copied codecs, retry with portable H.264/AAC, and step down
  to the safest advertised quality when that rendition is also rejected.
  iPhone and iPad compatible video now uses native HLS backed by an append-only
  event playlist and fixed-length initialization and media resources from the
  existing cache-controlled fragmented MP4 job. This avoids WebKit Media Source
  stalls and AVFoundation ambiguity around ranges of growing resources, keeps
  generation-safe cancellation and seek restarts, and remains eligible for
  AirPlay. One-second IDR segments and first-segment publication reduce
  startup, and advisory HLS playlist stalls no longer display a buffering
  error while decoded playback continues.
  Browser MP4 output also suppresses FFmpeg's implicit chapter text track so
  its stream set exactly matches the video/audio codecs declared to WebKit,
  and CUDA browser encodes now normalize decoded frames before NVENC so a
  mid-stream color-metadata change cannot reinitialize the filter graph into a
  500 response. H.264-to-NVENC browser jobs now use the lower-latency software
  decoder while HEVC retains CUDA decode, reducing cold HLS startup without
  sacrificing sustained throughput.
- Copied attached JPEG artwork directly from bounded Matroska/MP4 header
  metadata, avoiding FFmpeg jobs that could wait to the scanner deadline for a
  demuxed cover packet. Remaining per-file artwork I/O failures preserve the
  existing cover and no longer roll back or indefinitely retry the complete
  startup reconciliation.
- Replaced the library's Load more action with generation-safe infinite
  scrolling that preloads bounded pages near the viewport, preserves focused
  cards while appending, stops at the catalog end, and retains full-refresh
  recovery when the catalog changes between pages.
- Added explicit Browse and Watch presentations to the embedded player. Browse
  expands the library across the screen without restarting active playback,
  while stable URL state, per-mode scroll restoration, and the Now playing
  shortcut make switching presentations reversible and Back-button safe.
- Kept the last decoded video frame visible while direct and compatible seeks
  wait for the target frame instead of showing a black player surface.
- Added source-bound JPEG sprite sidecars under per-directory
  `.rusty_previews/<video-stem>/` paths for immediate timeline previews, with a
  640×360 default and generator-selectable bounded resolution and sprite
  layout, layout-aware adaptive intervals, confined validation/serving, bounded
  browser decoding, and last-frame fallback when previews are unavailable.
- Made media-root relocation aliases durable and transactionally bounded,
  converged raw-name probe sidecars with per-path provenance and shared physical
  probes, bounded/recoverable inotify state without mutating host sysctls, and
  preserved transcode source identity without changing shared file cursors.
- Made `--check` and `--print-effective-config` storage-neutral while preserving
  persisted identity validation and complete remap output, and prevented stale
  linked-title loads from overriding newer browser navigation or selection.
- Hardened outbound HTTP and SSDP wire construction with shared token and
  field-value validation, fallible production serializers, fail-closed legacy
  wrappers, standard `206 Partial Content`, and explicit UTF-8 HTML responses.
- Made startup and catalog storage fail closed: identity/network preflight is
  storage-neutral, root aliases commit atomically, publication generations must
  be sequential, and web list consistency uses at most three snapshots.
- Centralized caption extension, MIME, raw filename ownership, and browser
  WebVTT-conversion policy so scanning, catalog reloads, and web playback agree.
- Prepared scanner work in a reusable private SQLite stage, merged only
  disk-backed changed-key journals into the live catalog, preserved concurrent
  bookmarks and stable IDs, and made failed watcher publication retry a full
  reconciliation without waiting for another event. Ordered startup catalog
  maintenance now retries to completion before filesystem watches begin.
- Kept web, SOAP Browse, and Search database pages generation-consistent across
  catalog publication, wrapped UPnP `ui4` update IDs correctly, and invalidated
  cached pages across generation wrap.
- Preserved non-UTF-8 scanner identity across replacement and sidecar events,
  and made configured artwork selection deterministic and directory-linear
  across initial scans, reconciliation, watcher additions, and rebuilds.
- Hydrated Continue Watching from its bounded browser progress IDs instead of
  downloading and repeatedly sorting the entire media catalog.
- Serialized database-backed catalog publication without blocking Browse, and
  preserved newer bookmark state when an older scan snapshot is published.
- Restored 44-pixel timeline and volume hit areas in the compact player and
  covered the actual post-selection mobile controls.
- Kept the example Compose override on the managed cache volume and clarified
  port configuration and operator-owned bind-cache behavior.
- Reconciled fullscreen video wake locks across asynchronous pause, error,
  cancellation, visibility, denial, and system-release races.
- Escaped generated HTML error pages and stopped exposing raw media-helper or
  operating-system diagnostics in playback response bodies.
- Reclaimed expired renderer-profile slots, preserved GENA sequence-zero
  delivery under concurrent updates, and made poisoned notification shutdown
  recoverable.
- Validated cache-maintenance policy before startup mutation and made invalid
  HTTP or SSDP port environment overrides fail with actionable diagnostics.
- Stopped prior media immediately on title changes, scoped caption selection to
  one title, and kept keyboard-focused player controls visible.
- Centralized XML 1.0 sanitization for device descriptions, SOAP, and DIDL so
  invalid catalog or configuration scalars cannot make responses unparseable.
- Bounded compatible-playback recovery even when each replacement plays only
  briefly, and made reconnect range pulls use valid partial responses against
  growing browser streams.
- Unified external media-helper admission, bounded output, deadlines,
  cancellation, process-group termination, and reaping behind one shared
  supervisor; transcode job slots now release through RAII permits.
- Moved browser FFmpeg/timeline policy into the transcode layer, preserved
  non-UTF-8 paths through helper and cache identities, and made every
  output-affecting browser option part of the cache identity.
- Pinned Profile-8 jobs to one verified `ffmpeg`, `ffprobe`, and `dovi_tool`
  snapshot across cache-key construction and output production, including a
  Profile-8 cache revision that invalidates older incomplete tool identities.
- Advanced the embedded browser API to schema 2 and serialize every media item
  identifier as an exact decimal string, including IDs beyond JavaScript's
  safe-integer range.
- Made browser capability negotiation, queue completion, audio enrichment,
  picture-in-picture, previews, and source recovery generation-safe; kept
  compact Previous/Next controls keyboard reachable and added deduplicated
  screen-reader status announcements.
- Made inotify hard-link and directory-alias convergence event-driven and
  bounded across open writers, rename/unlink races, alias deletion, traversal
  churn, and overflow recovery without consulting partially published catalog
  state.
- Checked persisted sizes, detail/object allocation, playlist positions,
  SQLite paging bounds, and catalog reload arithmetic; native device/inode bits
  now round-trip through signed SQLite integers explicitly.
- Bounded SOAP sort input, required explicit Browse/Search paging arguments,
  rejected repeated `SOAPAction`, constrained SSDP jitter by `MX`, and moved the
  canonical ConnectionManager protocol-info table into the protocol crate.
- Centralized the bounded persisted stream-metadata grammar and the catalog's
  38-column row mapping so full and incremental publication cannot drift.
- Added the web unit suite to the canonical gate and the full Playwright browser
  matrix to CI and release validation without leaving runtime cache state in the
  fixture tree.

## 0.1.0 - 2026-08-18

- Hardened HTTP, SOAP, SSDP, scanner, eventing, image, and transcode resource
  limits and lifecycle behavior.
- Added first-class audio/image metadata, playlists, multi-interface SSDP,
  operational health, and reversible non-UTF-8 filesystem identity.
