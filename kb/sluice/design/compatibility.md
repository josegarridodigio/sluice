---
title: Compatibility promise (draft)
tags: [sluice, design, compatibility, alpha, h3]
---

# Compatibility promise (draft for ADR-0013 and `docs/compatibility.md`)

Status: draft for task H3 of [[sluice/plan/alpha-plan]], written against
the v0.2.0 code plus the alpha working tree. **GAP** marks a place where
the code does not yet honour the promise (each becomes a task for the
lead). Facts about current behaviour were checked in the sources (file:line).

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
| `.sluice.toml` | `version` key | 1 | `config.rs:21` |
| user `config.toml` | `version` key (optional) | 1 | `config.rs:416` |
| Markers, miss records | `format` field | 1 | `core/lib.rs:34` |
| Cache key | prefix `sluice-cache-v1` | v1 | `key.rs:15` |
| Daemon protocol | `hello/welcome.protocol` | 1 | `proto/lib.rs:39` |
| Features | `welcome.features` | `["queue"]` | `proto/lib.rs:49` |
| Status JSON | `schema` | 1 | `proto/lib.rs:42` |
| Queue status JSON | `schema` | 1 | `proto/lib.rs:45` |
| Daemon DB | table `schema(version)` | 1 | `daemon/store.rs:16` |
| Queue tables | table `queue_schema(version)` | 1 | `queue/store.rs:25` |
| Exit codes, CLI flags, MCP tools | none (absent) | n/a | `--help`, `mcp/src/lib.rs` |
| Plugin | `plugin.json` `version` (tracks the release) | 0.2.0 | `plugin/.claude-plugin/` |

## 3. Interface inventory

### 3.1 Repository config `.sluice.toml`

- Marker `version = 1`, required; other values fail with
  `unsupported version N` (`config.rs:46-55`). Written by humans and
  `sluice init`; read by every binary (the daemon reads `[queue]` with
  `git show <base>:.sluice.toml`).
- Parsing is strict everywhere (`deny_unknown_fields`, `config.rs:66, 84,
  142, 176, 417-606`). Fact: an **older binary rejects a config that uses a
  newer optional field** (v0.1.0 rejects `[queue]`; v0.2.0 will reject every
  0.3.0 addition). Stated, not changed; mixed-version repos avoid new keys.
- Compatible: new optional key/table whose default keeps today's behaviour;
  looser validation. Incompatible: rename, retype, new required key, tighter
  validation that fails a previously valid file, changed default, any change
  to cache-key inputs (3.3).
- Promise: every file valid at v0.1.0/v0.2.0 yields the same gates and
  therefore the same keys in every later 0.x. Another `version` is a new
  format with migration note and a warning release.
- **GAP G1** (alpha A2/D1): `gates.<name>.cache` is a bool; the plan floats
  `cache = "queue-only"`. Retyping a key is not additive: add a new optional
  key (for example `mode`) and keep `cache`.

### 3.2 User config `~/.config/sluice/config.toml`

- Marker `version` (optional, must be 1, `config.rs:712`); path via
  `SLUICE_CONFIG`. Strict parsing like 3.1. Daemon reads sections at start,
  CLI/MCP per call. The `SLUICE_*` variables and the truthy/falsy words of
  config-format.md are part of this interface: no renames, additions allowed.
- Promise: any file valid at v0.1.0/v0.2.0 loads with the same effective
  values; `daemon.idle_exit` stays accepted although inert.
- **GAP G2**: no test loads a config written by an older release (only the
  doc example, `config.rs:777`). Fixtures in section 6.

### 3.3 Cache key derivation

- Marker: the first hashed field `sluice-cache-v1`. Unit tests pin the exact
  bytes (`key.rs:203`) and `prefix == "sluice-cache-v{CACHE_FORMAT_VERSION}"`
  (`key.rs:281`). The marker file name is the key, so old and new binaries
  share a cache **only if they derive identical keys**.
- Promise: byte layout, `cmd` normalisation, env sorting, `key_inputs` /
  `key_files` folding and the tree-hash procedure do not change inside v1.
  Gates without the new `paths` option must keep the old key (alpha D1).
- Prefix change (`sluice-cache-v2`): bump `KEY_PREFIX` and
  `CACHE_FORMAT_VERSION` together (the test forces it). New keys differ, so
  old markers are never read: a lookup misses, the gate runs once, a v2
  marker is written. No migration and no run-time error; the release notes
  must say that the first run after upgrade is a miss.
