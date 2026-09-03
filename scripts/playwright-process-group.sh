#!/bin/sh
set -u

[ "$#" -gt 0 ] || {
	echo "usage: playwright-process-group.sh PROGRAM [ARG ...]" >&2
	exit 2
}

STOPPING=0
trap 'STOPPING=1' INT TERM HUP

"$@" &
CHILD_PID=$!
if wait "$CHILD_PID"; then
	CHILD_STATUS=0
else
	CHILD_STATUS=$?
fi

if [ "$STOPPING" -eq 1 ]; then
	# The outer wrapper owns escalation. Retaining this group leader until KILL
	# prevents its PID/PGID from being reused while descendants are still live.
	trap '' INT TERM HUP
	while :; do
		sleep 60
	done
fi

exit "$CHILD_STATUS"
