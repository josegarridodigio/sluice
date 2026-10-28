# Sluice reference (load on demand)

## Tools

All tools take `working_directory`: an absolute path inside the repository or
worktree. The server does not know your cwd.

| Tool | Use |
|---|---|
| `verify` | Run gates and block until they finish. `gates` (names, run in order), `timeout_seconds` (per gate, 0 = none), `extra_args` (appended to every gate command and part of the cache key), `fail_fast` (default true: return at the first failing gate and cancel the pending ones; false waits for every gate). All selected gates start at once unless the user disabled `[mcp] verify_parallel`. |
| `status` | Queued and running jobs; with `working_directory`, the cache state of that tree per gate (`hit`, `miss`, `uncached`, `error`). Never starts the daemon. |
| `gate_list` | Gates of `.sluice.toml`: name, command, weight, timeout, hooks, cache flag. Without a `.sluice.toml` and with the hooks entry point on: the check units of the hooks (`source: hook`, `manager`, named `<manager>:<hook>#<n>`) and `excluded`, the hooks and units that are not gates with the reason. |
| `logs` | Tail of a job log; newest job by default. `job_id`, `tail` (default 100, max 2000). |
| `queue_enqueue` | Put the committed branch (`branch`, default the current one; `base`, default the configured target) in the local merge queue. Refuses a detached HEAD and tracked uncommitted changes. |
| `queue_status` | Trains, members, conflicts with paths, trains ready for a human. |
| `queue_approve` | Human decision. Refused with `not-allowed` unless the user enabled agent approval. |
| `queue_retry` | Re-queue a `conflict` or `failed` job with the current head of its branch, always judged by the target's configuration. The CLI's `--trust-member-config` is for humans; the tool does not offer it. |
| `queue_cancel` | Withdraw a job. |

## Reading a `verify` result

One line per gate:

```
test  PASS 32.1s (job 1761...)
lint  PASS (cached, passed 2h ago)
e2e   FAIL exit=1 12.3s (job 1761...) log=/home/u/.local/state/sluice/logs/1761....log
--- last 40 lines of /home/u/.local/state/sluice/logs/1761....log ---
...
```

- `cached`: a previous pass of this exact tree answered it, nothing ran.
- `coalesced`: the same tree and gate was already running for another agent;
  you waited for and share that result.
- Status values: `pass`, `fail`, `timeout`, `killed`, `error`, `cancelled`.
  A failing gate is a normal result flagged as an error: read the tail.
- `KILLED for lack of memory after 41.2s (job ...) log=...: <message>`
  (`killed_by` is `memory` in the structured result): the kernel's OOM killer,
  systemd-oomd or Sluice's opt-in memory guard ended the gate. The message says
  its peak and what was available. Running it again unchanged is likely to end
  the same way: lower the gate's footprint (fewer test workers, less
  concurrency) or tell the human; never run the gate through the shell to get
  around it. A plain `KILLED after ...` is a cancel or a stop, not this.
  Through the shell (`sluice run`, `sluice exec`, a rewritten command, a git
  hook) the same event prints `sluice: <gate> KILLED for lack of memory after
  41.2s (job ...)` followed by the daemon's message line (exit code 143): same
  wording, same rule.
- By default (`fail_fast`) the call returns at the first failing gate and the
  gates still pending are cancelled (`cancelled`); fix what the tail shows,
  then call `verify` again. With `fail_fast` false every gate runs and every
  failure is reported.
- `not_cached_reason` appears on a pass that could not be cached (for example
  `CI` is set, or the tree is unknown): that pass will run again next time.
- A queued job in `status` has `reason: "budget"` and, when it fits the CPU
  budget but not the free memory, `wait_detail: "memory"` with the memory it is
  expected to use (the text of `status`, like `sluice status`, prints
  `(budget: memory)`, and the structured result has `wait_detail: "memory"`). It starts when a running gate ends: normal, wait for it
  and do not run it outside Sluice.
