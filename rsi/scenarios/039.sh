#!/bin/sh
# Black box: a worker whose engine ends on a transient model API error is
# retried without a human, a limited number of times, and the supervisor is
# told once when the retries are spent; one that ends on any other model
# error is never retried, and the supervisor is told once.
#
# Today (recover.rs) a failed turn whose error is transient (429, 500, 503,
# 529, overloaded, rate_limit, server_error) waits and is retried after the
# model backoff, resumed when its engine ended, for as long as it keeps
# failing: nobody is told. A failed turn with any other status (400, ...) is
# no cause at all, so the engine's end after it reads `crashed` and is
# resumed, again and again. Required: an execution that ended after a
# transient model error is resumed after the model backoff at most
# SPAWN_RECOVER_MODEL_RETRIES times in a row without a model answer (3 by
# default; 2 here); the next such end is recorded `execution.interrupted`
# cause `model_unavailable`, path `final` (no revival); an execution that
# ended after a non-transient model error (no progress after it) is recorded
# cause `model_error`, path `final`, and never revived; each final model
# record is told to the supervisor once by the dispatcher's digest, tagged
# `<session> model-<execution id>`.
#
# A real `spawn loop` (the Bridge, the dispatcher, the supervisor, the
# recovery; no model judge) runs three ephemeral workers of one mission,
# each with a delivered task, and the supervisor, on the fake engine below,
# whose plan per agent says what each of its turns does. Settings:
# SPAWN_RECOVER_MODEL_BACKOFF_SECS=2, SPAWN_RECOVER_MODEL_RETRIES=2.
# Checked:
#   (1) transient, then fine: a worker's engine ends twice on a 529, then
#       does its task: the task is reported; two `execution.interrupted`
#       records name cause `model_unavailable` and none `crashed` or
#       `exited`; two revivals, each a `resume` of the same native session
#       by the recovery (`spawn-liveness`, key `recover-...`), each sent at
#       least the 2 s backoff after the failed turn it retries; the third
#       execution finds what the first two wrote in the workspace; no
#       command into the worker from a person or the supervisor, and no
#       digest tag `<its session> model-...`.
#   (2) not transient: a worker's engine ends on a 400 (an invalid
#       request): one `execution.interrupted`, cause `model_error`, path
#       `final`; within 20 s of its end no revival (the worker's engine ran
#       once, one execution); the supervisor is told once (one digest tag
#       `<its session> model-<execution>`, still one 15 s later).
#   (3) retries spent: a worker's engine ends on a 529 every time: two
#       revivals, then the third end is recorded cause `model_unavailable`,
#       path `final`; no third revival within 20 s (three executions in
#       all); the supervisor is told once (one digest tag `<its session>
#       model-<execution>` naming the third execution, still one 15 s
#       later).
#   (4) the setting is checked: with the loop stopped, `spawn loop` with
#       SPAWN_RECOVER_MODEL_RETRIES set to 0, 21, -1 or `two` exits non-zero
#       within 15 s, naming the setting.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (2) on the live main (a484b2c7): the 400's end is
# recorded `crashed`, path `resume`, and the worker is resumed at once. It
# passes once a non-transient model error's end is final and told, and a
# transient one's resumes are counted and capped, the cap told.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.

set -eu


