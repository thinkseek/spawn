#!/bin/sh
# Black box: the storage line's "scratch nobody owns": the bytes under the
# scratch roots whose owner is gone or which nothing has touched in a day,
# and the test schemas no session holds; read only, nothing removed.
#
# The problem (owner, 2026-11-12; sync/roadmap-effects.md, Storage): the
# guardrail is counted by hand under /var/tmp/spawn and in the test
# databases: about 3.3 GB of files and 1,214 leaked test schemas, against a
# range of 2 GB and 200. Nothing says which scratch is still somebody's: a
# test schema's name carries a pid from the gate's own PID namespace, the
# database keeps no time a schema was made, and almost no scratch directory
# says who made it. Monitoring only: no gate, integrator or deploy reads any
# of it, and nothing here removes anything (a sweep is the owner's call).
#
# Required (read through `GET /api/core-lines`):
#   (1) core-lines.json's `storage` {scratch_roots: [<dir>], scratch_exclude:
#       [<path>]}. Each top-level entry of a root is owned (its
#       `.spawn-owner` "<pid> <start ticks>" names a process that still runs
#       at that start time, or, with no marker, something in it changed in
#       the last 24 h), gone (its marker names a process that does not run,
#       or a pid used again), or idle (no marker, nothing changed in 24 h).
#       Never counted, by fixed rule: symlinks (never followed, at any
#       depth), an entry holding any *.jsonl file (a transcript tree),
#       `.transcripts`, names starting `incident-` or ending `.pending` or
#       `-pending`, and the configured excludes. The answer lists `scratch`
#       {roots, unowned: [{path, bytes, reason gone|idle}]}, the 20 largest
#       first.
#   (2) the figures: `storage.unowned_scratch_bytes` ("Scratch nobody owns",
#       bytes: the gone and idle entries' bytes) and
#       `storage.leaked_test_schemas` ("Leaked test schemas", count): in the
#       test databases, the schemas but public and the system's that no live
#       session holds (no session's application_name is the schema's name)
#       and that were not made in the last hour (the comment on the schema
#       is {"made_at_ms": <ms>}; none, or one that does not read, is old).
#   (3) read only: every file, directory, symlink and schema is still there,
#       unchanged, after the answer.
#
# The history: a scratch root holding an entry the scenario's own shell
# owns; one whose marker names a process that exited; one with the shell's
# pid at another start time; an idle unmarked directory (2 days old) with a
# symlink inside to a big directory elsewhere; a fresh unmarked one; a
# top-level symlink; idle `.transcripts`, `incident-...`, `x.rebuilt-pending`
# and `y.pending` entries, an idle one holding a .jsonl file and an idle
# configured exclude. A test database of its own: a schema a live session
# holds, one made now, one made 2 hours ago, one with no comment, one whose
# comment is not JSON, and one more by another name.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "094: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test: the gate runs a scenario from the candidate's tree,
# and $controller is the deployed controller's copy, which lacks the
# candidate's web/src until it lands.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep sha1sum cut node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_094_scratch_$run"
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
tests_made=""
serve_pid=""
held_pid=""
TESTS_DB="bb_094_tests_$run"
cleanup() {
    for pid in "$serve_pid" "$held_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    if [ -n "$tests_made" ]; then
        psql -d postgres -q -c "drop database if exists $TESTS_DB with (force)" >/dev/null 2>&1 || true
    fi
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
        echo "094: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "094: harness: unexpected exit $rc" >&2
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
psql -d postgres -tA -c "create database $TESTS_DB" >/dev/null 2>&1 ||
    harness "cannot create the scenario's test database"
tests_made=1

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


note() { echo "094: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac

# The scratch root. Sizes are exact, so the expected bytes are the files'.
S=$WORK/scratch
fill() { # <file> <bytes>
    mkdir -p "$(dirname "$1")"
    head -c "$2" /dev/zero >"$1" || harness "cannot write $1"
}
ticks() { awk '{ print $22 }' "/proc/$1/stat"; }
my_ticks=$(ticks $$)
case "$my_ticks" in '' | *[!0-9]*) harness "no start time for this shell: [$my_ticks]" ;; esac
sh -c 'exit 0' &
dead=$!
wait "$dead" || true
[ ! -e "/proc/$dead" ] || harness "the exited process $dead is still there"

fill "$S/owned/data" 100000
printf '%s %s\n' "$$" "$my_ticks" >"$S/owned/.spawn-owner"
fill "$S/gone/data" 300000
printf '%s %s\n' "$dead" "$my_ticks" >"$S/gone/.spawn-owner"
fill "$S/reused/data" 200000
printf '%s %s\n' "$$" "$((my_ticks + 1))" >"$S/reused/.spawn-owner"
fill "$S/idle/a/data" 400000
fill "$S/idle/b" 1000
fill "$WORK/outside/big" 5000000
ln -s "$WORK/outside" "$S/idle/a/elsewhere"
fill "$S/fresh/data" 50000
ln -s "$WORK/outside" "$S/link"
fill "$S/.transcripts/x" 70000
fill "$S/incident-2026-11-12-x/x" 70000
fill "$S/x.rebuilt-pending/x" 70000
fill "$S/y.pending/x" 70000
fill "$S/sessions/deep/s.jsonl" 70000
fill "$S/sessions/other" 70000
fill "$S/notes/x" 70000
# Two days without a change: every entry but the fresh one and the link.
for entry in idle .transcripts incident-2026-11-12-x x.rebuilt-pending y.pending sessions notes; do
    find "$S/$entry" -exec touch -h -d '2 days ago' {} + || harness "cannot age $entry"
done
# The owned entry is as old: only its marker keeps it someone's.
find "$S/owned" "$S/gone" "$S/reused" -exec touch -h -d '2 days ago' {} +
want_bytes=$(find "$S/gone" "$S/reused" "$S/idle" -type f -printf '%s\n' | awk '{ s += $1 } END { print s }')
size_of() { find "$S/$1" -type f -printf '%s\n' | awk '{ s += $1 } END { print s }'; }
snapshot() { find "$S" "$WORK/outside" -printf '%p %y %s %T@ %l\n' | sort; }
snapshot >"$WORK/before.txt"

cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {},
 "storage": {"scratch_roots": ["$S"], "scratch_exclude": ["$S/notes"]}}
