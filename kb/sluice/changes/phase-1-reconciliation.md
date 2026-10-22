---
title: "Phase 1 reconciliation report"
tags: [sluice, reconciliation, phase-1]
---

# Phase 1 reconciliation report

Scope: Phase 1 only (cache-only `sluice run`, no daemon). Docs reviewed:
`design/cache-key.md`, `design/config-format.md`, `design/architecture.md`
(sections 1, 3, 4, 4b, 8, 9, 9b), `design/adr/0004-agent-detection.md`,
`design/adr/0006-hook-flag.md`, `changes/phase-1-cache-only.md`, `README.md`
(sections "What Sluice does", "How it works", "Command surface", "Install",
"Things to know") and the `--help` output. Code: `crates/sluice-gate-core/src/*.rs`
and `crates/sluice-gate/src/*.rs`. Statements marked Phase 2+/daemon/MCP/PSI were
skipped. Doc paths are relative to `kb/sluice/` (except README); code paths are
relative to `crates/sluice-gate-core/src/` (`core:`) or `crates/sluice-gate/src/` (`bin:`).
No file other than this report was written. Not re-checked: the 84 core + 8 unit + 8 CLI + 15
acceptance test counts in `phase-1-cache-only.md:64`, which match `cargo test -- --list`.

## 1. Documented but not implemented

| # | Statement | Doc file:line | What the code does instead | Code file:line |
|---|---|---|---|---|
| 1 | "One per-user daemon runs gates under a budget ... admission also watches real pressure (PSI)" (not marked Phase 2) | README.md:51-54 | No daemon and no PSI. `sluice daemon` exits 70; `sluice run` bounds concurrency with a `flock` semaphore of `daemon.budget` slots. | bin:main.rs:152, bin:run.rs:303-332 |
| 2 | "Coalesces identical requests. Two agents asking for the same tree and gate share one run" (not marked Phase 2); `--help` long text says Sluice "coalesces duplicate requests" | README.md:55-57, `--help` | No in-flight map. Two concurrent misses on the same tree both run (documented as a Phase 1 limitation only in the change note). | bin:main.rs:31-34, bin:run.rs:168-177, changes/phase-1-cache-only.md:44 |
| 3 | "Two front-ends, one brain ... `sluice mcp` for agents ... Both talk to the same daemon"; `--help`: "agents call the MCP server started by `sluice mcp`"; `run` help: "using the cache and the queue" | README.md:58-60, `--help` | `sluice mcp` exits 70 with "not implemented yet". There is no queue, only the semaphore. | bin:main.rs:24, 50, 151, 157-162 |
| 4 | Diagram and step 4: "Otherwise the request goes to the daemon, which either attaches it to an identical in-flight run or queues it" (not marked Phase 2) | README.md:75-81, 92-93 | On a miss the CLI itself acquires slots, spawns the gate and writes the marker. No socket is ever used. | bin:run.rs:168-177, bin:run.rs:376-425 |
| 5 | pre-push: "the tree is `<local sha>^{tree}` only if the worktree is clean; otherwise fall back to the working-tree hash"; architecture: Sluice "decides per hook type" | design/cache-key.md:71, design/architecture.md:81-82 | No hook-type logic in tree selection. pre-push does not read stdin and uses the working-tree hash like every other caller. The only per-hook behaviour is recording `committed_tree` for `--hook pre-commit`. `head_tree()` exists but is never called by the binary. | bin:run.rs:231, 598-604, core:git.rs:326 |
| 6 | "`sluice run --explain` prints which rule fired" | design/adr/0004-agent-detection.md:62-63 | No `--explain` flag. The rule that fired is only visible inside the fail-fast message. | bin:main.rs:50-66, bin:run.rs:155-164 |
| 7 | `SLUICE_LOG`: "Tracing filter for the client (`info`, `debug`, ...)" | design/config-format.md:287 | Not read anywhere. `tracing` and `tracing-subscriber` are dependencies but no code initialises or uses them. | bin Cargo.toml:26-27; no usage in `*.rs` |
| 8 | "The key derivation and the marker path are a public contract documented in the README, so shell snippets can check a marker without Sluice" | design/cache-key.md:149-151 | The README only names the directory (and imprecisely, see mismatch 22). It gives neither the key byte layout, the filename rule `<key>.json`, nor the marker schema. | README.md:90-91 |
| 9 | "git >= 2.31 at runtime" / "needs git >= 2.31" | README.md:164, design/cache-key.md:20, changes/phase-1-cache-only.md:21 | No version check. `version()` is never called by the binary. On an older git, `rev-parse --path-format=absolute` fails and every `GitCommand` error is reported as "not a git repository". | core:git.rs:109, core:git.rs:205-207 |
| 10 | "documented in `docs/agents.md` with instructions to add their markers or export `SLUICE_AGENT=1`" | design/adr/0004-agent-detection.md:67 | There is no `docs/` directory. README marks `docs/agents.md` as Phase 3, the ADR does not. | README.md:154, 193 |

