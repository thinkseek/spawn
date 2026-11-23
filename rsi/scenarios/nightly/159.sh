#!/bin/sh
# Black box (objective 159 [performance][observability]): a landing on a
# source no deploy ships is live when it lands, and the Core lines card says
# each mission's report to live beside the fleet's. A measurement fix: it
# corrects the figure, not the work.
#
# The problem (rev-g, sync/analysis-report-to-live.md, 2026-11-14): report to
# live runs from a landing's report to the first `deploy.finished` ok at or
# after it, for every mission. Only `<live>/missions/spawn.git` is deployed
# (deploy/on-landing.sh: "a landing on any other source ... deploys nothing
# here"), so a Flake Doctor landing, live on its source when it lands, was
# charged until the next, unrelated Spawn deploy: 35 of 102 landings, a
# median 25.6 min of wait that is not one. The fleet median read 33.2 min;
# with the fix about 25.2. Mission 4's own wait (44 min) was hidden in the
# fleet's figure, and stays visible on its own.
#
# Required (read through `GET /api/scorecard`, `GET /api/core-lines`, and
# web/src/coreLines.ts under node over the latter):
#   (1) a landing of a mission none of whose sources ends in
#       `/missions/spawn.git` counts in report_to_live with its wait to its
#       landing; each scope's report_to_live says `live_at_landing`, how
#       many of its live landings were; a deployed mission's landings still
#       wait for the first ok deploy at or after them.
#   (2) /api/core-lines' performance north star is the fleet's value and
#       carries `by_mission`: each mission with a live landing,
#       `{mission, value, n, live_at_landing}`, by mission.
#   (3) web/src/coreLines.ts: `coreLinesView(answer).byMission` gives each
#       mission's words: "<span> (<n> live)", and "· live when landed (no
#       deploy ships it)" when every one was.
#
# The history (hours before now), seeded as the store holds it (081's way):
#   mission 4, source <ws>/live/missions/spawn.git (deployed):
#     T1 reported -10, landed -9.5; deploy ok -9 (1.0 h).
#     T2 reported -8, landed -7.5; deploy ok -6 (2.0 h).
#     T3 reported -5, landed -4.5; deploy ok -4 (1.0 h).
#   mission 9, source <ws>/live/missions/flake-doctor.git (no deploy ships it):
#     T4 reported -9.9, landed -9.8 (6 min; on the base: to -9, 54 min).
#     T5 reported -7.2, landed -7.1 (6 min; on the base: to -6, 72 min).
#     T6 reported -3.0, landed -2.9 (6 min; on the base: not live).
#   A failed deploy 8 days before: the live record covers the window.
# So the fleet: waits 6, 6, 6, 60, 60, 120 min: median (nearest rank) 6 min,
# n 6, not_live 0, live_at_landing 3 (the base: 60 min, n 5, not_live 1).
# Mission 4: 60 min, n 3, live_at_landing 0. Mission 9: 6 min, n 3, 3.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "159: harness: $*" >&2
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
SPAWN_DB_NAME="bb_159_live_$run"
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
        echo "159: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "159: harness: unexpected exit $rc" >&2
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


note() { echo "159: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

B=$(printf 'bb-159-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-159-candidate' | sha1sum | cut -c1-40)
live=$WORK/ws/live/missions
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
create function pg_temp.assigned(h numeric, task int, mission int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', mission,
        'kind', 'ephemeral', 'created_by', 'spawn-integrate',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'brief', 'Task ' || task || ' of 159')) \$\$;
create function pg_temp.reported(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', '$C')) \$\$;
create function pg_temp.gated(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', 'accepted',
        'revision', 0, 'base', '$B', 'scenarios', 4, 'candidate', '$C', 'reason', 'accepted')) \$\$;
create function pg_temp.landed(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.landed', 'id', task, 'from', '$B', 'to', '$C',
        'missions', '[]'::jsonb)) \$\$;
create function pg_temp.deployed(h numeric, ok boolean) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'deploy.finished', 'by', 'landing', 'head', '$C',
        'ok', ok, 'reason', case when ok then '' else 'the build failed' end)) \$\$;

select pg_temp.deployed(-192, false);
select pg_temp.rec(-12, jsonb_build_object('type', 'mission.added', 'id', 4, 'text', '159 spawn',
    'origin', 'human', 'sources', jsonb_build_array('$live/spawn.git')));
select pg_temp.rec(-12, jsonb_build_object('type', 'mission.added', 'id', 9, 'text', '159 flake doctor',
    'origin', 'human', 'sources', jsonb_build_array('$live/flake-doctor.git')));
