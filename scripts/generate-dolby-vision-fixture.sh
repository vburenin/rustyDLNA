#!/usr/bin/env bash
# Rebuild the checked-in Profile 7 fixture from checksum-pinned MIT assets.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 OUTPUT_MKV" >&2
    exit 2
fi

OUTPUT=$1
FFMPEG=${FFMPEG:-ffmpeg}
FFPROBE=${FFPROBE:-ffprobe}
DOVI_TOOL=${DOVI_TOOL:-dovi_tool}
MKVMERGE=${MKVMERGE:-mkvmerge}
for tool in curl sha256sum "$FFMPEG" "$FFPROBE" "$DOVI_TOOL" "$MKVMERGE"; do
    command -v "$tool" >/dev/null || {
        echo "generate-dolby-vision-fixture: missing $tool" >&2
        exit 2
    }
done

readonly UPSTREAM_COMMIT=38adec045bf183c24df38149836c920398072281
readonly UPSTREAM_BASE="https://raw.githubusercontent.com/quietvoid/dovi_tool/$UPSTREAM_COMMIT/assets/hevc_tests"
readonly BL_SHA256=3e657fb6ed7378cb368b61dbbf7c291868086e1448380eebc77ed1f2bc3f53ea
readonly EL_SHA256=23cd8db3dc13cbf8e308bd13f84e11d177b94338cb847c089ede9fde86afe7dc
readonly P7_SHA256=74e417ec0ff36adab537b4f11400e20b78241fdf224104a0c37bdcc143e0a653

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-dvp7.XXXXXX")
trap 'rm -rf -- "$TMP_DIR"' EXIT INT TERM
mkdir -p "$(dirname "$OUTPUT")"

curl --fail --location --silent --show-error \
    "$UPSTREAM_BASE/regular_bl_start_code_4.hevc" -o "$TMP_DIR/bl.hevc"
curl --fail --location --silent --show-error \
    "$UPSTREAM_BASE/regular.hevc" -o "$TMP_DIR/el.hevc"
printf '%s  %s\n%s  %s\n' \
    "$BL_SHA256" "$TMP_DIR/bl.hevc" \
    "$EL_SHA256" "$TMP_DIR/el.hevc" | sha256sum --check --status

# The upstream EL contains RPU metadata. Mode 1 rewrites it to Profile 7 MEL
# while mux interleaves the BL, EL, and RPU into one genuine dual-layer stream.
"$DOVI_TOOL" -m 1 mux \
    --bl "$TMP_DIR/bl.hevc" \
    --el "$TMP_DIR/el.hevc" \
    --output "$TMP_DIR/dvp7.hevc" >/dev/null
printf '%s  %s\n' "$P7_SHA256" "$TMP_DIR/dvp7.hevc" \
    | sha256sum --check --status

# Keep the historical fixture's TrueHD characteristic, but make it real.
"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
    -f lavfi -i 'anullsrc=r=48000:cl=5.1:d=0.4' \
    -map_metadata -1 -strict -2 -c:a truehd -fflags +bitexact -flags:a +bitexact \
    "$TMP_DIR/truehd.mka"

"$MKVMERGE" --deterministic 42 --disable-track-statistics-tags \
    --default-duration 0:24000/1001fps \
    --output "$TMP_DIR/dvp7.mkv" "$TMP_DIR/dvp7.hevc" "$TMP_DIR/truehd.mka" \
    >/dev/null

# Check both the container declaration and the RPU carried in the bitstream.
"$FFPROBE" -v error -show_streams \
    -show_entries stream=index,codec_name,channels,color_space,color_transfer,color_primaries:stream_side_data \
    -of json "$TMP_DIR/dvp7.mkv" >"$TMP_DIR/probe.json"
grep -Fq '"dv_profile": 7' "$TMP_DIR/probe.json"
grep -Fq '"el_present_flag": 1' "$TMP_DIR/probe.json"
grep -Fq '"dv_bl_signal_compatibility_id": 6' "$TMP_DIR/probe.json"
grep -Fq '"codec_name": "truehd"' "$TMP_DIR/probe.json"
grep -Fq '"channels": 6' "$TMP_DIR/probe.json"
grep -Fq '"color_transfer": "smpte2084"' "$TMP_DIR/probe.json"

# The fixture exercises production TrueHD-to-AC-3 remapping, so metadata-only
# probing is insufficient. Reject encoder output that the pinned runtime
# decoder cannot turn into audio frames.
"$FFMPEG" -nostdin -hide_banner -loglevel error \
    -i "$TMP_DIR/dvp7.mkv" -map 0:a:0 -f null -

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
    -i "$TMP_DIR/dvp7.mkv" -map 0:v:0 -c:v copy -bsf:v hevc_mp4toannexb \
    -an -f hevc "$TMP_DIR/extracted.hevc"
"$DOVI_TOOL" extract-rpu -i "$TMP_DIR/extracted.hevc" \
    -o "$TMP_DIR/rpu.bin" >/dev/null
"$DOVI_TOOL" info -i "$TMP_DIR/rpu.bin" -f 0 >"$TMP_DIR/rpu.json"
grep -Fq '"dovi_profile": 7' "$TMP_DIR/rpu.json"
grep -Fq '"disable_residual_flag": false' "$TMP_DIR/rpu.json"

cp "$TMP_DIR/dvp7.mkv" "$OUTPUT"
echo "generated genuine Dolby Vision Profile 7 fixture at $OUTPUT"
