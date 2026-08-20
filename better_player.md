# Better embedded player plan

This is an implementation checklist for the embedded player currently defined by
`crates/server/web/index.html`, `app.css`, and `app.js`, with its Rust API and
compatibility-stream implementation in `crates/server/src/web_ui.rs`. The review
also covered `docs/WEB_PLAYER.md`, the media/range path, the transcode command,
and the existing server tests.

The player has a sound base: it is self-contained, uses same-origin media,
preserves folder navigation in browser history, supports direct-to-compatible
fallback, and reuses the server's path jail and bounded transcode machinery.
The next work should preserve those properties while fixing the issues below.

Priority meanings:

- **P0**: correctness, loss of control, misleading feedback, or an accessibility
  blocker. Complete these before adding more features.
- **P1**: core player completeness, architecture, and maintainability. These are
  required for a player that is pleasant to use and safe to evolve.
- **P2**: valuable product polish after the core is reliable.

Status updated 2026-08-20: **83 of 88 items complete**. The five open
compound gates below identify the remaining verification work.

## Confirmed problems in the current implementation

- Global keyboard handling in `app.js:865-896` turns `Ctrl+F` into fullscreen
  and captures arrow keys for seeking whenever any item is selected. This
  replaces browser Find and prevents ordinary keyboard scrolling outside form
  controls.
- Compatibility mode hides native controls (`app.js:390`), while the custom
  controls are outside `#player-stage`. Fullscreen therefore removes the only
  visible transport and timeline controls. The fullscreen button itself starts
  hidden, is revealed only by `mousemove`, and can be hidden while focused.
- A compatibility seek made while paused waits for a `playing` event to clear
  “Starting at…”, so the status can remain forever (`app.js:337-344`).
- Every source restart calls smooth `scrollIntoView` (`app.js:403`), including
  seeks, audio-track changes, fallback, and mode changes. On mobile this moves
  the user away from the control they just used.
- `play()` rejections are discarded, buffering/transcoding has no busy state,
  and the direct-fallback notice is immediately hidden by `start()`. Users can
  see a static poster with no explanation of whether playback is blocked,
  queued, transcoding, buffering, or broken.
- A native playback-speed change does not update or persist `state.rate`; the
  `ratechange` listener only redraws controls. PiP state is similarly not
  reflected in its button.
- The 470 px stage minimum remains active until the abrupt 760 px breakpoint.
  At intermediate widths the video column can be narrower than it is tall and
  the intended aspect ratio no longer governs the layout.
- The search debounce is not cancelled when changing view/folder/history, so an
  old search can run later in a new view. Stale fetches are ignored but not
  aborted. Audio-track enrichment marks a request permanently loaded before it
  succeeds, so a temporary failure cannot be retried.
- Previous/Next only sees the entries already loaded in the current page. There
  is no stable playback queue and no end-of-item behavior.
- The top bar's status dot is always green, including while “Connecting” and
  after a library error. API errors expose text such as “library request
  returned 500” and offer no Retry action.
- NFO/tag metadata title is sent as `metadata_title` but never displayed; the
  filename is used as the primary title. The full filename is then repeated in
  an opaque hover overlay that covers the art.
- Empty state copy is repeated in the top bar, stage, and side panel. “Video and
  audio play here, without leaving the page”, “PLAYER”, and both footer phrases
  add noise without helping a user complete a task.
- Several controls are roughly 30–35 px high, selected speed/stream buttons
  expose state only visually, tabs lack the expected arrow-key behavior, and
  the live media grid can announce a large batch of cards. White text on the
  orange primary button has only about 3.72:1 contrast.
- Audio with no artwork assigns an empty string to `<img src>`, producing a
  broken/current-page image request instead of a deliberate fallback.
- `ROOT_FOLDER = "64"` couples the browser to an internal protocol ID. The API
  already knows its real root and should be authoritative.
