# rustyDLNA — protocol notes

This document is a **wire-level description of this tree**, not a generic UPnP
textbook and not a second implementation. A replica that wants to be mistaken
for this daemon must emit the same bytes on SSDP, description XML, SOAP/DIDL,
GENA, and HTTP media. Internals (SQLite schema, inotify, scan) appear only
where they define object IDs or DIDL fields.

Identity:

| Symbol | Value |
|---|---|
| `SERVER_NAME` | `rustyDLNA` |
| `SERVER_VERSION` | crate version (`CARGO_PKG_VERSION`) |
| `Server:` | `{os} DLNADOC/1.50 UPnP/1.0 rustyDLNA/{version}` |

Example `Server:` header: `Linux DLNADOC/1.50 UPnP/1.0 rustyDLNA/0.1.0`.

Default listen port is **8200** (`runtime_vars.port` in `src/server`).
Default SSDP announce interval is **895** seconds (conf sample says 900; the
binary default is 895). UUID string is `uuidvalue[] = "uuid:........"` 42
chars (`src/upnpglobalvars.c`).

There is **no transcode**. Extra DIDL `<res>` rows that claim `DLNA.ORG_CI=1`
still point at `/MediaItems/{id}.{ext}` and serve the original file.

---

## 1. SSDP

Source: `src/minissdp.c`.

### Bind and multicast

| Item | Literal |
|---|---|
| Group | `239.255.255.250` (`SSDP_MCAST_ADDR`) |
| Port | `1900` (`SSDP_PORT`) |
| Notify TTL | `4` (`IP_MULTICAST_TTL`) |
| Loopback | `IP_MULTICAST_LOOP = 0` |
| Linux receive bind | `239.255.255.250:1900` plus `IP_PKTINFO` |
| Non-Linux receive bind | `INADDR_ANY:1900` (binding the group would make NOTIFY appear to come from the multicast address; some clients then ignore later unicast-source announces) |
| Membership | `IP_ADD_MEMBERSHIP` per configured LAN iface |
| Notify send socket | bound to the iface unicast address; `IP_MULTICAST_IF` set to that iface |

A replica must use **host / multicast-capable networking**. Publishing TCP 8200
on a bridge does not make clients discover the server.

### Service type list (`known_service_types[]`)

Index 0 is the live `uuidvalue` pointer (the device UUID, not a URN).

| i | ST / NT stem | Version suffix |
|---|---|---|
| 0 | `uuidvalue` (e.g. `uuid:aabbccdd-…`) | none |
| 1 | `upnp:rootdevice` | none |
| 2 | `urn:schemas-upnp-org:device:MediaServer:` | `1` |
| 3 | `urn:schemas-upnp-org:service:ContentDirectory:` | `1` |
| 4 | `urn:schemas-upnp-org:service:ConnectionManager:` | `1` |
| 5 | `urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:` | `1` |

For `i > 1` the on-wire token is the stem **plus** the character `1`.

USN construction (`SendSSDPResponse` / `SendSSDPNotifies`):

- `i == 0`: `USN: {uuidvalue}`
- `i == 1`: `USN: {uuidvalue}::{stem}`
- `i > 1`: `USN: {uuidvalue}::{stem}1`

`CACHE-CONTROL` / notify lifetime is `(notify_interval << 1) + 10` seconds.
With the compiled default 895 that is `max-age=1800`.

Every announce is sent **twice** (`dup < 2`). The second pass of NOTIFY waits
150–250 ms (`_usleep(150000, 250000)`).

### Unsolicited NOTIFY alive

```
NOTIFY * HTTP/1.1\r\n
HOST:239.255.255.250:1900\r\n
CACHE-CONTROL:max-age={lifetime}\r\n
LOCATION:http://{iface-ip}:{port}/rootDesc.xml\r\n
SERVER: {RUSTY_DLNA_SERVER_STRING}\r\n
NT:{st}\r\n
USN:{usn}\r\n
NTS:ssdp:alive\r\n
\r\n
```

Note the **missing spaces** after `HOST:`, `CACHE-CONTROL:`, `LOCATION:`,
`NT:`, `USN:`, `NTS:` on NOTIFY (M-SEARCH *responses* do use spaces after
`ST:`, `USN:`, `LOCATION:`). Replicas that “fix” the spacing are still
usually accepted; matching this daemon means copying the no-space form.

`LOCATION` is always `http://{iface}:{port}/rootDesc.xml` (`ROOTDESC_PATH`).

### NOTIFY byebye (`SendSSDPGoodbyes`)

Also doubled. No `CACHE-CONTROL`, `LOCATION`, or `SERVER`:

```
NOTIFY * HTTP/1.1\r\n
HOST:239.255.255.250:1900\r\n
NT:{st}\r\n
USN:{usn}\r\n
NTS:ssdp:byebye\r\n
\r\n
```

### M-SEARCH receive

Requires `HTTP/1.1` on the request line after `*`. Headers parsed
case-insensitively:

- `ST:` required, non-empty
- `MAN:` must be exactly `"ssdp:discover"` (15 chars including quotes)
- `MX:` must parse as an integer `>= 0`

Rejected (logged, no reply):

- `DLNA_STRICT_MASK` set **and** source port `<= 1024` or `== 1900`
- Packet arrived on an interface that is not in `lan_addr[]`
- `YOUKU-NOTIFY` — silently ignored
- Anything else that is not `NOTIFY`, `M-SEARCH`, or `YOUKU-NOTIFY`

ST matching: prefix of `known_service_types[i]`. If the client sends a longer
ST, the extra must be version `1` (when the stem ends in `:`) plus optional
spaces. Other extra characters drop the match.

`ST: ssdp:all` (exactly 8 chars) replies **once per** `known_service_types`
entry (six packets), after a 13–30 ms jitter. A unicast device-ST match
replies with a single `SendSSDPResponse` after 13–20 ms jitter, then
**returns** (does not also walk the rest of the table).

### M-SEARCH unicast response

```
HTTP/1.1 200 OK\r\n
CACHE-CONTROL: max-age={lifetime}\r\n
DATE: {IMF-fixdate GMT}\r\n
ST: {st}\r\n
USN: {usn}\r\n
EXT:\r\n
SERVER: {RUSTY_DLNA_SERVER_STRING}\r\n
LOCATION: http://{host}:{port}/rootDesc.xml\r\n
Content-Length: 0\r\n
\r\n
```

`{host}` is the IPv4 of the iface that received the search (`IP_PKTINFO` on
Linux; subnet match otherwise).

### Inbound NOTIFY (client sniffing only)

The daemon does **not** join other servers. It peeks at `NOTIFY` from
renderers to pre-fill the client cache:

Required: `NTS: ssdp:alive`, `NT:` starts with
`urn:schemas-upnp-org:device:MediaRenderer`, plus `LOCATION` and `SERVER`.

Then only if `SERVER` is `Allegro-Software-RomPlug` (Roku), `LOCATION`
contains `SamsungMRDesc.xml`, or `SERVER` contains `DigiOn DiXiM`
(Marantz), it fetches that LOCATION (`ParseUPnPClient`) unless a more
specific cache entry already exists.

Friendly-name SSDP match (`EFriendlyNameSSDP`, e.g. marantz DMP) is applied
during that fetch, not from M-SEARCH.

---

## 2. HTTP surface

