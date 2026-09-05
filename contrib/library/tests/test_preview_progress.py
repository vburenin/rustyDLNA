from __future__ import annotations

from contextlib import redirect_stderr
import fcntl
import importlib.util
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))
spec = importlib.util.spec_from_file_location("preview_progress", SCRIPTS_DIR / "generate-dlna-previews.py")
preview = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = preview
spec.loader.exec_module(preview)
run_progress = preview.run_ffmpeg_with_progress


class PreviewProgressExecutionTests(unittest.TestCase):
    def run_child(self, code, *, timeout=0.3, stop=None):
        processes = []
        popen = subprocess.Popen

        def start(*args, **kwargs):
            process = popen(*args, **kwargs)
            processes.append(process)
            return process

        try:
            with tempfile.TemporaryFile() as diagnostics, mock.patch.object(preview.subprocess, "Popen", side_effect=start):
                return run_progress(
                    [sys.executable, "-c", code], diagnostics, timeout,
                    "synthetic", "none", 10, time.monotonic(), stop or threading.Event(),
                )
        finally:
            self.assertEqual(len(processes), 1)
            self.assertIsNotNone(processes[0].returncode, "child was not reaped")
            self.assertTrue(processes[0].stdout.closed)

    def test_partial_line_cannot_bypass_deadline(self):
        started = time.monotonic()
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            self.run_child('import os,time;os.write(1,b"frame=");time.sleep(1)')
        self.assertLess(time.monotonic() - started, 2)

    def test_partial_line_eof_and_timely_success(self):
        self.assertEqual(self.run_child('import os;os.write(1,b"frame=2")'), 0)

    def test_cancellation_during_partial_line(self):
        stop = threading.Event()
        timer = threading.Timer(0.15, stop.set)
        timer.start()
        started = time.monotonic()
        try:
            with self.assertRaises(preview.PreviewInterrupted):
                self.run_child('import os,time;os.write(1,b"frame=");time.sleep(2)', timeout=3, stop=stop)
        finally:
            timer.cancel()
            timer.join()
        self.assertLess(time.monotonic() - started, 2)

    def test_continuous_oversized_output_cannot_starve_deadline(self):
        started = time.monotonic()
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            self.run_child('import os\nwhile True: os.write(1,b"x"*4096)')
        self.assertLess(time.monotonic() - started, 2)

    def test_oversized_line_is_discarded_and_ordinary_progress_recovers(self):
        output = io.StringIO()
        with redirect_stderr(output), mock.patch.object(preview, "PROGRESS_INTERVAL_SECONDS", 0):
            result = self.run_child(
                'import os,time;os.write(1,b"frame="+b"9"*10000+b"\\nframe=2\\n");time.sleep(.1)'
            )
        self.assertEqual(result, 0)
        self.assertIn("sheets=2/10", output.getvalue())
        self.assertNotIn("sheets=10/10", output.getvalue())

    def test_ignored_termination_is_killed_and_reaped_within_grace(self):
        started = time.monotonic()
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            self.run_child(
                'import os,signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);os.write(1,b"frame=");time.sleep(30)'
            )
        self.assertGreater(time.monotonic() - started, 5)
        self.assertLess(time.monotonic() - started, 7.5)

    def test_generation_failure_releases_title_lock(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "source.mp4"
            source.write_bytes(b"synthetic")

            def timeout(*args, **kwargs):
                return self.run_child('import os,time;os.write(1,b"frame=");time.sleep(1)')

            with (
                mock.patch.object(preview, "probe_media", return_value=(1, 64, 64)),
                mock.patch.object(preview, "run_ffmpeg_with_progress", side_effect=timeout),
                self.assertRaisesRegex(RuntimeError, "deadline"),
            ):
                preview.generate_one(
                    source, "unused", "unused", True,
                    preview.PreviewRequest((64, 64), None, None),
                    "none", False, "accurate", "synthetic", threading.Event(),
                )
            with (preview.preview_directory(source) / ".generate.lock").open("a+b") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertFalse((preview.preview_directory(source) / "manifest.json").exists())


if __name__ == "__main__":
    unittest.main()
