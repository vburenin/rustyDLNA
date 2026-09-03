#!/usr/bin/env python3
"""Make Dolby Vision Profile 7 sources playable on Google Streamer.

The normal path is lossless for video: extract the authored HDR10/HDR10+
base layer, remove only the unsupported Dolby Vision enhancement layer and
RPU, and remux the unchanged HEVC Main 10 video as hvc1 MP4. A lossy NVENC
transcode is available only as an explicit fallback.

The Streamer file stays in the catalog. The Profile 7 source is moved to
to-review/P7-Recoded-For-Streamer/ and remains available for future rebuilds.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

sys.dont_write_bytecode = True

from lib.dv_profile7 import (
    YEAR_RE,
    ffprobe_banner,
    ffprobe_json,
    find_profile7,
    inspect_profile7,
    matching_streamer_sibling,
    parse_dovi_profile,
    streamer_filename,
)
from lib.paths import add_root_argument, require_library_root


ARCHIVE_DIRNAME = "to-review/P7-Recoded-For-Streamer"
MANIFEST_NAME = "manifest.tsv"
BUILD_MANIFEST_NAME = "streamer-build-manifest.tsv"
VIDEO_BITRATE = "20M"
VIDEO_MAXRATE = "25M"
VIDEO_BUFSIZE = "50M"
AUDIO_BITRATE = "448k"
DURATION_TOLERANCE_SECONDS = 2.0
MP4_AUDIO_CODECS = {"aac", "ac3", "eac3"}
HDR10_PLUS_MARKER = "HDR Dynamic Metadata SMPTE2094-40"


@dataclass(frozen=True)
class Job:
    source: Path
    dest: Path
    already_archived: bool = False


def stream_language(stream: dict) -> str:
    tags = stream.get("tags") or {}
    return str(tags.get("language") or tags.get("LANGUAGE") or "").casefold()


def stream_title(stream: dict) -> str:
    tags = stream.get("tags") or {}
    return str(tags.get("title") or tags.get("TITLE") or "").casefold()


def choose_audio_streams(streams: list[dict]) -> list[dict]:
    audio = [stream for stream in streams if stream.get("codec_type") == "audio"]
    if not audio:
        return []

    def score(stream: dict) -> tuple:
        language = stream_language(stream)
        codec = str(stream.get("codec_name") or "")
        channels = int(stream.get("channels") or 0)
        return (
            "comment" not in stream_title(stream),
            language in {"eng", "en"},
            language in {"rus", "ru"},
            codec in MP4_AUDIO_CODECS,
            min(channels, 6),
            -audio.index(stream),
        )

    ranked = sorted(audio, key=score, reverse=True)
    chosen: list[dict] = []
    seen_languages: set[str] = set()
    for stream in ranked:
        language = stream_language(stream) or f"idx{stream.get('index')}"
        if language in seen_languages:
            continue
        if len(chosen) >= 2:
            break
        chosen.append(stream)
        seen_languages.add(language)
    return chosen or [audio[0]]


def choose_subtitle_stream(streams: list[dict]) -> dict | None:
    text_codecs = {"subrip", "srt", "mov_text", "ass", "ssa", "webvtt"}
    candidates = [
        stream
        for stream in streams
        if stream.get("codec_type") == "subtitle"
        and str(stream.get("codec_name") or "") in text_codecs
    ]
    if not candidates:
        return None
    english = [
        stream for stream in candidates if stream_language(stream) in {"eng", "en"}
    ]
    return english[0] if english else candidates[0]


def append_audio_options(command: list[str], audio: list[dict]) -> list[str]:
    modes: list[str] = []
    for output_index, stream in enumerate(audio):
        codec = str(stream.get("codec_name") or "")
        language = stream_language(stream) or f"stream-{stream.get('index')}"
        if codec in MP4_AUDIO_CODECS:
            command.extend([f"-c:a:{output_index}", "copy"])
            modes.append(f"{language}:{codec}-copy")
        else:
            command.extend(
                [
                    f"-c:a:{output_index}",
                    "ac3",
                    f"-b:a:{output_index}",
                    AUDIO_BITRATE,
                    f"-ac:a:{output_index}",
                    "6",
                ]
            )
            modes.append(f"{language}:{codec}-to-ac3")
    return modes


def build_transcode_command(source: Path, dest: Path) -> tuple[list[str], list[str]]:
    info = ffprobe_json(source)
    streams = info.get("streams") or []
    audio = choose_audio_streams(streams)
    subtitle = choose_subtitle_stream(streams)

    command = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-y",
        "-i",
        str(source),
        "-map",
        "0:v:0",
    ]
    for stream in audio:
        command.extend(["-map", f"0:{stream['index']}"])
    if subtitle is not None:
        command.extend(["-map", f"0:{subtitle['index']}"])
    command.extend(
        [
            "-c:v",
            "hevc_nvenc",
            "-preset",
            "p5",
            "-tune",
            "hq",
            "-profile:v",
            "main10",
            "-pix_fmt",
            "p010le",
            "-rc",
            "vbr",
            "-b:v",
            VIDEO_BITRATE,
            "-maxrate",
            VIDEO_MAXRATE,
            "-bufsize",
            VIDEO_BUFSIZE,
            "-spatial-aq",
            "1",
            "-rc-lookahead",
            "20",
            "-tag:v",
            "hvc1",
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
            "-colorspace",
            "bt2020nc",
            "-color_range",
            "tv",
        ]
    )
    audio_modes = append_audio_options(command, audio)
    if subtitle is not None:
        command.extend(["-c:s", "mov_text"])
    command.extend(
        ["-movflags", "+faststart", "-map_chapters", "0", "-f", "mp4", str(dest)]
    )
    return command, audio_modes


def build_lossless_mux_command(
    base_layer: Path, source: Path, dest: Path
) -> tuple[list[str], list[str]]:
    info = ffprobe_json(source)
    streams = info.get("streams") or []
    audio = choose_audio_streams(streams)
    subtitle = choose_subtitle_stream(streams)

    command = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-y",
        "-fflags",
        "+genpts",
        "-i",
        str(base_layer),
        "-i",
        str(source),
        "-map",
        "0:v:0",
    ]
    for stream in audio:
        command.extend(["-map", f"1:{stream['index']}"])
    if subtitle is not None:
        command.extend(["-map", f"1:{subtitle['index']}"])
    command.extend(["-c:v", "copy", "-tag:v", "hvc1"])
    audio_modes = append_audio_options(command, audio)
    if subtitle is not None:
        command.extend(["-c:s", "mov_text"])
    command.extend(
        [
            "-map_metadata",
            "1",
            "-map_chapters",
            "1",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            str(dest),
        ]
    )
    return command, audio_modes


def find_dovi_tool(_library_root: Path) -> Path:
    configured = os.environ.get("DOVI_TOOL", "").strip()
    if configured:
        path = Path(configured).expanduser()
        if path.is_file():
            return path
        raise RuntimeError(f"DOVI_TOOL is not an executable file: {path}")
    executable = shutil.which("dovi_tool")
    if executable:
        return Path(executable)
    raise RuntimeError(
        "dovi_tool is required for lossless Profile 7 base-layer extraction; "
        "install it on PATH or set DOVI_TOOL; refusing to substitute a lossy transcode"
    )


def extract_hdr10_base_layer(source: Path, output: Path, dovi_tool: Path) -> None:
    if output.exists():
        output.unlink()
    extractor = subprocess.Popen(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostats",
            "-i",
            str(source),
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-bsf:v",
            "hevc_mp4toannexb",
            "-f",
            "hevc",
            "-",
        ],
        stdout=subprocess.PIPE,
    )
    if extractor.stdout is None:
        extractor.kill()
        raise RuntimeError("could not open the HEVC extraction pipe")
    remover: subprocess.CompletedProcess[bytes] | None = None
    try:
        remover = subprocess.run(
            [str(dovi_tool), "remove", "-", "-o", str(output)],
            stdin=extractor.stdout,
        )
    finally:
        extractor.stdout.close()
    extractor_status = extractor.wait()
    if remover is None or remover.returncode != 0 or extractor_status != 0:
        if output.exists():
            output.unlink()
        remover_status = "not-started" if remover is None else str(remover.returncode)
        raise RuntimeError(
            "lossless Dolby Vision layer removal failed "
            f"(ffmpeg={extractor_status}, dovi_tool={remover_status})"
        )
    if not output.is_file() or output.stat().st_size < 1_000_000:
        raise RuntimeError(f"base-layer extraction is missing or too small: {output}")


def parse_clock_duration(value: str) -> float | None:
    match = re.fullmatch(r"(\d+):(\d+):(\d+(?:\.\d+)?)", value.strip())
    if not match:
        return None
    return int(match.group(1)) * 3600 + int(match.group(2)) * 60 + float(
        match.group(3)
    )


def video_stream(info: dict) -> dict:
    return next(
        (stream for stream in info.get("streams") or [] if stream.get("codec_type") == "video"),
        {},
    )


def video_duration_seconds(info: dict) -> float | None:
    stream = video_stream(info)
    duration = stream.get("duration")
    if duration not in (None, "N/A"):
        try:
            return float(duration)
        except (TypeError, ValueError):
            pass
    tags = stream.get("tags") or {}
    for key in ("DURATION", "duration"):
        parsed = parse_clock_duration(str(tags.get(key) or ""))
        if parsed is not None:
            return parsed
    format_duration = (info.get("format") or {}).get("duration")
    try:
        return float(format_duration) if format_duration is not None else None
    except (TypeError, ValueError):
        return None


def video_packet_count(path: Path) -> int | None:
    result = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            str(path),
        ],
        capture_output=True,
        timeout=900,
    )
    try:
        return int((result.stdout or b"").decode("ascii", "replace").strip())
    except ValueError:
        return None


def has_hdr10plus(path: Path) -> bool:
    result = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-read_intervals",
            "%+2",
            "-select_streams",
            "v:0",
            "-show_frames",
            "-show_entries",
            "frame=side_data_list",
            "-of",
            "json",
            str(path),
        ],
        capture_output=True,
    )
    return HDR10_PLUS_MARKER in (result.stdout or b"").decode("utf-8", "replace")


def verify_streamer(source: Path, dest: Path, lossless_video: bool) -> None:
    if not dest.is_file() or dest.stat().st_size < 1_000_000:
        raise RuntimeError(f"Streamer output is missing or too small: {dest}")
    source_info = ffprobe_json(source)
    dest_info = ffprobe_json(dest)
    source_video = video_stream(source_info)
    dest_video = video_stream(dest_info)
    if dest_video.get("codec_name") != "hevc":
        raise RuntimeError(f"Streamer output is not HEVC: {dest}")
    if str(dest_video.get("codec_tag_string") or "").casefold() != "hvc1":
        raise RuntimeError(f"Streamer output is not tagged hvc1: {dest}")
    if "10" not in str(dest_video.get("pix_fmt") or ""):
        raise RuntimeError(f"Streamer output is not 10-bit: {dest}")
    if dest_video.get("color_transfer") != "smpte2084" or dest_video.get(
        "color_primaries"
    ) != "bt2020":
        raise RuntimeError(f"Streamer output is missing HDR10 color metadata: {dest}")
    dovi = parse_dovi_profile(ffprobe_banner(dest))
    if dovi is not None and dovi["profile"] == "7":
        raise RuntimeError(f"Streamer output still has Dolby Vision Profile 7: {dest}")
    if has_hdr10plus(source) and not has_hdr10plus(dest):
        raise RuntimeError(f"Streamer output lost source HDR10+ metadata: {dest}")

    source_duration = video_duration_seconds(source_info)
    dest_duration = video_duration_seconds(dest_info)
    if source_duration and dest_duration:
        if abs(source_duration - dest_duration) > DURATION_TOLERANCE_SECONDS:
            source_packets = video_packet_count(source) if lossless_video else None
            dest_packets = video_packet_count(dest) if lossless_video else None
            if source_packets is None or source_packets != dest_packets:
                raise RuntimeError(
                    f"Streamer video duration {dest_duration:.2f}s differs from "
                    f"source video {source_duration:.2f}s; packet counts "
                    f"source={source_packets!r} output={dest_packets!r}"
                )
    if lossless_video:
        for field in ("width", "height", "pix_fmt", "color_space", "color_transfer", "color_primaries"):
            if source_video.get(field) != dest_video.get(field):
                raise RuntimeError(
                    f"lossless output changed video {field}: "
                    f"{source_video.get(field)!r} -> {dest_video.get(field)!r}"
                )


def archive_name(source: Path, library_root: Path) -> str:
    try:
        relative = source.relative_to(library_root)
        if YEAR_RE.search(source.stem):
            return source.name
        return str(relative).replace("/", " -- ")
    except ValueError:
        return source.name


def append_manifest(archive_dir: Path, row: dict[str, str]) -> None:
    manifest = archive_dir / MANIFEST_NAME
    write_header = not manifest.is_file()
    with manifest.open("a", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(
            handle,
            fieldnames=[
                "archived_at",
                "source",
                "archived_as",
                "streamer",
                "size_bytes",
                "duration",
            ],
            delimiter="\t",
        )
        if write_header:
            writer.writeheader()
        writer.writerow(row)


def append_build_manifest(archive_dir: Path, row: dict[str, str]) -> None:
    manifest = archive_dir / BUILD_MANIFEST_NAME
    write_header = not manifest.is_file()
    with manifest.open("a", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(
            handle,
            fieldnames=[
                "built_at",
                "source",
                "streamer",
                "video_method",
                "audio_methods",
                "source_size_bytes",
                "streamer_size_bytes",
            ],
            delimiter="\t",
        )
        if write_header:
            writer.writeheader()
        writer.writerow(row)


def move_preserving_source(source: Path, dest: Path) -> None:
    """Rename media, using the library's admin path for root-owned intake."""
    try:
        source.replace(dest)
    except PermissionError:
        completed = subprocess.run(
            ["sudo", "-n", "mv", "--", str(source), str(dest)]
        )
        if completed.returncode != 0:
            raise


