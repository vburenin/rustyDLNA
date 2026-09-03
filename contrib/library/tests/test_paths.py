from __future__ import annotations

import os
import re
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


sys.dont_write_bytecode = True
TOOLS_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS_DIR))

from lib.paths import (  # noqa: E402
    LibraryRootError,
    resolve_library_root,
    state_dir,
    tools_dir,
)


FORBIDDEN_CREDENTIAL_EXPORTS = (
    re.compile(r"export TMDB_API_TOKEN='[^']+'"),
    re.compile(r"export TMDB_API_KEY='[^']+'"),
    re.compile(r"export OMDB_API_KEYS='[^']+'"),
    re.compile(r"export OMDB_API_KEY='[^']+'"),
)


class LibraryPathTests(unittest.TestCase):
    def test_tools_dir_is_contrib_library(self) -> None:
        self.assertEqual(tools_dir(), TOOLS_DIR)
        self.assertTrue((tools_dir() / "update.sh").is_file())

    def test_state_dir_is_hidden_library_child(self) -> None:
        root = Path("/media/library")
        self.assertEqual(state_dir(root), root / ".rusty-library")

    def test_explicit_root_wins_over_environment(self) -> None:
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            with mock.patch.dict(
                os.environ, {"RUSTY_DLNA_MEDIA": second}, clear=False
            ):
                self.assertEqual(
                    resolve_library_root(Path(first)),
                    Path(first).resolve(),
                )

    def test_media_env_is_used_when_root_is_omitted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with mock.patch.dict(
                os.environ,
                {
                    "RUSTY_DLNA_LIBRARY_ROOT": "",
                    "LIBRARY_ROOT": "",
                    "RUSTY_DLNA_MEDIA": temporary,
                },
                clear=False,
            ):
                self.assertEqual(
                    resolve_library_root(),
                    Path(temporary).resolve(),
                )

    def test_missing_root_is_an_error(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "RUSTY_DLNA_LIBRARY_ROOT": "",
                "LIBRARY_ROOT": "",
                "RUSTY_DLNA_MEDIA": "",
            },
            clear=False,
        ):
            with self.assertRaisesRegex(LibraryRootError, "RUSTY_DLNA_MEDIA"):
                resolve_library_root()

    def test_missing_directory_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / "no-such-library"
            with self.assertRaisesRegex(LibraryRootError, "not a directory"):
                resolve_library_root(missing)


class CredentialHygieneTests(unittest.TestCase):
    def test_tooling_does_not_embed_provider_credentials(self) -> None:
        hits: list[str] = []
        for path in TOOLS_DIR.rglob("*"):
            if not path.is_file() or path.suffix in {".pyc"}:
                continue
            if "__pycache__" in path.parts or path.name == "test_paths.py":
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                continue
            for pattern in FORBIDDEN_CREDENTIAL_EXPORTS:
                if pattern.search(text):
                    hits.append(f"{path.relative_to(TOOLS_DIR)}: {pattern.pattern}")
        self.assertEqual(hits, [])


if __name__ == "__main__":
    unittest.main()
