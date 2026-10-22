# Configuration

Sluice reads two TOML files:

- **`.sluice.toml`** at the root of the repository (committed): the gates and
  the merge-queue defaults.
- **`~/.config/sluice/config.toml`** (per user, optional): the daemon budget,
  PSI admission, logging and agent detection. A missing file means defaults.
  `SLUICE_CONFIG` points elsewhere.

Both files start with `version = 1`. This is the **config format version**,
not the Sluice release: it is frozen, and only additive, optional fields are
added to it. Unknown fields are errors at every level, so a typo cannot
silently change behaviour.

For ready-to-adapt files per stack see [gates-cookbook.md](gates-cookbook.md).

## Repository config: `.sluice.toml`

```toml
version = 1

[defaults]
agent_hook_policy = "fail-fast"   # fail-fast | wait | run-unqueued

[gates.lint]
cmd = "npm run lint"
weight = 1
hooks = ["pre-commit"]

[gates.test]
cmd = "npm test -- --maxWorkers=2"
weight = 4
timeout = "20m"
hooks = ["pre-commit", "pre-push"]
key_inputs = ["node --version"]
key_files  = [".env.test"]
env        = { NODE_ENV = "test" }
paths        = ["src/**", "package.json", "package-lock.json"]
paths_ignore = ["src/**/*.md"]
```

The file is read from the current worktree's root; there is no parent-directory
search. Each worktree may carry its own revision of it: the gate definition is
part of the cache key. A file with only `version = 1` is valid (every
`sluice run` then fails with `unknown gate`).

### Top level and `[defaults]`

| Field | Type | Default | Notes |
|---|---|---|---|
| `version` | int | required | Config format version; must be `1`. |
| `defaults.agent_hook_policy` | `fail-fast` \| `wait` \| `run-unqueued` | `fail-fast` | What a hook invoked by an agent does on a cache miss. |
| `defaults.human_hook_policy` | `wait` \| `run-unqueued` | `wait` | Same, for a human. `fail-fast` is not allowed here. |
| `defaults.tail_lines` | int | unset | Log lines shown on failure for this repository; overrides the user config's `cache.tail_lines` (40). |

### `[gates.<name>]`

Gate names match `[a-z0-9][a-z0-9_-]*` and are at most 64 characters.

| Field | Type | Default | Notes |
|---|---|---|---|
| `cmd` | string | required | Run with `sh -c` in the worktree root. Must not be empty. |
| `weight` | int >= 1 | `1` | Budget units consumed while running. Roughly the number of CPU threads the gate uses. |
| `timeout` | duration | `30m` | Execution time only; queue wait does not count. Formats such as `90s`, `5m`, `1h`. |
| `hooks` | list | `[]` | Hook names this gate is meant for. Free-form; `pre-commit` and `pre-push` are recognised by `sluice hook print`. `verify` without an explicit gate list runs the gates whose `hooks` contain `pre-commit`. |
| `key_inputs` | list of commands | `[]` | The stdout of each command (`sh -c` in the worktree root) is folded into the cache key. They run on every cache consultation, hits included, so keep them fast. A failing command makes the run uncached, never a gate failure. |
| `key_files` | list of paths | `[]` | Relative to the repository root, no `..`. The content hash is part of the key. For files git ignores; a missing file counts as "absent". |
| `env` | table | `{}` | Added to the gate's environment and to the key. |
| `cache` | bool | `true` | `false` makes the gate always run (still queued). |
| `mode` | `"gate"` \| `"queue-only"` | `"gate"` | `queue-only`: always runs under the budget, never cached, identical simultaneous requests share one run, and the `fail-fast` policy does not apply (the hook waits). `cache = true` is an error with it. `sluice hook print` and `sluice doctor` warn when a queue-only gate has `hooks`. Needs Sluice 0.3.0 or later. |
| `paths` | list of patterns | unset | When set, the cache key covers only the tree entries matching one of these patterns, plus `.sluice.toml`. Must not be empty. Needs 0.3.0 or later. |
| `paths_ignore` | list of patterns | `[]` | Entries matching any of these are removed from the scope. Alone, it means "everything except". Needs 0.3.0 or later. |
| `depends_on` | list of gate names | `[]` | Validated (the gates must exist, no self-reference) but reserved: not scheduled. |

Binaries older than 0.3.0 reject a `.sluice.toml` that uses `mode`, `paths` or
`paths_ignore` (`unknown field`), so every clone and worktree running Sluice on
that file needs 0.3.0 or later.

### Path scoping

`paths` and `paths_ignore` let a change outside a gate's scope keep its cached
pass. It is opt-in: a gate that reads files outside its scope can pass stale.

- Patterns are git glob pathspecs, relative to the repository root whatever the
  current directory, byte-wise and case-sensitive. Leading `/`, `./`, `..`,
  `:` (magic pathspecs), `!` (negation), backslashes and control characters are
  rejected; at most 64 patterns per list.
- A pattern without `*`, `?` or `[` is literal: it matches the path itself and,
  as a directory, everything under it (`docs` matches `docs/a/b.md`; `doc`
  does not match `docs/x`).
- A pattern with wildcards must match the full path: `*` and `?` never cross
  `/`, `**/` matches zero or more leading directories, a trailing `/**`
  matches everything inside. A wildcard pattern does not recurse into a
  directory it matches: `src/1*` selects nothing under `src/12/`; write
  `src/1*/**`.
- Selected means (`paths` unset or any `paths` pattern matches) and no
  `paths_ignore` pattern matches; `.sluice.toml` is always added.
- Every entry of the working-tree snapshot counts, with mode and blob id (so
  `chmod +x` and a symlink retarget invalidate). Ignored files never do: use
  `key_files` for those.
