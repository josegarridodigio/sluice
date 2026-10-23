#!/usr/bin/env bash
# Generate the compatibility golden files from the released Sluice binaries.
#
#   scripts/compat/generate.sh [vX.Y.Z ...]      (default: v0.1.0 v0.2.0)
#
# Downloads each release with `gh`, verifies SHA256SUMS, runs the binary in a
# private HOME/XDG tree and a short runtime directory (/tmp/sa-compat.*), and
# writes the fixtures under crates/*/tests/fixtures/compat/<version>/.
# Needs: gh (authenticated), git, python3 >= 3.11. See docs/compatibility.md
# and kb/sluice/changes/compat-golden-files.md.
#
# Never edit the output by hand and never regenerate a released version to
# make a test pass: fixtures of a release are a record of that release.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TOOLS="$ROOT/scripts/compat/tools.py"
PROXY="$ROOT/scripts/compat/record-proxy.py"
REPO_SLUG=${COMPAT_REPO:-josegarridodigio/sluice}
S=$(mktemp -d /tmp/sa-compat.XXXXXX)   # short: Unix socket paths are limited to 107 bytes
DL=${COMPAT_DOWNLOADS:-}                  # optional directory of pre-downloaded v*/ trees

PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
  done
}
trap cleanup EXIT

py() { python3 -I "$@"; }

# --- per-version state (set by gen_version) --------------------------------
V="" ; W="" ; BIN="" ; H="" ; RUN="" ; REPO="" ; CSOCK=""

# sl ARGS...: run the release binary with a clean, private environment.
# Never inherit CLAUDECODE & co: agent detection would change the hook policy.
sl() {
  env -i PATH="$W/bin:/usr/bin:/bin" HOME="$H" XDG_CONFIG_HOME="$H/.config" \
    XDG_STATE_HOME="$H/.state" XDG_RUNTIME_DIR="$RUN" XDG_CACHE_HOME="$H/.cache" \
    SLUICE_AGENT=0 SLUICE_SOCKET="${CSOCK:-$RUN/real.sock}" \
    ${SLUICE_CONFIG:+SLUICE_CONFIG="$SLUICE_CONFIG"} \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
    GIT_AUTHOR_NAME=Compat GIT_AUTHOR_EMAIL=compat@example.invalid \
    GIT_COMMITTER_NAME=Compat GIT_COMMITTER_EMAIL=compat@example.invalid \
    LC_ALL=C TERM=dumb "$BIN" "$@"
}

# isolated CMD...: any command inside the same private environment.
isolated() {
  env -i PATH="$W/bin:/usr/bin:/bin" HOME="$H" XDG_CONFIG_HOME="$H/.config" \
    XDG_STATE_HOME="$H/.state" XDG_RUNTIME_DIR="$RUN" XDG_CACHE_HOME="$H/.cache" \
    SLUICE_AGENT=0 SLUICE_SOCKET="$RUN/none.sock" LC_ALL=C "$@"
}

# gitq ARGS...: git with the same private identity.
gitq() {
  env -i PATH="$W/bin:/usr/bin:/bin" HOME="$H" XDG_RUNTIME_DIR="$RUN" \
    SLUICE_AGENT=0 SLUICE_SOCKET="${CSOCK:-$RUN/real.sock}" \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
    GIT_AUTHOR_NAME=Compat GIT_AUTHOR_EMAIL=compat@example.invalid \
    GIT_COMMITTER_NAME=Compat GIT_COMMITTER_EMAIL=compat@example.invalid \
    LC_ALL=C git "$@"
}

