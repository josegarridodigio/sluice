---
title: "Fix: the trains' PATH keeps the daemon's directories after the login shell's"
tags: [sluice, fix, merge-queue, environment, release]
---

# Fix: the trains' PATH keeps the daemon's directories after the login shell's

## Symptom

The v0.6.1 tag's workflows (CI, Matrix, Release) failed: the end-to-end test
`the_pre_push_hook_of_the_trains_push_is_answered_from_the_train` ended
`key_input_failed` (`tool --version`: exit 127). No 0.6.1 binaries were published.
The same test passed on the developer machine.

## Cause

ADR-0020 (0.6.1) overlays the user's login environment on the daemon's for train tools,
login values winning. On the CI runner the login shell is `bash -l`, whose `/etc/profile`
rewrites `PATH` from scratch, so the directory the test put in the daemon's `PATH` (a fake
`tool`) vanished. On the developer machine the login shell (zsh) keeps the inherited
`PATH`, which hid the problem. The same would happen to a user who starts the daemon from
a shell with directories their rc files do not set (an activated virtualenv, direnv).

## Change

`toolenv::merged_path`: the trains' `PATH` is the login `PATH` followed by the daemon's
entries it lacks (duplicates and empty entries dropped). Login tools still win; nothing
the daemon had is lost. Unit tests `the_trains_path_is_login_first_then_what_only_the_daemon_had`
and the updated `login_values_win_over_the_daemon_env`. ADR-0020 §2, `docs/merge-queue.md`
and the 0.6.1 change note amended. Released as 0.6.2; the v0.6.1 tag stays (published
tags are never moved) and the changelog says it was not released.

## Verification

fmt, clippy and the whole suite through Sluice; the CI workflows of the 0.6.2 tag.
