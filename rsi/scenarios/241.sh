#!/bin/sh
# Objective 241 [observability] (from LOOK 4082, alert message_not_taken:4082
# to agent_supervisor): after a restart the Bridge follows each running
# engine's transcript again from its execution's start, and read at most one
# 8 MiB read (READ_LIMIT) per change of the file. A long-idle transcript (the
# supervisor's: 335 MB past its execution's start) never caught up: its next
# turn changed the file a few times, the Bridge read a few 8 MiB of old lines,
# and the turn's own lines were never read. The message read as not taken,
# and the turn, its usage and its tools were missing from the log.
#
# Black box, 214's world: a real `spawn loop` (the Bridge, the dispatcher,
# the alerts step) runs one ephemeral worker on 045's fake engine (take mode:
# what is typed is written as a user line, then a turn). Once it has taken
# its task and is idle, BB241_BULK_MIB (64) MiB of lines that carry no event
# are appended to its transcript in one write (a long transcript's bulk), the
# loop is restarted, and the engine stays idle. Then a person's input.
# SPAWN_MESSAGE_TAKEN_SECS=12, SPAWN_ALERT_CHECK_SECS=1. Checked (through
# GET /api/fleet/<session>/commands, GET /api/alerts and the records):
#   (1) the input is taken (taken >= 1) within 40 s of being typed, and 20 s
#       after its bound no message_not_taken alert was ever fired for it;
#   (2) its turn is logged: a turn.stopped of the worker's session read from
#       its transcript (source claude) after the input was typed;
#   (3) the task's lines read before the restart are not logged twice: the
#       session's turn.stopped and message.taken counts from before the input
#       are what they were before the restart.
# Exit: 0 pass, 1 a check failed, 2 harness.
#
# Fails with 1 on live main 9e1a8d22: in (1) the input is never taken and its
# alert opens.
set -eu

asserted=""
harness() {
    echo "241: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_tmux=$(command -v tmux)

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
SPAWN_DB_NAME="bb_241_backlog_$run"
SPAWN_TMUX_SOCKET="bb-241-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

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
    "$real_tmux" -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
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
        for log in loop serve; do
            if [ -s "$WORK/$log.log" ]; then
                echo "241: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "241: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "241: harness: unexpected exit $rc" >&2
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
note() { echo "241: $*"; }

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
    buffer_was=$buffer
    text=$(printf '%s' "$buffer" | jq -Rs .)
    bare=$(printf '%s' "$buffer" | sed 's/\[spawn message #[0-9]*\]//g' | jq -Rs .)
    buffer=""
    case "$(mode)" in
    queued)
      # Given mid-turn (214): the engine writes the input as a queued_command
      # attachment alone, with its marker, and no user line; the agent works on.
      line "\"type\":\"attachment\",\"attachment\":{\"type\":\"queued_command\",\"prompt\":$text,\"commandMode\":\"prompt\",\"origin\":{\"kind\":\"human\"},\"humanTurn\":true}"
      log "prompt-queued"
      ;;
    notify)
      # The input reaches the transcript only quoted inside a job's
      # notification (the engine's own text, commandMode task-notification).
      quoted=$(printf '<task-notification>\n<task-id>bb214job</task-id>\n<status>completed</status>\n<summary>printed %s</summary>\n</task-notification>' "$buffer_was" | jq -Rs .)
      line "\"type\":\"attachment\",\"attachment\":{\"type\":\"queued_command\",\"prompt\":$quoted,\"commandMode\":\"task-notification\",\"origin\":{\"kind\":\"task-notification\",\"producer\":\"session-task\"}}"
      log "prompt-notify"
      ;;
    nomark)
      # Takes it and works its turn, but its line lacks the marker (138):
      # the message is taken, its marker missed.
      line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$bare}"
      log "prompt-nomark"
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

