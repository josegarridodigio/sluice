"""Scenario 4: chaos. The daemon, the clients, the machine and the disk are
abused while gates run; nothing may be reported as a pass that did not run to
completion, nothing may be left behind, and Sluice must keep answering.

Test bed: pallets/itsdangerous with a controllable gate (see pygate.py).
Agents are scripts (CLI calls and `sluice mcp` over stdio); no language model.

  1. SIGKILL the daemon while 6 gates run: no gate process (nor its
     descendants) survives, the next client call restarts the daemon in < 2 s,
     no stale lock, interrupted runs are failures for their clients.
  2. SIGKILL 10 clients mid-wait: `last_subscriber_gone = "cancel"` cancels
     their jobs and kills the processes; `"finish"` lets them finish and write
     markers.
  3. 64 concurrent clients hammer status/run/verify for 60 s: only the
     expected exit codes, bounded daemon RSS, stable fd count, no restart.
  4. Synthetic CPU pressure: new jobs wait (psi_blocked_ms_total grows) and
     resume when the load stops; the psi_max_wait guard admits a starving job.
  5. Unwritable storage (no root, so no tmpfs: directories made read-only):
     verdicts still returned, the error reported once per run, no crash,
     `doctor` reports it.
  6. Corrupt markers and a truncated SQLite WAL are ignored or recovered;
     `doctor` reports what it can see.

Knobs: ALPHA_TESTBEDS, ALPHA_PYLIB, ALPHA_OUT, TMPDIR, ALPHA_HAMMER_SECONDS
(default 60), ALPHA_HAMMER_CLIENTS (default 64).
"""

from __future__ import annotations

import json
import os
import random
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

from lib import Report, Sandbox, add_worktree, descendants, kill_quietly, wait_until
from mcp_agent import McpAgent
from pygate import (Cleanup, alive, counter_lines, fd_count, gate_pids, kill_all, require_testbeds, rss_kib, setup_repo,
                    sluice_daemon_pids)

T = f"alpha-s4-{os.getpid()}"
HAMMER_SECONDS = float(os.environ.get("ALPHA_HAMMER_SECONDS", "60"))
HAMMER_CLIENTS = int(os.environ.get("ALPHA_HAMMER_CLIENTS", "64"))
BASE = Path(os.environ["TMPDIR"]) if os.environ.get("TMPDIR") else None

clean = Cleanup()
clean.tokens.append(T)


def new_env(name: str, user_cfg: str, gates: dict) -> tuple[Sandbox, Path, Path]:
    sb = Sandbox(f"s4-{name}", base=BASE)
    clean.sandboxes.append(sb)
    if user_cfg:
        sb.write_user_config(user_cfg)
    repo, counter = setup_repo(sb, name, T, gates)
    return sb, repo, counter


def start_daemon(sb: Sandbox, repo: Path) -> int:
    sb.run(["sluice", "daemon"], repo)
    wait_until(lambda: sb.daemon_pid(repo) is not None, 10)
    pid = sb.daemon_pid(repo)
    assert pid, "daemon did not start"
    clean.extra_pids.add(pid)
    return pid


def trees(sb: Sandbox, repo: Path, n: int, prefix: str) -> list[Path]:
    out = []
    for i in range(n):
        wt = add_worktree(sb, repo, f"{prefix}{i}", f"alpha/{prefix}{i}")
        (wt / f"tree-{prefix}{i}.txt").write_text(f"tree {prefix}{i}\n")
        sb.git(wt, "add", "-A")
        out.append(wt)
    return out


def running_jobs(sb: Sandbox, repo: Path) -> int:
    return len(sb.status_json(repo).get("running", []))


def lookup(sb: Sandbox, repo: Path, gate: str) -> str:
    out = sb.run(["sluice", "cache", "status"], repo).stdout
    for line in out.splitlines():
        parts = line.split()
        if parts and parts[0] == gate:
            return parts[1]
    return "?"


def census(daemon_pid: int) -> set[int]:
    """Every process the daemon is responsible for: all its descendants plus
    every gate process by command line (they are the same set when it works)."""
    pids = set(descendants(daemon_pid))
    pids.update(gate_pids(T))
    return {p for p in pids if alive(p)}


