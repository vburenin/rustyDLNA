# Protocol and product behavior

A generic UPnP 1.0 implementation will still fail real clients. These behaviors
are part of rustyDLNA's protocol contract.

## Dates (Kodi 1905)

Emit `YYYY-MM-DD` or `…Z` (length ≥ 20). Never a
19-character local datetime. Implemented in `rusty_dlna_protocol::date`.

NFO wins when present: `<premiered>`, then `<aired>`, then `<year>`
(`1999` → `1999-01-01`), then file mtime.

Image EXIF dates are checked as ASCII timestamps before normalization. Missing,
short, malformed, or non-ASCII timestamp fields are ignored without rejecting
an otherwise usable JPEG; the image keeps its other metadata and file-date
fallback. Valid EXIF timestamps and normalized date forms retain their dates.

## Library paths

- `exclude_dir=` — prefix or path-component; scan + inotify.
- Built-in junk dirs: `@eaDir`, `#recycle`, `lost+found`, `$RECYCLE.BIN`,
  `System Volume Information`, `.Trash` / `.Trash-<uid>`.
- Unfinished suffixes: `.part`, `.!qB`, `.!ut`, `.bc!`, `.crdownload`,
  `.aria2`, `.download`, `.tmp`.
- Case-sensitive `sample/` and `trailer/` directories; `*-sample.*` files.

`rusty_dlna_scan` has the skip helpers.

Configured and canonical media-root mappings, including byte-preserving
historical relocation aliases, are read, cross-root validated, and persisted
in one immediate SQLite transaction. Historical aliases survive repeated
restarts and later moves while their root remains configured; removing a root
removes that root's stored mapping family. Each root retains at most 64 aliases
and 64 KiB of alias path bytes, and the complete mapping is bounded to 4,096
settings and 4 MiB. An ambiguous or over-budget historical map, key collision,
or failed setting write leaves both the stored mappings and the caller's root
set unchanged.

## Demuxer input confinement

The scanner recognizes the explicit single-file demuxers in
`crates/protocol/src/media_input.rs`; unknown formats have no Matroska fallback.
DASH, HLS, concat manifests and image sequences cannot become catalog media by
using an admitted filename extension. Library playlist parsing remains separate.

Before libav opens headers or discovers streams, a custom seekable AVIO reads
only the admitted regular-file descriptor, and its nested-open callback rejects
all secondary I/O. Attached pictures use the same owner and limits. Image resize,
thumbnail generation, stream admission and transcode helpers use FFmpeg/FFprobe's
seekable `fd` protocol with only `fd` permitted and the same demuxer allowlist;
`file`, network and wrapper protocols are unavailable to media inputs. This also
covers every Profile-8 intermediate input. An independent descriptor cursor keeps
repeated probes and playback attempts from interfering with each other. Existing
cancellation, absolute helper deadlines, inode aliases and byte-preserving paths
continue to apply.

The external helper boundary requires FFmpeg 6 or newer (including production
FFmpeg 8), which provides the seekable `fd` protocol. The custom AVIO adapter is
compiled against the installed libav headers to account for structure layouts.
This is a demuxer I/O boundary, not an OS sandbox for arbitrary decoder code.

Stream-probe revision 7 rechecks older catalog entries in the private scan stage.
A failed probe removes an old entry only when a bounded, byte-only format probe
positively recognizes an unsupported demuxer. It reads at most 1 MiB and never
opens nested resources. Ordinary decoder/I/O failures retain supported entries,
and cancellation leaves the published catalog intact. Current unchanged inode
aliases reuse their existing probe result.

## Artwork and NFO

- Kodi `*-poster.jpg/.png`, `*-fanart.jpg/.png`, folder `poster.jpg`.
- PNG converted to JPEG for DLNA.
- Optional video thumbnails only when no sidecar/embedded art.
- Video Series (`2$E`) and Genre (`2$9`) from NFO `showtitle` / `<genre>`.
- `tvshow.nfo` metadata applied to episodes.
- Movie `<outline>` is the spoiler-safe About description and DLNA
  `dc:description`; `<plot>` is retained separately and revealed only through
  the browser's explicit spoiler disclosure.
