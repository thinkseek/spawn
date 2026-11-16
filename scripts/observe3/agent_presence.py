#!/usr/bin/env python3
"""agent_presence: write one sanitized, display-only document of the Spawn
fleet's agents for observe3 (contract "spawn-agent-presence v1",
docs/observe3-agent-presence.md).

It reads the local page service only, at 127.0.0.1:
  GET /api/fleet                      every run
  GET /api/session/<id>?...           only for a live agent with an open
                                      turn: its newest engine events
Both responses are mixed (ids, labels, task text, screen text, tool names,
judge reasons, event details), so nothing of them is copied: every output
field is a word of a fixed set, an opaque local id of our own, a random
identity epoch of our own, or the document's own times. No per-agent time
leaves: the times only decide the state here.

The state of an agent comes only from Spawn's recorded events:
  offline  its execution has ended (live is false)
  working  live, an open turn (message.received after the last
           turn.stopped/turn.failed), and an event written by the agent's
           OWN ENGINE in its CURRENT execution at most WORKING_QUIET_S old
           (OPEN_TOOL_QUIET_S with an open tool call). Observer, bridge and
           gateway records never count: they can be written while the
           agent itself is silent (cc-astra A89).
  idle     live and no open turn
  unknown  anything else: an open turn whose engine has gone quiet, no
           engine event found, a missing or malformed field, or no usable
           source
The judge model's state is never used: it is inferred. A tmux session's
existence is never used.

It writes ONE document, atomically, in its own folder, mode 0600, at most
16384 bytes, at most once per min_interval_s. Its other files there are its
own id map, its high-water mark and a lock. It opens no port, starts no process, reads no
database, no environment file and no credential, and changes nothing it
reads.
"""
import calendar
import errno
import fcntl
import hashlib
import http.client
import json
import math
import os
import re
import stat
import sys
import time
import urllib.parse

VERSION = "1"
ADAPTER = "spawn-agent-presence"
ROLES = ("supervisor", "worker", "rsi")      # anything else becomes "other"
STATES = ("idle", "working", "offline", "unknown")
SOURCE_STATUSES = ("ok", "stale", "unavailable")
MAX_BYTES = 16384
MAX_AGENTS = 64
MAX_IDS = 4096
MAX_SESSION_READS = 8          # open-turn agents checked per run; more are unknown
SESSION_WINDOW = 64            # newest session positions read per open-turn agent
WORKING_QUIET_S = 900          # an open turn whose engine is quiet for longer is unknown
OPEN_TOOL_QUIET_S = 3600       # ... unless a tool call is open, up to this
OFFLINE_WINDOW_S = 86400       # ended agents are listed (offline) for this long
MAX_TIME_MS = 4102444800000    # 2100-01-01; anything later is not a time
SAFE_ID = re.compile(r"^[A-Za-z0-9_.:-]{1,128}$")
SAFE_ENGINE = re.compile(r"^[a-z0-9_-]{1,32}$")
EPOCH = re.compile(r"^[0-9a-f]{16}$")


def default_config():
    home = os.path.expanduser("~")
    return {
        "out_dir": os.path.join(home, ".local/state/spawn-agent-presence"),
        "disable_file": os.path.join(home, ".config/spawn-agent-presence/disabled"),
        "port": 8787,
        "timeout_s": 5.0,
        "max_source_bytes": 1048576,
        "max_session_bytes": 262144,
        "min_interval_s": 10.0,
        "stale_after_s": 60,        # the timer runs every 15 s
        "carry_s": 300,             # on a failed read, ids/roles are carried this long, as unknown
        "min_free_bytes": 1024 * 1024 * 1024,
    }


class TooLarge(Exception):
    pass


