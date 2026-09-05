# Transcode

rustyDLNA transcodes as a **job**, not as a
rewrite of `/MediaItems/`.

## Decision

Default is **serve the original**. The Streamer plays most Dolby Vision;
only some bitstreams fail. Recode happens only when a `[[remap]]` row
matches **codecs** (and optional client / container). Not titles.

`decide(client, source, remaps)` — first matching row wins.

```toml
# Same codec, different software → different action.
[[remap]]
name = "streamer-p7"
client = "CrKey"             # this player (UA token)
hdr = "dv-p7"
action = "hdr10"

[[remap]]
name = "kodi-p7"
client = "Kodi"
hdr = "dv-p7"
action = "original"

[[remap]]
clients = ["CrKey", "BubbleUPnP"]
audio = "truehd"
action = "audio-ac3"
```

`client` is the **software**, not a title: User-Agent token (`CrKey`,
`Kodi`, `SEC_HHP_`), table name (`Google Cast / Streamer`), or alias
(`google-cast`, `streamer`, `samsung`, `any`). `clients = [...]` is the
same field as a list. Unset `client` matches every player.

| `action` | What |
|---|---|
| `original` | leave it (exception carve-out) |
| `remux-p8` | copy HEVC, rewrite RPU to Profile 8.1 — keeps DV, no NVENC |
| `hdr10` | encode PQ + BT.2020, strip DV |
| `audio-ac3` | copy video, convert audio |

Unset match fields are wildcards. `hdr = "dvhe.07"` is an alias for `dv-p7`.

Empty `[[remap]]` list → every client, including Cast, gets the original.

The embedded web player is the exception to the DLNA remap decision: its
dedicated `/web/media/` compatibility route uses an internal browser plan,
not a `[[remap]]` rule, and selects its H.264 encoder through `web.encoder`.
See [`WEB_PLAYER.md`](WEB_PLAYER.md). When `transcode.enable = false`, the web
player still serves Original files but disables the Compatible mode and returns
a structured `transcode_disabled` response if that route is requested.
CUDA-decoded browser encodes download bounded-resolution frames to a fixed
software pixel format before NVENC accepts them. HDR-to-SDR output performs
tone mapping and browser-profile scaling together in libplacebo, so a bounded
quality request does not download and CPU-scale a tone-mapped 4K frame. This
isolates the encoder from decoder hardware-context changes when a source begins
signaling color metadata partway through the stream; the browser cache identity
includes this output-pipeline revision. NVENC browser output decodes H.264 in
software because that avoids the costly CUDA download/re-upload path and reaches
the first fragmented-MP4 segment sooner while sustaining faster-than-playback
output. HEVC sources keep CUDA decode to bound CPU cost, including the HDR
tone-map path. The Profile 7 to browser HDR10 path instead decodes its base
layer in software because CUDA does not reliably expose that dual-layer input,
then uses NVENC for the output. Auto-profile
H.264 output lets the encoder derive the lowest valid H.264 level from the
actual output instead of forcing the 4K-capable Level 5.1 declaration onto
lower-resolution streams; explicit profiles retain their fixed compatibility
levels. The 720p, 480p, and 360p profiles use Constrained Baseline H.264 without
B-frames and with one reference frame so growing MP4 output begins with
monotonic decode timestamps on mobile Chromium decoders. The available caps are
25 Mbps and 16 Mbps at 4K, 8 Mbps at 1080p, 3 Mbps at 720p, 1.5 Mbps at 480p,
and 0.8 Mbps at 360p.

Browser Encoding presets change only video encoder tuning, not codec/HDR
negotiation, output resolution, bitrate caps, GPU filters, timestamp repair,
one-second HLS/MSE IDRs or producer pacing:

| Preset | NVENC (H.264 / HEVC) | Software H.264 |
|---|---|---|
| Balanced | `p4`, `hq` (existing behavior) | `veryfast` (existing behavior) |
| Fast start | `p4`, `ll` | `veryfast`, `zerolatency` |
| Maximum speed | `p2`, `ll` | `ultrafast`, `zerolatency` |

Both experimental presets disable B-frames. NVENC additionally uses zero
lookahead, `zerolatency=1`, and `delay=0` to avoid output queuing. Less buffering
and faster presets can reduce compression efficiency or quality at the same
bitrate; they do not guarantee lower end-to-end latency. The existing CPU
fallback applies the selected software preset. Copied video and audio-only
plans ignore tuning. Non-default encoded output adds the versioned
`browser-encoding-v1` preset identity; Balanced output remains reusable.

