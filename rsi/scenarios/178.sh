#!/bin/sh
# Objective 178 [recoverability]: the Rust readers that compare a stored
# commit id with git read it through the history rewrite's commit map
# (objective 173). The owner rewrites a source's history (every changed
# commit gets a new id, an empty one is dropped, the old objects are pruned)
# and loads git filter-repo's map (`spawn commit-map load`). Before 178,
# three readers compared the old id with git as it was:
#   - the controller's commit (its directory, controllers/<name>-<40 hex>,
#     built before the rewrite) against the owner's landing: the old id is
#     no commit of the rewritten source, so a task never waited for the
#     deploy and was judged by the old controller's scenarios;
#   - `spawn mission moved --from <old> --to <new>` (deploy.sh records a
#     deploy from the live controller's commit): refused as "does not
#     descend";
#   - the restore drill's recorded heads (the bundle's, made before the
#     rewrite) against live: every head a "mismatch".
# An id the map dropped, or in neither the map nor the repository, now
# reads as such, never as "behind", "does not descend" or "mismatch".
#
# Black box, on its own database, scratch HOME and GNUPGHOME, against a
# local R2 stand-in (104's): one gated mission whose gate is a controller
# directory (deploy/controller.sh's layout). Before the rewrite the owner
# lands m1 and the controller is deployed at m1; `spawn backup bundle`
# records the source's heads: main at m1, `e` at an empty commit E on m1,
# `lost` at a commit L on m1. The rewrite: m0 kept, m1 -> m1', E dropped
# (`e` moves to m1'), L unmapped (`lost` moves to m1'), every old object
# pruned; the map is loaded. After it the owner lands X on m1', and a
# reported task waits for its gate.
#   (1) one `spawn integrate --once` pass: the controller (spawn-<m1>) is
#       behind the owner's X through the map: no gate runs, the task has no
#       verdict, and the integrator says it waits for the deploy.
#   (2) the controller switched to spawn-<E> (a commit the map dropped): the
#       next pass judges the task (no wait), and says the controller's commit
#       "was dropped by the history rewrite (the commit map names no new
#       commit for it)".
#   (3) `spawn mission moved --from <m1> --to <X>` exits 0 and records
#       source.moved from m1' to X.
#   (4) `--from <E>`: refused, "--from <E> was dropped by the history
#       rewrite (the commit map names no new commit for it)", and never
#       "does not descend"; `--from <40 ones>`: refused, "--from <id> is an
#       unknown commit (in neither the commit map nor the source)".
#   (5) `spawn backup restore-drill`: main (bundled at m1) is read as m1',
#       which live's main has moved on from (a descendant); `e` is said "dropped by the history rewrite"; `lost`
#       (L) is an unknown commit: the drill fails (exit 1) on it alone,
#       "0 mismatch(es) and 1 unknown commit(s)", no line calls any of the
#       three a mismatch, and backup.drilled.missions records ok false,
#       mismatches 1.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on the base (173, e36a0ea3): (1) the gate runs, (3) "does
# not descend", (5) three mismatches.


set -eu

