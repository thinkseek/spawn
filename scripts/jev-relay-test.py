#!/usr/bin/env python3
"""Self-test of scripts/jev-relay.py against a fake hosted API on loopback.

Run: python3 -I scripts/jev-relay-test.py   (no network, no key)

Covers the relay's deadline and its failures: an upstream that stalls past
JEV_RELAY_TIMEOUT_SECS, and one that sends a byte now and then (each read
within the socket timeout, the whole call past it), are answered 502 by the
relay within the deadline, with the reason in the body and in the log; an
upstream 502 is passed back with its body (one whose body is cut short keeps
its status and says so, at once), an empty body logs "(no body)", and the log says that body's
text, never the key or a header; a client that hangs up first is one log
line, not a traceback; an answer passes through with the key added.
"""

import http.client
import importlib.util
import io
import json
import pathlib
import socket
import struct
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("jev_relay", HERE / "jev-relay.py")
relay = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(relay)

FAKE_KEY = "ts-test-key-0123456789"
BODY = json.dumps({"model": "jev", "question": "state?"}).encode()


class Upstream:
    """The hosted API, answering each POST as `behave` says; it records the
    requests it saw."""

    def __init__(self, behave):
        self.seen = []
        self.release = threading.Event()
        upstream = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                upstream.seen.append(
                    {"path": self.path, "headers": dict(self.headers), "body": self.rfile.read(length)}
                )
                behave(self, upstream)

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, args=(0.05,), daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


def answer(status, payload):
    def behave(handler, _upstream):
        data = json.dumps(payload).encode()
        handler.send_response(status)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(data)))
        handler.end_headers()
        handler.wfile.write(data)

    return behave


def answer_raw(status, data):
    def behave(handler, _upstream):
        handler.send_response(status)
        handler.send_header("Content-Length", str(len(data)))
        handler.end_headers()
        handler.wfile.write(data)

    return behave


def stall(handler, upstream):
    # Says nothing until the test ends.
    upstream.release.wait(30)


def drip(handler, upstream):
    # A header, then one byte of the body every 0.3 s: no read waits as
    # long as the socket timeout, the whole answer takes far longer.
    handler.send_response(200)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", "100")
    handler.end_headers()
    for _ in range(100):
        if upstream.release.wait(0.3):
            return
        try:
            handler.wfile.write(b" ")
            handler.wfile.flush()
        except OSError:
            return


def cut_502(handler, _upstream):
    # A 502 whose body is cut short: 5 of the 100 bytes it announces.
    handler.send_response(502)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", "100")
    handler.end_headers()
    handler.wfile.write(b'{"err')
    handler.wfile.flush()
    handler.close_connection = True


