#!/bin/sh
# Black box: a delivery whose retries are spent without ever typing is
# re-issued, not left for the owner. The task is cancelled and assigned
# again, once, after a bounded wait, recorded as such with its attempts; only
# a re-issue whose delivery is spent too is escalated. After a deploy
# (`spawn mission moved`) every spent delivery is tried once more, since the
# host may have been fixed.
#
# A real `spawn loop` runs a supervisor and two residents, `frail1` and
# `frail2`, on a fake engine, with no model judge; the residents are started
# idle (no prompt) and get their tasks typed in by the dispatcher. One
# mission on a source, no gate. The host's fault is injected from outside
# Spawn, never by editing it: the tmux wrapper refuses to stage any text
# naming `BB:FRAIL1` (or `BB:FRAIL2`) while $WORK/broken-1 (broken-2)
# exists, failing `load-buffer` with tmux's own words, `command too long`
# (task 66's fault on 2026-11-06): nothing is typed, every attempt is refused
# before typing. The loop runs with SPAWN_DELIVERY_RETRY_SECS=2 (the wait
# before the first counted retry; it doubles) and SPAWN_REISSUE_AFTER_SECS=10
# (the wait after a spent delivery before its re-issue); the stall bounds are
# long. Checked:
#   (1) spent: task T1 (frail1, broken) has its delivery's retries spent
#       within 150 s: every attempt (`deliver-T1-0`, `-retry-<n>`) failed,
#       none may have typed, and the failures past the free refusals number
#       more than three.
#   (2) re-issued, not escalated: no sooner than 10 s and within 40 s after
#       the last attempt, T1 is cancelled by `spawn-dispatch`: its
#       `task.cancelled` record names `by` `spawn-dispatch`, `reissued_as`
#       T2 and `attempts` (one entry per attempt, with its key); T2's
#       `task.assigned` names `reissue_of` T1 and has T1's mission, kind,
#       worker, brief and model. The supervisor never gets T1's delivery
#       failure (no digest tag `session_resident_frail1 deliver-T1-0`).
#   (3) the re-issue goes through: the host fixed (broken-1 removed once T2
#       exists), T2 is delivered within 60 s; it is the only re-issue of T1
#       and its delivery is never escalated.
#   (4) escalated only when the re-issue is spent too: task T3 (frail2,
#       broken for good) is spent, re-issued as T4, T4 is spent; then, within
#       60 s, the supervisor gets T4's delivery failure (tag
#       `session_resident_frail2 deliver-T4-0`), its line naming task #T4 and
#       task #T3; never T3's; and T4 is not re-issued.
#   (5) after a deploy, tried once more: the host fixed (broken-2 removed),
#       the owner moves the source's main and runs `spawn mission moved`,
#       which records `source.moved` though no mission has a baseline to
#       move; within 60 s exactly one new attempt of T4's delivery is made,
#       keyed `deliver-T4-0-moved-<that record's event id>`, after the
#       record; it succeeds and T4 is delivered.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (06cc205, the deployed binary built
# from it), which reads no SPAWN_DELIVERY_RETRY_SECS: its counted retries
# wait 60, 120 and 240 s, so T1's delivery is not spent within 150 s. With
# that bound lifted (BB_019_SPENT_WITHIN=600, only to show today's
# behaviour) it fails at (2): T1's spent delivery is escalated to the
# supervisor as "never reached its worker; cancel it and assign it again",
# and nothing cancels or re-issues it (task 66, which sat two hours until the
# owner did). It passes once the dispatcher honours the two waits, re-issues
# a spent first delivery that never typed (cancel and assign again, once, in
# one record each, with the attempts), escalates only a spent re-issue, and
# tries every spent delivery once more after a `source.moved` record, which
# `spawn mission moved` writes for every deploy.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "019: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

SPENT_WITHIN=${BB_019_SPENT_WITHIN:-150}
case "$SPENT_WITHIN" in '' | *[!0-9]*) harness "BB_019_SPENT_WITHIN is a number of seconds" ;; esac

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_019_reissue_$run"
SPAWN_TMUX_SOCKET="bb-019-$run"
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
            echo "019: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "019: the tasks (id state):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, state, agent_id from mission_tasks order by id" >&2 2>/dev/null || true
            echo "019: the delivery attempts (key, ok, error):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select idempotency_key, coalesce(succeeded::text, 'open'),
                       left(coalesce(error, ''), 90)
                  from spawn_commands where idempotency_key like 'deliver-%' order by id" >&2 2>/dev/null || true
            echo "019: the digests the supervisor received (id, tags):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, action -> 'tags' from spawn_commands
                where session_id = 'session_supervisor' and principal = 'spawn-dispatch'
                  and action ->> 'type' = 'input' and succeeded order by id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "019: harness: unexpected exit $rc" >&2
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
git -C "$WORK/seed" commit -qm base
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done
base=$(git --git-dir="$source" rev-parse main)

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet, the transcript's end of the turn first, then the prompt.
# A turn whose text names `BB:WORK <file>` writes the file and the report.
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
work=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:WORK "*) [ -n "$work" ] || work=${typed#*BB:WORK } ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$work" ]; then
      set -- $work
      printf 'done\n' >"$1"
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$1" >.spawn/report.md
      printf 'wrote %s %s\n' "$(date +%s)" "$1" >>"$BB_WORK/engine.log"
      work=
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# tmux on the private socket. While $WORK/broken-<n> exists, a text naming
# `BB:FRAIL<n>` cannot be staged: `load-buffer` (the Bridge stages every
# text from a file) fails as tmux fails a command over its message size.
# Nothing reaches the pane.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
stage=
for argument in "\$@"; do
  [ "\$argument" = load-buffer ] && stage=1
  last=\$argument
