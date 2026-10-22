#!/bin/sh
# Tests for guard.sh: feeds JSON fixtures and asserts the hook's decision.
# Usage: plugin/hooks/test-guard.sh   (needs only sh)
#
# Shell fallback passes (the 0.2.0 deny logic, never rewrites): with `sluice`
# removed from PATH (with jq when installed, and with the sed fallback) and
# with a fake pre-0.3 `sluice` that lacks `guard`. Rust pass: guard.sh with
# the built `sluice` (SLUICE_BIN, else target/debug or target/release of this
# checkout) in front of PATH, which rewrites gate and heavy commands; skipped
# with a notice when no binary is found.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
GUARD="$HERE/guard.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/repo/sub/dir" "$TMP/plain" "$TMP/home" "$TMP/oldbin" "$TMP/newbin"
cat > "$TMP/repo/.sluice.toml" <<'TOML'
version = 1

[gates.test]
cmd = "cargo test --workspace"
hooks = ["pre-commit"]

[gates.lint]
cmd = "cargo   clippy --workspace -- -D warnings"  # comment

[gates."web"]
cmd = 'npm run  check'
TOML

# PATH without any directory that holds a `sluice`. The trailing newline
# matters: `read` drops a last line without one, and on Alpine that line is
# `/bin`, the only directory with `sh`.
NOSLUICE_PATH=$(printf '%s\n' "$PATH" | tr ':' '\n' | while IFS= read -r d; do
    [ -n "$d" ] && [ ! -x "$d/sluice" ] && printf '%s:' "$d"
done)
NOSLUICE_PATH=${NOSLUICE_PATH%:}

# A pre-0.3 sluice: no `guard` subcommand (clap's usage error, exit 2).
cat > "$TMP/oldbin/sluice" <<'SH'
#!/bin/sh
echo "error: unrecognized subcommand '$1'" >&2
exit 2
SH
chmod +x "$TMP/oldbin/sluice"

fail=0
total=0

# check <label> <expected: allow|deny> <json> [env assignments passed to env]
check() {
    label="$1"; want="$2"; json="$3"; shift 3
    total=$((total + 1))
    out=$(printf '%s' "$json" | env "$@" sh "$GUARD" 2>/dev/null)
    rc=$?
    case "$out" in
        *'"permissionDecision":"deny"'*) got=deny ;;
        "") got=allow ;;
        *) got="garbage" ;;
    esac
    if [ "$got" = "$want" ] && [ "$rc" -eq 0 ]; then
        printf 'ok   %-44s %s\n' "$label" "$got"
    else
        printf 'FAIL %-44s want %s got %s (rc=%s) %s\n' "$label" "$want" "$got" "$rc" "$out"
        fail=$((fail + 1))
    fi
}

# The 0.2.0 fixtures; the shell fallback must keep these results.
run_all() {
    R="$TMP/repo"
    P="$TMP/plain"
    check "allowed command (ls)"            allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"ls -la\"}}" "$@"
    check "gate command verbatim"           deny  "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"cargo test --workspace\"}}" "$@"
    check "gate command, extra spaces"      deny  "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"cargo  clippy   --workspace -- -D warnings\"}}" "$@"
    check "single-quoted gate cmd"          deny  "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"npm run check\"}}" "$@"
    check "cargo test -p x"                 deny  "{\"cwd\":\"$R/sub\",\"tool_input\":{\"command\":\"cargo test -p x\"}}" "$@"
    check "compound: cd a && cargo test"    deny  "{\"cwd\":\"$R/sub/dir\",\"tool_input\":{\"command\":\"cd a && cargo test\"}}" "$@"
    check "piped: cargo test | tail"        deny  "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"cargo test 2>&1 | tail -5\"}}" "$@"
    check "env prefix: RUST_LOG=x cargo test" deny "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"RUST_LOG=x cargo test\"}}" "$@"
    check "runner without a gate (npm test)" allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"npm test\"}}" "$@"
    check "cargo build is not a gate"       allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"cargo build\"}}" "$@"
    check "cargo testing (word boundary)"   allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"cargo testing\"}}" "$@"
    check "sluice queue approve t-1"        deny  "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"sluice queue approve t-1\"}}" "$@"
    check "approve in a repo without config" deny "{\"cwd\":\"$P\",\"tool_input\":{\"command\":\"sluice  queue   approve t-1\"}}" "$@"
    check "sluice queue status is fine"     allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"sluice queue status\"}}" "$@"
    check "repo without .sluice.toml"       allow "{\"cwd\":\"$P\",\"tool_input\":{\"command\":\"cargo test\"}}" "$@"
    check "escaped quotes in the command"   allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"echo \\\"hi\\\" > x\"}}" "$@"
    check "malformed JSON"                  allow "{not json at all" "$@"
    check "empty input"                     allow "" "$@"
    check "missing cwd falls back to pwd"   allow "{\"tool_input\":{\"command\":\"ls\"}}" "$@"
    check "SLUICE_GUARD=0 (gate command)"   allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"cargo test\"}}" SLUICE_GUARD=0 "$@"
    check "SLUICE_GUARD=0 (approve)"        allow "{\"cwd\":\"$R\",\"tool_input\":{\"command\":\"sluice queue approve t-1\"}}" SLUICE_GUARD=0 "$@"
}

