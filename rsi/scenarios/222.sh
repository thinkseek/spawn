#!/bin/sh
# Objective 222 [storage][observability]: storage.disk_fall says the worse
# of the week's fit and the last day's fall, with its basis.
#
# The problem (4/141's cross-check, 2026-11-18): df's free disk fell 77 GB in
# 24 h while storage.disk_fall, the least-squares line through the week's
# daily readings, read -4.7 GB a day ("gaining"): the week's sweeps lifted
# the line through a day that lost 77 GB.
#
# Required (through `GET /api/core-lines` of a real `spawn serve`, its
# storage.disk_fall diagnostic; hourly host.core_lines samples planted as a
# loop records them, their free disk set against the free disk F the serve
# reads, "now" the database's clock):
#   (1) a week rising 10 GB a day up to F + 80 GB a day ago, then falling
#       through the last day to F: disk_fall is the day's, within 2 GB of
#       80 GB a day, its basis "day"; the store that grew most (no store
#       measured: unattributed) is the week's fit, under 40 GB a day, never
#       the day's 80 (R2, rev-e's F1);
#   (1b) the same, but no sample between 23 h and 29 h ago (the latest a
#       day old is 30 h old, F + 77.5 GB): 77.5 GB over 30 h, within 2 GB of
#       62 GB a day, basis "day";
#   (2) a week falling 10 GB a day down to F - 10 GB a day ago, then rising
#       through the last day to F: disk_fall is the fit's, above 0 and not
#       the day's (below 0), its basis "fit";
#   (3) the same week as (1), but no sample in the last 37 h (the newest a
#       day old is 37 h old, F + 74.6 GB, older than 36 h; taken it would
#       say 48 GB a day): the fit's, basis "fit";
#   (4) no sample at all: disk_fall has no value and no basis;
#   (5) two samples only: F + 25 GB at 24 h + 60 s ago (yesterday, UTC) and
#       F + 10 GB 60 s before today's UTC midnight (yesterday's last
#       reading): the readings span under a day, so no fit; disk_fall is
#       the day's, within 2 GB of 25 GB a day, basis "day".
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on live main (b701a85f): disk_fall is the fit's in (1) (2.6 GB
# a day on the first run) and no figure says a basis.

set -eu

asserted=""
harness() {
    echo "222: harness: $*" >&2
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
SPAWN_DB_NAME="bb_222_fall_$run"
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
        echo "222: the last answer's disk figures:" >&2
        jq -c '[.diagnostics[]? | select(.key == "storage.disk_fall" or .key == "storage.free_bytes")]' "$WORK/api.json" >&2 2>/dev/null || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "222: harness: unexpected exit $rc" >&2
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
note() { echo "222: $*" >&2; }
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

# --- (4) no sample: no value, no basis --------------------------------------------------------
answer "no sample"
expect_eq "$(fall_said)" "none none" "(4) disk_fall with no sample"
F=$(free_now)
case "$F" in '' | *[!0-9]*) harness "the serve reads no free disk: [$F]" ;; esac
note "(4) checked: no sample, no value and no basis (F = $F)"

# --- (1) the week rose, the last day lost 80 GB: the day's ------------------------------------
samples "$F" 80 -10 "$HOUR"
answer "a week rising, a day falling 80 GB"
set -- $(fall_said)
within_gb "$1" $((80 * GB)) ||
    check_failed "(1) disk_fall after a day that lost 80 GB: expected within 2 GB of $((80 * GB)), got [$1]"
expect_eq "$2" day "(1) disk_fall's basis after a day that lost 80 GB"
# No store is measured in this world, so the store that grew most is what
# none accounts for: the week's fit (a few GB a day), never the day's 80.
set -- $(grew_said)
expect_eq "$1" unattributed "(1) the store that grew most with no store measured"
case "$2" in '' | *[!0-9-]*) check_failed "(1) the store that grew most has no value: [$2]" ;; *)
    [ "$2" -lt $((40 * GB)) ] || check_failed "(1) the store that grew most says $2 a day, the day's fall, not the week's fit" ;;
