#!/bin/sh
set -eu

if [ -n "${RUSTY_DLNA_PLAYWRIGHT_HARNESS_ROOT:-}" ]; then
	ROOT=$RUSTY_DLNA_PLAYWRIGHT_HARNESS_ROOT
else
	ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
fi
WRAPPER="$ROOT/scripts/playwright-server.sh"

if [ -n "${RUSTY_DLNA_PLAYWRIGHT_FAKE_MODE:-}" ]; then
	printf '%s\n' "$$" >"$RUSTY_DLNA_PLAYWRIGHT_PID_RECORD"
	if [ -n "${RUSTY_DLNA_PLAYWRIGHT_STAT_RECORD:-}" ] && [ -r "/proc/$$/stat" ]; then
		cat "/proc/$$/stat" >"$RUSTY_DLNA_PLAYWRIGHT_STAT_RECORD"
	fi
	wrapper_pid=${RUSTY_DLNA_PLAYWRIGHT_WRAPPER_PID:-$PPID}
	runtime=$(cat "$RUSTY_DLNA_PLAYWRIGHT_RUNTIME_RECORD")
	test -d "$runtime/library"
	test ! -L "$runtime/library"
	cmp "$ROOT/testdata/library/video/movie.nfo" "$runtime/library/video/movie.nfo"
	case "$RUSTY_DLNA_PLAYWRIGHT_FAKE_MODE" in
		exit-zero)
			exit 0
			;;
		exit-failure)
			exit 42
			;;
		signal-int | signal-term | signal-hup)
			trap 'exit 0' INT TERM HUP
			case "$RUSTY_DLNA_PLAYWRIGHT_FAKE_MODE" in
				signal-int) signal=INT ;;
				signal-term) signal=TERM ;;
				signal-hup) signal=HUP ;;
			esac
			sleep 0.05
			kill "-$signal" "$wrapper_pid"
			while :; do
				sleep 0.05
			done
			;;
		signal-term-resistant-direct)
			trap '' TERM
			sleep 0.05
			kill -TERM "$wrapper_pid"
			exec sleep 300
			;;
		signal-term-self-exit)
			trap 'exit 0' TERM
			sleep 0.05
			kill -TERM "$wrapper_pid"
			sleep 0.5
			exit 0
			;;
		signal-term-responsive-leader-resistant-descendant)
			trap 'exit 0' TERM
			command_leader=$$
			sentinel=$PPID
			printf '%s\n' "$sentinel" >"$RUSTY_DLNA_PLAYWRIGHT_SENTINEL_RECORD"
			(
				trap '' TERM
				sleep 0.5
				command_state=missing
				[ ! -r "/proc/$command_leader/stat" ] \
					|| command_state=$(awk '{ print $3 }' "/proc/$command_leader/stat")
				[ "$command_state" = missing ] || [ "$command_state" = Z ]
				[ -r "/proc/$sentinel/stat" ]
				set -- $(awk '{ print $3, $5 }' "/proc/$sentinel/stat")
				[ "$1" != Z ]
				[ "$2" -eq "$sentinel" ]
				printf 'reserved\n' >"$RUSTY_DLNA_PLAYWRIGHT_RESERVATION_RECORD"
				printf 'sent\n' >"$RUSTY_DLNA_PLAYWRIGHT_SECOND_SIGNAL_RECORD"
				kill -TERM "$wrapper_pid"
				exec sleep 300
			) &
			descendant=$!
			printf '%s\n' "$descendant" >"$RUSTY_DLNA_PLAYWRIGHT_DESCENDANT_RECORD"
			sleep 0.05
			kill -TERM "$wrapper_pid"
			while :; do
				sleep 0.05
			done
			;;
		*)
			echo "unknown fake server mode: $RUSTY_DLNA_PLAYWRIGHT_FAKE_MODE" >&2
			exit 2
			;;
	esac
fi

TEST_TMP=$(mktemp -d "${TMPDIR:-/tmp}/rusty-dlna-playwright-harness.XXXXXX")