- A running job in `status` that shows `STALLED 6m12s (flat: cpu io output)`
  (`stalled: {for_ms, flat}` in the structured result, a `looks STALLED`
  line in the progress of `verify`) has shown no CPU time, block I/O, page
  faults, process changes or log output for its whole quiet window
  (`max(5 min, 3 x its usual duration)`, or the gate's `stall_timeout`). It
  holds the weight and memory shown on its line. It is probably hung (a test
  waiting forever): read `logs`, fix or skip the test, cancel your own
  `verify` call if you are waiting for it (the daemon ends the job when
  nobody else waits for it) and call `verify` again after the fix. Never
  re-run the gate through the shell. Sluice reports a stall and never ends
  the job itself; when the job shows activity again the line goes away.
- Use `logs` with the job id for the full log, not another run.

## Merge queue states

- `queued`: waiting for a train. `queue_status` shows the position, and
  `waiting: <reason>` when the train cannot be built yet (see below).
- `conflict`: your branch does not merge with the others on the target. The
  entry lists the conflicting paths. Rebase on the target, commit, `verify`,
  then `queue_retry` with the job id.
- `failed`: a gate failed on the combined tree, the prepare failed, or the
  target could not be fetched after several tries. Fix the cause, commit,
  `verify`, `queue_retry`.
- Train `ready`: everything passed; waiting for a human to run
  `sluice queue approve <train id>`. Tell the human and stop.
- Train `needs_reconcile`: a push was interrupted; a human runs
  `sluice queue reconcile`. Do nothing.

Reasons, with what you do:

| Reason | Meaning | You |
|---|---|---|
| `base_unavailable`, `base_config_invalid` | the target cannot be fetched, or its `.sluice.toml` does not parse; the job waits and fails after 3 tries | tell the human; `queue_retry` once fixed |
| `prepare_failed`, detail `base: true` | the target's `[queue] prepare` fails too (registry down, missing tool): the environment's or the target's problem; the job waits as `queued` | wait, or tell the human |
| `prepare_failed`, detail `base: false` | your branch broke the prepare, usually a lockfile out of sync | fix, commit, `verify`, `queue_retry` |
| `prepare_dirty` | the prepare leaves files the tree does not have (not in the commit, not git-ignored) | fix `.gitignore` or the prepare |
| `gate_failed` | a gate failed, timed out, was killed or errored (a memory kill says so and has `killed_by: "memory"` in the detail) | read the log, fix, `queue_retry`; never retry a memory kill unchanged |
| `gate_modified_tree` | the gates passed but edited tracked files (a formatter, for example) | commit the result or fix the gate |
| `key_input_failed` | a gate's `key_inputs` command failed in the train's worktree (exit 127 = a tool missing in the trains' `PATH`); the job waits and fails after 3 tries | tell the human; do not work around it |
| `member_config_invalid` | the `.sluice.toml` your branch changes does not parse once merged | fix the file, `queue_retry` |
| `hook_gate_unverified`, `config_changed` | the branch changes `.sluice.toml`; the target's configuration judges the train, so a gate the `pre-push` hook asks for may not be verified. `config_changed` is the warning shown on the member | tell the human: land the configuration change first, or the human runs `sluice queue retry <job> --trust-member-config` |
| `pre_push_hook` | the repository's `pre-push` hook declined during the push | read its output in the detail |
| `target_protected` | the forge refuses direct pushes to the target (protected branch, pull request required); the queue cannot land there | tell the human; do not retry |
| `push_rejected`, `target_checked_out`, `tree_mismatch`, `internal` | the remote refused the push, the target is checked out locally, the merged tree differs from what git snapshots, an unexpected error | tell the human |

`--trust-member-config` (on `sluice queue enqueue` and `sluice queue retry`)
builds the train from the branch's own `.sluice.toml`. It is a human decision:
a person reads the diff of the file in the approval plan. Never use it, never
suggest running it for you, and never ask to approve such a train (the daemon
refuses both for agents, over MCP and from an agent's shell); explain it to
the human instead.

During the daemon's own push of an approved train, a Sluice `pre-push` hook
(`sluice run --hook pre-push <gate>`) is answered from the train's record
(`PASS (cached)`, `verified by merge train <id>`). There is nothing for you to
do; a `sluice run` anywhere else is never answered this way.

Error codes: `conflict-state`, `stale-plan`, `duplicate-branch`, `not-allowed`,
`not-supported`. Report them to the human. `queue_enqueue` can also answer
`bad-request` with `invalid_config: ...` when your branch's `.sluice.toml`
does not parse: fix the file.

## Session start and shims

The plugin's `SessionStart` hook (`startup`, `resume`, `clear`, `compact`)
runs `sluice agent session-start`. It installs the shell shims in
`$XDG_DATA_HOME/sluice/shims` (default `~/.local/share/sluice/shims`), puts
that directory first in the session's `PATH` (through the session env file,
once, also after `--resume`) and adds the rules to your context. The shims
apply to every Bash command, background command and subagent of the session,
in every repository, with or without a `.sluice.toml`. `sluice shim status`
shows whether they are first in `PATH`. Commands run by absolute path or
under `env -i` bypass them. A human can turn them off with `SLUICE_SHIMS=0`.

Where the machine allows it (cgroup v2, systemd user manager), the same hook
also puts the agent, its MCP servers and its shells in a per-session cgroup so
no runaway process can starve the machine: the agent in a `ui` leaf, each shell
command in a `tools` leaf with less CPU weight (a guarded, silent line in the
env file; read-only cgroupfs, as in the Bash sandbox, means `nice` 19 instead).
Nothing is killed and nothing changes for you; `contained=` in the client log
says what happened.

The rules it gives you include the memory one: a gate `KILLED for lack of
memory` is not retried unchanged. Heavy commands include `cargo check` and
`cargo doc`, which go through the queue like `cargo build`.

Context variants: repository with `.sluice.toml` (rules and gate names);
git repository without it whose hooks go through Sluice (`sluice hooks on`
was run: the rules; the gates are the hooks' check units, see `gate_list`); git
repository without it (propose Sluice once: `sluice init --dry-run`, show
the output, `sluice init` only after the user agrees); anything else (one
line). A missing or older binary yields a note asking you to tell the user
and, only with their consent, run the installer or `sluice update`. In a clone
whose hooks entry point was displaced the session start puts it back and says so
(`hooks=restored` in the log); when it cannot, the context asks you to tell the
user to run `sluice hooks on`. It never turns the entry point on by itself.

## Troubleshooting

- Tools missing: Sluice is not installed or not in the `PATH` Claude Code
  sees (`claude mcp list` shows the `sluice` server state). Install with the
  installer line from `SKILL.md`, then restart Claude Code.
- `verify` says there is no `.sluice.toml`: the directory is not in a Sluice
  repository, or its hooks yield no unit Sluice can verify (`gate_list` and
  `sluice hooks status` say why); run the project's own commands as usual.
- `verify` reports the daemon unreachable: ask the human to run
  `sluice status` in a terminal; never run the gate in the shell.
- A Bash command was denied with "This repository verifies through Sluice":
  call `verify` with the gate named in the message. The human can disable the
  guard with `SLUICE_GUARD=0` in the environment.
- A Bash command came back with the note "Sluice ran this through its shared
  queue as: `<command>`": the guard inserted `sluice exec --label <L> -- ` in
  front of a heavy command (or replaced a gate command with
  `sluice run <gate> -v`) so it waited for the machine's shared budget. The
  output and exit code are those of your command (`sluice exec` adds at most
  two `sluice: exec ...` queue lines on stderr; `sluice run -v` streams the gate
  log on stderr and prints the gate summary). Nothing else changed: same
  arguments, quotes, directory and environment.
- A Bash command was denied with "This machine shares its CPU through Sluice:
  run `<command>` instead": the human enabled suggestion mode
  (`SLUICE_GUARD=deny`); run exactly the command given.
- The human is asked to approve a rewritten command (`ask`): this happens when
  the original command would have needed a permission prompt anyway.
- Hook exit code 3 on a git command: see the section in `SKILL.md`.
- `gate_list` shows a unit under `excluded`: it edits files or the index, depends
  on what a commit stages, or its hook is one Sluice does not understand
  completely. Do not run its command yourself: the commit's hook runs it. A unit
  that depends on the staged set becomes a gate once the change is staged
  (`git add`, then `verify`). `sluice hooks status` (human) says what each hook
  does.
