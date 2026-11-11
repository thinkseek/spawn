#!/usr/bin/env bash
# deploy/backup/test-restore-drill.sh: `spawn backup restore-drill` (core
# (ii)) end to end, with the real binary's own `spawn backup bundle` making
# the backup, a fake R2 (a local directory behind a small HTTP server that
# answers PUT, GET and ListObjectsV2) and a throwaway database on the test
# server; the live fleet is never touched.
#
#   SPAWN_BIN=<a build of spawn> deploy/backup/test-restore-drill.sh
#
# The database URL comes from ~/.config/spawn/test-database.url, or the file
# SPAWN_RESTORE_DRILL_TEST_URL_FILE names (read before HOME is faked; never
# printed); a database of its own is created there and dropped at the end.
# Everything else runs under a fake HOME in the scratch directory, and the
# test refuses (exit 2, nothing run) when that is the real home.
#
# Cases:
# 1. a full bundle, then an increment (a transcript grown, one new, a new
#    commit and tag, a source with no refs), then live moving on (a grown
#    transcript, a new one, a new commit, a new branch, a branch removed):
#    the drill restores the chain, says one line per resource, passes,
#    writes nothing live and leaves no scratch directory behind;
# 2. --into under the fake HOME, under the workspace root, through a
#    symlink into the home, or relative: refused (exit 2), nothing made;
# 3. a live transcript changed within what the bundle holds, and a live
#    branch moved to a commit that does not descend from the restored one:
#    exit 1, each named; put back, it passes again;
# 4. --keep leaves the scratch directory, mode 700;
# 5. a stored bundle whose bytes changed in the bucket: exit 1, the backup
#    cannot be restored;
# and the key's passphrase is in no output.
set -euo pipefail
umask 077

repo=$(git rev-parse --show-toplevel)
PROG=test-restore-drill
# shellcheck source=deploy/backup/lib.sh
. "$repo/deploy/backup/lib.sh"

failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
check() { # check <description> <command...>
  local what=$1
  shift
  if "$@"; then pass "$what"; else fail "$what"; fi
}

