---
title: "ADR-0001: Rust edition, MSRV and workspace layout"
tags: [sluice, adr]
---

# ADR-0001: Rust edition, MSRV and workspace layout

Status: accepted (2026-10-22, Phase-0 answer Q7). Toolchain installed: rustc 1.99.1, edition 2024, `rust-version = "1.99"`.

## Context

Sluice is a single binary with several modes (CLI client, MCP server,
daemon). Parts of it must stay synchronous and dependency-light (the cache
hit path), parts are async (daemon, MCP). We want clear boundaries so the
hit path never pulls tokio or SQLite.

## Decision

- Rust **edition 2024**, MSRV = latest stable at project start (recorded in
  `rust-version` once the toolchain is installed). `rust-toolchain.toml` pins
  `stable`.
- Cargo workspace with five crates:

| Crate dir | Role | Async? |
|---|---|---|
| `crates/sluice-gate-core` | config, cache key, markers, git tree computation, agent detection | no |
| `crates/sluice-gate-proto` | IPC types, versioned, serde only | no |
| `crates/sluice-gate-daemon` | scheduler, coalescing, executor, PSI, SQLite | tokio |
| `crates/sluice-gate-mcp` | rmcp server, thin client | tokio |
| `crates/sluice-gate` | the `sluice` binary; wires everything | tokio only for daemon/mcp subcommands |

- Workspace-level `[workspace.dependencies]`, `[workspace.lints]` with
  `clippy::all` + `pedantic` warnings reviewed case by case; CI runs
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --workspace`.
- Integration tests live in `crates/sluice-gate/tests/` so they can spawn
  the real binary with `assert_cmd` (Cargo builds the bin for them).
- Target platform is Linux only (Phase-0 answer Q4); no cfg gymnastics for
  macOS.

## Consequences

- The `sluice run` hit path links only `sluice-gate-core` code paths; the binary
  still contains tokio, but it is not initialised on the hit path.
- Directory names equal the published crate names (see
  [[sluice/design/adr/0003-crate-naming.md]]); the binary is `sluice`.

## Alternatives considered

- Single crate with modules: simpler, but makes it easy to leak async into
  the hit path and harder to test boundaries.
- Edition 2021: no benefit; 2024 is stable and the project is new.
