# Using Sluice with coding agents

This guide is for developers who run coding agents (Claude Code first, others
second) on a repository that uses Sluice. It assumes `.sluice.toml` and your
git hooks are already set up (`sluice init`, `sluice hook print`).

> Status: `sluice run`, the hook policy and `sluice mcp` (the MCP server
> with the `verify`, `status`, `gate_list` and `logs` tools) all work. The
> manual two-agent run that proves the Claude Code side end to end is still
> pending (`docs/manual-test-phase-3.md`).

## 1. Why agents need Sluice

Agents re-run the same test, lint and type-check suites over and over, often
on trees that already passed, and several agents in several worktrees do it
at once. A git hook that waits in a queue runs inside the agent's `git commit`
shell call, where a timeout can cut it off: a foreground Bash command that
hits its timeout is moved to the background rather than killed, and the agent
then carries on without the commit result. The MCP `verify` tool blocks
safely instead: Claude Code aborts an MCP call only if neither a response nor
a progress notification arrives within the idle window (30 minutes by default
for stdio servers); Sluice sends progress notifications every 15 seconds
when the call carries a progress token, and an interactive session that
auto-backgrounds a call after 2 minutes delivers the result later instead of
cancelling it (background tasks do not survive the session). Whether Claude
Code sends a progress token is not documented, so budget for the 30-minute
idle window: queue wait plus the longest gate should stay under it, or raise
`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`.

## 2. Install the MCP server

Add Sluice to the project's `.mcp.json`:

```json
{"mcpServers": {"sluice": {"command": "sluice", "args": ["mcp"]}}}
```

or register it from the command line with `claude mcp add` (the command is
`sluice`, the argument is `mcp`):

```sh
claude mcp add sluice -- sluice mcp
```

`sluice` must be in the `PATH` that Claude Code sees. The `claude mcp add`
form above follows the documented `claude mcp add <name> <command> [args]`
syntax; check `claude mcp list` afterwards.

The server exposes four tools:

| Tool | Purpose |
|---|---|
| `verify` | Run gates (default: those whose `hooks` contain `pre-commit`; if none is tagged, every gate). Blocks until they finish, returns one line per gate and the log tail of the first failure. |
| `status` | Queued and running jobs; with `working_directory`, also the cache state of that tree per gate. |
| `gate_list` | Gates defined in `.sluice.toml`. |
| `logs` | Tail of a job log (default: newest). |

`verify` takes `working_directory` (absolute path inside the repository or
worktree, required) and optionally `gates`, `timeout_seconds` and
`extra_args`. A failing gate is a normal result flagged as an error, so the
model reads the tail. It never runs a gate in the agent's process: if the
daemon is unreachable it reports an error.

## 3. Tell the agent what to do

Paste this into `CLAUDE.md` (Claude Code) or `AGENTS.md` (other agents):

```markdown
## Verification (Sluice)

- Before committing, call the `verify` tool (MCP server `sluice`). Its default
  gates are the ones tagged `pre-commit` in `.sluice.toml`.
- Never run test, lint, type-check or build-verification commands through the
  shell (no `npm test`, `cargo test`, `pytest`, `tsc`, linters, ...). `verify`
  queues them, caches them per git tree and keeps the output small.
- If `verify` reports FAIL, read the log tail, fix the code, and call `verify`
  again.
- Do not re-run a gate that just passed. A pass is cached for this exact tree.
- If a git hook fails with a message starting `sluice: '<gate>' has not passed
  on this tree`, call `verify` for that gate, then retry the git command.
- For diagnosis use `sluice cache status` (what is cached for this tree) and
  `sluice logs` (newest job log) instead of re-running anything.
```

Adapt the examples in the second bullet to your stack.

## 4. How the hooks behave with an agent

Hooks call `sluice run --hook <hook> <gate>`. On a cache hit the hook exits 0
in milliseconds whoever calls it. On a miss, the behaviour depends on who
invoked the hook and on `defaults.agent_hook_policy` in `.sluice.toml`:

| `agent_hook_policy` | Behaviour on a miss when an agent invoked the hook |
|---|---|
| `fail-fast` (default) | Refuse immediately with exit code 3 and an actionable message. |
| `wait` | Wait for budget in the queue. Risky: the wait happens inside the agent's shell call. |
| `run-unqueued` | Run the gate immediately, outside the queue and the budget. |

Humans use `human_hook_policy` (`wait` by default, or `run-unqueued`).

The `fail-fast` message is three lines on stderr (shown here as the Phase 1
wording; `<rule>` is the detection rule that fired):

