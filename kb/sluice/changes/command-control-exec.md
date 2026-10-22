---
title: "Change: sluice exec, queue-only gates and SLUICE_JOB (alpha A1)"
tags: [sluice, change, alpha, exec, command-control]
---

# Change: `sluice exec`, queue-only gates and `SLUICE_JOB` (2026-10-23)

Implements alpha task A1 ([[sluice/plan/alpha-plan.md]]) from the design
[[sluice/design/command-control.md]] sections 0, 2, 3 and the A1 part of
section 7; decision record [[sluice/design/adr/0010-command-control.md]].
Protocol stays 1, `status` schema stays 1, `.sluice.toml` stays `version = 1`;
every addition is optional ([[sluice/design/compatibility.md]]).

## What changed

- **`sluice exec [--weight N] [--label L] [--coalesce-key K] [--timeout DUR]
  [-q] -- <cmd> [args...]`** (`crates/sluice-gate/src/exec.rs`, `exec()`):
  resolves `argv[0]` in `PATH` skipping shims (127/126 on failure), runs it in
  the daemon under the budget (default weight `ceil(budget / 2)`), replays the
  output spool byte for byte to stdout/stderr (`replay`, `Replay`), forwards
  SIGINT/SIGTERM/SIGHUP (blocked with `SigSet::thread_block`, read from a
  `SignalFd` in `signal_thread`), prints the queued/started/sharing lines
  (`notice_thread`), and maps the outcome (`report`): the command's code,
  128+N for signal N, 124 timeout, 143 daemon stop, 125 Sluice error after
  `queued`, 141 when stdout is a closed pipe. A signal while queued hangs up
  (`interrupted while queued`, 128+N). Falls back in-process (`in_process`:
  `budget::acquire` slots, inherited stdio, `SLUICE_JOB=inproc-<pid>`, signals
  forwarded with `kill(child)`, in-process `--timeout`) when the daemon is
  down, lacks the `exec` feature, or refuses before `queued`; also for
  `SLUICE_DISABLE=1`. Inside a job (`SLUICE_JOB` set) it `exec`s the command
  directly.
- **Shared in-process budget**: `acquire_budget` moved from `run.rs` to
  `crates/sluice-gate/src/budget.rs` (`budget::acquire`).
- **PATH resolution**: new `sluice_gate_core::which` (`resolve`,
  `NotRunnable`, `SHIM_MARKER`, `SHIM_DIR_VAR`, `default_shim_dir`), ready for
  H5's shims. New constant `sluice_gate_core::JOB_ENV_VAR` (`SLUICE_JOB`).
- **Protocol** (`sluice-gate-proto`): `ClientMessage::Exec { id, request:
  ExecRequest }`, `ClientMessage::Signal { id, signal: ExecSignal }`,
  `FEATURE_EXEC`, `FEATURE_QUEUE_ONLY`, `JOB_KIND_EXEC`; optional
  `Started.spool`, `Outcome.signal`, `RunRequest.no_marker` (skipped when
  false), `RunningJob`/`QueuedJob` `kind` and `label`. Client:
  `Client::exec` → `ExecConn` (`recv`, `control`) and `ExecControl` (`signal`,
  `hang_up`).
- **Daemon executor** (`executor.rs`): `ExecSpec` gained `launch: Launch`
  (`Shell`/`Argv`) and `sink: Sink` (`Log`/`Spool`) replacing `cmd`/`extra`;
  module `executor::spool` (`MAGIC`, `write_record`, `parse`, `merged`);
  `ExecStatus::Signaled(i32)` carries the signal; `tail_lines` decodes spools.
- **Daemon server** (`server.rs`): `JobRequest` (`Gate`/`Exec`) in `JobEntry`
  (plus `spool`, `process`, `exec_identity`); `State::exec_inflight` for
  `--coalesce-key` coalescing (`exec_identity`, SHA-256 of
  `sluice-exec-v1\0 K \0 cwd \0 argv`); `register_job`/`register_exec`/
  `coalesce_into`; `start_exec`, shared `launch`; `handle_exec` with
  `forward_signal` (killpg when sole subscriber and running, SIGKILL of the
  leftover group once the leader is gone, cancel after `KILL_GRACE` + 5 s;
  otherwise detach); `detach` ignores `finish` for exec jobs; welcome
  announces `exec` and `queue-only` before extension features; every gate and
  exec process gets `SLUICE_JOB=<job id>`; `no_marker` skips the marker
  double-check and all marker/miss writes; `status` fills `kind`/`label`;
  `signal` outside an exec connection is `bad-request`. Store: retention also
  deletes `<id>.exec`.
