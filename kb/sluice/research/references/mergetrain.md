# Reference: mergetrain (yongjip/mergetrain)

Source studied: `/home/jose/Escritorio/Proyectos/sluice/refs/mergetrain` at commit `dd0316f`
(2026-10-19, "release: prepare mergetrain 3.7.0"). Paths below are relative to that root.

## 1. What it is

mergetrain is a local merge queue for parallel LLM coding agents working in Git worktrees.
Each agent commits and enqueues its branch; one lease-fenced runner merges the queued branches
in enqueue order into a disposable integration worktree, runs configured "gates" (tests/checks)
once over the combined tree, and, only after explicit approval, pushes the result atomically to
the configured refs (README.md:12-17, README.md:76-79). It keeps all state in a local SQLite
database and never talks to a hosted control plane (README.md:21-24).

- License: MIT (LICENSE:1, pyproject.toml:11).
- Version: 3.7.0, Python >= 3.10, machine contract 5, config schema 2, queue schema 16
  (pyproject.toml:6-8, docs/contract.md:30, src/mergetrain/persistence/schema.py:12).

## 2. Core model

**Enqueue.** `mergetrain enqueue --task T --branch B` resolves the unique live worktree that has
`B` checked out via Git's own worktree registry (`commands/queue.py:81-116`), refuses dirty
worktrees and worktrees from another repository (`commands/queue.py:52-78`), and captures
`base_sha` (integration tracking ref) and `head_sha` (`refs/heads/B`) itself
(`commands/queue.py:125-126`). Agents never copy SHAs (docs/quickstart.md:76-77). A branch
with an active job is rejected (`duplicate_active_branch`, docs/contract.md:165). All linked
worktrees share one SQLite queue and one runner lock in the control checkout (docs/design.md:39-44).

**Combined tree.** The runner creates a detached worktree
`{project}-mergetrain-{job_id}-{random8}` under `state.worktree_root`
(`worktree_manager.py:33-36`), fetches, then `git merge --no-edit <exact sha>` per job in FIFO
order (`git_runner.py:1139-1168`). A conflicting job is marked `blocked`, `git merge --abort`
runs, and the train continues (`git_runner.py:1169-1187`); a merge that leaves the tree dirty is
blocked and the worktree is reset to the pre-merge tip so it cannot ride the train
(`git_runner.py:1188-1211`). Gates run once over the whole train; on failure, multi-job trains
are bisected/probed to find an individually failing job or a "semantic conflict" pair, and the
survivors are re-run as a fresh train (docs/design.md:386-413). Before push, the runner asserts
HEAD is still the gated SHA and the tree is clean — gates must not mutate the tree
(`atomic_push.py:64-86`). Two modes: default `batch` (combine what is queued) and `ordered`
(claim only the earliest unresolved `queue_order`) (README.md:108-124, docs/design.md:20-27).

**"Lease-fenced single runner."** There is one `runner` row in the `locks` table with
`owner`, `heartbeat_at`, `expires_at`, and a random `token` (schema.py:62-71, design.md:126-128).
Acquisition, job selection, and the `queued -> in_progress` transition happen in one
`BEGIN IMMEDIATE` transaction (`persistence/claims.py:111-224`, design.md:369-373). Every claimed
job stores the same `claim_token` (`claims.py:57-65`). The token is the fencing token: every
lease refresh is `UPDATE locks ... WHERE name=? AND owner=? AND token=?` and raises `LostLease`
when `rowcount != 1` (`leases.py:341-365`); every job state write is compare-and-swap on the
claim token (`git_runner.py:171-184`, `persistence/recovery.py:79-123`). Lease TTL defaults to
30 minutes and the heartbeat to 10 s; config refuses a heartbeat >= TTL (`config.py:84-89`,
`config.py:775-785`). Heartbeats are driven by a "pulse" callback inside the managed subprocess
loop (`command_runner.py:376-397`); subprocesses run in their own session/process group
(`command_runner.py:333`) so a pulse that raises `LostLease` kills the whole group with
SIGTERM-then-SIGKILL (`command_runner.py:402-404`, `command_runner.py:233-254`). Tokens are never
serialized to CLI/MCP output (design.md:302-306).

