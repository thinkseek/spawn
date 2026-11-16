#!/usr/bin/env python3
"""Synthetic tests for presence_upload.py. No request leaves this machine: a
made-up receiver runs on a free local port (plain HTTP) and the uploader's
connection is pointed at it by the test. Every token here is a TEST-ONLY
value, which the production path refuses. Run: python3 -I test_presence_upload.py"""
import fcntl
import http.client
import http.server
import json
import os
import shutil
import stat
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import agent_presence as ap  # noqa: E402
import presence_upload as up  # noqa: E402

TOKEN = "TEST-ONLY-0123456789abcdefghijKLMNOPQRstuv"
REAL_LOOKING = "Zq7rT2mX9vB4nL8kP3wY6cH1sD5fG0jUeAoIiR"   # never used against anything


class Receiver:
    def __init__(self):
        self.status, self.requests, self.headers = 204, [], {}
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                n = int(self.headers.get("Content-Length", "0"))
                outer.requests.append({"path": self.path, "headers": dict(self.headers),
                                       "body": self.rfile.read(n)})
                self.send_response(outer.status)
                for k, v in outer.headers.items():
                    self.send_header(k, v)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"no")

            def do_GET(self):
                outer.requests.append({"path": self.path, "method": "GET"})
                self.send_response(500)
                self.end_headers()

            def log_message(self, *a):
                pass

        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), H)
        self.port = self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def connect(self):
        return http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


def document(now, agents=None, epoch="0123456789abcdef"):
    agents = agents if agents is not None else [{"id": "a1", "role": "worker", "state": "idle", "evidence": "observed"}]
    return {"version": "1", "generated_utc": up.utc(now), "source_last_ok_utc": up.utc(now),
            "source_status": "ok", "stale_after_s": 60, "id_epoch": epoch,
            "provenance": {"adapter": "spawn-agent-presence", "version": "1"}, "agents": agents}


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.rx = Receiver()
        conf = os.path.join(self.tmp, "conf")
        state = os.path.join(self.tmp, "state")
        os.makedirs(conf, mode=0o700)
        os.makedirs(state, mode=0o700)
        self.cfg = {"enable_file": os.path.join(conf, "upload.json"),
                    "token_file": os.path.join(conf, "ingest-token"),
                    "disable_file": os.path.join(conf, "disabled"), "state_dir": state}
        self.write(self.cfg["enable_file"], '{"enabled": true}')
        self.write(self.cfg["token_file"], TOKEN + "\n", 0o600)
        self.now = time.time()
        self.put(document(self.now))

    def tearDown(self):
        self.rx.close()
        shutil.rmtree(self.tmp)

    def write(self, path, text, mode=0o600):
        with open(path, "w") as f:
            f.write(text)
        os.chmod(path, mode)

    def put(self, doc):
        self.doc_bytes = (json.dumps(doc, separators=(",", ":"), sort_keys=True) + "\n").encode()
        with open(os.path.join(self.cfg["state_dir"], "presence.json"), "wb") as f:
            f.write(self.doc_bytes)

    def run_once(self, now=None, **kw):
        kw.setdefault("allow_test_token", True)
        return up.upload(self.cfg, connect=self.rx.connect, now=now if now is not None else self.now, **kw)


