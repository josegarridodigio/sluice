---
title: "Phase 6 reconciliation"
tags: [sluice, change, reconciliation, phase-6]
---

# Phase 6 reconciliation (2026-10-22)

Every behavioural statement of the Phase 6 documents was compared with the
code: `design/merge-queue.md`, `design/adr/0009-merge-queue.md`,
`design/mcp-tools.md` (queue tools), `design/protocol.md` (queue messages,
`via`, `features`, `log_level`), `design/config-format.md` (`[queue]` in both
files, `[debug]`), `docs/merge-queue.md` and `docs/debugging.md`. The
documentation was corrected; no behaviour changed. Doc line numbers in the
tables are those before the edits; code line numbers are at HEAD `259b19d`.

## Counts

| Document | Checked | Mismatches | Gaps | Fixed |
|---|---|---|---|---|
| `design/merge-queue.md` sections 1-7 | ~170 | 12 | 11 | 23 |
| `design/merge-queue.md` sections 7-15 and ADR-0009 | ~104 | 10 | 12 | 22 |
| `design/mcp-tools.md` queue tools | 48 | 2 | 3 | 5 |
| `docs/merge-queue.md` | 42 | 2 | 3 | 5 |
| `design/protocol.md` | ~120 | 3 | 5 | 8 |
| `design/config-format.md` | ~95 | 5 | 5 | 10 |
| `docs/debugging.md` | ~35 | 0 | 0 | 0 |

Statements with no implementation: only "member retried, config drift" as a
reason for `ready, approved -> queued` (removed). Implementations with no
statement: all meaningful ones were added (see the Gap rows).

## merge-queue.md sections 1-7

