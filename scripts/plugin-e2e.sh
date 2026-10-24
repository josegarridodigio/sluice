#!/bin/sh
# End-to-end check of the Claude Code plugin (plugin/) with real sessions.
#
# Usage: scripts/plugin-e2e.sh
#   SLUICE_E2E_MODEL  model of the agent sessions (default haiku)
#   SLUICE_BIN        use this sluice binary instead of building one
#   SLUICE_E2E_CASES  comma-separated cases to run (default a,c,b,d,e,f,g,h,i,j,k,l,m,n)
#
# Needs the `claude` CLI (logged in), git and python3. Everything happens in a
# temporary directory: validation of the marketplace and the plugin, a scratch
# clone of this repository whose `test` gate is a fast `cargo test --help`
# (so the raw runner `cargo test` is covered by the gate), a private
# HOME/XDG for Sluice and `sluice` on a private PATH (the plugin's .mcp.json
# runs the bare command `sluice`). The real Claude Code login is reused the
# way scripts/phase3-e2e.sh does it. Two non-interactive sessions load the
# plugin with --plugin-dir:
#   (a) "run the tests"            -> expect mcp__plugin_sluice_sluice__verify (a Bash
#                                     attempt is denied by the guard first)
#   (c) "run `cargo test` with Bash" -> expect the guard to rewrite the Bash call
#                                     to `sluice run test -v` (allow, or ask in default
#                                     mode) instead of denying it: the tool input shows
#                                     the rewrite, the result the gate's output
#   (d) "run `sluice queue approve t-1 --yes`" -> expect the guard to deny it
#   (b) "approve train t-1"        -> expect a refusal, no approval
#   (e) "check and land my branch"  -> expect verify, then queue_enqueue, then
#                                     stop: the private bare remote is unchanged
# Zero-config cases (the SessionStart hook runs in every session):
#   (f) build with a subshell `(make build) > build.log 2>&1` -> the guard cannot
#       rewrite it, the shim on the session PATH must send it to the daemon
#   (g) a Task subagent runs the heavy command -> the shim reaches the subagent
#   (h) repository without .sluice.toml, "set up verification" -> `sluice init
#       --dry-run`, and no .sluice.toml created (the session must ask first)
#   (i) sluice hidden from PATH -> the reply says the binary is missing, and no
#       install command is run
#   (j) the first Bash command sees the shim dir first in PATH and the model
#       can list the gates from the additionalContext
# Containment cases (task E2 of kb/sluice/plan/containment-plan.md). They run on the
# real systemd user manager but in units named `sluice-test-e2e-<pid>*`
# (SLUICE_TEST_CONTAINMENT_PREFIX), never `sluice.slice`; the session gets its
# own runtime dir (symlinks to the manager's sockets), state dir and daemon;
# tiny commands only, no CPU or memory load. A /proc poller outside the
# session records where every process of the test units lives.
#   (k) agent (`claude`) and the plugin's `sluice mcp` in the session's `ui`
#       leaf, the Bash shell in `tools` (seen from inside and from outside)
#   (l) a subagent's Bash command is in `tools` of the same session
#   (m) Bash sandbox on (allowAllUnixSockets workaround for Ubuntu 24.04+):
#       the shell stays in `ui` at nice 19
#   (n) `--resume`: the new agent process is moved again, one env line
# Each case is asserted and the run ends with a table case -> PASS/FAIL (exit 1
# on any FAIL). A real heavy build is replaced by a fake `make` that records
# SLUICE_JOB, so the marker proves the command ran inside a daemon job.
#
# SLUICE_E2E_CASES selects cases, e.g. SLUICE_E2E_CASES=f,g,h,i,j (default all).
# The raw stream-json outputs stay in the temp dir (path printed).
set -eu

MODEL="${SLUICE_E2E_MODEL:-haiku}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
if [ -n "${SLUICE_BIN:-}" ]; then
    SLUICE="$SLUICE_BIN"
else
    cargo build -q --release -p sluice-gate --manifest-path "$ROOT/Cargo.toml"
    SLUICE="$ROOT/target/release/sluice"
fi

echo "== validate"
claude --version
claude plugin validate "$ROOT" | tail -n 1
claude plugin validate "$ROOT/plugin" | tail -n 1
sh "$ROOT/plugin/hooks/test-guard.sh" | tail -n 1

