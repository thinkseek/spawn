#!/bin/sh
# Black box: work stealing (objective 239, owner t517u). An idle resident of
# a pool takes one unstarted task from a busy peer's queue by a recorded
# move (`task.stolen`) the store checks under the append lock; a started task
# never moves; two takers of one task cannot both win; the move survives a
# restart and a refold (docs/fleet-architecture.md §3.2 "Work stealing").
#
# A real loop (the Bridge, its pusher, the dispatcher) runs residents of two
# missions on a fake engine. Pool `logs`: ben (the busy worker), ann and dee
# (idle), cid (busy), eve and fay (idle until the loop stops), gus (holding
# only a task of the paused mission); pool `other`: sol; no pool: una. ben
# runs task T1 (it never ends its turn) and holds, queued: T2, T3 (oldest
# first), T4 (pinned), T7; T5 in mission P, which the owner pauses; T6,
# cancelled. The first loop runs with SPAWN_WORK_STEALING=0 (the owner's
# moves only); the second with stealing on. Checked:
#   (0) `spawn resident propose --pool` and `spawn task assign --pin` are
#       taken;
#   (1) `spawn task steal` refuses, exit 1 with "spawn: task steal: task <id> "
#       and the rule's reason, nothing recorded and the owner unchanged: T1
#       ("not queued (delivered"); T4 ("pinned"); T5 ("(paused)"); T6 ("not
#       queued (cancelled"); Tc, of mission C closed after it was queued (the
#       closure rule cancels it first: "not queued (cancelled"; a closed
#       mission's task is never refused by the mission rule in a black box,
#       and a halted mission needs the integrator's record: both are the
#       store tests'); T2 to sol ("is not other"), to una ("(no pool)"), to
#       hal, proposed and never approved ("not approved"), to ivy, approved
#       and retired ("retired"), to ben ("already its worker"), to nobody ("no
#       such resident"), to cid ("not idle": T8 delivered), to gus ("not
#       idle": a task assigned only); after (2), T3 to the winner ("not idle":
#       T2 delivered or reported);
#   (2) T2 stolen to ann and to dee at once: exactly one exits 0, one
#       task.stolen record for T2 (by human, from ben's session to the
#       winner's), T2's session the winner's; the other exits 1 with "spawn:
#       task steal: task <T2> ";
#   (3) T2 is delivered once, into the winner's session: every
#       `deliver-<T2>-` command is the winner's, none ben's, one
#       task.delivered record;
#   (4) T1 stays ben's: delivered, ben's session, no task.stolen record;
#   (5) after the loop stops, a refold of the log and init-db again change no
#       task's session and add no task.stolen record; then, the loop still
#       stopped, eve (idle) is assigned TA and TB: TA, eve's next, is refused
#       to fay ("its worker's next"); TB is taken by fay (exit 0);
#   (6) a loop started again with stealing on: the idle loser takes T3, ben's
#       oldest stealable task, by itself (task.stolen by spawn-dispatch), and
#       only T3: 15 s later T7 is still ben's, TA still eve's, and no other
#       task moved (three task.stolen records in all: T2, TB, T3);
#   (7) T3 is delivered once, into the loser's session, none into ben's;
#   (8) T4 (pinned) and T5 (paused mission) are still ben's, and gus's task
#       still gus's, with no task.stolen record; sol, una, hal (unapproved) and
#       ivy (retired) hold no task;
#   (9) `spawn task stages --id T3` shows a stage `stolen`.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (0) on the live main (9e1a8d22): the roster has no pool.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
# reads: binary
set -eu