done
if [ -n "\$stage" ] && [ -f "\$last" ]; then
  for n in 1 2; do
    if [ -e '$WORK'/broken-\$n ] && grep -q "BB:FRAIL\$n" "\$last" 2>/dev/null; then
      echo "command too long" >&2
      exit 1
    fi
  done
fi
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_DELIVERY_RETRY_SECS \
    SPAWN_REISSUE_AFTER_SECS; do
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
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# The waits under test, short; the stall bounds out of the way.
RETRY_SECS=2
REISSUE_AFTER=10
SPAWN_DELIVERY_RETRY_SECS=$RETRY_SECS
SPAWN_REISSUE_AFTER_SECS=$REISSUE_AFTER
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB \
    SPAWN_DISK_FLOOR_GB SPAWN_DELIVERY_RETRY_SECS SPAWN_REISSUE_AFTER_SECS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engine.log"

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
task_state() { q "select state from mission_tasks where id = $1"; }

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

# wait_until <seconds> <command...>: true once the command is, false past
# the limit; pings the supervisor while it waits.
wait_until() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        ping_supervisor
        sleep 0.5
    done
}

# The attempts at task `$1`'s first delivery: its key and its retries.
ATTEMPTS="idempotency_key = 'deliver-\$t-0' or idempotency_key like 'deliver-\$t-0-retry-%'"
attempts_of() { # <task> <select list>
    t=$1
    q "select $2 from spawn_commands where $(printf '%s' "$ATTEMPTS" | sed "s/\\\$t/$t/g")"
}
# `spent` when the task's first delivery is spent as the dispatcher judges
# it (deliver.rs `attempts`): every attempt finished failed, none may have
# typed, and more than three failures past the ten free refusals for a pane
# not ready; otherwise a short account of where it stands.
spent_state() {
    line=$(attempts_of "$1" "count(*) || ' ' ||
        count(*) filter (where succeeded) || ' ' ||
        count(*) filter (where state <> 'finished') || ' ' ||
        count(*) filter (where error like '%does not accept input%') || ' ' ||
        count(*) filter (where error like '%may have been typed%')")
    set -- $line
    total=$1 ok=$2 open=$3 refusals=$4 typed=$5
    free=$((refusals < 10 ? refusals : 10))
    if [ "$ok" -eq 0 ] && [ "$open" -eq 0 ] && [ "$typed" -eq 0 ] && [ $((total - free)) -gt 3 ]; then
        echo spent
    else
        echo "attempts $total, succeeded $ok, open $open, refused as not ready $refusals, may have typed $typed"
    fi
}
is_spent() { [ "$(spent_state "$1")" = spent ]; }
spent_at() { attempts_of "$1" "max(finished_at_ms)"; }

control() { # <type> <task id>: event id of the latest such record
    q "select coalesce(max(id), 0) from spawn_events
        where session_id = 'session_control' and event_name = 'control.recorded'
          and kind -> 'detail' -> 'record' ->> 'type' = '$1'
          and kind -> 'detail' -> 'record' ->> 'id' = '$2'"
}
rec() { # <event id> <jq-ish field path in SQL>
    q "select kind -> 'detail' -> 'record' $2 from spawn_events where id = $1"
}
reissue_of() { # tasks whose task.assigned names reissue_of $1
    q "select coalesce(string_agg(kind -> 'detail' -> 'record' ->> 'id', ' '), '') from spawn_events
        where session_id = 'session_control'
          and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
          and kind -> 'detail' -> 'record' ->> 'reissue_of' = '$1'"
}
reissued() { [ -n "$(reissue_of "$1")" ]; }

# The digests delivered to the supervisor carrying tag `$1`.
digests_with() {
    q "select count(*) from spawn_commands c,
              lateral jsonb_array_elements_text(
                  case when jsonb_typeof(c.action -> 'tags') = 'array'
                       then c.action -> 'tags' else '[]'::jsonb end) tag
        where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
          and c.action ->> 'type' = 'input' and c.state = 'finished' and c.succeeded
          and tag = '$1'"
}
digest_text_with() {
    sql "select c.action ->> 'text' from spawn_commands c,
              lateral jsonb_array_elements_text(
                  case when jsonb_typeof(c.action -> 'tags') = 'array'
                       then c.action -> 'tags' else '[]'::jsonb end) tag
        where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
          and c.action ->> 'type' = 'input' and c.succeeded and tag = '$1'
        order by c.id limit 1"
}
escalated() { [ "$(digests_with "$1")" -ge 1 ]; }
delivered() { case "$(task_state "$1")" in delivered | reported | accepted) return 0 ;; esac; return 1; }

