#!/bin/sh
# Benchmark for the README chart: N agents committing in parallel, with and
# without Sluice, on this repository.
#
# Usage: scripts/bench-agents.sh <out.csv> [agent counts...]   (default: 1 2 4 8)
#
# For every agent count N, N git worktrees of this repository (each with its
# own `target/`, as agents' worktrees have) make two commits each, all
# worktrees in parallel. Every commit runs the repository's pre-commit gates
# (fmt, clippy). The first commit has the same content in every worktree (all
# agents start from the same tree); the second differs per worktree. Both
# commits change a source file, so clippy really recompiles.
#   A) without Sluice: the hook runs the gate commands directly;
#   B) with Sluice:    the hook runs `sluice run --hook pre-commit <gate>`
#      (private daemon, default budget: the runner's CPUs).
# The order of A and B alternates between agent counts, so a warm page cache
# never favours the same scenario. Each scenario starts from the same warmed
# `target/` copy and a fresh daemon state.
#
# Appends one line per scenario to <out.csv>:
#   agents,scenario,wall_ms,gate_invocations,gate_runs,cpus,mem_gib
# `wall_ms` is the time until the last agent finished its last commit;
# `gate_runs` is what really ran (A: every invocation; B: the daemon's runs,
# without cache hits and coalesced requests).
#
# Isolation as in scripts/validation-bench.sh: private HOME and XDG_*
# directories, the daemon stopped and every worktree and branch removed on
# exit. Worktrees go under $SLUICE_BENCH_DIR (default: a temporary directory).
# Meant for a CI runner (`.github/workflows/bench.yml`): on a workstation the
# scenario without Sluice deliberately overloads the machine. Linux only.
set -eu

