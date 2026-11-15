#!/usr/bin/env python3
"""shadow_observe: one passive, read-only observation of the Spawn host.

Phase 1 of the heartbeat design (owner order 2026-11-15). It OBSERVES and
RECORDS. It has no action path: it restarts, stops, launches, retries,
prompts, deploys and repairs nothing, and it calls no model and no provider.

What it reads (an allowlist; nothing else):
  - `systemctl --user show <unit>` for a fixed list of units, four properties
  - the kernel boot id, the monotonic clock, loadavg, meminfo, statvfs
  - mtime and size (never content) of a fixed list of files and folders
  - the commit time of the mission store's main (`git log -1 --format=%ct`)
  - tmux pane metadata (dead flag, last activity) of two named sessions;
    never pane text
  - the HTTP status of the local page (GET /, no token, 127.0.0.1 only)
It reads no environment, no argv, no credential file, no log text, no
transcript and no database.

What it writes: only its own folder (state.json, lock, samples-*.jsonl),
bounded in size. Every field of a sample is a number, a boolean, or a short
word checked against a pattern, so free text can never enter the telemetry.

A probe that times out or returns something malformed yields "unknown".
Unknown is a result, never a reason to do anything.
"""
import errno
import fcntl
import json
import os
import re
import signal
import subprocess
import sys
import time

VERSION = 1
UNITS = ("spawn-loop.service", "spawn-serve.service", "spawn-integrate.service",
         "spawn-jev-relay.service", "spawn-model-relay.service")
SESSIONS = ("supervisor", "cc-astra")
SHOW_PROPS = "ActiveState,SubState,NRestarts,ActiveEnterTimestampMonotonic"
WORD = re.compile(r"^[a-z][a-z-]{0,23}$")
PANE_FORMAT = "#{window_index}.#{pane_index} #{pane_dead} #{window_activity}"
# The only commands this program can ever start, as whole argument lists.
# None stands for the one part that varies, and each varying part is checked
# in allowed(). run_probe refuses anything else.
ALLOWED = (
    ("systemctl", "--user", "show", None, "-p", SHOW_PROPS),
    ("git", "--git-dir", None, "log", "-1", "--format=%ct", "refs/heads/main"),
    ("tmux", "list-panes", "-s", "-t", None, "-F", PANE_FORMAT),
)


def allowed(cfg, argv):
    for tpl in ALLOWED:
        if len(argv) == len(tpl) and all(t is None or t == a for t, a in zip(tpl, argv)):
            var = argv[tpl.index(None)]
            return {"systemctl": var in UNITS, "git": var == cfg["store"], "tmux": var in SESSIONS}[tpl[0]]
    return False


def default_config():
    home = os.path.expanduser("~")
    base = os.path.join(home, ".local/share/spawn-rsi")
    return {
        "state_dir": os.path.join(home, ".local/state/spawn-shadow"),
        "disable_file": os.path.join(home, ".config/spawn-shadow/disabled"),
        "exclude_file": os.path.join(home, ".config/spawn-shadow/exclude"),
        "tools": {"systemctl": "/usr/bin/systemctl", "git": "/usr/bin/git", "tmux": "/usr/bin/tmux"},
        "store": os.path.join(base, "missions/spawn.git"),
        "paths": {"queue": "/var/tmp/spawn/sync/queue.md",
                  "loop_log": os.path.join(base, "loop.log"),
                  "gate_logs": os.path.join(base, "ws/gate-logs"),
                  "keep_test_version": "/run/keep-test/VERSION"},
        "page": ("127.0.0.1", 8787),
        "boot_id_file": "/proc/sys/kernel/random/boot_id",
        "probe_timeout_s": 5.0,       # candidate, unmeasured
        "run_deadline_s": 20.0,       # candidate
        "interval_s": 30.0,           # the timer's period; used for the gap flag
        "max_line_bytes": 8192,
        "max_total_bytes": 50 * 1024 * 1024,
        "min_free_bytes": 1024 * 1024 * 1024,
        "main_moved_recent_s": 2 * 3600,  # candidates for the inferred label
        "activity_recent_s": 15 * 60,
    }


