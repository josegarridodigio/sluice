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

## Verdict

Target met on a 5 000-file repository even under load. Larger repositories
scale with `git add -A` (about 15 ms per 5 000 files here); options if that
matters later: parse the index natively to drop spawn 2, merge spawns 2 and
4, or an fsmonitor-aware fast path. Tracked in
[[sluice/changes/phase-1-cache-only.md]] follow-ups.
