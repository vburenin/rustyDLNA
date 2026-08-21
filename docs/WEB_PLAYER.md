# Embedded web player

rustyDLNA includes a responsive, same-origin media player at `/`. Its HTML,
CSS, and dependency-free JavaScript modules are embedded in the Rust binary;
there is no runtime asset directory or separate frontend build.

```toml
[web]
enable = true
encoder = "libx264" # or "h264_nvenc"

[transcode]
enable = true
max_jobs = 2
cache_max_mb = 51200
```

The operator page remains at `/status`. When `web.enable = false`, `/web/*`
and `/api/web/*` return 404 and `/` serves the operator page. Disabling
`transcode.enable` does not disable the player: Original playback remains
available, while Compatible is disabled and returns a structured error.

## Library and player behavior

Folders follows the physical media tree. All media, Videos, and Audio use the
SQLite-backed searchable catalog, with a bounded in-memory fallback if the
database is unavailable. The active view, folder, search, sort, selected item,
and optional start time are represented in the URL, so Back/Forward and links
such as `/?view=video&item=42&t=125` work.

Metadata titles from NFO files or tags are primary. The filename is shown only
as secondary information when it differs. Details exposes the plot and indexed
descriptive and technical fields without crowding the card. Missing artwork has
an intentional fallback and never creates an empty image request.

Selecting an item takes a snapshot of the complete active folder/search order,
including later API pages. Previous and Next use that snapshot even after
library navigation. Queue position is shown next to Now playing. Optional
auto-advance is off by default and can be enabled under Advanced playback.

The player uses one custom control surface for Original and Compatible media.
The timeline, transport, volume, captions, audio and chapter selectors, and
fullscreen exit remain inside the fullscreen element. Controls have keyboard
focus styles and touch-sized targets, account for mobile safe-area insets, and
remain reachable if the Advanced section makes the fullscreen surface taller
than the screen.

Supported controls include play/pause, previous/next, mute and volume, speed,
loop, fit/fill, picture-in-picture,
fullscreen, captions, audio tracks, chapters, stream mode, and compatible
quality. A source restart preserves playback intent, global time, rate, volume,
mute, loop, caption choice, and audio choice. Only selecting a different title
may scroll the player into view, and reduced-motion preferences are honored.

The player information button shows the original container, video encoding,
and selected audio track beside the active browser output. Compatible playback
asks the browser about video and audio independently with Media Capabilities
and `canPlayType`; either positive result permits stream copy because deployed
browsers can disagree for HEVC. The information dialog shows the exact codec
candidate and both probe results. In Auto, supported H.264 or HEVC video and
supported AAC, AC-3, E-AC-3, or MP3 audio are copied unchanged into fragmented
MP4. Only unsupported streams are re-encoded. Explicit 1080p or Data saver
quality still re-encodes video to apply the requested output envelope.

The scanner also samples presentation and decode timestamps before approving
H.264/HEVC stream copy. Some malformed MP4 files contain reordered frames but
store every presentation timestamp in decode order; copying those packets
causes a visible forward/back cadence even though decoding reports no error.
Compatible Auto marks those files for frame-order repair. An HEVC-capable
browser receives an HEVC Main 10 repair encode through `hevc_nvenc`, preserving
HDR10 and mastering metadata; otherwise the portable H.264 path is used. The
information dialog states that a repair encode is active and why it is needed.

When available, Media Session receives title, artist, album, artwork, duration,
position, and transport handlers. Fullscreen video requests Screen Wake Lock
while playing and releases it on pause, end, error, visibility loss, or exit;
unsupported or denied platform APIs are nonfatal.

## Keyboard shortcuts

Shortcuts apply only while the player is focused or hovered, or while it is in
fullscreen. They are not captured in inputs, selects, text areas, or buttons.

| Key | Action |
|---|---|
| `Space` or `K` | Play or pause |
| `Left` or `J` | Back 10 seconds |
| `Right` or `L` | Forward 10 seconds |
| `M` | Mute or unmute |
| `F` | Enter or exit fullscreen |
| `?` | Open shortcut help |
| `Escape` | Exit fullscreen or close a dialog |

`Ctrl+F` and `Command+F` retain the browser's Find behavior. Up and Down keep
their normal page-scrolling behavior outside editable controls.

## Stream modes and quality

- **Auto** asks the browser about the indexed MIME and RFC 6381 codec string.
  It starts Original when the complete source is supported and retries once
  with Compatible if the media element still fails. Compatible then negotiates
  the exact MP4 video and selected audio candidates independently.
