#!/bin/sh
# Black box: a model-switch Stop that fails is retried, then escalated; the
# task is never stranded (GitHub issue #3).
#
# A resident `core` runs live on the default model profile, idle at its
# prompt. A task on the cheap profile is assigned to it, so the dispatcher
# (inside `spawn loop`) stops the worker first, to resume it on the cheap
# profile with the task. The worker's terminal refuses to be killed (a tmux
# wrapper fails every `kill-session`), so every Stop of it fails. Checked:
#   (1) a failed switch Stop is retried: the dispatcher submits another Stop
#       to the session under a new idempotency key (a retry may wait; the
#       scenario ages each failed Stop by two hours, so any wait up to an
#       hour is seen);
#   (2) it gives up: at most 5 Stops in all for the task, and none after the
#       escalation;
#   (3) the supervisor hears of it: a digest (an Input from spawn-dispatch to
#       session_supervisor) carries an escalation tagged
#       `session_resident_core <id>` (other than the resident's approval),
#       and its text names the task as `task #<task>`;
#   (4) the task stays `assigned` throughout: never delivered, never dropped.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on a dispatcher that replans the same `switch-model-*`
# key forever. Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs
# SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own database,
# uses a private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "006: harness: $*" >&2
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
SPAWN_DB_NAME="bb_006_switch_$run"
SPAWN_TMUX_SOCKET="bb-006-$run"
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
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/loop.log" ]; then
        echo "006: the loop's last lines:" >&2
        tail -n 15 "$WORK/loop.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "006: harness: unexpected exit $rc" >&2
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
# second of quiet. It writes its session transcript where Claude Code does
# (a user line when the turn opens, a turn_duration line when it ends), so
# the Bridge and the Observer see turns as they see a real engine's.
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
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
  elif [ "$pending" = 1 ]; then
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# tmux on the private socket; while $WORK/refuse-kill exists, no session can
# be killed: every Stop of a live worker fails at its terminal.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
if [ -e '$WORK/refuse-kill' ]; then
  for argument in "\$@"; do
    if [ "\$argument" = kill-session ]; then
      echo "kill-session refused by the scenario" >&2
      exit 1
    fi
  done
fi
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"
: >"$WORK/refuse-kill"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"

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

# The session's current judgement is a worker waiting at its prompt.
waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}

# wait_until <seconds> <what> <command...>: a harness error past the limit.
wait_until() {
    limit=$1
    what=$2
    shift 2
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || harness "$what did not happen within $limit s"
        sleep 0.25
    done
}

stops() {
    q "select count(*) from spawn_commands
        where session_id = '$session' and principal = 'spawn-dispatch'
          and action ->> 'type' = 'stop'"
}
stop_keys() {
    q "select count(distinct idempotency_key) from spawn_commands
        where session_id = '$session' and principal = 'spawn-dispatch'
          and action ->> 'type' = 'stop'"
}
stops_succeeded() {
    q "select count(*) from spawn_commands
        where session_id = '$session' and action ->> 'type' = 'stop' and succeeded"
}
stop_finished() {
    [ "$(q "select count(*) from spawn_commands
            where session_id = '$session' and action ->> 'type' = 'stop'
              and state = 'finished'")" -ge 1 ]
}
stops_failed() {
    q "select count(*) from spawn_commands
        where session_id = '$session' and principal = 'spawn-dispatch'
          and action ->> 'type' = 'stop' and state = 'finished' and not succeeded"
}
# Every failed Stop of the session happened two hours earlier: any retry
# wait of an hour or less is over.
age_failed_stops() {
    sql "update spawn_commands
            set created_at_ms = created_at_ms - 7200000,
                finished_at_ms = finished_at_ms - 7200000
          where session_id = '$session' and action ->> 'type' = 'stop'
            and state = 'finished' and not succeeded
            and finished_at_ms > (extract(epoch from clock_timestamp()) * 1000)::bigint - 3600000" \
        >/dev/null
}
# A digest to the supervisor that escalates this task: an item about the
# resident's session other than its approval, in a text that names the task.
escalated() {
    q "select count(*) from spawn_commands c
        where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
          and c.action ->> 'type' = 'input'
          and position('task #$task' in c.action ->> 'text') > 0
          and exists (select 1 from jsonb_array_elements_text(
                          case when jsonb_typeof(c.action -> 'tags') = 'array'
                               then c.action -> 'tags' else '[]'::jsonb end) tag
                       where tag like '$session %' and tag <> '$session approved-core')"
}
task_state() { q "select state from mission_tasks where id = $task"; }

