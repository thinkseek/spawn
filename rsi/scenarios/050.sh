#!/bin/sh
# Black box: an alert clears only on evidence recorded after the record it
# fired on, whatever restarts the loop. Every kind: a record-borne alert (a
# halt, a stale backup, a failed drill, ...) clears only at a record later
# in the log than the one it fired on.
#
# Today a restarted loop's alert step reads the last 7 days of records again
# from the start, over the alerts it reads back open, and a record older
# than an open alert's own clears it. On the live fleet, mission #6's halt
# of 2026-11-10 10:35 UTC was cleared at 10:36 (the minute a deploy
# restarted the loop) on a mission.resumed of 11-05; it stayed halted 7 h
# with no open alert. Mission #4's halt of 11-08 12:04 was cleared at +70 min
# on a resume of 11-04; a backup_stale of 11-10 was cleared on a dump of
# 11-08. Required (the objective's prompt says each rule): a clear rests on
# a record recorded after the record the alert fired on (or a reading taken
# after it fired); an older one clears nothing, at a loop's first start or
# any later one.
#
# A real `spawn loop` runs with no tasks; its alert step reads every 2 s.
# The scenario writes control records into its own database as the fleet's
# store would hold them (straight into spawn_events, folded by
# spawn_control_refold(), as 043 seeds history), hours before now:
#   mission #1: added -5, halted "first" -3, resumed -2, halted "second" -1.
#   backup.dumped: D1 -5, D2 -3 (the newest: older than the 2 h a dump may
#     be, so backup_stale fires on it).
#   backup.verified: V1 ok -4, V2 failed -1.
# Checked:
#   (1) the loop's first start: mission_halted on "second", backup_stale on
#       D2 and drill_failed on V2 are open. The first halt is not open: its
#       resume (-2) is in the same history, so it never fired, or it fired
#       and was cleared naming that resume.
#   (2) the loop is stopped and started: 10 s on (five readings), the three
#       are still open, each still uncleared in the store. Then again, once
#       more.
#   (3) later evidence clears each, within 15 s, naming it: `spawn mission
#       resume --id 1` (a real resume) clears the halt; a dump D3 now clears
#       backup_stale; a drill V3 ok now clears drill_failed.
#   (4) a further restart fires none of them again and clears nothing else.
#   (5) while the loop is down: mission #1 halted ("third") and resumed, a
#       drill V4 failed and V5 ok. At the next start neither the halt's nor
#       V4's alert is open (never fired, or cleared naming the resume / V5).
#       A loop that crashed or was stopped while the integrator halted a
#       mission and the owner resumed it from the CLI meets this history.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (ea3a2e9f): the first halt stays open
# though its resume is in the history the start read (the start feeds every
# record before it records any alert, so the resume finds nothing open).
# With that alone fixed it fails at (2): after the restart the three alerts
# are cleared on the -2 resume, D1 and V1. It passes once an alert clears on
# a record after the one it fired on, and only on such a record, whether or
# not both are in what one start reads.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.

set -eu


asserted=""
harness() {
    echo "050: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk curl node; do
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
SPAWN_DB_NAME="bb_050_clears_$run"
SPAWN_TMUX_SOCKET="bb-050-$run"
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
            echo "050: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/serve.log" ]; then
            echo "050: serve's last lines:" >&2
            tail -n 5 "$WORK/serve.log" >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "050: the engines' turns (time, agent, execution, plan):" >&2
            cat "$WORK/turns.log" >&2
        fi
        if [ -s "$WORK/cost-7d.json" ]; then
            echo "050: the last 7 d answer (first 1500 bytes):" >&2
            head -c 1500 "$WORK/cost-7d.json" >&2
            echo >&2
        fi
        if [ -n "$database_made" ]; then
            echo "050: the tasks (id, mission, state, agent):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, mission_id, state, agent_id
                from mission_tasks order by id" >&2 2>/dev/null || true
            echo "050: the alerts (key, kind, subject, state, text):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, kind, subject, state, left(coalesce(text, ''), 120)
                from spawn_alerts where not notice order by fired_at_ms" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "050: harness: unexpected exit $rc" >&2
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

tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"

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
# US dollars per million tokens. bb-model-z is not priced.
PRICES='{"bb-model-x":{"input":3,"output":15,"cache_write":3.75,"cache_read":0.3},"bb-model-y":{"input":1,"output":5,"cache_write":1.25,"cache_read":0.1}}'
SPAWN_MODEL_PRICES=$PRICES
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
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_MODEL_PRICES SPAWN_ALERT_CHECK_SECS \
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

reported() { [ "$(q "select state from mission_tasks where id = $1")" = reported ]; }

