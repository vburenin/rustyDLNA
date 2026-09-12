# Distribution and reproducibility

rustyDLNA is GPL-2.0-only. The official release artifact is the multi-architecture
OCI image. A bare ELF is deliberately not published: the scanner links Ubuntu's
FFmpeg libraries and such a file would not be portable without an exact runtime
dependency contract. The image contains those libraries, Ubuntu's FFmpeg command,
and the pinned MIT-licensed `dovi_tool`. Distributors are responsible for all
corresponding-source and notice obligations; an opaque image alone is insufficient.

## Release contract

For a `v*` tag, GitHub Actions:

1. rejects a tag that does not exactly match the Cargo version/changelog and
   pinned latest-stable Rust toolchain;
2. passes the full quality and dependency-policy gates;
3. builds and runtime-smokes the image on amd64 and arm64;
4. builds the final multi-architecture image once and pushes only a unique
   staging reference;
5. runtime-smokes and scans that exact digest before promotion;
6. emits BuildKit SBOM/provenance plus a GitHub provenance attestation;
7. signs the tested digest keylessly with Sigstore/cosign; and
8. promotes that same digest to version, minor, and immutable SHA tags, then
   creates a draft source-and-image release.

The Docker build accepts `BUILD_VERSION`, `VCS_REF`, `BUILD_DATE`, and
`SOURCE_DATE_EPOCH`; the release workflow derives them from the signed tag
commit. Base images, FFmpeg package version, `dovi_tool` version/checksums,
GitHub Actions, Cargo lockfile, and Rust toolchain are pinned.
The current Rust toolchain is `1.98.1`; the scheduled updater changes this
documentation, Cargo, Docker, Compose, and every CI/release/soak pin in one tested PR.
The isolated Compose test runner uses the same digest-pinned toolchain image,
checks the exact compiler build, and runs Cargo with `--locked`; it must not be
used as a floating-toolchain compatibility test.
The toolchain image uses Debian trixie so the Compose runner's packaged FFmpeg
supports seekable descriptor inputs. The production application is still built
and run against the pinned Ubuntu FFmpeg 8 libraries.

`python3 scripts/rust-pins.py check` runs in the ordinary quality gate and the
release contract, without a release tag or container publication. It rejects
Docker/Compose version or digest drift, inconsistent workflow/documentation pins,
and a mismatched Compose compiler assertion. The shared `files` inventory also
drives the scheduled updater's Git staging, including `AGENTS.md` and
`docs/INDEX.md`. Recorded benchmark tool versions are historical measurements.
`scripts/set-rust-version.sh X.Y.Z sha256:HEX` resolves the exact compiler build
from the official Rust release manifest, validates the entire prospective change,
then stages all replacements and rollback copies before replacing the pins.
Missing or malformed metadata fails before any file changes; publication failures
restore earlier replacements without overwriting concurrent external edits. The
scheduled version resolver reads the manifest's Rust compiler table, independently
of the Cargo package version.

Dependabot proposes weekly updates for Cargo, npm, Docker, and GitHub Actions.
Both the application and separate `fuzz/` Cargo workspace receive updates and
advisory and license/source-policy audits. The fuzz package declares the project's
GPL-2.0-only license and versioned internal path dependencies. Its local license
exception covers only libfuzzer-sys 0.4.13's additional NCSA license, reviewed
against that crate's declared `(MIT OR Apache-2.0) AND NCSA` expression; the root
license, wildcard, and source restrictions remain unchanged. The ordinary quality
gate and scheduled fuzz jobs resolve the fuzz graph with
`cargo metadata --locked` before invoking cargo-fuzz, which otherwise may silently
rewrite an outdated fuzz lockfile after application dependency changes.
The npm graph contains browser testing/accessibility tools only; the server embeds
its JavaScript source and has no npm runtime installation. The reusable browser
dependency-policy workflow runs on ordinary CI, before release validation, weekly,
and on manual dispatch. It installs `package-lock.json` with lifecycle scripts
disabled and development dependencies included, then audits that locked graph at
all advisory severities. Its retained artifacts include the graph, audit result,
Node/npm versions, and lockfile hashes. This reports development/test exposure
separately from the Rust server and container runtime scans. Python tooling uses
the standard library and has no third-party package graph.

Rust advisory, license/source, unused-dependency, and container vulnerability
checks remain separate gates. A failed audit service or unavailable local audit
tool is an unavailable check, not evidence of a vulnerability or license violation.

All Ubuntu image stages fetch packages over HTTPS. The digest-pinned Rust
image supplies the initial CA bundle until Ubuntu's `ca-certificates` package
is installed; TLS verification and APT signature/hash checks remain enabled.
Package downloads use 20-second connection/data timeouts and two retries,
and an incomplete package-index update fails the build. These are per-download
limits, not a deadline for the entire image build. The `dovi_tool` download
also has connection/transfer timeouts and bounded retries.

If the main archive endpoint is unavailable or returns inconsistent indexes,
an amd64 build can select another Ubuntu archive host without changing package
versions or trust settings, for example:

```sh
docker compose build --build-arg UBUNTU_ARCHIVE_HOST=us.archive.ubuntu.com
```

The default is `archive.ubuntu.com`. The override accepts only a hostname;
the scheme stays HTTPS and the path stays `/ubuntu/`. Security updates still
use `security.ubuntu.com`, and ARM's `ports.ubuntu.com` sources are unchanged
apart from HTTPS. Do not disable signature, hash, or TLS checks to work around
a mirror error.

