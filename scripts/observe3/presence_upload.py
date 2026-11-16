#!/usr/bin/env python3
"""presence_upload: send the sanitized agent-presence document to the observe3
receiver, by one outbound HTTPS POST (contract: docs/observe3-agent-presence.md,
"Delivery").

OFF BY DEFAULT. It sends nothing unless BOTH exist:
  ~/.config/spawn-agent-presence/upload.json   containing {"enabled": true}
  ~/.config/spawn-agent-presence/ingest-token  the dedicated bearer token,
                                               a regular file, mode 0600 or 0400
and the exporter's off switch (~/.config/spawn-agent-presence/disabled) is absent.

What it sends: exactly the bytes of ~/.local/state/spawn-agent-presence/
presence.json, after checking them with the exporter's own validator. Never the
id map, the lock or anything else.

Where: the fixed URL below. It is not configurable; a redirect is never
followed; TLS is verified with the system's default trust (hostname checked).
The token goes only in the Authorization header, never in a URL, a log line
or a file of ours.

How often: a document is not sent again once the receiver acknowledged it
(200, 204 or 409) and that was saved in the state file; this is not
exactly-once transport (a lost answer, an interruption or a failed state
write can resend it, and the receiver's replay check refuses the copy). In
strictly increasing generated_utc order, only while it is at most MAX_AGE_S old; at most one attempt per
MIN_ATTEMPT_S; after a refusal it backs off (see BACKOFF_S).

It opens no port, starts no process, reads no other credential, and changes
nothing but its own small state file. From the environment it uses only HOME
(to find its files); OpenSSL would also honour SSL_CERT_FILE/SSL_CERT_DIR,
which the unit clears so the system trust store is used.
"""
import calendar
import errno
import fcntl
import http.client
import json
import os
import ssl
import stat
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import agent_presence  # noqa: E402  (same folder: the document's own validator)

HOST = "www.flora.chat"
PORT = 443
PATH = "/observe3/ingest/agent-presence"
MAX_DOC_BYTES = 16384
MAX_AGE_S = 90            # the receiver refuses documents older than 120 s
MAX_AHEAD_S = 5           # the receiver refuses documents more than 30 s ahead
MIN_ATTEMPT_S = 10.0      # the receiver's producer budget: 4, +1 per 5 s
TIMEOUT_S = 10.0
MIN_TOKEN, MAX_TOKEN = 32, 256
TOKEN_CHARS = frozenset("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~")
TEST_ONLY_PREFIX = "TEST-ONLY-"
PLACEHOLDER_MARKERS = ("test-only", "testonly", "not-a-credential", "example", "placeholder", "changeme",
                       "change-me", "dummy", "sample", "synthetic", "insecure", "replace-me", "your-token")
# result word -> seconds before the next attempt
BACKOFF_S = {"unauthorized": 600, "rejected": 300, "rate_limited": 30, "server_error": 30,
             "network_error": 30}


def default_config():
    home = os.path.expanduser("~")
    conf = os.path.join(home, ".config/spawn-agent-presence")
    return {
        "enable_file": os.path.join(conf, "upload.json"),
        "token_file": os.path.join(conf, "ingest-token"),
        "disable_file": os.path.join(conf, "disabled"),
        "state_dir": os.path.join(home, ".local/state/spawn-agent-presence"),
    }


def utc(t):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t))


def parse_utc(s):
    try:
        return calendar.timegm(time.strptime(s, "%Y-%m-%dT%H:%M:%SZ"))
    except (TypeError, ValueError):
        return None


def read_small(path, cap, owner_only=False):
    """A regular file of ours, not a link, at most cap bytes; else None."""
    try:
        # O_NONBLOCK: a FIFO or device at the path cannot hold the run; it is
        # then refused by the regular-file check below.
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError:
        return None
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_size > cap:
            return None
        if owner_only and st.st_mode & 0o077:
            return None
        return os.read(fd, cap + 1)[:cap + 1]
    except OSError:
        return None
    finally:
        os.close(fd)


def _no_duplicates(pairs):
    keys = [k for k, _ in pairs]
    if len(keys) != len(set(keys)):
        raise ValueError("duplicate key")
    return dict(pairs)


def enabled(cfg):
    """True only for exactly {"enabled": true}: the value must be the JSON
    literal true (not 1 or 1.0), with no other key and no duplicate key."""
    raw = read_small(cfg["enable_file"], 256)
    if raw is None:
        return False
    try:
        doc = json.loads(raw.decode("utf-8"), object_pairs_hook=_no_duplicates)
    except ValueError:
        return False
    return isinstance(doc, dict) and list(doc) == ["enabled"] and doc["enabled"] is True


def token_ok(token, allow_test_token=False):
    if not isinstance(token, str) or not MIN_TOKEN <= len(token) <= MAX_TOKEN:
        return False
    if not set(token) <= TOKEN_CHARS or len(set(token)) < 16:
        return False
    if allow_test_token:
        return token.startswith(TEST_ONLY_PREFIX)
    lowered = token.lower()
    return not token.startswith(TEST_ONLY_PREFIX) and not any(m in lowered for m in PLACEHOLDER_MARKERS)