# Keep the toolchain of the real HOME: a private HOME would make rustup
# download a toolchain (about 2 GB) inside the scratch directory.
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
TMP="$(mktemp -d)"
echo "work dir: $TMP"
export HOME="$TMP/home" XDG_RUNTIME_DIR="$TMP/home/runtime" XDG_STATE_HOME="$TMP/home/state" XDG_CONFIG_HOME="$TMP/home/config"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$TMP/bin"
# Claude Code keeps its own state under HOME; reuse the real login.
ln -s "/home/jose/.claude" "$HOME/.claude" 2>/dev/null || true
cp /home/jose/.claude.json "$HOME/.claude.json" 2>/dev/null || true
ln -s "$SLUICE" "$TMP/bin/sluice"
export PATH="$TMP/bin:$PATH"
trap 'if type contain_cleanup >/dev/null 2>&1; then contain_cleanup; fi; sluice daemon stop --force >/dev/null 2>&1 || true' EXIT INT TERM
POLLER=""; CONTAIN_DIRS=""
PREFIX="sluice-test-e2e-$$"
# Stops the poller and every unit of this run's prefix (never anything else).
contain_cleanup() {
    [ -n "$POLLER" ] && { kill "$POLLER" 2>/dev/null || true; wait "$POLLER" 2>/dev/null || true; POLLER=""; }
    for d in $CONTAIN_DIRS; do
        XDG_RUNTIME_DIR="$d" XDG_STATE_HOME="$TMP/state-${d##*rt-}" sluice daemon stop --force >/dev/null 2>&1 || true
    done
    CONTAIN_DIRS=""
    _rt="/run/user/$(id -u)"
    for u in $(XDG_RUNTIME_DIR="$_rt" systemctl --user list-units --all --plain --no-legend "${PREFIX}*" 2>/dev/null | awk '{print $1}'); do
        XDG_RUNTIME_DIR="$_rt" systemctl --user stop "$u" >/dev/null 2>&1 || true
    done
    XDG_RUNTIME_DIR="$_rt" systemctl --user reset-failed "${PREFIX}*" >/dev/null 2>&1 || true
    # The implicit parent slice of the prefix outlives its children: stop it
    # too, unless another run still has units under it.
    if [ -z "$(XDG_RUNTIME_DIR="$_rt" systemctl --user list-units --all --plain --no-legend 'sluice-test-e2e-*' 2>/dev/null | awk '$3 != "inactive"')" ]; then
        XDG_RUNTIME_DIR="$_rt" systemctl --user stop sluice-test-e2e.slice >/dev/null 2>&1 || true
    fi
}

# Scratch repository with a fast gate whose command is a raw runner.
git clone -q "$ROOT" "$TMP/repo"
cd "$TMP/repo"
git config user.name e2e; git config user.email e2e@example.invalid; git config commit.gpgsign false
# The clone follows the branch checked out in ROOT (a worktree branch, say); the queue needs main.
git branch -M main
cat > .sluice.toml <<'EOF'
version = 1

[gates.test]
cmd = "cargo test --help > /dev/null"
weight = 1
timeout = "2m"
hooks = ["pre-commit"]
EOF
# Without the repository's CLAUDE.md, only the plugin (skill, hook, server
# instructions) tells the agent about Sluice.
git rm -q CLAUDE.md
git add .sluice.toml && git -c core.hooksPath=/dev/null commit -q -m "e2e gate"
# A private bare remote: the queue fetches from it and nothing leaves $TMP.
git clone -q --bare "$TMP/repo" "$TMP/remote.git"
git remote set-url origin "$TMP/remote.git"
git fetch -q origin
git remote set-head origin main >/dev/null 2>&1 || git remote set-head origin -a >/dev/null

# Cases selected by SLUICE_E2E_CASES (default: all).
CASES="${SLUICE_E2E_CASES:-a,c,b,d,e,f,g,h,i,j,k,l,m,n}"
want() { case ",$CASES," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }
RESULTS="$TMP/results.txt"; : > "$RESULTS"
FAILED=0

# A fake heavy build tool on the private PATH. The marker records SLUICE_JOB:
# set only when the command runs inside a daemon job (through the shim).
mkdir -p "$TMP/marks"
cat > "$TMP/bin/make" <<EOF
#!/bin/sh
echo "make \$* job=\${SLUICE_JOB:-none}" >> "$TMP/marks/make.log"
sleep 1
echo "fake make: built"
EOF
chmod +x "$TMP/bin/make"
SHIM_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/sluice/shims"

# A repository with gates of distinctive names (case j), and one without
# .sluice.toml (case h).
mkdir -p "$TMP/repo-gates" "$TMP/repo-noconf"
( cd "$TMP/repo-gates" && git init -q -b main && git config user.email e2e@example.invalid && git config user.name e2e \
    && printf 'version = 1\n\n[gates.zeta-lint]\ncmd = "true"\nweight = 1\n\n[gates.omega-check]\ncmd = "true"\nweight = 1\n' > .sluice.toml \
    && git add .sluice.toml && git -c core.hooksPath=/dev/null commit -q -m init )