def run_probe(cfg, argv, deadline):
    """Run one allowlisted read-only command in its own process group with a
    hard timeout. Returns (result, stdout). On timeout it kills its OWN
    child's group only. It never touches any other process."""
    if not allowed(cfg, argv):
        raise RuntimeError("probe refused: not on the allowlist")
    left = min(cfg["probe_timeout_s"], deadline - time.monotonic())
    if left <= 0:
        return "skipped_deadline", ""
    real = [cfg["tools"][argv[0]]] + list(argv[1:])
    env = {"PATH": "/usr/bin:/bin", "LC_ALL": "C", "GIT_OPTIONAL_LOCKS": "0"}
    for k in ("HOME", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "TMUX_TMPDIR"):
        if k in os.environ:
            env[k] = os.environ[k]
    try:
        child = subprocess.Popen(real, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL, env=env, start_new_session=True)
    except OSError:
        return "error", ""
    try:
        out, _ = child.communicate(timeout=left)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(child.pid, signal.SIGKILL)   # our own probe's group
        except OSError:
            pass
        try:
            child.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        return "timeout", ""
    if child.returncode != 0:
        return "error", ""
    return "ok", out[:65536].decode("ascii", "replace")


def word(s):
    return s if isinstance(s, str) and WORD.match(s) else None


def num(s, lo=0, hi=2**62):
    try:
        n = int(s)
    except (TypeError, ValueError):
        return None
    return n if lo <= n <= hi else None


def probe_unit(cfg, unit, deadline):
    res, out = run_probe(cfg, ["systemctl", "--user", "show", unit, "-p", SHOW_PROPS], deadline)
    c = {"probe": res, "basis": "unknown", "process": "unknown"}
    if res != "ok":
        return c
    kv = dict(l.split("=", 1) for l in out.splitlines() if "=" in l)
    active, sub, restarts = word(kv.get("ActiveState")), word(kv.get("SubState")), num(kv.get("NRestarts"))
    if active is None or sub is None or restarts is None:
        c["probe"] = "malformed"
        return c
    c.update(basis="observed", active=active, sub=sub, restarts=restarts,
             process="up" if (active, sub) == ("active", "running") else "down")
    since = num(kv.get("ActiveEnterTimestampMonotonic"))
    if since is not None:
        c["active_enter_mono_us"] = since
    return c


def probe_path(path, now):
    try:
        st = os.stat(path)
    except OSError:
        return {"probe": "absent", "basis": "observed"}
    return {"probe": "ok", "basis": "observed", "age_s": max(0, int(now - st.st_mtime)), "size": st.st_size}


def probe_dir_newest(path, now):
    """Age of the newest entry of a folder, by mtime of at most 5000 entries."""
    try:
        newest, n = 0.0, 0
        with os.scandir(path) as it:
            for e in it:
                n += 1
                if n > 5000:
                    break
                newest = max(newest, e.stat(follow_symlinks=False).st_mtime)
    except OSError:
        return {"probe": "absent", "basis": "observed"}
    if not n:
        return {"probe": "ok", "basis": "observed", "entries": 0}
    return {"probe": "ok", "basis": "observed", "entries": min(n, 5000), "age_s": max(0, int(now - newest))}


def probe_store(cfg, now, deadline):
    res, out = run_probe(cfg, ["git", "--git-dir", cfg["store"], "log", "-1", "--format=%ct",
                               "refs/heads/main"], deadline)
    c = {"probe": res, "basis": "unknown"}
    if res != "ok":
        return c
    ct = num(out.strip(), lo=1, hi=int(now) + 86400)
    if ct is None:
        c["probe"] = "malformed"
        return c
    c.update(basis="observed", age_s=max(0, int(now - ct)))
    return c


def probe_session(cfg, session, now, deadline):
    res, out = run_probe(cfg, ["tmux", "list-panes", "-s", "-t", session, "-F", PANE_FORMAT], deadline)
    c = {"probe": res, "basis": "unknown"}
    if res != "ok":
        return c
    panes, bad = {}, 0
    for line in out.splitlines()[:32]:
        m = re.match(r"^(\d{1,3}\.\d{1,3}) ([01]) (\d{9,11})$", line)
        if not m:
            bad += 1
            continue
        panes[m.group(1)] = {"dead": m.group(2) == "1", "activity_age_s": max(0, int(now - int(m.group(3))))}
    if not panes:
        c["probe"] = "malformed"
        return c
    c.update(basis="observed", panes=panes, malformed_lines=bad)
    return c


