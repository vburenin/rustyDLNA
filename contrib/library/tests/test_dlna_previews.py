from __future__ import annotations

import importlib.util
import math
import sys
import tempfile
import unittest
from pathlib import Path


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


if __name__ == "__main__":
    unittest.main()
