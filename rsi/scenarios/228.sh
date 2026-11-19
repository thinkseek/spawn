#!/bin/sh
# Objective 228 [storage]: storage.disk_fall's day figure reads each end
# as the median of a 6 h window of samples, so a gate holding build trees
# at one sample's minute no longer moves it.
#
# The problem (rev-i's 4/222 observation, 2026-11-19): the day's fall was
# read from one sample, the latest a day old or more, to the free disk now.
# A gate's build trees move df's free disk by about 6 GB within 10 minutes,
# so the figure, which worse() often says (basis "day"), moved by up to
# 6 GB a day with whether a gate held trees at that sample's minute. On the
# lead's 99 live hourly samples of 11-16..11-19 the one-sample figure moved
# 6.91 GB an hour on average; each end's median over 6 h moves 3.31 GB and
# still says the 11-18 day (66.9 GB).
#
# Required (through `GET /api/core-lines` of a real `spawn serve`, its
# storage.disk_fall diagnostic; hourly host.core_lines samples planted as a
# loop records them, as 222.sh does, their free disk set against the free
# disk F the serve reads, "now" the database's clock):
#   (1) a flat week at F + 50 GB, then falling through the last day to F:
#       disk_fall is the day's, basis "day", within 2 GB of 44.79 GB a day:
#       a day ago the median of the readings 24 h to 29 h old (F + 50) at
#       26.5 h; now the median of those 1 h to 5 h old and F (F + 5.21) at
#       2.5 h. (The one sample a day old would say 50.)
#   (2) the same, but the sample 24 h old, the one sample main read, 6 GB
#       lower (a gate's trees): disk_fall within 1 GB of (1)'s, basis
#       "day". Main moves by 6 GB.
#   (3) the same as (1), but the sample 1 h old 6 GB lower: within 1 GB of
#       (1)'s, basis "day".
#   (4) (R2, rev-b's F1) a flat week at F + 50 GB and no sample between 2 h
#       and 23 h ago, the one sample 1 h old at F + 20 GB: now's window is
#       that reading and F, an even count, median F + 10 at 0.5 h; a day ago
#       F + 50 at 26.5 h. disk_fall within 2 GB of 36.92 GB a day, basis
#       "day". Without F in now's window, or with the upper of the middle
#       two, now reads F + 20 and the figure is 28.2 or 27.7.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on live main (a4ac64dc): (1) says 50 GB a day and (2) moves
# by 6 GB.

set -eu

asserted=""
harness() {
    echo "228: harness: $*" >&2
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
SPAWN_DB_NAME="bb_228_window_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
test_made=""
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
    if [ -n "$test_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$TEST_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "228: the last answer's disk figures:" >&2
        jq -c '[.diagnostics[]? | select(.key == "storage.disk_fall" or .key == "storage.free_bytes")]' "$WORK/api.json" >&2 2>/dev/null || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "228: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1
# The serve's test server: a database of the scenario's own.
TEST_DB_NAME="${SPAWN_DB_NAME}_t"
psql -d postgres -tA -c "create database $TEST_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario's test database"
test_made=1

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
printf '{"ranges": {}}\n' >"$SPAWN_CORE_LINES_FILE"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

q() { scalar "$1"; }
note() { echo "228: $*" >&2; }
HOUR=3600000
DAY=$((24 * HOUR))
GB=1000000000

answer() { # <what>: GET /api/core-lines into WORK/api.json
    # The last serve's log goes first: the redirection below truncates it in
    # the child, so a read right after the fork could see the old port.
    rm -f "$WORK/serve.log" "$WORK/api.json"
    SPAWN_TEST_DATABASE_URL="$server_url/$TEST_DB_NAME" \
        "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >"$WORK/serve.log" 2>&1 &
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
}
# The disk_fall diagnostic: "<value> <basis>", "none" for a missing field.
fall_said() {
    jq -r '[.diagnostics[]? | select(.key == "storage.disk_fall")] | first
        | if . == null then "missing" else "\(.value // "none") \(.basis // "none")" end' "$WORK/api.json"
}
free_now() {
    jq -r '[.diagnostics[]? | select(.key == "storage.free_bytes")] | first | .value // empty' "$WORK/api.json"
}

# samples <F> <a> <b> <newest ms ago> [<gap from> <gap to>]: hourly host.core_lines samples from 7 d
# + 2 h ago down to <newest ms ago>, the free disk of one g ms ago being
# F + a * g / 24 h through the last day (g < 24 h) and F + a + b * (g - 24 h)
# / 24 h before it (bytes; a and b in GB); none in [gap from, gap to] ms ago.
samples() {
    cat >"$WORK/samples.sql" <<'SQL'
delete from spawn_events
 where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines';
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
         jsonb_build_object('type', 'host.core_lines', 'values', jsonb_build_object('storage.free_bytes',
           case when g < :day::bigint
                then :free::numeric + :a::numeric * :gb::numeric * g / :day::numeric
                else :free::numeric + :a::numeric * :gb::numeric
                     + :b::numeric * :gb::numeric * (g - :day::bigint) / :day::numeric end)))),
       (select coalesce(max(session_seq), 0) from spawn_events where session_id = 'session_control') + row_number() over (order by g),
       :now::bigint - g
  from generate_series((7 * :day::bigint + 2 * :hour::bigint), :newest::bigint, -:hour::bigint) g
 where not (g between :gap_from::bigint and :gap_to::bigint);
SQL
    now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
    case "$now_ms" in '' | *[!0-9]*) harness "no time from the database: [$now_ms]" ;; esac
    capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -v now="$now_ms" -v free="$1" -v a="$2" -v b="$3" \
        -v newest="$4" -v gap_from="${5:--2}" -v gap_to="${6:--1}" -v day="$DAY" -v hour="$HOUR" -v gb="$GB" -f "$WORK/samples.sql"
    [ "$status" -eq 0 ] || harness "seeding the samples failed: $(err)"
}
within_gb() { # <got> <want>: |got - want| <= 2 GB (integers; the free disk moves under load)
    case "$1" in '' | *[!0-9-]*) return 1 ;; esac
    d=$(($1 - $2))
    [ "${d#-}" -le $((2 * GB)) ]
}
# The store that grew most: "<store> <bytes a day>", "none" for a missing field.
grew_said() {
    jq -r '[.diagnostics[]? | select(.key == "storage.grew_most")] | first
        | if . == null then "missing" else "\(.store // "none") \(.value // "none")" end' "$WORK/api.json"
}

