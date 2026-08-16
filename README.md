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
| `crates/server` | `rusty-dlna` binary (`--check` today) |

The accept loop, SQLite library, and ffmpeg process manager are **not**
implemented yet. The crates that exist already encode the dialect so the
server can be filled in without guessing.

## Isolation from live MiniDLNA

The house server is the `minidlna` container: host network, TCP **8200**,
UDP **1900**. rustyDLNA must not bind those until cutover
([`docs/CHECKLIST.md`](docs/CHECKLIST.md) phase 9).

| Allowed | Not allowed |
|---|---|
| Host `cargo test` (no listen) | `network_mode: host` on rusty containers |
| `docker-compose.test.yaml` (bridge, **no** `ports:`) | Publishing 8200 or 1900 |
| Listen tests on **18200** / **11900** inside that container | Container name `minidlna` |
| `./scripts/prove.sh` | `docker stop minidlna` |

## Build / test

Needs Rust 1.80+ (this host: rustc 1.97, `source /etc/profile.d/rust.sh`).

```bash
cd /root/containers/rustyDLNA

# Host unit tests + prove :8200 is still MiniDLNA
./scripts/check.sh

# Same tests inside an isolated bridge container, then re-check MiniDLNA
./scripts/prove.sh
```

Work list, verify commands, and phase gates: **[`docs/CHECKLIST.md`](docs/CHECKLIST.md)**.

## Design in one paragraph

Keep MiniDLNA’s **bytes** (SSDP, DIDL, client quirks, Kodi dates).
Replace its **lifetime model**. One multi-thread Tokio process: SSDP and
SOAP on the async runtime, original-file reads on a blocking pool,
transcode as a bounded job supervisor that `Drop`s to kill ffmpeg.
Kodi gets `/MediaItems/{id}` unchanged. A Streamer / Cast client
(`NEED_SAFE_VIDEO`) gets a second `<res>` only when the source is
actually unplayable (DV P7 MKV, TrueHD, …). HDR10 can be preserved.
Dolby Vision cannot.

Sibling C tree (oracle, still the living server): `../minidlna`.
