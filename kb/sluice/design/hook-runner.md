---
title: "Design: the hooks entry point and the unit runner (waves H1, H2a, H2b, H3, H4)"
tags: [sluice, design, hooks, husky, lefthook, pre-commit]
---

# Design: the hooks entry point and the unit runner

Decision: [[sluice/design/adr/0021-sluice-runs-the-repository-hooks.md]]. Plan:
[[sluice/plan/hook-runner-plan.md]]. Research:
[[sluice/research/hook-managers-synthesis.md]]. Code: `crates/sluice-gate/src/hookentry/`.
User documentation: `docs/hooks.md`.

The feature was built in four waves and this note follows them: **H1** the entry point
(per-clone hooks directory, activation, delegation, drift), **H2a/H2b** the unit runner
(what Sluice runs itself and how it caches), **H3** hook units as gates. Read as one
system: git runs Sluice's script for a hook; `hooks run` decides **units** or
**delegation** at each run (an executable hook file must exist in the underlying
directory first); units go through the daemon with the cache; the same units are the
gates of `verify` in a repository without `.sluice.toml`. Wave H4 reconciled the
documentation with the code ([[sluice/changes/hook-runner-h4.md]]); where a section below
was written for an earlier wave it says so.

## H1: the entry point and delegation

H1 made Sluice the hooks entry point of a clone and delegated every hook untouched. The
delegation is still the default and the fallback of every later wave; since H2a it is
not what runs when Sluice understands the whole hook.

### Files (per clone, under `<git common dir>/sluice/`)

| File | Purpose |
|---|---|
| `hooks/<hook>` | one 0755 `sh` script for each hook of `githooks(5)` (`script.rs::ALL_HOOKS`, 28 names) that exists as an executable file in the underlying hooks directory, and no other |
| `hooks.gitconfig` | `[core] hooksPath = "<abs hooks dir>"`, included by the clone's config |
| `hooks.json` | `{schema, dir, previous, sluice}`: Sluice's directory, `core.hooksPath` before activation, the activating binary |

The mirror: git stops using `.git/hooks` once `core.hooksPath` is set, so a hook without a
script would stop running; but git fires `reference-transaction`, `post-index-change` and
`post-checkout` very often, and a script for a hook the repository lacks costs `sh` +
`sluice hooks run` + a `git config` read for nothing. So the scripts mirror exactly the
executable hooks of the underlying directory (`activate.rs::mirrored`, `reconcile`). Kept in
sync by: `on` (rewrites everything); `hooks run` (`activate::sync`, a few `stat` calls: add a
script for a new hook, never remove one, the current run unaffected); pruning happens only in
`on` and the restore, against the union of every worktree's hooks (`mirrored_in_clone`): the
worktrees share Sluice's directory while a relative underlying path (`.husky/_`, untracked)
resolves per worktree, so a run in a fresh linked worktree would otherwise delete every
script and silently stop the main worktree's hooks;
session-start restore (activates again when `Inspection::healthy` is false); doctor
(`Inspection::unmirrored` -> `fail`, "run `sluice hooks on`"). New hooks appear mainly via
husky's fixed 14 stubs or manual files; lefthook and pre-commit write elsewhere or refuse once
Sluice's value is set. Relative underlying paths resolve against the worktree root.

### Activation (`activate.rs`, `gitcfg.rs`)

1. Read `git config --show-origin --get-all core.hooksPath`; the previous value is the last
   one that is not Sluice's directory (a global value counts).
2. Write the mirrored scripts (remove stale ones), `hooks.gitconfig`, `hooks.json` (atomic, only when changed).
3. `git config --local --add include.path <hooks.gitconfig>`; if it is already listed,
   nothing is added. `--local` is the common config, so linked worktrees share it.
4. Verify: the last entry of `--show-origin --get-all core.hooksPath` has Sluice's value.
   If not (an `[include]` section before a `[core]` that holds a value), `git config --local
   --unset-all include.path` and add the other includes again in their order, ours last;
   verify again, else report the winner (`origin`, value).

