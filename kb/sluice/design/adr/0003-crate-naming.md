---
title: "ADR-0003: Brand, binary and crate names"
tags: [sluice, adr, naming]
---

# ADR-0003: Brand, binary and crate names

Status: accepted (2026-10-22, Phase-0 answer Q2). The user chose the hyphenated form `sluice-gate-*` over `sluicegate-*`.

## Context

The brand and binary name are `sluice`. On crates.io, `sluice` is an async
byte-pipe library, and `sluice-rs` / `sluice-cli` (plus a Homebrew `sluice`
binary) belong to an unrelated Maven index parser. Verified on 2026-10-22
through the crates.io API: those three names return HTTP 200 (taken).

## Decision

Published crate names use the `sluice-gate` prefix. All five checked free
on 2026-10-22 (HTTP 404 on crates.io):

| Directory | Published crate | Purpose |
|---|---|---|
| `crates/sluice-gate` | `sluice-gate` | installable CLI, provides the `sluice` binary (`[[bin]] name = "sluice"`) |
| `crates/sluice-gate-core` | `sluice-gate-core` | config, cache key, markers, git |
| `crates/sluice-gate-proto` | `sluice-gate-proto` | IPC types |
| `crates/sluice-gate-daemon` | `sluice-gate-daemon` | daemon |
| `crates/sluice-gate-mcp` | `sluice-gate-mcp` | MCP server |

`cargo install sluice-gate` installs `sluice`. The README notes the Homebrew
binary-name collision (only relevant if a user installs both tools).

Alternatives also free: `sluicegate-*` (proposed first), `sluicectl`. The
user preferred the hyphenated form.

## Consequences

- Directory names equal the published names. Internal `use` paths follow
  the package name (`sluice_gate_core::…`). The binary stays `sluice`.
- Re-verify availability with `cargo search` right before the first
  publish; names are only reserved by publishing.
