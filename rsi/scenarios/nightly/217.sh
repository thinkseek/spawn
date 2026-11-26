#!/bin/sh
# Objective 217 [recoverability]: the fleet's notes folder, which objective
# 216 puts into each bundle (the record's `sync` part), restored from the
# newest bundle chain and compared with the folder, read only.
# `spawn backup restore-drill --only sync` unpacks from each link of the
# chain only manifest.json, sync/manifest.json and sync/files/* (decrypted in
# a pipe), checks each link's notes manifest against its record's sha256,
# restores every file of the newest manifest from the newest link that took
# it (its size and sha256 the manifest's), compares the folder the record
# names with it (the same; changed, removed or made since: expected; other
# bytes with no later mtime, or an older file the backup lacks: a mismatch),
# and records backup.drilled.sync alone. deploy/systemd/
# spawn-backup-restore-drill-sync.{service,timer} run it daily (the timer
# ships disabled).
#
# Black box, on its own database, scratch HOME and GNUPGHOME, against 104's
# local R2 stand-in (216.sh's world): two real `spawn backup bundle` runs
# over a made-up notes folder, a full and an increment after a change, an
# add and a remove. The drill is given `--into <scratch parent>`.
#   (1) Setup: the full and the increment, each with a sync part.
#   (2) The drill passes (exit 0), says "--only sync", restores every file
#       ("<n> file(s)"), all "the same", and records exactly one
#       backup.drilled.sync {ok true, reason "", the newest bundle's key and
#       event, 0 mismatches} and no other drill record; nothing of its own
#       is left in the parent.
#   (3) With --keep, the kept folder's sync/ holds every file of the folder,
#       byte for byte, a file of the full the increment did not take
#       included, and no transcript or repository bundle.
#   (4) After a file is appended to, one is added and one removed (no new
#       bundle): passes, "1 changed since, 1 removed since, 1 made since".
#   (5) A file given other bytes with its bundled mtime put back, one given
#       other bytes with an mtime between its bundled one and the bundle's
#       start (the backup holds older bytes than the folder had then), and
#       an old file the backup lacks: exit 1, 3 mismatches, each path named;
#       the record ok false with 3 mismatches.
#   (5b) The notes folder gone (moved away for one run): exit 1, one
#       mismatch naming the folder; it is put back as it was.
#   (5c) A live file made unreadable (mode 000) for one run: exit 1, one
#       mismatch naming it; its mode is put back.
#   (5d) The bundled symbolic link (the bundle lists it as skipped)
#       replaced by a regular file written a day before the bundle: passes,
#       "1 skipped or changed while bundled", no mismatch; the link is put
#       back with its times.
#   (6) The newest link's stored object replaced by another ciphertext
#       under the same passphrase: the backup cannot be restored (exit 1),
#       the record ok false saying so; nothing left in the parent.
#   (7) The unit runs `restore-drill --only sync` (as systemd runs it, with
#       --into), and passes; the timer names an OnCalendar and is not
#       enabled by any file of the checkout.
#   (8) --into under the notes folder: exit 2, nothing recorded.
#   (8b) With a floor no disk meets (SPAWN_BACKUP_DRILL_FLOOR_MB): exit 2,
#       one line naming the floor, nothing recorded, nothing made.
#   (9) A newest bundle whose notes part was dropped (a credential value):
#       exit 1, the record's reason naming the dropped part.
#   (9b) A newest bundle made with no notes folder set (no sync part in its
#       record): exit 1, the record's reason saying it holds no notes folder.
#   (10) Every drill leaves the notes folder exactly as it was (path, type,
#       size, mode, mtime, sha256), and no secret (the passphrase, the R2
#       secret key, the made-up value) and no file's content is in any
#       output or record.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "217: harness: $*" >&2
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
# A scratch HOME in WORK: nothing here reads or writes the real home, nor
# is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
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
SPAWN_DB_NAME="bb_217_sync_$run"
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
    set +e
    if [ "$rc" -ne 0 ]; then
        if [ -s "$R2/requests.log" ]; then
            echo "217: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "217: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "217: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "217: harness: unexpected exit $rc" >&2
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
access_key="bb217$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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
transcript=agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0217.jsonl
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
made_value="bb217tok$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
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
    --text "217 mission: keep the fleet's notes off this host, and prove they restore"
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
note() { echo "217: $*" >&2; }
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
# The rest of the bundle: the transcript and the mission, as without 217.
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

