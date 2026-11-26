#!/bin/sh
# Black box: the RSI page's Landings card shows a mission's progress when its
# objectives carry no metric: objectives settled over time.
#
# The problem, as the owner reads the RSI page after 73e3596d (lead-mac,
# sync/page-rsi-2026-11-11-2336.png): the Landings card says "No landed
# measurements yet" and "The first landing records the starting baseline"
# for mission 4, which has 49 of 50 objectives settled and 48 landings. The
# card charts one metric objective's ratchet (web/src/rsi.ts
# chartObjective, metricPoints; web/src/components/rsi/MetricChart.tsx),
# and no objective of mission 4 or 6 (50 and 108 objectives, 48 and 108
# landings, live /api/rsi) names a metric, so the card is empty on both, for
# good, and the page's largest card says the mission has done nothing.
#
# Required (read through `GET /api/rsi` and a new module under node,
# web/src/rsiProgress.ts, which loads with node alone):
#   (1) an objective's `solved_on_base` carries `at_ms`, the time of the
#       integrator's `objective.solved` record.
#   (2) settledProgress(mission) gives {total, settled, points}: `total`
#       the mission's objectives, `settled` those solved or met, and one
#       point per settled objective, {objective, task, at_ms, settled,
#       on_base}, at its first landing (or, solved on the base without a
#       landing, at its record, on_base true), oldest first (ties by
#       objective id), `settled` counting up from 1.
#   (3) progressTitle(progress) reads "<settled> of <total> objectives
#       settled"; with no objective settled, points are [] and settled 0.
#
# The history is seeded as the store holds it and folded by
# spawn_control_refold(), as 052 does; `spawn serve` answers. One objective
# mission whose backlog holds objectives 1 to 5, none with a metric, hours
# before now:
#   objective 4: task 4 landed -8 (settled first, though not first by id).
#   objective 1: task 1 landed -7.     objective 2: task 2 landed -5.
#   objective 3: task 3, objective.solved on the base -4.5 (no landing).
#   objective 5: task 5 in flight.
# Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no tmux, no
# network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "063: harness: $*" >&2
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
# A scratch HOME in WORK: nothing here reads or writes the real home, nor
# is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_063_progress_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

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
        echo "063: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "063: harness: unexpected exit $rc" >&2
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


note() { echo "063: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the backlog and the history -------------------------------------------------------

ctl=$WORK/ctl
mkdir -p "$ctl/rsi/backlog" "$ctl/rsi/scenarios"
for n in 1 2 3 4 5; do
    printf '{"id": %d, "title": "Objective %d of 063", "prompt": "Make objective %d pass."}\n' "$n" "$n" "$n" \
        >"$ctl/rsi/backlog/00$n.json"
done
B=$(printf 'bb-063-base' | sha1sum | cut -c1-40)
C1=$(printf 'bb-063-candidate-1' | sha1sum | cut -c1-40)
C2=$(printf 'bb-063-candidate-2' | sha1sum | cut -c1-40)
C4=$(printf 'bb-063-candidate-4' | sha1sum | cut -c1-40)
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;
create function pg_temp.assigned(h numeric, task int, objective int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', 1,
        'objective', objective, 'kind', 'ephemeral', 'created_by', 'spawn-integrate',
        'agent_id', 'agent_worker_objective_' || task, 'session_id', 'session_worker_objective_' || task,
        'scenario', '$ctl/rsi/scenarios/objective-' || objective,
        'brief', 'Objective ' || objective || ' of 063')) \$\$;
create function pg_temp.lands(h numeric, task int, objective int, c text) returns void language sql as \$\$
    select pg_temp.assigned(h - 1, task, objective);
    select pg_temp.rec(h - 0.6, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', c));
    select pg_temp.rec(h - 0.3, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', 'accepted',
        'revision', 0, 'base', '$B', 'scenarios', 4, 'candidate', c));
    select pg_temp.rec(h, jsonb_build_object('type', 'task.landed', 'id', task, 'from', '$B', 'to', c,
        'missions', '[]'::jsonb)) \$\$;

select pg_temp.rec(-10, '{"type":"mission.added","id":1,"text":"063 objectives","origin":"human","sources":[]}');
select pg_temp.rec(-10, jsonb_build_object('type', 'mission.configured', 'id', 1, 'accept', 'gate',
    'gate', '$ctl/rsi/scenarios', 'budget', jsonb_build_object('minutes', 240, 'tool_calls', 600),
    'objectives', jsonb_build_object('backlog', '$ctl/rsi/backlog', 'resident', 'rsi', 'workers', 2)));
