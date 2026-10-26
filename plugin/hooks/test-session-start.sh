#!/bin/sh
# Tests for session-start.sh (the SessionStart wrapper), with fake `sluice`
# binaries: missing binary, older binary, equal or newer binary (hands over),
# a broken binary (exit 1, garbage), the ~/.local/bin fallback. With a real
# binary (SLUICE_BIN, else target/debug or target/release of this checkout) it
# also checks the end-to-end answer; skipped with a notice when none is found.
# Usage: plugin/hooks/test-session-start.sh   (needs only sh)
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# A plugin layout of its own, so the plugin version is under the test's control.
mkdir -p "$TMP/plugin/hooks" "$TMP/plugin/.claude-plugin" "$TMP/home/.local/bin" "$TMP/fake"
cp "$HERE/session-start.sh" "$TMP/plugin/hooks/session-start.sh"
cat > "$TMP/plugin/.claude-plugin/plugin.json" <<'JSON'
{
  "name": "sluice",
  "description": "test",
  "version": "0.3.0"
}
JSON
HOOK="$TMP/plugin/hooks/session-start.sh"

# PATH without any directory that holds a `sluice` (trailing newline matters).
NOSLUICE_PATH=$(printf '%s\n' "$PATH" | tr ':' '\n' | while IFS= read -r d; do
    [ -n "$d" ] && [ ! -x "$d/sluice" ] && printf '%s:' "$d"
done)
NOSLUICE_PATH=${NOSLUICE_PATH%:}

# fake_sluice <dir> <version line> <body of `agent session-start`>
fake_sluice() {
    mkdir -p "$1"
    cat > "$1/sluice" <<SH
#!/bin/sh
case "\$1" in
    --version) echo "$2"; exit 0 ;;
    agent)
        printf '%s\n' "\$*" > "$TMP/args"
        cat > "$TMP/stdin"
        $3
        ;;
esac
exit 0
SH
    chmod +x "$1/sluice"
}

fail=0
total=0

# run_hook <path> [stdin]: prints the hook's stdout; sets RC.
run_hook() {
    out=$(printf '%s' "${2:-{\}}" | env PATH="$1" HOME="$TMP/home" sh "$HOOK" 2>/dev/null)
    RC=$?
}

# expect <label> <substring or "-" for empty output>
expect() {
    total=$((total + 1))
    ok=1
    [ "$RC" -eq 0 ] || ok=0
    if [ "$2" = "-" ]; then
        [ -z "$out" ] || ok=0
    else
        case "$out" in *"$2"*) ;; *) ok=0 ;; esac
        case "$out" in '{"hookSpecificOutput":{'*'"hookEventName":"SessionStart"'*'}}') ;; *) ok=0 ;; esac
    fi
    if [ "$ok" -eq 1 ]; then
        printf 'ok   %s\n' "$1"
    else
        printf 'FAIL %s (rc=%s) %s\n' "$1" "$RC" "$out"
        fail=$((fail + 1))
    fi
}

echo "== missing binary"
run_hook "$NOSLUICE_PATH"
expect "no sluice anywhere: consent text" "binary is not installed"
case "$out" in *"scripts/install.sh | sh"*"Only if the user agrees"*|*"Only if the user agrees"*"scripts/install.sh | sh"*) ;; *) echo "FAIL install command"; fail=$((fail + 1)) ;; esac

echo "== older binary"
fake_sluice "$TMP/fake/old" "sluice 0.0.1" 'echo SHOULD-NOT-RUN'
run_hook "$TMP/fake/old:$NOSLUICE_PATH"
expect "0.0.1 < 0.3.0: update consent text" "older (0.0.1)"
case "$out" in *"sluice update"*) ;; *) echo "FAIL update command"; fail=$((fail + 1)) ;; esac
case "$out" in *SHOULD-NOT-RUN*) echo "FAIL ran the old binary"; fail=$((fail + 1)) ;; esac
fake_sluice "$TMP/fake/old2" "sluice 0.2.9" 'echo SHOULD-NOT-RUN'
run_hook "$TMP/fake/old2:$NOSLUICE_PATH"
expect "0.2.9 < 0.3.0" "older (0.2.9)"

