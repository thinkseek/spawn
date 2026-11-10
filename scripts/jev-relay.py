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
  JEV_RELAY_ADDRESS     default 127.0.0.1:8790; a bare port (":8790" or
                        "8790") means host 127.0.0.1
  JEV_RELAY_TIMEOUT_SECS  default 12: the whole hosted call, connect to the
                        last byte; past it the relay answers 502 itself. It
                        stays under the judge client's timeout (spawn-jev
                        `JevConfig`, 15 s), so the client reads that 502
                        and its reason instead of hanging up first (which
                        left every 502 a BrokenPipe in this log). Measured
                        2026-11-07..10: 500 answers, p99 3.5 s, max 8.5 s.
  https_proxy           honored; the hosted API answers 451 from some regions
"""

import json
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

KEY = os.environ.get("TYPESAFE_API_KEY", "")
BASE = os.environ.get("TYPESAFE_BASE_URL", "https://api.typesafe.ai").rstrip("/")
HOST, _, PORT = os.environ.get("JEV_RELAY_ADDRESS", "127.0.0.1:8790").rpartition(":")
# An empty host would bind every interface; a bare port means loopback.
HOST = HOST or "127.0.0.1"
PATH = "/v1/systemone"
DEFAULT_TIMEOUT_SECS = 12


def timeout_from(text):
    """JEV_RELAY_TIMEOUT_SECS as seconds (1 to 300), or None when it is not
    one: the relay then refuses to start rather than guess."""
    try:
        value = float(text)
    except (TypeError, ValueError):
        return None
    return value if 1 <= value <= 300 else None


TIMEOUT = timeout_from(os.environ.get("JEV_RELAY_TIMEOUT_SECS", str(DEFAULT_TIMEOUT_SECS)))
# How much of an error's text the log keeps, on one line.
ERROR_CHARS = 300
# A judgement request is a few kilobytes of evidence; refuse anything larger.
MAX_BODY = 256 * 1024


def error_text(payload):
    """An error's text for the log: one line, at most ERROR_CHARS, with the
    key and any bearer token taken out. Never a header: only the body or
    the exception's own words reach it."""
    if isinstance(payload, bytes):
        payload = payload.decode("utf-8", "replace")
    text = " ".join(str(payload).split())
    if KEY:
        text = text.replace(KEY, "[key]")
    text = re.sub(r"(?i)bearer\s+\S+", "Bearer [redacted]", text)
    return text[:ERROR_CHARS]


def forward(body):
    """POST `body` to the hosted API: (status, payload, error), `error` the
    reason of a failure the relay itself answers 502 for, else None."""
    request = urllib.request.Request(
        BASE + PATH,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {KEY}",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
            return response.status, response.read(), None
    except urllib.error.HTTPError as error:
        # Its status stands even when its body cannot be read (cut short,
        # reset, timed out): the reason then says so.
        try:
            return error.code, error.read(), None
        except Exception as cut:  # noqa: BLE001 - any failure of that read
            reason = (
                f"relay: the hosted API answered {error.code} and its body could not be read "
                f"({error_text(cut) or type(cut).__name__})"
            )
            return error.code, json.dumps({"error": reason}).encode(), reason
    except Exception as error:  # network, TLS, a socket timeout
        reason = f"relay: {error_text(error) or type(error).__name__}"
        return 502, json.dumps({"error": reason}).encode(), reason


def forward_caught(body):
    """forward(), whatever it raises: a worker that died would leave call()
    to report the deadline, a false reason, at once."""
    try:
        return forward(body)
    except Exception as error:  # noqa: BLE001 - the worker must always answer
        reason = f"relay: {type(error).__name__}: {error_text(error)}"
        return 502, json.dumps({"error": reason}).encode(), reason


def call(body):
    """forward() within TIMEOUT for the whole call. urlopen's timeout bounds
    each socket operation, so an upstream that sends a byte now and then
    could hold the relay far longer; past the deadline the relay answers
    502 and leaves the call to end on its own (a daemon thread)."""
    result = []
    worker = threading.Thread(target=lambda: result.append(forward_caught(body)), daemon=True)
    worker.start()
    worker.join(TIMEOUT)
    if result:
        return result[0]
    reason = f"relay: the hosted API did not answer within {TIMEOUT:g} s"
    return 502, json.dumps({"error": reason}).encode(), reason


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
        started = time.monotonic()
        status, payload, reason = call(body)
        elapsed = (time.monotonic() - started) * 1000
        line = f"jev-relay: {status} {elapsed:.0f}ms {len(body)}B"
        if status >= 400:
            # Every failure says why: the relay's own reason, or the hosted
            # API's answer (its body, never its headers).
            line += f": {reason or error_text(payload) or '(no body)'}"
        sys.stderr.write(line + "\n")
        try:
            self._reply(status, payload)
        except (BrokenPipeError, ConnectionResetError):
            sys.stderr.write(
                f"jev-relay: the client hung up before the {status} after {elapsed:.0f}ms\n"
            )

    def log_message(self, *_args):
        pass


def not_dumpable():
    """Makes this process not dumpable (prctl PR_SET_DUMPABLE=0): another
    process of this user can no longer read its environment, where the
    key lives. A child it execs is dumpable again (the kernel resets the
    flag at execve), so nothing else changes. Best effort: a kernel without
    prctl leaves the relay as it was."""
    try:
        import ctypes

        ctypes.CDLL(None, use_errno=True).prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE
    except Exception:  # noqa: BLE001 - a relay must start whatever the kernel says
        pass


def main():
    not_dumpable()  # its environment holds the key
    if not KEY:
        sys.exit("jev-relay: TYPESAFE_API_KEY is not set")
    if TIMEOUT is None:
        sys.exit("jev-relay: JEV_RELAY_TIMEOUT_SECS must be a number of seconds from 1 to 300")
    # Whoever reaches the relay spends the key, so it serves loopback only.
    if HOST not in ("127.0.0.1", "::1", "[::1]", "localhost"):
        sys.exit(f"jev-relay: refusing to listen on {HOST}; use 127.0.0.1")
    server = ThreadingHTTPServer((HOST, int(PORT)), Relay)
    sys.stderr.write(
        f"jev-relay: http://{HOST}:{PORT}{PATH} -> {BASE}{PATH} (timeout {TIMEOUT:g} s)\n"
    )
    server.serve_forever()


if __name__ == "__main__":
    main()