def replace_verified_output(source: Path, dest: Path) -> None:
    """Atomically install a verified derivative, including over root-owned files."""
    try:
        source.replace(dest)
    except PermissionError:
        completed = subprocess.run(
            ["sudo", "-n", "mv", "-f", "--", str(source), str(dest)]
        )
        if completed.returncode != 0:
            raise


def run_lossless_build(
    source: Path, temp_out: Path, archive_dir: Path, library_root: Path
) -> list[str]:
    dovi_tool = find_dovi_tool(library_root)
    base_layer = archive_dir / "tmp" / f"{temp_out.stem}.base-layer.hevc"
    try:
        print(f"extracting HDR10 base layer: {source.name}", flush=True)
        extract_hdr10_base_layer(source, base_layer, dovi_tool)
        command, audio_modes = build_lossless_mux_command(base_layer, source, temp_out)
        completed = subprocess.run(command)
        if completed.returncode != 0:
            raise RuntimeError(
                f"lossless MP4 remux failed for {source} (exit {completed.returncode})"
            )
        verify_streamer(source, temp_out, lossless_video=True)
        return audio_modes
    finally:
        if base_layer.exists():
            base_layer.unlink()


def run_transcode_build(source: Path, temp_out: Path) -> list[str]:
    command, audio_modes = build_transcode_command(source, temp_out)
    completed = subprocess.run(command)
    if completed.returncode != 0:
        raise RuntimeError(f"ffmpeg failed for {source} (exit {completed.returncode})")
    verify_streamer(source, temp_out, lossless_video=False)
    return audio_modes


