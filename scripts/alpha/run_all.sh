#!/bin/sh
# Run every scenario script (s*.py: s1 ... s4, s6, s7_command_control, s8_zero_config, ...) next to this file, in name order, and exit
# non-zero if any of them fails. Environment: see README.md (ALPHA_TESTBEDS,
# ALPHA_PYLIB, ALPHA_OUT, TMPDIR, SLUICE_BIN, ...). Extra arguments are not
# forwarded; set the per-scenario knobs through the environment. s5_merge_queue.py
# is included (default transport: local bare repository).
set -u
cd "$(dirname "$0")" || exit 1
export PYTHONDONTWRITEBYTECODE=1

failed=""
ran=0
for script in s*.py; do
  [ -f "$script" ] || continue
  ran=$((ran + 1))
  echo "=== $script"
  if ! python3 "$script"; then
    failed="$failed $script"
  fi
done

if [ "$ran" -eq 0 ]; then
  echo "run_all: no scenario found" >&2
  exit 1
fi
if [ -n "$failed" ]; then
  echo "run_all: FAILED:$failed" >&2
  exit 1
fi
echo "run_all: $ran scenario(s) passed"
