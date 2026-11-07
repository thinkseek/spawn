#!/usr/bin/env python3
"""Local relay between Spawn's Claude Code engines and their model APIs.

Engines reach their models through this relay (`ANTHROPIC_BASE_URL` in the
inline `--settings` the Bridge passes). Every request under `/v1/` is routed
by the `model` of its JSON body:

- `deepseek-*` goes to DeepSeek's Anthropic-compatible endpoint with only
  the headers in `DEEPSEEK_HEADERS` from the client (no auth, cookie or
  gateway header) plus `x-api-key: $DEEPSEEK_API_KEY`.
- Everything else goes unchanged to the upstream gateway, with the client's
  own auth headers. The relay never holds the gateway's token.
- With `MODEL_RELAY_EVERY_MODEL_AS` set (a `deepseek-*` name), every other
  model goes to DeepSeek too, as that model. For when the gateway cannot
  serve (its quota spent), so engines keep working without a restart; unset
  it to send them back. The request's `model` is rewritten, and its history
  made one DeepSeek takes (checked 2026-11-07 with deepseek-v4-pro):
  DeepSeek always thinks, and refuses an assistant message without a
  thinking block ("must be passed back") or with a `redacted_thinking` one.
  So each assistant message gets an empty thinking block first where it has
  none, and a redacted one becomes empty. Signed thinking blocks, tool calls
  and everything else pass as they are.

Responses, server-sent events included, are passed on as they arrive, with
one hold for DeepSeek's streams. Loaded, DeepSeek (seen 2026-11-07) answered
500 after 15 s to a quarter or more of requests, held requests over 120 s
without headers, sent a 200 and then only keep-alive comments, and closed
streams partway without `message_stop`, over HTTP/1.1 and HTTP/2 alike:

- Nothing reaches the client before the stream's first event (a line that
  is neither blank nor a `:` comment such as DeepSeek's keep-alive). Until
  then a failed attempt is asked for again: a SERVER_ERRORS status,
  DEEPSEEK_SILENCE seconds without headers or without an event (keep-alives
  are no progress), a stream that ends or breaks. Up to STREAM_ATTEMPTS
  attempts, all within STREAM_BUDGET seconds of the request's arrival; then
  the client gets a 529 `overloaded_error`, which Claude Code retries as it
  does the API's.
  This is the only real protection: the client never sees a failed attempt.
- From the first event on, the stream passes as it arrives and a cut can no
  longer be mended. A stream DeepSeek breaks off, or ends without
  `message_stop`, ends with an `overloaded_error` event instead of a dropped
  connection, which names the cause. It does not save the turn: once a
  content block has finished and output is shown, Claude Code (checked in
  2.1.319) keeps the partial output and ends the turn whatever the relay
  sends ("Server error mid-response" after the event, "Connection lost
  mid-response" after a dropped connection); before that it retries on
  either.

Holding a stream until whole (tried first) left the client without content
for minutes on a long answer, and Claude Code gave up on it after 5 to 8
minutes, pings or not. Waiting for the first event costs a retried request
its place in DeepSeek's queue. The hold has one deadline, STREAM_BUDGET
(120 s) from the request's arrival: every wait (for headers, for the first
event, for a socket read) is capped at what is left of it, so the client
has its first byte or its 529 within 120 s, well inside the 180 s
byte-idle default of Claude Code (2.1.319).

A request that is not streamed waits up to TIMEOUT for its answer, DeepSeek's
too: DeepSeek sends a non-streamed body only once it is done, so a silence
bound would cut every long answer (seen 2026-11-07 17:23), and Claude Code
falls back to such a request when a stream fails.

A redirect is passed back to the client, never followed: following it would
send the request's credentials to whatever host it names.

The DeepSeek key stays in this process. It never enters Spawn's environment,
the engine's environment, the event log, or an agent's terminal.

Environment:
  DEEPSEEK_API_KEY        required
  DEEPSEEK_ANTHROPIC_URL  default https://api.deepseek.com/anthropic
  MODEL_RELAY_EVERY_MODEL_AS  a deepseek-* model every other model is sent
                          to DeepSeek as; unset (the default) for none
  MODEL_RELAY_UPSTREAM    the gateway every other model goes to; default the
                          `env.ANTHROPIC_BASE_URL` of Claude Code's user
                          settings ($CLAUDE_CONFIG_DIR or ~/.claude), which
                          is where the engines would go without the relay
  MODEL_RELAY_ADDRESS     default 127.0.0.1:8791; a bare port (":8791" or
                          "8791") means host 127.0.0.1
  MODEL_RELAY_MAX_BODY    request cap in bytes, default 32 MiB (the
                          Messages API's own limit)
  MODEL_RELAY_CLIENT_TIMEOUT  seconds a client socket may stall (a request
                          that stops arriving, a stream it stops reading),
                          default 60
  http_proxy, https_proxy honored for both upstreams; no_proxy too
"""

