#!/usr/bin/env python3
"""Generate rustyDLNA timeline-preview sprite sidecars for catalog videos.

Each source `Title.ext` owns `.rusty_previews/Title/` within the source
directory. Sprite sheets are written under a new revision name before
`manifest.json` is atomically replaced, so interrupted work cannot replace a
previously complete preview set.

Frame geometry may be fixed with ``--resolution WIDTHxHEIGHT`` or derived from
each source with ``--scale Nx`` or ``--width PIXELS``. Width mode derives an
even height from every source's aspect ratio. The manifest records the selected
bounded layout so rustyDLNA needs no corresponding runtime configuration.

Video decoding requests FFmpeg's automatic hardware acceleration by default;
when CUDA is available, periodic selection and scaling stay GPU-resident before
the selected thumbnails are downloaded for CPU tiling and JPEG encoding. Fast
keyframe sampling is the default; compatible and accurate fallbacks preserve
generation across unsupported inputs.

Each active title reports its own elapsed time and sprite-sheet progress.
Ctrl+C cooperatively stops active helpers and leaves the last published
preview revision intact.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import math
import os
import re
import selectors
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path

sys.dont_write_bytecode = True

from lib.catalog_config import (
    MOVIE_SOURCES,
    SHOW_SOURCES,
    VIDEO_EXTENSIONS,
    catalog_movie_items,
)
from lib.paths import add_root_argument, require_library_root


SCHEMA_VERSION = 1
PREVIEW_CONTAINER = ".rusty_previews"
LEGACY_DIRECTORY_SUFFIX = ".rustydlna-previews"
MANIFEST_NAME = "manifest.json"
DEFAULT_FRAME_WIDTH = 640
DEFAULT_FRAME_HEIGHT = 360
TARGET_FRAMES = 2_400
MAX_SHEETS = 256
MAX_DURATION_SECONDS = 7 * 24 * 60 * 60
MAX_SHEET_BYTES = 16 * 1024 * 1024
MIN_FRAME_DIMENSION = 16
MAX_FRAME_DIMENSION = 4_096
MAX_FRAME_PIXELS = 4_194_304
MAX_LAYOUT_AXIS = 10
MAX_SHEET_DIMENSION = 4_096
MAX_SHEET_PIXELS = 12_000_000
MAX_SCALE_DIVISOR = 64
PROFILE_REVISION = "rustydlna-trickplay-v2"
GENERATED_SHEET_RE = re.compile(r"^sheet-[0-9a-f]{16}-\d{4}\.jpg$")
TEMP_SHEET_RE = re.compile(
    r"^\.sheet-[0-9a-f]{16}-(?:\d{4}|%04d)\.tmp\.jpg$"
)
TEMP_MANIFEST_RE = re.compile(r"^\.manifest\..+\.tmp$")
PROGRESS_INTERVAL_SECONDS = 5
PROGRESS_READ_BYTES = 4096
MAX_PROGRESS_LINE_BYTES = 1024
MEDIA_DURATION_RE = re.compile(
    r"^(?P<hours>[0-9]+):(?P<minutes>[0-5][0-9]):"
    r"(?P<seconds>[0-5][0-9](?:\.[0-9]+)?)$"
)


class PreviewInterrupted(Exception):
    """Raised when the user asks active preview work to stop."""


@dataclass(frozen=True)
class PreviewLayout:
    frame_width: int
    frame_height: int
    columns: int
    rows: int

    @property
    def frames_per_sheet(self) -> int:
        return self.columns * self.rows


@dataclass(frozen=True)
class PreviewRequest:
    resolution: tuple[int, int] | None
    scale_divisor: int | None
    target_width: int | None

    def frame_size(self, source_width: int, source_height: int) -> tuple[int, int]:
        if self.resolution is not None:
            return self.resolution
        if self.scale_divisor is not None:
            width = max(2, source_width // self.scale_divisor // 2 * 2)
            height = max(2, source_height // self.scale_divisor // 2 * 2)
            return width, height
        if self.target_width is not None:
            height = max(
                2,
                (source_height * self.target_width + source_width)
                // (2 * source_width)
                * 2,
            )
            return self.target_width, height
        raise ValueError("preview request has no resolution, width, or scale divisor")


def parse_resolution(value: str) -> tuple[int, int]:
    match = re.fullmatch(r"([1-9][0-9]{0,4})[xX]([1-9][0-9]{0,4})", value)
    if not match:
        raise argparse.ArgumentTypeError("resolution must use WIDTHxHEIGHT, for example 640x360")
    return int(match.group(1)), int(match.group(2))


def parse_scale(value: str) -> int:
    match = re.fullmatch(r"([1-9][0-9]{0,2})(?:[xX])?", value)
    if not match or int(match.group(1)) > MAX_SCALE_DIVISOR:
        raise argparse.ArgumentTypeError("scale must be 1x through 64x, for example 4x")
    return int(match.group(1))


def parse_width(value: str) -> int:
    try:
        width = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("width must be an integer from 16 through 4096") from error
    if not MIN_FRAME_DIMENSION <= width <= MAX_FRAME_DIMENSION:
        raise argparse.ArgumentTypeError("width must be an integer from 16 through 4096")
    return width


def layout_for_frame(frame_width: int, frame_height: int) -> PreviewLayout:
    if not (
        MIN_FRAME_DIMENSION <= frame_width <= MAX_FRAME_DIMENSION
        and MIN_FRAME_DIMENSION <= frame_height <= MAX_FRAME_DIMENSION
        and frame_width * frame_height <= MAX_FRAME_PIXELS
    ):
        raise ValueError(
            "preview frame is outside the supported 16–4096 pixel edge and 4-megapixel limit"
        )
    candidates: list[PreviewLayout] = []
    for columns in range(1, MAX_LAYOUT_AXIS + 1):
        for rows in range(1, MAX_LAYOUT_AXIS + 1):
            sheet_width = frame_width * columns
            sheet_height = frame_height * rows
            frames_per_sheet = columns * rows
            if (
                sheet_width <= MAX_SHEET_DIMENSION
                and sheet_height <= MAX_SHEET_DIMENSION
                and sheet_width * sheet_height <= MAX_SHEET_PIXELS
            ):
                candidates.append(PreviewLayout(frame_width, frame_height, columns, rows))
    if not candidates:
        raise ValueError(
            "preview resolution cannot fit even one frame in a bounded sprite sheet"
        )
    return max(
        candidates,
        key=lambda layout: (
            layout.frames_per_sheet,
            layout.frame_width * layout.columns * layout.frame_height * layout.rows,
            layout.rows,
        ),
    )


def frame_capacity(layout: PreviewLayout) -> int:
    return min(TARGET_FRAMES, layout.frames_per_sheet * MAX_SHEETS)


def preview_directory(source: Path) -> Path:
    return source.parent / PREVIEW_CONTAINER / source.stem


def ffmpeg_sheet_pattern(directory: Path, revision: str) -> str:
    """Build an image2 pattern without treating literal path percents as fields."""
    escaped_directory = os.fspath(directory).replace("%", "%%")
    return os.path.join(escaped_directory, f".sheet-{revision}-%04d.tmp.jpg")


def is_preview_container(name: str) -> bool:
    return name == PREVIEW_CONTAINER or name.endswith(LEGACY_DIRECTORY_SUFFIX)


def fsync_directory(directory: Path) -> None:
    descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def colliding_video_sources(source: Path) -> list[Path]:
    destination = preview_directory(source)
    try:
        entries = source.parent.iterdir()
    except OSError:
        return [source]
    return sorted(
        (
            candidate
            for candidate in entries
            if candidate.suffix.casefold() in VIDEO_EXTENSIONS
            and not candidate.is_symlink()
            and candidate.is_file()
            and preview_directory(candidate) == destination
        ),
        key=lambda path: os.fsencode(path),
    )


def source_stat(source: Path) -> os.stat_result:
    stat = source.stat()
    if not source.is_file() or source.is_symlink():
        raise ValueError("source must be a regular, non-symlink video")
    return stat


def interval_seconds(duration: float, target_frames: int = TARGET_FRAMES) -> int:
    if not math.isfinite(duration) or duration <= 0 or duration > MAX_DURATION_SECONDS:
        raise ValueError("duration is outside the supported 0–7 day range")
    if not 1 <= target_frames <= TARGET_FRAMES:
        raise ValueError("target frame count is outside the supported range")
    return max(1, math.ceil(duration / target_frames))


def end_padding_filter(
    sampling_mode: str, padding_frames: int, duration: float
) -> str:
    """Keep keyframe density observable while filling accurate container tails."""
    if sampling_mode == "accurate":
        return f"tpad=stop_duration={duration:.6f}:stop_mode=clone"
    return f"tpad=stop={padding_frames}:stop_mode=clone"


def preview_filter_graph(
    layout: PreviewLayout,
    interval: int,
    padding_frames: int,
    duration: float,
    sampling_mode: str,
    gpu_resident: bool,
) -> str:
    filters = [f"fps=fps=1/{interval}:round=up"]
    if gpu_resident:
        filters.extend(
            (
                f"scale_cuda={layout.frame_width}:{layout.frame_height}:format=yuv420p:"
                "force_original_aspect_ratio=decrease:force_divisible_by=2",
                "hwdownload",
                "format=yuv420p",
            )
        )
    else:
        filters.append(
            f"scale={layout.frame_width}:{layout.frame_height}:"
            "force_original_aspect_ratio=decrease:force_divisible_by=2"
        )
    filters.extend(
        (
            f"pad={layout.frame_width}:{layout.frame_height}:(ow-iw)/2:(oh-ih)/2:black",
            end_padding_filter(sampling_mode, padding_frames, duration),
            f"tile={layout.columns}x{layout.rows}:nb_frames={layout.frames_per_sheet}:"
            "padding=0:margin=0:color=black",
        )
    )
    return ",".join(filters)


def media_duration_seconds(value: object) -> float | None:
    """Parse an FFprobe numeric or Matroska ``HH:MM:SS.fraction`` duration."""
    try:
        duration = float(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        duration = math.nan
    if math.isfinite(duration) and 0 < duration <= MAX_DURATION_SECONDS:
        return duration
    if not isinstance(value, str):
        return None
    match = MEDIA_DURATION_RE.fullmatch(value)
    if not match:
        return None
    duration = (
        int(match.group("hours")) * 3600
        + int(match.group("minutes")) * 60
        + float(match.group("seconds"))
    )
    return duration if 0 < duration <= MAX_DURATION_SECONDS else None


def video_stream_duration(payload: dict[str, object]) -> float:
    """Prefer the selected video stream's duration over container tail data."""
    try:
        stream = payload["streams"][0]  # type: ignore[index]
        format_data = payload["format"]
    except (IndexError, KeyError, TypeError) as error:
        raise ValueError("FFprobe response has no selected video stream") from error
    if not isinstance(stream, dict) or not isinstance(format_data, dict):
        raise ValueError("FFprobe response has invalid stream or format data")
    tags = stream.get("tags")
    tagged_duration = None
    if isinstance(tags, dict):
        tagged_duration = next(
            (
                value
                for key, value in tags.items()
                if isinstance(key, str) and key.casefold() == "duration"
            ),
            None,
        )
    for candidate in (
        stream.get("duration"),
        tagged_duration,
        format_data.get("duration"),
    ):
        duration = media_duration_seconds(candidate)
        if duration is not None:
            return duration
    raise ValueError("FFprobe response has no usable video duration")