asserted=""
harness() {
    echo "239: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
SPAWN_HC=${SPAWN_HERDR_CORE_PROGRAM-}
if [ -z "$SPAWN_HC" ]; then
    spawn_dir=$(dirname "$SPAWN_BIN")
    if [ -x "$spawn_dir/spawn-hc" ]; then
        SPAWN_HC=$spawn_dir/spawn-hc
    elif [ -x "$(dirname "$spawn_dir")/spawn-hc" ]; then
        SPAWN_HC=$(dirname "$spawn_dir")/spawn-hc
    elif command -v spawn-hc >/dev/null 2>&1; then
        SPAWN_HC=$(command -v spawn-hc)
    fi
fi
[ -n "$SPAWN_HC" ] && [ -x "$SPAWN_HC" ] ||
    harness "spawn-hc is not installed (set SPAWN_HERDR_CORE_PROGRAM or build sibling spawn-hc beside SPAWN_BIN)"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep date; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and the tmux server's socket, in WORK (cc-rules (a)).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/r
XDG_STATE_HOME=$WORK/state
XDG_CONFIG_HOME=$WORK/config
mkdir -m 700 "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
export XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
hc_id=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
SPAWN_DB_NAME="bb_239_steal_$run"
SPAWN_HERDR_SESSION="bb-239-$hc_id"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_HERDR_SESSION SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
stop_loop() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
        loop_pid=""
    fi
}
hc() {
    env -u HERDR_SOCKET_PATH -u HERDR_SESSION -u HERDR_PANE_ID \
        HC_SESSION="$SPAWN_HERDR_SESSION" \
        HC_SOCKET="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock" \
        "$SPAWN_HC" "$@"
}
stop_herdr() {
    [ -n "${SPAWN_HERDR_SESSION-}" ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ] || return 0
    for addr in $(hc ls --json --label "spawn_owner=$SPAWN_HERDR_SESSION" 2>/dev/null |
        jq -r '.terminals[]? | select(.state == "live") | .address // empty' 2>/dev/null || true); do
        hc kill "$addr" >/dev/null 2>&1 || true
    done
    hc server stop >/dev/null 2>&1 || true
    hc_wait=0
    while [ "$hc_wait" -lt 20 ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ]; do
        hc_wait=$((hc_wait + 1))
        sleep 0.1
    done
}
cleanup() {
    stop_loop
    stop_herdr
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/loop.log" ]; then
        echo "239: the loop's last lines:" >&2
        tail -n 12 "$WORK/loop.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "239: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "base"
for bare in "$repo" "$WORK/source-m.git" "$WORK/source-p.git" "$WORK/source-c.git"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# The fake engine: `BB:HOLD` is a turn that never ends (the worker stays
# busy on it); `BB:WRITE:<name>` writes <name>.txt and the report; anything
# else is a turn that changes nothing.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
[ -n "${BB_PATH-}" ] && PATH=$BB_PATH && export PATH
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
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
hold=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in
    *"BB:HOLD"*) hold=1 ;;
    *"BB:WRITE:"*)
      job=${typed#*BB:WRITE:}
      job=${job%%[!a-z]*}
      ;;
    esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$hold" ]; then
      printf '✽ Working…\n'
      sleep 1
      continue
    fi
    if [ -n "$job" ]; then
      printf 'wrote %s\n' "$job" >"$job.txt"
      mkdir -p .spawn
      sleep 1
      printf '## Outcome\ndone\n## What changed\n%s.txt\n## Open issues\nnone\n' "$job" >.spawn/report.md
      job=
    fi
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"


for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_WORK_STEALING SPAWN_SUPERVISOR SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS \
    SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE SPAWN_HERDR_SHARDS SPAWN_HERDR_PROGRAM \
    SPAWN_TMUX_SOCKET SPAWN_TMUX_PROGRAM TMUX TMUX_PANE TMUX_TMPDIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_PATH=$PATH
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# No supervisor (it runs when SPAWN_SUPERVISOR is set at all): delivery and
# stealing are the dispatcher's, and nothing here asks a model.
# The stall bounds out of the way: nothing here is about stalls.
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_PATH SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"

init_db() {
    tries=0
    while :; do
        capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
        [ "$status" -eq 0 ] && return 0
        tries=$((tries + 1))
        [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
        sleep 1
    done
}
init_db

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
spawn_ok() {
    capture "$SPAWN_BIN" "$@"
    [ "$status" -eq 0 ] || harness "spawn $*: exit $status: $(err)"
}
state_of() { q "select state from mission_tasks where id = $1"; }
session_of() { q "select session_id from mission_tasks where id = $1"; }
# stolen <task> [<field>]: the count of its task.stolen records, or a field
# of its only one.
stolen_count() {
    q "select count(*) from spawn_events where session_id = 'session_control'
         and kind -> 'detail' -> 'record' ->> 'type' = 'task.stolen'
         and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
stolen_field() {
    q "select coalesce(kind -> 'detail' -> 'record' ->> '$2', '') from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.stolen'
          and kind -> 'detail' -> 'record' ->> 'id' = '$1' order by id limit 1"
}
all_stolen() {
    q "select count(*) from spawn_events where session_id = 'session_control'
         and kind -> 'detail' -> 'record' ->> 'type' = 'task.stolen'"
}
delivered_records() {
    q "select count(*) from spawn_events where session_id = 'session_control'
         and kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
         and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
# deliveries <task> <session>: delivery commands of the task into the session.
deliveries() {
    q "select count(*) from spawn_commands
        where idempotency_key like 'deliver-$1-%' and session_id = '$2'"
}
all_deliveries() { q "select count(*) from spawn_commands where idempotency_key like 'deliver-$1-%'"; }
wait_until() { # <seconds> <command...>
    bound=$(($(now) + $1))
    shift
    until "$@"; do
        [ "$(now)" -lt "$bound" ] || return 1
        sleep 0.5
    done
}
is_state() { [ "$(state_of "$1")" = "$2" ]; }
start_loop() { # [env assignments...]
    env "$@" "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}
assign() { # <mission> <worker> <brief> [--pin]
    m=$1 w=$2 b=$3
    shift 3
    capture "$SPAWN_BIN" task assign --mission "$m" --kind resident --worker "$w" --brief "$b" "$@"
    [ "$status" -eq 0 ] || return 1
    task=$(jq -r .task "$WORK/out.txt")
    [ -n "$task" ] && [ "$task" != null ] || harness "task assign printed no id: $(out)"
}
# refused <task> <to> <word> <what>: `spawn task steal` exits 1 naming the
# rule, records nothing, and leaves the task where it was.
refused() {
    before=$(session_of "$1")
    records=$(all_stolen)
    capture "$SPAWN_BIN" task steal --id "$1" --to "$2"
    [ "$status" -ne 0 ] || fail "(1) $4: spawn task steal --id $1 --to $2 exited 0: $(out)"
    grep -q -- "spawn: task steal: task $1 " "$WORK/err.txt" ||
        fail "(1) $4: the refusal does not start [spawn: task steal: task $1 ]: $(err)"
    grep -q -- "$3" "$WORK/err.txt" ||
        fail "(1) $4: the refusal does not name [$3]: $(err)"
    expect_eq "$(all_stolen)" "$records" "(1) $4: task.stolen records"
    expect_eq "$(session_of "$1")" "$before" "(1) $4: task $1's session"
}

ben=session_resident_ben

# --- (0) a pool and a pin -----------------------------------------------------

spawn_ok mission add --text "239 work" --source "$WORK/source-m.git"
mission=$(jq -r .mission "$WORK/out.txt")
spawn_ok mission add --text "239 paused work" --source "$WORK/source-p.git"
paused=$(jq -r .mission "$WORK/out.txt")
spawn_ok mission add --text "239 closed work" --source "$WORK/source-c.git"
closed=$(jq -r .mission "$WORK/out.txt")

capture "$SPAWN_BIN" resident propose --name ben --charter "Analyses the 239 logs." --pool logs
[ "$status" -eq 0 ] || fail "(0) spawn resident propose --pool exited $status: $(err)"
for name in ann cid dee eve fay gus; do
    spawn_ok resident propose --name "$name" --charter "Analyses the 239 logs." --pool logs
done
spawn_ok resident propose --name sol --charter "Analyses other 239 logs." --pool other
spawn_ok resident propose --name hal --charter "Analyses the 239 logs." --pool logs
spawn_ok resident propose --name ivy --charter "Analyses the 239 logs." --pool logs
spawn_ok resident propose --name una --charter "Owns its own 239 concern."
for name in ben ann cid dee eve fay gus sol una ivy; do
    spawn_ok resident approve --name "$name"
done
spawn_ok resident retire --name ivy
expect_eq "$(q "select coalesce(pool, '-') from mission_residents where name = 'ben'")" logs \
    "(0) ben's pool"
expect_eq "$(q "select coalesce(pool, '-') from mission_residents where name = 'una'")" - \
    "(0) una's pool"

# --- the first loop: stealing off, the owner's moves only --------------------

start_loop SPAWN_WORK_STEALING=0
assign "$mission" ben "Hold the line. BB:HOLD" || harness "assign T1 failed: $(err)"
t1=$task
assign "$mission" cid "Hold the line. BB:HOLD" || harness "assign T8 failed: $(err)"
t8=$task
wait_until 120 is_state "$t1" delivered || harness "T1 was not delivered: $(state_of "$t1")"
wait_until 120 is_state "$t8" delivered || harness "T8 was not delivered: $(state_of "$t8")"

assign "$mission" ben "Analyse log two. BB:WRITE:two" || harness "assign T2: $(err)"
t2=$task
assign "$mission" ben "Analyse log three. BB:WRITE:three" || harness "assign T3: $(err)"
t3=$task
assign "$mission" ben "Follow up your own candidate. BB:WRITE:four" --pin ||
    fail "(0) spawn task assign --pin exited $status: $(err)"
t4=$task
expect_eq "$(q "select pinned from mission_tasks where id = $t4")" t "(0) T4 pinned"
assign "$mission" ben "Analyse log seven. BB:WRITE:seven" || harness "assign T7: $(err)"
t7=$task
assign "$paused" ben "Analyse log five. BB:WRITE:five" || harness "assign T5: $(err)"
t5=$task
spawn_ok mission pause --id "$paused" --reason "239: the owner holds this mission"
assign "$paused" gus "Analyse log nine. BB:WRITE:nine" || harness "assign Tg: $(err)"
tg=$task
assign "$mission" ben "Analyse log six. BB:WRITE:six" || harness "assign T6: $(err)"
t6=$task
spawn_ok task cancel --id "$t6"
assign "$closed" ben "Analyse log c. BB:WRITE:cc" || harness "assign Tc: $(err)"
tc=$task
spawn_ok mission close --id "$closed" --state dropped
sleep 2
expect_eq "$(session_of "$t2")" "$ben" "the queued T2 waits for ben (stealing is off)"

# --- (1) refusals ----------------------------------------------------------------

refused "$t1" ann "not queued (delivered" "a running task"
refused "$t4" ann "pinned" "a pinned task"
refused "$t5" ann "(paused)" "a task of a paused mission"
refused "$t6" ann "not queued (cancelled" "a cancelled task"
refused "$tc" ann "not queued (cancelled" "a task of a closed mission (the closure rule cancelled it)"
refused "$t2" sol "is not other" "a taker in another pool"
refused "$t2" una "(no pool)" "a taker with no pool"
refused "$t2" hal "not approved" "a taker never approved"
refused "$t2" ivy "retired" "a retired taker"
refused "$t2" ben "already its worker" "the task's own worker"
refused "$t2" nobody "no such resident" "no such resident"
refused "$t2" cid "not idle" "a taker that is not idle (a task delivered)"
refused "$t2" gus "not idle" "a taker that is not idle (a task assigned only)"

# --- (2) two takers of one task ----------------------------------------------------

(set +e; "$SPAWN_BIN" task steal --id "$t2" --to ann >"$WORK/ann.out" 2>"$WORK/ann.err"; echo $? >"$WORK/ann.rc") &
a_pid=$!
(set +e; "$SPAWN_BIN" task steal --id "$t2" --to dee >"$WORK/dee.out" 2>"$WORK/dee.err"; echo $? >"$WORK/dee.rc") &
d_pid=$!
wait "$a_pid" || true
wait "$d_pid" || true
ann_rc=$(cat "$WORK/ann.rc")
dee_rc=$(cat "$WORK/dee.rc")
case "$ann_rc:$dee_rc" in
0:0) fail "(2) both takers won T2: ann and dee exited 0" ;;
0:*) winner=ann loser=dee ;;
*:0) winner=dee loser=ann ;;
*) fail "(2) neither taker won T2: ann $ann_rc ($(head -c 200 "$WORK/ann.err")), dee $dee_rc ($(head -c 200 "$WORK/dee.err"))" ;;
esac
grep -q -- "spawn: task steal: task $t2 " "$WORK/$loser.err" ||
    fail "(2) the loser's refusal does not start [spawn: task steal: task $t2 ]: $(head -c 300 "$WORK/$loser.err")"
expect_eq "$(stolen_count "$t2")" 1 "(2) task.stolen records of T2"
expect_eq "$(session_of "$t2")" "session_resident_$winner" "(2) T2's session"
expect_eq "$(stolen_field "$t2" from_session)" "$ben" "(2) T2's record from_session"
expect_eq "$(stolen_field "$t2" to_session)" "session_resident_$winner" "(2) T2's record to_session"
expect_eq "$(stolen_field "$t2" by)" human "(2) T2's record by"
refused "$t3" "$winner" "not idle" "the winner as a taker (T2 its own)"

# --- (3) delivered once, to the winner ------------------------------------------

wait_until 120 is_state "$t2" delivered ||
    [ "$(state_of "$t2")" = reported ] || fail "(3) T2 was not delivered: $(state_of "$t2")"
expect_eq "$(deliveries "$t2" "$ben")" 0 "(3) delivery commands of T2 into ben's session"
expect_eq "$(deliveries "$t2" "session_resident_$winner")" "$(all_deliveries "$t2")" \
    "(3) every delivery command of T2 is the winner's"
expect_eq "$(delivered_records "$t2")" 1 "(3) task.delivered records of T2"

# --- (4) the running task stays its worker's -------------------------------------

expect_eq "$(state_of "$t1")" delivered "(4) T1's state"
expect_eq "$(session_of "$t1")" "$ben" "(4) T1's session"
expect_eq "$(stolen_count "$t1")" 0 "(4) task.stolen records of T1"

# --- (5) a restart and a refold change no owner -----------------------------------

stop_loop
owners() { q "select string_agg(id || ':' || session_id, ' ' order by id) from mission_tasks"; }
before_owners=$(owners)
before_records=$(all_stolen)
sql "select spawn_control_refold()" >/dev/null 2>"$WORK/err.txt" ||
    harness "spawn_control_refold() failed: $(err)"
expect_eq "$(owners)" "$before_owners" "(5) the tasks' sessions after a refold"
init_db
expect_eq "$(owners)" "$before_owners" "(5) the tasks' sessions after init-db"
expect_eq "$(all_stolen)" "$before_records" "(5) task.stolen records after the refold"
# An idle worker's next task is never taken; its second one is.
assign "$mission" eve "Analyse log a. BB:WRITE:aa" || harness "assign TA: $(err)"
ta=$task
assign "$mission" eve "Analyse log b. BB:WRITE:bb" || harness "assign TB: $(err)"
tb=$task
refused "$ta" fay "next" "(5) an idle worker's next task"
capture "$SPAWN_BIN" task steal --id "$tb" --to fay
[ "$status" -eq 0 ] || fail "(5) eve's second task TB to the idle fay exited $status: $(err)"
expect_eq "$(session_of "$tb")" session_resident_fay "(5) TB's session"

# --- (6) the second loop: stealing on ------------------------------------------------

start_loop
took_t3() { [ "$(stolen_count "$t3")" -ge 1 ]; }
wait_until 120 took_t3 || fail "(6) the idle $loser did not take T3 (its session: $(session_of "$t3"))"
expect_eq "$(stolen_count "$t3")" 1 "(6) task.stolen records of T3"
expect_eq "$(stolen_field "$t3" by)" spawn-dispatch "(6) T3's record by"
expect_eq "$(session_of "$t3")" "session_resident_$loser" "(6) T3's session"
sleep 15
expect_eq "$(session_of "$t7")" "$ben" "(6) T7 (ben's next) is still ben's"
expect_eq "$(session_of "$ta")" session_resident_eve "(6) TA is still eve's"
expect_eq "$(all_stolen)" 3 "(6) task.stolen records in all (T2 and TB by the owner, T3 by the dispatcher)"
expect_eq "$(stolen_count "$t2")" 1 "(6) T2 is not moved again after the restart"

# --- (7) delivered once, to the taker --------------------------------------------------

delivered_or_after() { case "$(state_of "$1")" in delivered | reported) return 0 ;; *) return 1 ;; esac; }
wait_until 120 delivered_or_after "$t3" || fail "(7) T3 was not delivered: $(state_of "$t3")"
expect_eq "$(deliveries "$t3" "$ben")" 0 "(7) delivery commands of T3 into ben's session"
expect_eq "$(deliveries "$t3" "session_resident_$loser")" "$(all_deliveries "$t3")" \
    "(7) every delivery command of T3 is the taker's"
