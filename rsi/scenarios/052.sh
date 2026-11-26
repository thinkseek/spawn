#!/bin/sh
# Black box: an objective the integrator recorded solved on the base reads as
# solved everywhere the backlog is shown. The integrator records
# `objective.solved` when a task's gate finds the objective's scenario
# already passing at the base (no work could change anything), cancels the
# task on that record, and counts the objective as landed: it never assigns
# it again. The views do not read that record: /api/rsi, /api/overview's
# RSI summary and `spawn mission objectives` count only landed tasks, so
# the objective stays "open" and is named the next one to be assigned.
# And `next_objective` ("what the integrator assigns next") is the first
# open row, while the integrator skips objectives with a task in flight and
# puts those that yielded behind the others (crate::objectives::choose).
# On the live fleet, mission 4's objective 34 was recorded solved on the
# base at 2026-11-08 10:51 UTC (event 109278, task 201). On 11-11 the RSI
# page still showed it open and as `next_objective`, while the integrator
# had assigned 43, 46 and 47 after it and never 34 again.
#
# Required (the objective's prompt says each rule): an objective with an
# `objective.solved` record in its mission is "solved", as a landing makes
# it, in every view built from the backlog; its row names the record
# (`solved_on_base`: {event, task, base} and maybe more fields, null for
# every other objective);
# the counts follow; and `next_objective` is the objective the integrator's
# own choice would assign next (solved, in flight, held and owner-judged
# objectives left out, yields and failed attempts ordered, `after` obeyed).
#
# The scenario seeds a fresh database with no loop, as 043 and 049 do:
# control records written as the store holds them and folded by
# spawn_control_refold(). One objective mission (accept gate) whose backlog
# holds objectives 1 to 5, hours before now:
#   task 1 (objective 1): assigned -9, reported -8, gated accepted -7.5,
#     landed -7.
#   task 2 (objective 2): assigned -6, reported -5; objective.solved
#     (task 2, base B) -4.5; task.cancelled naming it -4.5.
#   another mission (2): objective.solved for its objective 3 -4.
#   task 4 (objective 4): assigned -4, rejected three times, cancelled
#     `yielded` -3.3 (one yield).
#   task 3 (objective 3): assigned -3, reported -2 (in flight).
#   objective 5: never assigned.
# Checked:
#   (1) /api/rsi: statuses solved, solved, open, open, open; objective 2's
#       `solved_on_base` names the record, task 2 and B, and every other
#       row's is null.
#   (2) /api/rsi `next_objective` 5: not 2 (solved), not 3 (in flight), not
#       4 (it yielded, 5 never did).
#   (3) /api/overview's rsi summary: open 3, solved 2, next_objective 5.
#   (4) `spawn mission objectives --mission 1`: open [3, 4, 5], solved 2,
#       next_objective 5.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (351fb1e6): objective 2 reads
# "open" and next_objective is 2. It passes once the views read the
# integrator's record as the integrator does.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no tmux, no
# network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "052: harness: $*" >&2
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
for tool in psql jq od curl sed grep sha1sum cut; do
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
SPAWN_DB_NAME="bb_052_solved_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "052: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "052: harness: unexpected exit $rc" >&2
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

fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
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


note() { echo "052: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || fail "$3: expected [$2], got [$1]"
}

# --- the backlog and the history -------------------------------------------------------

ctl=$WORK/ctl
mkdir -p "$ctl/rsi/backlog" "$ctl/rsi/scenarios"
for n in 1 2 3 4 5; do
    printf '{"id": %d, "title": "Objective %d of 052", "prompt": "Make scenario 00%d pass."}\n' "$n" "$n" "$n" \
        >"$ctl/rsi/backlog/00$n.json"
