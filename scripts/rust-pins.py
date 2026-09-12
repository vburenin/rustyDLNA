#!/usr/bin/env python3
"""Check/update the source-side Rust contract without building or publishing."""

import os
from pathlib import Path
import re
import sys
import tempfile
import urllib.request


FILES = (
    "rust-toolchain.toml", "Cargo.toml", "Dockerfile", "docker-compose.test.yaml",
    ".github/workflows/ci.yml", ".github/workflows/release.yml",
    ".github/workflows/soak.yml", "AGENTS.md", "docs/INDEX.md",
    "docs/DISTRIBUTION.md",
)
VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
IMAGE = rf"rust:({VERSION})-trixie@(sha256:[0-9a-f]{{64}})"
COMPILER = rf"rustc ({VERSION}) \([0-9a-f]{{9}} [0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}\)"
MAX_MANIFEST_BYTES = 2_000_000


def unique(pattern, content, label):
    matches = re.findall(pattern, content, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError(f"{label}: expected exactly one pin")
    return matches[0]


def check(contents, workflow):
    current = unique(rf'^channel = "({VERSION})"$', contents["rust-toolchain.toml"], "toolchain")
    cargo = unique(rf'^rust-version = "({VERSION})"$', contents["Cargo.toml"], "Cargo")
    if cargo != current:
        raise ValueError("Cargo rust-version differs from rust-toolchain.toml")
    docker = unique(rf"^FROM {IMAGE} AS build$", contents["Dockerfile"], "Docker builder")
    compose = unique(rf"^    image: {IMAGE}$", contents["docker-compose.test.yaml"], "Compose image")
    if docker != compose or docker[0] != current:
        raise ValueError("Docker/Compose Rust image version or digest drift")
    compiler = unique(rf'^        test "\$\(rustc --version\)" = "({COMPILER})"$',
                      contents["docker-compose.test.yaml"], "Compose compiler")
    if compiler[1] != current:
        raise ValueError("Compose exact compiler assertion drift")
    for path in FILES[4:]:
        content = contents[path]
        if current not in content:
            raise ValueError(f"{path}: current Rust pin missing")
        pins = re.findall(rf"(?:rustup (?:toolchain install|default) |Rust \*\*|Rust `|Rust |Rust toolchain is `)({VERSION})", content)
        if any(pin != current for pin in pins):
            raise ValueError(f"{path}: inconsistent Rust pin")
    if "python3 scripts/rust-pins.py manifest-version" not in workflow:
        raise ValueError("toolchain updater workflow must resolve the Rust compiler manifest table")
    if "python3 scripts/rust-pins.py files | xargs -d '\\n' git add --" not in workflow:
        raise ValueError("toolchain updater workflow must stage the shared pin inventory")
    return current


def manifest_compiler(data, expected_version=None):
    if len(data) > MAX_MANIFEST_BYTES:
        raise ValueError("Rust manifest exceeds 2 MB")
    block = unique(r"^\[pkg\.rustc\]\n([^\[]*)", data.decode("utf-8"), "Rust manifest rustc table")
    build = unique(r'^version = "([^"\n]+)"$', block, "Rust compiler metadata")
    compiler = f"rustc {build}"
    matched = re.fullmatch(COMPILER, compiler)
    if not matched or (expected_version is not None and matched[1] != expected_version):
        raise ValueError("Rust manifest compiler does not match requested release")
    return compiler


def compiler_metadata(version):
    # Offline tests supply the same published manifest format.
    manifest = os.environ.get("RUSTY_DLNA_RUST_MANIFEST")
    if manifest:
        with open(manifest, "rb") as source:
            data = source.read(MAX_MANIFEST_BYTES + 1)
    else:
        url = f"https://static.rust-lang.org/dist/channel-rust-{version}.toml"
        with urllib.request.urlopen(url, timeout=30) as response:
            data = response.read(MAX_MANIFEST_BYTES + 1)
    return manifest_compiler(data, version)


def publish_updates(root, originals, updated):
    """Stage complete files and rollback replaced pins without overwriting edits."""
    temporary_paths = []
    changes = []
    replaced = []

    def stage(target, data, mode):
        descriptor, name = tempfile.mkstemp(prefix=".rust-pins-", dir=target.parent)
        temporary = Path(name)
        temporary_paths.append(temporary)
        with os.fdopen(descriptor, "wb") as output:
            os.fchmod(output.fileno(), mode)
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        return temporary

    try:
        # Finish every write, including rollback copies, before replacing any
        # repository path. A full disk or failed staging write leaves pins intact.
        for path, content in updated.items():
            data = content.encode("utf-8")
            if data == originals[path]:
                continue
            target = root / path
            if target.read_bytes() != originals[path]:
                raise ValueError(f"{path}: changed during Rust update")
            mode = target.stat().st_mode & 0o777
            replacement = stage(target, data, mode)
            backup = stage(target, originals[path], mode)
            changes.append((path, target, data, replacement, backup))
        for change in changes:
            path, target, _, replacement, _ = change
            if target.read_bytes() != originals[path]:
                raise ValueError(f"{path}: changed during Rust update")
            os.replace(replacement, target)
            replaced.append(change)
    except (OSError, ValueError):
        for path, target, data, _, backup in reversed(replaced):
            try:
                if target.read_bytes() != data:
                    raise ValueError("changed after replacement; preserving external edit")
                os.replace(backup, target)
            except (OSError, ValueError) as error:
                # One failed restore must not prevent restoring other pins.
                print(f"Rust pin rollback: {path}: {error}", file=sys.stderr)
        raise
    finally:
        for temporary in temporary_paths:
            try:
                temporary.unlink(missing_ok=True)
            except OSError as error:
                print(f"Rust pin temporary cleanup: {temporary}: {error}", file=sys.stderr)


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else "check"
    if command == "files" and len(sys.argv) == 2:
        print("\n".join(FILES))
        return
    if command == "manifest-version" and len(sys.argv) == 3:
        with open(sys.argv[2], "rb") as source:
            compiler = manifest_compiler(source.read(MAX_MANIFEST_BYTES + 1))
        print(compiler.split()[1])
        return
    root = Path(os.environ.get("RUSTY_DLNA_UPDATE_ROOT", Path(__file__).resolve().parent.parent))
    originals = {path: (root / path).read_bytes() for path in FILES}
    contents = {path: data.decode("utf-8") for path, data in originals.items()}
    workflow = (root / ".github/workflows/rust-toolchain-update.yml").read_text(encoding="utf-8")
    current = check(contents, workflow)
    if command == "check" and len(sys.argv) == 2:
        print(f"Rust pin contract OK: {current}")
        return
    if command != "update" or len(sys.argv) != 4:
        raise ValueError("usage: rust-pins.py check|files|manifest-version PATH|update X.Y.Z sha256:HEX")
    version, digest = sys.argv[2:]
    if not re.fullmatch(VERSION, version) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        raise ValueError("expected exact X.Y.Z and sha256:<64 lowercase hex digits>")
    if tuple(map(int, version.split("."))) < tuple(map(int, current.split("."))):
        raise ValueError("Rust downgrades are not permitted")
    # Resolve every external datum and validate the prospective change before
    # the first write. Failed metadata/schema/pin checks leave files intact.
    compiler = compiler_metadata(version)
    updated = {path: re.sub(rf"(?<![0-9.]){re.escape(current)}(?![0-9.])", version, content)
               for path, content in contents.items()}
    for path in ("Dockerfile", "docker-compose.test.yaml"):
        updated[path] = re.sub(IMAGE, f"rust:{version}-trixie@{digest}", updated[path])
    updated["docker-compose.test.yaml"] = re.sub(COMPILER, compiler, updated["docker-compose.test.yaml"])
    check(updated, workflow)
    publish_updates(root, originals, updated)
    print(f"updated Rust {current} -> {version} ({digest})")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError) as error:
        print(f"Rust pin contract: {error}", file=sys.stderr)
        sys.exit(1)
