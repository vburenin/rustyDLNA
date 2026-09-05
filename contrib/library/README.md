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
| `fetch-dlna-artwork.py` | Fill missing `{stem}-poster.jpg` / `poster.jpg` sidecars |
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
clamped to 10 minutes–12 hours. Progress uses bounded nonblocking reads, so an
unfinished or oversized line cannot postpone the deadline or cancellation.
On timeout or interruption the generator terminates the helper process group,
allows up to five seconds for termination, then kills remaining processes and
reaps its child before releasing the title lock. The previous published
preview revision stays usable.

## Credentials

TMDB and OMDb keys are read only from the environment. The programs already
degrade to cached/IMDb-only mode when a key is missing. Never export secrets
from `update.sh` or any other file in this directory.

## Tests

```sh
python3 -m unittest discover -s contrib/library/tests -p 'test_*.py'
```
