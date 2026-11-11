#!/bin/sh
# Black box: every message into an agent is tracked by its id, from records:
# recorded, typed, taken, answered; a message not taken within its bound,
# and one taken twice, are said; the page shows each.
#
# A message is what Spawn types into an agent's engine: a person's input,
# the dispatcher's task, a send-back, a digest (each a command: an `input`,
# or a `start` or `resume` with a prompt). Today Spawn records when a
# command was created and when its text was typed (`finished_at_ms`), and a
# prompt reaching the engine as `message.received`, with no id: which
# message the engine took, whether it took it twice, and whether the turn
# it started ended, can only be guessed from the text and the order.
# Required (the objective's prompt says each rule): the text Spawn types
# carries the message's id (its command id) in a marker the engine's
# transcript keeps; the transcript's line that holds the marker is the
# message taken; the end of the turn it started is the message answered;
# each is recorded once, whatever restarts. `GET /api/fleet/<session>/
# commands` gives each command a `message` (null for a command that types
# nothing): `{state, typed_at_ms, taken_at_ms, answered_at_ms, taken,
# not_taken}`, state `recorded` (not typed yet), `typed`, `taken` or
# `answered`; `taken` the times its marker reached the transcript;
# `not_taken` true while it is not taken SPAWN_MESSAGE_TAKEN_SECS (here 6)
# after it was recorded. The alerts step raises `message_not_taken`, subject
# the session, key `message_not_taken:<command id>`, once per message, when
# a message passes its bound untaken; it clears when the message is taken or
# the session ends. web/src/messages.ts's `messageLine` says a command's
# message in words on the agent page.
#
# A real `spawn loop` (the Bridge, the dispatcher, the alerts step; no model
# judge, no supervisor) runs four ephemeral workers of one mission on the
# fake engine below; `spawn serve` answers on a loopback port. The engine
# writes what is typed into it to its transcript as Claude Code does (one
# user line with the whole text, after a second of quiet), works one step
# (a tool call), and ends the turn 3 s later (`turn_duration`). Its mode,
# per agent (ctl/<agent>.mode): `take` (the default); `deaf` (it reads
# what is typed and writes nothing: typed, never taken); `twice` (it writes
# the user line twice, as a paste delivered twice would). Spawn's tmux
# (SPAWN_TMUX_PROGRAM) refuses the paste buffer of the terminal named in
# `refuse` (load-buffer fails: nothing is typed, and the Bridge tries again
# later). Inputs come from a person (`spawn submit`, principal bb-person).
# SPAWN_MESSAGE_TAKEN_SECS=6, SPAWN_ALERT_CHECK_SECS=1. Checked:
#   (1) taken and answered, by id: worker `take`'s task (the dispatcher's
#       start) is answered, taken once. Two inputs with the same text, sent
#       together: each is typed, taken once and answered, the second's taken
#       after the first's answered (each its own: matched by id, not by
#       text or order), state `taken` seen while its turn runs, then
#       `answered`; typed <= taken <= answered; each within 20 s.
#   (2) typed, never taken: an input to `deaf` is `typed` within 15 s with
#       no taken time; 6 s after it was recorded `not_taken` is true and
#       exactly one open `message_not_taken` alert names it (subject the
#       session; evidence its command id), served by GET /api/alerts.
#   (3) never typed: an input to `refuse` stays `recorded` (no typed time)
#       past its bound, `not_taken`, one open alert; once the refusal is
#       lifted it is typed, taken and answered within 30 s, `not_taken`
#       false, and its alert cleared.
#   (4) taken twice: an input to `twice` is answered with `taken` 2.
#   (5) restarts: the loop and serve are restarted: 10 s later every
#       message reads as before (state, times, `taken`), `deaf`'s alert is
#       the one open alert (not fired again), `refuse`'s stays cleared; a
#       new input to `take` is tracked to `answered`.
#   (6) before the change: three inputs into `take` that an older Spawn
#       typed (seeded into the log after init-db, before the loop starts:
#       commands, their Bridge `typing` and `typed` steps and the engine's
#       `message.received`, text with no marker), recorded 1 h and 10 min
#       before the upgrade and one just after it, as the old loop, which a
#       deploy stops only after init-db, types on: each reads `typed`,
#       `not_taken` false, and no `message_not_taken` names any, after (4)
#       and again after (5)'s restart.
#   (7) the session ends: `deaf`'s task is cancelled (its worker stopped);
#       its message's alert is cleared within 30 s and stays in
#       /api/alerts as cleared.
#   (8) the page's words: web/src/messages.ts's `messageLine(command)`:
#       null for a command with no message; "not typed yet", "typed, not
#       taken yet"; "not taken (never typed)", "not taken (typed)" once
#       `not_taken`; "taken in <d>", "taken in <d> · answered in <d>" (taken
#       from the command's creation, answered from the taking), with
#       " · taken 2×" (the count) when taken more than once; <d> is "<1 s",
#       "<n> s" under a minute, "<n> min" under an hour, "<n> h", rounded
#       down. Given the answers of (1)-(4) and fixed cases. messages.ts and
#       its imports must load with node alone; one that does not fails (8).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (99565e2a): the commands answer no
# `message`.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN,
# SPAWN_TEST_DATABASE_URL, tmux, jq, curl and node; creates and drops its
# own database, uses a private tmux socket, loopback only. Every wait is
# bounded in time. On a passing tree it takes about a minute.
set -eu

