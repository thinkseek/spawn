#!/bin/sh
# Objective 116 [recoverability], two backup keys, part B2: `spawn store`
# opens the newest manifest with the key its object names
# (`x-amz-meta-spawn-seal`, none: k1). After a manifest of another key it
# puts every file whole under the sealing key and never names one of that
# manifest's segments again. Without the other key's file it refuses rather
# than start a fresh chain. Its record says `sealed_with`.
# (sync/design-backup-two-keys.md [R3], (6) N5; the lead's split of B,
# 11-13; after 114 (the header on every store PUT) and 115 (the record's
# field).)
#
# Black box against 104's local R2 stand-in ONLY: a second instance whose
# bucket is this scenario's own work bucket (bb116-work), under locktest/
# prefixes. The scenario reads neither r2-store.env nor r2.env, makes no
# call off the host, never names a real bucket and never names a keep
# bucket. Its own database, scratch HOME, GNUPGHOME and key directory
# (SPAWN_BACKUP_KEY_DIR); made-up keys, k1 under its old name at the start.
#   (1) Before the switch: `spawn store put` under two prefixes (A and B)
#       exits 0. Each record says "sealed_with": "k1".
#   (2) After the switch (backup.key renamed backup.key.k1, backup.key.k2
#       made, SPAWN_BACKUP_SEAL_KEY=k2), `spawn store plan` under A says
#       every file whole and all its bytes to send; a put under A exits 0. Its record
#       says sealed_with k2, every one of its files whole (whole = files,
#       unchanged 0, grown 0, segments_found 0), and its manifest's
#       generation is 2. No segment the k2 manifest names is a segment of A's
#       k1 manifest. Every object it wrote says k2. A second k2 put, nothing
#       changed, is as before two keys: unchanged 2, whole 0, segments_written
#       0, generation 3.
#   (3) With k1's file away, `spawn store get` (SPAWN_BACKUP_SEAL_KEY=k2)
#       restores a file of A's k2 manifest, byte for byte, and `spawn store
#       verify` exits 0.
#   (4) Still with k1's file away, a put under B (whose last manifest is
#       k1's) exits 2 and says "the last manifest is sealed with k1; its key
#       file <dir>/backup.key.k1 (or backup.key) is not on this host". It
#       writes no object.
#   (4b) With k1 back and SPAWN_BACKUP_SEAL_KEY=k2, B's newest manifest
#       (k1's) is read with k1: `spawn store get` restores a file of it byte
#       for byte, `spawn store verify` exits 0, `spawn store list` names it.
#   (5) With k1 back and B's manifest stripped of its header (as every store
#       object from before 114 is: no header means k1), a put under B with
#       SPAWN_BACKUP_SEAL_KEY=k2 exits 0: generation 2, every file whole,
#       record sealed_with k2.
#   (6) No key's passphrase and no R2 secret is in any output or record.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "116: harness: $*" >&2
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
SPAWN_DB_NAME="bb_116_store_$run"
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
    if [ "$rc" -ne 0 ]; then
        if [ -s "$R2/requests.log" ]; then
            echo "116: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "116: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "116: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "116: harness: unexpected exit $rc" >&2
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
access_key="bb116$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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
            --text "116 mission $i: back the fleet's record up off this host, every hour, and prove it restores"
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

add_missions 1 2
note() { echo "116: $*" >&2; }
rec_of() { printf '%s' "$1" | cut -d'|' -f2-; }
k1_shown="$KD/backup.key.k1 (or backup.key)"

# The store's own stand-in (its bucket the scenario's own work bucket).
R2S=$WORK/r2s
mkdir -p "$R2S"
store_bucket=bb116-work
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
FROM=$WORK/store-from
mkdir -p "$FROM"
awk 'BEGIN { for (i = 1; i <= 300; i++) printf "{\"n\":%d,\"text\":\"a line of the first file\"}\n", i }' >"$FROM/a.jsonl"
awk 'BEGIN { for (i = 1; i <= 200; i++) printf "{\"n\":%d,\"text\":\"a line of the second file\"}\n", i }' >"$FROM/b.jsonl"
A=locktest/bb116a/
B=locktest/bb116b/

