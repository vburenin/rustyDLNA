# rustyDLNA

A DLNA / UPnP media server for a local library. It advertises your
video, music, and pictures on the LAN so TVs, Kodi, game consoles, and
phones can browse and play them. There is no account, no cloud, and no
companion app.

The browse tree, object IDs, and client quirks match MiniDLNA, so a
ReadyMedia library should look familiar after a switch. rustyDLNA is
GPLv2.

## Capabilities

**Discovery and playback.** Clients find the server over IPv4 SSDP
(UDP 1900) and stream over HTTP (TCP 8200 by default). Original files
are served with byte-range seek. Several network interfaces can
announce at once; each subnet gets a matching address.

**Video.** Scans configured folders, watches for changes, and builds
Movie / Series / Genre / folder / Recently Added views. Kodi-style NFO
supplies titles, show names, dates, and genres. Sidecar posters, fanart,
and subtitles (`.srt`, `.ass`, `.ssa`, `.vtt`, `.sub`, `.smi`) are
first-class. Resume position and play count are stored for Kodi.

**Music.** Tagged files (FLAC, MP3, and the other declared audio types)
appear under Artist, Album Artist, Album, Genre, Composer, Contributing
Artist, Rating, Recently Added, folders, and playlists. NFO overrides
only the fields it actually contains.

**Pictures.** JPEG libraries with EXIF date, camera, orientation,
rating, and album, plus Date / Camera / Album / Rating / Recent /
folder / playlist views. Oriented thumbnails are generated when needed.

**Playlists.** M3U, M3U8, and PLS, limited to files under allowed media
roots, and refreshed when the playlist file changes.

**Library hygiene.** Incomplete downloads, sample/trailer junk, recycle
bins, and Synology `@eaDir` trees are skipped. Basename exclude globs,
directory excludes, and hidden-file policy are configurable. Hard links
and directory-symlink aliases share one probe and one artwork cache
entry. Recently Added is filesystem mtime, newest first, with optional
size and age caps.

**Renderer awareness.** The server identifies Kodi, Samsung, Xbox, Sony,
LG, Panasonic, Cast/Chromecast, BubbleUPnP, and generic DLNA clients,
and applies the matching MIME and browse quirks so those devices accept
the listing.

**Optional remux / transcode.** Default is to serve the original.
`[[remap]]` rules match container, video codec, HDR, audio, and *which
player asked* — not titles. Actions are leave it, remux Dolby Vision
Profile 7 to Profile 8.1, encode HDR10, or convert audio to AC3. Empty
rules means nobody is remuxed, including Cast.

**Operations.** TOML configuration with unknown keys rejected.
`--check` validates the file and the transcode tools on the host.
`--print-effective-config` shows the resolved settings. Logs go to
stderr (`RUST_LOG`). A `/health` endpoint is safe for Compose
healthchecks (no multicast). The Docker image runs unprivileged (uid
10001, capabilities dropped).

## Not in this product

These are intentional, not missing todos. See
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) for the full matrix.

- PNG, WebP, and HEIF/HEIC as library pictures
- IPv6, mDNS/Avahi, TiVo, MiniSSDPd
- Online sources, plugins, a web player, or remote access
- Upload / import APIs — media enters through read-only configured roots
- Transcoding everything so an old TV can play any file. Remaps are
  explicit. There is no live “make it MPEG-TS” profile pack yet.

## Run it

SSDP is multicast. The live container **must** use host networking.
Publishing port 8200 on a bridge is not enough for clients to discover
the server. Stop anything else bound to 8200/1900 first.

```bash
cp .env.example .env
# set RUSTY_DLNA_MEDIA to the host folder that should appear as the library

cp rusty-dlna.live.toml.example rusty-dlna.live.toml
# set advertise_ip (or network_interface) when the LAN address is ambiguous
# .env: RUSTY_DLNA_CONF=./rusty-dlna.live.toml

docker compose up -d --build
curl -sI http://127.0.0.1:8200/ | head
```

On first start the server picks a usable LAN address and persists a
UUID under the cache volume. If several addresses look equally valid it
exits; set `advertise_ip` or `network_interface`. Cache and the SQLite
library (including Kodi bookmarks) live in the Docker volume named by
`RUSTY_DLNA_CACHE_VOLUME` (default `rusty-dlna-cache`).
`./restart.sh --clean` discards that volume and starts with an empty catalog.
`bookmark_retention_days = 0` keeps resume state forever; a positive
value expires it that many days after the last update.

Do not commit `.env`, `rusty-dlna.live.toml`, or
`docker-compose.override.yaml`.

Shipped defaults are in [`rusty-dlna.toml`](rusty-dlna.toml). Remap
examples and HDR notes are in [`docs/TRANSCODE.md`](docs/TRANSCODE.md).
Release and source obligations are in
[`docs/DISTRIBUTION.md`](docs/DISTRIBUTION.md).
