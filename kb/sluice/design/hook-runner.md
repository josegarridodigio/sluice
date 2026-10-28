---
title: "Design: the hooks entry point (wave H1)"
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
