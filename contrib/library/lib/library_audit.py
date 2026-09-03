"""Read-only identity and classification audit for the complete movie catalog."""

from __future__ import annotations

import csv
import json
import os
import sys
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path

from .catalog_config import MOVIE_SOURCES, VIDEO_EXTENSIONS, catalog_movie_items
from .imdb_index import Candidate, load_imdb_matches, normalized_title
from .paths import state_dir
from .intake_media import (
    MediaProbe,
    identity_from_matches,
    identity_hints,
    movie_title_year,
    probe_media,
    compare_media_quality,
)
from .tmdb_metadata import TmdbClient


@dataclass(frozen=True)
class AuditFinding:
    severity: str
    kind: str
    source: str
    detail: str


@dataclass(frozen=True)
class LibraryAudit:
    catalog_items: int
    indexed_rows: int
    exact_identities: int
    catalog_fallbacks: int
    tmdb_classifications: int
    imdb_classifications: int
    classifier_probed: int
    classifier_resolved: int
    classifier_matches: int
    duplicate_candidates_checked: int
    findings: tuple[AuditFinding, ...]


def _catalog_inventory(
    library_root: Path,
) -> list[tuple[Path, tuple[str, ...]]]:
    inventory: list[tuple[Path, tuple[str, ...]]] = []
    for relative_root, fallback_genres in MOVIE_SOURCES.items():
        catalog = library_root / relative_root
        if not catalog.is_dir():
            continue
        inventory.extend(
            (path, fallback_genres) for path in catalog_movie_items(catalog)
        )
    return sorted(inventory, key=lambda item: str(item[0]).casefold())


def _select_match(
    candidates: list[Candidate], fallback_genres: tuple[str, ...]
) -> Candidate | None:
    if not candidates:
        return None
    fallback = set(fallback_genres)
    return max(
        candidates,
        key=lambda candidate: (
            -candidate[4],
            candidate[3],
            len(fallback.intersection(candidate[2])),
            len(candidate[2]),
        ),
    )


def _load_rows(index_path: Path) -> list[dict[str, str]]:
    if not index_path.is_file():
        return []
    with index_path.open(encoding="utf-8", newline="") as source:
        return list(csv.DictReader(source, delimiter="\t"))


def _primary_genres(fallback_genres: tuple[str, ...]) -> set[str]:
    # Anime and Kids are local placement signals; compare their external
    # companion genres rather than requiring IMDb to carry a local category.
    external = set(fallback_genres).difference({"Anime", "Kids"})
    return external or set(fallback_genres)


def _empty_probe() -> MediaProbe:
    return MediaProbe("", 0, 0, 0, "", "", "")


def _deep_classifier_validation(
    library_root: Path,
    inventory: list[tuple[Path, tuple[str, ...]]],
    rows_by_source: dict[str, dict[str, str]],
    imdb_index: Path,
    *,
    workers: int,
) -> tuple[int, int, int, list[AuditFinding]]:
    """Shadow the autonomous classifier across every catalog movie."""

    probes: dict[Path, MediaProbe] = {}
    findings: list[AuditFinding] = []
    file_paths = [path for path, _fallback in inventory if path.is_file()]
    print(
        f"Deep classifier: FFprobing {len(file_paths):,} catalog files "
        f"with {workers} workers ...",
        file=sys.stderr,
    )
    with ThreadPoolExecutor(max_workers=workers) as executor:
        futures = {executor.submit(probe_media, path): path for path in file_paths}
        for completed_count, future in enumerate(as_completed(futures), start=1):
            path = futures[future]
            try:
                probes[path] = future.result()
            except Exception as error:  # keep filename-only recognition available
                probes[path] = _empty_probe()
                findings.append(
                    AuditFinding(
                        "REVIEW",
                        "probe-failed",
                        str(path.relative_to(library_root)),
                        str(error),
                    )
                )
            if completed_count % 100 == 0 or completed_count == len(file_paths):
                print(
                    f"Deep classifier: probed {completed_count:,}/{len(file_paths):,}",
                    file=sys.stderr,
                )

    hints_by_path = {
        path: identity_hints(path, probes.get(path, _empty_probe()))
        for path, _fallback in inventory
    }
    hint_keys = {
        (normalized_title(hint.title), hint.year)
        for hints in hints_by_path.values()
        for hint in hints
    }
    matches = load_imdb_matches(imdb_index, hint_keys)
    tmdb = TmdbClient(
        state_dir(library_root) / "tmdb-cache.json",
        None,
        allow_fetch=False,
    )
    resolved_count = 0
    matching_count = 0
    for path, _fallback in inventory:
        relative = str(path.relative_to(library_root))
        probe = probes.get(path, _empty_probe())
        identity = identity_from_matches(
            hints_by_path[path], probe, matches, tmdb=tmdb
        )
        row = rows_by_source.get(relative)
        expected = row.get("imdb_id", "") if row is not None else ""
        if identity is None:
            if expected:
                findings.append(
                    AuditFinding(
                        "REVIEW",
                        "classifier-unresolved",
                        relative,
                        f"current index identity is {expected}",
                    )
                )
            continue
        resolved_count += 1
        if identity.imdb_id == expected:
            matching_count += 1
        elif expected:
            findings.append(
                AuditFinding(
                    "ERROR",
                    "classifier-disagrees",
                    relative,
                    f"shadow classifier={identity.imdb_id}; index={expected}",
                )
            )
        else:
            findings.append(
                AuditFinding(
                    "REVIEW",
                    "classifier-improves-fallback",
                    relative,
                    f"shadow classifier found {identity.imdb_id} ({identity.title})",
                )
            )
    return len(file_paths), resolved_count, matching_count, findings


