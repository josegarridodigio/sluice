#!/usr/bin/env python3
"""Helpers of scripts/compat/generate.sh (stdlib only; run with `python3 -I`).

Subcommands:

  hello SOCKET              send a raw protocol-1 `hello`, print the reply line
  mcp-tools BIN             `initialize` + `tools/list` over stdio, print the
                            `tools/list` result as JSON
  cli-help BIN              `--help` of every subcommand, concatenated
  normalise FILE KEY=VAL..  replace volatile absolute paths in a transcript
  extract-config MD KIND    print the first toml block under the repository
                            ("repo") or user ("user") config heading of a
                            config-format.md read from stdin-like file MD
  expected-repo TOML...     expected gate tables (documented defaults applied)
  expected-user TOML...     explicitly set scalars of user configs, flattened
  inputs MARKERS_DIR TOML REPO  inputs.json entries recomputed independently
  bad-markers MARKERS_DIR OUT   corrupted copies of one real marker
  checkpoint DB             WAL checkpoint (TRUNCATE) and journal cleanup
"""

import hashlib
import json
import os
import re
import socket
import sqlite3
import subprocess
import sys
import tomllib


def cmd_hello(sock_path):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(sock_path)
    hello = {"type": "hello", "protocol": 1, "client": "compat-raw 0", "pid": os.getpid()}
    s.sendall((json.dumps(hello, separators=(",", ":")) + "\n").encode())
    buf = b""
    while b"\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            break
        buf += chunk
    s.close()
    sys.stdout.write(buf.decode())


def cmd_mcp_tools(binary):
    proc = subprocess.Popen(
        [binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )

    def send(msg):
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    def request(rid, method, params):
        send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            line = proc.stdout.readline()
            if not line:
                raise SystemExit("mcp server closed stdout")
            msg = json.loads(line)
            if msg.get("id") == rid and "method" not in msg:
                return msg

    request(
        1,
        "initialize",
        {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "compat-generate", "version": "1"},
        },
    )
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    tools = request(2, "tools/list", {})["result"]
    proc.stdin.close()
    proc.wait(timeout=10)
    json.dump(tools, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


def run_help(binary, path):
    out = subprocess.run(
        [binary, *path, "--help"], capture_output=True, text=True, check=True
    ).stdout
    return out


def subcommands(help_text):
    names, inside = [], False
    for line in help_text.splitlines():
        if line.startswith("Commands:"):
            inside = True
            continue
        if inside:
            if not line.strip():
                break
            m = re.match(r"^  ([a-z][a-z0-9-]*)\s", line + " ")
            if m and m.group(1) != "help":
                names.append(m.group(1))
    return names


def cmd_cli_help(binary):
    chunks = []

    def walk(path):
        text = run_help(binary, path)
        title = " ".join(["sluice", *path])
        chunks.append(f"===== {title} =====\n{text.rstrip()}\n")
        for name in subcommands(text):
            walk([*path, name])

    walk([])
    sys.stdout.write("\n".join(chunks))


def replace_in(value, pairs):
    if isinstance(value, str):
        for old, new in pairs:
            value = value.replace(old, new)
        return value
    if isinstance(value, list):
        return [replace_in(v, pairs) for v in value]
    if isinstance(value, dict):
        return {replace_in(k, pairs): replace_in(v, pairs) for k, v in value.items()}
    return value


def cmd_normalise(path, maps):
    pairs = []
    for item in maps:
        old, new = item.split("=", 1)
        pairs.append((old, new))
    pairs.sort(key=lambda p: -len(p[0]))
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            rec["msg"] = replace_in(rec["msg"], pairs)
            out.append(json.dumps(rec, separators=(",", ":")))
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + ("\n" if out else ""))


def cmd_extract_config(md_path, kind):
    heading = "## Repository config" if kind == "repo" else "## User config"
    text = open(md_path, encoding="utf-8").read()
    start = text.index(heading)
    block = re.search(r"```toml\n(.*?)```", text[start:], re.S)
    sys.stdout.write(block.group(1))


def secs(text):
    total = 0.0
    for num, unit in re.findall(r"(\d+(?:\.\d+)?)\s*(ms|s|m|h|d)", text):
        total += float(num) * {"ms": 0.001, "s": 1, "m": 60, "h": 3600, "d": 86400}[unit]
    return total


