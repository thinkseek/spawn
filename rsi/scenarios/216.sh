#!/bin/sh
# Objective 216 [recoverability], the fleet's notes folder in the bundle:
# `spawn backup bundle`, with SPAWN_BACKUP_SYNC_DIR set, takes that folder
# into the same encrypted bundle: every file in the weekly full, the files
# new or changed since the base bundle in an increment, and a manifest of the
# whole folder (path, size, mtime, mode, sha256) inside the bundle. The
# record carries the manifest's sha256 and counts only. The files it takes
# are scanned for credential values first (SPAWN_BACKUP_SYNC_SCAN, by default
# /usr/local/lib/spawn/redact/secret-scan-files.py): a hit, a place the scan
# could not search, or a scan that fails drops the folder's part, and the
# rest of the bundle is stored as before. The folder is never written.
#
# Black box, on its own database, scratch HOME and GNUPGHOME, against 104's
# local R2 stand-in, with a scratch notes folder and a stand-in scan that
# answers as the real one does (its exit codes and output lines): a file
# holding the scenario's made-up value is a hit, a file named *.blind is a
# place not seen into (a damaged archive), a file with NUL bytes is a place
# not seen into whose bytes were searched raw.
#   (1) Full: the record's sync part is kind full, counts every regular file
#       (the symbolic link skipped), names the binary file as not seen into,
#       and its manifest_sha256 is the sha256 of the bundle's
#       sync/manifest.json; that manifest names each live file's size, mode
#       and sha256, and sync/files holds each file's bytes. The bundle
#       stages on the workspace's disk, never in TMPDIR (a /tmp that may be
#       memory): the tar it runs (a logging stand-in first in PATH) packs a
#       folder in the workspace root.
#   (2) Increment, after a file changed, one was added, one removed and one
#       given a new mtime with the same bytes: kind incremental, its base
#       used, 3 taken and 1 removed; sync/files holds just those 3, the
#       manifest names every file now and the removed one.
#   (2b) The local copy of the base manifest edited (other bytes than the
#       stored bundle's): not trusted; the next increment says its base is
#       unusable and takes every file.
#   (2c) A file given other bytes at the same size and mtime (rev-k's F1 on
#       R1): the next increment uses its base and takes that file alone.
#   (3) A file holding a credential value: the sync part is refused, the
#       bundle holds no sync/ and is stored with its transcripts.
#   (4) A place the scan could not search: refused the same way.
#   (5) The scan tool missing: refused the same way.
#   (5b) A file the run cannot read (mode 000; not checkable as root):
#       refused the same way, the refusal naming the file.
#   (6) SPAWN_BACKUP_SYNC_DIR unset: no sync part in the record or bundle.
#   (7) The bundle unit (deploy/systemd/spawn-backup-bundle.service, run as
#       systemd runs it, its notes folder pointed at the scratch one) takes
#       the notes folder /var/tmp/spawn/sync.
#   (8) Every run leaves the notes folder as it was (path, size, mode,
#       mtime, sha256), no scratch is left in the workspace (one a killed
#       run left before (1) is gone too), and no secret (the passphrase, the R2 secret key, the
#       made-up credential value) is in any output or record.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "216: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql pg_dump pg_restore gpg git jq od sha256sum python3 tar gzip awk sed cmp; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and the tmux server's socket, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_216_sync_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
db_password=${PGPASSWORD-}

