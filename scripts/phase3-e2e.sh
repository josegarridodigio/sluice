#!/bin/sh
# End-to-end run of the MCP server with real Claude Code sessions.
#
# Usage: scripts/phase3-e2e.sh [gate_seconds] [progress_interval]   (defaults 90 and 15s)
# SLUICE_E2E_MODEL picks the model of the agent sessions (default haiku).
#
# With progress_interval set above the idle window (for example 10m) the run
# shows what happens when no progress notification reaches Claude Code.
#
# Needs the `claude` CLI (logged in) and git. Everything happens in a
# temporary directory: a scratch clone of this repository with a slow
# `test` gate, two worktrees, a private HOME/XDG so the user's daemon is
# untouched. Two non-interactive Claude Code sessions call the `verify`
# tool at the same time (one should run the gate, the other coalesce) with
# the MCP idle window lowered to 60 s, so a gate longer than that only
# finishes if progress notifications keep the call alive. Then git hooks are
# exercised with CLAUDECODE=1 (cache hit on an unchanged tree, fail-fast on
# a new tree). Prints a summary; the raw session outputs stay in the temp
# dir (path printed) for the evidence note. Costs a few agent turns.
set -eu

GATE="${1:-90}"
PROGRESS="${2:-15s}"
# The sessions only call one tool and echo its result: the cheapest model is enough.
MODEL="${SLUICE_E2E_MODEL:-haiku}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -q --release -p sluice-gate --manifest-path "$ROOT/Cargo.toml"
SLUICE="$ROOT/target/release/sluice"

TMP="$(mktemp -d)"
echo "work dir: $TMP"
export HOME="$TMP/home" XDG_RUNTIME_DIR="$TMP/home/runtime" XDG_STATE_HOME="$TMP/home/state" XDG_CONFIG_HOME="$TMP/home/config"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
# Claude Code keeps its own state under HOME; reuse the real login.
ln -s "/home/jose/.claude" "$HOME/.claude" 2>/dev/null || true
cp /home/jose/.claude.json "$HOME/.claude.json" 2>/dev/null || true
COUNTER="$TMP/counter"; : > "$COUNTER"
mkdir -p "$XDG_CONFIG_HOME/sluice"
printf 'version = 1\n[mcp]\nprogress_interval = "%s"\n' "$PROGRESS" > "$XDG_CONFIG_HOME/sluice/config.toml"
trap '"$SLUICE" daemon stop --force >/dev/null 2>&1 || true' EXIT INT TERM

# Scratch repository with a slow gate and plain hooks.
git clone -q "$ROOT" "$TMP/repo"
cd "$TMP/repo"
git config user.name e2e; git config user.email e2e@example.invalid; git config commit.gpgsign false
cat > .sluice.toml <<EOF
version = 1

[gates.test]
cmd = "echo run >> '$COUNTER'; sleep $GATE"
weight = 2
timeout = "10m"
hooks = ["pre-commit"]
EOF
mkdir -p .githooks
printf '#!/bin/sh\nexec "%s" run --hook pre-commit test\n' "$SLUICE" > .githooks/pre-commit
chmod +x .githooks/pre-commit
git config core.hooksPath .githooks
git add .sluice.toml .githooks && git -c core.hooksPath=/dev/null commit -q -m "e2e gate"
git worktree add -q "$TMP/wt" -b e2e-wt
git -C "$TMP/wt" config core.hooksPath .githooks

# The same unstaged change in both worktrees: identical trees, one run expected.
printf '\ne2e\n' >> "$TMP/repo/README.md"
printf '\ne2e\n' >> "$TMP/wt/README.md"

cat > "$TMP/mcp.json" <<EOF
{"mcpServers":{"sluice":{"command":"$SLUICE","args":["mcp"],
 "env":{"HOME":"$HOME","XDG_RUNTIME_DIR":"$XDG_RUNTIME_DIR","XDG_STATE_HOME":"$XDG_STATE_HOME","XDG_CONFIG_HOME":"$XDG_CONFIG_HOME","PATH":"$PATH"}}}}
EOF

agent() { # agent <cwd> <label>
    dir="$1"; label="$2"
    prompt="Call the sluice MCP tool 'verify' with working_directory=\"$dir\" and gates=[\"test\"]. Wait for it. Then reply with the exact result text the tool returned, nothing else."
    start=$(date +%s)
    ( cd "$dir" && env -u CLAUDECODE -u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_CODE_SESSION_ID \
        CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000 \
        claude -p "$prompt" --model "$MODEL" --max-turns 3 --output-format json \
          --mcp-config "$TMP/mcp.json" --strict-mcp-config \
          --allowedTools mcp__sluice__verify --permission-mode default \
          > "$TMP/$label.json" 2> "$TMP/$label.err" ) || true
    end=$(date +%s)
    echo "$label: $((end - start)) s"
}

echo "== step 2-3: two agents verify the same tree concurrently (gate $GATE s, idle window 60 s, progress every $PROGRESS)"
agent "$TMP/repo" agentA &
sleep 5
agent "$TMP/wt" agentB &
sleep 20
echo "-- status during the run:"; "$SLUICE" status 2>&1 | sed 's/^/   /' | head -12
wait
for a in agentA agentB; do
    printf '%s result: ' "$a"
    python3 -I -c "import json,sys; d=json.load(open(sys.argv[1])); print((d.get('result') or '').strip()[:300].replace('\n',' | '))" "$TMP/$a.json" 2>/dev/null || head -c 300 "$TMP/$a.json"
done
echo "gate runs: $(wc -l < "$COUNTER") (expected 1)"

echo "== step 5: agent-invoked pre-commit on the verified tree (expects a cache hit)"
( cd "$TMP/repo" && git add README.md && CLAUDECODE=1 git commit -q -m "e2e commit" && echo "commit ok" ) 2>&1 | sed 's/^/   /'
echo "gate runs: $(wc -l < "$COUNTER") (expected 1)"

echo "== step 6: agent-invoked pre-commit on a new tree (expects fail-fast, exit 3)"
( cd "$TMP/wt" && echo new >> README.md && git add README.md && CLAUDECODE=1 git commit -q -m "e2e new"; echo "exit=$?" ) 2>&1 | sed 's/^/   /'
echo "gate runs: $(wc -l < "$COUNTER") (expected 1)"

echo "== totals"; "$SLUICE" status --json 2>/dev/null | python3 -I -c "import json,sys; print(json.load(sys.stdin).get('totals'))" || true
echo "raw outputs: $TMP/agentA.json $TMP/agentB.json (stderr in *.err)"
