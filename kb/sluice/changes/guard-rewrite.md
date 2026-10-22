---
title: "Change: guard rewrite and equivalences (alpha task A2)"
tags: [sluice, change, alpha, guard, plugin, exec, command-control]
---

# Change: guard rewrite and equivalences (2026-10-23)

Task A2 of [[sluice/plan/alpha-plan.md]]. Design:
[[sluice/design/command-control.md]] sections 1, 4, 6 and 7 (A2);
decision: [[sluice/design/adr/0010-command-control.md]].

## What changed

- New subcommand `sluice guard [--hook]`: the plugin's `PreToolUse` hook in
  Rust. Reads the hook JSON on stdin, writes one decision JSON line on stdout
  or nothing, always exits 0 (errors, panics via `catch_unwind` with a silent
  panic hook, and a 500 ms watchdog all print nothing). Never contacts the
  daemon. Order of decisions:
  1. `SLUICE_GUARD` falsy (`0`, `false`, `no`, `off`) → nothing.
  2. Not `PreToolUse`/`Bash`, no string `tool_input.command` → nothing.
  3. `sluice queue approve` in any piece of a naive split → deny
     ("Approval is reserved to humans.").
  4. User config (`UserConfig::load`, strict) invalid → no rewrites (legacy
     deny only, stderr note).
  5. `matcher::plan` over `lex::split`: rewrite edits, or nothing; a
     `.sluice.toml` that exists but cannot be parsed → no output at all.
  6. A raw gate left in another segment of the rewritten command → today's
     gate deny (so `make && cat x | cargo test` is still denied).
  7. `SLUICE_GUARD=deny` → deny with the gate text (gate edits) or the
     heavy suggestion text.
  8. Permission table (4.6) → `allow` / `ask` with `updatedInput` (the whole
     received `tool_input` with only `command` replaced, so `description`,
     `timeout`, `run_in_background` and unknown fields are echoed) and a
     one-line `additionalContext`; "nothing" falls back to the legacy deny.
  9. No rewrite → legacy deny of raw gate commands (the 0.2.0 shell logic,
     ported: naive split on `&|;` and newlines, `(`/`{`/`VAR=x` stripped,
     verbatim gate prefix, then the runner list).
- Rewrites (prefix insertion into the untouched original):
  `sluice exec --label <L> -- ` before the command word of a heavy segment
  (after `VAR=x` assignments and an optional `time`), or the gate's own
  words replaced by `sluice run <gate> -v` (plus ` --` when argument words
  follow). `<L>`: up to two canonical words stopping at the first option,
  lowercased, `[a-z0-9._-]`, max 32.
- `[agent]` user-config table: `heavy_commands` (replaces the default
  list), `heavy_commands_extra` (appended), `exec_weight` (0 = ceil(budget/2);
  now used by `sluice exec` when `--weight` is absent). Validation: entries
  non-empty, no control characters, ≤ 8 words, ≤ 256 entries in total.
- `plugin/hooks/guard.sh` is a thin wrapper: reads stdin once, runs
  `sluice guard --hook` when `sluice` is on `PATH`; exit 0 → print its
  output if it is a hook decision and stop; any other exit (127 missing,
  2 pre-0.3 usage error, crash) → `legacy_main`, the 0.2.0 logic unchanged
  (only `main` renamed). `SLUICE_GUARD=0` still exits first.
- `plugin/hooks/test-guard.sh`: the 21 fixtures of 0.2.0 run through the
  shell fallback three times (no `sluice` on `PATH` with jq, with the sed
  fallback, and with a fake pre-0.3 `sluice` that exits 2), then a Rust pass
  of 29 checks with the built binary (`SLUICE_BIN`, else `target/debug` or
  `target/release`; skipped with a notice when absent). 92 checks.

## Files and symbols

- `crates/sluice-gate-core/src/heavy.rs` (new): `DEFAULT_HEAVY_COMMANDS`,
  `canonical_words`, `HeavyMatcher::{new, from_config, matching_entry,
  matches}`.
- `crates/sluice-gate-core/src/config.rs`: `AgentConfig` (+
  `exec_weight_or`), `UserConfig::agent`, `MAX_HEAVY_COMMANDS`,
  `invalid_heavy_entry`, validation in `UserConfig::parse`, test
  `user_agent_section`. `lib.rs`: `pub mod heavy`.
