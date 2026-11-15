#!/bin/sh
# Objective 198 [recoverability], the controllers' drill, LIVE (drill 2 of
# the owner's four, 2026-11-14 18:43: restores passed 6 of 6; split from
# BUNDLE->X, objective 199): `spawn backup restore-drill --only controllers`
# checks, for each controller an active mission's gate names
# ($SPAWN_LIVE/controllers/<name>/), current and previous, read only, that
# the live directory is `git archive` of its .controller commit from the live
# source (.controller, .deps and web/node_modules left out). One
# backup.drilled.controllers per controller; exit 0 only when all pass.
# deploy/systemd/spawn-backup-restore-drill-controllers.{service,timer} run it
# daily with the bundle unit's environment.
#
# Black box, on its own database, scratch HOME and GNUPGHOME (186.sh's
# harness). A bundle is made first by the bundle unit, against 104's local R2
# stand-in: 198 does not read it, but 199 does, so 198.sh stays a valid kept
# scenario once 199 lands. Controllers are built by the checkout's
# deploy/controller.sh under the scratch SPAWN_LIVE.
#   (1) The controllers unit runs `restore-drill --only controllers`, and its
#       timer names it with an OnCalendar. Its run passes (exit 0), says
#       "--only controllers" and skips (and names) a mission whose gate is
#       outside the controllers; records exactly one backup.drilled.controllers
#       per controller and no other drill record: spawn {ok, missions [its
#       mission], current a2 live ok, prev a1 live ok}, flake {ok, current b1
#       live ok, prev null}; leaves no scratch directory; changes nothing live
#       (the controllers and every source, by path, size, sha256).
#   (2) .deps and web/node_modules in the live controller are not compared
#       (spawn passed in (1) with both there), and each record's left_out
#       names .controller, .deps and web/node_modules.
#   (3) A deploy (spawn moved to a3): the drill passes; spawn's current is a3,
#       its prev a2, both live ok.
#   (4) The live controller changed (other bytes, the executable bit, a file
#       the commit lacks), each in turn: the drill fails (exit 1), spawn's
#       record ok false naming the path, flake's ok.
#   (5) --only repos, controller or an empty value (never transcripts,
#       which objective 200 takes): exit 2, nothing recorded; no secret (the backup passphrase, the R2 secret key) is in
#       any record or output.
#   (6) Which missions name a controller: an active one under the current
#       link only. A closed (done) mission under spawn is not among spawn's
#       missions; a mission gated under spawn.prev and one under the built
#       tree spawn.prev points at are each skipped and named (the previous
#       controller; a built tree), and neither gets a record of its own.
#   (7) Two sources name one controller (a second mission, of spawn's
#       source, gated under flake): the drill fails (exit 1), flake's record
#       ok false saying its missions name 2 sources; spawn's ok.
#   (8) A mission gated under a controller whose current link is not there
#       (controllers/gone/): the drill fails (exit 1), gone's record ok false
#       saying the link is not there; spawn's and flake's ok.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "198: harness: $*" >&2
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
SPAWN_DB_NAME="bb_198_drill_$run"
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
            echo "198: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "198: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "198: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "198: harness: unexpected exit $rc" >&2
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
od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$WORK/backup.key"
passphrase=$(cat "$WORK/backup.key")
r2_secret=$(od -An -N20 -tx1 /dev/urandom | tr -d ' \n')
printf '%s' "$r2_secret" >"$WORK/r2.secret"
access_key="bb198$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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

