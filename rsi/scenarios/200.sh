#!/bin/sh
# Objective 200 [recoverability], restore drill 3, the transcript archive
# (the owner's 18:43 order: restores passed 6 of 6; the lead's (A), 11-15):
# `spawn backup restore-drill --only transcripts` restores the transcript
# archive alone from the newest bundle chain: from each bundle it unpacks
# by name only its manifest and its transcript slices (decrypted in a pipe,
# as --only missions does), restores the chain into its own drill folder,
# judges every file by the newest record alone (the size it names and its
# transcripts.sha256), never reads live, removes only its own folder, and
# records backup.drilled.transcripts alone. Before it fetches anything it
# refuses (exit 2) when the unpack would leave its file system under the
# 40 GB floor (SPAWN_BACKUP_DRILL_FLOOR_MB). deploy/systemd/spawn-backup-
# restore-drill-transcripts.{service,timer} run it daily with the bundle
# unit's environment.
#
# Black box, on its own database, scratch HOME and GNUPGHOME, against 104's
# local R2 stand-in (186.sh's harness). The bundle and the first drill run
# as their units run them (ExecStart, %h the scratch HOME, the unit's
# environment only); the drill is given `--into <scratch parent>`. While a
# drill runs, the originals it must never open are mode 000: every file of
# the live archive, a transcript planted in the fake HOME
# (~/.claude/projects/), and another session's transcript planted beside
# the drill folders in the --into parent. A drill that opened one would
# fail; each must be there after, unchanged.
#   (1) The bundle unit's run records backup.bundled with a per-file sha256
#       (setup for the rest).
#   (2) The transcripts drill unit runs `restore-drill --only transcripts`,
#       its timer names an OnCalendar. Its run passes (exit 0), says
#       "--only transcripts" and "<n> of <n> files as the newest record's
#       sha256", records exactly one backup.drilled.transcripts {ok true,
#       reason "", the bundle's key and event, 0 mismatches} and no
#       backup.drilled.missions, leaves nothing of its own in the parent,
#       and the originals (the archive, the planted HOME transcript, the
#       other session's) and every source are unchanged by path, size and
#       sha256, mode 000 throughout.
#   (3) With --keep, the kept folder holds the record's transcripts under
#       transcripts/, each at the record's size and sha256, and no
#       repository bundle.
#   (4) After a transcript grows and a new one is written, a second bundle:
#       the drill over the new chain passes, its record naming the new
#       bundle, "<n> of <n>" its new count.
#   (5) With a floor no disk meets (SPAWN_BACKUP_DRILL_FLOOR_MB), the drill
#       refuses: exit 2, one line naming the floor, nothing recorded,
#       nothing made in the parent.
#   (6) The newest bundle's stored bytes replaced by another ciphertext
#       under the same passphrase: the drill fails (exit 1), its record ok
#       false saying the stream decrypts to another sha256; nothing of its
#       own is left in the parent, and with --keep nothing was extracted
#       from it (the chain's links before it may unpack).
#   (7) --into under the fake HOME or the workspace root, and --only repos
#       or an empty value: exit 2, nothing recorded.
#   (8) No secret (the backup passphrase, the R2 secret key) and no
#       transcript's text is in any record or output.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "200: harness: $*" >&2
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
SPAWN_DB_NAME="bb_200_drill_$run"
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
            echo "200: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "200: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "200: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "200: harness: unexpected exit $rc" >&2
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
access_key="bb200$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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
            --text "200 mission $i: back the fleet's record up off this host, every hour, and prove it restores"
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
# Every file of the live archive and the sources: path, size, sha256.
snapshot() {
    for root in "$archive" "$src_spawn" "$src_other" "$src_empty" "$src_tag"; do
        find "$root" -type f | LC_ALL=C sort | while read -r file; do
            printf '%s %s %s\n' "$file" "$(size "$file")" "$(sha "$file")"
        done
    done
}
note() { echo "200: $*" >&2; }
drill() { # drill <log>: run the restore drill into the scratch parent
    mkdir -p "$WORK/drills"
    capture "$SPAWN_BIN" backup restore-drill --into "$WORK/drills"
    cat "$WORK/out.txt" "$WORK/err.txt" >"$1"
    cat "$1" >>"$WORK/backup.log"
}

