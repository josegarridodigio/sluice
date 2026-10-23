---
title: "Change: init keeps rewired hooks working without Sluice and writes them atomically (v0.4.1 F1, F2)"
tags: [sluice, change, init, hooks, fix]
---

# Change: hook fallback and atomic writes in `sluice init` (v0.4.1, F1 and F2)

Fixes F1 and F2 of [[sluice/plan/v0.4.1-plan.md]], found rolling 0.4.0 out to
a real project with husky. Decision amended:
[[sluice/design/adr/0016-init-detection-and-hooks.md]]; earlier work:
[[sluice/changes/init-detection.md]].

## F1: fallback when Sluice is absent

A line rewired to `sluice run --hook pre-commit lint` failed with "command not
found" for a teammate without Sluice. Now:

- Shell hooks (husky, plain, `.githooks/`, lefthook `run:`) get
  `if command -v sluice >/dev/null 2>&1; then sluice run --hook <hook> <gate>;
  else <original>; fi`. The indentation, the line terminator, the original text
  (leading `NAME=value` words included) and an `|| exit` tail (kept after
  `fi`) are preserved. The exit status of the branch that ran is the line's
  status, also under `sh -e` (errexit applies inside the branches).
- The pre-commit framework (`entry:` with `language: system`) gets the same
  through `sh -c '...'`; `pass_filenames: false` is kept. `language: script` is
  no longer rewired (the entry is a path, not a shell command): it is printed.
- A command that cannot be nested (a single quote in a pre-commit entry, `: `
  or ` #` in a YAML value) is left alone and the snippet is printed, now in the
  fallback form where the original is known.
- Files created by `init` (`.githooks/pre-commit`, `.githooks/pre-push`) have
  no original command: plain `sluice run` lines, guarded at the top by
  `command -v sluice >/dev/null 2>&1 || exit 0`. The `plain` snippet of
  `sluice hook print` has the same guard.
- Idempotence: the scanner recognises the fallback form (husky/plain, lefthook,
  `sh -c` entry) as a wired line with its gate. A bare line that is exactly
  `sluice run --hook <hook> <gate>` (the 0.4.0 form) is upgraded by
  `init --force`, taking the fallback from the existing gate's `cmd` with its
  `env` words in front (skipped when the `cmd` is not a simple command or an
  env value needs quoting). A second `--force` changes nothing.

Example, `.husky/pre-commit` before and after:

```sh
npx pretty-quick --staged
ng lint
```

```sh
npx pretty-quick --staged
if command -v sluice >/dev/null 2>&1; then sluice run --hook pre-commit lint; else ng lint; fi
```

## F2: atomic writes

`write_changes` used `fs::write`, so a hook being executed by `sh` could read a
half-written file. New module `crates/sluice-gate/src/init/atomic.rs`
(`atomic::write`): temporary file `.<name>.sluice-tmp-<pid>-<n>` in the same
directory (`create_new`, mode 0600), all bytes, `sync_all`, the target's mode
(existing file keeps its exact mode, a new hook 0755, a new config 0644),
`rename` over the target. A symlink is resolved first, so the link stays and
its target is replaced. On any error the temporary file is removed. No
`unsafe`.

## Files and symbols

- `crates/sluice-gate/src/init/hooks.rs`: `Candidate::original`,
  `Found::Wired { gate, upgrade }`, `Fallback`, `parse_fallback`,
  `parse_sh_c`, `render_wired`, `is_bare`; `language: script` is manual.
- `crates/sluice-gate/src/init/mod.rs`: `hook_edits` (replacement per format,
  `gate_original` for upgrades), `gates_from_hooks`, `script_for` (guard),
  `Manual::original`, `write_changes` uses `atomic::write`.
- `crates/sluice-gate/src/init/atomic.rs`: new.
- `crates/sluice-gate/src/snippets.rs`: `GUARD`, `Wire`, `fallback_shell`,
  `fallback_sh_c`, `snippet_with`; `snippet` delegates.
- Tests: `crates/sluice-gate/tests/init.rs` (fallback lines per manager, a
  rewired husky hook run with `sh -e` with and without a fake `sluice` on
  `PATH`, upgrade of 0.4.0 lines for four fixtures, a command that cannot be
  nested, inode/mode/symlink/no-temp-file checks), unit tests in `hooks.rs`,
  `atomic.rs` and `snippets.rs`.
- Docs: `docs/cli.md` (init), ADR-0016, CHANGELOG.

## Verification

`sluice run fmt`, `clippy`, `test` through Sluice (see the commit).