select pg_temp.lands(-8, 4, 4, '$C4');
select pg_temp.lands(-7, 1, 1, '$C1');
select pg_temp.lands(-5, 2, 2, '$C2');
select pg_temp.assigned(-4.8, 3, 3);
select pg_temp.rec(-4.6, '{"type":"task.reported","id":3,"commit":"$C2"}');
select pg_temp.rec(-4.5, '{"type":"objective.solved","id":1,"objective":3,"task":3,"scenario":"$ctl/rsi/scenarios/objective-3","base":"$C2","log":"| the scenario passes on the base"}');
select pg_temp.rec(-4.5, jsonb_build_object('type', 'task.cancelled', 'id', 3, 'by', 'spawn-integrate',
    'solved', (select max(id) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'objective.solved'),
    'reason', 'objective 3 is solved on the base, so task #3 ends without a rejection'));
select pg_temp.assigned(-1, 5, 5);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state || ':' || coalesce(objective::text, '-'), ' ' order by id) from mission_tasks")
[ "$tasks" = "1:landed:1 2:landed:2 3:cancelled:3 4:landed:4 5:assigned:5" ] || harness "the seed folded to tasks [$tasks]"
solved_ms=$(scalar "select recorded_at_ms from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'objective.solved'")
[ -n "$solved_ms" ] || harness "no objective.solved record was seeded"
# Each task's landing time, from its task.landed record: {"<task>": ms}.
landed_ms=$(scalar "select json_object_agg(kind -> 'detail' -> 'record' ->> 'id', recorded_at_ms) from spawn_events
    where kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'")
[ "$(printf '%s' "$landed_ms" | jq -c 'keys')" = '["1","2","4"]' ] || harness "the seed's landings are not tasks 1, 2 and 4: $landed_ms"

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/rsi" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/rsi answered $code"
row() { jq -c --argjson id "$1" "[.missions[] | select(.id == 1) | .objectives[] | select(.id == \$id)][0] | $2" "$WORK/api.json"; }
[ "$(jq -c '[.missions[] | select(.id == 1) | .objectives[] | [.id, .status, .metric, (.landings | length)]]' "$WORK/api.json")" = \
    '[[1,"solved",null,1],[2,"solved",null,1],[3,"solved",null,0],[4,"solved",null,1],[5,"open",null,0]]' ] ||
    harness "the backlog did not read as seeded: $(jq -c '[.missions[] | .objectives[]? | [.id, .status, .metric, (.landings | length)]]' "$WORK/api.json")"

# --- (1) the solved-on-base record's time ------------------------------------------------

expect_eq "$(row 3 '.solved_on_base.at_ms')" "$solved_ms" \
    "(1) objective 3's solved_on_base.at_ms, the time of its objective.solved record"
note "(1) checked: the record's time"

# --- (2), (3) the card's model -----------------------------------------------------------

cat >"$WORK/progress.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { settledProgress, progressTitle } = await import(
  pathToFileURL(`${env.BB_TREE}/web/src/rsiProgress.ts`).href
);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const mission = answer.missions.find((entry) => entry.id === 1);
const objective = (id) => mission.objectives.find((entry) => entry.id === id);
const landed = (task) => JSON.parse(env.BB_LANDED_MS)[String(task)];

test("(2) one point per settled objective, oldest first (not by id), counting up", () => {
  const progress = settledProgress(mission);
  assert.equal(progress.total, 5, "total");
  assert.equal(progress.settled, 4, "settled");
  assert.deepEqual(
    progress.points.map(({ objective, task, at_ms, settled, on_base }) => ({ objective, task, at_ms, settled, on_base })),
    [
      { objective: 4, task: 4, at_ms: landed(4), settled: 1, on_base: false },
      { objective: 1, task: 1, at_ms: landed(1), settled: 2, on_base: false },
      { objective: 2, task: 2, at_ms: landed(2), settled: 3, on_base: false },
      { objective: 3, task: 3, at_ms: Number(env.BB_SOLVED_MS), settled: 4, on_base: true },
    ],
  );
});

test("(3) the title, and a mission with nothing settled", () => {
  assert.equal(progressTitle(settledProgress(mission)), "4 of 5 objectives settled");
  const fresh = {
    ...mission,
    objectives: mission.objectives.map((entry) => ({
      ...entry,
      status: "open",
      landings: [],
      solved_on_base: null,
    })),
  };
  const none = settledProgress(fresh);
  assert.deepEqual([none.total, none.settled, none.points], [5, 0, []]);
  assert.equal(progressTitle(none), "0 of 5 objectives settled");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_SOLVED_MS=$solved_ms BB_LANDED_MS=$landed_ms node --test "$WORK/progress.test.mjs" >"$WORK/progress.out" 2>&1; then
    check_failed "(2)/(3) web/src/rsiProgress.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/progress.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(2), (3) checked: the card's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "063: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "063: the Landings card's model counts a metric-less mission's objectives settled over time, a solved-on-base one at its record"
exit 0