start_daemon() {
  env -i PATH="$W/bin:/usr/bin:/bin" HOME="$H" XDG_CONFIG_HOME="$H/.config" \
    XDG_STATE_HOME="$H/.state" XDG_RUNTIME_DIR="$RUN" XDG_CACHE_HOME="$H/.cache" \
    SLUICE_AGENT=0 SLUICE_SOCKET="$RUN/real.sock" \
    ${SLUICE_CONFIG:+SLUICE_CONFIG="$SLUICE_CONFIG"} LC_ALL=C \
    "$BIN" daemon --foreground >"$W/daemon.out" 2>&1 &
  DAEMON_PID=$!
  PIDS+=("$DAEMON_PID")
  for _ in $(seq 1 100); do
    [ -S "$RUN/real.sock" ] && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$W/daemon.out"; return 1; }
    sleep 0.1
  done
  echo "daemon did not start" >&2; cat "$W/daemon.out" >&2; return 1
}

stop_daemon() {
  CSOCK="$RUN/real.sock" sl daemon stop >/dev/null 2>&1 || true
  for _ in $(seq 1 100); do
    kill -0 "$DAEMON_PID" 2>/dev/null || return 0
    sleep 0.1
  done
  kill "$DAEMON_PID" 2>/dev/null || true
}

# record NAME CMD...: run CMD through a recording proxy; transcript -> $TR/NAME.ndjson
record() {
  local name=$1; shift
  local sock="$RUN/p-$name.sock"
  rm -f "$TR/$name.ndjson"
  python3 -I "$PROXY" --listen "$sock" --upstream "$RUN/real.sock" --out "$TR/$name.ndjson" &
  local pp=$!
  for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done
  CSOCK="$sock" "$@" || echo "  ($name: exit $?)"
  sleep 0.3
  kill "$pp" 2>/dev/null || true
  wait "$pp" 2>/dev/null || true
}

sha_of_asset() { grep -F "$1" "$2/SHA256SUMS" | cut -d' ' -f1; }

write_sums() {   # DIR: SHA256SUMS of every file below DIR except itself
  (cd "$1" && find . -type f ! -name SHA256SUMS | LC_ALL=C sort | sed 's|^\./||' \
     | xargs sha256sum > SHA256SUMS)
}

fixture_repo() {   # DIR CONFIG: the repository the recorded runs happen in
  rm -rf "$1"; mkdir -p "$1"
  gitq -C "$1" init -q
  printf 'alpha\n' > "$1/a.txt"
  cp "$2" "$1/.sluice.toml"
  gitq -C "$1" add -A
}

