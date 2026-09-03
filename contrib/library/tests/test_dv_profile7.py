from __future__ import annotations

import sys
import threading
import unittest
from pathlib import Path
from unittest import mock


sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))

from lib import dv_profile7  # noqa: E402


class ConcurrentProfileScanTests(unittest.TestCase):
    def test_probes_run_concurrently(self) -> None:
        paths = [Path("first.mkv"), Path("second.mkv"), Path("third.mkv")]
        rendezvous = threading.Barrier(len(paths))

        def inspect(path: Path) -> None:
            rendezvous.wait(timeout=2)
            return None

        with (
            mock.patch.object(dv_profile7, "iter_video_files", return_value=paths),
            mock.patch.object(dv_profile7, "inspect_profile7", side_effect=inspect),
        ):
            self.assertEqual(
                dv_profile7.find_profile7(Path("library"), workers=3),
                [],
            )

    def test_results_remain_in_catalog_order(self) -> None:
        paths = [Path("first.mkv"), Path("second.mkv"), Path("third.mkv")]

        def inspect(path: Path) -> dict | None:
            if path.name == "second.mkv":
                return None
            return {"path": str(path)}

        with (
            mock.patch.object(dv_profile7, "iter_video_files", return_value=paths),
            mock.patch.object(dv_profile7, "inspect_profile7", side_effect=inspect),
        ):
            records = dv_profile7.find_profile7(Path("library"), workers=3)

        self.assertEqual(
            [record["path"] for record in records],
            ["first.mkv", "third.mkv"],
        )

    def test_worker_count_must_be_positive(self) -> None:
        with self.assertRaisesRegex(ValueError, "at least 1"):
            dv_profile7.find_profile7(Path("library"), workers=0)


if __name__ == "__main__":
    unittest.main()