def _audit_duplicate_staging(
    library_root: Path,
    rows_by_source: dict[str, dict[str, str]],
    imdb_index: Path,
    tmdb: TmdbClient,
) -> tuple[int, list[AuditFinding]]:
    duplicates_root = library_root / "dupes"
    if not duplicates_root.is_dir():
        return 0, []
    candidates = sorted(
        (
            path
            for path in duplicates_root.rglob("*")
            if path.is_file()
            and not path.is_symlink()
            and path.suffix.casefold() in VIDEO_EXTENSIONS
        ),
        key=lambda path: str(path).casefold(),
    )
    findings: list[AuditFinding] = []
    sources_by_imdb: dict[str, list[Path]] = defaultdict(list)
    for source, row in rows_by_source.items():
        if row.get("imdb_id"):
            path = library_root / source
            if path.is_file():
                sources_by_imdb[row["imdb_id"]].append(path)

    for candidate in candidates:
        relative = str(candidate.relative_to(library_root))
        try:
            candidate_probe = probe_media(candidate)
        except Exception as error:
            findings.append(AuditFinding("REVIEW", "duplicate-probe-failed", relative, str(error)))
            continue
        hints = identity_hints(candidate, candidate_probe)
        keys = {(normalized_title(hint.title), hint.year) for hint in hints}
        matches = load_imdb_matches(imdb_index, keys)
        identity = identity_from_matches(hints, candidate_probe, matches, tmdb=tmdb)
        if identity is None:
            findings.append(
                AuditFinding(
                    "REVIEW",
                    "duplicate-unresolved",
                    relative,
                    "could not assign a unique IMDb identity",
                )
            )
            continue
        catalog_copies = sources_by_imdb.get(identity.imdb_id, [])
        if len(catalog_copies) != 1:
            findings.append(
                AuditFinding(
                    "REVIEW",
                    "duplicate-catalog-match",
                    relative,
                    f"IMDb {identity.imdb_id} has {len(catalog_copies)} catalog copies",
                )
            )
            continue
        catalog_copy = catalog_copies[0]
        try:
            catalog_probe = probe_media(catalog_copy)
        except Exception as error:
            findings.append(
                AuditFinding(
                    "REVIEW", "duplicate-catalog-probe-failed", relative, str(error)
                )
            )
            continue
        decision = compare_media_quality(
            candidate, candidate_probe, catalog_copy, catalog_probe
        )
        severity = "INFO" if decision.winner is not None else "REVIEW"
        winner = (
            str(catalog_copy.relative_to(library_root))
            if decision.winner == "existing"
            else relative if decision.winner == "incoming" else "undecided"
        )
        findings.append(
            AuditFinding(
                severity,
                "duplicate-quality",
                relative,
                f"IMDb {identity.imdb_id}; winner={winner}; {decision.reason}; "
                f"candidate={decision.incoming_summary}; catalog={decision.existing_summary}",
            )
        )
    return len(candidates), findings


