#!/bin/sh
# Black box: a Cost row's amount and share stay on its first line, in its
# own column; its unpriced mark and per-landing text go on a second line.
#
# The problem, as the owner reads the live Cost page at 1400 px (lead-mac,
# sync/page-cost-2026-11-12-0826.png, 9fd9cf41; sync/draft-objective-cost-
# 7d-layout.md). The 24 h card is clean. On the 7-day card:
# - each of the four Missions rows holds its id, "+ 1,463,163,060 tokens
#   unpriced", "$10.11 per landing (56 landed)", the amount and the share
#   on one line. Mark and per-landing text are both `shrink-0`, so the row
#   is wider than its third of the card, and the amount and share ("$566.20
#   31%") are painted over the first four rows of the Tasks column;
# - Tasks rows with a mark (T121, T113) cover their own label;
# - Agents rows with a mark push the amount past the card's right edge, and
#   two amounts are not visible at all.
# The row is one flex line in web/src/components/cost/CostView.tsx (`Rows`):
# key, label, UnpricedMark, the per-landing text, the amount, the share.
# This is the card the compute track steers by (dollars per landing over 7
# days), and the mission amounts are what is lost.
#
# Required (read through `GET /api/cost?window=24h`, and web/src/cost.ts
# under node over it):
#   (1) cost.ts exports `rowLines(row)` for a costView row:
#       {first: {key, label, usd, share}, second: [...]}: `first` the row's
#       own key, label, amount and share; `second` its unpriced mark's text
#       and its per-landing text, in that order, each only when the row has
#       it ([] for a row with neither).
#   (2) what the rows say is unchanged: their order, texts, folds (068),
#       per-landing text (060) and unpriced texts (059).
# CostView.tsx renders each row from rowLines; whether it does, and whether
# nothing overlaps at 1024 px and up, is read off a screenshot (no check here
# reads the page's source).
#
# The history is seeded as the store holds it (068's way): one mission, three
# tasks, each with one model call recorded as the Bridge records it, then
# priced by spawn_cost_record (bb-model-x: $3 per million input tokens; no
# price for bb-model-free). T1: 1,000,000 x ($3.00); T2: 500,000 on
# bb-model-free (unpriced); T3: 200,000 x ($0.60). `spawn serve` answers.
# So T1 is a row with neither secondary text, T2 one with an unpriced mark,
# and mission 1 (its per-landing text and 500,000 tokens unpriced) one with
# both.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "079: harness: $*" >&2
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
SPAWN_DB_NAME="bb_079_costrows_$run"
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
        echo "079: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "079: harness: unexpected exit $rc" >&2
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


note() { echo "079: $*" >&2; }
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
create function pg_temp.usage(h numeric, task int, model text, input bigint) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_objective_' || task, 'session_worker_objective_' || task,
            'exec_bb_079_' || task, 'bridge', null,
            jsonb_build_object('name', 'model.usage', 'detail', jsonb_build_object(
                'message_id', 'msg_bb_079_' || task, 'model', model, 'at_ms', pg_temp.ms(h),
                'input_tokens', input, 'output_tokens', 0,
                'cache_write_tokens', 0, 'cache_read_tokens', 0)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;

select pg_temp.rec(-3, '{"type":"mission.added","id":1,"text":"079 mission","origin":"human","sources":[]}');
select pg_temp.rec(-3, '{"type":"mission.configured","id":1,"accept":"review"}');
select pg_temp.rec(-2, jsonb_build_object('type', 'task.assigned', 'id', k, 'mission_id', 1,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_objective_' || k, 'session_id', 'session_worker_objective_' || k,
        'brief', 'Task ' || k || ' of 079'))
  from generate_series(1, 3) k;
select pg_temp.usage(-1, 1, 'bb-model-x', 1000000);
select pg_temp.usage(-1, 2, 'bb-model-free', 500000);
select pg_temp.usage(-1, 3, 'bb-model-x', 200000);
commit;
select spawn_control_refold();
select spawn_cost_record('{"bb-model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}}');
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"

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
[ "$(jq -c '[.total.usd, ([.tasks[] | .unpriced_tokens] | sort), ([.missions[] | .unpriced_tokens])]' "$WORK/api.json")" = '[3.6,[0,0,500000],[500000]]' ] ||
    harness "the answer is not \$3.60 with T2's 500,000 tokens unpriced: $(jq -c '[.total.usd, [.tasks[] | {task, usd, unpriced_tokens}], [.missions[] | {mission, unpriced_tokens}]]' "$WORK/api.json" | head -c 600)"

# --- (1), (2) the rows' two lines ------------------------------------------------------------

cat >"$WORK/rows.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const cost = await import(pathToFileURL(`${env.BB_TREE}/web/src/cost.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const view = cost.costView(answer);
const row = (list, key) => view[list].find((entry) => entry.key === key);
const first = (r) => ({ key: r.key, label: r.label, usd: r.usd, share: r.share });

test("(2) the rows say what they said", () => {
  assert.deepEqual(view.tasks.map((r) => r.key), ["T1", "T3", "T2"], "tasks, costliest first");
  assert.deepEqual([row("tasks", "T1").usd, row("tasks", "T1").share], ["$3.00", "83%"]);
  assert.equal(row("tasks", "T2").usd, "unpriced");
  assert.equal(row("tasks", "T2").unpriced_text, "+ 500,000 tokens unpriced");
  assert.equal(view.missions[0].unpriced_text, "+ 500,000 tokens unpriced");
  assert.equal(typeof view.missions[0].per_landing, "string", "a mission row's per-landing text (060)");
  assert.equal(row("tasks", "T1").unpriced_text, null);
});

test("(1) rowLines: the amount and share on the first line, the mark and per-landing text on the second", () => {
  assert.equal(typeof cost.rowLines, "function", "web/src/cost.ts exports rowLines");
  const mission = view.missions[0];
  assert.deepEqual(cost.rowLines(mission), {
    first: first(mission),
    second: [mission.unpriced_text, mission.per_landing],
  }, "a mission row with an unpriced mark and a per-landing text");
  const t2 = row("tasks", "T2");
  assert.deepEqual(cost.rowLines(t2), { first: first(t2), second: [t2.unpriced_text] }, "T2: only its unpriced mark");
  const t1 = row("tasks", "T1");
  assert.deepEqual(cost.rowLines(t1), { first: first(t1), second: [] }, "T1: neither, one line");
  for (const agent of view.agents) {
    const want = agent.unpriced_text ? [agent.unpriced_text] : [];
    assert.deepEqual(cost.rowLines(agent), { first: first(agent), second: want }, `agent row ${agent.key}`);
  }
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/rows.test.mjs" >"$WORK/rows.out" 2>&1; then
    check_failed "(1)/(2) web/src/cost.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/rows.out" | head -n 14 | tr '\n' ' ' | head -c 1200)"
fi
note "(1), (2) checked: the rows' two lines"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "079: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "079: a Cost row keeps its amount and share on its first line; its unpriced mark and per-landing text go on a second"
exit 0
