#!/usr/bin/env python3
"""Real bytes on disk per source, each physical extent counted once
(objective 180; the rule of the lead's storage study, exp3.py).

Read only: run as `python3 -I -B storage-extents.py`, it reads its
configuration as JSON on stdin,

    {"bound_secs": <n>,
     "sources": [{"name": <name>, "roots": [<dir>], "exclude": [<dir>]}]}

walks each source's roots (symbolic links never followed, an excluded
directory never entered), asks the file system for each regular file's
extents (FIEMAP, through fcntl.ioctl; the file is opened read-only and never
read), and prints one JSON object on stdout:

    {"sources": [{"name", "bytes", "files", "unreadable", "seconds"}
                 | {"name", "over_bound": true, "files", "seconds"}]}

An extent is counted once across the whole reading, in the first source
that holds it: a reflinked copy (cp --reflink) adds nothing, a plain copy
adds its bytes. Data kept inline in metadata counts as its length. A file
that cannot be opened or mapped counts in `unreadable`, and so does a
directory that cannot be listed (one gone mid-walk, or a root that does not
exist, counts nowhere). A source not done
within `bound_secs` of its start reads `over_bound`, never a partial count.
No file name, path or content is ever printed.
"""

import fcntl
import json
import os
import struct
import sys
import time

FS_IOC_FIEMAP = 0xC020660B
# Flush the file's dirty data first, so a file just written is mapped to
# its real extents, not delayed allocation.
FIEMAP_FLAG_SYNC = 0x1
EXTENTS = 512
LAST = 0x1
INLINE = 0x100
HEADER = struct.Struct("=QQLLLL")
EXTENT = struct.Struct("=QQQQQLLLL")


def extents(fd):
    """The (physical, length, flags) of each of the file's extents."""
    out = []
    start = 0
    if os.fstat(fd).st_size == 0:
        return out
    while True:
        buf = bytearray(HEADER.pack(start, 0xFFFFFFFFFFFFFFFF, FIEMAP_FLAG_SYNC, 0, EXTENTS, 0)
                        + bytes(EXTENT.size * EXTENTS))
        fcntl.ioctl(fd, FS_IOC_FIEMAP, buf)
        mapped = HEADER.unpack_from(buf, 0)[3]
        if mapped == 0:
            return out
        last = False
        for i in range(mapped):
            logical, physical, length, _, _, flags, _, _, _ = EXTENT.unpack_from(
                buf, HEADER.size + EXTENT.size * i)
            out.append((physical, length, flags))
            start = logical + length
            last = last or bool(flags & LAST)
        if last or mapped < EXTENTS:
            return out


def measure(source, seen, bound_secs):
    """One source's reading, its extents added to `seen`."""
    began = time.monotonic()
    excluded = {os.path.abspath(path) for path in source.get("exclude", [])}
    total = files = 0
    unlisted = []

    def unlistable(error):
        # A directory that cannot be listed is not measured: said in
        # `unreadable`, never skipped silently (rev-g's n1).
        if not isinstance(error, FileNotFoundError):
            unlisted.append(1)

    unreadable = 0
    for root in source.get("roots", []):
        for directory, subdirectories, names in os.walk(root, onerror=unlistable):
            subdirectories[:] = [name for name in subdirectories
                                 if os.path.join(directory, name) not in excluded]
            for name in names:
                if time.monotonic() - began > bound_secs:
                    return {"name": source["name"], "over_bound": True, "files": files,
                            "seconds": round(time.monotonic() - began, 1)}
                path = os.path.join(directory, name)
                try:
                    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
                except OSError:
                    unreadable += 1
                    continue
                try:
                    if not os.path.isfile("/proc/self/fd/%d" % fd):
                        continue
                    found = extents(fd)
                except OSError:
                    unreadable += 1
                    continue
                finally:
                    os.close(fd)
                files += 1
                for physical, length, flags in found:
                    if flags & INLINE or physical == 0:
                        total += length
                    elif (physical, length) not in seen:
                        seen.add((physical, length))
                        total += length
    return {"name": source["name"], "bytes": total, "files": files,
            "unreadable": unreadable + len(unlisted),
            "seconds": round(time.monotonic() - began, 1)}


def main():
    config = json.load(sys.stdin)
    bound = float(config["bound_secs"])
    seen = set()
    readings = [measure(source, seen, bound) for source in config["sources"]]
    json.dump({"sources": readings}, sys.stdout)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