# Two mission sources whose trees become controllers. spawn: a1 (an
# executable script, a symlink, a file), then a2 (the file changed). flake:
# b1.
src_a=$WORK/spawn.git
src_b=$WORK/flake.git
git init -q --initial-branch=main "$WORK/seed"
mkdir -p "$WORK/seed/rsi/scenarios"
printf '#!/bin/sh\necho scenario\n' >"$WORK/seed/rsi/scenarios/001.sh"
chmod +x "$WORK/seed/rsi/scenarios/001.sh"
printf 'spawn, as deployed first\n' >"$WORK/seed/README.md"
ln -s README.md "$WORK/seed/LINK"
git -C "$WORK/seed" add -A
GIT_COMMITTER_DATE='2026-11-15T01:00:00+00:00' git -C "$WORK/seed" commit -qm a1
a1=$(git -C "$WORK/seed" rev-parse HEAD)
printf 'spawn, as deployed second\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
GIT_COMMITTER_DATE='2026-11-15T02:00:00+00:00' git -C "$WORK/seed" commit -qm a2
a2=$(git -C "$WORK/seed" rev-parse HEAD)
git init -q --bare --initial-branch=main "$src_a"
git -C "$WORK/seed" push -q "$src_a" main:main
git init -q --initial-branch=main "$WORK/seed2"
mkdir -p "$WORK/seed2/gate"
printf 'flake\n' >"$WORK/seed2/gate/README.md"
git -C "$WORK/seed2" add -A
git -C "$WORK/seed2" commit -qm b1
b1=$(git -C "$WORK/seed2" rev-parse HEAD)
git init -q --bare --initial-branch=main "$src_b"
git -C "$WORK/seed2" push -q "$src_b" main:main
# The transcript archive, as the Bridge keeps it beside the workspaces.
# Where the units put it: %h/.local/share/spawn-rsi/ws, under the scratch HOME.
live_dir=$HOME/.local/share/spawn-rsi
SPAWN_WORKSPACE_ROOT=$live_dir/ws
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
SPAWN_BACKUP_KEY_FILE=$WORK/backup.key
DRILL_URL="$server_url/postgres"
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_FILE

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
decrypt() { # decrypt <file> <out>
    gpg --batch --quiet --no-tty --pinentry-mode loopback --no-symkey-cache \
        --passphrase-file "$SPAWN_BACKUP_KEY_FILE" -o "$2" -d "$1" 2>"$WORK/gpg.err"
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
    for secret in "$passphrase" "$r2_secret"; do
        if grep -qF -- "$secret" "$WORK/backup.log" 2>/dev/null; then echo "the command's output"; return; fi
        if [ "$(q "select count(*) from spawn_events where kind::text like '%$secret%'")" != 0 ] ||
            [ "$(q "select count(*) from spawn_commands where action::text like '%$secret%'")" != 0 ]; then
            echo "the database"
            return
        fi
    done
}
db_exists() { [ "$(psql -d postgres -tA -c "select count(*) from pg_database where datname = '$1'")" != 0 ]; }


# --- the units, as systemd runs them --------------------------------------------
# The checkout under test is the working directory (the gate runs scenarios
# from the candidate's tree); the units are its deploy/systemd files.
tree=$(pwd)
units=$tree/deploy/systemd
[ -f "$units/spawn-backup-bundle.service" ] ||
    harness "no deploy/systemd/spawn-backup-bundle.service under $tree: run from the checkout"
mkdir -p "$live_dir/bin"
ln -s "$SPAWN_BIN" "$live_dir/bin/spawn"
umask 077
cat >"$live_dir/spawn.env" <<ENV
SPAWN_DATABASE_URL=$SPAWN_DATABASE_URL
SPAWN_R2_ENV_FILE=$SPAWN_R2_ENV_FILE
SPAWN_BACKUP_KEY_FILE=$SPAWN_BACKUP_KEY_FILE
ENV
umask 022
# unit_run <unit file> [arg ...]: the unit's ExecStart (%h the scratch HOME)
# in an environment of its EnvironmentFile and Environment lines only: every
# SPAWN_*, PG* and git setting of this script's is unset first. Exit 3: the
# unit names no ExecStart or EnvironmentFile.
unit_run() {
    ur_file=$1
    shift
    ur_exec=$(sed -n 's/^ExecStart=//p' "$ur_file" | head -n 1 | sed "s#%h#$HOME#g")
    ur_envfile=$(sed -n 's/^EnvironmentFile=//p' "$ur_file" | head -n 1 | sed "s#%h#$HOME#g")
    [ -n "$ur_exec" ] && [ -n "$ur_envfile" ] || return 3
    sed -n 's/^Environment=//p' "$ur_file" | sed "s#%h#$HOME#g" >"$WORK/unit.environment"
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
        exec $ur_exec "$@"
    )
}
# Every file of the live archive, the sources and the controllers: path, size, sha256.
snapshot() {
    for root in "$archive" "$src_a" "$src_b" "$live_dir/controllers"; do
        find "$root" -type f | LC_ALL=C sort | while read -r file; do
            printf '%s %s %s\n' "$file" "$(size "$file")" "$(sha "$file")"
        done
    done
}
note() { echo "198: $*" >&2; }

