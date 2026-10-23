---
name: sluice
description: Use before running tests, linters, type checks or builds, before committing or pushing, and when landing a finished branch, in any repository that has a `.sluice.toml` at its root. Sluice is a shared verification queue and cache; this skill says how to verify through the `verify` MCP tool instead of the shell, what git hook exit code 3 means, and how to land work through the merge queue.
---

# Verifying and landing work with Sluice

A repository with a `.sluice.toml` runs its tests, linters and type checks
through Sluice: one queue and one cache shared by every agent on the machine.
A tree that already passed is answered from the cache in milliseconds.

## Automatic setup in agent sessions

The plugin's session hook does the setup; there is nothing for the user to
install, edit or export beyond the `sluice` binary itself:

- Shell shims are on automatically in every repository: heavy commands
  (builds, test runners, `ng`, `playwright`, package scripts, `make`, ...)
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
  none is tagged). Use `gate_list` to see the gates and their commands.
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
  a pass is cached for this exact git tree.

## Read the result

- One line per gate: pass or FAIL, with markers such as `cached` (answered
  from a previous pass of this tree) and `coalesced` (another agent was already
  running the same tree and gate, you shared its result).
- On FAIL the result includes the log tail of the first failure. Read it, fix
  the code, call `verify` again.
- For more output use `logs` (newest job by default, or a job id). For queued
  and running jobs, and the cache state of your tree, use `status` with
  `working_directory`. Do not re-run anything to diagnose.

## Git hook exit code 3

If `git commit` or `git push` fails with a message starting
`sluice: '<gate>' has not passed on this tree` (exit code 3), the hook refused
to run the gate inside your shell call. Call `verify` with that gate, then
retry the same git command. Do not bypass the hook (`--no-verify`) and do not
change the hook policy.

## Landing a branch (merge queue)

1. Commit the branch. Do not leave tracked changes uncommitted.
2. `verify` on the committed branch and wait for it to pass.
3. Call `queue_enqueue` with your absolute working directory.
4. Stop. Tell the human the job id and the train id (`queue_status` shows
   trains and which are ready). A human approves with
   `sluice queue approve <train id>`.
5. If `queue_status` shows a conflict or a failed gate for your branch: rebase
   or fix it, commit, `verify` again, then `queue_retry` with the job id.
   `queue_enqueue` would answer `duplicate-branch`. `queue_cancel` withdraws a
   job you no longer want.

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