def recode_one(
    job: Job,
    library_root: Path,
    archive_dir: Path,
    dry_run: bool,
    replace_existing: bool,
    force_transcode: bool,
    allow_transcode_fallback: bool,
) -> str:
    source, dest = job.source, job.dest
    if not source.is_file():
        raise FileNotFoundError(source)
    if dest.exists() and dest.resolve() == source.resolve():
        raise RuntimeError(f"refusing to overwrite source in place: {source}")

    if not job.already_archived:
        existing = matching_streamer_sibling(source)
        if existing is not None and dest.resolve() != existing.resolve():
            dest = existing

    if dest.exists() and not replace_existing:
        if job.already_archived:
            return f"keep existing Streamer: {dest}"
        action = "archive-only"
        if dry_run:
            return f"DRY {action}: keep {dest.name}; archive {source.name}"
        archived = archive_dir / archive_name(source, library_root)
        if archived.exists():
            raise RuntimeError(f"archive collision: {archived}")
        source_size = source.stat().st_size
        move_preserving_source(source, archived)
        append_manifest(
            archive_dir,
            {
                "archived_at": dt.datetime.now().isoformat(timespec="seconds"),
                "source": str(source.relative_to(library_root)),
                "archived_as": str(archived.relative_to(library_root)),
                "streamer": str(dest.relative_to(library_root)),
                "size_bytes": str(source_size),
                "duration": "",
            },
        )
        return f"{action}: {dest}  archived {archived.name}"

    if dry_run:
        mode = "NVENC transcode" if force_transcode else "lossless HDR10 base-layer copy"
        return f"DRY {mode}: {source} -> {dest}"

    dest.parent.mkdir(parents=True, exist_ok=True)
    temp_dir = archive_dir / "tmp"
    temp_dir.mkdir(parents=True, exist_ok=True)
    temp_out = temp_dir / dest.name
    if temp_out.exists():
        temp_out.unlink()

    source_record = inspect_profile7(source)
    if source_record is None:
        raise RuntimeError(f"source is no longer Dolby Vision Profile 7: {source}")
    source_size = source.stat().st_size
    video_method = "nvenc-transcode" if force_transcode else "lossless-hdr10-base-layer"
    try:
        if force_transcode:
            audio_modes = run_transcode_build(source, temp_out)
        else:
            try:
                audio_modes = run_lossless_build(
                    source, temp_out, archive_dir, library_root
                )
            except Exception:
                if not allow_transcode_fallback:
                    raise
                if temp_out.exists():
                    temp_out.unlink()
                print(
                    f"lossless extraction failed; explicit fallback enabled for {source.name}",
                    file=sys.stderr,
                    flush=True,
                )
                video_method = "nvenc-transcode-fallback"
                audio_modes = run_transcode_build(source, temp_out)
    except Exception:
        if temp_out.exists():
            temp_out.unlink()
        raise

    # The old derived playback file remains intact until the replacement has
    # passed all verification above. Path.replace is atomic on this filesystem.
    replace_verified_output(temp_out, dest)

    archived = source
    if not job.already_archived:
        archived = archive_dir / archive_name(source, library_root)
        if archived.exists():
            raise RuntimeError(f"archive collision: {archived}")
        original_relative = str(source.relative_to(library_root))
        move_preserving_source(source, archived)
        append_manifest(
            archive_dir,
            {
                "archived_at": dt.datetime.now().isoformat(timespec="seconds"),
                "source": original_relative,
                "archived_as": str(archived.relative_to(library_root)),
                "streamer": str(dest.relative_to(library_root)),
                "size_bytes": str(source_size),
                "duration": source_record.get("duration") or "",
            },
        )

    append_build_manifest(
        archive_dir,
        {
            "built_at": dt.datetime.now().isoformat(timespec="seconds"),
            "source": str(archived.relative_to(library_root)),
            "streamer": str(dest.relative_to(library_root)),
            "video_method": video_method,
            "audio_methods": ",".join(audio_modes),
            "source_size_bytes": str(source_size),
            "streamer_size_bytes": str(dest.stat().st_size),
        },
    )
    action = "rebuilt" if job.already_archived else "converted"
    return f"{action} ({video_method}): {dest}  source {archived.name}"


