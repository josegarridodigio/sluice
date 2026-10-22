---
title: Local merge queue
tags: [sluice, design, merge-queue, phase-6]
---

# Local merge queue (Phase 6)

Status: draft, awaiting the user's approval before any code. Decision record:
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

- **Job**: one queued branch: `(repo, branch, head_sha, remote, target)`. The SHA is captured
  by Sluice at enqueue time; later commits on the branch are not part of the job.
- **Train**: the unit the runner builds, verifies, gets approved and pushes: an ordered list of
  jobs merged onto one `base_sha`. It yields a commit `S` (the landing SHA) and its tree `T`.
- **Train hash**: `sha256(repo, remote, endpoint_hash, target, base_sha, [(job_id, head_sha)…],
  S, T, sorted gate keys)`. Shown at approval and stored with it; re-checked before push.
- **Lease**: the daemon's existing lease row (`lease.name = 'daemon'`, random token). The runner
  only works while the heartbeat succeeds; every queue write is a compare-and-swap on
  `claim_token = <lease token>` and the expected `state`.
- **Pin ref**: local ref `refs/sluice/pending/<train-id>` → `S`, created before the marker so
  the commit survives `git gc` and the merge worktree's removal until reconcile.
- **Write-ahead marker**: row in `queue_push_markers` written (and fsynced, `synchronous=FULL`)
  in the same transaction that moves the train to `pushing`. Its presence is the only evidence
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
| verifying | ready | every gate passed or hit | runner |
| verifying | failed | a gate failed / timed out / was killed (single-member train) | runner |
| verifying | queued | multi-member train failed: members split (section 5, step 7) | runner |
| ready | approved | `queue approve <train>` with matching train hash | human (CLI) or MCP if allowed |
| ready, approved | queued | base moved, member cancelled or retried, config drift; approval voided | runner |
| approved | pushing | marker + pin ref written, lease re-checked | runner |
| pushing | landed | push exit 0 | runner |
| pushing | queued | definitive rejection, lease mismatch on target (remote moved) | runner |
| pushing | failed | definitive rejection for permission/policy | runner |
| pushing | needs_reconcile | ambiguous push failure, or daemon start finds a marker | runner / daemon start |
| needs_reconcile | landed / ready / queued / cancelled | decision table, section 6 | `queue reconcile` |
| conflict, failed | queued | `queue retry <job>` re-reads the branch head | client |
| any pre-`pushing` state | cancelled | `queue cancel <job>`; in `merging`/`verifying` the train is rebuilt without it | client |
| merging, verifying | queued | daemon start finds an unfinished train (it is dissolved) | daemon start |

`pushing` cannot be cancelled: `cancel` sets `cancel_requested`, honoured only if reconcile
proves nothing landed. Terminal states: `landed`, `cancelled`; `conflict` and `failed` are
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
  enqueued_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER);
CREATE UNIQUE INDEX queue_jobs_active_branch ON queue_jobs(repo, branch)
  WHERE state NOT IN ('landed','cancelled');