def audit_library(
    library_root: Path, *, deep: bool = False, workers: int | None = None
) -> LibraryAudit:
    """Audit every configured catalog item without modifying the library."""

    inventory = _catalog_inventory(library_root)
    index_path = library_root / "genres" / "_genre-index.tsv"
    rows = _load_rows(index_path)
    rows_by_source = {row.get("source", ""): row for row in rows}
    caches = state_dir(library_root)
    imdb_index = caches / "imdb-index.sqlite3"
    tmdb_cache = TmdbClient(
        caches / "tmdb-cache.json",
        None,
        allow_fetch=False,
    )

    parsed: dict[Path, tuple[str, int]] = {}
    identities: set[tuple[str, int]] = set()
    findings: list[AuditFinding] = []
    for path, _fallback in inventory:
        relative = str(path.relative_to(library_root))
        title_year = movie_title_year(path)
        if title_year is None:
            findings.append(
                AuditFinding(
                    "ERROR",
                    "unnormalized-name",
                    relative,
                    "filename has no recognizable title and parenthesized year",
                )
            )
            continue
        parsed[path] = title_year
        identities.add((normalized_title(title_year[0]), title_year[1]))

    matches = load_imdb_matches(imdb_index, identities)
    exact_identities = 0
    for path, fallback_genres in inventory:
        relative = str(path.relative_to(library_root))
        row = rows_by_source.get(relative)
        if row is None:
            findings.append(
                AuditFinding(
                    "ERROR",
                    "missing-index-row",
                    relative,
                    "catalog movie is absent from _genre-index.tsv",
                )
            )
            continue
        title_year = parsed.get(path)
        if title_year is None:
            continue
        try:
            generated_links = json.loads(row.get("generated_links") or "[]")
        except json.JSONDecodeError:
            generated_links = []
            findings.append(
                AuditFinding(
                    "ERROR", "invalid-link-index", relative, "generated_links is invalid JSON"
                )
            )
        if isinstance(generated_links, list):
            for link_name in generated_links:
                link = library_root / "genres" / str(link_name)
                if not link.is_symlink() or not link.exists():
                    findings.append(
                        AuditFinding(
                            "ERROR",
                            "missing-or-broken-genre-link",
                            relative,
                            str(link.relative_to(library_root)),
                        )
                    )
        key = (normalized_title(title_year[0]), title_year[1])
        selected = _select_match(matches.get(key, []), fallback_genres)
        if selected is None:
            if row.get("classification_source") != "catalog-fallback":
                findings.append(
                    AuditFinding(
                        "ERROR",
                        "unreproducible-identity",
                        relative,
                        f"index says {row.get('imdb_id')}, but local IMDb data has no match",
                    )
                )
            continue
        exact_identities += 1
        indexed_candidate = next(
            (
                candidate
                for candidate in matches.get(key, [])
                if candidate[0] == row.get("imdb_id")
            ),
            None,
        )
        cached_tmdb = tmdb_cache.entries.get(f"imdb:{row.get('imdb_id', '')}")
        tmdb_movie = (
            tmdb_cache._decode_record(cached_tmdb)
            if isinstance(cached_tmdb, dict)
            else None
        )
        external_genres = (
            tmdb_movie.genres
            if tmdb_movie is not None
            else indexed_candidate[2]
            if indexed_candidate is not None
            else selected[2]
        )
        external_primary = _primary_genres(fallback_genres)
        is_local_placement = bool({"Anime", "Kids"}.intersection(fallback_genres))
        if not is_local_placement and not external_primary.intersection(external_genres):
            findings.append(
                AuditFinding(
                    "REVIEW",
                    "primary-placement",
                    relative,
                    f"catalog={','.join(fallback_genres)}; "
                    f"metadata={','.join(external_genres)}",
                )
            )

    inventory_sources = {
        str(path.relative_to(library_root)) for path, _fallback in inventory
    }
    for source, row in rows_by_source.items():
        if source and source not in inventory_sources:
            findings.append(
                AuditFinding(
                    "ERROR",
                    "stale-index-row",
                    source,
                    f"indexed source no longer exists (IMDb {row.get('imdb_id') or 'none'})",
                )
            )

    genres_root = library_root / "genres"
    for link in genres_root.rglob("*"):
        if link.is_symlink() and not link.exists():
            findings.append(
                AuditFinding(
                    "ERROR",
                    "broken-link",
                    str(link.relative_to(library_root)),
                    "symlink target does not exist",
                )
            )

    rows_by_imdb: dict[str, list[str]] = defaultdict(list)
    for row in rows:
        if row.get("imdb_id"):
            rows_by_imdb[row["imdb_id"]].append(row.get("source", ""))
    for imdb_id, sources in sorted(rows_by_imdb.items()):
        if len(sources) > 1:
            findings.append(
                AuditFinding(
                    "REVIEW",
                    "duplicate-imdb-identity",
                    imdb_id,
                    ", ".join(sorted(sources)),
                )
            )

    sources_by_title_year: dict[tuple[str, int], list[str]] = defaultdict(list)
    for path, _fallback in inventory:
        if path not in parsed:
            continue
        title, year = parsed[path]
        sources_by_title_year[(normalized_title(title), year)].append(
            str(path.relative_to(library_root))
        )
    for (title, year), sources in sorted(sources_by_title_year.items()):
        if len(sources) > 1:
            findings.append(
                AuditFinding(
                    "REVIEW",
                    "duplicate-title-year",
                    f"{title} ({year})",
                    ", ".join(sorted(sources)),
                )
            )

    classifier_probed = 0
    classifier_resolved = 0
    classifier_matches = 0
    duplicate_candidates_checked = 0
    if deep:
        worker_count = workers or min(8, os.cpu_count() or 4)
        if worker_count < 1:
            raise ValueError("workers must be at least 1")
        (
            classifier_probed,
            classifier_resolved,
            classifier_matches,
            deep_findings,
        ) = _deep_classifier_validation(
            library_root,
            inventory,
            rows_by_source,
            imdb_index,
            workers=worker_count,
        )
        findings.extend(deep_findings)
        duplicate_candidates_checked, duplicate_findings = _audit_duplicate_staging(
            library_root, rows_by_source, imdb_index, tmdb_cache
        )
        findings.extend(duplicate_findings)

    classifications = Counter(row.get("classification_source", "") for row in rows)
    return LibraryAudit(
        catalog_items=len(inventory),
        indexed_rows=len(rows),
        exact_identities=exact_identities,
        catalog_fallbacks=classifications["catalog-fallback"],
        tmdb_classifications=classifications["tmdb"],
        imdb_classifications=classifications["imdb"],
        classifier_probed=classifier_probed,
        classifier_resolved=classifier_resolved,
        classifier_matches=classifier_matches,
        duplicate_candidates_checked=duplicate_candidates_checked,
        findings=tuple(
            sorted(
                findings,
                key=lambda finding: (
                    finding.severity != "ERROR",
                    finding.kind,
                    finding.source.casefold(),
                ),
            )
        ),
    )


