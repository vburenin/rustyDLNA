#!/usr/bin/env python3
"""Build a multi-genre symlink index for the movie library."""

from __future__ import annotations

import argparse
import concurrent.futures
import csv
import json
import os
import re
import sys
import urllib.request
from pathlib import Path

# Keep the generated media view free of interpreter cache artifacts.
sys.dont_write_bytecode = True

from catalog_config import MOVIE_SOURCES, catalog_item_label, catalog_movie_items
from imdb_index import (
    IMDB_GENRES,
    ensure_imdb_index,
    load_imdb_matches,
    normalized_title,
)
from intake_media import (
    MediaProbe,
    identity_from_matches,
    identity_hints,
    probe_media,
)
from paths import add_root_argument, require_library_root, state_dir
from tmdb_metadata import TmdbClient


IMDB_DATA_URL = "https://datasets.imdbws.com/title.basics.tsv.gz"
IMDB_AKAS_URL = "https://datasets.imdbws.com/title.akas.tsv.gz"
CUSTOM_GENRES = ("Anime", "Kids")
ALL_GENRES = IMDB_GENRES + CUSTOM_GENRES
GENRE_DIRECTORY = {genre: genre.casefold() for genre in ALL_GENRES}

# The catalog genre is used only as a safe fallback if IMDb has no exact
# title/year match. Anime and Kids are also retained as useful local genres.
SOURCES = MOVIE_SOURCES

YEAR_RE = re.compile(r"\(((?:18|19|20)\d{2})\)")
LEADING_NUMBER_RE = re.compile(r"^\d{1,3}\s*-\s*")


def movie_title_year(path: Path) -> tuple[str, int] | None:
    label = catalog_item_label(path)
    matches = list(YEAR_RE.finditer(label))
    if not matches:
        return None
    match = matches[-1]
    title = LEADING_NUMBER_RE.sub("", label[: match.start()].strip())
    return title, int(match.group(1))


def movie_identity(path: Path) -> tuple[str, int] | None:
    parsed = movie_title_year(path)
    if parsed is None:
        return None
    title, year = parsed
    return normalized_title(title), year


def movie_files(
    library_root: Path,
) -> list[tuple[Path, tuple[str, ...], Path]]:
    movies: list[tuple[Path, tuple[str, ...], Path]] = []
    for relative_source, fallback_genres in SOURCES.items():
        source = library_root / relative_source
        if not source.is_dir():
            print(f"warning: source directory is missing: {source}", file=sys.stderr)
            continue
        for path in catalog_movie_items(source):
            movies.append((path, fallback_genres, path.relative_to(source)))
    return sorted(movies, key=lambda item: str(item[0]).casefold())


def acquire_imdb_data(
    data_dir: Path, requested_path: Path | None, filename: str, url: str
) -> Path:
    if requested_path is not None:
        if not requested_path.is_file():
            raise FileNotFoundError(f"IMDb data file not found: {requested_path}")
        return requested_path

    cached = data_dir / filename
    if cached.is_file():
        return cached

    cached.parent.mkdir(parents=True, exist_ok=True)
    temporary = cached.with_suffix(cached.suffix + ".download")
    print(f"Downloading IMDb title data to {cached} ...", file=sys.stderr)
    try:
        urllib.request.urlretrieve(url, temporary)
        temporary.replace(cached)
    finally:
        if temporary.exists():
            temporary.unlink()
    return cached


def select_match(
    candidates: list[tuple[str, str, tuple[str, ...], int, int, str, int | None]],
    fallback_genres: tuple[str, ...],
) -> tuple[str, str, tuple[str, ...], str] | None:
    if not candidates:
        return None
    fallback = set(fallback_genres)
    ranked = sorted(
        candidates,
        key=lambda candidate: (
            -candidate[4],
            candidate[3],
            len(fallback.intersection(candidate[2])),
            len(candidate[2]),
        ),
        reverse=True,
    )
    best = ranked[0]
    return best[0], best[1], best[2], best[5]


