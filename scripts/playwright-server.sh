#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
GROUP_RUNNER="$ROOT/scripts/playwright-process-group.sh"
RUNTIME_DIR=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-playwright.XXXXXX")
BUILD_PID=
BUILD_PROCESS_GROUP=0
SERVER_PID=
SERVER_PROCESS_GROUP=0

process_is_owned() {
	pid=$1
	if [ -r "/proc/$pid/stat" ]; then
		IFS= read -r process_stat <"/proc/$pid/stat" || return 1
		# Linux comm may contain spaces and `)`; split after its final `) `.
		process_stat=${process_stat##*) }
		set -- $process_stat
		[ "${2:-}" = "$$" ] && [ "${1:-}" != Z ]
		return
	fi
	command -v ps >/dev/null 2>&1 || return 1
	process_info=$(ps -o ppid= -o stat= -p "$pid" 2>/dev/null)
	set -- $process_info
	case "${2:-}" in
		'' | Z*) return 1 ;;
	esac
	[ "${1:-}" = "$$" ]
}

process_is_alive() {
	pid=$1
	process_group=$2
	process_is_owned "$pid" || return 1
	if [ "$process_group" -eq 1 ]; then
		/bin/kill -0 -- "-$pid" 2>/dev/null
	else
		kill -0 "$pid" 2>/dev/null
	fi
}

signal_process() {
	signal=$1
	pid=$2
	process_group=$3
	if [ -n "${RUSTY_DLNA_PLAYWRIGHT_SIGNAL_RECORD:-}" ]; then
		printf '%s %s %s\n' "$signal" "$pid" "$process_group" \
			>>"$RUSTY_DLNA_PLAYWRIGHT_SIGNAL_RECORD"
	fi
	if [ "$process_group" -eq 1 ]; then
		/bin/kill "-$signal" -- "-$pid" 2>/dev/null
	else
		kill "-$signal" "$pid" 2>/dev/null
	fi
}

process_groups_available() {
	[ "${RUSTY_DLNA_PLAYWRIGHT_DISABLE_SETSID:-0}" != 1 ] \
		&& command -v setsid >/dev/null 2>&1
}

stop_process() {
	pid=$1
	process_group=$2
	[ -n "$pid" ] || return 0
	if [ "$process_group" -eq 1 ] && process_is_alive "$pid" "$process_group"; then
		signal_process TERM "$pid" "$process_group" || true
		# Keep the leader unreaped throughout the grace period so neither its
		# PID nor its private process-group ID can be reused before escalation.
		sleep 2
		process_is_alive "$pid" "$process_group" \
			&& signal_process KILL "$pid" "$process_group" || true
	elif [ "$process_group" -eq 0 ] && process_is_alive "$pid" "$process_group"; then
		signal_process TERM "$pid" "$process_group" || true
		attempts=0
		while [ "$attempts" -lt 40 ] && process_is_alive "$pid" "$process_group"; do
			sleep 0.05
			attempts=$((attempts + 1))
		done
		process_is_alive "$pid" "$process_group" \
			&& signal_process KILL "$pid" "$process_group" || true
	fi
	wait "$pid" 2>/dev/null || true
}

cleanup() {
	status=$?
	trap - EXIT
	trap '' INT TERM HUP
	stop_process "$BUILD_PID" "$BUILD_PROCESS_GROUP"
	BUILD_PID=
	stop_process "$SERVER_PID" "$SERVER_PROCESS_GROUP"
	SERVER_PID=
	rm -rf -- "$RUNTIME_DIR"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

if [ -n "${RUSTY_DLNA_PLAYWRIGHT_RUNTIME_RECORD:-}" ]; then
	printf '%s\n' "$RUNTIME_DIR" >"$RUSTY_DLNA_PLAYWRIGHT_RUNTIME_RECORD"
fi

cp -R -- "$ROOT/testdata/library" "$RUNTIME_DIR/library"
CONFIG="$RUNTIME_DIR/rusty-dlna.toml"
printf '%s\n' \
	'friendly_name = "rustyDLNA-web-test"' \
	'media_dir = ["library"]' \
	'exclude_dir = ["exclude_me"]' \
	'cache_dir = "cache"' \
	'db_dir = "db"' \
	'advertise_ip = "127.0.0.1"' \
	'uuid = "uuid:00000000-0000-4000-8000-000000000001"' \
	'notify_interval = 895' \
	'' \
	'[transcode]' \
	'enable = true' \
	'encoder = "libx264"' \
	'max_jobs = 1' \
	'' \
	'[[remap]]' \
	'name = "crkey-dvp7"' \
	'client = "CrKey"' \
	'hdr = "dv-p7"' \
	'action = "remux-p8"' \
	'encoder = "copy"' \
	'audio_out = "to-aac"' >"$CONFIG"

cd "$ROOT"
# The program and process-group overrides are reserved for the deterministic
# lifecycle harness.
BUILD_PROGRAM=${RUSTY_DLNA_PLAYWRIGHT_BUILD_PROGRAM:-cargo}
SERVER_PROGRAM=${RUSTY_DLNA_PLAYWRIGHT_SERVER_PROGRAM:-target/debug/rusty-dlna}
if process_groups_available; then
	# A private group lets cleanup terminate only this build and its descendants.
	BUILD_PROCESS_GROUP=1
	RUSTY_DLNA_PLAYWRIGHT_WRAPPER_PID=$$ \
		setsid "$GROUP_RUNNER" "$BUILD_PROGRAM" build --locked -p rusty-dlna &
else
	RUSTY_DLNA_PLAYWRIGHT_WRAPPER_PID=$$ \
		"$BUILD_PROGRAM" build --locked -p rusty-dlna &
fi
BUILD_PID=$!
if wait "$BUILD_PID"; then
	process_status=0
else
	process_status=$?
fi
BUILD_PID=
[ "$process_status" -eq 0 ] || exit "$process_status"
if process_groups_available; then
	# The server and any transient children receive the same bounded shutdown.
	SERVER_PROCESS_GROUP=1
	RUSTY_DLNA_PLAYWRIGHT_WRAPPER_PID=$$ RUSTY_DLNA_SSDP_PORT=11901 \
		setsid "$GROUP_RUNNER" "$SERVER_PROGRAM" -c "$CONFIG" -p 18201 &
else
	RUSTY_DLNA_PLAYWRIGHT_WRAPPER_PID=$$ RUSTY_DLNA_SSDP_PORT=11901 \
		"$SERVER_PROGRAM" -c "$CONFIG" -p 18201 &
fi
SERVER_PID=$!
if wait "$SERVER_PID"; then
	process_status=0
else
	process_status=$?
fi
SERVER_PID=
exit "$process_status"
