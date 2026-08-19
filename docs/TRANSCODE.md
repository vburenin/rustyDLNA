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

`transcode.encoder` is the default only for `action = "hdr10"`; a rule-level
`encoder` overrides it. `remux-p8` and `audio-ac3` must copy video. Run
`rusty-dlna --config ... --check` on the deployed host to verify ffmpeg,
ffprobe, compiled encoder support, hardware-device usability, and dovi_tool.
Missing dovi_tool is reported as a warning because Profile-8 jobs retain the
documented HDR10 fallback.

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
sidecar is used. Docker CI regenerates and byte-compares this fixture.

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
rename). The first ~16 KiB (`ftyp` + init) is enough to start playback.
The rest fills in behind the client.

| State | `DLNA.ORG_OP` | Seek |
|---|---|---|
| Growing `.part` | `00` (no `Content-Length`) | small Range probe only |
| Finished dest | `01` + `Accept-Ranges` + `Content-Length` | byte Range 206 |
| Source `mtime`/`size` ≠ stamp | dest deleted, job restarts | same as growing |

`remux-p8` runs `dovi_tool -m 2 convert --discard` (BL + P8.1 RPU) when
the binary is on `PATH`. The pipeline carries the source Dolby Vision level
into a Profile 8 `dvvC` record, writes an `hvc1` fragmented MP4, and retains
PQ/BT.2020 signaling. If dovi_tool is missing or the convert/signaling step
fails, the job falls back to the `hdr10` encode. First audio map prefers `aac`
/ `ac3` / `eac3` over TrueHD / DTS.

Kodi opens several GETs at once. They **attach** to the same job. A
probe disconnect does **not** kill ffmpeg.

- `-movflags frag_keyframe+empty_moov+default_base_moof` (no `+faststart`)
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
