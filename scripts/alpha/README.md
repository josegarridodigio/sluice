# Alpha harness

Scenario scripts that exercise a real `sluice` binary against real projects.
No AI session is involved: agents are simulated by the CLI and by
`mcp_agent.py` (a scripted MCP client over stdio). Python 3 standard library
only. Plan: `kb/sluice/plan/alpha-plan.md` (section 6).

Every scenario runs in its own sandbox (`lib.py`): private HOME and XDG
directories, a short runtime directory `/tmp/sa-*` (the daemon socket path
must fit in 108 bytes), the binary under test first in `PATH`, fixed git
identity. The user's real daemon, cache and config are never touched. Each
scenario writes `<ALPHA_OUT>/<scenario>.jsonl` (one record per check) and ends
by asserting that no daemon or gate process is left behind.

## Scenarios

| Script | What it proves |
|---|---|
| `s1_hooks.py` | Real hook managers (husky, lefthook, pre-commit, plain) on a Node and a Python project |
| `s2_problem.py` | The original problem: N agents, one heavy suite per tree (`ALPHA_AGENTS`, `ALPHA_ITER`, `ALPHA_BURN_THREADS`, `ALPHA_BURN_SECONDS`) |
| `s3_correctness.py` | Invariants under concurrency, cache-key sensitivity, git state untouched, `CI` / `SLUICE_DISABLE` |
| `s4_chaos.py` | Daemon and client SIGKILL, 64-client hammering, CPU pressure (PSI), unwritable storage, corrupt markers and WAL (`ALPHA_HAMMER_SECONDS`, `ALPHA_HAMMER_CLIENTS`) |
| `s5_merge_queue.py` | Merge queue on the Node project with a bare remote: conflicts, failing gate split, human-only approval, audit ref, SIGKILL during the push and `reconcile`, gate reuse (`--transport local`, the default and the only one implemented here: the `--transport ssh|http` options exit with `not implemented yet`. Authenticated remotes, task H2, are covered by the Rust tests in `crates/sluice-gate/tests/queue_transports/`) |
| `s6_upgrade.py` | Upgrade across real releases with a live daemon and state (`ALPHA_TAGS`, `ALPHA_RELEASES`) |
| `s7_command_control.py` | Command control: 10 simulated agents through the guard hook (hook JSON on stdin, the returned command executed with `sh -c`) in `bypassPermissions` and `default` mode, a gate command answered from the cache, refusals (pipelines, `$(...)`, heredocs, `sluice queue approve`, deny rules), the shell-shim variant (`SLUICE_SHIMS=1` and off) and a raw baseline; budget 8, `exec_weight` 4, so at most 2 concurrent loads (`ALPHA_AGENTS`, `ALPHA_BURN_SECONDS`) |

Shared helpers: `lib.py` (sandbox, report, process census), `pygate.py`
(controllable Python gate and process helpers used by s3, s4 and s6),
`mcp_agent.py` (MCP client).

## Preparing the test beds (once)

Pick a scratch directory, for example `S=$HOME/scratch/alpha`.

```sh
mkdir -p $S/src $S/runs $S/out
# Node project: sindresorhus/slugify @ 3b17b2e (xo + ava), plus the hook managers s1 installs per checkout
git clone https://github.com/sindresorhus/slugify $S/src/node-slugify
git -C $S/src/node-slugify checkout 3b17b2e
(cd $S/src/node-slugify && npm install && npm install --no-save husky lefthook)
# Python project: pallets/itsdangerous @ 672971d (pytest + mypy)
git clone https://github.com/pallets/itsdangerous $S/src/py-itsdangerous
git -C $S/src/py-itsdangerous checkout 672971d
# The system has no venv module: install the tools with --target and point PYTHONPATH at them
pip install --target $S/pylib pytest mypy pre-commit freezegun
```

Scenarios clone these into their sandbox (with a bare origin) and link
`node_modules` per checkout (`link_node_modules`), so the test beds are never
modified.

## Environment

| Variable | Meaning |
|---|---|
| `ALPHA_TESTBEDS` | `$S/src` (contains `node-slugify` and `py-itsdangerous`) |
| `ALPHA_PYLIB` | `$S/pylib` |
| `ALPHA_OUT` | Directory for the `.jsonl` reports (default `.`) |
| `TMPDIR` | Where sandboxes are created, for example `$S/runs` (sandbox directories are kept; sockets live in `/tmp/sa-*` and are removed) |
| `SLUICE_BIN` | Binary under test (default: `sluice` in `PATH`) |
| `ALPHA_RELEASES` | s6: download cache for release archives (default: a new directory under `TMPDIR`) |
| `ALPHA_TAGS` | s6: release chain, default `v0.1.0,v0.2.0`; rerun at the release step with `v0.2.0,v0.3.0` |

s6 needs `gh` (authenticated, read access to `josegarridodigio/sluice`), `curl`
and `sha256sum`; it uses the real GitHub releases.

## Running

```sh
export PATH="$HOME/.cargo/bin:$PATH"
export ALPHA_TESTBEDS=$S/src ALPHA_PYLIB=$S/pylib ALPHA_OUT=$S/out TMPDIR=$S/runs
cd scripts/alpha
python3 s3_correctness.py   # about 30 s
python3 s4_chaos.py         # about 2.5 min (60 s of hammering, two PSI phases)
python3 s5_merge_queue.py    # about 2 min (needs the Node test bed only)
python3 s6_upgrade.py       # about 20 s
python3 s7_command_control.py  # about 1 min; needs `SLUICE_BIN` with exec, guard and shim (0.3.0+)
./run_all.sh                # every s*.py present, in order; exits non-zero if any fails
```

Each script prints one `[PASS]`/`[FAIL]` line per check and a final
`sN_name: x/y checks passed`; the exit status is 1 when any check failed.
`s4_chaos.py` generates CPU load (two busy loops per CPU for about a minute in
total) and must not share the machine with timing-sensitive work.

## Limitations recorded in the checks

- No root: a small tmpfs cannot be mounted, so "unwritable storage" is
  simulated with `chmod 0500` on the markers, state and logs directories.
- `cache.ttl_days` is an integer number of days: TTL expiry is exercised by
  setting the marker's mtime two days back with `ttl_days = 1`.
- v0.1.0 has no `sluice update`: s6 upgrades it with `scripts/install.sh`
  (`SLUICE_VERSION`), then uses `sluice update --version` from v0.2.0 on.
