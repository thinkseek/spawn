#!/bin/sh
# Objective 136 [recoverability]: backup.rs signs correctly with curl 7.88 and
# newer (lead-mac 23:03, PR #59 taken in through the gate). SigV4 signs the
# query sorted by name; a curl that signs it as written (7.88, Debian 12's)
# signed the listings' later pages wrong, their continuation-token last. And
# curl 7.88 sends no x-amz-content-sha256, which R2 requires. Now every
# bucket request names its payload hash (UNSIGNED-PAYLOAD), and both
# listings send their parameters in sorted order.
#
# Black box, on 116's world (its own database, scratch HOME, GNUPGHOME and
# key directory, made-up keys and R2 credentials), against 104's local R2
# stand-in made strict as R2 is: a request without x-amz-content-sha256 is
# 400, and a PUT with If-None-Match: * over an existing key is 412. The
# stand-in pages at 4 keys and logs each listing's parameter names in the
# order sent. A `curl` first on PATH logs, for each call, the names of the
# headers Spawn's config gives it (never a value, never the user line; its
# log under WORK, mode 600) and runs the real curl with the same config.
#   (1) `spawn store put` six times under one prefix, then `spawn store
#       list`: each exits 0; a manifest listing goes past its first page;
#       every listing sends its parameters sorted by name.
#   (2) `spawn backup dump` with five other objects in the bucket: exits 0;
#       its whole-bucket listing goes past its first page, sorted likewise.
#   (3) Every curl call names x-amz-content-sha256, the conditional PUTs
#       (If-None-Match) too, and each such PUT is answered 200.
#   (4) No secret in any output, record or the curl log.
#   (5) deploy/backup/lib.sh (the owner's part: deploy/ is protected), sourced
#       from the tree under test: its list_objects against the strict
#       stand-in exits 0, names the dump's object, and its curl call names
#       x-amz-content-sha256.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "136: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql pg_dump pg_restore gpg git jq od sha256sum md5sum stat bash paste python3 tar gzip awk sed cmp; do
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
SPAWN_DB_NAME="bb_136_sign_$run"
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
            echo "136: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "136: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "136: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "136: harness: unexpected exit $rc" >&2
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
access_key="bb136$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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
max-keys), with continuation tokens, logging its parameters' names in the
order sent. As R2: a request without x-amz-content-sha256 is 400, and a
PUT with If-None-Match: * over an existing key is 412 PreconditionFailed.
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
        if self.headers.get("x-amz-content-sha256") is None:
            # As R2 answers an s3 request without its payload hash (136).
            return self.error(400, "InvalidRequest", shown, "missing x-amz-content-sha256")
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
            if os.path.exists(file) and self.headers.get("If-None-Match") == "*":
                return self.error(412, "PreconditionFailed", key)
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
        # The parameters' names as sent, in their order (136).
        order = ",".join(name for name, _ in urllib.parse.parse_qsl(self.path.partition("?")[2], keep_blank_values=True))
        self.reply(200, "".join(parts).encode(), [("Content-Type", "application/xml")], shown, "order=" + order)

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
            --text "136 mission $i: back the fleet's record up off this host, every hour, and prove it restores"
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
note() { echo "136: $*" >&2; }
rec_of() { printf '%s' "$1" | cut -d'|' -f2-; }
k1_shown="$KD/backup.key.k1 (or backup.key)"

# The store's own stand-in (its bucket the scenario's own work bucket).
R2S=$WORK/r2s
mkdir -p "$R2S"
store_bucket=bb136-work
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
A=locktest/bb136a/
B=locktest/bb136b/

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

# --- the logging curl ----------------------------------------------------------------
# For each call: "call", then "header <name>" for each header line of the
# config Spawn hands it on stdin. Names only: no value and no user line is
# written. The config sits in WORK (mode 600) for the call and goes.
real_curl=$(command -v curl) || harness "curl is not installed"
mkdir -p "$WORK/bin"
CURL_LOG=$WORK/curl.log
umask 077
: >"$CURL_LOG"
cat >"$WORK/bin/curl" <<CURL
#!/bin/sh
umask 077
cfg=\$(mktemp "$WORK/curl-config.XXXXXX") || exit 99
cat >"\$cfg"
{ echo call; sed -n 's/^header = "\\([^:]*\\):.*/header \\1/p' "\$cfg"; } >>"$CURL_LOG"
prev=""
for arg in "\$@"; do
    case \$prev in -H | --header) printf 'argheader %s\\n' "\${arg%%:*}" >>"$CURL_LOG" ;; esac
    prev=\$arg
done
"$real_curl" "\$@" <"\$cfg"
rc=\$?
rm -f "\$cfg"
exit \$rc
CURL
chmod 700 "$WORK/bin/curl"
umask 022
PATH=$WORK/bin:$PATH
export PATH
[ "$(command -v curl)" = "$WORK/bin/curl" ] || harness "the logging curl is not first on PATH"

# listings <requests.log>: each listing's parameter names, as sent
listings() { sed -n 's/^GET ?.* 200 order=//p' "$1"; }
# unsorted <requests.log>: the listings whose parameters are not sorted by name
unsorted() {
    listings "$1" | while read -r order; do
        sorted=$(printf '%s\n' "$order" | tr ',' '\n' | LC_ALL=C sort | paste -sd, -)
        [ "$order" = "$sorted" ] || echo "$order"
    done
}
later_pages() { listings "$1" | grep -c 'continuation-token' || true; }