database_made=""
r2_pid=""
serve_pid=""
R2=$WORK/r2
GNUPGHOME=$WORK/gnupg
export GNUPGHOME
cleanup() {
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    if [ -n "$r2_pid" ]; then
        kill "$r2_pid" 2>/dev/null || true
        wait "$r2_pid" 2>/dev/null || true
    fi
    if command -v gpgconf >/dev/null 2>&1 && [ -d "$GNUPGHOME" ]; then
        gpgconf --homedir "$GNUPGHOME" --kill gpg-agent >/dev/null 2>&1 || true
        # gpg puts a scratch home's agent sockets in /run/user/<uid>/gnupg/d.*
        # whatever XDG_RUNTIME_DIR says (S20): that directory goes too.
        gpgconf --homedir "$GNUPGHOME" --remove-socketdir >/dev/null 2>&1 || true
    fi
    if [ -n "$database_made" ]; then
        # A scratch database a drill named and left behind goes too, but only
        # a name of this run's (it contains this run's id): candidate code may
        # name any database in a backup.verified record, and a database a
        # parallel gate is using is not this run's to drop.
        for left in $(psql -d "$SPAWN_DB_NAME" -tA -c "select distinct e.kind -> 'detail' -> 'record' ->> 'database'
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' = 'backup.verified'" 2>/dev/null); do
            case "$left" in
            *"$run"*) sh "$controller/scripts/blackbox/scenario-db.sh" drop "$left" >/dev/null 2>&1 || true ;;
            esac
        done
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$R2/requests.log" ]; then
            echo "216: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "216: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "216: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "216: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this scenario's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# scenario read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}
# --- the world ----------------------------------------------------------------

# Nothing of the owner's: no R2 or backup setting from outside, HOME and
# git's config in the scratch.
for name in SPAWN_R2_ENDPOINT SPAWN_R2_BUCKET SPAWN_R2_ACCESS_KEY_ID SPAWN_R2_SECRET_ACCESS_KEY \
    SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_FILE SPAWN_BACKUP_DRILL_DATABASE_URL SPAWN_BACKUP_R2_BUDGET_MB \
    SPAWN_BACKUP_R2_ALERT_MB SPAWN_BACKUP_R2_LIMIT_MB SPAWN_BACKUP_TRANSCRIPT_DAYS \
    SPAWN_BACKUP_PG_BIN SPAWN_BACKUP_DUMP_COMPRESS SPAWN_ENV_FILE SPAWN_BACKUP_DIR XDG_CONFIG_HOME \
    SPAWN_WORKSPACE_ROOT SPAWN_GIT_REPO SPAWN_BACKUP_SYNC_DIR SPAWN_BACKUP_SYNC_SCAN; do
    unset "$name" 2>/dev/null || true
done
HOME=$WORK/home
mkdir -p "$HOME/.config/spawn"
export HOME
cat >"$WORK/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL
mkdir -m 700 "$GNUPGHOME"

umask 077
od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$WORK/backup.key"
passphrase=$(cat "$WORK/backup.key")
r2_secret=$(od -An -N20 -tx1 /dev/urandom | tr -d ' \n')
printf '%s' "$r2_secret" >"$WORK/r2.secret"
access_key="bb216$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
bucket=spawn-bb
umask 022

# The stand-in for R2 (its header says what it answers).
mkdir -p "$R2"
cat >"$WORK/r2.py" <<'PY'
"""A local stand-in for Cloudflare R2's S3 API, for one scenario.

argv: <directory> <bucket> <access key id> <secret file>
Writes <directory>/port once it listens on 127.0.0.1. Objects live in
<directory>/objects/<hex of the key> with a .json beside each (ETag, size,
last modified, x-amz-meta-* headers). Every request is one line of
<directory>/requests.log: method, key or ?query, status, why.

Every request must carry a valid AWS Signature Version 4 (region auto or
us-east-1, service s3) for the access key: anything else is 403
SignatureDoesNotMatch, as R2 answers. PUT stores (ETag = MD5 of the body);
an existing object under a locked prefix is not overwritten (409
ObjectLockedByBucketPolicy), and DELETE is always 409, as the bucket's lock
rules answer the host's key. GET and HEAD read an object; GET on the bucket
with list-type=2 lists, at most 4 keys a page (S3 may return fewer than
max-keys), with continuation tokens.
"""
import datetime, hashlib, hmac, json, os, sys, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root, bucket, access_key = sys.argv[1], sys.argv[2], sys.argv[3]
secret = open(sys.argv[4]).read().strip()
objects = os.path.join(root, "objects")
os.makedirs(objects, exist_ok=True)
LOCKED = ("postgres/", "bundles/")
PAGE = 4
UNRESERVED = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~"


def enc(text, slash):
    safe = UNRESERVED + ("/" if slash else "")
    return "".join(c if c in safe else "".join("%%%02X" % b for b in c.encode()) for c in text)


def path_of(key):
    return os.path.join(objects, key.encode().hex())


def log(handler, key, status, why=""):
    with open(os.path.join(root, "requests.log"), "a") as out:
        out.write("%s %s %d %s\n" % (handler.command, key, status, why))


def hm(key, msg):
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def reply(self, status, body=b"", headers=(), key="-", why=""):
        log(self, key, status, why)
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def error(self, status, code, key, why=""):
        body = ("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>%s</Code><Message>%s</Message></Error>"
                % (code, why or code)).encode()
        self.reply(status, body, [("Content-Type", "application/xml")], key, code + (": " + why if why else ""))

    def body(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip(), 16)
                if size == 0:
                    self.rfile.readline()
                    return data
                data += self.rfile.read(size)
                self.rfile.readline()
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def signed(self, payload):
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("AWS4-HMAC-SHA256 "):
            return "no AWS4-HMAC-SHA256 Authorization"
        fields = {}
        for part in auth[len("AWS4-HMAC-SHA256 "):].split(","):
            name, _, value = part.strip().partition("=")
            fields[name] = value
        credential = fields.get("Credential", "").split("/")
        if len(credential) != 5 or credential[0] != access_key or credential[3] != "s3" \
                or credential[2] not in ("auto", "us-east-1") or credential[4] != "aws4_request":
            return "credential scope %s" % "/".join(credential[1:])
        amzdate = self.headers.get("x-amz-date", "")
        try:
            when = datetime.datetime.strptime(amzdate, "%Y%m%dT%H%M%SZ").replace(tzinfo=datetime.timezone.utc)
        except ValueError:
            return "no x-amz-date"
        if abs((datetime.datetime.now(datetime.timezone.utc) - when).total_seconds()) > 900 \
                or not amzdate.startswith(credential[1]):
            return "x-amz-date out of range"
        signed_headers = fields.get("SignedHeaders", "")
        names = signed_headers.split(";")
        if "host" not in names:
            return "host is not signed"
        canonical_headers = ""
        for name in names:
            value = self.headers.get(name)
            if value is None:
                return "signed header %s missing" % name
            canonical_headers += "%s:%s\n" % (name, " ".join(value.strip().split()))
        hashed = self.headers.get("x-amz-content-sha256")
        if hashed is None:
            hashed = hashlib.sha256(payload).hexdigest()
        elif hashed != "UNSIGNED-PAYLOAD" and hashed != hashlib.sha256(payload).hexdigest():
            return "x-amz-content-sha256 is not the body's"
        raw_path, _, raw_query = self.path.partition("?")
        uri = enc(urllib.parse.unquote(raw_path), True)
        pairs = sorted((enc(k, False), enc(v, False))
                       for k, v in urllib.parse.parse_qsl(raw_query, keep_blank_values=True))
        query = "&".join("%s=%s" % pair for pair in pairs)
        request = "\n".join([self.command, uri, query, canonical_headers, signed_headers, hashed])
        scope = "/".join(credential[1:])
        to_sign = "\n".join(["AWS4-HMAC-SHA256", amzdate, scope,
                             hashlib.sha256(request.encode()).hexdigest()])
        key = hm(("AWS4" + secret).encode(), credential[1])
        for part in credential[2:]:
            key = hm(key, part)
        signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
        if not hmac.compare_digest(signature, fields.get("Signature", "")):
            return "signature"
        return None

    def target(self):
        raw_path, _, raw_query = self.path.partition("?")
        path = urllib.parse.unquote(raw_path)
        query = dict(urllib.parse.parse_qsl(raw_query, keep_blank_values=True))
        prefix = "/" + bucket
        if path == prefix or path == prefix + "/":
            return "", query
        if path.startswith(prefix + "/"):
            return path[len(prefix) + 1:], query
        return None, query

    def handle_any(self):
        payload = self.body() if self.command == "PUT" else b""
        key, query = self.target()
        shown = key if key else "?" + urllib.parse.urlencode(sorted(query.items()))
        why = self.signed(payload)
        if why:
            return self.error(403, "SignatureDoesNotMatch", shown, why)
        if key is None:
            return self.error(404, "NoSuchBucket", shown)
        if key == "":
            if self.command == "GET" and query.get("list-type") == "2":
                return self.listing(query, shown)
            return self.error(400, "InvalidRequest", shown)
        file = path_of(key)
        if self.command == "PUT":
            if os.path.exists(file) and key.startswith(LOCKED):
                return self.error(409, "ObjectLockedByBucketPolicy", key)
            meta = {name.lower(): value for name, value in self.headers.items()
                    if name.lower().startswith("x-amz-meta-")}
            etag = hashlib.md5(payload).hexdigest()
            with open(file + ".part", "wb") as out:
                out.write(payload)
            os.replace(file + ".part", file)
            info = {"key": key, "etag": etag, "size": len(payload), "meta": meta,
                    "modified": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"}
            with open(file + ".json.part", "w") as out:
                json.dump(info, out)
            os.replace(file + ".json.part", file + ".json")
            return self.reply(200, b"", [("ETag", '"%s"' % etag)], key)
        if self.command == "DELETE":
            return self.error(409, "ObjectLockedByBucketPolicy", key, "delete")
        if self.command in ("GET", "HEAD"):
            if not os.path.exists(file + ".json"):
                return self.error(404, "NoSuchKey", key)
            info = json.load(open(file + ".json"))
            data = open(file, "rb").read()
            headers = [("ETag", '"%s"' % info["etag"]), ("Content-Type", "application/octet-stream"),
                       ("Last-Modified", datetime.datetime.strptime(info["modified"], "%Y-%m-%dT%H:%M:%S.%fZ")
                        .strftime("%a, %d %b %Y %H:%M:%S GMT"))]
            headers += sorted(info["meta"].items())
            return self.reply(200, data, headers, key)
        return self.error(405, "MethodNotAllowed", shown)

    def listing(self, query, shown):
        prefix = query.get("prefix", "")
        infos = []
        for name in os.listdir(objects):
            if name.endswith(".json"):
                info = json.load(open(os.path.join(objects, name)))
                if info["key"].startswith(prefix):
                    infos.append(info)
        infos.sort(key=lambda info: info["key"])
        after = query.get("continuation-token") or ""
        after = bytes.fromhex(after).decode() if after else query.get("start-after", "")
        infos = [info for info in infos if info["key"] > after]
        try:
            limit = max(1, min(int(query.get("max-keys", "1000")), 1000, PAGE))
        except ValueError:
            return self.error(400, "InvalidArgument", shown, "max-keys")
        page, more = infos[:limit], len(infos) > limit
        parts = ['<?xml version="1.0" encoding="UTF-8"?>',
                 '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">',
                 "<Name>%s</Name><Prefix>%s</Prefix><KeyCount>%d</KeyCount><MaxKeys>%d</MaxKeys><IsTruncated>%s</IsTruncated>"
                 % (bucket, prefix, len(page), limit, "true" if more else "false")]
        if more:
            parts.append("<NextContinuationToken>%s</NextContinuationToken>" % page[-1]["key"].encode().hex())
        for info in page:
            parts.append("<Contents><Key>%s</Key><LastModified>%s</LastModified><ETag>&quot;%s&quot;</ETag>"
                         "<Size>%d</Size><StorageClass>STANDARD</StorageClass></Contents>"
                         % (info["key"], info["modified"], info["etag"], info["size"]))
        parts.append("</ListBucketResult>")
        self.reply(200, "".join(parts).encode(), [("Content-Type", "application/xml")], shown)

    do_GET = do_PUT = do_HEAD = do_DELETE = do_POST = handle_any


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(os.path.join(root, "port.part"), "w") as out:
    out.write(str(server.server_address[1]))
os.replace(os.path.join(root, "port.part"), os.path.join(root, "port"))
server.serve_forever()
PY
: >"$R2/requests.log"
python3 -I "$WORK/r2.py" "$R2" "$bucket" "$access_key" "$WORK/r2.secret" 2>"$R2/err" &
r2_pid=$!
tries=0
while [ ! -s "$R2/port" ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 100 ] || harness "the R2 stand-in did not start within 10 s: $(cat "$R2/err")"
    kill -0 "$r2_pid" 2>/dev/null || harness "the R2 stand-in exited: $(cat "$R2/err")"
    sleep 0.1
done
r2_endpoint="http://127.0.0.1:$(cat "$R2/port")"
umask 077
cat >"$WORK/r2.env" <<EOF
SPAWN_R2_ENDPOINT=$r2_endpoint
SPAWN_R2_BUCKET=$bucket
SPAWN_R2_ACCESS_KEY_ID=$access_key
SPAWN_R2_SECRET_ACCESS_KEY=$r2_secret
EOF
umask 022

# One mission source and one transcript: the rest of the bundle, which a
# refused notes folder must not take with it.
src=$WORK/spawn.git
git init -q --initial-branch=main "$WORK/seed"
printf 'spawn\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$src"
git -C "$WORK/seed" push -q "$src" main:main
live_dir=$HOME/.local/share/spawn-rsi
SPAWN_WORKSPACE_ROOT=$live_dir/ws
archive=$SPAWN_WORKSPACE_ROOT/.transcripts
transcript=agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0216.jsonl
mkdir -p "$archive/agent_supervisor"
awk 'BEGIN { for (i = 1; i <= 200; i++)
    printf "{\"type\":\"assistant\",\"uuid\":\"s-%d\",\"message\":{\"role\":\"assistant\",\"content\":\"step %d\"}}\n", i, i }' \
    >"$archive/$transcript"

# The notes folder, as the fleet keeps it: text, an empty mark, a file only
# its owner reads, a binary file, a symbolic link. 700, as the live one is.
SYNC=$WORK/notes
mkdir -m 700 "$SYNC"
mkdir -p "$SYNC/inbox" "$SYNC/obj-7" "$SYNC/ready"
printf '## lead 10:00 - a queue line\n' >"$SYNC/queue.md"
printf -- '- [ ] read 7\n' >"$SYNC/inbox/rev-x.md"
printf 'diff --git a/x b/x\n+a line\n' >"$SYNC/obj-7/diff.patch"
printf '0123abcd  diff.patch\n' >"$SYNC/obj-7/sha256.txt"
: >"$SYNC/ready/7"
printf 'ENDED 4eab0e17 at 17:50:27\n' >"$SYNC/owner-gate.state"
chmod 600 "$SYNC/owner-gate.state"
printf 'PNG\000\001\002\003 an image\n' >"$SYNC/obj-7/shot.png"
ln -s obj-7 "$SYNC/latest"

# A made-up credential value: the stand-in scan's list. Never in an output.
made_value="bb216tok$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
SCAN=$WORK/scan.py
cat >"$SCAN" <<PY
"""A stand-in for secret-scan-files.py: its exit codes and output lines.
argv: <directory>. A file holding the made-up value is "value in <path>"
(exit 1); one named *.blind is not seen into, a damaged archive; one with
NUL bytes is not seen into, binary (its bytes searched raw) (exit 3 when
nothing was found); else exit 0. Never prints a value."""
import os, sys
VALUE = b"$made_value"
root = sys.argv[1]
with open("$WORK/scan.calls", "a") as calls:
    calls.write(root + "\n")
hits, unseen, files = [], [], 0
for top, dirs, names in os.walk(root):
    dirs.sort()
    for name in sorted(names):
        path = os.path.join(top, name)
        if os.path.islink(path) or not os.path.isfile(path):
            continue
        files += 1
        data = open(path, "rb").read()
        if VALUE in data:
            hits.append(path)
        elif name.endswith(".blind"):
            unseen.append(("damaged zstd (ZstdError)", path))
        elif b"\0" in data:
            unseen.append(("binary (NUL bytes)", path))
print("files: %d; not seen into: %d" % (files, len(unseen)))
for path in hits:
    print("value in " + path)
for why, path in unseen:
    print("not seen into (%s): %s" % (why, path))
sys.exit(1 if hits else 3 if unseen else 0)
PY

SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_R2_ENV_FILE=$WORK/r2.env
SPAWN_BACKUP_KEY_FILE=$WORK/backup.key
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_FILE

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done
capture "$SPAWN_BIN" mission add --source "$src" \
    --text "216 mission: keep the fleet's notes off this host, and prove they restore"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"

# --- helpers ----------------------------------------------------------------

q() { scalar "$1"; }
records() {
    sql "select e.id || '|' || (e.kind -> 'detail' -> 'record')::text from spawn_events e
          where e.session_id = 'session_control' and e.event_name = 'control.recorded'
            and e.kind -> 'detail' -> 'record' ->> 'type' = '$1' order by e.id"
}
newest() { records "$1" | tail -n 1; }
field() { printf '%s' "$1" | cut -d'|' -f2- | jq -r "$2"; }
hexkey() { printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }
object() { echo "$R2/objects/$(hexkey "$1")"; }
has_object() { [ -f "$(object "$1").json" ]; }
sha() { sha256sum <"$1" | cut -d' ' -f1; }
size() { wc -c <"$1" | tr -d ' '; }
mode() { echo $((0$(stat -c %a "$1"))); }
note() { echo "216: $*" >&2; }
# The notes folder as it is: every entry's path, type, size, mode, mtime and
# the sha256 of each regular file.
folder() {
    (cd "$SYNC" && find . | LC_ALL=C sort | while read -r entry; do
        if [ -f "$entry" ] && [ ! -L "$entry" ]; then
            printf '%s %s %s\n' "$(stat -c '%n %F %s %a %Y' "$entry")" "$(mode "$entry")" "$(sha "$entry" 2>/dev/null)"
        else
            stat -c '%n %F %a %Y' "$entry"
        fi
    done)
}
# The regular files of the notes folder, relative, sorted.
files_now() { (cd "$SYNC" && find . -type f | sed 's#^\./##' | LC_ALL=C sort); }

# bundle_run <name> [NAME=value ...]: `spawn backup bundle` in an
# environment holding the settings it is given (never on a command line).
# Checks the folder is left as it was and a new record was made; sets bun
# (the record) and dir (its bundle, decrypted and unpacked).
runs=0
bundle_run() {
    br_name=$1
    shift
    # Two bundles in one second would share an object name.
    sleep 1
    br_before=$(folder)
    br_count=$(records backup.bundled | grep -c . || true)
    capture sh -c 'for s in "$@"; do export "$s"; done; exec "$SPAWN_BIN" backup bundle' bundle "$@"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
    after_run "$br_name" "$br_before" "$br_count"
}
after_run() { # after_run <name> <folder before> <record count before>
    [ "$status" -eq 0 ] || fail "($1) the bundle exited $status: $(err | tail -n 2 | tr '\n' ' ')"
    [ "$(folder)" = "$2" ] || fail "($1) the bundle changed the notes folder"
    [ "$(records backup.bundled | grep -c . || true)" -gt "$3" ] || fail "($1) the bundle made no backup.bundled record"
    bun=$(newest backup.bundled)
    br_key=$(field "$bun" '.keys[0] // empty')
    [ -n "$br_key" ] && has_object "$br_key" || fail "($1) the bundle's key [$br_key] is not in the R2 stand-in"
    runs=$((runs + 1))
    dir=$WORK/opened/$runs
    mkdir -p "$dir"
    gpg --batch --quiet --no-tty --pinentry-mode loopback --no-symkey-cache \
        --passphrase-file "$SPAWN_BACKUP_KEY_FILE" -o "$WORK/bundle.$runs" -d "$(object "$br_key")" 2>"$WORK/gpg.err" ||
        harness "($1) the bundle does not decrypt: $(cat "$WORK/gpg.err")"
    case $(od -An -N4 -tx1 "$WORK/bundle.$runs" | tr -d ' \n') in
    28b52ffd) zstd -q -dc "$WORK/bundle.$runs" >"$WORK/bundle.$runs.tar" || harness "($1) zstd -d failed" ;;
    1f8b*) gzip -dc "$WORK/bundle.$runs" >"$WORK/bundle.$runs.tar" || harness "($1) gzip -d failed" ;;
    *) mv "$WORK/bundle.$runs" "$WORK/bundle.$runs.tar" ;;
    esac
    tar -C "$dir" -xpf "$WORK/bundle.$runs.tar" || harness "($1) the bundle's tar does not unpack"
    [ -f "$dir/manifest.json" ] || harness "($1) the bundle holds no manifest.json"
}
# The rest of the bundle: the transcript and the mission, as without 216.
rest_bundled() { # rest_bundled <case>
    [ "$(field "$bun" ".transcripts.sizes[\"$transcript\"] // 0")" = "$(size "$archive/$transcript")" ] ||
        fail "($1) the record does not name the transcript at its size"
    [ "$(field "$bun" '.repos | length')" -ge 1 ] || fail "($1) the record names no mission source"
    [ "$(jq -r '.repos | length' "$dir/manifest.json")" -ge 1 ] || fail "($1) the bundle's manifest names no mission source"
}
refused() { # refused <case> <what>: the sync part dropped, the rest stored
    [ "$(field "$bun" '.sync.refused // empty')" != "" ] ||
        fail "($1) $2: the record's sync part is not refused: $(field "$bun" '.sync | tostring' | cut -c1-200)"
    [ "$(field "$bun" '.sync.manifest_sha256 // empty')" = "" ] || fail "($1) $2: a refused part names a manifest"
    [ ! -e "$dir/sync" ] || fail "($1) $2: the bundle holds a sync/ folder"
    [ "$(jq -r '.sync.refused // empty' "$dir/manifest.json")" != "" ] || fail "($1) $2: the bundle's manifest does not say the part was refused"
    rest_bundled "$1"
}

# --- (1) the full bundle takes every file -------------------------------------
# tar as the bundle runs it, each command line logged (paths only).
mkdir -p "$WORK/bin" "$WORK/tmp"
real_tar=$(command -v tar)
cat >"$WORK/bin/tar" <<TAR
#!/bin/sh
printf '%s\n' "\$*" >>"$WORK/tar.log"
exec "$real_tar" "\$@"
TAR
chmod +x "$WORK/bin/tar"
: >"$WORK/tar.log"
# A scratch folder a killed bundle left (SIGKILL runs no cleanup): the next
# run with the folder removes it, checked in (8).
mkdir -p "$SPAWN_WORKSPACE_ROOT/.spawn-backup-bundle-41-0a1b2c3d0/stage/sync/files"
printf 'left\n' >"$SPAWN_WORKSPACE_ROOT/.spawn-backup-bundle-41-0a1b2c3d0/stage/sync/files/a.md"
bundle_run 1 "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
[ "$(field "$bun" '.sync | type')" = object ] || fail "(1) the record has no sync part (SPAWN_BACKUP_SYNC_DIR set)"
[ "$(field "$bun" '.sync.refused // empty')" = "" ] || fail "(1) the sync part was refused: $(field "$bun" '.sync.refused')"
[ "$(field "$bun" '.sync.kind')" = full ] || fail "(1) the sync part is kind $(field "$bun" '.sync.kind'), not full"
want=$(files_now | grep -c .)
[ "$(field "$bun" '.sync.files')" = "$want" ] || fail "(1) the record counts $(field "$bun" '.sync.files') files, not $want"
[ "$(field "$bun" '.sync.taken')" = "$want" ] || fail "(1) the full took $(field "$bun" '.sync.taken') files, not $want"
[ "$(field "$bun" '.sync.skipped | tostring')" != "[]" ] && field "$bun" '.sync.skipped | tostring' | grep -q latest ||
    fail "(1) the record does not name the symbolic link as skipped: $(field "$bun" '.sync.skipped | tostring')"
[ "$(field "$bun" '[.sync.scan.unseen[] | select(.path == "obj-7/shot.png")] | length')" = 1 ] ||
    fail "(1) the record does not name the binary file as not seen into: $(field "$bun" '.sync.scan | tostring')"
[ -f "$dir/sync/manifest.json" ] || fail "(1) the bundle holds no sync/manifest.json"
[ "$(sha "$dir/sync/manifest.json")" = "$(field "$bun" '.sync.manifest_sha256')" ] ||
    fail "(1) the record's manifest_sha256 is not the sha256 of the bundle's sync/manifest.json"
[ "$(jq -r '.files[].path' "$dir/sync/manifest.json" | LC_ALL=C sort)" = "$(files_now)" ] ||
    fail "(1) the bundle's manifest does not name every file of the folder"
for path in $(files_now); do
    entry=$(jq -c --arg p "$path" '.files[] | select(.path == $p)' "$dir/sync/manifest.json")
    [ "$(printf '%s' "$entry" | jq -r .sha256)" = "$(sha "$SYNC/$path")" ] || fail "(1) the manifest's sha256 of $path is not the file's"
    [ "$(printf '%s' "$entry" | jq -r .size)" = "$(size "$SYNC/$path")" ] || fail "(1) the manifest's size of $path is not the file's"
    [ "$(printf '%s' "$entry" | jq -r .mode)" = "$(mode "$SYNC/$path")" ] || fail "(1) the manifest's mode of $path is not the file's"
    [ -f "$dir/sync/files/$path" ] && cmp -s "$dir/sync/files/$path" "$SYNC/$path" ||
        fail "(1) the bundle's sync/files does not hold $path as it is"
done
packed=$(awk '{ for (i = 1; i < NF; i++) if ($i == "-C") { print $(i + 1); exit } }' "$WORK/tar.log")
case $packed in
"$SPAWN_WORKSPACE_ROOT"/.spawn-backup-bundle-*) ;;
*) fail "(1) the bundle packed its archive from [$packed], not a folder in the workspace root (TMPDIR is $WORK/tmp)" ;;
esac
rest_bundled 1
note "(1) checked: the full bundle holds the folder's $want files and their manifest"

