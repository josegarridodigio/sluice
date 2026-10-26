---
title: "Plan: trusted train push, prepared merge worktrees, configuration bootstrap (issues #3, #4)"
tags: [sluice, plan, merge-queue]
---

# Plan: trusted train push, prepared merge worktrees, configuration bootstrap

Decision: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]]. Design being
amended: [[sluice/design/merge-queue.md]]. Branch `fix/queue-push-and-prepare`.
Approved by the user on 2026-10-26. Sub-agents run on Sonnet 5.5.

Waves run **one after another** (they share `runner.rs`, `server.rs` and the proto
crate). Each wave ends with `cargo check --workspace --all-targets` clean and its
own new tests passing in isolation; the full verification (`fmt`, `clippy`,
`test` through Sluice `verify`) runs once after the last wave. Each wave writes a
change note under `kb/sluice/changes/` and updates the docs it touches.

## Rules for every wave

- Read the ADR, this plan, the sections of `kb/sluice/design/merge-queue.md` the
  wave touches, and the code before editing. Match the surrounding style and
  comment density. English everywhere.
- Gates (`cargo fmt --check`, `cargo clippy …`, `cargo test --workspace`) are never
  run through the shell. Allowed: `cargo check`, `cargo fmt` (formatting the
  files), and running the single new test being written
  (`cargo test -p <crate> <name>`).
- No git library, no change to the user's index or working tree from product code,
  no personal paths or machine names anywhere.
- Every new protocol field is optional (`#[serde(default, skip_serializing_if)]`);
  the frozen `status --json` / `queue status --json` schema tests must keep passing.
- Do not commit; leave the changes in the working tree.

## W1 — Environment and key inputs (ADR sections 1, 2, 7 part)

- `sluice_gate_core::env::daemon_env(env, user_config) -> EnvMap` (new module):
  strips `GIT_*`, `CLAUDE*`, `SLUICE_JOB`, `SLUICE_AGENT`, `SLUICE_CACHE`,
  `SLUICE_DISABLE`, `SLUICE_SESSION_LEAF`, `SLUICE_SHIM_DIR`, the
  `[agent_detection]` variables, and removes the shim directory (and only it) from
  `PATH`. Unit tests.
- Use it in `sluice-gate-proto` `client.rs` (daemon spawn) and in the daemon's
  `sanitised_env`. Check other daemon spawn paths (`daemon restart`, `update`).
- `key::collect_inputs`: `env_clear()` before `envs(env)`; check every caller
  passes a complete env (PATH included). Test: an input printing a variable not in
  the map prints nothing.
- `client.log`: add `key=<12 hex>` (or `-`); docs of the format.
- Daemon: `info` log line for every `run` request that carries a `hook`
  (gate, hook, tree, key prefix, caller, decision: hit/queued/coalesced).
- `sluice run --explain`: report the cache mode and show `lookup: OFF (<reason>)`
  when the mode or the gate disables caching.
- Docs: `kb/sluice/design/cache-key.md` (inputs run with a cleared env),
  `docs/` pages that describe the daemon environment.

Acceptance: unit tests above; existing tests unchanged in meaning.

## W2 — Queue corrections and real train results (ADR sections 4, 7)

