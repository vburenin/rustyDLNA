# Inherited product behavior

A replica that only copies generic UPnP 1.0 will still fail real clients. Keep
these behaviors when `scan` / SOAP land.

## Dates (Kodi 1905)

Emit `YYYY-MM-DD` or `…Z` (length ≥ 20). Never a
19-character local datetime. Implemented in `rusty_dlna_protocol::date`.

NFO wins when present: `<premiered>`, then `<aired>`, then `<year>`
(`1999` → `1999-01-01`), then file mtime.

## Library paths

- `exclude_dir=` — prefix or path-component; scan + inotify.
- Built-in junk dirs: `@eaDir`, `#recycle`, `lost+found`, `$RECYCLE.BIN`,
  `System Volume Information`, `.Trash` / `.Trash-<uid>`.
- Unfinished suffixes: `.part`, `.!qB`, `.!ut`, `.bc!`, `.crdownload`,
  `.aria2`, `.download`, `.tmp`.
- Case-sensitive `sample/` and `trailer/` directories; `*-sample.*` files.

`rusty_dlna_scan` has the skip helpers.

## Artwork and NFO

- Kodi `*-poster.jpg/.png`, `*-fanart.jpg/.png`, folder `poster.jpg`.
- PNG converted to JPEG for DLNA.
- Optional video thumbnails only when no sidecar/embedded art.
- Video Series (`2$E`) and Genre (`2$9`) from NFO `showtitle` / `<genre>`.
- `tvshow.nfo` inherited by episodes.

## Symlink aliases

Every path is browseable. Metadata is per inode (device + inode). Later
NFO/poster updates rewrite every alias. Deleting one path must not
delete the others.

## Subtitles

`.srt .smi .ass .ssa .vtt .sub`, language suffixes.
Advertise `/Captions/{id}/{n}.{ext}` and `/Captions/{id}.srt`.

## HTTP

Keep-Alive for SOAP/desc/art/captions. **Never** for `/MediaItems/`
(or a transcode pipe). Host must be literal IPv4 or 400. TimeSeek
without Range → 406.

## Version / name

rustyDLNA reports `rustyDLNA/{version}` with `DLNADOC/1.50 UPnP/1.0` tokens.

## Not inherited as architecture

Fork-per-GET, `MAP_SHARED` stream cache, lying `CI=1` extra `<res>`
that still serve the remux, empty client entry for Cast.
