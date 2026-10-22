---
title: "Change: repository presentation on GitHub"
tags: [sluice, change, docs]
---

# Change: repository presentation on GitHub (2026-10-22)

User request: make the repository attractive on GitHub (description, topics,
README) ahead of any public visibility.

## What changed

- GitHub metadata set with `gh repo edit`: description ("Verify each git
  tree once. A stack-agnostic verification queue and cache for parallel
  coding agents…"), homepage pointing at the README, 19 topics (`rust`,
  `git`, `git-hooks`, `cache`, `mcp`, `mcp-server`,
  `model-context-protocol`, `claude-code`, `coding-agents`, `ai-agents`,
  `developer-tools`, `devtools`, `cli`, `test-automation`, `pre-commit`,
  `husky`, `lefthook`, `concurrency`, `git-worktree`), issues and
  discussions enabled, wiki disabled.
- `README.md` rewritten: centred title and tagline, badges (CI, MIT, Rust
  2024, Linux, pre-alpha), the problem as a three-cause table with the
  real-world numbers from [[sluice/research/problem-statement.md]], a
  feature list, a process diagram and a five-step "how it works", the
  `.sluice.toml` example from [[sluice/design/config-format.md]], the
  command surface, the roadmap table, install and requirement notes, the
  caveats section, prior art, contributing and license.
- `CONTRIBUTING.md` added: quality bar, house rules, knowledge-base habit,
  bug report checklist.

## Why

A repository that explains the problem in the first screen, shows the
numbers, and is honest about its pre-alpha status gets stars from the
right people. Topics make it discoverable from the `mcp`, `claude-code`
and `git-hooks` topic pages.

## Not done (needs the GitHub UI)

- Social preview image (Settings → General → Social preview). A simple
  1280×640 card with the tagline would help link previews.
- Pinning the repository on the profile.

## Verification

`gh repo view --json description,repositoryTopics,hasIssuesEnabled,hasDiscussionsEnabled`
returned the expected values. README rendered locally and checked for
broken relative links (`LICENSE`, `CONTRIBUTING.md`,
`THIRD_PARTY_NOTICES.md`, `kb/sluice/plan/master-plan.md`,
`kb/sluice/design/architecture.md` all exist).