For local image builds, a matching release archive may be placed in
`.docker-cache/dovi-tool/`. Archive files in that directory are excluded from
Git but included in the Docker build context. The build verifies the pinned
checksum before extraction. When no project-local archive exists, BuildKit
downloads it once into a persistent builder cache and reuses it on later builds;
a clean builder still downloads the pinned release from GitHub.

## Build availability and retained evidence

Pins identify expected inputs; they do not retain those inputs. Ubuntu stages use
live archive indexes, and most transitive system packages resolve from those
indexes at build time. A superseded exact FFmpeg or fixture-tool package can
disappear from the selected mirror even while its base-image digest remains
available. Cargo downloads, OCI layers, and the checksum-verified dovi_tool release
archive also depend on their upstream services unless already retained locally.
The project does not operate an archive snapshot or retention mirror, and does
not promise indefinitely available historical rebuilds, offline clean builds, or
byte-identical images. A successful build at one revision is evidence for that
revision, architecture, and date only.

The source-side availability probe downloads each build, fixture, and runtime APT
graph using the Dockerfile's exact package declarations and HTTPS/CA/APT trust
setup. It uses fresh package indexes/downloads and disables RUN-layer caching;
immutable base layers may already exist locally. No application compilation,
installation on the host, running daemon, or media mount is involved:

```sh
python3 scripts/clean-build-probe.py --print-dockerfile
python3 scripts/clean-build-probe.py --output /tmp/rustydlna-package-probe --platform linux/amd64
```

The output directory must be new. It retains the generated Dockerfile, source
Dockerfile hash, full bounded build log/status, and exact downloaded package
versions, source identities, and SHA-256 hashes. `--archive-host` uses the existing
validated mirror override; `--timeout` sets the whole probe deadline (default
900 seconds). A rejected package version or download is a failed availability
probe. The probe deliberately does not claim to test Cargo/dovi_tool availability,
package installation, compilation, or final runtime behavior. Full clean-builder
validation additionally requires an isolated empty builder, empty Cargo/BuildKit
caches, no project-local dovi_tool archive, a full Docker build, and the existing
container smoke on each required architecture. Merely passing `--no-cache` to
the production Dockerfile does not empty its persistent cache mounts.

For media and performance evidence, capture the tools from the environment that
actually runs the workload:

```sh
python3 scripts/runtime-evidence.py --binary target/debug/rusty-dlna --output /tmp/runtime-environment.json
python3 scripts/runtime-evidence.py --docker-image IMAGE@DIGEST --platform linux/amd64 --output /tmp/image-environment.json
```

The collector records complete FFmpeg/FFprobe build and linked-libav versions,
development-library versions when present, installed Debian/Ubuntu package and
source versions, tool hashes, Rust/Node/npm/Python, and OS/CPU identity. A supplied
trusted local server binary adds its hash and resolved shared libraries. Image
inspection uses an already available image in a temporary read-only, networkless
container and records its image identity; it never pulls or mounts media. Missing
tools, failed commands, bounded-output truncation, and deadlines remain explicit
in the JSON. Preserve this file beside reports from other dedicated workloads;
the playback benchmark and persistent-process soak embed it automatically, and
the browser CI/release jobs retain it beside their traces and phase diagnostics.
Retaining a package inventory or SBOM identifies corresponding source; it does
not itself retain the source bytes or ensure future download availability.

## Corresponding source

Every public release must keep the repository tag and its lockfile available
beside the binary/image for at least as long as the artifacts are offered. The
release notes must link the tag source archive. System-package source is
identified by package version in the image SBOM; Ubuntu source and patches are
distributed through the Ubuntu package archive. Before distributing an image,
confirm that its exact corresponding source remains available and retain it as
needed for the distribution's source obligations; a live mirror URL alone is not
a retention guarantee. `THIRD_PARTY_NOTICES.md` gives the stable
source locations and the complete dovi_tool MIT notice. The image retains
distribution package copyright files under `/usr/share/doc` and copies the project
license/notices to `/usr/share/doc/rusty-dlna`.

The optional browser-gateway image is built from the digest-pinned official
nginx Alpine image. It retains nginx's BSD-2-Clause notice under
`/usr/share/licenses/nginx/COPYRIGHT` and copies rustyDLNA's license and runtime
notices to `/usr/share/doc/rusty-web`.

Anyone redistributing a modified image must publish the corresponding modified
rustyDLNA source and build scripts and must re-evaluate the FFmpeg license for
their chosen codecs. This document is engineering guidance, not legal advice.

## Verification

```sh
docker run --rm --entrypoint sh IMAGE@DIGEST -c \
  'rusty-dlna --version; ffmpeg -version; dovi_tool --version; \
   test -r /usr/share/doc/rusty-dlna/LICENSE; \
   test -r /usr/share/doc/rusty-dlna/THIRD_PARTY_NOTICES.md; \
   test -r /usr/share/doc/ffmpeg/copyright'
cosign verify \
  --certificate-identity-regexp='^https://github.com/.+/.github/workflows/release.yml@refs/tags/v' \
  --certificate-oidc-issuer=https://token.actions.githubusercontent.com IMAGE@DIGEST
```

Before publishing, the release run records a green `scripts/check.sh`, the full
Chromium/Firefox/WebKit/mobile-Chromium browser suite, dependency policy,
per-architecture container smoke, exact-digest smoke, Trivy result, SBOM,
attestation, and signature. A failure before promotion leaves only a uniquely
named staging tag; delete it after investigation. A failure after promotion is
handled by moving deployments back to the previous signed digest, never by
overwriting that digest. Remove or correct mutable version/minor tags in GHCR
and keep the GitHub release as a draft until the incident is resolved. The
SQLite startup path backs up before migrations and supports `database
check`/`database rebuild` for recovery.