add_missions 1 4

transcripts_unit=$units/spawn-backup-restore-drill-transcripts.service
transcripts_timer=$units/spawn-backup-restore-drill-transcripts.timer
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


# The originals a drill must never open, besides the live archive: a
# transcript in the fake HOME, and another session's beside the drill
# folders in the --into parent.
planted=$HOME/.claude/projects/-other-project/7a1c2e40-0000-4000-8000-00000000beef.jsonl
mkdir -p "$(dirname "$planted")"
printf '{"type":"user","message":{"role":"user","content":"a session of the owner, never to be opened"}}\n' >"$planted"
mkdir -p "$WORK/drills/another-session"
other=$WORK/drills/another-session/0d5e7f10-0000-4000-8000-00000000cafe.jsonl
printf '{"type":"user","message":{"role":"user","content":"another session, never to be opened"}}\n' >"$other"
originals() { # every original, the sources too: path, size, sha256 (modes put back first)
    for root in "$archive" "$planted" "$other" "$src_spawn" "$src_other" "$src_empty" "$src_tag"; do
        find "$root" -type f | LC_ALL=C sort | while read -r file; do
            printf '%s %s %s\n' "$file" "$(size "$file")" "$(sha "$file")"
        done
    done
}
lock_originals() { # mode 000: a drill that opens one fails
    find "$archive" -type f -exec chmod 000 {} +
    chmod 000 "$planted" "$other"
}
unlock_originals() {
    find "$archive" -type f -exec chmod 600 {} +
    chmod 600 "$planted" "$other"
}
locked_still() { # every original still mode 000
    [ -z "$(find "$archive" "$planted" "$other" -type f ! -perm 000 | head -n 1)" ]
}
# What the drill left in the parent besides the other session's folder.
own_left() { find "$WORK/drills" -mindepth 1 -maxdepth 1 ! -name another-session | head -n 3; }
marker='nothing needs a person'
grep -rqF "$marker" "$archive" || harness "the archive's transcripts do not hold the marker text"
free_kb=$(df -Pk "$WORK" | awk 'NR == 2 { print $4 }')
[ "$free_kb" -gt $((41 * 1024 * 1024)) ] ||
    harness "the file system under $WORK has $((free_kb / 1024)) MB free: the drill's 40 GB floor would refuse every run"

# --- (1) the bundle unit records ----------------------------------------------------
capture unit_run "$units/spawn-backup-bundle.service"
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || harness "the bundle unit's ExecStart exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bun=$(newest backup.bundled)
[ -n "$bun" ] || harness "the bundle unit's run recorded no backup.bundled"
bkey=$(field "$bun" '.keys[0] // empty')
bevent=$(event_of "$bun")
[ -n "$bkey" ] && has_object "$bkey" || harness "the bundle's key [$bkey] is not in the R2 stand-in"
nfiles=$(field "$bun" '.transcripts.sizes | keys | length')
[ "$nfiles" -ge 2 ] || harness "the bundle holds fewer than 2 transcripts"
[ "$(field "$bun" '.transcripts.sha256 | keys | length')" = "$nfiles" ] ||
    harness "the bundle record names no sha256 for each of its $nfiles transcripts"
note "(1) checked: the bundle unit recorded $bkey with $nfiles transcripts"

# --- (2) the transcripts drill unit: the archive alone, judged by its record ---------
[ -f "$transcripts_unit" ] || fail "(2) the checkout has no deploy/systemd/spawn-backup-restore-drill-transcripts.service"
[ -f "$transcripts_timer" ] || fail "(2) the checkout has no deploy/systemd/spawn-backup-restore-drill-transcripts.timer"
grep -qE '^ExecStart=.* backup restore-drill --only transcripts( |$)' "$transcripts_unit" ||
    fail "(2) the transcripts unit's ExecStart is not 'spawn backup restore-drill --only transcripts': $(grep '^ExecStart=' "$transcripts_unit")"
