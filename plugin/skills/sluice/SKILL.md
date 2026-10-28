---
name: sluice
description: Use before running tests, linters, type checks or builds, before committing or pushing, and when landing a finished branch, in any repository that has a `.sluice.toml` at its root, or whose git hooks go through Sluice (`sluice hooks on`). Sluice is a shared verification queue and cache; this skill says how to verify through the `verify` MCP tool instead of the shell, what git hook exit code 3 means, and how to land work through the merge queue.
---

# Verifying and landing work with Sluice

A repository with a `.sluice.toml` runs its tests, linters and type checks
through Sluice: one queue and one cache shared by every agent on the machine.
A tree that already passed is answered from the cache in milliseconds.

A repository without a `.sluice.toml` whose git hooks go through Sluice (the
session context says so) works the same way: its gates are the check units of
its hooks (husky, lefthook, pre-commit or plain hooks), and a `verify` pass is
the cache hit that the commit's hook then finds. `gate_list` shows those gates
and the hooks or units that are not gates (formatters that edit files, hooks
Sluice does not understand), each with the reason. The units of a hook run by
Sluice are not refused with exit code 3 when they miss: a commit made without
a `verify` first runs them inside `git commit`, which can outlast your shell
call. Call `verify` before every commit.

## Automatic setup in agent sessions

The plugin's session hook does the setup; there is nothing for the user to
install, edit or export beyond the `sluice` binary itself:

- Shell shims are on automatically in every repository: heavy commands
  (builds, `cargo check`/`cargo doc`, test runners, `ng`, `playwright`,
  package scripts, `make`, ...)
  are queued under the machine's shared budget even when they run inside a
  subshell, `$(...)`, `sh -c` or a script. You do not prefix anything.
- Never retry a queued command without Sluice. Do not wrap builds or tests
  in subshells, `$(...)` or scripts to capture their output; redirect with
  `>` or `2>&1`. Do not start several dev servers or watchers while gates
  run. `SLUICE_SHIMS=0` turns the shims off only if the human asks for it.
- The session start prints these rules and the gate names as context.

If the session context says the repository has no `.sluice.toml`: propose
Sluice to the user once. Run `sluice init --dry-run`, show its output to the
user and run `sluice init` only after the user agrees. If they decline, do
not bring it up again.

If the session context says the `sluice` binary is missing or older than the
plugin: tell the user, and only with their consent run the install command
(or `sluice update`) given in that context, then ask them to restart the
session. Never install or update on your own.

## Verify

- Call the `verify` MCP tool (`mcp__plugin_sluice_sluice__verify` when loaded from the plugin) with `working_directory`
  set to your absolute directory inside the repository or worktree. Optional:
  `gates` (names), `timeout_seconds`, `extra_args`, `fail_fast` (default true).
- With no `gates`, it runs the gates tagged `pre-commit` (or all of them if
  none is tagged); without a `.sluice.toml`, the check units of the
  `pre-commit` hook (or of all the hooks if it has none). Use `gate_list` to
  see the gates and their commands. A unit that is not listed (it edits files
  or depends on what is staged) runs only inside the commit's hook: do not run
  its command yourself, let the hook run it.
- Never run a gate command through Bash (`cargo test`, `npm test`, `pytest`,
  linters, ...). It bypasses the queue and the cache. A guard hook may deny it
  and tell you which gate to verify instead.