# check_rust <label> <want: silent|deny|allow:<cmd>|ask:<cmd>> <cwd> <command> <mode> [env]
# The command must not contain characters JSON would escape.
check_rust() {
    label="$1"; want="$2"; cwd="$3"; cmd="$4"; mode="$5"; shift 5
    total=$((total + 1))
    json="{\"cwd\":\"$cwd\",\"permission_mode\":\"$mode\",\"tool_input\":{\"command\":\"$cmd\",\"description\":\"d\",\"timeout\":1000}}"
    out=$(printf '%s' "$json" | env -u CLAUDE_CONFIG_DIR -u SLUICE_GUARD PATH="$TMP/newbin:$NOSLUICE_PATH" HOME="$TMP/home" \
        CLAUDE_PROJECT_DIR="$TMP/repo" SLUICE_CONFIG="$TMP/home/sluice.toml" "$@" sh "$GUARD" 2>/dev/null)
    rc=$?
    case "$out" in
        "") got=silent ;;
        *'"permissionDecision":"deny"'*) got=deny ;;
        *'"permissionDecision":"allow"'*|*'"permissionDecision":"ask"'*)
            decision=$(printf '%s' "$out" | sed -nE 's/.*"permissionDecision":"([a-z]+)".*/\1/p')
            command=$(printf '%s' "$out" | sed -nE 's/.*"updatedInput":\{"command":"([^"]*)".*/\1/p')
            got="$decision:$command"
            case "$out" in
                *'"description":"d","timeout":1000}'*) ;;
                *) got="$got (tool_input not echoed)" ;;
            esac
            ;;
        *) got=garbage ;;
    esac
    if [ "$got" = "$want" ] && [ "$rc" -eq 0 ]; then
        printf 'ok   %-44s %s\n' "$label" "$got"
    else
        printf 'FAIL %-44s want %s got %s (rc=%s) %s\n' "$label" "$want" "$got" "$rc" "$out"
        fail=$((fail + 1))
    fi
}