# --- (2) an increment takes what changed --------------------------------------
printf '## lead 10:05 - another line\n' >>"$SYNC/queue.md"
printf 'new package\n' >"$SYNC/obj-7/notes.md"
rm "$SYNC/inbox/rev-x.md"
touch -d '2 minutes ago' "$SYNC/obj-7/sha256.txt"
bundle_run 2 "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
[ "$(field "$bun" '.sync.kind // empty')" = incremental ] || fail "(2) the sync part is not incremental: $(field "$bun" '.sync | tostring' | cut -c1-200)"
[ "$(field "$bun" '.sync.base_manifest')" = used ] || fail "(2) the base manifest was not used: $(field "$bun" '.sync.base_manifest')"
[ "$(field "$bun" '.sync.taken')" = 3 ] || fail "(2) the increment took $(field "$bun" '.sync.taken') files, not 3"
[ "$(field "$bun" '.sync.removed')" = 1 ] || fail "(2) the increment counts $(field "$bun" '.sync.removed') removed, not 1"
[ "$(cd "$dir/sync/files" && find . -type f | sed 's#^\./##' | LC_ALL=C sort | tr '\n' ' ')" = "obj-7/notes.md obj-7/sha256.txt queue.md " ] ||
    fail "(2) the increment's sync/files is not the changed, the added and the touched file"
