---
title: "Change: release workflow and installer"
tags: [sluice, change, phase-5]
---

# Change: release workflow and installer (2026-10-22)

Phase 5 packaging. Adds a tag-triggered release workflow and a POSIX
installer. Nothing here publishes to crates.io, and nothing was tagged,
released or pushed.

## Release workflow (`.github/workflows/release.yml`)

Triggered by a pushed tag `v*`. Three jobs:

1. `test`: `cargo test --workspace` on x86_64 (ubuntu-latest). Gates the rest.
2. `build` (matrix: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`):
   `cross build --release --locked -p sluice-gate --target <target>`, then
   packages `sluice-<version>-<target>.tar.gz` (version = tag without the
   `v`) holding a directory of the same name with `sluice`, `README.md` and
   `LICENSE`.
3. `release` (`permissions: contents: write`): downloads both archives,
   writes `SHA256SUMS`, and attaches archives plus checksums to a GitHub
   release for the tag with `softprops/action-gh-release@v2` (generated
   notes).

Choice: plain `cross` (installed with `taiki-e/install-action@cross`) rather
than `taiki-e/upload-rust-binary-action`. `rusqlite` with `bundled` compiles
C, and cross's images ship the musl C toolchain for both targets. Doing the
packaging ourselves keeps the single combined `SHA256SUMS` and the exact
archive layout under our control. Actions are pinned to major tags.

## Installer (`scripts/install.sh`)

POSIX sh, `set -eu`. Steps: check Linux; map `uname -m` (x86_64/amd64,
aarch64/arm64) to the musl target; pick the base URL
(`releases/latest/download`, or `releases/download/$SLUICE_VERSION`);
download `SHA256SUMS`; resolve the versioned asset name from it (the
`latest/download` scheme needs an exact file name, and ours embeds the
version); download the archive; verify with `sha256sum -c` when available
(warns and continues otherwise); extract in a temp dir (removed by `trap`);
install atomically (copy to `sluice.new`, `mv`) into `$SLUICE_INSTALL_DIR`
or `~/.local/bin`; print `sluice --version` and a PATH hint if needed.
Uses `curl`, falling back to `wget`.

## Verified locally

- `sh -n scripts/install.sh`: OK. `shellcheck`: not installed here, not run.
- Installer against a fake local release (copy of the script with `base`
  pointed at a `file://` directory; real binary, tarball and `SHA256SUMS`),
  run with `dash`: installed, checksum OK, printed `sluice 0.1.0` and the
  PATH hint. With a corrupted `SHA256SUMS`: `checksum mismatch`, exit 1.
- `cargo install --path crates/sluice-gate --root "$(mktemp -d)"`: 16.9 s
  (warm dependency cache), binary 5,655,376 bytes (about 5.4 MiB), and
  `sluice --version` printed `sluice 0.1.0`.
- `rustup target add x86_64-unknown-linux-musl`: worked, but
  `cargo build --release -p sluice-gate --target x86_64-unknown-linux-musl`
  failed: no `x86_64-linux-musl-gcc` / `musl-gcc` on this machine (needed by
  the bundled SQLite). Not built locally; CI via `cross` will do it. The
  aarch64 target was not attempted for the same reason.

## Remaining for the first tag

- Push to GitHub and tag (`git tag v0.1.0 && git push --tags`); the
  workflow is untested on GitHub. Watch the first run: `cross` builds,
  artifact names, and the release step.
- The tag should match the crate version (the workflow does not check).
- `rust-version = "1.99"` and `rust-toolchain.toml` use stable; cross images
  use their own host toolchain only for the C side, rustc comes from stable.
- README Install section still only lists `cargo install --path`; add the
  `curl | sh` line once a release exists.
- Not published to crates.io (workspace crates are not published; a crates.io
  release would be a separate step).