expect_eq "$(delivered_records "$t3")" 1 "(7) task.delivered records of T3"

# --- (8) what never moves ---------------------------------------------------------------

for t in "$t4" "$t5"; do
    expect_eq "$(session_of "$t")" "$ben" "(8) task $t's session"
    expect_eq "$(stolen_count "$t")" 0 "(8) task.stolen records of task $t"
done
expect_eq "$(session_of "$tg")" session_resident_gus "(8) gus's task's session"
expect_eq "$(stolen_count "$tg")" 0 "(8) task.stolen records of gus's task"
expect_eq "$(q "select count(*) from mission_tasks
               where session_id in ('session_resident_sol', 'session_resident_una',
                                    'session_resident_hal', 'session_resident_ivy')")" 0 \
    "(8) tasks of sol, una, hal and ivy"
expect_eq "$(state_of "$t1")" delivered "(8) T1's state after the second loop"
expect_eq "$(session_of "$t1")" "$ben" "(8) T1's session after the second loop"

# --- (9) the stage ---------------------------------------------------------------------

capture "$SPAWN_BIN" task stages --id "$t3"
[ "$status" -eq 0 ] || fail "(9) spawn task stages --id $t3 exited $status: $(err)"
[ "$(jq -r '[.[] | select(.stage == "stolen")] | length' "$WORK/out.txt" 2>/dev/null)" = 1 ] ||
    fail "(9) the stages of T3 show no single stage stolen: $(out)"

echo "239: an idle pool member took queued work, once and recorded; running, pinned and paused work stayed; two takers had one winner; a restart and a refold kept the owner"
exit 0