- **Original** serves the jailed source file with byte-range support. It keeps
  source quality and consumes no encoder slot, but success depends on that
  browser and platform's container, video, and audio codecs.
- **Compatible** produces a growing fragmented MP4. Each browser-supported
  stream is copied; unsupported video becomes H.264 and unsupported audio
  becomes AAC. Copied AAC is normalized for MP4 with `aac_adtstoasc` without
  re-encoding it. Malformed reordered timestamps force a video repair encode;
  this is the one case where a browser-supported video codec is deliberately
  not copied. A failed negotiated copy automatically retries with portable
  H.264/AAC. It may start more slowly and uses an encoder slot while remuxing
  or transcoding.

When a stream must be encoded, compatible profiles are advertised by the API
rather than hard-coded in the UI:

| Profile | Video | Audio | Approximate peak bandwidth |
|---|---|---|---|
| Auto | H.264 High 5.1, yuv420p, source resolution up to 3840×2160 and 30 fps, CRF 20, 25 Mbps cap | AAC stereo, 192 kbps | 25.45 Mbps |
| 1080p | H.264 High 4.1, yuv420p, at most 1920×1080 and 30 fps, CRF 22, 8 Mbps cap | AAC stereo, 192 kbps | 8.45 Mbps |
| Data saver | H.264 Main 3.1, yuv420p, at most 1280×720 and 30 fps, CRF 25, 3 Mbps cap | AAC stereo, 128 kbps | 3.38 Mbps |

Scaling never enlarges the source. Auto therefore keeps a 3840×2160 source at
4K, keeps lower-resolution sources at their original dimensions, and limits
larger sources to 4K. The explicit 1080p and Data saver profiles reduce encoder
load and bandwidth when desired. Supported HDR10 and Dolby Vision profile 8
video can be copied unchanged; video that must be transcoded is tone-mapped to
BT.709 SDR with `libplacebo`. Multichannel audio is downmixed to stereo. The
technical/operator logs record the selected mode, fallback
reason, quality, encoder, source HDR state, tone mapping, audio index, start
offset, cache reuse, cancellation, failures, and startup-to-first-playable
latency without putting source filesystem paths in browser responses.

Compatibility jobs reuse the existing bounded helper/job gate, runtime
deadline, cache-size and age limits, cancellation, process reaping, and
finished-file verification. Concurrent equivalent requests share a job. A
zero-offset completed stream can be reused from cache. Seek restarts are
session artifacts: the player coalesces rapid scrubs and explicitly cancels a
producer when a new seek supersedes it. An unintentional dropped connection
still gets a 30-second reconnect window, and reopening the same source attaches
to that producer. Completed
nonzero-offset output remains reconnectable for 30 seconds after its final
reader and is then removed, so repeated exact seeks cannot retain movie-length
cache tails. When only one stream is copied, nonzero-offset jobs preserve the
same keyframe preroll for both streams so a seek does not begin with silent
video or audio over a blank frame.

For NVIDIA acceleration, set `web.encoder = "h264_nvenc"` and expose the GPU.
An NVIDIA Container Toolkit Compose override can include:

```yaml
services:
  rusty-dlna:
    gpus: all
    environment:
      NVIDIA_DRIVER_CAPABILITIES: compute,video,utility,graphics
```

H.264 and HEVC sources use CUDA decode/scaling where available. AAC remains on
the CPU, and Dolby Vision/HDR tone mapping requires the `graphics` capability.
If hardware preparation fails before the first playable fragment, the job
retries with software decode and `libx264`. `rusty-dlna --config ... --check`
validates the selected encoder on the deployed host.

## Captions, audio tracks, chapters, and resume

Indexed sidecar `.vtt`, `.srt`, `.ass`, `.ssa`, and `.smi` captions are exposed
with stable indexes, labels, inferred filename language, source format, and a
same-origin URL. Text must be valid UTF-8 and pass the bounded sidecar read and
cue validation. VTT is normalized; the other supported formats are converted
to WebVTT. Malformed, oversized, unsupported, and path-jail failures return
structured errors. Bitmap `.sub` files remain visible as unsupported metadata
but cannot be selected. Captions default to Off and the selected caption,
caption size, and background preference survive source restarts.

Audio language, title, channel count, codec, default disposition, and chapters
are normally read from compact scan metadata. Legacy records can request a
strict, helper-admitted one-item enrichment probe. The UI shows loading and a
retry action if that probe fails. Selecting a different audio track explains
that Compatible playback is required.

