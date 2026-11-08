#!/bin/sh
# Black box: the event log is backed up off the host every hour, the mission
# repositories and the transcripts every day (a full bundle a week, an
# increment on the other days), encrypted, in short windows that keep R2
# in its free tier, with an alert at 8 GB and nothing ever written past
# 10 GB (the owner's ruling, 2026-11-06), and a restore drill proves the
# latest dump restores and holds what its record says. Today one
# `pg-backup.sh` run a day (spawn-backup.timer, 03:30 CST, 19:30 UTC the day
# before, + up to 45 min) dumps the database to R2; nothing backs up the
# mission repositories or the transcripts, and the drill
# (deploy/backup/pg-restore-drill.sh) is run by hand, records nothing, and
# compares with the live counts of the moment.
# At 16:30 CST (08:30 UTC) on 2026-11-06 the newest copy in R2 (taken at
# 03:50 CST, 19:50 UTC on 11-05: postgres/daily/spawn-20261105-195031, 8,106
# events) held 34 % of the log (23,868 events in deploy.sh's dump of 16:30
# CST): a host lost then loses 15,762 events, 12 h 40 min of the record.
#
# Everything runs against a local stand-in for R2 (python3 on loopback: S3
# PUT, GET, HEAD and ListObjectsV2, a valid Signature Version 4 required on
# every request, at most 4 keys per listing page, DELETE and overwrites under
# postgres/ and bundles/ refused as the bucket's locks refuse them), a
# database of its own on SPAWN_TEST_DATABASE_URL filled with control records
# by real `spawn mission add` commands (its id sequence skips 1,000 values on
# the way, as a rolled-back append makes it skip, so the id watermark is not
# the row count), a throwaway passphrase file and R2 credentials file (mode
# 600), two mission sources (spawn.git with main and an agent's branch,
# other.git), and a transcript archive (<workspace root>/.transcripts, two
# agents written now and a third's file last written 20 days ago). HOME is
# a scratch directory: nothing here can read the owner's ~/.config/spawn.
# Checked:
#   (1) `spawn backup dump` exits 0 and records `backup.dumped`; its keys
#       exist in the stand-in, the first under postgres/hourly/ and one under
#       postgres/daily/ (the day's first dump); the stored object is no
#       pg_dump archive (no PGDMP), decrypts with the passphrase file (gpg,
#       symmetric) to one whose sha256 is the record's and the object's
#       x-amz-meta-sha256, whose size is the record's `bytes`
#       (`stored_bytes` the object's); the events and the highest event id
#       in that archive are the record's `events` and `max_event_id`, and at
#       least those the database held before the run; `held_bytes` is what
#       the bucket held before it; every request was signed, none deleted;
#       neither secret appears in the output or the database.
#   (2) Hourly: after more records, a second dump the same UTC day stores
#       one new object, under postgres/hourly/ only, holding more events
#       than the first; its `held_bytes` is the bucket's total before it.
#   (3) `spawn backup bundle` exits 0 and records `backup.bundled` kind
#       `full` (none before it): its key under bundles/full/, encrypted with
#       the same passphrase, a tar (plain, gzip or zstd) holding
#       manifest.json, for each mission source a git bundle that verifies
#       alone and lists exactly the source's branches at their commits (the
#       agent's work is in its branch), and every transcript file modified
#       within 14 days under transcripts/<agent>/<file>, the same bytes, and
#       not the 20-day-old one; the record names both sources and the
#       window's file count and bytes. Then one transcript grows, a new one
#       appears, the agent's branch moves and other.git does not: a second
#       bundle the same day records kind `incremental`, `base_event_id` the
#       full one's, one object under bundles/incr/, holding the grown file's
#       appended bytes alone (the manifest's offset its old size), the new
#       file whole (offset 0), nothing of the unchanged one; a git bundle of
#       spawn.git with prerequisites that, fetched after the full one's,
#       gives exactly the source's branches, and other.git `unchanged`; the
#       full bundle and the increment give the window back byte for byte.
#       other.git also carries refs/remotes/github/main, as the live
#       missions/supply-pa.git does: it is not the mission's history, so the
#       bundle lists no ref the manifest does not name and the record names
#       it not. A
#       drill after the full bundle exits 0 and records `.bundle.kind` full.
#   (4) After more records (the database now holds more events than the
#       dump), `spawn backup verify` exits 0 and records `backup.verified`
#       ok, naming the newest dump's key and its record's event id, with the
#       restored `events` and `max_event_id` equal to that record's (not to
#       the database's now); it restored into a database it created (named
#       in the record, absent before and after), never the source, whose
#       other records did not change, and names no password; `.bundle` is
#       the newest bundle opened — a full one, or the increment unless the
#       ISO week turned between the two bundles.
#   (5) Without SPAWN_BACKUP_DRILL_DATABASE_URL the drill refuses (exit 2,
#       `backup.verified` not ok, the setting named) and touches the source
#       not. With the newest dump's stored objects corrupted (one byte
#       flipped in the middle of each), the drill fails: exit 1, a
#       `backup.verified` not ok with its reason, no scratch database left.
#   (6) The alert and the limit, with a 1.5 MB object in the bucket: with
#       SPAWN_BACKUP_R2_ALERT_MB=1 a dump stores its hourly copy and records
#       `over_alert` true, `alert_bytes` 1000000 and `limit_bytes`
#       10000000000 (the default); with SPAWN_BACKUP_R2_LIMIT_MB=1 as well,
#       the bundle stores nothing under bundles/ and records the skipped tier
#       with the limit, and the dump writes nothing at all (no PUT), exits 4
#       and records no keys and the hourly tier skipped for the limit.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1. BB_031_MEASURE=1 (only to show today's behaviour)
# reports every failed check and goes on, then exits 1 if any failed; with
# no `spawn backup` it measures what today's tools give on the same world.
#
# Fails with 1 at (1) on the live main (b950e25; the deployed binary is built
# from it): "spawn: unknown command backup". It passes once `spawn
# backup dump|bundle|verify` exist as objective 031 describes. Runs inside
# the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no network
# but loopback. Every wait is bounded in time.
#
# Today (six plain runs and four with BB_031_MEASURE=1, 2026-11-06): (1)
# fails after the setup (120 `mission add`s), in 2.9 s on a quiet host and
# up to 17.3 s on a loaded one.
# With BB_031_MEASURE=1 the same world gives: the host's pg_dump 18.6 -Fc of
# the scenario's database (120 events, max id 1120) 94-111 ms, about 130 KB
# (the schema, mostly), -Z zstd:9 about 129 KB; `pg_restore
# --exit-on-error` 18.6 into the PostgreSQL 16.15 test server fails at once
# (ERROR: unrecognized configuration parameter "transaction_timeout", from
# the `SET transaction_timeout = 0;` pg_restore 17+ writes), and the
# archive's SQL without that one line restores in 97-116 ms; git bundles of
# the two sources 1,058-1,060 bytes; the transcripts, 319,613 bytes, 13,901
# bytes with tar and gzip -6 (324,195 and 14,579 since the archive also
# holds a 20-day-old file, one measure run at 18:19 CST); the stand-in
# received no request.
# A reference of the contract in shell (curl --aws-sigv4, gpg, pg_dump,
# git bundle, tar) passes every check in 10.5 s, on the host and in a
# sandbox like the gate's; variants of it that compare the drill with the
# live counts, record the row count as the watermark, ignore the budget,
# upload the dump unencrypted, or bundle main alone each fail at their check.
# That was the contract before the owner's ruling (task 96); the reference
# updated to it (full and incremental bundles, the limit, the alert) passes
# every check as they now stand, and variants of it that bundle every
# increment whole, write past the limit or never set `over_alert` fail at
# (3) and (6).
set -eu

