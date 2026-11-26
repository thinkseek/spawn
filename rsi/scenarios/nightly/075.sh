#!/bin/sh
# Black box: the Cost page names each mission, and a row's unpriced badge is
# short enough that the dollars stay on the page.
#
# The problem, as the owner reads the Cost page (lead-mac,
# sync/page-cost-2026-11-12-0158.png, after 059 went live):
#   - the Missions column says "#4", "#6", "#7", "#8" and nothing else,
#     while every other page names them ("Spawn RSI", "Supply PA", "Spawn ·
#     owner work"): `/api/cost`'s missions rows carry no name;
#   - in the 7-day Agents column at 1400 px, a row with an unpriced badge
#     ("+ 485,321,042 tokens unpriced") pushes its dollars past the right
#     edge ("$161.5", "$" cut off). The badge spells the whole count.
#
# Required (read through `GET /api/cost`, `GET /api/missions` and
# web/src/cost.ts's costView under node over the answer):
#   (1) every `missions` row of /api/cost carries `name`, the mission's
#       `mission_name` exactly as /api/missions gives it; costView's mission
#       Row carries it as `label` (the page shows "#4 Spawn RSI").
#   (2) every Row carries `unpriced_short`: "+<n> unpriced", the count
#       compact ("950", "55K", "1.1M", "17M", "485M", "1.5B": one decimal
#       under 10, whole from 10, K/M/B by thousands; rounded first, then
#       the unit picked, so 9,960 is "10K" and 999,600 "1.0M"), null with
#       none;
#       `unpriced_text` (the full count, objective 059) stays, for the
#       badge's title. The page's badge shows the short text, and the
#       dollars never shrink or wrap.
#
# The history is seeded as the store holds it: tasks assigned through
# control records and folded, and each worker's model calls as the Bridge
# records them (`model.usage`), priced by the loop's cost step
# (spawn_cost_record(): bb-model-x priced, bb-model-z not); `spawn serve`
# answers. Mission 1 ("Bb Alpha: ...", named "Bb Alpha") has task 1, which
# calls x (100,000 tokens) and z (485,321,042); mission 2 ("Owner work on
# Bb Beta: ...", named "Bb Beta · owner work") has task 2, which calls z
# (1,148,834). Each check is reported; the run
# fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "075: harness: $*" >&2
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
SPAWN_DB_NAME="bb_075_costnames_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "075: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "075: harness: unexpected exit $rc" >&2
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


