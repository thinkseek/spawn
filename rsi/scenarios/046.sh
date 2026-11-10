#!/bin/sh
# Black box: an agent answers a message by its id: `spawn ack <id> done` or
# `spawn ack <id> declined <why>`, from its own pane, with no database login;
# a task's report is the answer to the message that gave it; the message
# reads `acted`, with the outcome, and the page says it.
#
# Objective 045 tracks each message into an agent by its id (the marker
# `[spawn message #<command id>]` in the typed text) from recorded to typed,
# taken and answered. Answered says the turn the message started ended, not
# that the agent did what it asked. Required (the objective's prompt says
# each rule): `spawn ack <command id> done|declined [<why>]` (a why required
# for declined), run in an agent's pane, which has no database login, is
# recorded once in the agent's session: the first ack of a message stands,
# a later one records nothing; an ack of a message that is not into this
# session, or of a task's delivery, records nothing; one made while the loop
# is down is recorded when it is back. A task's report answers the message
# that delivered the task: Outcome done is `done`, blocked or failed is
# `declined` with the Outcome line as the why. `GET /api/fleet/<session>/
# commands` gives the message `acted` (`{outcome, why}`, null until acted)
# and `acted_at_ms`, and state `acted` once acted. web/src/messages.ts's
# `messageLine` appends " · done" or " · declined: <why>".
#
# A real `spawn loop` (the Bridge, the dispatcher, the alerts step; no model
# judge, no gate, no supervisor) runs ephemeral workers of one mission on
# the fake engine below; `spawn serve` answers on a loopback port. The
# engine writes what is typed into it to its transcript as Claude Code does,
# works one step, runs what its mode says, and ends the turn 3 s after the
# prompt. Modes (ctl/<agent>.mode): `ack` runs `spawn ack` in its pane, for
# the message it was given (the id read from the marker; the database
# variables unset): `done`; `declined "bb declines: busy elsewhere"` for a
# text with BB:DECLINE; for BB:BAD <n>: `maybe` (refused), `declined` with no
# why (refused), `done` for message <n> (another session's), then `done`;
# for BB:AGAIN: `done`, then `declined "changed my mind"`; for BB:LATE:
# `done` 8 s after the prompt. `silent` takes and answers, never acks.
# `report` writes its report (Outcome done) on its task; `block` writes one
# with Outcome "blocked: bb has no access to the data". Inputs come from a
# person (`spawn submit`, principal bb-person). Checked:
#   (1) done and declined: two inputs to `acker`, each `acted` within 20 s,
#       taken <= acted: {done}, then {declined, "bb declines: busy
#       elsewhere"}.
#   (2) what records nothing: for BB:BAD <silent's message> the two bad
#       forms exit non-zero, `silent`'s message stays unacted, the message
#       itself reads done; for BB:AGAIN it reads done (the first ack), not
#       declined; acker's own task delivery, which its engine acked too,
#       stays unacted.
#   (3) never acked: `silent`'s message is answered, `acted` null, state
#       `answered`, 10 s after it was answered.
#   (4) a report answers its task: `reporter`'s delivery reads {done};
#       `blocker`'s reads declined, its why naming "blocked".
#   (5) restarts: a BB:LATE input is taken, the loop and serve are stopped,
#       the engine acks while they are down, they start again: the message
#       reads done within 20 s. Then a second restart: 10 s later every
#       message of every worker reads as before.
#   (6) the page's words: web/src/messages.ts's `messageLine` appends
#       " · done", or " · declined: <why>" (a why over 80 characters cut to
#       its first 79 and "…"), after what 045 says; given fixed cases and
#       the answers of (1)-(4). It must load with node alone.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on objective 045's tree: there is no `spawn ack`.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN,
# SPAWN_TEST_DATABASE_URL, tmux, jq, curl and node; creates and drops its
# own database, uses a private tmux socket, loopback only. Every wait is
# bounded in time. On a passing tree it takes about 50 s.
set -eu

