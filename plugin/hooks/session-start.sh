#!/bin/sh
# Sluice session start: SessionStart hook (startup, resume, clear, compact).
#
# Thin wrapper. It finds the `sluice` binary (PATH, then ~/.local/bin and
# ~/.cargo/bin, the places the installer uses) and hands the hook over to
# `sluice agent session-start`, which installs the shims, puts them first in
# the agent's PATH through $CLAUDE_ENV_FILE and prints the rules as
# additionalContext.
#
# - No binary: prints additionalContext asking the agent to tell the user and,
#   only with the user's consent, run the installer. A hook cannot prompt and
#   never installs anything itself.
# - A binary older than this plugin: the same, with `sluice update`.
# - Otherwise: run `sluice agent session-start --plugin-version <v>` (the hook
#   JSON on stdin reaches it untouched) and print its answer when well formed.
#
# Always exits 0 (a broken Sluice never blocks a session).

HERE="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || exit 0
MANIFEST="$HERE/../.claude-plugin/plugin.json"
INSTALL='curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh'

# Print the additionalContext JSON for the text in $1 (no double quotes or
# backslashes in the texts below, so no escaping is needed).
emit() {
    printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$1"
}

# The first sluice binary found, or nothing.
find_sluice() {
    if command -v sluice >/dev/null 2>&1; then
        command -v sluice
        return 0
    fi
    for candidate in "$HOME/.local/bin/sluice" "$HOME/.cargo/bin/sluice"; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

# "X.Y.Z" found in $1 (a version line or the manifest), or nothing.
version_of() {
    printf '%s' "$1" | sed -nE 's/^[^0-9]*([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' | head -n 1
}

# True when version $1 is older than $2 (both X.Y.Z).
older() {
    a1=${1%%.*}; r=${1#*.}; a2=${r%%.*}; a3=${r#*.}
    b1=${2%%.*}; r=${2#*.}; b2=${r%%.*}; b3=${r#*.}
    [ "$a1" -lt "$b1" ] && return 0
    [ "$a1" -gt "$b1" ] && return 1
    [ "$a2" -lt "$b2" ] && return 0
    [ "$a2" -gt "$b2" ] && return 1
    [ "$a3" -lt "$b3" ]
}

plugin_version=""
if [ -r "$MANIFEST" ]; then
    plugin_version=$(sed -nE 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"([0-9]+\.[0-9]+\.[0-9]+)".*/\1/p' "$MANIFEST" | head -n 1)
fi

if ! bin=$(find_sluice) || [ -z "$bin" ]; then
    emit "The Sluice plugin is enabled but the sluice binary is not installed (it was not found in PATH, ~/.local/bin or ~/.cargo/bin), so its queue, shims and tools are inactive. Tell the user this at the start of your first reply. Only if the user agrees, install it by running: $INSTALL . Then ask the user to restart the session. Do not install anything without their consent."
    exit 0
fi

if [ -n "$plugin_version" ]; then
    installed=$(version_of "$("$bin" --version 2>/dev/null | head -n 1)")
    if [ -n "$installed" ] && older "$installed" "$plugin_version" 2>/dev/null; then
        emit "The Sluice plugin is version $plugin_version but the installed sluice binary is older ($installed), so some features are inactive. Tell the user this at the start of your first reply. Only if the user agrees, update it by running: sluice update . Then ask the user to restart the session. Do not update anything without their consent."
        exit 0
    fi
fi

# Hand over; print only a well-formed hook answer, whatever the binary does.
out=$("$bin" agent session-start --plugin-version "${plugin_version:-0.0.0}" 2>/dev/null) || exit 0
case "$out" in
    '{"hookSpecificOutput"'*) printf '%s\n' "$out" ;;
esac
exit 0
