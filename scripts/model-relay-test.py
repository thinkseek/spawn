#!/usr/bin/env python3
"""Self-test of scripts/model-relay.py against fake upstreams on loopback.

Run: python3 -I scripts/model-relay-test.py   (no network, no keys)

Covers routing by model, header replacement (an allow-list toward DeepSeek),
server-sent events arriving before the upstream finishes (the gateway's), a
DeepSeek stream asked for again on a server error or silence before it starts and
ending in an overloaded error when cut, upstream errors,
an unreachable upstream, redirects passed back unfollowed, a stalled client
timed out, and the request size cap.
"""

import http.client
import socket
import importlib.util
import json
import os
import pathlib
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("model_relay", HERE / "model-relay.py")
relay = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(relay)

FAKE_KEY = "sk-test-deepseek"
CLIENT_TOKEN = "Bearer client-gateway-token"
# The fake upstream's whole stream.
WHOLE = b"event: message_start\ndata: {}\n\nevent: message_stop\ndata: {}\n\n"


class Upstream:
    """A fake model API on port 0 that records what reached it."""

    def __init__(self, name):
        self.name = name
        self.requests = []
        # Set by the test to let a held stream finish.
        self.release = threading.Event()
        self.finished = threading.Event()
        # Streams still to cut, and how: "close" drops the connection
        # partway, "end" ends the body cleanly without message_stop.
        self.cuts = []
        # Requests still to refuse with a 500, as a loaded DeepSeek does.
        self.failures = 0
        # Requests still to hold, silent, for 3 s before dropping them.
        self.silences = 0
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
                if request.get("redirect"):
                    self.send_response(request.get("redirect_status", 302))
                    self.send_header("Location", request["redirect"])
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                elif request.get("unauthorized"):
                    self._json(401, {"type": "error", "error": {"type": "authentication_error"}})
                elif request.get("stream") and upstream.silences:
                    upstream.silences -= 1
                    time.sleep(3)
                    self.close_connection = True
                elif request.get("stream") and upstream.failures:
                    upstream.failures -= 1
                    self._json(500, {"type": "error", "error": {"type": "api_error"}})
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
                if upstream.cuts:
                    if upstream.cuts.pop(0) == "end":
                        self.wfile.write(b"0\r\n\r\n")
                        self.wfile.flush()
                    self.close_connection = True
                    return
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

    def start_relay(
        self, upstream, deepseek, max_body=4096, client_timeout=10, every_model_as=""
    ):
        settings = relay.config(
            {
                "DEEPSEEK_API_KEY": FAKE_KEY,
                "DEEPSEEK_ANTHROPIC_URL": deepseek,
                "MODEL_RELAY_UPSTREAM": upstream,
                "MODEL_RELAY_ADDRESS": "127.0.0.1:0",
                "MODEL_RELAY_MAX_BODY": str(max_body),
                "MODEL_RELAY_CLIENT_TIMEOUT": str(client_timeout),
                "MODEL_RELAY_EVERY_MODEL_AS": every_model_as,
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

    def test_every_model_goes_to_deepseek_as_its_model_when_set(self):
        sending = self.start_relay(
            self.gateway.url, self.deepseek.url + "/anthropic", every_model_as="deepseek-v4-pro"
        )
        request = {
            "model": "claude-opus-5-5",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "héllo"}],
        }
        for path in ("/v1/messages", "/v1/messages/count_tokens"):
            status, payload = self.post(path, request, server=sending)
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(payload)["from"], "deepseek")
        status, _ = self.post(
            "/v1/messages", {"model": "deepseek-flash", "max_tokens": 1}, server=sending
        )
        self.assertEqual(status, 200)
        self.assertEqual(self.gateway.requests, [])
        sent = [json.loads(seen["body"]) for seen in self.deepseek.requests]
        self.assertEqual(
            [body["model"] for body in sent],
            ["deepseek-v4-pro", "deepseek-v4-pro", "deepseek-flash"],
        )
        # Only the model is rewritten here: no assistant message to mend.
        self.assertEqual(sent[0], {**request, "model": "deepseek-v4-pro"})
        for seen in self.deepseek.requests:
            self.assertEqual(seen["headers"]["x-api-key"], FAKE_KEY)
            self.assertNotIn("authorization", seen["headers"])
        # A request without a model still has nowhere else to go.
        connection = self.connect(sending)
        connection.request("GET", "/v1/models", headers={"Authorization": CLIENT_TOKEN})
        connection.getresponse().read()
        connection.close()
        self.assertEqual([seen["path"] for seen in self.gateway.requests], ["/v1/models"])

    def test_assistant_messages_get_the_thinking_block_deepseek_needs(self):
        empty = {"type": "thinking", "thinking": "", "signature": ""}
        signed = {"type": "thinking", "thinking": "Run ls.", "signature": "sig"}
        call = {
            "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}
        }
        result = {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"}
        request = {
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "user", "content": "List the files."},
                {"role": "assistant", "content": [{"type": "text", "text": "Listing."}, call]},
                {"role": "user", "content": [result]},
                {"role": "assistant", "content": "Done."},
                {"role": "user", "content": "Again."},
                {
                    "role": "assistant",
                    "content": [{"type": "redacted_thinking", "data": "x"}, call],
                },
                {"role": "user", "content": [result]},
                {"role": "assistant", "content": [signed, call]},
                {"role": "user", "content": [result]},
            ],
        }
        sent = json.loads(relay.as_model(json.dumps(request).encode(), "deepseek-v4-pro"))
        self.assertEqual(sent["model"], "deepseek-v4-pro")
        messages = sent["messages"]
        self.assertEqual(
            messages[1]["content"], [empty, {"type": "text", "text": "Listing."}, call]
        )
        self.assertEqual(messages[3]["content"], [empty, {"type": "text", "text": "Done."}])
        self.assertEqual(messages[5]["content"], [empty, call])
        self.assertEqual(messages[7]["content"], [signed, call])
        # The user's messages pass as they are.
        for index in (0, 2, 4, 6, 8):
            self.assertEqual(messages[index], request["messages"][index])

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

    def test_a_gateway_stream_arrives_event_by_event(self):
        connection = self.connect()
        body = json.dumps({"model": "claude-x", "stream": True}).encode()
        connection.request(
            "POST", "/v1/messages", body=body, headers={"Content-Type": "application/json"}
        )
        response = connection.getresponse()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
        first = response.read1(65536)
        # The upstream still holds the rest of the stream.
        self.assertIn(b"message_start", first)
        self.assertFalse(self.gateway.finished.is_set())
        self.gateway.release.set()
        rest = response.read()
        self.assertIn(b"message_stop", first + rest)
        self.assertTrue(self.gateway.finished.is_set())
        connection.close()

    def stream(self, model="deepseek-flash", server=None):
        connection = self.connect(server)
        body = json.dumps({"model": model, "stream": True}).encode()
        connection.request(
            "POST", "/v1/messages", body=body, headers={"Content-Type": "application/json"}
        )
        return connection, connection.getresponse()

    def fast_retries(self):
        for name, value in (("STREAM_BACKOFF", (0, 0, 0, 0)), ("DEEPSEEK_SILENCE", 1)):
            self.addCleanup(setattr, relay, name, getattr(relay, name))
            setattr(relay, name, value)

    def test_a_deepseek_stream_arrives_event_by_event(self):
        connection, response = self.stream()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
        first = response.read1(65536)
        # The upstream still holds the rest: the client already has the start.
        self.assertIn(b"message_start", first)
        self.assertFalse(self.deepseek.finished.is_set())
        self.deepseek.release.set()
        self.assertEqual(first + response.read(), WHOLE)
        connection.close()

    def test_a_cut_deepseek_stream_ends_with_an_overloaded_error(self):
        for cut in ("close", "end"):
            self.deepseek.cuts = [cut]
            connection, response = self.stream()
            self.assertEqual(response.status, 200)
            events = response.read()
            connection.close()
            start, error = events.split(b"event: error\ndata: ")
            self.assertEqual(start, b"event: message_start\ndata: {}\n\n")
            self.assertEqual(json.loads(error)["error"]["type"], "overloaded_error", cut)
        self.assertEqual(len(self.deepseek.requests), 2)

    def test_a_deepseek_stream_refused_with_a_server_error_is_asked_for_again(self):
        self.fast_retries()
        self.deepseek.release.set()
        self.deepseek.failures = 2
        connection, response = self.stream()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.read(), WHOLE)
        connection.close()
        self.assertEqual(len(self.deepseek.requests), 3)

    def test_a_silent_deepseek_is_asked_again(self):
        self.fast_retries()
        self.deepseek.release.set()
        self.deepseek.silences = 1
        connection, response = self.stream()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.read(), WHOLE)
        connection.close()
        self.assertEqual(len(self.deepseek.requests), 2)

    def test_a_deepseek_stream_refused_for_its_request_passes_as_it_is(self):
        connection = self.connect()
        body = json.dumps({"model": "deepseek-flash", "stream": True, "unauthorized": True})
        connection.request(
            "POST", "/v1/messages", body=body, headers={"Content-Type": "application/json"}
        )
        response = connection.getresponse()
        self.assertEqual(response.status, 401)
        self.assertEqual(json.loads(response.read())["error"]["type"], "authentication_error")
        connection.close()
        self.assertEqual(len(self.deepseek.requests), 1)

    def test_a_deepseek_stream_failing_every_time_is_an_overloaded_529(self):
        self.fast_retries()
        self.deepseek.failures = relay.STREAM_ATTEMPTS
        connection, response = self.stream()
        self.assertEqual(response.status, 529)
        self.assertEqual(json.loads(response.read())["error"]["type"], "overloaded_error")
        connection.close()
        self.assertEqual(len(self.deepseek.requests), relay.STREAM_ATTEMPTS)

    def test_deepseek_gets_only_the_messages_api_headers(self):
        extra = {"Cookie": "session=gateway", "X-Gateway-Tenant": "tenant-secret"}
        status, _ = self.post(
            "/v1/messages", {"model": "deepseek-flash"},
            headers={**extra, "anthropic-beta": "beta-1"},
        )
        self.assertEqual(status, 200)
        (seen,) = self.deepseek.requests
        self.assertNotIn("cookie", seen["headers"])
        self.assertNotIn("x-gateway-tenant", seen["headers"])
        self.assertNotIn("authorization", seen["headers"])
        # What urllib adds itself: the hop's own headers.
        urllib_own = {"host", "content-length", "connection", "accept-encoding"}
        self.assertLessEqual(
            set(seen["headers"]), relay.DEEPSEEK_HEADERS | {"x-api-key"} | urllib_own
        )
        self.assertEqual(seen["headers"]["x-api-key"], FAKE_KEY)
        self.assertEqual(seen["headers"]["anthropic-beta"], "beta-1")
        # The gateway still gets the client's headers unchanged.
        self.post("/v1/messages", {"model": "claude-x"}, headers=extra)
        (seen,) = self.gateway.requests
        self.assertEqual(seen["headers"]["cookie"], "session=gateway")
        self.assertEqual(seen["headers"]["x-gateway-tenant"], "tenant-secret")

    def test_a_redirect_is_passed_back_not_followed(self):
        collector = Upstream("collector")
        self.addCleanup(collector.close)
        for model, upstream in (("deepseek-flash", self.deepseek), ("claude-x", self.gateway)):
            for code in (301, 302, 303, 307, 308):
                status, _ = self.post(
                    "/v1/messages",
                    {"model": model, "redirect": collector.url + "/steal", "redirect_status": code},
                )
                self.assertEqual(status, code, (model, code))
        self.assertEqual(collector.requests, [], "a credential reached the redirect target")
        self.assertEqual(len(self.deepseek.requests), 5)
        self.assertEqual(len(self.gateway.requests), 5)

    def test_a_stalled_client_is_timed_out(self):
        server = self.start_relay(self.gateway.url, self.deepseek.url, client_timeout=0.5)
        with socket.create_connection(("127.0.0.1", server.server_port), timeout=5) as client:
            # Half a request, then nothing: the relay must give the thread up.
            client.sendall(b"POST /v1/messages HTTP/1.1\r\nContent-Length: 100\r\n\r\n{")
            client.settimeout(5)
            try:
                closed = client.recv(1) == b""
            except ConnectionResetError:
                closed = True
            except socket.timeout:
                closed = False
        self.assertTrue(closed, "the relay still holds a client that stopped sending")
        self.assertEqual(self.gateway.requests, [])

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
        self.assertIsNone(
            relay.refusal(relay.config({**good, "MODEL_RELAY_EVERY_MODEL_AS": "deepseek-v4-pro"}))
        )
        for change in (
            {"DEEPSEEK_API_KEY": ""},
            {"MODEL_RELAY_ADDRESS": "0.0.0.0:8791"},
            {"MODEL_RELAY_EVERY_MODEL_AS": "claude-opus-5-5"},
        ):
            self.assertIsNotNone(relay.refusal(relay.config({**good, **change})))

if __name__ == "__main__":
    unittest.main(verbosity=2)
