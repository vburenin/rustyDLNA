# Plan: library / client surface (Phases 10–18)

Phase 9 cutover is done. Transcode and the video Browse/GET path work.
These phases close the gaps that make rustyDLNA look unfinished next to
MiniDLNA **for the video + Kodi + Streamer product**. Music/image as a
first-class library is **out of scope** here (see Deferred).

Isolation is unchanged: tests and `docker-compose.test.yaml` stay on
**18200 / 11900**, bridge, no published 8200/1900. Tick a box only when
the listed `cargo test` (or prove command) passes.

When implementing, append these phases to [`docs/CHECKLIST.md`](docs/CHECKLIST.md)
so the work list lives with the rest of the project.

---

## Constraints (every phase)

- Wire bytes stay in [`replica.md`](replica.md). Do not invent object IDs,
  SOAP names, or DIDL tags.
- Schema tables already exist: `ALBUM_ART`, `BOOKMARKS`, `CAPTIONS`,
  `DETAILS` columns `TITLE/CREATOR/ARTIST/ALBUM/GENRE/COMMENT/DISC/TRACK/ALBUM_ART`.
  Prefer filling them over new tables.
- Inode aliases: NFO / poster updates rewrite **every** `DETAILS` row
  with the same device+inode (`sync_inode_aliases` in MiniDLNA;
  `copy_stream_to_inode_aliases` is the existing pattern).
- No `network_mode: host` on test compose. No listen on 8200/1900 from
  tests. `./scripts/prove.sh` after any listen-adjacent change.
- Do **not** port `/Resized/` on-the-fly scale, `ffmpegthumbnailer`,
  TiVo, Avahi, or `stream_buffer_mb` in these phases.

---

## Phase 10 — Artwork HTTP + DIDL `albumArtURI`

**Why it hurts:** Kodi sidecars (`Movie-poster.jpg`, `poster.jpg`) sit
next to files and are ignored. Browse has no poster. `/AlbumArt/` is
routed then 404s.

**Current hooks:** `is_album_art_name`, `ALBUM_ART` table, `DETAILS.ALBUM_ART`,
`HttpRoute::AlbumArt` / `Thumbnail` / `Resized` (last two stay 404).
`handle()` falls through `_` for art.

### Scan

- [ ] On video insert / inode clone, call `find_album_art(path)`:
      `{stem}-poster.jpg/.png`, `{stem}-fanart.jpg/.png`, folder
      `poster.jpg` / `Poster.jpg` / `poster.png` (case variants MiniDLNA
      lists). First hit wins; poster before fanart.
- [ ] PNG (and any non-JPEG still) converted to JPEG once, stored under
      `cache_dir/art/{sha1}.jpg`. JPEG sidecars referenced in place.
- [ ] `INSERT` into `ALBUM_ART(PATH)` (reuse row if same path); set
      `DETAILS.ALBUM_ART`. Copy art id on inode clone.
- [ ] Poster / fanart mtime change updates every alias of that inode
      (same rule as MiniDLNA). Deleting the last alias may drop the
      cached JPEG; do not delete a shared sidecar.
- [ ] Art files themselves stay skipped as library items
      (`is_album_art_name` already).

### HTTP

- [ ] `GET /AlbumArt/{artId}-{detailId}.jpg` — `strtoll` on the leading
      integer is `ALBUM_ART.ID` (ext and `-{detailId}` decorative).
      `transferMode: Interactive`, `contentFeatures: DLNA.ORG_PN=JPEG_TN`,
      Keep-Alive allowed. Missing row → 404.
- [ ] Xbox `GET /MediaItems/{id}?albumArt=true` rewritten to the item’s
      album-art JPEG (`FLAG_MS_PFS`).
- [ ] `Streaming` / `Range` on an image → 406 (dialect).
- [ ] `/Thumbnails/` and `/Resized/` remain 404 (no EXIF extract, no
      live scale).

### DIDL

- [ ] Video items, **not** `FLAG_MS_PFS`: extra `<res>`  
      `http://{ip}:{port}/AlbumArt/{artId}-{detailId}.jpg` with
      `DLNA.ORG_PN=JPEG_TN`.
- [ ] Audio / containers: `<upnp:albumArtURI>` same URL. Samsung also
      `dlna:profileID="JPEG_TN"`.
