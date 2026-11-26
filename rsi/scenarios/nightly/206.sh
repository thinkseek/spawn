#!/bin/sh
# Black box: every engine the Bridge starts refuses cross-session peer
# messages, so no other Claude Code session of this user can deliver text
# into it. Spawn's own input, typed into the engine's pane, still lands, and
# the model relay is still handed to every engine (issue #42).
#
# The record (rev-f, read-only, 2026-11-16; sync/queue.md lead 21:55 and
# 22:03, lead-mac 22:04). Every loop engine and every ccd session runs as one
# user with one HOME and one XDG_RUNTIME_DIR, so each Claude Code process
# lists the others from ~/.claude/sessions/<pid>.json and can dial
# $XDG_RUNTIME_DIR/cc-socks/<pid>.sock. Claude Code's SendMessage then
# delivers into an engine. The engines run with
# --allow-dangerously-skip-permissions, so under Claude Code's default
# (mode parity) a message from another session that also skips the prompts
# is delivered with no one approving it. Spawn never sends such a message:
# its input to an engine is typed into the pane (the Bridge's tmux delivery).
#
# Claude Code 2.1.325 (the installed
# @anthropic-ai/claude-code-linux-arm64/claude, built 2026-11-13T18:52:23Z)
# has the setting `crossSessionInbound`. Its schema text: "Inbound
# cross-session peer messages (SendMessage from your other sessions):
# 'accept' delivers them, 'hold' parks them for your review without letting
# Claude act, 'refuse' opts this session out. An explicit value always
# wins." Its inline --settings are the flag source, read after managed
# settings and before the user's own.
#
# A real `spawn loop` (the Bridge, the dispatcher, the supervisor) runs a
# mission that takes its work from an objective backlog, with a resident
# (bbone), through a fake model relay the Bridge can reach. Every engine is
# the fake engine below, which keeps the --settings it is handed and every
# line typed into it. One objective is assigned and delivered (to the
# resident). Then the relay stops, and an ephemeral worker is given a task
# of a second mission: its engine starts on Claude Code's own settings (the
# Bridge's fallback when the relay is down).
#   (1) Each engine started (the supervisor's, the resident's, the
#       ephemeral worker's, and any other) was handed --settings whose
#       crossSessionInbound is "refuse".
#   (2) Its other settings are as before: a non-empty permissions.deny list
#       and skipDangerousModePermissionPrompt true; the engines started
#       through the relay were handed its URL and small model in env, and
#       the worker started with the relay down was handed no env.
#   (3) Spawn's typed input still lands. Each task is typed into its engine
#       (the objective's into the resident, the ephemeral task into the
#       worker), and a line submitted as an `input` command (BB:SAY) to the
#       supervisor and to the resident is typed into each within 30 s.
#   (4) Nothing the scenario started outlives it: no background job, and
#       10 s after its private tmux server is ended, no process in that
#       server or in any of its panes' sessions. The limit: a process that
#       leaves its pane's session (setsid) is out of this check's reach, as
#       nothing then ties it to the scenario; the fake engine never does so.
# The limit of the check: Claude Code enforces the setting, and the scenario
# cannot run Claude Code (no print mode). It reads what the Bridge hands a
# fake engine; it does not watch a real engine refuse a peer message.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "206: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl python3; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME in WORK: nothing here reads or writes the real home, nor
# is left outside it (cc-rules (a), 2026-11-12).
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "206"
SPAWN_DB_NAME="bb_206_runs_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
relay_pid=""
# The engines run in a private tmux server. kill-server returns before
# their processes have ended, so the world is stopped in order: the loop
# (which would start engines again), the serve and the relay, then the tmux
# server. Then, bounded, it waits until no process is left in any pane's
# session (each pane is its session's leader) or the server. Only these
# pids, which this script's own server made, are ever signalled.
# stop_world leaves in $WORK/left what is still alive after 10 s.
stop_world() {
    for pid in "$loop_pid" "$serve_pid" "$relay_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    loop_pid="" serve_pid="" relay_pid=""
    sessions=$(herdr_pane_pids)
    server=$(herdr_server_pid)
    herdr_kill_server
    i=0
    while :; do
        left=""
        [ -z "$server" ] || ! kill -0 "$server" 2>/dev/null || left="$left server:$server"
        if [ -n "$sessions" ]; then
            # One read of every process's stat: its pid first, its session
            # the 4th field after the ") " that ends its name.
            left="$left$(cat /proc/[0-9]*/stat 2>/dev/null | awk -v want=" $(echo $sessions) " '
                { pid = $1; sub(/.*\) /, ""); split($0, f, " ")
                  if (index(want, " " f[4] " ")) printf " %s", pid }')"
        fi
        [ -n "$left" ] || break
        i=$((i + 1))
        if [ "$i" -ge 50 ]; then
            break
        fi
        sleep 0.2
    done
    printf '%s' "$left" >"$WORK/left" 2>/dev/null || true
    for pid in $left; do
        kill -KILL "${pid#server:}" 2>/dev/null || true
    done
}
cleanup() {
    stop_world
    herdr_cleanup
    if [ -n "$database_made" ]; then
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
            echo "206: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/engines.log" ]; then
            echo "206: the engines' starts (ms|resident|execution|start|pid|native):" >&2
            cat "$WORK/engines.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "206: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# A failed check is reported and the run goes on; the run fails at its end
