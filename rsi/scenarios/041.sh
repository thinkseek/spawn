#!/bin/sh
# Black box: cost you can see. Each execution records its token use and its
# cost by model, read from the engine's own usage lines; the page's API and
# the page's own figures give the cost per mission, task and agent over the
# last 24 h and 7 d, to the cent; a mission's daily budget, once crossed,
# raises exactly one alert.
#
# Today Spawn reads the engine's usage only for the scorecard (scorecard/
# transcripts.rs): tokens per landing for the whole fleet, by model, with
# no price, nothing per execution, mission, task or agent, and no budget in
# money. The ingest leaves `turn.stopped`'s tokens unset. Required (the
# objective's prompt says each rule): an assistant message's usage (input,
# output, cache creation, cache read tokens, under its `message.model`)
# counts once per message id, however many lines repeat it, at its earliest
# line's timestamp, for the execution that ran it, the execution's agent,
# the task that agent held, and that task's mission; it is priced by
# SPAWN_MODEL_PRICES (US dollars per million tokens, per model; a model it
# does not price counts its tokens and no cost); `GET /api/cost?window=24h`
# and `?window=7d` answer the totals; web/src/cost.ts's `costView` turns an
# answer into the page's figures; `spawn mission configure --id <id> --daily-usd
# <amount>` sets a mission's daily budget, and the alerts step fires one
# `cost_budget` alert, subject `mission #<id>`, when the mission's cost of
# the UTC day reaches it.
#
# A real `spawn loop` (the Bridge, the dispatcher, the alerts step; no
# model judge, no supervisor) runs ephemeral workers of two missions on the
# fake engine below, whose plan per worker (named in the task's brief) says
# what usage its one turn reports; `spawn serve` answers on a loopback port.
# Prices (USD per million tokens: input, output, cache write, cache read):
# bb-model-x 3, 15, 3.75, 0.30; bb-model-y 1, 5, 1.25, 0.10; bb-model-z is
# not priced. Each message is written as two lines with the same id and
# usage (Claude Code writes one line per content block). The usage:
#   mission A, budget $0.50 a day:
#     a1: x 1000 in, 2000 out, 10000 cache write, 50000 cache read ($0.0855);
#         y 200000 in, 40000 out ($0.40); x 1000000 in, timed 3 days ago
#         ($3.00: in 7 d, not in 24 h, not today). $0.4855 in 24 h, $3.4855
#         in 7 d.
#     a2: x 100000 in, 10000 out ($0.45).
#     a3 (after the restart with bb-model-x's rates doubled, below): x 50000
#         in ($0.30 at the new rates, $0.15 at the old).
#   mission B, budget $1.00 a day:
#     b1: x 10000 in, 2000 out ($0.06).
#     b2: y 100 in, 10 out ($0.00015); z 5000 in, 500 out (not priced).
#     b3: z 3000 in, 300 out (not priced: it ran on no priced model).
# Checked:
#   (1) per execution: after a1, b1 and b2 report, `GET /api/cost?window=7d`
#       lists a1's execution with its four token counts and $3.4855, each
#       repeated message counted once, by model (x $3.0855, y $0.40), the
#       message timed before its task was assigned counted for that task; b2's
#       execution costs $0.00015 and lists bb-model-z with its 5500 tokens
#       and no cost (`usd` null); b3's costs $0 with its 3300 tokens unpriced;
#       the answer counts 8800 tokens unpriced.
#   (2) the budget: the daily budgets set with `spawn mission configure
#       --daily-usd` (A $0.50, B $1.00); while A has spent $0.4855 today
#       ($3.4855 in 7 d: the budget is the day's), 10 s of readings raise no
#       `cost_budget` alert; after a2 (A today $0.9355) exactly one, subject
#       `mission #A`, within 60 s, and the alerts API serves it; none for B.
#   (3) the totals, 24 h and 7 d, each within $0.000001: the total
#       ($0.99565, $3.99565), per mission (A $0.9355 and $3.9355, B
#       $0.06015), per task and per agent (a1 $0.4855 and $3.4855), per model
#       (x $0.5955 and $3.5955, y $0.40015, z unpriced), and the token
#       counts; `?window=30d` answers 400.
#   (4) the page's figures: web/src/cost.ts's `costView`, given each
#       answer, shows the total and each mission, task and agent to the
#       cent ("$1.00", "#A" "$0.94", "T<a1>" "$0.49"; 7 d "$4.00", "$3.94",
#       "$3.49"), and b2's cost, which is not zero, as "<$0.01", never
#       "$0.00"; b3's row, which ran only on an unpriced model, reads
#       "unpriced" (never "$0.00"), and the rows with unpriced use (b2, b3,
#       the total) are marked so. cost.ts and its imports must load with node
#       alone; one that does not fails (4).
#   (5) priced when recorded, once only: the loop and serve are restarted
#       with bb-model-x's rates doubled; 10 s later the totals are what they
#       were (a message keeps the price it was recorded at, and a restarted
#       loop counts no message twice); then a3 spends $0.30 in A at the new
#       rates: still exactly one `cost_budget` alert for A and none for B,
#       and the totals add a3's $0.30 once and nothing else.
#   (6) the setting is checked: `spawn loop` and `spawn serve` with
#       SPAWN_MODEL_PRICES not JSON, or pricing a model at a negative rate,
#       exit non-zero within 15 s, naming the setting.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (6ff6590e): `GET /api/cost` answers
# 404. It passes once usage is recorded per execution and priced, the
# totals are served and shown, and the budget alert fires once.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time. The UTC
# day must not change during the run (the budget is per UTC day): if it
# does, the run is the harness's failure.

