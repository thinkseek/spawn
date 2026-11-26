#!/bin/sh
# Black box: a mission that has no work says so where the owner looks.
#
# The problem, measured on the live fleet (sync/backlog.md, rev-b 058):
# mission 4 (Spawn RSI) had no task in work for 32.5 h from 2026-11-08
# 23:04Z to 11-10 07:35Z, then for 3.3 h, 1.7 h and 1 h on 11-11, and has had
# none since 15:15Z; mission 6 (Supply PA) has had none since 11-10 19:29Z,
# 20 h, its 108 objectives solved and none open. The fleet was idle 40
# minutes after it got 056 and 057 (lead-mac, queue.md 23:35). Home says
# only "No mission has work open" (Home.tsx, the Missions card): no alert,
# nothing in Needs you, no time since, no mission named as the one waiting,
# and an idle fleet is the most expensive state the owner has. The
# scorecard's "Idle hours by mission" flags the two owner-work missions (#7
# 75%, #8 76%) as idle beside #4 (37%) and #6 (40%), though owner work has
# no backlog of its own (lead-mac, queue.md 23:45): idle is a fault only for
# a mission that takes its work from an objective backlog.
#
# Required (read through `GET /api/alerts` and web/src/home.ts's needsYou
# under node over it), with SPAWN_ALERT_MISSION_IDLE_SECS=8 (the bound):
#   (1) an active mission that takes its work from an objective backlog,
#       whose last task ended (cancelled) and that has no task in work for
#       the bound, fires ONE open `mission_idle` alert,
#       subject `mission #<id>`; its evidence names `mission`, `since_ms`
#       (when its last task ended) and `event_id` (that record), and its
#       text names the mission.
#   (2) a mission whose task is still in work (delivered) fires none, a
#       closed mission fires none, and an owner-work mission (no objective
#       backlog: its work comes from the owner, so no task is no fault)
#       fires none: read once (1)'s alert is open, which a pass after the
#       bound fired (the barrier); each was without work longer than 1.
#   (3) Needs you shows (1)'s alert as one item naming mission #1.
#   (4) a task assigned to the mission clears it: cleared_evidence
#       `event_id` the `task.assigned` record.
#
# A real `spawn loop` runs (its alert step reads every 2 s) and `spawn serve`
# answers the API. Tasks are assigned through spawn_control_record(), as the
# integrator writes them, and cancelled with `spawn task cancel`; a mission
# is closed with `spawn mission close`. Each check is reported; the run
# fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "058: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
[ -f "$tree/web/src/home.ts" ] || harness "no web/src/home.ts under $tree (run from the candidate's tree)"
for tool in psql tmux git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and the tmux server's socket, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$HOME" "$TMUX_TMPDIR"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME TMUX_TMPDIR
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_058_runs_$run"
SPAWN_TMUX_SOCKET="bb-058-$run"
export SPAWN_TERMINAL_BACKEND=tmux
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
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
            echo "058: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "058: the mission_idle alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'mission_idle' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "058: harness: unexpected exit $rc" >&2
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

# A quiet engine: the loop may deliver the tasks below to a worker; this
# one shows a prompt, takes what is typed and does nothing with it.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if bb_read_line typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# The bound under test: a mission with no task in work this long is idle.
SPAWN_ALERT_MISSION_IDLE_SECS=8
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
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_MISSION_IDLE_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl/scenarios" "$WORK/web"
: >"$SPAWN_MISSION_FILE"
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
note() { echo "058: $*" >&2; }
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

assign() { # <task> <mission>: prints the event id
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":$2,\"objective\":$1,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$1\",\"session_id\":\"session_worker_objective_$1\",\"scenario\":\"$WORK/ctl/scenarios/objective-$1\",\"brief\":\"Objective $1 of 058\"}"
}
task_state() { q "select state from mission_tasks where id = $1"; }
delivered() { [ "$(task_state "$1")" = delivered ]; }
cancel() { # <task>: the owner's cancel; prints its event id
    capture "$SPAWN_BIN" task cancel --id "$1"
    [ "$status" -eq 0 ] || harness "spawn task cancel --id $1 failed: $(err)"
    q "select max(id) from spawn_events where event_name = 'control.recorded'
         and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
         and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}
event_ms() { q "select recorded_at_ms from spawn_events where id = $1"; }

start_serve() {
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >"$WORK/serve.log" 2>&1 &
    serve_pid=$!
    port=""
    i=0
    while [ "$i" -lt 100 ]; do
        i=$((i + 1))
        port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
        [ -n "$port" ] && break
        sleep 0.1
    done
    [ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
    base="http://127.0.0.1:$port"
}
alerts_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/alerts.json" -w '%{http_code}' "$base/api/alerts" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/alerts answered $code: $(head -c 300 "$WORK/alerts.json" 2>/dev/null)"
}
idle_of() { # <mission>: its mission_idle alerts in /api/alerts, any state
    alerts_json
    jq -c --arg s "mission #$1" '[.[] | select(.kind == "mission_idle" and .subject == $s)]' "$WORK/alerts.json"
}
summary() { printf '%s' "$1" | jq -c '[.[] | {key, state, text}]' | head -c 400; }

# --- the missions ----------------------------------------------------------------