- Flat library requests walk, filter, deduplicate, and sort the entire catalog
  while holding its read lock for every page/search. The repository already has
  a SQLite-backed stable pagination path in `catalog_query.rs` that should be
  reused.
- A compatibility seek adds the exact start second to the cache key and starts
  an FFmpeg output that runs through the rest of the title. Repeated seeks can
  leave multiple large tail copies of one movie in the shared cache.
- Browser-compatible video keeps the source resolution and frame rate. A 4K/8K
  source can therefore remain too expensive to encode, transfer, or decode,
  even though the UI labels it “Compatible”. Codec support is guessed from a
  coarse container/video/audio summary and a hard-coded H.264 codec string.
- Sidecar subtitles are already indexed and safely served by rustyDLNA, but the
  web item contract and player do not expose them. Resume/bookmark data also is
  not used by the web player.
- Existing web-player tests largely search embedded HTML/JavaScript for exact
  strings. They prove that text exists, not that fallback, seeking, focus,
  history, responsiveness, or controls work in a browser.

## P0 — make playback controllable and truthful

### Playback session and state

- [x] Replace the collection of loosely related booleans with one explicit
  playback-session state model: `idle`, `loading`, `waiting`, `playing`,
  `paused`, `seeking`, `ended`, and `error`, plus direct/compatible source mode.
  Render controls and messages from that model rather than mutating them in
  unrelated event handlers.
- [x] Give each source load a monotonically increasing session/request ID and
  ignore every media, timer, probe, and promise callback from an older session.
  Dispose one-shot listeners and timers when the source changes.
- [x] Split “select a new library item” from “reload the same source at another
  offset/mode/track”. Only a new selection may bring the player into view;
  seeking, fallback, mode changes, and audio changes must retain scroll and
  focus position.
- [x] Respect `prefers-reduced-motion` when bringing a newly selected item into
  view, and do not center it if it is already visible.
- [x] Preserve paused/playing intent, global media time, playback rate, volume,
  mute, loop, caption, and selected audio track through every source restart.
- [x] Handle `play()` rejection. For autoplay rejection, keep the title ready
  and show a clear Play affordance; for other failures, transition to an error
  state instead of swallowing the promise.
- [x] Add `loadstart`, `waiting`, `stalled`, `seeking`, `seeked`, `canplay`,
  `playing`, `ended`, and `error` handling. Show a non-blocking stage spinner or
  status while work is in progress, without covering usable controls.
- [x] Keep “switching to compatible playback” visible until compatible media is
  actually playable. Do not let `start()` erase the message in the same event
  turn.

### Timeline, seeking, and end behavior

- [x] Clear the compatibility-seek status on `loadedmetadata`/`canplay`, error,
  or cancellation—not only on `playing`—so a paused seek completes visibly.
- [x] Prevent overlapping seek restarts and stale “Starting at…” listeners when
  the user scrubs repeatedly. Indicate the preview time while dragging and the
  committed time while the new segment starts.
- [x] For direct playback, use the media element's finite duration when catalog
  duration is missing. For compatibility playback, clearly disable seeking only
  when neither catalog nor probed duration is available.
- [x] Handle the last second and `ended` without clamping every request to
  `duration - 1`. Define replay and auto-advance behavior and keep the timeline
  at the real duration after completion.
- [x] Disable transport actions only when they truly cannot work, and give
  disabled Previous/Next controls an accessible explanation when more results
  have not yet been loaded.

### Fullscreen, touch, and keyboard control

- [x] Put the active transport, timeline, volume/mute, captions, audio, and exit
  controls inside the element that enters fullscreen. Both direct and
  compatible playback must remain fully controllable with mouse, touch, and
  keyboard in fullscreen.
- [x] Use one coherent control surface instead of showing native controls plus
  a duplicate side transport in direct mode and a different subset in
  compatible mode. If native controls are retained as a fallback, document
  exactly when they are used.
