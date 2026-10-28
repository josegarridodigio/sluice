---
title: "Hook runner H4: documentation and agent text reconciled with the code"
tags: [sluice, changes, hooks, docs, plugin]
---

# Hook runner H4: documentation and agent text reconciled with the code

Wave H4 of [[sluice/plan/hook-runner-plan.md]] (ADR-0021). The feature was written over four
waves (H1, H2a, H2b, H3) and its documents contradicted each other and the code. This wave
checked every claim against `crates/sluice-gate/src/hookentry/`, `main.rs`, `doctor.rs`,
`session_start.rs`, `init/`, `gate_list.rs`, `run.rs`, `sluice-gate-core/src/hookgate.rs`,
`sluice-gate-mcp/src/lib.rs` and the built binary's `--help`, and rewrote the text. Docs and KB
first; the three findings under "Suspected code bugs and gaps" were then fixed in code (see "Review fixes").
Status: written, uncommitted; the lead verifies through Sluice.

## What changed

- `docs/hooks.md`: rewritten as one page. Order: what git does when it runs a hook (no file,
  units, delegation, decided before the first unit), opt-outs, units (order, kinds, output, exit
  codes, no fail-fast, cache key), husky and plain, lefthook, pre-commit, compatibility table,
  verify without `.sluice.toml`, files and activation, worktrees, pre-commit's `install`, drift
  and doctor and session start, known limitations, how to turn it off, cost. Removed the H1
  framing ("nothing about what runs changes", delegation as the whole story, "removes a script
  whose hook vanished" on every run), the duplicated stdin and exit-code paragraphs and the
  "Worktrees and husky" text that promised later waves would run hooks there.
- `docs/cli.md`: global help text and command list now match clap (the `hooks` row was
  missing; the long help does not mention `sluice hooks on`); `sluice gate` has its full help,
  text format and JSON fields; `sluice hooks` has its full help, the units/delegation split, the
  text of `status`, and a JSON example with `name`, `kind`, `shares`, `requires`, `problem`;
  `sluice init --hooks entry` fallback and the manager note; doctor `hooks` row (manager
  configuration calling sluice, `warn`/`ok` cases) and a new `repo-config` row; exit codes for
  `hooks` and `gate list` (and the internal `hooks run`: 126, 0); `SLUICE_HOOKS_DELEGATE`.
- `README.md`: a Features bullet, a Documentation row (the hooks page was not linked at all) and
  a note in the quick start for repositories whose hook files cannot be edited. Not oversold:
  optional, local, "every other hook runs as before".
- `docs/agents.md`, `docs/how-it-works.md`: `gate_list` row; the husky `PATH` limitation points
  to the entry point; section 4 and "Agents never hang" now say that units get no fail-fast
  refusal, so agents must `verify` first.
- Plugin: `plugin/README.md` (the user turns the entry point on, the plugin never does; restore
  on session start; no exit 3 for units), `skills/sluice/SKILL.md` (units are not refused: verify
  before every commit; staged-set units; exit code 3 without the Sluice message is the manager's
  own), `skills/sluice/reference.md` (context variants, restore, `gate_list` exclusions
  troubleshooting). No rule was loosened: never run gate commands in the shell, never approve
  trains, never push to the target branch, exit code 3 keeps its meaning.
- `CHANGELOG.md`: the Unreleased Added bullets (nine, one per wave) became three feature
  bullets plus a docs bullet; Changed lists the behaviour differences of units and the doctor
  change; headings unchanged (no new Unreleased heading, `Fixed` stays empty).
- KB: ADR-0021 amendment (delegation executes a file; fresh linked worktrees do not gain hooks;
  consequence about fail-fast) and the corrected Consequences; `kb/sluice/design/hook-runner.md`
  framed as one system with the H1 sections demoted under it, the H1 non-goal restated, the
  H2a gate-sharing sentence updated for H3, a new "Output and policy" section, doctor drift
  line; `kb/sluice/plan/hook-runner-plan.md` (H1 bullets as built, H2 order, H4 progress).

## Discrepancies found (docs against code)

1. **Fresh linked worktrees (ADR-0021 Consequences, the task's question).** The ADR said the
   repository's hooks now run in a fresh linked worktree. False in the code:
   `delegate.rs::run` returns `ExitCode::SUCCESS` when the underlying hooks directory has no
   executable file for the hook (`decision.target` is `None`) before `runner::try_units` is
   reached, and husky's `.husky/_/<hook>` (untracked) is missing in a fresh worktree; the husky
   planner also requires `_/h`. Nothing runs there, exactly as without Sluice. ADR corrected, the
   CHANGELOG makes no such claim. (The design note already said so in `delegate.rs`'s H1 section.)
2. `docs/hooks.md` "Worktrees and husky": promised running tracked hook files in such worktrees
   in later waves; replaced by the actual behaviour.
3. `docs/cli.md` global long help: text said "or, with `sluice hooks on`, Sluice runs the
   hooks' commands itself", the clap `long_about` does not; and the `hooks` command was missing
   from the printed list. Now identical to `sluice --help`.
4. `docs/cli.md` `sluice hooks`: said units were "husky, plain, lefthook or pre-commit" but
   described `on` as executing the repository's hook; `status` JSON documented as
   `{hook, mode, reason}` or `{hook, mode, units: [{index, kind, command}], requires?}`: the code
   also emits `name` and `shares` per unit. Doctor row omitted that the lefthook and pre-commit
   configuration calling sluice gives `ok`, and `repo-config` was undocumented. `sluice hooks run`
   exit codes (126) undocumented.
5. `docs/hooks.md` stale statements: "one script for each hook git supports" (design note and plan
   still said it: the code mirrors only the repository's hooks, `activate::mirrored`), "removes
   the scripts of hooks that no longer exist" on every run (only `on` and the session-start
   restore prune; `hooks run` only adds), "300 ms budget" (230 ms budget, 300 ms watchdog),
   "Sluice never refuses" duplicated three times.
6. ADR-0021 decision 3 says delegation calls `lefthook`'s script or `pre-commit hook-impl`
   directly; the code `exec`s the file in the underlying hooks directory for every manager.
   Amended.
7. Design note H2a: "sharing needs a key without the name and is left for H3": H3 shared by
   running the unit under the configured gate's own name. Fixed. H2a "A check gets
   `git::child_env`" is only true for husky and plain since H2b. Fixed.
8. Plan H2 said "mutating steps first"; the ADR amendment already changed it to file order.
   Plan bullet updated.
9. CHANGELOG: nine wave-by-wave bullets with "No behaviour changes" (H1) and "(wave H2a)" labels
   that were false or internal after H2/H3; the husky key change that H3 mentions ("one miss after
   upgrading") is not user-facing: H2a, H2b and H3 are all unreleased, so no released version ever
   had the earlier keys.
10. Behaviour documented nowhere before: unit output goes to the job log (`run_unit` uses
    `verbose = false`), `agent_hook_policy` is not applied to units (`enforce_policy = false`), so
    the "agents never hang" guarantee of the README and `how-it-works.md` does not hold for hooks run
    as units. Documented in hooks.md, agents.md, how-it-works.md, the skill, the plugin README and
    the CHANGELOG.

## Suspected code bugs and gaps (all three fixed, see "Review fixes")

1. **pre-commit `.legacy` hooks are skipped when the hook runs as units.**
   `crates/sluice-gate/src/hookentry/precommit.rs`, `read_stub` (about line 150) and `plan`
   (about line 119): the stub's `ARGS` is checked, but the `<hooks dir>/<hook>.legacy` file is never
   looked at. `pre-commit hook-impl --hook-dir "$HERE"` runs `<hook>.legacy` first (research note
   1.2, `_run_legacy`), so a repository whose `pre-commit install` moved an earlier hook to
   `pre-commit.legacy` ran it before; with the entry point on, `plan` yields units, the per-id runs
   do not chain it, and the legacy hook silently stops running. Scenario: a clone with an old
   `.git/hooks/pre-commit` that was installed over by pre-commit (`.legacy` file present), entry
   point on, `.pre-commit-config.yaml` with an eligible local hook, something unstaged-free.
   Suggested fix: `Err("a <hook>.legacy hook is chained by the stub")` (delegate) when
   `<dir>/<hook>.legacy` exists. Documented as a limitation in `docs/hooks.md` until fixed.
2. **A lefthook or pre-commit configuration already wired to `sluice run` is not delegated.**
   The "already runs sluice" rule exists only in `plan.rs::build` (husky and plain). A lefthook job
   `run: sluice run --hook pre-commit test` (what `sluice init` writes) passes `lefthook.rs`
   (`kind_of`: unknown program, so mutating) and runs as `lefthook run pre-commit --command <name>`
   inside a daemon job; the nested `sluice run` sees `SLUICE_JOB` and runs the gate directly
   (`run.rs`, the "nested" branch before the policy), so the `agent_hook_policy = "fail-fast"`
   refusal (exit 3) of an agent's commit is lost and the gate runs inside `git commit`. Same for a
   pre-commit `repo: local` with `entry: sluice run ...`. Suggested fix: the same check in both
   adapters (any unit whose first word is `sluice`). Documented in `docs/hooks.md`.
3. **Stale user-visible text in Rust** (docs and code now disagree; fix in code):
   - `main.rs` `Command::Hooks` doc comment (the `sluice hooks --help` long text) and the
     hidden `HooksCommand::Run` help say the hook is executed untouched; units exist since H2a.
   - `main.rs` `Cli` `long_about` ("Git hooks call `sluice run`") and `RunArgs.gate` ("Gate name
     as defined in `.sluice.toml`"): `sluice run` also takes unit names.
   - `doctor.rs::check_hooks` fail message: "Sluice becomes the hooks entry point and runs them
     untouched".
   - `session_start.rs::hooks_entry`: "the repository's own hooks still run unchanged" (they run
     through Sluice, as units or delegated).
   - MCP `INSTRUCTIONS` and the session context (`rules_text`) do not carry the sentence that the
     skill has ("a unit that `gate_list` leaves out is run by the commit's hook: do not run its
     command yourself") nor that units are not fail-fast; they do not contradict it.
   Docs cli.md quotes clap's text exactly, so it must be regenerated if these change.

## Review fixes

1. **pre-commit `.legacy` (fixed).** `precommit.rs::check_legacy`, called from `plan` right after
   `read_stub`: an executable `<hooks dir>/<hook>.legacy` delegates the whole hook with the reason
   "runs a legacy hook first". The condition is `_run_legacy` of pre-commit 4.7.0 (research note
   1.2): `<hook-dir>/<hook-type>.legacy`, run when `os.access(X_OK)`; a non-executable file is
   ignored, as there. Uses `delegate::is_executable` (any execute bit; `X_OK` for the current user
   can differ only for a file executable by others alone). Test:
   `tests/hook_managers.rs::a_legacy_hook_that_the_stub_chains_delegates_the_whole_hook`
   (non-executable file: units; executable: delegated; another hook: no legacy reason).
2. **Configs already wired to `sluice run` (fixed).** One shared predicate in `plan.rs`:
   `is_sluice`, `text_runs_sluice` (grammar first, a word scan when the text is outside the
   grammar, e.g. the `if command -v sluice` fallback form) and `ALREADY_ROUTED` ("already runs
   sluice"); `plan.rs::build` uses `is_sluice` and the constant. `lefthook.rs::read_job` delegates a
   job whose `run` calls sluice; `precommit.rs::runs_sluice` (checked in `plan` for the hooks at the
   stage only) does it for `entry` or any `args` word. Tests: new cases in
   `what_lefthook_would_skip_or_a_per_command_run_would_change_is_delegated` (plain and fallback
   form) and `what_pre_commit_would_run_differently_per_hook_is_delegated` (entry, path entry with
   args, `sh -c` args).
3. **Stale text (fixed).** `main.rs`: `long_about`, `Command::Run.gate`, `Command::Hooks` and hidden
   `HooksCommand::Run` help; `init/mod.rs` `HooksMode::Entry` help; `doctor.rs`
   (`check_managed_hooks` ok text, `check_hooks` fail text); `session_start.rs` (`hooks_entry`
   restored text, `rules_text` rule 7: a hook command `gate_list` leaves out is run by the commit's
   hook, never by hand); `sluice-gate-mcp/src/lib.rs` `INSTRUCTIONS` (same rule). `docs/cli.md`
   quotes the new clap output verbatim (global help, `run`, `hooks`, `init --hooks`). `docs/hooks.md`
   lost the two limitations and describes both delegations. No test asserted the old strings.

Verification: `cargo check --workspace --all-targets`, the three isolated tests above and
`tests/session_start.rs`; the full suite, clippy and fmt check are the lead's.

## Not found

- The claim in the lead's brief "hooks now run in fresh linked worktrees" is not true (above).
- No contradiction between the plugin README, skill, reference, session context and MCP
  instructions on the safety rules.
- Doc claims about the grammar, classification, lefthook and pre-commit delegation lists, exit
  codes, key composition and gate sharing were re-read against `grammar.rs`, `classify.rs`,
  `lefthook.rs`, `precommit.rs`, `runner.rs`, `gates.rs`, `precond.rs` and matched, except those
  listed above. Not re-verified by running anything: the real-tool tests (lefthook, pre-commit)
  are still pending in CI and Matrix.

## How it was verified

Reading the code and running the built debug binary's `--help` outputs (no tests, no clippy).
