"""Smart HTTP git server for the merge-queue transport tests.

`git http-backend` behind the standard library's `http.server`, run as a
CGI program by hand (the stdlib CGI handler is deprecated). The config file
(re-read on every request, so a test can change it on the fly) is
`{"users": {name: {"password": ..., "mode": ...}}, "anonymous_read": bool}`.
Pushes always need HTTP basic auth, reads only without `anonymous_read`.
A user's mode is `ok`, or `stall`: any `git-receive-pack` request of that
user sleeps for ten minutes without answering (a remote that accepts the
connection and stalls the push; fetches and `ls-remote` still work).

Usage: git_http_server.py <project-root> <config.json>
Prints the bound port on stdout, then serves until killed. Exits by itself
when its parent (the test process) goes away, so a crashed test never
leaves it behind.
"""

import base64
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = sys.argv[1]


def config():
    with open(sys.argv[2], encoding="utf-8") as f:
        return json.load(f)


def watchdog(parent):
    while True:
        time.sleep(0.5)
        if os.getppid() != parent:
            os._exit(0)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # no request log (it is never needed)
        pass

    def user(self, users):
        header = self.headers.get("Authorization", "")
        if not header.startswith("Basic "):
            return None
        try:
            name, _, password = base64.b64decode(header[6:]).decode().partition(":")
        except ValueError:
            return None
        entry = users.get(name)
        if entry is None or entry["password"] != password:
            return None
        return name

    def body(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = bytearray()
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip(), 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    return bytes(data)
                data += self.rfile.read(size)
                self.rfile.readline()
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def reply(self, code, headers, payload):
        self.send_response(code)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def handle_git(self):
        path, _, query = self.path.partition("?")
        data = self.body()
        cfg = config()
        users = cfg["users"]
        name = self.user(users)
        push = "git-receive-pack" in self.path
        if name is None and (push or not cfg.get("anonymous_read")):
            self.reply(
                401,
                [("WWW-Authenticate", 'Basic realm="sluice-test"'), ("Content-Type", "text/plain")],
                b"authentication required\n",
            )
            return
        if push and users[name]["mode"] == "stall":
            time.sleep(600)
            return
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "GIT_PROJECT_ROOT": ROOT,
            "GIT_HTTP_EXPORT_ALL": "1",
            "GATEWAY_INTERFACE": "CGI/1.1",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "REQUEST_METHOD": self.command,
            "PATH_INFO": path,
            "QUERY_STRING": query,
            "REMOTE_ADDR": "127.0.0.1",
            "CONTENT_TYPE": self.headers.get("Content-Type", ""),
            "CONTENT_LENGTH": str(len(data)),
        }
        if name is not None:
            env["REMOTE_USER"] = name
        for header in ("Content-Encoding", "Git-Protocol"):
            if self.headers.get(header):
                env["HTTP_" + header.upper().replace("-", "_")] = self.headers[header]
        out = subprocess.run(
            ["git", "http-backend"], input=data, env=env, capture_output=True, check=False
        ).stdout
        head, sep, payload = out.partition(b"\r\n\r\n")
        if not sep:
            head, sep, payload = out.partition(b"\n\n")
        code, headers = 200, []
        for line in head.decode("latin-1").splitlines():
            key, _, value = line.partition(":")
            if key.lower() == "status":
                code = int(value.strip().split()[0])
            elif key:
                headers.append((key.strip(), value.strip()))
        self.reply(code, headers, payload)

    do_GET = handle_git
    do_POST = handle_git


def main():
    threading.Thread(target=watchdog, args=(os.getppid(),), daemon=True).start()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    print(server.server_address[1], flush=True)
    server.serve_forever()


main()
