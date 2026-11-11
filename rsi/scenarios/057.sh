#!/bin/sh
# Black box: a stuck alert means someone must act. An agent that waits
# because its part is done (its task reported, waiting for the gate or a
# review) and a supervisor with nothing to dispatch are not stuck, whatever
# the judge scores; a worker stuck in its own work is, and its alert names
# its task and the one command that acts on it.
#
# The problem, measured on the live fleet over the 48 h to 2026-11-11 13:05Z
# (sync/backlog.md S13): 19 `agent_stuck` alerts; 15 fired on workers whose
# task had just landed (within seconds of `landing -> landed`) and cleared
# 2.6-12.3 s later on the worker's stop: 15 of the 21 landings raised one.
# The supervisor was "stuck (0.57): WAITING" in Needs you for 3 h while no
# mission had work (lead-mac's screenshot sync/home-2026-11-11-2155.png).
# At most 3 of the 19 were about an agent that needed anyone. The alert
# step fires on the judge's `stuck` score alone (crates/spawn-cli/src/
# alerts.rs, `observe`), and the page can only drop one whose session ended
# or whose score is stale (web/src/home.ts, alertMoot / alertStale).
#
# Required (read through `GET /api/alerts` and web/src/home.ts's needsYou
# under node over it); a fake model judge scores the sessions it is told to
# as stuck 0.9, WAITING:
#   (1) the supervisor with nothing to dispatch (no question to the owner,
#       no escalation, no objective waiting to be assigned: the tasks in
#       work are the workers'), judged stuck after a turn: no
#       `agent_stuck` alert for it.
#   (2) a worker whose task is reported (waiting for its gate), judged stuck
#       after a turn: no `agent_stuck` alert for it.
#   (1) and (2) are read once worker 2's alert of (3) has fired: the alert
#   step reads every session's latest judgement on each pass, and worker
#   2's is recorded after theirs, so that pass read them (the barrier).
#   (3) a worker whose task is delivered (its own work), judged stuck after a
#       turn: one open `agent_stuck` alert; its evidence names `task` (the
#       task id) and `command` (the one command that acts on it: `spawn task
#       restart --id <task>`), and Needs you's item for it carries that
#       command.
#   (4) that worker's task is then reported: the alert clears, its
#       cleared_evidence `event_id` the `task.reported` record.
#
# A real `spawn loop` runs with the supervisor on (its alert step reads
# every 2 s), `spawn serve` answers the API, a fake engine ends a turn on
# every prompt, and a fake model judge answers the Observer. Tasks are
# assigned and reported through spawn_control_record(), as the integrator
# and the loop write them; a turn is provoked by `spawn submit` of a line.
# Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "057: harness: $*" >&2
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
for tool in psql tmux git jq od bash sed grep awk curl node python3; do
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
SPAWN_DB_NAME="bb_057_runs_$run"
SPAWN_TMUX_SOCKET="bb-057-$run"
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
judge_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid" "$judge_pid"; do
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
            echo "057: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/judge.log" ]; then
            echo "057: the judge's answers (ms, session, stuck):" >&2
            tail -n 12 "$WORK/judge.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "057: the tasks (id, state, session):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, state, session_id from mission_tasks order by id" >&2 2>/dev/null || true
            echo "057: the stuck alerts (key, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, state, fired_at_ms, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'agent_stuck' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "057: harness: unexpected exit $rc" >&2
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

# The fake engine: what is typed in opens a turn, which ends after a second
# of quiet; it writes its transcript as Claude Code does (a user line, then
# the turn's end), so the Observer judges the session after each turn.
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=${2-}; shift ;; esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" >>"$transcript"
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    [ "$pending" = 1 ] || line '"type":"user","message":{"role":"user","content":"prompt"}'
    pending=1
  elif [ "$pending" = 1 ]; then
    pending=0
    line '"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"waiting"}],"stop_reason":"end_turn"}'
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
  fi
done
ENGINE
chmod 755 "$WORK/claude"
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

# The fake model judge (crates/spawn-jev's wire shape), in a directory of
# its own: WAITING for everyone, stuck 0.9 for the sessions named in
# $WORK/stuck-sessions, 0.05 for the others.
mkdir -p "$WORK/judge"
cat >"$WORK/judge/judge.py" <<'PY'
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

work = sys.argv[1]


