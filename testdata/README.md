# Test fixtures

The tiny audio, video, and image files under `library/` are original,
procedurally generated test material. They are dedicated to the public domain
under CC0-1.0; no third-party recordings or artwork are embedded.

`scripts/generate-fixtures.sh` creates the binary fixtures with FFmpeg's
synthetic `sine`, `testsrc`, and `color` sources. The checked-in EXIF injector
adds fixed camera/date/orientation/rating fields without depending on an EXIF
editor. The text NFO, subtitle, probe, and playlist files are hand-authored
project fixtures under the repository's GPL-2.0-only license. `SHA256SUMS`
locks the exact checked-in bytes; generator output can legitimately change
when FFmpeg changes, so checksum updates must be reviewed together with scan,
Browse, and GET assertions.

Files containing one byte or text with a media extension live only in the
explicit rejection/filter directories (`@eaDir`, `sample`, `exclude_me`, and
`unfinished.mkv.part`). Successful scanner cases always use valid containers.