set -eu


asserted=""
harness() {
    echo "041: harness: $*" >&2
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
SPAWN_DB_NAME="bb_041_cost_$run"
SPAWN_TMUX_SOCKET="bb-041-$run"
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
            echo "041: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/serve.log" ]; then
            echo "041: serve's last lines:" >&2
            tail -n 5 "$WORK/serve.log" >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "041: the engines' turns (time, agent, execution, plan):" >&2
            cat "$WORK/turns.log" >&2
        fi
        if [ -s "$WORK/cost-7d.json" ]; then
            echo "041: the last 7 d answer (first 1500 bytes):" >&2
            head -c 1500 "$WORK/cost-7d.json" >&2
            echo >&2
        fi
        if [ -n "$database_made" ]; then
            echo "041: the tasks (id, mission, state, agent):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, mission_id, state, agent_id
                from mission_tasks order by id" >&2 2>/dev/null || true
            echo "041: the alerts (key, kind, subject, state, text):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, kind, subject, state, left(coalesce(text, ''), 120)
                from spawn_alerts where not notice order by fired_at_ms" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "041: harness: unexpected exit $rc" >&2
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
        --brief "BB:PLAN:$1 Do the 041 task and report."
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
    capture "$SPAWN_BIN" mission add --text "041 cost mission $name"
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

assign a1 "$mission_A" "bb-model-x 1000 2000 10000 50000 0
bb-model-y 200000 40000 0 0 0
bb-model-x 1000000 0 0 0 259200"
assign b1 "$mission_B" "bb-model-x 10000 2000 0 0 0"
assign b2 "$mission_B" "bb-model-y 100 10 0 0 0
bb-model-z 5000 500 0 0 0"
assign b3 "$mission_B" "bb-model-z 3000 300 0 0 0"
for label in a1 b1 b2 b3; do
    eval "task=\$task_$label"
    wait_until 150 reported "$task" ||
        harness "the task of worker $label was not reported within 150 s ($(q "select state from mission_tasks where id = $task"))"
done

# --- (1) per execution ------------------------------------------------------------

cost 7d "(1)"
a1_runs=$(q "select string_agg(execution_id, ' ' order by started_at_ms) from agent_executions where agent_id = '$agent_a1'")
[ -n "$a1_runs" ] || harness "worker a1 ran no execution"
wait_until 30 costs "$task_a1" 3.4855 ||
    fail "(1) a1's task does not cost \$3.4855 over 7 d within 30 s of its report: $(task_usd "$task_a1")"
