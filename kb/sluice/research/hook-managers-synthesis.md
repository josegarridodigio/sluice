---
title: "Hook managers: synthesis of the source analyses (husky, lefthook, pre-commit)"
tags: [sluice, research, hooks, husky, lefthook, pre-commit]
---

# Hook managers: synthesis of the source analyses

Sources: [[sluice/research/hook-managers-husky.md]] (husky v9.1.7),
[[sluice/research/hook-managers-lefthook.md]] (lefthook v2.2.2),
[[sluice/research/hook-managers-pre-commit.md]] (pre-commit v4.7.0). Every fact below is
cited there (file:line). Trigger: a report that husky hooks bypass Sluice because husky
prepends `node_modules/.bin` to `PATH` after its only extension point (`init.sh`), in a
repository whose hook files cannot be edited. Question: can Sluice become the runner of
a repository's existing hooks without modifying the repository?

## Facts that hold across the three

| | husky 9 | lefthook 2 | pre-commit 4 |
|---|---|---|---|
| Who sets `core.hooksPath` | husky, on every install (`index.js:14`), silently overwriting a local value | never sets it; refuses to install into a local one (logs only), except `install --force` / `--reset-hooks-path` | never; `install` **fails** when it is set in any scope |
| Survives a Sluice-owned `core.hooksPath` | yes if Sluice's value comes from an `[include]` placed last in `.git/config` (measured: last value wins after husky's write) | yes | yes, but `pre-commit install` then errors loudly |
| Full-fidelity delegation of a whole hook | `.husky/_/<hook> "$@"` from the repo root, stdin forwarded | the hook script (`lefthook run <hook> "$@"` plus binary discovery/`rc`) | `pre-commit hook-impl --config=<cfg> --hook-type=<t> -- <args>` (stdin for pre-push) |
| Unit of work | a line of a shell script (no named units) | named jobs/commands: `lefthook run <hook> --command <name>` (`--job`), with templates, globs, env, skip rules, tags | hook ids: `pre-commit run <id> --hook-stage <stage>` |
| Units runnable one by one by the manager itself | no (Sluice must parse lines) | yes, for named jobs (`--no-auto-install --skip-lfs`, `--no-stage-fixed` if Sluice stages); a skipped command exits 1 ("no job matching"); exit codes only 0/1 | yes, but it clones every remote repo of the config on first use, ids are not unique, and without `--files` it stashes unstaged changes per invocation |
| Touches the working tree / index | `lint-staged`, `pretty-quick --staged`, `git add` lines re-stage files | `pre-commit` hooks stash and discard unstaged changes of partially staged files; `stage_fixed` runs `git add --force` after all jobs | stash: `git checkout -- .` and temporary removal of intent-to-add entries; a hook that changes the diff fails the run |
| PATH | prepends relative `node_modules/.bin` | does not touch it (`sh -c`) | per-language environments |
| What decides the files a unit sees | the script itself | `{staged_files}`, `{push_files}`, `glob`, `files`, `root`... (index, HEAD, remote refs) | `files`/`exclude`/`types`, the stash, `--from-ref/--to-ref` |

## Coverage (real-world samples, keyword-ranked, not random)

- husky: 105 of 199 hook files (53 %) fully understood by a conservative line grammar,
  about 61 % with small extensions (`&&` lists); the rest uses control flow, variables,
  pipes or functions and must be delegated. 28 of the 105 run a staged-files formatter.
- lefthook: 88/130 configs use `parallel`, 98 `glob`, 66 `stage_fixed`, 71
  `{staged_files}`; named units are the norm (`commands:` 97, `jobs:` 38), so running
  units through lefthook's own CLI covers most configs.
- pre-commit: 69/72 configs have a fixer; 66/72 use remote repos; full test suites are
  rare in general configs (2/72) but common in configs that bind heavy hooks (24/30 in a
  targeted sample, mostly `language: system`, `pass_filenames: false`, often pre-push).

## Constraints for a design

1. **Activation**: one mechanism serves the three: Sluice's hooks directory as the clone's
   local `core.hooksPath`, set through an `[include]` file placed last in `.git/config` so
   husky's rewrite does not displace it; lefthook and pre-commit do not fight it (pre-commit
   install fails loudly, which needs a documented answer).
2. **Delegation** is always possible with full fidelity through each manager's own entry
   point; it is the fallback for anything not understood.
3. **Units**: husky needs a parser (about half of real hooks); lefthook and pre-commit can
   run their own units one at a time, which avoids re-implementing their semantics.
4. **Where units run matters**: managers stash and re-stage in the user's working tree;
   running units concurrently there is unsafe. Index-modifying steps must run in the
   user's tree, in order, before the checks. Checks could run either sequentially in the
   user's tree (manager semantics, no concurrency) or in a clean checkout of the **index
   tree** (concurrency, no stash, exact cache key), which then needs the repository's
   ignored dependencies there (the `[queue] prepare` problem again).
5. **Cache keys** must describe what the unit really saw: the index tree (what is being
   committed), not only the working tree; the effective manager config (`lefthook dump`;
   pre-commit config with resolved `rev`s and hook environments); tool versions. Units
   whose file set depends on remote refs (`{push_files}`, pre-push ranges) need the refs
   in the key or stay uncached.
6. **Exit codes**: pre-commit uses 3 for an unexpected error, which collides with Sluice's
   "not verified on this tree" code in hooks.
7. **Worktrees**: husky runs no hook in a fresh linked worktree (`.husky/_` is untracked):
   a Sluice runner would change that behaviour (it would run the repository's hooks there).
