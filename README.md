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
| `docs/COMPATIBILITY.md` | Supported product scope and intentional MiniDLNA differences |
| `docs/DISTRIBUTION.md` | Release reproducibility, notices, and source obligations |
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
| `.env.example` | template for media / cache-volume / conf settings |
| `rusty-dlna.live.toml.example` | template for uuid, advertise_ip, remaps |
| `restart.sh` | `docker compose build && up -d` |

The accept loop, SQLite library (`files.db`), inotify scan, and remux
path are implemented. Transcode is a **growing fMP4 file cache** (not a
live ffmpeg stdout pipe). `remux-p8` uses `dovi_tool` when present and
falls back to HDR10. Video Browse includes Series (`2$E`) and Genre
(`2$9`) from NFO. The live daemon uses TCP **8200** and UDP **1900**.

Scan policy is explicit in TOML. `exclude_file` entries are basename globs
with ASCII case-insensitive `*`/`?` matching. Dot-prefixed paths stay private
unless `include_hidden=true`; subtitles and generated thumbnails can be
disabled independently. `album_art_names` adds literal folder-art basenames
or `{stem}`/`%s` templates. Thumbnail width (16–4096), JPEG quality (2–31),
optional four-frame filmstrip mode, and the 1–600 second libav/ffmpeg/ffprobe
deadline are configurable. `scan_workers` (1–64) bounds concurrent libav and
thumbnail/artwork preparation; its hardware-aware default is the available
CPU count capped at 16, while SQLite publication remains ordered on one
thread. Hard links and paths reached through directory symlinks are grouped by
device/inode and reuse one probe and one generated-artwork cache entry. The
shipped defaults are shown in
[`rusty-dlna.toml`](rusty-dlna.toml).

`Recently Added` is ordered by filesystem mtime (not metadata date), dedupes
hard-link/symlink aliases by device/inode, and defaults to 200 items with no
age window. Set `recent_limit` and optional `recent_days` to reproduce a
smaller, time-bounded deployment; future clock-skewed mtimes remain eligible.

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

Needs Rust 1.83+. This MSRV permits the patched XML parser required by the
RustSec policy gate.

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
# set RUSTY_DLNA_MEDIA; the persistent rusty-dlna-cache volume is automatic
cp rusty-dlna.live.toml.example rusty-dlna.live.toml
# set advertise_ip, uuid, remaps
# .env: RUSTY_DLNA_CONF=./rusty-dlna.live.toml
./restart.sh
curl -sI http://127.0.0.1:8200/ | head
```

On first start rustyDLNA chooses a usable LAN address and persists a unique
UUID below the cache directory. If address selection is ambiguous it exits
with a configuration error; set `advertise_ip` or `network_interface` in that
case. The image runs as unprivileged uid/gid 10001 with all capabilities
dropped and `no-new-privileges`; custom cache bind mounts must be writable by
that identity. The default Compose deployment instead uses the persistent
Docker-managed volume named by `RUSTY_DLNA_CACHE_VOLUME` (default
`rusty-dlna-cache`). `restart.sh` reuses it, repairs its ownership, proves uid
10001 can write, and waits for container health. The Compose healthcheck reads
the machine-readable `/health` endpoint locally and sends no
multicast traffic.

Kodi resume positions and play counts live in that persistent `files.db`.
`bookmark_retention_days = 0` keeps them indefinitely; a positive value
expires state that many 24-hour days after its last position or play-count
update. Expiration is checked during startup and periodic full library
reconciliation. The live example uses `90` days (three months).

Run `./scripts/compose-smoke.sh` for a disposable bridge-only image/start/scan/
HTTP/shutdown test. It never publishes the production ports.
For multi-homed SSDP, list every desired interface name/address in
`network_interface`; rustyDLNA joins and announces on each address and replies
from the subnet-facing address with a matching `LOCATION`. Root or
`CAP_NET_ADMIN` can run `./scripts/ssdp-netns-e2e.sh`, which proves this across
two isolated veth networks.

`docker-compose.override.yaml` is also gitignored if you prefer extra
bind mounts over env vars. Do not commit `.env`, `rusty-dlna.live.toml`,
or `docker-compose.override.yaml`.

## Design in one paragraph

Keep the locked **bytes** (SSDP, DIDL, client quirks, Kodi dates).
One multi-thread Tokio process: SSDP and SOAP on the async runtime,
original-file reads as ranged file I/O, remux/transcode as a growing
fragmented-MP4 cache (the first fragment is served while the job continues in
the background). Parallel readers share one supervised job; the default
policy lets a job finish after a probe disconnect, while shutdown, deadline,
invalidation, and explicit `continue_after_disconnect=false` cancellation
terminate and reap it. A remap row adds a `/Transcode/` `<res>` only when the
source matches (DV P7, TrueHD, …). HDR10 color signaling is preserved, but
Dolby Vision Profile 7 cannot stay dual-layer and exact mastering-display /
MaxCLL preservation is not claimed.

Music, video, JPEG pictures, playlists, subtitles, and NFO metadata are
first-class supported library content. PNG/WebP/HEIF library images, TiVo,
mDNS/MiniSSDPd, and IPv6 are intentionally outside the current support
contract; see [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) for the complete
matrix.
