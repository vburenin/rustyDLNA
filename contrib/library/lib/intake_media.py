"""Plan and apply confidence-gated intake of loose media at the library root."""

from __future__ import annotations

import csv
import hashlib
import json
import os
import re
import shutil
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

try:
    from .catalog_config import (
        MOVIE_INTAKE_OVERRIDES,
        MOVIE_SOURCES,
        catalog_item_label,
    )
    from .imdb_index import Candidate, load_imdb_matches, normalized_title
    from .paths import state_dir
    from .tmdb_metadata import TmdbClient, TmdbMovie
except ImportError:  # direct execution/import from scripts/lib workflows
    from catalog_config import MOVIE_INTAKE_OVERRIDES, MOVIE_SOURCES, catalog_item_label
    from imdb_index import Candidate, load_imdb_matches, normalized_title
    from paths import state_dir
    from tmdb_metadata import TmdbClient, TmdbMovie


VIDEO_EXTENSIONS = {
    ".avi",
    ".m4v",
    ".mkv",
    ".mov",
    ".mp4",
    ".mpeg",
    ".mpg",
    ".ts",
    ".webm",
    ".wmv",
}
SIDECAR_EXTENSIONS = {".ass", ".jpg", ".nfo", ".png", ".srt", ".ssa", ".sub"}
YEAR_RE = re.compile(r"(?<!\d)((?:18|19|20)\d{2})(?!\d)")
PAREN_YEAR_RE = re.compile(r"\(((?:18|19|20)\d{2})\)")
EPISODE_RE = re.compile(r"\bS\d{1,2}E\d{1,3}\b", re.IGNORECASE)
LEADING_NUMBER_RE = re.compile(r"^\d{1,3}\s*-\s*")

GENRE_HOMES = {
    "action": "action",
    "comedy": "comedy",
    "drama": "drama",
    "fantasy": "fantasy",
    "sci-fi": "sci-fi",
}
CATALOG_PRIORITY = ("sci-fi", "fantasy", "action", "comedy", "drama")
SOURCE_TAGS = (
    (
        re.compile(
            r"\b(?:bd[ ._-]*remux|blu[ ._-]?ray[ ._-]+remux)\b",
            re.I,
        ),
        "BDRemux",
    ),
    (re.compile(r"\bremux\b", re.I), "Remux"),
    (re.compile(r"\b(?:web[ ._-]?dl|webdl)\b", re.I), "WEB-DL"),
    (re.compile(r"\bweb[ ._-]?rip\b", re.I), "WEBRip"),
    (re.compile(r"\bhd[ ._-]?dvd\b", re.I), "HD-DVD"),
    (re.compile(r"\bblu[ ._-]?ray\b", re.I), "BluRay"),
    (re.compile(r"\bbd[ ._-]?rip\b", re.I), "BDRip"),
    (re.compile(r"\bhdtv(?:rip)?\b", re.I), "HDTV"),
)
EDITION_TAGS = (
    (re.compile(r"\bextended\b", re.I), "Extended"),
    (re.compile(r"\bdirector(?:'|’)?s[ ._-]?cut\b", re.I), "Director's Cut"),
    (re.compile(r"\bfinal[ ._-]?cut\b", re.I), "Final Cut"),
    (re.compile(r"\bredux\b", re.I), "Redux"),
    (re.compile(r"\bremaster(?:ed)?\b", re.I), "Remastered"),
    (re.compile(r"\buncut\b", re.I), "Uncut"),
    (re.compile(r"\bunrated\b", re.I), "Unrated"),
    (re.compile(r"\btheatrical\b", re.I), "Theatrical"),
    (re.compile(r"\bimax\b", re.I), "IMAX"),
    (re.compile(r"\bopen[ ._-]?matte\b", re.I), "Open Matte"),
)
AI_UPSCALE_RE = re.compile(
    r"(?:(?<![A-Z0-9])AI[ ._-]*(?:UP|AP)SCALE(?![A-Z0-9])|"
    r"(?<![A-Z0-9])(?:4K|2160P)[ ._-]+AI(?![A-Z0-9])|"
    r"(?<![A-Z0-9])AI[ ._-]+(?:4K|2160P)(?![A-Z0-9]))",
    re.I,
)
SOURCE_QUALITY = {
    "": 0,
    "HDTV": 1,
    "WEBRip": 2,
    "BDRip": 3,
    "WEB-DL": 3,
    "HD-DVD": 4,
    "BluRay": 4,
    "Remux": 5,
    "BDRemux": 5,
}
LOSSLESS_AUDIO_CODECS = {
    "alac",
    "dts",
    "flac",
    "mlp",
    "pcm_bluray",
    "pcm_s16le",
    "pcm_s24le",
    "truehd",
}


@dataclass(frozen=True)
class MediaProbe:
    title: str
    duration_seconds: float
    width: int
    height: int
    codec: str
    color_transfer: str
    color_primaries: str
    bit_rate: int = 0
    audio_channels: int = 0
    audio_codecs: tuple[str, ...] = ()
    dv_profile: int = 0


@dataclass(frozen=True)
class IdentityHint:
    title: str
    year: int
    source: str