- Runner: inspect `outcome.marker_written` / `not_cached_reason`. After the gates,
  `status --porcelain --untracked-files=no` of the merge worktree non-empty →
  reason `gate_modified_tree` (detail: gate, job_id, paths; the gate that ran last
  is named when several ran — record all gates' names), handled like a gate
  failure (single member `failed`, several → split). Otherwise a pass without a
  marker sets `TrainGate.note = "not cached: <reason>"` (new optional proto field,
  mirrored in the MCP schema, rendered by CLI and MCP).
- Train gate jobs are killed when the train drops them regardless of
  `last_subscriber_gone`: add what the `DaemonHandle` needs (e.g. a submit variant
  whose subscription cancels the job on drop), no protocol change.
- `config_changed`: three-dot (`base...head`); keep it when a gate fails (merge
  detail objects instead of overwriting); render it in `queue status` (CLI, MCP)
  and in the approval plan: "changes .sluice.toml; this train runs the base's
  gates".
- `target::defaults_at` returns an error for an unparsable head config; enqueue
  without `--base` refuses with `bad-request` `invalid_config: …`; with `--base`
  it is not needed.
- `TrainSettings`: hand-written `Default` with the real defaults
  (`push_timeout` 10 min, `audit_ref` true, network default), `#[serde(default)]`
  on the struct; `from_json` failures logged at `warn`.
- Push timeout: collect what the push printed before it was killed (bounded
  wait on the readers after the group is gone) and put its tail in the
  `push_timeout` note.
- A `key_input` that fails in the merge worktree: reason `key_input_failed`,
  handled as an environment failure: members back to `queued` with the reason,
  paced and counted like `base_unavailable`, `failed` after 3
  (`Writer::note_queued_failure`, `MAX_PREPARE_FAILURES`).
- Tests (queue crate integration tests and/or `queue_e2e.rs`): gate leaving an
  untracked file → ready with `note`; gate editing a tracked file →
  `gate_modified_tree`, no approval loop; `config_changed` false for a branch
  forked before the target changed `.sluice.toml`; invalid head config refused
  without `--base`; old `settings` JSON missing fields keeps real defaults;
  `key_input_failed` requeues then fails after 3.

## W3 — The daemon vouches for its own push (ADR section 3)

- `Extension::vouch_run(&RunRequest, peer_pid: Option<u32>) -> BoxFuture<Option<Vouch>>`
  with a default `None`; `Vouch::{Verified{train_id, job_id}, Refused{message}}`.
  The daemon calls extensions in `handle_run` before the marker double-check and
  scheduling and answers with `Done{Cached}` / `Done{Error}` (message names the
  train, the job, or why the gate is refused).
- Queue implementation: match a `pushing` train by canonical worktree path and
  common dir; peer pid descends from the marker's `push_pid` with matching start
  ticks (`/proc/<pid>/stat` ppid walk, depth ≤ 64); daemon-side `HEAD == S` and
  snapshot `== T` (cheap; run off the async threads); gate status `pass`/`cached`
  in the train's view → `Verified`; else `Refused` with: gate not verified by the
  train (`config_changed` named when it applies; hint: add it to `[queue] gates`),
  or the worktree no longer holds the verified tree. Any failed precondition other
  than the gate check → `None` (normal path).
- `select_gates`: `[queue] gates ∪ pre-push gates`; else pre-push gates; else all.
  Update docs and the design section 5 step 5.
- Tests: **a real Sluice `pre-push` hook** (`sluice run --hook pre-push <gate>`)
  in the repository of the e2e harness: the train lands without a second run of
  the gate (counter file) even when the approver's `PATH` makes `key_inputs`
  differ from the daemon's (e.g. a fake `tool --version` earlier in the approver's
  PATH) and with `CI=1` in the approver env; a hook gate outside the train's gates
  → fast `pre_push_hook` failure (well under `push_timeout`); a `sluice run` in the
  merge worktree that is not a descendant of the push → normal path.
- Docs: design sections 6 and 12 (what the hook answers, why it is safe),
  `docs/merge-queue.md`.

## W4 — `[queue] prepare` (ADR section 5)

- Config: `prepare`, `prepare_timeout` (default 10m, > 0), `prepare_weight`
  (gate weight syntax, default auto); validation and unknown-key behaviour as for
  every `[queue]` key; `kb/sluice/design/config-format.md`, `docs/configuration.md`
  ("needs 0.6.0; older binaries reject the file"); unreleased compat fixture
  (`tests/fixtures/unreleased/`) like `auto-weight.toml`.
- Runner: fast path (all keys hit → ready without preparing); prepare job through
  the scheduler (no key, `no_marker`, name `queue:prepare`, killed on drop, timeout
  always set); snapshot `== T` after it (`prepare_dirty`); keys derived after it;
  cancellation and shutdown checks while it runs (same `select!` as the gates).
- Attribution: base probe worktree `<state>/merge/<train>-base` (hooks disabled,
  removed after, swept like merge worktrees), result remembered per
  `(repo, base_sha, prepare cmd)`; base fails → all members `queued` with
  `prepare_failed` (`base: true`), paced, `failed` after 3; base passes → split /
  single `failed` `prepare_failed`. Detail like `gate_failed`
  (`job_id`, `status`, `exit_code`, `log_path`, `message`, `error`).
