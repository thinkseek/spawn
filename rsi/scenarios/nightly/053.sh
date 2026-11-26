#!/bin/sh
# Black box: a mission's gate_rejections notices follow every change of the
# count, the resets by a cancel included.
#
# The count of gate rejections in a row (mission_missions.gate_rejections)
# goes back to 0 on an accepted gate, on a resume, and on the cancel of a
# task of the objective every rejection of the run was: the integrator's
# yield of the slot (three rejections of one objective), the owner's cancel,
# an objective solved on the base (schema.sql, the `task.cancelled` fold).
# The alert step's `mission_state` notice says "a record after which
# gate_rejections differs" (alerts.rs), but its own copy of the count resets
# only on an accept and a resume. A cancel that ends the run fires no
# notice, and the step's count stays where it was until the loop restarts:
# the next rejection reads "3 -> 4" while the store says 1, the next accept
# "3 -> 0" where the run was 1. On the live fleet, task 249 of mission 4
# yielded at 2026-11-10 20:06 UTC (event 127934) after its third rejection:
# the store's count went 3 -> 0 with no notice; the next one read "0 -> 1"
# only because the loop happened to restart 7 s later.
#
# Required: a record after which the mission's gate_rejections differs fires
# a `mission_state` notice saying so (by that record), a cancel that ends the
# run included; a cancel of a task outside the run changes nothing and fires
# none; and every notice's `after` is the store's count, after a restart too.
#
# A real `spawn loop` runs (its alert step reads every 2 s). The records are
# written as the integrator and the gate write them, through
# spawn_control_record(); the owner's cancels through `spawn task cancel`.
# One gated mission:
#   (1) the yield: task 1 (objective 1) rejected three times, then cancelled
#       `yielded`; then task 2 (objective 2) rejected once.
#       gate_rejections notices: 0 -> 1, 1 -> 2, 2 -> 3 (by task.gated),
#       3 -> 0 (by task.cancelled), 0 -> 1.
#   (2) the owner's cancel: task 2 rejected again (1 -> 2), then cancelled by
#       the owner: 2 -> 0 (by task.cancelled).
#   (3) a run of two objectives: task 3 (objective 3) rejected (0 -> 1), task
#       4 (objective 4) rejected (1 -> 2); the owner cancels task 3: no
#       notice, the count stays 2; task 4 rejected again: 2 -> 3.
#   (4) a restart within a run: task 4 accepted (3 -> 0); task 5 (objective
#       5) rejected (0 -> 1); the loop is stopped and started; the owner
#       cancels task 5: 1 -> 0 (by task.cancelled).
# After each step, the last notice's `after` gate_rejections is the store's.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu


asserted=""
harness() {
    echo "053: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk sha1sum cut; do
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
SPAWN_DB_NAME="bb_053_runs_$run"
SPAWN_TMUX_SOCKET="bb-053-$run"
export SPAWN_TERMINAL_BACKEND=tmux
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
stop_loop() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
        loop_pid=""
    fi
}
cleanup() {
    stop_loop
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
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
            echo "053: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/serve.log" ]; then
            echo "053: serve's last lines:" >&2
            tail -n 5 "$WORK/serve.log" >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "053: the engines' turns (time, agent, execution, plan):" >&2
            cat "$WORK/turns.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "053: the tasks (id, mission, state, agent):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, mission_id, state, agent_id
                from mission_tasks order by id" >&2 2>/dev/null || true
            echo "053: the alerts, mission notices included (key, kind, subject, state, text):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, kind, subject, state, left(coalesce(text, ''), 120)
                from spawn_alerts where kind = 'mission_state' or not notice order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "053: harness: unexpected exit $rc" >&2
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

# An assertion exits 1, unless the database is gone: then nothing the
# scenario read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
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