def print_audit(report: LibraryAudit, *, show_placement_reviews: bool = False) -> None:
    placement_reviews = [
        finding
        for finding in report.findings
        if finding.kind == "primary-placement"
    ]
    for finding in report.findings:
        if finding.kind == "primary-placement" and not show_placement_reviews:
            continue
        print(
            f"{finding.severity}\t{finding.kind}\t{finding.source}\t{finding.detail}"
        )
    errors = sum(finding.severity == "ERROR" for finding in report.findings)
    reviews = sum(
        finding.severity == "REVIEW" and finding.kind != "primary-placement"
        for finding in report.findings
    )
    if placement_reviews and not show_placement_reviews:
        print(
            "INFO\tprimary-placement-summary\t"
            f"{len(placement_reviews)} optional suggestions hidden; "
            "no action required; rerun with --show-placement-reviews to inspect"
        )
    print(
        "AUDIT-SUMMARY\t"
        f"catalog={report.catalog_items}\tindex={report.indexed_rows}\t"
        f"exact-identities={report.exact_identities}\t"
        f"classifier-probed={report.classifier_probed}\t"
        f"classifier-resolved={report.classifier_resolved}\t"
        f"classifier-matches={report.classifier_matches}\t"
        f"duplicate-candidates={report.duplicate_candidates_checked}\t"
        f"tmdb={report.tmdb_classifications}\timdb={report.imdb_classifications}\t"
        f"fallback={report.catalog_fallbacks}\terrors={errors}\treview={reviews}\t"
        f"placement-suggestions={len(placement_reviews)}"
    )
