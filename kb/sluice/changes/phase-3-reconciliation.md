---
title: "Phase 3 reconciliation report"
tags: [sluice, reconciliation, phase-3]
---

# Phase 3 reconciliation report

Scope: Phase 3 (MCP server, shared `lookup` module, fail-fast message,
`sluice mcp`). Docs reviewed: `design/mcp-tools.md`,
`design/adr/0005-mcp-verify-blocking-or-handle.md`, `design/architecture.md`
(sections 1, 2, 7, 8, 9), `design/config-format.md` (`[mcp]`, `SLUICE_LOG`,
env table), `changes/phase-3-mcp.md`, `docs/agents.md` (plus
`docs/manual-test-phase-3.md`, which the guide points to), `README.md`
(status banner, "What Sluice does", "Command surface", env and exit-code
paragraphs) and the `--help` of `sluice mcp` and `sluice run`. Statements
about Claude Code were checked against `research/claude-code-facts.md`.
Code: `crates/sluice-gate-mcp/src/{lib,daemon,text}.rs` (`mcp:`),
`crates/sluice-gate-core/src/{lookup,config,mode}.rs` (`core:`),
`crates/sluice-gate/src/{run,main,cache}.rs` (`bin:`),
`crates/sluice-gate-proto/src/client.rs` (`proto:`) and
`crates/sluice-gate/tests/mcp.rs` (`test:`). Doc paths are relative to
`kb/sluice/` unless they start with `README.md` or `docs/`. Findings come from
reading the code; nothing was executed except `--help` and
`cargo test --workspace -- --list` (207 tests, matching
`changes/phase-3-mcp.md:26`). No file other than this report was written.
Phase 4/5/PSI statements were skipped.

Fail-fast wording, compared line by line: `bin:run.rs:182-184`,
`design/architecture.md:201`, `design/mcp-tools.md:129-131` and
`docs/agents.md:101-103` carry the same three lines (`'<gate>' has not passed
on this tree (<why>).` / `<hook> was invoked by a coding agent (<rule>) and
agent_hook_policy is "fail-fast".` / `call the 'verify' MCP tool (or run
'sluice run <gate>') first, then retry the git command.`), and
`bin:tests/run.rs:244` asserts the new last line. The only wording problem is
the label in `docs/agents.md:97-108` (table 3, row 11). The `instructions`
text (`mcp:lib.rs:57-64`) is only paraphrased in the docs (table 3, row 15).

## 1. Documented but not implemented

| # | Statement | Doc file:line | What the code does instead | Code file:line |
|---|---|---|---|---|
| 1 | Results use `Json<T>` "where the text summary is not needed", hand-built `CallToolResult` only for `verify` | design/mcp-tools.md:44-45 | No tool uses `Json<T>`. Every tool builds a `CallToolResult` by hand with a text block plus `structured_content` (`result()` for `verify`, `gate_list`, `logs`; inline for `status`). | mcp:lib.rs:314-327, 894-896 |
| 2 | "Commands of a later phase exit 70"; "70 command not implemented yet" | README.md:157-158, design/architecture.md:227 | No command is left unimplemented (`mcp` was the last) and no code path returns 70. `--help` no longer lists it. | bin:main.rs:157-194, bin:main.rs:36-39 |
| 3 | Manual step 5: after Agent A's `verify` passed, a trivial change to `README.md` is committed and the `pre-commit` hook prints `PASS (cached, ...)` | docs/manual-test-phase-3.md:54 | The key covers the exact working-tree content, so editing `README.md` changes the tree and the hook misses; for an agent it exits 3 (the behaviour step 6 expects for a changed tree). The step can only hit if the commit changes nothing in the tree. | core:lookup.rs:276-296, bin:run.rs:125-189 |
| 4 | Progress notifications keep a 3-minute gate alive under a 60 s idle timeout; a gate over 2 minutes survives auto-backgrounding | design/mcp-tools.md:145-149, design/adr/0005-mcp-verify-blocking-or-handle.md:57-61 | Implemented on the server side (tested at a 1 s interval), but the Claude Code side is unevidenced: `changes/phase-3-mcp.md:41-42` and `research/claude-code-facts.md:21` say it is unknown whether Claude Code sends a `progressToken`. `phase-3-acceptance.md` does not exist yet. | mcp:lib.rs:812-817, test:mcp.rs:246-259 |

## 2. Implemented but not documented

