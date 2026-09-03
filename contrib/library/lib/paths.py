"""Resolve the operator media library and its on-disk tool state.

These programs do not live inside the media tree. The library root must be
passed with ``--root`` or one of ``RUSTY_DLNA_LIBRARY_ROOT``, ``LIBRARY_ROOT``,
or ``RUSTY_DLNA_MEDIA``. Generated IMDb dumps, metadata caches, and locks live
under ``<library>/.rusty-library/``, which rustyDLNA already treats as a hidden
directory and does not scan.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path


STATE_DIRNAME = ".rusty-library"
LIBRARY_ROOT_ENV = (
    "RUSTY_DLNA_LIBRARY_ROOT",
    "LIBRARY_ROOT",
    "RUSTY_DLNA_MEDIA",
)


class LibraryRootError(ValueError):
    """The media library root is missing or not a directory."""


def tools_dir() -> Path:
    """Return the contrib/library directory that contains these programs."""
    return Path(__file__).resolve().parent.parent


def state_dir(library_root: Path) -> Path:
    """Return the per-library cache/lock directory."""
    return library_root / STATE_DIRNAME


def add_root_argument(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--root",
        type=Path,
        help=(
            "media library root (defaults to RUSTY_DLNA_LIBRARY_ROOT, "
            "LIBRARY_ROOT, or RUSTY_DLNA_MEDIA)"
        ),
    )


def resolve_library_root(explicit: Path | str | None = None) -> Path:
    """Return the canonical media-library directory."""
    candidate: Path | str | None = explicit
    if isinstance(candidate, str) and not candidate.strip():
        candidate = None
    if candidate is None:
        for key in LIBRARY_ROOT_ENV:
            value = os.environ.get(key, "").strip()
            if value:
                candidate = value
                break
    if candidate is None:
        raise LibraryRootError(
            "set --root or RUSTY_DLNA_MEDIA to the media library"
        )
    root = Path(candidate).expanduser()
    if not root.is_absolute():
        root = Path.cwd() / root
    try:
        root = root.resolve()
    except OSError as error:
        raise LibraryRootError(f"cannot resolve library root {root}: {error}") from error
    if not root.is_dir():
        raise LibraryRootError(f"library root is not a directory: {root}")
    return root


def prepare_library(explicit: Path | str | None = None) -> Path:
    """Resolve the library root and export it for child processes."""
    root = resolve_library_root(explicit)
    os.environ["RUSTY_DLNA_LIBRARY_ROOT"] = str(root)
    return root


def require_library_root(
    parser: argparse.ArgumentParser, explicit: Path | str | None = None
) -> Path:
    """Resolve the library root or exit through argparse."""
    try:
        return prepare_library(explicit)
    except LibraryRootError as error:
        parser.error(str(error))
        raise