event_of() { printf '%s' "$1" | cut -d'|' -f1; }
count_of() { records "$1" | grep -c . || true; }
mkdir -p "$WORK/drills"
# What a drill left in the parent.
own_left() { find "$WORK/drills" -mindepth 1 -maxdepth 1 | head -n 3; }
# drill_sync <log> [arg ...]: `restore-drill --only sync` into the scratch
# parent; the notes folder must be exactly as it was before (path, type,
# size, mode, mtime, sha256: `folder`).
drill_sync() {
    ds_log=$1
    shift
    ds_before=$(folder)
    capture "$SPAWN_BIN" backup restore-drill --only sync --into "$WORK/drills" "$@"
    cat "$WORK/out.txt" "$WORK/err.txt" >"$ds_log"
    cat "$ds_log" >>"$WORK/backup.log"
    [ "$(folder)" = "$ds_before" ] || fail "(10) the drill ($(basename "$ds_log")) changed the notes folder"
}
bundle_env() { echo "SPAWN_BACKUP_SYNC_DIR=$SYNC" "SPAWN_BACKUP_SYNC_SCAN=$SCAN" "TMPDIR=$WORK/tmp"; }

# --- (1) setup: a full, then an increment ------------------------------------------
mkdir -p "$WORK/tmp"
# shellcheck disable=SC2046 # three settings, each one word
bundle_run 1 $(bundle_env)
[ "$(field "$bun" '.sync.kind // empty')" = full ] || harness "(1) the first bundle holds no full notes part: $(field "$bun" '.sync | tostring' | cut -c1-200)"
printf '## lead 10:05 - another line\n' >>"$SYNC/queue.md"
printf 'new package\n' >"$SYNC/obj-7/notes.md"
rm "$SYNC/inbox/rev-x.md"
# shellcheck disable=SC2046
bundle_run 1 $(bundle_env)
[ "$(field "$bun" '.sync.kind // empty')" = incremental ] && [ "$(field "$bun" '.sync.base_manifest')" = used ] ||
    harness "(1) the second bundle is no increment on its base: $(field "$bun" '.sync | tostring' | cut -c1-200)"
[ "$(field "$bun" '.sync.taken')" = 2 ] || harness "(1) the increment took $(field "$bun" '.sync.taken') files, not 2"
bkey=$(field "$bun" '.keys[0]')
bevent=$(event_of "$bun")
# When the increment started, and the bundled mtime of a file it did not take.
b2start=$(jq -r '.created_at_ms' "$dir/manifest.json")
st_mtime=$(jq -r '.files[] | select(.path == "obj-7/sha256.txt") | .mtime_ms' "$dir/sync/manifest.json")
[ $((b2start - st_mtime)) -ge 2 ] 2>/dev/null ||
    harness "(1) obj-7/sha256.txt's bundled mtime [$st_mtime] is not well before the increment's start [$b2start]"
want=$(files_now | grep -c .)
note "(1) set up: a full and an increment over $want files"

# --- (2) the drill passes and records backup.drilled.sync alone ---------------------
drill_sync "$WORK/drill2.log"
[ "$status" -eq 0 ] || fail "(2) the drill exited $status, not 0: $(tail -n 4 "$WORK/drill2.log" | tr '\n' ' ')"
grep -qF -- '--only sync' "$WORK/drill2.log" || fail "(2) the drill does not say --only sync"
grep -qF "the notes folder: $want file(s)" "$WORK/drill2.log" ||
    fail "(2) the drill does not say it restored $want file(s): $(grep -F 'notes folder' "$WORK/drill2.log" | tr '\n' ' ')"
grep -qF "$want the same, 0 changed since, 0 removed since, 0 made since" "$WORK/drill2.log" ||
    fail "(2) the drill does not find all $want files the same: $(grep -F 'against live' "$WORK/drill2.log" | tr '\n' ' ')"
[ "$(count_of backup.drilled.sync)" = 1 ] || fail "(2) $(count_of backup.drilled.sync) backup.drilled.sync records, not 1"
[ "$(count_of backup.drilled.transcripts)" = 0 ] && [ "$(count_of backup.drilled.missions)" = 0 ] ||
    fail "(2) the sync drill recorded a transcripts or missions verdict"
