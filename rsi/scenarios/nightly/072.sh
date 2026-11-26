#!/bin/sh
# Black box: the Show page's mission footer says when the mission last
# landed, not "no landing yet" for every mission that has not landed today.
#
# The problem, as the owner reads Show (lead-mac,
# sync/page-show-2026-11-12-0012.png): the footer says "Spawn ... no
# landing yet" and "Supply PA ... no landing yet". Both missions have landed
# tasks: Supply PA last on 11-10 (T253), Spawn many times. The footer's last
# landing (web/src/components/show/ShowView.tsx lastLanding) is read only
# from the events the scene loaded, which start at local midnight. Any
# mission that has not landed since midnight reads "no landing yet". The
# `/api/rsi` answer that the footer already polls holds every landing
# (`objectives[].landings[].at_ms`).
#
# Required (read through `GET /api/rsi` and web/src/show.ts under node):
#   (1) show.ts exports `missionLastLanding(mission, seen, at)`. It returns
#       the latest landing of the mission at or before `at` (the scene's
#       time, which in replay is the replay clock). It takes the latest of
#       the mission's own landings in the answer (`objectives[].landings
#       [].at_ms`) and the scene's (`seen`, a Map of task id to time, as
#       show.ts `landings` returns it) for the mission's tasks, and null
#       when there is none. The footer's "landed ... ago" reads it.
#
# The history is seeded as the store holds it (control records folded by
# spawn_control_refold()); `spawn serve` answers. Mission 1 takes its work
# from a backlog: objective 1 landed (task 1) 30 h ago, objective 2 (task 2)
# 5 h ago, objective 3's task 3 is in flight. Each check is reported; the
# run fails if any failed.
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
    echo "072: harness: $*" >&2
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
SPAWN_DB_NAME="bb_072_lastland_$run"
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
        echo "072: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "072: harness: unexpected exit $rc" >&2
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


note() { echo "072: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the backlog and the history -------------------------------------------------------

ctl=$WORK/ctl
mkdir -p "$ctl/rsi/backlog" "$ctl/rsi/scenarios"
for n in 1 2 3; do
    printf '{"id": %d, "title": "Objective %d of 072", "prompt": "Make objective %d pass."}\n' "$n" "$n" "$n" \
        >"$ctl/rsi/backlog/00$n.json"
done
B=$(printf 'bb-072-base' | sha1sum | cut -c1-40)
C1=$(printf 'bb-072-candidate-1' | sha1sum | cut -c1-40)
C2=$(printf 'bb-072-candidate-2' | sha1sum | cut -c1-40)
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
        'brief', 'Objective ' || objective || ' of 072')) \$\$;
create function pg_temp.lands(h numeric, task int, objective int, c text) returns void language sql as \$\$
    select pg_temp.assigned(h - 1, task, objective);
    select pg_temp.rec(h - 0.6, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', c));
    select pg_temp.rec(h - 0.3, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', 'accepted',
        'revision', 0, 'base', '$B', 'scenarios', 4, 'candidate', c));
    select pg_temp.rec(h, jsonb_build_object('type', 'task.landed', 'id', task, 'from', '$B', 'to', c,
        'missions', '[]'::jsonb)) \$\$;

select pg_temp.rec(-40, '{"type":"mission.added","id":1,"text":"072 objectives","origin":"human","sources":[]}');
select pg_temp.rec(-40, jsonb_build_object('type', 'mission.configured', 'id', 1, 'accept', 'gate',
    'gate', '$ctl/rsi/scenarios', 'budget', jsonb_build_object('minutes', 240, 'tool_calls', 600),
    'objectives', jsonb_build_object('backlog', '$ctl/rsi/backlog', 'resident', 'rsi', 'workers', 1)));
select pg_temp.lands(-30, 1, 1, '$C1');
select pg_temp.lands(-5, 2, 2, '$C2');
select pg_temp.assigned(-1, 3, 3);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:landed 2:landed 3:assigned" ] || harness "the seed folded to tasks [$tasks]"
# Each task's landing time, from its task.landed record: {"<task>": ms}.
landed_ms=$(scalar "select json_object_agg(kind -> 'detail' -> 'record' ->> 'id', recorded_at_ms) from spawn_events
    where kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'")
[ "$(printf '%s' "$landed_ms" | jq -c 'keys')" = '["1","2"]' ] || harness "the seed's landings are not tasks 1 and 2: $landed_ms"

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
[ "$(jq -c --argjson l "$landed_ms" '[.missions[] | select(.id == 1) | .objectives[] | [.id, ([.landings[].at_ms] == ([$l[(.landings[].task | tostring)]] ))]]' "$WORK/api.json")" = '[[1,true],[2,true],[3,true]]' ] ||
    harness "/api/rsi's landings are not the seeded ones: $(jq -c '[.missions[] | .objectives[]? | {id, landings}]' "$WORK/api.json" | head -c 600)"
[ "$(jq -c '[.missions[] | select(.id == 1) | .tasks[].id] | sort' "$WORK/api.json")" = '[1,2,3]' ] ||
    harness "/api/rsi's mission 1 does not list tasks 1 to 3"

# --- (1) the footer's last landing ----------------------------------------------------------

cat >"$WORK/last.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { missionLastLanding } = await import(pathToFileURL(`${env.BB_TREE}/web/src/show.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const mission = answer.missions.find((entry) => entry.id === 1);
const landed = (task) => JSON.parse(env.BB_LANDED_MS)[String(task)];
const now = Date.now();
const HOUR = 3_600_000;

test("(1) a mission that has not landed since midnight: its last landing from the answer", () => {
  // The scene saw nothing of it (its events start at midnight).
  assert.equal(missionLastLanding(mission, new Map(), now), landed(2));
});

test("(1) a landing the scene saw after the answer was read is the latest", () => {
  const seen = new Map([[3, now - 60_000]]);
  assert.equal(missionLastLanding(mission, seen, now), now - 60_000);
  // Another mission's task is not this mission's landing.
  assert.equal(missionLastLanding(mission, new Map([[99, now - 60_000]]), now), landed(2));
});

test("(1) in replay, only landings at or before the scene's time", () => {
  assert.equal(missionLastLanding(mission, new Map(), landed(2) - HOUR), landed(1));
  assert.equal(missionLastLanding(mission, new Map(), landed(2)), landed(2));
  assert.equal(missionLastLanding(mission, new Map(), landed(1) - HOUR), null);
});

test("(1) a mission that never landed", () => {
  const fresh = { ...mission, objectives: mission.objectives.map((entry) => ({ ...entry, landings: [] })) };
  assert.equal(missionLastLanding(fresh, new Map(), now), null);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_LANDED_MS=$landed_ms node --test "$WORK/last.test.mjs" >"$WORK/last.out" 2>&1; then
    check_failed "(1) web/src/show.ts missionLastLanding over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/last.out" | head -n 12 | tr '\n' ' ' | head -c 1200)"
fi
note "(1) checked: the footer's last landing"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "072: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "072: the Show footer's last landing is the mission's, from its whole history, as of the scene's time"
exit 0