def get_json(cfg, path, cap):
    """One GET to the local page service. Returns a parsed JSON object or None;
    raises TooLarge when the answer exceeds cap. The host is fixed; a redirect
    is never followed (anything but 200 fails)."""
    conn = None
    try:
        conn = http.client.HTTPConnection("127.0.0.1", cfg["port"], timeout=cfg["timeout_s"])
        conn.request("GET", path, headers={"Accept": "application/json"})
        resp = conn.getresponse()
        if resp.status != 200:
            return None
        raw = resp.read(cap + 1)
        if len(raw) > cap:
            raise TooLarge()
        doc = json.loads(raw.decode("utf-8"))
        return doc if isinstance(doc, dict) else None
    except (OSError, http.client.HTTPException, ValueError):
        return None
    finally:
        if conn is not None:
            try:
                conn.close()
            except OSError:
                pass


def ms(v):
    """A source time in ms, or None. Booleans, non-numbers, negatives and
    far-future values are not times."""
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        return None
    if not math.isfinite(v) or v <= 0 or v > MAX_TIME_MS:
        return None
    return int(v)


def seq(v):
    if isinstance(v, bool) or not isinstance(v, int) or v < 0:
        return None
    return v


def utc(t):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t))


def parse_utc(s):
    try:
        return calendar.timegm(time.strptime(s, "%Y-%m-%dT%H:%M:%SZ"))
    except (TypeError, ValueError):
        return None


def engine_last_ms(cfg, agent):
    """The newest event written by this agent's own engine in its current
    execution, from the session's newest SESSION_WINDOW positions; or None.
    Two small GETs; only ids that pass SAFE_ID are put in a path, encoded."""
    sid, ex, eng = agent.get("session_id"), agent.get("execution_id"), agent.get("engine")
    if not (isinstance(sid, str) and SAFE_ID.match(sid) and sid.strip(".") and isinstance(ex, str)
            and SAFE_ID.match(ex) and isinstance(eng, str) and SAFE_ENGINE.match(eng)):
        return None
    base = "/api/session/" + urllib.parse.quote(sid, safe="")
    try:
        head = get_json(cfg, base + "?limit=1", cfg["max_session_bytes"])
        fresh = head.get("freshness") if head else None
        last = seq(fresh.get("last_session_seq")) if isinstance(fresh, dict) else None
        if last is None:
            return None
        page = get_json(cfg, "%s?after=%d&limit=%d" % (base, max(0, last - SESSION_WINDOW), SESSION_WINDOW),
                        cfg["max_session_bytes"])
    except TooLarge:
        return None
    tl = page.get("timeline") if page else None
    if not isinstance(tl, list):
        return None
    best = None
    for e in tl[:SESSION_WINDOW]:
        if isinstance(e, dict) and e.get("source") == eng and e.get("execution_id") == ex:
            t = ms(e.get("recorded_at_ms"))
            if t is not None and (best is None or t > best):
                best = t
    return best


def classify(agent, now, engine_ms):
    """One source agent -> state, from recorded events only. engine_ms is the
    newest own-engine event of the current execution (or None). Never raises."""
    if not isinstance(agent, dict):
        return "unknown"
    live = agent.get("live")
    act = agent.get("activity")
    act = act if isinstance(act, dict) else {}
    if live is False:
        return "offline"
    if live is not True:
        return "unknown"
    turn_open = act.get("turn_open")
    if turn_open is False:
        return "idle"
    if turn_open is not True or engine_ms is None:
        return "unknown"
    quiet = now - engine_ms / 1000.0
    tool = act.get("current_tool")
    limit = OPEN_TOOL_QUIET_S if isinstance(tool, str) and tool else WORKING_QUIET_S
    return "working" if -60 <= quiet <= limit else "unknown"


def role_of(agent):
    r = agent.get("role") if isinstance(agent, dict) else None
    return r if r in ROLES else "other"


def key_of(agent):
    """A digest of the source's agent id, used only inside our own id map.
    The raw id is never written anywhere."""
    if not isinstance(agent, dict):
        return None
    a = agent.get("agent_id")
    if not isinstance(a, str) or not a or len(a) > 256:
        return None
    return hashlib.sha256(a.encode("utf-8")).hexdigest()[:32]


