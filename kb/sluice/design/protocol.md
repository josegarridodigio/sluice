---
title: Daemon protocol
tags: [sluice, design, protocol, phase-2]
---

# Daemon ↔ client protocol (v1)

Status: implemented in Phase 2 as `PROTOCOL_VERSION = 1` (frozen; additive
changes only). Types live in `sluice-gate-proto` (serde, no I/O); the
blocking client lives in the same crate behind the `client` feature
([[sluice/design/adr/0008-client-in-proto-crate.md]]). Context: [[sluice/design/architecture.md]]
section 5, [[sluice/design/scheduler.md]], [[sluice/design/executor.md]].

## Transport

- Unix stream socket at `<runtime-dir>/daemon.sock`
  (`$XDG_RUNTIME_DIR/sluice/daemon.sock`, fallback `/tmp/sluice-<uid>/daemon.sock`),
  directory mode `0700`, socket `0600`. `SLUICE_SOCKET` overrides the
  socket path only; `daemon.lock` and `start.lock` stay in the runtime
  directory, so one daemon per user regardless of the socket path.
- Newline-delimited JSON: one JSON object per line, UTF-8, no pretty
  printing. Lines longer than 1 MiB are rejected.
- The first line in each direction is the handshake. One connection serves
  one request: `run`, `status`, `logs`, `stop`, `event` or one of the
  `queue-*` requests (section "Merge queue"). A `run`
  connection stays open until the job finishes or either side closes it.
- Closing a `run` connection unsubscribes the client (see "Disconnects").

## Handshake

Client → `{"type":"hello","protocol":1,"client":"sluice-gate 0.1.0","pid":4242}`
Server → `{"type":"welcome","protocol":1,"daemon":"sluice-gate 0.1.0","instance":"<pid>-<start unix ms>","pid":1234,"started_at":"<rfc3339>"}`

`instance` is also the value fenced into `jobs.daemon_instance`.

`welcome` has an optional `features` array of strings, omitted when empty
(so a daemon without extensions sends exactly the line above). A daemon
running the merge-queue extension sends `"features":["queue"]`
(`sluice_gate_proto::FEATURE_QUEUE`); the blocking client exposes it as
`Client::features()` / `Client::supports("queue")`. A `welcome` without
`features` parses as an empty list.

A daemon that does not speak the client's `protocol` answers
`{"type":"error","id":0,"code":"protocol","message":"..."}` and closes. A
client that receives a `welcome` with a different `protocol` reports
`the running daemon speaks protocol N, this client speaks M; run 'sluice
daemon stop'`. Client timeouts: 1 s to connect (retrying a full backlog),
5 s per write, 10 s to read a `status`/`logs`/`stop` reply, none while a
`run` is in flight.

## Requests and replies

Every message has a `type`. Client requests carry an `id` (u64, chosen by
the client, echoed in every reply so multiplexing can be added later).

### `run`

```json
{"type":"run","id":1,
 "repo":{"toplevel":"/abs","common_dir":"/abs/.git","git_dir":"/abs/.git"},
 "gate":{"name":"test","cmd":"npm test","weight":4,"timeout_ms":1200000,"env":{"NODE_ENV":"test"},"cache":true},
 "key":"<64 hex>|null","tree":"<hash>|null","committed_tree":"<hash>|null",
 "cmd":"npm test","extra":["--grep","x"],
 "cwd":"/abs/worktree","child_env":{"PATH":"...","HOME":"..."},"probe_env":{"GIT_DIR":"..."},
 "caller":"agent|human","session":"<CLAUDE_CODE_SESSION_ID or null>","client_pid":4242,
 "hook":"pre-commit|null"}
```

`key`/`tree` are `null` when the tree is unknown or caching is off: the
daemon then runs the gate without coalescing and without writing a marker.
`child_env` is already sanitised by the client ([[sluice/design/cache-key.md]]).

Server events on that connection, in order:

| Event | Fields | When |
|---|---|---|
| `queued` | `id`, `job_id`, `position` (1-based; `0` when attached to a job that is already running), `coalesced` (bool), `weight` | always first; `coalesced=true` when attached to an existing job |
| `started` | `id`, `job_id`, `started_at` | when the gate process starts; a request attached to a running job receives it right after `queued`, with the job's original `started_at` |
| `progress` | `id`, `job_id`, `elapsed_ms`, `log_bytes`, `position` (while queued) | every 5 s while queued or running |
| `done` | `id`, `job_id`, `outcome` | once, then the server closes |

`outcome`:

```json
{"status":"pass|fail|timeout|killed|cached|error",
 "exit_code":0,"duration_ms":32100,"log_path":"/.../<job>.log",
 "tail":["last","lines"],"marker_written":true,"not_cached_reason":null,
 "coalesced_with":"<job_id or null>","message":"human readable one-liner"}
```