store_cmd() { # store_cmd <prefix> <verb> [args...] -- [NAME=value ...]
    sc_prefix=$1
    shift
    capture store_env "$sc_prefix" "$@"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
}
store_env() {
    (
        se_prefix=$1
        shift
        se_args=""
        while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
            se_args="$se_args $1"
            shift
        done
        [ "$#" -eq 0 ] || shift
        for se_setting in "$@"; do export "$se_setting"; done
        export SPAWN_STORE_ENV_FILE="$WORK/r2-store.env" SPAWN_STORE_BUCKET="$store_bucket" \
            SPAWN_BACKUP_PREFIX="$se_prefix"
        # shellcheck disable=SC2086
        exec "$SPAWN_BIN" store $se_args
    )
}
put() { # put <prefix> [NAME=value ...]
    pt_prefix=$1
    shift
    store_cmd "$pt_prefix" put --area transcripts --from "$FROM" -- "$@"
}
stored() { newest backup.stored; }
# The newest manifest object under <prefix>: its stand-in metadata file.
manifest_meta() {
    for mm in "$R2S"/objects/*.json; do
        [ -f "$mm" ] || continue
        k=$(jq -r .key "$mm")
        case $k in "$1"transcripts/manifests/*) printf '%s\t%s\n' "$k" "$mm" ;; esac
    done | sort | tail -n 1 | cut -f2
}
# open_manifest <meta file> <key file> <out>: the manifest decrypted
open_manifest() { decrypt "$2" "${1%.json}" "$3"; }
seal_of() { jq -r '.meta["x-amz-meta-spawn-seal"] // ""' "$1"; }
segment_meta() { # segment_meta <prefix> <name>
    sg_head=$(printf '%s' "$2" | cut -c1-2)
    echo "$R2S/objects/$(hexkey "$1transcripts/segments/$sg_head/$2").json"
}

# --- (1) before the switch: two prefixes, k1 -------------------------------------------
for p in "$A" "$B"; do
    put "$p"
    [ "$status" -eq 0 ] || fail "(1) \`spawn store put\` under $p before the switch exited $status: $(err | tail -n 2 | tr '\n' ' ')"
    rec=$(stored)
    [ "$(field "$rec" '.sealed_with')" = k1 ] || fail "(1) the backup.stored record under $p says sealed_with [$(field "$rec" '.sealed_with')], not [k1]"
done
a1_meta=$(manifest_meta "$A")
[ -n "$a1_meta" ] || harness "(1) no manifest under $A"
open_manifest "$a1_meta" "$KD/backup.key" "$WORK/a1.json" || harness "(1) A's k1 manifest does not open with k1: $(head -c 200 "$WORK/gpg.err")"
note "(1) checked: before the switch, the store's records say k1"

# --- (2) the switch: put whole under k2 after A's k1 manifest ---------------------------
mv "$KD/backup.key" "$KD/backup.key.k1"
mv "$WORK/k2.new" "$KD/backup.key.k2"
# What put will do, as plan says it first (rev-e's F1): every file whole.
store_cmd "$A" plan --area transcripts --from "$FROM" -- SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(2) \`spawn store plan\` under $A with SPAWN_BACKUP_SEAL_KEY=k2 exited $status: $(err | tail -n 2 | tr '\n' ' ')"
total=$(( $(wc -c <"$FROM/a.jsonl") + $(wc -c <"$FROM/b.jsonl") ))
want="2 file(s): 0 unchanged, 0 grown, 2 whole; $total bytes, $total of them to send"
grep -qF -- "$want" "$WORK/out.txt" || fail "(2) the plan before the first k2 put does not say [$want]: $(grep -m1 'file(s)' "$WORK/out.txt")"
put "$A" SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(2) \`spawn store put\` under $A with SPAWN_BACKUP_SEAL_KEY=k2 after a k1 manifest exited $status: $(err | tail -n 2 | tr '\n' ' ')"
rec=$(stored)
got=$(rec_of "$rec" | jq -c '{sealed_with, files, whole, unchanged, grown, segments_found}')
want='{"sealed_with":"k2","files":2,"whole":2,"unchanged":0,"grown":0,"segments_found":0}'
[ "$got" = "$want" ] || fail "(2) the record of the first k2 put after a k1 manifest: expected $want, got $got"
a2_meta=$(manifest_meta "$A")
[ "$a2_meta" != "$a1_meta" ] || fail "(2) no new manifest under $A"
[ "$(seal_of "$a2_meta")" = k2 ] || fail "(2) A's new manifest carries x-amz-meta-spawn-seal [$(seal_of "$a2_meta")], not [k2]"
open_manifest "$a2_meta" "$KD/backup.key.k2" "$WORK/a2.json" || fail "(2) A's new manifest does not open with k2: $(head -c 200 "$WORK/gpg.err")"
[ "$(jq -r .generation "$WORK/a2.json")" = 2 ] || fail "(2) A's k2 manifest is of generation [$(jq -r .generation "$WORK/a2.json")], not 2"
jq -r '.files[].segments[].name' "$WORK/a1.json" | sort -u >"$WORK/a1.names"
jq -r '.files[].segments[].name' "$WORK/a2.json" | sort -u >"$WORK/a2.names"
shared=$(comm -12 "$WORK/a1.names" "$WORK/a2.names" | grep -c . || true)
[ "$shared" = 0 ] || fail "(2) A's k2 manifest names $shared segment(s) of its k1 manifest"
[ -s "$WORK/a2.names" ] || harness "(2) A's k2 manifest names no segment"
while read -r name; do
    sm=$(segment_meta "$A" "$name")
    [ -f "$sm" ] || fail "(2) the k2 manifest's segment $name is not in the stand-in"
    [ "$(seal_of "$sm")" = k2 ] || fail "(2) the k2 segment $name carries x-amz-meta-spawn-seal [$(seal_of "$sm")], not [k2]"
done <"$WORK/a2.names"
# The same key's manifest after it: as before two keys (rev-e's F2).
put "$A" SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(2) a second k2 put under $A exited $status: $(err | tail -n 2 | tr '\n' ' ')"
got=$(rec_of "$(stored)" | jq -c '{sealed_with, files, unchanged, whole, segments_written}')
want='{"sealed_with":"k2","files":2,"unchanged":2,"whole":0,"segments_written":0}'
[ "$got" = "$want" ] || fail "(2) the second k2 put, nothing changed: expected $want, got $got"
a3_meta=$(manifest_meta "$A")
open_manifest "$a3_meta" "$KD/backup.key.k2" "$WORK/a3.json" || fail "(2) A's third manifest does not open with k2"
[ "$(jq -r .generation "$WORK/a3.json")" = 3 ] || fail "(2) A's third manifest is of generation [$(jq -r .generation "$WORK/a3.json")], not 3"
note "(2) checked: after a k1 manifest, the k2 plan and put are whole, of generation 2, and share no segment; the next k2 put reuses"

# --- (3) get and verify of the k2 manifest, k1's file away ------------------------------------
mv "$KD/backup.key.k1" "$WORK/aside.k1"
mkdir -p "$WORK/got"
store_cmd "$A" get --area transcripts --path a.jsonl --into "$WORK/got" -- SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(3) \`spawn store get\` of A's k2 manifest with k1's file away exited $status: $(err | tail -n 2 | tr '\n' ' ')"
cmp -s "$WORK/got/a.jsonl" "$FROM/a.jsonl" || fail "(3) the file \`spawn store get\` restored is not the original"
store_cmd "$A" verify --area transcripts -- SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(3) \`spawn store verify\` of A's k2 manifest exited $status: $(err | tail -n 2 | tr '\n' ' ')"
note "(3) checked: get and verify read the k2 manifest without k1"

# --- (4) B's last manifest is k1's, k1's file away: refused -----------------------------------
before=$(wc -l <"$R2S/requests.log" | tr -d ' ')
n_before=$(count_records backup.stored)
put "$B" SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 2 ] || fail "(4) a k2 put under $B (its last manifest k1's) with k1's file away exited $status, not 2"
want="the last manifest is sealed with k1; its key file $k1_shown is not on this host"
grep -qF -- "$want" "$WORK/err.txt" || fail "(4) the refusal does not say [$want]: $(err | tail -n 2 | tr '\n' ' ')"
! tail -n "+$((before + 1))" "$R2S/requests.log" | grep -q '^PUT ' || fail "(4) the refused put wrote: $(tail -n "+$((before + 1))" "$R2S/requests.log" | grep '^PUT ' | head -n 2 | tr '\n' ' ')"
[ "$(count_records backup.stored)" = "$n_before" ] || fail "(4) the refused put recorded a backup.stored"
mv "$WORK/aside.k1" "$KD/backup.key.k1"
note "(4) checked: without the last manifest's key, a put refuses and writes nothing"

# --- (4b) B's k1 manifest read with its own key while k2 seals (rev-e's F3) ----------------
mkdir -p "$WORK/got-b"
store_cmd "$B" get --area transcripts --path b.jsonl --into "$WORK/got-b" -- SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(4b) \`spawn store get\` of B's k1 manifest with SPAWN_BACKUP_SEAL_KEY=k2 exited $status: $(err | tail -n 2 | tr '\n' ' ')"
cmp -s "$WORK/got-b/b.jsonl" "$FROM/b.jsonl" || fail "(4b) the file restored from B's k1 manifest is not the original"
store_cmd "$B" verify --area transcripts -- SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(4b) \`spawn store verify\` of B's k1 manifest exited $status: $(err | tail -n 2 | tr '\n' ' ')"
store_cmd "$B" list --area transcripts -- SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] && grep -q '^b.jsonl ' "$WORK/out.txt" ||
    fail "(4b) \`spawn store list\` of B's k1 manifest (exit $status) does not name b.jsonl: $(head -c 200 "$WORK/out.txt")"
note "(4b) checked: get, verify and list read another key's newest manifest with its key"

# --- (5) a last manifest with no header is k1's ------------------------------------------------
b1_meta=$(manifest_meta "$B")
[ -n "$b1_meta" ] || harness "(5) no manifest under $B"
jq -c 'del(.meta["x-amz-meta-spawn-seal"])' "$b1_meta" >"$WORK/b1.meta" && cp "$WORK/b1.meta" "$b1_meta"
put "$B" SPAWN_BACKUP_SEAL_KEY=k2
[ "$status" -eq 0 ] || fail "(5) a k2 put under $B after a manifest with no header exited $status: $(err | tail -n 2 | tr '\n' ' ')"
rec=$(stored)
got=$(rec_of "$rec" | jq -c '{sealed_with, files, whole, unchanged}')
want='{"sealed_with":"k2","files":2,"whole":2,"unchanged":0}'
[ "$got" = "$want" ] || fail "(5) the record of the k2 put after a headerless manifest: expected $want, got $got"
b2_meta=$(manifest_meta "$B")
open_manifest "$b2_meta" "$KD/backup.key.k2" "$WORK/b2.json" || fail "(5) B's new manifest does not open with k2"
[ "$(jq -r .generation "$WORK/b2.json")" = 2 ] || fail "(5) B's k2 manifest is of generation [$(jq -r .generation "$WORK/b2.json")], not 2"
note "(5) checked: a manifest with no header is read as k1's"

# --- (6) no secret ------------------------------------------------------------------------------
leak=$(secrets_leaked)
[ -z "$leak" ] || fail "(6) a secret (a key's passphrase or the R2 secret key) shows in $leak"
note "(6) checked: no secret in a record or the output"

echo "116: the store reads each manifest with its own key and starts whole under a new one"
exit 0
