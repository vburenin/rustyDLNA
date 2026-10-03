#!/bin/sh
# Exercise the HTTP contract that the native rustyView client relies on against
# a running server: schema 2 JSON, string IDs, the video/audio kind set, HEAD
# and Range on media, DELETE reaching the backend, native HLS playlist tags,
# and the progressive download 206/202/416/HEAD semantics.
#
# Usage: web-contract-smoke.sh BASE_URL [FILE_NAME] [light|full]
#
# FILE_NAME selects a library entry by its exact file name; empty selects the
# first video entry. `light` checks only metadata, original-media HEAD/Range,
# and DELETE routing, and never starts a transcode. `full` (the default) also
# prepares a short compatible HLS rendition and a progressive download.
# Requires POSIX sh, curl, grep, sed, awk, od and tr; no JSON tool.
set -eu

base=${1:?usage: web-contract-smoke.sh BASE_URL [FILE_NAME] [light|full]}
file_name=${2:-}
mode=${3:-full}
timeout_secs=${RUSTY_DLNA_CONTRACT_TIMEOUT:-180}
case "$mode" in
light | full) ;;
*)
	echo "web contract: mode must be light or full" >&2
	exit 2
	;;
esac
case "$timeout_secs" in
'' | *[!0-9]* | 0)
	echo "web contract: RUSTY_DLNA_CONTRACT_TIMEOUT must be a positive integer" >&2
	exit 2
	;;
esac

tmp=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-contract.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
	echo "web contract: $*" >&2
	exit 1
}

# fetch METHOD PATH [curl options...]: response headers land in $tmp/headers,
# the body in $tmp/body, and the status code is printed.
fetch() {
	method=$1
	path=$2
	shift 2
	rm -f "$tmp/headers"
	: >"$tmp/body"
	if [ "$method" = HEAD ]; then
		# curl copies HEAD response headers to its output; keep the body empty.
		set -- --head --output /dev/null "$@"
	else
		set -- --request "$method" --output "$tmp/body" "$@"
	fi
	curl --silent --show-error --max-time 60 --dump-header "$tmp/headers" \
		--write-out '%{http_code}' "$@" "$base$path"
}

# header NAME: the last value of a response header (case-insensitive name).
header() {
	tr -d '\r' <"$tmp/headers" | awk -v name="$1" '
		BEGIN { name = tolower(name) }
		{
			colon = index($0, ":")
			if (colon > 0 && tolower(substr($0, 1, colon - 1)) == name) {
				value = substr($0, colon + 1)
				sub(/^[ \t]+/, "", value)
				sub(/[ \t]+$/, "", value)
			}
		}
		END { print value }'
}

body_bytes() {
	wc -c <"$tmp/body" | tr -d ' '
}

expect_status() {
	[ "$1" = "$2" ] || fail "$3 returned HTTP $2, expected $1: $(head -c 300 "$tmp/body" 2>/dev/null || true)"
}

# Schema 2 JSON with string IDs and only the kinds rustyView decodes.
expect_schema_json() {
	what=$1
	case "$(header Content-Type)" in
	application/json*) ;;
	*) fail "$what is not JSON: $(header Content-Type)" ;;
	esac
	head -c 20 "$tmp/body" | grep -q '^{"schema_version":2,' ||
		fail "$what does not start with schema_version 2"
	if grep -o '"kind":"[^"]*"' "$tmp/body" | grep -v -x -e '"kind":"video"' -e '"kind":"audio"' >"$tmp/kinds"; then
		fail "$what has a kind other than video/audio: $(head -n 3 "$tmp/kinds" | tr '\n' ' ')"
	fi
	if grep -q -E '"(id|item_id)":-?[0-9]' "$tmp/body"; then
		fail "$what has a numeric ID; rustyView requires string IDs"
	fi
}

# Field values from one compact JSON object chunk (first occurrence).
string_field() {
	grep -o "\"$1\":\"[^\"]*\"" "$tmp/entry" | head -n 1 | sed "s/^\"$1\":\"//; s/\"\$//"
}