# The fake engine. A prompt typed in (a paste, then Enter) opens a turn
# after a second of quiet. A prompt naming a plan (`BB:PLAN:<name>`, the
# task's brief carries it) acts out $BB_WORK/ctl/<name>.plan, one message
# per line, `<model> <input> <output> <cache write> <cache read> <age s>`:
# each message is an assistant message with its own id, written as two
# transcript lines (a text block, then another), each repeating the id, the
# model and the usage, timed <age s> seconds ago, as Claude Code writes one
# line per content block. Then the turn runs a tool call, writes the report
# and ends (the task done), logged to turns.log once its lines are written.
# A prompt naming no plan (the supervisor's, a nudge) ends the turn at once
# and reports no usage.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=${2-}; shift ;; esac
  shift
done
agent=$SPAWN_AGENT_ID
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
stamp() { date -u -d "@$(( $(date +%s) - $1 ))" +%Y-%m-%dT%H:%M:%S.000Z; }
line() { # <timestamp> <rest>
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$1" "$native" "$2" >>"$transcript"
}
message() { # <model> <in> <out> <cache write> <cache read> <age>
  id="msg_${SPAWN_EXECUTION_ID}_$count"
  at=$(stamp "$6")
  usage="\"usage\":{\"input_tokens\":$2,\"cache_creation_input_tokens\":$4,\"cache_read_input_tokens\":$5,\"output_tokens\":$3}"
  for block in first second; do
    line "$at" "\"type\":\"assistant\",\"message\":{\"id\":\"$id\",\"model\":\"$1\",\"role\":\"assistant\",\"type\":\"message\",\"content\":[{\"type\":\"text\",\"text\":\"$block block\"}],$usage}"
  done
}
turn() {
  name=$(printf '%s' "$typed_all" | sed -n 's/.*BB:PLAN:\([a-z0-9]*\).*/\1/p' | head -n 1)
  typed_all=
  plan=$BB_WORK/ctl/$name.plan
  if [ -z "$name" ] || [ ! -f "$plan" ]; then
    line "$(stamp 0)" '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    return
  fi
  while read -r model in out cw cr age; do
    [ -n "$model" ] || continue
    message "$model" "$in" "$out" "$cw" "$cr" "$age"
  done <"$plan"
  id="toolu_${SPAWN_EXECUTION_ID}_$count"
  line "$(stamp 0)" "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  printf 'done\n' >result.txt
  sleep 0.3
  line "$(stamp 0)" "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
  mkdir -p .spawn
  printf '## Outcome\ndone\n## What changed\nresult.txt\n## Open issues\nnone\n' >.spawn/report.md
  line "$(stamp 0)" '"type":"system","subtype":"turn_duration","durationMs":1'
  printf '%s %s %s %s\n' "$(date +%s%3N)" "$agent" "$SPAWN_EXECUTION_ID" "$name" >>"$BB_WORK/turns.log"
  printf '\nidle\n❯ '
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
typed_all=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    typed_all="$typed_all $typed"
    if [ "$pending" = 0 ]; then
      line "$(stamp 0)" '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
  elif [ "$pending" = 1 ]; then
    pending=0
    turn
  fi
done
ENGINE
chmod 755 "$WORK/claude"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alerts step reads its conditions every 2 s (default 30 s), so 10 s
# with no alert means several readings found none.
SPAWN_ALERT_CHECK_SECS=2
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_ALERT_CHECK_SECS \
    SPAWN_MISSION_FILE SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/turns.log"

# The test server is shared: another run's init-db can collide on cluster-wide
# objects ("tuple concurrently updated"). init-db is idempotent.
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
day=$(date -u +%F)
same_day() {
    [ "$(date -u +%F)" = "$day" ] || harness "the UTC day changed during the run ($day to $(date -u +%F)); run it again"
}

start_loop() {
    "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}

# wait_until <seconds> <command...>: true once the command is, false past
# the limit.
wait_until() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

note() { echo "053: $*" >&2; }

# --- the records -------------------------------------------------------------------

ctl=$WORK/ctl
mkdir -p "$ctl/scenarios"
B=$(printf 'bb-053-base' | sha1sum | cut -c1-40)
C1=$(printf 'bb-053-candidate' | sha1sum | cut -c1-40)

# record <JSON>: written as the integrator writes it; prints the event id.
record() {
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}
assigned() { # <task> <objective>
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":1,\"objective\":$2,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$1\",\"session_id\":\"session_worker_objective_$1\",\"scenario\":\"$ctl/scenarios/objective-$2\",\"brief\":\"Objective $2 of 053\"}" >/dev/null
}
reported() { record "{\"type\":\"task.reported\",\"id\":$1,\"commit\":\"$C1\"}" >/dev/null; }
rejected() { # <task> <revision>: prints the event id
    record "{\"type\":\"task.gated\",\"id\":$1,\"verdict\":\"rejected\",\"revision\":$2,\"base\":\"$B\",\"scenarios\":4,\"reason\":\"regression: the scenario fails on the candidate\"}"
}
accepted() { # <task> <revision>
    record "{\"type\":\"task.gated\",\"id\":$1,\"verdict\":\"accepted\",\"revision\":$2,\"base\":\"$B\",\"scenarios\":4,\"candidate\":\"$C1\"}"
}
cancel() { # <task>: the owner's cancel; prints its event id
    capture "$SPAWN_BIN" task cancel --id "$1"
    [ "$status" -eq 0 ] || harness "spawn task cancel --id $1 failed: $(err)"
    q "select max(id) from spawn_events where event_name = 'control.recorded'
         and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
         and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}

record '{"type":"mission.added","id":1,"text":"053 objectives","origin":"human","sources":[]}' >/dev/null
record "{\"type\":\"mission.configured\",\"id\":1,\"accept\":\"gate\",\"gate\":\"$ctl/scenarios\"}" >/dev/null

# The step has read every record up to <event id>: a notice fired on that record
# exists (each record waited on here moves a task, so each has its task notice).
seen() { [ "$(q "select count(*) from spawn_alerts where key = 'task_state:' || (select kind -> 'detail' -> 'record' ->> 'id' from spawn_events where id = $1) || ':$1'")" -gt 0 ]; }
read_up_to() { # <event id> <what>
    wait_until 20 seen "$1" ||
        fail "$2: 20 s after record $1 the loop's alert step has fired no task notice on it (alerts: $(q "select string_agg(key, ' ' order by fired_event_id) from spawn_alerts"))"
}
# The gate_rejections notices of mission 1 fired on records after <event id>,
# in record order: "a -> b (by record)".
runs_after() {
    q "select coalesce(string_agg(regexp_replace(text, '^mission #1: (.*)$', '\\1'), ' | '
                                  order by (evidence ->> 'event_id')::bigint), '')
         from spawn_alerts where kind = 'mission_state' and subject = 'mission #1'
          and text like '%gate_rejections%' and (evidence ->> 'event_id')::bigint > $1"
}
store_count() { q "select gate_rejections from mission_missions where id = 1"; }
last_after() {
    q "select coalesce((select evidence -> 'after' ->> 'gate_rejections' from spawn_alerts
         where kind = 'mission_state' and subject = 'mission #1' and text like '%gate_rejections%'
         order by (evidence ->> 'event_id')::bigint desc limit 1), 'none')"
}
agrees() { # <step>
    assert_eq "$(last_after)" "$(store_count)" "$1 the last gate_rejections notice's after (the store's count of rejections in a row)"
}

