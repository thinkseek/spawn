#!/usr/bin/env python3
"""Local relay from Spawn's plain-HTTP Jev client to the hosted TypeSafe API.

`spawn-jev` speaks HTTP/1.1 over a std socket by design: no TLS, no auth
header. The hosted service needs both. This relay listens on loopback, adds
`Authorization: Bearer $TYPESAFE_API_KEY`, and forwards `POST /v1/systemone`
over HTTPS. Only that one path is forwarded.

The key stays in this process. It never enters Spawn's environment, the
event log, or the agent's terminal.

The key (objective 201): under systemd's LoadCredential, the unit's
credential `typesafe.api-key` (Keep's /run/keep/typesafe.api-key, copied by
systemd into the unit's own $CREDENTIALS_DIRECTORY), and only that: with a
credentials directory and no readable, non-empty `typesafe.api-key` in it,
the relay refuses to start and says which. Without a credentials directory
(the unit before its move), TYPESAFE_API_KEY.

Environment:
  CREDENTIALS_DIRECTORY set by systemd for a unit with LoadCredential=
  TYPESAFE_API_KEY      the key when there is no credentials directory
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

# The key's name as a credential: Keep's file name, LoadCredential's name.
CREDENTIAL = "typesafe.api-key"
# The exit status when systemd gives a credentials directory but the
# credential in it is no key (unreadable, not UTF-8 or empty): the unit lists
# it in RestartPreventExitStatus beside 243, so such a start stops once,
# saying why, as a missing file does, rather than restarting every 3 s.
CREDENTIAL_EXIT = 78  # EX_CONFIG


def key_from(environ):
    """(the API key, why there is none): the credential CREDENTIAL in
    $CREDENTIALS_DIRECTORY when systemd gives the relay one (objective 201),
    and nothing else then; else TYPESAFE_API_KEY. A why names the
    credential or the variable, never a value. The key is the file's text
    as Keep writes it (the raw value, no newline); one trailing newline, as
    a file written by hand ends, is taken off, and nothing else."""
    directory = environ.get("CREDENTIALS_DIRECTORY", "")
    if directory:
        try:
            with open(os.path.join(directory, CREDENTIAL), "rb") as credential:
                key = credential.read().decode("utf-8")
            key = key[:-1] if key.endswith("\n") else key
        except OSError as error:
            return "", f"the credential {CREDENTIAL} cannot be read ({error.strerror})"
        except UnicodeDecodeError:
            return "", f"the credential {CREDENTIAL} is not UTF-8 text"
        if not key:
            return "", f"the credential {CREDENTIAL} is empty"
        return key, None
    key = environ.get("TYPESAFE_API_KEY", "")
    if not key:
        return "", f"TYPESAFE_API_KEY is not set, and there is no credential {CREDENTIAL}"
    return key, None




def exit_status(environ):
    """The status the relay exits with when it has no key: CREDENTIAL_EXIT
    under a credentials directory (the unit stays stopped), else 1."""
    return CREDENTIAL_EXIT if environ.get("CREDENTIALS_DIRECTORY", "") else 1


KEY, KEY_MISSING = key_from(os.environ)
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
    """An error's text for the log and for the caller: one line, at most
    ERROR_CHARS, with the key and any bearer token taken out. Never a header:
    only the body or the exception's own words reach it.
    The exact key, and its JSON-escaped forms, go first, before anything
    changes the text's whitespace (201 R5, cc-astra A86: a key with a space at
    either end no longer matched once the text was joined into one line).
    Then its words joined by one space, as the line now is: what of the key a
    text holds without its spaces."""
    if isinstance(payload, bytes):
        payload = payload.decode("utf-8", "replace")
    text = str(payload)
    if KEY:
        escaped = json.dumps(KEY)[1:-1]
        # A JSON writer may also escape "/" as "\/" (rev-e's n2).
        for form in (KEY, escaped, escaped.replace("/", "\\/")):
            text = text.replace(form, "[key]")
    text = " ".join(text.split())
    words = " ".join(KEY.split())
    if words:
        text = text.replace(words, "[key]")
    text = re.sub(r"(?i)bearer\s+\S+", "Bearer [redacted]", text)
    return text[:ERROR_CHARS]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """The key goes to BASE only (201 R6, rev-e's n1): urllib copies the
    Authorization header to any host a 301, 302 or 303 names, and turns the
    POST into a GET. So no redirect is followed: it is the hosted API's
    status, answered as any error status is (forward)."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(NoRedirect)


def forward(body):
    """POST `body` to the hosted API: (status, payload, error). An answer
    passes through (error None); for any failure, the hosted API's error
    status included, the payload is the relay's own {"error": error}."""
    request = urllib.request.Request(
        BASE + PATH,
        data=body,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    # Never copied to a redirect's request either (201 R6: a second guard
    # beside NoRedirect).
    request.add_unredirected_header("Authorization", f"Bearer {KEY}")
    try:
        with OPENER.open(request, timeout=TIMEOUT) as response:
            return response.status, response.read(), None
    except urllib.error.HTTPError as error:
        # Its status stands even when its body cannot be read (cut short,
        # reset, timed out): the reason then says so. Its body never goes back
        # as it came (201 R5, cc-astra A85: one echoing the key would hand it
        # to the caller, which puts it in its own error): the caller gets the
        # relay's own payload, the status and the body's text as error_text
        # leaves it, as the log has it.
        try:
            text = error_text(error.read())
            reason = f"relay: the hosted API answered {error.code}: {text or '(no body)'}"
            return error.code, json.dumps({"error": reason}).encode(), reason
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
            # Every failure says why: the relay's reason, for the hosted
            # API's error status its body's text as error_text leaves it
            # (never its headers), the same the caller gets.
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
    process of this user can no longer read its memory or its environment,
    where the key lives. A child it execs is dumpable again (the kernel resets the
    flag at execve), so nothing else changes. Best effort: a kernel without
    prctl leaves the relay as it was."""
    try:
        import ctypes

        ctypes.CDLL(None, use_errno=True).prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE
    except Exception:  # noqa: BLE001 - a relay must start whatever the kernel says
        pass


def main():
    not_dumpable()  # its memory (and, without a credential, its environment) holds the key
    if not KEY:
        sys.stderr.write(f"jev-relay: {KEY_MISSING}\n")
        sys.exit(exit_status(os.environ))
    if TIMEOUT is None:
        sys.exit("jev-relay: JEV_RELAY_TIMEOUT_SECS must be a number of seconds from 1 to 300")
    # Whoever reaches the relay spends the key, so it serves loopback only.
    if HOST not in ("127.0.0.1", "::1", "[::1]", "localhost"):
        sys.exit(f"jev-relay: refusing to listen on {HOST}; use 127.0.0.1")
    server = ThreadingHTTPServer((HOST, int(PORT)), Relay)
    # The port it listens on, as bound: with port 0 the system's pick, which
    # a test reads from this line instead of guessing a free one (201 R6').
    port = server.server_address[1]
    sys.stderr.write(
        f"jev-relay: http://{HOST}:{port}{PATH} -> {BASE}{PATH} (timeout {TIMEOUT:g} s)\n"
    )
    sys.stderr.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
