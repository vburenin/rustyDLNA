#!/usr/bin/env python3
"""Persistent SQLite lookup index for IMDb's public title datasets."""

from __future__ import annotations

import argparse
import csv
import gzip
import json
import os
import re
import sqlite3
import sys
import unicodedata
from collections import defaultdict
from pathlib import Path


# Keep the generated media view free of interpreter cache artifacts.
sys.dont_write_bytecode = True

INDEX_SCHEMA_VERSION = 3
TITLE_NORMALIZATION_VERSION = 1
MOVIE_TYPE_PRIORITY = {"movie": 4, "tvMovie": 3, "video": 2, "short": 1}
SERIES_TYPE_PRIORITY = {"tvSeries": 2, "tvMiniSeries": 1}
TYPE_PRIORITY = MOVIE_TYPE_PRIORITY | SERIES_TYPE_PRIORITY
IMDB_GENRES = (
    "Action",
    "Adult",
    "Adventure",
    "Animation",
    "Biography",
    "Comedy",
    "Crime",
    "Documentary",
    "Drama",
    "Family",
    "Fantasy",
    "Film-Noir",
    "Game-Show",
    "History",
    "Horror",
    "Music",
    "Musical",
    "Mystery",
    "News",
    "Reality-TV",
    "Romance",
    "Sci-Fi",
    "Short",
    "Sport",
    "Talk-Show",
    "Thriller",
    "War",
    "Western",
)
WORD_RE = re.compile(r"[^\W_]+", flags=re.UNICODE)

Candidate = tuple[str, str, tuple[str, ...], int, int, str, int | None]
SeriesCandidate = tuple[str, str, int, int | None, int, str]


def normalized_title(value: str) -> str:
    value = value.replace("&", " and ")
    value = unicodedata.normalize("NFKD", value).casefold()
    value = value.replace("’", "").replace("'", "")
    return " ".join(WORD_RE.findall(value))


def source_signature(data_path: Path, akas_path: Path) -> dict[str, object]:
    def file_signature(path: Path) -> dict[str, object]:
        stat = path.stat()
        return {
            "path": str(path.resolve()),
            "size": stat.st_size,
            "mtime_ns": stat.st_mtime_ns,
        }

    return {
        "schema_version": INDEX_SCHEMA_VERSION,
        "normalization_version": TITLE_NORMALIZATION_VERSION,
        "movie_title_types": MOVIE_TYPE_PRIORITY,
        "series_title_types": SERIES_TYPE_PRIORITY,
        "genres": list(IMDB_GENRES),
        "title_basics": file_signature(data_path),
        "title_akas": file_signature(akas_path),
    }


def _read_index_signature(index_path: Path) -> dict[str, object] | None:
    if not index_path.is_file():
        return None
    try:
        with sqlite3.connect(
            f"file:{index_path.resolve()}?mode=ro", uri=True
        ) as connection:
            row = connection.execute(
                "SELECT value FROM metadata WHERE key = 'source_signature'"
            ).fetchone()
            if row is None:
                return None
            value = json.loads(row[0])
            return value if isinstance(value, dict) else None
    except (json.JSONDecodeError, OSError, sqlite3.DatabaseError):
        return None


def index_is_current(
    index_path: Path, data_path: Path, akas_path: Path
) -> bool:
    return _read_index_signature(index_path) == source_signature(
        data_path, akas_path
    )


def _title_number(value: str) -> int:
    if not value.startswith("tt"):
        raise ValueError(f"invalid IMDb title identifier: {value!r}")
    return int(value[2:])


def _imdb_id(value: int) -> str:
    return f"tt{value:07d}"


