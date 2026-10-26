---
title: Configuration format
tags: [sluice, design, config]
---

# Configuration format (v1)

Status: approved 2026-10-22 (Phase-0 answers Q10, Q11, Q14). `version = 1`
is frozen: only additive, optional fields are allowed (the merge-queue
`[queue]` tables, the user `[mcp]`, `[debug]`, `[scheduler]` and `[agent]`
tables, the `[admission]` overcommit keys, `gates.<name>.cache`,
`gates.<name>.mode` and `gates.<name>.paths` / `paths_ignore` were added this
way). Renames and incompatible changes bump the version with a migration
note.

## Repository config: `.sluice.toml`

Located at the repository root (the main worktree's root; worktrees share it
through the checked-out file, so each worktree may carry its own revision of
it, which is fine: the gate definition is part of the cache key).

```toml
version = 1

[defaults]
# Behaviour when a hook invoked by an agent misses the cache.
agent_hook_policy = "fail-fast"     # fail-fast | wait | run-unqueued
# Behaviour when a hook invoked by a human misses the cache.
human_hook_policy = "wait"          # wait | run-unqueued
# Lines of log shown on failure.
tail_lines = 40

[gates.lint]
cmd = "npm run lint"
weight = 1
timeout = "5m"
hooks = ["pre-commit"]              # which hooks this gate belongs to (used by `verify` defaults and `sluice hook print`)

[gates.test]
cmd = "npm test -- --maxWorkers=2"
weight = 4
timeout = "20m"
hooks = ["pre-commit", "pre-push"]
key_inputs = ["node --version"]     # stdout of each command is folded into the cache key
key_files  = [".env.test"]          # ignored/untracked files whose content must invalidate the cache
env        = { NODE_ENV = "test" }
depends_on = ["lint"]               # optional: reserved, validated but not scheduled in v1
paths        = ["src/**", "package.json", "package-lock.json"]  # optional, >= 0.3.0: key only these
paths_ignore = ["src/**/*.md"]      # optional, >= 0.3.0: minus these

[queue]                             # merge queue defaults, all optional (read from the base commit)
remote = "origin"
# target = "main"                   # default: the remote's HEAD branch
# gates = ["lint", "test"]          # default: gates with a pre-push hook, else all
max_train = 4
audit_ref = true
push_timeout = "10m"
network_timeout = "60s"
# prepare = "pnpm install --frozen-lockfile --prefer-offline"   # >= 0.6.0: once per train, before the gates
# prepare_timeout = "10m"           # >= 0.6.0
# prepare_weight = "auto"           # >= 0.6.0: integer >= 1 or "auto", like a gate weight
```

Field reference:

| Field | Type | Default | Notes |
|---|---|---|---|
| `version` | int | required | Config format version. |
| `defaults.agent_hook_policy` | enum | `fail-fast` | See [[sluice/design/adr/0004-agent-detection.md]]. |
| `defaults.human_hook_policy` | enum | `wait` | |
| `defaults.tail_lines` | int | unset | Overrides the user config's `cache.tail_lines` (40) for this repository. |
| `gates.<name>.cmd` | string | required | Run with `sh -c` in the repo worktree root. |
| `gates.<name>.weight` | int ≥ 1 or `"auto"` | 1 | Budget units consumed while running; a ceiling that learned weights lower. `"auto"` (0.4.0, ADR-0015) is `ceil(budget / 2)` of the admitting daemon. Omitted stays 1. Other strings are rejected: `invalid weight "x": use an integer >= 1 or "auto"`. |
| `gates.<name>.timeout` | duration | `30m` | Execution time only; queue wait does not count. Formats: `90s`, `5m`, `1h`. |
| `gates.<name>.hooks` | list | `[]` | Hook names this gate is meant for. Free-form, but `pre-commit` and `pre-push` are recognised by `sluice hook print`. |
| `gates.<name>.key_inputs` | list of commands | `[]` | Each command's stdout (via `sh -c` in the worktree root, with exactly the gate's environment and nothing inherited from the process that derives the key, stderr discarded) is part of the key. Keep them fast; they run on every invocation that consults the cache, including hits. A failing command makes the run uncached (notice on stderr), never a gate failure. |
| `gates.<name>.key_files` | list of paths | `[]` | Relative to repo root. Content hash is part of the key. For files git ignores. Missing file hashes as "absent". |
| `gates.<name>.env` | table | `{}` | Added to the gate's environment and to the key. |
| `gates.<name>.depends_on` | list | `[]` | Gate names; validated (must exist, no self-reference) but not scheduled in v1. |
| `gates.<name>.cache` | bool | `true` | `false` makes the gate always run (still queued). |
| `gates.<name>.mode` | `"gate"` \| `"queue-only"` | `"gate"` | Needs sluice ≥ 0.3.0 (older binaries refuse the file: ``unknown field `mode` ``). `queue-only`: the gate always runs under the budget, never looks up, writes a marker or a miss record, and simultaneous identical requests (same tree, definition and extra arguments) share one run; the agent `fail-fast` hook policy does not apply (the hook waits). Its `cache` is treated as `false`; an explicit `cache = true` is an error (`gates.<name>: mode = "queue-only" never caches; remove cache = true`), `cache = false` is accepted. `sluice hook print` and `sluice doctor` warn when a queue-only gate has `hooks`. |
| `gates.<name>.paths` | list of patterns | unset | Requires Sluice >= 0.3.0. When set, the cache key hashes only the tree entries matching one of these patterns, plus `.sluice.toml` (always). Must not be empty. See "Path scoping" below. |
| `gates.<name>.paths_ignore` | list of patterns | `[]` | Requires Sluice >= 0.3.0. Entries matching any of these are removed from the scope (never `.sluice.toml`). Alone, it scopes "everything except". |
| `queue.remote` | string | `origin` | Remote of the default merge-queue target (Phase 6, [[sluice/design/merge-queue.md]]). Must not be empty, start with `-` or contain whitespace/control characters (it reaches git as an argument); at enqueue it must also be a configured remote of the repository (`invalid_remote`). |
| `queue.target` | string | the remote's HEAD branch | Default target branch for `sluice queue enqueue`. |
| `queue.gates` | list | gates with a `pre-push` hook, else every gate | Gates a train must pass; each must exist. The gates whose `hooks` contain `pre-push` are always added (the repository's hook runs them during the train's push and is answered from the train; ADR-0018 section 3). |
| `queue.max_train` | int ≥ 1 | 4 | Jobs combined into one train; 1 disables batching. |
| `queue.audit_ref` | bool | `true` | Push `refs/sluice/landed/<sha>` atomically with the target. |
| `queue.push_timeout` | duration | `10m` | Limit for the push, including the repository's `pre-push` hook. |
| `queue.prepare` | string | unset | Needs sluice ≥ 0.6.0 (older binaries refuse the file: ``unknown field `prepare` ``). Command run with `sh -c` once per train in the merge worktree, before the keys are derived and the gates run ([[sluice/design/merge-queue.md]] section 5 step 4b; [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]] section 5). Must not be empty after trimming. |
| `queue.prepare_timeout` | duration > 0 | `10m` | Needs ≥ 0.6.0. Limit of the prepare job and of the base probe that attributes its failure. |
| `queue.prepare_weight` | int ≥ 1 or `"auto"` | `"auto"` | Needs ≥ 0.6.0. Budget units of the prepare job, same syntax and learning as a gate's `weight`. |
| `queue.network_timeout` | duration > 0 | `60s` | Limit of every other network call of the queue (`fetch` of the base, `ls-remote` checks at enqueue, in the ready watch, before the push, in reconcile); on expiry git's process group is killed and the call fails. For the base fetch it is read from Sluice's last fetched base (else the remote-tracking branch) of the target, never from a member. |

Rules:

- Gate names: `[a-z0-9][a-z0-9_-]*`, max 64 chars.
- Unknown fields are an error (strict parsing, at every level), so typos
  cannot silently change behaviour.
- `version` must be `1` (`unsupported version N: `version` must be 1
  (supported versions: 1)` otherwise; the same check applies to the user
  config); `cmd` must be
  non-empty after trimming; `weight >= 1` or `"auto"`; `key_files` must be relative
  paths without `..`; `depends_on` entries must name existing gates other
  than the gate itself; `human_hook_policy` may not be `fail-fast`.
  `[defaults]` and `[gates]` are optional; a file with only `version = 1`
  is valid (and every `sluice run` then fails with `unknown gate`).