- [ ] Load `ALBUM_ART` into `MediaItem` / `DidlObject` (new field).
      `load_catalog` SELECT must include `d.ALBUM_ART`.

**Files:** `crates/scan/src/lib.rs`, `crates/scan/src/db.rs`,
`crates/scan/src/watch.rs`, `crates/soap/src/lib.rs` (`DidlObject`,
`emit_didl_object`), `crates/server/src/lib.rs` (`handle`, `item_resources`,
`to_didl_snap`), `crates/http/src/lib.rs`, `crates/protocol/src/paths.rs`.

**Verify**

```bash
cargo test -p rusty-dlna-scan art_sidecar_indexed_and_cloned
cargo test -p rusty-dlna --lib album_art_get_and_didl
```

Fixture: `testdata/library/video/movie.mkv` + `movie-poster.jpg` (tiny
JPEG). Browse the item → `albumArtURI` or extra JPEG_TN `<res>`.
`GET /AlbumArt/1-….jpg` → 200 `image/jpeg`.

**Prove** `./scripts/prove.sh`. Host `:8200` still rustyDLNA.

---

## Phase 11 — NFO beyond date

**Why it hurts:** `testdata/library/video/movie.nfo` already has
`<title>Fixture Movie</title>` and we still advertise the filename.
Kodi plot / show / season never appear.

**Current hooks:** `nfo_date_from_text` only. `DETAILS` already has
`TITLE, CREATOR, ARTIST, GENRE, COMMENT, DISC, TRACK`. `MediaItem` and
DIDL emit only `title` + `date`.

### Parse (`rusty_dlna_scan::nfo`)

- [ ] Structured parse (not date-only). Episode/movie file:
      `title` / `episodetitle` / `showtitle` → display title
      (`Show - Episode` when both exist).
- [ ] `plot` → `COMMENT` (truncate DIDL `dc:description` to 384 chars
      on emit, dialect).
- [ ] Joined `<genre>` with ` / `.
- [ ] `director` / `credits` → `CREATOR`; else `studio`.
- [ ] `studio` or `showtitle` → `ARTIST`.
- [ ] `season` → `DISC`, `episode` → `TRACK`.
- [ ] Date unchanged: `premiered` then `aired` then `year` then mtime.
- [ ] Folder `tvshow.nfo`: title / plot / genre / studio inherited by
      episodes that do not set their own. Walk parent dirs up to the
      media root.
- [ ] Skip NFO larger than 64 KiB (MiniDLNA).

### Apply

- [ ] `insert_detail` / update path writes TITLE + those columns, not
      just filename + date.
- [ ] `OBJECT.NAME` stays the **file stem** (Browse Folders). DIDL
      `dc:title` uses `DETAILS.TITLE`.
- [ ] Inode clone copies the metadata columns (already in
      `clone_detail_for_path`). Later NFO/poster mtime on any alias
      rewrites every row for that inode.
- [ ] Inotify: `.nfo` / `tvshow.nfo` / poster change re-reads even when
      the video bytes are unchanged (`watch.rs` already notices `.nfo`
      names — hook the re-parse).

### DIDL

- [ ] `DidlObject` fields: `creator`, `description`, `artist`, `actor`,
      `album`, `genre`, `track`, `season`, `episode`.
- [ ] Emit `dc:creator`, `dc:description`, `upnp:artist` / `actor` /
      `genre`, `upnp:episodeSeason` / `upnp:episodeNumber` when set.
- [ ] Filter bits come in Phase 15. Until then emit these fields on
      empty / `*` filter (Kodi sends `*`).

**Files:** new `crates/scan/src/nfo.rs`, `lib.rs` (`nfo_for` → structured),
`db.rs` (`insert_detail` / `update_detail_nfo` / `load_catalog` SELECT),
`watch.rs`, `soap` emit, `server` snapshot.

**Verify**

```bash
cargo test -p rusty-dlna-scan nfo_title_plot_show_season
cargo test -p rusty-dlna-scan tvshow_nfo_inherited_by_episode
cargo test -p rusty-dlna --lib browse_uses_nfo_title_not_filename
```

Extend `movie.nfo` (or a dedicated fixture) with plot/genre. Browse
`dc:title` is `Fixture Movie`, not `movie`.

**Prove** existing date tests still pass
(`nfo_year_becomes_ten_char_date`, Kodi 1905 `Z` / 10-char).