`cached` is returned when a marker for `key` appeared while the request was
queued (another client finished the same tree): the daemon re-checks the
marker store right before starting a job ([[sluice/design/scheduler.md]]).
`error` carries a `message` and means Sluice itself failed after the job
was accepted (cannot spawn, cannot write the log); the client exits 1. A
daemon that is stopping or has lost its lease refuses the request before
`queued` with a top-level `error{internal}`, and the client falls back to
an in-process run. A gate killed by a signal Sluice did not send is `fail`
with `exit_code: null` (`<gate> was terminated by a signal`).
`killed` is also what every subscriber of a queued job receives when the
daemon stops (forced or not) or when the job's last subscriber left under
the `cancel` policy. For a coalesced subscriber `done` carries
`coalesced_with = <job_id>`. While a request is queued, `progress` is sent
per subscriber with its `position`; while it runs, `progress` is broadcast
with `elapsed_ms` and `log_bytes`. `gate.timeout_ms = 0` means no timeout.
A pass whose request had a `key` but no `tree` is reported with
`not_cached_reason: "the tree is unknown"`.

### `status`

`{"type":"status","id":2}` →

```json
{"type":"status","id":2,"schema":1,
 "daemon":{"instance":"...","pid":1,"started_at":"...","version":"0.1.0","socket":"..."},
 "budget":{"total":8,"used":5,"psi":{"enabled":true,"cpu_some_avg10":12.3,"memory_some_avg10":0.4,"blocking":false}},
 "running":[{"job_id":"...","repo":"/abs","gate":"test","weight":4,"started_at":"...","elapsed_ms":1200,"subscribers":2,"caller":"agent"}],
 "queued":[{"job_id":"...","repo":"/abs","gate":"lint","weight":1,"queued_at":"...","waiting_ms":300,"subscribers":1,"reason":"budget|psi|repo-cap|head-reserved"}],
 "totals":{"since":"...","runs":120,"passes":110,"failures":8,"timeouts":2,"hits":430,"coalesced":37,
           "wait_ms_total":..., "run_ms_total":..., "time_saved_ms":..., "psi_blocked_ms_total":...}}
```

`status` has an optional `queue` object, omitted when no extension provides
one (so the frozen fixture `status-schema-1.json` is unchanged):
`"queue":{"active":3,"ready":1,"needs_reconcile":0}` (`QueueSummary`;
`active` = jobs not `landed`/`cancelled`/`conflict`/`failed`, `ready` and
`needs_reconcile` = trains in that state). The daemon fills it from the
first extension whose `status_summary()` returns one.

`schema` is bumped when a field changes meaning; Phase 4 freezes it and
adds a test. `sluice status --json` prints this object (pretty-printed,
without the `type`/`id` envelope); with no daemon running it prints
`{"schema":1,"daemon":null}`, so `daemon` is nullable in the CLI output
and every other field is absent in that case. Queued `reason` precedence: `psi` for every queued job while PSI blocks,
else `budget` when the job does not fit the budget, then `repo-cap`, then
`head-reserved` for jobs behind a reserved head. `budget.psi.enabled` is
false both when PSI is disabled in the config and when the kernel offers
none; `totals.psi_blocked_ms_total` is per daemon instance.
`totals` are all-time over the database (`since` = oldest job or event);
`runs` counts every finished job except `orphaned` (so `cached` and
`error` outcomes count), `hits` counts only client `event`s, coalescing is
credited as `coalesced × run time` on passed jobs, and `wait_ms_total`
includes jobs still running.

### `logs`

`{"type":"logs","id":3,"job_id":"<id or null>","tail":40}` →
`{"type":"log","id":3,"job_id":"...","path":"...","lines":[...]}`. With
`job_id: null` the newest job is used; `tail: 0` means the daemon's default
(`cache.tail_lines`). The CLI uses the reply mainly to learn the path and
reads the file itself; on any failure (no daemon, `not-found` for a job
the daemon never saw, such as an in-process run) it falls back to the
newest file in `<state>/logs/`. `sluice logs` never autostarts the daemon.

### `event`

