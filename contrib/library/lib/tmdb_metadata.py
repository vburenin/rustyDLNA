"""Cached TMDB movie metadata for the genre-link builder."""

from __future__ import annotations

import json
import re
import sys
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path


API_BASE = "https://api.themoviedb.org/3"
CACHE_VERSION = 1
GENRE_MAP = {
    "Action": "Action",
    "Adventure": "Adventure",
    "Animation": "Animation",
    "Comedy": "Comedy",
    "Crime": "Crime",
    "Documentary": "Documentary",
    "Drama": "Drama",
    "Family": "Family",
    "Fantasy": "Fantasy",
    "History": "History",
    "Horror": "Horror",
    "Music": "Music",
    "Mystery": "Mystery",
    "Romance": "Romance",
    "Science Fiction": "Sci-Fi",
    "Thriller": "Thriller",
    "War": "War",
    "Western": "Western",
}


def normalized_title(value: str) -> str:
    value = value.replace("&", " and ")
    value = unicodedata.normalize("NFKD", value).casefold()
    value = value.replace("’", "").replace("'", "")
    return " ".join(re.findall(r"[^\W_]+", value, flags=re.UNICODE))


def release_year(value: str | None) -> int | None:
    if not value or len(value) < 4 or not value[:4].isdigit():
        return None
    return int(value[:4])


@dataclass(frozen=True)
class TmdbMovie:
    tmdb_id: int
    title: str
    year: int | None
    genres: tuple[str, ...]
    collection_id: int | None
    collection_name: str
    match_method: str


