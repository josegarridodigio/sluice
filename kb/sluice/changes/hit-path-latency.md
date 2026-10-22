---
title: "Measurement: cache-hit latency of sluice run"
tags: [sluice, measurement, phase-1]
---

# Measurement: cache-hit latency of `sluice run`

Target from [[sluice/research/problem-statement.md]]: under 30 ms end to
end. Script: `scripts/bench-hit-path.sh [files] [iterations]` (release
build, synthetic repository, Python measures the wall time of the `sluice`
process alone).

## Results (2026-10-22)

Machine: 12th Gen Intel Core i7-1280P, 20 logical CPUs, Linux 7.0, git
2.53.0, ext4. **The machine was heavily loaded during the measurement**
(load average 21 on 20 CPUs: an Ollama embedding server and an unrelated
`ng test --watch`), so these numbers are pessimistic.

| Files | Iterations | `sluice run` hit (min / median / p90 / max) | `git status --porcelain` (median) | `git --version` (median) |
|---|---|---|---|---|
| 5 000 | 40 | 24.6 / **29.1** / 31.8 / 33.7 ms | 8.2 ms | 1.5 ms |
| 20 000 | 10 | 75 / 79 / – / 92 ms (shell timing, before the lazy-env change) | – | – |

Before making the gate environment lazy (one `git rev-parse
--local-env-vars` spawn less) the 5 000-file median was 35–39 ms.

## Where the time goes

A hit spawns git five times (verified with `strace -e execve`):

1. `rev-parse --path-format=absolute --show-toplevel --git-dir --git-common-dir --git-path index --git-path objects`
2. `ls-files -v -z` (pre-check: unmerged, assume-unchanged, skip-worktree)
3. `add -A` on the private index (stats every file; dominates on large repos)
4. `ls-files -s -z` on the private index (gitlink check)
5. `write-tree`

Process spawn on this machine costs about 1.1 ms (`true`), a trivial git
command 1.5 ms. The index copy, key derivation and marker read are
negligible (microseconds). Each spawn also probes nine `PATH` entries
before finding `/usr/bin/git`; resolving the binary once would shave a
fraction of a millisecond.

## After Phase 2 (daemon client wired in)

Measured by the CLI client agent with the same script, load average about
10: median 24.7 ms with the daemon client linked in versus 20.5 ms for the
saved Phase 1 binary; interleaving both binaries over 60 runs gave 25.7 vs
24.6 ms with a daemon running and 24.4 vs 23.9 ms without, so the real
cost of the hit event (one 20 ms-bounded socket round trip) is about 1 ms.
No tokio runtime is created on a hit. The binary grew from 1.6 MB to
5.6 MB.

## Path-scoped keys (D1, 2026-10-22)

Design: [[sluice/design/path-scoped-keys.md]] section 10. The script now
defines two gates on the same synthetic repository, `noop` (unscoped) and
`scoped` (`paths = ["src/1*/**", ".sluice.toml"]`, `paths_ignore =
["src/12/**"]`), and times them **interleaved** (ABAB, 40 iterations each)
so machine load hits every series equally. With `SLUICE_BASELINE=<older
binary>` it also interleaves that binary and the new one on the unscoped
gate in a linked worktree without the scoped gate (0.2 rejects `paths`).
Then it edits `src/3/f300.txt` (outside the scope) and times 40 scoped hits
again. Baseline: the installed 0.2.0 binary. Same machine as above; load
average 3.0–3.6 on 20 CPUs during the runs (below the 4 threshold), git
2.53.0. Two runs at 5 000 files, one informational run at 20 000:

| Series (5 000 files) | Run 1 min / median / p90 / max | Run 2 min / median / p90 / max |
|---|---|---|
| unscoped (`noop`) | 19.4 / **22.2** / 33.4 / 45.7 ms | 22.1 / **25.4** / 34.6 / 38.9 ms |
| scoped | 20.2 / **23.0** / 33.0 / 44.4 ms | 22.4 / **26.4** / 33.9 / 40.4 ms |
| 0.2.0 baseline, unscoped (baseline worktree) | 19.8 / 23.9 / 34.3 / 46.8 ms | 22.0 / 25.0 / 31.0 / 37.4 ms |
| new binary, unscoped (baseline worktree) | 20.8 / 25.0 / 37.5 / 56.6 ms | 20.5 / 25.0 / 35.1 / 38.0 ms |
| scoped after an out-of-scope edit | 23.8 / **26.2** / 32.0 / 33.3 ms | 21.6 / **28.3** / 32.1 / 34.9 ms |
| `git status --porcelain` | median 5.9 ms | median 5.3 ms |

| Series (20 000 files, informational) | min / median / p90 / max |
|---|---|
| unscoped | 48.8 / 60.5 / 78.2 / 82.8 ms |
| scoped | 52.6 / 61.9 / 75.1 / 82.3 ms |
| 0.2.0 baseline, unscoped | 46.6 / 55.8 / 72.8 / 79.1 ms |
| new binary, unscoped (baseline worktree) | 45.1 / 57.8 / 69.6 / 84.2 ms |
| scoped after an out-of-scope edit | 50.5 / 60.7 / 67.1 / 69.2 ms |

Acceptance (design section 10):

- Scoped median under 35 ms at 5 000 files: **met** (23.0 and 26.4 ms; 26.2
  and 28.3 ms after an out-of-scope edit, where `add -A` also hashes the
  edited file into the private object directory).
- Scoped minus unscoped median at most 2 ms: **met** (+0.8 and +1.0 ms; the
  in-process filter and SHA-256 over about 330 KB of `ls-files -s` output,
  no extra git spawn).
- Unscoped median within noise of the baseline binary: **met** in the
  like-for-like worktree (+1.1 and 0.0 ms at 5 000 files; +2.0 ms at
  20 000, where the spread between runs is larger than that). The unscoped
  path does no extra work: the `ls-files -s` output it now keeps was
  already read for the gitlink check.

Note: a freshly created linked worktree has a racily clean index (files
written in the same second as the index), so every snapshot rehashes all
files (about 77 ms here for both binaries); the script refreshes that
index before timing.

## Verdict

Target met on a 5 000-file repository even under load. Larger repositories
scale with `git add -A` (about 15 ms per 5 000 files here); options if that
matters later: parse the index natively to drop spawn 2, merge spawns 2 and
4, or an fsmonitor-aware fast path. Tracked in
[[sluice/changes/phase-1-cache-only.md]] follow-ups.