@dataclass(frozen=True)
class MovieIdentity:
    imdb_id: str
    title: str
    year: int
    genres: tuple[str, ...]
    match_method: str
    hint_source: str


@dataclass(frozen=True)
class IntakePlan:
    source: Path
    destination: Path
    identity: MovieIdentity
    tmdb: TmdbMovie | None
    confidence: int
    evidence: tuple[str, ...]
    mappings: tuple[tuple[Path, Path], ...]
    action: str = "catalog"
    incumbent: Path | None = None
    archived_path: Path | None = None
    quality_summary: str = ""


@dataclass(frozen=True)
class IntakeIssue:
    source: Path
    reason: str


@dataclass(frozen=True)
class QualityDecision:
    winner: str | None
    reason: str
    incoming_summary: str
    existing_summary: str


def root_video_candidates(library_root: Path) -> list[Path]:
    return sorted(
        (
            entry
            for entry in library_root.iterdir()
            if entry.is_file()
            and not entry.is_symlink()
            and not entry.name.startswith((".", "._"))
            and entry.suffix.casefold() in VIDEO_EXTENSIONS
        ),
        key=lambda entry: entry.name.casefold(),
    )


def _open_for_writing(path: Path) -> bool:
    if shutil.which("lsof") is None:
        return False
    completed = subprocess.run(
        ["lsof", "-F", "f", "--", str(path)],
        capture_output=True,
        text=True,
    )
    return any(
        line.startswith("f") and line[-1:] in {"u", "w"}
        for line in completed.stdout.splitlines()
    )


def settled_candidates(
    paths: list[Path], settle_seconds: float
) -> tuple[list[Path], list[IntakeIssue]]:
    settled: list[Path] = []
    issues: list[IntakeIssue] = []
    initial: dict[Path, tuple[int, int]] = {}
    for path in paths:
        if ".partial" in path.name.casefold():
            issues.append(IntakeIssue(path, "active .partial download"))
            continue
        try:
            stat = path.stat()
        except OSError as error:
            issues.append(IntakeIssue(path, f"cannot stat candidate: {error}"))
            continue
        if _open_for_writing(path):
            issues.append(IntakeIssue(path, "file is open for writing"))
            continue
        initial[path] = (stat.st_size, stat.st_mtime_ns)

    if initial and settle_seconds > 0:
        time.sleep(settle_seconds)

    for path, before in initial.items():
        try:
            stat = path.stat()
        except OSError as error:
            issues.append(IntakeIssue(path, f"candidate changed while settling: {error}"))
            continue
        if (stat.st_size, stat.st_mtime_ns) != before or _open_for_writing(path):
            issues.append(IntakeIssue(path, "size, mtime, or writer changed while settling"))
            continue
        settled.append(path)
    return settled, issues


def probe_media(path: Path) -> MediaProbe:
    completed = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
            str(path),
        ],
        capture_output=True,
        text=True,
        timeout=90,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or f"ffprobe exited {completed.returncode}"
        raise RuntimeError(detail)
    payload = json.loads(completed.stdout or "{}")
    streams = payload.get("streams") if isinstance(payload, dict) else None
    video = next(
        (
            stream
            for stream in streams or []
            if isinstance(stream, dict) and stream.get("codec_type") == "video"
        ),
        None,
    )
    if not isinstance(video, dict):
        raise RuntimeError("ffprobe found no video stream")
    audio_streams = [
        stream
        for stream in streams or []
        if isinstance(stream, dict) and stream.get("codec_type") == "audio"
    ]
    media_format = payload.get("format")
    format_data = media_format if isinstance(media_format, dict) else {}
    tags = format_data.get("tags")
    tag_data = tags if isinstance(tags, dict) else {}
    side_data = video.get("side_data_list")
    dolby_vision = next(
        (
            entry
            for entry in side_data or []
            if isinstance(entry, dict)
            and str(entry.get("side_data_type") or "").casefold()
            == "dovi configuration record"
        ),
        {},
    )
    title = next(
        (
            str(value).strip()
            for key, value in tag_data.items()
            if str(key).casefold() == "title" and str(value).strip()
        ),
        "",
    )
    return MediaProbe(
        title=title,
        duration_seconds=float(format_data.get("duration") or 0),
        width=int(video.get("width") or 0),
        height=int(video.get("height") or 0),
        codec=str(video.get("codec_name") or ""),
        color_transfer=str(video.get("color_transfer") or ""),
        color_primaries=str(video.get("color_primaries") or ""),
        bit_rate=int(video.get("bit_rate") or format_data.get("bit_rate") or 0),
        audio_channels=max(
            (int(stream.get("channels") or 0) for stream in audio_streams),
            default=0,
        ),
        audio_codecs=tuple(
            sorted(
                {
                    str(stream.get("codec_name") or "").casefold()
                    for stream in audio_streams
                    if stream.get("codec_name")
                }
            )
        ),
        dv_profile=int(dolby_vision.get("dv_profile") or 0),
    )