Source: `src/upnphttp.c`, `src/path table`, `src/httpersist.h`.

TCP port = `runtime_vars.port` (default 8200). Methods: `GET`, `HEAD`,
`POST`, `SUBSCRIBE`, `UNSUBSCRIBE`. Anything else is **501**.

### URL map

| Path | Handler | Body |
|---|---|---|
| `/rootDesc.xml` | `genRootDesc` or Xbox/Samsung variants | device description |
| `/ContentDir.xml` | `genContentDirectory` | ContentDirectory SCPD |
| `/ConnectionMgr.xml` | `genConnectionManager` | ConnectionManager SCPD |
| `/X_MS_MediaReceiverRegistrar.xml` | `genX_MS_MediaReceiverRegistrar` | MS registrar SCPD |
| `/ctl/ContentDir` | SOAP `POST` (URL is **not** checked) | control |
| `/ctl/ConnectionMgr` | SOAP `POST` | control |
| `/ctl/X_MS_MediaReceiverRegistrar` | SOAP `POST` | control |
| `/evt/ContentDir` | `SUBSCRIBE` / `UNSUBSCRIBE` | GENA |
| `/evt/ConnectionMgr` | GENA | |
| `/evt/X_MS_MediaReceiverRegistrar` | GENA | |
| `/MediaItems/{id}.{ext}` | `SendResp_dlnafile` | original media |
| `/Thumbnails/{id}.jpg` | EXIF thumbnail | JPEG |
| `/AlbumArt/{artId}-{detailId}.jpg` | album art JPEG | JPEG |
| `/Resized/{id}.jpg?width=W,height=H` | on-the-fly JPEG scale | JPEG |
| `/icons/sm.png` `lrg.png` `sm.jpg` `lrg.jpg` | embedded icons | |
| `/Captions/{id}/{index}.{ext}` | caption file | see MIME table |
| `/Captions/{id}.srt` | same handler (`strtoll` then optional `/index`) | |
| `/status` and `/` | HTML presentation (not DLNA) | HTML |
| `/TiVoConnect?…` | only if compiled with `TIVO_SUPPORT` and `TIVO_MASK` | else 404 |

`POST` is dispatched solely by the `SOAPAction` header. The request-target
is ignored. A replica that 404s `POST /` or `POST /unused` is stricter than
this daemon.

`{ext}` on `/MediaItems/` is **decorative**. `SendResp_dlnafile` does
`strtoll(object, NULL, 10)` and looks up `DETAILS.ID`. Same for album art:
only the leading integer (`ALBUM_ART.ID`) is used.

### HTTP request headers this daemon understands

Parsed in `ParseHttpHeaders` (`src/upnphttp.c`):

| Header | Effect |
|---|---|
| `Content-Length` | POST body length; `FLAG_CONTENT_LENGTH` |
| `Connection` | `close` → `FLAG_CONN_CLOSE`; `keep-alive` → `FLAG_CONN_KEEP` (both can be set; close wins) |
| `SOAPAction` | SOAP method; optional surrounding `"` or `'` stripped |
| `Callback` | GENA; value between `<` `>` |
| `SID` | GENA (exact header name `SID`, not `SIDHEADER`) |
| `NT` | GENA; must be `upnp:event` on subscribe |
| `Timeout` | `Second-{n}` (no space after the hyphen) |
| `Range` | `bytes={start}-{end}`; `FLAG_RANGE` |
| `Host` | `FLAG_HOST`; selects `h->iface` by prefix match on `lan_addr[].str` |
| `User-Agent` | client table, `EUserAgent` |
| `X-AV-Client-Info` | client table, `EXAVClientInfo` |
| `FriendlyName` | client table, `EFriendlyName` |
| `Transfer-Encoding: chunked` | `FLAG_CHUNKED` (disables persist) |
| `Accept-Language` | response gets `Content-Language: en` |
| `getcontentFeatures.dlna.org` | value must be `1`; else `FLAG_INVALID_REQ` → 400 |
| `getAvailableSeekRange.dlna.org` | same `1` check |
| `TimeSeekRange.dlna.org` | `FLAG_TIMESEEK` |
| `PlaySpeed.dlna.org` | `FLAG_PLAYSPEED` |
| `realTimeInfo.dlna.org` | `FLAG_REALTIMEINFO` |
| `transferMode.dlna.org` | `Streaming` / `Interactive` / `Background` |
| `getCaptionInfo.sec` | `FLAG_CAPTION` (adds `CaptionInfo.sec` on media GET) |
| `uctt.upnp.org` | sets `DLNA_STRICT_MASK` (conformance) |

### Host / DNS-rebinding (400)

If a `Host` header is present (`src/upnphttp.c` ~1007–1031):

1. Optional `:port` must be all digits and `<= 65535`.
2. The host part must be a **dotted IPv4** that `inet_pton` accepts and is
   not `0.0.0.0`.

A hostname (`my-nas.local`), IPv6, or empty host is **400 Bad Request**
(“DNS rebinding attack suspected”). HTTP/1.1 GET/HEAD **without** `Host`
is also 400 (`FLAG_HOST` missing).

### TimeSeek / PlaySpeed (406)

DLNA 7.3.33.4 as implemented: if `TimeSeekRange.dlna.org` **or**
`PlaySpeed.dlna.org` is present **and** there is no `Range:` → **406**.
This daemon does not implement time-seek. A replica that answers TimeSeek
is already a different product.

`transferMode.dlna.org: Streaming` on an image (album art, thumbnail,
resized, or a `image/*` `/MediaItems/` object) → **406**.
`Interactive` on a non-image `/MediaItems/` object → **406**, except
Samsung clients unless `DLNA_STRICT_MASK` is on.

`realTimeInfo.dlna.org` combined with `Interactive` → **400**.

### Keep-Alive vs close (`src/httpersist.h`)

```
HTTP/1.1 stays open unless Connection: close.
HTTP/1.0 stays open only with Connection: keep-alive.
close always wins if both tokens are present.
```

Cap: `persist_left` starts at **100** requests per TCP connection.
Chunked requests force `persist = 0`.

**Media `/MediaItems/` always sets `h->persist = 0`**
(`SendResp_dlnafile`). The child `fork`s (`USE_FORK`), `sendfile`s, and
exits. SOAP, SCPD, icons, album art, captions, thumbnails *may* persist.

SOAP/desc response header (`BuildHeader_upnphttp`):

```
HTTP/1.1 {code} {msg}\r\n
Content-Type: text/xml; charset="utf-8"\r\n
Connection: {keep-alive|close}\r\n
Content-Length: {n}\r\n
Server: {RUSTY_DLNA_SERVER_STRING}\r\n
Date: {IMF-fixdate GMT}\r\n
EXT:\r\n
\r\n
```

HTML errors set `Content-Type: text/html`. Subscribe adds
`Timeout: Second-{n}` (default 300 if the client omitted it) and `SID:`.

---

## 3. Device and service description

Source: `src/upnpdescgen.c`, `src/path table`.

Root XML starts with `<?xml version="1.0"?>\r\n` then
`<root xmlns="urn:schemas-upnp-org:device-1-0">`.

Device fields:

