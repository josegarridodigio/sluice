---
title: "Change: CI compatibility matrix (H1)"
tags: [sluice, change, ci]
---

# Change: CI compatibility matrix (2026-10-22)

## What changed

- New `.github/workflows/matrix.yml`, triggered on push to `main`, tags `v*`,
  a weekly schedule (Mondays 04:17 UTC) and `workflow_dispatch`; not on pull
  requests. `ci.yml` (fmt, clippy, test on ubuntu-latest) is unchanged.
- Job `distros`: `cargo test --workspace --locked` in `ubuntu:22.04` (git 2.34),
  `ubuntu:24.04` (git 2.43), `debian:12` (git 2.39), `fedora:latest` and
  `alpine:latest` (musl, so the static-link and libc differences show up).
  `fail-fast: false`, 45 min timeout, `Swatinem/rust-cache` keyed by image.
- Job `git-minimum`: builds git 2.31.0 (the README minimum) from source,
  cached with `actions/cache` keyed on the version, puts it first in `PATH`,
  prints and asserts `git --version`, then runs
  `cargo test --locked -p sluice-gate-core -p sluice-gate` (the git wrapper
  and the CLI integration tests).
- Job `aarch64`: native `ubuntu-24.04-arm`, full `cargo test --workspace --locked`.
- Helper scripts: `scripts/ci/container-test.sh` (POSIX sh; installs git, a C
  compiler, tar, procps, coreutils per distro, then rustup with
  `--default-toolchain none` so the toolchain comes from `rust-toolchain.toml`)
  and `scripts/ci/build-git.sh <version> <prefix>`.

## Why

- Checkout and cache run on the runner and the container is started with
  `docker run`, with the workspace, cargo registry and git dirs mounted. JS
  actions inside the `container:` key are unreliable on Alpine and Fedora.
- Tests call `git`, `sh`, `tar`, `mkfifo`, `pgrep`, `sleep`, `kill`, and `curl`
  or `wget` (self-update). All are installed. Python is not used by Rust tests.
- PSI: daemon tests use a temporary `psi_root` with `psi_enabled: false` or a
  fake pressure directory, and the production code reports "unavailable" when
  `/proc/pressure` is missing, so containers without PSI should pass. No test
  or code change was needed. No test checks permissions, so running as root in
  the container is fine.
- The suite runs with `RUSTFLAGS=-D warnings` like `ci.yml`.

## Verified

- YAML parses (PyYAML); `sh -n` on the scripts.
- `docker run` of `scripts/ci/container-test.sh` with
  `CI_CARGO_ARGS="-p sluice-gate-core"` on `ubuntu:24.04` and `alpine:latest`
  (musl rustup): 104 tests passed in each.
- Not provable locally, only on GitHub: Debian 12, Fedora, Ubuntu 22.04, the
  full workspace in containers, the git 2.31.0 source build and cache,
  the aarch64 runner, cache keys, and the triggers.
