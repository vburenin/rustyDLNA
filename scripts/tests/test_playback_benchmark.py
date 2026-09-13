"""Run the dependency-free benchmark CLI regression suite in the canonical gate."""
import pathlib
import subprocess
import unittest


class PlaybackBenchmarkSummaryTests(unittest.TestCase):
    def test_report_cli(self):
        root = pathlib.Path(__file__).resolve().parents[2]
        subprocess.run(
            ["node", "--test", "scripts/tests/playback-benchmark.test.mjs"],
            cwd=root, check=True, timeout=60,
        )


if __name__ == "__main__":
    unittest.main()
