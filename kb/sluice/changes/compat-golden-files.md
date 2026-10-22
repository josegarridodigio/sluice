---
title: "Change: compatibility golden files (H3)"
tags: [sluice, change, alpha, compatibility, h3]
---

# Change: compatibility golden files (task H3, first half)

Plan: [[sluice/plan/alpha-plan.md]] task H3. Design:
[[sluice/design/compatibility.md]] sections 3, 4 and 6. Decision:
[[sluice/design/adr/0013-compatibility-promise.md]]. User-facing text:
`docs/compatibility.md`.

Adds fixtures recorded from the real v0.1.0 and v0.2.0 release binaries, the
scripts that record them, and new test files that read them with the current
code. No product code under `crates/*/src/` changed. The promises that need
code changes are written as `#[ignore = "gap Gn: fixed by task H3b"]` tests:
H3b removes the attribute after fixing the code.

## Files

- `scripts/compat/generate.sh`: downloads each release with `gh release
  download vX -R josegarridodigio/sluice -p '*x86_64*' -p SHA256SUMS`, verifies
  `SHA256SUMS`, runs the binary in a private HOME/XDG tree and a short runtime
  dir (`/tmp/sa-compat.XXXXXX`, Unix socket paths are limited to 107 bytes) with
  a clean environment (`env -i`, `SLUICE_AGENT=0`: the Claude Code variables must
  not turn the runs into agent runs), writes the fixtures and a `README.md` plus
  `SHA256SUMS` in each fixture directory.
- `scripts/compat/record-proxy.py`: transparent Unix-socket proxy that records
  the newline-JSON protocol, both directions, as `{"conn","dir","msg"}` lines
  (`conn` numbers the connections of one recording because a client opens
  several, for example the `status` poll of `sluice run`).
- `scripts/compat/tools.py`: helpers (raw `hello`, MCP `tools/list` over stdio,
  recursive `--help` capture, transcript normalisation, expected values, inputs,
  damaged markers, WAL checkpoint). All run with `python3 -I`.
- Tests (new files only): `sluice-gate-core/tests/compat.rs`,
  `sluice-gate-proto/tests/{compat.rs,compat_client.rs,queue_status_schema.rs}`,
  `sluice-gate-daemon/tests/compat.rs`, `sluice-gate-queue/tests/compat.rs`,
  `sluice-gate/tests/compat.rs`.
- `docs/compatibility.md` and this note.

## How the fixtures were generated

```sh
scripts/compat/generate.sh            # v0.1.0 and v0.2.0, needs gh, git, python3 >= 3.11
COMPAT_DOWNLOADS=<dir> scripts/compat/generate.sh v0.2.0   # reuse downloaded vX/ dirs
```

Per release `$v` (what the script does, in order):

1. `gh release download $v -R josegarridodigio/sluice -p '*x86_64*' -p
   SHA256SUMS`, `sha256sum -c --ignore-missing SHA256SUMS`, extract.
2. Config fixtures: the repository-config and user-config examples of
   `kb/sluice/design/config-format.md` at that tag (`git show $v:...`, first
   toml block under each heading), `minimal.toml`, `init.toml` (output of
   `sluice init`), `queue.toml` (v0.2.0: the example with `[queue]`; `full.toml`
   is then the same without that table) and `recorded.toml` (the config the
   recordings ran with). Every repository config is checked by the release
   itself (`sluice run __no_such_gate__` must answer `unknown gate`, not
   `invalid configuration`); every user config by starting the release's
   `daemon --foreground` with `SLUICE_CONFIG`.
3. Fixture repo: `git init`, `a.txt` = `alpha\n`, `.sluice.toml` =
   `recorded.toml`, `git add -A`, no commit. Copy in `core/.../repo-files/`.
   Daemon: `<bin> daemon --foreground` with `SLUICE_SOCKET=real.sock`; clients
   use `SLUICE_SOCKET=p-<scenario>.sock`, a proxy to it.
4. Scenarios, each recorded to `transcripts/<name>.ndjson`: `hello-welcome`
   (raw `hello`), `run-pass` (`sluice run test`), `run-hit-event` (the same
   again: a cache hit sends the `event`), `run-fail`, `run-timeout` (1 s timeout,
   `sleep 5`), `run-modified` (`echo x >> a.txt`; `a.txt` is restored after),
   `run-keyed` (gate with env, `key_inputs`, `key_files` and a whitespace-heavy
   `cmd`), `run-slow` (`sleep 6`, so a `progress` line exists) with a
   `status` recorded while it runs (also saved as `status/status.json`),
   `logs` (`sluice logs -n 5`), `logs-job` (`sluice logs -n 3 <job>`), `stop`.
   The pre-commit marker comes from a second repository: committed `a.txt` and
   `b.txt`, hook `.githooks/pre-commit` = `sluice run --hook pre-commit test`,
   `a.txt` staged and `b.txt` changed but unstaged, `git commit`.
