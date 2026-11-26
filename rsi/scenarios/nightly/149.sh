#!/bin/sh
# Objective 149 [recoverability], the transcript and mission restore drill
# runs daily and judges the backup against its record:
# `spawn backup bundle` records each transcript's sha256 as bundled
# (`transcripts.sha256`), `spawn backup restore-drill` checks every restored
# transcript against it, and deploy/systemd/spawn-backup-restore-drill.service
# runs the drill with the bundle unit's environment, so its records reach the
# store the bundle's records are in.
#
# Black box, on its own database, scratch HOME and GNUPGHOME, against 104's
# local R2 stand-in. The bundle and the first drill run as their units run
# them: the unit file's ExecStart, with %h the scratch HOME, in an
# environment of the unit's EnvironmentFile and Environment lines only
# (spawn.env in the scratch HOME names the database, R2 and the key); the
# drill's ExecStart is given `--into <scratch>`, so nothing goes to /data/tmp.
#   (1) The bundle unit's run records backup.bundled with transcripts.sha256:
#       for every file its sizes name, the sha256 of the live file's first
#       size bytes.
#   (2) The drill unit's run, in the same environment, passes (exit 0), says
#       "<n> of <n> files as the newest record's sha256", records
#       backup.drilled.transcripts and backup.drilled.missions ok in the same
#       store, naming the bundle unit's record, leaves no scratch directory,
#       and changes nothing live (the archive and every source, by path, size
#       and sha256).
#   (3) Live moves on, append-only (a transcript grows, a commit is pushed):
#       the drill still passes; the grown file is information, not a failure.
#   (4) A live transcript changed inside the backed-up range (same size, one
#       byte other): the drill fails (exit 1), transcripts ok false with a
#       mismatch, missions ok true.
#   (5) A newest bundle record made before 149 (the same record without
#       transcripts.sha256, inserted as the store writes control records):
#       with live put back, the drill passes, says the record names no
#       per-file sha256 and the files are judged against live only.
#   (6) A newest record naming, for one file, the sha256 of other bytes
#       (backup and live agree): the drill fails (exit 1), transcripts ok
#       false with that one mismatch, named; missions ok true.
#   (7) No secret (the backup passphrase, the R2 secret key) is in any record
#       or output.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "149: harness: $*" >&2
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
SPAWN_DB_NAME="bb_149_drill_$run"
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
            echo "149: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "149: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "149: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "149: harness: unexpected exit $rc" >&2
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
access_key="bb149$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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
add_missions() { # add_missions <from> <to>: control records by a real command
    i=$1
    while [ "$i" -le "$2" ]; do
        if [ $((i % 2)) -eq 0 ]; then src=$src_spawn; else src=$src_other; fi
        capture "$SPAWN_BIN" mission add --source "$src" \
            --text "149 mission $i: back the fleet's record up off this host, every hour, and prove it restores"
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
    # The bundle unit's notes folder (objective 216) is the live one: left
    # out here, as nothing live is read; 216.sh runs it on a scratch folder.
    sed -n 's/^Environment=//p' "$ur_file" | sed "s#%h#$HOME#g" |
        sed '/^SPAWN_BACKUP_SYNC_DIR=/d' >"$WORK/unit.environment"
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
# Every file of the live archive and the sources: path, size, sha256.
snapshot() {
    for root in "$archive" "$src_spawn" "$src_other" "$src_empty" "$src_tag"; do
        find "$root" -type f | LC_ALL=C sort | while read -r file; do
            printf '%s %s %s\n' "$file" "$(size "$file")" "$(sha "$file")"
        done
    done
}
note() { echo "149: $*" >&2; }
drill() { # drill <log>: run the restore drill into the scratch parent
    mkdir -p "$WORK/drills"
    capture "$SPAWN_BIN" backup restore-drill --into "$WORK/drills"
    cat "$WORK/out.txt" "$WORK/err.txt" >"$1"
    cat "$1" >>"$WORK/backup.log"
}

add_missions 1 4

# --- (1) the bundle unit records each transcript's sha256 --------------------------
capture unit_run "$units/spawn-backup-bundle.service"
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || harness "the bundle unit's ExecStart exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bun=$(newest backup.bundled)
[ -n "$bun" ] || harness "the bundle unit's run recorded no backup.bundled"
bkey=$(field "$bun" '.keys[0] // empty')
bevent=$(event_of "$bun")
[ -n "$bkey" ] && has_object "$bkey" || harness "the bundle's key [$bkey] is not in the R2 stand-in"
files=$(field "$bun" '.transcripts.sizes | keys | length')
[ "$files" -ge 2 ] || harness "the bundle record names $files transcript(s)"
for path in $(field "$bun" '.transcripts.sizes | keys[]'); do
    bytes=$(field "$bun" ".transcripts.sizes[\"$path\"]")
    want=$(head -c "$bytes" "$archive/$path" | sha256sum | cut -d' ' -f1)
    got=$(field "$bun" ".transcripts.sha256[\"$path\"] // \"none\"")
    [ "$got" = "$want" ] || fail "(1) the bundle record's sha256 of $path: expected $want (its first $bytes bytes), got $got"
done
note "(1) checked: the bundle record names each transcript's sha256 ($files files)"

# --- (2) the drill unit, in the same environment, passes and records -----------------
[ -f "$units/spawn-backup-restore-drill.service" ] ||
    fail "(2) the checkout has no deploy/systemd/spawn-backup-restore-drill.service"
mkdir -p "$WORK/drills"
snapshot >"$WORK/live-before.txt"
capture unit_run "$units/spawn-backup-restore-drill.service" --into "$WORK/drills"
case $status in 3) fail "(2) the drill unit names no ExecStart or EnvironmentFile" ;; esac
cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/drill1.log"
cat "$WORK/drill1.log" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || fail "(2) the drill unit's run exited $status: $(tail -n 4 "$WORK/drill1.log" | tr '\n' ' ')"
grep -qxF "restore-drill: transcripts: $files of $files files as the newest record's sha256" "$WORK/drill1.log" ||
    fail "(2) the drill does not say all $files files are as the record's sha256: $(grep '^restore-drill: transcripts' "$WORK/drill1.log" | tr '\n' ' ' | head -c 400)"
