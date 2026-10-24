"""Scenario 9: agent containment end to end, without AI sessions.

Proves the cgroup containment of `kb/sluice/plan/containment-plan.md` (ADR-0017)
with the release binary, a sandbox HOME/XDG tree and a throw-away unit tree
`sluice-test-e1_<pid>.slice` on the real systemd user manager. The "agent" is a
`sh` with a `sleep` child (its pid is `CLAUDE_PID`); agent shells carry
`CLAUDECODE=1`, are started from a parent that sits in the session's `ui` leaf
(as Claude Code's Bash tool is a child of the agent) and source the session's
`CLAUDE_ENV_FILE` first.

  1. Session start: `plugin/hooks/session-start.sh` with hook JSON and a temp
     env file (twice: startup and resume). The fake agent and its child are in
     `ui`; the env file holds the cgroup line once; a sourced agent shell is in
     `tools`.
  2. Escapes interception cannot see, each from its own agent shell: a script
     file, a tool by absolute path (copy of `sh`), `python3 -c
     subprocess.run`, a background command, `setsid nohup`. Every busy loop
     ends up in the session's `tools` leaf (`/proc/<pid>/cgroup`).
  3. CPU behaviour with the measured weights (S2 method): a latency probe in
     `ui`, busy loops in `tools` (2x the CPU count, then 1x), a probe outside
     as the desktop; bound of `containment-s2-weights.md`: ui p99 < 5 ms and
     desktop p99 < 2 ms at 2x. Then 1x loops in a gate job (daemon scope) against
     1x loops of the agents scope: the gates scope's CPU share must win.
  4. Sandbox-like fallback: the env line pointed at a read-only directory
     leaves the shell in `ui` at nice 19, silently.
  5. Fail open: `[containment] enabled = false` moves nothing, adds no line,
     creates no unit, and commands and gates still run.

SAFETY (hard rules of the plan): CPU load only, busy loops each under
`timeout`, rounds of at most 15 s, a few minutes of load in total; no memory
limit, no memory pressure, no memory hog; only processes started here are
moved or killed; every unit carries the `sluice-test-` prefix passed through
`SLUICE_TEST_CONTAINMENT_PREFIX`, so `sluice.slice`, `sluice-daemon.scope` and
`sluice-agents.scope` of the machine are never touched; everything is stopped
in a `finally`; the run aborts when the desktop probe sees a stall above 1 s.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import stat
import subprocess as sp
import sys
import threading
import time
from pathlib import Path

from lib import REPO_ROOT, Report, Sandbox, wait_until

ROUND_SECONDS = 15  # hard cap of a load round
PROBE_SECONDS = 12  # samples are taken while every loop is surely running
PERIOD = 0.010  # probe wake-up period
UI_P99_BOUND_US = 5000
DESKTOP_P99_BOUND_US = 2000
ABORT_US = 1_000_000  # desktop stall that aborts the run
SESSION = "alpha-s9"
NCPU = os.cpu_count() or 1

ABORT = threading.Event()
SPAWNED: list[sp.Popen] = []

PROBE_PY = r'''
import json, os, sys, time
cg, seconds, period = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
if cg != "-":
    with open(cg + "/cgroup.procs", "w") as f:
        f.write(str(os.getpid()))
lat = []
t0 = time.perf_counter()
nxt = t0 + period
last = t0
worst = 0.0
while True:
    now = time.perf_counter()
    if now - t0 >= seconds:
        break
    if nxt > now:
        time.sleep(nxt - now)
    woke = time.perf_counter()
    d = woke - nxt
    lat.append(d)
    worst = max(worst, d)
    while time.perf_counter() - woke < 0.001:
        pass
    nxt += period
    if time.perf_counter() - last >= 0.5:
        last = time.perf_counter()
        print(json.dumps({"max_us": int(worst * 1e6)}), flush=True)
print(json.dumps({"lat_us": [int(x * 1e6) for x in lat]}), flush=True)
'''

BUSY = "while :; do :; done"


def build_binary() -> None:
    if os.environ.get("SLUICE_BIN"):
        return
    print("== cargo build --release -p sluice-gate", flush=True)
    sp.run(["cargo", "build", "--release", "-p", "sluice-gate"], cwd=REPO_ROOT, check=True)
    os.environ["SLUICE_BIN"] = str(REPO_ROOT / "target" / "release" / "sluice")


# -- small helpers ---------------------------------------------------------------
def cg_of(pid: int) -> str:
    try:
        return Path(f"/proc/{pid}/cgroup").read_text().strip().removeprefix("0::")
    except OSError:
        return ""


def procs_of(d: Path) -> list[int]:
    try:
        return [int(x) for x in (d / "cgroup.procs").read_text().split()]
    except (OSError, ValueError):
        return []


def tree_procs(d: Path) -> list[int]:
    """Every pid in `d` and below (only cgroups of the test tree are passed)."""
    out: list[int] = []
    if not d.is_dir():
        return out
    out += procs_of(d)
    for sub in d.iterdir():
        if sub.is_dir():
            out += tree_procs(sub)
    return out


def cpu_busy_usec() -> int:
    """Machine-wide busy CPU time (all CPUs, excluding idle and iowait) in microseconds."""
    f = Path("/proc/stat").read_text().splitlines()[0].split()[1:]
    v = [int(x) for x in f]
    busy = sum(v) - v[3] - (v[4] if len(v) > 4 else 0)
    return busy * 10_000  # USER_HZ 100


def usage_usec(d: Path) -> int:
    for line in (d / "cpu.stat").read_text().splitlines():
        if line.startswith("usage_usec"):
            return int(line.split()[1])
    return 0


def pct(sorted_us: list[int], p: float) -> int:
    if not sorted_us:
        return -1
    return sorted_us[min(len(sorted_us) - 1, int(len(sorted_us) * p))]


def stats(lat: list[int]) -> dict:
    s = sorted(lat)
    worst_at = round(lat.index(s[-1]) * PERIOD, 1) if lat else -1
    # A single stall of D ms delays about D/10 consecutive samples (the probe's schedule catches
    # up), so p99 of a 12 s window is decided by one stall: keep the count and the position.
    return {"n": len(s), "p50": pct(s, 0.50), "p95": pct(s, 0.95), "p99": pct(s, 0.99), "max": s[-1] if s else -1,
            "over_10ms": sum(1 for x in lat if x > 10_000), "worst_at_s": worst_at}


def fmt(st: dict) -> str:
    return (f"p50 {st['p50']}us p95 {st['p95']}us p99 {st['p99']}us max {st['max']}us at {st['worst_at_s']}s "
            f"(n={st['n']}, {st['over_10ms']} over 10ms)")


def units(prefix: str) -> list[str]:
    cp = sp.run(["systemctl", "--user", "list-units", "--all", "--no-legend", "--plain", f"{prefix}*"],
                capture_output=True, text=True, timeout=30)
    return [ln.split()[0] for ln in cp.stdout.splitlines() if ln.strip()]


def machine() -> dict:
    cpu = ""
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("model name"):
            cpu = line.split(":", 1)[1].strip()
            break
    mem = ""
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal"):
            mem = f"{int(line.split()[1]) // 1024} MB"
    sd = sp.run(["systemctl", "--version"], capture_output=True, text=True).stdout.splitlines()[:1]
    uid = os.getuid()
    mgr = Path(f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service")
    weights = {}
    for name in ("app.slice", "session.slice", "background.slice"):
        try:
            weights[name] = (mgr / name / "cpu.weight").read_text().strip()
        except OSError:
            pass
    return {"cpus": NCPU, "cpu_model": cpu, "mem": mem, "kernel": os.uname().release,
            "systemd": sd[0] if sd else "", "loadavg": Path("/proc/loadavg").read_text().split()[:3],
            "desktop_slice_cpu_weights": weights}


# -- the scenario world ----------------------------------------------------------
class World:
    def __init__(self, sb: Sandbox, prefix: str, tag: str) -> None:
        self.sb = sb
        self.prefix = prefix
        self.tag = tag
        self.real_runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
        self.repo = sb.root / "proj"
        self.work = sb.root / "work"
        self.envfile = sb.root / "claude-env.sh"
        self.hook = REPO_ROOT / "plugin" / "hooks" / "session-start.sh"
        self.probe = sb.root / "probe.py"
        self.work.mkdir()
        self.probe.write_text(PROBE_PY)
        self.agents: list[sp.Popen] = []

    # environment of every process of this scenario
    def env(self, extra: dict | None = None, agent: bool = True) -> dict:
        e = self.sb.env(agent=agent, extra={"SLUICE_TEST_CONTAINMENT_PREFIX": self.prefix})
        if extra:
            e.update(extra)
        return e

    def make_repo(self, gates: str) -> None:
        self.repo.mkdir()
        (self.repo / ".sluice.toml").write_text("version = 1\n" + gates)
        (self.repo / "README.md").write_text("fixture\n")
        sb = self.sb
        sb.git(self.repo, "init", "-q", "-b", "main")
        sb.git(self.repo, "config", "commit.gpgsign", "false")
        sb.git(self.repo, "add", "-A")
        sb.git(self.repo, "commit", "-q", "-m", "fixture")

    def start_daemon(self) -> bool:
        cp = self.sb.run(["sluice", "daemon"], self.repo,
                         extra={"SLUICE_TEST_CONTAINMENT_PREFIX": self.prefix,
                                "SLUICE_TEST_MANAGER_RUNTIME_DIR": self.real_runtime}, timeout=60)
        return cp.returncode == 0 and wait_until(lambda: self.sb.daemon_pid(self.repo) is not None, 15)

    def fake_agent(self) -> tuple[sp.Popen, int]:
        p = sp.Popen(["sh", "-c", "sleep 600 & wait"], stdin=sp.DEVNULL, stdout=sp.DEVNULL, stderr=sp.DEVNULL,
                     start_new_session=True)
        SPAWNED.append(p)
        kids = Path(f"/proc/{p.pid}/task/{p.pid}/children")
        wait_until(lambda: kids.exists() and kids.read_text().strip() != "", 5)
        return p, int(kids.read_text().split()[0])

    def session_start(self, agent_pid: int, source: str) -> sp.CompletedProcess:
        hook_json = json.dumps({"session_id": SESSION, "transcript_path": "/dev/null", "cwd": str(self.repo),
                                "hook_event_name": "SessionStart", "source": source})
        env = self.env({"CLAUDE_ENV_FILE": str(self.envfile), "CLAUDE_PID": str(agent_pid),
                        "XDG_RUNTIME_DIR": self.real_runtime})
        return sp.run(["sh", str(self.hook)], input=hook_json, cwd=self.repo, env=env, capture_output=True,
                      text=True, timeout=30)

    def leaf_dirs(self) -> tuple[Path, Path] | None:
        """(ui, tools) of the session, read from the env-file line."""
        try:
            text = self.envfile.read_text()
        except OSError:
            return None
        m = re.search(r"SLUICE_SESSION_LEAF='([^']+)'", text)
        if not m:
            return None
        tools = Path(m.group(1))
        return tools.parent / "ui", tools

    def agent_shell(self, cmd: str, ui: Path | None, envfile: Path | None = None, capture: bool = False) -> sp.Popen:
        """A shell as Claude's Bash tool starts it: a child of the agent (so in
        `ui`), with CLAUDECODE=1, sourcing the env file before the command."""
        body = f'. "{envfile or self.envfile}"\n{cmd}'
        p = sp.Popen(["sh", "-c", 'read x; exec bash -c "$0"', body], cwd=self.work, env=self.env(),
                     stdin=sp.PIPE, stdout=sp.PIPE if capture else sp.DEVNULL,
                     stderr=sp.PIPE if capture else sp.DEVNULL, text=True, start_new_session=True)
        SPAWNED.append(p)
        if ui is not None:
            try:
                (ui / "cgroup.procs").write_text(str(p.pid))
            except OSError:
                pass
        assert p.stdin is not None
        p.stdin.write("\n")
        p.stdin.close()
        return p

    def run_probe(self, cg: Path | None, seconds: float, sink: dict, key: str) -> threading.Thread:
        """A latency probe (optionally placed in `cg` by itself); result in sink[key]."""
        proc = sp.Popen(["python3", str(self.probe), str(cg) if cg else "-", str(seconds), str(PERIOD)],
                        stdout=sp.PIPE, stderr=sp.DEVNULL, text=True, start_new_session=True)
        SPAWNED.append(proc)

        def read() -> None:
            assert proc.stdout is not None
            for line in proc.stdout:
                try:
                    msg = json.loads(line)
                except ValueError:
                    continue
                if "max_us" in msg and cg is None and msg["max_us"] > ABORT_US:
                    ABORT.set()
                if "lat_us" in msg:
                    sink[key] = msg["lat_us"]
            proc.wait()

        t = threading.Thread(target=read, daemon=True)
        t.start()
        return t


def kill_tree(prefix: str) -> None:
    """SIGKILL every process in the cgroups of the test tree (only ours: the prefix is unique)."""
    uid = os.getuid()
    mgr = Path(f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service")
    for pid in set(find_prefix_procs(mgr, prefix)):
        try:
            os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass


def find_prefix_procs(d: Path, prefix: str, depth: int = 0) -> list[int]:
    out: list[int] = []
    try:
        entries = list(d.iterdir())
    except OSError:
        return out
    for e in entries:
        if not e.is_dir():
            continue
        if e.name.startswith(prefix) and (e.name.endswith(".slice") or e.name.endswith(".scope")):
            out += tree_procs(e)
        elif depth < 6 and e.name.endswith(".slice") and prefix.startswith(e.name.removesuffix(".slice") + "-"):
            # the test slice nests under implicit parents (sluice.slice, sluice-test.slice, ...):
            # only descend into them, never touch them
            out += find_prefix_procs(e, prefix, depth + 1)
    return out


# -- phases ----------------------------------------------------------------------
def phase_session(rep: Report, w: World, nums: dict) -> tuple[Path, Path] | None:
    agent, child = w.fake_agent()
    before = cg_of(agent.pid)
    cp1 = w.session_start(agent.pid, "startup")
    cp2 = w.session_start(agent.pid, "resume")
    leaves = w.leaf_dirs()
    rep.check("S1: session-start.sh exits 0 and prints a SessionStart context twice",
              cp1.returncode == 0 and cp2.returncode == 0 and '"hookSpecificOutput"' in cp1.stdout and '"hookSpecificOutput"' in cp2.stdout,
              f"rc {cp1.returncode}/{cp2.returncode}")
    if not leaves:
        rep.check("S1: the env file has the containment line", False, "no SLUICE_SESSION_LEAF line (containment off?)")
        return None
    ui, tools = leaves
    a, c = cg_of(agent.pid), cg_of(child)
    rep.check("S1: the fake agent (CLAUDE_PID) moved to the session's ui leaf",
              a.endswith(f"/s-{SESSION}/ui") and a != before, f"{a}")
    rep.check("S1: the agent's child is in ui too", c.endswith(f"/s-{SESSION}/ui"), f"{c}")
    text = w.envfile.read_text()
    once = sum(1 for ln in text.splitlines() if "SLUICE_SESSION_LEAF=" in ln)
    rep.check("S1: the env file holds the containment line exactly once after startup and resume", once == 1,
              f"{once} line(s) of {len(text.splitlines())}")
    agents_scope = tools.parent.parent
    weights = {
        "ui": (ui / "cpu.weight").read_text().strip(),
        "tools": (tools / "cpu.weight").read_text().strip(),
        "agents_scope": (agents_scope / "cpu.weight").read_text().strip() if (agents_scope / "cpu.weight").exists() else "?",
        "slice": (agents_scope.parent / "cpu.weight").read_text().strip(),
    }
    nums["weights"] = weights
    rep.check("S1: the tree carries the measured weights (ui 400, tools 25, agents scope 30)",
              weights["ui"] == "400" and weights["tools"] == "25" and weights["agents_scope"] == "30", json.dumps(weights))
    sh = w.agent_shell('cat /proc/self/cgroup; echo "SHELL_PID=$$"', ui, capture=True)
    out, _ = sh.communicate(timeout=20)
    rep.check("S1: a sourced agent shell lands in tools",
              f"/s-{SESSION}/tools" in out and sh.returncode == 0, out.strip().replace("\n", " | "))
    return ui, tools


def escape_cmds(w: World) -> tuple[dict[str, str], dict[str, Path]]:
    esc = w.work / "esc"
    esc.mkdir()
    (esc / "loop.sh").write_text('#!/bin/sh\necho $$ > "$1"\n' + BUSY + "\n")
    (esc / "loop.sh").chmod(0o755)
    shutil.copy(os.path.realpath(shutil.which("sh") or "/bin/sh"), esc / "tool-sh")
    (esc / "tool-sh").chmod(0o755)
    inner = f"echo $$ > \"$1\"; {BUSY}"
    pid = lambda n: str(esc / f"pid_{n}")  # noqa: E731
    return {
        "script file": f"timeout 5 {esc}/loop.sh {pid('script')}",
        "tool by absolute path": f"timeout 5 {esc}/tool-sh -c '{inner}' tool-sh {pid('abs')}",
        "python subprocess": ("python3 -c \"import subprocess,sys; subprocess.run(['timeout','5','sh','-c',"
                              f"sys.argv[2],'sh',sys.argv[1]])\" {pid('py')} '{inner}'"),
        "background command": f"timeout 5 sh -c '{inner}' sh {pid('bg')} & wait",
        "setsid nohup child": f"setsid nohup timeout 5 sh -c '{inner}' sh {pid('setsid')} >/dev/null 2>&1 &",
    }, {k: esc / f"pid_{n}" for k, n in (("script file", "script"), ("tool by absolute path", "abs"),
                                          ("python subprocess", "py"), ("background command", "bg"),
                                          ("setsid nohup child", "setsid"))}


def phase_escapes(rep: Report, w: World, ui: Path, tools: Path) -> None:
    cmds, pidfiles = escape_cmds(w)
    shells = {name: w.agent_shell(cmd, ui) for name, cmd in cmds.items()}
    found: dict[str, int] = {}
    wait_until(lambda: all(pf.exists() and pf.read_text().strip() for pf in pidfiles.values()), 10)
    for name, pf in pidfiles.items():
        try:
            found[name] = int(pf.read_text().strip())
        except (OSError, ValueError):
            pass
    # Read the cgroups while the loops still run (they stop after 5 s).
    for name in cmds:
        pid = found.get(name)
        where = cg_of(pid) if pid else "(no pid)"
        alive = bool(pid) and Path(f"/proc/{pid}").exists()
        rep.check(f"E: {name}: the busy loop runs in the session's tools leaf",
                  bool(pid) and alive and where.endswith(f"/s-{SESSION}/tools"), f"pid {pid} cgroup {where}")
    for p in shells.values():
        try:
            p.wait(timeout=15)
        except sp.TimeoutExpired:
            pass
    rep.check("E: every escape loop ended by its timeout", wait_until(lambda: not any(Path(f"/proc/{p}").exists() for p in found.values()), 10))


def start_loops(w: World, ui: Path, n: int, seconds: int) -> sp.Popen:
    cmd = f"for i in $(seq {n}); do timeout -k 1 {seconds} sh -c '{BUSY}' & done; wait"
    return w.agent_shell(cmd, ui)


def wait_quiet(slice_dir: Path, limit: float = 90.0, threshold: float = 8.0) -> float:
    """Wait (at most `limit` s) until processes outside the test tree use less than
    `threshold` % of the machine over a 2 s window; returns the last reading.
    A shared developer machine runs other agents' builds: the S2 numbers were
    taken on an otherwise idle machine, so the rounds wait for such a moment."""
    end = time.monotonic() + limit
    while True:
        b0, s0, w0 = cpu_busy_usec(), usage_usec(slice_dir), time.monotonic()
        time.sleep(2.0)
        b1, s1, w1 = cpu_busy_usec(), usage_usec(slice_dir), time.monotonic()
        other = max(0.0, (b1 - b0) - (s1 - s0)) / ((w1 - w0) * 1e6 * NCPU) * 100
        if other < threshold or time.monotonic() > end:
            return other


def latency_round(rep: Report, w: World, ui: Path, tools: Path, loops: int, label: str, nums: dict) -> dict:
    """One S2-style round: `loops` busy loops in tools, a probe in ui, a probe outside."""
    slice_dir = tools.parent.parent.parent
    quiet = wait_quiet(slice_dir)
    print(f"   round {label}: {loops} loops, outside load before: {quiet:.1f}% of the machine", flush=True)
    sink: dict = {}
    loopsh = start_loops(w, ui, loops, ROUND_SECONDS)
    placed = wait_until(lambda: len(procs_of(tools)) >= loops, 8)
    time.sleep(1.0)
    n_tools = len(procs_of(tools))
    b0, s0, w0 = cpu_busy_usec(), usage_usec(slice_dir), time.monotonic()
    t1 = w.run_probe(ui, PROBE_SECONDS, sink, "ui")
    t2 = w.run_probe(None, PROBE_SECONDS, sink, "desktop")
    for t in (t1, t2):
        while t.is_alive() and not ABORT.is_set():
            t.join(0.5)
    if ABORT.is_set():
        kill_tree(w.prefix)
        for p in SPAWNED:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        raise SystemExit("ABORT: desktop probe stall above 1 s, run stopped")
    b1, s1, w1 = cpu_busy_usec(), usage_usec(slice_dir), time.monotonic()
    other = max(0.0, (b1 - b0) - (s1 - s0)) / ((w1 - w0) * 1e6 * NCPU) * 100
    try:
        loopsh.wait(timeout=ROUND_SECONDS + 10)
    except sp.TimeoutExpired:
        kill_tree(w.prefix)
    wait_until(lambda: not procs_of(tools), 10)
    ui_st, dk_st = stats(sink.get("ui", [])), stats(sink.get("desktop", []))
    nums[label] = {"ui": ui_st, "desktop": dk_st, "loops_in_tools": n_tools, "other_cpu_pct_of_machine": round(other, 1)}
    rep.check(f"C: {label}: all {loops} loops are in tools while measuring", placed and n_tools >= loops,
              f"{n_tools} processes (sh + timeout per loop)")
    return nums[label]


def phase_cpu(rep: Report, w: World, ui: Path, tools: Path, nums: dict) -> None:
    sink: dict = {}
    t1 = w.run_probe(ui, 3, sink, "ui")
    t2 = w.run_probe(None, 3, sink, "desktop")
    t1.join(10)
    t2.join(10)
    nums["idle"] = {"ui": stats(sink.get("ui", [])), "desktop": stats(sink.get("desktop", []))}
    print(f"   idle: ui {fmt(nums['idle']['ui'])}; desktop {fmt(nums['idle']['desktop'])}", flush=True)

    def bounded(loops: int, name: str) -> tuple[dict, int]:
        tries = 0
        while True:
            tries += 1
            label = name if tries == 1 else f"{name} retry"
            r = latency_round(rep, w, ui, tools, loops, label, nums)
            print(f"   {label}: ui {fmt(r['ui'])}; desktop {fmt(r['desktop'])}; "
                  f"other processes used {r['other_cpu_pct_of_machine']}% of the machine", flush=True)
            if (r["ui"]["p99"] < UI_P99_BOUND_US and r["desktop"]["p99"] < DESKTOP_P99_BOUND_US) or tries == 3:
                return r, tries
            time.sleep(3)

    loops2 = 2 * NCPU
    last, n2 = bounded(loops2, "2x")
    rep.check(f"C: ui probe p99 < {UI_P99_BOUND_US // 1000} ms at 2x oversubscription ({loops2} loops on {NCPU} CPUs)",
              last["ui"]["p99"] < UI_P99_BOUND_US, fmt(last["ui"]), attempts=n2)
    rep.check(f"C: desktop probe p99 < {DESKTOP_P99_BOUND_US // 1000} ms at 2x oversubscription",
              last["desktop"]["p99"] < DESKTOP_P99_BOUND_US, fmt(last["desktop"]), attempts=n2)
    r1, n1 = bounded(NCPU, "1x")
    rep.check(f"C: ui and desktop p99 within the same bounds at 1x ({NCPU} loops)",
              r1["ui"]["p99"] < UI_P99_BOUND_US and r1["desktop"]["p99"] < DESKTOP_P99_BOUND_US,
              f"ui {r1['ui']['p99']}us desktop {r1['desktop']['p99']}us", attempts=n1)
    rep.check("C: sanity: the idle probe itself wakes within 2 ms at p99",
              nums["idle"]["ui"]["p99"] < 2000, f"idle ui p99 {nums['idle']['ui']['p99']}us")


def phase_gates(rep: Report, w: World, ui: Path, tools: Path, nums: dict) -> None:
    agents_scope = tools.parent.parent
    daemon_scope = agents_scope.parent / f"{w.prefix}-daemon.scope"
    n = NCPU
    sink: dict = {}
    print(f"   gates vs agents: {n} loops each, outside load before: {wait_quiet(tools.parent.parent.parent):.1f}%", flush=True)
    agent_loops = start_loops(w, ui, n, ROUND_SECONDS)
    gate = w.sb.popen(["sluice", "run", "burn"], w.repo, agent=False)
    SPAWNED.append(gate)
    jobs = daemon_scope / "jobs"
    ready = wait_until(lambda: len(procs_of(tools)) >= n and len(tree_procs(jobs)) >= n, 10)
    time.sleep(1.0)
    dk = w.run_probe(None, 9, sink, "desktop")
    d0, a0 = usage_usec(daemon_scope), usage_usec(agents_scope)
    time.sleep(8.0)
    d1, a1 = usage_usec(daemon_scope), usage_usec(agents_scope)
    tools_n, jobs_n = len(procs_of(tools)), len(tree_procs(jobs))
    still = tools_n >= n - 2 and jobs_n >= n - 2
    dk.join(10)
    if ABORT.is_set():
        kill_tree(w.prefix)
        raise SystemExit("ABORT: desktop probe stall above 1 s, run stopped")
    try:
        gate.communicate(timeout=ROUND_SECONDS + 20)
        agent_loops.wait(timeout=ROUND_SECONDS + 10)
    except sp.TimeoutExpired:
        kill_tree(w.prefix)
    dg, da = d1 - d0, a1 - a0
    share = dg / (dg + da) if dg + da else 0.0
    nums["gates_vs_agents"] = {"gates_cpu_ms": dg // 1000, "agents_cpu_ms": da // 1000, "gates_share": round(share, 3),
                               "ratio": round(dg / da, 2) if da else None, "loops_each": n,
                               "desktop": stats(sink.get("desktop", []))}
    rep.check("G: equal load in a gate job (jobs leaf) and in the agents tools leaf ran together for 8 s",
              ready and still, f"at the end of the window: jobs procs {jobs_n}, tools procs {tools_n}")
    rep.check("G: the gates scope gets more CPU than the agents scope (cpu.stat share)",
              dg > da, f"gates {dg // 1000} ms, agents {da // 1000} ms, share {share:.1%}, ratio {dg / da if da else 0:.2f}x "
                       f"(weights 100:30 predict 3.3x, S2 measured 2x)")
    wait_until(lambda: not tree_procs(jobs) and not procs_of(tools), 15)


def phase_readonly(rep: Report, w: World, ui: Path) -> None:
    """The env line pointed at a leaf that cannot be written: nice 19, stays in ui."""
    leaves = w.leaf_dirs()
    assert leaves
    _, tools = leaves
    line = next(ln for ln in w.envfile.read_text().splitlines() if "SLUICE_SESSION_LEAF=" in ln)
    ro = w.sb.root / "ro-leaf" / "tools"
    ro.mkdir(parents=True)
    (ro / "cgroup.procs").write_text("")
    (ro / "cgroup.procs").chmod(0o444)
    ro.chmod(0o555)
    fake = w.sb.root / "claude-env-ro.sh"
    fake.write_text(line.replace(str(tools), str(ro)) + "\n")
    sh = w.agent_shell('cat /proc/self/cgroup; ps -o ni= -p $$; echo done', ui, envfile=fake, capture=True)
    out, err = sh.communicate(timeout=20)
    lines = [ln.strip() for ln in out.splitlines()]
    ok = (sh.returncode == 0 and len(lines) == 3 and lines[0].endswith(f"/s-{SESSION}/ui")
          and lines[1] == "19" and lines[2] == "done" and not err.strip())
    rep.check("F: unwritable tools leaf: the shell stays in ui at nice 19, silently, and the command runs", ok,
              f"{lines} stderr={err.strip()!r}")
    ro.chmod(0o755)


def phase_off(rep: Report, sb: Sandbox, prefix: str, nums: dict) -> None:
    sb.write_user_config("[containment]\nenabled = false\n")
    w = World(sb, prefix, "off")
    marker = sb.root / "gate-ran"
    w.make_repo(f'[gates.hello]\ncmd = "echo hello > {marker}"\ncache = false\n')
    daemon_ok = w.start_daemon()
    agent, child = w.fake_agent()
    before = (cg_of(agent.pid), cg_of(child))
    cp = w.session_start(agent.pid, "startup")
    after = (cg_of(agent.pid), cg_of(child))
    env_text = w.envfile.read_text() if w.envfile.exists() else ""
    sh = w.agent_shell("echo cmd-ran; cat /proc/self/cgroup", None, capture=True)
    out, _ = sh.communicate(timeout=20)
    shell_cg = out.splitlines()[-1].removeprefix("0::") if out.strip() else ""
    gate = sb.run(["sluice", "run", "hello"], w.repo, timeout=60)
    leftovers = units(prefix)
    rep.check("O: enabled = false: the daemon starts and a gate runs through it", daemon_ok and gate.returncode == 0
              and marker.exists(), f"rc {gate.returncode} {gate.stdout.strip()[-200:]} {gate.stderr.strip()[-200:]}")
    rep.check("O: enabled = false: no systemd unit is created", not leftovers, f"{leftovers}")
    rep.check("O: enabled = false: the agent and its child do not move", cp.returncode == 0 and before == after,
              f"{before[0]} -> {after[0]}")
    rep.check("O: enabled = false: the env file has no cgroup or renice line",
              "SLUICE_SESSION_LEAF" not in env_text and "renice" not in env_text and "cgroup.procs" not in env_text)
    rep.check("O: enabled = false: an agent shell runs its command and stays where it started",
              sh.returncode == 0 and "cmd-ran" in out and shell_cg == cg_of(os.getpid()),
              f"shell cgroup {shell_cg}")
    sb.stop_daemon(w.repo)


def main() -> int:
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s9_containment", out)
    build_binary()
    pid = os.getpid()
    prefix = os.environ.get("SLUICE_TEST_CONTAINMENT_PREFIX") or f"sluice-test-e1_{pid}"
    if not prefix.startswith("sluice-test-"):
        raise SystemExit("SLUICE_TEST_CONTAINMENT_PREFIX must start with sluice-test-")
    prefix_off = prefix.replace("sluice-test-e1_", "sluice-test-e1off_") if "sluice-test-e1_" in prefix else prefix + "off"
    base = Path(os.environ["TMPDIR"]) if os.environ.get("TMPDIR") else None
    sb = Sandbox("s9", base=base)
    sb_off = Sandbox("s9off", base=base)
    nums: dict = {"machine": machine(), "prefix": prefix}
    print(f"== containment: prefix {prefix}, {NCPU} cpus, load {nums['machine']['loadavg']}", flush=True)
    started = time.monotonic()
    try:
        w = World(sb, prefix, "on")
        w.make_repo('[gates.burn]\ncmd = \'\'\'i=0; while [ $i -lt %d ]; do timeout -k 1 %d sh -c "while :; do :; done" & i=$((i+1)); done; wait\'\'\'\n'
                    'cache = false\ntimeout = "1m"\n' % (NCPU, ROUND_SECONDS))
        rep.check("daemon starts in its test tree", w.start_daemon(), f"prefix {prefix}")
        print("-- 1: session start", flush=True)
        leaves = phase_session(rep, w, nums)
        if leaves:
            ui, tools = leaves
            daemon_scope = tools.parent.parent.parent / f"{prefix}-daemon.scope"
            rep.check("daemon and agents scope exist under the test slice", daemon_scope.is_dir() and tools.parent.parent.is_dir(),
                      f"{daemon_scope.name}")
            print("-- 2: escapes", flush=True)
            phase_escapes(rep, w, ui, tools)
            print("-- 3: CPU behaviour", flush=True)
            phase_cpu(rep, w, ui, tools, nums)
            phase_gates(rep, w, ui, tools, nums)
            print("-- 4: read-only leaf", flush=True)
            phase_readonly(rep, w, ui)
        print("-- 5: fail open", flush=True)
        phase_off(rep, sb_off, prefix_off, nums)
    finally:
        for p in SPAWNED:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        for pr in (prefix, prefix_off):
            kill_tree(pr)
        for s in (sb, sb_off):
            s.close()
        for pr in (prefix, prefix_off):
            for u in units(pr):
                sp.run(["systemctl", "--user", "stop", u], capture_output=True, timeout=30)
        time.sleep(1.0)
        left_units = [u for pr in (prefix, prefix_off) for u in units(pr)]
        rep.check("nothing left: no sluice-test unit active, no process in the test cgroups", not left_units, f"{left_units}")
        nums["load_seconds_total"] = round(time.monotonic() - started)
        rep.save()
    print(f"s9_containment: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    print("numbers:", json.dumps(nums))
    (out / "s9_containment_numbers.json").write_text(json.dumps(nums, indent=2))
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
