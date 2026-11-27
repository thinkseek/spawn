#!/bin/sh
# Objective 114 [recoverability], two backup keys, part A (dumps, bundles,
# verify, the restore drill): every new dump and bundle is sealed with the
# key SPAWN_BACKUP_SEAL_KEY names (unset: k1) and says so, in its record
# (`sealed_with`) and on its object (`x-amz-meta-spawn-seal`); verify and the
# drill read each with the key its record names (none: k1); the first bundle
# under a new key is full, and a reader that lacks a key says which.
# (sync/design-backup-two-keys.md revision 2, rev-g PASS; the owner,
# lead-mac 10:01.)
#
# Black box, on its own database, scratch HOME, GNUPGHOME and key directory
# (SPAWN_BACKUP_KEY_DIR, never the real ~/.config/spawn), against 104's local
# R2 stand-in, with 104's two mission sources and transcript archive. Made-up
# keys: k1 starts as the old name `backup.key`; k2 is made at the switch.
#   (1) Before the switch (SPAWN_BACKUP_SEAL_KEY unset, only `backup.key`):
#       `spawn backup dump` and `spawn backup bundle` exit 0; their records
#       say `"sealed_with": "k1"` and every object they stored carries
#       `x-amz-meta-spawn-seal: k1`; so does every object `spawn store put`
#       stores (a second stand-in, its bucket bb114-work, under a locktest/
#       prefix; never a real bucket, and neither r2-store.env nor r2.env is
#       read).
#   (1b) The runbook's window: `backup.key.k2` made, `backup.key` still
#       there, SPAWN_BACKUP_SEAL_KEY unset. A dump exits 0, says k1 in its
#       record and on its object, and decrypts with `backup.key` and not
#       with `backup.key.k2`; a bundle exits 0, an increment on (1)'s full
#       bundle, saying k1 in its record and on its object; verify exits 0,
#       its record saying k1 for the dump and the bundle.
#   (2) The switch (`backup.key` renamed `backup.key.k1`,
#       SPAWN_BACKUP_SEAL_KEY=k2): a dump exits 0, says k2 in its
#       record and on its object, and the object decrypts with k2 to its
#       record's sha256, and not with k1; a store put (a fresh prefix: the
#       store reading an older key's manifest is objective 116's) stores
#       objects that all say k2, and its manifest decrypts with k2 to a
#       manifest, and not with k1.
#   (3) `spawn backup verify` then exits 0: its record says `sealed_with`
#       k2 (the dump) and `bundle.sealed_with` k1 (yesterday's bundle). With
#       the dump's object header changed to k1 under its k2 record, verify
#       exits 1, reason exactly "record says k2, object says k1"; with the
#       bundle's changed to k2 under its k1 record, "record says k1, object
#       says k2".
#   (4) With k1's file away: verify exits 1, its reason "the bundle is
#       sealed with k1; its key file <dir>/backup.key.k1 (or backup.key) is
#       not on this host"; the drill exits 2 and records both resources
#       failed, reason exactly "the bundle chain is sealed with k1; its key
#       file <dir>/backup.key.k1 (or backup.key) is not on this host".
#       With k1 back and the k1 bundle's object stripped of its header (as
#       every bundle stored before this objective is), the drill exits 0 and
#       both resources pass: no header means k1.
#   (5) The first bundle under k2 is full with no base (a full k1 bundle is
#       there from this week), says k2 in its record and on its object, and
#       decrypts with k2 and not k1; the next is an increment on it.
#   (6) A host with only k2 and SPAWN_BACKUP_SEAL_KEY unset drills the k2
#       chain: exit 0, both resources ok.
#   (7) A host with only k1, the newest chain k2: the drill exits 2, both
#       resources failed, reason exactly "the bundle chain is sealed with
#       k2; its key file <dir>/backup.key.k2 is not on this host".
#   (8) The newest object's x-amz-meta-spawn-seal changed to k1 in the
#       stand-in, its record saying k2: the drill exits 1, both resources
#       failed, the reason holding "record says k2, object says k1".
#   (9) SPAWN_BACKUP_SEAL_KEY set to a value that is no key name (a made-up
#       secret-shaped value, as a key's content put there by mistake) and to
#       k3 (no file): each dump exits 2, stores nothing and records the
#       refusal, naming "SPAWN_BACKUP_SEAL_KEY is not a backup key name (k
#       and a number from 1, as k2)" or backup.key.k3; neither falls back to
#       k1.
#   (10) A newest bundle record whose sealed_with is that value: the drill
#       exits 2, both resources failed, reason exactly "the sealed_with of
#       backup.bundled record <its event id> is not a backup key name (k and
#       a number from 1, as k2)".
#   (11) No key's passphrase and no R2 secret is in any output or record.
#   (12) The same value as an object's x-amz-meta-spawn-seal: verify exits 1
#       with reason exactly "the x-amz-meta-spawn-seal of <the dump's object
#       key> is not a backup key name (k and a number from 1, as k2)"; the
#       drill exits 1, both resources failed, the reason holding "the
#       x-amz-meta-spawn-seal of <the bundle's object key> is not a backup
#       key name". The value is in no output of any run and in no record but
#       the one (10) planted (cc-astra A53: a value that is not a name can
#       hold anything, so it is never echoed).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "114: harness: $*" >&2
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
SPAWN_DB_NAME="bb_114_keys_$run"
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
r2s_pid=""
serve_pid=""
R2=$WORK/r2
GNUPGHOME=$WORK/gnupg
export GNUPGHOME
cleanup() {
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    if [ -n "$r2s_pid" ]; then
        kill "$r2s_pid" 2>/dev/null || true
        wait "$r2s_pid" 2>/dev/null || true
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
            echo "114: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "114: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "114: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "114: harness: unexpected exit $rc" >&2
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
    SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_FILE SPAWN_BACKUP_KEY_DIR SPAWN_BACKUP_SEAL_KEY SPAWN_BACKUP_DRILL_DATABASE_URL SPAWN_BACKUP_R2_BUDGET_MB \
    SPAWN_BACKUP_R2_ALERT_MB SPAWN_BACKUP_R2_LIMIT_MB SPAWN_BACKUP_TRANSCRIPT_DAYS \
    SPAWN_BACKUP_PG_BIN SPAWN_BACKUP_DUMP_COMPRESS SPAWN_ENV_FILE SPAWN_BACKUP_DIR XDG_CONFIG_HOME \
    SPAWN_WORKSPACE_ROOT SPAWN_GIT_REPO; do
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
# The key directory, with k1 under its old name; k2 waits outside it for
# the switch.
KD=$WORK/keys
mkdir -m 700 "$KD"
od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$KD/backup.key"
od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$WORK/k2.new"
k1_pass=$(cat "$KD/backup.key")
k2_pass=$(cat "$WORK/k2.new")
r2_secret=$(od -An -N20 -tx1 /dev/urandom | tr -d ' \n')
printf '%s' "$r2_secret" >"$WORK/r2.secret"
access_key="bb114$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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

# Two mission sources: Spawn's, with an agent's branch holding its work,
# and another.
src_spawn=$WORK/spawn.git
src_other=$WORK/other.git
git init -q --initial-branch=main "$WORK/seed"
printf 'spawn\n' >"$WORK/seed/README.md"
mkdir -p "$WORK/seed/crates"
printf 'fn main() {}\n' >"$WORK/seed/crates/main.rs"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$src_spawn"
git -C "$WORK/seed" push -q "$src_spawn" main:main
git -C "$WORK/seed" checkout -q -b work
printf 'the agent work, checkpointed\n' >"$WORK/seed/work.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "spawn checkpoint exec_7"
git -C "$WORK/seed" push -q "$src_spawn" work:refs/heads/spawn/agent_worker_objective_7
git init -q --initial-branch=main "$WORK/seed2"
printf 'other\n' >"$WORK/seed2/README.md"
git -C "$WORK/seed2" add -A
git -C "$WORK/seed2" commit -qm other
git init -q --bare --initial-branch=main "$src_other"
git -C "$WORK/seed2" push -q "$src_other" main:main
# As the live missions/supply-pa.git was seeded (a mirror clone leaves its
# remote-tracking ref): not a branch, not the mission's history.
remote_ref=refs/remotes/github/main
git --git-dir="$src_other" update-ref "$remote_ref" "$(git -C "$WORK/seed2" rev-parse HEAD)"
# A third source with nothing in it yet (a mission added before its first
# push): no branches, no tags. `git bundle create` refuses to make an empty
# bundle, so the record has to say what there is by name, not a git error.
src_empty=$WORK/empty.git
git init -q --bare --initial-branch=main "$src_empty"
# A fourth source that will change in the increment in ways that need no
# bytes: a tag added at a commit the full already holds, and a branch
# removed. Its main and its scratch branch are in the full bundle.
src_tag=$WORK/tag.git
git init -q --bare --initial-branch=main "$src_tag"
git -C "$WORK/seed2" push -q "$src_tag" main:main
git --git-dir="$src_tag" update-ref refs/heads/scratch "$(git -C "$WORK/seed2" rev-parse HEAD)"

# The transcript archive, as the Bridge keeps it beside the workspaces.
SPAWN_WORKSPACE_ROOT=$WORK/ws
archive=$SPAWN_WORKSPACE_ROOT/.transcripts
mkdir -p "$archive/agent_supervisor" "$archive/agent_worker_objective_7"
awk 'BEGIN { for (i = 1; i <= 1500; i++)
    printf "{\"type\":\"assistant\",\"uuid\":\"s-%d\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"step %d: I read the fleet and the missions, and nothing needs a person\"}]}}\n", i, i }' \
    >"$archive/agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0001.jsonl"
