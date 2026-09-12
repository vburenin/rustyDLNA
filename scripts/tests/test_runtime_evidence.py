"""Behavioral bounds and identity checks for retained runtime evidence."""

import hashlib
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("runtime_evidence", REPO / "scripts/runtime-evidence.py")
EVIDENCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EVIDENCE)


class RuntimeEvidenceTests(unittest.TestCase):
    def test_records_exact_output_and_failure_without_claiming_a_vulnerability(self):
        result = EVIDENCE.capture([sys.executable, "-c", "print('ffmpeg version 8.example'); raise SystemExit(7)"])
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["exit_code"], 7)
        self.assertEqual(result["output"], "ffmpeg version 8.example\n")
        missing = EVIDENCE.capture(["/nonexistent/rustydlna-evidence-tool"])
        self.assertEqual(missing["status"], "unavailable")

    def test_null_stdin_and_output_limit(self):
        result = EVIDENCE.capture([sys.executable, "-c", "import sys; print(repr(sys.stdin.read()))"])
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["output"], "''\n")
        result = EVIDENCE.capture([sys.executable, "-c", "import os;\nwhile True: os.write(1, b'x' * 10000)"], limit=1234)
        self.assertEqual(result["status"], "output-limit")
        self.assertEqual(len(result["output"]), 1234)

    def test_deadline_kills_a_descendant_that_keeps_the_pipe_open(self):
        with tempfile.TemporaryDirectory(prefix="rustydlna-evidence-test-") as directory:
            marker = Path(directory) / "descendant-survived"
            code = ("import os, pathlib, time; pid = os.fork(); "
                    "os._exit(0) if pid else None; time.sleep(0.6); "
                    f"pathlib.Path({str(marker)!r}).write_text('alive')")
            result = EVIDENCE.capture([sys.executable, "-c", code], timeout=0.1)
            self.assertEqual(result["status"], "timeout")
            # Wait with a bounded child so a surviving descendant has time to
            # write; this assertion verifies process-group cleanup, not timing.
            EVIDENCE.capture([sys.executable, "-c", "import time; time.sleep(0.7)"])
            self.assertFalse(marker.exists())

    def test_identity_follows_symlink_and_hashes_bytes(self):
        with tempfile.TemporaryDirectory(prefix="rustydlna-evidence-test-") as directory:
            target = Path(directory) / "tool"
            target.write_bytes(b"exact fixture tool")
            link = Path(directory) / "alias"
            link.symlink_to(target)
            identity = EVIDENCE.file_identity(link)
            self.assertEqual(identity["path"], str(target))
            self.assertEqual(identity["sha256"], hashlib.sha256(target.read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
