#!/bin/sh
# Black box: a session's latest judgement covers its latest fact, whether or
# not the verdict changed, so a worker idle at its prompt stays actionable.
#
# A resident `core` runs live under `spawn loop` with a fake engine and no
# model judge, no supervisor, and nobody typing into it but the dispatcher.
# Its engine ends a turn the way Claude Code does: the transcript records the
# turn's end, then the screen settles at the prompt, so the Bridge records a
# screen reading after the Observer has already judged the worker WAITING.
# That order is set up, not raced: after a task turn's end the engine holds
# its screen until the harness has seen that WAITING judgement in the log and
# says so (a file in the scratch directory, not a command or a record of the
# session), however loaded the host is. The verdict does not change with the
# reading. Checked:
#   (1) coverage: within 10 s of a session's latest fact (any record of the
#       session that is not the Observer's), with no newer fact, the latest
#       observation's `based_on_seq` is at least that fact's `session_seq`,
#       and the worker is judged WAITING;
#   (2) consequence: the worker's report is collected and its next task is
#       delivered (typed in by the dispatcher) within 30 s, with no input
#       from anyone else;
#   (3) no churn while nothing happens: once covered, an idle worker with no
#       new fact gets no new observation for 20 s;
#   (4) no churn while work goes on: a turn of about 8 s with a tool call
#       every 0.4 s and an unchanged verdict gets at most 5 observations of
#       that session based on its facts, not one per fact.
# The scenario also checks its own premise: a screen reading recorded after
# the WAITING judgement that followed the turn's end. Setting it up is
# bounded: no WAITING judgement after the turn's end within 45 s, or an engine
# that does not settle its screen once told, is a premise that could not be
# set up, a harness error (2). Once the screen has settled after that
# judgement, a missing reading is the Bridge's doing (it stopped recording
# the screen, or reordered it) and fails with 1: facts are not to be dropped
# or reordered to hide the gap.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on an Observer that writes a judgement only when it
# changes. Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs
# SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own database,
# uses a private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "011: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_011_covered_$run"
SPAWN_TMUX_SOCKET="bb-011-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
cleanup() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
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
    if [ "$rc" -ne 0 ] && [ -s "$WORK/loop.log" ]; then
        echo "011: the loop's last lines:" >&2
        tail -n 10 "$WORK/loop.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "011: harness: unexpected exit $rc" >&2
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

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It writes its transcript where Claude Code does. A turn
# whose text names `BB:WORK <file>` is a task: about 8 s of tool calls (one
# every 0.4 s), then the file and the report. A turn ends as Claude Code's
# does: the transcript's turn_duration line first, then the prompt on the
# screen. After a task turn's turn_duration line the screen stays as it was
# until the harness creates `$BB_WORK/settle.<file>` (it does once the
# Observer has judged that turn's end WAITING), or for 120 s at most; the
# engine's log says which (`settled` or `unsettled`). Any other turn shows
# its prompt at once.
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
printf 'Welcome to Claude Code\n\n❯ '
pending=0
work=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:WORK "*) [ -n "$work" ] || work=${typed#*BB:WORK } ;;
    esac
  elif [ "$pending" = 1 ]; then
    held=
    if [ -n "$work" ]; then
      set -- $work
      file=$1
      printf 'busy %s %s\n' "$(date +%s)" "$file" >>"$BB_WORK/engine.log"
      for n in $(seq 1 20); do
        id="toolu_${file%%.*}_$n"
        line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}]}"
        printf 'tool %s\n' "$n"
        sleep 0.2
        line "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
        sleep 0.2
      done
      printf 'done\n' >"$file"
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      printf 'wrote %s %s\n' "$(date +%s)" "$file" >>"$BB_WORK/engine.log"
      held=$file
      work=
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    if [ -n "$held" ]; then
      deadline=$((SECONDS + 120))
      until [ -e "$BB_WORK/settle.$held" ] || [ "$SECONDS" -ge "$deadline" ]; do
        sleep 0.1
      done
    fi
    printf '\nidle\n❯ '
    if [ -n "$held" ]; then
      if [ -e "$BB_WORK/settle.$held" ]; then settled=settled; else settled=unsettled; fi
      printf '%s %s %s\n' "$settled" "$(date +%s)" "$held" >>"$BB_WORK/engine.log"
    fi
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