5. Merge queue (v0.2.0): bare remote, clone with `.sluice.toml` committed on
   `main` (`[queue] remote = "origin", target = "main", gates = ["test"],
   max_train = 1`), branch `feature` with one commit; `queue-status-empty`
   (`sluice queue status --json`, saved as `queue-status/empty.json`),
   `queue-enqueue` (`sluice queue enqueue feature`), poll until a train is
   `ready`, `queue-status` (saved as `queue-status/populated.json`). Nothing is
   approved or pushed.
6. `sluice daemon stop` (recorded as `stop`), `sluice status --json` with no
   daemon (`status/status-no-daemon.json`), WAL checkpoint (TRUNCATE) of
   `$XDG_STATE_HOME/sluice/sluice.db` and a copy as `db/sluice.db` in the daemon
   and queue fixtures.
7. Markers and miss records copied from `<repo>/.git/sluice/markers/`;
   `inputs.json` (tree, gate, normalised cmd, env, key input hashes, key file
   hashes per marker, computed from the config and the repo by Python,
   independently of the Rust code); `markers-bad/` (three corrupted copies of
   a real marker: format 2, unknown field, key different from the file name).
8. `cli-help.txt`: `--help` of every subcommand, found recursively, one
   `===== sluice <path> =====` section each. `mcp-tools.json`: the `tools/list`
   result after `initialize` over stdio. Both live in the gate crate (it owns
   the binary; the MCP server has no binary of its own).

Output: `crates/<crate>/tests/fixtures/compat/<release>/` for core (configs,
user configs, markers, markers-bad, inputs.json, repo-files), proto (status,
queue-status, transcripts), daemon (db), queue (db, queue-status), gate
(cli-help.txt, mcp-tools.json). v0.1.0: 40 fixture files, v0.2.0: 48 (plus a
`README.md` and `SHA256SUMS` per directory).

## Normalisations (only where a test cannot do it)

- Transcripts only: absolute scratch paths in every string of both
  directions are replaced by placeholders, longest first: the fixture repo by
  `{{REPO}}`, the hook repo by `{{HOOK}}`, the queue clone and remote by
  `{{QUEUE_CLONE}}` and `{{QUEUE_REMOTE}}`, the private HOME by `{{HOME}}`, the
  runtime dir by `{{RUN}}`, the scratch root by `{{SCRATCH}}`. The replay test
  needs this because the scratch dir name is random; it substitutes a real
  temporary repository.
- Nothing else is normalised: markers, miss records, status JSON, queue status
  JSON, the database, help text and tool list are verbatim (they contain the
  scratch paths and times of the recording; the tests compare types and
  stable fields only). Pids, job ids and timestamps stay in the transcripts;
  the replay test ignores them.
- `expected.json` files are not recordings: computed from the TOML text by
  `tools.py` with the documented defaults, so the Rust parser is compared with
  an independent reading.

## Tests

Passing today (49): core 11, proto 19 (`compat.rs` 9, `compat_client.rs` 6,
`queue_status_schema.rs` 4), daemon 6, queue 5, gate 8.

- core: configs and user configs parse and equal `expected.json` (G2);
  markers (plain, with `committed_tree`) are hits under a century TTL; keys
  rederived from `inputs.json` equal the file names (cache layout frozen);
  miss records read as `Lookup::Miss(Some(..))` with reason and exit code;
  format-2 and wrong-key markers are deleted and miss; `prune` keeps fresh
  markers; every compat `SHA256SUMS` of every crate still matches its files.
- proto: `status.json` and queue status JSON parse and the current output is a
  superset (every old path, same type); frozen `{"schema":1,"daemon":null}`;
  queue status round-trips exactly and the state spellings are frozen (G8);
  every transcript line decodes; features per release; run outcome sequences;
  the current blocking `Client` against a fake daemon replaying the recorded
  server half (hello, run pass/fail/timeout, status, logs, stop, queue status
  and enqueue; `supports("queue")` false for v0.1.0, true for v0.2.0).