awk 'BEGIN { for (i = 1; i <= 400; i++)
    printf "{\"type\":\"user\",\"uuid\":\"w-%d\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_%d\",\"content\":\"ok %d\"}]}}\n", i, i, i * 7 }' \
    >"$archive/agent_worker_objective_7/5f2a9e10-7c44-4b8e-8f00-2a4e9d3c0002.jsonl"

# A third agent's transcript, last written 20 days ago: outside the
# bundles' 14-day window (SPAWN_BACKUP_TRANSCRIPT_DAYS).
old_transcript=agent_worker_objective_3/3e8b1f20-4a6c-4d2e-9b70-6c1a2f8e0004.jsonl
mkdir -p "$archive/agent_worker_objective_3"
awk 'BEGIN { for (i = 1; i <= 50; i++)
    printf "{\"type\":\"user\",\"uuid\":\"o-%d\",\"message\":{\"role\":\"user\",\"content\":\"an old session, line %d\"}}\n", i, i }' \
    >"$archive/$old_transcript"
touch -d '20 days ago' "$archive/$old_transcript"

SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_R2_ENV_FILE=$WORK/r2.env
SPAWN_BACKUP_KEY_DIR=$KD
DRILL_URL="$server_url/postgres"
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_DIR

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

q() { scalar "$1"; }
now_ms() { echo $(($(date +%s%N) / 1000000)); }
# The backup records of a type, `<event id>|<record>`, oldest first.
records() {
    sql "select e.id || '|' || (e.kind -> 'detail' -> 'record')::text from spawn_events e
          where e.session_id = 'session_control' and e.event_name = 'control.recorded'
            and e.kind -> 'detail' -> 'record' ->> 'type' = '$1' order by e.id"
}
newest() { records "$1" | tail -n 1; }
count_records() { records "$1" | grep -c . || true; }
field() { printf '%s' "$1" | cut -d'|' -f2- | jq -r "$2"; }
event_of() { printf '%s' "$1" | cut -d'|' -f1; }
# The events of the source that are not backup records: what a drill must
# leave alone.
others() {
    q "select count(*) || ' ' || coalesce(max(id), 0) from spawn_events
        where coalesce(kind -> 'detail' -> 'record' ->> 'type', '') not like 'backup.%'"
}
events_now() { q "select count(*) || ' ' || coalesce(max(id), 0) from spawn_events"; }
add_missions() { # add_missions <from> <to>: control records by a real command
    i=$1
    while [ "$i" -le "$2" ]; do
        if [ $((i % 2)) -eq 0 ]; then src=$src_spawn; else src=$src_other; fi
        capture "$SPAWN_BIN" mission add --source "$src" \
            --text "114 mission $i: back the fleet's record up off this host, every hour, and prove it restores"
        [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
        i=$((i + 1))
    done
}
hexkey() { printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }
object() { echo "$R2/objects/$(hexkey "$1")"; }
has_object() { [ -f "$(object "$1").json" ]; }
meta() { jq -r --arg h "$2" '.meta[$h] // empty' "$(object "$1").json"; }
held() {
    set -- "$R2"/objects/*.json
    if [ -f "$1" ]; then jq -s 'map(.size) | add' "$@"; else echo 0; fi
}
log_lines() { wc -l <"$R2/requests.log" | tr -d ' '; }
requests_since() { tail -n "+$(($1 + 1))" "$R2/requests.log"; }
decrypt() { # decrypt <key file> <file> <out>
    gpg --batch --quiet --no-tty --pinentry-mode loopback --no-symkey-cache \
        --passphrase-file "$1" -o "$3" -d "$2" 2>"$WORK/gpg.err"
}
sha() { sha256sum <"$1" | cut -d' ' -f1; }
size() { wc -c <"$1" | tr -d ' '; }
# The events and the highest event id in a pg_dump archive.
dump_events() {
    pg_restore -a -t spawn_events -f - "$1" | awk '
        /^COPY [a-z_."]*spawn_events / {
            on = 1; col = 0
            line = $0; sub(/^[^(]*\(/, "", line); sub(/\).*$/, "", line)
            n = split(line, names, /, */)
            for (i = 1; i <= n; i++) if (names[i] == "id") col = i
            next
        }
        on && /^\\\.$/ { on = 0; next }
        on { rows++; split($0, f, "\t"); if (f[col] + 0 > max) max = f[col] + 0 }
        END { print rows + 0, max + 0 }'
}
# The run in a subshell whose environment holds the NAME=value settings it
# is given: never on a command line (`env VAR=value`), which every local
# user reads in /proc. The drill's database URL is one of them.
backup_env() { # backup_env <verb> [NAME=value ...]
    bv_verb=$1
    shift
    (
        for bv_setting in "$@"; do export "$bv_setting"; done
        exec "$SPAWN_BIN" backup "$bv_verb"
    )
}
backup() { # backup <verb> [NAME=value ...]: run `spawn backup <verb>`, timed
    verb=$1
    shift
    started=$(now_ms)
    capture backup_env "$verb" "$@"
    took=$(($(now_ms) - started))
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
}
secrets_leaked() { # where a secret shows: the output or the database
    for secret in "$k1_pass" "$k2_pass" "$r2_secret"; do
        if grep -qF -- "$secret" "$WORK/backup.log" 2>/dev/null; then echo "the command's output"; return; fi
        if [ "$(q "select count(*) from spawn_events where kind::text like '%$secret%'")" != 0 ] ||
            [ "$(q "select count(*) from spawn_commands where action::text like '%$secret%'")" != 0 ]; then
            echo "the database"
            return
        fi
    done
}
db_exists() { [ "$(psql -d postgres -tA -c "select count(*) from pg_database where datname = '$1'")" != 0 ]; }