session=session_resident_core
agent=agent_resident_core

# wait_for <seconds> <command...>: true once the command is, false past the
# limit.
wait_for() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.25
    done
}

# The session's latest fact: its latest record that is not the Observer's.
latest_fact() {
    q "select coalesce(max(session_seq), 0) from spawn_events
        where session_id = '$session' and source <> 'observer'"
}
# The latest observation: `<session_seq> <based_on_seq> <state>`.
latest_observation() {
    q "select session_seq || ' ' || coalesce(kind -> 'detail' -> 'observation' ->> 'based_on_seq', '-1')
              || ' ' || coalesce(kind -> 'detail' -> 'observation' ->> 'state', '-')
         from spawn_events
        where session_id = '$session' and source = 'observer'
          and event_name = 'observation.recorded'
        order by session_seq desc limit 1"
}
observations() {
    q "select count(*) from spawn_events
        where session_id = '$session' and source = 'observer'
          and event_name = 'observation.recorded'"
}
# The latest observation covers the latest fact, and judges `$1`.
covered() {
    set -- "$1" $(latest_observation)
    [ "$#" -eq 4 ] && [ "$3" -ge "$(latest_fact)" ] && [ "$4" = "$1" ]
}
task_state() { q "select state from mission_tasks where id = $1"; }
is_state() { [ "$(task_state "$1")" = "$2" ]; }
engine_wrote() { grep -q "^wrote [0-9]* $1\$" "$WORK/engine.log"; }
# The turn of the task that wrote `$1` has ended (the turn's end recorded
# after the engine wrote the file).
turn_ended_after_busy() {
    [ "$(q "select count(*) from spawn_events
             where session_id = '$session' and event_name = 'turn.stopped'
               and session_seq > (select coalesce(max(session_seq), 0) from spawn_events
                                   where session_id = '$session' and event_name = 'tool.completed')")" -ge 1 ]
}
explain() {
    printf 'latest fact %s (%s); latest observation (seq based_on state) %s' \
        "$(latest_fact)" \
        "$(q "select event_name from spawn_events where session_id = '$session'
               and source <> 'observer' order by session_seq desc limit 1")" \
        "$(latest_observation)"
}

# --- a resident with a task, live under the loop ---------------------------------

capture "$SPAWN_BIN" mission add --text "011 covered judgements"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name core --charter "Owns the 011 concern."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name core
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"

assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker core \
        --brief "$1"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
task1=$(assign "BB:WORK one.txt")
[ -n "$task1" ] && [ "$task1" != null ] || harness "task assign printed no id"

# The resident's start, with no prompt: the dispatcher types the task in once
# the worker is idle at its prompt. Not submitted as `human`.
capture "$SPAWN_BIN" submit --principal bb-harness --agent "$agent" --session "$session" \
    --key bb-start-core --action '{"type":"start","engine":"claude","prompt":""}'
[ "$status" -eq 0 ] || harness "submit of the resident's start failed: $(err)"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# The first delivery reaches a worker that has done nothing yet: no turn has
# ended, so nothing of this scenario stands in its way.
wait_for 90 engine_wrote one.txt ||
    harness "task $task1 never reached the engine (state $(task_state "$task1"); $(explain))"
wait_for 30 turn_ended_after_busy || harness "the busy turn of task $task1 never ended"

# --- (4) no churn while work goes on ---------------------------------------------

