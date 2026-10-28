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
sluice queue enqueue [BRANCH] [--base <ref>] [--trust-member-config]
sluice: queued feat/login (job 1761...-a1b2c3) for origin/main, position 2
```

Queues `BRANCH`, or the current branch of this worktree. Only committed work is
queued: the command refuses a detached HEAD and a worktree with staged or
unstaged changes to tracked files (untracked files do not matter). It also fails
when the branch already has an active job or equals the target. Later commits on
the branch are not part of the job; run `retry` to pick them up.
`--trust-member-config` is for humans, see
[Fixing the queue's own configuration](#fixing-the-queues-own-configuration).

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

A gate that passed but wrote no cache marker (for example because it left untracked
files) shows `test PASS (not cached: <reason>)`; the train still goes on. A member whose
branch changes `.sluice.toml` is marked `changes .sluice.toml (this train runs the base's
gates)`, both in `status` and in the plan the approval prints: the gates always come from
the target, never from the branch. When the base has a `[queue] prepare`, the train shows its
state (`prepare: running (job <id>)`, `prepare: pass 41.2s`, `prepare: fail (job <id>)`; absent
when every gate hit the cache); see "Preparing the merge worktree".

Reasons a job shows (`waiting:` for a job still `queued`, otherwise next to `FAILED`):

| Reason | Meaning |
|---|---|
| `base_unavailable`, `base_config_invalid` | the target cannot be fetched, or its `.sluice.toml` does not parse; retried once per tick, `FAILED` after 3 |
| `key_input_failed` | a gate's `key_inputs` command failed in the train's worktree; retried once per tick, `FAILED` after 3 |
| `prepare_failed` | `[queue] prepare` failed. With `base: true` in the detail the target fails the same way (nobody's fault: the jobs wait as `queued`, retried once per tick, `FAILED` after 3); with `base: false` your change broke it and the job is `FAILED` |
| `prepare_dirty` | `prepare` passed but left files that are neither in the commit nor git-ignored (the detail lists the paths); make it write only ignored files |
| `gate_failed` | a gate failed, timed out, was killed or errored. A gate (or `prepare`) the kernel or `systemd-oomd` killed for lack of memory says `was killed for lack of memory` and carries `killed_by: "memory"` in the detail; do not retry it unchanged |
| `gate_modified_tree` | the gates passed but edited tracked files of the train's worktree (the detail lists the gates and the paths); make the gate leave tracked files alone |
| `hook_gate_unverified` | the branch's `.sluice.toml` asks the `pre-push` hook for gates the train does not verify (the train runs the target's configuration), so the push would be refused; detail `gates` and a `hint`. Land the configuration change first (its own train, or directly), or a human runs `sluice queue retry <job> --trust-member-config`. The other members of the train are requeued, not failed |
| `member_config_invalid` | the merged `.sluice.toml` of a branch that changed it does not parse (the detail has the parse error), or a `--trust-member-config` job's file does not |
| `conflict`, `merge_failed`, `dirty_merge` | the merge did not apply cleanly (`conflict` lists the paths) |
| `push_rejected`, `pre_push_hook` | the remote or the repository's `pre-push` hook refused the push |
| `target_protected` | the forge refuses direct pushes to the target (a protected branch, a pull request required); the detail carries the server's words. Retrying cannot help: see [Targets protected against direct pushes](#targets-protected-against-direct-pushes) |
| `target_checked_out`, `tree_mismatch`, `internal` | local target checked out at landing time; the merged tree differs from what git snapshots; an unexpected error |

`sluice queue enqueue` without `--base` refuses a branch whose own `.sluice.toml` does not
parse (`invalid_config: ...`): that file picks the default target, so a broken one must not
silently fall back to the built-in defaults. Pass `--base` to enqueue anyway; the queue reads
the gates from the target either way.

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
  again at the end of the line. A plain retry always goes back to the target's
  configuration; for a configuration that cannot pass see
  [Fixing the queue's own configuration](#fixing-the-queues-own-configuration)
  (`--trust-member-config`, humans only).
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

## Preparing the merge worktree

A train's gates run in a fresh worktree made from git: it has none of the git-ignored
files of your checkout, `node_modules` and virtual environments first of all, so
`pnpm test` fails at once. `[queue] prepare` is the command that restores them. It runs
once per train, in the train's worktree, before the gates:

```toml
[queue]
prepare = "pnpm install --frozen-lockfile --prefer-offline"
prepare_timeout = "10m"   # default
prepare_weight = "auto"   # like a gate's weight
```

(`sluice init` writes it for the lockfile it finds at the root: pnpm, npm, yarn, bun, uv
or poetry. It needs Sluice 0.6.0 on every machine that reads the file.) It goes through
the daemon's scheduler like a gate (job name `queue:prepare`, the budget applies, it is
killed when the train is dropped). A train whose gates all hit the cache never runs it.
The command may only create git-ignored files: if the tree no longer matches the merge
afterwards the train fails with `prepare_dirty`. `queue status` shows
`prepare: pass 41.2s` on the train.

When `prepare` fails Sluice does not guess whose fault it is: it runs the same command on
the target's commit alone, in a throw-away worktree. If that fails too (registry down,
missing tool), the jobs go back to `queued` with `prepare_failed` and are retried; if the
target prepares fine, your change broke it, so the job fails (a train of several members
is split first, as for a failing gate). Nothing is remembered: every prepare failure
checks the target again, because an environment failure (registry down, timeout, out of
memory) must never be blamed on a member just because the target prepared fine earlier.

## Where trains get their tools

A train runs in the daemon's process, not in your terminal or your agent's session, so
its tools (the gates' commands, `prepare`, the base probe and the `key_inputs` commands)
need an environment of their own. It is **your login environment**: when the daemon starts
it runs your login shell once (`$SHELL`, else your passwd entry, else `/bin/sh`, as
`<shell> -l -i -c '...; env -0'`, with a 10 s deadline) and keeps the result, so a daemon
restarted from a non-interactive SSH session, cron or a script finds the same `node`,
`pnpm` or `python` as your terminal. The trains' `PATH` is your login `PATH` followed by
the directories of the daemon's own `PATH` it lacks (so a tool only the daemon's starter
had, an activated virtualenv for example, is still found, and never shadows yours). It is
never the environment of whoever enqueued: that would let a branch's author choose the
binaries that judge the branch.

```toml
[queue]
tool_env = "login-shell"   # default; "daemon" = the daemon's own environment
tool_path = "/opt/node/bin:/usr/bin:/bin"   # optional: an explicit PATH, wins
```

If the login shell cannot be resolved (it times out because an rc file waits for input,
exits non-zero, or prints nothing) the trains fall back to the daemon's own environment
and say so: `sluice status` shows `tool env: daemon (<reason>)` and the daemon log has a
warning. `sluice doctor` checks that the first word of each `key_inputs` command of the
current repository is in the `PATH` trains use. Restart the daemon (`sluice daemon
restart`) after installing a toolchain so it is picked up.

A train that fails with `key_input_failed: node --version: exit status 127` is this
problem: the tool is not in the trains' `PATH`. Fix your login shell's `PATH` (or set
`tool_path`) and restart the daemon.

## Fixing the queue's own configuration

The train runs the gates and the `[queue]` settings of the **target's** `.sluice.toml`,
never the branch's own, so that a branch cannot choose the judges of its own change. The
price is a dead end: if the target's configuration makes every train fail (a gate that
cannot pass, a `prepare` that is wrong), the branch that fixes it cannot land through the
queue, because the broken configuration judges it. The way out used to be a direct push to
the target. For that case a human can say, explicitly:

```sh
sluice queue enqueue --trust-member-config        # a new job
sluice queue retry <JOB_ID> --trust-member-config # a job that failed
```

What it does for that job only:

- Its train is built from the **branch's** `.sluice.toml` (read at the branch's head):
  gates, `gates` selection, `prepare`, timeouts, `audit_ref`. `remote` and `target` stay
  what they were when the job was queued. A branch without the file runs no gates; an
  invalid file fails the job once with `member_config_invalid` and the parse error. A
  target whose own configuration does not parse does not stop it.
- The job is always alone in its train, which records `config_source = member` and the
  hash of the file. The hash enters the train hash, so a commit that changes the file
  voids a plan you were shown. Ordinary trains are not affected.
- `queue status` and the approval plan print `CONFIG FROM BRANCH <branch>: this train runs
  the branch's own .sluice.toml` and the diff of the file against the target
  (`git diff <base>...<branch> -- .sluice.toml`, or the whole file when the target has
  none; capped at 64 KiB). The plan comes before the `Land this train? [y/N]` prompt;
  read it. `--yes` still works from a script run by a human.
