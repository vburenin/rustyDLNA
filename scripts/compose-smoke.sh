#!/bin/sh
# Build and exercise the production image without host networking or ports.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
PACKAGE_VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-smoke.XXXXXX")
export RUSTY_DLNA_SMOKE_MEDIA="$TMP/media"
export RUSTY_DLNA_SMOKE_CACHE="$TMP/cache"
export RUSTY_DLNA_SMOKE_CONFIG="$TMP/rusty-dlna.toml"
COMPOSE="docker compose -f $ROOT/docker-compose.smoke.yaml"

cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then
        $COMPOSE logs --no-color rusty-dlna-smoke >&2 || true
    fi
    $COMPOSE exec --user 0 -T rusty-dlna-smoke \
        chmod -R a+rwX /var/cache/rusty-dlna >/dev/null 2>&1 || true
    $COMPOSE down --remove-orphans >/dev/null 2>&1 || true
    rm -rf -- "$TMP"
    exit "$status"
}
trap cleanup EXIT INT TERM

mkdir -p \
    "$RUSTY_DLNA_SMOKE_MEDIA" \
    "$RUSTY_DLNA_SMOKE_CACHE" \
    "$RUSTY_DLNA_SMOKE_CACHE/derived-images" \
    "$RUSTY_DLNA_SMOKE_CACHE/art"
chmod 0755 "$TMP" "$RUSTY_DLNA_SMOKE_MEDIA"
chmod 0777 \
    "$RUSTY_DLNA_SMOKE_CACHE" \
    "$RUSTY_DLNA_SMOKE_CACHE/derived-images" \
    "$RUSTY_DLNA_SMOKE_CACHE/art"
cp "$ROOT/testdata/library/video/dvp7.mkv" "$RUSTY_DLNA_SMOKE_MEDIA/movie.mkv"
cp "$ROOT/testdata/library/video/movie.nfo" "$RUSTY_DLNA_SMOKE_MEDIA/movie.nfo"
cp "$ROOT/testdata/library/video/movie-poster.jpg" "$RUSTY_DLNA_SMOKE_MEDIA/movie-poster.jpg"
printf '%s\n' \
    'friendly_name = "rustyDLNA smoke"' \
    'media_dir = ["V,/storage/video"]' \
    'listen_ip = "0.0.0.0"' \
    'cache_dir = "/var/cache/rusty-dlna"' \
    'rescan_secs = 1' \
    '[transcode]' \
    'enable = true' \
    'max_jobs = 1' \
    '[[remap]]' \
    'name = "smoke-dvp7"' \
    'client = "CrKey"' \
    'hdr = "dv-p7"' \
    'action = "remux-p8"' \
    'encoder = "copy"' \
    'audio_out = "to-ac3"' \
    >"$RUSTY_DLNA_SMOKE_CONFIG"
chmod 0644 "$RUSTY_DLNA_SMOKE_CONFIG"

cd "$ROOT"
if [ -n "${RUSTY_DLNA_SMOKE_IMAGE:-}" ]; then
    docker image inspect "$RUSTY_DLNA_SMOKE_IMAGE" >/dev/null
    $COMPOSE up --no-build --detach --wait
else
    $COMPOSE up --build --detach --wait
fi
$COMPOSE exec -T rusty-dlna-smoke rusty-dlna --version \
    | grep -Fx "rusty-dlna ${RUSTY_DLNA_EXPECT_VERSION:-$PACKAGE_VERSION}"
$COMPOSE exec -T rusty-dlna-smoke ffmpeg -version >/dev/null
$COMPOSE exec -T rusty-dlna-smoke dovi_tool --version >/dev/null
$COMPOSE exec -T rusty-dlna-smoke \
    curl --fail --silent http://127.0.0.1:18200/rootDesc.xml \
    | grep -q '<friendlyName>rustyDLNA smoke</friendlyName>'
