#!/bin/sh
# Prepare a release: bump the version everywhere, verify, commit and tag.
#
# Usage: scripts/release.sh <X.Y.Z>
#
# Bumps the workspace version and the internal crate dependencies in
# Cargo.toml, refreshes Cargo.lock, bumps the plugin manifest when present,
# moves the [Unreleased] section of CHANGELOG.md into "## [X.Y.Z] - <today>"
# (scripts/changelog-release.py; refuses when Unreleased has no entries),
# runs the repository's gates through Sluice (fmt, clippy, test), commits
# "chore(release): vX.Y.Z" and creates the annotated tag vX.Y.Z.
#
# It never pushes: publishing is a human decision. The last line printed is
# the push command. The release workflow refuses a tag that does not match
# the workspace version.
set -eu

die() { printf 'release: %s\n' "$*" >&2; exit 1; }

[ $# -eq 1 ] || die "usage: scripts/release.sh <X.Y.Z>"
new="$1"
printf '%s' "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "not a X.Y.Z version: $new"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

[ -z "$(git status --porcelain --untracked-files=no)" ] || die "the working tree has uncommitted changes"
[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || die "releases are cut from main"
git rev-parse -q --verify "refs/tags/v$new" >/dev/null && die "tag v$new already exists"

old="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
[ -n "$old" ] || die "cannot read the workspace version from Cargo.toml"
[ "$old" != "$new" ] || die "the workspace is already at $new"
command -v sluice >/dev/null 2>&1 || die "sluice is not in PATH (cargo install --path crates/sluice-gate)"

[ -f CHANGELOG.md ] || die "CHANGELOG.md is missing"
command -v python3 >/dev/null 2>&1 || die "python3 is needed to update CHANGELOG.md"

echo "release: $old -> $new"

# Changelog first: it refuses an empty Unreleased before anything is edited.
python3 scripts/changelog-release.py release "$new" || die "CHANGELOG.md not updated"

# Workspace version and the internal path dependencies (version = "...").
sed -i \
    -e "/^\[workspace.package\]/,/^\[/s/^version = \"$old\"/version = \"$new\"/" \
    -e "/^sluice-gate-[a-z]* = { path = /s/version = \"$old\"/version = \"$new\"/" \
    Cargo.toml
cargo update -q --workspace

manifest="plugin/.claude-plugin/plugin.json"
if [ -f "$manifest" ]; then
    sed -i "s/\"version\": \"$old\"/\"version\": \"$new\"/" "$manifest"
fi

sluice run fmt
sluice run clippy
sluice run test

git add Cargo.toml Cargo.lock CHANGELOG.md
[ -f "$manifest" ] && git add "$manifest"
git commit -q -m "chore(release): v$new"
git tag -a "v$new" -m "Sluice $new"

echo "release: committed and tagged v$new. Publish with:"
echo "  git push origin main v$new"
