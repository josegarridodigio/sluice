---
title: "Change: zero-config proof, harness scenario s8 (E1)"
tags: [sluice, change, proof, harness, zero-config, shims, init, scheduler]
---

# Change: harness scenario s8 "zero config" (2026-10-23)

Task E1 of [[sluice/plan/zero-config-plan.md]] (section 7). Replays the incident of
section 1 on a fixture, through the real binary, the real plugin hook script and the real
daemon. **Claude Code is not involved**: the "agent" is a script that sets `CLAUDECODE=1`
and sources the session's `CLAUDE_ENV_FILE` before every shell command, as Claude Code does
(`bash -c '. $CLAUDE_ENV_FILE; <command>'`). The real-session proof is E2
(`scripts/plugin-e2e.sh`, haiku).

## Method

`scripts/alpha/s8_zero_config.py` (Python 3, stdlib only, one JSONL record per check through
`lib.Report`, listed in `scripts/alpha/README.md`, picked up by `run_all.sh`). It builds the
release binary of the checkout (`cargo build --release -p sluice-gate`) unless `SLUICE_BIN` is set.

1. **Sandbox**: fresh HOME and `XDG_*`, private PATH (sandbox `bin`, fake `npm`/`npx`, system
   PATH). The only config is `[daemon] budget = 8` in the sandbox user config. That is *test
   infrastructure* (so that a few fake jobs saturate the daemon); the product needs no user
   config, and a check asserts that file holds nothing else. Everything else is default:
   overcommit 1.5 (limit `floor(1.5 x 8) = 12` admitted units), nice 10, auto ceiling 4.
2. **Session start**: `plugin/hooks/session-start.sh` with SessionStart hook JSON on stdin and a
   temp `CLAUDE_ENV_FILE`, twice (`startup` before `sluice init`, `resume` after).
3. **Fixture "angular-like"** (generic names): fake `ng` (node, `node_modules/.bin/ng`): `build`,
   `test` burn 4 busy worker threads for 3 s, `lint` 1 thread for 1 s, each writes an
   `invocations.log` line and prints a marker line (`build -c fail` exits 7 with stderr);
   `serve` and `test --watch` print a marker and sleep (dev-server stubs). Fake `playwright`
   (3 threads), fake `pretty-quick`, fake `npm` (runs `package.json` scripts through `sh` with
   `node_modules/.bin` first) and fake `npx` (local bin first). `package.json` scripts as in
   the plan (`test`, `build`, `axe` -> `axe:a` -> `playwright test`, `start`, `lint`),
   `angular.json`, `scripts/build.sh` (`npx ng build`). Husky 9 shape: `.husky/pre-commit` (`npx
   pretty-quick --staged`, `ng lint`, `ng test --watch=false`), `core.hooksPath=.husky/_` and a
   `.husky/_/h` runner that does `export PATH="node_modules/.bin:$PATH"` (so a bare `ng` in
   the hook resolves to the local tool, never to a shim, exactly why `init` must rewire it).
4. **Init**: `sluice init --dry-run` (tree, modes, mtimes, `git status`, `core.hooksPath`
   compared before and after), then `sluice init`.
5. **Learning**: `weight = "auto"` `test` gate, five runs on five trees with the fake using one
   thread; the admitted weight is read from `sluice status --json` while each runs.
6. **Heavy phase**: first a raw baseline with `SLUICE_SHIMS=0`, then, on the same
   fixture, eight agent commands launched concurrently plus a commit sequence:
   `npx ng test --watch=false`, `(time npx ng build -c mock) > out-time.log 2>&1`,
   `x=$(npx ng build); echo "captured: $x"`, `sh -c 'npm run axe'`, `./scripts/build.sh`,
   `npx ng build -c fail`, `npm start` and `npx ng test --watch` (killed after 3 s), and
   `git commit` (refused by the rewired hook, `sluice run lint`, `sluice run test`, commit).
   A sampler polls every 50 ms: `sluice status --json` (sum of admitted `weight_effective`),
   and `/proc` for every process in the process group of each job leader (a child of the daemon
   that leads its own group) plus its descendants, and every thread of those processes: their
   nice (field 19 of `stat`) must equal 10; a process is a violation only when seen wrong twice,
   150 ms apart (the `nice`/`ionice` wrapper itself is skipped, it is wrong for microseconds
   before it `exec`s). Status JSON exposes no process group, so the job leaders are found from the
   daemon pid.

## Results (release binary, 20 CPUs, run of 2026-10-23)