Resume progress is browser-local in `localStorage`; it does not overwrite the
accountless Kodi/DLNA bookmark identity. Writes are throttled and flushed on
pause, explicit seeks, title/source changes, and `pagehide`. Positions before
30 seconds and positions within the last 120 seconds or final 5 percent are
discarded. A partially watched title offers Resume and Start over, appears in
Continue watching, and starts Compatible playback directly at the saved
offset. Blocked/private storage degrades without preventing playback.

## Status and recovery

The library indicator distinguishes connecting, ready, empty, and error states.
Loading, buffering, seeking, compatible preparation, and paused
autoplay-policy states are rendered from the active playback session. Every
source load has a monotonically increasing request ID, and callbacks, timers,
polls, or errors from an older session are ignored.

User-facing failures distinguish missing media, unsupported Original playback,
disabled Compatible playback, a busy transcode queue, cancelled/failed
transcoding, network/offline failure, and browser autoplay policy. Busy or
cancelled replacement streams retry automatically up to three times. Depending on
the category, recovery offers Retry, Try compatible, Play original, or Return
to library. Raw helper output is never primary copy; limited technical details
remain in a disclosure.

## Versioned API and caching

All JSON success and error documents include `schema_version: 1`. Errors use
`error.code`, `message`, `recoverable`, and optional `action`. Query names,
enum values, numbers, duplicates, percent encoding, UTF-8, and item-path IDs are
validated strictly.

| Route | Purpose |
|---|---|
| `/` | Player, or status page when the player is disabled |
| `/web/app.css` | Embedded stylesheet |
| `/web/{app,api,core,library,player,preferences,store}.js` | Embedded ES modules |
| `/api/web/library` | Versioned folder or flat-library page, server root, capabilities, generation, and item DTOs |
| `/api/web/item/{id}` | One item; `enrich=1` explicitly probes legacy stream metadata |
| `/api/web/transcode/{id}?request={request_id}` | GET returns request-scoped `queued`, `starting`, `producing`, `complete`, `cancelled`, or `failed` state; DELETE cancels an exclusively owned, superseded request |
| `/web/media/{id}.mp4?mode=direct` | Original jailed media with byte ranges |
| `/web/media/{id}.mp4?...` | Compatible stream with validated audio track, start, quality, negotiated `video_mode`/`audio_mode`, reason, and request parameters |
| `/Captions/{id}/{index}...?format=webvtt` | Jailed browser caption conversion |
| `/status` and `/api/status` | Operator status and metrics |

Library parameters are `view=folders|library`, `folder`,
`kind=all|video|audio`, `q`, `sort=title|date_desc|episode`, `offset`,
`limit`, and `generation`. The default page is 60 and the maximum is 200.
Passing the first page's generation on later pages gives stable pagination; a
catalog change returns `409 catalog_changed` rather than mixing snapshots.
The flat view queries SQLite with deterministic ordering and only materializes
the requested page.

Library and item responses use generation-based weak ETags with
`private, max-age=0, must-revalidate`. A matching `If-None-Match` receives 304.
Embedded assets deliberately use `Cache-Control: no-cache`, so browsers may
store them but must revalidate; HTML contains no hand-maintained cachebuster.
Media and captions keep their route-specific range and safety policies.

## Browser support and verification

The automated behavior suite runs desktop Chromium, Firefox, and WebKit plus a
mobile Chromium viewport. It covers source selection/fallback and error
recovery, session cancellation, seeks, fullscreen controls, keyboard scoping,
responsive/touch layout, captions, audio tracks, resume, queue pagination,
history/search/catalog-generation races, reduced motion, and axe accessibility
checks. Headless Linux WebKit lacks a working Fullscreen API, so that one API
exercise is skipped there; the same control-surface behavior is covered by the
other engines.

Original playback capabilities vary with the browser, OS, and installed media
framework. In particular HEVC, Dolby Vision, MKV, and some multichannel audio
paths must be treated as device-dependent; Auto falls back to Compatible when
the browser rejects them. Compatibility fixtures and Rust tests cover H.264/AAC
MP4, WebM/MKV and multi-audio metadata, captions, HDR10, genuine Dolby Vision
Profile 7, MP3/AAC/FLAC/WAV metadata, no-audio and no-duration records, missing
art/files, and malformed/truncated inputs.

The player retains rustyDLNA's trusted-LAN model. It adds no accounts, TLS, or
Internet-facing authorization. Put an authenticated TLS reverse proxy in front
if the service is exposed beyond a trusted network.
