# Compatibility golden files: v0.3.0

daemon: sluice.db after the recorded scenarios.

Recorded from the release binary `sluice 0.3.0`
(archive `sluice-0.3.0-x86_64-unknown-linux-musl.tar.gz`, sha256 `043027a3963487be2cc6d8f585e3f9ec8ba7c87f1c00e44732e8d87c8980167e`) by
`scripts/compat/generate.sh` (task H3). Never edit these files to make a
test pass; a changed format needs a new version directory (ADR-0013). The
exact commands, normalisations and the tests that read the files are in
`kb/sluice/changes/compat-golden-files.md`. Integrity: `SHA256SUMS`.
