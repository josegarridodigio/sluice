---
title: "Plan: zero-config agent integration (v0.4.0)"
tags: [sluice, plan, beta, plugin, shims, init, scheduler]
---

# Plan: zero-config agent integration (v0.4.0)

Status: approved by the user on 2026-10-23; in progress (see section 5a).
Parent: [[sluice/plan/master-plan.md]] ("Zero-config" block). Predecessor:
[[sluice/plan/alpha-plan.md]] (v0.3.0).

## 1. Why this plan exists

On 2026-10-23 Sluice 0.3.0 was tried on a real Angular project (a private
client repository; never name it in the repository, the KB, commits or
issues: call it "an Angular project"). The machine (20 CPUs, 30 GiB)
reached a load average of 57 and a CPU PSI `some avg10` of 70 % while Sluice
itself ran one lint gate of weight 2. Everything else ran outside Sluice:

| Finding | Evidence | Consequence |
|---|---|---|
| The installed agent plugin was 0.2.0 while the binary was 0.3.0. The 0.2.0 hook only denies exact gate commands and never rewrites | `~/.claude/plugins/cache/sluice/sluice/0.2.0/hooks/guard.sh` | Plugin and binary versions must be checked against each other at session start |
| `npx ng test --watch=false` (20 vitest workers), `npx ng build`, `npm run axe:authenticated` (Playwright plus two `ng serve`) ran raw from the agent's Bash | process tree: parent `claude --resume` | The built-in heavy list knows neither the Angular CLI nor Playwright, and cannot see what a package script runs |
| With 0.3.0 and a user-level `heavy_commands_extra`, the build still escaped: `...; (time npx ng build ...) > $D/mock.log 2>&1; ...` | `/proc/<pid>/cmdline` of the agent's shell | The guard never rewrites inside subshells, `$(...)`, `sh -c` or scripts (by design). Only shell shims in `PATH` catch those, and today they must be installed and put in `PATH` by hand; `docs/agents.md` even says Claude Code users do not need them |
| A gate process (`ng lint`, in the job's process group) ran at nice 0 instead of 10; again 23 of 24 processes of a `test` job | `ps -eo pgid,ni` | Issue #1: `renice -p <leader>` runs asynchronously after spawn, children forked before it keep nice 0 |
| `sluice init` wrote a fixed template (`npm test` weight 4, `npm run lint` weight 1) in the current directory, not the repository root | scratch run | `init` must detect the stack, existing hook managers, realistic weights, and write at the root |
| A `test` gate declared with weight 8 started 20 workers | timeline + `ps` | Weights must be learnable without the user guessing them, also for `sluice exec` jobs (today fixed at half the budget) |
| The rules that made agents use Sluice lived in the user's private `~/.claude/CLAUDE.md` | user | Another user who installs the plugin gets none of them |

The user's goal: **a user installs the agent plugin and everything works,
with no CLAUDE.md, no PATH edits and no user config.** The `.sluice.toml`
still needs an explicit `sluice init`, because product code never writes the
user's working tree on its own; the agent proposes it.

## 2. Decisions taken with the user (2026-10-23)

| # | Question | Decision |
|---|---|---|
| D1 | Scope | Plugin SessionStart (shims + rules), heavy commands without config, `init` with stack detection, automatic weights + nice fix. Out of scope: gates inferred without a file, a `sluice setup` for other agents |
| D2 | Repository without `.sluice.toml` | The agent proposes `sluice init` to the user (shows the dry run, runs it only with consent). Shims still queue heavy commands there |
| D3 | When shims are active in agent sessions | Always, in every repository, with or without `.sluice.toml`. `SLUICE_SHIMS=0` stays the escape hatch |
| D4 | Binary missing or older than the plugin | The agent asks the user before installing or updating (the hook cannot prompt; it instructs the model, and gives the exact command) |
| D5 | Where `init` writes | `.sluice.toml` at the repository root, meant to be committed (versioned). No exclude by default |
| D6 | Existing hook managers (husky, lefthook, pre-commit, plain) | `init` edits them: gate commands in hooks become `sluice run --hook <hook> <gate>`. Not optional |
| D7 | Meaning of a declared numeric `weight` | A ceiling, as today: learned weights only lower it |
| D8 | Omitted weight vs the compatibility promise (ADR-0013) | An omitted `weight` keeps meaning 1. A new optional value `weight = "auto"` starts at `ceil(budget / 2)` and lowers with learning. `init` writes it for heavy gates. A binary older than 0.4.0 rejects such a file (one-way compatibility, documented) |
| D9 | Learned weights for `sluice exec` | Yes, keyed by repository and label, the exec weight as ceiling |
| D10 | How the work closes | Branches verified with Sluice, landed through the merge queue (the user approves trains), then release v0.4.0 prepared with `scripts/release.sh`; publishing waits for the user's approval |
| D11 | Implementers | Sub-agents on Sonnet 5.5. The lead keeps ADRs, merges, the master plan and the release |

## 3. Definition of done

1. On a fresh account (empty `HOME`, `XDG_*`), installing the agent plugin
   and the binary is all it takes: the first Bash command of a session already
   runs with the shims first in `PATH`, and the session's context carries the
   Sluice rules. No CLAUDE.md, no user config.
2. The 2026-10-23 incident replayed on a fixture (section 7, scenario s8)
   puts every heavy command through the daemon: raw, `npx`, package scripts,
   inside subshells, `$(...)`, `sh -c`, shell scripts, and from a subagent.
   Dev servers (`ng serve`, `vite`, `next dev`) are not queued.
3. Every process of a gate or exec job runs with the configured nice and
   I/O priority from its first instruction (issue #1 closed).
4. `sluice init` on fixture repositories (Angular, plain Node with
   npm/pnpm/yarn, Rust, Python, Go, a husky repo, a lefthook repo, a
   pre-commit repo) writes a working `.sluice.toml` at the root and rewires
   the hooks; `sluice init --dry-run` prints the same without writing.
5. `weight = "auto"` and learned exec weights work and are visible in
   `sluice status` and `sluice logs --timeline`.
6. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
   warnings` and `cargo test --workspace` are green through `sluice run
   fmt|clippy|test`; the alpha harness (s1..s7) still passes; s8 passes; the
   plugin end-to-end script passes with `--model haiku`.
7. KB change notes, ADR-0014..0016, design docs, `docs/*`, README and
   CHANGELOG describe the behaviour; a reconciliation pass reports no open
   discrepancy.
8. `v0.4.0` prepared with `scripts/release.sh 0.4.0`; push and publish only
   after the user approves.

Non-goals: gates inferred without a file; native integrations for other
agents (they keep `sluice shim install` and `SLUICE_SHIMS=1`); macOS or
Windows; queueing long-running dev servers.

## 4. Rules for the orchestrator

- Read this plan, [[sluice/plan/master-plan.md]], `CLAUDE.md` and the user's
  global rules before starting. Everything in English. No AI attribution in
  git history: check every message with
  `git log -1 --format=%B | grep -iE "claude|anthropic|🤖"` (the exact
  pattern). In commits say "agent plugin", never the product name of the
  agent. The client project of section 1 is never named anywhere.
- **Dogfooding is mandatory.** Full verification is
  `sluice run fmt && sluice run clippy && sluice run test` (or the `verify`
  MCP tool), never raw cargo. Run the full suite once at the end of a task,
  cheap checks (`cargo check -p <crate>`, one test) while implementing. After
  daemon or CLI changes land, reinstall from a clean worktree of HEAD
  (`git worktree add --detach <scratch> HEAD && cargo install --locked --path
  <scratch>/crates/sluice-gate --target-dir <scratch>/target`) and
  `sluice daemon restart`.
- **AI sessions.** Tests and harness scenarios use scripted agents
  (`scripts/alpha/mcp_agent.py`, CLI calls, hook JSON on stdin). The only
  real sessions are the spike S0 and `scripts/plugin-e2e.sh`, both with
  `--model haiku`.
- Sub-agents run on Sonnet 5.5 with `isolation: "worktree"`. When a worktree
  agent starts, it checks its base (`git log -1`) and rebases to the current
  `main` if older. Parallel agents never share files (file ownership in
  section 6).
- Every task ends with: tests, a KB change note under `kb/sluice/changes/`
  (fixes under `kb/sluice/fixes/`), updated design docs, a CHANGELOG
  `Unreleased` entry, a Conventional Commit on its branch, `verify` green on
  the committed branch, then `queue_enqueue` and stop. The user approves the
  trains (`sluice queue approve`); agents never approve and never push.
- Frozen formats (`.sluice.toml` v1, markers, protocol 1, status JSON schema
  1) only gain optional fields or values. Anything else stops and asks the
  user. `weight = "auto"` is the approved exception in kind (a new value of an
  existing key, D8).
- Never `rm -rf` outside directories the task created. Temp dirs from
  `tempfile`/`mktemp` in the scratchpad; Unix socket dirs short (`/tmp/sa-*`).
- Fail open everywhere on the agent path: a broken Sluice must never block
  the agent's Bash. The SessionStart hook and the shims exit 0 / exec the real
  command when anything goes wrong, and say so in a log line.

## 5. Waves

```
Wave 0 (lead + spike)             Wave 1 (parallel)                    Wave 2 (parallel)                 Wave 3 (prove)                 Wave 4 (ship)
S0 PATH spike        [sonnet]     T1 heavy classification  [sonnet]    T5 session-start + plugin [sonnet] E1 harness s8      [sonnet]    R1 review          [lead]
lead: ADR-0014..0016              T2 executor priority #1  [sonnet]    T3 init detection         [sonnet] E2 plugin e2e      [sonnet]    R2 fixes           [sonnet]
                                  T4 weights auto + exec   [sonnet]    T6 doctor + docs          [sonnet]                                R3 reconciliation  [lead]
                                                                                                                                        R4 release v0.4.0  [lead]
```

Dependencies: S0 → T5. T1 → T5, T3 (T3 reuses the canonical matcher to
match hook lines against gates). T4 (config part) → T3 (init writes
`"auto"`). T2 and T4 both touch `sluice-gate-daemon` but disjoint files
(`executor.rs` vs `scheduler.rs`, `server.rs`, `store.rs`): separate
worktrees, merged in order T2 then T4. T6 after T1..T5 have landed. E1 and E2
after wave 2. Nine sub-agents in total, at most three at once.

## 5a. Progress

| Task | State | Evidence |
|---|---|---|
| S0 PATH spike | done: option 2 (`CLAUDE_ENV_FILE`), plugin `bin/` is last in PATH | [[sluice/research/plugin-path-spike.md]] |
| ADR-0014..0016 | done | `kb/sluice/design/adr/0014..0016` |
| T1 heavy classification | in progress | |
| T2 gate priority (#1) | in progress | |
| T4 automatic weights | in progress | |
| T5 session start + plugin | pending | |
| T3 init detection | pending | |
| T6 doctor + docs | pending | |
| E1 harness s8 | pending | |
| E2 plugin e2e | pending | |
| R1..R4 | pending | |

## 6. Tasks

Each task: goal, design, files (ownership), acceptance, tests, depends.

### S0 — PATH spike (wave 0, Sonnet, real sessions with haiku)

- Goal: decide, with evidence, how the shim directory gets first in the
  `PATH` of every Bash command of a session, including subagents and
  background commands.
- Facts from the docs (2026-10-23): files in a plugin's `bin/` directory
  are on the Bash tool's `PATH` while the plugin is enabled (position not
  documented). A SessionStart hook gets `CLAUDE_ENV_FILE`; its content is run
  as shell code before every Bash command, so
  `export PATH="<dir>:$PATH"` prepends at run time. Not documented: whether
  subagents and background Bash commands source it. `CLAUDE_PLUGIN_ROOT` is
  not in the Bash environment and changes on plugin update. PreToolUse hooks
  of a plugin fire for subagent tool calls. `CLAUDECODE=1` is set in the Bash
  tool, and `CLAUDE_CODE_CHILD_SESSION=1` in Bash only.
- Method: a throw-away plugin under the scratchpad with (a) `bin/probe` and
  `bin/npm` that print `$PATH` and their own path, (b) a SessionStart hook
  that appends `export PATH="<scratch>/shims:$PATH"` and
  `export SPIKE_ENV=1` to `$CLAUDE_ENV_FILE`. `claude -p --model haiku
  --plugin-dir <it> --output-format stream-json` sessions that: run `echo
  $PATH; command -v npm` in the main session, in a background Bash command,
  in a subshell, in `sh -c`, in a script file, and from a subagent (Task
  tool); start with `--resume` and after `/clear` if scriptable.
- Output: `kb/sluice/research/plugin-path-spike.md` with a table (context ×
  mechanism → shim first in PATH yes/no), the Claude Code version, and the
  recommendation among:
  1. plugin `bin/` alone, if it is prepended everywhere;
  2. `CLAUDE_ENV_FILE` prepend of the stable shim dir (default), if
     subagents and background commands source it;
  3. (2) plus a guard fallback: for a Bash call whose hook input carries
     `agent_id` (subagent) and whose `PATH` lacks the shim dir, the guard
     prefixes the command with `export PATH='<dir>':"$PATH"; ` (one rewrite,
     visible in the note). Choose 3 only if (2) fails for subagents.
- Acceptance: the lead can pick the mechanism for T5 from the table alone.
- Files: scratchpad and the research note only.

### Lead step after S0 — ADRs

- ADR-0014 "Zero-config agent integration": SessionStart hook, shims always
  on in agent sessions (D3), the PATH mechanism chosen from S0, consent for
  installing or updating the binary (D4), proposing `init` (D2), the rules
  injected as context and their size limit, fail-open.
- ADR-0015 "Automatic weights": `weight = "auto"` (D8) semantics and its
  compatibility note (amend ADR-0013 and `docs/compatibility.md`: a file
  that uses `"auto"` needs 0.4.0); learned exec weights (D9); declared
  numbers stay ceilings (D7).
- ADR-0016 "`sluice init` detection and hook rewiring": stack detection
  rules, gates derived from existing hooks first, hook editing (D6) as an
  explicit, user-invoked exception to "never modify the user's working tree"
  (scope: `sluice init` only, never the daemon, the guard, shims or `verify`),
  idempotence, `--dry-run`.

### T1 — Heavy classification without config (wave 1, Sonnet)

- Goal: the guard and the shims classify, with no user config, the
  commands of the incident, and shims also route exact gate commands to the
  cache.
- Design:
  1. **Built-in list** (`DEFAULT_HEAVY_COMMANDS`): add `ng build`, `ng test`,
     `ng lint`, `ng e2e`, `nx run`, `nx run-many`, `nx affected`,
     `nx build`, `nx test`, `nx lint`, `nx e2e`, `turbo run`, `turbo build`,
     `turbo test`, `turbo lint`, `playwright test`, `cypress run`,
     `react-scripts build`, `react-scripts test`, `astro build`,
     `nuxt build`, `dotnet build`, `dotnet test`, `bazel build`,
     `bazel test`, `ninja`, `cmake --build`. Never dev servers or watchers
     (`ng serve`, `vite` alone, `next dev`, `nx serve`, `* --watch`): add a
     `NEVER_HEAVY` rule so `ng test --watch` (no `=false`) and
     `vitest --watch` stay direct (they never end). Update the
     `shim_names_cover_the_default_list` expectation (new shims: `ng`,
     `nx`, `turbo`, `playwright`, `cypress`, `react-scripts`, `astro`,
     `nuxt`, `dotnet`, `bazel`, `ninja`, `cmake`).
  2. **Package scripts** (new `crates/sluice-gate-core/src/pkgscripts.rs`):
     when canonical words start with `run <name>`, find the nearest
     `package.json` from the command's directory upwards (stop at the git
     root or `/`, file at most 1 MiB, unreadable or invalid → not heavy),
     read `scripts.<name>`, split it on `&&`, `||`, `;`, `|` (reuse a small
     word splitter in core; the guard's lexer stays in the CLI crate), and
     classify each piece with the same matcher, recursively for nested
     `npm run x` / `pnpm x` (depth ≤ 4, cycle-safe). Heavy if any piece is.
     `npm test` → script `test`. Pre/post scripts are ignored. The label
     stays the canonical words the agent typed (`run axe:authenticated`).
  3. **API**: `HeavyMatcher::classify(&self, canonical, dir: &Path,
     fs: &dyn ScriptSource) -> Option<Match>` used by both the guard
     (`guard/matcher.rs::plan_segment`, `World` gains `read_package_json`)
     and the shims (`shim.rs::route`). The existing `matches` keeps its
     meaning (pure word match) for callers that have no directory.
  4. **Shims and gates**: in `shim.rs::route`, before the heavy check, when
     the cwd is inside a repository with a `.sluice.toml` and the canonical
     words of `name args` equal the canonical words of a gate's `cmd`
     (same rule as `guard/matcher.rs::gate_edit`, exact, no extra args),
     route to `sluice run <gate> -v` (cache and coalescing) instead of
     `sluice exec`. New `Route::Gate { gate }`.
- Files (owner T1): `crates/sluice-gate-core/src/heavy.rs`,
  new `crates/sluice-gate-core/src/pkgscripts.rs`, `crates/sluice-gate-core/src/lib.rs`,
  `crates/sluice-gate/src/guard/matcher.rs`, `crates/sluice-gate/src/guard/mod.rs`
  (World impl), `crates/sluice-gate/src/guard/tests.rs`,
  `crates/sluice-gate/src/shim.rs`, `crates/sluice-gate/tests/guard.rs`,
  `crates/sluice-gate/tests/shim.rs`, `kb/sluice/design/command-control.md`.
- Acceptance (each a test): with an empty user config, in a fixture with
  `package.json` scripts `{"test":"ng test","axe":"npm run axe:a",
  "axe:a":"playwright test","start":"ng serve","lint":"eslint ."}`:
  `npx ng test --watch=false`, `npx ng build -c x`, `npm run axe`,
  `npm test`, `pnpm exec playwright test` are rewritten by the guard and
  queued by the shims; `npm start`, `npx ng serve`, `ng test --watch`,
  `npm run lint` are not. A shim call equal to a gate's cmd goes to
  `sluice run`. Guard injection and fuzz tests still pass. Shim overhead
  for a direct command stays under 15 ms (measure with `hyperfine` or a
  loop; record in the change note).
- Change note: `kb/sluice/changes/heavy-classification.md`.
- Depends: none.

### T2 — Gate priority from the first instruction, issue #1 (wave 1, Sonnet)

- Goal: every process of a gate or exec job runs with `daemon.nice` and the
  configured I/O class from its first instruction.
- Design: `unsafe_code` is forbidden in the workspace, so no `pre_exec`.
  Start the job through the standard tools, which set their own priority and
  `exec` the next program (same PID, same process group, so the reaper and
  `proc_start_ticks` are unaffected): argv becomes
  `[<nice> -n <N>] [<ionice> -c <class> -n <level>] sh -c <cmd>`, with the
  absolute paths of `nice` and `ionice` resolved once at daemon start
  (`find_in_path`). When a tool is missing, keep today's `apply_priority`
  for that half only and log at debug. Remove the asynchronous renice of the
  leader when the wrapper is used. `nice = 0` and `ionice_class = "none"`
  add no wrapper. The command shown in logs and the cache key are unchanged
  (the wrapper is not part of `cmd`).
- Files (owner T2): `crates/sluice-gate-daemon/src/executor.rs` only (plus
  its tests and `kb/sluice/design/executor.md`).
- Acceptance: a gate `sh -c 'sleep 0.3 & wait; ps -o ni= -p $$; sh -c "ps -o
  ni= -p \$\$"'` (a grandchild started at once) prints the configured nice
  for every process, 50 runs in a row, also with the machine loaded by
  `stress`-like busy loops in the test; `ionice -p` of a grandchild shows the
  class; exit codes, timeouts and kills are unchanged (existing executor and
  chaos tests green).
- Change note: `kb/sluice/fixes/gate-priority-inheritance.md`; commit
  message ends with `Fixes #1`.
- Depends: none.

### T4 — Automatic weights (wave 1, Sonnet)

- Goal: D7, D8, D9.
- Design:
  1. **Config** (`crates/sluice-gate-core/src/config.rs`): `weight` accepts
     an integer ≥ 1 or the string `"auto"` (custom serde: `Weight::Fixed(u32)
     | Weight::Auto`); omitted = `Fixed(1)` (unchanged). Error text for other
     strings names both forms. Golden-file compat tests: every old file loads
     to the same values; a new fixture with `"auto"`.
  2. **Scheduler**: `Auto` resolves at admission to `ceil(budget / 2)` (the
     daemon's budget, not the client's guess) and is a ceiling like any
     declared number; learning lowers it (existing `clamp_learned`, floor
     `ceil(ceiling / 4)`).
  3. **Exec learning**: exec jobs get a `stats_key` of (repository root,
     `exec:<label>`), so `effective_weight` applies; the ceiling is
     `--weight` when given, else `exec_weight_or(budget)`. `store.rs` keeps
     exec demand samples with the same window (`LEARN_WINDOW`) and pruning
     as gates. Exec jobs without a repository (cwd outside git) key on the
     canonical cwd.
  4. **Visibility**: `sluice status` shows `weight auto→5` / `weight 10→3`
     (configured→effective) for running and queued jobs; status JSON gains
     optional `weight_effective` and `weight_auto` (schema 1, optional
     fields); the timeline already prints demand.
  5. **Warning**: `sluice doctor` (check `weights`, implemented in T6 from a
     store query added here: `GateStats::unbounded_learned(repo, gate)`)
     warns when a gate's unbounded learned weight exceeds its declared
     ceiling by more than 50 % in the last window, suggesting the number or
     `"auto"`.
- Files (owner T4): `crates/sluice-gate-core/src/config.rs`,
  `crates/sluice-gate-core/tests/compat.rs` and fixtures,
  `crates/sluice-gate-daemon/src/scheduler.rs`,
  `crates/sluice-gate-daemon/src/scheduler/tests.rs`,
  `crates/sluice-gate-daemon/src/server.rs`, `crates/sluice-gate-daemon/src/store.rs`,
  `crates/sluice-gate-proto/src/lib.rs` (optional fields),
  `crates/sluice-gate-proto/tests/status_schema.rs`, `crates/sluice-gate/src/status.rs`,
  `crates/sluice-gate/src/exec.rs`, `docs/configuration.md` (weight row),
  `kb/sluice/design/scheduler-throughput.md`, `kb/sluice/design/config-format.md`.
- Acceptance: `"auto"` on a 20-unit budget admits at 10 and drops toward
  the measured demand after 3 runs; a fixed `weight = 8` never exceeds 8; two
  exec jobs `--label ng-lint` measured at ~1 CPU are admitted at ≤ 3 units
  from the fourth run; old configs and old status consumers are unaffected
  (compat and schema tests).
- Change note: `kb/sluice/changes/automatic-weights.md`.
- Depends: none. Merge after T2.

### T5 — Session start and plugin (wave 2, Sonnet)

- Goal: D2, D3, D4 and section 3 point 1.
- Design:
  1. **Binary**: new hidden subcommand `sluice agent session-start
     --plugin-version <X.Y.Z>` (logic in Rust, testable). Reads the hook JSON
     on stdin (`cwd`, `source`, `session_id`). Under a 300 ms watchdog (like
     the guard's), it:
     - installs or refreshes the shims in the stable directory
       (`$XDG_DATA_HOME/sluice/shims`) with the same code as `sluice shim
       install` (idempotent: rewrites only stale or missing shims, prunes old
       ones, never touches foreign files);
     - applies the PATH mechanism chosen in S0. Default: appends to
       `$CLAUDE_ENV_FILE` a guarded line
       `case ":$PATH:" in *":<dir>:"*) ;; *) export PATH='<dir>':"$PATH" ;; esac`
       (absolute path, single-quoted, no `CLAUDE_PLUGIN_ROOT`). No
       `SLUICE_SHIMS=1` needed: `CLAUDECODE` already marks the agent;
     - prints `{"hookSpecificOutput":{"hookEventName":"SessionStart",
       "additionalContext": ...}}` with at most ~1 500 characters:
       * repository with `.sluice.toml`: the rules (verify through the
         `verify` tool before commits; builds and test runners may be routed
         through the queue, never retry them without it; do not wrap builds or
         tests in subshells, `$(...)` or scripts to capture output, redirect
         instead; do not start several dev servers while gates run; never
         approve merge trains), and the gate names;
       * git repository without `.sluice.toml`: one paragraph telling the
         agent to propose Sluice once, show `sluice init --dry-run` output to
         the user and run `sluice init` only after the user agrees;
       * not a git repository: nothing but a one-line note.
     - logs one line to `logs/client.log` (`caller=agent hook=session-start
       decision=...`).
     Every failure prints nothing and exits 0.
  2. **Plugin**: `plugin/hooks/hooks.json` gains `SessionStart` (matchers
     `startup|resume|clear|compact`, `timeout` 10 s) running
     `plugin/hooks/session-start.sh`, a thin POSIX wrapper that:
     - when `sluice` is not on `PATH` (also try `~/.local/bin/sluice` and
       `~/.cargo/bin/sluice`), prints additionalContext asking the model to
       tell the user Sluice's binary is missing and, only with the user's
       consent, run the install command from the README
       (`curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh`);
     - when `sluice --version` is older than the plugin's version (read from
       `plugin.json` next to it), the same with `sluice update`;
     - otherwise execs `sluice agent session-start --plugin-version <v>`.
     If S0 chose the plugin `bin/` mechanism, `plugin/bin/` holds static shims
     generated by a script (`scripts/gen-plugin-shims.sh`, checked by a test
     against `HeavyMatcher::default().shim_names()`), each running
     `sluice shim exec <name> -- "$@"` when `sluice` is found, else the real
     command with the plugin `bin/` removed from `PATH`.
  3. **Guard fallback** (only if S0 chose option 3): in `guard/mod.rs`, the
     subagent PATH prefix described in S0.
  4. **Skill**: `plugin/skills/sluice/SKILL.md` and `reference.md`: shims are
     automatic; the rules above; the init proposal flow; what to do when the
     binary is missing. Remove any instruction that tells users to edit
     CLAUDE.md or PATH.
  5. **Plugin manifest**: description mentions the session hook. Version is
     bumped by `scripts/release.sh` only (the marketplace serves `main`, and
     the version gates updates).
- Files (owner T5): new `crates/sluice-gate/src/agent.rs` (or
  `session_start.rs`), `crates/sluice-gate/src/main.rs` (subcommand wiring
  only), `crates/sluice-gate/src/shim.rs` (expose `install_into` for reuse;
  coordinate: T1 has landed), new `crates/sluice-gate/tests/session_start.rs`,
  `plugin/hooks/hooks.json`, new `plugin/hooks/session-start.sh`,
  `plugin/hooks/test-guard.sh` (add wrapper tests), `plugin/skills/sluice/*`,
  `plugin/.claude-plugin/plugin.json` (description), `.claude-plugin/marketplace.json`
  (description), optional `plugin/bin/*`, `scripts/gen-plugin-shims.sh`,
  `kb/sluice/design/agent-plugin.md` (create if missing).
- Acceptance: with a temp `HOME`, `CLAUDE_ENV_FILE=<tmp>` and hook JSON on
  stdin: shims installed, env file prepends exactly once even when sourced
  twice, a following `sh -c '(npx ng build)'` with `CLAUDECODE=1` reaches the
  daemon (status totals +1); the three context variants; missing binary and
  older binary produce the consent text and exit 0; a broken `sluice`
  (exit 1, garbage output, hang) still exits 0 within the timeout; runs under
  300 ms warm.
- Change note: `kb/sluice/changes/session-start.md`.
- Depends: S0, T1.

### T3 — `sluice init` with detection and hook rewiring (wave 2, Sonnet)

- Goal: D5, D6, section 3 point 4.
- Design (new module `crates/sluice-gate/src/init/`: `detect.rs`,
  `hooks.rs`, `render.rs`, `mod.rs`; `main.rs` keeps only the CLI wiring):
  1. **Root**: write at `git rev-parse --show-toplevel` (through
     `sluice-gate-core::git`), not the cwd. Outside a git repository: error.
     Existing `.sluice.toml`: refuse unless `--force` (unchanged).
  2. **Gates from existing hooks first**: read husky (`.husky/<hook>`),
     lefthook (`lefthook.yml`, `.lefthook.yml`, `lefthook-local.yml` ignored),
     pre-commit framework (`.pre-commit-config.yaml`, `repo: local` hooks
     only), and plain hooks (`core.hooksPath` or `.git/hooks`, non-sample).
     Each command line that the heavy matcher (T1, with package scripts)
     classifies as heavy and that is not mutating (`--fix`, `--write`,
     `pretty-quick`, `lint-staged`, `prettier --write`, `eslint --fix` stay in
     the hook untouched) becomes a gate, named from its canonical words
     (`ng test` → `test`, `ng lint` → `lint`, `run typecheck` → `typecheck`,
     de-duplicated), tagged with that hook. Lines Sluice already runs
     (`sluice run ...`) are recognised (idempotent).
  3. **Stack detection** for gates not found in hooks (root only; nested
     projects listed as a comment):
     - Angular (`angular.json` with `lint`/`test` targets): `npx ng lint`,
       `npx ng test --watch=false`.
     - Node (`package.json`): package manager from the lockfile
       (`pnpm-lock.yaml`, `yarn.lock`, `bun.lockb`/`bun.lock`,
       `package-lock.json`, default npm); gates for existing scripts `lint`,
       `typecheck`/`type-check`, `test` (`<pm> test` with `env = { CI = "1" }`
       so jest/vitest do not watch).
     - Rust (`Cargo.toml`): `fmt`, `clippy`, `test` as in the cookbook, with
       `CARGO_BUILD_JOBS`.
     - Python (`pyproject.toml`, `setup.cfg`, `requirements*.txt`): `ruff`
       if configured, `mypy` if configured, `pytest`; `.venv/bin/` prefix when
       `.venv` exists.
     - Go (`go.mod`): `vet`, `test -p 4`.
     - Nothing detected: today's commented template.
  4. **Weights**: lint, fmt, vet, type checks → `weight = 2` (1 for fmt);
     test runners and multi-threaded builds → `weight = "auto"` (T4).
     `key_inputs` with the tool versions (`node --version`, `<pm>
     --version`, `rustc --version`, `python3 --version`, `go version`).
     `timeout` 10m for lint-like, 30m for tests.
  5. **Hook rewiring (always, D6)**: in each hook file found, replace every
     line that became a gate with `sluice run --hook <hook> <gate>` at the
     same position, keep every other line (formatters, commitlint, custom
     scripts) and the file mode. With no hook manager: write `.githooks/
     pre-commit` and `pre-push` (executable) and set `core.hooksPath` to
     `.githooks` (`git config`, local), unless `.git/hooks` has non-sample
     hooks, in which case print the snippet and say why. lefthook YAML and
     pre-commit YAML are edited textually, line by line, preserving comments
     (no YAML re-serialisation); if a line cannot be matched safely, print
     the snippet for it instead.
  6. **Output**: a summary of gates (name, cmd, weight, hooks, source:
     hook or detected), files written, lines replaced as a unified diff.
     `--dry-run` prints the same and writes nothing (the agent shows it to the
     user before asking, D2). Exit 0 on success, 1 on error, 2 usage.
  7. **Starter template** (`starter_repo_config`) stays for the "nothing
     detected" case and gains `"auto"` documentation.
- Files (owner T3): new `crates/sluice-gate/src/init/*`,
  `crates/sluice-gate/src/main.rs` (init wiring), `crates/sluice-gate/src/snippets.rs`,
  `crates/sluice-gate-core/src/config.rs` (`starter_repo_config` only; T4
  has landed), new `crates/sluice-gate/tests/init.rs` with fixtures under
  `crates/sluice-gate/tests/fixtures/init/` (angular-husky, node-pnpm,
  node-yarn-lefthook, rust-plain, python-precommit, go, empty, mixed),
  `docs/cli.md` (init), `docs/gates-cookbook.md` (pointer to init).
- Acceptance: for each fixture, the written `.sluice.toml` parses, `sluice
  doctor` reports no failure, `sluice hook print` matches the rewired hooks,
  a second `init --force` produces no diff in hooks (idempotent), and
  `--dry-run` writes nothing (tree hash unchanged). The Angular-husky
  fixture reproduces the incident's repository shape: `.husky/pre-commit`
  with `npx pretty-quick --staged`, `ng lint`, `ng test --watch=false` ends
  with the first line untouched and the other two as `sluice run --hook
  pre-commit lint|test`.
- Change note: `kb/sluice/changes/init-detection.md`.
- Depends: T1, T4 (config part), ADR-0016.

### T6 — Doctor and documentation (wave 2, after T1..T5 land, Sonnet)

- `sluice doctor` new checks: `shims` (installed, current, first in `PATH`
  for this shell; in an agent shell `warn` when not first), `plugin`
  (installed plugin version from `~/.claude/plugins/installed_plugins.json`
  when readable vs the binary: `warn` when they differ), `weights` (T4
  query), `init` (repository without `.sluice.toml`: `ok` with a hint).
- Docs: `docs/agents.md` (Claude Code: install the plugin, nothing else;
  remove "Claude Code users do not need shims"; shims section keeps other
  agents), `docs/configuration.md` (heavy list, `"auto"`), `docs/cli.md`
  (`init`, `agent session-start` hidden but documented for debugging),
  `docs/debugging.md` (how to see the session hook's decision), `README.md`
  quick start reduced to: install binary, install plugin, `sluice init`.
  CHANGELOG `Unreleased` consolidated.
- Files (owner T6): `crates/sluice-gate/src/doctor.rs`, `docs/*`,
  `README.md`, `CHANGELOG.md`.
- Change note: `kb/sluice/changes/zero-config-docs.md`.

## 7. Proof

### E1 — Harness scenario s8 "zero config" (wave 3, Sonnet, no AI sessions)

`scripts/alpha/s8_zero_config.py`, built on `lib.py`, one JSONL record per
check, added to `run_all.sh`:

1. Fresh sandbox `HOME`/`XDG_*`, the release-mode binary on a private PATH,
   no user config. Run `plugin/hooks/session-start.sh` with hook JSON and a
   temp `CLAUDE_ENV_FILE`; source the file in every later shell.
2. Fixture "angular-like": a fake `ng` (node script) whose `build` and
   `test` burn N busy threads for T seconds and write a marker, a fake
   `playwright`, `package.json` scripts like section 1, husky hooks.
3. Agent script launches, concurrently and with `CLAUDECODE=1`, the
   incident's commands: `npx ng test --watch=false`,
   `(time npx ng build -c mock) > out.log 2>&1`, `x=$(npx ng build)`,
   `sh -c 'npm run axe'`, `./scripts/build.sh` calling `npx ng build`,
   `npm start` (dev server stub, must stay direct), `ng test --watch`
   (must stay direct).
4. Checks: every heavy command appears in `sluice logs --timeline`; the
   direct ones do not; peak concurrent weight ≤ `floor(overcommit ×
   budget)`; outputs and exit codes equal the raw runs; every process of
   each job has nice 10 (sampled from `/proc` while running); `sluice init
   --dry-run` then `init` on the fixture gives gates `lint` and `test`;
   `weight = "auto"` jobs drop after 3 runs; a second session start does
   not duplicate the PATH entry.
- Change note: `kb/sluice/changes/zero-config-proof.md` with the numbers.

### E2 — Plugin end-to-end with real sessions (wave 3, Sonnet, haiku)

Extend `scripts/plugin-e2e.sh` (sessions with `--model haiku`):
(f) "build the project, capture the output in a file using a subshell" →
the build reaches the daemon; (g) a subagent asked to run the tests with
Bash → the guard or the shims route it; (h) a git repo without
`.sluice.toml`, "set up verification" → the agent shows `sluice init
--dry-run` and asks, `.sluice.toml` not created by the session;
(i) binary hidden from `PATH` → the agent reports the missing binary and does
not install it on its own. Record results in the E1 change note.

## 8. Ship

- R1 (lead): adversarial review of the merged diff against this plan:
  fail-open paths, guard injection, shim recursion, hook rewriting on odd
  files (CRLF, no trailing newline, symlinked hooks), compat of `"auto"`,
  privacy (no client names in fixtures, docs, commits, issues).
- R2 (Sonnet): fixes from R1, one branch, through the merge queue.
- R3 (lead): reconciliation of KB, ADRs, docs, CLI help and CHANGELOG
  (`kb/sluice/changes/zero-config-reconciliation.md`); master plan updated.
- R4 (lead): `scripts/release.sh 0.4.0` on `main` after the last train
  lands; show the user the tag, CHANGELOG section and plugin version; push
  the tag and publish only after the user approves; then `sluice update`
  and `claude plugin update sluice@sluice` on the user's machine, and a
  re-run of the original test on the Angular project with the user.

## 9. Risks

| Risk | Mitigation |
|---|---|
| Subagents or background Bash do not source `CLAUDE_ENV_FILE` | S0 measures it; guard fallback (option 3) |
| Plugin `bin/` or env-file behaviour changes between Claude Code versions | `doctor` `shims` check; s8 and plugin e2e record the Claude Code version; fail open |
| Shims slow every `npm`/`npx` call in agent sessions | Budget 15 ms per direct call, measured in T1 |
| Package-script classification misfires (a `test` script that starts a watcher) | `NEVER_HEAVY` rules for watch flags and dev servers; a queued watcher would hold its weight until its timeout, so watch detection has tests |
| `init` edits hooks badly | textual edits only on lines it can match exactly; diff printed; `--dry-run`; fixtures with odd files |
| `weight = "auto"` breaks teammates on older binaries | documented in CHANGELOG and compatibility; `doctor` on 0.4.0 says which binary is needed |
| The model does not wait for consent before installing | the wrapper never installs anything itself; the context says "only after the user agrees"; e2e (i) checks it |
