# Architecture

A classic select-loop file server plus `fork` per media GET is a sound
**file server**. It is the wrong shape for a shared growing cache, supervised
encoders, and multiple readers attaching to one job.

rustyDLNA keeps the dialect in `replica.md` and changes the process.

```
                    ┌─────────────────────────────────────┐
   SSDP UDP 1900    │  Tokio multi-thread runtime         │
   SOAP / desc TCP  │    ssdp task                        │
   GENA notify      │    http/soap tasks (Keep-Alive)     │
                    │    client cache (by IPv4 + MAC)     │
                    │           │                         │
                    │           ├─ original file ──► ranged file read
                    │           └─ remux/transcode ──► growing fMP4 file cache
                    └─────────────────────────────────────┘
```

## Tasks

| Work | Where | Why |
|---|---|---|
| SSDP announce + M-SEARCH | async UDP | cheap, periodic |
| SOAP Browse/Search | async, one DB pool | shared library |
| Description / art / captions | async, Keep-Alive | persist rule |
| Original `/MediaItems/` | file Range | large `Range` reads; **Connection: close** |
| Transcode | ffmpeg growing fMP4 file cache, cap `max_jobs` | first fragment immediately; finished dest is Rangeable |
| Scan / inotify | background task | must not stall Browse |

Do **not** `fork` the HTTP server to serve a file. Isolation is a
`JoinHandle` and a cancellation token, not a child that inherits the
SQLite handle and the cache maps.

## Modules (crates)

- `protocol` — the dialect. No I/O.
- `ssdp` / `soap` / `http` — packet and route helpers, then sockets.
- `scan` — coordinator, SQLite pool/query layer, probes, playlists, NFO,
  sidecars, watch reconciliation, and virtual views.
- `transcode` — `decide(client, source)` then ffmpeg argv.
- `server` — validated config, catalog/DIDL mapping, media serving, SSDP/GENA
  runtime, health/status, and supervised remux wiring.

A client profile is resolved **once** per TCP peer via `ClientCache`
(25 IPv4 slots, 1 hour TTL; same MAC extends another hour). Later
generic `DLNADOC/1.50` / `UPnP/1.0` must not overwrite a more specific
type (`type < StandardDlna150`). Samsung Series B is not overwritten
by Series A. SOAP and media GET must see the same profile so DIDL
and HTTP MIME lies match.

## DIDL and transcode

Two `<res>` rows when `decide` says `Recode` (remap match):

1. Transcode URL first, `DLNA.ORG_CI=1`, `OP=00` (growing representation), mime
   `video/mp4`.
2. Original listed **second** as a fallback.

No remap row → original only. Never remux “because it is MKV”.

Toshiba / Sony extra `CI=1` rows in the dialect are **lies** (same file).
rustyDLNA should not invent real transcodes for those unless a profile
says the client cannot play the source.

## Seek

Original files: `Accept-Ranges: bytes`, `DLNA.ORG_OP=01`, same as the dialect.

Remux / transcode **serve path** follows job state. While the `.part` file is
growing it is streamed with `OP=00` and no `Content-Length`; the handler waits
only for the initial fragment, not the finished file. After ffprobe validation
and atomic rename, the completed cache is served with byte Range,
`Content-Length`, `Accept-Ranges`, and `OP=01`.

Several HTTP readers can attach to the same immutable job key. By default a
probe disconnect does not cancel useful work (`continue_after_disconnect =
true`). An operator can choose immediate last-reader cancellation instead.
Every job has a wall-clock deadline; source replacement and shutdown cancel
jobs, terminate their process groups, and reap their children. Cache quota,
age, and minimum-free-space maintenance protect storage.

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
scan_workers = 16

[transcode]
enable = true
encoder = "hevc_nvenc"   # or libx265 / libx264
max_jobs = 2
```

Host paths stay in `rusty-dlna.local.toml` (gitignored).

Initial and rebuild scans first discover paths, group them by physical
device/inode, and prefer a canonical non-symlink path as the preparation
source. A bounded worker pool performs libav probing and generated-artwork
work once per physical source. Results are then applied to SQLite in discovery
order on the scanner's single writer, so concurrency cannot make object IDs or
transactions nondeterministic. Alias DETAILS rows copy the physical source's
stream metadata and artwork instead of launching duplicate probe/ffmpeg work.