# if any did. Unless the database is gone: then nothing read can be trusted.
: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
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
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet. It keeps what an engine is handed: its --settings, as
# settings-<session>.json, and every line typed into it, in
# typed-<session>.log (the session's id with every character outside
# [A-Za-z0-9_] as _). It writes its native transcript where Claude Code
# does, so the Bridge sees its turns as a real engine's.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
settings=
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=$2 ;;
  --session-id) native=$2 ;;
  --settings) settings=$2 ;;
  esac
  shift
done
key=$(printf '%s' "$SPAWN_SESSION_ID" | tr -c 'A-Za-z0-9_' '_')
printf '%s' "$settings" >"$BB_WORK/settings-$key.json"
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
while :; do
  if bb_read_line typed; then
    printf '%s\n' "$typed" >>"$BB_WORK/typed-$key.log"
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
  elif [ "$pending" = 1 ]; then
    printf '\nidle\n❯ '
    sleep 1
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_ENVELOPE SPAWN_SLICE http_proxy https_proxy \
    HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# A mission with no task in work this long is idle.
SPAWN_ALERT_MISSION_IDLE_SECS=8
SPAWN_RECOVER_BACKOFF_SECS=1
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# The supervisor runs, as on the host: its engine is one the Bridge starts.
SPAWN_SUPERVISOR=1
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_MISSION_IDLE_SECS \
    SPAWN_RECOVER_BACKOFF_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_SUPERVISOR

# A model relay the Bridge can reach. It only needs to accept a connection
# (the Bridge's check before each start), so every engine is started through
# it and handed its models, as on the host.
cat >"$WORK/relay.py" <<'RELAY'
import os, socket, sys
work = sys.argv[1]
server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen(16)
with open(os.path.join(work, "relay.port.partial"), "w") as f:
    f.write(str(server.getsockname()[1]))
os.rename(os.path.join(work, "relay.port.partial"), os.path.join(work, "relay.port"))
while True:
    connection, _ = server.accept()
    connection.close()
RELAY
python3 "$WORK/relay.py" "$WORK" >"$WORK/relay.log" 2>&1 &
relay_pid=$!
tries=0
until [ -s "$WORK/relay.port" ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 100 ] || harness "the fake model relay did not start: $(head -c 300 "$WORK/relay.log")"
    sleep 0.1
