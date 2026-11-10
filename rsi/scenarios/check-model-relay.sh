#!/bin/sh
# Check: the model relay (scripts/model-relay.py, the tree's own) as a fleet
# engine reaches it, black box: real relay processes on loopback, in front of
# two stub upstreams (a "gateway" and a "DeepSeek") this check runs itself.
# No network, no key, no Claude: the stubs only record what reached them and
# answer as told.
#
# What an engine relies on, each checked through HTTP alone:
#   (1) the route by model: a `claude-*` request goes to the gateway with the
#       client's own auth headers, untouched, and DeepSeek never sees it;
#   (2) the model mapping (MODEL_RELAY_EVERY_MODEL_AS=deepseek-…, the switch
#       for a gateway out of quota): the same `claude-*` request reaches
#       DeepSeek as that model, each assistant message given the empty
#       thinking block DeepSeek requires, with the relay's own `x-api-key`
#       and none of the client's auth or cookie headers; the client gets
#       DeepSeek's answer;
#   (3) a stream cut before its first event is asked for again: DeepSeek
#       answers 500, then a 200 that sends only a keep-alive and ends, then
#       a whole stream; the client gets one 200 holding the whole stream,
#       through `message_stop`, and no error event, and DeepSeek saw three
#       attempts;
#   (4) a stream cut after its first event ends with an `overloaded_error`
#       event, never a silently short stream (Claude Code reads it as a
#       server error, not a finished answer);
#   (5) a body over MODEL_RELAY_MAX_BODY is refused with 413 and reaches
#       neither upstream.
# A relay with any of these removed fails it with 1 (each was shown so on a
# copy with the behaviour taken out).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness failure (no python3, the relay did not
# start). Any unexpected error exits 2, never 1.
set -eu
relay=$PWD/scripts/model-relay.py
[ -f "$relay" ] || { echo "check-model-relay: harness: no $relay" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "check-model-relay: harness: python3 is not installed" >&2; exit 2; }
work=$(mktemp -d) || { echo "check-model-relay: harness: mktemp failed" >&2; exit 2; }
trap 'rm -rf "$work"' EXIT
cat >"$work/check.py" <<'PY'
import json, os, subprocess, sys, threading, time, urllib.error, urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RELAY, WORK = sys.argv[1], sys.argv[2]
KEY = "ds-check-key-0123456789"


class Harness(Exception):
    pass


failures = []


def check(ok, what):
    print(("ok   " if ok else "FAIL ") + what, flush=True)
    if not ok:
        failures.append(what)


class Stub:
    """An upstream that records each request (path, headers, body) and
    answers with the next of its scripted replies."""

    def __init__(self, name):
        self.name, self.seen, self.replies, self.lock = name, [], [], threading.Lock()
        stub = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length)
                with stub.lock:
                    stub.seen.append(
                        {"path": self.path, "headers": {k.lower(): v for k, v in self.headers.items()},
                         "body": body})
                    reply = stub.replies.pop(0) if stub.replies else ("json", 500, {"error": "no reply scripted"})
                kind = reply[0]
                if kind == "json":
                    data = json.dumps(reply[2]).encode()
                    self.send_response(reply[1])
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                elif kind == "stream":
                    # A 200 event stream: the given chunks, then the end.
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    for chunk in reply[1]:
                        self.wfile.write(chunk)
                        self.wfile.flush()
                        time.sleep(0.05)
                    self.close_connection = True

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"


def start_relay(gateway, deepseek, extra):
    env = {k: v for k, v in os.environ.items()
           if k.lower() not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")}
    env.update({"DEEPSEEK_API_KEY": KEY, "DEEPSEEK_ANTHROPIC_URL": deepseek.url + "/anthropic",
                "MODEL_RELAY_UPSTREAM": gateway.url, "MODEL_RELAY_ADDRESS": "127.0.0.1:0",
                "no_proxy": "127.0.0.1,localhost"})
    env.pop("MODEL_RELAY_EVERY_MODEL_AS", None)
    env.update(extra)
    log = open(os.path.join(WORK, f"relay-{len(relays)}.log"), "w")
    proc = subprocess.Popen([sys.executable, "-I", RELAY], env=env, stderr=subprocess.PIPE,
                            stdout=subprocess.DEVNULL, text=True)
    relays.append(proc)
    deadline = time.monotonic() + 15
    line = ""
    while time.monotonic() < deadline:
        line = proc.stderr.readline()
        if "http://127.0.0.1:" in line or not line:
            break
    if "http://127.0.0.1:" not in line:
        raise Harness(f"the relay did not start: {line.strip() or 'no output'} (exit {proc.poll()})")
    port = int(line.split("http://127.0.0.1:")[1].split("/")[0])

    def drain():
        for rest in proc.stderr:
            log.write(rest)
            log.flush()

    threading.Thread(target=drain, daemon=True).start()
    return f"http://127.0.0.1:{port}"


def post(base, body, headers=None, raw=None):
    data = raw if raw is not None else json.dumps(body).encode()
    request = urllib.request.Request(base + "/v1/messages", data=data, method="POST",
                                     headers={"Content-Type": "application/json",
                                              "anthropic-version": "2023-06-01", **(headers or {})})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(request, timeout=60) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def event_stream(*events):
    out = []
    for name, data in events:
        out.append(f"event: {name}\ndata: {json.dumps(data)}\n\n".encode())
    return out


WHOLE = event_stream(
    ("message_start", {"type": "message_start", "message": {"id": "m1", "role": "assistant", "content": []}}),
    ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "whole answer"}}),
    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
    ("message_stop", {"type": "message_stop"}),
)
CLIENT_AUTH = {"Authorization": "Bearer gateway-token-of-the-client", "x-api-key": "client-own-key",
               "Cookie": "session=client-cookie"}
