"""Shared helpers for the alpha-readiness harness (stdlib only).

Every scenario runs in its own sandbox: a private HOME / XDG tree so the
user's daemon, cache and config are never touched, the `sluice` binary under
test first in PATH, and git identity fixed. Agents are simulated: they call
the CLI or speak MCP over stdio (see mcp_agent.py); no AI session is used.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent


def sluice_binary() -> str:
    """The binary under test: $SLUICE_BIN, else `sluice` in PATH."""
    explicit = os.environ.get("SLUICE_BIN")
    if explicit:
        return explicit
    found = shutil.which("sluice")
    if not found:
        raise SystemExit("sluice not found: set SLUICE_BIN or install it")
    return found


@dataclass
class Result:
    """One checked claim of a scenario."""

    scenario: str
    check: str
    ok: bool
    detail: str = ""
    data: dict = field(default_factory=dict)


class Report:
    """Collects results and writes them as JSON lines."""

    def __init__(self, scenario: str, out_dir: Path) -> None:
        self.scenario = scenario
        self.out_dir = out_dir
        self.results: list[Result] = []
        out_dir.mkdir(parents=True, exist_ok=True)

    def check(self, check: str, ok: bool, detail: str = "", **data) -> bool:
        r = Result(self.scenario, check, bool(ok), detail, data)
        self.results.append(r)
        mark = "PASS" if ok else "FAIL"
        print(f"  [{mark}] {check}" + (f": {detail}" if detail else ""), flush=True)
        return bool(ok)

    def save(self) -> None:
        path = self.out_dir / f"{self.scenario}.jsonl"
        with path.open("w") as f:
            for r in self.results:
                f.write(json.dumps(r.__dict__) + "\n")

    @property
    def failed(self) -> int:
        return sum(1 for r in self.results if not r.ok)


class Sandbox:
    """Private HOME/XDG tree plus helpers to run commands in it."""

    def __init__(self, name: str, base: Path | None = None, keep: bool = True) -> None:
        root = Path(tempfile.mkdtemp(prefix=f"sluice-alpha-{name}-", dir=base))
        self.root = root
        self.home = root / "home"
        # A Unix socket path must fit in 108 bytes: keep the runtime dir short
        # (a deep scratch path makes the daemon refuse to start).
        self.runtime = Path(tempfile.mkdtemp(prefix="sa-", dir="/tmp"))
        self.state = self.home / "state"
        self.config = self.home / "config"
        for d in (self.home, self.runtime, self.state, self.config / "sluice"):
            d.mkdir(parents=True, exist_ok=True)
        os.chmod(self.runtime, 0o700)
        self.bin = root / "bin"
        self.bin.mkdir()
        os.symlink(sluice_binary(), self.bin / "sluice")
        self.keep = keep

    # -- environment ------------------------------------------------------
    def env(self, agent: bool = False, extra: dict | None = None) -> dict:
        base_path = os.environ.get("PATH", "/usr/bin:/bin")
        env = {
            "HOME": str(self.home),
            "XDG_RUNTIME_DIR": str(self.runtime),
            "XDG_STATE_HOME": str(self.state),
            "XDG_CONFIG_HOME": str(self.config),
            "PATH": f"{self.bin}:{base_path}",
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "GIT_AUTHOR_NAME": "Alpha Harness",
            "GIT_AUTHOR_EMAIL": "alpha@example.invalid",
            "GIT_COMMITTER_NAME": "Alpha Harness",
            "GIT_COMMITTER_EMAIL": "alpha@example.invalid",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": str(self.home / ".gitconfig"),
            # Toolchains keep living in the real home.
            "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
            "CARGO_HOME": os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")),
            "npm_config_cache": str(Path.home() / ".npm"),
        }
        if agent:
            env["CLAUDECODE"] = "1"
        else:
            env["SLUICE_AGENT"] = "0"
        if extra:
            env.update(extra)
        return env

    def write_user_config(self, text: str) -> None:
        (self.config / "sluice" / "config.toml").write_text("version = 1\n" + text)

    # -- processes --------------------------------------------------------
    def run(
        self,
        args: list[str] | str,
        cwd: Path,
        agent: bool = False,
        extra: dict | None = None,
        check: bool = False,
        timeout: float = 600,
        input: str | None = None,
    ) -> subprocess.CompletedProcess:
        shell = isinstance(args, str)
        cp = subprocess.run(
            args,
            cwd=cwd,
            env=self.env(agent, extra),
            shell=shell,
            capture_output=True,
            text=True,
            timeout=timeout,
            input=input,
        )
        if check and cp.returncode != 0:
            raise RuntimeError(f"{args!r} failed ({cp.returncode}):\n{cp.stdout}\n{cp.stderr}")
        return cp

    def popen(self, args: list[str] | str, cwd: Path, agent: bool = False, extra: dict | None = None, **kw):
        return subprocess.Popen(
            args,
            cwd=cwd,
            env=self.env(agent, extra),
            shell=isinstance(args, str),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
            **kw,
        )

    def git(self, cwd: Path, *args: str, check: bool = True) -> str:
        return self.run(["git", *args], cwd, check=check).stdout.strip()

    def sluice(self, cwd: Path, *args: str, agent: bool = False, extra: dict | None = None, timeout: float = 600):
        return self.run(["sluice", *args], cwd, agent=agent, extra=extra, timeout=timeout)

    def status_json(self, cwd: Path) -> dict:
        cp = self.sluice(cwd, "status", "--json")
        try:
            return json.loads(cp.stdout)
        except ValueError:
            return {}

    def totals(self, cwd: Path) -> dict:
        return self.status_json(cwd).get("totals", {})

    def daemon_pid(self, cwd: Path) -> int | None:
        return (self.status_json(cwd).get("daemon") or {}).get("pid")

    def stop_daemon(self, cwd: Path, force: bool = True) -> None:
        args = ["daemon", "stop"] + (["--force"] if force else [])
        self.sluice(cwd, *args, timeout=60)

    def close(self) -> None:
        try:
            self.stop_daemon(self.root, force=True)
        except Exception:  # noqa: BLE001 - best effort
            pass
        shutil.rmtree(self.runtime, ignore_errors=True)
        if not self.keep:
            shutil.rmtree(self.root, ignore_errors=True)


# -- repositories ---------------------------------------------------------
def clone_project(sb: Sandbox, src: Path, name: str) -> Path:
    """Clone a prepared test-bed project into the sandbox, with a bare origin."""
    bare = sb.root / f"{name}.git"
    sb.run(["git", "clone", "-q", "--bare", str(src), str(bare)], sb.root, check=True)
    work = sb.root / name
    sb.run(["git", "clone", "-q", str(bare), str(work)], sb.root, check=True)
    sb.git(work, "config", "commit.gpgsign", "false")
    sb.git(work, "config", "push.autoSetupRemote", "true")
    return work


def add_worktree(sb: Sandbox, repo: Path, name: str, branch: str, start: str = "HEAD") -> Path:
    wt = sb.root / f"wt-{name}"
    sb.git(repo, "worktree", "add", "-q", "-b", branch, str(wt), start)
    return wt


def descendants(pid: int) -> list[int]:
    """All live descendants of `pid` (via /proc)."""
    children: dict[int, list[int]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
        except OSError:
            continue
        ppid = int(stat.rsplit(")", 1)[1].split()[1])
        children.setdefault(ppid, []).append(int(entry.name))
    out, stack = [], [pid]
    while stack:
        for c in children.get(stack.pop(), []):
            out.append(c)
            stack.append(c)
    return out


def procs_matching(fragment: str) -> list[int]:
    """PIDs whose command line contains `fragment`."""
    pids = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            cmd = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            continue
        if fragment in cmd:
            pids.append(int(entry.name))
    return pids


def kill_quietly(pid: int, sig: int = signal.SIGKILL) -> None:
    try:
        os.kill(pid, sig)
    except ProcessLookupError:
        pass


def wait_until(pred, timeout: float, step: float = 0.05) -> bool:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if pred():
            return True
        time.sleep(step)
    return pred()


def loadavg1() -> float:
    return float(Path("/proc/loadavg").read_text().split()[0])


def link_node_modules(src: Path, dst_repo: Path) -> None:
    """A per-checkout node_modules that links every package of `src` but has
    its own `.cache` (tools such as xo write per-checkout state there; one
    shared cache makes parallel worktrees step on each other)."""
    nm = dst_repo / "node_modules"
    nm.mkdir()
    for entry in src.iterdir():
        if entry.name == ".cache":
            continue
        os.symlink(entry, nm / entry.name)
