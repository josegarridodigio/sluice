---
title: "Change: verification of waves W1–W5 (queue trust)"
tags: [sluice, change, queue-trust, verification]
---

# Change: verification of waves W1–W5

Plan: [[sluice/plan/queue-trust-plan.md]]. Waves:
[[sluice/changes/queue-trust-w1-env.md]], [[sluice/changes/queue-trust-w2-results.md]],
[[sluice/changes/queue-trust-w3-vouch.md]], [[sluice/changes/queue-trust-w4-prepare.md]],
[[sluice/changes/queue-trust-w5-bootstrap.md]].

The waves were written without running tests (a hook routes `cargo test` to Sluice).
The first full `verify` (fmt, clippy, test through Sluice) of the branch found the
items below; the last run passed all three gates (2026-10-26).

## Product bugs found and fixed

- **Paths read from a trimmed `git status --porcelain`.** `gate_modified_tree` (W2),
  `prepare_dirty` (W4) and, before this work, `dirty_merge` in `merge_members` took
  `line[3..]` of the trimmed text output, so the first path of a ` M path` line lost its
  first character. New `Git::status_paths(wt, untracked)` reads `--porcelain -z` raw
  output (`porcelain_z_paths`: NUL-separated, rename sources skipped, spaces and quotes
  intact); all three use it. Unit test `porcelain_z_paths_keep_every_character`.
- **Vouch refused pushes with untracked gate output** (W3, found in review): the
  daemon-side check compared a full snapshot (untracked files included) with `T`; it now
  applies the pre-push checks' rule (`HEAD == S`, no tracked change).
- **Agent approval of a trusted train** (W5, found in review): `approve` checked the
  allowlisted env, which has no agent-detection variables; it now checks the client's
  whole env.
- **Failed base probes were remembered** (W4, found in review): first only passes were
  remembered; later no probe is (a remembered pass also blamed members for environment
  failures).

## Test and lint fixes

- Clippy: `too_many_lines` (`RepoConfig::validate` → `validate_queue`;
  `prepare_and_claim` → `member_config`; init `plan` → `config_and_snippet`),
  `redundant_closure_for_method_calls`, `single_match_else`, `format_collect` (two
  tests), `doc_markdown`, `similar_names`, `type_complexity`, `assert_is_empty`.
- `client_log_records_the_reason_when_the_cache_is_off`: the line format now has
  `key=` (W1).
- `queue_gates_are_extended_with_the_pre_push_gates`: dropped an assertion on a
  configuration the parser rejects (unknown gate in `[queue] gates`).
- `a_prepare_failing_only_with_a_member_splits_the_train`: the solo train of the bad
  member is built only after the good member's train stops being active (one train per
  target); the test now lands A before waiting for B.

## Pre-existing, environment-dependent tests

Confirmed on an untouched checkout of `bfef109` (forced past the cache): when the
suite runs as a Sluice gate on a host with full containment, the test process lives in
the daemon's `sluice-daemon.scope/jobs/<id>` leaf.

- `containment_daemon`: `without_a_user_manager_the_daemon_runs_as_before` and
  `containment_can_be_switched_off_in_the_config` asserted "no `/jobs/` in the gate's
  cgroup"; they now assert the gate stays in its daemon's cgroup (what they mean).
  `the_daemon_and_its_jobs_live_in_their_own_cgroups` raced with `cg-exec`'s `exec`
  (the leaf briefly holds the wrapper); it now waits for the command.
- `session_start` tests that move processes into a test tree cannot move processes out
  of another daemon's delegated job leaf; `TestTree::build` now skips (printing why)
  when the test runs inside a Sluice gate job, like it already skips on hosts without
  full containment. They still run under plain `cargo test` and in CI.

## Incident during verification

A baseline worktree in the session's scratch directory (on `/tmp`, a tmpfs) built a
5 GiB `target/` and exhausted the tmpfs quota, failing unrelated tests (`update.rs`,
"Disk quota exceeded"). The worktree was removed. Lesson: baselines go in a directory
on disk, not on tmpfs.
