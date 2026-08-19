# rustyDLNA execution checklist

This is the work list. Tick items only when the **verify** command
passed. Phase 9 cutover is done: the live daemon is rustyDLNA on
TCP **8200** and UDP **1900**.

Tests and `docker-compose.test.yaml` still use **18200** / **11900** on
a bridge network. They must not publish 8200/1900 or use host networking.

## Isolation rules (every phase)

| Rule | Why |
|---|---|
| Do not `network_mode: host` on rusty **test** containers | Would collide with live SSDP |
| Do not publish `8200` or `1900` from test compose | Host already bound (rustyDLNA) |
| Do not reuse another product's container name | Compose / restart-all confusion |
| Unit tests bind nothing | Packet/string tests; listen tests use 18200/11900 |
| Listen tests use **18200** / **11900** inside a **bridge** network | `rusty_dlna_protocol::isolation` |
| Host `cargo test` is allowed | It does not listen |
| Integration / ffmpeg tests run in `docker-compose.test.yaml` | Isolated target + cargo cache volumes |

Prove isolation after any change that might listen:

```bash
./scripts/prove.sh
```

That script checks test compose is not host-network and does not publish
8200/1900, runs tests **in a bridge container with no published ports**,
and fails if compose isolation is broken. It does **not** require
rustyDLNA on `:8200`.

---

## Phase 0 — Workspace (current)

- [x] Copy `replica.md` and oracle headers
- [x] Architecture / transcode / inherited docs
- [x] Protocol crates + unit tests
- [x] Isolation constants `8200`/`1900` vs `18200`/`11900`
- [x] Codec `[[remap]]` table (not titles); empty list = original
- [x] Test compose file with **no** `network_mode: host` and **no** `ports:`
- [x] `./scripts/prove.sh` (container tests + compose isolation)

**Verify**

```bash
# from the repository root
./scripts/prove.sh
curl -sI http://127.0.0.1:8200/ | grep 'rustyDLNA/'
```

**Prove** `prove.sh` prints `PROVE OK` and test compose still has no host
network and no published 8200/1900.

---

## Phase 1 — Dialect lock

Goal: the C oracle and Rust constants cannot drift silently.

- [x] Test: `ROOTDESC_PATH`, control/event URLs match `docs/oracle/` — `cargo test -p rusty-dlna-protocol oracle::path_literals_appear_in_oracle_paths`
- [x] Test: object IDs `0` `64` `1` `2` `3` match `scanner.h` — `cargo test -p rusty-dlna-protocol oracle::object_ids_appear_in_scanner_h`
- [x] Test: `soapMethods[]` names match `replica.md` / `upnpsoap.c` — `cargo test -p rusty-dlna-protocol oracle::soap_method_names_appear_in_replica`
- [x] Test: `w3c_normalize_date` cases match `w3c_date.c` (19-char → `Z`, year → `YYYY-01-01`, EXIF) — `cargo test -p rusty-dlna-protocol oracle::w3c_normalize_matches_w3c_date_c_cases`
- [x] Test: client table **order** — `SEC_HHP_[PC]` before `SEC_HHP_`; `CrKey` before `DLNADOC/1.50` — `cargo test -p rusty-dlna-protocol oracle::client_table_order_matches_oracle_and_rusty_additions`
- [x] Test: Samsung `video/x-matroska` → `video/x-mkv` — `cargo test -p rusty-dlna-protocol oracle::samsung_mime_and_kodi_flags_locked_to_oracle`
- [x] Test: Kodi UA is `FLAG_DLNA` + captions, **not** `NEED_SAFE_VIDEO` — `cargo test -p rusty-dlna-protocol clients::tests::kodi_from_ua`

**Verify** `cargo test -p rusty-dlna-protocol` (host or container).

**Prove** a new test that reads the oracle `.h` files and asserts the
Rust `&str` constants appear in those files (no hand-copied expected
values that can rot).

---

## Phase 2 — SSDP (no LAN)

Goal: packet bytes match the dialect; **no** multicast on the host.

