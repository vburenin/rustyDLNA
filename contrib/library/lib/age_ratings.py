"""Cached, conservative TMDB content certifications for the age-link builder."""

from __future__ import annotations

import json
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path


API_BASE = "https://api.themoviedb.org/3"
CACHE_VERSION = 1
WIKIDATA_ENDPOINT = "https://query.wikidata.org/sparql"
WIKIDATA_CACHE_VERSION = 3
IMDB_ID_RE = re.compile(r"^tt\d+$")
WIKIDATA_REGION_FALLBACK = (
    "US",
    "DE",
    "GB",
    "RU",
    "NL",
    "JP",
    "FR",
    "BR",
    "ES",
    "RO",
    "KR",
    "MX",
    "ZA",
    "IE",
)

# US certifications do not all encode a literal minimum age.  In particular,
# PG and TV-PG stay in an explicit parental-guidance category rather than being
# assigned a guessed age.
US_CERTIFICATIONS: dict[tuple[str, str], tuple[str, int | None]] = {
    ("movie", "G"): ("ALL_AGES", 0),
    ("movie", "PG"): ("PARENTAL_GUIDANCE", None),
    ("movie", "PG-13"): ("AGE_13_PLUS", 13),
    ("movie", "R"): ("AGE_17_PLUS", 17),
    ("movie", "NC-17"): ("AGE_18_PLUS", 18),
    ("show", "TV-Y"): ("ALL_AGES", 0),
    ("show", "TV-Y7"): ("AGE_07_PLUS", 7),
    ("show", "TV-Y7-FV"): ("AGE_07_PLUS", 7),
    ("show", "TV-G"): ("ALL_AGES", 0),
    ("show", "TV-PG"): ("PARENTAL_GUIDANCE", None),
    ("show", "TV-14"): ("AGE_14_PLUS", 14),
    ("show", "TV-MA"): ("AGE_18_PLUS", 18),
}