def probe_page(cfg, deadline):
    import http.client
    left = min(3.0, deadline - time.monotonic())
    if left <= 0:
        return {"probe": "skipped_deadline", "basis": "unknown"}
    host, port = cfg["page"]
    if host != "127.0.0.1":
        return {"probe": "refused", "basis": "unknown"}
    try:
        conn = http.client.HTTPConnection(host, port, timeout=left)
        conn.request("GET", "/")
        status = conn.getresponse().status
        conn.close()
    except (OSError, http.client.HTTPException):
        return {"probe": "error", "basis": "observed", "read_path": "not_answering"}
    return {"probe": "ok", "basis": "observed", "http_status": status,
            "read_path": "answering" if status == 200 else "unexpected_status"}


def read_excludes(cfg):
    """Components the owner has excluded (stopped, tombstoned). One name per
    line. An unreadable file excludes everything: fail closed."""
    try:
        with open(cfg["exclude_file"]) as fh:
            return {l.strip() for l in fh if l.strip() and not l.startswith("#")}, False
    except FileNotFoundError:
        return set(), False
    except OSError:
        return set(), True


def host_facts(cfg):
    h = {}
    try:
        st = os.statvfs(cfg["state_dir"])
        h["disk_free_bytes"] = st.f_bavail * st.f_frsize
    except OSError:
        pass
    try:
        with open("/proc/meminfo") as fh:
            for line in fh:
                if line.startswith("MemAvailable:"):
                    h["mem_available_kb"] = num(line.split()[1])
        with open("/proc/loadavg") as fh:
            h["load1_x100"] = int(float(fh.read().split()[0]) * 100)
    except (OSError, ValueError, IndexError):
        pass
    return h


def load_state(cfg):
    try:
        with open(os.path.join(cfg["state_dir"], "state.json")) as fh:
            s = json.load(fh)
        if s.get("v") == VERSION and isinstance(s.get("seq"), int) and isinstance(s.get("boot_id"), str):
            return s, "ok"
        return None, "unknown_version"
    except FileNotFoundError:
        return None, "first_run"
    except (OSError, ValueError):
        return None, "corrupt"


