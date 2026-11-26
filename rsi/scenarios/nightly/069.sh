#!/bin/sh
# Black box: Home says when a task in flight has been sent back as a
# conflict more than once.
#
# The problem, as the owner reads Home: the mission's row lists its tasks in
# flight with a state ("sent back", "gating") and one "last sent back" line,
# and nothing says that a task has already lost two or three rounds to
# conflicts. On the live `/api/missions` (read-only, 2026-11-12) nine
# tasks of mission 4 were sent back as a conflict two or three times, T106
# over 29.9 h and T113 over 23.6 h before it was cancelled. Three in four
# conflict send-backs come from owner pushes onto a running task's files
# (sync/roadmap-effects.md, compute item 7), and each one is a full worker
# round and a gate run spent again. The stages are in the answer
# (`stages[]`, `stage` "gated", `verdict` "conflict"); web/src/home.ts's
# missionsToday reads them only for the row's latest send-back.
#
# Required (read through `GET /api/missions` and web/src/home.ts's
# missionsToday under node over it):
#   (1) every in-flight row carries `conflicts`, the number of the task's
#       gate verdicts that were `conflict` (a `rejected` or `incompatible`
#       one is not), and `conflictText`: "sent back <n> times as a
#       conflict" when there were two or more, else null.
#
# Records are written as the integrator writes them (control records,
# folded by spawn_control_refold()); `spawn serve` answers. Gate mission 1,
# hours before now:
#   T1: three conflicts (revisions 0, 1, 2), now sent back.
#   T2: a conflict, then a rejection, now reported again (gating).
#   T3: two conflicts, now reported again (gating).
#   T4: two incompatible verdicts, now sent back.
#   T5: three conflicts, then landed (not in flight).
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
    echo "069: harness: $*" >&2
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
SPAWN_DB_NAME="bb_069_conflicts_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "069: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "069: harness: unexpected exit $rc" >&2
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


note() { echo "069: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

B=$(printf 'bb-069-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-069-candidate' | sha1sum | cut -c1-40)
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
create function pg_temp.assigned(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', 1,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'brief', 'Task ' || task || ' of 069')) \$\$;
create function pg_temp.reported(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', '$C')) \$\$;
-- A verdict on the report of the given revision, after it was reported.
create function pg_temp.judged(h numeric, task int, verdict text, revision int) returns void language sql as \$\$
    select pg_temp.reported(h - 0.2, task);
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', verdict,
        'revision', revision, 'base', '$B', 'scenarios', 4, 'reason', verdict || ' with the base')) \$\$;

select pg_temp.rec(-10, '{"type":"mission.added","id":1,"text":"069 mission","origin":"human","sources":[]}');
select pg_temp.rec(-10, '{"type":"mission.configured","id":1,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
select pg_temp.assigned(-9, k) from generate_series(1, 5) k;
select pg_temp.judged(-8, 1, 'conflict', 0);
select pg_temp.judged(-6, 1, 'conflict', 1);
select pg_temp.judged(-4, 1, 'conflict', 2);
select pg_temp.judged(-7.5, 2, 'conflict', 0);
select pg_temp.judged(-5.5, 2, 'rejected', 1);
select pg_temp.reported(-3.5, 2);
select pg_temp.judged(-7, 3, 'conflict', 0);
select pg_temp.judged(-5, 3, 'conflict', 1);
select pg_temp.reported(-3, 3);
select pg_temp.judged(-6.5, 4, 'incompatible', 0);
select pg_temp.judged(-4.5, 4, 'incompatible', 1);
select pg_temp.judged(-8.5, 5, 'conflict', 0);
select pg_temp.judged(-7.8, 5, 'conflict', 1);
select pg_temp.judged(-6.8, 5, 'conflict', 2);
select pg_temp.rec(-6.2, jsonb_build_object('type', 'task.gated', 'id', 5, 'verdict', 'accepted',
    'revision', 3, 'base', '$B', 'scenarios', 4, 'candidate', '$C'));
select pg_temp.rec(-6, jsonb_build_object('type', 'task.landed', 'id', 5, 'from', '$B', 'to', '$C',
    'missions', '[]'::jsonb));
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state || ':' || revisions, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:revise:3 2:reported:2 3:reported:2 4:revise:2 5:landed:3" ] || harness "the seed folded to tasks [$tasks]"

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
gated=$(jq -c '[.missions[] | select(.id == 1) | .tasks[] | [.id, [.stages[] | select(.stage == "gated") | .verdict]]] | sort' "$WORK/api.json")
[ "$gated" = '[[1,["conflict","conflict","conflict"]],[2,["conflict","rejected"]],[3,["conflict","conflict"]],[4,["incompatible","incompatible"]],[5,["conflict","conflict","conflict","accepted"]]]' ] ||
    harness "the answer's gate stages are not as seeded: $gated"

# --- (1) the in-flight rows ----------------------------------------------------------------

cat >"$WORK/conflicts.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { missionsToday } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const today = missionsToday(answer, Date.now());

test("(1) each task in flight says how many times it was sent back as a conflict", () => {
  const row = today.rows.find((entry) => entry.id === 1);
  assert.ok(row, "mission 1 has a row");
  assert.deepEqual(
    row.inFlight.map(({ task_id, conflicts, conflictText }) => ({ task_id, conflicts, conflictText })),
    [
      { task_id: 1, conflicts: 3, conflictText: "sent back 3 times as a conflict" },
      { task_id: 2, conflicts: 1, conflictText: null },
      { task_id: 3, conflicts: 2, conflictText: "sent back 2 times as a conflict" },
      { task_id: 4, conflicts: 0, conflictText: null },
    ],
  );
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/conflicts.test.mjs" >"$WORK/conflicts.out" 2>&1; then
    check_failed "(1) web/src/home.ts missionsToday over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error|conflict' "$WORK/conflicts.out" | head -n 16 | tr '\n' ' ' | head -c 1200)"
fi
note "(1) checked: the in-flight rows"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "069: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "069: Home says when a task in flight has been sent back as a conflict more than once, and how many times"
exit 0
