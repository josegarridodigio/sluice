# Using Sluice with coding agents

This guide is for developers who run coding agents (Claude Code first, others
second) on a repository that uses Sluice. It assumes `.sluice.toml` and your
git hooks are already set up (`sluice init`, `sluice hook print`).

For Claude Code, the recommended path is the Sluice plugin (section 2): one
install per user, no block to paste into every repository. Other agents use
the MCP server and a short instruction block (section 3).

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
(Claude Code sends the progress token and resets its idle window on each
notification; verified with `scripts/phase3-e2e.sh`), and an interactive
session that auto-backgrounds a call after 2 minutes delivers the result
later instead of cancelling it (background tasks do not survive the
session). If you lower `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`, keep it above
`[mcp] progress_interval` (15 s by default).

## 2. Install the Claude Code plugin (recommended)

Prerequisite: `sluice` in the `PATH` that Claude Code sees (see the install
section of the README: `curl -fsSL .../scripts/install.sh | sh`). Then, in
Claude Code:

```
/plugin marketplace add josegarridodigio/sluice
/plugin install sluice@sluice
```

(From a shell: `claude plugin marketplace add josegarridodigio/sluice`, then
`claude plugin install sluice@sluice`.) The plugin, in `plugin/` of this
repository, contains:

- the MCP server, registered as `sluice` (`sluice mcp`). Because it comes from
  a plugin, its tools are named `mcp__plugin_sluice_sluice__verify` and so on:
  use those names in permission rules;
- the `sluice` skill: loaded when the agent is about to run tests, commit,
  push or land a branch in a repository with a `.sluice.toml`. It teaches the
  `verify` flow, exit code 3 from hooks, and the merge queue (verify,
  `queue_enqueue`, stop, never approve);
- a `PreToolUse` hook on `Bash` (`sluice guard`, from 0.3.0) that rewrites a
  gate command to `sluice run <gate> -v` and a heavy command (builds, test
  runners, `docker build`, `make`: `[agent] heavy_commands` of the user
  config) to `sluice exec --label <L> -- <command>`, so they queue under the
  machine's budget with the same output and exit code. The rewrite only
  inserts that prefix into the agent's unchanged command; pipelines after the
  first command, `&`, heredocs, `<`, `$(`, subshells, `sh -c` and wrappers are
  never rewritten. It answers `allow` only where your permission settings
  already allow the original (or in `bypassPermissions` mode), `ask` in
  `default`/`acceptEdits` mode (the prompt shows the rewritten command), and
  leaves the command alone when a deny or ask rule could match (section 6).
  The agent sees a one-line note naming the rewritten command. Raw gate
  commands it does not rewrite are denied with a message that names the gate
  to verify, and `sluice queue approve` is always denied. It fails open;
  `SLUICE_GUARD=0` disables it and `SLUICE_GUARD=deny` turns rewrites into
  denials that suggest the rewritten command. With an older `sluice` the hook
  falls back to the 0.2.0 deny-only logic.

With the plugin, the repository needs no `CLAUDE.md` block. Details and the
hook's limits: [plugin/README.md](../plugin/README.md).

### Manual MCP registration (any MCP-capable agent)

Without the plugin, add Sluice to the project's `.mcp.json`:

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
syntax; check `claude mcp list` afterwards. Registered this way, the tools are
named `mcp__sluice__verify` and so on.

The server exposes nine tools (the last five are the merge queue, see
section 3):

| Tool | Purpose |
|---|---|
| `verify` | Run gates (default: those whose `hooks` contain `pre-commit`; if none is tagged, every gate). Blocks until they finish, returns one line per gate and the log tail of the first failure. |
| `status` | Queued and running jobs; with `working_directory`, also the cache state of that tree per gate. |
| `gate_list` | Gates defined in `.sluice.toml`. |
| `logs` | Tail of a job log (default: newest). |
| `queue_enqueue` | Put the committed branch (default: the current one) in the local merge queue. Refuses a detached HEAD and tracked uncommitted changes. |
| `queue_status` | Trains, members and their state, conflicts with paths, and which trains are ready for a human to approve. |
| `queue_approve` | Approve a ready train. Refused with `not-allowed` unless the user config has `[queue] allow_agent_approval = true`. |
| `queue_retry` | Re-queue a `conflict` or `failed` job with the current head of its branch (after rebasing or fixing it). |
| `queue_cancel` | Withdraw a job (only recorded while it is pushing; honoured once nothing landed is proven). |

`verify` takes `working_directory` (absolute path inside the repository or
worktree, required) and optionally `gates`, `timeout_seconds` and
`extra_args`. A failing gate is a normal result flagged as an error, so the
model reads the tail. It never runs a gate in the agent's process: if the
daemon is unreachable it reports an error.

## 3. Fallback: tell the agent what to do

