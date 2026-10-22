# Compatibility golden files: v0.2.0

queue: sluice.db (the same file as the daemon fixture) and, since v0.2.0, queue status JSON.

Recorded from the release binary `sluice 0.2.0`
(archive `sluice-0.2.0-x86_64-unknown-linux-musl.tar.gz`, sha256 `35a00cae0f59f31eaa49b648d4e55218dd6c5ee528ec9bc04e048859757dbd9b`) by
`scripts/compat/generate.sh` (task H3). Never edit these files to make a
test pass; a changed format needs a new version directory (ADR-0013). The
exact commands, normalisations and the tests that read the files are in
`kb/sluice/changes/compat-golden-files.md`. Integrity: `SHA256SUMS`.