select pg_temp.rec(-12, '{"type":"mission.configured","id":4,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
select pg_temp.rec(-12, '{"type":"mission.configured","id":9,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
select pg_temp.assigned(-11, k, 4) from generate_series(1, 3) k;
select pg_temp.assigned(-11, k, 9) from generate_series(4, 6) k;
select pg_temp.reported(-10, 1);
select pg_temp.reported(-9.9, 4);
select pg_temp.gated(-9.85, 4);
select pg_temp.landed(-9.8, 4);
select pg_temp.gated(-9.6, 1);
select pg_temp.landed(-9.5, 1);
select pg_temp.deployed(-9, true);
select pg_temp.reported(-8, 2);
select pg_temp.gated(-7.6, 2);
select pg_temp.landed(-7.5, 2);
select pg_temp.reported(-7.2, 5);
select pg_temp.gated(-7.15, 5);
select pg_temp.landed(-7.1, 5);
select pg_temp.deployed(-6, true);
select pg_temp.reported(-5, 3);
select pg_temp.gated(-4.6, 3);
select pg_temp.landed(-4.5, 3);
select pg_temp.deployed(-4, true);
select pg_temp.reported(-3.0, 6);
select pg_temp.gated(-2.95, 6);
select pg_temp.landed(-2.9, 6);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:landed 2:landed 3:landed 4:landed 5:landed 6:landed" ] || harness "the seed folded to tasks [$tasks]"
sources=$(scalar "select string_agg(id || ':' || (sources ->> 0), ' ' order by id) from mission_missions")
[ "$sources" = "4:$live/spawn.git 9:$live/flake-doctor.git" ] || harness "the seed folded to sources [$sources]"

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
get() { # <path> <file>
    code=$(curl -s --noproxy '*' -o "$2" -w '%{http_code}' "http://127.0.0.1:$port$1" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET $1 answered $code"
}
get /api/scorecard "$WORK/api.json"
[ "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.lead_time.n]' "$WORK/api.json")" = '[6]' ] ||
    harness "the scorecard does not count the six seeded landings: $(jq -c '[.scopes[] | {scope, mission, lead: .dimensions.lead_time}]' "$WORK/api.json" | head -c 500)"

# --- (1) the Scorecard ---------------------------------------------------------------------

M=60000
rtl() { # <jq selector of a scope>
    jq -c "[.scopes[] | select($1) | .dimensions.report_to_live | {value, n, not_live, live_at_landing}]" "$WORK/api.json"
}
expect_eq "$(rtl '.scope == "fleet"')" \
    "[{\"value\":$((6 * M)),\"n\":6,\"not_live\":0,\"live_at_landing\":3}]" \
    "(1) the fleet's report_to_live: mission 9's three landings live when they landed (6 min each), mission 4's waiting for their deploys (60, 120, 60 min): median 6 min over six"
expect_eq "$(rtl '.scope == "mission" and .mission == 4')" \
    "[{\"value\":$((60 * M)),\"n\":3,\"not_live\":0,\"live_at_landing\":0}]" \
    "(1) mission 4 (spawn.git, deployed): each landing to the deploy that shipped it"
expect_eq "$(rtl '.scope == "mission" and .mission == 9')" \
    "[{\"value\":$((6 * M)),\"n\":3,\"not_live\":0,\"live_at_landing\":3}]" \
    "(1) mission 9 (no deploy ships it): each landing live when it landed, the one after the last deploy too"
note "(1) checked: the Scorecard"

# --- (2) the Core lines answer ---------------------------------------------------------------

get /api/core-lines "$WORK/core.json"
want=$(printf '%s' "[{\"key\":\"performance.report_to_live\",\"value\":$((6 * M)),\"by_mission\":[{\"mission\":4,\"value\":$((60 * M)),\"n\":3,\"live_at_landing\":0},{\"mission\":9,\"value\":$((6 * M)),\"n\":3,\"live_at_landing\":3}]}]" | jq -cS .)
expect_eq "$(jq -cS '[.lines[] | select(.line == "performance") | .north_star | {key, value, by_mission}]' "$WORK/core.json")" \
    "$want" \
    "(2) the performance north star: the fleet's value, and each mission's beside it"
note "(2) checked: the Core lines answer"

# --- (3) the card's model --------------------------------------------------------------------

cat >"$WORK/card.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const core = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/core.json`, "utf8"));

test("(3) the card says each mission's report to live", () => {
  const view = core.coreLinesView(answer);
  assert.deepEqual(view.byMission, [
    { name: "mission 4", text: "1 h (3 live)", tone: "plain" },
    { name: "mission 9", text: "6 min (3 live) · live when landed (no deploy ships it)", tone: "muted" },
  ]);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/card.test.mjs" >"$WORK/card.out" 2>&1; then
    check_failed "(3) web/src/coreLines.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/card.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(3) checked: the card's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "159: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "159: a landing on a source no deploy ships is live when it lands, and each mission's report to live is said beside the fleet's"
exit 0
