#!/bin/sh
# Black box: the loop's database work follows change, not the clock. The
# dispatcher's step (dispatch::step), the residents' and the supervisor's
# liveness (supervise::keep_residents, keep_alive) and the mission rows
# (deliver.rs Work::load) run only when the event watermark (max
# spawn_events.id) has moved since they last ran, or when one of their timers
# is due (a delivery retry, a stall bound, a lease expiry); between those the
# loop waits on LISTEN spawn_events (spawn_append_trusted already notifies
# every append) with a timeout equal to the next due time. An idle fleet costs
# at most one cheap watermark read per pass. And the per-session reads of
# spawn_commands that the dispatch facts make have the indexes they need:
# (session_id, id) and (session_id, principal, id).
#
# The measure is PostgreSQL's own: pg_stat_statements is not installed, so
# the scenario sums seq_scan + idx_scan of every user table of its own
# database (pg_stat_user_tables) over a window; transactions committed
# (pg_stat_database.xact_commit) are printed beside it.
#
# A real `spawn loop` (no model judge) runs a supervisor and three residents,
# `alpha`, `frail` and `idle`, on a fake engine, at the default pass interval
# (SPAWN_LOOP_INTERVAL_MS unset: 500 ms, 120 passes a minute). One mission on
# a source, no gate. SPAWN_STALL_ACTIVITY_SECS=20; the other stall bounds are
# out of the way. Checked:
#   (1) the indexes: spawn_commands has a plain (not partial) btree index
#       whose key is exactly (session_id, id) and one whose key is exactly
#       (session_id, principal, id), made by `spawn init-db`.
#   (2) an idle minute: every session waits at its prompt, no task is open,
#       no command is pending, nothing is due. Over 60 s the scans of every
#       user table add up to at most IDLE_MAX (150: 1.25 per pass, one
#       watermark read per pass and the disk watch's minute). Two minutes
#       are measured back to back and the lower one is judged: the loop's
#       own cost does not drop in the second minute (a step that reads on
#       every pass reads in both), while a host's does. A loaded host
#       (another gate's scenarios beside it, an alert such as tmpfs_high
#       firing) once put one minute at 152 with the loop's statements
#       unchanged (2026-11-09, batch 8, under log_statement=all: 137 and 138
#       statements, the same ones, on the candidate and the base). The bound
#       is not raised: a regression adds to both minutes. One extra
#       watermark read per pass still fails it, as below.
#       The higher minute is judged too, coarsely: at most 2 x IDLE_MAX
#       (300). The lower one cannot see what fires at most once in the two
#       minutes (a reread on a schedule slower than a minute, a cost that
#       settles in the first minute, a regression on a two-minute period).
#       300 is about twice the worst minute host load gave (152), and well
#       under one minute of task 202's regression (142 -> 675, +530). So
#       the lower minute judges what the loop does on every pass, at 150,
#       and the higher one judges what it does once, at 300.
#       The transactions printed beside the scans are not judged:
#       pg_stat_database.xact_commit also counts the server's own
#       (autovacuum and analyze after the busy setup), 159 to 300 a minute
#       for the same build.
#   (3) a busy minute, the same decisions at the same times. At its start
#       T1 is assigned to alpha (whose engine then stays busy and quiet) and
#       T2 to frail, whose text the tmux wrapper refuses to stage while
#       $WORK/broken exists (`command too long`: refused before typing, so
#       retried after NOT_READY_RETRY_MS, 5 s, by deliver.rs); the host is
#       fixed 32 s in. The supervisor is pinged now and then, as 013's and
#       019's scenarios do, so its judgements cover its facts.
#       - T1 is delivered within DELIVER_MAX_MS (4000) of its assignment;
#       - every retry of T2's delivery is made no sooner than 5 s and no later
#         than 5 s + RETRY_SLACK_MS (1500) after the attempt before it
#         finished, and the first after the fix succeeds, by the same rule;
#       - T1's activity stall reaches the supervisor once (a digest tagged
#         `session_resident_alpha stall-T1-activity-<n>`), no sooner than 20 s
#         and no later than 20 s + STALL_SLACK_MS (2500) after alpha's last
#         fact (deliver.rs and dispatch/stall.rs measure it so);
#       - the minute's scans add up to at most IDLE_MAX + SLICE_MAX (150) per
#         half-second slice of it that holds an event (recorded_at_ms): a full
#         pass's work (118 scans today) at most once per slice in which
#         something happened, and one watermark read per pass.
#
# Measured on the live main (49b92be, the deployed binary built from it),
# this fleet, this host, 2026-11-06 (BB_023_MEASURE=1 prints them):
#   idle minute: 14,221-14,256 scans (about 118 a pass: spawn_events 8,740,
#   spawn_commands 4,164, mission_tasks 520, mission_residents 312,
#   mission_missions 312, spawn_session_state 208), 3,400 transactions, no
#   event appended;
#   busy minute: 14,753-14,761 scans, events in 68 of its 120 slices (so its
#   bound is 150 + 150 x 68 = 10,350); T1 delivered 1.8-1.9 s after its
#   assignment; T2's retries 5.34-5.51 s after the attempt before; T1's stall
#   digests (two: the busy screen's 30 s refresh starts a second stall)
#   0.1-0.5 s after their due times.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main: spawn_commands has only its primary
# key, its (principal, idempotency_key) key and the partial index
# spawn_commands_open (session_id, id) where state <> 'finished'. With
# BB_023_MEASURE=1 (only to show today's behaviour) it goes on and shows (2)
# failing too: an idle minute costs about 12,300 scans, the full dispatcher,
# liveness and mission reads on every pass whether or not anything changed.
# It passes once init-db makes the two indexes and the loop runs those steps
# only when the watermark moved or a timer is due, waking on LISTEN
# spawn_events, while every delivery, retry and stall still happens when due.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "023: harness: $*" >&2
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

