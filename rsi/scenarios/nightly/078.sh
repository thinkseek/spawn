#!/bin/sh
# Black box: Home shows the supervisor's newest For-the-owner block as what
# is current, each line with what became of the tasks it names. A line said
# only in an earlier turn is no longer current, and so a repeat no longer
# crowds the card.
#
# The problem, as the owner reads Home after 070 went live (lead-mac,
# sync/queue.md 09:50; sync/home-2026-11-12-0945.png). Home's "From the
# supervisor · 6 more" lists the same ask again and again. The live
# `/api/attention` `supervisor.for_owner` holds 9 lines: 5 of them are the
# supervisor's "My engine needs `SPAWN_WORKSPACE` set, or a restart, so I
# can answer messages #1709, …" said at 00:51, 00:52, 04:02, 04:48 and
# 08:57, each with a longer list of messages. Beside them are asks that the
# supervisor's newest block (08:57) no longer makes: the 00:51 "If the RSI
# resident stays idle on task 271, restart it or reassign the task", and
# the 04:02 note about worker 272's model. Also a line of the newest block,
# "Scenario `070` exits 2 on the base (look at its log in the gate output
# for task 280). Deploying the controller at or after cafc8ec …
# clears it", which Home shows as current at 09:45 when task 280 had landed
# and 04dba790 was live. Nothing on the card says so: Home's rows carry the
# line and its age, not what became of the task it names. 064's `settled`
# hides only a review ask whose tasks have all ended, on purpose, since
# hiding a live ask is the worse failure.
#
# Required (read through `GET /api/attention` and web/src/home.ts under
# node over it):
#   (1) every `for_owner` item carries `current`: true for the items of the
#       supervisor's newest turn that has a For-the-owner line (inline or a
#       heading's list items), false for the items of every earlier turn. A
#       later turn without a For-the-owner line changes nothing.
#   (2) latestNotes (Home's card) shows only current lines that are not
#       settled; `more` counts the other current ones; `earlier` counts the
#       lines of earlier turns that are not settled, which are neither
#       shown nor in `more`.
#   (3) each row latestNotes shows carries `taskStates`, what became of
#       the tasks it names, worded as allNotes words its `tasks` ("T3
#       accepted"; "" when it names none).
#   (4) allNotes (the full list) still lists every line, each with
#       `current`.
#
# A real `spawn loop` runs with the supervisor on, `spawn serve` answers, a
# fake engine writes each turn's words to its transcript as Claude Code
# does, and a fake model judge answers the Observer. One review mission's
# tasks 2 to 5: 2 and 3 accepted and 4 cancelled by the owner (`spawn task
# accept`, `spawn task cancel`), 5 reported and waiting for review. Then the
# supervisor writes three turns, oldest first:
#   one: a heading over three items: "My engine needs SPAWN_WORKSPACE set,
#     or a restart, so I can answer messages #11 and #12 by id."; "If task 5
#     stays idle, restart it or reassign it."; "The relay names another
#     model for worker 5.";
#   two: a heading over two items: "My engine needs SPAWN_WORKSPACE set, or
#     a restart, so I can answer messages #11, #12 and #13 by id."; "Scenario
#     bb.sh exits 2 on the base (see the gate output for task 3). Deploying
#     the controller clears it.";
#   three: "Next: check the gate logs." (no For-the-owner line).
# Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "078: harness: $*" >&2
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
for tool in psql git jq od bash sed grep awk curl node python3; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_078_runs_$run"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "078"
# The tmux server's socket lives in WORK, so the cleanup removes nothing
# outside it (cc-rules (a), 2026-11-12).
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
judge_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid" "$judge_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
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
            echo "078: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/judge.log" ]; then
            echo "078: the judge's answers (ms, session):" >&2
            tail -n 12 "$WORK/judge.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "078: the tasks (id, state, session):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, state, session_id from mission_tasks order by id" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/attention.json" ]; then
            echo "078: the last /api/attention supervisor:" >&2
            jq -c '.supervisor | {label, for_owner}' "$WORK/attention.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "078: harness: unexpected exit $rc" >&2
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
# the assistant's words and the turn's end). A typed line with "BB:SAY "
# makes the turn's words what follows it, up to the message's own trailer
# ("[spawn message #<n>] (Answer this message ...)"): the supervisor's reply.
# Any other turn says "waiting". A read that times out mid-line keeps the
# part it read, so a line split by a busy host is not cut (sync/flake-070.md):
# bb_read_line, which bb_engine writes into the engine (assertions.sh).
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=${2-}; shift;; esac
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
words=waiting
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    case "$typed" in *"BB:SAY "*) words=${typed#*BB:SAY }; words=${words%% \[spawn message*};; esac
    [ "$pending" = 1 ] || line '"type":"user","message":{"role":"user","content":"prompt"}'
    pending=1
  elif [ "$pending" = 1 ]; then
    pending=0
    line '"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"'"$words"'"}],"stop_reason":"end_turn"}'
    line '"type":"system","subtype":"turn_duration","durationMs":1'
    words=waiting
    printf '\nidle\n❯ '
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# The fake model judge (crates/spawn-jev's wire shape), in a directory of
# its own: WAITING, stuck 0.05, for everyone (no judgement is under test).
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
        stuck = 0.05
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
# A scratch HOME: nothing here reads or writes the real one.
HOME=$WORK/home
mkdir -p "$HOME"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home";; esac
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
SPAWN_WORKER=worker-bb
SPAWN_SUPERVISOR=1
SPAWN_JUDGE_URL=http://127.0.0.1:$judge_port
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_ALERT_CHECK_SECS=2
# No stall is under test.
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
export HOME SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS SPAWN_WORKER SPAWN_SUPERVISOR \
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
note() { echo "078: $*" >&2; }
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
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")";; esac
    echo "$id"
}
C=$(printf 'bb-078-candidate' | sha1sum | cut -c1-40)
assign() { # <task>
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":1,\"kind\":\"ephemeral\",\"created_by\":\"human\",\"agent_id\":\"agent_worker_task_$1\",\"session_id\":\"session_worker_task_$1\",\"brief\":\"Task $1 of 078\"}" >/dev/null
}
reported() { record "{\"type\":\"task.reported\",\"id\":$1,\"commit\":\"$C\",\"report\":\"Task $1 is done.\"}" >/dev/null; }
task_state() { q "select state from mission_tasks where id = $1"; }
delivered() { [ "$(task_state "$1")" = delivered ]; }
judged() { # <session>: the judge answered for it at least once
    awk -v s="$1" '$2 == s { found = 1 } END { exit !found }' "$WORK/judge.log"
}
say() { # <tag> <words>: the supervisor's next turn writes <words>
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor --session session_supervisor \
        --key "bb-078-$run-$1" --action "{\"type\":\"input\",\"text\":\"BB:SAY $2\"}"
    [ "$status" -eq 0 ] || harness "submit of a line to the supervisor failed: $(err)"
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
attention_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/attention.json" -w '%{http_code}' "$base/api/attention" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/attention answered $code: $(head -c 300 "$WORK/attention.json" 2>/dev/null)"
}

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve

# --- the world: the supervisor, and a review mission's tasks 2 to 5 ------------------------

wait_until 60 judged session_supervisor ||
    harness "the supervisor was not judged within 60 s of the loop's start (loop: $(tail -n 3 "$WORK/loop.log" | tr '\n' ' '))"
record '{"type":"mission.added","id":1,"text":"078 owner work","origin":"human","sources":[]}' >/dev/null
record '{"type":"mission.configured","id":1,"accept":"review"}' >/dev/null
for task in 2 3 4 5; do
    assign "$task"
done
for task in 2 3 4 5; do
    wait_until 60 delivered "$task" || harness "task #$task was not delivered within 60 s (it is $(task_state "$task"))"
done
for task in 2 3 5; do
    reported "$task"
done
for task in 2 3; do
    capture "$SPAWN_BIN" task accept --id "$task"
    [ "$status" -eq 0 ] || harness "spawn task accept --id $task failed: $(err)"
done
capture "$SPAWN_BIN" task cancel --id 4
[ "$status" -eq 0 ] || harness "spawn task cancel --id 4 failed: $(err)"
states=$(q "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$states" = "2:accepted 3:accepted 4:cancelled 5:reported" ] || harness "the tasks are not as set up: [$states]"

# The supervisor's three turns, oldest first; each is in the transcript
# before the next is typed. A turn's words are one typed line: each "\n" in
# it is a line break in the words the transcript holds.
said() { grep -qs "$1" "$SPAWN_CLAUDE_PROJECTS"/*/*.jsonl; }
turn() { # <tag> <words> <pattern>
    say "$1" "$2"
    wait_until 60 said "$3" || {
        supervisor_turn_dump "$3"
        harness "the supervisor's turn ($3) was not in a transcript within 60 s"
    }
}
ws1='My engine needs SPAWN_WORKSPACE set, or a restart, so I can answer messages #11 and #12 by id.'
ws2='My engine needs SPAWN_WORKSPACE set, or a restart, so I can answer messages #11, #12 and #13 by id.'
idle5='If task 5 stays idle, restart it or reassign it.'
relay='The relay names another model for worker 5.'
gate3='Scenario bb.sh exits 2 on the base (see the gate output for task 3). Deploying the controller clears it.'
turn one "For the owner:\\\\n- $ws1\\\\n- $idle5\\\\n- $relay" 'another model for worker 5'
turn two "For the owner:\\\\n- $ws2\\\\n- $gate3" 'Deploying the controller clears it'
turn three 'Next: check the gate logs.' 'check the gate logs'
# The answer holds both blocks' five items once the newest block is read.
five_items() {
    attention_json
    [ "$(jq '[.supervisor.for_owner[]? | select(.text | test("Deploying the controller"))] | length' "$WORK/attention.json")" = 1 ] &&
        [ "$(jq '.supervisor.for_owner | length' "$WORK/attention.json")" = 5 ]
}
wait_until 60 five_items ||
    harness "/api/attention did not list the two blocks' five items within 60 s: $(jq -c '[.supervisor.for_owner[]? | .text]' "$WORK/attention.json" 2>/dev/null | head -c 900)"
[ "$(jq -c '[.supervisor.for_owner[].at_ms] | unique | length' "$WORK/attention.json")" = 2 ] ||
    harness "the five items do not carry two turns' times: $(jq -c '[.supervisor.for_owner[] | {at_ms, text}]' "$WORK/attention.json" | head -c 600)"

item() { # <text> <fields>: the for_owner items whose text is <text>
    jq -c --arg t "$1" "[.supervisor.for_owner[] | select(.text == \$t) | $2]" "$WORK/attention.json"
}

# --- (1) the newest block is current -----------------------------------------------------

for text in "$ws2" "$gate3"; do
    expect_eq "$(item "$text" '.current')" '[true]' "(1) [$text], of the newest turn with a For-the-owner block: current"
done
for text in "$ws1" "$idle5" "$relay"; do
    expect_eq "$(item "$text" '.current')" '[false]' "(1) [$text], said only in an earlier turn: not current"
done
expect_eq "$(item "$gate3" '{tasks, settled}')" '[{"tasks":[{"id":3,"state":"accepted"}],"settled":false}]' \
    "(1) the newest block's line naming task 3 keeps its tasks and is not settled (no review ask, as 064)"
note "(1) checked: current"

# --- (2), (3), (4) Home's card and the full list ---------------------------------------------

cat >"$WORK/notes.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { latestNotes, allNotes } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/attention.json`, "utf8"));
const now = Math.max(...answer.supervisor.for_owner.map((note) => note.at_ms ?? 0)) + 60_000;
const ws2 = env.BB_WS2;
const gate3 = env.BB_GATE3;

test("(2) Home's card shows the newest block alone; the earlier lines are counted as earlier", () => {
  const notes = latestNotes(answer, now);
  assert.deepEqual(notes.shown.map((note) => note.text).sort(), [gate3, ws2].sort(), "shown: the newest block's two lines");
  assert.equal(notes.more, 0, "more: no other current line");
  assert.equal(notes.earlier, 3, "earlier: the three lines said only in the earlier turn");
});

test("(3) each shown line says what became of the tasks it names", () => {
  const shown = latestNotes(answer, now).shown;
  const by = (text) => shown.find((note) => note.text === text);
  assert.equal(by(gate3)?.taskStates, "T3 accepted", "the line naming task 3");
  assert.equal(by(ws2)?.taskStates, "", "the line naming no task");
});

test("(4) the full list keeps every line, each saying whether it is current", () => {
  const rows = allNotes(answer);
  assert.equal(rows.length, 5, "every line");
  const current = rows.filter((row) => row.current === true).map((row) => row.text).sort();
  assert.deepEqual(current, [gate3, ws2].sort(), "the current rows");
  assert.equal(rows.filter((row) => row.current === false).length, 3, "the earlier rows, marked not current");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_WS2=$ws2 BB_GATE3=$gate3 node --test "$WORK/notes.test.mjs" >"$WORK/notes.out" 2>&1; then
    check_failed "(2)-(4) web/src/home.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/notes.out" | head -n 12 | tr '\n' ' ' | head -c 900)"
fi
note "(2), (3), (4) checked: Home's card and the full list"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "078: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "078: Home shows the supervisor's newest For-the-owner block as current, each line with its tasks' states; earlier lines are counted, not shown"
exit 0
