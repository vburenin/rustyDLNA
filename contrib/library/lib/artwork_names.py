"""Artwork sidecar names that rustyDLNA's scanner recognizes.

Keep these tables in step with ``STEM_ART_SUFFIXES`` and ``FOLDER_ART_NAMES``
in ``crates/scan/src/artwork.rs``; ``tests/test_artwork.py`` compares them.
Names configured through the server's ``album_art_names`` setting are not
visible here.
"""

from __future__ import annotations

import os
from pathlib import Path


STEM_ART_SUFFIXES = (
    "-poster.jpg",
    "-poster.jpeg",
    "-poster.png",
    "-fanart.jpg",
    "-fanart.jpeg",
    "-fanart.png",
)

FOLDER_ART_NAMES = (
    "poster.jpg",
    "poster.png",
    "poster.jpeg",
    "folder.jpg",
    "folder.png",
    "folder.jpeg",
    "cover.jpg",
    "cover.png",
    "cover.jpeg",
    "albumart.jpg",
    "albumart.jpeg",
    "albumart.png",
    "albumartsmall.jpg",
    "albumartsmall.jpeg",
    "albumartsmall.png",
    "album.jpg",
    "album.jpeg",
    "album.png",
    "thumb.jpg",
    "thumb.jpeg",
    "thumb.png",
)


def _ascii_casefold(name: str) -> str:
    # The scanner folds ASCII only, so other characters must match exactly.
    return name.translate(_ASCII_LOWER)


_ASCII_LOWER = str.maketrans(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz"
)


def recognized_artwork(
    directory: Path, stem: str | None, *, folder_art: bool
) -> list[Path]:
    """Return entries in *directory* the scanner may use as an item's artwork.

    For a media file pass its *stem* to check its stem sidecars. Pass
    *folder_art* when the directory's folder art also belongs to the item: a
    disc, show, or season directory (with ``stem=None``), or a movie that is
    the only catalog item in its directory. Folder art shared by a directory
    of several movies must not suppress their per-movie posters. Matching
    ignores ASCII case as the scanner does, and symlinks (even dangling ones)
    count as present so a fetch never writes through or beside them.
    """
    wanted: set[str] = set(FOLDER_ART_NAMES) if folder_art else set()
    if stem is not None:
        folded_stem = _ascii_casefold(stem)
        wanted.update(folded_stem + suffix for suffix in STEM_ART_SUFFIXES)
    try:
        entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
    except OSError:
        return []
    return [
        Path(entry.path)
        for entry in entries
        if _ascii_casefold(entry.name) in wanted
        and (entry.is_symlink() or entry.is_file(follow_symlinks=False))
    ]


def owns_folder_art(item: Path, catalog_files_in_directory: int) -> bool:
    """Return whether the folder art beside or inside *item* is its own art.

    A disc directory owns the folder art inside it. A movie file owns its
    directory's folder art only when it is the only catalog movie file there
    (*catalog_files_in_directory* is that count, disc directories excluded);
    folder art shared by several movies must not stand in for any one of them.
    The poster fetcher and maintain-library's poster check both use this rule.
    """
    return item.is_dir() or catalog_files_in_directory == 1


def catalog_item_artwork(item: Path, catalog_files_in_directory: int) -> list[Path]:
    """Return the recognized artwork of a catalog movie file or disc directory."""
    if item.is_dir():
        return recognized_artwork(item, None, folder_art=True)
    return recognized_artwork(
        item.parent,
        item.stem,
        folder_art=owns_folder_art(item, catalog_files_in_directory),
    )