# Spawn's tmux, on the private socket. While `refuse` names a terminal, the
# load of its paste buffer (`spawn-<terminal>`) fails: nothing is typed.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
if [ -s '$WORK/refuse' ]; then
    held=\$(cat '$WORK/refuse')
    load=""
    for arg in "\$@"; do
        [ "\$arg" = load-buffer ] && load=1
        if [ -n "\$load" ] && [ "\$arg" = "spawn-\$held" ]; then
            echo "bb refuse: the pane of \$held does not take a paste" >&2
            exit 1
        fi
    done
fi
exec '$real_tmux' -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

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
SPAWN_TMUX_PROGRAM=$WORK/tmux
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
    SPAWN_CLAUDE_PROJECTS CLAUDE_CONFIG_DIR BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
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
idle() { # the agent's engine ended a turn and has had none since
    awk -v a="$(get "$1" agent)" '$2 == a { last = $3 } END { exit !(last == "turn-ended" || last == "deaf") }' "$WORK/engine.log"
}
terminal_of() { echo "spawn-$(q "select execution_id from agent_executions where session_id = '$(get "$1" session)' and ended_at_ms is null order by started_at_ms desc limit 1")"; }
alerts() { # <command id>: its message_not_taken alerts, `state` a line each
    code=$(api /api/alerts "$WORK/alerts.json")
    [ "$code" = 200 ] || harness "GET /api/alerts answered $code"
    jq -r --arg id "$1" '[.. | objects | select(.kind? == "message_not_taken")
        | select((.evidence.command // .evidence.command_id // "" | tostring) == $id or (.key // "" | endswith(":" + $id)))]
        | unique_by(.key) | .[] | .state' "$WORK/alerts.json"
}


# --- the agent ----------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "241 a long transcript after a restart"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker busy \
    --brief "Do the 241 task: answer each message."
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(q "select max(id) from mission_tasks where mission_id = $mission")
put busy task "$task"
put busy agent "$(q "select agent_id from mission_tasks where id = $task")"
put busy session "$(q "select session_id from mission_tasks where id = $task")"

start_loop
start_serve

deadline=$(($(date +%s) + 120))
until idle busy; do
    [ "$(date +%s)" -lt "$deadline" ] ||
        harness "the worker's engine did not take its task and end its turn within 120 s"
    sleep 0.5
done
# Its task's marker was seen: the session tracks its messages from here on.
# The engine's turn end is in its own log at once, but the loop records
# message.taken only when it next ingests the transcript: wait for it,
# bounded, rather than read once (rev-k 00:09: 1 of 7 runs read 0 here).
deadline=$(($(date +%s) + 30))
until [ "$(q "select count(*) from spawn_events where session_id = '$(get busy session)' and event_name = 'message.taken'")" -ge 1 ]; do
    [ "$(date +%s)" -lt "$deadline" ] ||
        harness "the worker's task prompt was never taken within 30 s of its turn end: nothing after it is tracked"
    sleep 0.5
done
note "the worker took its task"

fired_count() { # <command id>: message_not_taken alerts ever fired for it, from the records
    q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'key' = 'message_not_taken:$1' and kind -> 'detail' -> 'record' ->> 'type' = 'alert.fired'"
}
given() { # <what logged>: how many inputs the engine wrote that way
    awk -v a="$(get busy agent)" -v w="$1" '$2 == a && $3 == w' "$WORK/engine.log" | wc -l | tr -d ' '
}

# --- the bulk, then a restart ---------------------------------------------------

turns() { # <session> <event>: how many the log holds, read from the transcript
    q "select count(*) from spawn_events where session_id = '$1' and source = 'claude' and event_name = '$2'"
}
session=$(get busy session)
transcript=$(q "select kind -> 'detail' ->> 'transcript_path' from spawn_events where session_id = '$session' and event_name = 'session.native_started' order by id desc limit 1")
[ -n "$transcript" ] && [ -f "$transcript" ] || harness "the worker's transcript is not known or not there: [$transcript]"
bulk_mib=${BB241_BULK_MIB:-64}
case "$bulk_mib" in '' | *[!0-9]*) harness "BB241_BULK_MIB is not a number: [$bulk_mib]" ;; esac
# Lines no event comes from (Claude Code's file-history snapshots are of this
# kind and size), built aside and appended in one write: one change.
awk -v n="$((bulk_mib * 262))" 'BEGIN {
    pad = sprintf("%4000s", ""); gsub(/ /, "x", pad)
    for (i = 1; i <= n; i++) printf "{\"type\":\"file-history-snapshot\",\"messageId\":\"bb241-%d\",\"pad\":\"%s\"}\n", i, pad
}' >"$WORK/bulk.jsonl" || harness "the bulk was not built"
size_before=$(wc -c <"$transcript")
cat "$WORK/bulk.jsonl" >>"$transcript" || harness "the bulk was not appended"
rm -f "$WORK/bulk.jsonl"
size=$(wc -c <"$transcript")
[ $((size - size_before)) -ge $((bulk_mib * 1048576)) ] || harness "the transcript grew by $((size - size_before)) bytes, not $bulk_mib MiB"
sleep 3
stopped_before=$(turns "$session" turn.stopped)
taken_before=$(turns "$session" message.taken)
[ "$stopped_before" -ge 1 ] && [ "$taken_before" -ge 1 ] ||
    harness "the task's turn is not in the log before the restart (turn.stopped $stopped_before, message.taken $taken_before)"