# Wikidata stores jurisdiction-specific certifications as item-valued
# properties.  Use exact property/value IDs so labels and languages cannot
# silently change classification behavior.
WIKIDATA_CERTIFICATIONS: dict[
    tuple[str, str], tuple[str, str, str, int | None]
] = {
    # MPA film rating (United States)
    ("P1657", "Q18665330"): ("US", "G", "ALL_AGES", 0),
    ("P1657", "Q18665334"): (
        "US",
        "PG",
        "PARENTAL_GUIDANCE",
        None,
    ),
    ("P1657", "Q18665339"): ("US", "PG-13", "AGE_13_PLUS", 13),
    ("P1657", "Q18665344"): ("US", "R", "AGE_17_PLUS", 17),
    ("P1657", "Q18665349"): ("US", "NC-17", "AGE_18_PLUS", 18),
    ("P1657", "Q47274658"): ("US", "X", "AGE_18_PLUS", 18),
    # FSK film rating (Germany)
    ("P1981", "Q20644794"): ("DE", "FSK 0", "ALL_AGES", 0),
    ("P1981", "Q20644795"): ("DE", "FSK 6", "AGE_06_PLUS", 6),
    ("P1981", "Q20644796"): ("DE", "FSK 12", "AGE_12_PLUS", 12),
    ("P1981", "Q20644797"): ("DE", "FSK 16", "AGE_16_PLUS", 16),
    ("P1981", "Q20644798"): ("DE", "FSK 18", "AGE_18_PLUS", 18),
    # BBFC rating (United Kingdom)
    ("P2629", "Q23301853"): ("GB", "U", "ALL_AGES", 0),
    ("P2629", "Q23301854"): (
        "GB",
        "PG",
        "PARENTAL_GUIDANCE",
        None,
    ),
    ("P2629", "Q23301856"): ("GB", "12", "AGE_12_PLUS", 12),
    ("P2629", "Q23301855"): ("GB", "12A", "AGE_12_PLUS", 12),
    ("P2629", "Q4550895"): ("GB", "15", "AGE_15_PLUS", 15),
    ("P2629", "Q134777959"): ("GB", "15A", "AGE_15_PLUS", 15),
    ("P2629", "Q4557532"): ("GB", "18", "AGE_18_PLUS", 18),
    # RARS rating (Russia)
    ("P2637", "Q23308560"): ("RU", "0+", "ALL_AGES", 0),
    ("P2637", "Q23308561"): ("RU", "6+", "AGE_06_PLUS", 6),
    ("P2637", "Q23308562"): ("RU", "12+", "AGE_12_PLUS", 12),
    ("P2637", "Q23308563"): ("RU", "16+", "AGE_16_PLUS", 16),
    ("P2637", "Q23308564"): ("RU", "18+", "AGE_18_PLUS", 18),
    # Kijkwijzer (Netherlands)
    ("P2684", "Q23649980"): ("NL", "AL", "ALL_AGES", 0),
    ("P2684", "Q23649981"): ("NL", "6", "AGE_06_PLUS", 6),
    ("P2684", "Q23649982"): ("NL", "9", "AGE_09_PLUS", 9),
    ("P2684", "Q23649983"): ("NL", "12", "AGE_12_PLUS", 12),
    ("P2684", "Q23649984"): ("NL", "16", "AGE_16_PLUS", 16),
    # EIRIN film rating (Japan)
    ("P2756", "Q23790275"): ("JP", "G", "ALL_AGES", 0),
    ("P2756", "Q23790279"): ("JP", "PG12", "AGE_12_PLUS", 12),
    ("P2756", "Q23790282"): ("JP", "R15+", "AGE_15_PLUS", 15),
    # CNC film rating (France)
    ("P2758", "Q23817729"): ("FR", "unrestricted", "ALL_AGES", 0),
    ("P2758", "Q23817739"): (
        "FR",
        "warning",
        "PARENTAL_GUIDANCE",
        None,
    ),
    ("P2758", "Q23817740"): ("FR", "12", "AGE_12_PLUS", 12),
    ("P2758", "Q23817741"): ("FR", "16", "AGE_16_PLUS", 16),
    # ClassInd rating (Brazil)
    ("P3216", "Q26678733"): ("BR", "12", "AGE_12_PLUS", 12),
    ("P3216", "Q26678734"): ("BR", "14", "AGE_14_PLUS", 14),
    ("P3216", "Q26678735"): ("BR", "16", "AGE_16_PLUS", 16),
    # ICAA rating (Spain)
    ("P3306", "Q27253939"): ("ES", "general", "ALL_AGES", 0),
    ("P3306", "Q27253940"): ("ES", "7", "AGE_07_PLUS", 7),
    ("P3306", "Q27253945"): ("ES", "12", "AGE_12_PLUS", 12),
    ("P3306", "Q27253947"): ("ES", "16", "AGE_16_PLUS", 16),
    ("P3306", "Q27253952"): ("ES", "18", "AGE_18_PLUS", 18),
    ("P3306", "Q27253957"): ("ES", "kids", "ALL_AGES", 0),
    # CNC film rating (Romania)
    ("P3402", "Q27915574"): ("RO", "A.G.", "ALL_AGES", 0),
    ("P3402", "Q27915575"): ("RO", "A.P.-12", "AGE_12_PLUS", 12),
    ("P3402", "Q27915576"): ("RO", "N-15", "AGE_15_PLUS", 15),
    ("P3402", "Q27915577"): ("RO", "I.M.-18", "AGE_18_PLUS", 18),
    # KMRB rating (South Korea)
    ("P3818", "Q28951021"): ("KR", "15", "AGE_15_PLUS", 15),
    # RTC rating (Mexico)
    ("P3834", "Q28980099"): ("MX", "A", "ALL_AGES", 0),
    ("P3834", "Q28980109"): ("MX", "B", "AGE_12_PLUS", 12),
    ("P3834", "Q28980118"): ("MX", "B15", "AGE_15_PLUS", 15),
    # FPB rating (South Africa)
    ("P4437", "Q42012505"): ("ZA", "16", "AGE_16_PLUS", 16),
    ("P4437", "Q42012509"): ("ZA", "18", "AGE_18_PLUS", 18),
    # IFCO rating (Ireland)
    ("P7573", "Q74434531"): ("IE", "12A", "AGE_12_PLUS", 12),
    ("P7573", "Q74434540"): ("IE", "16", "AGE_16_PLUS", 16),
}


