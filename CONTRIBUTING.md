# Contributing to Sluice

Thanks for looking. Sluice is early, so the most useful contributions right
now are design feedback, real-world failure stories and small, well-tested
increments.

## Before you start

- Read the [master plan](kb/sluice/plan/master-plan.md) and the
  [architecture](kb/sluice/design/architecture.md). Decisions are recorded
  as ADRs under `kb/sluice/design/adr/`; if you want to change one, open a
  discussion first.
- Check the open issues. If your idea is not there, open an issue before
  writing code so we can agree on the shape.

## Quality bar

Every change must keep these green:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Clippy runs with `pedantic` enabled. If a lint is wrong for a specific
case, allow it locally with a one-line reason; do not disable it globally.

## Sluice verifies itself

The repository ships its own gates (`.sluice.toml`: `fmt`, `clippy`,
`test`) and plain hooks under `.githooks/`. To use them:

```sh
cargo install --path crates/sluice-gate     # puts `sluice` in ~/.cargo/bin
git config core.hooksPath .githooks         # pre-commit: fmt + clippy; pre-push: test
```

A tree that already passed is answered from the cache, so repeated
commits and pushes of the same content cost milliseconds. Without
`sluice` in `PATH` the hooks run the gates directly. Agents working on
this repository register the MCP server as described in
[docs/agents.md](docs/agents.md). `docs/validation.md` explains the
multi-worktree benchmark that produces the before/after numbers.

## Rules of the house

- Everything in English: code, comments, commits, docs.
- Product code never modifies the user's index or working tree, never
  stages files, and only talks to git through the `git` binary via the
  wrapper in `sluice-gate-core`.
- Every bug fix comes with a regression test. Integration tests create
  throwaway repositories and worktrees with `tempfile`.
- Linux is the only supported platform in v1; do not add `cfg` branches for
  other platforms.
- Commit messages follow Conventional Commits (`feat:`, `fix:`, `chore:`,
  `docs:`). Keep the subject under 72 characters and explain the why in the
  body.

## Knowledge base

`kb/` is a Pando knowledge base versioned with the code. Each change gets a
short note under `kb/sluice/changes/` (what changed, files and symbols
touched, why, how it was verified). It is a small habit that keeps the
design history honest, and reviewers read it first.

## Reporting a bug

Include: the `sluice` version, the hook manager, whether the hook was
invoked by a human or an agent, the gate definition, and the output of
`sluice status --json` and `sluice cache status` where available.
