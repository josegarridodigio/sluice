---
title: Compatibility promise
tags: [sluice, design, compatibility, alpha, h3]
---

# Compatibility promise (ADR-0013 and `docs/compatibility.md`)

Status: accepted as [[sluice/design/adr/0013-compatibility-promise.md]]
(task H3 of [[sluice/plan/alpha-plan]]), written against the v0.2.0 code and
reconciled with the 0.3.0 code (task R3). **GAP** marks a place where the
v0.2.0 code did not honour the promise; every gap is marked CLOSED with how
it was closed, or stated as a documented fact. Facts about behaviour were
checked in the sources (named by file and symbol: line numbers drift).

## 1. The promise (from 0.3.0)

1. Every interface in section 3 keeps working with data and peers produced
   by every released version since its baseline (v0.1.0; the merge queue
   since v0.2.0).
2. Changes are **additive only**: new optional fields, optional messages
   negotiated through `welcome.features`, new subcommands and flags, new
   tables or nullable columns. Nothing is renamed, removed, retyped,
   re-nested or given another meaning.
3. A breaking change needs all three: a new version number for that
   interface, a migration path (automatic where data is involved) and
   **one release of deprecation warnings** first.
4. Direction: a new binary must read everything old binaries wrote.
   Whether an old binary reads new data is stated per interface as a fact
   (several old readers are strict), not promised.
5. Not covered: log text, `doctor` output, human `status` text,
   `client.log`/`daemon.log` lines, error wording.

Golden files (section 6) are the proof: a change to a frozen format fails
a compat test until a human changes a fixture deliberately.

## 2. Version markers today

| Interface | Marker | Value | Where |
|---|---|---|---|
| `.sluice.toml` | `version` key | 1 | `config.rs` `SUPPORTED_VERSION` |
| user `config.toml` | `version` key (optional) | 1 | `config.rs` `SUPPORTED_VERSION`, `UserConfig::parse` |
| Markers, miss records | `format` field | 1 | `core/lib.rs` `CACHE_FORMAT_VERSION` |
| Cache key | prefix `sluice-cache-v1` | v1 | `key.rs` `KEY_PREFIX` |
| Daemon protocol | `hello/welcome.protocol` | 1 | `proto/lib.rs` `PROTOCOL_VERSION` |
| Features | `welcome.features` | `["exec", "queue-only", "queue"]` (0.2.0: `["queue"]`, 0.1.0: none) | `proto/lib.rs` `FEATURE_*`, `daemon/server.rs` |
| Status JSON | `schema` | 1 | `proto/lib.rs` `STATUS_SCHEMA_VERSION` |
| Queue status JSON | `schema` | 1 | `proto/lib.rs` `QUEUE_STATUS_SCHEMA_VERSION` |
| Daemon DB | table `schema(version)` | 1 | `daemon/store.rs` `SCHEMA_VERSION` |
| Daemon DB additions | table `sched_schema(version)` (0.3.0) | 1 | `daemon/migrate.rs` `SCHED_SCHEMA_VERSION` |
| Queue tables | table `queue_schema(version)` | 1 | `queue/store.rs` `QUEUE_SCHEMA_VERSION` |
| Exit codes, CLI flags, MCP tools | none (absent) | n/a | `--help`, `mcp/src/lib.rs` |
| Plugin | `plugin.json` `version` (tracks the release) | 0.2.0 until the release bump | `plugin/.claude-plugin/` |

## 3. Interface inventory

### 3.1 Repository config `.sluice.toml`

- Marker `version = 1`, required; other values fail with
  `unsupported version N: `version` must be 1 (supported versions: 1)`
  (`config.rs` `check_version`). Written by humans and
  `sluice init`; read by every binary (the daemon reads `[queue]` with
  `git show <base>:.sluice.toml`).
- Parsing is strict everywhere (`deny_unknown_fields` on `RepoConfig`,
  `Defaults`, `Gate`, `QueueConfig` and every user-config table). Fact: an
  **older binary rejects a config that uses a newer optional field** (v0.1.0
  rejects `[queue]`; v0.2.0 rejects the 0.3.0 gate keys `mode`, `paths` and
  `paths_ignore`, and the user keys of `[agent]`, `[scheduler]`, `[mcp]`,
  `[debug]` and the `[admission]` overcommit keys). Stated, not changed;
  mixed-version repos avoid new keys.
