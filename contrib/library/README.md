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

## Credentials

TMDB and OMDb keys are read only from the environment. The programs already
degrade to cached/IMDb-only mode when a key is missing. Never export secrets
from `update.sh` or any other file in this directory.

## Tests

```sh
python3 -m unittest discover -s contrib/library/tests -p 'test_*.py'
```
