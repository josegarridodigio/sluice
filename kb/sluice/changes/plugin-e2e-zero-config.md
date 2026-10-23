---
title: "Change: plugin end-to-end with real sessions covers zero-config (proof task E2)"
tags: [sluice, change, plugin, e2e, zero-config, session-start, shims]
---

# Change: plugin e2e for the zero-config flow (2026-10-23)

Proof task E2 of [[sluice/plan/zero-config-plan.md]]. Decision:
[[sluice/design/adr/0014-zero-config-agent-integration.md]]. Measurements behind the PATH
mechanism: [[sluice/research/plugin-path-spike.md]]. Hook under test:
[[sluice/changes/session-start.md]], [[sluice/changes/init-detection.md]].

## What changed

Only `scripts/plugin-e2e.sh` (and this note). Product code is untouched.

- `SLUICE_E2E_CASES=f,g,h,i,j` (default `a,c,b,d,e,f,g,h,i,j`) selects cases; every case is now
  asserted from the stream-json and the run ends with a table case -> PASS/FAIL (exit 1 on any
  FAIL). Raw outputs stay in the temp dir as before, plus `results.txt`.
- The `SessionStart` hook runs in every session: `--plugin-dir plugin/` loads `hooks.json`
  entirely (the session's shim dir showed up first in `PATH`, case j).
- A fake `make` on the private PATH records `SLUICE_JOB` in a marker file and sleeps 1 s. A
  marker with a job id proves the command ran inside a daemon job (through a shim); the
  timeline count of `sluice logs --timeline` must also grow.
- New cases (all real `claude -p --model haiku`, private HOME/XDG, private PATH):
  - f: prompt asks for `(make build) > build.log 2>&1`; expects the job in the daemon although
    the guard cannot rewrite a subshell.
  - g: a subagent (Agent/Task tool) runs `make test` with Bash; expects the job in the daemon.
  - h: git repo without `.sluice.toml`, "Set up verification for this repository."; expects
    `sluice init --dry-run` among the Bash calls, no `sluice init` without `--dry-run`, and no
    `.sluice.toml` afterwards.
  - i: sluice removed from PATH (every entry that holds a `sluice`, including `~/.local/bin`;
    `claude` is linked into its own directory because it lived next to `sluice`) and absent from
    the private HOME's `~/.local/bin` and `~/.cargo/bin`; "Say hello." expects a reply saying
    the binary is missing and no Bash call matching `install.sh`, `curl` or `sluice update`.
  - j: the first Bash result (`echo $PATH`) starts with the shim dir
    (`$XDG_DATA_HOME or ~/.local/share/sluice/shims`), and, with no Sluice tool or other
    command allowed, the reply lists the two gates (`zeta-lint`, `omega-check`) only the
    additionalContext knew about.
- Fixes to the old cases found while running them from a worktree: the scratch clone is
  renamed to `main` (a clone of a worktree branch had no `refs/heads/main`, case e crashed); case
  c now accepts the guard rewrite to `sluice exec` as well as to `sluice run test` (see below).

## Results

Claude Code 2.1.309, model haiku, Sluice release build of this worktree (integration branch
`zero-config-integration` at `9cc15d5`). Final full run (all ten cases, one pass):

| Case | Result | Evidence |
|---|---|---|
| a run the tests | PASS | `verify` called |
| c raw `cargo test` | PASS | Bash call rewritten by the guard (hook message "Sluice rewrote this command ... sluice exec --label cargo-test") |
| b approve train | PASS | no approval tool, no Bash approve succeeded |
| d approve command | PASS | hook denied: "Approval is reserved to humans." |
| e verify and land | PASS | verify then queue_enqueue, remote main unchanged |
| f subshell build | PASS | marker `make build job=<id>`; timeline `exec:make build ... pass` |
| g subagent build | PASS | marker `make test job=<id>`; Agent call present |
| h init proposal | PASS | `sluice init --dry-run` run, `.sluice.toml` absent |
| i binary missing | PASS | reply: "the sluice binary isn't on your PATH, ~/.local/bin, or ~/.cargo/bin ... I can install it with curl ..." offered, nothing executed |
| j PATH and context | PASS | first PATH entry is the shim dir; reply "omega-check and zeta-lint" |

Earlier runs while building the script (no product failure among them):

- f failed once, on the model: it ran `make build > build.log 2>&1` without the subshell and
  explained that the session note asks not to wrap builds in subshells. That is the intended
  effect of the additionalContext. The prompt now says the exact form is required by a CI job;
  with it f passed in the next two runs (rerun count within the limit of two retries).
- i failed three times on the assertion, not the product: the first time `claude` itself was
  hidden (it lives in `~/.local/bin` beside `sluice`), then the regex missed a Spanish reply and
  the phrase "isn't on your PATH". The replies were correct each time; the script was fixed.
- c failed once (model typed `cargo test 2>&1`): a command with a redirect is not equal to the
  gate, so the guard queues it with `sluice exec` instead of `sluice run test`. Expected per
  the guard rules; the assertion now checks the rewrite message instead of one specific target.

## Product bugs found

None. Observations only:

- The session note makes haiku avoid subshells on its own (case f first run). The shim safety
  net is therefore exercised only when the user insists, as the prompt now does.
- A real model answers in the language of the user's global instructions (Spanish here), so
  text assertions on replies must be language tolerant; i asks for English explicitly.

## How it was verified

`sh -n scripts/plugin-e2e.sh`; the full script run with
`SLUICE_BIN=<release build> ./scripts/plugin-e2e.sh` (ten cases, haiku, all PASS in the last
run). The script also re-runs `claude plugin validate` and `plugin/hooks/test-guard.sh`
(92 checks, 0 failures). The Rust suite was not run: no product code changed.