def select(agents, now):
    """Live agents, then agents that ended within OFFLINE_WINDOW_S, newest end
    first; at most MAX_AGENTS. Agents without a usable id are left out."""
    if not isinstance(agents, list):
        return None
    live, ended = [], []
    for a in agents[:4096]:
        if not isinstance(a, dict) or key_of(a) is None:
            continue
        if a.get("live") is False:
            e = ms(a.get("ended_at_ms"))
            if e is not None and 0 <= now - e / 1000.0 <= OFFLINE_WINDOW_S:
                ended.append((e, a))
        else:
            live.append(a)          # live True, or a malformed flag (-> unknown)
    ended.sort(key=lambda p: -p[0])
    return (live + [a for _, a in ended])[:MAX_AGENTS]


def new_epoch():
    return os.urandom(8).hex()


def load_ids(path):
    """Our id map: (ids, next, epoch). A missing or damaged map starts a NEW
    identity epoch, so old public ids are never given to other agents
    (cc-astra A90): consumers key identity on (id_epoch, id)."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise OSError(errno.EINVAL, "not a regular file")
            raw = os.read(fd, 1 << 20)
        finally:
            os.close(fd)
        doc = json.loads(raw.decode("utf-8"))
        ids, nxt, ep = doc.get("ids"), doc.get("next"), doc.get("epoch")
        if isinstance(ids, dict) and isinstance(nxt, int) and not isinstance(nxt, bool) and nxt >= 1 \
                and isinstance(ep, str) and EPOCH.match(ep):
            clean = {k: v for k, v in ids.items()
                     if isinstance(k, str) and len(k) == 32 and isinstance(v, int)
                     and not isinstance(v, bool) and 1 <= v < nxt}
            if len(clean) == len(ids):
                return clean, nxt, ep, False
    except (OSError, ValueError, AttributeError):
        pass
    return {}, 1, new_epoch(), True


def load_issued(path):
    """The high-water mark: (epoch, next) of the highest number ever issued, kept
    apart from the map and written before it. None when missing or damaged."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                return None
            raw = os.read(fd, 4096)
        finally:
            os.close(fd)
        doc = json.loads(raw.decode("utf-8"))
        ep, nxt = doc.get("epoch"), doc.get("next")
        if isinstance(ep, str) and EPOCH.match(ep) and isinstance(nxt, int) and not isinstance(nxt, bool) \
                and nxt >= 1 and set(doc) == {"epoch", "next"}:
            return ep, nxt
    except (OSError, ValueError, AttributeError):
        pass
    return None


def regressed(issued, epoch, nxt):
    """True when the map cannot be trusted against the high-water mark: the mark
    is missing or damaged, names another epoch, or is ahead of the map (a map
    restored from an older copy, even after the higher ids left the document;
    cc-astra A90 R4). Then a new epoch is started."""
    return issued is None or issued[0] != epoch or nxt < issued[1]


def behind(prev, epoch, nxt):
    """True when our own last document, in the same epoch, already used a number
    the map would hand out again (a map rolled back to an older copy). Then a
    new epoch is started rather than reusing a published id (rev-e r3n1)."""
    if not prev or prev.get("id_epoch") != epoch:
        return False
    used = [int(r["id"][1:]) for r in prev.get("agents", [])]
    return bool(used) and max(used) >= nxt


def assign(keys, ids, nxt):
    """Stable opaque numbers: a known key keeps its number; a new key gets the
    next one. Numbers are never reused within an epoch. The map is bounded:
    when it is full, the lowest numbers not wanted now are dropped."""
    changed = False
    for k in keys:
        if k not in ids:
            ids[k] = nxt
            nxt += 1
            changed = True
    if len(ids) > MAX_IDS:
        want = set(keys)
        for k, _ in sorted(ids.items(), key=lambda p: p[1]):
            if len(ids) <= MAX_IDS:
                break
            if k not in want:
                del ids[k]
        changed = True
    return ids, nxt, changed