# --- the source database ----------------------------------------------------------

add_missions 1 4
note() { echo "114: $*" >&2; }
rec_of() { printf '%s' "$1" | cut -d'|' -f2-; }
drill() { # drill <log> [NAME=value ...]: the restore drill into a fresh scratch parent
    dl_log=$1
    shift
    mkdir -p "$WORK/drills"
    capture drill_env "$@"
    cat "$WORK/out.txt" "$WORK/err.txt" >"$dl_log"
    cat "$dl_log" >>"$WORK/backup.log"
}
drill_env() {
    (
        for dl_setting in "$@"; do export "$dl_setting"; done
        exec "$SPAWN_BIN" backup restore-drill --into "$WORK/drills"
    )
}
# both_failed <check> <exact reason or empty> <substring or empty> <bundle key>
both_failed() {
    for kind in backup.drilled.transcripts backup.drilled.missions; do
        rec=$(newest "$kind")
        [ "$(field "$rec" '.ok')" = false ] || fail "$1 $kind is not failed: $(rec_of "$rec")"
        got=$(field "$rec" '.reason')
        if [ -n "$2" ] && [ "$got" != "$2" ]; then
            fail "$1 $kind's reason: expected [$2], got [$got]"
        fi
        if [ -n "$3" ]; then
            case $got in *"$3"*) ;; *) fail "$1 $kind's reason [$got] does not hold [$3]" ;; esac
        fi
        [ "$(field "$rec" '.bundle')" = "$4" ] || fail "$1 $kind names the bundle [$(field "$rec" '.bundle')], not [$4]"
    done
}
both_passed() { # both_passed <check> <bundle key>
    for kind in backup.drilled.transcripts backup.drilled.missions; do
        rec=$(newest "$kind")
        [ "$(field "$rec" '.ok')" = true ] && [ "$(field "$rec" '.bundle')" = "$2" ] ||
            fail "$1 $kind: expected ok true on $2, got $(rec_of "$rec")"
    done
}
sealed() { # sealed <check> <record> <key name>: the record and every object it stored say <key name>
    [ "$(field "$2" '.sealed_with')" = "$3" ] ||
        fail "$1 the $(field "$2" '.type') record's sealed_with is [$(field "$2" '.sealed_with')], not [$3]"
    for k in $(field "$2" '.keys[]'); do
        has_object "$k" || harness "$1 the record's key $k is not in the stand-in"
        [ "$(meta "$k" x-amz-meta-spawn-seal)" = "$3" ] ||
            fail "$1 the object $k carries x-amz-meta-spawn-seal [$(meta "$k" x-amz-meta-spawn-seal)], not [$3]"
    done
}
opens_only_with() { # opens_only_with <check> <record> <key file> <other key file>
    k=$(field "$2" '.keys[0]')
    decrypt "$3" "$(object "$k")" "$WORK/opened" ||
        fail "$1 $k does not decrypt with its key: $(head -c 200 "$WORK/gpg.err")"
    [ "$(sha "$WORK/opened")" = "$(field "$2" '.sha256')" ] || fail "$1 $k decrypts to another sha256 than its record's"
    rm -f "$WORK/opened"
    if decrypt "$4" "$(object "$k")" "$WORK/opened"; then
        fail "$1 $k also decrypts with the other key"
    fi
    rm -f "$WORK/opened"
}
k1_shown="$KD/backup.key.k1 (or backup.key)"