def active_jobs(library_root: Path, dest_map: dict[str, str]) -> list[Job]:
    jobs: list[Job] = []
    if dest_map:
        for relative, destination in dest_map.items():
            source = library_root / relative
            if not source.is_file():
                continue
            try:
                record = inspect_profile7(source)
            except (OSError, subprocess.TimeoutExpired) as error:
                print(f"warning: cannot probe {source}: {error}", flush=True)
                continue
            if record is not None:
                jobs.append(Job(source, library_root / destination))
        return jobs

    for record in find_profile7(library_root):
        source = Path(record["path"])
        relative = str(source.relative_to(library_root))
        if relative in dest_map:
            dest = library_root / dest_map[relative]
        elif source.parent.name.casefold() == "sample":
            continue
        else:
            dest = source.with_name(streamer_filename(source))
        jobs.append(Job(source, dest))
    return jobs


def archived_jobs(library_root: Path, archive_dir: Path) -> list[Job]:
    manifest = archive_dir / MANIFEST_NAME
    if not manifest.is_file():
        return []
    completed_lossless: set[tuple[str, str, int, int]] = set()
    build_manifest = archive_dir / BUILD_MANIFEST_NAME
    if build_manifest.is_file():
        with build_manifest.open(encoding="utf-8", newline="") as handle:
            for row in csv.DictReader(handle, delimiter="\t"):
                if row.get("video_method") != "lossless-hdr10-base-layer":
                    continue
                try:
                    completed_lossless.add(
                        (
                            row["source"],
                            row["streamer"],
                            int(row["source_size_bytes"]),
                            int(row["streamer_size_bytes"]),
                        )
                    )
                except (KeyError, TypeError, ValueError):
                    continue

    jobs: list[Job] = []
    seen: set[tuple[Path, Path]] = set()
    with manifest.open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            source = library_root / row["archived_as"]
            dest = library_root / row["streamer"]
            key = (source, dest)
            completed_key = (
                row["archived_as"],
                row["streamer"],
                source.stat().st_size if source.is_file() else -1,
                dest.stat().st_size if dest.is_file() else -1,
            )
            if completed_key in completed_lossless:
                continue
            if source.is_file() and key not in seen:
                jobs.append(Job(source, dest, already_archived=True))
                seen.add(key)
    return jobs


