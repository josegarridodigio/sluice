---
title: "Fix: hook runner CI failures on the distro matrix (2026-10-28)"
tags: [sluice, fix, ci, hooks, tests]
---

# Fix: hook runner CI failures on the distro matrix

Branch `feat/hook-runner`, before release 0.7.0. Matrix runs 40701216902 and 40706837310
failed; 40709087495 passed on all seven jobs, with lefthook 2.2.2 installed on all of them and
pre-commit 4.7.0 on six (see below), so the real-tool tests ran for the first time.

| Failure | Root cause | Fix |
|---|---|---|
| Fake-manager tests got the real tools' results | the tests put their fakes in front of the host `PATH`; CI now installs lefthook and pre-commit | `tests/common/mod.rs::tools_dir`: each test builds a `PATH` of the fakes plus basic tools only (`hook_managers.rs`, `hook_gates.rs`, `hooks.rs`) |
| `env: 'bash': No such file or directory` on Alpine | fake pre-commit stubs used `#!/usr/bin/env bash` | fakes use `/bin/sh`; `real_pre_commit_local_hooks_run_as_units` skips without bash (the real stub needs it) |
| HTTP transport tests on Alpine (`base_unavailable`, "Authentication failed") | test-only: adding `python3` to Alpine made these tests run there for the first time; Alpine ships `git http-backend` in `git-daemon`, so the test server answered an empty ref list (the authentication line came from a test that tries a wrong password on purpose). Real users are unaffected: the client transport is in `git` | `queue_transports/servers.rs`: skip without `git http-backend`; `scripts/ci/container-test.sh` installs `git-daemon` on Alpine |
| `a_missing_sluice_binary_still_runs_the_original_hook` on Fedora | test assumption: Fedora's shell reports a hook's `$0` as an absolute path; reproduced in a `fedora:latest` container with the built binary, where the product ran the right hook | `tests/hooks.rs::ran` matches the recorded path in either form. While investigating, the fallback script was also hardened to skip Sluice's own directory among `core.hooksPath` values (`script.rs::render`, regression test in the same test) |
| `pre-commit==4.7.0` not found on ubuntu:22.04 | pre-commit 4.7.0 requires Python >= 3.11; ubuntu:22.04 has 3.10 | `scripts/ci/install-hook-managers.sh` skips pre-commit there (the real pre-commit tests skip; six jobs run them) |
| lefthook binary downloaded unverified | supply chain | SHA-256 of the release binaries pinned in the install script, checked against a download |

Verified: `verify` (fmt, clippy, test) through Sluice on each fix, and Matrix 40709087495 green.
