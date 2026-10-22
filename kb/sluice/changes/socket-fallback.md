---
title: "Change: socket path fallback (alpha task C)"
tags: [sluice, change, daemon, socket, alpha]
---

# Change: socket path fallback (2026-10-22)

Task C of [[sluice/plan/alpha-plan.md]]. Design: [[sluice/design/protocol.md]]
("Transport").

## What changed

- When `$XDG_RUNTIME_DIR/sluice/daemon.sock` (or the computed default) does
  not fit in `sockaddr_un.sun_path`, the daemon and every client use
  `/tmp/sluice-<uid>/daemon.sock`. The limit is computed, not hardcoded:
  `SUN_PATH_LEN = size_of::<sockaddr_un>() - size_of::<sa_family_t>()`
  (108 on Linux, including the NUL), `SOCKET_PATH_MAX = 107`; a unit test
  cross-checks it against `std::os::unix::net::SocketAddr::from_pathname`.
- The choice is a pure function of the environment and the uid
  (`choose_socket`), so the daemon and the clients agree without talking.
- The fallback directory is created `0700` (last component only) and
  refused, with `unusable daemon socket path <dir>: refusing the socket
  fallback directory: <problem>`, when it is a symlink, not a directory, not
  owned by the uid, or group/world writable. It is never chmodded or reused.
- `SLUICE_SOCKET` keeps precedence; an over-long value is an
  `Error::SocketPath` naming the variable, the length and the limit.
  `sluice run` now exits 1 on any `SocketPath` error instead of falling back
  in-process silently (other daemon failures still fall back as before).
- No abstract sockets.
- The daemon logs `socket path fallback in use: <preferred> is N bytes, …`
  at start (the existing `daemon started socket=…` line shows the path);
  `sluice status` adds `socket:   fallback in use (…)`; `sluice doctor` has a
  new `socket-path` check: ok (path and length), ok "fallback in use: …
  because …", fail when too long or the fallback directory is unsafe.
- Test-only `SLUICE_TEST_SOCKET_FALLBACK_DIR` replaces the `/tmp` base, so
  the suite never touches the user's real `/tmp/sluice-<uid>`.

## Files and symbols

- `crates/sluice-gate-core/src/dirs.rs`: `SUN_PATH_LEN`, `SOCKET_PATH_MAX`,
  `TEST_FALLBACK_BASE_ENV`, `SocketSource`, `SocketLocation`
  (`fallback_reason`), `socket_path_fits`, `current_uid`, `choose_socket`,
  `fallback_socket_dir`, `unsafe_socket_dir`, `ensure_fallback_dir`,
  `plan_socket`, `socket_location`; `socket_path` and
  `socket_path_no_create` now delegate to them; `runtime_dir_path` uses the
  shared `runtime_dir_for`.
- `crates/sluice-gate-core/src/error.rs`: `Error::SocketPath { path, problem }`.
- `crates/sluice-gate/src/daemon_cmd.rs`: fallback log line in `start`.
- `crates/sluice-gate/src/run.rs`: `SocketPath` errors are fatal before
  `via_daemon`.
- `crates/sluice-gate/src/status.rs`: `render` takes the fallback reason.
- `crates/sluice-gate/src/doctor.rs`: `check_socket_path`.
- Every other caller of `dirs::socket_path` (client autostart, MCP, logs,
  update, daemon stop/restart) gets the fallback without changes.
- Tests: unit tests in `dirs.rs` (limit, default up to the limit,
  deterministic fallback, explicit precedence and error, too-long fallback,
  directory creation and the owner/mode/symlink/file refusals) and
  `status.rs`; integration `crates/sluice-gate/tests/socket_fallback.rs`
  (XDG_RUNTIME_DIR > 120 bytes: two concurrent runs go through the daemon on
  the fallback socket and coalesce, status shows the job and the fallback,
  the daemon log names it, doctor reports it; 0777 and symlinked fallback
  directories refused by `run` and `daemon --foreground`; over-long
  `SLUICE_SOCKET` is an error, not an in-process run);
  `tests/debugging.rs` expects the `socket-path` check.
- Docs: `kb/sluice/design/protocol.md`, `docs/debugging.md`.

## Why

The first `s1_hooks` alpha run used a deep `XDG_RUNTIME_DIR`: the daemon
refused to bind and clients silently ran in-process, losing the queue and
coalescing. A wrong owner can only be simulated as non-root (unit test with
a different uid); the integration tests cover mode and symlink.

## Verification

```
sluice: fmt PASS 0.4s (job 1792704636147-1757352-c8730a)
sluice: clippy PASS 3.4s (job 1792704636628-1757352-7ae7a2)
sluice: test PASS 52.6s (job 1792704642912-1757352-5e324d)
```