def parse_identity_hint(value: str, source: str) -> IdentityHint | None:
    # Prefer an explicitly parenthesized release year. A title may itself
    # begin with a four-digit number, such as "2001: A Space Odyssey".
    match = PAREN_YEAR_RE.search(value) or YEAR_RE.search(value)
    if match is None:
        return None
    title = value[: match.start()]
    title = LEADING_NUMBER_RE.sub("", title)
    title = re.sub(r"[._]+", " ", title)
    title = title.strip(" -–—._()[]{}")
    title = re.sub(r"\s+", " ", title)
    if not title:
        return None
    return IdentityHint(title, int(match.group(1)), source)


def identity_hints(path: Path, probe: MediaProbe) -> list[IdentityHint]:
    values = (
        (probe.title, "embedded-title"),
        (catalog_item_label(path), "filename"),
    )
    hints: list[IdentityHint] = []
    seen: set[tuple[str, int]] = set()
    for value, source in values:
        hint = parse_identity_hint(value, source)
        if hint is None:
            continue
        key = (normalized_title(hint.title), hint.year)
        if key in seen:
            continue
        seen.add(key)
        hints.append(hint)
        if source == "embedded-title" and " / " in hint.title:
            for alternative in hint.title.split(" / "):
                alternative = alternative.strip()
                alternative_key = (normalized_title(alternative), hint.year)
                if not alternative or alternative_key in seen:
                    continue
                seen.add(alternative_key)
                hints.append(
                    IdentityHint(alternative, hint.year, "embedded-title-alternative")
                )
    return hints


def _top_candidates(candidates: list[Candidate], duration: float) -> list[Candidate]:
    usable = candidates
    if duration >= 40 * 60:
        feature_candidates = [
            candidate
            for candidate in candidates
            if candidate[3] >= 3
            or (
                candidate[3] == 2
                and (
                    len(candidate) <= 6
                    or candidate[6] is None
                    or candidate[6] >= 40
                )
            )
        ]
        if not feature_candidates:
            return []
        usable = feature_candidates
    if not usable:
        return []
    ranked = sorted(usable, key=lambda candidate: (candidate[3], -candidate[4]), reverse=True)
    best_rank = (ranked[0][3], -ranked[0][4])
    winners = [
        candidate
        for candidate in ranked
        if (candidate[3], -candidate[4]) == best_rank
    ]
    if len({candidate[0] for candidate in winners}) <= 1 or duration <= 0:
        return winners

    media_minutes = duration / 60
    timed = [
        (abs(int(candidate[6]) - media_minutes), candidate)
        for candidate in winners
        if len(candidate) > 6 and candidate[6] is not None
    ]
    if not timed:
        return winners
    timed.sort(key=lambda item: (item[0], item[1][0]))
    best_difference = timed[0][0]
    best_runtime = [
        candidate for difference, candidate in timed if difference == best_difference
    ]
    next_differences = [
        difference for difference, _candidate in timed if difference > best_difference
    ]
    separation = min(next_differences, default=float("inf")) - best_difference
    if (
        best_difference <= 20
        and separation >= 3
        and len({candidate[0] for candidate in best_runtime}) == 1
    ):
        return best_runtime
    return winners


def _select_candidate(candidates: list[Candidate], duration: float) -> Candidate | None:
    winners = _top_candidates(candidates, duration)
    return winners[0] if len({candidate[0] for candidate in winners}) == 1 else None


def identify_movie(
    hints: list[IdentityHint],
    probe: MediaProbe,
    imdb_index: Path,
    tmdb: TmdbClient | None = None,
) -> MovieIdentity | None:
    keys = {(normalized_title(hint.title), hint.year) for hint in hints}
    matches = load_imdb_matches(imdb_index, keys)
    return identity_from_matches(hints, probe, matches, tmdb=tmdb)


def identity_from_matches(
    hints: list[IdentityHint],
    probe: MediaProbe,
    matches: dict[tuple[str, int], list[Candidate]],
    *,
    tmdb: TmdbClient | None = None,
) -> MovieIdentity | None:
    resolved: list[tuple[IdentityHint, list[Candidate]]] = []
    for hint in hints:
        key = (normalized_title(hint.title), hint.year)
        candidates = _top_candidates(matches.get(key, []), probe.duration_seconds)
        if candidates:
            resolved.append((hint, candidates))
    if not resolved:
        return None
    candidate_sets = [
        {candidate[0] for candidate in candidates}
        for _hint, candidates in resolved
    ]
    common_ids = set.intersection(*candidate_sets)
    candidates_by_id: dict[str, tuple[IdentityHint, Candidate]] = {}
    for hint, candidates in resolved:
        for candidate in candidates:
            if candidate[0] in common_ids:
                candidates_by_id.setdefault(candidate[0], (hint, candidate))
    if len(candidates_by_id) != 1 and tmdb is not None:
        cached_ids: set[str] = set()
        for imdb_id, (_hint, candidate) in candidates_by_id.items():
            cache_key = tmdb._cache_key(imdb_id, candidate[1], _hint.year)
            cached = tmdb.entries.get(cache_key)
            if isinstance(cached, dict) and tmdb._decode_record(cached) is not None:
                cached_ids.add(imdb_id)
        if len(cached_ids) == 1:
            candidates_by_id = {
                imdb_id: candidates_by_id[imdb_id] for imdb_id in cached_ids
            }
    if len(candidates_by_id) != 1:
        return None
    hint, candidate = next(iter(candidates_by_id.values()))
    return MovieIdentity(
        imdb_id=candidate[0],
        title=candidate[1],
        year=hint.year,
        genres=candidate[2],
        match_method=candidate[5],
        hint_source=hint.source,
    )


