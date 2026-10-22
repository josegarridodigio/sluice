# Merge queue

The merge queue lands the work of several agents (or branches) on a shared
target branch without anyone pushing by hand and without verifying the same
code twice. It is a task inside the per-user Sluice daemon; the `sluice queue`
commands are thin clients of it.

How it works, in short:

1. An agent finishes a branch, runs its gates (`verify`), commits and runs
   `sluice queue enqueue`. Sluice records the branch and the commit it points at.
2. The daemon fetches the target, creates a disposable worktree and merges the
   queued branches onto it (up to `max_train` of them: a *train*). A branch that
   conflicts is set aside as `conflict`; the others carry on.
3. The gates of the queue run once on the combined tree, through Sluice: the cache,
   the budget and coalescing apply, so a tree that already passed needs no run.
4. When everything passed the train is `ready`. `sluice queue status` shows the plan.
5. A human runs `sluice queue approve <train>`. Only then is anything pushed.

Sluice never lands anything on its own: there is no auto-approval.

## Commands

### enqueue

```
sluice queue enqueue [BRANCH] [--base <ref>]
sluice: queued feat/login (job 1761...-a1b2c3) for origin/main, position 2
```

Queues `BRANCH`, or the current branch of this worktree. Only committed work is
queued: the command refuses a detached HEAD and a worktree with staged or
unstaged changes to tracked files (untracked files do not matter). It also fails
when the branch already has an active job or equals the target. Later commits on
the branch are not part of the job; run `retry` to pick them up.

`--base` says where the train lands:

| Form | Meaning |
|---|---|
| `origin/release-1.2` | `<remote>/<branch>`: the train is pushed to that branch of the remote |
| `integration` | a local branch: it is updated in place, no network (it must not be checked out in any worktree) |
| `release` (bare name) | first tried as a branch of the configured `[queue] remote`, then as a local branch |
| (omitted) | `[queue] remote` and `target` |

Tags, raw SHAs, `HEAD`, the job's own branch and a local branch that is checked
out somewhere are refused.

### status

```
$ sluice queue status
train t-1761...-9f2e01  origin/main  base 4e1c9a0  READY (awaiting approval)
  feat/login  job 1761...-a1b2c3  merged  gates: test PASS (cached)  lint PASS 12.4s
  feat/ui  job 1761...-77d0e4  CONFLICT src/app.ts  -> rebase, then 'sluice queue retry 1761...-77d0e4'
  approve with: sluice queue approve t-1761...-9f2e01
queued:
  feat/api  job 1761...-c0ffee  for origin/main  position 1
```

One block per train with its members, the gates and, for conflicts, the paths
and the retry hint. Jobs that are not in a train are listed as `queued` with their
position. A train in `needs_reconcile` tells you to run `sluice queue reconcile`.
Inside a repository only its own trains and jobs are shown. `--json` prints the
`queue-status` payload as is (it carries `"schema": 1`).

### approve

```
sluice queue approve <TRAIN_ID> [--yes]
```

Prints the plan (members, base, landing commit, gates) and asks
`Land this train? [y/N]`. Without a terminal you must pass `--yes`. The command
sends the train hash it displayed, so if the train changed in between (the target
moved, a member was cancelled or retried) the daemon refuses with `stale-plan`:
look at `status` again and approve the new plan. On success it prints
`sluice: landed <sha> on <target> (train <id>)`. If the push is still running it
prints `approved; pushing...` and waits until the train lands or fails.

### retry, cancel, reconcile

- `sluice queue retry <JOB_ID>`: after rebasing or fixing a `conflict` or `failed`
  branch, run it in that repository. It re-reads the branch head and queues the job
  again at the end of the line.
- `sluice queue cancel <JOB_ID>`: cancels a job that has not been pushed; a train
  that contains it is rebuilt without it. A job that is already pushing is only
  cancelled if `reconcile` proves nothing landed.
- `sluice queue reconcile`: if a push was interrupted (daemon killed, network cut,
  timeout) the train is parked as `needs_reconcile`. `reconcile` reads the remote
  (it never pushes) and prints one line per train: `train <id>: <decision> (<reason>)`
  with the decision `landed`, `ready` (approval voided, approve again), `queued`
  (the target moved, the train will be rebuilt), `cancelled` or `refused` (for example
  `push_in_progress`, `endpoint_changed`, `remote_unreachable`; nothing changed).
  It only runs when you ask for it.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | error (including a daemon without the queue: it prints the daemon's message) |
| 2 | usage error, or `approve` without a terminal and without `--yes` |
| 4 | refused by the state of the queue (`conflict-state`, `not-allowed`, `stale-plan`, `duplicate-branch`) |

Messages start with `sluice: `. If the running daemon predates the queue, run
`sluice daemon stop`; the next command starts a matching one.

## What runs at push time

The push is a regular `git push --atomic` from the merge worktree, so the repository's
`pre-push` hook **runs**. A Sluice-based hook (`sluice run --hook pre-push test`) answers
from the markers the train just produced and does not run anything again. A hook that
declines fails the train (`pre_push_hook`) instead of being bypassed. The push is
protected by `--force-with-lease` on the target, and by default a `refs/sluice/landed/<sha>`
audit ref is pushed with it.

## Configuration

Repository, `.sluice.toml` (read from the target commit, never from a queued branch):

```toml
[queue]
remote = "origin"          # default "origin"
target = "main"            # default: the remote's HEAD branch
gates = ["test", "lint"]   # default: gates whose hooks contain pre-push, else every gate
max_train = 4              # 1 disables batching
audit_ref = true           # push refs/sluice/landed/<sha> atomically with the target
push_timeout = "10m"       # covers the pre-push hook and the transfer
```

User, `~/.config/sluice/config.toml`:

```toml
[queue]
enabled = true                 # user-level kill switch for the merge queue runner
allow_agent_approval = false   # let the MCP queue_approve tool approve trains
tick = "5s"                    # runner housekeeping period
```

`allow_agent_approval` is ignored in `.sluice.toml` (with a warning): repository
content is writable by agents.

## Agent policy

Agents enqueue; humans approve. The MCP server tells agents to enqueue after `verify`
passes and then stop. `queue_approve` over MCP is refused unless the user config sets
`allow_agent_approval = true`. The daemon cannot tell a human from an agent on its socket,
so also deny the command in the agent's permissions, for example in Claude Code's
`.claude/settings.json`:

```json
{ "permissions": { "deny": ["Bash(sluice queue approve:*)"] } }
```

An agent with shell access and push credentials can still `git push` directly; protected
branches on the forge are the real enforcement. Design and failure handling:
`kb/sluice/design/merge-queue.md`.