class RelayTest(unittest.TestCase):
    def setUp(self):
        self.log = io.StringIO()
        for name, value in (("KEY", FAKE_KEY), ("TIMEOUT", 1.0)):
            self.addCleanup(setattr, relay, name, getattr(relay, name))
            setattr(relay, name, value)
        self.addCleanup(setattr, relay.sys, "stderr", relay.sys.stderr)
        relay.sys.stderr = self.log

    def start(self, behave):
        upstream = Upstream(behave)
        self.addCleanup(upstream.close)
        self.addCleanup(setattr, relay, "BASE", relay.BASE)
        relay.BASE = upstream.url
        server = ThreadingHTTPServer(("127.0.0.1", 0), relay.Relay)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, args=(0.05,), daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return upstream, server.server_port

    def post(self, port, timeout=10):
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
        self.addCleanup(connection.close)
        connection.request("POST", relay.PATH, body=BODY, headers={"Content-Type": "application/json"})
        response = connection.getresponse()
        return response.status, response.read()

    def logged(self):
        return self.log.getvalue()

    def test_an_answer_passes_through_with_the_key_added(self):
        upstream, port = self.start(answer(200, {"answers": {}}))
        status, body = self.post(port)
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body), {"answers": {}})
        self.assertEqual(upstream.seen[0]["headers"].get("Authorization"), f"Bearer {FAKE_KEY}")
        self.assertEqual(upstream.seen[0]["body"], BODY)
        self.assertRegex(self.logged(), r"jev-relay: 200 \d+ms \d+B\n")

    def test_an_upstream_that_stalls_is_answered_502_within_the_deadline(self):
        _upstream, port = self.start(stall)
        started = time.monotonic()
        status, body = self.post(port)
        elapsed = time.monotonic() - started
        self.assertEqual(status, 502)
        self.assertLess(elapsed, 3.0, "the relay answers by its deadline (1 s here)")
        reason = json.loads(body)["error"]
        self.assertTrue(reason.startswith("relay: "), reason)
        self.assertRegex(self.logged(), r"jev-relay: 502 \d+ms \d+B: relay: \S")
        self.assertIn(reason, self.logged())

    def test_an_upstream_that_drips_is_cut_at_the_deadline(self):
        _upstream, port = self.start(drip)
        started = time.monotonic()
        status, body = self.post(port)
        elapsed = time.monotonic() - started
        self.assertEqual(status, 502)
        self.assertLess(elapsed, 3.0, "the deadline bounds the whole call, not each read")
        self.assertEqual(json.loads(body)["error"], "relay: the hosted API did not answer within 1 s")
        self.assertIn(": relay: the hosted API did not answer within 1 s", self.logged())

    def test_an_upstream_502_is_passed_back_and_its_text_logged(self):
        _upstream, port = self.start(
            answer(
                502,
                {
                    "error": "model backend unavailable",
                    "detail": f"echoed Bearer {FAKE_KEY}",
                    "query": f"api_key={FAKE_KEY}",
                },
            )
        )
        status, body = self.post(port)
        self.assertEqual(status, 502)
        self.assertEqual(json.loads(body)["error"], "model backend unavailable")
        log = self.logged()
        self.assertRegex(log, r"jev-relay: 502 \d+ms \d+B: .*model backend unavailable")
        self.assertNotIn(FAKE_KEY, log, "the key never reaches the log")
        self.assertIn("Bearer [redacted]", log)
        self.assertIn("api_key=[key]", log)
        self.assertNotIn("Content-Type", log, "nor a header")

    def test_a_502_whose_body_is_cut_keeps_its_status_and_says_so(self):
        _upstream, port = self.start(cut_502)
        started = time.monotonic()
        status, body = self.post(port)
        self.assertEqual(status, 502)
        self.assertLess(time.monotonic() - started, 0.9, "answered at once, not at the deadline")
        reason = json.loads(body)["error"]
        self.assertIn("answered 502 and its body could not be read", reason)
        self.assertNotIn("did not answer within", reason)
        log = self.logged()
        self.assertIn(": relay: the hosted API answered 502 and its body could not be read", log)
        self.assertNotIn("Traceback", log)

    def test_a_worker_that_raises_still_answers_with_its_reason(self):
        def broken(_body):
            raise RuntimeError("an unforeseen failure")

        self.addCleanup(setattr, relay, "forward", relay.forward)
        relay.forward = broken
        status, payload, reason = relay.call(BODY)
        self.assertEqual(status, 502)
        self.assertEqual(reason, "relay: RuntimeError: an unforeseen failure")
        self.assertEqual(json.loads(payload)["error"], reason)

    def test_an_empty_error_body_says_so(self):
        _upstream, port = self.start(answer_raw(503, b""))
        status, _body = self.post(port)
        self.assertEqual(status, 503)
        self.assertRegex(self.logged(), r"jev-relay: 503 \d+ms \d+B: \(no body\)\n")

    def test_an_unreachable_upstream_says_why(self):
        _upstream, port = self.start(stall)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            dead = probe.getsockname()[1]
        relay.BASE = f"http://127.0.0.1:{dead}"
        status, body = self.post(port)
        self.assertEqual(status, 502)
        self.assertIn("refused", json.loads(body)["error"].lower())
        self.assertRegex(self.logged(), r"jev-relay: 502 \d+ms \d+B: relay: .*[Rr]efused")

    def test_a_client_that_hangs_up_first_is_one_line(self):
        _upstream, port = self.start(stall)
        # The judge client giving up: it sends the request, then closes with a
        # reset (linger 0), so the relay's answer meets a closed socket.
        client = socket.create_connection(("127.0.0.1", port))
        client.sendall(
            f"POST {relay.PATH} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n"
            f"Content-Length: {len(BODY)}\r\n\r\n".encode() + BODY
        )
        time.sleep(0.3)
        client.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        client.close()
        deadline = time.monotonic() + 5
        while "hung up" not in self.logged() and time.monotonic() < deadline:
            time.sleep(0.05)
        log = self.logged()
        self.assertIn("jev-relay: the client hung up before the 502", log)
        self.assertNotIn("Traceback", log)

    def test_the_timeout_setting_is_checked(self):
        self.assertEqual(relay.timeout_from("12"), 12.0)
        self.assertEqual(relay.timeout_from("1"), 1.0)
        self.assertEqual(relay.timeout_from("300"), 300.0)
        for bad in ("0", "301", "-5", "soon", "", None):
            self.assertIsNone(relay.timeout_from(bad), bad)


if __name__ == "__main__":
    unittest.main()
