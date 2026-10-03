"""Example catalog roots shared by the generated movie-library views.

The defaults are a generic movie/show layout. Reviewed IMDb intake homes and
show-identity overrides belong in the operator's local copy of this module or
in ``<library>/.rusty-library/``; they are not shipped with rustyDLNA.
"""

from __future__ import annotations

import os
from pathlib import Path


# Movie files that intake may file into a catalog home and that every catalog
# builder (genre/year/age views, artwork, previews, audits) processes. Each is
# a video format rustyDLNA admits (crates/protocol/src/media_format.rs). Keep
# intake and the builders on this one set: a file intake moves but a builder
# ignores never gets views, artwork, or previews. Disc-structure files
# (.m2ts/.vob/.iso) stay out; a disc directory is one catalog item.
VIDEO_EXTENSIONS = frozenset(
    {
        ".avi",
        ".m4v",
        ".mkv",
        ".mov",
        ".mp4",
        ".mpeg",
        ".mpg",
        ".ts",
        ".webm",
        ".wmv",
    }
)


def is_disc_directory(path: Path) -> bool:
    """Return whether *path* is a Blu-ray or DVD movie root."""
    return (path / "BDMV" / "index.bdmv").is_file() or (
        path / "VIDEO_TS" / "VIDEO_TS.IFO"
    ).is_file()


def catalog_item_label(path: Path) -> str:
    """Return a catalog item's filename stem or complete disc-directory name."""
    return path.name if path.is_dir() else path.stem


def catalog_movie_items(source: Path) -> list[Path]:
    """List movie files and whole disc directories below a catalog root.

    Disc internals are deliberately pruned so a Blu-ray or DVD is represented
    by one catalog item rather than by every transport stream or VOB.
    """
    items: list[Path] = []
    for directory, subdirectories, filenames in os.walk(source):
        directory_path = Path(directory)
        for name in list(subdirectories):
            disc = directory_path / name
            if is_disc_directory(disc):
                items.append(disc)
                subdirectories.remove(name)
        for filename in filenames:
            path = directory_path / filename
            if is_catalog_movie_file(path):
                items.append(path)
    return sorted(items, key=lambda path: str(path).casefold())


def is_catalog_movie_file(path: Path) -> bool:
    """Return whether *path* is a movie file that `catalog_movie_items` lists."""
    return (
        not path.is_symlink()
        and path.is_file()
        and path.suffix.casefold() in VIDEO_EXTENSIONS
    )


def catalog_files_in_directory(directory: Path) -> int:
    """Count catalog movie files directly in *directory* (not disc directories)."""
    try:
        with os.scandir(directory) as entries:
            return sum(1 for entry in entries if is_catalog_movie_file(Path(entry.path)))
    except OSError:
        return 0


# The catalog genre is used as a safe fallback if metadata matching fails.
MOVIE_SOURCES = {
    "action": ("Action",),
    "anime": ("Anime", "Animation"),
    "comedy": ("Comedy",),
    "drama": ("Drama",),
    "fantasy": ("Fantasy",),
    "kids/Movies": ("Kids", "Family"),
    "sci-fi": ("Sci-Fi",),
}

# Optional reviewed intake homes, keyed by IMDb ID. The integer is a
# release-order prefix inside a collection directory; None is standalone.
# Example: "tt0000001": ("sci-fi/Example Collection", 2)
MOVIE_INTAKE_OVERRIDES: dict[str, tuple[str, int | None]] = {}

# Each immediate child is one show catalog. Year links point to that whole
# directory rather than duplicating links for individual episodes.
SHOW_SOURCES = ("shows", "kids/Shows")

# TMDB certifications are jurisdiction-specific. The age builder uses this
# region unless --region or AGE_RATING_REGION selects another one.
AGE_RATING_REGION = "US"

# Directory names are not always unique IMDb series identities. Keep explicit,
# reviewable IDs for catalogs whose contents safely disambiguate the match.
# Example: "shows/Example Show": ("tt0000002",)
SHOW_IMDB_IDS: dict[str, tuple[str, ...]] = {}
