#!/bin/sh
# Black box: the cost page shows the costliest tasks and agents, and folds
# the rest into one line, with each row's share of the window.
#
# The problem, as the owner reads the Cost page after 73e3596d (lead-mac,
# sync/queue.md 23:45; sync/page-cost-2026-11-11-2336.png): the 7-day list
# "is hundreds of flat rows with no top-N or fold". The live
# `/api/cost?window=7d` has 254 tasks and 168 agents, every one a row on the
# page (web/src/cost.ts costView, web/src/components/cost/CostView.tsx), most
# costly first and none folded; the 20 costliest tasks are 43% of the
# $1,624.65, and no row says what share of the window it is.
#
# Required (read through `GET /api/cost?window=24h` and web/src/cost.ts's
# costView under node over it):
#   (1) costView(answer).tasks holds the 10 costliest tasks, most costly
#       first, and `tasks_more` the rest: {count, usd, text}, usd their
#       dollars summed and rounded to cents, text "<n> more tasks ·
#       <dollars>" ("1 more task" for one); null with 10 or fewer.
#       `agents` and `agents_more` the same ("<n> more agents").
#   (2) every Row carries `share`, its dollars over the window's: a whole
#       percent, rounded to the nearest ("14%"), "<1%" when above zero and
#       under one percent, null when the window cost nothing priced.
#
# The history is seeded as the store holds it: tasks assigned through
# spawn_control_record() and folded, and each worker's model calls as the
# Bridge records them (`model.usage` events), priced by the loop's cost step
# (spawn_cost_record(), bb-model-x at $3 per million input tokens); `spawn
# serve` answers. Thirteen tasks of one mission, T1 to T13, each with its
# own agent, task k costing k x $0.30 ($27.30 in all): the ten costliest
# are T13 to T4 ($25.50), and T3, T2 and T1 ($1.80) fold. Each check is
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
    echo "068: harness: $*" >&2
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
SPAWN_DB_NAME="bb_068_costfold_$run"
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
        echo "068: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "068: harness: unexpected exit $rc" >&2
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


note() { echo "068: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the history ---------------------------------------------------------------------

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
-- A worker's model call as the Bridge records it: one message's usage.
create function pg_temp.usage(h numeric, task int, input bigint) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_objective_' || task, 'session_worker_objective_' || task,
            'exec_bb_068_' || task, 'bridge', null,
            jsonb_build_object('name', 'model.usage', 'detail', jsonb_build_object(
                'message_id', 'msg_bb_068_' || task, 'model', 'bb-model-x', 'at_ms', pg_temp.ms(h),
                'input_tokens', input, 'output_tokens', 0,
                'cache_write_tokens', 0, 'cache_read_tokens', 0)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;

select pg_temp.rec(-3, '{"type":"mission.added","id":1,"text":"068 mission","origin":"human","sources":[]}');
select pg_temp.rec(-3, '{"type":"mission.configured","id":1,"accept":"review"}');
select pg_temp.rec(-2, jsonb_build_object('type', 'task.assigned', 'id', k, 'mission_id', 1,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_objective_' || k, 'session_id', 'session_worker_objective_' || k,
        'brief', 'Task ' || k || ' of 068'))
  from generate_series(1, 13) k;
select pg_temp.usage(-1, k, k * 100000) from generate_series(1, 13) k;
commit;
select spawn_control_refold();
select spawn_cost_record('{"bb-model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}}');
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
placed=$(scalar "select string_agg(task_id || ':' || usd::numeric(10,2), ' ' order by task_id) from spawn_cost_messages")
[ "$placed" = "1:0.30 2:0.60 3:0.90 4:1.20 5:1.50 6:1.80 7:2.10 8:2.40 9:2.70 10:3.00 11:3.30 12:3.60 13:3.90" ] ||
    harness "the cost step did not price the seeded calls as T1-T13 at k x \$0.30: [$placed]"

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/cost?window=24h" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/cost?window=24h answered $code"
[ "$(jq -c '[.total.usd, (.tasks | length), (.agents | length)]' "$WORK/api.json")" = '[27.3,13,13]' ] ||
    harness "the answer is not \$27.30 over 13 tasks and 13 agents: $(jq -c '[.total.usd, (.tasks | length), (.agents | length)]' "$WORK/api.json")"

# --- (1), (2) the page's figures -----------------------------------------------------------

cat >"$WORK/fold.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { costView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/cost.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const view = costView(answer);
const top = [13, 12, 11, 10, 9, 8, 7, 6, 5, 4];

test("(1) the ten costliest tasks, and the rest folded into one line", () => {
  assert.deepEqual(view.tasks.map((row) => row.key), top.map((k) => `T${k}`));
  assert.deepEqual(view.tasks_more, { count: 3, usd: 1.8, text: "3 more tasks · $1.80" });
});

test("(1) the agents the same", () => {
  assert.deepEqual(view.agents.map((row) => row.key), top.map((k) => `agent_worker_objective_${k}`));
  assert.deepEqual(view.agents_more, { count: 3, usd: 1.8, text: "3 more agents · $1.80" });
});

test("(1) one left over reads 'task', none left over is null", () => {
  const twelve = costView({ ...answer, tasks: answer.tasks.filter((entry) => entry.task !== 1 && entry.task !== 2) });
  assert.deepEqual(twelve.tasks_more, { count: 1, usd: 0.9, text: "1 more task · $0.90" });
  const ten = costView({ ...answer, tasks: answer.tasks.filter((entry) => entry.task > 3) });
  assert.equal(ten.tasks.length, 10);
  assert.equal(ten.tasks_more, null);
});

test("(2) each row's share of the window", () => {
  // T13 $3.90 of $27.30 is 14.3%; T4 $1.20 is 4.4%; mission 1 is all of it.
  assert.deepEqual([view.tasks[0].share, view.tasks[9].share, view.missions[0].share], ["14%", "4%", "100%"]);
  const tiny = costView({
    ...answer,
    total: { ...answer.total, usd: 1000 },
    tasks: [{ ...answer.tasks[0], usd: 3.9 }],
  });
  assert.equal(tiny.tasks[0].share, "<1%");
  const nothing = costView({ ...answer, total: { ...answer.total, usd: 0 } });
  assert.equal(nothing.tasks[0].share, null);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/fold.test.mjs" >"$WORK/fold.out" 2>&1; then
    check_failed "(1)/(2) web/src/cost.ts costView over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/fold.out" | head -n 14 | tr '\n' ' ' | head -c 1200)"
fi
note "(1), (2) checked: the page's figures"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "068: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "068: the cost page shows the ten costliest tasks and agents with their shares, the rest folded into one line"
exit 0