def doctor(sb: Sandbox, repo: Path) -> tuple[int, dict]:
    cp = sb.run(["sluice", "doctor", "--json"], repo)
    try:
        data = json.loads(cp.stdout)
    except ValueError:
        data = {}
    return cp.returncode, {c["name"]: c for c in data.get("checks", [])}


# ---------------------------------------------------------------------------
def s1_daemon_sigkill(rep: Report) -> None:
    print("== 1. SIGKILL the daemon while 6 gates run", flush=True)
    sb, repo, counter = new_env("c1", "", {"long": {"hold": 25, "child": True}, "quick": {"hold": 0.1}})
    old = start_daemon(sb, repo)
    rep.check("1: sanity: the process census finds the daemon (the final leak check is not vacuous)", old in sluice_daemon_pids(sb), f"daemon {old}, census {sluice_daemon_pids(sb)}")
    wts = trees(sb, repo, 6, "k")
    clients = [sb.popen(["sluice", "run", "long"], wt) for wt in wts]
    up = wait_until(lambda: len(gate_pids(T + "-long")) == 6 and running_jobs(sb, repo) == 6, 40)
    tracked = census(old)
    sleeps = [p for p in tracked if Path(f"/proc/{p}/comm").read_text().strip() == "sleep"]
    clean.extra_pids.update(tracked)
    rep.check("1: precondition: 6 gates running under the daemon, each with a child process", up and len(sleeps) == 6,
              f"{len(gate_pids(T + '-long'))} gates, {len(sleeps)} children, {len(tracked)} processes tracked")
    t0 = time.monotonic()
    kill_quietly(old, signal.SIGKILL)
    gone = wait_until(lambda: not any(alive(p) for p in tracked), 10, 0.05)
    t_gone = time.monotonic() - t0
    survivors = sorted(p for p in tracked if alive(p))
    detail = {p: Path(f"/proc/{p}/cmdline").read_bytes().replace(b"\0", b" ").decode()[:80] for p in survivors}
    rep.check("1: no gate process or descendant survives the daemon's SIGKILL", not survivors,
              f"all gone after {t_gone:.2f}s" if gone else f"{len(survivors)} of {len(tracked)} still alive after 10 s: {detail}")
    kill_all(survivors)
    wait_until(lambda: not any(alive(p) for p in survivors), 5)
    outs = []
    for c in clients:
        try:
            o, e = c.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            c.kill()
            o, e = c.communicate()
            outs.append((None, o, e))
            continue
        outs.append((c.returncode, o, e))
    bad = [(rc, (o + e)[-120:]) for rc, o, e in outs if rc in (0, None) or "PASS" in o]
    rep.check("1: interrupted runs are reported to their clients as failures, never as a pass", not bad,
              f"exit codes {[rc for rc, _, _ in outs]}; sample message {outs[0][2][-200:]!r}", offenders=bad)
    t0 = time.monotonic()
    cp = sb.run(["sluice", "run", "quick"], trees(sb, repo, 1, "after")[0])
    t_restart = time.monotonic() - t0
    new = sb.daemon_pid(repo)
    if new:
        clean.extra_pids.add(new)
    rep.check("1: the next client call restarts the daemon in under 2 s and runs the gate",
              cp.returncode == 0 and "PASS" in cp.stdout and new not in (None, old) and t_restart < 2.0,
              f"exit {cp.returncode}, {t_restart:.2f}s, daemon {old} -> {new}, stderr {cp.stderr[-160:]!r}")
    rc, checks = doctor(sb, repo)
    st = sb.status_json(repo)
    rep.check("1: no stale lock or socket: doctor sees one live daemon holding the lease, no jobs left over",
              checks.get("lease", {}).get("status") == "ok" and checks.get("daemon", {}).get("status") == "ok"
              and not st.get("running") and not st.get("queued"),
              f"lease: {checks.get('lease', {}).get('detail')}; running {len(st.get('running', []))}, queued {len(st.get('queued', []))}; "
              f"`sluice daemon` while one runs exits {sb.run(['sluice', 'daemon'], repo).returncode}")
    states = [lookup(sb, wt, "long") for wt in wts]
    rep.check("1: no interrupted tree was recorded as a pass", all(s != "HIT" for s in states), f"{states}")


