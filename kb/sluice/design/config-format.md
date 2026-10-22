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
| `defaults.tail_lines` | int | unset | Overrides the user config's `cache.tail_lines` (40) for this repository. |
| `gates.<name>.cmd` | string | required | Run with `sh -c` in the repo worktree root. |
| `gates.<name>.weight` | int ≥ 1 | 1 | Budget units consumed while running. |
| `gates.<name>.timeout` | duration | `30m` | Execution time only; queue wait does not count. Formats: `90s`, `5m`, `1h`. |
| `gates.<name>.hooks` | list | `[]` | Hook names this gate is meant for. Free-form, but `pre-commit` and `pre-push` are recognised by `sluice hook print`. |
| `gates.<name>.key_inputs` | list of commands | `[]` | Each command's stdout (via `sh -c` in the worktree root, stderr discarded) is part of the key. Keep them fast; they run on every invocation that consults the cache, including hits. A failing command makes the run uncached (notice on stderr), never a gate failure. |
| `gates.<name>.key_files` | list of paths | `[]` | Relative to repo root. Content hash is part of the key. For files git ignores. Missing file hashes as "absent". |
| `gates.<name>.env` | table | `{}` | Added to the gate's environment and to the key. |
| `gates.<name>.depends_on` | list | `[]` | Gate names; validated (must exist, no self-reference) but not scheduled in v1. |
| `gates.<name>.cache` | bool | `true` | `false` makes the gate always run (still queued). |

Rules:

- Gate names: `[a-z0-9][a-z0-9_-]*`, max 64 chars.
- Unknown fields are an error (strict parsing, at every level), so typos
  cannot silently change behaviour.
- `version` must be `1` (`unsupported version N` otherwise); `cmd` must be
  non-empty after trimming; `weight >= 1`; `key_files` must be relative
  paths without `..`; `depends_on` entries must name existing gates other
  than the gate itself; `human_hook_policy` may not be `fail-fast`.
  `[defaults]` and `[gates]` are optional; a file with only `version = 1`
  is valid (and every `sluice run` then fails with `unknown gate`).
- `cmd` is normalised for the key by trimming and collapsing whitespace
  runs; nothing else.
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
psi_max_wait = "5m"        # admit the head anyway after this long blocked by PSI (must be > 0)

[mcp]
progress_interval = "15s"  # progress notifications while verify runs (floored at 1s, re-read per call)

[cache]
ttl_days = 30
tail_lines = 40

[scheduler]
head_reserve_after = "30s" # backfill behind the queue head only this long (ADR-0007)

[agent_detection]
env_any_set = ["CLAUDECODE"]      # any of these set and non-empty => agent (SLUICE_AGENT is the override, see below)
env_any_equals = {}               # e.g. { CLAUDE_CODE_ENTRYPOINT = "cli" }

[repos."/home/jose/Escritorio/Proyectos/foo"]
budget_share_max = 4       # optional per-repo cap
```

Environment variables (highest precedence):

| Variable | Effect |
|---|---|
| `SLUICE_DISABLE=1` | `sluice run`: run the gate directly (needs the repo and `.sluice.toml`), no cache, no queue, no user config. Through `sluice mcp`: cache off, the gate still queues. |
| `SLUICE_CACHE=0` / `1` | Force cache off / on (on overrides the `CI` rule). |
| `CI` | Cache off unless `SLUICE_CACHE` is truthy. A falsy `CI` (`0`, `false`, `no`, `off`) does not count as set. |
| `SLUICE_AGENT=1` / `0` | Force agent / human detection (truthy / falsy words as below; any other value is ignored). |
| `SLUICE_SOCKET` | Override the daemon socket path. |
| `SLUICE_LOG` | Log filter of the daemon (`sluice daemon`) and MCP (`sluice mcp`) processes, e.g. `debug`; default `info`; written to stderr (the daemon's goes to `daemon.log`). Not read by `sluice run`. |
| `SLUICE_CONFIG` | Override the user config path (ignored when empty). |

Truthy words: `1`, `true`, `yes`, `on`; falsy: `0`, `false`, `no`, `off`
(case-insensitive, trimmed). An unrecognised `SLUICE_CACHE` value falls
through to the `CI` rule.

User config notes: every section and field is optional (a missing file
means defaults); unknown fields are errors; validation: `daemon.budget >= 1`,
`daemon.nice <= 19`, `daemon.ionice_level <= 7`, `admission.psi_poll_interval > 0`,
`budget_share_max >= 1`. `daemon.idle_exit` (always "never" in v1) is parsed and validated but has
no effect; everything else is live
(`log_retention_days` prunes job logs daily from the daemon;
`mcp.progress_interval` paces `verify`'s progress notifications).

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
