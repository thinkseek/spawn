#!/bin/sh
# Black box: no task waits silently. A task whose step stops moving is
# escalated to the supervisor exactly once per stall, naming the task, the
# step and the evidence; healthy tasks beside it are not.
#
# A real `spawn loop` runs a supervisor and four residents, all on a fake
# engine, with no model judge. One mission (accept = review); the harness
# reviews like a supervisor would, accepting each healthy report as soon as
# it is collected, and types a line into the supervisor now and then (see
# `ping_supervisor`). The stall bounds are set short by the loop's
# environment: delivery 90 s, activity 30 s, everything else 600 s. Two
# stalls are injected from outside Spawn, never by editing it:
#   - `stuck`: the tmux wrapper refuses every paste-buffer and send-keys into
#     the stuck resident's pane, so its task's delivery never lands (each
#     attempt fails after the Bridge began typing; the dispatcher's own
#     retry, 60 s later, inside the bound, fails the same way, or is held
#     while the worker has no current judgement);
#   - `quiet`: its engine takes the task, opens the turn, and goes silent:
#     no transcript line, no screen change, the turn never ends.
# Beside them, `busy` works through one turn of 60 s with a tool call every
# 0.5 s (longer than the activity bound, never quiet for it) and `quick`
# takes 2 s; both report and are accepted.
# Checked, with "escalation" meaning a digest the dispatcher delivered to
# session_supervisor (an input command from spawn-dispatch that succeeded)
# carrying the tag `<session> stall-<task>-<step>-<n>`:
#   (1) the stuck delivery: exactly one escalation for its task, with step
#       `delivery`, between the bound and the bound + 30 s after the task's
#       assignment; its line names `task #<id>`, the step, the failed
#       attempt's key (`deliver-<id>-0`) and its error, and the key of every
#       other attempt that had failed by then (what Spawn already tried).
#       Later failed retries in the window add none (the step did not move);
#   (2) the silent worker: exactly one escalation for its task, with step
#       `activity`, between the bound and the bound + 30 s after the
#       session's latest fact; its text names `task #<id>`, the step, that
#       fact's event name and the state of the session's latest judgement;
#   (3) keyed by the stall: the silent worker makes one tool call and goes
#       silent again; that stall gets exactly one escalation of its own, a
#       new tag, and the first one is not sent again;
#   (4) none for healthy tasks: no stall tag for `busy` or `quick`, which
#       both report and are accepted.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Holds whether or not objective 011 is solved (the pings, and nothing
# checked rests on a fresh judgement of a stalled worker). Fails with 1 at
# (1) on c98c1e8, which has no stall escalation: the supervisor hears
# nothing of either stall. Runs inside the gate's sandbox
# (scripts/rsi/gate.sh): needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL,
# creates and drops its own database, uses a private tmux socket, no
# network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "013: harness: $*" >&2
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
SPAWN_DB_NAME="bb_013_stalls_$run"
SPAWN_TMUX_SOCKET="bb-013-$run"
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
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "013: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "013: the digests the supervisor received (id, tags):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, action -> 'tags' from spawn_commands
                where session_id = 'session_supervisor' and principal = 'spawn-dispatch'
                  and action ->> 'type' = 'input' and succeeded order by id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "013: harness: unexpected exit $rc" >&2
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
# second of quiet, as Claude Code's does: the transcript's end of the turn
# first, then the prompt on the screen. It
# writes its transcript where Claude Code does. A turn whose text names
# `BB:WORK <file> <seconds>` is a task: a tool call every 0.5 s for that
# long, then the file and the report. One that names `BB:SILENT <name>`
# opens the turn and goes silent for good: nothing more in the transcript
# or on the screen, except one tool call each time the harness creates
# $BB_WORK/wake-<name>.
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
tool() {
  line "\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"$1\",\"name\":\"Bash\",\"input\":{}}]}"
  sleep 0.25
  line "\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$1\",\"content\":\"ok\"}]}"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
work=
silent=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:WORK "*) [ -n "$work" ] || work=${typed#*BB:WORK } ;;
    *"BB:SILENT "*) [ -n "$silent" ] || silent=${typed#*BB:SILENT } ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$silent" ]; then
      set -- $silent
      name=$1
      printf 'silent %s %s\n' "$(date +%s)" "$name" >>"$BB_WORK/engine.log"
      wakes=0
      while :; do
        if [ -e "$BB_WORK/wake-$name" ]; then
          rm -f "$BB_WORK/wake-$name"
          wakes=$((wakes + 1))
          tool "toolu_${name}_wake_$wakes"
          printf 'woke %s %s %s\n' "$(date +%s)" "$name" "$wakes" >>"$BB_WORK/engine.log"
        fi
        sleep 0.5
      done
    fi
    if [ -n "$work" ]; then
      set -- $work
      file=$1
      seconds=$2
      printf 'busy %s %s\n' "$(date +%s)" "$file" >>"$BB_WORK/engine.log"
      end=$(($(date +%s) + seconds))
      n=0
      while [ "$(date +%s)" -lt "$end" ]; do
        n=$((n + 1))
        tool "toolu_${file%%.*}_$n"
        printf 'tool %s\n' "$n"
        sleep 0.25
      done
      printf 'done\n' >"$file"
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      printf 'wrote %s %s\n' "$(date +%s)" "$file" >>"$BB_WORK/engine.log"
      work=
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# tmux on the private socket. Every pane named in $WORK/refused (one tmux
# pane target per line, `=<terminal>:`) refuses input: paste-buffer and
# send-keys into it fail, as a pane that never takes the text would.
: >"$WORK/refused"
# (Spawn puts its own `-L <socket>` before the subcommand.)
cat >"$WORK/tmux" <<EOF
#!/bin/sh
input=
for argument in "\$@"; do
  case "\$argument" in paste-buffer | send-keys) input=\$argument ;; esac