---

## Phase 12 — Caption headers (Samsung / Kodi)

**Why it hurts:** sidecar `.srt` is stored and `/Captions/` serves it,
but Samsung asks `getCaptionInfo.sec` and looks for `CaptionInfo.sec` /
`sec:CaptionInfoEx`. Without those, TVs play the file with no subs.

**Current hooks:** `FLAG_CAPTION_RES` extra `<res>` already in
`item_resources`. No `sec:` / `pv:` tags. Media GET ignores
`getCaptionInfo.sec`.

- [ ] Parse Browse `Filter`. Empty / `*` omits vendor `pv:` / `sec:`
      **except Samsung**, which gets `sec` by default (`replica.md` §11).
- [ ] DIDL namespaces: add `xmlns:sec` / `xmlns:pv` / `xmlns:dlna` only
      when those bits are on.
- [ ] `sec:CaptionInfoEx sec:type="{ext}"` per caption, indexed URL
      `/Captions/{id}/{n}.{ext}`.
- [ ] Filter `pv:subtitleFileType` / `pv:subtitleFileUri` on the
      **primary** `<res>`: type `SRT`, URI `/Captions/{id}.srt` (first
      caption only).
- [ ] Media GET: if request has `getCaptionInfo.sec` and the item has
      captions, add  
      `CaptionInfo.sec: http://{ip}:{port}/Captions/{id}.srt`.
- [ ] Do **not** add caption `<res>` for Samsung BDP (`SEC_HHP_BD*` has
      no `FLAG_CAPTION_RES` — folder-browse bug).
- [ ] AllShare `SEC_HHP_[PC]` stays non-Samsung (already locked).

**Files:** `crates/soap/src/lib.rs` (`SoapCall.filter`, `emit_didl`,
`DIDL_SCHEMAS` variants), `crates/server/src/lib.rs` (`item_resources`,
`media`), `crates/http` media header helper.

**Verify**

```bash
cargo test -p rusty-dlna --lib samsung_captioninfoex_and_header
cargo test -p rusty-dlna --lib kodi_caption_res_no_sec_by_default
cargo test -p rusty-dlna --lib samsung_bdp_no_caption_res
```

**Prove** existing `FLAG_CAPTION_RES` Kodi Browse test still lists
`/Captions/{id}/{n}.srt`.

---

## Phase 13 — Persistent bookmarks

**Why it hurts:** `X_SetBookmark` writes a process `HashMap`. Restart
clears resume. DIDL never emits `upnp:lastPlaybackPosition` /
`sec:dcmInfo`. `UpdateObject` is an empty 200.

**Current hooks:** `BOOKMARKS(ID, SEC, WATCH_COUNT)` table unused.
`bookmark_seconds` + `CONVERT_MS` unit-tested. `update_id` already
increments on library change.

- [ ] `X_SetBookmark`: write `BOOKMARKS` keyed by **detail id** (not
      object id string). `CONVERT_MS` ÷1000; `< 30` → 0. Empty 200 body.
- [ ] Load bookmarks into `App` on start (or read-through on Browse).
      Drop the HashMap-only path.
- [ ] DIDL item: `upnp:lastPlaybackPosition` is **raw seconds** (not
      `H:MM:SS`). `CONVERT_MS` multiplies by 1000 on the way out.
- [ ] Samsung: `sec:dcmInfo`  
      `CREATIONDATE=0,FOLDER={title},BM={sec}` when `sec` filter/default.
- [ ] `UpdateObject`: parse `CurrentTagValue` / `NewTagValue`.
      `upnp:lastPlaybackPosition` → `BOOKMARKS.SEC` (same 30s / −1
      dialect as MiniDLNA). `upnp:playbackCount` **or** Kodi’s
      `upnp:playCount` → `WATCH_COUNT`. Unknown tags ignored. Missing
      ObjectID → 402; unknown object → 701.
- [ ] `upnp:playbackCount` in DIDL when watch count > 0.

**Files:** `crates/scan/src/db.rs` (`get/set_bookmark`, `set_watch_count`),
`crates/soap/src/lib.rs` (`dispatch_simple` / new `dispatch_update_object`),
`crates/server/src/lib.rs` (persist, DIDL), `MediaItem` or side lookup.

**Verify**

