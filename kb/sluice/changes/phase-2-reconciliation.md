---
title: "Phase 2 reconciliation report"
tags: [sluice, reconciliation, phase-2]
---

# Phase 2 reconciliation report

Scope: Phase 2 (daemon, scheduler, coalescing, CLI client). Docs reviewed:
`design/protocol.md`, `design/scheduler.md`, `design/executor.md`,
`design/adr/0007-scheduler-fairness.md`, `design/architecture.md` (sections 1,
4b, 5, 6, 9, 9b; 8 and 10 where Phase 2 changed them), `design/config-format.md`
(user config, env table), `changes/phase-2-daemon.md`, `README.md` (status
banner, "What Sluice does", "How it works", "Command surface") and the `--help`
output of `sluice`, `daemon`, `status`, `run`, `logs`. Code:
`crates/sluice-gate-proto/src/lib.rs` (`proto:`), `crates/sluice-gate-daemon/src/*.rs`
(`daemon:`), `crates/sluice-gate/src/{client,daemon_cmd,status,run,logs,main}.rs`
(`bin:`) and `crates/sluice-gate-core/src/{config,dirs}.rs` (`core:`). Phase 3/4/MCP/PSI
statements were skipped. Doc paths are relative to `kb/sluice/` (except README).
Findings come from reading the code; nothing was executed except `--help` and
`cargo test --workspace -- --list` (test counts). No file other than this report
was written.

## 1. Documented but not implemented

| # | Statement | Doc file:line | What the code does instead | Code file:line |
|---|---|---|---|---|
| 1 | A weight above `B` is clamped "(logged once)" | design/scheduler.md:222-223 | The clamp is silent; nothing is logged at enqueue. | daemon:scheduler.rs:154, daemon:server.rs:1178-1180 |
| 2 | "Clients open [`sluice.db`] `mode=ro` only for diagnostics" | design/executor.md:85-86 | `Store::open_read_only` exists but no CLI code calls it; only the daemon integration tests do. No client reads the database. | daemon:store.rs:241, daemon:tests/daemon.rs:468, 685, 719 |
| 3 | `started` is sent to the subscribers of a job "when the gate process starts" | design/protocol.md:57-62 | `started` goes only to subscribers attached before the start. A request that coalesces onto an already running job subscribes afterwards and never gets it, so the CLI prints `queued (position 0 ...)` and nothing until `done`, and `-v` never starts the log follower for it. | daemon:server.rs:646-649, 1229, daemon:scheduler.rs:148, bin:run.rs:335-341 |

## 2. Implemented but not documented

