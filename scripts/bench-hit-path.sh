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

# Time the hits. Python measures the wall time of the sluice process alone
# (the shell's own `date` spawns would add about a millisecond each).
python3 -I - "$SLUICE" "$ITER" "$FILES" <<'PY'
import statistics, subprocess, sys, time
sluice, iterations, files = sys.argv[1], int(sys.argv[2]), sys.argv[3]
def bench(cmd):
    samples = []
    for _ in range(iterations):
        start = time.perf_counter()
        subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        samples.append((time.perf_counter() - start) * 1000)
    samples.sort()
    return (f"min={samples[0]:.1f} median={statistics.median(samples):.1f} "
            f"p90={samples[int(iterations * 0.9)]:.1f} max={samples[-1]:.1f} ms")
print(f"files={files} iterations={iterations}")
print("sluice run (hit):      ", bench([sluice, "run", "noop"]))
print("git status --porcelain:", bench(["git", "status", "--porcelain"]))
print("git --version:         ", bench(["git", "--version"]))
PY
echo "git version: $(git --version)"
echo "load average: $(cut -d' ' -f1-3 /proc/loadavg)"
