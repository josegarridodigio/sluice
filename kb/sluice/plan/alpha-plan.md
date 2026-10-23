---
title: "Plan: from pre-alpha to alpha (v0.3.0)"
tags: [sluice, plan, alpha]
---

# Plan: from pre-alpha to alpha (v0.3.0)

Status: done (2026-10-23). Approved by the user on 2026-10-22 and driven by a
`/goal`; every task below is implemented and release `v0.3.0` is published.
Parent: [[sluice/plan/master-plan.md]] ("Alpha readiness" block). Evidence
so far: [[sluice/changes/alpha-readiness.md]].

## 1. Why this plan exists

The first alpha-readiness runs (harness `scripts/alpha/`, no AI sessions)
proved the core claims on real non-Rust projects and also exposed gaps:

| Finding | Source | Consequence |
|---|---|---|
| Four hook managers (husky, lefthook, pre-commit, plain) on a public Node and a public Python project: 36/36 checks pass; a hit costs 17–137 ms per `git commit` including the manager | `s1_hooks` | Hooks integration is alpha-ready |
| 8 agents x 3 iterations on 20 CPUs: 40 → 25 heavy suites, peak concurrency 8 → 4, wall 80.5 → 70.4 s, a broken change never passes | `s2_problem` | The original problem is solved when the machine is not saturated |
| 12 agents with 8-thread suites (overload): 48 → 25 suites (48 % less work), peak load 74 → 31, but wall 147 → 162 s and p95 verify latency 37.5 → 79.6 s | `s2_problem` overload | The budget admits only 2 jobs of weight 8 in 19 units, leaves CPU idle and serialises agents. Scheduler policy must improve |
| The daemon refuses to start when the socket path exceeds 108 bytes (deep `XDG_RUNTIME_DIR`); clients fall back to in-process runs, silently losing queue and coalescing | `s1_hooks` first run | Needs a short-path fallback and a `doctor` check |
| `git commit` reports every failing hook as exit 1: Sluice's exit 3 (fail-fast) is visible only in the message | `s1_hooks` | Agent guidance must key on the message, not the code |
| Tools that keep per-checkout state in `node_modules/.cache` (xo) break when worktrees share one `node_modules` | `s2_problem` | Document: each worktree needs its own caches (not a Sluice bug) |
| Only declared gates are controlled. Raw Bash commands that are not gates (`docker build`, `make`, `pnpm run build`, commands inside `sh -c` or scripts) bypass Sluice, and a cache hit produces no build artifacts | review with the user | Command control must go beyond hooks and gates (workstream A) |
| A documentation-only change re-runs every gate: the key is the whole tree | dogfooding | Per-gate path scoping (workstream D) |

## 2. Goal and definition of done

Sluice v0.3.0 is "alpha": early adopters can install it on any Linux
project, agents cannot overload the machine through raw shell commands,
the throughput under overload is at least as good as without Sluice while
doing less work, and every claim is backed by a reproducible harness run
without AI sessions.

Done when:
1. Every task in section 5 is implemented with its acceptance criteria met
   and its tests green through `sluice run fmt|clippy|test`.
2. The alpha harness (section 6) passes every scenario; numbers are
   recorded in [[sluice/changes/alpha-readiness.md]].
3. Design docs and ADRs describe the implemented behaviour; a
   reconciliation pass reports no open discrepancy.
4. CHANGELOG, issue templates, README limitations and the alpha badge are in.
5. `v0.3.0` is released with `scripts/release.sh 0.3.0`, the release
   workflow is green, and `sluice update` moves the user's install to 0.3.0.
6. Latency (workstream L): the overload targets are met, or the tuning
   loop L3 has run its three rounds and the numbers, the remaining gap and
   its cause are recorded; "idle CPU while jobs wait" is measured and below
   its target.
7. Beta hardening that does not need outside users (workstream H) is done:
   CI matrix, authenticated remotes for the merge queue, the written
   compatibility promise with golden-file tests, crates.io readiness
   (dry run only), and command control for agents other than the agent
   plugin (shell shims).

Non-goals: crates.io publication (declined by the user for now; H4 only
proves readiness with `cargo publish --dry-run`); macOS or
Windows; caching build artifacts (outputs) — Sluice caches verdicts, and
build commands get queue-only treatment instead.

## 3. Rules for the orchestrator