- **GAP G3**: `prune` only deletes markers by mtime and skips miss records
  (`marker.rs:324-336`); after a bump, orphaned `*.miss.json` live forever
  and orphans markers one TTL. Prune records with a foreign `format` and miss
  records older than the TTL.

### 3.4 Markers and miss records

- Marker `format: 1`; path `<git-common-dir>/sluice/markers/<key>.json` and
  `<key>.miss.json` (public contract, cache-key.md). Written by whichever
  process ran the gate, read by every binary of any worktree and by shell
  snippets. Field sets: marker {format, key, tree, gate, cmd, passed_at,
  duration_ms, exit_code, job_id, sluice_version, committed_tree}; miss
  {format, key, gate, exit_code, reason, job_id, log_path, recorded_at}.
- Reader facts: a marker with another `format`, unparsable JSON or a `key`
  different from its file name is **deleted** and counts as a miss
  (`marker.rs:186-191`); a bad miss record is ignored, not deleted
  (`:229-235`).
- Both structs are `deny_unknown_fields` (`marker.rs:18, 46`). Fact: **any
  field added to a marker looks corrupt to every released binary, which
  deletes the marker** on its next lookup, silently defeating the cache
  whenever a v0.1.0/v0.2.0 binary (long-lived daemon, plugin) shares the
  repo with a newer one.
- Promise: `format` stays 1 and the field sets above stay exactly as listed
  for 0.3.0. New data goes in a separate file, or waits for G4a plus a
  release.
- **GAP G4a**: drop `deny_unknown_fields` on `Marker` and `MissRecord`
  (`marker.rs:18, 46`), keep the `format` check, and correct cache-key.md
  ("unknown fields are corrupt"). Field additions become safe one release
  later.
- **GAP G4b**: any 0.3.0 plan item that wants new per-marker data (D1 `paths`,
  L1) must not extend the marker (see promise).

### 3.5 Daemon protocol (NDJSON over the Unix socket)

- Marker `protocol: 1` in `hello`/`welcome`; `welcome.features` optional
  (omitted when empty): v0.1.0 daemons send none, v0.2.0 with the queue send
  `["queue"]`. A daemon outlives `sluice update`, so **old client with new
  daemon and new client with old daemon both occur**.
- Negotiation: the client sends `hello {protocol, client, pid}`; the daemon
  accepts an equal `protocol` only (`server.rs:1236`, else `error{protocol}`),
  the client checks `welcome` likewise (`client.rs:127`) and prints the
  `sluice daemon stop` hint. Optional families are gated by features: a
  client sends `queue-*` only when `Client::supports("queue")`; a daemon
  without the extension answers `error{not-supported}`; a v0.1.0 daemon
  does not know the types and answers `error{bad-request}`. A new family =
  a new feature string, `protocol` stays 1; new request/reply types are sent
  only to a peer that announced the feature.
- Compatible: a new optional field on any message (wire types ignore unknown
  fields, `proto/lib.rs:16`), a new feature with its messages, a new
  `status` field. Incompatible: removing/renaming a field or `type`,
  retyping or re-meaning a field, a new required request field, new
  unconditional reply events.
- Promise: v0.1.0 and v0.2.0 clients work against current daemons (hello,
  run with queued/started/progress/done, status, logs, event, stop, cancel)
  and current clients work against their daemons. A `protocol` bump keeps
  the stop-hint refusal and ships one release in which clients warn about an
  older daemon.
- **GAP G5**: the wire enums `OutcomeStatus` (`proto/lib.rs:497`),
  `ErrorCode` (`:516`), `WaitReason` (`:664`) and `Caller` (`:301`) have no
  `#[serde(other)]` fallback, unlike the queue enums (`:746, 772, 914`). A new
  variant from a newer daemon makes the whole line undecodable in an older
  client. Until fixed: add no variants. Add `Unknown` fallbacks one release
  before any variant is sent.
- **GAP G6**: no test pins that an unknown client `type` gets `bad-request`
  and the daemon keeps serving (add to the replay tests).
- **GAP G7** (alpha C): the planned `/tmp/sluice-<uid>` socket fallback is
  computed differently by a v0.2.0 binary (it fails to bind). Document as a
  known limitation; client and daemon of 0.3.0 agree.

### 3.6 Status JSON (`status` reply, `sluice status --json`, MCP `status`)