start_loop
sleep 4

# --- (1) the yield --------------------------------------------------------------------

mark=$(q "select coalesce(max(id), 0) from spawn_events")
assigned 1 1
reported 1
rejected 1 0 >/dev/null
reported 1
rejected 1 1 >/dev/null
reported 1
rejected 1 2 >/dev/null
yield=$(record '{"type":"task.cancelled","id":1,"yielded":true,"objective":1,"failures":3,"limit":3,"rejections":3,"reason":"3 gate rejections in a row, all of objective 1: task #1 yields its slot to the next objective instead of halting the mission","by":"spawn-integrate"}')
read_up_to "$yield" "(1) the yield"
assert_eq "$(store_count)" 0 "(1) the store's gate_rejections after task 1 yielded (harness check of the fold)"
assert_eq "$(runs_after "$mark")" "gate_rejections 0 -> 1 (by task.gated) | gate_rejections 1 -> 2 (by task.gated) | gate_rejections 2 -> 3 (by task.gated) | gate_rejections 3 -> 0 (by task.cancelled)" \
    "(1) mission 1's gate_rejections notices for three rejections of objective 1 and the yield that ends the run"
mark=$yield
assigned 2 2
reported 2
r=$(rejected 2 0)
read_up_to "$r" "(1) the next rejection"
assert_eq "$(runs_after "$mark")" "gate_rejections 0 -> 1 (by task.gated)" \
    "(1) the first rejection after the yield (the store's count is 1)"
