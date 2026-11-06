#!/bin/sh
# Black box: the integrator checks the host's resources before it runs a
# gate, instead of discovering them by the gate's crash. When free memory,
# free disk in the gate's TMPDIR, or the test database (reachable, and able
# to run a trivial parallel query) falls short, the gate is not run: the
# task is gated `no_room` (the existing path: not a verdict, not counted,
# judged again later), naming the check, the measured value and the
# threshold. When every check passes the gate runs as before.
#
# A real loop (`spawn loop`: the Bridge, its pusher, the dispatcher, no
# model judge, no supervisor) runs four residents, each with one task in a
# gated mission of its own (accept = gate) on one source, on a fake engine
# that writes `<name>.txt` and reports. `spawn integrate --once` passes, run
# by the scenario one at a time with the environment of each phase, judge
# through a recording gate (SPAWN_GATE) that accepts every candidate and
# notes every run. All four tasks are reported before the first pass. One
# judging per pass (one gate slot), and a task gated `no_room` waits out its
# backoff (minutes), so each phase judges a task no earlier phase touched:
#   (1) disk: SPAWN_GATE_MIN_FREE_GB=1000000 (a petabyte). The task judged is
#       gated `no_room` with `check` `disk`, `measured` the free KiB of the
#       gate's TMPDIR (as `df` reads it, within 5% or 1 GiB), `threshold`
#       1000000 GiB in KiB, `free_kb`/`min_gb`/`tmpdir` as today, and a
#       reason that names SPAWN_GATE_MIN_FREE_GB=1000000; the gate never ran.
#   (2) memory: SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000 (100 TB). `no_room`
#       with `check` `memory`, `measured` the MiB available (MemAvailable,
#       within a factor of two of what the scenario reads), `threshold`
#       100000000, a reason that names the measured MiB and
#       SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000; the gate never ran.
#   (3) database: SPAWN_TEST_DATABASE_URL names a socket directory where no
#       server listens. `no_room` with `check` `database`, a non-empty
#       `measured` (the error) and `threshold`, a reason that names the test
#       database; the password in the URL appears nowhere in the record; the
#       gate never ran.
#   (4) room: every threshold met (the real test database, which runs the
#       parallel query): the gate runs on the fourth task and it lands.
#   In (1)-(3) the task stays `reported`, the mission's gate_rejections and
#   gate_aborts stay 0, and the mission's no-room run names the task
#   (gate_room_task, gate_room_waits 1): it is judged again after the
#   backoff, by the path no_room already has.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (06cc205, the deployed binary built
# from it): the integrator runs the gate whatever the host has (the disk
# floor is read only by scripts/rsi/gate.sh, which a gate under
# SPAWN_GATE does not run), and the task is gated `accepted` and lands. It
# passes once the integrator, before it runs a gate, measures free memory,
# free disk in the gate's TMPDIR and the test database (connect, and a
# parallel query) against their thresholds and records `no_room` with the
# check, the measure and the threshold instead of running a gate that
# would fail for the host's sake.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "018: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep df awk; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_018_room_$run"
SPAWN_TMUX_SOCKET="bb-018-$run"
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
            echo "018: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/integrate.log" ]; then
            echo "018: the integrator's last lines:" >&2
            tail -n 15 "$WORK/integrate.log" >&2
        fi
        if [ -s "$WORK/judged.log" ]; then
            echo "018: the gate runs (time base candidate):" >&2
            cat "$WORK/judged.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "018: the gate records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'id', r ->> 'verdict', coalesce(r ->> 'check', '-'),
                       left(coalesce(r ->> 'reason', ''), 160)
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' = 'task.gated'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "018: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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

