# Library maintenance tools

Operator programs for a rustyDLNA media tree. They write Kodi-style NFO files,
`{stem}-poster.jpg` sidecars, generated `genres/` views, and `.rusty_previews/`
sprite sheets. rustyDLNA itself never writes the media root; these tools are
the missing operator half of that contract.

They are not part of the DLNA server. Do not run them from a request path.

## Setup

Python 3.10 or newer. There are no PyPI packages; [`requirements.txt`](requirements.txt)
records that contract and the host programs (`ffmpeg`, `ffprobe`, `curl`,
optional `dovi_tool`). Do not `pip install` this file unless a future
dependency is added.

The tools no longer live inside the media library. Point them at the library:

```sh
export RUSTY_DLNA_MEDIA=/path/to/media
# optional metadata providers; never commit real values
# export TMDB_API_TOKEN=
# export OMDB_API_KEYS=

contrib/library/update.sh --dry-run
contrib/library/maintain-library.py --dry-run
```

`--root` overrides the environment. Also accepted: `RUSTY_DLNA_LIBRARY_ROOT`
and `LIBRARY_ROOT`.

Copy `.env.example` to a gitignored location if you want a local env file.
Caches, IMDb dumps, and locks are created at:

```text
$RUSTY_DLNA_MEDIA/.rusty-library/
```

rustyDLNA already skips hidden directories, so that state is not scanned as
media. Do not vendor IMDb datasets or provider caches in Git.

Optional reviewed age overrides: copy `age-overrides.example.tsv` to
`$RUSTY_DLNA_MEDIA/.rusty-library/age-overrides.tsv`.

The default catalog layout is documented in `library.toml.example` and
implemented in `lib/catalog_config.py`.

## Commands

| Command | What it does |
|---|---|
| `maintain-library.py` | Confidence-gated intake of loose root-level movies, then the update |
| `update.sh` | Refresh IMDb data when due, rebuild genre/year/age views, fill NFO and posters |
| `generate-dlna-previews.py` | Write `.rusty_previews/` sprite sheets (separate from `update.sh`) |
| `fetch-dlna-artwork.py` | Fill `{stem}-poster.jpg` / `poster.jpg` for items with no artwork |
| `fetch-movie-descriptions.py` | Write managed NFO `<outline>` / `<plot>` sidecars |
| `find-unclassified-videos.py` | List videos without a live genre link |
| `find-dv-profile7.py` | Report Dolby Vision Profile 7 playback files |
| `recode-dv-profile7.py` | Explicit Profile 7 → Streamer HDR10 conversion |
| `clean-dead-links.sh` | Remove broken symlinks under `genres/` only |
| `audit-library.py` | Read-only identity/classification audit |

`dovi_tool` is expected on `PATH` (the rustyDLNA image already pins it), or
via `DOVI_TOOL`. Do not copy a binary into this directory.

Optical-disc MakeMKV remux and source deletion are not part of this tree.
Keep that as a separate, explicit operator pass.

Intake and every catalog builder share one movie-container list,
`VIDEO_EXTENSIONS` in `lib/catalog_config.py`: `.avi`, `.m4v`, `.mkv`, `.mov`,
`.mp4`, `.mpeg`, `.mpg`, `.ts`, `.webm`, and `.wmv`, all of which rustyDLNA
serves. A loose file intake moves therefore also gets genre/year/age views,
artwork, and previews. Blu-ray and DVD folders are one catalog item each; their
`.m2ts`/`.vob` streams and `.iso` images are only reported by
`find-unclassified-videos.py`. Earlier versions skipped `.mov`, `.mpeg`, `.mpg`,
`.webm`, and `.wmv` catalog files in the builders, so the first run after
upgrading can add views, posters, and previews for files already in catalog
homes; preview them with `update.sh --dry-run` and
`generate-dlna-previews.py --dry-run`.

## Artwork and NFO files

`fetch-dlna-artwork.py` never deletes or overwrites existing artwork. An item
is skipped when it already has any artwork name rustyDLNA recognizes (the
tables in `lib/artwork_names.py`, matched ignoring ASCII case): a movie file's
`{stem}-poster` or `{stem}-fanart` `.jpg`/`.jpeg`/`.png`, and for a disc,
show, or season folder, or a movie that is alone in its folder, `poster`,
`folder`, `cover`, `albumart`, `albumartsmall`, `album`, or `thumb` art.
Folder art shared by several movies in one directory does not stop their
per-movie posters. Names added through the server's `album_art_names` setting
are not known to the tool. A file at the fetcher's own name that is not a
2 KiB–8 MiB JPEG (for example a PNG saved as `poster.jpg`) is kept and
reported as `SKIP invalid-existing`; rename or remove it yourself, or use
`--refetch-existing`. That option replaces only the fetcher's own regular
files (`DRY replace` in a dry run) and still keeps every other artwork name
and symlink. New posters are published without replacing anything, so a file
or symlink created meanwhile wins and nothing is written through a link.
`maintain-library.py` applies the same rule after intake: it reports
`MISSING-POSTER` only for a moved item that has none of the artwork above.