grep -qE '^OnCalendar=.' "$transcripts_timer" || fail "(2) the transcripts timer names no OnCalendar"
originals >"$WORK/originals-before.txt"
lock_originals
capture unit_run "$transcripts_unit" --into "$WORK/drills"
case $status in 3) unlock_originals; fail "(2) the transcripts unit names no ExecStart or EnvironmentFile" ;; esac
locked=yes
locked_still || locked=no
unlock_originals
cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/drill1.log"
cat "$WORK/drill1.log" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || fail "(2) the transcripts unit's run exited $status: $(tail -n 4 "$WORK/drill1.log" | tr '\n' ' ')"
grep -q -- '--only transcripts' "$WORK/drill1.log" ||
    fail "(2) the drill does not say it drills the transcript archive alone: $(head -n 4 "$WORK/drill1.log" | tr '\n' ' ')"
grep -qF "$nfiles of $nfiles files as the newest record's sha256" "$WORK/drill1.log" ||
    fail "(2) the drill does not say \"$nfiles of $nfiles files as the newest record's sha256\": $(grep -i sha256 "$WORK/drill1.log" | head -n 2 | tr '\n' ' ')"
[ "$(count_records backup.drilled.transcripts)" = 1 ] ||
    fail "(2) the transcripts unit's run left $(count_records backup.drilled.transcripts) backup.drilled.transcripts record(s), not 1"
got=$(newest backup.drilled.transcripts | cut -d'|' -f2- | jq -c '{ok, reason, bundle, bundle_event_id, mismatches}')
want="{\"ok\":true,\"reason\":\"\",\"bundle\":\"$bkey\",\"bundle_event_id\":$bevent,\"mismatches\":0}"
[ "$got" = "$want" ] || fail "(2) backup.drilled.transcripts: expected $want, got $got"
[ "$(count_records backup.drilled.missions)" = 0 ] ||
    fail "(2) a transcripts-only drill recorded $(count_records backup.drilled.missions) backup.drilled.missions record(s), not 0"
[ -z "$(own_left)" ] || fail "(2) the drill left in the parent: $(own_left)"
[ "$locked" = yes ] || fail "(2) an original's mode changed while the drill ran"
originals >"$WORK/originals-after.txt"
cmp -s "$WORK/originals-before.txt" "$WORK/originals-after.txt" ||
    fail "(2) an original changed or went: $(diff "$WORK/originals-before.txt" "$WORK/originals-after.txt" | head -n 4 | tr '\n' ' ')"
note "(2) checked: the transcripts unit restored the archive alone, judged it by its record, opened no original and removed only its own folder"

# --- (3) the kept folder holds the record's transcripts, at its sha256 -----------------
lock_originals
drill_only "$WORK/drill2.log" --keep --only transcripts
unlock_originals
[ "$status" -eq 0 ] || fail "(3) the kept transcripts-only drill exited $status: $(tail -n 4 "$WORK/drill2.log" | tr '\n' ' ')"
kept=$(find "$WORK/drills" -mindepth 1 -maxdepth 1 -type d -name 'spawn-restore-drill-*' | head -n 1)
[ -n "$kept" ] || harness "--keep left no drill folder under $WORK/drills"
[ -z "$(find "$kept" -path '*/repos/*' | head -n 1)" ] || fail "(3) the transcripts-only drill unpacked a repository bundle"
rec_t=$(newest backup.bundled | cut -d'|' -f2-)
for path in $(printf '%s' "$rec_t" | jq -r '.transcripts.sizes | keys[]'); do
    file=$kept/transcripts/$path
    [ -f "$file" ] || fail "(3) the kept folder holds no transcripts/$path"
    [ "$(size "$file")" = "$(printf '%s' "$rec_t" | jq -r --arg p "$path" '.transcripts.sizes[$p]')" ] ||
        fail "(3) transcripts/$path is not the record's size"
    [ "$(sha "$file")" = "$(printf '%s' "$rec_t" | jq -r --arg p "$path" '.transcripts.sha256[$p]')" ] ||
        fail "(3) transcripts/$path is not the record's sha256"