# The recording gate: accepts every candidate, notes every run.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
printf '%s %s %s\n' "\$(date +%s)" "\$2" "\$3" >>'$WORK/judged.log'
echo "== candidate: build"
echo "| build: ok"
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a prompt; a text naming `BB:WRITE` opens a turn that
# writes `<name>.txt`, waits for the pusher to publish it (at most 30 s),
# then writes the report in a second turn. It writes its transcript where
# Claude Code does.
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_GATE_MIN_FREE_GB \
    SPAWN_GATE_MIN_FREE_MEMORY_MB SPAWN_INTEGRATE_GATES; do
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
# The loop's own disk warning and floor are not what this scenario is about.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
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
mission_of() { q "select mission_id from mission_tasks where id = $1"; }
mission_field() { q "select $2 from mission_missions where id = $1"; }
tasks_line() { q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks"; }
log_tail() { tail -n 6 "$WORK/integrate.log" 2>/dev/null | tr '\n' ' '; }
runs() { grep -c . "$WORK/judged.log" || true; }
last_event() { q "select coalesce(max(id), 0) from spawn_events"; }
# The first gate record (any verdict but `stale`) after event `$1`:
# `<event id> <task> <verdict>`.
next_gated() {
    q "select e.id || ' ' || (e.kind -> 'detail' -> 'record' ->> 'id') || ' ' ||
              (e.kind -> 'detail' -> 'record' ->> 'verdict')
         from spawn_events e
        where e.session_id = 'session_control'
          and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
          and e.kind -> 'detail' -> 'record' ->> 'verdict' <> 'stale' and e.id > $1
        order by e.id limit 1"
}
field() { q "select kind -> 'detail' -> 'record' ->> '$2' from spawn_events where id = $1"; }
record_text() { q "select (kind -> 'detail' -> 'record')::text from spawn_events where id = $1"; }

# One integrator pass with the phase's environment (`env` arguments).
integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL -u SPAWN_GATE_MIN_FREE_GB -u SPAWN_GATE_MIN_FREE_MEMORY_MB \
        SPAWN_GATE="$WORK/fake-gate.sh" "$@" \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || fail "integrate exited $status: $(err | tail -n 3)"
}

# The phase: passes with the environment `$@` until a gate record appears;
# sets `event`, `task`, `verdict`, and `runs_before`.
phase() {
    what=$1
    shift
    runs_before=$(runs)
    from=$(last_event)
    deadline=$(($(now) + 90))
    while :; do
        integrate_once "$@"
        found=$(next_gated "$from")
        [ -z "$found" ] || break
        [ "$(now)" -lt "$deadline" ] ||
            harness "($what) no gate record within 90 s of passes (tasks: $(tasks_line); integrate: $(log_tail))"
        sleep 0.5
    done
    set -- $found
    event=$1
    task=$2
    verdict=$3
}

# The common checks of a phase whose check fell short.
held_back() { # <label> <check> <phase description>
    label=$1
    if [ "$verdict" != no_room ]; then
        fail "($label) $3, yet the gate ran on task #$task's candidate and it was gated '$verdict' ($(field "$event" reason)); gate runs $runs_before -> $(runs)"
    fi
    assert_eq "$(runs)" "$runs_before" "($label) gate runs while $3"
    assert_eq "$(field "$event" check)" "$2" "($label) the no_room record's check"
    assert_eq "$(task_state "$task")" reported "($label) task #$task's state after no_room"
    m=$(mission_of "$task")
    assert_eq "$(mission_field "$m" gate_rejections)" 0 "($label) mission $m's gate_rejections"
    assert_eq "$(mission_field "$m" gate_aborts)" 0 "($label) mission $m's gate_aborts"
    assert_eq "$(mission_field "$m" "coalesce(gate_room_task, 0)")" "$task" \
        "($label) mission $m's no-room run (gate_room_task): judged again later"
    assert_eq "$(mission_field "$m" gate_room_waits)" 1 "($label) mission $m's gate_room_waits"
}

# --- four missions, four residents, four reported tasks ------------------------

names="disk memory dbase room"
for name in $names; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 018 $name change."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
done
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

