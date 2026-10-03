from __future__ import annotations

import errno
import importlib.util
import os
import re
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


sys.dont_write_bytecode = True
SCRIPTS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS_DIR))

from lib import artwork_names, paths  # noqa: E402

artwork_spec = importlib.util.spec_from_file_location(
    "fetch_dlna_artwork", SCRIPTS_DIR / "fetch-dlna-artwork.py"
)
assert artwork_spec is not None and artwork_spec.loader is not None
artwork = importlib.util.module_from_spec(artwork_spec)
sys.modules[artwork_spec.name] = artwork
artwork_spec.loader.exec_module(artwork)

maintain_spec = importlib.util.spec_from_file_location(
    "maintain_library", SCRIPTS_DIR / "maintain-library.py"
)
assert maintain_spec is not None and maintain_spec.loader is not None
maintain = importlib.util.module_from_spec(maintain_spec)
sys.modules[maintain_spec.name] = maintain
maintain_spec.loader.exec_module(maintain)

SCANNER_ARTWORK = SCRIPTS_DIR.parents[1] / "crates" / "scan" / "src" / "artwork.rs"
JPEG = b"\xff\xd8\xff" + b"\0" * 4096


def rust_names(source: str, constant: str) -> set[str]:
    match = re.search(rf"const {constant}: &\[&str\] = &\[(.*?)\];", source, re.DOTALL)
    assert match is not None, constant
    return {name.lower() for name in re.findall(r'"([^"]+)"', match.group(1))}


class UmaskTests(unittest.TestCase):
    def test_umask_is_read_without_changing_it(self) -> None:
        previous = os.umask(0o027)
        try:
            self.assertEqual(paths.current_umask(), 0o027)
            self.assertEqual(paths.created_file_mode(), 0o640)
            self.assertEqual(os.umask(0o027), 0o027)
        finally:
            os.umask(previous)


class ArtworkNameTests(unittest.TestCase):
    @unittest.skipUnless(SCANNER_ARTWORK.is_file(), "scanner source is not available")
    def test_names_match_the_scanner(self) -> None:
        source = SCANNER_ARTWORK.read_text(encoding="utf-8")
        self.assertEqual(
            set(artwork_names.STEM_ART_SUFFIXES), rust_names(source, "STEM_ART_SUFFIXES")
        )
        self.assertEqual(
            set(artwork_names.FOLDER_ART_NAMES), rust_names(source, "FOLDER_ART_NAMES")
        )

    def test_stem_and_folder_art_are_matched_ignoring_ascii_case(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "Movie (2001)-FANART.PNG").write_bytes(b"art")
            (directory / "Cover.jpg").write_bytes(b"art")
            (directory / "other-poster.jpg").write_bytes(b"art")
            self.assertEqual(
                artwork_names.recognized_artwork(directory, "movie (2001)", folder_art=False),
                [directory / "Movie (2001)-FANART.PNG"],
            )
            self.assertEqual(
                artwork_names.recognized_artwork(directory, None, folder_art=True),
                [directory / "Cover.jpg"],
            )

    def test_dangling_symlink_counts_as_present(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "folder.jpg").symlink_to(directory / "missing.jpg")
            self.assertEqual(
                artwork_names.recognized_artwork(directory, None, folder_art=True),
                [directory / "folder.jpg"],
            )


class PlanTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def movie_targets(self, *names: str, directory: str = "drama") -> list:
        folder = self.root / directory
        folder.mkdir(exist_ok=True)
        items = []
        for name in names:
            path = folder / name
            path.write_bytes(b"movie")
            items.append(path)
        return artwork.movie_artwork_targets(
            items, {path: f"tt{index:07d}" for index, path in enumerate(items, 1)}
        )

    def test_invalid_existing_poster_is_reported_and_kept(self) -> None:
        [target] = self.movie_targets("Movie (2001).mkv")
        large = b"\x89PNG\r\n\x1a\n" + b"\0" * 4096
        target.dest.write_bytes(large)
        pending, present, invalid = artwork.plan_targets([target], refetch_existing=False)
        self.assertEqual((pending, present, invalid), ([], 0, [target.dest]))
        self.assertEqual(target.dest.read_bytes(), large)

    def test_existing_operator_art_suppresses_fetch(self) -> None:
        [solo] = self.movie_targets("Solo (2001).mkv", directory="solo")
        (self.root / "solo" / "folder.jpg").write_bytes(b"art")
        [fanart] = self.movie_targets("Fan (2002).mkv", directory="fan")
        (self.root / "fan" / "Fan (2002)-fanart.png").write_bytes(b"art")
        disc = self.root / "disc" / "Disc (2003)"
        (disc / "BDMV").mkdir(parents=True)
        (disc / "BDMV" / "index.bdmv").touch()
        (disc / "cover.png").write_bytes(b"art")
        [disc_target] = artwork.movie_artwork_targets([disc], {disc: "tt0000009"})
        for refetch in (False, True):
            pending, present, invalid = artwork.plan_targets(
                [solo, fanart, disc_target], refetch_existing=refetch
            )
            self.assertEqual((pending, present, invalid), ([], 3, []))

    def test_shared_folder_art_does_not_block_per_movie_posters(self) -> None:
        targets = self.movie_targets("One (2001).mkv", "Two (2002).webm")
        (self.root / "drama" / "folder.jpg").write_bytes(b"art")
        pending, present, invalid = artwork.plan_targets(targets, refetch_existing=False)
        self.assertEqual([target for target, _ in pending], targets)
        self.assertEqual((present, invalid), (0, []))

    def test_refetch_replaces_only_the_fetchers_own_regular_file(self) -> None:
        own, linked = self.movie_targets("Own (2001).mkv", "Linked (2002).mkv")
        own.dest.write_bytes(JPEG)
        linked.dest.symlink_to(self.root / "elsewhere.jpg")
        pending, present, invalid = artwork.plan_targets([own, linked], refetch_existing=True)
        self.assertEqual(pending, [(own, True)])
        self.assertEqual((present, invalid), (0, [linked.dest]))


