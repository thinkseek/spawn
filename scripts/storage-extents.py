#!/usr/bin/env python3
"""The real bytes on disk of each storage source (objective 180).

Run as `python3 -I -B storage-extents.py`, by `spawn loop` once a day at
`nice -n 19` (crates/spawn-cli/src/storage_bytes.rs). The config comes as
JSON on stdin:

    {"bound_secs": <n>, "sources": [{"name": <s>, "roots": [<dir>], "exclude": [<path>]}]}

Each source's roots are walked: symbolic links are never followed, and an
excluded directory is never entered. Each regular file is opened O_RDONLY |
O_NOFOLLOW | O_NONBLOCK, never read, and the file system is asked for its
extents (FIEMAP, FIEMAP_FLAG_SYNC). An extent is counted once across the
whole reading, in the first source that holds it: a reflinked copy adds
nothing, a plain copy adds its bytes. Inline data counts as its length.

It prints one JSON object:

    {"sources": [{"name", "bytes", "files", "unreadable", "seconds"}
                 | {"name", "over_bound": true, "files", "seconds"}]}

A source not done within bound_secs reads over_bound, never a partial count.
`unreadable` counts the files that could not be opened or mapped and the
directories that could not be listed; a directory gone during the walk, or
a root that does not exist, counts nowhere. It never prints a name, a path
or anything a file holds.
"""

import fcntl
import json
import os
import stat
import struct
import sys
import time

FS_IOC_FIEMAP = 0xC020660B
FIEMAP_FLAG_SYNC = 0x1
FIEMAP_EXTENT_LAST = 0x1
FIEMAP_EXTENT_DATA_INLINE = 0x200
HEADER = struct.Struct("=QQIIII")  # fm_start, fm_length, fm_flags, mapped, count, reserved
EXTENT = struct.Struct("=QQQQQIIII")  # logical, physical, length, 2 reserved, flags, 3 reserved
BATCH = 256
WHOLE = 0xFFFFFFFFFFFFFFFF


class OverBound(Exception):
    """The source's bound passed."""


def extents(fd):
    """Every extent of the open file `fd`, as (physical, length, inline)."""
    start = 0
    while True:
        buffer = bytearray(HEADER.size + BATCH * EXTENT.size)
        HEADER.pack_into(buffer, 0, start, WHOLE - start, FIEMAP_FLAG_SYNC, 0, BATCH, 0)
        fcntl.ioctl(fd, FS_IOC_FIEMAP, buffer, True)
        mapped = HEADER.unpack_from(buffer, 0)[3]
        if mapped == 0:
            return
        last = False
        for index in range(mapped):
            fields = EXTENT.unpack_from(buffer, HEADER.size + index * EXTENT.size)
            logical, physical, length, flags = fields[0], fields[1], fields[2], fields[5]
            yield physical, length, bool(flags & FIEMAP_EXTENT_DATA_INLINE)
            start = logical + length
            last = last or bool(flags & FIEMAP_EXTENT_LAST)
        if last or start >= WHOLE:
            return


def measure(source, seen, bound_secs):
    """One source: its new bytes, its files and what could not be read."""
    began = time.monotonic()
    deadline = began + bound_secs
    excluded = {os.path.normpath(os.path.abspath(path)) for path in source.get("exclude") or [] if path}
    counts = {"bytes": 0, "files": 0, "unreadable": 0}

    def unlisted(error):
        if not isinstance(error, FileNotFoundError):
            counts["unreadable"] += 1

    def check_bound():
        if time.monotonic() > deadline:
            raise OverBound()

    try:
        for root in source.get("roots") or []:
            if not root:
                continue
            root = os.path.normpath(os.path.abspath(root))
            if root in excluded:
                continue
            try:
                if stat.S_ISLNK(os.lstat(root).st_mode):
                    continue
            except FileNotFoundError:
                continue
            except OSError:
                counts["unreadable"] += 1
                continue
            for directory, subdirectories, files in os.walk(root, onerror=unlisted):
                check_bound()
                subdirectories[:] = [
                    name for name in subdirectories if os.path.join(directory, name) not in excluded
                ]
                for name in files:
                    check_bound()
                    path = os.path.join(directory, name)
                    if path in excluded:
                        continue
                    count_file(path, seen, counts)
    except OverBound:
        return {
            "name": source["name"],
            "over_bound": True,
            "files": counts["files"],
            "seconds": round(time.monotonic() - began, 3),
        }
    return {
        "name": source["name"],
        "bytes": counts["bytes"],
        "files": counts["files"],
        "unreadable": counts["unreadable"],
        "seconds": round(time.monotonic() - began, 3),
    }


def count_file(path, seen, counts):
    """One file's extents not counted before; a symbolic link or a file
    that is not regular is no file."""
    try:
        if not stat.S_ISREG(os.lstat(path).st_mode):
            return
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return
    except OSError:
        counts["unreadable"] += 1
        return
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            return
        new = 0
        for physical, length, inline in extents(fd):
            if inline or physical == 0:
                new += length
                continue
            key = (physical, length)
            if key not in seen:
                seen.add(key)
                new += length
        counts["bytes"] += new
        counts["files"] += 1
    except OSError:
        counts["unreadable"] += 1
    finally:
        os.close(fd)


def main():
    try:
        config = json.load(sys.stdin)
        bound_secs = float(config["bound_secs"])
        sources = config["sources"]
        if not isinstance(sources, list) or not all(
            isinstance(source, dict) and isinstance(source.get("name"), str) for source in sources
        ):
            raise ValueError("sources")
    except (ValueError, KeyError, TypeError):
        print("storage-extents: the config on stdin is not {bound_secs, sources: [{name, roots, exclude}]}",
              file=sys.stderr)
        return 2
    seen = set()
    answer = [measure(source, seen, bound_secs) for source in sources]
    json.dump({"sources": answer}, sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