- Store: nullable `queue_trains.prepare` JSON column added by an idempotent
  migration (`PRAGMA table_info`), no `queue_schema` bump; optional
  `prepare` field in the train view (proto, MCP schema), rendered in CLI/MCP.
- `sluice init`: write `[queue] prepare` for detected lockfiles: pnpm
  `pnpm install --frozen-lockfile --prefer-offline`, npm `npm ci`, yarn berry
  `yarn install --immutable` (classic `yarn install --frozen-lockfile`), bun
  `bun install --frozen-lockfile`, uv `uv sync --frozen`, poetry
  `poetry install --no-root`; nothing for cargo/go; nothing when unsure.
- Tests: train whose gate needs a file created by `prepare` (ignored) passes;
  prepare failing on base and train → requeued with backoff then `failed`
  (one probe per base); prepare failing only with a member → split, the others
  land; `prepare_dirty`; fast path skips prepare; cancel during prepare kills it;
  init writes the prepare line per lockfile.

## W5 — Human configuration bootstrap (ADR section 6)

- Proto: optional `trust_member_config: bool` on `queue-enqueue` and
  `queue-retry`; `welcome.features` + `queue-trust-config`; train view optional
  `config_source` and `config_diff`.
- Daemon (extension): refuse the flag for `via: mcp` and for an agent-detected CLI
  environment regardless of `allow_agent_approval` (`not-allowed`); a train built
  from member config can only be approved by such a human CLI request.
- Store: `queue_jobs.trust_config INTEGER NOT NULL DEFAULT 0`,
  `queue_trains.config_source`/`config_hash` (idempotent migration).
- Runner: a trusted job is claimed alone; gates and `[queue]` (prepare included)
  from `git show <head>:.sluice.toml`, `remote`/`target` from the job; train hash
  gets the config hash appended only for these trains; prepare attribution: the
  base probe uses the member's prepare command.
- CLI: `--trust-member-config` on `enqueue` and `retry`; `status` and the approval
  plan show a warning and the `.sluice.toml` diff (`base...head`, or the whole
  file when the base has none) before the prompt. MCP: the tools do not offer the
  flag; `queue_status` shows the warning.
- Docs: `docs/merge-queue.md` (a "Fixing the queue's own configuration" section),
  design sections 3, 5 step 2, 9, 12, 14.
- Tests: MCP and agent-env CLI refused; human CLI builds a solo train with the
  member's gates; approval of that train refused over MCP even with
  `allow_agent_approval`; plain retry clears the flag; hash binds the config
  (a new commit changing `.sluice.toml` voids the approval).

## W6 — Memory safety of gate jobs (added 2026-10-26)

Evidence (a second machine, 12 threads, 30 GiB RAM, 8 GiB swap, five agents): one
`pnpm test` gate (`node --test`, one worker per core, 1.5–2.7 GiB each) peaked at
18 GiB, the daemon's `jobs` subtree at 24.5 GiB; two or three such suites were
admitted together (one over-admitted). `sluice-daemon.scope` has no memory limit
and no oomd policy, while `sluice-agents.scope` has `ManagedOOMMemoryPressure=kill`
at 30 %: systemd-oomd killed agents' `tools` leaves and the kernel's global OOM
killed agents' browsers and an unrelated user service. The culprit was never
the victim. Admission reads memory PSI only at admission time (0 % then).

Decision: [[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]]; analysis and test
list: [[sluice/research/memory-gates-w6-analysis.md]] (T-W6-0 measured: `oom_kill` 0 in
the gates' scope, 18 in the agents'; the gates' scope had comparable `full` stall and
`pgscan`). Sub-waves, in order (all deterministic and testable without memory pressure
except W6d):

- **W6a — outcome and kernel preference** (ADR-0019 pieces 2, 4): classification at job
  end in `launch()` (leaf `memory.events`, peak, sampled working set; bounded oomd journal
  check for SIGKILL-shaped endings), `killed_by = "memory"` with its message through
  `close_job`, interventions row, every client prints it (MCP, run, exec, queue runner,
  prepare); `cg-exec --oom-score-adj` (default 800, `max(configured, inherited)`);
  `memory.oom.group = 1` on job leaves; `MemoryWatcher` backstop on `jobs/` and the scope;
  doctor: ancestors' `oom.group`, effective adj. Tests T-W6-6..9, 12.
