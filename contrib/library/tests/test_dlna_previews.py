from __future__ import annotations

import importlib.util
import io
import json
import math
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock


sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))

preview_spec = importlib.util.spec_from_file_location(
    "generate_dlna_previews", SCRIPTS_DIR / "generate-dlna-previews.py"
)
assert preview_spec is not None and preview_spec.loader is not None
preview_module = importlib.util.module_from_spec(preview_spec)
sys.modules[preview_spec.name] = preview_module
preview_spec.loader.exec_module(preview_module)


def minimal_jpeg(width: int, height: int) -> bytes:
    return (
        b"\xff\xd8\xff\xc0\x00\x11\x08"
        + height.to_bytes(2, "big")
        + width.to_bytes(2, "big")
        + b"\x03\x01\x11\x00\x02\x11\x00\x03\x11\x00\xff\xd9"
    )


class VideoDurationTests(unittest.TestCase):
    def test_video_tag_wins_over_longer_container_tail(self) -> None:
        payload = {
            "streams": [
                {
                    "width": 3840,
                    "height": 1608,
                    "tags": {"DURATION": "02:34:33.972000000"},
                }
            ],
            "format": {"duration": "9484.096000"},
        }
        duration = preview_module.video_stream_duration(payload)
        self.assertAlmostEqual(duration, 9273.972)
        layout = preview_module.layout_for_frame(960, 402)
        interval = preview_module.interval_seconds(
            duration, preview_module.frame_capacity(layout)
        )
        frame_count = math.ceil(duration / interval)
        self.assertEqual(interval, 4)
        self.assertEqual(math.ceil(frame_count / layout.frames_per_sheet), 78)

    def test_numeric_stream_duration_wins_and_format_is_a_fallback(self) -> None:
        self.assertEqual(
            preview_module.video_stream_duration(
                {
                    "streams": [{"duration": "120.5", "tags": {"DURATION": "0:02:01.0"}}],
                    "format": {"duration": "122"},
                }
            ),
            120.5,
        )
        self.assertEqual(
            preview_module.video_stream_duration(
                {"streams": [{}], "format": {"duration": "122"}}
            ),
            122,
        )

    def test_invalid_duration_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            preview_module.video_stream_duration(
                {
                    "streams": [{"duration": "N/A", "tags": {"DURATION": "bad"}}],
                    "format": {"duration": "0"},
                }
            )


