# Architecture

A classic select-loop file server plus `fork` per media GET is a sound
**file server**. It is the wrong shape for a RAM window, an encoder, and
kill-on-disconnect.

rustyDLNA keeps the dialect in `replica.md` and changes the process.

```
                    ┌─────────────────────────────────────┐
   SSDP UDP 1900    │  Tokio multi-thread runtime         │
   SOAP / desc TCP  │    ssdp task                        │
   GENA notify      │    http/soap tasks (Keep-Alive)     │
                    │    client cache (by IPv4 + MAC)     │
                    │           │                         │
                    │           ├─ original file ──► ranged file read
                    │           └─ remux/transcode ──► ffmpeg live fMP4 pipe
                    └─────────────────────────────────────┘
```

## Tasks

| Work | Where | Why |
|---|---|---|
| SSDP announce + M-SEARCH | async UDP | cheap, periodic |
| SOAP Browse/Search | async, one DB pool | shared library |
| Description / art / captions | async, Keep-Alive | persist rule |
| Original `/MediaItems/` | file Range | large `Range` reads; **Connection: close** |
| Transcode | ffmpeg live fMP4 pipe, cap `max_jobs` | first fragment immediately; disconnect kills ffmpeg |
| Scan / inotify | background task | must not stall Browse |

Do **not** `fork` the HTTP server to serve a file. Isolation is a
`JoinHandle` and a cancellation token, not a child that inherits the
SQLite handle and the cache maps.

## Modules (crates)

- `protocol` — the dialect. No I/O.
- `ssdp` / `soap` / `http` — packet and route helpers, then sockets.
- `scan` — skip rules, later inode reuse + NFO.
- `transcode` — `decide(client, source)` then ffmpeg argv.
- `server` — config, runtime, wiring.

A client profile is resolved **once** per TCP peer via `ClientCache`
(25 IPv4 slots, 1 hour TTL; same MAC extends another hour). Later
generic `DLNADOC/1.50` / `UPnP/1.0` must not overwrite a more specific
type (`type < StandardDlna150`). Samsung Series B is not overwritten
by Series A. SOAP and media GET must see the same profile so DIDL
and HTTP MIME lies match.

## DIDL and transcode

Two `<res>` rows when `decide` says `Recode` (remap match):

1. Transcode URL first, `DLNA.ORG_CI=1`, `OP=00` (live pipe), mime
   `video/mp4`.
2. Original listed **second** as a fallback.

No remap row → original only. Never remux “because it is MKV”.

Toshiba / Sony extra `CI=1` rows in the dialect are **lies** (same file).
rustyDLNA should not invent real transcodes for those unless a profile
says the client cannot play the source.

## Seek

Original files: `Accept-Ranges: bytes`, `DLNA.ORG_OP=01`, same as the dialect.

Remux / transcode **serve path** is a live fragmented-MP4 pipe
(`OP=00`, no `Content-Length`). Do not wait for a finished cache file
before the first byte.

## Database

SQLite (or later) with WAL. Object IDs stay `64` / `1` / `2` / `$` hex.
Inode reuse and multi-caption (rustyDLNA DB v13) belong in `scan`, not in
the HTTP crate.

## What we refuse to port as-is

- `last_file` static cache in the media handler
- `process_fork` + `SIGCHLD` as the concurrency model
- `MAP_SHARED` window reset races (the RAM cache can return as a
  userspace ring owned by one task)
- Fake `CI=1` as a substitute for an encoder

## Config sketch

```toml
friendly_name = "rustyDLNA"
port = 8200
network_interface = ["eth0"]
notify_interval = 895
media_dir = ["/media/video"]
exclude_dir = ["incomplete"]

[transcode]
enable = true
encoder = "hevc_nvenc"   # or libx265 / libx264
max_jobs = 2
```

Host paths stay in `rusty-dlna.local.toml` (gitignored).