- On Linux, non-UTF-8 media stems remain byte-exact when matching built-in or
  configured `{stem}` / `%s` artwork names; ASCII sidecar suffixes such as
  `-poster.jpeg` are still recognized.

Targeted NFO events and periodic reconciliation use the same precedence:
filename/mtime defaults, embedded metadata, inherited `tvshow.nfo`, then local
NFO overrides. Removing a sidecar or an individual tag restores the underlying
value and rebuilds the affected virtual groups for every physical alias. The
catalog stores a fingerprint of the effective parsed NFO, including missing
fields. A changed fingerprint during reconciliation causes one physical
metadata probe; unchanged passes do not probe or publish another visible
generation. Newly admitted aliases also apply their sidecars immediately,
reusing the cloned embedded/default base when it has no previous NFO override.
Video filename titles stay local to each alias unless the effective NFO supplies
a title or show title; genre-only metadata does not replace those defaults. Catalogs predating
this fingerprint reconstruct their metadata once during reconciliation.
Malformed sidecars, cancellation, or probe-operation errors leave the published
catalog intact; a media decoder that returns no metadata still has the valid
filename/mtime defaults.

Media and sidecar reads require a regular-file descriptor confined to a
configured root. Both the Linux `openat2` path and its component-walking
fallback open without waiting for a FIFO writer, then verify the opened file's
type before reading. FIFOs, sockets, directories, and devices are rejected,
including when reached through an allowed symlink or substituted for a file
between path inspection and opening.

## Probe sidecars

For a media path `movie.ext`, the scanner considers the raw, byte-preserving
names `movie.ext.probe.toml` and then `movie.probe.toml`; the first eligible
confined file wins. The selected sidecar can override container, video, audio,
HDR, width, and height. Its bounded canonical semantic fingerprint is persisted
per browseable path, while the underlying libav probe and generated artwork are
shared once per physical file. This lets hard-link and symlink aliases keep
different path-local overlays without duplicating the physical probe.

Sidecar creation, replacement, deletion, and offline edits converge through
targeted, periodic, full, and fill-missing scans. Removing an override replaces
the affected fields from a fresh base probe instead of retaining stale values;
an all-empty or comment-only override is semantically the same as no override.
Schema-v10 upgrade rows have no provenance fingerprint and therefore receive a
one-time, bounded, private-stage reprobe before normal fingerprint reuse begins.

## Timeline-preview sidecars

For `movie.ext`, an offline generator may publish browser trick-play assets in
the video's parent directory at `.rusty_previews/movie/`. The hidden
`.rusty_previews` container is excluded from scanning and inotify traversal
even when hidden files are enabled. Each media-stem directory contains
`manifest.json` and revisioned `sheet-<16-lowercase-hex>-NNNN.jpg` assets. The
manifest is published last; rustyDLNA never writes the media root or generates
previews at request time. The operator generator is
`contrib/library/generate-dlna-previews.py`. Legacy `*.rustydlna-previews/`
directories remain scanner-excluded but are not served.

Schema 1 layout is manifest-driven. The offline generator defaults to 640×360
frames in a 5×10 grid, but it may publish another resolution and grid without a
server configuration change. Each frame edge is 16–4096 pixels and at most
4,194,304 pixels; grids are at most 10×10, and the resulting JPEG is at most
4096 pixels per edge and 12,000,000 decoded pixels. The layout capacity is the
smaller of 2,400 frames and `columns × rows × 256`; the whole-second interval
is `max(1, ceil(duration_seconds / capacity))`. Normal layouts therefore keep
the existing 2,400-frame cadence, while large or portrait frames use a longer
deterministic interval rather than exceeding 256 sheets.

The manifest records the source byte length and nanosecond mtime, duration,
interval, layout, frame count, and asset revision. The browser API exposes it
only when the source identity still matches, every referenced sheet exists
within the configured-root jail, all fields satisfy the schema bounds, and a
requested JPEG has the exact dimensions implied by the recorded frame and
grid. Stale, malformed, oversized, symlinked, escaped, or incomplete sets are
treated as unavailable rather than partially served. Raw non-UTF-8 media stems
remain byte-exact when deriving the child directory beneath `.rusty_previews`.