done
if [ -n "\$input" ]; then
  for argument in "\$@"; do
    if [ -n "\$argument" ] && grep -qxF -- "\$argument" '$WORK/refused'; then
      echo "\$input refused by the scenario" >&2
      exit 1
    fi
  done
fi
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS; do
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
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# The stall bounds: short for the two steps under test, long for the rest
# (the harness reviews at once; no gate, no halt, no cancel).
DELIVERY_BOUND=90
BOUND=30
SLACK=30
SPAWN_STALL_DELIVERY_SECS=$DELIVERY_BOUND
SPAWN_STALL_ACTIVITY_SECS=$BOUND
SPAWN_STALL_REVIEW_SECS=600
SPAWN_STALL_GATE_SECS=600
SPAWN_STALL_LANDING_SECS=600
SPAWN_STALL_STOP_SECS=600
SPAWN_STALL_HALTED_SECS=600
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB \
    SPAWN_DISK_FLOOR_GB SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"
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
db_now_ms() { q "select spawn_now_ms()"; }

# wait_for <seconds> <command...>: true once the command is, false past the
# limit.
wait_for() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        tend
        sleep 0.5
    done
}

# The session is judged waiting at its prompt, live.
waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}
task_state() { q "select state from mission_tasks where id = $1"; }

# The digests delivered to the supervisor that carry a stall tag for task
# `$1` (any step, or step `$2`): one row per (command, tag).
STALL_DIGESTS="select c.id, c.created_at_ms, tag
                 from spawn_commands c,
                      lateral jsonb_array_elements_text(
                          case when jsonb_typeof(c.action -> 'tags') = 'array'
                               then c.action -> 'tags' else '[]'::jsonb end) tag
                where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
                  and c.action ->> 'type' = 'input' and c.state = 'finished' and c.succeeded"
stall_digests() {
    session=$(q "select session_id from mission_tasks where id = $1")
    q "select count(*) from ($STALL_DIGESTS) d
        where d.tag like '$session stall-$1-${2:-}%'"
}
stall_tags() {
    session=$(q "select session_id from mission_tasks where id = $1")
    q "select count(distinct d.tag) from ($STALL_DIGESTS) d where d.tag like '$session stall-$1-%'"
}
escalated() { [ "$(stall_digests "$1" "$2")" -ge 1 ]; }
# Of the stall digests for task `$1`, the `$2`-th (1-based, oldest first):
# its command id.
nth_digest() {
    session=$(q "select session_id from mission_tasks where id = $1")
    q "select d.id from ($STALL_DIGESTS) d where d.tag like '$session stall-$1-%'
        order by d.id offset $(($2 - 1)) limit 1"
}
digest_at() { q "select created_at_ms from spawn_commands where id = $1"; }
digest_text() { sql "select action ->> 'text' from spawn_commands where id = $1"; }
# The line of digest `$1` about task `$2`: every line naming `task #<id>`.
digest_lines() { digest_text "$1" | grep -F "task #$2" || true; }
digest_tag() {
    session=$(q "select session_id from mission_tasks where id = $2")
    q "select tag from jsonb_array_elements_text((select action -> 'tags' from spawn_commands
                                                   where id = $1)) tag
        where tag like '$session stall-$2-%' limit 1"
}