@dataclass(frozen=True)
class AgeRating:
    category: str
    minimum_age: int | None
    region: str
    certification: str
    source: str
    tmdb_id: int | None
    match_method: str


def category_for_minimum_age(minimum_age: int) -> str:
    if not 0 <= minimum_age <= 18:
        raise ValueError(f"minimum age must be between 0 and 18: {minimum_age}")
    return "ALL_AGES" if minimum_age == 0 else f"AGE_{minimum_age:02d}_PLUS"


def classify_certification(
    kind: str, region: str, certification: str
) -> AgeRating | None:
    normalized_region = region.strip().upper()
    normalized_certification = certification.strip().upper()
    if normalized_region != "US":
        return None
    classification = US_CERTIFICATIONS.get((kind, normalized_certification))
    if classification is None:
        return None
    category, minimum_age = classification
    return AgeRating(
        category=category,
        minimum_age=minimum_age,
        region=normalized_region,
        certification=normalized_certification,
        source="tmdb-certification",
        tmdb_id=None,
        match_method="",
    )


def rating_severity(rating: AgeRating) -> int:
    if rating.minimum_age is not None:
        return rating.minimum_age
    if rating.category == "PARENTAL_GUIDANCE":
        return 1
    return 0


def select_region_rating(
    ratings: set[AgeRating], preferred_region: str
) -> AgeRating | None:
    """Select one jurisdiction first, then be conservative within it."""

    preferred = preferred_region.strip().upper()
    region_order = tuple(
        dict.fromkeys((preferred, *WIKIDATA_REGION_FALLBACK))
    )
    for region in region_order:
        regional = [rating for rating in ratings if rating.region == region]
        if regional:
            return max(
                regional,
                key=lambda item: (
                    rating_severity(item),
                    item.certification,
                ),
            )
    return None


