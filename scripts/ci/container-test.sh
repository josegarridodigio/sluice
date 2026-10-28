#!/bin/sh
# Runs inside a distro container (see .github/workflows/matrix.yml): installs
# git, a C toolchain and the tools the tests call, installs rustup with the
# toolchain from rust-toolchain.toml, then runs `cargo test --locked`.
#
# Environment:
#   CI_CARGO_ARGS  arguments after `cargo test --locked --no-fail-fast`
#                  (default: --workspace)
#   CI_OWNER       uid:gid to chown the mounted cache directories to
# Plain POSIX sh: Alpine has no bash.
set -eu

. /etc/os-release
case "$ID" in
ubuntu | debian)
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
        git gcc make libc6-dev curl ca-certificates tar procps coreutils \
        python3 python3-venv >/dev/null
    ;;
fedora)
    dnf install -y -q git gcc make curl ca-certificates tar procps-ng coreutils findutils python3
    ;;
alpine)
    # git-daemon ships `git http-backend`, which the HTTP transport tests serve with.
    apk add --no-cache git git-daemon gcc musl-dev make curl ca-certificates tar procps \
        coreutils python3 py3-pip >/dev/null
    ;;
*)
    echo "unsupported distribution: $ID" >&2
    exit 1
    ;;
esac

export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rustup}"
export CARGO_HOME="${CARGO_HOME:-/opt/cargo}"
export PATH="$CARGO_HOME/bin:$PATH"

# rustup-init picks the host triple itself (x86_64-unknown-linux-musl on
# Alpine). With no default toolchain, the first cargo call in the checkout
# installs the one pinned in rust-toolchain.toml.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain none >/dev/null

cd /work
git config --global --add safe.directory '*'
git config --global user.name ci
git config --global user.email ci@example.invalid

# The hook managers of the real-tool tests (lefthook, pre-commit).
sh /work/scripts/ci/install-hook-managers.sh /opt/hook-managers
export PATH="/opt/hook-managers/bin:$PATH"

echo "distribution: $PRETTY_NAME"
git --version
rustc --version
cargo --version

status=0
# --no-fail-fast: one failing test binary must not hide the others (the first
# matrix run stopped Alpine at guard.rs and Ubuntu at daemon.rs).
# shellcheck disable=SC2086
cargo test --locked --no-fail-fast ${CI_CARGO_ARGS:---workspace} || status=$?

# The cache directories are mounted from the runner; hand them back.
if [ -n "${CI_OWNER:-}" ]; then
    chown -R "$CI_OWNER" /work/target "$CARGO_HOME/registry" "$CARGO_HOME/git" 2>/dev/null || true
fi
exit "$status"
