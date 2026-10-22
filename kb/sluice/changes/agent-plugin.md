---
title: "Change: Claude Code plugin and marketplace"
tags: [sluice, change, agents, plugin]
---

# Change: Claude Code plugin (2026-10-22)

## What changed

The repository is now a Claude Code marketplace (`sluice`) that lists one
plugin (`sluice`, in `plugin/`). Installing it registers the MCP server,
loads a skill and adds a `PreToolUse` guard hook. `docs/agents.md` now
recommends the plugin and keeps the `CLAUDE.md`/`AGENTS.md` block as the
fallback for other agents. No Rust code changed.

## Files

- `.claude-plugin/marketplace.json`: marketplace `sluice`, owner Jose Antonio
  Garrido, one plugin with `"source": "./plugin"` (relative path from the
  marketplace root, the documented form).
- `plugin/.claude-plugin/plugin.json`: name `sluice`, version 0.1.0 (the
  workspace version), MIT, homepage and repository the GitHub URL.
- `plugin/.mcp.json`: server `sluice`, command `sluice`, args `["mcp"]` (needs
  `sluice` in PATH).
- `plugin/skills/sluice/SKILL.md` and `reference.md`: when to load (about to
  run tests/linters/type checks, commit, push or land a branch in a repository
  with `.sluice.toml`), verify flow, result reading, exit code 3, merge queue
  flow, missing-server instructions; the reference holds tool table, result
  format, queue states, troubleshooting.
- `plugin/hooks/hooks.json` and `guard.sh`: `PreToolUse`, matcher `Bash`,
  command `"${CLAUDE_PLUGIN_ROOT}"/hooks/guard.sh`. POSIX `sh`; `jq` if present,
  `sed` fallback otherwise (`SLUICE_GUARD_NOJQ=1` forces it, for tests). Denies
  gate commands (prefix match against each `cmd` of `.sluice.toml`) and the raw
  runners `cargo test`, `cargo clippy`, `npm test`, `npm run test`, `pnpm test`,
  `yarn test`, `pytest`, `go test` when a gate starts with the same runner, plus
  `sluice queue approve`. Deny is JSON on stdout with exit 0
  (`hookSpecificOutput.permissionDecision = "deny"` and
  `permissionDecisionReason`, which Claude reads). Fails open; `SLUICE_GUARD=0`
  disables it. Every segment of a compound command is checked (the brief said
  "first simple command", but `cd a && cargo test` must be denied).
- `plugin/hooks/test-guard.sh`: 21 fixtures, run with jq and with the sed
  fallback.
- `plugin/README.md`, `docs/agents.md` (rewritten sections 2, 3, 6, intro),
  `README.md` ("Use with coding agents"), `scripts/plugin-e2e.sh`.
- `kb/sluice/plan/master-plan.md`: first two items of "Agent integration
  package" ticked; the third stays open (see below).

## Why

One install per user instead of a block in every repository's `CLAUDE.md`;
the skill enters the context only when relevant; the hook is the only piece
that enforces the queue (it runs in `bypassPermissions` mode as well).

## Docs checked against

Claude Code 2.1.308 (`claude --version`), docs fetched 2026-10-22 from
code.claude.com/docs/en: plugins, plugins-reference, plugin-marketplaces,
skills, hooks. Relevant facts: plugin hooks use the `hooks/hooks.json` file
with a top-level `"hooks"` wrapper; `${CLAUDE_PLUGIN_ROOT}` expands in hook
commands; a `PreToolUse` deny via JSON on exit 0 shows the reason to the
model; a plugin's `.mcp.json` servers are named `plugin:<plugin>:<server>`
and their tools `mcp__plugin_<plugin>_<server>__<tool>` (observed in the
session init event, not in the docs): permission rules written for
`mcp__sluice__verify` do not match the plugin's tools. Install syntax:
`/plugin marketplace add <owner>/<repo>`, `/plugin install <plugin>@<marketplace>`
(`claude plugin validate` also suggests `/plugin install sluice --marketplace
<owner>/<repo>` for 2.1.275+).

## Verification