# --- the residents, live and idle --------------------------------------------------

capture "$SPAWN_BIN" mission add --text "019 re-issue" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"

for name in frail1 frail2; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 019 $name concern."
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

for name in frail1 frail2; do
    wait_until 120 waiting "session_resident_$name" ||
        harness "resident $name was never judged waiting at its prompt"
done
wait_until 90 waiting session_supervisor || harness "the supervisor was never judged waiting"

# --- the broken host, two tasks -------------------------------------------------------

: >"$WORK/broken-1"
: >"$WORK/broken-2"
assign() {
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$1" \
        --brief "$2"
    [ "$status" -eq 0 ] || harness "task assign for $1 failed: $(err)"
    jq -r .task "$WORK/out.txt"
}
t1=$(assign frail1 "Write f1. BB:FRAIL1 BB:WORK f1.txt")
t3=$(assign frail2 "Write f2. BB:FRAIL2 BB:WORK f2.txt")
for id in "$t1" "$t3"; do
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
done

# --- (1) T1's delivery is spent -----------------------------------------------------------

wait_until "$SPENT_WITHIN" is_spent "$t1" ||
    fail "(1) task #$t1's delivery, refused before typing at every attempt, was not spent within $SPENT_WITHIN s with SPAWN_DELIVERY_RETRY_SECS=$RETRY_SECS: $(spent_state "$t1")"
t1_spent=$(spent_at "$t1")
t1_attempts=$(attempts_of "$t1" "count(*)")

# --- (2) re-issued, not escalated ------------------------------------------------------------

deadline=$((t1_spent / 1000 + REISSUE_AFTER + 40))
until reissued "$t1"; do
    if [ "$(now)" -ge "$deadline" ]; then
        if escalated "session_resident_frail1 deliver-$t1-0"; then
            fail "(2) task #$t1's delivery was spent ($t1_attempts attempts, none typed) and escalated to the supervisor ('$(digest_text_with "session_resident_frail1 deliver-$t1-0" | grep -F "task #$t1" | head -n 1)'), not re-issued; task #$t1 is $(task_state "$t1")"
        fi
        fail "(2) task #$t1's delivery was spent ($t1_attempts attempts, none typed) and not re-issued within $((REISSUE_AFTER + 40)) s; task #$t1 is $(task_state "$t1")"
    fi
    ping_supervisor
    sleep 0.5
done
t2=$(reissue_of "$t1")
# The host is fixed as soon as the re-issue exists.
rm -f "$WORK/broken-1"
case "$t2" in *" "*) fail "(2) task #$t1 was re-issued more than once: tasks $t2" ;; esac
cancelled=$(control task.cancelled "$t1")
[ "$cancelled" -gt 0 ] || fail "(2) task #$t1 was re-issued as #$t2 but has no task.cancelled record"
assert_eq "$(task_state "$t1")" cancelled "(2) task #$t1's state after its re-issue"
assert_eq "$(rec "$cancelled" "->> 'by'")" spawn-dispatch "(2) the task.cancelled record's by"
assert_eq "$(rec "$cancelled" "->> 'reissued_as'")" "$t2" "(2) the task.cancelled record's reissued_as"
assert_eq "$(rec "$cancelled" "-> 'attempts' ->> 0 is not null")" t \
    "(2) the task.cancelled record names its attempts"
assert_eq "$(q "select jsonb_array_length(kind -> 'detail' -> 'record' -> 'attempts') from spawn_events where id = $cancelled")" \
    "$t1_attempts" "(2) the attempts named by the task.cancelled record"
assert_eq "$(q "select count(*) from spawn_events, jsonb_array_elements(kind -> 'detail' -> 'record' -> 'attempts') a
                 where id = $cancelled and a ->> 'key' like 'deliver-$t1-0%'")" "$t1_attempts" \
    "(2) the attempts of the task.cancelled record that name their key"
reissue_at=$(q "select recorded_at_ms from spawn_events where id = $cancelled")
[ $((reissue_at - t1_spent)) -ge $((REISSUE_AFTER * 1000 - 1000)) ] ||
    fail "(2) task #$t1 was re-issued $(((reissue_at - t1_spent) / 1000)) s after its last attempt, before SPAWN_REISSUE_AFTER_SECS=$REISSUE_AFTER"