**Interrupted push recovery.** Before `git push`, the runner records a fsync-durable write-ahead
marker (`pending_deploy_sha`, remote, push refs, credential-free endpoint hash, deployment id,
verification policy hash) on the claimed rows, fenced by claim token and refused if a cancel was
requested (`persistence/recovery.py:47-123`; durability via `PRAGMA synchronous = FULL`,
`connection.py:174-178`), and creates a local pin ref `refs/mergetrain/pending/<job_id>` so the
commit survives `git gc` (`atomic_push.py:202-221`, `git_ops.py:353-371`). The push is
`git push --atomic --no-follow-tags --recurse-submodules=no --force-with-lease=<audit>:<expected>`
of the SHA to every payload ref plus a permanent audit ref `refs/mergetrain/deploys/<sha>`
(`atomic_push.py:106-121`, design.md:447-452). A definitive rejection clears the marker; any
other failure after the marker raises `AmbiguousPush` and parks the job `needs_reconcile`
(`atomic_push.py:307-331`, `git_runner.py:1022-1025`). `reconcile` asks the recorded endpoint
(refusing if its hash changed) whether every push ref contains the SHA, and decides
`deployed` / `queued` / `canceled` / `blocked`, never guessing on unresolvable ancestry
(`recovery.py:337-392`, `recovery.py:160-214`). The rationale "Git alone cannot distinguish
never-pushed from pushed-then-died" is in docs/failure-modes.md:400-423.

**Approval.** Interactive `deploy` prints the exact plan and requires `y`/`yes` on a TTY
(`commands/deploy.py:405-415`); the plan hash covers train identity, destination, and execution
policy (`deploy_plan.py:81-99`) and is re-checked inside the claim transaction and before push
(`commands/deploy.py:177-214`, docs/agent-contract.md:29-32). Unattended daemons only touch jobs
enqueued with `--auto`, whose stored destination and execution-policy hashes must still match at
claim and before push (`claims.py:149-202`, docs/daemon.md:16-35). Over MCP, `mergetrain_deploy`
uses client elicitation with a confirmation field that is not in the model-visible schema
(docs/mcp.md:57-73, `mcp_server.py:855-895`).

## 3. Concurrency and locking primitives

| Primitive | Where | Notes |
| --- | --- | --- |
| SQLite WAL + `busy_timeout=5000` + `synchronous=FULL` | `persistence/connection.py:161-178` | one DB per repo, in-repo `.mergetrain/` with a self-managed `.gitignore` (`connection.py:71-113`) |
| `BEGIN IMMEDIATE` for every write; busy -> typed retryable `QueueBusy` | `persistence/transactions.py:36-70` | contention is never reported as job failure (docs/failure-modes.md:228-237) |
| Deferred read transaction for consistent multi-SELECT snapshots | `transactions.py:73-96` | status/hub builders |
| Read-only observer connection (`mode=ro`) with WAL sidecar bootstrap | `connection.py:13-68`, `connection.py:120-160` | hub never migrates/creates another repo's DB |
| Runner lock row with owner `{user}:{pid}`, TTL, heartbeat, uuid4 token | `leases.py:26-27`, `leases.py:289-320` | token is the fencing token for all later writes |
| PID liveness: `os.kill(pid, 0)`; `ProcessLookupError`->dead, `PermissionError`->alive, other `OSError`->unknown; Windows uses `OpenProcess` because signal 0 is Ctrl-C there | `leases.py:66-84`, `leases.py:30-63` | |
| Takeover policy: dead owner -> reclaim + split orphans; unexpired lease -> refuse (even if PID unknown); expired lease with no `in_progress` -> clear (guards recycled PIDs); expired with `in_progress` -> refuse, human must `unlock` | `leases.py:264-286`, docs/failure-modes.md:295-311 | |
| Marker-aware orphan split: cancel+no marker -> canceled; marker -> `needs_reconcile`; else -> `queued` and validated-train identity dissolved | `leases.py:157-220` | runs inside the acquiring transaction |
| Daemon detects its own stale lease (same owner at tick start) and force-clears it | `daemon.py:203-208`, `daemon.py:221-222` | |
| Daemon tick probes read-only first; writable connection only when there is work | `daemon.py:160-166`, `daemon.py:175-219` | |
| Per-push `flock` file `push-locks/<sha>.lock` inherited by `git push` via `pass_fds`; `reconcile` refuses `lock_held` while any inheriting process is alive | `push_liveness.py:103-139`, `atomic_push.py:122-143`, `recovery.py:469-481` | Windows: named Job Object + keeper process (`push_liveness.py:17-22`) |
| Managed subprocess: new session, pulse every heartbeat interval, timeout, SIGTERM then SIGKILL of the group | `command_runner.py:333`, `command_runner.py:376-404`, `command_runner.py:233-254` | |
| Schema upgrade re-reads `user_version` under `BEGIN IMMEDIATE`; newer DB fails closed | `schema.py:115-136` | |

