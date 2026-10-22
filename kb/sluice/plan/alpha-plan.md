---
title: "Plan: from pre-alpha to alpha (v0.3.0)"
tags: [sluice, plan, alpha]
---

# Plan: from pre-alpha to alpha (v0.3.0)

Status: approved for execution by the user on 2026-10-22; to be driven by a
`/goal` that implements every task below and cuts release `v0.3.0`.
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

Non-goals: crates.io publication (declined by the user for now); macOS or
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
Wave 1 (parallel, independent)      Wave 2 (design gate)         Wave 3 (implementation)            Wave 4 (prove)           Wave 5 (ship)
C socket fallback      [opus]       A0 command-control design    A1 sluice exec + queue-only        E2 s2 rerun + targets    R1 adversarial review
F intake + CHANGELOG   [sonnet]     B0 scheduler design          A2 guard rewrite + equivalences    E3 s7 command control    R2 fixes
E1 harness s3,s4,s6    [sonnet]     D0 path scoping design       B1 scheduler changes               E4 s5 merge queue        R3 reconciliation
G  docker/build docs   [sonnet]     (lead writes ADRs 0010-0012) D1 path-scoped keys                E5 full harness run      R4 release v0.3.0
```

Wave 2 is a lead step: the three design agents report, the lead writes and
accepts ADR-0010 (command control), ADR-0011 (scheduler throughput),
ADR-0012 (path-scoped keys). No stop for the user unless a frozen format
would change.

Within wave 3: A1 → A2 (A2 depends on `sluice exec`); B1 after A1 (both
touch the scheduler); D1 in parallel with A (core key derivation and
config, separate files), in a worktree.

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

- R1 adversarial review (Opus, read-only) of A1, A2, B1, D1 and C: races,
  signal handling, guard rewrite injection (a crafted command must never
  produce a rewritten command that does more than the original), key
  correctness of path scoping.
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
| The original problem (normal + overload) | `s2_problem.py` | done; overload target not met (B1) |
| Correctness invariants | `s3_correctness.py` | E1 |
| Chaos | `s4_chaos.py` | E1 |
| Merge queue on a real project | `s5_merge_queue.py` | E4 |
| Upgrade with live state | `s6_upgrade.py` | E1, rerun at R4 |
| Command control (exec, guard rewrite) | `s7_command_control.py` | E3 |

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
