#!/bin/sh
# Measure the cache-hit latency of `sluice run` on a synthetic repository.
#
# Usage: scripts/bench-hit-path.sh [files] [iterations]
#   files       number of tracked files to generate (default 5000)
#   iterations  number of timed `sluice run` invocations (default 20)
#
# The script builds the release binary, creates a throwaway repository with
# the requested number of files, defines a trivial gate, runs it once to
# populate the cache, then times repeated hits. It prints min / median / max
# wall time in milliseconds. Nothing outside the temporary directory is
# touched.
set -eu

FILES="${1:-5000}"
ITER="${2:-20}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

cargo build --release --manifest-path "$ROOT/Cargo.toml" -p sluice-gate >/dev/null
SLUICE="$ROOT/target/release/sluice"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cd "$TMP"

git init -q -b main .
git config user.name bench
git config user.email bench@example.invalid
git config commit.gpgsign false

i=0
while [ "$i" -lt "$FILES" ]; do
    d="src/$((i / 100))"
    mkdir -p "$d"
    printf 'file %s\n' "$i" > "$d/f$i.txt"
    i=$((i + 1))
done
cat > .sluice.toml <<'EOF'
version = 1

[gates.noop]
cmd = "true"
EOF
git add -A
git commit -q -m "bench"

# Populate the cache.
"$SLUICE" run noop >/dev/null

# Time the hits. `date +%s%N` is GNU-specific; this script targets Linux.
times=""
i=0
while [ "$i" -lt "$ITER" ]; do
    start=$(date +%s%N)
    "$SLUICE" run noop >/dev/null
    end=$(date +%s%N)
    times="$times $(( (end - start) / 1000000 ))"
    i=$((i + 1))
done

sorted=$(printf '%s\n' $times | sort -n)
min=$(printf '%s\n' "$sorted" | head -1)
max=$(printf '%s\n' "$sorted" | tail -1)
median=$(printf '%s\n' "$sorted" | sed -n "$(( (ITER + 1) / 2 ))p")
echo "files=$FILES iterations=$ITER hit latency ms: min=$min median=$median max=$max"
echo "git version: $(git --version)"
