# Changelog

All notable changes to Sluice are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

### Changed

- `sluice init` outside a git repository is now an error, and writes at the
  repository root instead of the current directory.

### Fixed

- Gates and `sluice exec` jobs now start under `nice` and `ionice`, so every
  process of a job inherits `daemon.nice` and the I/O class from its first
  instruction; before, children forked before the asynchronous `renice` of
  the leader stayed at nice 0 (#1).

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

[Unreleased]: https://github.com/josegarridodigio/sluice/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/josegarridodigio/sluice/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/josegarridodigio/sluice/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/josegarridodigio/sluice/releases/tag/v0.1.0
