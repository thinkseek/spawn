#!/bin/sh
# Objective 138 [observability]: an open message_not_taken alert clears once
# a later message of its session is taken (the agent took input after it).
# Before, it cleared only when that message itself was taken or its session
# ended. A resident agent's session never ends, so one missed marker left the
# alert open for good (agent_resident_flake #1925, open hours after the
# agent took later work). A message whose later one is taken before it falls
# due never fires.
#
# Black box, 045's world: a real `spawn loop` (the Bridge, the dispatcher,
# the alerts step) runs two ephemeral workers on 045's fake engine, its mode
# per agent (ctl/<agent>.mode): `take` writes what is typed into its
# transcript and ends the turn; `deaf` writes nothing of it: never seen
# taken, its transcript unmoved; `hold` (138's own) is `deaf` with a busy
# screen until the scenario releases it, so the Bridge, which types the
# next input only after a ready screen read since the last typing, can
# type a later one (after `deaf` it never does). Since PR #88 (ace244ad0,
# issue #66 Part 1) a transcript line read after a typed message's typing
# suppresses or clears its message_not_taken, so each untaken input here
# is `hold` or `deaf` (its `read_since_typed` stays 0), typed only once the
# previous turn's end is recorded. Inputs come from a person (`spawn submit`).
# SPAWN_MESSAGE_TAKEN_SECS=12, SPAWN_ALERT_CHECK_SECS=1. Checked:
#   (1) late, hold: input A is typed, never seen taken, and its alert
#       opens. late takes again; input B is taken. A's alert clears, its
#       clear's evidence naming B (`later_taken`, which outranks
#       `read_since_typed`).
#   (2) late, deaf again: input C after B fires and is still open 10 s
#       after it opened: nothing later was taken.
#   (3) quick, hold: input D is typed; quick takes again before D's bound
#       and input E is taken. D never has an alert (24 s on), and late's C
#       is still open: another session's taking counts for nothing.
# Exit: 0 pass, 1 a check failed, 2 harness.
set -eu

