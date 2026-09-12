#!/usr/bin/env python3
"""Download the Dockerfile's package graphs using fresh authenticated indexes.

This is an availability probe, not a server build or a historical-rebuild proof.
It reuses immutable base layers, but no RUN layer, apt index, or package cache.
"""

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys
import tempfile


ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("runtime_evidence", ROOT / "scripts/runtime-evidence.py")
EVIDENCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EVIDENCE)


def probe_dockerfile(source):
    instructions = re.sub(r"\\\n[ \t]*", " ", source).splitlines()
    stages = []
    for line in instructions:
        if line.startswith("FROM "):
            stages.append([])
        if stages and line.strip() and not line.lstrip().startswith("#"):
            stages[-1].append(line)
    if len(stages) < 3 or not stages[0][0].endswith(" AS build") or not stages[1][0].endswith(" AS ubuntu-base"):
        raise ValueError("Dockerfile bootstrap stages changed; review the package probe")
    # Reuse the exact CA bootstrap, HTTPS mirror validation and APT trust/timeouts.
    output = [*stages[0], *stages[1]]
    if any(not line.startswith("FROM ") for line in stages[0]):
        raise ValueError("toolchain bootstrap now executes steps; review the package probe")
    if any("apt-get install" in line for line in stages[1]):
        raise ValueError("bootstrap now installs packages; review the package probe")
    probes = []
    for stage in stages[2:]:
        for instruction in stage:
            if "apt-get install" not in instruction:
                continue
            matched = re.fullmatch(r"RUN apt-get update && apt-get install -y --no-install-recommends\s+(.+?)\s+&&.+", instruction)
            if not matched:
                raise ValueError("unrecognized package install instruction; refusing partial evidence")
            packages = matched[1]
            name = "package-probe-" + str(len(probes))
            probes.append(name)
            output.append("FROM ubuntu-base AS " + name)
            for variable in sorted(set(re.findall(r"\$([A-Z_]+)", packages))):
                declarations = [line for line in stage if line.startswith("ARG " + variable + "=")]
                if len(declarations) != 1:
                    raise ValueError("missing exact package argument: " + variable)
                output.extend(declarations)
            output.extend([
                "ENV DEBIAN_FRONTEND=noninteractive",
                "RUN apt-get update && apt-get install --download-only -y --no-install-recommends " + packages
                + " && for archive in /var/cache/apt/archives/*.deb; do test -f \"$archive\" || exit 1; "
                + "dpkg-deb --show --showformat='${Package}\\t${Version}\\t${Architecture}\\t${Source}\\n' \"$archive\"; "
                + "sha256sum \"$archive\"; done > /package-evidence.txt",
            ])
    if not probes:
        raise ValueError("no package graphs found")
    output.append("FROM scratch")
    for name in probes:
        output.append(f"COPY --from={name} /package-evidence.txt /{name}.txt")
    return "\n".join(output) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="new directory for logs, package versions and hashes")
    parser.add_argument("--print-dockerfile", action="store_true", help="review the generated probe without running Docker")
    parser.add_argument("--platform", choices=("linux/amd64", "linux/arm64"), default="linux/amd64")
    parser.add_argument("--archive-host", help="Dockerfile's validated Ubuntu mirror override; trust settings stay intact")
    parser.add_argument("--timeout", type=int, default=900, help="whole probe deadline in seconds (30–3600)")
    args = parser.parse_args()
    if not 30 <= args.timeout <= 3600:
        parser.error("--timeout must be between 30 and 3600 seconds")
    source = (ROOT / "Dockerfile").read_text()
    try:
        generated = probe_dockerfile(source)
    except ValueError as error:
        parser.error(str(error))
    if args.print_dockerfile:
        print(generated, end="")
        return 0
    if not args.output:
        parser.error("--output is required unless --print-dockerfile is selected")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    (output / "Dockerfile").write_text(generated)
    captured_at = datetime.now(timezone.utc).isoformat()
    docker_version = EVIDENCE.capture(["docker", "version", "--format", "{{json .}}"])
    builder = EVIDENCE.capture(["docker", "buildx", "ls"])
    with tempfile.TemporaryDirectory(prefix="rustydlna-package-probe-") as directory:
        argv = ["docker", "buildx", "build", "--pull", "--no-cache", "--progress=plain",
                "--platform", args.platform, "--file", str(output / "Dockerfile"),
                "--output", "type=local,dest=" + str(output / "packages")]
        if args.archive_host:
            argv.extend(["--build-arg", "UBUNTU_ARCHIVE_HOST=" + args.archive_host])
        argv.append(directory)
        result = EVIDENCE.capture(argv, timeout=args.timeout, limit=4 * 1024 * 1024)
    report = {
        "schema_version": 1, "scope": "fresh package availability; not a complete clean or historical build",
        "captured_at": captured_at, "docker_version": docker_version, "builder": builder,
        "dockerfile_sha256": hashlib.sha256(source.encode()).hexdigest(),
        "platform": args.platform, "archive_host": args.archive_host or "Dockerfile default",
        "result": result,
    }
    (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    (output / "build.log").write_text(result.get("output", result.get("error", "")))
    print(f"package availability probe: {result['status']}; evidence: {output}")
    return 0 if result["status"] == "ok" else 1


if __name__ == "__main__":
    sys.exit(main())
