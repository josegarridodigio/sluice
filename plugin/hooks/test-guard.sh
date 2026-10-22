#!/bin/sh
# Tests for guard.sh: feeds JSON fixtures and asserts allow/deny.
# Usage: plugin/hooks/test-guard.sh   (needs only sh; runs twice when jq is
# installed: with jq and with the sed fallback)
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
GUARD="$HERE/guard.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/repo/sub/dir" "$TMP/plain"
cat > "$TMP/repo/.sluice.toml" <<'EOF'
version = 1

[gates.test]
cmd = "cargo test --workspace"
hooks = ["pre-commit"]

[gates.lint]
cmd = "cargo   clippy --workspace -- -D warnings"  # comment

[gates."web"]
cmd = 'npm run  check'
EOF

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

if command -v jq >/dev/null 2>&1; then
    echo "== with jq"
    run_all SLUICE_GUARD_X=1
else
    echo "== jq not installed: skipping the jq pass"
fi
echo "== sed fallback (SLUICE_GUARD_NOJQ=1)"
run_all SLUICE_GUARD_NOJQ=1

# Reason text the model sees.
reason=$(printf '%s' "{\"cwd\":\"$TMP/repo\",\"tool_input\":{\"command\":\"cargo test -p x\"}}" | sh "$GUARD")
echo "-- sample deny: $reason"
case "$reason" in
    *'gates=[\"test\"]'*"Raw runs bypass the shared queue and cache."*) ;;
    *) echo "FAIL reason text"; fail=$((fail + 1)) ;;
esac

echo "== $total checks per pass, $fail failure(s)"
[ "$fail" -eq 0 ]
