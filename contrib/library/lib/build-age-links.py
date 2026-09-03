#!/usr/bin/env python3
"""Build conservative exact and cumulative age-guidance media views."""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# Keep the generated media view free of interpreter cache artifacts.
sys.dont_write_bytecode = True

from age_ratings import (
    AgeRating,
    TmdbAgeClient,
    WikidataAgeClient,
    category_for_minimum_age,
    rating_severity,
)
from catalog_config import (
    AGE_RATING_REGION,
    MOVIE_SOURCES,
    SHOW_SOURCES,
    catalog_item_label,
    catalog_movie_items,
)
from paths import add_root_argument, require_library_root, state_dir


EXACT_AGE_VIEW = "BY_AGE"
CUMULATIVE_AGE_VIEW = "UNTIL_AGE"
RATING_VIEW = "BY_RATING"
MINIMUM_SUPPORTED_AGE = 0
MAXIMUM_SUPPORTED_AGE = 18
MINIMUM_CUMULATIVE_VIEWER_AGE = 1
PARENTAL_GUIDANCE_CUMULATIVE_AGE = 13
FOREIGN_CERTIFICATION_CUMULATIVE_FLOOR = 13
EXACT_CATEGORY_RE = re.compile(
    r"^(?:ALL_AGES|PARENTAL_GUIDANCE|AGE_\d{2}_PLUS|UNRATED)$"
)
# Keep accepting legacy 00_YEARS index entries so a rebuild can remove them.
CUMULATIVE_CATEGORY_RE = re.compile(r"^(?:0\d|1[0-8])_YEARS$")
RATING_CATEGORY_RE = re.compile(r"^[A-Z0-9][A-Z0-9+._-]*$")


@dataclass(frozen=True)
class ReviewedOverride:
    rating: AgeRating
    note: str


def relative_link_target(link: Path, target: Path) -> str:
    return os.path.relpath(target, start=link.parent)


def movie_items(library_root: Path) -> list[tuple[Path, Path]]:
    items: list[tuple[Path, Path]] = []
    for relative_source in MOVIE_SOURCES:
        source = library_root / relative_source
        if not source.is_dir():
            print(f"warning: source directory is missing: {source}", file=sys.stderr)
            continue
        for path in catalog_movie_items(source):
            items.append((path, path.relative_to(source)))
    return sorted(items, key=lambda item: str(item[0]).casefold())


def show_items(library_root: Path) -> list[Path]:
    items: list[Path] = []
    for relative_source in SHOW_SOURCES:
        source = library_root / relative_source
        if not source.is_dir():
            print(f"warning: show source directory is missing: {source}", file=sys.stderr)
            continue
        items.extend(
            path
            for path in source.iterdir()
            if path.is_dir() and not path.is_symlink()
        )
    return sorted(items, key=lambda path: str(path).casefold())


def safe_relative_source(value: str) -> Path:
    path = Path(value)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise RuntimeError(f"unsafe source path in age metadata: {value!r}")
    return path


def load_reviewed_overrides(path: Path) -> dict[str, ReviewedOverride]:
    overrides: dict[str, ReviewedOverride] = {}
    if not path.is_file():
        return overrides
    with path.open(encoding="utf-8", newline="") as source:
        for line_number, row in enumerate(
            csv.DictReader(source, delimiter="\t"), start=2
        ):
            relative_source = str(safe_relative_source(row.get("source", "")))
            if relative_source in overrides:
                raise RuntimeError(
                    f"duplicate reviewed age override on line {line_number}: "
                    f"{relative_source}"
                )
            try:
                minimum_age = int(row.get("minimum_age", ""))
                category = category_for_minimum_age(minimum_age)
            except ValueError as error:
                raise RuntimeError(
                    f"invalid minimum_age on line {line_number}: {error}"
                ) from error
            overrides[relative_source] = ReviewedOverride(
                rating=AgeRating(
                    category=category,
                    minimum_age=minimum_age,
                    region="REVIEWED",
                    certification=f"minimum-age-{minimum_age}",
                    source="reviewed-override",
                    tmdb_id=None,
                    match_method="exact-catalog-path",
                ),
                note=row.get("note", "").strip(),
            )
    return overrides