## Symlink aliases

Every path is browseable. Physical probe work is shared per inode (device +
inode), while probe-sidecar overlays remain path-local. Later NFO/poster updates
rewrite every compatible alias. Deleting one path must not delete the others.

Recursive `tvshow.nfo` refresh follows the catalog walker's ancestor device/inode
cycle policy. Self-links and links to ancestors are pruned, while separate
non-cyclic directory aliases remain browseable and receive metadata updates.
The walk checks cancellation before entry and between directory entries.

## Subtitles

One protocol-owned table defines caption admission, canonical extension, and
original HTTP MIME type: `.srt` (`text/srt`), `.ass` and `.ssa`
(`text/x-ssa`), `.vtt` (`text/vtt`), `.smi` (`smi/caption`), and `.sub`
(`text/plain`). Final extensions match ASCII case-insensitively. On Unix, the
media and sidecar stems are compared as raw filesystem bytes: a caption belongs
to a title only when its stem is exactly the media stem or begins with that
exact stem plus `.`. The first two- or three-letter language subtag of that
dot-owned variant is the optional language label; `.`, `-`, and `_` can end the
subtag, but leading `-en` and `_en` names are not associated variants.

The same table declares whether a format has a bounded browser WebVTT
conversion. WebVTT validation and SRT, ASS/SSA, and SMI conversion are
supported. The ambiguous `.sub` extension remains available to DLNA clients as
an indexed sidecar but has no browser conversion; unknown formats are neither
admitted nor converted.

Advertise `/Captions/{id}/{n}.{ext}` and `/Captions/{id}.srt`. Indexed caption
numbers are unsigned 32-bit values; negative and overflowing indices are
rejected, while a decorative suffix after the numeric prefix remains compatible.

## Persisted stream metadata

The compact SQLite stream descriptor is a compatibility format shared by the
scanner, catalog restoration, transcode selection, and browser API. Parsing is
bounded to 1 MiB, exposes at most 1,024 audio records, and inspects at most 512
chapter entries. Marker presence remains distinct from field validity so older
partial `@v:` and `@t:` records retain their conservative fallback behavior.
The scanner writes audio records followed by `@v:`, `@t:`, and optional `@c:`
records through the protocol crate's canonical encoder.

Persisted media detail IDs are positive SQLite integers below `i64::MAX`, so a
next ID always exists without wrapping. File sizes must fit a nonnegative
SQLite `i64`; oversized or corrupt negative values fail catalog loading rather
than being reinterpreted. Unix device and inode values deliberately preserve
all native `u64` bits in SQLite's signed integer representation and restore
those bits explicitly.

## Catalog publication and generations

Filesystem walks, sidecar parsing, artwork selection, playlist work, and media
probes run against a private disk-backed SQLite stage. Publication validates a
non-wrapping internal catalog epoch and merges only the stage's disk-bounded
key journals with set-wise SQL. The live transaction is limited to that
journal-keyed merge, live bookmark capture, and epoch/updateID writes; it never
runs filesystem, probe, or helper work, and targeted latency is proportional
to journaled keys. Scanner publication never copies staged `BOOKMARKS` or
`updateID`: bookmark retention is decided against the current live rows, and
the replacement catalog's canonical bookmark snapshot is captured inside the
live merge transaction, including any retention deletion. That snapshot is
applied to the owned replacement catalog only after the database writer is
released. A cancelled, stale, failed, or dropped prepared change
cannot expose a partial catalog and forces a fresh backup before later
preparation.

