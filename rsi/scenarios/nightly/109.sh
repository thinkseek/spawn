#!/bin/sh
# Black box: spawn reads a database password from a passfile, as libpq
# does, when the URL holds none (part 1 of A1; inert until 110 uses it).
#
# Why (sync/url-leak-sources.md; the lead and the owner, 11-13). An engine's
# environment holds SPAWN_TEST_DATABASE_URL with its password, and the
# supervisor's holds a live login with its password, so any print of the
# environment puts a password into a transcript. Objectives 110 and 111
# hand engines URLs without a password, with the password in a 0600
# passfile their deny list covers. psql reads such a file (PGPASSFILE, else
# ~/.pgpass); spawn did not, and a URL without a password failed to log in.
#
# The scenario makes its own login (a made-up password) and its own
# database on the server of SPAWN_TEST_DATABASE_URL. If that server lets
# the login in with no password, it cannot judge (exit 2). Every URL below
# names that login and holds no password; HOME is the scenario's own.
#   (1) PGPASSFILE a 0600 file with a comment line, a line for another user,
#       then the matching line: `spawn init-db` exits 0, and `spawn serve`
#       answers GET /api/fleet with 200.
#   (2) The login's password changed to one holding `:` and `\`, written
#       escaped in the line (`\:`, `\\`): `spawn init-db` exits 0.
#   (3) No PGPASSFILE, the line in $HOME/.pgpass (0600): exits 0.
#   (4) Neither: exits non-zero.
#   (5) PGPASSFILE mode 0644: exits non-zero, and stderr names the file
#       with "group or world access; not used".
#   (6) The URL holding a wrong password and the passfile the right one:
#       exits non-zero (the URL's own password wins, as with libpq).
#   (7) Nothing spawn printed in (1) to (6) holds the made-up passwords.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "109: harness: $*" >&2
    exit 2
}
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
controller=$(cd "$(dirname "$0")/../.." && pwd)
for tool in psql curl od sed grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -m 700 "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
role="bb_109_$run"
db="bb_109_$run"
pw="bb109-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
pw2="bb:109\\$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
role_made=""
db_made=""
serve_pid=""
cleanup() {
    [ -z "$serve_pid" ] || { kill "$serve_pid" 2>/dev/null || true; wait "$serve_pid" 2>/dev/null || true; }
    # Each drop only for what this run made (rev-e's F2): a database of
    # that name made by someone else is never dropped.
    [ -z "$db_made" ] || psql -d postgres -qtA -c "drop database if exists $db" >/dev/null 2>&1 || true
    [ -z "$role_made" ] || psql -d postgres -qtA -c "drop role if exists $role" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "109: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# The made-up passwords go to psql on stdin, never on its argv (rev-e's N1).
printf "create role %s login createrole password '%s'\n" "$role" "$pw" |
    psql -d postgres -qtA -v ON_ERROR_STOP=1 -f - >/dev/null 2>"$WORK/err" ||
    harness "cannot create the scenario's login: $(head -c 300 "$WORK/err")"
role_made=1
psql -d postgres -qtA -v ON_ERROR_STOP=1 -c "create database $db owner $role" >/dev/null 2>"$WORK/err" ||
    harness "cannot create the scenario's database: $(head -c 300 "$WORK/err")"
db_made=1
if env -u PGPASSWORD PGUSER="$role" PGPASSFILE=/dev/null psql -w -d "$db" -qtAc 'select 1' >/dev/null 2>&1; then
    harness "the server lets $role in with no password: it cannot judge a password's source"
fi

host_enc=$(pg_url_encode_host "$pg_url_host")
url="$pg_url_scheme://$role@$host_enc:$pg_url_port/$db"
escape() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/:/\\:/g'; }
line() { # <password>
    printf '%s:%s:%s:%s:%s\n' "$(escape "$pg_url_host")" "$pg_url_port" "$db" "$role" "$(escape "$1")"
}
passfile() { # <path> <mode> <password>
    {
        printf '# made by 109\n'
        printf '*:*:*:someone_else_%s:not-this-one\n' "$run"
        line "$3"
    } >"$1"
    chmod "$2" "$1"
}
: >"$WORK/printed"
spawn_run() { # <name> <url> [env assignments...]: runs init-db; status in $status
    name=$1
    u=$2
    shift 2
    status=0
    env -u PGPASSWORD -u PGPASSFILE -u PGUSER -u PGHOST -u PGPORT -u PGDATABASE \
        SPAWN_DATABASE_URL="$u" "$@" "$SPAWN_BIN" init-db >"$WORK/$name.out" 2>"$WORK/$name.err" </dev/null || status=$?
    cat "$WORK/$name.out" "$WORK/$name.err" >>"$WORK/printed"
}
check_failed() {
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

# --- (1) PGPASSFILE --------------------------------------------------------------
passfile "$WORK/pass1" 600 "$pw"
spawn_run one "$url" PGPASSFILE="$WORK/pass1"
[ "$status" -eq 0 ] || check_failed "(1) spawn init-db with PGPASSFILE (0600, the matching line third) exited $status: $(head -c 300 "$WORK/one.err")"
mkdir "$WORK/web"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"
env -u PGPASSWORD -u PGPASSFILE -u PGUSER SPAWN_DATABASE_URL="$url" PGPASSFILE="$WORK/pass1" \
    "$SPAWN_BIN" serve --address 127.0.0.1:0 --web "$WORK/web" >"$WORK/serve.log" 2>&1 </dev/null &
serve_pid=$!
port=""
i=0
while [ "$i" -lt 100 ]; do
    i=$((i + 1))
    port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
    [ -n "$port" ] && break
    kill -0 "$serve_pid" 2>/dev/null || break
    sleep 0.1
done
if [ -n "$port" ]; then
    code=$(curl -sS --noproxy '*' --max-time 30 -o "$WORK/fleet.json" -w '%{http_code}' "http://127.0.0.1:$port/api/fleet" 2>"$WORK/curl.err") || code=000
    [ "$code" = 200 ] || check_failed "(1) spawn serve with PGPASSFILE answered GET /api/fleet with $code: $(head -c 200 "$WORK/fleet.json" 2>/dev/null)"
else
    check_failed "(1) spawn serve with PGPASSFILE did not start: $(head -c 300 "$WORK/serve.log")"
fi
kill "$serve_pid" 2>/dev/null || true
wait "$serve_pid" 2>/dev/null || true
serve_pid=""
cat "$WORK/serve.log" >>"$WORK/printed"
echo "109: (1) checked: PGPASSFILE" >&2

# --- (2) escapes -----------------------------------------------------------------
pw2_sql=$(printf '%s' "$pw2" | sed "s/'/''/g")
printf "alter role %s password '%s'\n" "$role" "$pw2_sql" |
    psql -d postgres -qtA -v ON_ERROR_STOP=1 -f - >/dev/null 2>"$WORK/err" ||
    harness "cannot change the login's password: $(head -c 300 "$WORK/err")"
passfile "$WORK/pass2" 600 "$pw2"
spawn_run two "$url" PGPASSFILE="$WORK/pass2"
[ "$status" -eq 0 ] || check_failed "(2) spawn init-db with a password holding ':' and '\\' written escaped exited $status: $(head -c 300 "$WORK/two.err")"
echo "109: (2) checked: escapes" >&2

# --- (3) $HOME/.pgpass -----------------------------------------------------------
passfile "$HOME/.pgpass" 600 "$pw2"
spawn_run three "$url"
[ "$status" -eq 0 ] || check_failed "(3) spawn init-db with the line in \$HOME/.pgpass and no PGPASSFILE exited $status: $(head -c 300 "$WORK/three.err")"
rm -f "$HOME/.pgpass"
echo "109: (3) checked: ~/.pgpass" >&2

# --- (4) neither -------------------------------------------------------------------
spawn_run four "$url"
[ "$status" -ne 0 ] || check_failed "(4) spawn init-db with no password anywhere exited 0 (the server asked for one)"
echo "109: (4) checked: no password" >&2

# --- (5) a passfile others can read --------------------------------------------------
passfile "$WORK/pass5" 644 "$pw2"
spawn_run five "$url" PGPASSFILE="$WORK/pass5"
[ "$status" -ne 0 ] || check_failed "(5) spawn init-db with PGPASSFILE mode 0644 exited 0"
grep -qF "$WORK/pass5" "$WORK/five.err" && grep -qF "group or world access; not used" "$WORK/five.err" ||
    check_failed "(5) stderr does not name $WORK/pass5 with 'group or world access; not used': $(head -c 300 "$WORK/five.err")"
echo "109: (5) checked: mode 0644" >&2

# --- (6) the URL's own password wins -----------------------------------------------------
wrong="$pg_url_scheme://$role:bb-wrong-$run@$host_enc:$pg_url_port/$db"
spawn_run six "$wrong" PGPASSFILE="$WORK/pass2"
[ "$status" -ne 0 ] || check_failed "(6) spawn init-db with a wrong password in the URL and the right one in PGPASSFILE exited 0"
echo "109: (6) checked: the URL wins" >&2

# --- (7) no password printed ---------------------------------------------------------
if grep -qF -e "$pw" -e "$pw2" "$WORK/printed"; then
    check_failed "(7) spawn printed a made-up password in (1) to (6)"
fi
echo "109: (7) checked: nothing printed holds a password" >&2

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "109: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "109: spawn reads a URL's missing password from PGPASSFILE or ~/.pgpass, as libpq does, and never from a file others can read"
exit 0