def save_state(cfg, state):
    path = os.path.join(cfg["state_dir"], "state.json")
    fd = os.open(path + ".tmp", os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    try:
        os.write(fd, json.dumps(state).encode())
        os.fsync(fd)
    finally:
        os.close(fd)
    os.replace(path + ".tmp", path)


def enforce_total(cfg):
    """Keep this program's own samples under the cap by removing its own
    oldest sample files. It never removes anything else."""
    d = cfg["state_dir"]
    files = sorted(f for f in os.listdir(d) if re.match(r"^samples-\d{8}\.jsonl$", f))
    total = sum(os.path.getsize(os.path.join(d, f)) for f in files)
    removed = 0
    while files and total > cfg["max_total_bytes"] and len(files) > 1:
        victim = files.pop(0)
        total -= os.path.getsize(os.path.join(d, victim))
        os.unlink(os.path.join(d, victim))
        removed += 1
    return total, removed


def classify(cfg, comps):
    """An inferred label from two ages. It claims nothing about usefulness.
    "main moved" is the committer time of main's tip: it says a commit made
    then is on main now. It does NOT say a gate checked that commit, and
    the gate folder's mtime is not linked to it. So the only labels are
    main_moved_recent / activity_only / quiet / unknown; no label and no
    field name is a word of achievement."""
    m = comps.get("store_main", {}).get("age_s")
    acts = [comps.get(k, {}).get("age_s") for k in ("queue", "loop_log", "gate_logs")]
    for s in SESSIONS:
        for pane in comps.get("session:" + s, {}).get("panes", {}).values():
            acts.append(pane["activity_age_s"])
    acts = [a for a in acts if a is not None]
    out = {"basis": "inferred", "main_tip_commit_age_s": m, "activity_age_s": min(acts) if acts else None}
    if m is None:
        out["label"] = "unknown"
    elif m <= cfg["main_moved_recent_s"]:
        out["label"] = "main_moved_recent"
    elif acts and min(acts) <= cfg["activity_recent_s"]:
        out["label"] = "activity_only"
    else:
        out["label"] = "quiet"
    return out


def observe(cfg):
    """One observation. Returns an exit word; writes at most one sample line."""
    if os.path.lexists(cfg["disable_file"]):
        return "disabled"
    t0 = time.monotonic()
    deadline = t0 + cfg["run_deadline_s"]
    try:
        old_umask = os.umask(0o077)       # every folder and file it makes is the user's only
        try:
            os.makedirs(cfg["state_dir"], mode=0o700, exist_ok=True)
        finally:
            os.umask(old_umask)
        st = os.lstat(cfg["state_dir"])
        import stat as _stat
        if not _stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid():
            return "state_dir_refused"    # a link, or someone else's: write nothing
        lock = os.open(os.path.join(cfg["state_dir"], "lock"),
                       os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    except OSError:
        return "no_state_dir"
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        os.close(lock)
        return "busy"
    try:
        try:
            with open(cfg["boot_id_file"]) as fh:
                boot = fh.read().strip()
            if not re.match(r"^[0-9a-f-]{36}$", boot):
                boot = "unknown"
        except OSError:
            boot = "unknown"
        prev, state_status = load_state(cfg)
        same_boot = bool(prev) and prev["boot_id"] == boot and boot != "unknown"
        mono = time.monotonic()
        gap = round(mono - prev["mono_s"], 1) if same_boot and isinstance(prev.get("mono_s"), (int, float)) else None
        now = time.time()
        excludes, exclude_unreadable = read_excludes(cfg)
        comps = {}

        def add(name, fn):
            if exclude_unreadable or name in excludes:
                comps[name] = {"probe": "excluded", "basis": "excluded"}
            else:
                comps[name] = fn()

        for u in UNITS:
            add("unit:" + u, lambda u=u: probe_unit(cfg, u, deadline))
        add("page", lambda: probe_page(cfg, deadline))
        add("store_main", lambda: probe_store(cfg, now, deadline))
        for key in ("queue", "loop_log", "keep_test_version"):
            add(key, lambda key=key: probe_path(cfg["paths"][key], now))
        add("gate_logs", lambda: probe_dir_newest(cfg["paths"]["gate_logs"], now))
        for s in SESSIONS:
            add("session:" + s, lambda s=s: probe_session(cfg, s, now, deadline))

        sample = {
            "v": VERSION, "seq": (prev["seq"] + 1) if prev else 1,
            "boot_id": boot, "boot_changed": bool(prev) and not same_boot,
            "state": state_status,
            "wall_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now)),
            "mono_s": round(mono, 1),
            # Monotonic time is compared only inside one boot. A gap much
            # longer than the period means a pause of this observer or a
            # suspend: earlier ages must not be extended across it.
            "gap_s": gap,
            "discontinuity": gap is None or gap > 3 * cfg["interval_s"],
            "components": comps, "movement": classify(cfg, comps), "host": host_facts(cfg),
            "exclude_unreadable": exclude_unreadable,
            "runtime_ms": int((time.monotonic() - t0) * 1000),
        }
        line = json.dumps(sample, separators=(",", ":"), sort_keys=True)
        if len(line) + 1 > cfg["max_line_bytes"]:
            for s in SESSIONS:
                k = "session:" + s
                if "panes" in comps.get(k, {}):
                    comps[k] = {"probe": "truncated", "basis": "unknown"}
            sample["truncated"] = True
            line = json.dumps(sample, separators=(",", ":"), sort_keys=True)
            if len(line) + 1 > cfg["max_line_bytes"]:
                return "sample_too_large"
        free = sample["host"].get("disk_free_bytes")
        if free is None or free < cfg["min_free_bytes"]:
            return "disk_low_no_write"      # nothing written; nothing claimed
        try:
            total, _ = enforce_total(cfg)
            if total + len(line) + 1 > cfg["max_total_bytes"]:
                # Only today's file is left and it is at the cap: it is kept,
                # and nothing more is written until the day changes.
                return "cap_reached_no_write"
            path = os.path.join(cfg["state_dir"], "samples-%s.jsonl" % time.strftime("%Y%m%d", time.gmtime(now)))
            fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW, 0o600)
            try:
                data = (line + "\n").encode()
                if os.write(fd, data) != len(data):
                    return "short_write"
            finally:
                os.close(fd)
            save_state(cfg, {"v": VERSION, "seq": sample["seq"], "boot_id": boot, "mono_s": sample["mono_s"]})
        except OSError as e:
            return "disk_full" if e.errno == errno.ENOSPC else "write_error"
        return "ok"
    finally:
        os.close(lock)


if __name__ == "__main__":
    word_out = observe(default_config())
    if word_out not in ("ok", "disabled"):
        print("shadow_observe: %s" % word_out)
    sys.exit(0)
