# Embedded web player

rustyDLNA includes a responsive, same-origin media player at `/`. Its HTML,
CSS, and dependency-free JavaScript modules are embedded in the Rust binary;
there is no runtime asset directory or separate frontend build.

```toml
[web]
enable = true
encoder = "libx264" # or "h264_nvenc"
ai_upscale_max_jobs = 1

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

Library categories and playback routes use the canonical media kind, including
Ogg audio and RealMedia video whose MIME types begin with `application/`.
Embedded album covers remain artwork; they do not turn an audio file into video.

The desktop top bar is 32 pixels tall, with a matching sticky-player offset.
Touch devices retain larger header controls for reliable tapping.

Playback mode choices are **Automatic** (prefer the original; convert when
needed), **Original only** (no conversion or automatic fallback), and
**Prepared streaming** (server-prepared output that copies supported streams).
Hovering a mode shows a native explanatory tooltip; brief descriptions stay
visible and are associated with the radios for keyboard and screen-reader use.
The playback indicator reports the actual operation: Original file,
Repackaging, Converting audio, or Re-encoding video, with more detail on hover.
It shows Prepared streaming while codec negotiation is pending. The internal
Auto/Original/Compatible modes and saved preference values are unchanged;
Prepared streaming does not force video re-encoding. Technical references to
Compatible below refer to that same server-prepared delivery path.

The flat library's Title sort groups explicitly numbered movie files by their
canonical source collection folder, so genre symlink aliases share one group.
The scanner captures a root-qualified, validated source identity in the catalog
(stable across configured-root relocations); paging and
schema migration do not resolve filesystem links. Existing catalogs acquire
source identities during normal reconciliation, including unchanged files.
Collection names sort alphabetically alongside
standalone titles, and each collection's movies follow their numeric sequence.
A numbered movie has the filename form `NN - Title (YYYY) ...`; episode and
workout filenames without that movie year remain ordinary entries. Nested
collection folders form their own groups. The browser shows a heading for each
collection and joins its cards across page boundaries. Recently added,
episode/track ordering, physical folder browsing, and DLNA sorting retain their
existing behavior. No media filenames or metadata are rewritten.

Folders follows the physical media tree. All media, Videos, and Audio use the
SQLite-backed searchable catalog, with a bounded in-memory fallback if the
database is unavailable. Browse presents that library across the available
screen width; Watch presents the player together with the library. A page without a
selected item starts in Browse, while a link to an item starts in Watch.
Selecting a card also switches to Watch. Switching back to Browse keeps the
current media element and playback session alive, so returning through the Now
playing title does not reload or restart the source. Search, folder, loaded list,
selection, playback, and each mode's scroll position remain intact while the
presentation changes. Close player stops the current title, cancels any
compatible job, and returns to Browse with the library focused. It remains on
the player during resume, inline Watch, fullscreen, and iPhone expanded
playback so leaving a movie does not require finding Browse in the header.
Closing stops media and cancels its work immediately, even while the browser
is still completing a fullscreen or picture-in-picture exit.
Each video card's Details dialog contains a deliberately secondary “Download
original” action. It downloads the indexed source file without starting or
changing playback; audio details do not advertise that action. Stream information
also offers Download original for the current video, using the same download
endpoint without restarting or changing playback.
Empty searches offer Clear search without changing the current view or playback.
An empty Continue watching view explains that progress is saved in this browser.

On short touch-screen landscape viewports, Watch keeps the compact application
header in the page and places the 16:9 player beside an independently scrolling
library. The layout follows Android's visible viewport as browser controls change
size. Only an explicit full-screen action expands the player over the complete
visible screen.

The active view, folder, search, sort, selected item, and optional start time are
represented in the URL, so Back/Forward and links such as
`/?view=video&item=42&t=125` work. The `layout` query appears only when the
chosen presentation differs from its default: for example,
`/?view=video&item=42&layout=browse` keeps a selected title while showing the
full library, and `/?layout=watch` opens an empty player. Presentation changes
replace the current history entry rather than adding Back-button stops.
Navigation uses a request epoch: when newer Back/Forward or in-page navigation
or selection supersedes a linked title that is still loading or being enriched,
the older request cannot select, focus, message, or start that title. Linked
item details load alongside the library request. Playback waits only for the
validated first page's capabilities and item metadata from that same catalog
generation. Later library pages continue in the background; a failure on a later
page offers library retry without preventing linked playback. A linked selection
keeps a singleton queue. Card selection retains the complete loaded-list snapshot.
A changed generation causes a guarded item refetch; a superseded navigation
cannot start playback or move focus. Queue changes also update the URL,
page title, and current library card without adding history entries.

Metadata titles from NFO files or tags are primary. The filename is shown only
as secondary information when it differs. For videos, Details exposes the
spoiler-safe NFO `<outline>` as About and keeps the full NFO `<plot>` behind an
explicit “Reveal full plot (spoilers)” disclosure. A plot is never substituted
for a missing outline. Other media comments remain visible as About. Indexed
descriptive and technical fields stay off the card. Missing artwork has an
intentional fallback and never creates an empty image request.
Folder cards use the same 2:3 portrait artwork dimensions as movie posters,
with a centered folder icon and item count.
Artwork uses four concurrent loading slots; leaving a view releases its pending
images so a slow response cannot block artwork in the new view.

The browser loads all metadata for the current folder or search before displaying
its cards. It assembles one catalog generation using batches of up to 200 entries
and at most four concurrent requests. The complete list stays in browser memory;
scrolling loads only nearby posters and does not insert cards or adjust the scroll
position. Changing views cancels unfinished metadata requests, and a failed batch
offers Retry without exposing an incomplete list.
For lists of at least 500 entries, browsers supporting `content-visibility`
skip offscreen rendering in batches of cards. Card creation yields between
bounded private batches, then publishes the complete list at once. Navigation
cancels unfinished construction. Each batch reserves its measured height;
matching chunk layouts reuse measurements at up to two widths for the current
list. Titles remain available to browser Find and assistive technology. Poster
images occupy reserved space and do not change card geometry as they arrive.

Selecting an item takes the complete active folder/search order directly from
that in-memory list. Previous and Next use that snapshot even after library
navigation, without additional queue requests. Queue position is shown next to
Now playing. Queue-position lookup is indexed by the immutable queue snapshot;
playback clock ticks update time, chapter, and position controls without
rebuilding the full player control state. Optional auto-advance is off by default
and can be enabled under Advanced playback.

The player uses one custom control surface for Original and Compatible media.
The close control, timeline, transport, volume, captions, audio and chapter
selectors, and fullscreen exit remain inside the fullscreen element. Fullscreen
locks page scrolling. A pointer-initiated Safari fullscreen transition discards
incidental focus that WebKit assigns to the fullscreen element, while genuine
keyboard focus continues to keep controls visible. Controls have keyboard focus
styles and touch-sized targets, account for mobile safe-area insets, and remain
reachable if the Advanced section makes the fullscreen surface taller than the
screen. Player settings, quality, stream information, keyboard help, and item
details use the same top-right X action. When a dialog is taller than the
viewport, its contents scroll beneath a pinned heading so that close action
remains visible. Long titles and metadata wrap within the dialog; exceptionally
tall titles scroll independently so the close action stays reachable. Settings
show selected stream modes, Loop, and Fill frame consistently. Stream modes
include short explanations, and Playback quality explains that a specific
quality switches to Prepared streaming. Settings use a compact two-column
selector grid and a single row of stream modes (stacked on the narrowest
screens), with 44-pixel control targets.
Quality choices also use two columns where space permits. Stream information
keeps source and output facts compact and puts advisory browser probes behind
an initially collapsed Browser diagnostics disclosure; no diagnostic data is
removed. Recovery messages and actions remain inside the fullscreen or expanded
player, and errors keep the playback and close controls visible. Closing clears
loading and error UI as well as the source.
Timeline and volume inputs keep at least a 44-pixel hit area
even though their visible tracks remain thin. Coarse-pointer devices enlarge
the timeline thumb and expose a 52-pixel-high horizontal drag area without
making the track visually heavy. At the supported 300-pixel minimum
width, the transport and settings controls wrap onto separate rows so Previous
and Next remain visible and keyboard reachable. On portrait phones, the
toolbar matches landscape Watch with the player beside the library: Previous,
Next, and stream information stay off the row so the time label does not
overlap the remaining 44-pixel controls. Queue changes remain available from
the library and Media Session. Touch-first phones and tablets hide the in-player
volume slider, including fullscreen and iPhone expanded playback: iOS ignores
element volume, and hardware buttons already control loudness. Mute stays on
the toolbar. A touch keeps the controls
visible for at least five seconds after the last interaction, including after
the synthetic pointer-leave event emitted by mobile browsers; mouse controls
retain their shorter idle timeout and immediate pointer-leave behavior. The
top-left close action fades with the rest of the controls and remains visible
when keyboard focus pins the control surface.

Supported controls include close player, play/pause, previous/next, mute and
volume, speed, loop, fit/fill, picture-in-picture,
fullscreen, captions, audio tracks, chapters, stream mode, and compatible
quality. The player toolbar shows the active transcoded-quality shortcut; it
opens a dedicated, scrollable chooser with every advertised profile, including
480p and 360p. Playback settings retains the same selector.
The saved speed applies from the first presented frame and survives source
loads, buffered seeks, compatible restarts and pause/resume.

Playback settings also offers a browser-local **Encoding preset**, independent
of quality/resolution: **Balanced** (default), **Fast start** (less encoder
buffering), and **Maximum speed** (faster encoding with a larger quality
tradeoff). The latter two are experimental choices, not guaranteed startup
times; browser buffering, source keyframes, decoding and storage still matter.
Changing this setting restarts currently re-encoded Compatible video at the
same position and playback intent, keeping quality, HDR selection and tracks.
Original playback, copied video and audio-only playback are not restarted.
The selection is retained through seeks and codec recovery, and Stream
information reports the active preset or that video is not re-encoded.
Older servers without `capabilities.encoding_presets` hide the selector and
retain Balanced requests. The media API accepts `encoding_preset=balanced`,
`fast_start`, or `maximum_speed`, defaults to Balanced, and rejects invalid
or duplicate values. Distinct encoded presets have distinct cache identities;
copied-video output and existing Balanced caches are unchanged.

On touch
screens, two taps on the right half of the video seek forward
30 seconds and two taps on the left half seek backward 30 seconds. A single
video-surface tap only reveals the controls; touch play/pause remains on its
explicit button so a missed double tap cannot pause playback. In fullscreen,
the empty area of the visible control overlay remains part of that video
gesture surface, while buttons, sliders, menus, and other interactive controls
keep their normal touch behavior. A source restart preserves playback intent,
global time, rate, volume, mute, loop, caption choice, and audio choice.
Play and Pause remain authoritative during capability checks and recovery,
including commands from Media Session. Changing quality, audio, or stream mode
while loading preserves that intent. Stream-information rows remain stable
during clock updates so reading or selecting their text is not interrupted.
Loop repeats the whole title, including after a compatible seek, and takes
precedence over queue auto-advance.
While playing media is loading, buffering, or being replaced after a seek, the
transport continues to show Pause; seeking while paused continues to show Play.
Selecting a different title stops and clears the previous media immediately,
resets captions to Off, and is the only action that may scroll the player into
view.
Reduced-motion preferences are honored. Desktop and iPad browsers use element
fullscreen. On iPhone, the same button expands the in-page player over the
visible viewport so the custom timeline, captions, and playback controls remain
available; Safari's browser chrome may remain because this is not native video
fullscreen. The expanded surface follows Visual Viewport size and position
changes as browser chrome moves or the device rotates. While an expanded video
is actively playing and the page is visible, the player requests a Screen Wake
Lock and releases it on pause, exit, playback failure, or page hide. Wake
prevention remains subject to browser and device policy. On touch-first Android
phones in landscape, inline Watch mode follows the browser's measured visible
viewport rather than deriving player height from the wide screen or a cached
CSS viewport unit. Its compact header overlays the video, retaining Browse and
Watch access while letting the player use the complete visible height. The
bounded surface and control gradient keep the 44-pixel controls visible without
scroll-induced vertical jumping or clipping the lower video letterbox. Android
element fullscreen also binds its inner media and control planes to the Visual
Viewport size and position, so rotation, browser UI, display cutouts, and system
bars cannot stretch the picture past the visible landscape boundaries. Browsers
without either supported presentation are given a disabled control; a rejected
display-mode request never changes playback into an error.

The player information button shows the original container, video encoding,
and selected audio track beside the active browser output. Compatible playback
asks the browser about video and audio independently with Media Capabilities
and `canPlayType`; either positive result permits stream copy because deployed
browsers can disagree for HEVC. An advertised HEVC Main 10/HDR10 transcode
candidate is probed separately with its exact RFC 6381 codec, requested profile
dimensions, frame rate, bitrate, PQ transfer function, and Rec. 2020 gamut. The
player also records the CSS dynamic-range media feature for diagnostics. It is
not a hard veto: Safari can decode an HDR rendition and tone-map it when the
current display is reported as standard range. Media Capabilities is advisory
and bounded to one second; a missing response falls back to the synchronous
`canPlayType` result instead of delaying the first media request indefinitely. Only completed
probes enter a bounded browser-session cache, so one permanently pending probe
cannot poison later titles with the same codec configuration. The
information dialog shows the exact codec candidate and both probe results. In
Auto, supported H.264 or HEVC video and
supported AAC, AC-3, E-AC-3, or MP3 audio are copied unchanged into fragmented
MP4. Only unsupported streams are re-encoded. Any explicit quality still
re-encodes video to apply the requested output envelope.

The scanner also samples presentation and decode timestamps before approving
H.264/HEVC stream copy. Some malformed MP4 files contain reordered frames but
store every presentation timestamp in decode order; copying those packets
causes a visible forward/back cadence even though decoding reports no error.
Compatible Auto marks those files for frame-order repair. When the configured
browser encoder is `h264_nvenc`, more-than-8-bit HEVC HDR10 and Dolby Vision
Profile 8 sources use an HEVC Main 10 repair encode through `hevc_nvenc`,
preserving HDR10 color signaling. Other encoder configurations, Dolby Vision
Profile 5, Profile 7, and other HDR repairs use the configured H.264 path with
SDR tone mapping. If advisory browser support for the HEVC repair proves wrong
at playback time, the player retries once with portable H.264 and AAC. The
information dialog states that a repair encode is active and why it is needed.

When available, Media Session receives title, artist, album, artwork, duration,
position, and transport handlers. Fullscreen or expanded video requests Screen
Wake Lock while playing and releases it on pause, end, error, source replacement
or cancellation, visibility loss, or exit. A request that finishes after one
of those transitions is released instead of being retained; unsupported or
denied platform APIs are nonfatal.

## Keyboard shortcuts

Shortcuts apply only while the player is focused or hovered, or while it is in
fullscreen. They are not captured in inputs, selects, or text areas. Transport
shortcuts leave buttons alone. Open modal dialogs own their keyboard input, so
Escape closes the dialog without changing playback or display mode. Escape in
the captions popup closes it and restores focus to Captions before exiting
fullscreen or closing the player. Clicking outside that popup also dismisses it.

| Key | Action |
|---|---|
| `Space` or `K` | Play or pause |
| `Left` or `J` | Back 10 seconds |
| `Right` or `L` | Forward 10 seconds |
| `M` | Mute or unmute |
| `F` | Enter or exit fullscreen |
| `?` | Open shortcut help |
| `Escape` | Exit fullscreen, close a dialog, or close the player |

`Ctrl+F` and `Command+F` retain the browser's Find behavior. Up and Down keep
their normal page-scrolling behavior outside editable controls.

## Stream modes and quality

- **Automatic** (internal Auto mode) asks the browser about the indexed MIME
  and RFC 6381 codec string.
  It starts Original when the complete source is supported and retries once
  with Compatible if the media element still fails. When an exact video codec
  string is unavailable for a format that normally needs conversion, Auto does
  not treat the browser's broad container-only answer as proof that the video
  can decode. Compatible then negotiates the exact MP4 video and selected audio
  candidates independently.
- **Original only** (internal Original mode) serves the jailed source file with
  byte-range support. It keeps
  source quality, HDR/Dolby Vision metadata, and bitrate without transcoding or
  an encoder slot, but success depends on that
  browser and platform's container, video, and audio codecs.
- **Prepared streaming** (internal Compatible mode) produces a growing
  fragmented MP4. Each browser-supported
  stream is copied; unsupported video normally becomes H.264 SDR and
  unsupported audio becomes AAC. Copied AAC is normalized for MP4 with
  `aac_adtstoasc` without
  re-encoding it. Malformed reordered H.264/HEVC timestamps use the dedicated
  video repair mode; malformed codecs that cannot use that stream-aware mode
  use the normal portable transcode instead. A failed negotiated copy
  automatically retries with portable
  H.264/AAC. If that portable rendition is still rejected with a decode or
  source-support error while quality is Auto, the player retries once at the
  server-designated automatic fallback quality without changing the saved
  preference. An explicit quality is never silently lowered; its failure is
  shown so the user can choose another profile. Compatible playback may start
  more slowly and uses an encoder slot while
  remuxing or transcoding.

On iPhone and iPad, compatible video uses native HTTP Live Streaming rather
than handing arbitrary pieces of a growing MP4 to WebKit's Media Source APIs.
The server indexes each complete movie fragment as FFmpeg produces it and
publishes an append-only HLS event playlist whose initialization and media
segments are separate fixed-length HTTP resources backed by bounded regions of
the same cache-controlled fragmented MP4. Every advertised segment starts at a
random-access point; incomplete fragments are never exposed. The playlist
gains an end marker only after the producer finishes and the cache file is
published atomically.

Publication preserves the generation's pinned output descriptor for playlist,
fragment and range reads. Stopping and immediately restarting the same output
waits up to two seconds for the cancelled producer to release its helper/GPU
permits and finish cleanup; longer cleanup returns `503 transcode_busy`.
New generations never attach to a producer whose cancellation has begun.
Cancelling one session leaves a producer owned by other sessions usable.

Apple mobile HLS uses the server-designated automatic fallback profile only
when the saved quality is Auto; an explicit quality remains explicit. For an
eligible HDR source and advertised HEVC Main 10 output, native Apple HLS tries
HDR even when Safari's synchronous codec probe returns an empty result. A real
media decode failure retains the same-quality H.264 SDR recovery. This does not
delay attachment of the playlist URL on an asynchronous capability promise.
macOS Safari keeps the
selected Auto profile instead of inheriting the mobile 720p starting profile.
Playback settings on native-HLS-capable Apple browsers include **Try original
HEVC with HLS**, an experimental browser-local toggle that is off by default.
With Compatible playback and Auto quality, eligible HEVC video is copied at
source quality while audio still becomes AAC. This trial keeps Auto even on
iPhone/iPad; it does not apply the encoded mobile fallback profile to a copied
stream. Original playback, non-HEVC sources, explicit quality choices, and
sources requiring timestamp repair are unchanged. Sources without a
server-advertised compatible video type (including Dolby Vision Profile 7)
are not eligible. Codec capability promises do not delay playlist attachment.
Changing the toggle during eligible HLS playback restarts at the current
position with playback intent, tracks, and other preferences preserved.
Stream information identifies copied versus re-encoded video. A copy decode
failure, producer failure, or startup stall returns to the normal encoded HLS
plan, preferring advertised HDR output where available. Seeks and resume keep
that recovered plan; the saved toggle remains enabled for future trials.
Copying cannot force new keyframes, so startup and seeking depend on the source's
keyframe spacing. The existing fragment index groups copied output at verified
random-access points rather than assuming every fragment is independent.
The encoder uses one-second random-access segments and publishes the initial
playlist as soon as its first complete fragment is available; it does not wait
for a second keyframe to confirm the already-forced boundary. Subsequent
segments remain append-only. A playlist-refresh `stalled`
event does not claim that playback is buffering; the player reports buffering
only when the media element actually enters its waiting state. The native
media engine may continue refreshing the HLS playlist or buffering segments
while paused. rustyDLNA treats those same-generation requests as resource
reattachments: they reuse the active producer without another cache scan,
playback-request metric, or info-level job-reuse log. The
saved quality preference is unchanged, Stream information identifies native
HLS and the active profile, and the ordinary HLS URL remains eligible for
AirPlay. Browser MP4 output does not carry FFmpeg's implicit chapter text track:
chapter navigation continues to use catalog metadata while the native stream
contains exactly its declared video and audio tracks.

Chrome for Android uses finite Media Source initialization and movie fragments
for compatible video when the browser supports the selected SourceBuffer type.
Supported video and AAC remain copied; encoding follows the existing independent
stream negotiation. Exact HEVC Main 10/HDR10 SourceBuffer support keeps an HDR
output; otherwise the output is H.264 SDR. Auto uses the server-designated
mobile fallback profile, while an explicit quality is preserved.
The fragment path bounds both buffered duration and estimated compressed bytes.
A nearby MSE seek reuses the source only when its selected tracks, quality,
capabilities, and active generation still match and the required decoder data is
buffered. The server's per-fragment `#EXT-X-RUSTY-TIMING` metadata identifies the
preceding random-access point. Copied video requires that point through two
seconds after the target; encoded video requires a quarter-second forward margin.
If an owned buffer operation is still completing, the seek can wait up to 100 ms
before rechecking source ownership and the actual remaining decoder data. A newer
seek or source cancellation invalidates that pending decision.
Missing metadata, timestamp gaps, evicted data, stale sources, and changed settings
retain the generation-safe restart path. Source-local time is mapped through the
current global segment offset, with initial priming learned once per source.
Eligible seeks preserve playing/paused intent, captions, loop, progress, rate,
volume and mute, and fetch no new initialization or generation. Native HLS and raw
MP4 keep their existing seek behavior. A native MP4 that
unexpectedly fails can still enter the same recovery path. Working copied-video
titles and the saved quality preference are unchanged.