## 2. Implemented but not documented

| # | Behaviour | Code file:line | Suggested doc location |
|---|---|---|---|
| 1 | Exit code 70 and message `sluice: 'mcp'/'daemon'/'status' is not implemented yet (it arrives with the daemon and MCP phases)` for the three unimplemented commands. | bin:main.rs:24, 151-153, 157-162 | README Command surface; architecture.md section 9 exit-code list; `--help` |
| 2 | `sluice init --force`; without it an existing `.sluice.toml` is refused (exit 1, "already exists; not overwriting (use --force)"). `init` and `hook print` read and write `./.sluice.toml` in the current directory, not the repository root, and do not require a git repo. | bin:main.rs:205-215, 186-187 | architecture.md:24 (section 1); config-format.md "Repository config" |
| 3 | `cache status` output: `markers:` dir line, `tree:` line, one `HIT (passed N ago, job ID)` / `MISS (last run N ago: reason, exit C)` / `MISS` / `ERROR (...)` line per gate, `tree: unknown (...)` line; `--all` appends every marker (gate, age, 12-char tree and key prefixes). It ignores `cache = false`, and runs every gate's `key_inputs` commands. | bin:cache.rs:44-119 | cache-key.md Markers rules; architecture.md:23 |
| 4 | `cache clear` also deletes miss records, works without `.sluice.toml`, and prints `sluice: removed N cache entr(y/ies) for all gates / gate 'x'`. Temp files `.tmp-*` are untouched. | bin:cache.rs:124-146, core:marker.rs:238-312 | cache-key.md:142 |
| 5 | `sluice logs`: default is the newest `*.log` by file name; a `# <path>` header goes to stderr and the content to stdout; errors "no job logs in <dir>" and "cannot open <path>" exit 1; `-n` prints the last N lines. | bin:logs.rs:14-48 | README Command surface; architecture.md section 4b |
| 6 | Log file framing: header `# sluice job ID: gate 'x' in <root>` + `# command: <raw cmd>`, trailer `# exit: N after T s (timed out)`; stdout and stderr are pumped by two threads, so order is by arrival. Gate stdin is `/dev/null` (inherited only under `SLUICE_DISABLE`). | bin:run.rs:437-443, 447, 506-514, 353 | architecture.md section 9b |
| 7 | Failure/timeout tail is introduced by `--- last N lines of <log> ---` on stderr; the FAIL line shows `exit=signal` when the gate died from a signal (process exit code then 143). | bin:run.rs:402-407, 646-654, 359-364 | architecture.md section 9 |
| 8 | Stderr notices on the way to an uncached run: `sluice: cache disabled (<reason>)` (reasons `SLUICE_CACHE=0`, `CI is set`), `tree unknown (<reason>); running uncached`, `cannot snapshot the tree`, `cannot derive the cache key`, `cannot read the cache`, `SLUICE_DISABLE is set; running 'x' directly`, `waiting for N of M budget units for 'x'`. | bin:run.rs:107, 222, 234, 238, 245, 258, 323-326 | architecture.md section 9 |
| 9 | Miss-record JSON schema: `format, key, gate, exit_code (null when the process did not exit normally), reason, job_id, log_path, recorded_at`; `reason` is one of `gate failed`, `timed out`, `the gate modified the tree` (the last with `exit_code` 0). | core:marker.rs:45-64, bin:run.rs:386, 400, 595 | cache-key.md:144-148 (names only "a timestamp") |
| 10 | Miss records are written only when caching was active and the tree known. None when `CI`, `SLUICE_CACHE=0`, `cache = false`, or an unknown tree. A pass with caching off writes no marker and no miss. | bin:run.rs:385, 399, 414-415 | cache-key.md:144-148 |
| 11 | Miss records never expire: `prune()` skips them and the lookup does not delete them. They disappear only on a later pass of the same key or `cache clear`. | core:marker.rs:320-334 | cache-key.md:140-141 |
| 12 | Marker details: `committed_tree` is serialised as `null` when absent (no skip), `deny_unknown_fields` means a marker with extra fields is treated as corrupt and deleted, files are pretty-printed with a trailing newline, directory mode `0700`, temp files `.tmp-<pid>-<n>` with `fsync`. | core:marker.rs:17-42, 129-160 | cache-key.md Markers (example and rules) |
| 13 | The fail-fast policy fires on any non-hit, including uncached runs: under `CI`, `SLUICE_CACHE=0`, `cache = false` or an unknown tree an agent hook always exits 3. The message has three lines and names the rule: `'g' has not passed on this tree (<why>)`, `invoked by a coding agent (<rule>) and agent_hook_policy is "fail-fast"`, `run 'sluice run g' or the 'verify' MCP tool first, then retry the commit`; `<why>` is `last run failed N ago [with exit code C]`, `no cached pass for this tree` or the cache-off reason. | bin:run.rs:142-166 | architecture.md section 8; ADR-0004 Consequences |
| 14 | `human_hook_policy = "run-unqueued"` skips the semaphore (a valid human option); there is no such row in the policy table. Outside a hook (no `--hook`, no `GIT_DIR`/`GIT_INDEX_FILE`) the policy is always `wait`. | bin:run.rs:127-141, 168-170 | architecture.md:175-180 (section 8) |
| 15 | `--hook` accepts any string and the hook policy applies to every hook name, including `commit-msg` and `post-*` which the design calls "not a target". An empty `--hook ""` also counts as a hook. | bin:run.rs:127-128 | cache-key.md:72, ADR-0006 |
| 16 | `SLUICE_AGENT` parsing: `1/true/yes/on` force agent, `0/false/no/off` force human, case-insensitive, trimmed; any other value is ignored and detection continues. Rule names reported: `SLUICE_AGENT=1`, `<VAR> is set`, `<k>=<v>`. `SLUICE_CACHE` has the same word set; an unrecognised value is ignored and `CI` decides. | core:agent.rs:24-66, core:mode.rs:39-59 | config-format.md:284-285 env table; ADR-0004 decision |
| 17 | Repo config validation not listed in "Rules": `cmd` must be non-empty after trim, `weight >= 1`, `key_files` must be relative with no `..`, `depends_on` may not reference itself or an unknown gate, `human_hook_policy = "fail-fast"` rejected, `version` must be 1 with the message `unsupported version N: ...`. `gates` may be empty. | core:config.rs:46-58, 208-273 | config-format.md Rules (after line 241) |
| 18 | User config: unknown fields rejected; `version` is optional (defaults to 1) although the example shows it as required-looking; a missing file means defaults; validation `daemon.budget >= 1`, `daemon.nice <= 19`, `daemon.ionice_level <= 7`, `admission.psi_poll_interval > 0`, `repos."p".budget_share_max >= 1`. `SLUICE_CONFIG` empty is ignored. | core:config.rs:336, 417-428, 477-536 | config-format.md "User config" |
| 19 | In Phase 1 only `daemon.budget`, `cache.ttl_days`, `cache.tail_lines` and `agent_detection.*` are consumed. Everything else (`nice`, `ionice_*`, `log_retention_days`, `idle_exit`, `last_subscriber_gone`, `admission.*`, `repos.*`) is parsed and validated but inert. Logs are never pruned in Phase 1 (`log_retention_days` unused). | bin:run.rs:131-132, 194, 304, 394; no log pruning in bin | config-format.md "User config"; architecture.md section 3 |
| 20 | `key_inputs` run via `sh -c` in the worktree root with the sanitised env plus the gate `env`, stderr discarded. A non-zero exit or unreadable key file does not fail the gate: it prints `cannot derive the cache key (...)` and runs uncached. They are not run at all when caching is off. | core:key.rs:114-154, bin:run.rs:242-248 | config-format.md:231-232 |
| 21 | Every Sluice error prints `sluice: error: <message>` and exits 1 (unknown gate, no repo, bad config, key-input and snapshot hard errors, spawn failure, log creation failure). | bin:run.rs:180-183 | architecture.md:192-194 |
| 22 | Unknown-tree reason strings: `unmerged index: <path>`, `skip-worktree or assume-unchanged entry: <path>`, `submodules are not supported yet: <path>`, `could not stage working copy: ...`, `could not write tree: ...`. `scratch_root` errors if the runtime dir exists but is not a directory owned by the user and forces mode `0700`. | core:tree.rs:132-144, 323, 326, 361, 366 | cache-key.md:51-58 |
| 23 | Budget wait: weight clamped to `[1, budget]`, polls with backoff 25 ms to 500 ms, prints one notice, waits forever (no timeout; `Semaphore::acquire` timeout path is unused). Slot files are `<runtime>/sluice/semaphore/slot-<n>.lock`. | bin:run.rs:303-332, core:semaphore.rs:61-109 | architecture.md section 9b |
| 24 | State dir resolution: empty `XDG_STATE_HOME` ignored; if `HOME` is unset/empty the logs go to `/.local/state/sluice`. | bin:dirs.rs:12-21 | architecture.md:96 (section 4b) |
| 25 | `hook print` / `init` snippet details: Husky lines end with `|| exit $?`; lefthook command names `sluice-<gate>`; pre-commit entries `id: sluice-<gate>`, `pass_filenames: false`, `always_run: true`, `stages: [<hook>]`; plain adds `chmod +x .githooks/*` and `git config core.hooksPath .githooks`; every snippet ends with a two-line footer; with no `.sluice.toml`, a `# No .sluice.toml found here; example gates shown` preface and gates `lint`/`test`; an invalid config exits 1. Only `pre-commit` and `pre-push` gate lists are emitted. | bin:snippets.rs:18, 25-74, bin:main.rs:186-203 | README Install / Command surface; config-format.md:230 |
| 26 | The gate runs with `current_dir` = worktree root even when invoked from a subdirectory, with `env_clear` plus the sanitised env, and the `sh -c '<cmd> "$@"'` form requires the command text to be trimmed. | bin:run.rs:335-345 | architecture.md section 9b (partly stated) |
| 27 | README never states the cache TTL default (30 days), the default gate timeout (30m), the default budget (`max(2, cpu-1)`), the log directory, or that `cache status`/`logs` exist as the way to find a failure log. | core:config.rs:104-106, 430-443, 459-462 | README "Things to know" or Command surface |

