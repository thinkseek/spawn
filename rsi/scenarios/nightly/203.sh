#!/bin/sh
# Objective 203 [recoverability] (cc-astra A84 HIGH on objective 200, the
# lead's contract of 2026-11-15): the transcript archive's restore drill
# (`spawn backup restore-drill`, every mode: full, --only missions, --only
# transcripts) judges each unpacked bundle's tree before it reads anything
# of it, the bundle's manifest first, and reads a manifest
# only through a no-symlink, regular-file open. A bundle whose recorded
# sha256 matches, holding a manifest.json that is a symlink to a file
# outside the drill folder, is refused without that file ever being opened.
#
# Black box, on its own database, scratch HOME and GNUPGHOME, against 104's
# local R2 stand-in (200.sh's harness, kept as it is up to its step (1)).
#   (1) The bundle unit's run records backup.bundled (setup).
#   (2) The newest bundle's stored object is replaced by a crafted bundle,
#       encrypted under the same passphrase: ./manifest.json is a symlink
#       to a FIFO outside the drill folder ($WORK/outside/sentinel.fifo),
#       beside one regular transcript slice. Its record's sha256 and the
#       object's x-amz-meta-sha256 are set to the crafted stream's, so every
#       integrity check passes and only the tree's types can refuse it.
#       A witness waits to open the FIFO for writing: its open returns only
#       when something opens the FIFO for reading, and then it writes a
#       marker. So "never opened" is observed, not inferred from an exit.
#       The drill exits 1 (in under 120 s, never waiting on the FIFO); its
#       backup.drilled.transcripts record is ok false, its reason names
#       manifest.json as a symlink; the witness's marker is absent and the
#       witness still waits; the FIFO is still there.
#   (3) The same with a regular manifest.json of the record's kind, naming
#       two transcript slices, one of which is the symlink to the FIFO
#       (y.jsonl, the manifest's own transcript entry): the drill exits 1,
#       its reason names that slice as a symlink, and the FIFO is never
#       opened (each unpacked tree is judged before any file of it is read,
#       not only its manifest).
#   R2 (every drill, cc-astra A84's class in the other modes; the lead
#   2026-11-16): the same witness on each.
#   (4) The full drill (no --only) over (3)'s bundle: the full drill
#       restores the transcripts first; it exits 1, its
#       backup.drilled.transcripts reason names y.jsonl as a symlink, and
#       the FIFO is never opened.
#   (5) --only missions over a bundle whose repository bundle (the file the
#       record names first under repos/) is a symlink to the FIFO, beside a
#       regular manifest of the record's kind: exits 1, its
#       backup.drilled.missions reason names that file as a symlink, and git
#       never opens the FIFO.
#   (6) (2)'s bundle (manifest.json a symlink) under --only missions and
#       under the full drill: each exits 1, its backup.drilled.missions
#       reason names manifest.json as a symlink, and the FIFO is never
#       opened.
# On the base (d73e9ef0's drill, 200 as landed), read_manifest's fs::read
# follows the symlink before plain_tree runs: the FIFO is opened, the marker
# is written, and (2) fails.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "203: harness: $*" >&2
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
    set +e
    if [ "$rc" -ne 0 ]; then
        if [ -s "$R2/requests.log" ]; then
            echo "203: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "203: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "203: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "203: harness: unexpected exit $rc" >&2
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
note() { echo "203: $*" >&2; }
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

# --- (2) a bundle whose manifest.json is a symlink out of the drill folder ----------
obj=$(object "$bkey")
mkdir -p "$WORK/outside" "$WORK/evil/transcripts/agent_x"
fifo=$WORK/outside/sentinel.fifo
mkfifo "$fifo" || harness "mkfifo failed"
ln -s "$fifo" "$WORK/evil/manifest.json" || harness "ln -s failed"
printf '{"type":"user"}\n' >"$WORK/evil/transcripts/agent_x/x.jsonl"
tar -C "$WORK/evil" -cf "$WORK/evil.tar" ./manifest.json ./transcripts/agent_x/x.jsonl ||
    harness "cannot write the crafted bundle"
tar -tvf "$WORK/evil.tar" | grep -q '^l.* \./manifest\.json -> ' ||
    harness "the crafted bundle holds no manifest.json symlink"
