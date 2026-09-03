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
The current Rust toolchain is `1.97.1`; the scheduled updater changes this
documentation, Cargo, Docker, and every CI/release/soak pin in one tested PR.
The isolated Compose test runner uses the same digest-pinned toolchain image,
checks the exact compiler build, and runs Cargo with `--locked`; it must not be
used as a floating-toolchain compatibility test.

For local image builds, a matching release archive may be placed in
`.docker-cache/dovi-tool/`. Archive files in that directory are excluded from
Git but included in the Docker build context. The build verifies the pinned
checksum before extraction. When no project-local archive exists, BuildKit
downloads it once into a persistent builder cache and reuses it on later builds;
a clean builder still downloads the pinned release from GitHub.

## Corresponding source

Every public release must keep the repository tag and its lockfile available
beside the binary/image for at least as long as the artifacts are offered. The
release notes must link the tag source archive. System-package source is
identified by package version in the image SBOM; Ubuntu source and patches are
available from the Ubuntu package archive. `THIRD_PARTY_NOTICES.md` gives the stable
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