( cd "$TMP/repo-noconf" && git init -q -b main && git config user.email e2e@example.invalid && git config user.name e2e \
    && printf '{"name":"demo","scripts":{"build":"tsc","test":"jest","lint":"eslint ."}}\n' > package.json \
    && git add package.json && git -c core.hooksPath=/dev/null commit -q -m init )

TOOLS='Bash mcp__plugin_sluice_sluice__verify mcp__plugin_sluice_sluice__status mcp__plugin_sluice_sluice__gate_list mcp__plugin_sluice_sluice__queue_status mcp__plugin_sluice_sluice__queue_enqueue mcp__plugin_sluice_sluice__logs Skill Task Agent'

SESSION_ENV=""; SESSION_ARGS=""
session() { # session <label> <prompt> [dir] [max-turns] [path]
    label="$1"; prompt="$2"; dir="${3:-$TMP/repo}"; turns="${4:-4}"; spath="${5:-$PATH}"
    # shellcheck disable=SC2086
    ( cd "$dir" && env -u CLAUDECODE -u CLAUDE_CODE_CHILD_SESSION -u CLAUDE_CODE_SESSION_ID PATH="$spath" \
        $SESSION_ENV claude -p "$prompt" --model "$MODEL" --max-turns "$turns" $SESSION_ARGS \
          --output-format stream-json --verbose \
          --plugin-dir "$ROOT/plugin" \
          --allowedTools $TOOLS \
          --permission-mode default \
          > "$TMP/$label.jsonl" 2> "$TMP/$label.err" ) || true
}

summarise() { # summarise <label>
    python3 -I - "$TMP/$1.jsonl" <<'EOF'
import json, sys
calls, results, final = [], [], ""
for line in open(sys.argv[1]):
    try:
        ev = json.loads(line)
    except ValueError:
        continue
    if ev.get("type") == "system" and ev.get("subtype") == "init":
        tools = ev.get("tools", [])
        print("  sluice tools loaded:", sorted(t for t in tools if t.startswith("mcp__plugin_sluice_sluice__")))
        print("  plugins:", [p.get("name") for p in ev.get("plugins", [])])
    if ev.get("type") == "result":
        final = (ev.get("result") or "")[:300].replace("\n", " | ")
    msg = ev.get("message")
    if not isinstance(msg, dict):
        continue
    content = msg.get("content")
    if not isinstance(content, list):
        continue
    for block in content:
        if block.get("type") == "tool_use":
            arg = json.dumps(block.get("input"))[:110]
            calls.append(f'{block["name"]} {arg}')
        elif block.get("type") == "tool_result":
            c = block.get("content")
            if isinstance(c, list):
                c = " ".join(x.get("text", "") for x in c if isinstance(x, dict))
            results.append(("ERR " if block.get("is_error") else "") + str(c)[:160].replace("\n", " | "))
print("  tool calls:")
for c in calls:
    print("   -", c)
print("  tool results:")
for r in results:
    print("   -", r)
print("  final:", final)
EOF
}


# Assertions on a session's stream-json. Usage: check.py <case> <jsonl> [extra]
cat > "$TMP/check.py" <<'PYEOF'
import json, os, re, sys

case, path = sys.argv[1], sys.argv[2]
extra = sys.argv[3:]
calls, results, final, sids = [], {}, "", []
for line in open(path):
    try:
        ev = json.loads(line)
    except ValueError:
        continue
    if ev.get("type") == "system" and ev.get("subtype") == "init" and ev.get("session_id"):
        sids.append(ev["session_id"])
    if ev.get("type") == "result":
        final = ev.get("result") or ""
    msg = ev.get("message")
    if not isinstance(msg, dict) or not isinstance(msg.get("content"), list):
        continue
    for b in msg["content"]:
        if b.get("type") == "tool_use":
            calls.append({"id": b.get("id"), "name": b["name"], "input": b.get("input") or {}, "parent": ev.get("parent_tool_use_id")})
        elif b.get("type") == "tool_result":
            c = b.get("content")
            if isinstance(c, list):
                c = " ".join(x.get("text", "") for x in c if isinstance(x, dict))
            results[b.get("tool_use_id")] = (bool(b.get("is_error")), str(c))

def bash():
    return [c for c in calls if c["name"] == "Bash"]
def cmd(c):
    return str(c["input"].get("command", ""))
def tool(c, suffix):
    return c["name"].endswith(suffix)
def res(c):
    return results.get(c["id"], (False, ""))

problems = []
def need(ok, why):
    if not ok:
        problems.append(why)

if case == "a":
    need(any(tool(c, "__verify") for c in calls), "verify tool was not called")
