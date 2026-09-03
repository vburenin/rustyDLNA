# rustyDLNA

A DLNA / UPnP media server for a local library. It advertises your
video, music, and pictures on the LAN so TVs, Kodi, game consoles, and
phones can browse and play them. There is no account, no cloud, and no
companion app.

The browse tree, object IDs, and renderer behavior form rustyDLNA's stable,
regression-tested compatibility contract. rustyDLNA is GPLv2.

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

**Library maintenance.** rustyDLNA does not write the media root. Titles,
About/plot text, posters, multi-genre `genres/` views, and browser timeline
previews come from operator-generated sidecars. Serving a folder of raw
files works; a good catalog needs the tools in
[`contrib/library/`](contrib/library/README.md). They take the same
`RUSTY_DLNA_MEDIA` path as the server. Without them, clients see filenames
and whatever sidecars already exist.

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

**Embedded web player.** The responsive player at `/` traverses physical media
folders and also searches flat video/audio views without a separate frontend
deployment. Its single mouse/touch/keyboard control surface supports captions,
audio tracks, chapters, browser-local resume/Continue watching, stable queues,
Media Session, fullscreen, Original/Compatible stream modes, held video frames
during seeks, and optional offline JPEG timeline previews. Its HTML,
CSS, and dependency-free JavaScript modules are compiled into the server
binary. Auto tries browser-native playback first; when `[transcode].enable =
true`, unsupported media falls back to bounded fragmented MP4 with selectable
4K 30/16 Mbps, 1080p, 720p, 480p, and 360p profiles, stereo downmix, and
HDR/Dolby Vision to SDR tone mapping. Original remains the no-transcode option.
Set
`[web].encoder = "h264_nvenc"` to use an exposed NVIDIA GPU, with automatic
CUDA decode and scaling for H.264/HEVC sources, capability-gated HEVC Main 10
HDR10 output for compatible HDR10/Dolby Vision Profile 7/8 sources, and
automatic `libx264` SDR fallback if the GPU or browser HDR path cannot start.
Set `[web].enable = false` to remove all
player routes and restore the status page at `/`; `/status` is always the
operator status page. See [the web-player guide](docs/WEB_PLAYER.md).

**Operations.** TOML configuration with unknown keys rejected. A shared,
cancellation-aware graceful-stop budget defaults to 15 seconds; scanner
transactions roll back and helper process groups are terminated and reaped.
`--check` validates the file and the transcode tools on the host.
`--print-effective-config` shows the resolved settings, including complete
remap rules. Both diagnostic modes are read-only: they do not create or migrate
the catalog or UUID and do not run cache maintenance. Logs go to
stderr (`RUST_LOG`). A bounded `/health` endpoint is safe for Compose
healthchecks (no multicast, catalog walk, or inline database integrity scan);
degraded service remains HTTP 200 while unhealthy service is 503. The Docker image runs unprivileged (uid
10001, capabilities dropped).

## Not in this product

These are intentional, not missing todos. See
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) for the full matrix.

- PNG, WebP, and HEIF/HEIC as library pictures
- IPv6, mDNS/Avahi, TiVo, MiniSSDPd
- Online sources, plugins, or built-in remote-access/authentication services
- Upload / import APIs — media enters through read-only configured roots
- Transcoding everything so an old TV can play any file. Remaps are
  explicit. There is no live “make it MPEG-TS” profile pack yet.

## Hardware

Linux on amd64 or arm64. The scanner uses inotify, `/proc`, and rooted
`openat2` I/O; other operating systems are not supported. Discovery is IPv4
SSDP multicast, so the live service needs a real LAN interface (Docker **host
network**). A GPU is not required to browse or to serve original files.

Local, writable disk holds the SQLite catalog, artwork, and optional transcode
cache. The shipped defaults reserve 128 MiB free, cap on-demand JPEGs at
512 MiB, and cap completed transcodes at 50 GiB; size that volume for the
library and for simultaneous remap jobs. Details are in
[`docs/OPERATIONS.md`](docs/OPERATIONS.md).

Optional Compatible playback and `[[remap]]` jobs need FFmpeg on the host.
`libx264` is the CPU path. An NVIDIA GPU with NVENC, exposed through the
NVIDIA Container Toolkit (`web.encoder = "h264_nvenc"`), is the fast path for
browser encodes and HDR tone mapping; if that path cannot start, the job falls
back to software SDR. See [`docs/TRANSCODE.md`](docs/TRANSCODE.md) and
[`docs/WEB_PLAYER.md`](docs/WEB_PLAYER.md).

The library-maintenance tools need Python 3.10+, FFmpeg, FFprobe, and curl.
Timeline-preview generation uses NVDEC when CUDA is present and otherwise
decodes on the CPU; a first full-library pass is a long FFmpeg workload.
[`contrib/library/requirements.txt`](contrib/library/requirements.txt) lists
those host programs. There are no PyPI packages.

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

A complete library is a second step. The server will advertise whatever is
already on disk; it will not fetch posters, write NFO, rebuild genre links,
or generate timeline previews. After the media folder is set, run the
operator tools against that same path:

```bash
export RUSTY_DLNA_MEDIA=/path/to/media   # same folder as in .env
contrib/library/update.sh --dry-run
contrib/library/maintain-library.py --dry-run
```

Python 3.10+, `ffmpeg`, `ffprobe`, and `curl` are required.
[`contrib/library/requirements.txt`](contrib/library/requirements.txt) records
that there are no PyPI packages.

Drop `--dry-run` when the plan looks right. Caches and IMDb dumps stay in
`$RUSTY_DLNA_MEDIA/.rusty-library/`. Optional `TMDB_API_TOKEN` and
`OMDB_API_KEYS` come from the environment; do not put them in Git. See
[the library-tools guide](contrib/library/README.md).

For an Internet-facing reverse proxy, run the separate browser gateway and
point the authenticated HTTPS virtual host at its port instead of TCP 8200:

```bash
./restart-web.sh
```

The `rusty-web` image contains nginx only: it has no media or cache mounts, no
rustyDLNA binary, and no UDP listener. It forwards the browser API/media,
artwork/caption, health, and status routes to the LAN service. UPnP device and
service descriptions, SOAP, GENA, DLNA media/transcode URLs, and icons return
404. The default bind is loopback; set `RUSTY_WEB_BIND_IP` only when a separate
reverse-proxy container must reach it. Authentication and TLS remain the outer
reverse proxy's responsibility. `restart-web.sh` rebuilds and recreates only
this gateway, waits for health, and runs the route-isolation smoke test without
restarting DLNA.

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
Web-player behavior and routes are in
[`docs/WEB_PLAYER.md`](docs/WEB_PLAYER.md).
Release and source obligations are in
[`docs/DISTRIBUTION.md`](docs/DISTRIBUTION.md).
Capacity planning, alerts, persistent-volume ownership, and the native systemd
example are in [`docs/OPERATIONS.md`](docs/OPERATIONS.md).
The reproducible 50k-file scale workload and its reference measurements are in
[`docs/LARGE_LIBRARY_BENCHMARK.md`](docs/LARGE_LIBRARY_BENCHMARK.md).
