#!/bin/sh
# Black box: the cost page says what a landing costs.
#
# The problem, as the owner reads the pages after 73e3596d (lead-mac,
# sync/queue.md 23:45): no page says dollars per landing. The Scorecard
# says "26,683,516 tokens per landing" (tokens, not dollars); the Cost page
# lists dollars per task, mission and agent; "$ per landing, 24 h and 7 d"
# is the number an owner steers the fleet by, and the owner has to divide
# two pages by hand to get it (live 7 d: $1,778.71 priced over 209
# landings, about $8.51, and 38% of the tokens unpriced besides).
#
# Required (read through `GET /api/cost?window=24h` and web/src/cost.ts's
# costView under node over it):
#   (1) the answer carries `landed`, the tasks that landed or were accepted
#       in the window, and `usd_per_landing`, the window's priced dollars
#       over them (null with none).
#   (2) every `missions` row carries its own `landed` and `usd_per_landing`.
#   (3) costView(answer).per_landing reads "<dollars> per landing (<n>
#       landed)", and "no landing in the window" with none.
#
# A real `spawn loop` (the Bridge records each model call's usage and cost)
# runs ephemeral workers of two missions on 048's fake engine (a plan per
# worker says what usage its one turn writes); `spawn serve` answers on a
# loopback port; the owner accepts with `spawn task accept` and cancels
# with `spawn task cancel`. Prices (USD per million tokens: input, output,
# cache write, cache read): bb-model-x 3, 15, 3.75, 0.30.
#   mission A: c1: x 100000 in, 10000 out ($0.45), accepted;
#              c3: x 20000 in ($0.06), cancelled.
#   mission B: c2: x 50000 in ($0.15), accepted.
# So 24 h: $0.66 over 2 landings, $0.33; A $0.51 over 1, B $0.15 over 1.
# Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.

set -eu