# The record that assigned task `$1`: its time.
assigned_at() {
    q "select recorded_at_ms from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
# The latest fact of session `$1` recorded at or before `$2` (ms): a record
# that is not the Observer's, nor storage's. Prints `<recorded_at_ms> <event_name>`.
latest_fact_before() {
    q "select recorded_at_ms || ' ' || event_name from spawn_events
        where session_id = '$1' and source <> 'observer'
          and event_name not in ('artifact.committed', 'merge.conflicted')
          and recorded_at_ms <= $2
        order by session_seq desc limit 1"
}
# The state of the latest judgement of session `$1` recorded at or before `$2`.
judged_before() {
    q "select kind -> 'detail' -> 'observation' ->> 'state' from spawn_events
        where session_id = '$1' and source = 'observer' and event_name = 'observation.recorded'
          and recorded_at_ms <= $2
        order by session_seq desc limit 1"
}

# The supervisor gets a digest only while its latest judgement covers its
# latest fact. An Observer that writes a judgement only when it changes
# (objective 011 not solved) leaves an idle supervisor's stale at the first
# record after it, so a line is typed into it now and then (not as `human`:
# no news, and nothing the stall rules read). With 011 solved it is harmless.
pings=0
next_ping=0
ping_supervisor() {
    [ "$(now)" -ge "$next_ping" ] || return 0
    next_ping=$(($(now) + 8))
    pings=$((pings + 1))
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor \
        --session session_supervisor --key "bb-ping-$pings" \
        --action '{"type":"input","text":"ping"}'
    [ "$status" -eq 0 ] || harness "submit of a ping to the supervisor failed: $(err)"
}

# The harness is the reviewer: a healthy task's report is accepted as soon as
# it is collected. Called while waiting, with the pings.
healthy=""
tend() {
    [ -z "$healthy" ] || ping_supervisor
    for id in $healthy; do
        if [ "$(task_state "$id")" = reported ]; then
            capture "$SPAWN_BIN" task accept --id "$id"
            [ "$status" -eq 0 ] || harness "task accept of #$id failed: $(err)"
        fi
    done
}

# --- the residents, live and idle --------------------------------------------------

capture "$SPAWN_BIN" mission add --text "013 stalls"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"

for name in stuck quiet busy quick; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 013 $name concern."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
    # Started with no prompt: tasks are typed in by the dispatcher. Not
    # submitted as `human`: a human's command is news for the supervisor.
    capture "$SPAWN_BIN" submit --principal bb-harness --agent "agent_resident_$name" \
        --session "session_resident_$name" --key "bb-start-$name" \
        --action '{"type":"start","engine":"claude","prompt":""}'
    [ "$status" -eq 0 ] || harness "submit of $name's start failed: $(err)"
    command=$(sed -n 's/^command=//p' "$WORK/out.txt")
    [ -n "$command" ] || harness "submit of $name's start printed no command id: $(out)"
    eval "start_$name=$command"
done
# The stuck resident's pane refuses input from the start; its start has no
# prompt to type, so it starts all the same.
printf '=spawn-exec_%s:\n' "$start_stuck" >"$WORK/refused"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

for name in stuck quiet busy quick; do
    wait_for 120 waiting "session_resident_$name" ||
        harness "resident $name was never judged waiting at its prompt"
done
wait_for 90 waiting session_supervisor || harness "the supervisor was never judged waiting"

# --- the tasks ---------------------------------------------------------------------

assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
task_stuck=$(assign stuck "BB:WORK stuck.txt 2")
task_quiet=$(assign quiet "BB:SILENT quiet")
task_busy=$(assign busy "BB:WORK busy.txt 60")
task_quick=$(assign quick "BB:WORK quick.txt 2")
for id in "$task_stuck" "$task_quiet" "$task_busy" "$task_quick"; do
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
done
healthy="$task_busy $task_quick"
started=$(now)

# The premises: the stuck delivery failed at the pane, the quiet worker took
# its task and went silent, the healthy ones were delivered.
first_failed() {
    [ "$(q "select count(*) from spawn_commands
             where idempotency_key = 'deliver-$task_stuck-0' and state = 'finished'
               and not succeeded")" -ge 1 ]
}
wait_for 60 first_failed ||
    harness "the stuck task's first delivery did not fail (state $(task_state "$task_stuck"))"
q "select coalesce(error, '') from spawn_commands where idempotency_key = 'deliver-$task_stuck-0'" |
    grep -q 'refused by the scenario' ||
    harness "the stuck task's first delivery failed for another reason"
went_silent() { grep -q "^silent [0-9]* quiet\$" "$WORK/engine.log"; }
wait_for 60 went_silent || harness "the quiet worker never took its task"
is_past_assigned() { [ "$(task_state "$1")" != assigned ]; }
for id in $healthy; do
    wait_for 60 is_past_assigned "$id" || harness "healthy task #$id was never delivered"
done

# --- (1) the stuck delivery ---------------------------------------------------------

# The dispatcher's own retry of the delivery (60 s after the failure) is
# its recovery, inside the bound; the escalation comes once the bound is
# behind, whether that retry ran (and failed) or is held (a worker with no
# current judgement gets nothing typed), and names what was tried.
wait_for $((DELIVERY_BOUND + SLACK + 30)) escalated "$task_stuck" delivery ||
    fail "(1) no escalation of task #$task_stuck stuck at its delivery within $((DELIVERY_BOUND + SLACK + 30)) s (state $(task_state "$task_stuck"), stall digests for it: $(stall_digests "$task_stuck"))"
digest=$(nth_digest "$task_stuck" 1)
at=$(digest_at "$digest")
assigned=$(assigned_at "$task_stuck")
[ $((at - assigned)) -ge $((DELIVERY_BOUND * 1000 - 1000)) ] ||
    fail "(1) task #$task_stuck was escalated $(((at - assigned) / 1000)) s after its assignment, before the ${DELIVERY_BOUND} s bound"
[ $((at - assigned)) -le $(((DELIVERY_BOUND + SLACK) * 1000)) ] ||
    fail "(1) task #$task_stuck was escalated $(((at - assigned) / 1000)) s after its assignment (bound $DELIVERY_BOUND s + $SLACK s)"
lines=$(digest_lines "$digest" "$task_stuck")
[ -n "$lines" ] || fail "(1) the escalation's text does not name task #$task_stuck: $(digest_text "$digest")"
for word in delivery "deliver-$task_stuck-0" "refused by the scenario"; do
    printf '%s\n' "$lines" | grep -qF -- "$word" ||
        fail "(1) the escalation of task #$task_stuck does not name '$word': $lines"
done
# Every attempt that had failed by then is named: what Spawn already tried.
for key in $(q "select idempotency_key from spawn_commands
                 where idempotency_key like 'deliver-$task_stuck-0%' and state = 'finished'
                   and finished_at_ms < $at"); do
    printf '%s\n' "$lines" | grep -qF -- "$key" ||
        fail "(1) the escalation of task #$task_stuck does not name the failed attempt $key: $lines"
done
case "$(digest_tag "$digest" "$task_stuck")" in
"session_resident_stuck stall-$task_stuck-delivery-"?*) ;;
*) fail "(1) the escalation's tag is not '<session> stall-<task>-delivery-<n>': $(digest_tag "$digest" "$task_stuck")" ;;
esac