MEASURE=${BB_023_MEASURE:-}
IDLE_MAX=150
SLICE_MAX=150
DELIVER_MAX_MS=4000
RETRY_SLACK_MS=1500
STALL_SECS=20
STALL_SLACK_MS=2500

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_023_wake_$run"
SPAWN_TMUX_SOCKET="bb-023-$run"
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
            echo "023: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "023: the tasks (id state agent):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, state, agent_id from mission_tasks order by id" >&2 2>/dev/null || true
            echo "023: the delivery attempts (key, ok, created, finished, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select idempotency_key, coalesce(succeeded::text, 'open'),
                       created_at_ms, finished_at_ms, left(coalesce(error, ''), 70)
                  from spawn_commands where idempotency_key like 'deliver-%' order by id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "023: harness: unexpected exit $rc" >&2
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
# A check that BB_023_MEASURE=1 only reports.
measured_fail() {
    if [ -n "$MEASURE" ]; then
        echo "023: (measure) would fail: $*"
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

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet, the transcript's end of the turn first, then the prompt.
# A turn whose text names `BB:HANG` never ends: the screen shows a running
# turn and the engine writes nothing more (a worker busy and quiet).
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
hang=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:HANG"*) hang=1 ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$hang" ]; then
      printf '\n✻ Working… (esc to interrupt)\n\n❯ '
      while :; do sleep 3600; done
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# tmux on the private socket. While $WORK/broken exists, a text naming
# `BB:FRAIL` cannot be staged: `load-buffer` (the Bridge stages every text
# from a file) fails as tmux fails a command over its message size. Nothing
# reaches the pane.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
stage=
for argument in "\$@"; do
  [ "\$argument" = load-buffer ] && stage=1
  last=\$argument
done
if [ -n "\$stage" ] && [ -f "\$last" ] && [ -e '$WORK/broken' ] &&
    grep -q "BB:FRAIL" "\$last" 2>/dev/null; then
  echo "command too long" >&2
  exit 1
fi
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS \
    SPAWN_DELIVERY_RETRY_SECS SPAWN_REISSUE_AFTER_SECS; do
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
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# The stall under test, short; the others out of the way.
SPAWN_STALL_ACTIVITY_SECS=$STALL_SECS
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_SUPERVISOR \
    SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"

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
task_state() { q "select state from mission_tasks where id = $1"; }

# The key columns of every plain index of spawn_commands, `a,b c,d`.
index_keys() {
    q "select coalesce(string_agg(keys, ' ' order by keys), '') from (
         select string_agg(a.attname, ',' order by k.ord) as keys
           from pg_index i
           join pg_class t on t.oid = i.indrelid
           cross join lateral unnest(i.indkey::int2[]) with ordinality as k(attnum, ord)
           join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
          where t.oid = 'spawn_commands'::regclass and i.indpred is null
            and k.ord <= i.indnkeyatts
          group by i.indexrelid) x"
}