gen_version() {
  V=$1
  W="$S/$V"; H="$W/home"; RUN="$W/run"; REPO="$W/repo"
  mkdir -p "$W/bin" "$H/.config" "$H/.state" "$RUN"
  chmod 700 "$RUN"
  echo "== $V"

  # 1. download, verify, extract
  if [ -n "$DL" ] && [ -d "$DL/$V" ]; then
    cp "$DL/$V"/* "$W/" 2>/dev/null || true
  else
    gh release download "$V" -R "$REPO_SLUG" -p '*x86_64*' -p SHA256SUMS -D "$W"
  fi
  (cd "$W" && sha256sum -c --ignore-missing SHA256SUMS)
  tar xzf "$W"/sluice-*-x86_64-unknown-linux-musl.tar.gz -C "$W/bin" --strip-components=1
  BIN="$W/bin/sluice"
  ARCHIVE=$(cd "$W" && ls sluice-*-x86_64-unknown-linux-musl.tar.gz)
  ARCHIVE_SHA=$(sha_of_asset "$ARCHIVE" "$W")
  VERSION_LINE=$(sl --version)
  HAS_QUEUE=no
  sl queue --help >/dev/null 2>&1 && HAS_QUEUE=yes

  # Output directories (owning crate of each fixture, see design section 6)
  CORE="$ROOT/crates/sluice-gate-core/tests/fixtures/compat/$V"
  PROTO="$ROOT/crates/sluice-gate-proto/tests/fixtures/compat/$V"
  DAEMON="$ROOT/crates/sluice-gate-daemon/tests/fixtures/compat/$V"
  QUEUE="$ROOT/crates/sluice-gate-queue/tests/fixtures/compat/$V"
  GATE="$ROOT/crates/sluice-gate/tests/fixtures/compat/$V"
  OUTS=("$CORE" "$PROTO" "$DAEMON" "$GATE" "$QUEUE")
  for d in "${OUTS[@]}"; do
    case "$d" in "$ROOT"/crates/*/tests/fixtures/compat/v*) rm -rf "$d" ;; esac
    mkdir -p "$d"
  done
  TR="$PROTO/transcripts"; mkdir -p "$TR"
  mkdir -p "$CORE/config" "$CORE/user-config" "$CORE/markers" "$CORE/repo-files" \
           "$PROTO/status" "$DAEMON/db" "$GATE"

  # 2. repository configs: doc example of the tag, minimal, init output, recorded
  mkdir -p "$W/doc"
  gitq -C "$ROOT" show "$V:kb/sluice/design/config-format.md" > "$W/doc/config-format.md"
  py "$TOOLS" extract-config "$W/doc/config-format.md" repo > "$W/doc/repo-example.toml"
  py "$TOOLS" extract-config "$W/doc/config-format.md" user > "$CORE/user-config/full.toml"
  printf 'version = 1\n\n[gates.test]\ncmd = "true"\n' > "$CORE/config/minimal.toml"
  if grep -q '^\[queue\]' "$W/doc/repo-example.toml"; then
    cp "$W/doc/repo-example.toml" "$CORE/config/queue.toml"
    # full.toml: the same example without the [queue] table
    awk '/^\[queue\]/{skip=1; next} /^\[/{skip=0} !skip' "$W/doc/repo-example.toml" > "$CORE/config/full.toml"
  else
    cp "$W/doc/repo-example.toml" "$CORE/config/full.toml"
  fi
  : > "$CORE/user-config/empty.toml"
  mkdir -p "$W/init" && (cd "$W/init" && gitq init -q && sl init >/dev/null && cp .sluice.toml "$CORE/config/init.toml")

  cat > "$CORE/config/recorded.toml" <<'EOF'
version = 1

[defaults]
agent_hook_policy = "fail-fast"
human_hook_policy = "wait"
tail_lines = 20

[gates.test]
cmd = "true"
weight = 1
timeout = "1m"
hooks = ["pre-commit"]

[gates.fail]
cmd = "false"

[gates.timeout]
cmd = "sleep 5"
timeout = "1s"

[gates.modify]
cmd = "echo x >> a.txt"

[gates.slow]
cmd = "sleep 6"