| Statement/location | Code location | Finding | Resolution |
|---|---|---|---|
| merge-queue.md:55-57 refusal tokens | target.rs:44-47,203; extension.rs:365 | Gap: token `invalid_base` (worktrees cannot be listed, `bad-request`) not listed | Added to the token list |
| merge-queue.md:71-78 endpoint binding | target.rs:303-318 | Gap: local target endpoint hash is `sha256("local:"+common dir)`, not stated | Stated |
| merge-queue.md:60-61 train hash | model.rs:224-250; runner.rs:1369-1380 | Gap: encoding (`sluice-train-v1` prefix, NUL-terminated fields, `\|` separator, `gate=key`/`gate=-` entries) not stated | Stated |
| merge-queue.md:62-64 lease | store.rs:703-713, 1019-1060, 830-905 | Mismatch: "every write is CAS on claim_token and state"; job updates have no claim_token (CAS on state only), the lease is checked in `Writer::new`; train updates CAS on token+state | Reworded |
| merge-queue.md:79-80 audit ref | push.rs:171-189; core config.rs:96,114 | Gap: audit ref is conditional on `[queue] audit_ref` (default true) | Stated |
| merge-queue.md:93 merging->conflict | runner.rs:836-870, 1233-1260 | Gap: reasons `conflict`, `merge_failed`, `dirty_merge` all land the job in `conflict` | Stated in table |
| merging->failed (doc :94) | runner.rs:777-799 | Gap: `internal` failure also applies to `verifying` jobs | Stated |
| verifying->failed (doc :96) | runner.rs:1090-1110, 913-925 | Mismatch: "single-member train" is "single surviving member"; `gate_failed` reason; error status also fails; tree_mismatch is before gates | Fixed |
| ready,approved->queued (doc :99) | runner.rs:345-378, 1445-1472; extension.rs:575-585 | Mismatch: "member retried, config drift" has no implementation (retry only from conflict/failed; no drift check). Real causes: `stale_plan`, `endpoint_changed`, worktree HEAD/clean check | Fixed |
| pushing->queued / pushing->failed (doc :103-104) | runner.rs:1620-1660; push.rs:131-160 | Gap: failed also covers refused refspec and declining `pre-push` hook (`pre_push_hook`); local CAS failure -> queued | Stated |
| doc :122-124 train states | model.rs:149-164 | Gap (minor): train transition table (TRAIN_TRANSITIONS) is a subset of members' mirroring; consistent, nothing changed | none |
| doc :128-131 schema | store.rs:29-65 | Consistent (all columns, index, `queue_schema` v1, fail-closed) | none |
| doc :170-178 retention/credentials | store.rs:880-885,1060-1085,1125-1150; runner.rs:236-262 | Consistent | none |
| doc :182-192 runner intro | runner.rs:208-250 | Gap: `[queue] enabled=false` makes every queue-* request fail with `not-supported` (extension.rs:213-219) | Stated |
| doc :199 max_train | core config.rs:113 | Gap: default 4 not stated | Stated |
| doc :202 fetch command | git.rs:216-231 | Mismatch: `--quiet` is also passed | Added |
| doc :225-226 dirty merge | runner.rs:1262-1275 | Mismatch: job goes to `conflict` (not a separate state), `git clean -fd`, paths in detail | Fixed |
| doc :239 step 6 | runner.rs:965-1035 | Gap: no gates (no config) -> `ready` immediately | Stated |
| doc :251-252 step 7 | runner.rs:1090-1125 | Mismatch: single/multi decided on surviving (verifying) members, not claimed members | Fixed |
| doc :280-281 pre-push retry | runner.rs:1452-1466 | Gap: unresolvable push URL also leaves train `approved` and retries | Stated |
| doc :289-291 push command | push.rs:171-189 | Mismatch: audit options/refspec only when `audit_ref` true | Fixed |
| doc :295-296 push env | git.rs:25-41,56-70 | Gap: allowlist contents not enumerated | Enumerated |
| doc :312-315 local target ambiguity | runner.rs:1550-1556 | Mismatch: spawn error of `update-ref` yields `needs_reconcile` too | Stated |
| doc :325-327 classification | push.rs:93-96 | Gap: exit 0 with a `!` ref line is ambiguous | Stated |
| doc :354-355 rows 7/9 | store.rs:1236-1290; extension.rs:723-733 | Mismatch: row 9 also honours pending cancels, and queued members get `attempts+1` | Fixed |
| doc :395 SQLite busy | daemon extension.rs:133-147; queue extension.rs:61-71 | Mismatch: wire error is code `internal` message `database busy, retry later`; typed `StoreError::Busy` internal | Fixed |
| Section 1 (goals, 9 statements) | extension/runner/push as cited in sections 5-6 | 9 checked, 9 consistent | none |
| Section 2 (concepts, ~30 statements) | target.rs, git.rs, store.rs, model.rs | ~30 checked; 5 findings above (token, local endpoint, hash encoding, lease CAS, audit ref); rest consistent (resolve order, tokens/error codes, `--`/`--end-of-options`, push URL, userinfo strip, pin ref, marker FULL, ids) | see rows |
| Section 3 (table 25 rows + prose ~10) | model.rs:106-175; runner.rs; extension.rs | ~35 checked; 5 findings above; JOB_TRANSITIONS and prose consistent (cancel semantics, train_id clearing, terminal states) | see rows |
| Section 4 (schema, ~25 statements) | store.rs | ~25 checked, 0 mismatches except 0; all consistent | none |
| Section 5 (steps 1-9, ~45 statements) | runner.rs, git.rs, target.rs | ~45 checked; 7 findings above; rest consistent (tick 5s, 60s, claim rules, 3 failures, paced retry, worktree flags, merge args, key derivation, RunRequest fields, statuses, watch, housekeeping) | see rows |
| Section 6 (push + reconcile, ~40 statements) | runner.rs, push.rs, reconcile.rs, extension.rs | ~40 checked; 6 findings above; decision table rows 1-10 match `reconcile::decide` | see rows |
| Section 7 (failure handling, ~12 statements) | runner.rs, store.rs | ~12 checked; 1 finding above | see rows |

## merge-queue.md sections 7-15 and ADR-0009