asserted=""
harness() {
    echo "031: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE=${BB_031_MEASURE-}

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql pg_dump pg_restore gpg git jq od sha256sum python3 tar gzip awk sed cmp; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_031_backup_$run"
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
R2=$WORK/r2
GNUPGHOME=$WORK/gnupg
export GNUPGHOME
cleanup() {
    if [ -n "$r2_pid" ]; then
        kill "$r2_pid" 2>/dev/null || true
        wait "$r2_pid" 2>/dev/null || true
    fi
    if command -v gpgconf >/dev/null 2>&1 && [ -d "$GNUPGHOME" ]; then
        gpgconf --homedir "$GNUPGHOME" --kill gpg-agent >/dev/null 2>&1 || true
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
            echo "031: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "031: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "031: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "031: harness: unexpected exit $rc" >&2
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
# A check that BB_031_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "031: (measure) would fail: $*"
    else
        fail "$*"
    fi
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
access_key="bb031$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
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
            --text "031 mission $i: back the fleet's record up off this host, every hour, and prove it restores"
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

# What today's tools give on the same world (BB_031_MEASURE=1 without a
# `spawn backup`): the numbers in the header.
measure_today() {
    mkdir -p "$WORK/today"
    started=$(now_ms)
    pg_dump -Fc -d "$SPAWN_DB_NAME" -f "$WORK/today/a.dump"
    echo "031: (measure) today: pg_dump $(pg_dump --version | awk '{print $NF}') -Fc: $(size "$WORK/today/a.dump") bytes in $(($(now_ms) - started)) ms ($(events_now | cut -d' ' -f1) events)"
    if pg_dump -Fc -Z zstd:9 -d "$SPAWN_DB_NAME" -f "$WORK/today/z.dump" 2>/dev/null; then
        echo "031: (measure) today: with -Z zstd:9: $(size "$WORK/today/z.dump") bytes"
    fi
    scratch="bb_031_today_$run"
    psql -d postgres -tA -c "create database $scratch" >/dev/null
    if pg_restore --no-owner --no-acl --exit-on-error -d "$scratch" "$WORK/today/a.dump" 2>"$WORK/today/restore.err"; then
        echo "031: (measure) today: pg_restore $(pg_restore --version | awk '{print $NF}') --exit-on-error restores into the test server"
    else
        echo "031: (measure) today: pg_restore $(pg_restore --version | awk '{print $NF}') --exit-on-error into the test server ($(psql -d postgres -tA -c 'show server_version')) fails: $(grep -m1 'ERROR' "$WORK/today/restore.err")"
    fi
    psql -d postgres -tA -c "drop database $scratch with (force)" >/dev/null
    psql -d postgres -tA -c "create database $scratch" >/dev/null
    started=$(now_ms)
    pg_restore --no-owner --no-acl -f - "$WORK/today/a.dump" | sed '/^SET transaction_timeout = 0;$/d' |
        psql -X -q -v ON_ERROR_STOP=1 --single-transaction -d "$scratch" >/dev/null
    echo "031: (measure) today: the archive's SQL without SET transaction_timeout restores in $(($(now_ms) - started)) ms: $(psql -d "$scratch" -tA -c 'select count(*) || '"' events, max id '"' || max(id) from spawn_events')"
    psql -d postgres -tA -c "drop database $scratch with (force)" >/dev/null
    git --git-dir="$src_spawn" bundle create -q "$WORK/today/spawn.bundle" --all
    git --git-dir="$src_other" bundle create -q "$WORK/today/other.bundle" --all
    echo "031: (measure) today: git bundles of the two sources: $(($(size "$WORK/today/spawn.bundle") + $(size "$WORK/today/other.bundle"))) bytes"
    raw=$(find "$archive" -type f -exec cat {} + | wc -c | tr -d ' ')
    gz=$(tar -C "$archive" -cf - . | gzip -6 | wc -c | tr -d ' ')
    echo "031: (measure) today: the transcripts, $raw bytes, are $gz bytes with tar and gzip -6"
    echo "031: (measure) today: the R2 stand-in received $(log_lines) requests"
}

# --- the source database ----------------------------------------------------------

add_missions 1 80
# The id sequence skips, as a rolled-back append makes it skip: the
# watermark is not the row count.
psql -d "$SPAWN_DB_NAME" -tA -c "select setval(pg_get_serial_sequence('spawn_events', 'id'),
    (select last_value from spawn_events_id_seq) + 1000)" >/dev/null 2>&1 ||
    psql -d "$SPAWN_DB_NAME" -tA -c "select setval(pg_get_serial_sequence('spawn_events', 'id'),
        coalesce((select max(id) from spawn_events), 0) + 1000)" >/dev/null ||
    harness "cannot move the events' id sequence"
