#!/bin/sh
# Before/after benchmark of Sluice on its own repository.
#
# Usage: scripts/validation-bench.sh [worktrees] [commits]   (defaults: 4 2)
#
# Simulates N agents in N git worktrees of this repository, each making
# `commits` commits that trigger the pre-commit gates of .sluice.toml (fmt,
# clippy), all worktrees in parallel:
#   A) without Sluice: the hook runs the gate commands directly;
#   B) with Sluice:    the hook runs `sluice run --hook pre-commit <gate>`,
#      private daemon autostarted, budget = 8.
# Every hook command is timed with scripts/measure-hook.sh (one CSV per
# scenario). The FIRST commit of every worktree has identical content
# (bench/notes.txt = "bench 1 <run id>" plus the same comment line appended to
# crates/sluice-gate-core/src/lib.rs, so cargo recompiles), so the trees coincide and Sluice
# should verify them once; later commits differ per worktree.
#
# Isolation: HOME, XDG_RUNTIME_DIR, XDG_STATE_HOME and XDG_CONFIG_HOME point
# into a temporary directory, so the real daemon, config and logs are never
# touched. The daemon is stopped (`sluice daemon stop --force`) and the
# worktrees, branches and temporary directory removed on exit and on Ctrl-C.
# Cargo and rustup keep using the real CARGO_HOME / RUSTUP_HOME. Build
# artifacts go to a shared target directory (SLUICE_BENCH_TARGET, default
# target/validation-bench), kept between runs, and warmed (fmt + clippy in the
# main checkout and in each worktree) before anything is timed, so the numbers
# measure contention, not compilation. The first-commit marker is made unique
# per run so earlier runs' cache markers in .git cannot produce hits.
# Per-worktree core.hooksPath is set with GIT_CONFIG_* variables, so the
# repository config is not modified. Linux only.
set -eu

N="${1:-4}"
COMMITS="${2:-2}"
case "$N$COMMITS" in
    *[!0-9]* | "") echo "usage: $0 [worktrees] [commits]" >&2; exit 2 ;;
esac
[ "$N" -ge 1 ] && [ "$COMMITS" -ge 1 ] || { echo "usage: $0 [worktrees] [commits]" >&2; exit 2; }

ROOT=$(cd "$(dirname "$0")/.." && pwd)
MEASURE="$ROOT/scripts/measure-hook.sh"
REPORT="$ROOT/scripts/validation-report.sh"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_TARGET_DIR="${SLUICE_BENCH_TARGET:-$ROOT/target/validation-bench}"
export LC_ALL=C

LOAD0=$(cut -d" " -f1-3 /proc/loadavg)
BASE=$(git -C "$ROOT" rev-parse HEAD)
RUNID="run-$$-$(date +%s)"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/sluice-bench.XXXXXX")
export HOME="$TMP/home"
export XDG_RUNTIME_DIR="$TMP/run"
export XDG_STATE_HOME="$TMP/state"
export XDG_CONFIG_HOME="$TMP/config"
export SLUICE_CONFIG="$XDG_CONFIG_HOME/sluice/config.toml"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME/sluice"
chmod 700 "$XDG_RUNTIME_DIR"
printf 'version = 1\n\n[daemon]\nbudget = 8\n' > "$SLUICE_CONFIG"
# Humans wait on a miss; an agent environment would fail fast. Cache stays on.
export SLUICE_AGENT=0
unset CI CLAUDECODE SLUICE_DISABLE SLUICE_CACHE SLUICE_SOCKET || true
export GIT_AUTHOR_NAME=bench GIT_AUTHOR_EMAIL=bench@example.invalid
export GIT_COMMITTER_NAME=bench GIT_COMMITTER_EMAIL=bench@example.invalid
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

SLUICE="$ROOT/target/release/sluice"
PIDS=""
CREATED=0

cleanup() {
    trap '' INT TERM HUP
    for p in $PIDS; do kill "$p" 2>/dev/null || true; done
    if [ -x "$SLUICE" ]; then "$SLUICE" daemon stop --force >/dev/null 2>&1 || true; fi
    i=1
    while [ "$i" -le "$CREATED" ]; do
        git -C "$ROOT" worktree remove --force "$TMP/wt-$i" >/dev/null 2>&1 || true
        git -C "$ROOT" branch -D "bench-$i" >/dev/null 2>&1 || true
        i=$((i + 1))
    done
    git -C "$ROOT" worktree prune >/dev/null 2>&1 || true
    rm -rf "$TMP"
}
trap cleanup EXIT
trap 'echo "interrupted" >&2; exit 130' INT TERM HUP

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }

echo "== building release binary" >&2
(cd "$ROOT" && CARGO_TARGET_DIR="$ROOT/target" cargo build --release -p sluice-gate >&2)

echo "== warming the shared target dir ($CARGO_TARGET_DIR)" >&2
(cd "$ROOT" && cargo fmt --all --check >&2 && cargo clippy --workspace --all-targets -- -D warnings >&2)

echo "== creating $N worktrees" >&2
i=1
while [ "$i" -le "$N" ]; do
    git -C "$ROOT" worktree add -q "$TMP/wt-$i" -b "bench-$i" "$BASE" >&2
    CREATED=$i
    (cd "$TMP/wt-$i" && cargo clippy --workspace --all-targets -- -D warnings >&2)
    i=$((i + 1))
done