run_rust() {
    R="$TMP/repo"
    P="$TMP/plain"
    D=default
    check_rust "allowed command (ls)"          silent "$R" "ls -la" $D
    check_rust "gate verbatim"                 "ask:sluice run test -v" "$R" "cargo test --workspace" $D
    check_rust "gate with extra"               "ask:sluice run test -v -- -- --nocapture" "$R" "cargo test --workspace -- --nocapture" $D
    check_rust "gate, extra spaces"            "ask:sluice run lint -v" "$R" "cargo  clippy   --workspace -- -D warnings" $D
    check_rust "single-quoted gate cmd"        "ask:sluice run web -v" "$R" "npm run check" $D
    check_rust "runner-only match -> exec"     "ask:sluice exec --label cargo-test -- cargo test -p x" "$R/sub" "cargo test -p x" $D
    check_rust "cd to a missing dir: deny"     deny "$R/sub/dir" "cd a && cargo test" $D
    check_rust "first in a pipeline -> exec"   "ask:sluice exec --label cargo-test -- cargo test 2>&1 | tail -5" "$R" "cargo test 2>&1 | tail -5" $D
    check_rust "env prefix -> exec"            "ask:RUST_LOG=x sluice exec --label cargo-test -- cargo test" "$R" "RUST_LOG=x cargo test" $D
    check_rust "npm test (no gate, not heavy)" silent "$R" "npm test" $D
    check_rust "cargo build -> exec"           "ask:sluice exec --label cargo-build -- cargo build" "$R" "cargo build" $D
    check_rust "cargo testing"                 silent "$R" "cargo testing" $D
    check_rust "approve"                       deny "$R" "sluice queue approve t-1" $D
    check_rust "approve without config"        deny "$P" "sluice  queue   approve t-1" $D
    check_rust "queue status"                  silent "$R" "sluice queue status" $D
    check_rust "no .sluice.toml -> exec"       "ask:sluice exec --label cargo-test -- cargo test" "$P" "cargo test" $D
    check_rust "bypass mode -> allow"          "allow:sluice exec --label make -- make" "$P" "make" bypassPermissions
    check_rust "cd sub && pnpm build"          "allow:cd sub && sluice exec --label run-build -- pnpm build" "$R" "cd sub && pnpm build" bypassPermissions
    check_rust "plan mode, heavy: nothing"     silent "$P" "make" plan
    check_rust "plan mode, gate: deny"         deny "$R" "cargo test --workspace" plan
    check_rust "downstream of a pipe"          silent "$P" "cat x | make" $D
    check_rust "background &"                  silent "$P" "make &" $D
    check_rust "sh -c"                         silent "$P" "sh -c make" $D
    check_rust "sluice exec already"           silent "$P" "sluice exec -- make" $D
    check_rust "SLUICE_GUARD=deny, heavy"      deny "$P" "make" $D SLUICE_GUARD=deny
    check_rust "SLUICE_GUARD=0, gate"          silent "$R" "cargo test --workspace" $D SLUICE_GUARD=0
    check_rust "SLUICE_GUARD=0, approve"       silent "$R" "sluice queue approve t-1" $D SLUICE_GUARD=0
    for bad in "{not json at all" ""; do
        total=$((total + 1))
        out=$(printf '%s' "$bad" | env PATH="$TMP/newbin:$NOSLUICE_PATH" sh "$GUARD" 2>/dev/null)
        if [ -n "$out" ]; then
            echo "FAIL malformed input printed $out"; fail=$((fail + 1))
        else
            printf 'ok   %-44s %s\n' "malformed input ($(printf '%s' "$bad" | wc -c) bytes)" silent
        fi
    done
}

echo "== shell fallback, sluice not in PATH"
if command -v jq >/dev/null 2>&1; then
    echo "-- with jq"
    run_all PATH="$NOSLUICE_PATH"
else
    echo "-- jq not installed: skipping the jq pass"
fi
echo "-- sed fallback (SLUICE_GUARD_NOJQ=1)"
run_all PATH="$NOSLUICE_PATH" SLUICE_GUARD_NOJQ=1
echo "== shell fallback, pre-0.3 sluice without guard"
run_all PATH="$TMP/oldbin:$NOSLUICE_PATH"

# Reason text the model sees.
reason=$(printf '%s' "{\"cwd\":\"$TMP/repo\",\"tool_input\":{\"command\":\"cargo test -p x\"}}" | env PATH="$NOSLUICE_PATH" sh "$GUARD")
echo "-- sample deny: $reason"
case "$reason" in
    *'gates=[\"test\"]'*"Raw runs bypass the shared queue and cache."*) ;;
    *) echo "FAIL reason text"; fail=$((fail + 1)) ;;
esac

BIN="${SLUICE_BIN:-}"
if [ -z "$BIN" ]; then
    for candidate in "$ROOT/target/debug/sluice" "$ROOT/target/release/sluice"; do
        if [ -x "$candidate" ]; then BIN="$candidate"; break; fi
    done
fi
if [ -n "$BIN" ] && "$BIN" guard --hook </dev/null >/dev/null 2>&1; then
    ln -s "$BIN" "$TMP/newbin/sluice"
    echo "== Rust guard ($BIN)"
    run_rust
else
    echo "== Rust guard skipped: no sluice with 'guard' (set SLUICE_BIN or cargo build)"
fi

echo "== $total checks, $fail failure(s)"
[ "$fail" -eq 0 ]