def valid_sheet_prefix(
    paths: list[Path], expected_dimensions: tuple[int, int]
) -> int | None:
    """Return the contiguous valid prefix, or None for malformed/holey output."""
    count = 0
    missing = False
    for path in paths:
        if not path.exists():
            missing = True
            continue
        if missing or jpeg_dimensions(path) != expected_dimensions:
            return None
        count += 1
    return count


def keyframes_require_accurate_fallback(
    return_code: int, valid_sheets: int | None, expected_sheets: int
) -> bool:
    """Treat a clean, coherent short keyframe run as a sampling limitation."""
    return (
        return_code == 0
        and valid_sheets is not None
        and 0 < valid_sheets < expected_sheets
    )


def jpeg_dimensions(path: Path) -> tuple[int, int] | None:
    try:
        size = path.stat().st_size
        if size <= 0 or size > MAX_SHEET_BYTES:
            return None
        with path.open("rb") as source:
            data = source.read(min(size, 256 * 1024))
            source.seek(-2, os.SEEK_END)
            ending = source.read(2)
    except OSError:
        return None
    if not data.startswith(b"\xff\xd8") or ending != b"\xff\xd9":
        return None
    offset = 2
    sof = {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}
    while offset < len(data):
        while offset < len(data) and data[offset] == 0xFF:
            offset += 1
        if offset >= len(data):
            return None
        marker = data[offset]
        offset += 1
        if marker in {0xD9, 0xDA}:
            return None
        if marker == 0x01 or 0xD0 <= marker <= 0xD7:
            continue
        if offset + 2 > len(data):
            return None
        length = int.from_bytes(data[offset : offset + 2], "big")
        if length < 2 or offset + length > len(data):
            return None
        if marker in sof:
            if length < 7:
                return None
            height = int.from_bytes(data[offset + 3 : offset + 5], "big")
            width = int.from_bytes(data[offset + 5 : offset + 7], "big")
            return (width, height) if width > 0 and height > 0 else None
        offset += length
    return None