class SamplingFallbackTests(unittest.TestCase):
    def test_keyframe_padding_remains_finite_for_density_detection(self) -> None:
        self.assertEqual(
            preview_module.end_padding_filter("keyframes", 18, 2842.592),
            "tpad=stop=18:stop_mode=clone",
        )

    def test_accurate_padding_covers_a_longer_container_timeline(self) -> None:
        self.assertEqual(
            preview_module.end_padding_filter("accurate", 18, 2842.592),
            "tpad=stop_duration=2842.592000:stop_mode=clone",
        )

    def test_clean_short_keyframe_output_skips_other_decoder_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            paths = [directory / f"sheet-{index}.jpg" for index in range(3)]
            paths[0].write_bytes(minimal_jpeg(100, 50))
            paths[1].write_bytes(minimal_jpeg(100, 50))
            valid = preview_module.valid_sheet_prefix(paths, (100, 50))
            self.assertEqual(valid, 2)
            self.assertTrue(
                preview_module.keyframes_require_accurate_fallback(0, valid, len(paths))
            )

    def test_failed_or_malformed_output_still_tries_a_compatible_decoder(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            paths = [directory / f"sheet-{index}.jpg" for index in range(3)]
            paths[1].write_bytes(minimal_jpeg(100, 50))
            valid = preview_module.valid_sheet_prefix(paths, (100, 50))
            self.assertIsNone(valid)
            self.assertFalse(
                preview_module.keyframes_require_accurate_fallback(0, valid, len(paths))
            )
            self.assertFalse(
                preview_module.keyframes_require_accurate_fallback(1, 2, len(paths))
            )


class ImageSequencePatternTests(unittest.TestCase):
    def test_literal_percent_in_directory_is_escaped_for_ffmpeg(self) -> None:
        directory = Path("/library/shows/50% Off/100% Complete")
        self.assertEqual(
            preview_module.ffmpeg_sheet_pattern(directory, "0123456789abcdef"),
            "/library/shows/50%% Off/100%% Complete/"
            ".sheet-0123456789abcdef-%04d.tmp.jpg",
        )

    def test_number_placeholder_remains_an_ffmpeg_sequence(self) -> None:
        pattern = preview_module.ffmpeg_sheet_pattern(
            Path("/library/shows/Ordinary Title"), "0123456789abcdef"
        )
        self.assertEqual(pattern.count("%"), 1)
        self.assertTrue(pattern.endswith("-%04d.tmp.jpg"))

    def test_literal_pattern_orphan_is_a_temporary_sheet(self) -> None:
        self.assertIsNotNone(
            preview_module.TEMP_SHEET_RE.fullmatch(
                ".sheet-0123456789abcdef-%04d.tmp.jpg"
            )
        )


def fake_ffmpeg_sheets(layout):
    """Write minimal valid sheets to the image2 pattern, as FFmpeg would."""

    def run(command, *args, **kwargs):
        pattern = command[-1]
        count = int(command[command.index("-frames:v") + 1])
        for index in range(count):
            with open(pattern % index, "wb") as output:
                output.write(
                    minimal_jpeg(
                        layout.frame_width * layout.columns,
                        layout.frame_height * layout.rows,
                    )
                )
        return 0

    return run


def tree_snapshot(root: Path) -> list[tuple[str, int, bytes]]:
    entries = []
    for path in sorted(root.rglob("*")):
        info = path.lstat()
        content = path.read_bytes() if stat.S_ISREG(info.st_mode) else b""
        entries.append((str(path.relative_to(root)), info.st_mode, content))
    return entries


class PreviewTimeoutFallbackTests(unittest.TestCase):
    REQUEST = preview_module.PreviewRequest((64, 64), None, None)

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.source = Path(self.temporary.name) / "Title.mp4"
        self.source.write_bytes(b"synthetic")
        self.decoders: list[str] = []

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def generate(self, hwaccel: str, cuda_pipeline: bool, timed_out: set[str]):
        write_sheets = fake_ffmpeg_sheets(preview_module.layout_for_frame(64, 64))

        def run(command, diagnostics, timeout, name, decoder, *args):
            self.decoders.append(decoder)
            pattern = command[-1]
            if decoder.split("/")[0] in timed_out:
                # A partial sheet must not survive into the next attempt.
                with open(pattern % 0, "wb") as output:
                    output.write(b"partial")
                raise preview_module.PreviewTimeout("ffmpeg made no progress for 600s")
            return write_sheets(command)

        with (
            mock.patch.object(preview_module, "probe_media", return_value=(1, 64, 64)),
            mock.patch.object(preview_module, "run_ffmpeg_with_progress", side_effect=run),
            redirect_stderr(io.StringIO()),
        ):
            return preview_module.generate_one(
                self.source, "unused", "unused", True, self.REQUEST,
                hwaccel, cuda_pipeline, "accurate", "synthetic", threading.Event(),
            )

    def test_hung_hardware_decoder_falls_back_to_software(self) -> None:
        _, status = self.generate("auto", True, {"cuda-resident"})
        self.assertTrue(status.startswith("generated"), status)
        self.assertIn("software-fallback", status)
        self.assertEqual(self.decoders, ["cuda-resident/accurate", "none/accurate"])

    def test_software_timeout_fails_without_retrying(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "no progress"):
            self.generate("auto", True, {"cuda-resident", "none"})
        self.assertEqual(self.decoders, ["cuda-resident/accurate", "none/accurate"])
        directory = preview_module.preview_directory(self.source)
        self.assertEqual(
            [path.name for path in directory.iterdir() if path.name.startswith(".sheet-")], []
        )

    def test_explicit_hardware_decoder_without_software_fallback_fails(self) -> None:
        with self.assertRaises(preview_module.PreviewTimeout):
            self.generate("cuda", True, {"cuda-resident"})
        self.assertEqual(self.decoders, ["cuda-resident/accurate"])


class PreviewPermissionTests(unittest.TestCase):
    """Preview output never broadens permissions or follows symlinks."""

    REQUEST = preview_module.PreviewRequest((64, 64), None, None)

    def setUp(self) -> None:
        self.previous_umask = os.umask(0o027)
        self.temporary = tempfile.TemporaryDirectory()
        self.base = Path(self.temporary.name)
        self.library = self.base / "library"
        self.library.mkdir(mode=0o750)
        os.chmod(self.library, 0o750)
        self.source = self.library / "Title.mp4"
        self.source.write_bytes(b"synthetic")

    def tearDown(self) -> None:
        self.temporary.cleanup()
        os.umask(self.previous_umask)

    def generate_fake(self) -> tuple[Path, str]:
        layout = preview_module.layout_for_frame(64, 64)
        with (
            mock.patch.object(preview_module, "probe_media", return_value=(1, 64, 64)),
            mock.patch.object(
                preview_module,
                "run_ffmpeg_with_progress",
                side_effect=fake_ffmpeg_sheets(layout),
            ),
        ):
            return preview_module.generate_one(
                self.source, "unused", "unused", True, self.REQUEST,
                "none", False, "accurate", "synthetic", threading.Event(),
            )

    def assert_no_world_bits(self, root: Path) -> None:
        for path in [root, *root.rglob("*")]:
            mode = stat.S_IMODE(path.lstat().st_mode)
            self.assertEqual(mode & 0o007, 0, f"{path} has mode {mode:o}")

    def test_existing_directory_mode_is_kept_and_outputs_follow_umask(self) -> None:
        container = self.library / preview_module.PREVIEW_CONTAINER
        container.mkdir(mode=0o750)
        os.chmod(container, 0o750)
        _, status = self.generate_fake()
        self.assertTrue(status.startswith("generated"), status)
        self.assertEqual(stat.S_IMODE(container.stat().st_mode), 0o750)
        self.assertEqual(stat.S_IMODE(self.library.stat().st_mode), 0o750)
        self.assert_no_world_bits(container)
        directory = preview_module.preview_directory(self.source)
        self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o750)
        manifest = directory / preview_module.MANIFEST_NAME
        self.assertEqual(stat.S_IMODE(manifest.stat().st_mode), 0o640)

    def test_symlinked_container_outside_tree_is_refused(self) -> None:
        outside = self.base / "outside"
        outside.mkdir(mode=0o750)
        os.chmod(outside, 0o750)
        (outside / "sentinel.txt").write_text("keep\n", encoding="utf-8")
        before = (stat.S_IMODE(outside.stat().st_mode), tree_snapshot(outside))
        (self.library / preview_module.PREVIEW_CONTAINER).symlink_to(outside)
        with self.assertRaisesRegex(RuntimeError, "symlinked preview path"):
            self.generate_fake()
        self.assertEqual(
            (stat.S_IMODE(outside.stat().st_mode), tree_snapshot(outside)), before
        )

    def test_symlinked_title_directory_outside_tree_is_refused(self) -> None:
        outside = self.base / "outside"
        outside.mkdir(mode=0o750)
        os.chmod(outside, 0o750)
        (outside / "sentinel.txt").write_text("keep\n", encoding="utf-8")
        before = (stat.S_IMODE(outside.stat().st_mode), tree_snapshot(outside))
        container = self.library / preview_module.PREVIEW_CONTAINER
        container.mkdir()
        (container / self.source.stem).symlink_to(outside)
        with self.assertRaisesRegex(RuntimeError, "symlinked preview path"):
            self.generate_fake()
        self.assertEqual(
            (stat.S_IMODE(outside.stat().st_mode), tree_snapshot(outside)), before
        )

    @unittest.skipUnless(
        shutil.which("ffmpeg") and shutil.which("ffprobe"), "ffmpeg/ffprobe unavailable"
    )
    def test_real_ffmpeg_run_publishes_usable_umask_restricted_previews(self) -> None:
        self.source.unlink()
        subprocess.run(
            [
                "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc=size=128x72:rate=10:duration=3",
                "-c:v", "mpeg4", "-y", str(self.source),
            ],
            check=True,
            timeout=60,
        )
        _, status = preview_module.generate_one(
            self.source, "ffmpeg", "ffprobe", False, self.REQUEST,
            "none", False, "accurate", "synthetic", threading.Event(),
        )
        self.assertTrue(status.startswith("generated"), status)
        directory = preview_module.preview_directory(self.source)
        manifest = json.loads(
            (directory / preview_module.MANIFEST_NAME).read_text(encoding="utf-8")
        )
        self.assertTrue(
            preview_module.manifest_is_current(
                self.source, self.source.stat(), manifest, self.REQUEST, "accurate"
            )
        )
        sheets = sorted(directory.glob("sheet-*.jpg"))
        self.assertTrue(sheets)
        width = manifest["frame_width"] * manifest["columns"]
        height = manifest["frame_height"] * manifest["rows"]
        for sheet in sheets:
            self.assertEqual(preview_module.jpeg_dimensions(sheet), (width, height))
        self.assertFalse(list(directory.glob(".*.tmp*")))
        self.assert_no_world_bits(directory.parent)



class RequestedVideoTests(unittest.TestCase):
    def test_every_intake_container_is_a_requested_preview_source(self) -> None:
        from lib import intake_media

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            requested = []
            for suffix in sorted(intake_media.VIDEO_EXTENSIONS):
                path = root / f"movie{suffix}"
                path.write_bytes(b"movie")
                requested.append(path)
            self.assertEqual(
                preview_module.collect_requested_videos(root, requested),
                sorted((path.resolve() for path in requested), key=os.fsencode),
            )

    def test_explicit_unsupported_file_is_still_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "notes.txt").write_text("notes")
            with self.assertRaisesRegex(ValueError, "not a supported video"):
                preview_module.collect_requested_videos(root, [Path("notes.txt")])


if __name__ == "__main__":
    unittest.main()
