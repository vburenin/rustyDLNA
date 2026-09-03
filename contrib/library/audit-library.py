#!/usr/bin/env python3
"""Dry, full-library identity and classification audit."""

from __future__ import annotations

import argparse
import sys

sys.dont_write_bytecode = True

from lib.library_audit import audit_library, print_audit
from lib.paths import add_root_argument, require_library_root


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Read-only audit of all catalog identity and genre classifications."
    )
    parser.add_argument(
        "--deep",
        action="store_true",
        help="FFprobe every movie and shadow-test the autonomous intake classifier",
    )
    parser.add_argument(
        "--show-placement-reviews",
        action="store_true",
        help="list optional primary catalog-placement suggestions",
    )
    parser.add_argument("--workers", type=int, default=None)
    add_root_argument(parser)
    args = parser.parse_args()
    if args.workers is not None and args.workers < 1:
        parser.error("--workers must be at least 1")
    library_root = require_library_root(parser, args.root)
    report = audit_library(library_root, deep=args.deep, workers=args.workers)
    print_audit(report, show_placement_reviews=args.show_placement_reviews)
    return int(any(finding.severity == "ERROR" for finding in report.findings))


if __name__ == "__main__":
    raise SystemExit(main())
