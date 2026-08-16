# rustyDLNA

A from-scratch **Rust** DLNA / UPnP MediaServer that speaks the same
on-the-wire dialect as the MiniDLNA 1.3.3-kodi fork, with a process
model that can actually do **live transcode** and shared work without
`fork` + `MAP_SHARED`.

This directory is the workspace. The protocol contract is
[`replica.md`](replica.md) (also at [`docs/replica.md`](docs/replica.md)).
Do not invent new object IDs, SOAP names, or Samsung MIME lies without
updating that file.

MiniDLNA is GPLv2. This tree is GPLv2 because it copies that dialect
(client table, DIDL extras, path strings).

## What is here now

| Path | Role |
|---|---|
| `replica.md` | Full wire spec + gotchas from the C tree |
| `docs/ARCHITECTURE.md` | Threads, tasks, what not to fork |
| `docs/TRANSCODE.md` | When to transcode; HDR10 vs Dolby Vision |
| `docs/INHERITED.md` | Product behavior to keep (dates, NFO, inodes, skips) |
| `docs/minidlna-oracle/` | Copied C headers / client table for diffing |
| `crates/protocol` | Paths, clients, `dc:date`, persist, SOAP names |
| `crates/ssdp` | NOTIFY / M-SEARCH packet builders |
| `crates/soap` | Envelope / Browse Result |
| `crates/http` | URL map, Host 400, media never Keep-Alive |
| `crates/scan` | Junk / sample / unfinished skip rules |
| `crates/transcode` | Client×source decision + ffmpeg argv |
| `crates/server` | `rusty-dlna` binary: accept loop, SOAP/Browse, media GET, SSDP |
| `Dockerfile` | multi-stage Debian / ffmpeg build of `rusty-dlna` |
| `docker-compose.yaml` | live daemon, host network (required for SSDP) |
| `docker-compose.test.yaml` | isolated bridge tests; no published 8200/1900 |
| `docker-compose.override.yaml.example` | template for host paths (copy, do not commit) |
| `.env.example` | template for `RUSTY_DLNA_MEDIA` / cache / conf |
| `restart.sh` | `docker compose build && up -d` |

The accept loop, SQLite library (`files.db`), inotify scan, and remux
path are implemented. Transcode Range is a **file cache** (not a live
ffmpeg pipe). Live LAN ports after cutover are **8200** / **1900**.

## Isolation from the live daemon

The house server is rustyDLNA on host TCP **8200** and UDP **1900**.
Tests and `docker-compose.test.yaml` must not steal those ports
([`docs/CHECKLIST.md`](docs/CHECKLIST.md)).

| Allowed | Not allowed |
|---|---|
| Host `cargo test` (no listen) | `network_mode: host` on rusty **test** containers |
| `docker-compose.test.yaml` (bridge, **no** `ports:`) | Publishing 8200 or 1900 from test compose |
| Listen tests on **18200** / **11900** inside that container | Container name `minidlna` |
| `./scripts/prove.sh` (bridge tests + compose isolation) | Binding test listeners to 8200 / 1900 |

## Build / test

Needs Rust 1.80+ (this host: rustc 1.97, `source /etc/profile.d/rust.sh`).

```bash
cd /root/containers/rustyDLNA

# Host unit tests + compose isolation (does not start rusty listeners)
./scripts/check.sh

# Same tests inside an isolated bridge container; live :8200 must stay up
./scripts/prove.sh
```

Work list, verify commands, and phase gates: **[`docs/CHECKLIST.md`](docs/CHECKLIST.md)**.

Image name: `rusty-dlna:local`. Container name: `rusty-dlna` (never `minidlna`).

## Docker

SSDP is multicast `239.255.255.250:1900`. The **live** compose file
**must** use `network_mode: host`. Publishing port 8200 on a bridge
network is not enough for Kodi to discover the server.

`docker-compose.test.yaml` stays on a bridge with no published ports.

This host currently runs rustyDLNA as `rusty-dlna.service` (host binary).
Starting the container on 8200/1900 requires stopping that unit first.

Point the container at your library with a private `.env` (gitignored):

```bash
cp .env.example .env
# set RUSTY_DLNA_MEDIA / RUSTY_DLNA_CACHE
# optional: RUSTY_DLNA_CONF=./rusty-dlna.live.toml
./restart.sh
curl -sI http://127.0.0.1:8200/ | head
```

`docker-compose.override.yaml` is also supported and gitignored if you
prefer bind mounts over env vars. Do not commit either file.

Same helper as MiniDLNA: `/root/containers/rusty-dlna-container`.

## Design in one paragraph

Keep MiniDLNA’s **bytes** (SSDP, DIDL, client quirks, Kodi dates).
Replace its **lifetime model**. One multi-thread Tokio process: SSDP and
SOAP on the async runtime, original-file reads as ranged file I/O,
remux/transcode as a bounded job that writes a cache file (honest
`Range`) and `Drop`s to kill ffmpeg. Kodi gets `/MediaItems/{id}`
unchanged. A Streamer / Cast client (`NEED_SAFE_VIDEO`) gets a second
`<res>` only when the source is actually unplayable (DV P7 MKV, TrueHD,
…). HDR10 can be preserved. Dolby Vision cannot.

Sibling C tree (oracle, no longer the living server): `../minidlna`.
