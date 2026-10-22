#!/bin/sh
# Records scripts/demo/demo.sh and renders docs/assets/demo.gif.
# Everything runs under a private HOME so the real daemon is untouched.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
export PATH="$HOME/.cargo/bin:$PATH"
cd "$root"
cargo build --release -p sluice-gate

tmp=$(mktemp -d)
export HOME="$tmp/home" XDG_RUNTIME_DIR="$tmp/run" XDG_STATE_HOME="$tmp/state" \
       XDG_CONFIG_HOME="$tmp/config" SLUICE_AGENT=0
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
unset CLAUDECODE SLUICE_SOCKET SLUICE_CONFIG SLUICE_DISABLE SLUICE_CACHE CI
mkdir -p "$tmp/bin"
cp "$root/target/release/sluice" "$tmp/bin/sluice"
export PATH="$tmp/bin:$PATH"
cleanup() {
  sluice daemon stop --force >/dev/null 2>&1 || true
  [ -n "${KEEP:-}" ] || rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

export DEMO_REPO="$tmp/work/app"
mkdir -p "$DEMO_REPO"
cd "$DEMO_REPO"
git init -q
git config user.email demo@example.com
git config user.name demo
cat > .sluice.toml <<'TOML'
version = 1

[gates.test]
cmd = "sleep 4"
weight = 2

[gates.lint]
cmd = "true"
TOML
echo init > f
git add -A
git commit -q -m init
cd "$root"

out="$tmp/rec"
mkdir -p "$out"
COLUMNS=80 LINES=24 script -q -T "$out/timing" -O "$out/typescript" \
  -c "sh scripts/demo/demo.sh"
python3 scripts/demo/render-gif.py "$out/typescript" "$out/timing" docs/assets/demo.gif
