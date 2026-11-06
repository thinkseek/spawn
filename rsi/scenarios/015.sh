#!/bin/sh
# Black box: an input to a session that is busy waits for its turn; it is
# not refused. It is recorded as waiting, typed once the turn ends, in the
# order it was sent, at most once, and fails with the reason when the
# session is stopped first. The dispatcher's own inputs keep their contract
# (refused at once while the pane is not ready, then retried), and its
# deliveries still land.
#
# A real `spawn loop` (the Bridge, the Observer with no model judge, the
# dispatcher, no supervisor) runs one resident, `echo`, on a fake engine.
# The engine opens a turn for every text typed at its prompt. A text naming
# `BB:BUSY <seconds> <tag>` is a long turn: the screen shows the engine's
# spinner (`✻ Churning… (esc to interrupt)`, read as Busy) and the
# transcript a tool call every 2 s, for that long, then the turn ends (the
# transcript's end of the turn, then `✻ Churned for <n>s · done` and the
# prompt). A text naming `BB:NOTE <tag>` is a short turn of 4 s, Busy too.
# The engine logs every text it reads, and flags any text that reaches it
# while a turn runs (typed into a busy pane). Inputs are sent with
# `spawn submit` as `human` and as `spawn_supervisor` (the supervisor's
# principal).
# Checked:
#   (1) waiting, not refused: two inputs sent while a 45 s turn runs (one
#       from `human`, one from `spawn_supervisor`) are not finished while it
#       runs, and each has a `command.step` record from the Bridge with step
#       `waiting` (once per command); nothing reached the engine.
#   (2) typed when the turn ends, in order, at most once: the loop is
#       restarted while they wait; once the turn ends both finish succeeded,
#       the engine reads each exactly once, the first before the second, the
#       first only after the long turn ended and the second only after the
#       first's own turn ended; nothing is ever typed into a busy pane; each
#       has exactly one `typed` step.
#   (3) stopped first: an input waiting behind a 120 s turn, then a `stop`
#       of the session: the stop is handled while the turn still runs (not
#       held behind the waiting input), and the input finishes failed with
#       an error that says the session was stopped (`stopped`), was never
#       typed (no `typing` step) and never reached the engine.
#   (4) the dispatcher's contract holds: the session resumed after the
#       stop, a task assigned to the resident while a human's input waits
#       behind another long turn is delivered exactly once (an input from
#       spawn-dispatch keyed `deliver-<task>-...` that succeeded; the task
#       is `delivered`), after the waiting input; every one of its attempts
#       that failed was refused at once as not ready (`does not accept
#       input`), and no input of the dispatcher is ever recorded `waiting`.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (115f078, and the deployed binary
# built from 3e2c08e): the first input sent during the turn finishes failed
# at once with `pane state Busy does not accept input`, and nothing types it
# later. It passes once the Bridge holds an input it cannot type yet as
# `waiting` (recorded, so a restarted Bridge keeps it), types the session's
# waiting inputs oldest first when the pane is ready again, lets a stop
# through, and fails what still waits when the session's execution ends.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "015: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_015_waiting_$run"
SPAWN_TMUX_SOCKET="bb-015-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
stop_loop() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
        loop_pid=""
    fi
}
cleanup() {
    stop_loop
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "$database_made" ]; then
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "015: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "015: what the engine read:" >&2
            cat "$WORK/engine.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "015: the resident's commands (id, principal, key, state, succeeded, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, principal, idempotency_key, state, succeeded,
                    left(coalesce(error, ''), 160)
                from spawn_commands where session_id = 'session_resident_echo' order by id" \
                >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "015: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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