class TmdbAgeClient:
    """Read cached age ratings and optionally fill exact IMDb-ID cache misses."""

    def __init__(
        self,
        cache_path: Path,
        token: str | None,
        region: str,
        *,
        allow_fetch: bool,
        refresh: bool = False,
    ) -> None:
        self.cache_path = cache_path
        self.token = token.strip() if token else ""
        self.region = region.strip().upper()
        self.allow_fetch = allow_fetch and bool(self.token)
        self.refresh = refresh and self.allow_fetch
        self.entries = self._load_cache()
        self.dirty = False
        self.fetch_count = 0
        self.rating_count = 0
        self._warning_emitted = False
        if self.allow_fetch:
            action = "refreshing" if self.refresh else "filling missing"
            print(
                f"TMDB age ratings: {action} cache {self.cache_path}",
                file=sys.stderr,
            )

    def _load_cache(self) -> dict[str, dict[str, object]]:
        if not self.cache_path.is_file():
            return {}
        try:
            payload = json.loads(self.cache_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(
                f"warning: ignoring invalid age-rating cache "
                f"{self.cache_path}: {error}",
                file=sys.stderr,
            )
            return {}
        if (
            not isinstance(payload, dict)
            or payload.get("version") != CACHE_VERSION
            or not isinstance(payload.get("entries"), dict)
        ):
            print(
                f"warning: ignoring unsupported age-rating cache "
                f"{self.cache_path}",
                file=sys.stderr,
            )
            return {}
        return payload["entries"]

    def _cache_key(self, kind: str, imdb_id: str) -> str:
        return f"{self.region}:{kind}:imdb:{imdb_id}"

    @staticmethod
    def _decode_record(value: dict[str, object]) -> AgeRating | None:
        if value.get("status") == "not-found":
            return None
        try:
            minimum_age = value.get("minimum_age")
            return AgeRating(
                category=str(value["category"]),
                minimum_age=(
                    int(minimum_age) if minimum_age is not None else None
                ),
                region=str(value["region"]),
                certification=str(value["certification"]),
                source=str(value["source"]),
                tmdb_id=(
                    int(value["tmdb_id"])
                    if value.get("tmdb_id") is not None
                    else None
                ),
                match_method=str(value["match_method"]),
            )
        except (KeyError, TypeError, ValueError):
            return None

    def lookup(
        self, kind: str, imdb_id: str, known_tmdb_id: int | None = None
    ) -> AgeRating | None:
        if kind not in {"movie", "show"}:
            raise ValueError(f"unsupported rating kind: {kind}")
        if not imdb_id:
            return None

        key = self._cache_key(kind, imdb_id)
        cached = self.entries.get(key)
        if isinstance(cached, dict) and not self.refresh:
            return self._decode_record(cached)
        if not self.allow_fetch:
            return None

        try:
            tmdb_id = known_tmdb_id or self._find_tmdb_id(kind, imdb_id)
            rating = (
                self._movie_rating(tmdb_id)
                if kind == "movie" and tmdb_id is not None
                else self._show_rating(tmdb_id)
                if tmdb_id is not None
                else None
            )
            if rating is not None:
                rating = AgeRating(
                    category=rating.category,
                    minimum_age=rating.minimum_age,
                    region=rating.region,
                    certification=rating.certification,
                    source=rating.source,
                    tmdb_id=tmdb_id,
                    match_method=(
                        "tmdb-known-id" if known_tmdb_id else "tmdb-imdb-id"
                    ),
                )
                self.rating_count += 1
            self.entries[key] = (
                {"status": "not-found"}
                if rating is None
                else {"status": "ok", **asdict(rating)}
            )
            self.dirty = True
            return rating
        except (OSError, RuntimeError, ValueError, KeyError) as error:
            if not self._warning_emitted:
                print(
                    f"warning: TMDB age lookup disabled after an API error: "
                    f"{error}",
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
                "User-Agent": "movie-age-builder/1.0",
            },
        )
        for attempt in range(3):
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    self.fetch_count += 1
                    if self.fetch_count % 50 == 0:
                        print(
                            f"TMDB age ratings: completed "
                            f"{self.fetch_count} API requests",
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

    def _find_tmdb_id(self, kind: str, imdb_id: str) -> int | None:
        payload = self._request(
            f"/find/{urllib.parse.quote(imdb_id)}",
            {"external_source": "imdb_id"},
        )
        if not isinstance(payload, dict):
            return None
        result_field = "movie_results" if kind == "movie" else "tv_results"
        candidates = payload.get(result_field)
        if not isinstance(candidates, list):
            return None
        ids = {
            candidate["id"]
            for candidate in candidates
            if isinstance(candidate, dict)
            and isinstance(candidate.get("id"), int)
        }
        return next(iter(ids)) if len(ids) == 1 else None

    def _movie_rating(self, tmdb_id: int) -> AgeRating | None:
        payload = self._request(f"/movie/{tmdb_id}/release_dates")
        if not isinstance(payload, dict):
            return None
        results = payload.get("results")
        if not isinstance(results, list):
            return None
        certifications: set[str] = set()
        for result in results:
            if (
                not isinstance(result, dict)
                or str(result.get("iso_3166_1", "")).upper() != self.region
            ):
                continue
            releases = result.get("release_dates")
            if not isinstance(releases, list):
                continue
            certifications.update(
                str(release.get("certification", "")).strip()
                for release in releases
                if isinstance(release, dict)
                and str(release.get("certification", "")).strip()
            )
        ratings = [
            rating
            for certification in certifications
            if (
                rating := classify_certification(
                    "movie", self.region, certification
                )
            )
            is not None
        ]
        return max(ratings, key=rating_severity) if ratings else None

    def _show_rating(self, tmdb_id: int) -> AgeRating | None:
        payload = self._request(f"/tv/{tmdb_id}/content_ratings")
        if not isinstance(payload, dict):
            return None
        results = payload.get("results")
        if not isinstance(results, list):
            return None
        ratings = [
            rating
            for result in results
            if isinstance(result, dict)
            and str(result.get("iso_3166_1", "")).upper() == self.region
            and (
                rating := classify_certification(
                    "show", self.region, str(result.get("rating", ""))
                )
            )
            is not None
        ]
        return max(ratings, key=rating_severity) if ratings else None

    def save(self) -> None:
        if not self.dirty:
            return
        self.cache_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.cache_path.with_suffix(self.cache_path.suffix + ".new")
        payload = {
            "version": CACHE_VERSION,
            "region": self.region,
            "entries": self.entries,
        }
        temporary.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True)
            + "\n",
            encoding="utf-8",
        )
        temporary.replace(self.cache_path)


