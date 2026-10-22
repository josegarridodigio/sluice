"""A scripted coding agent that talks to `sluice mcp` over stdio.

No language model is involved: the "agent" is this client sending the same
JSON-RPC messages an MCP host sends (initialize, tools/list, tools/call with
a progress token), so the harness exercises the real server path for free.
"""

from __future__ import annotations

import json
import subprocess
import threading
import time
from pathlib import Path


class McpAgent:
    def __init__(self, env: dict, cwd: Path, name: str = "agent") -> None:
        self.name = name
        self.proc = subprocess.Popen(
            ["sluice", "mcp"],
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            start_new_session=True,
        )
        self._next_id = 0
        self._lock = threading.Lock()
        self.progress: list[dict] = []
        self.stderr_lines: list[str] = []
        threading.Thread(target=self._drain_stderr, daemon=True).start()
        init = self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": f"alpha-harness-{name}", "version": "1"},
            },
        )
        self.server_info = init.get("result", {})
        self.notify("notifications/initialized", {})

    def _drain_stderr(self) -> None:
        for line in self.proc.stderr:
            self.stderr_lines.append(line.rstrip())

    def notify(self, method: str, params: dict) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _send(self, msg: dict) -> None:
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method: str, params: dict, timeout: float = 900) -> dict:
        with self._lock:
            self._next_id += 1
            rid = self._next_id
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"{self.name}: server closed stdout; stderr: {self.stderr_lines[-5:]}")
            msg = json.loads(line)
            if msg.get("method") == "notifications/progress":
                self.progress.append(msg.get("params", {}))
                continue
            if "method" in msg and "id" in msg:
                # Server-to-client request (ping, roots/list): answer empty.
                self._send({"jsonrpc": "2.0", "id": msg["id"], "result": {}})
                continue
            if msg.get("id") == rid:
                return msg
        raise TimeoutError(f"{self.name}: no answer to {method} in {timeout}s")

    def tools(self) -> list[str]:
        return [t["name"] for t in self.request("tools/list", {})["result"]["tools"]]

    def call(self, tool: str, args: dict, progress: bool = True, timeout: float = 900) -> dict:
        params: dict = {"name": tool, "arguments": args}
        if progress:
            params["_meta"] = {"progressToken": f"{self.name}-{time.monotonic_ns()}"}
        msg = self.request("tools/call", params, timeout=timeout)
        if "error" in msg:
            return {"is_error": True, "text": json.dumps(msg["error"]), "structured": None}
        res = msg["result"]
        text = " ".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
        return {"is_error": bool(res.get("isError")), "text": text, "structured": res.get("structuredContent")}

    def verify(self, workdir: Path, gates: list[str] | None = None, timeout: float = 900) -> dict:
        args: dict = {"working_directory": str(workdir)}
        if gates:
            args["gates"] = gates
        return self.call("verify", args, timeout=timeout)

    def close(self) -> None:
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except Exception:  # noqa: BLE001
            self.proc.kill()
