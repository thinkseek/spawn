#!/bin/sh
# Objective 185 [storage]: the storage north star, "Bytes a landing leaves
# behind", and S1, "Rebuildable bytes on disk", measured from the storage
# readings (objective 180).
#
# The problem (the owner's core metric set, 2026-11-14 18:42; objective 176
# shows both as "not measured yet"): the storage line steers by what each
# landing leaves on the disk, and by how much of the disk is rebuildable.
# 180 records, once a day, the real bytes of each source (`host.storage_bytes`:
# transcripts, build cache, scratch, session backups, test databases, the
# live database, each with its bytes or why it is not measured). The north
# star is the gain between the newest reading and the newest a day older,
# over the landings between them (every task.landed and every owner's
# landing, on every mission and source); S1 the newest reading's scratch,
# build cache and test databases.
#
# Required (through `GET /api/core-lines`), with readings as the loop
# records them (control records):
#   (1) one reading: the north star is not measured yet and says why; S1 is
#       the reading's scratch, build cache and test databases, by source.
#   (2) a reading two days ago and one an hour ago, with two task.landed,
#       an owner's landing (a source.moved naming its request) and a
#       deploy's source.moved (no landing) between them, and one task.landed
#       before the older: the north star is the gain of the sources measured
#       in both (6 MiB) over the 3 landings, each source's gain in
#       `by_source`, the sources measured in one reading only named in its
#       note; S1 is 9 MiB.
#   (3) a newest reading a minute old, its scratch over its bound (not
#       measured): S1 is not measured and says why; the north star is still
#       read against the reading two days back (the newest at least a day
#       older), not the one an hour old.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on the base (180): "(1) S1, rebuildable bytes (one reading):
# expected [3145728], got [null]"; "(2) the storage north star (6 MiB over 3
# landings ...): expected [2097152], got [null]".

set -eu