# The fake engine. Every line it reads is logged `got <time> <text>`; a
# line read while one of its turns runs is logged `during <time> <text>`
# (typed into a busy pane) and otherwise ignored. A turn opens with the
# first line typed at its prompt and starts after a second of quiet; it is
# `BB:BUSY <seconds> <tag>` (a long turn, `busy-start`/`busy-end <time>
# <tag>` in the log), `BB:NOTE <tag>` (4 s, `note-end <time> <tag>` at its
# end), or anything else (4 s, like a note). While a turn runs the screen
# shows the spinner line and the prompt below it, as Claude Code does; at
# its end the transcript's end of the turn comes first, then the finished
# line and the prompt.
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
log() { printf '%s %s %s\n' "$1" "$(date +%s.%N)" "$2" >>"$BB_WORK/engine.log"; }
tool() {
  line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"$1\",\"name\":\"Bash\",\"input\":{}}]}"
  line "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$1\",\"content\":\"ok\"}]}"
}
# run_turn <seconds> <id>: Busy on the screen for that long, a tool call
# every 2 s; anything typed meanwhile is logged as typed into a busy pane.
calls=0
run_turn() {
  end=$(($(date +%s) + $1))
  next=0
  printf '\n✻ Churning… (esc to interrupt)\n❯ '
  while [ "$(date +%s)" -lt "$end" ]; do
    if [ "$(date +%s)" -ge "$next" ]; then
      calls=$((calls + 1))
      tool "toolu_${SPAWN_EXECUTION_ID}_$2_$calls"
      printf '\n✻ Churning… (esc to interrupt)\n❯ '
      next=$(($(date +%s) + 2))
    fi
    if IFS= read -r -t 0.5 typed; then
      log during "$typed"
    fi
  done
  line '"type":"system","subtype":"turn_duration","durationMs":1'
  printf '\n✻ Churned for %ss · done\n❯ ' "$1"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
turns=0
while :; do
  if IFS= read -r -t 1 typed; then
    log got "$typed"
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:BUSY "*) [ -n "$job" ] || job="busy ${typed#*BB:BUSY }" ;;
    *"BB:NOTE "*) [ -n "$job" ] || job="note ${typed#*BB:NOTE }" ;;
    esac
  elif [ "$pending" = 1 ]; then
    turns=$((turns + 1))
    set -- ${job:-note other}
    case "$1" in
    busy)
      log busy-start "$3"
      run_turn "$2" "t$turns"
      log busy-end "$3"
      ;;
    note)
      run_turn 4 "t$turns"
      log note-end "$2"
      ;;
    esac
    job=
    pending=0
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$WORK/engine.log"

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

SESSION=session_resident_echo
AGENT=agent_resident_echo
# send <principal> <key> <action json>: the command's id.
send() {
    capture "$SPAWN_BIN" submit --principal "$1" --agent "$AGENT" --session "$SESSION" \
        --key "$2" --action "$3"
    [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
    id=$(sed -n 's/^command=//p' "$WORK/out.txt")
    [ -n "$id" ] || harness "submit $2 printed no command id: $(out)"
    echo "$id"
}
say() { send "$1" "$2" "{\"type\":\"input\",\"text\":\"$3\"}"; }

state_of() { q "select state || ':' || coalesce(succeeded::text, '-') from spawn_commands where id = $1"; }
error_of() { q "select coalesce(error, '') from spawn_commands where id = $1"; }
finished() { [ "$(q "select state from spawn_commands where id = $1")" = finished ]; }
succeeded() { [ "$(state_of "$1")" = finished:true ]; }
# The Bridge's `command.step` records of command `$1` with step `$2`.
steps() {
    q "select count(*) from spawn_events
        where event_name = 'command.step' and source = 'bridge'
          and (kind -> 'detail' ->> 'command_id')::bigint = $1
          and kind -> 'detail' ->> 'step' = '$2'"
}
is_waiting() { [ "$(steps "$1" waiting)" -ge 1 ]; }
# Lines of the engine's log of kind `$1` whose text holds `$2`.
read_count() { grep -c "^$1 [0-9.]* .*$2" "$WORK/engine.log" || true; }
# The time of the first such line, and its line number.
read_at() { grep "^$1 [0-9.]* .*$2" "$WORK/engine.log" | head -1 | cut -d' ' -f2; }
line_of() { grep -n "^$1 [0-9.]* .*$2" "$WORK/engine.log" | head -1 | cut -d: -f1; }
logged() { [ "$(read_count "$1" "$2")" -ge 1 ]; }
before() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a + 0 <= b + 0) }'; }
# The pane showed Busy (the Bridge's own reading) after event `$1`.
busy_seen() {
    [ "$(q "select count(*) from spawn_events
            where session_id = '$SESSION' and event_name = 'screen.observed'
              and kind -> 'detail' ->> 'state' = 'busy' and id > $1")" -ge 1 ]
}
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
judged_waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$SESSION'")" = WAITING ]
}
no_typing_into_busy() {
    [ "$(read_count during '')" -eq 0 ] ||
        fail "$1: text was typed into a busy pane: $(grep '^during ' "$WORK/engine.log" | head -3 | tr '\n' ' ')"
}
# open_turn <key> <seconds> <tag>: a long turn, running and seen Busy.
open_turn() {
    mark=$(last_event)
    turn=$(say human "$1" "BB:BUSY $2 $3")
    wait_until 60 succeeded "$turn" ||
        harness "the input that opens turn $3 did not succeed at a ready pane ($(state_of "$turn") $(error_of "$turn"))"
    wait_until 30 logged busy-start "$3\$" || harness "turn $3 never started"
    wait_until 30 busy_seen "$mark" || harness "the Bridge never read the pane as Busy during turn $3"
}

