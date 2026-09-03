#!/usr/bin/env python3
"""Enforce coverage floors on P1 risk paths, independent of workspace totals."""

from __future__ import annotations

import json
import pathlib
import sys


# Keep a little headroom below the pinned-toolchain baseline so harmless source
# movement does not fail CI while meaningful regressions remain visible.
FLOORS = {
    "crates/helper/src/process.rs": (75.0, 75.0),
    "crates/server/src/main.rs": (84.0, 62.0),
    "crates/scan/src/watch.rs": (64.0, 65.0),
    "crates/server/src/config.rs": (72.0, 72.0),
    "crates/server/src/catalog_query.rs": (71.0, 85.0),
    "crates/server/src/remux.rs": (73.0, 79.0),
}


def percent(summary: dict[str, object], metric: str) -> float:
    values = summary[metric]
    if not isinstance(values, dict):
        raise ValueError(f"coverage metric {metric!r} is not an object")
    count = int(values["count"])
    covered = int(values["covered"])
    if count <= 0:
        raise ValueError(f"coverage metric {metric!r} has no instrumented entries")
    return covered * 100.0 / count


def main() -> int:
    report_path = pathlib.Path(sys.argv[1] if len(sys.argv) == 2 else "target/coverage.json")
    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
        files = report["data"][0]["files"]
    except (OSError, KeyError, IndexError, TypeError, json.JSONDecodeError) as error:
        print(f"cannot read LLVM coverage report {report_path}: {error}", file=sys.stderr)
        return 2

    summaries: dict[str, dict[str, object]] = {}
    for entry in files:
        filename = str(entry["filename"]).replace("\\", "/")
        for wanted in FLOORS:
            if filename.endswith(f"/{wanted}") or filename == wanted:
                if wanted in summaries:
                    print(f"duplicate coverage entry for {wanted}", file=sys.stderr)
                    return 2
                summaries[wanted] = entry["summary"]

    failed = False
    for filename, (line_floor, function_floor) in FLOORS.items():
        summary = summaries.get(filename)
        if summary is None:
            print(f"MISSING  {filename}", file=sys.stderr)
            failed = True
            continue
        try:
            lines = percent(summary, "lines")
            functions = percent(summary, "functions")
        except (KeyError, TypeError, ValueError) as error:
            print(f"INVALID  {filename}: {error}", file=sys.stderr)
            failed = True
            continue
        passed = lines >= line_floor and functions >= function_floor
        print(
            f"{'PASS' if passed else 'FAIL':4}  {filename}: "
            f"lines {lines:.2f}% (floor {line_floor:.2f}%), "
            f"functions {functions:.2f}% (floor {function_floor:.2f}%)"
        )
        failed |= not passed
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
