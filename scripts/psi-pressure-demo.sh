#!/bin/sh
# Generate synthetic CPU pressure and watch Linux PSI respond.
#
# Usage: scripts/psi-pressure-demo.sh [loops] [seconds]
#   loops    number of busy loops to start (default: nproc)
#   seconds  how long to sample /proc/pressure/cpu, once per second (default 15)
#
# Starts the busy loops, prints the `some` line of /proc/pressure/cpu every
# second, then stops the loops (also on Ctrl-C). Used to record real numbers
# for the Phase 4 acceptance document; not a CI test. Exits 2 when the
# kernel has no PSI (missing /proc/pressure or booted with psi=0).
set -eu

LOOPS="${1:-$(nproc)}"
SECONDS_TO_SAMPLE="${2:-15}"
PSI=/proc/pressure/cpu

if ! cat "$PSI" >/dev/null 2>&1; then
    echo "psi-pressure-demo: $PSI is not readable; this kernel has no PSI" >&2
    exit 2
fi

PIDS=""
stop() {
    # shellcheck disable=SC2086
    [ -n "$PIDS" ] && kill $PIDS 2>/dev/null || true
    PIDS=""
}
trap stop EXIT INT TERM

echo "baseline: $(head -n1 "$PSI")"
i=0
while [ "$i" -lt "$LOOPS" ]; do
    sh -c 'while :; do :; done' &
    PIDS="$PIDS $!"
    i=$((i + 1))
done
echo "started $LOOPS busy loop(s) on $(nproc) CPU(s)"

t=1
while [ "$t" -le "$SECONDS_TO_SAMPLE" ]; do
    sleep 1
    printf '%3ss  %s\n' "$t" "$(head -n1 "$PSI")"
    t=$((t + 1))
done

stop
echo "stopped the busy loops"
sleep 1
echo "after:    $(head -n1 "$PSI")"