# Bytes 4..7 of a fragmented MP4 initialization are `ftyp`.
expect_ftyp() {
	[ "$(od -An -c -j 4 -N 4 "$tmp/body" | tr -d ' \n')" = ftyp ] || fail "$1 does not start with an ftyp box"
}

request_id=$(($(date +%s) % 100000000 * 1000 + $$ % 1000))

echo "web contract: library and item JSON"
status=$(fetch GET "/api/web/library?view=library&kind=video&sort=title&offset=0&limit=200")
expect_status 200 "$status" "library"
expect_schema_json "library"
# Split the compact JSON into one chunk per entry object. awk keeps this
# portable; GNU-only `\n` in a sed replacement is not.
if ! awk '{
		rest = $0
		while ((at = index(rest, "{\"entry_type\":")) > 0) {
			if (at > 1) printf "%s", substr(rest, 1, at - 1)
			printf "\n"
			printf "%s", substr(rest, at, 14)
			rest = substr(rest, at + 14)
		}
		printf "%s\n", rest
	}' "$tmp/body" | grep '^{"entry_type":"media"' >"$tmp/entries"; then
	# An operator's gateway may front a library that is still empty. Without
	# a requested title, keep the method-routing check and skip media checks.
	[ -z "$file_name" ] && [ "$mode" = light ] || fail "library has no media entries"
	echo "web contract: no media among the first 200 library entries (empty, or only folders); skipping media checks"
	status=$(fetch DELETE "/api/web/transcode/1")
	expect_status 400 "$status" "DELETE without a request ID"
	expect_schema_json "DELETE error"
	echo "web contract OK (light, empty library)"
	exit 0
fi
if [ -n "$file_name" ]; then
	grep -F "\"file_name\":\"$file_name\"" "$tmp/entries" | head -n 1 >"$tmp/entry" || true
else
	grep -F '"kind":"video"' "$tmp/entries" | head -n 1 >"$tmp/entry" || true
fi
[ -s "$tmp/entry" ] || fail "library has no entry for ${file_name:-a video}"
id=$(string_field id)
source_url=$(string_field source_url)
fallback_url=$(string_field fallback_url)
case "$id" in
'' | *[!0-9]*) fail "entry ID is not a decimal string: $id" ;;
esac
case "$source_url" in
/web/media/*) ;;
*) fail "source_url is not a same-origin /web/media path: $source_url" ;;
esac

status=$(fetch GET "/api/web/item/$id?enrich=1")
expect_status 200 "$status" "item $id"
expect_schema_json "item $id"
grep -q "\"id\":\"$id\"" "$tmp/body" || fail "item $id does not echo its string ID"

echo "web contract: original media HEAD and Range"
status=$(fetch GET "$source_url" --header 'Range: bytes=0-1')
expect_status 206 "$status" "original Range"
content_range=$(header Content-Range)
original_size=${content_range#bytes 0-1/}
case "$content_range" in
"bytes 0-1/"[0-9]*) ;;
*) fail "original Range has Content-Range '$content_range'" ;;
esac
case "$original_size" in
'' | *[!0-9]*) fail "original Range has Content-Range '$content_range'" ;;
esac
[ "$(body_bytes)" = 2 ] || fail "original Range body is not two bytes"
case "$(header ETag)" in
\"*\") ;;
*) fail "original Range has no quoted ETag" ;;
esac
status=$(fetch HEAD "$source_url")
expect_status 200 "$status" "original HEAD"
[ "$(header Content-Length)" = "$original_size" ] ||
	fail "original HEAD Content-Length $(header Content-Length) differs from $original_size"

echo "web contract: DELETE reaches transcode cancellation"
status=$(fetch DELETE "/api/web/transcode/$id")
expect_status 400 "$status" "DELETE without a request ID"
expect_schema_json "DELETE error"

if [ "$mode" = light ]; then
	echo "web contract OK (light)"
	exit 0
fi

# rustyView's compatible query (RustyDLNAClient.compatiblePath).
compatible="mode=compatible&audio=0&start=0&quality=auto&video_mode=copy&audio_mode=transcode&reason=native_ios"
hls_path="${fallback_url%.mp4}.m3u8?$compatible&request=$request_id&session=$request_id&delivery=hls"
download_request=$((request_id + 1))
download_path="$fallback_url?$compatible&request=$download_request&session=$download_request&download_audio=selected"
case "$fallback_url" in
/web/media/*.mp4) ;;
*) fail "fallback_url is not a same-origin MP4 path: $fallback_url" ;;
esac

# poll_ready PATH ACCEPT_STATUS [curl options...]: retry 202 preparation using
# its Retry-After (bounded) until the accepted status or the deadline.
# POSIX sh has no local variables, so every name here carries a poll_ prefix
# and cannot overwrite a caller's deadline or path.
poll_ready() {
	poll_path=$1
	poll_accept=$2
	shift 2
	poll_started=$(date +%s)
	while :; do
		status=$(fetch GET "$poll_path" "$@")
		[ "$status" = "$poll_accept" ] && return 0
		[ "$status" = 202 ] || fail "$poll_path returned HTTP $status while preparing"
		poll_retry=$(header Retry-After)
		case "$poll_retry" in
		'' | *[!0-9]*) fail "202 preparation has a non-numeric Retry-After '$poll_retry'" ;;
		esac
		[ $(($(date +%s) - poll_started)) -lt "$timeout_secs" ] ||
			fail "$poll_path did not become ready in ${timeout_secs}s"
		[ "$poll_retry" -ge 1 ] && [ "$poll_retry" -le 5 ] || poll_retry=1
		sleep "$poll_retry"
	done
}

echo "web contract: native HLS playlist"
poll_ready "$hls_path" 200
case "$(header Content-Type)" in
*mpegurl*) ;;
*) fail "HLS playlist Content-Type is $(header Content-Type)" ;;
esac
case "$(header Content-Encoding)" in
'' | identity) ;;
*) fail "HLS playlist is encoded; the rustyView relay requires identity" ;;
esac
tr -d '\r' <"$tmp/body" >"$tmp/playlist"
[ "$(head -n 1 "$tmp/playlist")" = '#EXTM3U' ] || fail "playlist does not start with #EXTM3U"
# The rustyView relay fails closed on any other tag
# (MediaRelayProtocol.plainTags/attributeTags in the rustyView app).
# #EXT-X-RUSTY-TIMING is Media Source metadata and never appears here.
grep '^#EXT' "$tmp/playlist" | sed 's/:.*//' | sort -u >"$tmp/tags"
while IFS= read -r tag; do
	case "$tag" in
	'#EXTM3U' | '#EXTINF' | '#EXT-X-VERSION' | '#EXT-X-TARGETDURATION' | \
		'#EXT-X-MEDIA-SEQUENCE' | '#EXT-X-DISCONTINUITY-SEQUENCE' | '#EXT-X-ENDLIST' | \
		'#EXT-X-PLAYLIST-TYPE' | '#EXT-X-I-FRAMES-ONLY' | '#EXT-X-INDEPENDENT-SEGMENTS' | \
		'#EXT-X-START' | '#EXT-X-DISCONTINUITY' | '#EXT-X-PROGRAM-DATE-TIME' | '#EXT-X-GAP' | \
		'#EXT-X-BYTERANGE' | '#EXT-X-BITRATE' | '#EXT-X-ALLOW-CACHE' | '#EXT-X-STREAM-INF' | \
		'#EXT-X-I-FRAME-STREAM-INF' | '#EXT-X-MEDIA' | '#EXT-X-KEY' | '#EXT-X-SESSION-KEY' | \
		'#EXT-X-MAP') ;;
	*) fail "native HLS playlist has a tag rustyView rejects: $tag" ;;
	esac