class Judge(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        size = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(size)
        try:
            session = json.loads(body).get("state", {}).get("session_id", "")
        except ValueError:
            session = ""
        try:
            with open(os.path.join(work, "stuck-sessions")) as names:
                stuck_sessions = names.read().split()
        except OSError:
            stuck_sessions = []
        stuck = 0.9 if session in stuck_sessions else 0.05
        answer = json.dumps({"answers": {
            "semantic_state": {"choice": "WAITING", "confidence": 0.9},
            "needs_human": {"noul": 0.3},
            "stuck": {"noul": stuck},
        }}).encode()
        with open(os.path.join(work, "judge.log"), "a") as log:
            log.write("%d %s %s\n" % (int(time.time() * 1000), session, stuck))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(answer)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(answer)

    def log_message(self, *_args):
        pass


server = HTTPServer(("127.0.0.1", 0), Judge)
with open(os.path.join(work, "judge.port.partial"), "w") as port:
    port.write(str(server.server_address[1]))
os.rename(os.path.join(work, "judge.port.partial"), os.path.join(work, "judge.port"))
server.serve_forever()
PY
: >"$WORK/stuck-sessions"
: >"$WORK/judge.log"
python3 -I "$WORK/judge/judge.py" "$WORK" >"$WORK/judge.out" 2>&1 &
judge_pid=$!
i=0
while [ ! -s "$WORK/judge.port" ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || harness "the fake judge did not start: $(head -c 300 "$WORK/judge.out")"
    sleep 0.1
done
judge_port=$(cat "$WORK/judge.port")

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_SERVE_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
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
SPAWN_JUDGE_URL=http://127.0.0.1:$judge_port
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# No stall is under test: the judge's score alone.
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
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_SUPERVISOR \
    SPAWN_JUDGE_URL SPAWN_LOOP_INTERVAL_MS SPAWN_ALERT_CHECK_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS \
    SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
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
note() { echo "057: $*" >&2; }
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
C=$(printf 'bb-057-candidate' | sha1sum | cut -c1-40)
assign() { # <task>
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":1,\"objective\":$1,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$1\",\"session_id\":\"session_worker_objective_$1\",\"scenario\":\"$WORK/ctl/scenarios/objective-$1\",\"brief\":\"Objective $1 of 057\"}" >/dev/null
}
reported() { record "{\"type\":\"task.reported\",\"id\":$1,\"commit\":\"$C\"}"; }
task_state() { q "select state from mission_tasks where id = $1"; }
delivered() { [ "$(task_state "$1")" = delivered ]; }
# Judged <score> after <ms>: the judge answered for <session> since then.
judged_since() { # <session> <score> <ms>
    awk -v s="$1" -v v="$2" -v t="$3" '$2 == s && $3 == v && $1 >= t { found = 1 } END { exit !found }' "$WORK/judge.log"
}
now_ms() { date +%s%3N; }
poke() { # <agent> <session> <tag>: a line typed into the session, so a turn runs
    capture "$SPAWN_BIN" submit --principal bb-harness --agent "$1" --session "$2" \
        --key "bb-057-$run-$3" --action "{\"type\":\"input\",\"text\":\"BB:NOOP $3\"}"
    [ "$status" -eq 0 ] || harness "submit of a line to $2 failed: $(err)"
}
# stuck_judged <agent> <session> <tag>: the session is judged stuck after a
# turn of its own, then three alert passes go by.
stuck_judged() {
    echo "$2" >>"$WORK/stuck-sessions"
    from=$(now_ms)
    poke "$1" "$2" "$3"
    wait_until 60 judged_since "$2" 0.9 "$from" ||
        harness "the fake judge was not asked about $2 within 60 s of a turn (judge log: $(tail -n 5 "$WORK/judge.log" | tr '\n' ' '))"
    wait_until 30 observed_stuck "$2" "$from" ||
        harness "no judgement of $2 with stuck >= 0.5 was recorded within 30 s of the judge's answer"
}
# observed_stuck <session> <ms>: the Observer recorded a judgement of the
# session with stuck >= 0.5 since then (the alert step reads the latest).
observed_stuck() {
    [ "$(q "select count(*) from spawn_events where event_name = 'observation.recorded'
              and session_id = '$1' and recorded_at_ms >= $2
              and (kind -> 'detail' -> 'observation' ->> 'stuck')::float8 >= 0.5")" -gt 0 ]
}

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
stuck_of() { # <session>: its agent_stuck alerts in /api/alerts, any state
    alerts_json
    jq -c --arg s "$1" '[.[] | select(.kind == "agent_stuck" and .subject == $s)]' "$WORK/alerts.json"
}
summary() { printf '%s' "$1" | jq -c '[.[] | {key, state, text}]' | head -c 400; }

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve

# --- the world's agents: the supervisor, worker 1 (its task reported) and
# worker 2 (its task delivered) -----------------------------------------------------------

wait_until 60 judged_since session_supervisor 0.05 0 ||
    harness "the supervisor was not judged within 60 s of the loop's start (loop: $(tail -n 3 "$WORK/loop.log" | tr '\n' ' '))"
record '{"type":"mission.added","id":1,"text":"057 objectives","origin":"human","sources":[]}' >/dev/null
record "{\"type\":\"mission.configured\",\"id\":1,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\"}" >/dev/null
for task in 1 2; do
    assign "$task"
    wait_until 60 delivered "$task" || harness "task #$task was not delivered within 60 s (it is $(task_state "$task"))"
    wait_until 60 judged_since "session_worker_objective_$task" 0.05 0 ||
        harness "worker $task was not judged after its brief within 60 s"
done
reported 1 >/dev/null

# The supervisor and worker 1 are judged stuck after a turn each, and the
# judgements are recorded; then worker 2. Worker 2's alert is the barrier:
# the alert step reads every session's latest judgement on each pass, so
# the pass that fires on worker 2's (recorded after the other two) read
# theirs too. Without it, (1) and (2) are not judged.
stuck_judged agent_supervisor session_supervisor one
stuck_judged agent_worker_objective_1 session_worker_objective_1 two
stuck_judged agent_worker_objective_2 session_worker_objective_2 three
open_two() { [ "$(stuck_of session_worker_objective_2 | jq -c '[.[] | .state]')" = '["open"]' ]; }
if wait_until 30 open_two; then
    # --- (1) the supervisor, with nothing to dispatch ----------------------------------
    got=$(stuck_of session_supervisor)
    expect_eq "$(printf '%s' "$got" | jq length)" 0 \
        "(1) agent_stuck alerts for the supervisor, judged stuck (0.9, WAITING) with nothing to dispatch (no question, no escalation, no objective waiting to be assigned; the tasks in work are the workers'): $(summary "$got")"
    note "(1) checked: an idle supervisor with nothing to dispatch"
    # --- (2) a worker whose task is reported ------------------------------------------
    got=$(stuck_of session_worker_objective_1)
    expect_eq "$(printf '%s' "$got" | jq length)" 0 \
        "(2) agent_stuck alerts for worker 1, judged stuck (0.9, WAITING) while its task is reported (waiting for its gate): $(summary "$got")"
    note "(2) checked: a worker whose task is reported"
else
    check_failed "(3) worker 2, judged stuck (0.9) while its task is delivered, has no open agent_stuck alert 30 s after the judgement was recorded: $(summary "$(stuck_of session_worker_objective_2)"); (1) and (2) are not judged without it"
fi

# --- (3) a worker stuck in its own work -------------------------------------------------

got=$(stuck_of session_worker_objective_2)
expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .evidence.task]')" '[2]' "(3) the alert's evidence.task"
expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .evidence.command]')" '["spawn task restart --id 2"]' \
    "(3) the alert's evidence.command (the one command that acts on it)"
