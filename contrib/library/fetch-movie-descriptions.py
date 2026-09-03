#!/usr/bin/env python3
"""Fetch cached movie descriptions and write owned Kodi-style NFO sidecars.

TMDB overviews become rustyDLNA's spoiler-conscious About text. When an OMDb
API key is available, OMDb's full plot becomes the separately disclosed plot.
Identity comes only from the generated genre index; filenames are never sent
to a remote search endpoint or used to guess a match.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from xml.sax.saxutils import escape


TMDB_API_BASE = "https://api.themoviedb.org/3"
OMDB_API_BASE = "https://www.omdbapi.com/"
CACHE_VERSION = 1
MANAGED_MARKER = "managed-by: rustyDLNA fetch-movie-descriptions.py"
IMDB_ID_RE = re.compile(r"^tt\d+$")
MAX_DESCRIPTION_BYTES = 24 * 1024
USER_AGENT = "rustyDLNA-description-fetch/1.0 (local NFO sidecars)"


@dataclass(frozen=True)
class IndexedMovie:
    source: Path
    imdb_id: str
    tmdb_id: int


class RemoteError(RuntimeError):
    """A bounded remote metadata request failed."""


def normalize_description(value: object) -> str:
    """Return bounded readable text, or an empty string for unusable data."""
    if not isinstance(value, str):
        return ""
    value = value.replace("\r\n", "\n").replace("\r", "\n")
    value = "".join(
        character
        for character in value
        if character in "\n\t" or ord(character) >= 0x20
    )
    value = "\n".join(re.sub(r"[ \t]+", " ", line).strip() for line in value.split("\n"))
    value = re.sub(r"\n{3,}", "\n\n", value).strip()
    if value.casefold() in {"", "n/a", "none"}:
        return ""
    if len(value.encode("utf-8")) > MAX_DESCRIPTION_BYTES:
        return ""
    return value


def load_index(path: Path) -> tuple[list[IndexedMovie], list[str]]:
    """Load unique, confined movie identities from the generated index."""
    movies: dict[Path, IndexedMovie] = {}
    issues: list[str] = []
    try:
        source = path.open(encoding="utf-8", newline="")
    except OSError as error:
        return [], [f"cannot read {path}: {error}"]
    with source:
        reader = csv.DictReader(source, delimiter="\t")
        required = {"source", "imdb_id", "tmdb_id"}
        if not required.issubset(reader.fieldnames or ()):
            return [], [f"unsupported genre index header: {path}"]
        for line_number, row in enumerate(reader, start=2):
            raw_source = row.get("source", "").strip()
            relative = Path(raw_source)
            if (
                not raw_source
                or relative.is_absolute()
                or ".." in relative.parts
            ):
                issues.append(f"line {line_number}: unsafe source path {raw_source!r}")
                continue
            imdb_id = row.get("imdb_id", "").strip()
            raw_tmdb_id = row.get("tmdb_id", "").strip()
            if not IMDB_ID_RE.fullmatch(imdb_id) or not raw_tmdb_id.isdecimal():
                issues.append(
                    f"line {line_number}: no exact IMDb/TMDB identity for {raw_source}"
                )
                continue
            movie = IndexedMovie(relative, imdb_id, int(raw_tmdb_id))
            previous = movies.get(relative)
            if previous is not None and previous != movie:
                issues.append(
                    f"line {line_number}: conflicting identity for {raw_source}"
                )
                continue
            movies[relative] = movie
    ordered = sorted(movies.values(), key=lambda item: str(item.source).casefold())
    owners: dict[Path, IndexedMovie] = {}
    collisions: set[Path] = set()
    for movie in ordered:
        nfo = movie.source.with_suffix(".nfo")
        previous = owners.get(nfo)
        if previous is not None and previous.source != movie.source:
            collisions.add(nfo)
            issues.append(
                f"NFO collision: {previous.source} and {movie.source} both own {nfo}"
            )
        else:
            owners[nfo] = movie
    if collisions:
        ordered = [
            movie
            for movie in ordered
            if movie.source.with_suffix(".nfo") not in collisions
        ]
    return ordered, issues


def load_cache(path: Path) -> dict[str, object]:
    if not path.is_file():
        return {"version": CACHE_VERSION, "tmdb": {}, "omdb": {}}
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"warning: ignoring invalid description cache {path}: {error}", file=sys.stderr)
        return {"version": CACHE_VERSION, "tmdb": {}, "omdb": {}}
    if (
        not isinstance(payload, dict)
        or payload.get("version") != CACHE_VERSION
        or not isinstance(payload.get("tmdb"), dict)
        or not isinstance(payload.get("omdb"), dict)
    ):
        print(f"warning: ignoring unsupported description cache {path}", file=sys.stderr)
        return {"version": CACHE_VERSION, "tmdb": {}, "omdb": {}}
    return payload


def save_cache(path: Path, cache: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(cache, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        delete=False,
    ) as handle:
        temporary = Path(handle.name)
        try:
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    temporary.replace(path)


def request_json(
    url: str,
    *,
    headers: dict[str, str],
    not_found_codes: set[int] | None = None,
) -> object | None:
    request = urllib.request.Request(url, headers=headers)
    accepted_not_found = not_found_codes or {404}
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            if error.code in accepted_not_found:
                return None
            if error.code == 429 and attempt < 2:
                try:
                    delay = int(error.headers.get("Retry-After", "2"))
                except ValueError:
                    delay = 2
                time.sleep(max(1, min(delay, 10)))
                continue
            raise RemoteError(f"remote service returned HTTP {error.code}") from error
        except urllib.error.URLError as error:
            if attempt < 2:
                continue
            raise RemoteError(f"remote service is unreachable: {error.reason}") from error
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise RemoteError("remote service returned invalid JSON") from error
    raise RemoteError("remote request retries exhausted")


def fetch_tmdb_overview(tmdb_id: int, imdb_id: str, token: str) -> dict[str, object]:
    query = urllib.parse.urlencode({"language": "en-US"})
    payload = request_json(
        f"{TMDB_API_BASE}/movie/{tmdb_id}?{query}",
        headers={
            "Accept": "application/json",
            "Authorization": f"Bearer {token}",
            "User-Agent": USER_AGENT,
        },
    )
    if not isinstance(payload, dict):
        return {"status": "not-found"}
    if payload.get("id") != tmdb_id:
        raise RemoteError("TMDB returned a different movie identity")
    returned_imdb_id = payload.get("imdb_id")
    if returned_imdb_id and returned_imdb_id != imdb_id:
        raise RemoteError("TMDB returned a conflicting IMDb identity")
    return {
        "status": "ok",
        "tmdb_id": tmdb_id,
        "imdb_id": imdb_id,
        "overview": normalize_description(payload.get("overview")),
    }


def fetch_omdb_plot(imdb_id: str, api_key: str) -> dict[str, object]:
    query = urllib.parse.urlencode(
        {
            "apikey": api_key,
            "i": imdb_id,
            "type": "movie",
            "plot": "full",
            "r": "json",
        }
    )
    payload = request_json(
        f"{OMDB_API_BASE}?{query}",
        headers={"Accept": "application/json", "User-Agent": USER_AGENT},
    )
    if not isinstance(payload, dict):
        raise RemoteError("OMDb returned an invalid response")
    if payload.get("Response") == "False":
        error = payload.get("Error")
        if isinstance(error, str) and error.casefold() in {
            "incorrect imdb id.",
            "movie not found!",
        }:
            return {"status": "not-found"}
        raise RemoteError("OMDb rejected the request or its request limit was reached")
    if payload.get("Response") != "True":
        raise RemoteError("OMDb returned an invalid response")
    returned_imdb_id = payload.get("imdbID")
    if returned_imdb_id != imdb_id:
        raise RemoteError("OMDb returned a conflicting IMDb identity")
    return {
        "status": "ok",
        "imdb_id": imdb_id,
        "plot": normalize_description(payload.get("Plot")),
    }


def configured_omdb_keys() -> list[str]:
    """Return distinct OMDb keys in failover order without logging them."""
    configured = os.environ.get("OMDB_API_KEYS", "")
    candidates = re.split(r"[,\n]", configured)
    candidates.append(os.environ.get("OMDB_API_KEY", ""))
    keys: list[str] = []
    for candidate in candidates:
        key = candidate.strip()
        if key and key not in keys:
            keys.append(key)
    return keys


def fetch_omdb_plot_with_fallback(
    imdb_id: str,
    api_keys: list[str],
    start_index: int,
) -> tuple[dict[str, object], int]:
    """Fetch a plot, advancing after a credential rejection or limit."""
    last_error: RemoteError | None = None
    for key_index in range(start_index, len(api_keys)):
        try:
            return fetch_omdb_plot(imdb_id, api_keys[key_index]), key_index
        except RemoteError as error:
            last_error = error
            if key_index + 1 < len(api_keys):
                print(
                    f"warning: OMDb credential {key_index + 1} unavailable; "
                    "trying the next configured credential",
                    file=sys.stderr,
                )
    if last_error is not None:
        raise last_error
    raise RemoteError("no OMDb credentials are configured")


def cached_text(record: object, field: str) -> str:
    if not isinstance(record, dict) or record.get("status") != "ok":
        return ""
    return normalize_description(record.get(field))


def render_nfo(movie: IndexedMovie, about: str, plot: str) -> str:
    lines = [
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>',
        f"<!-- {MANAGED_MARKER} -->",
        "<movie>",
        f'  <uniqueid type="imdb" default="true">{escape(movie.imdb_id)}</uniqueid>',
        f'  <uniqueid type="tmdb">{movie.tmdb_id}</uniqueid>',
    ]
    if about:
        lines.append(f"  <outline>{escape(about)}</outline>")
    if plot and plot.casefold() != about.casefold():
        lines.append(f"  <plot>{escape(plot)}</plot>")
    lines.append("</movie>")
    return "\n".join(lines) + "\n"


def managed_nfo(path: Path) -> bool:
    try:
        with path.open(encoding="utf-8") as source:
            return MANAGED_MARKER in source.read(4096)
    except (OSError, UnicodeDecodeError):
        return False


def publish_nfo(path: Path, content: str) -> str:
    """Atomically publish one owned NFO; never replace a hand-authored file."""
    if path.exists():
        if not managed_nfo(path):
            return "protected"
        try:
            if path.read_text(encoding="utf-8") == content:
                return "unchanged"
        except (OSError, UnicodeDecodeError):
            return "protected"
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        delete=False,
    ) as handle:
        temporary = Path(handle.name)
        try:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
            os.chmod(temporary, 0o664)
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    if path.exists() and not managed_nfo(path):
        temporary.unlink(missing_ok=True)
        return "protected"
    temporary.replace(path)
    return "written"


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Fetch exact-ID movie descriptions and write rustyDLNA/Kodi NFO "
            "sidecars without replacing hand-authored NFO files."
        )
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="audit cached coverage without network requests or writes",
    )
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="refetch cached TMDB overviews and available OMDb full plots",
    )
    parser.add_argument(
        "--show-paths",
        action="store_true",
        help="print each pending, written, protected, or unchanged NFO path",
    )
    from lib.paths import add_root_argument, require_library_root, state_dir

    add_root_argument(parser)
    parser.add_argument("--library-root", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--index", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--cache", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()

    library_root = require_library_root(parser, args.root or args.library_root)
    index_path = args.index or library_root / "genres" / "_genre-index.tsv"
    cache_path = args.cache or state_dir(library_root) / "movie-description-cache.json"
    movies, index_issues = load_index(index_path)
    cache = load_cache(cache_path)
    tmdb_cache = cache["tmdb"]
    omdb_cache = cache["omdb"]
    assert isinstance(tmdb_cache, dict) and isinstance(omdb_cache, dict)

    tmdb_token = os.environ.get("TMDB_API_TOKEN", "").strip()
    omdb_api_keys = configured_omdb_keys()
    omdb_key_index = 0
    tmdb_enabled = bool(tmdb_token) and not args.dry_run
    omdb_enabled = bool(omdb_api_keys) and not args.dry_run
    cache_dirty = False
    request_count = 0
    counts = {
        "indexed": len(movies),
        "written": 0,
        "unchanged": 0,
        "protected": 0,
        "pending": 0,
        "missing_source": 0,
        "about": 0,
        "plot": 0,
    }

    for movie in movies:
        source_path = library_root / movie.source
        try:
            confined = source_path.resolve(strict=True)
            confined.relative_to(library_root)
        except (OSError, ValueError):
            counts["missing_source"] += 1
            if args.show_paths:
                print(f"MISSING-SOURCE\t{movie.source}")
            continue
        if not source_path.is_file() or source_path.is_symlink():
            counts["missing_source"] += 1
            if args.show_paths:
                print(f"UNSUPPORTED-SOURCE\t{movie.source}")
            continue

        nfo_path = source_path.with_suffix(".nfo")
        if nfo_path.exists() and not managed_nfo(nfo_path):
            counts["protected"] += 1
            if args.show_paths:
                print(f"PROTECTED-NFO\t{nfo_path.relative_to(library_root)}")
            continue

        tmdb_key = str(movie.tmdb_id)
        tmdb_record = tmdb_cache.get(tmdb_key)
        if tmdb_enabled and (args.refresh or not isinstance(tmdb_record, dict)):
            try:
                tmdb_record = fetch_tmdb_overview(movie.tmdb_id, movie.imdb_id, tmdb_token)
                tmdb_cache[tmdb_key] = tmdb_record
                cache_dirty = True
                request_count += 1
            except RemoteError as error:
                print(f"warning: TMDB description lookup disabled: {error}", file=sys.stderr)
                tmdb_enabled = False

        omdb_record = omdb_cache.get(movie.imdb_id)
        if omdb_enabled and (args.refresh or not isinstance(omdb_record, dict)):
            try:
                omdb_record, omdb_key_index = fetch_omdb_plot_with_fallback(
                    movie.imdb_id,
                    omdb_api_keys,
                    omdb_key_index,
                )
                omdb_cache[movie.imdb_id] = omdb_record
                cache_dirty = True
                request_count += 1
            except RemoteError as error:
                print(f"warning: OMDb plot lookup disabled: {error}", file=sys.stderr)
                omdb_enabled = False

        about = cached_text(tmdb_record, "overview")
        plot = cached_text(omdb_record, "plot")
        if about:
            counts["about"] += 1
        if plot and plot.casefold() != about.casefold():
            counts["plot"] += 1
        else:
            plot = ""

        if not about and not plot:
            counts["pending"] += 1
            if args.show_paths:
                print(f"PENDING-REMOTE\t{nfo_path.relative_to(library_root)}")
        elif args.dry_run:
            rendered = render_nfo(movie, about, plot)
            if (
                nfo_path.exists()
                and nfo_path.read_text(encoding="utf-8") == rendered
            ):
                counts["unchanged"] += 1
                status = "UNCHANGED"
            else:
                counts["pending"] += 1
                status = "WOULD-WRITE"
            if args.show_paths:
                print(f"{status}\t{nfo_path.relative_to(library_root)}")
        else:
            result = publish_nfo(nfo_path, render_nfo(movie, about, plot))
            counts[result] += 1
            if args.show_paths:
                print(f"{result.upper()}\t{nfo_path.relative_to(library_root)}")

        if request_count and request_count % 50 == 0:
            print(f"movie descriptions: completed {request_count} API requests", file=sys.stderr)
            if cache_dirty:
                save_cache(cache_path, cache)
                cache_dirty = False

    if cache_dirty and not args.dry_run:
        save_cache(cache_path, cache)

    for issue in index_issues:
        print(f"warning: {issue}", file=sys.stderr)
    mode = "Would process" if args.dry_run else "Processed"
    print(
        f"{mode} {counts['indexed']} indexed movies: "
        f"{counts['about']} About descriptions, {counts['plot']} full plots, "
        f"{counts['written']} written, {counts['unchanged']} unchanged, "
        f"{counts['protected']} protected NFOs, {counts['pending']} pending remote data, "
        f"and {counts['missing_source']} missing/unsupported sources."
    )
    if not omdb_api_keys:
        print(
            "OMDB_API_KEYS/OMDB_API_KEY is not set; full plots remain optional and only cached plots are used.",
            file=sys.stderr,
        )
    if not tmdb_token and not args.dry_run:
        print(
            "TMDB_API_TOKEN is not set; only cached About descriptions are used.",
            file=sys.stderr,
        )
    blocking_index_issue = any(
        "conflicting" in issue or "NFO collision" in issue
        for issue in index_issues
    )
    return 1 if counts["missing_source"] or blocking_index_issue else 0


if __name__ == "__main__":
    raise SystemExit(main())
