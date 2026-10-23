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
out somewhere are refused, and so is a `[queue] remote` that is not a configured
remote of the repository or starts with `-` (`invalid_remote`).

Everything the queue asks a remote (the fetch of the target, the checks before the
push, `reconcile`) goes to the URL `git push <remote>` would use: the remote's
`pushurl` (or its `url`, with `insteadOf`/`pushInsteadOf` applied). With a fetch URL A
and a push URL B, trains are built on B's branch and land on B. A remote with several
push URLs is refused. Each of these calls is limited by `[queue] network_timeout`
(default `60s`): a remote that hangs makes the jobs wait with `base_unavailable` (and
`failed` after 3 attempts) without holding up other repositories or approved trains.

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
and the retry hint. A dissolved train is hidden once a newer train exists for the
same target (its members show up in the newer train or on their own; `--json`
keeps every train). Jobs that are not in a train are listed as `queued` with their
position; other jobs outside a train (for example `CONFLICT` or `FAILED`) are listed
under `jobs needing attention:`. An empty queue prints `sluice: the merge queue is empty`. When no train can be built because the target cannot be fetched or the
base commit's `.sluice.toml` is invalid, the queued line says why
(`waiting: base_unavailable <git error>` or `waiting: base_config_invalid <error>`);
the queue tries again once per `[queue] tick` and, after 3 failed attempts in a
row, marks the jobs `FAILED` with that reason. Fix the remote or the config, then
`sluice queue retry <JOB_ID>`. A train in `needs_reconcile` tells you to run `sluice queue reconcile`.
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
prints `sluice: approved; pushing…` and waits until the train lands or fails, at most the
base's `push_timeout` plus its `network_timeout`, two ticks and 30 s; then it prints
the train's current state and exits 1 (the daemon carries on; check
`sluice queue status`).

### retry, cancel, reconcile

- `sluice queue retry <JOB_ID>`: after rebasing or fixing a `conflict` or `failed`
  branch, run it in that repository. It re-reads the branch head and queues the job
  again at the end of the line.
- `sluice queue cancel <JOB_ID>`: cancels a job that has not been pushed; a train
  that contains it is rebuilt without it. A job that is already pushing is only
  cancelled if `reconcile` proves nothing landed.
- `sluice queue reconcile`: if a push was interrupted (daemon killed, network cut,
  timeout, or the push could not be started) the train is parked as
  `needs_reconcile`. `reconcile` reads the remote
  (it never pushes) and prints one line per train: `train <id>: <decision> (<reason>)`
  with the decision `landed`, `ready` (approval voided, approve again), `queued`
  (the target moved, the train will be rebuilt), `cancelled` or `refused` (for example
  `push_in_progress`, `endpoint_changed`, `remote_unreachable`; nothing changed).
  With nothing to reconcile it prints `sluice: no train needs reconciling`.
It only runs when you ask for it. If the decision cannot be recorded, the train
  stays `needs_reconcile` with its pin ref and worktree, so `reconcile` can run again.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | error: not a repository, detached HEAD, dirty worktree, unknown train or job, a base the queue cannot use (tag, raw SHA, invalid remote), `not-supported` (the queue is disabled with `[queue] enabled = false`), an `approve` declined at the prompt or that stopped waiting, and a daemon without the queue (the client says it does not support the merge queue) |
| 2 | usage error, or `approve` without a terminal and without `--yes` |
| 4 | refused by the state of the queue (`conflict-state`, `not-allowed`, `stale-plan`, `duplicate-branch`), for example `approve` of a train that is not `ready`, or an `enqueue` of the target itself or of a branch that already has an active job |

Messages start with `sluice: `. If the running daemon predates the queue, run
`sluice daemon stop`; the next command starts a matching one.

## What runs at push time

The push is a regular `git push --atomic` from the merge worktree, so the repository's
`pre-push` hook **runs**. A Sluice-based hook (`sluice run --hook pre-push test`) answers
from the markers the train just produced and does not run anything again. A hook that
declines fails the train (`pre_push_hook`) instead of being bypassed. Untracked or
ignored files that gates leave in the merge worktree do not matter (they are not part of
the commit); a modified tracked file dissolves the train. The push is
protected by `--force-with-lease` on the target, and by default a `refs/sluice/landed/<sha>`
audit ref is pushed with it.

## Authenticated remotes (SSH, HTTPS)