# The store's own stand-in: a second instance of the same R2 stand-in, its
# bucket a work bucket's name of this scenario's own (never a real bucket),
# and a store credential file naming it. Nothing here reads r2-store.env or
# r2.env, and no call leaves the host.
R2S=$WORK/r2s
mkdir -p "$R2S"
store_bucket=bb114-work
: >"$R2S/requests.log"
python3 -I "$WORK/r2.py" "$R2S" "$store_bucket" "$access_key" "$WORK/r2.secret" 2>"$R2S/err" &
r2s_pid=$!
tries=0
while [ ! -s "$R2S/port" ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 100 ] || harness "the store's R2 stand-in did not start: $(cat "$R2S/err")"
    kill -0 "$r2s_pid" 2>/dev/null || harness "the store's R2 stand-in exited: $(cat "$R2S/err")"
    sleep 0.1
done
umask 077
cat >"$WORK/r2-store.env" <<STOREENV
R2_ENDPOINT=http://127.0.0.1:$(cat "$R2S/port")
R2_ACCESS_KEY_ID=$access_key
R2_SECRET_ACCESS_KEY=$r2_secret
R2_BUCKETS=$store_bucket
STOREENV
umask 022
mkdir -p "$WORK/store-from"
printf 'one\n' >"$WORK/store-from/a.jsonl"
store_put() { # store_put <prefix> [NAME=value ...]: `spawn store put` into the store's stand-in
    sp_prefix=$1
    shift
    capture store_env "SPAWN_BACKUP_PREFIX=$sp_prefix" "$@"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
}
store_env() {
    (
        for sp_setting in "$@"; do export "$sp_setting"; done
        export SPAWN_STORE_ENV_FILE="$WORK/r2-store.env" SPAWN_STORE_BUCKET="$store_bucket"
        exec "$SPAWN_BIN" store put --area transcripts --from "$WORK/store-from"
    )
}
store_sealed() { # store_sealed <check> <prefix> <key name>: every store object under <prefix> says <key name>
    n=0
    for meta_json in "$R2S"/objects/*.json; do
        [ -f "$meta_json" ] || continue
        case $(jq -r .key "$meta_json") in "$2"*) ;; *) continue ;; esac
        n=$((n + 1))
        got=$(jq -r '.meta["x-amz-meta-spawn-seal"] // ""' "$meta_json")
        [ "$got" = "$3" ] || fail "$1 the store's object $(jq -r .key "$meta_json") carries x-amz-meta-spawn-seal [$got], not [$3]"
    done
    [ "$n" -ge 2 ] || harness "$1 the store put $n object(s) under $2, not a segment and a manifest"
}

# A value that is no key name, shaped as a secret (made up): never echoed.
not_a_name=bb114-made-up-key-content-5e1c7a9f04d2b863
rule="is not a backup key name (k and a number from 1, as k2)"

# --- (1) before the switch: k1 under its old name -----------------------------------
backup dump
[ "$status" -eq 0 ] || fail "(1) \`spawn backup dump\` before the switch exited $status: $(err | tail -n 2 | tr '\n' ' ')"
dump1=$(newest backup.dumped)
sealed "(1)" "$dump1" k1
backup bundle
[ "$status" -eq 0 ] || fail "(1) \`spawn backup bundle\` before the switch exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bun1=$(newest backup.bundled)
[ "$(field "$bun1" '.kind')" = full ] || harness "the first bundle is not full: $(rec_of "$bun1")"
sealed "(1)" "$bun1" k1
bkey1=$(field "$bun1" '.keys[0]')
store_put locktest/bb114k1/
[ "$status" -eq 0 ] || fail "(1) \`spawn store put\` before the switch exited $status: $(err | tail -n 2 | tr '\n' ' ')"
store_sealed "(1)" locktest/bb114k1/ k1
note "(1) checked: before the switch, a dump, a bundle and the store's objects say k1"

# --- (1b) k2 made, the switch not yet made --------------------------------------------
# The runbook's window (the lead, 13:2x): `backup.key` and `backup.key.k2`
# both on the host, SPAWN_BACKUP_SEAL_KEY unset, for as long as the switch
# waits on its proof. Hourly dumps and a daily bundle fall into it: they
# stay k1, and the k1 chain goes on.
cp "$WORK/k2.new" "$KD/backup.key.k2"
chmod 600 "$KD/backup.key.k2"
sleep 1.1
backup dump
[ "$status" -eq 0 ] || fail "(1b) \`spawn backup dump\` with backup.key and backup.key.k2, the seal key unset, exited $status: $(err | tail -n 2 | tr '\n' ' ')"
dump1b=$(newest backup.dumped)
sealed "(1b)" "$dump1b" k1
opens_only_with "(1b)" "$dump1b" "$KD/backup.key" "$KD/backup.key.k2"
sleep 1.1
backup bundle
[ "$status" -eq 0 ] || fail "(1b) \`spawn backup bundle\` with backup.key and backup.key.k2, the seal key unset, exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bun1b=$(newest backup.bundled)
got=$(rec_of "$bun1b" | jq -c '{kind, base_event_id}')
want="{\"kind\":\"incremental\",\"base_event_id\":$(event_of "$bun1")}"
[ "$got" = "$want" ] || fail "(1b) the bundle with backup.key.k2 present, the seal key unset: expected $want (the k1 chain goes on), got $got"
sealed "(1b)" "$bun1b" k1
# The k1 chain's newest object, the one verify and the drill read from now.
bkey1=$(field "$bun1b" '.keys[0]')
backup verify "SPAWN_BACKUP_DRILL_DATABASE_URL=$DRILL_URL"
[ "$status" -eq 0 ] || fail "(1b) verify with backup.key and backup.key.k2, the seal key unset, exited $status: $(err | tail -n 2 | tr '\n' ' ')"
ver=$(newest backup.verified)
got=$(rec_of "$ver" | jq -c '{ok, sealed_with, bundle: {ok: .bundle.ok, sealed_with: .bundle.sealed_with}}')
want='{"ok":true,"sealed_with":"k1","bundle":{"ok":true,"sealed_with":"k1"}}'
[ "$got" = "$want" ] || fail "(1b) the verify record: expected $want, got $got"
note "(1b) checked: with k2 made and the seal key unset, a dump, a bundle and verify stay k1"

# --- (2) the switch -----------------------------------------------------------------------
mv "$KD/backup.key" "$KD/backup.key.k1"
sleep 1.1
backup dump SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(2) the first dump with SPAWN_BACKUP_SEAL_KEY=k2 exited $status: $(err | tail -n 2 | tr '\n' ' ')"
dump2=$(newest backup.dumped)
sealed "(2)" "$dump2" k2
opens_only_with "(2)" "$dump2" "$KD/backup.key.k2" "$KD/backup.key.k1"
store_put locktest/bb114k2/ SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(2) \`spawn store put\` with SPAWN_BACKUP_SEAL_KEY=k2 exited $status: $(err | tail -n 2 | tr '\n' ' ')"
store_sealed "(2)" locktest/bb114k2/ k2
# One of them opened (rev-g): the newest manifest decrypts with k2 to a
# manifest, and not with k1.
man_meta=$(grep -l '"key": *"locktest/bb114k2/transcripts/manifests/' "$R2S"/objects/*.json | head -n 1)
[ -n "$man_meta" ] || harness "(2) no store manifest under locktest/bb114k2/"
man_obj=${man_meta%.json}
decrypt "$KD/backup.key.k2" "$man_obj" "$WORK/manifest.json" ||
    fail "(2) the store's k2 manifest does not decrypt with k2: $(head -c 200 "$WORK/gpg.err")"
[ "$(jq -r '.version' "$WORK/manifest.json" 2>/dev/null)" = 2 ] || fail "(2) the store's k2 manifest, decrypted with k2, is no manifest"
rm -f "$WORK/manifest.json"
if decrypt "$KD/backup.key.k1" "$man_obj" "$WORK/manifest.json"; then
    fail "(2) the store's k2 manifest also decrypts with k1"
fi
rm -f "$WORK/manifest.json"
note "(2) checked: after the switch, the dump says k2 and opens with k2 only; the store's objects say k2"

# --- (3) verify reads each half with its own key ------------------------------------------
backup verify SPAWN_BACKUP_SEAL_KEY=k2 "SPAWN_BACKUP_DRILL_DATABASE_URL=$DRILL_URL"
[ "$status" -eq 0 ] || fail "(3) verify between the switch and the first k2 bundle exited $status: $(err | tail -n 2 | tr '\n' ' ')"
ver=$(newest backup.verified)
got=$(rec_of "$ver" | jq -c '{ok, sealed_with, bundle: {ok: .bundle.ok, sealed_with: .bundle.sealed_with}}')
want='{"ok":true,"sealed_with":"k2","bundle":{"ok":true,"sealed_with":"k1"}}'
[ "$got" = "$want" ] || fail "(3) the verify record: expected $want, got $got"
# Each half checks its object's header against its record (rev-e's F2):
# the dump's header made k1 under its k2 record, then the bundle's k2 under
# its k1 record.
header_swapped() { # header_swapped <check> <object key> <header value> <expected reason>
    hs_file="$(object "$2").json"
    cp "$hs_file" "$WORK/meta.saved"
    jq -c --arg v "$3" '.meta["x-amz-meta-spawn-seal"] = $v' "$WORK/meta.saved" >"$hs_file"
    backup verify SPAWN_BACKUP_SEAL_KEY=k2 "SPAWN_BACKUP_DRILL_DATABASE_URL=$DRILL_URL"
    hs_status=$status
    cp "$WORK/meta.saved" "$hs_file"
    [ "$hs_status" -eq 1 ] || fail "$1 verify with $2's header [$3] exited $hs_status, not 1"
    ver=$(newest backup.verified)
    [ "$(field "$ver" '.ok')" = false ] && [ "$(field "$ver" '.reason')" = "$4" ] ||
        fail "$1 verify with $2's header [$3]: expected ok false, reason [$4], got $(rec_of "$ver" | jq -c '{ok, reason}')"
}
header_swapped "(3)" "$(field "$dump2" '.keys[0]')" k1 "record says k2, object says k1"
header_swapped "(3)" "$bkey1" k2 "record says k1, object says k2"
# (12): a header that is no key name is refused by where it is.
dkey2=$(field "$dump2" '.keys[0]')
header_swapped "(12)" "$dkey2" "$not_a_name" "the x-amz-meta-spawn-seal of $dkey2 $rule"
note "(3) checked: verify reads the k2 dump and the k1 bundle, each checked against its header"

# --- (4) k1's file away --------------------------------------------------------------------
mv "$KD/backup.key.k1" "$WORK/aside.k1"
backup verify SPAWN_BACKUP_SEAL_KEY=k2 "SPAWN_BACKUP_DRILL_DATABASE_URL=$DRILL_URL"
[ "$status" -eq 1 ] || fail "(4) verify with k1's file away exited $status, not 1: $(err | tail -n 2 | tr '\n' ' ')"
ver=$(newest backup.verified)
want="the bundle is sealed with k1; its key file $k1_shown is not on this host"
[ "$(field "$ver" '.ok')" = false ] && [ "$(field "$ver" '.reason')" = "$want" ] ||
    fail "(4) the verify record with k1's file away: expected ok false, reason [$want], got $(rec_of "$ver" | jq -c '{ok, reason}')"
drill "$WORK/drill4.log" SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 2 ] || fail "(4) the drill of the k1 chain with k1's file away exited $status, not 2: $(tail -n 3 "$WORK/drill4.log" | tr '\n' ' ')"
both_failed "(4)" "the bundle chain is sealed with k1; its key file $k1_shown is not on this host" "" "$bkey1"
mv "$WORK/aside.k1" "$KD/backup.key.k1"
# An object with no header reads as k1 (rev-e's R3-1): today's bundles have
# none. The k1 chain's object without its header, its record still k1: the
# drill restores it.
k1_meta="$(object "$bkey1").json"
cp "$k1_meta" "$WORK/meta.saved"
jq -c 'del(.meta["x-amz-meta-spawn-seal"])' "$WORK/meta.saved" >"$k1_meta"
drill "$WORK/drill4b.log"
k1_status=$status
cp "$WORK/meta.saved" "$k1_meta"
[ "$k1_status" -eq 0 ] || fail "(4) the drill of the k1 chain whose object has no header exited $k1_status, not 0: $(tail -n 3 "$WORK/drill4b.log" | tr '\n' ' ')"
both_passed "(4)" "$bkey1"
note "(4) checked: a missing k1 is named by verify and the drill; an object with no header reads as k1"

# --- (5) the first bundle under k2 is full ----------------------------------------------
sleep 1.1
backup bundle SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(5) the first bundle under k2 exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bun2=$(newest backup.bundled)
got=$(rec_of "$bun2" | jq -c '{kind, base_event_id}')
[ "$got" = '{"kind":"full","base_event_id":null}' ] ||
    fail "(5) the first bundle under k2, with a full k1 bundle this week: expected {\"kind\":\"full\",\"base_event_id\":null}, got $got"
sealed "(5)" "$bun2" k2
opens_only_with "(5)" "$bun2" "$KD/backup.key.k2" "$KD/backup.key.k1"
sleep 1.1
backup bundle SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(5) the second bundle under k2 exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bun3=$(newest backup.bundled)
got=$(rec_of "$bun3" | jq -c '{kind, base_event_id}')
want="{\"kind\":\"incremental\",\"base_event_id\":$(event_of "$bun2")}"
[ "$got" = "$want" ] || fail "(5) the second bundle under k2: expected $want, got $got"
sealed "(5)" "$bun3" k2
bkey3=$(field "$bun3" '.keys[0]')
note "(5) checked: the first k2 bundle is full, the next an increment on it"

# --- (6) a k2-only host, the seal key unset, drills the k2 chain ----------------------------
mv "$KD/backup.key.k1" "$WORK/aside.k1"
drill "$WORK/drill6.log"
[ "$status" -eq 0 ] || fail "(6) the drill of the k2 chain on a k2-only host exited $status: $(tail -n 3 "$WORK/drill6.log" | tr '\n' ' ')"
both_passed "(6)" "$bkey3"
mv "$WORK/aside.k1" "$KD/backup.key.k1"
note "(6) checked: a k2-only host drills the k2 chain"

# --- (7) a k1-only host, the newest chain k2 ---------------------------------------------
mv "$KD/backup.key.k2" "$WORK/aside.k2"
drill "$WORK/drill7.log"
[ "$status" -eq 2 ] || fail "(7) the drill of the k2 chain on a k1-only host exited $status, not 2: $(tail -n 3 "$WORK/drill7.log" | tr '\n' ' ')"
both_failed "(7)" "the bundle chain is sealed with k2; its key file $KD/backup.key.k2 is not on this host" "" "$bkey3"
mv "$WORK/aside.k2" "$KD/backup.key.k2"
note "(7) checked: a missing k2 is named by the drill"

# --- (8) the record and the object disagree ---------------------------------------------
meta_file="$(object "$bkey3").json"
cp "$meta_file" "$WORK/meta.saved"
jq -c '.meta["x-amz-meta-spawn-seal"] = "k1"' "$WORK/meta.saved" >"$meta_file"
drill "$WORK/drill8.log"
[ "$status" -eq 1 ] || fail "(8) the drill with the object saying k1 and its record k2 exited $status, not 1: $(tail -n 3 "$WORK/drill8.log" | tr '\n' ' ')"
both_failed "(8)" "" "record says k2, object says k1" "$bkey3"
cp "$WORK/meta.saved" "$meta_file"
note "(8) checked: a record and an object that disagree are refused"

# --- (9) a sealing key with no name or no file --------------------------------------------
for bad in "$not_a_name" k3; do
    before=$(log_lines)
    n_before=$(count_records backup.dumped)
    backup dump "SPAWN_BACKUP_SEAL_KEY=$bad"
    case $bad in k3) shown=k3 says=backup.key.k3 ;; *) shown="a value that is no key name" says="SPAWN_BACKUP_SEAL_KEY $rule" ;; esac
    [ "$status" -eq 2 ] || fail "(9) a dump with SPAWN_BACKUP_SEAL_KEY=$shown exited $status, not 2"
    ! requests_since "$before" | grep -q '^PUT ' || fail "(9) a dump with SPAWN_BACKUP_SEAL_KEY=$shown stored: $(requests_since "$before" | grep '^PUT ' | head -n 2 | tr '\n' ' ')"
    [ "$(count_records backup.dumped)" -eq $((n_before + 1)) ] || fail "(9) a dump with SPAWN_BACKUP_SEAL_KEY=$shown recorded no refusal"
    ref=$(newest backup.dumped)
    got=$(field "$ref" '[(.keys | length), (.skipped[0].reason // "")] | @tsv' | sed "s/$not_a_name/<the value>/g")
    case $got in "0	"*"$says"*) ;; *) fail "(9) the refusal with SPAWN_BACKUP_SEAL_KEY=$shown: expected no key and a reason naming [$says], got [$got]" ;; esac
done
note "(9) checked: a seal key value that is no name, or a name with no file, refuses and never falls back"

# --- (12) the header: a value that is no name, never echoed --------------------------------
# Before (10) plants its record, so the drill reads bun3's chain.
meta_file="$(object "$bkey3").json"
cp "$meta_file" "$WORK/meta.saved"
jq -c --arg v "$not_a_name" '.meta["x-amz-meta-spawn-seal"] = $v' "$WORK/meta.saved" >"$meta_file"
drill "$WORK/drill12.log"
d12=$status
cp "$WORK/meta.saved" "$meta_file"
[ "$d12" -eq 1 ] || fail "(12) the drill with the bundle's header no name exited $d12, not 1: $(tail -n 3 "$WORK/drill12.log" | sed "s/$not_a_name/<the value>/g" | tr '\n' ' ')"
both_failed "(12)" "" "the x-amz-meta-spawn-seal of $bkey3 is not a backup key name" "$bkey3"
# --- (10) a record whose key name is no name ---------------------------------------------------
forged=$(rec_of "$bun3" | jq -c --arg key "${bkey3%.gpg}-114-forged.gpg" --arg bad "$not_a_name" '.sealed_with = $bad | .keys = [$key]')
forged_event=$(sql "begin;
    select pg_advisory_xact_lock(1399957401, hashtext('session_control'));
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms, commit_xid)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', '$forged'::jsonb)),
            (select coalesce(max(session_seq), 0) + 1 from spawn_events where session_id = 'session_control'),
            (floor(extract(epoch from clock_timestamp()) * 1000))::bigint,
            pg_current_xact_id())
    returning id;
    commit;" | grep -E '^[0-9]+$' | head -n 1)
case "$forged_event" in '' | *[!0-9]*) harness "the forged record was not inserted: [$forged_event]" ;; esac
drill "$WORK/drill10.log"
[ "$status" -eq 2 ] || fail "(10) the drill of a record whose sealed_with is no name exited $status, not 2: $(tail -n 3 "$WORK/drill10.log" | sed "s/$not_a_name/<the value>/g" | tr '\n' ' ')"
both_failed "(10)" "the sealed_with of backup.bundled record $forged_event $rule" "" "${bkey3%.gpg}-114-forged.gpg"
note "(10) checked: a key name that is no name is refused by its record, never echoed"

# --- (11) no secret ------------------------------------------------------------------------
leak=$(secrets_leaked)
[ -z "$leak" ] || fail "(11) a secret (a key's passphrase or the R2 secret key) shows in $leak"
note "(11) checked: no secret in a record or the output"

# --- (12) nowhere echoed: every run's output, and every record but the one
# (10) planted.
if grep -qF -- "$not_a_name" "$WORK/backup.log"; then
    fail "(12) a value that is no key name is echoed in the output: $(grep -F -- "$not_a_name" "$WORK/backup.log" | head -n 1 | sed "s/$not_a_name/<the value>/g" | cut -c1-200)"
fi
in_records=$(q "select count(*) from spawn_events where id <> $forged_event and kind::text like '%$not_a_name%'")
in_commands=$(q "select count(*) from spawn_commands where action::text like '%$not_a_name%'")
[ "$in_records" = 0 ] && [ "$in_commands" = 0 ] ||
    fail "(12) a value that is no key name is copied into $in_records record(s) and $in_commands command(s)"
note "(12) checked: a value that is no key name is refused by where it is, and never echoed"

echo "114: dumps and bundles say the key that sealed them; verify and the drill read each with its own"
exit 0
