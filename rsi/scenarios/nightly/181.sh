#!/bin/sh
# Objective 181 [sustainability]: the sustainability line as the owner's
# core metric set defines it (2026-11-14 18:42; the lead's rulings of
# 2026-11-14): each north star's target beside its range, U1 out of the
# recovery table's rows, and no range for U2 in its first week of data.
#
# Required (through `GET /api/core-lines`):
#   (1) a ranges file entry's "target" is said as the figure's `target`,
#       apart from its range: the sustainability north star with {"max":
#       0.1, "target": 0.1} has both; the storage north star with a target
#       alone has a target, no range, and its entry is not left out.
#   (2) U1, "Critical resources with a restore passed in 7 days", is out of
#       the recovery table's rows: three rows (one passed an hour ago, one
#       never passed, one with no drill) read 1 `of` 3, its note naming the
#       three rows.
#   (3) U2, "Needs you is right", takes no range while it has no week of
#       judged data, whatever the file gives it ({"min": 0.8}): with no
#       sample carrying it, `range_note` is "range proposed 7 days after the
#       first judged value"; with its first sample two days ago, "range
#       proposed after <that day + 7>"; with its first sample eight days
#       ago, no note and the file's range.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on the base (185): BASE_TEXT

set -eu

asserted=""
harness() {
    echo "181: harness: $*" >&2
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
SPAWN_DB_NAME="bb_181_sustainability_$run"
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
        echo "181: the last answer's sustainability line:" >&2
        jq -c '.lines[] | select(.line == "sustainability")' "$WORK/api.json" >&2 2>/dev/null || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "181: harness: unexpected exit $rc" >&2
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
note() { echo "181: $*" >&2; }
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
    figure() { jq -c --arg key "$1" '[.lines[] | .north_star, .guardrails[]] | map(select(.key == $key)) | first' "$WORK/api.json"; }
    star=$(figure observability.interventions_per_agent_hour)
    storage_star=$(figure storage.bytes_per_landing)
    u1=$(figure recoverability.restores_passed)
    u2=$(figure observability.needs_you_right)
}

cat >"$SPAWN_CORE_LINES_FILE" <<'EOF'
{"ranges": {
   "observability.interventions_per_agent_hour": {"max": 0.1, "target": 0.1},
   "storage.bytes_per_landing": {"target": 52428800},
   "observability.needs_you_right": {"min": 0.8}},
 "recoverability": {"drills": [
   {"name": "Live database", "record": "backup.verified"},
   {"name": "Mission repositories", "record": "backup.drilled.missions"},
   {"name": "Controllers"}]}}
EOF
DAY=86400000
day() { date -u -d "@$(($1 / 1000))" +%Y-%m-%d; }
seed '{"type": "backup.verified", "ok": true, "key": "bb"}' $((now_ms - 3600000))

# --- (1) targets ------------------------------------------------------------------------

answer "targets"
expect_eq "$(printf '%s' "$star" | jq -c '[.target, .range]')" '[0.1,{"max":0.1}]' \
    "(1) the sustainability north star's target and range"
expect_eq "$(printf '%s' "$storage_star" | jq -c '[.target, .range]')" '[52428800,null]' \
    "(1) the storage north star's target alone"
expect_eq "$(jq -c '[.ignored_ranges[].key]' "$WORK/api.json")" '[]' "(1) entries left out"
note "(1) checked: targets"

# --- (2) U1 out of the rows ---------------------------------------------------------------

expect_eq "$(printf '%s' "$u1" | jq -c '[.value, .of]')" '[1,3]' "(2) U1: passed in 7 days, out of the rows"
expect_eq "$(printf '%s' "$u1" | jq -r '.note // ""')" "rows: Live database, Mission repositories, Controllers" "(2) U1's note"
note "(2) checked: U1"

# --- (3) U2's first week ------------------------------------------------------------------

expect_eq "$(printf '%s' "$u2" | jq -c '[.range, .range_note]')" '[null,"range proposed 7 days after the first judged value"]' \
    "(3) U2 before any judged value"
seed '{"type": "host.core_lines", "values": {"observability.needs_you_right": 0.5}}' $((now_ms - 2 * DAY))
answer "a first judged value two days ago"
expect_eq "$(printf '%s' "$u2" | jq -c '[.range, .range_note]')" "[null,\"range proposed after $(day $((now_ms + 5 * DAY)))\"]" \
    "(3) U2 in its first week"
seed '{"type": "host.core_lines", "values": {"observability.needs_you_right": 0.4}}' $((now_ms - 8 * DAY))
answer "a first judged value eight days ago"
expect_eq "$(printf '%s' "$u2" | jq -c '[.range, has("range_note")]')" '[{"min":0.8},false]' "(3) U2 after its first week"
note "(3) checked: U2"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "181: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "181: each north star says its target, U1 is out of the recovery table's rows, U2 takes no range in its first week"
exit 0