JSON

# The test database's schemas, as Spawn's tests make them: the comment is
# what a test writes when it makes its schema.
SPAWN_TEST_DATABASE_URL="$server_url/$TESTS_DB"
export SPAWN_TEST_DATABASE_URL
p="spawn_test_1_a_0"
cat >"$WORK/tests.sql" <<SQL
\set ON_ERROR_STOP 1
create schema ${p}_held;
create schema ${p}_fresh;
comment on schema ${p}_fresh is '{"made_at_ms": $seed_now}';
create schema ${p}_old;
comment on schema ${p}_old is '{"made_at_ms": $((seed_now - 7200000))}';
create schema ${p}_bare;
create schema ${p}_junk;
comment on schema ${p}_junk is 'made by hand';
create schema bb_other;
create table ${p}_old.t (x int);
SQL
capture psql -d "$TESTS_DB" -q -f "$WORK/tests.sql"
[ "$status" -eq 0 ] || harness "seeding the test schemas failed: $(err)"
schemas() {
    psql -d "$TESTS_DB" -tA -c "select string_agg(nspname || '=' || coalesce(obj_description(oid, 'pg_namespace'), ''), ',' order by nspname) from pg_namespace where nspname not like 'pg\\_%' and nspname <> 'information_schema'"
}
schemas >"$WORK/schemas-before.txt" || harness "cannot list the test schemas"
# A live session holding the held schema, as a running test's connection.
PGAPPNAME=${p}_held psql -d "$TESTS_DB" -q -c "select pg_sleep(300)" >/dev/null 2>&1 &
held_pid=$!
i=0
until [ "$(psql -d "$TESTS_DB" -tA -c "select count(*) from pg_stat_activity where application_name = '${p}_held'")" = 1 ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || harness "the holding session did not start"
    sleep 0.1
done

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
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

code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
if [ "$code" != 200 ]; then
    check_failed "(1) GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"
else
    expect_eq "$(jq -c '.scratch.roots' "$WORK/api.json")" "[\"$S\"]" "(1) the scratch roots read"
    expect_eq "$(jq -c '[.scratch.unowned[] | {path, bytes, reason}]' "$WORK/api.json")" \
        "[{\"path\":\"$S/idle\",\"bytes\":$(size_of idle),\"reason\":\"idle\"},{\"path\":\"$S/gone\",\"bytes\":$(size_of gone),\"reason\":\"gone\"},{\"path\":\"$S/reused\",\"bytes\":$(size_of reused),\"reason\":\"gone\"}]" \
        "(1) nobody's entries, largest first: idle (its symlink not followed), an exited owner, a pid used again; not the owned, the fresh, the link or any excluded entry"
    fig() { jq -c ".lines[] | select(.line == \"storage\") | ([.north_star] + .guardrails)[] | select(.key == \"$1\") | {name, unit, value}" "$WORK/api.json"; }
    expect_eq "$(fig storage.unowned_scratch_bytes)" "{\"name\":\"Scratch nobody owns\",\"unit\":\"bytes\",\"value\":$want_bytes}" \
        "(2) scratch nobody owns: the gone and idle entries' bytes"
    expect_eq "$(fig storage.leaked_test_schemas)" '{"name":"Leaked test schemas","unit":"count","value":4}' \
        "(2) leaked test schemas: the one made 2 h ago, the bare one, the one whose comment is not JSON and bb_other; not the held or the fresh one"
    note "(1) and (2) checked"
fi
# Read only.
snapshot >"$WORK/after.txt"
if ! cmp -s "$WORK/before.txt" "$WORK/after.txt"; then
    check_failed "(3) the scratch changed: $(diff "$WORK/before.txt" "$WORK/after.txt" | head -n 6 | tr '\n' ' ' | head -c 400)"
fi
schemas >"$WORK/schemas-after.txt" || harness "cannot list the test schemas"
cmp -s "$WORK/schemas-before.txt" "$WORK/schemas-after.txt" ||
    check_failed "(3) the test schemas changed: before [$(cat "$WORK/schemas-before.txt")] after [$(cat "$WORK/schemas-after.txt")]"
expect_eq "$(psql -d "$TESTS_DB" -tA -c "select count(*) from pg_stat_activity where application_name = '${p}_held'")" 1 \
    "(3) the holding session still runs"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "094: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "094: scratch nobody owns and leaked test schemas are counted, read only"
exit 0