# ---------------------------------------------------------------------------
def s2_clients_sigkill(rep: Report, mode: str) -> None:
    print(f"== 2. SIGKILL 10 clients mid-wait, last_subscriber_gone = {mode}", flush=True)
    hold = 30 if mode == "cancel" else 6
    sb, repo, counter = new_env(f"c2{mode}", f'[daemon]\nlast_subscriber_gone = "{mode}"\n', {"w": {"hold": hold, "child": mode == "cancel"}})
    dpid = start_daemon(sb, repo)
    wts = trees(sb, repo, 10, "m")
    clients = [sb.popen(["sluice", "run", "w"], wt) for wt in wts]
    up = wait_until(lambda: len(gate_pids(T + "-w")) == 10 and running_jobs(sb, repo) == 10, 40)
    tracked = census(dpid)
    clean.extra_pids.update(tracked)
    for c in clients:
        kill_quietly(c.pid, signal.SIGKILL)
    for c in clients:
        c.communicate()
    t0 = time.monotonic()
    rep.check(f"2/{mode}: precondition: 10 jobs running, 10 clients killed", up, f"{len(tracked)} processes tracked")
    if mode == "cancel":
        gone = wait_until(lambda: not any(alive(p) for p in tracked), 15, 0.1)
        rep.check("2/cancel: the jobs are cancelled and every gate process and child is gone",
                  gone, f"{time.monotonic() - t0:.1f}s after the kill" if gone else f"still alive: {sorted(p for p in tracked if alive(p))}")
        wait_until(lambda: running_jobs(sb, repo) == 0, 10)
        st = sb.status_json(repo)
        states = [lookup(sb, wt, "w") for wt in wts]
        rep.check("2/cancel: nothing left running or queued and no marker written", not st.get("running") and not st.get("queued") and all(s != "HIT" for s in states),
                  f"running {len(st.get('running', []))}, queued {len(st.get('queued', []))}, lookups {sorted(set(states))}")
    else:
        time.sleep(1.0)
        still = [p for p in gate_pids(T + "-w") if alive(p)]
        rep.check("2/finish: the gates keep running after their clients died", len(still) == 10, f"{len(still)} of 10 gate processes alive 1 s later")
        done = wait_until(lambda: all(lookup(sb, wt, "w") == "HIT" for wt in wts), 40, 0.5)
        states = [lookup(sb, wt, "w") for wt in wts]
        rep.check("2/finish: all 10 jobs finished and wrote their markers", done and counter_lines(counter, "w") == 10,
                  f"lookups {sorted(set(states))}, executions {counter_lines(counter, 'w')}")
        wait_until(lambda: running_jobs(sb, repo) == 0, 10)
        rep.check("2/finish: nothing left running afterwards", running_jobs(sb, repo) == 0 and not any(alive(p) for p in gate_pids(T + "-w")))


