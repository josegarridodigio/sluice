---
title: "Change: Phase 0 workspace scaffold"
tags: [sluice, change, phase-0]
---

# Change: Phase 0 workspace scaffold (2026-10-22)

Part of [[sluice/plan/master-plan.md]] Phase 0. Decisions applied:
[[sluice/design/adr/0001-rust-edition-and-workspace.md]],
[[sluice/design/adr/0003-crate-naming.md]].

## What changed

- Rust toolchain installed with rustup (rustc 1.99.1, cargo 1.99.1), stable
  channel pinned in `rust-toolchain.toml`; `~/.zshrc` sources
  `~/.cargo/env`.
- Cargo workspace (`Cargo.toml`, resolver 3, edition 2024,
  `rust-version = "1.99"`, license MIT) with five crates under `crates/`:
  `sluice-gate-core`, `sluice-gate-proto`, `sluice-gate-daemon`,
  `sluice-gate-mcp` (placeholder libraries with module docs and the
  `CACHE_FORMAT_VERSION` / `PROTOCOL_VERSION` constants) and `sluice-gate`
  (binary `sluice`).
- Workspace lints: `unsafe_code = "forbid"`, clippy `all` + `pedantic` as
  warnings (CI turns warnings into errors), two pedantic lints relaxed.
- `crates/sluice-gate/src/main.rs`: clap command surface agreed in the
  architecture (`run`, `mcp`, `daemon`, `status [--json]`,
  `cache status|clear [--gate]`, `init`, `hook print <manager>`,
  `logs [job-id]`). Every command prints "not implemented yet" to stderr
  and exits with code 70.
- `crates/sluice-gate/tests/cli.rs`: four smoke tests with `assert_cmd`
  (help lists all commands, version, exit code 70, invalid hook manager).
- `LICENSE` (MIT, copyright Jose Antonio Garrido), `README.md` skeleton with the
  crates.io and Homebrew naming notes, `THIRD_PARTY_NOTICES.md`,
  `CLAUDE.md` with repository conventions, `.github/workflows/ci.yml`
  (fmt, clippy `-D warnings`, test on ubuntu-latest), `.gitignore`
  (`/target/`, `/refs/`, `/.pando/`).

## Why

Exit criteria of Phase 0: a compiling, linted, tested workspace with the
public command surface fixed, so Phase 1 can fill `sluice run` and the
cache without renaming anything.

## Verification

```
cargo fmt --all --check                                   # ok
cargo clippy --workspace --all-targets -- -D warnings     # no warnings
cargo test --workspace                                    # 5 passed (4 CLI smoke + 1 unit)
```

## Follow-ups

- `repository` / `homepage` in `Cargo.toml` point to
  `https://github.com/josegarridodigio/sluice` (remote created by the user on 2026-10-22).
- External dependency versions are re-checked when each crate first uses
  them (Phase 1: `serde`, `toml`, `sha2`, `humantime`, `directories`,
  `tempfile`).
