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

## First runs (run 39870460775, 2026-10-22)

Debian 12, Fedora and aarch64 passed; git 2.31.0, Ubuntu 22.04, Ubuntu
24.04 and Alpine failed. `cargo test` stopped at the first failing test
binary, so each red job hid whatever came after it.

- **git 2.31.0: the source build failed** (`http.h:54:24: error:
  'CURLUSESSL_TRY' undeclared`): git 2.31's HTTP client does not compile
  against the runner's current libcurl headers. Fix: `build-git.sh` builds
  with `NO_CURL=1` (the workflow no longer installs `libcurl4-gnutls-dev` /
  `libexpat1-dev`; cache key `…-nocurl-v2`). `git http-backend`, which the
  test HTTP server uses, needs no curl, but the client transport
  `git-remote-http` is gone, so `servers::http_remote` in
  `crates/sluice-gate/tests/queue_transports/` now returns `None` and
  prints `skipped: this git has no HTTP transport (no git-remote-http,
  built without curl)` when `git --exec-path` has no `git-remote-http`
  (the 5 HTTP tests skip, the SSH ones run). Verified: git 2.31.0 built
  with the script in `ubuntu:24.04` with libcurl headers installed
  (rc 0, `git version 2.31.0`).
- **git 2.31.0: a real product bug behind the build failure.** With that
  git first in `PATH` the SSH transport tests failed: the merge queue ran
  `git worktree add --detach --lock --reason "sluice merge queue"`, and
  `--reason` on `add` exists only since git 2.33 (`error: unknown option
  'reason'`), so every train failed with `internal` on git 2.31 and 2.32,
  the declared minimum. Fix (`crates/sluice-gate-queue/src/runner.rs`,
  `merge_members`): `worktree add --detach`, then `worktree lock --reason
  "sluice merge queue"` (since 2.10). Design note updated
  ([[sluice/design/merge-queue.md]], step 3). The job now also tests
  `-p sluice-gate-queue`.
- **Ubuntu 22.04 and 24.04: `daemon.rs`
  `exec_runs_an_argv_into_a_spool_with_sluice_job_set`** (`left:
  "<job>out"`, `right: "out<job>"`): the test concatenated the stdout and
  stderr records of the spool and assumed stdout arrives first; the two
  pipes are pumped independently, so either order is valid. Test bug,
  fixed on `main` by the lead (each stream checked exactly, either
  cross-stream order accepted). Debian and Fedora passed by luck of
  scheduling; it is not image-specific.
- **Alpine: `guard.rs` `plugin_test_guard_script_passes`**, every check
  `rc=127`: `plugin/hooks/test-guard.sh` built a `PATH` without `sluice`
  with `printf '%s' "$PATH" | tr ':' '\n' | while read`, and `read` drops a
  last line without a newline. The container `PATH` ends in `/bin`, and on
  Alpine (busybox, no merged `/usr`) `/bin/sh` is the only `sh`, so every
  `env PATH=… sh guard.sh` was "not found". Merged-`/usr` images (Ubuntu,
  Debian, Fedora) have `/usr/bin/sh` earlier in `PATH`, so they passed by
  luck. Test-script bug; `guard.sh` itself already reads with
  `|| [ -n "$line" ]`. Fix: `printf '%s\n'`. Running as root, dash vs
  busybox, `jq` (absent everywhere: the jq pass is skipped by design),
  `script` and `procps` were ruled out.
- `container-test.sh` now runs `cargo test --locked --no-fail-fast` so one
  failing test binary no longer hides the others.

Verified locally with `docker run` of `scripts/ci/container-test.sh`
(worktree mounted at `/work`, `CARGO_TARGET_DIR` per image,
`CI_CARGO_ARGS="-p sluice-gate --test guard --test exec --test chaos"`):
`alpine:latest` (git 2.54.0) and `ubuntu:24.04` (git 2.43.0) pass guard
(7), exec (16) and chaos (8). Before the fix `test-guard.sh` alone in
`alpine:latest`: `42 checks, 43 failure(s)`; after: `0 failure(s)`. git
2.31.0 first in `PATH` (the git-minimum job, locally): `cargo test --locked
--no-fail-fast -p sluice-gate-core -p sluice-gate -p sluice-gate-queue`, 463
passed, 0 failed; before the runner fix 4 SSH transport tests failed
(`error: unknown option 'reason'`), the 5 HTTP ones print the skip line.
