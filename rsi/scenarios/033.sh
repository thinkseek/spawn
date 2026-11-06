#!/bin/sh
# Black box: a base that disagrees with itself is a host fault, not an abort.
# The gate judges each objective scenario on the base before any candidate
# code runs; an unsolved result needs two runs that agree. When the first run
# exits 1 and the second 0, scripts/rsi/gate.sh prints
#   | 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts
# and exits 4, and the integrator records `aborted`, which counts toward the
# mission's three-aborts halt. A base whose own scenario fails and then passes
# says nothing about the candidate (no candidate code has run yet): it says
# the host was too loaded or too flaky to judge. On 2026-11-06 at 14:43 CST
# (06:43 UTC) mission 4's task 87 (objective 017) was gated `aborted` that way, on 011.sh,
# on a host so loaded that the gate's setup took 244 s instead of 28; task 87
# was gated again at 15:27:56 CST, accepted, and landed. Required: such a gate is
# recorded `host_fault` (objective 020's verdict) with the evidence
# `base_disagrees`, the scenario and both exits; it counts toward no halt
# and the task is judged again after 020's doubling backoff. By the owner's
# ruling (2026-11-06) such a host fault is never escalated at two in a row;
# three in a row on one mission raise an alert instead. Objective 030, which
# owns Spawn's alerts, comes after this one in the owner's order, so the
# alert is recorded in the third host fault's own `task.gated` record
# (`alert` true, `base_disagrees` 3) and the dispatcher's digest takes it to
# the supervisor once (tag `host gate-base-disagrees-<event id>`); 030 later
# raises its `gate_base_disagrees` alert from that same record. Every other
# harness error on the base (a first run that exits 2, `1, then 2`) stays an
# abort, as today.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, the
# supervisor, no model judge) runs one resident, `flaky`, of one gated
# mission (accept = gate) on a fake engine that writes `flaky.txt` and
# reports; the supervisor runs on the same fake engine. `spawn integrate
# --once` passes, run by the scenario one at a time with
# SPAWN_GATE_HOST_FAULT_BACKOFF_SECS=4 (and objective 018's thresholds met),
# judge through a recording gate (SPAWN_GATE) that prints what
# scripts/rsi/gate.sh prints and exits as it does, by the gate run's number
# for flaky's candidate:
#   runs 1-3: the base disagrees with itself: `| == base: 011.sh`, its
#       output, `| stage gate exit 1 <ms>ms base: 011.sh`, `| == base: 011.sh
#       again (unsolved needs two runs that agree)`, its output (run 1's holds
#       the output of a nested gate, `| | 102.sh exited 1, then 0 on the base:
#       ...`, as 024.sh's toy gates print), then the gate's own
#       `| 011.sh exited 1, then 0 on the base: a harness error, not a
#       verdict; the gate aborts`, `| stage gate aborted <ms>ms base: 011.sh
#       again (unsolved needs two runs that agree)` and `| gate time: base ...
#       setup 244 s ...` (run 1; 31 s and 29 s after), no verdict, exit 4;
#   run 4: a real harness error: 011.sh's first run exits 2 (`| 011.sh exited
#       2 on the base: ...`), exit 4;
#   run 5: `| 011.sh exited 1, then 2 on the base: ...`, exit 4;
#   run 6 and later: accepted.
# Checked:
#   (1) run 1 is gated `host_fault`: gate_aborts and gate_rejections stay 0,
#       the task stays `reported` with its revisions unchanged, the mission is
#       not halted; the record names `evidence` `base_disagrees`, `scenario`
#       `011.sh` (the gate's own line, not the nested gate's 102.sh), `exits`
#       [1, 0], `setup_s` 244 (from the gate time line), `host_faults` 1 (and
#       the mission's gate_host_faults 1), `base_disagrees` 1 (and the
#       mission's gate_base_disagrees 1), `escalate` false, `alert` false,
#       `backoff_ms` 4000 and `due_ms` its own time + 4000.
#   (2) passes every half second do not run the gate again before `due_ms`;
#       then run 2 is gated.
#   (3) run 2 is `host_fault` again: `host_faults` 2, `base_disagrees` 2,
#       `backoff_ms` 8000, `due_ms`, `escalate` false, `alert` false,
#       gate_aborts 0, no halt (no escalation at two: checked in (5)).
#   (4) not before run 2's due time, run 3 is `host_fault` (`host_faults` 3,
#       `base_disagrees` 3 and the mission's gate_base_disagrees 3,
#       `backoff_ms` 16000, `escalate` false, `alert` true): three disagreeing
#       bases in a row raise the alert, recorded in that record; within 60 s
#       a digest tagged `host gate-base-disagrees-<that record's event id>`
#       reaches the supervisor, its line naming task #<id>, 011.sh,
#       `1, then 0` and `3 in a row`; after two more passes the mission is
#       still not halted, gate_aborts 0.
#   (5) control, not before run 3's due time: run 4 (exit 2 first) is
#       `aborted`, gate_aborts 1, gate_host_faults 0, gate_base_disagrees 0;
#       run 5 (`1, then 2`) is `aborted`, gate_aborts 2; run 6 is accepted and
#       the task lands; gate_aborts 0, the mission never halted, exactly one
#       base-disagreement alert delivered and no host-fault escalation.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1. BB_033_MEASURE=1 (only to show today's behaviour)
# reports every failed check and goes on (a halted mission is resumed, as a
# person would, so the control still runs), then exits 1 if any failed.
#
# Fails with 1 at (1) on the live main (b950e25; the deployed binary is built
# from it): run 1 is gated `aborted` ("the gate failed: the gate ended
# with exit status: 4") and gate_aborts goes 0 -> 1. It passes once the
# integrator, for a gate that exits 4 after the gate's own pair of lines
# (`| <name> exited 1, then 0 on the base: a harness error, not a verdict;
# the gate aborts` followed by `| stage gate aborted <ms>ms base: <name> again
# (unsolved needs two runs that agree)`), records `host_fault` with evidence
# `base_disagrees`, holds the task for 020's doubling backoff, never halts on
# them, raises the alert at the third in a row (in that record, and to the
# supervisor once), and still records every other base harness error as an
# abort.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
#
# Today (five runs, 2026-11-06): the task is reported 8.8-9.3 s after its
# assignment; run 1 is gated `aborted` ("the gate failed: the gate ended with
# exit status: 4"; gate_aborts 0 -> 1); run 2 starts in the next pass,
# 0.34-0.42 s after run 1's record, `aborted` (2), with no wait of any kind;
# run 3 0.30-0.35 s after run 2's, `aborted` (3); the next pass halts the
# mission "3 gate aborts in a row", 1.25-1.43 s after the first abort: three
# disagreeing bases halt a mission for a person in under a second and a half
# (the halt reaches the supervisor only as 013's halted-mission stall; 1800 s
# here), and no alert. No record names the scenario, its exits or the gate's setup time
# (they are only in the gate's log). Resumed by hand, runs 4 and 5 are
# aborted (gate_aborts 1, 2) and run 6 is accepted and lands, as (5)
# requires. It fails at (1) in 15-26 s; a measure run takes 31-52 s.
set -eu

