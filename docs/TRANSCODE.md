# Transcode

MiniDLNA does not transcode. rustyDLNA does, as a **job**, not as a
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

Empty `[[remap]]` list → every client, including Cast, gets the remux.

## HDR and Dolby Vision

- **HDR10** (PQ `smpte2084` + BT.2020, 10-bit) can be preserved.
  ffmpeg flags: `-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc`,
  `-tag:v hvc1`, 10-bit `p010le`.
- **Dolby Vision Profile 7** (BL+EL+RPU, `compatibility_id=6`) cannot.
  That is what blacks out Google Streamer. The encoder must strip EL/RPU
  (ffmpeg already logs `Skipping NAL 63`). Output is HDR10, not DV.
- CUDA decode of DV P7 failed on the box that proved the offline path
  (`Impossible to convert… Function not implemented`). Encode jobs
  default to **software decode + NVENC** (or libx264), same as that encode.
- Copy mastering display / MaxCLL when present (`-mastering_display`,
  `-max_cll`) so tone-map matches the disc.

## Serve path: file cache (not a live pipe)

`GET /Transcode/{id}` waits for (or reuses) a **cache file** under
`cache_dir`, then serves it with the same byte-`Range` path as
`/MediaItems/`. That is why remux Range is honest.

`ffmpeg_live_args` still builds a stdout pipe
(`frag_keyframe+empty_moov+default_base_moof`, `pipe:1`) for tests and
a future supervisor. It is **not** what the HTTP handler serves today.

Cache jobs:

- Map `0:v:0` and a chosen audio (not “whatever is default”).
- TrueHD / Atmos → AC-3 640k or AAC per `audio_out`.
- `JobGate` caps `max_jobs`; `Drop` kills the ffmpeg process.
- A finished non-empty dest is reused (no re-encode).

If `decide` says original, the handler serves the source file.

## HTTP

Transcoded GETs (file cache):

- `Connection: close`
- `transferMode.dlna.org: Streaming`
- `contentFeatures.dlna.org: … DLNA.ORG_CI=1; DLNA.ORG_OP=01…`
- Honest `Content-Length` / `Accept-Ranges: bytes` (it is a file)
- TimeSeek without Range is still 406

Do not reuse `stream_buffer_mb` for encoded bytes. That window is source
file data.

## What “proper” means here

Not “transcode everything.” Proper means:

1. Know the source (container, codec, HDR, audio) — MiniDLNA DB does not.
2. Know the client (table + `NEED_SAFE_VIDEO`).
3. Hide or demote the unplayable `<res>` for that client.
4. Preserve HDR10; drop DV.
5. Bound encoder jobs; cancel cleanly.