def load_movie_metadata(path: Path) -> dict[str, tuple[str, int | None]]:
    metadata: dict[str, tuple[str, int | None]] = {}
    if not path.is_file():
        return metadata
    with path.open(encoding="utf-8", newline="") as source:
        for row in csv.DictReader(source, delimiter="\t"):
            tmdb_id = row.get("tmdb_id", "")
            metadata[row["source"]] = (
                row.get("imdb_id", ""),
                int(tmdb_id) if tmdb_id.isdigit() else None,
            )
    return metadata


def load_show_metadata(path: Path) -> dict[str, tuple[str, ...]]:
    metadata: dict[str, tuple[str, ...]] = {}
    if not path.is_file():
        return metadata
    with path.open(encoding="utf-8", newline="") as source:
        for row in csv.DictReader(source, delimiter="\t"):
            if row.get("kind") != "show":
                continue
            metadata[row["source"]] = tuple(
                value
                for value in row.get("imdb_id", "").split(",")
                if value
            )
    return metadata


def unrated() -> AgeRating:
    return AgeRating(
        category="UNRATED",
        minimum_age=None,
        region="",
        certification="",
        source="unrated",
        tmdb_id=None,
        match_method="no-trustworthy-rating",
    )


def strictest_rating(ratings: list[AgeRating]) -> AgeRating | None:
    return max(ratings, key=rating_severity) if ratings else None


def assert_safe_link_parent(link: Path, genres_root: Path) -> None:
    """Refuse to traverse symlinks or non-directories below the view root."""

    relative = link.relative_to(genres_root)
    current = genres_root
    for part in relative.parts[:-1]:
        current /= part
        if current.is_symlink():
            raise RuntimeError(
                f"refusing generated age-link path beneath symlink: {current}"
            )
        if current.exists() and not current.is_dir():
            raise RuntimeError(
                f"refusing generated age-link path beneath non-directory: "
                f"{current}"
            )


def indexed_age_links(row: dict[str, str], genres_root: Path) -> list[Path]:
    encoded_paths = row.get("generated_links", "")
    relative_paths = json.loads(encoded_paths)
    if not isinstance(relative_paths, list) or not all(
        isinstance(path, str) for path in relative_paths
    ):
        raise RuntimeError("invalid generated_links value in age index")

    links: list[Path] = []
    for value in relative_paths:
        relative = Path(value)
        if relative.parts:
            category_pattern = {
                EXACT_AGE_VIEW: EXACT_CATEGORY_RE,
                CUMULATIVE_AGE_VIEW: CUMULATIVE_CATEGORY_RE,
                RATING_VIEW: RATING_CATEGORY_RE,
            }.get(relative.parts[0])
        else:
            category_pattern = None
        if (
            relative.is_absolute()
            or len(relative.parts) < 4
            or ".." in relative.parts
            or category_pattern is None
            or not category_pattern.fullmatch(relative.parts[1])
            or relative.parts[2] not in {"Movies", "Shows"}
        ):
            raise RuntimeError(
                f"unsafe generated age-link path in age index: {value!r}"
            )
        link = genres_root / relative
        assert_safe_link_parent(link, genres_root)
        links.append(link)
    return links


def load_owned_links(
    index_path: Path, genres_root: Path, library_root: Path
) -> dict[Path, Path]:
    owned: dict[Path, Path] = {}
    if not index_path.is_file():
        return owned
    with index_path.open(encoding="utf-8", newline="") as source:
        for row in csv.DictReader(source, delimiter="\t"):
            target = library_root / safe_relative_source(row["source"])
            for link in indexed_age_links(row, genres_root):
                owned[link] = target
    return owned


def add_desired_link(
    desired: dict[Path, Path], link: Path, target: Path
) -> None:
    previous = desired.get(link)
    if previous is not None and previous != target:
        raise RuntimeError(
            f"duplicate generated age-link path for {link}: "
            f"{previous} and {target}"
        )
    desired[link] = target


def cumulative_age_categories(minimum_age: int | None) -> tuple[str, ...]:
    """Return every supported viewer age for a numeric minimum age."""

    if minimum_age is None:
        return ()
    if not MINIMUM_SUPPORTED_AGE <= minimum_age <= MAXIMUM_SUPPORTED_AGE:
        raise ValueError(
            f"minimum age must be between {MINIMUM_SUPPORTED_AGE} and "
            f"{MAXIMUM_SUPPORTED_AGE}: {minimum_age}"
        )
    first_viewer_age = max(minimum_age, MINIMUM_CUMULATIVE_VIEWER_AGE)
    return tuple(
        f"{viewer_age:02d}_YEARS"
        for viewer_age in range(first_viewer_age, MAXIMUM_SUPPORTED_AGE + 1)
    )