asserted=""
harness() {
    echo "033: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE=${BB_033_MEASURE-}

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk date; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_033_base_$run"
SPAWN_TMUX_SOCKET="bb-033-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
TEST_URL=$SPAWN_TEST_DATABASE_URL

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
            echo "033: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "033: the integrator's last lines:" >&2
            grep -v '^\s*$' "$WORK/integrate.log" | grep -v '^| ' | tail -n 12 >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "033: the gate runs (time_ms run candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -s "$WORK/passes.log" ]; then
            echo "033: the passes (before event, gate_aborts before -> after, verdicts):" >&2
            tail -n 20 "$WORK/passes.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "033: harness: unexpected exit $rc" >&2
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
# A check that BB_033_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "033: (measure) would fail: $*"
    else
        fail "$*"
    fi
}
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || measured_fail "$3: expected [$2], got [$1]"
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
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$WORK/judged.log"

# The recording gate, by the run's number for flaky's candidate (see the
# header). It prints what scripts/rsi/gate.sh prints for each case, `| `
# prefixed, its stage lines included, and exits as it does: 4 with no
# verdict for a harness error on the base.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
has() { git --git-dir="\$1" cat-file -e "\$3:\$4" 2>/dev/null; }
cand=\$3
note() { printf '%s %s %s\n' "\$(date +%s%3N)" "\$1" "\$cand" >>'$WORK/judged.log'; }
accept() {
    note "\$1"
    echo "| == candidate: build"
    echo "| stage gate ok 900ms candidate: build"
    printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"accepted"}'
    exit 0
}
gate_time() { # <setup s>
    echo "| gate time: base 141 s (2 scenario runs, 19 results from the cache), candidate 0 s, setup \$1 s (exports, socket scan, static checks), total \$((\$1 + 141)) s"
}
build() {
    echo "| == base: build"
    echo "| build: ok"
    echo "| stage gate ok 10512ms base: build"
}
disagrees() { # <setup s> [nested]
    build
    echo "| == base: 011.sh"
    echo "| blackbox: (1) coverage: the latest observation of session_resident_core is based on seq 41, its latest fact is seq 44"
    echo "| stage gate exit 1 74210ms base: 011.sh"
    echo "| == base: 011.sh again (unsolved needs two runs that agree)"
    if [ -n "\${2-}" ]; then
        # A nested gate's output, as 024.sh's toy gates print it.
        echo "| | 102.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts"
    fi
    echo "| 011: covered within 10 s, delivered within 30 s, no churn"
    echo "| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; the gate aborts"
    echo "| stage gate aborted 66871ms base: 011.sh again (unsolved needs two runs that agree)"
    gate_time "\$1"
    exit 4
}
if has "\$1" "\$2" "\$3" flaky.txt; then
    n=\$(cat '$WORK/runs' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$WORK/runs'
    case "\$n" in
    1) note run-1; disagrees 244 nested ;;
    2) note run-2; disagrees 31 ;;
    3) note run-3; disagrees 29 ;;
    4)
        note run-4
        build
        echo "| == base: 011.sh"
        echo "| 011: harness: init-db failed 5 times: could not connect to the test database"
        echo "| 011.sh exited 2 on the base: a harness error, not a verdict; the gate aborts"
        echo "| stage gate aborted 5120ms base: 011.sh"
        gate_time 30
        exit 4
        ;;
    5)
        note run-5
        build
        echo "| == base: 011.sh"
        echo "| stage gate exit 1 70112ms base: 011.sh"
        echo "| == base: 011.sh again (unsolved needs two runs that agree)"
        echo "| 011: harness: the database is unreachable (while checking: (1) coverage)"
        echo "| 011.sh exited 1, then 2 on the base: a harness error, not a verdict; the gate aborts"
        echo "| stage gate aborted 12003ms base: 011.sh again (unsolved needs two runs that agree)"
        gate_time 28
        exit 4
        ;;
    *) accept "run-\$n" ;;
    esac
