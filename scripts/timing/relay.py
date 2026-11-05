"""Adversarial timing: the screen lags the engine.

The engine proxy (scripts/timing/timing.sh, `engine-proxy`) points the
engine's stdout and stderr at this relay, which writes them to the pane
late, in order: each chunk the engine writes is held by a seeded delay, and a
chunk that draws the prompt (`❯`) by a longer one, so the screen can still
read Busy after the transcript says the turn ended, and the prompt can show
late after a turn's end. What is typed into the pane is echoed by the
terminal at once, as it is with a real engine.

usage: relay.py <timing-dir> <name>
"""

import os
import sys
import threading
import time
import zlib

PROMPT = "❯".encode()


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
    directory, name = sys.argv[1], sys.argv[2]
    values = config(directory)
    seed = values.get("TIMING_SEED", "0")
    scale = int(values.get("TIMING_SCALE", "100"))
    chunk_p = int(values.get("P_SCREEN_CHUNK", "0"))
    chunk_range = (int(values.get("SCREEN_CHUNK_MIN", "0")), int(values.get("SCREEN_CHUNK_MAX", "0")))
    prompt_p = int(values.get("P_SCREEN_PROMPT", "0"))
    prompt_range = (int(values.get("SCREEN_PROMPT_MIN", "0")), int(values.get("SCREEN_PROMPT_MAX", "0")))
    log_path = os.path.join(directory, "perturbations.log")

    def roll(salt):
        return zlib.crc32(f"{seed}:{salt}".encode()) % 1000

    def log(n, action, detail):
        stamp = time.strftime("%H:%M:%S") + ".%03d" % (int(time.time() * 1000) % 1000)
        try:
            with open(log_path, "a") as handle:
                handle.write(f"{stamp} screen#{name}.{n} {action} {detail}\n")
        except OSError:
            pass

    queue = []
    lock = threading.Condition()
    done = [False]

    def writer():
        last = 0.0
        while True:
            with lock:
                while not queue and not done[0]:
                    lock.wait()
                if not queue:
                    return
                due, data = queue.pop(0)
            due = max(due, last)
            pause = due - time.monotonic()
            if pause > 0:
                time.sleep(pause)
            last = due
            try:
                os.write(1, data)
            except OSError:
                return

    thread = threading.Thread(target=writer, daemon=True)
    thread.start()
    n = 0
    while True:
        try:
            data = os.read(0, 65536)
        except OSError:
            break
        if not data:
            break
        n += 1
        delay = 0
        if PROMPT in data and roll(f"screen:{name}:{n}:prompt") < prompt_p:
            low, high = prompt_range
            delay = (low + roll(f"screen:{name}:{n}:prompt:delay") * (high - low) // 999) * scale // 100
            log(n, f"delay={delay}ms", "prompt shown late")
        elif roll(f"screen:{name}:{n}") < chunk_p:
            low, high = chunk_range
            delay = (low + roll(f"screen:{name}:{n}:delay") * (high - low) // 999) * scale // 100
            log(n, f"delay={delay}ms", f"screen lags ({len(data)} bytes)")
        with lock:
            queue.append((time.monotonic() + delay / 1000.0, data))
            lock.notify()
    with lock:
        done[0] = True
        lock.notify()
    thread.join()


if __name__ == "__main__":
    main()
