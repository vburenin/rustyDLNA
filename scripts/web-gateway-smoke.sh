#!/bin/sh
set -eu

base=${1:-http://127.0.0.1:8201}

expect_status() {
	expected=$1
	method=$2
	path=$3
	actual=$(curl --silent --output /dev/null --write-out '%{http_code}' \
		--request "$method" "$base$path")
	if [ "$actual" != "$expected" ]; then
		echo "expected $method $path to return $expected, got $actual" >&2
		exit 1
	fi
}

expect_status 200 GET /
expect_status 200 GET /web/app.css
expect_status 200 GET /api/web/library
expect_status 200 GET /health

encoding=$(curl --silent --dump-header - --output /dev/null \
	--header 'Accept-Encoding: gzip' "$base/web/app.js" \
	| tr -d '\r' \
	| awk 'tolower($1) == "content-encoding:" { print tolower($2) }')
if [ "$encoding" != "gzip" ]; then
	echo "expected the browser gateway to gzip JavaScript, got ${encoding:-no encoding}" >&2
	exit 1
fi

# Every DLNA/UPnP surface is unreachable through this container.
expect_status 404 GET /rootDesc.xml
expect_status 404 GET /ContentDir.xml
expect_status 404 POST /ctl/ContentDir
expect_status 404 SUBSCRIBE /evt/ContentDir
expect_status 404 GET /MediaItems/1.mkv
expect_status 404 GET /Transcode/1.mp4
expect_status 404 GET /Icons/48x48.png

# Startup telemetry is the sole POST route in the browser allowlist. Missing
# bounded event parameters must reach rustyDLNA's 400 response instead of being
# hidden by the gateway's 404 policy.
expect_status 400 POST /api/web/transcode/1

# POST is SOAP in the backend regardless of its path, so the gateway must also
# reject it when an attacker places it below a browser prefix.
expect_status 404 POST /api/web/library

echo "browser gateway route isolation OK"
