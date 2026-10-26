# Compatibility promise

Sluice stores data in your repositories and talks to a long-lived daemon that
can outlive an update, so versions meet each other all the time. This page
says what stays stable, what you can rely on when you upgrade or downgrade,
and what to do when something breaks.

The promise starts with release 0.3.0. It is checked by golden files recorded
from the released binaries (see [Proof](#proof)).

## The promise

1. Every interface listed below keeps working with the data and the peers
   produced by every released version since its baseline (0.1.0; the merge
   queue since 0.2.0).
2. Changes are additive only: new optional fields, new optional messages
   negotiated through the daemon handshake, new subcommands and flags, new
   tables or nullable columns, and new accepted values of an existing key
   (`weight = "auto"`, 0.4.0; every value valid before keeps its meaning).
   Nothing is renamed, removed, retyped, re-nested or given another meaning.
3. A breaking change needs all three of: a new version number for that
   interface, a migration path (automatic where data is involved) and one
   release of deprecation warnings before it happens.
4. Direction: a new binary reads everything old binaries wrote. Whether an
   old binary reads data written by a new one is stated per interface below as
   a fact. Several old readers are strict, so it is not promised.

Not covered: log text, `sluice doctor` output, the human text of `sluice
status`, `client.log` and `daemon.log` lines, and the wording of errors.

## What is covered

| Interface | Version marker | Value |
|---|---|---|
| `.sluice.toml` | `version` | 1 |
| User `config.toml` | `version` (optional) | 1 |
| Markers and miss records | `format` | 1 |
| Cache key | prefix `sluice-cache-v1` | v1 |
| Daemon protocol | `protocol` in `hello` / `welcome` | 1 |
| Daemon features | `welcome.features` | `["exec", "queue-only", "queue"]` (0.2.0: `["queue"]`) |
| Status JSON | `schema` | 1 |
| Queue status JSON | `schema` | 1 |
| Database | tables `schema`, `queue_schema` and `sched_schema` | 1, 1 and 1 (`containment_schema` is 2 from 0.6.0; older binaries ignore it) |
| Exit codes, CLI flags, MCP tools | none | n/a |
| Plugin | `plugin.json` `version`, follows the release | |

### Repository config `.sluice.toml`

`version = 1` is required. Every file that was valid for 0.1.0 or 0.2.0 yields
the same gates, and therefore the same cache keys, in every later 0.x. New
options are optional and default to today's behaviour. Another `version` value
is a new format and comes with a migration note and a warning release.

Old binaries read new data like this: parsing is strict (unknown keys are an
error), so **an older binary rejects a config that uses a newer option**. In
`.sluice.toml`, 0.6.0 adds `[queue] prepare`, `prepare_timeout` and
`prepare_weight` (fixture `queue-prepare.toml`): every machine that reads a file
using them needs 0.6.0, hooks and agents' binaries included. For
example 0.1.0 rejects a `[queue]` table, and 0.2.0 will reject options added in
0.3.0. The same holds for a new value of an existing key: `weight = "auto"`
(0.4.0) is rejected by 0.3.0 and older, which accept only integers. If several versions share a repository, avoid the new keys until
everyone has upgraded.

### User config `~/.config/sluice/config.toml`

Any file valid for 0.1.0 or 0.2.0 loads with the same effective values;
`daemon.idle_exit` stays accepted although it has no effect. The `SLUICE_*`
variables and their truthy and falsy words keep their meaning; new ones may be
added. An older binary rejects keys it does not know, as above: a
`[containment] gates_oom_score_adj` / `gates_memory_max` / `gates_memory_swap_max` or `[admission] memory_aware` /
`memory_reserve` / `memory_guard` (0.6.0) in a file shared with an older binary stops that
binary's daemon from loading the config.

### Cache keys, markers and miss records

The key layout, command normalisation, environment sorting, `key_inputs` and
`key_files` handling and the tree hash procedure do not change inside `v1`.
The marker files under `<git-common-dir>/sluice/markers/` keep `format: 1` and
the field sets of 0.2.0, so old and new binaries share one cache.

Facts about old binaries: a marker with another `format`, unparsable JSON or a
key that does not match its file name is deleted by every released binary and
counts as a miss. Released binaries (0.1.0 and 0.2.0) also treat a marker with an
unknown extra field as damaged, so no field is added to markers in 0.3.0; new
per-marker data goes in separate files. From 0.3.0 on, Sluice itself accepts
unknown fields in markers and miss records (they are ignored, not deleted), so
a field added by a later release is safe once 0.3.0 is the oldest binary in
use. The TTL pruning of markers (`[cache] ttl_days`) also removes miss records and files of a foreign `format`
once they are older than the TTL. If a future release changes the key prefix
(`sluice-cache-v2`), the first run after the upgrade is a cache miss, the gate
runs once and a `v2` marker is written. Nothing else happens: no migration and
no error.

### Daemon protocol

The daemon speaks newline-delimited JSON over a Unix socket. `protocol` stays
1. Clients and daemons of different versions meet, because a daemon outlives
`sluice update`: an old client works against a new daemon and a new client
against an old one. Optional message families are announced in
`welcome.features` (a new family is a new feature string; 0.6.0 adds
`queue-trust-config`, and a client that does not see it refuses
`--trust-member-config` instead of sending a flag an old daemon would ignore) and are only sent to
a peer that announced them; a daemon without the merge queue answers queue
requests with an error and keeps serving. Unknown fields are ignored on both
sides. A real `protocol` bump keeps the refusal that tells you to run `sluice
daemon stop`, and ships one release in which clients warn about an older
daemon.

Fact: in 0.1.0 and 0.2.0 some wire enumerations (outcome status, error code,
wait reason, caller) have no fallback for unknown values, so a newer daemon
that sent a new value would make those clients fail to decode the line. From
0.3.0 they parse unknown values as `unknown` (an unknown outcome is reported as
an error, never as a pass). No new values are sent until 0.3.0 is the oldest
client in use.

### Status JSON and queue status JSON

`sluice status --json` (and the MCP `status` tool) prints `{"schema": 1, ...}`;
with no daemon it prints `{"schema":1,"daemon":null}`. `sluice queue status
--json` (and `queue_status`) prints `{"schema": 1, "trains": [...], "jobs":
[...]}`. Every field a script could read in the 0.1.0 and 0.2.0 output keeps its
path, type and meaning. New optional fields and new counters may appear (0.6.0:
`containment.gates_oom_policy`, the memory fields of running and queued jobs, in
`queue status` the train's `prepare`, `config_source` and `config_diff`, a gate's
`note`, and the reasons `gate_modified_tree`, `prepare_failed`, `prepare_dirty`,
`key_input_failed`, `hook_gate_unverified` and `member_config_invalid`; an
outcome's `killed_by` may be `memory`);
unknown queue job and train states are parsed as `unknown` by current clients.

0.6.0 also changes where `systemd-oomd` acts: `sluice-daemon.scope` gets its own
memory pressure policy (the agents' limit with half the duration) and its
`daemon/` leaf the `user.oomd_omit` attribute, on the machines where the agents'
policy exists. Nothing to configure and nothing an older binary has to
understand: a scope that outlives an upgrade is aligned at runtime, and an older
daemon simply creates it without the policy. The opt-in `[admission]
memory_guard` and its `sluice_memory_guard` interventions are one-way like the
other 0.6.0 keys (fixture `crates/sluice-gate-core/tests/fixtures/unreleased/admission-memory-guard.toml`).

0.6.1 adds the user config keys `[queue] tool_env` and `[queue] tool_path` (ADR-0020).
They are one-way like the 0.6.0 keys: 0.6.0 and older binaries reject a user config that
has them (fixture `crates/sluice-gate-core/tests/fixtures/unreleased/queue-tool-env.toml`).
The status JSON gains the optional `queue.tool_env` (`source`, `reason`, `path`), which
older clients ignore.

### The database

`$XDG_STATE_HOME/sluice/sluice.db` holds the daemon's job history and the merge
queue's state in two independent schemas. Markers are not in it.

- A database written by any release opens with the current binary.
- Additive changes (a new table, a nullable column, an index) are applied when
  the daemon opens the file, do not change the schema version, and so keep
  older binaries working.
- The queue's 0.6.0 additions are nullable or defaulted columns
  (`queue_trains.prepare`, `config_source`, `config_hash`,
  `queue_jobs.trust_config`), added by an idempotent migration when the daemon
  opens the file; `queue_schema` stays 1 and an older binary ignores them.
- The scheduler timeline and the learned-weight statistics of 0.3.0 (extra
  `jobs` columns and the `job_requests` and `gate_stats` tables) are additive
  and tracked by their own `sched_schema` version; older binaries ignore them.
- The containment additions have their own `containment_schema` version
  (`interventions` in 0.5.0, version 1; the `gate_memory` table of memory-aware
  admission in 0.6.0, version 2). A newer version is never an error for an older
  binary, which finds nothing to apply and ignores the table; the migration is
  one new table, so no backup is written. `gate_stats` and `sched_schema` did
  not change: a column there would make 0.5.0 and older refuse the file.
- A non-additive change bumps that schema's version and ships a forward
  migration that runs once on open, after copying `sluice.db` to
  `sluice.db.bak-<version>`. One release of warnings comes first.

**Downgrading.** An older binary that meets a database with a newer schema
version refuses it and never writes to it. From 0.3.0 the message names the
release that last opened the database (kept in an additive `meta` table that
older binaries ignore) and how to recover: the daemon does not start (clients
fall back to running gates in-process and `sluice doctor` reports the reason),
or, for the queue schema, the merge queue does not start while gates keep
working. Before you update across a release that announces a schema change,
copy the file:

```sh
cp "${XDG_STATE_HOME:-$HOME/.local/state}/sluice/sluice.db" sluice.db.backup
```

To go back, stop the daemon (`sluice daemon stop`), restore that copy under the
name `sluice.db`, and start the old binary. If you have no copy, deleting
`sluice.db` is safe for the cache: you lose job history and queue state, never
markers, which live in the repository.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | pass or cache hit |
| gate's own | the gate failed; its code is passed through |
| 1 | Sluice error |
| 2 | usage error |
| 3 | refused by `agent_hook_policy = "fail-fast"` |
| 4 | `sluice queue` request refused by the state of the queue |
| 75 | `sluice daemon` found another daemon holding the lock |
| 124 | gate timeout |
| 125 | `sluice exec` failed after the daemon accepted the request |
| 141 | `sluice exec` stdout is a closed pipe |
| 143 | gate killed (signal, `daemon stop --force`) |

A new code may be added for a new situation, or a former `1` refined into a new
documented code. Listed numbers keep their meaning; the agent hooks and the
plugin rely on `3`.

### Command line

Subcommand names, their flags and positionals as shown by `--help`, the `--`
separator of `sluice run`, the `--json` outputs and the `SLUICE_*` variables
are stable; human-readable text is not. New subcommands and optional flags may
appear. An old form that is going away keeps working for one release and prints
one line on stderr: `warning: <x> is deprecated, use <y> (removed in <N+1>)`.

### Plugin and MCP tools

The plugin registers the MCP server `sluice` (command `sluice mcp`), so tools
appear as `mcp__plugin_sluice_sluice__<tool>`; use those names in permission
rules. The tools `verify`, `status`, `gate_list`, `logs`, `queue_enqueue`,
`queue_status`, `queue_approve`, `queue_retry` and `queue_cancel` keep their
names, arguments and output fields; new tools and optional fields may appear.
The skill is named `sluice`; the `PreToolUse` guard (`sluice guard`, from 0.3.0)
rewrites gate and heavy commands to run through Sluice, denies raw gate
commands it does not rewrite and denies `sluice queue approve` (an older binary
only denies); the marketplace id is `sluice@sluice`.

The plugin (installed by Claude Code) and the binary (installed by
`install.sh` or `sluice update`) update independently. Each release's plugin
works with the previous binary for the tools that already existed, and the
binary works with the previous plugin; new tools appear once both are upgraded.

### Locations

State, runtime and config directories and the `markers/` directory keep working
across versions. A location may gain a fallback but does not move without a
migration. Known limitation: the fallback socket path used when the preferred
one is longer than 107 bytes (`/tmp/sluice-<uid>/daemon.sock`, from 0.3.0) is
not known to 0.2.0 binaries, which fail to bind such a path; a client and a
daemon of 0.3.0 agree with each other.

## Deprecation procedure

1. Release N: the old form works and warns once, naming the replacement and the
   release that removes it (N+1 at the earliest).
2. Release N+1: the old form stops, that interface's version number is bumped,
   the migration is described here and in the CHANGELOG, and the old golden
   files stay in a test that asserts the helpful refusal or the migration
   result.

## Proof

Golden files recorded from the real 0.1.0 and 0.2.0 release binaries live under
`crates/*/tests/fixtures/compat/<release>/`: configs, markers and miss records,
status and queue status JSON, protocol transcripts of every message, a database
after some history, the `--help` of every subcommand and the MCP tool list.
Compat tests make the current code read, replay or answer them. A change to a
frozen format fails one of them until a human changes a fixture on purpose;
fixtures are never edited to make a test pass, and a new release adds its own
directory. `scripts/compat/generate.sh` regenerates them from the release
archives.

## Reporting a compatibility break

If an upgrade or downgrade broke something this page says should work, please
open an issue at <https://github.com/josegarridodigio/sluice/issues> with:

- the two versions involved (`sluice --version` for each, and whether a daemon
  from the old version was still running: `sluice status`);
- what you did and what you expected;
- the output of `sluice doctor --bundle` (a redacted diagnostics bundle), and
  for a config problem the exact error line;
- for a database refusal, the error text and which release last wrote the file.

A break of a promise on this page is treated as a bug in the release that
introduced it, not as an expected consequence of upgrading.