The live database merge, current-bookmark capture, and optional `updateID`
write commit atomically. When a server generation is supplied for a real
catalog change, it must be exactly the next wrapping value derived from the
current live transaction; stale or skipped values roll back the entire merge,
while a semantic no-op consumes no generation. The in-memory catalog changes
under the same publication serialization, and DB-backed web, Browse, and Search
pages are accepted only when the app generation before and after the query
matches the database generation read from the same SQLite snapshot. Each
request makes at most three snapshot attempts. UPnP's `ui4` SystemUpdateID
advances modulo 2^32 (`4294967295` wraps to `0`); every publication clears
generation-keyed query caches so wrap cannot revive an ancient page.
Class searches compare full DIDL `object.*` values case insensitively in both
SQLite and memory. Equality, inequality, ordering, and `derivedfrom` also accept
the stored short form (for example `item.videoItem`). `contains` and
`doesNotContain` search the same full value with literal substring semantics and
are complementary; substring needles are never prefixed. Stored classes and
object IDs are unchanged.

SOAP Search criteria admit at most 128 KiB of UTF-8 input, 512 tokens, and
64 KiB per literal. Before expanding parentheses, rustyDLNA checks the complete
expression against 256 OR groups, 64 clauses per group, 1,024 total expanded
clauses, and 256 KiB of expanded literal bytes. Exceeding any limit returns
fault 708 before catalog conversion or SQL parameter allocation. Search cache
keys are canonical structured-query digests built incrementally, with fixed
storage independent of literal escaping or expansion size.

SQLite and in-memory Browse/Search ordering both append ascending object ID as
the final tie-breaker so page boundaries remain stable during fallback.

Scanner child allocation initializes the maximum legacy hexadecimal suffix once
per parent in a writable transaction. Connection-private SQLite TEMP state tracks
insertions, moves, and deletion of the maximum suffix, and rolls back with
transactions and savepoints. A later transaction rechecks persisted rows. This
changes neither the stored schema nor the existing IDs, overflow checks, or
deletion/recreation rules. Catalog restoration appends unique item rows and uses
temporary membership sets for seeded and mirrored container overlap. Existing
mirror metadata and folder child vectors are not cloned again.
Aggregate membership starts with the existing inode index and checks only that
inode's object mappings. Bulk patches build child membership once per affected
parent receiving multiple links, preserving ordered vectors and seeded overlap.
Fixed-size patches remove changed detail mappings directly; object-only changes
also journal their affected details so canonical
representatives are reselected from surviving mappings.

Playlist discovery and member resolution check scanner cancellation before
continuing private staged work. Empty discovery does not read or canonicalize
media details, while disappeared playlists are still removed. Targeted media
arrivals and playlist events reuse known playlist paths and resolve requested
members through path/inode indexes, retaining duplicates, alias-local paths,
encoding rules, and references that become available after later media arrivals.

Browser search lowercases Unicode text with Rust's lowercase mapping and matches
literal substrings of the displayed title, artist, album artist, album, and
filename. It does not search parent directory names, fold accents, normalize
composed/decomposed Unicode, or interpret wildcard characters. SQLite and memory
use the same functions, including byte-preserving stored-path decoding followed
by lossy filename presentation. This browser rule does not change SOAP search.

Catalog requests share one five-second absolute budget across heavy-query and
reader admission, SQLite execution, generation retries, and memory fallback. SQLite progress
callbacks belong exclusively to a reader lease and are removed before reuse;
cancelling an old request cannot interrupt a later reader owner. Cancellation or
budget exhaustion does not start another fallback query or publish a partial
result. Connection-task cancellation, server shutdown, and detected socket reset
cancel the underlying work. A TCP read-side FIN can be a valid HTTP half-close,
so FIN alone retains the absolute deadline while the response is prepared.
At most four browser-library or SOAP Browse/Search queries execute concurrently;
media, item-detail and status routes do not take this admission permit. Memory
query projections stop at one million inspected records and a 64 MiB scratch/key
budget. These are query work bounds, not a process RSS limit. Exhausted web queries
return recoverable `503 catalog_busy`; SOAP returns fault 501. Detailed status
reports fixed-cardinality admission wait, reader wait, execution, fallback,
timeout, cancellation and work-budget counters.

## SOAP and SSDP parsing