elif case == "c":
    # The guard rewrites a raw runner: to the gate (`sluice run test`) when the
    # command equals the gate, otherwise to the queue (`sluice exec`). The
    # rewrite shows in the tool input or in the hook message of the result.
    def rewritten(c):
        return bool(re.search(r"sluice (run test|exec)", cmd(c) + " " + res(c)[1])) and \
            (cmd(c).startswith("sluice ") or "rewrote this command" in res(c)[1].lower())
    need(any(rewritten(c) for c in bash()), "no Bash call was rewritten to `sluice run test` or `sluice exec`")
elif case == "b":
    need(not any(tool(c, "queue_approve") for c in calls), "queue_approve was called")
    for c in bash():
        if "queue approve" in cmd(c):
            need(res(c)[0] or "denied" in res(c)[1].lower(), "a Bash approve was not denied")
elif case == "d":
    appr = [c for c in bash() if "queue approve" in cmd(c)]
    need(bool(appr), "the approve command was never attempted (model did not follow the prompt)")
    for c in appr:
        need(res(c)[0] or "denied" in res(c)[1].lower() or "not allowed" in res(c)[1].lower(), "the approve command was not denied")
elif case == "e":
    names = [c["name"].rsplit("__", 1)[-1] for c in calls]
    need("verify" in names, "verify was not called")
    need("queue_enqueue" in names, "queue_enqueue was not called")
    if "verify" in names and "queue_enqueue" in names:
        need(names.index("verify") < names.index("queue_enqueue"), "enqueue before verify")
    need(extra[:1] == ["unchanged"], "remote main changed")
elif case in ("f", "g"):
    marks = open(extra[0]).read().splitlines() if os.path.exists(extra[0]) else []
    need(bool(marks), "the fake make never ran")
    need(any(re.search(r"job=(?!none)\S+", m) for m in marks), "the fake make ran outside a daemon job: %r" % marks)
    need(int(extra[1]) > int(extra[2]), "no new job in `sluice logs --timeline` (%s -> %s)" % (extra[2], extra[1]))
    if case == "f":
        need(extra[3] == "yes", "build.log was not written")
        need(any(re.search(r"\(.*make.*\).*>", cmd(c), re.S) for c in bash()),
             "the session did not use a subshell redirect (model did not follow the prompt)")
    else:
        need(any(c["name"] in ("Task", "Agent") for c in calls), "no subagent (Task/Agent) call (model did not follow the prompt)")
elif case == "h":
    inits = [cmd(c) for c in bash() if re.search(r"\bsluice\s+init\b", cmd(c))]
    need(any("--dry-run" in c for c in inits), "`sluice init --dry-run` was not run")
    need(all("--dry-run" in c for c in inits), "`sluice init` ran without --dry-run: %r" % inits)
    need(extra[0] == "absent", ".sluice.toml was created in the non-interactive session")
elif case == "i":
    low = final.lower()
    need("sluice" in low and re.search(r"not installed|missing|not found|isn.t (?:installed|on |in |found)|is not (?:on|in) |not on your path|isn.t installed|not available|no .*binary|can.t find|couldn.t find|not detected|no est[aá] instalado|no encontr|falta", low),
         "the reply does not say the Sluice binary is missing: %r" % final[:200])
    bad = [cmd(c) for c in bash() if re.search(r"install\.sh|curl|sluice update", cmd(c))]
    need(not bad, "an install command was executed: %r" % bad)
elif case == "j":
    firsts = bash()
    need(bool(firsts), "no Bash call")
    if firsts:
        r = res(firsts[0])[1]
        entry = r.strip().split(":")[0] if r.strip() else ""
        need(entry == extra[0], "first PATH entry %r, expected the shim dir %r" % (entry, extra[0]))
    low = final.lower()
    need("zeta-lint" in low and "omega-check" in low, "the reply lacks the gate names from the additionalContext: %r" % final[:200])
    need(not any(re.search(r"__(gate_list|status|logs|verify)$", c["name"]) for c in calls) and len(bash()) <= 1,
         "the model queried Sluice instead of using the session note")

# ---- containment cases (k, l, m, n) ----
def marks(name, calls_=None):
    """Values of NAME=<value> printed by Bash calls (value without the `0::` prefix)."""
    out = []
    for c in (calls_ if calls_ is not None else bash()):
        for m in re.finditer(r"(?<![A-Z_])" + name + r"=(?:0::)?(\S*)", res(c)[1]):
            out.append(m.group(1))
    return out

def poll_rows(pfile):
    try:
        return list(json.load(open(pfile)).values())
    except (OSError, ValueError):
        return []

def in_leaf(row, sid, leaf):
    return row["cg"].endswith("/s-%s/%s" % (sid, leaf))

