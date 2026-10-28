---
title: "Queue Q4b: land through a pull request (ADR-0025 3-7)"
tags: [sluice, changes, queue, forge, github, pull-request, issue-5]
---

# Queue Q4b: `land = "pull-request"`

Wave Q4b of [[sluice/plan/queue-scheduling-plan.md]] (S3). Decision:
[[sluice/design/adr/0025-protected-targets-and-pull-request-landing.md]] items 3 to 7 (the user's
decisions of 2026-10-28 are binding); research:
[[sluice/research/protected-targets-analysis.md]] sections 1, 3 and 4.2; builds on
[[sluice/changes/queue-q4a-protected-targets.md]]. Status: implemented; verification through Sluice
pending (the lead runs `fmt`, `clippy` and `test`).

## What changed

1. **Config.** `[queue] land = "push" | "pull-request"` (`Land`, `QueueConfig.land`,
   `UserQueueConfig.land`, both `Option`). `Land::effective(base, user)` is the only place the
   precedence lives: the target commit's value, else the user config's, else `push`. The runner
   computes it in `prepare_and_claim` from `base_config` only (`train_land`), so a queued branch's
   file, a trusted member's included, never sets it; it is stored in `TrainSettings.land`
   (`#[serde(default)]`, older rows read as `push`). One-way key; fixtures `queue-land.toml` and
   `queue-land-user.toml` under `crates/sluice-gate-core/tests/fixtures/unreleased/`.