for kind in backup.drilled.transcripts backup.drilled.missions; do
    [ "$(count_records "$kind")" = 1 ] || fail "(2) the drill unit's run left $(count_records "$kind") $kind record(s) in the bundle's store, not 1"
    rec=$(newest "$kind")
    got=$(printf '%s' "$rec" | cut -d'|' -f2- | jq -c '{ok, reason, bundle, bundle_event_id, mismatches}')
    want="{\"ok\":true,\"reason\":\"\",\"bundle\":\"$bkey\",\"bundle_event_id\":$bevent,\"mismatches\":0}"
    [ "$got" = "$want" ] || fail "(2) $kind: expected $want, got $got"
done
left=$(find "$WORK/drills" -mindepth 1 -maxdepth 1 | head -n 3)
[ -z "$left" ] || fail "(2) the drill left its scratch: $left"
snapshot >"$WORK/live-after.txt"
cmp -s "$WORK/live-before.txt" "$WORK/live-after.txt" ||
    fail "(2) the drill changed live: $(diff "$WORK/live-before.txt" "$WORK/live-after.txt" | head -n 4 | tr '\n' ' ')"
note "(2) checked: the drill unit's run passed against the record and recorded in the bundle's store"

# --- (3) live moves on, append-only ----------------------------------------------------
supervisor_file=$(find "$archive/agent_supervisor" -name '*.jsonl' | head -n 1)
worker_file=$(find "$archive/agent_worker_objective_7" -name '*.jsonl' | head -n 1)
printf '{"type":"assistant","uuid":"s-grown","message":{"role":"assistant","content":"grown since the bundle"}}\n' >>"$supervisor_file"
printf 'after the bundle\n' >"$WORK/seed/after.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "after the bundle"
git -C "$WORK/seed" push -q "$src_spawn" work:refs/heads/spawn/agent_worker_objective_7
drill "$WORK/drill2.log"
[ "$status" -eq 0 ] || fail "(3) the drill after live grew (append-only) exited $status: $(tail -n 4 "$WORK/drill2.log" | tr '\n' ' ')"
grep -q ' 1 grown since' "$WORK/drill2.log" ||
    fail "(3) the drill does not count the grown transcript: $(grep '^restore-drill: transcripts' "$WORK/drill2.log" | tr '\n' ' ' | head -c 400)"
[ "$(field "$(newest backup.drilled.transcripts)" '.ok')" = true ] || fail "(3) the transcripts' record is not ok after live grew"
note "(3) checked: a transcript grown since the bundle is information, not a failure"

# --- (4) a live transcript changed inside the backed-up range -----------------------
cp "$worker_file" "$WORK/worker.keep"
printf 'X' | dd of="$worker_file" bs=1 seek=5 conv=notrunc 2>/dev/null || harness "cannot change a byte of $worker_file"
[ "$(size "$worker_file")" = "$(size "$WORK/worker.keep")" ] || harness "the changed transcript's size moved"
drill "$WORK/drill3.log"
[ "$status" -eq 1 ] || fail "(4) the drill over a transcript changed inside its backed-up range exited $status, not 1: $(tail -n 4 "$WORK/drill3.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.transcripts)
[ "$(field "$rec" '.ok')" = false ] && [ "$(field "$rec" '.mismatches')" -ge 1 ] ||
    fail "(4) the transcripts' record: $(printf '%s' "$rec" | cut -d'|' -f2-)"
