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
  (`Impossible to convert… Function not implemented`). Live jobs should
  default to **software decode + NVENC**, same as that encode.
- Copy mastering display / MaxCLL when present (`-mastering_display`,
  `-max_cll`) so tone-map matches the disc.

## Live ffmpeg shape

`ffmpeg_live_args` builds a **pipe**, not a `.mp4` on disk:

- No `-movflags +faststart` (needs a finished file).
- Use `frag_keyframe+empty_moov+default_base_moof` for fMP4, or MPEG-TS.
- Map `0:v:0` and a chosen audio (not “whatever is default” — some remuxes
  default to a non-English AC-3).
- TrueHD / Atmos → AC-3 640k (or copy an existing AC-3 track).
- Supervisor: spawn, read stdout → socket, `kill` on client drop / seek
  restart. Cap `max_jobs` (a consumer NVENC is not a farm).

Offline sidecar MP4s remain valid: if `decide` says original, MiniDLNA’s
sendfile path (here: blocking read) is enough. Live transcode is for
titles you refuse to pre-encode.

## HTTP

Transcoded GETs:

- `Connection: close`
- `transferMode.dlna.org: Streaming`
- `contentFeatures.dlna.org: … DLNA.ORG_CI=1; DLNA.ORG_OP=00…` (or time-seek)
- No honest `Content-Length` (chunked or estimated)
- TimeSeek without Range is still 406 unless we implement TimeSeek

Do not reuse `stream_buffer_mb` for encoded bytes. That window is source
file data.

## What “proper” means here

Not “transcode everything.” Proper means:

1. Know the source (container, codec, HDR, audio) — MiniDLNA DB does not.
2. Know the client (table + `NEED_SAFE_VIDEO`).
3. Hide or demote the unplayable `<res>` for that client.
4. Preserve HDR10; drop DV.
5. Bound encoder jobs; cancel cleanly.