- `[queue]` (merge queue) is valid only in `.sluice.toml`; the daemon reads
  it with `git show <base commit>:.sluice.toml`, never from a queued branch
  or the working tree. A missing or invalid file at the base yields the
  built-in `[queue]` defaults. Validation errors (at parse): `queue.max_train must be
  >= 1`, `queue.remote <must not be empty | must not start with '-' | must
  not contain whitespace or control characters>`, `queue.network_timeout
  must be > 0`, `queue.prepare must not be empty`, `queue.prepare_timeout must be
  > 0`, `queue.prepare_weight must be >= 1`, `queue.gates names an unknown gate "<name>"`. The per-user
  queue keys below (`enabled`, `allow_agent_approval`, `tick`, `retention`)
  are rejected here as unknown fields, and `queue.remote` and the other
  repo keys are rejected in the user file.
- `cmd` is normalised for the key by trimming and collapsing whitespace
  runs; nothing else.
- `paths` / `paths_ignore` (errors read `invalid configuration in <path>:
  gates.<name>.paths[<i>]: <reason>`): `paths = []` gives `gates.<name>.paths:
  must not be empty (remove the field to use the whole tree)`; per pattern:
  empty, leading `/`, leading `./`, a `..`, `.` or empty (`//`) component
  (one trailing `/` is fine), leading `:` (`magic pathspecs are not
  supported; use paths_ignore`), leading `!` (`negation is not supported;
  use paths_ignore`), a backslash (no escapes), NUL or control characters.
  More than 64 patterns in a list read `gates.<name>.<field>: at most 64
  patterns are allowed` (no index). With `cache = false` they are accepted and
  have no effect (`sluice doctor` notes it).
- The file is read from the current worktree's root (`git rev-parse
  --show-toplevel`); there is no parent-directory search. `sluice init`
  and `sluice hook print` work on `./.sluice.toml` in the current
  directory and do not need a repository.
- Error messages: `no .sluice.toml found in <root> (run 'sluice init' there)`,
  `invalid configuration in <path>: <reason>`, `unknown gate '<name>'`.

## User config: `~/.config/sluice/config.toml`

```toml
version = 1

[daemon]
budget = 8                 # default: max(2, available CPUs: affinity mask and cgroup limit); 0.2.0: max(2, cpus - 1)
nice = 10                  # 0..19; applied to gate processes
ionice_class = "best-effort"   # none | best-effort | idle
ionice_level = 7
log_retention_days = 7
idle_exit = "0s"           # 0 = never exit when idle
last_subscriber_gone = "cancel"   # cancel | finish

[queue]
enabled = true             # user-level kill switch for the merge queue runner
allow_agent_approval = false   # let the MCP queue_approve tool approve trains
tick = "5s"                # runner housekeeping period
retention = "30d"          # finished queue rows (landed/cancelled jobs, landed/dissolved trains, markers, events) older than this are pruned at daemon start and hourly

[admission]
psi_enabled = true
cpu_some_avg10_max = 60.0  # percent; do not admit new jobs above this
memory_some_avg10_max = 20.0
psi_poll_interval = "1s"
psi_max_wait = "5m"        # while PSI blocks, admit the queue head once it has waited this long (must be > 0)
overcommit = 1.5           # 0.3.0, ADR-0011 M3: over-admission cap = floor(overcommit * budget); 1.0 disables; 1.0..=4.0
cpu_some_avg10_low = 25.0  # over-admit only below this CPU some avg10 (percent)
memory_some_avg10_low = 5.0  # ... and below this memory some avg10 (percent)
overcommit_interval = "5s" # at most one over-admission per interval, none sooner after a PSI resume (must be > 0)
memory_aware = true        # 0.6.0, ADR-0019: admit by learned memory, not only by CPU; false = CPU-only admission
memory_reserve = "auto"    # 0.6.0: memory left for the desktop and the agents: "auto" (10 % of MemTotal) or a size such as "3G"
memory_guard = false       # 0.6.0: opt-in, cancel Sluice's own largest gate job under real memory exhaustion

