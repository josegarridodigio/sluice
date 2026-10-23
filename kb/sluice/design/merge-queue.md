---
title: Local merge queue
tags: [sluice, design, merge-queue, phase-6]
---

# Local merge queue (Phase 6)

Status: approved by the user on 2026-10-22 (answers in section 14). Decision record:
[[sluice/design/adr/0009-merge-queue.md]]. Reference analysis:
[[sluice/research/references/mergetrain.md]]. Builds on
[[sluice/design/architecture.md]], [[sluice/design/protocol.md]],
[[sluice/design/executor.md]], [[sluice/design/scheduler.md]],
[[sluice/design/cache-key.md]], [[sluice/design/mcp-tools.md]].

## 1. Goals and non-goals

Goals:
- Agents in separate worktrees enqueue **committed** branches; one runner combines them on top
  of the remote target branch in a disposable worktree and verifies the combined tree once.
- The runner is a task inside `sluice daemon`, fenced by the daemon's existing lease; no
  second long-lived process.
- Gates run **through Sluice**: a `run` with the combined tree's key, so cache, budget, repo
  cap and coalescing apply. A combined tree that already has markers needs no execution.
- Nothing is pushed without explicit approval (`sluice queue approve` or `queue_approve`
  when the human enabled it). Approval binds to the exact train; any change voids it.
- Pushes are atomic and crash-safe: write-ahead marker + local pin ref first, then
  `git push --atomic`; `sluice queue reconcile` resolves an interrupted push deterministically.

Non-goals (v1): hosted forges, PR APIs or CI status checks; unattended auto-landing; pushing
to more than one remote per train; submodules (already unsupported by the tree hash);
Windows/macOS; rewriting or squashing agent commits; stopping an agent with push credentials
from bypassing the queue (documented, enforced by the forge, not by Sluice).

## 2. Concepts

- **Job**: one queued branch: `(repo, branch, head_sha, target)`. The SHA is captured
  by Sluice at enqueue time; later commits on the branch are not part of the job.
- **Target**: where a train lands. Two kinds: `remote` (`<remote>/<branch>`: the train is
  pushed to `refs/heads/<branch>` on that remote) and `local` (`<branch>`: the train updates
  the local `refs/heads/<branch>` with a compare-and-swap, no network). `--base` accepts
  `<remote>/<branch>`, `<branch>`, or a bare name that is first tried as a branch of
  `[queue] remote` and then as a local branch (`refs/heads/x` and `refs/remotes/r/x` spelled
  out are honoured); the default is `[queue] remote`/`target`, and without `target` the
  remote's HEAD branch (`refs/remotes/<remote>/HEAD`, else `ls-remote --symref` of its
  push URL). The `[queue]` defaults used to pick the default target are read from the
  enqueued head's `.sluice.toml` (built-in defaults when absent or invalid, e.g. a
  `remote` starting with `-`); gates never are (step 2). The remote must be a configured
  remote of the repository and must not start with `-` (`invalid_remote`; the config
  parser also rejects a leading `-` or whitespace), so it can never reach git as an
  option; user-derived positional arguments follow `--` (`--end-of-options` for
  `rev-parse`). The enqueue's remote queries use the head's `[queue] network_timeout`.
  Refused: tags, raw SHAs, `HEAD`, the job's own branch (a target branch with the job's
  branch name, local or remote), and a local branch checked out in any worktree
  (`target_checked_out`: updating it would desync that worktree, which product code never
  touches). The refusal message starts with a token: `head`, `tag`, `sha`, `unknown_base`,
  `invalid_remote`, `no_default_target`, `invalid_base` (the worktrees cannot be listed) (error code `bad-request`), `own_branch`,
  `target_checked_out` (`not-allowed`). The columns `remote`/`target` store `""`/`<branch>` for local targets.
- **Train**: the unit the runner builds, verifies, gets approved and pushes: an ordered list of
  jobs merged onto one `base_sha`. It yields a commit `S` (the landing SHA) and its tree `T`.
- **Train hash**: `sha256(repo, remote, endpoint_hash, target, base_sha, [(job_id, head_sha)…],
  S, T, sorted gate keys)`. Shown at approval and stored with it; re-checked before push. Encoding (`model::train_hash`): the
  prefix `sluice-train-v1`, then each field NUL-terminated, a `|` field after the members, and the
  gate keys as `gate=key` (`gate=-` for an uncached gate), sorted.
- **Lease**: the daemon's existing lease row (`lease.name = 'daemon'`, random token). The runner
  only works while the heartbeat succeeds; every queue write first checks that the lease row still
  carries its token (`Writer::new`, else the write is refused as lease lost); train updates are a
  compare-and-swap on `claim_token = <lease token>` and the expected `state`, job updates on the
  expected `state`.
- **Pin ref**: local ref `refs/sluice/pending/<train-id>` → `S`, created before the marker so
  the commit survives `git gc` and the merge worktree's removal until reconcile.