$COMPOSE exec -T rusty-dlna-smoke sh -ec '
    health=$(curl --fail --silent http://127.0.0.1:18200/health)
    printf "%s\n" "$health" | grep -q "\"status\": \"healthy\""
    runs_before=$(printf "%s\n" "$health" | sed -n '"'"'s/.*"runs_total": \([0-9][0-9]*\).*/\1/p'"'"')
    test -n "$runs_before"
    polls=0
    while [ "$polls" -lt 20 ]; do
        runs_after=$(curl --fail --silent http://127.0.0.1:18200/health \
            | sed -n '"'"'s/.*"runs_total": \([0-9][0-9]*\).*/\1/p'"'"')
        test "$runs_after" = "$runs_before"
        polls=$((polls + 1))
    done
    status=$(curl --fail --silent http://127.0.0.1:18200/api/status)
    printf "%s\n" "$status" | grep -q "\"listener_state\": \"running\""
    printf "%s\n" "$status" | grep -q "\"browse_latency_ms\""
    printf "%s\n" "$status" | grep -q "\"dropped_events_total\""
    curl --fail --silent http://127.0.0.1:18200/status \
        | grep -Eq "<td>Video</td><td>[1-9]"
'
$COMPOSE exec -T rusty-dlna-smoke sh -ec '
    echo "smoke: SOAP Browse"
    soap="<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"><ObjectID>2\$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"
    browse=$(curl --fail --silent \
      -H "SOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"" \
      -H "Content-Type: text/xml" \
      -H "User-Agent: CrKey/1.54" \
      --data "$soap" http://127.0.0.1:18200/ctl/ContentDir \
    )
    printf "%s" "$browse" | grep -q "NumberReturned"
    media_path=$(printf "%s" "$browse" | grep -oE "/MediaItems/[0-9]+\\.[A-Za-z0-9]+" | head -n 1)
    test -n "$media_path"
    media_id=$(printf "%s" "$media_path" | sed -n "s|/MediaItems/\\([0-9][0-9]*\\)\\..*|\\1|p")
    test -n "$media_id"

    echo "smoke: original media GET"
    curl --fail --silent "http://127.0.0.1:18200$media_path" -o /tmp/original.mkv
    test "$(sha256sum /tmp/original.mkv | cut -d" " -f1)" = "$(sha256sum /storage/video/movie.mkv | cut -d" " -f1)"

    echo "smoke: thumbnail GET"
    curl --fail --silent "http://127.0.0.1:18200/Thumbnails/$media_id.jpg" -o /tmp/thumb.jpg
    test "$(od -An -tx1 -N3 /tmp/thumb.jpg | tr -d " \n")" = ffd8ff

    echo "smoke: resized image GET"
    curl --fail --silent "http://127.0.0.1:18200/Resized/$media_id.jpg?width=64,height=64" -o /tmp/resized.jpg
    test "$(od -An -tx1 -N3 /tmp/resized.jpg | tr -d " \n")" = ffd8ff

    echo "smoke: genuine Profile 7 to Profile 8 remux GET"
    curl --fail --silent --max-time 30 -H "User-Agent: CrKey/1.54" \
      "http://127.0.0.1:18200/Transcode/$media_id.mp4" -o /tmp/remux.mp4
    test -s /tmp/remux.mp4
    test "$(dd if=/tmp/remux.mp4 bs=1 skip=4 count=4 2>/dev/null)" = ftyp
    grep -aq moof /tmp/remux.mp4
    ffprobe -v error -show_streams \
      -show_entries stream=codec_name,profile,pix_fmt,color_space,color_transfer,color_primaries:stream_side_data \
      -of json /tmp/remux.mp4 > /tmp/remux-probe.json
    grep -q "\"codec_name\": \"hevc\"" /tmp/remux-probe.json
    grep -q "\"profile\": \"Main 10\"" /tmp/remux-probe.json
    grep -q "\"pix_fmt\": \"yuv420p10le\"" /tmp/remux-probe.json
    grep -q "\"color_space\": \"bt2020nc\"" /tmp/remux-probe.json
    grep -q "\"color_transfer\": \"smpte2084\"" /tmp/remux-probe.json
    grep -q "\"color_primaries\": \"bt2020\"" /tmp/remux-probe.json
    grep -q "\"dv_profile\": 8" /tmp/remux-probe.json
    grep -q "\"rpu_present_flag\": 1" /tmp/remux-probe.json
    grep -q "\"el_present_flag\": 0" /tmp/remux-probe.json
    grep -q "\"bl_present_flag\": 1" /tmp/remux-probe.json
    grep -q "\"dv_bl_signal_compatibility_id\": 1" /tmp/remux-probe.json
    test "$(ffprobe -v error -select_streams a:0 -show_entries stream=codec_name -of default=noprint_wrappers=1:nokey=1 /tmp/remux.mp4)" = ac3
    ffmpeg -nostdin -v error -i /tmp/remux.mp4 -map 0:v:0 -frames:v 1 -f null -
    ffmpeg -nostdin -v error -y -i /tmp/remux.mp4 -map 0:v:0 -c:v copy \
      -bsf:v hevc_mp4toannexb -an -f hevc /tmp/remux.hevc
    dovi_tool extract-rpu -i /tmp/remux.hevc -o /tmp/remux-rpu.bin >/dev/null
    dovi_tool info -i /tmp/remux-rpu.bin -f 0 > /tmp/remux-rpu.json
    grep -q "\"dovi_profile\": 8" /tmp/remux-rpu.json
    grep -q "\"disable_residual_flag\": true" /tmp/remux-rpu.json
'
$COMPOSE exec --user 0 -T rusty-dlna-smoke \
    mv /usr/local/bin/dovi_tool /usr/local/bin/dovi_tool.disabled
$COMPOSE exec -T rusty-dlna-smoke sh -ec '
    echo "smoke: genuine Profile 7 HDR10 fallback GET"
    soap="<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"><ObjectID>2\$8</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"
    browse=$(curl --fail --silent \
      -H "SOAPAction: \"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"" \
      -H "Content-Type: text/xml" \
      -H "User-Agent: CrKey/1.54" \
      --data "$soap" http://127.0.0.1:18200/ctl/ContentDir)
    media_id=$(printf "%s" "$browse" | grep -oE "/MediaItems/[0-9]+\\.[A-Za-z0-9]+" \
      | head -n 1 | sed -n "s|/MediaItems/\\([0-9][0-9]*\\)\\..*|\\1|p")
    test -n "$media_id"
    curl --fail --silent --max-time 30 -H "User-Agent: CrKey/1.54" \
      "http://127.0.0.1:18200/Transcode/$media_id.mp4" -o /tmp/fallback.mp4
    test -s /tmp/fallback.mp4
    test "$(dd if=/tmp/fallback.mp4 bs=1 skip=4 count=4 2>/dev/null)" = ftyp
    grep -aq moof /tmp/fallback.mp4
    ffprobe -v error -show_streams \
      -show_entries stream=codec_name,profile,pix_fmt,color_space,color_transfer,color_primaries:stream_side_data \
      -of json /tmp/fallback.mp4 > /tmp/fallback-probe.json
    grep -q "\"codec_name\": \"h264\"" /tmp/fallback-probe.json
    grep -q "\"profile\": \"High 10\"" /tmp/fallback-probe.json
    grep -q "\"pix_fmt\": \"yuv420p10le\"" /tmp/fallback-probe.json
    grep -q "\"color_space\": \"bt2020nc\"" /tmp/fallback-probe.json
    grep -q "\"color_transfer\": \"smpte2084\"" /tmp/fallback-probe.json
    grep -q "\"color_primaries\": \"bt2020\"" /tmp/fallback-probe.json
    grep -q "\"codec_name\": \"ac3\"" /tmp/fallback-probe.json
    ! grep -q "DOVI configuration record" /tmp/fallback-probe.json
    ffmpeg -nostdin -v error -i /tmp/fallback.mp4 -map 0:v:0 -frames:v 1 -f null -
'
$COMPOSE kill --signal SIGTERM rusty-dlna-smoke

tries=0
while $COMPOSE ps --status running --quiet | grep -q .; do
    tries=$((tries + 1))
    [ "$tries" -lt 20 ] || { echo "smoke daemon did not stop" >&2; exit 1; }
    sleep 1
done
logs=$($COMPOSE logs --no-color rusty-dlna-smoke)
printf '%s\n' "$logs" | grep -q 'SSDP byebye sent'
shutdown_log=$(printf '%s\n' "$logs" | grep 'graceful shutdown complete')
printf '%s\n' "$shutdown_log" | grep -q 'duration_ms'
echo "compose smoke OK"
