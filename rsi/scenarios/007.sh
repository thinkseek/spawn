#!/bin/sh
# Black box: a task the closure rule cancels has a `task.cancelled` record
# (GitHub issue #4).
#
# A resident `core` holds an assigned task, a reported one and an accepted
# one; it is retired. A second mission holds an assigned and a reported
# ephemeral task; it is dropped. Checked, for each of the four tasks the
# closure rule cancels:
#   (1) its state is `cancelled`;
#   (2) `spawn task stages --id N` ends with the stage `cancelled`;
#   (3) the control stream holds exactly one `task.cancelled` record for it;
#   (4) the accepted task (which the rule lets go on) is not cancelled and has
#       no such record;
#   (5) rebuilding the tables from the log (`spawn_control_refold()`) and
#       migrating again (`spawn init-db`) change none of this: the record is
#       written once, by the writer, and replaying the log never adds one.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (2) where retiring a resident or dropping a mission only
# updates the tables. Runs inside the gate's sandbox (scripts/rsi/gate.sh):
# needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# database, no tmux, no network.
set -eu

asserted=""
harness() {
    echo "007: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_007_cancelled_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
cleanup() {
    if [ -n "$database_made" ]; then
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "007: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
export SPAWN_DATABASE_URL

# init_db: `spawn init-db`. The test server is shared: another run's init-db
# can collide on cluster-wide objects ("tuple concurrently updated").
# init-db is idempotent.
init_db() {
    tries=0
    while :; do
        capture "$SPAWN_BIN" init-db
        [ "$status" -eq 0 ] && return 0
        tries=$((tries + 1))
        [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
        sleep 1
    done
}
init_db

# --- helpers ----------------------------------------------------------------

q() { scalar "$1"; }

# spawn <args...>: the CLI, which must succeed (setup only).
spawn_ok() {
    capture "$SPAWN_BIN" "$@"
    [ "$status" -eq 0 ] || harness "spawn $*: exit $status: $(err)"
}

# assign <mission> <kind> <worker>: a task; prints its id.
assign() {
    spawn_ok task assign --mission "$1" --kind "$2" --worker "$3" --brief "work of $3"
    task=$(jq -r .task "$WORK/out.txt")
    [ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
    echo "$task"
}

# deliver_and_report <task>: the dispatcher's bookkeeping through its own SQL
# functions: delivered by a command into the task's session, then reported.
deliver_and_report() {
    session=$(q "select session_id from mission_tasks where id = $1")
    agent=$(q "select agent_id from mission_tasks where id = $1")
    spawn_ok submit --principal bb-harness --agent "$agent" --session "$session" \
        --key "bb-deliver-$1" --action '{"type":"input","text":"the task"}'
    command=$(q "select id from spawn_commands where idempotency_key = 'bb-deliver-$1'")
    [ "$(q "select spawn_task_delivered($1, $command)")" = t ] || harness "task $1 not delivered"
    [ "$(q "select spawn_task_reported($1, 'done')")" = t ] || harness "task $1 not reported"
}

state_of() { q "select state from mission_tasks where id = $1"; }
cancelled_records() {
    q "select count(*) from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
# last_stage <task>: sets $stage to the last stage `spawn task stages` shows.
last_stage() {
    capture "$SPAWN_BIN" task stages --id "$1"
    [ "$status" -eq 0 ] || fail "spawn task stages --id $1 exited $status: $(err)"
    stage=$(jq -r 'if type == "array" and length > 0 then .[-1].stage else "none" end' \
        "$WORK/out.txt") || fail "spawn task stages --id $1 printed no JSON array: $(out)"
}

# --- a retired resident and a dropped mission ---------------------------------

spawn_ok mission add --text "007 resident work"
resident_mission=$(jq -r .mission "$WORK/out.txt")
spawn_ok mission add --text "007 dropped mission"
dropped_mission=$(jq -r .mission "$WORK/out.txt")
spawn_ok resident propose --name core --charter "Owns the 007 concern."
spawn_ok resident approve --name core

resident_assigned=$(assign "$resident_mission" resident core)
resident_reported=$(assign "$resident_mission" resident core)
resident_accepted=$(assign "$resident_mission" resident core)
deliver_and_report "$resident_reported"
deliver_and_report "$resident_accepted"
spawn_ok task accept --id "$resident_accepted"

dropped_assigned=$(assign "$dropped_mission" ephemeral a)
dropped_reported=$(assign "$dropped_mission" ephemeral b)
deliver_and_report "$dropped_reported"

spawn_ok resident retire --name core
spawn_ok mission close --id "$dropped_mission" --state dropped

cancelled="$resident_assigned $resident_reported $dropped_assigned $dropped_reported"

# check <when>: every assertion, after <when>.
check() {
    for task in $cancelled; do
        assert_eq "$(state_of "$task")" cancelled "(1) task $task's state $1"
        last_stage "$task"
        assert_eq "$stage" cancelled "(2) the last stage of task $task $1"
        assert_eq "$(cancelled_records "$task")" 1 "(3) task.cancelled records of task $task $1"
    done
    assert_eq "$(state_of "$resident_accepted")" accepted \
        "(4) the accepted task $resident_accepted's state $1"
    assert_eq "$(cancelled_records "$resident_accepted")" 0 \
        "(4) task.cancelled records of the accepted task $resident_accepted $1"
    last_stage "$resident_accepted"
    assert_eq "$stage" reviewed \
        "(4) the last stage of the accepted task $resident_accepted $1"
}

check "after the retirement and the drop"

# --- replaying the log ----------------------------------------------------------

sql "select spawn_control_refold()" >/dev/null 2>"$WORK/err.txt" ||
    harness "spawn_control_refold() failed: $(err)"
check "(5) after the tables were rebuilt from the log"
init_db
check "(5) after init-db ran again"

echo "007: retirement and a drop recorded task.cancelled once for each cancelled task; replays add none"
exit 0