- [x] Make controls appear on pointer/touch activity and keyboard focus. Never
  hide a focused control, do not rely on hover/`mousemove`, and keep an always
  available way to exit fullscreen.
- [x] Remove the `Ctrl+F` override and restore browser Find everywhere. Scope
  playback shortcuts to a focused/hovered player or fullscreen mode rather than
  capturing the entire page.
- [x] Adopt familiar, documented shortcuts such as Space/K for play/pause,
  Left/Right or J/L for seeking, M for mute, and F for fullscreen. Never capture
  shortcuts in editable controls, never repurpose Up/Down page scrolling
  globally, and add a small shortcut-help dialog.
- [ ] Add touch-safe fullscreen controls and account for mobile safe-area
  insets. Verify rotation between portrait and landscape without losing the
  playback session.
  Remaining: run an explicit portrait-to-landscape session-retention check on
  a touch device; the safe-area and touch control work is complete.

### Errors, recovery, and server state

- [x] Define user-facing error categories for missing media, unsupported direct
  playback, transcoding disabled, transcode queue busy, transcode failure,
  network/offline failure, and browser-policy failure. Do not show raw HTTP
  status prose as the primary explanation.
- [x] Give recoverable errors an action: Retry, Try compatible playback, Play
  original, or Return to library as appropriate. An error from an old source
  must never replace the current title's state.
- [x] Make the connection/library indicator accurate: connecting, ready,
  degraded/error, and rescanning/empty if that information is available. Never
  use a green dot before a successful response; include a Retry action after a
  load failure.
- [x] Keep technical transcode details available in a disclosure/debug area,
  while the primary status uses plain language such as “Preparing video”.

### Immediate accessibility and responsive fixes

- [x] Implement the media-view tabs as a complete tabs pattern: associated
  tabpanel, roving `tabindex`, Left/Right/Home/End behavior, and deterministic
  focus after activation. Alternatively use ordinary links/buttons and remove
  incomplete tab roles.
- [x] Mark speed and stream choices as real radio groups or expose
  `aria-pressed`; synchronize the accessible state on every native and custom
  change. Give PiP, fullscreen, fill/fit, loop, and mute accurate state/action
  labels.
- [x] Give the timeline a media-neutral label, `aria-valuetext` with current and
  total time, and an accessible busy description during compatible seeks.
- [x] Use a concise live status region for load/playback updates, `role="alert"`
  for blocking failures, and remove `aria-live` from the card container so a
  full page of media is not re-announced.
- [x] Add visible `:focus-visible` styles to every interactive element and make
  touch targets at least 44 by 44 CSS pixels without requiring tiny text.
- [x] Fix normal-size text contrast, including the white-on-orange primary
  control and search placeholder, then verify all states rather than only the
  default palette.
- [ ] Fix the 761–1150 px layout. Let stage width determine a stable video
  aspect ratio, stack settings before either column becomes unusably narrow,
  and avoid a hard 470 px minimum at tablet widths. Test from 300 px through
  ultrawide layouts and at 200% browser zoom.
  Remaining: run explicit 300 px and ultrawide viewport checks; the responsive
  layout, mobile matrix, and 200% zoom coverage are complete.
- [x] Keep focus in a logical place after folder navigation, search completion,
  errors, and dialogs. Loading more items must not reset scroll or focus.

## P1 — complete the player experience

### Captions and audio tracks

- [x] Add caption descriptors to the typed web-item response: stable index,
  label, inferred language from filename suffix where available, default flag,
  source format, and a same-origin URL. Do not expose filesystem paths.
- [x] Serve browser captions as UTF-8 WebVTT. Pass through valid `.vtt` and
  safely convert supported SRT/ASS/SSA/SMI inputs; reject malformed or oversized
  input with a structured error. Decide explicitly how unsupported bitmap SUB
  captions appear in the selector.