asserted=""
harness() {
    echo "178: harness: $*" >&2
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
SPAWN_DB_NAME="bb_178_commit_now_$run"
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
            echo "178: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "178: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "178: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "178: harness: unexpected exit $rc" >&2
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
access_key="bb178$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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

: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
note() { echo "178: $*" >&2; }
q() { scalar "$1"; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}
for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL \
    SPAWN_GATE_MIN_FREE_GB SPAWN_GATE_JOBS SPAWN_GATE_WORKER_CORES SPAWN_GATE_MIN_FREE_MEMORY_MB \
    SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG SPAWN_INTEGRATE_DRAIN_SECS SPAWN_GATE_ENV_FILE \
    SPAWN_INTEGRATE_WARM SPAWN_INTEGRATE_SHADOW SPAWN_GATE_RUNNER; do
    unset "$name" 2>/dev/null || true
done
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
mkdir -p "$SPAWN_CLAUDE_PROJECTS"
SPAWN_WORKSPACE_ROOT=$WORK/ws
mkdir -p "$SPAWN_WORKSPACE_ROOT/.transcripts/agent_supervisor"
printf '{"type":"user","uuid":"u-1","message":{"role":"user","content":"one line"}}\n' \
    >"$SPAWN_WORKSPACE_ROOT/.transcripts/agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0178.jsonl"
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_R2_ENV_FILE=$WORK/r2.env
SPAWN_BACKUP_KEY_FILE=$WORK/backup.key
export SPAWN_CLAUDE_PROJECTS SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_FILE

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- the world: a source, its controllers, a gated mission ---------------------------------
seed=$WORK/seed
mkdir -p "$seed/rsi/scenarios"
git init -q --initial-branch=main "$seed"
printf '#!/bin/sh\nexit 0\n' >"$seed/rsi/scenarios/check-ok.sh"
printf '# nothing protected\n' >"$seed/rsi/scenarios/protected"
: >"$seed/rsi/scenarios/no-test-database"
printf 'm0\n' >"$seed/file"
git -C "$seed" add -A
git -C "$seed" commit -qm m0
m0=$(git -C "$seed" rev-parse HEAD)
src=$WORK/source.git
git clone -q --bare "$seed" "$src"
controllers=$WORK/controllers
deploy() { # <commit>: deploy/controller.sh's layout, controllers/spawn-<commit> and the spawn link
    mkdir -p "$controllers/spawn-$1/rsi"
    [ -d "$controllers/spawn-$1/rsi/scenarios" ] || cp -R "$seed/rsi/scenarios" "$controllers/spawn-$1/rsi/scenarios"
    rm -f "$controllers/spawn.next"
    ln -s "spawn-$1" "$controllers/spawn.next" || harness "ln spawn.next failed"
    mv -T "$controllers/spawn.next" "$controllers/spawn" || harness "switching the controller to $1 failed"
}
deploy "$m0"
# The gate's environment is the integrator's choice: the log's path is
# written into the script.
cat >"$WORK/fake-gate.sh" <<GATEEOF
#!/bin/sh
echo "judged \$*" >>"$WORK/judged.log"
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
exit 0
GATEEOF
chmod 755 "$WORK/fake-gate.sh"
: >"$WORK/judged.log"
capture "$SPAWN_BIN" mission add --text "178 commit map readers" --source "$src" \
    --gate "$controllers/spawn/rsi/scenarios" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")

git clone -q "$src" "$WORK/owner"
push() { # <commit> <reason>: the owner's ungated request
    capture sh -c 'cd "$1" && shift && exec "$@"' sh "$WORK/owner" "$SPAWN_BIN" mission push --source "$src" --commit "$1" --reason "$2"
    [ "$status" -eq 0 ] || harness "mission push ($2) failed: $(err)"
}
commit() { # <subject> [--allow-empty]: an owner commit; prints its id
    if [ "${2-}" = --allow-empty ]; then
        git -C "$WORK/owner" commit -q --allow-empty -m "$1"
    else
        printf '%s\n' "$1" >>"$WORK/owner/file"
        git -C "$WORK/owner" add -A
        git -C "$WORK/owner" commit -qm "$1"
    fi
    git -C "$WORK/owner" rev-parse HEAD
}
mkdir -p "$WORK/integrator"
integrate() {
    capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
    cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/pass.log"
    cat "$WORK/pass.log" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
}
main_now() { git --git-dir="$src" rev-parse refs/heads/main; }
judged() { grep -c . "$WORK/judged.log" || true; }

# Before the rewrite: the owner lands m1, the deploy switches the
# controller to it; `e` (an empty E) and `lost` (L) are branches on m1.
m1=$(commit m1)
push "$m1" "m1, before the rewrite"
integrate
[ "$(main_now)" = "$m1" ] || harness "the owner's m1 did not land: main is $(main_now)"
deploy "$m1"
e=$(commit E --allow-empty)
git -C "$WORK/owner" push -q origin "$e:refs/heads/e" || harness "push e failed"
git -C "$WORK/owner" reset -q --hard "$m1"
l=$(commit L)
git -C "$WORK/owner" push -q origin "$l:refs/heads/lost" || harness "push lost failed"
git -C "$WORK/owner" reset -q --hard "$m1"

backup_env() { # backup_env <verb>: never a setting on a command line
    exec_verb=$1
    ( exec "$SPAWN_BIN" backup "$exec_verb" )
}
capture backup_env bundle
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || harness "\`spawn backup bundle\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bundled=$(q "select (kind -> 'detail' -> 'record' -> 'repos' -> 0 -> 'heads')::text from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'backup.bundled' order by id desc limit 1")
for head in "\"refs/heads/main\": \"$m1\"" "\"refs/heads/e\": \"$e\"" "\"refs/heads/lost\": \"$l\""; do
    case $bundled in *"$head"*) ;; *) harness "the bundle does not record $head: $bundled" ;; esac
