"""Detect Dolby Vision Profile 7 files that Google Streamer cannot play."""

from __future__ import annotations

import json
import os
import re
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path


VIDEO_EXTENSIONS = {
    ".avi",
    ".m2ts",
    ".m4v",
    ".mkv",
    ".mov",
    ".mp4",
    ".ts",
    ".webm",
}

DEFAULT_PROBE_WORKERS = min(8, os.cpu_count() or 4)

# Playback catalogs and loose intake. Generated views, downloads, and
# already-archived remuxes are not candidates for a new Streamer recode.
SKIP_TOP_LEVEL = {
    "audio-books",
    "drone",
    "dupes",
    "genres",
    "incomplete",
    "sport",
    "Sport",
}

SKIP_DIR_NAMES = {
    "BACKUP",
    "AUXDATA",
    "JAR",
    "BDJO",
    "META",
    "CLIPINF",
    "PLAYLIST",
    "CERTIFICATE",
    "Sample",
    "sample",
    "P7-Recoded-For-Streamer",
}

YEAR_RE = re.compile(r"\(((?:18|19|20)\d{2})\)")
DOVI_RE = re.compile(
    r"DOVI configuration record:.*?profile:\s*(\d+)"
    r".*?el flag:\s*(\d+)"
    r".*?compatibility id:\s*(\d+)",
    re.S,
)
DURATION_RE = re.compile(
    r"Duration:\s*(\d+):(\d+):(\d+(?:\.\d+)?).*bitrate:\s*(\d+)\s*kb/s"
)
EDITION_WORDS = {
    "extended",
    "imax",
    "open",
    "matte",
    "theatrical",
    "unrated",
    "hybrid",
}


def is_disc_directory(path: Path) -> bool:
    return (path / "BDMV" / "index.bdmv").is_file() or (
        path / "VIDEO_TS" / "VIDEO_TS.IFO"
    ).is_file()


def iter_video_files(library_root: Path):
    for child in sorted(library_root.iterdir(), key=lambda item: item.name.casefold()):
        if child.name.startswith("."):
            continue
        if (
            child.is_file()
            and child.suffix.casefold() in VIDEO_EXTENSIONS
            and ".partial" not in child.name.casefold()
            and ".recode-tmp." not in child.name.casefold()
        ):
            yield child
            continue
        if (
            not child.is_dir()
            or child.name in SKIP_TOP_LEVEL
            or is_disc_directory(child)
        ):
            continue
        for directory, subdirectories, filenames in os.walk(child):
            directory_path = Path(directory)
            subdirectories[:] = [
                name
                for name in subdirectories
                if name not in SKIP_DIR_NAMES and not name.startswith(".")
            ]
            for name in list(subdirectories):
                if is_disc_directory(directory_path / name):
                    subdirectories.remove(name)
            for filename in filenames:
                path = directory_path / filename
                if (
                    path.is_file()
                    and not path.is_symlink()
                    and path.suffix.casefold() in VIDEO_EXTENSIONS
                    and ".partial" not in path.name.casefold()
                    and ".recode-tmp." not in path.name.casefold()
                ):
                    yield path


def ffprobe_banner(path: Path, timeout: int = 90) -> str:
    result = subprocess.run(
        ["ffprobe", "-hide_banner", str(path)],
        capture_output=True,
        timeout=timeout,
    )
    stderr = (result.stderr or b"").decode("utf-8", "replace")
    stdout = (result.stdout or b"").decode("utf-8", "replace")
    return f"{stderr}\n{stdout}"


def parse_dovi_profile(banner: str) -> dict[str, str] | None:
    match = DOVI_RE.search(banner)
    if not match:
        fallback = re.search(r"dv_profile=(\d+)", banner)
        if not fallback:
            return None
        return {"profile": fallback.group(1), "el": "", "compat": ""}
    return {
        "profile": match.group(1),
        "el": match.group(2),
        "compat": match.group(3),
    }


def parse_duration_seconds(banner: str) -> float | None:
    match = DURATION_RE.search(banner)
    if not match:
        return None
    hours, minutes, seconds = match.group(1), match.group(2), match.group(3)
    return int(hours) * 3600 + int(minutes) * 60 + float(seconds)


def matching_streamer_sibling(path: Path) -> Path | None:
    parsed = YEAR_RE.search(path.stem)
    prefix = path.stem[: parsed.end()].casefold() if parsed else None
    for sibling in path.parent.iterdir():
        if not sibling.is_file():
            continue
        if "streamer" not in sibling.name.casefold():
            continue
        if sibling.suffix.casefold() not in {".mp4", ".mkv"}:
            continue
        if prefix is None or sibling.stem.casefold().startswith(prefix):
            return sibling
    return None


def streamer_filename(source: Path) -> str:
    matches = list(YEAR_RE.finditer(source.stem))
    if not matches:
        return f"{source.stem} - 2160p HDR10 Streamer.mp4"
    year_match = matches[-1]
    prefix = source.stem[: year_match.end()].strip()
    rest = source.stem[year_match.end() :].strip(" -._")
    editions: list[str] = []
    for token in re.split(r"[\s._-]+", rest):
        if token.casefold() in EDITION_WORDS:
            if editions and editions[-1].casefold() == "open" and token.casefold() == "matte":
                editions[-1] = "Open Matte"
            else:
                editions.append(token.capitalize() if token.casefold() != "imax" else "IMAX")
    if editions:
        return f"{prefix} - {' '.join(editions)} 2160p HDR10 Streamer.mp4"
    return f"{prefix} - 2160p HDR10 Streamer.mp4"


def inspect_profile7(path: Path) -> dict | None:
    banner = ffprobe_banner(path)
    dovi = parse_dovi_profile(banner)
    if dovi is None or dovi["profile"] != "7":
        return None
    duration_match = DURATION_RE.search(banner)
    sibling = matching_streamer_sibling(path)
    video_line = next(
        (line.strip() for line in banner.splitlines() if "Video:" in line),
        "",
    )
    return {
        "path": str(path),
        "profile": dovi["profile"],
        "el": dovi["el"],
        "compat": dovi["compat"],
        "duration": duration_match.group(0).split(",")[0].replace("Duration:", "").strip()
        if duration_match
        else "",
        "duration_seconds": parse_duration_seconds(banner),
        "bitrate_kbps": int(duration_match.group(4)) if duration_match else None,
        "size_bytes": path.stat().st_size,
        "video": video_line[:200],
        "streamer_sibling": str(sibling) if sibling else "",
    }


def _probe_profile7(path: Path) -> tuple[dict | None, str]:
    try:
        return inspect_profile7(path), ""
    except (OSError, subprocess.TimeoutExpired) as error:
        return None, f"warning: cannot probe {path}: {error}"


def find_profile7(
    library_root: Path, *, workers: int = DEFAULT_PROBE_WORKERS
) -> list[dict]:
    if workers < 1:
        raise ValueError("workers must be at least 1")

    paths = list(iter_video_files(library_root))
    found: list[dict] = []
    with ThreadPoolExecutor(max_workers=workers) as executor:
        # executor.map preserves catalog order while running ffprobe calls in
        # parallel, so hits and warnings remain deterministic.
        for record, warning in executor.map(_probe_profile7, paths):
            if warning:
                print(warning, flush=True)
            if record is not None:
                found.append(record)
    return found


def ffprobe_json(path: Path, timeout: int = 90) -> dict:
    result = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            str(path),
        ],
        capture_output=True,
        timeout=timeout,
    )
    payload = (result.stdout or b"").decode("utf-8", "replace")
    return json.loads(payload or "{}")
