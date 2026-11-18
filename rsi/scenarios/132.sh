#!/bin/sh
# Black box: the performance north star, "time from report to live", has a
# value as soon as the live record gives one, over what the record covers,
# and says so beside its name.
#
# The problem (lead-mac's review §1, 2026-11-13; the lead's scope 20:05):
# since objective 095 the Scorecard counts only landings at or after the
# live record's start (the first deploy.finished), so its report to live is
# the wait, never the record's start. But the Core lines card still held the
# north star back until the record covered all 7 days ("measuring since
# 11-12 03:10" until 11-19), though the fleet had 76 live landings, median
# 44.6 min.
#
# "Report": a task's last task.reported record before it landed. "Live":
# the first deploy.finished record with ok true at or after the landing.
# Report to live is the median over the window's landings at or after the
# record's start, with at least 3 live (MIN_N).
#
# Required (through `GET /api/core-lines`, `GET /api/scorecard` and
# web/src/coreLines.ts under node):
#   (1) while the record does not cover the 7 days, the north star is the
#       Scorecard's fleet report_to_live value, state no_range, with
#       measuring_since_ms (the record's start) and window "since the
#       deploy record began";
#   (2) the card reads its name "Time from report to live (since the deploy
#       record began)" and its value in words ("1 h");
#   (3) once the record covers the 7 days: the Scorecard's value, window
#       "7d" ("(last 7 days)" on the card), no measuring_since_ms.
#
# The history: 095's (the first deploy record at -9 h; three live landings
# after it, median 1.0 h). Then a deploy record 8 days back covers the 7
# days.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "132: harness: $*" >&2
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
SPAWN_DB_NAME="bb_132_report_live_$run"
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
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
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
        echo "132: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "132: harness: unexpected exit $rc" >&2
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL; do
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


note() { echo "132: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

B=$(printf 'bb-132-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-132-candidate' | sha1sum | cut -c1-40)
# One "now" for the whole seed, so the intervals are exact to the millisecond.
seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select $seed_now::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;
create function pg_temp.assigned(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', 1,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'brief', 'Task ' || task || ' of 132')) \$\$;
create function pg_temp.reported(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', '$C')) \$\$;
create function pg_temp.gated(h numeric, task int, verdict text, revision int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', verdict,
        'revision', revision, 'base', '$B', 'scenarios', 4, 'candidate', '$C', 'reason', verdict)) \$\$;
create function pg_temp.landed(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.landed', 'id', task, 'from', '$B', 'to', '$C',
        'missions', '[]'::jsonb)) \$\$;
create function pg_temp.deployed(h numeric, ok boolean) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'deploy.finished', 'by', 'landing', 'head', '$C',
        'ok', ok, 'reason', case when ok then '' else 'the build failed' end)) \$\$;