done <"$tmp/tags"
# The relay also rejects a URI= or URL= reference on any plain (non-attribute)
# tag line, since only attribute tags have their references rewritten.
if grep '^#EXT' "$tmp/playlist" | grep -v -E '^#EXT-X-(STREAM-INF|I-FRAME-STREAM-INF|MEDIA|KEY|SESSION-KEY|MAP):' |
	grep -E 'URI=|URL=' >"$tmp/plain-refs"; then
	fail "native HLS playlist has a plain tag with a reference rustyView rejects: $(head -n 1 "$tmp/plain-refs")"
fi
init_uri=$(sed -n 's/^#EXT-X-MAP:URI="\([^"]*\)".*/\1/p' "$tmp/playlist" | head -n 1)
segment_uri=$(grep -v -e '^#' -e '^$' "$tmp/playlist" | head -n 1)
for uri in "$init_uri" "$segment_uri"; do
	case "$uri" in
	/web/media/*) ;;
	*) fail "playlist URI is not a same-origin /web/media path: $uri" ;;
	esac
done
if grep -v -e '^#' -e '^$' "$tmp/playlist" | grep -v '^/web/media/' >/dev/null; then
	fail "playlist has a segment URI outside /web/media"
fi
status=$(fetch GET "$init_uri")
expect_status 200 "$status" "HLS initialization"
expect_ftyp "HLS initialization"
status=$(fetch GET "$segment_uri")
expect_status 200 "$status" "HLS segment"
grep -a -q moof "$tmp/body" || fail "HLS segment has no moof box"

echo "web contract: progressive download"
download_started=$(date +%s)
while :; do
	poll_ready "$download_path" 206 \
		--header 'X-RustyDLNA-Download: progressive' --header 'Range: bytes=0-'
	content_range=$(header Content-Range)
	total=${content_range##*/}
	case "$total" in
	'' | *[!0-9]*) ;;
	*) break ;;
	esac
	[ $(($(date +%s) - download_started)) -lt "$timeout_secs" ] ||
		fail "download length stayed unknown: $content_range"
	sleep 1