- [x] Bind M-SEARCH receiver only in the test container on `TEST_SSDP_PORT` (11900) — `RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900 cargo test -p rusty-dlna --test listen_e2e`
- [x] Do not `IP_ADD_MEMBERSHIP` on `239.255.255.250` from the host — no `IP_ADD_MEMBERSHIP` in tree (`rg IP_ADD_MEMBERSHIP` empty); unicast bind only
- [x] NOTIFY alive: 6 types, `LOCATION:http://{ip}:{port}/rootDesc.xml`, `NTS:ssdp:alive`, no space after `HOST:` — `cargo test -p rusty-dlna-ssdp alive_has_six_notifies_and_rootdesc`
- [x] M-SEARCH 200: spaces after `LOCATION:`, `max-age=(interval<<1)+10` — `cargo test -p rusty-dlna-ssdp msearch_response_has_spaces`
- [x] Reject `MAN` not exactly `"ssdp:discover"` — `cargo test -p rusty-dlna-ssdp parse_msearch_rejects_bad_man`
- [x] `ssdp:all` → 6 replies; specific ST → 1 reply — `cargo test -p rusty-dlna-ssdp ssdp_all_is_six_specific_is_one`
- [x] byebye: 6 packets, no LOCATION — `cargo test -p rusty-dlna-ssdp byebye_has_six_no_location`

**Verify** in-container only: send a UDP M-SEARCH to `127.0.0.1:11900`
inside `rusty-dlna-test`.

**Prove** host `ss -ulnp | grep ':1900'` still lists `rusty-dlna` only
(plus whatever else was already there). rusty-dlna must not appear.

---

## Phase 3 — HTTP + description XML

Goal: `/rootDesc.xml` and SCPDs match `replica.md`. Listen on **18200**.

- [x] GET `/rootDesc.xml` → `deviceType` MediaServer:1, three services, icon URLs — `cargo test -p rusty-dlna --lib rootdesc_mediaserver_and_xbox`
- [x] Xbox UA: `modelNumber` `1`, `friendlyName` contains `: 1` — same
- [x] Samsung DCM10 UA: `sec:ProductCap` / `sec:X_ProductCap` — same
- [x] HTTP/1.1 without `Host` → 400 — `cargo test -p rusty-dlna --lib host_and_timeseek_errors`
- [x] `Host: localhost` → 400 (not dotted IPv4) — same
- [x] TimeSeek / PlaySpeed without `Range` → 406 — same
- [x] SOAP/desc Keep-Alive; never persist a future `/MediaItems/` GET — `cargo test -p rusty-dlna-http media_never_keepalive`
- [x] `Server:` contains `DLNADOC/1.50 UPnP/1.0 rustyDLNA/` — `cargo test -p rusty-dlna-protocol server_header_has_dlna_tokens`

**Verify** `curl` against `http://127.0.0.1:18200` **inside** the test
container (not the host).

**Prove** host `curl -sI :8200` is still the live rustyDLNA daemon (tests did not steal the port).

---

## Phase 4 — SOAP Browse / Search

- [x] POST any path with `SOAPAction` `#Browse` works (The dialect ignores URL) — `cargo test -p rusty-dlna --lib soap_caps_and_unknown_path_browse`
- [x] `BrowseDirectChildren` / `BrowseMetadata` / missing ObjectID → 402 — `cargo test -p rusty-dlna --lib missing_objectid_is_402_unknown_is_401`
- [x] Result is XML-escaped DIDL (`&lt;DIDL-Lite`) — `cargo test -p rusty-dlna --lib browse_root_and_kodi_original`
- [x] Roots: `0` (parent `-1`), children include `64` / `1` / `2` as configured — same
- [x] `GetProtocolInfo`, `GetSortCapabilities`, `GetSearchCapabilities` — `cargo test -p rusty-dlna --lib soap_caps_and_unknown_path_browse`
- [x] `X_GetFeatureList` ids `1`/`2`/`3` or `A`/`V`/`I` for DCM10 — `cargo test -p rusty-dlna --lib client_matrix_handlers`
- [x] `X_SetBookmark` + `FLAG_CONVERT_MS` — `cargo test -p rusty-dlna-soap bookmark_convert_ms`
- [x] Search requires `ContainerID`/`ObjectID`; missing scope → 402 — `cargo test -p rusty-dlna --lib search_missing_container_id_is_402`
- [x] `GetCurrentConnectionInfo` missing/malformed ID → 402, nonzero → 701; registrar authorization requires `DeviceID` — `cargo test -p rusty-dlna --lib connection_and_registrar_required_arguments_reach_http_faults`
- [x] Unknown method → HTTP 500 + UPnPError 401 — `cargo test -p rusty-dlna --lib missing_objectid_is_402_unknown_is_401`