class MaintainPosterCheckTests(unittest.TestCase):
    """maintain-library's MISSING-POSTER check must agree with the fetcher."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def movies(self, directory: str, *names: str) -> list[Path]:
        folder = self.root / directory
        folder.mkdir(parents=True, exist_ok=True)
        paths = []
        for name in names:
            path = folder / name
            path.write_bytes(b"movie")
            paths.append(path)
        return paths

    def fetcher_pending(self, items: list[Path]) -> list[Path]:
        targets = artwork.movie_artwork_targets(
            items, {path: f"tt{index:07d}" for index, path in enumerate(items, 1)}
        )
        pending, _present, _invalid = artwork.plan_targets(targets, refetch_existing=False)
        return [target.dest for target, _replaces in pending]

    def test_lone_destination_with_folder_art_is_not_missing_a_poster(self) -> None:
        [movie] = self.movies("sci-fi/Coll", "Movie (2001).mkv")
        (self.root / "sci-fi" / "Coll" / "cover.jpg").write_bytes(b"art")
        # A sibling disc directory is its own item and does not share cover.jpg.
        disc = self.root / "sci-fi" / "Coll" / "Disc (2002)"
        (disc / "BDMV").mkdir(parents=True)
        (disc / "BDMV" / "index.bdmv").touch()
        (disc / "folder.jpg").write_bytes(b"art")
        self.assertEqual(self.fetcher_pending([movie]), [])
        self.assertEqual(maintain.missing_posters([movie, disc]), [])

    def test_movies_sharing_folder_art_are_still_missing_posters(self) -> None:
        movies = self.movies("drama", "One (2001).mkv", "Two (2002).webm")
        (self.root / "drama" / "folder.jpg").write_bytes(b"art")
        expected = [path.with_name(f"{path.stem}-poster.jpg") for path in movies]
        self.assertEqual(self.fetcher_pending(movies), expected)
        self.assertEqual(maintain.missing_posters(movies), expected)

    def test_stem_art_satisfies_a_shared_directory(self) -> None:
        one, two = self.movies("drama", "One (2001).mkv", "Two (2002).mkv")
        (self.root / "drama" / "One (2001)-FANART.png").write_bytes(b"art")
        self.assertEqual(
            maintain.missing_posters([one, two]),
            [two.with_name("Two (2002)-poster.jpg")],
        )


class PublicationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.previous_umask = os.umask(0o022)
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.source = self.root / "cache.jpg"
        self.source.write_bytes(JPEG)
        os.chmod(self.source, 0o666)

    def tearDown(self) -> None:
        self.temporary.cleanup()
        os.umask(self.previous_umask)

    def test_new_poster_follows_umask_and_leaves_source_inode_alone(self) -> None:
        dest = self.root / "Movie-poster.jpg"
        self.assertEqual(artwork.place_jpeg(self.source, dest), "written")
        self.assertEqual(stat.S_IMODE(dest.stat().st_mode), 0o644)
        self.assertEqual(stat.S_IMODE(self.source.stat().st_mode), 0o666)
        self.assertNotEqual(dest.stat().st_ino, self.source.stat().st_ino)
        self.assertEqual(sorted(path.name for path in self.root.iterdir()), ["Movie-poster.jpg", "cache.jpg"])

    def test_new_poster_never_replaces_or_writes_through_an_entry(self) -> None:
        existing = self.root / "Existing-poster.jpg"
        existing.write_bytes(b"tiny")
        self.assertEqual(artwork.place_jpeg(self.source, existing), "exists")
        self.assertEqual(existing.read_bytes(), b"tiny")

        outside = self.root / "outside"
        outside.mkdir()
        dangling = self.root / "Dangling-poster.jpg"
        dangling.symlink_to(outside / "target.jpg")
        self.assertEqual(artwork.place_jpeg(self.source, dangling), "exists")
        self.assertFalse((outside / "target.jpg").exists())
        self.assertTrue(dangling.is_symlink())

    def test_exclusive_fallback_when_hard_links_are_unsupported(self) -> None:
        for error in (
            PermissionError(errno.EPERM, "Operation not permitted"),
            OSError(errno.ENOSYS, "Function not implemented"),
            PermissionError(errno.EACCES, "Permission denied"),
        ):
            with self.subTest(errno=error.errno):
                dest = self.root / f"Fallback{error.errno}-poster.jpg"
                with mock.patch.object(artwork.os, "link", side_effect=error):
                    self.assertEqual(artwork.place_jpeg(self.source, dest), "written")
                self.assertEqual(dest.read_bytes(), JPEG)
                self.assertEqual(stat.S_IMODE(dest.stat().st_mode), 0o644)

    def test_replacement_follows_umask(self) -> None:
        dest = self.root / "Movie-poster.jpg"
        dest.write_bytes(b"old")
        os.chmod(dest, 0o666)
        self.assertEqual(artwork.replace_jpeg(self.source, dest), "replaced")
        self.assertEqual(stat.S_IMODE(dest.stat().st_mode), 0o644)
        self.assertEqual(dest.read_bytes(), JPEG)

    def test_regeneration_keeps_existing_mode(self) -> None:
        dest = self.root / "poster.jpg"
        dest.write_bytes(JPEG)
        os.chmod(dest, 0o640)

        def convert(_source: Path, output: Path) -> bool:
            output.write_bytes(JPEG + b"x")
            return True

        with mock.patch.object(artwork, "ffmpeg_to_jpeg", side_effect=convert):
            self.assertIsNone(artwork.regenerate_jpeg(dest))
        self.assertEqual(dest.read_bytes(), JPEG + b"x")
        self.assertEqual(stat.S_IMODE(dest.stat().st_mode), 0o640)


if __name__ == "__main__":
    unittest.main()
