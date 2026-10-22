---
title: "ADR-0008: The blocking daemon client lives in sluice-gate-proto"
tags: [sluice, adr, protocol, phase-3]
---

# ADR-0008: The blocking daemon client lives in `sluice-gate-proto`

Status: accepted (2026-10-22), decided while preparing Phase 3.

## Context

Phase 2 put the blocking Unix-socket client (`handshake`, `run` streaming,
`status`, `logs`, `stop`, `hit` event, `ensure_daemon` autostart) in the
CLI crate. Phase 3's MCP server needs the same client. `sluice-gate-core`
cannot host it (the protocol types depend on core, so core cannot depend
on them), and `sluice-gate-daemon` would drag tokio and SQLite into the
MCP binary's dependency on a pure client.

## Decision

- `sluice-gate-proto` gains a `client` module behind a Cargo feature
  `client` (default off): `std`-only (`UnixStream`, `BufReader`), no
  tokio. It owns the handshake, request/stream helpers, the 20 ms hit
  event and `ensure_daemon` (start lock + detached spawn of the current
  executable with `daemon`).
- The CLI and the MCP crate enable the feature; the daemon crate does not.
- The proto crate stays the single place that knows the wire format.

## Consequences

- One client implementation, one set of tests, for both front-ends.
- `ensure_daemon` assumes the current executable is the `sluice` binary
  (true for `sluice run` and `sluice mcp`); a future embedder can pass an
  explicit path.