fi
accept other
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a text naming `BB:WRITE` writes `<name>.txt`, waits for
# the pusher to publish it (at most 30 s), then writes the report. Anything
# else is a turn that changes nothing.
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
name=${SPAWN_SESSION_ID#session_resident_}
agent=agent_resident_$name
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:WRITE"*) job=write ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      file=$name.txt
      printf '%s was here\n' "$name" >"$file"
      turn_ends "wrote $file"
      start=$(date +%s)
      until [ "$(git --git-dir="$BB_WORK/source.git" show "refs/heads/spawn/$agent:$file" 2>/dev/null)" = "$(cat "$file")" ]; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      job=
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
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG \
    SPAWN_GATE_HOST_FAULT_BACKOFF_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
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
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
# The stall bounds out of the way: only the base-disagreement alert's digest
# is checked.
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
# The first host fault's backoff (it doubles with each in a row).
BACKOFF_SECS=4
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
    SPAWN_MISSION_FILE SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/integrate.log"
: >"$WORK/passes.log"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
now_ms() { echo $(($(date +%s%N) / 1000000)); }
q() { scalar "$1"; }
task_state() { q "select state from mission_tasks where id = $1"; }
revisions() { q "select revisions from mission_tasks where id = $1"; }
rejections() { q "select gate_rejections from mission_missions where id = $mission"; }
aborts() { q "select gate_aborts from mission_missions where id = $mission"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
# 020's column; empty where the store has none (today).
host_faults() {
    q "select coalesce((to_jsonb(m) ->> 'gate_host_faults'), '') from mission_missions m where id = $mission"
}
# This objective's column; empty where the store has none (today).
base_disagrees() {
    q "select coalesce((to_jsonb(m) ->> 'gate_base_disagrees'), '') from mission_missions m where id = $mission"
}
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 6 | tr '\n' ' '; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
field() { q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events where id = $1"; }
recorded_ms() { q "select recorded_at_ms from spawn_events where id = $1"; }
# The time (ms) the gate run `$1` (run-<n>) started, from the recording gate.
run_at() { awk -v name="$1" '$2 == name { print $1; exit }' "$WORK/judged.log"; }
runs() { cat "$WORK/runs" 2>/dev/null || echo 0; }

# The session is judged waiting at its prompt, live.
waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}
# The supervisor gets a digest only while its latest judgement covers its
# latest fact: a line is typed into it now and then (not as `human`: no
# news), as objective 013's scenario does.
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
# The digests delivered to the supervisor whose tags match `$1` (SQL like).
DIGEST_TAGS="select c.id, tag from spawn_commands c,
                lateral jsonb_array_elements_text(
                    case when jsonb_typeof(c.action -> 'tags') = 'array'
                         then c.action -> 'tags' else '[]'::jsonb end) tag
              where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
                and c.action ->> 'type' = 'input' and c.state = 'finished' and c.succeeded"
digests_like() { q "select count(*) from ($DIGEST_TAGS) d where d.tag like '$1'"; }
digest_text_tagged() {
    sql "select action ->> 'text' from spawn_commands
          where id = (select min(d.id) from ($DIGEST_TAGS) d where d.tag = '$1')"
}

# One pass, with 018's checks met if they exist: the real test database,
# no disk or memory floor to speak of.
integrate_once() {
    before_count=$(aborts)
    before_event=$(last_event)
    capture env SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_HOST_FAULT_BACKOFF_SECS="$BACKOFF_SECS" \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate exited $status: $(err | tail -n 3)"
    printf '%s %s -> %s %s\n' "$before_event" "$before_count" "$(aborts)" \
        "$(q "select coalesce(string_agg((kind -> 'detail' -> 'record' ->> 'id') || ':' ||
                                         (kind -> 'detail' -> 'record' ->> 'verdict'), ';' order by id), '')
               from spawn_events where session_id = 'session_control' and id > $before_event
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'")" >>"$WORK/passes.log"
}

# The next gate verdict (not `stale`) on task `$1` after event `$2`, passes
# run every half second until it comes: sets `event`, `verdict`, and
# `before` / `after` (the mission's gate_aborts around the pass that
# recorded it). 1 when the mission halted first (nothing is judged then).
judge_next() {
    deadline=$(($(now) + 90))
    while :; do
        integrate_once
        found=$(q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'verdict') from spawn_events e
                    where e.session_id = 'session_control'
                      and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                      and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale'
                      and e.kind -> 'detail' -> 'record' ->> 'id' = '$1' and e.id > $2
                    order by e.id limit 1")
        if [ -n "$found" ]; then
            set -- $found
            event=$1
            verdict=$2
            before=$before_count
            after=$(aborts)
            return 0
        fi
        [ -z "$(halted)" ] || return 1
        [ "$(now)" -lt "$deadline" ] ||
            harness "task #$1 got no verdict within 90 s of passes (tasks: $(tasks_line); integrate: $(log_tail))"
        sleep 0.5
    done
}
# Under BB_033_MEASURE=1 only: a halted mission is resumed, as a person
# would, so that the checks after it still run.
resume_if_halted() {
    reason=$(halted)
    [ -n "$reason" ] || return 0
    capture "$SPAWN_BIN" mission resume --id "$mission"
    [ "$status" -eq 0 ] || harness "mission resume failed: $(err)"
    echo "033: (measure) the mission was halted ($reason); resumed by hand"
}
# A judging that the halt stopped: in a measure run the mission is resumed
# and the passes go on; otherwise that is the failure.
judge_or_halt() { # <task> <after event> <check>
    if judge_next "$1" "$2"; then return 0; fi
    measured_fail "$3: the mission halted before task #$1 was judged again: [$(halted)]"
    resume_if_halted
    judge_next "$1" "$2" || harness "task #$1 was not judged after the resume"
}

# --- one mission, one resident, one reported task ---------------------------------

capture "$SPAWN_BIN" mission add --text "033 a base that disagrees with itself" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" resident propose --name flaky --charter "Owns the 033 change."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name flaky
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
started=$(now)
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker flaky \
    --brief "Write your file. BB:WRITE"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(jq -r .task "$WORK/out.txt")
[ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
assigned=$(now_ms)

deadline=$(($(now) + 120))
until [ "$(task_state "$task")" = reported ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "task #$task was not reported within 120 s (tasks: $(tasks_line))"
    sleep 0.5
done
echo "033: task #$task reported $(($(now_ms) - assigned)) ms after its assignment"
git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_flaky:flaky.txt" 2>/dev/null ||
    harness "flaky's pushed branch does not hold flaky.txt"
deadline=$(($(now) + 120))
until waiting session_supervisor; do
    [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
    sleep 0.5
done

# --- (1) a base that disagrees with itself -> host_fault ----------------------------------

start=$(last_event)
revisions_before=$(revisions "$task")
rejections_before=$(rejections)
judge_next "$task" "$start" || harness "the mission halted before task #$task was judged"
e1=$event
t1=$(recorded_ms "$e1")
if [ "$verdict" != host_fault ]; then
    measured_fail "(1) task #$task, whose gate found 011.sh exiting 1 and then 0 on the base (a base that disagrees with itself, before any candidate code ran), was gated '$verdict' ($(field "$e1" reason)), not 'host_fault'; gate_aborts $before -> $after, toward the halt after three"
fi
expect_eq "$after" "$before" "(1) gate_aborts after the disagreeing base (was $before)"
expect_eq "$(rejections)" "$rejections_before" "(1) gate_rejections after the disagreeing base"
expect_eq "$(task_state "$task")" reported "(1) task #$task's state after its disagreeing base"
expect_eq "$(revisions "$task")" "$revisions_before" "(1) task #$task's revisions (nothing went back to the worker)"
expect_eq "$(halted)" "" "(1) the mission's halt after one disagreeing base"
expect_eq "$(field "$e1" evidence)" base_disagrees "(1) the host fault's evidence"
expect_eq "$(field "$e1" scenario)" 011.sh "(1) the scenario the host fault names (the gate's own line, not a nested gate's)"
expect_eq "$(q "select coalesce((kind -> 'detail' -> 'record' -> 'exits')::text, '') from spawn_events where id = $e1")" \
    "[1, 0]" "(1) the exits the host fault names"
expect_eq "$(field "$e1" setup_s)" 244 "(1) the gate's setup time the host fault names (its gate time line)"
expect_eq "$(field "$e1" host_faults)" 1 "(1) the host fault's count in a row"
expect_eq "$(host_faults)" 1 "(1) the mission's gate_host_faults"
expect_eq "$(field "$e1" escalate)" false "(1) the first host fault's escalate"
expect_eq "$(field "$e1" base_disagrees)" 1 "(1) the base disagreements in a row the first host fault names"
expect_eq "$(base_disagrees)" 1 "(1) the mission's gate_base_disagrees"
expect_eq "$(field "$e1" alert)" false "(1) the first host fault's alert"
expect_eq "$(field "$e1" backoff_ms)" $((BACKOFF_SECS * 1000)) "(1) the first host fault's backoff"
due1=$(field "$e1" due_ms)
expect_eq "$due1" $((t1 + BACKOFF_SECS * 1000)) "(1) the first host fault's due time"

# --- (2) judged again only after the backoff -------------------------------------------------

judge_or_halt "$task" "$e1" "(2)"
e2=$event
t2=$(recorded_ms "$e2")
ran=$(run_at run-2)
echo "033: run 2 started $((ran - t1)) ms after run 1's record ($(field "$e1" verdict)), gated '$verdict' (gate_aborts $before -> $after)"
case "$due1" in
'' | *[!0-9]*)
    measured_fail "(2) task #$task's gate ran again $((ran - t1)) ms after its first disagreeing base, with no due time to wait for (the record names no due_ms)"
    ;;
*)
    [ "$ran" -ge "$due1" ] ||
        measured_fail "(2) task #$task's gate ran again at $ran ms, before its host fault's due time $due1 ms"
    ;;
esac

# --- (3) the second in a row: no escalation at two -------------------------------------------

if [ "$verdict" != host_fault ]; then
    measured_fail "(3) task #$task's second disagreeing base in a row was gated '$verdict' ($(field "$e2" reason)), not 'host_fault'; gate_aborts $before -> $after"
fi
expect_eq "$after" "$before" "(3) gate_aborts after the second disagreeing base"
expect_eq "$(field "$e2" evidence)" base_disagrees "(3) the second host fault's evidence"
expect_eq "$(field "$e2" host_faults)" 2 "(3) the second host fault's count in a row"
expect_eq "$(host_faults)" 2 "(3) the mission's gate_host_faults after two in a row"
expect_eq "$(field "$e2" backoff_ms)" $((2 * BACKOFF_SECS * 1000)) "(3) the second host fault's backoff (doubled)"
due2=$(field "$e2" due_ms)
expect_eq "$due2" $((t2 + 2 * BACKOFF_SECS * 1000)) "(3) the second host fault's due time"
expect_eq "$(field "$e2" escalate)" false "(3) the second host fault's escalate (no escalation at two: the owner's ruling)"
expect_eq "$(field "$e2" base_disagrees)" 2 "(3) the base disagreements in a row the second host fault names"
expect_eq "$(base_disagrees)" 2 "(3) the mission's gate_base_disagrees after two in a row"
expect_eq "$(field "$e2" alert)" false "(3) the second host fault's alert"
expect_eq "$(halted)" "" "(3) the mission's halt after two disagreeing bases in a row"

# --- (4) three in a row raise an alert and never halt ------------------------------------------

judge_or_halt "$task" "$e2" "(4)"
e3=$event
t3=$(recorded_ms "$e3")
ran=$(run_at run-3)
echo "033: run 3 started $((ran - t2)) ms after run 2's record, gated '$verdict' (gate_aborts $before -> $after)"
case "$due2" in
'' | *[!0-9]*) measured_fail "(4) task #$task's gate ran a third time $((ran - t2)) ms after its second disagreeing base, with no due time to wait for" ;;
*) [ "$ran" -ge "$due2" ] || measured_fail "(4) task #$task's gate ran a third time at $ran ms, before its second host fault's due time $due2 ms" ;;
esac
if [ "$verdict" != host_fault ]; then
    measured_fail "(4) task #$task's third disagreeing base in a row was gated '$verdict' ($(field "$e3" reason)), not 'host_fault'; gate_aborts $before -> $after"
fi
expect_eq "$(field "$e3" host_faults)" 3 "(4) the third host fault's count in a row"
expect_eq "$(field "$e3" backoff_ms)" $((4 * BACKOFF_SECS * 1000)) "(4) the third host fault's backoff (doubled again)"
expect_eq "$(field "$e3" escalate)" false "(4) the third host fault's escalate"
expect_eq "$(field "$e3" base_disagrees)" 3 "(4) the base disagreements in a row the third host fault names"
expect_eq "$(base_disagrees)" 3 "(4) the mission's gate_base_disagrees after three in a row"
expect_eq "$(field "$e3" alert)" true "(4) the third host fault's alert (three in a row raise one, recorded in that record)"
due3=$(field "$e3" due_ms)
# Before objective 030 exists the record is the alert, and the digest takes
# it to the supervisor once.
tag="host gate-base-disagrees-$e3"
if [ "$verdict" = host_fault ]; then
    deadline=$(($(now) + 60))
    until [ "$(digests_like "$tag")" -ge 1 ]; do
        [ "$(now)" -lt "$deadline" ] || {
            measured_fail "(4) three disagreeing bases in a row on task #$task did not reach the supervisor within 60 s (no digest tagged '$tag')"
            break
        }
        ping_supervisor
        sleep 0.5
    done
    if [ "$(digests_like "$tag")" -ge 1 ]; then
        line=$(digest_text_tagged "$tag" | grep -F "task #$task" | head -n 1)
        case "$line" in
        *"task #$task"*) ;;
        *) line="" ;;
        esac
        case "$line" in
        *011.sh*) ;;
        *) line="" ;;
        esac
        case "$line" in
        *"1, then 0"*) ;;
        *) line="" ;;
        esac
        case "$line" in
        *"3 in a row"*) ;;
        *) line="" ;;
        esac
        [ -n "$line" ] ||
            measured_fail "(4) the alert of three disagreeing bases does not name task #$task, 011.sh, its exits (1, then 0) and \`3 in a row\`: [$(digest_text_tagged "$tag" | head -n 3 | tr '\n' ' ')]"
    fi