done
chmod -R u+rwX "$kept" && rm -rf "$kept"
note "(3) checked: the kept folder holds each of the record's transcripts at its size and sha256, and no repository bundle"

# --- (4) a second bundle over a grown and a new transcript -------------------------
printf '{"type":"user","uuid":"s-grown","message":{"role":"user","content":"one more line"}}\n' \
    >>"$archive/agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0001.jsonl"
mkdir -p "$archive/agent_worker_objective_9"
printf '{"type":"user","uuid":"n-1","message":{"role":"user","content":"a new session"}}\n' \
    >"$archive/agent_worker_objective_9/9c0d1e20-0000-4000-8000-000000000009.jsonl"
capture unit_run "$units/spawn-backup-bundle.service"
cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
[ "$status" -eq 0 ] || harness "the second bundle exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bun2=$(newest backup.bundled)
bkey2=$(field "$bun2" '.keys[0] // empty')
[ -n "$bkey2" ] && [ "$bkey2" != "$bkey" ] || harness "the second bundle stored no new object"
nfiles2=$(field "$bun2" '.transcripts.sizes | keys | length')
originals >"$WORK/originals-before.txt"
lock_originals
drill_only "$WORK/drill3.log" --only transcripts
unlock_originals
[ "$status" -eq 0 ] || fail "(4) the drill over the second bundle exited $status: $(tail -n 4 "$WORK/drill3.log" | tr '\n' ' ')"
grep -qF "$nfiles2 of $nfiles2 files as the newest record's sha256" "$WORK/drill3.log" ||
    fail "(4) the drill does not say \"$nfiles2 of $nfiles2 files as the newest record's sha256\""
[ "$(newest backup.drilled.transcripts | cut -d'|' -f2- | jq -r '.ok, .bundle' | tr '\n' ' ')" = "true $bkey2 " ] ||
    fail "(4) the record over the second bundle: $(newest backup.drilled.transcripts | cut -d'|' -f2- | head -c 300)"
[ -z "$(own_left)" ] || fail "(4) the drill left in the parent: $(own_left)"
originals >"$WORK/originals-after.txt"
cmp -s "$WORK/originals-before.txt" "$WORK/originals-after.txt" || fail "(4) an original changed or went"
note "(4) checked: the drill over the newer chain passes, $nfiles2 of $nfiles2 files"

# --- (5) the free-space floor ---------------------------------------------------------
before=$(backup_records)
(
    SPAWN_BACKUP_DRILL_FLOOR_MB=999999999
    export SPAWN_BACKUP_DRILL_FLOOR_MB
    exec "$SPAWN_BIN" backup restore-drill --into "$WORK/drills" --only transcripts
) >"$WORK/out.txt" 2>"$WORK/err.txt" && status=0 || status=$?
cat "$WORK/out.txt" "$WORK/err.txt" >"$WORK/drill4.log"
cat "$WORK/drill4.log" >>"$WORK/backup.log"
[ "$status" -eq 2 ] || fail "(5) the drill under a floor no disk meets exited $status, not 2: $(tail -n 2 "$WORK/drill4.log" | tr '\n' ' ')"
grep -qi 'floor' "$WORK/err.txt" || fail "(5) the refusal does not name the floor: $(head -n 2 "$WORK/err.txt" | tr '\n' ' ')"
[ "$(backup_records)" = "$before" ] || fail "(5) the refused drill recorded a drill"
[ -z "$(own_left)" ] || fail "(5) the refused drill made something in the parent: $(own_left)"
note "(5) checked: a drill that would go under the floor refuses before it fetches anything"

