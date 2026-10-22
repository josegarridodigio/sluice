"""Shared fixtures of the E1 scenarios (s3, s4, s6): a Python test bed with a
controllable gate, process census helpers and daemon bookkeeping.

The gate is `python3 .alpha/gate.py <token> <hold> [--pytest] [--child]`:

  * appends one line to $ALPHA_COUNTER (one line per real execution);
  * sleeps `hold` seconds (so concurrency is observable from /proc);
  * with --child it leaves a long-lived `sleep` child behind (to prove that
    descendants of a killed gate go away too);
  * fails (exit 1) when `flag.txt` exists and is not exactly "ok";
  * with --pytest it also runs a real pytest module of the project.

`token` is unique per scenario and shows up in the command line of the gate
process, which is how the harness counts and finds gate processes without
touching anything else on the machine.
"""

from __future__ import annotations

import os
import shutil
import signal
import time
from pathlib import Path

from lib import Sandbox, clone_project, descendants, kill_quietly, procs_matching

TB = Path(os.environ.get("ALPHA_TESTBEDS", ""))
PYLIB = Path(os.environ.get("ALPHA_PYLIB", ""))

GATE_PY = '''"""Controllable gate of the alpha harness."""
import os, subprocess, sys, time

args = sys.argv[1:]
hold = float(args[1])
counter = os.environ.get("ALPHA_COUNTER")
if counter:
    fd = os.open(counter, os.O_WRONLY | os.O_APPEND | os.O_CREAT)
    os.write(fd, f"{os.getpid()} {time.time():.3f}\\n".encode())
    os.close(fd)
if "--child" in args:
    # Detached stdio: a child holding the gate's output pipe would delay the end of the job.
    subprocess.Popen(["sleep", str(int(hold) + 120)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(hold)
if os.path.exists("flag.txt") and open("flag.txt").read().strip() != "ok":
    print("alpha gate: flag.txt is not ok")
    sys.exit(1)
if "--pytest" in args:
    env = dict(os.environ, PYTHONPATH=os.environ["ALPHA_PYLIB"] + ":src", PYTHONDONTWRITEBYTECODE="1")
    r = subprocess.run([sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider",
                        "tests/test_itsdangerous/test_encoding.py"], env=env)
    sys.exit(r.returncode)
print("alpha gate: ok")
'''


def gate_toml(token: str, counter: Path, gates: dict[str, dict]) -> str:
    """Render a `.sluice.toml`. `gates[name]` accepts: hold, weight, pytest,
    child, key_inputs, key_files, cache, timeout, hooks, extra_env."""
    out = ["version = 1", ""]
    for name, g in gates.items():
        cmd = f"python3 .alpha/gate.py {token}-{name} {g.get('hold', 0.3)}"
        if g.get("pytest"):
            cmd += " --pytest"
        if g.get("child"):
            cmd += " --child"
        env = {"ALPHA_COUNTER": str(counter.with_name(f"{counter.name}-{name}")), "ALPHA_PYLIB": str(PYLIB)}
        env.update(g.get("extra_env", {}))
        out += [f"[gates.{name}]", f'cmd = "{cmd}"', f"weight = {g.get('weight', 1)}",
                f'timeout = "{g.get("timeout", "5m")}"']
        if "hooks" in g:
            out.append("hooks = [" + ", ".join(f'"{h}"' for h in g["hooks"]) + "]")
        if g.get("key_inputs"):
            out.append("key_inputs = [" + ", ".join(f'"{k}"' for k in g["key_inputs"]) + "]")
        if g.get("key_files"):
            out.append("key_files = [" + ", ".join(f'"{k}"' for k in g["key_files"]) + "]")
        if "cache" in g:
            out.append(f"cache = {'true' if g['cache'] else 'false'}")
        out.append("env = { " + ", ".join(f'{k} = "{v}"' for k, v in env.items()) + " }")
        out.append("")
    return "\n".join(out)


def counter_lines(counter: Path, gate: str) -> int:
    p = counter.with_name(f"{counter.name}-{gate}")
    return len(p.read_text().splitlines()) if p.exists() else 0