all_tasks=""
for name in $names; do
    capture "$SPAWN_BIN" mission add --text "018 $name" --source "$source"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    mission=$(jq -r .mission "$WORK/out.txt")
    [ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
    capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
    [ "$status" -eq 0 ] || harness "mission configure exited $status: $(err)"
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$name" \
        --brief "Write your file. BB:WRITE"
    [ "$status" -eq 0 ] || harness "task assign for $name failed: $(err)"
    id=$(jq -r .task "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
    all_tasks="$all_tasks $id"
done
all_reported() {
    for id in $all_tasks; do
        [ "$(task_state "$id")" = reported ] || return 1
    done
}
deadline=$(($(now) + 240))
until all_reported; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "the four tasks were not all reported within 240 s (tasks: $(tasks_line))"
    sleep 1
done

# The phases' environments. The real test database passes the database check
# (it runs parallel queries); 0 GiB and 1 MiB pass the others.
ok_disk=SPAWN_GATE_MIN_FREE_GB=0
ok_memory=SPAWN_GATE_MIN_FREE_MEMORY_MB=1
ok_db="SPAWN_TEST_DATABASE_URL=$TEST_URL"
tmpdir=${TMPDIR:-/tmp}

# --- (1) disk --------------------------------------------------------------------

phase 1 SPAWN_GATE_MIN_FREE_GB=1000000 "$ok_memory" "$ok_db"
held_back 1 disk "the gate's TMPDIR had less than SPAWN_GATE_MIN_FREE_GB=1000000 free"
measured=$(field "$event" measured)
threshold=$(field "$event" threshold)
df_kb=$(df -Pk "$tmpdir" | awk 'NR == 2 { print $4 }')
case "$measured" in '' | *[!0-9]*) fail "(1) the disk record's measured is not a whole number of KiB: [$measured]" ;; esac
assert_eq "$threshold" "$((1000000 * 1048576))" "(1) the disk record's threshold (KiB)"
assert_eq "$(field "$event" free_kb)" "$measured" "(1) the disk record's free_kb against its measured"
assert_eq "$(field "$event" min_gb)" 1000000 "(1) the disk record's min_gb"
diff_kb=$((measured > df_kb ? measured - df_kb : df_kb - measured))
[ "$diff_kb" -le 1048576 ] || [ "$diff_kb" -le $((df_kb / 20)) ] ||
    fail "(1) the disk record measured $measured KiB free; df reads $df_kb KiB in $tmpdir"
assert_contains "$(field "$event" reason)" "SPAWN_GATE_MIN_FREE_GB=1000000" "(1) the disk record's reason"
assert_contains "$(field "$event" reason)" "not a verdict" "(1) the disk record's reason"

# --- (2) memory ------------------------------------------------------------------

phase 2 "$ok_disk" SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000 "$ok_db"
held_back 2 memory "the host had less than SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000 available"
measured=$(field "$event" measured)
case "$measured" in '' | *[!0-9]*) fail "(2) the memory record's measured is not a whole number of MiB: [$measured]" ;; esac
assert_eq "$(field "$event" threshold)" 100000000 "(2) the memory record's threshold (MiB)"
if [ -r /proc/meminfo ]; then
    available=$(awk '/^MemAvailable:/ { print int($2 / 1024) }' /proc/meminfo)
    if [ -n "$available" ] && [ "$available" -gt 0 ]; then
        [ "$measured" -le $((available * 2)) ] && [ "$measured" -ge $((available / 2)) ] ||
            fail "(2) the memory record measured $measured MiB available; /proc/meminfo reads $available MiB"
    fi
fi
assert_contains "$(field "$event" reason)" "$measured" "(2) the memory record's reason (the measured MiB)"
assert_contains "$(field "$event" reason)" "SPAWN_GATE_MIN_FREE_MEMORY_MB=100000000" "(2) the memory record's reason"

# --- (3) database ----------------------------------------------------------------

mkdir -p "$WORK/nodb"
nodb=$(printf '%s' "$WORK/nodb" | sed 's#/#%2F#g')
phase 3 "$ok_disk" "$ok_memory" "SPAWN_TEST_DATABASE_URL=postgres://bb:bb018secret@$nodb/bb_none"
held_back 3 database "the test database could not be reached"
[ -n "$(field "$event" measured)" ] || fail "(3) the database record's measured is empty"
[ -n "$(field "$event" threshold)" ] || fail "(3) the database record's threshold is empty"
assert_contains "$(field "$event" reason)" "test database" "(3) the database record's reason"
assert_not_contains "$(record_text "$event")" "bb018secret" "(3) the database record (the URL's password)"

# --- (4) room: the gate runs ---------------------------------------------------------

phase 4 "$ok_disk" "$ok_memory" "$ok_db"
[ "$verdict" = accepted ] ||
    fail "(4) with every threshold met, task #$task was gated '$verdict' ($(field "$event" reason)), not accepted"
assert_eq "$(runs)" "$((runs_before + 1))" "(4) gate runs with every threshold met"
deadline=$(($(now) + 60))
until [ "$(task_state "$task")" = landed ]; do
    [ "$(now)" -lt "$deadline" ] || fail "(4) task #$task was accepted but did not land within 60 s"
    integrate_once "$ok_disk" "$ok_memory" "$ok_db"
    sleep 0.5
done

echo "018: the gate was not run while disk, memory or the test database fell short; each was recorded no_room with its check, measure and threshold, and the gate ran once the host had room"
exit 0