# --- (6) a bundle whose decrypted stream is not its record's sha256 -----------------
obj=$(object "$bkey2")
cp "$obj" "$WORK/object.keep"
mkdir -p "$WORK/fake/transcripts/agent_x"
printf '{"kind":"full"}\n' >"$WORK/fake/manifest.json"
printf 'not the bundled bytes\n' >"$WORK/fake/transcripts/agent_x/x.jsonl"
tar -C "$WORK/fake" -cf "$WORK/fake.tar" ./manifest.json ./transcripts/agent_x/x.jsonl
gpg --batch --yes --quiet --pinentry-mode loopback --passphrase-file "$WORK/backup.key" \
    --symmetric --cipher-algo AES256 --output "$obj" "$WORK/fake.tar" ||
    harness "cannot write the replaced object"
drill_only "$WORK/drill5.log" --only transcripts
[ "$status" -eq 1 ] || fail "(6) the drill over a replaced object exited $status, not 1: $(tail -n 3 "$WORK/drill5.log" | tr '\n' ' ')"
rec=$(newest backup.drilled.transcripts)
[ "$(field "$rec" '.ok')" = false ] && field "$rec" '.reason' | grep -qF "decrypts to sha256" ||
    fail "(6) the transcripts record over a replaced object: $(printf '%s' "$rec" | cut -d'|' -f2- | head -c 400)"
[ -z "$(own_left)" ] || fail "(6) the failed drill left in the parent: $(own_left)"
drill_only "$WORK/drill6.log" --keep --only transcripts
[ "$status" -eq 1 ] || fail "(6) the kept drill over a replaced object exited $status, not 1"
kept=$(find "$WORK/drills" -mindepth 1 -maxdepth 1 -type d -name 'spawn-restore-drill-*' | head -n 1)
[ -n "$kept" ] || harness "--keep left no drill folder under $WORK/drills"
# The replaced object is the chain's newest link: its folder is the last
# bundle-<n> (the links before it are their records' and may unpack).
last=$(find "$kept" -mindepth 1 -maxdepth 1 -type d -name 'bundle-*' | sed 's#.*/bundle-##' | sort -n | tail -n 1)
[ -n "$last" ] || harness "the kept drill folder holds no bundle-<n> folder"
extracted=$(find "$kept/bundle-$last" -path '*/unpacked/*' -type f | head -n 3)
[ -z "$extracted" ] || fail "(6) the drill extracted from a stream that is not the record's sha256: $extracted"
chmod -R u+rwX "$kept" && rm -rf "$kept"
cp "$WORK/object.keep" "$obj"
note "(6) checked: a bundle whose stream is not its record's sha256 fails the drill, nothing extracted"

# --- (7) refused --into and --only ----------------------------------------------------
before=$(backup_records)
mkdir -p "$HOME/drills" "$SPAWN_WORKSPACE_ROOT/drills"
for into in "$HOME/drills" "$SPAWN_WORKSPACE_ROOT/drills"; do
    capture "$SPAWN_BIN" backup restore-drill --into "$into" --only transcripts
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
    [ "$status" -eq 2 ] || fail "(7) --into $into exited $status, not 2"
    [ -z "$(find "$into" -mindepth 1 | head -n 1)" ] || fail "(7) a refused --into $into holds something"
done
for value in repos ""; do
    drill_only "$WORK/drill7.log" --only "$value"
    [ "$status" -eq 2 ] || fail "(7) --only '$value' exited $status, not 2: $(tail -n 2 "$WORK/drill7.log" | tr '\n' ' ')"
done
[ "$(backup_records)" = "$before" ] || fail "(7) a refused run recorded a drill"
note "(7) checked: --into under HOME or the workspace root, and --only repos or '', are refused with nothing recorded"

# --- (8) no secret, no transcript text ------------------------------------------------
leak=$(secrets_leaked)
[ -z "$leak" ] || fail "(8) a secret (the passphrase or the R2 secret key) shows in $leak"
if grep -qF "$marker" "$WORK/backup.log"; then fail "(8) a transcript's text shows in the output"; fi
[ "$(q "select count(*) from spawn_events where kind::text like '%$marker%'")" = 0 ] ||
    fail "(8) a transcript's text shows in a record"
note "(8) checked: no secret and no transcript's text in a record or the output"

echo "200: the transcript archive's restore drill restores it alone, judges it by its record, opens no original, and removes only its own folder"