| Statement/location | Code location | Finding | Resolution |
|---|---|---|---|
| merge-queue.md:380-381 "daemon start dissolves `merging`/`verifying` trains" | sluice-gate-queue/src/extension.rs:274-325 (`start_recovery`), proto lib.rs:753 | Mismatch: trains have no `merging` state; code dissolves `building`/`verifying` trains, re-queues stranded `merging`/`verifying` jobs, and keeps (adopts) `ready`/`approved` trains (store.rs:1160 `adopt_trains`) | Reworded in section 7 |
| merge-queue.md:382 "marker present -> needs_reconcile" | extension.rs:284-303 | Gap: every `pushing` train is parked, marker present, absent or push alive (alive push logged, not killed) | Documented |
| merge-queue.md:389-390 push failures | runner.rs:1630-1650, 1558-1580; push.rs:25-30 | Gap: `push_rejected` (policy rejection) and `target_checked_out` (local target checked out at landing) job failures were undocumented | New bullet in section 7 |
| merge-queue.md:402 CLI text "run 'sluice daemon stop'" | sluice-gate/src/queue.rs:103 | Mismatch: text ends "... and retry" | Doc quotes full text |
| merge-queue.md:411-412 error codes (4 listed) | proto lib.rs:520-540 (`ErrorCode::NotSupported`), extension.rs:216 | Gap: `not-supported` missing | Added |
| merge-queue.md:413 `status.queue` summary | proto lib.rs:558-568, model.rs:225 `job_is_active` | Gap: meaning of `active`/`ready`/`needs_reconcile` not stated | Added |
| merge-queue.md:429-433 enqueue refusals | sluice-gate/src/queue.rs:204-240; extension.rs:345-390; target.rs:62 | Mismatch/gap: "dirty worktree" is tracked changes only; detached HEAD/unknown branch are client side; own-branch and checked-out target -> `not-allowed`, bad base -> `bad-request` | Rewritten |
| merge-queue.md:427 approve "asks 'yes'" | queue.rs:578 `confirm("Land this train? [y/N] ")` | Mismatch: prompt is `[y/N]`, accepts y/yes | Comment fixed |
| merge-queue.md:438-439 exit code 4 list | queue.rs:112-117 | Mismatch: `duplicate-branch` also exits 4 | Added |
| merge-queue.md (CLI scoping) | queue.rs:273-285, 737-760, 815-826 | Gap: `status`/`reconcile` scoped to current repo when inside one; `retry` must run in the job's repo | Documented |
| merge-queue.md:441-449 MCP | mcp/src/queue.rs:225-440, lib.rs:1105-1340 | Gap: client-side error codes (`dirty-worktree`, `detached-head`, `unknown-branch`, `not-a-repository`), progress notifications of `queue_approve`, `not-found` for foreign jobs in retry/cancel, status scoped to working_directory repo | Documented |
| merge-queue.md:456-458 `Extension` trait sketch | sluice-gate-daemon/src/server/extension.rs:41-77 | Mismatch: `handle` returns `Option<ServerMessage>`, methods return `BoxFuture`; `on_start` and `status_summary` omitted | Rewritten |
| merge-queue.md:463 `DaemonHandle` list | extension.rs:164-345 | Gap: `log_dir()` omitted | Added |
| merge-queue.md:504-505 `git::ENV_ALLOWLIST` | sluice-gate-queue/src/git.rs:29; queue.rs:30; mcp/queue.rs:24 | Mismatch (ambiguous path): constant lives in the queue crate, mirrored by clients | Path made explicit |
| merge-queue.md:507-508 "Endpoint URLs are stored without userinfo" | target.rs:294-323, store.rs (no url column) | Mismatch: URLs are not stored at all, only `endpoint_hash` of the stripped URL | Reworded; also documented `GIT_TERMINAL_PROMPT=0` (queue/src/git.rs:61-70) |
| merge-queue.md:485-492 config | core/src/config.rs:81-135, 278-295, 441-475 | Gap: validation rules (max_train>=1, network_timeout>0, remote name, unknown gates, unknown keys) undocumented | Added paragraph |
| merge-queue.md:526 test 7 "endpoint_changed refusal" | sluice-gate/tests/queue_e2e.rs:888-930 | Mismatch: e2e shows approved train dissolved with `endpoint_changed` and rebuilt; reconcile refusal is in test 1 (queue_e2e.rs:398) | Reworded |
| merge-queue.md section 13 test names | queue_e2e.rs, queue/tests/queue.rs | Gap: no mapping to test names | Mapping and extra tests added |
| merge-queue.md:551-553 enqueue handling | extension.rs:345-390 | Gap: error codes for bad base/own branch/checked-out | Added |
| merge-queue.md:559-560 retry | extension.rs:517-560 | Gap: `not-found`, `bad-request` for bad head; `reason`/`detail` also cleared | Added |
| merge-queue.md:566-567 enabled=false | extension.rs:196-224, 143-170; server/extension.rs:50 | Gap: failed `on_start` disables extension (no features, `error{internal}`); runner idle when disabled | Added; also `queue-status` repo filter bullet |
| adr/0009:44 push `--force-with-lease=<target>:<base>` | push.rs:163-180 | Mismatch/gap: full ref is `refs/heads/<target>`, audit ref has its own expect-absent lease, local target uses `update-ref` CAS | Reworded |
| adr/0009:59-60 "restart dissolves unfinished trains" | extension.rs:274-325, store.rs:1160 | Mismatch: only `building`/`verifying` dissolved, `pushing` parked, `ready`/`approved` kept | Reworded |
| **Summary section 7 (failure handling)** | runner.rs, store.rs, extension.rs, push.rs | 15 statements checked; 12 consistent (base failure recording/pacing/3-strike, retry, conflict, lease lost, remote moved, hook declined/hung, stalled remote, SQLite busy); 2 mismatches, 1 gap, all fixed | |
| **Summary section 8 (protocol)** | proto lib.rs | 9 checked; 7 consistent; 1 mismatch, 2 gaps, fixed | |
| **Summary section 9 (CLI/MCP)** | sluice-gate/src/queue.rs, mcp | 14 checked; 10 consistent (output formats, wait bound, `--yes`, hash, dissolved-train hiding, MCP gate); 2 mismatches, 3 gaps, fixed | |
| **Summary section 10 (APIs)** | daemon server/extension.rs, core git.rs, executor.rs | 10 checked; 8 consistent (QueueOptions, modules, git helpers, executor helpers, store_tx*); 1 mismatch, 1 gap, fixed | |
| **Summary section 11 (config)** | core config.rs | 11 checked; 11 consistent (keys, defaults, strict parser, allow_agent_approval only in user config); 1 gap (validation), fixed | |
| **Summary section 12 (security)** | queue/git.rs, store.rs, target.rs, docs/agents.md:158 | 9 checked; 7 consistent; 2 mismatches (ENV_ALLOWLIST path, URL storage), fixed | |
| **Summary section 13 (acceptance)** | queue_e2e.rs | 8 tests checked against code; 7 consistent; 1 mismatch (test 7 wording), mapping gap; fixed | |
| **Summary section 14 (decisions)** | runner.rs, push.rs, reconcile.rs, config.rs | 9 decisions checked; 9 consistent | |
| **Summary section 15 (request handling)** | extension.rs | 12 checked; 9 consistent; 3 gaps, fixed | |
| **Summary ADR-0009** | as above | 16 statements checked; 14 consistent; 2 mismatches, fixed | |