asserted=""
harness() {
    echo "039: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_039_retry_$run"
SPAWN_TMUX_SOCKET="bb-039-$run"
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
        if [ -s "$WORK/loop.log" ]; then
            echo "039: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "039: what the engines read (last lines):" >&2
            tail -n 20 "$WORK/engine.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "039: the workers' commands (id, session, principal, key, type, state, succeeded, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, session_id, principal, idempotency_key,
                    action ->> 'type', state, succeeded, left(coalesce(error, ''), 120)
                from spawn_commands where session_id <> 'session_supervisor' order by id" \
                >&2 2>/dev/null || true
            echo "039: the recovery's records (id, session, type, cause, path, attempt, due):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, kind -> 'detail' -> 'record' ->> 'session_id',
                    kind -> 'detail' -> 'record' ->> 'type', kind -> 'detail' -> 'record' ->> 'cause',
                    kind -> 'detail' -> 'record' ->> 'path', kind -> 'detail' -> 'record' ->> 'attempt',
                    kind -> 'detail' -> 'record' ->> 'due_ms'
                from spawn_events where event_name = 'control.recorded'
                 and kind -> 'detail' -> 'record' ->> 'type' like 'execution.%' order by id" \
                >&2 2>/dev/null || true
            if [ -s "$WORK/turns.log" ]; then
                echo "039: the engines' turns:" >&2
                cat "$WORK/turns.log" >&2
            fi
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "039: harness: unexpected exit $rc" >&2
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
# after a second of quiet. An agent with a plan ($BB_WORK/ctl/<agent>.plan,
# one step per line, the n-th turn of the agent across its executions takes
# the n-th line, the last line once they run out) acts it out; every other
# (the supervisor) ends the turn at once. Each turn first appends `turn <n>`
# to notes.txt in its workspace, so what earlier executions wrote shows. The
# steps: `529` and `400` write the API error line Claude Code writes to its
# transcript (isApiErrorMessage, error, apiErrorStatus, its text) and show
# it, then the engine exits 1 two seconds later (an engine that ends on the
# model's error); `work` runs a tool call, writes the report and ends the
# turn (the task done). Each turn is logged to turns.log: time, agent,
# execution, n, step, the lines notes.txt held before it and the native
# session it runs in.
cat >"$WORK/claude" <<'ENGINE'
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
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
api_error() { # <error> <status> <text>
  text=$(printf '%s' "$3" | jq -Rs .)
  line "\"type\":\"assistant\",\"isApiErrorMessage\":true,\"error\":\"$1\",\"apiErrorStatus\":$2,\"message\":{\"id\":\"msg_$count\",\"model\":\"<synthetic>\",\"role\":\"assistant\",\"type\":\"message\",\"stop_reason\":\"stop_sequence\",\"content\":[{\"type\":\"text\",\"text\":$text}]}"
  printf '\nclaude: %s\n\n❯ ' "$3"
}
turn() {
  plan=$BB_WORK/ctl/$agent.plan
  if [ ! -f "$plan" ]; then
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    return
  fi
  counter=$BB_WORK/ctl/$agent.n
  n=$(cat "$counter" 2>/dev/null || echo 0)
  n=$((n + 1))
  printf '%s\n' "$n" >"$counter"
  step=$(sed -n "${n}p" "$plan")
  [ -n "$step" ] || step=$(tail -n 1 "$plan")
  held=0
  [ -f notes.txt ] && held=$(wc -l <notes.txt)
  printf 'turn %s\n' "$n" >>notes.txt
  printf '%s %s %s %s %s %s %s\n' "$(date +%s%3N)" "$agent" "$SPAWN_EXECUTION_ID" "$n" "$step" "$held" \
    "$native" >>"$BB_WORK/turns.log"
  case "$step" in
  529)
    api_error overloaded 529 'API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}'
    sleep 2
    exit 1
    ;;
  400)
    api_error invalid_request 400 'API Error: 400 {"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: unexpected tool_use_id found in tool_result blocks"}}'
    sleep 2
    exit 1
    ;;
  *)
    id="toolu_${SPAWN_EXECUTION_ID}_$count"
    line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
    printf 'done\n' >result.txt
    sleep 0.3
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
    mkdir -p .spawn
    printf '## Outcome\ndone\n## What changed\nresult.txt\n## Open issues\nnone\n' >.spawn/report.md
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    ;;
  esac
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    printf 'got %s %s %s\n' "$(date +%s.%N)" "$agent" "$typed" >>"$BB_WORK/engine.log"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
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
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The supervisor and the dispatcher run, as on the host: the dispatcher's
# digest is how the supervisor is told.
SPAWN_SUPERVISOR=1
# The recovery's settings, short for the scenario (their defaults: 10 s,
# 60 s, 3 retries): the resume backoff's base, the model retry backoff's
# base, and the retries of an execution that ended on a transient model
# error before the supervisor is told instead.
SPAWN_RECOVER_BACKOFF_SECS=2
SPAWN_RECOVER_MODEL_BACKOFF_SECS=2
SPAWN_RECOVER_MODEL_RETRIES=2
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
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
    SPAWN_MISSION_FILE SPAWN_RECOVER_BACKOFF_SECS SPAWN_RECOVER_MODEL_BACKOFF_SECS \
    SPAWN_RECOVER_MODEL_RETRIES SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engine.log"
: >"$WORK/turns.log"
mkdir -p "$WORK/ctl"

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

# The recovery's records of a session: `<cause> <path>` lines, in order.
RECORDS="select kind -> 'detail' -> 'record' ->> 'cause', kind -> 'detail' -> 'record' ->> 'path',
                kind -> 'detail' -> 'record' ->> 'execution_id', id
           from spawn_events
          where event_name = 'control.recorded'
            and kind -> 'detail' -> 'record' ->> 'type' = 'execution.interrupted'"
