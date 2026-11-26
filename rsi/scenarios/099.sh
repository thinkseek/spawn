#!/bin/sh
# Black box: the owner pauses a mission and resumes it. While it is paused
# nothing is assigned or delivered to it, its resident's engine is stopped
# with its branch and transcript kept, no "A mission has no work" alert is
# raised for it,
# the Scorecard does not count its paused time as idle, and Home and the
# Missions page say "paused". Resume undoes all of it, and the resident goes
# on in its own session.
#
# The problem (owner, 2026-11-12 19:20 and 20:00, sync/queue.md lead-mac
# 20:00, "Mission 6 pause"). The owner asked for mission 6 (Supply PA) to be
# paused. Spawn has no such state: a mission is active, closed (`spawn
# mission close`, final) or halted, which only the system sets (a raw push,
# a gate's rejections) and `spawn mission resume` clears. There is no verb
# for the owner to hold a mission, and no way to stop its resident without
# killing a process by hand. So mission 6 stays active with no objectives:
# its resident idles, Home raises "A mission has no work" for it, and the
# Scorecard counts its hours as idle.
#
# Required:
#   - `spawn mission pause --id N [--reason <text>]`, the owner's (a human's)
#     only: records `mission.paused` {id, reason}. It refuses (non-zero,
#     nothing recorded) a mission that is closed or already paused.
#   - While a mission is paused:
#       - no objective is assigned to it: spawn_objective_assign refuses,
#         naming the pause;
#       - nothing is delivered to its workers, and its resident is stopped
#         (a Stop, which takes a final checkpoint). The recovery does not
#         revive it. Its delivered task stays as it is. Kept: its branch
#         with the final checkpoint, and its engine session (the
#         transcript). Its workspace directory is removed at the stop, as
#         at any stop, and rebuilt from the branch at the resume: what the
#         checkpoint leaves out is lost, as at any stop;
#       - no `mission_idle` alert fires for it, and an open one clears at
#         the pause (its cleared evidence names the `mission.paused`
#         record);
#       - the Scorecard (`/api/scorecard/fleet`) mission row carries
#         `paused_hours` (its paused time in the window). That time is not
#         idle (`idle_hours` leaves it out), and a mission paused at the
#         window's end is never flagged `idle`;
#       - `/api/missions` gives it `paused` {at_ms, event_id, reason}
#         (null when not paused), and its state stays as it was;
#       - web/src: missionTimeline.ts laneState says "paused"; home.ts
#         missionsToday marks it `paused: true` (a row with work in flight,
#         or a quiet one with idle_ms null), quietText names it
#         "#<id> <name> (paused)", and noWorkText leaves it out of
#         "none for".
#   - `spawn mission resume --id N` on a paused mission records
#     `mission.unpaused` {id, pause_event}: assignment and delivery go on,
#     and the resident is resumed in its own native session on its
#     workspace rebuilt from its branch. (A halted mission's resume is
#     unchanged. On a mission both paused and halted, resume clears only
#     the pause: the halt stays, for a second resume.)
#
# A real `spawn loop` (the Bridge, the pusher, the dispatcher, the alert
# step; no supervisor, no model judge) and `spawn serve` run four missions,
# each taking its work from an objective backlog with a resident of its own:
#   - mission 1: its resident bbone gets a task (spawn_objective_assign, the
#     integrator's own path). Its first turn writes work.txt with a tool call
#     (a checkpoint), and then it idles at its prompt.
#   - mission 2 and mission 3: each gets a task that is then cancelled
#     (`spawn task cancel`), so each has no task in work. Mission 3 is paused
#     right after, before the idle bound.
#   - mission 4: closed.
# The fake engine writes its native transcript where Claude Code does and
# logs each start (new or resume) with its native session.
# SPAWN_ALERT_MISSION_IDLE_SECS=8, read every 2 s. Checked:
#   (1) the pause: `spawn mission pause --id 1` exits 0, and one
#       `mission.paused` record names mission 1 and the reason. /api/missions
#       gives mission 1 `paused` {at_ms, event_id, reason}, its state still
#       active. A second pause of mission 1, and a pause of the closed
#       mission 4, exit non-zero and record nothing.
#   (2) the resident: bbone's execution ends within 20 s of the pause (its
#       engine gone). Its work.txt is on its branch (the Stop's final
#       checkpoint; the workspace is made again from the branch when it is
#       resumed, (5)), and its transcript is still there. For 8 s more (over 20 loop
#       passes), no execution of bbone starts and its task stays delivered.
#   (3) no assignment: spawn_objective_assign on paused mission 3 refuses,
#       naming the pause, and makes no task.
#   (4) no idle alert: once mission 2 (idle, not paused) has its open
#       mission_idle alert (the barrier: a reading after the bound ran),
#       mission 3 has none. The Scorecard (24 h) flags mission 2 idle, and
#       not mission 3, whose row has paused_hours > 0 and idle_hours plus
#       paused_hours no more than its hours. Under node over /api/missions,
#       web/src says paused as required above (missions 1 and 3), and not
#       for mission 2. Then mission 2 is paused, and its alert clears within
#       10 s, its cleared evidence naming the pause's record.
#   (5) the resume: `spawn mission resume --id 1` exits 0, and one
#       `mission.unpaused` names mission 1 and its pause's event.
#       /api/missions gives `paused` null. bbone's engine starts again within
#       30 s, as a resume of its own native session, finding work.txt. Then
#       mission 3 is resumed, and spawn_objective_assign on it makes a task.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "099: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
SPAWN_HC=${SPAWN_HERDR_CORE_PROGRAM-}
if [ -z "$SPAWN_HC" ]; then
    spawn_dir=$(dirname "$SPAWN_BIN")
    if [ -x "$spawn_dir/spawn-hc" ]; then
        SPAWN_HC=$spawn_dir/spawn-hc
    elif [ -x "$(dirname "$spawn_dir")/spawn-hc" ]; then
        SPAWN_HC=$(dirname "$spawn_dir")/spawn-hc
    elif command -v spawn-hc >/dev/null 2>&1; then
        SPAWN_HC=$(command -v spawn-hc)
    fi