cat >"$WORK/needs.test.mjs" <<'EOF'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { needsYou } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const alerts = JSON.parse(readFileSync(env.BB_ALERTS, "utf8"));
const items = needsYou({ attention: null, missions: null, alerts, agents: [], health: null, missingAlwaysOn: 0, now: Date.now() });
test("the stuck worker's item carries its command; nothing for the others", () => {
  const stuck = items.filter((item) => String(item.key).startsWith("alert:agent_stuck"));
  assert.deepEqual(stuck.map((item) => item.command), ["spawn task restart --id 2"],
    `agent_stuck items: ${JSON.stringify(stuck.map((item) => [item.key, item.command]))}`);
});
EOF
if ! BB_TREE=$tree BB_ALERTS=$WORK/alerts.json node --test "$WORK/needs.test.mjs" >"$WORK/needs.out" 2>&1; then
    check_failed "(3) web/src/home.ts needsYou over /api/alerts: $(grep -E 'not ok|AssertionError|Error|expected|actual' "$WORK/needs.out" | head -n 8 | tr '\n' ' ' | head -c 700)"
fi
note "(3) checked: a worker stuck in its own work, its task and its command"

# --- (4) its task reported: the alert clears ---------------------------------------------

r=$(reported 2)
cleared() { [ "$(stuck_of session_worker_objective_2 | jq -c '[.[] | .state]')" = '["cleared"]' ]; }
wait_until 15 cleared || true
got=$(stuck_of session_worker_objective_2)
expect_eq "$(printf '%s' "$got" | jq -c '[.[] | [.state, .cleared_evidence.event_id]]')" "[[\"cleared\",$r]]" \
    "(4) worker 2's agent_stuck alert 15 s after its task was reported (record $r): $(summary "$got")"
note "(4) checked: the alert clears once the task is reported"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "057: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "057: no stuck alert for an agent waiting because its part is done or a supervisor with nothing to dispatch; a worker stuck in its own work raises one naming its task and command, cleared once it reports"
exit 0