Order matters because git reads the config top to bottom, expands includes in place and
applies the last `core.hooksPath`. husky's `git config core.hooksPath .husky/_` edits the
existing `[core]` key (or appends to the `[core]` section), which sits before our trailing
`[include]`, so the include keeps winning (measured with git 2.53; covered by a test).

`off`: `git config --local --fixed-value --unset-all include.path <file>`, then remove
`hooks/`, `hooks.gitconfig`, `hooks.json`. The repository's tracked files, other config
entries and `<git common dir>/sluice/markers` are never touched. When Sluice's include was
the only one, `.git/config` is byte-identical to its state before `on` (git drops the empty
section).

### Scripts (`script.rs`)

Run `exec <bin> hooks run --common-dir <common> <hook> -- "$@"`, with `<bin>` the binary that
activated the clone if still executable, else `command -v sluice` (an older `sluice` earlier
in `PATH` would not know `hooks run`). With no binary: the repository's own hook directly,
from `git config --file <common>/config --get-all core.hooksPath` (the local file only: a
husky rewrite is followed), else the value recorded at activation (baked into the script),
else `<common>/hooks`; executable file -> `exec`, otherwise `exit 0`. Paths are shell-quoted
at generation.

### Delegation (`delegate.rs`)

`hooks run` never fails a hook. (Describing H1's path; since H2a `runner::try_units` may run
the hook as units instead of the `exec` below, after the target was found to exist.) It reads
the `core.hooksPath` entries (if git cannot be
asked, the recorded previous value, with a stderr line), drops Sluice's own value, expands
`~/`, defaults to `<common>/hooks`, and `exec`s `<dir>/<hook>` when it is an executable
regular file (through `Command::exec`: same argv, stdin, cwd, environment; signals and exit
code are the hook's). Otherwise exit 0. A relative directory (husky's `.husky/_`) resolves
against the cwd git runs hooks in (the worktree root).

Per manager, all through this one rule because each manager's own entry file is what lives in
the directory git would have used: husky `.husky/_/<hook>` (missing stub: exit 0 as husky),
lefthook's generated script, pre-commit's stub (its `--hook-dir "$HERE"` legacy chaining still
works when the hook is delegated, because the stub runs in place; a hook run as units does not
chain `<hook>.legacy`, see the H4 change note), plain hooks. `detect()` classifies (husky: `.husky/_`
with `h`; lefthook: `LEFTHOOK` in a script, else a config file; pre-commit: its generation
marker, else `.pre-commit-config.yaml`; plain) for `status` and `doctor` only.

Deliberate non-goal, kept through every wave: calling `lefthook run <hook>` or `pre-commit
hook-impl ...` when the manager's script is not installed (and running `.husky/<hook>`
when `.husky/_/<hook>` is missing, which is what a fresh linked worktree has). That hook
did not run before; running it would be a behaviour change. `delegate::run` returns before
`runner::try_units` when no executable file exists, and the lefthook and pre-commit
adapters read the installed script, so units are only attempted for hooks that already
ran. Consequently ADR-0021's original statement that hooks now run in fresh linked
worktrees is wrong (amended in H4).

### Drift

- `doctor` (`check_hooks`): activated -> `ok` or `fail` with the reason (include missing,
  other value wins and where from, a repository hook without a script); not activated -> `ok` when a hook (or the lefthook or pre-commit
  configuration) calls sluice, `fail` when a manager owns non-calling hooks or gates with hooks meet non-calling
  hook files, `warn` for hooks files with no gates to run or gates with hooks and no hook files,
  `ok` when there is nothing to route.