# dip <hours ago> <GB>: the planted sample <hours ago> (as samples() planted
# it, at its now_ms) lowered by <GB>, as a gate's build trees lower df.
dip() {
    capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -c "update spawn_events
        set kind = jsonb_set(kind, '{detail,record,values,storage.free_bytes}',
              to_jsonb((kind #>> '{detail,record,values,storage.free_bytes}')::numeric - $2::numeric * $GB))
      where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
        and recorded_at_ms = $now_ms - $1 * $HOUR"
    [ "$status" -eq 0 ] || harness "lowering the sample $1 h old failed: $(err)"
    [ "$(q "select count(*) from spawn_events where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
              and recorded_at_ms = $now_ms - $1 * $HOUR")" -eq 1 ] || harness "no sample $1 h old to lower"
}
# set_to <hours ago> <GB>: the planted sample <hours ago> set to F + <GB>.
set_to() {
    capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -c "update spawn_events
        set kind = jsonb_set(kind, '{detail,record,values,storage.free_bytes}', to_jsonb($F::numeric + $2::numeric * $GB))
      where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
        and recorded_at_ms = $now_ms - $1 * $HOUR"
    [ "$status" -eq 0 ] || harness "setting the sample $1 h old failed: $(err)"
    [ "$(q "select count(*) from spawn_events where session_id = 'session_control'
              and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
              and recorded_at_ms = $now_ms - $1 * $HOUR")" -eq 1 ] || harness "no sample $1 h old to set"
}
within() { # <got> <want> <bytes>: |got - want| <= bytes
    case "$1" in '' | *[!0-9-]*) return 1 ;; esac
    d=$(($1 - $2))
    [ "${d#-}" -le "$3" ]
}

answer "no sample"
F=$(free_now)
case "$F" in '' | *[!0-9]*) harness "the serve reads no free disk: [$F]" ;; esac

# --- (1) a flat week, a day falling 50 GB: the windows' medians ------------------------------
samples "$F" 50 0 "$HOUR"
answer "a flat week, a day falling 50 GB"
set -- $(fall_said)
steady=$1
within "$steady" 44791667000 $((2 * GB)) ||
    check_failed "(1) disk_fall from the windows' medians: expected within 2 GB of 44791667000, got [$steady]"
expect_eq "$2" day "(1) disk_fall's basis"
note "(1) checked: $steady"

# --- (2) a gate's 6 GB on the sample a day old -----------------------------------------------
F=$(free_now)
samples "$F" 50 0 "$HOUR"
dip 24 6
answer "the sample 24 h old 6 GB lower"
set -- $(fall_said)
within "$1" "$steady" "$GB" ||
    check_failed "(2) disk_fall with the sample 24 h old 6 GB lower: expected within 1 GB of (1)'s $steady, got [$1]"
expect_eq "$2" day "(2) disk_fall's basis"
note "(2) checked: $1"

# --- (3) a gate's 6 GB on the sample an hour old ---------------------------------------------
F=$(free_now)
samples "$F" 50 0 "$HOUR"
dip 1 6
answer "the sample 1 h old 6 GB lower"
set -- $(fall_said)
within "$1" "$steady" "$GB" ||
    check_failed "(3) disk_fall with the sample 1 h old 6 GB lower: expected within 1 GB of (1)'s $steady, got [$1]"
expect_eq "$2" day "(3) disk_fall's basis"
note "(3) checked: $1"

# --- (4) now's window: one reading and the free disk now (R2, rev-b's F1) -------------------
F=$(free_now)
samples "$F" 50 0 "$HOUR" $((2 * HOUR)) $((23 * HOUR))
set_to 1 20
answer "now's window holding one reading, F + 20 GB at 1 h"
set -- $(fall_said)
within "$1" 36923077000 $((2 * GB)) ||
    check_failed "(4) disk_fall with now's window one reading at F + 20 GB and F: expected within 2 GB of 36923077000, got [$1]"
expect_eq "$2" day "(4) disk_fall's basis"
note "(4) checked: $1"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "228: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "228: storage.disk_fall's day figure reads each end as a 6 h window's median"
exit 0
