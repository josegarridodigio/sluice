---
title: Local merge queue
tags: [sluice, design, merge-queue, phase-6]
---

# Local merge queue (Phase 6)

Status: approved by the user on 2026-10-22 (answers in section 14); implemented in Phase 6. Decision record:
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
  `invalid_remote`, `no_default_target`, `invalid_base` (the worktrees cannot be listed), all with error code `bad-request`, and `own_branch`,
  `target_checked_out`, both `not-allowed`. The columns `remote`/`target` store `""`/`<branch>` for local targets.
- **Train**: the unit the runner builds, verifies, gets approved and pushes: an ordered list of
  jobs merged onto one `base_sha`. It yields a commit `S` (the landing SHA) and its tree `T`.
- **Train hash**: `sha256(repo, remote, endpoint_hash, target, base_sha, [(job_id, head_sha)…],
  S, T, sorted gate keys)`. Shown at approval and stored with it; re-checked before push. Encoding (`model::train_hash`): the
  prefix `sluice-train-v1`, then each field NUL-terminated, a `|` field after the members, and the
  gate keys as `gate=key` (`gate=-` for an uncached gate), sorted. A train built from its member's
  own `.sluice.toml` (`--trust-member-config`, section 5 step 2) appends two more fields after the
  gate keys: `config` and the `config_hash` (`sha256` of the config blob, or `absent`). No other
  train has them, so ordinary hashes are byte-identical to those of earlier releases and a ready
  train's approval survives an upgrade (`TrainHashInput.config_hash`, `None` for ordinary trains).
  A commit that changes the member's file changes `head_sha` and so the train anyway; the config
  hash makes the binding explicit.
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
| queued | failed | 3 consecutive failures to prepare the base (`base_unavailable`, `base_config_invalid`), to derive the keys in the merge worktree (`key_input_failed`), or to run `[queue] prepare` on the base too (`prepare_failed`, `base: true`); at once, with no retry, for a trusted job (`--trust-member-config`) whose branch `.sluice.toml` does not parse (`member_config_invalid`, `detail.error` the parse error) | runner |
| merging | verifying | its merge applied cleanly | runner |
| merging | conflict | `git merge` conflicted (with base or earlier members; reason `conflict`, `merge_failed` or `dirty_merge`); the job leaves the train (`train_id` cleared) | runner |
| merging | failed | internal error while building the train (reason `internal`; a `verifying` job fails the same way) | runner |
| verifying | ready | every gate passed or hit | runner |
| verifying | failed | a gate failed / timed out / was killed / errored and the train has a single surviving member (reason `gate_failed`); the gates passed but left tracked files modified (reason `gate_modified_tree`); `[queue] prepare` failed or left the tree changed while the base prepares (reason `prepare_failed` / `prepare_dirty`, `base: false`; step 4b); or `tree_mismatch` (checked before any gate runs); its merged `.sluice.toml` asks the `pre-push` hook for gates the train does not verify (reason `hook_gate_unverified`) or does not parse (`member_config_invalid`), for the members that changed it (step 5b) | runner |
| verifying | queued | multi-member train failed (`gate_failed`, `gate_modified_tree`, `prepare_failed`, `prepare_dirty`): members split (section 5, steps 4b and 7); or a `key_inputs` command failed in the merge worktree (reason `key_input_failed`) or `[queue] prepare` fails on the base as well (reason `prepare_failed`, `base: true`): train dissolved, members not split (section 7); or another member's merged configuration is rejected (`hook_gate_unverified` / `member_config_invalid`, step 5b): the train is dissolved, the members that did not change it requeued, not split | runner |
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
| conflict, failed | queued | `queue retry <job>` re-reads the branch head; `--trust-member-config` sets the job's `trust_config`, a plain retry clears it | client |
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
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, landed_at INTEGER, note TEXT,
  prepare TEXT);                                     -- JSON {status, job_id, duration_ms, log_path} of the [queue] prepare run; nullable
CREATE TABLE queue_push_markers (
  train_id TEXT PRIMARY KEY, sha TEXT NOT NULL, base_sha TEXT NOT NULL,
  remote TEXT NOT NULL, endpoint_hash TEXT NOT NULL, push_refs TEXT NOT NULL,  -- JSON
  audit_ref TEXT, pin_ref TEXT NOT NULL, claim_token TEXT NOT NULL,
  push_pid INTEGER, push_start_ticks INTEGER, written_at INTEGER NOT NULL);
