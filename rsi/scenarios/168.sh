#!/bin/sh
# Black box: `spawn mission landing`'s `judging` counts only the tasks that
# can still land through the gate (objective 168, after 160).
#
# deploy/on-landing.sh keeps its 5-minute quiet spell only while `judging` >
# 0 (160): a reported, accepted or landing task of the source may land
# within a gate's time, so a burst can still come. But only a gated
# mission's task is landed by the integrator (a gate-accepted task is
# `landing`, then `landed`). A review mission's reported task waits for a
# person, and its `accepted` is final (spawn_task_may_continue keeps a
# mission's `accepted` task when the mission has no gate): it never lands.
# Counted, such a task keeps `judging` above 0 for good, and every landing
# pays the whole quiet spell, as before 160. Likewise a gated mission that
# accepts by review sends a task to its gate only once a person accepted
# it: its reported task cannot land within a gate's time.
#
# The history is seeded as the store holds it and folded by
# spawn_control_refold(), as 052 and 066 do. On source S (a bare git
# repository with a main): gated mission 1's task 1 is reported and task 2
# gate-accepted (landing); review mission 2's task 3 is reported and task 4
# reviewed and accepted. Gated mission 3 on another source has a reported
# task 5. Mission 4 on S has a gate and accepts by review (a person first,
# then its gate): its task 6 is reported, task 7 accepted by a person.
# Checks:
#   (1) `spawn mission landing --source S` says judging 3 (tasks 1, 2, 7);
#   (2) once task 2 has landed, judging 2.
# The folded states are checked to be the seed's first (a harness check).
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
    echo "168: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od git sed grep sha1sum cut; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and the tmux server's socket, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
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
SPAWN_DB_NAME="bb_168_judging_$run"
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
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/landing.json" ]; then
        echo "168: the last answer: $(head -c 600 "$WORK/landing.json")" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "168: harness: unexpected exit $rc" >&2
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


note() { echo "168: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the source and the history --------------------------------------------------------

S=$WORK/spawn.git
git init -q --bare -b main "$S" || harness "git init failed"
git init -q -b main "$WORK/work" || harness "git init failed"
git -C "$WORK/work" -c user.name=t -c user.email=t@localhost commit -q --allow-empty -m main ||
    harness "git commit failed"
git -C "$WORK/work" push -q "$S" main || harness "git push failed"
MAIN=$(git -C "$S" rev-parse main)
B=$(printf 'bb-168-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-168-candidate' | sha1sum | cut -c1-40)
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
create function pg_temp.reported(h numeric, task int, mission int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', mission,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_task_' || task, 'session_id', 'session_worker_task_' || task,
        'brief', 'Task ' || task || ' of 168'));
    select pg_temp.rec(h + 0.1, jsonb_build_object('type', 'task.delivered', 'id', task, 'command_id', task));
    select pg_temp.rec(h + 0.2, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', '$C',
        'report', 'Task ' || task || ' is done.')) \$\$;

select pg_temp.rec(-5, '{"type":"mission.added","id":1,"text":"168 gated","origin":"human","sources":["$S"]}');
select pg_temp.rec(-5, '{"type":"mission.configured","id":1,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
select pg_temp.rec(-5, '{"type":"mission.added","id":2,"text":"168 reviewed","origin":"human","sources":["$S"]}');
select pg_temp.rec(-5, '{"type":"mission.configured","id":2,"accept":"review"}');
select pg_temp.rec(-5, '{"type":"mission.added","id":3,"text":"168 elsewhere","origin":"human","sources":["$WORK/other.git"]}');
select pg_temp.rec(-5, '{"type":"mission.configured","id":3,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
select pg_temp.reported(-4, 1, 1);
select pg_temp.reported(-4, 2, 1);
select pg_temp.rec(-3, '{"type":"task.gated","id":2,"verdict":"accepted","revision":0,"base":"$B","candidate":"$C"}');
select pg_temp.reported(-4, 3, 2);
select pg_temp.reported(-4, 4, 2);
select pg_temp.rec(-3, '{"type":"task.reviewed","id":4,"accepted":true,"note":""}');
select pg_temp.reported(-4, 5, 3);
select pg_temp.rec(-5, '{"type":"mission.added","id":4,"text":"168 reviewed, then gated","origin":"human","sources":["$S"]}');
select pg_temp.rec(-5, '{"type":"mission.configured","id":4,"accept":"review","gate":"$WORK/ctl/scenarios"}');
select pg_temp.reported(-4, 6, 4);
select pg_temp.reported(-4, 7, 4);
select pg_temp.rec(-3, '{"type":"task.reviewed","id":7,"accepted":true,"note":""}');
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"

judging() { # the store's judging for S, through the CLI
    capture "$SPAWN_BIN" mission landing --source "$S"
    [ "$status" -eq 0 ] || harness "spawn mission landing failed: $(err)"
    cp "$WORK/out.txt" "$WORK/landing.json"
    jq -r '.judging' "$WORK/landing.json"
}

# --- the folded states: the seed's, or the harness is wrong ---------------------------
asserted=1
tasks=$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:reported 2:landing 3:reported 4:accepted 5:reported 6:reported 7:accepted" ] || harness "the seed folded to tasks [$tasks]"

# --- (1) only the gate's inputs, and past them, on S ----------------------------------------
expect_eq "$(judging)" 3 "(1) judging counts gated mission 1's tasks 1 (reported) and 2 (landing) and mission 4's task 7 (accepted by a person: its gate's input), not review mission 2's tasks 3 (reported) and 4 (accepted), nor mission 4's task 6 (reported: a person comes first), nor task 5 of another source"

# --- (2) a landed task is no longer counted -------------------------------------------------
capture psql -d "$SPAWN_DB_NAME" -q -c "select spawn_control_record(jsonb_build_object('type', 'task.landed', 'id', 2, 'from', '$B', 'to', '$MAIN'))"
[ "$status" -eq 0 ] || harness "recording task 2's landing failed: $(err)"
expect_eq "$(judging)" 2 "(2) judging once task 2 has landed: tasks 1 and 7"

if [ -s "$WORK/failures" ]; then
    echo "168: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "168: judging counts only the tasks the gate can still land"
exit 0