- Marker `"schema": 1`; `{"schema":1,"daemon":null}` when no daemon runs.
  Fixture `crates/sluice-gate-proto/tests/fixtures/status-schema-1.json`,
  test `status_schema.rs` (equality, round trip, additive `queue` and
  `daemon.log_level`). Written by the daemon (`server.rs:1438`); read by
  scripts, MCP clients, dashboards, the doctor bundle.
- Compatible: new optional fields omitted when absent (as `queue`,
  `log_level`), new `totals` counters (alpha L1 metrics go here). Incompatible:
  rename, remove, re-nest, change units, change what a number counts (bumps
  `schema`).
- Promise: a consumer of the v0.1.0 JSON keeps working: every path exists
  with the same type and meaning. H3 adds a **superset** test against the
  v0.1.0/v0.2.0 outputs (the existing equality test stays; it fails on any
  addition by design).

### 3.7 Queue status JSON (`queue-status` reply, `sluice queue status --json`, MCP `queue_status`)

- Marker `"schema": 1` (`queue/extension.rs:414`); shape `{schema, trains,
  jobs}`, views in protocol.md. Since v0.2.0. Unknown job/train states and
  reconcile decisions parse as `Unknown` (`serde(other)`), so new states are
  additive. Incompatible: renaming fields or states, changing snake_case
  spellings, changing `position` or `gates[].status` meaning. Same rules for
  the `queue-*` messages and their error codes.
- **GAP G8**: no frozen fixture or schema test exists (only a unit round
  trip, `proto/lib.rs:1602`). Add them (section 6).

### 3.8 SQLite databases

One WAL file, `$XDG_STATE_HOME/sluice/sluice.db`, two independent schemas.
Facts:

