# Changelog

All notable changes to Sluice are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

### Changed

### Fixed

## [0.7.0] - 2026-10-28

### Added

- **Hooks entry point** (`sluice hooks on|off|status`, optional and local to a
  clone, ADR-0021). Sluice becomes the git hooks entry point of a clone without
  editing the repository: `on` gives the clone a hooks directory under its git
  directory (one script for each hook the repository has, kept in sync) and sets
  `core.hooksPath` through a file included by the last `[include]` of the
  clone's git config, so husky's own `core.hooksPath` write on every install does
  not displace it. `off` removes it. A missing `sluice` binary never disables
  the repository's hooks. `sluice init --hooks entry` activates it instead of
  rewiring the hook files (`--hooks rewire`, the default, is the previous
  behaviour). The agent plugin's session start restores an activated clone whose
  entry point was displaced and says so; it never activates a clone by itself.
- **Hooks run through the queue and the cache.** With the entry point on, a hook
  Sluice understands completely runs its commands as jobs of the daemon, in the
  manager's order; anything else, a daemon that cannot be reached or
  `SLUICE_HOOKS_DELEGATE=1` runs the hook exactly as before (same arguments,
  stdin, directory, environment and exit code). husky and plain hooks: simple
  commands and `&&` lists; lefthook 2.x: named commands and jobs, each run by
  `lefthook run <hook> --command <name>`, when none of them can be skipped;
  pre-commit 4.x: each hook of the `pre-commit` and `pre-merge-commit` stages by
  `pre-commit run <id>`, while the worktree equals the index. Commands that may
  change files or the index (`lint-staged`, formatters with a write flag, `git`,
  unknown programs) always run; read-only checks (tests, linters, `npm run`
  scripts made of them, and the manager's equivalents) are cached by the working
  tree they saw. `sluice hooks status` shows, for each hook, whether it runs as
  units or is delegated, and why.
- **Gates without `.sluice.toml`.** In a repository with none, the check units of
  its hooks (`pre-commit`, `pre-merge-commit`, `pre-push`; named
  `<manager>:<hook>#<n>`, e.g. `husky:pre-commit#2`) are the gates of `verify`
  and `gate_list` (MCP), `sluice run <gate>` and the new `sluice gate list
  [--json]`, with the same cache entries as the hook runs: a `verify` before `git
  commit` makes the commit's hook a cache hit, and the other way round.
  `gate_list` also lists the hooks and units that are not gates and why
  (delegated hooks, mutating units, units that depend on what is staged). With a
  `.sluice.toml` its gates are unchanged; a plain-hook unit whose command equals a
  cached, unscoped gate with no `env` shares that gate's entry. The agent plugin's
  session context, skill and MCP instructions cover such repositories.
- Docs: new `docs/hooks.md` (activation, units per manager, caching, opt-outs,
  compatibility table, limits). `docs/merge-queue.md` says that the queue cannot
  land on a target the forge protects against direct pushes (it fails at push
  time with `push_rejected`); land through pull requests there (issue #5).

### Changed

- With the hooks entry point on, a hook run as units differs from the hook run by
  its manager: the output of each command goes to a job log (a one-line summary,
  and the tail on failure), checks run without git's own hook variables, and
  the `agent_hook_policy = "fail-fast"` refusal does not apply, so an agent's
  commit on a cache miss runs the units inside `git commit`; agents should
  `verify` first. What runs does not change for a clone that never ran `sluice hooks on`.
- `sluice doctor` no longer tells you to ignore a hook manager's hooks that
  bypass Sluice: the `hooks` check fails when a manager owns hooks that do not
  call sluice (suggesting `sluice hooks on`), and fails with the reason when an
  activated entry point is displaced or a hook of the repository has no script.
  `repo-config` is `ok` for a missing `.sluice.toml` in a clone whose hooks go
  through Sluice.

## [0.6.3] - 2026-10-26

### Added

- The daemon log says when a job waits for memory: one `info` line when a job
  first becomes memory-blocked (`waits for memory`, with the expected peak, free
  memory and the running jobs' headroom) and one when it starts after waiting
  (`started after waiting ...s for memory`). Memory waits stay folded into the
  budget columns and wire reason; the log is how to measure them.

## [0.6.2] - 2026-10-26

### Fixed

- The merge trains' `PATH` keeps the daemon's own directories after the login
  shell's: a login profile that rewrites `PATH` (`/etc/profile` under `bash -l`)
  made 0.6.1 drop every directory only the daemon's starter had (an activated
  virtualenv, a direnv shell), and trains failed with `key_input_failed`
  (exit 127). 0.6.1 was tagged but never released (its release workflow failed
  on this); 0.6.2 ships its changes.

## [0.6.1] - 2026-10-26 (tagged, not released)

### Added

- `[queue] tool_env` (`"login-shell"`, the default, or `"daemon"`) and `[queue]
  tool_path` in the user config: where merge trains get the environment of their
  tools. `sluice status` shows `tool env: login shell` or `daemon (<reason>)`
  (and the optional `queue.tool_env` field of the status JSON), and `sluice
  doctor` warns when a `key_inputs` command of the repository is not in the
  `PATH` trains use. One-way keys: 0.6.0 and older reject them.

### Changed

- Memory-aware admission: a gate that has never run is no longer guessed at the
  weight share of the usable memory alone (2.3 GiB for a weight-1 gate on a 30 GiB
  machine, which kept first-run `fmt`/`lint` gates of about 20 MB waiting behind a
  test suite). The guess is capped by the largest peak learned on this daemon for
  gates of the same or a lighter weight, when there is one. Still a guess: it never
  allows over-admission.

### Fixed

- Merge trains depended on the `PATH` of whoever started the daemon: a daemon
  restarted from a non-interactive shell made every train fail with
  `key_input_failed` (exit 127) while agents' gates passed. Trains now run their
  gates, `prepare` and `key_inputs` in the user's login environment, resolved
  once at daemon start (ADR-0020).

## [0.6.0] - 2026-10-26

### Added

- Golden files recorded from the 0.4.1 and 0.5.0 release binaries, read by the
  compatibility tests next to 0.1.0 through 0.4.0.
- `[queue] prepare` (with `prepare_timeout`, `prepare_weight`): a command run
  once per merge train, through the scheduler, before its gates, so gates that
  need git-ignored dependencies (`node_modules`, `.venv`) can run in the merge
  worktree. A failure is attributed by preparing the base alone: nobody's fault
  (members requeued, `prepare_failed`, `failed` after three attempts) or the
  member's (split, `prepare_failed`); `prepare_dirty` when it leaves files the
  tree does not have. `sluice init` writes it for detected lockfiles (#3).
- `sluice queue enqueue|retry --trust-member-config`: a human-only way to land a
  fix of the queue's own configuration; the train runs the branch's
  `.sluice.toml`, the approval shows its diff and binds its hash. Refused over
  MCP and from an agent's environment, whatever `allow_agent_approval` says.
- Memory safety of gate jobs (ADR-0019): admission by each gate's learned
  memory working set (`[admission] memory_aware`, `memory_reserve`), so two
  suites that do not fit together wait for each other; gate processes are the
  preferred victims of the kernel's OOM killer (`[containment]
  gates_oom_score_adj`, `memory.oom.group` on each job leaf) and of
  `systemd-oomd` (a policy on the gates' scope that acts before the agents');
  a gate killed for memory ends `killed` with `killed_by = "memory"` and a
  message naming its peak; opt-in `[admission] memory_guard` cancels Sluice's
  own largest gate under real exhaustion; optional `[containment]
  gates_memory_max` / `gates_memory_swap_max` cap all gate jobs together on
  `jobs/` (unset by default; a hit kills one job whole, reported as a memory
  kill).
- `client.log` lines carry `key=`; the daemon logs every hook `run` request;
  `sluice run --explain` reports the cache mode.
- `sluice status` shows the memory admission sees (`memory:` line, `mem` per
  job, `(budget: memory)` for a gate that waits for memory) and the gates'
  `systemd-oomd` policy; `status --json` gains the optional fields
  (`budget.memory`, `wait_detail = "memory"`, per-job memory,
  `containment.gates_oom_policy`). `sluice doctor` gains `gates-oom`,
  `gates-memory-policy` and `memory-guard`.
- A branch whose merged `.sluice.toml` asks the `pre-push` hook for gates the
  train does not verify, or does not parse, fails at build time
  (`hook_gate_unverified`, `member_config_invalid`) instead of at the push; the
  other members are requeued.

### Changed

- The daemon's environment no longer inherits per-invocation variables from
  the client that autostarted it (`SLUICE_JOB`, `SLUICE_AGENT`, `SLUICE_CACHE`,
  `SLUICE_DISABLE`, the session leaf, agent-detection variables, the shim
  directory in `PATH`); `key_inputs` run with exactly the gate's environment.
- A merge train verifies `[queue] gates` plus every gate with a `pre-push`
  hook.
- `cargo check` and `cargo doc` are heavy commands (they go through the queue).
- New config keys (`[queue] prepare*`, `[admission] memory_*`,
  `[containment] gates_oom_score_adj`, `gates_memory_max`,
  `gates_memory_swap_max`) are rejected by older binaries.

### Fixed

- The daemon's own push of an approved train no longer re-runs the gates the
  train verified: a Sluice `pre-push` hook launched by that push is answered
  from the train's record (checked by process ancestry and the worktree's
  commit); a hook gate the train did not verify fails the push at once instead
  of running into `push_timeout` and `needs_reconcile` (#4).
- A gate that modifies tracked files fails its train (`gate_modified_tree`)
  instead of looping through dissolve and re-approval; a pass that wrote no
  marker is shown as `not cached`.
- `config_changed` compares against the merge base and is shown in `queue
  status` and the approval plan; a gate failure keeps it.
- An unparsable `.sluice.toml` on the enqueued branch is reported
  (`invalid_config`) instead of silently picking the default target.
- Stored train settings missing a field keep the real defaults (a zero push
  timeout could follow from an unreadable row).
- The output of a push that hit `push_timeout` is kept in the train's note.
- A `key_inputs` command failing in the merge worktree requeues the members
  (`key_input_failed`) instead of failing them as `internal`.
- A train's gates are killed when the train drops them, also under
  `last_subscriber_gone = "finish"`.
- Paths of changed files reported by the queue (`dirty_merge`,
  `gate_modified_tree`, `prepare_dirty`) no longer lose their first character.

## [0.5.0] - 2026-10-24

### Added

- `sluice status` shows `containment: full|partial (...)|off (<reason>)`, one
  line per live agent session (`s-<id>  cpu  mem  procs`, from cgroupfs),
  `outside jobs: <x>% CPU` (the agents' CPU over the last sampling window) and
  the last interventions. `status --json` gains the optional `containment`,
  `sessions[]` and `interventions[]` (schema 1, additive).
- `sluice doctor` gains the `containment` group (cgroup v2, user manager, own
  cgroup, delegated controllers by `cpu.weight` presence, `systemd-oomd` and
  the derived memory thresholds, cgroupfs writable) and the `session` check
  (where this shell and the agent are, and why).
- The agent is told once about an intervention in its own session: the MCP
  `verify` and `status` results and the guard's context note append a short
  line. The daemon keeps `interventions.jsonl` for the guard.

- `[containment]` table of the user config (ADR-0017): weights, `pids_max`,
  optional CPU ceiling, `memory_policy`, `memory_pressure_limit` and
  `memory_pressure_duration` (`"auto"` derives them from the system's
  `systemd-oomd` policy), opt-in `memory_high` and `memory_max`. Configs
  without the table load unchanged.
- `sluice-gate-core::cgroup`: detection of cgroup v2, the user manager, the
  delegated controllers and `systemd-oomd` (`full`, `partial`, `off` with the
  reason `doctor` will print); the Sluice cgroup tree created through the
  systemd user manager (`busctl`, `systemctl`); session leaves with weights and
  `pids.max`; placement of processes (direct write, `AttachProcessesToUnit`
  fallback); pruning of empty cgroups; usage readings. Not wired into the
  daemon yet.
- Hidden `sluice cg-exec <leaf> -- <argv>`: moves itself into a cgroup and
  `exec`s, running the command even when the move fails.

- Session containment (ADR-0017): the plugin's session hook creates the
  session's cgroup (`ui` and `tools` leaves under `sluice-agents.scope`), moves
  the agent process and its current descendants into `ui`, and appends one
  guarded, silent line to `CLAUDE_ENV_FILE` that moves each shell into `tools`
  (nice 19 when the cgroup files are not writable, as in the Bash sandbox).
  Where the CPU cannot be weighted the line only renices. The hook's log line
  gains `contained=full|partial|off`. Shims place themselves in the session's
  `tools` leaf (`SLUICE_SESSION_LEAF`) before running a heavy tool. Fails open;
  nothing is ever killed.

- Daemon containment (ADR-0017): the daemon adopts itself into
  `sluice-daemon.scope` (`daemon/` leaf, with its reaper), creates (or reuses)
  `sluice-agents.scope` around a `sluice daemon holder` process, sets the slice
  weight and the derived `systemd-oomd` policy, and runs every gate and `exec`
  job in its own `jobs/<job-id>` leaf through `sluice cg-exec` placed before
  the `nice`/`ionice` chain. Empty session and job cgroups are pruned every
  minute. Any failure is logged once and the daemon runs as before.
- Interventions: the daemon records kills by the kernel OOM killer
  (`memory.events`) and by `systemd-oomd` (journal) on its cgroups in a new
  `interventions` table (`containment_schema`, additive) and reports them in
  the `status` reply as an optional `interventions` field (protocol 1).

### Fixed

- The agent plugin's guard no longer denies harmless commands that mention
  approval: when it cannot parse a command (heredocs, loops, `$(...)`) or
  meets an opaque wrapper, it now needs the whole word `queue` followed by
  the whole word `approve` (after the shell's joins of quotes, backslashes
  and line continuations), instead of the substrings anywhere in the text.
  "approved" in a commit message or a heredoc is allowed again; every real
  approval, wrapped or not, is still denied, and a computed program name
  (`$x queue approve`) now is too (#2).
- The session hook's stopgap holder is `sluice daemon holder --lifetime 600`
  instead of `sleep 86400`: it ends by itself, when its scope is no longer
  active, or when no scope adopted it; the hook verifies the scope is active
  and holds the holder, else kills it and fails open. A holder once stayed
  alive inside a dead scope.
- `plugin/hooks/test-session-start.sh` ran the real binary against the
  developer's real user manager; it now uses an empty runtime directory.

## [0.4.1] - 2026-10-23

### Added

- Golden files recorded from the 0.4.0 release binary, read by the
  compatibility tests next to 0.1.0, 0.2.0 and 0.3.0.

### Changed

- `sluice init` writes every rewired hook line with a fallback to the original
  command when Sluice is not installed (`if command -v sluice ...; then sluice
  run --hook <hook> <gate>; else <original>; fi`, through `sh -c` for the
  pre-commit framework), so the rewired hooks can be committed for a team. The
  `.githooks/` files it creates start with `command -v sluice ... || exit 0`.
  `init --force` upgrades the bare lines of 0.4.0 to the new form.
- `sluice run` prints `sluice: cache off for this tree: <reason>` once on
  stderr (without `-v`, hooks included) when the tree cannot be hashed, and
  `client.log` lines carry `reason=<code>` after `tree=-`.

### Fixed

- `sluice init` writes `.sluice.toml` and hook files atomically (temporary
  file in the same directory, fsync, mode kept, `rename`; the target of an
  in-repository symlink), so a hook that is running is never read half
  written.
- `npm exec X`, `npm x X`, `npm exec -- X`, `npm exec -y X`, `npx -- X`,
  `pnpm dlx X` and `yarn dlx X` are classified as `X`, so the guard and the
  shims queue `npm exec playwright test` and `pnpm dlx vitest run`.
- `sluice doctor` reads the scripts a husky v9 runner executes
  (`.husky/<hook>`) instead of warning that no hook calls sluice.
- `sluice doctor` has a `cache` check that warns about skip-worktree and
  assume-unchanged index entries, which silently disable the cache.

## [0.4.0] - 2026-10-23

### Added

- Golden files recorded from the 0.3.0 release binary, read by the
  compatibility tests next to 0.1.0 and 0.2.0.
- `weight = "auto"` for gates: the ceiling is half the daemon's budget and
  learned weights lower it, so there is no number to guess. A declared number
  stays a ceiling and an omitted `weight` stays 1. A `.sluice.toml` that uses
  `"auto"` needs 0.4.0 (older binaries accept only integers).
- `sluice exec` jobs learn their weight from the measured demand after three
  runs, per repository and `--label`; `--weight` or `[agent] exec_weight` is
  the ceiling.
- `sluice status` shows configured and effective weights (`weight auto→5`);
  its JSON gains the optional `weight_effective` and `weight_auto` (schema 1).
- The built-in heavy list knows the Angular CLI, Nx, Turborepo, Playwright,
  Cypress, react-scripts, Astro, Nuxt, .NET, Bazel, Ninja and `cmake --build`,
  so the guard and the shims catch them with no user config. Dev servers and
  watch modes (`ng serve`, `nx serve`, `next dev`, bare `vite`, `--watch`)
  are never heavy.
- `npm run <script>` (and `npm test`, `pnpm <script>`, `yarn <script>`) is
  classified by what the script in the nearest `package.json` runs, following
  nested scripts, in both the guard and the shims.
- A shim call that is exactly a gate command inside a repository with a
  `.sluice.toml` now runs `sluice run <gate> -v` (cache and coalescing)
  instead of `sluice exec`.
- The agent plugin sets itself up when a session starts (`SessionStart` hook,
  `sluice agent session-start`, hidden): it installs the shell shims, puts
  them first in the session's `PATH` through `CLAUDE_ENV_FILE` (once, also on
  resume) and gives the agent the rules as context: verify before commits,
  no subshell or script around builds, no parallel dev servers during gates,
  never approve merge trains. In a git repository without `.sluice.toml` the
  agent proposes `sluice init` to the user once. A missing or older `sluice`
  binary is reported to the agent, which asks the user before installing or
  updating. Users no longer edit `PATH` or their `CLAUDE.md`.
- `sluice init` detects the stack and the existing hooks. It takes gates from
  the heavy commands of husky, lefthook, pre-commit framework and plain hooks,
  fills the rest from Angular, Node (npm/pnpm/yarn/bun), Rust, Python and Go
  projects, picks weights (`"auto"` for test runners) and timeouts, and
  rewires the hooks in place to `sluice run --hook <hook> <gate>` (comments,
  mode and CRLF kept; formatters and fixers untouched). With no hook manager
  it creates `.githooks/` and sets `core.hooksPath` locally. `--dry-run`
  prints the gates, the files and the diff and writes nothing; `--force` is
  idempotent.
- `sluice doctor` checks the zero-config setup: `shims` (installed and
  current in the stable directory, first in `PATH` for this shell; a warning
  in an agent shell when not), `plugin` (the installed agent plugin's version
  against the binary; a warning when they differ, never a failure),
  `weights` (a warning when a declared weight is more than 50 % below what the
  gate's runs need, suggesting a higher number or `weight = "auto"`) and
  `init` (a repository without `.sluice.toml`: a hint to run `sluice init
  --dry-run`).

### Changed

- The MCP `status` text shows configured and effective weights like the CLI
  (`weight auto→5`).
- `docs/agents.md`, the README quick start and the configuration, CLI and
  debugging guides describe the zero-config flow: install the binary and the
  agent plugin, then `sluice init`; the built-in heavy list, the never-heavy
  rules and package-script resolution; the session hook and how to inspect
  its decision.
- `sluice init` outside a git repository is now an error, and writes at the
  repository root instead of the current directory.

### Fixed

- Gates and `sluice exec` jobs now start under `nice` and `ionice`, so every
  process of a job inherits `daemon.nice` and the I/O class from its first
  instruction; before, children forked before the asynchronous `renice` of
  the leader stayed at nice 0 (#1). When the kernel refuses the I/O
  priority (seccomp, some containers) the gate still runs, with the default
  I/O class.

## [0.3.0] - 2026-10-23

### Added

- `sluice shim install|uninstall|status`: PATH shims that route the heavy
  commands (make, cargo build, pnpm build, docker build, …) of agents
  without hook support (Codex, Cursor, Aider) through the queue in agent
  sessions or with `SLUICE_SHIMS=1`, and run the real binary unchanged for
  humans.
- `sluice guard`: the agent plugin's PreToolUse hook now rewrites gate
  commands to `sluice run <gate> -v` and heavy commands to
  `sluice exec -- …` by inserting a fixed prefix in front of the untouched
  original, approving or asking according to the agent client's
  permission settings; `sluice queue approve` stays denied to agents.
  User config `[agent] heavy_commands`, `heavy_commands_extra` and
  `exec_weight`; `SLUICE_GUARD=deny` turns rewrites into suggestions and
  `SLUICE_GUARD=0` disables the hook. With binaries older than 0.3.0 the
  hook keeps the previous deny-only behaviour.
- `docs/compatibility.md`: what stays compatible from 0.3.0 and how
  changes are made; golden files recorded from the 0.1.0 and 0.2.0 release
  binaries (configs, markers, status and queue JSON, protocol transcripts,
  databases, CLI surface, MCP tools) with tests that read them.
- Path-scoped gates: optional `paths` / `paths_ignore` (git glob pathspecs,
  root-relative; `.sluice.toml` always counts). The key hashes only the
  selected files, so changes outside the scope keep the cached pass, with
  no extra git call; unscoped keys and markers are unchanged. `cache
  status` and MCP `status` show the scope and `sluice doctor` warns about
  manifests and lockfiles outside it. A config using them needs Sluice
  0.3.0 or newer.
- `sluice exec [--weight N] [--label L] [--coalesce-key K] [--timeout DUR]
  [-q] -- <cmd>` runs any command under the budget without cache, replays
  its output byte for byte, propagates the exit code (128+N on signals),
  forwards signals to the job's process group and falls back in-process
  when the daemon is down or older than 0.3.0.
- Gate field `mode = "queue-only"`: the gate always runs, is never cached,
  coalesces simultaneous identical requests, and an agent hook waits for
  it instead of failing fast.
- `sluice logs --timeline [JOB_ID] [--since DUR]` and, in `status --json` and
  `verify` results, a per-job scheduler timeline (requested, queued,
  admitted, started, finished, wait reasons, measured CPU demand,
  requesters) and `idle_while_waiting_ms`: why a job waited.
- `SLUICE_JOB` on every process Sluice runs, so a nested `exec` or `run`
  runs directly; welcome features `exec` and `queue-only`; `kind` and
  `label` in `status --json`.
- `docs/gates-cookbook.md`: per-stack `.sluice.toml` examples (Node,
  Python, Rust, Go, Docker, monorepos) and guidance on per-worktree caches,
  verdicts versus build artifacts and choosing hooks.
- A compatibility CI matrix: Ubuntu 22.04/24.04, Debian 12, Fedora,
  Alpine (musl), git 2.31.0 built from source and native aarch64, on main,
  tags, weekly and on demand.
- Issue templates that ask for `sluice doctor --bundle`, this changelog,
  release notes taken from it, and a "Status and limitations" README section.
- Package metadata, per-crate READMEs and LICENSE files; `cargo publish
  --workspace --dry-run` passes for all six crates (crates.io readiness,
  nothing published).

### Changed

- Status moves from pre-alpha to alpha: every scenario of the alpha harness
  passes on real Node and Python projects (`kb/sluice/changes/alpha-readiness.md`).
- `sluice queue approve` run from an agent's environment is refused unless
  `[queue] allow_agent_approval = true`.
- Scheduler throughput (ADR-0011): the default budget is the number of
  available CPUs, at least 2 (affinity and cgroup limits honoured; 0.2.0:
  CPUs minus one); gates learn their real CPU demand from passing runs and
  are admitted with it (resolved at every admission pass; running jobs keep
  the weight they started with); PSI-driven over-admission up to
  1.5x the budget when the machine has idle CPUs; MCP `verify` runs its
  gates concurrently and stops at the first failure (`fail_fast`); jobs are
  shared fairly between worktrees, humans first, with optional
  shortest-job-first. Every mechanism has a config switch.
- The daemon database gains additive, transactional migrations with a
  backup copy before migrating; 0.2.0 binaries still open the migrated
  file.
- The README is short and current (no development phases, no pinned
  release numbers); the details moved to `docs/how-it-works.md`,
  `docs/cli.md` (generated from `--help`) and `docs/configuration.md`.
- Markers and miss records tolerate unknown fields; the TTL pruning of
  markers also removes expired miss records and foreign-format files.
- Wire enums parse unknown values as `unknown`; an unknown outcome is an
  error, never a pass.
- A refusal to open a newer database names the release that wrote it (new
  additive `meta` table); `sluice --help` documents every exit code.
- `sluice logs` and log retention handle `.exec` spools; `doctor` and
  `hook print` warn when a queue-only gate is listed in `hooks`.

### Fixed

- A non-UTF-8 environment variable no longer makes `sluice` commands panic
  (such entries are skipped; `sluice exec` then runs in-process with the raw
  environment).
- A queued job dropped by its last subscriber or a stop can no longer
  start unsupervised, and terminated process groups are never signalled
  again (pid reuse).
- Every git process Sluice spawns now runs with
  `-c maintenance.auto=false -c gc.auto=0`: the merge queue's `fetch` and
  `merge` could start git's automatic maintenance (repack, prune) in the
  user's repository. The cache check itself never did.
- `sluice queue status` retries once when the daemon drops the connection
  before replying (for example right after it died), instead of failing.
- A daemon killed while it applied `nice` and `ionice` to a new gate left
  that gate running: the gate was registered with the reaper only after
  the `renice` and `ionice` helpers had finished. It is now registered
  first and the priorities follow.
- The merge queue works with git 2.31 and 2.32 (2.31 is the declared
  minimum): its worktree was created with `git worktree add --lock
  --reason`, which needs git 2.33.
- Merge queue: a wrong SSH key or HTTP password fails `sluice queue
  approve` at once with `push_rejected` instead of leaving the train
  approved forever or parking it for reconcile; the SSH error text reaches
  the user; a password embedded in the remote URL no longer reaches job
  details, train notes or `daemon.log`.
- A daemon killed by SIGKILL or the OOM killer no longer leaves its gates
  running: a detached reaper terminates them within seconds, the next
  daemon start kills any leftover group (also when only a background child
  survives) and records the jobs as `killed`, and waiting clients see "the
  daemon died while running <gate>; it was killed" (exit 143).
- An unwritable logs or state directory no longer stops gates: the output
  is kept in memory for the failure tail and the error is reported once.
- `sluice doctor` reports an unwritable markers directory (fail), damaged
  marker files (warn) and a database the daemon cannot open (fail).
- The daemon and its clients fall back to `/tmp/sluice-<uid>/daemon.sock`
  (created 0700; refused when it is a symlink, owned by another user or
  group/world writable) when the runtime-dir socket path exceeds the Unix
  limit, instead of silently running gates in-process; an over-long
  `SLUICE_SOCKET` is a clear error; `sluice doctor` gains a `socket-path`
  check.

## [0.2.0] - 2026-10-22

### Added

- Local merge queue inside the daemon (`sluice-gate-queue`): trains of up
  to four branches, no-ff merges in a disposable worktree, gates run
  through the scheduler, approval bound to a train hash, a write-ahead
  marker and pin ref before an atomic push, deterministic reconcile after a
  crash, and local-branch targets (failing multi-job trains split into
  single-job trains). CLI `sluice queue ...` and MCP
  `queue_*` tools. Humans approve; agents cannot unless a user-config key
  allows it.
- Debugging tools: `[debug] level` in the user config, `sluice daemon
  restart --debug`, persistent `sluice mcp` and `sluice run` logs
  (`logs/mcp-<pid>.log`, `logs/client.log`), `daemon.log` rotation, and
  `sluice doctor [--json] [--bundle]` with a redacted `.tar.gz` bundle.
  See [docs/debugging.md](docs/debugging.md).
- `sluice update [--check] [--version vX.Y.Z]`: replaces the binary with a
  verified release (SHA-256 checked) and restarts an idle daemon.
- Claude Code plugin and marketplace in this repository: MCP registration,
  the `sluice` skill and a `PreToolUse` guard hook that denies the raw
  commands your gates run.
- `scripts/release.sh`, and a release workflow step that refuses a tag that
  does not match the workspace version.
- Daemon extension point and protocol messages for the merge queue.

### Changed

- The merge queue was hardened after an adversarial review: network calls
  have deadlines and run off the runner's main loop, the `[queue] remote`
  must be a configured remote, reconcile cleans up only after its decision
  commits, approve waits are bounded, credentials are dropped when rows
  finish, and untracked gate output no longer dissolves a train. MCP gained
  `queue_retry` and `queue_cancel`.
- `sluice status` shows the log level and path; the status reply gained an
  optional `daemon.log_level` (status JSON schema stays 1).
- `docs/agents.md` recommends the plugin and keeps the `CLAUDE.md` or
  `AGENTS.md` block as the fallback for other agents.

### Fixed

- A daemon killed with SIGKILL kept its lease for 90 s and blocked every
  restart; the new daemon frees a lease whose owner is gone. Queued jobs
  whose base cannot be prepared now fail after three attempts with the
  reason, and approve says why a train was dissolved.
- `sluice update`: retry the exec of the fresh binary copy on `ETXTBSY`.
- Review findings in the merge queue, the debugging tools and the
  knowledge-base documents (final reconciliation, no open discrepancy).

## [0.1.0] - 2026-10-22

### Added

- Phase 0: workspace of `sluice-gate-*` crates, research of four reference
  projects and the first architecture decision records (MIT license,
  Linux only).
- Phase 1, cache only: `sluice run <gate>` with tree hashing from a private
  index and object directory (the user's index and working tree are never
  touched), cache keys with `key_inputs` and `key_files`, markers and miss
  records with a TTL shared by every worktree, agent detection, a flock
  semaphore, `sluice cache`, `sluice init` and `sluice hook print`
  (husky, lefthook, pre-commit, plain). Cache-hit latency 29 ms on a
  5 000-file repository.
- Phase 2, daemon: per-user daemon with socket protocol v1, a fenced SQLite
  lease, a weighted budget scheduler with backfill and head-of-line
  reservation, coalescing of identical requests, an executor with process
  groups and timeouts, orphan reconciliation, `sluice daemon`,
  `sluice status` and `sluice logs`.
- Phase 3, MCP server: `sluice mcp` with `verify` (blocking, with progress
  notifications so agent idle timeouts do not fire), `status`, `gate_list`
  and `logs`; a hook policy that fails fast for agents (exit 3).
- Phase 4, load awareness: admission by Linux PSI with hysteresis and a
  starvation guard; status JSON schema 1 pinned by a fixture.
- Phase 5, packaging: tag-triggered release workflow (static musl binaries
  for x86_64 and aarch64, `SHA256SUMS`), POSIX installer
  `scripts/install.sh`, a multi-worktree benchmark and the README demo.
- Sluice verifies its own repository through `.sluice.toml` and plain hooks
  under `.githooks/`.

### Fixed

- MCP `verify`: extra arguments join the cache key, per-gate errors keep the
  other results, and structured results validate against their output
  schema.
- PSI thresholds must be percentages (rejected at config load otherwise).
- Findings of the Phase 1, 2, 3 and 4 reconciliation passes.

[Unreleased]: https://github.com/josegarridodigio/sluice/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/josegarridodigio/sluice/compare/v0.6.3...v0.7.0
[0.6.3]: https://github.com/josegarridodigio/sluice/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/josegarridodigio/sluice/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/josegarridodigio/sluice/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/josegarridodigio/sluice/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/josegarridodigio/sluice/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/josegarridodigio/sluice/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/josegarridodigio/sluice/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/josegarridodigio/sluice/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/josegarridodigio/sluice/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/josegarridodigio/sluice/releases/tag/v0.1.0