evil_sha=$(sha "$WORK/evil.tar")
gpg --batch --yes --quiet --pinentry-mode loopback --passphrase-file "$WORK/backup.key" \
    --symmetric --cipher-algo AES256 --output "$obj" "$WORK/evil.tar" ||
    harness "cannot write the crafted object"
jq --arg s "$evil_sha" --argjson n "$(size "$obj")" \
    '.meta |= with_entries(if (.key | ascii_downcase) == "x-amz-meta-sha256" then .value = $s else . end) | .size = $n' \
    "$obj.json" >"$obj.json.new" && mv "$obj.json.new" "$obj.json" || harness "cannot set the crafted object's metadata"
[ "$(meta "$bkey" x-amz-meta-sha256)" = "$evil_sha" ] || harness "the crafted object's x-amz-meta-sha256 is not its stream's"
psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "update spawn_events
    set kind = jsonb_set(kind, '{detail,record,sha256}', to_jsonb('$evil_sha'::text))
    where id = $bevent" >/dev/null || harness "cannot set the bundle record's sha256 to the crafted stream's"
[ "$(field "$(newest backup.bundled)" '.sha256')" = "$evil_sha" ] || harness "the bundle record's sha256 is not the crafted stream's"
# The witness: its open of the FIFO for writing returns only once a reader
# opens it; then it leaves a marker.
( exec 3>"$fifo"; : >"$WORK/outside/opened" ) &
witness=$!
sleep 1
[ ! -e "$WORK/outside/opened" ] || harness "the witness got through before the drill ran"
started=$(date +%s)
drill_only "$WORK/drill-a84.log" --only transcripts
took=$(($(date +%s) - started))
sleep 1
opened=no
[ -e "$WORK/outside/opened" ] && opened=yes
waiting=no
kill -0 "$witness" 2>/dev/null && waiting=yes
kill "$witness" 2>/dev/null || true
wait "$witness" 2>/dev/null || true
[ "$opened" = no ] ||
    fail "(2) the FIFO outside the drill folder was opened through the bundle's manifest.json symlink (the witness got through)"
[ "$waiting" = yes ] || fail "(2) the witness no longer waits on the FIFO: something opened it"
[ -p "$fifo" ] || fail "(2) the FIFO outside the drill folder is gone"
[ "$status" -eq 1 ] ||
    fail "(2) the drill over a bundle with a symlinked manifest exited $status, not 1: $(tail -n 3 "$WORK/drill-a84.log" | tr '\n' ' ')"
[ "$took" -lt 120 ] || fail "(2) the drill took $took s: it waited on something"
rec=$(newest backup.drilled.transcripts)
[ "$(field "$rec" '.ok')" = false ] ||
    fail "(2) the drill over a bundle with a symlinked manifest recorded ok $(field "$rec" '.ok')"
reason=$(field "$rec" '.reason')
case $reason in
    *manifest.json*symlink*) ;;
    *) fail "(2) the record's reason does not name manifest.json as a symlink: $(printf '%s' "$reason" | head -c 300)" ;;
esac
# A plain-only drill folder holding a symlink is left (200's rule): said,
# then removed here.
find "$WORK/drills" -mindepth 1 -maxdepth 1 -type d -name 'spawn-restore-drill-*' | while read -r left; do
    chmod -R u+rwX "$left" && rm -rf "$left"
done
note "(2) checked: a bundle with a symlinked manifest.json and matching sha256 is refused, and the file it points to is never opened"

# --- (3) a regular manifest, a transcript slice that is a symlink out --------------
rm -rf "$WORK/evil" "$WORK/outside/opened"
mkdir -p "$WORK/evil/transcripts/agent_x"
# The manifest names the symlinked slice as a transcript, so a drill that
# reads slices before judging the tree would open it.
printf '{"kind":"%s","transcripts":[{"path":"agent_x/x.jsonl","offset":0},{"path":"agent_x/y.jsonl","offset":0}]}\n' \
    "$(field "$(newest backup.bundled)" '.kind')" >"$WORK/evil/manifest.json"
printf '{"type":"user"}\n' >"$WORK/evil/transcripts/agent_x/x.jsonl"
ln -s "$fifo" "$WORK/evil/transcripts/agent_x/y.jsonl" || harness "ln -s failed"
tar -C "$WORK/evil" -cf "$WORK/evil.tar" ./manifest.json ./transcripts/agent_x/x.jsonl ./transcripts/agent_x/y.jsonl ||
    harness "cannot write the second crafted bundle"