- Read this plan, [[sluice/plan/master-plan.md]], `CLAUDE.md` and the user's
  global rules before starting. English everywhere; no AI attribution in
  git history (check every message with
  `git log -1 --format=%B | grep -iE "claude|anthropic|🤖"`, the exact
  pattern, never a narrowed one; say "agent plugin", never the product
  name).
- **Dogfooding is mandatory**: full verification is
  `sluice run fmt && sluice run clippy && sluice run test` (never raw
  `cargo test --workspace`); every brief says so. Reinstall the binary from
  a clean worktree of HEAD after daemon/CLI changes land
  (`git worktree add --detach <scratch> HEAD && cargo install --locked --path <scratch>/crates/sluice-gate --target-dir <scratch>/target`), then `sluice daemon restart`.
- **No AI sessions in tests.** Agents in tests are scripts (`scripts/alpha/mcp_agent.py` speaks MCP over stdio; CLI calls for hooks). Real `claude -p` runs are not part of this plan. If an end-to-end check of the agent plugin is unavoidable, it uses `--model haiku` and is stated as such.
- Sub-agents implement; the lead keeps design decisions, ADRs, merges,
  commits, the master plan and the release. Model per task is given in
  section 5 (Sonnet for bounded specs, Opus for scheduler, executor,
  protocol, cache keys, hooks semantics and reviews).
- Parallel agents never share files. Tasks that touch the same crate run
  sequentially or in `isolation: "worktree"`; when a worktree agent starts,
  check its base commit (`git log -1`) and reset it to the current HEAD if
  it is older.
- Never `rm -rf` outside directories the task created; temp dirs come from
  `tempfile`/`mktemp` and live in the scratchpad, except Unix socket dirs,
  which must be short (`/tmp/sa-*`).
- Every task ends with a KB change note under `kb/sluice/changes/`,
  updated design docs, and a commit (Conventional Commits). Push when CI is
  green on the previous push.
- Formats frozen at version 1 (`.sluice.toml`, markers, protocol, status
  JSON schema) may only gain optional fields; any other change stops and
  asks the user.

## 4. Orchestration waves

```
Wave 1 (parallel, independent)   Wave 2 (design gate)            Wave 3 (implementation)           Wave 4 (prove and tune)          Wave 5 (ship)
C  socket fallback     [opus]    A0 command-control design       A1 sluice exec + queue-only       E2 s2 rerun + targets            R1 adversarial review
F  intake + CHANGELOG  [sonnet]  B0 scheduler + latency design   A2 guard rewrite + equivalences   L2 latency policies              R2 fixes
E1 harness s3,s4,s6    [sonnet]  D0 path scoping design          B1 scheduler changes              L3 tuning loop (<= 3 rounds)     R3 reconciliation
G  docker/build docs   [sonnet]  H3 compatibility promise draft  L1 timeline + idle-CPU metric     E3 s7 command control            R4 release v0.3.0
H1 CI matrix           [sonnet]  (lead writes ADRs 0010-0013)    D1 path-scoped keys               E4 s5 merge queue
H4 crates.io dry run   [sonnet]                                  H5 shell shims (other agents)     H2 authenticated remotes
                                                                 H3 golden-file tests              E5 full harness run
```

Wave 2 is a lead step: the design agents report, the lead writes and
accepts ADR-0010 (command control, including shell shims), ADR-0011
(scheduler throughput and latency), ADR-0012 (path-scoped keys) and
ADR-0013 (compatibility promise). No stop for the user unless a frozen
format would change.

Within wave 3: A1 → X1 → B1 (all three touch the daemon); A1 → A2 and A1 → H5 (both build on `sluice exec`); B1 and
L1 after A1 and together in one agent (both touch the scheduler and the
store); D1 in parallel with A in a worktree; H3 golden files in parallel
(tests and fixtures only). Within wave 4: L2 → L3 strictly after E2 has
produced the first numbers; H2 in parallel with L (queue crate and test
servers only).

## 4a. Progress

Updated by the R3 reconciliation (2026-10-23). Evidence lives in the
change notes named in the last column.

