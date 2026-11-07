#!/bin/sh
# Black box: an objective mission keeps up to N objective tasks in flight,
# each on its own agent; landing stays one at a time; N defaults to 1.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# model judge, no supervisor) runs every worker with a fake engine, and
# `spawn integrate --once` passes, run by the scenario, land their work
# through a recording gate (SPAWN_GATE) that accepts every candidate. Each
# objective's prompt names one file and its content (`BB:TASK <file>
# <word>`); the fake engine, given the task, waits at a barrier before it
# writes the file and its report: until 3 tasks have reached their engines,
# or until a hold runs out. Checked:
#   (1) N=3 (`spawn mission configure ... --workers 3`), a backlog of 4
#       independent objectives (each a different file): within 90 s, three
#       objective tasks are delivered at once, on 3 distinct agents, only one
#       of them the mission's resident (the only agent anyone approved).
#       Their delivered-to-reported intervals overlap. Never more than 3
#       tasks of the mission open at once, never two open for one objective;
#       the fourth objective is assigned once a slot is free. All 4 land,
#       each once, and the source's main holds all four files.
#   (2) N=1, the default (a mission configured without --workers), a backlog
#       of 3: they run one after another on the resident. Never two of the
#       mission's tasks open at once, delivered-to-reported intervals
#       disjoint, each assigned only after the one before it landed. All 3
#       land and main holds the three files.
#
# Not checked here: two in-flight objectives touching the same lines (one
# gated `conflict`, resolved, then landed). That needs the conflict-merge
# fix, which is not on main yet; the objective's prompt states it.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on an integrator that keeps one objective task in
# flight per mission. Runs inside the gate's sandbox (scripts/rsi/gate.sh):
# needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and drops its own
# database, uses a private tmux socket, no network. Every wait is bounded in
# time.
set -eu