[mcp]
progress_interval = "15s"  # progress notifications while verify runs (floored at 1s, re-read per call)
verify_parallel = true     # 0.3.0, ADR-0011 M4: verify runs its gates concurrently (false: sequential, 0.2.0 behaviour)

[debug]
level = "info"             # log filter (RUST_LOG syntax) of the daemon, `sluice mcp` and `sluice run`; SLUICE_LOG wins
daemon_log_max_mib = 20    # rotate daemon.log above this size, at daemon start and by the daily pruning task (0 = never); keeps daemon.log.1 .. .3

[cache]
ttl_days = 30
tail_lines = 40

[scheduler]
head_reserve_after = "30s" # backfill behind the queue head only this long (ADR-0007); also the ageing bound (ADR-0011)
learned_weights = true     # 0.3.0, ADR-0011 M2: gates hold P75 of their measured demand after 3 runs, in [ceil(w/4), w]
fairness = "lane"          # 0.3.0, ADR-0011 M5: lane (worktrees take turns) | off
human_first = true         # 0.3.0, ADR-0011 M6: human-waited jobs before agent jobs
order = "fifo"             # 0.3.0, ADR-0011 M7: fifo | sjf (shortest expected job first with ageing)

[containment]                      # 0.5.0, all optional; keys, defaults and validation: [[sluice/design/containment.md]] section 7
enabled = true                     # false: no cgroups (0.4.1 behaviour); gates_oom_score_adj = "auto", gates_memory_max = "" and gates_memory_swap_max = "" (0.6.0)

[agent_detection]
env_any_set = ["CLAUDECODE"]      # any of these set and non-empty => agent (SLUICE_AGENT is the override, see below)
env_any_equals = {}               # e.g. { CLAUDE_CODE_ENTRYPOINT = "cli" }

[agent]                            # optional; needs sluice >= 0.3.0 (older binaries reject the table)
# heavy_commands = ["make", "cargo build"]  # replaces the built-in heavy-command list when present
heavy_commands_extra = []         # appended to the (default or replaced) list, e.g. ["bazel build"]
exec_weight = 0                   # default `sluice exec` weight (ceiling, learned down since 0.4.0); 0 = ceil(budget / 2)

[repos."/home/alice/src/foo"]
budget_share_max = 4       # optional per-repo cap
```

Environment variables (highest precedence):

| Variable | Effect |
|---|---|
| `SLUICE_DISABLE` (truthy) | `sluice run`: run the gate directly (needs the repo and `.sluice.toml`), no cache, no queue, no user config. Through `sluice mcp`: cache off, the gate still queues. |
| `SLUICE_CACHE=0` / `1` | Force cache off / on (on overrides the `CI` rule). |
| `CI` | Cache off unless `SLUICE_CACHE` is truthy. A falsy `CI` (`0`, `false`, `no`, `off`) does not count as set. |
| `SLUICE_AGENT=1` / `0` | Force agent / human detection (truthy / falsy words as below; any other value is ignored). |
| `SLUICE_SOCKET` | Override the daemon socket path. |
| `SLUICE_JOB` | Set by Sluice on every process it runs (`<job id>`, or `inproc-<pid>` for in-process runs). When set, `sluice exec` runs the command directly and a `sluice run` miss runs in-process, unqueued (the parent job holds the budget). Not meant to be set by hand. |
| `SLUICE_LOG` | Log filter of the daemon (`sluice daemon`), MCP (`sluice mcp`) and CLI (`sluice run`) processes, e.g. `debug`. Precedence: `SLUICE_LOG` (when valid), then `[debug] level`, then `info`. Written to stderr (the daemon's goes to `daemon.log`, the MCP server's also to `logs/mcp-<pid>.log`). |
| `SLUICE_SHIMS` | Shell shims (`sluice shim`): truthy forces the shims to queue their heavy commands even without agent detection (`sluice shim install` prints it exported with the shim directory on `PATH`); falsy (`0`) makes them exec the real binary unchanged. Inside a Sluice job (`SLUICE_JOB` set) they always exec directly. |
| `SLUICE_SHIM_DIR` | Directory of the shims, set by the shims themselves; `sluice exec` and the shims skip it (and any file carrying the shim marker) when resolving the real binary. Default `$XDG_DATA_HOME/sluice/shims`, else `$HOME/.local/share/sluice/shims`. Not meant to be set by hand. |
| `SLUICE_GUARD` | Plugin guard hook: `0` (or another falsy word) disables it; `deny` turns rewrites into denials suggesting the rewritten command; anything else rewrites. |
| `SLUICE_GUARD_NOJQ` | Test-only: `plugin/hooks/guard.sh` uses its `sed` fallback instead of `jq`. Not a user setting. |
| `SLUICE_TEST_SOCKET_FALLBACK_DIR` | Test-only: base directory of the socket fallback instead of `/tmp`. Not a user setting. |
| `SLUICE_TEST_GUARD_WATCHDOG_MS` | Test-only: the guard's watchdog in milliseconds instead of 500. Not a user setting. |
| `SLUICE_TEST_CLIENT_LOG_CAP` | Test-only: byte cap that triggers the `client.log` rotation instead of 5 MiB. Not a user setting. |
| `SLUICE_CONFIG` | Override the user config path (ignored when empty). |
| `SLUICE_VERSION`, `SLUICE_INSTALL_DIR` | `scripts/install.sh` only: release tag to install (default latest) and target directory (default `$HOME/.local/bin`). |
| `SLUICE_UPDATE_BASE_URL` | `sluice update`: directory or `file://` URL with the archives, `SHA256SUMS` and `LATEST`, instead of GitHub releases. |

