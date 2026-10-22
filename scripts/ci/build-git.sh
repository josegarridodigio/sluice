#!/bin/sh
# Builds a given git version from source into a prefix.
# Usage: build-git.sh <version> <prefix>
set -eu
version="${1:?usage: build-git.sh <version> <prefix>}"
prefix="${2:?usage: build-git.sh <version> <prefix>}"
src="$(mktemp -d)"
curl -fsSL "https://mirrors.edge.kernel.org/pub/software/scm/git/git-$version.tar.xz" |
    tar -xJ -C "$src" --strip-components=1
make -C "$src" -j"$(nproc)" prefix="$prefix" \
    NO_GETTEXT=1 NO_TCLTK=1 NO_PERL=1 NO_PYTHON=1 all install
rm -rf "$src"
"$prefix/bin/git" --version
