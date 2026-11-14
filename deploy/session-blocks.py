#!/usr/bin/env python3
"""Session backups as blocks (objective 147, stage 1 of design 143).

Run by deploy/session-backup.sh as the backup user, with `python3 -I -B`;
stdlib only, zstd through its CLI. Stage 1 is the dual write: every new
version is also written as blocks beside its plain directory, which stays
the version restore and thinning read. Nothing here removes a version, a
directory or a block.

    session-blocks.py write <account dir> <stamp> <version dir> <manifest> <previous stamp or ""> <hints>
    session-blocks.py publish <account dir> <stamp>
    session-blocks.py check <account dir>
    session-blocks.py emit <account dir> <stamp>

Layout, under <root>/<account>/:
  blocks/<h1h2>/<sha256>.zst  one block: the zstd -3 frame of 1 to 65536
      bytes of a file, named by the sha256 of those bytes. Mode 0400,
      written once: staged as blocks/<h1h2>/.<sha256>.<random>.tmp, fsynced,
      hard-linked to its name (an existing name is the same bytes and is
      never written again), the staged name unlinked, the directory fsynced.
  <stamp>.blocks  the version's index, mode 0400: one JSON line per file,
      sorted by path, {"path", "size", "mtime", "mode", "sha256", "blocks"}
      (the file cut at every 65536-byte offset, the last block short; size
      and mtime as its manifest line says, sha256 the whole file's), then one
      summary line {"version", "files", "bytes", "blocks", "new_blocks",
      "new_stored_bytes"}. write leaves it as <stamp>.blocks.partial,
      fsynced; publish renames it and fsyncs the account directory, and the
      copy calls publish only after the version is committed (its directory
      renamed into place: cc-astra's A63). So a .blocks never names a
      version that is not one; a kill before publish leaves a .blocks.partial,
      which check reports and nothing reads.

write: from the version directory the copy built and its manifest. The
copy's hints say how each file was made: `U path` (taken whole from the
version before), `G <previous size> <previous sha256> path` (the version
before's bytes, then new ones), anything else is read whole. An unchanged file takes the version
before's block list; a grown one keeps the version before's full blocks and
cuts only from the last full block on. A hint that does not hold against
the version before's .blocks (another size or sha256) is read whole.

check: the account's indexes, whole (design 143 section 4):
  1. every <stamp>.blocks parses whole: every line JSON, sorted and unique,
     the last line the summary, its version the stamp, its files, bytes and
     blocks those of the file lines; no <stamp>.blocks.partial is left;
  2. every .blocks agrees with its version's manifest exactly: the same set
     of paths, one line each (a file missing from either side fails, even
     with a summary that counts what is there: cc-astra's A61), and each
     one's size and sha256; every manifest has a .blocks or its directory,
     and every .blocks its manifest and its directory;
  3. every block any index names exists, decompresses within 65536 bytes,
     and hashes to its name;
  4. every distinct (sha256, block list) rebuilds to that sha256. The lists
     are walked sorted, keeping the hash of each common prefix, so a file
     that grew across many versions costs its blocks once, not its size per
     version.
Exit 0 when all hold; 1 with each problem named otherwise; 2 on a usage or
setup error.

emit: the version's files as a tar stream on stdout, rebuilt from its
blocks: each block's bytes are checked against its name before they are
written, and the file's whole sha256 against its index line after. A
mismatch stops the stream (exit 2), naming the version, the file and the
block, so a bad block never reaches the restore as a whole file.
"""

import hashlib
import json
import os
import secrets
import shutil
import stat
import subprocess
import sys
import tarfile

BLOCK = 65536
STAMP_LEN = 16


def die(message, code=2):
    sys.stderr.write("session-blocks: error: %s\n" % message)
    sys.exit(code)


def valid_stamp(stamp):
    return (
        len(stamp) == STAMP_LEN
        and stamp[:8].isdigit()
        and stamp[8] == "T"
        and stamp[9:15].isdigit()
        and stamp[15] == "Z"
        and stamp.isascii()
    )


def zstd():
    path = shutil.which("zstd")
    if not path:
        die("zstd is not installed")
    return path


def fsync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def block_path(account_dir, name):
    return os.path.join(account_dir, "blocks", name[:2], name + ".zst")


