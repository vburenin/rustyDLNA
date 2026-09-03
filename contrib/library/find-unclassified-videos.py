#!/usr/bin/env python3
"""List video files that have no live symlink in the genre index."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path


VIDEO_EXTENSIONS = {
    ".avi",
    ".iso",
    ".m2ts",
    ".m4v",
    ".mkv",
    ".mov",
    ".mp4",
    ".mpeg",
    ".mpg",
    ".ts",
    ".vob",
    ".webm",
    ".wmv",
}

# These are not movie catalogs. They are skipped by default so the report does
# not fill up with episodes, working files, duplicates, and unrelated videos.
DEFAULT_EXCLUSIONS = {
    "audio-books",
    "drone",
    "dupes",
    "incomplete",
    "shows",
    "sport",
    "to-review",
}


def identity(path: Path) -> tuple[int, int]:
    stat = path.stat()
    return stat.st_dev, stat.st_ino


def is_disc_directory(path: Path) -> bool:
    return (
        (path / "BDMV" / "index.bdmv").is_file()
        or (path / "VIDEO_TS" / "VIDEO_TS.IFO").is_file()
    )


def is_video_item(path: Path) -> bool:
    return (
        path.is_file() and path.suffix.casefold() in VIDEO_EXTENSIONS
    ) or (path.is_dir() and is_disc_directory(path))


def linked_video_identities(genres_root: Path) -> set[tuple[int, int]]:
    linked: set[tuple[int, int]] = set()
    for directory, subdirectories, filenames in os.walk(genres_root):
        directory_path = Path(directory)
        if directory_path == genres_root:
            # Faceted links are not genre classifications and must not hide a
            # movie whose normal genre links are missing.
            for generated_view in (
                "BY_YEAR",
                "BY_AGE",
                "UNTIL_AGE",
                "BY_RATING",
            ):
                if generated_view in subdirectories:
                    subdirectories.remove(generated_view)
        names = list(subdirectories) + filenames
        for name in names:
            link = directory_path / name
            if not link.is_symlink():
                continue
            try:
                target = link.resolve(strict=True)
                if is_video_item(target):
                    linked.add(identity(target))
            except (FileNotFoundError, OSError):
                # Broken links are handled by scripts/clean-dead-links.sh.
                continue
    return linked


def candidate_videos(
    library_root: Path, include_all: bool
) -> list[Path]:
    candidates: list[Path] = []
    for directory, subdirectories, filenames in os.walk(
        library_root, followlinks=False
    ):
        directory_path = Path(directory)
        relative_directory = directory_path.relative_to(library_root)

        if relative_directory == Path("."):
            subdirectories[:] = [
                name
                for name in subdirectories
                if name != "genres"
                and not name.startswith(".")
                and (include_all or name not in DEFAULT_EXCLUSIONS)
            ]
        elif (
            not include_all
            and relative_directory == Path("kids")
            and "Shows" in subdirectories
        ):
            subdirectories.remove("Shows")

        # Treat a Blu-ray or DVD directory as one movie and do not report all
        # of its transport-stream/VOB parts separately.
        for name in list(subdirectories):
            disc = directory_path / name
            if is_disc_directory(disc):
                candidates.append(disc)
                subdirectories.remove(name)

        for filename in filenames:
            path = directory_path / filename
            if (
                not path.is_symlink()
                and path.is_file()
                and path.suffix.casefold() in VIDEO_EXTENSIONS
            ):
                candidates.append(path)
    return sorted(candidates, key=lambda path: str(path).casefold())


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Find videos that have no live link in any genre directory."
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="also scan shows, duplicates, incomplete/review areas, and sport",
    )
    from lib.paths import add_root_argument, require_library_root

    add_root_argument(parser)
    args = parser.parse_args()

    library_root = require_library_root(parser, args.root)
    genres_root = library_root / "genres"
    linked = linked_video_identities(genres_root)
    candidates = candidate_videos(library_root, args.all)

    unclassified: list[Path] = []
    for path in candidates:
        try:
            if identity(path) not in linked:
                unclassified.append(path)
        except OSError as error:
            print(f"warning: cannot inspect {path}: {error}", file=sys.stderr)

    for path in unclassified:
        print(path.relative_to(library_root))

    print(
        f"Unclassified videos: {len(unclassified)} "
        f"(checked {len(candidates)} movie candidates)",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