| Task | State | Evidence |
|---|---|---|
| C socket fallback | done | [[sluice/changes/socket-fallback.md]] |
| F user intake, CHANGELOG | done | [[sluice/changes/user-intake.md]] |
| G builds and Docker guidance | done | [[sluice/changes/gates-cookbook.md]] |
| E1 harness s3, s4, s6 | done | [[sluice/changes/alpha-readiness.md]] |
| X1 fixes from the chaos harness | done | [[sluice/changes/chaos-fixes.md]] |
| H1 CI matrix | done | [[sluice/changes/ci-matrix.md]] |
| H4 crates.io dry run | done | [[sluice/changes/packaging.md]] |
| A0 / B0 / D0 designs, ADR-0010..0012 | done | [[sluice/design/command-control.md]], [[sluice/design/scheduler-throughput.md]], [[sluice/design/path-scoped-keys.md]] |
| H3 compatibility promise and golden files | done | [[sluice/changes/compat-golden-files.md]], ADR-0013 |
| A1 `sluice exec`, queue-only gates | done | [[sluice/changes/command-control-exec.md]] |
| A2 guard rewrite | done | [[sluice/changes/guard-rewrite.md]] |
| B1 scheduler changes, L1 timeline | done | [[sluice/changes/scheduler-throughput.md]] |
| D1 path-scoped keys | done | [[sluice/changes/path-scoped-keys.md]] |
| H5 shell shims | done | [[sluice/changes/shell-shims.md]] |
| H2 authenticated remotes | done | [[sluice/changes/queue-transports.md]] |
| E2, E3, E4 harness runs | done | [[sluice/changes/alpha-readiness.md]] |
| L2, L3 latency policies and tuning | done | [[sluice/changes/alpha-readiness.md]] (L3 rounds 1–3; L2 per-policy ablation: learned weights decisive, parallel verify helps, the others neutral; supersede deferred by ADR-0011) |
| R1 adversarial review | done | [[sluice/changes/alpha-review.md]] |
| R2 review fixes | done | [[sluice/changes/alpha-review-fixes.md]], [[sluice/changes/ci-flakes.md]] |
| E5 full harness run | done | 320/320 checks ([[sluice/changes/alpha-readiness.md]]) |
| R3 reconciliation | done | [[sluice/changes/alpha-reconciliation.md]], open discrepancies: none |
| R4 release v0.3.0 | done | alpha badge, `v0.3.0` published, `sluice update` on the user install, s6 v0.2.0 → v0.3.0 46/46, v0.3.0 golden files |

## 5. Tasks

Each task: goal, files, acceptance, tests, model, depends.

### C — Socket path fallback (wave 1, Opus)

- Goal: the daemon starts and clients connect even when
  `$XDG_RUNTIME_DIR/sluice/daemon.sock` exceeds `SUN_LEN` (108 bytes).
- Design: when the preferred path is too long, use
  `/tmp/sluice-<uid>/daemon.sock` (directory created 0700, owner checked,
  refused if not owned by the uid or group/world writable); both daemon and
  client compute the same path deterministically; `SLUICE_SOCKET` still
  wins and is validated with a clear error. No abstract sockets (they
  bypass file permissions).
- Files: `crates/sluice-gate/src/dirs.rs`, daemon socket bind, client
  connect, `doctor.rs` (new check `socket-path`: ok / fallback in use /
  too long), `kb/sluice/design/protocol.md` (socket location),
  `docs/debugging.md`.
- Acceptance: with `XDG_RUNTIME_DIR` set to a 120-byte path, `sluice run`
  goes through the daemon (status shows the job, coalescing works);
  `doctor` reports the fallback; an unsafe fallback dir (wrong owner or
  mode 0777) is refused with a message.
- Tests: integration in `crates/sluice-gate/tests/daemon_cli.rs`; unit tests
  for the path choice and the safety checks.

### F — User intake (wave 1, Sonnet)

- `.github/ISSUE_TEMPLATE/bug_report.yml` (asks for `sluice --version`,
  hook manager, agent or human, gate definition, and the attached
  `sluice doctor --bundle`), `feature_request.yml`, `config.yml`
  (no blank issues; link to discussions/docs).
- `CHANGELOG.md` in Keep a Changelog format, backfilled for 0.1.0 and
  0.2.0 from the KB change notes and `git log`, an `Unreleased` section that
  every later task appends to; `scripts/release.sh` moves `Unreleased` to the
  new version (extend the script; test it on a copy).
- README: a "Known limitations" section (from the final report and this
  plan), the badge stays pre-alpha until R4.