[gates.keyed]
cmd = "true    # key   material"
key_inputs = ["echo hi"]
key_files = ["a.txt", "missing.txt"]
env = { B = "2", A = "1" }
EOF
  cp "$CORE/config/recorded.toml" "$CORE/repo-files/sluice.toml"
  printf 'alpha\n' > "$CORE/repo-files/a.txt"

  py "$TOOLS" expected-repo "$CORE"/config/*.toml > "$CORE/config/expected.json"
  py "$TOOLS" expected-user "$CORE"/user-config/*.toml > "$CORE/user-config/expected.json"

  # 3. every config must be accepted by the release itself
  fixture_repo "$REPO" "$CORE/config/recorded.toml"
  for f in "$CORE"/config/*.toml; do
    rm -rf "$W/cfgcheck"; fixture_repo "$W/cfgcheck" "$f"
    out=$( (cd "$W/cfgcheck" && CSOCK="$RUN/none.sock" sl run __no_such_gate__ 2>&1) || true)
    case "$out" in
      *"unknown gate"*) ;;
      *) echo "config $f rejected by $V: $out" >&2; exit 1 ;;
    esac
  done
  # user configs: the daemon must start with them
  for f in "$CORE"/user-config/*.toml; do
    SLUICE_CONFIG="$f" start_daemon
    stop_daemon
    rm -f "$RUN/real.sock"
  done
  unset SLUICE_CONFIG
  sleep 0.3

  # 4. daemon scenarios, recorded through the proxy
  start_daemon
  cd "$REPO"
  echo "  hello-welcome"
  python3 -I "$PROXY" --listen "$RUN/p-hello.sock" --upstream "$RUN/real.sock" --out "$TR/hello-welcome.ndjson" &
  PP=$!; sleep 0.5
  py "$TOOLS" hello "$RUN/p-hello.sock" > /dev/null
  sleep 0.3; kill "$PP"; wait "$PP" 2>/dev/null || true

  echo "  run-pass, run-hit-event"
  record run-pass sl run test
  record run-hit-event sl run test
  echo "  run-fail, run-timeout, run-modified, run-keyed"
  record run-fail sl run fail
  record run-timeout sl run timeout
  record run-modified sl run modify
  printf 'alpha\n' > "$REPO/a.txt"      # undo the gate's edit
  record run-keyed sl run keyed

  echo "  run-slow + status (a running and a finished job)"
  : > "$W/slow.out"
  (
    export V W H RUN BIN
    record run-slow sl run slow
  ) &
  SLOW=$!
  sleep 1.5
  record status sl status --json > "$PROTO/status/status.json"
  wait "$SLOW" || true

  echo "  logs"
  record logs sl logs -n 5
  JOB=$(ls "$H/.state/sluice/logs" | grep -v '^daemon' | head -1 | sed 's/\.log$//')
  record logs-job sl logs -n 3 "$JOB"

  # 5. pre-commit hook: marker with committed_tree
  echo "  hook marker (committed_tree)"
  HOOK="$W/repo-hook"; rm -rf "$HOOK"; mkdir -p "$HOOK"
  gitq -C "$HOOK" init -q
  printf 'alpha\n' > "$HOOK/a.txt"; printf 'beta\n' > "$HOOK/b.txt"
  cp "$CORE/config/recorded.toml" "$HOOK/.sluice.toml"
  gitq -C "$HOOK" add -A
  gitq -C "$HOOK" commit -q -m initial
  mkdir -p "$HOOK/.githooks"
  printf '#!/bin/sh\nset -e\nsluice run --hook pre-commit test\n' > "$HOOK/.githooks/pre-commit"
  chmod +x "$HOOK/.githooks/pre-commit"
  gitq -C "$HOOK" config core.hooksPath .githooks
  printf 'alpha2\n' > "$HOOK/a.txt"; gitq -C "$HOOK" add a.txt
  printf 'beta2\n' > "$HOOK/b.txt"                       # unstaged change
  (cd "$HOOK" && CSOCK="$RUN/real.sock" gitq commit -q -m "hook run" 2>&1) || true

  # 6. merge queue (v0.2.0 and later)
  if [ "$HAS_QUEUE" = yes ]; then
    echo "  queue"
    QR="$W/queue-remote.git"; QC="$W/queue-clone"
    rm -rf "$QR" "$QC"
    gitq init -q --bare -b main "$QR"
    gitq clone -q "$QR" "$QC" 2>/dev/null
    gitq -C "$QC" checkout -q -b main
    printf 'alpha\n' > "$QC/a.txt"
    cat > "$QC/.sluice.toml" <<'EOF'
version = 1

[gates.test]
cmd = "true"
hooks = ["pre-push"]

[queue]
remote = "origin"
target = "main"
gates = ["test"]
max_train = 1
EOF
    gitq -C "$QC" add -A && gitq -C "$QC" commit -q -m base
    gitq -C "$QC" push -q -u origin main
    gitq -C "$QC" checkout -q -b feature
    printf 'feature\n' > "$QC/f.txt"
    gitq -C "$QC" add -A && gitq -C "$QC" commit -q -m feature
    mkdir -p "$QUEUE/queue-status"
    (
      cd "$QC"
      record queue-status-empty sl queue status --json > "$QUEUE/queue-status/empty.json"
      record queue-enqueue sl queue enqueue feature
      ready=no
      for _ in $(seq 1 120); do
        if CSOCK="$RUN/real.sock" sl queue status --json | grep -q '"ready"'; then ready=yes; break; fi
        sleep 0.5
      done
      [ "$ready" = yes ] || echo "  (queue train did not reach ready)" >&2
      record queue-status sl queue status --json > "$QUEUE/queue-status/populated.json"
    )
    mkdir -p "$PROTO/queue-status"
    cp "$QUEUE/queue-status"/*.json "$PROTO/queue-status/"
  fi
  cd "$ROOT"

  # 7. stop; database; status without a daemon
  echo "  stop, database, status without a daemon"
  (cd "$REPO" && record stop sl daemon stop) || true
  for _ in $(seq 1 100); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 0.1; done
  (cd "$REPO" && CSOCK="$RUN/none.sock" sl status --json > "$PROTO/status/status-no-daemon.json")
  DB="$H/.state/sluice/sluice.db"
  py "$TOOLS" checkpoint "$DB"
  cp "$DB" "$DAEMON/db/sluice.db"
  mkdir -p "$QUEUE/db"; cp "$DB" "$QUEUE/db/sluice.db"   # the queue crate opens the same file

  # 8. markers, miss records, inputs.json, markers-bad
  for m in "$REPO"/.git/sluice/markers/*.json "$HOOK"/.git/sluice/markers/*.json; do
    [ -f "$m" ] && cp "$m" "$CORE/markers/"
  done
  # the queue repo's markers are not fixtures (gate `test` there has another tree)
  py "$TOOLS" inputs "$CORE/markers" "$CORE/config/recorded.toml" "$REPO" > "$CORE/inputs.json"
  py "$TOOLS" bad-markers "$CORE/markers" "$CORE/markers-bad"

  # 9. transcripts: normalise absolute scratch paths into placeholders
  for t in "$TR"/*.ndjson; do
    py "$TOOLS" normalise "$t" "$REPO={{REPO}}" "$HOOK={{HOOK}}" \
      "$W/queue-clone={{QUEUE_CLONE}}" "$W/queue-remote.git={{QUEUE_REMOTE}}" \
      "$H={{HOME}}" "$RUN={{RUN}}" "$W={{SCRATCH}}"
  done
  # recordings of status/queue replies used as fixtures keep their real paths.

  # 10. CLI surface and MCP tools
  isolated python3 -I "$TOOLS" cli-help "$BIN" > "$GATE/cli-help.txt"
  isolated python3 -I "$TOOLS" mcp-tools "$BIN" > "$GATE/mcp-tools.json"   # tested from the gate crate (it owns the binary)

  # 11. READMEs and checksums
  readme "$CORE" "core: configs, user configs, markers, miss records, inputs.json, markers-bad"
  readme "$PROTO" "proto: status JSON, queue status JSON, protocol transcripts"
  readme "$DAEMON" "daemon: sluice.db after the recorded scenarios"
  readme "$GATE" "gate: cli-help.txt (--help of every subcommand) and mcp-tools.json (the MCP tools/list result)"
  readme "$QUEUE" "queue: sluice.db (the same file as the daemon fixture) and, since v0.2.0, queue status JSON"
  for d in "${OUTS[@]}"; do write_sums "$d"; done
}

readme() {   # DIR WHAT
  cat > "$1/README.md" <<EOF
# Compatibility golden files: $V

$2.

Recorded from the release binary \`$VERSION_LINE\`
(archive \`$ARCHIVE\`, sha256 \`$ARCHIVE_SHA\`) by
\`scripts/compat/generate.sh\` (task H3). Never edit these files to make a
test pass; a changed format needs a new version directory (ADR-0013). The
exact commands, normalisations and the tests that read the files are in
\`kb/sluice/changes/compat-golden-files.md\`. Integrity: \`SHA256SUMS\`.
EOF
}

if [ "$#" -gt 0 ]; then VERSIONS=("$@"); else VERSIONS=(v0.1.0 v0.2.0); fi
for v in "${VERSIONS[@]}"; do gen_version "$v"; done
echo "done; scratch kept at $S"