## 4. Agent integration

- CLI is the only engine; six public verbs `init status enqueue validate deploy inspect`
  (README.md:360-366). `status --json` returns `health`, `state`, and an exact `next_action`
  so agents never infer the next command (docs/contract.md:85-97). All JSON carries
  `contract_version`; errors use one envelope with stable `error.code` and `retryable`
  (docs/contract.md:46-75, 154-179).
- MCP: optional `mergetrain[mcp]` extra, stdio, five tools that shell out to the CLI `--json`
  and return the payload verbatim; daemon, `--auto`, gc, cancel, unlock, recovery are
  deliberately absent (`mcp_server.py:1-15`, docs/mcp.md:17-29). Server stops the CLI's
  process group on cancellation with a 20 s grace (`mcp_server.py:83-90`, docs/mcp.md:111-123).
- Plugins: Claude Code and Codex plugins ship a SKILL.md plus a pinned `uvx --from
  "mergetrain[mcp]==3.7.0" mergetrain mcp` server (`integrations/claude/plugin/.mcp.json`,
  `plugins/mergetrain/.mcp.json`, `mcp_config.json`); agy uses `plugin.json`. No Claude Code
  hooks (PreToolUse etc.) are used anywhere (grep over `integrations/`, `plugins/`, `skills/`).
- Guidance: `init --write` generates `AGENTS.mergetrain.md` and `CLAUDE.mergetrain.md` sidecars
  from one rendered contract, refreshed with `init --refresh-instructions`
  (`commands/setup.py:95-96`, `:121-130`, docs/quickstart.md:45-55). The protocol block
  (five rules: task branch, clean commit, read status, enqueue by task+branch only, stop after
  enqueue / never push integration refs) is embedded between generated markers in CLAUDE.md:9-47,
  skills/mergetrain/SKILL.md:14-52, and docs/agent-contract.md:3-14. AGENTS.md:6-51 adds repo
  rules for contributors (retry not re-enqueue, never rewrite `refs/mergetrain/deploys/*`).
- Authority model: "queue for validation" authorizes enqueue only; deploy needs a human-readable
  plan confirmation or prior bounded `--auto` approval; agents never choose train IDs or plan
  hashes (docs/agent-contract.md:22-40).

## 5. Ideas relevant to Sluice

**Now (daemon single-instance, stale detection, fencing):**
- Lease row = owner `user:pid` + heartbeat + expiry + random token; fence every later write
  with `WHERE token = ?` (`leases.py:341-365`) — cheap, and a stale daemon cannot clobber state.
- Keep the SQLite `BEGIN IMMEDIATE` claim atomic with job selection (`claims.py:111-224`) —
  avoids the TOCTOU they explicitly closed (`claims.py:113-120`).
- Liveness triage dead/alive/unknown from the PID with the `PermissionError -> alive` rule
  (`leases.py:66-84`) — honest on multi-user machines.
- Never steal an unexpired lease, even from an unknown PID; reclaim an expired lease only when
  no work is in progress (`leases.py:264-286`) — defends against PID reuse without a human.
- Validate heartbeat < TTL at config load (`config.py:782-785`) — trivial misconfig guard.
- Drive heartbeats from the subprocess polling loop and kill the whole process group on lost
  lease (`command_runner.py:333`, `:395-404`) — gates cannot outlive their lease.
