#!/bin/sh
# Objective 214 [observability] (GitHub #66 part 1): an input typed while
# the agent works is taken. The engine (Claude Code 2.1.32x) gives such an
# input to the agent mid-turn and writes it ONLY as a transcript line
# {"type":"attachment","attachment":{"type":"queued_command",
# "commandMode":"prompt","prompt":"<text> [spawn message #<id>]"}}, never as
# a user line. Spawn read markers from user lines alone, so every input to a
# busy worker was not taken: its message_not_taken alert fired and stayed
# open until the session ended (20 open at once on the issue's host).
#
# Black box, 138's world: a real `spawn loop` (the Bridge, the dispatcher,
# the alerts step) runs one ephemeral worker on 045's fake engine, its mode
# per agent (ctl/<agent>.mode): `take` writes what is typed as a user line
# (a prompt of its own) and works a turn; `queued` (214's own) writes it as
# the mid-turn queued_command attachment line above, with its marker, and
# works on; `notify` writes it as a queued_command of commandMode
# task-notification (a job's notification, the engine's own text) that
# quotes the marker. Inputs come from a person (`spawn submit`).
# SPAWN_MESSAGE_TAKEN_SECS=12, SPAWN_ALERT_CHECK_SECS=1. Checked (through
# GET /api/fleet/<session>/commands and GET /api/alerts, and the records):
#   (1) queued: input A is given mid-turn; it is taken (taken >= 1) within
#       40 s, and 20 s after its bound no message_not_taken alert was ever
#       fired for it.
#   (2) notify: input B appears only inside a notification that quotes its
#       marker; it is never taken (the marker, as Spawn typed it, stays the
#       taken signal, and the engine's own text takes nothing). The
#       notification and its turn move the transcript after B's typing,
#       recorded before B's bound, so since PR #88 (ace244ad0, issue #66
#       Part 1) its message_not_taken never fires: past the bound B reads
#       not taken (taken 0, not_taken) with no alert ever fired.
# Exit: 0 pass, 1 a check failed, 2 harness.
#
# Fails with 1 on live main a599100c: in (1) A is never taken and its alert
# opens.
set -eu

asserted=""
harness() {
    echo "214: harness: $*" >&2
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
SPAWN_DB_NAME="bb_214_queued_$run"
SPAWN_TMUX_SOCKET="bb-214-$run"
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
                echo "214: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "214: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "214: harness: unexpected exit $rc" >&2
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
note() { echo "214: $*"; }

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

capture "$SPAWN_BIN" mission add --text "214 mid-turn input"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker busy \
    --brief "Do the 214 task: answer each message."
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
moved() { # <command id>: transcript events of its session recorded after its typing
    q "select count(*) from spawn_events e,
         (select min(t.id) as id, min(t.recorded_at_ms) as at from spawn_events t
           where t.session_id = '$(get busy session)' and t.source = 'bridge'
             and t.source_event_id = 'cmd_$1:typed') typing
        where e.session_id = '$(get busy session)' and e.source = 'claude'
          and (e.recorded_at_ms > typing.at or (e.recorded_at_ms = typing.at and e.id > typing.id))"
}

# --- (1) an input given mid-turn is taken -------------------------------------------

printf 'queued\n' >"$WORK/ctl/$(get busy agent).mode"
before=$(given prompt-queued)
a_id=$(send busy bb-busy-a "BB:PING busy A")
until_message busy "$a_id" '.typed_at_ms != null' 30 ||
    harness "(1) input A was not typed within 30 s: $(m busy "$a_id" '.message | tostring')"
deadline=$(($(date +%s) + 30))
until [ "$(given prompt-queued)" -gt "$before" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "(1) the engine did not write A as a mid-turn line within 30 s"
    sleep 0.2
done
until_message busy "$a_id" '.taken >= 1' 40 ||
    fail "(1) input A, given mid-turn as a queued_command line with its marker, is not taken within 40 s: $(m busy "$a_id" '.message | tostring'); alerts [$(alerts "$a_id" | tr '\n' ' ')]"
# Its bound (12 s) and more pass: no alert was ever fired for it.
sleep 20
[ "$(fired_count "$a_id")" = 0 ] && [ -z "$(alerts "$a_id")" ] ||
    fail "(1) input A was taken mid-turn, yet message_not_taken fired $(fired_count "$a_id") time(s) for it; alerts [$(alerts "$a_id" | tr '\n' ' ')]"
note "(1) an input given mid-turn was taken, and no alert fired"

# --- (2) a marker quoted in the engine's own notification takes nothing ---------------

# A's turn end is recorded too: what moves after B's typing is B's own.
deadline=$(($(date +%s) + 30))
until idle busy &&
    [ "$(q "select count(*) from spawn_events where session_id = '$(get busy session)' and source = 'claude' and event_name = 'turn.stopped'")" -ge "$(given turn-ended)" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "(2) the engine did not end A's turn, recorded, within 30 s"
    sleep 0.2
done
printf 'notify\n' >"$WORK/ctl/$(get busy agent).mode"
before=$(given prompt-notify)
b_id=$(send busy bb-busy-b "BB:PING busy B")
deadline=$(($(date +%s) + 30))
until [ "$(given prompt-notify)" -gt "$before" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "(2) the engine did not write B inside a notification within 30 s"
    sleep 0.2
done
# The notification line is recorded: a transcript event of the session after
# B's typing (read_since_typed's own count, spawn-postgres alerts.rs).
deadline=$(($(date +%s) + 30))
until [ "$(moved "$b_id")" -ge 1 ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "(2) no transcript line after B's typing was recorded within 30 s"
    sleep 0.2
done
commands busy
[ "$(m busy "$b_id" '.message.taken')" = 0 ] ||
    fail "(2) input B, seen only quoted in a job's notification, reads taken: $(m busy "$b_id" '.message | tostring')"
[ "$(m busy "$b_id" '.message.not_taken')" = false ] ||
    harness "(2) B was already past its bound when its transcript moved (load?): $(m busy "$b_id" '.message | tostring')"
# Its bound (12 s) passes: still not taken, and never an alert.
until_message busy "$b_id" '.not_taken == true' 40 ||
    harness "(2) input B did not reach its bound within 40 s: $(m busy "$b_id" '.message | tostring')"
sleep 5
commands busy
[ "$(m busy "$b_id" '.message.taken')" = 0 ] ||
    fail "(2) input B, seen only quoted in a job's notification, reads taken past its bound: $(m busy "$b_id" '.message | tostring')"
[ "$(fired_count "$b_id")" = 0 ] && [ -z "$(alerts "$b_id")" ] ||
    fail "(2) input B's transcript moved after its typing, yet message_not_taken fired $(fired_count "$b_id") time(s); alerts [$(alerts "$b_id" | tr '\n' ' ')]; none (PR #88)"
note "(2) a marker quoted in a notification took nothing; its transcript moved, so no alert fired"

echo "214: an input given mid-turn is taken; the engine's own text takes nothing"
exit 0