HISTORY = [{"role": "user", "content": "hello"}, {"role": "assistant", "content": "earlier answer"},
           {"role": "user", "content": "go on"}]
relays = []


def main():
    gateway, deepseek = Stub("gateway"), Stub("deepseek")
    plain = start_relay(gateway, deepseek, {"MODEL_RELAY_MAX_BODY": "4096"})
    mapped = start_relay(gateway, deepseek, {"MODEL_RELAY_EVERY_MODEL_AS": "deepseek-check"})

    # (1) the route by model
    gateway.replies.append(("json", 200, {"type": "message", "from": "gateway"}))
    status, body = post(plain, {"model": "claude-check-1", "messages": HISTORY, "max_tokens": 8}, CLIENT_AUTH)
    check(status == 200 and b'"gateway"' in body, f"(1) a claude-* request is answered by the gateway (status {status})")
    got = gateway.seen[-1] if gateway.seen else {"headers": {}, "body": b"{}", "path": ""}
    check(got["path"] == "/v1/messages", f"(1) the gateway gets the request's path ({got['path']})")
    check(got["headers"].get("authorization") == CLIENT_AUTH["Authorization"]
          and got["headers"].get("x-api-key") == CLIENT_AUTH["x-api-key"],
          "(1) with the client's own auth headers, untouched")
    check(json.loads(got["body"]).get("model") == "claude-check-1", "(1) and its own model")
    check(not deepseek.seen, "(1) DeepSeek never sees it")

    # (2) the model mapping
    deepseek.replies.append(("json", 200, {"type": "message", "from": "deepseek"}))
    before = len(gateway.seen)
    status, body = post(mapped, {"model": "claude-check-2", "messages": HISTORY, "max_tokens": 8}, CLIENT_AUTH)
    check(status == 200 and b'"deepseek"' in body,
          f"(2) with MODEL_RELAY_EVERY_MODEL_AS the claude-* request is answered by DeepSeek (status {status})")
    check(len(gateway.seen) == before, "(2) the gateway never sees it")
    got = deepseek.seen[-1] if deepseek.seen else {"headers": {}, "body": b"{}", "path": ""}
    sent = json.loads(got["body"] or b"{}")
    check(got["path"] == "/anthropic/v1/messages", f"(2) at DeepSeek's endpoint ({got['path']})")
    check(sent.get("model") == "deepseek-check", f"(2) as the mapped model ({sent.get('model')})")
    assistant = [m for m in sent.get("messages", []) if m.get("role") == "assistant"]
    first = assistant[0]["content"][0] if assistant and isinstance(assistant[0].get("content"), list) and assistant[0]["content"] else {}
    check(first.get("type") == "thinking", f"(2) each assistant message opens with a thinking block ({first.get('type')})")
    check(got["headers"].get("x-api-key") == KEY, "(2) with the relay's own x-api-key")
    check("authorization" not in got["headers"] and "cookie" not in got["headers"],
          "(2) and none of the client's auth or cookie headers")

    # (3) a stream cut before its first event is asked for again
    deepseek.seen.clear()
    deepseek.replies.extend([("json", 500, {"error": "busy"}),
                             ("stream", [b": keep-alive\n\n"]),
                             ("stream", WHOLE)])
    status, body = post(plain, {"model": "deepseek-chat", "stream": True, "messages": HISTORY, "max_tokens": 8})
    check(status == 200, f"(3) the client gets a 200 for the stream (status {status})")
    check(b"whole answer" in body and b"event: message_stop" in body,
          "(3) holding the whole stream, through message_stop")
    check(b"event: error" not in body and b"keep-alive" not in body,
          "(3) and nothing of the failed attempts")
    check(len(deepseek.seen) == 3, f"(3) DeepSeek saw three attempts ({len(deepseek.seen)})")

    # (4) a stream cut after its first event
    deepseek.replies.append(("stream", WHOLE[:3]))
    status, body = post(plain, {"model": "deepseek-chat", "stream": True, "messages": HISTORY, "max_tokens": 8})
    check(status == 200 and b"whole answer" in body, f"(4) a stream cut partway is passed as it came (status {status})")
    check(b"event: error" in body and b"overloaded_error" in body,
          "(4) and ends with an overloaded_error event, never silently short")

    # (5) the body cap
    seen = len(gateway.seen) + len(deepseek.seen)
    status, _ = post(plain, None, raw=json.dumps({"model": "claude-check-5", "messages": [
        {"role": "user", "content": "x" * 8192}]}).encode())
    check(status == 413, f"(5) a body over MODEL_RELAY_MAX_BODY is refused with 413 ({status})")
    check(len(gateway.seen) + len(deepseek.seen) == seen, "(5) and reaches neither upstream")

    for proc in relays:
        if proc.poll() is not None:
            raise Harness(f"a relay exited during the check (exit {proc.returncode})")


try:
    main()
except Harness as error:
    print(f"check-model-relay: harness: {error}", file=sys.stderr)
    sys.exit(2)
except Exception as error:  # anything unexpected is the harness's, never 1
    print(f"check-model-relay: harness: unexpected {error!r}", file=sys.stderr)
    sys.exit(2)
finally:
    for proc in relays:
        proc.kill()
        proc.wait()
if failures:
    for name in os.listdir(WORK):
        if name.startswith("relay-"):
            with open(os.path.join(WORK, name)) as log:
                tail = log.read().splitlines()[-5:]
            for line in tail:
                print(f"  {name}: {line}")
    sys.exit(1)
print("check-model-relay: route, mapping, stream retry, cut signal and body cap hold")
PY
python3 -I "$work/check.py" "$relay" "$work"
