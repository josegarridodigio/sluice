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
  remote's HEAD branch (`refs/remotes/<remote>/HEAD`, else `ls-remote --symref`). The
  `[queue]` defaults used to pick the default target are read from the enqueued head's
  `.sluice.toml` (built-in defaults when absent or invalid); gates never are (step 2).
  Refused: tags, raw SHAs, `HEAD`, the job's own branch (a target branch with the job's
  branch name, local or remote), and a local branch checked out in any worktree
  (`target_checked_out`: updating it would desync that worktree, which product code never
  touches). The refusal message starts with a token: `head`, `tag`, `sha`, `unknown_base`,
  `no_default_target` (error code `bad-request`), `own_branch`, `target_checked_out`
  (`not-allowed`). The columns `remote`/`target` store `""`/`<branch>` for local targets.
- **Train**: the unit the runner builds, verifies, gets approved and pushes: an ordered list of
  jobs merged onto one `base_sha`. It yields a commit `S` (the landing SHA) and its tree `T`.
- **Train hash**: `sha256(repo, remote, endpoint_hash, target, base_sha, [(job_id, head_sha)…],
  S, T, sorted gate keys)`. Shown at approval and stored with it; re-checked before push.
- **Lease**: the daemon's existing lease row (`lease.name = 'daemon'`, random token). The runner
  only works while the heartbeat succeeds; every queue write is a compare-and-swap on
  `claim_token = <lease token>` and the expected `state`.
- **Pin ref**: local ref `refs/sluice/pending/<train-id>` → `S`, created before the marker so
  the commit survives `git gc` and the merge worktree's removal until reconcile.
- **Write-ahead marker**: row in `queue_push_markers` written in the same transaction that
  moves the train to `pushing`, committed with `synchronous = FULL` through
  `DaemonHandle::store_tx_durable` (the daemon's connection otherwise runs `NORMAL`). Its presence is the only evidence
  that a push may have happened; Git alone cannot tell "never pushed" from "pushed then died".
- **Endpoint binding**: `endpoint_hash = sha256(git ls-remote --get-url <remote>)` with userinfo
  stripped. Recorded in the train hash and the marker; push and reconcile refuse when the
  remote now resolves to another URL (`endpoint_changed`).
- **Audit ref**: remote ref `refs/sluice/landed/<S>` pushed atomically with the target, so
  reconcile can tell "landed and later rewritten" from "never landed".

## 3. Job state machine

Happy path: `queued → merging → verifying → ready → approved → pushing → landed`.
Side exits: `conflict`, `failed`, `cancelled`, `needs_reconcile`. Exact transitions:

| From | To | Trigger | Who |
|---|---|---|---|
| — | queued | `queue enqueue` | CLI / MCP client (agent or human) |
| queued | merging | claim into a train (one `BEGIN IMMEDIATE` with job selection) | runner |
| merging | verifying | its merge applied cleanly | runner |
| merging | conflict | `git merge` conflicted (with base or earlier members) | runner |
| merging | failed | internal error while building the train (reason `internal`) | runner |
| verifying | ready | every gate passed or hit | runner |
| verifying | failed | a gate failed / timed out / was killed (single-member train), or `tree_mismatch` | runner |
| verifying | queued | multi-member train failed: members split (section 5, step 7) | runner |
| ready | approved | `queue approve <train>` with matching train hash | human (CLI) or MCP if allowed |
| ready, approved | queued | base moved, member cancelled or retried, config drift; approval voided | runner |
| approved | pushing | marker + pin ref written, lease re-checked | runner |
| approved | failed | local target checked out at landing time (reason `target_checked_out`) | runner |
| pushing | landed | push exit 0 | runner |
| pushing | queued | definitive rejection, lease mismatch on target (remote moved) | runner |
| pushing | failed | definitive rejection for permission/policy | runner |
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
`TRAIN_TRANSITIONS`); the store refuses any other transition. Terminal states: `landed`, `cancelled`; `conflict` and `failed` are
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
  settings TEXT,                                     -- JSON {push_timeout_ms, audit_ref} of the base
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

## 5. Runner algorithm