asserted=""
harness() {
    echo "060: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_060_landing_$run"
SPAWN_TMUX_SOCKET="bb-060-$run"
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
stop_loop() {
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
        loop_pid=""
    fi
}
cleanup() {
    stop_loop
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
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
            echo "060: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/serve.log" ]; then
            echo "060: serve's last lines:" >&2
            tail -n 5 "$WORK/serve.log" >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "060: the engines' turns (time, agent, execution, plan):" >&2
            cat "$WORK/turns.log" >&2
        fi
        if [ -s "$WORK/cost-7d.json" ]; then
            echo "060: the last 7 d answer (first 1500 bytes):" >&2
            head -c 1500 "$WORK/cost-7d.json" >&2
            echo >&2
        fi
        if [ -n "$database_made" ]; then
            echo "060: the tasks (id, mission, state, agent):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, mission_id, state, agent_id
                from mission_tasks order by id" >&2 2>/dev/null || true
            echo "060: the alerts (key, kind, subject, state, text):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, kind, subject, state, left(coalesce(text, ''), 120)
                from spawn_alerts where not notice order by fired_at_ms" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "060: harness: unexpected exit $rc" >&2
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

tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"

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

# The fake engine. A prompt typed in (a paste, then Enter) opens a turn
# after a second of quiet. A prompt naming a plan (`BB:PLAN:<name>`, the
# task's brief carries it) acts out $BB_WORK/ctl/<name>.plan, one message
# per line, `<model> <input> <output> <cache write> <cache read> <age s>`:
# each message is an assistant message with its own id, written as two
# transcript lines (a text block, then another), each repeating the id, the
# model and the usage, timed <age s> seconds ago, as Claude Code writes one
# line per content block. Then the turn runs a tool call, writes the report
# and ends (the task done), logged to turns.log once its lines are written.
# A prompt naming no plan (the supervisor's, a nudge) ends the turn at once
# and reports no usage.
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=${2-}; shift ;; esac
  shift
done
agent=$SPAWN_AGENT_ID
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
stamp() { date -u -d "@$(( $(date +%s) - $1 ))" +%Y-%m-%dT%H:%M:%S.000Z; }
line() { # <timestamp> <rest>
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$1" "$native" "$2" >>"$transcript"
}
message() { # <model> <in> <out> <cache write> <cache read> <age>
  id="msg_${SPAWN_EXECUTION_ID}_$count"
  at=$(stamp "$6")
  usage="\"usage\":{\"input_tokens\":$2,\"cache_creation_input_tokens\":$4,\"cache_read_input_tokens\":$5,\"output_tokens\":$3}"
  for block in first second; do
    line "$at" "\"type\":\"assistant\",\"message\":{\"id\":\"$id\",\"model\":\"$1\",\"role\":\"assistant\",\"type\":\"message\",\"content\":[{\"type\":\"text\",\"text\":\"$block block\"}],$usage}"
  done
}
turn() {
  name=$(printf '%s' "$typed_all" | sed -n 's/.*BB:PLAN:\([a-z0-9]*\).*/\1/p' | head -n 1)
  typed_all=
  plan=$BB_WORK/ctl/$name.plan
  if [ -z "$name" ] || [ ! -f "$plan" ]; then
    line "$(stamp 0)" '"type":"system","subtype":"turn_duration","durationMs":1'
    printf '\nidle\n❯ '
    return
  fi
  while read -r model in out cw cr age; do
    [ -n "$model" ] || continue
    message "$model" "$in" "$out" "$cw" "$cr" "$age"
  done <"$plan"
  id="toolu_${SPAWN_EXECUTION_ID}_$count"
  line "$(stamp 0)" "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  printf 'done\n' >result.txt
  sleep 0.3
  line "$(stamp 0)" "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
  mkdir -p .spawn
  printf '## Outcome\ndone\n## What changed\nresult.txt\n## Open issues\nnone\n' >.spawn/report.md
  line "$(stamp 0)" '"type":"system","subtype":"turn_duration","durationMs":1'
  printf '%s %s %s %s\n' "$(date +%s%3N)" "$agent" "$SPAWN_EXECUTION_ID" "$name" >>"$BB_WORK/turns.log"
  printf '\nidle\n❯ '
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
typed_all=
while :; do
  if IFS= read -r -t 1 typed; then
    printf 'you: %s\n' "$typed"
    typed_all="$typed_all $typed"
    if [ "$pending" = 0 ]; then
      line "$(stamp 0)" '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
  elif [ "$pending" = 1 ]; then
    pending=0
    turn
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
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# US dollars per million tokens. bb-model-z is not priced.
PRICES='{"bb-model-x":{"input":3,"output":15,"cache_write":3.75,"cache_read":0.3},"bb-model-y":{"input":1,"output":5,"cache_write":1.25,"cache_read":0.1}}'
SPAWN_MODEL_PRICES=$PRICES
# The alerts step reads its conditions every 2 s (default 30 s), so 10 s
# with no alert means several readings found none.
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
# Free disk is not what this scenario is about: no warning, no floor.
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_LOOP_INTERVAL_MS SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_MODEL_PRICES SPAWN_ALERT_CHECK_SECS \
    SPAWN_MISSION_FILE SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/turns.log"

# The test server is shared: another run's init-db can collide on cluster-wide
# objects ("tuple concurrently updated"). init-db is idempotent.
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
day=$(date -u +%F)
same_day() {
    [ "$(date -u +%F)" = "$day" ] || harness "the UTC day changed during the run ($day to $(date -u +%F)); run it again"
}

start_loop() {
    "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}

# wait_until <seconds> <command...>: true once the command is, false past
# the limit.
wait_until() {
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

reported() { [ "$(q "select state from mission_tasks where id = $1")" = reported ]; }

# assign <label> <mission> <plan lines>: an ephemeral worker whose one turn
# reports that usage; sets task_<label>, agent_<label>.
assign() {
    printf '%s\n' "$3" >"$WORK/ctl/$1.plan"
    capture "$SPAWN_BIN" task assign --mission "$2" --kind ephemeral --worker "$1" \
        --brief "BB:PLAN:$1 Do the 060 task and report."
    [ "$status" -eq 0 ] || harness "task assign ($1) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $2")
    agent=$(q "select agent_id from mission_tasks where id = $task")
    [ -n "$task" ] && [ -n "$agent" ] || harness "task assign ($1) made no task with an agent"
    eval "task_$1=\$task agent_$1=\$agent"
}

# The page's API: GET <path> into <file>; its status code.
get() { curl -s --noproxy '*' -o "$2" -w '%{http_code}' "$base$1" 2>/dev/null || echo 000; }
# cost <window> <check>: the answer for the window in $WORK/cost-<window>.json;
# an answer other than 200 with JSON is the behaviour's failure.
cost() {
    code=$(get "/api/cost?window=$1" "$WORK/cost-$1.json")
    [ "$code" = 200 ] || fail "$2 GET /api/cost?window=$1 answered $code: $(head -c 300 "$WORK/cost-$1.json" 2>/dev/null)"
    jq -e 'type == "object"' "$WORK/cost-$1.json" >/dev/null 2>&1 ||
        fail "$2 GET /api/cost?window=$1 answered no JSON object: $(head -c 300 "$WORK/cost-$1.json")"
}
# j <window> <jq filter>: a value of the last answer for the window.
j() { jq -r "$2" "$WORK/cost-$1.json"; }
# near <a> <b>: the two amounts differ by at most $0.000001.
near() { awk -v a="$1" -v b="$2" 'BEGIN { d = a - b; if (a == "" || a == "null") exit 1; exit !(d <= 0.000001 && d >= -0.000001) }'; }
assert_usd() { # <actual> <expected> <what>
    near "$1" "$2" || fail "$3: \$$1, expected \$$2"
}
# The usd of a list's entry whose <key> is <value>, in the window's answer.
usd_of() { j "$1" "[.$2[] | select((.$3 | tostring) == \"$4\") | .usd] | if length == 1 then .[0] else \"none\" end"; }
# A task's usd as known to the answer, without failing (for waiting).
task_usd() {
    code=$(get "/api/cost?window=7d" "$WORK/wait.json")
    [ "$code" = 200 ] || return 1
    jq -r "[.tasks[]? | select((.task | tostring) == \"$1\") | .usd][0] // \"none\"" "$WORK/wait.json" 2>/dev/null
}
costs() { near "$(task_usd "$1")" "$2"; }
alerts_for() { q "select count(*) from spawn_alerts where kind = 'cost_budget' and subject = 'mission #$1'"; }

# --- the missions, the loop, the page's API ------------------------------------

for name in A B; do
    capture "$SPAWN_BIN" mission add --text "060 cost mission $name"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    id=$(jq -r .mission "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "mission add printed no id: $(out)"
    eval "mission_$name=\$id"
done

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
start_serve() {
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/serve.log" 2>&1 &
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
stop_serve() {
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
start_loop
start_serve
code=$(get /api/missions "$WORK/missions.json")
[ "$code" = 200 ] || harness "GET /api/missions answered $code: $(head -c 300 "$WORK/missions.json")"


note() { echo "060: $*" >&2; }

# --- the use: c1 and c2 land (accepted), c3 does not ----------------------------------

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

assign c1 "$mission_A" "bb-model-x 100000 10000 0 0 0"
assign c2 "$mission_B" "bb-model-x 50000 0 0 0 0"
assign c3 "$mission_A" "bb-model-x 20000 0 0 0 0"
for label in c1 c2 c3; do
    eval "task=\$task_$label"
    wait_until 150 reported "$task" ||
        harness "the task of worker $label was not reported within 150 s ($(q "select state from mission_tasks where id = $task"))"
done
for label in c1 c2; do
    eval "task=\$task_$label"
    capture "$SPAWN_BIN" task accept --id "$task"
    [ "$status" -eq 0 ] || harness "task accept ($label) failed: $(err)"
done
capture "$SPAWN_BIN" task cancel --id "$task_c3"
[ "$status" -eq 0 ] || harness "task cancel (c3) failed: $(err)"
# The Bridge records every call: $0.45 + $0.15 + $0.06 in the 24 h answer.
counted() {
    code=$(get "/api/cost?window=24h" "$WORK/cost-24h.json")
    [ "$code" = 200 ] && [ "$(jq '.total.usd * 100 | round' "$WORK/cost-24h.json" 2>/dev/null)" = 66 ]
}
wait_until 60 counted ||
    harness "the 24 h cost answer does not total \$0.66 (c1, c2, c3) within 60 s: $(jq -c .total "$WORK/cost-24h.json" 2>/dev/null | head -c 300)"
landed_count() { q "select count(*) from mission_tasks where id in ($task_c1, $task_c2, $task_c3) and state = 'accepted'"; }
[ "$(landed_count)" = 2 ] || harness "c1 and c2 are not both accepted ($(q "select string_agg(id || ':' || state, ' ') from mission_tasks"))"

# --- (1) the window's landings and dollars per landing ------------------------------------

expect_eq "$(jq -c '{landed, per: (if .usd_per_landing == null then null else (.usd_per_landing * 1000 | round) end)}' "$WORK/cost-24h.json")" \
    '{"landed":2,"per":330}' \
    "(1) the 24 h answer's landed (tasks landed or accepted in the window) and usd_per_landing (priced dollars over them, \$0.66 / 2)"
note "(1) checked: the window's dollars per landing"

# --- (2) per mission ------------------------------------------------------------------------

expect_eq "$(jq -c --argjson a "$mission_A" --argjson b "$mission_B" '[.missions[] | select(.mission == $a or .mission == $b)
        | {mission, landed, per: (if .usd_per_landing == null then null else (.usd_per_landing * 1000 | round) end)}] | sort_by(.mission)' "$WORK/cost-24h.json")" \
    "[{\"mission\":$mission_A,\"landed\":1,\"per\":510},{\"mission\":$mission_B,\"landed\":1,\"per\":150}]" \
    "(2) the missions' landed and usd_per_landing (A: c1 landed, c3 cancelled, \$0.51 over 1; B: \$0.15 over 1)"
note "(2) checked: per mission"

# --- (3) the page's words ----------------------------------------------------------------------

cat >"$WORK/cost.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { costView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/cost.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/cost-24h.json`, "utf8"));

test("the window's line: dollars per landing and how many landed", () => {
  assert.equal(costView(answer).per_landing, "$0.33 per landing (2 landed)");
});

test("no landing: no figure, and it says so", () => {
  assert.equal(costView({ ...answer, landed: 0, usd_per_landing: null }).per_landing, "no landing in the window");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/cost.test.mjs" >"$WORK/cost.out" 2>&1; then
    check_failed "(3) web/src/cost.ts costView over the 24 h answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/cost.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(3) checked: the page's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "060: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "060: the cost answer and page say what a landing cost in the window, for the fleet and per mission"
exit 0
