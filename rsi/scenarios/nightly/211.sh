#!/bin/sh
# Black box: a delivered task says what its worker did since it reached it
# (GitHub #63 part 1, objective 211).
#
# The problem (#63, observed on mission 2): a task's stages are assigned,
# delivered, reported, reviewed. Between delivered and reported the board
# had no signal at all, so tasks that sat in delivered for 30-74 minutes
# read the same whether the worker was working or hung.
#
# Required (read through `GET /api/missions`; the history and the workers'
# events seeded as the store records them):
#   (1) task 1, delivered 60 min ago to session_worker_a, whose engine
#       (source claude) started a call before the delivery (a previous
#       task's); after it, the brief reached the engine (message.received
#       and message.taken: the worker's input), then it started two calls
#       and ended one; after those
#       the Bridge read a busy pane and the Observer judged it: its
#       `activity` is {since_ms: the time of its task.delivered record,
#       at_ms: the time of the call that ended, event: "tool.completed",
#       tool_calls: 2}. Neither the busy pane nor the judgement is activity,
#       and the call before the delivery is not counted.
#   (2) task 2, delivered 30 min ago to session_worker_b, whose engine
#       then took the brief (message.received and message.taken, as live
#       at +0 s after every delivery) and did nothing more, after which only
#       the Bridge read its pane: {since_ms: its delivery, at_ms: null,
#       event: null, tool_calls: 0}. The worker's input is not its output.
#   (3) task 3 (reported) and task 4 (assigned) carry `activity` null.
#   (4) session_worker_b's engine then ends a turn (turn.stopped, as it
#       does when it answers): task 2's activity moves to it (event
#       "turn.stopped", tool_calls 0), on a fresh serve; task 1's is
#       unchanged.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "211: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep cut awk; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_211_activity_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/board.json" ]; then
        echo "211: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/board.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "211: harness: unexpected exit $rc" >&2
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


note() { echo "211: $*" >&2; }
expect_eq() { # <got> <want> <what>
    # A filter that does not compile or run is the harness's fault, not a red.
    case "$1" in *jq-failed*) harness "$3: a jq filter failed" ;; esac
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the world: one mission, four tasks, two workers' events ---------------------------------

# Control records and worker events in log order (the event ids follow
# it): what a worker did before its task's delivery comes before the
# delivery's record. ms(h): now plus h hours. Returns nothing.
cat >"$WORK/seed.sql" <<'SQL'
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as $$
    select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint + round(h * 3600000)::bigint $$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) $$;
create function pg_temp.ev(w text, seq bigint, h numeric, src text, name text, detail jsonb)
returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_' || w, 'session_worker_' || w, 'exec_bb_211_' || w, src, null,
            jsonb_build_object('name', name, 'detail', detail), seq, pg_temp.ms(h)) $$;
select pg_temp.rec(-3, '{"type":"mission.added","id":1,"text":"211 mission","origin":"human","sources":[]}');
select pg_temp.rec(-3, '{"type":"mission.configured","id":1,"accept":"review"}');
select pg_temp.rec(-2, '{"type":"task.assigned","id":1,"mission_id":1,"kind":"ephemeral","created_by":"human","agent_id":"agent_worker_a","session_id":"session_worker_a","brief":"Task 1 of 211"}');
select pg_temp.rec(-2, '{"type":"task.assigned","id":2,"mission_id":1,"kind":"ephemeral","created_by":"human","agent_id":"agent_worker_b","session_id":"session_worker_b","brief":"Task 2 of 211"}');
select pg_temp.rec(-2, '{"type":"task.assigned","id":3,"mission_id":1,"kind":"ephemeral","created_by":"human","agent_id":"agent_worker_c","session_id":"session_worker_c","brief":"Task 3 of 211"}');
select pg_temp.rec(-2, '{"type":"task.assigned","id":4,"mission_id":1,"kind":"ephemeral","created_by":"human","agent_id":"agent_worker_d","session_id":"session_worker_d","brief":"Task 4 of 211"}');
-- Before task 1's delivery: a previous task's call on worker a.
select pg_temp.ev('a', 1, -1.5, 'claude', 'tool.started', '{"tool_name":"Bash","tool_use_id":"toolu_old"}');
select pg_temp.ev('a', 2, -1.5, 'claude', 'tool.completed', '{"tool_use_id":"toolu_old","duration_ms":5}');
select pg_temp.rec(-1, '{"type":"task.delivered","id":1,"command_id":1}');
select pg_temp.rec(-1, '{"type":"task.delivered","id":3,"command_id":3}');
select pg_temp.rec(-0.5, '{"type":"task.delivered","id":2,"command_id":2}');
select pg_temp.rec(-0.25, '{"type":"task.reported","id":3,"report":"done"}');
-- After task 1's delivery: two calls started, one ended; then a busy pane
-- the Bridge read and an Observer judgement, both newer.
select pg_temp.ev('a', 3, -1, 'claude', 'message.received', null);
select pg_temp.ev('a', 4, -1, 'claude', 'message.taken', '{"command_id":1}');
select pg_temp.ev('a', 5, -0.75, 'claude', 'tool.started', '{"tool_name":"Bash","tool_use_id":"toolu_1"}');
select pg_temp.ev('a', 6, -0.70, 'claude', 'tool.completed', '{"tool_use_id":"toolu_1","duration_ms":5}');
select pg_temp.ev('a', 7, -0.40, 'claude', 'tool.started', '{"tool_name":"Bash","tool_use_id":"toolu_2"}');
select pg_temp.ev('a', 8, -0.20, 'claude', 'tool.completed', '{"tool_use_id":"toolu_2","duration_ms":5}');
select pg_temp.ev('a', 9, -0.05, 'bridge', 'screen.observed', '{"state":"busy","evidence":"Thinking (esc to interrupt)"}');
select pg_temp.ev('a', 10, -0.02, 'observer', 'observation.recorded', '{"state":"working"}');
-- After task 2's delivery: its brief reaching the engine (input, not
-- output: the transcript's user line and the command's marker), then only
-- a pane the Bridge read.
select pg_temp.ev('b', 1, -0.5, 'claude', 'message.received', null);
select pg_temp.ev('b', 2, -0.5, 'claude', 'message.taken', '{"command_id":2}');
select pg_temp.ev('b', 3, -0.1, 'bridge', 'screen.observed', '{"state":"busy","evidence":"Thinking (esc to interrupt)"}');
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
states=$(scalar "select string_agg(id || '=' || state, ',' order by id) from mission_tasks")
[ "$states" = "1=delivered,2=delivered,3=reported,4=assigned" ] || harness "the seed's task states are [$states]"