def cmd_expected_repo(paths):
    out = {}
    for path in paths:
        doc = tomllib.load(open(path, "rb"))
        defaults = doc.get("defaults", {})
        gates = {}
        for name, g in doc.get("gates", {}).items():
            gates[name] = {
                "cmd": g["cmd"],
                "weight": g.get("weight", 1),
                "timeout_secs": secs(g["timeout"]) if "timeout" in g else 1800.0,
                "hooks": g.get("hooks", []),
                "key_inputs": g.get("key_inputs", []),
                "key_files": g.get("key_files", []),
                "env": g.get("env", {}),
                "depends_on": g.get("depends_on", []),
                "cache": g.get("cache", True),
            }
        entry = {
            "version": doc["version"],
            "defaults": {
                "agent_hook_policy": defaults.get("agent_hook_policy", "fail-fast"),
                "human_hook_policy": defaults.get("human_hook_policy", "wait"),
                "tail_lines": defaults.get("tail_lines"),
            },
            "gates": gates,
        }
        if "queue" in doc:
            q = doc["queue"]
            entry["queue"] = {
                "remote": q.get("remote", "origin"),
                "target": q.get("target"),
                "gates": q.get("gates"),
                "max_train": q.get("max_train", 4),
                "audit_ref": q.get("audit_ref", True),
                "push_timeout_secs": secs(q.get("push_timeout", "10m")),
                "network_timeout_secs": secs(q.get("network_timeout", "60s")),
            }
        out[os.path.basename(path)] = entry
    json.dump(out, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


DURATION_KEYS = {
    "daemon.idle_exit",
    "queue.tick",
    "queue.retention",
    "admission.psi_poll_interval",
    "admission.psi_max_wait",
    "mcp.progress_interval",
    "scheduler.head_reserve_after",
}


def flatten(prefix, value, out):
    if isinstance(value, dict) and prefix != "agent_detection.env_any_equals":
        for k, v in value.items():
            if prefix == "repos":
                flatten(f"repos|{k}", v, out)
            elif prefix.startswith("repos|"):
                flatten(f"{prefix}|{k}", v, out)
            else:
                flatten(f"{prefix}.{k}" if prefix else k, v, out)
    else:
        key = prefix
        if key in DURATION_KEYS:
            out[key + "_secs"] = secs(value)
        else:
            out[key] = value


def cmd_expected_user(paths):
    out = {}
    for path in paths:
        text = open(path, "rb").read()
        flat = {}
        flatten("", tomllib.loads(text.decode()), flat)
        out[os.path.basename(path)] = flat
    json.dump(out, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def cmd_inputs(markers_dir, config_toml, repo_dir):
    """Describe every marker and miss record of MARKERS_DIR from the gate
    definitions of CONFIG_TOML, hashing key inputs and files from REPO_DIR."""
    gates = tomllib.load(open(config_toml, "rb"))["gates"]
    entries = []
    names = sorted(os.listdir(markers_dir))
    # Miss records carry no tree; their key was derived from the tree of the
    # fixture repository, which is the tree of its plain (non-hook) markers.
    repo_tree = None
    for name in names:
        rec = json.load(open(os.path.join(markers_dir, name)))
        if rec.get("tree") and rec.get("committed_tree") is None:
            repo_tree = rec["tree"]
            break
    for name in names:
        full = os.path.join(markers_dir, name)
        rec = json.load(open(full))
        gate = gates[rec["gate"]]
        inputs = []
        for c in gate.get("key_inputs", []):
            out = subprocess.run(
                ["sh", "-c", c], cwd=repo_dir, capture_output=True, check=True
            ).stdout
            inputs.append([c, sha(out)])
        files = []
        for rel in gate.get("key_files", []):
            p = os.path.join(repo_dir, rel)
            files.append([rel, sha(open(p, "rb").read()) if os.path.exists(p) else "absent"])
        entries.append(
            {
                "file": name,
                "key": rec["key"],
                "tree": rec.get("tree") or repo_tree,
                "gate": rec["gate"],
                "cmd": " ".join(gate["cmd"].split()),
                "env": dict(sorted(gate.get("env", {}).items())),
                "key_inputs": inputs,
                "key_files": files,
            }
        )
    json.dump(entries, sys.stdout, indent=2)
    sys.stdout.write("\n")


def cmd_bad_markers(markers_dir, out_dir):
    pick = None
    for name in sorted(os.listdir(markers_dir)):
        if name.endswith(".json") and not name.endswith(".miss.json"):
            pick = name
            break
    body = json.load(open(os.path.join(markers_dir, pick)))
    os.makedirs(out_dir, exist_ok=True)
    key = body["key"]
    other = ("0" * 63 + "1") if key != "0" * 63 + "1" else ("0" * 63 + "2")
    cases = {}

    def put(case, content, file_key):
        fname = f"{case}.json"
        json.dump(content, open(os.path.join(out_dir, fname), "w"), indent=2)
        cases[case] = {"body": fname, "file_key": file_key}

    put("format-2", {**body, "format": 2}, key)
    put("unknown-field", {**body, "added_in_a_future_release": 1}, key)
    put("wrong-key", body, other)
    with open(os.path.join(out_dir, "cases.json"), "w") as f:
        json.dump(cases, f, indent=2, sort_keys=True)
        f.write("\n")


def cmd_checkpoint(db):
    con = sqlite3.connect(db)
    con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    con.close()
    for suffix in ("-wal", "-shm"):
        if os.path.exists(db + suffix):
            os.unlink(db + suffix)


def main(argv):
    cmd, args = argv[1], argv[2:]
    {
        "hello": lambda: cmd_hello(*args),
        "mcp-tools": lambda: cmd_mcp_tools(*args),
        "cli-help": lambda: cmd_cli_help(*args),
        "normalise": lambda: cmd_normalise(args[0], args[1:]),
        "extract-config": lambda: cmd_extract_config(*args),
        "expected-repo": lambda: cmd_expected_repo(args),
        "expected-user": lambda: cmd_expected_user(args),
        "inputs": lambda: cmd_inputs(*args),
        "bad-markers": lambda: cmd_bad_markers(*args),
        "checkpoint": lambda: cmd_checkpoint(*args),
    }[cmd]()


if __name__ == "__main__":
    main(sys.argv)
