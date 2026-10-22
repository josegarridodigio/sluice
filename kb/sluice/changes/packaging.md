---
title: "Change: release workflow and installer"
tags: [sluice, change, phase-5]
---

# Change: release workflow and installer (2026-10-22)

Phase 5 packaging. Adds a tag-triggered release workflow and a POSIX
installer. Nothing here publishes to crates.io. The first tag, `v0.1.0`,
was pushed on 2026-10-22 with the user's approval (see "First release"
below).

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

## First release: `v0.1.0` (2026-10-22)

Tag `v0.1.0` (annotated, matches the workspace version) pushed to
`origin`; workflow run 39826813822 went green on the first attempt:
`test`, both `build` jobs (`cross` with the musl images; only a Node 20
deprecation warning from `download-artifact`/`action-gh-release`), and
`release`. Published assets at
https://github.com/josegarridodigio/sluice/releases/tag/v0.1.0:

| Asset | Size |
|---|---|
| `sluice-0.1.0-x86_64-unknown-linux-musl.tar.gz` | 3.5 MB |
| `sluice-0.1.0-aarch64-unknown-linux-musl.tar.gz` | 3.3 MB |
| `SHA256SUMS` | 225 B |

Verified from this machine: `scripts/install.sh` against the real release
(latest and `SLUICE_VERSION=v0.1.0`, into a scratch `SLUICE_INSTALL_DIR`)
prints `Checksum OK` and `Installed: sluice 0.1.0`; the binary is a
static x86_64 ELF and `sluice --version` answers `sluice 0.1.0`; both
archives match `SHA256SUMS` (`sha256sum -c`); the x86_64 archive holds
`sluice`, `README.md` and `LICENSE` under `sluice-0.1.0-<target>/`.
The README Install section now leads with the `curl | sh` line and the
release page.

Still open: the tag must match the crate version by hand (the workflow
does not check), and the crates are not published to crates.io (separate
step, needs the user's explicit "publish").

## Release guard and release script (2026-10-22)

- `.github/workflows/release.yml`, job `test`, first step "Tag matches the
  workspace version": reads `version` from `[workspace.package]` in
  `Cargo.toml` and fails the run when `v<version>` differs from the pushed
  tag, so a mislabelled binary is never published (it would make
  `sluice update` see no update, or the wrong one).
- `scripts/release.sh <X.Y.Z>`: refuses a dirty tree, a branch other than
  `main`, an existing tag and a non `X.Y.Z` version; bumps the workspace
  version and the five internal dependency versions, `cargo update
  --workspace`, bumps `plugin/.claude-plugin/plugin.json` when present,
  runs `sluice run fmt|clippy|test`, commits `chore(release): vX.Y.Z` and
  creates the annotated tag. It never pushes; it prints
  `git push origin main vX.Y.Z`.
- Verified: `sh -n`; argument validation (`1.x` and no argument refused);
  the two `sed` expressions applied to a copy of `Cargo.toml` change
  exactly the workspace version and the five internal dependency lines;
  the workflow's extraction prints `0.1.0` on the current tree. The full
  script and the workflow step run for real at the next release.

## Second release: `v0.2.0` (2026-10-22)

Cut with `scripts/release.sh 0.2.0` (first real run of the script: version
bump in `Cargo.toml`, `Cargo.lock` and `plugin/.claude-plugin/plugin.json`,
gates through Sluice, commit `chore(release): v0.2.0`, annotated tag) and
pushed with the user's approval. Workflow run 39847226061: the new
"Tag matches the workspace version" step passed, then `test`, both
`build` jobs and `release` succeeded. Assets: x86_64 4.8 MB, aarch64
4.4 MB, `SHA256SUMS`.

First real `sluice update`: the user's installed 0.1.0 (built from source
in `~/.cargo/bin`) reported `update available: 0.1.0 -> 0.2.0` with
`--check`, then `--yes` downloaded the x86_64 archive, verified the
checksum, replaced the binary (now the static release build, `sluice
0.2.0`), printed the cargo-metadata note, stopped the idle daemon and
reminded that running MCP servers keep the old binary. `sluice doctor`
afterwards: no failures.
