---
title: "Change: sluice update (self-update)"
tags: [sluice, change, cli, release]
---

# Change: `sluice update` (2026-10-22)

## What changed

- New subcommand `sluice update [--check] [--version <vX.Y.Z>] [--yes] [--no-restart]`.
  It resolves the latest release from the redirect of
  `https://github.com/josegarridodigio/sluice/releases/latest` (no API, no
  token) or takes `--version`, compares `X.Y.Z` with `CARGO_PKG_VERSION`,
  downloads `sluice-<version>-<target>.tar.gz` and `SHA256SUMS` (same layout
  as `scripts/install.sh`), verifies the SHA-256, extracts with `tar`, checks
  that the new binary prints `sluice <version>` and replaces the running
  executable (`current_exe`, symlinks resolved) by copying to
  `.sluice.new.<pid>` next to it, mode 0755, then `rename`.
- Unless `--no-restart`, an idle daemon is stopped gracefully
  (`daemon_cmd::stop(false)`); with gates running or queued it is left alone
  and a message says it keeps the old version until `sluice daemon stop`.
  Running `sluice mcp` servers keep the old binary until the agent reconnects
  (said in the final message).
- Confirmation on a TTY, `--yes` to skip, exit 2 without either. A downgrade
  through `--version` follows the same rule (prompt says "Downgrade").
- Exit codes: 0 updated, up to date or `--check`; 1 error; 2 usage.
- `SLUICE_UPDATE_BASE_URL` (directory or `file://` URL with the archive,
  `SHA256SUMS` and a `LATEST` file, or pass `--version`) is the test hook.
- Under `~/.cargo/bin` a note says cargo's install metadata will not know the
  new version and that `cargo install` would overwrite it.

## Files and symbols

- `crates/sluice-gate/src/update.rs` (`run`, `do_update`, `Version`,
  `target_for`, `Fetcher`, `resolve`, `find_asset`, `fetch_binary`,
  `replace_executable`, `restart_daemon`), wired in `main.rs`
  (`Command::Update`, long help).
- `crates/sluice-gate/Cargo.toml`: `sha2`, `hex` (already in the lockfile via
  core/queue) and `tempfile` (was dev-only). No HTTP or TLS crate.
- `README.md` Install section, `kb/sluice/design/architecture.md` section 1.

## Why `curl` / `tar` binaries

The installer already requires them, so the update path has the same
prerequisites and the binary stays free of an HTTP/TLS stack (size, audit
surface, static musl build). `wget` is the fallback for downloads.

## Verification

Unit tests (versions, arch mapping, `SHA256SUMS` parsing) and
`crates/sluice-gate/tests/update.rs` (10 cases over a `file://` fake
release, on a private copy of the binary): `--check` does not write, `--yes`
replaces, explicit version, missing LATEST, checksum mismatch, missing
checksum line, up to date, no TTY (exit 2), unwritable directory (hint, file
untouched; skipped as root). Final gates: `sluice run fmt`, `clippy`, `test`.
