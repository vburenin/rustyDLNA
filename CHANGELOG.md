# Changelog

All notable changes are recorded here. Releases use semantic version tags and
publish a signed, content-addressed OCI image with SBOM and provenance.

## Unreleased

- Fixed an unmounted or missing media root emptying the library. Periodic
  reconciliation read a vanished root, or an empty mount point, as "every file
  deleted". It dropped every item under it with its bookmarks, and files that
  came back got new IDs. Such a root is now held: its items, IDs, bookmarks,
  and playlists are kept until it returns, while every other root keeps being
  reconciled. An empty root counts as unmounted when its device differs from
  the one the last successful pass recorded, so an emptied root still
  converges. A root that disappears while earlier roots are being walked is
  held too, and removing a dangling symlink no longer deletes the item it
  pointed to under a held root. Upgrade note: a catalog written before root
  devices were recorded cannot tell an emptied root from an unmounted one, so
  a root emptied before the first pass after upgrading is held until any entry
  (for example an empty `.keep` file) appears in it or it is removed from the
  configuration.
- One unreadable subdirectory (`EACCES`) no longer fails every scan and
  reconciliation or stops the inotify watcher. It is skipped with a warning,
  and items and playlists already indexed under it keep their IDs, bookmarks,
  and metadata. A permission error, stale handle, or I/O error while opening a
  single file or symlink target no longer removes its row, whether a full
  reconciliation or an inotify event saw it.
- `.m2ts`/`.mts` (BDAV/AVCHD), DSF/DFF, and RealMedia files added after the
  first scan are now admitted. Before, they were dropped without a log line,
  and existing ones were removed from the catalog when they changed.
- A malformed or non-XML `.nfo` no longer fails every scan and reconciliation.
  Scene ASCII-art and scraper-URL NFOs carry no overrides; a bare `&`, `<br>`,
  and HTML entities are tolerated; and a sidecar that is still not well-formed
  XML keeps the item's existing metadata (a new item uses its filename) and is
  logged once.
- Kodi folder `movie.nfo` now applies to the only video in its folder when
  that video has no `{stem}.nfo`.