## mcp-tools.md (queue tools) and docs/merge-queue.md

| Statement/location | Code location | Finding | Resolution |
|---|---|---|---|
| docs/merge-queue.md:94 `approved; pushing...` | crates/sluice-gate/src/queue.rs:392 (`sluice: approved; pushing…`) | Output text differs (prefix, ellipsis char) | Doc text corrected |
| docs/merge-queue.md:70-76 status output | crates/sluice-gate/src/queue.rs:~395-430 render_status (`jobs needing attention:`, empty-queue line) | Meaningful behaviour with no statement | Added to doc |
| docs/merge-queue.md:108-116 reconcile | crates/sluice-gate/src/queue.rs do_reconcile (`no train needs reconciling`) | Gap: empty-result line undocumented | Added |
| docs/merge-queue.md:123 exit 1 "daemon prints the daemon's message" | crates/sluice-gate/src/queue.rs send() (client-side message "does not support the merge queue") | Message comes from the client, not the daemon; exit 1 also covers `not-supported` when `[queue] enabled = false` (crates/sluice-gate-queue/src/extension.rs:216) and declined/expired approve | Row rewritten |
| docs/merge-queue.md:125 exit 4 | queue.rs send() ErrorCode map; do_approve non-ready train -> EXIT_REFUSED | Gap: approve of non-ready train and own-branch enqueue also exit 4 (target.rs is_policy) | Examples added |
| kb mcp-tools.md:181-182 "server identifies itself as `sluice-gate-mcp <version>`" | sluice-gate-proto/src/client.rs:114 (`sluice-gate`), mcp/lib.rs:1358 (`sluice`), mcp/daemon.rs:88 | Queue tools use the proto client (`sluice-gate <version>`); MCP server info is `sluice` | Corrected |
| kb mcp-tools.md:187-190 daemon error codes list | crates/sluice-gate-queue/src/extension.rs:342-368 (`bad-request`) | `bad-request` missing from list (also used at line 213-218 for enqueue) | Added; noted `not-supported` for disabled queue |
| kb mcp-tools.md:226-239 queue_status text | mcp/src/queue.rs render_status (empty text, `jobs outside a train:`) | Empty-queue text undocumented | Added |
| kb mcp-tools.md:258 step 4 "approved/pushing" | mcp/src/lib.rs:1293 (loop also while `ready`) | Wait loop also covers `ready` | Corrected |
| kb mcp-tools.md:265-266 cancel | mcp/src/lib.rs:1303-1311 (`cancelled: stopped waiting...` error result) | Cancel text undocumented | Added |
| SUMMARY docs/merge-queue.md (whole file) | CLI --help of all 6 subcommands, queue.rs, config.rs, queue crate | 42 statements checked (flags, defaults, config keys, exit codes, push args, timeouts, policy); 37 consistent; 2 mismatches, 3 gaps; all fixed. Every flag and default in `--help` matches | Fixed |
| SUMMARY kb mcp-tools.md "Merge-queue tools" lines 161-292 + queue mention at 302 | mcp/src/queue.rs, lib.rs | 48 statements checked; 43 consistent; 2 mismatches, 3 gaps; fixed. Tool names, input params, schema mirrors, allow_agent_approval gating (user config only, fail-closed, no daemon contact), env allowlist, `via: mcp`, timeouts verified | Fixed |