Configured browser AI-upscale profiles are an explicit exception to the normal
no-enlargement rule. They apply only to user-selected, at-most-2× Compatible
output for exactly 8-bit SDR sources inside a measured model envelope. The
Vulkan/libplacebo shader is inherited through a pre-opened descriptor, its hash
and model name own the cache identity, and a separate `web.ai_upscale_max_jobs`
gate protects the measured real-time rate. Auto and every HDR/10-bit path keep
the ordinary resolution-preserving policy. Configuration and reference model
measurements are in [`WEB_PLAYER.md`](WEB_PLAYER.md#optional-sdr-ai-upscaling).

When `web.encoder = "h264_nvenc"`, the browser API also advertises an HEVC Main
10 HDR10 output. More-than-8-bit HEVC HDR10 and Dolby Vision Profile 7/8 sources
can use it only after exact browser codec and Media Capabilities checks. The
display-range media query is diagnostic rather than a veto because Safari can
accept and tone-map HDR while reporting the current display as standard range.
The encode keeps PQ/BT.2020 signaling and discards Dolby Vision data;
Profile 5 or unknown Dolby Vision stays on the libplacebo SDR path. A failed HDR
output is retried at the same quality with H.264/AAC SDR. Source mastering
display and content-light metadata are not promised on the HDR output.

`transcode.encoder` is the default only for `action = "hdr10"`; a rule-level
`encoder` overrides it. `remux-p8` and `audio-ac3` must copy video. Run
`rusty-dlna --config ... --check` on the deployed host to verify ffmpeg,
ffprobe, compiled encoder support, hardware-device usability, and dovi_tool.
Missing dovi_tool is reported as a warning because Profile-8 jobs retain the
documented HDR10 fallback. These checks have hard deadlines and bounded output;
a hung or noisy tool cannot block validation indefinitely or grow memory
without limit.

## Advanced-media fixture contract

`scripts/generate-advanced-fixtures.sh OUTPUT_DIRECTORY` creates small media
inputs entirely from FFmpeg `lavfi` sources. The generated set includes real
six-channel TrueHD, PQ/BT.2020 HEVC with mastering-display and MaxCLL/MaxFALL
SEI, audio-before-video with two audio ordinals and a subtitle, an 80 KiB
embedded tag, and deterministic truncated/corrupt Matroska inputs. No fixture
depends on copyrighted source media.

`scripts/generate-dolby-vision-fixture.sh OUTPUT_MKV` separately rebuilds the
checked-in `dvp7.mkv` from checksum-pinned HEVC test assets at quietvoid
`dovi_tool` commit `38adec045bf183c24df38149836c920398072281`. Those assets
are MIT-licensed (the complete notice is in `testdata/README.md`). The script
uses `dovi_tool -m 1` and deterministic `mkvmerge` output to produce a genuine
Profile 7 MEL stream with BL+EL+RPU and real six-channel TrueHD; no probe
sidecar is used. Docker CI regenerates and byte-compares this fixture with the
pinned production FFmpeg toolchain and also verifies that its TrueHD packets
decode successfully.

The scanner tests verify the real codecs, HDR signaling, stream indices,
metadata allocation cap, and bounded malformed-input behavior. The transcode
tests and production-image smoke execute TrueHD-to-AC-3 remapping, genuine
Profile 7 to signaled Profile 8 conversion, and the HDR10 failure fallback.
They require fragmented, decodable MP4 output and probe codec profile, 10-bit
format, color metadata, Dolby Vision configuration, and converted RPU bytes.

## HDR and Dolby Vision

- **HDR10-compatible output** uses PQ `smpte2084`, BT.2020, and 10-bit pixels.
  ffmpeg flags: `-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc`,
  `-tag:v hvc1`, 10-bit `p010le`.
- **Dolby Vision Profile 7** (BL+EL+RPU, `compatibility_id=6`) cannot.
  That is what blacks out Google Streamer. The encoder must strip EL/RPU
  (ffmpeg already logs `Skipping NAL 63`). Output is HDR10, not DV.
- CUDA decode of DV P7 failed on the box that proved the offline path
  (`Impossible to convert… Function not implemented`). Encode jobs
  default to **software decode + NVENC** (or libx264), same as that encode.
- Mastering-display, MaxCLL, and MaxFALL metadata are not currently copied.
  The server does not claim mastering-metadata preservation; deployments that
  require exact HDR mastering metadata should serve the original resource.

## Serve path: background growing fMP4 (file cache)

This is **not** a live ffmpeg stdout pipe. `GET /Transcode/{id}` starts
**one** job per title that writes a fragmented MP4 (`.part`, then
rename). Ordinary growing-MP4 delivery may attach after the first ~16 KiB;
fragment delivery waits for its initialization and first complete media
fragment. The rest fills in behind the client.

Each job pins its output descriptor before atomic publication. Growing reads,
finished ranges, and HLS/Media Source indexing retain that descriptor across
the `.part` rename, so publication cannot invalidate an in-flight first open.
Reads and index updates use explicit offsets on the pinned file; a later
pathname replacement cannot redirect an existing generation to different
bytes. Output I/O and admission waits run outside asynchronous socket tasks.

A producer that has received cancellation is unavailable for new attachments,
even while its public state still says Starting or Growing. A same-output
restart waits up to two seconds for helper reaping, intermediate cleanup and
permit release before registering a replacement. If cleanup takes longer,
admission returns retryable busy. Cancellation tombstones and shared-session
ownership still apply while waiting; the old producer cannot remove the
replacement's registry entry or output.

HLS/Media Source output that encodes either track runs without input pacing for
its first 30 seconds of media, then reads at playback rate. This preserves a
useful startup buffer without racing through the rest of a feature film at full
CPU/GPU or cache-write utilization. On FFmpeg 8 and newer, the catch-up rate is
raised only while filling that lead; otherwise its near-realtime default also
paces the accurate-seek preroll and delays the first fragment by several
seconds. Older FFmpeg releases do not expose that catch-up option and retain
their legacy initial-burst behavior without it.
Fragmented remuxes that copy both tracks remain unpaced because they consume
negligible encoder resources. Playlist,
fragment, and reconnect requests from one browser generation reuse its initial
descriptor-backed job plan; completed output stays protected from cache
eviction while the generation heartbeat remains active.

For a nonzero mixed seek that encodes video and copies audio, FFmpeg first
seeks to a bounded five-second lead and then trims both output streams at the
requested timestamp. This prevents copied audio packets from the demuxer's
preceding video keyframe from starting ahead of the newly encoded video. Mixed
seeks that copy video retain the preceding independently decodable keyframe.

Fragmented browser encoding forces a one-second keyframe interval. Native HLS
therefore publishes its first completed movie fragment immediately as an
independently decodable segment instead of waiting for the following keyframe
boundary. Copied-video HLS retains stream-aware look-ahead so non-random-access
fragments are never advertised alone. Every encoded Media Source append also
begins at a random-access point for Android hardware-decoder compatibility.
Both delivery modes retain roughly one-second movie fragments for bounded
startup and transfer.

| State | `DLNA.ORG_OP` | Seek |
|---|---|---|
| Growing `.part` | `00` (no `Content-Length`) | small Range probe only |
| Finished dest | `01` + `Accept-Ranges` + `Content-Length` | byte Range 206 |
| Source `mtime`/`size` ≠ stamp | dest deleted, job restarts | same as growing |

`remux-p8` runs `dovi_tool -m 2 convert --discard` (BL + P8.1 RPU) when
the binary is on `PATH`. The pipeline carries the source Dolby Vision level
into a Profile 8 `dvvC` record, writes an `hvc1` fragmented MP4, and retains
PQ/BT.2020 signaling. The signaling pass scans MP4 box headers without loading
the media-sized intermediate into memory and shifts staging bytes in fixed-size
chunks under the job cancellation token and hard deadline. Every preprocessing
stage also checks the server cache limits; pressure stops and reaps the active
helper, removes all staging files, and fails the job instead of starting the
HDR10 fallback. Consumed HEVC/Profile-8 stages are removed before the next
stage starts. If dovi_tool is missing or the convert/signaling step fails, the
job falls back to the `hdr10` encode. A runtime HDR10 fallback can complete the
current request but is not reused under the requested Profile-8 cache identity.
First audio map prefers `aac` / `ac3` / `eac3` over TrueHD / DTS.

Kodi opens several GETs at once. They **attach** to the same job. A
probe disconnect does **not** kill ffmpeg.

- `-movflags frag_keyframe+empty_moov+delay_moov+default_base_moof` (no
  `+faststart`); delaying initial sample-entry construction keeps AC-3-in-MP4
  streamable and is also used for copy/AAC output
- `-flush_packets 1` and ~1 s `-frag_duration`
- Map `0:v:0` and a chosen audio
- `JobGate` caps **titles** (`max_jobs`), not TCP connections
- Finished dest is reused (Range / `OP=01`). Growing dest is `OP=00`.
- ffmpeg stderr and HTTP 4xx/5xx are logged at **error**

If `decide` says original, the handler serves the source file.

## HTTP

Transcoded GETs while the `.part` file is growing:

- `Connection: close`
- `transferMode.dlna.org: Streaming`
- `contentFeatures.dlna.org: … DLNA.ORG_CI=1; DLNA.ORG_OP=00…`
- No `Content-Length`, no `Accept-Ranges`
- TimeSeek without Range is still 406

After ffprobe validates and atomically publishes the finished cache, GET/HEAD
uses `DLNA.ORG_OP=01`, `Content-Length`, `Accept-Ranges: bytes`, and normal
single-range 206/416 behavior. This distinction is derived from job state, not
from the DIDL resource advertisement.

The supervisor bounds stderr by streaming a fixed tail, uses UTF-8-safe lossy
decoding only after capture, applies the configured wall-clock deadline, and
terminates/reaps the process group on cancellation or shutdown. Persisted
per-stream descriptors retain both the source stream index and the audio
ordinal used in `0:a:N`; repeated codecs therefore do not collapse selection.
Successful output must pass a bounded ffprobe check before `.part` is renamed.
Failed verification deletes the incomplete output and leaves no reusable
cache entry.

Cache identities include the effective codec, audio, browser-quality, HDR
preservation, Dolby Vision, source, and tool-version inputs. Tool versions are
queried with the same bounded supervisor and cached by executable path plus
file identity. A request-time cache hit only restats that identity and consumes
no helper slot. An identity miss is single-flight, enters the global helper
gate, and observes daemon cancellation; admission, cancellation, deadline, and
query failures do not emit a server cache key. Each lookup rechecks the file
identity, so replacing an executable in place invalidates its cached version
without requiring a rustyDLNA restart. An output-producing fallback may finish
the current request, but is not stamped under the failed primary plan's cache
identity; the next request re-evaluates the preferred plan.

Browser cache basenames contain a fixed-size digest of the complete cache
identity. The full identity, including readable policy revisions, remains in
the cache stamp and job key. Adding a policy revision therefore invalidates the
output without risking the filesystem's per-component filename limit.

Within one browser playback session, seek generations with the same source,
stream plan, delivery mode, and quality reuse the already opened source and
verified FFmpeg identity. The server derives the new start-specific cache key
without sampling the source or discovering the tool again. The prepared state
is capped at 64 recent sessions and expires after two minutes without a matching
request or heartbeat; changing any output-affecting plan input replaces it.
Each distant seek still launches a new producer for the new encoder and fMP4
timeline state.

Every cache-producing job retains the opened `ffmpeg` inode represented by its
verified file identity and version fingerprint, including browser jobs and an
HDR10 fallback after a Profile-8 attempt. Profile-8 identities additionally
retain `ffprobe` and `dovi_tool`. Producers execute the retained descriptors,
not a fresh `PATH` lookup, and reject detectable in-place changes before a
spawn. Atomic package upgrades or path replacements therefore cannot make a
producer write bytes under a key derived from different tools. These snapshots
are inode-pinned rather than private byte-for-byte copies: executable files
must not be modified concurrently in place. Profile-8 keys have a separate
toolchain revision so output cached before the complete three-tool identity is
not reused.

Source cache identities sample the opened descriptor with positioned reads.
They retain the established digest byte grammar while leaving both the caller's
file cursor and cursors on cloned descriptors unchanged, including concurrent
lookups. Each sample accepts the first successful read, including a short read,
exactly as the prior cache grammar did; interrupted reads are retried at most 16
times.

Do not reuse `stream_buffer_mb` for encoded bytes. That window is source
file data.

## What “proper” means here

Not “transcode everything.” Proper means:

1. Know the source (container, codec, HDR, audio stream descriptors) from the
   persisted scanner probe.
2. Know the client (table + `NEED_SAFE_VIDEO`).
3. Hide or demote the unplayable `<res>` for that client.
4. Produce HDR10-compatible color signaling and drop DV, without claiming
   mastering-display/MaxCLL preservation.
5. Bound encoder jobs and cache space; cancel and reap cleanly.
