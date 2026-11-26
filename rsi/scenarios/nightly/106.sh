#!/bin/sh
# Black box: a task assigned while the alert step starts is the step's
# from its first check: its assignment is said (a `task_state` notice), and
# once it is cancelled its mission's idle time is counted (`mission_idle`).
#
# The problem (cc-astra A41; rev-e, sync/review-099-on-322.md; T322 was
# rejected at 2026-11-13 03:31 on 099.sh, and the same tree was accepted at
# 03:47: 13.8 gate minutes and a resident's turn lost). The alert step's
# start (crates/spawn-cli/src/alerts.rs, Monitor::start) reads the missions
# and tasks (the store's MIRROR) first, then the alerts, the history, the
# observations, the commands and the messages, and only then where the event
# log stands (CLOCK): its first check reads the records after that. A
# record committed between the MIRROR read and the CLOCK read is in neither:
# the step never learns of it. A task assigned in that window is unknown to
# the step for the life of the loop: its assignment is never said, and when
# it ends its mission's idle time never starts, so no `mission_idle` fires
# (099's check (4) on T322's gate).
#
# Required: whatever runs between the start's reads, every record is the
# step's exactly once: a record the start's reads hold is not fed again (a
# task's revisions, a mission's gate rejections fold once), and every later
# record is fed by the checks.
#
# A real `spawn loop` (the Bridge, the dispatcher, the alert step; no
# supervisor, no model judge) and `spawn serve` run two missions, each
# taking its work from an objective backlog with a resident of its own
# (bbone, bbtwo) on a fake engine. SPAWN_ALERT_MISSION_IDLE_SECS=8, read
# every 2 s. The window is hit for certain, from outside. Mission 3 is
# halted before the loop starts, so the start reads its halt back in the
# history and records its mission_halted alert: after it has read the
# missions and tasks, before it reads where the log stands. A trigger the
# scenario puts on spawn_alerts assigns mission 2's task
# (spawn_objective_assign, the integrator's own path) in the transaction of
# the first alert recorded, once. (A lock on spawn_alerts cannot make the
# window: the loop's storage sample, earlier in the same first pass, sizes
# every table and would wait on it first.)
#   Mission 1's task is assigned before the loop starts (the control). Both
#   tasks are delivered, then cancelled (`spawn task cancel`).
#   (1) mission 2's assignment is said: /api/alerts holds the notice
#       `task_state:<task>:<its task.assigned event>` within 20 s of the
#       deliveries.
#   (2) its cancel is said: `task_state:<task>:<its task.cancelled event>`
#       within 20 s of the cancel.
#   (3) mission 2 is idle: once mission 1 (the control) has its open
#       `mission_idle` alert (the barrier: a reading after the bound ran),
#       mission 2 has its open `mission_idle`, keyed on its task's
#       task.cancelled event, within 10 s more.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "106: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl; do
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
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home";; esac
export HOME
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_106_runs_$run"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "106"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    herdr_cleanup
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "106: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engines.log" ]; then
            echo "106: the engines' starts (ms|resident|execution|start|pid|native):" >&2
            cat "$WORK/engines.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "106: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the world ----------------------------------------------------------------

cat >"$WORK/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL
repo=$WORK/repo.git
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main


# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet, the transcript's end of the turn first, then the prompt.
# It writes its native transcript where Claude Code does, and logs each
# start: ms|resident|execution|new or resume|pid|native session.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
start=new
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=$2 start=resume;;
  --session-id) native=$2;;
  esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
name=${SPAWN_SESSION_ID#session_resident_}
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
printf '%s|%s|%s|%s|%s|%s\n' "$(date +%s%3N)" "$name" "$SPAWN_EXECUTION_ID" "$start" "$$" "$native" \
  >>"$BB_WORK/engines.log"
printf 'Welcome to Claude Code\n\n❯ '
pending=0
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
  elif [ "$pending" = 1 ]; then
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_ENVELOPE SPAWN_SLICE http_proxy https_proxy \
    HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# A mission with no task in work this long is idle.
SPAWN_ALERT_MISSION_IDLE_SECS=8
SPAWN_RECOVER_BACKOFF_SECS=1
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_MISSION_IDLE_SECS \
    SPAWN_RECOVER_BACKOFF_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl/scenarios" "$WORK/ctl/backlog" "$WORK/web"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engines.log"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
note() { echo "106: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")";; esac
    echo "$id"
}
# objective_assign <mission> <objective>: the integrator's own path; prints
# the task's id, or nothing (the refusal in $WORK/assign.err).
objective_assign() {
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 \
        -c "select spawn_objective_assign($1, $2, '$WORK/ctl/scenarios/$2.sh', 'Objective $2 of 106 mission $1', null)" \
        2>"$WORK/assign.err" | head -n 1 || true
}
task_state() { q "select state from mission_tasks where id = $1"; }
delivered() { [ "$(task_state "$1")" = delivered ]; }
cancel() { # <task>
    capture "$SPAWN_BIN" task cancel --id "$1"
    [ "$status" -eq 0 ] || harness "spawn task cancel --id $1 failed: $(err)"
}
task_event() { # <type> <task id>: the newest such record's event id
    q "select max(id) from spawn_events where event_name = 'control.recorded'
         and kind -> 'detail' -> 'record' ->> 'type' = '$1'
         and kind -> 'detail' -> 'record' ->> 'id' = '$2'"
}
start_serve() {
    rm -f "$WORK/serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >"$WORK/serve.log" 2>&1 &
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
    base="http://127.0.0.1:$port"
}
get() { # <path> <file>
    code=$(curl -sS --noproxy '*' -o "$2" -w '%{http_code}' "$base$1" 2>"$WORK/curl.err") || code="000 ($(head -c 200 "$WORK/curl.err"))"
    [ "$code" = 200 ] || harness "GET $1 answered $code: $(head -c 300 "$2" 2>/dev/null)"
}
has_alert() { # <key>: /api/alerts holds it
    get /api/alerts "$WORK/alerts.json"
    [ "$(jq --arg k "$1" '[.[] | select(.key == $k)] | length' "$WORK/alerts.json")" -gt 0 ]
}
open_idle() { # <mission>: the keys of its open mission_idle alerts
    get /api/alerts "$WORK/alerts.json"
    jq -c --arg s "mission #$1" '[.[] | select(.kind == "mission_idle" and .subject == $s and .state == "open") | .key]' "$WORK/alerts.json"
}
idle_open() { [ "$(open_idle "$1")" != "[]" ]; }