def load_token(cfg, allow_test_token=False):
    raw = read_small(cfg["token_file"], 512, owner_only=True)
    if raw is None or len(raw) > 512:
        return None
    try:
        token = raw.decode("ascii").strip()
    except UnicodeDecodeError:
        return None
    return token if token_ok(token, allow_test_token) else None


def load_state(path):
    raw = read_small(path, 4096)
    try:
        st = json.loads(raw.decode("utf-8")) if raw else {}
    except ValueError:
        st = {}
    if not isinstance(st, dict):
        st = {}
    out = {"last_sent_utc": None, "backoff_until": 0.0, "last_attempt": 0.0}
    if parse_utc(st.get("last_sent_utc")) is not None:
        out["last_sent_utc"] = st["last_sent_utc"]
    for k in ("backoff_until", "last_attempt"):
        v = st.get(k)
        if isinstance(v, (int, float)) and not isinstance(v, bool) and 0 <= v < 4102444800:
            out[k] = float(v)
    return out


def save_state(path, st):
    tmp = path + ".tmp"
    try:
        os.unlink(tmp)
    except FileNotFoundError:
        pass
    data = json.dumps(st, sort_keys=True).encode("utf-8")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        os.write(fd, data)
        os.fsync(fd)
    finally:
        os.close(fd)
    os.replace(tmp, path)
    dfd = os.open(os.path.dirname(path), os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(dfd)
    finally:
        os.close(dfd)


def https_connect():
    return http.client.HTTPSConnection(HOST, PORT, timeout=TIMEOUT_S, context=ssl.create_default_context())


def post(connect, token, body):
    """One POST. Returns the HTTP status, or None on a network or TLS error.
    The response body is read (bounded) and dropped; nothing is logged."""
    conn = None
    try:
        conn = connect()
        conn.request("POST", PATH, body=body, headers={
            "Authorization": "Bearer " + token,
            "Content-Type": "application/json",
            "Content-Length": str(len(body)),
        })
        resp = conn.getresponse()
        resp.read(1024)
        return resp.status
    except (OSError, http.client.HTTPException, ssl.SSLError):
        return None
    finally:
        if conn is not None:
            try:
                conn.close()
            except OSError:
                pass


def word_for(status):
    if status is None:
        return "network_error"
    if status in (200, 204):
        return "sent"
    if status == 409:
        return "replay_or_old"      # the receiver already has it or newer: never resend it
    if status == 401:
        return "unauthorized"
    if status == 429:
        return "rate_limited"
    if 500 <= status <= 599:
        return "server_error"
    return "rejected"               # 3xx (never followed), 400, 403, 404, 405, 411, 413, 415, 422 ...


def upload(cfg, connect=https_connect, allow_test_token=False, now=None):
    """One run. Returns a fixed word; sends at most one request."""
    if os.path.lexists(cfg["disable_file"]):
        return "disabled"
    if not enabled(cfg):
        return "off"
    token = load_token(cfg, allow_test_token)
    if token is None:
        return "token_refused"
    d = cfg["state_dir"]
    try:
        st = os.lstat(d)
        if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
            return "state_dir_refused"
        lock = os.open(os.path.join(d, "upload.lock"), os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    except OSError:
        return "no_document"
    try:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return "busy"
        now = time.time() if now is None else now
        state_path = os.path.join(d, "upload-state.json")
        state = load_state(state_path)
        if now < state["backoff_until"]:
            return "backoff"
        if 0 <= now - state["last_attempt"] < MIN_ATTEMPT_S:
            return "too_soon"
        raw = read_small(os.path.join(d, "presence.json"), MAX_DOC_BYTES + 1)   # one over: an oversize file is invalid
        if raw is None:
            return "no_document"
        if not 1 <= len(raw) <= MAX_DOC_BYTES:
            return "invalid_document"
        try:
            # Duplicate keys refused at every level (cc-astra A91): the bytes
            # sent are exactly the bytes validated, with no hidden values.
            doc = json.loads(raw.decode("utf-8"), object_pairs_hook=_no_duplicates)
        except ValueError:
            return "invalid_document"
        if not isinstance(doc, dict) or not agent_presence.valid(doc):
            return "invalid_document"
        gen = parse_utc(doc["generated_utc"])
        if gen - now > MAX_AHEAD_S:
            return "invalid_document"
        if now - gen > MAX_AGE_S:
            return "too_old"
        last = parse_utc(state["last_sent_utc"]) if state["last_sent_utc"] else None
        if last is not None and gen <= last:
            return "already_sent"
        status = post(connect, token, raw)
        word = word_for(status)
        state["last_attempt"] = now
        if word in ("sent", "replay_or_old"):
            state["last_sent_utc"] = doc["generated_utc"]
        state["backoff_until"] = now + BACKOFF_S.get(word, 0)
        try:
            save_state(state_path, state)
        except OSError as e:
            return "disk_full" if e.errno == errno.ENOSPC else "state_write_error"
        return word
    finally:
        os.close(lock)


if __name__ == "__main__":
    word = upload(default_config())
    if word not in ("sent", "off", "disabled", "already_sent", "too_soon"):
        print("presence_upload: %s" % word)
    sys.exit(0)
