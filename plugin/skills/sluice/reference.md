# Sluice reference (load on demand)

## Tools

All tools take `working_directory`: an absolute path inside the repository or
worktree. The server does not know your cwd.

| Tool | Use |
|---|---|
| `verify` | Run gates and block until they finish. `gates` (names, run in order), `timeout_seconds` (per gate, 0 = none), `extra_args` (appended to every gate command and part of the cache key). |
| `status` | Queued and running jobs; with `working_directory`, the cache state of that tree per gate (`hit`, `miss`, `uncached`, `error`). Never starts the daemon. |
| `gate_list` | Gates of `.sluice.toml`: name, command, weight, timeout, hooks, cache flag. |
| `logs` | Tail of a job log; newest job by default. |
| `queue_enqueue` | Put the committed branch in the local merge queue. Refuses a detached HEAD and tracked uncommitted changes. |
| `queue_status` | Trains, members, conflicts with paths, trains ready for a human. |
| `queue_approve` | Human decision. Refused with `not-allowed` unless the user enabled agent approval. |
| `queue_retry` | Re-queue a `conflict` or `failed` job with the current head of its branch. |
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
- All selected gates run even after one fails; fix everything the tail shows,
  then call `verify` again.
- `not_cached_reason` appears on a pass that could not be cached (for example
  `CI` is set, or the tree is unknown): that pass will run again next time.
- Use `logs` with the job id for the full log, not another run.

## Merge queue states

- `queued`: waiting for a train. `queue_status` shows the position.
- `conflict`: your branch does not merge with the others on the target. The
  entry lists the conflicting paths. Rebase on the target, commit, `verify`,
  then `queue_retry` with the job id.
- `failed`: a gate failed on the combined tree, or the target could not be
  fetched (`base_unavailable`) after several tries. Fix the cause, commit,
  `verify`, `queue_retry`.
- Train `ready`: everything passed; waiting for a human to run
  `sluice queue approve <train id>`. Tell the human and stop.
- Train `needs_reconcile`: a push was interrupted; a human runs
  `sluice queue reconcile`. Do nothing.

Error codes: `conflict-state`, `stale-plan`, `duplicate-branch`, `not-allowed`,
`not-supported`. Report them to the human.

## Troubleshooting

- Tools missing: Sluice is not installed or not in the `PATH` Claude Code
  sees (`claude mcp list` shows the `sluice` server state). Install with the
  installer line from `SKILL.md`, then restart Claude Code.
- `verify` says there is no `.sluice.toml`: the directory is not in a Sluice
  repository; run the project's own commands as usual.
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
