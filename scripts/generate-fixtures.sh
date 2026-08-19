#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
FFMPEG=${FFMPEG:-ffmpeg}
RUSTC=${RUSTC:-rustc}
TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-fixtures.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

command -v "$FFMPEG" >/dev/null
command -v "$RUSTC" >/dev/null
mkdir -p "$ROOT/testdata/library/music" "$ROOT/testdata/library/video" "$ROOT/testdata/library/pictures"

COMMON='-nostdin -hide_banner -loglevel error -y -fflags +bitexact'

# shellcheck disable=SC2086 # COMMON is an intentional fixed argv fragment.
"$FFMPEG" $COMMON -f lavfi -i 'sine=frequency=440:sample_rate=44100:duration=0.25' \
    -map_metadata -1 -flags:a +bitexact -c:a flac \
    -metadata title='Fixture Track' -metadata artist='Fixture Artist' \
    -metadata album_artist='Fixture Album Artist' -metadata album='Fixture Album' \
    -metadata genre='Fixture Genre' -metadata composer='Fixture Composer' \
    -metadata track='2/9' -metadata disc='1/2' -metadata date='2024-03-15' \
    "$ROOT/testdata/library/music/song.flac"

# shellcheck disable=SC2086
"$FFMPEG" $COMMON -f lavfi -i 'sine=frequency=660:sample_rate=44100:duration=0.25' \
    -map_metadata -1 -flags:a +bitexact -c:a libmp3lame -b:a 48k -write_xing 0 \
    -id3v2_version 3 -metadata title='Fixture Track' -metadata artist='Fixture Artist' \
    -metadata album_artist='Fixture Album Artist' -metadata album='Fixture Album' \
    -metadata genre='Fixture Genre' -metadata composer='Fixture Composer' \
    -metadata track='2/9' -metadata disc='1/2' -metadata date='2024-03-15' \
    "$ROOT/testdata/library/music/song.mp3"

# shellcheck disable=SC2086
"$FFMPEG" $COMMON -f lavfi -i 'testsrc=size=32x24:rate=5:duration=0.4' \
    -f lavfi -i 'sine=frequency=330:sample_rate=48000:duration=0.4' \
    -map_metadata -1 -shortest -flags:v +bitexact -flags:a +bitexact \
    -c:v libx264 -preset ultrafast -pix_fmt yuv420p -x264-params 'threads=1:lookahead_threads=1' \
    -c:a aac -b:a 32k -metadata title='Fixture Video' \
    "$ROOT/testdata/library/video/movie.mkv"

# shellcheck disable=SC2086
"$FFMPEG" $COMMON -f lavfi -i 'testsrc=size=32x24:rate=5:duration=0.4' \
    -f lavfi -i 'sine=frequency=550:sample_rate=48000:duration=0.4' \
    -map_metadata -1 -shortest -flags:v +bitexact -flags:a +bitexact \
    -c:v libx264 -preset ultrafast -pix_fmt yuv420p -x264-params 'threads=1:lookahead_threads=1' \
    -c:a aac -b:a 32k -movflags +faststart -metadata title='Tagged MP4 Fixture' \
    "$ROOT/testdata/library/video/tagged.mp4"

cp "$ROOT/testdata/library/video/movie.mkv" "$ROOT/testdata/library/video/dvp7.mkv"
cp "$ROOT/testdata/library/video/movie.mkv" \
    "$ROOT/testdata/library/video/Movie.2024.2160p.UHD.BDRemux.HDR.DV.HEVC.mkv"

# shellcheck disable=SC2086
"$FFMPEG" $COMMON -f lavfi -i 'color=c=0x315c88:size=16x12:duration=0.04' \
    -map_metadata -1 -frames:v 1 -flags:v +bitexact "$TMP/base.jpg"
"$RUSTC" --edition=2021 "$ROOT/scripts/fixture-exif.rs" -o "$TMP/fixture-exif"
"$TMP/fixture-exif" "$TMP/base.jpg" "$ROOT/testdata/library/pictures/shot.jpg"
cp "$ROOT/testdata/library/pictures/shot.jpg" "$ROOT/testdata/library/video/movie-poster.jpg"

echo 'Generated deterministic media fixtures. Refresh testdata/SHA256SUMS with sha256sum.'
