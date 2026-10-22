#!/usr/bin/env python3
"""Transparent recording proxy for the Sluice daemon protocol (stdlib only).

Listens on a Unix socket, forwards every connection to the real daemon
socket and appends every protocol line, in both directions, to an ndjson
transcript:

    {"conn": 1, "dir": "c2s", "msg": {...}}     client to server
    {"conn": 1, "dir": "s2c", "msg": {...}}     server to client

`msg` is the decoded JSON object of the line (a plain string when the line
is not JSON). Lines are written in arrival order; nothing is rewritten or
normalised here (the generator does that afterwards, see generate.sh).

Usage: record-proxy.py --listen PROXY.sock --upstream REAL.sock --out FILE

Run it with `python3 -I` so nothing is imported from the current directory.
It exits on SIGTERM or SIGINT.
"""

import argparse
import json
import os
import signal
import socket
import sys
import threading


class Recorder:
    def __init__(self, path):
        self._file = open(path, "a", encoding="utf-8")
        self._lock = threading.Lock()

    def line(self, conn, direction, raw):
        text = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        try:
            msg = json.loads(text)
        except ValueError:
            msg = text
        record = {"conn": conn, "dir": direction, "msg": msg}
        with self._lock:
            self._file.write(json.dumps(record, separators=(",", ":")) + "\n")
            self._file.flush()

    def close(self):
        with self._lock:
            self._file.close()


def pump(src, dst, conn, direction, recorder):
    """Copy bytes src -> dst, recording each complete line."""
    pending = b""
    try:
        while True:
            chunk = src.recv(65536)
            if not chunk:
                break
            dst.sendall(chunk)
            pending += chunk
            while b"\n" in pending:
                one, pending = pending.split(b"\n", 1)
                recorder.line(conn, direction, one)
    except OSError:
        pass
    finally:
        if pending:
            recorder.line(conn, direction, pending)
        for sock, how in ((dst, socket.SHUT_WR), (src, socket.SHUT_RD)):
            try:
                sock.shutdown(how)
            except OSError:
                pass


def serve_connection(client, conn, upstream_path, recorder):
    upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        upstream.connect(upstream_path)
    except OSError as err:
        sys.stderr.write(f"record-proxy: cannot reach {upstream_path}: {err}\n")
        client.close()
        return
    up = threading.Thread(
        target=pump, args=(client, upstream, conn, "c2s", recorder), daemon=True
    )
    down = threading.Thread(
        target=pump, args=(upstream, client, conn, "s2c", recorder), daemon=True
    )
    up.start()
    down.start()
    up.join()
    down.join()
    client.close()
    upstream.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--listen", required=True)
    parser.add_argument("--upstream", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    if os.path.exists(args.listen):
        os.unlink(args.listen)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(args.listen)
    os.chmod(args.listen, 0o600)
    server.listen(64)
    recorder = Recorder(args.out)

    def stop(_signum, _frame):
        try:
            server.close()
        finally:
            recorder.close()
            if os.path.exists(args.listen):
                os.unlink(args.listen)
            os._exit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    conn = 0
    while True:
        try:
            client, _ = server.accept()
        except OSError:
            break
        conn += 1
        threading.Thread(
            target=serve_connection,
            args=(client, conn, args.upstream, recorder),
            daemon=True,
        ).start()


if __name__ == "__main__":
    main()