# --- the resident, live and idle ------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "015 inputs wait"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name echo --charter "Owns the 015 inputs."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name echo
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
send bb-harness bb-start '{"type":"start","engine":"claude","prompt":""}' >/dev/null
start_loop
wait_until 120 judged_waiting || harness "the resident was never judged waiting at its prompt"

# --- (1) two inputs sent while a long turn runs wait ----------------------------------

open_turn bb-busy-first 45 first
one=$(say human bb-note-one "BB:NOTE one")
sleep 1
two=$(say spawn_supervisor bb-note-two "BB:NOTE two")
sleep 12
logged busy-end "first\$" && harness "the first turn ended before the inputs could be checked"
for id in "$one" "$two"; do
    if finished "$id"; then
        fail "(1) input $id, sent while the session's turn ran, finished $(state_of "$id") ($(error_of "$id")) instead of waiting for the turn to end"
    fi
    is_waiting "$id" ||
        fail "(1) input $id waits unrecorded: no command.step 'waiting' from the Bridge"
    assert_eq "$(steps "$id" waiting)" 1 "(1) the 'waiting' records of input $id"
done
assert_eq "$(read_count got 'BB:NOTE')" 0 "(1) notes the engine read while its turn ran"
no_typing_into_busy "(1)"

# --- (2) typed when the turn ends, in order, once; across a restart --------------------

# The waiting is a record, not the Bridge's memory: a restarted loop keeps it.
stop_loop
start_loop
wait_until 60 logged busy-end "first\$" || harness "the first turn never ended"
both_finished() { finished "$one" && finished "$two"; }
wait_until 180 both_finished ||
    fail "(2) the waiting inputs did not finish within 180 s of the turn's end (one: $(state_of "$one"), two: $(state_of "$two"))"
for id in "$one" "$two"; do
    succeeded "$id" || fail "(2) waiting input $id finished $(state_of "$id"): $(error_of "$id")"
    assert_eq "$(steps "$id" typed)" 1 "(2) the 'typed' records of input $id"
done
wait_until 30 logged note-end "two\$" || harness "the second note's turn never ended"
assert_eq "$(read_count got 'BB:NOTE one')" 1 "(2) times the engine read the first input"
assert_eq "$(read_count got 'BB:NOTE two')" 1 "(2) times the engine read the second input"
[ "$(line_of got 'BB:NOTE one')" -lt "$(line_of got 'BB:NOTE two')" ] ||
    fail "(2) the second input reached the engine before the first"
before "$(read_at busy-end 'first$')" "$(read_at got 'BB:NOTE one')" ||
    fail "(2) the first input reached the engine before the long turn ended"
before "$(read_at note-end 'one$')" "$(read_at got 'BB:NOTE two')" ||
    fail "(2) the second input reached the engine before the first one's turn ended"
no_typing_into_busy "(2)"

# --- (3) a stop first: the waiting input fails with the reason ---------------------------

wait_until 60 judged_waiting || harness "the resident was not judged waiting after the notes"
open_turn bb-busy-third 120 third
three=$(say human bb-note-three "BB:NOTE three")
wait_until 15 is_waiting "$three" ||
    fail "(1) input $three, sent during turn third, was not recorded waiting ($(state_of "$three") $(error_of "$three"))"