import json
import os
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Headers that describe one hop, never forwarded either way.
HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    # Compressed bodies would hold back server-sent events until a block
    # fills; the hop to the client is loopback, so plain is free.
    "accept-encoding",
}
# The only client headers DeepSeek gets: the Messages API's own. Anything
# else (the gateway's auth, cookies, a gateway's custom header) stays here;
# DeepSeek gets its own key.
DEEPSEEK_HEADERS = {"content-type", "accept", "anthropic-version", "anthropic-beta", "user-agent"}
# A model's first token can take minutes behind a long prompt, and a stream
# may pause as long between events.
TIMEOUT = 600
CHUNK = 64 * 1024
# A DeepSeek stream is asked for this many times in all before the client is
# told the upstream is overloaded; the waits between attempts, in seconds.
STREAM_ATTEMPTS = 5
STREAM_BACKOFF = (2, 4, 8, 16)
# The bytes kept of a stream's end, to find its `message_stop` in.
STREAM_TAIL = CHUNK + 64
# Statuses of an upstream that could not serve this time: asked again.
SERVER_ERRORS = {429, 500, 502, 503, 504, 529}
# Seconds a DeepSeek stream may send nothing at all (no headers, not even
# its keep-alive comment, which it sends every 10-15 s while a request
# waits), and after its headers the seconds it may send no event, its
# keep-alives not counting: then the attempt is over. Seen 2026-11-07: a
# request held over 120 s without headers.
DEEPSEEK_SILENCE = 60
# Seconds from a DeepSeek stream request's arrival to its first byte or its
# 529, whatever DeepSeek does: inside Claude Code's 180 s byte-idle default,
# with room to spare.
STREAM_BUDGET = 120


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Hands a 3xx back as an HTTPError instead of following it."""

    def redirect_request(self, *_args, **_kwargs):
        return None


def opener():
    """An opener with the environment's proxies (read now, as urlopen
    would) that never follows a redirect."""
    return urllib.request.build_opener(NoRedirect)


def settings_upstream():
    """`env.ANTHROPIC_BASE_URL` of Claude Code's user settings, if any."""
    home = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    try:
        with open(os.path.join(home, "settings.json"), encoding="utf-8") as file:
            return json.load(file).get("env", {}).get("ANTHROPIC_BASE_URL", "")
    except (OSError, ValueError, AttributeError):
        return ""


def config(environ):
    """The relay's settings from `environ`."""
    host, _, port = environ.get("MODEL_RELAY_ADDRESS", "127.0.0.1:8791").rpartition(":")
    return {
        "key": environ.get("DEEPSEEK_API_KEY", ""),
        "deepseek": environ.get(
            "DEEPSEEK_ANTHROPIC_URL", "https://api.deepseek.com/anthropic"
        ).rstrip("/"),
        "upstream": (environ.get("MODEL_RELAY_UPSTREAM") or settings_upstream()).rstrip("/"),
        # An empty host would bind every interface; a bare port means loopback.
        "host": host or "127.0.0.1",
        "port": int(port),
        "max_body": int(environ.get("MODEL_RELAY_MAX_BODY", str(32 * 1024 * 1024))),
        "client_timeout": float(environ.get("MODEL_RELAY_CLIENT_TIMEOUT", "60")),
        "every_model_as": environ.get("MODEL_RELAY_EVERY_MODEL_AS", ""),
    }


