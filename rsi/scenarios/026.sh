#!/bin/sh
# Black box: a task over its budget keeps its work. Today the integrator
# stops a delivered task that ran past its mission's budget and records
# `task.aborted`, which the mission counts as a gate abort (three in a row
# halt it); the objective is then issued again to a fresh worker, from main,
# whose brief says nothing of the work the stopped task left on its branch.
# On 2026-11-06 mission 4's task 84 (objective 017) hit the 150 tool-call
# budget after 51 minutes: aborted at 05:12:59 UTC, its branch
# spawn/agent_worker_objective_84 holding 4,259 lines in 21 files (head
# d44c601), and re-issued at 05:13:05 as task 87 on agent_worker_objective_87,
# which was not told of it. Required: (1) an overrun is its own record
# (`task.over_budget`), not a gate abort, and counts toward no halt; (2) the
# objective's next task continues the overrun task: its `task.assigned`
# names it (`continues`), what it is told names the branch, its commit and
# the diff stat against main, and its workspace holds that work; (3) a
# second overrun on the same objective is escalated to the supervisor, both
# tasks named, and the objective is not issued again (the mission's other
# objectives go on).
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, the
# supervisor, no model judge) runs every agent on a fake engine, and
# `spawn integrate --once` passes, run by the scenario, enforce the budget
# and assign objectives. One gated objective mission (accept gate, a budget
# of 5 tool calls, 2 workers) whose resident, `keeper`, holds a task of the
# mission, so objective tasks go to per-task workers, as task 84 did. Its
# backlog: objective 961, whose engine writes big.txt (120 lines) if its
# workspace lacks it, notes whether it found it, then makes a tool call
# every 0.2 s until it is stopped; objective 962, whose engine writes a file
# and stops making calls. Checked:
#   (1) T1 (961) goes past 5 tool calls: within 45 s of its delivery a record
#       for it says so; it is `task.over_budget` with `reason`, `tool_calls`
#       (more than 5) and `budget` ({"tool_calls": 5}); no `task.aborted`;
#       the mission's gate_aborts and gate_rejections stay 0.
#   (2) Within 60 s of that record, T2 (961) is assigned with `continues`
#       T1; the text delivered to its worker names T1's branch
#       (spawn/<T1's agent>), a commit of that branch (7+ hex) whose tree
#       holds the big.txt T1 wrote, and that commit's diff stat against main
#       (`git diff --shortstat`'s line, and big.txt); T2's engine finds
#       big.txt in its workspace, the same bytes as in that commit.
#   (3) T2 goes past the budget too: within 60 s of its delivery a
#       `task.over_budget` record for it (no `task.aborted`); within 40 s of
#       that, a digest delivered to the supervisor names both tasks (#T1 and
#       #T2), and objective 962 gets a task; after five more integrator
#       passes 961 has exactly those two tasks, the supervisor was told once,
#       gate_aborts is still 0 and the mission is not halted.
#
# Measured on the live main (49b92be, the deployed binary built from it),
# 2026-11-06: see "Today" at the end of this header.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1. BB_026_MEASURE=1 (only to show today's behaviour)
# reports every failed check and goes on, then exits 1 if any failed.
#
# Fails with 1 at (1) on the live main: the overrun is recorded as
# `task.aborted` and counts as a gate abort. It passes once an overrun is a
# record of its own that counts toward no halt, the objective's next task
# continues the overrun task's branch and is told so, and a second overrun
# on an objective goes to the supervisor instead of a third task.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
#
# Today (three runs): T1 is aborted 2.43-2.53 s after its delivery
# (task.aborted, "the task made more than 5 tool calls"; gate_aborts 0 -> 1);
# T2 is assigned 137-157 ms later on a fresh worker, with no `continues`,
# told no branch, commit or diff stat, and its engine finds no big.txt,
# though T1's branch holds it (1 file changed, 120 insertions(+)) by the
# time T2 starts: the Stop's final checkpoint reaches the source after the
# abort's record, so a continuation waits for it. T2 is aborted too
# (gate_aborts 2), objective 961 gets a third task, whose abort halts the
# mission ("3 gate aborts in a row"); the supervisor hears of no overrun
# and objective 962 never gets a task. The run takes 15-29 s; it fails at
# (1) in 15-19 s.
set -eu