def parse_manifest(path):
    """{path: (sha256, size, mtime)}: each line `sha256 size mtime path`,
    split on the first three spaces (the path is the rest)."""
    lines = {}
    with open(path, "rb") as f:
        for raw in f:
            line = raw.decode("utf-8", "surrogateescape").rstrip("\n")
            parts = line.split(" ", 3)
            if len(parts) != 4 or not parts[1].isdigit():
                raise ValueError("a manifest line that is not `sha256 size mtime path`: %r" % line[:120])
            # One line per path (cc-astra's A61 note): a second line of a
            # path is refused, never the later one silently kept.
            if parts[3] in lines:
                raise ValueError("the manifest names %r twice" % parts[3][:120])
            lines[parts[3]] = (parts[0], int(parts[1]), parts[2])
    return lines


def parse_blocks(path, stamp):
    """(file lines, summary) of a whole <stamp>.blocks, or ValueError."""
    with open(path, "rb") as f:
        raw = f.read()
    if not raw.endswith(b"\n"):
        raise ValueError("does not end with a newline (a short read or a cut file)")
    rows = raw.decode("utf-8", "surrogateescape").split("\n")[:-1]
    if not rows:
        raise ValueError("is empty")
    try:
        objects = [json.loads(row) for row in rows]
    except json.JSONDecodeError as error:
        raise ValueError("holds a line that is no JSON: %s" % error) from None
    summary = objects[-1]
    files = objects[:-1]
    want = {"version", "files", "bytes", "blocks", "new_blocks", "new_stored_bytes"}
    if not isinstance(summary, dict) or set(summary) != want:
        raise ValueError("its last line is not the summary")
    if summary["version"] != stamp:
        raise ValueError("its summary names version %r" % summary["version"])
    names = 0
    last = None
    for line in files:
        if not isinstance(line, dict) or set(line) != {"path", "size", "mtime", "mode", "sha256", "blocks"}:
            raise ValueError("a file line without exactly its six fields")
        if not isinstance(line["blocks"], list) or not isinstance(line["size"], int) or line["size"] < 0:
            raise ValueError("a file line with a bad size or block list: %r" % line.get("path"))
        if last is not None and line["path"] <= last:
            raise ValueError("its file lines are not sorted and unique at %r" % line["path"])
        last = line["path"]
        expected = (line["size"] + BLOCK - 1) // BLOCK
        if len(line["blocks"]) != expected:
            raise ValueError("%r lists %d blocks for %d bytes" % (line["path"], len(line["blocks"]), line["size"]))
        for name in line["blocks"]:
            if not (isinstance(name, str) and len(name) == 64 and all(c in "0123456789abcdef" for c in name)):
                raise ValueError("%r names a block that is no sha256" % line["path"])
        names += len(line["blocks"])
    size_total = sum(line["size"] for line in files)
    if summary["files"] != len(files) or summary["blocks"] != names or summary["bytes"] != size_total:
        raise ValueError(
            "its summary says %s files, %s bytes and %s blocks, and it holds %d, %d and %d"
            % (summary["files"], summary["bytes"], summary["blocks"], len(files), size_total, names)
        )
    return files, summary


def chunk_sizes(size, count):
    if count == 0:
        return []
    return [BLOCK] * (count - 1) + [size - BLOCK * (count - 1)]


class Bad(Exception):
    pass


def decode(account_dir, names, sizes):
    """Yield each block's plaintext, in order, each checked against its
    name. One zstd process decodes the frames in turn; exactly the expected
    bytes are read, then one more must not come: a frame that expands
    beyond its block, or a short or missing one, is refused."""
    if not names:
        return
    paths = []
    for name in names:
        path = block_path(account_dir, name)
        try:
            st = os.lstat(path)
        except FileNotFoundError:
            raise Bad("block %s is missing" % name) from None
        if not stat.S_ISREG(st.st_mode):
            raise Bad("block %s is not a regular file" % name)
        paths.append(path)
    try:
        proc = subprocess.Popen(
            [zstd(), "-d", "-q", "-c", "--"] + paths,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL,
        )
    except OSError as error:
        # E2BIG past about 16k blocks (a file over about 1 GB): refused, said.
        raise Bad("the %d blocks %s.. cannot be decoded: %s" % (len(names), names[0][:12], error)) from None
    try:
        last = len(names) - 1
        for index, (name, size) in enumerate(zip(names, sizes)):
            data = b""
            while len(data) < size:
                more = proc.stdout.read(size - len(data))
                if not more:
                    break
                data += more
            if len(data) != size:
                raise Bad("block %s decompresses short or fails (%d of %d bytes)" % (name, len(data), size))
            if hashlib.sha256(data).hexdigest() != name:
                raise Bad("block %s does not hash to its name" % name)
            # The one more byte is read before the last block is given, so
            # a reader that stops at the file's size (emit's tarfile) is
            # refused too, not only one that runs the generator out (check).
            if index == last:
                if proc.stdout.read(1):
                    raise Bad("the blocks %s.. decompress beyond their %d bytes" % (names[0][:12], sum(sizes)))
                err = proc.stderr.read()
                if proc.wait() != 0:
                    raise Bad(
                        "zstd failed on the blocks %s.. (status %d): %s"
                        % (names[0][:12], proc.returncode, err.decode("utf-8", "replace").strip()[:200])
                    )
            yield data
    finally:
        proc.stdout.close()
        proc.kill()
        proc.wait()
        proc.stderr.close()


