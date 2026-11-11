#!/bin/sh
# Black box: the page says a task handed to its worker is in work, not
# "delivered".
#
# The problem, as the owner reads Home after ed67a656 (lead-mac, sync/
# queue.md 00:30; sync/page-home-2026-11-12-0012.png): the Missions card
# says "T268 ... delivered 5m", "T269 ... delivered 5m", "T270 ...
# delivered 5m" while "Working now" says the same tasks are "working ·
# 5m", and the sidebar says "RSI mission · delivered" (sync/page-mission4-
# rsi-2026-11-12-0012.png: the RSI page's current task pill too). To an
# owner "delivered" reads as finished; it means the task was handed to its
# worker, which is working on it. The page shows the store's state word
# as it is: web/src/ordering.ts `taskStateLabel` passes `delivered` and
# `assigned` through, and the sidebar badge (web/src/components/
# Sidebar.tsx) and the RSI page's PhaseBadge (web/src/components/rsi/
# Phase.tsx) print the raw state.
#
# Required (read through `GET /api/missions` and web/src/ordering.ts under
# node over it, which loads it alone):
#   (1) ordering.ts exports `stateWords(state)`: a task state in the
#       owner's words: `assigned` "waiting for its worker", `delivered` "in
#       work", `revise` "sent back"; every other state as it is.
#   (2) `taskStateLabel(task, mission)` (Home's Missions card) says the
#       same: "waiting for its worker" for an assigned task, "in work" for
#       a delivered one; "gating", "in review" and "sent back" as today.
#   The API keeps the store's states.
#
# The history is seeded as the store holds it and folded by
# spawn_control_refold(), as 052 does; `spawn serve` answers. Gate mission
# 1's task 1 is assigned, 2 delivered, 3 reported (gating), 4 sent back by
# its gate; review mission 2's task 5 is reported (in review). Each check is
# reported; the run fails if any failed.
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
    echo "066: harness: $*" >&2
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_066_words_$run"
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
        echo "066: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "066: harness: unexpected exit $rc" >&2
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


note() { echo "066: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the history -----------------------------------------------------------------------

B=$(printf 'bb-066-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-066-candidate' | sha1sum | cut -c1-40)
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
create function pg_temp.assigned(h numeric, task int, mission int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', mission,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_task_' || task, 'session_id', 'session_worker_task_' || task,
        'brief', 'Task ' || task || ' of 066')) \$\$;

select pg_temp.rec(-5, '{"type":"mission.added","id":1,"text":"066 gated","origin":"human","sources":[]}');
select pg_temp.rec(-5, '{"type":"mission.configured","id":1,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
select pg_temp.rec(-5, '{"type":"mission.added","id":2,"text":"066 reviewed","origin":"human","sources":[]}');
select pg_temp.rec(-5, '{"type":"mission.configured","id":2,"accept":"review"}');
select pg_temp.assigned(-4, 1, 1);
select pg_temp.assigned(-4, 2, 1);
select pg_temp.rec(-3.9, '{"type":"task.delivered","id":2,"command_id":2}');
select pg_temp.assigned(-4, 3, 1);
select pg_temp.rec(-3.9, '{"type":"task.delivered","id":3,"command_id":3}');
select pg_temp.rec(-3, '{"type":"task.reported","id":3,"commit":"$C","report":"Task 3 is done."}');
select pg_temp.assigned(-4, 4, 1);
select pg_temp.rec(-3.9, '{"type":"task.delivered","id":4,"command_id":4}');
select pg_temp.rec(-3, '{"type":"task.reported","id":4,"commit":"$C","report":"Task 4 is done."}');
select pg_temp.rec(-2, '{"type":"task.gated","id":4,"verdict":"rejected","revision":0,"base":"$B","scenarios":4,"reason":"the scenario fails on the candidate"}');
select pg_temp.assigned(-4, 5, 2);
select pg_temp.rec(-3.9, '{"type":"task.delivered","id":5,"command_id":5}');
select pg_temp.rec(-3, '{"type":"task.reported","id":5,"commit":"$C","report":"Task 5 is done."}');
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:assigned 2:delivered 3:reported 4:revise 5:reported" ] || harness "the seed folded to tasks [$tasks]"

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/missions" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/missions answered $code"
# The API keeps the store's states; the words are the page's.
[ "$(jq -c '[.missions[] | .tasks[] | [.id, .state]] | sort' "$WORK/api.json")" = \
    '[[1,"assigned"],[2,"delivered"],[3,"reported"],[4,"revise"],[5,"reported"]]' ] ||
    harness "the answer's tasks are not as seeded: $(jq -c '[.missions[] | .tasks[] | [.id, .state]]' "$WORK/api.json")"

# --- (1), (2) the page's words ---------------------------------------------------------------

cat >"$WORK/words.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const ordering = await import(pathToFileURL(`${env.BB_TREE}/web/src/ordering.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const label = (id) => {
  for (const mission of answer.missions) {
    const task = mission.tasks.find((entry) => entry.id === id);
    if (task) return ordering.taskStateLabel(task, mission);
  }
  return undefined;
};

test("(1) a task state in the owner's words", () => {
  assert.equal(typeof ordering.stateWords, "function", "ordering.ts exports stateWords");
  assert.deepEqual(
    ["assigned", "delivered", "revise", "landing", "landed", "cancelled"].map(ordering.stateWords),
    ["waiting for its worker", "in work", "sent back", "landing", "landed", "cancelled"],
  );
});

test("(2) Home's Missions card says the same", () => {
  assert.deepEqual(
    [1, 2, 3, 4, 5].map(label),
    ["waiting for its worker", "in work", "gating", "sent back", "in review"],
  );
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/words.test.mjs" >"$WORK/words.out" 2>&1; then
    check_failed "(1)/(2) web/src/ordering.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error|exports' "$WORK/words.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(1), (2) checked: the page's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "066: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "066: the page says a task handed to its worker is in work, and one not yet received waits for its worker"
exit 0