asserted=""
harness() {
    echo "138: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed awk curl node python3; do
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "138"
SPAWN_DB_NAME="bb_138_later_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
wrapper_pid=""
serve_pid=""
stop_loop() {
    [ -n "$wrapper_pid" ] || return 0
    : >"$WORK/loop.stop"
    child=$(cat "$WORK/loop.pid" 2>/dev/null || true)
    if [ -n "$child" ]; then kill -TERM "$child" 2>/dev/null || true; fi
    i=0
    while kill -0 "$wrapper_pid" 2>/dev/null; do
        i=$((i + 1))
        if [ "$i" -eq 150 ] && [ -n "$child" ]; then kill -KILL "$child" 2>/dev/null || true; fi
        if [ "$i" -ge 200 ]; then
            kill -KILL "$wrapper_pid" 2>/dev/null || true
            break
        fi
        sleep 0.2
    done
    wait "$wrapper_pid" 2>/dev/null || true
    wrapper_pid=""
}
stop_serve() {
    [ -n "$serve_pid" ] || return 0
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
cleanup() {
    stop_serve
    stop_loop
    herdr_cleanup
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
    set +e
    if [ "$rc" -ne 0 ]; then
        for log in loop serve; do
            if [ -s "$WORK/$log.log" ]; then
                echo "138: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "138: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "138: harness: unexpected exit $rc" >&2
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
note() { echo "138: $*"; }

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
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$repo"
git -C "$WORK/seed" push -q "$repo" main:main

mkdir -p "$WORK/claude-config" "$WORK/ctl" "$WORK/state"

# The fake engine: a prompt; what is typed into it is one message, written
# after a second of quiet to its session transcript as Claude Code writes a
# prompt (one user line with the whole text), unless its mode (ctl/<agent>.
# mode) is `deaf`; `twice` writes the line twice. Then its turn: one tool
# call, and the turn's end (`turn_duration`) 3 s after the prompt. Every
# prompt and turn end is logged (engine.log: ms agent what).
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in
  --resume | --session-id) native=${2-}; shift ;;
  esac
  shift
done
agent=$SPAWN_AGENT_ID
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$$" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
log() { printf '%s %s %s\n' "$(date +%s%3N)" "$agent" "$1" >>"$BB_WORK/engine.log"; }
mode() { cat "$BB_WORK/ctl/$agent.mode" 2>/dev/null || echo take; }
turn() {
  id="toolu_${SPAWN_EXECUTION_ID}_$$_$count"
  line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  sleep 0.5
  line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
  sleep 2.5
  line '"type":"system","subtype":"turn_duration","durationMs":3000'
  log "turn-ended"
  # The spinner is drawn over in place: the screen reader scans the lines
  # above the prompt, and one left there reads busy.
  printf '\r\033[2Kdone\n\n❯ '
}
printf 'Welcome to Claude Code\n\n❯ '
buffer=""
while :; do
  if bb_read_line typed; then
    buffer="${buffer:+$buffer
}$typed"
  elif [ -n "$buffer" ]; then
    text=$(printf '%s' "$buffer" | jq -Rs .)
    buffer=""
    case "$(mode)" in
    hold)
      # Shows a turn running but writes nothing of it to the transcript
      # until released (ctl/<agent>.release): never seen taken, transcript
      # unmoved. Its busy-then-ready screen is what lets the Bridge type the
      # next input (bridge.rs SessionView::answered); after `deaf` it never
      # would.
      # The spinner sits above an empty prompt, where the screen reader
      # looks (screen.rs read_claude_screen): busy. The release erases both.
      log "hold"
      printf '\n✻ Working… (esc to interrupt)\n❯ '
      until [ -e "$BB_WORK/ctl/$agent.release" ]; do sleep 0.2; done
      rm -f "$BB_WORK/ctl/$agent.release"
      log "released"
      printf '\r\033[2K\033[1A\r\033[2Kdone\n\n❯ '
      continue
      ;;
    deaf)
      log "deaf"
      printf '\n❯ '
      continue
      ;;
    twice)
      line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$text}"
      line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$text}"
      log "prompt-twice"
      ;;
    *)
      line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$text}"
      log "prompt"
      ;;
    esac
    printf '\n✻ Working… (esc to interrupt)'
    turn
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# herdr-core proxy: while `refuse` names a terminal, staging for that
# terminal fails before typing (`agent_not_ready`).
herdr_use_fault_proxy "$WORK" --refuse-stage-file "$WORK/refuse"

# `spawn loop` as systemd runs spawn-loop.service: started again 5 s after
# it exits (Restart=always, RestartSec=5).
cat >"$WORK/run-loop" <<'EOF'
#!/bin/sh
while [ ! -f "$WORK/loop.stop" ]; do
    "$SPAWN_BIN" loop --repo "$BB_REPO" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    child=$!
    printf '%s\n' "$child" >"$WORK/loop.pid"
    status=0
    wait "$child" || status=$?
    [ ! -f "$WORK/loop.stop" ] || exit 0
    sleep 5
done
EOF

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_GATE SPAWN_DISPATCH \
    SPAWN_SUPERVISOR SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_RELAY http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
BB_REPO=$repo
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
CLAUDE_CONFIG_DIR=$WORK/claude-config
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_WORKER=worker-bb
SPAWN_MESSAGE_TAKEN_SECS=12
SPAWN_ALERT_CHECK_SECS=1
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL BB_REPO SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS CLAUDE_CONFIG_DIR BB_PROJECTS BB_WORK SPAWN_WORKER \
    SPAWN_MESSAGE_TAKEN_SECS SPAWN_ALERT_CHECK_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$WORK/engine.log"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