SOAP arguments are accepted only as direct children of the one action selected
by `SOAPAction`. A normal request must have matching Envelope/Body/action
structure; legacy bare top-level parameter fragments remain accepted. Header
values and nested elements are not arguments. Mismatched or multiple actions
and conflicting duplicate arguments are rejected, while identical repeated
values remain compatible. Browse and Search require explicit `StartingIndex`
and `RequestedCount` arguments; omission is not treated as a request for the
first unbounded page. Signed action integers, including `PosSecond`, are bounded
to the UPnP `i4` range. `SOAPAction` is a singleton HTTP field; repeated fields
are rejected even when their values match. `SortCriteria` is bounded to 1,024
decoded bytes and 32 keys; exceeding either limit follows the existing
invalid-sort behavior, including fault 709 for DLNA clients.

SSDP accepts CRLF or LF line endings and SP/HTAB whitespace, but request-line
tokens remain exact uppercase protocol tokens. Headers require a valid token
followed by a colon. Conflicting routing duplicates, control bytes, and lone CR
line endings are rejected. After line validation, `MAN` and specific service
target comparisons retain the established Unicode-whitespace trimming
compatibility; HTTP header OWS parsing itself remains limited to SP/HTAB.
Search-response jitter stays within the parsed, five-second-capped `MX` window;
the compatible `MX: 0` form is answered without an artificial delay.

Outbound SSDP builders validate UUID, host, search-target, `SERVER`, and `DATE`
line safety before generating any datagram, and a service-type index is checked
rather than used for unchecked indexing. Production callers use the fallible
builders. The original public builder signatures remain available for source
compatibility and fail closed to an empty packet or packet list on invalid
input. Unknown but syntactically safe search targets remain a successful empty
reply list.

## HTTP

Keep-Alive for SOAP/desc/art/captions. **Never** for `/MediaItems/`
(or a transcode pipe). Host must be literal IPv4 or 400. TimeSeek
without Range → 406.

HTTP field names use the shared RFC token grammar, and optional whitespace is
only SP/HTAB. Final response serialization validates the public status, reason,
`Server`, `Date`, and every header field, including fields replaced by the
serializer. Multiple or malformed `Content-Length` fields, any response
`Transfer-Encoding`, line-control bytes, and a declared length that disagrees
with a non-empty in-memory body are rejected before output. A persistent
non-empty in-memory body requires `Content-Length`. HEAD and descriptor-backed
streaming responses intentionally carry an empty in-memory body, so their
advertised length may remain nonzero. Production uses the fallible serializer
and logs a rejection; the legacy infallible method retains its signature and
returns one fixed empty `500 Internal Server Error` response on failure. Byte
ranges use the standard `206 Partial Content` reason, and generated HTML
declares UTF-8.

## Version / name

rustyDLNA reports `rustyDLNA/{version}` with `DLNADOC/1.50 UPnP/1.0` tokens.

## XML output

Configuration, catalog, DIDL, and SOAP values are sanitized to the XML 1.0
character repertoire before emission. Unicode scalar values that XML 1.0 does
not permit become U+FFFD. Normal XML values escape markup and both quote
characters. DIDL serialized inside a SOAP `<Result>` keeps attribute quotes
literal for renderer compatibility while still escaping XML text markup.

## Renderer identity and eventing

The renderer-profile cache holds at most 25 IPv4 addresses for one hour. A
matching MAC can refresh the same address, and admitting a new renderer
reclaims expired addresses before reporting the cache full.

A new GENA subscription admits its mandatory `SEQ: 0` notification atomically
with publishing the subscriber. Later pending updates for that SID coalesce to
the newest sequence but cannot overtake sequence zero. ContentDirectory deltas
retain the union of pending parent IDs and the last update ID received for each
parent, including across sequence/update-ID wrap. A later global update does
not erase earlier parent invalidations. Each pending SID is limited to 1,024
parent IDs and 64 KiB of ID bytes. If either bound is exceeded, the server retires
that subscription and sends its final SystemUpdateID with an empty container
list; renewal returns 412, requiring a new subscription and catalog refresh.
Truncated parent lists are never delivered as complete deltas. If initial notification
admission is full, the subscription is rolled back and returns 503.

## Architectural boundaries

Do not use fork-per-GET, a `MAP_SHARED` stream cache, a misleading `CI=1` extra
`<res>` that still serves the remux, or an empty renderer entry for Cast.