| Element | Source |
|---|---|
| `deviceType` | `urn:schemas-upnp-org:device:MediaServer:1` |
| `friendlyName` | `friendly_name` (config / `getfriendlyname`) |
| `manufacturer` | `ROOTDEV_MANUFACTURER` (`Justin Maggard` unless NETGEAR) |
| `manufacturerURL` | `http://www.netgear.com/` |
| `modelDescription` | `rustyDLNA on ` + `OS_NAME` |
| `modelName` | `modelname` (default `Windows Media Connect compatible (rustyDLNA)`) |
| `modelNumber` | `modelnumber` |
| `modelURL` | `OS_URL` |
| `serialNumber` | `serialnumber` |
| `UDN` | `uuidvalue` |
| `dlna:X_DLNADOC` | `DMS-1.50` (`xmlns:dlna="urn:schemas-dlna-org:device-1-0"`) |
| `presentationURL` | `presentationurl` |

Icons (all under `/icons/`): 48×48 and 120×120, PNG and JPEG, depth 24.

Services (exact URLs):

| serviceType | serviceId | controlURL | eventSubURL | SCPDURL |
|---|---|---|---|---|
| `urn:schemas-upnp-org:service:ContentDirectory:1` | `urn:upnp-org:serviceId:ContentDirectory` | `/ctl/ContentDir` | `/evt/ContentDir` | `/ContentDir.xml` |
| `urn:schemas-upnp-org:service:ConnectionManager:1` | `urn:upnp-org:serviceId:ConnectionManager` | `/ctl/ConnectionMgr` | `/evt/ConnectionMgr` | `/ConnectionMgr.xml` |
| `urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1` | `urn:microsoft.com:serviceId:X_MS_MediaReceiverRegistrar` | `/ctl/X_MS_MediaReceiverRegistrar` | `/evt/X_MS_MediaReceiverRegistrar` | `/X_MS_MediaReceiverRegistrar.xml` |

### Xbox 360 `rootDesc` lie

If the client type is `EXbox`, `genRootDesc` is called after:

- `modelnumber` is overwritten with `"1"`
- if `friendly_name` contains no `:`, append `": 1"` (colon, space, one)

Then both are restored. Xbox 360 will not list the server otherwise.

### Samsung DCM10 `rootDesc` (`genRootDescSamsung`)

Used when the client has `FLAG_SAMSUNG_DCM10`. The optional
`manufacturerURL` and `modelURL` slots are replaced with:

```
<sec:ProductCap>smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec</sec:ProductCap>
<sec:X_ProductCap>smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec</sec:X_ProductCap>
```

(`rootDesc` indices 8 and 12.)

---

## 4. SOAP control

Source: `src/upnpsoap.c` `soapMethods[]`, `ExecuteSoapAction`.

`SOAPAction` value looks like
`"urn:schemas-upnp-org:service:ContentDirectory:1#Browse"`.
The method is the substring after `#` up to the closing quote.
Match is **`strncmp` prefix** against `soapMethods[]` in order. First hit
wins. A replica should use exact names; this daemon would also treat
`BrowseDirectChildren` as `Browse`.

| methodName | Service (by convention) | Notes |
|---|---|---|
| `QueryStateVariable` | any | legacy |
| `Browse` | ContentDirectory | |
| `Search` | ContentDirectory | |
| `GetSearchCapabilities` | ContentDirectory | |
| `GetSortCapabilities` | ContentDirectory | |
| `GetSystemUpdateID` | ContentDirectory | `<Id>{updateID}</Id>` |
| `GetProtocolInfo` | ConnectionManager | |
| `GetCurrentConnectionIDs` | ConnectionManager | always `0` |
| `GetCurrentConnectionInfo` | ConnectionManager | only `ConnectionID=0` |
| `IsAuthorized` | X_MS_MediaReceiverRegistrar | always `<Result>1</Result>` |
| `IsValidated` | same handler as `IsAuthorized` | |
| `RegisterDevice` | X_MS_MediaReceiverRegistrar | echoes `uuidvalue` |
| `UpdateObject` | ContentDirectory | bookmark / watch count writes |
| `X_GetFeatureList` | Samsung / ContentDirectory | |
| `X_SetBookmark` | Samsung / ContentDirectory | |

Unknown method → SOAP fault **401 Invalid Action**.
Missing args → **402 Invalid Args**.
Missing object → **701 No such object error**.
Bad sort (DLNA client or `DLNA_STRICT_MASK`) → **709 Unsupported or invalid sort criteria**.
`GetCurrentConnectionInfo` for id ≠ 0 → **701**.

### SOAP envelope (success)

```
<?xml version="1.0" encoding="utf-8"?>\r\n
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>
{body}
</s:Body></s:Envelope>\r\n
```

HTTP status **200**.

### SOAP fault (HTTP **500** Internal Server Error)

```
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><s:Fault>
<faultcode>s:Client</faultcode>
<faultstring>UPnPError</faultstring>
<detail>
<UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
<errorCode>{n}</errorCode>
<errorDescription>{text}</errorDescription>
</UPnPError>
</detail>
</s:Fault></s:Body></s:Envelope>
```

### `GetProtocolInfo`

`<Source>` is the compile-time macro `RESOURCE_PROTOCOL_INFO_VALUES`
(`src/upnpglobalvars.h`): a long comma-separated list of
`http-get:*:{mime}:DLNA.ORG_PN=…` plus wildcard `http-get:*:video/x-matroska:*`
and friends. `<Sink>` is empty.

This list is **advertising**, not a filter. HEVC MKV remuxes are served
even though no `HEVC_*` PN appears here.

### `GetSortCapabilities`

```
dc:title,dc:date,upnp:class,upnp:album,upnp:episodeNumber,upnp:originalTrackNumber
```

### `GetSearchCapabilities`

```
dc:creator,dc:date,dc:title,upnp:album,upnp:actor,upnp:artist,upnp:class,upnp:genre,@id,@parentID,@refID
```

### `Browse`

Args: `ObjectID` (or `ContainerID`), `BrowseFlag`
(`BrowseDirectChildren` | `BrowseMetadata`), `Filter`, `StartingIndex`,
`RequestedCount`, `SortCriteria`.

- `RequestedCount == 0` means “no limit” (internally `-1`).
- Negative count or index → 402.
- `BrowseMetadata` always returns at most one element; magic containers
  may rewrite the SQL object/parent/ref columns.
- Response:

```
<u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<Result>&lt;DIDL-Lite …&gt;…&lt;/DIDL-Lite&gt;</Result>
<NumberReturned>{n}</NumberReturned>
<TotalMatches>{n}</TotalMatches>
<UpdateID>{updateID}</UpdateID>
</u:BrowseResponse>
```

`Result` is **XML-escaped DIDL** (so clients see `&lt;DIDL-Lite` in the SOAP
body). Default buffer 131072, grows to at most `MAX_RESPONSE_SIZE` 2097152;
overflow sets `RESPONSE_TRUNCATED` and still returns 200 with a partial
DIDL.

`Filter` `*` or empty → `STANDARD_FILTER_MASK` (all standard fields).
Samsung also gets `sec:CaptionInfoEx` and `sec:dcmInfo` by default.

### `Search`

Same DIDL machinery. `SearchCriteria` is translated by
`parse_search_criteria` into a SQL `WHERE`. `NULL` criteria becomes
`1 = 1`.

### `X_GetFeatureList`

Returns an XML-escaped `<Features>` blob with
`name="samsung.com_BASICVIEW"` and three containers:

| type | default id | `root_container=64` | `FLAG_SAMSUNG_DCM10` and no root override |
|---|---|---|---|
| `object.item.audioItem` | `1` | `1$14` | `A` |
| `object.item.videoItem` | `2` | `2$15` | `V` |
| `object.item.imageItem` | `3` | `3$16` | `I` |

If `root_container` is set to something other than `64`, all three ids
become that single container.

### `X_SetBookmark`

Args `ObjectID`, `PosSecond`. Resolves magic containers, looks up
`OBJECTS.DETAIL_ID`, writes `BOOKMARKS.SEC`.

- `FLAG_CONVERT_MS` (Samsung Q/QN): `sec /= 1000` (client sent
  milliseconds).
- `sec < 30` is stored as `0`.

---

## 5. Object IDs (ContentDirectory tree)

Source: `src/scanner.h`, `src/containers.c`.

PlaysForSure-style roots. **These literals must appear in a replica.**

| ID | Meaning |
|---|---|
| `0` | True root (unless `root_container` remaps it) |
| `64` | Browse Folders (`BROWSEDIR_ID`) — filesystem tree |
| `1` | Music (`MUSIC_ID`) |
| `1$4` | Music / All |
| `1$5` | Music / Genre |
| `1$6` | Music / Artist |
| `1$7` | Music / Album |
| `1$F` | Music / Playlists |
| `1$14` | Music / Folders |
| `1$100` | Contributing artists |
| `1$107` | Album artists |
| `1$108` | Composers |
| `1$101` | Music rating |
| `1$FF0` | Music “Recently Added” (magic, last 90 days, max 50) |
| `2` | Video (`VIDEO_ID`) |
| `2$8` | Video / All |
| `2$9` | Video / Genre |
| `2$A` | Video / Actor |
| `2$E` | Video / Series |
| `2$10` | Video / Playlists |
| `2$15` | Video / Folders |
| `2$200` | Video rating |
| `2$FF0` | Video “Recently Added” |
| `3` | Images |
| `3$B` | Images / All |
| `3$C` | Images / Date |
| `3$D` | Images / Album |
| `3$D2` | Images / Camera (PFS Keyword) |
| `3$11` | Images / Playlists |
| `3$16` | Images / Folders |
| `3$300` | Image rating |
| `3$FF0` | Images “Recently Added” |

Child object IDs are `{parent}${hex}` (uppercase hex from an integer
sequence). File items live under their folder container; virtual views
(`2$8`, etc.) are `REF_ID` aliases of the same `DETAIL_ID`.

`root_container` config (often `V` / `2` / `64`) makes magic container
`"0"` rewrite to that id, so `Browse` of `0` is already Video or Folders.

### Magic / alias IDs (`src/containers.c`)

Only applied when the client has the required flag:

| Client id | Real id | Flag |
|---|---|---|
| `4` `5` `6` `7` `8` `B` `C` `F` `14` `15` `16` `D2` | Music-all / genre / … | `FLAG_MS_PFS` (Xbox, Cling, EVA2000, MediaRoom, LIFETAB, Roku SoundBridge) |
| `A` `V` `I` | `1` `2` `3` | `FLAG_SAMSUNG_DCM10` |
| `0` | `1` (Music) | `FLAG_AUDIO_ONLY` (Roku SoundBridge) |

---

## 6. DIDL-Lite emit

Source: `callback()` in `src/upnpsoap.c`.

Opening tag (escaped in SOAP `Result`):

```
<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/"
           xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"
           xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
           [xmlns:dlna="urn:schemas-dlna-org:metadata-1-0/"]
           [xmlns:pv="http://www.pv.com/pvns/"]
           [xmlns:sec="http://www.sec.co.kr/dlna"]>
```

`dlna` ns is added if the filter asked for `dlna` or the client is Samsung.
`pv` if `pv:subtitleFileType` / `pv:subtitleFileUri` requested.
`sec` if any `FILTER_SEC_*` bit is set.

### Item

```
<item id="{OBJECT_ID}" parentID="{PARENT_ID}" restricted="1" [refID="…"]>
  <dc:title>…</dc:title>
  <upnp:class>object.{CLASS}</upnp:class>
  [<dc:date>{normalized}</dc:date>]
  [<dc:creator>…] [<dc:description> first 384 chars]
  [<upnp:artist> / <upnp:actor> / <upnp:album> / <upnp:genre>]
  [<upnp:originalTrackNumber> audio]
  [<upnp:episodeSeason> / <upnp:episodeNumber> video]
  [<upnp:lastPlaybackPosition>{seconds or ms}</upnp:lastPlaybackPosition>]
  [<sec:dcmInfo>CREATIONDATE=0,FOLDER={title},BM={sec}</sec:dcmInfo>]
  [<upnp:playbackCount>]
  <res …>http://{lan}:{port}/MediaItems/{detailID}.{ext}</res>
  … extra res / captions / album art …
</item>
```

`CLASS` in the DB is already `item.videoItem` etc.; the emit prefixes
`object.`.

### Primary `<res>` (`add_res`)

Attributes included only if the corresponding filter bit is set:
`size`, `duration`, `bitrate`, `sampleFrequency`, `nrAudioChannels`,
`resolution`.

`duration` stored as `H:MM:SS.mmm` (`duration_str` in `src/utils.c`).

`bitrate`: raw `DETAILS.BITRATE` except `FLAG_MS_PFS` divides by 8.

`protocolInfo`:

```
http-get:*:{mime}:{dlna_buf}
```

where `{dlna_buf}` is one of:

1. If `DETAILS.DLNA_PN` is set **and** not `FLAG_SKIP_DLNA_PN`:
   `DLNA.ORG_PN={pn};DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS={flags}{24 zero hex}`
2. Else if client has `FLAG_DLNA`:
   `DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS={flags}{24 zero hex}`
   (**no PN** — this is the HEVC MKV remux case)
3. Else: `*`

`{flags}` is 8 hex digits. Base
`DLNA_FLAG_DLNA_V1_5|DLNA_FLAG_HTTP_STALLING|DLNA_FLAG_TM_B`
(`0x00100000|0x00200000|0x00400000` = `0x00700000`) plus:

| MIME class | extra |
|---|---|
| video / audio | `DLNA_FLAG_TM_S` (`0x01000000`) → `0x01700000` |
| image | `DLNA_FLAG_TM_I` (`0x00800000`) → `0x00F00000` |

URL: `http://{lan_addr[iface]}:{port}/MediaItems/{detailID}.{ext}`
with `{ext}` from `mime_to_ext` (`src/utils.c`). After Samsung remaps
`video/x-matroska` → `video/x-mkv`, ext is still `mkv`.

### `dc:date` (Kodi 1905)

