# Third-party runtime notices

The release SBOM is the authoritative machine-readable inventory of Rust and
system components. The production container additionally distributes FFmpeg
from Ubuntu and the pinned `dovi_tool` binary described here.

## nginx 1.31.3 (optional browser gateway)

The browser-only gateway uses the digest-pinned official nginx Alpine image.
nginx is distributed under the BSD-2-Clause license. The complete copyright
and license notice remains in the image at:

```text
/usr/share/licenses/nginx/COPYRIGHT
```

Source and licensing information are available from
`https://nginx.org/en/download.html` and `https://nginx.org/LICENSE`.

## FFmpeg (Ubuntu resolute package)

FFmpeg is free software whose effective license depends on the enabled build
options. Ubuntu's `ffmpeg` package includes GPL-covered components and its
package copyright file describes the exact build's licenses and authors. In
the image, read:

```text
/usr/share/doc/ffmpeg/copyright
```

Corresponding Ubuntu source, including distribution patches and copyright metadata,
is identified at [Ubuntu's FFmpeg source page](https://packages.ubuntu.com/source/resolute/ffmpeg).
The precise binary package version appears in the image SBOM and in:

```sh
dpkg-query -W ffmpeg
```

FFmpeg source and upstream license information are also available from
`https://ffmpeg.org/download.html` and `https://ffmpeg.org/legal.html`.

## dovi_tool 2.3.3 and dolby_vision 3.4.0

Source: `https://github.com/quietvoid/dovi_tool/tree/2.3.3`

The server links the `dolby_vision` Rust library from this release for bounded
per-sample Profile-7-to-8.1 metadata conversion. Its version and transitive Rust
dependencies are pinned in `Cargo.lock` and included in the release SBOM.
The library and command-line tool share the following license notice.

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