- With `cache = false` they are accepted and have no effect.

See "Choosing paths" in [gates-cookbook.md](gates-cookbook.md).

### `[queue]` (merge queue defaults)

All optional. The daemon reads them from the base commit
(`git show <base>:.sluice.toml`), never from a queued branch or the working
tree; a missing or invalid file at the base yields the defaults. See
[merge-queue.md](merge-queue.md).

| Field | Type | Default | Notes |
|---|---|---|---|
| `remote` | string | `origin` | Remote of the default target. Must not be empty, start with `-` or contain whitespace; at enqueue it must be a configured remote. |
| `target` | string | the remote's HEAD branch | Default target branch for `sluice queue enqueue`. |
| `gates` | list | gates with a `pre-push` hook, else every gate | Gates a train must pass; each must exist. |
| `max_train` | int >= 1 | `4` | Jobs combined into one train; `1` disables batching. |
| `audit_ref` | bool | `true` | Push `refs/sluice/landed/<sha>` atomically with the target. |
| `push_timeout` | duration | `10m` | Limit for the push, including the repository's `pre-push` hook. |
| `network_timeout` | duration > 0 | `60s` | Limit of every other network call of the queue (fetch of the base, `ls-remote` checks). |

The per-user queue keys below are rejected here, and the keys above are
rejected in the user file.

## User config: `~/.config/sluice/config.toml`

Every section and field is optional.

```toml
version = 1

[daemon]
budget = 8                     # default: max(2, cpu_count - 1)
nice = 10
ionice_class = "best-effort"   # none | best-effort | idle
ionice_level = 7
log_retention_days = 7
idle_exit = "0s"
last_subscriber_gone = "cancel"   # cancel | finish

[admission]
psi_enabled = true
cpu_some_avg10_max = 60.0
memory_some_avg10_max = 20.0
psi_poll_interval = "1s"
psi_max_wait = "5m"

[scheduler]
head_reserve_after = "30s"

[cache]
ttl_days = 30
tail_lines = 40

[mcp]
progress_interval = "15s"

[queue]
enabled = true
allow_agent_approval = false
tick = "5s"
retention = "30d"

[debug]
level = "info"
daemon_log_max_mib = 20

[agent_detection]
env_any_set = ["CLAUDECODE"]
env_any_equals = {}

[repos."/path/to/repo"]
budget_share_max = 4
```

| Field | Default | Notes |
|---|---|---|
| `daemon.budget` | `max(2, cpus - 1)` | Total weight of gates running at once. Must be >= 1. |
| `daemon.nice` | `10` | 0 to 19; applied to gate processes. |
| `daemon.ionice_class` | `best-effort` | `none`, `best-effort` or `idle`. |
| `daemon.ionice_level` | `7` | 0 to 7. |
| `daemon.log_retention_days` | `7` | Job logs older than this are pruned daily by the daemon. |
| `daemon.idle_exit` | `0s` | Parsed but has no effect: the daemon never exits when idle. |
| `daemon.last_subscriber_gone` | `cancel` | `cancel` or `finish`: what happens to a job when everyone waiting for it is gone. |
| `admission.psi_enabled` | `true` | Hold new work while the machine is under pressure (Linux PSI). |
| `admission.cpu_some_avg10_max` | `60.0` | Percent, 0 to 100: do not admit new jobs above this. |
| `admission.memory_some_avg10_max` | `20.0` | Percent, 0 to 100. |
| `admission.psi_poll_interval` | `1s` | Must be > 0. |
| `admission.psi_max_wait` | `5m` | While PSI blocks, admit the queue head once it has waited this long. Must be > 0. |
| `scheduler.head_reserve_after` | `30s` | Backfill behind the queue head only this long. |
| `cache.ttl_days` | `30` | Age after which a marker is ignored. |
| `cache.tail_lines` | `40` | Log lines shown on failure (`defaults.tail_lines` overrides it per repository). |
| `mcp.progress_interval` | `15s` | Progress notifications while `verify` runs (floored at 1s). |
| `queue.enabled` | `true` | Kill switch for the merge queue runner. |
| `queue.allow_agent_approval` | `false` | Let the MCP `queue_approve` tool approve trains. |
| `queue.tick` | `5s` | Runner housekeeping period (floored at 50 ms). |
| `queue.retention` | `30d` | Finished queue rows older than this are pruned at daemon start and hourly. |
| `debug.level` | `info` | Log filter (`RUST_LOG` syntax, for example `sluice_gate_daemon=debug,info`) of the daemon, `sluice mcp` and `sluice run`. `SLUICE_LOG` wins. |
| `debug.daemon_log_max_mib` | `20` | Rotate `daemon.log` above this size (0 = never); keeps `daemon.log.1` to `.3`. |
| `agent_detection.env_any_set` | `["CLAUDECODE"]` | Any of these variables set and non-empty means the caller is an agent (`SLUICE_AGENT` overrides). |
| `agent_detection.env_any_equals` | `{}` | Variable/value pairs that mean agent, for example `{ CLAUDE_CODE_ENTRYPOINT = "cli" }`. |
| `repos."<path>".budget_share_max` | unset | Optional cap on the budget one repository may use at once. Must be >= 1. |

The daemon reads `[daemon]`, `[scheduler]`, `[admission]`, `[repos]`, `[queue]`,
`[debug]` and `cache.*` once at start: run `sluice daemon restart` to apply
changes (`sluice daemon restart --debug` forces `debug` logging for that
instance only). The client and the MCP server re-read the file on every call.

Environment variables that override or complement these files are listed in
[cli.md](cli.md#environment-variables). Durations are strings such as `90s`,
`5m`, `1h`.