[ $# -ge 1 ] || { echo "usage: $0 <out.csv> [agent counts...]" >&2; exit 2; }
OUT=$(realpath -m "$1")
shift
[ $# -ge 1 ] || set -- 1 2 4 8
for n in "$@"; do
    case "$n" in *[!0-9]* | "" | 0) echo "not an agent count: $n" >&2; exit 2 ;; esac
done

ROOT=$(cd "$(dirname "$0")/.." && pwd)
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
unset CARGO_TARGET_DIR || true
export LC_ALL=C

CPUS=$(nproc)
MEM_GIB=$(awk '/^MemTotal:/ { printf "%.0f", $2 / 1048576 }' /proc/meminfo)
BASE=$(git -C "$ROOT" rev-parse HEAD)
RUNID="run-$$-$(date +%s)"
GATES="fmt clippy"

TMP=$(mktemp -d "${SLUICE_BENCH_DIR:-${TMPDIR:-/tmp}}/sluice-agents.XXXXXX")
export HOME="$TMP/home"
export XDG_RUNTIME_DIR="$TMP/run"
export XDG_CONFIG_HOME="$TMP/config"
export SLUICE_CONFIG="$XDG_CONFIG_HOME/sluice/config.toml"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/sluice"
chmod 700 "$XDG_RUNTIME_DIR"
printf 'version = 1\n' > "$SLUICE_CONFIG"
# Humans wait on a miss; an agent environment would fail fast and call
# `verify`, which waits the same way.
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

echo "== building the release binary" >&2
(cd "$ROOT" && cargo build -q --release -p sluice-gate >&2)

MAX=0
for n in "$@"; do [ "$n" -gt "$MAX" ] && MAX=$n; done

echo "== warming one target/ (fmt + clippy at the base commit)" >&2
WARM="$TMP/warm-target"
git -C "$ROOT" worktree add -q --detach "$TMP/warm" "$BASE" >&2
(cd "$TMP/warm" && CARGO_TARGET_DIR="$WARM" cargo clippy -q --workspace --all-targets -- -D warnings >&2)
git -C "$ROOT" worktree remove --force "$TMP/warm" >&2

echo "== creating $MAX worktrees" >&2
i=1
while [ "$i" -le "$MAX" ]; do
    git -C "$ROOT" worktree add -q "$TMP/wt-$i" -b "bench-$i" "$BASE" >&2
    CREATED=$i
    i=$((i + 1))
done

# make_hook <dir> <mode>: mode "direct" or "sluice".
make_hook() {
    mkdir -p "$1"
    {
        echo '#!/bin/sh'
        echo 'set -e'
        if [ "$2" = direct ]; then
            echo 'cargo fmt --all --check'
            echo 'cargo clippy -q --workspace --all-targets -- -D warnings'
        else
            for g in $GATES; do echo "\"$SLUICE\" run --hook pre-commit $g"; done
        fi
    } > "$1/pre-commit"
    chmod +x "$1/pre-commit"
}

# reset <n>: worktrees 1..n back at the base, each with a fresh copy of the
# warmed target/.
reset() {
    i=1
    while [ "$i" -le "$1" ]; do
        git -C "$TMP/wt-$i" reset -q --hard "$BASE"
        git -C "$TMP/wt-$i" clean -qfdx -e target
        rm -rf "$TMP/wt-$i/target"
        cp -a --reflink=auto "$WARM" "$TMP/wt-$i/target"
        i=$((i + 1))
    done
}

# agent <i> <hooks-dir>: the two commits of one simulated agent.
agent() {
    cd "$TMP/wt-$1"
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0="$2"
    # Commit 1: the same tree in every worktree.
    echo "// bench $RUNID" >> crates/sluice-gate-core/src/lib.rs
    git commit -qam "bench: shared change"
    # Commit 2: a change of this agent's own.
    echo "// bench $RUNID agent $1" >> crates/sluice-gate-core/src/lib.rs
    git commit -qam "bench: agent $1"
}

# scenario <n> <name> <mode>: one timed run; appends its CSV line.
scenario() {
    n=$1
    hooks="$TMP/hooks-$3"
    make_hook "$hooks" "$3"
    reset "$n"
    "$SLUICE" daemon stop --force >/dev/null 2>&1 || true
    rm -rf "$TMP/state"
    export XDG_STATE_HOME="$TMP/state"
    # Cache markers live in each clone's .git; a fresh run id per scenario
    # keeps an earlier scenario's markers from answering this one.
    RUNID="run-$$-$(date +%s%N)"
    echo "== $n agent(s), scenario $2 ($3)" >&2
    PIDS=""
    start=$(now_ms)
    i=1
    while [ "$i" -le "$n" ]; do
        ( agent "$i" "$hooks" ) >"$TMP/agent-$n-$2-$i.log" 2>&1 &
        PIDS="$PIDS $!"
        i=$((i + 1))
    done
    failed=0
    for p in $PIDS; do wait "$p" || failed=1; done
    wall=$(( $(now_ms) - start ))
    PIDS=""
    if [ "$failed" -ne 0 ]; then
        echo "a commit failed ($n agents, scenario $2):" >&2
        tail -n 30 "$TMP"/agent-"$n"-"$2"-*.log >&2
        exit 1
    fi
    invocations=$(( n * 2 * $(echo "$GATES" | wc -w) ))
    if [ "$3" = direct ]; then
        runs=$invocations
    else
        runs=$("$SLUICE" status --json | tr -d '\n ' |
            sed 's/.*"totals":{//; s/}.*//' | tr ',' '\n' | sed -n 's/^"runs":\([0-9]*\)$/\1/p')
    fi
    echo "$n,$2,$wall,$invocations,$runs,$CPUS,$MEM_GIB" >> "$OUT"
    echo "   wall $((wall / 1000)) s, $runs of $invocations gate invocations ran" >&2
}

[ -s "$OUT" ] || echo "agents,scenario,wall_ms,gate_invocations,gate_runs,cpus,mem_gib" > "$OUT"
flip=0
for n in "$@"; do
    if [ "$flip" -eq 0 ]; then
        scenario "$n" without direct
        scenario "$n" with sluice
        flip=1
    else
        scenario "$n" with sluice
        scenario "$n" without direct
        flip=0
    fi
done
echo "== done: $OUT" >&2