def routed_model(body):
    """The `model` a JSON request body names, or "" for anything else."""
    try:
        model = json.loads(body).get("model", "")
    except (ValueError, AttributeError, UnicodeDecodeError):
        return ""
    return model if isinstance(model, str) else ""


EMPTY_THINKING = {"type": "thinking", "thinking": "", "signature": ""}


def as_model(body, model):
    """`body` (a JSON object naming a model) naming `model` instead, its
    assistant messages each with a thinking block DeepSeek takes."""
    request = json.loads(body)
    request["model"] = model
    for message in request.get("messages") or []:
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        content = message.get("content")
        if isinstance(content, str):
            content = [{"type": "text", "text": content}] if content else []
        if not isinstance(content, list):
            continue
        content = [
            dict(EMPTY_THINKING)
            if isinstance(block, dict) and block.get("type") == "redacted_thinking"
            else block
            for block in content
        ]
        thinks = (isinstance(block, dict) and block.get("type") == "thinking" for block in content)
        if not any(thinks):
            content.insert(0, dict(EMPTY_THINKING))
        message["content"] = content
    return json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode()


def wants_stream(body):
    """Whether the JSON request `body` asks for a stream."""
    try:
        return json.loads(body).get("stream") is True
    except (ValueError, AttributeError, UnicodeDecodeError):
        return False


def error_body(message, kind="api_error"):
    """An error in the Messages API's shape, which Claude Code shows."""
    return json.dumps(
        {"type": "error", "error": {"type": kind, "message": f"model relay: {message}"}}
    ).encode()


class ClientGone(Exception):
    """A write to the client failed: it went away, or stopped reading for
    longer than the client timeout. Only this counts as the client's; any
    other error once a response has started is the upstream's."""


def sse_lines(data):
    """The whole lines in `data`, part of a server-sent event stream, without
    their line ends."""
    return [line.rstrip(b"\r") for line in data.split(b"\n")[:-1]]


def has_event(data):
    """Whether `data`, the start of a stream, holds a line of an event: one
    that is neither blank nor a comment (DeepSeek's `: keep-alive`)."""
    return any(line and not line.startswith(b":") for line in sse_lines(data))


def read_timeout(response, seconds):
    """Make the next socket reads of `response` wait at most `seconds`.
    http.client keeps its socket private (`fp.raw._sock`); without it, reads
    keep the timeout the request was opened with."""
    sock = getattr(getattr(getattr(response, "fp", None), "raw", None), "_sock", None)
    if sock is not None:
        sock.settimeout(max(seconds, 0.001))


def ends_whole(tail):
    """Whether `tail`, the end of a stream, holds its `message_stop` event.
    Matched as a whole line: the same words inside an event's JSON data
    (a model writing about this relay) are never a line of their own, since
    a JSON string holds no raw newline."""
    return any(
        line.startswith(b"event:") and line[6:].strip() == b"message_stop"
        for line in sse_lines(tail)
    )