start_loop() {
    rm -f "$WORK/loop.stop" "$WORK/loop.pid"
    sh "$WORK/run-loop" &
    wrapper_pid=$!
    i=0
    until [ -s "$WORK/loop.pid" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "the loop did not start"
        sleep 0.1
    done
}
web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
# A restart writes the same log: it is emptied here, in this shell, before
# the launch, and the launch appends. With `serve >"$WORK/serve.log" &` the
# truncation ran in the child, after the fork, so this shell could read the
# stopped serve's port line first and send every request to a dead port.
start_serve() {
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
    base="http://127.0.0.1:$port"
    # This launch answers on the port it named.
    ready=$(curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' "$base/" 2>"$WORK/ready.err") || true
    [ "$ready" = 200 ] || harness "serve (pid $serve_pid) named port $port and GET / there answered [$ready]: $(head -c 300 "$WORK/ready.err")"
}

# --- helpers ----------------------------------------------------------------

now_ms() { date +%s%3N; }
q() { scalar "$1"; }
put() { printf '%s\n' "$3" >"$WORK/state/$1.$2"; }
get() { cat "$WORK/state/$1.$2" 2>/dev/null || true; }
# api <path> <file>: GET from serve; its status code.
api() { bb_http "$2" --noproxy '*' "$base$1"; }
# commands <label>: the session's commands into $WORK/commands-<label>.json.
commands() {
    code=$(api "/api/fleet/$(get "$1" session)/commands?limit=100" "$WORK/commands-$1.json")
    [ "$code" = 200 ] || harness "GET /api/fleet/$(get "$1" session)/commands answered $code"
    jq -e 'type == "array"' "$WORK/commands-$1.json" >/dev/null 2>&1 ||
        harness "the commands of $1 are no JSON array: $(head -c 200 "$WORK/commands-$1.json")"
}
# m <label> <command id> <jq filter on the message>: from the last commands read.
m() { jq -r --argjson id "$2" "[.[] | select(.command_id == \$id)][0] | $3" "$WORK/commands-$1.json"; }
# send <label> <key> <text>: a person's input; prints its command id.
send() {
    capture "$SPAWN_BIN" submit --principal bb-person --agent "$(get "$1" agent)" \
        --session "$(get "$1" session)" --key "$2" \
        --action "$(jq -cn --arg text "$3" '{type: "input", text: $text}')"
    [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
    id=$(q "select id from spawn_commands where idempotency_key = '$2'")
    [ -n "$id" ] || harness "submit $2 recorded no command"
    echo "$id"
}
# until_message <label> <id> <jq condition> <seconds>: poll until the
# message meets it; 0 when it did.
until_message() {
    deadline=$(($(date +%s) + $4))
    while :; do
        commands "$1"
        [ "$(m "$1" "$2" "(.message // {}) | ($3)")" = true ] && return 0
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
logged() { # <label> <what>: how many times its engine logged that
    awk -v a="$(get "$1" agent)" -v w="$2" '$2 == a && $3 == w' "$WORK/engine.log" | wc -l | tr -d ' '
}
# settled <label> <seconds>: its engine is idle and every turn end it wrote
# is recorded (turn.stopped from the transcript), so an input typed next has
# no older transcript line read after its typing; 0 when so in time.
settled() {
    deadline=$(($(date +%s) + $2))
    until idle "$1" &&
        [ "$(q "select count(*) from spawn_events where session_id = '$(get "$1" session)' and source = 'claude' and event_name = 'turn.stopped'")" -ge "$(logged "$1" turn-ended)" ]; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.2
    done
}
# untaken <label> <deaf|hold> <key> <text> <step>: switch the agent to that
# mode, send, and wait until its engine logged the input so; prints its id.
untaken() {
    settled "$1" 30 || harness "($5) $1's last turn end was not recorded within 30 s"
    printf '%s\n' "$2" >"$WORK/ctl/$(get "$1" agent).mode"
    seen=$(logged "$1" "$2")
    id=$(send "$1" "$3" "$4")
    deadline=$(($(date +%s) + 20))
    until [ "$(logged "$1" "$2")" -gt "$seen" ]; do
        [ "$(date +%s)" -lt "$deadline" ] || harness "($5) $1's engine did not read input $id ($2) within 20 s"
        sleep 0.2
    done
    echo "$id"
}
# release <label> <held id> <step>: once the Bridge recorded the held turn's
# busy screen (after that input's typing), end the hold: its ready screen
# then lets the next input be typed.
release() {
    deadline=$(($(date +%s) + 30))
    until [ "$(q "select count(*) from spawn_events where session_id = '$(get "$1" session)' and event_name = 'screen.observed' and kind -> 'detail' ->> 'state' = 'busy' and id > (select min(id) from spawn_events where session_id = '$(get "$1" session)' and source = 'bridge' and source_event_id = 'cmd_$2:typed')")" -ge 1 ]; do
        [ "$(date +%s)" -lt "$deadline" ] || harness "($3) no busy screen of $1 was recorded after input $2's typing within 30 s"
        sleep 0.2
    done
    : >"$WORK/ctl/$(get "$1" agent).release"
}
idle() { # the agent's engine ended a turn and has had none since
    awk -v a="$(get "$1" agent)" '$2 == a { last = $3 } END { exit !(last == "turn-ended" || last == "deaf" || last == "released") }' "$WORK/engine.log"
}
terminal_of() { echo "spawn-$(q "select execution_id from agent_executions where session_id = '$(get "$1" session)' and ended_at_ms is null order by started_at_ms desc limit 1")"; }
alerts() { # <command id>: its message_not_taken alerts, `state` a line each
    code=$(api /api/alerts "$WORK/alerts.json")
    [ "$code" = 200 ] || harness "GET /api/alerts answered $code"
    jq -r --arg id "$1" '[.. | objects | select(.kind? == "message_not_taken")
        | select((.evidence.command // .evidence.command_id // "" | tostring) == $id or (.key // "" | endswith(":" + $id)))]
        | unique_by(.key) | .[] | .state' "$WORK/alerts.json"
}

old_quiet() { # <when>: the seeded older inputs are no untaken messages
    commands take
    for id in $old_ids; do
        [ "$(m take "$id" '.message | .state == "typed" and .not_taken == false and .taken_at_ms == null')" = true ] ||
            fail "(6) $1: input $id, typed by an older Spawn with no marker, reads $(m take "$id" '.message | tostring'); typed, not not_taken"
        [ -z "$(alerts "$id")" ] ||
            fail "(6) $1: input $id, typed by an older Spawn with no marker, has message_not_taken alerts [$(alerts "$id" | tr '\n' ' ')]"
    done
}

# --- the agents ----------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "138 messages"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
labels="late quick"
for label in $labels; do
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
        --brief "Do the 138 task: answer each message."
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    put "$label" task "$task"
    put "$label" agent "$(q "select agent_id from mission_tasks where id = $task")"
    put "$label" session "$(q "select session_id from mission_tasks where id = $task")"
done

start_loop
start_serve

deadline=$(($(date +%s) + 120))
for label in $labels; do
    until idle "$label"; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            harness "$label's engine did not take its task and end its turn within 120 s"
        sleep 0.5
    done
done
note "every worker took its task"

# --- (1) a later message taken clears the earlier one's alert --------------------------

a_id=$(untaken late hold bb-late-a "BB:PING late A" 1)
deadline=$(($(date +%s) + 40))
until [ "$(alerts "$a_id" | tr '\n' ' ')" = "open " ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
[ "$(alerts "$a_id" | tr '\n' ' ')" = "open " ] ||
    fail "(1) input A to late, typed and never taken, has alerts [$(alerts "$a_id" | tr '\n' ' ')], not one open within 40 s; its records: $(q "select left(string_agg((kind -> 'detail' -> 'record' ->> 'type') || ' ' || (kind -> 'detail' -> 'record' -> 'evidence')::text, ' | '), 600) from spawn_events where kind -> 'detail' -> 'record' ->> 'key' = 'message_not_taken:$a_id'")"
printf 'take\n' >"$WORK/ctl/$(get late agent).mode"
b_id=$(send late bb-late-b "BB:PING late B")
release late "$a_id" 1
until_message late "$b_id" '.taken >= 1' 40 ||
    harness "(1) input B to late was not taken within 40 s: $(m late "$b_id" '.message | tostring')"
deadline=$(($(date +%s) + 20))
until [ "$(alerts "$a_id" | tr '\n' ' ')" = "cleared " ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
[ "$(alerts "$a_id" | tr '\n' ' ')" = "cleared " ] ||
    fail "(1) input B to late was taken after input A, and A's message_not_taken alert is [$(alerts "$a_id" | tr '\n' ' ')], not cleared within 20 s"
named=$(q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'key' = 'message_not_taken:$a_id' and kind -> 'detail' -> 'record' ->> 'type' = 'alert.cleared' and kind -> 'detail' -> 'record' -> 'evidence' ->> 'later_taken' = '$b_id'")
[ "$named" -ge 1 ] ||
    fail "(1) A's alert cleared, and its clear's evidence does not name B ($b_id) as later_taken: $(q "select left(string_agg((kind -> 'detail' -> 'record')::text, ' | '), 600) from spawn_events where kind -> 'detail' -> 'record' ->> 'key' = 'message_not_taken:$a_id' and kind -> 'detail' -> 'record' ->> 'type' = 'alert.cleared'")"
note "(1) a later message taken cleared A's alert, naming it"

# --- (2) one after the taken one, never taken, stays open ------------------------------

c_id=$(untaken late deaf bb-late-c "BB:PING late C" 2)
deadline=$(($(date +%s) + 40))
until [ "$(alerts "$c_id" | tr '\n' ' ')" = "open " ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
[ "$(alerts "$c_id" | tr '\n' ' ')" = "open " ] ||
    fail "(2) input C to late, after B and never taken, has alerts [$(alerts "$c_id" | tr '\n' ' ')], not one open within 40 s"
sleep 10
[ "$(alerts "$c_id" | tr '\n' ' ')" = "open " ] ||
    fail "(2) input C's alert is [$(alerts "$c_id" | tr '\n' ' ')] 10 s on, with nothing taken after C; still open"
note "(2) C, with nothing taken after it, fired and stays open"

# --- (3) a later message taken before the bound: no alert ------------------------------

# The engine reads its mode as it takes the line: untaken returns only
# once it has read D (held), so E, sent next, meets `take`.
d_id=$(untaken quick hold bb-quick-d "BB:PING quick D" 3)
until_message quick "$d_id" '.typed_at_ms != null' 20 ||
    harness "(3) input D to quick was not typed within 20 s: $(m quick "$d_id" '.message | tostring')"
printf 'take\n' >"$WORK/ctl/$(get quick agent).mode"
e_id=$(send quick bb-quick-e "BB:PING quick E")
release quick "$d_id" 3
until_message quick "$e_id" '.taken >= 1' 20 ||
    harness "(3) input E to quick was not taken within 20 s: $(m quick "$e_id" '.message | tostring')"
[ "$(m quick "$d_id" '.message.taken')" = 0 ] ||
    harness "(3) D's marker was seen taken, so D does not stand for a missed one: $(m quick "$d_id" '.message | tostring')"
[ "$(m quick "$d_id" '.message.not_taken')" = false ] ||
    harness "(3) D was already past its bound when E was taken (load?): $(m quick "$d_id" '.message | tostring')"
sleep 24
# Any alert ever fired for D, from the records (an open one, or one fired
# and cleared since, which the alerts page may no longer list).
fired=$(q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'key' = 'message_not_taken:$d_id' and kind -> 'detail' -> 'record' ->> 'type' = 'alert.fired'")
[ -z "$(alerts "$d_id")" ] && [ "$fired" = 0 ] ||
    fail "(3) input D, whose later input E was taken before D's bound, has alerts [$(alerts "$d_id" | tr '\n' ' ')] and $fired fired; none"
note "(3) D, with E taken before its bound, never fired"
# Another session's taking counts for nothing: late's C is still open after
# quick took D and E.
[ "$(alerts "$c_id" | tr '\n' ' ')" = "open " ] ||
    fail "(3) late's C is [$(alerts "$c_id" | tr '\n' ' ')] after quick took E; another session's taking must leave it open"

echo "138: an open message_not_taken clears once a later message of its session is taken"
exit 0