class TmdbClient:
    """Read TMDB metadata from a local cache and optionally fill cache misses."""

    def __init__(
        self,
        cache_path: Path,
        token: str | None,
        *,
        allow_fetch: bool,
        refresh: bool = False,
    ) -> None:
        self.cache_path = cache_path
        self.token = token.strip() if token else ""
        self.allow_fetch = allow_fetch and bool(self.token)
        self.refresh = refresh and self.allow_fetch
        self.entries = self._load_cache()
        self.dirty = False
        self.fetch_count = 0
        self._warning_emitted = False
        if self.allow_fetch:
            action = "refreshing" if self.refresh else "filling missing"
            print(
                f"TMDB: {action} metadata cache {self.cache_path}",
                file=sys.stderr,
            )

    def _load_cache(self) -> dict[str, dict[str, object]]:
        if not self.cache_path.is_file():
            return {}
        try:
            payload = json.loads(self.cache_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(
                f"warning: ignoring invalid TMDB cache {self.cache_path}: {error}",
                file=sys.stderr,
            )
            return {}
        if (
            not isinstance(payload, dict)
            or payload.get("version") != CACHE_VERSION
            or not isinstance(payload.get("entries"), dict)
        ):
            print(
                f"warning: ignoring unsupported TMDB cache {self.cache_path}",
                file=sys.stderr,
            )
            return {}
        return payload["entries"]

    @staticmethod
    def _cache_key(imdb_id: str, title: str, year: int) -> str:
        if imdb_id:
            return f"imdb:{imdb_id}"
        return f"title:{normalized_title(title)}:{year}"

    @staticmethod
    def _decode_record(value: dict[str, object]) -> TmdbMovie | None:
        if value.get("status") == "not-found":
            return None
        try:
            return TmdbMovie(
                tmdb_id=int(value["tmdb_id"]),
                title=str(value["title"]),
                year=int(value["year"]) if value.get("year") is not None else None,
                genres=tuple(str(genre) for genre in value["genres"]),
                collection_id=(
                    int(value["collection_id"])
                    if value.get("collection_id") is not None
                    else None
                ),
                collection_name=str(value.get("collection_name", "")),
                match_method=str(value["match_method"]),
            )
        except (KeyError, TypeError, ValueError):
            return None

    def lookup(
        self, imdb_id: str, title: str, year: int
    ) -> TmdbMovie | None:
        key = self._cache_key(imdb_id, title, year)
        cached = self.entries.get(key)
        if isinstance(cached, dict) and not self.refresh:
            return self._decode_record(cached)
        if not self.allow_fetch:
            return None

        try:
            result = (
                self._find_by_imdb_id(imdb_id, year)
                if imdb_id
                else self._search_by_title(title, year)
            )
            if result is None:
                self.entries[key] = {"status": "not-found"}
                self.dirty = True
                return None
            movie = self._movie_details(
                int(result["id"]),
                expected_year=year,
                match_method="tmdb-imdb-id" if imdb_id else "tmdb-title-year",
            )
            self.entries[key] = (
                {"status": "not-found"}
                if movie is None
                else {"status": "ok", **asdict(movie)}
            )
            self.dirty = True
            return movie
        except (OSError, RuntimeError, ValueError, KeyError) as error:
            if not self._warning_emitted:
                print(
                    f"warning: TMDB lookup disabled after an API error: {error}",
                    file=sys.stderr,
                )
                self._warning_emitted = True
            self.allow_fetch = False
            return None

    def _request(self, path: str, query: dict[str, str] | None = None) -> object:
        url = f"{API_BASE}{path}"
        if query:
            url = f"{url}?{urllib.parse.urlencode(query)}"
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/json",
                "Authorization": f"Bearer {self.token}",
                "User-Agent": "movie-genre-builder/1.0",
            },
        )
        for attempt in range(3):
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    self.fetch_count += 1
                    if self.fetch_count % 50 == 0:
                        print(
                            f"TMDB: completed {self.fetch_count} API requests",
                            file=sys.stderr,
                        )
                    return json.load(response)
            except urllib.error.HTTPError as error:
                if error.code == 404:
                    return None
                if error.code == 429 and attempt < 2:
                    delay = min(int(error.headers.get("Retry-After", "2")), 10)
                    time.sleep(max(delay, 1))
                    continue
                raise RuntimeError(f"HTTP {error.code} from TMDB") from error
            except urllib.error.URLError as error:
                if attempt < 2:
                    continue
                raise OSError(f"could not reach TMDB: {error.reason}") from error
        raise RuntimeError("TMDB request retries exhausted")

    def _find_by_imdb_id(
        self, imdb_id: str, expected_year: int
    ) -> dict[str, object] | None:
        payload = self._request(
            f"/find/{urllib.parse.quote(imdb_id)}",
            {"external_source": "imdb_id"},
        )
        if not isinstance(payload, dict):
            return None
        candidates = payload.get("movie_results")
        if not isinstance(candidates, list):
            return None
        return self._select_candidate(candidates, "", expected_year)

    def _search_by_title(
        self, title: str, expected_year: int
    ) -> dict[str, object] | None:
        payload = self._request(
            "/search/movie",
            {
                "query": title,
                "year": str(expected_year),
                "include_adult": "false",
            },
        )
        if not isinstance(payload, dict):
            return None
        candidates = payload.get("results")
        if not isinstance(candidates, list):
            return None
        return self._select_candidate(candidates, title, expected_year)

    @staticmethod
    def _select_candidate(
        candidates: list[object], title: str, expected_year: int
    ) -> dict[str, object] | None:
        usable: list[tuple[int, int, dict[str, object]]] = []
        wanted_title = normalized_title(title) if title else ""
        for candidate in candidates:
            if not isinstance(candidate, dict) or not isinstance(
                candidate.get("id"), int
            ):
                continue
            year = release_year(
                candidate.get("release_date")
                if isinstance(candidate.get("release_date"), str)
                else None
            )
            if year is None or abs(year - expected_year) > 1:
                continue
            candidate_titles = {
                normalized_title(value)
                for field in ("title", "original_title")
                if isinstance((value := candidate.get(field)), str)
            }
            title_match = not wanted_title or wanted_title in candidate_titles
            if wanted_title and not title_match:
                continue
            usable.append((int(title_match), -abs(year - expected_year), candidate))
        if not usable:
            return None
        return max(usable, key=lambda item: (item[0], item[1]))[2]

    def _movie_details(
        self, tmdb_id: int, expected_year: int, match_method: str
    ) -> TmdbMovie | None:
        payload = self._request(f"/movie/{tmdb_id}", {"language": "en-US"})
        if not isinstance(payload, dict):
            return None
        year = release_year(
            payload.get("release_date")
            if isinstance(payload.get("release_date"), str)
            else None
        )
        if year is None or abs(year - expected_year) > 1:
            return None

        raw_genres = payload.get("genres")
        genre_entries = raw_genres if isinstance(raw_genres, list) else []
        genres = tuple(
            sorted(
                {
                    mapped
                    for entry in genre_entries
                    if isinstance(entry, dict)
                    and isinstance(entry.get("name"), str)
                    and (mapped := GENRE_MAP.get(entry["name"])) is not None
                }
            )
        )
        if not genres:
            return None

        collection = payload.get("belongs_to_collection")
        collection_id = (
            int(collection["id"])
            if isinstance(collection, dict)
            and isinstance(collection.get("id"), int)
            else None
        )
        collection_name = (
            str(collection.get("name", ""))
            if isinstance(collection, dict)
            else ""
        )
        return TmdbMovie(
            tmdb_id=tmdb_id,
            title=str(payload.get("title", "")),
            year=year,
            genres=genres,
            collection_id=collection_id,
            collection_name=collection_name,
            match_method=match_method,
        )

    def save(self) -> None:
        if not self.dirty:
            return
        self.cache_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.cache_path.with_suffix(self.cache_path.suffix + ".new")
        payload = {"version": CACHE_VERSION, "entries": self.entries}
        temporary.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        temporary.replace(self.cache_path)