- `.github/workflows/release.yml`: release notes body from the matching
  CHANGELOG section instead of `generate_release_notes` (keep generated
  notes appended).
- Acceptance: templates render (validate YAML with python), release.sh on a
  scratch copy produces the right CHANGELOG diff.

### E1 — Harness scenarios s3, s4, s6 (wave 1, Sonnet, scripts only)

Against the current binary (they must keep passing after waves 3–5).
Build on `scripts/alpha/lib.py` and `mcp_agent.py`; each scenario writes
`<out>/<scenario>.jsonl` with one record per check (as `s1_hooks.py`).

- `s3_correctness.py` — invariants under concurrency, Python project:
  1. 16 concurrent `verify` calls on one tree → exactly 1 run; all 16 get
     the same job id and verdict.
  2. 16 concurrent requests on 16 distinct trees → 16 runs, never more than
     the budget at once (sample `/proc`).
  3. A tree whose gate fails is never reported as pass, including after a
     concurrent pass on a sibling tree that differs by one byte.
  4. Untracked non-ignored file → miss; ignored file → hit; file mode
     change (chmod +x) → miss; rename → miss; symlink target change → miss.
  5. `key_inputs` output change (fake `python3 --version` via a PATH shim)
     → miss; `key_files` content change → miss; TTL expiry (short TTL in user
     config) → miss.
  6. The user's index, `index.lock` and `.git/objects` are byte-identical
     before and after 200 cache checks (hash them).
  7. `CI=1` disables the cache; `SLUICE_DISABLE=1` bypasses everything.
- `s4_chaos.py`:
  1. SIGKILL the daemon while 6 gates run → no gate process survives
     (`descendants` and `/proc` scan for the gate marker), next client call
     restarts the daemon in < 2 s, no stale lock, interrupted runs are
     reported as failed/killed to their clients, never as pass.
  2. SIGKILL 10 clients mid-wait → with `last_subscriber_gone = cancel`
     their jobs are cancelled and processes gone; with `finish` they finish
     and write markers.
  3. 64 concurrent clients hammering status/run/verify for 60 s → no
     error other than documented ones, daemon RSS stays bounded (sample
     `/proc/<pid>/status`), fd count stable.
  4. Synthetic CPU pressure (python busy loops on every CPU) → new jobs
     wait (`psi_blocked_ms_total` grows), resume when the load stops; the
     `psi_max_wait` guard admits a starving job.
  5. Unwritable storage (mounting a small tmpfs needs root, so simulate it
     by making the markers dir and then the state dir read-only) → runs
     still return verdicts, the write error is reported once per run, no
     crash, `doctor` reports it.
  6. Corrupt marker files and a truncated SQLite WAL → ignored or
     recovered, `doctor` reports them.
- `s6_upgrade.py`: download the v0.1.0 and v0.2.0 release binaries
  (`gh release download` or the installer with `SLUICE_VERSION`), run
  v0.1.0 daemon, create markers and job history on a test repo, upgrade with
  `sluice update --version v0.2.0 --yes` (with `SLUICE_UPDATE_BASE_URL`
  unset, real release) and then to the binary under test; after each step:
  old markers still hit, `status` history intact, `queue_schema` created on
  first queue use, daemon restarted by `update`, `doctor` clean.
  The same scenario is rerun at R4 for v0.2.0 → v0.3.0.

### G — Builds and Docker guidance (wave 1, Sonnet)

- `docs/gates-cookbook.md`: gates for Node (npm/pnpm/yarn), Python
  (pytest/mypy/ruff), Rust, Go, Docker (`docker build` with
  `key_inputs = ["docker image inspect --format '{{.Id}}' <base>"]` or
  `cache = false`), monorepos; the per-worktree caches warning (xo,
  `node_modules/.cache`, `.mypy_cache`, `target/`); when to use
  queue-only gates (after A1). Linked from README and the skill reference.

### A0 / B0 / D0 — Designs (wave 2, three Opus agents in parallel, read-only + design docs)