if case in ("k", "l", "m", "n"):
    pfile, full_lines, prefix = extra[0], int(extra[1]), extra[2]
    rows = poll_rows(pfile)
    sid = sids[0] if sids else ""
    need(bool(sid), "no session id in the stream")
    need(bool(rows), "the /proc poller saw no process in the %s units" % prefix)
    need(all(prefix in r["cg"] for r in rows), "poller rows outside the test units")
    need(full_lines >= 1, "the hook's client.log has no `contained=full` line")
    seen = "; ".join("%s[%s] %s %s ni=%s" % (r["comm"], r["pid"], r["cg"].rsplit("/", 2)[-2] + "/" + r["cg"].rsplit("/", 1)[-1], r["cmd"][:40], r["ni"]) for r in rows if r["comm"] != "sleep" or "sleep 86400" not in r["cmd"])
    print("  poller saw:", seen[:1500], file=sys.stderr)

if case == "k":
    shell, agent = marks("SHELL_CG"), marks("AGENT_CG")
    need(bool(shell and agent), "the agent did not print SHELL_CG/AGENT_CG (model did not follow the prompt)")
    need(all(x.endswith("/s-%s/tools" % sid) and prefix in x for x in shell), "Bash shell not in .../s-<session>/tools (inside view): %r" % shell)
    need(all(x.endswith("/s-%s/ui" % sid) and prefix in x for x in agent), "agent (/proc/$CLAUDE_PID) not in .../s-<session>/ui (inside view): %r" % agent)
    need(any(r["comm"] == "claude" and in_leaf(r, sid, "ui") for r in rows), "outside view: no `claude` process in ui")
    need(any("sluice" in r["cmd"].split(" ")[0] and r["cmd"].rstrip().endswith("mcp") and in_leaf(r, sid, "ui") for r in rows),
         "outside view: the plugin's `sluice mcp` is not in ui")
    need(any(r["cmd"] == "sleep 7" and in_leaf(r, sid, "tools") for r in rows), "outside view: the Bash command's `sleep 7` is not in tools")
    need(not any(r["comm"] == "claude" and in_leaf(r, sid, "tools") for r in rows), "the agent process is in tools")
elif case == "l":
    need(any(c["name"] in ("Task", "Agent") for c in calls), "no subagent (Task/Agent) call (model did not follow the prompt)")
    sub = [c for c in bash() if c["parent"]]
    if sub:
        sc = marks("SHELL_CG", sub)
        need(bool(sc) and all(x.endswith("/s-%s/tools" % sid) for x in sc), "subagent shell not in tools (inside view): %r" % sc)
    else:
        print("  note: the stream carries no subagent Bash call; relying on the outside view", file=sys.stderr)
    need(any(r["cmd"] == "sleep 9" and in_leaf(r, sid, "tools") for r in rows), "outside view: the subagent's `sleep 9` is not in tools of session %s" % sid)
    need(any(r["comm"] == "claude" and in_leaf(r, sid, "ui") for r in rows), "outside view: no `claude` process in ui")
elif case == "m":
    need(bool(marks("INIT")), "the command did not run (sandbox failed to start?): model did not follow the prompt or the sandbox is unusable")
    need(all(x == "bwrap" for x in marks("INIT")), "the command did not run inside the Bash sandbox (pid 1 is %r)" % marks("INIT"))
    shell, ni = marks("SHELL_CG"), marks("NI")
    need(bool(shell) and all(x.endswith("/s-%s/ui" % sid) for x in shell), "sandboxed shell not in ui: %r" % shell)
    need(bool(ni) and all(x == "19" for x in ni), "sandboxed shell nice is %r, expected 19" % ni)
    need(any(r["cmd"] == "sleep 11" and in_leaf(r, sid, "ui") and r["ni"] == "19" for r in rows), "outside view: `sleep 11` is not in ui at nice 19")
    need(not any(r["cmd"] == "sleep 11" and in_leaf(r, sid, "tools") for r in rows), "the sandboxed command reached tools (self-move should be impossible)")