select pg_temp.rec(-48, '{"type":"mission.added","id":1,"text":"132 mission","origin":"human","sources":[]}');
select pg_temp.rec(-48, '{"type":"mission.configured","id":1,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
-- Task 6 lands before the live record begins.
select pg_temp.assigned(-40, 6);
select pg_temp.reported(-31, 6);
select pg_temp.gated(-30.5, 6, 'accepted', 0);
select pg_temp.landed(-30, 6);
select pg_temp.assigned(-11, k) from generate_series(1, 5) k;
select pg_temp.reported(-10, 1);
select pg_temp.gated(-9.6, 1, 'accepted', 0);
select pg_temp.landed(-9.5, 1);
select pg_temp.deployed(-9, true);
select pg_temp.reported(-8.5, 2);
select pg_temp.gated(-8.2, 2, 'rejected', 0);
select pg_temp.reported(-8, 2);
select pg_temp.gated(-7.5, 2, 'accepted', 1);
select pg_temp.landed(-7.4, 2);
select pg_temp.deployed(-7, false);
select pg_temp.deployed(-6, true);
select pg_temp.reported(-5, 3);
select pg_temp.reported(-4.8, 4);
select pg_temp.gated(-4.6, 3, 'accepted', 0);
select pg_temp.landed(-4.5, 3);
select pg_temp.gated(-4.3, 4, 'accepted', 0);
select pg_temp.landed(-4.2, 4);
select pg_temp.deployed(-4, true);
select pg_temp.reported(-2, 5);
select pg_temp.gated(-1.6, 5, 'accepted', 0);
select pg_temp.landed(-1.5, 5);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:landed 2:landed 3:landed 4:landed 5:landed 6:landed" ] || harness "the seed folded to tasks [$tasks]"

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/scorecard" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/scorecard answered $code"
[ "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.lead_time.n]' "$WORK/api.json")" = '[6]' ] ||
    harness "the scorecard does not count the six seeded landings: $(jq -c '[.scopes[] | {scope, mission, lead: .dimensions.lead_time}]' "$WORK/api.json" | head -c 500)"

H=3600000
record_from=$((seed_now - 9 * H))
fleet_rtl() { jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.report_to_live.value][0]' "$1"; }
star_of() { jq -c '.lines[] | select(.line == "performance") | .north_star | {value, state, measuring_since_ms, window}' "$1"; }
read_api() { # <path> <file>
    code=$(curl -s --noproxy '*' -o "$2" -w '%{http_code}' "http://127.0.0.1:$port$1" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET $1 answered $code: $(head -c 300 "$2")"
}
start_serve() {
    # Emptied here, in the scenario's shell: a redirect in the background
    # serve truncates only in the child process, so the port read below
    # could still see the last serve's port (the 155.sh pattern).
    : >"$WORK/serve.log" || harness "cannot empty serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >>"$WORK/serve.log" 2>&1 &
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
stop_serve() {
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}

# --- (1) not covered: the value over what the record covers ----------------------------

expect_eq "$(fleet_rtl "$WORK/api.json")" "$H" "the seed: the Scorecard's fleet report to live, 1.0 h over three live landings (095's (1))"
read_api /api/core-lines "$WORK/lines.json"
expect_eq "$(star_of "$WORK/lines.json")" \
    "{\"value\":$H,\"state\":\"no_range\",\"measuring_since_ms\":$record_from,\"window\":\"since the deploy record began\"}" \
    "(1) the north star while the record does not cover the 7 days: the Scorecard's value, since the record's start"
note "(1) checked: the value over what the record covers"

# --- (3) covered --------------------------------------------------------------------------

stop_serve
cat >"$WORK/seed2.sql" <<SQL
\set ON_ERROR_STOP 1
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
values ('agent_control', 'session_control', null, 'control', null,
        jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
            jsonb_build_object('type', 'deploy.finished', 'by', 'landing', 'head', '$C', 'ok', true, 'reason', ''))),
        900000, $((seed_now - 192 * H)));
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed2.sql"
[ "$status" -eq 0 ] || harness "seeding the 8-day-old deploy record failed: $(err)"
start_serve
read_api /api/scorecard "$WORK/api2.json"
read_api /api/core-lines "$WORK/lines2.json"
live2=$(fleet_rtl "$WORK/api2.json")
[ "$live2" != null ] || check_failed "(3) the seed: the Scorecard has a value once the record covers the 7 days"
expect_eq "$(star_of "$WORK/lines2.json")" \
    "{\"value\":$live2,\"state\":\"no_range\",\"measuring_since_ms\":null,\"window\":\"7d\"}" \
    "(3) covered: the Scorecard's value over the 7 days, window 7d, no measuring since"
note "(3) checked: covered"

# --- (2) the card ----------------------------------------------------------------------------

cat >"$WORK/lines.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView, unitWords } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const read = (name) => JSON.parse(readFileSync(`${env.BB_WORK}/${name}`, "utf8"));
const star = (answer) => coreLinesView(answer).rows.find((r) => r.line === "Performance").north_star;

test("(2) not covered: the value, and the window beside the name", () => {
  const shown = star(read("lines.json"));
  assert.equal(shown.name, "Time from report to live (since the deploy record began)");
  assert.equal(shown.value, "1 h");
});

test("(3) covered: the last 7 days", () => {
  const answer = read("lines2.json");
  const value = answer.lines.find((l) => l.line === "performance").north_star.value;
  const shown = star(answer);
  assert.equal(shown.name, "Time from report to live (last 7 days)");
  assert.equal(shown.value, unitWords("ms", value));
});
TEST
if ! TZ=Asia/Shanghai BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/lines.test.mjs" >"$WORK/lines.out" 2>&1; then
    check_failed "(2) web/src/coreLines.ts over the answers: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/lines.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(2) checked: the card's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "132: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "132: report to live has a value as soon as the live record gives one, and says over what"
exit 0