agrees "(1)"
note "(1) the yield: 0 -> 1 -> 2 -> 3, 3 -> 0 by the cancel, then 0 -> 1"

# --- (2) the owner's cancel -------------------------------------------------------------

mark=$r
reported 2
rejected 2 1 >/dev/null
c=$(cancel 2)
read_up_to "$c" "(2) the owner's cancel"
assert_eq "$(runs_after "$mark")" "gate_rejections 1 -> 2 (by task.gated) | gate_rejections 2 -> 0 (by task.cancelled)" \
    "(2) task 2 rejected again, then cancelled by the owner (every rejection of the run was objective 2's)"
agrees "(2)"
note "(2) the owner's cancel of the run's objective: 2 -> 0"

# --- (3) a run of two objectives --------------------------------------------------------

mark=$c
assigned 3 3
assigned 4 4
reported 3
rejected 3 0 >/dev/null
reported 4
rejected 4 0 >/dev/null
c=$(cancel 3)
read_up_to "$c" "(3) the cancel of task 3"
assert_eq "$(store_count)" 2 "(3) the store's gate_rejections after the cancel of task 3 (harness check of the fold: a run of two objectives is not ended by either's cancel)"
assert_eq "$(runs_after "$mark")" "gate_rejections 0 -> 1 (by task.gated) | gate_rejections 1 -> 2 (by task.gated)" \
    "(3) objectives 3 and 4 rejected, then task 3 cancelled (the run was not objective 3's alone: no reset, no notice)"
mark=$c
reported 4
r=$(rejected 4 1)
read_up_to "$r" "(3) the rejection after the cancel"
assert_eq "$(runs_after "$mark")" "gate_rejections 2 -> 3 (by task.gated)" "(3) task 4 rejected again"
agrees "(3)"
note "(3) a run of two objectives: a cancel of one resets nothing; 2 -> 3"

# --- (4) a restart within a run ---------------------------------------------------------

mark=$r
reported 4
accepted 4 2 >/dev/null
assigned 5 5
reported 5
r=$(rejected 5 0)
read_up_to "$r" "(4) before the restart"
assert_eq "$(runs_after "$mark")" "gate_rejections 3 -> 0 (by task.gated) | gate_rejections 0 -> 1 (by task.gated)" \
    "(4) task 4 accepted, then task 5 (objective 5) rejected"
stop_loop
start_loop
sleep 4
mark=$r
c=$(cancel 5)
read_up_to "$c" "(4) the cancel after the restart"
assert_eq "$(runs_after "$mark")" "gate_rejections 1 -> 0 (by task.cancelled)" \
    "(4) after a loop restart, the owner cancels task 5 (every rejection of the run was objective 5's)"
agrees "(4)"
note "(4) after a restart within objective 5's run, its cancel: 1 -> 0"

echo "053: every change of a mission's gate_rejections fires its notice, the resets by a cancel that ends the run included, and the notices agree with the store, after a restart too"
exit 0