done
# Scenario file names built here: a literal other-scenario name in this text
# would tie the gate's base cache key to that scenario.
SC2="$ctl/rsi/scenarios/00${two:=2}.sh"
SC3="/elsewhere/00${three:=3}.sh"
B=$(printf 'bb-052-base' | sha1sum | cut -c1-40)
C1=$(printf 'bb-052-candidate-1' | sha1sum | cut -c1-40)
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
        'scenario', '$ctl/rsi/scenarios/00' || objective || '.sh',
        'brief', 'Objective ' || objective || ' of 052')) \$\$;

select pg_temp.rec(-10, '{"type":"mission.added","id":1,"text":"052 objectives","origin":"human","sources":[]}');
select pg_temp.rec(-10, jsonb_build_object('type', 'mission.configured', 'id', 1, 'accept', 'gate',
    'gate', '$ctl/rsi/scenarios', 'budget', jsonb_build_object('minutes', 240, 'tool_calls', 600),
    'objectives', jsonb_build_object('backlog', '$ctl/rsi/backlog', 'resident', 'rsi', 'workers', 2)));
select pg_temp.assigned(-9, 1, 1);
select pg_temp.rec(-8, '{"type":"task.reported","id":1,"commit":"$C1"}');
select pg_temp.rec(-7.5, '{"type":"task.gated","id":1,"verdict":"accepted","revision":0,"base":"$B","scenarios":4,"candidate":"$C1"}');
select pg_temp.rec(-7, '{"type":"task.landed","id":1,"from":"$B","to":"$C1","missions":[]}');
select pg_temp.assigned(-6, 2, 2);
select pg_temp.rec(-5, '{"type":"task.reported","id":2,"commit":"$C1"}');
select pg_temp.rec(-4.5, '{"type":"objective.solved","id":1,"objective":2,"task":2,"scenario":"$SC2","base":"$B","log":"| the scenario passes on the base"}');
select pg_temp.rec(-4.5, jsonb_build_object('type', 'task.cancelled', 'id', 2, 'by', 'spawn-integrate',
    'solved', (select max(id) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'objective.solved' and kind -> 'detail' -> 'record' ->> 'id' = '1'),
    'reason', 'objective 2 is solved on the base: $SC2 passes at $B, so task #2 ends without a rejection'));
-- Another mission's objective 3 solved on its base: not this mission's.
select pg_temp.rec(-4, '{"type":"objective.solved","id":2,"objective":3,"task":99,"scenario":"$SC3","base":"$B","log":""}');
-- Objective 3 in flight: assigned and reported, its gate not run yet.
select pg_temp.assigned(-3, 3, 3);
select pg_temp.rec(-2, '{"type":"task.reported","id":3,"commit":"$C1"}');
-- Objective 4 failed three times and its task yielded the slot: it waits
-- behind the objectives that never yielded.
select pg_temp.assigned(-4, 4, 4);
select pg_temp.rec(-3.9, '{"type":"task.reported","id":4,"commit":"$C1"}');
select pg_temp.rec(-3.8, '{"type":"task.gated","id":4,"verdict":"rejected","revision":0,"base":"$B","scenarios":4,"reason":"the scenario fails on the candidate"}');
select pg_temp.rec(-3.7, '{"type":"task.reported","id":4,"commit":"$C1"}');
select pg_temp.rec(-3.6, '{"type":"task.gated","id":4,"verdict":"rejected","revision":1,"base":"$B","scenarios":4,"reason":"the scenario fails on the candidate"}');
select pg_temp.rec(-3.5, '{"type":"task.reported","id":4,"commit":"$C1"}');
select pg_temp.rec(-3.4, '{"type":"task.gated","id":4,"verdict":"rejected","revision":2,"base":"$B","scenarios":4,"reason":"the scenario fails on the candidate"}');
select pg_temp.rec(-3.3, '{"type":"task.cancelled","id":4,"yielded":true,"objective":4,"failures":3,"limit":3,"rejections":3,"reason":"objective 4 yields its slot after 3 failed attempts","by":"spawn-integrate"}');
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
solved=$(scalar "select max(id) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'objective.solved' and kind -> 'detail' -> 'record' ->> 'id' = '1'")
[ -n "$solved" ] || harness "no objective.solved record was seeded"
tasks=$(scalar "select string_agg(id || ':' || state || ':' || coalesce(objective::text, '-'), ' ' order by id) from mission_tasks")
[ "$tasks" = "1:landed:1 2:cancelled:2 3:reported:3 4:cancelled:4" ] || harness "the seed folded to tasks [$tasks]"
[ "$(scalar "select objectives ->> 'backlog' from mission_missions where id = 1")" = "$ctl/rsi/backlog" ] ||
    harness "mission 1 did not fold as an objective mission"

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
get() { # <path> <file>: the status code
    bb_http "$2" --noproxy '*' "http://127.0.0.1:$port$1"
}