New and replaced posters and managed NFO files get the default file mode
filtered by the operator's umask, like previews; `--regenerate-existing` keeps
each poster's current mode. Earlier versions made posters world-writable
(`0666`), changed the mode of a hard-linked source, and made managed NFO files
group-writable (`0664`). Those modes are not tightened automatically. After
reviewing the command, remove world write from existing sidecars:

```sh
find /path/to/library -type f \( -name 'poster.jpg' -o -name '*-poster.jpg' -o -name '*.nfo' \) \
  -perm -o+w -exec chmod o-w {} +
```

Group write on NFO files is left alone; remove it with `chmod g-w` where the
group should not edit metadata.

## Safe intake and conversion

Intake moves each file, sidecar, or preview/disc directory with an atomic
no-replace rename. Any occupied destination, including a dangling symlink or
an entry created after planning, rejects the move and preserves both entries.
Reviewed replacement plans first archive the incumbent, then populate its
cleared catalog pathname. If a later mapping fails, rollback uses the same
no-replace operation. If another writer has occupied an original pathname,
rollback preserves both entries, continues recovering the other mappings, and
reports the remaining paths for manual recovery. A multi-file plan is not a
filesystem transaction.

Before planning, a loose file must keep the same size and modification time for
`--settle-seconds` (default 30), must not have `.partial` in its name, and must
not be open for writing according to `lsof` (checked before and after the
settle window). If `lsof` does not answer within 10 seconds, the file is kept
for review. `lsof` sees only processes in the same PID namespace that the
operator may inspect, so a writer in another container, on another NFS/SMB
host, or owned by another user (unless maintenance runs as root) is invisible.
Without `lsof` the check is skipped. In those cases the settle window is the
only guard; raise it for sources that can pause for longer, such as torrent
clients or slow network copies.

These moves require one filesystem and native no-replace rename support
(Linux `renameat2`, or macOS `renamex_np`). Cross-device moves and unsupported
filesystems/hosts fail without copying or deleting the source. Run maintenance
as the library owner where possible. A permission retry through `sudo -n`
executes `lib/safe_move.py` with the current Python interpreter and uses the
same primitive; an existing sudo policy that allows only `mv` will reject it.

Profile 7 conversion checks even an already-existing Streamer derivative for
HEVC/hvc1, 10-bit HDR10 metadata and matching, finite video duration before
archiving its source. Invalid or incomplete derivatives leave the source in
the catalog and produce an error, including in dry runs. Archive collisions
never overwrite an existing entry. Rebuilding an existing derivative still
requires `--replace-existing`; the old output stays in place until the build
passes verification. Lossy conversion remains an explicit option.

Preview FFmpeg attempts have an absolute deadline of twice the video duration,
clamped to 10 minutes–12 hours. An attempt also times out when FFmpeg's
reported sheet count and output time stop advancing for twice the video time
one sheet covers (at least 10 minutes, at most the deadline). If a hardware
decoder attempt times out, the generator skips the other hardware variants and
retries the title once with software decoding; a software timeout, or a
timeout with an explicit `--hwaccel` that has no software fallback, fails the
title. Progress uses bounded nonblocking reads, so an
unfinished or oversized line cannot postpone the deadline or cancellation.
On timeout or interruption the generator terminates the helper process group,
allows up to five seconds for termination, then kills remaining processes and
reaps its child before releasing the title lock. The previous published
preview revision stays usable.

The preview generator never changes permissions. New `.rusty_previews/`
directories, sprite sheets, the lock file, and `manifest.json` get the default
creation modes filtered by the operator's umask, and existing directories keep
their mode. Run it with a umask (or as a user/group) that lets the rustyDLNA
service account read the output; it needs no write access. If `.rusty_previews`
or a title directory beneath it is a symlink, that title fails with
`refusing symlinked preview path` (or `preview path is not a directory` for
another non-directory) and nothing is written through the link; lock and
manifest files are opened without following symlinks.

Migration: earlier versions made every preview directory world-writable
(`0777`) and its files `0666`. Those modes are not tightened automatically.
After reviewing the command, remove world write from existing preview trees
only. `find` does not follow symlinks, and only real directories and regular
files are changed:

```sh
find /path/to/library \( -path '*/.rusty_previews' -o -path '*/.rusty_previews/*' \) \
  \( -type d -o -type f \) -perm -o+w -exec chmod o-w {} +
```

## Credentials

TMDB and OMDb keys are read only from the environment. The programs already
degrade to cached/IMDb-only mode when a key is missing. Never export secrets
from `update.sh` or any other file in this directory.

## Tests

```sh
python3 -m unittest discover -s contrib/library/tests -p 'test_*.py'
```