add_missions 81 120

# --- (1) an hourly encrypted dump ------------------------------------------------------

before=$(events_now)
n0=${before% *}
max0=${before#* }
lines=$(log_lines)
held0=$(held)
backup dump
if [ "$status" -ne 0 ]; then
    if [ -n "$MEASURE" ] && grep -q 'unknown command backup' "$WORK/err.txt"; then
        measured_fail "(1) \`spawn backup dump\` exited $status ($(err | tail -n 1)): no encrypted dump reached R2 and no backup.dumped record was written ($n0 events, max id $max0)"
        measure_today
        echo "031: (measure) would fail: (2)-(6): there is no \`spawn backup\` (dump, bundle, verify): no hourly copy, no bundle of the mission sources and the transcripts, full or incremental, no restore drill, no limit and no alert"
        asserted=1
        exit 1
    fi
    fail "(1) \`spawn backup dump\` exited $status ($(err | tail -n 1)): no encrypted dump reached R2 and no backup.dumped record was written ($n0 events, max id $max0)"
fi
dump1_ms=$took
rec1=$(newest backup.dumped)
[ -n "$rec1" ] || fail "(1) \`spawn backup dump\` exited 0 and recorded no backup.dumped"
key1=$(field "$rec1" '.keys[0] // empty')
printf '%s' "$key1" | grep -Eq '^postgres/hourly/spawn-[0-9]{8}-[0-9]{6}\.dump\.gpg$' ||
    measured_fail "(1) the dump's first key is [$key1], not postgres/hourly/spawn-<YYYYMMDD-HHMMSS>.dump.gpg"
for key in $(field "$rec1" '.keys[]?'); do
    has_object "$key" || measured_fail "(1) the record names $key, which R2 does not hold"
done
field "$rec1" '.keys[]?' | grep -Eq '^postgres/daily/spawn-[0-9]{8}-[0-9]{6}\.dump\.gpg$' ||
    measured_fail "(1) the day's first dump has no copy under postgres/daily/: keys $(field "$rec1" '.keys' | tr -d ' \n')"
has_object "$key1" || fail "(1) R2 does not hold the dump's key [$key1]"
stored=$(object "$key1")
[ "$(head -c 5 "$stored")" != PGDMP ] ||
    measured_fail "(1) the object $key1 is a plain pg_dump archive: the dump reached R2 unencrypted"
decrypt "$stored" "$WORK/dump1" ||
    fail "(1) the object $key1 does not decrypt with the passphrase file: $(head -c 300 "$WORK/gpg.err")"
[ "$(head -c 5 "$WORK/dump1")" = PGDMP ] ||
    measured_fail "(1) $key1 decrypts to no pg_dump custom archive"
sha1=$(sha "$WORK/dump1")
[ "$sha1" = "$(field "$rec1" '.sha256')" ] && [ "$sha1" = "$(meta "$key1" x-amz-meta-sha256)" ] ||
    measured_fail "(1) the dump's sha256 $sha1 is not the record's [$(field "$rec1" '.sha256')] and the object's x-amz-meta-sha256 [$(meta "$key1" x-amz-meta-sha256)]"
[ "$(size "$WORK/dump1")" = "$(field "$rec1" '.bytes')" ] &&
    [ "$(size "$stored")" = "$(field "$rec1" '.stored_bytes')" ] ||
    measured_fail "(1) the record's bytes [$(field "$rec1" '.bytes')] and stored_bytes [$(field "$rec1" '.stored_bytes')] are not the dump's $(size "$WORK/dump1") and the object's $(size "$stored")"
in_dump=$(dump_events "$WORK/dump1")
echo "031: dump 1: $(size "$WORK/dump1") bytes ($(size "$stored") stored), $in_dump (events, max id), in $dump1_ms ms; the database held $n0 events, max id $max0"
[ "$in_dump" = "$(field "$rec1" '.events') $(field "$rec1" '.max_event_id')" ] ||
    measured_fail "(1) the archive holds $in_dump (events, max id), the record says events [$(field "$rec1" '.events')] max_event_id [$(field "$rec1" '.max_event_id')]"
[ "${in_dump% *}" -ge "$n0" ] && [ "${in_dump#* }" -ge "$max0" ] ||
    measured_fail "(1) the archive holds $in_dump (events, max id), less than the $n0 events (max id $max0) the database held before the run"
[ "$(meta "$key1" x-amz-meta-spawn-events)" = "$(field "$rec1" '.events')" ] ||
    measured_fail "(1) the object's x-amz-meta-spawn-events [$(meta "$key1" x-amz-meta-spawn-events)] is not the record's events"
[ "$(field "$rec1" '.held_bytes')" = "$held0" ] ||
    measured_fail "(1) the record's held_bytes [$(field "$rec1" '.held_bytes')] is not the $held0 bytes the bucket held before the run"
bad=$(requests_since "$lines" | awk '$3 == 403 || $1 == "DELETE"' | head -n 3)
[ -z "$bad" ] || measured_fail "(1) requests R2 refused or a delete: $bad"
leak=$(secrets_leaked)
[ -z "$leak" ] || measured_fail "(1) a secret (the passphrase or the R2 secret key) shows in $leak"

# --- (2) every hour ------------------------------------------------------------------

add_missions 121 135
before=$(events_now)
n1=${before% *}
day1=$(date -u +%Y%m%d)
lines=$(log_lines)
held1=$(held)
backup dump
[ "$status" -eq 0 ] || fail "(2) the second \`spawn backup dump\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
dump2_ms=$took
rec2=$(newest backup.dumped)
[ "$(event_of "$rec2")" != "$(event_of "$rec1")" ] || fail "(2) the second dump recorded nothing"
key2=$(field "$rec2" '.keys[0] // empty')
keys2=$(field "$rec2" '.keys | length')
puts2=$(requests_since "$lines" | awk '$1 == "PUT" && $3 == 200' | wc -l | tr -d ' ')
printf '%s' "$key2" | grep -Eq '^postgres/hourly/' && [ "$key2" != "$key1" ] && has_object "$key2" ||
    measured_fail "(2) the second dump's key [$key2] is no new object under postgres/hourly/ (the first: $key1)"
if [ "$(date -u +%Y%m%d)" = "$day1" ] && [ "$day1" = "$(printf '%s' "$key1" | sed -E 's#.*spawn-([0-9]{8})-.*#\1#')" ]; then
    [ "$keys2" = 1 ] && [ "$puts2" = 1 ] ||
        measured_fail "(2) the second dump of the UTC day stored $puts2 objects, keys $(field "$rec2" '.keys' | tr -d ' \n'): one, under postgres/hourly/, the daily copy being the day's first"
fi
[ "$(field "$rec2" '.events')" -ge "$n1" ] && [ "$(field "$rec2" '.events')" -gt "$(field "$rec1" '.events')" ] ||
    measured_fail "(2) the second dump holds [$(field "$rec2" '.events')] events: the database held $n1, the first dump $(field "$rec1" '.events')"
[ "$(field "$rec2" '.held_bytes')" = "$held1" ] ||
    measured_fail "(2) the second record's held_bytes [$(field "$rec2" '.held_bytes')] is not the $held1 bytes the bucket held before it"
echo "031: dump 2: $(field "$rec2" '.events') events, $(field "$rec2" '.stored_bytes') bytes stored, in $dump2_ms ms"

# --- (3) the daily bundle: full, then incremental ------------------------------------

# unpack_bundle <key> <dir>: the object decrypted (its sha256 checked against
# the record $2's) and untarred into <dir>; 1 when it does not decrypt.
unpack_bundle() {
    stored=$(object "$1")
    rm -f "$WORK/bundle.raw" "$WORK/bundle.tar"
    decrypt "$stored" "$WORK/bundle.raw" || return 1
    magic=$(head -c 4 "$WORK/bundle.raw" | od -An -tx1 | tr -d ' \n')
    case "$magic" in
    1f8b*) gzip -dc <"$WORK/bundle.raw" >"$WORK/bundle.tar" ;;
    28b52ffd)
        command -v zstd >/dev/null 2>&1 || harness "the bundle is zstd-compressed and zstd is not installed"
        zstd -q -dc <"$WORK/bundle.raw" >"$WORK/bundle.tar"
        ;;
    *) cp "$WORK/bundle.raw" "$WORK/bundle.tar" ;;
    esac
    mkdir -p "$2"
    tar -xf "$WORK/bundle.tar" -C "$2" 2>"$WORK/tar.err" ||
        fail "(3) the bundle $1 decrypts to no tar archive (magic $magic): $(head -c 200 "$WORK/tar.err")"
}
heads_of() { git --git-dir="$1" for-each-ref --format='%(objectname) %(refname)' refs/heads refs/tags | sort; }
bundle_heads() { git bundle list-heads "$1" 2>/dev/null | awk '$2 != "HEAD"' | sort; }
# The files of the archive modified within the window (14 days), sorted.
window_files() { (cd "$archive" && find . -type f -mtime -14 | sed 's#^\./##' | sort); }

