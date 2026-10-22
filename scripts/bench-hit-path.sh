#!/bin/sh
# Measure the cache-hit latency of `sluice run` on a synthetic repository,
# for an unscoped gate and a path-scoped one.
#
# Usage: scripts/bench-hit-path.sh [files] [iterations]
#   files       number of tracked files to generate (default 5000)
#   iterations  timed `sluice run` invocations per series (default 40)
#
# Environment:
#   SLUICE_BASELINE  path of an older `sluice` binary (for example 0.2.0) to
#                    interleave with the unscoped gate. It runs in a linked
#                    worktree whose `.sluice.toml` has no path-scoped gate
#                    (older binaries reject `paths`); the new binary is timed
#                    in that worktree too, so the two compare like for like.
#
# The script builds the release binary, creates a throwaway repository with
# the requested number of files in `src/<n>/`, defines two trivial gates
# (`noop`, unscoped, and `scoped`, with `paths`), runs each once to populate
# the cache, then times the hits interleaved (ABAB...) so that machine load
# hits every series equally. It then edits a file outside the scope and
# times the scoped hits again (the case path scoping exists for). Every
# series prints min / median / p90 / max wall time in milliseconds. Nothing
# outside the temporary directory is touched.
set -eu

FILES="${1:-5000}"
ITER="${2:-40}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASELINE="${SLUICE_BASELINE:-}"

cargo build --release --manifest-path "$ROOT/Cargo.toml" -p sluice-gate >/dev/null
SLUICE="$ROOT/target/release/sluice"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir "$TMP/repo"
cd "$TMP/repo"

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

[gates.scoped]
cmd = "true"
paths = ["src/1*/**", ".sluice.toml"]
paths_ignore = ["src/12/**"]
EOF
git add -A
git commit -q -m "bench"

# Populate the cache.
"$SLUICE" run noop >/dev/null
"$SLUICE" run scoped >/dev/null

BASE_WT=""
if [ -n "$BASELINE" ]; then
    BASE_WT="$TMP/baseline"
    git worktree add -q "$BASE_WT" -b baseline
    printf 'version = 1\n\n[gates.noop]\ncmd = "true"\n' > "$BASE_WT/.sluice.toml"
    # The fresh index is racily clean (written in the same second as the
    # files), which makes every snapshot rehash all files: refresh it.
    sleep 1
    git -C "$BASE_WT" update-index -q --refresh || true
    (cd "$BASE_WT" && "$BASELINE" run noop >/dev/null)
fi

# Time the hits. Python measures the wall time of the sluice process alone
# (the shell's own `date` spawns would add about a millisecond each).
python3 -I - "$SLUICE" "$ITER" "$FILES" "$BASELINE" "$BASE_WT" <<'PY'
import os, statistics, subprocess, sys, time
sluice, iterations, files, baseline, base_wt = sys.argv[1:6]
iterations = int(iterations)

def run(cmd, cwd=None):
    start = time.perf_counter()
    subprocess.run(cmd, cwd=cwd, stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL, check=False)
    return (time.perf_counter() - start) * 1000

def check_hit(cmd, cwd=None):
    out = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if "PASS (cached" not in out.stdout:
        sys.exit(f"not a hit: {cmd} in {cwd}: {out.stdout}{out.stderr}")

def stats(samples):
    s = sorted(samples)
    return (f"min={s[0]:.1f} median={statistics.median(s):.1f} "
            f"p90={s[int(len(s) * 0.9)]:.1f} max={s[-1]:.1f} ms")

series = [
    ("unscoped (noop)", [sluice, "run", "noop"], None),
    ("scoped",          [sluice, "run", "scoped"], None),
]
if baseline:
    series.append(("baseline unscoped (baseline wt)", [baseline, "run", "noop"], base_wt))
    series.append(("unscoped (baseline wt)", [sluice, "run", "noop"], base_wt))
for _, cmd, cwd in series:
    check_hit(cmd, cwd)
samples = {name: [] for name, _, _ in series}
for _ in range(iterations):
    for name, cmd, cwd in series:
        samples[name].append(run(cmd, cwd))

print(f"files={files} iterations={iterations} (interleaved)")
for name, _, _ in series:
    print(f"{name:<32} {stats(samples[name])}")

# Edit a file outside the scope: the unscoped gate would miss, the scoped
# gate must still hit.
with open("src/3/f300.txt", "a") as f:
    f.write("edited outside the scope\n")
scoped = [sluice, "run", "scoped"]
check_hit(scoped)
edited = [run(scoped) for _ in range(iterations)]
print(f"{'scoped, out-of-scope edit':<32} {stats(edited)}")
print(f"{'git status --porcelain':<32} {stats([run(['git', 'status', '--porcelain']) for _ in range(iterations)])}")
print(f"{'git --version':<32} {stats([run(['git', '--version']) for _ in range(iterations)])}")
PY
echo "git version: $(git --version)"
echo "load average: $(cut -d' ' -f1-3 /proc/loadavg)"