## protocol.md, config-format.md, debugging.md

| Statement/location | Code location | Finding | Resolution |
|---|---|---|---|
| protocol.md (whole, ~120 statements: transport, handshake, run/status/logs/event/stop/cancel, queue-* requests and replies, views, via, features, log_level, versioning) | crates/sluice-gate-proto/src/lib.rs, client.rs; crates/sluice-gate-daemon/src/server.rs; crates/sluice-gate-queue/src/extension.rs | 120 checked, 112 consistent, 3 mismatches, 5 gaps | see rows below |
| protocol.md:257-258 "a second `hello` -> `duplicate hello`" (no code given) | server.rs:1385 | Code is `protocol` | Stated `protocol` explicitly |
| protocol.md:250-255 "last five codes only for queue-*"; `not-allowed` = "e.g. agent approval" | extension.rs:366 (enqueue policy refusal), 432-436 | `not-allowed` is also sent for enqueue targets the policy forbids; queue-* also uses bad-request/not-found/internal (extension.rs:342-378, 512, 68) | Error section rewritten with the full set of causes |
| protocol.md:255 `not-supported` = "no extension serves the request" | extension.rs:216-220 | Also sent when user config has `[queue] enabled = false`; undocumented | Documented, with message text |
| protocol.md Errors (gap) | server.rs:1375-1382 | `cancel` outside a run connection -> `bad-request` "cancel outside a run connection" was undocumented | Added |
| protocol.md Errors (gap) | server.rs:1254-1259 | Malformed first line -> `bad-request` "malformed hello: ..." undocumented | Added |
| protocol.md:36-41 `features`, `FEATURE_QUEUE`, `Client::features()/supports()` | lib.rs:49,343; client.rs:195,200; server.rs:1268 | Consistent | None |
| protocol.md:123-127 `daemon.log_level`, test name, `sluice status` line | lib.rs:587; server.rs:1445; status.rs:106; tests/status_schema.rs:120 | Consistent | None |
| protocol.md:194-232 queue-* fields, `via` optional on enqueue/retry/cancel, required on approve, views, state enums, reconcile decisions | lib.rs:117-182,186-210,435-470,720-930; extension.rs:144-148,239 | All fields, types, defaults and enum values consistent | None |
| protocol.md:265-278 versioning rules | lib.rs:16,39-52 | Consistent (no deny_unknown_fields; PROTOCOL_VERSION 1; schemas 1) | None |
| config-format.md (whole, ~95 statements) | crates/sluice-gate-core/src/config.rs; sluice-gate-queue; sluice-gate/src/{logging,daemon_cmd,doctor,clientlog}.rs | 95 checked, 85 consistent, 5 mismatches, 5 gaps | see rows below |
| config-format.md:8-11 "frozen once Phase 1 ships; until then additions allowed" | config.rs (queue/mcp/debug added later) | Stale wording | Reworded as frozen, additive-only |
| config-format.md:19-45 example lacks `[queue]` | config.rs:65-110 | Repo `[queue]` not shown in example (gap) | Added `[queue]` block |
| config-format.md:44 `depends_on` comment "run only after these gates passed" | config.rs:166 ("reserved, unimplemented"), doc line 70/182 | Example comment contradicted the field table | Comment now says reserved, not scheduled |
| config-format.md:69-71 three field rows after a blank line | markdown | Rows detached from the table (rendered as plain text) | Moved into the field table |
| config-format.md `[queue]` repo-only vs user-only (gap) | config.rs:65,88,447 (both deny_unknown_fields); target.rs:71-78 | Which file may set which key, base-commit read and fallback to defaults, and validation messages were undocumented | Added rule bullet with the four validation messages (config.rs:272-292) |
| config-format.md:159-172 "idle_exit parsed and validated"; "everything else is live" | config.rs:703-760 (no idle_exit check); runner.rs:211-216,259; extension.rs:216,432; server.rs:1445 | idle_exit is not validated. User `[queue]` (enabled, tick, retention, allow_agent_approval) and `[debug]` are read once at daemon start (handle.config() = start snapshot), not live; MCP/CLI re-read allow_agent_approval/tick per call (mcp lib.rs:1230, queue.rs:601) | Corrected both statements; documented 50 ms tick floor and absence of `[queue]` validation |
| config-format.md env table (gap) | update.rs:31; scripts/install.sh:5-6 | `SLUICE_UPDATE_BASE_URL`, `SLUICE_VERSION`, `SLUICE_INSTALL_DIR` undocumented here | Added rows |
| config-format.md:146 `SLUICE_DISABLE=1` | mode.rs:40 | Any truthy word works | Row now says "truthy" |
| config-format.md `[queue]` defaults/types (remote, target, gates, max_train, audit_ref, push_timeout, network_timeout; enabled, allow_agent_approval, tick, retention), `[debug]` (level, daemon_log_max_mib, SLUICE_LOG precedence, restart --debug), `[mcp]` floor 1 s | config.rs:76-110,438-470,540-575,676-690; mcp lib.rs:923; main.rs:255-264 | Consistent | None |
| docs/debugging.md (whole, ~35 statements) | config.rs:540-575; daemon_cmd.rs; clientlog.rs; doctor.rs; run.rs:50; logging.rs; mcp lib.rs:95-125 | 35 checked, 35 consistent: [debug] keys/defaults, SLUICE_LOG precedence, rotation (.1-.3, 5 MiB x1), decision values, exit 3, doctor checks, bundle contents and redaction | No change needed |

Open discrepancies: none