# --- (1) /api/rsi -----------------------------------------------------------------------

code=$(get /api/rsi "$WORK/api.json")
[ "$code" = 200 ] || harness "GET /api/rsi answered $code"
row() { jq -c --argjson id "$1" "[.missions[] | select(.id == 1) | .objectives[] | select(.id == \$id)][0] | $2" "$WORK/api.json"; }
expect_eq "$(jq -c '[.missions[] | select(.id == 1) | .objectives[] | .status]' "$WORK/api.json")" '["solved","solved","open","open","open"]' \
    "(1) /api/rsi: the statuses of objectives 1 to 5 (2 was recorded solved on the base, event $solved)"
# Only the three keys this objective names: a later objective may add
# fields to the record (063 adds at_ms for its time axis), which is
# compatible and no failure here.
expect_eq "$(row 2 '.solved_on_base | if type == "object" then {base, event, task} else (. // "none") end')" "{\"base\":\"$B\",\"event\":$solved,\"task\":2}" \
    "(1) /api/rsi: objective 2's solved_on_base (the record that solved it)"
for n in 1 3 4 5; do
    expect_eq "$(row "$n" 'if has("solved_on_base") then .solved_on_base else "absent" end')" null \
        "(1) /api/rsi: objective $n's solved_on_base (no such record)"
done
note "(1) /api/rsi: objective 2 solved on the base by event $solved; another mission's record solves nothing here"

# --- (2) next_objective is the integrator's own choice -------------------------------------

expect_eq "$(jq -c '[.missions[] | select(.id == 1) | .next_objective][0]' "$WORK/api.json")" 5 \
    "(2) /api/rsi: next_objective, what the integrator assigns next (1 landed, 2 solved on the base, 3 has a task in flight, 4 yielded once and waits behind 5, which never yielded)"
note "(2) /api/rsi: next_objective 5 (not 2, solved; not 3, in flight; not 4, yielded)"

# --- (3) /api/overview ------------------------------------------------------------------

code=$(get /api/overview "$WORK/api.json")
[ "$code" = 200 ] || harness "GET /api/overview answered $code"
expect_eq "$(jq -c '.rsi | [.objectives.open, .objectives.solved, .next_objective]' "$WORK/api.json")" '[3,2,5]' \
    "(3) /api/overview's rsi summary: [open, solved, next_objective]"
note "(3) /api/overview: open 3, solved 2, next 5"

# --- (4) spawn mission objectives --------------------------------------------------------

capture "$SPAWN_BIN" mission objectives --mission 1
[ "$status" -eq 0 ] || harness "spawn mission objectives exited $status: $(err)"
cp "$WORK/out.txt" "$WORK/api.json"
expect_eq "$(jq -c '.[0] | [[.open[].id], .solved, .next_objective]' "$WORK/out.txt" 2>/dev/null || echo unreadable)" '[[3,4,5],2,5]' \
    "(4) spawn mission objectives --mission 1: [open ids, solved, next_objective]"
note "(4) spawn mission objectives: open [3, 4, 5], solved 2, next 5"

echo "052: an objective solved on the base reads as solved in every view of the backlog, naming its record, and next_objective is the integrator's own choice"
exit 0