# Every user table's scans: `<table> <seq_scan> <idx_scan>` lines.
snapshot() {
    sql "select relname, seq_scan, coalesce(idx_scan, 0) from pg_stat_user_tables" >"$1"
    sql "select xact_commit from pg_stat_database where datname = current_database()" >"$1.xact"
}
# delta <before> <after>: `<total> <seq> <idx> <transactions> <top tables>`.
delta() {
    awk -F'|' -v xa="$(cat "$1.xact")" -v xb="$(cat "$2.xact")" '
        NR == FNR { seq[$1] = $2; idx[$1] = $3; next }
        { s = $2 - seq[$1]; i = $3 - idx[$1]; ts += s; ti += i
          if (s + i > 0) top[$1] = s + i }
        END {
            n = 0; line = ""
            while (n < 6) {
                best = ""; for (t in top) if (best == "" || top[t] > top[best]) best = t
                if (best == "") break
                line = line " " best "=" top[best]; delete top[best]; n++
            }
            printf "%d %d %d %d%s\n", ts + ti, ts, ti, xb - xa, line
        }' "$1" "$2"
}

# The session is judged waiting at its prompt, live.
waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}
all_waiting() {
    for session in session_supervisor session_resident_alpha session_resident_frail \
        session_resident_idle; do
        waiting "$session" || return 1
    done
}
pings=0
ping_supervisor() {
    pings=$((pings + 1))
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor \
        --session session_supervisor --key "bb-ping-$pings" \
        --action '{"type":"input","text":"ping"}'
    [ "$status" -eq 0 ] || harness "submit of a ping to the supervisor failed: $(err)"
}
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
record_at() { # <type> <task>: recorded_at_ms of the task's first such record, or empty
    q "select min(recorded_at_ms) from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = '$1'
          and kind -> 'detail' -> 'record' ->> 'id' = '$2'"
}

# --- (1) the indexes ---------------------------------------------------------------

keys=$(index_keys)
for want in session_id,id session_id,principal,id; do
    case " $keys " in
    *" $want "*) ;;
    *) measured_fail "(1) spawn_commands has no plain index on ($want) after \`spawn init-db\` (its plain indexes' keys: $keys; the per-session reads of the dispatch facts scan the table)" ;;
    esac
done

# --- the fleet, idle ---------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "023 wake on change" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
for name in alpha frail idle; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 023 $name concern."
    [ "$status" -eq 0 ] || harness "resident propose $name failed: $(err)"
    capture "$SPAWN_BIN" resident approve --name "$name"
    [ "$status" -eq 0 ] || harness "resident approve $name failed: $(err)"
    capture "$SPAWN_BIN" submit --principal bb-harness --agent "agent_resident_$name" \
        --session "session_resident_$name" --key "bb-start-$name" \
        --action '{"type":"start","engine":"claude","prompt":""}'
    [ "$status" -eq 0 ] || harness "submit of $name's start failed: $(err)"
done
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!

wait_until 150 all_waiting || harness "the supervisor and the three residents were not all judged waiting within 150 s"
# Settled: no command open and no event for 5 s in a row.
quiet_since=$(now)
last_seen=$(q "select coalesce(max(id), 0) from spawn_events")
deadline=$(($(now) + 90))
while :; do
    sleep 1
    latest=$(q "select coalesce(max(id), 0) from spawn_events")
    open=$(q "select count(*) from spawn_commands where state <> 'finished'")
    if [ "$latest" != "$last_seen" ] || [ "$open" -ne 0 ]; then
        last_seen=$latest
        quiet_since=$(now)
    fi
    [ $(($(now) - quiet_since)) -lt 5 ] || break
    [ "$(now)" -lt "$deadline" ] || harness "the fleet never went quiet for 5 s within 90 s (events still appended)"
done

# --- (2) an idle minute ------------------------------------------------------------

# Two idle minutes back to back, and the lower is the measure (the header
# says why): the loop's own cost is the same in both, a host's is not.
idle_from=$(q "select coalesce(max(id), 0) from spawn_events")
snapshot "$WORK/idle.before"
sleep 60
snapshot "$WORK/idle.mid"
sleep 60
snapshot "$WORK/idle.after"
idle_events=$(q "select count(*) from spawn_events where id > $idle_from")
first=$(delta "$WORK/idle.before" "$WORK/idle.mid")
second=$(delta "$WORK/idle.mid" "$WORK/idle.after")
echo "023: idle minutes: first ${first%% *} scans, second ${second%% *}"
if [ "${second%% *}" -lt "${first%% *}" ]; then
    idle_high=${first%% *}
    set -- $second