[ "$(field "$(newest backup.drilled.missions)" '.ok')" = true ] || fail "(4) the missions' record is not ok"
cp "$WORK/worker.keep" "$worker_file"
note "(4) checked: a byte changed inside the backed-up range fails the transcripts"

# --- (5) a record bundled before 149 -------------------------------------------------
old=$(printf '%s' "$bun" | cut -d'|' -f2- | jq -c 'del(.transcripts.sha256)')
old_event=$(sql "begin;
    select pg_advisory_xact_lock(1399957335);
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', '$old'::jsonb)),
            (select coalesce(max(session_seq), 0) + 1 from spawn_events where session_id = 'session_control'),
            (floor(extract(epoch from clock_timestamp()) * 1000))::bigint)
    returning id;
    commit;" | grep -E '^[0-9]+$' | head -n 1)
case "$old_event" in '' | *[!0-9]*) harness "the record without sha256 was not inserted: [$old_event]" ;; esac
drill "$WORK/drill4.log"
[ "$status" -eq 0 ] || fail "(5) the drill over a record without per-file sha256 exited $status: $(tail -n 4 "$WORK/drill4.log" | tr '\n' ' ')"
grep -q 'names no per-file sha256' "$WORK/drill4.log" ||
    fail "(5) the drill does not say the record names no per-file sha256: $(grep '^restore-drill: transcripts' "$WORK/drill4.log" | tr '\n' ' ' | head -c 400)"
[ "$(field "$(newest backup.drilled.transcripts)" '.bundle_event_id')" = "$old_event" ] ||
    fail "(5) the drill did not judge the newest record ($old_event)"
note "(5) checked: a record without per-file sha256 is said and judged against live"

# --- (6) a record whose sha256 is not the restored bytes ------------------------------
# The newest record names, for one file, the sha256 of other bytes; the
# backup and live agree with each other, so only the record can say no.
insert_record() { # insert_record <record json>: as the store writes control records; prints the event id
    sql "begin;
    select pg_advisory_xact_lock(1399957335);
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', '$1'::jsonb)),
            (select coalesce(max(session_seq), 0) + 1 from spawn_events where session_id = 'session_control'),
            (floor(extract(epoch from clock_timestamp()) * 1000))::bigint)
    returning id;
    commit;" | grep -E '^[0-9]+$' | head -n 1
}
one=$(field "$bun" '.transcripts.sizes | keys[0]')
other_sha=$(printf 'not these bytes\n' | sha256sum | cut -d' ' -f1)
wrong=$(printf '%s' "$bun" | cut -d'|' -f2- | jq -c --arg path "$one" --arg sha "$other_sha" '.transcripts.sha256[$path] = $sha')
wrong_event=$(insert_record "$wrong")
case "$wrong_event" in '' | *[!0-9]*) harness "the record with another sha256 was not inserted: [$wrong_event]" ;; esac
drill "$WORK/drill5.log"
[ "$status" -eq 1 ] || fail "(6) the drill over a record naming another sha256 for $one exited $status, not 1: $(tail -n 4 "$WORK/drill5.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.transcripts)
[ "$(field "$rec" '.bundle_event_id')" = "$wrong_event" ] && [ "$(field "$rec" '.ok')" = false ] &&
    [ "$(field "$rec" '.mismatches')" = 1 ] ||
    fail "(6) the transcripts' record over a record naming another sha256: $(printf '%s' "$rec" | cut -d'|' -f2-)"
grep -qF "transcripts/$one: the restored bytes are not the sha256 the newest record names" "$WORK/drill5.log" ||
    fail "(6) the drill does not name $one as not the record's sha256: $(grep 'mismatch' "$WORK/drill5.log" | head -n 2 | tr '\n' ' ')"
[ "$(field "$(newest backup.drilled.missions)" '.ok')" = true ] || fail "(6) the missions' record is not ok"
note "(6) checked: restored bytes that are not the record's sha256 fail, with live the same"

# --- (7) no secret ------------------------------------------------------------------
leak=$(secrets_leaked)
[ -z "$leak" ] || fail "(7) a secret (the passphrase or the R2 secret key) shows in $leak"
note "(7) checked: no secret in a record or the output"

echo "149: the restore drill runs as its unit, judges the backup against its record, and records in the bundle's store"
exit 0