cmp -s "$dir/sync/files/queue.md" "$SYNC/queue.md" || fail "(2) the increment holds queue.md other than it is"
[ "$(sha "$dir/sync/manifest.json")" = "$(field "$bun" '.sync.manifest_sha256')" ] ||
    fail "(2) the record's manifest_sha256 is not the sha256 of the bundle's sync/manifest.json"
[ "$(jq -r '.files[].path' "$dir/sync/manifest.json" | LC_ALL=C sort)" = "$(files_now)" ] ||
    fail "(2) the increment's manifest does not name every file of the folder now"
[ "$(jq -c '.removed' "$dir/sync/manifest.json")" = '["inbox/rev-x.md"]' ] ||
    fail "(2) the increment's manifest does not name the removed file: $(jq -c '.removed' "$dir/sync/manifest.json")"
rest_bundled 2
note "(2) checked: the increment holds the changed, the added and the touched file, and names the removed one"

# --- (2b) an edited local base manifest is not trusted ----------------------------
local_manifest=$SPAWN_WORKSPACE_ROOT/.backup-sync-manifest.json
[ -f "$local_manifest" ] || fail "(2b) no local copy of the base manifest at $local_manifest"
jq -c . "$local_manifest" >"$WORK/edited.json" || harness "(2b) the local manifest is not JSON"
cat "$WORK/edited.json" >"$local_manifest"
bundle_run 2b "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
case $(field "$bun" '.sync.base_manifest // empty') in
unusable*) ;;
*) fail "(2b) an edited local manifest was not refused as the base: $(field "$bun" '.sync.base_manifest // "none"')" ;;
esac
want=$(files_now | grep -c .)
[ "$(field "$bun" '.sync.taken')" = "$want" ] || fail "(2b) with no usable base the bundle took $(field "$bun" '.sync.taken') files, not all $want"
note "(2b) checked: an edited local manifest is not a base; every file taken"