asserted=""
harness() {
    echo "185: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od sed grep curl head; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
[ -d "$WORK" ] || harness "mktemp gave no directory"
trap 'rm -rf "$WORK"' EXIT
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_185_storage_figures_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
serve_pid=""
stop_serve() {
    [ -n "$serve_pid" ] || return 0
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
cleanup() {
    stop_serve
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "185: the last answer's storage line:" >&2
        jq -c '.lines[] | select(.line == "storage")' "$WORK/api.json" >&2 2>/dev/null || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "185: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

for name in SPAWN_SERVE_DATABASE_URL SPAWN_SESSION_BACKUP_STATUS SPAWN_FLEET_TEST_DATABASE_URL \
    SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
export HOME
mkdir -p "$HOME"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_CORE_LINES_FILE=$WORK/core-lines.json
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROJECTS SPAWN_CORE_LINES_FILE
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$SPAWN_CLAUDE_PROJECTS" "$WORK/web"
printf '{}\n' >"$SPAWN_CORE_LINES_FILE"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

q() { scalar "$1"; }
note() { echo "185: $*" >&2; }
KIB=1024
MIB=1048576
now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$now_ms" in '' | *[!0-9]*) harness "no time from the database: [$now_ms]" ;; esac

seed() { # <record JSON> <recorded_at_ms>: a control record after every other, as the loop's
    cat >"$WORK/seed.sql" <<'SQL'
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', :'record'::jsonb)),
       coalesce(max(session_seq), 0) + 1, :at
  from spawn_events where session_id = 'session_control';
SQL
    capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -v record="$1" -v at="$2" -f "$WORK/seed.sql"
    [ "$status" -eq 0 ] || harness "seeding $1 failed: $(err)"
}
reading() { # <scratch entry> <transcripts bytes>: a host.storage_bytes record
    printf '{"type": "host.storage_bytes", "sources": {"transcripts": {"bytes": %s}, "build_cache": {"bytes": %s}, "scratch": %s, "test_databases": {"bytes": 0, "databases": 0}, "session_backups": {"not_measured": "no status file"}, "live_database": %s}}' \
        "$2" "$MIB" "$1" "$3"
}
answer() { # <what>: GET /api/core-lines into WORK/api.json; sets storage
    # Emptied here, in the scenario's shell: a redirect in the background
    # serve truncates only in the child process, so the port read below
    # could still see the last serve's port (the 155.sh pattern).
    : >"$WORK/serve.log" || harness "cannot empty serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >>"$WORK/serve.log" 2>&1 &
    serve_pid=$!
    port=""
    i=0
    while [ "$i" -lt 100 ]; do
        i=$((i + 1))
        port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
        [ -n "$port" ] && break
        sleep 0.1
    done
    [ -n "$port" ] || harness "serve did not start ($1): $(head -c 400 "$WORK/serve.log")"
    code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code ($1): $(head -c 200 "$WORK/api.json")"
    stop_serve
    storage=$(jq -c '.lines[] | select(.line == "storage")' "$WORK/api.json")
    star=$(printf '%s' "$storage" | jq -c '.north_star')
    s1=$(printf '%s' "$storage" | jq -c '.guardrails[] | select(.key == "storage.rebuildable_bytes")')
}

# --- (1) one reading ------------------------------------------------------------------

seed '{"type": "task.landed", "id": 900}' $((now_ms - 3 * 86400000))
seed "$(reading "{\"bytes\": $((2 * MIB))}" $((512 * KIB)) '{"not_measured": "seeded"}')" $((now_ms - 2 * 86400000))
answer "one reading"
expect_eq "$(printf '%s' "$star" | jq -c '[.key, .value, .state]')" '["storage.bytes_per_landing",null,"not_measured_yet"]' \
    "(1) the storage north star with one reading"
expect_eq "$(printf '%s' "$star" | jq -r '.note // ""')" "not measured yet: the figure needs two storage readings a day apart" \
    "(1) the north star's note"
expect_eq "$(printf '%s' "$s1" | jq -c '.value')" $((3 * MIB)) "(1) S1, rebuildable bytes (one reading)"
expect_eq "$(printf '%s' "$s1" | jq -cS '.by_source')" "{\"build_cache\":$MIB,\"scratch\":$((2 * MIB)),\"test_databases\":0}" \
    "(1) S1 by source"
note "(1) checked: one reading"

# --- (2) two readings a day apart -------------------------------------------------------

seed '{"type": "task.landed", "id": 901}' $((now_ms - 2 * 86400000 + 1000))
seed '{"type": "task.landed", "id": 902}' $((now_ms - 86400000))
seed '{"type": "source.moved", "source": "/s", "from": "a", "to": "b", "request": 77}' $((now_ms - 86400000 + 1000))
seed '{"type": "source.moved", "source": "/s", "from": "b", "to": "c"}' $((now_ms - 86400000 + 2000))
seed "$(reading "{\"bytes\": $((8 * MIB))}" $((512 * KIB)) '{"bytes": 999}')" $((now_ms - 3600000))
answer "two readings"
expect_eq "$(printf '%s' "$star" | jq -c '.value')" $((6 * MIB / 3)) \
    "(2) the storage north star (6 MiB over 3 landings; its note: $(printf '%s' "$star" | jq -r '.note // ""'))"
expect_eq "$(printf '%s' "$star" | jq -cS '.by_source')" \
    "{\"build_cache\":0,\"scratch\":$((6 * MIB)),\"test_databases\":0,\"transcripts\":0}" "(2) the north star by source"
printf '%s' "$star" | jq -r '.note // ""' | grep -qF "not measured in both readings: live_database, session_backups" ||
    check_failed "(2) the north star's note does not name the sources measured in one reading only: $(printf '%s' "$star" | jq -r '.note // ""')"
printf '%s' "$star" | jq -r '.note // ""' | grep -qF "3 landing(s) in the 47 h between the two readings" ||
    check_failed "(2) the north star's note does not give its landings and span: $(printf '%s' "$star" | jq -r '.note // ""')"
expect_eq "$(printf '%s' "$s1" | jq -c '.value')" $((9 * MIB)) "(2) S1, rebuildable bytes"
note "(2) checked: two readings"

# --- (3) scratch over its bound ---------------------------------------------------------

seed "$(reading '{"not_measured": "over the bound of 1200 s (9 files read)"}' $((512 * KIB)) '{"bytes": 999}')" $((now_ms - 60000))
answer "scratch over its bound"
expect_eq "$(printf '%s' "$s1" | jq -c '[.value, .state]')" '[null,"not_measured_yet"]' "(3) S1 with scratch over its bound"
expect_eq "$(printf '%s' "$s1" | jq -r '.note // ""')" "not measured: scratch: over the bound of 1200 s (9 files read)" "(3) S1's note"
# The older reading is still the one two days back, not the one an hour
# before the newest: the same 3 landings, scratch left out (measured in one).
printf '%s' "$star" | jq -r '.note // ""' | grep -qF "3 landing(s) in the 47 h between the two readings" ||
    check_failed "(3) the north star is not read against the reading a day older: $(printf '%s' "$star" | jq -c '{value, note}')"
note "(3) checked: scratch over its bound"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "185: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "185: the storage north star and S1 are measured from the storage readings"
exit 0