**Verify** scripted SOAP against container `:18200`.

**Prove** same SOAP against host `:8200` still returns rustyDLNA
`BrowseResponse` (oracle gold in `testdata/oracle/`). Diff shapes.

---

## Phase 5 — Scan, DB, object IDs

- [x] Skip junk / `sample` / unfinished (`rusty_dlna_scan`) — `cargo test -p rusty-dlna-scan scan_skips_junk_sample_exclude_and_reads_nfo_captions`
- [x] `exclude_dir` / `exclude_file` — same
- [x] Inode reuse for symlink aliases — `cargo test -p rusty-dlna-scan inode_reuse_for_hardlink_alias`
- [x] NFO year → `dc:date` — `cargo test -p rusty-dlna-scan nfo_year_becomes_ten_char_date`
- [x] Multi-caption `/Captions/{id}/{n}.ext` — scan test plus `caption_from_path` in protocol
- [x] Per-root `V,`/`A,`/`P,` admission and root-qualified identity —
      `per_root_masks_keys_and_persisted_relocation_survive_reconcile`
- [x] Valid tagged FLAC/MP3/MP4/MKV/JPEG fixtures and checksums —
      `checked_fixtures_match_minidlna_scan_didl_and_get_contract`
- [x] No scan of host library paths from the test container unless a
      **fixture** tree is copied in (never mount the live video dataset
      read-write) — `testdata/rusty-dlna.test.toml` `media_dir = ["library"]`

**Verify** fixture library of a few tiny files in `testdata/`.

**Prove** rustyDLNA cache path is not opened (`lsof` / container mounts).

---

## Phase 6 — Original media GET

- [x] `/MediaItems/{id}.{ext}` `strtoll` ignores ext — `cargo test -p rusty-dlna-protocol strtoll_ignores_media_extension`
- [x] `Accept-Ranges: bytes`, `OP=01`, `CI=0`, `Connection: close` — `cargo test -p rusty-dlna --lib original_get_and_two_ranges`
- [x] Empty `DLNA_PN` on HEVC MKV remux — `dlna_org_features(None, …)` (no `DLNA.ORG_PN=` when PN is empty)
- [x] Range 416 / 400 — `cargo test -p rusty-dlna --lib original_get_and_two_ranges`
- [x] Optional RAM window must not bind extra ports — no extra listen besides HTTP/SSDP

**Verify** serve a 1 MiB fixture; `curl -r` two ranges.

**Prove** host `:8200` is still the live rustyDLNA daemon; tests did not
`sendfile` the live library from the test container.

---

## Phase 7 — Transcode (opt-in)

- [x] Kodi + DV MKV → `ServeOriginal` (already unit-tested) — `cargo test -p rusty-dlna-transcode kodi_ignores_cast_only_rule`
- [x] `CrKey` + DV MKV / TrueHD → `Transcode`, drop DV, keep HDR10 flags — `cargo test -p rusty-dlna-transcode cast_p7_hits_hdr10_remap`
- [x] `CrKey` + HDR10 MP4 AC-3 → original — `cargo test -p rusty-dlna-transcode cast_p8_is_not_p7` (P8/HDR10 not matching `hdr = "dv-p7"`)
- [x] ffmpeg supervisor: cancellation/deadline/shutdown terminate and reap the
      process group; `max_jobs` caps titles — remux supervisor tests.
- [x] Background remux: growing fMP4 `.part`, shared job across Kodi’s parallel
      GETs, first fragment before completion
      (`transcode_get_serves_growing_file_before_completion`,
      `grow_file_emits_ftyp_before_process_exits`). A probe disconnect follows
      the explicit `continue_after_disconnect` policy.
- [x] Transcode URL **first** in DIDL for `NEED_SAFE_VIDEO` only — `cargo test -p rusty-dlna --lib crkey_dvp7_remap_first_kodi_original`
- [x] `[[remap]]` matches codec/hdr/audio/client; first row wins
- [x] CUDA decode not required (software decode + NVENC) — test remap uses `copy` / `libx264`

**Verify** ffmpeg jobs only inside the test container, input = fixture
(not the live remux unless copied to a scratch dir). GPU optional.

**Prove** no NVENC session on the host from rusty during unit prove.
Live `:8200` still 200.

---

## Phase 8 — Client matrix (wire)

Run each UA against the **container** (18200), not 8200.