- `crates/sluice-gate/src/guard/mod.rs` (new): `run_hook`, `respond`,
  `Inputs`, `Mode`, `Fs` (`World` over the real file system), texts
  `APPROVE_DENY`, `gate_deny_text`, `heavy_deny_text`, `context_note`.
- `guard/lex.rs` (new): `split -> Option<Vec<Segment>>`, `Segment`, `Word`,
  `Op`, `MAX_LEN`.
- `guard/matcher.rs` (new): `plan`, `Plan::{apply, first_gate}`, `Edit`,
  `World`, `Gates`, `NoPlan`, `label`, `runs_approve`, `legacy_gate_hit`.
- `guard/rules.rs` (new): `Rule`, `Rules`, `Decision`, `parse_rule`,
  `add_settings`, `settings_paths`, `load`, `decide`, `MANAGED_SETTINGS`.
- `guard/tests.rs` (new): table-driven fixtures and the injection suite.
- `crates/sluice-gate/src/main.rs`: `Command::Guard { hook }`.
- `crates/sluice-gate/src/exec.rs`: default weight from
  `user.agent.exec_weight_or(budget)`.
- `crates/sluice-gate/tests/guard.rs` (new): the built binary with JSON on
  stdin, and `test-guard.sh` run with `SLUICE_BIN`.
- `plugin/hooks/guard.sh`, `plugin/hooks/test-guard.sh`,
  `plugin/.claude-plugin/plugin.json` (description), `plugin/README.md`,
  `plugin/skills/sluice/SKILL.md`, `plugin/skills/sluice/reference.md`,
  `docs/agents.md`, `kb/sluice/design/config-format.md`.

## Interpretations and deviations from the design

- Label: "first two canonical words" stops at the first option word, so
  `make -j8` is `make`, and `make install` is `make-install` (the design's
  example table shows `--label make` for `make install`, which contradicts
  its own rule; the rule was followed).
- Gate rewrites only for segments with no redirection and not in a pipeline
  (`cargo test --workspace 2>&1 | tail` becomes `exec`): `sluice run -v`
  prints the gate log on stderr and a summary on stdout, so a pipe or a
  redirection would see different bytes; `exec` keeps them exact. Gate words
  and the words replaced in the canonical-only case must be unquoted literals
  (no `$ ~ * ? [ ]`), and a canonical-only match is refused when a
  package-manager directory option is present (`npm --prefix web run build`
  → `exec`).
- `cd`: besides the design's rules (one plain argument, joined with `&&`,
  must exist), any `||` or `;` anywhere in a command that contains `cd`,
  `pushd` or `popd` means no rewrite at all (the directory of later
  segments would depend on exit codes).
- The tokenizer additionally refuses: every unquoted backslash, `{`/`}`
  anywhere (so `${X}` too), `$'…'`, `$"…"`, `$[`, comments, `;;`, `;&`,
  compound-command keywords (`if`, `for`, `while`, `!`, `[[`, ...), and
  control characters other than tab.
- Wrapper skip list extended beyond the design's: `doas`, `ionice`, `dash`,
  `ksh`, `fish`, `source`, `.`, `watch`, `parallel`, `flock`, `setsid`,
  `stdbuf`, `chrt`, `taskset`, `strace`, and any first word starting with
  `sluice`. `time -p` is skipped (only a bare `time` is passed over).
- Permission matching: deny/ask rules are matched liberally against the
  whole command, each segment's text, its text from the command word and from
  every later word, and its unquoted words, for the original and for the
  rewrite (so an ask rule on `sluice exec` also blocks the rewrite); an
  unknown `Bash…` rule shape counts as matching. Allow coverage is strict:
  exact text, `prefix:*` with a word boundary, `*` globs over the raw segment
  text, and a segment with any redirection is never covered. An unknown
  `permission_mode` gives no rewrite. Without `CLAUDE_PROJECT_DIR` (or
  without `HOME` and `CLAUDE_CONFIG_DIR`) the guard cannot see every rule and
  does not rewrite.
- When the permission table says "nothing" (or the rewrite is suppressed),
  the legacy gate deny still applies, as in 0.2.0 (`plan` mode with a gate
  command → deny with the `verify` text).