cost 7d "(1)"
a1_run=$(j 7d "[.executions[]? | select(.agent == \"$agent_a1\")] | if length == 1 then .[0].execution else \"\" end")
[ -n "$a1_run" ] || fail "(1) the 7 d answer does not list a1's one execution ($a1_runs): $(j 7d '[.executions[]? | {execution, agent}]' | tr -d '\n ')"
case " $a1_runs " in *" $a1_run "*) ;; *) fail "(1) the execution listed for a1 ($a1_run) is not one of its own ($a1_runs)" ;; esac
assert_eq "$(j 7d "[.executions[] | select(.execution == \"$a1_run\") | .task][0] | tostring")" "$task_a1" "(1) the task of a1's execution"
assert_eq "$(j 7d "[.executions[] | select(.execution == \"$a1_run\") | .tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(\" \")][0]")" \
    "1201000 42000 10000 50000" "(1) a1's execution's tokens, input output cache_write cache_read (each repeated line counted once)"
assert_usd "$(j 7d "[.executions[] | select(.execution == \"$a1_run\") | .usd][0]")" 3.4855 "(1) a1's execution's cost"
assert_usd "$(j 7d "[.executions[] | select(.execution == \"$a1_run\") | .models[] | select(.model == \"bb-model-x\") | .usd][0]")" 3.0855 \
    "(1) a1's execution's cost on bb-model-x"
assert_usd "$(j 7d "[.executions[] | select(.execution == \"$a1_run\") | .models[] | select(.model == \"bb-model-y\") | .usd][0]")" 0.4 \
    "(1) a1's execution's cost on bb-model-y"
wait_until 30 costs "$task_b2" 0.00015 ||
    fail "(1) b2's task does not cost \$0.00015 within 30 s of its report: $(task_usd "$task_b2")"
cost 7d "(1)"
b2_z=$(j 7d "[.executions[] | select(.agent == \"$agent_b2\") | .models[] | select(.model == \"bb-model-z\")][0]")
[ "$b2_z" != null ] || fail "(1) b2's execution lists no bb-model-z: $(j 7d "[.executions[] | select(.agent == \"$agent_b2\")]" | tr -d '\n ')"
assert_eq "$(printf '%s' "$b2_z" | jq -r '.usd')" null "(1) the cost of b2's bb-model-z, a model SPAWN_MODEL_PRICES does not price"
assert_eq "$(printf '%s' "$b2_z" | jq -r '.tokens | .input + .output + .cache_write + .cache_read')" 5500 "(1) b2's bb-model-z tokens"
assert_usd "$(j 7d "[.executions[] | select(.agent == \"$agent_b2\") | .usd] | add")" 0.00015 "(1) b2's execution's cost (the priced model only)"
unpriced_b3() { [ "$(j 7d "[.executions[]? | select(.agent == \"$agent_b3\") | .unpriced_tokens] | add")" = 3300 ]; }
cost 7d "(1)"
if ! unpriced_b3; then
    sleep 10
    cost 7d "(1)"
fi
assert_eq "$(j 7d "[.executions[] | select(.agent == \"$agent_b3\") | .unpriced_tokens] | add")" 3300 "(1) b3's unpriced tokens"
assert_usd "$(j 7d "[.executions[] | select(.agent == \"$agent_b3\") | .usd] | add")" 0 "(1) the cost of b3's execution, on an unpriced model only"
assert_eq "$(j 7d '.total.unpriced_tokens')" 8800 "(1) the answer's unpriced tokens"

# --- (2) the budget ------------------------------------------------------------------

same_day
capture "$SPAWN_BIN" mission configure --id "$mission_A" --daily-usd 0.50
[ "$status" -eq 0 ] || fail "(2) spawn mission configure --id $mission_A --daily-usd 0.50 failed: $(err)"
capture "$SPAWN_BIN" mission configure --id "$mission_B" --daily-usd 1.00
[ "$status" -eq 0 ] || fail "(2) spawn mission configure --id $mission_B --daily-usd 1.00 failed: $(err)"
assert_eq "$(q "select (budget ->> 'daily_usd')::numeric = 0.50 from mission_missions where id = $mission_A")" t \
    "(2) mission A's recorded daily budget ($(q "select budget from mission_missions where id = $mission_A"))"
sleep 10
assert_eq "$(alerts_for "$mission_A")" 0 "(2) cost_budget alerts for mission A while it has spent \$0.4855 of \$0.50 today (\$3.4855 in 7 d)"
assign a2 "$mission_A" "bb-model-x 100000 10000 0 0 0"
wait_until 150 reported "$task_a2" ||
    harness "the task of worker a2 was not reported within 150 s ($(q "select state from mission_tasks where id = $task_a2"))"
fired() { [ "$(alerts_for "$mission_A")" -ge 1 ]; }
wait_until 60 fired ||
    fail "(2) no cost_budget alert for mission A within 60 s of its spending \$0.9355 today, over its \$0.50 (a2's task costs $(task_usd "$task_a2"))"
same_day
assert_eq "$(alerts_for "$mission_A")" 1 "(2) cost_budget alerts for mission A"
assert_eq "$(q "select state from spawn_alerts where kind = 'cost_budget' and subject = 'mission #$mission_A'")" open \
    "(2) the state of mission A's cost_budget alert"
code=$(get /api/alerts "$WORK/alerts.json")
[ "$code" = 200 ] || harness "GET /api/alerts answered $code"
assert_eq "$(jq "[.. | objects | select(.kind? == \"cost_budget\" and .subject? == \"mission #$mission_A\") | .key] | unique | length" "$WORK/alerts.json")" 1 \
    "(2) mission A's cost_budget alerts the alerts API serves"
assert_eq "$(alerts_for "$mission_B")" 0 "(2) cost_budget alerts for mission B, \$0.06015 of \$1.00 today"

# --- (3) the totals ----------------------------------------------------------------------

code=$(get "/api/cost?window=30d" "$WORK/cost-30d.json")
assert_eq "$code" 400 "(3) GET /api/cost?window=30d (a window it does not serve)"
cost 24h "(3)"
cost 7d "(3)"
for check in \
    "24h .total.usd 0.99565 the total over 24 h" \
    "7d .total.usd 3.99565 the total over 7 d"; do
    set -- $check
    window=$1 filter=$2 expected=$3
    shift 3
    assert_usd "$(j "$window" "$filter")" "$expected" "(3) $*"
done
for row in \
    "24h missions mission $mission_A 0.9355" "7d missions mission $mission_A 3.9355" \
    "24h missions mission $mission_B 0.06015" "7d missions mission $mission_B 0.06015" \
    "24h tasks task $task_a1 0.4855" "7d tasks task $task_a1 3.4855" \
    "24h tasks task $task_a2 0.45" "7d tasks task $task_a2 0.45" \
    "24h tasks task $task_b1 0.06" "24h tasks task $task_b2 0.00015" "24h tasks task $task_b3 0" \
    "24h agents agent $agent_a1 0.4855" "7d agents agent $agent_a1 3.4855" "24h agents agent $agent_a2 0.45" \
    "24h agents agent $agent_b1 0.06" "24h agents agent $agent_b2 0.00015" \
    "24h models model bb-model-x 0.5955" "7d models model bb-model-x 3.5955" \
    "24h models model bb-model-y 0.40015" "7d models model bb-model-y 0.40015"; do
    set -- $row
    assert_usd "$(usd_of "$1" "$2" "$3" "$4")" "$5" "(3) the $1 cost of $3 $4"
done
assert_eq "$(usd_of 24h models model bb-model-z)" null "(3) the 24 h cost of bb-model-z, not priced"
assert_eq "$(j 24h '.total.tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(" ")')" \
    "319100 54810 10000 50000" "(3) the 24 h tokens, input output cache_write cache_read"
assert_eq "$(j 7d '.total.tokens.input')" 1319100 "(3) the 7 d input tokens"
assert_eq "$(j 24h "[.tasks[] | select(.task == $task_a2) | .mission][0] | tostring")" "$mission_A" "(3) the mission of a2's task"
assert_eq "$(j 7d "[.tasks[] | select(.task == $task_a1) | .mission][0] | tostring")" "$mission_A" "(3) the mission of a1's task"

# --- (4) the page's figures ----------------------------------------------------------------

[ -f "$tree/web/src/cost.ts" ] || fail "(4) there is no web/src/cost.ts"
cat >"$WORK/cost.test.mjs" <<'TEST'
// The cost page's figures, as web/src/cost.ts's costView makes them from what
// GET /api/cost answers.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { costView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/cost.ts`).href);
const answer = (window) => JSON.parse(readFileSync(`${env.BB_WORK}/cost-${window}.json`, "utf8"));
const one = (rows, key) => {
  const found = rows.filter((entry) => entry.key === key);
  assert.equal(found.length, 1, `one row ${key} in ${JSON.stringify(rows)}`);
  return found[0];
};
const row = (rows, key) => one(rows, key).usd;

test("the page shows the 24 h cost to the cent, and a cost under a cent as <$0.01", () => {
  const view = costView(answer("24h"));
  assert.equal(view.total, "$1.00", "the total");
  assert.equal(row(view.missions, `#${env.BB_MA}`), "$0.94", "mission A");
  assert.equal(row(view.missions, `#${env.BB_MB}`), "$0.06", "mission B");
  assert.equal(row(view.tasks, `T${env.BB_TA1}`), "$0.49", "a1's task");
  assert.equal(row(view.tasks, `T${env.BB_TA2}`), "$0.45", "a2's task");
  assert.equal(row(view.tasks, `T${env.BB_TB2}`), "<$0.01", "b2's task: not zero, so never $0.00");
  assert.equal(row(view.agents, env.BB_AA1), "$0.49", "a1");
  assert.equal(row(view.agents, env.BB_AB2), "<$0.01", "b2");
  assert.equal(row(view.tasks, `T${env.BB_TB3}`), "unpriced", "b3's task, on an unpriced model only: never $0.00");
  assert.equal(row(view.agents, env.BB_AB3), "unpriced", "b3");
  assert.equal(one(view.tasks, `T${env.BB_TB3}`).unpriced, true, "b3's task is marked unpriced");
  assert.equal(one(view.tasks, `T${env.BB_TB2}`).unpriced, true, "b2's task, part unpriced, is marked so");
  assert.equal(one(view.tasks, `T${env.BB_TA1}`).unpriced, false, "a1's task, all priced, is not");
  assert.equal(view.unpriced, true, "the total has unpriced use");
});

test("and the 7 d cost", () => {
  const view = costView(answer("7d"));
  assert.equal(view.total, "$4.00", "the total");
  assert.equal(row(view.missions, `#${env.BB_MA}`), "$3.94", "mission A");
  assert.equal(row(view.tasks, `T${env.BB_TA1}`), "$3.49", "a1's task");
  assert.equal(row(view.tasks, `T${env.BB_TA2}`), "$0.45", "a2's task");
  assert.equal(row(view.agents, env.BB_AA1), "$3.49", "a1");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_MA=$mission_A BB_MB=$mission_B BB_TA1=$task_a1 BB_TA2=$task_a2 \
    BB_TB2=$task_b2 BB_AA1=$agent_a1 BB_AB2=$agent_b2 BB_TB3=$task_b3 BB_AB3=$agent_b3 \
    node --test "$WORK/cost.test.mjs" >"$WORK/cost.out" 2>&1; then
    if grep -q "ERR_MODULE_NOT_FOUND\|SyntaxError\|ERR_UNKNOWN_FILE_EXTENSION\|ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX" "$WORK/cost.out"; then
        fail "(4) web/src/cost.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/cost.out" | tr '\n' ' ')"
    fi
    fail "(4) the page's figures: $(grep -E 'AssertionError|one row|expected|actual|is not a function' "$WORK/cost.out" | head -n 6 | tr '\n' ' ')"
fi

# --- (5) once only -------------------------------------------------------------------------

# bb-model-x's rates doubled; bb-model-y's as they were.
stop_loop
stop_serve
SPAWN_MODEL_PRICES='{"bb-model-x":{"input":6,"output":30,"cache_write":7.5,"cache_read":0.6},"bb-model-y":{"input":1,"output":5,"cache_write":1.25,"cache_read":0.1}}'
start_loop
start_serve
sleep 10
cost 24h "(5)"
cost 7d "(5)"
assert_usd "$(j 24h .total.usd)" 0.99565 "(5) the 24 h total after the restart with new prices (each message keeps the price it was recorded at; none counted twice)"
assert_usd "$(j 7d .total.usd)" 3.99565 "(5) the 7 d total after the restart with new prices"
assert_usd "$(usd_of 7d tasks task "$task_a1")" 3.4855 "(5) a1's 7 d cost after the restart with new prices"
assign a3 "$mission_A" "bb-model-x 50000 0 0 0 0"
wait_until 150 reported "$task_a3" ||
    harness "the task of worker a3 was not reported within 150 s ($(q "select state from mission_tasks where id = $task_a3"))"
wait_until 30 costs "$task_a3" 0.30 ||
    fail "(5) a3's task does not cost \$0.30 (bb-model-x at the new rates) within 30 s of its report: $(task_usd "$task_a3")"
sleep 10
same_day
assert_eq "$(alerts_for "$mission_A")" 1 "(5) cost_budget alerts for mission A after the restart and more spending"
assert_eq "$(alerts_for "$mission_B")" 0 "(5) cost_budget alerts for mission B after the restart"
cost 24h "(5)"
cost 7d "(5)"
assert_usd "$(j 24h .total.usd)" 1.29565 "(5) the 24 h total after a3 (its \$0.30 once, nothing else)"
assert_usd "$(j 7d .total.usd)" 4.29565 "(5) the 7 d total after a3"
assert_usd "$(usd_of 24h missions mission "$mission_A")" 1.2355 "(5) mission A's 24 h cost after a3"
assert_eq "$(j 24h '.total.tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(" ")')" \
    "369100 54810 10000 50000" "(5) the 24 h tokens after a3"

# --- (6) the setting is checked --------------------------------------------------------------

stop_loop
stop_serve
for case in "loop:not json" 'loop:{"bb-model-x":{"input":-1,"output":15,"cache_write":3.75,"cache_read":0.3}}' \
    "serve:not json" 'serve:{"bb-model-x":{"input":-1,"output":15,"cache_write":3.75,"cache_read":0.3}}'; do
    which=${case%%:*}
    bad=${case#*:}
    if [ "$which" = loop ]; then
        SPAWN_MODEL_PRICES=$bad "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >"$WORK/bad.log" 2>&1 &
    else
        SPAWN_MODEL_PRICES=$bad "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/bad.log" 2>&1 &
    fi
    bad_pid=$!
    gone() { ! kill -0 "$bad_pid" 2>/dev/null; }
    if ! wait_until 15 gone; then
        kill "$bad_pid" 2>/dev/null || true
        wait "$bad_pid" 2>/dev/null || true
        fail "(6) spawn $which with SPAWN_MODEL_PRICES='$bad' still runs after 15 s"
    fi
    rc=0
    wait "$bad_pid" || rc=$?
    [ "$rc" -ne 0 ] || fail "(6) spawn $which with SPAWN_MODEL_PRICES='$bad' exited 0"
    grep -q SPAWN_MODEL_PRICES "$WORK/bad.log" ||
        fail "(6) spawn $which with SPAWN_MODEL_PRICES='$bad' did not name the setting: $(head -c 300 "$WORK/bad.log")"
done

echo "041: each execution's usage is recorded once per message and priced by model when recorded; the API and the page give the cost per mission, task and agent over 24 h and 7 d to the cent, unpriced use marked, never \$0.00; a mission over its daily budget raised one alert, still one after a restart and more spending"
exit 0
