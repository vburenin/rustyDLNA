from __future__ import annotations

import csv
import importlib.util
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
from unittest import mock


sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
module_spec = importlib.util.spec_from_file_location(
    "fetch_movie_descriptions", SCRIPTS_DIR / "fetch-movie-descriptions.py"
)
assert module_spec is not None and module_spec.loader is not None
descriptions = importlib.util.module_from_spec(module_spec)
sys.modules[module_spec.name] = descriptions
module_spec.loader.exec_module(descriptions)


class DescriptionTextTests(unittest.TestCase):
    def test_normalize_description_is_bounded_and_removes_xml_controls(self) -> None:
        self.assertEqual(
            descriptions.normalize_description("  One\t two\r\n\x01Three  "),
            "One two\nThree",
        )
        self.assertEqual(descriptions.normalize_description("N/A"), "")
        self.assertEqual(
            descriptions.normalize_description(
                "x" * (descriptions.MAX_DESCRIPTION_BYTES + 1)
            ),
            "",
        )

    def test_rendered_nfo_escapes_text_and_keeps_descriptions_separate(self) -> None:
        movie = descriptions.IndexedMovie(
            Path("action/A & B (2020).mkv"), "tt1234567", 42
        )
        rendered = descriptions.render_nfo(
            movie,
            "Safe <outline> & introduction",
            "Full plot > ending",
        )
        root = ET.fromstring(rendered)
        self.assertEqual(root.tag, "movie")
        self.assertEqual(root.findtext("outline"), "Safe <outline> & introduction")
        self.assertEqual(root.findtext("plot"), "Full plot > ending")
        self.assertEqual(root.findtext("uniqueid[@type='imdb']"), "tt1234567")
        self.assertIn(descriptions.MANAGED_MARKER, rendered)