| Client | Prove |
|---|---|
| Kodi | original MKV `<res>`, `dc:date` has `Z` or 10 chars |
| `SEC_HHP_[PC]` | not Samsung BASICVIEW |
| `SEC_HHP_[TV]` | `A`/`V`/`I` + `x-mkv` |
| `[BD]J5500` | no `DLNA.ORG_PN` |
| Xbox | rootDesc `modelNumber=1` |
| `CrKey` | transcode `<res>` first when source is DV MKV |
| Generic `DLNADOC/1.50` | not `NEED_SAFE_VIDEO` |

- [x] Phase 8 UA matrix against handlers / container `:18200` — `cargo test -p rusty-dlna --lib client_matrix_handlers` and `RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900 cargo test -p rusty-dlna --test listen_e2e`

**Prove** Kodi Browse dialect matches the gold file
(`testdata/oracle/`) — dates, ids, mime — not the transcode path.

---

## Phase 9 — Cutover (do not do until 1–8 are green)

- [x] Live daemon serves the configured `media_dir` on 8200/1900.

Only then:

1. `./scripts/prove.sh` green
2. Tell the operator
3. `docker stop rusty-dlna` **and** disable `restart: always` / compose
   so it does not come back
4. Run rustyDLNA with host network, ports 8200/1900
5. `curl -sI :8200` shows `rustyDLNA/`
6. Kodi refresh; Streamer plays a DV title via transcode or sidecar

Phase 9 is done. rustyDLNA is the LAN daemon. Tests stay on 18200/11900.

---

## Phase 10 — Artwork HTTP + DIDL `albumArtURI`

- [x] Sidecar `{stem}-poster.jpg` / folder `poster.jpg` indexed into `ALBUM_ART` / `DETAILS.ALBUM_ART`; inode clone copies art id; art files skipped as library items — `cargo test -p rusty-dlna-scan art_sidecar_indexed_and_cloned`
- [x] `GET /AlbumArt/{artId}-{detailId}.jpg` 200 `image/jpeg`; Browse extra JPEG_TN `<res>` or `albumArtURI` — `cargo test -p rusty-dlna --lib album_art_get_and_didl`
- [x] Xbox `?albumArt=true` and image Streaming/Range rejection; bounded,
      oriented `/Thumbnails/` and `/Resized/` derivatives have identity-aware
      cache keys, atomic publication, quotas, and decoder limits.

**Verify** fixture `testdata/library/video/movie.mkv` + `movie-poster.jpg`.

**Prove** isolation: tests stay on 18200/11900; host `:8200` is not a test target.

---

## Phase 11 — NFO beyond date

- [x] Structured NFO writes TITLE/COMMENT/GENRE/CREATOR/ARTIST/DISC/TRACK — `cargo test -p rusty-dlna-scan nfo_title_plot_show_season`
- [x] Folder `tvshow.nfo` inherited up to media root — `cargo test -p rusty-dlna-scan tvshow_nfo_inherited_by_episode`
- [x] DIDL `dc:title` is `DETAILS.TITLE` (`Fixture Movie`) — `cargo test -p rusty-dlna --lib browse_uses_nfo_title_not_filename`
- [x] Existing date tests still pass — `cargo test -p rusty-dlna-scan nfo_year_becomes_ten_char_date`

**Prove** Kodi 1905 `Z` / 10-char date regressions still pass.

---

## Phase 12 — Caption headers (Samsung / Kodi)

- [x] Samsung `sec:CaptionInfoEx` + `CaptionInfo.sec` header — `cargo test -p rusty-dlna --lib samsung_captioninfoex_and_header`
- [x] Kodi `*` Filter has caption `<res>`, no `sec`/`pv` — `cargo test -p rusty-dlna --lib kodi_caption_res_no_sec_by_default`
- [x] Samsung BDP no caption `<res>` — `cargo test -p rusty-dlna --lib samsung_bdp_no_caption_res`

**Prove** existing `FLAG_CAPTION_RES` Kodi Browse still lists `/Captions/{id}/{n}.srt`.

---

## Phase 13 — Persistent bookmarks