With the plugin you can skip this section: the skill carries the same
instructions. For agents without the plugin, paste this into `AGENTS.md` (or
`CLAUDE.md`):

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

### Landing work through the merge queue

If the repository uses the Sluice merge queue, add these lines to the same
file (the plugin's skill already covers them). The MCP server also sends the
gist as its own instructions, but the file is read first:

```markdown
## Landing work (Sluice merge queue)

- When a branch is finished: commit it, call `verify` and wait for it to pass
  on the committed branch, then call `queue_enqueue` with your absolute
  working directory. Then stop.
- Tell the human the job id (and the train id that `queue_status` shows). A
  human approves the train with `sluice queue approve <train id>`.
- Never approve a train, never call `queue_approve` unless the user told you
  to, and never push to the target branch yourself.
- If `queue_status` reports a conflict (or a failed gate) for your branch,
  rebase or fix it, commit, call `verify` again, and call `queue_retry` with
  the job id. The job keeps the branch, so `queue_enqueue` would answer
  `duplicate-branch`. `queue_cancel` withdraws a job you no longer want.
- Errors start with a code: `conflict-state`, `stale-plan`,
  `duplicate-branch`, `not-allowed`, `not-supported`. Report them; do not work
  around them.
```

Approval is a human decision. `queue_approve` is off by default and answers
`not-allowed` without contacting the daemon; it only works when the user
enables `[queue] allow_agent_approval = true` in
`~/.config/sluice/config.toml` (the daemon checks the flag again). The daemon
cannot tell a human from an agent on its socket, so also deny the CLI path to
agents in `.claude/settings.json`:

```json
{"permissions": {"deny": ["Bash(sluice queue approve:*)"]}}
```

The plugin's hook already denies this command; the settings rule is a second,
independent layer. Deny rules still apply in `bypassPermissions` mode. An agent with shell
access and push credentials can still `git push` directly: protect the target
branch on the forge as well.

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

The `fail-fast` message is three lines on stderr (`<rule>` is the detection
rule that fired, `<why>` the cache state; the exit code is 3):

```
sluice: '<gate>' has not passed on this tree (<why>).
sluice: <hook> was invoked by a coding agent (<rule>) and agent_hook_policy is "fail-fast".
sluice: call the 'verify' MCP tool (or run 'sluice run <gate>') first, then retry the git command.
```

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

Levers, in order:

1. The plugin (section 2). Its skill tells the agent to verify, and its hook
   rewrites gate and heavy commands run through Bash to go through Sluice (or
   denies raw gate commands it cannot rewrite) with a note the model reads.
   Hooks run in `bypassPermissions` mode too. Without the plugin, use the
   `CLAUDE.md` / `AGENTS.md` section from section 3.
2. A deny rule for the raw command is the enforcement option. In
   `.claude/settings.json`:

   ```json
   {"permissions": {"deny": ["Bash(npm test:*)"]}}
   ```

   Add one rule per raw command your gates run. Deny rules still apply in
   `bypassPermissions` mode.

MCP tool rules use the form `mcp__<server>__<tool>`, so the `verify` tool is
`mcp__sluice__verify`, or `mcp__plugin_sluice_sluice__verify` when the server
comes from the plugin. A rule like `Bash(sluice run:*)` is reasonable:
`sluice run` goes through the daemon queue (it only runs in-process when the
daemon is unavailable, and `run-unqueued` must be configured explicitly).
Prefer it to a blanket `Bash(sluice:*)`, which would also allow
`sluice daemon stop --force`.

How the guard's rewrite interacts with your rules: Claude Code matches deny
and ask rules against the command a hook returns, so a rule such as
`Bash(make deploy:*)` would not match `sluice exec --label make-deploy -- make
deploy`. The guard therefore reads the `Bash` rules of
`~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`),
`.claude/settings.json`, `.claude/settings.local.json` of
`$CLAUDE_PROJECT_DIR` and `/etc/claude-code/managed-settings.json`, and does
not rewrite at all when a deny or ask rule may match the original or the
rewrite (matching is deliberately liberal) or a file cannot be parsed. It
answers `allow` only when every part of the original matches an allow rule
(matching is strict: exact text, `prefix:*` with a word boundary, `*` globs;
any redirection counts as not allowed) or the session is in
`bypassPermissions` mode; in `plan`, `auto` and `dontAsk` modes it rewrites
only commands the allow rules cover. Rules given on the command line
(`--allowedTools`, `--disallowedTools`) are invisible to hooks and are not
considered.

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
to `[agent_detection]` (section 5) so the fail-fast policy applies. Agents that
support MCP can use `sluice mcp` the same way as Claude Code (manual registration), with the same
`mcpServers` entry in their own config format. For agents that expose no
reliable marker, export `SLUICE_AGENT=1` in the shell the agent uses.