asserted=""
harness() {
    echo "009: harness: $*" >&2
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
SPAWN_DB_NAME="bb_009_parallel_$run"
SPAWN_TMUX_SOCKET="bb-009-$run"
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
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "009: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "009: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "009: the gate verdicts:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'id', r ->> 'verdict', left(coalesce(r ->> 'reason', ''), 200)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "009: harness: unexpected exit $rc" >&2
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
src3=$WORK/src3.git
src1=$WORK/src1.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
for bare in "$repo" "$src3" "$src1"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# The mission's scenarios directory: the recording gate below judges, this
# only has to exist and hold a scenario (the integrator counts them).
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

# Backlogs: four independent objectives for N=3, the first three for N=1.
objective() {
    printf '{"id": %s, "title": "Write %s", "prompt": "Write the file %s.\\nBB:TASK %s %s\\nNothing else."}\n' \
        "$2" "$3" "$3" "$3" "$4" >"$1/$2.json"
}
mkdir -p "$WORK/backlog3" "$WORK/backlog1"
objective "$WORK/backlog3" 901 ALPHA.txt alpha
objective "$WORK/backlog3" 902 BRAVO.txt bravo
objective "$WORK/backlog3" 903 CHARLIE.txt charlie
objective "$WORK/backlog3" 904 DELTA.txt delta
objective "$WORK/backlog1" 901 ALPHA.txt alpha
objective "$WORK/backlog1" 902 BRAVO.txt bravo
objective "$WORK/backlog1" 903 CHARLIE.txt charlie

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It writes its session transcript where Claude Code does,
# so the Bridge and the Observer see turns as they see a real engine's. A
# turn whose text carried `BB:TASK <file> <word>` notes its arrival, waits
# at the barrier (until $BB_WORK/barrier tasks have arrived, or
# $BB_WORK/hold seconds pass), then writes the file, ends the turn, and
# writes the report in a second turn once its checkpoint is pushed.
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
# turn_ends <text>: the screen settles at the prompt first, then the
# transcript ends the turn. The other order leaves a screen record after
# the idle judgement, and with no model judge nothing judges it again: the
# worker would never be idle at its prompt for its next task.
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
# pushed <agent> <file>: a source's spawn/<agent> holds the file.
pushed() {
  for source in "$BB_WORK"/src*.git; do
    git --git-dir="$source" cat-file -e "refs/heads/spawn/$1:$2" 2>/dev/null && return 0
  done
  return 1
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
    *"BB:TASK "*) [ -n "$task" ] || task=${typed#*BB:TASK } ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$task" ]; then
      set -- $task
      file=$1
      word=$2
      mkdir -p "$BB_WORK/arrived"
      : >"$BB_WORK/arrived/$SPAWN_SESSION_ID-$file"
      printf 'arrived %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$file" >>"$BB_WORK/engine.log"
      start=$(date +%s)
      while :; do
        arrived=$(ls "$BB_WORK/arrived" | wc -l)
        [ "$arrived" -ge "$(cat "$BB_WORK/barrier")" ] && break
        [ $(($(date +%s) - start)) -ge "$(cat "$BB_WORK/hold")" ] && break
        sleep 0.2
      done
      printf '%s\n' "$word" >"$file"
      # The work's turn ends first: the report comes in a turn of its own,
      # once the pusher published a checkpoint holding the file (or 30 s
      # passed). A report collected before its turn's checkpoint is recorded
      # would be judged on the checkpoint before it, a race of its own that
      # this scenario is not about.
      turn_ends "wrote $file"
      agent=agent_${SPAWN_SESSION_ID#session_}
      start=$(date +%s)
      until pushed "$agent" "$file"; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      # Open long enough for the Observer to see it working: with no model
      # judge it writes a judgement only when it changes, and an idle one
      # older than the turn's records is not acted on.
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      printf 'reported %s %s %s\n' "$(date +%s)" "$SPAWN_SESSION_ID" "$file" >>"$BB_WORK/engine.log"
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

# barrier <tasks> <seconds>: what the fake engine waits for, from now on.
barrier() {
    rm -rf "$WORK/arrived"
    mkdir -p "$WORK/arrived"
    printf '%s\n' "$1" >"$WORK/barrier.tmp" && mv "$WORK/barrier.tmp" "$WORK/barrier"
    printf '%s\n' "$2" >"$WORK/hold.tmp" && mv "$WORK/hold.tmp" "$WORK/hold"
}

integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate exited $status: $(err)"
    return 0
}

# The time of a task's first control record of a type, or empty.
record_at() {
    q "select min(recorded_at_ms) from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = '$2'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
# The mission's objective tasks, by id.
tasks_of() {
    sql "select id from mission_tasks where mission_id = $1 and objective is not null order by id"
}
open_tasks() {
    q "select count(*) from mission_tasks
        where mission_id = $1 and state not in ('landed', 'cancelled')"
}
# Objectives with two or more open tasks at once.
doubled() {
    q "select count(*) from (select objective from mission_tasks
        where mission_id = $1 and objective is not null and state not in ('landed', 'cancelled')
        group by objective having count(*) > 1) d"
}
# Tasks of the mission ever delivered (a task.delivered record), whatever
# their state now. The barrier releases three delivered tasks at once, and
# each moves on to reported by itself, possibly between two polls: a wait
# for three in state delivered at one poll can miss them. That they were in
# flight together is checked after, from the records (the intervals overlap).
delivered_ever() {
    q "select count(distinct kind -> 'detail' -> 'record' ->> 'id') from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
          and (kind -> 'detail' -> 'record' ->> 'id')::bigint in
              (select id from mission_tasks where mission_id = $1)"
}
landed() {
    q "select count(*) from mission_tasks where mission_id = $1 and state = 'landed'"
}
main_file() { git --git-dir="$1" show "main:$2" 2>/dev/null || echo missing; }
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
mission_line() {
    q "select string_agg(id || ':' || state || ':' || agent_id, ' ' order by id)
         from mission_tasks where mission_id = $1"
}

# watch <mission> <limit>: the invariants every poll checks.
watch() {
    open=$(open_tasks "$1")
    [ "$open" -le "$2" ] ||
        fail "$open tasks of mission $1 open at once (at most $2): $(mission_line "$1")"
    [ "$(doubled "$1")" -eq 0 ] ||
        fail "two open tasks for one objective in mission $1: $(mission_line "$1")"
    if [ "$open" -gt "$peak" ]; then peak=$open; fi
}

resident() {
    capture "$SPAWN_BIN" resident propose --name "$1" --charter "Pursues the 009 objectives."
    [ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$1"
    [ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
}

capture "$SPAWN_BIN" mission add --text "009 three at once" --source "$src3"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
m3=$(jq -r .mission "$WORK/out.txt")
[ -n "$m3" ] && [ "$m3" != null ] || harness "mission add printed no id: $(out)"
resident wide
barrier 3 120

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# --- (1) N=3: three in flight on three agents --------------------------------------

capture "$SPAWN_BIN" mission configure --id "$m3" --gate "$gate" --accept gate \
    --backlog "$WORK/backlog3" --resident wide --workers 3
[ "$status" -eq 0 ] || fail "(1) mission configure --workers 3 exited $status: $(err)"

peak=0
deadline=$(($(now) + 90))
until [ "$(delivered_ever "$m3")" -ge 3 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(1) three objective tasks were not all delivered within 90 s (most open at once: $peak; tasks: $(mission_line "$m3"))"
    fi
    integrate_once
    watch "$m3" 3
    sleep 0.5
done

deadline=$(($(now) + 180))
until [ "$(landed "$m3")" -ge 4 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(1) the 4 objectives did not all land within 180 s (tasks: $(mission_line "$m3"); integrate: $(log_tail))"
    fi
    integrate_once
    watch "$m3" 3
    sleep 0.5
done
# A few more passes: nothing more to assign.
for _ in 1 2 3; do integrate_once; done

tasks=$(tasks_of "$m3" | tr '\n' ' ')
set -- $tasks
assert_eq "$#" 4 "(1) objective tasks of mission $m3, one per objective ($(mission_line "$m3"))"
assert_eq "$(q "select count(distinct objective) from mission_tasks where mission_id = $m3")" 4 \
    "(1) distinct objectives among the mission's tasks"
first3="$1, $2, $3"
assert_eq "$(q "select count(distinct agent_id) from mission_tasks where id in ($first3)")" 3 \
    "(1) distinct agents of the first three objective tasks ($(mission_line "$m3"))"
assert_eq "$(q "select count(*) from mission_tasks where id in ($first3)
                  and agent_id = 'agent_resident_wide'")" 1 \
    "(1) the first three tasks include exactly one on the mission's resident"
latest_delivery=0
first_report=""
for task in $1 $2 $3; do
    delivered=$(record_at "$task" task.delivered)
    reported=$(record_at "$task" task.reported)
    [ -n "$delivered" ] && [ -n "$reported" ] ||
        fail "(1) task $task has no delivered or reported record"
    [ "$delivered" -gt "$latest_delivery" ] && latest_delivery=$delivered
    if [ -z "$first_report" ] || [ "$reported" -lt "$first_report" ]; then
        first_report=$reported
    fi
done
[ "$latest_delivery" -lt "$first_report" ] ||
    fail "(1) the three tasks' delivered-to-reported intervals do not overlap (last delivery at $latest_delivery ms, first report at $first_report ms)"
# The fourth waited for a free slot: assigned after the first landing.
fourth_at=$(record_at "$4" task.assigned)
first_landing=$(q "select min(recorded_at_ms) from spawn_events
    where session_id = 'session_control'
      and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
      and kind -> 'detail' -> 'record' ->> 'id' in ('$1', '$2', '$3')")
[ -n "$fourth_at" ] && [ -n "$first_landing" ] && [ "$fourth_at" -ge "$first_landing" ] ||
    fail "(1) the fourth objective was assigned before a slot was free (assigned $fourth_at, first landing $first_landing)"
assert_eq "$(main_file "$src3" ALPHA.txt)" alpha "(1) ALPHA.txt on main"
assert_eq "$(main_file "$src3" BRAVO.txt)" bravo "(1) BRAVO.txt on main"
assert_eq "$(main_file "$src3" CHARLIE.txt)" charlie "(1) CHARLIE.txt on main"
assert_eq "$(main_file "$src3" DELTA.txt)" delta "(1) DELTA.txt on main"
assert_eq "$(q "select count(*) from spawn_events where session_id = 'session_control'
                  and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
                  and (kind -> 'detail' -> 'record' ->> 'id')::bigint in
                      (select id from mission_tasks where mission_id = $m3)")" 4 \
    "(1) landings of mission $m3, one per task"

# --- (2) N=1, the default: one at a time --------------------------------------------

capture "$SPAWN_BIN" mission add --text "009 one at a time" --source "$src1"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
m1=$(jq -r .mission "$WORK/out.txt")
[ -n "$m1" ] && [ "$m1" != null ] || harness "mission add printed no id: $(out)"
resident narrow
# No barrier to fill: each task holds its worker 3 s, long enough for a
# second task, if one were delivered, to be caught in flight with it.
barrier 1000 3
capture "$SPAWN_BIN" mission configure --id "$m1" --gate "$gate" --accept gate \
    --backlog "$WORK/backlog1" --resident narrow
[ "$status" -eq 0 ] || fail "(2) mission configure without --workers exited $status: $(err)"

peak=0
deadline=$(($(now) + 240))
until [ "$(landed "$m1")" -ge 3 ]; do
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(2) the 3 objectives did not all land within 240 s (tasks: $(mission_line "$m1"); integrate: $(log_tail))"
    fi
    integrate_once
    watch "$m1" 1
    sleep 0.5
done
for _ in 1 2 3; do integrate_once; done

tasks=$(tasks_of "$m1" | tr '\n' ' ')
set -- $tasks
assert_eq "$#" 3 "(2) objective tasks of mission $m1 ($(mission_line "$m1"))"
previous_landed=""
previous_reported=""
for task in "$@"; do
    assigned=$(record_at "$task" task.assigned)
    delivered=$(record_at "$task" task.delivered)
    reported=$(record_at "$task" task.reported)
    [ -n "$delivered" ] && [ -n "$reported" ] ||
        fail "(2) task $task has no delivered or reported record"
    if [ -n "$previous_landed" ]; then
        [ "$assigned" -ge "$previous_landed" ] ||
            fail "(2) task $task was assigned before the task before it landed"
        [ "$delivered" -ge "$previous_reported" ] ||
            fail "(2) task $task was delivered while the task before it was in flight"
    fi
    previous_landed=$(record_at "$task" task.landed)
    previous_reported=$reported
done
assert_eq "$(q "select count(distinct agent_id) from mission_tasks where mission_id = $m1")" 1 \
    "(2) agents of mission $m1 (its resident only)"
assert_eq "$(main_file "$src1" ALPHA.txt)" alpha "(2) ALPHA.txt on main"
assert_eq "$(main_file "$src1" BRAVO.txt)" bravo "(2) BRAVO.txt on main"
assert_eq "$(main_file "$src1" CHARLIE.txt)" charlie "(2) CHARLIE.txt on main"

echo "009: N=3 kept three objective tasks in flight on three agents and landed all four; N=1 ran them one at a time"
exit 0
