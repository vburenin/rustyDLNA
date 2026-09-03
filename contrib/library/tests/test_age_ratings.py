from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))
sys.path.insert(0, str(SCRIPTS_DIR / "lib"))

from lib.age_ratings import (  # noqa: E402
    AgeRating,
    WIKIDATA_CERTIFICATIONS,
    category_for_minimum_age,
    classify_certification,
    rating_severity,
    select_region_rating,
)

audit_spec = importlib.util.spec_from_file_location(
    "find_unclassified_videos", SCRIPTS_DIR / "find-unclassified-videos.py"
)
assert audit_spec is not None and audit_spec.loader is not None
audit_module = importlib.util.module_from_spec(audit_spec)
audit_spec.loader.exec_module(audit_module)
linked_video_identities = audit_module.linked_video_identities

age_builder_spec = importlib.util.spec_from_file_location(
    "build_age_links", SCRIPTS_DIR / "lib" / "build-age-links.py"
)
assert age_builder_spec is not None and age_builder_spec.loader is not None
age_builder_module = importlib.util.module_from_spec(age_builder_spec)
sys.modules[age_builder_spec.name] = age_builder_module
age_builder_spec.loader.exec_module(age_builder_module)
cumulative_age_categories = age_builder_module.cumulative_age_categories
cumulative_minimum_age = age_builder_module.cumulative_minimum_age
certification_category = age_builder_module.certification_category
generated_age_links = age_builder_module.generated_age_links
generated_rating_link = age_builder_module.generated_rating_link


class MinimumAgeCategoryTests(unittest.TestCase):
    def test_exact_minimum_ages_are_not_rounded_down(self) -> None:
        self.assertEqual(category_for_minimum_age(0), "ALL_AGES")
        self.assertEqual(category_for_minimum_age(4), "AGE_04_PLUS")
        self.assertEqual(category_for_minimum_age(13), "AGE_13_PLUS")
        self.assertEqual(category_for_minimum_age(18), "AGE_18_PLUS")

    def test_invalid_minimum_ages_are_rejected(self) -> None:
        for value in (-1, 19):
            with self.subTest(value=value), self.assertRaises(ValueError):
                category_for_minimum_age(value)


class CumulativeAgeViewTests(unittest.TestCase):
    def test_numeric_zero_override_starts_at_one_year(self) -> None:
        categories = cumulative_age_categories(0)
        self.assertNotIn("00_YEARS", categories)
        self.assertIn("01_YEARS", categories)

    def test_all_ages_certification_is_not_a_numeric_age(self) -> None:
        self.assertIsNone(cumulative_minimum_age("ALL_AGES", 0))
        links = generated_age_links(
            "ALL_AGES", 0, "Movies", Path("Movie (2020).mkv")
        )
        self.assertEqual(
            links,
            (Path("BY_AGE/ALL_AGES/Movies/Movie (2020).mkv"),),
        )

    def test_age_ten_contains_every_numeric_rating_up_to_ten(self) -> None:
        for minimum_age in range(0, 11):
            with self.subTest(minimum_age=minimum_age):
                self.assertIn(
                    "10_YEARS", cumulative_age_categories(minimum_age)
                )

        for minimum_age in range(11, 19):
            with self.subTest(minimum_age=minimum_age):
                self.assertNotIn(
                    "10_YEARS", cumulative_age_categories(minimum_age)
                )

    def test_parental_guidance_enters_cumulative_view_at_thirteen(
        self,
    ) -> None:
        self.assertEqual(
            cumulative_minimum_age("PARENTAL_GUIDANCE", None), 13
        )
        links = generated_age_links(
            "PARENTAL_GUIDANCE", None, "Movies", Path("Movie (2020).mkv")
        )
        self.assertIn(
            Path("UNTIL_AGE/13_YEARS/Movies/Movie (2020).mkv"), links
        )
        self.assertNotIn(
            Path("UNTIL_AGE/12_YEARS/Movies/Movie (2020).mkv"), links
        )

    def test_unrated_items_are_excluded_from_cumulative_view(self) -> None:
        self.assertEqual(cumulative_age_categories(None), ())
        self.assertEqual(cumulative_minimum_age("UNRATED", None), None)

    def test_low_foreign_certification_is_delayed_conservatively(self) -> None:
        self.assertEqual(
            cumulative_minimum_age(
                "AGE_06_PLUS",
                6,
                rating_region="DE",
                rating_source="wikidata-certification",
            ),
            13,
        )

    def test_reviewed_age_is_not_delayed(self) -> None:
        self.assertEqual(
            cumulative_minimum_age(
                "AGE_06_PLUS",
                6,
                rating_region="REVIEWED",
                rating_source="reviewed-override",
            ),
            6,
        )

    def test_us_age_specific_certification_keeps_its_age(self) -> None:
        self.assertEqual(
            cumulative_minimum_age(
                "AGE_07_PLUS",
                7,
                rating_region="US",
                rating_source="tmdb-certification",
            ),
            7,
        )

    def test_generated_links_keep_exact_and_cumulative_views(self) -> None:
        links = generated_age_links(
            "AGE_10_PLUS", 10, "Movies", Path("Movie (2020).mkv")
        )
        self.assertEqual(
            links[0],
            Path("BY_AGE/AGE_10_PLUS/Movies/Movie (2020).mkv"),
        )
        self.assertIn(
            Path("UNTIL_AGE/10_YEARS/Movies/Movie (2020).mkv"),
            links,
        )
        self.assertNotIn(
            Path("UNTIL_AGE/09_YEARS/Movies/Movie (2020).mkv"),
            links,
        )


