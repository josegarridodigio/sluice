---
title: "ADR-0021: Sluice runs a repository's existing git hooks, without modifying the repository"
tags: [sluice, adr, hooks, husky, lefthook, pre-commit]
---

# ADR-0021: Sluice runs a repository's existing git hooks, without modifying the repository

Status: accepted (2026-10-28, user decisions below). Research:
[[sluice/research/hook-managers-synthesis.md]] (and the three per-manager notes). Plan:
[[sluice/plan/hook-runner-plan.md]]. Related: ADR-0006 (hook flag), ADR-0016 (init
detection and hook rewiring), ADR-0014 (zero-config agent integration).

## Context

Sluice reaches the work of agents through two seams, and both depend on things it does
not own:

1. **Git hooks** call Sluice only if the repository's hook files say `sluice run`
   (ADR-0016 rewires them), which needs the repository edited and a `.sluice.toml` in it.
2. **PATH shims** work only while nobody reorders `PATH`; husky prepends
   `node_modules/.bin` after its only extension point (`~/.config/husky/init.sh`), so a
   hook's `ng test` never meets the shim.

Reported on 0.6.3: in a client repository whose husky hook files cannot be edited, every
`git commit` re-ran lint and the whole test suite outside the queue and the cache for
minutes, competed with the daemon's jobs, made timing tests fail and once refused a
commit whose tree was already verified. `sluice doctor` reported it as a warning "to
ignore if a hook manager owns the hooks".

The source analyses of husky 9.1.7, lefthook 2.2.2 and pre-commit 4.7.0 establish that
Sluice can own the hooks entry point of a clone without editing the repository, delegate
any hook untouched with full fidelity, and run lefthook's and pre-commit's units through
their own CLIs (synthesis note).

## Decision

1. **Sluice is the hooks entry point of a clone, locally.** Sluice keeps a hooks
   directory per clone (under the clone's git directory) and sets `core.hooksPath` to it
   through a file included by the **last** `[include]` of `.git/config`. Nothing in the
   repository changes; nothing shared with others changes. husky's rewrite of
   `core.hooksPath` on every install does not displace it (the last value wins, measured);
   lefthook never sets it; `pre-commit install` refuses while it is set (documented, with
   the command to use instead).
2. **Activation and drift.** `sluice init` (and the plugin's session start, for agent
   sessions) activates it on a clone that has a supported hook manager or plain hooks;
   `sluice doctor` and the session context report a clone whose hooks no longer go
   through Sluice as a **failure**, and the session start restores the activation.
   `sluice hooks off` removes it (restoring the previous `core.hooksPath`).
3. **The principle: understand the hook completely, or delegate it untouched.** For each
   hook git runs, Sluice detects the manager (husky, lefthook, pre-commit, plain
   `.git/hooks`) and either runs the hook's units itself (below) or calls the manager's
   own entry point with the same arguments and stdin (`.husky/_/<hook>`, lefthook's hook
   script, `pre-commit hook-impl`). Anything not understood, any unknown manager, any
   parse doubt → delegation. A Sluice problem must never stop a hook from running or make
   it run differently.
4. **Units.** husky and plain hooks: a conservative line grammar (simple commands, `&&`
   lists, comments; control flow, variables, pipes, functions, `cd` → delegate the whole
   hook). lefthook: named jobs, each run by `lefthook run <hook> --command <name>
   --no-auto-install --skip-lfs` (unnamed jobs or setups → delegate). pre-commit: hook ids,
   each run by `pre-commit run <id> --hook-stage <stage>` (ambiguous ids, remote-ref
   dependent stages → delegate the hook).
5. **Execution (option a, user decision).** Units run **in the user's working tree, one
   after another, in the manager's order**, through the daemon (queue, budget, containment).
   Steps that modify files or the index (formatters, `lint-staged`, `stage_fixed`,
   pre-commit fixers) run first and are never cached; checks follow. Running checks in a
   clean checkout of the index tree (option b: concurrency, no stash) is a later,
   measured improvement that would reuse `[queue] prepare`.
6. **Caching.** A check unit is cached by: the index tree (what is being committed), the
   unit's identity and command, the manager's effective configuration (lefthook `dump`;
   pre-commit config with resolved `rev`s and hook environments), and the toolchain inputs
   the unit declares or the manager implies. A unit whose inputs cannot be described
   exactly (remote refs for pre-push ranges, network) runs uncached.
7. **`.sluice.toml` becomes optional in such repositories.** Gates are derived from the
   hooks; a `.sluice.toml`, when present, still defines weights, paths, the queue and extra
   gates, and its gates that match hook units share their cache.
8. **Exit codes.** A hook's exit code is the manager's (or the unit's); Sluice's own
   refusal code is never confused with a delegated manager's code 3 (pre-commit).

## Amendment (2026-10-28, wave H2a)

Decision 5 is refined: units keep the **hook file's order** instead of running mutating
steps first. Reordering changes semantics (a check placed before a formatter would see
different files), and order preservation is what "run the hook faithfully" means.
Mutating units (and every program not recognised as read-only) always run and are never
cached; recognised checks are cached by the working-tree snapshot taken right before they
start, so they reflect earlier mutating units. A hook that receives stdin runs as units
only when every unit is a recognised check (none of them reads stdin); otherwise it is
delegated whole. Details: [[sluice/changes/hook-runner-h2a.md]].

## User decisions (2026-10-28)

| # | Question | Answer |
|---|---|---|
| 1 | Patch (`init.sh`) or a product solution | Product solution, scalable to the team |
| 2 | Cover only agents' commits | No: every hook run (agents, people, tools) |
| 3 | Exec interception for non-hook escapes | Out of scope |
| 4 | Where units run first | Option (a): the user's working tree, sequentially |
| 5 | Teammates rewriting hooks or migrating managers | Must need no Sluice change: read hooks at run time; delegate what is not understood |

## Consequences

- Repositories Sluice does not own get the cache and the queue for their hooks with no
  file of theirs changed and without a `.sluice.toml`.
- A teammate's hook change takes effect at the next run; a migration to a supported
  manager keeps working; to an unsupported one degrades to delegation, reported.
- Sluice becomes responsible for faithfully invoking three managers; each adapter is
  tested against configurations taken from public repositories, and delegation is the
  safety net.
- In a fresh linked worktree husky runs no hook today; with Sluice as the entry point the
  repository's hooks do run there (documented behaviour change).

## Alternatives considered

- **`~/.config/husky/init.sh` takeover**: husky-only, shell logic outside the product,
  a patch (user decision 1).
- **Interposing only agents' commits** (`git -c core.hooksPath` in the guard): misses
  people and tools (decision 2).
- **Running the whole hook as one cached job**: too coarse; `verify` cannot warm it.
- **Exec interception (`LD_PRELOAD`, seccomp)**: a different problem (escapes outside
  hooks); out of scope (decision 3).
- **Re-implementing lefthook and pre-commit**: unnecessary; their CLIs run one unit with
  full semantics.