class WikidataAgeClient:
    """Batch exact IMDb-ID lookups against cached CC0 Wikidata statements."""

    def __init__(
        self,
        cache_path: Path,
        region: str,
        *,
        allow_fetch: bool,
        refresh: bool = False,
    ) -> None:
        self.cache_path = cache_path
        self.region = region.strip().upper()
        self.allow_fetch = allow_fetch
        self.refresh = refresh and allow_fetch
        self.entries = self._load_cache()
        self.dirty = False
        self.fetch_count = 0
        self.rating_count = 0
        self._warning_emitted = False

    def _load_cache(self) -> dict[str, dict[str, object]]:
        if not self.cache_path.is_file():
            return {}
        try:
            payload = json.loads(self.cache_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(
                f"warning: ignoring invalid Wikidata age cache "
                f"{self.cache_path}: {error}",
                file=sys.stderr,
            )
            return {}
        if (
            not isinstance(payload, dict)
            or payload.get("version") != WIKIDATA_CACHE_VERSION
            or payload.get("region") != self.region
            or not isinstance(payload.get("entries"), dict)
        ):
            print(
                f"warning: ignoring unsupported Wikidata age cache "
                f"{self.cache_path}",
                file=sys.stderr,
            )
            return {}
        return payload["entries"]

    @staticmethod
    def _decode_record(value: dict[str, object]) -> AgeRating | None:
        if value.get("status") == "not-found":
            return None
        try:
            minimum_age = value.get("minimum_age")
            return AgeRating(
                category=str(value["category"]),
                minimum_age=(
                    int(minimum_age) if minimum_age is not None else None
                ),
                region=str(value["region"]),
                certification=str(value["certification"]),
                source=str(value["source"]),
                tmdb_id=None,
                match_method=str(value["match_method"]),
            )
        except (KeyError, TypeError, ValueError):
            return None

    def lookup_many(self, imdb_ids: set[str]) -> dict[str, AgeRating]:
        wanted = {
            imdb_id for imdb_id in imdb_ids if IMDB_ID_RE.fullmatch(imdb_id)
        }
        ratings: dict[str, AgeRating] = {}
        missing: list[str] = []
        for imdb_id in sorted(wanted):
            cached = self.entries.get(imdb_id)
            if isinstance(cached, dict) and not self.refresh:
                rating = self._decode_record(cached)
                if rating is not None:
                    ratings[imdb_id] = rating
                continue
            missing.append(imdb_id)

        if not missing or not self.allow_fetch:
            return ratings

        action = "refreshing" if self.refresh else "filling missing"
        print(
            f"Wikidata age ratings: {action} {len(missing)} exact IMDb IDs",
            file=sys.stderr,
        )
        try:
            for start in range(0, len(missing), 400):
                batch = missing[start : start + 400]
                fetched = self._fetch_batch(batch)
                for imdb_id in batch:
                    rating = fetched.get(imdb_id)
                    self.entries[imdb_id] = (
                        {"status": "not-found"}
                        if rating is None
                        else {"status": "ok", **asdict(rating)}
                    )
                    if rating is not None:
                        ratings[imdb_id] = rating
                        self.rating_count += 1
                self.dirty = True
        except (OSError, RuntimeError, ValueError, KeyError) as error:
            if not self._warning_emitted:
                print(
                    f"warning: Wikidata age lookup failed; using cache and "
                    f"UNRATED fallbacks: {error}",
                    file=sys.stderr,
                )
                self._warning_emitted = True
        return ratings

    def _fetch_batch(self, imdb_ids: list[str]) -> dict[str, AgeRating]:
        values = " ".join(json.dumps(imdb_id) for imdb_id in imdb_ids)
        query = f"""
SELECT ?imdb ?property ?rating WHERE {{
  VALUES ?imdb {{ {values} }}
  ?item wdt:P345 ?imdb.
  VALUES ?property {{
    wdt:P1657 wdt:P1981 wdt:P2629 wdt:P2637
    wdt:P2684 wdt:P2756 wdt:P2758 wdt:P3216
    wdt:P3306 wdt:P3402 wdt:P3818 wdt:P3834
    wdt:P4437 wdt:P7573
  }}
  ?item ?property ?rating.
}}
"""
        body = urllib.parse.urlencode({"query": query}).encode()
        request = urllib.request.Request(
            WIKIDATA_ENDPOINT,
            data=body,
            headers={
                "Accept": "application/sparql-results+json",
                "Content-Type": "application/x-www-form-urlencoded",
                "User-Agent": (
                    "movie-age-builder/1.0 (personal media library)"
                ),
            },
        )
        payload: object | None = None
        for attempt in range(3):
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    self.fetch_count += 1
                    payload = json.load(response)
                    break
            except urllib.error.HTTPError as error:
                if error.code == 429 and attempt < 2:
                    delay = min(int(error.headers.get("Retry-After", "2")), 10)
                    time.sleep(max(delay, 1))
                    continue
                raise RuntimeError(
                    f"HTTP {error.code} from Wikidata"
                ) from error
            except urllib.error.URLError as error:
                if attempt < 2:
                    continue
                raise OSError(
                    f"could not reach Wikidata: {error.reason}"
                ) from error
        if payload is None:
            raise RuntimeError("Wikidata request retries exhausted")
        if (
            not isinstance(payload, dict)
            or not isinstance(payload.get("results"), dict)
            or not isinstance(payload["results"].get("bindings"), list)
        ):
            raise RuntimeError("invalid Wikidata query response")

        collected: dict[str, set[AgeRating]] = {}
        for binding in payload["results"]["bindings"]:
            if not isinstance(binding, dict):
                continue
            try:
                imdb_id = str(binding["imdb"]["value"])
                property_id = str(binding["property"]["value"]).rsplit("/", 1)[-1]
                rating_id = str(binding["rating"]["value"]).rsplit("/", 1)[-1]
            except (KeyError, TypeError):
                continue
            classification = WIKIDATA_CERTIFICATIONS.get(
                (property_id, rating_id)
            )
            if classification is None:
                continue
            region, certification, category, minimum_age = classification
            collected.setdefault(imdb_id, set()).add(
                AgeRating(
                    category=category,
                    minimum_age=minimum_age,
                    region=region,
                    certification=certification,
                    source="wikidata-certification",
                    tmdb_id=None,
                    match_method="wikidata-imdb-id",
                )
            )

        combined: dict[str, AgeRating] = {}
        for imdb_id, item_ratings in collected.items():
            selected = select_region_rating(item_ratings, self.region)
            if selected is None:
                continue
            combined[imdb_id] = AgeRating(
                category=selected.category,
                minimum_age=selected.minimum_age,
                region=selected.region,
                certification=selected.certification,
                source="wikidata-certification",
                tmdb_id=None,
                match_method=(
                    "wikidata-imdb-id-preferred-region"
                    if selected.region == self.region
                    else "wikidata-imdb-id-region-fallback"
                ),
            )
        return combined

    def save(self) -> None:
        if not self.dirty:
            return
        self.cache_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.cache_path.with_suffix(self.cache_path.suffix + ".new")
        payload = {
            "version": WIKIDATA_CACHE_VERSION,
            "source": "Wikidata CC0",
            "region": self.region,
            "entries": self.entries,
        }
        temporary.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True)
            + "\n",
            encoding="utf-8",
        )
        temporary.replace(self.cache_path)