fi
[ -n "$SPAWN_HC" ] && [ -x "$SPAWN_HC" ] ||
    harness "spawn-hc is not installed (set SPAWN_HERDR_CORE_PROGRAM or build sibling spawn-hc beside SPAWN_BIN)"

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
[ -f "$tree/web/src/home.ts" ] || harness "no web/src/home.ts under $tree (run from the candidate's tree)"
for tool in psql git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and isolated XDG dirs, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/r
XDG_STATE_HOME=$WORK/state
XDG_CONFIG_HOME=$WORK/config
mkdir -m 700 "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
export XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
hc_id=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
SPAWN_DB_NAME="bb_099_runs_$run"
SPAWN_HERDR_SESSION="bb-099-$hc_id"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_HERDR_SESSION SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
hc() {
    env -u HERDR_SOCKET_PATH -u HERDR_SESSION -u HERDR_PANE_ID \
        HC_SESSION="$SPAWN_HERDR_SESSION" \
        HC_SOCKET="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock" \
        "$SPAWN_HC" "$@"
}
stop_herdr() {
    [ -n "${SPAWN_HERDR_SESSION-}" ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ] || return 0
    for addr in $(hc ls --json --label "spawn_owner=$SPAWN_HERDR_SESSION" 2>/dev/null |
        jq -r '.terminals[]? | select(.state == "live") | .address // empty' 2>/dev/null || true); do
        hc kill "$addr" >/dev/null 2>&1 || true
    done
    hc server stop >/dev/null 2>&1 || true
    hc_wait=0
    while [ "$hc_wait" -lt 20 ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ]; do
        hc_wait=$((hc_wait + 1))
        sleep 0.1
    done
}
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    stop_herdr
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
            echo "099: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engines.log" ]; then
            echo "099: the engines' starts (ms|resident|execution|start|pid|native|work.txt):" >&2
            cat "$WORK/engines.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "099: harness: unexpected exit $rc" >&2
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
# start: ms|resident|execution|new or resume|pid|native session|work.txt.
# bbone's first turn writes work.txt with a tool call (a checkpoint).
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
[ -n "${BB_PATH-}" ] && PATH=$BB_PATH && export PATH
native=
start=new
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=$2 start=resume ;;
  --session-id) native=$2 ;;
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
tool() { # <id> <command...>: a tool call around a command
  id="toolu_${SPAWN_EXECUTION_ID}_$1"
  shift
  line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  "$@"
  line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
}
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
work=absent
[ -f work.txt ] && work=found
printf '%s|%s|%s|%s|%s|%s|%s\n' "$(date +%s%3N)" "$name" "$SPAWN_EXECUTION_ID" "$start" "$$" "$native" "$work" \
  >>"$BB_WORK/engines.log"
