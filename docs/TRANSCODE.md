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

For Kodi 21 on Android with **Dolby Vision compatibility mode** enabled under
Settings → Player → Videos → Processing, keep Profile-7 delivery `original`
(or omit the Kodi remap). Kodi converts the RPU to Profile 8.1 during decoding,
while the original container retains its full duration, audio tracks, and
byte-range seeking. The setting requires Advanced/Expert settings visibility
and a Dolby Vision-capable device; see [Kodi's conversion implementation](https://github.com/xbmc/xbmc/blob/21.2-Omega/xbmc/utils/BitstreamConverter.cpp).
A server `remux-p8` rule instead delivers a growing MP4 with unknown final
length. Kodi disables seeking for that response until the completed file is
reopened; increasing the cache quota does not change that transport limitation.

Browser stream mapping excludes attached pictures, so an audio file with a
cover produces only audio. Video selection uses the first actual video stream.
The scanner also excludes covers from video capabilities and refreshes affected
stored probe records during reconciliation, preserving the artwork.

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
output. H.264 SDR conversions of HEVC HDR10 and Dolby Vision Profile 8 use
Vulkan video decode on the same device as libplacebo. Full-resolution decoded
frames stay on that device through tone mapping and scaling; only the bounded
SDR output is downloaded for NVENC. This avoids the full-resolution CUDA-to-host
download and host-to-Vulkan upload that can limit offline download throughput.
If Vulkan cannot start before playable output is available, the job retries
CUDA decode with NVENC, then the portable software encoder. All attempts share
the original cancellation and runtime budget, and fallback output is not cached
under the primary Vulkan identity. Other HEVC paths retain CUDA decode.
The Profile 7 to browser HDR10 path instead decodes its base
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

The opt-in `gpu_graph` server example compares the existing CUDA download
boundary with device-resident NVENC input and software decode at identical
resolution, bitrate and encoding preset. It uses supervised helpers, generated
SDR media, decoded-frame hashes, SSIM and sampled process/GPU counters. Its
experimental graphs do not participate in server selection:

```sh
cargo run --locked -p rusty-dlna --example gpu_graph -- --help
```

The example captures hashes and SSIM for the first trial of each graph/preset.
Compare those records separately; successful helper exits alone do not establish
frame equality or quality acceptance.

Keeping frames on a device can reduce transfer and CPU costs, as described by
the [NVIDIA FFmpeg guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html#n-hwaccel-transcode-with-scaling),
but fewer transfers do not establish compatibility or end-to-end improvement.
On the tested RTX 3050, removing the download boundary failed when SPS color
metadata changed mid-stream; the existing download path completed. The daemon
therefore retains its current decode choices, frame-context isolation and
portable recovery. Preset comparisons must disclose their different quality
effects; they cannot establish a graph improvement by changing encoder tuning.

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
Delivery uses positioned reads without taking the index lock or changing the
shared Unix seek cursor. Each client owns one 256 KiB read buffer, starts with a
64 KiB read, and has one outstanding blocking read, then drains that buffer under
socket backpressure.
There is no read-ahead queue or helper retained by a blocked socket write.
Growing EOF triggers a fresh metadata/state check and bounded growth wait;
truncation, failed producers, cancellation and incomplete promised ranges end
delivery. Cancellation also interrupts a pending socket write.

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

Resource-aware admission and demand pacing remain experiments. The daemon keeps
its existing fair global helper gate, title ceiling, independent AI-upscale
ceiling and encoder threading defaults. A fixed 1× producer can lose its initial
lead during sustained 2× playback, and a paused browser suspends its MSE downloads
but does not stop a shared producer. A per-viewer pause must not stop another
viewer or a native/download consumer.

The opt-in `resource_budget` server example creates disposable 40-second,
640×360/24-fps SDR H.264/AAC media and compares three explicitly separate arms:
FIFO with automatic threading, FIFO with one-thread codec/filter pools, and
resource admission with those same bounded pools. All retain four global helper
slots. The admission prototype reserves interactive capacity, charges hardware
work for CPU use, limits device/upscale concurrency and gives waiting background
work a turn after two interactive admissions, or one when it has waited 500 ms.
An occupied background slot cannot block the reserved interactive slot. These
weights are experimental assumptions. Thread settings do not cap every internal FFmpeg or
driver thread, and changing them may change encoded bytes.

```sh
cargo run --locked -p rusty-dlna --example resource_budget -- 5 8 > /tmp/resource-budget.tsv 2> /tmp/resource-budget.log
# Optional separate H.264 NVENC workload, still using software H.264 decoding:
cargo run --locked -p rusty-dlna --example resource_budget -- 5 8 gpu > /tmp/resource-budget-gpu.tsv 2> /tmp/resource-budget-gpu.log
cargo test --locked -p rusty-dlna --example resource_budget
```

The requested CPU budget is capped by Rust's advisory
[parallelism estimate](https://doc.rust-lang.org/std/thread/fn.available_parallelism.html);
inaccessible cgroup controllers and VM limits remain
measurement limitations. Each child uses the shared supervisor, bounded capture,
a private process group and a 90-second deadline. Queue admission has a ten-second
deadline. SIGINT/SIGTERM cancel active helpers, join their workers and remove the
temporary fixture directory. Reports include each helper's queue time, first observed 16 KiB,
duration, sampled CPU ticks, threads and RSS; every generated output is decoded
after timing. First bytes are not first-frame measurements. The separate demand
model tests two-hour 2× playback, deliberate pause, shared viewers and lease
expiry using synthetic time; it does not pace real FFmpeg processes. Neither
this model nor the small generated workload establishes production defaults.

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

The incremental index keeps cumulative timing and immutable 256-entry history
chunks. Playlist formatting releases the live index lock and reads a consistent
view, sharing sealed chunks instead of copying every fragment. Index parsing is
bounded to 100,000 fragments, 200,000 top-level boxes, 32 million selected-track
samples and 256 MiB aggregate initialization/fragment metadata, with at most
4 MiB per metadata box. MSE cursors can reach 100,000 while each response still
contains at most 256 fragments. Manifest allocation is limited to 4 MiB for MSE
and 32 MiB for native HLS before formatting request-derived resource URLs.

A process-local completed-index LRU retains at most 16 entries and 16 MiB of
metadata, keyed by pinned output identity and parser revision. Reattachment still
requires the validated completed-output stamp; the index does not substitute for
validation. Replacement, timestamp/length changes and parser changes prevent
reuse. Retained entries own neither media descriptors nor disk sidecars, so disk
eviction and reservations keep their existing contract. Process restart reparses
the index. Active pinned views survive publication or unlinking of their inode.

Native HLS keeps the complete EVENT history and its network cost. Target duration
is frozen for each session/request generation. If a later copied GOP exceeds the
published rounded target, that generation reports restart required; a new
generation chooses the known larger maximum without discarding old segments.
One session's cancellation cannot reset another session's target. This fixes
target mutation but does not establish seamless native Safari recovery for
variable-GOP copies; native-device validation remains necessary before introducing
playlist windows or delta delivery.

| State | `DLNA.ORG_OP` | Seek |
|---|---|---|
| Growing `.part` | `00` (no `Content-Length`) | small Range probe only |
| Finished dest | `01` + `Accept-Ranges` + `Content-Length` | byte Range 206 |
| Source `mtime`/`size` ≠ stamp | dest deleted, job restarts | same as growing |

`remux-p8` streams Profile 7 as Profile 8.1 when `helper_max_jobs >= 2`.
FFmpeg wraps the source video in roughly one-second MP4 fragments. A bounded
converter uses `dolby_vision` 3.4.0 to rewrite each sample's RPU with the same
mode-2 policy as `dovi_tool`, removes enhancement-layer NALs, and passes the
converted fragments through a pipe to the final FFmpeg audio/video mux. Only
the growing final output occupies the transcode cache; the entire video does
not need to be extracted or converted before playback begins.

The converter preserves base-layer VCL bytes, decode/composition timing,
edit lists, and the source Dolby Vision level. Fragment sample sizes and relative
data offsets are updated after conversion. The final `hvc1` MP4 has a Profile-8
`dvvC` record with HDR10 compatibility ID 1 and retains PQ/BT.2020 signaling.
Both mux inputs retain the source clock, and a common nonnegative shift applies
to video and audio together. VFR intervals and selected audio offsets therefore
remain aligned. The `profile8-streaming-rpu-v4-dolby-vision-3.4.0` cache revision
separates this recipe from incompatible cached output.

This policy discards the enhancement layer, including FEL residual image
information; it does not reconstruct the full FEL image. The
[dovi_tool conversion modes](https://github.com/quietvoid/dovi_tool#usage)
describe the Profile-8.1 RPU rewrite. HDR10 fallback separately discards Dolby
Vision metadata and is an explicit change in delivered features.

The streaming converter accepts one unencrypted HEVC track, one picture and
one RPU per sample, and relative fragment offsets. It bounds metadata boxes to
1 MiB, fragment payloads to 64 MiB, RPU input to 64 KiB, samples per fragment to
65,536, and boxes/NALs per parsed container/sample to 4,096. Bounded pipes apply
backpressure; slow output cannot accumulate a whole video in memory or staging.
The two cooperating FFmpeg processes reserve two global helper slots atomically.
Each has null stdin, bounded diagnostics, its own process group, and the shared
job deadline. Cancellation or a failed stage stops and reaps both processes.
A deliberate `helper_max_jobs = 1` retains the sequential file-staging converter
and its whole-video preparation delay, preserving the configured helper ceiling.

Only the final mux can become Growing: its initialization already has final
`dvvC`/`hvc1` signaling, and the bounded fragment index must find a complete
playable copied-video segment with the established dependency look-ahead.
Quota and minimum free space are checked before exposure and throughout the job.
Pressure terminates the producer; it does not start another HDR10 encode.
The mux appends fragments without rewriting exposed initialization bytes.
An unpinned failed attempt may fall back to HDR10; a pinned generation fails
and cannot be replaced underneath a reader. A runtime HDR10 fallback is not
reused under the Profile-8 cache identity. Completed structural validation,
quota admission, and atomic publication still precede reusable cache stamps.
First audio map prefers `aac` / `ac3` / `eac3` over TrueHD / DTS.

Profile-8 progress is available in
`/api/status` → `transcode.web_player.performance.profile8`. Streaming reports
its lifetime in `final_mux`, including preparation and growing delivery. The
sequential converter reports its seven individual stages. Diagnostics retain
at most 64 pipelines, with elapsed times, logical input/output lengths and
optional I/O counters. Application counters measure successful reads/writes;
sampled process counters are incomplete lower bounds and do not measure a disk
device. No path, title or raw diagnostic output is exported.

The `profile8_stages` example measures the sequential converter explicitly,
using disposable output and a shared deadline. It is not a streaming startup
benchmark and does not publish reusable cache output:

```sh
DOVI_TOOL=/path/to/dovi_tool cargo run --locked -p rusty-dlna --example profile8_stages -- /path/to/clip.mkv 5 keep > /tmp/profile8-stages.jsonl
```

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

After structural validation and atomic publication of the finished cache, GET/HEAD
uses `DLNA.ORG_OP=01`, `Content-Length`, `Accept-Ranges: bytes`, and normal
single-range 206/416 behavior. This distinction is derived from job state, not
from the DIDL resource advertisement.

The supervisor bounds stderr by streaming a fixed tail, uses UTF-8-safe lossy
decoding only after capture, applies the configured wall-clock deadline, and
terminates/reaps the process group on cancellation or shutdown. Persisted
per-stream descriptors retain both the source stream index and the audio
ordinal used in `0:a:N`; repeated codecs therefore do not collapse selection.
Successful output passes structural fragmented-MP4 validation before `.part`
is renamed and before the job becomes Complete. The validator shares the
streaming index's bounded box readers but checks every requested audio/video
track, codec initialization (including AVC/HEVC parameter sets, VP9/AV1
configuration and MPEG audio/video descriptors), fragment
sequence, sample sizes and offsets, and per-track decode timestamps. Every
sample extent must lie inside a complete media-data box; tracks without samples,
initialization-only files, truncated boxes, wrong codecs and missing tail fragments
are rejected. It reads box metadata and skips media payloads, without decoding
the movie or opening another media helper.

Decode timestamps may differ by at most 2 ms between consecutive fragments of a
track. Initial muxer priming may shift the decode start by 250 ms; composition
reordering is bounded to 2 seconds. The longest output audio/video track must
reach the known overall source duration minus the requested seek, allowing a 5% shortfall
with a 50 ms minimum and 1 second maximum for container rounding. Output
may exceed that duration by 1.25 seconds for priming and final samples; a seek
that copies video permits up to 10 seconds of preceding-keyframe preroll plus
1 second of final-sample tolerance. Short positive-duration media retains these
subsecond checks. Source tracks can legitimately have very different lengths:
the Dolby Vision fixture contains 10.803 seconds of video and 0.4 seconds of
audio. The catalog records overall duration, not individual track endings, so
the validator checks every track's samples, start, continuity and upper time
bound but compares minimum expected coverage against the longest track only.
It cannot prove that a shorter track reached its individual source ending.
When the catalog has no overall duration, structural and per-track continuity
checks still apply, but no source-coverage comparison is possible. These checks
prove structural completeness within those tolerances; they do not claim full
bitstream decoding or detect corruption inside payloads.

Non-browser remuxing preserves one QuickTime text chapter track when an
audio/video track explicitly references it through `tref/chap`. Its initialization,
sample extents, continuity and upper time bound are validated too. Chapters may
begin later than playback and do not establish the minimum audio/video coverage.
Other non-media tracks and unreferenced text tracks are rejected.

Validation admits at most 33 audio/video tracks (32 audio plus video) and one
referenced chapter track, 200,000 top-level boxes, 32 million samples,
4 MiB per initialization/fragment metadata box and 256 MiB of aggregate metadata.
Only one metadata box and its bounded nested descriptors are retained at a time.
Cancellation/deadline checkpoints run between boxes, track runs and every 4,096
samples. Verification, quota checking and publication share the remaining
original job deadline and `transcode.verify_timeout_secs`; verification never
starts a fresh job budget. A reproducible 60-second, 64×64 H.264/AAC fixture
checks the metadata-only cost in `remux::validation_tests`: 4,254 samples,
123 top-level boxes and 35,185 metadata bytes out of 1,005,058 output bytes on
FFmpeg 6.1, with approximately 1–2 ms validation on the development host. These
figures describe that small fixture, not a throughput guarantee for all movies.

The pinned output inode, length, modification time and change time are checked
before and after validation, before rename, and around stamp publication. The
versioned validation stamp also binds the completed output identity, rejecting
older stamps and metadata-visible replacement or modification. These metadata
checks assume the configured cache remains private to the server and trusted
operator; they are not authenticated payload integrity and cannot distinguish a
same-length rewrite with restored modification time within one tick of the
filesystem change time. Cancellation is serialized with the final Complete
transition. Failed verification or publication removes staging output and any
unpublished final file/stamp.
Growing-fragment delivery continues to use its existing incremental index while
the producer is running.

Finished raw-MP4 GET/HEAD requests and completed HLS/Media Source attachments
refresh cache recency on the completion stamp, with writes throttled to once per
minute. This preserves the validated media's timestamps and stamp contents.
Age and quota eviction use that recency time, falling back to the media's age
for unstamped output. Resume validators include the immutable media identity and
stable stamp identity, so ordinary reads retain both reusable-cache status and
the same `ETag`; changed media still invalidates both. Requested completed output
is reserved before admission, so an older candidate cannot be evicted while its
new reader is being registered. Active generations remain protected regardless
of the recency timestamp. Full cache discovery follows one process-wide
30-second reconciliation cadence; active artifact sizes and final publication
accounting are refreshed separately.

Cache identities include the effective codec, audio, browser-quality, HDR
preservation, Dolby Vision, source, and tool-version inputs. Tool versions are
queried with the same bounded supervisor and cached by executable path plus
file identity. A request-time cache hit only restats that identity and consumes
no helper slot. An identity miss is single-flight, enters the global helper
gate, and observes daemon cancellation; admission, cancellation, deadline, and
query failures do not emit a server cache key. Each lookup rechecks the file
identity, so replacing an executable in place invalidates its cached version
without requiring a rustyDLNA restart. An output-producing fallback may finish
the current request. Its validation stamp identifies the actual successful
recipe, including byte-exact output arguments and bounded Linux device/driver
observations. It never carries the failed primary plan's identity. Lookup may
reuse only a fallback that current negotiation still offers. Stable unsupported
failures may prefer that output for one hour from the immutable media mtime;
recency touches do not extend the preference. Resource pressure, malformed input
and unknown failures remain separate bounded categories and do not suppress a
future primary attempt. There is no device-wide failure blacklist.

The reserved request pathname remains the lookup slot for fallback output, so
existing staging reservations, publication, quota accounting and eviction remain
in force. Fallbacks from different requested plans are not deduplicated across
those slots. Actual codecs, dynamic range, pixel format, quality/preset and bitrate
settings are exposed in the owning generation's status and Stream details.
Historical attempt timing is not persisted with a cache hit.

Fallback requires an unpinned output generation. Pinning before response headers,
indexing or status exposure conservatively ends in-process retry eligibility,
even below 16 KiB. A pinned generation may already have an observer and cannot
be overwritten. Readiness checks and pinning share the replacement lock; a
request that races an unpinned retry waits within its original readiness budget.
All permitted attempts retain the original deadline and
cancellation owner; successful fallback output passes the completed-output
validator before publication under its effective recipe.

Media Source playlists disclose changed encoded streams through
`X-Rusty-Video-Output` and `X-Rusty-Audio-Codec`, after pinning the output. The
browser replaces its empty SourceBuffer before appending initialization bytes
when the permitted fallback changed codecs. Copied streams keep their negotiated
codec declarations, and the playback request/session remain unchanged. This
applies to fresh attempts and validated cached fallbacks; Stream details show
the actual recipe independently of the originally requested plan. Those facts
remain available after playback ends and cannot carry into a newer source.

Browser cache basenames contain a fixed-size digest of the requested cache
identity. The full requested identity, including readable policy revisions, remains
in the job key; the stamp records the actual primary or fallback identity. Adding
a policy revision therefore invalidates the output without risking the
filesystem's per-component filename limit.

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