- Compatible: new optional key/table whose default keeps today's behaviour;
  looser validation. Incompatible: rename, retype, new required key, tighter
  validation that fails a previously valid file, changed default, any change
  to cache-key inputs (3.3).
- Promise: every file valid at v0.1.0/v0.2.0 yields the same gates and
  therefore the same keys in every later 0.x. Another `version` is a new
  format with migration note and a warning release.
- **GAP G1 (closed: `gates.<name>.mode`)**: `gates.<name>.cache` is a bool;
  the plan floated `cache = "queue-only"`. Retyping a key is not additive:
  0.3.0 adds the new optional key `mode` (`"gate"` | `"queue-only"`) and
  keeps `cache` a bool (`config.rs` `Gate.mode`, `GateMode`).

### 3.2 User config `~/.config/sluice/config.toml`

- Marker `version` (optional, must be 1, `UserConfig::parse`); path via
  `SLUICE_CONFIG`. Strict parsing like 3.1. Daemon reads sections at start,
  CLI/MCP per call. The `SLUICE_*` variables and the truthy/falsy words of
  config-format.md are part of this interface: no renames, additions allowed.
- Promise: any file valid at v0.1.0/v0.2.0 loads with the same effective
  values; `daemon.idle_exit` stays accepted although inert.
- **GAP G2 (closed)**: fixtures of both releases (`user-config/{empty,full}.toml`
  with an independent `expected.json`) are loaded by
  `core/tests/compat.rs::user_configs_parse`; repository configs by
  `configs_parse` (section 6).

### 3.3 Cache key derivation

- Marker: the first hashed field `sluice-cache-v1`. Unit tests pin the exact
  bytes (`key::tests::golden_vector_matches_byte_layout`) and
  `prefix == "sluice-cache-v{CACHE_FORMAT_VERSION}"`
  (`key::tests::key_prefix_tracks_cache_format_version`). The marker file name is the key, so old and new binaries
  share a cache **only if they derive identical keys**.
- Promise: byte layout, `cmd` normalisation, env sorting, `key_inputs` /
  `key_files` folding and the tree-hash procedure do not change inside v1.
  Gates without the new `paths`/`paths_ignore` options keep the old key
  byte for byte (alpha D1; pinned by
  `lookup::tests::unscoped_key_is_byte_identical_to_v0_2`,
  `core/tests/compat.rs::old_keys_are_derived_identically` and
  `sluice-gate/tests/scoped.rs::markers_written_by_0_2_still_hit`). A scoped
  gate replaces only the tree field by `scope-v1:<hash>` (cache-key.md).
- Prefix change (`sluice-cache-v2`): bump `KEY_PREFIX` and
  `CACHE_FORMAT_VERSION` together (the test forces it). New keys differ, so
  old markers are never read: a lookup misses, the gate runs once, a v2
  marker is written. No migration and no run-time error; the release notes
  must say that the first run after upgrade is a miss.
- **GAP G3 (closed in H3b: `MarkerStore::prune` extended)**: `prune` used to
  delete markers by mtime and skip miss records; after a bump, orphaned
  `*.miss.json` would live forever and orphan markers one TTL. It now removes
  every marker, miss record and foreign-`format` file whose mtime is older
  than the TTL.

### 3.4 Markers and miss records

- Marker `format: 1`; path `<git-common-dir>/sluice/markers/<key>.json` and
  `<key>.miss.json` (public contract, cache-key.md). Written by whichever
  process ran the gate, read by every binary of any worktree and by shell
  snippets. Field sets: marker {format, key, tree, gate, cmd, passed_at,
  duration_ms, exit_code, job_id, sluice_version, committed_tree}; miss
  {format, key, gate, exit_code, reason, job_id, log_path, recorded_at}.
- Reader facts: a marker with another `format`, unparsable JSON or a `key`
  different from its file name is **deleted** and counts as a miss
  (`MarkerStore::lookup`); a bad miss record is ignored, not deleted
  (`read_miss`).
