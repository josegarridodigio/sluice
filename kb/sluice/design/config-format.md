---
title: Configuration format
tags: [sluice, design, config]
---

# Configuration format (v1)

Status: approved 2026-10-22 (Phase-0 answers Q10, Q11, Q14). `version = 1`
is frozen once Phase 1 ships; until then field additions are allowed,
renames are not. Any incompatible change later bumps the version with a
migration note.

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
depends_on = ["lint"]               # optional: run only after these gates passed (same tree)
```

Field reference:

| Field | Type | Default | Notes |
|---|---|---|---|
| `version` | int | required | Config format version. |
| `defaults.agent_hook_policy` | enum | `fail-fast` | See [[sluice/design/adr/0004-agent-detection.md]]. |
| `defaults.human_hook_policy` | enum | `wait` | |
| `defaults.tail_lines` | int | 40 | |
| `gates.<name>.cmd` | string | required | Run with `sh -c` in the repo worktree root. |
| `gates.<name>.weight` | int ≥ 1 | 1 | Budget units consumed while running. |
| `gates.<name>.timeout` | duration | `30m` | Execution time only; queue wait does not count. Formats: `90s`, `5m`, `1h`. |
| `gates.<name>.hooks` | list | `[]` | Hook names this gate is meant for. Free-form, but `pre-commit` and `pre-push` are recognised by `sluice hook print`. |
| `gates.<name>.key_inputs` | list of commands | `[]` | Each command's stdout is part of the key. Keep them fast; they run on every invocation, including hits. |
| `gates.<name>.key_files` | list of paths | `[]` | Relative to repo root. Content hash is part of the key. For files git ignores. Missing file hashes as "absent". |
| `gates.<name>.env` | table | `{}` | Added to the gate's environment and to the key. |
| `gates.<name>.depends_on` | list | `[]` | Gate names. Not in Phase 1; reserved. |
| `gates.<name>.cache` | bool | `true` | `false` makes the gate always run (still queued). |

Rules:

- Gate names: `[a-z0-9][a-z0-9_-]*`, max 64 chars.
- Unknown fields are an error (strict parsing), so typos cannot silently
  change behaviour.
- `cmd` is normalised for the key by trimming and collapsing whitespace
  runs; nothing else.

## User config: `~/.config/sluice/config.toml`

```toml
version = 1

[daemon]
budget = 8                 # default: max(2, cpu_count - 1)
nice = 10                  # 0..19; applied to gate processes
ionice_class = "best-effort"   # none | best-effort | idle
ionice_level = 7
log_retention_days = 7
idle_exit = "0s"           # 0 = never exit when idle
last_subscriber_gone = "cancel"   # cancel | finish

[admission]
psi_enabled = true
cpu_some_avg10_max = 60.0  # percent; do not admit new jobs above this
memory_some_avg10_max = 20.0
psi_poll_interval = "1s"

[cache]
ttl_days = 30
tail_lines = 40

[agent_detection]
env_any_set = ["CLAUDECODE", "SLUICE_AGENT"]   # any of these set and non-empty => agent
env_any_equals = { CLAUDE_CODE_ENTRYPOINT = "cli" }

[repos."/home/jose/Escritorio/Proyectos/foo"]
budget_share_max = 4       # optional per-repo cap
```

Environment variables (highest precedence):

| Variable | Effect |
|---|---|
| `SLUICE_DISABLE=1` | Bypass Sluice entirely: run the gate directly, no cache, no queue. |
| `SLUICE_CACHE=0` / `1` | Force cache off / on (on overrides the `CI` rule). |
| `CI` (any value) | Cache off unless `SLUICE_CACHE=1`. |
| `SLUICE_AGENT=1` / `0` | Force agent / human detection. |
| `SLUICE_SOCKET` | Override the daemon socket path. |
| `SLUICE_CONFIG` | Override the user config path. |
| `SLUICE_LOG` | Tracing filter for the client (`info`, `debug`, …). |

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