first_tool=$(q "select min(session_seq) from spawn_events
                 where session_id = '$session' and event_name = 'tool.started'")
last_tool=$(q "select max(session_seq) from spawn_events
                where session_id = '$session' and event_name = 'tool.completed'")
tools=$(q "select count(*) from spawn_events where session_id = '$session'
            and event_name in ('tool.started', 'tool.completed')")
[ "$tools" -ge 30 ] || harness "the busy turn recorded $tools tool facts, not about 40"
busy_observations=$(q "select count(*) from spawn_events
    where session_id = '$session' and source = 'observer' and event_name = 'observation.recorded'
      and (kind -> 'detail' -> 'observation' ->> 'based_on_seq')::bigint between $first_tool and $last_tool")
[ "$busy_observations" -le 5 ] ||
    fail "(4) $busy_observations observations during one busy turn of $tools tool facts (at most 5)"

# --- the premise: a fact after the idle judgement --------------------------------

stopped=$(q "select max(session_seq) from spawn_events
              where session_id = '$session' and event_name = 'turn.stopped'")
waiting_after_stop() {
    [ "$(q "select count(*) from spawn_events
             where session_id = '$session' and source = 'observer'
               and event_name = 'observation.recorded' and session_seq > $stopped
               and kind -> 'detail' -> 'observation' ->> 'state' = 'WAITING'")" -ge 1 ]
}
# Premise waits, not the contract: generous, so a loaded host (other jobs
# share it) does not fail the setup; they return as soon as the fact is there.
wait_for 45 waiting_after_stop ||
    fail "the worker was never judged WAITING after its turn ended ($(explain))"
first_waiting=$(q "select min(session_seq) from spawn_events
                    where session_id = '$session' and source = 'observer'
                      and event_name = 'observation.recorded' and session_seq > $stopped
                      and kind -> 'detail' -> 'observation' ->> 'state' = 'WAITING'")
screen_after() {
    [ "$(q "select count(*) from spawn_events
             where session_id = '$session' and event_name = 'screen.observed'
               and session_seq > $first_waiting")" -ge 1 ]
}
wait_for 30 screen_after ||
    fail "no screen reading recorded after the WAITING judgement that followed the turn's end: the screen settling at the prompt is a fact and is recorded ($(explain))"

# --- (1) the judgement covers the latest fact -------------------------------------

wait_for 10 covered WAITING ||
    fail "(1) 10 s after the screen reading, the latest judgement does not cover the latest fact, or is not WAITING ($(explain))"

# --- (2) the dispatcher acts on it, unaided ---------------------------------------

wait_for 30 is_state "$task1" reported ||
    fail "(2) the idle worker's report was not collected within 30 s (task $task1: $(task_state "$task1"); $(explain))"
capture "$SPAWN_BIN" task accept --id "$task1"
[ "$status" -eq 0 ] || harness "task accept failed: $(err)"
task2=$(assign "BB:WORK two.txt")
wait_for 30 engine_wrote two.txt ||
    fail "(2) the next task was not delivered to the idle worker within 30 s (task $task2: $(task_state "$task2"); $(explain))"
others=$(q "select count(*) from spawn_commands where session_id = '$session'
             and principal not in ('spawn-dispatch', 'bb-harness')")
assert_eq "$others" 0 "(2) commands to the worker from anyone but the dispatcher"
assert_eq "$(q "select count(*) from spawn_commands where session_id = '$session'
                 and principal = 'bb-harness'")" 1 "(2) the harness's commands to the worker (its start)"

# --- (3) no churn while nothing happens -------------------------------------------

wait_for 30 turn_ended_after_busy || harness "the busy turn of task $task2 never ended"
wait_for 30 is_state "$task2" reported ||
    fail "(2) the second report was not collected within 30 s ($(explain))"
wait_for 15 covered WAITING ||
    fail "(1) after the second task, the latest judgement does not cover the latest fact ($(explain))"
facts_before=$(latest_fact)
seen_before=$(observations)
sleep 20
if [ "$(latest_fact)" = "$facts_before" ]; then
    assert_eq "$(observations)" "$seen_before" \
        "(3) observations written in 20 s without a new fact"
else
    # Something new happened (the engine is idle, so it should not); the
    # judgement must then cover it again.
    wait_for 10 covered WAITING ||
        fail "(1) a later fact is not covered ($(explain))"
fi

echo "011: an unchanged judgement covers the facts after it; the idle worker got its next task unaided; no churn"
exit 0