- daemon: the fixture repository has the recorded tree hash; every recorded
  old-client connection is replayed against a current daemon and answers with
  the recorded type sequence and stable fields (`done.status`, exit code,
  schema, protocol); `stop` is honoured; an unknown request `type` gets
  `bad-request` and the daemon keeps serving (G6: the behaviour already
  exists, the test pins it); the old databases open (read-only and read-write)
  with history, and a daemon runs on them.
- queue: queue tables are added to the v0.1.0 database, the v0.2.0 database
  keeps its job and train, a queue daemon serves both, and the recorded v0.2.0
  `queue-status` and `queue-enqueue` requests are served by the current queue.
- gate: the CLI surface (commands, options, positionals, possible values of
  every `--help`) of both releases is a subset of the current one (G12); a
  table of exit codes 0, 7, 1, 2, 3, 124, 143, 75 plus 4 through a duplicate
  enqueue (G11, behaviour part); MCP tool names and input and output schemas
  are additive (G13); `plugin.json` version equals the workspace version, plugin
  contract pinned (G14: the check did not exist, the versions agree today).

Ignored pending H3b (each fails today, verified with `--ignored`):

| Test | Gap | What the code must do |
|---|---|---|
| core `a_marker_with_an_unknown_field_is_still_a_hit` | G4a | drop `deny_unknown_fields` on `Marker` and `MissRecord` |
| core `prune_removes_orphaned_miss_records_and_foreign_formats` | G3 | `prune` also removes expired miss records and foreign-format records |
| proto `wire_enums_have_an_unknown_fallback` | G5 | `Unknown` fallback on `OutcomeStatus`, `ErrorCode`, `WaitReason`, `Caller` |
| daemon `a_refused_newer_database_names_the_release_that_wrote_it` | G10 | store the writer release (`meta` table) and print it in `Error::Schema` |
| gate `long_help_documents_every_exit_code` | G11 | document exit codes 4 and 75 in `sluice --help` |

G1, G7 and G9 have no test here (G1 and G7 are design decisions, G9 a
migration framework for H3b).

## Verification

`sluice run fmt`, `sluice run clippy`, `sluice run test` from the worktree
root (output lines quoted in the task report). Every new test file also ran on
its own while it was written (`cargo test -p <crate> --test compat`).

## Doubts and notes

- The design assigned `mcp/tests/compat.rs`; the MCP server has no binary and
  its router is private, so the test spawns `sluice mcp` from the gate crate and
  the fixture is `sluice-gate/tests/fixtures/compat/<release>/mcp-tools.json`.
- The daemon and queue tests read transcripts and `repo-files` from the proto
  and core fixture directories (`../sluice-gate-proto/...`): fine inside the
  workspace; if the crates are packaged separately those tests would need the
  fixtures copied.
- Transcripts contain the clean child environment of the recorded run (git
  author `Compat <compat@example.invalid>`, scratch PATH); no real user data.
- The fixtures were recorded on x86_64 glibc hosts with the musl release
  binaries; nothing in them depends on the host except scratch paths.

## Gaps closed

The five ignored tests were un-ignored (fixtures untouched) by task H3b:

- G4a: `Marker` and `MissRecord` no longer use `deny_unknown_fields`
  (`sluice-gate-core/src/marker.rs`); a comment records that 0.3.0 still must
  not write new fields.
- G3: `MarkerStore::prune` now covers miss records and foreign-format files,
  by mtime age.
- G5: `Unknown` (`#[serde(other)]`) on `Caller`, `OutcomeStatus`, `ErrorCode`,
  `WaitReason` (`sluice-gate-proto/src/lib.rs`, round-trip unit test). Match
  sites: daemon `server.rs` (labels), queue `store.rs`/`runner.rs`, mcp
  `lib.rs`/`text.rs`, CLI `run.rs`/`exec.rs`/`status.rs`. An unknown outcome is
  reported as an error and never as a pass. `ApproveVia` and `ExecSignal` flow
  client to daemon only and stay strict.
- G10: new `sluice-gate-daemon/src/meta.rs` (`meta(key, value)` table, key
  `writer_version`, written after a successful `Store::open`, one line in
  `store.rs`); `Error::Schema` gained a `writer` field and its message names it
  (`sluice 0.2.0 or older` when the row is absent) and the recovery.
- G11: `sluice --help` lists every exit code (0, N, 1, 2, 3, 4, 75, 124, 125,
  141, 143); unit test `exit_code_tests` in `sluice-gate/src/main.rs` pins the
  constants and the help text.
