#!/bin/sh
# Black box: Home counts a task's conflict send-backs across its objective's
# earlier attempts, not from zero at each new attempt.
#
# The problem, on the live fleet (read-only /api/missions, 2026-11-12): when
# a task is cancelled and its objective is worked again, the new task starts
# from nothing. Objective 19 was sent back as a conflict twice under T88 and
# twice more under T127; objective 23 three times under T113 and once under
# T167; objective 28 once each under T175, T202 and T203, so no single
# attempt was ever sent back twice. Objective 069 says on Home how many
# times a task in flight was sent back as a conflict, counting that task's
# own verdicts only: a third conflict round on one objective reads as the
# first.
#
# Required (read through `GET /api/missions` and web/src/home.ts's
# missionsToday under node over it):
#   (1) every in-flight row carries `earlierConflicts`: the `conflict` gate
#       verdicts of the mission's other tasks for the same objective that
#       were assigned before it (0 for a task with no objective; another
#       mission's objective of the same number is not the same objective).
#       `conflictText` counts them with the task's own `conflicts`: from two
#       in all, "sent back <n> times as a conflict", followed by ", <e> on
#       earlier attempts" when <e> is above zero; null under two.
#
# Records are written as the integrator writes them (control records,
# folded by spawn_control_refold()); `spawn serve` answers. Missions 1 and 2
# take their work from a backlog. Mission 1, hours before now:
#   objective 1: T1 one conflict, cancelled; T2 one conflict, cancelled;
#     T3 one conflict, now sent back.
#   objective 2: T4 two conflicts, cancelled; T5 in work, none.
#   objective 3: T6 two conflicts, now sent back (no earlier attempt).
#   objective 4: T7 rejected once, cancelled; T8 one conflict, sent back.
#   no objective: T10 two conflicts, cancelled; T11 in work, none (no
#     objective is no chain).
# Mission 2: objective 3, T9 two conflicts, cancelled (not mission 1's).
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
    echo "073: harness: $*" >&2
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
SPAWN_DB_NAME="bb_073_chain_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# A scratch HOME and Claude Code projects dir: nothing here reads or writes
# the real ones.
real_home=$HOME
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"

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
        echo "073: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "073: harness: unexpected exit $rc" >&2
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


note() { echo "073: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}



# --- the history -------------------------------------------------------------------------

ctl=$WORK/ctl
for m in 1 2; do
    mkdir -p "$ctl/m$m/backlog" "$ctl/m$m/scenarios"
    for n in 1 2 3 4; do
        printf '{"id": %d, "title": "Objective %d of 073", "prompt": "Make objective %d pass."}\n' "$n" "$n" "$n" \
            >"$ctl/m$m/backlog/00$n.json"
    done
done
B=$(printf 'bb-073-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-073-candidate' | sha1sum | cut -c1-40)
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
create function pg_temp.mission(m int) returns void language sql as \$\$
    select pg_temp.rec(-40, jsonb_build_object('type', 'mission.added', 'id', m, 'text', '073 mission ' || m,
        'origin', 'human', 'sources', '[]'::jsonb));
    select pg_temp.rec(-40, jsonb_build_object('type', 'mission.configured', 'id', m, 'accept', 'gate',
        'gate', '$ctl/m' || m || '/scenarios',
        'objectives', jsonb_build_object('backlog', '$ctl/m' || m || '/backlog', 'resident', 'rsi', 'workers', 1))) \$\$;
create function pg_temp.assigned(h numeric, m int, task int, objective int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', m,
        'objective', objective, 'kind', 'ephemeral', 'created_by', 'spawn-integrate',
        'agent_id', 'agent_worker_objective_' || task, 'session_id', 'session_worker_objective_' || task,
        'scenario', '$ctl/m' || m || '/scenarios/objective-' || objective,
        'brief', 'Objective ' || objective || ' of 073')) \$\$;
-- A verdict on the report of the given revision, after it was reported.
create function pg_temp.judged(h numeric, task int, verdict text, revision int) returns void language sql as \$\$
    select pg_temp.rec(h - 0.2, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', '$C'));
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', verdict,
        'revision', revision, 'base', '$B', 'scenarios', 4, 'reason', verdict || ' with the base')) \$\$;
create function pg_temp.cancelled(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.cancelled', 'id', task, 'by', 'human',
        'reason', '073: worked again as a new task')) \$\$;

