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
- Length limit and short fallback (`sluice_gate_core::dirs::choose_socket`):
  a socket path must fit in `sockaddr_un.sun_path`, computed as
  `SUN_PATH_LEN` = 108 bytes on Linux including the NUL, so at most 107
  bytes (`SOCKET_PATH_MAX`). Selection, the same pure function of the
  environment and the uid in the daemon and every client:
  1. `SLUICE_SOCKET` when set. An over-long value is an error
     (`unusable daemon socket path …: SLUICE_SOCKET is N bytes, longer
     than the 107-byte Unix socket limit`); `sluice run` exits 1 instead of
     running in-process, so the queue is never lost silently.
  2. `<runtime-dir>/daemon.sock` when it fits.
  3. Otherwise `/tmp/sluice-<uid>/daemon.sock`. Its directory is created
     `0700` (last component only) and refused, with an error naming the
     directory and the problem, when it is a symlink, not a directory, not
     owned by the uid, or group/world writable; Sluice never chmods or
     reuses such a directory. The locks stay in the (long) runtime
     directory, which `flock` does not limit.
  No abstract sockets: they bypass file permissions. The daemon logs
  `socket path fallback in use: <preferred> is N bytes, …` at start;
  `sluice status` prints `socket:   fallback in use (…)`; `sluice doctor`
  has a `socket-path` check (ok / fallback in use with the reason / fail
  when too long or the fallback directory is unsafe).
  `SLUICE_TEST_SOCKET_FALLBACK_DIR` replaces the `/tmp` base of the
  fallback; it is test-only (the suite uses it so it never touches the
  user's real `/tmp/sluice-<uid>`), not a supported setting.
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

`welcome` has an optional `features` array of strings, omitted when empty.
From 0.3.0 every daemon announces `"exec"` (`FEATURE_EXEC`: serves `exec`
and `signal`) and `"queue-only"` (`FEATURE_QUEUE_ONLY`: honours
`run.no_marker`), followed by the features of its extensions; a daemon
running the merge-queue extension adds `"queue"`
(`sluice_gate_proto::FEATURE_QUEUE`), so it sends
`"features":["exec","queue-only","queue"]`. The blocking client exposes
them as `Client::features()` / `Client::supports(..)`. A `welcome` without
`features` (a v0.2 daemon) parses as an empty list; clients send `exec` and
`no_marker` only to a daemon listing the matching feature.

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

Optional `"no_marker": true` (omitted when false; only sent to a daemon
announcing `queue-only`) marks a queue-only gate: the daemon skips the
marker double-check before the start and never writes a marker or a miss
record. The client then sends `key = sha256("queue-only\0" K)` (K the
gate's normal key on the tree), a namespace no marker can match, so
simultaneous identical requests still coalesce. To a daemon without
`queue-only` the client sends `key: null` (runs, never coalesces, never
cached). Every gate process the daemon starts carries
`SLUICE_JOB=<job id>` in its environment.

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
`outcome` may carry an optional `signal` (number of the signal that killed
the process; `exec` jobs only, omitted when absent).
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
 "daemon":{"instance":"...","pid":1,"started_at":"...","version":"0.1.0","socket":"...","log_level":"info"},
 "budget":{"total":8,"used":5,"psi":{"enabled":true,"cpu_some_avg10":12.3,"memory_some_avg10":0.4,"blocking":false}},
 "running":[{"job_id":"...","repo":"/abs","gate":"test","weight":4,"started_at":"...","elapsed_ms":1200,"subscribers":2,"caller":"agent"}],
 "queued":[{"job_id":"...","repo":"/abs","gate":"lint","weight":1,"queued_at":"...","waiting_ms":300,"subscribers":1,"reason":"budget|psi|repo-cap|head-reserved"}],
 "totals":{"since":"...","runs":120,"passes":110,"failures":8,"timeouts":2,"hits":430,"coalesced":37,
           "wait_ms_total":..., "run_ms_total":..., "time_saved_ms":..., "psi_blocked_ms_total":...}}
```

`daemon.log_level` is an optional string (the daemon's active log filter,
for example `info` or `debug`), omitted when the daemon does not report it,
so the frozen fixture is unchanged and the protocol stays at version 1; a
second additive test (`log_level_is_an_additive_optional_field`) pins its
shape. `sluice status` prints it as `log:      level <level>, <path>/daemon.log`.

Entries of `running` and `queued` have optional `kind` and `label`, omitted
for gates: an exec job shows `"kind":"exec","label":"cargo build"` and
`"gate":"exec:cargo build"` (so a v0.2 client prints something meaningful).
The fixture is unchanged; `exec_kind_and_label_are_additive_optional_fields`
pins their shape.

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

### `exec` (`sluice exec`, from 0.3.0)

Runs an arbitrary command under the budget, without cache
([[sluice/design/command-control.md]] section 2). Only sent to a daemon
announcing `"exec"`; an older daemon answers `error{bad-request}` (unknown
type) before `queued` and the client runs the command in-process.

```json
{"type":"exec","id":1,"argv":["/usr/bin/cargo","build","--release"],
 "cwd":"/abs/worktree","env":{"PATH":"...","HOME":"...","CLAUDECODE":"1"},
 "label":"cargo build","weight":4,"timeout_ms":0,"coalesce":null,
 "repo":"/abs/main-worktree|null","caller":"agent","session":"<id|null>","client_pid":4242}
```

`argv[0]` is resolved by the client (absolute path, shims skipped); no
shell is involved. `env` is the caller's environment, verbatim, plus
`SLUICE_JOB=<job id>` added by the daemon; `cwd` is the caller's. `repo`
(main worktree or common dir) feeds the per-repository cap; `null` uses
`cwd`. `weight` is clamped like a gate's. `timeout_ms = 0` means none.

Events are those of `run` (`queued`, `started`, `progress`, `done`).
`started` carries an optional `spool`: the absolute path of the output
spool `<logs>/<job>.exec`, which the client replays to its stdout and
stderr. Spool format: the magic `SLUICEX1\n`, then records `[stream: u8
(1 = stdout, 2 = stderr)][len: u32 big-endian][bytes]`, appended whole by
the two output pumps under one mutex in chunks of up to 8 KiB. `done` is
sent only after both pumps finished, so the client reads to EOF after it.
`done.outcome`: `pass` (exit 0), `fail` with `exit_code` (non-zero exit) or
with `signal` and `exit_code: null` (killed by a signal), `timeout`,
`killed` (daemon stop, lost lease, cancel), `error`. `marker_written` is
always false; `tail` and `logs` decode the spool (streams merged).

Coalescing only with `coalesce: K`: requests coalesce when the SHA-256 of
`"sluice-exec-v1\0" K "\0" cwd "\0" argv joined by NUL` is equal (the
environment is ignored). A late subscriber gets `started` (with `spool`)
right after `queued` and replays from offset 0.

An exec job ignores `last_subscriber_gone = "finish"`: when its last
subscriber leaves it is killed (nothing to cache).

### `signal`

`{"type":"signal","id":1,"signal":"INT|TERM|HUP"}` is valid on an `exec`
connection only (elsewhere `error{bad-request}`, `signal outside an exec
connection`). When the job runs and this connection is its only
subscriber, the daemon sends the signal to the job's process group; once
the leader is gone it SIGKILLs what is left of the group (background
children that ignore SIGINT and hold the output pipes); if the leader is
still alive `KILL_GRACE` + 5 s later, the cancel sequence terminates the
group. Otherwise (queued, or other subscribers) the subscriber is detached
and the connection closed.

### Merge queue (`queue-*`)

Single-reply requests served by a daemon extension (the merge queue,
[[sluice/design/merge-queue.md]] sections 8 and 9). `id` is optional on
these messages (`#[serde(default)]`, 0 when absent) and echoed in the reply.
Timestamps in the views are RFC 3339; paths are absolute.

| Client request | Fields | Reply |
|---|---|---|
| `queue-enqueue` | `id`, `repo` (git common dir), `worktree`, `branch`, `head_sha`, `base` (as typed, optional), `caller` (`agent`/`human`), `session` (optional), `env` (map, default `{}`), `via` (`cli`/`mcp`, optional) | `queue-job` |
| `queue-status` | `id`, `repo` (common dir, optional = all) | `queue-status` |
| `queue-approve` | `id`, `train_id`, `train_hash`, `via` (`cli`/`mcp`), `env` (map, default `{}`) | `queue-approved` |
| `queue-retry` | `id`, `job_id`, `head_sha`, `via` (`cli`/`mcp`, optional) | `queue-job` |
| `queue-cancel` | `id`, `job_id`, `via` (`cli`/`mcp`, optional) | `queue-job` |
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
  `approved_at?`, `created_at`, `updated_at`, `landed_at?`, `note?`,
  `push_timeout_ms?`, `network_timeout_ms?` (the base's `[queue]` limits
  stored with the train; clients bound their wait for a landing with them).
  Fields marked `?` are omitted when absent.
- The optional `via` of `queue-enqueue`, `queue-retry` and `queue-cancel`
  (same spelling as `queue-approve`'s required `via`) names the surface for
  the audit trail (`queue_events.actor`): `mcp` from the MCP tools; absent
  (older clients) or `cli` is recorded as `cli`. It is omitted when absent.
- Unknown job/train states and reconcile decisions from a newer daemon
  parse as `Unknown` (`#[serde(other)]`).

A daemon without an extension that handles the request answers
`{"type":"error","id":N,"code":"not-supported","message":"the running daemon does not support the merge queue; run 'sluice daemon stop' and retry"}`.
When the user config sets `[queue] enabled = false` the extension answers
`error{not-supported}` with `the merge queue is disabled in the user config
([queue] enabled = false)`. When the extension is configured but its
start-up failed, the answer is `error{internal}` with `the merge queue
failed to start: <cause>`. The
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
(refused by policy: an MCP approval while `[queue] allow_agent_approval` is
off, or an enqueue whose target the policy forbids), `stale-plan` (the
train hash no longer matches), `duplicate-branch` (the branch already has
an active job), `not-supported` (no extension serves the request, or the
queue is disabled by `[queue] enabled = false`). `queue-*` requests also
use `bad-request` (not a git common dir, no branch, not a commit, invalid
base or remote), `not-found` (unknown job or train) and `internal` (lease
lost, store failure, queue failed to start).
Unknown `type`, malformed JSON, a line over 1 MiB or invalid UTF-8 →
`bad-request` (`id` 0 when unknown); a `cancel` outside a `run` connection
→ `bad-request` (`cancel outside a run connection`); a `signal` outside an
`exec` connection → `bad-request`; an `exec` with an empty `argv` →
`bad-request`; a first line that is not `hello` →
`protocol` (`expected hello`), a malformed first line → `bad-request`
(`malformed hello: ...`), a second `hello` → `protocol` (`duplicate hello`); a
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
  `status.queue`, the new error codes, the optional `via` of
  `queue-enqueue`/`queue-retry`/`queue-cancel` and the optional train
  timeouts) keep `protocol` at 1: old clients
  never send `queue-*`, and the new fields are optional and omitted when
  empty.
- The command-control additions of 0.3.0 (`exec`, `signal`, the welcome
  features `exec` and `queue-only`, `started.spool`, `outcome.signal`,
  `run.no_marker`, `status` entries' `kind` and `label`) keep `protocol` at
  1 and `status` schema at 1: all optional, omitted when unset; old daemons
  are detected through `welcome.features`.
- `sluice-gate-proto` exposes `PROTOCOL_VERSION` and the typed enums
  `ClientMessage` / `ServerMessage`, each `#[serde(tag = "type", rename_all = "kebab-case")]`.