class Off(Base):
    def test_off_without_the_enable_file(self):
        os.unlink(self.cfg["enable_file"])
        self.assertEqual(self.run_once(), "off")
        self.assertEqual(self.rx.requests, [])

    def test_enable_file_must_say_exactly_enabled_true(self):
        for text in ('{"enabled": "yes"}', '{"enabled": true, "url": "https://x"}', "true", "garbage", "",
                     '{"enabled": 1}', '{"enabled": 1.0}', '{"enabled": false, "enabled": true}',
                     '{"enabled": true, "enabled": true}', '[{"enabled": true}]', '{"Enabled": true}'):
            self.write(self.cfg["enable_file"], text)
            self.assertEqual(self.run_once(), "off", text)
        self.assertEqual(self.rx.requests, [])

    def test_exactly_true_turns_it_on(self):
        for text in ('{"enabled": true}', '{"enabled":true}\n', ' { "enabled" : true } '):
            self.write(self.cfg["enable_file"], text)
            self.assertTrue(up.enabled(self.cfg), text)

    def bounded_run(self):
        """One run in a daemon thread, joined for at most 2 s: a run that
        blocks on a FIFO fails the test instead of hanging it (rev-e n1)."""
        out = []
        t = threading.Thread(target=lambda: out.append(self.run_once()), daemon=True)
        t.start()
        t.join(2)
        self.assertFalse(t.is_alive(), "the run blocked on a FIFO")
        return out[0]

    def test_a_fifo_cannot_hold_the_run(self):
        os.unlink(self.cfg["token_file"])
        os.mkfifo(self.cfg["token_file"], 0o600)
        self.assertEqual(self.bounded_run(), "token_refused")
        os.unlink(self.cfg["token_file"])
        self.write(self.cfg["token_file"], TOKEN, 0o600)
        p = os.path.join(self.cfg["state_dir"], "presence.json")
        os.unlink(p)
        os.mkfifo(p, 0o600)
        self.assertEqual(self.bounded_run(), "no_document")
        self.assertEqual(self.rx.requests, [])

    def test_exporter_off_switch_also_stops_it(self):
        self.write(self.cfg["disable_file"], "")
        self.assertEqual(self.run_once(), "disabled")
        self.assertEqual(self.rx.requests, [])


class Token(Base):
    def test_production_refuses_test_and_placeholder_tokens(self):
        for t in (TOKEN, "example-" + REAL_LOOKING, REAL_LOOKING + "placeholder", "changeme" + REAL_LOOKING,
                  "DUMMY" + REAL_LOOKING):
            self.assertFalse(up.token_ok(t), t)
        self.assertTrue(up.token_ok(REAL_LOOKING))
        self.write(self.cfg["token_file"], TOKEN, 0o600)
        self.assertEqual(self.run_once(allow_test_token=False), "token_refused")
        self.assertEqual(self.rx.requests, [])

    def test_test_mode_accepts_only_test_tokens(self):
        self.assertFalse(up.token_ok(REAL_LOOKING, allow_test_token=True))
        self.assertTrue(up.token_ok(TOKEN, allow_test_token=True))

    def test_weak_or_odd_tokens(self):
        for t in ("short", "a" * 40, "TEST-ONLY-" + "ab" * 20, REAL_LOOKING + " x", REAL_LOOKING + "é", "x" * 300):
            self.assertFalse(up.token_ok(t) and up.token_ok(t, True), t)

    def test_token_file_must_be_private_regular_and_ours(self):
        self.write(self.cfg["token_file"], TOKEN, 0o640)
        self.assertEqual(self.run_once(), "token_refused")
        os.unlink(self.cfg["token_file"])
        target = os.path.join(self.tmp, "elsewhere")
        self.write(target, TOKEN, 0o600)
        os.symlink(target, self.cfg["token_file"])
        self.assertEqual(self.run_once(), "token_refused")
        os.unlink(self.cfg["token_file"])
        self.write(self.cfg["token_file"], TOKEN + "x" * 600, 0o600)
        self.assertEqual(self.run_once(), "token_refused")
        self.assertEqual(self.rx.requests, [])