evil_sha=$(sha "$WORK/evil.tar")
gpg --batch --yes --quiet --pinentry-mode loopback --passphrase-file "$WORK/backup.key" \
    --symmetric --cipher-algo AES256 --output "$obj" "$WORK/evil.tar" ||
    harness "cannot write the second crafted object"
jq --arg s "$evil_sha" --argjson n "$(size "$obj")" \
    '.meta |= with_entries(if (.key | ascii_downcase) == "x-amz-meta-sha256" then .value = $s else . end) | .size = $n' \
    "$obj.json" >"$obj.json.new" && mv "$obj.json.new" "$obj.json" || harness "cannot set the second crafted object's metadata"
psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "update spawn_events
    set kind = jsonb_set(kind, '{detail,record,sha256}', to_jsonb('$evil_sha'::text))
    where id = $bevent" >/dev/null || harness "cannot set the bundle record's sha256 to the second crafted stream's"
( exec 3>"$fifo"; : >"$WORK/outside/opened" ) &
witness=$!
sleep 1
[ ! -e "$WORK/outside/opened" ] || harness "the second witness got through before the drill ran"
drill_only "$WORK/drill-a84-slice.log" --only transcripts
sleep 1
opened=no
[ -e "$WORK/outside/opened" ] && opened=yes
waiting=no
kill -0 "$witness" 2>/dev/null && waiting=yes
kill "$witness" 2>/dev/null || true
wait "$witness" 2>/dev/null || true
[ "$opened" = no ] && [ "$waiting" = yes ] ||
    fail "(3) the FIFO outside the drill folder was opened through a transcript slice's symlink"
[ "$status" -eq 1 ] ||
    fail "(3) the drill over a bundle with a symlinked transcript slice exited $status, not 1: $(tail -n 3 "$WORK/drill-a84-slice.log" | tr '\n' ' ')"
reason=$(field "$(newest backup.drilled.transcripts)" '.reason')
case $reason in
    *y.jsonl*symlink*) ;;
    *) fail "(3) the record's reason does not name the transcript slice as a symlink: $(printf '%s' "$reason" | head -c 300)" ;;
esac
find "$WORK/drills" -mindepth 1 -maxdepth 1 -type d -name 'spawn-restore-drill-*' | while read -r left; do
    chmod -R u+rwX "$left" && rm -rf "$left"
done
note "(3) checked: a symlinked transcript slice is refused before any file of its tree is read"