echo "== equal or newer binary hands over"
for v in 0.3.0 0.3.1 0.10.0 1.0.0; do
    fake_sluice "$TMP/fake/new" "sluice $v" 'echo "{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"FAKE-OK\"}}"'
    rm -f "$TMP/args" "$TMP/stdin"
    run_hook "$TMP/fake/new:$NOSLUICE_PATH" '{"cwd":"/x","source":"startup"}'
    expect "sluice $v runs session-start" "FAKE-OK"
    if [ "$(cat "$TMP/args" 2>/dev/null)" != "agent session-start --plugin-version 0.3.0" ]; then
        echo "FAIL arguments: $(cat "$TMP/args" 2>/dev/null)"; fail=$((fail + 1))
    fi
    if [ "$(cat "$TMP/stdin" 2>/dev/null)" != '{"cwd":"/x","source":"startup"}' ]; then
        echo "FAIL stdin: $(cat "$TMP/stdin" 2>/dev/null)"; fail=$((fail + 1))
    fi
done

echo "== fallback locations"
rm -rf "$TMP/fake/new"
fake_sluice "$TMP/home/.local/bin" "sluice 0.3.0" 'echo "{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"FROM-LOCAL-BIN\"}}"'
run_hook "$NOSLUICE_PATH"
expect "found in ~/.local/bin" "FROM-LOCAL-BIN"

echo "== broken binary never blocks"
rm -f "$TMP/home/.local/bin/sluice"
fake_sluice "$TMP/fake/bad" "sluice 0.3.0" 'echo garbage; exit 1'
run_hook "$TMP/fake/bad:$NOSLUICE_PATH"
expect "exit 1 with garbage: no output, exit 0" "-"
fake_sluice "$TMP/fake/junk" "sluice 0.3.0" 'echo "not a hook answer"'
run_hook "$TMP/fake/junk:$NOSLUICE_PATH"
expect "garbage on stdout is not forwarded" "-"
fake_sluice "$TMP/fake/nover" "no version here" 'echo "{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"NOVER-OK\"}}"'
run_hook "$TMP/fake/nover:$NOSLUICE_PATH"
expect "unparseable version still hands over" "NOVER-OK"

BIN="${SLUICE_BIN:-}"
if [ -z "$BIN" ]; then
    for candidate in "$ROOT/target/debug/sluice" "$ROOT/target/release/sluice"; do
        if [ -x "$candidate" ]; then BIN="$candidate"; break; fi
    done
fi
if [ -n "$BIN" ] && "$BIN" agent session-start --help >/dev/null 2>&1; then
    echo "== real binary ($BIN)"
    # A private, empty runtime directory: without it the real binary would
    # find the developer's systemd user manager (/run/user/UID) and create or
    # touch real cgroups and holder processes. There is no manager here, so
    # containment is off and nothing outside this script's TMP is touched.
    mkdir -p "$TMP/real/bin" "$TMP/real/dir" "$TMP/real/run"
    ln -s "$BIN" "$TMP/real/bin/sluice"
    out=$(printf '{"cwd":"%s","source":"startup"}' "$TMP/real/dir" |
        env -i PATH="$TMP/real/bin:$NOSLUICE_PATH" HOME="$TMP/real/home" XDG_STATE_HOME="$TMP/real/state" XDG_RUNTIME_DIR="$TMP/real/run" \
            ${SLUICE_TEST_SESSION_WATCHDOG_MS:+SLUICE_TEST_SESSION_WATCHDOG_MS=$SLUICE_TEST_SESSION_WATCHDOG_MS} \
            CLAUDE_ENV_FILE="$TMP/real/env.sh" sh "$HOOK" 2>/dev/null)
    RC=$?
    expect "end to end outside git" "not a git repository"
    if [ ! -f "$TMP/real/home/.local/share/sluice/shims/npx" ] || ! grep -q 'export PATH=' "$TMP/real/env.sh"; then
        echo "FAIL shims or env line missing"; fail=$((fail + 1))
    fi
else
    echo "== real binary pass skipped: no sluice with 'agent session-start' (set SLUICE_BIN or cargo build)"
fi

echo "== $total checks, $fail failure(s)"
[ "$fail" -eq 0 ]
