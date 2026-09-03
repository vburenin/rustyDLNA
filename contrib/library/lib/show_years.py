"""Cache exact episode release years for selected IMDb series."""

from __future__ import annotations

import csv
import gzip
import json
import sys
from pathlib import Path


CACHE_VERSION = 1


def file_signature(path: Path) -> dict[str, int]:
    stat = path.stat()
    return {"size": stat.st_size, "mtime_ns": stat.st_mtime_ns}


def load_cache(
    cache_path: Path,
    basics_path: Path,
    episodes_path: Path,
) -> dict[str, list[int]]:
    if not cache_path.is_file():
        return {}
    try:
        payload = json.loads(cache_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    if (
        not isinstance(payload, dict)
        or payload.get("version") != CACHE_VERSION
        or payload.get("title_basics") != file_signature(basics_path)
        or payload.get("title_episode") != file_signature(episodes_path)
        or not isinstance(payload.get("entries"), dict)
    ):
        return {}
    entries: dict[str, list[int]] = {}
    for imdb_id, years in payload["entries"].items():
        if isinstance(imdb_id, str) and isinstance(years, list) and all(
            isinstance(year, int) for year in years
        ):
            entries[imdb_id] = years
    return entries


def save_cache(
    cache_path: Path,
    basics_path: Path,
    episodes_path: Path,
    entries: dict[str, list[int]],
) -> None:
    payload = {
        "version": CACHE_VERSION,
        "title_basics": file_signature(basics_path),
        "title_episode": file_signature(episodes_path),
        "entries": entries,
    }
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = cache_path.with_suffix(cache_path.suffix + ".new")
    temporary.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    temporary.replace(cache_path)


def load_show_release_years(
    basics_path: Path,
    episodes_path: Path,
    cache_path: Path,
    imdb_ids: set[str],
    *,
    allow_build: bool,
) -> dict[str, tuple[int, ...]]:
    if not episodes_path.is_file():
        print(
            f"warning: IMDb episode data is missing: {episodes_path}; "
            "using series spans for show year links",
            file=sys.stderr,
        )
        return {}

    entries = load_cache(cache_path, basics_path, episodes_path)
    missing = imdb_ids.difference(entries)
    if missing and allow_build:
        print(
            f"Building exact release-year cache for {len(missing)} IMDb series ...",
            file=sys.stderr,
        )
        episode_parents: dict[str, str] = {}
        with gzip.open(
            episodes_path, "rt", encoding="utf-8", newline=""
        ) as source:
            for row in csv.DictReader(source, delimiter="\t"):
                parent = row["parentTconst"]
                if parent in missing:
                    episode_parents[row["tconst"]] = parent

        found: dict[str, set[int]] = {imdb_id: set() for imdb_id in missing}
        with gzip.open(
            basics_path, "rt", encoding="utf-8", newline=""
        ) as source:
            for row in csv.DictReader(source, delimiter="\t"):
                parent = episode_parents.get(row["tconst"])
                if parent is None or row["startYear"] == r"\N":
                    continue
                found[parent].add(int(row["startYear"]))

        for imdb_id, years in found.items():
            entries[imdb_id] = sorted(years)
        save_cache(cache_path, basics_path, episodes_path, entries)
    elif missing:
        print(
            f"warning: exact show-year cache is missing {len(missing)} series; "
            "using series spans for those shows during dry run",
            file=sys.stderr,
        )

    return {
        imdb_id: tuple(entries[imdb_id])
        for imdb_id in imdb_ids
        if imdb_id in entries and entries[imdb_id]
    }