def cumulative_minimum_age(
    exact_category: str,
    minimum_age: int | None,
    *,
    rating_region: str = "",
    rating_source: str = "",
) -> int | None:
    """Map exact categories to a conservative cumulative-view threshold."""

    # An unrestricted-admission certification is not an editorial claim that
    # the content is suitable from birth.  Keep it out of the numeric view
    # unless a reviewed override has converted it to an actual age category.
    if exact_category == "ALL_AGES":
        return None
    if rating_source == "reviewed-override":
        return minimum_age
    if exact_category == "PARENTAL_GUIDANCE":
        return PARENTAL_GUIDANCE_CUMULATIVE_AGE
    if minimum_age is None:
        return None
    if rating_region and rating_region != "US":
        return max(minimum_age, FOREIGN_CERTIFICATION_CUMULATIVE_FLOOR)
    return minimum_age


def generated_age_links(
    exact_category: str,
    minimum_age: int | None,
    media_kind: str,
    relative_item: Path,
    *,
    rating_region: str = "",
    rating_source: str = "",
) -> tuple[Path, ...]:
    """Build exact-age plus cumulative-age link paths for one item."""

    links = [
        Path(EXACT_AGE_VIEW) / exact_category / media_kind / relative_item
    ]
    links.extend(
        Path(CUMULATIVE_AGE_VIEW) / category / media_kind / relative_item
        for category in cumulative_age_categories(
            cumulative_minimum_age(
                exact_category,
                minimum_age,
                rating_region=rating_region,
                rating_source=rating_source,
            )
        )
    )
    return tuple(links)


def certification_category(certification: str) -> str:
    """Return a stable, path-safe category for an exact certification."""

    if not certification.strip():
        return "UNRATED"
    category = re.sub(
        r"[^A-Z0-9+._-]+", "_", certification.strip().upper()
    ).strip("._-")
    if not category or not RATING_CATEGORY_RE.fullmatch(category):
        raise ValueError(f"unsafe certification category: {certification!r}")
    return category