- M3U/PLS entries written with Windows `\` separators now resolve; drive-letter
  and UNC entries are skipped.
- Deleting the first-indexed hard link of a video no longer drops the
  surviving link from All Video, Series, and Genre.
- Deleting a PNG (or other converted) poster now clears its artwork on the next
  reconciliation, and a JPEG poster replaced in place gets a new art URL so
  clients stop showing the cached image.
- A media probe that times out (for example while a disk spins up) is retried
  by later reconciliations, up to three consecutive timeouts, instead of being
  cached as a failure on the first one. After the third it is cached as failed
  until the file's size, mtime, or inode changes. A replaced or modified file
  whose probe times out shows no stream details until a probe succeeds, rather
  than the previous file's.
- Items whose scan probe failed now report `embedded_captions_complete: true`,
  so native clients no longer fail opening them through a doomed enrichment
  probe. Enrichment now applies `.probe.toml` overrides, and the first startup
  after upgrading re-probes catalog rows recorded before embedded-subtitle
  discovery once.
- The full SQLite integrity check now runs once at startup instead of on every
  scanner, stage-backup, and writer reopen, and corruption found while running
  no longer renames the live database from under open connections.
- Opening a database written by a newer release now fails with a message naming
  both schema versions, and leaves the file unchanged.
- Native app copy requests (`reason=native_ios`) for video the server cannot
  copy are now encoded instead of failing with HTTP 400. This covers 10-bit
  H.264, H.264 above level 5.1, HEVC Range Extensions, and Dolby Vision
  Profile 7. SDR sources use the H.264 SDR encode. HDR sources use HEVC HDR10
  where the server offers it and are otherwise still rejected, so HDR is never
  silently dropped. Browser requests keep the strict rejection.
- HEVC stream copy now requires 4:2:0 Main or Main 10 video of at most 10
  bits. 4:2:2, 4:4:4, and 12-bit Range Extension streams are re-encoded instead
  of being copied to clients that cannot decode them.
- Added the additive `video_copy_available` field to video DTOs.
- A Compatible request that cannot fit the transcode cache limits now returns
  `503 transcode_storage` with `Retry-After: 30` instead of `503 transcode_busy`,
  and is counted in `web_player.failures.storage_total`.
- An active output larger than the whole transcode cache quota no longer evicts
  every completed cache entry before failing. Completed output is kept, and a
  warning names `cache_max_mb`.

- Fixed native HLS playback of copied video (the iPhone app's main path and
  Safari's opt-in native HLS) stopping partway through a title. The playlist
  froze its target duration at the first one or two keyframe intervals, so a
  later, longer interval made every playlist reload fail with HTTP 500. A
  growing copied playlist now waits up to five seconds for 20 seconds of
  segments before it is first published, and reserves a target duration of at
  least 10 seconds. No playlist tags change.
- The generation status (`/api/web/transcode/{id}`) now reports optional
  `stream_start_seconds`: the source time that output time zero represents.
  A nonzero seek that copies video from Matroska or MP4 starts at the preceding
  keyframe, which a bounded source probe establishes beside the producer
  (accurate to a fraction of a second). Until then, if the probe is skipped or
  fails, or if a generic-seek container such as MPEG-TS lands between
  keyframes, the value is `null`. A skipped probe does not count as a helper
  rejection. The probe runs only on helper slots that no admissible producer
  could need, so it never turns another session's start into
  `503 transcode_busy`. Shutdown cancels it and waits for FFprobe to be reaped.
- SubRip and ASS/SSA caption sidecars no longer fail as a whole because of one
  bad cue: zero-length, reversed, unreadable, or empty cues are omitted, and an
  SRT text block split by a stray blank line continues its cue. Sidecars that
  are not UTF-8 are read as Windows-1252, and UTF-16 files with a byte-order
  mark are decoded, instead of returning `422 caption_encoding`. Offline
  packages that store every advertised caption no longer fail on such files.
- A missing, unreadable, or confinement-rejected media file now returns
  `404 media_missing` (was 403) from `/api/web/item`, `/web/media` (including
  `mode=direct`), and `/web/download`, so status-based clients no longer
  report it as a sign-in problem. DLNA `/MediaItems` keeps its 403.
- One-item metadata enrichment (`enrich=1`) waits at most ten seconds for
  helper admission, answering a busy server with `503 transcode_busy` before
  common client request timeouts.
- Web `art_url` is now `null` for items without stored artwork instead of a
  `/Thumbnails` URL that always returned 404.
- `/AlbumArt` and `/Thumbnails` responses carry a strong file-identity `ETag`
  and answer a matching `If-None-Match` with 304. Artwork, embedded web
  assets, and web generation validators share one `If-None-Match` evaluator;
  `*` matches only on its own.
- Web library and item ETags include a per-start server tag, so a restart that
  changes configuration (transcoding, encoder outputs, captions) cannot serve
  stale capabilities through 304.
- Recently added (`sort=date_desc`) now orders by file modification time, like
  the DLNA Recently Added views, instead of NFO or embedded release dates.
- The Folders view now honors Recently added and Episode/track sorting for its
  media (subfolders stay first) instead of always using name order.
- Browser search matches every whitespace-separated word in any order and
  across fields, so `blade 2049` or `beatles abbey` find their titles. Up to 16
  distinct words are considered; single-word and exact-phrase searches return
  at least the same results as before.
- Fixed remapped (`/Transcode/`) items losing sidecar subtitles: Kodi, LG,
  BubbleUPnP and other caption-resource renderers now get the caption `<res>`
  rows after the remap and original rows, and Samsung TVs get
  `CaptionInfo.sec` on `/Transcode/` responses.
- Video items with a poster now carry `upnp:albumArtURI`, so VLC, Kodi and
  BubbleUPnP show movie and episode artwork. The existing thumbnail `<res>` row
  is unchanged; Samsung's `dlna:profileID` attribute is emitted only when the
  DIDL declares its namespace.
- The default subtitle for Samsung `CaptionInfo.sec`, `pv:subtitleFileUri` and
  `/Captions/{id}.srt` is now the untagged SRT (then any SRT, SMI, or the first
  sidecar) instead of the alphabetically first file. A non-SRT default is
  advertised with its own URL and type instead of being labelled SRT.
- SOAP Search on titles, artists, albums, genres and creators now ignores case
  for non-ASCII text too ("матриця" finds "Матриця"), matching the browser
  search.
- Unicast M-SEARCH to an announced interface (UPnP 1.1 revalidation, add by IP)
  is now answered, including requests without `MX`, when the sender is on that
  interface's subnet. Off-link unicast searches are ignored.
- With several announced interfaces, DIDL, artwork, caption and
  `presentationURL` addresses now use the interface the renderer connected to,
  so renderers on a second subnet can play media.
- `DLNA.ORG_PN` no longer advertises invented profiles for HEVC, MPEG-2 in MP4
  or H.264 in AVI, and HD H.264 MP4 uses the advertised `AVC_MP4_HP_HD_AAC`
  profile. Existing catalogs update on the next scan.
- Changed the default periodic library walk from a fixed 30 seconds to an
  adaptive 300 to 3600 seconds (`rescan_secs = 300`, `rescan_max_secs` defaults
  to 3600). Inotify still publishes ordinary local changes immediately; an
  unchanged library now backs off instead of re-reading every root about every
  30 seconds. An omitted `rescan_max_secs` is never below `rescan_secs`, and an
  explicit `rescan_max_secs = 0` keeps a fixed cadence. Lower these for NFS,
  SMB, or FUSE roots whose remote changes raise no inotify events.
  Upgrade note: a configuration that sets only `rescan_secs` now backs off up
  to `max(3600, rescan_secs)` after an idle period; add `rescan_max_secs = 0`
  to keep the previous fixed cadence.
- Health no longer reports `degraded` / "scanner success is stale" when periodic
  reconciliation is disabled (`rescan_secs = 0`) and the library is quiet.
- Idle keep-alive connections and connections that never send a request are now
  closed silently. They no longer receive an unrequested `408` that a client
  reusing the socket could read as the reply to its next request, and they no
  longer count as request timeouts in delivery metrics.
- A persistent `accept` failure such as running out of file descriptors now
  backs off from 10 ms to at most one second with rate-limited warnings,
  instead of spinning a core and flooding the log.
- The daemon exits within about one second after `graceful shutdown complete`,
  even when a blocking read is stuck on a hung media mount.
- The live `docker-compose.yaml` now sets `stop_grace_period: 45s`, and
  `restart.sh` stops with the same 45 seconds, so Docker no longer kills the
  daemon before its 15-second graceful-shutdown budget ends.
- CI now checks the native-client HTTP contract (schema 2 JSON, native HLS
  tags, Range, HEAD, DELETE, and progressive downloads) inside the production
  image on its shipped FFmpeg and each release architecture, and builds and
  tests the browser gateway image, including `nginx -t` and the HEAD, Range,
  and DELETE methods native clients use through it.
- Fixed browser Compatible audio in Chrome and Edge. Audio-only output was sent
  through a Media Source buffer typed for video and failed on the first append;
  it now plays through the native audio loader, and its recovery no longer
  tries to lower a video quality profile.
- A pause from the media keys, lock screen, or notification while the browser
  tab is hidden now stays paused. Previously the stream restarted on its own
  about 20 seconds after returning to the page, sometimes at lower quality. A
  pause the platform makes by itself in the background no longer triggers that
  restart either.
- Play (button, `Space`/`K`, or media keys) after a playback error now runs the
  recovery the error offers (Retry, Try prepared streaming, or Play original)
  instead of doing nothing.
- The focused position slider now moves 10 seconds per arrow key (60 seconds
  for Page Up/Down, Home/End for start and end) instead of 0.1 seconds, without
  restarting a Compatible stream for each step.
- Safari native HLS playback that kept playing in a background tab or Picture
  in Picture is no longer restarted from scratch on the next pause and play.
- Lock-screen, notification, and browser media controls show Previous and Next
  only when they can act (queue neighbors or chapters).
- Browser player: Back/Forward to the title that is already playing no longer
  restarts it or asks to resume again; only the library changes.
- Browser player: Back/Forward and reload restore the library's scroll position
  and focus the folder card that was opened, instead of returning to the top.
- Browser player: the Resume/Start over choice takes keyboard focus and is
  announced to screen readers; choosing keeps focus in the player.
- Browser player: cards in every view show a progress bar for partly watched
  titles and a Watched badge for titles that played to the end in this browser.
- Browser player: Clear progress keeps keyboard focus on the neighbouring card.
- Browser player: a slow library load, or the focus step after closing the
  player, no longer pulls keyboard focus back from Search or a newly opened
  title. Focus moves only when the user has not moved it elsewhere.
- Browser player: a catalog update or busy server while a list loads is retried
  automatically, a busy server is no longer reported as a connection problem,
  and a load that failed offline is retried when the browser reconnects.
- Browser player: loading shows “Loading…” instead of the previous view's count,
  a failed load clears the stale count and folder path, and Continue watching
  hides its no-op Sort control.
- Web API: `library_state` now describes the whole catalog. An empty search,
  folder, or kind view on a populated server reports `ready` instead of turning
  the status indicator amber and announcing an empty server.
- The browser player's document, scripts, and stylesheet now carry content
  ETags, so reloads revalidate with 304 instead of downloading about 400 KB.
- Fixed Compatible stream indexing at the end of a title. FFmpeg writes the
  audio that runs past the last video frame as a final movie fragment without
  video; the fragment index rejected it and failed every later playlist request
  for that job, including seeks and reopening finished output. That audio tail
  is now delivered as part of the final segment.
- Fixed Compatible playback failing at the end of titles whose container runs
  longer than the selected video and audio (another audio or subtitle stream,
  or a trailing gap). Completed-output validation now confirms such outputs
  against the source's own stream endings instead of rejecting them against the
  container duration. A library check found about 190 such titles.
- Fixed copied-video seeks into sources whose keyframes are more than ten
  seconds apart; the actual keyframe lead-in is now confirmed from the source.
- A failed or rejected Compatible job now answers playlist and fragment requests
  with HTTP 500 `transcode_failed` instead of closing the connection without a
  response, so the player recovers the stream instead of retrying a transfer.
- Stopped treating network trouble as a decoder failure in Media Source
  playback. A dropped connection, transfer timeout, truncated fragment, or HTTP
  408/429/502/503/504 now retries the same stream at the current position instead of
  abandoning stream copying or lowering quality. Quality is lowered only after
  those retries are exhausted. A slow link that keeps delivering a large
  fragment is no longer cut off by a fixed whole-request deadline.
- Kept slow clients connected for large prepared responses: the server's write
  timeout now bounds writes that make no progress, not the total time to send
  a response of up to 8 MiB.
- Made first scans of large flat folders linear: caption sidecars are found
  from one directory listing per batch instead of re-reading the folder for
  every video.
- Hard-link and symlink aliases now publish their own caption sidecars on the
  first scan instead of copying the original path's captions.
- Replaced exponential backtracking in exclusion-glob matching with a bounded
  matcher; matching results are unchanged.
- Fixed a watcher race that left a file unpublished when its writer created it,
  linked another name, and removed the first name before the watcher read the
  create event.
- Library intake now detects files that are still open for writing. Its `lsof`
  check read the wrong field and never fired, so a download that paused for
  more than the five-second settle window could be filed and compared while
  incomplete. The default `--settle-seconds` is now 30, and an unresponsive
  `lsof` keeps the file for review.
- Library intake and the catalog builders now share one container list. Loose
  `.mov`, `.mpeg`, `.mpg`, `.webm`, and `.wmv` movies that intake filed got no
  genre/year/age views, posters, or previews, and the maintenance run failed
  after moving them. The first run after upgrading can add views, posters, and
  previews for such files already in catalog homes.
- The poster fetcher no longer deletes an existing poster it cannot validate
  (for example a large or PNG image saved as `poster.jpg`) and no longer hides
  operator artwork such as `folder.jpg`, `cover.jpg`, `-fanart`, or PNG posters
  behind a fetched poster. It reports `SKIP invalid-existing` instead, and new
  posters never replace or write through an existing name.
- Posters and managed NFO files now follow the operator's umask instead of
  being made world-writable (`0666`) or group-writable (`0664`); see
  `contrib/library/README.md` for tightening files written by older versions.
- A preview attempt whose hardware decoder times out or stops making progress
  now retries with software decoding instead of failing the title after up to
  twice the video duration.
- The scheduled Rust toolchain updater now pushes and opens its pull request
  with the `RUST_UPDATE_TOKEN` secret. With `GITHUB_TOKEN` the push of the
  workflow pin changes was refused, and its pull requests would not run CI.
- The preview generator no longer makes preview folders world-writable or
  writes through symlinks. New previews follow the process umask; see
  `contrib/library/README.md` for tightening previews made by older versions.
- Increased the Media Source playback reserve from ten media seconds to about
  30 seconds of real time, scaled by the playback rate, within the existing
  compressed-byte budget.

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