# --- (2c) other bytes at the same size and mtime are taken (rev-k's F1) -----------
cp -p "$SYNC/obj-7/notes.md" "$WORK/notes.ref"
printf 'NEW PACKAGE\n' >"$SYNC/obj-7/notes.md"
touch -r "$WORK/notes.ref" "$SYNC/obj-7/notes.md"
[ "$(size "$SYNC/obj-7/notes.md")" = "$(size "$WORK/notes.ref")" ] || harness "(2c) the rewrite changed the size"
bundle_run 2c "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
[ "$(field "$bun" '.sync.base_manifest')" = used ] || fail "(2c) the base manifest was not used: $(field "$bun" '.sync.base_manifest')"
[ "$(cd "$dir/sync/files" && find . -type f | sed 's#^\./##' | tr '\n' ' ')" = "obj-7/notes.md " ] ||
    fail "(2c) a file with other bytes at the same size and mtime was not taken: [$(cd "$dir/sync/files" 2>/dev/null && find . -type f | tr '\n' ' ')]"
note "(2c) checked: other bytes at the same size and mtime are taken"

# --- (3) a credential value drops the part ----------------------------------------
printf 'a note that should not be here: %s\n' "$made_value" >"$SYNC/obj-7/leak.md"
bundle_run 3 "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
refused 3 "a credential value"
note "(3) checked: a credential value drops the folder, the rest is stored"

