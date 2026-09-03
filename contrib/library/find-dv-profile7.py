#!/usr/bin/env python3
"""List Dolby Vision Profile 7 videos that need a Google Streamer recode."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.dont_write_bytecode = True

from lib.dv_profile7 import (
    DEFAULT_PROBE_WORKERS,
    find_profile7,
)
from lib.paths import add_root_argument, require_library_root


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Find Dolby Vision Profile 7 files in playback catalogs and loose "
            "intake. Profile 7 (BL+EL) does not play reliably on Google Streamer."
        )
    )
    add_root_argument(parser)
    parser.add_argument(
        "--workers",
        type=int,
        default=DEFAULT_PROBE_WORKERS,
        help="concurrent ffprobe processes (default: %(default)s)",
    )
    args = parser.parse_args()
    if args.workers < 1:
        parser.error("--workers must be at least 1")

    library_root = require_library_root(parser, args.root)
    records = find_profile7(library_root, workers=args.workers)
    for record in records:
        path = Path(record["path"])
        try:
            relative = path.relative_to(library_root)
        except ValueError:
            relative = path
        size_gb = record["size_bytes"] / 1e9
        sibling = " recoded" if record["streamer_sibling"] else ""
        print(
            f"{relative}\tP7 el={record['el'] or '?'} "
            f"compat={record['compat'] or '?'} "
            f"{size_gb:.2f}GB {record['duration']}{sibling}"
        )

    print(
        f"Dolby Vision Profile 7 videos: {len(records)}",
        file=sys.stderr,
    )
    return 1 if records else 0


if __name__ == "__main__":
    raise SystemExit(main())
