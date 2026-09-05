from __future__ import annotations

import errno
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from lib import intake_media, safe_move


class IntakeExecutionTests(unittest.TestCase):
    def plan(self, mappings):
        return intake_media.IntakePlan(
            source=mappings[0][0], destination=mappings[0][1],
            identity=intake_media.MovieIdentity(
                "tt0000001", "Movie", 2020, ("Drama",), "test", "test"
            ),
            tmdb=None, confidence=100, evidence=("test",), mappings=tuple(mappings),
        )

    def test_existing_entries_survive_planning_and_execution(self):
        for kind in ("regular", "dangling", "symlink", "directory"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                source, dest = root / "incoming", root / "catalog"
                source.write_bytes(b"incoming")
                if kind == "regular":
                    dest.write_bytes(b"incumbent")
                elif kind == "directory":
                    dest.mkdir()
                else:
                    dest.symlink_to(source if kind == "symlink" else root / "missing")
                before = dest.lstat()
                self.assertEqual(intake_media._mapping_collision(((source, dest),)), dest)
                with self.assertRaises(FileExistsError):
                    intake_media.apply_intake(root, [self.plan([(source, dest)])])
                self.assertEqual(source.read_bytes(), b"incoming")
                self.assertEqual(dest.lstat().st_ino, before.st_ino)

    def test_collision_at_atomic_rename_boundary_preserves_both_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, dest = root / "incoming", root / "catalog"
            source.write_bytes(b"incoming")
            rename = safe_move.rename_noreplace

            def race(origin, target):
                target.write_bytes(b"other writer")
                rename(origin, target)

            with mock.patch.object(safe_move, "rename_noreplace", side_effect=race):
                with self.assertRaises(FileExistsError):
                    intake_media.apply_intake(root, [self.plan([(source, dest)])])
            self.assertEqual(source.read_bytes(), b"incoming")
            self.assertEqual(dest.read_bytes(), b"other writer")

    def test_apply_rechecks_dangling_entry_created_after_planning(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, dest = root / "incoming", root / "catalog"
            source.write_bytes(b"incoming")
            plan = self.plan([(source, dest)])
            self.assertIsNone(intake_media._mapping_collision(plan.mappings))
            dest.symlink_to(root / "missing")
            with self.assertRaises(FileExistsError):
                intake_media.apply_intake(root, [plan])
            self.assertEqual(source.read_bytes(), b"incoming")
            self.assertTrue(dest.is_symlink())

    def test_directory_mapping_and_later_failure_roll_back(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, dest = root / "disc", root / "catalog" / "disc"
            source.mkdir()
            (source / "index.bdmv").write_bytes(b"disc")
            sidecar, collision = root / "sidecar", root / "occupied"
            sidecar.write_bytes(b"sidecar")
            collision.write_bytes(b"incumbent")
            with self.assertRaises(FileExistsError):
                intake_media.apply_intake(root, [self.plan([(source, dest), (sidecar, collision)])])
            self.assertEqual((source / "index.bdmv").read_bytes(), b"disc")
            self.assertFalse(dest.exists())
            self.assertEqual(sidecar.read_bytes(), b"sidecar")
            self.assertEqual(collision.read_bytes(), b"incumbent")
            intake_media.apply_intake(root, [self.plan([(source, dest)])])
            self.assertEqual((dest / "index.bdmv").read_bytes(), b"disc")

    def test_rollback_collision_does_not_clobber_and_other_mappings_recover(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            origins = [root / f"incoming{i}" for i in range(3)]
            targets = [root / f"catalog{i}" for i in range(3)]
            for origin in origins:
                origin.write_bytes(origin.name.encode())
            targets[2].write_bytes(b"incumbent")
            rename = safe_move.rename_noreplace

            def race(origin, target):
                if target == origins[1]:
                    target.symlink_to(root / "missing")
                rename(origin, target)

            with mock.patch.object(safe_move, "rename_noreplace", side_effect=race):
                with self.assertRaisesRegex(RuntimeError, "manual recovery"):
                    intake_media.apply_intake(root, [self.plan(list(zip(origins, targets)))])
            self.assertEqual(origins[0].read_bytes(), b"incoming0")
            self.assertTrue(origins[1].is_symlink())
            self.assertEqual(targets[1].read_bytes(), b"incoming1")
            self.assertEqual(origins[2].read_bytes(), b"incoming2")
            self.assertEqual(targets[2].read_bytes(), b"incumbent")

    def test_privileged_retry_uses_same_no_replace_primitive(self):
        for collision in (False, True):
            with self.subTest(collision=collision), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                source, dest = root / "incoming", root / "catalog"
                source.write_bytes(b"incoming")
                run = subprocess.run

                def sudo_as_current_user(command, **kwargs):
                    self.assertEqual(command[:2], ["sudo", "-n"])
                    if collision:
                        dest.write_bytes(b"other writer")
                    return run(command[2:], **kwargs)

                with (
                    mock.patch.object(safe_move, "rename_noreplace", side_effect=PermissionError),
                    mock.patch.object(safe_move.subprocess, "run", side_effect=sudo_as_current_user),
                ):
                    if collision:
                        with self.assertRaises(RuntimeError):
                            intake_media.apply_intake(root, [self.plan([(source, dest)])])
                    else:
                        intake_media.apply_intake(root, [self.plan([(source, dest)])])
                self.assertEqual(dest.read_bytes(), b"other writer" if collision else b"incoming")
                self.assertEqual(source.exists(), collision)

    def test_cross_device_moves_fail_without_copying_or_deleting(self):
        if not os.access("/dev/shm", os.W_OK):
            self.skipTest("second temporary filesystem unavailable")
        with tempfile.TemporaryDirectory() as temporary, tempfile.TemporaryDirectory(dir="/dev/shm") as other:
            root, other_root = Path(temporary), Path(other)
            if root.stat().st_dev == other_root.stat().st_dev:
                self.skipTest("temporary directories share a filesystem")
            source, dest = root / "incoming", other_root / "catalog"
            source.write_bytes(b"incoming")
            with self.assertRaises(OSError) as raised:
                intake_media.apply_intake(root, [self.plan([(source, dest)])])
            self.assertEqual(raised.exception.errno, errno.EXDEV)
            self.assertEqual(source.read_bytes(), b"incoming")
            self.assertFalse(dest.exists())

    def test_unsupported_platform_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, dest = root / "incoming", root / "catalog"
            source.write_bytes(b"incoming")
            with mock.patch.object(safe_move.sys, "platform", "unsupported"):
                with self.assertRaises(OSError) as raised:
                    intake_media.apply_intake(root, [self.plan([(source, dest)])])
            self.assertEqual(raised.exception.errno, errno.ENOTSUP)
            self.assertEqual(source.read_bytes(), b"incoming")
            self.assertFalse(dest.exists())


if __name__ == "__main__":
    unittest.main()
