#!/bin/sh
# Black box: the Bridge follows screens and transcripts without a
# subprocess per agent per tick. Today bridge.rs `follow_screen` runs a tmux
# capture for every live session and `follow_output` re-reads every live
# transcript, on every pass (500 ms), serially, on the loop thread, before
# the next pass claims a command: with dozens of agents a tick gets long and
# every command waits behind it. Transcripts are watched (inotify, or the
# platform's equivalent, with a polling fallback), and a session's screen is
# captured only when its transcript has been quiet for a bound or when a
# command is in flight on that session; every screen change that matters for
# delivery readiness is still seen as soon as today.
#
# A real `spawn loop` (no model judge, no supervisor, the default pass
# interval of 500 ms) runs twelve residents, `r01` to `r12`, on a fake engine,
# all started idle at their prompt. The tmux the loop runs is a wrapper on a
# private socket that notes every call and makes each `capture-pane` cost
# CAPTURE_DELAY_MS (50 ms): what one capture costs a host that runs dozens of
# agents and their tmux server (unloaded, one costs 4-8 ms here; the delay
# makes the cost of capturing every session on every pass visible with
# twelve). Checked:
#   (1) an idle fleet: every session waiting, nothing in flight, no
#       transcript growing. Over 30 s the loop captures at most
#       IDLE_CAPTURES_MAX (24) panes in all, two per session: no capture per
#       agent per pass.
#   (2) the tick: twelve inputs are submitted, one to each resident, 2.3 s
#       apart. Each is claimed (its lease taken: `lease_until_ms` minus the
#       Bridge's two-minute lease) within CLAIM_MAX_MS (750: the 500 ms pass
#       and a 250 ms tick) of its creation: a command does not wait behind a
#       tick that captures every pane.
#   (3) readiness, as soon as today: three residents are made busy (a turn
#       of 8 s whose transcript grows every 2 s and whose screen shows the
#       running turn), and a task is assigned to each while it is busy. Each
#       task's delivery (`deliver-<task>-0`) is created within READY_MAX_MS
#       (2500) of the moment the engine ended its turn (the transcript's end
#       of the turn written, the prompt back on the screen; the engine notes
#       the time), and goes through at its first attempt.
#
# Measured on the live main (49b92be, the deployed binary built from it),
# this host, 2026-11-06 (BB_025_MEASURE=1 prints them): (1) 272-276
# captures in the idle 30 s (one per session per pass, a pass every 1.3 s
# with the 50 ms captures); (2) claims 134-1745 ms after creation, mean
# 815-838 ms (without the injected delay: 15-731 ms, mean 346); (3)
# deliveries 1742-1896 ms after the turn ended. READY_MAX_MS is today's
# worst plus 0.6 s.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main: an idle fleet of twelve costs a
# capture per session per pass, about 274 in 30 s; (2) fails too (worst
# claim 1445-1745 ms), (3) passes, which is what the change must keep. It passes once the
# Bridge watches transcripts instead of re-reading them, captures a screen
# only when its transcript has been quiet for a bound or a command is in
# flight on its session, and still sees a turn's end, and the screen it
# leaves, soon enough that a waiting delivery goes as fast as today.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "025: harness: $*" >&2
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

MEASURE=${BB_025_MEASURE:-}
AGENTS=12
CAPTURE_DELAY_MS=50
IDLE_CAPTURES_MAX=24
CLAIM_MAX_MS=750
READY_MAX_MS=2500
LEASE_MS=120000

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_025_follow_$run"
SPAWN_TMUX_SOCKET="bb-025-$run"
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
            echo "025: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "025: the engines' turn ends:" >&2
            tail -n 6 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "025: harness: unexpected exit $rc" >&2
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
# A check that BB_025_MEASURE=1 only reports.
measured_fail() {
    if [ -n "$MEASURE" ]; then
        echo "025: (measure) would fail: $*"
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
# A turn whose text names `BB:BUSY <seconds>` runs that long first: the
# screen shows the running turn and the transcript grows every two seconds
# (a tool call and its result). Each turn's end is noted, with the time, in
# $BB_WORK/engine.log.
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
busy=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:BUSY "*) set -- ${typed#*BB:BUSY }; busy=$1 ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$busy" ]; then
      printf '\n✻ Working… (esc to interrupt)\n\n❯ '
      end=$(($(date +%s) + busy))
      n=0
      while [ "$(date +%s)" -lt "$end" ]; do
        sleep 2
        n=$((n + 1))
        line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tu_${SPAWN_EXECUTION_ID}_$count\",\"name\":\"Bash\",\"input\":{}}]}"
        line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tu_${SPAWN_EXECUTION_ID}_$((count - 1))\",\"content\":\"ok\"}]}"
      done
      busy=
      # The turn's last words close the running turn's region of the screen.
      printf '\nclaude: done\n'
    fi
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    printf '%s %s\n' "$(($(date +%s%N) / 1000000))" "$SPAWN_SESSION_ID" >>"$BB_WORK/engine.log"
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# tmux on the private socket, every call noted (`<ms> <subcommand>`), each
# capture-pane CAPTURE_DELAY_MS slower.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
sub=other
for argument in "\$@"; do
  [ "\$argument" != capture-pane ] || sub=capture-pane