# --- (4) a place the scan could not search drops the part ------------------------
rm "$SYNC/obj-7/leak.md"
printf 'not a zstd frame\n' >"$SYNC/obj-7/old.blind"
bundle_run 4 "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
refused 4 "a place not seen into"
note "(4) checked: a place the scan could not search drops the folder"

# --- (5) no scan tool, no part -----------------------------------------------------
rm "$SYNC/obj-7/old.blind"
bundle_run 5 "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$WORK/no-such-scan.py" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
refused 5 "the scan tool missing"
note "(5) checked: without its scan the folder is dropped"

# --- (5b) a file the run cannot read drops the part, named --------------------------
printf 'kept from others\n' >"$SYNC/obj-7/locked.md"
chmod 000 "$SYNC/obj-7/locked.md"
if cat "$SYNC/obj-7/locked.md" >/dev/null 2>&1; then
    note "(5b) skipped: this user reads a mode-000 file (root)"
else
    bundle_run 5b "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp" "PATH=$WORK/bin:$PATH"
    refused 5b "an unreadable file"
    field "$bun" '.sync.refused' | grep -q 'locked\.md' ||
        fail "(5b) the refusal does not name the unreadable file: $(field "$bun" '.sync.refused')"
    note "(5b) checked: an unreadable file drops the folder, named; the rest is stored"
