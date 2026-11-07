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

Responses, server-sent events included, are passed on as they arrive. A
redirect is passed back to the client, never followed: following it would
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


def error_body(message):
    """An error in the Messages API's shape, which Claude Code shows."""
    return json.dumps(
        {"type": "error", "error": {"type": "api_error", "message": f"model relay: {message}"}}
    ).encode()


def handler(settings):
    """A request handler class bound to `settings`."""

    class Relay(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        # The client socket's timeout: a client that stalls (a body that
        # never arrives, an idle keep-alive, a stream left unread) frees its
        # thread instead of holding it forever.
        timeout = settings["client_timeout"]

        def _reply(self, status, body, close=False):
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            if close:
                self.send_header("Connection", "close")
                self.close_connection = True
            self.end_headers()
            self.wfile.write(body)

        def _relay(self):
            started = time.monotonic()
            # Set once the response's status line is out: an error after it
            # can only cut the connection.
            self._started = False
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
            sent = 0
            status = 502
            # An error's first bytes, logged: the upstream's reason, never a
            # request's content.
            self._error_head = b""
            try:
                try:
                    response = opener().open(request, timeout=TIMEOUT)
                except urllib.error.HTTPError as error:
                    response = error
                status = response.status if hasattr(response, "status") else response.code
                sent = self._pass(response, status)
            except (BrokenPipeError, ConnectionResetError):
                # The client went away; the upstream stream is dropped with it.
                self.close_connection = True
            except Exception as error:  # network, TLS, timeout
                if not self._started:
                    self._reply(502, error_body(f"{route}: {error}"), close=True)
                else:
                    self.close_connection = True
            elapsed = (time.monotonic() - started) * 1000
            sys.stderr.write(
                f"model-relay: {self.command} {self.path.split('?')[0]} "
                f"{model or '-'}{'' if sent_as == model else ' as ' + sent_as} "
                f"-> {route} {status} {elapsed:.0f}ms "
                f"{len(body)}B in {sent}B out"
                + (
                    f" {self._error_head.decode('utf-8', 'replace')!r}"
                    if self._error_head
                    else ""
                )
                + "\n"
            )

        def _pass(self, response, status):
            """Send `response` to the client as it arrives; returns the body
            bytes sent."""
            with response:
                self._started = True
                self.send_response(status)
                length = response.headers.get("Content-Length")
                for name, value in response.headers.items():
                    if name.lower() not in HOP:
                        self.send_header(name, value)
                if length is not None:
                    self.send_header("Content-Length", length)
                else:
                    self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                self.wfile.flush()
                sent = 0
                while True:
                    # read1 returns what has arrived, without waiting to fill
                    # the buffer: each event goes on as soon as it lands.
                    chunk = response.read1(CHUNK)
                    if not chunk:
                        break
                    if status >= 400 and len(self._error_head) < 300:
                        self._error_head += chunk[: 300 - len(self._error_head)]
                    sent += len(chunk)
                    if length is None:
                        self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                    else:
                        self.wfile.write(chunk)
                    self.wfile.flush()
                if length is None:
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                elif sent != int(length):
                    self.close_connection = True
                return sent

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