# The times the answers must name, from the seeded log itself.
delivered_at() { scalar "select recorded_at_ms from spawn_events where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered' and kind -> 'detail' -> 'record' ->> 'id' = '$1'"; }
event_at() { scalar "select recorded_at_ms from spawn_events where session_id = 'session_worker_$1' and session_seq = $2"; }
since1=$(delivered_at 1)
since2=$(delivered_at 2)
ended1=$(event_at a 8)
[ -n "$since1" ] && [ -n "$since2" ] && [ -n "$ended1" ] || harness "the seeded times are not all there"

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
serve_start() {
    : >"$WORK/serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >>"$WORK/serve.log" 2>&1 &
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
}
serve_stop() {
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
# <case>: GET /api/missions on a fresh serve (it shares a board answer for a
# second); leaves board.json.
read_board() {
    serve_start
    code=$(bb_http "$WORK/board.json" --noproxy '*' "http://127.0.0.1:$port/api/missions")
    serve_stop
    [ "$code" = 200 ] || harness "($1) GET /api/missions answered $code: $(head -c 300 "$WORK/board.json")"
    [ "$(jq '[.missions[].tasks[]] | length' "$WORK/board.json" 2>/dev/null)" = 4 ] ||
        harness "($1) the board does not hold the 4 seeded tasks"
}
# The named fields of task <id>'s activity, or "absent" when the task has no
# such key.
activity() {
    jq -c --argjson id "$1" '[.missions[].tasks[] | select(.id == $id)] | .[0]
        | if has("activity") | not then "absent"
          elif .activity == null then null
          else .activity | {since_ms, at_ms, event, tool_calls} end' "$WORK/board.json" || echo jq-failed
}

# --- (1)-(3) ---------------------------------------------------------------------------------------

read_board 1
expect_eq "$(activity 1)" "{\"since_ms\":$since1,\"at_ms\":$ended1,\"event\":\"tool.completed\",\"tool_calls\":2}" \
    "(1) task 1: its worker's latest engine event since delivery (not the busy pane or the judgement after it) and the 2 calls it started since (not the one before)"
note "(1) checked: a working worker"
expect_eq "$(activity 2)" "{\"since_ms\":$since2,\"at_ms\":null,\"event\":null,\"tool_calls\":0}" \
    "(2) task 2: no engine output since its delivery (the brief reaching the engine and the pane the Bridge read are none), no call"
note "(2) checked: a worker with no output"
expect_eq "$(activity 3)" null "(3) task 3 (reported): activity null"
expect_eq "$(activity 4)" null "(3) task 4 (assigned): activity null"
note "(3) checked: tasks not delivered"

# --- (4) worker b's engine ends a turn ----------------------------------------------------------

cat >"$WORK/more.sql" <<'SQL'
\set ON_ERROR_STOP 1
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                          kind, session_seq, recorded_at_ms)
values ('agent_worker_b', 'session_worker_b', 'exec_bb_211_b', 'claude', null,
        '{"name": "turn.stopped", "detail": {"input_tokens": 1200, "output_tokens": 80}}',
        4, (floor(extract(epoch from clock_timestamp()) * 1000))::bigint);
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/more.sql"
[ "$status" -eq 0 ] || harness "recording worker b's turn end failed: $(err)"
said=$(event_at b 4)
[ -n "$said" ] || harness "worker b's turn end is not there"
read_board 4
expect_eq "$(activity 2)" "{\"since_ms\":$since2,\"at_ms\":$said,\"event\":\"turn.stopped\",\"tool_calls\":0}" \
    "(4) task 2: its worker's turn end is its latest engine event"
expect_eq "$(activity 1)" "{\"since_ms\":$since1,\"at_ms\":$ended1,\"event\":\"tool.completed\",\"tool_calls\":2}" \
    "(4) task 1: unchanged by worker b's turn end"
note "(4) checked: activity follows the engine"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "211: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "211: a delivered task says when its worker's engine last moved and its tool calls since delivery"
exit 0
