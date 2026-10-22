# Debugging Sluice

## Where everything is logged

All paths are under the state directory, `$XDG_STATE_HOME/sluice` or
`~/.local/state/sluice`.

| File | What it holds | Retention |
|---|---|---|
| `logs/<job-id>.log` | Output of one gate run (`sluice logs [job-id]`) | `[daemon] log_retention_days` (7) |
| `daemon.log` | The daemon's own tracing output (stdout and stderr of the detached daemon) | Rotated above `[debug] daemon_log_max_mib` (20 MiB): `daemon.log.1` to `.3` |
| `logs/mcp-<pid>.log` | Tracing output of one `sluice mcp` process (also on its stderr; stdout is the MCP channel and is never written) | `[daemon] log_retention_days`, pruned by the daemon |
| `logs/client.log` | One line per `sluice run`, hit or miss, hook or not | Rotated above 5 MiB to `client.log.1` (one generation) |

`client.log` lines look like this:

```
2026-10-22T10:00:00Z pid=4242 caller=agent hook=pre-commit gate=test tree=0123456789ab decision=hit exit=0 ms=12 cwd=/home/u/repo
```

`caller` is `agent` or `human`; `hook` and `tree` are `-` when unknown;
`decision` is `hit`, `miss-run` (queued in the daemon), `miss-fail-fast`
(refused by `agent_hook_policy`, exit 3), `coalesced` (shared a job another
caller started), `in-process` (ran without the daemon), `disabled`
(`SLUICE_DISABLE`) or `error`. Writing the line never fails the command.

## Raising the log level

Precedence: the `SLUICE_LOG` environment variable, then `[debug] level` in
`~/.config/sluice/config.toml`, then `info`. The value uses `RUST_LOG`
syntax (`debug`, `sluice_gate_daemon=debug,info`). It applies to the daemon,
`sluice mcp` and the messages `sluice run` prints on stderr.

```toml
[debug]
level = "debug"
daemon_log_max_mib = 20
```

The daemon reads the level at start. To change it without editing anything:

```
sluice daemon restart --debug      # debug for that daemon instance only
sluice daemon restart              # back to the configured level
sluice daemon restart --force      # also kill running gates
```

Without `--force`, restart refuses while gates are running. `sluice status`
shows the daemon's active level and the log path (`log:  level debug, ...`).
An MCP server picks up its level when the agent starts it.

## `sluice doctor`

```
sluice doctor [--json] [--bundle [PATH]]
```

Checks, each `ok`, `warn` or `fail` with a one-line detail: git is found and
at least 2.31; the PSI files are readable; the state and runtime directories
exist (the runtime directory must be mode 0700); the daemon socket answers
and the daemon runs the installed version (otherwise: `run 'sluice daemon
restart'`); the lease row matches the live daemon pid; the user config
parses; inside a repository, `.sluice.toml` parses, a hook in the hooks
directory (`core.hooksPath` or `.git/hooks`) calls sluice, and the marker
directory size; the `sluice` found in `PATH` is the binary you are running.
It exits 1 when any check fails and never uses the network (it does not look
for new releases; `sluice update --check` does).

## `--bundle`

`sluice doctor --bundle` writes `./sluice-doctor-<timestamp>.tar.gz` (or the
given path) with: `doctor.json`, `version.txt`, `user-config.toml` and
`repo-sluice.toml` (if present), `status.json`, `queue-status.json` (when the
daemon runs the queue), and under `logs/`: the last 2000 lines of
`daemon.log` and `client.log`, the newest three `mcp-*.log` and, under
`logs/jobs/`, the logs of the last five failed jobs.

Every text file is redacted before it is archived:

- your home directory becomes `~`;
- the value of any `KEY=value`, `key: value` or JSON `"key": "value"` whose
  key contains `token`, `secret`, `password`, `passwd`, `auth`, `key` or
  `credential` (case-insensitive) becomes `<redacted>` (this also hides
  values such as `key_inputs` in a config, on purpose);
- the `user:password@` part of URLs is removed.

Redaction is pattern based. Logs of your gates can still contain project
names, commands and paths, so open the bundle and review it before attaching
it to an issue.
