# Compatibility and product scope

This matrix defines the supported rustyDLNA contract. “Compatible” means the
corresponding object IDs, UPnP actions, resource MIME types, and renderer
behavior are regression-tested. Features listed as unsupported are deliberately
outside the current product contract.

| Area | Status | Contract / intentional difference |
|---|---|---|
| Video library | Supported | First-class scanning, NFO, subtitles, artwork, Browse/Search, original playback/download GETs, and opt-in remap/transcode. |
| Music library | Supported | Tagged FLAC/MP3 and the declared audio extension table; Artist, Album Artist, Album, Genre, Composer, Contributing Artist, Rating, Recently Added, folder, and playlist views. NFO overrides only fields it supplies. |
| Picture library | Supported for JPEG | EXIF date/camera/orientation/rating/album, oriented derivatives, Date/Camera/Album/Rating/Recent/folder/playlist views. JPEG/JPEG_TN is the supported DLNA profile target. |
| PNG, WebP, HEIF/HEIC library images | Unsupported by design | Not advertised or admitted. Adding one requires one shared scan/DIDL/GET/protocol-info entry plus decoder-limit and representative-client tests; extension-only acceptance is not allowed. |
| Playlists | Supported | Bounded M3U/M3U8 and PLS parsing; allowed-root normalization, stable membership, and refresh on change/rename/delete. |
| SSDP / UPnP AV | Supported on IPv4 | Per-interface IPv4 membership, alive/byebye, M-SEARCH, ContentDirectory:1, ConnectionManager:1, Microsoft registrar, and GENA. |
| Browser-only gateway | Supported as a separate container | The unprivileged nginx gateway exposes the browser route/method allowlist without a rustyDLNA binary, media/cache mounts, or UDP listener. DLNA/UPnP endpoints return 404; TLS and authentication belong to the outer reverse proxy. |
| IPv6 HTTP/SSDP | Unsupported | The daemon rejects unsupported address configuration. A future design must specify IPv6 multicast membership, bracketed `LOCATION`/Host, callback validation, and dual-stack identity before enabling it. |
| TiVo discovery/protocol | Unsupported | `enable_tivo` and `tivo_discovery` are rejected configuration keys and no TiVo capability is advertised. |
| Avahi/mDNS | Unsupported | DLNA discovery is SSDP only. |
| MiniSSDPd socket integration | Unsupported | rustyDLNA owns its SSDP sockets; there is no MiniSSDPd client mode. |
| Filesystem upload/import API | Out of scope | rustyDLNA exposes no upload/import endpoint. Media enters through configured read-only roots. |
| Privilege-drop daemon option | Out of scope | The image starts directly as uid/gid 10001. Native services should set `User=`, `Group=`, `NoNewPrivileges=`, and filesystem permissions in the service manager instead of starting privileged. |

## Configuration contract

| Option | rustyDLNA decision |
|---|---|
| `friendly_name`, `network_interface`, `media_dir`, `exclude_dir`, `root_container`, `max_connections` | Supported with strict startup validation. `media_dir` type prefixes are per root. |
| HTTP and SSDP ports | The HTTP port is selected with `--port` or `RUSTY_DLNA_HTTP_PORT`; the SSDP port uses `RUSTY_DLNA_SSDP_PORT`. Environment values take precedence and invalid values fail startup. Ports are not TOML keys. |
| `exclude_file` | Supported with documented ASCII case-insensitive basename `*`/`?` semantics. |
| `presentation_url` | Generated from the selected advertised interface; not user-overridable, preventing an inconsistent address. |
| model name/number | Fixed compatibility identity, with client-specific Xbox behavior; not user-overridable. |
| `strict_dlna` | Not a global switch. Strict checks and client exceptions are applied by identified renderer profile. |
| `force_sort_criteria` | Not a global switch. Required forced ordering is selected by renderer profile; explicit valid SOAP sort criteria still work. |
| `merge_media_dirs` | Virtual Music/Video/Pictures views merge typed roots while Browse Folders preserves each root identity. This behavior is always on and not configurable. |
| `stream_buffer_mb` | Not exposed. Original media uses bounded file reads; encoded output uses a disk-backed growing cache with separate quotas. |
| bookmark retention | `bookmark_retention_days` is a rustyDLNA extension: `0` retains Kodi resume/play-count state indefinitely; a positive value expires it after that many 24-hour days since the last update. |
| `user` | Delegated to the container/service manager as described above. |
| `log_dir`, `log_level` | Logs go to stderr. Level/filter is set with `RUST_LOG`; rotation and storage belong to the runtime. |

Unknown keys are rejected. Relative paths resolve against the selected TOML
file, and `--print-effective-config` provides a secret-free resolved view that
includes complete remap rules. It and `--check` validate without opening or
migrating the catalog, creating a UUID, persisting root mappings, or maintaining
caches.

## Contract evidence

The repository keeps normalized wire/database fixtures in `testdata/oracle/`.
Self-contained contract tests cover rootDesc/SCPDs, SSDP
variants, SOAP faults, protocol-info entry structure, DIDL fields, object IDs,
media classification, virtual views, and representative FLAC/MP3/MP4/MKV/JPEG
scan/Browse/GET behavior. This file is authoritative for rustyDLNA's supported
behavior and intentional exclusions.