def read_manifest(directory: Path) -> dict[str, object] | None:
    path = directory / MANIFEST_NAME
    try:
        if path.stat().st_size > 16 * 1024:
            return None
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def manifest_is_current(
    source: Path,
    stat: os.stat_result,
    manifest: dict[str, object] | None,
    request: PreviewRequest,
    sampling_mode: str,
) -> bool:
    if not manifest:
        return False
    try:
        duration = float(manifest["duration_seconds"])
        interval = int(manifest["interval_seconds"])
        frame_count = int(manifest["frame_count"])
        revision = str(manifest["asset_revision"])
        layout = PreviewLayout(
            int(manifest["frame_width"]),
            int(manifest["frame_height"]),
            int(manifest["columns"]),
            int(manifest["rows"]),
        )
        sheet_count = math.ceil(frame_count / layout.frames_per_sheet)
    except (KeyError, TypeError, ValueError, OverflowError):
        return False
    try:
        if layout_for_frame(layout.frame_width, layout.frame_height) != layout:
            return False
        expected = {
            "schema_version": SCHEMA_VERSION,
            "source_size": stat.st_size,
            "source_mtime_ns": stat.st_mtime_ns,
            "interval_seconds": interval_seconds(duration, frame_capacity(layout)),
            "frame_count": math.ceil(duration / interval),
        }
    except (ValueError, ZeroDivisionError):
        return False
    if any(manifest.get(key) != value for key, value in expected.items()):
        return False
    if request.resolution is not None:
        if (layout.frame_width, layout.frame_height) != request.resolution:
            return False
        if manifest.get("scale_divisor") is not None:
            return False
    elif request.target_width is not None:
        if layout.frame_width != request.target_width:
            return False
        if manifest.get("scale_divisor") is not None:
            return False
    elif (
        type(manifest.get("scale_divisor")) is not int
        or manifest["scale_divisor"] != request.scale_divisor
    ):
        return False
    if revision != asset_revision(
        stat,
        duration,
        interval,
        layout,
        request.scale_divisor,
        request.target_width,
        sampling_mode,
    ):
        return False
    if not re.fullmatch(r"[0-9a-f]{16}", revision) or not 1 <= sheet_count <= MAX_SHEETS:
        return False
    sheets = [
        preview_directory(source) / f"sheet-{revision}-{index:04}.jpg"
        for index in range(sheet_count)
    ]
    try:
        if not all(
            path.is_file() and 0 < path.stat().st_size <= MAX_SHEET_BYTES
            for path in sheets
        ):
            return False
    except OSError:
        return False
    expected_dimensions = (
        layout.frame_width * layout.columns,
        layout.frame_height * layout.rows,
    )
    # The generator publishes the manifest only after every sheet validates.
    # Check both boundaries on later audits without rereading gigabytes of
    # immutable JPEGs; rustyDLNA validates each requested sheet before serving.
    return jpeg_dimensions(sheets[0]) == expected_dimensions and (
        len(sheets) == 1 or jpeg_dimensions(sheets[-1]) == expected_dimensions
    )