- A0 `kb/sluice/design/command-control.md`: how Sluice controls commands
  that are not hook gates:
  - `sluice exec [--weight N] [--label L] -- <cmd...>`: runs any command
    through the daemon under the budget, no cache, coalescing only when
    explicitly keyed (`--coalesce-key`), output streamed to the caller,
    exit code propagated, signals forwarded (Ctrl-C cancels the job),
    works without a daemon (in-process fallback, like `run`).
  - Gate option `cache = "queue-only"` (or a new `mode` field; decide,
    respecting the frozen format: new optional field only): always runs,
    queued, coalesced between simultaneous identical requests on the same
    tree, never writes a pass marker; for builds whose outputs matter.
  - Guard hook rewrite: for a Bash command matching a gate, return
    `updatedInput` with `sluice run <gate> -v -- <extra args>` (if the
    Claude Code version supports allow + updatedInput; verify on the docs
    and with one haiku session at most, else keep deny); for a command
    matching the heavy-command list (configurable in the user config
    `[agent] heavy_commands`, default list: docker build, docker compose
    build, pnpm/npm/yarn build and run build, cargo build/test/clippy, go
    build/test, make, gradle, mvn, tsc, vite build, webpack, next build,
    pytest, jest, vitest), rewrite to `sluice exec --label <first words> --
    <cmd>`; never rewrite inside quoted strings, heredocs, pipelines into
    interactive tools, or commands with `&` backgrounding (allow them and
    log); equivalence normalisation (`npm|pnpm|yarn [run] <script>`,
    `npx`, `pnpm -C dir`, leading `cd dir &&`, env assignments).
  - What the agent sees (output, exit code, a one-line note that the
    command went through Sluice), how a human disables it
    (`SLUICE_GUARD=0`), and security limits.
- B0 `kb/sluice/design/scheduler.md` revision + ADR-0011 draft. Inputs:
  the overload numbers in section 1. Options to evaluate and decide:
  - default budget = online CPUs (not CPUs − 1) and documented weight
    guidance (weight ≈ threads the gate uses);
  - PSI-driven over-admission: when `cpu some avg10` is below a low-water
    mark, admit beyond the nominal budget up to a cap (e.g. 1.5x), so idle
    CPU is used; the existing high-water hysteresis still blocks;
  - learned weights: record each gate's measured CPU (utime+stime / wall
    from `/proc/<pid>/stat` of the process group) and use it as the
    effective weight after N runs, bounded by the configured weight;
  - fairness: round-robin between sessions/worktrees so one agent's burst
    does not starve others (p95 latency);
  - supersede: when the same session/worktree requests a newer tree for a
    gate, cancel its queued (not running) older request (configurable,
    default on for agents);
  - expose queue wait in the `verify` result and `status`.
  Target for E2: under the overload variant, wall ≤ baseline and p95
  verify latency ≤ 1.2x baseline, while suite executions stay at the
  distinct-tree count. If the target is unreachable, document why.
- D0 `kb/sluice/design/path-scoped-keys.md` + ADR-0012 draft: optional gate
  fields `paths = [...]` / `paths_ignore = [...]` (git pathspec syntax);
  the cache key uses the hash of the filtered tree (computed from
  `git ls-tree -r` of the private index, filtered, hashed deterministically)
  instead of the root tree; correctness rules (config file always
  included; a gate with `paths` must still rerun when `key_inputs` or
  `key_files` change; documentation of the risk); hook and queue paths
  use the same derivation; marker compatibility (old markers keep working
  for gates without `paths`); latency budget on the hit path (< 35 ms on
  5 000 files).

### X1 — Fixes from the E1 harness findings (wave 3, Opus, after A1, before B1)

Found by `s4_chaos.py` on 0.2.0 (evidence in
[[sluice/changes/alpha-readiness.md]]):

- SIGKILL of the daemon leaves gate process groups alive and the restarted
  daemon neither reaps nor counts them: on start, reconcile every job row
  still `running` (pgid + start ticks), kill surviving groups
  (`terminate_group`, verified with `process_matches`), mark the jobs
  killed; additionally make gate groups die with the daemon where Linux
  allows it without `unsafe` (decide: `PR_SET_PDEATHSIG` through a crate
  that wraps it safely, or a supervisor approach) and document the choice.
- An unwritable `logs/` (or state dir) prevents the gate from running: run
  it with output discarded (or kept in memory for the tail) and report the
  log error once.
- `doctor`: report an unwritable markers dir, corrupt or key-mismatched
  marker files, and a database the daemon cannot open (exit 1).