lines=$(log_lines)
backup bundle
[ "$status" -eq 0 ] || fail "(3) \`spawn backup bundle\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bundle_ms=$took
bun=$(newest backup.bundled)
[ -n "$bun" ] || fail "(3) \`spawn backup bundle\` exited 0 and recorded no backup.bundled"
bkey=$(field "$bun" '.keys[0] // empty')
week1=$(date -u +%G%V)
printf '%s' "$bkey" | grep -Eq '^bundles/full/' && has_object "$bkey" ||
    fail "(3) the first bundle's key [$bkey] is no object under bundles/full/ (the first bundle, with none before it, is a full one)"
[ "$(field "$bun" '.kind // empty')" = full ] ||
    measured_fail "(3) the first bundle is recorded kind [$(field "$bun" '.kind // empty')], not full"
unpack_bundle "$bkey" "$WORK/unpacked" ||
    fail "(3) the bundle $bkey does not decrypt with the passphrase file: $(head -c 300 "$WORK/gpg.err")"
[ "$(sha "$WORK/bundle.raw")" = "$(field "$bun" '.sha256')" ] ||
    measured_fail "(3) the bundle's sha256 is not the record's"
[ "$(jq -r '.kind // empty' "$WORK/unpacked/manifest.json" 2>/dev/null)" = full ] ||
    measured_fail "(3) the full bundle holds no manifest.json of kind full: $(head -c 200 "$WORK/unpacked/manifest.json" 2>/dev/null)"
