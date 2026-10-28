---
title: "Design: the hooks entry point and the unit runner (waves H1, H2a)"
tags: [sluice, design, hooks, husky, lefthook, pre-commit]
---

# Design: the hooks entry point (wave H1)

Decision: [[sluice/design/adr/0021-sluice-runs-the-repository-hooks.md]]. Plan:
[[sluice/plan/hook-runner-plan.md]]. Research:
[[sluice/research/hook-managers-synthesis.md]]. Code: `crates/sluice-gate/src/hookentry/`.

H1 makes Sluice the hooks entry point of a clone and delegates every hook untouched. No
unit parsing, no caching, no behaviour change.

## Files (per clone, under `<git common dir>/sluice/`)

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

## Activation (`activate.rs`, `gitcfg.rs`)

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

## Scripts (`script.rs`)

Run `exec <bin> hooks run --common-dir <common> <hook> -- "$@"`, with `<bin>` the binary that
activated the clone if still executable, else `command -v sluice` (an older `sluice` earlier
in `PATH` would not know `hooks run`). With no binary: the repository's own hook directly,
from `git config --file <common>/config --get-all core.hooksPath` (the local file only: a
husky rewrite is followed), else the value recorded at activation (baked into the script),
else `<common>/hooks`; executable file -> `exec`, otherwise `exit 0`. Paths are shell-quoted
at generation.

## Delegation (`delegate.rs`)

`hooks run` never fails a hook. It reads the `core.hooksPath` entries (if git cannot be
asked, the recorded previous value, with a stderr line), drops Sluice's own value, expands
`~/`, defaults to `<common>/hooks`, and `exec`s `<dir>/<hook>` when it is an executable
regular file (through `Command::exec`: same argv, stdin, cwd, environment; signals and exit
code are the hook's). Otherwise exit 0. A relative directory (husky's `.husky/_`) resolves
against the cwd git runs hooks in (the worktree root).

Per manager, all through this one rule because each manager's own entry file is what lives in
the directory git would have used: husky `.husky/_/<hook>` (missing stub: exit 0 as husky),
lefthook's generated script, pre-commit's stub (its `--hook-dir "$HERE"` legacy chaining still
works because the stub runs in place), plain hooks. `detect()` classifies (husky: `.husky/_`
with `h`; lefthook: `LEFTHOOK` in a script, else a config file; pre-commit: its generation
marker, else `.pre-commit-config.yaml`; plain) for `status` and `doctor` only.

Deliberate non-goal in H1: calling `lefthook run <hook>` or `pre-commit hook-impl ...` when the
manager's script is not installed. That hook did not run before; running it would be a
behaviour change. It belongs to H2, where Sluice runs units itself.

## Drift

- `doctor` (`check_hooks`): activated -> `ok` or `fail` with the reason (include missing,
  other value wins and where from, a repository hook without a script); not activated -> `ok` when a hook calls
  sluice, `fail` when a manager owns non-calling hooks or gates with hooks meet non-calling
  hook files, `warn` for hooks files with no gates to run or gates with hooks and no hook files,
  `ok` when there is nothing to route.
- Session start: only a clone with `hooks.json` is looked at (found without spawning git: walk
  to `.git`, follow a worktree's `gitdir`/`commondir`); one `git config` read; if displaced,
  `git rev-parse` and `activate` in a worker bounded by the remaining 230 ms budget. The
  context gains one sentence; the log decision gets `hooks=on|restored|displaced`. Never
  activates a clone that was not activated.

## `sluice init`

`--hooks rewire` (default, unchanged) or `--hooks entry`: only `.sluice.toml` is written,
`hooks on` is run, the repository's hook files are not edited (`--dry-run` shows "would run
sluice hooks on"). With managed hooks and the default mode, a note offers `--hooks entry`.
init reads `core.hooksPath` without Sluice's value, so it works on an activated clone.

## H2a: units for husky and plain hooks

Decisions 3 to 6 and 8 of ADR-0021 for the two managers whose hooks are shell files. Code:
`hookentry/{grammar,classify,units,plan,runner}.rs`; the shared run pipeline is
`run.rs::run_unit`.

### Flow

`delegate::run` resolves the hook file exactly as in H1, then calls `runner::try_units`. `None`
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
command (the gate name is part of `KeyMaterial`); sharing needs a key without the name and is
left for H3.

### Environment

A check gets `git::child_env` (no `GIT_DIR`, `GIT_INDEX_FILE`, ..., no `CLAUDE*`), like a gate:
required for a key that says "working tree". A mutating unit keeps git's variables (only
`CLAUDE*` removed) so that `git add` and `lint-staged` edit the index the commit is using, also
with `git commit -a` or paths (a temporary `GIT_INDEX_FILE`).

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
change.