| # | Behaviour | Code file:line | Suggested doc location |
|---|---|---|---|
| 1 | A job whose weight exceeds its repository cap (`repos."p".budget_share_max`) never fits: the weight is clamped to the budget only, not to the cap. Once it is the queue head and has waited `head_reserve_after`, the reservation stops every job behind it, including other repositories, until the cap is raised and the daemon restarted. The reservation also fires when the head is blocked only by the cap. | daemon:scheduler.rs:154, 180-188, 336-347 | scheduler.md "Per-repository cap", config-format.md:120-121, ADR-0007 Consequences |
| 2 | `queued.position` is 1-based; coalescing onto a running job reports `position: 0`, and the CLI prints `sluice: g queued (position 0, U of B units busy), sharing job <id>`. The `U of B` part is dropped when the extra `status` call fails. | daemon:scheduler.rs:34-45, 233-239, daemon:server.rs:1233, bin:run.rs:317-334 | protocol.md `queued` row; architecture.md section 9b |
| 3 | `reason` precedence in `status`: `budget` (does not fit the budget), then `repo-cap`, then `head-reserved` (only for jobs behind a head that has waited long enough); the head itself and anything else shows `budget`. `psi` is never produced in Phase 2. | daemon:scheduler.rs:279-298, daemon:server.rs:1048, 1097 | scheduler.md "Per-repository cap"; protocol.md:98 |
| 4 | `scheduler.head_reserve_after = "0s"` is accepted (no validation), which makes the head reserve immediately (strict FIFO, no backfill). | core:config.rs:413-425, 530-557 | config-format.md:113-114 |
| 5 | `repos."<path>"` keys are compared as exact paths with the worktree top-level reported by git: no `~` or symlink normalisation, and a key that matches no repository is silently ignored. | core:config.rs:431-435, bin:daemon_cmd.rs:45-49, daemon:scheduler.rs:337 | config-format.md:120-121; scheduler.md:230 |
| 6 | The daemon reads the user config once, at start (`UserConfig::load` in `start`). Changes to `daemon.*`, `scheduler`, `repos`, `cache.ttl_days` and `cache.tail_lines` reach the daemon only after `sluice daemon stop`; the client re-reads the file on every run (hit lookup TTL, failure tail, fallback budget). | bin:daemon_cmd.rs:141-143, bin:run.rs:561-567 | config-format.md:140-145 |
| 7 | `sluice daemon stop` output and waiting: `sluice: daemon stopping (N running jobs)` on stdout, or `sluice: no daemon running`, both exit 0; without `--force` it returns right after the `stopping` reply; with `--force` it polls `/proc/<pid>` for up to 10 s so the caller knows the gates are gone. Connection errors print `sluice: error: ...` and exit 1. | bin:daemon_cmd.rs:29-30, 196-242 | executor.md:32-35; README Command surface |
| 8 | `sluice daemon` start-up failures, all exit 1: `sluice: a daemon is already running (lock <path>)` (checked before detaching and again from the server result), `unusable socket path <p>` (`sun_path` length, checked before detaching), config errors, `cannot start the async runtime`. A successful start exits 0 only after `stop` or SIGTERM/SIGINT. `--foreground` is accepted and ignored with `stop`. | bin:daemon_cmd.rs:82-92, 139-194 | executor.md:133 (only "a clear message") |
| 9 | `sluice status` text layout: `daemon:` line (pid, version, uptime, socket), `budget: U of B units busy`, `running: N` with one line per job (gate, repo basename, weight, elapsed, subscribers, job id), `queued: N` with `#i` lines and the reason in parentheses, `totals:` line (runs, passed, failed, timed out, hits, coalesced, time saved). Durations print as `850ms`, `12.3s`, `4m05s`, `2h10m`. Caller and `wait_ms_total`/`run_ms_total` appear only in `--json`. | bin:status.rs:294-375 | README Command surface; architecture.md section 9 |
| 10 | Client timeouts: connect 1 s (retrying `EAGAIN` on a full backlog), write 5 s, read 10 s for `status`/`logs`/`stop`, no read timeout during `run`. Handshake errors: `the running daemon speaks protocol N, this client speaks M; run 'sluice daemon stop' ...` and `the daemon refused the handshake (<message>); run 'sluice daemon stop'`. | bin:client.rs:27-38, 65-89, 99-115 | protocol.md "Handshake" |
| 11 | Autostart details beyond the lock/poll steps: the spawned `sluice daemon` receives the client's environment minus every `GIT_*` variable, stdin null, stdout and stderr appended to `daemon.log`, its own process group; the start lock is a blocking `flock` with no timeout; a non-zero early exit of the spawned process is reported immediately instead of waiting 5 s; failure text is `the daemon could not be started (<why>); see <daemon.log>` (`no socket at <p> after 5s`, `it exited with <status>`, `cannot spawn: ...`). | bin:client.rs:311-390 | executor.md:27-31 (architecture.md:5.1 mentions only `GIT_*`) |
| 12 | Wire errors beyond the documented ones: first line is a valid non-`hello` message -> `error{protocol}` `expected hello`; malformed first line -> `error{bad-request}` `malformed hello: ...`; second `hello` as request -> `duplicate hello`; `cancel` outside a run connection -> `bad-request` `cancel outside a run connection`; any non-`cancel` message on a run connection -> `bad-request` `only cancel is accepted during a run` (connection stays open); line over 1 MiB or invalid UTF-8 -> `bad-request` (id 0); `logs` for an unknown job -> `not-found` `no such job`, for a job with no log yet -> `not-found` `job <id> has no log yet`. | daemon:server.rs:922-927, 936-963, 1029-1040, 1117-1127, 1426-1432 | protocol.md "Errors" |
| 13 | Hit event conditions: sent only when the socket file exists (a hit never autostarts the daemon); `saved_ms` is the marker's `duration_ms`; a failed or slow daemon costs at most about 20 ms. | bin:run.rs:141-150, bin:client.rs:264-294 | executor.md:126; protocol.md:113-119 |
| 14 | `totals` semantics: all-time over the database (never reset by a restart; `since` is the oldest job or event, empty string with none); `runs` counts every finished job except `orphaned`, so it includes daemon-detected `cached` and `error` jobs; `hits` counts only client `event`s, so a `cached` double-check hit adds no hit and no `time_saved_ms`; coalescing is credited as `coalesced x run time` only on jobs that passed; `wait_ms_total` also sums jobs that are still running. | daemon:store.rs:629-705, daemon:server.rs:1083 | protocol.md:99-104; scheduler.md:248-250 |
| 15 | A gate killed by a signal it did not get from Sluice yields `outcome.status = fail`, `exit_code = null`, message `<g> was terminated by a signal after N s`, miss record reason `gate failed` with a null exit code; the CLI prints `FAIL exit=signal` and exits 143. | daemon:server.rs:698-703, 756-759, bin:run.rs:455-460 | executor.md:76-77; protocol.md `outcome` |
| 16 | Invalid or unparsable `SLUICE_LOG` is silently replaced by `info`; the filter uses `RUST_LOG` syntax; without `--foreground` all tracing goes to `<state>/daemon.log` (append, never rotated or pruned). | bin:daemon_cmd.rs:9-10, 112-120, 95-110 | config-format.md:133; executor.md:15-17 |
| 17 | File and directory modes: the runtime directory is `0700` and the socket `0600`, but `daemon.lock`, `start.lock`, `sluice.db` (and its WAL files), `daemon.log`, the `logs/` directory and job logs are created with the process umask (no explicit mode). A custom `SLUICE_SOCKET` directory is created by `create_dir_all` without `0700` if it does not exist. | bin:client.rs:325-345, daemon:server.rs:189-218, daemon:store.rs:218, bin:daemon_cmd.rs:87-90, daemon:executor.rs:277-281 | architecture.md sections 4b and 10 |
| 18 | `SLUICE_SOCKET` moves only the socket: `daemon.lock` and `start.lock` stay in the runtime directory, so a second daemon on another socket is refused with `a daemon is already running`. | core:dirs.rs:51-79 | config-format.md:132; protocol.md:16 |
| 19 | Gates run by the in-process fallback or by `run-unqueued` are outside the daemon: they do not appear in `status`, do not consume the daemon's budget (the fallback uses its own `flock` semaphore of `daemon.budget` slots, `run-unqueued` uses none), and get no `nice`/`ionice`. | bin:run.rs:188-204, 678-707, 712-727 | executor.md:123-126; config-format.md:92 |
| 20 | After `queued`, a lost connection is final: `sluice: error: lost the connection to the daemon: <e>`, exit 1, no fallback and no retry (the job may keep running in the daemon). Before `queued`, any refusal falls back with `sluice: daemon unavailable (<why>); running in-process`. | bin:run.rs:196-199, 347-354 | executor.md:118-125 |
| 21 | A queued job dropped by `sluice daemon stop` (or a subscriber of a cancelled job) reaches the CLI as `killed` without a log or job id, printing `sluice: g KILLED after 0.0s (job ) log=-` and exiting 143. | daemon:server.rs:424-434, 1265, bin:run.rs:359-369, 402, 449-452 | architecture.md section 9b |
| 22 | README environment list omits `SLUICE_SOCKET` and `SLUICE_LOG` (both in `--help` and in config-format.md). | README.md:159-164, bin:main.rs:44-48 | README "Command surface" |