- [x] `BOOKMARKS` keyed by detail id survives `LibraryDb::open` — `cargo test -p rusty-dlna-scan bookmark_survives_reopen`
- [x] `CONVERT_MS` dialect — `cargo test -p rusty-dlna-soap bookmark_convert_ms`
- [x] `X_SetBookmark` then Browse `upnp:lastPlaybackPosition` — `cargo test -p rusty-dlna --lib setbookmark_then_browse_position`
- [x] `UpdateObject` playCount / lastPlaybackPosition — `cargo test -p rusty-dlna --lib updateobject_playcount_and_position`
- [x] Missing `PosSecond` returns 402 without clearing an existing bookmark — `cargo test -p rusty-dlna --lib setbookmark_missing_position_is_402_without_clearing_state`
- [x] `UpdateObject` rejects missing/malformed/read-only/mismatched tag lists with 402/702/703/705/706 — `cargo test -p rusty-dlna --lib updateobject_rejects_missing_malformed_and_unsupported_tag_arguments`
- [x] `CurrentTagValue` is an atomic optimistic-concurrency guard; stale updates return 702 without mutation — `cargo test -p rusty-dlna --lib updateobject_current_value_is_an_optimistic_concurrency_guard`
- [x] Kodi Platinum percent-encoded/trailing-slash `UpdateObject` round trip — `cargo test -p rusty-dlna --lib kodi_encoded_updateobject_persists_and_browses_resume_position`
- [x] Failed SQLite writes return UPnP 501 without catalog drift — `cargo test -p rusty-dlna --lib bookmark_database_failure_returns_action_failed_without_catalog_drift`
- [x] Bookmark writes advance `SystemUpdateID` and emit every affected parent through `ContainerUpdateIDs`, invalidating Kodi's Platinum Browse cache — `cargo test -p rusty-dlna --lib kodi_bookmark_update_invalidates_every_cached_parent_container`
- [x] Configurable indefinite/90-day retention, migration timestamp, and full-reconcile publication — `cargo test -p rusty-dlna-scan bookmark`

---

## Phase 14 — GENA notify

- [x] Subscribe 400/412 rules, peer IPv4, SID — `cargo test -p rusty-dlna --lib gena_subscribe_rules`
- [x] Catalog bump NOTIFY `NTS:upnp:propchange` + `SystemUpdateID` — `cargo test -p rusty-dlna --lib gena_notify_on_catalog_bump`

**Prove** notify only goes to the test callback; isolation ports unchanged.

---

## Phase 15 — Real Search + SortCriteria

- [x] Parsed boolean Search supports `contains`, `doesNotContain`, `=`, `!=`,
      `<`, `<=`, `>`, `>=`, `derivedfrom`, `exists`, parentheses, AND/OR,
      escaped quotes, and explicit 708 failures — SOAP parser and large-catalog
      query tests.
- [x] `upnp:class derivedfrom object.item.videoItem` returns no folders —
      `search_class_derivedfrom_video`
- [x] Case-insensitive parsed Filter controls every optional DIDL element and
      attribute while required fields and client exceptions remain explicit —
      filter unit tests plus `browse_listed_filter_omits_res_size`.
- [x] Unparseable SortCriteria + `FLAG_DLNA` → 709 — `cargo test -p rusty-dlna-soap bad_sort_is_709_for_dlna`
- [x] `FLAG_FORCE_SORT` Browse track order — `cargo test -p rusty-dlna --lib browse_force_sort_track_order`

**Prove** VLC container Search still returns folders.

---

## Phase 16 — `/status`

- [x] `GET /` and `GET /status` list Video count (non-zero on fixture) — `cargo test -p rusty-dlna --lib status_lists_video_count`

**Prove** optional `curl` only against container `:18200`, never host `:8200`.

---

## Phase 17 — 25-slot client cache

- [x] Cache keeps Kodi when generic UA follows — `cargo test -p rusty-dlna --lib cache_keeps_kodi_when_generic_ua_follows`
- [x] Cache expires after one hour — `cargo test -p rusty-dlna --lib cache_expires_after_one_hour`

**Prove** CrKey Browse remap and CrKey GET `/Transcode/` still match when GET UA is only `DLNADOC/1.50`.

---

## Phase 18 — SSDP byebye + multi-iface

- [x] Byebye six packets, no LOCATION — `cargo test -p rusty-dlna-ssdp byebye_has_six_no_location`
- [x] Byebye on drop — `RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900 cargo test -p rusty-dlna --test listen_e2e ssdp_byebye_on_drop`
- [x] Join/announce/reply on every selected IPv4 interface with matching
      source and `LOCATION`; ambiguous named interfaces are rejected — unit
      selection tests and `scripts/ssdp-netns-e2e.sh`.