Truthy words: `1`, `true`, `yes`, `on`; falsy: `0`, `false`, `no`, `off`
(case-insensitive, trimmed). An unrecognised `SLUICE_CACHE` value falls
through to the `CI` rule.

User config notes: every section and field is optional (a missing file
means defaults); unknown fields are errors; validation: `daemon.budget >= 1`,
`daemon.nice <= 19`, `daemon.ionice_level <= 7`, `admission.psi_poll_interval > 0`,
`admission.psi_max_wait > 0`, `admission.cpu_some_avg10_max`,
`memory_some_avg10_max`, `cpu_some_avg10_low` and `memory_some_avg10_low` in
`[0, 100]`, `admission.overcommit` in `[1.0, 4.0]`,
`admission.overcommit_interval > 0`, `admission.memory_reserve` `"auto"` or a size
(`"3G"`, `"512M"`; `admission.memory_reserve "x" must be "auto" or a size such as "3G"`),
`scheduler.fairness` one of `lane`/`off`,
`scheduler.order` one of `fifo`/`sjf`, `budget_share_max >= 1`, `debug.level` a valid
`EnvFilter` directive (`info`, `debug`, `sluice_gate_daemon=debug,info`; the error names the
file and the value). `sluice daemon restart [--debug]` applies a changed `[debug]` section;
`--debug` forces `debug` for that daemon instance only. `daemon.idle_exit` (always "never" in v1) is parsed but has
no effect and is not range-checked. `[agent]`: `heavy_commands` and
`heavy_commands_extra` entries must not be empty or contain control
characters, have at most 8 words each and at most 256 entries together;
`exec_weight` is a non-negative integer (the daemon clamps it to the budget).
Errors read `agent.<field>[<i>]: <reason>` and `agent: at most 256 heavy
commands are allowed`.
Entries are command words compared after the guard's equivalences
(`npm run build`, `pnpm build`, `yarn build` all mean `run build`); the
default list is `docker build`, `docker buildx build`, `docker compose build`,
`run build`, `cargo build`, `cargo test`, `cargo clippy`, `cargo check`, `cargo doc`, `cargo nextest`,
`go build`, `go test`, `make`, `gradle`, `mvn`, `tsc`, `vite build`,
`webpack`, `next build`, `pytest`, `jest`, `vitest`, `ng build|test|lint|e2e`,
`nx run|run-many|affected|build|test|lint|e2e`, `turbo run|build|test|lint`,
`playwright test`, `cypress run`, `react-scripts build|test`, `astro build`,
`nuxt build`, `dotnet build|test`, `bazel build|test`, `ninja`, `cmake --build`
(`DEFAULT_HEAVY_COMMANDS` in `heavy.rs` is the authority). The plugin guard
(`sluice guard`) and `sluice exec` re-read `[agent]` per call; an invalid user
config makes the guard skip every rewrite. The `[queue]` keys have no validation
beyond their types; the runner floors `tick` at 50 ms, and `retention = "0s"`
prunes every finished row at the next pass. The daemon reads `[daemon]`, `[scheduler]`, `[admission]`,
`[repos]`, `[queue]` (`enabled`, `allow_agent_approval`, `tick`, `retention`),
`[debug]` and `cache.*` once at start (`sluice daemon restart` to apply
changes); the client and the MCP server re-read the file per call (so
`sluice mcp` checks `queue.allow_agent_approval` live, on top of the
daemon's own start-time check, and the CLI and MCP take `queue.tick` live to
bound their wait for a landing). Everything else is live
(`log_retention_days` prunes job logs daily from the daemon;
`mcp.progress_interval` paces `verify`'s progress notifications and
`mcp.verify_parallel` is read per `verify` call). The ADR-0011 switches
(`scheduler.learned_weights`, `fairness`, `human_first`, `order`,
`admission.overcommit` and its three companions, `mcp.verify_parallel`)
each turn one mechanism on or off so the tuning loop (L3) can measure them
one at a time on the same binary; the v0.2.0 policy is `budget = cpus - 1`,
`learned_weights = false`, `overcommit = 1.0`, `verify_parallel = false`,
`fairness = "off"`, `human_first = false`, `order = "fifo"`. Releases up to
0.2.0 reject these keys (unknown fields).