def handler(settings):
    """A request handler class bound to `settings`."""

    class Relay(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        # The client socket's timeout: a client that stalls (a body that
        # never arrives, an idle keep-alive, a stream left unread) frees its
        # thread instead of holding it forever.
        timeout = settings["client_timeout"]

        def _send(self, data=b"", headers=False):
            """Write to the client the headers sent so far (with `headers`),
            then `data`. Any failure is the client's: ClientGone."""
            try:
                if headers:
                    self.end_headers()
                if data:
                    self.wfile.write(data)
                self.wfile.flush()
            except OSError as error:
                raise ClientGone(repr(error)) from error

        def _reply(self, status, body, close=False):
            self._status, self._sent = status, len(body)
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            if close:
                self.send_header("Connection", "close")
                self.close_connection = True
            self._send(body, headers=True)

        def _relay(self):
            started = self._arrived = time.monotonic()
            # Set once the response's status line is out: an error after it
            # can only cut the connection.
            self._started = False
            # What the log reports: the status the client got (502 when the
            # upstream failed before any) and the body bytes it was sent.
            self._status, self._sent = 502, 0
            if self.path == "/healthz":
                self._reply(200, b'{"ok":true}')
                return
            if not self.path.startswith("/v1/"):
                self._reply(404, error_body("only /v1/ is relayed"))
                return
            if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
                self._reply(411, error_body("send a Content-Length"), close=True)
                return
            try:
                length = int(self.headers.get("Content-Length") or 0)
            except ValueError:
                self._reply(400, error_body("Content-Length is not a number"), close=True)
                return
            if length < 0 or length > settings["max_body"]:
                # The body stays unread, so the connection cannot be reused.
                self._reply(
                    413, error_body(f"body over {settings['max_body']} bytes"), close=True
                )
                return
            body = self.rfile.read(length) if length else b""
            if len(body) != length:
                self._reply(400, error_body("body is shorter than Content-Length"), close=True)
                return
            model = routed_model(body) if body else ""
            sent_as = model
            if model and not model.startswith("deepseek-") and settings["every_model_as"]:
                sent_as = settings["every_model_as"]
                body = as_model(body, sent_as)
            headers = {
                name: value
                for name, value in self.headers.items()
                if name.lower() not in HOP
            }
            if sent_as.startswith("deepseek-"):
                route, base = "deepseek", settings["deepseek"]
                headers = {
                    name: value
                    for name, value in headers.items()
                    if name.lower() in DEEPSEEK_HEADERS
                }
                headers["x-api-key"] = settings["key"]
            else:
                route, base = "upstream", settings["upstream"]
            request = urllib.request.Request(
                base + self.path,
                data=body if self.command not in ("GET", "HEAD") else None,
                method=self.command,
                headers=headers,
            )
            # An error's first bytes, logged: the upstream's reason, never a
            # request's content.
            self._error_head = b""
            # Why a response stopped partway, if it did: the client sees only
            # a connection lost.
            cut = ""
            # The attempts at a DeepSeek stream, and why each failed one failed.
            self._attempts, self._failures = 1, []
            try:
                if route == "deepseek" and wants_stream(body):
                    cut = self._pass_deepseek_stream(request)
                else:
                    try:
                        response = opener().open(request, timeout=TIMEOUT)
                    except urllib.error.HTTPError as error:
                        response = error
                    status = response.status if hasattr(response, "status") else response.code
                    self._pass(response, status)
            except ClientGone as error:
                # The client went away; the upstream response is dropped with it.
                self.close_connection = True
                cut = f"client gone: {error}"
            except Exception as error:  # the upstream: network, TLS, a timeout, a reset
                if not self._started:
                    try:
                        self._reply(502, error_body(f"{route}: {error}"), close=True)
                    except ClientGone as gone:
                        cut = f"client gone: {gone}"
                else:
                    self.close_connection = True
                    cut = f"{route} cut: {error!r}"
            elapsed = (time.monotonic() - started) * 1000
            sys.stderr.write(
                f"model-relay: {self.command} {self.path.split('?')[0]} "
                f"{model or '-'}{'' if sent_as == model else ' as ' + sent_as} "
                f"-> {route} {self._status} {elapsed:.0f}ms "
                f"{len(body)}B in {self._sent}B out"
                + (
                    f" {self._error_head.decode('utf-8', 'replace')!r}"
                    if self._error_head
                    else ""
                )
                + (
                    f" after {self._attempts} attempts ({'; '.join(self._failures)})"
                    if self._attempts > 1
                    else ""
                )
                + (f" CUT {cut}" if cut else "")
                + "\n"
            )

        def _pass(self, response, status):
            """Send `response` to the client as it arrives. An upstream error
            while reading it propagates; a failed write is ClientGone."""
            with response:
                self._started = True
                self._status = status
                self.send_response(status)
                length = response.headers.get("Content-Length")
                for name, value in response.headers.items():
                    if name.lower() not in HOP:
                        self.send_header(name, value)
                if length is not None:
                    self.send_header("Content-Length", length)
                else:
                    self.send_header("Transfer-Encoding", "chunked")
                self._send(headers=True)
                while True:
                    # read1 returns what has arrived, without waiting to fill
                    # the buffer: each event goes on as soon as it lands.
                    chunk = response.read1(CHUNK)
                    if not chunk:
                        break
                    if status >= 400 and len(self._error_head) < 300:
                        self._error_head += chunk[: 300 - len(self._error_head)]
                    self._sent += len(chunk)
                    if length is None:
                        self._chunk(chunk)
                    else:
                        self._send(chunk)
                if length is None:
                    self._send(b"0\r\n\r\n")
                elif self._sent != int(length):
                    self.close_connection = True

        def _chunk(self, data):
            self._send(f"{len(data):x}\r\n".encode() + data + b"\r\n")

        def _open(self, request, seconds):
            """One attempt at a DeepSeek request, waited for at most `seconds`
            of wall clock (connect, TLS and headers together): its response,
            its status and "", or None, None and why. The open runs in a
            thread, since a socket timeout bounds each step, not their sum;
            one given up on closes its own response when it comes."""
            got, lock, done = {}, threading.Lock(), threading.Event()

            def run():
                try:
                    response = opener().open(request, timeout=max(seconds, 0.001))
                except urllib.error.HTTPError as error:
                    response = error
                except Exception as error:
                    got["why"] = repr(error)[:120]
                    done.set()
                    return
                with lock:
                    if got.get("given up"):
                        response.close()
                        return
                    got["response"] = response
                done.set()

            threading.Thread(target=run, daemon=True).start()
            done.wait(seconds)
            with lock:
                if "response" not in got:
                    got["given up"] = True
                    return None, None, got.get("why") or f"no headers in {seconds:.1f}s"
            response = got["response"]
            status = response.status if hasattr(response, "status") else response.code
            return response, status, ""

        def _first_event(self, response, deadline):
            """Read DeepSeek's 200 stream up to its first event, keep-alive
            comments before it being no progress. Returns the bytes read and
            "", or None and why: the stream ended or broke first, or brought
            no event by `deadline` (monotonic). The response is then closed.
            Each read waits only for what is left until the deadline, so
            neither a keep-alive just before it nor a silence after one
            stretches the attempt."""
            began, data = time.monotonic(), b""
            try:
                while not has_event(data):
                    left = deadline - time.monotonic()
                    if left <= 0:
                        why = f"no event in {time.monotonic() - began:.0f}s"
                        break
                    read_timeout(response, left)
                    chunk = response.read1(CHUNK)
                    if not chunk:
                        why = "ended before an event"
                        break
                    data += chunk
                else:
                    # The live stream gets the open's own timeout back.
                    read_timeout(response, DEEPSEEK_SILENCE)
                    return data, ""
            except TimeoutError:
                why = f"no event in {time.monotonic() - began:.0f}s"
            except Exception as error:  # a reset: nothing was sent yet
                why = f"cut before an event: {error!r}"[:120]
            response.close()
            return None, why

        def _pass_deepseek_stream(self, request):
            """DeepSeek's stream for `request`, held until its first event and
            asked for again until then (see the module's notes). Records the
            attempts and their failures; returns why the stream was cut after
            it started, if it was."""
            # One deadline for the whole hold, from the request's arrival:
            # every wait below is capped at what is left of it.
            end = self._arrived + STREAM_BUDGET
            self._attempts = 0
            while True:
                self._attempts += 1
                response, status, why = self._open(
                    request, min(DEEPSEEK_SILENCE, end - time.monotonic())
                )
                if response is not None and status == 200:
                    deadline = min(time.monotonic() + DEEPSEEK_SILENCE, end)
                    start, why = self._first_event(response, deadline)
                    if start is not None:
                        return self._pass_live(response, start)
                elif response is not None and status not in SERVER_ERRORS:
                    # A refusal of the request itself: asking again cannot mend it.
                    self._pass(response, status)
                    return ""
                elif response is not None:
                    response.close()
                    why = str(status)
                self._failures.append(why)
                backoff = STREAM_BACKOFF[min(self._attempts, len(STREAM_BACKOFF)) - 1]
                # No attempt starts at or after the deadline, its backoff included.
                if self._attempts >= STREAM_ATTEMPTS or time.monotonic() + backoff >= end:
                    body = error_body(
                        f"DeepSeek failed {self._attempts} times; try again", "overloaded_error"
                    )
                    self._reply(529, body)
                    return ""
                time.sleep(backoff)

        def _pass_live(self, response, start):
            """Send DeepSeek's 200 stream from `start` (its bytes up to the
            first event) as it arrives. A stream DeepSeek breaks off (a reset,
            a timeout) or ends without `message_stop` gets an
            `overloaded_error` event. Returns why it was cut, if it was; a
            failed write to the client raises ClientGone."""
            with response:
                self._started = True
                self._status = 200
                self.send_response(200)
                for name, value in response.headers.items():
                    if name.lower() not in HOP:
                        self.send_header(name, value)
                self.send_header("Transfer-Encoding", "chunked")
                self._send(headers=True)
                chunk, tail, cut = start, b"", ""
                while chunk:
                    self._chunk(chunk)
                    self._sent += len(chunk)
                    tail = (tail + chunk)[-STREAM_TAIL:]
                    try:
                        chunk = response.read1(CHUNK)
                    except Exception as error:  # DeepSeek's side only
                        chunk, cut = b"", f"deepseek cut: {error!r}"
                if not ends_whole(tail):
                    cut = cut or "deepseek ended the stream without message_stop"
                    message = "DeepSeek cut the stream partway; try again"
                    event = b"event: error\ndata: " + error_body(message, "overloaded_error")
                    self._chunk(event + b"\n\n")
                self._send(b"0\r\n\r\n")
                return cut

        do_POST = _relay
        do_GET = _relay
        do_PUT = _relay
        do_PATCH = _relay
        do_DELETE = _relay

        def log_message(self, *_args):
            pass

    return Relay


def serve(settings):
    """A server for `settings`, bound and not yet serving."""
    server = ThreadingHTTPServer((settings["host"], settings["port"]), handler(settings))
    server.daemon_threads = True
    return server


def refusal(settings):
    """Why the relay must not start with `settings`, or None."""
    if not settings["key"]:
        return "DEEPSEEK_API_KEY is not set"
    if not settings["upstream"]:
        return "no upstream; set MODEL_RELAY_UPSTREAM"
    if settings["every_model_as"] and not settings["every_model_as"].startswith("deepseek-"):
        return (
            f"MODEL_RELAY_EVERY_MODEL_AS names {settings['every_model_as']}, "
            "not a deepseek-* model"
        )
    # Whoever reaches the relay spends the key, so it serves loopback only.
    if settings["host"] not in ("127.0.0.1", "::1", "[::1]", "localhost"):
        return f"refusing to listen on {settings['host']}; use 127.0.0.1"
    return None


def main():
    settings = config(os.environ)
    problem = refusal(settings)
    if problem:
        sys.exit(f"model-relay: {problem}")
    server = serve(settings)
    upstream = urllib.parse.urlsplit(settings["upstream"]).hostname
    deepseek = urllib.parse.urlsplit(settings["deepseek"]).hostname
    others = (
        f"others to {deepseek} as {settings['every_model_as']}"
        if settings["every_model_as"]
        else f"others to {upstream}"
    )
    sys.stderr.write(
        f"model-relay: http://{settings['host']}:{server.server_port}/v1/ -> "
        f"deepseek-* to {deepseek}, {others}\n"
    )
    server.serve_forever()


if __name__ == "__main__":
    main()