- The gate `additionalContext` reads "... as gate '<g>' (cached per tree):
  `<rewritten>`. The exit code is the gate's; a cached pass skips the run.
  Use the `verify` tool for gates." ("output is the command's own" is not true
  for `sluice run -v`).
- Test-only environment variable `SLUICE_TEST_GUARD_WATCHDOG_MS` overrides
  the 500 ms watchdog (integration tests on a loaded machine).
- `exec_weight` is honoured by `sluice exec` (the design lists it as the
  second default after `--weight`).

## Verification

- Rust unit tests (`cargo test` via the `test` gate): whole-guard fixtures
  through `respond` with a scratch layout — 54 heavy rewrites (every default
  entry and every equivalence), 20 gate cases (verbatim, extra args, other
  spelling with and without extra, runner-only, `cd sub &&`, `cd ..`, env
  prefix, pipeline, redirection, `time`, dir option), 56 refusals, 18 denies
  (approve, legacy, suggestion mode, `SLUICE_GUARD` values), 19 permission
  rows (each row of table 4.6 through real settings files, managed file,
  `CLAUDE_CONFIG_DIR`, missing project dir, ask rule on a gate), 11 input
  cases (echo of `tool_input` with escapes and unicode, malformed JSON,
  other tool/event, missing cwd), 9 config cases; tokenizer 26 parse and 43
  refusal fixtures plus offsets; permission table unit rows 23; heavy
  equivalences 28 + 14 non-equivalences; config `user_agent_section`.
- Injection suite: 71 crafted commands (≥ 30 rewritten) and 50,000
  deterministic random shell-ish strings; for every rewrite: removing the
  inserted prefixes (and restoring gate words) gives the original byte for
  byte; the output equals original with only the edit texts inserted; the
  rewrite re-tokenizes to the same segments, operators, redirections and
  assignments, each rewritten segment's words being the prefix words
  followed by the original words; replaced gate words carry no shell syntax;
  inserted texts contain no metacharacters; labels are `[a-z0-9._-]{1,32}`.
- Integration (`crates/sluice-gate/tests/guard.rs`, 7 tests): the built
  binary with hook JSON on stdin (allow in bypass, ask in default with a
  gate, deny rule in project settings, approve, silent cases, env switches,
  broken user config, `sluice run <gate> -v -- -- x` parses) and
  `plugin/hooks/test-guard.sh` with `SLUICE_BIN`: 92 checks, 0 failures.
- One haiku session (the only AI run), from a scratch directory:

  ```sh
  PATH="$WT/target/debug:$PATH" SLUICE_DISABLE=1 SLUICE_CONFIG="$D/sluice.toml" \
    claude -p --model haiku --max-turns 2 --plugin-dir "$WT/plugin" \
    --permission-mode bypassPermissions --output-format stream-json --verbose \
    "Run exactly this Bash command once and report its output verbatim: printenv SLUICE_JOB"
  ```

  with `$D/sluice.toml` = `[agent] heavy_commands = ["printenv"]` (Claude
  Code 2.1.309, 2 turns, $0.0047). The model's `tool_use` showed the original
  `printenv SLUICE_JOB`; the tool result was `inproc-2802905`, i.e. the
  rewritten `sluice exec --label printenv-sluice_job -- printenv SLUICE_JOB`
  ran (in-process because of `SLUICE_DISABLE=1`). The session transcript
  shows the hook's `allow` output and the `additionalContext` delivered to the
  model as a system reminder ("Sluice ran this through its shared queue as:
  `sluice exec --label printenv-sluice_job -- printenv SLUICE_JOB`. ...").
  Confirms consequences (a) and (c) of section 1: allow + `updatedInput` runs
  the rewrite, and the model sees the rewritten command only through the note.
- Dogfooding from the worktree root: `sluice run fmt` PASS,
  `sluice run clippy` PASS, `sluice run test` PASS 77.5 s.

## Open points

- `scripts/plugin-e2e.sh` scenario (c) expects a deny for `cargo test` with
  Bash; with 0.3.0 the guard rewrites it instead (or asks). E3 should update
  that expectation.
- Allow-rule matching approximates Claude Code's matcher (design open
  question 4); it errs towards `ask` or no rewrite.