- Fact about released binaries: both structs are `deny_unknown_fields` in
  v0.1.0/v0.2.0, so **any field added to a marker looks corrupt to them and
  they delete the marker** on its next lookup, silently defeating the cache
  whenever a v0.1.0/v0.2.0 binary (long-lived daemon, plugin) shares the
  repo with a newer one. The 0.3.0 structs tolerate unknown fields.
- Promise: `format` stays 1 and the field sets above stay exactly as listed
  for 0.3.0. New data goes in a separate file, or waits for G4a plus a
  release.
- **GAP G4a (closed in H3b: `deny_unknown_fields` dropped on markers and miss records; format still forbids new fields in 0.3.0)**:
  `Marker` and `MissRecord` no longer reject unknown fields (the `format`
  and key checks stay), and cache-key.md says so. Field additions become
  safe one release later.
- **GAP G4b (honoured)**: no 0.3.0 item extended the marker: path scoping
  (D1) derives the scope hash from the stored root `tree` and the committed
  config instead of storing it (ADR-0012).

### 3.5 Daemon protocol (NDJSON over the Unix socket)

- Marker `protocol: 1` in `hello`/`welcome`; `welcome.features` optional
  (omitted when empty): v0.1.0 daemons send none, v0.2.0 with the queue send
  `["queue"]`. A daemon outlives `sluice update`, so **old client with new
  daemon and new client with old daemon both occur**.
- Negotiation: the client sends `hello {protocol, client, pid}`; the daemon
  accepts an equal `protocol` only (`server.rs`, else `error{protocol}`),
  the client checks `welcome` likewise (`client.rs`) and prints the
  `sluice daemon stop` hint. Optional families are gated by features: a
  client sends `queue-*` only when `Client::supports("queue")`; a daemon
  without the extension answers `error{not-supported}`; a v0.1.0 daemon
  does not know the types and answers `error{bad-request}`. A new family =
  a new feature string, `protocol` stays 1; new request/reply types are sent
  only to a peer that announced the feature.
- Compatible: a new optional field on any message (wire types ignore unknown
  fields, `proto/lib.rs` module docs), a new feature with its messages, a new
  `status` field. Incompatible: removing/renaming a field or `type`,
  retyping or re-meaning a field, a new required request field, new
  unconditional reply events.
- Promise: v0.1.0 and v0.2.0 clients work against current daemons (hello,
  run with queued/started/progress/done, status, logs, event, stop, cancel)
  and current clients work against their daemons. A `protocol` bump keeps
  the stop-hint refusal and ships one release in which clients warn about an
  older daemon.
- **GAP G5 (closed in H3b: `Unknown` fallbacks on `Caller`, `OutcomeStatus`, `ErrorCode`, `WaitReason`; unknown outcome is an error, never a pass; `ApproveVia` and `ExecSignal` are client-to-daemon only and stay strict)**:
  in v0.1.0/v0.2.0 the wire enums `OutcomeStatus`, `ErrorCode`, `WaitReason`
  and `Caller` have no `#[serde(other)]` fallback (the queue enums
  `QueueJobState`, `QueueTrainState` and `ReconcileDecision` do). A new
  variant from a newer daemon makes the whole line undecodable in those
  older clients, so a 0.3.0 daemon sends no variant they do not know. The
  fallbacks exist now (`proto/lib.rs`); a variant may be sent one release
  after them.
- **GAP G6 (closed)**: `daemon/tests/compat.rs::unknown_request_type_gets_bad_request_and_the_daemon_keeps_serving`
  pins that an unknown client `type` gets `bad-request` and the daemon keeps
  serving.
- **GAP G7** (alpha C; documented limitation): the `/tmp/sluice-<uid>/daemon.sock`
  socket fallback (used when the default path does not fit `sun_path`) is
  computed differently by a v0.2.0 binary (it fails to bind). Client and
  daemon of 0.3.0 agree (`dirs.rs` `choose_socket`).
- 0.3.0 adds two features, each gating optional messages: `exec` (`exec` and
  `signal` requests, `sluice exec`) and `queue-only` (`RunRequest.no_marker`,
  queue-only gates). A client sends them only to a daemon that announced the
  feature; otherwise `sluice exec` runs in-process and a queue-only gate
  runs without key (uncoalesced, uncached).

### 3.6 Status JSON (`status` reply, `sluice status --json`, MCP `status`)

