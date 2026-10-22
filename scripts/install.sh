#!/bin/sh
# Sluice installer (Linux only).
#   curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh
# Environment:
#   SLUICE_VERSION      pin a release tag, e.g. v0.1.0 (default: latest)
#   SLUICE_INSTALL_DIR  target directory (default: $HOME/.local/bin)
set -eu

REPO="josegarridodigio/sluice"

say() { printf '%s\n' "$*"; }
die() { printf 'sluice-install: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "only Linux is supported"
case "$(uname -m)" in
  x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

if [ -n "${SLUICE_VERSION:-}" ]; then
  base="https://github.com/$REPO/releases/download/$SLUICE_VERSION"
else
  base="https://github.com/$REPO/releases/latest/download"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "curl or wget is required"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Fetching checksums from $base"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download SHA256SUMS (does the release exist?)"

# Asset names embed the version, so resolve the name from SHA256SUMS.
line="$(grep -E "[ *]sluice-.*-$target\\.tar\\.gz\$" "$tmp/SHA256SUMS" | head -n 1 || true)"
[ -n "$line" ] || die "no asset for $target in SHA256SUMS"
asset="${line##* }"
asset="${asset#\*}"

say "Downloading $asset"
fetch "$base/$asset" "$tmp/$asset" || die "could not download $asset"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$tmp" && printf '%s\n' "$line" | sha256sum -c - >/dev/null) || die "checksum mismatch for $asset"
  say "Checksum OK"
else
  say "warning: sha256sum not found, skipping checksum verification" >&2
fi

tar -xzf "$tmp/$asset" -C "$tmp"
bin="$tmp/${asset%.tar.gz}/sluice"
[ -f "$bin" ] || die "archive does not contain sluice"

dir="${SLUICE_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dir"
cp "$bin" "$dir/sluice.new"
chmod 755 "$dir/sluice.new"
mv -f "$dir/sluice.new" "$dir/sluice"

say "Installed: $("$dir/sluice" --version)"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "Note: $dir is not in your PATH. Add it with:"
     say "  export PATH=\"$dir:\$PATH\"" ;;
esac