def movie_title_year(path: Path) -> tuple[str, int] | None:
    label = catalog_item_label(path)
    matches = list(PAREN_YEAR_RE.finditer(label))
    if not matches:
        return None
    match = matches[-1]
    title = LEADING_NUMBER_RE.sub("", label[: match.start()].strip())
    return (title, int(match.group(1))) if title else None


def broken_catalog_targets(library_root: Path) -> dict[tuple[str, int], list[Path]]:
    genres_root = library_root / "genres"
    catalog_roots = [library_root / relative for relative in MOVIE_SOURCES]
    targets: dict[tuple[str, int], set[Path]] = {}
    for link in genres_root.rglob("*"):
        if not link.is_symlink() or link.exists():
            continue
        target = (link.parent / os.readlink(link)).resolve(strict=False)
        if not any(
            target == root or root in target.parents
            for root in catalog_roots
        ):
            continue
        parsed = movie_title_year(target)
        if parsed is None:
            continue
        key = (normalized_title(parsed[0]), parsed[1])
        targets.setdefault(key, set()).add(target)
    return {
        key: sorted(values, key=lambda path: str(path).casefold())
        for key, values in targets.items()
    }


def existing_collection_directory(
    library_root: Path, collection_id: int | None
) -> Path | None:
    if collection_id is None:
        return None
    index_path = library_root / "genres" / "_genre-index.tsv"
    if not index_path.is_file():
        return None
    with index_path.open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            if row.get("tmdb_collection_id") != str(collection_id):
                continue
            source = library_root / row["source"]
            relative = source.relative_to(library_root)
            source_root_parts = next(
                (
                    Path(configured).parts
                    for configured in MOVIE_SOURCES
                    if relative.parts[: len(Path(configured).parts)]
                    == Path(configured).parts
                ),
                (),
            )
            if len(relative.parts) > len(source_root_parts) + 1:
                return source.parent
    return None


def choose_catalog(genres: tuple[str, ...]) -> str | None:
    available = {
        home
        for genre in genres
        if (home := GENRE_HOMES.get(genre.casefold())) is not None
    }
    for home in CATALOG_PRIORITY:
        if home in available:
            return home
    return None


def edition_signature(path: Path, embedded_title: str = "") -> tuple[str, ...]:
    evidence = f"{path.stem} {embedded_title}"
    return tuple(
        label for pattern, label in EDITION_TAGS if pattern.search(evidence)
    )


def source_label(path: Path, embedded_title: str = "") -> str:
    evidence = f"{path.stem} {embedded_title}"
    return next(
        (label for pattern, label in SOURCE_TAGS if pattern.search(evidence)),
        "",
    )


def resolution_tier(probe: MediaProbe) -> int:
    if probe.width >= 3000 or probe.height >= 1600:
        return 4
    if probe.width >= 1800 or probe.height >= 1000:
        return 3
    if probe.width >= 1200 or probe.height >= 700:
        return 2
    return 1


def is_hdr(probe: MediaProbe) -> bool:
    return probe.color_transfer.casefold() in {"smpte2084", "arib-std-b67"}


def dynamic_range_tier(probe: MediaProbe) -> int:
    if probe.dv_profile > 0:
        return 2
    return int(is_hdr(probe))


def has_lossless_audio(probe: MediaProbe) -> bool:
    return bool(LOSSLESS_AUDIO_CODECS.intersection(probe.audio_codecs))


def is_ai_upscale(path: Path, embedded_title: str = "") -> bool:
    return AI_UPSCALE_RE.search(f"{path.stem} {embedded_title}") is not None


def quality_summary(path: Path, probe: MediaProbe) -> str:
    source = source_label(path, probe.title) or "unknown-source"
    if is_ai_upscale(path, probe.title):
        source = f"{source}/AI-upscale"
    if probe.dv_profile > 0:
        hdr = f"DV-P{probe.dv_profile}/HDR" if is_hdr(probe) else f"DV-P{probe.dv_profile}"
    else:
        hdr = "HDR" if is_hdr(probe) else "SDR/unknown-HDR"
    bitrate = (
        f"{probe.bit_rate / 1_000_000:.1f}Mbps"
        if probe.bit_rate > 0
        else "unknown-bitrate"
    )
    audio = ",".join(probe.audio_codecs) or "unknown-audio"
    return (
        f"{probe.width}x{probe.height} {source} {hdr} {bitrate} "
        f"audio={audio}/{probe.audio_channels}ch"
    )