else
    idle_high=${second%% *}
    set -- $first
fi
idle_scans=$1 idle_seq=$2 idle_idx=$3 idle_xact=$4
shift 4
idle_top="$*"
echo "023: idle minute: $idle_scans scans ($idle_seq sequential, $idle_idx by index), $idle_xact transactions, $idle_events events appended; most:$idle_top"
[ "$idle_high" -le $((2 * IDLE_MAX)) ] ||
    measured_fail "(2) the higher of the two idle minutes (no task open, no command pending, $idle_events events appended, nothing due) cost $idle_high table scans, more than $((2 * IDLE_MAX)) (twice IDLE_MAX): something reads everything once in the two minutes (a slower schedule than a minute, or a cost that settles), which the lower minute cannot see"
[ "$idle_scans" -le "$IDLE_MAX" ] ||
    measured_fail "(2) an idle minute (no task open, no command pending, $idle_events events appended, nothing due) cost $idle_scans table scans ($idle_seq sequential, $idle_idx by index; most:$idle_top), more than $IDLE_MAX (one watermark read per pass at 120 passes a minute, and slack): the loop reads everything on every pass whether or not anything changed"

# --- (3) a busy minute -------------------------------------------------------------

snapshot "$WORK/busy.before"
busy_start=$(now_ms)
: >"$WORK/broken"
t1=$(assign alpha "Look into it. BB:HANG")
t2=$(assign frail "Write it. BB:FRAIL")
for id in "$t1" "$t2"; do
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
done
fixed_ms=""
pinged=""
while :; do
    elapsed=$((($(now_ms) - busy_start) / 1000))
    [ "$elapsed" -lt 60 ] || break
    for at in 5 15 35 45; do
        if [ "$elapsed" -ge "$at" ]; then
            case " $pinged " in *" $at "*) ;; *)
                ping_supervisor
                pinged="$pinged $at"
                ;;
            esac
        fi
    done
    if [ -z "$fixed_ms" ] && [ "$elapsed" -ge 32 ]; then
        rm -f "$WORK/broken"
        fixed_ms=$(now_ms)
    fi
    sleep 0.25
done
snapshot "$WORK/busy.after"
set -- $(delta "$WORK/busy.before" "$WORK/busy.after")
busy_scans=$1 busy_seq=$2 busy_idx=$3 busy_xact=$4
shift 4
busy_top="$*"
echo "023: busy minute: $busy_scans scans ($busy_seq sequential, $busy_idx by index), $busy_xact transactions; most:$busy_top"

# T1: delivered in time.
assigned_ms=$(record_at task.assigned "$t1")
delivered_ms=$(record_at task.delivered "$t1")
[ -n "$delivered_ms" ] ||
    fail "(3) task #$t1, assigned to the idle resident alpha, was never delivered in the busy minute (state $(task_state "$t1"))"
echo "023: task #$t1 delivered $((delivered_ms - assigned_ms)) ms after its assignment"
[ $((delivered_ms - assigned_ms)) -le "$DELIVER_MAX_MS" ] ||
    fail "(3) task #$t1 was delivered $((delivered_ms - assigned_ms)) ms after its assignment, more than $DELIVER_MAX_MS ms"

# T2: every retry when due, the first after the fix through.
sql "select created_at_ms, coalesce(finished_at_ms, 0), coalesce(succeeded::text, 'open')
       from spawn_commands
      where idempotency_key = 'deliver-$t2-0' or idempotency_key like 'deliver-$t2-0-retry-%'
      order by id" >"$WORK/attempts"
attempts=$(grep -c . "$WORK/attempts" || true)
[ "$attempts" -ge 4 ] ||
    fail "(3) task #$t2's delivery, refused before typing until the fix 32 s in, was attempted $attempts times in the busy minute; its retries are due every 5 s"
