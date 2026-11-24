#!/bin/sh
# Objective 225 [observability]: the Fleet page's Core lines diagnostics say
# which fall "Free disk lost a day" is: the week's fit or the last day's fall
# (storage.disk_fall's basis, objective 222), after its name.
#
# The problem (rev-e's n1 on 222): since 222 the answer of GET
# /api/core-lines says storage.disk_fall's basis ("fit" or "day"), but the
# page (web/src/coreLines.ts, the diagnostics the Fleet page's health strip
# shows) drops it: the owner reads "Free disk lost a day 77.1 GB" without
# knowing it is one day's fall and not the week's trend.
#
# Required (a real `spawn serve` on its own database, hourly host.core_lines
# samples planted as a loop records them against the free disk F the serve
# reads, as 222.sh plants them; each answer of GET /api/core-lines read
# through web/src/coreLines.ts's coreLinesView under node, as the page does):
#   (1) a week rising 10 GB a day up to F + 80 GB a day ago, then falling
#       through the last day to F (the answer's basis "day"): the diagnostic
#       is named "Free disk lost a day (the last day's fall)";
#   (2) a week falling 10 GB a day, then a day gaining 10 GB (basis "fit"):
#       "Free disk lost a day (the week's fit)";
#   (3) no sample (no value, no basis): "Free disk lost a day", as before;
#   (4) in each, no other figure on the card (rows, readings, diagnostics)
#       says either basis's words, and no figure's value or range says
#       them (the basis is beside the name, never in the value).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on live main (c99302e1): the name has no basis in (1) and (2).

set -eu

asserted=""
harness() {
    echo "225: harness: $*" >&2
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
SPAWN_DB_NAME="bb_225_basis_$run"
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
        echo "225: the last answer's disk figures:" >&2
        jq -c '[.diagnostics[]? | select(.key == "storage.disk_fall" or .key == "storage.free_bytes")]' "$WORK/api.json" >&2 2>/dev/null || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "225: harness: unexpected exit $rc" >&2
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
note() { echo "225: $*" >&2; }
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

tree=$PWD
[ -f "$tree/web/src/coreLines.ts" ] || harness "no web/src/coreLines.ts under $tree (run from the tree under test)"
command -v node >/dev/null 2>&1 || harness "node is not installed"
# basis_of: the answer's storage.disk_fall basis, "none" without one
basis_of() {
    jq -r '[.diagnostics[]? | select(.key == "storage.disk_fall")] | first | .basis // "none"' "$WORK/api.json"
}

# --- (3) no sample: no value, no basis ---------------------------------------------------------
answer "no sample"
[ "$(basis_of)" = none ] || harness "(3) the answer with no sample has a basis: [$(basis_of)] (222)"
cp "$WORK/api.json" "$WORK/none.json"
F=$(free_now)
case "$F" in '' | *[!0-9]*) harness "the serve reads no free disk: [$F]" ;; esac

# --- (1) the week rose, the last day lost 80 GB: the day's ------------------------------------
samples "$F" 80 -10 "$HOUR"
answer "a week rising, a day falling 80 GB"
[ "$(basis_of)" = day ] || harness "(1) the answer's basis is not day: [$(basis_of)] (222)"
cp "$WORK/api.json" "$WORK/day.json"

# --- (2) the week fell, the last day gained 10 GB: the fit's ----------------------------------
F=$(free_now)
samples "$F" -10 10 "$HOUR"
answer "a week falling, a day gaining 10 GB"
[ "$(basis_of)" = fit ] || harness "(2) the answer's basis is not fit: [$(basis_of)] (222)"
cp "$WORK/api.json" "$WORK/fit.json"

# --- the page's words, as coreLinesView says them ------------------------------------------------
cat >"$WORK/basis.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
const { coreLinesView } = await import(`${process.env.BB_TREE}/web/src/coreLines.ts`);
const view = (name) => coreLinesView(JSON.parse(readFileSync(`${process.env.BB_WORK}/${name}.json`, "utf8")));
const fall = (v) => v.diagnostics.filter((mark) => mark.name.startsWith("Free disk lost a day"));
const marks = (v) => [...v.rows.flatMap((row) => [row.north_star, ...row.guardrails, ...(row.readings ?? [])]), ...v.diagnostics];
const all = (v) => marks(v).map((mark) => mark.name);
const words = /\(the week's fit\)|\(the last day's fall\)/;

test("(1) a day that lost 80 GB: the last day's fall", () => {
  assert.deepEqual(fall(view("day")).map((mark) => mark.name), ["Free disk lost a day (the last day's fall)"]);
});
test("(2) a week that fell, a day that gained: the week's fit", () => {
  assert.deepEqual(fall(view("fit")).map((mark) => mark.name), ["Free disk lost a day (the week's fit)"]);
});
test("(3) no sample: the name as before", () => {
  assert.deepEqual(fall(view("none")).map((mark) => mark.name), ["Free disk lost a day"]);
});
test("(4) no other figure says a basis", () => {
  for (const name of ["day", "fit", "none"]) {
    const said = all(view(name)).filter((n) => words.test(n) && !n.startsWith("Free disk lost a day"));
    assert.deepEqual(said, [], `${name}: other figures that say a basis`);
    const inValue = marks(view(name)).filter((mark) => words.test(`${mark.value} ${mark.range}`)).map((mark) => `${mark.name}: ${mark.value} ${mark.range}`);
    assert.deepEqual(inValue, [], `${name}: a value or range that says a basis`);
  }
});
TEST
if BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/basis.test.mjs" >"$WORK/basis.out" 2>&1; then
    note "(1)-(4) checked: the page's words"
else
    # node's reporter: "ℹ fail <n>" (spec) or "# fail <n>" (tap); neither means node did not run them
    grep -q '^ℹ fail [1-9]\|^# fail [1-9]' "$WORK/basis.out" || harness "node did not run the checks: $(head -c 600 "$WORK/basis.out")"
    grep -E '^not ok|^✖ \(|AssertionError|expected|actual' "$WORK/basis.out" | sort -u | head -n 16 | while IFS= read -r line; do
        check_failed "web/src/coreLines.ts: $line"
    done
    [ -s "$WORK/failures" ] || check_failed "web/src/coreLines.ts: $(tail -c 600 "$WORK/basis.out")"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "225: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "225: the Core lines diagnostics say which fall Free disk lost a day is"
exit 0
