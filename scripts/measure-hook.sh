#!/bin/sh
# Times a hook command and appends one CSV line to the validation log.
#
# Usage, inside a git hook:
#   scripts/measure-hook.sh <label> <command...>
#
# Appends `unix_seconds,label,duration_ms,exit_code,worktree` to
# $SLUICE_VALIDATION_LOG (default: ~/.local/state/sluice/validation.csv) and
# exits with the command's exit code. Used for the before/after numbers of
# the real-world validation (docs/validation.md). Linux only.
set -u

label="$1"
shift
log="${SLUICE_VALIDATION_LOG:-$HOME/.local/state/sluice/validation.csv}"
mkdir -p "$(dirname "$log")"

start=$(date +%s%N)
"$@"
code=$?
end=$(date +%s%N)
printf '%s,%s,%s,%s,%s\n' \
    "$(date +%s)" "$label" "$(( (end - start) / 1000000 ))" "$code" "$(pwd)" >> "$log"
exit "$code"