write_work() { printf "bbone's work, written by %s\n" "$SPAWN_EXECUTION_ID" >work.txt; }
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
    if [ "$name" = bbone ] && [ ! -f work.txt ]; then
      tool work write_work
    fi
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_ENVELOPE SPAWN_SLICE \
    SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE SPAWN_HERDR_SHARDS SPAWN_HERDR_PROGRAM \
    SPAWN_TMUX_SOCKET SPAWN_TMUX_PROGRAM TMUX TMUX_PANE TMUX_TMPDIR \
    HERDR_SOCKET_PATH HERDR_SESSION HERDR_PANE_ID HC_SOCKET HC_SESSION \
    http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
BB_PATH=$PATH
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# A mission with no task in work this long is idle.
SPAWN_ALERT_MISSION_IDLE_SECS=8
# The recovery's resume backoff, short: a revived resident would show.
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
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK BB_PATH SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER \
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
note() { echo "099: $*" >&2; }
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
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}
# objective_assign <mission> <objective>: the integrator's own path; prints
# the task's id, or nothing (the refusal in $WORK/assign.err).
objective_assign() {
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 \
        -c "select spawn_objective_assign($1, $2, '$WORK/ctl/scenarios/$2.sh', 'Objective $2 of 099 mission $1', null)" \
        2>"$WORK/assign.err" | head -n 1 || true
}
task_state() { q "select state from mission_tasks where id = $1"; }
delivered() { [ "$(task_state "$1")" = delivered ]; }
cancel() { # <task>
    capture "$SPAWN_BIN" task cancel --id "$1"
    [ "$status" -eq 0 ] || harness "spawn task cancel --id $1 failed: $(err)"
}
records_of() { # <type> <mission id>: the control records, oldest first
    sql "select (kind -> 'detail' -> 'record')::text from spawn_events
          where event_name = 'control.recorded'
            and kind -> 'detail' -> 'record' ->> 'type' = '$1'
            and kind -> 'detail' -> 'record' ->> 'id' = '$2'
          order by id"
}
event_of() { # <type> <mission id>: the newest such record's event id
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
mission_json() { # <id>: the mission on /api/missions
    get /api/missions "$WORK/missions.json"
    jq -c --argjson id "$1" '[.missions[] | select(.id == $id)][0]' "$WORK/missions.json"
}
idle_of() { # <mission>: its mission_idle alerts, any state
    get /api/alerts "$WORK/alerts.json"
    jq -c --arg s "mission #$1" '[.[] | select(.kind == "mission_idle" and .subject == $s)]' "$WORK/alerts.json"
}
engine_lines() { awk -F'|' -v n="$1" '$2 == n' "$WORK/engines.log"; }
live_execution() {
    q "select execution_id from agent_executions where session_id = 'session_resident_$1'
        and ended_at_ms is null order by started_at_ms desc limit 1"
}
executions_of() { q "select count(*) from agent_executions where session_id = 'session_resident_$1'"; }
alive() { [ -n "$1" ] && kill -0 "$1" 2>/dev/null; }

# --- the missions ----------------------------------------------------------------

for name in bbone bbtwo bbthree bbfour; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 099 $name work."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
# (record() sets `id` for itself: the mission's number is `mission_no`.)
mission_no=0
for name in bbone bbtwo bbthree bbfour; do
    mission_no=$((mission_no + 1))
    record "{\"type\":\"mission.added\",\"id\":$mission_no,\"text\":\"099 mission $mission_no\",\"origin\":\"human\",\"sources\":[]}" >/dev/null
    record "{\"type\":\"mission.configured\",\"id\":$mission_no,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"$name\",\"workers\":1}}" >/dev/null
done
record '{"type":"mission.closed","id":4,"state":"done","summary":"099 closes it"}' >/dev/null
[ "$(q "select state from mission_missions where id = 4")" = done ] || harness "mission 4 did not close"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve

t1=$(objective_assign 1 1)
[ -n "$t1" ] || harness "spawn_objective_assign on mission 1 failed: $(head -c 300 "$WORK/assign.err")"
t2=$(objective_assign 2 1)
[ -n "$t2" ] || harness "spawn_objective_assign on mission 2 failed: $(head -c 300 "$WORK/assign.err")"
t3=$(objective_assign 3 1)
[ -n "$t3" ] || harness "spawn_objective_assign on mission 3 failed: $(head -c 300 "$WORK/assign.err")"
for task in "$t1" "$t2" "$t3"; do
    wait_until 60 delivered "$task" || harness "task #$task was not delivered within 60 s (it is $(task_state "$task"))"
done
# bbone's work is checkpointed on its branch before the pause. A mission
# with no source keeps its agents' branches in the workspace root's scratch
# repository.
branches=$SPAWN_WORKSPACE_ROOT/.sources/scratch.git
wait_until 60 eval '[ -n "$(git --git-dir="$branches" show refs/heads/agent_resident_bbone:work.txt 2>/dev/null)" ]' ||
    harness "bbone's work.txt did not reach its branch agent_resident_bbone within 60 s"
first=$(engine_lines bbone | head -n 1)
[ -n "$first" ] || harness "bbone's engine logged no start"
native=$(printf '%s' "$first" | cut -d'|' -f6)
engine_pid=$(printf '%s' "$first" | cut -d'|' -f5)
exec1=$(live_execution bbone)
[ -n "$exec1" ] || harness "bbone has no running execution before the pause"
cancel "$t2"
cancel "$t3"

# Mission 3 is paused before the idle bound (8 s) has passed.
capture "$SPAWN_BIN" mission pause --id 3 --reason "099: the owner holds mission 3"
p3_status=$status
p3_err=$(err)

# --- (1) the pause --------------------------------------------------------------

capture "$SPAWN_BIN" mission pause --id 1 --reason "099: the owner holds mission 1"
pause_ms=$(($(date +%s%N) / 1000000))
if [ "$status" -ne 0 ]; then
    check_failed "(1) spawn mission pause --id 1 exited $status: $(err | head -c 300)"
fi
[ "$p3_status" -eq 0 ] || check_failed "(1) spawn mission pause --id 3 exited $p3_status: $(printf '%s' "$p3_err" | head -c 300)"
got=$(records_of mission.paused 1 | jq -sc '[.[] | {id, reason}]')
expect_eq "$got" '[{"id":1,"reason":"099: the owner holds mission 1"}]' "(1) the mission.paused records of mission 1"
p1=$(event_of mission.paused 1)
got=$(mission_json 1)
expect_eq "$(printf '%s' "$got" | jq -c '{state, paused: (.paused | if . == null then null else {event_id, reason, at: (.at_ms | type)} end)}')" \
    "{\"state\":\"active\",\"paused\":{\"event_id\":${p1:-null},\"reason\":\"099: the owner holds mission 1\",\"at\":\"number\"}}" \
    "(1) /api/missions' mission 1: its state and its pause"
capture "$SPAWN_BIN" mission pause --id 1 --reason "again"
[ "$status" -ne 0 ] || check_failed "(1) a second spawn mission pause --id 1 exited 0: $(out | head -c 200)"
expect_eq "$(records_of mission.paused 1 | wc -l | tr -d ' ')" 1 "(1) mission.paused records of mission 1 after a second pause"
capture "$SPAWN_BIN" mission pause --id 4 --reason "closed"
[ "$status" -ne 0 ] || check_failed "(1) spawn mission pause --id 4 (closed) exited 0: $(out | head -c 200)"
expect_eq "$(records_of mission.paused 4 | wc -l | tr -d ' ')" 0 "(1) mission.paused records of the closed mission 4"
note "(1) checked: the pause"

# --- (2) the resident is stopped, branch and transcript kept--------------------------

if wait_until 20 eval '[ -z "$(live_execution bbone)" ] && ! alive "$engine_pid"'; then
    branch_work=$(git --git-dir="$branches" show refs/heads/agent_resident_bbone:work.txt 2>/dev/null || true)
    [ -n "$branch_work" ] || check_failed "(2) work.txt is no longer on bbone's branch agent_resident_bbone after the pause"
    transcript=$(find "$BB_PROJECTS" -name "$native.jsonl" 2>/dev/null | head -n 1)
    [ -n "$transcript" ] && [ -s "$transcript" ] ||
        check_failed "(2) bbone's native transcript $native.jsonl is gone or empty after the pause"
    before=$(executions_of bbone)
    sleep 8
    expect_eq "$(executions_of bbone)" "$before" "(2) bbone's executions 8 s after its stop (none may start while mission 1 is paused)"
    expect_eq "$(task_state "$t1")" delivered "(2) mission 1's task #$t1 while paused"
else
    check_failed "(2) bbone still runs 20 s after mission 1 was paused (execution $(live_execution bbone), engine pid $engine_pid alive: $(alive "$engine_pid" && echo yes || echo no))"
fi
note "(2) checked: the resident"

# --- (3) no assignment while paused ----------------------------------------------------

t3b=$(objective_assign 3 2)
if [ -n "$t3b" ]; then
    check_failed "(3) spawn_objective_assign on paused mission 3 made task #$t3b"
else
    grep -qi paused "$WORK/assign.err" ||
        check_failed "(3) spawn_objective_assign on paused mission 3 refused without naming the pause: $(head -c 300 "$WORK/assign.err")"
fi
note "(3) checked: no assignment"

# --- (4) no idle alert, the Scorecard, the page ----------------------------------------

open_two() { [ "$(idle_of 2 | jq -c '[.[] | .state]')" = '["open"]' ]; }
if wait_until 40 open_two; then
    got=$(idle_of 3)
    expect_eq "$(printf '%s' "$got" | jq length)" 0 \
        "(4) mission_idle alerts for mission 3, paused before the bound: $(printf '%s' "$got" | jq -c '[.[] | {key, state}]')"
    get "/api/scorecard/fleet?window=24h" "$WORK/scorecard.json"
    expect_eq "$(jq -c '[.missions[] | select(.mission == 2) | .idle]' "$WORK/scorecard.json")" '[true]' \
        "(4) the Scorecard's idle flag of mission 2 (idle, not paused)"
    got=$(jq -c '[.missions[] | select(.mission == 3)][0]' "$WORK/scorecard.json")
    expect_eq "$(printf '%s' "$got" | jq -c '{idle, paused: ((.paused_hours | type) == "number" and .paused_hours > 0), within: ((.idle_hours // 0) + (.paused_hours // 0) <= (.hours // 0) + 0.001)}')" \
        '{"idle":false,"paused":true,"within":true}' "(4) the Scorecard's row of paused mission 3: $got"
    cat >"$WORK/page.test.mjs" <<'EOF'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { laneState } = await import(pathToFileURL(`${env.BB_TREE}/web/src/missionTimeline.ts`).href);
const { missionsToday, quietText, noWorkText } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
// The answer as the page's adapter passes it on (missions.ts keeps `paused`;
// it imports the API client, which node alone does not load).
const missions = JSON.parse(readFileSync(env.BB_MISSIONS, "utf8"));
const byId = (id) => missions.missions.find((mission) => mission.id === id);

test("the Missions page's lanes", () => {
  assert.equal(laneState(byId(1)), "paused", "mission 1");
  assert.equal(laneState(byId(3)), "paused", "mission 3");
  assert.equal(laneState(byId(2)), "active", "mission 2");
});

test("Home's missions card", () => {
  const model = missionsToday(missions, Date.now());
  const row = model.rows.find((one) => one.id === 1);
  assert.ok(row, `mission 1 (a task in flight) is a row: ${JSON.stringify(model)}`);
  assert.equal(row.paused, true, "mission 1's row is paused");
  const three = model.quiet.find((one) => one.id === 3);
  assert.ok(three, `mission 3 is quiet: ${JSON.stringify(model.quiet)}`);
  assert.equal(three.paused, true, "mission 3 is paused");
  assert.equal(three.idle_ms, null, "a paused mission has no idle time");
  const two = model.quiet.find((one) => one.id === 2);
  assert.ok(two && two.paused !== true && typeof two.idle_ms === "number", `mission 2 is idle, not paused: ${JSON.stringify(two)}`);
  const quietLine = quietText(model.quiet);
  assert.ok(quietLine.includes(`#3 ${three.name} (paused)`), `quietText: ${quietLine}`);
  const none = noWorkText(model);
  assert.match(none, /#2 /, none);
  assert.doesNotMatch(none, /#3 /, none);
});
EOF
    get /api/missions "$WORK/missions.json"
    if ! BB_TREE=$tree BB_MISSIONS=$WORK/missions.json node --test "$WORK/page.test.mjs" >"$WORK/page.out" 2>&1; then
        check_failed "(4) web/src over /api/missions: $(grep -E 'not ok|AssertionError|Error|expected|actual' "$WORK/page.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
    fi
    capture "$SPAWN_BIN" mission pause --id 2 --reason "099: the owner holds mission 2"
    [ "$status" -eq 0 ] || check_failed "(4) spawn mission pause --id 2 exited $status: $(err | head -c 300)"
    p2=$(event_of mission.paused 2)
    cleared_two() { [ "$(idle_of 2 | jq -c '[.[] | .state]')" = '["cleared"]' ]; }
    wait_until 10 cleared_two || true
    got=$(idle_of 2)
    expect_eq "$(printf '%s' "$got" | jq -c '[.[] | [.state, .cleared_evidence.event_id]]')" "[[\"cleared\",${p2:-null}]]" \
        "(4) mission 2's mission_idle alert 10 s after its pause (record ${p2:-none})"
else
    check_failed "(4) mission 2 has had no task in work past the 8 s bound and has no open mission_idle alert 40 s on: $(idle_of 2 | jq -c '[.[] | {key, state}]'); (4) is not judged without it"
fi
note "(4) checked: no idle alert, the Scorecard, the page"

# --- (5) the resume ------------------------------------------------------------------------

starts_before=$(engine_lines bbone | wc -l | tr -d ' ')
capture "$SPAWN_BIN" mission resume --id 1
[ "$status" -eq 0 ] || check_failed "(5) spawn mission resume --id 1 exited $status: $(err | head -c 300)"
got=$(records_of mission.unpaused 1 | jq -sc '[.[] | {id, pause_event}]')
expect_eq "$got" "[{\"id\":1,\"pause_event\":${p1:-null}}]" "(5) the mission.unpaused records of mission 1"
expect_eq "$(mission_json 1 | jq -c .paused)" null "(5) /api/missions' mission 1 pause after the resume"
if wait_until 30 eval '[ "$(engine_lines bbone | wc -l | tr -d " ")" -gt "$starts_before" ]'; then
    again=$(engine_lines bbone | tail -n 1)
    expect_eq "$(printf '%s' "$again" | cut -d'|' -f4,6,7)" "resume|$native|found" \
        "(5) bbone's engine after the resume: how it started, its native session and work.txt"
else
    check_failed "(5) bbone's engine did not start again within 30 s of mission 1's resume"
fi
capture "$SPAWN_BIN" mission resume --id 3
[ "$status" -eq 0 ] || check_failed "(5) spawn mission resume --id 3 exited $status: $(err | head -c 300)"
t3c=$(objective_assign 3 2)
[ -n "$t3c" ] || check_failed "(5) spawn_objective_assign on mission 3, resumed, made no task: $(head -c 300 "$WORK/assign.err")"
note "(5) checked: the resume"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "099: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "099: a paused mission gets no work, its resident stopped with its branch and transcript kept, no idle alert, no idle hours, and reads paused; its resume goes on in the same session"
exit 0