# --- (2) the silent worker -----------------------------------------------------------

wait_for 120 escalated "$task_quiet" activity ||
    fail "(2) no escalation of task #$task_quiet stuck waiting for its worker's activity within 120 s (stall digests for it: $(stall_digests "$task_quiet"))"
digest=$(nth_digest "$task_quiet" 1)
at=$(digest_at "$digest")
set -- $(latest_fact_before session_resident_quiet "$at")
[ "$#" -eq 2 ] || harness "the quiet session has no fact before its escalation"
fact_at=$1
fact_name=$2
state=$(judged_before session_resident_quiet "$at")
[ $((at - fact_at)) -ge $((BOUND * 1000 - 1000)) ] ||
    fail "(2) task #$task_quiet was escalated $(((at - fact_at) / 1000)) s after its worker's latest fact ($fact_name), before the ${BOUND} s bound"
[ $((at - fact_at)) -le $(((BOUND + SLACK) * 1000)) ] ||
    fail "(2) task #$task_quiet was escalated $(((at - fact_at) / 1000)) s after its worker's latest fact (bound $BOUND s + $SLACK s)"
lines=$(digest_lines "$digest" "$task_quiet")
[ -n "$lines" ] || fail "(2) the escalation's text does not name task #$task_quiet: $(digest_text "$digest")"
for word in activity "$fact_name" "$state"; do
    printf '%s\n' "$lines" | grep -qF -- "$word" ||
        fail "(2) the escalation of task #$task_quiet does not name '$word' (the latest fact and judgement): $lines"