else
    measured_fail "(4) the supervisor was not told of three disagreeing bases in a row (no host fault was recorded)"
fi
# Two more passes: a halt rule fires at the start of a pass.
integrate_once
sleep 0.3
integrate_once
reason=$(halted)
if [ -n "$reason" ]; then
    measured_fail "(4) three disagreeing bases in a row halted the mission ($reason), $(($(q "select max(recorded_at_ms) from spawn_events where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'") - t1)) ms after the first; a host fault never halts"
    resume_if_halted
fi
expect_eq "$(aborts)" 0 "(4) gate_aborts after three disagreeing bases"

# --- (5) control: a real harness error on the base is still an abort ---------------------

judge_or_halt "$task" "$e3" "(5)"
e4=$event
ran=$(run_at run-4)
case "$due3" in
'' | *[!0-9]*) ;;
*) [ "$ran" -ge "$due3" ] || measured_fail "(5) task #$task's gate ran a fourth time at $ran ms, before its third host fault's due time $due3 ms" ;;
esac
[ "$verdict" = aborted ] ||
    fail "(5) task #$task, whose base run of 011.sh exited 2 (a harness error, not a disagreement), was gated '$verdict' ($(field "$e4" reason)), not 'aborted'"
assert_eq "$after" "$((before + 1))" "(5) gate_aborts after a base that exited 2 (was $before)"
expect_eq "$(host_faults)" 0 "(5) the mission's gate_host_faults after a real abort"
expect_eq "$(base_disagrees)" 0 "(5) the mission's gate_base_disagrees after a real abort"
judge_or_halt "$task" "$e4" "(5)"
e5=$event
[ "$verdict" = aborted ] ||
    fail "(5) task #$task, whose base run of 011.sh exited 1 and then 2 (a harness error, not a disagreement), was gated '$verdict' ($(field "$e5" reason)), not 'aborted'"