done
[ "$(header X-RustyDLNA-Download)" = progressive ] || fail "206 download lacks X-RustyDLNA-Download: progressive"
case "$(header ETag)" in
\"*\") ;;
*) fail "206 download has no quoted ETag" ;;
esac
case "$content_range" in
"bytes 0-$((total - 1))/$total") ;;
*) fail "206 download Content-Range is '$content_range'" ;;
esac
[ "$(body_bytes)" = "$total" ] || fail "206 download body is $(body_bytes) bytes, expected $total"
expect_ftyp "download"
status=$(fetch GET "$download_path" \
	--header 'X-RustyDLNA-Download: progressive' --header "Range: bytes=$total-")
expect_status 416 "$status" "download Range past the end"
[ "$(header Content-Range)" = "bytes */$total" ] || fail "416 Content-Range is '$(header Content-Range)'"
[ "$(header X-RustyDLNA-Download)" = progressive ] || fail "416 lacks X-RustyDLNA-Download: progressive"
[ -n "$(header ETag)" ] || fail "416 lacks an ETag"
status=$(fetch HEAD "$download_path" --header 'Accept: video/mp4, application/octet-stream')
expect_status 200 "$status" "completed download HEAD"
case "$(header Content-Type)" in
video/mp4*) ;;
*) fail "completed download HEAD Content-Type is $(header Content-Type)" ;;
esac
[ "$(header Content-Length)" = "$total" ] ||
	fail "completed download HEAD Content-Length $(header Content-Length) differs from $total"

echo "web contract: cancellation"
status=$(fetch DELETE "/api/web/transcode/$id?request=$request_id&session=$request_id")
expect_status 200 "$status" "DELETE of the HLS generation"
expect_schema_json "cancellation"
grep -q "\"item_id\":\"$id\"" "$tmp/body" || fail "cancellation does not echo the string item_id"

echo "web contract OK (full)"
