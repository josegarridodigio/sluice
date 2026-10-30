---
title: "README benchmark chart: agents in parallel, without and with Sluice"
tags: [sluice, changes, benchmark, ci, docs]
---

# README benchmark chart: agents in parallel

User decision (2026-10-30): a benchmark chart in the README, measured in CI, headline
metric the total wall time.

## What changed

- `scripts/bench-agents.sh`: N agents (default 1 2 4 8), each in its own worktree with
  its own warmed `target/`, make two commits in parallel through the repository's
  pre-commit gates (`fmt`, `clippy`); the first commit is shared, the second is the
  agent's own. Scenario A runs the gate commands directly, scenario B through
  `sluice run --hook`; the order alternates per agent count; isolated `HOME`/`XDG_*`.
  Appends `agents,scenario,wall_ms,gate_invocations,gate_runs,cpus,mem_gib` to a CSV.
- `scripts/bench-chart.py`: grouped columns (with Sluice slot 1 blue, without slot 2
  orange, validated against GitHub's light and dark surfaces), medians per agent count,
  seconds below five minutes, light and dark SVGs.
- `.github/workflows/bench.yml`: on demand, three rounds by default, uploads the CSV
  and the SVGs; a human reviews them before they are committed.
- `README.md`: the chart (`<picture>`, light/dark) under "Why", plus the 0.8.0/0.9.0
  features (memory expectations kept across arguments, observed reserve, backfilling,
  stalled jobs, pull-request landing). `docs/validation.md`: "Agents in parallel (CI)"
  with the method and the table. `docs/assets/bench-agents-{light,dark}.svg` and
  `bench-agents.csv`.

## Result (run 2026-10-30, 3 rounds, 4 CPUs / 16 GiB)

Medians without / with Sluice: 1 agent 27 / 28 s (+1 %), 2 agents 49 / 40 s (-18 %),
4 agents 103 / 72 s (-30 %), 8 agents 213 / 119 s (-44 %). Rounds varied by at most
6 s. A first single-round run gave the same trend (-6 % to -41 %).

## Verification

The scripts are not Rust (path-scoped gates stay cached); `sh -n`, `py_compile` and a
YAML parse locally; the chart rendered in a browser in both themes; the workflow ran
green twice on GitHub.
