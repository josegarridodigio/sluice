---
title: "ADR-0025: Targets that refuse direct pushes: warn early, land through a pull request"
tags: [sluice, adr, queue, forge, github]
---

# ADR-0025: Targets that refuse direct pushes: warn early, land through a pull request

Status: accepted (2026-10-28, user decisions below). Issue #5. Research:
[[sluice/research/protected-targets-analysis.md]]. Plan: [[sluice/plan/queue-scheduling-plan.md]]
(S3, wave Q4). Related: ADR-0018 (trusted push, approval), ADR-0020 (train tool environment).

## Context

The merge queue lands a train with one `git push --atomic --force-with-lease` of the verified
merge commit `S` to the target, in the approver's allowlisted environment. A target the forge
only lets change through pull requests refuses that push with a server-side hook; Sluice finds out
after verification and approval, classifies it as a generic `push_rejected` and dissolves the
train. Facts found while researching: GitHub tells whether a branch is protected with read access
(`branches/{b}` and the rules API; "not protected" and "no access" differ only by the 404 body);
a repository's `origin` can be an SSH alias, so the forge is identified after `ssh -G`; and `gh`
may be logged in through the desktop keyring, which the daemon's environment cannot reach.

## Decision

1. **Early warning, never a refusal.** At `queue enqueue` (CLI and MCP) and in `sluice doctor`, the
   client (which has the user's real environment) probes the target through the forge's CLI:
   `open | pull-request-required | protected | unknown`. Anything but `open` prints a warning
   naming the protection and, when the repository's `land` is `push`, says the train will not
   land (configure `land = "pull-request"`). The enqueue always proceeds (user decision). An
   unknown result (no CLI, no login, unknown forge, no access) warns that it could not check.
2. **Learned signal, forge-agnostic.** A push refused by a recognised protected-branch rejection
   gets its own reason `target_protected` (instead of `push_rejected`) and is remembered per
   remote endpoint and target; the next enqueue to that target warns from it, with no forge API.
3. **Pull-request landing.** `[queue] land = "push" | "pull-request"` (default `push`; set in the
   target commit's `.sluice.toml` or the user config, never by a branch). With `pull-request`,
   after the human approval (unchanged: plan hash, agent refusal, trust rules):
   a. the daemon pushes `S` to `refs/heads/sluice/train/<id>` (lease on absence, approver's env,
      same classifier) instead of the target, and the train and its jobs go to a new state
      `awaiting_merge`;
   b. **the client opens the pull request** (user decision): `sluice queue approve` waits for the
      branch push and then find-or-creates one PR per train with the user's own `gh`
      (`gh pr list --head`, else `gh pr create`), and records the PR number with the daemon. No
      token is stored or passed to the daemon. A client that dies between the push and the PR
      leaves the train `awaiting_merge` without a PR; `sluice queue reconcile` (and the next
      `approve` of that train) finishes the find-or-create;
   c. Sluice never merges and never uses admin rights. Auto-merge (`gh pr merge --auto
      --match-head-commit S`) is armed only when the approver asks for it on the command line;
   d. **merge detection runs from clients**: `sluice queue status` and `sluice queue reconcile`
      (CLI, and MCP `queue_status`) read the PR state with the caller's CLI and report it to the
      daemon (a read of a fact, not an approval). Merged and its merge commit reachable from the
      target → `landed`, with `tree(merge commit) == tree(S)` recorded; a different tree is still
      landed (a human merged it) but noted `tree_differs_from_verified` and never called
      verified. Closed without merge → jobs `failed`, reason `pr_closed`; `queue retry` is the
      way back.
4. **The target moves before the forge merges: wait** (user decision). The PR stays as it is; the
   forge's own rules decide (for example "require branches up to date"); `status` shows how far
   the target moved. Sluice does not requeue.
5. **Reconcile** gets a second pure decision table for `pull-request` landing (remote branch SHA,
   PR state, merged, merge commit reachable, CLI usable), tested like the existing one. Sluice
   deletes a `sluice/train/<id>` branch or closes a PR only when it created them, the train is
   dissolved, with a lease on the recorded SHA; `doctor` lists leftovers.
6. **Forges.** Forge-agnostic: the probe result type, the learned signal and reason, the `land`
   key, `awaiting_merge`, the reconcile table and the `landed` rule. GitHub first through `gh`
   behind a `Forge` interface; GitLab (`glab`, push options) and others later through the same
   interface. A commit status `sluice/verify` on the PR is a later, opt-in addition (user
   decision).
7. **Compatibility.** New queue states and fields are additive (older clients show them as
   unknown); `welcome.features` gains `queue-pr-landing`, and a client that does not see it
   refuses `land = "pull-request"` instead of ignoring it.

## Amendment (2026-10-28, wave Q4b, user decision)

While a train waits in `awaiting_merge`, it keeps its target: the next train for that target is
not built until the pull request is merged or closed (or the job cancelled). Every merge is then
exactly the combination Sluice verified; the cost is that a pull request left unreviewed blocks
the queue of that target. Stacking the next train on the pending commit (merge-train style) was
considered and left for later.

## User decisions (2026-10-28)

| # | Question | Answer |
|---|---|---|
| 1 | Enqueue on a target that requires pull requests | Warn only |
| 2 | Target moves before the forge merges | Wait for the forge |
| 3 | Who talks to the forge with the user's `gh` (keyring) | The client: approve opens the PR, status/reconcile read it |
| 4 | Commit status on the PR | Later, opt-in |
| 5 | Release | Together with the queue waves Q1-Q3 |
| 6 | Next train while a PR waits | Waits for the PR to be merged or closed |

## Consequences

- Repositories with a protected target can use the queue: verification and the combined check stay
  Sluice's, the merge stays the forge's and the human's.
- The PR exists only after a human approval and is opened with that human's credentials; agents
  gain no merge path.
- Landing is visible only when some client looks (status, reconcile); a train can sit
  `awaiting_merge` unseen. The daemon may later poll by itself when the CLI works in its
  environment.

## Alternatives considered

- Refusing the enqueue (recommended by the research): declined, warn only.
- The daemon opening and polling PRs with a file-stored `gh` login: requires every machine to move
  its login out of the keyring; declined.
- One PR per member: loses the combined verification that is the queue's purpose.
- Requeue when the target moves: spends verification and approval again; declined.
