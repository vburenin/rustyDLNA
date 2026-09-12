#!/usr/bin/env python3
"""Capture exact media/runtime identities without installing or changing tools."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import shlex
import shutil
import signal
import subprocess
import time
import uuid


MAX_OUTPUT_BYTES = 512 * 1024
COMMANDS = {
    "ffmpeg": ["ffmpeg", "-version"],
    "ffprobe": ["ffprobe", "-version"],
    "dovi_tool": ["dovi_tool", "--version"],
    "libav_development": ["pkg-config", "--modversion", "libavformat", "libavcodec", "libavutil", "libswscale", "libswresample"],
    # Include the transitive runtime packages and source versions, not just the
    # ffmpeg executable package; development-library versions can differ.
    "packages": ["dpkg-query", "-W", "-f=${binary:Package}\t${Version}\t${Architecture}\t${source:Package}\t${source:Version}\t${db:Status-Abbrev}\n"],
}


def capture(argv, timeout=10, limit=MAX_OUTPUT_BYTES):
    """Bound diagnostics and wall time, terminating the owned process group."""
    started = time.monotonic()
    result = {"command": argv}
    try:
        child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.STDOUT, start_new_session=True)
    except OSError as error:
        return {**result, "status": "unavailable", "error": str(error)}
    output = bytearray()
    status = "ok"
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            while selector.get_map():
                remaining = timeout - (time.monotonic() - started)
                if remaining <= 0:
                    status = "timeout"
                    break
                for key, _ in selector.select(min(remaining, 0.1)):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    retained = min(len(chunk), max(0, limit - len(output)))
                    output.extend(chunk[:retained])
                    if retained < len(chunk):
                        status = "output-limit"
                        break
                if status != "ok":
                    break
        if status == "ok":
            try:
                child.wait(timeout=max(0.001, timeout - (time.monotonic() - started)))
            except subprocess.TimeoutExpired:
                status = "timeout"
    finally:
        # Also remove descendants of a command that exited with a pipe held open.
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=0.5)
        except subprocess.TimeoutExpired:
            pass
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()
        child.stdout.close()
    if status == "ok" and child.returncode:
        status = "failed"
    return {**result, "status": status, "exit_code": child.returncode,
            "elapsed_seconds": round(time.monotonic() - started, 3),
            "output": output.decode("utf-8", errors="replace")}


def file_identity(path):
    try:
        resolved = Path(path).resolve(strict=True)
        with resolved.open("rb") as source:
            digest = hashlib.sha256()
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)
        return {"path": str(resolved), "sha256": digest.hexdigest()}
    except OSError as error:
        return {"status": "unavailable", "error": str(error)}


def collect(binary=None):
    commands = {**COMMANDS, "rust": ["rustc", "--version", "--verbose"],
                "node": ["node", "--version"], "npm": ["npm", "--version"]}
    if binary:
        commands["server"] = [str(Path(binary).resolve()), "--version"]
        commands["server_linked_libraries"] = ["ldd", str(Path(binary).resolve())]
    report = {
        "schema_version": 1,
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "platform": {"system": platform.system(), "release": platform.release(),
                     "machine": platform.machine(), "python": platform.python_version(),
                     "logical_cpus": os.cpu_count(), "load_average": os.getloadavg()},
        "tools": {name: capture(argv) for name, argv in commands.items()},
        "files": {},
    }
    try:
        report["platform"]["os_release"] = Path("/etc/os-release").read_text()
    except OSError as error:
        report["platform"]["os_release"] = {"status": "unavailable", "error": str(error)}
    for name in ("ffmpeg", "ffprobe", "dovi_tool"):
        executable = shutil.which(name)
        report["files"][name] = file_identity(executable) if executable else {"status": "unavailable"}
    if binary:
        report["files"]["server"] = file_identity(binary)
    return report


def collect_container(image, selected_platform=None):
    """No Python or host mounts are needed inside the read-only image."""
    commands = {"os_release": ["cat", "/etc/os-release"], **COMMANDS,
                "server": ["rusty-dlna", "--version"],
                "server_linked_libraries": ["ldd", "/usr/local/bin/rusty-dlna"],
                "tool_hashes": ["sha256sum", "/usr/bin/ffmpeg", "/usr/bin/ffprobe", "/usr/local/bin/dovi_tool", "/usr/local/bin/rusty-dlna"]}
    lines = []
    for name, argv in commands.items():
        lines.extend(["printf '\\n== " + name + " ==\\n'", shlex.join(argv),
                      "printf 'exit_code=%s\\n' \"$?\""])
    name = "rustydlna-runtime-evidence-" + uuid.uuid4().hex
    argv = ["docker", "run", "--rm", "--pull=never", "--network=none", "--read-only",
            "--name", name, "--entrypoint", "sh"]
    if selected_platform:
        argv.extend(["--platform", selected_platform])
    argv.extend([image, "-c", "\n".join(lines)])
    identity_format = ('{"id":{{json .Id}},"repo_digests":{{json .RepoDigests}},'
                       '"architecture":{{json .Architecture}},"os":{{json .Os}}}')
    report = {"image": image, "platform": selected_platform,
              "identity": capture(["docker", "image", "inspect", "--format", identity_format, image])}
    try:
        report["runtime"] = capture(argv, timeout=60)
    finally:
        # Docker containers outlive a killed Docker client. Clean up only this
        # invocation's unique name, including interruption during docker run.
        cleanup = capture(["docker", "rm", "--force", name])
        if cleanup["status"] != "ok" and "No such container" not in cleanup.get("output", ""):
            report["cleanup"] = cleanup
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="JSON evidence path (default: stdout)")
    parser.add_argument("--binary", type=Path, help="trusted local server binary to identify")
    parser.add_argument("--docker-image", help="also inspect an already available image without pulling it")
    parser.add_argument("--platform", choices=("linux/amd64", "linux/arm64"))
    args = parser.parse_args()
    if args.docker_image and args.docker_image.startswith("-"):
        parser.error("--docker-image must be an image reference")
    if args.platform and not args.docker_image:
        parser.error("--platform requires --docker-image")
    report = collect(args.binary)
    if args.docker_image:
        report["container"] = collect_container(args.docker_image, args.platform)
    data = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(data)
    else:
        print(data, end="")


if __name__ == "__main__":
    main()