asserted=""
harness() {
    echo "046: harness: $*" >&2
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_046_acks_$run"
SPAWN_TMUX_SOCKET="bb-046-$run"
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
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
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
                echo "046: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "046: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "046: harness: unexpected exit $rc" >&2
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
note() { echo "046: $*"; }

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
# prompt (one user line with the whole text). Then its turn: one tool call,
# what its mode (ctl/<agent>.mode) says, and the turn's end
# (`turn_duration`) 3 s after the prompt. Its `spawn ack` runs in its own
# pane, in its workspace, with the pane's environment less any database
# variable. Every prompt, ack and turn end is logged (engine.log: ms agent
# what ...; ack.out: each ack's arguments, exit status and output).
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
mode() { cat "$BB_WORK/ctl/$agent.mode" 2>/dev/null || echo silent; }
ack() { # the agent's own `spawn ack <args>`: no database variable reaches it
  words=$(printf '%s' "$*" | tr ' \n' '__')
  unset_db=$(env | sed -n 's/^\(PG[A-Z_]*\)=.*/-u \1/p' | tr '\n' ' ')
  # shellcheck disable=SC2086
  out=$(env $unset_db -u SPAWN_DATABASE_URL -u SPAWN_TEST_DATABASE_URL "$BB_SPAWN" ack "$@" 2>&1)
  rc=$?
  log "ack rc=$rc $words"
  printf '%s rc=%s %s: %s\n' "$agent" "$rc" "$words" "$out" >>"$BB_WORK/ack.out"
}
report() { # <outcome line>
  mkdir -p .spawn
  printf '## Outcome\n%s\n## What changed\nnothing\n## Open issues\nnone\n' "$1" >.spawn/report.md
  log "reported"
}
act() { # <the prompt's text>: what the mode does with it
  id=$(printf '%s\n' "$1" | sed -n 's/.*\[spawn message #\([0-9][0-9]*\)\].*/\1/p' | tail -n 1)
  case "$(mode)" in
  ack)
    if [ -z "$id" ]; then
      log "no-marker"
      return 0
    fi
    case "$1" in
    *BB:DECLINE*) ack "$id" declined "bb declines: busy elsewhere" ;;
    *BB:BAD*)
      other=$(printf '%s\n' "$1" | sed -n 's/.*BB:BAD \([0-9][0-9]*\).*/\1/p' | head -n 1)
      ack "$id" maybe
      ack "$id" declined
      ack "$other" done
      ack "$id" done
      ;;
    *BB:AGAIN*)
      ack "$id" done
      ack "$id" declined "changed my mind"
      ;;
    *BB:LATE*) (sleep 8; ack "$id" done) </dev/null >/dev/null 2>&1 & ;;
    *) ack "$id" done ;;
    esac
    ;;
  report) report done ;;
  block) report "blocked: bb has no access to the data" ;;
  esac
}
turn() { # <the prompt's text>
  id="toolu_${SPAWN_EXECUTION_ID}_$$_$count"
  line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  tool=$id
  act "$1"
  sleep 0.5
  line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$tool\",\"content\":\"ok\"}]}"
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
    raw=$buffer
    buffer=""
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$(printf '%s' "$raw" | jq -Rs .)}"
    log "prompt"
    printf '\n✻ Working… (esc to interrupt)'
    turn "$raw"
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# Spawn's tmux, on the private socket.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
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
BB_SPAWN=$SPAWN_BIN
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
    SPAWN_CLAUDE_PROJECTS CLAUDE_CONFIG_DIR BB_PROJECTS BB_WORK BB_SPAWN SPAWN_TMUX_PROGRAM SPAWN_WORKER \
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