Best-effort telemetry from clients that did not need the daemon:
`{"type":"event","id":0,"event":"hit","repo":"/abs","gate":"test","saved_ms":32100}`
(`saved_ms` = the marker's `duration_ms`). Sent only when the socket file
exists (a hit never starts a daemon); the client writes `hello` and the
event, waits for `welcome` within the same 20 ms budget and closes; the
daemon sends no reply; any failure is ignored. It feeds `totals.hits` and
`time_saved_ms`.

### `stop`

`{"type":"stop","id":4,"force":false}` → `{"type":"stopping","id":4,"running":2}`.
Without `force` the daemon stops accepting work, drops queued jobs
(`done{killed}` to their subscribers), lets running jobs finish and exits;
with `force` it also kills running jobs (their subscribers get `done` with
`status:"killed"`). A SIGTERM/SIGINT to the daemon behaves like
`stop --force`.

### `cancel`

`{"type":"cancel","id":1}` on a `run` connection detaches that subscriber;
the daemon then closes its side. Equivalent to closing the connection
(what the CLI does; the MCP server sends `cancel`). Any other message on a
`run` connection is answered with `error{bad-request}` and ignored.

### Merge queue (`queue-*`)

Single-reply requests served by a daemon extension (the merge queue,
[[sluice/design/merge-queue.md]] sections 8 and 9). `id` is optional on
these messages (`#[serde(default)]`, 0 when absent) and echoed in the reply.
Timestamps in the views are RFC 3339; paths are absolute.

| Client request | Fields | Reply |
|---|---|---|
| `queue-enqueue` | `id`, `repo` (git common dir), `worktree`, `branch`, `head_sha`, `base` (as typed, optional), `caller` (`agent`/`human`), `session` (optional), `env` (map, default `{}`) | `queue-job` |
| `queue-status` | `id`, `repo` (common dir, optional = all) | `queue-status` |
| `queue-approve` | `id`, `train_id`, `train_hash`, `via` (`cli`/`mcp`), `env` (map, default `{}`) | `queue-approved` |
| `queue-retry` | `id`, `job_id`, `head_sha` | `queue-job` |
| `queue-cancel` | `id`, `job_id` | `queue-job` |
| `queue-reconcile` | `id`, `repo` (common dir, optional = all) | `queue-reconcile` |

Server replies:

- `queue-job {id, job}`; `queue-approved {id, train}`;
  `queue-reconcile {id, results: [{train_id, decision, reason?}]}` with
  `decision` one of `landed`, `ready`, `queued`, `cancelled`, `refused`.
- `queue-status {id, schema: 1, trains: [...], jobs: [...]}` (the report is
  flattened like `status`; `QUEUE_STATUS_SCHEMA_VERSION = 1`).
- `job` (`QueueJobView`): `job_id`, `repo`, `worktree`, `branch`,
  `head_sha`, `remote` (`""` for a local target), `target`, `state`,
  `train_id?`, `position?` (1-based among the target's queued jobs),
  `caller`, `session?`, `reason?`, `detail?` (any JSON), `cancel_requested`,
  `attempts`, `enqueued_at`, `updated_at`, `finished_at?`. Job `state` is
  snake_case: `queued`, `merging`, `verifying`, `ready`, `approved`,
  `pushing`, `landed`, `conflict`, `failed`, `cancelled`, `needs_reconcile`.
- `train` (`QueueTrainView`): `train_id`, `repo`, `remote`, `target`,
  `base_sha`, `state` (`building`, `verifying`, `ready`, `approved`,
  `pushing`, `landed`, `dissolved`, `needs_reconcile`), `members` (job ids
  in merge order), `merge_sha?`, `gates: [{gate, key?, job_id?, status,
  duration_ms?}]` (`status`: `pending`, `running`, `pass`, `cached`,
  `fail`, `timeout`, `killed`, `error`), `train_hash?`, `approved_via?`,
  `approved_at?`, `created_at`, `updated_at`, `landed_at?`, `note?`.
  Fields marked `?` are omitted when absent.
- Unknown job/train states and reconcile decisions from a newer daemon
  parse as `Unknown` (`#[serde(other)]`).

A daemon without an extension that handles the request answers
`{"type":"error","id":N,"code":"not-supported","message":"the running daemon does not support the merge queue; run 'sluice daemon stop' and retry"}`.
When the extension is configured but its start-up failed, the answer is
`error{internal}` with `the merge queue failed to start: <cause>`. The
blocking client's `Client::request(&msg)` returns the single reply as is,
an `error` included, so the caller can map its code.

## Disconnects

A job keeps running while it has at least one subscriber. When the last
subscriber disconnects, `daemon.last_subscriber_gone` decides: `cancel`
(default, kill the process group, record `killed`) or `finish` (run to
completion and write the marker so the next request hits).

## Errors

`{"type":"error","id":N,"code":"protocol|bad-request|not-found|internal|conflict-state|not-allowed|stale-plan|duplicate-branch|not-supported","message":"..."}`.
The last five are only sent for `queue-*` requests: `conflict-state` (the
job or train is not in a state that allows the request), `not-allowed`
(refused by policy, e.g. agent approval), `stale-plan` (the train hash no
longer matches), `duplicate-branch` (the branch already has an active
job), `not-supported` (no extension serves the request).
Unknown `type`, malformed JSON, a line over 1 MiB or invalid UTF-8 →
`bad-request` (`id` 0 when unknown); a first line that is not `hello` →
`protocol` (`expected hello`), a second `hello` → `duplicate hello`; a
`hello` with another protocol → `protocol`; a `run` while the daemon is
stopping or has lost its lease → `internal`; `logs` for an unknown job or a
job without a log yet → `not-found`. Both the `hello` and the request line
must arrive within 10 s or the connection is closed. The server never
panics on malformed input.

## Versioning rules

- `protocol` is an integer in the handshake; any incompatible change bumps it.
- New optional fields may be added to any message within a version; clients
  and server ignore unknown fields (`#[serde(default)]`, no
  `deny_unknown_fields` on the wire types).
- The merge-queue additions (`queue-*` messages, `welcome.features`,
  `status.queue`, the new error codes) keep `protocol` at 1: old clients
  never send `queue-*`, and the new fields are optional and omitted when
  empty.
- `sluice-gate-proto` exposes `PROTOCOL_VERSION` and the typed enums
  `ClientMessage` / `ServerMessage`, each `#[serde(tag = "type", rename_all = "kebab-case")]`.