# The supervisor gets a digest only while its latest judgement covers its
# latest fact, and an Observer with no model judge writes a judgement only
# when it changes: an idle supervisor's goes stale at the first record after
# it. A line typed into it (not as `human`: no news) makes it judged afresh.
# The worker is left alone on purpose: a failed Stop changes nothing on it.
pings=0
ping_supervisor() {
    pings=$((pings + 1))
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor \
        --session session_supervisor --key "bb-ping-$pings" \
        --action '{"type":"input","text":"ping"}'
    [ "$status" -eq 0 ] || harness "submit of a ping to the supervisor failed: $(err)"
}

# --- a resident on the default profile, with a task on the cheap one ------------

capture "$SPAWN_BIN" mission add --text "006 model switch"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name core --charter "Owns the 006 concern."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name core
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"

# The task first, so the resident's workspace comes from the mission from the
# start. No loop runs yet, and the resident's own start (below) is pending
# when the loop's first pass begins: the dispatcher never delivers the task
# with a start of its own.
capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker core \
    --brief "work for the cheap profile" --model cheap
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
[ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"

# The resident runs on the default profile. Not submitted as `human`: a
# human's command is news for the supervisor.
capture "$SPAWN_BIN" submit --principal bb-harness --agent "$agent" --session "$session" \
    --key bb-start-core --action '{"type":"start","engine":"claude","prompt":""}'
[ "$status" -eq 0 ] || harness "submit of the resident's start failed: $(err)"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# --- the switch Stop, which fails -------------------------------------------------

deadline=$(($(now) + 120))
until [ "$(stops)" -ge 1 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        waiting "$session" ||
            harness "the resident was never judged waiting at its prompt (judged: $(q "select observation ->> 'state' from spawn_session_state where session_id = '$session'"))"
        fail "no Stop of the worker within 120 s of a cheap task for a worker on the default profile"
    fi
    sleep 0.25
done
# Read as the dispatcher reads it: no model named is the default profile.
[ "$(q "select coalesce(kind -> 'detail' ->> 'model', 'default') from spawn_events
        where session_id = '$session' and event_name = 'execution.started'
        order by session_seq desc limit 1")" = default ] ||
    harness "the resident does not run on the default profile"
wait_until 90 "the supervisor judged waiting at its prompt" waiting session_supervisor
wait_until 60 "the first Stop finished" stop_finished
[ "$(stops_succeeded)" -eq 0 ] || harness "a Stop succeeded although the terminal refuses to die"
[ "$(stops_failed)" -ge 1 ] || harness "the first Stop did not fail"

# --- retries, then the escalation -----------------------------------------------

deadline=$(($(now) + 240))
next_ping=$(($(now) + 8))
while [ "$(escalated)" -eq 0 ]; do
    [ "$(stops_succeeded)" -eq 0 ] || harness "a Stop succeeded although the terminal refuses to die"
    if [ "$(now)" -ge "$deadline" ]; then
        [ "$(stop_keys)" -ge 2 ] ||
            fail "(1) the failed switch Stop was never retried under a new key (Stops: $(stops), task: $(task_state))"
        fail "(3) no digest escalated task #$task to the supervisor within 240 s ($(stops) Stops under $(stop_keys) keys, task: $(task_state))"
    fi
    [ "$(stops)" -le 5 ] || fail "(2) more than 5 Stops for one task without an escalation ($(stops))"
    age_failed_stops
    if [ "$(now)" -ge "$next_ping" ]; then
        ping_supervisor
        next_ping=$(($(now) + 8))
    fi
    sleep 0.5
done
[ "$(stop_keys)" -ge 2 ] || fail "(1) escalated, but the failed switch Stop was never retried under a new key"
[ "$(stop_keys)" -eq "$(stops)" ] || fail "(1) a Stop key was used twice"

# After the escalation, no more Stops: a few passes, with failed Stops aged.
settled=$(stops)
[ "$settled" -le 5 ] || fail "(2) $settled Stops for one task (at most 5)"
end=$(($(now) + 6))
while [ "$(now)" -lt "$end" ]; do
    age_failed_stops
    sleep 0.5
done
assert_eq "$(stops)" "$settled" "(2) Stops of the worker after the escalation"
assert_eq "$(task_state)" assigned "(4) the task's state"
assert_eq "$(q "select count(*) from spawn_events where session_id = 'session_control'
                  and kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
                  and kind -> 'detail' -> 'record' ->> 'id' = '$task'")" 0 \
    "(4) task.delivered records of the task"

echo "006: a failed switch Stop was retried under new keys ($settled Stops), then escalated to the supervisor; the task stayed assigned"
exit 0