asserted=""
harness() {
    echo "045: harness: $*" >&2
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
# A scratch HOME, and the tmux server's socket, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_045_messages_$run"
SPAWN_TMUX_SOCKET="bb-045-$run"
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
                echo "045: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "045: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "045: harness: unexpected exit $rc" >&2
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
note() { echo "045: $*"; }

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
cat >"$WORK/claude" <<'ENGINE'
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
  if IFS= read -r -t 1 typed; then
    buffer="${buffer:+$buffer
}$typed"
  elif [ -n "$buffer" ]; then
    text=$(printf '%s' "$buffer" | jq -Rs .)
    buffer=""
    case "$(mode)" in
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
SPAWN_MESSAGE_TAKEN_SECS=6
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
start_serve() {
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
    base="http://127.0.0.1:$port"
}

# --- helpers ----------------------------------------------------------------

now_ms() { date +%s%3N; }
q() { scalar "$1"; }
put() { printf '%s\n' "$3" >"$WORK/state/$1.$2"; }
get() { cat "$WORK/state/$1.$2" 2>/dev/null || true; }
# api <path> <file>: GET from serve; its status code.
api() { curl -s --noproxy '*' -o "$2" -w '%{http_code}' "$base$1" 2>/dev/null || echo 000; }
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

capture "$SPAWN_BIN" mission add --text "045 messages"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
labels="take deaf twice refuse"
for label in $labels; do
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
        --brief "Do the 045 task: answer each message."
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    put "$label" task "$task"
    put "$label" agent "$(q "select agent_id from mission_tasks where id = $task")"
    put "$label" session "$(q "select session_id from mission_tasks where id = $task")"
done

# What an older Spawn left in the log (6): three inputs into take, typed by
# a Bridge that typed no marker, received by the engine: 1 h and 10 min
# before this upgrade (init-db above), and one after it, as the old loop
# types on until the deploy restarts it (deploy.sh stops only the
# integrator before init-db).
cat >"$WORK/seed.sql" <<SQL
\\set ON_ERROR_STOP 1
begin;
create function pg_temp.event(at_ms bigint, src text, key text, kind jsonb) returns void
language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('$(get take agent)', '$(get take session)', null, src, key, kind,
            coalesce((select max(session_seq) from spawn_events
                       where session_id = '$(get take session)'), 0) + 1, at_ms) \$\$;
create function pg_temp.old(k text, text_ text, ago_ms bigint) returns void
language plpgsql as \$\$
declare
    v_id bigint;
    v_at bigint := spawn_now_ms() - ago_ms;
begin
    insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                                succeeded, error, created_at_ms, finished_at_ms)
    values ('bb-person', k, '$(get take agent)', '$(get take session)',
            jsonb_build_object('type', 'input', 'text', text_), 'finished', true, null,
            v_at, v_at + 1000)
    returning id into v_id;
    perform pg_temp.event(v_at + 200, 'bridge', 'cmd_' || v_id || ':typing',
        jsonb_build_object('name', 'command.step',
                           'detail', jsonb_build_object('command_id', v_id, 'step', 'typing')));
    perform pg_temp.event(v_at + 300, 'bridge', 'cmd_' || v_id || ':typed',
        jsonb_build_object('name', 'command.step',
                           'detail', jsonb_build_object('command_id', v_id, 'step', 'typed')));
    perform pg_temp.event(v_at + 1500, 'claude', k || '-line',
        jsonb_build_object('name', 'message.received'));