rec=$(newest backup.drilled.sync)
[ "$(field "$rec" '.ok')" = true ] && [ "$(field "$rec" '.reason')" = "" ] && [ "$(field "$rec" '.mismatches')" = 0 ] ||
    fail "(2) the record is not a pass: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 300)"
[ "$(field "$rec" '.bundle')" = "$bkey" ] && [ "$(field "$rec" '.bundle_event_id')" = "$bevent" ] ||
    fail "(2) the record names bundle $(field "$rec" '.bundle') event $(field "$rec" '.bundle_event_id'), not $bkey event $bevent"
[ "$(field "$rec" 'keys | join(",")')" = "bundle,bundle_event_id,duration_ms,mismatches,ok,reason,type" ] ||
    fail "(2) the record's fields are $(field "$rec" 'keys | join(",")')"
[ -z "$(own_left)" ] || fail "(2) the drill left in the parent: $(own_left)"
note "(2) checked: the drill passes, all $want files the same, one backup.drilled.sync"

# --- (3) the kept folder holds the folder, byte for byte -------------------------------
drill_sync "$WORK/drill3.log" --keep
[ "$status" -eq 0 ] || fail "(3) the kept drill exited $status"
kept=$(find "$WORK/drills" -mindepth 1 -maxdepth 1 -type d -name 'spawn-restore-drill-*' | head -n 1)
[ -n "$kept" ] || harness "--keep left no drill folder under $WORK/drills"
for path in $(files_now); do
    cmp -s "$kept/sync/$path" "$SYNC/$path" || fail "(3) the restored sync/$path is not the folder's"
done
[ "$(cd "$kept/sync" && find . -type f | sed 's#^\./##' | LC_ALL=C sort)" = "$(files_now)" ] ||
    fail "(3) the restored folder holds other files than the folder"
[ -z "$(find "$kept" -path '*/unpacked/transcripts*' -o -path '*/unpacked/repos*' | head -n 1)" ] ||
    fail "(3) the drill unpacked a transcript or a repository bundle"
chmod -R u+rwX "$kept" && rm -rf "$kept"
note "(3) checked: the kept folder holds every file, the full's untaken ones included"

# --- (4) changed, removed and made since: expected --------------------------------------
printf '## lead 11:00 - a line after the bundle\n' >>"$SYNC/queue.md"
printf 'after the bundle\n' >"$SYNC/obj-7/after.md"
rm "$SYNC/ready/7"
drill_sync "$WORK/drill4.log"
[ "$status" -eq 0 ] || fail "(4) the drill after expected changes exited $status: $(tail -n 4 "$WORK/drill4.log" | tr '\n' ' ')"
grep -qF "$((want - 2)) the same, 1 changed since, 1 removed since, 1 made since" "$WORK/drill4.log" ||
    fail "(4) the drill does not count 1 changed, 1 removed, 1 made since: $(grep -F 'against live' "$WORK/drill4.log" | tr '\n' ' ')"
[ "$(field "$(newest backup.drilled.sync)" '.ok')" = true ] || fail "(4) expected changes failed the record"
note "(4) checked: changed, removed and made since pass"

# --- (5) other bytes with the bundled mtime, and an old file the backup lacks ---------
cp -p "$SYNC/obj-7/diff.patch" "$WORK/diff.ref"
printf 'other bytes, the mtime put back\n' >"$SYNC/obj-7/diff.patch"
touch -r "$WORK/diff.ref" "$SYNC/obj-7/diff.patch"
cp -p "$SYNC/obj-7/sha256.txt" "$WORK/sha.ref"
mid=$(((st_mtime + b2start) / 2))
printf 'other bytes, written before the bundle began\n' >"$SYNC/obj-7/sha256.txt"
touch -d "@$((mid / 1000)).$(printf %03d $((mid % 1000)))" "$SYNC/obj-7/sha256.txt"
printf 'an old note\n' >"$SYNC/old.md"
touch -d '2 days ago' "$SYNC/old.md"
drill_sync "$WORK/drill5.log"
[ "$status" -eq 1 ] || fail "(5) the drill over three mismatches exited $status, not 1: $(tail -n 4 "$WORK/drill5.log" | tr '\n' ' ')"
grep -qF 'restore-drill: mismatch: sync/obj-7/diff.patch: live has other bytes' "$WORK/drill5.log" ||
    fail "(5) the drill does not name obj-7/diff.patch's other bytes"
