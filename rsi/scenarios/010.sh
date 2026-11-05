#!/bin/sh
# Black box: a worker's report is judged against the checkpoint of the turn
# that produced it, never an older one, and never "no checkpoint yet".
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# model judge, no supervisor) runs ephemeral workers of one gated mission
# (accept = gate) with a fake engine; `spawn integrate --once` passes, run by
# the scenario as soon as a task is reported, judge them with a recording
# gate (SPAWN_GATE) that accepts every candidate. A `git` first in PATH makes
# each checkpoint commit (`commit-tree ... -m "spawn checkpoint ..."`) take
# 4 s, so a checkpoint is recorded well after the turn that asks for it
# ended. Everything else goes to the real git at once.
#   (1) One turn writes the work and the report, as a real engine does
#       (`BB:ONE <file> <word>`).
#   (2) One turn writes the work and ends; once its checkpoint is pushed, a
#       second turn writes only the report: that turn's checkpoint changes
#       nothing (`BB:TWO <file> <word>`).
# For each task: it lands within 150 s with no `rejected` verdict, the
# mission's gate_rejections stay 0 and it never halts, main holds the file;
# its `task.reported` record names in `commit` a checkpoint of the agent
# (an `artifact.committed` of its own, recorded before the report) whose tree
# holds the file, and in (2) the checkpoint of the first turn.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on a dispatcher that collects a report before its
# turn's checkpoint is recorded: the integrator judges the pushed branch of
# the agent's mount, which holds none of the work ("the work changes nothing
# on main"). Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs
# SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own database,
# uses a private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "010: harness: $*" >&2
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
SPAWN_DB_NAME="bb_010_report_$run"
SPAWN_TMUX_SOCKET="bb-010-$run"
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
            echo "010: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "010: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "010: the gate verdicts:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'id', r ->> 'verdict', left(coalesce(r ->> 'reason', ''), 200)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "010: harness: unexpected exit $rc" >&2
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
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# A slow checkpoint: the Bridge's commit of a workspace takes 4 s more.
real_git=$(command -v git)
mkdir -p "$WORK/bin"
cat >"$WORK/bin/git" <<EOF
#!/bin/sh
for argument in "\$@"; do
  case "\$argument" in
  "spawn checkpoint "*) sleep 4 ;;
  esac