- **W6b — memory-aware admission** (piece 1): working-set sampling in `sample_tick`,
  `gate_memory` table (containment schema group v2), learned peak cache, `MemoryFacts` in
  `Snapshot`, `fits_mem` in the nominal pass, over-admission and guard, memory in the head
  reservation, `WaitCause::Memory` (wire `budget` + `wait_detail = memory`), status/JSON
  fields, `[admission] memory_aware` / `memory_reserve`. Tests T-W6-1..5, extended
  no-starvation property and the memory-off differential test.
- **W6c — oomd policy on the gates' scope** (piece 3) as decided, `derive_gates_policy`,
  `user.oomd_omit` on `daemon/`, doctor line. Tests T-W6-10, 13.
- **W6d — bounded live tests** T-W6-14/16 on the developer machine and on the second
  machine of the incident (consent for both given 2026-10-26; S5 §9 safeguards; on the
  second machine only while no heavy gate runs there), T-W6-15/17–19 only in a disposable VM; they confirm
  piece 3's shape or switch it to variant B. **Done for T-W6-14/16** on both machines:
  [[sluice/research/memory-gates-w6d-oomd-spike.md]] (piece 3 as designed).
- Optional ceiling `gates_memory_max` / `gates_memory_swap_max` (opt-in, empty default),
  config docs, compatibility fixture. **Done** (docs-sync follow-up).
- **W6e — Sluice-side guard** (ADR-0019 piece 7, approved 2026-10-26): opt-in
  `[admission] memory_guard`, cancels the daemon's own largest gate job under real
  exhaustion; pure decision function with fake samples, daemon test with fake
  `meminfo`/pressure files.

### W6 addendum — commands that escape the budget (found 2026-10-26)

While the waves ran, sub-agents' `cargo check --workspace` (about 20 parallel
`rustc`) competed with another agent's timing-sensitive `ng test` gate (weight 16
of 20); the gate's axe tests (5 s timeout) timed out in two runs. `cargo check`
is not in the built-in heavy list (`heavy.rs`: `cargo build`, `cargo test`,
`cargo clippy`, `cargo nextest`), so it never enters the queue. Add `cargo check`
(and review the list for other compilers that are as heavy, e.g. `cargo doc`)
with a test, and document it.

## Final

- Sluice `verify` (fmt, clippy, test) on the working tree; fix until green.
- Review pass of the whole diff (bugs, compatibility, docs vs code).
- CHANGELOG entries under the existing `[Unreleased]` headings; master plan row.
- Reply on issues #3 and #4 only with the user's approval.

## Progress

| Wave | Status | Change note |
|---|---|---|
| W1 | done | [[sluice/changes/queue-trust-w1-env.md]] |
| W2 | done | [[sluice/changes/queue-trust-w2-results.md]] |
| W3 | done | [[sluice/changes/queue-trust-w3-vouch.md]] |
| W4 | done | [[sluice/changes/queue-trust-w4-prepare.md]] |
| W5 | done | [[sluice/changes/queue-trust-w5-bootstrap.md]] |
| W1–W5 verification | done | [[sluice/changes/queue-trust-verification-w1-w5.md]] |
| W6 | W6a–W6e done (the VM-only tests T-W6-15/17–19 stay open) | [[sluice/changes/queue-trust-w6a-memory-outcome.md]], [[sluice/changes/queue-trust-w6b-memory-admission.md]], [[sluice/changes/queue-trust-w6c-oomd-policy.md]], [[sluice/changes/queue-trust-w6e-memory-guard.md]] |
| Docs sync | done (agent surfaces, full audit of docs against code; ADR-0019 piece 5 found unimplemented, then implemented) | [[sluice/changes/queue-trust-docs-sync.md]] |
| Review fixes | done (two independent reviews: base-probe memo removed, build-time `hook_gate_unverified`, memory admission exemption only when nothing runs, guard by wall time, ...) | [[sluice/changes/queue-trust-review-fixes.md]] |
| Final | verification green 2026-10-26 (fmt, clippy, whole suite through Sluice); commits and issue replies await the user | |