class Send(Base):
    def test_sends_exactly_the_document_bytes_once(self):
        self.assertEqual(self.run_once(), "sent")
        self.assertEqual(len(self.rx.requests), 1)
        r = self.rx.requests[0]
        self.assertEqual(r["path"], up.PATH)
        self.assertEqual(r["body"], self.doc_bytes)
        h = {k.lower(): v for k, v in r["headers"].items()}
        self.assertEqual(h["authorization"], "Bearer " + TOKEN)
        self.assertEqual(h["content-type"], "application/json")
        self.assertEqual(int(h["content-length"]), len(self.doc_bytes))
        self.assertNotIn("origin", h)
        self.assertNotIn("?", r["path"])
        self.assertEqual(self.run_once(now=self.now + 20), "already_sent")
        self.assertEqual(len(self.rx.requests), 1)

    def test_a_newer_document_is_sent_and_order_is_strict(self):
        self.assertEqual(self.run_once(), "sent")
        self.put(document(self.now + 15))
        self.assertEqual(self.run_once(now=self.now + 16), "sent")
        self.put(document(self.now + 5))                       # older than the last sent
        self.assertEqual(self.run_once(now=self.now + 32), "already_sent")
        self.assertEqual(len(self.rx.requests), 2)

    def test_at_most_one_attempt_per_min_interval(self):
        self.rx.status = 500
        self.assertEqual(self.run_once(), "server_error")
        self.put(document(self.now + 1))
        self.assertEqual(self.run_once(now=self.now + 1), "backoff")
        self.assertEqual(len(self.rx.requests), 1)

    def test_too_soon_after_a_success(self):
        # rev-e R2 F1: the 10 s rate holds after a success too, where no
        # back-off can hide it.
        self.assertEqual(self.run_once(), "sent")
        self.put(document(self.now + 5))
        self.assertEqual(self.run_once(now=self.now + 5), "too_soon")
        self.assertEqual(len(self.rx.requests), 1)
        self.assertEqual(self.run_once(now=self.now + 11), "sent")
        self.assertEqual(len(self.rx.requests), 2)
        self.assertEqual(self.rx.requests[1]["body"], self.doc_bytes)

    def put_raw(self, text):
        with open(os.path.join(self.cfg["state_dir"], "presence.json"), "wb") as f:
            f.write(text.encode())

    def test_duplicate_keys_never_leave(self):
        # cc-astra A91: raw bytes, since json.dumps of a dict cannot keep a
        # duplicate. In each case the LAST value is fully valid.
        good = json.dumps(document(self.now), separators=(",", ":"), sort_keys=True)
        prov = '{"adapter":"spawn-agent-presence","version":"1"}'
        cases = [
            good.replace('"role":"worker"', '"role":"PRIVATE_MARKER","role":"worker"'),
            good.replace('"agents":[', '"agents":[{"id":"PRIVATE_MARKER"}],"agents":['),
            good.replace('"provenance":' + prov,
                         '"provenance":{"path":"/home/PRIVATE_MARKER"},"provenance":' + prov),
            good.replace('"stale_after_s":60,"version":"1"}', '"stale_after_s":60,"version":"PRIVATE_MARKER","version":"1"}'),
        ]
        for text in cases:
            self.assertEqual(text.count("PRIVATE_MARKER"), 1, text)
            self.assertNotEqual(text, good)
            self.put_raw(text)
            self.assertEqual(self.run_once(), "invalid_document", text)
        self.assertEqual(self.rx.requests, [])
        self.put_raw(good)                      # the same document without the duplicate is sent
        self.assertEqual(self.run_once(), "sent")
        self.assertEqual(self.rx.requests[0]["body"], good.encode())

    def test_old_future_and_invalid_documents_are_not_sent(self):
        self.put(document(self.now - up.MAX_AGE_S - 1))
        self.assertEqual(self.run_once(), "too_old")
        self.put(document(self.now + 60))
        self.assertEqual(self.run_once(), "invalid_document")
        bad = document(self.now)
        bad["agents"][0]["name"] = "agent_worker_secret"
        self.put(bad)
        self.assertEqual(self.run_once(), "invalid_document")
        with open(os.path.join(self.cfg["state_dir"], "presence.json"), "wb") as f:
            f.write(b"x" * (up.MAX_DOC_BYTES + 1))
        self.assertEqual(self.run_once(), "invalid_document")
        os.unlink(os.path.join(self.cfg["state_dir"], "presence.json"))
        self.assertEqual(self.run_once(), "no_document")
        self.assertEqual(self.rx.requests, [])

    def test_a_linked_document_is_not_read(self):
        p = os.path.join(self.cfg["state_dir"], "presence.json")
        other = os.path.join(self.tmp, "other.json")
        shutil.move(p, other)
        os.symlink(other, p)
        self.assertEqual(self.run_once(), "no_document")
        self.assertEqual(self.rx.requests, [])

    def test_the_id_map_and_lock_never_leave(self):
        with open(os.path.join(self.cfg["state_dir"], "ids.json"), "w") as f:
            f.write('{"epoch": "0123456789abcdef", "ids": {"%s": 1}, "next": 2}' % ("f" * 32))
        self.run_once()
        body = self.rx.requests[0]["body"]
        self.assertNotIn(b"ffffffff", body)
        self.assertEqual(body, self.doc_bytes)