def build_imdb_index(
    data_path: Path,
    akas_path: Path,
    index_path: Path,
) -> None:
    """Build a complete relevant-title index and atomically install it."""

    initial_signature = source_signature(data_path, akas_path)
    index_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = index_path.with_name(
        f".{index_path.name}.{os.getpid()}.new"
    )
    if temporary.exists():
        temporary.unlink()

    allowed = set(IMDB_GENRES)
    eligible_ids: set[int] = set()
    basics_rows = 0
    title_rows = 0
    aka_rows = 0

    print(
        f"Building persistent IMDb index at {index_path} ...",
        file=sys.stderr,
    )
    try:
        connection = sqlite3.connect(temporary)
        try:
            connection.executescript(
                """
                PRAGMA page_size = 32768;
                PRAGMA journal_mode = OFF;
                PRAGMA synchronous = OFF;
                PRAGMA locking_mode = EXCLUSIVE;
                PRAGMA temp_store = FILE;
                PRAGMA cache_size = -524288;

                CREATE TABLE metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                ) WITHOUT ROWID;

                CREATE TABLE titles (
                    title_id INTEGER PRIMARY KEY,
                    primary_title TEXT NOT NULL,
                    title_type TEXT NOT NULL,
                    start_year INTEGER NOT NULL,
                    end_year INTEGER,
                    genres TEXT NOT NULL,
                    type_priority INTEGER NOT NULL,
                    runtime_minutes INTEGER
                );

                CREATE TABLE names (
                    normalized_title TEXT NOT NULL,
                    title_id INTEGER NOT NULL,
                    match_method INTEGER NOT NULL,
                    PRIMARY KEY (normalized_title, title_id)
                ) WITHOUT ROWID;
                """
            )

            connection.execute("BEGIN")
            title_batch: list[
                tuple[int, str, str, int, int | None, str, int, int | None]
            ] = []
            name_batch: list[tuple[str, int, int]] = []
            with gzip.open(
                data_path, "rt", encoding="utf-8", newline=""
            ) as source:
                for row in csv.DictReader(source, delimiter="\t"):
                    basics_rows += 1
                    if basics_rows % 2_000_000 == 0:
                        print(
                            f"  title.basics: {basics_rows:,} rows scanned, "
                            f"{title_rows:,} indexed",
                            file=sys.stderr,
                        )
                    priority = TYPE_PRIORITY.get(row["titleType"])
                    if priority is None or row["startYear"] == r"\N":
                        continue
                    genres = tuple(
                        genre
                        for genre in row["genres"].split(",")
                        if genre in allowed
                    )
                    if (
                        row["titleType"] in MOVIE_TYPE_PRIORITY
                        and not genres
                    ):
                        continue

                    title_id = _title_number(row["tconst"])
                    primary_title = row["primaryTitle"]
                    end_year = (
                        None
                        if row["endYear"] == r"\N"
                        else int(row["endYear"])
                    )
                    title_batch.append(
                        (
                            title_id,
                            primary_title,
                            row["titleType"],
                            int(row["startYear"]),
                            end_year,
                            ",".join(genres),
                            priority,
                            (
                                None
                                if row["runtimeMinutes"] == r"\N"
                                else int(row["runtimeMinutes"])
                            ),
                        )
                    )
                    eligible_ids.add(title_id)
                    title_rows += 1

                    normalized_names = {
                        normalized_title(primary_title),
                        normalized_title(row["originalTitle"]),
                    }
                    name_batch.extend(
                        (name, title_id, 0)
                        for name in normalized_names
                        if name
                    )
                    if len(title_batch) >= 50_000:
                        connection.executemany(
                            "INSERT INTO titles VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                            title_batch,
                        )
                        connection.executemany(
                            "INSERT OR IGNORE INTO names VALUES (?, ?, ?)",
                            name_batch,
                        )
                        title_batch.clear()
                        name_batch.clear()

            if title_batch:
                connection.executemany(
                    "INSERT INTO titles VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    title_batch,
                )
                connection.executemany(
                    "INSERT OR IGNORE INTO names VALUES (?, ?, ?)", name_batch
                )

            print(
                f"  title.basics complete: {basics_rows:,} rows scanned, "
                f"{title_rows:,} relevant titles indexed",
                file=sys.stderr,
            )

            alias_batch: list[tuple[str, int, int]] = []
            with gzip.open(
                akas_path, "rt", encoding="utf-8", newline=""
            ) as source:
                next(source, None)
                for line_number, line in enumerate(source, start=1):
                    if line_number % 5_000_000 == 0:
                        print(
                            f"  title.akas: {line_number:,} rows scanned, "
                            f"{aka_rows:,} relevant aliases indexed",
                            file=sys.stderr,
                        )
                    fields = line.rstrip("\n").split("\t", 3)
                    if len(fields) < 3:
                        continue
                    title_id = _title_number(fields[0])
                    if title_id not in eligible_ids:
                        continue
                    name = normalized_title(fields[2])
                    if not name:
                        continue
                    alias_batch.append((name, title_id, 1))
                    aka_rows += 1
                    if len(alias_batch) >= 100_000:
                        connection.executemany(
                            "INSERT OR IGNORE INTO names VALUES (?, ?, ?)",
                            alias_batch,
                        )
                        alias_batch.clear()

            if alias_batch:
                connection.executemany(
                    "INSERT OR IGNORE INTO names VALUES (?, ?, ?)",
                    alias_batch,
                )
            final_signature = source_signature(data_path, akas_path)
            if final_signature != initial_signature:
                raise RuntimeError(
                    "IMDb datasets changed while the index was being built"
                )
            connection.execute(
                "INSERT INTO metadata VALUES ('source_signature', ?)",
                (
                    json.dumps(
                        final_signature,
                        ensure_ascii=False,
                        sort_keys=True,
                        separators=(",", ":"),
                    ),
                ),
            )
            connection.commit()
            check = connection.execute("PRAGMA quick_check").fetchone()
            if check != ("ok",):
                raise RuntimeError(f"IMDb index quick check failed: {check!r}")
            connection.execute("PRAGMA optimize")
        finally:
            connection.close()

        temporary.replace(index_path)
    finally:
        if temporary.exists():
            temporary.unlink()

    print(
        f"IMDb index ready: {title_rows:,} titles, "
        f"{aka_rows:,} relevant alternate-title rows "
        f"({index_path.stat().st_size / (1024 ** 3):.2f} GiB).",
        file=sys.stderr,
    )