**Prove** host `:8200`/`:1900` still only the live daemon. Tests do not join `239.255.255.250`.

---

## Phase 19 — Video Series (`2$E`) + Genre (`2$9`)

NFO already has `showtitle` / `season` / `episode` / `genre`. Flattening
into `Show - Episode` stays for All Video. Virtual trees are aliases of
the same `DETAIL_ID`.

- [x] Seed `2$E` Series and `2$9` Genre under Video
- [x] Store `showtitle` in `DETAILS.ALBUM`; episode title is `TITLE` with
      `{show} - ` stripped under Series
- [x] Series → optional `Season N` / `Specials` → episode `REF_ID`
- [x] Genre splits `"Drama / Crime"` into two folders
- [x] Movies (no `showtitle`) stay out of Series; still join Genre
- [x] Re-NFO / inode clone rewrites aliases; empty season/show/genre
      folders prune (roots stay)

**Verify**

```bash
cargo test -p rusty-dlna-scan series_and_genre_trees_from_nfo
cargo test -p rusty-dlna --lib browse_series_seasons_and_genre
```

**Prove** `RUSTY_DLNA_HTTP_PORT=18200 RUSTY_DLNA_SSDP_PORT=11900 cargo test -p rusty-dlna --test listen_e2e series_genre_and_remux_e2e`

---

## Phase 20 — `remux-p8` actually remuxes

`-c:v copy` is not Profile 8.1. Convert RPU with `dovi_tool -m 2 convert
--discard` when present; otherwise fall back to the `hdr10` encode.

- [x] Pipeline: annex-B HEVC → `dovi_tool` P8.1 → fMP4 (copy video)
- [x] Map a lossy audio track (`aac`/`ac3`/`eac3`) instead of `0:a:0`
- [x] Missing `dovi_tool` or convert failure → `hdr10` fallback (log error)
- [x] Cache stamp (`mtime`+`size`); source replace rebuilds dest

**Verify**

```bash
cargo test -p rusty-dlna-transcode remux_p8_pipeline_and_audio_pick
cargo test -p rusty-dlna-transcode remux_p8_falls_back_without_dovi
cargo test -p rusty-dlna-transcode cache_stamp_invalidates_on_source_change
```

---

## Phase 21 — Remux seek / first-play contract

Growing-file cache, not a stdout pipe. Document and lock it.

| State | Headers | Seek |
|---|---|---|
| Growing `.part` | `OP=00`, no `Content-Length` | small Range probe only |
| Finished dest | `OP=01`, `Accept-Ranges`, `Content-Length` | byte Range 206 |
| Source newer than stamp | rebuild | same as growing |

- [x] Finished remux Range 206
- [x] First GET still returns headers before ffmpeg exits
- [x] Stale dest is deleted and rebuilt
- [x] `docs/TRANSCODE.md` matches this table (no “live pipe” claim)

**Verify**

```bash
cargo test -p rusty-dlna --lib remux_finished_range_and_stale_rebuild
cargo test -p rusty-dlna --lib transcode_get_serves_growing_file_before_completion
```

---

## Phase 22 — Art when there is no sidecar

Priority: sidecar → embedded attached pic → one-shot thumbnail.

- [x] Extract `AV_DISPOSITION_ATTACHED_PIC` to `cache/art/{sha1}.jpg`
- [x] Else `ffmpeg -ss 1 -frames:v 1` thumbnail (fail closed)
- [x] Sidecar still wins; `/Thumbnails/` and `/Resized/` use the selected art
      or image source, apply EXIF orientation, and enforce configured limits.

**Verify**

```bash
cargo test -p rusty-dlna-scan art_embedded_and_thumbnail_fallback
cargo test -p rusty-dlna --lib album_art_get_and_didl
```

---

## Commands cheat sheet

```bash
# Live rustyDLNA
curl -sI http://127.0.0.1:8200/ | head

# Host unit tests (no listen)
source /etc/profile.d/rust.sh
cargo test --workspace

# Isolated container tests + compose isolation
./scripts/prove.sh

# Compose file sanity (must fail if someone adds host net)
./scripts/assert-isolation.sh
```

## Definition of done for “it works”

A phase is done when:

1. Automated tests for that phase pass **in** `rusty-dlna-test`
2. `assert-isolation.sh` passes
3. Host `:8200` `Server:` still contains `rustyDLNA/` (tests did not steal it)
4. The checklist box is ticked with the command that proved it