- **Write-ahead marker**: row in `queue_push_markers` written in the same transaction that
  moves the train to `pushing`, committed with `synchronous = FULL` through
  `DaemonHandle::store_tx_durable` (the daemon's connection otherwise runs `NORMAL`). Its presence is the only evidence
  that a push may have happened; Git alone cannot tell "never pushed" from "pushed then died".
- **Endpoint binding**: `endpoint_hash = sha256(git remote get-url --push <remote>)` with
  userinfo stripped: the URL `git push <remote>` uses (`pushurl`, else `url`, with
  `insteadOf`/`pushInsteadOf` applied; `target::endpoint`). A remote with several push
  URLs is refused (a train lands on exactly one endpoint). A local target has no URL: its
  `endpoint_hash` is `sha256("local:" + git common dir)`. Recorded in the train hash and
  the marker; push and reconcile refuse when the remote now resolves to another URL
  (`endpoint_changed`). Every network query of a train (base fetch, ready watch,
  pre-push target check, reconcile) goes to that push URL, never to the fetch URL, so a
  triangular setup (fetch from A, push to B) builds on B's branch and lands on B.
- **Audit ref**: remote ref `refs/sluice/landed/<S>` pushed atomically with the target (while
  `[queue] audit_ref` is true, the default), so
  reconcile can tell "landed and later rewritten" from "never landed".

## 3. Job state machine

Happy path: `queued → merging → verifying → ready → approved → pushing → landed`.
Side exits: `conflict`, `failed`, `cancelled`, `needs_reconcile`. Exact transitions:

| From | To | Trigger | Who |
|---|---|---|---|
| — | queued | `queue enqueue` | CLI / MCP client (agent or human) |
| queued | merging | claim into a train (one `BEGIN IMMEDIATE` with job selection) | runner |
| queued | failed | 3 consecutive failures to prepare the base (`base_unavailable`, `base_config_invalid`) | runner |
| merging | verifying | its merge applied cleanly | runner |
| merging | conflict | `git merge` conflicted (with base or earlier members; reason `conflict`, `merge_failed` or `dirty_merge`); the job leaves the train (`train_id` cleared) | runner |
| merging | failed | internal error while building the train (reason `internal`; a `verifying` job fails the same way) | runner |
| verifying | ready | every gate passed or hit | runner |
| verifying | failed | a gate failed / timed out / was killed / errored and the train has a single surviving member (reason `gate_failed`), or `tree_mismatch` (checked before any gate runs) | runner |
| verifying | queued | multi-member train failed: members split (section 5, step 7) | runner |
| ready | approved | `queue approve <train>` with matching train hash | human (CLI); an agent (MCP, or the CLI from an agent's environment) only if allowed |
| ready, approved | queued | base moved, member cancelled, `stale_plan` (train hash changed or a cancel pending), `endpoint_changed`, merge worktree changed; approval voided | runner |
| approved | pushing | marker + pin ref written, lease re-checked | runner |
| approved | failed | local target checked out at landing time (reason `target_checked_out`) | runner |
| pushing | landed | push exit 0 | runner |
| pushing | queued | definitive rejection, lease mismatch on target (remote moved), or a failed local compare-and-swap | runner |
| pushing | failed | definitive rejection for permission/policy or a refused refspec (reason `push_rejected`), or a declining `pre-push` hook (reason `pre_push_hook`) | runner |
| pushing | needs_reconcile | ambiguous push failure, or daemon start finds the train `pushing` | runner / daemon start |
| pushing | cancelled | definitive rejection (nothing landed) while `cancel_requested` | runner |
| needs_reconcile | landed / ready / queued / cancelled | decision table, section 6 | `queue reconcile` |
| conflict, failed | queued | `queue retry <job>` re-reads the branch head | client |
| any pre-`pushing` state | cancelled | `queue cancel <job>`; in `merging`/`verifying` the train is rebuilt without it | client |
| merging, verifying | queued | daemon start finds an unfinished train (it is dissolved) | daemon start |

`pushing` and `needs_reconcile` cannot be cancelled: `cancel` sets `cancel_requested`
(the reply shows the job unchanged), honoured only once nothing landed is proven (a
definitive push rejection, or reconcile rows 7 to 9). Cancelling a `ready`/`approved` member
dissolves its train at once (the other members return to `queued`); a member cancelled while
`merging` makes the build task dissolve the train once the merges are done, and while
`verifying` stop its gates at the next tick and dissolve it (the other members return to
`queued`). The state machine is the table in `sluice-gate-queue` `model.rs` (`JOB_TRANSITIONS`,
`TRAIN_TRANSITIONS`); the store refuses any other transition. A job that enters `conflict` or
`failed` loses its `train_id` (`Writer::job_to`; the audit row still names the train), so a
later `cancel` or `retry` of it never touches the train that goes on with the other members.
Terminal states: `landed`, `cancelled`; `conflict` and `failed` are
terminal until `retry`. A train's state mirrors its members' (`building`, `verifying`, `ready`,
`approved`, `pushing`, `landed`, `dissolved`, `needs_reconcile`).

## 4. Data model

The queue crate owns its tables in `<state>/sluice.db`, created with `CREATE TABLE IF NOT
EXISTS` and versioned by its own `queue_schema(version)` row (v1). The daemon's
`schema.version` stays 1, so older binaries keep opening the file; a newer `queue_schema`
fails closed. Timestamps are unix ms; job ids use the daemon's
format, train ids `t-<unix-ms>-<6 hex>`.

```sql
CREATE TABLE queue_schema (version INTEGER NOT NULL);
CREATE TABLE queue_jobs (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL,          -- git common dir
  worktree TEXT NOT NULL, branch TEXT NOT NULL, head_sha TEXT NOT NULL,
  remote TEXT NOT NULL, target TEXT NOT NULL,
  state TEXT NOT NULL, queue_order INTEGER NOT NULL, train_id TEXT,
  caller TEXT NOT NULL, session TEXT,                -- agent|human, CLAUDE_CODE_SESSION_ID
  reason TEXT, detail TEXT,                          -- e.g. 'conflict', JSON paths / gate+job id
  cancel_requested INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 1,
  solo INTEGER NOT NULL DEFAULT 0,                   -- split: next train has this job alone
  env TEXT,                                          -- JSON allowlist of the enqueuing client
  enqueued_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER);
CREATE UNIQUE INDEX queue_jobs_active_branch ON queue_jobs(repo, branch)
  WHERE state NOT IN ('landed','cancelled');
CREATE TABLE queue_trains (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL, remote TEXT NOT NULL, target TEXT NOT NULL,
  endpoint_hash TEXT NOT NULL, base_sha TEXT NOT NULL, state TEXT NOT NULL,
  merge_sha TEXT, merge_tree TEXT, worktree TEXT NOT NULL,
  gates TEXT,                                        -- JSON [{gate,key,job_id,status,duration_ms}]
  members TEXT NOT NULL DEFAULT '[]',                -- JSON job ids in merge order, as claimed
  settings TEXT,                                     -- JSON {push_timeout_ms, audit_ref, network_timeout_ms} of the base
  train_hash TEXT, claim_token TEXT NOT NULL,
  approved_at INTEGER, approved_via TEXT,            -- cli|mcp
  approved_hash TEXT, push_env TEXT,                 -- JSON allowlist from the approving client
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, landed_at INTEGER, note TEXT);
CREATE TABLE queue_push_markers (
  train_id TEXT PRIMARY KEY, sha TEXT NOT NULL, base_sha TEXT NOT NULL,
  remote TEXT NOT NULL, endpoint_hash TEXT NOT NULL, push_refs TEXT NOT NULL,  -- JSON
  audit_ref TEXT, pin_ref TEXT NOT NULL, claim_token TEXT NOT NULL,
  push_pid INTEGER, push_start_ticks INTEGER, written_at INTEGER NOT NULL);
CREATE TABLE queue_events (                          -- append-only audit trail
  at INTEGER NOT NULL, job_id TEXT, train_id TEXT, from_state TEXT, to_state TEXT NOT NULL,
  actor TEXT NOT NULL, note TEXT);                   -- runner|cli|mcp|reconcile|daemon-start
```

Credentials do not outlive their use: `queue_jobs.env` is set to `NULL` when the job
reaches `landed` or `cancelled`, and `queue_trains.push_env` when the train reaches
`landed` or `dissolved` (or its approval is voided). Retention (user config `[queue]
retention`, default `30d`; `Writer::prune`, run by `runner::maintenance` at daemon start
and once per hour): `landed`/`cancelled` jobs finished before the cutoff, markers of
`landed`/`dissolved` trains, `landed`/`dissolved` trains no job or marker still refers
to, and `queue_events` rows older than the cutoff are deleted. Live rows are never
pruned. The runner's per-step reads only load live trains (`store::live_trains`) and the
finished trains changed within the last hour (their worktrees).

## 5. Runner algorithm

One tokio task per daemon, woken by every `queue-*` request and by the user config's
`[queue] tick` (default 5 s); it does nothing while the lease is lost, the daemon is stopping
or `[queue] enabled = false` (every `queue-*` request is then refused with error code
`not-supported`). Per (repo, remote, target) at most one train is active.
Preparing a group's base and claiming it (steps 1–2), building a train (steps 3–7), the
ready watch of a train (step 8, at most once per tick) and pushing it (step 9) run as
child tasks of the runner, and every network call has a deadline (`[queue]
network_timeout`, default `60s`: `fetch` and `ls-remote` run in their own process group,
killed with SIGTERM then SIGKILL after 2 s on expiry; `git::run_output_deadline`). So a
stalled remote, a long gate or a slow push never delays other repositories, approvals,
or the push of an approved train; on daemon stop the tasks are aborted (the next start
dissolves an unfinished build and parks an unfinished push).

1. **Claim.** For every (repo, remote, target) with a `queued` job and no active train,
   oldest first, the runner spawns one task (busy key per group) that runs step 2 for it
   (fetch, config), then in one
   `BEGIN IMMEDIATE`: verifies the lease token, re-reads the queued jobs of that target and
   takes up to `[queue] max_train` (default 4) of them by `queue_order` (a `solo` job only alone, and the batch
   stops before a `solo` job), creates the train row, moves them to `merging` with
   `claim_token`. No train is built while an older train of the same target is
   `building`/`verifying`/`ready`/`approved`/`pushing`/`needs_reconcile`.
2. **Trusted config.** Remote target: `git fetch --no-tags --quiet --no-write-fetch-head -- <push
   URL> +refs/heads/<target>:refs/sluice/base/<remote>/<target>` (env: sanitised daemon env
   plus the enqueuer's allowlist; the repository's `FETCH_HEAD` is never written), `base_sha`
   = that ref. Its deadline is the `[queue] network_timeout` of a trusted commit of the
   target (the last fetched `refs/sluice/base/<remote>/<target>`, else
   `refs/remotes/<remote>/<target>`; default `60s`), never a member's. Local target: `base_sha` =
   `refs/heads/<target>` (no fetch; later "ls-remote" checks read the local ref). Read
   `[queue]` and the gate definitions from `git show <base_sha>:.sluice.toml`, never from a
   member branch. A member that changes `.sluice.toml` is flagged `config_changed` in the plan
   (job `detail = {"config_changed": true}`). A base without `.sluice.toml` has no gates and
   the default `[queue]` settings; an invalid one (or a failed fetch) leaves the jobs `queued`
   with the reason recorded (section 7, "Base unavailable"), retried at most once per tick,
   and `failed` after 3 consecutive failures. `push_timeout`, `audit_ref` and
   `network_timeout` are stored with the train (`settings`).
3. **Worktree.** `git worktree add --detach <state>/merge/<train-id> <base_sha>`, then
   `git worktree lock --reason "sluice merge queue"` (two steps: `add --lock --reason` needs
   git 2.33, the minimum is 2.31); all git calls with `-c core.hooksPath=/dev/null`, every
   `GIT_*` variable stripped.
4. **Merge.** For each member in order: if the train has one member and `base_sha` is an
   ancestor of `head_sha`, fast-forward (`git checkout --detach <head_sha>`, `S = head_sha`);
   otherwise `git merge --no-ff --no-edit -m "Merge <branch> (sluice job <id>)" <head_sha>`
   (with `-c user.name="Sluice merge queue" -c user.email=sluice@localhost` only when git has
   no committer identity). On conflict: record the conflicting paths, `git merge --abort`,
   mark the job `conflict` (reason `conflict`, detail `{"paths": [...]}`; `merge_failed` when
   git failed without conflicting paths), continue. A merge that leaves the tree dirty is
   reset (`git reset --hard`, `git clean -fd`) and the job becomes `conflict` with reason
   `dirty_merge` and the dirty paths in `detail`. No member left → the train is `dissolved`.
   Survivors → `verifying`. The train keeps every claimed member in `members`; the hash uses
   the survivors.
5. **Key.** `T = git rev-parse S^{tree}`. Take `tree::snapshot_entries` of the clean worktree (the same
   code path as every client) and assert it equals `T` (a mismatch, e.g. from eol filters,
   makes the train fail with `tree_mismatch`, never silently uncached). For each queue gate
   (`[queue] gates`, else the gates whose `hooks` contain `pre-push`, else every gate)
   derive the key with the shared `lookup::key_tree` + `lookup::derive_key_at` from `T` (or, for a gate with `paths`/`paths_ignore`, the scope hash of the snapshot entries; [[sluice/design/path-scoped-keys.md]] section 7), the base config's gate
   (name, normalised cmd, env), `key_inputs` run in the merge worktree and `key_files` read
   there (no extra arguments; a gate with `cache = false` has no key, always runs and enters
   the hash as `<gate>=-`). Markers live in the repo's common dir, so keys computed by agents' `verify` calls
   on the same tree hit: a fast-forwarded single-member train that the agent already
   verified needs no run at all.
6. **Run through Sluice.** `MarkerStore::lookup` per key; all hits (or no gate at all, e.g. a base without `.sluice.toml`) →
   `ready` immediately.
   Otherwise submit one `RunRequest` per missing gate through `DaemonHandle::submit_run`
   (section 10): `repo.common_dir` = the real common dir, `toplevel`/`cwd` = merge worktree,
   `key`/`tree` = derived, `committed_tree = T`, `caller = human`, `session =
   "queue:<train-id>"`, `child_env` = sanitised daemon env. Jobs enter the scheduler like any
   other (weight, budget, repo cap, coalescing, double-check before start, marker on pass).
   Gates run concurrently; the first failure unsubscribes the rest. Gate statuses in the
   train view: `cached` (marker hit before any run), `running` (with the sluice job id),
   then `pass`, `cached` (hit found by the scheduler), `fail`, `timeout`, `killed`, `error`.
7. **Outcome.** Decided in one write transaction (`runner::finish_train`), which re-reads
   the members: a member that left `verifying` meanwhile (cancelled) dissolves the train
   and re-queues the others (never an `internal` failure). All pass/cached → train `ready`,
   `train_hash` computed, `queue status` shows the plan. A failure in a train with a single surviving member → job `failed`, reason `gate_failed` (detail:
   `gate`, `job_id`, `status`, `exit_code`, `log_path`, `message`). A failure with several surviving members → v1 splits: every member returns to `queued`
   with the flag `solo`, so the next trains contain one job each (linear isolation). Stretch
   (optional, not v1): bisection of the failing train and re-run of the survivors as a fresh
   train, plus detection of a semantic-conflict pair, as in mergetrain.
8. **Wait.** `ready` waits for approval. Each tick re-checks cheaply (a child task per
   train, at most once per tick): a member cancelled, or `ls-remote -- <push URL>
   refs/heads/<target>` (the local ref for a local target; enqueuer's env; the train's
   `network_timeout`) ≠ `base_sha` → train dissolved, members `queued` (`attempts + 1`),
   approval voided (rebuilt on the new base; the gates may still hit). An unreachable
   remote changes nothing, nor does a push URL that no longer matches the train's
   endpoint (an approved train is then dissolved by the pre-push check with
   `endpoint_changed`).
9. **Push** (only `approved` trains, section 6). Then `landed`, pin ref deleted, worktree
   removed (`git worktree remove --force --force`, the worktree is locked; fallback: delete
   the directory and `git worktree prune`), base ref left in place. Worktrees of dissolved or
   failed trains are removed at once (and by the runner's housekeeping on every tick for
   trains finished within the last hour); `<state>/merge/` leftovers not owned by an
   active train are swept at daemon start and once per hour (`runner::sweep_merge_dir`).

## 6. Push protocol and reconcile

Before push: re-verify lease, train hash (recomputed from the rows) = `approved_hash` and no
pending cancel (else dissolved, note `stale_plan`), `HEAD == S` and no change to tracked
files in the merge worktree (`status --porcelain --untracked-files=no`: untracked or
ignored output left by a gate does not change the commit being pushed), `endpoint_hash`
of the current push URL unchanged (else dissolved with note `endpoint_changed`, so the
train is rebuilt against the new URL), for a local target that it is not checked out
(else jobs `failed`, reason `target_checked_out`), `ls-remote` of the push URL (train's
`network_timeout`) target == `base_sha` (else dissolved, `target moved`). A remote that
cannot be reached (or whose push URL cannot be resolved) leaves the train `approved` and the check is
retried at the next tick, except when the remote refused the approver's credentials
(`push::credentials_refused`: `Permission denied`, `Authentication failed`, HTTP 401/403,
`could not read Username/Password`, `Host key verification failed`): the stored env can
never succeed, so the members are `failed` with reason `push_rejected` (detail: "the remote
refused the approver's credentials: …") and the train is dissolved; nothing was pushed.
Then:

1. `git update-ref refs/sluice/pending/<train-id> S` (pin ref).
2. One transaction (`DaemonHandle::store_tx_durable`, `synchronous = FULL`): insert
   `queue_push_markers`, train and its `approved` members → `pushing`, fenced on the lease,
   `claim_token` and `cancel_requested = 0`. A refused transaction deletes the pin ref.
3. Spawn, in its own process group, with the approver's env allowlist:
   `git push --atomic --porcelain --no-follow-tags --recurse-submodules=no
   --force-with-lease=refs/heads/<target>:<base_sha> --force-with-lease=refs/sluice/landed/<S>:
   -- <remote> <S>:refs/heads/<target> <S>:refs/sluice/landed/<S>` (the audit-ref lease option and refspec
   are present only with `[queue] audit_ref = true`, the default; `push::push_args`); record `push_pid` and
   `push_start_ticks` (`executor::proc_start_ticks`) in the marker right after spawn. A
   push that cannot be spawned after the marker was written is an ambiguous end
   (`needs_reconcile`, note `push_spawn_failed: …`), never a train left `pushing`. The
   push env is the daemon's env without `GIT_*`, plus `LC_ALL=C` (stderr is parsed) and
   `GIT_TERMINAL_PROMPT=0`, plus the approver's allowlist (`SSH_AUTH_SOCK`, `SSH_AGENT_PID`,
   `GIT_SSH_COMMAND`, `GIT_ASKPASS`, `SSH_ASKPASS`, `HOME`, `PATH`, `TERM`; `git::ENV_ALLOWLIST`).
   The repository's `pre-push` hook **runs** (user decision, section 14): the push is a
   regular push from the merge worktree with the repository's `core.hooksPath`, so a
   Sluice-based hook answers from the markers the train just produced (the daemon's env
   carries no agent marker, so the human policy `wait` applies and the hook never
   fails fast). A hook that declines (`git` exits 1 with no ref line in the porcelain
   output, no `fatal:` line, no git `error:` line other than `failed to push some refs`,
   which must be present; the hook's stderr is captured) is a definitive rejection:
   jobs `failed` with reason `pre_push_hook` and the hook output in `detail`; the marker is
   deleted. The push has `[queue] push_timeout` (default `10m`); on expiry the process
   group is terminated (`terminate_group`, SIGTERM then SIGKILL after 2 s), the process is
   reaped and the train goes to `needs_reconcile` with note `push_timeout`, like any other
   ambiguous end.
   Local target: no push process; the same transaction's follow-up is
   `git update-ref -m "sluice: land <train-id>" refs/heads/<target> <S> <base_sha>` plus
   `git update-ref refs/sluice/landed/<S> <S>`, run with hooks disabled (there is no
   `pre-push` for a ref update); a failed compare-and-swap is a definitive rejection
   (train dissolved, jobs `queued`). A local ref update cannot end ambiguously, so
   `needs_reconcile` only arises for it when the daemon dies between the marker and the
   update (or `git` cannot be spawned for it); reconcile then applies rows 4 to 9 against the local refs.
4. Exit 0 → `landed`, marker kept as history (`outcome: landed` in `queue_events`), pin ref
   deleted, worktree removed. Classification (`push::classify`, on the line of
   `refs/heads/<target>`): `[rejected]` (`stale info`, `non-fast-forward`, `fetch first`) →
   the target moved: marker deleted, train dissolved, jobs `queued`; `[remote rejected]`
   (server hook, protected branch) or refused credentials before any ref line
   (`push::credentials_refused`: SSH `Permission denied (publickey)`, HTTP `Authentication
   failed`, 401/403, `could not read Username`, `Host key verification failed`) or a refspec git refused locally before sending anything (`error: src refspec`,
   `error: dst refspec`, `error: unable to push to unqualified destination`) → jobs
   `failed`, reason `push_rejected`; the `pre-push` hook declined → jobs `failed`,
   reason `pre_push_hook`, detail `{"output": ...}`. A job with a pending cancel ends
   `cancelled` instead of `failed`/`queued`. Anything else (signal, timeout, exit 0 with a rejected ref line, network error,
   truncated or unparsable output, or only the audit ref reported stale, which hints that `S`
   already landed) → `needs_reconcile`.

Daemon start (`on_start`): non-terminal trains are re-stamped with the new lease token;
`building`/`verifying` trains are dissolved (members `queued`, `attempts + 1`); trains still
`pushing` (with or without a marker) → `needs_reconcile` with their members; a push process
still alive (`executor::process_matches(pid, ticks)`) is **not** killed, it is logged and named
in the train's note. Jobs left in `merging`/`verifying` without a train are re-queued.

`sluice queue reconcile` evaluates every `needs_reconcile` train, first row that matches wins:

| # | Condition | Decision |
|---|---|---|
| 1 | recorded push process still alive (remote targets) | refuse `push_in_progress`, unchanged |
| 2 | current endpoint hash ≠ recorded (remote targets) | refuse `endpoint_changed`, unchanged |
| 3 | remote unreachable / `ls-remote` fails (remote targets) | refuse `remote_unreachable`, unchanged |
| 4 | target == `S` | `landed` |
| 5 | audit ref `refs/sluice/landed/<S>` exists on the remote | `landed` (note `rewritten_after_landing` if target ∌ `S`) |
| 6 | target ≠ `S`, fetched, and `S` is an ancestor of target | `landed` (someone pushed on top) |
| 7 | target == `base_sha`, no audit ref, `cancel_requested` | `cancelled` |
| 8 | target == `base_sha`, no audit ref | `ready` (approval voided: re-approve to re-push) |
| 9 | target moved elsewhere, no audit ref, `S` not an ancestor | `queued` (train dissolved) |
| 10 | ancestry cannot be determined (fetch fails, objects missing) | refuse `ancestry_unknown`, unchanged |

Every decision deletes the marker except refusals; the pin ref goes with it, and the merge
worktree too except for row 8 (the train must be pushable again). Pin ref and worktree are
removed only after the decision's transaction committed: when that write is refused (the
state or the token changed meanwhile, lease lost) the result is `refused` with the store's
reason and both stay for the next reconcile. Rows 7 and 9 dissolve the train
(`Writer::dissolve`): members with a pending cancel → `cancelled`, the others → `queued` with
`attempts + 1`. Reconcile only reads the remote (`ls-remote` of the push URL, and a `fetch` of it into
`refs/sluice/base/<remote>/<target>` when the target's commit is missing for the ancestry
check, both with the train's `network_timeout`; a timeout is row 3 or 10); it never
pushes. A deleted target branch counts as "moved" (row 9). With the push
atomic, rows 4–6 and 7–9 are exhaustive. The table is `reconcile::decide`, a pure function
over `ReconcileInput`; `reconcile::gather` collects the facts.

## 7. Failure handling

- **Gate failure**: as step 7; `queue status` names the gate, the sluice job and its log (the
  executor writes the usual miss record for the key).
- **Base unavailable or base config invalid** (step 2 fails: the fetch, the local branch or
  the endpoint fails, or `.sluice.toml` of the base commit does not parse): no train is
  built. Every `queued` job of that (repo, remote, target) keeps its state but records
  `reason = base_unavailable` or `base_config_invalid` and `detail = {"error": <the git or
  config error>, "failures": n}`, where `n` counts consecutive failed attempts (a claim or a
  retry clears it). `queue status` shows it on the queued line (`waiting: <reason> <error>`;
  MCP: `waiting: <reason>: <error>`). Attempts are paced: a group whose oldest job failed
  less than one `[queue] tick` ago is skipped, whatever wakes the runner. At the third
  consecutive failure the jobs become `failed` with the same reason and detail
  (`Writer::note_queued_failure`, `runner::MAX_PREPARE_FAILURES`); `queue retry` re-queues
  them with the reason cleared.
- **Merge conflict**: job `conflict` with paths; the agent rebases its branch and calls
  `queue retry <job>`, which re-reads `refs/heads/<branch>` and re-queues (new `queue_order`).
- **Runner death mid-train**: daemon start dissolves `building`/`verifying` trains (jobs
  `queued`, `attempts + 1`) and re-queues any job stranded in `merging`/`verifying` without a
  live train; `ready`/`approved` trains survive the restart (they are re-stamped with the new
  lease token and go on); gate jobs are already handled as orphans by the executor.
- **Runner death mid-push**: daemon start parks every `pushing` train as `needs_reconcile`
  (section 6), whether the marker is present, absent or the push process is still alive (a
  live push is logged, never killed); pin ref keeps `S`.
- **Lease lost**: the runner stops claiming and never starts a push; a running push finishes
  and its result is written only if the fence still matches, otherwise the next daemon parks
  it as `needs_reconcile`.
- **Remote moved**: detected while `ready`/`approved` (dissolve, rebuild, re-approve) or at
  push time through `--force-with-lease` (definitive rejection → dissolve). Local targets:
  the same through the `update-ref` compare-and-swap.
- **`pre-push` hook declined or hung**: declined → jobs `failed` (`pre_push_hook`); hung →
  `push_timeout`, group killed, `needs_reconcile`.
- **Push rejected by policy** (`[remote rejected]`, refused credentials, a refspec git refuses):
  nothing landed, jobs `failed` with reason `push_rejected`. Refused credentials are
  recognised in the pre-push `ls-remote` too (section 6), so a wrong key or password fails
  the approval at once instead of leaving the train `approved` forever. **Local target checked out** in a
  worktree at landing time: jobs `failed` with reason `target_checked_out`.
- **Stalled remote**: every `fetch`/`ls-remote` has the `network_timeout` deadline and runs
  in a child task; a stall during step 2 records `base_unavailable` (`git … timed out after
  …`) like any fetch failure, during the watch or the pre-push check it is an unreachable
  remote (nothing changes, retried next tick), during reconcile row 3/10.
- **SQLite busy**: `StoreError::Busy` (retryable, nothing written); a request gets error code
  `internal` with the message `database busy, retry later`, the runner retries at the next tick;
  never a job state change.

## 8. Protocol additions

Protocol stays **1**. All additions follow the versioning rules (new message types and optional
fields); old clients never send them. To avoid relying on `bad-request` from an old daemon,
`welcome` gains an optional `features: ["queue"]`; a client that does not see it prints
`the running daemon does not support the merge queue; run 'sluice daemon stop' and retry`. A bump would
force every hook client to restart the daemon for a feature most repositories do not use.

Client: `queue-enqueue {repo, worktree, branch, head_sha, base?, caller, session, env, via?}`
(`base` as typed by the user; the daemon resolves it against the repository, section 2), `queue-status {repo?}`, `queue-approve {train_id, train_hash, via: cli|mcp, env}`,
`queue-retry {job_id, head_sha, via?}`, `queue-cancel {job_id, via?}`, `queue-reconcile {repo?}`.
The optional `via` (`cli`|`mcp`, absent = `cli`) is the `queue_events.actor` of the
request. Train views carry the base's `push_timeout_ms` and `network_timeout_ms`.
Server: `queue-job {job}`, `queue-status {schema: 1, trains, jobs}`, `queue-approved {train}`,
`queue-reconcile {results: [{train_id, decision, reason}]}`; errors reuse `error` with new
codes `conflict-state`, `not-allowed`, `stale-plan`, `duplicate-branch` and `not-supported`
(no extension serves the request, or the queue is disabled, section 15). `status` gains an
optional `queue: {active, ready, needs_reconcile}` summary (`active` = jobs not `landed`,
`cancelled`, `conflict` or `failed`; `ready` and `needs_reconcile` count trains). The client resolves the branch,
SHA and cleanliness itself (git in the caller's worktree) and sends facts, like `run` does.

## 9. CLI and MCP surfaces

```
$ sluice queue enqueue                     # current branch of this worktree
sluice: queued feat/login (job 1761…-a1b2c3) for origin/main, position 2
$ sluice queue enqueue feat/api --base origin/release-1.2   # or --base integration (local branch)
$ sluice queue status
train t-1761…-9f2e01  origin/main  base 4e1c9a0  READY (awaiting approval)
  feat/login  job …a1b2c3  merged   gates: test PASS (cached)  lint PASS 12.4s
  feat/ui     job …77d0e4  CONFLICT src/app.ts  → rebase, then 'sluice queue retry …77d0e4'
  approve with: sluice queue approve t-1761…-9f2e01
$ sluice queue approve t-1761…-9f2e01      # prints the plan, asks 'Land this train? [y/N]' on a TTY
sluice: landed 3b7d2f1 on origin/main (train t-1761…-9f2e01)
$ sluice queue retry …77d0e4 | cancel <job> | reconcile | status --json
```

`enqueue` refuses a worktree with staged or unstaged changes to tracked files (untracked files
are ignored), a detached HEAD and a branch that does not exist locally (all in the client);
the daemon refuses a branch already active (`duplicate-branch`), a branch equal to the target
or a checked-out local target (`not-allowed`), and a base that is not a branch (`bad-request`,
section 2). `status` and `reconcile` are scoped to the repository of the current directory
when run inside one (everything otherwise); `retry` must run inside the repository of the
job. `approve` without a TTY requires `--yes` (usage error, exit 2); it always sends the train hash
it displayed (`stale-plan` if the train changed), then waits for the landing at most the
train's `push_timeout` + `network_timeout` + two ticks + 30 s; on expiry it prints the
train's state and exits 1 with the hint `sluice queue status`. The human `status` view
hides a dissolved train once a newer train exists for the same (repo, remote, target);
`--json` keeps every train. Exit codes: 0 ok, 1 error, 2 usage, 4
refused by state (`conflict-state`, `not-allowed`, `stale-plan`, `duplicate-branch`).

MCP (thin clients of the same messages, `working_directory` required, every request
`via: mcp`, errors are tool errors whose text starts with the error code; the client's own
refusals use `dirty-worktree`, `detached-head`, `unknown-branch`, `not-a-repository`):
`queue_enqueue {branch?, base?}`, `queue_status {}` (text +
`structured_content` = `status --json`, scoped to the repository of `working_directory`),
`queue_approve {train_id}` (sends progress notifications while it waits when the request
carries a `progressToken`): returns `not-allowed`
unless the user config has `[queue] allow_agent_approval = true` (checked by the MCP server
and again by the daemon; its wait has the same bound as the CLI's), `queue_retry
{job_id}` (re-reads the branch head like the CLI) and `queue_cancel {job_id}` (both answer
`not-found` for a job of another repository). The server
instructions say: enqueue after `verify` passes, then stop; after a conflict or failure fix
the branch and call `queue_retry` (a new `queue_enqueue` would be `duplicate-branch`);
never approve.

**Agent approval through the CLI** (alpha review finding 2). Without `[queue]
allow_agent_approval = true` the daemon also refuses (`not-allowed`) a `queue-approve` with
`via: cli` whose client environment is detected as an agent's: the CLI sends, in the
existing `env` map of the request, the agent-detection variables that are set
(`SLUICE_AGENT` and those named by `[agent_detection]`) next to the push allowlist, and the
daemon runs `agent::detect` on them with its user config's rules
(`extension::agent_approval_refusal`). Only the allowlisted variables are stored for the
push. No protocol field was added (protocol 1 unchanged): an older client sends only the
allowlist and is treated as a human, as before. The check is a speed bump against an
agent's shell, not a security boundary (an agent controls its environment); the guard's
deny and the forge's protected branches remain.

## 10. Crate and daemon APIs

New crate `sluice-gate-queue` (async, tokio; keeps `sluice-gate-core` sync-only). Depends on
`sluice-gate-core` (config, git, tree, key, marker), `sluice-gate-proto` (message types) and
`sluice-gate-daemon` only through these small public items, added for it:
- `server::Extension` trait (object-safe, methods return `BoxFuture`: `fn features() ->
  &[&'static str]`, `on_start(&DaemonHandle) -> Result<()>` (after the lease and the orphan
  sweep, before the socket accepts; an error disables the extension for the daemon's life:
  its features are not announced and its messages get `error{internal}`), `run(Arc<Self>,
  handle)`, `handle(msg, ctx) -> Option<ServerMessage>` (`None` = not mine, the next
  extension is tried), `status_summary() -> Option<QueueSummary>`), passed in
  `DaemonConfig.extensions`; the binary `sluice-gate` wires `QueueExtension` in, so the daemon
  crate never depends on the queue.
- `DaemonHandle`: `submit_run(RunRequest) -> RunSubscription` (stream of server events; drop
  = unsubscribe), `fence() -> LeaseFence {instance, token}`, `lease_held() -> bool`,
  `shutdown()` (watch), `store_tx(|tx| …)` (`BEGIN IMMEDIATE`, busy → retryable),
  `store_tx_durable(|tx| …)` (the same with `synchronous = FULL`, for the push marker),
  `sanitised_env()`, `state_dir()`, `log_dir()`, `config()`, `daemon_config()`.
- `sluice_gate_core::git::command` (builds the push's `git` command, spawned by the queue in
  its own process group), `git::run_output` (raw output for status-interpreting calls),
  `git::run_output_deadline` (network calls: own process group, killed on expiry,
  `Error::GitTimeout`) and `git::command_error`.
- Public API of the crate: `QueueExtension::new(QueueOptions)` (`QueueOptions.base_env`
  replaces the daemon's sanitised env and `QueueOptions.push_program` the `git` program of
  the push, both for tests only), modules `model`, `store`, `target`,
  `git`, `push`, `reconcile`, `runner`, `extension`.
- Existing helpers: `executor::process_matches`, `proc_start_ticks`, `terminate_group`.

## 11. Configuration

`.sluice.toml` (read from `base_sha`, section 5 step 2):

```toml
[queue]
remote = "origin"          # default "origin"
target = "main"            # default: the remote's HEAD branch
gates = ["test", "lint"]   # default: gates whose hooks contain pre-push, else every gate
max_train = 4              # 1 disables batching
audit_ref = true           # push refs/sluice/landed/<sha> atomically with the target
push_timeout = "10m"       # covers the pre-push hook and the transfer
network_timeout = "60s"    # fetch / ls-remote deadline (process group killed)
```

Validation of `[queue]` (the file is rejected otherwise): `max_train >= 1`, `network_timeout
> 0`, `remote` non-empty, without whitespace or control characters and not starting with `-`,
every name in `gates` defined in the same file; unknown keys are rejected.

User config `[queue]`: `allow_agent_approval = false`, `enabled = true` (a user-level kill
switch), `tick = "5s"`, `retention = "30d"` (section 4). `allow_agent_approval` is only a user-config key: in `.sluice.toml`
it is an unknown field and the strict parser rejects the file, so repository content (which
agents can write) can never grant approval rights.

## 12. Security notes

- Approval is never inferred: no auto-landing, no `--auto` in v1. The approved hash binds
  members' SHAs, base, `S`, `T`, gate keys and endpoint; any drift voids it.
- The daemon cannot tell a human from an agent on its `0600` socket. Defences: approval via
  MCP is off by default; CLI approval prompts on a TTY; `docs/agents.md` recommends a deny
  rule `Bash(sluice queue approve:*)` for agents. An agent with shell access and push
  credentials can still `git push` directly; protected branches on the forge are the real
  enforcement.
- Pushes use the user's own credentials, taken from the approving client's env allowlist
  (`SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`, `SSH_ASKPASS`, `HOME`,
  `PATH`, `TERM`; `sluice_gate_queue::git::ENV_ALLOWLIST`, mirrored by the CLI and MCP clients
before sending), stored with the train (`push_env`) until it lands, is
  dissolved or the approval is voided; the pre-push `ls-remote` and `reconcile` use it too.
  The base fetch and the ready watch use the enqueuing client's
  allowlist (`queue_jobs.env`, cleared once the job is `landed` or `cancelled`); git runs with
  `GIT_TERMINAL_PROMPT=0` and no stdin, so interactive auth fails instead of hanging. Push
  URLs are never stored: only `endpoint_hash`, the hash of the push URL with its userinfo
  stripped, is; the errors of network calls replace the URL by its userinfo-free form
  (`git::hide_userinfo` in `Git::net`), since they land in job details, train notes and the
  daemon log. Stored env values are paths (agent socket, askpass program), never the secret
  they give access to. Confirmed end to end over SSH (unprivileged `sshd`, `ssh-agent`) and
  smart HTTP (`git http-backend`, basic auth, `GIT_ASKPASS`, and a password embedded in the
  URL) by `crates/sluice-gate/tests/queue_transports/`: no private key or password in the
  queue database (with its WAL), `daemon.log`, `client.log`, job logs, `sluice doctor
  --bundle` or the CLI output ([[sluice/changes/queue-transports.md]]). Gates and `[queue]` come from the base. A `[queue]
  remote` is a configured remote name that never starts with `-` (section 2), and
  user-derived positional arguments follow `--`.
- Hooks are disabled in the merge worktree for merge and ref operations. The push runs the
  repository's `pre-push` hook on purpose (user decision): a Sluice-based hook answers from
  the markers; any other hook keeps its say, and a hook that declines fails the train
  rather than being bypassed.

## 13. Acceptance tests

1. Two agents (worktrees A, B) enqueue: A non-conflicting, B conflicting with A. The runner
   merges A, reports B `conflict` with paths, A reaches `ready`; after a wait the local bare
   remote is unchanged; `approve` lands A; B never pushed.
2. SIGKILL the daemon while `git push` blocks (bare remote `pre-receive` waiting on a FIFO):
   restart shows `needs_reconcile`; releasing the hook then `reconcile` → `landed`; aborting
   the hook instead → `ready`. Same test with the marker but no push spawned → `ready`.
3. Train whose combined tree already has markers → `ready` with zero gate processes.
4. Two identical gates requested by a train and an agent `verify` coalesce into one run.
5. Base moved while `ready` → dissolved, rebuilt, old approval rejected (`stale-plan`).
6. `queue_approve` over MCP refused by default; allowed with `allow_agent_approval = true`.
7. After `git remote set-url` the approved train is dissolved with `endpoint_changed` and
   rebuilt, nothing is pushed to either URL without a new approval (`reconcile` also refuses
   a changed endpoint, covered inside test 1); lost lease → no push.
8. Multi-member train with one failing member → split, other members land after approval.

Implemented in `crates/sluice-gate/tests/queue_e2e.rs` (real binary), as
`acceptance_1_conflict_ready_no_push_before_approval_then_landing`,
`acceptance_2a_sigkill_during_push_then_hook_released_reconciles_to_landed`,
`acceptance_2b_sigkill_during_push_then_hook_aborted_reconciles_to_ready`,
`acceptance_3_marked_tree_is_ready_without_running_a_gate`,
`acceptance_4_train_gate_and_agent_run_coalesce`,
`acceptance_5_base_moved_dissolves_old_plan_is_stale_new_one_lands`,
`acceptance_6_mcp_approval_refused_by_default_then_allowed`,
`acceptance_7a_endpoint_changed_never_pushes`, `acceptance_7b_lost_lease_never_pushes` and
`acceptance_8_failing_member_splits_and_the_others_land`; the "marker but no push spawned" case of
test 2 is `marker_without_push_is_parked_at_start_then_ready` in
`crates/sluice-gate-queue/tests/queue.rs`. Extra e2e tests: local branch target and checked-out
refusal, declining `pre-push` hook, `base_unavailable` and `base_config_invalid` failing after
three attempts, MCP retry/cancel after a conflict.

## 14. Decisions (user answers, 2026-10-22)

| # | Question | Answer |
|---|---|---|
| 1 | Landing order | Best-effort batch (section 5); a conflicting job never blocks the others |
| 2 | What "base" is | **Any branch ref**: `<remote>/<branch>` (push) or a local branch (ref update); tags, SHAs and checked-out local branches refused (section 2) |
| 3 | Merge commits or rebase | `--no-ff` merge commits; agents' SHAs and verified trees are preserved |
| 4 | Batching in v1 | Yes, `max_train = 4`, split-to-singles on failure; bisection stays a stretch goal |
| 5 | Config source | Gates and `[queue]` from the base commit, never from a member |
| 6 | Reconcile row 8 | Back to `ready`, approval voided: a new approval is required |
| 7 | Audit ref | Pushed by default, `[queue] audit_ref = false` disables it |
| 8 | Reconcile trigger | Only on command (`sluice queue reconcile`); daemon start only parks trains |
| 9 | `pre-push` hook on push | **The hook runs** (no `--no-verify`); a Sluice hook hits the cache, a declining hook fails the train (section 6) |

Answers 2 and 9 changed the draft; the rest confirmed it.

## 15. Request handling (as implemented)

- `queue-enqueue`: `repo` must be a git common dir and `head_sha` a commit of it; the branch
  must be non-empty and not `HEAD` (`bad-request`); the base is resolved (section 2; a branch
  equal to the target or a checked-out local target → `not-allowed`, any other unusable base
  → `bad-request`). Duplicate active branch
  → `duplicate-branch`. The stored `env` is the allowlist only. Paths are canonicalised.
  The audit actor is `via` (`mcp` from the MCP tools, `cli` otherwise); the same holds for
  `queue-retry` and `queue-cancel`.
- `queue-approve`: `via: mcp` without the user config's `allow_agent_approval` →
  `not-allowed` (checked by the daemon whatever the MCP server did); unknown train →
  `not-found`; a `dissolved` train → `stale-plan` (it was rebuilt; the old plan is void);
  another non-`ready` state → `conflict-state`; a hash that differs from the stored one or
  from the hash recomputed from the rows → `stale-plan`.
- `queue-retry`: unknown job → `not-found`; a `head_sha` that is not a commit → `bad-request`;
  only `conflict`/`failed` jobs (`conflict-state` otherwise); re-queued at the back with the
  new head, `attempts + 1`, `solo`, `reason` and `detail` cleared.
- `queue-cancel`: see section 3; `landed`/`cancelled` → `conflict-state`.
- `queue-reconcile`: one result per `needs_reconcile` train; a refusal reports
  `decision: refused` and its reason.
- Lease lost or a write refused by the fence → `error{internal}`; SQLite busy →
  `error{internal}` ("database busy, retry later"), nothing changed.
- With `[queue] enabled = false` every request gets `error{not-supported}` naming the
  setting (the runner does not step; start-up recovery still runs); `welcome.features` still
  lists `queue`. If the extension's `on_start` failed, it is disabled for the daemon's life:
  no feature is announced and queue requests get `error{internal}`.
- `queue-status`: `repo` limits trains and jobs to one repository (canonicalised).