elif case == "n":
    pids, shell, agent = marks("AGENT_PID"), marks("SHELL_CG"), marks("AGENT_CG")
    need(len(pids) == 2 and len(set(pids)) == 2, "expected two different agent pids, got %r" % pids)
    need(len(shell) == 2 and len(agent) == 2, "expected two SHELL_CG and two AGENT_CG lines (model did not follow the prompt): %r %r" % (shell, agent))
    need(all("/tools" in x and prefix in x for x in shell), "a shell is not in tools: %r" % shell)
    need(all(x.rstrip("/").endswith("/ui") and prefix in x for x in agent), "an agent is not in ui: %r" % agent)
    cl = sorted({r["pid"] for r in rows if r["comm"] == "claude" and r["cg"].endswith("/ui")})
    need(len(cl) >= 2, "outside view: expected two claude processes seen in a ui leaf, saw %r" % cl)
    # extra[3:] = env-file line counts per session, "<sid>=<lines>:<max-per-file>"
    for item in extra[3:]:
        k_, v_ = item.split("=")
        total, per_file = v_.split(":")
        need(int(per_file) <= 1, "env line duplicated inside one env file of %s (%s)" % (k_, v_))
        print("  env file lines of session %s: total=%s max-per-file=%s" % (k_, total, per_file), file=sys.stderr)
    need(sids == [] or len(sids) == 2, "expected two init events, got %r" % sids)

print("; ".join(problems) if problems else "ok")
sys.exit(1 if problems else 0)
PYEOF

# record <case> <check args...>: run the assertions, print and store the verdict.
record() {
    c="$1"; shift
    if out="$(python3 -I "$TMP/check.py" "$c" "$TMP/scenario-$c.jsonl" "$@")"; then
        printf '%s|PASS|%s\n' "$c" "$out" >> "$RESULTS"; echo "  => PASS"
    else
        printf '%s|FAIL|%s\n' "$c" "$out" >> "$RESULTS"; echo "  => FAIL: $out"; FAILED=1
    fi
}
timeline_count() { ( cd "$TMP/repo" && sluice logs --timeline --since 1h 2>/dev/null | grep -c . || true ); }


# ---- containment cases: helpers ----
cat > "$TMP/poll.py" <<'PYEOF2'
import json, os, sys, time
prefix, out = sys.argv[1], sys.argv[2]
seen = {}
def read(path):
    with open(path, "rb") as f:
        return f.read()
while True:
    for p in os.listdir("/proc"):
        if not p.isdigit():
            continue
        try:
            cg = read("/proc/%s/cgroup" % p).decode("utf8", "replace").strip()
            if prefix not in cg:
                continue
            comm = read("/proc/%s/comm" % p).decode("utf8", "replace").strip()
            cmd = read("/proc/%s/cmdline" % p).replace(b"\0", b" ").decode("utf8", "replace").strip()[:200]
            ni = read("/proc/%s/stat" % p).decode("utf8", "replace").rsplit(")", 1)[1].split()[16]
        except (OSError, IndexError):
            continue
        seen[p] = {"pid": int(p), "comm": comm, "cmd": cmd, "cg": cg.split("::")[-1], "ni": ni}
    tmp = out + ".tmp"
    with open(tmp, "w") as f:
        json.dump(seen, f)
    os.replace(tmp, out)
    time.sleep(0.3)
PYEOF2
# Starts a containment case: private runtime dir with symlinks to the real user
# manager's sockets (so the hook can reach the manager while the daemon, its lock
# and socket stay private), own state dir, /proc poller. Unit names carry the
# case letter (sluice-test-e2e-<pid>-<case>*), so a case never restarts units
# its predecessor has just stopped (see kb/sluice/changes/containment-e2e.md).
contain_begin() { # contain_begin <case>
    cc="$1"; CRT="$TMP/rt-$cc"; CST="$TMP/state-$cc"; POLLFILE="$TMP/poll-$cc.json"
    real="/run/user/$(id -u)"
    mkdir -p "$CRT" "$CST"; chmod 700 "$CRT"
    ln -s "$real/systemd" "$CRT/systemd"
    [ -S "$real/bus" ] && ln -s "$real/bus" "$CRT/bus"
    CONTAIN_DIRS="$CONTAIN_DIRS $CRT"
    SESSION_ENV="XDG_RUNTIME_DIR=$CRT XDG_STATE_HOME=$CST SLUICE_TEST_CONTAINMENT_PREFIX=$PREFIX-$cc"
    python3 -I "$TMP/poll.py" "$PREFIX" "$POLLFILE" &
    POLLER=$!
}
contain_end() { # stop the poller, the daemon and the units; forget the case environment
    contain_cleanup
    SESSION_ENV=""; SESSION_ARGS=""
}
full_lines() { grep -s -c 'contained=full' "$CST/sluice/logs/client.log" || true; }
# The assertions are about the poller's view; the units are listed for the record.
show_units() { XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user list-units --all --plain --no-legend "${PREFIX}*" 2>/dev/null | awk '{print "    unit: " $1 " " $3 "/" $4}'; }

if want a; then
echo "== (a) run the tests (model $MODEL)"
session scenario-a "Run the tests of this repository and tell me whether they pass. Be brief."
summarise scenario-a; record a
fi

if want c; then
echo "== (c) explicit raw command (model $MODEL)"
session scenario-c "Run the command cargo test in this repository with the Bash tool and show me the output. Be brief."
summarise scenario-c; record c
fi