grep -qF 'restore-drill: mismatch: sync/obj-7/sha256.txt: live has other bytes' "$WORK/drill5.log" ||
    fail "(5) the drill does not name obj-7/sha256.txt, written between its bundled copy and the bundle's start"
grep -qF 'restore-drill: mismatch: sync/old.md: live, last written' "$WORK/drill5.log" ||
    fail "(5) the drill does not name the old file the backup lacks"
rec=$(newest backup.drilled.sync)
[ "$(field "$rec" '.ok')" = false ] && [ "$(field "$rec" '.mismatches')" = 3 ] &&
    field "$rec" '.reason' | grep -q '^sync: 3 mismatch(es) older than bundle' ||
    fail "(5) the record is not 3 mismatches: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 300)"
cp -p "$WORK/diff.ref" "$SYNC/obj-7/diff.patch"
cp -p "$WORK/sha.ref" "$SYNC/obj-7/sha256.txt"
rm "$SYNC/old.md"
note "(5) checked: other bytes last written before the bundle started, and an old file the backup lacks, are mismatches"

# --- (5b) the notes folder gone ---------------------------------------------------------
listing=$(folder)
mv "$SYNC" "$WORK/notes.away"
capture "$SPAWN_BIN" backup restore-drill --only sync --into "$WORK/drills"
cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/drill5b.log"
cat "$WORK/drill5b.log" >>"$WORK/backup.log"
mv "$WORK/notes.away" "$SYNC"
[ "$(folder)" = "$listing" ] || harness "(5b) the notes folder did not come back as it was"
[ "$status" -eq 1 ] || fail "(5b) the drill with the notes folder gone exited $status, not 1: $(tail -n 3 "$WORK/drill5b.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.sync)
[ "$(field "$rec" '.ok')" = false ] && [ "$(field "$rec" '.mismatches')" = 1 ] &&
    field "$rec" '.reason' | grep -qF "the notes folder $SYNC" ||
    fail "(5b) the record with the notes folder gone: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 300)"
[ -z "$(own_left)" ] || fail "(5b) the drill left in the parent: $(own_left)"
note "(5b) checked: a notes folder that cannot be read is a mismatch naming it"

# --- (5c) a live file that cannot be read --------------------------------------------
locked_mode=$(stat -c %a "$SYNC/obj-7/diff.patch")
chmod 000 "$SYNC/obj-7/diff.patch"
if cat "$SYNC/obj-7/diff.patch" >/dev/null 2>&1; then
    chmod "$locked_mode" "$SYNC/obj-7/diff.patch"
    harness "(5c) a mode-000 file is still readable (root?): the rule cannot be shown"
fi
drill_sync "$WORK/drill5c.log"
chmod "$locked_mode" "$SYNC/obj-7/diff.patch"
[ "$status" -eq 1 ] || fail "(5c) the drill over an unreadable file exited $status, not 1: $(tail -n 3 "$WORK/drill5c.log" | tr '\n' ' ')"
grep -qF 'restore-drill: mismatch: sync/obj-7/diff.patch: live cannot be read' "$WORK/drill5c.log" ||
    fail "(5c) the drill does not name obj-7/diff.patch as unreadable: $(grep -F 'against live' "$WORK/drill5c.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.sync)
[ "$(field "$rec" '.ok')" = false ] && [ "$(field "$rec" '.mismatches')" = 1 ] ||
    fail "(5c) the record over an unreadable file: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 300)"
[ -z "$(own_left)" ] || fail "(5c) the drill left in the parent: $(own_left)"
note "(5c) checked: a live file that cannot be read is a mismatch naming it"