## 3. Mismatches

| # | Doc says (file:line) | Code does (file:line) | Severity |
|---|---|---|---|
| 1 | `SLUICE_DISABLE=1` "bypass[es] Sluice entirely: run the gate directly" (config-format.md:281, README.md:138, cache-key.md:160-161, architecture.md:217) | The bypass check runs after `resolve()`, so it still requires a git repository, a valid `.sluice.toml`, the gate to exist and a valid user config. Verified: `SLUICE_DISABLE=1 sluice run x` outside a repo exits 1 with "not a git repository". The gate env is also sanitised, and a signal exit maps to 143. (bin:run.rs:100-112, 347-364) | user-visible |
| 2 | `defaults.tail_lines` "Lines of log shown on failure" (config-format.md:199-200, 226); architecture.md:190-191 says `cache.tail_lines` | Only the user-config `cache.tail_lines` is used (bin:run.rs:394, 410). `defaults.tail_lines` is parsed (core:config.rs:87) and even written by `sluice init` (core:config.rs:286-287) but has no effect. | user-visible |
| 3 | `CI` (any value): "Cache off unless `SLUICE_CACHE=1`" (config-format.md:283); "CI set and `SLUICE_CACHE != 1`" (cache-key.md:158) | `CI` with a falsy value (`0/false/no/off`) does not disable the cache; `SLUICE_CACHE=true/yes/on` also re-enables; an unrecognised `SLUICE_CACHE` value falls through to `CI` (core:mode.rs:39-59). cache-key.md:162-163 states the real rule, so the docs disagree with each other. | user-visible |
| 4 | `depends_on`: "Not in Phase 1; reserved" / "reserved and unimplemented" (config-format.md:234, 297) | Validated in Phase 1: self-reference and unknown gate names are config errors (core:config.rs:257-270); no cycle check, no scheduling. | user-visible |
| 5 | "An unknown tree never fails the gate ... runs the gate uncached" (cache-key.md:55-58) | For an agent-invoked hook with `fail-fast` the run is refused with exit 3 (reason `tree unknown: ...`), because the policy is applied to every non-hit (bin:run.rs:142-166). | user-visible |
| 6 | TTL: "ignored and removed lazily (pruning on write only, mtime-based)" (cache-key.md:140-141) | Lookup deletes an expired marker on read, using the older of file mtime and `passed_at` (core:marker.rs:190-196); `prune()` on write is mtime-only (core:marker.rs:320-334). | internal |
| 7 | `sluice cache status` "lists markers for the current tree with age" (cache-key.md:143); `sluice cache status\|clear [--gate]` (architecture.md:23) | Per-gate HIT/MISS for the current tree; `--all` lists all markers regardless of tree (bin:cache.rs:58-119). README.md:125 and `--help` mention `--all`, the design docs do not. | wording |
| 8 | `cache clear` "removes markers for the current repo" (cache-key.md:142) | Also removes miss records (bin:cache.rs:124-146, core:marker.rs:293-312). `--help` and phase-1-cache-only.md:34 say so. | wording |
| 9 | "a single `git ls-files -v -z` on the copied index" before step 3 (cache-key.md:42-43) | It runs on the real index path (honouring inherited `GIT_INDEX_FILE`) before the private copy exists, read-only with `--no-optional-locks` (core:tree.rs:306-307, 356). | internal |
| 10 | "Read-only probes (`rev-parse`, `status`, `ls-files`)" (cache-key.md:47) | `status` is never run; probes are `rev-parse`, `ls-files`, `config --get core.worktree`, `rev-parse --local-env-vars` (core:git.rs:164, 205, 291; core:tree.rs:307, 363). | wording |
| 11 | Key = SHA-256 over (... "cache-format version") (architecture.md:83-84) | The key starts with the literal `sluice-cache-v1` (core:key.rs:14), independent of `CACHE_FORMAT_VERSION` (core:lib.rs:122). Bumping the format invalidates markers through the `format` check (core:marker.rs:187), not through the key. | internal |
| 12 | Job ids `<unix-ms>-<pid>-<random hex>` (architecture.md:98); marker example `job_id "01J..."` (cache-key.md:124) | `<unix-ms>-<pid>-<6 hex of sub-second nanos>`, not random and not a ULID (bin:dirs.rs:32-42). | wording |
| 13 | Exit codes: architecture.md:192-194 lists 0, gate code, 3, 124, 143, 1, 2; README.md:134-136, phase-1-cache-only.md:39-40 and `--help` omit 143 (and 2); none mention 70 | Code produces 143 for signal death, 2 via clap usage errors, 70 for unimplemented commands (bin:run.rs:359-364, bin:main.rs:24). | user-visible |
| 14 | ADR-0004: "the daemon removes `CLAUDECODE` and `CLAUDE_CODE_*` from the environment it builds for gate processes" (adr/0004:44-46) | The Phase 1 client strips every variable starting with `CLAUDE` (so also `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_CONFIG_DIR`, ...) plus the `git rev-parse --local-env-vars` list (core:git.rs:313). config-format.md:294-295 and architecture.md:214 say "every `CLAUDE*`", so the ADR is the outlier. | user-visible |
| 15 | ADR-0004 "Status: proposed ... needs user confirmation" (adr/0004:8); "Detection lives in `sluice-core`" (adr/0004:62) | Implemented and in use; the crate is `sluice-gate-core` (core:agent.rs). | wording |
| 16 | ADR-0004:54 default `env_any_set = ["CLAUDECODE"]`; config-format.md:270 example `["CLAUDECODE", "SLUICE_AGENT"]` | Default is `["CLAUDECODE"]` only (core:config.rs:468). Listing `SLUICE_AGENT` makes any non-empty value (for example `banana`, ignored by rule 1) mean agent. | wording |
| 17 | `.sluice.toml` "located at the repository root (the main worktree's root ...)" (config-format.md:187) | Loaded from the current worktree's `--show-toplevel` (core:git.rs:197, core:config.rs:178-187); the following sentence about per-worktree revisions is the accurate one. | wording |
| 18 | Not-found error (not documented) | Message "no .sluice.toml found in X or its parents" but only the worktree root is checked, no parent walk (core:error.rs:32, core:config.rs:178-187). | user-visible |
| 19 | Human policy `wait`: "wait in the queue, show progress" (architecture.md:180) | One `waiting for N of M budget units` line, only when blocked, no progress (bin:run.rs:322-328). | wording |
| 20 | Fail-fast "message pointing to `sluice run <gate>` and the `verify` MCP tool" (architecture.md:177); README.md:62-63 quotes "run the `verify` tool first, then retry the commit" | Actual text differs (`run 'sluice run g' or the 'verify' MCP tool first, then retry the commit.`) and the `verify` tool does not exist until Phase 3, so the message points agents at an unavailable tool (bin:run.rs:159-164). | user-visible |
| 21 | README.md:154 (Roadmap, outside the audited sections): Phase 3 includes "agent detection, `agent_hook_policy`" | Both are implemented and tested in Phase 1 (core:agent.rs, bin:run.rs:127-166). | wording |
| 22 | README.md:90: marker lookup "under `$(git rev-parse --git-common-dir)/sluice/`" | Markers live in `<common-dir>/sluice/markers/` (core:marker.rs:112); cache-key.md:111 is correct. | wording |
| 23 | architecture.md:24 `sluice init`; architecture.md:26 `sluice logs [<job-id>]`; architecture.md:23 `cache status\|clear [--gate]` | README.md:125-128 and `--help` add `init --force`, `logs -n`, `cache status --all`; the two docs disagree (bin:main.rs:53-117). | wording |
| 24 | phase-1-cache-only.md:21 lists `version()` and `head_tree()` among delivered `git.rs` symbols; :23 lists `normalize_cmd()` in `key.rs` | The binary uses none of them; `Gate::normalized_cmd` duplicates `key::normalize_cmd` (core:config.rs:147-149 vs core:key.rs:99-101). | internal |

