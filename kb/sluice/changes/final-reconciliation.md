---
title: "Final reconciliation"
tags: [sluice, change, reconciliation]
---

# Final reconciliation (2026-10-22)

Scope: the whole KB after the Phase 1-5 reconciliations (items 2), the user
facing documents against the installed CLI (item 3, `sluice --help` and every
subcommand's `--help` of the binary built from HEAD `259b19d`) and the Pando
wanted concepts (item 4). The Phase 6 documents are in
[[sluice/changes/phase-6-reconciliation.md]]. Only documentation and one
`--help` string changed; line numbers on the doc side are after the edit
unless stated.

## Findings

| Statement / location | Code location | Finding | Resolution |
|---|---|---|---|
| `sluice mcp --help` and the top-level help list only `verify, status, gate_list, logs` (`crates/sluice-gate/src/main.rs:37`, `:80`) | `crates/sluice-gate-mcp/src/lib.rs:1121-1218` (nine `#[tool]` functions) | Mismatch: help omits the five queue tools | Help strings now name `queue_enqueue`, `queue_status`, `queue_approve`, `queue_retry`, `queue_cancel` (main.rs:37-38, 81-83); `sluice run fmt/clippy/test` green |
| `architecture.md:19-28` command list | `main.rs` `Command` enum (`queue`, `doctor`, `daemon restart`) | Gap: no `queue`, `doctor`, `daemon restart` | Added (architecture.md:24-30) |
| `architecture.md` section 3 user config | `core/src/config.rs` `[queue]`, `[debug]`, `[mcp]` | Gap: keys not mentioned | Added |
| `architecture.md` 4b directories | `queue/src/runner.rs` (`<state>/merge/<train>`), `core/src/logfile.rs` | Gap: `daemon.log` rotations, `mcp-<pid>.log`, `client.log`, `merge/` missing | Added to the table row |
| `architecture.md` 5.1 autostart | `daemon_cmd.rs:311-356`, `core/src/config.rs` `resolve_log_level` | Gap: log filter precedence, rotation and `daemon restart` | Added |
| `architecture.md` section 7 tools list | `mcp/src/lib.rs:758-1218` | Gap: queue tools and `mcp-<pid>.log` | Added |
| `architecture.md` 5.5/5.6/6 (extension point, queue crate, messages) | `daemon/src/server/extension.rs:41-338`, `server.rs:82` (`EXTENSION_GRACE` 10 s), `daemon_cmd.rs:84`, `proto/src/lib.rs:61-460` | Checked: trait methods, handle methods, 10 s grace, wiring and message names all match | None needed |
| `architecture.md` has no section for logs, doctor, update | `doctor.rs`, `update.rs`, `clientlog.rs`, `logfile.rs` | Gap | New section 5.7 |
| `executor.md:121` "no CLI command uses `Store::open_read_only` yet" | `sluice-gate/src/doctor.rs:359`, `:783` | Mismatch: `sluice doctor` uses it | Corrected (executor.md:127-131) |
| `executor.md` lifecycle has no restart | `daemon_cmd.rs:311-356` | Gap | Bullet "Restart" added |
| `executor.md` log retention, `mcp-*.log`, `client.log`, `daemon.log` rotation (lines 96-117) | `daemon/src/server.rs:133` (`DAEMON_LOG_KEEP` 3), `:425`, `:78` (24 h prune), `core/src/logfile.rs`, `clientlog.rs:26` (5 MiB) | Checked: consistent | None needed |
| `executor.md` lease 90 s / heartbeat 15 s | `server.rs:66`, `:68` | Checked: consistent | None needed |
| `scheduler.md` silent on queue-submitted jobs | `queue/src/runner.rs:992` (`session: queue:<train>`), `extension.rs:182` | Gap | Section "Merge queue jobs" added |
| `cache-key.md:81` committed-tree lookup "planned for Phase 2" | `core/src/lookup.rs:310-378` (field only recorded); decision in `committed-tree-lookup.md` | Mismatch with the recorded decision (no automatic lookup) | Reworded; cross-reference to the queue's key derivation added |
| `committed-tree-lookup.md:26` `sluice cache status` shows "passed with unstaged changes present" | no such string in `crates/` | Defect in code, not fixable here (behaviour change): feature promised and not implemented | Doc marked "Not implemented"; listed as a deferred item below |
| `master-plan.md` Phase 6 checklist (3 items unticked) and table row "in progress" | extension API, `sluice-gate-queue`, CLI/MCP all present; acceptance note exists | Mismatch | Items ticked, final item points to the reconciliation note; row set to done with change-note links |
| `master-plan.md` post-phase blocks not in status table | n/a | Gap | Rows 6a, 6b and final added; blocks verified item by item against code (logs, rotation, doctor, bundle, plugin files) |
| `master-plan.md` "Next steps" still says implement Phase 6 | n/a | Mismatch | Replaced |
| `open-questions.md:27` row Q14 sits after the "Decisions" section | n/a | Mismatch (layout) | Moved into the Phase 0 table |
| `open-questions.md` no record of the Phase 6 answers | `merge-queue.md` section 14 | Gap | Phase 6 section added |
| `README.md:16` "Phases 1-4 done ... first release comes next" | tag `v0.1.0` published; Phases 0-6 done | Mismatch | Banner rewritten |
| `README.md:140-153` command surface | `sluice --help` | Gap: `sluice update` missing | Added (README.md:155) |
| `README.md:170-171` stray empty code fence | n/a | Defect (markup) | Removed |
| `README.md:191` `SLUICE_LOG` covers daemon and mcp only | `sluice --help` (daemon, mcp, run; then `[debug] level`) | Mismatch | Corrected; `SLUICE_UPDATE_BASE_URL` added |
| `README.md:204` MCP tools list has four tools | `mcp/src/lib.rs` nine tools | Mismatch | Queue tools added |
| `README.md` roadmap stops at Phase 5 | master plan | Gap | Phase 6 and two post-phase rows added (README.md:243-245) |
| `docs/agents.md:180-190` fail-fast wording "Phase 1 wording" plus a dangling sentence fragment | `sluice-gate/src/run.rs:280-283` | Mismatch: message is unchanged, text was corrupt | Rewritten around the real three lines |
| `docs/agents.md` tool table, plugin text, queue rules | `mcp/src/lib.rs:59-76` (instructions), `plugin/hooks/guard.sh`, `plugin/.mcp.json` | Checked: consistent | None needed |
| `docs/validation.md:20-26` only the manual MCP registration | `plugin/README.md` | Gap: recommended path is the plugin | Plugin pointer added |
| `docs/merge-queue.md`, `docs/debugging.md` | see Phase 6 note | Checked there | None here |
| `plugin/README.md`, `plugin/skills/sluice/SKILL.md`, `reference.md` (tools, `verify` is sequential and runs every gate, status values, error codes, install lines, `SLUICE_GUARD`, `SLUICE_E2E_MODEL`) | `mcp/src/lib.rs:896-955`, `:1387-1408`, `:59-76`; `scripts/plugin-e2e.sh:28`; `plugin/.claude-plugin/plugin.json` | Checked: consistent | None needed |
| CLI flags, defaults, exit codes (`run`, `mcp`, `daemon` stop/restart, `status`, `cache`, `init`, `hook print`, `logs`, `queue` x6, `update`, `doctor`) vs README, `docs/*`, KB | `--help` of every subcommand | Checked: flags, exit codes 0/1/2/3/4/75/124/143 and env vars agree; `daemon` has no `start` subcommand (bare `sluice daemon` starts it) and no document claims one | None needed |
| Installation lines (installer, `sluice update`, plugin install, `cargo install --path`) | `scripts/install.sh:5-6,21,61`, `update.rs` | Checked: same commands and env vars in README, plugin README, SKILL.md, agents.md, CONTRIBUTING | None needed |

## Pando wanted concepts (item 4)

`kb_related_documents` with no arguments listed three unresolved link
targets: `sluice/plan/master-plan.md` (10 links),
`sluice/design/adr/0009-merge-queue.md` (3) and
`sluice/changes/final-reconciliation.md` (1). The first two files exist on
disk; Pando did not have them stored at the time of the query (the KB watcher
re-imports a file after an edit and both were being edited). The third is this
note. A second query after the watcher re-imported the files reported that every wiki link resolves. None of the three was a Phase 6 or post-phase concept that lacks a
document: every Phase 6 and post-phase document already exists
(`phase-6-*`, `debugging`, `self-update`, `agent-plugin`, ADR-0009).

## Deferred items

- None. The one item this pass found (`sluice cache status` did not show the
  "passed with unstaged changes present" note promised by
  `committed-tree-lookup.md`) was implemented right after it:
  `crates/sluice-gate/src/cache.rs` `status` and
  `MarkerSummary.committed_tree`, test
  `cache_status_notes_a_pass_recorded_with_unstaged_changes`; the design doc
  now describes the implemented note.

## Verification

`sluice run fmt`, `sluice run clippy`, `sluice run test` after the `main.rs`
help change: all three PASS.

Open discrepancies: none.
