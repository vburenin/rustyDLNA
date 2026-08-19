# Distribution and reproducibility

rustyDLNA is GPL-2.0-only. The release binary links Rust dependencies and
FFmpeg libraries, while the production image also contains Debian's FFmpeg
command-line package and the pinned MIT-licensed `dovi_tool`. Distributors are
responsible for satisfying all corresponding-source and notice obligations;
publishing only an opaque container image is not sufficient.

## Release contract

For a `v*` tag, GitHub Actions:

1. builds with the exact `rust-toolchain.toml` toolchain and `Cargo.lock`;
2. creates a versioned binary archive containing GPL and third-party notices;
3. emits CycloneDX SBOMs and SHA-256 checksums;
4. attests the binary provenance;
5. builds the digest-pinned, snapshot-repository image and pushes immutable
   tag/version/SHA references to GHCR;
6. scans the image for HIGH/CRITICAL known vulnerabilities; and
7. signs the pushed image digest keylessly with Sigstore/cosign.

The Docker build accepts `BUILD_VERSION`, `VCS_REF`, `BUILD_DATE`, and
`SOURCE_DATE_EPOCH`; the release workflow derives them from the signed tag
commit. Base images, Debian snapshot timestamp, `dovi_tool` version, archive
checksums, GitHub Actions, Cargo lockfile, and Rust toolchain are pinned.

## Corresponding source

Every public release must keep the repository tag and its lockfile available
beside the binary/image for at least as long as the artifacts are offered. The
release notes must link the tag source archive. System-package source is
identified by package version in the image SBOM; Debian source and patches are
available from `sources.debian.org`. `THIRD_PARTY_NOTICES.md` gives the stable
source locations and the complete dovi_tool MIT notice. The image retains
Debian package copyright files under `/usr/share/doc` and copies the project
license/notices to `/usr/share/doc/rusty-dlna`.

Anyone redistributing a modified image must publish the corresponding modified
rustyDLNA source and build scripts and must re-evaluate the FFmpeg license for
their chosen codecs. This document is engineering guidance, not legal advice.

## Verification

```sh
docker run --rm --entrypoint sh IMAGE -c \
  'rusty-dlna --version; ffmpeg -version; dovi_tool --version; \
   test -r /usr/share/doc/rusty-dlna/LICENSE; \
   test -r /usr/share/doc/rusty-dlna/THIRD_PARTY_NOTICES.md; \
   test -r /usr/share/doc/ffmpeg/copyright'
cosign verify \
  --certificate-identity-regexp='^https://github.com/.+/.github/workflows/release.yml@refs/tags/v' \
  --certificate-oidc-issuer=https://token.actions.githubusercontent.com IMAGE@DIGEST
```

Before publishing, record a green `scripts/check.sh`, container smoke test,
Trivy result, and SBOM generation in the release run. Rollback means deploying
the previous signed digest; the SQLite startup path backs up before migrations
and supports `database check`/`database rebuild` for recovery.
