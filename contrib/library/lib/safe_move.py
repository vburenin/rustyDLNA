"""Atomic, same-filesystem moves that never replace a destination entry."""

from __future__ import annotations

import ctypes
import errno
import os
from pathlib import Path
import subprocess
import sys


def rename_noreplace(source: Path, destination: Path) -> None:
    """Fail closed when the host/filesystem cannot provide no-replace rename."""
    libc = ctypes.CDLL(None, use_errno=True)
    if sys.platform.startswith("linux") and hasattr(libc, "renameat2"):
        rename = libc.renameat2
        rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
        rename.restype = ctypes.c_int
        result = rename(-100, os.fsencode(source), -100, os.fsencode(destination), 1)
    elif sys.platform == "darwin" and hasattr(libc, "renamex_np"):
        rename = libc.renamex_np
        rename.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
        rename.restype = ctypes.c_int
        result = rename(os.fsencode(source), os.fsencode(destination), 4)
    else:
        raise OSError(errno.ENOTSUP, "atomic no-replace rename is unavailable", str(destination))
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(destination))


def move_without_overwrite(source: Path, destination: Path, *, allow_sudo: bool = True) -> None:
    """Move files, symlinks or directories; reject collisions and cross-device moves.

    The privileged retry executes the very same primitive, without a copying or
    check-then-rename fallback. Each call is atomic; a multi-file plan is not.
    """
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
        rename_noreplace(source, destination)
    except PermissionError:
        if not allow_sudo:
            raise
        completed = subprocess.run(
            ["sudo", "-n", sys.executable, str(Path(__file__).resolve()),
             str(source.absolute()), str(destination.absolute())],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=60,
        )
        if completed.returncode != 0:
            raise RuntimeError(f"could not move without overwrite: {source} -> {destination}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: safe_move.py SOURCE DESTINATION")
    move_without_overwrite(Path(sys.argv[1]), Path(sys.argv[2]), allow_sudo=False)