esac
note "(1) checked: a week that rose and a day that lost 80 GB says the day's fall"

# --- (1b) the latest day-old sample 30 h old: scaled by the days between ---------------------
F=$(free_now)
samples "$F" 80 -10 "$HOUR" $((23 * HOUR)) $((29 * HOUR))
answer "the latest day-old sample 30 h old"
set -- $(fall_said)
within_gb "$1" $((62 * GB)) ||
    check_failed "(1b) disk_fall from a sample 30 h old at F + 77.5 GB: expected within 2 GB of $((62 * GB)), got [$1]"
expect_eq "$2" day "(1b) disk_fall's basis from a sample 30 h old"
note "(1b) checked: the day's fall is scaled by the days between"

# --- (2) the week fell, the last day gained 10 GB: the fit's ----------------------------------
F=$(free_now)
samples "$F" -10 10 "$HOUR"
answer "a week falling, a day gaining 10 GB"
set -- $(fall_said)
case "$1" in '' | *[!0-9]*) check_failed "(2) disk_fall after a week that fell: expected above 0 (the fit's), got [$1]" ;; *)
    [ "$1" -gt 0 ] || check_failed "(2) disk_fall after a week that fell: expected above 0, got [$1]" ;;
esac
expect_eq "$2" fit "(2) disk_fall's basis after a week that fell and a day that gained"
note "(2) checked: a week that fell and a day that gained says the fit's fall"

# --- (3) no sample in the last 37 h: the fit's alone ------------------------------------------
F=$(free_now)
samples "$F" 80 -10 $((37 * HOUR))
answer "no sample in the last 37 h"
set -- $(fall_said)
case "$1" in '' | *[!0-9-]*) check_failed "(3) disk_fall with no sample in 37 h: expected the fit's (a number), got [$1]" ;; *)
    within_gb "$1" $((48 * GB)) && check_failed "(3) disk_fall with no sample in 37 h reads the stale sample's 48 GB a day: [$1]" ;;
esac
expect_eq "$2" fit "(3) disk_fall's basis with no sample in the last 37 h"
note "(3) checked: no day-old sample within 36 h says the fit's fall"

# --- (5) the day's fall with no fit --------------------------------------------------------
F=$(free_now)
now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
tod=$((now_ms % DAY))
# Yesterday must hold both samples: 24 h + 60 s ago is yesterday once today
# is 60 s old; 60 s before midnight is later than a day ago until 23:59.
if [ "$tod" -lt $((2 * 60000)) ] || [ "$tod" -gt $((DAY - 2 * 60000)) ]; then
    harness "(5) the UTC day is within 2 minutes of its turn; run again"
fi
cat >"$WORK/two.sql" <<'SQL'
delete from spawn_events
 where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines';
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
         jsonb_build_object('type', 'host.core_lines', 'values', jsonb_build_object('storage.free_bytes', v.free)))),
       (select coalesce(max(session_seq), 0) from spawn_events where session_id = 'session_control') + v.n,
       v.at
  from (values (1, :a_at::bigint, :a_free::numeric), (2, :b_at::bigint, :b_free::numeric)) v(n, at, free);
SQL
capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 \
    -v a_at=$((now_ms - DAY - 60000)) -v a_free=$((F + 25 * GB)) \
    -v b_at=$((now_ms - tod - 60000)) -v b_free=$((F + 10 * GB)) -f "$WORK/two.sql"
[ "$status" -eq 0 ] || harness "seeding the two samples failed: $(err)"
answer "two samples, no fit"
set -- $(fall_said)
within_gb "$1" $((25 * GB)) ||
    check_failed "(5) disk_fall with no fit and F + 25 GB a day and a minute ago: expected within 2 GB of $((25 * GB)), got [$1]"
expect_eq "$2" day "(5) disk_fall's basis with no fit"
note "(5) checked: the day's fall is said when the fit has none"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "222: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "222: storage.disk_fall says the worse of the week's fit and the last day, with its basis"
exit 0
