#!/usr/bin/env python3
"""Intake loose media, rebuild catalog views, and verify the library."""

from __future__ import annotations

import argparse
import csv
import fcntl
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True

from lib.intake_media import apply_intake, plan_intake, print_intake_report
from lib.paths import add_root_argument, require_library_root, state_dir, tools_dir


def run(command: list[str], *, accepted: set[int] | None = None) -> int:
    accepted_statuses = accepted if accepted is not None else {0}
    sys.stdout.flush()
    sys.stderr.flush()
    completed = subprocess.run(command)
    if completed.returncode not in accepted_statuses:
        raise RuntimeError(
            f"command failed with status {completed.returncode}: {' '.join(command)}"
        )
    return completed.returncode


def write_intake_receipt(library_root: Path, plans: list) -> Path:
    """Record every applied source-to-destination mapping for later review."""
    timestamp = datetime.now(timezone.utc)
    report_dir = library_root / "to-review" / "Intake-Reports"
    report_dir.mkdir(parents=True, exist_ok=True)
    report = report_dir / timestamp.strftime("intake-%Y%m%dT%H%M%S.%fZ.tsv")
    with report.open("x", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
        writer.writerow(
            (
                "timestamp_utc",
                "action",
                "imdb_id",
                "title",
                "source",
                "destination",
            )
        )
        for plan in plans:
            for source, destination in plan.mappings:
                writer.writerow(
                    (
                        timestamp.isoformat(),
                        plan.action,
                        plan.identity.imdb_id,
                        f"{plan.identity.title} ({plan.identity.year})",
                        str(source.relative_to(library_root)),
                        str(destination.relative_to(library_root)),
                    )
                )
    return report


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Perform confidence-gated loose-media intake, rebuild all generated "
            "views, fill artwork, generate previews, and verify the library. "
            "The default mode applies safe plans; use --dry-run to preview."
        )
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="show intake and update plans without moving media or changing indexes",
    )
    parser.add_argument(
        "--settle-seconds",
        type=float,
        default=5,
        help="seconds over which loose candidates must remain unchanged (default: %(default)s)",
    )
    parser.add_argument(
        "--minimum-confidence",
        type=int,
        default=85,
        help="minimum automatic-intake confidence percentage (default: %(default)s)",
    )
    parser.add_argument("--no-refresh", action="store_true")
    parser.add_argument("--refresh-data", action="store_true")
    parser.add_argument("--refresh-tmdb", action="store_true")
    parser.add_argument("--refresh-wikidata", action="store_true")
    parser.add_argument("--all", action="store_true")
    add_root_argument(parser)
    args = parser.parse_args()
    if args.settle_seconds < 0:
        parser.error("--settle-seconds cannot be negative")
    if not 0 <= args.minimum_confidence <= 100:
        parser.error("--minimum-confidence must be between 0 and 100")
    if args.no_refresh and args.refresh_data:
        parser.error("--no-refresh and --refresh-data cannot be used together")

    tools = tools_dir()
    library_root = require_library_root(parser, args.root)
    lock_path = state_dir(library_root) / ".maintenance.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("another complete library-maintenance run is active")

        print("== Inventory and recognize loose media ==")
        plans, issues, tmdb = plan_intake(
            library_root,
            settle_seconds=args.settle_seconds,
            minimum_confidence=args.minimum_confidence,
            tmdb_token=os.environ.get("TMDB_API_TOKEN"),
            allow_network=True,
        )
        print_intake_report(library_root, plans, issues)

        update_command = [str(tools / "update.sh"), "--root", str(library_root)]
        for enabled, option in (
            (args.no_refresh, "--no-refresh"),
            (args.refresh_data, "--refresh-data"),
            (args.refresh_tmdb, "--refresh-tmdb"),
            (args.refresh_wikidata, "--refresh-wikidata"),
            (args.all, "--all"),
        ):
            if enabled:
                update_command.append(option)

        if args.dry_run:
            print("\n== Preview post-intake catalog maintenance ==")
            run([*update_command, "--dry-run"])
            for plan in plans:
                print(
                    "PREVIEW-PLAN\t"
                    f"generate-dlna-previews.py --width 960 "
                    f"{plan.destination.relative_to(library_root)}"
                )
            print("\n== Full read-only timeline-preview audit ==")
            run(
                [
                    str(tools / "generate-dlna-previews.py"),
                    "--root",
                    str(library_root),
                    "--width",
                    "960",
                    "--dry-run",
                    "--summary-only",
                ]
            )
            print("\n== Full read-only catalog identity/classification audit ==")
            audit_status = run(
                [str(tools / "audit-library.py"), "--root", str(library_root), "--deep"],
                accepted={0, 1},
            )
            return 1 if issues or audit_status else 0

        if plans:
            print("\n== Pre-move Dolby Vision Profile 7 scan ==")
            run(
                [str(tools / "find-dv-profile7.py"), "--root", str(library_root)],
                accepted={0, 1},
            )
            destinations = apply_intake(library_root, plans)
            tmdb.save()
            receipt = write_intake_receipt(library_root, plans)
            for plan in plans:
                print(
                    f"MOVED\t{plan.source.relative_to(library_root)}\t"
                    f"{plan.destination.relative_to(library_root)}"
                )
            print(f"INTAKE-RECEIPT\t{receipt.relative_to(library_root)}")
        else:
            destinations = []

        print("\n== Rebuild and verify generated views ==")
        run(update_command)

        if destinations:
            print("\n== Generate rustyDLNA timeline previews ==")
            preview_command = [
                str(tools / "generate-dlna-previews.py"),
                "--root",
                str(library_root),
                "--width",
                "960",
                "--workers",
                "4",
                *(str(path) for path in destinations),
            ]
            run(preview_command)
            run([*preview_command[:7], "--dry-run", *preview_command[7:]])

            missing_posters = [
                path.with_name(f"{path.stem}-poster.jpg")
                for path in destinations
                if not path.with_name(f"{path.stem}-poster.jpg").is_file()
            ]
            if missing_posters:
                for poster in missing_posters:
                    print(
                        f"MISSING-POSTER\t{poster.relative_to(library_root)}",
                        file=sys.stderr,
                    )
                return 1

        print("\n== Full read-only timeline-preview audit ==")
        run(
            [
                str(tools / "generate-dlna-previews.py"),
                "--root",
                str(library_root),
                "--width",
                "960",
                "--dry-run",
                "--summary-only",
            ]
        )
        print("\n== Full read-only catalog identity/classification audit ==")
        audit_status = run(
            [str(tools / "audit-library.py"), "--root", str(library_root), "--deep"],
            accepted={0, 1},
        )
        if audit_status:
            print("Full catalog audit found errors.", file=sys.stderr)
            return 1
        if issues:
            print(
                f"Maintenance completed safe items, but {len(issues)} item(s) still need review.",
                file=sys.stderr,
            )
            return 1
        print("Library maintenance complete.")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