```
sluice: '<gate>' has not passed on this tree (<why>).
sluice: <hook> was invoked by a coding agent (<rule>) and agent_hook_policy is "fail-fast".
sluice: call the 'verify' MCP tool (or run 'sluice run <gate>') first, then retry the git command.
```

'verify' MCP tool (or run 'sluice run <gate>') first"). The exit code is 3
in both versions.

With the cache off (for example `CI` is set, or `SLUICE_CACHE=0`), the gate
can never be cached, so refusing would block the caller forever: the gate
just runs. The policy applies only when the cache was consulted and missed.

`sluice run --explain <gate>` prints every decision (cache mode, caller and
rule, hook, policies, tree, key, lookup result) without running anything.

## 5. How Sluice knows it is an agent

Detection runs in the `sluice run` client, at hook time, in this order:

1. `SLUICE_AGENT` is set: `1` forces agent, `0` forces human. Other values
   are ignored.
2. Any variable listed in `agent_detection.env_any_set` is set and non-empty.
   The default is `["CLAUDECODE"]`. Claude Code documents `CLAUDECODE=1` as
   set in every subprocess it spawns (Bash, hooks, status line, stdio MCP
   servers), without a stability guarantee.
3. Any `KEY = value` pair in `agent_detection.env_any_equals` matches
   (default empty).

Git hooks inherit the environment of the `git` process, which inherits the
agent's shell, so the markers are visible inside hooks. Detection is never
done inside the daemon, and gate processes do not receive any `CLAUDE*`
variable.

To recognise another agent, extend the `[agent_detection]` table of your user
config (`~/.config/sluice/config.toml`):

```toml
version = 1

[agent_detection]
env_any_set = ["CLAUDECODE", "MYAGENT_ACTIVE"]
env_any_equals = { MYAGENT_MODE = "cli" }
```

`MYAGENT_ACTIVE` and `MYAGENT_MODE` are made up. Before you add a marker,
verify in that agent's documentation, or by printing its environment from
inside one of its shell calls, that it really sets the variable in
subprocesses.

## 6. Permissions caveat (read this)

Sluice cannot stop an agent from running the test command directly. If it can,
it bypasses the queue and the cache.

- Allow rules such as `Bash(npm test:*)` let the agent run `npm test ...`
  without prompting.
- In `bypassPermissions` mode, allow rules have no effect: everything is
  allowed except what deny rules and explicit ask rules cover.

Two levers, in order:

1. The `CLAUDE.md` / `AGENTS.md` section from step 3. It is the first lever
   and, in `bypassPermissions` mode, the only one that does not need a deny
   rule.
2. A deny rule for the raw command is the enforcement option. In
   `.claude/settings.json`:

   ```json
   {"permissions": {"deny": ["Bash(npm test:*)"]}}
   ```

   Add one rule per raw command your gates run. Deny rules still apply in
   `bypassPermissions` mode.

MCP tool rules use the form `mcp__<server>__<tool>`, so the `verify` tool is
`mcp__sluice__verify`. A rule like `Bash(sluice run:*)` is reasonable:
`sluice run` goes through the daemon queue (it only runs in-process when the
daemon is unavailable, and `run-unqueued` must be configured explicitly).
Prefer it to a blanket `Bash(sluice:*)`, which would also allow
`sluice daemon stop --force`.

## 7. Running several agents

Cache markers live under `$(git rev-parse --git-common-dir)/sluice/markers/`,
so every worktree of a repository shares them. With several agents on
several worktrees:

- One agent calls `verify` and runs the gate. A tree that passed once passes
  everywhere: another worktree, a no-op rebase, the push.
- Agents that ask for the same tree and gate while it runs coalesce onto the
  same job and wait for its result. If a marker appears while a request is
  queued, it completes without running.
- Different trees queue under the daemon's budget, so the machine is never
  asked to run N suites at once.

`sluice status` (or the `status` tool) shows the queued and running jobs, the
repository each belongs to, and totals. `sluice status --json` gives the same
data for scripts.

## 8. Other agents

Anything that spawns `sluice run <gate>` from git hooks works with Sluice
today, because hooks do not care who triggered them: add the agent's marker
to `[agent_detection]` (step 5) so the fail-fast policy applies. Agents that
support MCP can use `sluice mcp` the same way as Claude Code, with the same
`mcpServers` entry in their own config format. For agents that expose no
reliable marker, export `SLUICE_AGENT=1` in the shell the agent uses.
