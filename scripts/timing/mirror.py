"""Adversarial timing: the transcript lags the screen.

The spawn shim (scripts/timing/timing.sh) points a Bridge's
SPAWN_CLAUDE_PROJECTS at a mirror of the projects directory the engine
writes, and starts this daemon, which copies each complete line the engine
appends to the mirror late, in order per file: a seeded delay per line, a
longer one for a line that ends a turn (`turn_duration`). So the screen can
show the prompt before the transcript says the turn ended. Lines already
there when the daemon starts are copied at once.

One daemon per mirror (an exclusive lock); it exits when the source's
parent directory or the timing directory is gone, or after six hours.

usage: mirror.py <timing-dir> <source> <mirror>
"""

import fcntl
import os
import sys
import time
import zlib



def config(directory):
    values = {}
    with open(os.path.join(directory, "config")) as handle:
        for line in handle:
            line = line.strip()
            if "=" in line and not line.startswith("#"):
                key, _, value = line.partition("=")
                values[key] = value.strip("'\"")
    return values


def main():
    directory, source, mirror = sys.argv[1:4]
    os.makedirs(mirror, exist_ok=True)
    lock = open(os.path.join(mirror, ".timing-mirror.lock"), "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        return
    values = config(directory)
    seed = values.get("TIMING_SEED", "0")
    scale = int(values.get("TIMING_SCALE", "100"))
    line_p = int(values.get("P_TRANSCRIPT_LINE", "0"))
    line_range = (int(values.get("TRANSCRIPT_LINE_MIN", "0")), int(values.get("TRANSCRIPT_LINE_MAX", "0")))
    end_p = int(values.get("P_TRANSCRIPT_END", "0"))
    end_range = (int(values.get("TRANSCRIPT_END_MIN", "0")), int(values.get("TRANSCRIPT_END_MAX", "0")))
    log_path = os.path.join(directory, "perturbations.log")
    parent = os.path.dirname(os.path.abspath(source))

    def roll(salt):
        return zlib.crc32(f"{seed}:{salt}".encode()) % 1000

    def log(key, action, detail):
        stamp = time.strftime("%H:%M:%S") + ".%03d" % (int(time.time() * 1000) % 1000)
        try:
            with open(log_path, "a") as handle:
                handle.write(f"{stamp} transcript#{key} {action} {detail}\n")
        except OSError:
            pass

    offsets = {}  # relative path -> bytes of the source consumed
    partial = {}  # relative path -> bytes after the last newline
    counts = {}  # relative path -> lines seen
    pending = {}  # relative path -> [(due, bytes)]
    last_due = {}
    started = time.monotonic()
    first = True
    while os.path.isdir(parent) and os.path.isdir(directory):
        if time.monotonic() - started > 6 * 3600:
            return
        now = time.monotonic()
        for root, _dirs, files in os.walk(source):
            for name in files:
                path = os.path.join(root, name)
                relative = os.path.relpath(path, source)
                try:
                    size = os.path.getsize(path)
                except OSError:
                    continue
                offset = offsets.get(relative, 0)
                if size < offset:
                    offset = 0
                    partial[relative] = b""
                if size == offset:
                    continue
                try:
                    with open(path, "rb") as handle:
                        handle.seek(offset)
                        data = handle.read(size - offset)
                except OSError:
                    continue
                offsets[relative] = offset + len(data)
                data = partial.get(relative, b"") + data
                *lines, rest = data.split(b"\n")
                partial[relative] = rest
                for line in lines:
                    line += b"\n"
                    n = counts.get(relative, 0) + 1
                    counts[relative] = n
                    key = f"{os.path.basename(relative)}.{n}"
                    delay = 0
                    if not first:
                        if b"turn_duration" in line and roll(f"transcript:{relative}:{n}:end") < end_p:
                            low, high = end_range
                            delay = (low + roll(f"transcript:{relative}:{n}:end:delay") * (high - low) // 999) * scale // 100
                            log(key, f"delay={delay}ms", "turn end lags the screen")
                        elif roll(f"transcript:{relative}:{n}") < line_p:
                            low, high = line_range
                            delay = (low + roll(f"transcript:{relative}:{n}:delay") * (high - low) // 999) * scale // 100
                            log(key, f"delay={delay}ms", "transcript line lags")
                    due = max(now + delay / 1000.0, last_due.get(relative, 0.0))
                    last_due[relative] = due
                    pending.setdefault(relative, []).append((due, line))
        first = False
        now = time.monotonic()
        for relative, queue in pending.items():
            ready = b""
            while queue and queue[0][0] <= now:
                ready += queue.pop(0)[1]
            if ready:
                target = os.path.join(mirror, relative)
                os.makedirs(os.path.dirname(target), exist_ok=True)
                with open(target, "ab") as handle:
                    handle.write(ready)
        time.sleep(0.05)


if __name__ == "__main__":
    main()