fi
chmod 600 "$SYNC/obj-7/locked.md"
rm "$SYNC/obj-7/locked.md"

# --- (6) unset: nothing of 216 --------------------------------------------------------
bundle_run 6
[ "$(field "$bun" 'has("sync")')" = false ] || fail "(6) SPAWN_BACKUP_SYNC_DIR unset, the record still has a sync part"
[ "$(jq -r 'has("sync")' "$dir/manifest.json")" = false ] || fail "(6) SPAWN_BACKUP_SYNC_DIR unset, the bundle's manifest has a sync part"
[ ! -e "$dir/sync" ] || fail "(6) SPAWN_BACKUP_SYNC_DIR unset, the bundle holds a sync/ folder"
rest_bundled 6
note "(6) checked: unset, the bundle is as before"

# --- (7) the bundle unit takes the notes folder ---------------------------------------
# The checkout under test is the working directory; its unit file is run as
# systemd runs it: ExecStart (%h the scratch HOME) in an environment of its
# EnvironmentFile and Environment lines only. Its notes folder,
# /var/tmp/spawn/sync, is pointed at the scratch one; any other is left out,
# so nothing live is read.
tree=$(pwd)
unit=$tree/deploy/systemd/spawn-backup-bundle.service
[ -f "$unit" ] || harness "no deploy/systemd/spawn-backup-bundle.service under $tree: run from the checkout"
mkdir -p "$live_dir/bin"
ln -s "$SPAWN_BIN" "$live_dir/bin/spawn"
umask 077
cat >"$live_dir/spawn.env" <<ENV
SPAWN_DATABASE_URL=$SPAWN_DATABASE_URL
SPAWN_R2_ENV_FILE=$SPAWN_R2_ENV_FILE
SPAWN_BACKUP_KEY_FILE=$SPAWN_BACKUP_KEY_FILE
ENV
umask 022
ur_exec=$(sed -n 's/^ExecStart=//p' "$unit" | head -n 1 | sed "s#%h#$HOME#g")
ur_envfile=$(sed -n 's/^EnvironmentFile=//p' "$unit" | head -n 1 | sed "s#%h#$HOME#g")
[ -n "$ur_exec" ] && [ -n "$ur_envfile" ] || harness "the unit names no ExecStart or EnvironmentFile"
sed -n 's/^Environment=//p' "$unit" | sed "s#%h#$HOME#g" |
    awk -v sync="$SYNC" '/^SPAWN_BACKUP_SYNC_DIR=/ { if ($0 != "SPAWN_BACKUP_SYNC_DIR=/var/tmp/spawn/sync") next; $0 = "SPAWN_BACKUP_SYNC_DIR=" sync } { print }' \
        >"$WORK/unit.environment"
