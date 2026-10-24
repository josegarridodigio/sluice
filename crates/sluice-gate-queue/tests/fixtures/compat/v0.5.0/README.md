# Compatibility golden files: v0.5.0

queue: sluice.db (the same file as the daemon fixture) and, since v0.2.0, queue status JSON.

Recorded from the release binary `sluice 0.5.0`
(archive `sluice-0.5.0-x86_64-unknown-linux-musl.tar.gz`, sha256 `ce440caba2912a840552d6085a38432bd8963a0d30f2d39448606409e3624043`) by
`scripts/compat/generate.sh` (task H3). Never edit these files to make a
test pass; a changed format needs a new version directory (ADR-0013). The
exact commands, normalisations and the tests that read the files are in
`kb/sluice/changes/compat-golden-files.md`. Integrity: `SHA256SUMS`.
