#!/bin/sh
# Black box: the scorecard says how long a finished change waits before the
# owner can use it: from a landing's report to the deploy that put it live.
#
# The problem (owner, 2026-11-12 13:30; sync/roadmap-effects.md,
# "North-star and guardrail metrics", Performance): the performance line's
# north star is "time from work reported to live". No figure says it. The
# Scorecard's lead time runs from a task's assignment to its landing (1 h 24
# min a landing over 7 days), and the deploy that follows is in no figure
# (about 8 min by hand). The record has both ends: a task's `task.reported`
# and each `deploy.finished` (`ok`, `head`) that ships the then-current
# main, so every landing before it.
#
# Required (read through `GET /api/scorecard`, and web/src/scorecard.ts
# under node over it):
#   (1) every scope's `dimensions` carries `report_to_live` as the others
#       do: `value` the median (nearest rank) over the window's landings
#       of the time from the landing task's last `task.reported` before it
#       landed to the first `deploy.finished` with `ok` true at or after the
#       landing; `p90` the same at 90; `n` how many of the window's landings
#       are live; `not_live` how many have no such deploy yet (left out of
#       value and p90); better "lower". A deploy is known by when it
#       finished, so a landing made while one ran counts as shipped by it:
#       optimistic by at most one deploy's run.
#   (2) web/src/scorecardFormat.ts (node-loadable) exports
#       `reportToLiveText(dimension)`: "<value in hours, one decimal> h
#       from report to live (<n> landing[s])", the parenthesis ending
#       "; <not_live> not live yet" when not_live is above 0; null when
#       the dimension or its value is missing. The Scorecard page shows the
#       dimension as a card with that line (read off a screenshot: no check
#       here reads the page's source).
#
# The history is seeded as the store holds it (069's way: control records
# folded by spawn_control_refold()), hours before now; `spawn serve`
# answers. One mission, five tasks:
#   T1 reported -10, accepted -9.6, landed -9.5; deploy ok -9 (1.0 h).
#   T2 reported -8.5, rejected -8.2, reported -8, accepted -7.5, landed
#     -7.4; a deploy that failed (ok false) at -7; deploy ok -6 (2.0 h, from
#     its last report).
#   T3 reported -5, landed -4.5; T4 reported -4.8, landed -4.2; one deploy
#     ok at -4 ships both (1.0 h and 0.8 h).
#   T5 reported -2, landed -1.5; no deploy since (not live).
# So report_to_live: value 1.0 h, p90 2.0 h, n 4, not_live 1.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "081: harness: $*" >&2
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
SPAWN_DB_NAME="bb_081_conflicts_$run"
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
        echo "081: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "081: harness: unexpected exit $rc" >&2
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


note() { echo "081: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

B=$(printf 'bb-081-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-081-candidate' | sha1sum | cut -c1-40)
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
        'brief', 'Task ' || task || ' of 081')) \$\$;
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

-- A deploy that failed 8 days ago: the live record began before this
-- window, so every landing in it counts under objective 095's rule as
-- under 081's (a failed deploy ships nothing).
select pg_temp.deployed(-192, false);
select pg_temp.rec(-12, '{"type":"mission.added","id":1,"text":"081 mission","origin":"human","sources":[]}');
select pg_temp.rec(-12, '{"type":"mission.configured","id":1,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
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
[ "$tasks" = "1:landed 2:landed 3:landed 4:landed 5:landed" ] || harness "the seed folded to tasks [$tasks]"

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
[ "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.lead_time.n]' "$WORK/api.json")" = '[5]' ] ||
    harness "the scorecard does not count the five seeded landings: $(jq -c '[.scopes[] | {scope, mission, lead: .dimensions.lead_time}]' "$WORK/api.json" | head -c 500)"

# --- (1) the answer ----------------------------------------------------------------------

H=3600000
expect_eq "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.report_to_live | {value, p90, n, not_live, better, enough}]' "$WORK/api.json")" \
    "[{\"value\":$H,\"p90\":$((2 * H)),\"n\":4,\"not_live\":1,\"better\":\"lower\",\"enough\":true}]" \
    "(1) the fleet's report_to_live: median 1.0 h and p90 2.0 h over the four live landings (T2 from its last report, past the failed deploy), T5 not live"
expect_eq "$(jq -c '[.scopes[] | select(.scope == "mission" and .mission == 1) | .dimensions.report_to_live | {value, n, not_live}]' "$WORK/api.json")" \
    "[{\"value\":$H,\"n\":4,\"not_live\":1}]" \
    "(1) mission 1's report_to_live, the same"
note "(1) checked: the answer"

# --- (2) the page's model ------------------------------------------------------------------

cat >"$WORK/live.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const format = await import(pathToFileURL(`${env.BB_TREE}/web/src/scorecardFormat.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const fleet = answer.scopes.find((scope) => scope.scope === "fleet").dimensions.report_to_live;

test("(2) the line the card says", () => {
  assert.equal(typeof format.reportToLiveText, "function", "web/src/scorecardFormat.ts exports reportToLiveText");
  assert.equal(format.reportToLiveText(fleet), "1.0 h from report to live (4 landings; 1 not live yet)");
  assert.equal(format.reportToLiveText({ ...fleet, not_live: 0, n: 1, value: 5_400_000 }), "1.5 h from report to live (1 landing)");
  assert.equal(format.reportToLiveText({ ...fleet, value: null }), null);
  assert.equal(format.reportToLiveText(undefined), null);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/live.test.mjs" >"$WORK/live.out" 2>&1; then
    check_failed "(2) web/src/scorecardFormat.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/live.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(2) checked: the page's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "081: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "081: the scorecard says how long a landing waits from its report to the deploy that puts it live"
exit 0
