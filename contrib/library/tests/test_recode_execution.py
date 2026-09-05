from __future__ import annotations

from contextlib import ExitStack
import copy
import importlib.util
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))
from lib import safe_move

spec = importlib.util.spec_from_file_location("recode_execution", SCRIPTS_DIR / "recode-dv-profile7.py")
recode = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = recode
spec.loader.exec_module(recode)


class RecodeExecutionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "Movie (2020) - Profile 7.mkv"
        self.dest = self.root / "Movie (2020) - HDR10 Streamer.mp4"
        self.archive = self.root / recode.ARCHIVE_DIRNAME
        self.archive.mkdir(parents=True)
        shutil.copyfile(SCRIPTS_DIR.parents[1] / "testdata/library/video/dvp7.mkv", self.source)
        self.original = self.source.read_bytes()
        self.source_info = {"streams": [{
            "codec_type": "video", "codec_name": "hevc", "codec_tag_string": "hvc1",
            "pix_fmt": "yuv420p10le", "color_transfer": "smpte2084",
            "color_primaries": "bt2020", "duration": "120", "width": 3840, "height": 2160,
        }]}

    def apply(self, *, replace=False, dry=False):
        return recode.recode_one(
            recode.Job(self.source, self.dest), self.root, self.archive,
            dry, replace, False, False,
        )

    def probes(self, dest_info=None):
        stack = ExitStack()
        stack.enter_context(mock.patch.object(
            recode, "ffprobe_json",
            side_effect=lambda path: self.source_info if path == self.source else (dest_info or self.source_info),
        ))
        stack.enter_context(mock.patch.object(recode, "ffprobe_banner", return_value=""))
        stack.enter_context(mock.patch.object(recode, "has_hdr10plus", return_value=False))
        return stack

    def assert_original_preserved(self):
        self.assertEqual(self.source.read_bytes(), self.original)
        self.assertFalse((self.archive / self.source.name).exists())
        self.assertFalse((self.archive / recode.MANIFEST_NAME).exists())

    def test_empty_and_truncated_existing_outputs_leave_source_in_catalog(self):
        for data in (b"", b"x", b"truncated" * 150_000):
            with self.subTest(size=len(data)):
                self.dest.write_bytes(data)
                with self.assertRaises(RuntimeError):
                    self.apply()
                self.assert_original_preserved()
                self.assertEqual(self.dest.read_bytes(), data)

    def test_wrong_codec_duration_and_missing_duration_leave_source_in_catalog(self):
        self.dest.write_bytes(b"x" * 1_000_000)
        for field, value in (("codec_name", "h264"), ("duration", "30"),
                             ("duration", "nan"), ("duration", "0"), ("duration", "N/A")):
            with self.subTest(field=field, value=value):
                info = copy.deepcopy(self.source_info)
                info["streams"][0][field] = value
                with self.probes(info), self.assertRaises(RuntimeError):
                    self.apply()
                self.assert_original_preserved()

    def test_valid_existing_output_permits_archive_after_verification(self):
        self.dest.write_bytes(b"verified output" * 100_000)
        with self.probes():
            self.assertIn("archive-only", self.apply())
        self.assertFalse(self.source.exists())
        self.assertEqual((self.archive / self.source.name).read_bytes(), self.original)
        self.assertEqual(self.dest.read_bytes(), b"verified output" * 100_000)
        self.assertIn(self.dest.name, (self.archive / recode.MANIFEST_NAME).read_text())

    def test_dry_run_checks_existing_output_without_archiving(self):
        self.dest.write_bytes(b"x")
        with self.assertRaises(RuntimeError):
            self.apply(dry=True)
        self.assert_original_preserved()

    def test_archive_collision_at_rename_boundary_preserves_source_and_incumbent(self):
        self.dest.write_bytes(b"x" * 1_000_000)
        archived = self.archive / self.source.name
        rename = safe_move.rename_noreplace

        def race(source, target):
            target.write_bytes(b"other archived source")
            rename(source, target)

        with self.probes(), mock.patch.object(safe_move, "rename_noreplace", side_effect=race):
            with self.assertRaises(FileExistsError):
                self.apply()
        self.assertEqual(self.source.read_bytes(), self.original)
        self.assertEqual(archived.read_bytes(), b"other archived source")

    def test_dangling_archive_collision_is_not_replaced(self):
        self.dest.write_bytes(b"x" * 1_000_000)
        archived = self.archive / self.source.name
        archived.symlink_to(self.root / "missing")
        with self.probes(), self.assertRaisesRegex(RuntimeError, "archive collision"):
            self.apply()
        self.assertTrue(archived.is_symlink())
        self.assertEqual(self.source.read_bytes(), self.original)

    def test_replacement_waits_for_verification_failure_preserves_incumbent(self):
        self.dest.write_bytes(b"incumbent")

        def build(source, temp_out, archive, root):
            temp_out.write_bytes(b"invalid replacement")
            recode.verify_streamer(source, temp_out, lossless_video=True)

        with (
            mock.patch.object(recode, "inspect_profile7", return_value={"duration": "120"}),
            mock.patch.object(recode, "run_lossless_build", side_effect=build),
            self.assertRaises(RuntimeError),
        ):
            self.apply(replace=True)
        self.assertEqual(self.dest.read_bytes(), b"incumbent")
        self.assert_original_preserved()
        self.assertFalse((self.archive / "tmp" / self.dest.name).exists())

    def test_verified_new_build_does_not_replace_newly_occupied_destination(self):
        def build(source, temp_out, archive, root):
            temp_out.write_bytes(b"x" * 1_000_000)
            recode.verify_streamer(source, temp_out, lossless_video=True)
            self.dest.write_bytes(b"other writer")
            return []

        with (
            self.probes(),
            mock.patch.object(recode, "inspect_profile7", return_value={"duration": "120"}),
            mock.patch.object(recode, "run_lossless_build", side_effect=build),
            self.assertRaises(FileExistsError),
        ):
            self.apply()
        self.assertEqual(self.dest.read_bytes(), b"other writer")
        self.assert_original_preserved()

    def test_authorized_replacement_publishes_verified_output_then_archives(self):
        self.dest.write_bytes(b"incumbent")
        replacement = b"verified output" * 100_000

        def build(source, temp_out, archive, root):
            temp_out.write_bytes(replacement)
            self.assertEqual(self.dest.read_bytes(), b"incumbent")
            recode.verify_streamer(source, temp_out, lossless_video=True)
            self.assert_original_preserved()
            return []

        with (
            self.probes(),
            mock.patch.object(recode, "inspect_profile7", return_value={"duration": "120"}),
            mock.patch.object(recode, "run_lossless_build", side_effect=build),
        ):
            self.apply(replace=True)
        self.assertEqual(self.dest.read_bytes(), replacement)
        self.assertFalse(self.source.exists())
        self.assertEqual((self.archive / self.source.name).read_bytes(), self.original)


if __name__ == "__main__":
    unittest.main()