- A plain `retry` (no flag) clears it: the next train uses the target's configuration
  again.

Security model: the branch picks its own judges, so a person must read what it picked.
Both the flag and the approval of such a train are therefore refused, with `not-allowed`,
to agents: over MCP (the tools do not offer the flag) and from a CLI whose environment is
detected as an agent's, **even with `[queue] allow_agent_approval = true`**. The daemon
checks this itself; deny `Bash(sluice queue enqueue:*)` and `Bash(sluice queue retry:*)`
for agents too if you want the same defence in depth as for the approval command. The flag needs a
daemon of this version (it announces the `queue-trust-config` feature); with an older one
the CLI refuses instead of ignoring it. Once the fix has landed, ordinary trains are
judged by the fixed file.

## What runs at push time

The push is a regular `git push --atomic` from the merge worktree, so the repository's
`pre-push` hook **runs**. A Sluice-based hook (`sluice run --hook pre-push test`) is
answered by the daemon from the train: it recognises its own push (the hook's process
descends from the push it started, in the worktree it holds, which still contains exactly
the tree it verified) and replies `PASS (cached)` with `verified by merge train <id>`
without running anything, whatever `PATH` or `CI` the hook sees. A gate the hook runs
that the train did not verify fails the push at once (`pre_push_hook`) with a message
such as `gate 'x' runs in the pre-push hook but merge train <id> did not verify it`:
add it to `[queue] gates` (the gates with a `pre-push` hook are always included), or,
when a member changes `.sluice.toml`, land that change first (the train runs the gates of
the target's `.sluice.toml`). The answer is given only to the daemon's own push; a
`sluice run` from anywhere else is never answered this way. A hook that
declines fails the train (`pre_push_hook`) instead of being bypassed, and hooks that are
not Sluice's run as always. Untracked or
ignored files that gates leave in the merge worktree do not matter (they are not part of
the commit); a modified tracked file dissolves the train. The push is
protected by `--force-with-lease` on the target, and by default a `refs/sluice/landed/<sha>`
audit ref is pushed with it.

### Targets protected against direct pushes

The queue lands by pushing the merge commit to the target. A target branch that the forge
only lets change through a pull request (GitHub "Require a pull request before merging",
GitLab protected branches with "Allowed to push: No one", and similar) refuses that push
with a server-side hook (`protected branch hook declined`). A `git push --dry-run` does not
run that hook, so Sluice finds out in two other ways, and neither refuses anything:

- **At enqueue and in `sluice doctor`**, when `gh` can tell. `sluice queue enqueue` (and the
  MCP `queue_enqueue`) asks the forge, with *your own* `gh` login (never the daemon's), whether
  the target accepts direct pushes: it reads the rules that apply to the branch, the branch's
  `protected` flag and, when it is readable, the classic branch protection, and prints a
  warning for a target that requires a pull request or is protected, or "could not check:
  <reason>" when it cannot tell (no `gh`, not logged in, a forge other than GitHub, no network,
  no access). GitHub, GitHub Enterprise Server hosts you are logged in to, and SSH aliases
  (resolved with `ssh -G`) are understood. The probe takes a few seconds at most and its answer
  is cached for 5 minutes. `sluice doctor`, which never uses the network, shows the last answer
  an enqueue got (kept 30 days) as the `target-protection` check. **The enqueue always
  proceeds.**
- **From a refused push.** A push rejected with a recognised protected-branch text (GitHub
  `protected branch hook declined` / `GH006`, GitLab `You are not allowed to push code to
  protected branches`, or a `pre-receive hook declined` that says `protected branch`) fails
  the jobs with `target_protected` instead of `push_rejected`, and the queue remembers it for
  that remote and target. The next enqueue to it warns from what was learned, even where `gh`
  cannot tell. A push that lands forgets it.

The queue **cannot land on such a target yet**: the train is verified and approved, then its
jobs fail with `target_protected` and nothing lands. Until pull-request landing exists (issue
#5), verify with `verify` / `sluice run` and land through the forge's pull requests instead.
The probe describes your `gh` identity, not the SSH or HTTPS identity that pushes, so an
administrator who can bypass the protection may still see the warning.

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
  tick); the push by `push_timeout`, after which the train is `needs_reconcile` (its note
  is `push_timeout: ` followed by the last lines the push or its `pre-push` hook printed) and
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
gates = ["test", "lint"]   # plus every gate whose hooks contain pre-push; default: those, else every gate
max_train = 4              # 1 disables batching
audit_ref = true           # push refs/sluice/landed/<sha> atomically with the target
push_timeout = "10m"       # covers the pre-push hook and the transfer
network_timeout = "60s"    # every other network call (fetch, ls-remote); the process group is killed on expiry
prepare = "pnpm install --frozen-lockfile --prefer-offline"  # optional, needs 0.6.0: runs once per train before the gates
prepare_timeout = "10m"    # default
prepare_weight = "auto"    # integer >= 1 or "auto"
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
