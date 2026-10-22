# Sluice plugin for Claude Code

Makes Claude Code verify through [Sluice](../README.md) instead of running
test suites in its own shell: one queue and one cache shared by every agent on
the machine, one run per git tree.

## Install

Prerequisite: the `sluice` binary in the `PATH` that Claude Code sees.

```sh
curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh
```

Then, inside Claude Code:

```
/plugin marketplace add josegarridodigio/sluice
/plugin install sluice@sluice
```

or from a shell: `claude plugin marketplace add josegarridodigio/sluice` and
`claude plugin install sluice@sluice`. On Claude Code 2.1.275 or later,
`/plugin install sluice --marketplace josegarridodigio/sluice` does both in one
step. Run `/reload-plugins` (or restart) and check `claude mcp list`: the
`plugin:sluice:sluice` server should be connected.

The repository must have a `.sluice.toml` (`sluice init`). Nothing else has to
be added to its `CLAUDE.md`.

## What is inside

| Piece | File | What it does |
|---|---|---|
| MCP server | `.mcp.json` | Registers `sluice mcp`. Tools: `verify`, `status`, `gate_list`, `logs`, `queue_enqueue`, `queue_status`, `queue_approve`, `queue_retry`, `queue_cancel`. From a plugin they are named `mcp__plugin_sluice_sluice__<tool>`; use that form in permission rules. |
| Skill | `skills/sluice/SKILL.md` (+ `reference.md`) | Loaded when the agent is about to run tests, linters or type checks, commit, push or land a branch in a repository with a `.sluice.toml`. Teaches `verify`, reading its result, exit code 3 from hooks, and the merge-queue flow (verify, `queue_enqueue`, stop, never approve). |
| Guard hook | `hooks/hooks.json`, `hooks/guard.sh` | `PreToolUse` on `Bash`. Denies, with a reason the model reads, a command that runs a gate of `.sluice.toml` (or a raw runner the repository defines a gate for) and `sluice queue approve`. |

### The guard hook

For a Bash command inside a repository with a `.sluice.toml`, the hook splits
the command on `&&`, `||`, `;`, `|`, `&` and newlines (quotes are not parsed, so
it errs on the side of denying), and compares each piece, after normalising
whitespace and skipping leading `VAR=value` words, with:

- the `cmd = "..."` of every gate (one-line commands): the piece is denied if it
  equals the gate command or starts with it plus a space;
- these raw runners, only when some gate's command starts with the same
  runner: `cargo test`, `cargo clippy`, `npm test`, `npm run test`, `pnpm test`,
  `yarn test`, `pytest`, `go test`.

A match is denied with: "This repository verifies through Sluice: call the
`verify` MCP tool with gates=["<name>"] (gate '<name>' runs this command). Raw
runs bypass the shared queue and cache." Any piece that starts with
`sluice queue approve` is denied with "Approval is reserved to humans."
Everything else is allowed silently.

Behaviour and limits:

- It needs only `sh`, `sed`, `tr` and `head`. It uses `jq` when installed,
  otherwise a small `sed` fallback to read `tool_input.command` and `cwd`. The
  command text is only pattern-matched, never executed.
- It fails open: any internal error allows the command and prints nothing.
- `SLUICE_GUARD=0` in the environment of Claude Code disables it.
- It is a guardrail, not a security boundary: an agent can still build a
  command it does not recognise (`sh -c`, a wrapper script, a `Makefile`
  target). Multi-line `cmd = """..."""` gates are ignored. Pair it with
  `permissions.deny` rules and branch protection on the forge, see
  [docs/agents.md](../docs/agents.md).

## Test the hook

```sh
plugin/hooks/test-guard.sh
```

It feeds JSON fixtures to `guard.sh` (allowed command, gate command,
`cargo test -p x`, `cd a && cargo test`, `sluice queue approve t-1`, a
repository without `.sluice.toml`, malformed JSON, `SLUICE_GUARD=0`) and checks
the decision, once with `jq` and once with the `sed` fallback. It exits non-zero
on any failure.

## Validate and try locally

```sh
claude plugin validate .          # the marketplace (.claude-plugin/marketplace.json)
claude plugin validate ./plugin   # the plugin
claude --plugin-dir ./plugin      # load it for one session, without installing
scripts/plugin-e2e.sh             # real non-interactive sessions in a sandbox
```

`scripts/plugin-e2e.sh` uses the model in `SLUICE_E2E_MODEL` (default `haiku`)
and a scratch clone with a private HOME; it does not touch your Sluice daemon.