# assign <label> <mission> <plan lines>: an ephemeral worker whose one turn
# reports that usage; sets task_<label>, agent_<label>.
assign() {
    printf '%s\n' "$3" >"$WORK/ctl/$1.plan"
    capture "$SPAWN_BIN" task assign --mission "$2" --kind ephemeral --worker "$1" \
        --brief "BB:PLAN:$1 Do the 041 task and report."
    [ "$status" -eq 0 ] || harness "task assign ($1) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $2")
    agent=$(q "select agent_id from mission_tasks where id = $task")
    [ -n "$task" ] && [ -n "$agent" ] || harness "task assign ($1) made no task with an agent"
    eval "task_$1=\$task agent_$1=\$agent"
}

# The page's API: GET <path> into <file>; its status code.
get() { bb_http "$2" --noproxy '*' "$base$1"; }
# cost <window> <check>: the answer for the window in $WORK/cost-<window>.json;
# an answer other than 200 with JSON is the behaviour's failure.
cost() {
    code=$(get "/api/cost?window=$1" "$WORK/cost-$1.json")
    [ "$code" = 200 ] || fail "$2 GET /api/cost?window=$1 answered $code: $(head -c 300 "$WORK/cost-$1.json" 2>/dev/null)"
    jq -e 'type == "object"' "$WORK/cost-$1.json" >/dev/null 2>&1 ||
        fail "$2 GET /api/cost?window=$1 answered no JSON object: $(head -c 300 "$WORK/cost-$1.json")"
}
# j <window> <jq filter>: a value of the last answer for the window.
j() { jq -r "$2" "$WORK/cost-$1.json"; }
# near <a> <b>: the two amounts differ by at most $0.000001.
near() { awk -v a="$1" -v b="$2" 'BEGIN { d = a - b; if (a == "" || a == "null") exit 1; exit !(d <= 0.000001 && d >= -0.000001) }'; }
assert_usd() { # <actual> <expected> <what>
    near "$1" "$2" || fail "$3: \$$1, expected \$$2"
}
# The usd of a list's entry whose <key> is <value>, in the window's answer.
usd_of() { j "$1" "[.$2[] | select((.$3 | tostring) == \"$4\") | .usd] | if length == 1 then .[0] else \"none\" end"; }
# A task's usd as known to the answer, without failing (for waiting).
task_usd() {
    code=$(get "/api/cost?window=7d" "$WORK/wait.json")
    [ "$code" = 200 ] || return 1
    jq -r "[.tasks[]? | select((.task | tostring) == \"$1\") | .usd][0] // \"none\"" "$WORK/wait.json" 2>/dev/null
}
costs() { near "$(task_usd "$1")" "$2"; }
alerts_for() { q "select count(*) from spawn_alerts where kind = 'cost_budget' and subject = 'mission #$1'"; }


note() { echo "050: $*" >&2; }

# --- the records --------------------------------------------------------------------

# rec <hours before now> <record JSON>: a control record as the store holds it.
rec_sql() {
    printf "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
      select 'agent_control', 'session_control', null, 'control', null,
             jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', '%s'::jsonb)),
             coalesce((select max(session_seq) from spawn_events where session_id = 'session_control'), 0) + 1,
             (floor(extract(epoch from clock_timestamp()) * 1000) + round(%s * 3600000))::bigint
      returning id;\n" "$2" "$1"
}
rec() { # <hours> <json>: prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "$(rec_sql "$1" "$2")" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $2 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}
refold() {
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_refold()" >/dev/null 2>"$WORK/rec.err" ||
        harness "spawn_control_refold failed: $(head -c 300 "$WORK/rec.err")"
}
dumped() { printf '{"type":"backup.dumped","keys":["postgres/hourly/bb-%s.dump.gpg"],"bytes":1000,"duration_ms":10,"dump_ms":5}' "$1"; }
verified() { # <name> <true|false>
    if [ "$2" = true ]; then
        printf '{"type":"backup.verified","ok":true,"key":"postgres/hourly/bb-%s.dump.gpg"}' "$1"
    else
        printf '{"type":"backup.verified","ok":false,"key":"postgres/hourly/bb-%s.dump.gpg","reason":"the bb drill failed"}' "$1"
    fi
}

rec -5 '{"type":"mission.added","id":1,"text":"050 mission","origin":"human","sources":[]}' >/dev/null
d1=$(rec -5 "$(dumped d1)")
v1=$(rec -4 "$(verified v1 true)")
h1=$(rec -3 '{"type":"mission.halted","id":1,"reason":"first"}')
d2=$(rec -3 "$(dumped d2)")
r1=$(rec -2 '{"type":"mission.resumed","id":1}')
h2=$(rec -1 '{"type":"mission.halted","id":1,"reason":"second"}')
v2=$(rec -1 "$(verified v2 false)")
refold
[ -n "$(q "select halted from mission_missions where id = 1")" ] ||
    harness "mission 1 did not fold as halted: $(q "select row_to_json(m) from mission_missions m where id = 1")"

# state <key>: open, cleared, notice or none.
state() { q "select coalesce((select state from spawn_alerts where key = '$1'), 'none')"; }
cleared_by() { q "select coalesce((select cleared_evidence ->> 'event_id' from spawn_alerts where key = '$1'), '')"; }
K_H1="mission_halted:1:$h1"
K_H2="mission_halted:1:$h2"
K_D2="backup_stale:dump:$d2"
K_V2="drill_failed:$v2"
opened() { [ "$(state "$K_H2")" = open ] && [ "$(state "$K_D2")" = open ] && [ "$(state "$K_V2")" = open ]; }
three() { echo "halt $(state "$K_H2"), backup_stale $(state "$K_D2"), drill_failed $(state "$K_V2")"; }
# settled <label> <key> <the later record>: an alert whose firing record and a
# later clearing record were both in what a start read is not open: never
# fired, or fired and cleared naming that later record.
settled() {
    st=$(state "$2")
    case "$st" in
    none) ;;
    cleared) assert_eq "$(cleared_by "$2")" "$3" "$1 the record $2 cleared on" ;;
    *) fail "$1 $2 is '$st' (cleared on '$(cleared_by "$2")'): its firing record and the later record $3 that clears it were both in what the loop's start read, so it must not be open" ;;
    esac
}