- **Queue-only gates**: `Gate::mode: GateMode` (`gate` | `queue-only`,
  `Gate::is_queue_only`) in `config.rs`; `RepoConfig::check_queue_only`
  rejects an explicit `cache = true` and forces `cache = false` on queue-only
  gates. `run.rs`: `queue_only_key` (`sha256("queue-only\0" K)`), no lookup,
  `fail-fast` turns into `wait`, `no_marker` + key sent only to a daemon with
  `queue-only`, no "not cached" note. MCP `verify` reports a plain pass.
  `doctor::queue_only_hook_warning` feeds a `queue-only-hooks` doctor warning
  and a `sluice hook print` warning.
- **`sluice run` inside a job**: a miss with `SLUICE_JOB` set runs
  in-process, unqueued (client-log decision `nested`); in-process gate
  processes carry `SLUICE_JOB`.
- `sluice logs` decodes `.exec` spools (and finds `<id>.exec` without a daemon).
- Docs: `kb/sluice/design/protocol.md` (`exec`, `signal`, features, spool,
  `no_marker`, `kind`/`label`), `executor.md`, `config-format.md` (`mode`,
  `SLUICE_JOB`), `docs/gates-cookbook.md`, `README.md`, `sluice --help`.

## Deviations from the design

- `[agent] exec_weight` is not read: the `[agent]` user table belongs to A2
  (design 4.4) and touching the strict user config here would collide; the
  default weight is `ceil([daemon] budget / 2)` from the client's user config.
- Queue-only gates get `cache = false` at parse time instead of special cases
  in `lookup.rs` (owned by the parallel task D1). Consequence: MCP `verify`
  and the merge-queue runner send them with `key = null` (they always run and
  never write markers, but do not coalesce there); `sluice run` coalesces.
- `which::resolve(name, env, cwd, skip_shims)` takes the environment map
  (for `PATH` and `SLUICE_SHIM_DIR`) instead of a `PATH` string.
- Signal forwarding adds one step: once the leader has exited after a
  forwarded signal, the rest of its process group is SIGKILLed right away
  (background children of a non-interactive `sh` ignore SIGINT and would
  otherwise hold the output pipes until the 10 s escalation).
- A job killed after a forwarded signal exits 128+N (the signal received),
  not 143.
- A broken user config makes `sluice exec` warn and use defaults (exec never
  fails because Sluice is broken). The "daemon unavailable" line is omitted
  with `--quiet`. `SLUICE_DISABLE=1` uses the in-process path.
- Test plan: large output is 12 MiB (not 200 MB) to keep the suite fast on a
  shared machine; queue-only "different trees" is implied by "always runs".

## Verification

- New tests: `crates/sluice-gate/tests/exec.rs` (16: exit codes incl. 137,
  127, 126, 124; stdout/stderr separation, 1 MiB binary and 12 MiB output by
  SHA-256; per-stream order; stdin `/dev/null`, no TTY, env, cwd, `SLUICE_JOB`,
  `logs` of the spool; SIGINT/SIGTERM → 130/143 with leader and grandchild gone
  within 6 s; signal while queued; client SIGKILL under `finish`; 20
  concurrent `--weight 2` with budget 8 → peak ≤ 4 by counter file and `/proc`,
  `status --json` with `kind: exec`, labels and `reason: budget`; coalescing
  with key / different argv / no key; nested exec with budget 1; `daemon stop
  --force` → 143; daemon down, v0.2 fake daemon and lying fake daemon →
  in-process with identical output and code, semaphore serialises; queue-only
  never caches, coalesces two simultaneous requests, agent hook waits, `cache =
  true` error). Daemon tests (`tests/daemon.rs`): welcome features, exec spool
  and `SLUICE_JOB`, `logs` decoding, `signal` refusal, `no_marker`. Unit tests:
  `which`, `spool`, `GateMode`, labels, exit codes, proto round trips and
  additive-field golden checks, status schema `kind`/`label`.
- Shared-machine robustness: tests disable PSI admission in their user config
  and synchronise on `status` (subscriber counts) and go-files instead of
  sleeps.
- Dogfooding from the worktree root: `sluice run fmt` PASS, `sluice run
  clippy` PASS, `sluice run test` PASS.
