---
title: "Phase 6 acceptance evidence (merge queue)"
tags: [sluice, acceptance, phase-6, merge-queue]
---

# Phase 6 acceptance evidence (2026-10-22)

The eight acceptance tests of [[sluice/design/merge-queue.md]] section 13,
plus the extras the lead asked for, run as one integration test file:
`crates/sluice-gate/tests/queue_e2e.rs` (14 tests, about 8 s wall time when
run in parallel; the slowest test, 8, takes about 7 s).

Setup of every test: a `Sandbox` (temporary repository, private HOME and XDG
directories, so a private daemon socket and state database), a bare remote
made with `git clone --bare` and wired with `git remote add origin` +
`git remote set-head origin main`, the user config `[queue] tick = "200ms"`,
and linked worktrees (`git worktree add -b feat/x`) with one commit each. The
daemon is autostarted by the first `sluice` call and stopped when the sandbox
is dropped. Every interaction goes through the real binary (`sluice queue
enqueue|status [--json]|approve --yes|retry|cancel|reconcile`, `sluice run`,
`sluice daemon stop`, `sluice mcp` for test 6). The gate `test` appends to a
counter file, so real gate runs can be counted. Run with `-- --nocapture` to
see the `[evidence]` lines quoted below (ids and SHAs vary).

## Results

