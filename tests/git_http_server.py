#!/usr/bin/env python3
"""Minimal smart-HTTP git server for offline differential testing.

Wraps git-http-backend (the same CGI git itself uses to serve http://) behind
http.server, so the shim talks to a real git server rather than a mock.
"""
import base64
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.environ["GIT_HTTP_ROOT"]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        if os.environ.get("GIT_HTTP_VERBOSE"):
            sys.stderr.write("[git-http] " + (fmt % args) + "\n")

    def _auth(self):
        user = os.environ.get("GIT_HTTP_USER")
        want = os.environ.get("GIT_HTTP_PASS")
        if not user:
            return True
        header = self.headers.get("Authorization", "")
        if not header.startswith("Basic "):
            return False
        decoded = base64.b64decode(header[6:]).decode("utf-8", "replace")
        return decoded == f"{user}:{want}"

    def _deny(self):
        self.send_response(401)
        self.send_header("WWW-Authenticate", 'Basic realm="git"')
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _cgi(self):
        if not self._auth():
            self._deny()
            return
        path, _, query = self.path.partition("?")
        env = dict(os.environ)
        env.update(
            {
                "GIT_PROJECT_ROOT": ROOT,
                "GIT_HTTP_EXPORT_ALL": "1",
                "REQUEST_METHOD": self.command,
                "QUERY_STRING": query,
                "PATH_INFO": path,
                "REMOTE_USER": os.environ.get("GIT_HTTP_USER", "tester"),
                "REMOTE_ADDR": self.client_address[0],
                "CONTENT_TYPE": self.headers.get("Content-Type", ""),
                "CONTENT_LENGTH": self.headers.get("Content-Length", "0"),
                "GIT_PROTOCOL": self.headers.get("Git-Protocol", ""),
                "HTTP_CONTENT_ENCODING": self.headers.get("Content-Encoding", ""),
            }
        )
        length = int(self.headers.get("Content-Length", "0") or 0)
        body = self.rfile.read(length) if length else b""
        proc = subprocess.run(
            ["git", "http-backend"], input=body, capture_output=True, env=env
        )
        if proc.returncode != 0:
            self.send_response(500)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        head, _, payload = proc.stdout.partition(b"\r\n\r\n")
        status = 200
        headers = []
        for line in head.split(b"\r\n"):
            if not line:
                continue
            key, _, value = line.decode("utf-8", "replace").partition(":")
            key, value = key.strip(), value.strip()
            if key.lower() == "status":
                status = int(value.split()[0])
            else:
                headers.append((key, value))
        self.send_response(status)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_GET = _cgi
    do_POST = _cgi


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
