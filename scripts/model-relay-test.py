#!/usr/bin/env python3
"""Self-test of scripts/model-relay.py against fake upstreams on loopback.

Run: python3 -I scripts/model-relay-test.py   (no network, no keys)

Covers routing by model, header replacement, server-sent events arriving
before the upstream finishes, upstream errors, an unreachable upstream, and
the request size cap.
"""

import http.client
import importlib.util
import json
import os
import pathlib
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("model_relay", HERE / "model-relay.py")
relay = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(relay)

FAKE_KEY = "sk-test-deepseek"
CLIENT_TOKEN = "Bearer client-gateway-token"


class Upstream:
    """A fake model API on port 0 that records what reached it."""

    def __init__(self, name):
        self.name = name
        self.requests = []
        # Set by the test to let a held stream finish.
        self.release = threading.Event()
        self.finished = threading.Event()
        upstream = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def _record(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                upstream.requests.append(
                    {
                        "method": self.command,
                        "path": self.path,
                        "headers": {k.lower(): v for k, v in self.headers.items()},
                        "body": body,
                    }
                )
                return body

            def do_GET(self):
                self._record()
                self._json(200, {"data": [{"id": "fake-model"}], "from": upstream.name})

            def do_POST(self):
                body = self._record()
                try:
                    request = json.loads(body or b"{}")
                except ValueError:
                    request = {}
                if request.get("unauthorized"):
                    self._json(401, {"type": "error", "error": {"type": "authentication_error"}})
                elif request.get("stream"):
                    self._stream()
                else:
                    self._json(200, {"model": request.get("model"), "from": upstream.name})

            def _json(self, status, value):
                data = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def _stream(self):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()

                def chunk(data):
                    self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
                    self.wfile.flush()

                chunk(b"event: message_start\ndata: {}\n\n")
                # Hold the rest until the client has the first event.
                upstream.release.wait(10)
                chunk(b"event: message_stop\ndata: {}\n\n")
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
                upstream.finished.set()

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(
            target=self.server.serve_forever, args=(0.05,), daemon=True
        ).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class RelayTest(unittest.TestCase):
    def setUp(self):
        # Loopback only: a proxy from the caller's shell must not intercept.
        for name in list(os.environ):
            if name.lower() in ("http_proxy", "https_proxy", "all_proxy", "no_proxy"):
                del os.environ[name]
        self.gateway = Upstream("gateway")
        self.deepseek = Upstream("deepseek")
        self.relay = self.start_relay(self.gateway.url, self.deepseek.url + "/anthropic")

    def start_relay(self, upstream, deepseek, max_body=4096):
        settings = relay.config(
            {
                "DEEPSEEK_API_KEY": FAKE_KEY,
                "DEEPSEEK_ANTHROPIC_URL": deepseek,
                "MODEL_RELAY_UPSTREAM": upstream,
                "MODEL_RELAY_ADDRESS": "127.0.0.1:0",
                "MODEL_RELAY_MAX_BODY": str(max_body),
            }
        )
        server = relay.serve(settings)
        threading.Thread(target=server.serve_forever, args=(0.05,), daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server

    def tearDown(self):
        self.gateway.release.set()
        self.deepseek.release.set()
        self.gateway.close()
        self.deepseek.close()

    def connect(self, server=None):
        server = server or self.relay
        return http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=10)

    def post(self, path, body, headers=None, server=None):
        connection = self.connect(server)
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        connection.request(
            "POST",
            path,
            body=data,
            headers={
                "Content-Type": "application/json",
                "Authorization": CLIENT_TOKEN,
                "x-api-key": "client-key",
                "anthropic-version": "2023-06-01",
                **(headers or {}),
            },
        )
        response = connection.getresponse()
        payload = response.read()
        connection.close()
        return response.status, payload

    def test_claude_models_go_to_the_gateway_with_the_clients_auth(self):
        status, payload = self.post("/v1/messages", {"model": "claude-opus-5-5", "max_tokens": 1})
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(payload)["from"], "gateway")
        self.assertEqual(self.deepseek.requests, [])
        (seen,) = self.gateway.requests
        self.assertEqual(seen["path"], "/v1/messages")
        self.assertEqual(seen["headers"]["authorization"], CLIENT_TOKEN)
        self.assertEqual(seen["headers"]["x-api-key"], "client-key")
        self.assertEqual(seen["headers"]["anthropic-version"], "2023-06-01")
        self.assertNotIn(FAKE_KEY, json.dumps(seen["headers"]))

    def test_deepseek_models_go_to_deepseek_with_its_key_only(self):
        for path in ("/v1/messages", "/v1/messages/count_tokens"):
            status, payload = self.post(path, {"model": "deepseek-flash", "max_tokens": 1})
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(payload)["from"], "deepseek")
        self.assertEqual(self.gateway.requests, [])
        paths = [seen["path"] for seen in self.deepseek.requests]
        self.assertEqual(paths, ["/anthropic/v1/messages", "/anthropic/v1/messages/count_tokens"])
        for seen in self.deepseek.requests:
            self.assertEqual(seen["headers"]["x-api-key"], FAKE_KEY)
            self.assertNotIn("authorization", seen["headers"])
            self.assertEqual(seen["headers"]["anthropic-version"], "2023-06-01")
            self.assertEqual(json.loads(seen["body"])["model"], "deepseek-flash")

    def test_requests_without_a_model_go_to_the_gateway(self):
        connection = self.connect()
        connection.request("GET", "/v1/models", headers={"Authorization": CLIENT_TOKEN})
        response = connection.getresponse()
        self.assertEqual(response.status, 200)
        self.assertEqual(json.loads(response.read())["from"], "gateway")
        connection.close()
        self.assertEqual(self.gateway.requests[0]["headers"]["authorization"], CLIENT_TOKEN)
        status, _ = self.post("/v1/messages", b"not json")
        self.assertEqual(status, 200)
        self.assertEqual(len(self.gateway.requests), 2)
        self.assertEqual(self.deepseek.requests, [])

    def test_a_stream_arrives_event_by_event(self):
        for upstream, model in ((self.deepseek, "deepseek-flash"), (self.gateway, "claude-x")):
            connection = self.connect()
            body = json.dumps({"model": model, "stream": True}).encode()
            connection.request(
                "POST", "/v1/messages", body=body, headers={"Content-Type": "application/json"}
            )
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
            first = response.read1(65536)
            # The upstream still holds the rest of the stream.
            self.assertIn(b"message_start", first)
            self.assertFalse(upstream.finished.is_set())
            upstream.release.set()
            rest = response.read()
            self.assertIn(b"message_stop", first + rest)
            self.assertTrue(upstream.finished.is_set())
            connection.close()

    def test_upstream_errors_pass_through(self):
        status, payload = self.post(
            "/v1/messages", {"model": "deepseek-flash", "unauthorized": True}
        )
        self.assertEqual(status, 401)
        self.assertEqual(json.loads(payload)["error"]["type"], "authentication_error")

    def test_an_unreachable_upstream_is_a_502_in_the_api_shape(self):
        closed = ThreadingHTTPServer(("127.0.0.1", 0), BaseHTTPRequestHandler)
        port = closed.server_port
        closed.server_close()
        server = self.start_relay(f"http://127.0.0.1:{port}", f"http://127.0.0.1:{port}")
        status, payload = self.post("/v1/messages", {"model": "claude-x"}, server=server)
        self.assertEqual(status, 502)
        error = json.loads(payload)
        self.assertEqual(error["type"], "error")
        self.assertIn("model relay", error["error"]["message"])

    def test_a_body_over_the_cap_is_refused_unsent(self):
        big = {"model": "deepseek-flash", "pad": "x" * 8192}
        status, payload = self.post("/v1/messages", big)
        self.assertEqual(status, 413)
        self.assertIn("model relay", json.loads(payload)["error"]["message"])
        self.assertEqual(self.deepseek.requests, [])
        self.assertEqual(self.gateway.requests, [])

    def test_only_v1_and_health_are_served(self):
        connection = self.connect()
        connection.request("GET", "/healthz")
        response = connection.getresponse()
        self.assertEqual(response.status, 200)
        response.read()
        connection.request("GET", "/admin")
        response = connection.getresponse()
        self.assertEqual(response.status, 404)
        response.read()
        connection.close()
        self.assertEqual(self.gateway.requests, [])

    def test_the_relay_refuses_to_start_off_loopback_or_without_a_key(self):
        good = {"DEEPSEEK_API_KEY": FAKE_KEY, "MODEL_RELAY_UPSTREAM": "http://gateway"}
        self.assertIsNone(relay.refusal(relay.config(good)))
        self.assertEqual(relay.config({**good, "MODEL_RELAY_ADDRESS": ":9"})["host"], "127.0.0.1")
        for change in (
            {"DEEPSEEK_API_KEY": ""},
            {"MODEL_RELAY_ADDRESS": "0.0.0.0:8791"},
        ):
            self.assertIsNotNone(relay.refusal(relay.config({**good, **change})))

if __name__ == "__main__":
    unittest.main(verbosity=2)