# --- R2: every drill judges the tree before it reads it ---------------------------
# plant <member ...>: $WORK/evil's members as the newest bundle's object,
# encrypted under the same passphrase, its record's and its object's
# sha256 the crafted stream's (only the tree's types can refuse it).
plant() {
    tar -C "$WORK/evil" -cf "$WORK/evil.tar" "$@" || harness "cannot write a crafted bundle"
    evil_sha=$(sha "$WORK/evil.tar")
    gpg --batch --yes --quiet --pinentry-mode loopback --passphrase-file "$WORK/backup.key" \
        --symmetric --cipher-algo AES256 --output "$obj" "$WORK/evil.tar" ||
        harness "cannot write a crafted object"
    jq --arg s "$evil_sha" --argjson n "$(size "$obj")" \
        '.meta |= with_entries(if (.key | ascii_downcase) == "x-amz-meta-sha256" then .value = $s else . end) | .size = $n' \
        "$obj.json" >"$obj.json.new" && mv "$obj.json.new" "$obj.json" || harness "cannot set a crafted object's metadata"
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "update spawn_events
        set kind = jsonb_set(kind, '{detail,record,sha256}', to_jsonb('$evil_sha'::text))
        where id = $bevent" >/dev/null || harness "cannot set the bundle record's sha256 to a crafted stream's"
}
# watched <check> <log> <record type> <reason pattern> [drill arg ...]: the
# drill with a witness on the FIFO; it must exit 1 within 120 s, never open
# the FIFO, and record <record type> ok false with a reason matching
# <reason pattern> (a case pattern).
watched() {
    w_check=$1 w_log=$2 w_type=$3 w_reason=$4
    shift 4
    rm -f "$WORK/outside/opened"
    ( exec 3>"$fifo"; : >"$WORK/outside/opened" ) &
    witness=$!
    sleep 1
    [ ! -e "$WORK/outside/opened" ] || harness "$w_check: the witness got through before the drill ran"
    w_before=$(backup_records)
    w_started=$(date +%s)
    drill_only "$w_log" "$@"
    w_took=$(($(date +%s) - w_started))
    sleep 1
    opened=no
    [ -e "$WORK/outside/opened" ] && opened=yes
    waiting=no
    kill -0 "$witness" 2>/dev/null && waiting=yes
    kill "$witness" 2>/dev/null || true
    wait "$witness" 2>/dev/null || true
    [ "$opened" = no ] && [ "$waiting" = yes ] ||
        fail "$w_check the FIFO outside the drill folder was opened through the crafted bundle (drill $*)"
    [ -p "$fifo" ] || fail "$w_check the FIFO outside the drill folder is gone"
    [ "$status" -eq 1 ] ||
        fail "$w_check the drill ($*) exited $status, not 1: $(tail -n 3 "$w_log" | tr '\n' ' ')"
    [ "$w_took" -lt 120 ] || fail "$w_check the drill ($*) took $w_took s: it waited on something"
    [ "$(backup_records)" -gt "$w_before" ] || fail "$w_check the drill ($*) recorded no backup.drilled record"
    w_rec=$(newest "$w_type")
    [ "$(field "$w_rec" '.ok')" = false ] || fail "$w_check the drill ($*) recorded $w_type ok $(field "$w_rec" '.ok')"
    w_got=$(field "$w_rec" '.reason')
    # shellcheck disable=SC2254
    case $w_got in
        $w_reason) ;;
        *) fail "$w_check the $w_type reason does not name the symlink: $(printf '%s' "$w_got" | head -c 300)" ;;
    esac
    find "$WORK/drills" -mindepth 1 -maxdepth 1 -type d -name 'spawn-restore-drill-*' | while read -r left; do
        chmod -R u+rwX "$left" && rm -rf "$left"
    done
}

# (4) the full drill (no --only) over (3)'s bundle: a transcript slice the
# manifest names is a symlink to the FIFO. The full drill restores the
# transcripts first; it must refuse the tree before it opens the slice.
watched "(4)" "$WORK/drill-r2-full-slice.log" backup.drilled.transcripts '*y.jsonl*symlink*'
note "(4) checked: the full drill refuses a symlinked transcript slice before opening it"

# (5) --only missions over a bundle whose repository bundle (the one the
# record names first) is a symlink to the FIFO, beside a regular manifest
# of the record's kind: refused, the FIFO never opened by git.
repo_file=$(field "$(newest backup.bundled)" '.repos[0].file // empty')
case $repo_file in
    repos/?*) ;;
    *) harness "the bundle record names no repository bundle file [$repo_file]" ;;
esac
rm -rf "$WORK/evil"
mkdir -p "$WORK/evil/repos"
printf '{"kind":"%s","transcripts":[]}\n' "$(field "$(newest backup.bundled)" '.kind')" >"$WORK/evil/manifest.json"
ln -s "$fifo" "$WORK/evil/$repo_file" || harness "ln -s failed"
plant ./manifest.json "./$repo_file"
watched "(5)" "$WORK/drill-r2-missions-repo.log" backup.drilled.missions "*${repo_file##*/}*symlink*" --only missions
note "(5) checked: --only missions refuses a symlinked repository bundle before git opens it"

# (6) (2)'s bundle (manifest.json a symlink to the FIFO) under --only
# missions and under the full drill: refused, naming manifest.json.
rm -rf "$WORK/evil"
mkdir -p "$WORK/evil/transcripts/agent_x"
ln -s "$fifo" "$WORK/evil/manifest.json" || harness "ln -s failed"
printf '{"type":"user"}\n' >"$WORK/evil/transcripts/agent_x/x.jsonl"
plant ./manifest.json ./transcripts/agent_x/x.jsonl
watched "(6)" "$WORK/drill-r2-missions-manifest.log" backup.drilled.missions '*manifest.json*symlink*' --only missions
watched "(6)" "$WORK/drill-r2-full-manifest.log" backup.drilled.missions '*manifest.json*symlink*'
note "(6) checked: --only missions and the full drill refuse a symlinked manifest.json before opening it"

echo "203: every restore drill judges a bundle's tree before it reads anything of it: a symlinked manifest, transcript slice or repository bundle is refused and its target never opened"