One tokio task per daemon, woken by every `queue-*` request and by the user config's
`[queue] tick` (default 5 s); it does nothing while the lease is lost, the daemon is stopping
or `[queue] enabled = false`. Per (repo, remote, target) at most one train is active. Building
a train (steps 3–7) and pushing it (step 9) run as child tasks of the runner, so a long gate
never delays approvals or the watch of ready trains; on daemon stop they are aborted (the
next start dissolves an unfinished build and parks an unfinished push).

1. **Claim.** The runner picks the (repo, remote, target) of the oldest `queued` job whose
   target has no active train, runs step 2 for it (fetch, config), then in one
   `BEGIN IMMEDIATE`: verifies the lease token, re-reads the queued jobs of that target and
   takes up to `max_train` of them by `queue_order` (a `solo` job only alone, and the batch
   stops before a `solo` job), creates the train row, moves them to `merging` with
   `claim_token`. No train is built while an older train of the same target is
   `building`/`verifying`/`ready`/`approved`/`pushing`/`needs_reconcile`.
2. **Trusted config.** Remote target: `git fetch --no-tags <remote>
   +refs/heads/<target>:refs/sluice/base/<remote>/<target>` (env: sanitised daemon env plus
   the enqueuer's allowlist), `base_sha` = that ref. Local target: `base_sha` =
   `refs/heads/<target>` (no fetch; later "ls-remote" checks read the local ref). Read
   `[queue]` and the gate definitions from `git show <base_sha>:.sluice.toml`, never from a
   member branch. A member that changes `.sluice.toml` is flagged `config_changed` in the plan
   (job `detail = {"config_changed": true}`). A base without `.sluice.toml` has no gates and
   the default `[queue]` settings; an invalid one (or a failed fetch) leaves the jobs `queued`
   and is logged, retried at the next tick. `push_timeout` and `audit_ref` are stored with
   the train (`settings`).
3. **Worktree.** `git worktree add --detach --lock <state>/merge/<train-id> <base_sha>`
   (`--reason "sluice merge queue"`); all git calls with `-c core.hooksPath=/dev/null`, every
   `GIT_*` variable stripped.
4. **Merge.** For each member in order: if the train has one member and `base_sha` is an
   ancestor of `head_sha`, fast-forward (`git checkout --detach <head_sha>`, `S = head_sha`);
   otherwise `git merge --no-ff --no-edit -m "Merge <branch> (sluice job <id>)" <head_sha>`
   (with `-c user.name="Sluice merge queue" -c user.email=sluice@localhost` only when git has
   no committer identity). On conflict: record the conflicting paths, `git merge --abort`,
   mark the job `conflict` (reason `conflict`, detail `{"paths": [...]}`; `merge_failed` when
   git failed without conflicting paths), continue. A merge that leaves the tree dirty is
   reset and the job gets reason `dirty_merge`. No member left → the train is `dissolved`.
   Survivors → `verifying`. The train keeps every claimed member in `members`; the hash uses
   the survivors.
5. **Key.** `T = git rev-parse S^{tree}`. Take `tree::snapshot` of the clean worktree (the same
   code path as every client) and assert it equals `T` (a mismatch, e.g. from eol filters,
   makes the train fail with `tree_mismatch`, never silently uncached). For each queue gate
   (`[queue] gates`, else the gates whose `hooks` contain `pre-push`, else every gate)
   derive the key with `sluice_gate_core::key` from `T`, the base config's gate
   (name, normalised cmd, env), `key_inputs` run in the merge worktree and `key_files` read
   there (no extra arguments; a gate with `cache = false` has no key, always runs and enters
   the hash as `<gate>=-`). Markers live in the repo's common dir, so keys computed by agents' `verify` calls
   on the same tree hit: a fast-forwarded single-member train that the agent already
   verified needs no run at all.
6. **Run through Sluice.** `MarkerStore::lookup` per key; all hits → `ready` immediately.
   Otherwise submit one `RunRequest` per missing gate through `DaemonHandle::submit_run`
   (section 10): `repo.common_dir` = the real common dir, `toplevel`/`cwd` = merge worktree,
   `key`/`tree` = derived, `committed_tree = T`, `caller = human`, `session =
   "queue:<train-id>"`, `child_env` = sanitised daemon env. Jobs enter the scheduler like any
   other (weight, budget, repo cap, coalescing, double-check before start, marker on pass).
   Gates run concurrently; the first failure unsubscribes the rest. Gate statuses in the
   train view: `cached` (marker hit before any run), `running` (with the sluice job id),
   then `pass`, `cached` (hit found by the scheduler), `fail`, `timeout`, `killed`, `error`.
7. **Outcome.** All pass/cached → train `ready`, `train_hash` computed, `queue status` shows
   the plan. A failure in a single-member train → job `failed`, reason `gate_failed` (detail:
   `gate`, `job_id`, `status`, `exit_code`, `log_path`, `message`). A failure in a multi-member train → v1 splits: every member returns to `queued`
   with the flag `solo`, so the next trains contain one job each (linear isolation). Stretch
   (optional, not v1): bisection of the failing train and re-run of the survivors as a fresh
   train, plus detection of a semantic-conflict pair, as in mergetrain.
8. **Wait.** `ready` waits for approval. Each tick re-checks cheaply: a member cancelled, or
   `ls-remote <remote> refs/heads/<target>` (the local ref for a local target; enqueuer's env)
   ≠ `base_sha` → train dissolved, members `queued` (`attempts + 1`), approval voided (rebuilt
   on the new base; the gates may still hit). An unreachable remote changes nothing.
9. **Push** (only `approved` trains, section 6). Then `landed`, pin ref deleted, worktree
   removed (`git worktree remove --force --force`, the worktree is locked; fallback: delete
   the directory and `git worktree prune`), base ref left in place. Worktrees of dissolved or
   failed trains are removed at once (and by the runner's housekeeping on every tick);
   `<state>/merge/` leftovers not owned by an active train are swept at daemon start.

## 6. Push protocol and reconcile

Before push: re-verify lease, train hash (recomputed from the rows) = `approved_hash` and no
pending cancel (else dissolved, note `stale_plan`), `HEAD == S` and a clean tree in the merge
worktree, `endpoint_hash` unchanged (else dissolved with note `endpoint_changed`, so the train
is rebuilt against the new URL), for a local target that it is not checked out (else jobs
`failed`, reason `target_checked_out`), `ls-remote` target == `base_sha` (else dissolved,
`target moved`). A remote that cannot be reached leaves the train `approved` and the check is
retried at the next tick. Then:

1. `git update-ref refs/sluice/pending/<train-id> S` (pin ref).
2. One transaction (`DaemonHandle::store_tx_durable`, `synchronous = FULL`): insert
   `queue_push_markers`, train and its `approved` members → `pushing`, fenced on the lease,
   `claim_token` and `cancel_requested = 0`. A refused transaction deletes the pin ref.
3. Spawn, in its own process group, with the approver's env allowlist:
   `git push --atomic --porcelain --no-follow-tags --recurse-submodules=no
   --force-with-lease=refs/heads/<target>:<base_sha> --force-with-lease=refs/sluice/landed/<S>:
   <remote> <S>:refs/heads/<target> <S>:refs/sluice/landed/<S>`; record `push_pid` and
   `push_start_ticks` (`executor::proc_start_ticks`) in the marker right after spawn. The
   push env is the daemon's env without `GIT_*`, plus `LC_ALL=C` (stderr is parsed) and
   `GIT_TERMINAL_PROMPT=0`, plus the approver's allowlist.
   The repository's `pre-push` hook **runs** (user decision, section 14): the push is a
   regular push from the merge worktree with the repository's `core.hooksPath`, so a
   Sluice-based hook answers from the markers the train just produced (the daemon's env
   carries no agent marker, so the human policy `wait` applies and the hook never
   fails fast). A hook that declines (`git` exits 1 with no ref line in the porcelain
   output, no `fatal:` line and `failed to push some refs` on stderr; the hook's stderr is
   captured) is a definitive rejection:
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
   update; reconcile then applies rows 4 to 9 against the local refs.
4. Exit 0 → `landed`, marker kept as history (`outcome: landed` in `queue_events`), pin ref
   deleted, worktree removed. Classification (`push::classify`, on the line of
   `refs/heads/<target>`): `[rejected]` (`stale info`, `non-fast-forward`, `fetch first`) →
   the target moved: marker deleted, train dissolved, jobs `queued`; `[remote rejected]`
   (server hook, protected branch) or "permission denied" (also before any ref line, e.g. SSH
   auth) → jobs `failed`, reason `push_rejected`; the `pre-push` hook declined → jobs `failed`,
   reason `pre_push_hook`, detail `{"output": ...}`. A job with a pending cancel ends
   `cancelled` instead of `failed`/`queued`. Anything else (signal, timeout, network error,
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
worktree too except for row 8 (the train must be pushable again). Row 7 dissolves the train:
members with a pending cancel → `cancelled`, the others → `queued`. Row 9 dissolves it with
every member `queued`. Reconcile only reads the remote (`ls-remote`, and a `fetch` into
`refs/sluice/base/<remote>/<target>` when the target's commit is missing for the ancestry
check); it never pushes. A deleted target branch counts as "moved" (row 9). With the push
atomic, rows 4–6 and 7–9 are exhaustive. The table is `reconcile::decide`, a pure function
over `ReconcileInput`; `reconcile::gather` collects the facts.

## 7. Failure handling

- **Gate failure**: as step 7; `queue status` names the gate, the sluice job and its log (the
  executor writes the usual miss record for the key).
- **Merge conflict**: job `conflict` with paths; the agent rebases its branch and calls
  `queue retry <job>`, which re-reads `refs/heads/<branch>` and re-queues (new `queue_order`).
- **Runner death mid-train**: daemon start dissolves `merging`/`verifying` trains (jobs
  `queued`, `attempts + 1`); gate jobs are already handled as orphans by the executor.
- **Runner death mid-push**: marker present → `needs_reconcile` (section 6); pin ref keeps `S`.
- **Lease lost**: the runner stops claiming and never starts a push; a running push finishes
  and its result is written only if the fence still matches, otherwise the next daemon parks
  it as `needs_reconcile`.
- **Remote moved**: detected while `ready`/`approved` (dissolve, rebuild, re-approve) or at
  push time through `--force-with-lease` (definitive rejection → dissolve). Local targets:
  the same through the `update-ref` compare-and-swap.
- **`pre-push` hook declined or hung**: declined → jobs `failed` (`pre_push_hook`); hung →
  `push_timeout`, group killed, `needs_reconcile`.
- **SQLite busy**: typed retryable error, never a job state change.

## 8. Protocol additions

Protocol stays **1**. All additions follow the versioning rules (new message types and optional
fields); old clients never send them. To avoid relying on `bad-request` from an old daemon,
`welcome` gains an optional `features: ["queue"]`; a client that does not see it prints
`the running daemon does not support the merge queue; run 'sluice daemon stop'`. A bump would
force every hook client to restart the daemon for a feature most repositories do not use.

Client: `queue-enqueue {repo, worktree, branch, head_sha, base?, caller, session, env}`
(`base` as typed by the user; the daemon resolves it against the repository, section 2), `queue-status {repo?}`, `queue-approve {train_id, train_hash, via: cli|mcp, env}`,
`queue-retry {job_id, head_sha}`, `queue-cancel {job_id}`, `queue-reconcile {repo?}`.
Server: `queue-job {job}`, `queue-status {schema: 1, trains, jobs}`, `queue-approved {train}`,
`queue-reconcile {results: [{train_id, decision, reason}]}`; errors reuse `error` with new
codes `conflict-state`, `not-allowed`, `stale-plan`, `duplicate-branch`. `status` gains an
optional `queue: {active, ready, needs_reconcile}` summary. The client resolves the branch,
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
$ sluice queue approve t-1761…-9f2e01      # prints the plan, asks 'yes' on a TTY
sluice: landed 3b7d2f1 on origin/main (train t-1761…-9f2e01)
$ sluice queue retry …77d0e4 | cancel <job> | reconcile | status --json
```

`enqueue` refuses a dirty worktree, a detached HEAD, a branch already active, a branch
equal to the target, and a base that is not a branch (section 2). `approve` without a TTY requires `--yes`; it always sends the train hash
it displayed (`stale-plan` if the train changed). Exit codes: 0 ok, 1 error, 2 usage, 4
refused by state (`conflict-state`, `not-allowed`, `stale-plan`).

MCP (thin clients of the same messages, `working_directory` required): `queue_enqueue
{branch?, base?}`, `queue_status {}` (text + `structured_content` = `status --json`),
`queue_approve {train_id}`: returns `not-allowed` unless the user config has
`[queue] allow_agent_approval = true` (checked by the MCP server and again by the daemon).
The server instructions say: enqueue after `verify` passes, then stop; never approve.

## 10. Crate and daemon APIs

New crate `sluice-gate-queue` (async, tokio; keeps `sluice-gate-core` sync-only). Depends on
`sluice-gate-core` (config, git, tree, key, marker), `sluice-gate-proto` (message types) and
`sluice-gate-daemon` only through these small public items, added for it:
- `server::Extension` trait (`fn features() -> &[&str]`, `async fn handle(msg, ctx) ->
  ServerMessage`, `async fn run(handle)`), passed in `DaemonConfig.extensions`; the binary
  `sluice-gate` wires `QueueExtension` in, so the daemon crate never depends on the queue.
- `DaemonHandle`: `submit_run(RunRequest) -> RunSubscription` (stream of server events; drop
  = unsubscribe), `fence() -> LeaseFence {instance, token}`, `lease_held() -> bool`,
  `shutdown()` (watch), `store_tx(|tx| …)` (`BEGIN IMMEDIATE`, busy → retryable),
  `store_tx_durable(|tx| …)` (the same with `synchronous = FULL`, for the push marker),
  `sanitised_env()`, `state_dir()`, `config()`, `daemon_config()`.
- `sluice_gate_core::git::command` (builds the push's `git` command, spawned by the queue in
  its own process group) and `git::run_output` (raw output for status-interpreting calls).
- Public API of the crate: `QueueExtension::new(QueueOptions)` (`QueueOptions.base_env`
  replaces the daemon's sanitised env, for tests), modules `model`, `store`, `target`,
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
```

User config `[queue]`: `allow_agent_approval = false`, `enabled = true` (a user-level kill
switch), `tick = "5s"`. `allow_agent_approval` is ignored in `.sluice.toml` with a warning:
repository content is agent-writable.

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
  `PATH`, `TERM`; `git::ENV_ALLOWLIST`), stored with the train (`push_env`) until it lands or
  the approval is voided. Fetches and `ls-remote` use the enqueuing client's allowlist
  (`queue_jobs.env`). Endpoint URLs are stored without userinfo. Gates and `[queue]` come from the base.
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
7. `endpoint_changed` refusal after `git remote set-url`; lost lease → no push.
8. Multi-member train with one failing member → split, other members land after approval.

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
  must be non-empty and not `HEAD`; the base is resolved (section 2). Duplicate active branch
  → `duplicate-branch`. The stored `env` is the allowlist only. Paths are canonicalised.
- `queue-approve`: `via: mcp` without the user config's `allow_agent_approval` →
  `not-allowed` (checked by the daemon whatever the MCP server did); unknown train →
  `not-found`; a `dissolved` train → `stale-plan` (it was rebuilt; the old plan is void);
  another non-`ready` state → `conflict-state`; a hash that differs from the stored one or
  from the hash recomputed from the rows → `stale-plan`.
- `queue-retry`: only `conflict`/`failed` jobs (`conflict-state` otherwise); re-queued at the
  back with the new head, `attempts + 1`, `solo` cleared.
- `queue-cancel`: see section 3; `landed`/`cancelled` → `conflict-state`.
- `queue-reconcile`: one result per `needs_reconcile` train; a refusal reports
  `decision: refused` and its reason.
- Lease lost or a write refused by the fence → `error{internal}`; SQLite busy →
  `error{internal}` ("database busy, retry later"), nothing changed.
- With `[queue] enabled = false` every request gets `error{not-supported}` naming the
  setting; `welcome.features` still lists `queue`.