def inspect_catalog_media(
    movies: list[tuple[Path, tuple[str, ...], Path]],
) -> tuple[dict[Path, MediaProbe], dict[Path, str]]:
    """FFprobe catalog files concurrently for safe identity disambiguation."""

    files = [movie for movie, _fallback, _relative in movies if movie.is_file()]
    probes: dict[Path, MediaProbe] = {}
    errors: dict[Path, str] = {}
    workers = min(8, os.cpu_count() or 4)
    print(
        f"Inspecting {len(files)} catalog movies with {workers} FFprobe workers...",
        file=sys.stderr,
    )
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
        futures = {executor.submit(probe_media, path): path for path in files}
        for completed, future in enumerate(
            concurrent.futures.as_completed(futures), start=1
        ):
            path = futures[future]
            try:
                probes[path] = future.result()
            except Exception as error:
                errors[path] = str(error)
            if completed % 100 == 0 or completed == len(files):
                print(
                    f"Inspected {completed}/{len(files)} catalog movies...",
                    file=sys.stderr,
                )
    return probes, errors


def empty_probe() -> MediaProbe:
    return MediaProbe("", 0, 0, 0, "", "", "")


def relative_link_target(link: Path, target: Path) -> str:
    return os.path.relpath(target, start=link.parent)


def indexed_link_paths(
    row: dict[str, str], genres_root: Path, library_root: Path
) -> list[Path]:
    """Return the exact generated links owned by an index row.

    Older indexes did not record link paths and always used a flat
    ``genre/movie-name`` layout. Keep that migration path so the first
    hierarchy-preserving rebuild removes the old generated links.
    """
    encoded_paths = row.get("generated_links", "")
    if not encoded_paths:
        movie = library_root / row["source"]
        return [
            genres_root / GENRE_DIRECTORY[genre] / movie.name
            for genre in row["genres"].split(",")
        ]

    relative_paths = json.loads(encoded_paths)
    if not isinstance(relative_paths, list) or not all(
        isinstance(path, str) for path in relative_paths
    ):
        raise RuntimeError("invalid generated_links value in genre index")

    links: list[Path] = []
    genre_directories = set(GENRE_DIRECTORY.values())
    for value in relative_paths:
        relative = Path(value)
        if (
            relative.is_absolute()
            or len(relative.parts) < 2
            or ".." in relative.parts
            or relative.parts[0] not in genre_directories
        ):
            raise RuntimeError(
                f"unsafe generated link path in genre index: {value!r}"
            )
        links.append(genres_root / relative)
    return links


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Rebuild genre symlinks from exact IMDb title/year matches."
    )
    parser.add_argument(
        "--imdb-data",
        type=Path,
        help="path to title.basics.tsv.gz (downloaded and cached if omitted)",
    )
    parser.add_argument(
        "--imdb-akas",
        type=Path,
        help="path to title.akas.tsv.gz (downloaded and cached if omitted)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="report the result without changing links or the index",
    )
    parser.add_argument(
        "--refresh-tmdb",
        action="store_true",
        help="refetch cached TMDB metadata (requires TMDB_API_TOKEN)",
    )
    add_root_argument(parser)
    args = parser.parse_args()
    if args.refresh_tmdb and not os.environ.get("TMDB_API_TOKEN"):
        parser.error("--refresh-tmdb requires TMDB_API_TOKEN")

    library_root = require_library_root(parser, args.root)
    caches = state_dir(library_root)
    caches.mkdir(parents=True, exist_ok=True)
    genres_root = library_root / "genres"
    movies = movie_files(library_root)
    imdb_data = acquire_imdb_data(
        caches, args.imdb_data, "title.basics.tsv.gz", IMDB_DATA_URL
    )
    imdb_akas = acquire_imdb_data(
        caches, args.imdb_akas, "title.akas.tsv.gz", IMDB_AKAS_URL
    )
    imdb_index = caches / "imdb-index.sqlite3"
    ensure_imdb_index(
        imdb_data,
        imdb_akas,
        imdb_index,
        allow_build=not args.dry_run,
    )
    tmdb = TmdbClient(
        caches / "tmdb-cache.json",
        os.environ.get("TMDB_API_TOKEN"),
        allow_fetch=not args.dry_run,
        refresh=args.refresh_tmdb and not args.dry_run,
    )
    probes, probe_errors = inspect_catalog_media(movies)
    hints_by_movie = {
        movie: identity_hints(movie, probes.get(movie, empty_probe()))
        for movie, _fallback, _relative in movies
    }
    identities = {
        (normalized_title(hint.title), hint.year)
        for hints in hints_by_movie.values()
        for hint in hints
    }
    imdb_matches = load_imdb_matches(imdb_index, identities)

    old_index = genres_root / "_genre-index.tsv"
    previous_ids: dict[str, str] = {}
    if old_index.is_file():
        with old_index.open(encoding="utf-8", newline="") as source:
            previous_ids = {
                row.get("source", ""): row.get("imdb_id", "")
                for row in csv.DictReader(source, delimiter="\t")
            }
    rows: list[tuple[str, ...]] = []
    desired: dict[Path, Path] = {}
    matched_count = 0
    tmdb_count = 0
    identity_corrections: list[tuple[Path, str, str]] = []

    for movie, fallback_genres, relative_in_source in movies:
        media_identity = identity_from_matches(
            hints_by_movie[movie],
            probes.get(movie, empty_probe()),
            imdb_matches,
            tmdb=tmdb,
        )
        if media_identity is not None:
            selected = (
                media_identity.imdb_id,
                media_identity.title,
                media_identity.genres,
                media_identity.match_method,
            )
        else:
            filename_identity = movie_identity(movie)
            selected = select_match(
                imdb_matches.get(filename_identity, [])
                if filename_identity
                else [],
                fallback_genres,
            )
        if selected:
            imdb_id, imdb_title, imdb_genres, match_method = selected
            genres = set(imdb_genres)
            matched_count += 1
        else:
            imdb_id, imdb_title = "", ""
            genres = set(fallback_genres)
            match_method = "catalog-fallback"

        relative_movie = movie.relative_to(library_root)
        previous_id = previous_ids.get(str(relative_movie), "")
        if previous_id and imdb_id and previous_id != imdb_id:
            identity_corrections.append((relative_movie, previous_id, imdb_id))

        parsed = movie_title_year(movie)
        tmdb_movie = (
            tmdb.lookup(imdb_id, parsed[0], parsed[1]) if parsed is not None else None
        )
        if tmdb_movie is not None:
            genres = set(tmdb_movie.genres)
            classification_source = "tmdb"
            tmdb_count += 1
        else:
            classification_source = "imdb" if selected else "catalog-fallback"

        # The catalog placement is intentional and remains authoritative.
        # TMDB or IMDb adds useful secondary genres rather than removing it.
        genres.update(fallback_genres)

        generated_links: list[str] = []
        for genre in sorted(genres):
            relative_link = Path(GENRE_DIRECTORY[genre]) / relative_in_source
            link = genres_root / relative_link
            previous = desired.get(link)
            if previous is not None and previous != movie:
                raise RuntimeError(
                    f"duplicate generated link path for {relative_link}: "
                    f"{previous} and {movie}"
                )
            desired[link] = movie
            generated_links.append(str(relative_link))
        rows.append(
            (
                str(relative_movie),
                ",".join(sorted(genres)),
                imdb_id,
                imdb_title,
                match_method,
                classification_source,
                tmdb_movie.match_method if tmdb_movie is not None else "",
                str(tmdb_movie.tmdb_id) if tmdb_movie is not None else "",
                tmdb_movie.title if tmdb_movie is not None else "",
                (
                    str(tmdb_movie.collection_id)
                    if tmdb_movie is not None
                    and tmdb_movie.collection_id is not None
                    else ""
                ),
                tmdb_movie.collection_name if tmdb_movie is not None else "",
                json.dumps(
                    generated_links, ensure_ascii=False, separators=(",", ":")
                ),
            )
        )

    if args.dry_run:
        for source, previous_id, imdb_id in identity_corrections:
            print(f"WOULD-CORRECT\t{source}\t{previous_id}\t{imdb_id}")
        for movie, error in sorted(
            probe_errors.items(), key=lambda item: str(item[0]).casefold()
        ):
            print(
                f"warning: filename-only classification for "
                f"{movie.relative_to(library_root)}: {error}",
                file=sys.stderr,
            )
        print(
            f"Would index {len(movies)} movies with {len(desired)} symlinks; "
            f"{tmdb_count} TMDB classifications, "
            f"{matched_count} exact IMDb matches, and "
            f"{len(movies) - matched_count} catalog fallbacks."
        )
        return 0

    tmdb.save()

    for genre in ALL_GENRES:
        (genres_root / GENRE_DIRECTORY[genre]).mkdir(exist_ok=True)

    # Reconcile only links recorded in the old generated index. Correct links
    # stay in place, which avoids needless churn and permits rebuilds when an
    # unchanged generated collection directory is not writable by this user.
    # Manually added links are intentionally left alone.
    old_owned: dict[Path, Path] = {}
    if old_index.is_file():
        with old_index.open(encoding="utf-8", newline="") as source:
            for row in csv.DictReader(source, delimiter="\t"):
                movie = library_root / row["source"]
                for link in indexed_link_paths(row, genres_root, library_root):
                    old_owned[link] = movie

    # Refuse manual collisions before removing any obsolete generated link.
    for link, movie in desired.items():
        if not (link.exists() or link.is_symlink()):
            continue
        if (
            link.is_symlink()
            and link.resolve(strict=False) == movie.resolve(strict=False)
        ):
            continue
        old_movie = old_owned.get(link)
        if not (
            old_movie is not None
            and link.is_symlink()
            and link.resolve(strict=False) == old_movie.resolve(strict=False)
        ):
            raise FileExistsError(f"refusing to replace existing path: {link}")

    old_link_parents: set[Path] = set()
    for link, old_movie in old_owned.items():
        new_movie = desired.get(link)
        if (
            new_movie is not None
            and link.is_symlink()
            and link.resolve(strict=False) == new_movie.resolve(strict=False)
        ):
            continue
        if (
            link.is_symlink()
            and link.resolve(strict=False) == old_movie.resolve(strict=False)
        ):
            link.unlink()
            old_link_parents.add(link.parent)

    # Remove collection directories left empty by the links just removed.
    # Genre roots and directories containing manual entries remain untouched.
    for directory in sorted(
        old_link_parents, key=lambda path: len(path.parts), reverse=True
    ):
        while directory.parent != genres_root:
            try:
                directory.rmdir()
            except OSError:
                break
            directory = directory.parent

    for link, movie in desired.items():
        link.parent.mkdir(parents=True, exist_ok=True)
        if link.exists() or link.is_symlink():
            if link.is_symlink() and link.resolve(strict=False) == movie.resolve():
                continue
            raise FileExistsError(f"refusing to replace existing path: {link}")
        link.symlink_to(relative_link_target(link, movie))

    with old_index.open("w", encoding="utf-8", newline="") as destination:
        writer = csv.writer(destination, delimiter="\t", lineterminator="\n")
        writer.writerow(
            (
                "source",
                "genres",
                "imdb_id",
                "imdb_title",
                "match_method",
                "classification_source",
                "tmdb_match_method",
                "tmdb_id",
                "tmdb_title",
                "tmdb_collection_id",
                "tmdb_collection",
                "generated_links",
            )
        )
        writer.writerows(rows)

    print(
        f"Indexed {len(movies)} movies with {len(desired)} symlinks; "
        f"{tmdb_count} TMDB classifications, "
        f"{matched_count} exact IMDb matches, and "
        f"{len(movies) - matched_count} catalog fallbacks."
    )
    for source, previous_id, imdb_id in identity_corrections:
        print(f"CORRECTED\t{source}\t{previous_id}\t{imdb_id}")
    for movie, error in sorted(
        probe_errors.items(), key=lambda item: str(item[0]).casefold()
    ):
        print(
            f"warning: filename-only classification for "
            f"{movie.relative_to(library_root)}: {error}",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