def terminate_process(process: subprocess.Popen[bytes], *, process_group: bool = False) -> None:
    if process.poll() is not None and not process_group:
        return
    try:
        if process_group:
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
    finally:
        if process_group:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.wait()


def acquire_lock_interruptibly(lock: object, stop_event: threading.Event) -> None:
    while not stop_event.is_set():
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return
        except BlockingIOError:
            stop_event.wait(0.25)
    raise PreviewInterrupted


def ffmpeg_has_cuda_pipeline(ffmpeg: str) -> bool:
    try:
        hwaccels = subprocess.run(
            [ffmpeg, "-hide_banner", "-hwaccels"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=10,
        )
        filters = subprocess.run(
            [ffmpeg, "-hide_banner", "-filters"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=10,
        )
        device = subprocess.run(
            [
                ffmpeg,
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-init_hw_device",
                "cuda=preview",
                "-f",
                "lavfi",
                "-i",
                "nullsrc=s=16x16:d=0.01",
                "-frames:v",
                "1",
                "-f",
                "null",
                "-",
            ],
            check=False,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return (
        hwaccels.returncode == 0
        and filters.returncode == 0
        and device.returncode == 0
        and b"cuda" in hwaccels.stdout.split()
        and b"scale_cuda" in filters.stdout
        and b"hwdownload" in filters.stdout
    )


def probe_media(
    source: Path,
    ffprobe: str,
    stop_event: threading.Event,
) -> tuple[float, int, int]:
    process = subprocess.Popen(
        [
            ffprobe,
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=duration,width,height:"
            "stream_tags=DURATION,rotate:stream_side_data=rotation",
            "-select_streams",
            "v:0",
            "-of",
            "json",
            "--",
            str(source),
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    deadline = time.monotonic() + 60
    while True:
        if stop_event.is_set():
            terminate_process(process)
            raise PreviewInterrupted
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            terminate_process(process)
            raise RuntimeError("ffprobe exceeded its 60s deadline")
        try:
            stdout, stderr = process.communicate(timeout=min(0.5, remaining))
            break
        except subprocess.TimeoutExpired:
            continue
    if process.returncode != 0:
        detail = stderr[-4096:].decode("utf-8", "replace").strip()
        raise RuntimeError(detail or "ffprobe failed")
    try:
        payload = json.loads(stdout.decode("utf-8"))
        duration = video_stream_duration(payload)
        stream = payload["streams"][0]
        width = int(stream["width"])
        height = int(stream["height"])
        rotation = int(stream.get("tags", {}).get("rotate", 0))
        for side_data in stream.get("side_data_list", []):
            if "rotation" in side_data:
                rotation = int(side_data["rotation"])
                break
    except (
        IndexError,
        KeyError,
        TypeError,
        UnicodeError,
        ValueError,
        json.JSONDecodeError,
    ) as error:
        raise RuntimeError("ffprobe did not return a duration and video dimensions") from error
    interval_seconds(duration)
    if width <= 0 or height <= 0:
        raise RuntimeError("ffprobe returned invalid video dimensions")
    if rotation % 180 != 0:
        width, height = height, width
    return duration, width, height


def asset_revision(
    stat: os.stat_result,
    duration: float,
    interval: int,
    layout: PreviewLayout,
    scale_divisor: int | None,
    target_width: int | None,
    sampling_mode: str,
) -> str:
    value = (
        f"{PROFILE_REVISION}\0{stat.st_size}\0{stat.st_mtime_ns}\0"
        f"{duration:.6f}\0{interval}\0{layout.frame_width}x{layout.frame_height}\0"
        f"{layout.columns}x{layout.rows}\0scale={scale_divisor or 0}"
    )
    if target_width is not None:
        value += f"\0width={target_width}"
    if sampling_mode != "accurate":
        value += f"\0sampling={sampling_mode}"
    return hashlib.sha256(value.encode("ascii")).hexdigest()[:16]


def elapsed_label(seconds: float) -> str:
    total = max(0, round(seconds))
    hours, remainder = divmod(total, 3600)
    minutes, secs = divmod(remainder, 60)
    return f"{hours:d}:{minutes:02d}:{secs:02d}" if hours else f"{minutes:d}:{secs:02d}"


def run_ffmpeg_with_progress(
    command: list[str],
    diagnostics: object,
    timeout: int,
    display_name: str,
    decoder: str,
    sheet_count: int,
    title_started: float,
    stop_event: threading.Event,
) -> int:
    attempt_started = time.monotonic()
    deadline = attempt_started + timeout
    last_report = attempt_started
    completed_sheets = 0
    process = subprocess.Popen(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=diagnostics,
        start_new_session=True,
        bufsize=0,
    )
    if process.stdout is None:
        process.kill()
        process.wait()
        raise RuntimeError("ffmpeg progress pipe was not created")
    selector = selectors.DefaultSelector()
    pending = bytearray()
    dropping_line = False
    try:
        os.set_blocking(process.stdout.fileno(), False)
        selector.register(process.stdout, selectors.EVENT_READ)
        while process.poll() is None:
            if stop_event.is_set():
                raise PreviewInterrupted
            now = time.monotonic()
            if now >= deadline:
                raise RuntimeError(f"ffmpeg exceeded its {timeout}s deadline")
            for key, _ in selector.select(timeout=min(0.1, deadline - now)):
                try:
                    chunk = os.read(key.fd, PROGRESS_READ_BYTES)
                except BlockingIOError:
                    continue
                if not chunk:
                    selector.unregister(key.fileobj)
                # Read at most one bounded chunk per readiness event, so neither
                # an unfinished line nor continuous output delays cancellation.
                segments = chunk.split(b"\n") if chunk else [b"", b""]
                for index, segment in enumerate(segments):
                    if not dropping_line:
                        if len(pending) + len(segment) <= MAX_PROGRESS_LINE_BYTES:
                            pending.extend(segment)
                        else:
                            pending.clear()
                            dropping_line = True
                    if index == len(segments) - 1:
                        continue
                    if not dropping_line:
                        name, separator, value = bytes(pending).strip().partition(b"=")
                        if separator and name == b"frame":
                            try:
                                completed_sheets = max(completed_sheets, min(sheet_count, int(value)))
                            except ValueError:
                                pass
                    pending.clear()
                    dropping_line = False
            now = time.monotonic()
            if now - last_report < PROGRESS_INTERVAL_SECONDS:
                continue
            elapsed = now - title_started
            attempt_elapsed = now - attempt_started
            percent = min(100, completed_sheets * 100 // sheet_count)
            eta = (
                elapsed_label(
                    attempt_elapsed * (sheet_count - completed_sheets) / completed_sheets
                )
                if completed_sheets > 0
                else "unknown"
            )
            print(
                f"PROGRESS\t{display_name}\tdecoder={decoder}\t"
                f"sheets={completed_sheets}/{sheet_count}\t{percent}%\t"
                f"elapsed={elapsed_label(elapsed)}\teta={eta}",
                file=sys.stderr,
                flush=True,
            )
            last_report = now
        if stop_event.is_set():
            raise PreviewInterrupted
        if time.monotonic() >= deadline:
            raise RuntimeError(f"ffmpeg exceeded its {timeout}s deadline")
        return process.wait()
    finally:
        terminate_process(process, process_group=True)
        selector.close()
        process.stdout.close()


def generate_one(
    source: Path,
    ffmpeg: str,
    ffprobe: str,
    force: bool,
    request: PreviewRequest,
    hwaccel: str,
    cuda_pipeline: bool,
    sampling_mode: str,
    display_name: str,
    stop_event: threading.Event,
) -> tuple[Path, str]:
    title_started = time.monotonic()
    print(
        f"START\t{display_name}\tdecoder={hwaccel}\t"
        f"sampling={sampling_mode}\telapsed=0:00",
        file=sys.stderr,
        flush=True,
    )
    if stop_event.is_set():
        raise PreviewInterrupted
    stat = source_stat(source)
    directory = preview_directory(source)
    current = read_manifest(directory)
    if not force and manifest_is_current(source, stat, current, request, sampling_mode):
        return source, "current"

    directory.parent.mkdir(mode=0o777, parents=False, exist_ok=True)
    os.chmod(directory.parent, 0o777)
    directory.mkdir(mode=0o777, parents=False, exist_ok=True)
    os.chmod(directory, 0o777)

    lock_path = directory / ".generate.lock"
    with lock_path.open("a+b") as lock:
        acquire_lock_interruptibly(lock, stop_event)
        if stop_event.is_set():
            raise PreviewInterrupted
        stat = source_stat(source)
        collisions = colliding_video_sources(source)
        if collisions != [source]:
            names = ", ".join(path.name for path in collisions)
            raise RuntimeError(f"multiple same-stem video sources own this preview directory: {names}")
        current = read_manifest(directory)
        if not force and manifest_is_current(source, stat, current, request, sampling_mode):
            return source, "current"

        duration, source_width, source_height = probe_media(source, ffprobe, stop_event)
        frame_width, frame_height = request.frame_size(source_width, source_height)
        layout = layout_for_frame(frame_width, frame_height)
        interval = interval_seconds(duration, frame_capacity(layout))
        frame_count = math.ceil(duration / interval)
        sheet_count = math.ceil(frame_count / layout.frames_per_sheet)
        padding_frames = sheet_count * layout.frames_per_sheet - frame_count
        revision = asset_revision(
            stat,
            duration,
            interval,
            layout,
            request.scale_divisor,
            request.target_width,
            sampling_mode,
        )

        for child in directory.iterdir():
            if TEMP_SHEET_RE.fullmatch(child.name) or TEMP_MANIFEST_RE.fullmatch(child.name):
                child.unlink(missing_ok=True)

        pattern = ffmpeg_sheet_pattern(directory, revision)
        temp_paths = [
            directory / f".sheet-{revision}-{index:04}.tmp.jpg"
            for index in range(sheet_count)
        ]
        timeout = max(600, min(12 * 60 * 60, math.ceil(duration * 2)))
        if hwaccel == "auto" and cuda_pipeline:
            decoder_attempts = [("cuda", True), ("cuda", False), ("none", False)]
        elif hwaccel == "auto":
            decoder_attempts = [("auto", False), ("none", False)]
        elif hwaccel == "cuda" and cuda_pipeline:
            decoder_attempts = [("cuda", True), ("cuda", False)]
        else:
            decoder_attempts = [(hwaccel, False)]
        sampling_attempts = [sampling_mode]
        if sampling_mode == "keyframes":
            sampling_attempts.append("accurate")
        decoder_status = hwaccel
        sampling_status = sampling_mode
        successful = False
        last_failure = "ffmpeg did not produce the complete validated sprite set"
        for sample_attempt in sampling_attempts:
            for decoder, gpu_resident in decoder_attempts:
                for child in directory.iterdir():
                    if TEMP_SHEET_RE.fullmatch(child.name):
                        child.unlink(missing_ok=True)
                if stop_event.is_set():
                    raise PreviewInterrupted
                sample_options = (
                    ["-skip_frame", "nokey"] if sample_attempt == "keyframes" else []
                )
                if decoder == "none":
                    decoder_options: list[str] = []
                else:
                    decoder_options = ["-hwaccel", decoder]
                    if gpu_resident:
                        decoder_options.extend(["-hwaccel_output_format", "cuda"])
                decoder_label = f"{decoder}-resident" if gpu_resident else decoder
                switch_to_accurate = False
                valid_sheets: int | None = None
                with tempfile.TemporaryFile() as diagnostics:
                    try:
                        completed = run_ffmpeg_with_progress(
                            [
                                ffmpeg,
                                "-nostdin",
                                "-hide_banner",
                                "-loglevel",
                                "error",
                                "-nostats",
                                "-stats_period",
                                "2",
                                *sample_options,
                                *decoder_options,
                                "-i",
                                str(source),
                                "-an",
                                "-vf",
                                preview_filter_graph(
                                    layout,
                                    interval,
                                    padding_frames,
                                    duration,
                                    sample_attempt,
                                    gpu_resident,
                                ),
                                "-frames:v",
                                str(sheet_count),
                                "-start_number",
                                "0",
                                "-f",
                                "image2",
                                "-c:v",
                                "mjpeg",
                                "-q:v",
                                "5",
                                "-progress",
                                "pipe:1",
                                pattern,
                            ],
                            diagnostics,
                            timeout,
                            display_name,
                            f"{decoder_label}/{sample_attempt}",
                            sheet_count,
                            title_started,
                            stop_event,
                        )
                    except PreviewInterrupted:
                        for child in directory.iterdir():
                            if TEMP_SHEET_RE.fullmatch(child.name):
                                child.unlink(missing_ok=True)
                        print(
                            f"INTERRUPTED\t{display_name}\t"
                            f"elapsed={elapsed_label(time.monotonic() - title_started)}",
                            file=sys.stderr,
                            flush=True,
                        )
                        raise
                    if completed != 0:
                        diagnostics.seek(0, os.SEEK_END)
                        end = diagnostics.tell()
                        diagnostics.seek(max(0, end - 64 * 1024))
                        detail = diagnostics.read().decode("utf-8", "replace").strip()
                        last_failure = detail or f"ffmpeg exited {completed}"
                    else:
                        valid_sheets = valid_sheet_prefix(
                            temp_paths,
                            (
                                layout.frame_width * layout.columns,
                                layout.frame_height * layout.rows,
                            ),
                        )
                    if completed == 0 and valid_sheets == len(temp_paths):
                        decoder_status = (
                            "software-fallback"
                            if hwaccel == "auto" and decoder == "none"
                            else decoder_label
                        )
                        sampling_status = (
                            "accurate-fallback"
                            if sampling_mode == "keyframes" and sample_attempt == "accurate"
                            else sample_attempt
                        )
                        successful = True
                        break
                    if sample_attempt == "keyframes" and keyframes_require_accurate_fallback(
                        completed, valid_sheets, sheet_count
                    ):
                        switch_to_accurate = True
                        last_failure = (
                            f"keyframe density produced {valid_sheets}/{sheet_count} sheets; "
                            "switching to accurate periodic sampling"
                        )
                    elif completed == 0:
                        produced = (
                            "malformed or noncontiguous"
                            if valid_sheets is None
                            else f"{valid_sheets}/{sheet_count} validated"
                        )
                        last_failure = f"{sample_attempt} sampling produced {produced} sheets"
                print(
                    f"RETRY\t{display_name}\t{last_failure}; trying another safe path\t"
                    f"elapsed={elapsed_label(time.monotonic() - title_started)}",
                    file=sys.stderr,
                    flush=True,
                )
                if switch_to_accurate:
                    break
            if successful:
                break
        if not successful:
            raise RuntimeError(last_failure)

        if stop_event.is_set():
            for child in directory.iterdir():
                if TEMP_SHEET_RE.fullmatch(child.name):
                    child.unlink(missing_ok=True)
            raise PreviewInterrupted
        after = source_stat(source)
        if (after.st_size, after.st_mtime_ns) != (stat.st_size, stat.st_mtime_ns):
            raise RuntimeError("source changed during preview generation; retry after it settles")
        if stop_event.is_set():
            for child in directory.iterdir():
                if TEMP_SHEET_RE.fullmatch(child.name):
                    child.unlink(missing_ok=True)
            raise PreviewInterrupted
        final_paths = [directory / f"sheet-{revision}-{index:04}.jpg" for index in range(sheet_count)]
        for temporary, final in zip(temp_paths, final_paths, strict=True):
            os.chmod(temporary, 0o666)
            os.replace(temporary, final)
        fsync_directory(directory)

        manifest = {
            "schema_version": SCHEMA_VERSION,
            "source_size": stat.st_size,
            "source_mtime_ns": stat.st_mtime_ns,
            "duration_seconds": duration,
            "interval_seconds": interval,
            "frame_width": layout.frame_width,
            "frame_height": layout.frame_height,
            "columns": layout.columns,
            "rows": layout.rows,
            "frame_count": frame_count,
            "asset_revision": revision,
        }
        if request.scale_divisor is not None:
            manifest["scale_divisor"] = request.scale_divisor
        fd, temporary_name = tempfile.mkstemp(prefix=".manifest.", suffix=".tmp", dir=directory)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as output:
                json.dump(manifest, output, indent=2, sort_keys=True)
                output.write("\n")
                output.flush()
                os.fsync(output.fileno())
            os.chmod(temporary_name, 0o666)
            os.replace(temporary_name, directory / MANIFEST_NAME)
            fsync_directory(directory)
        finally:
            Path(temporary_name).unlink(missing_ok=True)

        keep = {path.name for path in final_paths}
        for child in directory.iterdir():
            if GENERATED_SHEET_RE.fullmatch(child.name) and child.name not in keep:
                child.unlink(missing_ok=True)
        fsync_directory(directory)
        return source, (
            f"generated {frame_count} {layout.frame_width}x{layout.frame_height} frames "
            f"every {interval}s in {sheet_count} sheets decoder={decoder_status} "
            f"sampling={sampling_status} "
            f"elapsed={elapsed_label(time.monotonic() - title_started)}"
        )


def collect_catalog_videos(root: Path) -> list[Path]:
    videos: set[Path] = set()
    for relative in MOVIE_SOURCES:
        source = root / relative
        if source.is_dir():
            videos.update(path for path in catalog_movie_items(source) if path.is_file())
    for relative in SHOW_SOURCES:
        source = root / relative
        if not source.is_dir():
            continue
        for directory, subdirectories, filenames in os.walk(source):
            subdirectories[:] = [
                name
                for name in subdirectories
                if not is_preview_container(name)
                and not (Path(directory) / name).is_symlink()
            ]
            for filename in filenames:
                path = Path(directory) / filename
                if path.suffix.casefold() in VIDEO_EXTENSIONS and path.is_file() and not path.is_symlink():
                    videos.add(path)
    return sorted(videos, key=lambda path: os.fsencode(path))


def collect_requested_videos(root: Path, requested: list[Path]) -> list[Path]:
    videos: set[Path] = set()
    root = root.resolve()
    for supplied in requested:
        path = supplied if supplied.is_absolute() else root / supplied
        try:
            resolved = path.resolve(strict=True)
            resolved.relative_to(root)
        except (OSError, ValueError) as error:
            raise ValueError(f"path is outside the library root: {supplied}") from error
        if path.is_symlink():
            raise ValueError(f"generated-view symlinks are not preview sources: {supplied}")
        if resolved.is_file():
            if resolved.suffix.casefold() not in VIDEO_EXTENSIONS:
                raise ValueError(f"not a supported video: {supplied}")
            videos.add(resolved)
            continue
        if not resolved.is_dir():
            raise ValueError(f"not a file or directory: {supplied}")
        for directory, subdirectories, filenames in os.walk(resolved):
            subdirectories[:] = [name for name in subdirectories if not is_preview_container(name)]
            for filename in filenames:
                child = Path(directory) / filename
                if child.suffix.casefold() in VIDEO_EXTENSIONS and child.is_file() and not child.is_symlink():
                    videos.add(child)
    return sorted(videos, key=lambda path: os.fsencode(path))


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate rustyDLNA timeline-preview sidecars.")
    parser.add_argument("paths", nargs="*", type=Path, help="video files/directories; default: catalog sources")
    add_root_argument(parser)
    parser.add_argument("--dry-run", action="store_true", help="report missing/stale previews without decoding")
    parser.add_argument(
        "--summary-only",
        action="store_true",
        help="with --dry-run, print totals without one DRY line per pending video",
    )
    parser.add_argument("--force", action="store_true", help="regenerate even when the current set validates")
    parser.add_argument("--workers", type=int, default=1, help="parallel FFmpeg jobs (1-4; default 1)")
    geometry = parser.add_mutually_exclusive_group()
    geometry.add_argument(
        "--resolution",
        type=parse_resolution,
        metavar="WIDTHxHEIGHT",
        help="exact preview-frame canvas (default: 640x360)",
    )
    geometry.add_argument(
        "--scale",
        type=parse_scale,
        metavar="Nx",
        help="divide each source video width and height by N (for example: 4x)",
    )
    geometry.add_argument(
        "--width",
        type=parse_width,
        metavar="PIXELS",
        help="set every preview frame width and derive its even aspect-ratio height",
    )
    parser.add_argument(
        "--sampling",
        choices=("keyframes", "accurate"),
        default="keyframes",
        help="fast keyframe or exact periodic decoding (default: keyframes)",
    )
    parser.add_argument(
        "--hwaccel",
        choices=("auto", "cuda", "vaapi", "qsv", "vdpau", "none"),
        default="auto",
        help="FFmpeg hardware decoder (default: auto; auto retries in software on failure)",
    )
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--ffprobe", default="ffprobe")
    args = parser.parse_args()
    if args.summary_only and not args.dry_run:
        parser.error("--summary-only requires --dry-run")

    request = PreviewRequest(
        resolution=args.resolution
        if args.resolution is not None
        else (
            (DEFAULT_FRAME_WIDTH, DEFAULT_FRAME_HEIGHT)
            if args.scale is None and args.width is None
            else None
        ),
        scale_divisor=args.scale,
        target_width=args.width,
    )
    if request.resolution is not None:
        try:
            layout_for_frame(*request.resolution)
        except ValueError as error:
            parser.error(str(error))

    root = require_library_root(parser, args.root)
    print("AUDIT\tcollecting catalog videos...", file=sys.stderr, flush=True)
    try:
        videos = collect_requested_videos(root, args.paths) if args.paths else collect_catalog_videos(root)
    except ValueError as error:
        parser.error(str(error))
    print(
        f"AUDIT\tchecking {len(videos)} video sources and existing previews...",
        file=sys.stderr,
        flush=True,
    )
    pending: list[Path] = []
    errors: list[str] = []
    owners: dict[Path, list[Path]] = {}
    for source in videos:
        owners.setdefault(preview_directory(source), []).append(source)
    collisions = {
        directory: sources for directory, sources in owners.items() if len(sources) > 1
    }
    for directory, sources in collisions.items():
        names = ", ".join(str(source.relative_to(root)) for source in sources)
        errors.append(
            f"{directory.relative_to(root)} has multiple same-stem sources: {names}"
        )
    colliding_sources = {source for sources in collisions.values() for source in sources}
    videos = [source for source in videos if source not in colliding_sources]
    audit_started = time.monotonic()
    last_audit_report = audit_started
    for index, source in enumerate(videos, start=1):
        try:
            stat = source_stat(source)
            siblings = colliding_video_sources(source)
            if siblings != [source]:
                names = ", ".join(path.name for path in siblings)
                raise ValueError(
                    f"multiple same-stem video sources own {preview_directory(source).name}: {names}"
                )
            if args.force or not manifest_is_current(
                source,
                stat,
                read_manifest(preview_directory(source)),
                request,
                args.sampling,
            ):
                pending.append(source)
        except (OSError, ValueError) as error:
            errors.append(f"{source}: {error}")
        now = time.monotonic()
        if now - last_audit_report >= PROGRESS_INTERVAL_SECONDS:
            print(
                f"AUDIT\tchecked={index}/{len(videos)} pending={len(pending)} "
                f"elapsed={elapsed_label(now - audit_started)}",
                file=sys.stderr,
                flush=True,
            )
            last_audit_report = now

    print(f"preview targets: {len(videos)} pending: {len(pending)} invalid: {len(errors)}", file=sys.stderr)
    if args.dry_run:
        if not args.summary_only:
            for source in pending:
                print(f"DRY\t{source.relative_to(root)}")
        for error in errors:
            print(f"ERROR\t{error}", file=sys.stderr)
        return 1 if errors else 0
    if shutil.which(args.ffmpeg) is None or shutil.which(args.ffprobe) is None:
        parser.error("ffmpeg and ffprobe must be installed")

    workers = max(1, min(args.workers, 4))
    generation_failures = 0
    cuda_pipeline = args.hwaccel in {"auto", "cuda"} and ffmpeg_has_cuda_pipeline(
        args.ffmpeg
    )
    if args.hwaccel in {"auto", "cuda"}:
        print(
            "CUDA-resident preview filtering: "
            + ("available" if cuda_pipeline else "unavailable; using compatible fallback"),
            file=sys.stderr,
        )
    stop_event = threading.Event()
    executor = concurrent.futures.ThreadPoolExecutor(max_workers=workers)
    futures: dict[concurrent.futures.Future[tuple[Path, str]], Path] = {}
    interrupted = False
    try:
        futures = {
            executor.submit(
                generate_one,
                source,
                args.ffmpeg,
                args.ffprobe,
                args.force,
                request,
                args.hwaccel,
                cuda_pipeline,
                args.sampling,
                str(source.relative_to(root)),
                stop_event,
            ): source
            for source in pending
        }
        for future in concurrent.futures.as_completed(futures):
            source = futures[future]
            try:
                _, status = future.result()
                print(f"OK\t{source.relative_to(root)}\t{status}")
            except Exception as error:  # report each title without abandoning the rest
                generation_failures += 1
                errors.append(f"{source.relative_to(root)}: {error}")
                print(f"ERROR\t{source.relative_to(root)}\t{error}", file=sys.stderr)
    except KeyboardInterrupt:
        interrupted = True
        stop_event.set()
        for future in futures:
            future.cancel()
        print("Stopping active preview work...", file=sys.stderr, flush=True)
    finally:
        executor.shutdown(wait=True, cancel_futures=True)
    if interrupted:
        print("Preview generation interrupted; completed previews were preserved.", file=sys.stderr)
        return 130
    print(f"generated: {len(pending) - generation_failures} failed: {len(errors)}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    try:
        exit_code = main()
    except KeyboardInterrupt:
        print("Preview generation interrupted.", file=sys.stderr)
        exit_code = 130
    raise SystemExit(exit_code)
