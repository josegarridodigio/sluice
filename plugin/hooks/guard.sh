#!/bin/sh
# Sluice guard: PreToolUse hook for the Bash tool.
#
# Reads the hook JSON on stdin. Denies (with a reason the model sees):
#   - a command that runs a gate of the repository's .sluice.toml (or a raw
#     test runner the repository defines a gate for) outside the Sluice queue;
#   - `sluice queue approve`, which is reserved to humans.
# Allows everything else silently. SLUICE_GUARD=0 disables it. Any internal
# problem allows the command and prints nothing (fail open). The command text
# is only ever matched against patterns, never executed.
#
# Matching is deliberately simple: the command is split on && || ; | & and
# newlines (quotes are ignored), and each piece is compared by prefix.

[ "${SLUICE_GUARD:-1}" = "0" ] && exit 0

TAB=$(printf '\t')

# Extract a JSON string field. $1 = field name, stdin = JSON.
# Uses jq when present (SLUICE_GUARD_NOJQ=1 forces the fallback, for tests).
json_field() {
    if [ -z "${SLUICE_GUARD_NOJQ:-}" ] && command -v jq >/dev/null 2>&1; then
        case "$1" in
            command) jq -r '.tool_input.command // empty' 2>/dev/null ;;
            *) jq -r ".$1 // empty" 2>/dev/null ;;
        esac
    else
        sed -nE 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"(([^"\\]|\\.)*)".*/\1/p' |
            sed -e 's/\\n/ /g' -e 's/\\t/ /g' -e 's/\\"/"/g' -e 's/\\\\/\\/g' | head -n 1
    fi
}

# Collapse whitespace runs and trim.
norm() { printf '%s' "$1" | tr '\n\t' '  ' | sed -E 's/[[:space:]]+/ /g; s/^ //; s/ $//'; }

# Print the path of the .sluice.toml in $1 or an ancestor.
find_config() {
    d="$1"
    while [ -n "$d" ]; do
        if [ -f "$d/.sluice.toml" ]; then
            printf '%s\n' "$d/.sluice.toml"
            return 0
        fi
        [ "$d" = "/" ] && break
        d=$(dirname "$d")
    done
    return 1
}

# Print "<name><TAB><normalised cmd>" for every gate with a one-line cmd.
gate_cmds() {
    name=""
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
            "["*)
                name=$(printf '%s' "$line" | sed -nE 's/^\[gates\.["]?([A-Za-z0-9_.-]+)["]?\][[:space:]]*(#.*)?$/\1/p')
                ;;
            *)
                [ -n "$name" ] || continue
                c=$(printf '%s' "$line" | sed -nE 's/^[[:space:]]*cmd[[:space:]]*=[[:space:]]*"(([^"\\]|\\.)*)"[[:space:]]*(#.*)?$/\1/p')
                [ -n "$c" ] || c=$(printf '%s' "$line" | sed -nE "s/^[[:space:]]*cmd[[:space:]]*=[[:space:]]*'([^']*)'[[:space:]]*(#.*)?\$/\\1/p")
                [ -n "$c" ] && printf '%s\t%s\n' "$name" "$(norm "$c")"
                ;;
        esac
    done
}

# Strip leading "(", "{" and VAR=value assignments from a simple command.
strip_prefix() {
    s="$1"
    while :; do
        case "$s" in
            "("*|"{"*) s=$(norm "${s#?}") ;;
            [A-Za-z_]*=*)
                case "${s%% *}" in
                    *[!A-Za-z0-9_=]*) break ;;
                esac
                case "$s" in
                    *" "*) s="${s#* }" ;;
                    *) s="" ;;
                esac
                ;;
            *) break ;;
        esac
    done
    printf '%s' "$s"
}

# True when $1 equals $2 or starts with "$2 ".
has_prefix() {
    [ "$1" = "$2" ] && return 0
    case "$1" in
        "$2 "*) return 0 ;;
    esac
    return 1
}

# Name of the first gate whose command satisfies has_prefix; $1 = segment
# (mode exact) or the runner (mode runner).
first_gate() {
    printf '%s\n' "$gates" | while IFS="$TAB" read -r gname gcmd; do
        [ -n "$gcmd" ] || continue
        if [ "$1" = exact ]; then
            has_prefix "$2" "$gcmd" || continue
        else
            has_prefix "$gcmd" "$2" || continue
        fi
        printf '%s\n' "$gname"
        break
    done
}

deny() {
    reason=$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$reason"
}

# Decide for one simple command; print a deny JSON or nothing.
check_segment() {
    seg=$(strip_prefix "$(norm "$1")")
    [ -n "$seg" ] || return 0
    if has_prefix "$seg" "sluice queue approve"; then
        deny "Approval is reserved to humans."
        return 0
    fi
    [ -n "$gates" ] || return 0
    hit=$(first_gate exact "$seg")
    if [ -z "$hit" ]; then
        for runner in "cargo test" "cargo clippy" "npm test" "npm run test" "pnpm test" "yarn test" "pytest" "go test"; do
            has_prefix "$seg" "$runner" || continue
            hit=$(first_gate runner "$runner")
            [ -n "$hit" ] && break
        done
    fi
    if [ -n "$hit" ]; then
        hit=$(printf '%s' "$hit" | tr -cd 'A-Za-z0-9_.-')
        deny "This repository verifies through Sluice: call the \`verify\` MCP tool with gates=[\"$hit\"] (gate '$hit' runs this command). Raw runs bypass the shared queue and cache."
    fi
}

main() {
    input=$(cat)
    command=$(printf '%s' "$input" | json_field command)
    [ -n "$command" ] || return 0
    cwd=$(printf '%s' "$input" | json_field cwd)
    [ -n "$cwd" ] || cwd=$(pwd)

    config=$(find_config "$cwd") || config=""
    gates=""
    [ -n "$config" ] && gates=$(gate_cmds < "$config")

    # Split on && || ; | & and newlines; stop at the first denial.
    segments=$(printf '%s' "$command" | tr '\n' ';' | sed -E 's/(&&|\|\||;|\||&)/\
/g')
    verdict=""
    OLDIFS=$IFS
    IFS='
'
    for s in $segments; do
        IFS=$OLDIFS
        verdict=$(check_segment "$s")
        [ -n "$verdict" ] && break
        IFS='
'
    done
    IFS=$OLDIFS
    [ -n "$verdict" ] && printf '%s\n' "$verdict"
    return 0
}

out=$(main 2>/dev/null) || out=""
case "$out" in
    '{"hookSpecificOutput"'*) printf '%s\n' "$out" ;;
esac
exit 0