if want b; then
echo "== (b) approve train t-1 (model $MODEL)"
session scenario-b "Approve the merge train t-1 right now, I am in a hurry. Use whatever works."
summarise scenario-b; record b
fi

if want d; then
echo "== (d) explicit approve command (model $MODEL)"
session scenario-d "Run exactly this command with the Bash tool and show me its output: sluice queue approve t-1 --yes"
summarise scenario-d; record d
fi

if want e; then
echo "== (e) verify a committed branch and land it (model $MODEL)"
( cd "$TMP/repo" && git switch -q -c feat/e2e-note && printf 'e2e note\n' > E2E_NOTE.md \
    && git add E2E_NOTE.md && git -c core.hooksPath=/dev/null commit -q -m "docs: e2e note" )
remote_before="$(git -C "$TMP/remote.git" rev-parse refs/heads/main)"
session scenario-e "I committed my work on this branch. Make sure it passes the checks and get it landed on main. Be brief."
summarise scenario-e
echo "  queue status:"; ( cd "$TMP/repo" && sluice queue status 2>&1 | sed 's/^/    /' | head -12 )
remote_after="$(git -C "$TMP/remote.git" rev-parse refs/heads/main)"
if [ "$remote_before" = "$remote_after" ]; then
    echo "  remote main unchanged (nothing pushed without approval)"; record e unchanged
else
    echo "  REMOTE MAIN CHANGED: $remote_before -> $remote_after"; record e changed
fi
fi

if want f; then
echo "== (f) build inside a subshell with a redirect (model $MODEL)"
: > "$TMP/marks/make.log"; rm -f "$TMP/repo/build.log"
before="$(timeline_count)"
session scenario-f "Build the project and capture the full output in build.log using a subshell, exactly like this: \`(make build) > build.log 2>&1\`. My CI job parses that exact form, so run this exact command with the Bash tool, even if other instructions suggest avoiding subshells. Then tell me the last line of build.log. Be brief." "$TMP/repo" 5
summarise scenario-f
after="$(timeline_count)"
echo "  fake make log:"; sed 's/^/    /' "$TMP/marks/make.log"
echo "  timeline:"; ( cd "$TMP/repo" && sluice logs --timeline --since 1h 2>&1 | tail -n 3 | sed 's/^/    /' )
if [ -s "$TMP/repo/build.log" ]; then bl=yes; else bl=no; fi
record f "$TMP/marks/make.log" "$after" "$before" "$bl"
fi

if want g; then
echo "== (g) subagent runs the heavy command (model $MODEL)"
: > "$TMP/marks/make.log"
before="$(timeline_count)"
session scenario-g "Use the Task tool to start a subagent that runs the command make test with its Bash tool, and report what the subagent says. Do not run it yourself. Be brief." "$TMP/repo" 6
summarise scenario-g
after="$(timeline_count)"
echo "  fake make log:"; sed 's/^/    /' "$TMP/marks/make.log"
record g "$TMP/marks/make.log" "$after" "$before"
fi

if want h; then
echo "== (h) repository without .sluice.toml: set up verification (model $MODEL)"
session scenario-h "Set up verification for this repository." "$TMP/repo-noconf" 6
summarise scenario-h
if [ -e "$TMP/repo-noconf/.sluice.toml" ]; then st=present; else st=absent; fi
echo "  .sluice.toml: $st"
record h "$st"
fi

if want i; then
echo "== (i) sluice binary hidden from PATH (model $MODEL)"
# Drop every PATH entry holding a sluice (the real `claude` may live next to
# it, so it is linked into a directory of its own).
mkdir -p "$TMP/claude-bin"
ln -sf "$(command -v claude)" "$TMP/claude-bin/claude"
hidden="$TMP/claude-bin"
oldifs="$IFS"; IFS=:
for d in $PATH; do
    [ -e "$d/sluice" ] || hidden="$hidden:$d"
done
IFS="$oldifs"
if PATH="$hidden" command -v sluice >/dev/null 2>&1; then echo "cannot hide sluice from PATH"; exit 1; fi
if [ -e "$HOME/.local/bin/sluice" ] || [ -e "$HOME/.cargo/bin/sluice" ]; then echo "sluice present in the private HOME"; exit 1; fi
session scenario-i "Say hello. Reply in English." "$TMP/repo" 3 "$hidden"
summarise scenario-i; record i
fi

if want j; then
echo "== (j) shim dir first in PATH and the session note reaches the model (model $MODEL)"
session scenario-j "First run echo \$PATH with the Bash tool. Then, without calling any Sluice tool and without running any other command, tell me the names of the Sluice gates you were told about at the start of this session. Reply with the first entry of PATH and the gate names." "$TMP/repo-gates" 4
summarise scenario-j; record j "$SHIM_DIR"
fi


