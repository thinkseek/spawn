#!/bin/sh
# Black box: releasing a scratch folder that is already gone is no error, and
# a kept scratch reading leaves out a claimed folder removed since
# (objective 220). A folder removed with its owner file before its release
# has no claim left to end: `spawn scratch release` prints {"gone":"<dir>"}
# and exits 0, writing nothing. A name that is not a plain absolute path to
# a missing folder is still refused. The storage line's reading, kept when
# a walk runs over its bound, lists only the claimed folders still there.
#
# Required:
#   (1) claim S/g, remove it (owner file and all), release S/g: exit 0,
#       stdout exactly {"gone":"S/g"}, and S/g is not made again.
#   (2) claim S/p/q, remove S/p, release S/p/q: exit 0, {"gone":"S/p/q"},
#       nothing made.
#   (3) refused (exit 1, no {"gone"} line, nothing made): a relative name;
#       a name with ..; a missing folder under a symlink to a directory; a
#       missing folder under a file; an existing folder with no claim;
#       renew of a removed folder.
#   (4) a claimed folder still there still releases: exit 0, its claim's
#       line with lease_until_ms at most now, the owner file kept.
#   (5) serve with "scratch_bound": {"entries": 100000, "ms": 30000} over a
#       root holding claimed folders G, K and L (leases running) and an
#       idle folder of 30 files: scratch.claimed lists G, K and L. Remove G,
#       replace L with a symlink to K, set the bound to 3 entries (a new
#       plan, so a new reading, which runs over its bound and keeps the
#       previous one): once serve's log says "ran over its bound (3
#       entries", scratch.claimed lists K only.
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "220: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep cut; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_220_gone_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# A scratch HOME and Claude Code projects dir: nothing here reads or writes
# the real ones.
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
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

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
serve_pid=""
cleanup() {
    for pid in "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "220: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "220: harness: unexpected exit $rc" >&2
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

# A failed check is reported and the run goes on; the run fails at its end
# if any did. Unless the database is gone: then nothing read can be trusted.
: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

note() { echo "220: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

S=$(cd "$WORK" && mkdir -p scratch && cd scratch && pwd -P)
claim() { # <dir>: claimed by rev-j for 4/220
    mkdir -p "$1" || harness "mkdir $1"
    capture "$SPAWN_BIN" scratch claim "$1" --agent rev-j --objective 4/220
    [ "$status" -eq 0 ] || harness "claiming $1: $(err)"
}

# --- (1) a folder removed with its owner file ------------------------------------------------------
claim "$S/g"
rm -rf "$S/g" || harness "cannot remove $S/g"
capture "$SPAWN_BIN" scratch release "$S/g"
expect_eq "$status" 0 "(1) release of a removed folder: exit (stderr: $(err | head -c 300))"
expect_eq "$(out)" "{\"gone\":\"$S/g\"}" "(1) release of a removed folder: stdout"
[ ! -e "$S/g" ] || check_failed "(1) release made $S/g again"

# --- (2) its parent removed too ----------------------------------------------------------------------
claim "$S/p/q"
rm -rf "$S/p" || harness "cannot remove $S/p"
capture "$SPAWN_BIN" scratch release "$S/p/q"
expect_eq "$status" 0 "(2) release of a folder whose parent is removed: exit (stderr: $(err | head -c 300))"
expect_eq "$(out)" "{\"gone\":\"$S/p/q\"}" "(2) release of a folder whose parent is removed: stdout"
[ ! -e "$S/p" ] || check_failed "(2) release made $S/p again"

# --- (3) still refused ---------------------------------------------------------------------------------
mkdir -p "$S/real" "$S/plain" || harness "mkdir"
ln -s "$S/real" "$S/link" || harness "ln -s"
: >"$S/file" || harness "cannot write $S/file"
claim "$S/r"
rm -rf "$S/r" || harness "cannot remove $S/r"
snapshot() { find "$S" -printf '%p %y %s %l\n' | sort; }
snapshot >"$WORK/before3.txt"
refused() { # <what> <args...>
    what=$1
    shift
    capture "$SPAWN_BIN" scratch "$@"
    expect_eq "$status" 1 "(3) $what: exit (stdout: $(out | head -c 200))"
    case $(out) in *'"gone"'*) check_failed "(3) $what: said gone: $(out | head -c 200)" ;; esac
}
(cd "$S" && capture "$SPAWN_BIN" scratch release gone-rel && echo "$status" >"$WORK/rel.status" && out >"$WORK/rel.out")
expect_eq "$(cat "$WORK/rel.status")" 1 "(3) a relative name: exit (stdout: $(head -c 200 "$WORK/rel.out"))"
! grep -q '"gone"' "$WORK/rel.out" || check_failed "(3) a relative name: said gone"
refused "a name with .." release "$S/real/../missing"
refused "a missing folder under a symlink" release "$S/link/missing"
refused "a missing folder under a file" release "$S/file/missing"
refused "an existing folder with no claim" release "$S/plain"
refused "renew of a removed folder" renew "$S/r"
snapshot >"$WORK/after3.txt"
cmp -s "$WORK/before3.txt" "$WORK/after3.txt" ||
    check_failed "(3) a refusal changed the scratch: $(diff "$WORK/before3.txt" "$WORK/after3.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"
# Any error on the way but "not found" is no "gone": a missing folder under a
# directory this user cannot search answers "permission denied" (root can
# search anything, so the check needs another user).
if [ "$(id -u)" -ne 0 ]; then
    mkdir -p "$S/shut" && chmod 000 "$S/shut" || harness "cannot make $S/shut"
    refused "a missing folder under a directory that cannot be searched" release "$S/shut/missing"
    chmod 700 "$S/shut" || harness "cannot open $S/shut again"
else
    note "(3) run as root: the permission-denied case is not checked"
fi

# --- (4) a claim still there still releases ----------------------------------------------------------
claim "$S/k"
capture "$SPAWN_BIN" scratch release "$S/k"
released=$(date +%s%3N)
expect_eq "$status" 0 "(4) release of a claimed folder: exit (stderr: $(err | head -c 300))"
expect_eq "$(out | jq -r '[.agent, .objective] | join(" ")' 2>/dev/null)" "rev-j 4/220" "(4) release prints its claim"
until_ms=$(out | jq -r '.lease_until_ms' 2>/dev/null)
case $until_ms in
    '' | *[!0-9]*) check_failed "(4) no lease_until_ms in [$(out | head -c 200)]" ;;
    *) [ "$until_ms" -le "$released" ] || check_failed "(4) the lease runs on: $until_ms > $released" ;;