records_of() { sql "select c || ' ' || p from ($RECORDS) r(c, p, e, id) where r.e in
                    (select execution_id from agent_executions where session_id = '$1') order by id"; }
count_cause() { q "select count(*) from ($RECORDS) r(c, p, e, id) where c = '$2' and r.e in
                    (select execution_id from agent_executions where session_id = '$1')"; }
final_of() { # <session> <cause>: the execution of its final record of that cause
    q "select string_agg(e, ' ' order by id) from ($RECORDS) r(c, p, e, id) where c = '$2' and p = 'final' and r.e in
        (select execution_id from agent_executions where session_id = '$1')"
}
executions_of() { q "select count(*) from agent_executions where session_id = '$1'"; }
execution_n() { # <session> <n>: its n-th execution, from 1
    q "select execution_id from agent_executions where session_id = '$1'
        order by started_at_ms, execution_id offset $(($2 - 1)) limit 1"
}
ended() { [ -n "$(q "select ended_at_ms from agent_executions where execution_id = '$1' and ended_at_ms is not null")" ]; }
# The recovery's revivals into a session: id, type, created_at_ms.
revivals() { sql "select id || ' ' || (action ->> 'type') || ' ' || created_at_ms from spawn_commands
                   where session_id = '$1' and principal = 'spawn-liveness' and idempotency_key like 'recover-%'
                     and action ->> 'type' in ('resume', 'start', 'input') order by id"; }
revival_count() { q "select count(*) from spawn_commands where session_id = '$1'
                      and principal = 'spawn-liveness' and idempotency_key like 'recover-%'
                      and action ->> 'type' in ('resume', 'start', 'input')"; }
turns_of() { grep -c " $1 " "$WORK/turns.log" || true; }
# The digests delivered to the supervisor whose tags match `$1` (SQL like).
DIGEST_TAGS="select c.id, tag from spawn_commands c,
                lateral jsonb_array_elements_text(
                    case when jsonb_typeof(c.action -> 'tags') = 'array'
                         then c.action -> 'tags' else '[]'::jsonb end) tag
              where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
                and c.action ->> 'type' = 'input' and c.state = 'finished' and c.succeeded"
digests_like() { q "select count(*) from ($DIGEST_TAGS) d where d.tag like '$1'"; }
reported() { [ "$(q "select state from mission_tasks where id = $1")" = reported ]; }

# --- the workers -------------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "039 transient model errors"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
for label in transient refused spent; do
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
        --brief "Do the 039 task and report."
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    agent=$(q "select agent_id from mission_tasks where id = $task")
    session=$(q "select session_id from mission_tasks where id = $task")
    [ -n "$agent" ] && [ -n "$session" ] || harness "task $task ($label) names no agent or session"
    eval "task_$label=\$task agent_$label=\$agent session_$label=\$session"
done
printf '529\n529\nwork\n' >"$WORK/ctl/$agent_transient.plan"
printf '400\nwork\n' >"$WORK/ctl/$agent_refused.plan"
printf '529\n' >"$WORK/ctl/$agent_spent.plan"
start_loop

# --- (1) transient twice, then the task is done -------------------------------------

wait_until 150 reported "$task_transient" ||
    fail "(1) the task of the worker whose engine ended twice on a 529 was not reported within 150 s (task $(q "select state from mission_tasks where id = $task_transient"); its records: $(records_of "$session_transient" | tr '\n' ',') turns: $(turns_of "$agent_transient"))"
assert_eq "$(awk -v a="$agent_transient" '$2 == a { printf "%s ", $5 }' "$WORK/turns.log")" "529 529 work " \
    "(1) the transient worker's turns"
assert_eq "$(awk -v a="$agent_transient" '$2 == a { print $3 }' "$WORK/turns.log" | sort -u | wc -l | tr -d ' ')" 3 \
    "(1) executions the transient worker's three turns ran in"
assert_eq "$(awk -v a="$agent_transient" '$2 == a && $5 == "work" { print $6 }' "$WORK/turns.log")" 2 \
    "(1) lines of notes.txt the working execution found (what the two failed ones wrote, kept)"
assert_eq "$(count_cause "$session_transient" model_unavailable)" 2 \
    "(1) model_unavailable interruptions of the transient worker ($(records_of "$session_transient" | tr '\n' ','))"
for cause in crashed exited; do
    assert_eq "$(count_cause "$session_transient" "$cause")" 0 \
        "(1) $cause interruptions of the transient worker ($(records_of "$session_transient" | tr '\n' ','))"
done
assert_eq "$(revival_count "$session_transient")" 2 "(1) the recovery's revivals of the transient worker ($(revivals "$session_transient" | tr '\n' ','))"
revivals "$session_transient" >"$WORK/revivals.txt"
while read -r id type created; do
    assert_eq "$type" resume "(1) the type of revival $id"
    failed=$(q "select max(recorded_at_ms) from spawn_events where session_id = '$session_transient'
                 and event_name = 'turn.failed' and recorded_at_ms <= $created")
    [ -n "$failed" ] || fail "(1) no failed turn before revival $id"
    [ $((created - failed)) -ge 1900 ] ||
        fail "(1) revival $id was sent $((created - failed)) ms after the failed turn it retries: the backoff is 2 s"
done <"$WORK/revivals.txt"
assert_eq "$(awk -v a="$agent_transient" '$2 == a { print $7 }' "$WORK/turns.log" | sort -u | wc -l | tr -d ' ')" 1 \
    "(1) native sessions the transient worker's turns ran in (each retry a resume of the same one)"
assert_eq "$(q "select count(*) from spawn_commands where session_id = '$session_transient'
                  and principal in ('human', 'spawn_supervisor')")" 0 \
    "(1) commands into the transient worker from a person or the supervisor"
assert_eq "$(digests_like "$session_transient model-%")" 0 "(1) digests telling the supervisor of the transient worker's model errors"

# --- (2) not transient: never retried, told once --------------------------------------------

first=$(execution_n "$session_refused" 1)
[ -n "$first" ] || harness "the refused worker never ran"
wait_until 60 ended "$first" || harness "the refused worker's engine did not end within 60 s"
told_refused() { [ "$(digests_like "$session_refused model-$first")" -ge 1 ]; }
wait_until 20 told_refused || true
assert_eq "$(executions_of "$session_refused")" 1 "(2) executions of the worker whose engine ended on a 400 ($(records_of "$session_refused" | tr '\n' ','))"
assert_eq "$(revival_count "$session_refused")" 0 "(2) the recovery's revivals of the worker that ended on a 400"
assert_eq "$(turns_of "$agent_refused")" 1 "(2) turns of the worker that ended on a 400"
assert_eq "$(records_of "$session_refused")" "model_error final" "(2) the interruptions of the worker that ended on a 400"
assert_eq "$(digests_like "$session_refused model-$first")" 1 "(2) digests telling the supervisor of the 400"
sleep 15
assert_eq "$(digests_like "$session_refused model-%")" 1 "(2) digests telling the supervisor of the 400, 15 s later (told once)"
assert_eq "$(executions_of "$session_refused")" 1 "(2) executions of the worker that ended on a 400, 15 s later"

# --- (3) the retries spent: final, told once ------------------------------------------------

three() { [ "$(executions_of "$session_spent")" -ge 3 ] && ended "$(execution_n "$session_spent" 3)"; }
wait_until 120 three || fail "(3) the worker whose engine always ends on a 529 did not run and end three times within 120 s ($(records_of "$session_spent" | tr '\n' ','))"
third=$(execution_n "$session_spent" 3)
told_spent() { [ "$(digests_like "$session_spent model-$third")" -ge 1 ]; }
wait_until 20 told_spent || true
assert_eq "$(executions_of "$session_spent")" 3 "(3) executions of the worker whose retries are spent ($(records_of "$session_spent" | tr '\n' ','))"
assert_eq "$(revival_count "$session_spent")" 2 "(3) the recovery's revivals of the worker whose retries are spent"
assert_eq "$(final_of "$session_spent" model_unavailable)" "$third" "(3) the execution of its final model_unavailable record"
assert_eq "$(digests_like "$session_spent model-$third")" 1 "(3) digests telling the supervisor the retries are spent"
sleep 15
assert_eq "$(digests_like "$session_spent model-%")" 1 "(3) digests telling the supervisor the retries are spent, 15 s later (told once)"
assert_eq "$(executions_of "$session_spent")" 3 "(3) executions of the worker whose retries are spent, 15 s later"

# --- (4) the setting is checked at the start -----------------------------------------------

stop_loop
for value in 0 21 -1 two; do
    SPAWN_RECOVER_MODEL_RETRIES=$value "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" \
        >"$WORK/bad.out" 2>"$WORK/bad.err" &
    bad=$!
    i=0
    while kill -0 "$bad" 2>/dev/null && [ "$i" -lt 150 ]; do
        i=$((i + 1))
        sleep 0.1
    done
    if kill -0 "$bad" 2>/dev/null; then
        kill "$bad" 2>/dev/null || true
        wait "$bad" 2>/dev/null || true
        fail "(4) spawn loop with SPAWN_RECOVER_MODEL_RETRIES=$value still ran after 15 s"
    fi
    code=0
    wait "$bad" || code=$?
    [ "$code" -ne 0 ] || fail "(4) spawn loop with SPAWN_RECOVER_MODEL_RETRIES=$value exited 0"
    grep -q SPAWN_RECOVER_MODEL_RETRIES "$WORK/bad.err" "$WORK/bad.out" ||
        fail "(4) spawn loop refused SPAWN_RECOVER_MODEL_RETRIES=$value without naming it: $(head -c 300 "$WORK/bad.err")"
done

echo "039: a worker that ended twice on a 529 was resumed twice after the backoff and did its task with no person or supervisor; a 400 was never retried and told once; spent retries were final and told once; a bad retry setting stops the loop at its start"
exit 0
