# Test fixtures

Except for the Dolby Vision fixture described below, the tiny audio, video,
and image files under `library/` are original, procedurally generated test
material. They are dedicated to the public domain under CC0-1.0; no
third-party recordings or artwork are embedded.

`scripts/generate-fixtures.sh` creates the binary fixtures with FFmpeg's
synthetic `sine`, `testsrc`, and `color` sources. The checked-in EXIF injector
adds fixed camera/date/orientation/rating fields without depending on an EXIF
editor. The text NFO, subtitle, probe, and playlist files are hand-authored
project fixtures under the repository's GPL-2.0-only license. `SHA256SUMS`
locks the exact checked-in bytes; generator output can legitimately change
when FFmpeg changes, so checksum updates must be reviewed together with scan,
Browse, and GET assertions.

`REQUIRED_FILES` is the authoritative library manifest used by the quality
gate. Missing and unexpected fixture paths both fail instead of being created
or silently accepted during a test run.

`library/video/dvp7.mkv` is derived from quietvoid's `dovi_tool` HEVC test
assets at commit `38adec045bf183c24df38149836c920398072281`. The upstream
base and enhancement-layer inputs are checksum-pinned in
`scripts/generate-dolby-vision-fixture.sh`; that script uses `dovi_tool -m 1`
to create a genuine Profile 7 MEL BL+EL+RPU stream and adds procedurally
generated TrueHD audio. The upstream assets and `dovi_tool` are MIT-licensed:

```text
MIT License

Copyright (c) 2026 quietvoid

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Files containing one byte or text with a media extension live only in the
explicit rejection/filter directories (`@eaDir`, `sample`, `exclude_me`, and
`unfinished.mkv.part`). Successful scanner cases always use valid containers.