- Tests: the failing `s4_chaos.py` checks pass, plus Rust integration tests
  for each fix.

### A1 — `sluice exec` and queue-only gates (wave 3, Opus)

- Implements A0 parts 1–2: CLI subcommand, protocol additions (optional
  fields only: run request with `exec` kind and no key; protocol stays 1),
  daemon executor path, coalescing by explicit key, in-process fallback,
  signal forwarding, `status` shows exec jobs with their label.
- Tests: exit code and output fidelity (stdout/stderr interleaving,
  binary output, large output), Ctrl-C/SIGTERM forwarding kills the group,
  budget respected with 20 concurrent `exec` (counter + `/proc`),
  queue-only gate never writes a marker and coalesces simultaneous
  requests, daemon down → in-process.

### A2 — Guard rewrite and equivalences (wave 3, Opus, after A1)

- Implements A0 part 3 in `plugin/hooks/guard.sh` (POSIX sh, fail open,
  jq optional), extends `plugin/hooks/test-guard.sh` with ≥ 60 fixtures
  (each rewrite rule, each refusal to rewrite, quoting, heredocs, `&&`,
  `;`, pipes, `cd`, env prefixes, npm/pnpm/yarn equivalences, `sh -c`,
  malformed input), updates the skill and `docs/agents.md`.
- If allow + `updatedInput` is not supported by the installed Claude Code
  version, the hook keeps deny for gates and uses deny with an exact
  `sluice exec -- …` suggestion for heavy commands; document it.

### B1 — Scheduler changes (wave 3, Opus, after A1)

- Implements the options accepted in ADR-0011, with unit tests in
  `scheduler.rs` (admission decisions as pure functions over a snapshot),
  integration tests for fairness and supersede, PSI over-admission with a
  fake PSI source, learned weights persisted in SQLite (new table, schema
  migration tested from v0.2.0 databases), `status` and `verify` show
  queue wait.

### D1 — Path-scoped keys (wave 3, Opus, worktree, parallel to A)

- Implements ADR-0012 in `sluice-gate-core` (`tree.rs`, `key.rs`,
  `config.rs`, `lookup.rs`) and the queue's key derivation; hit-path
  latency re-measured with `scripts/bench-hit-path.sh` and recorded.
- Tests: doc-only change hits a `paths = ["src/**"]` gate; a change under
  `src/` misses; config change always misses; `paths_ignore`; rename across
  the filter boundary; old markers of unscoped gates still hit.

### L — Latency and wasted waiting (waves 3–4)

Problem: under overload, jobs waited while CPUs were idle (two weight-8
jobs in a 19-unit budget on 20 CPUs) and p95 `verify` latency doubled.
Waiting is fine while the CPU is full; waiting with idle CPU is waste.

- L1 instrumentation (wave 3, Opus, with B1): per-job timeline
  (requested, queued, admitted, started, finished, subscribers, wait
  reason) kept in SQLite and exposed in `status --json` (optional
  fields) and `sluice logs --timeline`; a daemon-side utilisation sampler
  (`/proc/stat` busy vs idle every second) so the daemon can report
  "idle CPU while the queue was not empty" (`totals.idle_while_waiting_ms`,
  optional field). The harness reads both instead of guessing.
- L2 latency policies (wave 4, Opus): on top of B1, implement and measure
  in this order, keeping each only if it helps E2:
  1. shortest-expected-job-first with ageing, using learned durations per
     gate (median of the last runs), never starving a job beyond
     `max_wait`;
  2. per-session fairness (round-robin across sessions/worktrees at equal
     priority);
  3. supersede (queued older tree of the same session and gate is
     cancelled; its subscribers move to the newer job only if they asked
     for "latest");
  4. early verdict: a `verify` with several gates returns as soon as one
     fails, cancelling the others that nobody else waits for;
  5. human-waiting priority: a hook run by a human in `wait` policy is
     admitted before agent requests of equal weight.
- L3 tuning loop (wave 4, lead + Opus): after each change run E2 (normal,
  overload, low-core). Targets: wall ≤ baseline; p95 verify latency ≤ 1.2x
  baseline; heavy suite executions = distinct-tree count; idle CPU while
  jobs wait < 10 % of the run's wall time; peak load under Sluice not
  above the baseline's. Up to three rounds of hypothesis → change → rerun,
  each recorded in [[sluice/changes/alpha-readiness.md]] (numbers before
  and after, what was kept, what was reverted). If a target is still
  missed after round three, stop tuning and record the gap and its cause.