```bash
cargo test -p rusty-dlna-scan bookmark_survives_reopen
cargo test -p rusty-dlna-soap bookmark_convert_ms   # existing
cargo test -p rusty-dlna --lib setbookmark_then_browse_position
cargo test -p rusty-dlna --lib updateobject_playcount_and_position
```

**Prove** restart fixture: set bookmark, `LibraryDb::open` again, Browse
still has `lastPlaybackPosition`.

---

## Phase 14 — GENA notify

**Why it hurts:** `SUBSCRIBE` returns `SID` + `Timeout` and stores
nothing. `update_id` increments on scan/inotify (`apply_catalog`) but
no client is told. Kodi/Platinum subscribe and never see new files
except by refresh/restart.

**Dialect (`replica.md` §9):**

- New subscribe: `CALLBACK: <http://peer-ipv4[:port]/path>`, `NT: upnp:event`.
- Callback host **must** be the TCP peer IPv4 else **412**.
- `SID` + `Callback` together → 400. Missing `NT` on new → 400.
- Renew: `SID` without `NT`. Unknown SID → 412.
- Renew response **forces** `Timeout: Second-300`.
- Max 500 subscribers. Default timeout 300 if omitted.
- Notify:

```
NOTIFY {path} HTTP/1.1
Host: {ip}{:port}
Content-Type: text/xml; charset="utf-8"
NT: upnp:event
NTS: upnp:propchange
SID: {sid}
SEQ: {n}
Connection: close
```

Body: `<e:propertyset>` with `SystemUpdateID` (same counter as
`GetSystemUpdateID` / Browse `UpdateID`).

### Work

- [ ] Subscriber table on `App` (SID, callback URL, service, timeout,
      seq, created). Persist not required (MiniDLNA is in-memory too).
- [ ] `gena()` implements the 400/412 rules. Pass **peer IPv4** into
      `handle` (accept loop currently discards `_peer`).
- [ ] On successful subscribe, send initial `SEQ: 0` notify.
- [ ] `apply_catalog` / full scan: bump `update_id` **and** persist via
      `LibraryDb::set_update_id` (counter is RAM-only today), then
      notify ContentDirectory subscribers. Increment `SEQ` per subscriber.
- [ ] Unsubscribe + timeout GC. `UNSUBSCRIBE` unknown SID → 412.
- [ ] ConnectionManager / Registrar subscribe: accept + SID; propertyset
      can be the evented CM vars (`SourceProtocolInfo` / `SinkProtocolInfo`
      / `CurrentConnectionIDs`) — no notify storm required until those
      change.

**Files:** new `crates/server/src/events.rs`, `lib.rs` (`handle` peer,
`gena`, `apply_catalog`), `crates/http/src/desc.rs` already advertises
`sendEvents="yes"` on `SystemUpdateID`.

**Verify**

```bash
cargo test -p rusty-dlna --lib gena_subscribe_rules
cargo test -p rusty-dlna --lib gena_notify_on_catalog_bump
```

Second test: local TCP listener as Callback, trigger `apply_catalog`,
assert one `NTS:upnp:propchange` with the new `SystemUpdateID`.

**Prove** isolation: notify only goes to the test callback, never to
LAN :8200 clients.

---

## Phase 15 — Real Search + SortCriteria

**Why it hurts:** `GetSearchCapabilities` / `GetSortCapabilities`
advertise MiniDLNA’s lists. Search dumps every container/item.
`SortCriteria` is ignored. DLNA TVs that Search-by-title get garbage;
`FLAG_FORCE_SORT` clients (Panasonic / NetFront) get filename order.

**Current hooks:** Search walks `catalog.containers` then `catalog.items`
with only `object.container` / `object.item` class sniff. Browse sorts
folders-first, ASCII case-insensitive title. `SoapCall` has no
`sort_criteria` / `filter` (Filter added in Phase 12).

- [ ] Parse `SearchCriteria` the dialect way: `contains` / `derivedfrom`
      / `=` on `dc:title`, `dc:creator`, `dc:date`, `upnp:class`,
      `upnp:artist`, `upnp:genre`, `upnp:album`, `upnp:actor`, `@id`,
      `@parentID`, `@refID`. Unknown clause → no match (do not dump-all).
      Empty / `*` / missing → `1=1` (all in scope).
- [ ] Scope: if `ContainerID` / `ObjectID` is not root, search that
      subtree only (including `refID` aliases).
