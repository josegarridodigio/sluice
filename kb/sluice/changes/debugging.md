# Debugging and observability

## What changed

- `[debug]` user config table: `level` (validated as an `EnvFilter` directive
  at parse time) and `daemon_log_max_mib` (default 20, 0 disables rotation).
  Level precedence `SLUICE_LOG` > `[debug] level` > `info`, shared by the
  daemon, `sluice mcp` and `sluice run` (`resolve_log_level`).
- `sluice daemon restart [--debug] [--force]`: refuses while gates run unless
  `--force`; stops the daemon, then autostarts a detached one with
  `SLUICE_LOG=debug` when `--debug` (that instance only).
- `sluice status` shows `log:  level <l>, <state>/daemon.log`; status reply
  gained the optional `daemon.log_level` (schema stays 1, fixture unchanged).
- `sluice mcp` also writes `<state>/logs/mcp-<pid>.log`; the daemon prunes
  `mcp-*.log` with the job-log retention.
- `sluice run` appends one line to `<state>/logs/client.log` (format and
  decisions in `docs/debugging.md`), `O_APPEND`, one write per line, errors
  debug-logged, rotation to `client.log.1` above 5 MiB.
- `daemon.log` rotation (3 generations) at daemon start and from the daily
  pruning task, reopening stdout/stderr with the safe `dup2` wrappers.
- `sluice doctor [--json] [--bundle [PATH]]` with redacted `.tar.gz` bundle.

## Files and symbols

- `sluice-gate-core`: `config::{DebugConfig, resolve_log_level, DEFAULT_LOG_LEVEL}`,
  `UserConfig.debug` (+ validation), new `logfile` module (`rotate`,
  `rotate_if_larger`, `append_line`, `is_job_log_name`, `is_mcp_log_name`),
  `dirs::{runtime_dir_path, socket_path_no_create}`; new dependency
  `tracing-subscriber` (env-filter only) for parse-time validation.
- `sluice-gate-proto`: `DaemonInfo.log_level: Option<String>`.
- `sluice-gate-daemon`: `server::{LogSettings, DaemonConfig.log, DAEMON_LOG_KEEP,
  LEASE_NAME (now pub), rotate_daemon_log}`, `Store::prune_unowned_logs`
  (mcp logs explicit, `client.log` excluded).
- `sluice-gate-mcp`: `init_tracing` (registry with stderr + file layers),
  `open_mcp_log`; job-log lookup ignores non-job logs.
- `sluice-gate` (CLI): `clientlog.rs`, `logging.rs`, `doctor.rs`, `run::{run,
  Trace}` (wrapper around the former `run`), `daemon_cmd::{restart, log_level,
  detach}`, `status::render`, `logs::show` (digit-first job logs), `main.rs`
  (`Doctor`, `DaemonCommand::Restart`). New direct dependencies: `regex`,
  `serde` (derive).
- Docs: `docs/debugging.md`, README "Debugging", CONTRIBUTING bug report,
  `design/config-format.md`, `design/executor.md`, `design/protocol.md`.

## Why

The log level was fixed at daemon start, the MCP server and the hook client
left no persistent trace, `daemon.log` grew without bound and a bug report
needed manual collection.

## Decisions and caveats

- `client.log` and `mcp-*.log` live in `logs/` so they share its permissions
  and retention; `sluice logs` and the MCP `logs` tool now pick the newest
  name that starts with a digit so they never return them.
- The exit code recorded in `client.log` is read back from `ExitCode`'s debug
  form (there is no accessor on stable); a unit test pins it.
- Redaction is deliberately broad (`key` also hides `key_inputs` values).
- A long-idle `sluice mcp` whose log has not been written for the retention
  period loses its file to pruning (the process keeps writing to the
  unlinked file).
- Runtime `daemon.log` rotation (24 h task) has no integration test; the start
  path has. `SLUICE_TEST_CLIENT_LOG_CAP` is test-only.

## Verification

Unit tests: `[debug]` parsing and level precedence (`config.rs`), `logfile`
rotation, `client.log` line format, redaction, git version parsing, store
pruning of mcp/client logs, status fixture for `log_level`.
Integration (`crates/sluice-gate/tests/debugging.rs`, `mcp.rs`): client.log
for hit, miss, fail-fast, disabled and error, rotation at the cap, a broken
log never failing the command, daemon.log rotation at start, restart
(graceful, refusal with running gates, `--debug` and config level in
`status`), doctor (healthy, broken `.sluice.toml`, broken user config, stale
socket), bundle content and redaction, MCP log file with content.
Final run: `sluice run fmt`, `sluice run clippy`, `sluice run test`.