### H — Beta hardening that does not need outside users

- H1 CI matrix (wave 1, Sonnet): `.github/workflows/ci.yml` gains a matrix
  of containers running `cargo test --workspace` (Sluice itself is not
  available inside CI, so CI keeps cargo): Ubuntu 22.04 (git 2.34),
  Ubuntu 24.04 (git 2.43), Debian 12 (git 2.39), Fedora latest, Alpine
  latest (musl); a job that builds git 2.31.0 from source and runs the
  core and CLI integration tests against it (the declared minimum); a
  native aarch64 job on `ubuntu-24.04-arm` running the full suite; PSI
  absent containers must pass (the fallback path). Keep the main job
  fast; the matrix may run on push to main and on tags only.
- H2 authenticated remotes for the merge queue (wave 4, Opus): tests and
  an `s5` variant where the bare remote is reached through (a) SSH: an
  unprivileged `sshd` on a high port with a generated host key and an
  authorised client key loaded in a test `ssh-agent`, the approver's
  `SSH_AUTH_SOCK` reaching the push through the allowlist; (b) smart HTTP:
  `git http-backend` behind a small Python CGI server with basic auth and
  a `GIT_ASKPASS` helper. Cases: push lands; wrong key or password fails
  definitively without hanging (bounded by `network_timeout`); credentials
  never appear in the database, logs or `doctor --bundle`; reconcile works
  over both transports. Skip cleanly with a reason when `sshd` is not
  installed.
- H3 compatibility promise (draft in wave 2, tests in wave 3, Sonnet with
  the lead deciding): `docs/compatibility.md` and ADR-0013 state what is
  stable from 0.3.0 (`.sluice.toml` version 1 with optional additions
  only, marker files, protocol 1, status JSON schema 1, queue status
  schema 1, user config, exit codes, CLI flags), how changes are made
  (additive only; breaking changes need a new version number, a migration
  path and one release of deprecation warnings), and how the database
  migrates. Golden files under `crates/*/tests/fixtures/compat/`: configs,
  markers and miss records written by v0.1.0 and v0.2.0 (generated with the
  release binaries), protocol transcripts recorded from v0.1.0/v0.2.0
  clients and servers (hello/run/status/logs), status JSON from each
  version; tests prove the current code reads and answers them.
- H4 crates.io readiness (wave 1, Sonnet, no publishing): `cargo publish
  --dry-run` for each crate in dependency order (core, proto, daemon,
  queue, mcp, gate), complete package metadata (description, readme,
  keywords, categories, documentation), a short README per library crate,
  `cargo package --list` reviewed for stray files; results in
  [[sluice/changes/packaging.md]]. Publishing stays a user decision.
- H5 other agents (wave 3, Opus, after A1): agent-agnostic command control
  through shell shims: `sluice shim install [--dir DIR]` writes small
  wrappers named after the heavy commands of the A0 list (or the user
  config list) into `~/.local/share/sluice/shims`; each wrapper resolves
  the real binary further down `PATH` (never itself), and when it runs
  inside an agent session (agent detection) or with `SLUICE_SHIMS=1`
  executes `sluice exec --label <cmd> -- <real> "$@"`, otherwise execs the
  real binary unchanged; `sluice shim uninstall`, `sluice shim status`;
  docs show how to put the shim dir first in `PATH` for agent shells
  (Codex, Cursor, Aider and others that run a shell) plus a ready-to-paste
  `AGENTS.md` block. Tests: wrapper resolution with nested PATHs, no
  recursion, exit codes and stdin/stdout/tty fidelity, human sessions
  untouched, concurrency bounded when 10 shells call a shimmed `make` at
  once. E3 gains a shim variant.

### E2–E5 — Prove (wave 4, Sonnet for scripts, lead runs and interprets)

- E2: rerun `s2_problem.py` normal and overload variants with the new
  scheduler; record both against the targets of B0; also a variant on a
  low core count (`taskset -c 0-5` for every process of the run) to model
  a laptop.