def project(cfg, source, ids, nxt, now, engine_reader=engine_last_ms):
    """Source document -> (agents list or None, ids, nxt, ids_changed)."""
    chosen = select(source.get("agents"), now)
    if chosen is None:
        return None, ids, nxt, False
    ids, nxt, changed = assign([key_of(a) for a in chosen], ids, nxt)
    out, reads = [], 0
    for a in chosen:
        engine_ms = None
        act = a.get("activity") if isinstance(a.get("activity"), dict) else {}
        if a.get("live") is True and act.get("turn_open") is True and reads < MAX_SESSION_READS:
            reads += 1
            engine_ms = engine_reader(cfg, a)
        out.append({"id": "a%d" % ids[key_of(a)], "role": role_of(a),
                    "state": classify(a, now, engine_ms), "evidence": "observed"})
    out.sort(key=lambda r: int(r["id"][1:]))
    return out, ids, nxt, changed


def valid(doc):
    """The contract's rules, checked on our own document before it is written."""
    try:
        if set(doc) != {"version", "generated_utc", "source_last_ok_utc", "source_status",
                        "stale_after_s", "provenance", "id_epoch", "agents"}:
            return False
        if doc["version"] != VERSION or parse_utc(doc["generated_utc"]) is None:
            return False
        if doc["provenance"] != {"adapter": ADAPTER, "version": VERSION}:
            return False
        ep = doc["id_epoch"]
        if ep is not None and not (isinstance(ep, str) and EPOCH.match(ep)):
            return False
        if doc["source_last_ok_utc"] is not None and parse_utc(doc["source_last_ok_utc"]) is None:
            return False
        if doc["source_status"] not in SOURCE_STATUSES:
            return False
        sa = doc["stale_after_s"]
        if isinstance(sa, bool) or not isinstance(sa, int) or not 30 <= sa <= 600:
            return False
        gen = parse_utc(doc["generated_utc"])
        if doc["source_status"] == "ok":
            last = parse_utc(doc["source_last_ok_utc"])
            if last is None or gen - last > sa:
                return False
        agents = doc["agents"]
        if not isinstance(agents, list) or len(agents) > MAX_AGENTS:
            return False
        if doc["source_status"] == "unavailable" and agents:
            return False
        if agents and ep is None:
            return False
        seen = set()
        for r in agents:
            if set(r) != {"id", "role", "state", "evidence"}:
                return False
            i = r["id"]
            if not isinstance(i, str) or len(i) > 12 or not i.startswith("a") or not i[1:].isdigit() \
                    or i[1:].startswith("0") or i in seen:
                return False
            seen.add(i)
            if r["role"] not in ROLES + ("other",) or r["state"] not in STATES or r["evidence"] != "observed":
                return False
            if doc["source_status"] != "ok" and r["state"] != "unknown":
                return False
        return len(json.dumps(doc, separators=(",", ":")).encode()) <= MAX_BYTES
    except (KeyError, TypeError, AttributeError):
        return False


