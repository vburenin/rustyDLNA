from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))

from lib import intake_media  # noqa: E402
from lib.intake_media import (  # noqa: E402
    IdentityHint,
    MediaProbe,
    MovieIdentity,
    IntakePlan,
    choose_catalog,
    compare_media_quality,
    edition_signature,
    identity_from_matches,
    identity_hints,
    movie_title_year,
    parse_identity_hint,
    quality_summary,
    source_label,
)


def probe(
    width: int,
    height: int,
    *,
    title: str = "",
    duration: float = 7200,
    hdr: bool = False,
    bitrate: int = 20_000_000,
    audio: tuple[str, ...] = ("ac3",),
    channels: int = 6,
    dv: int = 0,
) -> MediaProbe:
    return MediaProbe(
        title,
        duration,
        width,
        height,
        "hevc",
        "smpte2084" if hdr else "bt709",
        "bt2020" if hdr else "bt709",
        bitrate,
        channels,
        audio,
        dv,
    )


class IdentityRecognitionTests(unittest.TestCase):
    def test_parenthesized_year_wins_after_numeric_title(self) -> None:
        self.assertEqual(
            parse_identity_hint(
                "2001: A Space Odyssey (1968) - 2160p BDRemux.mkv",
                "filename",
            ),
            IdentityHint("2001: A Space Odyssey", 1968, "filename"),
        )

    def test_embedded_release_title_recognizes_barbie(self) -> None:
        self.assertEqual(
            parse_identity_hint("Barbie 2023 from seleZen", "embedded-title"),
            IdentityHint("Barbie", 2023, "embedded-title"),
        )

    def test_scene_punctuation_and_collection_prefix_are_normalized(self) -> None:
        self.assertEqual(
            parse_identity_hint(
                "05 - Mission.Impossible.Rogue.Nation.2015.2160p",
                "filename",
            ),
            IdentityHint("Mission Impossible Rogue Nation", 2015, "filename"),
        )

    def test_last_parenthesized_year_is_the_catalog_year(self) -> None:
        self.assertEqual(
            movie_title_year(Path("Movie 1984 Retrospective (2024) - 1080p.mkv")),
            ("Movie 1984 Retrospective", 2024),
        )

    def test_disc_directory_period_does_not_truncate_title_or_year(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            disc = Path(temporary) / "01 - Kill Bill: Vol. 1 (2003) - 1080p BluRay"
            (disc / "BDMV").mkdir(parents=True)
            (disc / "BDMV" / "index.bdmv").touch()
            self.assertEqual(movie_title_year(disc), ("Kill Bill: Vol. 1", 2003))
            self.assertIn(
                IdentityHint("Kill Bill: Vol 1", 2003, "filename"),
                identity_hints(disc, probe(1920, 1080)),
            )

    def test_long_media_prefers_feature_over_same_name_short(self) -> None:
        hint = IdentityHint("Example", 2023, "filename")
        candidates = {
            ("example", 2023): [
                ("tt0000001", "Example", ("Short",), 1, 0, "imdb-title-year"),
                ("tt0000002", "Example", ("Drama",), 4, 0, "imdb-title-year"),
            ]
        }
        identity = identity_from_matches([hint], probe(1920, 1080), candidates)
        self.assertIsNotNone(identity)
        assert identity is not None
        self.assertEqual(identity.imdb_id, "tt0000002")

    def test_conflicting_embedded_and_filename_identities_are_rejected(self) -> None:
        hints = [
            IdentityHint("First", 2020, "embedded-title"),
            IdentityHint("Second", 2020, "filename"),
        ]
        candidates = {
            ("first", 2020): [
                ("tt0000001", "First", ("Drama",), 4, 0, "imdb-title-year")
            ],
            ("second", 2020): [
                ("tt0000002", "Second", ("Comedy",), 4, 0, "imdb-title-year")
            ],
        }
        self.assertIsNone(identity_from_matches(hints, probe(1920, 1080), candidates))

    def test_actual_runtime_disambiguates_same_title_and_year(self) -> None:
        hint = IdentityHint("Split", 2016, "filename")
        candidates = {
            ("split", 2016): [
                (
                    "tt2660118",
                    "Split",
                    ("Comedy", "Romance", "Sport"),
                    4,
                    0,
                    "imdb-title-year",
                    90,
                ),
                (
                    "tt4972582",
                    "Split",
                    ("Horror", "Thriller"),
                    4,
                    0,
                    "imdb-title-year",
                    117,
                ),
            ]
        }
        identity = identity_from_matches(
            [hint], probe(3840, 2160, duration=7027), candidates
        )
        self.assertIsNotNone(identity)
        assert identity is not None
        self.assertEqual(identity.imdb_id, "tt4972582")

    def test_long_video_is_not_mistaken_for_a_same_name_short(self) -> None:
        hint = IdentityHint("The Hunter", 2016, "filename")
        candidates = {
            ("the hunter", 2016): [
                (
                    "tt14132998",
                    "Gigots",
                    ("Drama", "Horror", "Short"),
                    1,
                    0,
                    "imdb-alternate-title-year",
                    9,
                )
            ]
        }
        self.assertIsNone(
            identity_from_matches([hint], probe(1920, 1080), candidates)
        )

    def test_embedded_slash_title_supplies_separate_alias_hints(self) -> None:
        hints = identity_hints(
            Path("The Hunter (2016) - 1080p.mkv"),
            probe(
                1920,
                1080,
                title="The Headhunter's Calling / A Family Man (2016) BDRip",
            ),
        )
        self.assertIn(
            IdentityHint(
                "The Headhunter's Calling",
                2016,
                "embedded-title-alternative",
            ),
            hints,
        )
        self.assertIn(
            IdentityHint("A Family Man", 2016, "embedded-title-alternative"),
            hints,
        )

    def test_same_identity_from_two_hints_is_corroborated(self) -> None:
        hints = [
            IdentityHint("Localized Title", 2020, "embedded-title"),
            IdentityHint("Canonical Title", 2020, "filename"),
        ]
        candidate = (
            "tt0000001",
            "Canonical Title",
            ("Comedy",),
            4,
            0,
            "imdb-alternate-title-year",
        )
        identity = identity_from_matches(
            hints,
            probe(1920, 1080),
            {("localized title", 2020): [candidate], ("canonical title", 2020): [candidate]},
        )
        self.assertIsNotNone(identity)
        assert identity is not None
        self.assertEqual(identity.imdb_id, "tt0000001")
        self.assertEqual(identity.hint_source, "embedded-title")

    def test_primary_catalog_selection_uses_reviewed_library_priority(self) -> None:
        self.assertEqual(
            choose_catalog(("Adventure", "Comedy", "Fantasy")), "fantasy"
        )
        self.assertEqual(
            choose_catalog(("Comedy", "Action", "Sci-Fi")), "sci-fi"
        )
        self.assertEqual(
            choose_catalog(("Drama", "Comedy", "Fantasy")), "fantasy"
        )
        self.assertEqual(choose_catalog(("Drama", "Action")), "action")
        self.assertEqual(choose_catalog(("Drama", "Comedy")), "comedy")
        self.assertEqual(choose_catalog(("Drama",)), "drama")
        self.assertEqual(choose_catalog(("Animation", "Family")), None)


class QualitySelectionTests(unittest.TestCase):
    def _files(self, left: str, right: str, left_size: int = 2, right_size: int = 3):
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        left_path = root / left
        right_path = root / right
        left_path.write_bytes(b"a" * left_size)
        right_path.write_bytes(b"b" * right_size)
        self.addCleanup(temporary.cleanup)
        return left_path, right_path

    def test_2160p_remux_clearly_beats_1080p_hdtv(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 2160p BDRemux HDR.mkv",
            "Movie (2020) - 1080p HDTV.mkv",
        )
        decision = compare_media_quality(
            incoming, probe(3840, 2160, hdr=True), existing, probe(1920, 1080)
        )
        self.assertEqual(decision.winner, "incoming")

    def test_generic_remux_label_is_a_lossless_source_tier(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 2160p Remux HDR.mkv",
            "Movie (2020) - 720p BluRay.mkv",
        )
        decision = compare_media_quality(
            incoming,
            probe(
                3840,
                2160,
                hdr=True,
                bitrate=49_000_000,
                audio=("ac3", "truehd"),
                channels=8,
            ),
            existing,
            probe(1280, 720, bitrate=8_000_000),
        )
        self.assertEqual(decision.winner, "incoming")
        self.assertIn("Remux", decision.incoming_summary)

    def test_dotted_blu_ray_remux_is_normalized_as_bdremux(self) -> None:
        self.assertEqual(
            source_label(Path("Movie.2020.UHD.Blu-Ray.Remux.2160p.mkv")),
            "BDRemux",
        )

    def test_hd_dvd_source_is_preserved(self) -> None:
        self.assertEqual(source_label(Path("Movie.2005.HD-DVD.1080p.mkv")), "HD-DVD")

    def test_resolution_source_tradeoff_requires_review(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 2160p WEB-DL.mkv",
            "Movie (2020) - 1080p BDRemux.mkv",
        )
        decision = compare_media_quality(
            incoming, probe(3840, 2160), existing, probe(1920, 1080)
        )
        self.assertIsNone(decision.winner)

    def test_ai_upscale_does_not_automatically_beat_native_1080p(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 2160p BDRemux AI Upscale.mkv",
            "Movie (2020) - 1080p BDRemux.mkv",
        )
        decision = compare_media_quality(
            incoming,
            probe(3840, 2160, bitrate=50_000_000),
            existing,
            probe(1920, 1080, bitrate=30_000_000),
        )
        self.assertIsNone(decision.winner)
        self.assertIn("AI-upscaled", decision.reason)
        self.assertIn("AI-upscale", decision.incoming_summary)

    def test_ai_upscale_is_preserved_in_technical_filename(self) -> None:
        identity = MovieIdentity(
            "tt0000001",
            "Movie",
            2020,
            ("Sci-Fi",),
            "imdb-title-year",
            "filename",
        )
        self.assertEqual(
            intake_media._technical_filename(
                Path("Movie.2020.BD.AI_UPSCALE_3840x2160.mkv"),
                identity,
                probe(3840, 2160),
            ),
            "Movie (2020) - 2160p AI Upscale.mkv",
        )

    def test_different_editions_are_never_automatically_replaced(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - Extended 1080p WEB-DL.mkv",
            "Movie (2020) - 1080p WEB-DL.mkv",
        )
        decision = compare_media_quality(
            incoming, probe(1920, 1080), existing, probe(1920, 1080)
        )
        self.assertIsNone(decision.winner)
        self.assertIn("edition/cut", decision.reason)
        self.assertEqual(edition_signature(incoming), ("Extended",))

    def test_redux_is_preserved_as_an_edition(self) -> None:
        self.assertEqual(
            edition_signature(Path("Apocalypse Now. Redux (1979) 2160p.mkv")),
            ("Redux",),
        )

    def test_remastered_is_preserved_as_an_edition(self) -> None:
        self.assertEqual(
            edition_signature(Path("The Evil Dead.1981.Remastered.1080p.mkv")),
            ("Remastered",),
        )

    def test_hdr_wins_only_after_resolution_and_source_are_equal(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 2160p WEB-DL HDR.mkv",
            "Movie (2020) - 2160p WEB-DL.mkv",
        )
        decision = compare_media_quality(
            incoming,
            probe(3840, 2160, hdr=True),
            existing,
            probe(3840, 2160),
        )
        self.assertEqual(decision.winner, "incoming")

    def test_dolby_vision_wins_over_plain_hdr_at_equal_other_tiers(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 2160p BDRemux DV HDR.mkv",
            "Movie (2020) - 2160p BDRemux HDR.mkv",
        )
        decision = compare_media_quality(
            incoming,
            probe(3840, 2160, hdr=True, dv=8),
            existing,
            probe(3840, 2160, hdr=True),
        )
        self.assertEqual(decision.winner, "incoming")
        self.assertIn("DV-P8/HDR", decision.incoming_summary)

    def test_sample_identical_files_keep_existing_catalog_copy(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 1080p WEB-DL.mkv",
            "Movie (2020) - 1080p WEB-DL copy.mkv",
            left_size=100,
            right_size=100,
        )
        existing.write_bytes(incoming.read_bytes())
        decision = compare_media_quality(
            incoming, probe(1920, 1080), existing, probe(1920, 1080)
        )
        self.assertEqual(decision.winner, "existing")
        self.assertIn("byte identity", decision.reason)

    def test_bitrate_must_have_a_meaningful_margin(self) -> None:
        incoming, existing = self._files(
            "Movie (2020) - 1080p WEB-DL.mkv",
            "Movie (2020) - 1080p WEB-DL copy.mkv",
        )
        close = compare_media_quality(
            incoming,
            probe(1920, 1080, bitrate=22_000_000),
            existing,
            probe(1920, 1080, bitrate=20_000_000),
        )
        clear = compare_media_quality(
            incoming,
            probe(1920, 1080, bitrate=26_000_000),
            existing,
            probe(1920, 1080, bitrate=20_000_000),
        )
        self.assertIsNone(close.winner)
        self.assertEqual(clear.winner, "incoming")

    def test_quality_report_includes_actual_media_characteristics(self) -> None:
        summary = quality_summary(
            Path("Movie (2020) - 2160p BDRemux.mkv"),
            probe(3840, 1608, hdr=True, audio=("truehd",), channels=8),
        )
        self.assertIn("3840x1608", summary)
        self.assertIn("BDRemux", summary)
        self.assertIn("HDR", summary)
        self.assertIn("truehd/8ch", summary)


class PlanningSafetyTests(unittest.TestCase):
    def test_root_inventory_excludes_hidden_nonvideo_and_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "movie.mkv").write_bytes(b"movie")
            (root / ".hidden.mkv").write_bytes(b"hidden")
            (root / "notes.txt").write_text("notes")
            (root / "alias.mkv").symlink_to("movie.mkv")
            self.assertEqual(
                [path.name for path in intake_media.root_video_candidates(root)],
                ["movie.mkv"],
            )

    def test_partial_candidates_are_never_settled(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "movie.partial.mkv"
            path.write_bytes(b"active")
            settled, issues = intake_media.settled_candidates([path], 0)
            self.assertEqual(settled, [])
            self.assertIn(".partial", issues[0].reason)

    def test_recovery_plan_restores_unique_prior_catalog_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".rusty-library").mkdir(parents=True)
            source = root / "барби.mkv"
            source.write_bytes(b"movie")
            destination = root / "comedy" / "Barbie (2023) - 2160p WEB-DL HDR.mkv"
            identity = MovieIdentity(
                "tt1517268",
                "Barbie",
                2023,
                ("Adventure", "Comedy", "Fantasy"),
                "imdb-title-year",
                "embedded-title",
            )
            with (
                mock.patch.object(intake_media, "settled_candidates", return_value=([source], [])),
                mock.patch.object(
                    intake_media,
                    "probe_media",
                    return_value=probe(3840, 1920, title="Barbie 2023 from seleZen", hdr=True),
                ),
                mock.patch.object(intake_media, "identify_movie", return_value=identity),
                mock.patch.object(
                    intake_media,
                    "broken_catalog_targets",
                    return_value={("barbie", 2023): [destination]},
                ),
                mock.patch.object(intake_media, "catalog_paths_for_imdb", return_value=[]),
            ):
                plans, issues, _tmdb = intake_media.plan_intake(
                    root,
                    settle_seconds=0,
                    minimum_confidence=85,
                    tmdb_token=None,
                    allow_network=False,
                )
            self.assertEqual(issues, [])
            self.assertEqual(len(plans), 1)
            self.assertEqual(plans[0].destination, destination)
            self.assertEqual(plans[0].confidence, 100)
            self.assertIn("unique-broken-catalog-target", plans[0].evidence)

    def test_reviewed_identity_override_selects_numbered_collection_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".rusty-library").mkdir(parents=True)
            source = root / "Example.2009.2160p.BDRemux.DV.HDR.mkv"
            source.write_bytes(b"movie")
            identity = MovieIdentity(
                "tt0000001",
                "Example",
                2009,
                ("Action", "Adventure", "Sci-Fi"),
                "imdb-title-year",
                "embedded-title",
            )
            with (
                mock.patch.object(
                    intake_media,
                    "MOVIE_INTAKE_OVERRIDES",
                    {"tt0000001": ("sci-fi/Example Collection", 2)},
                ),
                mock.patch.object(intake_media, "settled_candidates", return_value=([source], [])),
                mock.patch.object(
                    intake_media,
                    "probe_media",
                    return_value=probe(
                        3840,
                        2160,
                        title="Example 2009 2160p BDRemux DV HDR",
                        hdr=True,
                        dv=8,
                    ),
                ),
                mock.patch.object(intake_media, "identify_movie", return_value=identity),
                mock.patch.object(intake_media, "broken_catalog_targets", return_value={}),
                mock.patch.object(intake_media, "catalog_paths_for_imdb", return_value=[]),
            ):
                plans, issues, _tmdb = intake_media.plan_intake(
                    root,
                    settle_seconds=0,
                    minimum_confidence=85,
                    tmdb_token=None,
                    allow_network=False,
                )

            self.assertEqual(issues, [])
            self.assertEqual(
                plans[0].destination,
                root
                / "sci-fi"
                / "Example Collection"
                / "02 - Example (2009) - 2160p BDRemux DV HDR.mkv",
            )
            self.assertIn("reviewed-intake-override", plans[0].evidence)

    def test_lower_quality_duplicate_is_planned_for_preserved_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".rusty-library").mkdir(parents=True)
            source = root / "Movie (2020) - 1080p HDTV.mkv"
            source.write_bytes(b"incoming")
            existing = root / "action" / "Movie (2020) - 2160p BDRemux HDR.mkv"
            existing.parent.mkdir()
            existing.write_bytes(b"established-catalog")
            identity = MovieIdentity(
                "tt0000001",
                "Movie",
                2020,
                ("Action",),
                "imdb-title-year",
                "filename",
            )

            def inspect(path: Path) -> MediaProbe:
                return (
                    probe(1920, 1080)
                    if path == source
                    else probe(3840, 2160, hdr=True)
                )

            with (
                mock.patch.object(intake_media, "settled_candidates", return_value=([source], [])),
                mock.patch.object(intake_media, "probe_media", side_effect=inspect),
                mock.patch.object(intake_media, "identify_movie", return_value=identity),
                mock.patch.object(intake_media, "broken_catalog_targets", return_value={}),
                mock.patch.object(
                    intake_media, "catalog_paths_for_imdb", return_value=[existing]
                ),
            ):
                plans, issues, _tmdb = intake_media.plan_intake(
                    root,
                    settle_seconds=0,
                    minimum_confidence=85,
                    tmdb_token=None,
                    allow_network=False,
                )
            self.assertEqual(issues, [])
            self.assertEqual(plans[0].action, "archive-duplicate")
            self.assertEqual(plans[0].destination, existing)
            self.assertIn("Duplicates-Lower-Quality", str(plans[0].archived_path))

    def test_better_duplicate_gets_new_technical_name_and_archives_sidecars(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".rusty-library").mkdir(parents=True)
            source = root / "Movie.2020.2160p.BDRemux.HDR.mkv"
            source.write_bytes(b"incoming")
            existing = (
                root
                / "action"
                / "Movie Collection"
                / "02 - Movie (2020) - 2160p WEB-DL HDR.mkv"
            )
            existing.parent.mkdir(parents=True)
            existing.write_bytes(b"established-catalog")
            poster = existing.with_name(f"{existing.stem}-poster.jpg")
            poster.write_bytes(b"poster")
            preview = existing.parent / ".rusty_previews" / existing.stem
            preview.mkdir(parents=True)
            (preview / "manifest.json").write_text("{}")
            identity = MovieIdentity(
                "tt0000001",
                "Movie",
                2020,
                ("Action",),
                "imdb-title-year",
                "embedded-title",
            )

            def inspect(path: Path) -> MediaProbe:
                return (
                    probe(
                        3840,
                        2160,
                        title="Movie 2020 2160p BDRemux HDR",
                        hdr=True,
                        bitrate=70_000_000,
                        audio=("truehd",),
                        channels=8,
                        dv=8,
                    )
                    if path == source
                    else probe(3840, 2160, hdr=True, bitrate=20_000_000)
                )

            with (
                mock.patch.object(intake_media, "settled_candidates", return_value=([source], [])),
                mock.patch.object(intake_media, "probe_media", side_effect=inspect),
                mock.patch.object(intake_media, "identify_movie", return_value=identity),
                mock.patch.object(intake_media, "broken_catalog_targets", return_value={}),
                mock.patch.object(
                    intake_media, "catalog_paths_for_imdb", return_value=[existing]
                ),
            ):
                plans, issues, _tmdb = intake_media.plan_intake(
                    root,
                    settle_seconds=0,
                    minimum_confidence=85,
                    tmdb_token=None,
                    allow_network=False,
                )

            self.assertEqual(issues, [])
            plan = plans[0]
            self.assertEqual(plan.action, "replace")
            self.assertEqual(
                plan.destination,
                existing.with_name("02 - Movie (2020) - 2160p BDRemux DV HDR.mkv"),
            )
            archived = root / "to-review" / "Duplicates-Replaced" / existing.relative_to(root)
            self.assertIn((existing, archived), plan.mappings)
            self.assertIn(
                (poster, archived.with_name(f"{archived.stem}-poster.jpg")),
                plan.mappings,
            )
            self.assertIn(
                (
                    preview,
                    archived.parent / ".rusty_previews" / archived.stem,
                ),
                plan.mappings,
            )

    def test_apply_replacement_preserves_old_catalog_bytes_and_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            incoming = root / "Movie incoming.mkv"
            catalog = root / "action" / "Movie (2020) - 2160p.mkv"
            archived = (
                root
                / "to-review"
                / "Duplicates-Replaced"
                / "action"
                / catalog.name
            )
            incoming.write_bytes(b"better")
            catalog.parent.mkdir()
            catalog.write_bytes(b"older")
            identity = MovieIdentity(
                "tt0000001",
                "Movie",
                2020,
                ("Action",),
                "imdb-title-year",
                "filename",
            )
            plan = IntakePlan(
                source=incoming,
                destination=catalog,
                identity=identity,
                tmdb=None,
                confidence=100,
                evidence=("test",),
                mappings=((catalog, archived), (incoming, catalog)),
                action="replace",
                incumbent=catalog,
                archived_path=archived,
                quality_summary="incoming clearly wins",
            )
            destinations = intake_media.apply_intake(root, [plan])
            self.assertEqual(destinations, [catalog])
            self.assertEqual(catalog.read_bytes(), b"better")
            self.assertEqual(archived.read_bytes(), b"older")
            manifest = root / "to-review" / "Duplicates-Replaced" / "manifest.tsv"
            self.assertIn("incoming clearly wins", manifest.read_text())


if __name__ == "__main__":
    unittest.main()