Media Source delivery has its own append-only fragment index. Unlike native
HLS, it may append complete non-random-access movie fragments after the initial
random-access fragment, because they remain part of one continuous SourceBuffer
timeline. This keeps copied high-bitrate HEVC startup bounded to the next
roughly one-second fragment instead of delaying Chrome until a complete large
keyframe group has downloaded. Encoded Media Source output likewise uses
roughly one-second random-access movie fragments; this accommodates Android
hardware decoders that reject a separately appended fragment unless it begins
at an IDR. After startup, the browser sends its appended fragment cursor and receives at most 256 new entries per
long-polled playlist; it does not repeatedly download and parse the complete
movie history. Its bounded cursor supports up to 100,000 fragments, including
eight-hour titles with one-second fragments. When the pinned playlist reports a
fallback with different codecs, Media Source replaces its empty decoder buffer
before initialization is appended, preserving the playback request and session.
Stream details show the successful recipe, including cached fallback reuse.
Native HLS advertises independently
decodable keyframe-aligned segments, grouping dependent copied fragments;
encoded segments are approximately one second. It retains the complete EVENT
history. A later copied GOP above an already published target duration requires
a new playlist generation, which can use the larger known maximum. Native Safari
recovery for that case is not established by WebKit automation. After Media
Source has one playable fragment, pausing also suspends its playlist polling and media
downloads until Play. An exact seek within a ten-second server bucket keeps
its target pending until the corresponding fragment is buffered and the native
media clock accepts that offset. A paused seek fetches only enough fragments
to finish the decoder's seek and stays paused. Audio priming can require the
next fragment even when the target is already inside the buffered range;
downloads continue through that handoff. Resumed and deep-linked starts apply the
same offset before playback begins. Replacing the source discards its pending
target. Seeking or deep-linking to the title's exact end shows Replay without
opening a replacement bucket. The bounded generation heartbeat continues so resuming
does not discard an otherwise healthy compatible stream. Playlist fetch,
fragment fetch, and SourceBuffer failures use the same producer-status and
codec/quality recovery policy as native media-element errors; a temporarily
busy producer therefore retries instead of becoming an immediate terminal
Media Source failure. Playlist headers have 120 seconds of preparation grace;
initialization and fragment headers have 30 seconds. Once headers arrive,
each nonempty body chunk must arrive within 15 seconds. The entire request
is limited to 180 seconds for a playlist and 120 seconds for media. Bodies are
counted while streaming, including responses without `Content-Length`: playlists
are capped at 4 MiB, media resources at 32 MiB, and either at 65,536 reads.
The server advertises `capabilities.mse_resource_max_bytes` and applies the same
32 MiB ceiling before advertising or serving an MSE resource. A complete copied
fragment that exceeds that ceiling returns `resource_limit`; rustyDLNA preserves
its GOP and offers Original playback rather than silently changing quality.
Native HLS retains its separate 64 MiB resource ceiling. These delivery checks do
not change encoded output or its cache identity.
Source opening and each SourceBuffer append/remove operation have a 20-second
completion deadline. The producer-status request used by recovery also has a
15-second deadline, including its body.