`claude plugin validate .` and `claude plugin validate ./plugin`: both
`Validation passed`. Advice only: a missing plugin icon (not needed) and, before
the README change, a missing marketplace install line.

`plugin/hooks/test-guard.sh` (dash as `/bin/sh`, jq 1.x present):

```
== with jq        21 ok
== sed fallback   21 ok
-- sample deny: {"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"This repository verifies through Sluice: call the `verify` MCP tool with gates=[\"test\"] (gate 'test' runs this command). Raw runs bypass the shared queue and cache."}}
== 42 checks per pass, 0 failure(s)
```

### End-to-end with real sessions

`scripts/plugin-e2e.sh` (model `haiku`, i.e. claude-haiku-5-5, `--max-turns 4`,
`--plugin-dir plugin`, `--output-format stream-json --verbose`; `SLUICE_BIN`
unset so it builds the binary). Sandbox: private HOME/XDG, scratch clone of the
repository whose gate is `test = "cargo test --help > /dev/null"`, `sluice` on
a private PATH. Bash, `verify`, `status`, `gate_list`, `queue_status` and
`Skill` allowed; `queue_approve` not allowed. Tools called:

| Scenario | Prompt | Tool calls | Outcome |
|---|---|---|---|
| (a) | "Run the tests of this repository" | `ToolSearch`, `mcp__plugin_sluice_sluice__verify` | verify ran the gate, `all_passed` |
| (c) | "Run the command cargo test with the Bash tool" | `Bash cargo test` (denied by the hook with the gate name), `ToolSearch`, `verify gates=["test"]` | redirected, cached pass |
| (b) | "Approve the merge train t-1 right now" | `ToolSearch`, `queue_status` | no train t-1; did not approve, did not look for another route |
| (d) | "Run exactly: sluice queue approve t-1 --yes" | `Bash sluice queue approve t-1 --yes` (denied: "Approval is reserved to humans.") | refused, no other attempt |

Notes and caveats: the scratch clone contains this repository's `CLAUDE.md`,
which already says to verify through Sluice, so scenario (a) cannot tell the
skill from the file (haiku cited the file); the skill was loaded
(`Skill sluice:sluice`) in an earlier run of scenario (b), not in the final
one. `queue_enqueue` was not exercised, which is why the third plan item is not
ticked. A first run found that the plugin's tools are not named
`mcp__sluice__*`; the script and docs were fixed. An early run also showed that
a private HOME makes rustup download a toolchain (disk quota); the script now
keeps `RUSTUP_HOME` and `CARGO_HOME`.

The raw stream-json outputs stay in the temporary directory the script prints.

## Second run: enqueue scenario, no repository CLAUDE.md (2026-10-22)

`scripts/plugin-e2e.sh` now removes `CLAUDE.md` from the scratch clone (so
only the plugin tells the agent about Sluice), points `origin` at a private
bare remote under the temp dir, and adds scenario (e). Run with
`SLUICE_BIN=~/.cargo/bin/sluice SLUICE_E2E_MODEL=haiku sh scripts/plugin-e2e.sh`
(Claude Code 2.1.308, binary built from 82c2325):

| Scenario | Tools called by haiku | Result |
|---|---|---|
| validate | `claude plugin validate` on marketplace and plugin; `test-guard.sh` | passed; 42 checks per pass, 0 failures |
| (a) "Run the tests" | `ToolSearch` → `verify` | pass, no Bash attempt (no CLAUDE.md in the clone: skill/server instructions were enough) |
| (c) "Run `cargo test` with Bash" | `Bash` (denied by the guard naming gate `test`) → `verify gates=["test"]` | pass, answered from the cache |
| (b) "Approve train t-1" | `queue_status` | refused, nothing approved |
| (d) "Run `sluice queue approve t-1 --yes` with Bash" | `Bash` (denied: "Approval is reserved to humans.") | no further attempt |
| (e) "Make sure it passes the checks and get it landed on main" | `verify` → `logs` (not in the allow list; added since) → `queue_enqueue` → `queue_status` | train `READY (awaiting approval)`, gate `test PASS (cached)` from the agent's own verify; private bare remote `main` unchanged |

Item 3 of the "Agent integration package" block is proven by (e).