end \$\$;
select pg_temp.old('bb-old-1h', 'BB:PING from an older Spawn, an hour ago', 3600000);
select pg_temp.old('bb-old-10m', 'BB:PING from an older Spawn, ten minutes ago', 600000);
select pg_temp.old('bb-old-after', 'BB:PING from the old loop after the upgrade', 0);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the older inputs failed: $(err)"
old_ids=$(q "select string_agg(id::text, ' ' order by id) from spawn_commands where idempotency_key like 'bb-old-%'")
[ "$(echo "$old_ids" | wc -w)" -eq 3 ] || harness "the seeded older inputs are not three commands: [$old_ids]"

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

# --- (1) taken and answered, by id ----------------------------------------------------

commands take
start_id=$(jq -r '[.[] | select(.message != null and .action == "start") | .command_id] | min // empty' "$WORK/commands-take.json")
[ -n "$start_id" ] || fail "(1) no command into worker take has a message (its task was typed into it): $(jq -c '[.[] | {command_id, action, message}]' "$WORK/commands-take.json" | head -c 400)"
until_message take "$start_id" '.state == "answered" and .taken == 1' 20 ||
    fail "(1) the task typed into take is not answered, taken once: $(m take "$start_id" '.message | tostring')"
first=$(send take bb-ping-1 "BB:PING the same text")
second=$(send take bb-ping-2 "BB:PING the same text")
until_message take "$first" '.state == "taken" and .answered_at_ms == null' 20 ||
    fail "(1) the first input to take was never seen taken with its turn running: $(m take "$first" '.message | tostring')"
for id in $first $second; do
    until_message take "$id" '.state == "answered"' 30 ||
        fail "(1) input $id to take is not answered within 30 s: $(m take "$id" '.message | tostring')"
done
for id in $first $second; do
    [ "$(m take "$id" '.message | .taken == 1 and .typed_at_ms <= .taken_at_ms and .taken_at_ms <= .answered_at_ms and .not_taken == false')" = true ] ||
        fail "(1) input $id to take is not typed <= taken <= answered, taken once, not late: $(m take "$id" '.message | tostring')"
    [ "$(m take "$id" '(.message.answered_at_ms - .created_at_ms) <= 20000')" = true ] ||
        fail "(1) input $id to take was answered more than 20 s after it was sent: $(m take "$id" '.message | tostring')"
done
[ "$(m take "$second" '.message.taken_at_ms')" -gt "$(m take "$first" '.message.answered_at_ms')" ] ||
    fail "(1) the second input (same text) reads as taken at $(m take "$second" '.message.taken_at_ms'), before the first was answered at $(m take "$first" '.message.answered_at_ms'): messages are matched by text or order, not by id"
cp "$WORK/commands-take.json" "$WORK/answer-take.json"
note "(1) the task and two inputs with the same text: each typed, taken once and answered"

# --- (2) typed, never taken; (3) never typed; (4) taken twice ----------------------------

printf 'deaf\n' >"$WORK/ctl/$(get deaf agent).mode"
printf 'twice\n' >"$WORK/ctl/$(get twice agent).mode"
terminal_of refuse >"$WORK/refuse"
deaf_id=$(send deaf bb-deaf "BB:PING deaf")
refuse_id=$(send refuse bb-refuse "BB:PING refuse")
twice_id=$(send twice bb-twice "BB:PING twice")
sent=$(now_ms)
until_message deaf "$deaf_id" '.state == "typed" and .typed_at_ms != null and .taken_at_ms == null' 15 ||
    fail "(2) the input to deaf is not typed and untaken within 15 s: $(m deaf "$deaf_id" '.message | tostring')"
until_message deaf "$deaf_id" '.not_taken == true and .state == "typed"' 15 ||
    fail "(2) the input to deaf, typed and never taken, is not not_taken 15 s on (bound 6 s): $(m deaf "$deaf_id" '.message | tostring')"
