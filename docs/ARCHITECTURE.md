# Architecture

MiniDLNA is a select loop plus `fork` per media GET. That is a sound
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
                    │           ├─ original file ──► spawn_blocking / sendfile
                    │           └─ transcode job ──► ffmpeg supervisor
                    └─────────────────────────────────────┘
```

## Tasks

| Work | Where | Why |
|---|---|---|
| SSDP announce + M-SEARCH | async UDP | cheap, periodic |
| SOAP Browse/Search | async, one DB pool | shared library |
| Description / art / captions | async, Keep-Alive | MiniDLNA persist rule |
| Original `/MediaItems/` | blocking pool | large `Range` reads; **Connection: close** |
| Transcode | dedicated supervisor, cap `max_jobs` | NVENC / CPU; kill process group on drop |
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

A client profile is resolved **once** per TCP peer (MiniDLNA’s 25-slot
cache, 1 hour, do not overwrite a specific type with generic
`DLNADOC/1.50`). SOAP and media GET must see the same profile so DIDL
and HTTP MIME lies match.

## DIDL and transcode

Two `<res>` rows only when the client is `NEED_SAFE_VIDEO` **and**
`decide` says `Transcode`:

1. Optional: original, listed **second** (some clients pick the first).
2. Transcode URL first, `DLNA.ORG_CI=1`, `OP=00` or time-seek, mime
   `video/mp4`.

Kodi (`FLAG_DLNA`, no `NEED_SAFE_VIDEO`) sees **only** the original, same
as MiniDLNA. Never transcode “because it is MKV”.

Toshiba / Sony extra `CI=1` rows in MiniDLNA are **lies** (same file).
rustyDLNA should not invent real transcodes for those unless a profile
says the client cannot play the source.

## Seek

Original files: `Accept-Ranges: bytes`, `DLNA.ORG_OP=01`, same as MiniDLNA.

Live transcode: no honest byte map.

- Advertise `OP=00` or implement `TimeSeekRange.dlna.org`.
- Practical: on `Range`, restart ffmpeg with `-ss` estimated from
  bytes/duration. Document that VBR makes this sloppy.
- Do not Keep-Alive a transcode pipe.

## Database

SQLite (or later) with WAL. Object IDs stay `64` / `1` / `2` / `$` hex.
Inode reuse and multi-caption (MiniDLNA DB v13) belong in `scan`, not in
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