## Path scoping (`paths`, `paths_ignore`)

Design and rationale: [[sluice/design/path-scoped-keys.md]], ADR-0012.

- Patterns are git `:(glob)` pathspecs (what `git --glob-pathspecs
  ls-files -- <pattern>` selects), relative to the repository root
  whatever the current directory, byte-wise and case-sensitive.
- A pattern without `*`, `?` or `[` is literal: it matches the path itself
  and, as a directory, everything under it (`docs` and `docs/` match
  `docs/a/b.md`; `doc` does not match `docs/x`).
- A pattern with wildcards must match the full path (`WM_PATHNAME`): `*`
  and `?` never cross `/`, `[...]` is a class, `**/` matches zero or more
  leading directories, a trailing `/**` everything inside, `a/**/b` zero or
  more middle directories. A wildcard pattern does not recurse into a
  directory it matches: `src/1*` selects no file under `src/12/`; write
  `src/1*/**`.
- Selected = (`paths` unset or any `paths` pattern matches) and no
  `paths_ignore` pattern matches; `.sluice.toml` is then always added.
  Each pattern is evaluated on its own (unlike `git ls-files -- <paths>
  ':(exclude)<ignore>'`, which also strips the common literal prefix of
  the positive pathspecs from the exclusions).
- Every entry of the working-tree snapshot counts: tracked and untracked
  non-ignored files, with mode and blob id (so `chmod +x` and a symlink
  retarget invalidate; a symlink's target content is not followed).
  Ignored files never do: use `key_files` for those.
- Binaries older than 0.3.0 reject a `.sluice.toml` that uses these fields
  (`unknown field paths`): every clone and worktree that runs Sluice on
  that config needs 0.3.0 or later.

## Decisions on this format

- Hooks are declared per gate with `hooks = [...]` (Q14). `verify` without
  an explicit gate list runs the gates whose `hooks` contain `pre-commit`.
- `daemon.idle_exit` is accepted but only `"0s"` (never) is implemented in v1 (Q10).
- Gate processes receive the client's full environment minus every
  variable from `git rev-parse --local-env-vars` and every `CLAUDE*`
  variable, plus the gate's `env` (Q11).
- `depends_on` is reserved and unimplemented in v1.
- Durations are strings parsed by `humantime` (`90s`, `5m`, `1h`).

Related: [[sluice/design/cache-key.md]], [[sluice/design/architecture.md]].
