---
title: "Change: Phase 3, MCP server for agents"
tags: [sluice, change, phase-3]
---

# Change: Phase 3, MCP server for agents (2026-10-22)

Implements [[sluice/plan/master-plan.md]] Phase 3 from
[[sluice/design/mcp-tools.md]] and
[[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]].

## What changed

| Where | What |
|---|---|
| `sluice-gate-core::lookup` (new) | the hit check shared by the CLI and the MCP server: `resolve_repo` + `RepoContext::select`, `resolve`, `lookup` (`Hit` / `Miss` / `Off { reason, notice }`), `derive_key`, `child_env`, `committed_tree`, `run_material` → `RunMaterial`. `run.rs` and `cache.rs` now use it; every message and exit code is unchanged. |
| `sluice-gate-mcp` | rmcp 3.6 stdio server (`serve(env)`): tools `verify`, `status`, `gate_list`, `logs` with text + `structured_content` (output schemas for `verify`, `gate_list`, `logs`), server `instructions`, progress notifications every `[mcp] progress_interval`, cancellation through the request token (sends `cancel` to the daemon), stderr-only tracing (`SLUICE_LOG`). Misses are spoken over an async socket client in `daemon.rs`; `ensure_daemon`, `status`, `logs` and the hit event use the blocking proto client on `spawn_blocking`. |
| `sluice-gate` | `sluice mcp` runs the server on a tokio runtime created only for that subcommand; the fail-fast message now says `call the 'verify' MCP tool (or run 'sluice run <gate>') first`; `tests/mcp.rs` drives the built binary with the rmcp child-process client. |
| core `config.rs` | `[mcp] progress_interval` (default 15 s). |
| `docs/agents.md`, `docs/manual-test-phase-3.md` | agent guide with the `CLAUDE.md` block, permissions caveat and detection notes; the manual two-agent test script. |

## Verification

```
cargo fmt --all --check; cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # 207 tests, including tests/mcp.rs:
  gate_list; verify miss then hit (counter stays 1); the fail gate (exit 7, tail "boom");
  status (schema 1 + cache state), logs, bad requests -> MCP errors;
  progress notifications at a 1 s interval; a dropped verify of `sleep 30` leaves no process within 7 s
```

Manual acceptance (two Claude Code agents, auto-backgrounding, lowered idle
timeout) is a user task: `docs/manual-test-phase-3.md`, evidence to be
recorded in [[sluice/changes/phase-3-acceptance.md]].

## Verified against rmcp 3.6.0 source

`#[tool_handler(router = self.tool_router)]` is needed because the macro
defaults to `Self::tool_router()`; `#[tool(output_schema = …)]` works with
a hand-built `CallToolResult`; dropping a client `call_tool` future sends
`notifications/cancelled`. Whether Claude Code sends a `progressToken` is
still unverified (manual test step 7).
