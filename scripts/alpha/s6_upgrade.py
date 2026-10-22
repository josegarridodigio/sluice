"""Scenario 6: upgrade with live state.

Real release binaries are downloaded with `gh release download` (the
`*x86_64*` archive plus `SHA256SUMS`, verified here) into a scratch directory
and used through a symlink in the sandbox's bin dir. A repository gets
markers, a failure record and job history from the oldest release with a
daemon running; then the binary is replaced hop by hop with that daemon still
running. After every step:

  * the old markers still hit (no gate re-runs) and the failure record stays;
  * `status` answers and the job history (database rows, `sluice logs`) is
    intact;
  * the queue schema does not exist before the first queue use and is created
    by it (only from the first release that has a queue);
  * the daemon is restarted by `update` (idle) or left alone (busy);
  * `doctor` is clean (no failures, no warning other than the expected
    "no hook" one) once the daemon runs the installed version;
  * a fresh tree still runs, over the CLI and over MCP.

Hops: a release that has no `update` subcommand (v0.1.0) is upgraded with the
installer (`scripts/install.sh`, `SLUICE_VERSION`, `SLUICE_INSTALL_DIR`) which
is what such a user would run; releases that have it use
`sluice update --version <tag> --yes` (real GitHub release, no
SLUICE_UPDATE_BASE_URL). The last hop swaps in the binary under test the way
the installer does (copy + atomic rename). A final pair of steps exercises
`sluice update` itself with the binary under test: a busy daemon is left
alone, an idle one is restarted (a downgrade to the first release).

Knobs: ALPHA_TAGS (default "v0.1.0,v0.2.0"; rerun at R4 with
"v0.2.0,v0.3.0"), ALPHA_RELEASES (download cache, default a new directory
under TMPDIR), ALPHA_OUT, TMPDIR, ALPHA_TESTBEDS, ALPHA_PYLIB; SLUICE_BIN is
the binary under test.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

from lib import REPO_ROOT, Report, Sandbox, add_worktree, sluice_binary, wait_until
from mcp_agent import McpAgent
from pygate import Cleanup, alive, counter_lines, require_testbeds, setup_repo, sluice_daemon_pids

GH_REPO = "josegarridodigio/sluice"
TOKEN = f"alpha-s6-{os.getpid()}"
TAGS = [t.strip() for t in os.environ.get("ALPHA_TAGS", "v0.1.0,v0.2.0").split(",") if t.strip()]
BASE = Path(os.environ["TMPDIR"]) if os.environ.get("TMPDIR") else None


def fetch_release(tag: str, cache: Path, rep: Report) -> Path:
    """Download the x86_64 archive and SHA256SUMS of `tag`, verify, extract only the binary."""
    d = cache / tag
    d.mkdir(parents=True, exist_ok=True)
    sums = d / "SHA256SUMS"
    if not sums.exists() or not list(d.glob("*x86_64*.tar.gz")):
        subprocess.run(["gh", "release", "download", tag, "-R", GH_REPO, "-p", "*x86_64*", "-p", "SHA256SUMS", "-D", str(d), "--clobber"],
                       check=True, capture_output=True, text=True, timeout=300)
    archive = next(d.glob("*x86_64*.tar.gz"))
    expected = None
    for line in sums.read_text().splitlines():
        digest, _, name = line.partition("  ")
        if name.strip().lstrip("*") == archive.name:
            expected = digest
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    rep.check(f"release {tag}: {archive.name} matches SHA256SUMS", expected is not None and actual == expected, f"sha256 {actual[:16]}")
    binary = d / "sluice"
    with tarfile.open(archive) as tf:
        member = next(m for m in tf.getmembers() if m.isfile() and m.name.endswith("/sluice") and ".." not in m.name)
        binary.write_bytes(tf.extractfile(member).read())
    binary.chmod(0o755)
    return binary


def version_of(path: str | Path) -> str:
    out = subprocess.run([str(path), "--version"], capture_output=True, text=True, timeout=30).stdout.strip()
    return out.split()[-1] if out else "?"


class Upgrade:
    def __init__(self, rep: Report, cache: Path) -> None:
        self.rep = rep
        self.sb = Sandbox("s6", base=BASE)
        self.cache = cache
        self.store = self.sb.root / "store"
        self.store.mkdir()
        self.counter = Path()
        self.repo = Path()
        self.wt_fail = Path()
        self.baseline: dict = {}
        self.n = 0

    # -- binaries ----------------------------------------------------------
    def install_copy(self, src: Path, name: str) -> None:
        """Make bin/sluice a symlink to a private copy of `src` (so `update` and
        the installer never touch the download cache or the binary under test)."""
        dst = self.store / name
        shutil.copy2(src, dst)
        link = self.sb.bin / "sluice.tmp"
        if link.is_symlink() or link.exists():
            link.unlink()
        os.symlink(dst, link)
        os.replace(link, self.sb.bin / "sluice")  # atomic, like the installer's mv

    def current_version(self) -> str:
        return version_of(self.sb.bin / "sluice")

    def supports_update(self) -> bool:
        return "update" in self.sb.run(["sluice", "--help"], self.repo).stdout.split()

    def env_extra(self) -> dict:
        return {}  # SLUICE_UPDATE_BASE_URL stays unset (the sandbox env is built from scratch): real GitHub releases

    # -- observation ---------------------------------------------------------
    def db(self) -> dict:
        path = self.sb.state / "sluice" / "sluice.db"
        out = {"tables": [], "jobs": set(), "schema": None, "rows": {}}
        if not path.exists():
            return out
        c = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5)
        try:
            out["tables"] = sorted(r[0] for r in c.execute("select name from sqlite_master where type='table'"))
            out["jobs"] = {r[0] for r in c.execute("select id from jobs")}
            out["schema"] = [r[0] for r in c.execute("select version from schema")]
            out["rows"] = {t: c.execute(f"select count(*) from {t}").fetchone()[0] for t in out["tables"]}
        finally:
            c.close()
        return out

    def daemon(self) -> dict:
        return self.sb.status_json(self.repo).get("daemon") or {}

    def lookup(self, wt: Path, gate: str) -> str:
        out = self.sb.run(["sluice", "cache", "status"], wt).stdout
        for line in out.splitlines():
            parts = line.split()
            if parts and parts[0] == gate:
                return " ".join(parts[1:])
        return "?"

    def new_tree(self) -> Path:
        self.n += 1
        wt = add_worktree(self.sb, self.repo, f"u{self.n}", f"alpha/u{self.n}")
        (wt / f"fresh-{self.n}.txt").write_text(f"fresh {self.n}\n")
        self.sb.git(wt, "add", "-A")
        return wt

    # -- the checks after every step ------------------------------------------
    def after(self, step: str, doctor: bool = True, queue_absent: bool | None = None) -> None:
        rep, sb = self.rep, self.sb
        n0 = counter_lines(self.counter, "a") + counter_lines(self.counter, "b")
        cp = sb.run(["sluice", "run", "a"], self.repo)
        cp2 = sb.run(["sluice", "run", "b"], self.repo)
        executed = counter_lines(self.counter, "a") + counter_lines(self.counter, "b") - n0
        rep.check(f"{step}: markers written before the upgrade still hit (no gate re-runs)",
                  cp.returncode == 0 and cp2.returncode == 0 and "cached" in cp.stdout and "cached" in cp2.stdout and executed == 0
                  and self.lookup(self.repo, "a").startswith("HIT"),
                  f"exit {cp.returncode}/{cp2.returncode}, re-executed {executed}, {cp.stdout.strip()!r}")
        fail = self.lookup(self.wt_fail, "f")
        rep.check(f"{step}: the failure record is still reported (never a pass)", fail.startswith("MISS") and "exit 1" in fail, fail)
        st = sb.status_json(self.repo)
        d = self.db()
        old_jobs = self.baseline["jobs"]
        logs_ok = True
        for job in sorted(old_jobs)[:3]:
            cpl = sb.run(["sluice", "logs", job], self.repo)
            logs_ok &= cpl.returncode == 0 and "# sluice job" in cpl.stdout
        rep.check(f"{step}: status answers and the job history is intact (rows and logs of the first runs)",
                  st.get("schema") == 1 and old_jobs <= d["jobs"] and logs_ok and (d["schema"] or [0])[0] >= (self.baseline["schema"] or [0])[0],
                  f"daemon {st.get('daemon', {}).get('version')}, {len(old_jobs)} early jobs of {len(d['jobs'])} rows, logs readable {logs_ok}, "
                  f"schema {d['schema']}, totals {st.get('totals', {}).get('runs')} runs since {st.get('totals', {}).get('since')}")
        if queue_absent is not None:
            queue_tables = [t for t in d["tables"] if "queue" in t]
            rep.check(f"{step}: queue schema {'absent before any queue use' if queue_absent else 'present'}", bool(queue_tables) != queue_absent,
                      f"tables {d['tables']}")
        if doctor:
            cpd = sb.run(["sluice", "doctor", "--json"], self.repo)
            try:
                checks = {c["name"]: c for c in json.loads(cpd.stdout)["checks"]}
            except (ValueError, KeyError):
                checks = {}
            bad = {n: c["status"] for n, c in checks.items() if c["status"] == "fail" or (c["status"] == "warn" and n != "hooks")}
            rep.check(f"{step}: doctor is clean (no failure, no warning except the expected 'no hook')", cpd.returncode == 0 and bool(checks) and not bad,
                      f"exit {cpd.returncode}, offending {bad}")
        # A fresh tree: CLI and MCP still work.
        wt = self.new_tree()
        cpr = sb.run(["sluice", "run", "new"], wt)
        wt2 = self.new_tree()
        ag = McpAgent(sb.env(agent=True), wt2, name=step[:8])
        res = ag.verify(wt2, ["new"])
        ag.close()
        rep.check(f"{step}: a fresh tree runs over the CLI and over MCP", cpr.returncode == 0 and "PASS" in cpr.stdout and not res["is_error"]
                  and (res["structured"] or {}).get("all_passed") is True, f"CLI exit {cpr.returncode}, MCP all_passed {(res['structured'] or {}).get('all_passed')}")

    def restart_daemon_checks(self, step: str, expect_version: str) -> None:
        sb = self.sb
        old = self.daemon().get("pid")
        sb.sluice(self.repo, "daemon", "restart", timeout=60)
        wait_until(lambda: self.daemon().get("pid") not in (None, old), 15)
        d = self.daemon()
        self.rep.check(f"{step}: `daemon restart` starts the new version on the old state",
                       d.get("pid") not in (None, old) and d.get("version") == expect_version, f"pid {old} -> {d.get('pid')}, version {d.get('version')}")


def main() -> int:
    require_testbeds()
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s6_upgrade", out)
    clean = Cleanup()
    clean.tokens.append(TOKEN)
    t_start = time.monotonic()
    cache = Path(os.environ.get("ALPHA_RELEASES") or tempfile.mkdtemp(prefix="alpha-releases-", dir=BASE))
    cache.mkdir(parents=True, exist_ok=True)
    under_test = Path(sluice_binary())
    up = None
    try:
        # ---- the releases ------------------------------------------------------
        binaries = {t: fetch_release(t, cache, rep) for t in TAGS}
        for t, b in binaries.items():
            rep.check(f"release {t}: the extracted binary reports its version", version_of(b) == t.lstrip("v"), f"{version_of(b)}")
        up = Upgrade(rep, cache)
        clean.sandboxes.append(up.sb)
        sb = up.sb
        up.install_copy(binaries[TAGS[0]], f"sluice-{TAGS[0]}")

        # ---- step 0: state created by the first release -------------------------
        print(f"== 0. {TAGS[0]}: daemon running, markers, a failure and history", flush=True)
        gates = {"a": {"hold": 0.2}, "b": {"hold": 0.2}, "f": {"hold": 0.1}, "new": {"hold": 0.2}, "slow": {"hold": 8}}
        up.repo, up.counter = setup_repo(sb, "up", TOKEN, gates)
        repo = up.repo
        up.wt_fail = add_worktree(sb, repo, "fail", "alpha/fail")
        (up.wt_fail / "flag.txt").write_text("broken\n")
        sb.git(up.wt_fail, "add", "-A")
        r = [sb.run(["sluice", "run", g], repo) for g in ("a", "b")]
        rf = sb.run(["sluice", "run", "f"], up.wt_fail)
        ag = McpAgent(sb.env(agent=True), repo, name="s0")
        res = ag.verify(repo, ["a", "b"])
        ag.close()
        d0 = up.daemon()
        up.baseline = up.db()
        rep.check(f"0: {TAGS[0]} daemon running with markers, one failure and history",
                  all(x.returncode == 0 for x in r) and rf.returncode != 0 and d0.get("version") == TAGS[0].lstrip("v") and len(up.baseline["jobs"]) >= 3,
                  f"daemon {d0.get('version')} pid {d0.get('pid')}, {len(up.baseline['jobs'])} jobs, failing gate exit {rf.returncode}, MCP verify all_passed {(res['structured'] or {}).get('all_passed')}")
        rep.check("0: the process census sees the daemon (sanity of the final leak check)", d0.get("pid") in sluice_daemon_pids(sb), f"{sluice_daemon_pids(sb)}")
        up.after(f"0/{TAGS[0]}", doctor=False)

        # ---- the hops ------------------------------------------------------------
        prev = TAGS[0]
        for tag in TAGS[1:]:
            print(f"== {prev} -> {tag}", flush=True)
            old = up.daemon()
            step = f"{prev}->{tag}"
            if up.supports_update():
                cp = sb.run(["sluice", "update", "--version", tag, "--yes"], repo, extra=up.env_extra(), timeout=300)
                rep.check(f"{step}: `sluice update --version {tag} --yes` succeeds", cp.returncode == 0 and up.current_version() == tag.lstrip("v"),
                          f"exit {cp.returncode}, now {up.current_version()}: {(cp.stdout + cp.stderr)[-240:]!r}")
                gone = wait_until(lambda: not alive(old["pid"]), 15)
                rep.check(f"{step}: the idle daemon was stopped by update", gone, f"old daemon {old.get('pid')} alive={alive(old['pid'])}")
            else:
                env = dict(sb.env(), SLUICE_VERSION=tag, SLUICE_INSTALL_DIR=str(sb.bin))
                cp = subprocess.run(["sh", str(REPO_ROOT / "scripts" / "install.sh")], env=env, cwd=repo, capture_output=True, text=True, timeout=300)
                rep.check(f"{step}: {prev} has no `update`: the installer (SLUICE_VERSION={tag}) replaces the binary", cp.returncode == 0 and up.current_version() == tag.lstrip("v"),
                          f"exit {cp.returncode}, now {up.current_version()}: {(cp.stdout + cp.stderr)[-200:]!r}")
                still = up.daemon()
                rep.check(f"{step}: the old daemon keeps running and answers the new client", still.get("pid") == old.get("pid") and alive(old["pid"]) and still.get("version") == prev.lstrip("v"),
                          f"daemon pid {still.get('pid')} version {still.get('version')}")
                # The old daemon, the new client: markers and runs keep working before any restart.
                wt = up.new_tree()
                cpr = sb.run(["sluice", "run", "new"], wt)
                n0 = counter_lines(up.counter, "a")
                cph = sb.run(["sluice", "run", "a"], repo)
                rep.check(f"{step}: new client against the old daemon: hit and run both work", cpr.returncode == 0 and cph.returncode == 0 and counter_lines(up.counter, "a") == n0,
                          f"run exit {cpr.returncode}, hit exit {cph.returncode}")
                cpd = sb.run(["sluice", "doctor", "--json"], repo)
                try:
                    checks = {c["name"]: c for c in json.loads(cpd.stdout)["checks"]}
                except ValueError:
                    checks = {}
                rep.check(f"{step}: doctor flags the stale daemon (daemon-version is not ok)", checks.get("daemon-version", {}).get("status") in ("warn", "fail"),
                          f"{checks.get('daemon-version')}")
                tables = up.db()["tables"]
                rep.check(f"{step}: nothing queue-related exists in the database before the first queue use", not [t for t in tables if "queue" in t], f"{tables}")
                up.restart_daemon_checks(step, tag.lstrip("v"))
            queue_before = [t for t in up.db()["tables"] if "queue" in t]
            up.after(step, queue_absent=True if not queue_before else None)
            cpq = sb.run(["sluice", "queue", "status", "--json"], repo)
            d = up.db()
            qt = [t for t in d["tables"] if "queue" in t]
            rep.check(f"{step}: the first queue use creates the queue schema and keeps the old tables",
                      cpq.returncode == 0 and bool(qt) and all(d["rows"].get(t, 0) >= up.baseline["rows"].get(t, 0) for t in up.baseline["rows"]),
                      f"queue status exit {cpq.returncode}, queue tables {qt}")
            up.after(f"{step} (after first queue use)", queue_absent=False)
            prev = tag

        # ---- the binary under test --------------------------------------------------
        print(f"== {prev} -> binary under test ({version_of(under_test)})", flush=True)
        step = f"{prev}->under-test"
        old = up.daemon()
        up.install_copy(under_test, "sluice-under-test")
        still = up.daemon()
        rep.check(f"{step}: swapping the binary leaves the running daemon alone", still.get("pid") == old.get("pid") and alive(old["pid"]),
                  f"daemon pid {still.get('pid')} version {still.get('version')}")
        up.after(f"{step}/old-daemon", doctor=version_of(under_test) == prev.lstrip("v"))
        if still.get("version") != version_of(under_test):
            up.restart_daemon_checks(step, version_of(under_test))
        else:
            up.restart_daemon_checks(step, still.get("version"))
        up.after(step)

        # ---- `sluice update` itself, with the binary under test ----------------------
        first = TAGS[0]
        if up.supports_update():
            print(f"== update: busy daemon is left alone, idle daemon is restarted (downgrade to {first})", flush=True)
            wt = up.new_tree()
            slow = sb.popen(["sluice", "run", "slow"], wt)
            busy = wait_until(lambda: len(up.sb.status_json(repo).get("running", [])) == 1, 20)
            before = up.daemon()
            cp = sb.run(["sluice", "update", "--version", first, "--yes"], repo, extra=up.env_extra(), timeout=300)
            after = up.daemon() if up.current_version() != first.lstrip("v") else {}
            out_text = cp.stdout + cp.stderr
            rep.check("update/busy: with a gate running the update succeeds and the daemon is left alone (same pid, still running the job)",
                      busy and cp.returncode == 0 and up.current_version() == first.lstrip("v") and alive(before["pid"]) and "daemon" in out_text.lower(),
                      f"exit {cp.returncode}, now {up.current_version()}, daemon {before.get('pid')} alive={alive(before['pid'])}; message {out_text[-260:]!r}")
            o, e = slow.communicate(timeout=60)
            rep.check("update/busy: the running gate finished normally", slow.returncode == 0 and "PASS" in o, f"exit {slow.returncode}")
            up.install_copy(under_test, "sluice-under-test-2")
            before = up.daemon()
            wait_until(lambda: not up.sb.status_json(repo).get("running"), 10)
            cp = sb.run(["sluice", "update", "--version", first, "--yes"], repo, extra=up.env_extra(), timeout=300)
            gone = wait_until(lambda: not alive(before["pid"]), 15)
            rep.check("update/idle: `sluice update` stops the idle daemon so the next call starts the new version", cp.returncode == 0 and gone,
                      f"exit {cp.returncode}, daemon {before.get('pid')} alive={alive(before['pid'])}: {(cp.stdout + cp.stderr)[-200:]!r}")
            wt = up.new_tree()
            cpr = sb.run(["sluice", "run", "new"], wt)
            d = up.daemon()
            rep.check(f"update/idle: the next call starts a {first} daemon on the state written by newer versions", cpr.returncode == 0 and d.get("version") == first.lstrip("v") and d.get("pid") != before.get("pid"),
                      f"run exit {cpr.returncode}, daemon {d.get('version')} pid {d.get('pid')}")
            up.after(f"downgrade->{first}", doctor=False)
        else:
            rep.check("update: the binary under test has no `update` subcommand", False, "unexpected")
        # Leave the sandbox with the binary under test so the cleanup uses it.
        up.install_copy(under_test, "sluice-under-test-3")
    finally:
        left = clean.settle()
        rep.check("no daemon or gate process left behind", not left, f"{left}")
        if up:
            up.sb.close()
    dur = time.monotonic() - t_start
    rep.check("scenario finished in under 5 minutes", dur < 300, f"{dur:.0f}s")
    rep.save()
    print(f"s6_upgrade: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