- [ ] `upnp:class derivedfrom "object.item.videoItem"` must not return
      folders (today `object.item` is a substring of nothing useful and
      the dump is unfiltered).
- [ ] Parse `SortCriteria`: `+`/`-` `dc:title`, `dc:date`, `upnp:class`,
      `upnp:album`, `upnp:episodeNumber`, `upnp:originalTrackNumber`.
      Unparseable + `FLAG_DLNA` → SOAP **709**.
- [ ] Client defaults when SortCriteria empty:
      `FLAG_FORCE_SORT` → `CLASS, DISC, TRACK, TITLE`;
      LG → `CLASS, TITLE`. Else keep folders-first + title.
- [ ] Apply the same sort on Browse (not only Search).
- [ ] Honor Filter bits from Phase 12 on Search emit too
      (`FILTER_DC_DATE` etc.). A Filter that omits `dc:date` must not
      emit `<dc:date>`.

**Files:** new `crates/soap/src/search.rs` + `sort.rs`, `server` Browse /
Search paths, `scan` Catalog helpers (`search_page`).

**Verify**

```bash
cargo test -p rusty-dlna-soap search_title_contains
cargo test -p rusty-dlna-soap search_class_derivedfrom_video
cargo test -p rusty-dlna-soap bad_sort_is_709_for_dlna
cargo test -p rusty-dlna --lib browse_force_sort_track_order
```

**Prove** existing VLC “search for containers” case still returns
folders (`derivedfrom object.container` / class contains
`object.container`).

---

## Phase 16 — `/status`

**Why it hurts:** `/` and `/status` print the friendly name. You cannot
see scan progress, library size, or remux jobs without logs.

Keep it HTML, Refresh 20s, same spirit as MiniDLNA — not a new product UI.

- [ ] Counts: audio / video / image (`MIME` prefix), distinct video
      inodes, videos with `ALBUM_ART > 0`, caption rows, `updateID`,
      DB path.
- [ ] While scanning: current path (write `cache_dir/scan.status` from
      the walker; MiniDLNA format is fine).
- [ ] Remux: `max_jobs`, active title ids, dest `.part` / finished.
- [ ] Client cache size (after Phase 17; until then omit or `0`).
- [ ] `GET /` same body as `/status` (dialect presentation URL).
- [ ] Keep-Alive allowed.

**Files:** `crates/server/src/lib.rs` (replace the one-line HTML),
optional `scan` progress file in `lib.rs` walker.

**Verify**

```bash
cargo test -p rusty-dlna --lib status_lists_video_count
```

Fixture library → body contains `Video` and a non-zero count.

**Prove** `curl -s http://127.0.0.1:18200/status` inside the test
container only.

---

## Phase 17 — 25-slot client cache

**Why it hurts:** `identify()` is per-request UA only. Architecture.md
and `replica.md` §8 require a 25-slot IPv4 cache, 1 hour TTL, and “do
not overwrite a specific type with generic `DLNADOC/1.50`”. SOAP Browse
and a later media GET from the same TV can disagree on remap / MIME.

**Oracle:** `docs/oracle/clients.c` `SearchClientCache` / `AddClientCache`.

- [ ] `ClientCache` (25 slots) on `App`: IPv4, optional MAC, profile
      pointer, `age`, connection count.
- [ ] `handle(&req)` becomes `handle(&req, peer: SocketAddr)` (Phase 14
      already needs peer for GENA). Identify:
      1. specific UA / X-AV / FriendlyName on **this** request
      2. else cache hit if age ≤ 3600s, or same MAC extends another hour
      3. else table match; `AddClientCache`
- [ ] Do not overwrite `type < StandardDlna150` with generic
      `DLNADOC/1.50` / `UPnP/1.0`. Samsung Series B not overwritten by A.
- [ ] MAC: Linux `/proc/net/arp` or `SIOCGARP` for the peer. Failure is
      fine (cache still keyed by IPv4).
- [ ] Optional same phase: inbound SSDP `NOTIFY` sniff (Roku
      `Allegro-Software-RomPlug`, `SamsungMRDesc.xml`, DigiOn DiXiM) to
      **pre-fill** the cache. Fetch LOCATION only if no more-specific
      entry exists. Skip if this is too much; it is not required for
      Kodi/Cast.