done

# --- the rewrite, as filter-repo does it ---------------------------------------------------------
tree_of() { git --git-dir="$src" rev-parse "$1^{tree}"; }
m1n=$(GIT_COMMITTER_DATE="2026-11-14T12:00:00Z" GIT_AUTHOR_DATE="2026-11-14T12:00:00Z" \
    git --git-dir="$src" commit-tree "$(tree_of "$m1")" -p "$m0" -m "m1 (rewritten)")
git --git-dir="$src" for-each-ref --format='%(refname) %(objectname)' | while read -r ref at; do
    case $at in "$m1" | "$e" | "$l") git --git-dir="$src" update-ref "$ref" "$m1n" ;; esac
done
git --git-dir="$src" reflog expire --expire=now --all
git --git-dir="$src" gc -q --prune=now
for old in "$m1" "$e" "$l"; do
    if git --git-dir="$src" cat-file -e "$old^{commit}" 2>/dev/null; then
        harness "the old commit $old is still in the source after the prune"
    fi
done
zeros=0000000000000000000000000000000000000000
printf 'old                                      new\n%s %s\n%s %s\n%s %s\n' \
    "$m0" "$m0" "$m1" "$m1n" "$e" "$zeros" >"$WORK/commit-map"
capture "$SPAWN_BIN" commit-map load "$WORK/commit-map" --source "$src"
[ "$status" -eq 0 ] || harness "spawn commit-map load exited $status: $(err | head -c 300)"

# After it: the owner lands X on m1'; the controller is still spawn-<m1>.
git -C "$WORK/owner" fetch -q origin || harness "the owner's fetch failed"
git -C "$WORK/owner" reset -q --hard origin/main
x=$(commit X)
push "$x" "X, after the rewrite"
integrate
[ "$(main_now)" = "$x" ] || harness "the owner's X did not land: main is $(main_now)"

# A reported task whose branch holds its work on X.
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker w178 --brief "work after the rewrite"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
agent=$(q "select agent_id from mission_tasks where id = $task")
session=$(q "select session_id from mission_tasks where id = $task")
capture "$SPAWN_BIN" submit --agent "$agent" --session "$session" --key k178 \
    --action '{"type":"start","engine":"claude","prompt":"made-up work"}'
[ "$status" -eq 0 ] || harness "submit failed: $(err)"
git -C "$WORK/owner" checkout -q -b work
printf 'work\n' >"$WORK/owner/work.txt"
git -C "$WORK/owner" add -A
git -C "$WORK/owner" commit -qm work
git -C "$WORK/owner" push -q origin "work:refs/heads/spawn/$agent" || harness "push the task's branch failed"
command=$(q "select id from spawn_commands where idempotency_key = 'k178'")
[ "$(q "select spawn_task_delivered($task, $command)")" = t ] || harness "task $task not delivered"
[ "$(q "select spawn_task_reported($task, 'done')")" = t ] || harness "task $task not reported"
verdicts() {
    q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
        and (kind -> 'detail' -> 'record' ->> 'task')::bigint = $task"
}

# --- (1) the controller from before the rewrite is behind X ----------------------------------------
integrate
expect_eq "$(judged)" 0 "(1) gate runs while the controller (spawn-<m1>) is behind the owner's X"
expect_eq "$(verdicts)" 0 "(1) the task's verdicts while it waits"
grep -qF "waits for the deploy that switches the controller" "$WORK/pass.log" ||
    check_failed "(1) the integrator does not say the task waits for the deploy: $(grep -v '^| ' "$WORK/pass.log" | tail -n 5 | tr '\n' ' ' | cut -c1-600)"
note "(1) done"

# --- (2) a controller at a dropped commit: never a wait -----------------------------------------------
deploy "$e"
integrate
[ "$(judged)" -ge 1 ] || check_failed "(2) the task is not judged with the controller at the dropped E: $(grep -v '^| ' "$WORK/pass.log" | tail -n 5 | tr '\n' ' ' | cut -c1-600)"
grep -qF "the controller's commit $e was dropped by the history rewrite (the commit map names no new commit for it)" "$WORK/integrate.log" ||
    check_failed "(2) the integrator does not say the controller's commit was dropped: $(grep -v '^| ' "$WORK/pass.log" | tail -n 5 | tr '\n' ' ' | cut -c1-600)"