# --- the controllers, as deploy/controller.sh builds them -------------------------
ctl=$tree/deploy/controller.sh
[ -f "$ctl" ] || harness "no deploy/controller.sh under $tree"
controllers=$live_dir/controllers
controller() { # controller <name> <git-dir> <commit>: built and made current
    SPAWN_LIVE=$live_dir bash "$ctl" update "$1" "$2" "$3" >>"$WORK/controller.log" 2>&1 ||
        harness "deploy/controller.sh update $1 $3 failed: $(tail -n 3 "$WORK/controller.log" | tr '\n' ' ')"
}
writable() { chmod -R u+w "$(readlink -f "$controllers/$1")"; }
readonly_again() { chmod -R a-w "$(readlink -f "$controllers/$1")"; }
controller spawn "$src_a" "$a1"
controller spawn "$src_a" "$a2"
controller flake "$src_b" "$b1"
[ "$(cat "$controllers/spawn/.controller")" = "$a2" ] && [ "$(cat "$controllers/spawn.prev/.controller")" = "$a1" ] ||
    harness "the spawn controller is not a2 with a1 before it"
# What deploy/controller.sh adds and git does not hold: a deps file and
# installed packages.
writable spawn
printf '%064d\n' 0 >"$controllers/spawn/.deps"
mkdir -p "$controllers/spawn/web/node_modules/left"
printf 'installed\n' >"$controllers/spawn/web/node_modules/left/index.js"
readonly_again spawn

# The missions: one per controller, gated by it; one with no gate; one
# whose gate is outside the controllers.
mission_with() { # mission_with <source> [<gate>]: the new mission's id
    capture "$SPAWN_BIN" mission add --source "$1" \
        --text "198 mission: rebuild every controller from the backup and prove what runs is what was deployed"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    mw_id=$(jq -r .mission "$WORK/out.txt")
    [ -n "$mw_id" ] && [ "$mw_id" != null ] || harness "mission add printed no id: $(out)"
    if [ -n "${2-}" ]; then
        capture "$SPAWN_BIN" mission configure --id "$mw_id" --gate "$2" --accept gate
        [ "$status" -eq 0 ] || harness "mission configure --gate $2 exited $status: $(err)"
    fi
    echo "$mw_id"
}
m_spawn=$(mission_with "$src_a" "$controllers/spawn/rsi/scenarios")
m_flake=$(mission_with "$src_b" "$controllers/flake/gate")
mission_with "$src_a" >/dev/null
mkdir -p "$WORK/elsewhere/gate"
m_outside=$(mission_with "$src_a" "$WORK/elsewhere/gate")