The daemon has no credentials of its own and never prompts: git runs with
`GIT_TERMINAL_PROMPT=0`, no stdin and every `GIT_*` variable of the daemon removed.
Credentials come from the environment of the CLI (or MCP) command, and only these
variables cross over: `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `GIT_SSH_COMMAND`,
`GIT_ASKPASS`, `SSH_ASKPASS`, `HOME`, `PATH`, `TERM`. Anything else (tokens in other
variables, `GIT_CONFIG_*`) is dropped.

- **Whose environment.** `enqueue` (and `retry`, which keeps it) supplies the env of the
  fetches and `ls-remote` calls that build and watch the train; `approve` supplies the
  env of the pre-push check, the push itself and a later `reconcile` of that train. So
  run `sluice queue approve` in a shell that can push (`git push` works there), and make
  sure the enqueuing agent can at least fetch. The approver's variables are stored with
  the train until it lands, is dissolved or its approval is voided.
- **SSH.** Load your key in an agent (`ssh-add`) so `SSH_AUTH_SOCK` reaches the push;
  a key with a passphrase and no agent fails (there is no terminal to ask). Keep the
  agent running until the push and any `reconcile` are done. Host keys must already be
  known (or `GIT_SSH_COMMAND="ssh -o StrictHostKeyChecking=accept-new"`); an unknown host
  key fails instead of asking. `core.sshCommand` in the repository config works as usual.
- **HTTPS.** Point `GIT_ASKPASS` at a program that prints the user name (when asked
  `Username for ...`) and the token (when asked `Password for ...`). A `credential.helper`
  from your git config is read too (the daemon's git sees your `HOME`), but only if it
  needs nothing outside the allowlist: a desktop keyring reached through D-Bus does not
  get `DBUS_SESSION_BUS_ADDRESS`. A token embedded in the remote URL (`https://user:token@host/...`) works too and
  is never stored: messages show the URL without it.
- **Refused credentials fail at once.** A wrong key or password, an unknown host key or
  no credentials at all fail the train's jobs with reason `push_rejected` (the error says
  the remote refused the approver's credentials) and `approve` exits 1 right away; nothing
  was pushed. Fix the credentials, `sluice queue retry` the jobs and approve the new train.
- **A remote that stalls.** Every fetch and `ls-remote` is bounded by `network_timeout`
  (an unreachable remote leaves an approved train `approved`, checked again at the next
  tick); the push by `push_timeout`, after which the train is `needs_reconcile` and
  `reconcile` (with the approver's stored env) tells whether it landed. A stalled remote
  never blocks other repositories' trains.
- **What is never written.** Passwords, tokens and keys never reach the queue database,
  `daemon.log`, `client.log` or `sluice doctor --bundle`: the database stores variable
  values such as the askpass path or the agent socket path, not what they give access to.

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
network_timeout = "60s"    # every other network call (fetch, ls-remote); the process group is killed on expiry
```

User, `~/.config/sluice/config.toml`:

```toml
[queue]
enabled = true                 # user-level kill switch for the merge queue runner
allow_agent_approval = false   # let agents approve trains (MCP queue_approve, CLI from an agent)
tick = "5s"                    # runner housekeeping period
retention = "30d"              # finished jobs, trains and their history are pruned after this
```

`allow_agent_approval` only exists in the user config. Putting it in `.sluice.toml`
is a configuration error: repository content is writable by agents, so it can
never grant approval rights.

## Agent policy

Agents enqueue; humans approve. The MCP server tells agents to enqueue after `verify`
passes and then stop; after a conflict or a failure they fix the branch and call
`queue_retry` (`queue_cancel` withdraws a job). Unless the user config sets
`allow_agent_approval = true`, approval by an agent is refused: `queue_approve` over MCP,
and also `sluice queue approve` run from an agent's environment (detected with the same
`[agent_detection]` rules that mark agent runs, for example `CLAUDECODE` set; `SLUICE_AGENT=0`
marks a human). The Claude Code plugin's guard also denies the command. An agent controls its
own environment, so the daemon cannot be sure who is on its socket: also deny the command in
the agent's permissions, for example in Claude Code's `.claude/settings.json`:

```json
{ "permissions": { "deny": ["Bash(sluice queue approve:*)"] } }
```

An agent with shell access and push credentials can still `git push` directly; protected
branches on the forge are the real enforcement. Design and failure handling:
`kb/sluice/design/merge-queue.md`.
