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

## Why did a job wait? `sluice logs --timeline`

```
sluice logs --timeline [JOB_ID] [--since 10m]
```

Prints one line per job from the daemon's database (it works without a
running daemon): `<job> <gate> <repo>: requested +0.0 queued +0.0 admitted
+31.2 (over) started +31.3 done +43.9 pass; waited budget 29.0s,
order 2.2s; demand 6.1/8; 3 request(s)`. Times are seconds after the job was
queued; `admitted` carries the kind of admission when it was not within the
budget (`over`: above it, by over-admission; `guard`: by the PSI starvation
guard); `waited` splits the queue time by cause (`budget`, `psi`, `repo-cap`,
`head-reserved`, `order`); `demand 6.1/8` is the measured CPU demand against
the configured weight. Without a job id it lists the jobs queued within
`--since` (default `10m`; exit 2 when the duration is invalid).

## `sluice doctor`

```
sluice doctor [--json] [--bundle [PATH]]
```

Checks, each `ok`, `warn` or `fail` with a one-line detail: git is found and
at least 2.31; the PSI files are readable; the state and runtime directories
exist (the runtime directory must be mode 0700); `socket-path`: the socket
path fits the 107-byte Unix socket limit, or the short fallback is in use
(see below), or it is too long; the daemon socket answers
and the daemon runs the installed version (otherwise: `run 'sluice daemon
restart'`); the lease row matches the live daemon pid; `database`: the
daemon's `sluice.db` opens (read-only, as `doctor` reads it) with the
expected schema, otherwise `fail` with the SQLite error (a daemon that
cannot open it does not start, and every gate then runs in-process; move
the file aside and the next daemon creates a fresh one, losing the job
history); the user config parses; inside a repository, `.sluice.toml`
parses, a hook in the hooks directory (`core.hooksPath` or `.git/hooks`)
calls sluice, `queue-only-hooks`: a warning when a `mode = "queue-only"`
gate lists hooks, `scoped-gates`: one line per path-scoped gate, a warning
when `paths` matches no file or a well-known build input (`Cargo.toml`,
`package.json`, lockfiles, `Makefile`, `Dockerfile`, ...) or a symlink target
lies outside the scope, and `markers`: the marker directory size, `fail` when the
directory is not writable (passes are then never cached), `warn` with the
count and the first path when marker or miss files are damaged (truncated,
not valid JSON, or recorded under a key other than their file name; runs
ignore and replace them, `sluice cache clear` removes them); the `sluice`
found in `PATH` is the binary you are running. It exits 1 when any check
fails and never uses the network (it does not look for new releases;
`sluice update --check` does).

## Damaged or read-only storage

- A read-only `logs/` (or state) directory never stops a gate: it runs, its
  output is kept in memory for the failure tail only, and the run prints
  once `sluice: warning: cannot create <log>: Permission denied (os error
  13); the output was kept in memory for the tail only`. `sluice logs`
  has nothing to show for that job.
- A read-only markers directory: the gate's verdict is returned, the pass
  says `(not cached: cannot write the marker: …)`, and `doctor` fails the
  `markers` check.
- If the daemon is killed (SIGKILL, OOM), its gate reaper (`sluice daemon
  reaper --daemon <pid>`, started with the daemon) terminates every gate it was running,
  descendants included, within about 5 s; waiting clients print `the daemon
  died while running <gate>; it was killed` and exit 143. The next client
  starts a new daemon, which also kills any gate group a dead daemon left
  behind and records those jobs as `killed`.

## Socket path too long

A Unix socket path can be at most 107 bytes on Linux (108 with the
terminating NUL). When `$XDG_RUNTIME_DIR/sluice/daemon.sock` is longer (a
deeply nested `XDG_RUNTIME_DIR`, common in containers and test harnesses),
the daemon and every client use `/tmp/sluice-<uid>/daemon.sock` instead;
nothing to configure. You can see it in three places:

- `daemon.log`: `socket path fallback in use: <preferred> is N bytes, ...`;
- `sluice status`: `socket:   fallback in use (...)` under the `daemon:` line;
- `sluice doctor`: `[ok  ] socket-path    fallback in use: ...`.

Sluice creates `/tmp/sluice-<uid>` with mode 0700. It refuses to use it,
with an error naming the directory and the problem, when it is a symlink, is
not a directory, is owned by another user, or is group or world writable
(someone else may be able to replace the socket). Fix it with
`chmod 700 /tmp/sluice-<uid>`, or remove it if it is not yours, and retry.

`SLUICE_SOCKET` always wins. When it is longer than the limit, `sluice run`
exits 1 with `SLUICE_SOCKET is N bytes, longer than the 107-byte Unix socket
limit` instead of silently running the gate in-process; set a shorter path.

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
