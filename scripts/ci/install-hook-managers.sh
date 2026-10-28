#!/bin/sh
# Installs the git hook managers that the real-tool tests run against
# (crates/sluice-gate/tests/hook_managers.rs, hooks.rs): lefthook and
# pre-commit, at the versions whose source was analysed (ADR-0021). Without
# them those tests skip themselves.
#
# Usage: install-hook-managers.sh [prefix]   (default: $HOME/.local)
# The tools end up in <prefix>/bin; the caller puts that directory on PATH.
# Plain POSIX sh: Alpine has no bash. Needs curl and python3 with venv.
set -eu

LEFTHOOK_VERSION=2.2.2
PRE_COMMIT_VERSION=4.7.0

prefix="${1:-$HOME/.local}"
mkdir -p "$prefix/bin"

# SHA-256 of the release binaries (lefthook_checksums.txt of the release,
# checked against a download).
case "$(uname -m)" in
x86_64 | amd64)
    arch=x86_64
    sum=4fbf89eb7748c6aed84d833ee1a29edac48fae614468ca9a82896ec32f2800b6
    ;;
aarch64 | arm64)
    arch=arm64
    sum=a4d4bf78d902af1a148bf0662af08e13b791061e46e6e2183b0660eed96b43f6
    ;;
*)
    echo "unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac
tmp="$prefix/bin/.lefthook.download"
curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp" \
    "https://github.com/evilmartians/lefthook/releases/download/v$LEFTHOOK_VERSION/lefthook_${LEFTHOOK_VERSION}_Linux_$arch"
if ! echo "$sum  $tmp" | sha256sum -c - >/dev/null; then
    rm -f "$tmp"
    echo "lefthook $LEFTHOOK_VERSION ($arch): checksum mismatch" >&2
    exit 1
fi
chmod 0755 "$tmp"
mv "$tmp" "$prefix/bin/lefthook"

python3 -m venv "$prefix/pre-commit-venv"
"$prefix/pre-commit-venv/bin/pip" install --quiet "pre-commit==$PRE_COMMIT_VERSION"
ln -sf "$prefix/pre-commit-venv/bin/pre-commit" "$prefix/bin/pre-commit"

"$prefix/bin/lefthook" version
"$prefix/bin/pre-commit" --version