37 of 37 checks pass (`ALPHA_OUT` JSONL: `s8_zero_config.jsonl`). Highlights:

| Claim | Observed |
|---|---|
| Shims installed by the hook | 32 shims in `$XDG_DATA_HOME/sluice/shims` (includes `ng`, `npm`, `npx`, `playwright`) |
| PATH entry after startup + resume | 1 line in the env file; in an agent shell the shim dir is first, once; `npm` resolves to the shim |
| Context variants | repo without `.sluice.toml`: init proposal ("only after the user agrees"); after init: rules plus `Gates of this repository: lint, test` |
| `init --dry-run` | writes nothing; shows gates `lint` (weight 2) and `test` (`"auto"`) and the two rewired lines |
| `init` | `.sluice.toml` gates exactly `lint`, `test`; hook becomes `npx pretty-quick --staged` / `sluice run --hook pre-commit lint` / `... test`; husky wrapper untouched |
| `weight = "auto"` learning | admitted weights over five runs: 4, 4, 4, 1, 1 (ceiling `ceil(8/2)` = 4; after 3 runs 1 for a 1-thread fake); `weight_auto` true in status |
| Raw baseline | `SLUICE_SHIMS=0`: totals `runs` unchanged (escape hatch), 3.4 s wall, every command ran |
| Heavy commands queued | 4 x `exec:ng build` (subshell with `time`, `$(...)`, script, failing) and `exec:run axe` in the timeline; `npx ng test --watch=false` equals the `test` gate's cmd, so the shim ran the gate `test` (cache path) |
| Dev servers | `npm start`, `npx ng test --watch`: not in the timeline, ran direct (marker printed), killed by the agent (rc -9 in both runs) |
| Concurrency | peak admitted weight 9 (limit 12), 3 jobs running, up to 4 queued; wall 9.3 s against 3.4 s raw |
| Outputs and exit codes | `$(...)`, `sh -c`, script, failing build: stdout and exit code identical to raw; failing build exit 7 propagated, its stderr replayed; `(time ...) > log`: marker and `real` in the log |
| Nice | 11 processes and 66 extra threads of jobs sampled over about 9 s, 344 observations, all nice 10, no violation |
| Hook through Sluice | first `git commit`: stopped by the hook (`agent_hook_policy` fail-fast, client.log `hook=pre-commit gate=lint decision=miss-fail-fast`); after `sluice run lint` / `test` the second commit succeeds with `lint:hit`, `test:hit`; `PRETTY-QUICK-MARKER` printed directly |
| Leftovers | none (daemon and fake tool census empty) |

Run the scenario: `ALPHA_OUT=<dir> python3 scripts/alpha/s8_zero_config.py` (about 1 minute).

## Behaviours worth knowing (not failures)

- A command that is **exactly a gate's cmd** is routed by the shim to `sluice run <gate> -v`
  (plan T1, point 4): the agent gets exit code and the gate log on stderr, and stdout holds only
  the one-line verdict `sluice: test PASS ...`. Capturing it with `x=$(npx ng test --watch=false)`
  therefore captures the verdict, not the tool's stdout. The scenario checks this explicitly
  (exit code equal, marker visible on stderr). Commands that are not a gate's cmd (builds, axe)
  keep the tool's own stdout and stderr.
- With the default `agent_hook_policy`, an agent's `git commit` is refused until the tree
  has passed (`verify` or `sluice run <gate>`); the scenario follows that flow.
- Overcommit lets the daemon admit more than the budget (peak 9 of a budget of 8, limit 12)
  when the machine is calm.

## Side effect on s7

s7 (`s7_command_control.py`) asserted at most `budget / exec_weight` = 2 concurrent exec jobs.
With 0.4.0 `sluice exec` weights are learned per repository and label (D9): its loads use about
one CPU, so after three runs of a label the weight drops from 4 to the floor 1 and up to 8 loads
run at once (peaks 3, 8, 6 on the integration branch). This is the intended product behaviour,
not a bug; the harness assumption is outdated. s7's sandbox config now sets
`[scheduler] learned_weights = false` (comment in the docstring), after which s7 passes 64/64.

## Product bugs found

None. No check was weakened to hide a failure.

## Verification

`python3 scripts/alpha/s8_zero_config.py` 37/37 and `python3 scripts/alpha/s7_command_control.py`
64/64 against the release binary of this checkout (test beds for s7: slugify @ 3b17b2e rebuilt
in a scratch directory).