done
SPAWN_MODEL_RELAY=http://127.0.0.1:$(cat "$WORK/relay.port")
SPAWN_SMALL_MODEL=bb-206-small
export SPAWN_MODEL_RELAY SPAWN_SMALL_MODEL
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl/scenarios" "$WORK/ctl/backlog" "$WORK/web"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/engines.log"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

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
note() { echo "206: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}
# objective_assign <mission> <objective>: the integrator's own path; prints
# the task's id, or nothing (the refusal in $WORK/assign.err).
objective_assign() {
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 \
        -c "select spawn_objective_assign($1, $2, '$WORK/ctl/scenarios/$2.sh', 'Objective $2 of 206 mission $1', null)" \
        2>"$WORK/assign.err" | head -n 1 || true
}
task_state() { q "select state from mission_tasks where id = $1"; }
delivered() { [ "$(task_state "$1")" = delivered ]; }
worker_typed() { # the engine that was typed the task (the resident does objective work)
    for f in "$WORK"/typed-*.log; do
        [ -f "$f" ] || continue
        grep -qi "task #$t1\b" "$f" && { worker_key=${f#"$WORK"/typed-}; worker_key=${worker_key%.log}; return 0; }
    done
    return 1
}

# --- the mission -------------------------------------------------------------------

capture "$SPAWN_BIN" resident propose --name bbone --charter "Owns the 206 work."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name bbone
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
record '{"type":"mission.added","id":1,"text":"206 mission 1","origin":"human","sources":[]}' >/dev/null
record "{\"type\":\"mission.configured\",\"id\":1,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"bbone\",\"workers\":1}}" >/dev/null
t1=$(objective_assign 1 1)
[ -n "$t1" ] || harness "spawn_objective_assign failed: $(head -c 300 "$WORK/assign.err")"

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
wait_until 90 delivered "$t1" || harness "task #$t1 was not delivered within 90 s (it is $(task_state "$t1"))"
worker_key=""
wait_until 30 worker_typed || {
    for f in "$WORK"/typed-*.log "$WORK"/settings-*.json; do [ -f "$f" ] && echo "206: seen ${f#"$WORK"/}: $(head -c 160 "$f" | tr '\n' ' ')" >&2; done
    harness "no engine was typed task #$t1 within 30 s of its delivery"
}
resident_key=$(printf '%s' session_resident_bbone | tr -c 'A-Za-z0-9_' '_')
supervisor_key=session_supervisor
supervisor_started() { [ -s "$WORK/settings-$supervisor_key.json" ]; }
wait_until 60 supervisor_started || harness "the supervisor's engine was not started within 60 s of the loop's start"
keys=$(printf '%s\n%s\n%s\n' "$worker_key" "$resident_key" "$supervisor_key" | sort -u)
for key in $keys; do
    [ -s "$WORK/settings-$key.json" ] || harness "the engine of $key was handed no --settings"
    jq -e 'type == "object"' "$WORK/settings-$key.json" >/dev/null 2>&1 ||
        harness "the engine of $key was handed --settings that are no JSON object: $(head -c 300 "$WORK/settings-$key.json")"
done
note "engines: the task went to $worker_key; the resident is $resident_key; the supervisor is $supervisor_key"


# --- the relay stops; an ephemeral worker starts on Claude Code's own settings ---

relayed=$keys
kill "$relay_pid" 2>/dev/null || true
wait "$relay_pid" 2>/dev/null || true
relay_pid=""
# Written as the first one is (the CLI's mission add would take id 1 again).
record '{"type":"mission.added","id":2,"text":"206 ephemeral worker","origin":"human","sources":[]}' >/dev/null
mission2=2
capture "$SPAWN_BIN" task assign --mission "$mission2" --kind ephemeral --worker eph \
    --brief "Do the 206 task and report."
[ "$status" -eq 0 ] || harness "task assign (eph) failed: $(err)"
t2=$(q "select max(id) from mission_tasks where mission_id = $mission2")
eph_session=$(q "select session_id from mission_tasks where id = $t2")
[ -n "$eph_session" ] || harness "task $t2 names no session"
eph_key=$(printf '%s' "$eph_session" | tr -c 'A-Za-z0-9_' '_')
eph_typed() { grep -qi "task #$t2\b" "$WORK/typed-$eph_key.log" 2>/dev/null; }
wait_until 90 eph_typed || harness "task #$t2 was not typed into the ephemeral worker $eph_session within 90 s (it is $(task_state "$t2"))"
[ -s "$WORK/settings-$eph_key.json" ] || harness "the engine of $eph_key was handed no --settings"
keys="$keys
$eph_key"
note "the relay stopped; the ephemeral worker is $eph_key"

# --- (1) every engine refuses cross-session peer messages ---------------------

# Any other engine the loop started is held to the same.
for f in "$WORK"/settings-*.json; do
    [ -f "$f" ] || continue
    key=${f#"$WORK"/settings-}
    key=${key%.json}
    case " $(printf '%s' "$keys" | tr '\n' ' ') " in *" $key "*) ;; *) keys="$keys
$key" ;; esac
done
started=0
for key in $keys; do
    started=$((started + 1))
    expect_eq "$(jq -r '.crossSessionInbound // "unset"' "$WORK/settings-$key.json")" refuse \
        "(1) crossSessionInbound in the --settings handed to the engine of $key"
done
note "(1) checked: $started engines"

# --- (2) the relay and the rest of the settings are as before -----------------

for key in $relayed; do
    f=$WORK/settings-$key.json
    expect_eq "$(jq -r '.env.ANTHROPIC_BASE_URL // "unset"' "$f")" "$SPAWN_MODEL_RELAY" \
        "(2) the model relay handed to the engine of $key"
    expect_eq "$(jq -r '.env.ANTHROPIC_DEFAULT_HAIKU_MODEL // "unset"' "$f")" bb-206-small \
        "(2) the small model handed to the engine of $key"
done
expect_eq "$(jq -r '.env // "unset"' "$WORK/settings-$eph_key.json")" unset \
    "(2) env handed to the ephemeral worker started with the relay down"
for key in $keys; do
    f=$WORK/settings-$key.json
    expect_eq "$(jq -r '(.permissions.deny // []) | length > 0' "$f")" true \
        "(2) a permissions.deny list handed to the engine of $key"
    expect_eq "$(jq -r '.skipDangerousModePermissionPrompt' "$f")" true \
        "(2) skipDangerousModePermissionPrompt handed to the engine of $key"
done
note "(2) checked: the relay, the deny list and the prompt skip"

# --- (3) Spawn's typed input still lands --------------------------------------

grep -qi "task #$t1\b" "$WORK/typed-$worker_key.log" ||
    check_failed "(3) task #$t1 was not typed into the engine of $worker_key"
eph_typed || check_failed "(3) task #$t2 was not typed into the ephemeral worker $eph_key"
say() { # <agent> <session> <words>: an input command, as a person submits one
    capture "$SPAWN_BIN" submit --principal bb-harness --agent "$1" --session "$2" \
        --key "bb-206-$run-$2" --action "{\"type\":\"input\",\"text\":\"BB:SAY $3\"}"
    [ "$status" -eq 0 ] || harness "submit of a line to $2 failed: $(err)"
}
said() { # <key> <words>
    grep -qF "BB:SAY $2" "$WORK/typed-$1.log" 2>/dev/null
}
say agent_supervisor session_supervisor "206 to the supervisor"
say agent_resident_bbone session_resident_bbone "206 to the resident"
wait_until 30 said "$supervisor_key" "206 to the supervisor" ||
    check_failed "(3) a line submitted to the supervisor was not typed into its engine within 30 s"
wait_until 30 said "$resident_key" "206 to the resident" ||
    check_failed "(3) a line submitted to the resident was not typed into its engine within 30 s"
note "(3) checked: the two tasks and two submitted lines were typed in"

# Nothing this script started outlives it: the loop, the serve and the
# relay are ended, and no process is left in the tmux server or any pane's
# session 10 s after the server's end. Not $(jobs -p): a command
# substitution is a subshell, which holds no jobs under dash; dash also lists
# an ended job until it is reported, so only a listed pid still alive counts.
stop_world
jobs -p >"$WORK/jobs.left"
alive=""
while IFS= read -r job; do
    if [ -n "$job" ] && kill -0 "$job" 2>/dev/null; then alive="$alive job:$job"; fi
done <"$WORK/jobs.left"
[ ! -s "$WORK/left" ] || alive="$alive$(cat "$WORK/left")"
[ -z "$alive" ] || check_failed "(4) processes outlive the scenario 10 s after its end:$alive"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "206: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "206: every engine (the supervisor, the resident, an ephemeral worker started with the relay down) is handed crossSessionInbound refuse beside its relay or none and its deny list, and Spawn's typed input still lands"
exit 0