def generated_rating_link(
    certification: str, media_kind: str, relative_item: Path
) -> Path:
    return (
        Path(RATING_VIEW)
        / certification_category(certification)
        / media_kind
        / relative_item
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Rebuild conservative BY_AGE, UNTIL_AGE, and BY_RATING links "
            "for movies and shows."
        )
    )
    add_root_argument(parser)
    parser.add_argument(
        "--overrides",
        type=Path,
        help="reviewed minimum-age override TSV",
    )
    parser.add_argument(
        "--region",
        default=os.environ.get("AGE_RATING_REGION", AGE_RATING_REGION),
        help="TMDB certification region (default: %(default)s)",
    )
    parser.add_argument(
        "--refresh-tmdb",
        action="store_true",
        help="refetch cached TMDB certifications (requires TMDB_API_TOKEN)",
    )
    parser.add_argument(
        "--refresh-wikidata",
        action="store_true",
        help="refetch cached Wikidata certifications",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="report the result without changing links, cache, or index",
    )
    args = parser.parse_args()
    library_root = require_library_root(parser, args.root)
    genres_root = library_root / "genres"
    data_dir = state_dir(library_root)
    overrides_path = args.overrides or data_dir / "age-overrides.tsv"
    if args.refresh_tmdb and not os.environ.get("TMDB_API_TOKEN"):
        parser.error("--refresh-tmdb requires TMDB_API_TOKEN")
    if args.region.strip().upper() != "US":
        print(
            f"warning: TMDB certification mapping currently supports only "
            f"US; Wikidata will use region "
            f"{args.region.strip().upper()!r} and its fallback order",
            file=sys.stderr,
        )

    movies = movie_items(library_root)
    shows = show_items(library_root)
    overrides = load_reviewed_overrides(overrides_path)
    movie_metadata = load_movie_metadata(genres_root / "_genre-index.tsv")
    show_metadata = load_show_metadata(genres_root / "_year-index.tsv")
    tmdb = TmdbAgeClient(
        data_dir / "age-ratings-cache.json",
        os.environ.get("TMDB_API_TOKEN"),
        args.region,
        allow_fetch=not args.dry_run,
        refresh=args.refresh_tmdb and not args.dry_run,
    )
    wikidata = WikidataAgeClient(
        data_dir / "wikidata-age-ratings-cache.json",
        args.region,
        allow_fetch=not args.dry_run,
        refresh=args.refresh_wikidata and not args.dry_run,
    )
    wanted_wikidata_ids = {
        imdb_id
        for imdb_id, _ in movie_metadata.values()
        if imdb_id
    }
    wanted_wikidata_ids.update(
        imdb_id
        for imdb_ids in show_metadata.values()
        for imdb_id in imdb_ids
    )
    wikidata_ratings = wikidata.lookup_many(wanted_wikidata_ids)

    desired: dict[Path, Path] = {}
    rows: list[tuple[str, ...]] = []
    used_overrides: set[str] = set()
    override_count = 0
    tmdb_count = 0
    wikidata_count = 0
    unrated_count = 0
    exact_link_count = 0
    cumulative_link_count = 0
    rating_link_count = 0
    cumulative_adjustment_count = 0

    for movie, relative_in_source in movies:
        relative_source = str(movie.relative_to(library_root))
        reviewed = overrides.get(relative_source)
        imdb_id, known_tmdb_id = movie_metadata.get(
            relative_source, ("", None)
        )
        note = ""
        if reviewed is not None:
            rating = reviewed.rating
            note = reviewed.note
            used_overrides.add(relative_source)
            override_count += 1
        else:
            candidates: list[AgeRating] = []
            if imdb_id:
                tmdb_rating = tmdb.lookup(
                    "movie", imdb_id, known_tmdb_id
                )
                if tmdb_rating is not None:
                    candidates.append(tmdb_rating)
                wikidata_rating = wikidata_ratings.get(imdb_id)
                if wikidata_rating is not None:
                    candidates.append(wikidata_rating)
            rating = strictest_rating(candidates) or unrated()
            if rating.source == "tmdb-certification":
                tmdb_count += 1
            elif rating.source == "wikidata-certification":
                wikidata_count += 1
            else:
                unrated_count += 1

        cumulative_age = cumulative_minimum_age(
            rating.category,
            rating.minimum_age,
            rating_region=rating.region,
            rating_source=rating.source,
        )
        if cumulative_age != rating.minimum_age:
            cumulative_adjustment_count += 1
        relative_links = (
            *generated_age_links(
                rating.category,
                rating.minimum_age,
                "Movies",
                relative_in_source,
                rating_region=rating.region,
                rating_source=rating.source,
            ),
            generated_rating_link(
                rating.certification, "Movies", relative_in_source
            ),
        )
        for relative_link in relative_links:
            add_desired_link(desired, genres_root / relative_link, movie)
        exact_link_count += 1
        cumulative_link_count += len(relative_links) - 2
        rating_link_count += 1
        rows.append(
            (
                "movie",
                relative_source,
                catalog_item_label(movie),
                rating.category,
                (
                    str(rating.minimum_age)
                    if rating.minimum_age is not None
                    else ""
                ),
                str(cumulative_age) if cumulative_age is not None else "",
                rating.region,
                rating.certification,
                rating.source,
                imdb_id,
                str(rating.tmdb_id or known_tmdb_id or ""),
                rating.match_method,
                note,
                json.dumps(
                    [str(relative_link) for relative_link in relative_links],
                    ensure_ascii=False,
                    separators=(",", ":"),
                ),
            )
        )

    for show in shows:
        relative_source = str(show.relative_to(library_root))
        reviewed = overrides.get(relative_source)
        imdb_ids = show_metadata.get(relative_source, ())
        note = ""
        if reviewed is not None:
            rating = reviewed.rating
            note = reviewed.note
            used_overrides.add(relative_source)
            override_count += 1
        else:
            candidates = []
            for imdb_id in imdb_ids:
                tmdb_rating = tmdb.lookup("show", imdb_id)
                if tmdb_rating is not None:
                    candidates.append(tmdb_rating)
                wikidata_rating = wikidata_ratings.get(imdb_id)
                if wikidata_rating is not None:
                    candidates.append(wikidata_rating)
            rating = strictest_rating(candidates) or unrated()
            if rating.source == "tmdb-certification":
                tmdb_count += 1
            elif rating.source == "wikidata-certification":
                wikidata_count += 1
            else:
                unrated_count += 1

        cumulative_age = cumulative_minimum_age(
            rating.category,
            rating.minimum_age,
            rating_region=rating.region,
            rating_source=rating.source,
        )
        if cumulative_age != rating.minimum_age:
            cumulative_adjustment_count += 1
        relative_links = (
            *generated_age_links(
                rating.category,
                rating.minimum_age,
                "Shows",
                Path(show.name),
                rating_region=rating.region,
                rating_source=rating.source,
            ),
            generated_rating_link(
                rating.certification, "Shows", Path(show.name)
            ),
        )
        for relative_link in relative_links:
            add_desired_link(desired, genres_root / relative_link, show)
        exact_link_count += 1
        cumulative_link_count += len(relative_links) - 2
        rating_link_count += 1
        rows.append(
            (
                "show",
                relative_source,
                show.name,
                rating.category,
                (
                    str(rating.minimum_age)
                    if rating.minimum_age is not None
                    else ""
                ),
                str(cumulative_age) if cumulative_age is not None else "",
                rating.region,
                rating.certification,
                rating.source,
                ",".join(imdb_ids),
                str(rating.tmdb_id or ""),
                rating.match_method,
                note,
                json.dumps(
                    [str(relative_link) for relative_link in relative_links],
                    ensure_ascii=False,
                    separators=(",", ":"),
                ),
            )
        )

    for unused in sorted(set(overrides) - used_overrides):
        print(
            f"warning: reviewed age override does not match catalog media: "
            f"{unused}",
            file=sys.stderr,
        )

    index_path = genres_root / "_age-index.tsv"
    old_owned = load_owned_links(index_path, genres_root, library_root)

    # Refuse manual collisions before removing any previously generated link.
    for link, target in desired.items():
        assert_safe_link_parent(link, genres_root)
        if not (link.exists() or link.is_symlink()):
            continue
        old_target = old_owned.get(link)
        if (
            old_target is not None
            and link.is_symlink()
            and link.resolve(strict=False) == old_target.resolve(strict=False)
        ):
            continue
        raise FileExistsError(f"refusing to replace existing path: {link}")

    summary = (
        f"{len(movies)} movies and {len(shows)} shows with "
        f"{exact_link_count} BY_AGE and {cumulative_link_count} UNTIL_AGE "
        f"and {rating_link_count} BY_RATING symlinks; "
        f"{override_count} reviewed overrides, "
        f"{tmdb_count} TMDB ratings, {wikidata_count} Wikidata ratings, and "
        f"{unrated_count} unrated items; {cumulative_adjustment_count} "
        f"conservative cumulative adjustments."
    )
    if args.dry_run:
        print(f"Would index {summary}")
        return 0

    tmdb.save()
    wikidata.save()

    old_link_parents: set[Path] = set()
    for link, old_target in old_owned.items():
        new_target = desired.get(link)
        if (
            new_target is not None
            and link.is_symlink()
            and link.resolve(strict=False) == new_target.resolve(strict=False)
        ):
            continue
        if (
            link.is_symlink()
            and link.resolve(strict=False) == old_target.resolve(strict=False)
        ):
            link.unlink()
            old_link_parents.add(link.parent)

    for directory in sorted(
        old_link_parents, key=lambda path: len(path.parts), reverse=True
    ):
        while directory != genres_root and directory.parent != genres_root:
            try:
                directory.rmdir()
            except OSError:
                break
            directory = directory.parent

    for link, target in desired.items():
        link.parent.mkdir(parents=True, exist_ok=True)
        if link.exists() or link.is_symlink():
            if (
                link.is_symlink()
                and link.resolve(strict=False) == target.resolve(strict=False)
            ):
                continue
            raise FileExistsError(f"refusing to replace existing path: {link}")
        link.symlink_to(relative_link_target(link, target))

    temporary_index = index_path.with_suffix(index_path.suffix + ".new")
    with temporary_index.open(
        "w", encoding="utf-8", newline=""
    ) as destination:
        writer = csv.writer(destination, delimiter="\t", lineterminator="\n")
        writer.writerow(
            (
                "kind",
                "source",
                "title",
                "age_category",
                "minimum_age",
                "cumulative_minimum_age",
                "rating_region",
                "certification",
                "rating_source",
                "imdb_id",
                "tmdb_id",
                "match_method",
                "note",
                "generated_links",
            )
        )
        writer.writerows(rows)
    temporary_index.replace(index_path)

    print(f"Indexed {summary}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
