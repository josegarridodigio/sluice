# Compatibility golden files: v0.1.0

proto: status JSON, queue status JSON, protocol transcripts.

Recorded from the release binary `sluice 0.1.0`
(archive `sluice-0.1.0-x86_64-unknown-linux-musl.tar.gz`, sha256 `620b1f6d66312e2bec051d406cda55f92cc029b98a4f5ed1565b211b7baaa62e`) by
`scripts/compat/generate.sh` (task H3). Never edit these files to make a
test pass; a changed format needs a new version directory (ADR-0013). The
exact commands, normalisations and the tests that read the files are in
`kb/sluice/changes/compat-golden-files.md`. Integrity: `SHA256SUMS`.
