---
title: "Change: user intake (issue forms, CHANGELOG, release notes)"
tags: [sluice, change, release, docs, alpha]
---

# Change: user intake (2026-10-22)

Task F of [[sluice/plan/alpha-plan.md]]. No Rust code changed.

## What changed

- Issue forms: a bug report that asks for the Sluice version, Linux
  distribution and kernel, git version, hook manager (husky, lefthook,
  pre-commit, plain, other), whether a human or an agent invoked it, the
  gate definition (rendered as TOML), what happened and what was expected,
  and a required checkbox confirming `sluice doctor --bundle` was attached
  after reviewing it for secrets. A feature request form. Blank issues are
  disabled; contact links point to `docs/debugging.md` and the README
  limitations.
- `CHANGELOG.md` in Keep a Changelog 1.1 format: empty `[Unreleased]`,
  `[0.2.0]` (merge queue, debugging and doctor, update, release guard,
  agent plugin, review fixes) and `[0.1.0]` (phases 0 to 5), backfilled from
  the change notes and `git log`, with compare links.
- `scripts/changelog-release.py` with two commands: `release X.Y.Z` moves
  the Unreleased entries into `## [X.Y.Z] - <today>`, leaves a fresh
  Unreleased section with empty placeholders, drops empty subsections from
  the released section, and updates the compare links; it exits 1 without
  touching the file when Unreleased has no entries or the version already
  has a section. `extract X.Y.Z` prints one section's body.
- `scripts/release.sh` calls `release` right after its pre-checks and before
  any version edit, and stages `CHANGELOG.md` in the release commit.
- `.github/workflows/release.yml`: the `release` job checks out the tag,
  writes the matching section to `release-notes.md` with `extract`, and
  passes it as `body_path`; `generate_release_notes: true` stays, so the
  generated notes are appended. The tag/version guard step is untouched.
- README: "Known limitations" section and CHANGELOG links; the badge stays
  pre-alpha. CONTRIBUTING: "Changelog" rule and "Releasing" procedure.

## Files

`.github/ISSUE_TEMPLATE/{bug_report,feature_request,config}.yml`,
`CHANGELOG.md`, `scripts/changelog-release.py`, `scripts/release.sh`,
`.github/workflows/release.yml`, `README.md`, `CONTRIBUTING.md`.

## Why

Alpha users need a structured way to report problems with the evidence we
ask for, and a readable history of what each release contains. Putting the
changelog move inside `release.sh` keeps the notes and the tag in one
commit.

## Verification

- All four YAML files parse with PyYAML; every form field has an id and a
  label.
- The helper was run on copies of `CHANGELOG.md`: an empty Unreleased exits
  1 and leaves the file byte-identical; with entries under Added and Fixed
  the entries move to `[0.3.0] - 2026-11-01` (the empty Changed group is
  dropped), a fresh Unreleased remains and links become
  `Unreleased: v0.3.0...HEAD`, `0.3.0: v0.2.0...v0.3.0`; a repeated
  release, an unknown version in `extract` and a second release with an
  empty Unreleased all exit 1. `extract` works on the real 0.1.0 and 0.2.0
  sections. `sh -n scripts/release.sh` passes.
- The full `release.sh` was not run (it commits and tags); the workflow
  change is not exercised until the next tag.
- Repository gates through Sluice: `sluice run fmt`, `clippy`, `test`.

## Open points

- The notes step runs in the last job, so a missing CHANGELOG section fails
  the workflow after the build; `release.sh` makes that case impossible for
  normal releases.
- `release.sh` now needs `python3` (maintainers only).
