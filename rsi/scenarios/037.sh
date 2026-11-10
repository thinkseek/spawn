#!/bin/sh
# Black box: an integrator asked to stop drains instead of killing. Today a
# restart of spawn-integrate kills every running gate, which starts again
# from nothing: on 2026-11-09 at 10:33 a deploy restarted it 22 minutes
# into task 210's gate (70 minutes in all); objective 021 measured task 48
# losing a 40-minute gate to a stop. Gates run 17 minutes on average and up
# to 101. A gate may not simply outlive the integrator (warm.rs: nothing
# the integrator starts outlives it unaccounted), so the integrator must
# finish what it started before it goes, within a bound.
#
# Required: a drain request (SIGUSR1 to the integrator, or `spawn integrate
# --drain` with the same --root, which sends it to the integrator running
# there and waits until that process is gone, then exits 0) makes the
# integrator record `integrator.draining` {`gates`: the tasks whose gates
# run, as numbers, `bound_s`}, start no gate and no base warm (nor any other
# background judging) from then on, and keep its passes going otherwise
# (records, holds, assignment, the landing of a verdict that arrives). Once
# no gate of its runs and the verdicts they recorded were acted on (an
# accepted task landed), it records `integrator.drained` and exits 0.
# SPAWN_INTEGRATE_DRAIN_SECS (default 7200; a whole number from 1 to 86400,
# any other value refused at start: the integrator exits non-zero naming it
# and judges nothing) bounds the drain: at the bound the integrator stops
# the gates still running (their process groups, as a gate past its time
# limit is stopped: nothing of theirs outlives it), records nothing for
# them, records `integrator.drain_expired` {`gates`: those tasks,
# `bound_s`} and exits 0. The dispatcher's digest tells the supervisor of
# an expired drain once (session `host`, tag
# `host integrator-drain-expired-<event id>`, its line naming each task as
# `task #<id>`). TERM keeps today's behaviour: the process stops at once
# and records no drain.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, the
# supervisor, no model judge) runs two residents on a fake engine: `hold` in
# mission A on source SA and `wait` in mission B on source SB, both accept =
# gate with one gate directory marked `no-test-database` (so the two gates
# can run at once: one per source); the supervisor runs on the same fake
# engine. Each `spawn integrate` judges through a recording gate (SPAWN_GATE)
# that notes each run (a base warm, `--base-only`, is noted as `warm`): task
# A's candidate (held.txt) is held until $WORK/release-a, task B's
# (wait.txt) until $WORK/release-b, which never comes. The base warm is on.
#   (1) task A's gate runs; `spawn integrate --drain --root <its root>`:
#       within 15 s `integrator.draining` names gates [A] and bound_s 7200
#       (the default), while `--drain` still waits.
#   (2) task B is assigned and reported while A's gate is held: over 12 s
#       (two passes and more) no gate runs for B and B has no `task.gated`;
#       the integrator is still running.
#   (3) A's gate is released: A is gated `accepted` and lands (`task.landed`),
#       then `integrator.drained` is recorded (after the landing), the
#       integrator exits 0 within 30 s and `--drain` exits 0 once it is gone;
#       no gate ever ran for B, no base warm ran for A's landing, B is still
#       `reported`.
#   (4) SPAWN_INTEGRATE_DRAIN_SECS of 0, 86401, `x` and 1.5: each integrator
#       exits non-zero within 15 s naming SPAWN_INTEGRATE_DRAIN_SECS, records
#       nothing and runs no gate.
#   (5) an integrator with SPAWN_INTEGRATE_DRAIN_SECS=3 judges B (held);
#       SIGUSR1: `integrator.draining` names [B] and bound_s 3; within 20 s
#       `integrator.drain_expired` names [B], recorded at least 3 s after the
#       draining record; the integrator exits 0; within 5 s B's gate process
#       is gone; no `task.gated` for B was recorded.
#   (6) within 60 s one digest tagged `host integrator-drain-expired-<id>`
#       reaches the supervisor, its text naming task #B; 15 s of pings later
#       still exactly one, and no other drain digest.
#   (7) an integrator (default bound) judges B again (held); TERM: it exits
#       within 5 s and records no `integrator.draining` nor
#       `integrator.drained`.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (984d059): `spawn integrate --drain`
# is refused by the usage ("spawn integrate [--once] ..."), and no
# `integrator.draining` is ever recorded. It passes once a drain request
# lets running gates finish and land without starting new ones, within a
# bound that stops what is left, and is told to the supervisor when it
# expires.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "037: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk date; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
[ -r /proc/self/stat ] || harness "no /proc: a gate's process cannot be seen"

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_037_drain_$run"
SPAWN_TMUX_SOCKET="bb-037-$run"
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
integrator_pid=""
wrapper_pid=""
drain_pid=""
started=$(date +%s)
cleanup() {
    : >"$WORK/release-a" 2>/dev/null || true
    : >"$WORK/release-b" 2>/dev/null || true
    for pid in "$drain_pid" "$integrator_pid" "$wrapper_pid" "$loop_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    # A held gate an integrator left behind (today's TERM leaves its group).
    for file in "$WORK"/gate-*.pid; do
        [ -f "$file" ] || continue
        pid=$(cat "$file" 2>/dev/null) || continue
        kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
    done
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
        echo "037: ended after $(($(date +%s) - started)) s" >&2
        if [ -s "$WORK/loop.log" ]; then
            echo "037: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "037: the integrators' last lines:" >&2
            grep -v '^\s*$' "$WORK/integrate.log" | grep -v '^| ' | tail -n 14 >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "037: the gate runs (time_ms name base candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "037: harness: unexpected exit $rc" >&2
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
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || fail "$3: expected [$2], got [$1]"
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
sa=$WORK/sa.git
sb=$WORK/sb.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$sa" "$sb"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# The gate directory: no test database, so the two sources' gates may run
# at once (one per source) and neither waits for the shared server.
gate=$WORK/ctl/rsi/scenarios
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
: >"$gate/no-test-database"
: >"$WORK/judged.log"

# The recording gate (see the header). A held run writes its pid (the
# leader of its process group) to gate-<name>.pid and waits, at most 150 s,
# for its release file.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
note() { printf '%s %s %s %s\n' "\$(date +%s%3N)" "\$1" "\$3" "\$4" >>'$WORK/judged.log'; }
if [ "\$1" = --base-only ]; then
    note warm "\$@"
    echo "| base only: nothing to do"
    exit 0
fi
has() { git --git-dir="\$1" cat-file -e "\$2:\$3" 2>/dev/null; }
hold() { # <name> <release file>
    echo \$\$ >'$WORK'/gate-\$1.pid
    i=0
    while [ ! -e "\$2" ] && [ "\$i" -lt 1500 ]; do
        sleep 0.1
        i=\$((i + 1))
    done
}
verdict() {
    echo "| == candidate: build"
    echo "| stage gate ok 900ms candidate: build"
    printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
    echo '{"verdict":"accepted"}'
    exit 0
}
echo "| == setup"
if has "\$1" "\$3" held.txt; then
    note a "\$@"
    hold a '$WORK/release-a'
    verdict
fi
if has "\$1" "\$3" wait.txt; then
    n=\$(cat '$WORK/runs-b' 2>/dev/null || echo 0)
    n=\$((n + 1))
    echo "\$n" >'$WORK/runs-b'
    note "b-\$n" "\$@"
    hold "b-\$n" '$WORK/release-b'
    verdict
fi
note other "\$@"
verdict
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a resident's text naming `BB:WRITE:<file>` writes
# `<file>.txt`, waits for the pusher to publish it on either source (at most
# 30 s), then writes the report. Anything else is a turn that changes
# nothing.
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
published() {
  for source in "$BB_WORK/sa.git" "$BB_WORK/sb.git"; do
    [ "$(git --git-dir="$source" show "refs/heads/spawn/$agent:$1" 2>/dev/null)" = "$(cat "$1")" ] && return 0
  done
  return 1
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
    case "$SPAWN_SESSION_ID:$typed" in
    session_resident_*"BB:WRITE:"*)
      job=${typed#*BB:WRITE:}
      job=${job%%[!a-z]*}
      ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      file=$job.txt
      printf '%s wrote %s\n' "$name" "$job" >"$file"
      turn_ends "wrote $file"
      start=$(date +%s)
      until published "$file"; do
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
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB SPAWN_GATE_JOBS \
    SPAWN_GATE_WORKER_CORES SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES \
    SPAWN_TEST_DATABASE_LOG SPAWN_INTEGRATE_DRAIN_SECS SPAWN_GATE_ENV_FILE \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS \
    SPAWN_STALL_HALTED_SECS SPAWN_INTEGRATE_WARM; do
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
# The stall bounds out of the way: nothing here is about stalls.
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
# The base warm is on (the default), its pid file and logs under $WORK.
XDG_CACHE_HOME=$WORK/cache
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR \
    SPAWN_MISSION_FILE SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS XDG_CACHE_HOME
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator" "$XDG_CACHE_HOME"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/integrate.log"

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
q() { scalar "$1"; }
task_state() { q "select state from mission_tasks where id = $1"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 6 | tr '\n' ' '; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
RECORDS="from spawn_events e where e.session_id = 'session_control'
           and e.event_name = 'control.recorded'"
# The first record of type $1 after event $2 ('' when none); with $3, only
# one about task $3 (its `id`).
record_after() {
    about=""
    [ -z "${3-}" ] || about="and e.kind -> 'detail' -> 'record' ->> 'id' = '$3'"
    q "select coalesce(min(e.id)::text, '') $RECORDS
         and e.kind -> 'detail' -> 'record' ->> 'type' = '$1' and e.id > $2 $about"
}
records_after() { # <type> <event> [task]: how many
    about=""
    [ -z "${3-}" ] || about="and e.kind -> 'detail' -> 'record' ->> 'id' = '$3'"
    q "select count(*) $RECORDS and e.kind -> 'detail' -> 'record' ->> 'type' = '$1'
         and e.id > $2 $about"
}
integrator_records_after() { # <event>: how many integrator.* records
    q "select count(*) $RECORDS and e.kind -> 'detail' -> 'record' ->> 'type' like 'integrator.%'
         and e.id > $1"
}
# A path into event $1's record (jsonb #> '{a,b}') as JSON text; `absent`
# when the path does not exist.
json_at() {
    q "select coalesce((kind -> 'detail' -> 'record' #> '{$2}')::text, 'absent')
         from spawn_events where id = $1"
}
recorded_ms() { q "select recorded_at_ms from spawn_events where id = $1"; }
main_of() { git --git-dir="$1" rev-parse main; }
ran() { awk -v name="$1" '$2 == name { n++ } END { print n + 0 }' "$WORK/judged.log"; }
ran_like() { awk -v prefix="$1" 'index($2, prefix) == 1 { n++ } END { print n + 0 }' "$WORK/judged.log"; }
# A process that is gone, or a zombie no one reaped.
gone() {
    [ ! -e "/proc/$1" ] || sed -n 's/^[0-9]* (.*) \([A-Z]\) .*/\1/p' "/proc/$1/stat" 2>/dev/null | grep -qx Z
}

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

# An integrator, started fresh: its real pid in $integrator_pid, its exit
# status in $WORK/integrate.exit once it ends. `$@`: extra VAR=value.
start_integrator() {
    rm -f "$WORK/integrate.pid" "$WORK/integrate.exit"
    (
        env "$@" SPAWN_GATE="$WORK/fake-gate.sh" SPAWN_TEST_DATABASE_URL="$TEST_URL" \
            SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 SPAWN_GATE_JOBS=1 \
            SPAWN_GATE_WORKER_CORES=0 \
            "$SPAWN_BIN" integrate --root "$WORK/integrator" >>"$WORK/integrate.log" 2>&1 &
        echo "$!" >"$WORK/integrate.pid.new"
        mv "$WORK/integrate.pid.new" "$WORK/integrate.pid"
        rc=0
        wait "$!" || rc=$?
        echo "$rc" >"$WORK/integrate.exit.new"
        mv "$WORK/integrate.exit.new" "$WORK/integrate.exit"
    ) &
    wrapper_pid=$!
    i=0
    until [ -s "$WORK/integrate.pid" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "the integrator did not start"
        sleep 0.1
    done
    integrator_pid=$(cat "$WORK/integrate.pid")
}
integrator_exited() { [ -s "$WORK/integrate.exit" ]; }
integrator_exit() { cat "$WORK/integrate.exit"; }
# wait_exit <seconds>: true once the integrator has exited within the time.
wait_exit() {
    deadline=$(($(now) + $1))
    until integrator_exited; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.2
    done
    wait "$wrapper_pid" 2>/dev/null || true
    integrator_pid=""
    wrapper_pid=""
}
# wait_run <name> <seconds>: the gate run named $1 has begun (judged.log).
wait_run() {
    deadline=$(($(now) + $2))
    until [ "$(ran "$1")" -ge 1 ]; do
        integrator_exited && harness "the integrator exited before gate run $1 began: $(log_tail)"
        [ "$(now)" -lt "$deadline" ] ||
            harness "gate run $1 did not begin within $2 s (tasks: $(tasks_line); integrate: $(log_tail))"
        sleep 0.2
    done
    j=0
    until [ -s "$WORK/gate-$1.pid" ]; do
        j=$((j + 1))
        [ "$j" -lt 50 ] || harness "gate run $1 wrote no pid"
        sleep 0.1
    done
}

# --- two missions, two residents, task A reported ---------------------------------

for name in hold wait; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 037 $name change."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
new_mission() { # <text> <source>
    capture "$SPAWN_BIN" mission add --text "$1" --source "$2"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    m=$(jq -r .mission "$WORK/out.txt")
    [ -n "$m" ] && [ "$m" != null ] || harness "mission add printed no id: $(out)"
    capture "$SPAWN_BIN" mission configure --id "$m" --gate "$gate" --accept gate
    [ "$status" -eq 0 ] || harness "mission configure $m exited $status: $(err)"
    echo "$m"
}
mission_a=$(new_mission "037 A" "$sa")
mission_b=$(new_mission "037 B" "$sb")
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

# assign_and_report <mission> <resident> <file name>: sets `task`; waits for
# its report (an integrator may judge it the moment it comes: the wait also
# ends at its first gate run, named $4 when given).
assign_and_report() {
    capture "$SPAWN_BIN" task assign --mission "$1" --kind resident --worker "$2" \
        --brief "Write your file. BB:WRITE:$3"
    [ "$status" -eq 0 ] || harness "task assign failed: $(err)"
    task=$(jq -r .task "$WORK/out.txt")
    [ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
    deadline=$(($(now) + 120))
    until [ "$(task_state "$task")" = reported ]; do
        [ -n "${4-}" ] && [ "$(ran "$4")" -ge 1 ] && break
        [ "$(now)" -lt "$deadline" ] ||
            harness "task #$task was not reported within 120 s (tasks: $(tasks_line))"
        sleep 0.3
    done
}

assign_and_report "$mission_a" hold held
task_a=$task
deadline=$(($(now) + 120))
until waiting session_supervisor; do
    [ "$(now)" -lt "$deadline" ] || harness "the supervisor was never judged waiting"
    sleep 0.5
done
main_a=$(main_of "$sa")
start_integrator
wait_run a 60
first=$(last_event)

# --- (1) a drain request while A's gate runs --------------------------------------

(
    rc=0
    "$SPAWN_BIN" integrate --drain --root "$WORK/integrator" >"$WORK/drain.out" 2>&1 || rc=$?
    echo "$rc" >"$WORK/drain.exit.new"
    mv "$WORK/drain.exit.new" "$WORK/drain.exit"
) &
drain_pid=$!
deadline=$(($(now) + 15))
draining=""
until [ -n "$draining" ]; do
    draining=$(record_after integrator.draining "$first")
    [ -n "$draining" ] && break
    if [ "$(now)" -ge "$deadline" ]; then
        fail "(1) no integrator.draining within 15 s of \`spawn integrate --drain\` (it said: [$(head -c 300 "$WORK/drain.out" | tr '\n' ' ')])"
    fi
    sleep 0.2
done
expect_eq "$(json_at "$draining" gates)" "[$task_a]" "(1) integrator.draining's gates (task A's gate runs)"
expect_eq "$(json_at "$draining" bound_s)" 7200 "(1) integrator.draining's bound_s (the default)"
[ ! -e "$WORK/drain.exit" ] ||
    fail "(1) \`spawn integrate --drain\` exited ($(cat "$WORK/drain.exit")) while the integrator still drains"

# --- (2) no gate starts while draining ----------------------------------------------

assign_and_report "$mission_b" wait wait b-1
task_b=$task
deadline=$(($(now) + 12))
while [ "$(now)" -lt "$deadline" ]; do
    [ "$(ran_like b-)" -eq 0 ] ||
        fail "(2) a gate started for task B (#$task_b) while the integrator drained"
    integrator_exited &&
        fail "(2) the integrator exited ($(integrator_exit)) while task A's gate still ran: $(log_tail)"
    sleep 0.3
done
expect_eq "$(records_after task.gated "$first" "$task_b")" 0 "(2) task.gated records for task B while draining"

# --- (3) the running gate finishes, lands, then the integrator exits 0 -----------

: >"$WORK/release-a"
wait_exit 30 ||
    fail "(3) the integrator did not exit within 30 s of task A's gate ending (tasks: $(tasks_line); integrate: $(log_tail))"
expect_eq "$(integrator_exit)" 0 "(3) the drained integrator's exit status"
gated_a=$(record_after task.gated "$first" "$task_a")
[ -n "$gated_a" ] || fail "(3) task A (#$task_a) has no verdict: the drain did not let its gate record"
expect_eq "$(json_at "$gated_a" verdict)" '"accepted"' "(3) task A's verdict"
landed_a=$(record_after task.landed "$first" "$task_a")
[ -n "$landed_a" ] || fail "(3) task A (#$task_a) was accepted but did not land before the integrator exited"
drained=$(record_after integrator.drained "$draining")
[ -n "$drained" ] || fail "(3) no integrator.drained record"
[ "$drained" -gt "$landed_a" ] ||
    fail "(3) integrator.drained ($drained) came before task A's landing ($landed_a)"
[ "$(main_of "$sa")" != "$main_a" ] || fail "(3) SA's main did not move with task A's landing"
deadline=$(($(now) + 10))
until [ -s "$WORK/drain.exit" ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(3) \`spawn integrate --drain\` did not exit within 10 s of the integrator"
    sleep 0.2
done
wait "$drain_pid" 2>/dev/null || true
drain_pid=""
expect_eq "$(cat "$WORK/drain.exit")" 0 "(3) \`spawn integrate --drain\`'s exit status"
expect_eq "$(ran_like b-)" 0 "(3) gate runs for task B (#$task_b) during the drain"
expect_eq "$(ran warm)" 0 "(3) base warms started after task A's landing, during the drain"
expect_eq "$(task_state "$task_b")" reported "(3) task B's state"

# --- (4) the bound's value is checked at start -----------------------------------

before=$(last_event)
for value in 0 86401 x 1.5; do
    start_integrator SPAWN_INTEGRATE_DRAIN_SECS="$value"
    if ! wait_exit 15; then
        fail "(4) an integrator with SPAWN_INTEGRATE_DRAIN_SECS=$value still ran after 15 s"
    fi
    [ "$(integrator_exit)" != 0 ] ||
        fail "(4) an integrator with SPAWN_INTEGRATE_DRAIN_SECS=$value exited 0"
    tail -n 5 "$WORK/integrate.log" | grep -q SPAWN_INTEGRATE_DRAIN_SECS ||
        fail "(4) an integrator refusing SPAWN_INTEGRATE_DRAIN_SECS=$value did not name it: $(log_tail)"
done
expect_eq "$(q "select count(*) $RECORDS and e.id > $before")" 0 "(4) records by refused integrators"
expect_eq "$(ran_like b-)" 0 "(4) gate runs by refused integrators"

# --- (5) the bound: what still runs is stopped, recorded, and the integrator exits 0 --

start_integrator SPAWN_INTEGRATE_DRAIN_SECS=3
wait_run b-1 30
gate_b1=$(cat "$WORK/gate-b-1.pid")
mark=$(last_event)
kill -USR1 "$integrator_pid" 2>/dev/null || harness "cannot signal the integrator"
deadline=$(($(now) + 15))
draining=""
until [ -n "$draining" ]; do
    draining=$(record_after integrator.draining "$mark")
    [ -n "$draining" ] && break
    [ "$(now)" -lt "$deadline" ] ||
        fail "(5) no integrator.draining within 15 s of SIGUSR1 (integrate: $(log_tail))"
    integrator_exited && fail "(5) SIGUSR1 ended the integrator ($(integrator_exit)) without a drain"
    sleep 0.2
done
expect_eq "$(json_at "$draining" gates)" "[$task_b]" "(5) integrator.draining's gates"
expect_eq "$(json_at "$draining" bound_s)" 3 "(5) integrator.draining's bound_s"
wait_exit 20 ||
    fail "(5) the integrator still ran 20 s after a drain bounded at 3 s (integrate: $(log_tail))"
expect_eq "$(integrator_exit)" 0 "(5) the exit status at the drain's bound"
expired=$(record_after integrator.drain_expired "$draining")
[ -n "$expired" ] || fail "(5) no integrator.drain_expired record at the bound"
expect_eq "$(json_at "$expired" gates)" "[$task_b]" "(5) integrator.drain_expired's gates (task B's still ran)"
waited=$(($(recorded_ms "$expired") - $(recorded_ms "$draining")))
[ "$waited" -ge 3000 ] || fail "(5) integrator.drain_expired came ${waited} ms after the drain began, before its 3 s bound"
deadline=$(($(now) + 5))
until gone "$gate_b1"; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(5) task B's gate (pid $gate_b1) outlived the integrator by 5 s"
    sleep 0.2
done
expect_eq "$(records_after task.gated "$mark" "$task_b")" 0 "(5) task.gated records for the stopped gate"
expect_eq "$(records_after integrator.drained "$draining")" 0 "(5) integrator.drained records for an expired drain"

# --- (6) the supervisor is told of the expired drain once -----------------------------

tag="host integrator-drain-expired-$expired"
deadline=$(($(now) + 60))
until [ "$(digests_like "$tag")" -ge 1 ]; do
    [ "$(now)" -lt "$deadline" ] ||
        fail "(6) the expired drain did not reach the supervisor within 60 s (no digest tagged '$tag')"
    ping_supervisor
    sleep 1
done
digest_text_tagged "$tag" | grep -F "task #$task_b" >/dev/null ||
    fail "(6) the digest about the expired drain does not name task #$task_b: [$(digest_text_tagged "$tag" | head -n 4 | tr '\n' ' ')]"
deadline=$(($(now) + 15))
while [ "$(now)" -lt "$deadline" ]; do
    ping_supervisor
    sleep 1
done
expect_eq "$(digests_like "$tag")" 1 "(6) digests about the expired drain (told once)"
expect_eq "$(digests_like 'host integrator-drain-expired-%')" 1 "(6) digests about the expired drain"

# --- (7) TERM still stops at once ----------------------------------------------------

start_integrator
wait_run b-2 30
mark=$(last_event)
kill -TERM "$integrator_pid" 2>/dev/null || harness "cannot signal the integrator"
wait_exit 5 || fail "(7) the integrator still ran 5 s after TERM (integrate: $(log_tail))"
expect_eq "$(integrator_records_after "$mark")" 0 "(7) integrator.* records after TERM"

echo "037: the run took $(($(now) - started)) s"
echo "037: a drain let the running gate record its verdict and land, started no gate or warm meanwhile, then exited 0; its bound was checked, stopped what still ran at the bound and was told to the supervisor once; TERM still stops at once"
exit 0