- [x] Add an Off/default caption selector, attach `<track>` elements to video,
  retain the selection across direct/compatible restarts and seeks, and expose
  caption controls in fullscreen.
- [x] Preserve useful styling where practical, provide readable default caption
  styling, and test multiline cues, Unicode, overlapping cues, language
  variants, malformed files, and sidecar path-jail behavior.
- [x] Show an audio-track loading state and retry enrichment after a temporary
  probe failure. Keep the old selection on failure and explain that changing
  tracks requires compatible playback before silently switching global stream
  preference.
- [x] Persist language/title/default disposition in the scan data so ordinary
  playback does not require a synchronous FFprobe just to label tracks.

### Resume, queue, and platform integration

- [x] Add per-item resume state with throttled writes plus `pagehide`/ended
  handling. Decide and document whether web progress is browser-local or a
  separate server-side web profile; do not silently overwrite Kodi's bookmark
  identity.
- [x] On a partially watched title offer Resume at `H:MM:SS` and Start over.
  Clear trivial opening positions and near-end positions, and begin a compatible
  transcode at the saved offset rather than loading from zero first.
- [x] Add a Continue watching view and a clear-progress action once resume data
  exists. Mark completion consistently and test reload/crash/private-storage
  failure paths.
- [x] Build a stable playback queue snapshot from the active folder/search
  order. Previous/Next and optional auto-advance must cross API page boundaries,
  not stop at the first 60 loaded cards; show enough queue context to make the
  actions predictable.
- [x] Decide whether changing folder/search mutates the current queue. Prefer
  preserving the queue that started playback until the user starts another
  item or explicitly clears it.
- [x] Integrate the Media Session API when available: title, artist/show,
  album/artwork, duration/position, and play/pause/seek/previous/next handlers.
  Keep ordinary playback working when the API is unavailable.
- [x] Consider Screen Wake Lock during active fullscreen video and release it on
  pause, end, visibility loss, or error. Treat denial as normal, not an error.

### Library and copy cleanup

- [x] Use `metadata_title`/NFO/tag title as the primary display and document
  title. Show the filename only as secondary technical information when it adds
  value. Add show/season/episode, artist/album, date, duration, and resolution in
  a consistent item model.
- [x] Remove the full-title hover overlay that duplicates the title and covers
  art. Let titles wrap to a bounded number of lines and provide an accessible
  details affordance for long names on touch and keyboard.
- [x] Reduce the idle state to one useful prompt. Remove “Video and audio play
  here, without leaving the page”, the redundant side-panel prompt, decorative
  “PLAYER” copy, and the footer's “rustyDLNA web player / Embedded in the server
  binary” implementation trivia.
- [x] Keep one Now playing label/title, not several competing copies. On narrow
  screens prioritize the title and player actions over server branding/status
  links.
- [x] Move Direct/Compatible and volume boost above 100% into an Advanced
  settings disclosure. Auto should be the low-friction default; explain CPU,
  startup, quality, and support tradeoffs before a user forces a mode.
- [x] Replace or remove the unexplained `COMPAT` card badge. If retained, use a
  plain-language tooltip/details label and do not imply that browser support is
  known with certainty.
- [x] Add a deliberate no-art fallback for video and audio. Do not assign
  `img.src = ""`, request the current page as an image, or paint the generic
  music note over valid album artwork.
- [x] Add useful sort choices (title, recently added/date, and episode/track
  order where relevant) and preserve them in navigation state. Search results
  should state their query and result count.

## P1 — architecture and backend

### Frontend structure

- [x] Split the 921-line script into small dependency-light modules (or equally
  clear internal units): API client, library/navigation, playback session,
  media controls, preferences/resume, and DOM rendering. Keep the no-runtime-
  dependency deployment model; a frontend framework is not required.
- [x] Keep mutable state in one store/controller with explicit actions and
  derived selectors. UI rendering must not be the hidden owner of playback
  policy.
