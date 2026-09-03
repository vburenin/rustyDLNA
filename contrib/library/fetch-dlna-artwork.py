#!/usr/bin/env python3
"""Download rustyDLNA / MiniDLNA sidecar posters for catalog movies and shows.

Movie files get `{stem}-poster.jpg` next to the video. Disc directories and TV
show/season folders get `poster.jpg`, which rustyDLNA uses as folder album art
for every episode in that directory. New downloads are normalized for browse-
card use; existing valid JPEGs are left untouched.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

sys.dont_write_bytecode = True

from lib.catalog_config import (
    MOVIE_SOURCES,
    SHOW_IMDB_IDS,
    SHOW_SOURCES,
    catalog_movie_items,
    is_disc_directory,
)
from lib.imdb_index import _imdb_id, normalized_title
from lib.paths import add_root_argument, require_library_root, state_dir


USER_AGENT = "rustyDLNA-artwork/1.0 (local library sidecar fetch)"
METAHUB_POSTERS = (
    "https://images.metahub.space/poster/medium/{imdb_id}/img",
    "https://images.metahub.space/poster/large/{imdb_id}/img",
)
CINEMETA_META = "https://v3-cinemeta.strem.io/meta/{kind}/{imdb_id}.json"
CINEMETA_SEARCH = (
    "https://v3-cinemeta.strem.io/catalog/{kind}/top/search={query}.json"
)
JPEG_MAGIC = b"\xff\xd8\xff"
MIN_JPEG_BYTES = 2048
MAX_JPEG_BYTES = 8 * 1024 * 1024
POSTER_WIDTH = 360
POSTER_HEIGHT = 540
POSTER_JPEG_QUALITY = 5
YEAR_RE = re.compile(r"\(((?:18|19|20)\d{2})\)")
LEADING_NUMBER_RE = re.compile(r"^\d{1,3}\s*-\s*")
SEASON_DIR_RE = re.compile(r"^Season-\d+$", re.IGNORECASE)

# Optional extra show IMDb IDs for nested catalogs that are not in the year
# index. Keep this empty in the shipped tree; operators can add reviewed IDs.
LOCAL_SHOW_IMDB_IDS: dict[str, str] = {}


def make_world_readable(path: Path) -> None:
    try:
        os.chmod(path, 0o666)
    except OSError:
        pass


def is_valid_jpeg(path: Path) -> bool:
    try:
        size = path.stat().st_size
    except OSError:
        return False
    if size < MIN_JPEG_BYTES or size > MAX_JPEG_BYTES:
        return False
    try:
        with path.open("rb") as handle:
            magic = handle.read(3)
    except OSError:
        return False
    return magic == JPEG_MAGIC


def http_get(url: str, timeout: int = 30) -> tuple[bytes, str]:
    request = urllib.request.Request(
        url,
        headers={"User-Agent": USER_AGENT, "Accept": "*/*"},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        content_type = response.headers.get("Content-Type", "")
        data = response.read(MAX_JPEG_BYTES + 1)
    return data, content_type


def ffmpeg_to_jpeg(source: Path, dest: Path) -> bool:
    command = [
        "ffmpeg",
        "-y",
        "-loglevel",
        "error",
        "-i",
        str(source),
        "-frames:v",
        "1",
        "-vf",
        (
            f"scale=w={POSTER_WIDTH}:h={POSTER_HEIGHT}:"
            "force_original_aspect_ratio=decrease:flags=lanczos,"
            f"pad={POSTER_WIDTH}:{POSTER_HEIGHT}:"
            "(ow-iw)/2:(oh-ih)/2:color=black"
        ),
        "-map_metadata",
        "-1",
        "-f",
        "image2",
        "-c:v",
        "mjpeg",
        "-q:v",
        str(POSTER_JPEG_QUALITY),
        str(dest),
    ]
    try:
        completed = subprocess.run(
            command,
            check=False,
            capture_output=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return completed.returncode == 0 and is_valid_jpeg(dest)


def bytes_to_jpeg_file(data: bytes, _content_type: str, dest: Path) -> bool:
    dest.parent.mkdir(parents=True, exist_ok=True)
    fd, raw_name = tempfile.mkstemp(
        prefix=f".{dest.name}.",
        suffix=".bin",
        dir=dest.parent,
    )
    raw_path = Path(raw_name)
    tmp_path = dest.with_name(f".{dest.name}.{os.getpid()}.jpg")
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
        # Providers return inconsistent artwork sizes (currently up to 780 px
        # wide in this library). Normalize JPEG responses too, so browse-card
        # clients do not decode near-megapixel source images.
        if not ffmpeg_to_jpeg(raw_path, tmp_path):
            return False
        if not is_valid_jpeg(tmp_path):
            return False
        os.replace(tmp_path, dest)
        make_world_readable(dest)
        return True
    finally:
        for leftover in (raw_path, tmp_path):
            try:
                leftover.unlink()
            except OSError:
                pass


def first_imdb_id(value: str) -> str:
    for part in value.replace(",", " ").split():
        part = part.strip()
        if part.startswith("tt") and part[2:].isdigit():
            return part
    return ""


def movie_title_year(path: Path) -> tuple[str, int] | None:
    matches = list(YEAR_RE.finditer(path.stem if path.is_file() else path.name))
    if not matches:
        return None
    match = matches[-1]
    stem = path.stem if path.is_file() else path.name
    title = LEADING_NUMBER_RE.sub("", stem[: match.start()].strip())
    if not title:
        return None
    return title, int(match.group(1))


def movie_poster_path(path: Path) -> Path:
    if path.is_dir() or is_disc_directory(path):
        return path / "poster.jpg"
    return path.with_name(f"{path.stem}-poster.jpg")


def load_genre_index(path: Path) -> dict[str, dict[str, str]]:
    if not path.is_file():
        return {}
    with path.open(encoding="utf-8", newline="") as handle:
        return {
            row["source"]: row
            for row in csv.DictReader(handle, delimiter="\t")
            if row.get("source")
        }


def load_show_imdb_ids(path: Path) -> dict[str, str]:
    ids = {
        relative: ids[0]
        for relative, ids in SHOW_IMDB_IDS.items()
        if ids
    }
    ids.update(LOCAL_SHOW_IMDB_IDS)
    if not path.is_file():
        return ids
    with path.open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            if row.get("kind") != "show":
                continue
            source = row.get("source", "")
            imdb_id = first_imdb_id(row.get("imdb_id", ""))
            if source and imdb_id:
                ids[source] = imdb_id
    return ids


def local_imdb_id(library_root: Path, title: str, year: int | None, series: bool) -> str:
    index_path = state_dir(library_root) / "imdb-index.sqlite3"
    if not index_path.is_file():
        return ""
    wanted = normalized_title(title)
    if not wanted:
        return ""
    types = ("tvSeries", "tvMiniSeries") if series else ("movie", "tvMovie", "video", "short")
    placeholders = ",".join("?" for _ in types)
    sql = f"""
        SELECT t.title_id, t.start_year
        FROM names AS n
        JOIN titles AS t ON t.title_id = n.title_id
        WHERE n.normalized_title = ?
          AND t.title_type IN ({placeholders})
        ORDER BY t.type_priority DESC, t.start_year
    """
    try:
        with sqlite3.connect(f"file:{index_path}?mode=ro", uri=True) as connection:
            rows = connection.execute(sql, (wanted, *types)).fetchall()
    except sqlite3.Error:
        return ""
    if year is None:
        return _imdb_id(int(rows[0][0])) if len(rows) == 1 else ""
    exact = [row for row in rows if row[1] == year]
    if exact:
        return _imdb_id(int(exact[0][0]))
    near = [row for row in rows if row[1] is not None and abs(int(row[1]) - year) <= 1]
    if len(near) == 1:
        return _imdb_id(int(near[0][0]))
    return ""


def season_directories(show_dir: Path) -> list[Path]:
    seasons: list[Path] = []
    for child in sorted(show_dir.iterdir(), key=lambda item: item.name.casefold()):
        if child.is_dir() and SEASON_DIR_RE.match(child.name):
            seasons.append(child)
    return seasons


def iter_show_roots(library_root: Path) -> list[Path]:
    roots: list[Path] = []
    for relative in SHOW_SOURCES:
        source = library_root / relative
        if not source.is_dir():
            continue
        for child in sorted(source.iterdir(), key=lambda item: item.name.casefold()):
            if not child.is_dir() or child.is_symlink():
                continue
            if season_directories(child):
                roots.append(child)
                continue
            for grandchild in sorted(
                child.iterdir(), key=lambda item: item.name.casefold()
            ):
                if grandchild.is_dir() and season_directories(grandchild):
                    roots.append(grandchild)
    return roots


def cinemeta_poster_url(imdb_id: str, kind: str) -> str | None:
    url = CINEMETA_META.format(kind=kind, imdb_id=urllib.parse.quote(imdb_id))
    try:
        payload, _content_type = http_get(url)
        data = json.loads(payload.decode("utf-8"))
    except (OSError, TimeoutError, UnicodeDecodeError, ValueError, urllib.error.URLError):
        return None
    meta = data.get("meta") if isinstance(data, dict) else None
    if not isinstance(meta, dict):
        return None
    poster = meta.get("poster")
    return poster if isinstance(poster, str) and poster.startswith("http") else None


def cinemeta_search_poster(imdb_id: str, kind: str, title: str) -> str | None:
    queries = [imdb_id]
    parsed = movie_title_year(Path(title)) if title else None
    if parsed:
        queries.append(parsed[0])
    elif title:
        queries.append(title)
    for query in queries:
        url = CINEMETA_SEARCH.format(kind=kind, query=urllib.parse.quote(query))
        try:
            payload, _content_type = http_get(url)
            data = json.loads(payload.decode("utf-8"))
        except (OSError, TimeoutError, UnicodeDecodeError, ValueError, urllib.error.URLError):
            continue
        metas = data.get("metas") if isinstance(data, dict) else None
        if not isinstance(metas, list):
            continue
        for meta in metas:
            if not isinstance(meta, dict):
                continue
            if first_imdb_id(str(meta.get("imdb_id") or "")) != imdb_id:
                continue
            poster = meta.get("poster")
            if isinstance(poster, str) and poster.startswith("http"):
                if "m.media-amazon.com" in poster:
                    if "._V1_" in poster:
                        poster = re.sub(r"\._V1_.*$", "._V1_FMjpg_UX600_.jpg", poster)
                    else:
                        poster = poster.rstrip("@") + "@._V1_FMjpg_UX600_.jpg"
                return poster
    return None


def download_poster(imdb_id: str, kind: str, dest: Path, title: str = "") -> str | None:
    last_error = "no source"
    attempted_urls: set[str] = set()

    def try_url(url: str | None) -> bool:
        nonlocal last_error
        if not url or url in attempted_urls:
            return False
        attempted_urls.add(url)
        for attempt in range(3):
            try:
                data, content_type = http_get(url)
            except urllib.error.HTTPError as error:
                last_error = f"HTTP {error.code}"
                if error.code in {404, 410}:
                    break
                time.sleep(0.4 * (attempt + 1))
                continue
            except (OSError, TimeoutError, urllib.error.URLError) as error:
                last_error = str(error)
                time.sleep(0.4 * (attempt + 1))
                continue
            if len(data) < MIN_JPEG_BYTES or len(data) > MAX_JPEG_BYTES:
                last_error = f"bad size {len(data)}"
                return False
            if bytes_to_jpeg_file(data, content_type, dest):
                return True
            last_error = "not a usable jpeg"
            return False
        return False

    # MetaHub serves the normal fast path. Resolve the slower Cinemeta metadata
    # and search fallbacks only after both direct poster sizes fail.
    for template in METAHUB_POSTERS:
        if try_url(template.format(imdb_id=imdb_id)):
            return None
    for candidate_kind in (kind, "movie" if kind == "series" else "series"):
        if try_url(cinemeta_poster_url(imdb_id, candidate_kind)):
            return None
        if try_url(cinemeta_search_poster(imdb_id, candidate_kind, title)):
            return None
    return last_error


def place_jpeg(source: Path, dest: Path) -> str:
    if dest.resolve() == source.resolve():
        return "exists"
    if dest.exists():
        if is_valid_jpeg(dest):
            return "exists"
        dest.unlink()
    dest.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.link(source, dest)
        make_world_readable(dest)
        return "linked"
    except OSError:
        shutil.copy2(source, dest)
        make_world_readable(dest)
        return "copied" if is_valid_jpeg(dest) else "copy-failed"


def replace_jpeg(source: Path, dest: Path) -> str:
    dest.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(
        prefix=f".{dest.name}.",
        suffix=".tmp.jpg",
        dir=dest.parent,
    )
    os.close(fd)
    temporary = Path(temporary_name)
    try:
        shutil.copy2(source, temporary)
        if not is_valid_jpeg(temporary):
            return "copy-failed"
        os.replace(temporary, dest)
        make_world_readable(dest)
        return "replaced"
    except OSError as error:
        return f"replace-failed: {error}"
    finally:
        try:
            temporary.unlink()
        except OSError:
            pass


def catalog_poster_sidecars(library_root: Path) -> list[Path]:
    posters: set[Path] = set()
    for relative_source in (*MOVIE_SOURCES, *SHOW_SOURCES):
        source = library_root / relative_source
        if not source.is_dir():
            continue
        for path in source.rglob("*.jpg"):
            if path.is_symlink() or not path.is_file():
                continue
            if path.name == "poster.jpg" or path.name.endswith("-poster.jpg"):
                posters.add(path)
    return sorted(posters, key=lambda path: str(path).casefold())


def regenerate_jpeg(path: Path) -> str | None:
    fd, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.",
        suffix=".tmp.jpg",
        dir=path.parent,
    )
    os.close(fd)
    temporary = Path(temporary_name)
    try:
        if not ffmpeg_to_jpeg(path, temporary):
            return "conversion or validation failed"
        os.replace(temporary, path)
        make_world_readable(path)
        return None
    except OSError as error:
        return str(error)
    finally:
        try:
            temporary.unlink()
        except OSError:
            pass


def regenerate_existing_posters(
    library_root: Path,
    workers: int,
    dry_run: bool,
) -> int:
    posters = catalog_poster_sidecars(library_root)
    print(
        f"existing catalog posters: {len(posters)} profile: "
        f"{POSTER_WIDTH}x{POSTER_HEIGHT} jpeg-q={POSTER_JPEG_QUALITY}",
        file=sys.stderr,
    )
    if dry_run:
        for path in posters:
            print(f"DRY regenerate\t{path.relative_to(library_root)}")
        return 0

    failures: list[tuple[Path, str]] = []
    completed_count = 0
    with ThreadPoolExecutor(max_workers=max(1, min(workers, 12))) as executor:
        futures = {
            executor.submit(regenerate_jpeg, path): path
            for path in posters
        }
        for future in as_completed(futures):
            path = futures[future]
            try:
                error = future.result()
            except Exception as exception:  # Preserve every unprocessed source.
                error = str(exception)
            if error:
                failures.append((path, error))
            completed_count += 1
            if completed_count % 25 == 0 or completed_count == len(posters):
                print(
                    f"regenerated {completed_count}/{len(posters)} posters",
                    file=sys.stderr,
                )

    print(
        f"regenerated {len(posters) - len(failures)} sidecars, "
        f"failed {len(failures)}",
        file=sys.stderr,
    )
    for path, error in failures:
        print(f"FAIL {path.relative_to(library_root)} ({error})")
    return 1 if failures else 0


def collect_targets(library_root: Path) -> tuple[list[tuple[Path, str, str]], list[str]]:
    targets: list[tuple[Path, str, str]] = []
    skipped: list[str] = []
    genre_index = load_genre_index(library_root / "genres" / "_genre-index.tsv")
    show_ids = load_show_imdb_ids(library_root / "genres" / "_year-index.tsv")

    for relative_source in MOVIE_SOURCES:
        source = library_root / relative_source
        if not source.is_dir():
            skipped.append(f"missing movie source {relative_source}")
            continue
        for path in catalog_movie_items(source):
            relative = str(path.relative_to(library_root))
            row = genre_index.get(relative, {})
            imdb_id = first_imdb_id(row.get("imdb_id", ""))
            if not imdb_id:
                skipped.append(f"no IMDb id for movie {relative}")
                continue
            targets.append((movie_poster_path(path), imdb_id, "movie"))

    for show_dir in iter_show_roots(library_root):
        relative = str(show_dir.relative_to(library_root))
        parsed_show = movie_title_year(show_dir)
        imdb_id = show_ids.get(relative, "")
        if not imdb_id:
            title = parsed_show[0] if parsed_show else show_dir.name
            year = parsed_show[1] if parsed_show else None
            imdb_id = local_imdb_id(library_root, title, year, series=True)
        if not imdb_id:
            skipped.append(f"no IMDb id for show {relative}")
            continue
        targets.append((show_dir / "poster.jpg", imdb_id, "series"))
        for season in season_directories(show_dir):
            targets.append((season / "poster.jpg", imdb_id, "series"))
        movies_dir = show_dir / "Movies"
        if movies_dir.is_dir():
            for path in catalog_movie_items(movies_dir):
                parsed = movie_title_year(path)
                movie_id = ""
                if parsed is not None:
                    movie_id = local_imdb_id(
                        library_root, parsed[0], parsed[1], series=False
                    )
                if not movie_id:
                    skipped.append(
                        f"no IMDb id for show movie {path.relative_to(library_root)}"
                    )
                    continue
                targets.append((movie_poster_path(path), movie_id, "movie"))

    unique: dict[Path, tuple[str, str]] = {}
    for dest, imdb_id, kind in targets:
        unique[dest] = (imdb_id, kind)
    ordered = sorted(
        ((dest, imdb_id, kind) for dest, (imdb_id, kind) in unique.items()),
        key=lambda item: str(item[0]).casefold(),
    )
    return ordered, skipped


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Fetch official posters into rustyDLNA sidecar names for catalog "
            "movies and shows."
        )
    )
    add_root_argument(parser)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument(
        "--regenerate-existing",
        action="store_true",
        help=(
            "atomically normalize all existing catalog poster sidecars to the "
            "configured browse-card profile without downloading them again"
        ),
    )
    parser.add_argument(
        "--refetch-existing",
        action="store_true",
        help=(
            "download and atomically replace every IMDb-backed catalog poster "
            "even when its current sidecar is valid"
        ),
    )
    args = parser.parse_args()
    library_root = require_library_root(parser, args.root)
    if args.regenerate_existing:
        return regenerate_existing_posters(
            library_root,
            args.workers,
            args.dry_run,
        )
    targets, skipped = collect_targets(library_root)
    pending = [
        (dest, imdb_id, kind)
        for dest, imdb_id, kind in targets
        if args.refetch_existing or not is_valid_jpeg(dest)
    ]
    print(
        f"artwork targets: {len(targets)} pending: {len(pending)} "
        f"skipped: {len(skipped)}",
        file=sys.stderr,
    )
    if args.dry_run:
        for dest, imdb_id, kind in pending:
            print(f"DRY {kind}\t{imdb_id}\t{dest.relative_to(library_root)}")
        for item in skipped:
            print(f"SKIP {item}")
        return 0 if not skipped else 0

    by_id: dict[tuple[str, str], list[Path]] = defaultdict(list)
    for dest, imdb_id, kind in pending:
        by_id[(imdb_id, kind)].append(dest)

    cache_dir = Path(tempfile.mkdtemp(prefix="dlna-artwork-"))
    downloaded: dict[tuple[str, str], Path | None] = {}
    errors: dict[tuple[str, str], str] = {}

    def fetch(item: tuple[str, str]) -> tuple[tuple[str, str], Path | None, str | None]:
        imdb_id, kind = item
        dest = cache_dir / f"{imdb_id}-{kind}.jpg"
        sample = by_id[item][0]
        title = sample.parent.name if sample.name.lower() == "poster.jpg" else sample.stem
        error = download_poster(imdb_id, kind, dest, title=title)
        if error:
            return item, None, error
        return item, dest, None

    workers = max(1, min(args.workers, 12))
    with ThreadPoolExecutor(max_workers=workers) as executor:
        futures = [executor.submit(fetch, item) for item in by_id]
        done = 0
        for future in as_completed(futures):
            item, path, error = future.result()
            downloaded[item] = path
            if error:
                errors[item] = error
            done += 1
            if done % 25 == 0 or done == len(futures):
                print(
                    f"downloaded {done}/{len(futures)} unique posters",
                    file=sys.stderr,
                )

    written = 0
    existed = 0
    failed_dests: list[str] = []
    for dest, imdb_id, kind in targets:
        if is_valid_jpeg(dest) and not args.refetch_existing:
            existed += 1
            continue
        source = downloaded.get((imdb_id, kind))
        if source is None:
            failed_dests.append(
                f"{dest.relative_to(library_root)} ({imdb_id}: "
                f"{errors.get((imdb_id, kind), 'missing')})"
            )
            continue
        status = (
            replace_jpeg(source, dest)
            if args.refetch_existing
            else place_jpeg(source, dest)
        )
        if status == "exists":
            existed += 1
        elif status in {"linked", "copied", "replaced"}:
            written += 1
        else:
            failed_dests.append(f"{dest.relative_to(library_root)} ({status})")

    shutil.rmtree(cache_dir, ignore_errors=True)
    print(
        f"wrote {written} sidecars, already present {existed}, "
        f"failed {len(failed_dests)}, unresolved {len(skipped)}",
        file=sys.stderr,
    )
    for item in skipped:
        print(f"SKIP {item}")
    for item in failed_dests:
        print(f"FAIL {item}")
    return 1 if failed_dests else 0


if __name__ == "__main__":
    raise SystemExit(main())