CREATE TABLE queue_events (                          -- append-only audit trail
  at INTEGER NOT NULL, job_id TEXT, train_id TEXT, from_state TEXT, to_state TEXT NOT NULL,
  actor TEXT NOT NULL, note TEXT);                   -- runner|cli|mcp|reconcile|daemon-start
```

`queue_trains.prepare` is not in the original `CREATE TABLE`: `store::init` adds it to a
database of an earlier release with `PRAGMA table_info` then `ALTER TABLE ... ADD COLUMN`
(idempotent, no `queue_schema` bump; an older binary names its columns and ignores it). It is
the train's view of its prepare run (`TrainPrepare`, an optional `prepare` field of
`QueueTrainView`: `running`, `pass`, `fail`, `timeout`, `killed`, `error`, or `dirty` for a
pass that left the tree changed) and never enters the train hash.

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
   takes up to `[queue] max_train` (default 4) of them by `queue_order` (a `solo` or trusted job only alone, and the batch
   stops before one), creates the train row, moves them to `merging` with
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
   (job `detail = {"config_changed": true}`; `git diff --quiet <base>...<head> -- .sluice.toml`,
   three dots: what the member changed since it forked, not what the base changed meanwhile;
   the flag is merged into the detail, so a later gate failure keeps it; `queue status`, the
   approval plan and MCP show `changes .sluice.toml (this train runs the base's gates)`). A base without `.sluice.toml` has no gates and
   the default `[queue]` settings; an invalid one (or a failed fetch) leaves the jobs `queued`
   with the reason recorded (section 7, "Base unavailable"), retried at most once per tick,
   and `failed` after 3 consecutive failures. `push_timeout`, `audit_ref` and
   `network_timeout` are stored with the train (`settings`).

   **Trusted job** (`queue_jobs.trust_config`, set by a human's `--trust-member-config`,
   ADR-0018 section 6). When the first queued job of the group is trusted, the runner reads
   `git show <head_sha>:.sluice.toml` instead of the base's file: absent means no gates and the
   defaults, an invalid file fails the job at once with `member_config_invalid` (no retry loop,
   no network). The base is still fetched, but its config is not read, so a broken base file does
   not stop the trusted job. Gates, the `[queue] gates` selection, `prepare` and its timeouts,
   `push_timeout`, `network_timeout` and `audit_ref` come from that file; `remote` and `target`
   stay as resolved at enqueue. The claim takes that job alone (`max = 1`), and refuses to claim
   when the first queued job is not the one the config was read for. The train records
   `config_source = member` and `config_hash`; the hash enters the train hash (section 2). The
   prepare attribution probe of step 4b runs the member's own prepare command on the base
   (it compares like with like). The vouch (section 6) needs nothing special: the train's gates
   are what it verified; for a trusted train the refusal never claims "the train runs the base's
   gates" (`config_changed` is ignored there).
3. **Worktree.** `git worktree add --detach <state>/merge/<train-id> <base_sha>`, then
   `git worktree lock --reason "sluice merge queue"` (two steps: `add --lock --reason` needs
   git 2.33, the minimum is 2.31); all git calls with `-c core.hooksPath=/dev/null`, every
   `GIT_*` variable stripped (the client's allowlisted `GIT_SSH_COMMAND` and `GIT_ASKPASS` are
   added back for network calls). Every git process Sluice spawns, the queue's included,
   also carries `-c maintenance.auto=false -c gc.auto=0`
   (`sluice_gate_core::git::NO_AUTO_MAINTENANCE`), so a fetch or merge never starts a repack or
   prune in the user's repository.
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
4b. **Prepare** (only when the base's `[queue]` has `prepare`; otherwise nothing changes).
   A fresh worktree has none of the git-ignored dependencies (`node_modules`, `.venv`) the
   gates need, so the base's `[queue] prepare` command runs once per train, after the merges
   and before the keys (the checks below are in the order the runner does them):
   a. *Fast path.* Take the snapshot (step 5), derive the keys and look them up as in
      steps 5 and 6; if every selected gate hits (or there is no gate) the train goes on
      without preparing: an already verified fast-forward costs nothing, not even an
      installation. A key derivation that fails here is not an error yet: the prepare may be
      what makes it work.
   b. *The job.* Otherwise one `RunRequest` through `submit_run_owned`: gate name
      `queue:prepare` (gate names match `[a-z0-9][a-z0-9_-]*`, so no gate can collide with it;
      the learned weight is keyed by repo, name and command like any gate's), `cmd` =
      `prepare`, `key`/`tree`/`committed_tree` none, `no_marker`, `cache = false`, `weight`
      from `prepare_weight` (resolved like a gate's, `auto` included), `timeout` from
      `prepare_timeout` (always set, never 0), `cwd` = the merge worktree, `child_env` like the
      gates, `caller = human`, `session = "queue:<train-id>"`. The train stays `verifying`
      (no new state); its `prepare` column shows `running` with the job id, then the result.
      It is awaited under the same cancellation rule as the gates: a member cancelled meanwhile
      dissolves the train, and dropping the subscription kills the job.
   c. *Clean tree.* After a pass the worktree must still snapshot to `T` (the same assertion
      as step 5, which also covers untracked files that are not git-ignored); otherwise the
      prepare is `dirty` and handled as a failure with the paths of
      `git status --porcelain`. A prepare may only create ignored files.
   d. *Keys.* The keys are derived after it (a `key_inputs` that needs the installed tools
      works); a key input that still fails is `key_input_failed` (section 7).
   e. *Attribution of a failure* (`fail`, `timeout`, `killed`, `error`, `dirty`). A prepare
      fails because a member broke it (lockfile out of sync) or because of the environment
      (registry down, missing tool, broken base). The runner decides by a fact: it runs the
      same command on the **base alone** in a probe worktree `<state>/merge/<train>-base`
      (`git worktree add --detach` at `base_sha`, locked, hooks disabled, same limits, same
      cleanliness rule; removed afterwards, and swept like a merge worktree: a `-base` path is
      owned by the train it is named for). Nothing is
      remembered: every prepare failure probes the base afresh (a remembered pass would blame
      the members for a later environment failure on the same base: registry down, timeout,
      memory kill). The base fails too: nobody's fault, the train is dissolved and every member
      goes back to `queued` (not `solo`) with reason `prepare_failed` and `detail = {"error",
      "job_id", "status", "exit_code", "log_path", "message", "base": true, "base_job_id",
      "failures": n}` (`paths` too for a dirty prepare), paced and counted like
      `base_unavailable`, `failed` after 3 attempts. The base prepares: a member's fault; a
      single member is `failed` with `prepare_failed` (`prepare_dirty` for a dirty tree,
      `paths` in the detail) and `base: false`; several split into solo trains, exactly like
      a gate failure (the `Failure` of step 7). When the probe cannot be set up (the probe
      worktree cannot be created) there is no fact to attribute by and the train is requeued
      as an environment failure.
5. **Key.** `T = git rev-parse S^{tree}`. Take `tree::snapshot_entries` of the clean worktree (the same
   code path as every client) and assert it equals `T` (a mismatch, e.g. from eol filters,
   makes the train fail with `tree_mismatch`, never silently uncached). For each queue gate
   (`[queue] gates` plus every gate whose `hooks` contain `pre-push`, else the `pre-push`
   gates, else every gate: the `[queue] gates` order first, then the added `pre-push` gates by
   name, no duplicates; `runner::select_gates`; the push's hook runs the `pre-push` gates and
   the daemon answers them from the train, section 6, so the train must have verified them)
   derive the key with the shared `lookup::key_tree` + `lookup::derive_key_at` from `T` (or, for a gate with `paths`/`paths_ignore`, the scope hash of the snapshot entries; [[sluice/design/path-scoped-keys.md]] section 7), the base config's gate
   (name, normalised cmd, env), `key_inputs` run in the merge worktree and `key_files` read
   there (no extra arguments; a gate with `cache = false` has no key, always runs and enters
   the hash as `<gate>=-`). A `key_inputs` command (or `key_files` entry) that fails in the
   merge worktree is reason `key_input_failed`, an environment failure (section 7), never
   `internal`. Markers live in the repo's common dir, so keys computed by agents' `verify` calls
   on the same tree hit: a fast-forwarded single-member train that the agent already
   verified needs no run at all.
   *Hook gates (step 5b, `runner::reject_hook_gates`, `hookcheck`).* Right after the snapshot
   assertion and before preparing or running anything, the runner parses the **merged**
   worktree's `.sluice.toml` (the file the push's `pre-push` hook will read) and compares
   `gates_for_hook("pre-push")` with the gate names the train selected (those of the config the
   train uses: the base's, or the member's for a trusted train, which cannot mismatch). A gate
   the train will not verify would make the hook fail fast after a human approved, so the
   train ends at once: members whose job detail has `config_changed` fail with
   `hook_gate_unverified` (detail `{"gates": [...], "hint"}`: land the configuration change
   first, or a human retries with `--trust-member-config`), the other members are requeued
   (dissolved, not split). A merged config that does not parse fails the `config_changed`
   members with `member_config_invalid` and the parse error. No gate ran and no approval was
   needed. If no member changed the configuration the check is skipped (nobody to blame).
6. **Run through Sluice.** `MarkerStore::lookup` per key; all hits (or no gate at all, e.g. a base without `.sluice.toml`) →
   `ready` immediately.
   Otherwise submit one `RunRequest` per missing gate through `DaemonHandle::submit_run`
   (section 10): `repo.common_dir` = the real common dir, `toplevel`/`cwd` = merge worktree,
   `key`/`tree` = derived, `committed_tree = T`, `caller = human`, `session =
   "queue:<train-id>"`, `child_env` = sanitised daemon env. Jobs enter the scheduler like any
   other (weight, budget, repo cap, coalescing, double-check before start, marker on pass).
   The runs are submitted with `DaemonHandle::submit_run_owned`: dropping the subscription
   (the first failure, a cancelled member, daemon stop) cancels the job whatever
   `last_subscriber_gone` says, since the worktree it runs in is about to be removed.
   Gates run concurrently; the first failure unsubscribes the rest. Gate statuses in the
   train view: `cached` (marker hit before any run), `running` (with the sluice job id),
   then `pass`, `cached` (hit found by the scheduler), `fail`, `timeout`, `killed`, `error`.
   A `pass` whose outcome says `marker_written = false` with a reason (typically "the gate
   modified the tree": it left untracked files) carries the optional `TrainGate.note =
   "not cached: <reason>"`, shown as `pass (not cached: …)` by `queue status` and MCP; the
   train goes on. The note never enters the train hash (only the gate names and keys do).
   After every gate that had to run passed, the runner checks `git status --porcelain
   --untracked-files=no` of the merge worktree: a non-empty answer means a gate edited
   tracked files, which the pre-push check would dissolve and the rebuild repeat, so the train
   fails with reason `gate_modified_tree` (detail `{"gates": [the gates that ran], "paths":
   [...]}`) and is handled exactly like a gate failure (step 7).
7. **Outcome.** Decided in one write transaction (`runner::finish_train`), which re-reads
   the members: a member that left `verifying` meanwhile (cancelled) dissolves the train
   and re-queues the others (never an `internal` failure). All pass/cached → train `ready`,
   `train_hash` computed, `queue status` shows the plan. A failure in a train with a single surviving member → job `failed`, reason `gate_failed` (detail:
   `gate`, `job_id`, `status`, `exit_code`, `log_path`, `message`) or `gate_modified_tree`
   (detail: `gates`, `paths`), merged into any detail the job already carries
   (`config_changed`). A failure with several surviving members → v1 splits: every member returns to `queued`
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
   regular push from the merge worktree with the repository's `core.hooksPath`. A
   Sluice-based hook (`sluice run --hook pre-push <gate>`) is **answered by the daemon from
   the train's record**, not by a re-derived cache key (ADR-0018 section 3; before, the hook
   derived its key in the push's environment, where `key_inputs` such as `node --version`
   follow the approver's `PATH` and `CI` can turn the cache off, missed, and re-ran the whole
   suite inside `push_timeout`). `Extension::vouch_run` is consulted for every `run` request
   that carries a `hook` (and only those, so `verify` and plain runs pay nothing) before the
   scheduler or the marker double-check; the queue answers when **all** hold:
   - a train is `pushing` whose merge worktree is the request's `toplevel` and whose
     repository is its `common_dir` (both canonicalised);
   - the peer process (`SO_PEERCRED`) descends from the marker's `push_pid` (walk of the
     `/proc/<pid>/stat` parent chain, at most 64 steps, stopping at pid 1) and that pid still
     has the marker's `push_start_ticks` (no pid reuse); a push pid not yet recorded is
     waited for up to 2 s (it is written right after the spawn);
   - the daemon's own check of the worktree gives `HEAD == S` and no change to tracked
     files (`status --porcelain --untracked-files=no`, the rule of the pre-push checks:
     untracked output a gate left does not change what is pushed; run off the async
     threads).

   Then the reply is one `done` (no `queued` first; the client reads until `done`): a gate the
   train verified (`pass` or `cached` in its gate views) gets `cached`, exit 0, message
   `<gate>: verified by merge train <id> (job <job>)`, no marker written; any other gate gets
   `error` with `gate '<g>' runs in the pre-push hook but merge train <id> did not verify
   it` and a hint (a member changes `.sluice.toml` and the train runs the base's gates; else add
   the gate to `[queue] gates` or give it the `pre-push` hook). The hook exits non-zero at
   once, so the push fails definitively as `pre_push_hook` (the hook's output, message
   included, is the job's detail) instead of timing out into `needs_reconcile`. A worktree
   that no longer holds `S`/`T` is answered with `error` too (`the merge worktree of train <id>
   no longer holds the verified tree`). Any other condition that does not hold (no such
   train, peer not a descendant of the push, a database or git error) sends the request the
   normal way. The daemon logs `push hook vouched: train=.. gate=.. verdict=..` at `info`.
   Why this is safe: it states a fact about the daemon's own locked worktree, which holds
   exactly the verified `S`, checked by process ancestry and by the daemon's own snapshot; it
   lets nobody skip a gate anywhere else (a `sluice run` from any other process or directory
   goes the normal way), and hooks that are not Sluice's keep their say. It works with older
   hook clients (daemon side only; the client prints the message on stderr from this
   version). A hook that declines (`git` exits 1 with no ref line in the porcelain
   output, no `fatal:` line, no git `error:` line other than `failed to push some refs`,
   which must be present; the hook's stderr is captured) is a definitive rejection:
   jobs `failed` with reason `pre_push_hook` and the hook output in `detail`; the marker is
   deleted. The push has `[queue] push_timeout` (default `10m`); on expiry the process
   group is terminated (`terminate_group`, SIGTERM then SIGKILL after 2 s), the process is
   reaped, the readers of its pipes get a bounded 2 s to drain, and the train goes to
   `needs_reconcile` with note `push_timeout: <last 20 lines the push printed>` (just
   `push_timeout` when it printed nothing; at most 4000 characters), like any other
   ambiguous end. Train settings stored without a field (an older row) or unreadable fall
   back to the real `[queue]` defaults (10 min, `audit_ref`, 60 s) with a `warn` log, never to
   zeros.
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
- **Key inputs fail in the merge worktree** (`key_inputs` exits non-zero or cannot run, a
  `key_files` entry cannot be read; reason `key_input_failed`): an environment failure,
  nobody's fault, and every member would hit it. The train is dissolved (not split, not solo)
  and its members go back to `queued` with `reason = key_input_failed`, `detail = {"error":
  "gate <name>: <error>", "failures": n}`, through the same machinery as a base that cannot
  be prepared: paced one attempt per `[queue] tick`, counted across the attempts (a claim
  after a `key_input_failed` keeps the count; deriving the keys clears it), `failed` with the
  same reason after the third (`Writer::note_queued_failure`,
  `runner::MAX_PREPARE_FAILURES`). `queue status` shows `waiting: key_input_failed <error>`.
- **`[queue] prepare` fails** (step 4b): `prepare_failed` / `prepare_dirty`. Probed on the
  base; the base fails too: members requeued (not solo) with `prepare_failed`, `base: true`,
  paced and counted like a key-input failure (a claim after a `prepare_failed` keeps the
  count; deriving the keys clears it), `failed` after 3; the base prepares: the single member
  is `failed`, a pair splits. `queue status` shows `prepare: pass 41.2s` /
  `prepare: running (job <id>)` / `prepare: fail (job <id>)` on the train and `waiting:
  prepare_failed <error> (see 'sluice logs <job>')` on a requeued job. Edge cases: a prepare
  that dirties the tree also on the base is an environment failure (requeued with the
  paths); every failure probes the base again (no probe is remembered), which costs one prepare
  per failure; with no
  gate selected the fast path always holds, so a base without gates never prepares.
- **The merged config asks the `pre-push` hook for unverified gates** (`hook_gate_unverified`,
  step 5b): found at build time, with no gate run and no approval; only the members that
  changed `.sluice.toml` fail, the others are requeued. `queue status` shows the gate names
  and the hint (CLI and MCP).
- **A gate leaves tracked files modified** (`gate_modified_tree`): see step 6; the member
  fails, or the train splits, like for a failing gate. Untracked or ignored output only
  produces the `not cached` note.
- **Invalid `.sluice.toml` on the enqueued head**: the head's config only picks the default
  target. An enqueue without `--base` is refused with `bad-request` and a message starting
  `invalid_config:`; with `--base` the head's config is not read. A file that is absent
  gives the built-in defaults, as before.
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
`welcome` gains an optional `features` array that lists `queue` (a 0.3.0 daemon sends
`["exec","queue-only","queue"]`); a client that does not see it prints
`the running daemon does not support the merge queue; run 'sluice daemon stop' and retry`. A bump would
force every hook client to restart the daemon for a feature most repositories do not use.

Client: `queue-enqueue {repo, worktree, branch, head_sha, base?, caller, session, env, via?}`
(`base` as typed by the user; the daemon resolves it against the repository, section 2), `queue-status {repo?}`, `queue-approve {train_id, train_hash, via: cli|mcp, env}`,
`queue-retry {job_id, head_sha, via?, trust_member_config?, env?}`, `queue-cancel {job_id, via?}`, `queue-reconcile {repo?}`.
`queue-enqueue` and `queue-retry` gain the optional boolean `trust_member_config` (omitted when
false); `queue-retry` also gains the optional `env` (the agent-detection variables, sent only with
the flag). The daemon announces the feature `queue-trust-config` in `welcome.features`; a client
that does not see it refuses the flag instead of sending a request an old daemon would silently
ignore. Train views gain the optional `config_source` (`"member"` only for a trusted train) and
`config_diff` (the unified diff `git diff <base_sha>...<head> -- .sluice.toml`, or, when the base has
no such file, a note followed by the whole member file; capped at 64 KiB with a truncation note;
computed by `queue-status` for trusted trains that are `verifying` or `ready`).
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
$ sluice queue enqueue --trust-member-config   # humans only, also on retry (section 12)
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
`--json` keeps every train. Jobs outside any train are listed under `queued:` (with
`waiting: <reason> <error>` when the base cannot be prepared) and `jobs needing attention:`;
landed and cancelled jobs are not shown, and an empty queue prints `sluice: the merge queue is
empty`. `reconcile` prints `train <id>: <decision> (<reason>)` per train, or `sluice: no
train needs reconciling`. `enqueue` sends `caller` from the agent detection of the user config
and no `via` (recorded as `cli`). Every request except `enqueue`, `approve` and `reconcile`
has a 60 s reply timeout; a `queue-status` whose connection drops before any reply is sent
once more after making sure a daemon runs (requests with effects are never retried). Exit codes: 0 ok, 1 error, 2 usage, 4
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
never approve. The MCP tools do not offer `trust_member_config`; `queue_status` text prints
`CONFIG FROM BRANCH` for a trusted train and says that the diff is in `sluice queue status` and in
the structured `config_diff`.

**`--trust-member-config`** (human bootstrap, section 12). `sluice queue enqueue
--trust-member-config` and `sluice queue retry <job> --trust-member-config` send the flag together
with the agent-detection variables (like the approval) and require the daemon feature. `queue
status` prints, under the train, `CONFIG FROM BRANCH <branch>: this train runs the branch's own
.sluice.toml` and the indented diff; the approval plan prints the same before the TTY prompt;
`--yes` still works for a human without a TTY. A trusted train's job lines never say "this train runs
the base's gates".

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
gates = ["test", "lint"]   # plus every gate whose hooks contain pre-push (the push's hook runs them);
                           # default: those gates, else every gate
max_train = 4              # 1 disables batching
audit_ref = true           # push refs/sluice/landed/<sha> atomically with the target
push_timeout = "10m"       # covers the pre-push hook and the transfer
network_timeout = "60s"    # fetch / ls-remote deadline (process group killed)
prepare = "pnpm install --frozen-lockfile --prefer-offline"  # optional, needs 0.6.0: once per train, before the gates
prepare_timeout = "10m"    # default 10m, > 0
prepare_weight = "auto"    # integer >= 1 or "auto", like a gate's weight
```

Validation of `[queue]` (the file is rejected otherwise): `max_train >= 1`, `network_timeout
> 0`, `remote` non-empty, without whitespace or control characters and not starting with `-`,
every name in `gates` defined in the same file, `prepare` not empty after trimming,
`prepare_timeout > 0`, `prepare_weight >= 1` or `"auto"`; unknown keys are rejected (so a
binary older than 0.6.0 rejects a file that sets `prepare*`).

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
- **Human bootstrap of the configuration** (ADR-0018 section 6). Gates and `[queue]` come from
  the base so that a branch cannot choose its own judges; if the base's configuration makes every
  train fail, the only way out was a direct push. `--trust-member-config` builds one job's train from
  the member's file instead. It is stricter than approval: the daemon refuses it with `not-allowed`
  for `via: mcp`, for a CLI request whose environment is detected as an agent's (the same
  `agent::detect` as the approval, `extension::trust_refusal`), and for an enqueue that declares
  `caller: agent`, **regardless of `allow_agent_approval`**. Approving a train whose
  `config_source` is `member` has the same rule, even when agents may approve ordinary trains. The
  job is alone in its train, the diff of the file is shown before the prompt, and the config hash
  binds the approval. Like all environment checks it stops an agent's honest shell, not a hostile one
  (an agent controls its environment): deny the commands in the agent's permissions too.
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
  repository's `pre-push` hook on purpose (user decision): a Sluice-based hook is answered
  from the train's own record (section 6) and any other hook keeps its say; a hook that
  declines fails the train rather than being bypassed. That answer is not a way around a
  gate: it is given only to a process descending from the daemon's own push, in the
  daemon's own worktree, while the daemon's snapshot of it is the verified tree, and it
  names the train and job that ran the gate; a gate the train did not run is refused.

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
| 9 | `pre-push` hook on push | **The hook runs** (no `--no-verify`); a Sluice hook is answered by the daemon from the train (ADR-0018, section 6), a declining hook fails the train |

| 10 | Dead end when the base's configuration cannot pass (2026-10-26) | **Human bootstrap**: `--trust-member-config` on `enqueue`/`retry` builds that job's train from the member's `.sluice.toml`; alone, diff shown, refused to agents and MCP whatever `allow_agent_approval` says; a plain retry clears it (ADR-0018 section 6) |

Answers 2 and 9 changed the draft; the rest confirmed it. Answer 5 has the single, explicit,
human-only exception of answer 10.

## 15. Request handling (as implemented)

- `queue-enqueue`: `repo` must be a git common dir and `head_sha` a commit of it; the branch
  must be non-empty and not `HEAD` (`bad-request`); the base is resolved (section 2; a branch
  equal to the target or a checked-out local target → `not-allowed`, any other unusable base
  → `bad-request`). With `trust_member_config`, first refused with `not-allowed` for `via: mcp`, an
  agent-detected `env` or `caller: agent` (`allow_agent_approval` does not matter). Duplicate active branch
  → `duplicate-branch`. The stored `env` is the allowlist only. Paths are canonicalised.
  The audit actor is `via` (`mcp` from the MCP tools, `cli` otherwise); the same holds for
  `queue-retry` and `queue-cancel`.
- `queue-approve`: `via: mcp` without the user config's `allow_agent_approval` →
  `not-allowed` (checked by the daemon whatever the MCP server did); unknown train →
  `not-found`; a `dissolved` train → `stale-plan` (it was rebuilt; the old plan is void);
  a train with `config_source = member` through MCP or an agent-detected CLI → `not-allowed`
  (whatever `allow_agent_approval` says, checked before the state);
  another non-`ready` state → `conflict-state`; a hash that differs from the stored one or
  from the hash recomputed from the rows → `stale-plan`.
- `queue-retry`: unknown job → `not-found`; a `head_sha` that is not a commit → `bad-request`;
  only `conflict`/`failed` jobs (`conflict-state` otherwise); re-queued at the back with the
  new head, `attempts + 1`, `solo`, `reason` and `detail` cleared, `trust_config` set to the
  request's `trust_member_config` (so a plain retry clears it; the flag is refused like an
  enqueue's, with the request's `env`).
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