def setup_repo(sb: Sandbox, name: str, token: str, gates: dict[str, dict], extra_files: dict[str, str] | None = None) -> tuple[Path, Path]:
    """Clone the Python test bed, install the controllable gate and commit.
    Returns (repo, counter_base_path)."""
    repo = clone_project(sb, TB / "py-itsdangerous", name)
    (repo / ".alpha").mkdir()
    (repo / ".alpha" / "gate.py").write_text(GATE_PY)
    (repo / "flag.txt").write_text("ok\n")
    counter = sb.root / f"counter-{name}"
    (repo / ".sluice.toml").write_text(gate_toml(token, counter, gates))
    for rel, text in (extra_files or {}).items():
        (repo / rel).parent.mkdir(parents=True, exist_ok=True)
        (repo / rel).write_text(text)
    sb.git(repo, "add", "-A")
    sb.run(["git", "commit", "-q", "-m", "chore: alpha gate"], repo, check=True)
    return repo, counter


def gate_pids(token: str) -> list[int]:
    """Main gate processes: python3 .alpha/gate.py <token>-... (not the
    `sh -c` wrapper, whose command line merely contains the same text)."""
    out = []
    for p in procs_matching(f"gate.py {token}"):
        try:
            argv = Path(f"/proc/{p}/cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if argv and Path(argv[0].decode(errors="replace")).name.startswith("python") and len(argv) > 2 and argv[1].endswith(b"gate.py"):
            out.append(p)
    return out


def alive(pid: int) -> bool:
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return False
    return stat.rsplit(")", 1)[1].split()[0] != "Z"


def kill_all(pids) -> None:
    for p in pids:
        kill_quietly(p, signal.SIGKILL)


def rss_kib(pid: int) -> int:
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1])
    return 0


def fd_count(pid: int) -> int:
    return len(os.listdir(f"/proc/{pid}/fd"))


def sluice_daemon_pids(sb: Sandbox) -> list[int]:
    """Daemon processes whose environment points at this sandbox's runtime dir."""
    needle = f"XDG_RUNTIME_DIR={sb.runtime}".encode()
    out = []
    for p in procs_matching("daemon"):
        try:
            env = Path(f"/proc/{p}/environ").read_bytes().split(b"\0")
            argv = Path(f"/proc/{p}/cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if needle in env and len(argv) > 1 and Path(argv[0].decode(errors="replace")).name.startswith("sluice") and argv[1] == b"daemon":
            out.append(p)
    return out


class Cleanup:
    """Final census: nothing of the scenario may be left running."""

    def __init__(self) -> None:
        self.tokens: list[str] = []
        self.sandboxes: list[Sandbox] = []
        self.extra_pids: set[int] = set()

    def leftovers(self) -> dict:
        left: dict = {}
        for t in self.tokens:
            pids = [p for p in procs_matching(t) if alive(p)]
            if pids:
                left[t] = pids
        for sb in self.sandboxes:
            pids = [p for p in sluice_daemon_pids(sb) if alive(p)]
            if pids:
                left[f"daemon:{sb.runtime}"] = pids
        gone = sorted(p for p in self.extra_pids if alive(p))
        if gone:
            left["tracked"] = gone
        return left

    def settle(self, timeout: float = 15.0) -> dict:
        """Stop every daemon, wait for the census to come out empty, and kill
        whatever is still there (reported by the caller as a failure)."""
        for sb in self.sandboxes:
            try:
                sb.stop_daemon(sb.root, force=True)
            except Exception:  # noqa: BLE001
                pass
        end = time.monotonic() + timeout
        left = self.leftovers()
        while left and time.monotonic() < end:
            time.sleep(0.2)
            left = self.leftovers()
        for pids in left.values():
            kill_all(pids)
        return left


def require_testbeds() -> None:
    if not TB.name or not (TB / "py-itsdangerous").is_dir():
        raise SystemExit("set ALPHA_TESTBEDS (see scripts/alpha/README.md)")
    if not PYLIB.name or not PYLIB.is_dir():
        raise SystemExit("set ALPHA_PYLIB (see scripts/alpha/README.md)")


def which_real(cmd: str) -> str:
    found = shutil.which(cmd)
    if not found:
        raise SystemExit(f"{cmd} not found")
    return found