# --- (5d) a live file under a name the bundle skipped -----------------------------------
# `latest` is a symbolic link: the bundle listed it as skipped. Live, it is
# now a regular file written a day before the bundle: said apart, no mismatch.
[ -L "$SYNC/latest" ] || harness "(5d) latest is not a symbolic link"
: >"$WORK/latest.times"
touch -h -r "$SYNC/latest" "$WORK/latest.times"
rm "$SYNC/latest"
printf 'a file where the link was\n' >"$SYNC/latest"
touch -d '1 day ago' "$SYNC/latest"
drill_sync "$WORK/drill5d.log"
rm "$SYNC/latest"
ln -s obj-7 "$SYNC/latest"
touch -h -r "$WORK/latest.times" "$SYNC/latest"
[ "$status" -eq 0 ] || fail "(5d) the drill over a file under a skipped name exited $status, not 0: $(tail -n 3 "$WORK/drill5d.log" | tr '\n' ' ')"
grep -qF '1 skipped or changed while bundled, 0 mismatch(es)' "$WORK/drill5d.log" ||
    fail "(5d) the drill does not say the skipped name apart: $(grep -F 'against live' "$WORK/drill5d.log" | tr '\n' ' ')"
[ "$(field "$(newest backup.drilled.sync)" '.ok')" = true ] || fail "(5d) a file under a skipped name failed the record"
note "(5d) checked: a live file under a name the bundle skipped is said apart"

# --- (6) a link whose stored object is another ciphertext --------------------------------
obj=$(object "$bkey")
cp "$obj" "$WORK/object.keep"
mkdir -p "$WORK/fake/sync/files"
printf '{"kind":"incremental"}\n' >"$WORK/fake/manifest.json"
printf '{"files":[]}\n' >"$WORK/fake/sync/manifest.json"
tar -C "$WORK/fake" -cf "$WORK/fake.tar" ./manifest.json ./sync/manifest.json
gpg --batch --yes --quiet --pinentry-mode loopback --passphrase-file "$SPAWN_BACKUP_KEY_FILE" \
    --symmetric --cipher-algo AES256 --output "$obj" "$WORK/fake.tar" ||
    harness "cannot write the replaced object"
drill_sync "$WORK/drill6.log"
[ "$status" -eq 1 ] || fail "(6) the drill over a replaced object exited $status, not 1: $(tail -n 3 "$WORK/drill6.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.sync)
[ "$(field "$rec" '.ok')" = false ] && field "$rec" '.reason' | grep -qF 'the backup cannot be restored' &&
    field "$rec" '.reason' | grep -qF 'decrypts to sha256' ||
    fail "(6) the record over a replaced object: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 300)"
[ -z "$(own_left)" ] || fail "(6) the failed drill left in the parent: $(own_left)"
cp "$WORK/object.keep" "$obj"
note "(6) checked: a link that is not its record's sha256 cannot be restored"

# --- (7) the unit and its timer -------------------------------------------------------------
tree=$(pwd)
unit=$tree/deploy/systemd/spawn-backup-restore-drill-sync.service
timer=$tree/deploy/systemd/spawn-backup-restore-drill-sync.timer
[ -f "$unit" ] && [ -f "$timer" ] || fail "(7) no deploy/systemd/spawn-backup-restore-drill-sync.{service,timer} under $tree"
grep -q '^OnCalendar=' "$timer" || fail "(7) the timer names no OnCalendar"
mkdir -p "$live_dir/bin"
[ -e "$live_dir/bin/spawn" ] || ln -s "$SPAWN_BIN" "$live_dir/bin/spawn"
umask 077
cat >"$live_dir/spawn.env" <<ENV
SPAWN_DATABASE_URL=$SPAWN_DATABASE_URL
SPAWN_R2_ENV_FILE=$SPAWN_R2_ENV_FILE
SPAWN_BACKUP_KEY_FILE=$SPAWN_BACKUP_KEY_FILE
ENV
umask 022
ur_exec=$(sed -n 's/^ExecStart=//p' "$unit" | head -n 1 | sed "s#%h#$HOME#g")
ur_envfile=$(sed -n 's/^EnvironmentFile=//p' "$unit" | head -n 1 | sed "s#%h#$HOME#g")
[ -n "$ur_exec" ] && [ -n "$ur_envfile" ] || fail "(7) the unit names no ExecStart or EnvironmentFile"
sed -n 's/^Environment=//p' "$unit" | sed "s#%h#$HOME#g" >"$WORK/unit.environment"
unit_run() { # the unit's ExecStart in its environment only, given --into
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
        exec $ur_exec --into "$WORK/drills"
    )
}
before=$(folder)
count=$(count_of backup.drilled.sync)
capture unit_run
cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/drill7.log"
cat "$WORK/drill7.log" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || fail "(7) the unit's drill exited $status: $(tail -n 4 "$WORK/drill7.log" | tr '\n' ' ')"
grep -qF -- '--only sync' "$WORK/drill7.log" || fail "(7) the unit's drill is not --only sync"
[ "$(count_of backup.drilled.sync)" = $((count + 1)) ] || fail "(7) the unit's drill recorded no backup.drilled.sync"
[ "$(folder)" = "$before" ] || fail "(10) the unit's drill changed the notes folder"
[ -z "$(own_left)" ] || fail "(7) the unit's drill left in the parent: $(own_left)"
note "(7) checked: the unit runs the sync drill and passes"