spawn_bin=${SPAWN_BIN:-${CARGO_TARGET_DIR:-$repo/target}/debug/spawn}
[ -x "$spawn_bin" ] || die "no spawn build at $spawn_bin (cargo build -p spawn-cli, or SPAWN_BIN)"
need_tools git gpg python3 curl psql tar
test_url_file=${SPAWN_RESTORE_DRILL_TEST_URL_FILE:-$HOME/.config/spawn/test-database.url}
[ -f "$test_url_file" ] || die "no $test_url_file"
test_url=$(cat "$test_url_file")
case $test_url in *:15433/*) die "the test URL names port 15433, the live database" ;; esac
db=spawn_restore_drill_test_$$
db_url=${test_url%/*}/$db

real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/spawn-restore-drill-test.XXXXXX")
scratch=$(cd "$scratch" && pwd -P)
fake_home=$scratch/home
mkdir -m 700 "$fake_home"
# guard_home: HOME is the fake one, and is not the real home by any path.
guard_home() {
  local resolved
  resolved=$(cd "$HOME" 2>/dev/null && pwd -P) || { echo "$PROG: HOME does not resolve" >&2; exit 2; }
  if [ -z "$HOME" ] || [ "$resolved" != "$fake_home" ] || [ "$resolved" = "$(cd "$real_home" && pwd -P)" ]; then
    echo "$PROG: refusing: HOME ($HOME) is not this test's fake home" >&2
    exit 2
  fi
}
server_pid=""
cleanup() {
  [ -z "$server_pid" ] || kill "$server_pid" 2>/dev/null || true
  [ -z "$server_pid" ] || wait "$server_pid" 2>/dev/null || true
  with_db "$test_url" psql -XAtq -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT
export HOME=$fake_home XDG_CACHE_HOME=$fake_home/.cache XDG_CONFIG_HOME=$fake_home/.config \
  XDG_DATA_HOME=$fake_home/.local/share XDG_RUNTIME_DIR=$scratch/run GNUPGHOME=$fake_home/.gnupg
unset SPAWN_LIVE
mkdir -m 700 "$XDG_RUNTIME_DIR" "$GNUPGHOME"
guard_home
with_db "$test_url" psql -XAtq -v ON_ERROR_STOP=1 -c "CREATE DATABASE $db" >/dev/null

# The fake R2: objects are files under $bucket_dir/<bucket>/<key>, each
# with its x-amz-meta-* headers beside it; the ETag is the body's MD5, as
# R2's is for a single PUT. Signatures are not checked.
bucket_dir=$scratch/r2
mkdir -p "$bucket_dir/drill-bucket" "$scratch/bin"
cat >"$scratch/bin/fake-r2.py" <<'PY'
import hashlib, http.server, json, os, sys, urllib.parse
from xml.sax.saxutils import escape
ROOT = sys.argv[1]
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *args):
        pass
    def split(self):
        url = urllib.parse.urlsplit(self.path)
        parts = urllib.parse.unquote(url.path).lstrip("/").split("/", 1)
        return parts[0], (parts[1] if len(parts) > 1 else ""), urllib.parse.parse_qs(url.query)
    def file(self, bucket, key):
        path = os.path.realpath(os.path.join(ROOT, bucket, key))
        if not path.startswith(os.path.realpath(os.path.join(ROOT, bucket)) + os.sep):
            raise ValueError(key)
        return path
    def answer(self, status, body=b"", headers=()):
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_PUT(self):
        bucket, key, _ = self.split()
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        path = self.file(bucket, key)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as out:
            out.write(body)
        meta = {k.lower(): v for k, v in self.headers.items() if k.lower().startswith("x-amz-meta-")}
        with open(path + ".meta", "w") as out:
            json.dump(meta, out)
        self.answer(200, headers=[("ETag", '"%s"' % hashlib.md5(body).hexdigest())])
    def do_GET(self):
        bucket, key, query = self.split()
        if key == "" and "list-type" in query:
            rows = []
            base = os.path.join(ROOT, bucket)
            for directory, _, names in os.walk(base):
                for name in names:
                    if name.endswith(".meta"):
                        continue
                    path = os.path.join(directory, name)
                    rows.append((os.path.relpath(path, base), os.path.getsize(path)))
            body = "<ListBucketResult><IsTruncated>false</IsTruncated>" + "".join(
                "<Contents><Key>%s</Key><Size>%d</Size></Contents>" % (escape(k), s) for k, s in sorted(rows)
            ) + "</ListBucketResult>"
            self.answer(200, body.encode())
            return
        path = self.file(bucket, key)
        if not os.path.isfile(path):
            self.answer(404, b"<Error><Code>NoSuchKey</Code></Error>")
            return
        with open(path, "rb") as source:
            body = source.read()
        headers = []
        if os.path.exists(path + ".meta"):
            with open(path + ".meta") as meta:
                headers = list(json.load(meta).items())
        self.answer(200, body, headers)
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(sys.argv[2], "w") as out:
    out.write(str(server.server_address[1]))
server.serve_forever()
PY
python3 -I "$scratch/bin/fake-r2.py" "$bucket_dir" "$scratch/port" &
server_pid=$!
for _ in $(seq 50); do [ -s "$scratch/port" ] && break; sleep 0.1; done
[ -s "$scratch/port" ] || die "the fake R2 did not start"

# The owner's files, as `spawn backup` reads them: mode 600.
mkdir -m 700 -p "$fake_home/.config/spawn"
passphrase="drill-passphrase-$RANDOM$RANDOM-not-printed"
printf '%s\n' "$passphrase" >"$fake_home/.config/spawn/backup.key"
cat >"$fake_home/.config/spawn/r2.env" <<EOF
SPAWN_R2_ENDPOINT=http://127.0.0.1:$(cat "$scratch/port")
SPAWN_R2_BUCKET=drill-bucket
SPAWN_R2_ACCESS_KEY_ID=test-access
SPAWN_R2_SECRET_ACCESS_KEY=test-secret
EOF
chmod 600 "$fake_home/.config/spawn/backup.key" "$fake_home/.config/spawn/r2.env"
printf 'SPAWN_DATABASE_URL=%q\n' "$db_url" >"$scratch/spawn.env"
chmod 600 "$scratch/spawn.env"
# spawn_run ARGS: the binary with the database in its environment (never an
# argument), under the fake home.
spawn_run() {
  guard_home
  (set -a && . "$scratch/spawn.env" && set +a && "$spawn_bin" "$@")
}
spawn_run init-db >/dev/null

# The workspace: its transcript archive, and two mission sources.
ws=$scratch/ws
archive=$ws/.transcripts
mkdir -p "$archive/agent_a" "$archive/agent_b" "$scratch/drills"
printf '{"n":1}\n' >"$archive/agent_a/one.jsonl"
printf '{"n":1}\n{"n":2}\n' >"$archive/agent_b/two.jsonl"
work=$scratch/work
src=$scratch/missions/spawn.git
empty=$scratch/missions/empty.git
git init -q -b main "$work"
git init -q --bare -b main "$src"
git init -q --bare -b main "$empty"
commit() { # <message>: a commit in the work tree, pushed to main
  echo "$1" >>"$work/value"
  git -C "$work" add -A
  git -C "$work" -c user.name=t -c user.email=t@localhost commit -q -m "$1"
  git -C "$work" push -q "$src" HEAD:refs/heads/main
}
commit "first"
git -C "$work" push -q "$src" HEAD:refs/heads/side
spawn_run mission add --text "drill: spawn" --source "$src" >/dev/null
spawn_run mission add --text "drill: empty" --source "$empty" >/dev/null

out=$scratch/out
echo "== the backup: a full bundle, then an increment"
spawn_run backup bundle --root "$ws" >"$out" 2>&1 || { cat "$out"; die "the full bundle failed"; }
check "the first bundle is full" grep -q "spawn backup bundle: full" "$out"
sleep 1.2
printf '{"n":2}\n' >>"$archive/agent_a/one.jsonl"
printf '{"n":1}\n' >"$archive/agent_b/three.jsonl"
commit "second"
git -C "$work" tag v1
git -C "$work" push -q "$src" refs/tags/v1
spawn_run backup bundle --root "$ws" >"$out" 2>&1 || { cat "$out"; die "the increment failed"; }
check "the second is an increment" grep -q "spawn backup bundle: incremental" "$out"
# Live moves on after the backup.
sleep 1.2
printf '{"n":3}\n' >>"$archive/agent_a/one.jsonl"
printf '{"n":1}\n' >"$archive/agent_b/four.jsonl"
commit "third"
git -C "$work" push -q "$src" HEAD:refs/heads/after
git -C "$src" update-ref -d refs/heads/side

drill() { # drill ARGS: the drill, its output in $out, its status in $status
  set +e
  spawn_run backup restore-drill --root "$ws" "$@" >"$out" 2>&1
  status=$?
  set -e
  sed 's/^/  | /' "$out"
  cat "$out" >>"$scratch/all.out"
}
drills_left() { find "$scratch/drills" -mindepth 1 -maxdepth 1 | wc -l; }

# live_state: every live file and directory, its size, mode and mtime: the
# drill reads live and writes none of it.
live_state() { find "$ws" "$scratch/missions" -printf '%p %s %m %T@\n' | LC_ALL=C sort | sha256sum; }

echo "== 1. the drill restores the chain and matches live"
before=$(live_state)
drill --into "$scratch/drills"
check "live is as it was: nothing written there" [ "$(live_state)" = "$before" ]
check "passes (status $status)" [ "$status" -eq 0 ]
check "the chain is the full bundle and its increment" grep -qE "restore-drill: chain full [0-9]+, incremental [0-9]+: 2 bundle" "$out"
check "transcripts: 3 restored, 2 the same, 1 grown, 1 new since" \
  grep -qE "transcripts: 3 files \([0-9]+ bytes\) restored; against .*: 2 the same, 1 grown since \(their start the same\), 1 new since the bundle, 0 in no bundle" "$out"
check "the source: 3 refs, main moved on, side removed, after added" \
  grep -qF "missions $src: 3 refs restored (clone --mirror, fsck connected); against live: 1 the same, 1 moved on since (descendants), 1 added since, 1 removed since, 0 mismatch(es)" "$out"
check "the empty source restores as empty" grep -qF "missions $empty: 0 refs restored" "$out"
check "says it passed" grep -q "restore-drill: passed: bundle" "$out"
check "no scratch directory is left" [ "$(drills_left)" -eq 0 ]

echo "== 2. --into is never under the home, the root, nor relative"
mkdir -p "$fake_home/drills"
ln -s "$fake_home" "$scratch/into-home"
for case in "$fake_home/drills|under \$HOME" "$ws|under the workspace root" \
  "$scratch/into-home|under \$HOME" "drills|is not an absolute path"; do
  into=${case%%|*}
  drill --into "$into"
  check "--into $into refused (status $status)" [ "$status" -eq 2 ]
  check "--into $into: says ${case#*|}" grep -qF "${case#*|}" "$out"
done
check "nothing made in the home" [ -z "$(ls -A "$fake_home/drills")" ]

echo "== 3. a mismatch older than the bundle fails, named"
cp "$archive/agent_b/two.jsonl" "$scratch/two.saved"
printf '{"n":9}\n{"n":2}\n' >"$archive/agent_b/two.jsonl"
main_before=$(git -C "$src" rev-parse refs/heads/main)
orphan=$(git -C "$src" -c user.name=t -c user.email=t@localhost commit-tree -m "an unrelated root" "$(git -C "$src" mktree </dev/null)")
git -C "$src" update-ref refs/heads/main "$orphan"
drill --into "$scratch/drills"
check "fails (status $status)" [ "$status" -eq 1 ]
check "names the changed transcript" grep -qF "mismatch: transcripts/agent_b/two.jsonl: live's first" "$out"
check "names the rewritten branch" grep -qF "mismatch: $src: refs/heads/main: restored at" "$out"
check "says FAILED with the count" grep -q "restore-drill: FAILED: 2 mismatch(es) older than bundle" "$out"
check "no scratch directory is left" [ "$(drills_left)" -eq 0 ]
cp "$scratch/two.saved" "$archive/agent_b/two.jsonl"
git -C "$src" update-ref refs/heads/main "$main_before"
drill --into "$scratch/drills"
check "put back, it passes again" [ "$status" -eq 0 ]

echo "== 4. --keep leaves the scratch directory"
drill --into "$scratch/drills" --keep
kept=$(sed -n 's/^restore-drill: kept //p' "$out")
check "kept, and named" [ -d "$kept" ]
check "mode 700" [ "$(stat -c %a "$kept")" = 700 ]
check "holds the restored archive" [ -f "$kept/transcripts/agent_a/one.jsonl" ]
rm -rf "$kept"

echo "== 5. a stored bundle changed in the bucket cannot be restored"
stored=$(find "$bucket_dir/drill-bucket/bundles/incr" -type f ! -name '*.meta' | head -1)
printf 'x' | dd of="$stored" bs=1 seek=100 conv=notrunc status=none
drill --into "$scratch/drills"
check "fails (status $status)" [ "$status" -eq 1 ]
check "says the backup cannot be restored" grep -q "restore-drill: FAILED: the backup cannot be restored" "$out"
check "no scratch directory is left" [ "$(drills_left)" -eq 0 ]

echo "== the key"
check "the passphrase is in no output of any drill" bash -c '! grep -qF "$1" "$2"' _ "$passphrase" "$scratch/all.out"

if [ "$failures" -gt 0 ]; then
  echo "restore-drill: $failures check(s) failed"
  exit 1
fi
echo "restore-drill: all cases pass"
