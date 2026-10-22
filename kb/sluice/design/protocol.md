---
title: Daemon protocol
tags: [sluice, design, protocol, phase-2]
---

# Daemon ↔ client protocol (v1)

Status: design for Phase 2, to be frozen when Phase 2 closes. Types live in
`sluice-gate-proto` (serde, no I/O). Context: [[sluice/design/architecture.md]]
section 5, [[sluice/design/scheduler.md]], [[sluice/design/executor.md]].

## Transport

- Unix stream socket at `<runtime-dir>/daemon.sock`
  (`$XDG_RUNTIME_DIR/sluice/daemon.sock`, fallback `/tmp/sluice-<uid>/daemon.sock`),
  directory mode `0700`. `SLUICE_SOCKET` overrides the path.
- Newline-delimited JSON: one JSON object per line, UTF-8, no pretty
  printing. Lines longer than 1 MiB are rejected.
- The first line in each direction is the handshake. One connection serves
  one request: `run`, `status`, `logs`, `stop` or `event`. A `run`
  connection stays open until the job finishes or either side closes it.
- Closing a `run` connection unsubscribes the client (see "Disconnects").

## Handshake

Client → `{"type":"hello","protocol":1,"client":"sluice-gate 0.1.0","pid":4242}`
Server → `{"type":"welcome","protocol":1,"daemon":"sluice-gate 0.1.0","instance":"<uuid>","pid":1234,"started_at":"<rfc3339>"}`

A daemon that does not speak the client's `protocol` answers
`{"type":"error","code":"protocol","message":"..."}` and closes. A client
that receives a `welcome` with a different `protocol` reports the mismatch
and tells the user to run `sluice daemon stop` (the next client restarts a
matching daemon).

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
| `queued` | `id`, `job_id`, `position`, `coalesced` (bool), `weight` | always first; `coalesced=true` when attached to an existing job |
| `started` | `id`, `job_id`, `started_at` | when the gate process starts |
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
`error` carries a `message` and means Sluice itself failed (cannot spawn,
cannot write the log); the client exits 1.

### `status`

`{"type":"status","id":2}` →

```json
{"type":"status","id":2,"schema":1,
 "daemon":{"instance":"...","pid":1,"started_at":"...","version":"0.1.0","socket":"..."},
 "budget":{"total":8,"used":5,"psi":{"enabled":true,"cpu_some_avg10":12.3,"memory_some_avg10":0.4,"blocking":false}},
 "running":[{"job_id":"...","repo":"/abs","gate":"test","weight":4,"started_at":"...","elapsed_ms":1200,"subscribers":2,"caller":"agent"}],
 "queued":[{"job_id":"...","repo":"/abs","gate":"lint","weight":1,"queued_at":"...","waiting_ms":300,"subscribers":1,"reason":"budget|psi|repo-cap|head-reserved"}],
 "totals":{"since":"...","runs":120,"passes":110,"failures":8,"timeouts":2,"hits":430,"coalesced":37,
           "wait_ms_total":..., "run_ms_total":..., "time_saved_ms":...}}
```

`schema` is bumped when a field changes meaning; Phase 4 freezes it and
adds a test. `sluice status --json` prints this object verbatim.

### `logs`

`{"type":"logs","id":3,"job_id":"<id or null>","tail":40}` →
`{"type":"log","id":3,"job_id":"...","path":"...","lines":[...]}`. With
`job_id: null` the newest job is used. When the daemon is not running the
client falls back to reading `<state>/logs/` directly (Phase 1 behaviour).

### `event`

Best-effort, fire-and-forget telemetry from clients that did not need the
daemon: `{"type":"event","id":0,"event":"hit","repo":"/abs","gate":"test","saved_ms":32100}`.
The client connects with a 20 ms timeout, writes one line and closes; any
failure is ignored. It feeds `totals.hits` and `time_saved_ms`.

### `stop`

`{"type":"stop","id":4,"force":false}` → `{"type":"stopping","id":4,"running":2}`.
Without `force` the daemon stops accepting work, lets running jobs finish
and exits; with `force` it kills running jobs (their subscribers get
`done` with `status:"killed"`).

### `cancel`

`{"type":"cancel","id":1}` on a `run` connection detaches that subscriber
without closing the socket (used by the MCP server when the model cancels a
tool call). Equivalent to closing the connection.

## Disconnects

A job keeps running while it has at least one subscriber. When the last
subscriber disconnects, `daemon.last_subscriber_gone` decides: `cancel`
(default, kill the process group, record `killed`) or `finish` (run to
completion and write the marker so the next request hits).

## Errors

`{"type":"error","id":N,"code":"protocol|bad-request|not-found|internal","message":"..."}`.
Unknown `type` → `bad-request`. The server never panics on malformed input.

## Versioning rules

- `protocol` is an integer in the handshake; any incompatible change bumps it.
- New optional fields may be added to any message within a version; clients
  and server ignore unknown fields (`#[serde(default)]`, no
  `deny_unknown_fields` on the wire types).
- `sluice-gate-proto` exposes `PROTOCOL_VERSION` and the typed enums
  `ClientMessage` / `ServerMessage`, each `#[serde(tag = "type", rename_all = "kebab-case")]`.