gaps=$(awk -F'|' 'NR > 1 { printf "%s ", $1 - finished } { finished = $2 }' "$WORK/attempts")
echo "023: task #$t2's attempts, each after the one before finished (ms): $gaps"
prev_finished=""
ok_at=""
while IFS='|' read -r created finished outcome; do
    if [ -n "$prev_finished" ]; then
        gap=$((created - prev_finished))
        [ "$gap" -ge 4900 ] ||
            fail "(3) a retry of task #$t2's delivery was made $gap ms after the attempt before it finished, sooner than NOT_READY_RETRY_MS (5000)"
        [ "$gap" -le $((5000 + RETRY_SLACK_MS)) ] ||
            fail "(3) a retry of task #$t2's delivery was made $gap ms after the attempt before it finished: due at 5000 ms, late by more than $RETRY_SLACK_MS ms"
    fi
    prev_finished=$finished
    if [ "$outcome" = true ] && [ -z "$ok_at" ]; then ok_at=$created; fi
done <"$WORK/attempts"
[ -n "$ok_at" ] ||
    fail "(3) task #$t2's delivery did not go through after the host was fixed 32 s in ($attempts attempts, none succeeded)"
[ "$ok_at" -ge "$fixed_ms" ] ||
    fail "(3) task #$t2's delivery succeeded before the host was fixed"

# T1's activity stall: escalated once per stall, each when due. A stall is
# measured from alpha's last fact; the busy screen is recorded again every
# 30 s (bridge.rs screen_refresh), which starts a new stall, so the minute
# may hold two, each with its own tag.
tag_like="session_resident_alpha stall-$t1-activity-%"
sql "select tag, min(c.created_at_ms), count(*) from spawn_commands c,
            lateral jsonb_array_elements_text(
                case when jsonb_typeof(c.action -> 'tags') = 'array'
                     then c.action -> 'tags' else '[]'::jsonb end) tag
      where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
        and c.action ->> 'type' = 'input' and tag like '$tag_like'
      group by tag order by 2" >"$WORK/stalls"
[ -s "$WORK/stalls" ] ||
    fail "(3) task #$t1 (delivered, its worker busy and quiet) never had its activity stall escalated to the supervisor within the busy minute (SPAWN_STALL_ACTIVITY_SECS=$STALL_SECS)"
lateness=""
while IFS='|' read -r tag digest_ms copies; do
    [ "$copies" -eq 1 ] ||
        fail "(3) task #$t1's stall '$tag' was escalated $copies times; once per stall"
    last_fact=$(q "select max(recorded_at_ms) from spawn_events
                    where session_id = 'session_resident_alpha' and source <> 'observer'
                      and event_name not in ('artifact.committed', 'merge.conflicted')
                      and recorded_at_ms < $digest_ms")
    if [ -z "$last_fact" ] || [ "$last_fact" -lt "$delivered_ms" ]; then last_fact=$delivered_ms; fi
    due=$((last_fact + STALL_SECS * 1000))
    lateness="$lateness $((digest_ms - due))"
    [ "$digest_ms" -ge $((due - 500)) ] ||
        fail "(3) task #$t1's stall '$tag' was escalated $((due - digest_ms)) ms before it was due (alpha's last fact + ${STALL_SECS} s)"
    [ "$digest_ms" -le $((due + STALL_SLACK_MS)) ] ||
        fail "(3) task #$t1's stall '$tag' was escalated $((digest_ms - due)) ms after it was due (alpha's last fact + ${STALL_SECS} s), late by more than $STALL_SLACK_MS ms: a timer must wake the loop"
done <"$WORK/stalls"
echo "023: task #$t1's stall digests, each after its due time (ms):$lateness"
# The passes that had something to act on, at most: the busy minute's 500 ms
# slices that hold an event.
slices=$(q "select count(distinct (recorded_at_ms - $busy_start) / 500) from spawn_events
             where recorded_at_ms >= $busy_start and recorded_at_ms < $busy_start + 60000")
echo "023: busy minute: events in $slices of its 120 half-second slices"

busy_max=$((IDLE_MAX + SLICE_MAX * slices))
[ "$busy_scans" -le "$busy_max" ] ||
    measured_fail "(3) the busy minute cost $busy_scans table scans ($busy_seq sequential, $busy_idx by index; most:$busy_top), more than $busy_max ($IDLE_MAX, and $SLICE_MAX for each of the $slices half-second slices that held an event): its steps ran on passes where nothing had changed"

if [ -n "$MEASURE" ]; then
    echo "023: measured (BB_023_MEASURE=1): idle $idle_scans, busy $busy_scans scans a minute"
    asserted=1
    exit 1
fi
echo "023: an idle minute cost $idle_scans table scans, a busy one $busy_scans; deliveries, retries and the stall happened when due"
exit 0