stop_loop
start_loop
note "the loop restarted over a transcript of $size bytes ($bulk_mib MiB of bulk)"
# The engine stays idle while the restarted Bridge starts over.
sleep 10
idle busy || harness "the worker's engine is not idle after the restart"

# --- (1) the input is taken, (2) its turn is logged, (3) nothing twice ----------------

typed_before=$(given prompt)
c_id=$(send busy bb-busy-c "BB:PING busy C")
until_message busy "$c_id" '.typed_at_ms != null' 30 ||
    harness "(1) input C was not typed within 30 s: $(m busy "$c_id" '.message | tostring')"
typed_at=$(m busy "$c_id" '.message.typed_at_ms')
deadline=$(($(date +%s) + 30))
until [ "$(given prompt)" -gt "$typed_before" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "(1) the engine did not write C as a user line within 30 s"
    sleep 0.2
done
until_message busy "$c_id" '.taken >= 1' 40 ||
    fail "(1) input C, typed into a long-idle engine after a restart, is not taken within 40 s: $(m busy "$c_id" '.message | tostring'); alerts [$(alerts "$c_id" | tr '\n' ' ')]"
sleep 20
[ "$(fired_count "$c_id")" = 0 ] && [ -z "$(alerts "$c_id")" ] ||
    fail "(1) input C was taken, yet message_not_taken fired $(fired_count "$c_id") time(s) for it; alerts [$(alerts "$c_id" | tr '\n' ' ')]"
note "(1) the input was taken, and no alert fired"

ended=$(q "select count(*) from spawn_events where session_id = '$session' and source = 'claude' and event_name = 'turn.stopped' and recorded_at_ms >= $typed_at")
[ "$ended" -ge 1 ] ||
    fail "(2) the turn of input C is not logged: no turn.stopped read from the transcript since it was typed (engine.log: $(tail -n 2 "$WORK/engine.log" | tr '\n' ' '))"
note "(2) its turn is logged"

stopped_old=$(q "select count(*) from spawn_events where session_id = '$session' and source = 'claude' and event_name = 'turn.stopped' and recorded_at_ms < $typed_at")
taken_old=$(q "select count(*) from spawn_events where session_id = '$session' and source = 'claude' and event_name = 'message.taken' and recorded_at_ms < $typed_at")
[ "$stopped_old" = "$stopped_before" ] && [ "$taken_old" = "$taken_before" ] ||
    fail "(3) after the restart read the transcript again, the log holds turn.stopped $stopped_old and message.taken $taken_old from before the input, not $stopped_before and $taken_before"
note "(3) nothing read before the restart was logged twice"

echo "241: after a restart, a long-idle engine's transcript is read through: its input is taken and its turn logged"
exit 0