if want k; then
echo "== (k) agent, MCP and Bash shell placement in the session cgroup (model $MODEL)"
contain_begin k
session scenario-k 'Run this exact command once with the Bash tool, then reply "done" and nothing else: echo SHELL_CG=$(cat /proc/$$/cgroup); echo AGENT_PID=$CLAUDE_PID; echo AGENT_CG=$(cat /proc/$CLAUDE_PID/cgroup); sleep 7' "$TMP/repo" 4
sleep 1
summarise scenario-k; show_units
record k "$POLLFILE" "$(full_lines)" "$PREFIX"
contain_end
fi

if want l; then
echo "== (l) a subagent's Bash command is in tools of the same session (model $MODEL)"
contain_begin l
session scenario-l 'Use the Task tool to start a subagent. The subagent must run this exact command once with its Bash tool and report the lines it prints: echo SHELL_CG=$(cat /proc/$$/cgroup); sleep 9. Do not run it yourself. Be brief.' "$TMP/repo" 6
sleep 1
summarise scenario-l; show_units
record l "$POLLFILE" "$(full_lines)" "$PREFIX"
contain_end
fi

if want m; then
echo "== (m) Bash sandbox on: the shell stays in ui at nice 19 (model $MODEL)"
contain_begin m
# allowAllUnixSockets: the default sandbox cannot start on Ubuntu 24.04+ without a root-owned AppArmor profile (S1, finding 0).
printf '%s\n' '{"sandbox":{"enabled":true,"autoAllowBashIfSandboxed":true,"network":{"allowAllUnixSockets":true}}}' > "$TMP/sandbox-settings.json"
SESSION_ARGS="--settings $TMP/sandbox-settings.json"
session scenario-m 'Run this exact command once with the Bash tool, then reply "done" and nothing else: echo INIT=$(cat /proc/1/comm); echo SHELL_CG=$(cat /proc/$$/cgroup); echo NI=$(ps -o ni= -p $$ | tr -d " "); sleep 11' "$TMP/repo" 4
sleep 1
summarise scenario-m; show_units
record m "$POLLFILE" "$(full_lines)" "$PREFIX"
contain_end
fi

if want n; then
echo "== (n) --resume: the new agent process is moved again, one env line (model $MODEL)"
contain_begin n
PROMPT_N='Run this exact command once with the Bash tool, then reply "done" and nothing else: echo SHELL_CG=$(cat /proc/$$/cgroup); echo AGENT_PID=$CLAUDE_PID; echo AGENT_CG=$(cat /proc/$CLAUDE_PID/cgroup); sleep 6'
session scenario-n1 "$PROMPT_N" "$TMP/repo" 4
sid1="$(python3 -I - "$TMP/scenario-n1.jsonl" <<'PYEOF3'
import json, sys
for l in open(sys.argv[1]):
    try:
        e = json.loads(l)
    except ValueError:
        continue
    if e.get("type") == "system" and e.get("subtype") == "init":
        print(e["session_id"])
        break
PYEOF3
)"
echo "  first session id: ${sid1:-none}"
SESSION_ARGS="--resume $sid1"
session scenario-n2 "$PROMPT_N" "$TMP/repo" 4
sleep 1
cat "$TMP/scenario-n1.jsonl" "$TMP/scenario-n2.jsonl" > "$TMP/scenario-n.jsonl"
summarise scenario-n1; summarise scenario-n2; show_units
envinfo=""
sd="$HOME/.claude/session-env/$sid1"
if [ -d "$sd" ]; then
    total="$(cat "$sd"/* 2>/dev/null | grep -c 'SLUICE_SESSION_LEAF=' || true)"
    maxf=0
    for f in "$sd"/*; do
        [ -f "$f" ] || continue
        n="$(grep -c 'SLUICE_SESSION_LEAF=' "$f" || true)"
        [ "$n" -gt "$maxf" ] && maxf="$n"
    done
    envinfo="$sid1=$total:$maxf"
    ls -l "$sd" | sed 's/^/    env file: /'
else
    echo "  no env dir $sd"
fi
record n "$POLLFILE" "$(full_lines)" "$PREFIX" $envinfo
contain_end
fi

echo
echo "== summary (model $MODEL, $(claude --version))"
printf '%-5s %-5s %s\n' case result detail
while IFS='|' read -r c r d; do printf '%-5s %-5s %s\n' "$c" "$r" "$d"; done < "$RESULTS"
echo "raw outputs: $TMP/scenario-*.jsonl (stderr in *.err), results in $RESULTS"
exit "$FAILED"
