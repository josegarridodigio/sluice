---
title: "Fix: a disabled cache is visible; doctor reads husky v9 hooks (v0.4.1 F4, F5)"
tags: [sluice, fix, cache, doctor, husky, client-log, mcp]
---

# Fix: cache off must be visible (F5), doctor on husky v9 (F4)

## F5: cache off

A `skip-worktree` or `assume-unchanged` index entry makes the tree unknown, so
no gate is cached. An afternoon ran uncached unnoticed.

- `Lookup::Off` (`crates/sluice-gate-core/src/lookup.rs`) gained `code:
  &'static str`; the tree-unknown notice is now `cache off for this tree:
  <reason>`. `tree::unknown_code` (`crates/sluice-gate-core/src/tree.rs`) maps
  a reason to `skip-worktree`, `unmerged`, `submodules` or `git-error`; other
  Off causes use `disabled`, `snapshot-failed`, `key-failed`,
  `cache-read-failed`, `gate-cache-off`, `queue-only`.
- `sluice run` (`crates/sluice-gate/src/run.rs`) prints the notice once on
  stderr, without `-v`, hook mode included, and stores the code in
  `Trace.reason`.
- `client.log` (`crates/sluice-gate/src/clientlog.rs`): additive
  ` reason=<code>` after `tree=` only when the cache was off.
- MCP `verify`: the pass line already carries `(not cached: <reason>)` and
  `not_cached_reason`; a test now pins it for the skip-worktree case. Status
  JSON and the protocol are unchanged.
- `sluice doctor` (`crates/sluice-gate/src/doctor.rs`, `check_cache`): `warn`
  naming up to 5 flagged paths from `git ls-files -v` (via
  `sluice-gate-core::git`), with the `git update-index --no-skip-worktree` /
  `--no-assume-unchanged` remedy; `ok` otherwise.

## F4: husky v9

`check_hooks` read `.husky/_` (husky's runner stubs, which only source `h`).
`husky_scripts_call_sluice` now recognises a hooks directory with an `h` file
and inspects `<parent>/<hook>` for each stub. A wrapper that mentions sluice
directly is still accepted.

## Tests

`tests/run.rs` (`a_tree_that_cannot_be_hashed_says_so_on_stderr_without_verbose`,
plain and `--hook`), `tests/debugging.rs`
(`client_log_records_the_reason_when_the_cache_is_off`), `tests/mcp.rs`
(`verify_says_when_the_tree_cannot_be_hashed`), `tests/doctor.rs`
(`doctor_warns_when_index_entries_disable_the_cache`,
`doctor_reads_the_scripts_a_husky_v9_runner_executes`), `client.log` unit test.

## Docs

`docs/debugging.md`, `docs/cli.md` (doctor checks), `CHANGELOG.md`.

## Verification

`verify` through Sluice (fmt, clippy, test) on the committed branch.