CREATE TABLE queue_trains (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL, remote TEXT NOT NULL, target TEXT NOT NULL,
  endpoint_hash TEXT NOT NULL, base_sha TEXT NOT NULL, state TEXT NOT NULL,
  merge_sha TEXT, merge_tree TEXT, worktree TEXT NOT NULL,
  gates TEXT,                                        -- JSON [{gate,key,job_id,status}]
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

One tokio task per daemon, woken by enqueue/retry/approve/cancel messages and a 5 s tick;
it does nothing while the lease is lost or the daemon is stopping. Per (repo, remote, target)
at most one train is active.

1. **Claim.** In one `BEGIN IMMEDIATE`: verify the lease token, pick up to `max_train`
   `queued` jobs of the oldest (repo, target) by `queue_order`, create the train row, move
   them to `merging` with `claim_token`. No train is built while an older train of the same
   target is `ready`/`approved`/`pushing`/`needs_reconcile`.
2. **Trusted config.** `git fetch --no-tags <remote> +refs/heads/<target>:refs/sluice/base/<target>`
   (env: sanitised daemon env plus the enqueuer's allowlist), `base_sha` = that ref. Read
   `[queue]` and the gate definitions from `git show <base_sha>:.sluice.toml`, never from a
   member branch. A member that changes `.sluice.toml` is flagged `config_changed` in the plan.
3. **Worktree.** `git worktree add --detach --lock <state>/merge/<train-id> <base_sha>`
   (`--reason "sluice merge queue"`); all git calls with `-c core.hooksPath=/dev/null`, every
   `GIT_*` variable stripped.
4. **Merge.** For each member in order: if the train has one member and `base_sha` is an
   ancestor of `head_sha`, fast-forward (`S = head_sha`); otherwise
   `git merge --no-ff --no-edit -m "Merge <branch> (sluice job <id>)" <head_sha>`. On conflict:
   record the conflicting paths, `git merge --abort`, mark the job `conflict`, continue. A
   merge that leaves the tree dirty is treated as a conflict and reset. No member left → the
   train is `dissolved`. Survivors → `verifying`.
5. **Key.** `T = git rev-parse S^{tree}`. Take `tree::snapshot` of the clean worktree (the same
   code path as every client) and assert it equals `T` (a mismatch, e.g. from eol filters,
   makes the train fail with `tree_mismatch`, never silently uncached). For each queue gate
   (section 9) derive the key with `sluice_gate_core::key` from `T`, the base config's gate
   (name, normalised cmd, env), `key_inputs` run in the merge worktree and `key_files` read
   there. Markers live in the repo's common dir, so keys computed by agents' `verify` calls
   on the same tree hit: a fast-forwarded single-member train that the agent already
   verified needs no run at all.
6. **Run through Sluice.** `MarkerStore::lookup` per key; all hits → `ready` immediately.
   Otherwise submit one `RunRequest` per missing gate through `DaemonHandle::submit_run`
   (section 10): `repo.common_dir` = the real common dir, `toplevel`/`cwd` = merge worktree,
   `key`/`tree` = derived, `committed_tree = T`, `caller = human`, `session =
   "queue:<train-id>"`, `child_env` = sanitised daemon env. Jobs enter the scheduler like any
   other (weight, budget, repo cap, coalescing, double-check before start, marker on pass).
   Gates run concurrently; the first failure unsubscribes the rest.
7. **Outcome.** All pass/cached → train `ready`, `train_hash` computed, `queue status` shows
   the plan. A failure in a single-member train → job `failed` (detail: gate, sluice job id,
   log path). A failure in a multi-member train → v1 splits: every member returns to `queued`
   with the flag `solo`, so the next trains contain one job each (linear isolation). Stretch
   (optional, not v1): bisection of the failing train and re-run of the survivors as a fresh
   train, plus detection of a semantic-conflict pair, as in mergetrain.
8. **Wait.** `ready` waits for approval. Each tick re-checks cheaply: a member cancelled, or
   `ls-remote <remote> refs/heads/<target>` ≠ `base_sha` → train dissolved, members `queued`,
   approval voided (rebuilt on the new base; the gates may still hit).
9. **Push** (only `approved` trains, section 6). Then `landed`, pin ref deleted, worktree
   removed (`git worktree remove --force`), base ref left in place. Worktrees of dissolved or
   failed trains are removed at once; `<state>/merge/` leftovers are swept at daemon start.

## 6. Push protocol and reconcile

Before push: re-verify lease, train hash = `approved_hash`, `HEAD == S` and a clean tree in
the merge worktree, `endpoint_hash` unchanged, `ls-remote` target == `base_sha`. Then:

1. `git update-ref refs/sluice/pending/<train-id> S` (pin ref).
2. One transaction: insert `queue_push_markers` (`synchronous=FULL`), train and jobs →
   `pushing`, fenced on `claim_token` and `cancel_requested = 0`.
3. Spawn, in its own process group, with the approver's env allowlist:
   `git push --atomic --porcelain --no-verify --no-follow-tags --recurse-submodules=no
   --force-with-lease=refs/heads/<target>:<base_sha> --force-with-lease=refs/sluice/landed/<S>:
   <remote> <S>:refs/heads/<target> <S>:refs/sluice/landed/<S>`; record `push_pid` and
   `push_start_ticks` (`executor::proc_start_ticks`) in the marker right after spawn.
4. Exit 0 → `landed`, marker kept as history (`outcome` in `queue_events`), pin ref deleted.
   Porcelain shows a definitive rejection (`stale info`, `non-fast-forward`, `rejected`,
   permission denied) → marker deleted; stale lease → train dissolved, jobs `queued`;
   policy/permission → jobs `failed`. Anything else (signal, network error, unparsable) →
   `needs_reconcile`.

Daemon start: markers whose train is still `pushing` → `needs_reconcile`; a push process
still alive (`executor::process_matches(pid, ticks)`) is **not** killed and is reported.

`sluice queue reconcile` evaluates every `needs_reconcile` train, first row that matches wins:

| # | Condition | Decision |
|---|---|---|
| 1 | recorded push process still alive | refuse `push_in_progress`, unchanged |
| 2 | current endpoint hash ≠ recorded | refuse `endpoint_changed`, unchanged |
| 3 | remote unreachable / `ls-remote` fails | refuse `remote_unreachable`, unchanged |
| 4 | target == `S` | `landed` |
| 5 | audit ref `refs/sluice/landed/<S>` exists on the remote | `landed` (note `rewritten_after_landing` if target ∌ `S`) |
| 6 | target ≠ `S`, fetched, and `S` is an ancestor of target | `landed` (someone pushed on top) |
| 7 | target == `base_sha`, no audit ref, `cancel_requested` | `cancelled` |
| 8 | target == `base_sha`, no audit ref | `ready` (approval voided: re-approve to re-push) |
| 9 | target moved elsewhere, no audit ref, `S` not an ancestor | `queued` (train dissolved) |
| 10 | ancestry cannot be determined (fetch fails, objects missing) | refuse `ancestry_unknown`, unchanged |

Every decision deletes the marker except refusals; the pin ref goes with it. Reconcile only
reads the remote; it never pushes. With the push atomic, rows 4–6 and 7–9 are exhaustive.

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
  push time through `--force-with-lease` (definitive rejection → dissolve).
- **SQLite busy**: typed retryable error, never a job state change.

## 8. Protocol additions

Protocol stays **1**. All additions follow the versioning rules (new message types and optional
fields); old clients never send them. To avoid relying on `bad-request` from an old daemon,
`welcome` gains an optional `features: ["queue"]`; a client that does not see it prints
`the running daemon does not support the merge queue; run 'sluice daemon stop'`. A bump would
force every hook client to restart the daemon for a feature most repositories do not use.

Client: `queue-enqueue {repo, worktree, branch, head_sha, remote?, target?, caller, session,
env}`, `queue-status {repo?}`, `queue-approve {train_id, train_hash, via: cli|mcp, env}`,
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
$ sluice queue enqueue feat/api --base release-1.2
$ sluice queue status
train t-1761…-9f2e01  origin/main  base 4e1c9a0  READY (awaiting approval)
  feat/login  job …a1b2c3  merged   gates: test PASS (cached)  lint PASS 12.4s
  feat/ui     job …77d0e4  CONFLICT src/app.ts  → rebase, then 'sluice queue retry …77d0e4'
  approve with: sluice queue approve t-1761…-9f2e01
$ sluice queue approve t-1761…-9f2e01      # prints the plan, asks 'yes' on a TTY
sluice: landed 3b7d2f1 on origin/main (train t-1761…-9f2e01)
$ sluice queue retry …77d0e4 | cancel <job> | reconcile | status --json
```

`enqueue` refuses a dirty worktree, a detached HEAD, a branch already active and a branch
equal to the target. `approve` without a TTY requires `--yes`; it always sends the train hash
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
  `shutdown()` (watch), `store_tx(|tx| …)` (`BEGIN IMMEDIATE`, busy → retryable).
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
  (`SSH_AUTH_SOCK`, `GIT_ASKPASS`, `SSH_ASKPASS`, `HOME`, `PATH`), never stored beyond the
  train. Endpoint URLs are stored without userinfo. Gates and `[queue]` come from the base.
- Hooks are disabled in the merge worktree and on push (`--no-verify`): the gates already ran.

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

## 14. Open questions for the user

1. Landing order: strict enqueue order (a conflicting head blocks nothing, but a failed
   head is retried in place) or best-effort batch (the v1 draft)?
2. What is "base": the remote target branch (draft: `--base` names that branch) or any ref?
3. Merge commits (`--no-ff`, draft) or rebase/cherry-pick members onto the base (linear
   history, but rewritten SHAs and the agents' verified trees no longer match)?
4. Batching in v1 (`max_train = 4` with split-to-singles) or one job per train first?
   Bisection stays a stretch goal either way.
5. Gates and `[queue]` from the base commit (draft, safe) or from the combined tree?
6. Reconcile row 8: return to `ready` needing a new approval (draft) or keep the approval?
7. Push the audit ref `refs/sluice/landed/<sha>` by default (some forges reject custom refs)?
8. Should the daemon run `reconcile` itself at start, or only on command (draft)?
9. Push with `--no-verify` (draft) or run the user's pre-push hook?