- E3 `s7_command_control.py`: simulated agents issue raw Bash-like commands
  through the guard hook exactly as the agent client would (feed the hook
  JSON on stdin, execute the command it returns): 10 agents run `pnpm run
  build` / `docker build`-like heavy commands at once → concurrency within
  budget, outputs (a build artifact file) exist after each run, exit codes
  propagated, a gate command is answered from the cache when its tree was
  verified.
- E4 `s5_merge_queue.py`: on the Node project with a bare remote: 6
  simulated agents enqueue (2 conflicting pairs, 1 failing gate), a human
  approves through the CLI; the remote moves only on approval; conflicts
  reported with paths; the failing member splits out; SIGKILL during push
  then `reconcile` lands or re-queues deterministically.
- E5: `scripts/alpha/run_all.sh` runs every scenario, writes
  `scripts/alpha/report.py` output (markdown tables) into
  [[sluice/changes/alpha-readiness.md]]; all checks pass.

### R1–R4 — Ship (wave 5)

- R1 adversarial review (Opus, read-only) of A1, A2, B1, L1, L2, D1, C,
  H2 and H5: races, signal handling, guard rewrite and shim injection (a
  crafted command must never produce a rewritten command that does more
  than the original; a shim must never recurse or run something other than
  the next real binary in `PATH`), key correctness of path scoping,
  starvation in the latency policies, credential leaks in H2.
- R2 fixes (Opus), each with a regression test; rerun E5.
- R3 reconciliation (Sonnet): design docs, ADRs 0010–0012, README,
  `docs/`, `--help`, skill, CHANGELOG vs code; report in
  `kb/sluice/changes/alpha-reconciliation.md` with "Open discrepancies:
  none".
- R4 release: badge to alpha, `scripts/release.sh 0.3.0`, push main and
  tag, watch the release workflow, `sluice update --yes` on the user's
  install, rerun `s6_upgrade.py` for v0.2.0 → v0.3.0, update the final
  report and the master plan.

## 6. Harness inventory (no AI sessions)

| Scenario | Script | State |
|---|---|---|
| Real stacks and hook managers | `s1_hooks.py` | done, 36/36 |
| The original problem (normal + overload) | `s2_problem.py` | done; targets met after B1 and L3 ([[sluice/changes/alpha-readiness.md]]) |
| Correctness invariants | `s3_correctness.py` | done (E1) |
| Chaos | `s4_chaos.py` | done (E1, X1) |
| Merge queue on a real project | `s5_merge_queue.py` | done (E4) |
| Upgrade with live state | `s6_upgrade.py` | done (E1), rerun at R4 |
| Command control (exec, guard rewrite, shell shims) | `s7_command_control.py` | done (E3, with the H5 shim variant), 64/64 |
| Merge queue over SSH and smart HTTP | Rust tests `crates/sluice-gate/tests/queue_transports/` | done (H2); the `s5_merge_queue.py --transport ssh|http` harness variant was not implemented |

Test beds: `sindresorhus/slugify` (Node, xo + ava) and
`pallets/itsdangerous` (Python, pytest + mypy), cloned into the scratch
area with dependencies installed once (`npm install`; Python tools via
`pip install --target` because the system has no `venv`); each sandbox
links packages and keeps its own caches. Environment variables:
`ALPHA_TESTBEDS`, `ALPHA_PYLIB`, `ALPHA_OUT`, `TMPDIR`, plus per-scenario
knobs documented at the top of each script.

## 7. Risks

- Guard rewriting runs agent-provided text through a shell: the rewrite
  must only prepend a fixed prefix to the original command string, never
  re-quote or interpolate it; R1 attacks it.
- Over-admission can bring back overload: bounded cap, hysteresis, and the
  E2 low-core run guard against it.
- Path-scoped keys can produce stale passes if a gate depends on files
  outside its paths: opt-in only, documented, config file always in the
  key.
- The v0.1.0 → v0.3.0 upgrade crosses two queue/database schema changes:
  covered by `s6_upgrade.py`.
- Latency policies interact (SJF can starve long gates, supersede can
  surprise an agent waiting on an old tree): ageing bound, opt-out per
  gate, and every policy measured on its own in L3.
- Shell shims sit in the agent's `PATH`: a wrong resolution could recurse
  or shadow a binary for humans; shims are inert outside agent sessions
  unless `SLUICE_SHIMS=1`, and R1 attacks the resolution.
- The CI matrix lengthens CI; it runs on main and tags only, not on every
  pull request.