- [x] Extract and unit-test pure functions for source choice, duration/time
  conversion, queue navigation, resume thresholds, track labels, error mapping,
  and URL state.
- [x] Centralize DOM lookups and assert required elements once at startup.
  Centralize timers/listeners so teardown on source or navigation change is
  reviewable.
- [x] Use `AbortController` for library and item fetches. Cancel the pending
  search debounce on tab/folder/popstate changes and make loading state belong
  to the active request only.
- [x] Replace the manual `?v=fullscreen-19` asset suffix and source-string tests
  with a build/version-derived content hash or deliberate revalidation policy.
  Do not require developers to increment a UI nickname in HTML and tests.

### Typed API contract and catalog performance

- [x] Replace ad hoc `serde_json::json!` response construction with serializable
  DTOs and contract tests. Define one versioned item schema used by folder and
  flat-library responses, with explicit optional fields and structured errors.
- [x] Return root folder identity and web capabilities from the server; remove
  the browser's hard-coded `"64"`. Capabilities should cover transcoding,
  captions, available quality profiles, resume, and any disabled feature the UI
  must explain.
- [x] Validate `view`, `kind`, pagination, item IDs, audio indexes, and start
  offsets strictly. Reject invalid values consistently instead of treating
  unknown values as “all” or accepting an ID with arbitrary trailing text.
- [x] Reuse the existing SQLite-backed catalog query/pagination machinery for
  flat browsing and search. Do not sort the whole in-memory catalog under its
  read lock for every page; keep the documented memory fallback if the database
  query is unavailable.
- [x] Make pagination stable across catalog generations (cursor or explicit
  generation token plus deterministic sort). Prevent duplicate/missing queue
  entries when the scanner changes the library between pages.
- [x] Add an API/cache strategy for repeated search and metadata reads (catalog
  generation/ETag is sufficient). Keep media URLs and filesystem paths
  same-origin and jailed as they are now.

### Compatibility stream design

- [x] Stop retaining one full remaining-title transcode per exact seek second.
  Design seek output as bounded/reusable segments or explicitly ephemeral
  session artifacts, deduplicate nearby seeks, and remove abandoned partial and
  seek-specific outputs promptly. Preserve the current “last reader cancels the
  producer” behavior.
- [x] Add compatible quality profiles with explicit maximum resolution, frame
  rate, H.264 level/profile, pixel format, bitrate/CRF, AAC channel layout, and
  bandwidth expectations. Auto preserves source resolution up to 4K30; compact
  explicit 1080p and 720p choices remain available in Advanced settings.
- [x] Improve direct-play capability data. Persist actual codec profile/level,
  pixel format/bit depth, audio codec/layout, and a correct RFC 6381 codec string
  where possible. Do not claim every WebM or H.264/AAC MP4 is equivalent, and
  allow genuinely browser-supported FLAC/WAV/HEVC paths to be tested without an
  unconditional server veto.
- [x] Make “Compatible” truthful for HDR, very high resolutions, high frame
  rates, and multichannel audio. Define when tone mapping, downscaling, or
  stereo downmix occurs and expose that result in the technical details.
- [x] Expose machine-readable transcode admission/preparation state so the UI
  can distinguish queued, starting, producing, cancelled, and failed jobs.
  Include Retry-After where meaningful without leaking raw helper output.
- [x] Add web-player metrics/log fields for selected source mode, fallback
  reason, startup-to-first-playable latency, seek restart, cancellation, cache
  reuse, and failure category. Never log full media paths to browser-visible
  responses.

## P2 — useful polish after P0/P1

- [x] Add an item-details view that uses existing NFO/tag fields (plot,
  show/episode, genre, year, technical media facts) without crowding every card.
- [x] Support a deep link for an item and optional start time while retaining
  folder/view navigation. Validate that link against the catalog and provide a
  helpful missing-item state.
- [x] Add chapters when chapter metadata is available: chapter markers on the
  timeline, a keyboard/touch list, and chapter-aware Media Session actions.