2. **Daemon, after the human approval (rules unchanged).** `runner.rs`: `push_train` is split into
   `build_marker` and `run_push`. With `land = pull-request` it keeps the pin ref and the marker,
   pushes `S` to `refs/heads/sluice/train/<id>` with `push::branch_push_args` (`--porcelain`,
   `--force-with-lease=refs/heads/<branch>:`, no audit ref, approver's env, hooks on) and classifies
   with the same `push::classify`. `branch_verdict` maps `Moved` and `TargetProtected` to `Rejected`
   (a branch has no target that moved or is protected, and nothing is learned about the target), so
   a refusal is a plain `push_rejected`. `Landed` becomes `awaiting_merge`
   (`awaiting_merge()`): jobs and train go to the new state, the marker stays, `pr_since` is set, the
   worktree is removed and the pin ref is kept. An ambiguous end parks the train as `needs_reconcile`
   like a push. A local target with `land = pull-request` fails the members with `land_needs_remote`
   (`pre_push_checks`).
3. **New states and tables.** `QueueJobState::AwaitingMerge` and `QueueTrainState::AwaitingMerge`
   (`awaiting_merge`), transitions in `model.rs` (`Pushing`/`NeedsReconcile` to it; it to
   `Landed`/`Failed`/`Cancelled`/`Queued` for jobs and `Landed`/`Dissolved` for trains);
   `train_is_active` includes it (see deviations). `store.rs`: nullable `pr_*` columns on
   `queue_push_markers` added in place (`MARKER_PR_COLUMNS`, `add_column_if_missing`, no
   `queue_schema` bump), `PrColumns` inside `MarkerRow`, `Writer::set_pr`.
4. **Forge interface.** `forge::Forge` grows `pr_state(branch)`, `ensure_pr(spec)`,
   `arm_auto_merge(number, head)` and `close_pr(number, comment)`; `GitHub` implements them with
   `gh pr list --head <branch> --state all --json number,url,state,headRefOid,mergedAt,mergeCommit,
   autoMergeRequest`, `gh pr create --base --head --title --body`, `gh pr merge <n> --auto --merge
   --match-head-commit <S>` and `gh pr close <n> --comment` (all with `-R [host/]owner/repo`, the
   caller's environment, bounded by `Budget`). New in core: `forge/pr.rs` (`PrObservation`,
   `PullRequest`, `PrFact`, `AutoMerge`, `describe` for the title and body: members, gates, train id,
   `parse_list`, `pick`, `number_from_url`), `forge/landing.rs` (`TrainRef`, `observe`,
   `observe_now`, `open`, `close`), `forge/cache.rs` (`forge-pr.json`, 20 s, `load_pr`/`store_pr`/
   `forget_pr`), and `locate_remote`/`unusable`/`call_env` factored out of the probe.
5. **Client messages.** `queue-pr-facts` (`ClientMessage::QueuePrFacts { id, facts, env }`, reply
   `queue-reconcile`), `pr_landing` on `queue-approve`, feature `queue-pr-landing`
   (`FEATURE_QUEUE_PR_LANDING`), reconcile decisions `awaiting_merge` and `failed`, optional
   `QueueTrainView.land` and `.pr` (`PullRequestView`), optional `QueueJobView.land` (enqueue reply
   only, for the client's warnings). All additive.
6. **Client flow.** `crates/sluice-gate/src/queue/pr.rs`:
   - `approve` sends `pr_landing: true`, waits for the branch push (`wait_for_landing` sees
     `awaiting_merge`), then `finish`: `landing::open` (find-or-create, `--auto-merge` only on the flag)
     and reports the fact. A failure leaves the train `awaiting_merge` with the note
     `pr_create_failed: <why>`, exit 1, and says how to finish (`queue reconcile`, or approving the
     train again: `resume`, which refuses agents and is idempotent).
   - `status` and `reconcile` call `refresh`/`reconcile_waiting`: `observe` each waiting train in
     parallel, send the facts, read the status again. `reconcile` also finds-or-creates a missing pull
     request for a human (an agent environment is refused).
   - `cancel` remembers the pull request first (`before_cancel`), and after the daemon dissolved the
     train closes the pull request when Sluice opened it (`landing::close`) and reports it closed
     (`after_cancel`) so the daemon deletes the branch.
   - `render_train` shows `AWAITING MERGE (pull request)`, number and address, age, auto-merge, target
     drift and the note (`waiting_text`); `pr_closed` has its own hint.
   MCP: `refresh_pull_requests` (read-only) before `queue_status` renders, state texts, schema mirror
   (`PullRequestSchema`), `INSTRUCTIONS`; `queue_approve` of a pull-request train and `queue_cancel` of a
   waiting job are refused.
7. **Daemon decisions.** `crates/sluice-gate-queue/src/awaiting.rs`: `pr_facts` loads the train,
   gathers what the daemon can read itself (`pr::gather_pr`: ls-remote of the branch, fetch of the target
   into `refs/sluice/base/<remote>/<target>`, `merge-base --is-ancestor`, tree comparison with
   `merge_tree`, `rev-list --count base..target`), decides with `pr::decide_pr` and applies. The
   daemon never talks to a forge and receives no token.
8. **Cancel in `awaiting_merge`** (`awaiting::cancel`): job `cancelled`, train dissolved (the other
   members go back to `queued`), pin ref dropped. The branch is deleted by the follow-up: when the
   client reports the dissolved train's pull request closed or absent, `clean_branch` deletes
   `sluice/train/<id>` with `Git::delete_remote_branch` (lease on `S`, the client's allowlisted env sent
   with the facts), only if the pull request was Sluice's (or there was none). Over MCP the cancel is
   refused.
9. **Doctor.** `queue-pr-branches` (`doctor/target.rs`, `pr_branches_check`): from the running daemon's
   status (no network), the number of waiting trains and the `sluice/train/*` branches of dissolved
   pull-request trains not deleted. `target-protection` is `ok` for a repository that lands through a
   pull request; the enqueue warnings are empty for it and otherwise say how to land (`land =
   "pull-request"`).
10. **Agent text and docs.** MCP `INSTRUCTIONS`, `plugin/skills/sluice/SKILL.md`, `reference.md`,
    `docs/agents.md`, `docs/merge-queue.md` (new "Landing through a pull request"), `docs/configuration.md`,
    `docs/cli.md`, `docs/compatibility.md`, `CHANGELOG.md`. Nothing is loosened.

## The flow end to end

enqueue (a warning only for a push repository) -> train built and verified -> `ready` -> human
`sluice queue approve` (plan hash, agent refusal; for a pull-request train also: human at the CLI,
`pr_landing`) -> daemon: pre-push checks (the target must still be at the base; otherwise the train is
dissolved as before, since nothing was published yet), pin ref, marker with the branch, push of `S` to
`sluice/train/<id>` -> `awaiting_merge` -> the approving client opens or finds the pull request, arms
auto-merge only with `--auto-merge`, reports the number -> anyone running `status`/`reconcile` (or MCP
`queue_status`) reports what their `gh` sees -> `landed` / `pr_closed` / notes.

## The reconcile tables (`crates/sluice-gate-queue/src/pr.rs`, pure, every row tested)

Push of the branch in doubt (`decide_branch`, state `needs_reconcile`): 1 push alive -> refuse
`push_in_progress`; 2 endpoint changed -> refuse `endpoint_changed`; 3 remote unreadable -> refuse
`remote_unreachable`; 4 branch at `S` -> `awaiting_merge`; 5 branch absent -> `ready` (`cancelled` with a
pending cancel); 6 branch elsewhere -> refuse `branch_diverged`.

Pull request of an `awaiting_merge` train (`decide_pr`): 1 forge unusable -> wait,
`forge_unreachable: <why>`; 2 creation failed -> wait, `pr_create_failed: <why>`; 3 closed -> jobs
`failed` `pr_closed`; 4 open and head != `S` -> wait, `pr_head_changed`; 5 open at `S` -> wait (target
drift recorded); 6 none, endpoint changed -> wait; 7 none, remote unreadable -> wait
`remote_unreachable`; 8 none, branch gone -> `pr_closed`; 9 none, branch present -> wait
`pr_not_opened`; 10 merged without a merge commit -> wait `merge_commit_unknown`; 11-12 merged,
endpoint/remote problems -> wait; 13 merged, commit not on the target -> wait
`merge_commit_not_on_target`; 14 reachability unknown -> wait `ancestry_unknown`; 15 merged, on the
target, same tree -> `landed`; 16 other tree -> `landed` + `tree_differs_from_verified`; 17 tree
unknown -> `landed` + `tree_unchecked`.

## Why

Issue #5: a target the forge only changes through pull requests could not be landed on. The client
opens and reads the pull request because only the client has the user's `gh` login (a keyring login is
invisible to the daemon); the daemon keeps what it can prove itself (the remote's refs and history) and
only records facts about the forge. Nothing merges: the forge's rules and a human do.

## Verification

Isolated runs during the work: `cargo check --workspace --all-targets`; `sluice-gate-core`
`forge` unit tests, `tests/forge_landing.rs` (9: fake `gh` on a test-only `PATH`, find-or-create and its
idempotence, a crash between push and pull request, auto-merge only on the flag and never `--admin`,
every observation, unreachable, the cache), the `queue-land` compat test; `sluice-gate-queue` lib (98,
including both tables row by row and the transition tables), `tests/pr_landing.rs` (14, a real daemon and
a bare remote: the push to the train branch and `awaiting_merge`, no audit ref, pin ref kept; merged same
tree; merged other tree; closed and retry; head changed, unreachable, missing, create failed, commit not on
the target, target moved without a requeue, branch gone; cancel and the lease-guarded branch deletion;
somebody else's pull request left alone; a refused branch push is `push_rejected` and teaches nothing;
reconcile by the branch in both outcomes; human-only approval with agent approval enabled; a branch cannot
set `land`; the user default; the in-place marker columns), `tests/queue.rs` and `tests/approve_policy.rs`
(unchanged behaviour, 44 + 2); MCP and CLI unit tests (rendering, feature gating, the schema mirror); the
e2e `a_pull_request_landing_pushes_the_branch_and_waits_even_without_a_forge` through the real binary
(a local remote has no forge: the train stays `awaiting_merge` with a note, an agent cannot finish it,
cancel dissolves it). The full suite, clippy and `fmt --check` are the lead's through Sluice. No GitHub
write of any kind was made; `gh pr list --json` was read once against a public repository to confirm the
JSON shape (`mergeCommit` is `{oid}`, `autoMergeRequest` is null or an object, states are
`OPEN`/`MERGED`/`CLOSED`).

## Deviations and open points

- **A waiting train occupies its target.** `train_is_active` includes `awaiting_merge`, so no other train
  is built for that target until it lands, fails or is cancelled (building on a base the pending pull
  request does not include would verify a combination that is never merged as verified). A pull request
  that waits for days blocks the queue behind it; `queue cancel` unblocks. Worth a user decision if it
  hurts.
- **Target moved before the branch is pushed**: the existing pre-push check still dissolves and requeues
  (nothing was published). The "wait, never requeue" rule applies once the pull request exists.
- **Auto-merge uses `--merge`** (a merge commit keeps `S` as the head of the history and its tree);
  a forge that only allows squash or rebase refuses it and the note says so (`auto-merge ... refused`).
- **No daemon polling**: landing is seen only when a client looks (ADR consequence). A `queue watch`
  or a daemon poll when `gh` works in its environment is later work.
- **Cancel's branch deletion needs the client**: it is triggered by the client reporting the pull request
  closed, so a cancel that never ran through this version's CLI leaves the branch, which `doctor` lists.
- **A refused branch push is `push_rejected`, not `target_protected`**, and does not record the learned
  fact (it says nothing about the target); the ADR wording "same classifier" is kept for the matching.
- **A `closed` report fails the jobs without proof** (the daemon cannot read the forge): the harm of a
  false report is a failed job that `retry` requeues; an MCP client can send facts but cannot cancel.
- **Doctor reads the daemon's status** (a local socket, no network) instead of opening the store, so the
  leftover list needs a running daemon.
- **CLI e2e with a real forge is not possible offline**: the forge is identified from the push URL
  after `insteadOf`, so a local bare remote has none; the `gh` paths are covered at the library level
  with the fake `gh`, and the daemon paths with facts.
- Not done, by decision: commit status `sluice/verify` on the pull request (later, opt-in); GitLab and
  other forges (same `Forge` interface).

## Lead review

- The user confirmed that a train in `awaiting_merge` keeps its target (ADR-0025 amendment).
- Clippy fixes: `large_enum_variant` (`ServerMessage::QueueApproved` carries
  `Box<QueueTrainView>`), `too_many_lines` (`extension.rs::resolve_enqueue` extracted from
  `enqueue`; the queue messages of the proto round-trip test moved to
  `queue_client_messages`), `elidable_lifetime_names`, `default_trait_access`, `nonminimal_bool`,
  `many_single_char_names` (test bindings renamed). Full fmt, clippy and test through Sluice: pass.
