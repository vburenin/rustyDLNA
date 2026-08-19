# Third-party runtime notices

The release SBOM is the authoritative machine-readable inventory of Rust and
system components. The production container additionally distributes FFmpeg
from Debian and the pinned `dovi_tool` binary described here.

## FFmpeg (Debian bookworm package)

FFmpeg is free software whose effective license depends on the enabled build
options. Debian's `ffmpeg` package includes GPL-covered components and its
package copyright file describes the exact build's licenses and authors. In
the image, read:

```text
/usr/share/doc/ffmpeg/copyright
```

Corresponding Debian source, including Debian patches and copyright metadata,
is available from `https://sources.debian.org/src/ffmpeg/`. The precise binary
package version appears in the image SBOM and in:

```sh
dpkg-query -W ffmpeg
```

FFmpeg source and upstream license information are also available from
`https://ffmpeg.org/download.html` and `https://ffmpeg.org/legal.html`.

## dovi_tool 2.3.3

Source: `https://github.com/quietvoid/dovi_tool/tree/2.3.3`

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