assert_eq "$after" "$((before + 1))" "(5) gate_aborts after a base that exited 1, then 2 (was $before)"
judge_or_halt "$task" "$e5" "(5)"
assert_eq "$verdict" accepted "(5) task #$task's verdict on its sixth gate run"
deadline=$(($(now) + 60))
until [ "$(task_state "$task")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] || fail "(5) task #$task was accepted but did not land within 60 s"
    integrate_once
    sleep 0.5
done
assert_eq "$(aborts)" 0 "(5) gate_aborts after the landing"
expect_eq "$(halted)" "" "(5) the mission's halt at the end"
expect_eq "$(digests_like 'host gate-base-disagrees-%')" 1 "(5) base-disagreement alerts delivered"
expect_eq "$(digests_like 'host gate-host-fault-%')" 0 "(5) host-fault escalations delivered (none at two for a base that disagrees with itself)"
expect_eq "$(q "select count(*) from spawn_events where session_id = 'session_control'
                  and kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'
                  and kind -> 'detail' -> 'record' ->> 'id' = '$mission'")" 0 \
    "(5) halts of the mission"
echo "033: the run took $(($(now) - started)) s"

if [ -n "$failed_any" ]; then
    asserted=1
    exit 1
fi
echo "033: three bases that disagreed with themselves were recorded as host faults, not counted, judged again after a doubling backoff, never escalated at two, alerted once at three in a row, and never halted the mission; real harness errors on the base stayed aborts"
exit 0
