#!/usr/bin/env bash
# Generate small, legally clean advanced-media fixtures from lavfi sources.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 OUTPUT_DIRECTORY" >&2
    exit 2
fi

OUTPUT=$1
FFMPEG=${FFMPEG:-ffmpeg}
FFPROBE=${FFPROBE:-ffprobe}
for tool in "$FFMPEG" "$FFPROBE" python3; do
    command -v "$tool" >/dev/null || {
        echo "generate-advanced-fixtures: missing $tool" >&2
        exit 2
    }
done

mkdir -p "$OUTPUT"
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-advanced-fixtures.XXXXXX")
trap 'rm -rf -- "$TMP_DIR"' EXIT INT TERM

COMMON=(-nostdin -hide_banner -loglevel error -y)
X264=(-c:v libx264 -preset ultrafast -pix_fmt yuv420p -x264-params threads=1:lookahead_threads=1)

# A real six-channel TrueHD stream, used to exercise the audio-ac3 path.
"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i 'testsrc2=size=64x36:rate=5:duration=0.4' \
    -f lavfi -i 'anullsrc=r=48000:cl=5.1:d=0.4' \
    -map 0:v:0 -map 1:a:0 -map_metadata -1 -shortest \
    "${X264[@]}" -strict -2 -c:a truehd \
    "$OUTPUT/truehd.mkv"

# Genuine PQ/BT.2020 HEVC with mastering-display and MaxCLL/MaxFALL SEI.
"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i 'testsrc2=size=64x36:rate=5:duration=0.4' \
    -map_metadata -1 -frames:v 2 -an -c:v libx265 -preset ultrafast -pix_fmt yuv420p10le \
    -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
    -x265-params 'pools=1:frame-threads=1:wpp=0:log-level=error:keyint=1:min-keyint=1:scenecut=0:repeat-headers=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400' \
    "$OUTPUT/hdr10-mastering.mkv"

# Audio precedes video globally, two different audio codecs follow distinct
# ordinals, and a subtitle follows both. This catches global-index/audio-index
# confusion in scanners and remap command builders.
printf '1\n00:00:00,000 --> 00:00:00,300\nfixture subtitle\n' >"$TMP_DIR/fixture.srt"
"$FFMPEG" "${COMMON[@]}" \
    -f lavfi -i 'testsrc2=size=64x36:rate=5:duration=0.4' \
    -f lavfi -i 'anullsrc=r=48000:cl=5.1:d=0.4' \
    -f lavfi -i 'sine=frequency=440:sample_rate=48000:duration=0.4' \
    -f srt -i "$TMP_DIR/fixture.srt" \
    -map 1:a:0 -map 0:v:0 -map 2:a:0 -map 3:s:0 -map_metadata -1 -shortest \
    "${X264[@]}" -strict -2 -c:a:0 truehd -c:a:1 ac3 -b:a:1 192k -c:s srt \
    "$OUTPUT/unusual-layout.mkv"

# Embed one tag above the scanner's 64 KiB metadata budget. Python avoids
# constructing the large argument in the shell and also creates deterministic
# malformed/truncated inputs without depending on /dev/urandom.
python3 - "$FFMPEG" "$OUTPUT" <<'PY'
import pathlib
import subprocess
import sys

ffmpeg, output_arg = sys.argv[1:]
output = pathlib.Path(output_arg)
comment = "x" * (80 * 1024)
subprocess.run(
    [
        ffmpeg,
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-i",
        str(output / "truehd.mkv"),
        "-map",
        "0",
        "-c",
        "copy",
        "-map_metadata",
        "-1",
        "-metadata",
        "title=bounded-title",
        "-metadata",
        f"comment={comment}",
        str(output / "oversized-metadata.mkv"),
    ],
    check=True,
)
source = (output / "truehd.mkv").read_bytes()
(output / "truncated.mkv").write_bytes(source[:32])
(output / "corrupt.mkv").write_bytes(bytes([0x1A, 0x45, 0xDF, 0xA3]) + b"corrupt" * 512)
PY

# Fail generation itself if the host encoders silently omitted the advanced
# properties that make these fixtures useful.
"$FFPROBE" -v error -select_streams a:0 -show_entries stream=codec_name \
    -of default=noprint_wrappers=1:nokey=1 "$OUTPUT/truehd.mkv" | grep -Fx truehd >/dev/null
"$FFPROBE" -v error -read_intervals '%+#1' -select_streams v:0 -show_frames \
    -show_entries frame=color_space,color_primaries,color_transfer:frame_side_data \
    -of json "$OUTPUT/hdr10-mastering.mkv" >"$TMP_DIR/hdr10.json"
grep -Fq 'Mastering display metadata' "$TMP_DIR/hdr10.json"
grep -Fq 'Content light level metadata' "$TMP_DIR/hdr10.json"
grep -Fq '"max_content": 1000' "$TMP_DIR/hdr10.json"
grep -Fq '"max_average": 400' "$TMP_DIR/hdr10.json"

echo "generated advanced media fixtures in $OUTPUT"