select pg_temp.mission(1);
select pg_temp.mission(2);
-- Mission 1, objective 1: three attempts, one conflict each.
select pg_temp.assigned(-30, 1, 1, 1);
select pg_temp.judged(-29, 1, 'conflict', 0);
select pg_temp.cancelled(-28, 1);
select pg_temp.assigned(-27, 1, 2, 1);
select pg_temp.judged(-26, 2, 'conflict', 0);
select pg_temp.cancelled(-25, 2);
select pg_temp.assigned(-24, 1, 3, 1);
select pg_temp.judged(-23, 3, 'conflict', 0);
-- Objective 2: two conflicts on the first attempt, none yet on the second.
select pg_temp.assigned(-22, 1, 4, 2);
select pg_temp.judged(-21, 4, 'conflict', 0);
select pg_temp.judged(-20, 4, 'conflict', 1);
select pg_temp.cancelled(-19, 4);
select pg_temp.assigned(-18, 1, 5, 2);
-- Objective 3: one attempt, two conflicts.
select pg_temp.assigned(-17, 1, 6, 3);
select pg_temp.judged(-16, 6, 'conflict', 0);
select pg_temp.judged(-15, 6, 'conflict', 1);
-- Objective 4: a rejection on the first attempt, a conflict on the second.
select pg_temp.assigned(-14, 1, 7, 4);
select pg_temp.judged(-13, 7, 'rejected', 0);
select pg_temp.cancelled(-12, 7);
select pg_temp.assigned(-11, 1, 8, 4);
select pg_temp.judged(-10, 8, 'conflict', 0);
-- Two tasks with no objective: one cancelled after two conflicts, one in
-- work with none (no objective is no chain).
select pg_temp.rec(-9, jsonb_build_object('type', 'task.assigned', 'id', 10, 'mission_id', 1,
    'kind', 'ephemeral', 'created_by', 'human', 'agent_id', 'agent_worker_10', 'session_id', 'session_worker_10',
    'brief', 'Task 10 of 073'));
select pg_temp.judged(-8, 10, 'conflict', 0);
select pg_temp.judged(-7, 10, 'conflict', 1);
select pg_temp.cancelled(-6, 10);
select pg_temp.rec(-5, jsonb_build_object('type', 'task.assigned', 'id', 11, 'mission_id', 1,
    'kind', 'ephemeral', 'created_by', 'human', 'agent_id', 'agent_worker_11', 'session_id', 'session_worker_11',
    'brief', 'Task 11 of 073'));
-- Mission 2's objective 3: two conflicts, cancelled.
select pg_temp.assigned(-35, 2, 9, 3);
select pg_temp.judged(-34, 9, 'conflict', 0);
select pg_temp.judged(-33, 9, 'conflict', 1);
select pg_temp.cancelled(-32, 9);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || mission_id || ':' || coalesce(objective::text, '-') || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:1:1:cancelled 2:1:1:cancelled 3:1:1:revise 4:1:2:cancelled 5:1:2:assigned 6:1:3:revise 7:1:4:cancelled 8:1:4:revise 9:2:3:cancelled 10:1:-:cancelled 11:1:-:assigned" ] ||
    harness "the seed folded to tasks [$tasks]"

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
gated=$(jq -c '[.missions[] | .tasks[] | [.id, .objective, ([.stages[] | select(.stage == "gated" and .verdict == "conflict")] | length)]] | sort' "$WORK/api.json")
[ "$gated" = '[[1,1,1],[2,1,1],[3,1,1],[4,2,2],[5,2,0],[6,3,2],[7,4,0],[8,4,1],[9,3,2],[10,null,2],[11,null,0]]' ] ||
    harness "the answer's tasks are not as seeded: $gated"

# --- (1) the in-flight rows ----------------------------------------------------------------

cat >"$WORK/chain.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { missionsToday } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const today = missionsToday(answer, Date.now());

test("(1) a task's conflicts count its objective's earlier attempts", () => {
  const row = today.rows.find((entry) => entry.id === 1);
  assert.ok(row, "mission 1 has a row");
  assert.deepEqual(
    row.inFlight.map(({ task_id, conflicts, earlierConflicts, conflictText }) => ({
      task_id, conflicts, earlierConflicts, conflictText,
    })),
    [
      { task_id: 3, conflicts: 1, earlierConflicts: 2, conflictText: "sent back 3 times as a conflict, 2 on earlier attempts" },
      { task_id: 5, conflicts: 0, earlierConflicts: 2, conflictText: "sent back 2 times as a conflict, 2 on earlier attempts" },
      { task_id: 6, conflicts: 2, earlierConflicts: 0, conflictText: "sent back 2 times as a conflict" },
      { task_id: 8, conflicts: 1, earlierConflicts: 0, conflictText: null },
      { task_id: 11, conflicts: 0, earlierConflicts: 0, conflictText: null },
    ],
  );
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/chain.test.mjs" >"$WORK/chain.out" 2>&1; then
    check_failed "(1) web/src/home.ts missionsToday over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error|conflict' "$WORK/chain.out" | head -n 20 | tr '\n' ' ' | head -c 1400)"
fi
note "(1) checked: the in-flight rows"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "073: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "073: Home counts a task's conflict send-backs across its objective's earlier attempts"
exit 0