deadline=$(($(date +%s) + 15))
until [ "$(alerts "$deaf_id" | grep -c open)" -ge 1 ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
[ "$(alerts "$deaf_id" | tr '\n' ' ')" = "open " ] ||
    fail "(2) the input to deaf, not taken, has alerts [$(alerts "$deaf_id" | tr '\n' ' ')]; exactly one open message_not_taken: $(jq -c '[.. | objects | select(.kind? == "message_not_taken")]' "$WORK/alerts.json" | head -c 400)"
[ "$(jq -r --arg s "$(get deaf session)" --arg id "$deaf_id" '[.. | objects | select(.kind? == "message_not_taken" and (.key // "" | endswith(":" + $id)))][0].subject' "$WORK/alerts.json")" = "$(get deaf session)" ] ||
    fail "(2) the alert for the input to deaf is not keyed message_not_taken:<command id> with the session as subject: $(jq -c '[.. | objects | select(.kind? == "message_not_taken")]' "$WORK/alerts.json" | head -c 300)"
note "(2) deaf: typed, never taken, not_taken and one alert"

until_message refuse "$refuse_id" '.not_taken == true and .state == "recorded" and .typed_at_ms == null' 15 ||
    fail "(3) the input to refuse, whose pane refuses the paste, is not recorded, untyped and not_taken 15 s on: $(m refuse "$refuse_id" '.message | tostring')"
deadline=$(($(date +%s) + 15))
until [ "$(alerts "$refuse_id" | grep -c open)" -ge 1 ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
[ "$(alerts "$refuse_id" | tr '\n' ' ')" = "open " ] ||
    fail "(3) the input to refuse, never typed, has alerts [$(alerts "$refuse_id" | tr '\n' ' ')]; exactly one open message_not_taken"
rm -f "$WORK/refuse"
until_message refuse "$refuse_id" '.state == "answered" and .taken == 1 and .not_taken == false' 30 ||
    fail "(3) the input to refuse, once the pane takes a paste, is not typed, taken and answered within 30 s: $(m refuse "$refuse_id" '.message | tostring')"
deadline=$(($(date +%s) + 15))
until [ "$(alerts "$refuse_id" | tr '\n' ' ')" = "cleared " ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
[ "$(alerts "$refuse_id" | tr '\n' ' ')" = "cleared " ] ||
    fail "(3) the input to refuse was taken and its message_not_taken alert is [$(alerts "$refuse_id" | tr '\n' ' ')], not cleared"
note "(3) refuse: recorded, untyped, not_taken and one alert; typed, taken and answered once lifted, its alert cleared"

until_message twice "$twice_id" '.state == "answered"' 30 ||
    fail "(4) the input to twice is not answered within 30 s: $(m twice "$twice_id" '.message | tostring')"
[ "$(m twice "$twice_id" '.message.taken')" = 2 ] ||
    fail "(4) the input to twice reached its transcript twice and reads taken $(m twice "$twice_id" '.message.taken'), not 2: $(m twice "$twice_id" '.message | tostring')"
note "(4) twice: taken 2"
old_quiet "before the restart"
note "(6) the inputs from before the change: typed, not not_taken, no alert"

# --- (5) restarts ----------------------------------------------------------------

for label in $labels; do
    commands "$label"
    jq -S '[.[] | select(.message != null) | {command_id, message}]' "$WORK/commands-$label.json" >"$WORK/before-$label.json"
done
cp "$WORK/commands-deaf.json" "$WORK/answer-deaf.json"
cp "$WORK/commands-refuse.json" "$WORK/answer-refuse.json"
cp "$WORK/commands-twice.json" "$WORK/answer-twice.json"
stop_serve
stop_loop
start_loop
start_serve
sleep 10
for label in $labels; do
    commands "$label"
    jq -S '[.[] | select(.message != null) | {command_id, message}]' "$WORK/commands-$label.json" >"$WORK/after-$label.json"
    cmp -s "$WORK/before-$label.json" "$WORK/after-$label.json" ||
        fail "(5) after a restart the messages into $label read otherwise: before $(jq -c . "$WORK/before-$label.json" | head -c 300) after $(jq -c . "$WORK/after-$label.json" | head -c 300)"
done
[ "$(alerts "$deaf_id" | tr '\n' ' ')" = "open " ] ||
    fail "(5) after a restart deaf's message has alerts [$(alerts "$deaf_id" | tr '\n' ' ')], not its one open alert"
[ "$(alerts "$refuse_id" | tr '\n' ' ')" = "cleared " ] ||
    fail "(5) after a restart refuse's message has alerts [$(alerts "$refuse_id" | tr '\n' ' ')], not its one cleared alert"
others=$(jq -r --arg d "$deaf_id" --arg r "$refuse_id" '[.. | objects | select(.kind? == "message_not_taken")
    | select((.key // "" | endswith(":" + $d) or endswith(":" + $r)) | not)] | unique_by(.key) | length' "$WORK/alerts.json")
[ "$others" = 0 ] || fail "(5) $others message_not_taken alerts name a message that was taken"
after=$(send take bb-ping-3 "BB:PING after the restart")
until_message take "$after" '.state == "answered" and .taken == 1' 30 ||
    fail "(5) an input to take after the restart is not answered, taken once, within 30 s: $(m take "$after" '.message | tostring')"
note "(5) a restart changed no message and fired no alert again; a new input was tracked"
old_quiet "after the restart"
note "(6) after the restart too"

# --- (7) the session ends ----------------------------------------------------------------

capture "$SPAWN_BIN" task cancel --id "$(get deaf task)"
[ "$status" -eq 0 ] || harness "task cancel (deaf) failed: $(err)"
deadline=$(($(date +%s) + 30))
until [ "$(alerts "$deaf_id" | tr '\n' ' ')" = "cleared " ] || [ "$(date +%s)" -ge "$deadline" ]; do sleep 0.5; done
[ "$(alerts "$deaf_id" | tr '\n' ' ')" = "cleared " ] ||
    fail "(7) deaf's task was cancelled and its worker stopped; its untaken message's alert is [$(alerts "$deaf_id" | tr '\n' ' ')], not cleared within 30 s"
note "(7) deaf's session ended: its message's alert cleared"

# --- (8) the page's words ----------------------------------------------------------------

[ -f "$tree/web/src/messages.ts" ] || fail "(8) there is no web/src/messages.ts"
cat >"$WORK/messages.test.mjs" <<'TEST'
// A command's message in words, as web/src/messages.ts's messageLine says it.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { messageLine } = await import(pathToFileURL(`${env.BB_TREE}/web/src/messages.ts`).href);
const answer = (label) => JSON.parse(readFileSync(`${env.BB_WORK}/answer-${label}.json`, "utf8"));
const command = (label, id) => {
  const found = answer(label).filter((entry) => String(entry.command_id) === String(id));
  assert.equal(found.length, 1, `command ${id} in ${label}'s answer`);
  return found[0];
};
const message = (fields) => ({
  state: "answered", typed_at_ms: 100, taken_at_ms: 1_500, answered_at_ms: 241_500, taken: 1, not_taken: false, ...fields,
});
const at = (fields) => ({ command_id: 1, action: "input", created_at_ms: 0, message: message(fields) });

test("fixed cases: each state, the durations and the count", () => {
  assert.equal(messageLine({ command_id: 1, action: "stop", created_at_ms: 0, message: null }), null);
  assert.equal(messageLine(at({ state: "recorded", typed_at_ms: null, taken_at_ms: null, answered_at_ms: null, taken: 0 })), "not typed yet");
  assert.equal(messageLine(at({ state: "typed", taken_at_ms: null, answered_at_ms: null, taken: 0 })), "typed, not taken yet");
  assert.equal(messageLine(at({ state: "recorded", typed_at_ms: null, taken_at_ms: null, answered_at_ms: null, taken: 0, not_taken: true })), "not taken (never typed)");
  assert.equal(messageLine(at({ state: "typed", taken_at_ms: null, answered_at_ms: null, taken: 0, not_taken: true })), "not taken (typed)");
  assert.equal(messageLine(at({ state: "taken", taken_at_ms: 800, answered_at_ms: null })), "taken in <1 s");
  assert.equal(messageLine(at({})), "taken in 1 s · answered in 4 min");
  assert.equal(messageLine(at({ taken_at_ms: 59_999, answered_at_ms: 59_999 + 3_600_000 })), "taken in 59 s · answered in 1 h");
  assert.equal(messageLine(at({ taken: 2 })), "taken in 1 s · answered in 4 min · taken 2×");
});

test("the scenario's own answers", () => {
  assert.match(messageLine(command("take", env.BB_FIRST)), /^taken in (<1|\d+) s · answered in (<1|\d+) s$/);
  assert.equal(messageLine(command("deaf", env.BB_DEAF)), "not taken (typed)");
  assert.match(messageLine(command("refuse", env.BB_REFUSE)), /^taken in \d+ s · answered in (<1|\d+) s$/);
  assert.match(messageLine(command("twice", env.BB_TWICE)), / · taken 2×$/);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_FIRST=$first BB_DEAF=$deaf_id BB_REFUSE=$refuse_id BB_TWICE=$twice_id \
    node --test "$WORK/messages.test.mjs" >"$WORK/messages.out" 2>&1; then
    if grep -q "ERR_MODULE_NOT_FOUND\|SyntaxError\|ERR_UNKNOWN_FILE_EXTENSION\|ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX" "$WORK/messages.out"; then
        fail "(8) web/src/messages.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/messages.out" | tr '\n' ' ')"
    fi
    fail "(8) the page's words: $(grep -E 'AssertionError|expected|actual|is not a function|command ' "$WORK/messages.out" | head -n 6 | tr '\n' ' ')"
fi
note "(8) the page says each message in words"

note "every message tracked by its id: typed, taken, answered; the untaken and the doubled said; nothing twice across a restart"
exit 0