git init -q --bare "$WORK/check.git"
for src in "$src_spawn" "$src_other"; do
    want=$(heads_of "$src")
    found=""
    for b in "$WORK"/unpacked/repos/*.bundle; do
        [ -f "$b" ] || continue
        if [ "$(bundle_heads "$b")" = "$want" ]; then found=$b; break; fi
    done
    [ -n "$found" ] ||
        measured_fail "(3) no repos/*.bundle in the bundle lists exactly the branches of $(basename "$src") ($(printf '%s\n' "$want" | wc -l | tr -d ' ') refs; bundles: $(cd "$WORK/unpacked" && find . -name '*.bundle' | tr '\n' ' '))"
    [ -n "$found" ] || continue
    git --git-dir="$WORK/check.git" bundle verify -q "$found" >/dev/null 2>"$WORK/verify.err" ||
        measured_fail "(3) the bundle of $(basename "$src") does not verify alone: $(head -c 200 "$WORK/verify.err")"
    if [ "$src" = "$src_spawn" ]; then
        git clone -q -b spawn/agent_worker_objective_7 "$found" "$WORK/clone" 2>"$WORK/clone.err" &&
            [ "$(cat "$WORK/clone/work.txt" 2>/dev/null)" = "the agent work, checkpointed" ] ||
            measured_fail "(3) the agent's branch spawn/agent_worker_objective_7 does not come back from the bundle with its work.txt"
        full_spawn=$found
    fi
done
files=0
bytes=0
for path in $(window_files); do
    files=$((files + 1))
    bytes=$((bytes + $(size "$archive/$path")))
    cmp -s "$archive/$path" "$WORK/unpacked/transcripts/$path" ||
        measured_fail "(3) the bundle has no transcripts/$path with the archive's bytes"
done
[ ! -e "$WORK/unpacked/transcripts/$old_transcript" ] ||
    measured_fail "(3) the full bundle holds $old_transcript, last written 20 days ago: outside the 14-day window"
[ "$(field "$bun" '.transcripts.files')" = "$files" ] && [ "$(field "$bun" '.transcripts.bytes')" = "$bytes" ] ||
    measured_fail "(3) the record's transcripts [$(field "$bun" '.transcripts | {files, bytes}' | tr -d ' \n')] are not the window's $files files, $bytes bytes"
named=$(field "$bun" '[.repos[]?.source] | sort | join(" ")')
[ "$named" = "$(printf '%s\n%s\n' "$src_other" "$src_spawn" | sort | tr '\n' ' ' | sed 's/ $//')" ] ||
    measured_fail "(3) the record names the sources [$named], not the two missions' $src_spawn and $src_other"
echo "031: full bundle: $(field "$bun" '.stored_bytes') bytes stored for $bytes bytes of transcripts and two sources, in $bundle_ms ms"

# The remote-tracking ref other.git carries is not the mission's history: the
# bundle holds no ref the manifest does not name, and the record names it not.
[ -n "$(git --git-dir="$src_other" for-each-ref --format='%(refname)' "$remote_ref")" ] ||
    harness "the fixture lost $src_other's $remote_ref"
field "$bun" '.repos[].heads | keys[]' | grep -qxF -- "$remote_ref" &&
    measured_fail "(3) the record names $remote_ref, which no source's bundle holds"
for b in "$WORK"/unpacked/repos/*.bundle; do
    [ -f "$b" ] || continue
    git bundle list-heads "$b" 2>/dev/null | awk '{ print $2 }' | grep -qxF -- "$remote_ref" &&
        measured_fail "(3) the bundle $(basename "$b") holds $remote_ref, which its manifest does not name"
done

# The drill opens the newest bundle too: now it is the full one.
backup verify SPAWN_BACKUP_DRILL_DATABASE_URL="$DRILL_URL"
[ "$status" -eq 0 ] ||
    fail "(3) \`spawn backup verify\` after the full bundle exited $status: $(err | tail -n 1)"
ver_full=$(newest backup.verified)
[ "$(field "$ver_full" '.ok')" = true ] && [ "$(field "$ver_full" '.bundle.checked')" = true ] &&
    [ "$(field "$ver_full" '.bundle.kind')" = full ] && [ "$(field "$ver_full" '.bundle.key')" = "$(field "$bun" '.keys[0]')" ] ||
    measured_fail "(3) the drill after the full bundle recorded $(field "$ver_full" '.bundle // empty' | head -c 300), not the full bundle $bkey opened as kind full"

# The archive grows (one file appended to, one new), the agent's branch
# moves, other.git does not: the next bundle the same day is an increment.
sup=agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0001.jsonl
wrk=agent_worker_objective_7/5f2a9e10-7c44-4b8e-8f00-2a4e9d3c0002.jsonl
new=agent_worker_objective_7/9c1d4e77-2b6a-4f0e-8d11-7e5a3c9b0003.jsonl
sup_before=$(size "$archive/$sup")
awk 'BEGIN { for (i = 1501; i <= 1700; i++)
    printf "{\"type\":\"assistant\",\"uuid\":\"s-%d\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"step %d: the next day, still nothing needs a person\"}]}}\n", i, i }' \
    >>"$archive/$sup"
awk 'BEGIN { for (i = 1; i <= 120; i++)
    printf "{\"type\":\"user\",\"uuid\":\"n-%d\",\"message\":{\"role\":\"user\",\"content\":\"a new session, line %d\"}}\n", i, i }' \
    >"$archive/$new"
printf 'more agent work\n' >>"$WORK/seed/work.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "spawn checkpoint exec_8"
git -C "$WORK/seed" push -q "$src_spawn" work:refs/heads/spawn/agent_worker_objective_7
lines=$(log_lines)
backup bundle
[ "$status" -eq 0 ] || fail "(3) the second \`spawn backup bundle\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
incr_ms=$took
inc=$(newest backup.bundled)
[ "$(event_of "$inc")" != "$(event_of "$bun")" ] || fail "(3) the second \`spawn backup bundle\` recorded nothing"
ikey=$(field "$inc" '.keys[0] // empty')
puts=$(requests_since "$lines" | awk '$1 == "PUT" && $3 == 200 && $2 ~ /^bundles\//' | wc -l | tr -d ' ')
if [ "$(date -u +%G%V)" != "$week1" ]; then
    echo "031: the ISO week turned between the two bundles: the second is a week's first, so a full one; the increment is not judged in this run"
else
    printf '%s' "$ikey" | grep -Eq '^bundles/incr/' && has_object "$ikey" && [ "$puts" = 1 ] ||
        measured_fail "(3) the second bundle of the day stored $puts objects, keys $(field "$inc" '.keys' | tr -d ' \n'): one, under bundles/incr/ (an increment of the full one)"
    [ "$(field "$inc" '.kind // empty')" = incremental ] && [ "$(field "$inc" '.base_event_id // empty')" = "$(event_of "$bun")" ] ||
        measured_fail "(3) the second bundle is recorded kind [$(field "$inc" '.kind // empty')] base_event_id [$(field "$inc" '.base_event_id // empty')], not incremental of the full one (event $(event_of "$bun"))"
    if [ -n "$ikey" ] && has_object "$ikey"; then
        unpack_bundle "$ikey" "$WORK/unpacked2" ||
            fail "(3) the increment $ikey does not decrypt with the passphrase file: $(head -c 300 "$WORK/gpg.err")"
        manifest=$WORK/unpacked2/manifest.json
        [ "$(jq -r '.kind // empty' "$manifest" 2>/dev/null)" = incremental ] ||
            measured_fail "(3) the increment holds no manifest.json of kind incremental: $(head -c 200 "$manifest" 2>/dev/null)"
        # The grown file: its appended bytes alone, at the full one's size.
        tail -c "+$((sup_before + 1))" "$archive/$sup" >"$WORK/appended"
        [ "$(jq -r --arg p "$sup" '.transcripts[]? | select(.path == $p) | .offset' "$manifest" 2>/dev/null)" = "$sup_before" ] &&
            cmp -s "$WORK/appended" "$WORK/unpacked2/transcripts/$sup" ||
            measured_fail "(3) the increment does not hold exactly the $(size "$WORK/appended") bytes appended to $sup at offset $sup_before (manifest: $(jq -c --arg p "$sup" '[.transcripts[]? | select(.path == $p)]' "$manifest" 2>/dev/null); held: $(size "$WORK/unpacked2/transcripts/$sup" 2>/dev/null || echo none) bytes)"
        [ "$(jq -r --arg p "$new" '.transcripts[]? | select(.path == $p) | .offset' "$manifest" 2>/dev/null)" = 0 ] &&
            cmp -s "$archive/$new" "$WORK/unpacked2/transcripts/$new" ||
            measured_fail "(3) the increment does not hold the new transcript $new whole at offset 0"
        [ ! -e "$WORK/unpacked2/transcripts/$wrk" ] ||
            measured_fail "(3) the increment holds $wrk, which did not change since the full bundle"
        # The full bundle and the increment give the window back.
        for path in $(window_files); do
            if [ -f "$WORK/unpacked2/transcripts/$path" ]; then
                off=$(jq -r --arg p "$path" '[.transcripts[]? | select(.path == $p) | .offset][0] // 0' "$manifest")
                { if [ "$off" -gt 0 ]; then head -c "$off" "$WORK/unpacked/transcripts/$path"; fi
                  cat "$WORK/unpacked2/transcripts/$path"; } >"$WORK/restored"
            else
                cp "$WORK/unpacked/transcripts/$path" "$WORK/restored" 2>/dev/null || : >"$WORK/restored"
            fi
            cmp -s "$WORK/restored" "$archive/$path" ||
                measured_fail "(3) the full bundle and the increment do not give $path back byte for byte"
        done
        # spawn.git: the bundle its manifest names (an increment lists only
        # the refs that moved), with prerequisites the full one holds, which
        # fetched after the full one gives exactly the source's branches now;
        # other.git unchanged.
        want=$(heads_of "$src_spawn")
        found=""
        f=$(jq -r --arg s "$src_spawn" '[.repos[]? | select(.source == $s) | .file][0] // empty' "$manifest" 2>/dev/null)
        [ -n "$f" ] && [ -f "$WORK/unpacked2/$f" ] && found=$WORK/unpacked2/$f
        if [ -z "$found" ]; then
            measured_fail "(3) the increment's manifest names no bundle of spawn.git, whose agent branch moved: $(jq -c '.repos' "$manifest" 2>/dev/null) (bundles: $(cd "$WORK/unpacked2" && find . -name '*.bundle' | tr '\n' ' '))"
        else
            # A v2 bundle's header: its prerequisites are the lines that
            # start with `-`, before the first blank line.
            prerequisites=$(awk 'NR == 1 { next } /^$/ { exit } /^-/ { n++ } END { print n + 0 }' "$found" 2>/dev/null || echo 0)
            [ "$prerequisites" -ge 1 ] ||
                measured_fail "(3) spawn.git's bundle in the increment names no prerequisites: a whole copy, not an increment"
            if [ -n "${full_spawn-}" ]; then
                git init -q --bare "$WORK/restore.git"
                git --git-dir="$WORK/restore.git" fetch -q "$full_spawn" 'refs/*:refs/*' 2>"$WORK/fetch.err" &&
                    git --git-dir="$WORK/restore.git" fetch -q "$found" 'refs/*:refs/*' 2>>"$WORK/fetch.err" &&
                    [ "$(heads_of "$WORK/restore.git")" = "$want" ] ||
                    measured_fail "(3) the full bundle of spawn.git and the increment's, fetched in turn, do not give the source's branches: $(head -c 200 "$WORK/fetch.err")"
            fi
        fi
        [ "$(jq -r --arg s "$src_other" '[.repos[]? | select(.source == $s) | .unchanged][0] // empty' "$manifest" 2>/dev/null)" = true ] &&
            [ "$(printf '%s' "$inc" | cut -d'|' -f2- | jq -r --arg s "$src_other" '[.repos[]? | select(.source == $s) | .unchanged][0] // empty' 2>/dev/null)" = true ] ||
            measured_fail "(3) the increment and its record do not mark other.git unchanged (no ref of it moved): $(jq -c '.repos' "$manifest" 2>/dev/null)"
    fi
    echo "031: increment: $(field "$inc" '.stored_bytes') bytes stored for $(field "$inc" '.transcripts.bytes') bytes of transcripts, in $incr_ms ms"
fi

# --- (4) the restore drill -------------------------------------------------------------

add_missions 136 145
latest=$(newest backup.dumped)
others_before=$(others)
now_events=$(events_now)
verified_before=$(count_records backup.verified)
backup verify SPAWN_BACKUP_DRILL_DATABASE_URL="$DRILL_URL"
[ "$status" -eq 0 ] ||
    fail "(4) \`spawn backup verify\` exited $status on an intact dump ($(err | tail -n 1)); its record: $(newest backup.verified | cut -d'|' -f2- | head -c 300)"
drill_ms=$took
ver=$(newest backup.verified)
[ "$(count_records backup.verified)" -gt "$verified_before" ] ||
    fail "(4) \`spawn backup verify\` exited 0 and recorded no backup.verified"
[ "$(field "$ver" '.ok')" = true ] ||
    measured_fail "(4) the drill recorded ok [$(field "$ver" '.ok')]: $(field "$ver" '.reason // empty')"
[ "$(field "$ver" '.dumped_event_id')" = "$(event_of "$latest")" ] &&
    field "$latest" '.keys[]' | grep -qxF -- "$(field "$ver" '.key')" ||
    measured_fail "(4) the drill names key [$(field "$ver" '.key')] and dumped_event_id [$(field "$ver" '.dumped_event_id')], not the newest dump's record (event $(event_of "$latest"), $(field "$latest" '.keys[0]'))"
[ "$(field "$ver" '.events') $(field "$ver" '.max_event_id')" = "$(field "$latest" '.events') $(field "$latest" '.max_event_id')" ] &&
    [ "$(field "$ver" '.expected.events') $(field "$ver" '.expected.max_event_id')" = "$(field "$latest" '.events') $(field "$latest" '.max_event_id')" ] ||
    measured_fail "(4) the drill restored events [$(field "$ver" '.events')] max id [$(field "$ver" '.max_event_id')], expected [$(field "$ver" '.expected' | tr -d ' \n')]; the dump's record says $(field "$latest" '.events') and $(field "$latest" '.max_event_id') (the database now: $now_events)"
want_kind=incremental
if [ "$(date -u +%G%V)" != "$week1" ]; then want_kind=full; fi
[ "$(field "$ver" '.bundle.checked')" = true ] && [ "$(field "$ver" '.bundle.kind')" = "$want_kind" ] ||
    measured_fail "(4) the drill recorded bundle [$(field "$ver" '.bundle | {checked, kind, key}' | tr -d ' \n')], not the newest bundle ($want_kind)"
scratch=$(field "$ver" '.database // empty')
case "$scratch" in
"" | "$SPAWN_DB_NAME" | postgres | template* | *[!A-Za-z0-9_]*)
    measured_fail "(4) the drill names [$scratch] as the database it restored into: a database of its own, not the source $SPAWN_DB_NAME"
    ;;
*)
    ! db_exists "$scratch" ||
        measured_fail "(4) the drill's scratch database $scratch is still there after it"
    ;;
esac
[ "$(others)" = "$others_before" ] ||
    measured_fail "(4) the source's other records changed during the drill: $others_before (count, max id) -> $(others)"
if [ -n "$db_password" ] && records backup.verified | grep -qF -- "$db_password"; then
    measured_fail "(4) the drill's record holds the test database's password"
fi
echo "031: drill: restored $(field "$ver" '.events') events (max id $(field "$ver" '.max_event_id')) into $scratch in $drill_ms ms; the database now holds $now_events"

# --- (5) refusals and a corrupted dump -----------------------------------------------------

others_before=$(others)
verified_before=$(count_records backup.verified)
backup verify
[ "$status" -eq 2 ] ||
    measured_fail "(5) without SPAWN_BACKUP_DRILL_DATABASE_URL the drill exited $status, not 2 (it could not run)"
[ "$(count_records backup.verified)" -gt "$verified_before" ] &&
    [ "$(field "$(newest backup.verified)" '.ok')" = false ] &&
    field "$(newest backup.verified)" '.reason // ""' | grep -q SPAWN_BACKUP_DRILL_DATABASE_URL ||
    measured_fail "(5) without a drill server no backup.verified ok=false names SPAWN_BACKUP_DRILL_DATABASE_URL: $(newest backup.verified | cut -d'|' -f2- | head -c 300)"
[ "$(others)" = "$others_before" ] ||
    measured_fail "(5) the drill without a drill server changed the source's records: $others_before -> $(others)"

latest=$(newest backup.dumped)
for key in $(field "$latest" '.keys[]'); do
    file=$(object "$key")
    python3 -I -c 'import sys
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
data[len(data) // 2] ^= 0xFF
open(path, "wb").write(data)' "$file" || harness "cannot corrupt $key"
done
verified_before=$(count_records backup.verified)
backup verify SPAWN_BACKUP_DRILL_DATABASE_URL="$DRILL_URL"
corrupt_ms=$took
bad=$(newest backup.verified)
[ "$status" -eq 1 ] ||
    measured_fail "(5) the drill of a corrupted dump ($(field "$latest" '.keys[0]'), one byte flipped) exited $status, not 1"
[ "$(count_records backup.verified)" -gt "$verified_before" ] && [ "$(field "$bad" '.ok')" = false ] &&
    [ -n "$(field "$bad" '.reason // empty')" ] ||
    measured_fail "(5) the drill of a corrupted dump recorded no backup.verified ok=false with a reason: $(printf '%s' "$bad" | cut -d'|' -f2- | head -c 300)"
left=$(field "$bad" '.database // empty')
if [ -n "$left" ] && [ "$left" != "$SPAWN_DB_NAME" ] && db_exists "$left"; then
    measured_fail "(5) the failed drill left its scratch database $left behind"
fi
echo "031: corrupted dump: the drill failed in $corrupt_ms ms: $(field "$bad" '.reason // empty' | head -c 200)"

# --- (6) the alert and the limit -------------------------------------------------------

junk=junk/kept-elsewhere.bin
head -c 1500000 /dev/urandom >"$(object "$junk")"
jq -n --arg key "$junk" --argjson size 1500000 \
    '{key: $key, etag: "0", size: $size, meta: {}, modified: "2026-01-01T00:00:00.000Z"}' >"$(object "$junk").json"
held6=$(held)
lines=$(log_lines)
backup dump SPAWN_BACKUP_R2_ALERT_MB=1
[ "$status" -eq 0 ] || fail "(6) a dump over the alert (and under the limit) exited $status: $(err | tail -n 2 | tr '\n' ' ')"
rec6=$(newest backup.dumped)
key6=$(field "$rec6" '.keys[0] // empty')
printf '%s' "$key6" | grep -Eq '^postgres/hourly/' && has_object "$key6" && [ "$key6" != "$key2" ] &&
    [ "$(field "$rec6" '.over_alert')" = true ] && [ "$(field "$rec6" '.alert_bytes')" = 1000000 ] &&
    [ "$(field "$rec6" '.limit_bytes')" = 10000000000 ] && [ "$(field "$rec6" '.held_bytes')" = "$held6" ] ||
    measured_fail "(6) with $held6 bytes held and an alert at 1 MB the dump must store its hourly copy and record over_alert true, alert_bytes 1000000, limit_bytes 10000000000 and held_bytes $held6: key [$key6], $(printf '%s' "$rec6" | cut -d'|' -f2- | jq -c '{over_alert, alert_bytes, limit_bytes, held_bytes, written_bytes}' 2>/dev/null)"
held7=$(held)
lines=$(log_lines)
backup bundle SPAWN_BACKUP_R2_ALERT_MB=1 SPAWN_BACKUP_R2_LIMIT_MB=1
bun2=$(newest backup.bundled)
puts=$(requests_since "$lines" | awk '$1 == "PUT" && $2 ~ /^bundles\//' | head -n 3)
[ -z "$puts" ] ||
    measured_fail "(6) with $held7 bytes held and a limit of 1 MB the bundle was still stored: $puts"
[ "$(event_of "$bun2")" != "$(event_of "$inc")" ] && [ "$(field "$bun2" '.keys | length')" = 0 ] &&
    [ "$(field "$bun2" '.skipped | length')" -ge 1 ] && [ "$(field "$bun2" '.limit_bytes')" = 1000000 ] &&
    [ "$(field "$bun2" '.held_bytes')" = "$held7" ] &&
    field "$bun2" '[.skipped[]?.reason] | join(" ")' | grep -qi limit ||
    measured_fail "(6) the bundle over the limit recorded no skipped tier with the limit (limit_bytes 1000000, held_bytes $held7): $(printf '%s' "$bun2" | cut -d'|' -f2- | head -c 300)"
dumped_before=$(count_records backup.dumped)
lines=$(log_lines)
backup dump SPAWN_BACKUP_R2_ALERT_MB=1 SPAWN_BACKUP_R2_LIMIT_MB=1
[ "$status" -eq 4 ] ||
    measured_fail "(6) a dump whose hourly copy the limit kept out exited $status, not 4: $(err | tail -n 2 | tr '\n' ' ')"
rec7=$(newest backup.dumped)
puts=$(requests_since "$lines" | awk '$1 == "PUT"' | head -n 3)
[ -z "$puts" ] ||
    measured_fail "(6) with $held7 bytes held and a limit of 1 MB the dump still wrote to R2: $puts"
[ "$(count_records backup.dumped)" -gt "$dumped_before" ] && [ "$(field "$rec7" '.keys | length')" = 0 ] &&
    [ "$(field "$rec7" '[.skipped[]? | select(.tier == "hourly")] | length')" -ge 1 ] &&
    [ "$(field "$rec7" '.limit_bytes')" = 1000000 ] &&
    field "$rec7" '[.skipped[]?.reason] | join(" ")' | grep -qi limit ||
    measured_fail "(6) the dump the limit kept out recorded no hourly tier skipped for the limit (keys [], limit_bytes 1000000): $(printf '%s' "$rec7" | cut -d'|' -f2- | head -c 300)"

if [ -n "$failed_any" ]; then
    asserted=1
    exit 1
fi
echo "031: hourly and daily dumps encrypted in R2 with their records, the sources and transcripts bundled whole and then as an increment, the drill restored the newest dump to its record's counts and failed a corrupted one, the alert was recorded and nothing was written past the limit"
exit 0