def ensure_imdb_index(
    data_path: Path,
    akas_path: Path,
    index_path: Path,
    *,
    allow_build: bool,
) -> None:
    if index_is_current(index_path, data_path, akas_path):
        print(f"Using persistent IMDb index from {index_path}", file=sys.stderr)
        return
    if not allow_build:
        raise RuntimeError(
            f"IMDb index is missing or stale: {index_path}; "
            "run a non-dry genre build first"
        )
    build_imdb_index(data_path, akas_path, index_path)


def load_imdb_matches(
    index_path: Path,
    identities: set[tuple[str, int]],
) -> dict[tuple[str, int], list[Candidate]]:
    matches: dict[tuple[str, int], list[Candidate]] = defaultdict(list)
    query = """
        SELECT
            titles.title_id,
            titles.primary_title,
            titles.genres,
            titles.type_priority,
            ABS(titles.start_year - ?),
            names.match_method
            , titles.runtime_minutes
        FROM names
        JOIN titles USING (title_id)
        WHERE names.normalized_title = ?
          AND titles.start_year BETWEEN ? AND ?
          AND titles.title_type IN ('movie', 'tvMovie', 'video', 'short')
        ORDER BY printf('tt%07d', titles.title_id)
    """
    with sqlite3.connect(
        f"file:{index_path.resolve()}?mode=ro", uri=True
    ) as connection:
        connection.execute("PRAGMA query_only = ON")
        connection.execute("PRAGMA mmap_size = 268435456")
        for title, year in identities:
            candidates: list[Candidate] = []
            for row in connection.execute(
                query, (year, title, year - 1, year + 1)
            ):
                candidates.append(
                    (
                        _imdb_id(int(row[0])),
                        str(row[1]),
                        tuple(str(row[2]).split(",")),
                        int(row[3]),
                        int(row[4]),
                        (
                            "imdb-title-year"
                            if int(row[5]) == 0
                            else "imdb-alternate-title-year"
                        ),
                        int(row[6]) if row[6] is not None else None,
                    )
                )
            if candidates:
                matches[(title, year)] = candidates
    return matches


def load_imdb_series_matches(
    index_path: Path,
    titles: set[str],
) -> dict[str, list[SeriesCandidate]]:
    """Return exact normalized-name matches for top-level show titles."""

    matches: dict[str, list[SeriesCandidate]] = defaultdict(list)
    query = """
        SELECT
            titles.title_id,
            titles.primary_title,
            titles.start_year,
            titles.end_year,
            titles.type_priority,
            names.match_method
        FROM names
        JOIN titles USING (title_id)
        WHERE names.normalized_title = ?
          AND titles.title_type IN ('tvSeries', 'tvMiniSeries')
        ORDER BY printf('tt%07d', titles.title_id)
    """
    with sqlite3.connect(
        f"file:{index_path.resolve()}?mode=ro", uri=True
    ) as connection:
        connection.execute("PRAGMA query_only = ON")
        connection.execute("PRAGMA mmap_size = 268435456")
        for title in titles:
            candidates: list[SeriesCandidate] = []
            for row in connection.execute(query, (title,)):
                candidates.append(
                    (
                        _imdb_id(int(row[0])),
                        str(row[1]),
                        int(row[2]),
                        int(row[3]) if row[3] is not None else None,
                        int(row[4]),
                        (
                            "imdb-series-title"
                            if int(row[5]) == 0
                            else "imdb-series-alternate-title"
                        ),
                    )
                )
            if candidates:
                matches[title] = candidates
    return matches


def main() -> int:
    from paths import add_root_argument, require_library_root, state_dir

    parser = argparse.ArgumentParser(
        description="Build the persistent SQLite lookup index for IMDb data."
    )
    add_root_argument(parser)
    parser.add_argument(
        "--imdb-data",
        type=Path,
        help="path to title.basics.tsv.gz",
    )
    parser.add_argument(
        "--imdb-akas",
        type=Path,
        help="path to title.akas.tsv.gz",
    )
    parser.add_argument(
        "--index",
        type=Path,
        help="destination SQLite index",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="rebuild even when the existing index is current",
    )
    args = parser.parse_args()
    library_root = require_library_root(parser, args.root)
    data_dir = state_dir(library_root)
    imdb_data = args.imdb_data or data_dir / "title.basics.tsv.gz"
    imdb_akas = args.imdb_akas or data_dir / "title.akas.tsv.gz"
    index_path = args.index or data_dir / "imdb-index.sqlite3"

    for path in (imdb_data, imdb_akas):
        if not path.is_file():
            parser.error(f"IMDb data file not found: {path}")
    if args.force:
        build_imdb_index(imdb_data, imdb_akas, index_path)
    else:
        ensure_imdb_index(
            imdb_data,
            imdb_akas,
            index_path,
            allow_build=True,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