def sampled_digest(path: Path, sample_size: int = 4 * 1024 * 1024) -> str:
    """Hash bounded samples for fast byte-identical duplicate recognition."""

    size = path.stat().st_size
    digest = hashlib.sha256()
    digest.update(size.to_bytes(16, "big"))
    offsets = {0, max(0, size // 2 - sample_size // 2), max(0, size - sample_size)}
    with path.open("rb") as source:
        for offset in sorted(offsets):
            source.seek(offset)
            digest.update(offset.to_bytes(16, "big"))
            digest.update(source.read(sample_size))
    return digest.hexdigest()


def compare_media_quality(
    incoming_path: Path,
    incoming: MediaProbe,
    existing_path: Path,
    existing: MediaProbe,
) -> QualityDecision:
    """Return a winner only for same-cut, technically clear comparisons."""

    incoming_summary = quality_summary(incoming_path, incoming)
    existing_summary = quality_summary(existing_path, existing)
    incoming_edition = edition_signature(incoming_path, incoming.title)
    existing_edition = edition_signature(existing_path, existing.title)
    if incoming_edition != existing_edition:
        return QualityDecision(
            None,
            "edition/cut differs or is not equally identified "
            f"(incoming={incoming_edition or ('unspecified',)}, "
            f"existing={existing_edition or ('unspecified',)})",
            incoming_summary,
            existing_summary,
        )

    try:
        if (
            incoming_path.stat().st_size == existing_path.stat().st_size
            and sampled_digest(incoming_path) == sampled_digest(existing_path)
        ):
            return QualityDecision(
                "existing",
                "sampled byte identity and equal file size",
                incoming_summary,
                existing_summary,
            )
    except OSError:
        pass

    incoming_ai_upscale = is_ai_upscale(incoming_path, incoming.title)
    existing_ai_upscale = is_ai_upscale(existing_path, existing.title)
    incoming_resolution = resolution_tier(incoming) - int(
        incoming_ai_upscale and resolution_tier(incoming) == 4
    )
    existing_resolution = resolution_tier(existing) - int(
        existing_ai_upscale and resolution_tier(existing) == 4
    )
    incoming_source = SOURCE_QUALITY[source_label(incoming_path, incoming.title)]
    existing_source = SOURCE_QUALITY[source_label(existing_path, existing.title)]
    incoming_dynamic_range = dynamic_range_tier(incoming)
    existing_dynamic_range = dynamic_range_tier(existing)

    if incoming_resolution != existing_resolution:
        higher = "incoming" if incoming_resolution > existing_resolution else "existing"
        higher_source = incoming_source if higher == "incoming" else existing_source
        lower_source = existing_source if higher == "incoming" else incoming_source
        if higher_source >= lower_source - 1:
            return QualityDecision(
                higher,
                "higher resolution without a materially worse source tier",
                incoming_summary,
                existing_summary,
            )
        return QualityDecision(
            None,
            "resolution and source quality point to different winners",
            incoming_summary,
            existing_summary,
        )

    if incoming_ai_upscale != existing_ai_upscale:
        return QualityDecision(
            None,
            "AI-upscaled and native-source presentations require review",
            incoming_summary,
            existing_summary,
        )

    if incoming_source != existing_source:
        higher = "incoming" if incoming_source > existing_source else "existing"
        higher_dynamic_range = (
            incoming_dynamic_range if higher == "incoming" else existing_dynamic_range
        )
        lower_dynamic_range = (
            existing_dynamic_range if higher == "incoming" else incoming_dynamic_range
        )
        if (
            abs(incoming_source - existing_source) >= 2
            and higher_dynamic_range >= lower_dynamic_range
        ):
            return QualityDecision(
                higher,
                "materially better source tier at equal resolution",
                incoming_summary,
                existing_summary,
            )
        if (
            higher_dynamic_range >= lower_dynamic_range
            and abs(incoming_source - existing_source) == 1
        ):
            return QualityDecision(
                higher,
                "better source tier with no HDR disadvantage at equal resolution",
                incoming_summary,
                existing_summary,
            )
        return QualityDecision(
            None,
            "source tier and HDR properties point to different winners",
            incoming_summary,
            existing_summary,
        )

    if incoming_dynamic_range != existing_dynamic_range:
        winner = (
            "incoming"
            if incoming_dynamic_range > existing_dynamic_range
            else "existing"
        )
        return QualityDecision(
            winner,
            "better dynamic-range presentation at equal resolution and source tier",
            incoming_summary,
            existing_summary,
        )

    if incoming.bit_rate > 0 and existing.bit_rate > 0:
        ratio = incoming.bit_rate / existing.bit_rate
        if ratio >= 1.25 or ratio <= 0.8:
            winner = "incoming" if ratio > 1 else "existing"
            return QualityDecision(
                winner,
                "at least 25% higher video/container bitrate at otherwise equal tiers",
                incoming_summary,
                existing_summary,
            )

    incoming_lossless = int(has_lossless_audio(incoming))
    existing_lossless = int(has_lossless_audio(existing))
    if incoming_lossless != existing_lossless:
        winner = "incoming" if incoming_lossless else "existing"
        return QualityDecision(
            winner,
            "lossless audio advantage at otherwise equal tiers",
            incoming_summary,
            existing_summary,
        )
    if abs(incoming.audio_channels - existing.audio_channels) >= 2:
        winner = (
            "incoming"
            if incoming.audio_channels > existing.audio_channels
            else "existing"
        )
        return QualityDecision(
            winner,
            "material audio-channel advantage at otherwise equal tiers",
            incoming_summary,
            existing_summary,
        )
    return QualityDecision(
        None,
        "quality is too close or incomplete for automatic selection",
        incoming_summary,
        existing_summary,
    )


def _technical_filename(
    source: Path, identity: MovieIdentity, probe: MediaProbe
) -> str:
    evidence_text = f"{source.stem} {probe.title}"
    editions = [label for pattern, label in EDITION_TAGS if pattern.search(evidence_text)]
    source_tag = source_label(source, probe.title)
    if probe.width >= 3000 or probe.height >= 1600:
        resolution = "2160p"
    elif probe.width >= 1800 or probe.height >= 1000:
        resolution = "1080p"
    elif probe.width >= 1200 or probe.height >= 700:
        resolution = "720p"
    else:
        resolution = f"{probe.height}p" if probe.height else "Unknown-Resolution"
    tags = [*editions, resolution]
    if source_tag:
        tags.append(source_tag)
    if is_ai_upscale(source, probe.title):
        tags.append("AI Upscale")
    if is_hdr(probe):
        tags.append("HDR")
    if probe.dv_profile > 0:
        tags.insert(tags.index("HDR") if "HDR" in tags else len(tags), "DV")
    if probe.codec.casefold() in {"hevc", "h265"} and "HEVC" in evidence_text.upper():
        tags.append("HEVC")
    return f"{identity.title} ({identity.year}) - {' '.join(tags)}{source.suffix}"


def _reviewed_intake_destination(
    library_root: Path,
    source: Path,
    identity: MovieIdentity,
    probe: MediaProbe,
) -> Path | None:
    override = MOVIE_INTAKE_OVERRIDES.get(identity.imdb_id)
    if override is None:
        return None
    relative_directory, order = override
    directory = Path(relative_directory)
    if directory.is_absolute() or ".." in directory.parts:
        raise ValueError(f"unsafe movie intake override for {identity.imdb_id}")
    configured_roots = tuple(Path(configured).parts for configured in MOVIE_SOURCES)
    if not any(
        directory.parts[: len(configured)] == configured
        for configured in configured_roots
    ):
        raise ValueError(f"unknown catalog in movie intake override for {identity.imdb_id}")
    prefix = f"{order:02d} - " if order is not None else ""
    return library_root / directory / f"{prefix}{_technical_filename(source, identity, probe)}"


def _sidecar_mappings(source: Path, destination: Path) -> list[tuple[Path, Path]]:
    mappings: list[tuple[Path, Path]] = [(source, destination)]
    old_stem = source.stem
    new_stem = destination.stem
    for sibling in source.parent.iterdir():
        if sibling == source or not sibling.is_file() or sibling.is_symlink():
            continue
        if sibling.suffix.casefold() not in SIDECAR_EXTENSIONS:
            continue
        if sibling.name == f"{old_stem}-poster.jpg":
            new_name = f"{new_stem}-poster.jpg"
        elif sibling.name.startswith(f"{old_stem}."):
            new_name = f"{new_stem}{sibling.name[len(old_stem):]}"
        else:
            continue
        mappings.append((sibling, destination.with_name(new_name)))

    preview = source.parent / ".rusty_previews" / old_stem
    if preview.is_dir() and not preview.is_symlink():
        mappings.append(
            (preview, destination.parent / ".rusty_previews" / new_stem)
        )
    return mappings


def catalog_paths_for_imdb(library_root: Path, imdb_id: str) -> list[Path]:
    index_path = library_root / "genres" / "_genre-index.tsv"
    if not index_path.is_file():
        return []
    matches: set[Path] = set()
    with index_path.open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            if row.get("imdb_id") != imdb_id or not row.get("source"):
                continue
            source = library_root / row["source"]
            if source.exists() and not source.is_symlink():
                matches.add(source)
    return sorted(matches, key=lambda path: str(path).casefold())


def duplicate_archive_path(
    library_root: Path,
    category: str,
    identity: MovieIdentity,
    source: Path,
    *,
    preserve_catalog_path: bool,
) -> Path:
    root = library_root / "to-review" / category
    if preserve_catalog_path:
        relative = source.relative_to(library_root)
        return root / relative
    title_dir = f"{identity.title} ({identity.year})"
    return root / title_dir / source.name


def _mapping_collision(mappings: tuple[tuple[Path, Path], ...]) -> Path | None:
    origins = {origin for origin, _target in mappings}
    seen: set[Path] = set()
    for _origin, target in mappings:
        if target in seen or (target.exists() and target not in origins):
            return target
        seen.add(target)
    return None


def plan_intake(
    library_root: Path,
    *,
    settle_seconds: float,
    minimum_confidence: int,
    tmdb_token: str | None,
    allow_network: bool,
) -> tuple[list[IntakePlan], list[IntakeIssue], TmdbClient]:
    candidates, issues = settled_candidates(
        root_video_candidates(library_root), settle_seconds
    )
    caches = state_dir(library_root)
    imdb_index = caches / "imdb-index.sqlite3"
    recovery_targets = broken_catalog_targets(library_root)
    tmdb = TmdbClient(
        caches / "tmdb-cache.json",
        tmdb_token,
        allow_fetch=allow_network,
    )
    plans: list[IntakePlan] = []
    reserved_destinations: set[Path] = set()

    for source in candidates:
        if EPISODE_RE.search(source.stem):
            issues.append(IntakeIssue(source, "episode intake requires an exact show match"))
            continue
        try:
            probe = probe_media(source)
        except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
            issues.append(IntakeIssue(source, f"cannot inspect media: {error}"))
            continue
        hints = identity_hints(source, probe)
        if not hints:
            issues.append(IntakeIssue(source, "no trustworthy title and year in filename or media tags"))
            continue
        identity = identify_movie(hints, probe, imdb_index, tmdb=tmdb)
        if identity is None:
            issues.append(IntakeIssue(source, "no unique feature-film IMDb title/year match"))
            continue

        evidence = [identity.hint_source, identity.match_method]
        confidence = 60 + (20 if identity.hint_source == "embedded-title" else 10)
        key = (normalized_title(identity.title), identity.year)
        recovery = recovery_targets.get(key, [])
        tmdb_movie = tmdb.lookup(identity.imdb_id, identity.title, identity.year)
        genres = tmdb_movie.genres if tmdb_movie is not None else identity.genres
        if tmdb_movie is not None:
            confidence += 15
            evidence.append(tmdb_movie.match_method)

        incumbents = catalog_paths_for_imdb(library_root, identity.imdb_id)
        if len(incumbents) > 1:
            issues.append(
                IntakeIssue(
                    source,
                    "multiple catalog copies already have this IMDb identity: "
                    + ", ".join(str(path.relative_to(library_root)) for path in incumbents),
                )
            )
            continue
        if incumbents:
            incumbent = incumbents[0]
            try:
                incumbent_probe = probe_media(incumbent)
            except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
                issues.append(
                    IntakeIssue(source, f"cannot inspect existing catalog copy: {error}")
                )
                continue
            decision = compare_media_quality(
                source, probe, incumbent, incumbent_probe
            )
            if decision.winner is None:
                issues.append(
                    IntakeIssue(
                        source,
                        f"duplicate of {incumbent.relative_to(library_root)} but "
                        f"automatic quality choice is unsafe: {decision.reason}; "
                        f"incoming={decision.incoming_summary}; "
                        f"existing={decision.existing_summary}",
                    )
                )
                continue

            if decision.winner == "existing":
                archived = duplicate_archive_path(
                    library_root,
                    "Duplicates-Lower-Quality",
                    identity,
                    source,
                    preserve_catalog_path=False,
                )
                mappings = tuple(_sidecar_mappings(source, archived))
                action = "archive-duplicate"
                destination = incumbent
                evidence.append("existing-catalog-copy-wins")
            else:
                archived = duplicate_archive_path(
                    library_root,
                    "Duplicates-Replaced",
                    identity,
                    incumbent,
                    preserve_catalog_path=True,
                )
                destination = _reviewed_intake_destination(
                    library_root, source, identity, probe
                )
                if destination is None:
                    technical_name = _technical_filename(source, identity, probe)
                    collection_prefix = LEADING_NUMBER_RE.match(incumbent.stem)
                    destination = incumbent.with_name(
                        f"{collection_prefix.group(0) if collection_prefix else ''}"
                        f"{technical_name}"
                    )
                else:
                    evidence.append("reviewed-intake-override")
                mappings = (
                    *_sidecar_mappings(incumbent, archived),
                    *_sidecar_mappings(source, destination),
                )
                action = "replace"
                evidence.append("incoming-copy-wins")

            confidence = min(confidence + 20, 100)
            if confidence < minimum_confidence:
                issues.append(
                    IntakeIssue(
                        source,
                        f"confidence {confidence}% is below "
                        f"{minimum_confidence}% threshold",
                    )
                )
                continue
            collision = _mapping_collision(mappings)
            if collision is not None or any(
                target in reserved_destinations for _origin, target in mappings
            ):
                issues.append(
                    IntakeIssue(
                        source,
                        f"duplicate preservation collision: {collision or 'reserved target'}",
                    )
                )
                continue
            reserved_destinations.update(target for _origin, target in mappings)
            plans.append(
                IntakePlan(
                    source=source,
                    destination=destination,
                    identity=identity,
                    tmdb=tmdb_movie,
                    confidence=confidence,
                    evidence=tuple(evidence),
                    mappings=mappings,
                    action=action,
                    incumbent=incumbent,
                    archived_path=archived,
                    quality_summary=(
                        f"{decision.reason}; incoming={decision.incoming_summary}; "
                        f"existing={decision.existing_summary}"
                    ),
                )
            )
            continue

        if len(recovery) == 1:
            destination = recovery[0]
            confidence += 20
            evidence.append("unique-broken-catalog-target")
        elif len(recovery) > 1:
            issues.append(IntakeIssue(source, "multiple prior catalog destinations match"))
            continue
        else:
            destination = _reviewed_intake_destination(
                library_root, source, identity, probe
            )
            if destination is not None:
                confidence += 20
                evidence.append("reviewed-intake-override")
            else:
                collection_dir = existing_collection_directory(
                    library_root,
                    tmdb_movie.collection_id if tmdb_movie is not None else None,
                )
                if collection_dir is not None:
                    issues.append(
                        IntakeIssue(
                            source,
                            "existing collection requires reviewed release-order numbering",
                        )
                    )
                    continue
                catalog = choose_catalog(genres)
                if catalog is None:
                    issues.append(IntakeIssue(source, f"no safe primary catalog for genres {genres}"))
                    continue
                destination = library_root / catalog / _technical_filename(
                    source, identity, probe
                )

        confidence = min(confidence, 100)
        if confidence < minimum_confidence:
            issues.append(
                IntakeIssue(
                    source,
                    f"confidence {confidence}% is below {minimum_confidence}% threshold",
                )
            )
            continue

        mappings = tuple(_sidecar_mappings(source, destination))
        collision = _mapping_collision(mappings)
        if collision is None:
            collision = next(
                (
                    target
                    for _origin, target in mappings
                    if target in reserved_destinations
                ),
                None,
            )
        if collision is not None:
            issues.append(IntakeIssue(source, f"destination collision: {collision}"))
            continue
        reserved_destinations.update(target for _origin, target in mappings)
        plans.append(
            IntakePlan(
                source=source,
                destination=destination,
                identity=identity,
                tmdb=tmdb_movie,
                confidence=confidence,
                evidence=tuple(evidence),
                mappings=mappings,
            )
        )
    return plans, issues, tmdb


def print_intake_report(
    library_root: Path, plans: list[IntakePlan], issues: list[IntakeIssue]
) -> None:
    for plan in plans:
        genres = plan.tmdb.genres if plan.tmdb is not None else plan.identity.genres
        source_name = plan.source.relative_to(library_root)
        destination_name = plan.destination.relative_to(library_root)
        print(f"PLAN\t{plan.action}\t{source_name}\t{destination_name}")
        print(
            f"  {plan.confidence}% {plan.identity.title} ({plan.identity.year}) "
            f"{plan.identity.imdb_id}; genres={','.join(genres)}; "
            f"evidence={','.join(plan.evidence)}"
        )
        if plan.incumbent is not None:
            print(
                f"  QUALITY\t{plan.quality_summary}; "
                f"archive={plan.archived_path.relative_to(library_root) if plan.archived_path else ''}"
            )
        for origin, target in plan.mappings[1:]:
            print(
                f"  SIDECAR\t{origin.relative_to(library_root)}\t"
                f"{target.relative_to(library_root)}"
            )
    for issue in issues:
        print(f"REVIEW\t{issue.source.relative_to(library_root)}\t{issue.reason}")
    print(f"Intake plans: {len(plans)}; review items: {len(issues)}")


def _move_without_overwrite(source: Path, destination: Path) -> None:
    if destination.exists():
        raise FileExistsError(f"destination appeared during intake: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    try:
        source.rename(destination)
    except PermissionError:
        completed = subprocess.run(
            ["sudo", "-n", "mv", "--no-clobber", "--", str(source), str(destination)]
        )
        if completed.returncode != 0 or source.exists() or not destination.exists():
            raise RuntimeError(f"could not move {source} to {destination}")


def _append_duplicate_manifests(
    library_root: Path, plans: list[IntakePlan]
) -> None:
    rows_by_manifest: dict[Path, list[tuple[str, ...]]] = {}
    timestamp = datetime.now(timezone.utc).isoformat()
    for plan in plans:
        if plan.archived_path is None or plan.action not in {
            "archive-duplicate",
            "replace",
        }:
            continue
        category_root = next(
            (
                parent
                for parent in plan.archived_path.parents
                if parent.parent == library_root / "to-review"
            ),
            None,
        )
        if category_root is None:
            continue
        manifest = category_root / "manifest.tsv"
        winner = plan.destination.relative_to(library_root)
        archived = plan.archived_path.relative_to(library_root)
        rows_by_manifest.setdefault(manifest, []).append(
            (
                timestamp,
                plan.action,
                plan.identity.imdb_id,
                f"{plan.identity.title} ({plan.identity.year})",
                str(winner),
                str(archived),
                plan.quality_summary,
            )
        )
    for manifest, rows in rows_by_manifest.items():
        manifest.parent.mkdir(parents=True, exist_ok=True)
        write_header = not manifest.exists()
        with manifest.open("a", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
            if write_header:
                writer.writerow(
                    (
                        "timestamp_utc",
                        "action",
                        "imdb_id",
                        "title",
                        "catalog_winner",
                        "archived_copy",
                        "quality_evidence",
                    )
                )
            writer.writerows(rows)


def apply_intake(library_root: Path, plans: list[IntakePlan]) -> list[Path]:
    completed_mappings: list[tuple[Path, Path]] = []
    try:
        for plan in plans:
            for source, destination in plan.mappings:
                _move_without_overwrite(source, destination)
                completed_mappings.append((source, destination))
    except Exception:
        for source, destination in reversed(completed_mappings):
            if destination.exists() and not source.exists():
                source.parent.mkdir(parents=True, exist_ok=True)
                destination.rename(source)
        raise
    _append_duplicate_manifests(library_root, plans)
    return [
        plan.destination
        for plan in plans
        if plan.action in {"catalog", "replace"}
    ]