- **Daemon schema** (`schema`, `lease`, `jobs`, `events`): `Store::open`
  creates everything in an empty file, otherwise requires `schema.version == 1`
  **exactly** (`daemon/store.rs:313-321`). Lower, higher or a missing `schema`
  table all give `Error::Schema` ("database <path> has schema version N, this
  daemon needs 1"), propagated from daemon start (`server.rs:476`). So **an
  older binary on a bumped database refuses to start the daemon** (clients
  fall back to in-process runs; `doctor` reports it via a read-only open,
  `doctor.rs:359`). There is **no migration code**: a lower version is also
  refused. Pinned only for version 99 (`store.rs:860-880`).
- **Queue schema** (`queue_jobs`, `queue_trains`, `queue_push_markers`,
  `queue_events`, `CREATE ... IF NOT EXISTS` on every start): empty
  `queue_schema` gets version 1; a **newer** version is refused
  (`queue/store.rs:306`; the queue fails to start, the daemon keeps serving
  gates, queue requests answer `error{internal}` "the merge queue failed to
  start"); an **older** version is accepted and nothing is migrated
  (`store.rs:307`). Test at `store.rs:1396-1407`.
- A v0.1.0 binary on a v0.2.0 database works today (daemon schema still 1,
  queue tables ignored, its SQL names columns: `JOB_COLUMNS`,
  `INSERT INTO jobs (...)`). A v0.2.0 binary on a v0.1.0 database adds the
  queue tables.

Migration policy (promise):

1. A database written by any release opens with the current binary.
   **Additive** changes (new table `IF NOT EXISTS`, nullable or defaulted
   column via `ALTER TABLE ADD COLUMN`, new index) are applied on open in one
   `IMMEDIATE` transaction and do **not** bump the version, so older binaries
   keep working. The 0.3.0 timeline and idle-CPU storage (L1) must be done
   this way.
2. A non-additive change bumps that schema's version and ships a forward
   migration `N -> N+1`, run on open in one transaction by the daemon
   (under the lease), after copying `sluice.db` to `sluice.db.bak-vN`. One
   release of warnings (`doctor`, a daemon log line) precedes it.
3. Downgrade: an older binary meeting a newer database refuses with a clear
   error and never writes (today's daemon and queue behaviour). The message
   says which release wrote the database and how to recover (restore the
   backup, or delete `sluice.db`: job history and queue state are lost, never
   markers, which live in the repository).
4. Markers are not in the database; a reset never invalidates the cache.

- **GAP G9**: no migration framework: `check_version` is an equality test
  (`daemon/store.rs:313`), the queue tolerates older versions without
  running anything (`queue/store.rs:307`). Needed before the first bump: a
  `migrate(from, to)` step per schema, the `.bak` copy and tests on committed
  v0.1.0/v0.2.0 database files (section 6).
- **GAP G10**: `Error::Schema` (`daemon/lib.rs:33`) does not name the writer
  release. Store `sluice_version` in a new `meta` table (additive) and print
  it.

### 3.9 Exit codes

No marker; documented in `sluice --help`.

| Code | Meaning | Source |
|---|---|---|
| 0 | pass or cache hit | |
| gate's own | gate failed, code passed through | |
| 1 | Sluice error | `run.rs:54` |
| 2 | usage error (clap; also `queue`, `update`) | `queue.rs:29`, `update.rs:26` |
| 3 | refused by `agent_hook_policy = "fail-fast"` | `run.rs:50` |
| 4 | `sluice queue` request refused by queue state | `queue.rs:31` |
| 75 | `sluice daemon` found a daemon holding the lock | `proto/client.rs:31` |
| 124 | gate timeout | `run.rs:52` |
| 143 | gate killed (signal, `daemon stop --force`) | `run.rs:56` |

- Compatible: a new code for a new situation; refining a former 1 into a
  new documented non-zero code. Incompatible: changing a listed number or
  meaning (hooks and the plugin skill rely on 3).
- **GAP G11**: the long help lists 0, gate code, 3, 124, 143, 1, 2 but not 4
  and 75; only eight assertions in `crates/sluice-gate/tests` cover codes.
  Document both and add a table-driven test.

### 3.10 CLI surface

- Frozen: subcommand names (`run, mcp, daemon [stop|restart], doctor, status,
  cache [status|clear], init, hook print, logs, queue [enqueue|status|
  approve|retry|cancel|reconcile], update`), their flags as shown by
  `--help` today (for example `run --hook/-v/--explain`, `daemon
  --foreground`, `status --json`, `logs -n/--lines`, `init --force`,
  `doctor --json/--bundle`, `update --check/--version/--yes/--no-restart`),
  positionals, the `--` separator of `run`, the `--json` outputs and the
  `SLUICE_*` variables. Human text is not frozen.
- Compatible: new subcommands (`exec`, `shim`), new optional flags, new
  output lines. Incompatible: removing/renaming, changing a flag's meaning
  or a `--json` shape.
- Deprecation: the old form keeps working one release and prints one
  stderr line `warning: <x> is deprecated, use <y> (removed in <N+1>)`.
- **GAP G12**: nothing pins the command and flag list (snapshot test in
  section 6).

### 3.11 Plugin surface (`plugin/`)

- No format marker; `plugin.json` `version` follows the release
  (`scripts/release.sh:44`). Contract: the MCP server key `sluice` from
  `.mcp.json` running `sluice mcp` (tools appear as
  `mcp__plugin_sluice_sluice__<tool>`, used in users' permission rules); the
  tools `verify, status, gate_list, logs, queue_enqueue, queue_status,
  queue_approve, queue_retry, queue_cancel` with their argument and output
  schemas (mcp-tools.md); the skill name `sluice`; the `PreToolUse`/`Bash`
  guard (`hooks/hooks.json`, `guard.sh`: deny with a reason; `sluice queue
  approve` always denied); the marketplace id `sluice@sluice`.
- Compatible: new tools, new optional arguments or output fields, a guard
  that denies only gate bypasses and approvals. Incompatible: renaming a tool
  or the server key, removing an argument or output field, a guard denying
  anything else.
- Skew: plugin (installed by Claude Code) and binary (`install.sh`,
  `update`) update independently; each release's plugin works with the
  previous binary for the tools that already existed, and the binary with
  the previous plugin. New tools appear when both are upgraded.
- **GAP G13**: nothing pins tool names and output schemas
  (`mcp/src/lib.rs:896-1217`). **GAP G14**: nothing but `release.sh` keeps
  the `plugin.json` version equal to the workspace version.

### 3.12 Locations

State, runtime, config dirs and the `markers/` directory are covered:
older binaries must find newer data and vice versa. Locations may gain
fallbacks, never move without a migration.

## 4. Gap summary (tasks for the lead)

| Id | Where | Gap | Fix |
|---|---|---|---|
| G1 | alpha A2/D1 | `cache` bool cannot become a string | new key `mode` |
| G2 | `config.rs` tests | no older-release config tests | fixtures |
| G3 | `marker.rs:324-336` | prune skips miss records/foreign formats | extend prune |
| G4a | `marker.rs:18,46` | strict markers make added fields destructive | drop `deny_unknown_fields` |
| G4b | alpha D1/L1 | cannot add marker data in 0.3.0 | side file |
| G5 | `proto/lib.rs:301,497,516,664` | wire enums lack `serde(other)` | `Unknown` fallback |
| G6 | protocol tests | unknown `type` not pinned | replay test |
| G7 | alpha C | socket fallback differs per version | document |
| G8 | queue status | no frozen fixture | add |
| G9 | `daemon/store.rs:313`, `queue/store.rs:307` | no migration framework | `migrate` + `.bak` + tests |
| G10 | `daemon/lib.rs:33` | downgrade error omits writer release | `meta` row |
| G11 | `--help` | codes 4, 75 undocumented; few pinned | doc + table test |
| G12 | CLI | surface unpinned | snapshot |
| G13 | `mcp/src/lib.rs` | tools unpinned | snapshot |
| G14 | plugin | version equality unchecked | test/CI |

## 5. Deprecation procedure

1. Release N: old form works and warns once (names replacement and the
   removal release, N+1 at the earliest).
2. Release N+1: old form stops, that interface's version is bumped, the
   migration is documented in `docs/compatibility.md` and the CHANGELOG,
   and the old fixtures stay in a test asserting the helpful refusal or the
   migration result.
3. Fixtures are never edited to make a test pass; a new version adds a new
   `compat/vX.Y.Z/` directory (v0.3.0 at R4).

## 6. Golden files H3 must add

Layout `crates/<crate>/tests/fixtures/compat/<release>/` (`v0.1.0`,
`v0.2.0`), committed verbatim from release binaries, each directory with a
`README.md` (exact commands) and `SHA256SUMS`. Setup, per release `$v`:

```sh
S=$(mktemp -d /tmp/sa-compat.XXXXXX)            # short: socket path limit
mkdir -p $S/$v/bin && cd $S/$v
gh release download $v -R josegarridodigio/sluice \
   -p "sluice-${v#v}-x86_64-unknown-linux-musl.tar.gz" -p SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS && tar xzf sluice-*.tar.gz -C bin
export HOME=$S/$v/home XDG_CONFIG_HOME=$S/$v/home/.config \
  XDG_STATE_HOME=$S/$v/home/.state XDG_RUNTIME_DIR=$S/$v/run \
  SLUICE_SOCKET=$S/$v/run/d.sock           # never touch the real HOME
```

Fixture repo: `git init`, `a.txt` = `alpha\n`, the `.sluice.toml` under
test, `git add -A` (no commit; the tree hash depends on content only). Keep
the same files under `repo-files/` so tests can rebuild the tree.

| Fixture (crate, path under `compat/<rel>/`) | Generated with | Test it feeds |
|---|---|---|
| core `config/{minimal,full}.toml`, `config/queue.toml` (v0.2.0 only) | the config-format.md example of that tag (`git show <tag>:kb/sluice/design/config-format.md`) and `sluice init` output; validated by `<bin> run <gate>` in the fixture repo (no `invalid configuration`) | `core/tests/compat.rs::configs_parse`: current parser accepts; gate table equals `expected.json` |
| core `user-config/{empty,full}.toml` | doc example of that tag, validated by starting the daemon with `SLUICE_CONFIG` | `compat.rs::user_configs_parse`: effective values equal `expected.json` |
| core `markers/<key>.json` (pass, `cmd = "true"`) | `<bin> run test` in the fixture repo, copy `$repo/.git/sluice/markers/*.json` | `compat.rs::markers_hit`: `lookup` is `Hit` with a century TTL; `key::derive` over `inputs.json` equals the file name |
| core `markers/<key>.json` with `committed_tree` | snippet from `sluice hook print` as `.git/hooks/pre-commit`, stage a file leaving an unstaged change, `git commit` | same test, field parsed |
| core `markers/<key>.miss.json` x3: `gate failed` (`false`), `timed out` (`timeout = "1s"`, `sleep 5`), `the gate modified the tree` (`echo x >> a.txt`) | one run of each gate, copy the `*.miss.json` | `compat.rs::miss_records_read`: `read_miss`, `Lookup::Miss(Some(..))` with reason and code |
| core `inputs.json` (tree, gate, normalised cmd, env, key_inputs outputs, key_files hashes) | by hand from the fixture repo; cross-check tree with `sluice cache status --all` | key recomputation above |
| core `markers-bad/` (format 2, unknown field, wrong key) | edit a real marker | `lookup` deletes and misses; the unknown-field case flips when G4a lands |
| proto `status/status.json`, `status/status-no-daemon.json` | `<bin> status --json` while the daemon (autostarted by `run`) has a finished job and a running one (`sleep 3 &`); the second after `daemon stop` | `proto/tests/compat.rs::status_json_superset`: parses as `StatusReport`, every old path and type exists in `to_value(current)`, `schema == 1` |
| proto `queue-status/{empty,populated}.json` (v0.2.0) | local bare remote plus clone with `.sluice.toml`; `sluice queue status --json` before and after `queue enqueue`; a `ready` train needs a passing gate and `max_train = 1` | `compat.rs::queue_status_json_superset` plus new frozen test `queue_status_schema.rs` (fixes G8) |
| proto `transcripts/<scenario>.ndjson`, lines `{"dir":"c2s"\|"s2c","line":{...}}` | proxy `scripts/compat/record-proxy.py` (`python3 -I`): listens on `$SLUICE_SOCKET`, forwards to the daemon's `real.sock`, logs both directions per connection. Daemon: `<bin> daemon --foreground` with `SLUICE_SOCKET=real.sock`; clients use the proxy. Scenarios: `hello-welcome` (raw `nc -U`), `run-pass`, `run-fail`, `run-hit-event` (second run sends the `event`), `status`, `logs`, `stop`; v0.2.0 adds `queue-status`, `queue-enqueue` | `compat.rs::transcripts_decode` (every c2s line via `decode_client`, every s2c via `decode_server`); `daemon/tests/compat.rs::old_client_replay`: replay c2s lines (placeholders `{{REPO}}`, `{{CWD}}`) against the current daemon, assert the s2c type sequence (`welcome`, `queued`, `started`, `done{status}`) and stable fields, ignoring ids, times, paths, pids; also an unknown `type` gets `bad-request` (G6) |
| proto same transcripts, s2c half | same recordings | `compat.rs::new_client_against_recorded_daemon`: a fake daemon replays s2c lines to `client::Client` (hello, run, status, logs); `supports("queue")` false for v0.1.0, true for v0.2.0 |
| daemon `db/sluice.db` per release (binary) | scenarios above, then `<bin> daemon stop`, `sqlite3 sluice.db 'PRAGMA wal_checkpoint(TRUNCATE)'`; v0.2.0 run also enqueues one job | `daemon/tests/compat.rs::old_databases_open` (copy to tempdir, `Store::open`, rows readable); `queue/tests/compat.rs::queue_tables_added_to_v0_1_0_db`, `v0_2_0_db_keeps_rows`; the schema-99 refusal stays |
| gate `cli-surface.txt` (option names per subcommand) | `<bin> <cmd> --help` of v0.2.0, option names one per line | `gate/tests/cli.rs::cli_surface_is_additive` (G12) |
| none (table in the test) | n/a | `gate/tests/cli.rs::exit_codes_are_frozen`: 1, 2, 3, 124, 143, 4, 75 (G11) |
| mcp `tools-v0.2.0.json` | `tools/list` from `<bin> mcp` after `initialize` over stdio | `mcp/tests/compat.rs::tools_are_additive` (G13) |

Cross-binary check (not cargo; run by the E harness or a script, since it
needs both binaries): old CLI against the current daemon, current CLI
against an old daemon, and one repository shared by a v0.2.0 and the
current binary, asserting that a marker written by one is a hit for the
other. The tests above are the permanent, hermetic form of those facts.

Existing tests that stay: `status_schema.rs`, `key.rs:203/281`, marker unit
tests (format, corrupt, TTL), `daemon/store.rs:860-880`,
`queue/store.rs:1396`, `daemon/tests/daemon.rs::handshake_checks_the_protocol`.

## 7. Decisions needed from the lead (ADR-0013)

1. Accept G4a now (new marker fields only safe one release later).
2. Add `Unknown` fallbacks to the four wire enums (G5) in 0.3.0.
3. Confirm the database policy in 3.8; schedule G9/G10 before any task that
   needs a non-additive change (none planned in the alpha).
4. Confirm that new gate options use new keys, never new types (G1).