class DescriptionIndexTests(unittest.TestCase):
    def test_index_requires_exact_ids_and_rejects_path_escape(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            index = Path(temporary) / "index.tsv"
            with index.open("w", encoding="utf-8", newline="") as handle:
                writer = csv.DictWriter(
                    handle,
                    fieldnames=("source", "imdb_id", "tmdb_id"),
                    delimiter="\t",
                    lineterminator="\n",
                )
                writer.writeheader()
                writer.writerow(
                    {
                        "source": "action/Film (2020).mkv",
                        "imdb_id": "tt1234567",
                        "tmdb_id": "42",
                    }
                )
                writer.writerow(
                    {
                        "source": "../escape.mkv",
                        "imdb_id": "tt7654321",
                        "tmdb_id": "24",
                    }
                )
                writer.writerow(
                    {
                        "source": "drama/Unknown (2020).mkv",
                        "imdb_id": "",
                        "tmdb_id": "",
                    }
                )
            movies, issues = descriptions.load_index(index)
        self.assertEqual(
            movies,
            [
                descriptions.IndexedMovie(
                    Path("action/Film (2020).mkv"), "tt1234567", 42
                )
            ],
        )
        self.assertEqual(len(issues), 2)

    def test_same_stem_media_collision_is_rejected_before_writes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            index = Path(temporary) / "index.tsv"
            with index.open("w", encoding="utf-8", newline="") as handle:
                writer = csv.DictWriter(
                    handle,
                    fieldnames=("source", "imdb_id", "tmdb_id"),
                    delimiter="\t",
                    lineterminator="\n",
                )
                writer.writeheader()
                writer.writerow(
                    {
                        "source": "action/Film (2020).mkv",
                        "imdb_id": "tt1234567",
                        "tmdb_id": "42",
                    }
                )
                writer.writerow(
                    {
                        "source": "action/Film (2020).mp4",
                        "imdb_id": "tt1234567",
                        "tmdb_id": "42",
                    }
                )
            movies, issues = descriptions.load_index(index)
        self.assertEqual(movies, [])
        self.assertTrue(any("NFO collision" in issue for issue in issues))


class DescriptionSidecarTests(unittest.TestCase):
    def test_publish_never_replaces_an_unmanaged_nfo(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "movie.nfo"
            path.write_text("<movie><plot>hand written</plot></movie>\n", encoding="utf-8")
            status = descriptions.publish_nfo(
                path,
                f"<!-- {descriptions.MANAGED_MARKER} -->\n<movie />\n",
            )
            self.assertEqual(status, "protected")
            self.assertIn("hand written", path.read_text(encoding="utf-8"))

    def test_publish_updates_only_owned_sidecars_atomically(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "movie.nfo"
            first = f"<!-- {descriptions.MANAGED_MARKER} -->\n<movie />\n"
            second = f"<!-- {descriptions.MANAGED_MARKER} -->\n<movie><plot>new</plot></movie>\n"
            self.assertEqual(descriptions.publish_nfo(path, first), "written")
            self.assertEqual(descriptions.publish_nfo(path, first), "unchanged")
            self.assertEqual(descriptions.publish_nfo(path, second), "written")
            self.assertEqual(path.read_text(encoding="utf-8"), second)
            self.assertEqual(list(path.parent.glob(".*.tmp")), [])


class DescriptionRemoteTests(unittest.TestCase):
    def test_omdb_keys_are_distinct_and_keep_configured_order(self) -> None:
        with mock.patch.dict(
            descriptions.os.environ,
            {"OMDB_API_KEYS": "first, second\nfirst", "OMDB_API_KEY": "legacy"},
            clear=True,
        ):
            self.assertEqual(
                descriptions.configured_omdb_keys(),
                ["first", "second", "legacy"],
            )

    def test_omdb_falls_back_without_exposing_credentials(self) -> None:
        with mock.patch.object(
            descriptions,
            "fetch_omdb_plot",
            side_effect=[
                descriptions.RemoteError("request limit reached"),
                {"status": "ok", "imdb_id": "tt1234567", "plot": "full"},
            ],
        ) as fetch:
            with mock.patch.object(descriptions.sys, "stderr"):
                record, key_index = descriptions.fetch_omdb_plot_with_fallback(
                    "tt1234567", ["secret-one", "secret-two"], 0
                )
        self.assertEqual(record["plot"], "full")
        self.assertEqual(key_index, 1)
        self.assertEqual(fetch.call_count, 2)

    def test_remote_records_must_preserve_exact_identity(self) -> None:
        with mock.patch.object(
            descriptions,
            "request_json",
            return_value={
                "id": 42,
                "imdb_id": "tt1234567",
                "overview": "A safe overview.",
            },
        ):
            record = descriptions.fetch_tmdb_overview(42, "tt1234567", "token")
        self.assertEqual(record["overview"], "A safe overview.")

        with mock.patch.object(
            descriptions,
            "request_json",
            return_value={
                "Response": "True",
                "imdbID": "tt1234567",
                "Plot": "The full ending.",
            },
        ):
            record = descriptions.fetch_omdb_plot("tt1234567", "key")
        self.assertEqual(record["plot"], "The full ending.")

    def test_conflicting_remote_identity_is_rejected(self) -> None:
        with mock.patch.object(
            descriptions,
            "request_json",
            return_value={"id": 7, "imdb_id": "tt9999999", "overview": "wrong"},
        ):
            with self.assertRaises(descriptions.RemoteError):
                descriptions.fetch_tmdb_overview(42, "tt1234567", "token")

    def test_omdb_quota_error_is_not_cached_as_not_found(self) -> None:
        with mock.patch.object(
            descriptions,
            "request_json",
            return_value={"Response": "False", "Error": "Request limit reached!"},
        ):
            with self.assertRaises(descriptions.RemoteError):
                descriptions.fetch_omdb_plot("tt1234567", "key")

    def test_omdb_missing_movie_is_cacheable(self) -> None:
        with mock.patch.object(
            descriptions,
            "request_json",
            return_value={"Response": "False", "Error": "Incorrect IMDb ID."},
        ):
            self.assertEqual(
                descriptions.fetch_omdb_plot("tt1234567", "key"),
                {"status": "not-found"},
            )


if __name__ == "__main__":
    unittest.main()