- Session start: only a clone with `hooks.json` is looked at (found without spawning git: walk
  to `.git`, follow a worktree's `gitdir`/`commondir`); one `git config` read; if displaced,
  `git rev-parse` and `activate` in a worker bounded by the remaining 230 ms budget. The
  context gains one sentence; the log decision gets `hooks=on|restored|displaced`. Never
  activates a clone that was not activated.

### `sluice init`

`--hooks rewire` (default, unchanged) or `--hooks entry`: only `.sluice.toml` is written,
`hooks on` is run, the repository's hook files are not edited (`--dry-run` shows "would run
sluice hooks on"). With managed hooks and the default mode, a note offers `--hooks entry`;
`--hooks entry` with no existing hook file says so and does the usual setup.
init reads `core.hooksPath` without Sluice's value, so it works on an activated clone.

## H2a: units for husky and plain hooks

Decisions 3 to 6 and 8 of ADR-0021 for the two managers whose hooks are shell files. Code:
`hookentry/{grammar,classify,units,plan,runner}.rs`; the shared run pipeline is
`run.rs::run_unit`.

### Flow

`delegate::run` resolves the hook file exactly as in H1 (no executable file: exit 0, units are never
attempted), then calls `runner::try_units`. `None`
means "delegate as H1" and is returned, in this order, for: `SLUICE_HOOKS_DELEGATE` set,
`SLUICE_DISABLE` set, a non-UTF-8 environment, no git worktree, a `Plan::Delegate`, a repository
or daemon that cannot be prepared (`prepare`: `lookup::resolve_repo_or_implicit`,
`client::ensure_daemon`, `client::connect`). All of that is decided before the first unit starts,
so a hook is never half run by Sluice and then delegated.

### Model (`units.rs`)

`Plan = Delegate(reason) | Units(Program)`; `Program { lists: Vec<Vec<Unit>>, exit_ok }`, where a
list is an `&&` list (a plain command is a list of one). `Unit { manager, hook, index (from 1,
file order), command (the text `sh -c` runs), argv, cwd (worktree root), path_prepend, kind }`;
`Kind = Mutating | Check`; `Unit::name() = <manager>:<hook>#<index>`. An adapter for another
manager (H2b: lefthook, pre-commit) only has to produce a `Program` (a list per job, `argv`
or `command` per unit, its own `path_prepend`) and the runner, classification of kinds, caching
and fail-open paths are reused.

### Grammar (`grammar.rs`)

Whitelist per physical line: blank; `#` comment; first-line shebang for `sh`/`bash`
(`#!/bin/sh -e` means errexit); the two spellings of husky's `. "$(dirname [--] "$0")/_/husky.sh"`;
`set -e` before the first command; `exit 0` as the last command; an `&&` list. Words are bare
text (`[A-Za-z0-9_@%+=:,./-]`) or `'..'`/`".."` segments (no `$`, backtick, `\`, `!` inside
double quotes); `&&` must be a separate word. The first word must not be a `NAME=` assignment or
a shell word/builtin (`echo`, `cd`, `export`, `set`, `test`, `[`, `.` ...). Everything else is an
error whose text becomes the delegation reason (`line 3: uses a variable or a substitution ($)`);
`$1`/`$@` get their own wording because they are the likeliest request (hook arguments are not
part of a unit's identity yet: an extension for later). Deviations from the research note's
optional extensions: E2 (`FOO=1 cmd`) and E3 (`cd`) are not accepted; E1 and E4 are.

### Semantics (`runner.rs`)

Exactly `sh -e` over the file (husky runs the user file with `sh -e`; a plain hook is accepted
only with `set -e`, the husky bootstrap, or a single list, where the two coincide): an `&&` list
stops at its first failure; the hook ends with that unit's code only when the failing unit is the
last of its list; a non-final failure skips the rest of the list and the next line runs (errexit
does not apply to non-final members of an AND list, measured in the husky note); `exit 0` at the
end forces status 0. For husky the `husky - <hook> script failed (code N)` line (stdout) is printed.

### Execution

Each unit becomes a `Gate` (`gate_of`: `cmd` = the command text, `weight = auto`, no timeout,
`cache = (kind == Check)`, `env = {PATH: "<prepend>:<caller PATH>"}` when the manager prepends)
selected with `RepoContext::select_unit` (a gate that is not in `.sluice.toml`; the repository
needs no `.sluice.toml`: `resolve_repo_or_implicit`) and run by `run::run_unit`, which is
`sluice run`'s own pipeline (`run_resolved`): cache lookup, `Hit` line, daemon `run` request with
its queue, budget, containment and logs, the in-process fallback under the `flock` semaphore,
marker or miss record, `client.log` line. The only difference is the policy: a hook unit never
applies `agent_hook_policy` (`enforce_policy = false`; the hook runs what it would have run). The
daemon is asked for one unit at a time, in order; units are not queued together.

### Cache key of a check (decision 6)

The key is `lookup::derive_key_at` over `KeyMaterial { tree, gate_name = unit name, cmd, env =
{PATH...}, key_inputs = [], key_files = [] }`, `tree` the root working-tree snapshot
(`tree::snapshot_entries`: what `git add -A && git write-tree` would give, private index, nothing
written to the repository), taken in `lookup::lookup` right before the unit starts, hence after
the earlier units ran: the key describes what that unit actually saw, and a hook whose first
unit is a formatter keys the later checks on the formatted tree. The marker is written only if the
tree is unchanged after the run (daemon and in-process path both). An unknown tree (unmerged,
skip-worktree, submodules, git error) or a failed key derivation runs the unit uncached. Not in
the key: untracked toolchain inputs (`node_modules`, installed binaries), exactly as for gates
without `key_inputs`; the husky `PATH` string is a proxy for the toolchain location. The unit
name is in the key, so a unit does not share the entry of a `.sluice.toml` gate with the same
command (the gate name is part of `KeyMaterial`). H3 solved it without a key change: an `Exact`
match runs the unit under the gate's own name and definition (see "Sharing with `.sluice.toml`").

### Environment

A husky or plain check gets `git::child_env` (no `GIT_DIR`, `GIT_INDEX_FILE`, ..., no `CLAUDE*`),
like a gate: required for a key that says "working tree". A mutating unit keeps git's variables
(only `CLAUDE*` removed) so that `git add` and `lint-staged` edit the index the commit is using,
also with `git commit -a` or paths (a temporary `GIT_INDEX_FILE`). Since H2b every lefthook and
pre-commit unit keeps them too (the managers ask git what is staged).

### Output and policy

A unit runs with `verbose = false`: its output goes to the job log, the terminal gets the summary
line and, on failure, the last `tail_lines` lines (`report`). `run_unit` runs with `enforce_policy
= false`, so neither `agent_hook_policy` nor `human_hook_policy` applies: an agent's commit on a
miss runs or waits for the units inside `git commit` (the "agents never hang" property of
`sluice run --hook` does not hold for units; the plugin tells agents to `verify` first, which warms
the keys). A nested `sluice run` inside a unit (a lefthook or pre-commit configuration wired by
`sluice init`) runs directly because `SLUICE_JOB` is set; see the H4 change note for why that
configuration is not delegated.

### Classification (`classify.rs`)

Check is a whitelist (test runners, linters, type checkers, build tools, formatters in check
mode, `cargo`/`go`/`ruff`/`black`/`isort`/`dotnet`/`terraform` subcommands, package scripts whose
bodies, `pre` and `post` scripts (through `sluice_gate_core::pkgscripts`) are all checks, wrappers
`npx`/`pnpm exec`/`yarn dlx`/`bunx` looked through). Write flags (`--fix`, `--write`, `-w`, `-i`,
`--staged`, `--cached`, `-u`, ...) make any command mutating. Unknown programs, `git`,
package-manager installs, `npm|pnpm|yarn audit` (network advisory database, not the tree: never cached), redirections in script bodies: mutating.

Stdin: units run without the hook's stdin, so a hook in `STDIN_HOOKS` (pre-push, post-rewrite,
reference-transaction, pre-receive, post-receive, proc-receive) is run as units only when every
unit is a `Check` (`plan::check_stdin`); otherwise it is delegated with the offending command in
the reason.

### Fail open

Before the first unit: delegation (above). After it: a failing `select_unit` runs that unit
directly with `sh -c`; a daemon that dies or refuses uses `run_queued`'s in-process fallback
(budget semaphore) with its stderr line.

### Status

`hooks status [--json]` gains one entry per hook the repository has (`plan::plan` with the same
inputs a run would use; `SLUICE_HOOKS_DELEGATE`/`SLUICE_DISABLE` shown as the reason). No doctor
change in H2a (H3 and the drift section below cover doctor).

## H2b: units for lefthook and pre-commit

Two adapters (`hookentry/lefthook.rs`, `hookentry/precommit.rs`) that only produce a
`Program`; the runner, cache, daemon path and fail-open paths are H2a's. Shared helpers:
`tooling.rs` (PATH lookup, `capture`, version parse, shell words, SHA-256), `precond.rs`.

### Model changes (`units.rs`)

`Unit` gains `git_env` (keep git's variables: true for mutating units as before, and for
every lefthook and pre-commit unit, because those managers ask git what is staged),
`key_inputs` and `key_files` (the gate fields of the same name: tool version, configuration).
`Program` gains `keep_going` (run every unit, fail at the end) and `precondition`
(`Precondition::{StagedNotPartial, WorktreeEqualsIndex}`, checked by `precond::check` from
`git status --porcelain -uno` in the hook's own environment, right after planning and before the
daemon is contacted; false means delegate). `runner::execute_all` implements `keep_going`: code 0
goes on, code 1 is remembered, any other code ends the hook with it; the hook exits 1 if any unit
returned 1.

### lefthook

`plan` order: hook gate (`pre-push` and the git-lfs hooks delegate), environment (`LEFTHOOK=0|false`,
`LEFTHOOK_EXCLUDE`), binary (`$LEFTHOOK_BIN`, else `lefthook` on `PATH`; the script's other discovery
routes delegate), `lefthook version` (2.x from 2.2.2), `lefthook dump --format json`, hook options,
jobs. Selection is `--command <name>`: v2.2.2 merges it into `--job` and matches job names, for jobs
and commands alike. **Skip vs failure**: `RunHook` fails with `no job matching` when every selected job is
skipped, and a job skipped for no files cannot be told from a failure (both exit 1), so a hook is
only run as units when no selected job can be skipped: no `skip`/`only`/`exclude_tags`/
`LEFTHOOK_EXCLUDE`, no file template (`{word}`) or filter (`glob`, `exclude`, `root`, `files`,
`file_types`, hook-level `files`/`exclude`), and for `pre-commit` something staged
(`build_command.go:53-69` skips a command with no staged file that matches). Partial staging is
the other `pre-commit` per-invocation effect (`guard.go`: hides the unstaged half, runs, restores):
the precondition requires that no file be partially staged, so the guard is a no-op and the worktree
equals what the key describes. `stage_fixed` delegates (staging happens after all jobs; per-command
runs would stage between jobs and the guard would run N times). Order: `jobs` as written, then
`commands` as `CommandsToJobs` sorts them (priority ascending, zeros after; then leading number,
then name); ties are an unstable sort there, so they delegate; non-ASCII names delegate.
Kind: `grammar::parse(run)`, every command `classify`-ed `Check`, no `env` value with `$`.
Key: tree snapshot + unit name + command + `key_inputs = [<bin> version, printf %s <sha256 of
dump>]`. The dump hash covers `extends`, `lefthook-local` and cloned remotes (remotes delegate
anyway); script files are not covered because scripts delegate.

### pre-commit

`plan` order: hook (`pre-commit`, `pre-merge-commit` only; the others need refs, stdin or a message
file), `SKIP`, the stub in the underlying hooks directory (`INSTALL_PYTHON`, `--config=`,
`--hook-type=`, nothing else), launcher (`INSTALL_PYTHON -mpre_commit` while executable, else
`pre-commit` on `PATH`), YAML through `serde_norway` into JSON values (merge keys, non-string
scalars where a string or boolean is needed, unknown keys: delegate), stage membership, ids. The
stash decision is `Precondition::WorktreeEqualsIndex`: `run` without `--files` stashes on every
invocation (`staged_files_only`), so the per-id units are only used when it would be a no-op;
otherwise delegate and let pre-commit stash once. A skipped hook exits 0, so a cached pass would be
a lie unless the hook cannot be skipped: `always_run: true` is required for a check. Remote hooks:
placed only with explicit `stages` in the config entry (the manifest is in a clone Sluice does not
read), uncached.

### Not modelled, on purpose

lefthook: skip detection by output, `scripts`, groups, `setup`, `piped`, remotes, binary routes other
than `LEFTHOOK_BIN` and `PATH`, `pre-push`. pre-commit: the manifest of remote hooks (so remote hooks
without `stages` delegate), `pre-push` (first-usable-ref rule and from/to refs), commit-msg style
stages, `fail_fast`, `meta`. Each is a delegation with its reason in `hooks status`; a later wave can
lift one at a time with measurements.


## H3: hook units as gates (`verify`, `gate_list`, `sluice run`, `sluice gate list`)

ADR-0021 decision 7. Code: `hookentry/gates.rs` (derivation and sharing), `sluice-gate-core`
`hookgate.rs` and `lookup::resolve_repo_hook_gates` / `hook_config` / `resolve_hook_gates`,
the MCP server (`serve(env, HookGateSource)`), `gate_list.rs`, `run.rs::resolve`, `doctor.rs`,
`session_start.rs` (`Place::Hooked`).

### Layering

The planning lives in the `sluice` binary (it owns the adapters); the MCP crate cannot depend on
it, so `sluice_gate_mcp::serve` receives a `HookGateSource` (`Fn(&RepoPaths, &EnvMap) -> HookGates`)
and `sluice-gate-core` defines the data (`HookGate {name, manager, hook, gate}`, `HookExclusion`,
`HookGates`). A repository **without** `.sluice.toml` whose derivation yields at least one gate gets
`RepoConfig::implicit()` plus those gates (`gate.hooks = [hook]`, so the existing default selection
"the `pre-commit` gates, else all" applies); with none, `ConfigNotFound` as before (its message now
also points to `sluice hooks status`). With a `.sluice.toml` nothing is derived: its gates stay as they
are.

### Decisions

1. **Which hooks**: `pre-commit`, `pre-merge-commit`, `pre-push` (`gates::GATE_HOOKS`), planned by the very
   `plan::plan` a hook run uses, with the hooks directory of `hooks status` (the repository's own
   `core.hooksPath` without Sluice's value), whether or not the entry point is on. `commit-msg` and
   `prepare-commit-msg` receive the message file: excluded with that reason. Delegated hooks
   (`Plan::Delegate`) contribute nothing and are listed with the reason. Only `Kind::Check` units are
   gates; a `Mutating` unit is excluded ("may change files or the index"): `verify` must never edit
   the user's files or index outside a commit. A hook with a mutating unit still contributes its checks;
   a check after a mutating one is keyed (at the hook) on the tree after the mutation, so a `verify` pass
   hits only when the mutation was a no-op: a miss, never a wrong hit.
2. **Names**: unchanged, `<manager>:<hook>#<index>` (`Unit::name`). A command-derived scheme was
   considered and rejected: two units can share a command, commands are long and contain characters
   gate names avoid, and H2's keys and tests already hash this name. The cost: editing a hook renumbers the
   units after the edit and costs one miss.
3. **Key equality**: a hook gate *is* `runner::gate_of(unit, key_env)`, the gate the hook runner builds;
   `verify` resolves it through `RepoContext::select` and the hook through `select_unit`, both then
   `lookup::lookup` / `derive_key_at` over the same `Gate` and the same working-tree snapshot. What differs
   is the environment around the run, so the only environment-dependent key input needed care:
   **`PATH`** (husky hashes `gate.env["PATH"]`). git puts its exec path (`git --exec-path`) first in
   the `PATH` of every hook; `runner::key_env` removes exactly that leading entry from the string that is
   hashed, while the process still runs with the full `PATH` (`select_unit` gained `run_env`, applied after
   `gate.env`). Both sides use `key_env`. An agent whose `PATH` differs from the commit's `PATH` gets a
   miss (documented limitation: the `PATH` is the toolchain proxy chosen in H2a).
   lefthook (`key_inputs`: version, dump hash) and pre-commit (`key_inputs` version, `key_files` config)
   keys have no environment input.
4. **git env**: `verify` runs units without git's hook variables (`select` = `git::child_env`). Mutating
   units are never gates; lefthook and pre-commit keep `git_env` in hook runs because they ask git for the
   staged files; from `verify` there is no commit in progress, so they read the real index, which is the
   same index except for `git commit -a`/`<paths>` (a temporary index): in that case the hook sees a different
   tree and the key differs (a miss). Their `key_inputs` commands do not read git's variables.
5. **Preconditions** are evaluated when the gates are derived (`precond::check` in the caller's
   environment): lefthook's `StagedNotPartial` and pre-commit's `WorktreeEqualsIndex` depend on the staged set
   (pre-commit would stash unstaged changes: it must not run from `verify`). When the condition does not
   hold now, those units are excluded with the reason and are gates again once it holds (stage the change,
   then verify). `gate_list` is therefore a snapshot of "what can be verified now".
6. **Output**: `gate_list` (MCP) and `sluice gate list [--json]` print each gate (`source: hook`, `manager`,
   `hooks`, `cmd`, `cache`) and `excluded` (`hook`, `unit`, `command`, `reason`); `verify` reports the units
   like gates. `sluice gate list` is new (the lead's brief named it; it did not exist). `sluice run <gate>`
   (the gate stays a required argument) accepts the names; to avoid planning every hook, the hook is read
   from the name (`gates::hook_of_name`). `cache status` and the MCP `status` cache section still read
   `.sluice.toml` only (not extended). `doctor`: `repo-config` is `ok` ("gates are derived from the
   hooks (n units can be verified, m left out)") when there is no `.sluice.toml` and the entry point is
   active; with the entry point off it still fails.
7. **Agent integration**: `session_start::Place::Hooked` (no config, `hooks.json` in the common dir, found
   without spawning git) gives the rules with a sentence about derived gates (`rules-hooks` in the log); the skill,
   its reference, the plugin README and the MCP instructions say that such repositories verify through
   Sluice, that a `verify` pass warms the commit's hook, and that an excluded unit is left to the hook.
   No rule was loosened.

### Sharing with `.sluice.toml` (exact or not at all)

`gates::share(unit, config)` returns `Exact(gate name, gate)`, `Differs(gate, reason)` or `NoMatch`.
`runner::identity` runs an `Exact` unit under the gate's name and definition (timeout zeroed: a hook has none;
weight kept), so the gate's key equals the unit's. Conditions, all required: the unit is a check with
no git variables and no `PATH` addition; exactly one gate has the same command text (trimmed, exact);
the gate is cached, `mode = "gate"`, not path-scoped (its key would cover fewer files than the hook
sees) and has no `env` (the hook would run with variables its repository does not set). Consequently
husky units never share (the `PATH` addition), lefthook/pre-commit units never share (`git_env`; their
command is `lefthook run ...`), plain-hook units can. `hooks status` prints `cache shared with gate X` or
`same command as gate X, cache not shared: <reason>`.