Emit path always runs `w3c_normalize_date` (`src/utils.c`) before writing
`<dc:date>`. See [§11](#11-gotchas-client-visible).

New scans store `YYYY-MM-DDTHH:MM:SSZ` via `w3c_date_from_time(st_mtime)`
(`src/metadata.c`). NFO `<year>1999</year>` becomes `1999-01-01`
(`src/nfo.c`). Audio tags were already often `YYYY-01-01` (10 chars).

### Extra `<res>` that still serve the **same** file (`CI=1` lies)

These call `add_res` again, so the URL is still `/MediaItems/{id}.{ext}`.

**Toshiba TV** (`EToshibaTV`): if PN is `MPEG_TS_HD_NA*`, `MPEG_TS_SD_NA*`,
`AVC_TS_MP_HD_AC3*`, or `AVC_TS_HP_HD_AC3*`, add a second res with
`DLNA.ORG_PN=MPEG_PS_NTSC;DLNA.ORG_OP=01;DLNA.ORG_CI=1`.

**Sony BDP** (`ESonyBDP`):

- TS (`AVC_TS*` / `MPEG_TS*`): extra `MPEG_TS_SD_NA` and `MPEG_TS_SD_EU`
  with `CI=1` (skip the one that already matches).
- MP4 AVC / MPEG4_P2, **or** mime `x-matroska` / `x-msvideo` / `mpeg`:
  rewrite mime to `video/avi` and add `MPEG_PS_NTSC` and `MPEG_PS_PAL`
  with `CI=1`.

**Sony Bravia**: if PN is `AVC_TS_MP_SD_AC3*`, `AVC_TS_MP_HD_AC3*`, or
`AVC_TS_HP_HD_AC3*`, add another res with PN rewritten to
`AVC_TS_HD_50_AC3` + the original suffix (keeps `_T` / `_ISO`).

None of these transcode.

### Images

Additional `/Resized/` URLs for `JPEG_LRG` (4096), `JPEG_MED` (1024×768),
`JPEG_SM` (640×480), `JPEG_TN` (160) — skipped when `FLAG_NO_RESIZE` and
the target is larger than 160×160. Native EXIF thumb is
`/Thumbnails/{detailID}.jpg` with
`DLNA.ORG_PN=JPEG_TN;DLNA.ORG_CI=1` unless `FLAG_RESIZE_THUMBS`.

### Captions in DIDL

If `FLAG_HAS_CAPTIONS` (a `CAPTIONS` row exists for this `DETAIL_ID`):

- `FLAG_CAPTION_RES` (Kodi, Samsung CDE/Q, LG, BubbleUPnP, Movian, Asus,
  NetFront, or any “generic DLNA 1.5+” when `SUBTITLES_MASK` is on):

  ```
  <res protocolInfo="http-get:*:{cmime}:*">
    http://{lan}:{port}/Captions/{detailID}/{index}.{cext}
  </res>
  ```

  `{index}` is 0-based in `PATH` sort order (schema v13, multi-row PK).

- Filter `sec:CaptionInfoEx`:

  ```
  <sec:CaptionInfoEx sec:type="{cext}">
    http://{lan}:{port}/Captions/{detailID}/{index}.{cext}
  </sec:CaptionInfoEx>
  ```

- Filter `pv:subtitleFileType` / `pv:subtitleFileUri` adds those
  attributes on the **primary** `<res>` (type always `SRT`, URI
  `/Captions/{detailID}.srt` — first caption only).

`caption_ext` / `caption_http_mime` (`src/utils.c`):

| file suffix | DIDL type | HTTP Content-Type |
|---|---|---|
| `.srt` | `srt` | `text/srt` |
| `.ass` / `.ssa` | `ass` / `ssa` | `text/x-ssa` |
| `.vtt` | `vtt` | `text/vtt` |
| `.smi` | `smi` | `smi/caption` |
| `.sub` | `sub` | `text/plain` |

### Album art

Video items (and not `FLAG_MS_PFS`): extra `<res>`  
`http://{lan}:{port}/AlbumArt/{albumArtId}-{detailID}.jpg`  
with `DLNA.ORG_PN=JPEG_TN`. Samsung Series CDE adds a second JPEG_SM res
claiming `320x320` and `CI=1` (still the same JPEG file).

Audio / containers: `<upnp:albumArtURI>` with the same URL; Samsung also
wants `dlna:profileID="JPEG_TN"`.

Xbox `FLAG_MS_PFS` does **not** get the video `<res>` art; it requests
`/MediaItems/{id}?albumArt=true` instead, which is rewritten to
`SendResp_albumArt`.

### Container DIDL

```
<container id="…" parentID="…" restricted="1" [searchable="0|1"] [childCount="n"]>
  <dc:title>…</dc:title>
  <upnp:class>object.{CLASS}</upnp:class>
  …
</container>
```

The true root (`id="0"`) has `parentID="-1"`. `BrowseMetadata` of `0`
includes three `<upnp:searchClass includeDerived="1">` for
`object.item.audioItem`, `imageItem`, `videoItem` if not filtered out.
`searchable` is `0` when `0` is a magic container (default
`root_container` rewrite, or `FLAG_AUDIO_ONLY`).

Sony `av:mediaClass`: `M` / `V` / `P` for ids starting with `1` / `2` / `3`.

`upnp:storageUsed` is emitted for `storageFolder` (or if requested); the
value is often `-1`.

### Title hacks (still DIDL)

| Client | Hack |
|---|---|
| `ELGDevice` + captions | append `.` to `dc:title` (else LG ignores subs) |
| `EAsusOPlay` + captions | truncate title to 23 chars (else reboot) |
| `EHyundaiTV` | `dc:title` becomes `{title}.{ext}` |
| `FORCE_ALPHASORT` | prefix a zero-padded index so client-side sorts keep server order |

---

## 7. HTTP media GET (`/MediaItems/`)

Source: `SendResp_dlnafile` in `src/upnphttp.c`.

After looking up `PATH, MIME, DLNA_PN`:

1. Samsung: `video/x-matroska` → `video/x-mkv`; Series A `video/x-msvideo`
   → `video/mpeg`.
2. Sony BDP: `x-matroska` or `mpeg` → `video/divx`.
3. `streamcache_touch(path, size, RangeStart)` if MIME starts with `video`.
4. `process_fork` — parent closes the HTTP socket; child serves.
5. `h->persist = 0`.
6. Header via `start_dlna_header`:

```
HTTP/1.1 {200|206} OK\r\n
Connection: close\r\n
Date: …\r\n
Server: {RUSTY_DLNA_SERVER_STRING}\r\n
EXT:\r\n
realTimeInfo.dlna.org: DLNA.ORG_TLAG=*\r\n
transferMode.dlna.org: {Streaming|Interactive|Background}\r\n
Content-Type: {mime}\r\n
Content-Length: {n}\r\n
[Content-Range: bytes {a}-{b}/{size}\r\n]
[CaptionInfo.sec: http://{lan}:{port}/Captions/{id}.srt\r\n]
Accept-Ranges: bytes\r\n
contentFeatures.dlna.org: {pn?}DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS={flags}{24 zeros}\r\n
\r\n
```

`transferMode`: `Interactive` for `image/*`; `Background` if the client
asked for it **and** the child could `setpriority(…, 19)`; else
`Streaming`.

`contentFeatures` PN prefix is `DLNA.ORG_PN={pn};` if the DB has one,
else empty (HEVC MKV). `DLNA.ORG_OP` is **always `01`** (byte seek) and
`CI` is **always `0`** on the HTTP response, even when DIDL advertised a
fake `CI=1` extra res.

If `getCaptionInfo.sec` was sent and a caption row exists, add
`CaptionInfo.sec: http://{lan}:{port}/Captions/{id}.srt` (first/default
caption; not the indexed multi-caption URL).

Body: `sendfile` of `[offset, RangeEnd]`, or `streamcache_copy` when the
byte is inside the live RAM window.

Invalid range → 400; range past EOF → 416; missing file → 404; path
escapes the media dirs (and `WIDE_LINKS` is off) → 403.

### Stream-cache vs Range (client-visible)

`src/streamcache.c`. Optional (`stream_buffer_mb`). Not a second URL.

- Window is the first **non-cue** `Range` on a file. Cap is configurable.
- A request whose start is in the last **32 MiB** of a file larger than
  32 MiB (`SC_TAIL`) is treated as a container cue/index probe **if a
  window already exists** and does **not** dump that window. A first play
  that starts in the last 32 MiB still opens a window.
- While the window is filling, an outside seek does not abort the fill.
- HTTP still returns the requested `Range` either from the window or via
  `sendfile`. The cache only changes latency, not headers.

Kodi MKV often issues a Range near EOF (~last 32 MiB) to read cues.
Without the tail-probe rule, a multi-GB window would be thrown away and
the HTTP child would look stuck.

### Album art / thumbs / captions / resized

`transferMode.dlna.org: Interactive`. Album art
`contentFeatures.dlna.org: DLNA.ORG_PN=JPEG_TN`. Captions use
`caption_http_mime` and **no** `contentFeatures`. Streaming/Range on
images → 406. These paths **can** Keep-Alive.

Resized URL query: `width`, `height`, optional `rotation`, `pixelshape`.
Output is always JPEG.

---

## 8. Client identification

Source: `src/clients.c`, `src/clients.h`. First-match in table order
(more specific Samsung entries sit **above** the generic `SEC_HHP_`).

Cache: 25 slots, keyed by IPv4. TTL 1 hour; same MAC extends another
hour. Later requests with a generic UA (`DLNADOC/1.50`, `UPnP/1.0`) do
**not** overwrite a more specific cached type (`type < EStandardDLNA150`).
Samsung Series B is not overwritten by Series A.

Match sources:

| `match_type` | Header / SSDP |
|---|---|
| `EUserAgent` | `User-Agent` substring |
| `EXAVClientInfo` | `X-AV-Client-Info` substring |
| `EFriendlyName` | HTTP `FriendlyName` |
| `EFriendlyNameSSDP` | SSDP / device-desc friendly name |
| `EModelName` | device-desc `modelName` (SSDP fetch) |

### Table (order matters)

| type | match | flags (wire-relevant) |
|---|---|---|
| Xbox 360 | `Xbox/` | `FLAG_MIME_AVI_AVI` `FLAG_MS_PFS` |
| PS3 | `PLAYSTATION` / `PLAYSTATION 3` | `FLAG_DLNA` `FLAG_MIME_AVI_DIVX` |
| Cling | `Cling/` | `FLAG_MS_PFS` |
| AllShare PC | `SEC_HHP_[PC]` | `FLAG_DLNA` only (must **not** get Samsung extras) |
| Samsung BD J5500 | `[BD]J5500` | Samsung + `FLAG_SKIP_DLNA_PN` + `FLAG_CAPTION_RES` + `FLAG_NO_RESIZE` |
| Samsung BDP CDEF | `SEC_HHP_BD` | Samsung + `FLAG_NO_RESIZE` (no caption res — BDP browse bug) |
| Samsung Q | `SEC_HHP_[TV] Samsung Q` | Samsung + DCM10 + captions + `FLAG_CONVERT_MS` |
| Samsung QN | `SEC_HHP_Samsung QN` | same |
| Samsung CDEFJ | `SEC_HHP_` | Samsung + DCM10 + captions + `FLAG_NO_RESIZE` |
| Samsung A | `SamsungWiselinkPro` | Samsung + `FLAG_NO_RESIZE` |
| Samsung B | model `Samsung DTV DMR` | Samsung + `FLAG_NO_RESIZE` |
| Panasonic | `Panasonic` | `FLAG_DLNA` `FLAG_FORCE_SORT` |
| NetFront | `IPI/1` | `FLAG_DLNA` `FLAG_FORCE_SORT` `FLAG_CAPTION_RES` |
| Denon | `bridgeCo-DMP/3` | `FLAG_DLNA` |
| FreeBox | `fbxupnpav/` | `FLAG_RESIZE_THUMBS` |
| Popcorn Hour | `SMP8634` | `FLAG_MIME_FLAC_FLAC` |
| Sony BDP | `mv="2.0"` in X-AV | `FLAG_DLNA` (SMP-100 included) |
| LG NetCast | `LGE_DLNA_SDK/1.6.0` | `FLAG_DLNA` captions `FLAG_MIME_FLAC_FLAC` |
| LG | `LGE_DLNA_SDK` | same |
| Sony Bravia | `BRAVIA` | `FLAG_DLNA` |
| Sony Internet TV | `INTERNET TV` | `FLAG_DLNA` |
| EVA2000 | `Verismo,` | `FLAG_MS_PFS` `FLAG_RESIZE_THUMBS` |
| DirecTV | `DIRECTV ` | `FLAG_RESIZE_THUMBS` |
| Toshiba TV | `UPnP/1.0 DLNADOC/1.50 Intel_SDK_for_UPnP_devices/1.2` | `FLAG_DLNA` |
| Hyundai | friendly `HYUNDAITV` | `FLAG_DLNA` |
| Roku SoundBridge | model `Roku SoundBridge` | PFS + `FLAG_AUDIO_ONLY` + wav + force sort |
| marantz DMP | SSDP friendly `marantz DMP` | `FLAG_DLNA` `FLAG_MIME_WAV_WAV` |
| MS MediaRoom | `Microsoft-IPTV-Client` | `FLAG_MS_PFS` |
| LIFETAB | friendly `LIFETAB` | `FLAG_MS_PFS` |
| Asus OPlay | `O!Play` | `FLAG_DLNA` avi + captions |
| BubbleUPnP | `BubbleUPnP` | `FLAG_CAPTION_RES` |
| Movian | `Movian` | `FLAG_CAPTION_RES` |
| **Kodi** | `Kodi` | `FLAG_DLNA` `FLAG_MIME_AVI_AVI` `FLAG_CAPTION_RES` |
| Windows | `FDSSDP` | `FLAG_DLNA` `FLAG_MIME_AVI_AVI` |
| TiVo | `TvHttpClient` | none |
| Generic DLNA 1.5 | `DLNADOC/1.50` | `FLAG_DLNA` `FLAG_MIME_AVI_AVI` |
| Generic UPnP 1.0 | `UPnP/1.0` | none |

There is **no** Google Streamer / Chromecast / Cast entry. Those clients
fall through to Unknown or Generic DLNA 1.5 depending on UA. They receive
the original MKV bytes and the stock DIDL.

### Flags that change bytes

| Flag | Wire effect |
|---|---|
| `FLAG_DLNA` | `protocolInfo` uses `DLNA.ORG_OP/CI/FLAGS` instead of `*`; bad sort → 709 |
| `FLAG_SKIP_DLNA_PN` | drop `DLNA.ORG_PN=…` even if the DB has one (Samsung BD J5500) |
| `FLAG_SAMSUNG` | `video/x-matroska` → `video/x-mkv` in DIDL **and** HTTP |
| `FLAG_SAMSUNG_DCM10` | Samsung `rootDesc`; `X_GetFeatureList` ids `A`/`V`/`I`; magic containers |
| `FLAG_CAPTION_RES` | extra caption `<res>` |
| `FLAG_CONVERT_MS` | bookmarks: SOAP in ms→s, DIDL out s→ms |
| `FLAG_NO_RESIZE` | no JPEG_LRG/MED/SM extra res |
| `FLAG_RESIZE_THUMBS` | always `/Resized/` for TN, never `/Thumbnails/` |
| `FLAG_MIME_AVI_AVI` | `video/x-msvideo` → `video/avi` |
| `FLAG_MIME_AVI_DIVX` | `x-msvideo` → `video/divx` if `CREATOR` set, else `video/avi` |
| `FLAG_MIME_FLAC_FLAC` | `audio/x-flac` → `audio/flac` |
| `FLAG_MIME_WAV_WAV` | `audio/x-wav` → `audio/wav` |
| `FLAG_MS_PFS` | bitrate / 8; PFS magic ids; no video art `<res>`; `?albumArt=true` |
| `FLAG_FORCE_SORT` | default `ORDER BY CLASS, DISC, TRACK, TITLE` |
| `FLAG_AUDIO_ONLY` | Browse `0` → Music |
| `FLAG_HAS_CAPTIONS` | runtime, not a client type |

Sony BDP / Bravia / Toshiba extra `<res>` are switched on **`client` enum**,
not a flag.

---

## 9. GENA (`/evt/…`)

Source: `src/upnpevents.c`, subscribe handlers in `src/upnphttp.c`.

Subscribe:

- `CALLBACK: <http://{client-ip}[:port]/path>`
- `NT: upnp:event`
- `TIMEOUT: Second-{n}` (optional)

Rules:

- Callback host **must** be the TCP peer IPv4 (else 412).
- `http://` only; default port 80.
- `SID` + `Callback` together → 400.
- Missing `NT` on new subscribe → 400.
- Renew: `SID` without `NT`. Unknown SID → 412.
- Renew response **forces** `Timeout: Second-300` (DLNA 5-minute rule).
- Max 500 subscribers.

Event notify the daemon **sends**:

```
NOTIFY {path} HTTP/1.1\r\n
Host: {ip}{:port}\r\n
Content-Type: text/xml; charset="utf-8"\r\n
Content-Length: {n}\r\n
NT: upnp:event\r\n
NTS: upnp:propchange\r\n
SID: {sid}\r\n
SEQ: {u}\r\n
Connection: close\r\n
Cache-Control: no-cache\r\n
\r\n
{propertyset}
```

Body (`getVarsContentDirectory` etc.):

```
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0" xmlns:s="{serviceType}">
  <e:property><SystemUpdateID>{updateID}</SystemUpdateID></e:property>
  … other EVENTED state vars …
</e:propertyset>
```

`updateID` increments when the library changes; `GetSystemUpdateID` and
Browse `UpdateID` use the same counter.

---

## 10. How scan metadata becomes protocol

Only the bits a replica must reproduce in DIDL / HTTP.

`src/metadata.c` assigns `MIME` and `DLNA_PN` from libav:

- Matroska (`iformat` name starts with `matroska`) → `video/x-matroska`
  and **`goto video_no_dlna`**. `DLNA_PN` stays **NULL**. HEVC, DV P7,
  HDR10 — all the same: empty PN, mime `video/x-matroska`.
- AVI → `video/x-msvideo`; XVID/DX50/DIVX fourcc sets `CREATOR=DiVX`
  (feeds the PS3 divx mime rewrite).
- MPEG-TS / PS / MP4 H.264 get real `AVC_*` / `MPEG_*` PNs and sometimes
  `video/vnd.dlna.mpeg-tts`.
- `.mov` → `video/quicktime`.
- FLV / RealMedia as in the MIME table; no PN.

`mime_to_ext` for unknown video is `dat` (`/MediaItems/{id}.dat`). After
Samsung’s in-place `x-mkv` rewrite, ext is `mkv`.

NFO sidecars (`src/nfo.c`) can replace `dc:date` / title / plot; the date
is normalized before it is stored, and **again** on emit.

---

## 11. Gotchas (client-visible)

This is the dialect. Copy these or clients misbehave in the ways this
tree already fixed or still lies about.

### Kodi `dc:date` and year 1905

Kodi’s Platinum `NPT_DateTime` `FORMAT_W3C`
(`NptTime.cpp`, not in this tree) accepts:

- `YYYY-MM-DD` — **exactly 10** characters, or
- a datetime **with timezone**, length **≥ 20** (`…SSZ` or `…SS+00:00`).

A 19-character `YYYY-MM-DDTHH:MM:SS` is rejected (`input_size < 20` when
seconds are present). Platinum then **clears** `m_Date`. Kodi treats a
leftover year as an OLE serial day count from 1899-12-30 → mid-**1905**.

This tree:

- **Must not** emit 19-char datetimes.
- `w3c_date_from_time` writes `YYYY-MM-DDTHH:MM:SSZ` (20 chars + NUL,
  UTC, needs `buflen >= 21`).
- `w3c_normalize_date` on emit:

  | input | output |
  |---|---|
  | `1999` (4-digit year) | `1999-01-01` |
  | `2024-03-15T14:30:00` or space-T | `2024-03-15T14:30:00Z` |
  | EXIF `2024:03:15 14:30:00` | `2024-03-15T14:30:00Z` |
  | already `…Z` / 10-char day / anything else | copied through |

Audio in this tree was already often 10-char `YYYY-01-01`. Video used to
be the 19-char form; that is the 1905 bug.

After a date-format change, Kodi must refresh/restart so it does not keep
cached DIDL.

### `FLAG_SKIP_DLNA_PN`

Samsung BD J5500: even a valid `DETAILS.DLNA_PN` is omitted from
`protocolInfo`. Only `DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=…`.

### Empty `DLNA_PN` on HEVC / DV MKV remuxes

Not a bug in emit. Scan never assigns a PN for matroska. DIDL for a
`FLAG_DLNA` client (including Kodi) is:

```
protocolInfo="http-get:*:video/x-matroska:DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000…"
```

HTTP `contentFeatures.dlna.org` likewise has no `DLNA.ORG_PN=`.
`GetProtocolInfo` still lists a canned PN table that does not mention HEVC.

### Fake extra `<res>` (`CI=1`) still serve the original file

Toshiba / Sony BDP / Bravia extra rows change `protocolInfo` only. The
URL is the same `/MediaItems/{id}.{ext}`. HTTP `CI` is always `0`.
A replica that actually transcodes those URLs is not this daemon.

### MIME remaps (DIDL and HTTP)

| Who | From | To |
|---|---|---|
| `FLAG_SAMSUNG` | `video/x-matroska` | `video/x-mkv` |
| Samsung Series A | `video/x-msvideo` | `video/mpeg` (HTTP) |
| Sony BDP | `x-matroska` or `mpeg` | `video/divx` (HTTP); DIDL may also advertise `video/avi` extra res |
| `FLAG_MIME_AVI_AVI` (Kodi, Xbox, …) | `video/x-msvideo` | `video/avi` |
| `FLAG_MIME_AVI_DIVX` (PS3) | `x-msvideo` + DiVX creator | `video/divx` |
| FreeBox + TS PN | mime → `video/mp2t` |
| non-`FLAG_DLNA` | `video/vnd.dlna.mpeg-tts` | `video/mpeg` |
| `FLAG_MIME_FLAC_FLAC` | `audio/x-flac` | `audio/flac` |
| `FLAG_MIME_WAV_WAV` | `audio/x-wav` | `audio/wav` |

Samsung rewrite is `strcpy(mime+8, "mkv")` on `video/x-matroska` (keeps
`video/x-`).

### Host header 400

Only literal IPv4 (plus optional numeric port). Hostnames are rejected.
HTTP/1.1 without `Host` is rejected.

### TimeSeek / PlaySpeed without `Range` → 406

No NPT seek. Clients that send `TimeSeekRange.dlna.org` and no byte
`Range` get 406.

### Media is `Connection: close`; SOAP may Keep-Alive

`/MediaItems/` always close + `fork`. SOAP/SCPD/art/captions follow
`http_should_persist`, max 100 requests. Do not Keep-Alive the media
child.

### `DLNA.ORG_OP` / `CI` / `FLAGS`

On the real file: `OP=01` (byte Range), `CI=0`, FLAGS =
`DLNA_V1_5 | HTTP_STALLING | TM_B | TM_S` for A/V (`0x01700000` plus 24
trailing zero hex digits). Images use `TM_I` instead of `TM_S`.

HTTP media always sends `Accept-Ranges: bytes`.

### Caption URL shape

Indexed (DIDL `FLAG_CAPTION_RES` / `sec:CaptionInfoEx`):

`http://{ip}:{port}/Captions/{detailID}/{index}.{ext}`

Header `CaptionInfo.sec` and `pv:subtitleFileUri`:

`http://{ip}:{port}/Captions/{detailID}.srt`

The GET handler accepts both: `strtoll` then optional `/{index}`.

### Xbox / Samsung `rootDesc` variants

Xbox: `modelNumber=1` and `friendlyName` contains `: 1`.
Samsung DCM10: `sec:ProductCap` / `sec:X_ProductCap` =
`smi,DCM10,getMediaInfo.sec,getCaptionInfo.sec`.

### AllShare must not look like a TV

`SEC_HHP_[PC]` is a **non-Samsung** `FLAG_DLNA` entry **above**
`SEC_HHP_`. Giving it `X_GetFeatureList` BASICVIEW / caption res breaks
Windows AllShare.

### Samsung BDP vs TV

`SEC_HHP_BD*` is a separate type **without** `FLAG_CAPTION_RES`. Extra
caption `<res>` rows trigger a folder-browse bug on those BDPs.

### Bookmark units

`upnp:lastPlaybackPosition` is **raw seconds**, not `H:MM:SS` (comment in
`upnpsoap.c`: Kodi is the consumer). Samsung Q (`FLAG_CONVERT_MS`)
multiplies by 1000 on the way out and divides on `X_SetBookmark`.
Positions under 30 seconds are stored as 0.

### Browse `Filter` and namespaces

Empty / `*` filter omits vendor `pv:` / `sec:` **except** Samsung, which
gets `sec` by default. A Kodi `Filter` that does not list `dc:date` will
not receive `<dc:date>` (`FILTER_DC_DATE`). Most clients send `*`.

### Sort

LG TV (`ELGDevice`) forces `ORDER BY CLASS, TITLE`.
`FLAG_FORCE_SORT` / Panasonic / NetFront force
`CLASS, DISC, TRACK, TITLE`.
Playlists under `1$F` sort by title (the list) or by `OBJECT_ID` length
then id (items).
DLNA clients get SOAP 709 on unparseable `SortCriteria`.

### SSDP notify header spacing

NOTIFY uses `HOST:239…` (no space). M-SEARCH 200 uses `LOCATION: http://…`
(space). `LOCATION` path is always `/rootDesc.xml`.

### M-SEARCH `MAN` quotes

`MAN: "ssdp:discover"` including the double quotes. Unquoted
`ssdp:discover` is ignored.

### `ssdp:all` vs specific ST

Specific ST: **one** response (first table match) then return.
`ssdp:all`: one response **per** known type (6).

### SOAP method prefix match

`strncmp` — `Browse` matches any action that starts with `Browse`.
Unknown → 401, HTTP 500 + UPnPError body (not HTTP 404).

### POST URL is not the control URL

Any `POST` with `SOAPAction` is executed. SCPD `controlURL` values are
still `/ctl/ContentDir` etc. and must be advertised that way.

### Object ID `64` / `1` / `2`

Browse Folders / Music / Video. Samsung BASICVIEW and many TVs hard-code
these. Changing them is not optional if the goal is “this daemon”.

### Presentation `/` and `/status`

HTML, `text/html`, not DIDL. Safe to keep or 404; this daemon serves a
status page with client-cache rows.

### Stream Range vs last-32MB cue probes

A Range whose start is in the last 32 MiB of a large file must not reset
an existing prefetch window. HTTP Range semantics stay standard; only
the cache policy changes. Without this, Kodi cue reads make the next
play look like a hung `:8200`.

### No transcode, no Chromecast profile

A Google Streamer that cannot decode DV Profile 7 MKV / TrueHD will get
exactly those bytes. This is protocol-correct for *this* server.

---

## 12. Source map

| Topic | File |
|---|---|
| Paths / control / event URLs | `src/path table` |
| Version, FLAGS, `RESOURCE_PROTOCOL_INFO_VALUES` | `src/upnpglobalvars.h` |
| SSDP | `src/minissdp.c` |
| HTTP parse, dispatch, media | `src/upnphttp.c` |
| Persist rule | `src/httpersist.h` |
| SOAP + DIDL | `src/upnpsoap.c`, `src/upnpsoap.h` |
| Description XML | `src/upnpdescgen.c` |
| GENA | `src/upnpevents.c` |
| Client table / flags | `src/clients.c`, `src/clients.h` |
| Object ID roots | `src/scanner.h` |
| Magic containers | `src/containers.c` |
| Dates | `src/utils.c` `w3c_date_from_time`, `w3c_normalize_date` |
| NFO year | `src/nfo.c` |
| MIME / PN assignment | `src/metadata.c` |
| Caption MIME / `mime_to_ext` | `src/utils.c` |
| Range window / 32 MiB tail | `src/streamcache.c` |

A replica that matches SSDP + `rootDesc` + `Browse` DIDL (including
`dc:date` normalize and the client MIME/`<res>` lies) + `/MediaItems/`
headers will be accepted by the clients this tree was built for. Everything
else is optional compatibility.

### Quoted constants (as in `src/`)

```
#define ROOTDESC_PATH 				"/rootDesc.xml"
#define CONTENTDIRECTORY_CONTROLURL		"/ctl/ContentDir"
#define CONNECTIONMGR_CONTROLURL		"/ctl/ConnectionMgr"
#define BROWSEDIR_ID		"64"
#define MUSIC_ID		"1"
#define VIDEO_ID		"2"
#define SSDP_MCAST_ADDR ("239.255.255.250")
#define FLAG_SKIP_DLNA_PN       0x00002000 /* during browsing */
{ "Browse", BrowseContentDirectory},
{ "Search", SearchContentDirectory},
{ "GetProtocolInfo", GetProtocolInfo},
{ "X_GetFeatureList", SamsungGetFeatureList},
{ "X_SetBookmark", SamsungSetBookmark},
strftime(buf, buflen, "%Y-%m-%dT%H:%M:%SZ", tm)
"Kodi"
```