# ---------------------------------------------------------------------------
def s3_hammer(rep: Report) -> None:
    print(f"== 3. {HAMMER_CLIENTS} clients hammer the daemon for {HAMMER_SECONDS:.0f} s", flush=True)
    gates = {
        "hit": {"hold": 0.05},
        "quick": {"hold": 0.03, "cache": False},
        "bad": {"hold": 0.03, "cache": False},
        "ff": {"hold": 0.03, "hooks": ["pre-commit"]},
    }
    sb, repo, counter = new_env("c3", "", gates)
    dpid = start_daemon(sb, repo)
    hitwt, badwt, ffwt = trees(sb, repo, 3, "h")
    (badwt / "flag.txt").write_text("ox\n")
    sb.git(badwt, "add", "-A")
    cp = sb.run(["sluice", "run", "hit"], hitwt)
    assert cp.returncode == 0, cp.stderr
    expected = {  # operation -> (argv, agent, expected exit code, cwd, must appear in stdout+stderr)
        "status": (["sluice", "status", "--json"], False, 0, hitwt, '"schema"'),
        "cache": (["sluice", "cache", "status"], False, 0, hitwt, "HIT"),
        "hit": (["sluice", "run", "hit"], False, 0, hitwt, "cached"),
        "quick": (["sluice", "run", "quick"], False, 0, hitwt, "PASS"),
        "bad": (["sluice", "run", "bad"], False, 1, badwt, "FAIL"),
        "fail-fast": (["sluice", "run", "--hook", "pre-commit", "ff"], True, 3, ffwt, "fail-fast"),
    }
    stop = threading.Event()
    lock = threading.Lock()
    counts: dict[str, int] = {}
    errors: list[tuple] = []
    mcp_calls = [0]
    n_mcp = max(1, HAMMER_CLIENTS // 10)

    def record(op: str, bad: tuple | None) -> None:
        with lock:
            counts[op] = counts.get(op, 0) + 1
            if bad and len(errors) < 50:
                errors.append(bad)

    def cli_worker(seed: int) -> None:
        rnd = random.Random(seed)
        names = list(expected)
        while not stop.is_set():
            op = rnd.choice(names)
            argv, agent, rc_expected, cwd, needle = expected[op]
            cp = sb.run(argv, cwd, agent=agent, timeout=120)
            text = cp.stdout + cp.stderr
            ok = cp.returncode == rc_expected and needle in text
            record(op, None if ok else (op, cp.returncode, text[-200:]))
            time.sleep(rnd.uniform(0.0, 0.1))

    def mcp_worker(i: int) -> None:
        ag = McpAgent(sb.env(agent=True), hitwt, name=f"h{i}")
        try:
            while not stop.is_set():
                r = ag.verify(hitwt, ["quick"], timeout=120)
                ok = not r["is_error"] and (r["structured"] or {}).get("all_passed") is True
                record("mcp-verify", None if ok else ("mcp-verify", r["is_error"], r["text"][-200:]))
                mcp_calls[0] += 1
                time.sleep(random.uniform(0.0, 0.2))
        finally:
            ag.close()

    series: list[tuple[float, int, int]] = []

    def sampler() -> None:
        t0 = time.monotonic()
        while not stop.is_set():
            try:
                series.append((time.monotonic() - t0, rss_kib(dpid), fd_count(dpid)))
            except OSError:
                break
            time.sleep(1.0)

    idle_fd = fd_count(dpid)
    idle_rss = rss_kib(dpid)
    threads = [threading.Thread(target=cli_worker, args=(i,)) for i in range(HAMMER_CLIENTS - n_mcp)]
    threads += [threading.Thread(target=mcp_worker, args=(i,)) for i in range(n_mcp)]
    samp = threading.Thread(target=sampler)
    for t in threads:
        t.start()
    samp.start()
    time.sleep(HAMMER_SECONDS)
    stop.set()
    for t in threads:
        t.join(timeout=150)
    samp.join()
    time.sleep(3)
    total = sum(counts.values())
    rep.check("3: every operation returned its documented result under load (exit codes and messages)", not errors and total > 0,
              f"{total} operations ({counts}); {len(errors)} unexpected", errors=errors[:10])
    survived = sb.daemon_pid(repo) == dpid and alive(dpid)
    log = (sb.state / "sluice" / "daemon.log").read_text(errors="replace")
    panics = [ln for ln in log.splitlines() if "panick" in ln.lower()]
    rep.check("3: the daemon neither crashed nor restarted and logged no panic", survived and not panics, f"pid {dpid}, panics {panics[:2]}")
    warm = [r for t, r, _ in series if 5 <= t <= 15] or [idle_rss]
    late = [r for t, r, _ in series if t >= HAMMER_SECONDS - 15]
    peak = max(r for _, r, _ in series)
    rep.check("3: daemon RSS stays bounded (no growth trend, peak under 256 MiB)",
              peak < 256 * 1024 and (not late or max(late) <= max(warm) * 1.5 + 16 * 1024),
              f"idle {idle_rss / 1024:.1f} MiB, warm-up max {max(warm) / 1024:.1f} MiB, last-15s max {max(late or [0]) / 1024:.1f} MiB, peak {peak / 1024:.1f} MiB",
              series=[(round(t), r // 1024, f) for t, r, f in series][::5])
    fd_end = fd_count(dpid)
    fd_peak = max(f for _, _, f in series)
    rep.check("3: the daemon's fd count returns to its idle level once the clients are gone (no leak)", fd_end <= idle_fd + 4,
              f"idle {idle_fd}, peak {fd_peak}, after the load {fd_end}")
    st = sb.status_json(repo)
    rep.check("3: the daemon is idle and consistent afterwards", not st.get("running") and not st.get("queued") and (st.get("budget") or {}).get("used") == 0,
              f"running {len(st.get('running', []))}, queued {len(st.get('queued', []))}, budget used {(st.get('budget') or {}).get('used')}, totals {st.get('totals')}")


# ---------------------------------------------------------------------------
class Burners:
    """Busy loops that keep every CPU runnable (twice as many loops as CPUs)."""

    def __init__(self) -> None:
        self.procs: list[subprocess.Popen] = []

    def start(self, n: int) -> None:
        for _ in range(n):
            p = subprocess.Popen(["python3", "-c", "while True: pass", f"{T}-burn"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.procs.append(p)
            clean.extra_pids.add(p.pid)

    def stop(self) -> None:
        for p in self.procs:
            kill_quietly(p.pid, signal.SIGKILL)
        for p in self.procs:
            p.wait()
        self.procs = []


def psi_state(sb: Sandbox, repo: Path) -> tuple[bool, int, int, int]:
    st = sb.status_json(repo)
    psi = (st.get("budget") or {}).get("psi") or {}
    return bool(psi.get("blocking")), len(st.get("running", [])), len(st.get("queued", [])), int((st.get("totals") or {}).get("psi_blocked_ms_total", 0))


def s4_pressure(rep: Report) -> None:
    print("== 4. synthetic CPU pressure", flush=True)
    cfg = '[admission]\ncpu_some_avg10_max = 25.0\npsi_poll_interval = "500ms"\npsi_max_wait = "%s"\n'
    burners = Burners()
    ncpu = os.cpu_count() or 4
    try:
        # A: a job waits while PSI blocks and resumes when the load stops.
        sb, repo, counter = new_env("c4a", cfg % "180s", {"g": {"hold": 0.2}, "base": {"hold": 0.1}})
        start_daemon(sb, repo)
        t_base = trees(sb, repo, 2, "p")
        sb.run(["sluice", "run", "base"], t_base[0])
        blocked0 = psi_state(sb, repo)[3]
        burners.start(2 * ncpu)
        blocking = wait_until(lambda: psi_state(sb, repo)[0], 60, 0.5)
        rep.check("4: precondition: PSI turns blocking under synthetic load", blocking, f"{2 * ncpu} busy loops on {ncpu} CPUs")
        client = sb.popen(["sluice", "run", "g"], t_base[1])
        t_sub = time.monotonic()
        samples = []
        while time.monotonic() - t_sub < 6:
            samples.append(psi_state(sb, repo))
            time.sleep(0.5)
        waiting = client.poll() is None and counter_lines(counter, "g") == 0 and all(s[1] == 0 for s in samples) and any(s[2] >= 1 for s in samples)
        grew = samples[-1][3] - blocked0
        rep.check("4: while PSI blocks, a new job waits in the queue (not run, client still waiting)", waiting,
                  f"client alive {client.poll() is None}, executions {counter_lines(counter, 'g')}, queued samples {[s[2] for s in samples]}")
        rep.check("4: psi_blocked_ms_total grows while the job waits", grew >= 3000, f"+{grew} ms over {time.monotonic() - t_sub:.0f} s")
        burners.stop()
        t_stop = time.monotonic()
        try:
            out, err = client.communicate(timeout=120)
            rc = client.returncode
        except subprocess.TimeoutExpired:
            client.kill()
            out, err = client.communicate()
            rc = None
        rep.check("4: the job resumes and passes once the load stops", rc == 0 and "PASS" in out and counter_lines(counter, "g") == 1,
                  f"exit {rc}, {time.monotonic() - t_stop:.1f} s after the load stopped, executions {counter_lines(counter, 'g')}")
        sb.stop_daemon(repo)

        # B: the starvation guard admits the queue head while PSI still blocks.
        sb, repo, counter = new_env("c4b", cfg % "12s", {"g": {"hold": 4}, "base": {"hold": 0.1}})
        start_daemon(sb, repo)
        t_b = trees(sb, repo, 2, "q")
        sb.run(["sluice", "run", "base"], t_b[0])
        burners.start(2 * ncpu)
        blocking = wait_until(lambda: psi_state(sb, repo)[0], 60, 0.5)
        rep.check("4: guard precondition: PSI turns blocking again", blocking)
        client = sb.popen(["sluice", "run", "g"], t_b[1])
        t_sub = time.monotonic()
        admitted = None
        while time.monotonic() - t_sub < 45 and client.poll() is None:
            blk, run_n, queued, _ = psi_state(sb, repo)
            if run_n >= 1:
                admitted = (time.monotonic() - t_sub, blk)
                break
            time.sleep(0.25)
        rep.check("4: psi_max_wait admits the starving job (12 s) although PSI still blocks",
                  admitted is not None and 10 <= admitted[0] <= 35 and admitted[1] is True,
                  f"admitted after {admitted[0]:.1f} s with psi.blocking={admitted[1]}" if admitted else "never admitted within 45 s")
        try:
            out, err = client.communicate(timeout=120)
            rc = client.returncode
        except subprocess.TimeoutExpired:
            client.kill()
            out, err = client.communicate()
            rc = None
        rep.check("4: the admitted job completes and passes under pressure", rc == 0 and "PASS" in out, f"exit {rc}")
    finally:
        burners.stop()


# ---------------------------------------------------------------------------
def s5_unwritable(rep: Report) -> None:
    print("== 5. unwritable storage", flush=True)
    sb, repo, counter = new_env("c5", "", {"a": {"hold": 0.2}, "b": {"hold": 0.2}, "c": {"hold": 0.2}})
    dpid = start_daemon(sb, repo)
    common = Path(sb.git(repo, "rev-parse", "--path-format=absolute", "--git-common-dir"))
    markers = common / "sluice" / "markers"
    state = sb.state / "sluice"
    logs = state / "logs"
    sb.run(["sluice", "run", "a"], repo, check=True)
    (repo / "x.txt").write_text("x\n")

    # A: the markers directory is read-only.
    os.chmod(markers, 0o500)
    try:
        cp = sb.run(["sluice", "run", "a"], repo)
        text = cp.stdout + cp.stderr
        rep.check("5/markers: with an unwritable markers dir the run still returns its verdict (PASS, exit 0)", cp.returncode == 0 and "PASS" in cp.stdout, f"exit {cp.returncode}: {text[-260:]!r}")
        rep.check("5/markers: the write error is reported once for the run", text.count("Permission denied") == 1, f"{text.count('Permission denied')} occurrence(s) of the error")
        rc, checks = doctor(sb, repo)
        rep.check("5/markers: doctor reports the unwritable markers directory", checks.get("markers", {}).get("status") in ("warn", "fail"),
                  f"doctor exit {rc}; markers: {checks.get('markers')}")
    finally:
        os.chmod(markers, 0o700)
    rep.check("5/markers: the daemon survived", sb.daemon_pid(repo) == dpid and alive(dpid))
    cp = sb.run(["sluice", "run", "a"], repo)
    cp2 = sb.run(["sluice", "run", "a"], repo)
    rep.check("5/markers: after restoring permissions the next run writes a marker and the one after hits", cp.returncode == 0 and "cached" in cp2.stdout,
              f"{cp.stdout.strip()!r} then {cp2.stdout.strip()!r}")

    # B: the state directory (logs, database) is read-only while the daemon runs.
    (repo / "y.txt").write_text("y\n")
    os.chmod(logs, 0o500)
    os.chmod(state, 0o500)
    try:
        cp = sb.run(["sluice", "run", "b"], repo)
        text = cp.stdout + cp.stderr
        rep.check("5/state: with an unwritable state dir the run still returns its verdict (PASS, exit 0)", cp.returncode == 0 and "PASS" in cp.stdout,
                  f"exit {cp.returncode}: {text[-300:]!r}; the gate ran {counter_lines(counter, 'b')} time(s)")
        rep.check("5/state: the write error is reported once for the run", text.count("Permission denied") == 1, f"{text.count('Permission denied')} occurrence(s)")
        rc, checks = doctor(sb, repo)
        rep.check("5/state: doctor reports the unwritable state directory", checks.get("state-dir", {}).get("status") == "fail" and rc == 1,
                  f"doctor exit {rc}; state-dir: {checks.get('state-dir', {}).get('status')}")
        rep.check("5/state: the daemon did not crash and still answers status", sb.daemon_pid(repo) == dpid and alive(dpid))
        # C: a new daemon started while the state dir is read-only.
        sb.stop_daemon(repo)
        wait_until(lambda: not alive(dpid), 10)
        (repo / "z.txt").write_text("z\n")
        cp = sb.run(["sluice", "run", "c"], repo)
        text = cp.stdout + cp.stderr
        rep.check("5/state: a daemon (re)started on a read-only state dir still returns verdicts (PASS, exit 0)", cp.returncode == 0 and "PASS" in cp.stdout,
                  f"exit {cp.returncode}: {text[-300:]!r}; the gate ran {counter_lines(counter, 'c')} time(s)")
    finally:
        os.chmod(state, 0o700)
        os.chmod(logs, 0o700)
    cp = sb.run(["sluice", "run", "c"], repo)
    rc, checks = doctor(sb, repo)
    rep.check("5/state: after restoring permissions everything works again", cp.returncode == 0 and rc == 0, f"run exit {cp.returncode}, doctor exit {rc}")
    rep.check("5: limitation noted", True, "no root: a read-only tmpfs cannot be mounted; directories are chmod 0500 instead (an owner-writable existing file such as sluice.db stays writable)")


# ---------------------------------------------------------------------------
def s6_corruption(rep: Report) -> None:
    print("== 6. corrupt markers and a truncated SQLite WAL", flush=True)
    sb, repo, counter = new_env("c6", "", {"a": {"hold": 0.2}, "b": {"hold": 0.2}, "c": {"hold": 0.2}, "q": {"hold": 0.1}})
    dpid = start_daemon(sb, repo)
    common = Path(sb.git(repo, "rev-parse", "--path-format=absolute", "--git-common-dir"))
    markers = common / "sluice" / "markers"
    for g in "abc":
        sb.run(["sluice", "run", g], repo, check=True)
    files = {}
    for m in markers.glob("*.json"):
        files[json.loads(m.read_text())["gate"]] = m
    original_a = files["a"].read_text()
    files["a"].write_bytes(original_a.encode()[:25])  # truncated JSON
    files["b"].write_bytes(os.urandom(300))  # binary garbage
    files["c"].write_text(original_a)  # valid marker of another key under this name
    (markers / "feedface.miss.json").write_text("{not json")
    (markers / "notes.txt").write_text("stray file\n")
    (markers / ".tmp-4242-1").write_text("half written")
    rc, checks = doctor(sb, repo)
    rep.check("6/markers: doctor reports the damaged markers directory", checks.get("markers", {}).get("status") in ("warn", "fail"),
              f"doctor exit {rc}; markers: {checks.get('markers')}")
    results = []
    for g in "abc":
        n0 = counter_lines(counter, g)
        cp = sb.run(["sluice", "run", g], repo)
        results.append((g, cp.returncode, counter_lines(counter, g) - n0, (cp.stdout + cp.stderr)[-140:]))
    rep.check("6/markers: corrupt, garbage and mismatching markers are ignored: each gate re-runs once and passes",
              all(rc == 0 and ran == 1 for _, rc, ran, _ in results), f"{[(g, rc, ran) for g, rc, ran, _ in results]}", rows=results)
    again = [sb.run(["sluice", "run", g], repo).stdout for g in "abc"]
    rep.check("6/markers: the rewritten markers hit on the next run", all("cached" in o for o in again), f"{[o.strip()[-40:] for o in again]}")

    # Truncated WAL: build history, SIGKILL the daemon so the WAL is not checkpointed, cut the WAL in half.
    wts = trees(sb, repo, 8, "w")
    for wt in wts:
        sb.run(["sluice", "run", "q"], wt, check=True)
    before = sb.totals(repo)
    sb.status_json(repo)
    kill_quietly(dpid, signal.SIGKILL)
    wait_until(lambda: not alive(dpid), 5)
    wal = sb.state / "sluice" / "sluice.db-wal"
    size = wal.stat().st_size if wal.exists() else 0
    if size:
        with wal.open("r+b") as f:
            f.truncate(size // 2)
    cp = sb.run(["sluice", "run", "q"], trees(sb, repo, 1, "wafter")[0])
    npid = sb.daemon_pid(repo)
    if npid:
        clean.extra_pids.add(npid)
    after = sb.totals(repo)
    rep.check("6/wal: precondition: an uncheckpointed WAL existed and was truncated", size > 0, f"sluice.db-wal was {size} bytes, cut to {size // 2}")
    rep.check("6/wal: the daemon restarts on the truncated WAL, runs gates and answers status", cp.returncode == 0 and "PASS" in cp.stdout and npid not in (None, dpid) and bool(after),
              f"run exit {cp.returncode}, totals before the cut {before.get('runs')} runs, after {after.get('runs')} runs")
    log = (sb.state / "sluice" / "daemon.log").read_text(errors="replace")
    rep.check("6/wal: no panic in the daemon log", "panick" not in log.lower())
    rc, checks = doctor(sb, repo)
    rep.check("6/wal: doctor still works and sees a healthy daemon (it has no database check: SQLite silently drops the cut WAL tail)",
              rc == 0 and checks.get("daemon", {}).get("status") == "ok", f"doctor exit {rc}")
    # Beyond the plan: a damaged main database file.
    sb.stop_daemon(repo)
    wait_until(lambda: not sluice_daemon_pids(sb), 10)
    db = sb.state / "sluice" / "sluice.db"
    with db.open("r+b") as f:
        f.write(b"\xff" * 100)
    for ext in ("-wal", "-shm"):
        p = Path(str(db) + ext)
        if p.exists():
            p.unlink()
    cp = sb.run(["sluice", "run", "q"], trees(sb, repo, 1, "dbafter")[0])
    npid = sb.daemon_pid(repo)
    if npid:
        clean.extra_pids.add(npid)
    rc, checks = doctor(sb, repo)
    rep.check("6/db (beyond the plan): a damaged sluice.db does not stop gates from returning verdicts", cp.returncode == 0 and "PASS" in cp.stdout,
              f"run exit {cp.returncode}: {(cp.stdout + cp.stderr)[-300:]!r}; doctor exit {rc}")


# ---------------------------------------------------------------------------
def main() -> int:
    require_testbeds()
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s4_chaos", out)
    t_start = time.monotonic()
    steps = [
        s1_daemon_sigkill,
        lambda r: s2_clients_sigkill(r, "cancel"),
        lambda r: s2_clients_sigkill(r, "finish"),
        s3_hammer,
        s4_pressure,
        s5_unwritable,
        s6_corruption,
    ]
    try:
        for step in steps:
            try:
                step(rep)
            except Exception as e:  # noqa: BLE001 - a crashed step is a failed check, not a crashed run
                import traceback
                rep.check(f"step crashed: {getattr(step, '__name__', 'step')}", False, f"{type(e).__name__}: {e}", trace=traceback.format_exc()[-1500:])
    finally:
        left = clean.settle()
        rep.check("no daemon, gate or load process left behind", not left, f"{left}")
        for s in clean.sandboxes:
            s.close()
    dur = time.monotonic() - t_start
    rep.check("scenario finished in under 5 minutes", dur < 300, f"{dur:.0f}s")
    rep.save()
    print(f"s4_chaos: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
