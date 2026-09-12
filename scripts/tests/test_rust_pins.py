"""Exercise the real checker/updater using isolated repository copies."""

import importlib.util
import contextlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import sys
import unittest
from unittest.mock import patch


REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("rust_pins", REPO / "scripts/rust-pins.py")
PINS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PINS)


class RustPinsTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rustydlna-rust-pins-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for name in (*PINS.FILES, ".github/workflows/rust-toolchain-update.yml",
                     "scripts/rust-pins.py", "scripts/set-rust-version.sh",
                     "scripts/release-contract.sh", "CHANGELOG.md"):
            dest = self.root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO / name, dest)
        self.manifest = self.root / "manifest.toml"
        self.manifest.write_text('[pkg.rustc]\nversion = "9.88.7 (abcdef123 2027-01-02)"\n')
        self.env = dict(os.environ, RUSTY_DLNA_UPDATE_ROOT=str(self.root),
                        RUSTY_DLNA_RUST_MANIFEST=str(self.manifest))

    def run_command(self, *args):
        return subprocess.run(args, cwd=self.root, env=self.env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)

    def snapshot(self):
        return {name: (self.root / name).read_bytes() for name in PINS.FILES}

    def update(self, digest="sha256:" + "a" * 64):
        return self.run_command("sh", "scripts/set-rust-version.sh", "9.88.7", digest)

    def test_updates_all_pins_and_compiler_metadata_and_release_contract(self):
        before = self.snapshot()
        modes = {name: (self.root / name).stat().st_mode for name in PINS.FILES}
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        for name, original in before.items():
            self.assertNotEqual((self.root / name).read_bytes(), original, name)
            self.assertEqual((self.root / name).stat().st_mode, modes[name], name)
        compose = (self.root / "docker-compose.test.yaml").read_text()
        self.assertIn('rustc 9.88.7 (abcdef123 2027-01-02)', compose)
        self.assertIn('rust:9.88.7-trixie@sha256:' + 'a' * 64, compose)
        checked = self.run_command("python3", "scripts/rust-pins.py", "check")
        self.assertEqual(checked.returncode, 0, checked.stderr)
        package = PINS.unique(r'^version = "([^"\n]+)"$',
                              (self.root / "Cargo.toml").read_text(), "package")
        release = self.run_command("sh", "scripts/release-contract.sh", "v" + package)
        self.assertEqual(release.returncode, 0, release.stderr)
        staged = self.run_command("python3", "scripts/rust-pins.py", "files")
        self.assertEqual(set(staged.stdout.splitlines()), set(PINS.FILES))

    def test_manifest_version_reads_rustc_even_when_cargo_comes_first(self):
        self.manifest.write_text(
            '[pkg.cargo]\nversion = "0.99.0 (123456789 2027-01-01)"\n'
            '[pkg.rustc]\nversion = "9.88.7 (abcdef123 2027-01-02)"\n'
            '[pkg.rustc.target.x86_64-unknown-linux-gnu]\navailable = true\n')
        result = self.run_command("python3", "scripts/rust-pins.py", "manifest-version", str(self.manifest))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "9.88.7\n")
        update = self.update()
        self.assertEqual(update.returncode, 0, update.stderr)
        self.assertIn('rustc 9.88.7 (abcdef123 2027-01-02)',
                      (self.root / "docker-compose.test.yaml").read_text())

    def test_manifest_version_rejects_missing_duplicate_and_malformed_rustc(self):
        valid = '[pkg.rustc]\nversion = "9.88.7 (abcdef123 2027-01-02)"\n'
        for data in (
            b'[pkg.cargo]\nversion = "0.99.0 (123456789 2027-01-01)"\n',
            (valid + valid).encode(),
            (valid + 'version = "9.88.7 (abcdef123 2027-01-02)"\n').encode(),
            b'[pkg.rustc]\nversion = "9.88.7"\n',
            b'[pkg.rustc]\nversion = "nightly (abcdef123 2027-01-02)"\n',
            b'\xff', b'x' * 2_000_001,
        ):
            with self.subTest(manifest=data[:60]):
                self.manifest.write_bytes(data)
                before = self.snapshot()
                result = self.run_command("python3", "scripts/rust-pins.py", "manifest-version", str(self.manifest))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertEqual(self.snapshot(), before)

    def test_metadata_failures_do_not_mutate_any_pin(self):
        for metadata in ('', '[pkg.rustc]\nversion = "1.2.3 (abcdef123 2027-01-02)"\n',
                         '[pkg.rustc]\nversion = "9.88.7"\n', 'x' * 2_000_001):
            with self.subTest(metadata=metadata[:60]):
                self.manifest.write_text(metadata)
                before = self.snapshot()
                self.assertNotEqual(self.update().returncode, 0)
                self.assertEqual(self.snapshot(), before)
        self.manifest.unlink()
        before = self.snapshot()
        self.assertNotEqual(self.update().returncode, 0)
        self.assertEqual(self.snapshot(), before)

    def test_invalid_digest_leaves_all_files_intact(self):
        before = self.snapshot()
        self.assertNotEqual(self.update("sha256:bad").returncode, 0)
        self.assertEqual(self.snapshot(), before)

    def test_downgrade_leaves_all_files_intact(self):
        before = self.snapshot()
        result = self.run_command("sh", "scripts/set-rust-version.sh", "1.0.0", "sha256:" + "a" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("downgrades", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_docker_compose_and_workflow_drift_fail_before_update(self):
        for name, transform in (
            ("Dockerfile", lambda text: PINS.re.sub(PINS.IMAGE, 'rust:9.0.0-trixie@sha256:' + 'b' * 64, text)),
            ("docker-compose.test.yaml", lambda text: PINS.re.sub(PINS.IMAGE, 'rust:9.0.0-trixie@sha256:' + 'b' * 64, text)),
            ("docker-compose.test.yaml", lambda text: PINS.re.sub(r"sha256:[0-9a-f]{64}", 'sha256:' + 'b' * 64, text)),
            ("docker-compose.test.yaml", lambda text: PINS.re.sub(PINS.COMPILER, 'rustc 9.0.0 (abcdef123 2027-01-02)', text)),
            (".github/workflows/ci.yml", lambda text: PINS.re.sub(r"rustup default [0-9.]+", 'rustup default 9.0.0', text, count=1)),
            (".github/workflows/rust-toolchain-update.yml", lambda text: text.replace('python3 scripts/rust-pins.py files', 'echo Cargo.toml')),
            (".github/workflows/rust-toolchain-update.yml", lambda text: text.replace('python3 scripts/rust-pins.py manifest-version', 'echo 0.99.0')),
        ):
            with self.subTest(file=name):
                path = self.root / name
                original = path.read_text()
                path.write_text(transform(original))
                before = self.snapshot()
                checked = self.run_command("python3", "scripts/rust-pins.py", "check")
                self.assertNotEqual(checked.returncode, 0)
                self.assertNotEqual(self.update().returncode, 0)
                self.assertEqual(self.snapshot(), before)
                path.write_text(original)

    def invoke_update(self):
        with patch.dict(os.environ, self.env), patch.object(sys, "argv", [
                "rust-pins.py", "update", "9.88.7", "sha256:" + "a" * 64]):
            PINS.main()

    def test_staging_failure_leaves_every_pin_intact_and_cleans_temporary_files(self):
        before = self.snapshot()
        original = PINS.tempfile.mkstemp
        attempts = 0

        def fail_staging(*args, **kwargs):
            nonlocal attempts
            attempts += 1
            if attempts == 3:
                raise OSError("fixture staging disk full")
            return original(*args, **kwargs)

        with patch.object(PINS.tempfile, "mkstemp", fail_staging):
            with self.assertRaisesRegex(OSError, "staging disk full"):
                self.invoke_update()
        self.assertEqual(self.snapshot(), before)
        self.assertEqual(list(self.root.rglob(".rust-pins-*")), [])

    def test_failed_replacement_restores_prior_pins_without_retrying_failed_target(self):
        before = self.snapshot()
        original = PINS.os.replace

        def fail_replacement(source, destination):
            if destination == self.root / "Cargo.toml":
                raise PermissionError("fixture Cargo replacement denied")
            return original(source, destination)

        with patch.object(PINS.os, "replace", fail_replacement):
            with self.assertRaisesRegex(PermissionError, "replacement denied"):
                self.invoke_update()
        self.assertEqual(self.snapshot(), before)
        self.assertEqual(list(self.root.rglob(".rust-pins-*")), [])

    def test_rollback_preserves_external_edits_after_replacement(self):
        before = self.snapshot()
        original = PINS.os.replace
        edited = self.root / "rust-toolchain.toml"
        external = b"# Concurrent external edit\n" + before["rust-toolchain.toml"]

        def external_edit_and_fail(source, destination):
            if destination == self.root / "Cargo.toml":
                edited.write_bytes(external)
                raise PermissionError("fixture Cargo replacement denied")
            return original(source, destination)

        diagnostics = io.StringIO()
        with patch.object(PINS.os, "replace", external_edit_and_fail), contextlib.redirect_stderr(diagnostics):
            with self.assertRaisesRegex(PermissionError, "replacement denied"):
                self.invoke_update()
        self.assertIn("preserving external edit", diagnostics.getvalue())
        expected = {**before, "rust-toolchain.toml": external}
        self.assertEqual(self.snapshot(), expected)
        self.assertEqual(list(self.root.rglob(".rust-pins-*")), [])


if __name__ == "__main__":
    unittest.main()