# --- the missions ----------------------------------------------------------------

for name in bbone bbtwo; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 106 $name work."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
mission_no=0
for name in bbone bbtwo; do
    mission_no=$((mission_no + 1))
    record "{\"type\":\"mission.added\",\"id\":$mission_no,\"text\":\"106 mission $mission_no\",\"origin\":\"human\",\"sources\":[]}" >/dev/null
    record "{\"type\":\"mission.configured\",\"id\":$mission_no,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"$name\",\"workers\":1}}" >/dev/null
done

# The control: mission 1's task is in the store before the loop starts.
t1=$(objective_assign 1 1)
[ -n "$t1" ] || harness "spawn_objective_assign on mission 1 failed: $(head -c 300 "$WORK/assign.err")"

# Mission 3 is halted before the loop starts: the start reads its halt in
# the history and records its mission_halted alert, after the missions and
# tasks, before where the log stands. The scenario's trigger on spawn_alerts
# assigns mission 2's task in that alert's transaction, once.
record '{"type":"mission.added","id":3,"text":"106 mission 3","origin":"human","sources":[]}' >/dev/null
record '{"type":"mission.halted","id":3,"reason":"106: read back by the start"}' >/dev/null
sql "create table bb_106_assigned (task bigint);
create function bb_106_assign() returns trigger language plpgsql as \$\$
begin
    if not exists (select 1 from bb_106_assigned) then
        insert into bb_106_assigned
        select spawn_objective_assign(2, 1, '$WORK/ctl/scenarios/1.sh', 'Objective 1 of 106 mission 2', null);
    end if;
    return new;
end \$\$;
create trigger bb_106_assign after insert on spawn_alerts
    for each row execute function bb_106_assign();" >/dev/null 2>"$WORK/trigger.err" ||
    harness "the scenario's trigger on spawn_alerts was not made: $(head -c 300 "$WORK/trigger.err")"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

assigned() { [ -n "$(q "select task from bb_106_assigned")" ]; }
wait_until 60 assigned || harness "the start recorded no alert within 60 s (mission 2's task was not assigned)"
t2=$(q "select task from bb_106_assigned")
case "$t2" in '' | *[!0-9]*) harness "mission 2's assignment in the start gave no task: [$t2]";; esac
a2=$(task_event task.assigned "$t2")
[ -n "$a2" ] || harness "task #$t2 has no task.assigned record"
first=$(q "select key from spawn_alerts order by fired_event_id limit 1")
# The window is the start's: the first alert recorded is the halt's, read
# back by the start (an alert recorded before the mirror would not test it).
case $first in
    mission_halted:3:*);;
    *) harness "the first alert recorded is [$first], not mission 3's mission_halted read back by the start";;
esac
note "task #$t2 was assigned at event $a2, in the transaction of the start's first alert ($first)"
start_serve

for task in "$t1" "$t2"; do
    wait_until 60 delivered "$task" || harness "task #$task was not delivered within 60 s (it is $(task_state "$task"))"
done

# --- (1) the assignment is said ----------------------------------------------------

wait_until 20 has_alert "task_state:$t2:$a2" ||
    check_failed "(1) no task_state notice task_state:$t2:$a2 for mission 2's task #$t2 within 20 s of the deliveries (its assignment, made while the step started)"
note "(1) checked: the assignment's notice"

# --- (2) the cancel is said ------------------------------------------------------

cancel "$t1"
cancel "$t2"
c1=$(task_event task.cancelled "$t1")
c2=$(task_event task.cancelled "$t2")
[ -n "$c1" ] && [ -n "$c2" ] || harness "a task.cancelled record is missing (#$t1: [$c1], #$t2: [$c2])"
wait_until 20 has_alert "task_state:$t2:$c2" ||
    check_failed "(2) no task_state notice task_state:$t2:$c2 for the cancel of task #$t2 within 20 s"
note "(2) checked: the cancel's notice"

# --- (3) mission 2 is idle --------------------------------------------------------

wait_until 40 idle_open 1 ||
    harness "mission 1 (the control) had no open mission_idle within 40 s of its task's cancel: $(open_idle 1)"
expect_eq "$(open_idle 1)" "[\"mission_idle:1:$c1\"]" "(3) mission 1's open mission_idle (the control)"
wait_until 10 idle_open 2 || true
expect_eq "$(open_idle 2)" "[\"mission_idle:2:$c2\"]" "(3) mission 2's open mission_idle, 10 s after mission 1's"
note "(3) checked: mission 2 is idle"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "106: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "106: a task assigned while the alert step starts is said, and its mission goes idle when it ends"
exit 0