class Responses(Base):
    def expect(self, status, word, backoff, marks_sent):
        self.rx.status = status
        self.assertEqual(self.run_once(), word, status)
        st = up.load_state(os.path.join(self.cfg["state_dir"], "upload-state.json"))
        self.assertAlmostEqual(st["backoff_until"] - self.now, backoff, delta=0.01)
        self.assertEqual(st["last_sent_utc"] is not None, marks_sent, status)

    def test_words_and_backoff(self):
        cases = [(204, "sent", 0, True), (409, "replay_or_old", 0, True), (401, "unauthorized", 600, False),
                 (429, "rate_limited", 30, False), (503, "server_error", 30, False), (422, "rejected", 300, False),
                 (413, "rejected", 300, False), (302, "rejected", 300, False)]
        for status, word, backoff, sent in cases:
            os.path.exists(os.path.join(self.cfg["state_dir"], "upload-state.json")) and \
                os.unlink(os.path.join(self.cfg["state_dir"], "upload-state.json"))
            self.expect(status, word, backoff, sent)

    def test_a_redirect_is_never_followed(self):
        self.rx.status = 302
        self.rx.headers = {"Location": "http://example.invalid/steal"}
        self.assertEqual(self.run_once(), "rejected")
        self.assertEqual(len(self.rx.requests), 1)
        self.assertNotIn("method", self.rx.requests[0])

    def test_network_error(self):
        self.assertEqual(up.upload(self.cfg, connect=lambda: http.client.HTTPConnection("127.0.0.1", 1, timeout=1),
                                   allow_test_token=True, now=self.now), "network_error")

    def test_busy(self):
        fd = os.open(os.path.join(self.cfg["state_dir"], "upload.lock"), os.O_WRONLY | os.O_CREAT, 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            self.assertEqual(self.run_once(), "busy")
        finally:
            os.close(fd)


class Static(unittest.TestCase):
    def test_fixed_https_target_and_no_secret_paths(self):
        src = open(up.__file__).read()
        code = src.split('"""', 2)[2]
        self.assertEqual((up.HOST, up.PORT, up.PATH), ("www.flora.chat", 443, "/observe3/ingest/agent-presence"))
        self.assertIn("ssl.create_default_context()", code)
        for bad in ("check_hostname = False", "CERT_NONE", "_create_unverified_context", "subprocess", "os.system",
                    "os.environ", "getenv(", "print(token", "urllib.request", "requests", "?token", "eval(", "exec("):
            self.assertFalse(bad in code, bad)
        self.assertEqual(code.count('conn.request("POST"'), 1)
        self.assertNotIn("allow_test_token=True", code)      # only the tests turn test tokens on

    def test_https_connection_verifies(self):
        c = up.https_connect()
        self.assertTrue(c._context.check_hostname)
        self.assertEqual(c._context.verify_mode, __import__("ssl").CERT_REQUIRED)
        self.assertEqual((c.host, c.port), ("www.flora.chat", 443))

    def test_main_never_allows_test_tokens(self):
        src = open(up.__file__).read()
        main = src[src.index('if __name__ == "__main__":'):]
        self.assertIn("upload(default_config())", main)


if __name__ == "__main__":
    unittest.main(verbosity=1)
