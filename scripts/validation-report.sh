#!/bin/sh
# Summarises a validation CSV written by scripts/measure-hook.sh.
#
# Usage: scripts/validation-report.sh [csv]   (default: ~/.local/state/sluice/validation.csv)
#
# Prints, per label: number of runs, runs per day, mean and median duration
# in seconds, and failures. Use it on the "before" log (hooks wrapped with
# measure-hook.sh, no Sluice) and on the "after" log (hooks calling sluice
# run, still wrapped) to compare. `sluice status --json` adds hits,
# coalesced requests and time saved for the "after" period.
set -eu

csv="${1:-$HOME/.local/state/sluice/validation.csv}"
[ -f "$csv" ] || { echo "no log at $csv" >&2; exit 1; }

sort -t, -k2,2 -k3,3n "$csv" | awk -F, '
{
    n[$2]++; sum[$2] += $3; if ($4 != 0) fail[$2]++
    day = int($1 / 86400); if (!(($2, day) in seen)) { seen[$2, day] = 1; days[$2]++ }
    vals[$2, n[$2]] = $3
}
END {
    printf "%-14s %6s %9s %10s %10s %6s\n", "label", "runs", "runs/day", "mean(s)", "median(s)", "fail"
    for (l in n) {
        m = n[l]; mid = int((m + 1) / 2)
        median = (m % 2) ? vals[l, mid] : (vals[l, mid] + vals[l, mid + 1]) / 2
        printf "%-14s %6d %9.1f %10.1f %10.1f %6d\n", l, m, m / days[l], sum[l] / m / 1000, median / 1000, fail[l] + 0
    }
}'