- Translate SQLite busy into a typed retryable error distinct from "job failed"
  (`transactions.py:36-70`) — contention must never cache a false negative.
- Read-only probe before any writable open in the daemon tick (`daemon.py:160-166`) — idle
  ticks never create or migrate state.
- The daemon notices a lease under its own owner at tick start and clears it
  (`daemon.py:203-208`) — self-healing after an exception path missed release.
- Hub/observer connections open `mode=ro` and refuse other schema versions
  (`connection.py:120-160`) — a cross-repo status view that cannot corrupt.
- Strip `GIT_DIR`/`GIT_INDEX_FILE` etc. from child env (`command_runner.py:450-480`) — a runner
  started from a hook would otherwise operate on the wrong worktree.

**Later (merge-queue phase):**
- Write-ahead marker + local pin ref before the irreversible push, with fsync
  (`persistence/recovery.py:47-123`, `connection.py:174-178`) — only way to tell
  "never pushed" from "pushed then died".
- Content-addressed permanent audit ref pushed atomically with `--force-with-lease`
  (`atomic_push.py:106-121`) — lets reconcile detect "landed then rewritten".
- Endpoint hash stored with the marker; reconcile fails closed if the remote now resolves
  elsewhere (`recovery.py:356-360`) — config drift cannot redirect recovery.
- `flock` inherited by `git push` to detect a push that outlived its runner
  (`push_liveness.py:8-16`) — reconcile must not read the remote mid-push.
- Train identity hashes (member heads, base, tree, gate policy, env) bound to approval
  (design.md:112-118, `deploy_plan.py:81-99`) — changed branches cannot reuse an approval.
- Batch mode with failure isolation/bisection and re-run of survivors (design.md:386-413) —
  unrelated work lands while one agent repairs.
- Path-scoped gates that fail closed on diff errors (docs/security.md:160-172) — safe skip.
- `retry` replaces a blocked job atomically and preserves `queue_order` in ordered mode
  (design.md:20-27) — no duplicate-branch bookkeeping.
- Five-tool MCP that is a veneer over `--json`, with deploy confirmation outside the model
  schema (`mcp_server.py:1-15`, docs/mcp.md:57-73) — one contract, no fabricated consent.

## 6. Pitfalls they document

- Gates that mutate the checkout (bytecode, caches) make the train block after a passing gate;
  keep the tree clean or the pushed commit differs from the tested one
  (docs/best-practices.md:203-221, README.md:209-210).
- SQLite contention (`queue_busy`) does not mean nothing was pushed; reread status and
  reconcile if a marker exists (docs/failure-modes.md:228-264).
- A killed runner does not kill `git push`; a late push can still land after reconcile read the
  remote on network remotes (docs/failure-modes.md:162-186, `push_liveness.py:24-25`).
- Helper daemons spawned by the push (credential cache, pre-push hooks) can hold the push lock;
  `lsof` the lock file or delete it (docs/failure-modes.md:188-200).
- Leases do not stop an agent with shell access and a push credential from bypassing the queue;
  enforce via separate deploy identity and protected branch (README.md:137-152,
  docs/security.md:3-31).
- A recycled PID can look alive; that is why expired leases with no in-progress work are
  reclaimed anyway (docs/failure-modes.md:307-309).
- Requeuing an orphaned in-progress row dissolves its validated-train identity on purpose; the
  note records it (`leases.py:193-200`, docs/failure-modes.md:266-271).
- `os.kill(pid, 0)` on Windows sends a real Ctrl-C (`leases.py:31-35`).
- A `*` `.gitignore` in a shared state dir would fake a clean worktree; only ignore exact queue
  artifacts there (`connection.py:71-89`).
- Operator `.mergetrain.yaml` drift from the integration ref silently changes gates; status
  reports `config_drift` (docs/best-practices.md:321-338).
- Validation reuse needs environment fingerprints; command text alone is not an identity
  (docs/security.md:144-158).
- Sandboxed full test suites can hit `PermissionError` on localhost sockets; rerun outside the
  sandbox once instead of repeating (AGENTS.md:64-69).