assigned=$(control task.assigned "$t2")
for field in mission_id kind agent_id session_id brief model; do
    assert_eq "$(q "select kind -> 'detail' -> 'record' ->> '$field' from spawn_events where id = $assigned")" \
        "$(q "select kind -> 'detail' -> 'record' ->> '$field' from spawn_events where id = $(control task.assigned "$t1")")" \
        "(2) the re-issue #$t2's $field against task #$t1's"
done
[ "$(digests_with "session_resident_frail1 deliver-$t1-0")" -eq 0 ] ||
    fail "(2) task #$t1's spent delivery, re-issued, was escalated to the supervisor too"

# --- (3) the re-issue goes through -------------------------------------------------------------

wait_until 60 delivered "$t2" ||
    fail "(3) the re-issue #$t2, the host fixed, was not delivered within 60 s: $(spent_state "$t2")"
assert_eq "$(reissue_of "$t1")" "$t2" "(3) the re-issues of task #$t1"
[ "$(digests_with "session_resident_frail1 deliver-$t2-0")" -eq 0 ] ||
    fail "(3) the re-issue #$t2's delivery, which went through, was escalated"

# --- (4) a spent re-issue is escalated ----------------------------------------------------------

wait_until $((SPENT_WITHIN + REISSUE_AFTER + 40)) reissued "$t3" ||
    fail "(4) task #$t3 was not re-issued within $((SPENT_WITHIN + REISSUE_AFTER + 40)) s: $(spent_state "$t3")"
t4=$(reissue_of "$t3")
case "$t4" in *" "*) fail "(4) task #$t3 was re-issued more than once: tasks $t4" ;; esac
wait_until "$SPENT_WITHIN" is_spent "$t4" ||
    fail "(4) the re-issue #$t4's delivery, refused at every attempt, was not spent within $SPENT_WITHIN s: $(spent_state "$t4")"
t4_tag="session_resident_frail2 deliver-$t4-0"
wait_until 60 escalated "$t4_tag" ||
    fail "(4) the re-issue #$t4, whose delivery is spent too, was not escalated to the supervisor within 60 s (no digest tagged '$t4_tag')"
line=$(digest_text_with "$t4_tag" | grep -F "task #$t4" | head -n 1)
assert_contains "$line" "task #$t4" "(4) the escalation of the spent re-issue"
assert_contains "$line" "#$t3" "(4) the escalation of the spent re-issue (the task it re-issued)"
[ "$(digests_with "session_resident_frail2 deliver-$t3-0")" -eq 0 ] ||
    fail "(4) task #$t3's spent delivery, re-issued, was escalated too"
sleep $((REISSUE_AFTER + 5))
assert_eq "$(reissue_of "$t4")" "" "(4) the re-issues of the spent re-issue #$t4"

# --- (5) after a deploy, tried once more ------------------------------------------------------------

rm -f "$WORK/broken-2"
git -C "$WORK/seed" commit -q --allow-empty -m "deploy: the host is fixed"
git -C "$WORK/seed" push -q "$source" main:main
moved_to=$(git --git-dir="$source" rev-parse main)
before_moved=$(q "select coalesce(max(id), 0) from spawn_events")
capture "$SPAWN_BIN" mission moved --source "$source" --from "$base" --to "$moved_to"
[ "$status" -eq 0 ] || fail "(5) spawn mission moved exited $status: $(err)"
moved=$(q "select coalesce(min(id), 0) from spawn_events
            where session_id = 'session_control' and id > $before_moved
              and kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'")
[ "$moved" -gt 0 ] ||
    fail "(5) \`spawn mission moved\` recorded no source.moved (it printed $(out | tr -d '\n ' | head -c 120)): the deploy is not a fact anything can act on"
moved_ms=$(q "select recorded_at_ms from spawn_events where id = $moved")
wait_until 60 delivered "$t4" ||
    fail "(5) the spent re-issue #$t4 was not delivered within 60 s of the deploy's source.moved: $(q "select count(*) from spawn_commands where idempotency_key like 'deliver-$t4-0%' and created_at_ms >= $moved_ms") attempts since"
after=$(q "select string_agg(idempotency_key || ':' || coalesce(succeeded::text, 'open'), ' ' order by id)
            from spawn_commands where idempotency_key like 'deliver-$t4-0%' and created_at_ms >= $moved_ms")
assert_eq "$after" "deliver-$t4-0-moved-$moved:true" "(5) the attempts of #$t4's delivery after the deploy"

echo "019: a delivery spent without typing was re-issued once and went through; a spent re-issue was escalated, naming both; after a deploy the spent delivery was tried once more and delivered"
exit 0