While playback is requested, the first appended fragment must produce a frame
within 20 seconds, and decoded playback must advance its clock within 20 seconds.
Producer heartbeats or more downloaded bytes do not reset the decoded-playback
clock. Before the first fragment, or while a seek target is being assembled,
120 seconds without newly produced fragment timestamps or appended fragments
triggers recovery; preparation has an absolute five-minute limit. Deliberate
pause, an autoplay block, and background suspension suspend the playback
watchdog; resuming gets the normal grace without replenishing automatic retry
budgets. A pending seek keeps its preparation budget even while paused. Network
and SourceBuffer operations remain bounded while paused.
All deadline failures share the existing source-owned recovery promise. Replacing
or terminating a source aborts its pending reads and clears timers/listeners.
A live copied-HEVC or encoded-HEVC/HDR generation gets
one fresh Media Source attachment at the same position and quality. That
reattachment adopts the existing producer instead of cancelling it first. The
forward buffer is limited to about ten seconds and retains about five seconds
behind playback; initial audio priming or reordered-video timestamp gaps are
counted in that limit even before the media clock advances. Initialization and
retained movie fragments also have a 96 MiB compressed-byte budget; a fragment
remains fully charged until it is evicted. This is an estimate of encoded data,
not a measurement or guarantee of the browser's decoded surfaces. Eviction
preserves copied-video random-access prerequisites. Quota pressure can prune
behind playback and retry within the existing operation deadline. A paused seek
may prune toward its target; if its required decoder data cannot fit, it reports a
resource error without invoking codec or lossy-quality fallback. If copied
HEVC remains unreliable and the browser accepts the advertised encoded-HDR
SourceBuffer type, the next recovery re-encodes to independently keyed HEVC
HDR10 at the same quality. Portable H.264/AAC is the final fallback if that HDR
path also fails. This avoids silently discarding HDR after rapid seek churn.