# --- (8) never into the notes folder ----------------------------------------------------------
count=$(count_of backup.drilled.sync)
before=$(folder)
capture "$SPAWN_BIN" backup restore-drill --only sync --into "$SYNC/obj-7"
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
[ "$status" -eq 2 ] || fail "(8) --into under the notes folder exited $status, not 2"
[ "$(count_of backup.drilled.sync)" = "$count" ] || fail "(8) --into under the notes folder recorded a verdict"
[ "$(folder)" = "$before" ] || fail "(8) --into under the notes folder changed it"
note "(8) checked: --into under the notes folder is refused, nothing recorded"

# --- (8b) the free-space floor --------------------------------------------------------------
count=$(count_of backup.drilled.sync)
capture env SPAWN_BACKUP_DRILL_FLOOR_MB=999999999 "$SPAWN_BIN" backup restore-drill --only sync --into "$WORK/drills"
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
[ "$status" -eq 2 ] || fail "(8b) a floor no disk meets exited $status, not 2"
grep -qF 'SPAWN_BACKUP_DRILL_FLOOR_MB' "$WORK/err.txt" || fail "(8b) the refusal does not name the floor: $(head -c 300 "$WORK/err.txt")"
[ "$(count_of backup.drilled.sync)" = "$count" ] || fail "(8b) the refused drill recorded a verdict"
[ -z "$(own_left)" ] || fail "(8b) the refused drill made in the parent: $(own_left)"
note "(8b) checked: the floor refuses the sync drill before anything is fetched"

# --- (9) a newest bundle whose notes part was dropped -------------------------------------------
printf 'a note that should not be here: %s\n' "$made_value" >"$SYNC/obj-7/leak.md"
# shellcheck disable=SC2046
bundle_run 9 $(bundle_env)
[ "$(field "$bun" '.sync.refused // empty')" != "" ] || harness "(9) the bundle with a credential value kept its notes part"
drill_sync "$WORK/drill9.log"
[ "$status" -eq 1 ] || fail "(9) the drill over a dropped part exited $status, not 1: $(tail -n 3 "$WORK/drill9.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.sync)
[ "$(field "$rec" '.ok')" = false ] && field "$rec" '.reason' | grep -qF 'dropped its notes folder' ||
    fail "(9) the record over a dropped part: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 300)"
note "(9) checked: a newest bundle whose notes part was dropped fails, naming it"

# --- (9b) a newest bundle with no notes part ----------------------------------------------------
bundle_run 9b "TMPDIR=$WORK/tmp"
[ "$(field "$bun" '.sync // empty')" = "" ] || harness "(9b) the bundle with no SPAWN_BACKUP_SYNC_DIR has a sync part"
drill_sync "$WORK/drill9b.log"
[ "$status" -eq 1 ] || fail "(9b) the drill over a bundle with no notes part exited $status, not 1: $(tail -n 3 "$WORK/drill9b.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.sync)
[ "$(field "$rec" '.ok')" = false ] && field "$rec" '.reason' | grep -qF 'holds no notes folder' ||
    fail "(9b) the record over a bundle with no notes part: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 300)"
note "(9b) checked: a newest bundle with no notes part fails, saying so"

# --- (10) no secret, no content -----------------------------------------------------------------
for secret in "$passphrase" "$r2_secret" "$made_value" 'a queue line' 'new package'; do
    if grep -qF -- "$secret" "$WORK/backup.log"; then fail "(10) a secret or a file's content is in the output"; fi
    [ "$(q "select count(*) from spawn_events where kind::text like '%$secret%'")" = 0 ] ||
        fail "(10) a secret or a file's content is in a record"
done
note "(10) checked: every drill left the folder as it was; no secret or content in any output or record"
echo "217: the notes folder restores from the bundle chain and matches live"