**Files:** new `crates/protocol/src/client_cache.rs` or
`crates/server/src/clients.rs`, `server` accept + `identify`,
`docs/ARCHITECTURE.md` (mark cache as implemented).

**Verify**

```bash
cargo test -p rusty-dlna --lib cache_keeps_kodi_when_generic_ua_follows
cargo test -p rusty-dlna --lib cache_expires_after_one_hour
```

**Prove** CrKey Browse remap and CrKey GET `/Transcode/` still match
when the GET UA is only `DLNADOC/1.50`.

---

## Phase 18 — SSDP byebye + multi-iface

**Why it hurts:** Packet builders exist (`notify_byebye`) but shutdown
never sends them. Clients keep a dead LOCATION until `max-age=1800`.
One advertise IP / one `IP_ADD_MEMBERSHIP`. `network_interface` in TOML
is unused. NOTIFY is doubled with no 150–250 ms gap; M-SEARCH has no
13–30 ms jitter.

- [ ] On SIGTERM / SIGINT / `serve` return: send byebye **twice** (six
      types × 2), no `LOCATION`/`SERVER`/`CACHE-CONTROL`.
- [ ] Second NOTIFY alive pass waits 150–250 ms (`replica.md` §1).
- [ ] M-SEARCH reply jitter 13–30 ms (`ssdp:all`) / 13–20 ms (specific).
- [ ] `network_interface = ["eth0", …]`: resolve IPv4s, join multicast
      **per iface**, send NOTIFY from a socket bound to that iface
      (`IP_MULTICAST_IF`). `advertise_ip` remains the LOCATION host when
      set; else the iface that received the search (`IP_PKTINFO` if
      practical, else first configured iface).
- [ ] Empty `network_interface` keeps today’s single-iface behavior.
- [ ] Do not join `239.255.255.250` from unit tests. Live-port guard
      unchanged.

**Files:** `crates/ssdp/src/lib.rs` (jitter helpers are testable),
`crates/server/src/lib.rs` (`ssdp_loop`, `serve` shutdown), `main.rs`
signal.

**Verify**

```bash
cargo test -p rusty-dlna-ssdp byebye_has_six_no_location   # existing
cargo test -p rusty-dlna --test listen_e2e ssdp_byebye_on_drop
```

**Prove** `./scripts/prove.sh`. Host `ss -ulnp | grep ':1900'` still only
the live daemon.

---

## Order and dependencies

```
10 Artwork ──┐
11 NFO     ──┼── 12 Caption headers (Filter + sec/pv)
             ├── 13 Bookmarks (DIDL fields)
             └── 15 Search/Sort (needs TITLE/GENRE)
14 GENA        (needs peer addr; 17 also needs it)
16 /status     (independent; client-cache line waits for 17)
17 Client cache
18 SSDP
```

Ship **10 → 13** first (visible in Kodi/Samsung Browse). 14 and 16 are
small and unstick ops. 15 needs 11. 17/18 are reliability.

Do not start 15 until 11’s TITLE/GENRE are in `MediaItem`. Do not start
17 until `handle` has a peer address (land that with 14).

---

## Deferred (explicitly not these phases)

- Music Genre/Artist/Album trees, playlists (`.m3u`/`.pls`), ID3/FLAC tags
- Pictures by Date/Camera, PNG-as-library-item, `/Resized/`, `/Thumbnails/`,
  `ffmpegthumbnailer`
- Toshiba/Sony lying extra `CI=1` `<res>`
- `stream_buffer_mb`, sendfile, TiVo, Avahi, `minissdpd`
- SOAP Keep-Alive (closed on purpose for VLC)
- `merge_media_dirs`, `wide_links`, `user=`, log reopen, i18n
- `exclude_file` glob (`*`/`?`) — one-liner, do it if you touch
  `path_excluded` in 10/11

---

## Definition of done

A phase is done when:

1. Its **Verify** tests pass in `rusty-dlna-test` (or host `cargo test`
   when the phase does not listen).
2. `./scripts/prove.sh` is green.
3. Existing Phase 8 UA matrix still passes
   (`cargo test -p rusty-dlna --lib client_matrix_handlers`).
4. Live `:8200` was not stolen.

After 10–13 a Kodi refresh should show NFO titles, posters, and resume
position; a Samsung TV should see captions. After 14, adding a file
updates subscribed clients without a Kodi restart.
