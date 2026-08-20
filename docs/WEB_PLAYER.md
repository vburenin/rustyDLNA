# Embedded web player

The player is a self-contained server module. Its HTML, CSS, and JavaScript
are embedded with the Rust binary at compile time; there is no runtime asset
directory and no separate web build.

```toml
[web]
enable = true
encoder = "libx264" # or "h264_nvenc"

[transcode]
enable = true
max_jobs = 2
cache_max_mb = 51200
```

Open `http://SERVER:8200/`. The operator page remains available at `/status`.
With `web.enable = false`, `/web/*` and `/api/web/*` return 404 and `/` serves
the operator page again.

The default view follows the physical media-folder tree and keeps the selected
folder in the page URL, so browser Back/Forward and copied links work. The All
media, Videos, and Audio tabs retain the flat searchable library view.

## Player controls

The compact top bar shows the current movie title; the server-brand label is
deliberately small. The side panel provides one-click previous/next, 10-second
back, 30-second forward, play/pause, mute, loop, video fill/fit,
picture-in-picture, fullscreen, and common playback-speed buttons. Audio track
selection uses a compact dropdown. Auto, Direct, and Compatible buttons change
the stream choice without reselecting the title. Speed, stream mode, mute,
loop, and fit preferences are stored locally by the browser.

Press `Ctrl+F` to enter or leave video fullscreen. The shortcut is ignored
while typing in a search or other editable field, where the browser's normal
Find shortcut remains available. The overlaid fullscreen button appears only
while the pointer is moving over the video and hides after five idle seconds.

Compatibility playback uses a compact movie-length scrubber whose end comes
from the indexed source duration rather than the few seconds currently buffered
in a growing MP4 segment. The misleading native segment seek bar is hidden in
this mode. Clicking or dragging to any movie time requests a new segment
beginning at that timestamp.

Previous and Next operate on the playable items currently loaded from the
selected folder or library page.

## Playback behavior

The library API returns the original media URL and a compatibility URL. The
server first applies a conservative container/video/audio codec check, then the
browser checks its own codec support. Auto starts with the original only when
both checks say it is suitable. This avoids trusting a generic MP4 "maybe" for
files such as HEVC Main 10 HDR video with AC-3 audio. If direct playback still
fails, Auto retries once through the compatibility URL. Direct remains
available as an explicit one-click override.

When transcoding is enabled, compatibility output is:

- video: fragmented MP4 with H.264 8-bit 4:2:0 video and AAC audio;
- fallback video is encoded even when its source codec is H.264, because the
  stored scan summary cannot prove that its profile, level, and pixel format
  are browser-safe;
- audio: AAC in an MP4 container.

The existing bounded transcode job gate, helper queue, disk cache, runtime
deadline, cache-size limit, cache-age policy, cancellation, and finished-file
verification also apply to browser jobs. Concurrent requests for the same
source and plan share one job. Closing the page (or switching streams) cancels
an unfinished browser producer as soon as its last HTTP reader disconnects;
DLNA jobs retain their independently configured cache policy. Finished output
supports byte-range seeking. Seeking beyond the currently generated fragment
starts a new compatibility segment at the requested media time.

For NVIDIA acceleration, set `web.encoder = "h264_nvenc"` and expose the GPU
to the container. With NVIDIA Container Toolkit installed, a local Compose
override can contain:

```yaml
services:
  rusty-dlna:
    gpus: all
    environment:
      NVIDIA_DRIVER_CAPABILITIES: compute,video,utility,graphics
```

For H.264 and HEVC sources, the player uses NVDEC and NVENC; AAC audio encoding
remains on the CPU. Dolby Vision and HDR10 compatibility playback is converted
to BT.709 SDR with FFmpeg's Vulkan-backed `libplacebo` filter. This applies the
Dolby Vision RPU before tone mapping, which is essential for Profile 5 sources
that otherwise appear purple after their DV metadata is merely discarded.
The container therefore needs the NVIDIA `graphics` capability in addition to
`video`, `compute`, and `utility`.

Other input video codecs retain software decode with NVENC output. If the
hardware command fails before the first playable fragment is published, the
same job retries with software decode and `libx264`. `rusty-dlna --config ...
--check` exercises the selected GPU encoder rather than only checking whether
FFmpeg lists it. The official container uses Ubuntu 26.04's FFmpeg 8.0.1 so the
scanner library ABI and transcoder command come from the same FFmpeg release.

With `transcode.enable = false`, the player still serves browser-native files.
For other files, the compatibility URL aliases the original file and the UI
warns that playback may not be supported.

## Routes

| Route | Purpose |
|---|---|
| `/` | Player, or status page when the module is disabled |
| `/web/app.css` | Embedded stylesheet |
| `/web/app.js` | Embedded player application |
| `/api/web/library` | Paginated/searchable audio and video JSON |
| `/api/web/item/{id}` | On-demand audio-track metadata for one item |
| `/web/media/{id}.mp4` | Browser compatibility stream |
| `/status` | Operator status page |

The library endpoint accepts `kind=all|video|audio`, `q`, `offset`, and
`limit`. `view=folders&folder=OBJECT_ID` returns the direct physical child
folders/media plus safe breadcrumbs; omitting `folder` starts at the media
root. Folder object IDs are opaque and absolute filesystem paths are never
returned. The limit is capped at 200 so large libraries do not create an
unbounded response.

The web player is intended for a trusted LAN, like the DLNA server itself. It
does not add accounts, TLS termination, or Internet-facing access control.
Put an authenticated reverse proxy in front if access is extended beyond the
trusted network.