note() { echo "075: $*" >&2; }
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
create function pg_temp.usage(h numeric, task int, message text, model text, input bigint) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_objective_' || task, 'session_worker_objective_' || task,
            'exec_bb_075_' || task, 'bridge', null,
            jsonb_build_object('name', 'model.usage', 'detail', jsonb_build_object(
                'message_id', 'msg_bb_075_' || message, 'model', model, 'at_ms', pg_temp.ms(h),
                'input_tokens', input, 'output_tokens', 0,
                'cache_write_tokens', 0, 'cache_read_tokens', 0)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;

select pg_temp.rec(-3, '{"type":"mission.added","id":1,"text":"Bb Alpha: the first mission of 075.","origin":"human","sources":[]}');
select pg_temp.rec(-3, '{"type":"mission.configured","id":1,"accept":"review"}');
select pg_temp.rec(-3, '{"type":"mission.added","id":2,"text":"Owner work on Bb Beta: the second mission of 075.","origin":"human","sources":[]}');
select pg_temp.rec(-3, '{"type":"mission.configured","id":2,"accept":"review"}');
select pg_temp.rec(-2, jsonb_build_object('type', 'task.assigned', 'id', k, 'mission_id', k,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_objective_' || k, 'session_id', 'session_worker_objective_' || k,
        'brief', 'Task ' || k || ' of 075'))
  from generate_series(1, 2) k;
select pg_temp.usage(-1, 1, '1x', 'bb-model-x', 100000);
select pg_temp.usage(-1, 1, '1z', 'bb-model-z', 485321042);
select pg_temp.usage(-1, 2, '2z', 'bb-model-z', 1148834);
commit;
select spawn_control_refold();
select spawn_cost_record('{"bb-model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}}');
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
placed=$(scalar "select string_agg(task_id || ':' || model || ':' || coalesce(usd::numeric(10,2)::text, '-'), ' ' order by task_id, model) from spawn_cost_messages")
[ "$placed" = "1:bb-model-x:0.30 1:bb-model-z:- 2:bb-model-z:-" ] ||
    harness "the cost step did not place the seeded calls as T1 x \$0.30, T1 z and T2 z unpriced: [$placed]"

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
for path in "cost?window=24h:api" "missions:missions"; do
    code=$(curl -s --noproxy '*' -o "$WORK/${path#*:}.json" -w '%{http_code}' "http://127.0.0.1:$port/api/${path%:*}" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/${path%:*} answered $code"
done
[ "$(jq -c '[.missions[] | [.mission, .unpriced_tokens]] | sort' "$WORK/api.json")" = '[[1,485321042],[2,1148834]]' ] ||
    harness "/api/cost's missions are not 1 and 2 with their unpriced tokens: $(jq -c '.missions' "$WORK/api.json" | head -c 400)"
names=$(jq -c '[.missions[] | select(.id == 1 or .id == 2) | {mission: .id, name: .mission_name}] | sort_by(.mission)' "$WORK/missions.json")
[ "$names" = '[{"mission":1,"name":"Bb Alpha"},{"mission":2,"name":"Bb Beta · owner work"}]' ] ||
    harness "/api/missions does not name missions 1 and 2 Bb Alpha and Bb Beta · owner work: $names"

# --- (1) the missions' names ------------------------------------------------------------------

expect_eq "$(jq -c '[.missions[] | {mission, name}] | sort_by(.mission)' "$WORK/api.json")" "$names" \
    "(1) /api/cost's missions rows carry the name /api/missions gives each mission"
note "(1) checked: the answer"

# --- (1), (2) the page's rows -------------------------------------------------------------------

cat >"$WORK/rows.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { costView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/cost.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const missions = JSON.parse(readFileSync(`${env.BB_WORK}/missions.json`, "utf8"));
const view = costView(answer);
const nameOf = (id) => missions.missions.find((mission) => mission.id === id).mission_name;

test("(1) a mission row carries the mission's name", () => {
  assert.deepEqual(
    view.missions.map((row) => ({ key: row.key, label: row.label })).sort((a, b) => a.key.localeCompare(b.key)),
    [{ key: "#1", label: nameOf(1) }, { key: "#2", label: nameOf(2) }],
  );
});

test("(2) every row's unpriced badge is short; the full count stays for its title", () => {
  const short = (rows) => Object.fromEntries(rows.map((row) => [row.key, row.unpriced_short]));
  assert.deepEqual(short(view.missions), { "#1": "+485M unpriced", "#2": "+1.1M unpriced" });
  assert.deepEqual(short(view.tasks), { T1: "+485M unpriced", T2: "+1.1M unpriced" });
  assert.deepEqual(short(view.agents), {
    agent_worker_objective_1: "+485M unpriced",
    agent_worker_objective_2: "+1.1M unpriced",
  });
  assert.equal(view.tasks.find((row) => row.key === "T1").unpriced_text, "+ 485,321,042 tokens unpriced");
});

test("(2) the compact count's steps", () => {
  const one = (tokens) =>
    costView({ ...answer, missions: [{ ...answer.missions[0], unpriced_tokens: tokens }] }).missions[0].unpriced_short;
  assert.deepEqual(
    [950, 9_960, 55_000, 999_600, 1_148_834, 17_000_456, 485_321_042, 1_463_163_060, 0].map(one),
    [
      "+950 unpriced", "+10K unpriced", "+55K unpriced", "+1.0M unpriced", "+1.1M unpriced",
      "+17M unpriced", "+485M unpriced", "+1.5B unpriced", null,
    ],
  );
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/rows.test.mjs" >"$WORK/rows.out" 2>&1; then
    check_failed "(1)/(2) web/src/cost.ts costView over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/rows.out" | head -n 14 | tr '\n' ' ' | head -c 1200)"
fi
note "(1), (2) checked: the page's rows"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "075: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "075: the cost page names each mission, and a row's unpriced badge is short enough to keep its dollars on the page"
exit 0
