# Architecture — rustyDLNA

## Overview

rustyDLNA is a standalone DLNA / UPnP server for a local media library on Linux.
The eight-crate Rust workspace serves discovery, protocol, catalog, and playback
behavior. HTML, CSS, and JavaScript under `crates/server/web/` are embedded in the
server binary. The operator tools in `contrib/library/` are separate programs.
This overview follows `README.md` and the project index in `AGENTS.md`; the linked
product guides remain authoritative for detailed behavior.

## Layout

- `crates/helper/` — bounded helper processes and cancellation
- `crates/protocol/` — shared wire constants and pure protocol behavior
- `crates/ssdp/`, `crates/http/`, `crates/soap/` — protocol parsing and responses
- `crates/scan/` — scanning, watching, metadata, and SQLite persistence
- `crates/transcode/` — media policy and controlled FFmpeg execution
- `crates/server/` — configuration, listeners, dispatch, and embedded web player
- `contrib/library/` — operator-owned library maintenance tools
- `web-gateway/` — separate nginx browser gateway configuration
- `web-tests/`, `testdata/`, `fuzz/` — browser tests, fixtures, and fuzz targets
- `scripts/` — quality checks, benchmarks, and deployment utilities
- `docs/` — current product and operational guides

## Context diagram

(Update me: document component relationships and trust boundaries using
`AGENTS.md` and `PROTOCOL_CONTRACT.md`.)

## Key flows

1. (Update me: summarize catalog publication and browse/playback from the protocol guide.)
2. (Update me: summarize media-job failure and recovery from the transcode/web guides.)

## Data

- Stores: (Update me from `OPERATIONS.md`.)
- External services: (Update me; distinguish the server from separate operator tools.)

## Non-goals

See [Compatibility](./COMPATIBILITY.md) and the README's product exclusions.

## References

- [Documentation index](./INDEX.md)
- [Protocol contract](./PROTOCOL_CONTRACT.md)
- [Web player](./WEB_PLAYER.md)
- [Transcode](./TRANSCODE.md)
- [Operations](./OPERATIONS.md)
- [ADRs](./adr/)
- [Agent contract](../AGENTS.md)