done
printf '%s %s\n' "\$(date +%s%3N)" "\$sub" >>'$WORK/tmux.log'
[ "\$sub" != capture-pane ] || sleep $(printf '0.%03d' "$CAPTURE_DELAY_MS")
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_LOOP_INTERVAL_MS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS \
    SPAWN_STALL_HALTED_SECS; do
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
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$WORK/engine.log"
: >"$WORK/tmux.log"

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
names=$(i=1; while [ "$i" -le "$AGENTS" ]; do printf 'r%02d ' "$i"; i=$((i + 1)); done)

waiting() {
    [ "$(q "select coalesce(observation ->> 'state', '') from spawn_session_state
            where session_id = '$1'")" = WAITING ] &&
        [ "$(q "select count(*) from agent_executions
                where session_id = '$1' and ended_at_ms is null")" -ge 1 ]
}
all_waiting() {
    for name in $names; do
        waiting "session_resident_$name" || return 1
    done
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
# Settled: no command open and no event for `$1` seconds in a row.
settle() {
    quiet_since=$(now)
    last_seen=$(q "select coalesce(max(id), 0) from spawn_events")
    deadline=$(($(now) + 120))
    while :; do
        sleep 1
        latest=$(q "select coalesce(max(id), 0) from spawn_events")
        open=$(q "select count(*) from spawn_commands where state <> 'finished'")
        if [ "$latest" != "$last_seen" ] || [ "$open" -ne 0 ]; then
            last_seen=$latest
            quiet_since=$(now)
        fi
        [ $(($(now) - quiet_since)) -lt "$1" ] || return 0
        [ "$(now)" -lt "$deadline" ] || harness "the fleet never went quiet for $1 s within 120 s"
    done
}
captures_between() { # <from ms> <to ms>
    awk -v from="$1" -v to="$2" '$2 == "capture-pane" && $1 >= from && $1 < to { n++ } END { print n + 0 }' \
        "$WORK/tmux.log"
}
submit_input() { # <name> <key> <text>
    capture "$SPAWN_BIN" submit --principal bb-harness --agent "agent_resident_$1" \
        --session "session_resident_$1" --key "$2" \
        --action "{\"type\":\"input\",\"text\":\"$3\"}"
    [ "$status" -eq 0 ] || harness "submit of an input to $1 failed: $(err)"
}

# --- twelve residents, idle -----------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "025 follow screens" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
for name in $names; do
    capture "$SPAWN_BIN" resident propose --name "$name" --charter "Owns the 025 $name concern."
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
wait_until 240 all_waiting || harness "the twelve residents were not all judged waiting within 240 s"
settle 5

# --- (1) an idle fleet ------------------------------------------------------------------

idle_from=$(now_ms)
sleep 30
idle_to=$(now_ms)
idle=$(captures_between "$idle_from" "$idle_to")
echo "025: idle 30 s, twelve sessions: $idle pane captures"
[ "$idle" -le "$IDLE_CAPTURES_MAX" ] ||
    measured_fail "(1) an idle fleet of $AGENTS (every session waiting, nothing in flight, no transcript growing) had its panes captured $idle times in 30 s, more than $IDLE_CAPTURES_MAX: a tmux subprocess per agent per tick"

# --- (2) the tick: a command waits for no capture ---------------------------------------------

i=0
for name in $names; do
    i=$((i + 1))
    submit_input "$name" "bb-hello-$i" "hello"
    sleep 2.3
done
deadline=$(($(now) + 60))
until [ "$(q "select count(*) from spawn_commands where idempotency_key like 'bb-hello-%' and state = 'finished'")" -eq "$AGENTS" ]; do
    [ "$(now)" -lt "$deadline" ] || fail "(2) the $AGENTS inputs were not all finished within 60 s"
    sleep 0.5
done
sql "select lease_until_ms - $LEASE_MS - created_at_ms from spawn_commands
      where idempotency_key like 'bb-hello-%' order by id" >"$WORK/claims"
claims=$(tr '\n' ' ' <"$WORK/claims")
worst=$(sort -n "$WORK/claims" | tail -n 1)
mean=$(awk '{ s += $1 } END { printf "%d", s / NR }' "$WORK/claims")
echo "025: claims after creation (ms): $claims(mean $mean, worst $worst)"
[ "$worst" -le "$CLAIM_MAX_MS" ] ||
    measured_fail "(2) a command waited $worst ms for the Bridge to claim it (mean $mean ms over $AGENTS), more than $CLAIM_MAX_MS: the pass in front of it captured every pane"

# --- (3) readiness, as soon as today -------------------------------------------------------------

settle 3
tasks=""
for name in r01 r05 r09; do
    submit_input "$name" "bb-busy-$name" "BB:BUSY 8"
done
sleep 2
for name in r01 r05 r09; do
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker "$name" \
        --brief "Carry on."
    [ "$status" -eq 0 ] || harness "task assign for $name failed: $(err)"
    id=$(jq -r .task "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "task assign printed no id"
    tasks="$tasks $name:$id"
done
for pair in $tasks; do
    name=${pair%%:*}
    id=${pair#*:}
    deadline=$(($(now) + 60))
    until [ "$(q "select count(*) from spawn_commands
                   where (idempotency_key = 'deliver-$id-0' or idempotency_key like 'deliver-$id-0-%')
                     and succeeded")" -ge 1 ]; do
        [ "$(now)" -lt "$deadline" ] ||
            fail "(3) task #$id, assigned to $name while it was busy, was not delivered within 60 s of its turn's end (attempts: $(q "select string_agg(idempotency_key || ':' || coalesce(succeeded::text, 'open') || ':' || left(coalesce(error, ''), 60), ' ' order by id) from spawn_commands where idempotency_key like 'deliver-$id-0%'"))"
        sleep 0.5
    done
    retries=$(q "select count(*) from spawn_commands where idempotency_key like 'deliver-$id-0-%'")
    [ "$retries" -eq 0 ] ||
        fail "(3) task #$id's delivery needed $retries retries ($(q "select string_agg(idempotency_key || ':' || coalesce(succeeded::text, 'open') || ':' || left(coalesce(error, ''), 60), ' ' order by id) from spawn_commands where idempotency_key like 'deliver-$id-0%'")): it was sent before its worker was ready"
    created=$(q "select created_at_ms from spawn_commands where idempotency_key = 'deliver-$id-0'")
    ended=$(awk -v s="session_resident_$name" -v c="$created" '$2 == s && $1 <= c { t = $1 } END { print t + 0 }' "$WORK/engine.log")
    [ "$ended" -gt 0 ] || fail "(3) task #$id's delivery was created before $name's busy turn ended"
    latency=$((created - ended))
    echo "025: task #$id ($name) delivered $latency ms after its worker's turn ended"
    [ "$latency" -le "$READY_MAX_MS" ] ||
        fail "(3) task #$id waited $latency ms after $name's turn ended (the transcript's end of turn written, the prompt back) for its delivery, more than $READY_MAX_MS ms (today's worst and slack): a screen change that decides readiness was seen late"
done

if [ -n "$MEASURE" ]; then
    echo "025: measured (BB_025_MEASURE=1): idle captures $idle, claims worst $worst mean $mean"
    asserted=1
    exit 1
fi
echo "025: an idle fleet of $AGENTS cost $idle captures in 30 s; commands were claimed within $worst ms; waiting deliveries went as soon as their workers' turns ended"
exit 0