def put_block(account_dir, data):
    """The block of these bytes, written once. Returns (name, stored bytes
    or 0 when it was there)."""
    name = hashlib.sha256(data).hexdigest()
    final = block_path(account_dir, name)
    if os.path.lexists(final):
        return name, 0
    directory = os.path.dirname(final)
    os.makedirs(directory, mode=0o700, exist_ok=True)
    frame = subprocess.run([zstd(), "-3", "-q", "-c"], input=data, capture_output=True, check=True).stdout
    staged = os.path.join(directory, ".%s.%s.tmp" % (name, secrets.token_hex(8)))
    fd = os.open(staged, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        written = 0
        while written < len(frame):
            written += os.write(fd, frame[written:])
        os.fchmod(fd, 0o400)
        os.fsync(fd)
    finally:
        os.close(fd)
    try:
        os.link(staged, final)
        stored = len(frame)
    except FileExistsError:
        stored = 0
    os.unlink(staged)
    fsync_dir(directory)
    return name, stored


def cut(account_dir, path, offset, counts):
    """The blocks of a file from `offset` (a block boundary) on."""
    names = []
    with open(path, "rb") as f:
        f.seek(offset)
        while True:
            data = f.read(BLOCK)
            if not data:
                break
            counts["read_bytes"] += len(data)
            name, stored = put_block(account_dir, data)
            names.append(name)
            if stored:
                counts["new_blocks"] += 1
                counts["new_stored_bytes"] += stored
    return names


def write(account_dir, stamp, version_dir, manifest_path, previous, hints_path):
    if not valid_stamp(stamp) or (previous and not valid_stamp(previous)):
        die("write: bad stamp")
    manifest = parse_manifest(manifest_path)
    before = {}
    if previous and os.path.exists(os.path.join(account_dir, previous + ".blocks")):
        try:
            files, _ = parse_blocks(os.path.join(account_dir, previous + ".blocks"), previous)
            before = {line["path"]: line for line in files}
        except ValueError:
            before = {}
    hints = {}
    with open(hints_path, "rb") as f:
        for raw in f:
            parts = raw.decode("utf-8", "surrogateescape").rstrip("\n").split("\t")
            if parts[0] == "U" and len(parts) == 2:
                hints[parts[1]] = ("U", None)
            elif parts[0] == "G" and len(parts) == 4 and parts[1].isdigit():
                hints[parts[3]] = ("G", (int(parts[1]), parts[2]))
    counts = {"new_blocks": 0, "new_stored_bytes": 0, "read_bytes": 0}
    lines = []
    total = 0
    for path in sorted(manifest):
        sha, size, mtime = manifest[path]
        full = os.path.join(version_dir, path)
        st = os.lstat(full)
        if not stat.S_ISREG(st.st_mode) or st.st_size != size:
            die("write: %s in %s is not the manifest's %d-byte file" % (path, stamp, size))
        hint, psize = hints.get(path, (None, None))  # psize: G's (size, sha256)
        old = before.get(path)
        if hint == "U" and old and old["size"] == size and old["sha256"] == sha:
            blocks = list(old["blocks"])
        elif hint == "G" and old and (old["size"], old["sha256"]) == psize and psize[0] <= size:
            keep = psize[0] // BLOCK
            blocks = list(old["blocks"][:keep]) + cut(account_dir, full, keep * BLOCK, counts)
        else:
            blocks = cut(account_dir, full, 0, counts)
        lines.append(
            {"path": path, "size": size, "mtime": mtime, "mode": stat.S_IMODE(st.st_mode), "sha256": sha, "blocks": blocks}
        )
        total += size
    summary = {
        "version": stamp,
        "files": len(lines),
        "bytes": total,
        "blocks": sum(len(line["blocks"]) for line in lines),
        "new_blocks": counts["new_blocks"],
        "new_stored_bytes": counts["new_stored_bytes"],
    }
    partial = os.path.join(account_dir, stamp + ".blocks.partial")
    body = "".join(json.dumps(line, sort_keys=True, ensure_ascii=False) + "\n" for line in lines)
    body += json.dumps(summary, sort_keys=True) + "\n"
    fd = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        data = body.encode("utf-8", "surrogateescape")
        written = 0
        while written < len(data):
            written += os.write(fd, data[written:])
        os.fchmod(fd, 0o400)
        os.fsync(fd)
    finally:
        os.close(fd)
    print(
        "blocks %s: %d files, %d blocks, %d new (%d bytes stored), %d bytes read"
        % (stamp, summary["files"], summary["blocks"], summary["new_blocks"], summary["new_stored_bytes"], counts["read_bytes"])
    )


def publish(account_dir, stamp):
    """<stamp>.blocks.partial renamed to <stamp>.blocks, once its version is
    committed (its directory and manifest in place), and the account
    directory fsynced."""
    if not valid_stamp(stamp):
        die("publish: bad stamp")
    if not (os.path.isdir(os.path.join(account_dir, stamp)) and os.path.isfile(os.path.join(account_dir, stamp + ".manifest"))):
        die("publish: version %s is not committed (no directory or manifest)" % stamp)
    final = os.path.join(account_dir, stamp + ".blocks")
    if os.path.lexists(final):
        die("publish: %s.blocks exists already" % stamp)
    os.rename(os.path.join(account_dir, stamp + ".blocks.partial"), final)
    fsync_dir(account_dir)


def stamps(account_dir, suffix):
    out = set()
    for entry in os.listdir(account_dir):
        if entry.endswith(suffix) and valid_stamp(entry[: -len(suffix)] if suffix else entry):
            out.add(entry[: -len(suffix)] if suffix else entry)
    return out


def check(account_dir):
    problems = []
    if not os.path.isdir(account_dir):
        die("check: no account directory %s" % account_dir)
    entries = set(os.listdir(account_dir))
    indexed = stamps(account_dir, ".blocks")
    manifests = stamps(account_dir, ".manifest")
    directories = {e for e in entries if valid_stamp(e) and os.path.isdir(os.path.join(account_dir, e))}
    for entry in sorted(entries):
        if entry.endswith(".blocks.partial"):
            problems.append("%s is left (a copy died while writing it)" % entry)
    lists = {}
    for stamp in sorted(indexed):
        try:
            files, _ = parse_blocks(os.path.join(account_dir, stamp + ".blocks"), stamp)
        except (ValueError, OSError) as error:
            problems.append("%s.blocks %s" % (stamp, error))
            continue
        if stamp not in manifests or stamp not in directories:
            problems.append("%s.blocks names no committed version (no manifest or no directory)" % stamp)
            continue
        try:
            manifest = parse_manifest(os.path.join(account_dir, stamp + ".manifest"))
        except (ValueError, OSError) as error:
            problems.append("%s.manifest %s" % (stamp, error))
            continue
        have = {line["path"]: (line["sha256"], line["size"]) for line in files}
        want = {path: (sha, size) for path, (sha, size, _) in manifest.items()}
        if have != want:
            diff = sorted(set(have.items()) ^ set(want.items()))[:3]
            problems.append("%s.blocks disagrees with its manifest: %s" % (stamp, ", ".join(p for p, _ in diff)))
            continue
        for line in files:
            lists.setdefault((line["sha256"], line["size"], tuple(line["blocks"])), "%s %s" % (stamp, line["path"]))
    for stamp in sorted(manifests - indexed - directories):
        problems.append("%s.manifest has neither a .blocks nor a directory: its content is nowhere" % stamp)
    # Items 3 and 4: every distinct list rebuilt, sorted, the hash of each
    # common prefix kept.
    named = {name for (_, _, names) in lists for name in names}
    states = [hashlib.sha256()]
    walked = ()
    walked_full = 0
    blocks_read = 0
    bytes_read = 0
    for (sha, size, names), where in sorted(lists.items(), key=lambda item: item[0][2]):
        common = 0
        # Only states after full blocks of the walked list are reused.
        limit = min(len(names), walked_full, len(states) - 1)
        while common < limit and names[common] == walked[common]:
            common += 1
        # A prefix state is reusable only after full blocks.
        while common > 0 and common == len(names) and size != BLOCK * len(names):
            common -= 1
        del states[common + 1 :]
        sizes = chunk_sizes(size, len(names))
        if size and not 0 < sizes[-1] <= BLOCK:
            problems.append("%s: %d blocks cannot hold %d bytes" % (where, len(names), size))
            walked, walked_full = (), 0
            states = [hashlib.sha256()]
            continue
        try:
            for data in decode(account_dir, list(names[common:]), sizes[common:]):
                state = states[-1].copy()
                state.update(data)
                states.append(state)
                blocks_read += 1
                bytes_read += len(data)
        except Bad as error:
            problems.append("%s: %s" % (where, error))
            walked, walked_full = (), 0
            states = [hashlib.sha256()]
            continue
        walked = names
        walked_full = len(names) if size == BLOCK * len(names) else len(names) - 1
        if states[len(names)].hexdigest() != sha:
            problems.append("%s: its blocks do not rebuild its sha256" % where)
    for problem in problems[:40]:
        print("CHECK FAILED %s: %s" % (os.path.basename(account_dir), problem))
    if len(problems) > 40:
        print("CHECK FAILED %s: and %d more" % (os.path.basename(account_dir), len(problems) - 40))
    if problems:
        sys.exit(1)
    print(
        "check %s: %d indexes, %d distinct files, %d blocks named, %d decoded (%d bytes); every line agrees with its manifest"
        % (os.path.basename(account_dir), len(indexed), len(lists), len(named), blocks_read, bytes_read)
    )


class Reader:
    """A file's verified bytes, for tarfile (which only calls read(n))."""

    def __init__(self, chunks):
        self.chunks = chunks
        self.rest = b""

    def read(self, n=-1):
        while (n < 0 or len(self.rest) < n) and self.chunks is not None:
            try:
                self.rest += next(self.chunks)
            except StopIteration:
                self.chunks = None
        if n < 0:
            n = len(self.rest)
        out, self.rest = self.rest[:n], self.rest[n:]
        return out


def emit(account_dir, stamp):
    if not valid_stamp(stamp):
        die("emit: bad stamp")
    index = os.path.join(account_dir, stamp + ".blocks")
    if not os.path.exists(index):
        die("no blocks of version %s" % stamp)
    try:
        files, _ = parse_blocks(index, stamp)
        manifest = parse_manifest(os.path.join(account_dir, stamp + ".manifest"))
    except (ValueError, OSError) as error:
        die("version %s: %s" % (stamp, error))
    if {line["path"]: (line["sha256"], line["size"]) for line in files} != {
        p: (s, n) for p, (s, n, _) in manifest.items()
    }:
        die("version %s: its .blocks disagrees with its manifest" % stamp)
    out = tarfile.open(fileobj=sys.stdout.buffer, mode="w|", format=tarfile.PAX_FORMAT)
    made = set()
    for line in files:
        parts = line["path"].split("/")
        for depth in range(1, len(parts)):
            directory = "/".join(parts[:depth])
            if directory not in made:
                made.add(directory)
                info = tarfile.TarInfo(directory)
                info.type = tarfile.DIRTYPE
                info.mode = 0o700
                out.addfile(info)
        info = tarfile.TarInfo(line["path"])
        info.size = line["size"]
        info.mode = line["mode"] & 0o777
        try:
            info.mtime = float(line["mtime"])
        except ValueError:
            info.mtime = 0
        whole = hashlib.sha256()

        def chunks(line=line, whole=whole):
            for data in decode(account_dir, line["blocks"], chunk_sizes(line["size"], len(line["blocks"]))):
                whole.update(data)
                yield data

        try:
            out.addfile(info, Reader(chunks()))
        except Bad as error:
            die("version %s, %s: %s" % (stamp, line["path"], error))
        if whole.hexdigest() != line["sha256"]:
            die("version %s, %s: its blocks do not rebuild its sha256" % (stamp, line["path"]))
    out.close()
    sys.stdout.buffer.flush()


def main(argv):
    if len(argv) < 2:
        die("usage: session-blocks.py write|check|emit ...")
    command, args = argv[1], argv[2:]
    os.umask(0o077)
    if command == "write" and len(args) == 6:
        write(*args)
    elif command == "publish" and len(args) == 2:
        publish(*args)
    elif command == "check" and len(args) == 1:
        check(args[0])
    elif command == "emit" and len(args) == 2:
        emit(*args)
    else:
        die("usage: session-blocks.py write <account dir> <stamp> <version dir> <manifest> <previous> <hints> | publish <account dir> <stamp> | check <account dir> | emit <account dir> <stamp>")


if __name__ == "__main__":
    main(sys.argv)