cleanup() {
	status=$?
	trap - EXIT
	trap '' INT TERM HUP
	for record in "$TEST_TMP"/*/pid "$TEST_TMP"/*/descendant "$TEST_TMP"/*/sentinel; do
		[ -s "$record" ] || continue
		pid=$(cat "$record")
		case "$pid" in
			'' | *[!0-9]*) continue ;;
		esac
		# Never act on a reused PID. Linux CI proves ownership through the
		# per-run marker inherited by fake children; other hosts skip emergency
		# signaling when process identity cannot be proven.
		if [ -r "/proc/$pid/environ" ] \
			&& tr '\000' '\n' <"/proc/$pid/environ" \
				| grep -Fqx "RUSTY_DLNA_PLAYWRIGHT_HARNESS_TOKEN=$TEST_TMP"; then
			kill -KILL "$pid" 2>/dev/null || true
		fi
	done
	rm -rf -- "$TEST_TMP"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

assert_stopped_and_clean() {
	case_dir=$1
	expected_status=$2
	actual_status=$3
	if [ "$actual_status" -ne "$expected_status" ]; then
		cat "$case_dir/stderr" >&2
		echo "expected status $expected_status, got $actual_status for $case_dir" >&2
		return 1
	fi
	runtime=$(cat "$case_dir/runtime")
	test ! -e "$runtime"
	if [ -s "$case_dir/pid" ]; then
		child=$(cat "$case_dir/pid")
		! kill -0 "$child" 2>/dev/null
	fi
	if [ -s "$case_dir/descendant" ]; then
		descendant=$(cat "$case_dir/descendant")
		! kill -0 "$descendant" 2>/dev/null
	fi
}

run_sync_case() {
	name=$1
	build_program=$2
	mode=$3
	expected_status=$4
	disable_setsid=${5:-0}
	require_reservation=${6:-0}
	expect_kill=${7:-any}
	odd_process_name=${8:-0}
	case_dir="$TEST_TMP/$name"
	mkdir -p "$case_dir/tmp"
	server_program=$0
	if [ "$odd_process_name" -eq 1 ]; then
		server_program="$case_dir/own) name"
		ln -s "$ROOT/scripts/test-playwright-server.sh" "$server_program"
	fi
	if TMPDIR="$case_dir/tmp" \
		RUSTY_DLNA_PLAYWRIGHT_BUILD_PROGRAM="$build_program" \
		RUSTY_DLNA_PLAYWRIGHT_SERVER_PROGRAM="$server_program" \
		RUSTY_DLNA_PLAYWRIGHT_FAKE_MODE="$mode" \
		RUSTY_DLNA_PLAYWRIGHT_PID_RECORD="$case_dir/pid" \
		RUSTY_DLNA_PLAYWRIGHT_DESCENDANT_RECORD="$case_dir/descendant" \
		RUSTY_DLNA_PLAYWRIGHT_RUNTIME_RECORD="$case_dir/runtime" \
		RUSTY_DLNA_PLAYWRIGHT_RESERVATION_RECORD="$case_dir/reservation" \
		RUSTY_DLNA_PLAYWRIGHT_SENTINEL_RECORD="$case_dir/sentinel" \
		RUSTY_DLNA_PLAYWRIGHT_SIGNAL_RECORD="$case_dir/signals" \
		RUSTY_DLNA_PLAYWRIGHT_STAT_RECORD="$case_dir/stat" \
		RUSTY_DLNA_PLAYWRIGHT_SECOND_SIGNAL_RECORD="$case_dir/second-signal" \
		RUSTY_DLNA_PLAYWRIGHT_DISABLE_SETSID="$disable_setsid" \
		RUSTY_DLNA_PLAYWRIGHT_HARNESS_TOKEN="$TEST_TMP" \
		RUSTY_DLNA_PLAYWRIGHT_HARNESS_ROOT="$ROOT" \
		"$WRAPPER" >"$case_dir/stdout" 2>"$case_dir/stderr"; then
		status=0
	else
		status=$?
	fi
	assert_stopped_and_clean "$case_dir" "$expected_status" "$status"
	if [ "$require_reservation" -eq 1 ] && [ -r /proc/self/stat ]; then
		if ! grep -Fxq reserved "$case_dir/reservation"; then
			cat "$case_dir/reservation" >&2
			return 1
		fi
		grep -Fxq sent "$case_dir/second-signal"
		sentinel=$(cat "$case_dir/sentinel")
		! kill -0 "$sentinel" 2>/dev/null
	fi
	case "$expect_kill" in
		yes) grep -q '^KILL ' "$case_dir/signals" ;;
		no) ! grep -q '^KILL ' "$case_dir/signals" ;;
		any) ;;
		*) return 2 ;;
	esac
	if [ "$odd_process_name" -eq 1 ]; then
		grep -Fq '(own) name)' "$case_dir/stat"
		grep -q '^TERM ' "$case_dir/signals"
	fi
}

FIXTURE_STATE_BEFORE=$(find "$ROOT/testdata/library" -type f -exec sha256sum {} + | LC_ALL=C sort)
run_sync_case ordinary /usr/bin/true exit-zero 0
run_sync_case build-failure /usr/bin/false exit-zero 1
run_sync_case launch-failure /usr/bin/true exit-failure 42
run_sync_case interrupt /usr/bin/true signal-int 130
run_sync_case terminate /usr/bin/true signal-term 143
run_sync_case hangup /usr/bin/true signal-hup 129
run_sync_case direct-responsive-termination /usr/bin/true signal-term 143 1 0 no
if [ -r /proc/self/stat ]; then
	run_sync_case odd-process-name /usr/bin/true signal-term-self-exit 143 1 0 no 1
fi
run_sync_case forced-server-termination /usr/bin/true signal-term-resistant-direct 143 1 0 yes
run_sync_case forced-build-termination "$0" signal-term-resistant-direct 143 1 0 yes
if command -v setsid >/dev/null 2>&1; then
	run_sync_case group-descendant-termination "$0" \
		signal-term-responsive-leader-resistant-descendant 143 0 1 yes
fi
FIXTURE_STATE_AFTER=$(find "$ROOT/testdata/library" -type f -exec sha256sum {} + | LC_ALL=C sort)
test "$FIXTURE_STATE_AFTER" = "$FIXTURE_STATE_BEFORE"

echo "playwright server lifecycle OK"