asserted=""
harness() {
    echo "026: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE=${BB_026_MEASURE-}

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_026_overrun_$run"
SPAWN_TMUX_SOCKET="bb-026-$run"
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
            echo "026: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "026: the integrator's last lines:" >&2
            grep -v '^\s*$' "$WORK/integrate.log" | tail -n 12 >&2
        fi
        if [ -n "$database_made" ]; then
            echo "026: the mission's task records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'type', r ->> 'id', left(coalesce(r ->> 'reason', r ->> 'agent_id', ''), 120)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' like 'task.%'
                  and r ->> 'type' not in ('task.delivered')
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "026: harness: unexpected exit $rc" >&2
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
# A check that BB_026_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "026: (measure) would fail: $*"
    else
        fail "$*"
    fi
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

# The mission's scenarios directory: no gate runs here (nothing is
# reported), it only has to exist and hold a scenario.
gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
cat >"$WORK/fake-gate.sh" <<'EOF'
#!/bin/sh
printf '{"verdict":"accepted"}\n' >"$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

backlog=$WORK/backlog
mkdir -p "$backlog"
printf '{"id": 961, "title": "Write the big file", "prompt": "Write big.txt.\\nBB:BIG\\nNothing else."}\n' \
    >"$backlog/961.json"
printf '{"id": 962, "title": "Write the small file", "prompt": "Write small.txt.\\nBB:SMALL\\nNothing else."}\n' \
    >"$backlog/962.json"

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet, the transcript's end of the turn first, then the prompt.
# It writes its session transcript where Claude Code does, so the Bridge
# and the Observer see turns and tool calls as they see a real engine's.
# A turn whose text named `BB:BIG` notes whether big.txt was in the
# workspace (and its sha256), writes it if not (120 lines), then shows a
# running turn and makes a tool call every 0.2 s (an assistant `tool_use`
# line, then its `tool_result`) until it is stopped. `BB:SMALL` writes
# small.txt and ends the turn.
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
calls() {
  printf '\n✻ Working… (esc to interrupt)\n\n❯ '
  n=0
  while [ "$n" -lt 3000 ]; do
    n=$((n + 1))
    id="toolu_${SPAWN_EXECUTION_ID}_$n"
    line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
    sleep 0.1
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
    printf '%s %s\n' "$(date +%s%3N)" "$n" >"$BB_WORK/calls-$SPAWN_SESSION_ID"
    sleep 0.1
  done
  while :; do sleep 3600; done
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
mode=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:BIG"*) mode=big ;;
    *"BB:SMALL"*) mode=small ;;
    esac
  elif [ "$pending" = 1 ]; then
    case "$mode" in
    big)
      if [ -f big.txt ]; then
        found="found $(sha256sum <big.txt | cut -d' ' -f1)"
      else
        found=absent
      fi
      printf '%s %s %s\n' "$(date +%s%3N)" "$SPAWN_SESSION_ID" "$found" >>"$BB_WORK/found.log"
      if [ ! -f big.txt ]; then
        i=0
        while [ "$i" -lt 120 ]; do
          i=$((i + 1))
          printf 'the big work, line %s\n' "$i"
        done >big.txt
      fi
      calls
      ;;
    small) printf 'small\n' >small.txt ;;
    esac
    mode=
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
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
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS; do
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
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# No stall is under test: keeper's task waits idle, by design.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB \
    SPAWN_DISK_FLOOR_GB SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/found.log"

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
now_ms() { echo $(($(date +%s%N) / 1000000)); }
q() { scalar "$1"; }

integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate exited $status: $(err | tail -n 3)"
    return 0
}
# A task's control records of a type: `<at_ms>|<record>`, oldest first.
records() {
    sql "select e.recorded_at_ms || '|' || (e.kind -> 'detail' -> 'record')::text
           from spawn_events e
          where e.session_id = 'session_control'
            and e.kind -> 'detail' -> 'record' ->> 'type' = '$2'
            and e.kind -> 'detail' -> 'record' ->> 'id' = '$1'
          order by e.session_seq"
}
record_at() { records "$1" "$2" | head -n 1 | cut -d'|' -f1; }
record_of() { records "$1" "$2" | head -n 1 | cut -d'|' -f2-; }
# The time of the overrun's record, whatever its type: `task.over_budget`
# as required, `task.aborted` as today.
overrun_at() {
    q "select min(e.recorded_at_ms) from spawn_events e
        where e.session_id = 'session_control'
          and e.kind -> 'detail' -> 'record' ->> 'type' in ('task.over_budget', 'task.aborted')
          and e.kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
objective_tasks() {
    sql "select id from mission_tasks where mission_id = $mission and objective = $1 order by id"
}
counter() { q "select $1 from mission_missions where id = $mission"; }
calls_of() { # the tool calls the engine of a session has made
    if [ -f "$WORK/calls-$1" ]; then cut -d' ' -f2 "$WORK/calls-$1"; else echo 0; fi
}
mission_line() {
    q "select string_agg(id || ':' || coalesce(objective::text, '-') || ':' || state || ':' || agent_id, ' ' order by id)
         from mission_tasks where mission_id = $mission"
}
# wait_until <seconds> <shell condition>: integrator passes every 0.5 s
# until the condition holds; 1 when the time ran out.
wait_until() {
    limit=$(($(now) + $1))
    shift
    while :; do
        integrate_once
        if eval "$*"; then return 0; fi
        [ "$(now)" -lt "$limit" ] || return 1
        sleep 0.5
    done
}
# The supervisor's digests that name both tasks.
told_both() {
    q "select count(*) from spawn_commands c
        where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
          and c.action ->> 'type' = 'input' and c.succeeded
          and c.action ->> 'text' ~ '#$1([^0-9]|\$)' and c.action ->> 'text' ~ '#$2([^0-9]|\$)'"
}

# --- the mission ----------------------------------------------------------------

capture "$SPAWN_BIN" resident propose --name keeper --charter "Holds a task of the 026 mission."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name keeper
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" mission add --text "026 over budget" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
# keeper's own task of the mission: the resident is taken, so every
# objective task goes to a per-task worker, as task 84 did.
capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker keeper \
    --brief "Hold this task open. BB:HOLD"
[ "$status" -eq 0 ] || harness "task assign to keeper failed: $(err)"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate \
    --tool-calls 5 --backlog "$backlog" --resident keeper --workers 2
[ "$status" -eq 0 ] || harness "mission configure failed: $(err)"

# --- (1) the first overrun --------------------------------------------------------

t1=""
t1_delivered=""
wait_until 90 '[ -n "$(objective_tasks 961 | head -n 1)" ] &&
    t1=$(objective_tasks 961 | head -n 1) &&
    t1_delivered=$(record_at "$t1" task.delivered) && [ -n "$t1_delivered" ]' ||
    harness "objective 961 was not assigned and delivered within 90 s (tasks: $(mission_line))"
t1_agent=$(q "select agent_id from mission_tasks where id = $t1")
t1_session=$(q "select session_id from mission_tasks where id = $t1")
t1_kind=$(q "select kind from mission_tasks where id = $t1")
[ "$t1_kind" = ephemeral ] ||
    harness "task #$t1 went to the resident ($t1_agent), not to a per-task worker"

wait_until 45 '[ -n "$(overrun_at "$t1")" ]' ||
    fail "(1) task #$t1 made $(calls_of "$t1_session") tool calls on a budget of 5, and no record of its overrun came within 45 s of its delivery"
t1_over=$(overrun_at "$t1")
echo "026: task #$t1 delivered, its overrun recorded $((t1_over - t1_delivered)) ms later"
aborted=$(record_of "$t1" task.aborted)
if [ -n "$aborted" ]; then
    measured_fail "(1) task #$t1's budget overrun was recorded as task.aborted ($(printf '%s' "$aborted" | jq -r .reason)), a gate abort: the mission's gate_aborts went from 0 to $(counter gate_aborts), toward the halt after three; an overrun is a record of its own (task.over_budget) and counts toward no halt"
fi
over=$(record_of "$t1" task.over_budget)
if [ -z "$over" ]; then
    measured_fail "(1) task #$t1 went over its budget and has no task.over_budget record"
else
    [ -n "$(printf '%s' "$over" | jq -r '.reason // empty')" ] &&
        [ "$(printf '%s' "$over" | jq -r '(.tool_calls // 0) > 5')" = true ] &&
        [ "$(printf '%s' "$over" | jq -c '.budget')" = '{"tool_calls":5}' ] ||
        measured_fail "(1) task #$t1's task.over_budget record lacks its reason, its tool calls (more than 5) or its budget ({\"tool_calls\":5}): $over"
fi
[ "$(counter gate_aborts)" -eq 0 ] && [ "$(counter gate_rejections)" -eq 0 ] ||
    measured_fail "(1) after task #$t1's overrun the mission counts $(counter gate_aborts) gate aborts and $(counter gate_rejections) rejections; an overrun is neither"

# --- (2) the next task continues it ----------------------------------------------------

t2=""
wait_until 60 't2=$(objective_tasks 961 | sed -n 2p) && [ -n "$t2" ]' ||
    fail "(2) objective 961 got no task after task #$t1's overrun within 60 s (tasks: $(mission_line))"
t2_assigned=$(record_at "$t2" task.assigned)
echo "026: task #$t2 assigned $((t2_assigned - t1_over)) ms after task #$t1's overrun"
t2_agent=$(q "select agent_id from mission_tasks where id = $t2")
t2_session=$(q "select session_id from mission_tasks where id = $t2")
continues=$(record_of "$t2" task.assigned | jq -r '.continues // empty')
[ "$continues" = "$t1" ] ||
    measured_fail "(2) task #$t2's task.assigned names continues=[$continues], not task #$t1, whose overrun it follows"
t2_delivered=""
wait_until 45 't2_delivered=$(record_at "$t2" task.delivered) && [ -n "$t2_delivered" ]' ||
    fail "(2) task #$t2 was not delivered within 45 s (tasks: $(mission_line))"
# What T2's worker was told: every delivery command of the task.
told=$(sql "select coalesce(action ->> 'text', action ->> 'prompt', '') from spawn_commands
             where idempotency_key like 'deliver-$t2-%' order by id")
printf '%s\n' "$told" >"$WORK/told-$t2.txt"
branch=spawn/$t1_agent
git --git-dir="$source" rev-parse -q --verify "refs/heads/$branch" >/dev/null ||
    measured_fail "(2) task #$t1's work never reached the source: it has no branch $branch"
grep -qF -- "$branch" "$WORK/told-$t2.txt" ||
    measured_fail "(2) what task #$t2's worker was told does not name task #$t1's branch $branch ($(wc -c <"$WORK/told-$t2.txt") bytes, starting: $(head -c 160 "$WORK/told-$t2.txt" | tr '\n' ' '))"
commit=""
for hex in $(grep -oE '\b[0-9a-f]{7,40}\b' "$WORK/told-$t2.txt" | sort -u); do
    full=$(git --git-dir="$source" rev-parse -q --verify "$hex^{commit}" 2>/dev/null) || continue
    git --git-dir="$source" merge-base --is-ancestor "$full" "refs/heads/$branch" 2>/dev/null || continue
    git --git-dir="$source" cat-file -e "$full:big.txt" 2>/dev/null || continue
    commit=$full
    break
done
if [ -z "$commit" ]; then
    measured_fail "(2) what task #$t2's worker was told names no commit of $branch that holds task #$t1's big.txt"
else
    stat=$(git --git-dir="$source" diff --shortstat main "$commit" | sed 's/^ *//')
    grep -qF -- "$stat" "$WORK/told-$t2.txt" && grep -qF big.txt "$WORK/told-$t2.txt" ||
        measured_fail "(2) what task #$t2's worker was told does not give the diff stat of ${commit%"${commit#???????}"} against main ('$stat', big.txt)"
fi
# The engine's first note on T2's session since T2's delivery (T2 may run
# on T1's own agent, resumed: the same session).
found_of_t2() {
    awk -v s="$t2_session" -v t="$t2_delivered" '$2 == s && $1 >= t { $1 = ""; $2 = ""; sub(/^  /, ""); print; exit }' \
        "$WORK/found.log"
}
wait_until 30 '[ -n "$(found_of_t2)" ]' ||
    fail "(2) task #$t2's engine never started the work within 30 s of its delivery"
found=$(found_of_t2)
want_sha=$(git --git-dir="$source" show "refs/heads/$branch:big.txt" 2>/dev/null | sha256sum | cut -d' ' -f1)
[ -n "$commit" ] && want_sha=$(git --git-dir="$source" show "$commit:big.txt" | sha256sum | cut -d' ' -f1)
echo "026: $branch against main once task #$t2 started: $(git --git-dir="$source" diff --shortstat main "refs/heads/$branch" 2>/dev/null | sed 's/^ *//')"
[ "$found" = "found $want_sha" ] ||
    measured_fail "(2) task #$t2's workspace ($t2_agent) did not hold task #$t1's work: its engine found big.txt [$found], wanted the bytes of $branch's (sha256 $want_sha)"

# --- (3) the second overrun goes to the supervisor --------------------------------------

wait_until 60 '[ -n "$(overrun_at "$t2")" ]' ||
    fail "(3) task #$t2 made $(calls_of "$t2_session") tool calls on a budget of 5, and no record of its overrun came within 60 s of its delivery"
t2_over=$(overrun_at "$t2")
echo "026: task #$t2 delivered, its overrun recorded $((t2_over - t2_delivered)) ms later"
[ -z "$(record_of "$t2" task.aborted)" ] && [ -n "$(record_of "$t2" task.over_budget)" ] ||
    measured_fail "(3) task #$t2's overrun was not recorded as task.over_budget (task.aborted: $(record_of "$t2" task.aborted | jq -r '.reason // "none"'))"
deadline=$(($(now) + 40))
while :; do
    integrate_once
    if [ "$(told_both "$t1" "$t2")" -ge 1 ] && [ -n "$(objective_tasks 962)" ]; then break; fi
    if [ "$(now)" -ge "$deadline" ] || [ -n "$(q "select halted from mission_missions where id = $mission")" ]; then
        break
    fi
    sleep 0.5
done
for _ in 1 2 3 4 5; do integrate_once; sleep 0.3; done
[ "$(told_both "$t1" "$t2")" -ge 1 ] ||
    measured_fail "(3) task #$t2, the second overrun on objective 961, was not escalated to the supervisor within 40 s: no digest it was told names #$t1 and #$t2"
[ "$(told_both "$t1" "$t2")" -le 1 ] ||
    measured_fail "(3) the supervisor was told of the two overruns $(told_both "$t1" "$t2") times; once"
tasks961=$(objective_tasks 961 | tr '\n' ' ')
[ "$(objective_tasks 961 | wc -l)" -eq 2 ] ||
    measured_fail "(3) objective 961 was issued again after its second overrun: its tasks are now $tasks961(all tasks: $(mission_line))"
[ -n "$(objective_tasks 962)" ] ||
    measured_fail "(3) objective 962 got no task while 961 was held after its second overrun (tasks: $(mission_line))"
halted=$(q "select coalesce(halted, '') from mission_missions where id = $mission")
[ -z "$halted" ] && [ "$(counter gate_aborts)" -eq 0 ] ||
    measured_fail "(3) after two overruns the mission counts $(counter gate_aborts) gate aborts and is halted: [$halted]"

if [ -n "$failed_any" ]; then
    asserted=1
    exit 1
fi
echo "026: two overruns recorded as such, counted toward no halt; the second task continued the first's branch, told so; the second overrun went to the supervisor and 961 was not issued again"
exit 0