controllers_unit=$units/spawn-backup-restore-drill-controllers.service
controllers_timer=$units/spawn-backup-restore-drill-controllers.timer
drill_only() { # drill_only <log> [arg ...]: the drill with the given arguments, into the scratch parent
    do_log=$1
    shift
    mkdir -p "$WORK/drills"
    capture "$SPAWN_BIN" backup restore-drill --into "$WORK/drills" "$@"
    cat "$WORK/out.txt" "$WORK/err.txt" >"$do_log"
    cat "$do_log" >>"$WORK/backup.log"
}
backup_records() { q "select count(*) from spawn_events where session_id = 'session_control'
    and kind -> 'detail' -> 'record' ->> 'type' like 'backup.drilled.%'"; }
# The newest run's controllers records, one JSON line each, by controller.
run_records() {
    records backup.drilled.controllers | cut -d'|' -f2- | jq -sc '
        (map(.run // 0) | max) as $run | map(select((.run // 0) == $run)) | sort_by(.controller) | .[]'
}
of() { run_records | jq -c --arg c "$1" 'select(.controller == $c)'; }
gotj() { printf '%s' "$1" | jq -r "$2"; }

# --- setup: a bundle, as the bundle unit makes it -----------------------------------
# Not read by this drill; there so a drill that also reads the backup (199)
# runs this contract as it is.
capture unit_run "$units/spawn-backup-bundle.service"
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || harness "the bundle unit's ExecStart exited $status: $(err | tail -n 2 | tr '\n' ' ')"
[ -n "$(newest backup.bundled)" ] || harness "the bundle unit's run recorded no backup.bundled"

# --- (1) the controllers drill unit ---------------------------------------------------
[ -f "$controllers_unit" ] || fail "(1) the checkout has no deploy/systemd/spawn-backup-restore-drill-controllers.service"
[ -f "$controllers_timer" ] || fail "(1) the checkout has no deploy/systemd/spawn-backup-restore-drill-controllers.timer"
grep -qE '^ExecStart=.* backup restore-drill --only controllers( |$)' "$controllers_unit" ||
    fail "(1) the controllers unit's ExecStart is not 'spawn backup restore-drill --only controllers': $(grep '^ExecStart=' "$controllers_unit")"
grep -qE '^OnCalendar=.' "$controllers_timer" || fail "(1) the controllers timer names no OnCalendar"
mkdir -p "$WORK/drills"
snapshot >"$WORK/live-before.txt"
capture unit_run "$controllers_unit" --into "$WORK/drills"
case $status in 3) fail "(1) the controllers unit names no ExecStart or EnvironmentFile" ;; esac
cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/drill1.log"
cat "$WORK/drill1.log" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || fail "(1) the controllers unit's run exited $status: $(tail -n 6 "$WORK/drill1.log" | tr '\n' ' ')"
grep -q -- '--only controllers' "$WORK/drill1.log" ||
    fail "(1) the drill does not say it drills the controllers: $(head -n 4 "$WORK/drill1.log" | tr '\n' ' ')"
grep -q "mission $m_outside" "$WORK/drill1.log" ||
    fail "(1) the drill does not name mission $m_outside, whose gate is outside the controllers"
[ "$(count_records backup.drilled.controllers)" = 2 ] ||
    fail "(1) the run left $(count_records backup.drilled.controllers) backup.drilled.controllers record(s), not 2 (one per controller)"
[ "$(count_records backup.drilled.missions)" = 0 ] && [ "$(count_records backup.drilled.transcripts)" = 0 ] ||
    fail "(1) a controllers drill recorded another drill's record"
spawn_rec=$(of spawn)
got=$(gotj "$spawn_rec" '[.ok, .reason, .missions, .current.commit, .current.live.ok, .prev.commit, .prev.live.ok] | tojson')
want="[true,\"\",[$m_spawn],\"$a2\",true,\"$a1\",true]"
[ "$got" = "$want" ] || fail "(1) spawn's record: expected $want, got $got"
flake_rec=$(of flake)
got=$(gotj "$flake_rec" '[.ok, .missions, .current.commit, .current.live.ok, .prev] | tojson')
want="[true,[$m_flake],\"$b1\",true,null]"
[ "$got" = "$want" ] || fail "(1) flake's record: expected $want, got $got"
left=$(find "$WORK/drills" -mindepth 1 -maxdepth 1 | head -n 3)
[ -z "$left" ] || fail "(1) the drill left its scratch: $left"
snapshot >"$WORK/live-after.txt"
cmp -s "$WORK/live-before.txt" "$WORK/live-after.txt" ||
    fail "(1) the drill changed live: $(diff "$WORK/live-before.txt" "$WORK/live-after.txt" | head -n 4 | tr '\n' ' ')"
note "(1) checked: the controllers unit drilled each controller, current and previous, and recorded one record each"

# --- (2) what the controller adds is left out ------------------------------------------
for rec in "$spawn_rec" "$flake_rec"; do
    [ "$(gotj "$rec" '.left_out | tojson')" = '[".controller",".deps","web/node_modules"]' ] ||
        fail "(2) a record's left_out is $(gotj "$rec" '.left_out | tojson'), not .controller, .deps and web/node_modules"
done
note "(2) checked: .deps and web/node_modules were there and not compared; left_out names them"

# --- (3) a deploy -------------------------------------------------------
printf 'spawn, as deployed third\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
GIT_COMMITTER_DATE='2026-11-15T05:00:30+00:00' git -C "$WORK/seed" commit -qm a3
a3=$(git -C "$WORK/seed" rev-parse HEAD)
git -C "$WORK/seed" push -q "$src_a" main:main
controller spawn "$src_a" "$a3"
drill_only "$WORK/drill2.log" --only controllers
[ "$status" -eq 0 ] || fail "(3) the drill after a deploy exited $status: $(tail -n 6 "$WORK/drill2.log" | tr '\n' ' ')"
got=$(gotj "$(of spawn)" '[.ok, .current.commit, .current.live.ok, .prev.commit, .prev.live.ok] | tojson')
want="[true,\"$a3\",true,\"$a2\",true]"
[ "$got" = "$want" ] || fail "(3) spawn's record after a deploy: expected $want, got $got"
note "(3) checked: after a deploy the drill reads the new current controller and the one before it"

# --- (4) the live controller changed --------------------------------------------------
live_spawn=$(readlink -f "$controllers/spawn")
changed() { # changed <what> <path text>: the drill fails on spawn alone, naming it
    drill_only "$WORK/drill3.log" --only controllers
    [ "$status" -eq 1 ] || fail "(4) the drill over a live controller with $1 exited $status, not 1"
    rec=$(of spawn)
    [ "$(gotj "$rec" '.ok')" = false ] && [ "$(gotj "$rec" '.current.live.ok')" = false ] ||
        fail "(4) spawn's record over $1: $(printf '%s' "$rec" | head -c 400)"
    gotj "$rec" '.current.live.differing[]' | grep -qF "$2" ||
        fail "(4) spawn's record over $1 does not name [$2]: $(gotj "$rec" '.current.live.differing | tojson')"
    [ "$(gotj "$(of flake)" '.ok')" = true ] || fail "(4) flake's record failed when only spawn changed"
}
writable spawn
cp "$live_spawn/README.md" "$WORK/readme.keep"
printf 'changed by hand\n' >>"$live_spawn/README.md"
changed "other bytes" "README.md: other bytes"
cp "$WORK/readme.keep" "$live_spawn/README.md"
chmod +x "$live_spawn/README.md"
changed "the executable bit" "README.md: the executable bit differs"
chmod -x "$live_spawn/README.md"
printf 'not deployed\n' >"$live_spawn/extra.txt"
changed "a file the commit lacks" "extra.txt: not in the commit"
rm -f "$live_spawn/extra.txt"
readonly_again spawn
drill_only "$WORK/drill4.log" --only controllers
[ "$status" -eq 0 ] || harness "the controllers drill does not pass again after the live tree was put back: $(tail -n 4 "$WORK/drill4.log" | tr '\n' ' ')"
note "(4) checked: other bytes, the executable bit and an extra file in a live controller each fail its record alone"

# --- (5) --only's values; no secret -------------------------------------------------
before=$(backup_records)
for value in repos controller ""; do
    drill_only "$WORK/drill6.log" --only "$value"
    [ "$status" -eq 2 ] || fail "(5) --only '$value' exited $status, not 2: $(tail -n 2 "$WORK/drill6.log" | tr '\n' ' ')"
done
[ "$(backup_records)" = "$before" ] || fail "(5) a refused --only recorded a drill ($before before, $(backup_records) after)"
leak=$(secrets_leaked)
[ -z "$leak" ] || fail "(5) a secret (the passphrase or the R2 secret key) shows in $leak"
note "(5) checked: --only refuses repos, controller and an empty value; no secret in a record or the output"

# --- (6) which missions name a controller ---------------------------------------------
close_mission() { # close_mission <id> <state>
    capture "$SPAWN_BIN" mission close --id "$1" --state "$2" --summary "198: the controller's drill no longer reads this one"
    [ "$status" -eq 0 ] || harness "mission close --id $1 --state $2 exited $status: $(err)"
}
m_closed=$(mission_with "$src_a" "$controllers/spawn/rsi/scenarios")
close_mission "$m_closed" done
built=$(basename "$(readlink -f "$controllers/spawn.prev")")
[ -d "$controllers/$built" ] && [ ! -L "$controllers/$built" ] && [ "$built" != spawn.prev ] ||
    harness "spawn.prev does not point at a built tree under the controllers: $built"
m_prev=$(mission_with "$src_a" "$controllers/spawn.prev/rsi/scenarios")
m_built=$(mission_with "$src_a" "$controllers/$built/rsi/scenarios")
drill_only "$WORK/drill7.log" --only controllers
[ "$status" -eq 0 ] || fail "(6) the drill with a closed, a previous-gated and a built-gated mission exited $status: $(tail -n 6 "$WORK/drill7.log" | tr '\n' ' ')"
[ "$(run_records | jq -r .controller | tr '\n' ' ')" = "flake spawn " ] ||
    fail "(6) the run recorded controllers [$(run_records | jq -r .controller | tr '\n' ' ')], not flake and spawn alone"
[ "$(gotj "$(of spawn)" '.missions | tojson')" = "[$m_spawn]" ] ||
    fail "(6) spawn's missions are $(gotj "$(of spawn)" '.missions | tojson'), not [$m_spawn] (mission $m_closed is closed)"
grep -q "skipped mission $m_prev: .*the previous controller" "$WORK/drill7.log" ||
    fail "(6) the drill does not skip and name mission $m_prev, gated under spawn.prev: $(grep skipped "$WORK/drill7.log" | tr '\n' ' ')"
grep -q "skipped mission $m_built: .*a built tree" "$WORK/drill7.log" ||
    fail "(6) the drill does not skip and name mission $m_built, gated under the built $built: $(grep skipped "$WORK/drill7.log" | tr '\n' ' ')"
note "(6) checked: only active missions under a current link name a controller; spawn.prev and a built tree are skipped and named"

# --- (7) two sources name one controller ------------------------------------------------
m_two=$(mission_with "$src_a" "$controllers/flake/gate")
drill_only "$WORK/drill8.log" --only controllers
[ "$status" -eq 1 ] || fail "(7) the drill with two sources under flake exited $status, not 1"
rec=$(of flake)
[ "$(gotj "$rec" '.ok')" = false ] && gotj "$rec" '.reason' | grep -q 'name 2 sources, not one' ||
    fail "(7) flake's record with two sources: $(printf '%s' "$rec" | head -c 400)"
[ "$(gotj "$(of spawn)" '.ok')" = true ] || fail "(7) spawn's record failed when only flake had two sources"
close_mission "$m_two" dropped
note "(7) checked: a controller its missions name by two sources fails its record alone"

# --- (8) a controller whose current link is not there ------------------------------------
mkdir -p "$WORK/gone-target/gate"
ln -s "$WORK/gone-target" "$controllers/gone"
m_gone=$(mission_with "$src_a" "$controllers/gone/gate")
rm "$controllers/gone"
drill_only "$WORK/drill9.log" --only controllers
[ "$status" -eq 1 ] || fail "(8) the drill with controllers/gone missing exited $status, not 1"
rec=$(of gone)
[ "$(gotj "$rec" '.ok')" = false ] && gotj "$rec" '.reason' | grep -q 'gone is not there' ||
    fail "(8) gone's record: $(printf '%s' "$rec" | head -c 400)"
[ "$(gotj "$(of spawn)" '.ok')" = true ] && [ "$(gotj "$(of flake)" '.ok')" = true ] ||
    fail "(8) spawn's or flake's record failed when only gone's link was missing"
close_mission "$m_gone" dropped
note "(8) checked: a named controller with no current link fails its record"

echo "198: each live controller is its own commit's tree, what deploy/controller.sh adds left out"
exit 0