# make_hook <dir> <mode>: mode "direct" or "sluice".
make_hook() {
    mkdir -p "$1"
    if [ "$2" = direct ]; then
        cat > "$1/pre-commit" <<HOOK
#!/bin/sh
set -e
"$MEASURE" fmt cargo fmt --all --check
"$MEASURE" clippy cargo clippy --workspace --all-targets -- -D warnings
HOOK
    else
        cat > "$1/pre-commit" <<HOOK
#!/bin/sh
set -e
"$MEASURE" fmt "$SLUICE" run --hook pre-commit fmt
"$MEASURE" clippy "$SLUICE" run --hook pre-commit clippy
HOOK
    fi
    chmod +x "$1/pre-commit"
}

# agent <i> <hooks-dir>: the commits of one simulated agent.
agent() {
    wt="$TMP/wt-$1"
    cd "$wt"
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0="$2"
    mkdir -p bench
    k=1
    while [ "$k" -le "$COMMITS" ]; do
        if [ "$k" -eq 1 ]; then
            line="bench 1 $RUNID"
            echo "$line" > bench/notes.txt
        else
            line="bench $1 $k"
            echo "$line" >> bench/notes.txt
        fi
        # A real source change, so clippy has to recompile (a notes-only change
        # would make cargo a no-op and the gates meaningless).
        echo "// $line" >> crates/sluice-gate-core/src/lib.rs
        git add bench/notes.txt crates/sluice-gate-core/src/lib.rs
        git commit -q -m "bench: worktree $1 commit $k"
        k=$((k + 1))
    done
}

# run_scenario <name> <mode>: sets WALL_MS; log in $TMP/<name>.csv.
run_scenario() {
    name="$1"
    hooks="$TMP/hooks-$name"
    make_hook "$hooks" "$2"
    export SLUICE_VALIDATION_LOG="$TMP/$name.csv"
    rm -f "$SLUICE_VALIDATION_LOG"
    echo "== scenario $name ($2): $N worktrees x $COMMITS commits in parallel" >&2
    PIDS=""
    start=$(now_ms)
    i=1
    while [ "$i" -le "$N" ]; do
        ( agent "$i" "$hooks" ) >"$TMP/agent-$name-$i.log" 2>&1 &
        PIDS="$PIDS $!"
        i=$((i + 1))
    done
    failed=0
    for p in $PIDS; do wait "$p" || failed=1; done
    WALL_MS=$(( $(now_ms) - start ))
    PIDS=""
    if [ "$failed" -ne 0 ]; then
        echo "a commit failed in scenario $name:" >&2
        tail -n 30 "$TMP"/agent-"$name"-*.log >&2
        exit 1
    fi
}

reset_worktrees() {
    i=1
    while [ "$i" -le "$N" ]; do
        git -C "$TMP/wt-$i" reset -q --hard "$BASE"
        git -C "$TMP/wt-$i" clean -qfd bench
        i=$((i + 1))
    done
}

# csv_stats <csv>: "<runs> <mean seconds>" over all lines.
csv_stats() {
    awk -F, '{ n++; s += $3 } END { printf "%d %.1f", n, (n ? s / n / 1000 : 0) }' "$1"
}

# json_total <field>: a number from the "totals" object of `sluice status --json`.
json_total() {
    echo "$STATUS_JSON" | tr -d '\n ' | sed 's/.*"totals":{//; s/}.*//' |
        tr ',' '\n' | sed -n "s/^\"$1\":\([0-9]*\)\$/\1/p"
}

run_scenario A direct
A_WALL=$WALL_MS
reset_worktrees
run_scenario B sluice
B_WALL=$WALL_MS
STATUS_JSON=$("$SLUICE" status --json)

set -- $(csv_stats "$TMP/A.csv"); A_RUNS=$1; A_MEAN=$2
set -- $(csv_stats "$TMP/B.csv"); B_RUNS=$1; B_MEAN=$2
B_SUITE=$(json_total runs); B_HITS=$(json_total hits); B_COAL=$(json_total coalesced)
COMMITS_TOTAL=$((N * COMMITS))

per_commit() { awk -v w="$1" -v c="$2" 'BEGIN { printf "%.1f", w / c / 1000 }'; }
secs() { awk -v w="$1" 'BEGIN { printf "%.1f", w / 1000 }'; }

echo
echo "worktrees=$N commits_per_worktree=$COMMITS budget=8 load_at_start=$LOAD0"
echo
printf '%-22s %9s %11s %9s %6s %10s %12s %14s\n' scenario "wall(s)" "hook-runs" "suite-runs" hits coalesced "mean-hook(s)" "s/commit(wall)"
printf '%-22s %9s %11s %9s %6s %10s %12s %14s\n' "A without Sluice" "$(secs "$A_WALL")" "$A_RUNS" "$A_RUNS" "-" "-" "$A_MEAN" "$(per_commit "$A_WALL" "$COMMITS_TOTAL")"
printf '%-22s %9s %11s %9s %6s %10s %12s %14s\n' "B with Sluice" "$(secs "$B_WALL")" "$B_RUNS" "$B_SUITE" "$B_HITS" "$B_COAL" "$B_MEAN" "$(per_commit "$B_WALL" "$COMMITS_TOTAL")"
echo
echo "hook-runs: gate invocations from the hook log (fmt + clippy per commit)."
echo "suite-runs, hits, coalesced: totals of the private daemon (B)."
echo
echo "== validation-report A (log A)"
"$REPORT" "$TMP/A.csv"
echo
echo "== validation-report B (log B)"
"$REPORT" "$TMP/B.csv"
echo
echo "== sluice status --json (scenario B)"
echo "$STATUS_JSON"