Admission-busy playback waits and retries for up to five minutes without using
the three-attempt budget reserved for abandoned or failed stream generations;
requests back off to a five-second interval. A current-generation producer status
of starting, producing, or ready (including completed cached output) ends that
admission window without replenishing failed-generation retries. Sustained
decoded playback renews those retries as described below. The player still
exposes a manual Retry action if a slot does not become available
within that bounded window.

Safari on macOS and Apple mobile devices uses the native HLS path when the
media element advertises it. Other browsers use Media Source for encoded
H.264 SDR or HEVC HDR10 whenever the exact advertised video-and-AAC output type
is accepted, and for copied H.264 or HEVC on Android when the exact type is
accepted. This keeps Chrome from treating the currently available tail of a
growing fragmented MP4 as EOF, including during AI-upscaled playback and after
a seek. Unsupported video or audio is converted to portable H.264/AAC before
fragmented delivery. Desktop copied HEVC with converted AAC also uses Media
Source when its exact type is accepted.
When Auto chooses an advertised-supported Original video but it remains loading
or buffering for twelve seconds without playing, the player preserves the
position and switches to the safest lower-bandwidth Compatible profile. This
recovers high-bitrate sources such as 4K HEVC without changing an explicit
Original selection.
When Safari hides the page for device sleep or backgrounding, the player marks
the native HLS attachment as suspended. The next Play—whether it comes from
rustyDLNA's controls or native media controls—starts a fresh HLS generation at
the saved global position instead of trusting Safari's stale buffered-ready
state. A newly attached playlist that requests no media fragments is reopened
after twelve seconds; a second startup stall advances to the normal bounded
fresh-generation recovery instead of leaving the controls on `Preparing
video…` indefinitely. Title selection requests playback while the selecting
tap is still active, then lets `canplay` retry only if the browser remains
paused and playback intent was not blocked. A recurring `canplay` event cannot
change an already-running element to Paused, and a pause caused while the page
is hidden preserves the user's previous playback intent.

When a stream must be encoded, compatible profiles are advertised by the API
rather than hard-coded in the UI. Profile IDs are bounded opaque values: a
browser-local choice is retained while the server still advertises it and falls
back to the advertised Auto profile (or the first profile) when it is removed.
An explicitly advertised empty profile list resets the choice to Auto; a
missing profile field from a legacy response preserves a validated local
choice. If transcoding availability, the advertised profile IDs, or the video
output contracts change while a browser codec probe is pending, the player
discards that negotiation and restarts or blocks it against the current server
capabilities:

The table describes the portable H.264 output. A selected HEVC HDR10 output
uses the same dimensions, frame-rate limit, video bitrate cap, and audio rate.

| Profile | Video | Audio | Approximate peak bandwidth |
|---|---|---|---|
| Auto | H.264 High with an encoder-derived level up to 5.1, yuv420p, source resolution up to 3840×2160 and 30 fps, CRF 20, 25 Mbps cap | AAC stereo, 192 kbps | 25.45 Mbps |
| 4K High | H.264 High 5.1, yuv420p, at most 3840×2160 and 30 fps, CRF 20, 25 Mbps cap | AAC stereo, 192 kbps | 25.45 Mbps |
| 4K Optimized | H.264 High 5.1, yuv420p, at most 3840×2160 and 30 fps, CRF 22, 16 Mbps cap | AAC stereo, 192 kbps | 16.45 Mbps |
| 1080p | H.264 High 4.1, yuv420p, at most 1920×1080 and 30 fps, CRF 22, 8 Mbps cap | AAC stereo, 192 kbps | 8.45 Mbps |
| 720p | H.264 Constrained Baseline 3.1 without B-frames, yuv420p, at most 1280×720 and 30 fps, CRF 25, 3 Mbps cap | AAC stereo, 128 kbps | 3.38 Mbps |
| 480p | H.264 Constrained Baseline 3.1 without B-frames, yuv420p, at most 854×480 and 30 fps, CRF 26, 1.5 Mbps cap | AAC stereo, 128 kbps | 1.88 Mbps |
| 360p | H.264 Constrained Baseline 3.0 without B-frames, yuv420p, at most 640×360 and 30 fps, CRF 27, 0.8 Mbps cap | AAC stereo, 96 kbps | 1.15 Mbps |

Scaling never enlarges the source. Auto therefore keeps a 3840×2160 source at
4K, keeps lower-resolution sources at their original dimensions, and limits
larger sources to 4K. Its H.264 encoder derives the lowest valid stream level
from the resulting dimensions, rate, and frame rate instead of labeling every
output as Level 5.1; this keeps lower-resolution streams acceptable to mobile
decoders without constraining true 4K output. Selecting an explicit quality
also selects Compatible mode, so the requested envelope is applied immediately.
For the active video, the quality controls stop at the smallest advertised
envelope that can preserve the source resolution; larger resolution choices are
not offered. A higher preference retained from another title is capped only for
the current source, so it becomes available again for a later higher-resolution
video.
Supported HDR10 and Dolby Vision Profile 8 video bitstreams can be copied
unchanged when the browser accepts the resulting MP4. Original remains the
exact source-container path for Dolby Vision metadata and enhancement layers.

### Optional SDR AI upscaling

An operator can opt into descriptor-backed libplacebo neural shaders. AI
upscaling is offered only when all of these conditions hold:

- the user explicitly selects a Compatible quality larger than the source;
- the indexed source is exactly 8-bit SDR with a known positive frame rate;
- the aspect-preserving output scale factor is no more than 2×;
- one configured model contains the source dimensions and sustained source
  pixel rate (`width × height × frames/second`) in its measured envelope.

Auto never selects AI upscaling. HDR, Dolby Vision, 10-bit, and incompletely
probed video never enter this path. Without an eligible profile, the player
retains the normal source-resolution ceiling even if a crafted request asks
for more. Stream information and the quality chooser label an active or
available AI upscale explicitly.

Models are external `.glsl`/mpv `.hook` shaders and are not shipped by
rustyDLNA. This avoids silently redistributing model code under terms that may
not match rustyDLNA's GPL-2.0-only license; operators and downstream packagers
must review the selected model's license. At startup rustyDLNA opens each model
once as a bounded, regular UTF-8 file, validates its hook structure, hashes its
bytes into the transcode cache identity, and retains that immutable descriptor.
FFmpeg receives only the reserved descriptor path. Replacing a pathname after
startup therefore cannot change the shader used by a running process.