- Marker `"schema": 1`; `{"schema":1,"daemon":null}` when no daemon runs.
  Fixture `crates/sluice-gate-proto/tests/fixtures/status-schema-1.json`,
  test `status_schema.rs` (equality, round trip, additive `queue`,
  `daemon.log_level`, exec and scheduler fields). Written by the daemon
  (`server.rs`); read by scripts, MCP clients, dashboards, the doctor
  bundle.
- Compatible: new optional fields omitted when absent (as `queue`,
  `log_level`), new `totals` counters (alpha L1 metrics go here). Incompatible:
  rename, remove, re-nest, change units, change what a number counts (bumps
  `schema`).
- Promise: a consumer of the v0.1.0 JSON keeps working: every path exists
  with the same type and meaning. The **superset** test against the
  v0.1.0/v0.2.0 outputs is `proto/tests/compat.rs::status_json_superset`
  (the equality test `status_schema.rs` stays; it fails on any addition by
  design).

### 3.7 Queue status JSON (`queue-status` reply, `sluice queue status --json`, MCP `queue_status`)

- Marker `"schema": 1` (`queue/extension.rs`); shape `{schema, trains,
  jobs}`, views in protocol.md. Since v0.2.0. Unknown job/train states and
  reconcile decisions parse as `Unknown` (`serde(other)`), so new states are
  additive. Incompatible: renaming fields or states, changing snake_case
  spellings, changing `position` or `gates[].status` meaning. Same rules for
  the `queue-*` messages and their error codes.
- **GAP G8 (closed)**: frozen fixtures
  `compat/v0.2.0/queue-status/{empty,populated}.json` and
  `proto/tests/queue_status_schema.rs` (schema version, exact round trip,
  state spellings, unknown states) pin the shape; the superset test is
  `compat.rs::queue_status_json_superset`.

### 3.8 SQLite databases

One WAL file, `$XDG_STATE_HOME/sluice/sluice.db`, with independent schemas.
Facts:

- **Daemon schema** (`schema`, `lease`, `jobs`, `events`, plus the 0.3.0
  `meta`, `job_requests` and `gate_stats` tables and the timeline columns
  of `jobs`): `Store::open` creates everything in an empty file, otherwise
  requires `schema.version == 1` **exactly** (`daemon/store.rs`
  `check_version`). Lower, higher or a missing `schema` table all give
  `Error::Schema` ("database <path> has schema version N (written by
  sluice X), this daemon needs 1; restore a backup or delete the file ..."),
  propagated from daemon start. So **an older binary on a database whose
  `schema.version` was bumped refuses to start the daemon** (clients fall
  back to in-process runs; `doctor` reports it via a read-only open).
  Pinned by `store::tests::version_mismatch_is_schema_error` (version 99).
  `schema.version` is **still 1 in 0.3.0**: every 0.3.0 addition goes
  through the migration framework below and keeps the 0.1.0/0.2.0 check
  passing.