done
case "$(digest_tag "$digest" "$task_quiet")" in
"session_resident_quiet stall-$task_quiet-activity-"?*) ;;
*) fail "(2) the escalation's tag is not '<session> stall-<task>-activity-<n>': $(digest_tag "$digest" "$task_quiet")" ;;
esac
first_quiet_tag=$(digest_tag "$digest" "$task_quiet")

# --- (3) it moved, and stalled anew ----------------------------------------------------

# Hold a while first: the same stall is not escalated again.
hold_until=$(($(now) + 20))
while [ "$(now)" -lt "$hold_until" ]; do
    tend
    sleep 0.5
done
assert_eq "$(stall_digests "$task_quiet")" 1 "(3) escalations of task #$task_quiet before it moved"
: >"$WORK/wake-quiet"
woke() { grep -q "^woke [0-9]* quiet 1\$" "$WORK/engine.log"; }
wait_for 30 woke || harness "the quiet worker did not make its tool call"
two_tags() { [ "$(stall_tags "$task_quiet")" -ge 2 ]; }
wait_for $((BOUND + SLACK + 30)) two_tags ||
    fail "(3) task #$task_quiet moved (one tool call) and stalled again, but was not escalated again within $((BOUND + SLACK + 30)) s"
second=$(nth_digest "$task_quiet" 2)
second_tag=$(digest_tag "$second" "$task_quiet")
[ "$second_tag" != "$first_quiet_tag" ] ||
    fail "(3) the new stall of task #$task_quiet reused the first one's tag ($second_tag)"
case "$second_tag" in
"session_resident_quiet stall-$task_quiet-activity-"?*) ;;
*) fail "(3) the second escalation's tag is not '<session> stall-<task>-activity-<n>': $second_tag" ;;
esac
woke_at=$(q "select max(recorded_at_ms) from spawn_events
              where session_id = 'session_resident_quiet' and event_name = 'tool.completed'")
[ "$(digest_at "$second")" -ge $((woke_at + BOUND * 1000 - 1000)) ] ||
    fail "(3) the new stall of task #$task_quiet was escalated before the bound after its new fact"

# --- (4) the window closes: exactly one per stall, none for the healthy ----------------

# At least 240 s after the assignments: the stuck delivery's second retry
# (180 s after its first failure) fails within it, and moves nothing.
hold_until=$((started + 240))
[ "$(($(now) + 20))" -gt "$hold_until" ] && hold_until=$(($(now) + 20))
while [ "$(now)" -lt "$hold_until" ]; do
    tend
    sleep 0.5
done
is_accepted() { [ "$(task_state "$1")" = accepted ]; }
for id in $healthy; do
    wait_for 60 is_accepted "$id" ||
        fail "(4) healthy task #$id was not reported and accepted (state $(task_state "$id"))"
done
assert_eq "$(stall_digests "$task_stuck")" 1 "(1) escalations of the stuck delivery of task #$task_stuck"
assert_eq "$(stall_tags "$task_stuck")" 1 "(1) stall tags for task #$task_stuck"
assert_eq "$(stall_digests "$task_quiet")" 2 "(3) escalations of task #$task_quiet (two stalls)"
assert_eq "$(stall_tags "$task_quiet")" 2 "(3) stall tags for task #$task_quiet"
for id in $healthy; do
    assert_eq "$(stall_digests "$id")" 0 "(4) escalations of healthy task #$id"
done
others=$(q "select count(*) from ($STALL_DIGESTS) d
             where d.tag like '% stall-%'
               and d.tag not like 'session_resident_stuck stall-$task_stuck-%'
               and d.tag not like 'session_resident_quiet stall-$task_quiet-%'")
assert_eq "$others" 0 "(4) stall escalations about anything but the two stalled tasks"
[ "$(task_state "$task_stuck")" = assigned ] ||
    harness "the stuck task left 'assigned' ($(task_state "$task_stuck")): the wrapper let it through"
[ "$(task_state "$task_quiet")" = delivered ] ||
    harness "the quiet task left 'delivered' ($(task_state "$task_quiet"))"

echo "013: the stuck delivery and the silent worker were each escalated once per stall, within the bound, naming the task, the step and the evidence; the healthy tasks were not"
exit 0