def previous(path):
    """Our own last output, if it is a valid document. Never the raw source."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            raw = os.read(fd, MAX_BYTES + 1)
        finally:
            os.close(fd)
        doc = json.loads(raw.decode("utf-8"))
        return doc if isinstance(doc, dict) and valid(doc) else None
    except (OSError, ValueError):
        return None


def build(cfg, agents, epoch, prev, now):
    """agents: the projected list, or None when the source failed."""
    doc = {"version": VERSION, "generated_utc": utc(now), "source_last_ok_utc": None,
           "source_status": "unavailable", "stale_after_s": cfg["stale_after_s"],
           "provenance": {"adapter": ADAPTER, "version": VERSION}, "id_epoch": None, "agents": []}
    if agents is not None:
        doc.update(source_status="ok", source_last_ok_utc=utc(now), agents=agents, id_epoch=epoch)
        return doc
    if prev is not None and prev.get("source_last_ok_utc"):
        last = parse_utc(prev["source_last_ok_utc"])
        doc["source_last_ok_utc"] = prev["source_last_ok_utc"]      # carried, never refreshed
        if last is not None and 0 <= now - last <= cfg["carry_s"] and prev.get("id_epoch"):
            # Who was there is carried for a while, in the same epoch; what they do is not known.
            doc["source_status"] = "stale"
            doc["id_epoch"] = prev["id_epoch"]
            doc["agents"] = [{"id": r["id"], "role": r["role"], "state": "unknown",
                              "evidence": "observed"} for r in prev["agents"]]
    return doc


def write_atomic(path, data):
    tmp = path + ".tmp"
    try:
        os.unlink(tmp)
    except FileNotFoundError:
        pass
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        if os.write(fd, data) != len(data):
            raise OSError(errno.EIO, "short write")
        os.fsync(fd)
    finally:
        os.close(fd)
    os.replace(tmp, path)
    # The folder too (rev-e R5 n1): the rename itself survives a power loss, so
    # the order mark -> map -> document holds on disk, not only in this process.
    dfd = os.open(os.path.dirname(path) or ".", os.O_RDONLY)
    try:
        os.fsync(dfd)
    finally:
        os.close(dfd)


def export(cfg, engine_reader=engine_last_ms):
    """One run. Returns a fixed word; writes at most the document and the id map."""
    if os.path.lexists(cfg["disable_file"]):
        return "disabled"
    d = cfg["out_dir"]
    try:
        old = os.umask(0o077)
        try:
            os.makedirs(d, mode=0o700, exist_ok=True)
        finally:
            os.umask(old)
        st = os.lstat(d)
        if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
            return "out_dir_refused"
        lock = os.open(os.path.join(d, "lock"), os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    except OSError:
        return "no_out_dir"
    try:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return "busy"
        path = os.path.join(d, "presence.json")
        ids_path = os.path.join(d, "ids.json")
        issued_path = os.path.join(d, "issued.json")
        now = time.time()
        try:
            if 0 <= now - os.lstat(path).st_mtime < cfg["min_interval_s"]:
                return "too_soon"
        except OSError:
            pass
        try:
            sv = os.statvfs(d)
            if sv.f_bavail * sv.f_frsize < cfg["min_free_bytes"]:
                return "disk_low_no_write"
        except OSError:
            return "no_out_dir"
        prev = previous(path)
        too_large = False
        try:
            source = get_json(cfg, "/api/fleet", cfg["max_source_bytes"])
        except TooLarge:
            source, too_large = None, True
        now = time.time()
        agents, epoch, ids, nxt, ids_changed = None, None, None, None, False
        if source is not None:
            ids, nxt, epoch, fresh_epoch = load_ids(ids_path)
            if not fresh_epoch and (regressed(load_issued(issued_path), epoch, nxt) or behind(prev, epoch, nxt)):
                ids, nxt, epoch, fresh_epoch = {}, 1, new_epoch(), True
            agents, ids, nxt, ids_changed = project(cfg, source, ids, nxt, now, engine_reader)
            ids_changed = ids_changed or fresh_epoch
        doc = build(cfg, agents, epoch, prev, now)
        result = "ok" if agents is not None else ("source_too_large" if too_large else doc["source_status"])
        if not valid(doc):
            doc = build(cfg, None, None, None, now)       # never a bad document
            result, ids_changed = "invalid_projection", False
            if not valid(doc):
                return "invalid_no_write"
        data = (json.dumps(doc, separators=(",", ":"), sort_keys=True) + "\n").encode("utf-8")
        if len(data) > MAX_BYTES:
            return "too_large_no_write"
        try:
            if ids_changed:
                # The high-water mark first, then the map, then the document: a
                # document never names a number or epoch the mark and map lost.
                write_atomic(issued_path, json.dumps({"epoch": epoch, "next": nxt},
                                                     sort_keys=True).encode("utf-8"))
                write_atomic(ids_path, json.dumps({"epoch": epoch, "next": nxt, "ids": ids},
                                                  sort_keys=True).encode("utf-8"))
            write_atomic(path, data)
        except OSError as e:
            return "disk_full" if e.errno == errno.ENOSPC else "write_error"
        return result
    finally:
        os.close(lock)


if __name__ == "__main__":
    word = export(default_config())
    if word not in ("ok", "disabled", "too_soon"):
        print("agent_presence: %s" % word)
    sys.exit(0)