| # | Behaviour | Code file:line | Suggested doc location |
|---|---|---|---|
| 1 | A request the daemon answers `cached` (its double check found a marker) renders `PASS (cached)` without the age, with `cached: true`, no `duration_ms`, and `job_id` set to the done job (not the job that wrote the marker, unlike a client-side hit). | mcp:lib.rs:536-547, 556, 567, 602-604 | mcp-tools.md `verify` text and structured result |
| 2 | Error mapping. `invalid_params`: relative `working_directory` (`working_directory must be an absolute path, got '<dir>'`), any `resolve_repo` failure (not a repository, missing or invalid `.sluice.toml`, invalid user config), no gates defined, unknown gate (`unknown gate '<g>' (defined: ...)`), unsafe `job_id`, `status` on a non-repository. `internal_error`: daemon unavailable (`the Sluice daemon is unavailable: <e>`), refusal before `queued` (`the daemon did not accept the request: <why>`), connection lost after `queued` (`lost the connection to the daemon: <why>`), failed request material, failed blocking task, `status` when the daemon query fails for a reason other than "not running". A protocol mismatch reads `the running daemon speaks protocol N, this server speaks M; run \`sluice daemon stop\``. | mcp:lib.rs:274-291, 368-380, 695, 736, 775, 808, 876-877, 968; mcp:daemon.rs:55-57, 100-105 | mcp-tools.md `verify` ("is_error" paragraph) |
| 3 | `timeout_seconds: 0` means no timeout (`timeout_ms = 0` goes to the daemon); large values saturate; the `TIMEOUT after <d>` line prints the override. | mcp:lib.rs:423-425, 726-728, 580-583 | mcp-tools.md parameter table |
| 4 | `extra_args` are not part of the cache key: a hit ignores them (a cached pass answers a call that asked for extra arguments), and a pass that ran with them writes the marker of the plain gate. Same rule as `sluice run -- args`, but no doc states it for either front-end. | core:lookup.rs:276-296, mcp:lib.rs:413-425 | mcp-tools.md parameter table; design/cache-key.md key material |
| 5 | Cache-off conditions apply to `verify` too: `CI`, `SLUICE_CACHE=0`, `cache = false`, an unknown tree and a failed snapshot or key make the gate run uncached through the daemon with `not_cached_reason` on a pass. `SLUICE_DISABLE` additionally skips reading the user config (default TTL, `tail_lines` and `progress_interval`). `status` always computes the cache section with caching enabled, whatever the environment. | core:lookup.rs:79-90, 179-234, core:mode.rs:39-59, mcp:lib.rs:562-565, 1019 | mcp-tools.md `verify` (only `SLUICE_DISABLE` is mentioned) and `status` |
| 6 | `mcp.progress_interval` has a floor of 1 s (`0s` is accepted and clamped), and the user config is re-read on every `verify` call, so changes apply without restarting the MCP process. No `[mcp]` validation rule exists. | mcp:lib.rs:815, core:config.rs:430-446, 931-941 | config-format.md validation list (line ~143) and `[mcp]` comment (line 107) |
| 7 | Progress notification shape: `progress` is seconds since the `verify` call began (no `total`); the message is `[i/n] <gate>: submitting to the daemon`, `<gate>: queued, position <p> (job <id>)` or `<gate>: running for <12.3s> (job <id>)`; the first one is sent one interval after the daemon phase begins; none is sent while the hit check or `ensure_daemon` runs. | mcp:lib.rs:433-510, 746-760 | mcp-tools.md `verify` progress paragraph |
| 8 | Default gate order is alphabetical (gates live in a `BTreeMap`); an empty `gates: []` is treated like an omitted one; a repeated gate name runs twice (the second is normally a hit). | mcp:lib.rs:362-393, core:config.rs:199-206, mcp:lib.rs:1165-1170 | mcp-tools.md parameter table |
| 9 | `status` details: with the tree unknown (or the snapshot failing) `cache.gates` is `[]` and `cache.tree_unknown` carries the reason; the cache entry per gate has `passed_at`/`job_id` on a hit, `last_run {recorded_at, reason, exit_code, job_id, log_path}` on a miss, `message` on `error`; a non-repository `working_directory` is `invalid_params` after the daemon query; `status` never starts the daemon. | mcp:lib.rs:256-270, 862-877, 1018-1102 | mcp-tools.md `status` |
| 10 | `logs` details: `tail` 0 becomes 1 and values over 2000 become 2000 silently; `omitted_bytes` counts only what the 64 KiB cap dropped; the lines are the raw file (they include the `# sluice job`, `# command:` and `# exit:` framing lines that `verify`'s tail strips); failures (no log directory, unknown job file) are `is_error` text results without `structured_content` although the tool declares an `output_schema`; the job file is read whole into memory. | mcp:lib.rs:970-1003, 1107-1139, mcp:text.rs:46-88 | mcp-tools.md `logs` |
| 11 | `gate_list` structured fields are `name`, `cmd` (trimmed), `weight`, `timeout_seconds` (0 = none), `hooks`, `cache`, sorted by name; the text line shows the whitespace-normalised command and a humantime timeout; an empty set prints `no gates defined in .sluice.toml`. | mcp:lib.rs:220-242, 915-947 | mcp-tools.md `gate_list` |
| 12 | `sluice-gate-proto` now ships the blocking client (`connect`, `ensure_daemon`, `Client::{run,status,logs,stop}`, `send_hit_event`) shared by the CLI and the MCP server, and `sluice-gate-core` gained `lookup`. The crate-layout block still describes proto as "IPC message types" and core as "config, cache key, markers, git tree computation". | proto:client.rs:55-292, core:lookup.rs:1-10 | design/architecture.md:53-57 |
| 13 | A client-side hit in `verify` sends the best-effort `hit` event to the daemon, only when the socket exists (never autostarts), so MCP hits count in `status` totals. | mcp:lib.rs:704-711 | mcp-tools.md `verify` |
| 14 | Cancellation details: before `queued` only the connection is dropped (no `cancel` frame); a cancel during the hit check, `ensure_daemon` or the handshake abandons the blocking task, which keeps running (a daemon start in progress still completes); a cancelled result starts with `sluice: the verification was cancelled; the daemon subscription was dropped.` and the pending gate shows `CANCELLED (job <id>)`; `all_passed` is false. | mcp:lib.rs:303-311, 518-527, 620-624, 837; mcp:daemon.rs:112-119, 127-135 | mcp-tools.md `verify` |
| 15 | `sluice mcp` exits 0 when the client disconnects and 1 with `sluice: error: <e>` when the runtime or transport fails; an invalid `SLUICE_LOG` filter is silently replaced by `info`. | bin:main.rs:199-217, mcp:lib.rs:74-94 | mcp-tools.md process model; config-format.md:133 |
| 16 | The server identifies itself to the daemon as `sluice-gate-mcp <version>` and forwards `CLAUDE_CODE_SESSION_ID` as `session` for attribution; `hook` is always null. | mcp:daemon.rs:86-90, core:lookup.rs:21, 375, mcp:lib.rs:353-356 | mcp-tools.md process model |

## 3. Mismatches

| # | Doc says (file:line) | Code does (file:line) | Severity |
|---|---|---|---|
| 1 | Example result for a cached pass has `"exit_code":0` (design/mcp-tools.md:88) | `exit_code` is `null` for a cached pass; `job_id`, `log_path` and `exit_code` are always serialised (null when absent), only `duration_ms`, `not_cached_reason`, `message`, `tail` are skipped (mcp:lib.rs:165-187, 198, 712-714) | wording |
| 2 | Text example `--- last 40 lines of <path> ---` (design/mcp-tools.md:79) | The number is the real count of tail lines, which depends on `defaults.tail_lines` / `cache.tail_lines`, and may be fewer; the tail is also attached to timeout and killed gates, not only to FAIL (mcp:lib.rs:590-598, 636, core:lookup.rs:253-258) | wording |
| 3 | Only "protocol-level problems (bad or relative `working_directory`, no `.sluice.toml`, invalid user config, unreachable daemon)" are MCP errors (design/mcp-tools.md:98-101) | Also a connection lost after `queued`, a refusal before it, and a failed request build are MCP errors; and any such error in gate k aborts the whole call: results of gates already run are discarded, there is no `is_error` report (mcp:lib.rs:695, 775, 820-830) | user-visible |
| 4 | `logs` "falls back to the log directory when the daemon is absent" (design/mcp-tools.md:123) | Falls back on any daemon failure, including `not-found` for jobs the daemon does not know (mcp:lib.rs:1112-1122) | wording |
| 5 | `logs` default is "the newest job" (design/mcp-tools.md:120, README.md:165) | Without a daemon answer the newest is the lexicographically greatest `*.log` file name, not the newest by time (mcp:lib.rs:1126-1134) | internal |
| 6 | Gates run "in the given order" (design/mcp-tools.md:58) | True only for an explicit `gates`; the default selection is alphabetical (core:config.rs:199-206, mcp:lib.rs:384-391) | wording |
| 7 | `SLUICE_DISABLE=1` "run the gate directly, no cache, no queue" (design/config-format.md:128, README.md:167, bin:main.rs:40) | Through `sluice mcp` it turns the cache off only; the gate still goes through the daemon queue (design/mcp-tools.md:69-71 is right) (core:lookup.rs:184-189, mcp:lib.rs:796) | user-visible |
| 8 | `status` returns "the cache state of the current tree" (docs/agents.md:50) | Only when `working_directory` is passed; the server does not know the agent's cwd (mcp:lib.rs:856-892) | user-visible |
| 9 | "Sluice sends progress notifications every 15 seconds" as what keeps a blocking call safe (docs/agents.md:19-22, README.md:183-184, ADR-0005:41-44); "no shell timeout applies to MCP tool calls" (design/architecture.md:188) | Sent only if the request carries a `progressToken` (mcp:lib.rs:812), and whether Claude Code sends one is unknown (research/claude-code-facts.md:21). Without it Claude Code's 30-minute idle window applies (facts:17); queue wait plus a long suite can exceed it. The guide gives no such warning; ADR-0005 does state the fallback | user-visible |
| 10 | Status banner "`sluice mcp` ... arrives in Phase 3; the MCP parts of this guide describe that design" and "`sluice mcp` arrives in Phase 3. Until then ..." (docs/agents.md:7-9, 40-43) | `sluice mcp` is implemented (bin:main.rs:76, 184-218; README.md:16-20 says so) | user-visible |
| 11 | The fail-fast block is "shown here as the Phase 1 wording" and "Phase 3 changes the last line" (docs/agents.md:97-98, 106-108) | The block already shows the Phase 3 last line, which is the code's only wording (bin:run.rs:182-184) | wording |
| 12 | `SLUICE_LOG` "sets the daemon's log filter ... written to daemon.log" (README.md:173-174) | Also the filter of `sluice mcp` (stderr). `--help` and config-format.md:133 say so (mcp:lib.rs:83-94, bin:main.rs:44) | wording |
| 13 | Empty fenced block after the command surface (README.md:152-153) | Stray "```" pair renders an empty code box | wording |
| 14 | The shipped `CLAUDE.md` section is quoted as "Before committing, call the `verify` tool. Do not run test, lint or type-check commands through Bash." (design/mcp-tools.md:132-134) | `docs/agents.md:64-79` has a six-bullet block that says "shell", not "Bash" | wording |
| 15 | `instructions` summarised as four points (design/mcp-tools.md:34-37) | The text also states that a passed tree is answered from the cache in milliseconds, tells the model to pass its absolute working directory, and names `status`, `logs` and `gate_list` (mcp:lib.rs:57-64); the test only checks `contains("verify")` (test:mcp.rs:99-103) | wording |
| 16 | `CLAUDE.md` block claims: default gates "are the ones tagged `pre-commit`"; "`verify` queues them [test, lint, type-check, build commands]" (docs/agents.md:67-71) | Falls back to every gate when none is tagged (docs/agents.md:49 says so, the block does not); only commands declared as gates are queued, others are not; the block never says `working_directory` must be absolute (relative is `invalid_params`) (mcp:lib.rs:282-291, 384-391). The other claims hold: "has not passed on this tree" is the exact start of the first fail-fast line (bin:run.rs:182), a pass is cached per exact tree, `sluice cache status` and `sluice logs` exist | wording |
| 17 | "every message and exit code is unchanged" for `run.rs` and `cache.rs` (changes/phase-3-mcp.md:16) | The fail-fast last line changed, as row 18 of the same note says (bin:run.rs:182-184) | wording |
| 18 | Cancellation fires "on `notifications/cancelled` and on transport EOF" (design/mcp-tools.md:64-66) | Only the `notifications/cancelled` path is tested (a dropped client call, test:mcp.rs:262-283, changes/phase-3-mcp.md:40); the EOF case is not covered | internal |
| 19 | Acceptance: `verify` miss then hit, `status`, `logs`, cancelled leaves no process (design/mcp-tools.md:141-144) | Covered (test:mcp.rs:83-285). Not covered by any automated test: coalescing (`coalesced: true`), `timeout_seconds`, `extra_args`, `timeout`/`killed`/`error` statuses, `SLUICE_DISABLE`, all gates running after a failure, the default pre-commit selection (unit test only, mcp:lib.rs:1152-1171), `cancelled: true` in the result, `logs` limits, unknown job and unsafe id, the daemon-unavailable error, progress message content | internal |
| 20 | `claude mcp add sluice -- sluice mcp` and a project `.mcp.json` with `{"mcpServers": ...}` (docs/agents.md:27-38, README.md:147, docs/manual-test-phase-3.md:39, design/mcp-tools.md:17-21) | Not in `research/claude-code-facts.md`, so unverified against its sources. The auto-backgrounding sentence (docs/agents.md:22-23) omits that it applies to interactive sessions only and does not survive session exit (facts:19) | wording |
| 21 | "`Bash(sluice:*)` can be allowed safely: `sluice run` is the queue, not a way around it" (docs/agents.md:177-178) | The in-process fallback runs outside the daemon budget when the daemon is down, `run-unqueued` policy skips the queue, and the same rule also allows `sluice daemon stop --force` (bin:run.rs:201-207, 191-193) | wording |
| 22 | Command surface lists `sluice run <gate> [--hook <hook>] [-v] [-- extra args]` and "143 gate killed by a signal" (README.md:137, 156-157) | `--explain` exists (design/architecture.md:19 has it) and 143 also covers `daemon stop --force` and dropped queued jobs (bin:main.rs:64-66, 36-39); `sluice run --help` is correct | wording |

## Summary

Phase 3 matches its design closely: the four tool names, the parameter names
and defaults (`working_directory`, `gates`, `timeout_seconds`, `extra_args`,
`job_id`, `tail` 100/max 2000, 64 KiB cap), the result fields (`repository`,
`results`, `all_passed`, `cancelled`, `status` values, `cached`, `coalesced`,
`tail`, `omitted_bytes`), `is_error` on any failed gate and on an unknown log,
no daemon fallback, progress every `[mcp] progress_interval` (default 15 s),
cancellation sending `cancel`, `status` returning `{"schema":1,"daemon":null}`
with no daemon, the fail-fast wording in all four places, the 207-test count
and the `[mcp]` / `SLUICE_LOG` config rows all agree. The important gaps are:
the guides present progress notifications as the guarantee that a long
`verify` survives, but they are sent only when the client supplies a
`progressToken`, which nobody has verified for Claude Code; `docs/agents.md`
still announces `sluice mcp` as "arrives in Phase 3" and labels the new
fail-fast wording as Phase 1; manual step 5 expects a cache hit on a tree it
has just changed; `extra_args` and the `timeout_seconds: 0` rule are
undocumented and `extra_args` silently share the plain gate's cache entry; an
error in one gate (daemon lost mid-run) discards the results of the gates
that already ran, while the docs describe only up-front errors; README and
architecture.md still promise exit code 70; and `SLUICE_DISABLE` is described
as "no queue" although the MCP path still queues. Most other differences are
wording or undocumented detail, and the automated MCP tests cover only the
happy paths plus cancellation.

Counts: Table 1 (documented but not implemented) = 4; Table 2 (implemented but
not documented) = 16; Table 3 (mismatches) = 22.

## Resolution (2026-10-22, lead)

| Items | Resolution |
|---|---|
| 2.4 | Code: extra arguments are folded into the key command (`lookup::key_command`, `key::command_with_args`) for the CLI, the MCP server and the marker's `cmd`; tests in `tests/run.rs` and `tests/mcp.rs`. Docs: mcp-tools.md, cache-key.md. |
| 3.3 | Code: a Sluice error on one gate becomes that gate's `error` result and the remaining gates still run; the call returns `is_error`. Test with an impossible socket. |
| 3.19 | Code: tests added for coalescing, `timeout_seconds`, `extra_args`, a gate after a failure, `logs` limits and an unknown job; `cancelled: true` cannot be observed over the wire (rmcp drops the response of a cancelled request), covered by a unit test. |
| 1.2, 3.13, 3.22 | README: exit 70 removed, stray fences removed, `--explain` listed. |
| 1.3 | `docs/manual-test-phase-3.md`: the change is made before `verify` so the commit hits. |
| 1.4, 3.9 | ADR-0005, architecture.md, `docs/agents.md`: progress notifications depend on a `progressToken`; the 30-minute idle window is the practical limit until the manual test confirms Claude Code sends one. |
| 3.10, 3.11, 3.8, 3.16, 3.21, 3.12 | `docs/agents.md` and README updated (status, `status` needs a directory, `CLAUDE.md` block, narrower allow rule, `SLUICE_LOG`). |
| 1.1, 2.1–2.3, 2.5–2.16, 3.1, 3.2, 3.4–3.7, 3.14, 3.15, 3.17, 3.18, 3.20 | Documented in mcp-tools.md, config-format.md, architecture.md (crate layout) and the change note. |