## 3. Mismatches

| # | Doc says (file:line) | Code does (file:line) | Severity |
|---|---|---|---|
| 1 | `welcome.instance` is `"<uuid>"` (design/protocol.md:27) | `"<pid>-<started unix ms>"`, also the value fenced in `jobs.daemon_instance` (daemon:server.rs:271) | wording |
| 2 | Protocol error "(no `id`)" and example `{"type":"error","code":"protocol","message":"..."}` (design/protocol.md:30, 147) | `Error.id` always serialises, so the line carries `"id":0` (proto:lib.rs:298-301, daemon:server.rs:947) | wording |
| 3 | `event`: "The client connects with a 20 ms timeout, writes one line and closes" (design/protocol.md:117-118) | Writes `hello` and `event` in one write, then waits (within the same 20 ms budget) for `welcome` before closing (bin:client.rs:264-294) | internal |
| 4 | `cancel` "detaches that subscriber without closing the socket" (design/protocol.md:132-134) | The server ignores the `id`, detaches, leaves the handler and closes its side (daemon:server.rs:1421-1425, 1444); nothing in the CLI sends `cancel` | internal |
| 5 | `error` outcome covers "daemon stopping or lease lost"; "the client exits 1" (design/protocol.md:78-79) | Those cases send a top-level `error{internal}` before `queued` (daemon:server.rs:1166-1167, 1379-1384); the CLI treats it as a refusal and falls back to the in-process run (bin:run.rs:349-351, 198). Exit 1 applies only to the `error` outcome (spawn or log failure, bin:run.rs:382) | user-visible |
| 6 | `stop` "removes the socket file first (so new clients start a fresh one)" (design/executor.md:32-34); protocol.md:32-33 "the next client restarts a matching daemon" | The old daemon keeps `daemon.lock` until its running jobs drain (daemon:server.rs:268, 325-335, 1026). A client autostarting during that window spawns a daemon that exits 1 with `a daemon is already running` (bin:daemon_cmd.rs:151-157), so the client falls back to the in-process run (bin:client.rs:373-376, bin:run.rs:198) | user-visible |
| 7 | Orphans are `state = 'running'` rows of another instance (design/executor.md:37-38) | `mark_orphaned` also closes `queued` rows of other instances (daemon:store.rs:598-606) | internal |
| 8 | "Record `pgid`, `proc_start_ticks` and `log_path` in SQLite before the first byte of output is read" (design/executor.md:50-51) | The output pumps start inside `executor::spawn` (daemon:executor.rs:334-348); `mark_started` runs afterwards (daemon:server.rs:626-635). Rows of queued jobs have a null `log_path` | internal |
| 9 | Spawn is `sh -c '<cmd> "$@"' sh <extra...>` (design/executor.md:47) | `"$@"` is appended only when extra arguments exist (daemon:executor.rs:295-299); architecture.md:245 and phase-2-daemon.md:216 state the real rule | wording |
| 10 | `gate.timeout_ms = 0` / `0` "disables" the timeout (design/protocol.md:85, design/executor.md:62-63) | True only on the daemon path (daemon:server.rs:573-577). Config accepts `timeout = "0s"` (core:config.rs:122); the in-process fallback and `run-unqueued` treat it as an immediate timeout (`elapsed > 0`, bin:run.rs:854-855). config-format.md:57 does not mention 0 | user-visible |
| 11 | `log_retention_days` "prunes job logs daily from the daemon" (design/config-format.md:145); executor.md:80-81 | Only logs of jobs with a terminal row in `sluice.db` are deleted (daemon:store.rs:712-735); logs written by in-process fallback runs, `daemon.log` and orphan files are never pruned; pruning stops after the lease is lost (daemon:server.rs:466-472) | user-visible |
| 12 | `sluice status --json` "prints this object verbatim" (design/protocol.md:104); change note: `no daemon running` when absent (changes/phase-2-daemon.md:215) | Pretty-printed `StatusReport` without `type`/`id` (bin:status.rs:266). With no daemon, `--json` prints `{"schema":1,"daemon":null}` (bin:status.rs:280), where the schema defines `daemon` as an object | user-visible |
| 13 | `logs` request `"tail":40`; `sluice logs` "asks the daemon for the newest (or the given) job's tail" (design/protocol.md:108, design/architecture.md:249-250) | The CLI sends `tail = 0` without `-n` (daemon default = `cache.tail_lines`) and then reads the whole file itself; the daemon's lines are used only if the file cannot be read (bin:logs.rs:441-446, 488) | wording |
| 14 | Client falls back to the directory "when the daemon is not running" (design/protocol.md:110-111) | Falls back on any failure, including `not-found` for jobs the daemon does not know (fallback-run logs); never autostarts a daemon (bin:logs.rs:477-489) | wording |
| 15 | Reservation when the head "waited ... > `head_reserve_after`" / "longer than" (design/scheduler.md:213, 218) | `>=` (daemon:scheduler.rs:349-351) | internal |
| 16 | In-flight map `key -> JobId` (design/scheduler.md:185); `key -> broadcast::Sender<Outcome>` (design/architecture.md:43-45) | No map: `find_by_key` scans running then queued jobs; per-job `broadcast::Sender<JobEvent>` lives in the server (daemon:scheduler.rs:252-258, daemon:server.rs:113-120). The two docs also contradict each other | internal |
| 17 | "no weight resizing in v1" (design/adr/0007-scheduler-fairness.md:160) | Weights above the budget are clamped to it (daemon:scheduler.rs:154); scheduler.md:222 says so | wording |
| 18 | Cap "limits the units running for one repository" (design/scheduler.md:228-230); config-format.md:121 "per-repo cap" | Counted per worktree top-level, so N worktrees of one project get N independent caps (daemon:scheduler.rs:336-347, bin:run.rs:227) | user-visible |
| 19 | Metrics per job include `wait_ms`, `run_ms` (design/scheduler.md:246); jobs table with "wait/run durations" (design/architecture.md:164-166) | Not stored; derived from `started_at - queued_at` and `finished_at - started_at` in `totals` (daemon:store.rs:651-657) | wording |
| 20 | "FIFO with a no-starvation rule ...; to be recorded as an ADR" (design/architecture.md:141) | ADR-0007 exists and is accepted | wording |
| 21 | Executor runs "in the requester's working directory" / "requester's cwd" (design/architecture.md:142, 269) | `cwd` is the worktree top-level, also when `sluice run` starts in a subdirectory (bin:run.rs:245) | wording |
| 22 | `<data-dir>/logs/...`, `<data-dir>/sluice.db` (design/architecture.md:144, 163) | The directory is `<state>` (core:dirs.rs:11-34); section 4b uses that name | wording |
| 23 | Human `wait`: "one `waiting for N of M budget units` notice when blocked (progress output arrives with the daemon)" (design/architecture.md:199) | On the daemon path the notices are `queued (position N, U of B units busy)` and `running (job ID)` (bin:run.rs:333, 336); `waiting for N of M` appears only in the in-process fallback (bin:run.rs:698-701) | wording |
| 24 | "When the daemon is unavailable (and under `run-unqueued`), `sluice run` ... takes `weight` slots of a `flock` counting semaphore" (design/architecture.md:245-247) | `run-unqueued` returns to `execute` without acquiring slots (bin:run.rs:188-190); only the fallback takes them (bin:run.rs:200-204). executor.md:125 and the architecture.md:200 table agree with the code | user-visible |
| 25 | Commands of a later phase print `sluice: '<cmd>' is not implemented yet` (design/architecture.md:29) | `... is not implemented yet (it arrives with the daemon and MCP phases)`; only `mcp` remains, and the daemon phase is done (bin:main.rs:192-196) | wording |
| 26 | Hit path "no daemon involved" (README.md:82-83, design/architecture.md:38) | A hit sends the best-effort `hit` event when the socket exists (bin:run.rs:141-150); architecture.md:43 and 9b state the nuance | wording |
| 27 | "143 gate killed by a signal" (README.md:148-149, design/architecture.md:218-219) | Also a queued job dropped by `daemon stop`, a cancelled job and `daemon stop --force`; `--help` says so (bin:run.rs:449-452, bin:main.rs:42-43) | wording |
| 28 | `CLI 8 unit + 8 + 19 + 8 integration` (changes/phase-2-daemon.md:225) | `cargo test -- --list`: CLI binary has 10 unit tests; core 89, proto 11, daemon 26 + 12, CLI 8 + 19 + 8 integration match | internal |
| 29 | Fallback prints a `running in-process` notice (changes/phase-2-daemon.md:216) | `sluice: daemon unavailable (<why>); running in-process` (bin:run.rs:198); architecture.md:5.1 has the right text | wording |
| 30 | `daemon.nice` / `ionice` "applied to gate processes" (design/config-format.md:92-94) | Applied only to gates run by the daemon, through the `renice`/`ionice` binaries when found in `PATH` (daemon:executor.rs:372-401); the fallback and `run-unqueued` gates keep the client's priority | wording |
| 31 | `Status: design for Phase 2` and "to be frozen when Phase 2 closes" (design/protocol.md:8, design/scheduler.md:8, design/executor.md:8) | Phase 2 is closed and the protocol is implemented as `PROTOCOL_VERSION = 1` (proto:lib.rs:31) | wording |

## Summary

Phase 2 matches its design closely: the wire message names and fields, event
order, 1 MiB line limit, 10 s handshake timeout, 5 s progress tick, 15 s/90 s
lease, 5 s kill grace, 20 ms hit event, 5 s autostart wait, SQL schema columns,
scheduler rules (FIFO, backfill, reservation, repo cap, double-check before
start), reasons, exit codes 124/143 and the config defaults all agree. The
important gaps are: a job heavier than its repository cap never runs and, once
it is the head, freezes the whole queue after `head_reserve_after` (undocumented);
`daemon stop` without `--force` leaves `daemon.lock` held, so clients started
during the drain fall back in-process instead of getting a fresh daemon, while
the docs promise a fresh daemon; `timeout = "0s"` means "no timeout" on the
daemon but "immediate timeout" in the fallback; `architecture.md` 9b says
`run-unqueued` takes semaphore slots but it does not; `status --json` with no
daemon prints a shape (`"daemon":null`) that contradicts the schema; the
per-repo cap is per worktree; log pruning ignores fallback logs; and late
coalescers never receive `started`. Most other differences are wording.

Counts: Table 1 (documented but not implemented) = 3; Table 2 (implemented but
not documented) = 22; Table 3 (mismatches) = 31.
