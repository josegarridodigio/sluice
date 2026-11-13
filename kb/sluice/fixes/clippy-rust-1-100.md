---
title: "Fix: the workspace passes the clippy of Rust 1.100"
tags: [sluice, fix, ci, clippy, toolchain]
---

# Fix: the workspace passes the clippy of Rust 1.100

## What happened

`rust-toolchain.toml` and CI (`dtolnay/rust-toolchain@stable`) follow the stable
channel. Stable became 1.100.0 on 2026-11-09, and its clippy fails the workspace under
`-D warnings`. The first run to hit it was the CI of the docs pull request
`docs/post-0.12.1-accuracy` on 2026-11-13. Local gates had passed because the
workstation's stable was still 1.99.1.

## Cause

New or extended lints in clippy 1.100:

- `map_unwrap_or` now flags `.map(f).unwrap_or_default()` on `Option` and `Result`
  (38 places in 26 files).
- `needless_range_loop`: `for k in 0..5 { t.waits[k] = ... }` in
  `sluice-gate-daemon/src/store.rs`.

## Fix

- Every `.map(f).unwrap_or_default()` became `.map_or_default(f)`, then `cargo fmt`.
- The store loop iterates `t.waits.iter_mut().enumerate()`.
- No behaviour change. The minimum Rust version stays 1.99: `cargo +1.99.1 check
  --workspace --all-targets` passes.

## Verification

- With the local stable updated to 1.100.0: `fmt`, `clippy` and `test` through Sluice
  `verify`.
- `cargo +1.99.1 check --workspace --all-targets` for the MSRV.

Cargo 1.100 also warns `redundant_homepage` on the crates' manifests. It is a warning,
not a `-D warnings` error, and is left for a separate change.