class CertificationViewTests(unittest.TestCase):
    def test_common_certifications_have_direct_categories(self) -> None:
        for certification in ("G", "PG", "PG-13", "R", "TV-PG"):
            with self.subTest(certification=certification):
                self.assertEqual(
                    certification_category(certification), certification
                )

    def test_certification_categories_are_path_safe(self) -> None:
        self.assertEqual(certification_category("FSK 12"), "FSK_12")
        self.assertEqual(certification_category(""), "UNRATED")
        self.assertEqual(
            generated_rating_link(
                "PG", "Movies", Path("Movie (2020).mkv")
            ),
            Path("BY_RATING/PG/Movies/Movie (2020).mkv"),
        )


class CertificationMappingTests(unittest.TestCase):
    def test_us_movie_certifications(self) -> None:
        expected = {
            "G": ("ALL_AGES", 0),
            "PG": ("PARENTAL_GUIDANCE", None),
            "PG-13": ("AGE_13_PLUS", 13),
            "R": ("AGE_17_PLUS", 17),
            "NC-17": ("AGE_18_PLUS", 18),
        }
        for certification, classification in expected.items():
            with self.subTest(certification=certification):
                rating = classify_certification(
                    "movie", "US", certification
                )
                self.assertIsNotNone(rating)
                assert rating is not None
                self.assertEqual(
                    (rating.category, rating.minimum_age), classification
                )

    def test_us_tv_certifications(self) -> None:
        expected = {
            "TV-Y": ("ALL_AGES", 0),
            "TV-Y7": ("AGE_07_PLUS", 7),
            "TV-G": ("ALL_AGES", 0),
            "TV-PG": ("PARENTAL_GUIDANCE", None),
            "TV-14": ("AGE_14_PLUS", 14),
            "TV-MA": ("AGE_18_PLUS", 18),
        }
        for certification, classification in expected.items():
            with self.subTest(certification=certification):
                rating = classify_certification(
                    "show", "US", certification
                )
                self.assertIsNotNone(rating)
                assert rating is not None
                self.assertEqual(
                    (rating.category, rating.minimum_age), classification
                )

    def test_unsupported_values_are_not_guessed(self) -> None:
        self.assertIsNone(classify_certification("movie", "GB", "12"))
        self.assertIsNone(classify_certification("movie", "US", "UNRATED"))

    def test_strictness_order_is_conservative(self) -> None:
        pg = classify_certification("movie", "US", "PG")
        pg13 = classify_certification("movie", "US", "PG-13")
        restricted = classify_certification("movie", "US", "R")
        assert pg is not None and pg13 is not None and restricted is not None
        self.assertLess(rating_severity(pg), rating_severity(pg13))
        self.assertLess(rating_severity(pg13), rating_severity(restricted))

    def test_wikidata_ids_have_reviewed_cross_region_mappings(self) -> None:
        expected = {
            ("P1657", "Q18665339"): ("US", "PG-13", "AGE_13_PLUS", 13),
            ("P1981", "Q20644797"): ("DE", "FSK 16", "AGE_16_PLUS", 16),
            ("P2629", "Q4550895"): ("GB", "15", "AGE_15_PLUS", 15),
            ("P2637", "Q23308564"): ("RU", "18+", "AGE_18_PLUS", 18),
            ("P2684", "Q23649982"): ("NL", "9", "AGE_09_PLUS", 9),
            ("P2756", "Q23790279"): ("JP", "PG12", "AGE_12_PLUS", 12),
            ("P3216", "Q26678734"): ("BR", "14", "AGE_14_PLUS", 14),
            ("P3306", "Q27253940"): ("ES", "7", "AGE_07_PLUS", 7),
        }
        for identifiers, classification in expected.items():
            with self.subTest(identifiers=identifiers):
                self.assertEqual(
                    WIKIDATA_CERTIFICATIONS[identifiers], classification
                )

    def test_preferred_region_beats_a_stricter_foreign_outlier(self) -> None:
        ratings = {
            AgeRating(
                "PARENTAL_GUIDANCE",
                None,
                "US",
                "PG",
                "wikidata-certification",
                None,
                "wikidata-imdb-id",
            ),
            AgeRating(
                "ALL_AGES",
                0,
                "DE",
                "FSK 0",
                "wikidata-certification",
                None,
                "wikidata-imdb-id",
            ),
            AgeRating(
                "AGE_18_PLUS",
                18,
                "RU",
                "18+",
                "wikidata-certification",
                None,
                "wikidata-imdb-id",
            ),
        }
        selected = select_region_rating(ratings, "US")
        self.assertIsNotNone(selected)
        assert selected is not None
        self.assertEqual(
            (selected.region, selected.category),
            ("US", "PARENTAL_GUIDANCE"),
        )


class AuditIsolationTests(unittest.TestCase):
    def test_age_links_do_not_count_as_genre_classification(self) -> None:
        for relative_link in (
            Path("BY_AGE/ALL_AGES/Movies/Movie (2020).mkv"),
            Path("UNTIL_AGE/10_YEARS/Movies/Movie (2020).mkv"),
            Path("BY_RATING/G/Movies/Movie (2020).mkv"),
        ):
            with (
                self.subTest(relative_link=relative_link),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                genres = root / "genres"
                movie = root / "action" / "Movie (2020).mkv"
                movie.parent.mkdir()
                movie.write_bytes(b"video")
                link = genres / relative_link
                link.parent.mkdir(parents=True)
                link.symlink_to(movie)

                self.assertEqual(linked_video_identities(genres), set())


if __name__ == "__main__":
    unittest.main()
