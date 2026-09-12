# ADR 0003: Confine media demuxers to admitted descriptors

- Status: accepted
- Date: 2026-09-11
- Deciders: rustyDLNA maintainers

## Context

Opening a media file through the configured-root boundary does not authorize
resources referenced inside that file. Demuxers can discover secondary files or
network URLs while opening headers, before stream discovery or final admission.
Allowing the `file` protocol would still permit secondary local-file access.

## Decision

Media probing uses custom seekable AVIO on the admitted descriptor, rejects
nested opens, and accepts an explicit set of demuxers. Unknown containers are
unsupported. FFmpeg and FFprobe receive independently positioned inherited
descriptors through their seekable `fd` protocol, with other input protocols
disabled. The same boundary applies to artwork, thumbnails and generated
transcode intermediates.

## Consequences

- Top-level root confinement also governs the bytes available to demuxers.
  Non-UTF-8 paths and surviving inode aliases retain their existing behavior.
- External media helpers require FFmpeg/FFprobe 6 or newer. The isolated
  Compose runner uses a toolchain image whose distribution provides that
  capability; production uses FFmpeg 8.
- Manifest demuxers and image sequences are excluded from single-file media.
  Rooted library playlists retain their separate parser.
- This boundary limits demuxer I/O. It is not an operating-system sandbox for
  arbitrary native decoder code.

The current contract is documented in
[Protocol contract](../PROTOCOL_CONTRACT.md#demuxer-input-confinement).