- Heavy Bash commands (builds, test runners, `docker build`, `make`, ...) may
  be rewritten by the guard to run through Sluice's shared queue: you then see
  a note "Sluice ran this through its shared queue as: `sluice exec --label
  ... -- <your command>`" (or `sluice run <gate> -v` for a gate). Output and
  exit code are your command's own; treat the result as if you had run it. Do
  not retry it without the prefix and do not prefix commands yourself.
- Call `verify` before every commit. Do not re-run a gate that just passed:
  a pass is cached for this exact git tree. A lefthook or pre-commit
  unit depends on what is staged: if `gate_list` leaves it out for that
  reason, `git add` your change and call `verify` again.

## Read the result

- One line per gate: pass or FAIL, with markers such as `cached` (answered
  from a previous pass of this tree) and `coalesced` (another agent was already
  running the same tree and gate, you shared its result).
- On FAIL the result includes the log tail of the first failure. Read it, fix
  the code, call `verify` again.
- `KILLED for lack of memory ...` is not a test failure: the kernel or
  systemd-oomd (or Sluice's opt-in memory guard) ended the gate for lack of
  memory. Do not retry it unchanged and never run it through the shell to get
  around it: reduce its footprint (for example test concurrency) or tell the
  human.
- A gate that `status` shows as waiting for memory (`(budget: memory)` in
  its text, `wait_detail` `memory` in the structured result) is only queued
  behind other gates: normal, wait. Do not run it outside Sluice.
- A `verify` progress message, or a `status` line, that reports your job
  `STALLED` (no CPU, I/O or output for its quiet window, 5 minutes or more)
  means the gate is probably hung, for example a test waiting forever. Look
  at the test and its `logs`; you may cancel your own `verify` call (the
  daemon then ends the job if nobody else waits for it). Never re-run the
  gate through the shell. Sluice only reports a stall: it never ends the job
  by itself, so the gate's timeout still applies.
- For more output use `logs` (newest job by default, or a job id). For queued
  and running jobs, and the cache state of your tree, use `status` with
  `working_directory`. Do not re-run anything to diagnose.

## Git hook exit code 3

If `git commit` or `git push` fails with a message starting
`sluice: '<gate>' has not passed on this tree` (exit code 3), the hook refused
to run the gate inside your shell call. Call `verify` with that gate, then
retry the same git command. Do not bypass the hook (`--no-verify`) and do not
change the hook policy. A hook that Sluice runs as units never answers with
that message; an exit code 3 without it is the hook manager's own (for example
pre-commit's internal error): read the output, do not retry blindly.

## Landing a branch (merge queue)

1. Commit the branch. Do not leave tracked changes uncommitted.
2. `verify` on the committed branch and wait for it to pass.
3. Call `queue_enqueue` with your absolute working directory.
4. Stop. Tell the human the job id and the train id (`queue_status` shows
   trains and which are ready). A human approves with
   `sluice queue approve <train id>`. If the `queue_enqueue` result carries a
   warning about the target (pull request required, protected, could not
   check), pass it on: the job is queued anyway.
5. If `queue_status` shows a conflict or a failed gate for your branch: rebase
   or fix it, commit, `verify` again, then `queue_retry` with the job id.
   `queue_enqueue` would answer `duplicate-branch`. `queue_cancel` withdraws a
   job you no longer want.

For a reason you cannot read in `queue_status`, see "Merge queue states" in
`reference.md`. In short: `prepare_failed` on a waiting job is the target's or
the environment's problem (wait or tell the human); on a failed job with
`base: false` your branch broke the prepare (usually a lockfile out of sync):
fix it, commit, `queue_retry`; a branch that changes
`.sluice.toml` (`hook_gate_unverified`, `config_changed`) is for the human.
`--trust-member-config` is human-only: never use it and never ask to approve
such a train; explain it to the human.
A job or train `awaiting_merge` means a human approved it and its pull request
(not you) is the way it lands: wait, or tell the human. Never merge, close or
push to that pull request or its `sluice/train/*` branch, and do not cancel it
(`queue_cancel` is refused). `pr_closed`: tell the human, do not retry.

Never approve a train: not with `queue_approve`, not with
`sluice queue approve` in Bash, unless the human told you to in this
conversation (and `queue_approve` is refused anyway unless the user enabled
agent approval). Never push to the target branch yourself. Errors start with
a code (`conflict-state`, `stale-plan`, `duplicate-branch`, `not-allowed`,
`not-supported`): report them, do not work around them.

## If the sluice tools are missing

If the sluice MCP tools (`verify`, `status`, ...) are not available, or `verify` reports that the daemon is
unreachable, do not fall back to running the gates in the shell. Tell the
human: Sluice is not installed or not in the `PATH` Claude Code sees. With
their consent, install:

```sh
curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh
```

then restart Claude Code (or run `/reload-plugins`) and check `claude mcp list`.

More detail (tool parameters, result format, queue states, troubleshooting) is
in `reference.md` next to this file; read it only when needed.