if grep -F "$e" "$WORK/integrate.log" | grep -qF "is newer than the controller its scenarios come from"; then
    check_failed "(2) the dropped controller is said to be behind"
fi
note "(2) done"

# --- (3) and (4) spawn mission moved -------------------------------------------------------------------
moved_before=$(q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'")
capture "$SPAWN_BIN" mission moved --source "$src" --from "$m1" --to "$x"
if [ "$status" -ne 0 ]; then
    check_failed "(3) mission moved --from <m1> --to <X> exited $status: $(err | head -c 300)"
else
    rec=$(q "select (kind -> 'detail' -> 'record')::text from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'source.moved' order by id desc limit 1")
    expect_eq "$(printf '%s' "$rec" | jq -c '{from, to}')" "{\"from\":\"$m1n\",\"to\":\"$x\"}" "(3) the source.moved recorded"
    expect_eq "$(q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'")" \
        "$((moved_before + 1))" "(3) source.moved records"
fi
capture "$SPAWN_BIN" mission moved --source "$src" --from "$e" --to "$x"
[ "$status" -ne 0 ] || check_failed "(4) mission moved --from <E> exited 0"
grep -qF -- "--from $e was dropped by the history rewrite (the commit map names no new commit for it)" "$WORK/err.txt" ||
    check_failed "(4) --from <E> is not refused as dropped: $(err | head -c 300)"
! grep -qF "does not descend" "$WORK/err.txt" || check_failed "(4) --from <E> is said not to descend"
ones=1111111111111111111111111111111111111111
capture "$SPAWN_BIN" mission moved --source "$src" --from "$ones" --to "$x"
[ "$status" -ne 0 ] || check_failed "(4) mission moved --from <unknown> exited 0"
grep -qF -- "--from $ones is an unknown commit (in neither the commit map nor the source)" "$WORK/err.txt" ||
    check_failed "(4) --from <unknown> is not refused as an unknown commit: $(err | head -c 300)"
! grep -qF "does not descend" "$WORK/err.txt" || check_failed "(4) --from <unknown> is said not to descend"
note "(3) (4) done"

# --- (5) the restore drill against live --------------------------------------------------------------
mkdir -p "$WORK/drills"
capture "$SPAWN_BIN" backup restore-drill --into "$WORK/drills"
cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/drill.log"
expect_eq "$status" 1 "(5) the drill's exit (lost alone is an unknown commit)"
line=$(grep -F "restore-drill: missions $src:" "$WORK/drill.log" | grep -F "refs restored" | head -n 1)
case $line in
*"against live: 0 the same, 1 moved on since (descendants), "*"0 mismatch(es), 1 dropped by the history rewrite, 1 unknown commit(s)") ;;
*) check_failed "(5) main bundled at m1 is not read as m1' (live moved on past it), or the counts differ: $line" ;;
esac
grep -qF "refs/heads/e: restored at $e was dropped by the history rewrite (the commit map names no new commit for it)" "$WORK/drill.log" ||
    check_failed "(5) e is not said dropped: $(grep -F 'refs/heads/e' "$WORK/drill.log" | head -c 300)"
grep -qF "refs/heads/lost: restored at $l is an unknown commit (in neither the commit map nor the source)" "$WORK/drill.log" ||
    check_failed "(5) lost is not said an unknown commit: $(grep -F 'refs/heads/lost' "$WORK/drill.log" | head -c 300)"
if grep -F "mismatch:" "$WORK/drill.log" | grep -qE "refs/heads/(main|e|lost)"; then
    check_failed "(5) a head is called a mismatch: $(grep -F 'mismatch:' "$WORK/drill.log" | head -n 3 | tr '\n' ' ' | cut -c1-500)"
fi
grep -qF "restore-drill: FAILED: 0 mismatch(es) and 1 unknown commit(s) older than bundle" "$WORK/drill.log" ||
    check_failed "(5) the verdict line: $(grep -F 'restore-drill: FAILED' "$WORK/drill.log" | head -c 300)"
drilled=$(q "select (kind -> 'detail' -> 'record')::text from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'backup.drilled.missions' order by id desc limit 1")
expect_eq "$(printf '%s' "$drilled" | jq -c '{ok, mismatches}')" '{"ok":false,"mismatches":1}' "(5) backup.drilled.missions"
note "(5) done"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "178: $(wc -l <"$WORK/failures") checks failed" >&2
    exit 1
fi
echo "178: a controller, a deploy's from and a backup's heads recorded before the rewrite are read through the commit map"
exit 0
