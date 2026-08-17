# rustyDLNA

A **Rust** DLNA / UPnP MediaServer. The wire dialect (SSDP, DIDL, client
quirks, Kodi dates) is locked in [`replica.md`](replica.md). Do not invent
new object IDs, SOAP names, or Samsung MIME lies without updating that file.

This tree is GPLv2.

## What is here

| Path | Role |
|---|---|
| `replica.md` | Full wire spec |
| `docs/specs/` | Official UPnP Forum PDFs (UDA, MediaServer, CDS, CMS). IEC 62481 / DLNA Guidelines are not redistributable — see that README |
| `docs/ARCHITECTURE.md` | Threads, tasks, process model |
| `docs/TRANSCODE.md` | When to transcode; HDR10 vs Dolby Vision |
| `docs/INHERITED.md` | Product behavior to keep (dates, NFO, inodes, skips) |
| `docs/oracle/` | Locked C header snippets used by dialect tests |
| `crates/protocol` | Paths, clients, `dc:date`, persist, SOAP names |
| `crates/ssdp` | NOTIFY / M-SEARCH packet builders |
| `crates/soap` | Envelope / Browse Result |
| `crates/http` | URL map, Host 400, media never Keep-Alive |
| `crates/scan` | Junk / sample / unfinished skip rules |
| `crates/transcode` | Client×source decision + ffmpeg argv |
| `crates/server` | `rusty-dlna` binary: accept loop, SOAP/Browse, media GET, SSDP |
| `Dockerfile` | multi-stage Debian / ffmpeg build |
| `docker-compose.yaml` | live daemon, host network (required for SSDP) |
| `docker-compose.test.yaml` | isolated bridge tests; no published 8200/1900 |
| `docker-compose.override.yaml.example` | template for host paths (copy, do not commit) |
| `.env.example` | template for media / cache / conf paths |
| `rusty-dlna.live.toml.example` | template for uuid, advertise_ip, remaps |
| `restart.sh` | `docker compose build && up -d` |

The accept loop, SQLite library (`files.db`), inotify scan, and remux
path are implemented. Transcode is a **growing fMP4 file cache** (not a
live ffmpeg stdout pipe). `remux-p8` uses `dovi_tool` when present and
falls back to HDR10. Video Browse includes Series (`2$E`) and Genre
(`2$9`) from NFO. The live daemon uses TCP **8200** and UDP **1900**.

## Isolation from the live daemon

Tests and `docker-compose.test.yaml` must not steal 8200/1900
([`docs/CHECKLIST.md`](docs/CHECKLIST.md)).

| Allowed | Not allowed |
|---|---|
| Host `cargo test` (no listen) | `network_mode: host` on **test** containers |
| `docker-compose.test.yaml` (bridge, **no** `ports:`) | Publishing 8200 or 1900 from test compose |
| Listen tests on **18200** / **11900** | Binding test listeners to 8200 / 1900 |
| `./scripts/prove.sh` |  |

## Build / test

Needs Rust 1.80+.

```bash
./scripts/check.sh
./scripts/prove.sh
```

Image: `rusty-dlna:local`. Live container name: `rusty-dlna`. Test container: `rusty-dlna-test`.

## Docker

SSDP is multicast `239.255.255.250:1900`. The **live** compose file
**must** use `network_mode: host`. Publishing port 8200 on a bridge
is not enough for clients to discover the server.

`docker-compose.test.yaml` stays on a bridge with no published ports.

Stop any other process bound to 8200/1900 before starting the container.

Host-specific paths and identity belong in **gitignored** files:

```bash
cp .env.example .env
# set RUSTY_DLNA_MEDIA, RUSTY_DLNA_CACHE, optional RUSTY_DLNA_MEDIA_AT
cp rusty-dlna.live.toml.example rusty-dlna.live.toml
# set advertise_ip, uuid, remaps
# .env: RUSTY_DLNA_CONF=./rusty-dlna.live.toml
./restart.sh
curl -sI http://127.0.0.1:8200/ | head
```

`docker-compose.override.yaml` is also gitignored if you prefer extra
bind mounts over env vars. Do not commit `.env`, `rusty-dlna.live.toml`,
or `docker-compose.override.yaml`.

## Design in one paragraph

Keep the locked **bytes** (SSDP, DIDL, client quirks, Kodi dates).
One multi-thread Tokio process: SSDP and SOAP on the async runtime,
original-file reads as ranged file I/O, remux/transcode as a live
fragmented-MP4 pipe (first fragment immediately; disconnect kills
ffmpeg). A remap row adds a `/Transcode/` `<res>` only when the source
matches (DV P7, TrueHD, …). HDR10 can be preserved. Dolby Vision
Profile 7 cannot stay dual-layer.