done
exec '$real_git' "\$@"
EOF
chmod 755 "$WORK/bin/git"
PATH=$WORK/bin:$PATH
export PATH

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The recording gate: accepts every candidate and notes what it judged.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It writes its session transcript where Claude Code does,
# so the Bridge and the Observer see turns as they see a real engine's.
#   BB:ONE <file> <word>  the turn writes the file and the report
#   BB:TWO <file> <word>  the turn writes the file and ends; once the
#                         source's spawn/<agent> holds it, a second turn
#                         writes the report alone
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
# The screen settles at the prompt, then the transcript ends the turn.
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
report() {
  mkdir -p .spawn
  printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$1" >.spawn/report.md
  printf 'reported %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$1" >>"$BB_WORK/engine.log"
}
pushed() {
  git --git-dir="$BB_WORK/source.git" cat-file -e "refs/heads/spawn/$1:$2" 2>/dev/null
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
task=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:ONE "*) [ -n "$task" ] || task="one ${typed#*BB:ONE }" ;;
    *"BB:TWO "*) [ -n "$task" ] || task="two ${typed#*BB:TWO }" ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$task" ]; then
      set -- $task
      printf '%s\n' "$3" >"$2"
      printf 'wrote %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$2" >>"$BB_WORK/engine.log"
      if [ "$1" = two ]; then
        turn_ends "wrote $2"
        agent=agent_${SPAWN_SESSION_ID#session_}
        start=$(date +%s)
        until pushed "$agent" "$2"; do
          [ $(($(date +%s) - start)) -ge 60 ] && break
          sleep 0.2
        done
        # A turn of its own, open long enough for the Observer to see it.
        line '"type":"user","message":{"role":"user","content":"report"}'
        sleep 2
      fi
      report "$2"
      task=
    fi
    turn_ends idle
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
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"

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

integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate exited $status: $(err)"
    return 0
}

# A control record of the task: its event id, or empty.
record_id() {
    q "select min(id) from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = '$2'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
reported_commit() {
    q "select kind -> 'detail' -> 'record' ->> 'commit' from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.reported'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'
        order by id desc limit 1"
}
# The event id of the agent's checkpoint <commit>, or empty.
checkpoint_id() {
    q "select min(id) from spawn_events
        where agent_id = '$1' and event_name = 'artifact.committed'
          and kind -> 'detail' -> 'artifact' ->> 'commit' = '$2'"
}
# The agent's checkpoints in order, one commit per line.
checkpoints() {
    sql "select kind -> 'detail' -> 'artifact' ->> 'commit' from spawn_events
          where agent_id = '$1' and event_name = 'artifact.committed' order by id"
}
rejections() {
    q "select count(*) from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and kind -> 'detail' -> 'record' ->> 'verdict' = 'rejected'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
task_state() { q "select state from mission_tasks where id = $1"; }
task_agent() { q "select agent_id from mission_tasks where id = $1"; }
main_file() { git --git-dir="$source" show "main:$1" 2>/dev/null || echo missing; }
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
# The commit (in the source or the integrator's mirror) holds the file.
holds() {
    git --git-dir="$source" cat-file -e "$1:$2" 2>/dev/null && return 0
    for mirror in "$WORK"/integrator/.sources/*.git; do
        git --git-dir="$mirror" cat-file -e "$1:$2" 2>/dev/null && return 0
    done
    return 1
}

assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    jq -r .task "$WORK/out.txt"
}

# work <task> <file> <word> <what>: integrate as soon as the task is
# reported, until it lands; then check the report's checkpoint.
work() {
    deadline=$(($(now) + 150))
    until [ "$(task_state "$1")" = landed ]; do
        [ "$(rejections "$1")" -eq 0 ] ||
            fail "$4: the gate rejected task $1 ($(q "select kind -> 'detail' -> 'record' ->> 'reason' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  and kind -> 'detail' -> 'record' ->> 'id' = '$1' order by id desc limit 1"))"
        [ "$(now)" -lt "$deadline" ] ||
            fail "$4: task $1 did not land within 150 s (state: $(task_state "$1"); integrate: $(log_tail))"
        case "$(task_state "$1")" in
        reported | accepted | landing) integrate_once ;;
        esac
        sleep 0.3
    done
    assert_eq "$(rejections "$1")" 0 "$4: rejections of task $1"
    assert_eq "$(main_file "$2")" "$3" "$4: $2 on main"
    agent=$(task_agent "$1")
    commit=$(reported_commit "$1")
    [ -n "$commit" ] || fail "$4: task $1's task.reported record names no commit"
    at=$(checkpoint_id "$agent" "$commit")
    [ -n "$at" ] || fail "$4: task $1 reported commit $commit, which is no checkpoint of $agent"
    [ "$at" -lt "$(record_id "$1" task.reported)" ] ||
        fail "$4: task $1's checkpoint $commit was recorded after its report"
    holds "$commit" "$2" || fail "$4: task $1's reported checkpoint $commit does not hold $2"
}

capture "$SPAWN_BIN" mission add --text "010 reports and checkpoints" --source "$source" \
    --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# --- (1) the work and the report in one turn ----------------------------------

one=$(assign one "Write the file.
BB:ONE ONE.txt one")
work "$one" ONE.txt one "(1) one turn"

# --- (2) the report in a turn of its own, whose checkpoint changes nothing ---

two=$(assign two "Write the file, then report.
BB:TWO TWO.txt two")
work "$two" TWO.txt two "(2) a report turn that changes nothing"
agent_two=$(task_agent "$two")
first=$(checkpoints "$agent_two" | while read -r commit; do
    if holds "$commit" TWO.txt; then echo "$commit"; break; fi
done)
assert_eq "$(reported_commit "$two")" "$first" \
    "(2) task $two's report names the checkpoint of the turn that wrote TWO.txt"

assert_eq "$(q "select gate_rejections from mission_missions where id = $mission")" 0 \
    "the mission's gate rejections"
assert_eq "$(q "select coalesce(halted, '') from mission_missions where id = $mission")" "" \
    "the mission's halt"
assert_eq "$(main_file ONE.txt)" one "ONE.txt still on main"

echo "010: each report was judged against its turn's checkpoint, in one turn and in a report turn of its own"
exit 0