# Missions 1, 2 and 3 take their work from an objective backlog (as the
# live RSI and Supply PA missions do); mission 4 is owner work: no backlog,
# its tasks come from the owner when there is something to do.
mkdir -p "$WORK/ctl/backlog"
for id in 1 2 3; do
    record "{\"type\":\"mission.added\",\"id\":$id,\"text\":\"058 mission $id\",\"origin\":\"human\",\"sources\":[]}" >/dev/null
    record "{\"type\":\"mission.configured\",\"id\":$id,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"bbres\",\"workers\":1}}" >/dev/null
done
record '{"type":"mission.added","id":4,"text":"058 owner work","origin":"human","sources":[]}' >/dev/null
record '{"type":"mission.configured","id":4,"accept":"review"}' >/dev/null

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve

# Mission 2 has a task, then is closed; mission 3's task stays in work;
# mission 1's task is cancelled last.
assign 5 4 >/dev/null
assign 2 2 >/dev/null
assign 3 3 >/dev/null
assign 1 1 >/dev/null
for task in 1 2 3 5; do
    wait_until 60 delivered "$task" || harness "task #$task was not delivered within 60 s (it is $(task_state "$task"))"
done
capture "$SPAWN_BIN" mission close --id 2 --state done --summary "058 closes it"
[ "$status" -eq 0 ] || harness "spawn mission close --id 2 failed: $(err)"
cancel 5 >/dev/null
c1=$(cancel 1)
c1_ms=$(event_ms "$c1")

# --- (1) mission 1, with no task in work for the bound ------------------------------

open_one() { [ "$(idle_of 1 | jq -c '[.[] | .state]')" = '["open"]' ]; }
if wait_until 40 open_one; then
    got=$(idle_of 1)
    expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .evidence.mission]')" '[1]' "(1) the alert's evidence.mission"
    expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .evidence.event_id]')" "[$c1]" \
        "(1) the alert's evidence.event_id (the record that ended mission 1's last task: its cancel)"
    expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .evidence.since_ms]')" "[$c1_ms]" \
        "(1) the alert's evidence.since_ms (when that record was written)"
    expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .text | test("mission #1\\b")]')" '[true]' \
        "(1) the alert's text names mission #1: $(summary "$got")"
    note "(1) checked: mission 1's idle alert"
    # --- (2) the barrier passed: missions 2 and 3 --------------------------------------
    got=$(idle_of 3)
    expect_eq "$(printf '%s' "$got" | jq length)" 0 \
        "(2) mission_idle alerts for mission 3, whose task is delivered (in work): $(summary "$got")"
    got=$(idle_of 2)
    expect_eq "$(printf '%s' "$got" | jq length)" 0 \
        "(2) mission_idle alerts for mission 2, closed before mission 1's task ended: $(summary "$got")"
    got=$(idle_of 4)
    expect_eq "$(printf '%s' "$got" | jq length)" 0 \
        "(2) mission_idle alerts for mission 4, owner work (no objective backlog), without a task since before mission 1's ended: $(summary "$got")"
    note "(2) checked: no alert for a mission in work or a closed one"
else
    check_failed "(1) mission 1 has had no task in work since its cancel (record $c1), past the 8 s bound, and has no open mission_idle alert 40 s on: $(summary "$(idle_of 1)"); (2) is not judged without it"
fi

# --- (3) Needs you: one item naming mission #1 ------------------------------------------

cat >"$WORK/needs.test.mjs" <<'EOF'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { needsYou } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const alerts = JSON.parse(readFileSync(env.BB_ALERTS, "utf8"));
const items = needsYou({ attention: null, missions: null, alerts, agents: [], health: null, missingAlwaysOn: 0, now: Date.now() });
test("one item for the idle mission, naming it", () => {
  const idle = items.filter((item) => String(item.key).startsWith("alert:mission_idle"));
  assert.equal(idle.length, 1, `mission_idle items: ${JSON.stringify(idle.map((item) => item.title))}`);
  const text = `${idle[0].title ?? ""} ${idle[0].detail ?? ""}`;
  assert.match(text, /#1\b/, `the item names mission #1: ${text}`);
});
EOF
alerts_json
if ! BB_TREE=$tree BB_ALERTS=$WORK/alerts.json node --test "$WORK/needs.test.mjs" >"$WORK/needs.out" 2>&1; then
    check_failed "(3) web/src/home.ts needsYou over /api/alerts: $(grep -E 'not ok|AssertionError|Error|expected|actual' "$WORK/needs.out" | head -n 8 | tr '\n' ' ' | head -c 700)"
fi
note "(3) checked: Needs you shows the idle mission"

# --- (4) work again: the alert clears ---------------------------------------------------

a4=$(assign 4 1)
cleared_one() { [ "$(idle_of 1 | jq -c '[.[] | .state]')" = '["cleared"]' ]; }
wait_until 15 cleared_one || true
got=$(idle_of 1)
expect_eq "$(printf '%s' "$got" | jq -c '[.[] | [.state, .cleared_evidence.event_id]]')" "[[\"cleared\",$a4]]" \
    "(4) mission 1's mission_idle alert 15 s after task 4 was assigned to it (record $a4): $(summary "$got")"
note "(4) checked: work again clears it"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "058: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "058: an active mission with no task in work past the bound is one alert and one Needs you item naming it, none for a mission in work or a closed one, cleared by its next task"
exit 0