- [x] Offer caption size/background preferences and persist them accessibly.
- [x] Add optional autoplay as an explicit preference, defaulting to off until
  queue behavior and resume semantics are tested.

## Tests and completion gates

- [x] Add browser behavior tests rather than source-string assertions. Run the
  fixture server and exercise at least Chromium, Firefox, and WebKit (or clearly
  document a smaller supported matrix) at desktop and mobile viewports.
- [x] Cover direct success, direct error then automatic compatible fallback,
  forced Direct, forced Compatible, transcoding disabled, autoplay rejection,
  transcode queue busy/failure, missing file, offline library request, rapid
  item switching, repeated seeks, paused seek, track switch, end/replay, and
  stale-event suppression.
- [x] Cover folder/filter/search/load-more/popstate interactions, including a
  search debounce followed immediately by navigation and a catalog generation
  change between pages.
- [x] Cover fullscreen in direct and compatible modes with mouse, touch, and
  keyboard. Assert that controls remain reachable, Escape exits, browser Find
  still works, and arrow keys can scroll the library outside the player.
- [ ] Add accessibility automation plus a manual keyboard/screen-reader pass.
  Assert names, roles, states, focus order/restoration, live-region behavior,
  contrast, 44 px targets, 200% zoom, reduced motion, and no hover-only action.
  Remaining: perform the manual screen-reader pass; axe, keyboard, focus,
  contrast, target-size, zoom, reduced-motion, and hover-independent checks are
  automated.
- [ ] Test captions end to end for VTT and converted formats, language labels,
  selection persistence, fullscreen, compatible seeks, malformed/oversized
  input, Unicode, and path traversal/symlink races.
  Remaining: add explicit SMI/SSA conversion cases and select captions while
  fullscreen; the core conversion, persistence, compatible-restart, Unicode,
  malformed input, size limit, and path-jail cases are covered.
- [x] Test resume and queue behavior across reload, private/blocked storage,
  pagination, completion thresholds, Start over, search/folder changes, and
  compatible playback from a nonzero offset.
- [x] Add Rust contract tests for all DTOs, validation errors, capabilities,
  root identity, stable pagination/generation handling, caption endpoints, and
  machine-readable transcode status.
- [x] Add a large-library web-query benchmark using the existing 50k-file
  fixture. Set a repeatable latency/lock-hold target and prove that later pages
  do not re-sort/materialize the entire catalog.
- [x] Add a seek/cache stress test that performs many nearby and distant seeks,
  verifies producer cancellation/deduplication, and proves cache growth is
  bounded independently of “number of exact seconds clicked”.
- [ ] Manually verify representative media: MP4 H.264/AAC, HEVC where supported,
  WebM, MKV with multiple audio tracks and captions, HDR10, Dolby Vision,
  MP3/AAC, FLAC/WAV, no-audio video, no-duration media, missing artwork, and a
  corrupt/truncated file.
  Remaining: complete real-browser/device playback checks, including the
  no-duration and missing-artwork cases. Probe/fixture audits cover the other
  listed codec, container, HDR, audio-track, caption, and corrupt-file cases.
- [x] Update `docs/WEB_PLAYER.md`, README capability text, configuration docs,
  and asset-cache behavior after implementation. Document shortcuts, stream
  modes/quality, captions, resume storage, queue/autoplay, supported browsers,
  and recovery actions accurately.
- [x] Remove brittle assertions for exact copy, CSS cachebuster nicknames, and
  implementation snippets once equivalent behavior/contract tests exist.

## Explicit non-goals for this plan

The trusted-LAN security model, no-account design, server-side asset embedding,
and path-jail/CSP protections are deliberate and should remain. This plan does
not require a frontend framework, cloud service, DRM, upload/import UI, or
built-in Internet-facing authentication/TLS. If remote access is desired, it
remains the reverse proxy's responsibility as documented today.