| # | Test (`queue_e2e.rs`) | Command flow | Observed output (trimmed) | Result |
|---|---|---|---|---|
| 1 | `acceptance_1_conflict_ready_no_push_before_approval_then_landing` | Placeholder job X enqueued and `ready` (holds the target), A (`a.txt`, `y.txt`) and B (`a.txt`) enqueued, `queue cancel X` so A and B are claimed as one train; `queue status`; wait 1 s; `queue approve <t> --yes`. The repository has a `pre-push` hook that copies its stdin to a witness file. | `train t-…  origin/main  base c08cff2  READY (awaiting approval)` / `feat/a  job …  merged  gates: test PASS 0.0s` / `feat/b  job …  CONFLICT a.txt  -> rebase, then 'sluice queue retry …'`; approve: `sluice: approved; pushing…` / `sluice: landed aaa567e on origin/main (train t-…)` | Pass. Remote `main` unchanged before approval and no hook call; after: remote `main` == landing SHA, `refs/sluice/landed/<S>` == S, A `landed`, B still `conflict`, B's commit is neither an ancestor of `main` nor present in the bare remote; the witness holds `refs/heads/main` (the pre-push hook ran). |
| 2a | `acceptance_2a_sigkill_during_push_then_hook_released_reconciles_to_landed` | Bare remote `pre-receive` hook writes its pid file then blocks on `cat <fifo>`. `queue approve --yes` in the background; when the pid file appears the train is `pushing`; the approver is killed, the daemon gets SIGKILL; `queue status` (autostarts a new daemon); `queue reconcile`; `go` written into the FIFO; `queue reconcile` until it decides. | After restart: `NEEDS RECONCILE (push outcome unknown)` / `note: daemon start: push process 1475631 still running`; reconcile: `train t-…: refused (push_in_progress)`; after release: `train t-…: landed (no reason given)` | Pass. New daemon pid, job `needs_reconcile`, remote untouched while blocked; after release remote `main` == S, train and job `landed`, nothing left to reconcile. Found bug 1 (below). |
| 2b | `acceptance_2b_sigkill_during_push_then_hook_aborted_reconciles_to_ready` | As 2a, but `abort` is written (the hook exits 1); after the hook ends, `git remote set-url origin <clone>` and `queue reconcile` (row 2), URL restored, `queue reconcile`. | `train t-…: refused (push_in_progress)`; with the other URL: `train t-…: refused (endpoint_changed)`; then `train t-…: ready (no reason given)` | Pass. Train stays `needs_reconcile` on the refusal; then `ready` with no `approved_via` (approval voided), job `ready`, remote `main` == base. |
| 2c | (not in this file) | Marker written but no push spawned. | — | Covered by `sluice-gate-queue` `tests/queue.rs` `marker_without_push_reconciles_to_ready` and `marker_without_push_is_parked_at_start_then_ready` (they write the marker row with SQL between two daemon runs). There is no outside knob (env or config) to stop the runner between the marker transaction and the spawn, and adding one only for tests was not worth a product switch. |
| 3 | `acceptance_3_marked_tree_is_ready_without_running_a_gate` | `sluice run test` in worktree A (verifies the committed tree, writes the marker), `queue enqueue`, wait `ready`. | gate view `{"gate":"test","status":"cached"}` without `job_id` | Pass. Counter 1 after `sluice run`, still 1 after the train is `ready`: zero gate executions for the train (single member, fast-forward, so the train's tree is A's tree). |
| 4 | `acceptance_4_train_gate_and_agent_run_coalesce` | Gate `sleep 2; echo run >> counter`. `sluice run test` in worktree A in a thread; once `sluice status --json` shows it running, `queue enqueue` from A; wait until the running job has `subscribers == 2`; wait `ready`. | `running[0].subscribers == 2` | Pass. `sluice run` exits 0, the train is `ready` with the gate `pass`, counter == 1: one execution served both. |
| 5 | `acceptance_5_base_moved_dissolves_old_plan_is_stale_new_one_lands` | A `ready`; another clone pushes a commit to the bare remote's `main`; wait for the old train `dissolved` and a new `ready` train on the moved base; `queue approve <old> --yes`; raw `queue-approve` with the old hash over the socket; `queue approve <new> --yes`. | CLI: exit 4, `sluice: train t-… is DISSOLVED, not ready for approval`; socket: `Error { code: StalePlan, message: "train t-… was dissolved (target moved); check 'sluice queue status'" }` | Pass. New train has the same single member, job `attempts == 2`; after approval remote `main` == the new S and the moved commit is its ancestor. |
| 6 | `acceptance_6_mcp_approval_refused_by_default_then_allowed` | A `ready`; rmcp child-process client on `sluice mcp` calls `queue_approve`; user config gets `allow_agent_approval = true`; `sluice daemon stop`; `queue_approve` again (autostarts the daemon). | Refused: `not-allowed: approval is reserved to humans (sluice queue approve); set [queue] allow_agent_approval = true in the user config to allow agents`; allowed: `landed de4e32b on origin/main (train t-…)` | Pass. Remote unchanged and train `ready` after the refusal; after: remote `main` == S, train `landed`, `approved_via == "mcp"`. The daemon restart is needed because the daemon reads the user config only at start (open point in [[sluice/changes/phase-6-merge-queue.md]]). |
| 7a | `acceptance_7a_endpoint_changed_never_pushes` | A `ready`; `git remote set-url origin <other bare clone>`; `queue approve --yes`; wait for the rebuilt train. | exit 1, `sluice: train t-… was dissolved (endpoint_changed); its jobs were re-queued (see 'sluice queue status')` | Pass. Old train `dissolved` with note `endpoint_changed`; a new train with the same member is `ready` against the new URL and waits for approval; neither the old nor the new remote's `main` moved. The message's reason is bug 2 (below). |
| 7b | `acceptance_7b_lost_lease_never_pushes` | A `ready`; the bare remote is renamed away (unreachable), `queue approve --yes` in the background → train `approved`, push pre-check postponed every tick; the lease token in `sluice.db` is overwritten (rusqlite, dev-dependency) as if another daemon took it; the remote is restored; 2 s (10 ticks) pass. | — | Pass. Remote `main` unchanged, no row in `queue_push_markers`, train still `approved`: the fenced marker transaction is refused, so the push never starts. |
| 8 | `acceptance_8_failing_member_splits_and_the_others_land` | `[queue] max_train = 4`, gate `test ! -e bad.txt && { test ! -e slow.txt \|\| sleep 2; }`. Blocker X (`slow.txt`) claimed first; A, B (`bad.txt`), C enqueued while X verifies; X approved; wait for A alone `ready`; approve; wait for C alone `ready`; approve. | `train t-…  DISSOLVED` / `feat/b  job …  FAILED gate_failed gate test (see 'sluice logs …')` / `note: gate test failed; members split` | Pass. The three-member train is `dissolved` with the gate `fail`; X, A, C `landed`; B `failed`, `reason == gate_failed`, `detail.gate == "test"`; remote tree has `ok-a.txt`, `ok-c.txt`, no `bad.txt`. |
| + | `local_branch_target_lands_by_ref_update_and_refuses_checked_out` | `git branch integration main`; `queue enqueue --base refs/heads/main` from A; `queue enqueue --base integration`; approve. | refusal: exit 4, `sluice: target_checked_out: local branch 'main' is checked out in a worktree; updating it would desync that worktree`; approve: `sluice: landed … on integration` | Pass. Train `remote == ""`, `target == "integration"`; local `refs/heads/integration` and `refs/sluice/landed/<S>` == S; the bare remote untouched. |
| + | `declining_pre_push_hook_fails_the_train` | Repository `pre-push` hook prints `policy says no` and exits 1; A `ready`; approve. | exit 1, `sluice: train t-… failed: feat/a: pre_push_hook` | Pass. Job `failed`, `reason == pre_push_hook`, `detail.output` contains the hook's stderr; remote unchanged. (The witness hook of test 1 shows the hook runs on success.) |
| + | `unreachable_base_is_reported_then_fails_after_three_attempts` | User tick `1s`; `git remote set-url origin <path that does not exist>`; `queue enqueue --base origin/main`; status; wait `failed`; URL restored; `queue retry`. | queued: `feat/a  job …  for origin/main  position 1  waiting: base_unavailable git fetch --no-tags --quiet origin …: fatal: '…/nowhere.git' does not appear to be a git repository`; then `FAILED base_unavailable …  -> fix and commit, then 'sluice queue retry …'` | Pass. `detail.failures == 3` when `failed`; after the retry the reason is cleared and the train is built and `ready`. |
| + | `invalid_base_config_is_reported_then_fails_after_three_attempts` | User tick `1s`; `main` gets `.sluice.toml` with an unknown key, pushed to the remote; A (branched before) enqueued; status; wait `failed`; `queue retry`. | `waiting: base_config_invalid invalid .sluice.toml on the base: invalid configuration in <sha>:.sluice.toml: TOML parse error at line 2, column 1` | Pass. `detail.error` names `bogus_key`; `failed` with the same reason after 3 attempts; `queue retry` prints `requeued` and the job is no longer `failed`. |