esac
[ -f "$S/k/.spawn-owner" ] || check_failed "(4) the owner file is gone"

# --- (5) a kept reading leaves out a removed claimed folder ------------------------------------------
R=$(cd "$WORK" && mkdir -p root && cd root && pwd -P)
claim "$R/G"
claim "$R/K"
claim "$R/L"
mkdir -p "$R/idle" || harness "mkdir"
i=0
while [ "$i" -lt 30 ]; do
    printf x >"$R/idle/f$i" || harness "cannot write"
    i=$((i + 1))
done
find "$R/idle" -exec touch -h -d '2 days ago' {} + || harness "cannot age"

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
lines() { # <storage json>: the Core lines file
    printf '{"ranges": {}, "storage": %s}\n' "$1" >"$SPAWN_WORKSPACE_ROOT/core-lines.json.new" &&
        mv "$SPAWN_WORKSPACE_ROOT/core-lines.json.new" "$SPAWN_WORKSPACE_ROOT/core-lines.json" ||
        harness "cannot write core-lines.json"
}
start_serve() {
    : >"$WORK/serve.log" || harness "cannot empty serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/serve.log" 2>&1 &
    serve_pid=$!
    port=""
    i=0
    while [ "$i" -lt 100 ]; do
        i=$((i + 1))
        port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
        [ -n "$port" ] && break
        sleep 0.1
    done
    [ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
}
ask() { # the answer into $WORK/api.json
    code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 300 "$WORK/api.json")"
}
claimed() { jq -c '[(.scratch.claimed // [])[] | .path] | sort' "$WORK/api.json"; }

lines "{\"scratch_roots\": [\"$R\"], \"scratch_bound\": {\"entries\": 100000, \"ms\": 30000}}"
start_serve
ask
[ "$(claimed)" = "[\"$R/G\",\"$R/K\",\"$R/L\"]" ] ||
    harness "(5) the first reading does not list G, K and L: $(claimed | head -c 300)"
rm -rf "$R/G" || harness "cannot remove $R/G"
# L is removed and a symlink to K put in its place: never followed, so gone too.
rm -rf "$R/L" && ln -s "$R/K" "$R/L" || harness "cannot replace $R/L with a symlink"
lines "{\"scratch_roots\": [\"$R\"], \"scratch_bound\": {\"entries\": 3, \"ms\": 30000}}"
# The new plan's request starts the refresh and is answered from the kept
# reading; the next one, after the refresh, is the new reading.
ask
i=0
while [ "$i" -lt 100 ] && ! grep -q 'ran over its bound (3 entries' "$WORK/serve.log"; do
    i=$((i + 1))
    sleep 0.1
done
grep -q 'ran over its bound (3 entries' "$WORK/serve.log" ||
    harness "(5) the walk did not run over its 3-entry bound: $(grep -m 3 'core-lines' "$WORK/serve.log" | tr '\n' ' ' | head -c 400)"
sleep 0.2
ask
expect_eq "$(claimed)" "[\"$R/K\"]" "(5) the kept reading's claimed folders after G's removal and L's replacement by a symlink"
kill "$serve_pid" 2>/dev/null || true
wait "$serve_pid" 2>/dev/null || true
serve_pid=""

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "220: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "220: a removed folder's release is gone and no error, other names are still refused, a live claim still releases, and a kept reading lists only the claimed folders still there"
exit 0