- **Queue schema** (`queue_jobs`, `queue_trains`, `queue_push_markers`,
  `queue_events`, `CREATE ... IF NOT EXISTS` on every start): empty
  `queue_schema` gets version 1; a **newer** version is refused
  (`queue/store.rs` `init`; the queue fails to start, the daemon keeps
  serving gates, queue requests answer `error{internal}` "the merge queue
  failed to start"); an **older** version is accepted and nothing is
  migrated. Test `store::tests::schema_is_idempotent_and_newer_versions_fail_closed`.
  0.3.0 changes nothing in these tables.
- A v0.1.0 binary on a v0.2.0 database works (daemon schema still 1, queue
  tables ignored, its SQL names columns: `JOB_COLUMNS`, `INSERT INTO jobs
  (...)`). A v0.2.0 binary on a v0.1.0 database adds the queue tables. A
  v0.2.0 binary still opens, reads and writes a database the 0.3.0 daemon
  migrated (`daemon/tests/compat.rs::a_v020_daemon_still_opens_and_writes_the_migrated_file`);
  the rows it writes have no timeline, which 0.3.0 reads as unknown.

Migration policy (promise):

1. A database written by any release opens with the current binary.
   **Additive** changes (new table `IF NOT EXISTS`, nullable or defaulted
   column via `ALTER TABLE ADD COLUMN`, new index) are applied on open in one
   `IMMEDIATE` transaction and do **not** bump `schema.version`, so older
   binaries keep working. In 0.3.0 they are numbered migrations of
   `daemon/src/migrate.rs`, versioned by their own one-row table
   (`sched_schema`, version 1: the job timeline columns, `job_requests`,
   `gate_stats`); a `sched_schema` newer than the build is refused
   (`Error::NewerSchema`).
2. A non-additive change bumps that schema's version and ships a forward
   migration `N -> N+1`, run on open in one transaction by the daemon
   (under the lease), after copying `sluice.db` to `sluice.db.bak-vN`. One
   release of warnings (`doctor`, a daemon log line) precedes it. As built,
   the backup of an additive migration is written the same way, named
   `sluice.db.bak-<version before>` (`sluice.db.bak-0` for the first
   `sched_schema` migration), with `VACUUM INTO` a temporary file, fsync and
   rename; an existing backup is kept only when it passes `PRAGMA
   quick_check`, otherwise it is replaced. No backup is written for a
   database created by this run.
3. Downgrade: an older binary meeting a newer database refuses with a clear
   error and never writes (today's daemon and queue behaviour). The message
   says which release wrote the database and how to recover (restore the
   backup, or delete `sluice.db`: job history and queue state are lost, never
   markers, which live in the repository).
4. Markers are not in the database; a reset never invalidates the cache.

- **GAP G9 (closed in H3b / ADR-0011 section 7.4: `daemon/src/migrate.rs`)**:
  v0.2.0 had no migration framework (`check_version` is an equality test,
  the queue tolerates older versions without running anything). 0.3.0 adds
  numbered, additive, transactional migrations with the `.bak` copy and
  tests on the committed v0.1.0/v0.2.0 database files
  (`daemon/tests/compat.rs::old_databases_open`,
  `the_v020_database_migrates_without_losing_history`; migrate.rs and
  store.rs unit tests for the rollback and the backup). The queue tables
  have no migrations yet; the first change to them must follow the same
  policy.
- **GAP G10 (closed in H3b: `meta` table, `daemon/src/meta.rs`, named in `Error::Schema`)**:
  `Error::Schema` (`daemon/src/lib.rs`) did not name the writer release. The
  additive `meta` table stores `sluice_version` as `writer_version` after
  every successful open and the error prints it (`sluice 0.2.0 or older`
  when the row is absent).

### 3.9 Exit codes

No marker; documented in `sluice --help`.

| Code | Meaning | Source |
|---|---|---|
| 0 | pass or cache hit | |
| gate's own | gate failed, code passed through (also the command of `sluice exec`) | |
| 1 | Sluice error | `run.rs` `EXIT_ERROR` |
| 2 | usage error (clap; also `queue`, `update`) | `queue.rs`, `update.rs` `EXIT_USAGE` |
| 3 | refused by `agent_hook_policy = "fail-fast"` | `run.rs` `EXIT_FAIL_FAST` |
| 4 | `sluice queue` request refused by queue state | `queue.rs` `EXIT_REFUSED` |
| 75 | `sluice daemon` found a daemon holding the lock | `proto/client.rs` |
| 124 | gate or `exec` timeout | `run.rs`, `exec.rs` `EXIT_TIMEOUT` |
| 125 | `sluice exec` failed after the daemon accepted the request (0.3.0) | `exec.rs` `EXIT_SLUICE` |
| 141 | `sluice exec` stdout is a closed pipe (0.3.0) | `exec.rs` `EXIT_PIPE` |
| 143 | gate killed (signal, `daemon stop --force`) | `run.rs`, `exec.rs` `EXIT_KILLED` |

`sluice exec` and the shims also use the shell's 126 (not executable), 127
(command not found) and 128+N (killed by signal N).

- Compatible: a new code for a new situation; refining a former 1 into a
  new documented non-zero code. Incompatible: changing a listed number or
  meaning (hooks and the plugin skill rely on 3).
- **GAP G11 (closed in H3b: long help lists every code incl. 4, 75, 125, 141; unit table test in `main.rs`)**:
  the v0.2.0 long help listed 0, gate code, 3, 124, 143, 1, 2 but not 4 and
  75. `sluice --help` now lists every code in the table, pinned by the
  `exit_code_tests` module of `sluice-gate/src/main.rs` and, through the
  binary, by `sluice-gate/tests/compat.rs::exit_codes_are_frozen`,
  `queue_requests_refused_by_the_queue_state_exit_4` and
  `long_help_documents_every_exit_code`.

### 3.10 CLI surface

- Frozen: subcommand names (`run, mcp, daemon [stop|restart], doctor, status,
  cache [status|clear], init, hook print, logs, queue [enqueue|status|
  approve|retry|cancel|reconcile], update`; 0.3.0 adds `exec`, `guard` and
  `shim [install|uninstall|status]`, frozen from then on), their flags as shown by
  `--help` today (for example `run --hook/-v/--explain`, `daemon
  --foreground`, `status --json`, `logs -n/--lines`, `init --force`,
  `doctor --json/--bundle`, `update --check/--version/--yes/--no-restart`),
  positionals, the `--` separator of `run`, the `--json` outputs and the
  `SLUICE_*` variables. Human text is not frozen.
- Compatible: new subcommands (`exec`, `guard`, `shim`), new optional flags, new
  output lines. Incompatible: removing/renaming, changing a flag's meaning
  or a `--json` shape.
- Deprecation: the old form keeps working one release and prints one
  stderr line `warning: <x> is deprecated, use <y> (removed in <N+1>)`.
- **GAP G12 (closed)**: `sluice-gate/tests/compat.rs::cli_surface_is_additive`
  checks that the commands, options, positionals and possible values of every
  `--help` of 0.1.0 and 0.2.0 (`cli-help.txt`) are a subset of the current
  ones, and `every_released_subcommand_is_listed_by_the_top_level_help`.

### 3.11 Plugin surface (`plugin/`)

- No format marker; `plugin.json` `version` follows the release
  (`scripts/release.sh`). Contract: the MCP server key `sluice` from
  `.mcp.json` running `sluice mcp` (tools appear as
  `mcp__plugin_sluice_sluice__<tool>`, used in users' permission rules); the
  tools `verify, status, gate_list, logs, queue_enqueue, queue_status,
  queue_approve, queue_retry, queue_cancel` with their argument and output
  schemas (mcp-tools.md); the skill name `sluice`; the `PreToolUse`/`Bash`
  guard (`hooks/hooks.json`, `guard.sh`; since 0.3.0 it delegates to
  `sluice guard --hook`, which may rewrite a gate or heavy command to run
  through `sluice run`/`sluice exec`, and falls back to the 0.2.0 shell logic
  (deny a raw gate command with a reason) when the binary has no `guard`;
  `sluice queue approve` always denied); the marketplace id `sluice@sluice`.
- Compatible: new tools, new optional arguments or output fields, a guard
  that denies only gate bypasses and approvals or rewrites a command into
  its queued form. Incompatible: renaming a tool
  or the server key, removing an argument or output field, a guard denying
  anything else.
- Skew: plugin (installed by Claude Code) and binary (`install.sh`,
  `update`) update independently; each release's plugin works with the
  previous binary for the tools that already existed, and the binary with
  the previous plugin. New tools appear when both are upgraded.
- **GAP G13 (closed)**: `sluice-gate/tests/compat.rs::tools_are_additive` and
  `queue_tools_exist_since_v0_2_0` pin tool names and input/output schemas
  against `mcp-tools.json` of both releases. **GAP G14 (closed)**:
  `plugin_version_equals_the_workspace_version` and
  `plugin_contract_is_frozen` (same file) check the `plugin.json` version
  and the server key, hook matcher and guard command.

### 3.12 Locations

State, runtime, config dirs and the `markers/` directory are covered:
older binaries must find newer data and vice versa. Locations may gain
fallbacks, never move without a migration.

## 4. Gap summary

| Id | Where | Gap | Fix |
|---|---|---|---|
| G1 | alpha A2/D1 | `cache` bool cannot become a string | new key `mode` (CLOSED) |
| G2 | `config.rs` tests | no older-release config tests | fixtures (CLOSED) |
| G3 | `marker.rs` `prune` | prune skips miss records/foreign formats | extend prune (CLOSED H3b) |
| G4a | `marker.rs` | strict markers make added fields destructive | drop `deny_unknown_fields` (CLOSED H3b) |
| G4b | alpha D1/L1 | cannot add marker data in 0.3.0 | side file (honoured: none added) |
| G5 | `proto/lib.rs` wire enums | wire enums lack `serde(other)` | `Unknown` fallback (CLOSED H3b) |
| G6 | protocol tests | unknown `type` not pinned | replay test (CLOSED) |
| G7 | alpha C | socket fallback differs per version | document (documented limitation) |
| G8 | queue status | no frozen fixture | add (CLOSED) |
| G9 | `daemon/store.rs`, `queue/store.rs` | no migration framework | `migrate` + `.bak` + tests (CLOSED; queue tables have no migrations yet) |
| G10 | `daemon/src/lib.rs` | downgrade error omits writer release | `meta` row (CLOSED H3b) |
| G11 | `--help` | codes 4, 75 undocumented; few pinned | doc + table test (CLOSED H3b) |
| G12 | CLI | surface unpinned | snapshot (CLOSED) |
| G13 | `mcp/src/lib.rs` | tools unpinned | snapshot (CLOSED) |
| G14 | plugin | version equality unchecked | test (CLOSED) |

## 5. Deprecation procedure

1. Release N: old form works and warns once (names replacement and the
   removal release, N+1 at the earliest).
2. Release N+1: old form stops, that interface's version is bumped, the
   migration is documented in `docs/compatibility.md` and the CHANGELOG,
   and the old fixtures stay in a test asserting the helpful refusal or the
   migration result.
3. Fixtures are never edited to make a test pass; a new version adds a new
   `compat/vX.Y.Z/` directory (v0.3.0 when 0.3.0 is released; there is no
   `v0.3.0` directory yet).

## 6. Golden files (as built)

Layout `crates/<crate>/tests/fixtures/compat/<release>/` (`v0.1.0`,
`v0.2.0`), committed verbatim from release binaries, each directory with a
`README.md` (exact commands) and `SHA256SUMS` (checked by
`core/tests/compat.rs::compat_fixtures_match_their_checksums`). They are
generated by `scripts/compat/generate.sh` (see
[[sluice/changes/compat-golden-files.md]] for the full recipe): each release
is downloaded with `gh release download <tag> -R josegarridodigio/sluice -p
'*x86_64*' -p SHA256SUMS`, verified against `SHA256SUMS` and run from a
clean environment (`env -i`, `SLUICE_AGENT=0`) in a private HOME/XDG tree
and a short runtime directory (`/tmp/sa-compat.XXXXXX`: socket path limit).
`scripts/compat/record-proxy.py` records the protocol and
`scripts/compat/tools.py` holds the helpers (all run with `python3 -I`).

Fixture repo: `git init`, `a.txt` = `alpha\n`, the `.sluice.toml` under
test (`repo-files/sluice.toml`), `git add -A` (no commit; the tree hash
depends on content only).

| Fixture (crate, path under `compat/<rel>/`) | Test it feeds |
|---|---|
| core `config/{minimal,full,init,queue,recorded}.toml` (`queue.toml` from v0.2.0 only), `config/expected.json` | `core/tests/compat.rs::configs_parse`, `v0_2_0_ships_a_queue_config`: the current parser accepts them and the gate table equals `expected.json` |
| core `user-config/{empty,full}.toml`, `user-config/expected.json` | `user_configs_parse`, `an_empty_user_config_means_defaults` |
| core `markers/<key>.json` (plain and with `committed_tree`) | `markers_hit`, `markers_written_inside_pre_commit_keep_committed_tree`, `old_keys_are_derived_identically` (keys re-derived from `inputs.json`) |
| core `markers/<key>.miss.json` (`gate failed`, `timed out`, `the gate modified the tree`) | `miss_records_read` |
| core `markers-bad/{format-2,unknown-field,wrong-key}.json`, `cases.json` | `foreign_format_and_wrong_key_markers_are_misses`, `a_marker_with_an_unknown_field_is_still_a_hit` (G4a), `prune_removes_orphaned_miss_records_and_foreign_formats` (G3), `prune_keeps_fresh_markers` |
| proto `status/{status,status-no-daemon}.json` | `proto/tests/compat.rs::status_json_superset`, `status_without_daemon_json_is_frozen`, `status_replies_in_transcripts_are_status_reports` |
| proto `queue-status/{empty,populated}.json` (v0.2.0; copies in the queue crate) | `queue_status_json_superset`, `queue_status_populated_has_a_ready_train`, `queue_status_schema.rs` (G8) |
| proto `transcripts/<scenario>.ndjson`, lines `{"conn","dir","msg"}` (`dir` is `c2s` or `s2c`; scratch paths replaced by `{{REPO}}`, `{{HOOK}}`, `{{QUEUE_CLONE}}`, `{{QUEUE_REMOTE}}`, `{{HOME}}`, `{{RUN}}`, `{{SCRATCH}}`): `hello-welcome`, `run-pass`, `run-hit-event`, `run-fail`, `run-timeout`, `run-modified`, `run-keyed`, `run-slow`, `status`, `logs`, `logs-job`, `stop`; v0.2.0 adds `queue-status-empty`, `queue-status`, `queue-enqueue` | `compat.rs::transcripts_decode`, `welcome_features_follow_the_release`, `recorded_run_outcomes_decode_with_the_documented_statuses`, `run_requests_of_old_clients_decode_to_runs`, `wire_enums_have_an_unknown_fallback` (G5); `compat_client.rs` (the current `Client` against a fake daemon replaying the s2c half); `daemon/tests/compat.rs::old_client_replay`, `old_client_stop_is_honoured`, `unknown_request_type_gets_bad_request_and_the_daemon_keeps_serving` (G6) |
| daemon `db/sluice.db` per release (queue crate: `db/sluice.db`) | `daemon/tests/compat.rs::old_databases_open`, `a_daemon_runs_on_an_old_database`, `the_v020_database_migrates_without_losing_history`, `a_v020_daemon_still_opens_and_writes_the_migrated_file`, `a_refused_newer_database_names_the_release_that_wrote_it` (G10); `queue/tests/compat.rs::queue_tables_added_to_v0_1_0_db`, `v0_2_0_db_keeps_rows`, `a_queue_daemon_serves_an_old_database`, `v0_2_0_queue_requests_are_served_by_the_current_queue` |
| gate `cli-help.txt` (`--help` of every subcommand, one `===== sluice <path> =====` section each) | `gate/tests/compat.rs::cli_surface_is_additive`, `every_released_subcommand_is_listed_by_the_top_level_help` (G12) |
| none (table in the test) | `gate/tests/compat.rs::exit_codes_are_frozen`, `queue_requests_refused_by_the_queue_state_exit_4`, `long_help_documents_every_exit_code` (G11) |
| gate `mcp-tools.json` (`tools/list` from `sluice mcp` after `initialize` over stdio; in the gate crate because the MCP crate has no binary) | `gate/tests/compat.rs::tools_are_additive`, `queue_tools_exist_since_v0_2_0` (G13), `plugin_version_equals_the_workspace_version`, `plugin_contract_is_frozen` (G14) |

Cross-binary check (not cargo; run by the E harness or a script, since it
needs both binaries): old CLI against the current daemon, current CLI
against an old daemon, and one repository shared by a v0.2.0 and the
current binary, asserting that a marker written by one is a hit for the
other (`scripts/alpha/`, scenario `s6_upgrade.py`). The tests above are the
permanent, hermetic form of those facts.

Existing tests that stay: `status_schema.rs`,
`key::tests::golden_vector_matches_byte_layout`,
`key::tests::key_prefix_tracks_cache_format_version`, the marker unit tests
(format, corrupt, TTL), `daemon/store.rs`
`version_mismatch_is_schema_error`, `queue/store.rs`
`schema_is_idempotent_and_newer_versions_fail_closed`,
`daemon/tests/daemon.rs::handshake_checks_the_protocol`.

## 7. Decisions (taken in ADR-0013)

1. G4a accepted: new marker fields are only safe one release later; 0.3.0
   writes none.
2. `Unknown` fallbacks on the four wire enums (G5) shipped in 0.3.0.
3. Database policy of 3.8 confirmed and implemented (G9, G10); no
   non-additive change is planned.
4. New gate options use new keys, never new types (G1): `mode`, `paths`,
   `paths_ignore`.