The reference RTX 3050 calibration selected FSRCNNX 16-0-4-1 as the
quality-first model and FSRCNNX 8-0-4-1 as its speed fallback. Those shader
files are published in the upstream
[FSRCNNX 1.1 release](https://github.com/igv/FSRCNN-TensorFlow/releases/tag/1.1);
review its GPL-3.0 terms before use or redistribution. On the shipped
FFmpeg 8/libplacebo/Vulkan/H.264 NVENC path, an 8.08-second 1920×1080 24 fps SDR
clip completed at approximately 26 fps with the 16-feature shader and 34 fps
with the 8-feature shader, including decode, upscale, and encode. The larger
Real-ESRGAN anime-video model managed about 7.7 fps for 480p→960p and visibly
smoothed general-film detail, so it is not a real-time default. The following
envelopes leave operating headroom on that tested GPU; different drivers,
GPUs, concurrent limits, models, or FFmpeg builds must be recalibrated with a
representative clip through the deployed container:

```toml
[web]
enable = true
encoder = "h264_nvenc"
# Keep one unless the envelopes below were measured with concurrent streams.
ai_upscale_max_jobs = 1

[[web.ai_upscale]]
name = "fsrcnnx-16"
shader_path = "models/FSRCNNX_x2_16-0-4-1.glsl"
max_source_width = 1920
max_source_height = 1080
max_source_pixels_per_second = 52000000 # admits 1080p24, not 1080p30

[[web.ai_upscale]]
name = "fsrcnnx-8"
shader_path = "models/FSRCNNX_x2_8-0-4-1.glsl"
max_source_width = 1920
max_source_height = 1080
max_source_pixels_per_second = 70000000 # admits 1080p30

[transcode]
enable = true
```

Profile order is policy: the first matching envelope wins. Put the preferred
quality model before the faster fallback. Relative shader paths resolve beside
the selected configuration file. `rusty-dlna --config ... --check` executes
every configured shader through the same Vulkan upload, libplacebo filter,
download, and NVENC stages used by playback. A missing shader, unavailable
Vulkan device, unsupported libplacebo build, or failed NVENC encode makes the
check fail before runtime. The check verifies function, not sustained speed;
the configured pixel-rate limit remains the operator's measured admission
contract.

With `web.encoder = "h264_nvenc"`, a more-than-8-bit HEVC HDR10, Dolby Vision
Profile 7, or Dolby Vision Profile 8 source can instead be encoded as HEVC Main
10 HDR10 when the browser codec probe permits it. The display dynamic-range
query is shown as advisory stream information; it does not suppress HDR because
Safari can use its own display-aware tone mapping.
The conversion retains PQ/BT.2020 signaling and drops Dolby Vision enhancement
and RPU data; it does not claim to preserve mastering-display, MaxCLL, or
MaxFALL metadata. Dolby Vision Profile 5 and unclassified Dolby Vision use the
SDR path because rustyDLNA cannot establish an HDR10-compatible base layer.
If the HDR encode or browser decode fails, playback restarts at the same quality
as H.264/AAC SDR. Auto may then consider the 720p automatic recovery profile;
an explicit profile does not. Other
video that must be transcoded is tone-mapped to BT.709 SDR with `libplacebo`,
except the `hevc_nvenc` Main 10 frame-order repair described above, which
preserves HDR10 color signaling. Multichannel audio is
downmixed to stereo. The technical/operator logs record the selected mode,
fallback reason, quality, encoder, source HDR state, tone mapping, audio index,
start offset, cache reuse, cancellation, failures, and
startup-to-initial-bytes, startup-to-playlist-ready, and browser-reported
Media Source playlist receipt, initialization fetch/append, first-fragment
fetch/append, `canplay`, and `playing` latency, plus the requested byte range,
without putting source filesystem paths in browser responses.

Compatibility jobs reuse the existing bounded helper/job gate, runtime
deadline, cache-size and age limits, cancellation, process reaping, and
finished-file verification. Concurrent equivalent requests share a job. A
zero-offset completed stream can be reused from cache. The browser assigns one
stable playback-session ID to the selected title and a newer generation ID to
each replacement source. The player waits for a short pause in rapid keyboard
or timeline scrubbing; when the next generation arrives, the server cancels
every older producer owned only by that playback session. Another browser or
tab sharing an equivalent producer keeps it alive. The session registry retains
at most 1,024 sessions for ten minutes of inactivity; expiration or eviction
also removes that session's ownership from cached jobs. A shared job accepts at
most 1,024 request owners, including callers without a session ID. Existing
owners can reconnect at capacity; additional owners receive retryable busy.
Explicit cancellation also
records the generation before looking up its job, so a late media GET cannot
restart work that was already abandoned. An unintentional dropped connection
still gets a 30-second reconnect window, and reopening the same source attaches
to that producer. While a compatible source remains active, the player renews
its generation with a low-frequency status heartbeat. This covers Chromium's
normal reader-free gaps while it plays buffered fragmented MP4; a missing
heartbeat expires after two minutes, and an explicit replacement still cancels
immediately. An active generation reuses its original descriptor-backed plan
for playlist polls, fragments, and range reconnects, avoiding repeated source
sampling and tool discovery. Completed cache output remains registered and
protected from cache eviction while that heartbeat is active, then releases
its in-memory job after the lease and retention window without deleting the
cacheable file. Across newer seek generations in that playback session, the
browser preserves the negotiated video, audio, delivery, and quality choices.
The server keeps a bounded prepared identity containing the confined source
descriptor, sample-derived base cache identity, and verified FFmpeg snapshot,
then derives only the new start-specific cache key and arguments. A changed
title, audio choice, delivery mode, or quality/codec plan is prepared again.
Prepared identities are limited to 64 recent sessions and expire after two
minutes without a matching request or heartbeat. A distant seek still starts a
new FFmpeg process because encoder and fragmented-MP4 state cannot be resumed
at an arbitrary timestamp.
A browser range pull that resumes inside growing output gets a bounded partial
response while the producer continues; the initial open request remains a live
stream. Completed nonzero-offset output remains available while its active
generation is renewed, then is removed after the lease and retention window,
so repeated exact seeks cannot retain movie-length cache tails. A nonzero
mixed seek with copied audio decodes a bounded five-second lead and then trims
both streams at the requested time, preventing copied AAC preroll from starting
ahead of newly encoded video. A copied-video seek retains keyframe preroll so
the first output video packet remains independently decodable.

The browser keeps the last decoded video frame visible while a direct seek or
compatible replacement stream is pending. The held frame is bounded to about
four megapixels and is released as soon as the target source has displayable
video data; choosing a different title clears it immediately.

When a valid offline timeline-preview sidecar is present, scrubbing selects the
nearest frame from its revisioned JPEG sprite sheets. Frame size and grid come
from the validated manifest; 640×360 in a 5×10 grid is the generator default,
not a server setting. The operator generator is
`contrib/library/generate-dlna-previews.py`. The adaptive whole-second interval targets at most 2,400
frames per title: typically 1 second for 20 minutes, 2 seconds for 45 minutes,
3 seconds for 90 minutes to 2 hours, and 5 seconds for 3 hours. Large or
portrait frames that fit fewer samples within 256 sheets use the corresponding
longer layout-aware interval. Speculative manifest and sheet loading waits for
first-frame presentation and at least three seconds of buffered media. An active
scrub target may load immediately. Up to eight speculative sheets are sampled
evenly across the title; every other sheet remains available on demand. One worker
fetches and decodes at a time, with one pending latest scrub target. New targets
cancel stale work, and repeated targets share a request. A
failed cached response is refreshed once before the preview falls back to the
last decoded video frame.
Only two sheets are retained. Existing images and the next decode share a 64 MiB
budget for estimated RGBA pixels and data-URL text. A bounded JPEG header check
verifies dimensions before decoding; each sheet permits at most 4,096 pixels per
edge and 12 million pixels. One compressed body of at most 16 MiB and a 256 KiB
header buffer can additionally be held, with bounded reads and fetch/decode
deadlines. These are application estimates rather than browser heap guarantees.
A selected preview stays visible through
the direct seek or compatible source replacement and is removed only when the
real target frame is displayable. Missing, stale, or invalid sidecars silently
use the last decoded frame instead.

`GET /api/web/preview/{detail_id}` returns either the validated layout and
revision-qualified sheet URLs or `available: false` for the normal absent/stale
case. `GET /web/preview/{detail_id}/{revision}/{n}.jpg`
serves a validated sheet with an immutable private cache policy. These routes
never expose media paths and never generate or repair files in a request.

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
retries with software decode and `libx264` SDR. `rusty-dlna --config ... --check`
validates the configured browser encoder, the conditional `hevc_nvenc` HDR/repair
encoder, and the `libx264` retry encoder on the deployed host.

## Captions, audio tracks, chapters, and resume

Indexed sidecar `.vtt`, `.srt`, `.ass`, `.ssa`, `.smi`, and `.sub` captions are
exposed with stable indexes, labels, an inferred language subtag from the
dot-owned filename variant, and source format. Browser-selectable entries also
receive a same-origin WebVTT URL. Text must be valid UTF-8 and pass the bounded
sidecar read and cue validation. VTT is normalized; SRT, ASS/SSA, and SMI are
converted to WebVTT.
WebVTT validation distinguishes the signature from annotations, requires valid
cue timestamps, and retains supported cue identifiers, settings, markup, comments,
and pre-cue style/region blocks. UTF-8 BOM and LF, CRLF, or CR line endings are
accepted. SAMI synchronization markers end preceding text even when the next
marker is empty or contains only a nonbreaking space; clear markers therefore
leave real caption gaps. A final nonempty SAMI cue without another marker keeps
the existing five-second ending convention.
ASS override styling and SAMI formatting tags are flattened to text. ASS line
breaks and hard spaces, SAMI paragraph/BR breaks, and common named or numeric
SAMI entities are preserved. Literal angle brackets and ampersands in converted
text are escaped for WebVTT, so text such as an encoded `<b>` is displayed
literally. Timing arithmetic is checked; conversion accepts at most the existing
16 MiB sidecar input limit and caps expanded output at 96 MiB.
The ambiguous `.sub` extension remains visible as unsupported metadata but
cannot be selected in the browser. Malformed, oversized, unsupported, and
path-jail failures return structured errors. Captions default to Off for each
title. The selected caption survives source restarts for that title, while
caption size and background remain browser-local preferences.

Caption URLs use the full title's timestamps. When Compatible playback starts
at a nonzero position, the browser shifts parsed cues to the source's local
timeline before showing them. It drops cues that have already ended and clips
a cue spanning the source start to local time zero, preserving its text,
identifier, and positioning. Each source restart gets fresh tracks; delayed
loads from an earlier source cannot change the current captions. This keeps
deep links, saved resume, repeated seeks, and returns to Original playback in
sync. `captions.js` owns these tracks and their selection; `player.js` supplies
the source lifetime and timeline origin. Fragmented MediaSource delivery and
buffer maintenance live in `media-source.js`.

Caption selection updates existing radio controls, preserving Space/Arrow focus
and visible controls. Long track lists scroll within the player bounds so their
first and last choices remain reachable on small screens. If available tracks
change, focus follows the same available choice, the selected choice, or Off;
removing the selected track selects Off.
A caption load failure opens a polite status message with Retry captions and Turn
captions off. Retry replaces only that caption track and reapplies the source's
timeline offset. It preserves healthy video, position, intent, and playback
recovery allowances. Turning captions off, changing selection while loading, or
replacing the media source detaches obsolete tracks and removes their listeners;
late loads or errors cannot affect the current selection.

Audio language, title, channel count, codec, default disposition, and chapters
are normally read from compact scan metadata. Legacy records can request a
strict, helper-admitted one-item enrichment probe. The UI shows loading and a
retry action if that probe fails. Selecting a different audio track explains
that Compatible playback is required. English-tagged audio (`eng`, `en`, and
regional variants) is preferred over a non-English file default. In Auto mode,
the player starts Compatible playback when necessary to enforce that choice;
explicit Original mode keeps and displays the file's original/default track.
A successful metadata retry re-negotiates Auto or Prepared playback to apply
the preferred audio track, preserving the title session, position, and pause
intent. The request survives source replacement for the same selected title;
selecting another title or stopping playback cancels it. A catalog change while
it is pending produces a retryable error instead of applying stale details.
Initial linked enrichment must match the generation of its item and capabilities
before the player commits that selection.
Explicit track choices survive later metadata updates, including when
the chosen index was previously the default. If no English tag is present, the
marked default and existing codec fallback order are kept.
When transcoding is disabled, the track selector is disabled and no recovery,
retry, quality, or audio-track action can start a Compatible request.

Resume progress is browser-local in `localStorage`; it does not overwrite the
accountless Kodi/DLNA bookmark identity. Writes are throttled and flushed on
pause, explicit seeks, title/source changes, and `pagehide`. Positions before
30 seconds and positions within the last 120 seconds or final 5 percent are
discarded. A partially watched title offers Resume and Start over, appears in
Continue watching, and starts Compatible playback directly at the saved
offset. The resume choice is the top player overlay, suppresses transient video
controls until a choice is made, and keeps both actions at touch-target size on
small screens. Blocked/private storage degrades without preventing playback.

## Status and recovery

The library indicator distinguishes connecting, ready, empty, and error states.
Loading, buffering, seeking, compatible preparation, and paused playback
states are rendered from the active playback session. If browser autoplay
policy blocks a requested start, the player remains visibly paused with its
Play control exposed instead of presenting the ready stream as an error. Every
source load has a monotonically increasing request ID, and callbacks, timers,
polls, picture-in-picture events, previews, or errors from an older session are
ignored. Pending and active picture-in-picture state is rebound when the same
title restarts its source, while selecting another title clears it.
Picture-in-picture entry or exit denial is a nonfatal status message: it leaves
the media source, position, intent, controls, and recovery allowances intact.
Only one entry/exit request runs at a time; late rejections after source
replacement are ignored, and events agree with the browser's active PiP element.
Delayed stream-metadata enrichment can update or restart only the title that
requested it. Screen readers receive plain-language server/library and playback
updates through separate polite, atomic live regions. Repeated renders of the
same asynchronous state do not repeat an announcement, and visible status or
alert messages are not mirrored into a competing live region.
A delayed startup-status connection failure does not interrupt playback that
has already become playable; subsequent media events remain authoritative.
Native Compatible seek completion restores the ready playback state when the
target has decoded data, including WebKit sequences that omit a later `canplay`
event. The current playing or paused intent remains authoritative.

User-facing failures distinguish missing media, unsupported Original playback,
disabled Compatible playback, a busy transcode queue, cancelled/failed
transcoding, and network/offline failure. Fresh-generation recovery retries at
most three times before requiring user action. Selection, explicit Retry, seek,
and playback-setting changes reset that allowance. Automatic renewal requires
at least 30 continuous foreground seconds of decoded progress in the current
source. Video must present new frames or advance a browser decoded-frame counter
as its media clock advances; audio requires an advancing playing media clock.
Credit is limited by both elapsed
monotonic time and media-time advancement divided by playback rate, so faster
playback cannot earn the allowance sooner. A playing/canplay event, one frame,
downloaded bytes, or a producer heartbeat is insufficient.

Pause, buffering, seeking, rate changes, hiding the page, source replacement,
nonadvancing media, and observation gaps over two seconds discard the unfinished
healthy interval. A discontinuous media-clock sample also starts a new interval.
Samples are considered at least 250 ms apart; advancement below half or above
1.5 times the elapsed interval at the selected rate starts a new interval.
Resuming therefore requires a new continuous run; brief recoveries still exhaust
the three retries. Browsers without frame callbacks or decoded-frame counters
retain the finite video allowance until an explicit user reset.
Renewal restores the three generation retries and the one same-plan HEVC/HDR
Media Source reattachment. It does not retry rejected codecs, raise a recovered
quality, reset native growing-MP4 startup reopen counts, or change the separate
five-minute admission policy. A pending or obsolete recovery cannot be renewed
by a late frame. A media connection that fails while its producer is still healthy
starts a newer generation automatically and cancels the abandoned one. If a
Chromium media element remains attached without decoding data after its
compatible producer is healthy, the player first reopens the same growing MP4
generation so accumulated output and encoder work are preserved. A bounded
failure of that recovery falls back to the normal replacement-generation retry.
Apple mobile HLS does not use this growing-MP4 recovery loop.
A desktop browser that supports the exact encoded H.264 SDR or HEVC HDR10
output type through Media Source receives bounded fragmented-MP4 resources
rather than a native growing-file response. The same path is used for supported
copied HEVC with converted AAC. AI-upscaled H.264 therefore does not depend on
the native media loader following a growing file indefinitely.
An early `ended` event on any Compatible stream, including fully transcoded
H.264/AAC, enters the same recovery path as a decoder or media-resource failure.
The player checks the producer state and retains the actual position. Media
Source first reopens the same
HEVC/HDR plan once at the observed position. A repeatedly failing copied-HEVC
plan then uses the independently keyed HEVC HDR10 encoder when supported; only
a failure of that path continues at the same quality with portable H.264/AAC.
If recovery fails or exhausts its budget, playback stops with an error and
retains resume progress. An interrupted title is never marked complete,
looped, or advanced to the next queue entry. Completion allows for rounded
duration metadata within five seconds or 0.1% of the title's duration,
whichever is greater.
Explicitly seeking to the end stops the source and shows Replay. Seeking
backward from there creates a new source at the chosen position, paused until
Play is pressed. This also applies to a deep link whose start is the end.
A mobile or desktop browser that rejects a copied compatible codec with either a
decode or source-support media error, the player retries once with portable
H.264 video and AAC audio instead of repeating the rejected stream.
Depending on the category, recovery offers Retry, Try prepared streaming, Play
original, or Return to library. Raw helper output is never primary copy;
limited technical details remain in a disclosure.

## Versioned API and caching

All JSON success and error documents include `schema_version: 2`. Errors use
`error.code`, `message`, `recoverable`, and optional `action`. Query names,
enum values, numbers, duplicates, percent encoding, UTF-8, and item-path IDs are
validated strictly.

Media `id` and `item_id` fields are canonical decimal strings. Treat them as
opaque identifiers rather than JavaScript numbers: SQLite IDs can exceed the
integer range that JavaScript represents exactly. The same decimal form is
used in item, media, caption, preview, and transcode-status URLs.

| Route | Purpose |
|---|---|
| `/` | Player, or status page when the player is disabled |
| `/web/app.css` | Embedded stylesheet |
| `/web/{app,api,core,library,player,preferences,store,captions,media-source,playback-source,source-selection,playback-timing,preview-cache}.js` | Embedded ES modules |
| `/api/web/library` | Versioned folder, flat-library, or bounded Continue Watching hydration page, plus server root, capabilities, generation, and item DTOs |
| `/api/web/item/{id}` | One item and its catalog `generation`; optional `generation` requires a matching snapshot (409 otherwise); `enrich=1` explicitly probes legacy stream metadata |
| `/api/web/transcode/{id}?session={session_id}&request={generation_id}` | GET returns generation-scoped `queued`, `starting`, `producing`, `ready`, `cancelled`, or `failed` state plus optional `produced_seconds` measured from complete output-fragment timestamps; POST with a bounded startup `event` records the current generation's server-clock observation, or `selection_to_frame`, `seek_to_frame`, and `capability_negotiation` with an integer `elapsed_ms` from 0–120,000 records browser-local elapsed time; DELETE records and cancels an abandoned generation |
| `/web/download/{id}` | Original video as an attachment, with byte ranges and the source filename |
| `/web/media/{id}.mp4?mode=direct` | Original jailed media with byte ranges |
| `/web/media/{id}.mp4?...` | Compatible stream with validated audio track, start, quality, negotiated `video_mode`/`video_output`/`audio_mode`, reason, playback session, and generation parameters |
| `/Captions/{id}/{index}...?format=webvtt` | Jailed browser caption conversion |
| `/status` and `/api/status` | Operator status and metrics |

Generation-scoped transcode status, startup timing, and cancellation use the
active generation's request/session ownership and already validated descriptor.
They continue working after the source pathname is renamed or removed from the
catalog, without reopening it for each status poll. Matching-generation DELETE
is idempotent; stale requests and another viewer cannot cancel the current
owner's work. Starting new media still requires a current catalog item and a
successful confined source open.
An `idle` control response means no matching generation is registered; it does
not assert that the source still exists in the library.

Original downloads advertise an opaque strong `ETag` derived from the opened
file's identity and change metadata. Clients can resume with `Range` and
`If-Range`; a matching tag returns the requested bytes, while a changed file,
weak tag, or unsupported date validator returns the complete current file.
Finalized compatible cache artifacts also advertise a validator based on the
media's immutable change/physical identity and the completion stamp's stable
identity. Reads update only stamp modification time for cache recency, preserving
the validator and reusable output validation. Outputs without a completion stamp
have no resume validator.
HEAD on the same compatible rendition URL reports the exact `Content-Length`
once output is complete, with the same range headers as GET and no response
body. While output is growing, an ordinary HEAD omits `Content-Length`;
a ranged HEAD may report the snapshot length with an unknown total (`/*`).
Growing compatible output does not advertise a stable validator. Its partial
responses with an unknown total describe a snapshot and must not be treated as
a complete offline movie.

Browse parameters are `view=folders|library`, `folder`,
`kind=all|video|audio`, `q`, `sort=title|date_desc|episode`, `offset`,
`limit`, and `generation`. The server default page is 60 and the maximum is
200; the browser collects the complete current view in batches of up to 200,
with at most four requests in flight, before publishing the list.
Media DTOs include a nullable `collection` with an opaque directory `id`, a
folder `title`, and numeric `sequence`. Collection ordering happens before
pagination in both SQLite and the in-memory fallback.
Passing the first page's generation on later pages gives stable pagination; a
catalog change returns `409 catalog_changed` rather than mixing snapshots.
Metadata loading is independent of scrolling. A view change aborts the whole
load, and stale responses cannot replace a newer view. Card artwork is lazy,
asynchronously decoded, and explicitly lower priority than
library API traffic. The UI serves versioned
scanner artwork directly with a one-day private browser cache; a deployment
that prepares 360x540 posters therefore performs no request-time resize. At
most four near-viewport artwork requests run at once, and quickly skipped cards
leave the queue before they consume server capacity. The browser checks nearby
posters at most once per animation frame after scrolling, resizing, or revealing
an offscreen batch, without depending on `IntersectionObserver`. A catalog change
offers a full refresh without displaying an incomplete list or mixing pages from
different generations.
The flat view performs at most three total SQLite snapshot attempts, uses
deterministic ordering, and only materializes the requested page. Browser search
uses Unicode lowercase substring matching over title, artist, album artist,
album, and filename in both SQLite and memory. Accents and combining marks remain
distinct; parent directory names are outside the search domain, and `%`, `_`, and
backslash are literal characters. Date order breaks equal dates by normalized
title and detail ID; episode/track order uses normalized album, disc, track,
normalized title, and detail ID.

Later SQLite pages reuse generation-scoped population and matching counts in the
bounded catalog query cache. Identical SQLite pages also reuse validated IDs
and counts; each response hydrates current metadata after generation and private
epoch checks. The shared cache holds at most 256 entries. Physical-folder pages
reuse ordered ID projections
for up to eight folder/query combinations within 32 MiB; child counts have their
own bounded cache. Generation publication invalidates these caches, including
when the public generation wraps. Search-key collection and child counting use
bounded catalog-lock intervals; sorting and page DTO construction release the
catalog lock. Unchanged conditional requests validate parameters, folder
existence where applicable, and generation before skipping page work.

Heavy-query admission, reader admission, SQLite execution, retries, and memory
fallback share a five-second absolute budget. An exhausted request returns recoverable
`503 catalog_busy` rather than retrying an expensive memory search. SQLite
unavailability can still use the bounded fallback. Four catalog queries may
execute concurrently; media, item-detail and status requests bypass that gate.
Memory query work is capped at one million inspected records and 64 MiB of
scratch/key storage. The fallback retains a catalog read guard during filtering
and sorting, so its bounded execution can still delay publication. Detailed status
includes fixed-cardinality `catalog_queries` wait, execution, fallback, timeout,
cancellation, and work-budget measurements. Continue
Watching takes one browser-local progress snapshot, keeps at most the 500
persisted IDs, and hydrates them through
`view=continue&ids=...` in generation-consistent batches of at most 100. It
accepts only `ids` and `generation` in that mode and never downloads the full
catalog to reconstruct the view.

Library and item responses use API-schema-, representation-revision-, and
generation-based weak ETags with `private, max-age=0, must-revalidate`. The
representation revision invalidates cached capabilities and metadata after an
upgrade even when the media catalog generation is unchanged. A matching
`If-None-Match` receives 304.
Embedded assets deliberately use `Cache-Control: no-cache`, so browsers may
store them but must revalidate; HTML contains no hand-maintained cachebuster.
Media and captions keep their route-specific range and safety policies.
Video item DTOs advertise `download_url`; audio item DTOs return `null`. The
download endpoint accepts no query parameters, never invokes FFmpeg, and opens
the source through the same configured-root confinement used by original
playback. Responses use a bounded, header-safe UTF-8 attachment filename,
`private, no-store`, and byte ranges so browsers can resume a large download.

### Native offline copies

Native clients can send `X-RustyDLNA-Download: resumable` on compatible MP4 GETs.
While the exact generation is preparing, the server returns an empty HTTP 202
with `X-RustyDLNA-Download: preparing` and `Retry-After: 30`. The client schedules
another background request to the same URL; this is preparation, not a failed
transfer. The producer remains bounded by its normal limits and stays alive
between these requests. Explicit generation cancellation still stops it.
Once validated and finalized, the response includes Content-Length, ETag and
byte-range support. Reconnects use Range with If-Range so downloaded bytes are
reused only for the same artifact. Preparation transfers no provisional video
bytes: a close-delimited growing stream cannot distinguish an interruption
from a completed download. Requests without this header, including browser
playback and HEAD size probes, retain their existing streaming behavior.

Schema 2 capabilities advertise `native_downloads` when compatible output is
enabled. Native clients can add `download_audio=selected` or `download_audio=all`
to a compatible MP4 request starting at zero. The existing `audio` index selects
the preferred/default track. All-track requests are bounded to 32 audio tracks.
Supported AAC, AC-3, E-AC-3 and MP3 are copied; other audio is encoded as AAC
with one to eight channels. Language metadata and the chosen default survive
the mapping. Video uses the selected profile's resolution and video bitrate
limits, and native download requests disable AI enlargement. Smaller source
dimensions are retained. These outputs have a distinct cache identity.

The native iOS request marker `reason=native_ios` also treats quality as a
maximum for ordinary MP4/HLS playback, including seek and fallback generations:
a 720p source with `quality=full_hd` stays 720p while retaining the 1080p
profile's video bitrate limit. It never selects the browser's AI upscaler.
This applies to installed native clients without requiring a new query field.
AI and ordinary capped encodes have separate cache identities, so previously
upscaled browser output cannot satisfy these native requests.

Other requests without `download_audio` retain the web player's existing audio
and upscale negotiation. Native audio choices can exceed the profile's stereo
bandwidth estimate: `max_video_kbps` describes the video budget; copied or
additional audio contributes separately. MP4 delivery still omits embedded
subtitles; clients can store advertised caption sidecars alongside it.

Each video DTO also advertises source-specific `prepared_video_outputs`.
`hevc_hdr10` is included only when the configured encoder and the probed source
qualify for that existing HDR output contract; otherwise clients may request
`h264_sdr`, retain the original, or explain why their HDR requirement cannot be
met. This list is additive and does not replace browser capability probing.

### Browser-only gateway

`docker-compose.web.yaml` runs a separate `rusty-web` nginx container in front
of the host-network server. The gateway has no rustyDLNA executable, media or
cache mount, FFmpeg tools, database, scanner, or UDP listener. It permits only
GET/HEAD for the player, browser API/media, artwork, caption, health, and status
routes; browser transcode status additionally permits bounded startup-telemetry
POSTs and DELETE for cancellation. All other paths and methods return 404. In
particular, `/rootDesc.xml`, SCPD, SOAP control, GENA eventing,
`/MediaItems/*`, `/Transcode/*`, and `/Icons/*` cannot cross this container
boundary.

The gateway is deliberately not an authentication service. Downloads use the
same `/web/*` allowlist and same-origin credentials as the rest of the browser
application. Keep TLS and authentication at the outer reverse proxy and send
that proxy to the gateway's TCP port rather than the server's TCP 8200. The
default gateway bind is `127.0.0.1:8201`; a reverse proxy in another container can set
`RUSTY_WEB_BIND_IP` to a host address it can reach. `scripts/web-gateway-smoke.sh`
verifies both the allowed browser paths and denied DLNA surface.
`restart-web.sh` rebuilds and recreates only the gateway, waits for it to become
healthy, runs that smoke test, and leaves the DLNA service untouched.

## Playback measurements

`playbackTimingSnapshot()` in `/web/playback-timing.js` exposes at most 64 recent
browser-local records and fixed histograms. Selection starts before capability
negotiation; a linked navigation includes metadata and first-page capabilities.
`requestVideoFrameCallback` supplies presented-frame completion. Browsers without
that API use explicitly marked estimates, kept in separate histograms. Original
playback records remain browser-local; compatible durations also reach the
owning server generation when the elapsed value is within 0–120,000 ms. Longer
durations remain in the local record and are not clipped into a shorter server
measurement. Browser and server clock origins are never subtracted.

`/api/status` retains existing startup fields and adds fixed duration buckets,
cache operation counts, and `transcode.web_player.performance`. Preparation,
source open/sample, tool identity, admission, helper attempts, server-observed
first complete fragment, and playlist readiness are distinct observations.
The recent server record is bounded to 64 generations; paths, titles, session
IDs, and request IDs are absent from metric labels and exported records.
Profile-8 remap jobs additionally expose bounded per-stage file sizes and I/O
in `transcode.web_player.performance.profile8`; this shared diagnostic location
does not imply that the browser negotiated a Dolby Vision remap. See the
[Profile-8 pipeline contract](TRANSCODE.md#serve-path-background-growing-fmp4-file-cache).

`node scripts/playback-benchmark.mjs --help` describes the generated-fixture
benchmark. Run it against each saved server binary under matching conditions;
reports, logs, and media belong in temporary storage. The CPU subset covers
Original, cold/warm Compatible, attachment, and buffered/restarted seeks, with
explicit recipe, display/delivery, concurrency, cache conditions, versions, and
resource-sampling limitations. Percentiles include sample counts and variability;
small-sample p95/p99 values are descriptive observations, not reliable tail
estimates. Dedicated HDR/Dolby Vision, GPU, device, and storage tiers require
separate measurements and are reported as unavailable when not exercised.

For an ordinary CPU smoke run after building the server:

```sh
node scripts/playback-benchmark.mjs --samples=2 --recipes=copy,audio,video,both --size=640x360 --output=/tmp/playback-smoke.json
```

Use at least ten independent trials for a before/after comparison, saving the
baseline executable before editing its embedded assets. Pass `--binary` for each
executable, its `--build-profile=debug|release`, and
`--compare=/tmp/playback-before.json` to the after run. Comparisons
reject mismatched workload/environment conditions. Default engineering guard
bands flag median increases exceeding both 25% and 50 ms, and observed p95
increases exceeding both 30% and 100 ms; these are configurable thresholds, not
statistical confidence. Fewer than ten trials cannot gate a regression; p99
gating is disabled below 1,000 independent trials and is still an engineering
threshold rather than a confidence interval. Concurrent viewers from one
trial are correlated. Repeat flagged workloads under matching idle conditions.

`--concurrency=1|2|4`, `--fps=24|30|60`, `--rate=1|2` and `--size` select generated
tiers. `--quality` and `--encoding-preset=balanced|fast_start|maximum_speed`
select existing browser preferences and verify the negotiated recipe. Different
presets are not comparable workloads for a graph regression gate.
Each validation records the requested and effective quality separately when
the browser limits a profile to the source resolution.
A supplied 35–600-second `--fixture` is copied into the temporary library;
`--tier` labels the intended coverage and `--encoder` selects an existing server
encoder. Actual negotiation, output probes and frame checks establish which tier
ran. They do not force a GPU/HDR/Dolby Vision pipeline. The CPU subset deliberately
ends audio before video; audio-only final fragments remain a separate index
limitation and are recorded in each report. Resource sampling includes the
harness, browser, server and live descendants at approximately 100-ms intervals;
short-lived helpers may be missed, and summed RSS is not unique physical memory.
The report records actual media-element speed at presentation, seek completion
and sustained-window boundaries and rejects a mismatch with the requested speed.
Media-clock progression and dropped-frame observations remain separate checks;
a selected 2× rate alone does not establish sustained 2× playback. Cumulative
process-sampling time is observer overhead, separate from measured window duration
and browser latency.

The separate linked-navigation benchmark injects 600-ms page latency into
400-card and 10,000-card libraries while serving generated media from memory:

```sh
RUSTY_DLNA_LINK_BENCHMARK_REPORT=/tmp/linked-after.jsonl npx playwright test web-tests/linked-startup.spec.js --grep 'measure linked startup' --project=chromium --workers=1 --repeat-each=8
```

Set `RUSTY_DLNA_PLAYWRIGHT_SERVER_PROGRAM` to a saved baseline executable for
the before run and choose a different JSONL report path. Each record includes
the fixture hash, versions, hardware, delay, media-request timing and presented
frame timing. Run these measurements separately from the workspace gate and
other browser workloads; list-rendering completion is separate from link-to-frame.

## Browser support and verification

Playback implementation has three owners. `source-selection.js` reads a state
snapshot and browser capabilities to choose the stream and delivery plan; it
owns the bounded cache of completed capability probes. Native Safari selection
remains synchronous to retain user activation. `playback-source.js` owns one
source's abort signal, media listeners, polling/startup timers, telemetry, and
shared in-flight recovery. Cancelling it invalidates queued callbacks and
stops its timers. `player.js` coordinates user actions and rendering, including
seek/retry delays that outlive the source being replaced. `store.js` publishes
the negotiated plan and completion as atomic transitions, and pure recovery
decisions live in `core.js`. See [the ownership decision](adr/0002-playback-source-ownership.md).

The automated behavior suite runs desktop Chromium, Firefox, and WebKit plus a
mobile Chromium viewport. It covers source selection/fallback and error
recovery, truncated-stream completion, backward seeks after an explicit end,
session cancellation, seeks, fullscreen controls, keyboard scoping,
responsive/touch layout, captions, audio tracks, resume, complete queue snapshots,
stable scrolling and lazy posters, history/search/catalog-generation
races, reduced motion, expanded iPhone playback, wake-lock lifecycle, and axe accessibility
checks. Headless Linux WebKit lacks a working element Fullscreen API, so that
one API exercise is skipped there; its native video-fullscreen fallback is
covered separately. `scripts/check.sh` runs the dependency-free web unit suite as
part of the canonical local gate and reports an actionable error when Node.js
20 or npm is unavailable. CI and tagged releases install the locked npm
dependencies and all three Playwright browser engines, then run both the unit
and multi-browser behavior suites. For the same focused checks locally, run
`npm run test:web-unit` or, after `npm ci` and `npx playwright install`,
`npm run test:web-browser`. Browser tests create their configuration, database,
and derived-media cache under a temporary directory and remove it when the
test server stops; they do not write runtime state into `testdata/`.

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