acted_ok() { # <label> <id> <jq condition on the message>: from the last commands read
    [ "$(m "$1" "$2" "(.message // {}) | ($3)")" = true ]
}
show() { m "$1" "$2" '.message | tostring'; }
ack_rc() { # <words>: the exit status of the engine's `spawn ack <words>` (words joined by _)
    awk -v w="$1" '$3 == "ack" && $5 == w { sub("rc=", "", $4); rc = $4 } END { print rc }' "$WORK/engine.log"
}

# --- the agents ----------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "046 acks"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
labels="acker silent reporter blocker"
for label in $labels; do
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
        --brief "Do the 046 task: answer each message."
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    put "$label" task "$task"
    put "$label" agent "$(q "select agent_id from mission_tasks where id = $task")"
    put "$label" session "$(q "select session_id from mission_tasks where id = $task")"
done
printf 'ack\n' >"$WORK/ctl/$(get acker agent).mode"
printf 'silent\n' >"$WORK/ctl/$(get silent agent).mode"
printf 'report\n' >"$WORK/ctl/$(get reporter agent).mode"
printf 'block\n' >"$WORK/ctl/$(get blocker agent).mode"

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
for label in $labels; do
    commands "$label"
    put "$label" start "$(jq -r '[.[] | select(.action == "start") | .command_id] | min // empty' "$WORK/commands-$label.json")"
    [ -n "$(get "$label" start)" ] || harness "no start command into $label"
done
note "every worker took its task"

# --- (1) done and declined ----------------------------------------------------------------

one=$(send acker bb-one "BB:PING one")
until_message acker "$one" '.acted != null' 20 ||
    fail "(1) acker ran \`spawn ack $one done\` and its message is not acted within 20 s: $(show acker "$one"); its ack: $(grep " ${one}_done:" "$WORK/ack.out" | head -n 1 | cut -c1-300)"
acted_ok acker "$one" '.state == "acted" and .acted.outcome == "done" and .taken_at_ms <= .acted_at_ms' ||
    fail "(1) acker's message $one, acked done, reads $(show acker "$one")"
two=$(send acker bb-two "BB:DECLINE two")
until_message acker "$two" '.acted != null' 20 ||
    fail "(1) acker declined message $two and it is not acted within 20 s: $(show acker "$two")"
acted_ok acker "$two" '.state == "acted" and .acted.outcome == "declined" and .acted.why == "bb declines: busy elsewhere"' ||
    fail "(1) acker's message $two, declined with a why, reads $(show acker "$two")"
note "(1) acker's acks: done, and declined with its why"

# --- (2) what records nothing ----------------------------------------------------------------

quiet=$(send silent bb-quiet "BB:PING silent")
until_message silent "$quiet" '.state == "answered"' 20 ||
    fail "(2) silent's message $quiet is not answered within 20 s: $(show silent "$quiet")"
bad=$(send acker bb-bad "BB:BAD $quiet")
until_message acker "$bad" '.acted != null' 20 ||
    fail "(2) acker's message $bad, acked done after two bad acks, is not acted within 20 s: $(show acker "$bad")"
for words in "${bad}_maybe" "${bad}_declined"; do
    rc=$(ack_rc "$words")
    [ -n "$rc" ] || harness "the engine never ran spawn ack $words"
    [ "$rc" != 0 ] || fail "(2) \`spawn ack $(echo "$words" | tr _ ' ')\` exited 0: $(grep " $words:" "$WORK/ack.out" | head -n 1 | cut -c1-200)"
done
sleep 3
commands acker
acted_ok acker "$bad" '.acted.outcome == "done"' ||
    fail "(2) acker's message $bad reads $(show acker "$bad"), not done"
commands silent
acted_ok silent "$quiet" '.acted == null and .state == "answered"' ||
    fail "(2) silent's message $quiet was acked from acker's pane and reads $(show silent "$quiet"); an ack of another session's message records nothing"
again=$(send acker bb-again "BB:AGAIN")
until_message acker "$again" '.acted != null' 20 ||
    fail "(2) acker's message $again is not acted within 20 s: $(show acker "$again")"
[ -n "$(ack_rc "${again}_declined_changed_my_mind")" ] || {
    sleep 3
    [ -n "$(ack_rc "${again}_declined_changed_my_mind")" ] || harness "the engine never acked $again a second time"
}
sleep 3
commands acker
acted_ok acker "$again" '.acted.outcome == "done"' ||
    fail "(2) acker's message $again, acked done and then declined, reads $(show acker "$again"); the first ack stands"
[ -n "$(ack_rc "$(get acker start)_done")" ] || harness "acker's engine never acked its task's delivery $(get acker start)"
acted_ok acker "$(get acker start)" '.acted == null' ||
    fail "(2) acker's task delivery $(get acker start), which a report answers, reads $(show acker "$(get acker start)") after \`spawn ack $(get acker start) done\`"
note "(2) the bad acks refused; another session's, a second, and a task delivery's ack recorded nothing"

# --- (3) never acked ----------------------------------------------------------------

answered=$(m silent "$quiet" '.message.answered_at_ms')
while [ "$(now_ms)" -lt $((answered + 10000)) ]; do sleep 0.5; done
commands silent
acted_ok silent "$quiet" '.acted == null and .acted_at_ms == null and .state == "answered"' ||
    fail "(3) silent's message $quiet, never acked, reads $(show silent "$quiet") 10 s after it was answered"
note "(3) silent: answered, never acted"

# --- (4) a report answers its task ----------------------------------------------------------------

until_message reporter "$(get reporter start)" '.acted != null' 30 ||
    fail "(4) reporter reported its task (Outcome done) and its delivery $(get reporter start) is not acted within 30 s: $(show reporter "$(get reporter start)")"
acted_ok reporter "$(get reporter start)" '.state == "acted" and .acted.outcome == "done"' ||
    fail "(4) reporter's delivery, reported done, reads $(show reporter "$(get reporter start)")"
until_message blocker "$(get blocker start)" '.acted != null' 30 ||
    fail "(4) blocker reported its task blocked and its delivery $(get blocker start) is not acted within 30 s: $(show blocker "$(get blocker start)")"
acted_ok blocker "$(get blocker start)" '.acted.outcome == "declined" and (.acted.why | test("blocked"))' ||
    fail "(4) blocker's delivery, reported blocked, reads $(show blocker "$(get blocker start)"); declined, its why naming blocked"
for label in $labels; do
    commands "$label"
    cp "$WORK/commands-$label.json" "$WORK/answer-$label.json"
done
note "(4) a report answers its delivery: done; blocked is declined"

# --- (5) restarts ----------------------------------------------------------------

late=$(send acker bb-late "BB:LATE")
until_message acker "$late" '.taken_at_ms != null' 20 ||
    fail "(5) acker's message $late is not taken within 20 s: $(show acker "$late")"
stop_serve
stop_loop
deadline=$(($(date +%s) + 20))
until [ -n "$(ack_rc "${late}_done")" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "acker's engine did not ack $late within 20 s of the loop's stop"
    sleep 0.2
done
[ "$(ack_rc "${late}_done")" = 0 ] ||
    fail "(5) \`spawn ack $late done\`, run while the loop is down, exited $(ack_rc "${late}_done"): $(grep " ${late}_done:" "$WORK/ack.out" | head -n 1 | cut -c1-200)"
start_loop
start_serve
until_message acker "$late" '.acted.outcome == "done"' 20 ||
    fail "(5) acker acked $late while the loop was down, and 20 s after it is back the message reads $(show acker "$late")"
# The turn it started ended while the loop was down too: read once it is
# back, before the messages are compared across the next restart.
until_message acker "$late" '.answered_at_ms != null' 20 ||
    fail "(5) acker's message $late, whose turn ended while the loop was down, is not answered 20 s after it is back: $(show acker "$late")"
note "(5) an ack made while the loop was down is recorded once it is back"
for label in $labels; do
    commands "$label"
    jq -S '[.[] | select(.message != null) | {command_id, message}]' "$WORK/commands-$label.json" >"$WORK/before-$label.json"
done
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
note "(5) a restart changed no message"

# --- (6) the page's words ----------------------------------------------------------------

[ -f "$tree/web/src/messages.ts" ] || fail "(6) there is no web/src/messages.ts"
cat >"$WORK/messages.test.mjs" <<'TEST'
// A command's message in words, as web/src/messages.ts's messageLine says it, acted.
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
  state: "answered", typed_at_ms: 100, taken_at_ms: 1_500, answered_at_ms: 241_500, taken: 1, not_taken: false,
  acted: null, acted_at_ms: null, ...fields,
});
const at = (fields) => ({ command_id: 1, action: "input", created_at_ms: 0, message: message(fields) });
const acted = (outcome, why, fields = {}) => at({ state: "acted", acted: { outcome, why }, acted_at_ms: 250_000, ...fields });

test("fixed cases: done, declined, a long why, with the count", () => {
  assert.equal(messageLine(at({})), "taken in 1 s · answered in 4 min");
  assert.equal(messageLine(acted("done", null)), "taken in 1 s · answered in 4 min · done");
  assert.equal(messageLine(acted("declined", "busy")), "taken in 1 s · answered in 4 min · declined: busy");
  assert.equal(messageLine(acted("declined", "x".repeat(100))), `taken in 1 s · answered in 4 min · declined: ${"x".repeat(79)}…`);
  assert.equal(messageLine(acted("declined", "y".repeat(80))), `taken in 1 s · answered in 4 min · declined: ${"y".repeat(80)}`);
  assert.equal(messageLine(acted("done", null, { taken: 2 })), "taken in 1 s · answered in 4 min · taken 2× · done");
});

test("the scenario's own answers", () => {
  assert.match(messageLine(command("acker", env.BB_ONE)), / · done$/);
  assert.match(messageLine(command("acker", env.BB_TWO)), / · declined: bb declines: busy elsewhere$/);
  assert.match(messageLine(command("silent", env.BB_QUIET)), / · answered in (<1|\d+) s$/);
  assert.match(messageLine(command("reporter", env.BB_REPORTER)), / · done$/);
  assert.match(messageLine(command("blocker", env.BB_BLOCKER)), / · declined: .*blocked/);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_ONE=$one BB_TWO=$two BB_QUIET=$quiet \
    BB_REPORTER="$(get reporter start)" BB_BLOCKER="$(get blocker start)" \
    node --test "$WORK/messages.test.mjs" >"$WORK/messages.out" 2>&1; then
    if grep -q "ERR_MODULE_NOT_FOUND\|SyntaxError\|ERR_UNKNOWN_FILE_EXTENSION\|ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX" "$WORK/messages.out"; then
        fail "(6) web/src/messages.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/messages.out" | tr '\n' ' ')"
    fi
    fail "(6) the page's words: $(grep -E 'AssertionError|expected|actual|is not a function|command ' "$WORK/messages.out" | head -n 6 | tr '\n' ' ')"
fi
note "(6) the page says each answer in words"

note "every ack recorded once, by id, from the pane; a report answers its task; nothing twice across a restart"
exit 0