stop=$(send human bb-stop '{"type":"stop"}')
wait_until 60 finished "$stop" ||
    fail "(3) the stop was not handled within 60 s while an input waited ($(state_of "$stop"))"
succeeded "$stop" || fail "(3) the stop finished $(state_of "$stop"): $(error_of "$stop")"
logged busy-end "third\$" && harness "turn third ended before the stop: the stop proves nothing"
wait_until 30 finished "$three" ||
    fail "(3) input $three still waits ($(state_of "$three")) after its session was stopped"
succeeded "$three" && fail "(3) input $three succeeded though its session was stopped before its turn"
error=$(error_of "$three")
printf '%s' "$error" | grep -qi stopped ||
    fail "(3) input $three failed without saying the session was stopped: [$error]"
assert_eq "$(steps "$three" typing)" 0 "(3) the 'typing' records of input $three"
assert_eq "$(read_count got 'BB:NOTE three')" 0 "(3) times the engine read input three"
no_typing_into_busy "(3)"

# The session again, for the dispatcher's part (resumed by the harness, not
# as `human`).
send bb-harness bb-resume '{"type":"resume"}' >/dev/null
wait_until 120 judged_waiting || harness "the resumed resident was never judged waiting at its prompt"

# --- (4) the dispatcher's deliveries still land ----------------------------------------

open_turn bb-busy-second 30 second
four=$(say human bb-note-four "BB:NOTE four")
wait_until 15 is_waiting "$four" || fail "(1) input $four, sent during turn second, was not recorded waiting ($(state_of "$four") $(error_of "$four"))"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker echo \
    --brief "BB:NOTE task"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
[ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id"
delivered() { [ "$(q "select state from mission_tasks where id = $task")" = delivered ]; }
wait_until 60 logged busy-end "second\$" || harness "turn second never ended"
wait_until 240 delivered ||
    fail "(4) task #$task was not delivered within 240 s of the turn's end (state $(q "select state from mission_tasks where id = $task"); input four $(state_of "$four"))"
succeeded "$four" || fail "(4) waiting input $four finished $(state_of "$four"): $(error_of "$four")"
wait_until 30 logged note-end "task\$" || harness "the task's turn never ended"
assert_eq "$(read_count got 'BB:NOTE four')" 1 "(4) times the engine read input four"
assert_eq "$(read_count got 'BB:NOTE task')" 1 "(4) times the engine read the task"
[ "$(line_of got 'BB:NOTE four')" -lt "$(line_of got 'BB:NOTE task')" ] ||
    fail "(4) the task, sent after input four, reached the engine before it"
DELIVERIES="from spawn_commands where session_id = '$SESSION' and principal = 'spawn-dispatch'
              and action ->> 'type' = 'input' and idempotency_key like 'deliver-$task-%'"
assert_eq "$(q "select count(*) $DELIVERIES and state = 'finished' and succeeded")" 1 \
    "(4) deliveries of task #$task that succeeded"
refused=$(q "select count(*) $DELIVERIES and state = 'finished' and not succeeded
              and coalesce(error, '') not like '%does not accept input%'")
[ "$refused" -eq 0 ] ||
    fail "(4) a delivery attempt of task #$task failed otherwise than as refused at once: $(q "select string_agg(idempotency_key || ': ' || error, '; ') $DELIVERIES and not succeeded")"
dispatcher_waiting=$(q "select count(*) from spawn_events e join spawn_commands c
                         on c.id = (e.kind -> 'detail' ->> 'command_id')::bigint
                        where e.event_name = 'command.step' and e.kind -> 'detail' ->> 'step' = 'waiting'
                          and c.principal = 'spawn-dispatch'")
assert_eq "$dispatcher_waiting" 0 "(4) inputs of the dispatcher recorded waiting"
no_typing_into_busy "(4)"

echo "015: inputs sent to a busy session waited, were typed once each in order after the turn (across a restart), did not disturb the dispatcher's delivery, and failed with the reason when the session was stopped first"
exit 0