def load_dest_map(path: Path | None) -> dict[str, str]:
    if path is None or not path.is_file():
        return {}
    payload = json.loads(path.read_text(encoding="utf-8"))
    if isinstance(payload, dict) and "destinations" in payload:
        payload = payload["destinations"]
    if not isinstance(payload, dict):
        raise RuntimeError(f"invalid destination map: {path}")
    return {str(key): str(value) for key, value in payload.items()}


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Losslessly extract HDR10/HDR10+ base layers from Dolby Vision "
            "Profile 7 sources for Google Streamer."
        )
    )
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--dest-map",
        type=Path,
        help="JSON map of library-relative source paths to Streamer destinations",
    )
    parser.add_argument(
        "--only",
        action="append",
        default=[],
        help="limit to this library-relative source path (repeatable)",
    )
    parser.add_argument(
        "--rebuild-archived",
        action="store_true",
        help="also rebuild Streamer files from sources already in the P7 archive",
    )
    parser.add_argument(
        "--replace-existing",
        action="store_true",
        help="atomically replace an existing derived Streamer file after verification",
    )
    parser.add_argument(
        "--force-transcode",
        action="store_true",
        help="explicitly use the lossy NVENC fallback instead of base-layer copying",
    )
    parser.add_argument(
        "--allow-transcode-fallback",
        action="store_true",
        help="allow NVENC only when lossless extraction or verification fails",
    )
    add_root_argument(parser)
    args = parser.parse_args()

    library_root = require_library_root(parser, args.root)
    archive_dir = library_root / ARCHIVE_DIRNAME
    archive_dir.mkdir(parents=True, exist_ok=True)
    dest_map = load_dest_map(args.dest_map)
    jobs = active_jobs(library_root, dest_map)
    if args.rebuild_archived:
        jobs.extend(archived_jobs(library_root, archive_dir))
    if args.only:
        wanted = {item.rstrip("/") for item in args.only}
        jobs = [
            job
            for job in jobs
            if str(job.source.relative_to(library_root)) in wanted
        ]

    if not jobs:
        print("No Dolby Vision Profile 7 files to convert or rebuild.")
        return 0

    failures = 0
    for job in jobs:
        try:
            print(
                recode_one(
                    job,
                    library_root,
                    archive_dir,
                    args.dry_run,
                    args.replace_existing,
                    args.force_transcode,
                    args.allow_transcode_fallback,
                ),
                flush=True,
            )
        except Exception as error:  # noqa: BLE001 - batch must continue
            failures += 1
            print(f"error: {job.source}: {error}", file=sys.stderr, flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