# --- (1) the store: six puts under one prefix, then list -----------------------------------
P=locktest/bb136/
for n in 1 2 3 4 5 6; do
    printf '{"n":%d,"text":"put %d"}\n' "$n" "$n" >>"$FROM/a.jsonl"
    put "$P"
    [ "$status" -eq 0 ] || fail "(1) \`spawn store put\` $n under $P exited $status: $(err | tail -n 2 | tr '\n' ' ')"
done
store_cmd "$P" list --area transcripts --
[ "$status" -eq 0 ] || fail "(1) \`spawn store list\` under $P exited $status: $(err | tail -n 2 | tr '\n' ' ')"
[ "$(later_pages "$R2S/requests.log")" -gt 0 ] ||
    fail "(1) no store listing went past its first page: $(listings "$R2S/requests.log" | sort | uniq -c | tr '\n' ' ')"
bad=$(unsorted "$R2S/requests.log" | sort -u | tr '\n' ' ')
[ -z "$bad" ] || fail "(1) store listings send their parameters out of SigV4's sorted order: $bad"
note "(1) checked: the store's listings, past their first page, send their parameters sorted"

# --- (2) the whole bucket's listing: a dump with five other objects there ------------------
for n in 1 2 3 4 5; do
    k="other/made-up-$n"
    printf 'made up %d\n' "$n" >"$(object "$k")"
    jq -nc --arg k "$k" --arg e "$(printf 'made up %d\n' "$n" | md5sum | cut -d' ' -f1)" \
        '{key: $k, etag: $e, size: 10, meta: {}, modified: "2026-11-13T00:00:00.000Z"}' >"$(object "$k").json"
done
backup dump
[ "$status" -eq 0 ] || fail "(2) \`spawn backup dump\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
[ "$(later_pages "$R2/requests.log")" -gt 0 ] ||
    fail "(2) the bucket listing did not go past its first page: $(listings "$R2/requests.log" | sort | uniq -c | tr '\n' ' ')"
bad=$(unsorted "$R2/requests.log" | sort -u | tr '\n' ' ')
[ -z "$bad" ] || fail "(2) bucket listings send their parameters out of SigV4's sorted order: $bad"
note "(2) checked: the whole bucket's listing, past its first page, sends its parameters sorted"

# --- (3) every call names its payload hash; conditional PUTs answered ----------------------
calls=$(grep -c '^call$' "$CURL_LOG" || true)
[ "$calls" -gt 0 ] || harness "(3) the logging curl saw no call"
without=$(awk '/^call$/ { if (n && !h) bad++; n = 1; h = 0; next } /^header x-amz-content-sha256$/ { h = 1 } END { if (n && !h) bad++; print bad + 0 }' "$CURL_LOG")
[ "$without" = 0 ] || fail "(3) $without of $calls curl call(s) name no x-amz-content-sha256 header"
conditional=$(grep -c '^header If-None-Match$' "$CURL_LOG" || true)
[ "$conditional" -gt 0 ] || harness "(3) no conditional PUT was made"
refused=$(grep -c '^PUT .* \(400\|403\) ' "$R2S/requests.log" "$R2/requests.log" | awk -F: '{ n += $2 } END { print n + 0 }')
[ "$refused" = 0 ] || fail "(3) $refused PUT(s) were refused by the stand-in: $(grep -h '^PUT .* \(400\|403\) ' "$R2S/requests.log" "$R2/requests.log" | head -n 2 | tr '\n' ' ')"
note "(3) checked: $calls calls, each naming x-amz-content-sha256; $conditional conditional PUT(s), none refused"

# --- (5) deploy/backup/lib.sh's requests name it too (the owner's part) ----------------------
# Sourced from the tree under test, as pg-backup.sh sources it; its listing
# against the strict stand-in, with the dump's object in it.
echo lib >>"$CURL_LOG"
bash -c '. "$1"; PROG=136; load_r2; list_objects postgres/' _ "$(pwd)/deploy/backup/lib.sh" </dev/null >"$WORK/lib.out" 2>"$WORK/lib.err" ||
    fail "(5) lib.sh's list_objects exited non-zero: $(tail -n 2 "$WORK/lib.err" | tr '\n' ' ')"
named=$(sed -n '/^lib$/,$p' "$CURL_LOG" | grep -c '^argheader x-amz-content-sha256$' || true)
[ "$named" -gt 0 ] || fail "(5) lib.sh's s3() names no x-amz-content-sha256 header"
grep -q '^postgres/' "$WORK/lib.out" || fail "(5) lib.sh's listing names no postgres/ object: $(head -c 200 "$WORK/lib.out")"
note "(5) checked: lib.sh's requests name x-amz-content-sha256"

# --- (4) no secret -------------------------------------------------------------------------
leak=$(secrets_leaked)
[ -z "$leak" ] || fail "(4) a secret (a key's passphrase or the R2 secret key) shows in $leak"
! grep -qF -- "$r2_secret" "$CURL_LOG" || fail "(4) the R2 secret key is in the curl log"
[ "$(stat -c %a "$CURL_LOG")" = 600 ] || fail "(4) the curl log is mode $(stat -c %a "$CURL_LOG"), not 600"
note "(4) checked: no secret in a record, the output or the curl log"

echo "136: every bucket request names its payload hash, and both listings send their query as SigV4 signs it"
exit 0