echo "SPAWN_BACKUP_SYNC_SCAN=$SCAN" >>"$WORK/unit.environment"
unit_run() {
    (
        for ur_name in $(env | sed -n 's/^\(SPAWN_[A-Za-z0-9_]*\)=.*/\1/p; s/^\(PG[A-Z]*\)=.*/\1/p'); do
            unset "$ur_name"
        done
        unset GIT_CONFIG_GLOBAL
        set -a
        . "$ur_envfile"
        set +a
        while read -r ur_line; do
            for ur_setting in $ur_line; do export "$ur_setting"; done
        done <"$WORK/unit.environment"
        # shellcheck disable=SC2086 # ExecStart is a command line, split as systemd does
        exec $ur_exec
    )
}
printf 'the unit run\n' >"$SYNC/obj-7/unit.md"
sleep 1
before=$(folder)
count=$(records backup.bundled | grep -c . || true)
capture unit_run
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
after_run 7 "$before" "$count"
[ "$(field "$bun" '.sync | type')" = object ] && [ "$(field "$bun" '.sync.refused // empty')" = "" ] ||
    fail "(7) the bundle unit's run took no notes folder: $(field "$bun" '.sync | tostring' | cut -c1-200)"
[ -f "$dir/sync/files/obj-7/unit.md" ] || fail "(7) the bundle unit's run did not take the new file"
note "(7) checked: the bundle unit takes the notes folder"

# --- (8) no secret anywhere ---------------------------------------------------------
for secret in "$passphrase" "$r2_secret" "$made_value"; do
    if grep -qF -- "$secret" "$WORK/backup.log"; then fail "(8) a secret is in the command's output"; fi
    [ "$(q "select count(*) from spawn_events where kind::text like '%$secret%'")" = 0 ] ||
        fail "(8) a secret is in a record"
done
left=$(find "$SPAWN_WORKSPACE_ROOT" -maxdepth 1 -name '.spawn-backup-bundle-*' | wc -l | tr -d ' ')
[ "$left" = 0 ] || fail "(8) $left bundle scratch folder(s) left in the workspace"
note "(8) checked: the folder unchanged by every run; no scratch left; no secret in any output or record"
echo "216: the notes folder goes into the bundle, scanned, read only"