# --- (1) the first start ----------------------------------------------------------

start_loop
wait_until 30 opened ||
    fail "(1) 30 s after the loop's first start the alerts read [$(three)]: each should be open (the halt of $h1/$h2, the dump $d2 older than its 2 h, the failed drill $v2)"
settled "(1) first start:" "$K_H1" "$r1"
note "(1) first start: the second halt, the stale dump and the failed drill are open; the first halt, resumed at $r1, is not"

# --- (2) restarts --------------------------------------------------------------------

for round in 1 2; do
    stop_loop
    start_loop
    sleep 10
    opened ||
        fail "(2) 10 s after restart $round of the loop the alerts read [$(three)] (cleared on events $(cleared_by "$K_H2")/$(cleared_by "$K_D2")/$(cleared_by "$K_V2")): each must stay open; records older than the ones they fired on ($r1 before the halt $h2, $d1 before the dump $d2, $v1 before the drill $v2) clear nothing"
done
note "(2) two restarts: still open"

# --- (3) later evidence clears each -----------------------------------------------------

capture "$SPAWN_BIN" mission resume --id 1
[ "$status" -eq 0 ] || harness "spawn mission resume --id 1 failed: $(err)"
d3=$(rec 0 "$(dumped d3)")
v3=$(rec 0 "$(verified v3 true)")
gone() { [ "$(state "$K_H2")" = cleared ] && [ "$(state "$K_D2")" = cleared ] && [ "$(state "$K_V2")" = cleared ]; }
wait_until 15 gone ||
    fail "(3) 15 s after a resume, a new dump and a good drill, the alerts read [$(three)]: each clears on its later evidence"
resumed=$(q "select max(id) from spawn_events where event_name = 'control.recorded' and kind -> 'detail' -> 'record' ->> 'type' = 'mission.resumed'")
assert_eq "$(cleared_by "$K_H2")" "$resumed" "(3) the evidence the second halt's alert cleared on (the resume just made)"
assert_eq "$(cleared_by "$K_D2")" "$d3" "(3) the evidence backup_stale cleared on (the new dump)"
assert_eq "$(cleared_by "$K_V2")" "$v3" "(3) the evidence drill_failed cleared on (the good drill)"
note "(3) later evidence clears each"

# --- (4) no second fire --------------------------------------------------------------

before=$(q "select count(*) || ' ' || count(*) filter (where state = 'open') from spawn_alerts where kind in ('mission_halted', 'backup_stale', 'drill_failed')")
stop_loop
start_loop
sleep 10
assert_eq "$(q "select count(*) || ' ' || count(*) filter (where state = 'open') from spawn_alerts where kind in ('mission_halted', 'backup_stale', 'drill_failed')")" \
    "$before" "(4) the alerts of the three kinds (all, open) after a further restart"

note "(4) a further restart: nothing fired again, nothing else cleared"

# --- (5) a halt and its resume while the loop is down ---------------------------------

stop_loop
h3=$(rec 0 '{"type":"mission.halted","id":1,"reason":"third"}')
r3=$(rec 0 '{"type":"mission.resumed","id":1}')
v4=$(rec 0 "$(verified v4 false)")
v5=$(rec 0 "$(verified v5 true)")
refold
start_loop
sleep 10
settled "(5) a halt and its resume written while the loop was down:" "mission_halted:1:$h3" "$r3"
settled "(5) a failed drill and a good one written while the loop was down:" "drill_failed:$v4" "$v5"
note "(5) a halt resumed and a drill failed then passed while the loop was down: neither is open at the next start"

echo "050: an alert clears only on a record after the one it fired on, at a first start and after restarts, and when both an alert's record and its clearing record came while the loop was down; later evidence clears each"
exit 0