## Bugs found and fixed

1. **No restart for 90 s after SIGKILL** (test 2): the dead daemon's lease
   blocked every new daemon until its TTL expired. Fixed in
   `sluice-gate-daemon` `server::take_lease` / `reclaim_dead_lease`.
2. **Dissolved-at-push reason not shown** (test 7a): `queue approve` now
   prints the train note.

Details, files and regression tests:
[[sluice/changes/phase-6-merge-queue.md]] ("Bugs found by the acceptance
run"). The lead's request on base failures (recorded reason, `failed` after
3 attempts) is described in the same note and tested by the last two rows.

## Not covered, and why

- **Test 2c (marker written, no push spawned)** through the binary: no
  outside way to stop the runner between the marker transaction and the
  spawn; covered at crate level (row 2c).
- **Lost lease *during* a running push** (the push finishes and its result is
  written only if the fence still matches): the lease can be stolen from
  outside, but not at a controlled point of a running push without a
  blocking hook *and* a crash; 7b proves the "never starts a push" half.
- **Reconcile rows 5 and 6** (audit ref present but target rewritten; target
  moved on top of S) against a real remote: only unit-tested (`reconcile::decide`).
- **`push_timeout`** is covered by the crate test
  `hung_pre_push_hook_times_out_and_reconciles_to_ready`, not repeated here.
- **TTY prompt of `queue approve`**: tests run without a terminal, so they use
  `--yes` (the no-TTY refusal is in `tests/queue_cli.rs`).

## Commands

```
cargo test -p sluice-gate --test queue_e2e            # 14 passed, ~8 s
cargo test -p sluice-gate --test queue_e2e -- --nocapture   # with [evidence] lines
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                                # 339 passed, 0 failed
```
