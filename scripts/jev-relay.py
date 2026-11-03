#!/usr/bin/env python3
"""Local relay from Spawn's plain-HTTP Jev client to the hosted TypeSafe API.

`spawn-jev` speaks HTTP/1.1 over a std socket by design: no TLS, no auth
header. The hosted service needs both. This relay listens on loopback, adds
`Authorization: Bearer $TYPESAFE_API_KEY`, and forwards `POST /v1/systemone`
over HTTPS. Only that one path is forwarded.

The key stays in this process. It never enters Spawn's environment, the
event log, or the agent's terminal.

Environment:
  TYPESAFE_API_KEY      required
  TYPESAFE_BASE_URL     default https://api.typesafe.ai
  JEV_RELAY_ADDRESS     default 127.0.0.1:8790
  https_proxy           honored; the hosted API answers 451 from some regions
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

KEY = os.environ.get("TYPESAFE_API_KEY", "")
BASE = os.environ.get("TYPESAFE_BASE_URL", "https://api.typesafe.ai").rstrip("/")
HOST, _, PORT = os.environ.get("JEV_RELAY_ADDRESS", "127.0.0.1:8790").rpartition(":")
PATH = "/v1/systemone"
TIMEOUT = 20
# A judgement request is a few kilobytes of evidence; refuse anything larger.
MAX_BODY = 256 * 1024


class Relay(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _reply(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/healthz":
            self._reply(200, b'{"ok":true}')
        else:
            self._reply(404, b'{"error":"not found"}')

    def do_POST(self):
        if self.path != PATH:
            self._reply(404, b'{"error":"only POST /v1/systemone is relayed"}')
            return
        try:
            length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            self._reply(400, b'{"error":"Content-Length is required"}')
            return
        if length <= 0 or length > MAX_BODY:
            self._reply(413, b'{"error":"body is empty or too large"}')
            return
        body = self.rfile.read(length)
        if len(body) != length:
            self._reply(400, b'{"error":"body is shorter than Content-Length"}')
            return
        request = urllib.request.Request(
            BASE + PATH,
            data=body,
            method="POST",
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {KEY}",
            },
        )
        started = time.monotonic()
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
                status, payload = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, payload = error.code, error.read()
        except Exception as error:  # network, TLS, timeout
            status = 502
            payload = json.dumps({"error": f"relay: {error}"}).encode()
        elapsed = (time.monotonic() - started) * 1000
        sys.stderr.write(f"jev-relay: {status} {elapsed:.0f}ms {len(body)}B\n")
        self._reply(status, payload)

    def log_message(self, *_args):
        pass


def main():
    if not KEY:
        sys.exit("jev-relay: TYPESAFE_API_KEY is not set")
    # Whoever reaches the relay spends the key, so it serves loopback only.
    # An empty host (":8790") would bind every interface.
    if HOST not in ("127.0.0.1", "::1", "[::1]", "localhost"):
        sys.exit(f"jev-relay: refusing to listen on {HOST or 'every interface'}; use 127.0.0.1")
    server = ThreadingHTTPServer((HOST, int(PORT)), Relay)
    sys.stderr.write(f"jev-relay: http://{HOST}:{PORT}{PATH} -> {BASE}{PATH}\n")
    server.serve_forever()


if __name__ == "__main__":
    main()