## Summary

Phase 1 is largely faithful to the documentation: the cache key layout, marker
JSON, TTL default, file names, exit codes 0/3/124/1, default values (timeout 30m,
weight 1, TTL 30 days, tail 40, budget `max(2, cpu-1)`, nice 10), the `--hook`
semantics of ADR-0006 and the test counts all match. The real gaps are:
README and `--help` present daemon, coalescing, PSI and MCP behaviour as if
present; the `SLUICE_DISABLE` bypass is not a true bypass; the repo-level
`defaults.tail_lines` (which `sluice init` writes) is ignored; the fail-fast
message sends agents to a `verify` tool that does not exist yet; fail-fast also
fires when caching is off (for example any agent hook under `CI`); pre-push
tree selection and `--explain` are described but absent; `SLUICE_LOG` is
documented but unimplemented; and a large amount of user-visible output
(notices, miss-record schema, validation errors, `cache status` format, exit 70)
is not documented anywhere.

Counts: Table 1 (documented but not implemented) = 10; Table 2 (implemented but
not documented) = 27; Table 3 (mismatches) = 24.

## Resolution (2026-10-22, lead)

Every finding was handled the same day, in code or in documentation:

| Table | Items | Resolution |
|---|---|---|
| 1.1–1.4, 1.10 | README/`--help` present daemon, coalescing, PSI, MCP, `docs/agents.md` as present | Rewritten with phase markers; exit 70 documented; ADR-0004 points `docs/agents.md` to Phase 3. |
| 1.5 | pre-push tree selection | Design simplified to "always the working-tree hash" (equal to the pushed commit's tree on a clean checkout); `cache-key.md` and `architecture.md` updated. |
| 1.6 | `--explain` | Implemented (`sluice run --explain <gate>`), tested. |
| 1.7 | `SLUICE_LOG` | Removed from the docs; unused tracing dependencies removed from the CLI crate. |
| 1.8 | public contract | `cache-key.md` now documents path, file name, JSON and layout as the contract; README links it. |
| 1.9 | git ≥ 2.31 | `discover()` maps the `--path-format` failure to `git <v> is too old: Sluice needs git 2.31 or newer`. |
| 2.1–2.27 | undocumented behaviour | Documented in `architecture.md` (sections 1, 4b, 8, 9, 9b), `cache-key.md`, `config-format.md`, README; `cache status` now also reports `cache = false` gates. |
| 3.1 | `SLUICE_DISABLE` | Needs the repo and gate command (documented); user config is no longer loaded under bypass (code). |
| 3.2 | `defaults.tail_lines` | Now `Option<usize>` overriding `cache.tail_lines` (code, tested). |
| 3.3, 3.16 | `CI` rule and `env_any_set` example | Docs aligned with the code (`CI` falsy keeps the cache; default `["CLAUDECODE"]`). |
| 3.4 | `depends_on` | Documented as validated, not scheduled. |
| 3.5, 3.13 | fail-fast on uncached runs; exit codes | Policy now applies only to a consulted-and-missed cache (code, tested); exit codes 2/70/143 documented everywhere. |
| 3.6–3.12, 3.17–3.19, 3.21–3.24 | wording | Docs corrected (TTL on read, `cache status/clear` semantics, probe list, key prefix pinned to `CACHE_FORMAT_VERSION` by a test, job id format, `.sluice.toml` location and error message, roadmap row, markers path, command list, duplicate `normalize_cmd` removed). |
| 3.14, 3.15 | ADR-0004 | Accepted; `CLAUDE*` stripping and crate name corrected. |
| 3.20 | fail-fast message | Now points to `sluice run <gate>` only; the `verify` mention returns with Phase 3. |
