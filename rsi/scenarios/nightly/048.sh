#!/bin/sh
# Black box: the cost counts every archived execution. The model calls of
# executions the cost step never read (they ended before 041's Bridge read
# their transcripts, or their usage events are missing) are counted once
# from the transcript archive; until then, the page says since when it has
# counted.
#
# Objective 041 records a message's cost only from the Bridge's
# `model.usage` events: a message the Bridge read before it emitted them
# (every execution that ended before 041 was deployed) is in the archive
# (`<root>/.transcripts/<agent>/<native>[.<n>].jsonl`) and in no cost. On
# the live fleet on 2026-11-11 (rev-b's check, sync/features.md) the 24 h
# and 7 d answers were the same $109.71, while the archive's last 24 h held
# about $168.9 of calls from 13 more workers; the page's "Last 7 days" read
# about 35% low and said nothing of it. Required (the objective's prompt
# says each rule): the loop counts, once and without a person, every
# assistant message the archive holds that the cost record lacks, by 041's
# rules (once per message id across every line and copy, its largest usage,
# its earliest line's time, its model, priced when recorded) for the
# archive's agent, that agent's execution and task at the message's time,
# and that task's mission; doing it again adds nothing. `/api/cost` says
# `counted_since_ms`, the time of the earliest message the record holds
# (null when it holds none); web/src/cost.ts's `costView(answer).since` is
# "Counted since <YYYY-MM-DD HH:MM> UTC" when the window starts before it,
# and null otherwise.
#
# A real `spawn loop` (the Bridge, the dispatcher, the cost step; no model
# judge, no supervisor) runs ephemeral workers of two missions on the fake
# engine below (041's: a plan per worker says what usage its one turn
# writes, each message as two lines with the same id and usage, timed <age>
# seconds ago); `spawn serve` answers on a loopback port. Prices (USD per
# million tokens: input, output, cache write, cache read): bb-model-x 3, 15,
# 3.75, 0.30; bb-model-y 1, 5, 1.25, 0.10; bb-model-z is not priced.
#   mission A: h1 (history): x 100000 in, 10000 out, now ($0.45); x 1000000
#     in, 2 days ago ($3.00); y 500000 in, 11 days ago ($0.50, outside 7 d).
#     l1 (live, after the history is counted): x 50000 in, now ($0.15).
#   mission B: h2 (history in part): x 10000 in, 2000 out ($0.06, its usage
#     events removed); y 200000 in, 40000 out ($0.40, kept); z 5000 in, 500
#     out (unpriced, kept).
# History is made as the live fleet has it: h1 and h2 report and their
# tasks are cancelled (their executions end); with the loop stopped, the
# scenario deletes, in its own database, every `model.usage` event of h1
# and the one of h2's first message, and their rows in the cost record,
# leaving the archive and every other record as they were.
# Checked:
#   (1) before the loop counts history (serve alone): `GET /api/cost`
#       answers h2's kept use only ($0.40, 5500 unpriced tokens) and
#       `counted_since_ms`, the time of the earliest message it holds (h2's
#       kept messages); `costView` of the 24 h and 7 d answers says
#       "Counted since <that minute> UTC".
#   (2) the loop starts: within 30 s, 24 h $0.91 and 7 d $3.91 (h1's 11-day
#       message is in neither); per mission (A $0.45 / $3.45, B $0.46 /
#       $0.46), per task and agent (h1 $3.45 over 7 d, its messages from
#       before its task counted for that task), h1's execution listed with
#       its $3.45; 5500 tokens unpriced; `counted_since_ms` is h1's 11-day
#       message's time; per agent, the 7 d tokens equal a recount of the
#       archive over the answer's own window.
#   (3) l1 runs live and is counted once: 24 h $1.06, 7 d $4.06.
#   (4) once only: a second copy of h1's archived transcript is written
#       (`<native>.1.jsonl`, as the Bridge writes after a resume) and the
#       loop is restarted twice: 10 s after each, every total, mission,
#       task and agent is what it was, to the token.
#   (5) the page's words: `costView(answer).since` "Counted since <minute>
#       UTC" for (1)'s answers, null for (4)'s; cost.ts must load with node
#       alone.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (ea3a2e9f): the answer has no
# `counted_since_ms`; were it there, (2) would fail: nothing counts h1.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket, no network. Every wait is bounded in time.


set -eu


asserted=""
harness() {
    echo "048: harness: $*" >&2
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
SPAWN_DB_NAME="bb_048_backfill_$run"
SPAWN_TMUX_SOCKET="bb-048-$run"
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
            echo "048: the loop's last lines:" >&2
            tail -n 15 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/serve.log" ]; then
            echo "048: serve's last lines:" >&2
            tail -n 5 "$WORK/serve.log" >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "048: the engines' turns (time, agent, execution, plan):" >&2
            cat "$WORK/turns.log" >&2
        fi
        if [ -s "$WORK/cost-7d.json" ]; then
            echo "048: the last 7 d answer (first 1500 bytes):" >&2
            head -c 1500 "$WORK/cost-7d.json" >&2
            echo >&2
        fi
        if [ -n "$database_made" ]; then
            echo "048: the tasks (id, mission, state, agent):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select id, mission_id, state, agent_id
                from mission_tasks order by id" >&2 2>/dev/null || true
            echo "048: the alerts (key, kind, subject, state, text):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, kind, subject, state, left(coalesce(text, ''), 120)
                from spawn_alerts where not notice order by fired_at_ms" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "048: harness: unexpected exit $rc" >&2
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
bb_engine >"$WORK/claude" <<'ENGINE'
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
  if bb_read_line typed; then
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
        --brief "BB:PLAN:$1 Do the 048 task and report."
    [ "$status" -eq 0 ] || harness "task assign ($1) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $2")
    agent=$(q "select agent_id from mission_tasks where id = $task")
    [ -n "$task" ] && [ -n "$agent" ] || harness "task assign ($1) made no task with an agent"
    eval "task_$1=\$task agent_$1=\$agent"
}

# The page's API: GET <path> into <file>; its status code.
get() { bb_http "$2" --noproxy '*' "$base$1"; }
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
    capture "$SPAWN_BIN" mission add --text "048 cost mission $name"
    [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
    id=$(jq -r .mission "$WORK/out.txt")
    [ -n "$id" ] && [ "$id" != null ] || harness "mission add printed no id: $(out)"
    eval "mission_$name=\$id"
done

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
# A restart writes the same log: it is emptied here, in this shell, before
# the launch, and the launch appends. With `serve >"$WORK/serve.log" &` the
# truncation ran in the child, after the fork, so this shell could read the
# stopped serve's port line first and send every request to a dead port.
start_serve() {
    : >"$WORK/serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >>"$WORK/serve.log" 2>&1 &
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
    # This launch answers on the port it named.
    ready=$(curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' "$base/" 2>"$WORK/ready.err") || true
    [ "$ready" = 200 ] || harness "serve (pid $serve_pid) named port $port and GET / there answered [$ready]: $(head -c 300 "$WORK/ready.err")"
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


# --- helpers of 048 --------------------------------------------------------------

archive=$SPAWN_WORKSPACE_ROOT/.transcripts
# The assistant messages an agent's archive holds, each once: "<id> <at ms>
# <model> <input> <output> <cache write> <cache read>" (its earliest line's
# time, its largest usage), as 041 counts a transcript.
archived() { # <agent>
    find "$archive/$1" -name '*.jsonl' -type f -exec cat {} + 2>/dev/null | jq -rs '
        [.[] | select(.type == "assistant" and .message.id != null and .message.usage != null)
         | {id: .message.id, model: .message.model,
            at: (((.timestamp[0:19] + "Z") | fromdateiso8601) * 1000 + (.timestamp[20:23] | tonumber)),
            t: [.message.usage.input_tokens // 0, .message.usage.output_tokens // 0,
                .message.usage.cache_creation_input_tokens // 0, .message.usage.cache_read_input_tokens // 0]}]
        | group_by(.id)
        | map({id: .[0].id, model: (sort_by(.at)[0].model), at: (map(.at) | min),
               t: [range(4) as $k | (map(.t[$k]) | max)]})
        | .[] | "\(.id) \(.at) \(.model) \(.t | map(tostring) | join(" "))"'
}
# The archive's tokens of an agent's messages timed in [since, until):
# "input output cache_write cache_read".
archive_tokens() { # <agent> <since ms> <until ms>
    archived "$1" | awk -v s="$2" -v u="$3" '
        $2 >= s && $2 < u { i += $4; o += $5; w += $6; r += $7 }
        END { printf "%d %d %d %d\n", i, o, w, r }'
}
# The time of an agent's archived message of <model> with <input> tokens.
message_at() { # <agent> <model> <input>
    archived "$1" | awk -v m="$2" -v i="$3" '$3 == m && $4 == i { print $2; exit }'
}
message_id() { # <agent> <model> <input>
    archived "$1" | awk -v m="$2" -v i="$3" '$3 == m && $4 == i { print $1; exit }'
}
minute() { date -u -d "@$(($1 / 1000))" '+%Y-%m-%d %H:%M'; }
ended() { [ "$(q "select count(*) from agent_executions where agent_id = '$1' and ended_at_ms is null")" = 0 ]; }
archived_count() { # <agent> <n>: the archive holds n messages of the agent
    [ "$(archived "$1" | wc -l)" -ge "$2" ]
}
# Every figure the answers give, one per line, for (4)'s comparison.
figures() {
    for w in 24h 7d; do
        jq -r --arg w "$w" '
            "\($w) total \(.total.usd) \(.total.unpriced_tokens) \(.total.tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(" "))",
            (.missions[] | "\($w) mission \(.mission) \(.usd) \(.tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(" "))"),
            (.tasks[] | "\($w) task \(.task) \(.usd) \(.tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(" "))"),
            (.agents[] | "\($w) agent \(.agent) \(.usd) \(.tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(" "))")' \
            "$WORK/cost-$w.json" | sort
    done
}

# --- history: h1 and h2 run, report, end ---------------------------------------------

assign h1 "$mission_A" "bb-model-x 100000 10000 0 0 0
bb-model-x 1000000 0 0 0 172800
bb-model-y 500000 0 0 0 950400"
assign h2 "$mission_B" "bb-model-x 10000 2000 0 0 0
bb-model-y 200000 40000 0 0 0
bb-model-z 5000 500 0 0 0"
for label in h1 h2; do
    eval "task=\$task_$label agent=\$agent_$label"
    wait_until 150 reported "$task" ||
        harness "the task of worker $label was not reported within 150 s ($(q "select state from mission_tasks where id = $task"))"
    wait_until 30 archived_count "$agent" 3 ||
        harness "worker $label's three messages were not archived within 30 s of its report ($(archived "$agent" | wc -l))"
    capture "$SPAWN_BIN" task cancel --id "$task"
    [ "$status" -eq 0 ] || harness "task cancel ($label) failed: $(err)"
    wait_until 30 ended "$agent" || harness "worker $label's execution did not end within 30 s of its task's cancel"
done
# 041 recorded them; history is what it did not.
wait_until 30 costs "$task_h2" 0.46 ||
    harness "h2's task does not cost \$0.46 within 30 s (041's recording): $(task_usd "$task_h2")"
h2_x=$(message_id "$agent_h2" bb-model-x 10000)
h2_y_at=$(message_at "$agent_h2" bb-model-y 200000)
h2_z_at=$(message_at "$agent_h2" bb-model-z 5000)
h1_old_at=$(message_at "$agent_h1" bb-model-y 500000)
[ -n "$h2_x" ] && [ -n "$h2_y_at" ] && [ -n "$h2_z_at" ] && [ -n "$h1_old_at" ] ||
    harness "the archive lacks a message of h1 or h2: $(archived "$agent_h1" | tr '\n' ';') $(archived "$agent_h2" | tr '\n' ';')"
kept_since=$h2_y_at
[ "$h2_z_at" -ge "$kept_since" ] || kept_since=$h2_z_at

stop_loop
stop_serve
# As the live fleet's executions from before 041: the archive holds their
# messages, the cost record and its events do not.
psql -d "$SPAWN_DB_NAME" -v ON_ERROR_STOP=1 -q -c "
    delete from spawn_cost_messages where agent_id = '$agent_h1' or message_id = '$h2_x';
    delete from spawn_events where event_name = 'model.usage'
       and (agent_id = '$agent_h1' or kind -> 'detail' ->> 'message_id' = '$h2_x');" >/dev/null 2>&1 ||
    harness "cannot remove h1's and h2's first message's usage records"
[ "$(q "select count(*) from spawn_events where event_name = 'model.usage' and agent_id = '$agent_h2'")" -ge 2 ] ||
    harness "h2's kept messages have no model.usage events left"

# --- (1) before history is counted ----------------------------------------------------

start_serve
cost 24h "(1)"
cost 7d "(1)"
assert_usd "$(j 7d .total.usd)" 0.40 "(1) the 7 d total with h1's and h2's first message's usage records removed (h2's kept \$0.40 only)"
assert_eq "$(j 7d .total.unpriced_tokens)" 5500 "(1) the 7 d unpriced tokens before history is counted"
assert_eq "$(j 7d '.counted_since_ms | tostring')" "$kept_since" \
    "(1) the 7 d answer's counted_since_ms (the earliest message the record holds: h2's kept messages, $(minute "$kept_since") UTC)"
assert_eq "$(j 24h '.counted_since_ms | tostring')" "$kept_since" "(1) the 24 h answer's counted_since_ms"
cp "$WORK/cost-24h.json" "$WORK/before-24h.json"
cp "$WORK/cost-7d.json" "$WORK/before-7d.json"

# --- (2) the loop counts history ------------------------------------------------------

start_loop
all_counted() { near "$(task_usd "$task_h1")" 3.45; }
wait_until 30 all_counted ||
    fail "(2) h1's task, whose messages are only in the archive, costs $(task_usd "$task_h1") over 7 d 30 s after the loop started, not \$3.45: the loop counts what the archive holds and the cost record lacks"
cost 24h "(2)"
cost 7d "(2)"
assert_usd "$(j 24h .total.usd)" 0.91 "(2) the 24 h total once history is counted (h1 \$0.45, h2 \$0.46)"
assert_usd "$(j 7d .total.usd)" 3.91 "(2) the 7 d total once history is counted (h1 \$3.45, h2 \$0.46; h1's 11-day message in neither)"
assert_usd "$(usd_of 24h missions mission "$mission_A")" 0.45 "(2) mission A's 24 h cost"
assert_usd "$(usd_of 7d missions mission "$mission_A")" 3.45 "(2) mission A's 7 d cost"
assert_usd "$(usd_of 7d missions mission "$mission_B")" 0.46 "(2) mission B's 7 d cost (h2's removed message counted again, once)"
assert_usd "$(usd_of 7d tasks task "$task_h2")" 0.46 "(2) h2's task's 7 d cost"
assert_usd "$(usd_of 7d agents agent "$agent_h1")" 3.45 "(2) h1's 7 d cost"
assert_usd "$(usd_of 24h tasks task "$task_h1")" 0.45 "(2) h1's task's 24 h cost"
h1_runs=$(q "select string_agg(execution_id, ' ') from agent_executions where agent_id = '$agent_h1'")
listed=$(j 7d "[.executions[]? | select(.agent == \"$agent_h1\")] | if length == 1 then \"\(.[0].execution) \(.[0].usd) \(.[0].task)\" else \"none\" end")
[ "${listed%% *}" = "$h1_runs" ] ||
    fail "(2) the 7 d answer's executions of h1 [$listed] are not its one execution $h1_runs: $(j 7d "[.executions[]? | select(.agent == \"$agent_h1\")]" | tr -d '\n ')"
rest=${listed#* }
assert_usd "${rest%% *}" 3.45 "(2) h1's execution's 7 d cost"
assert_eq "${rest#* }" "$task_h1" "(2) the task of h1's execution (its messages timed before the task count for it)"
assert_eq "$(j 7d .total.unpriced_tokens)" 5500 "(2) the 7 d unpriced tokens"
assert_eq "$(j 7d '.counted_since_ms | tostring')" "$h1_old_at" \
    "(2) counted_since_ms once history is counted: h1's 11-day message's time ($(minute "$h1_old_at") UTC)"
since7=$(j 7d .since_ms)
until7=$(j 7d .until_ms)
for agent in "$agent_h1" "$agent_h2"; do
    assert_eq "$(j 7d "[.agents[] | select(.agent == \"$agent\") | .tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(\" \")][0] // \"none\"")" \
        "$(archive_tokens "$agent" "$since7" "$until7")" "(2) $agent's 7 d tokens against a recount of the archive over the answer's window"
done

# --- (3) live use is counted once ---------------------------------------------------------

assign l1 "$mission_A" "bb-model-x 50000 0 0 0 0"
wait_until 150 reported "$task_l1" ||
    harness "the task of worker l1 was not reported within 150 s ($(q "select state from mission_tasks where id = $task_l1"))"
wait_until 30 costs "$task_l1" 0.15 ||
    fail "(3) l1's task does not cost \$0.15 within 30 s of its report: $(task_usd "$task_l1")"
wait_until 30 archived_count "$agent_l1" 1 || harness "l1's message was not archived within 30 s"
sleep 5
cost 24h "(3)"
cost 7d "(3)"
assert_usd "$(j 24h .total.usd)" 1.06 "(3) the 24 h total after l1 (its \$0.15 once: recorded live and archived)"
assert_usd "$(j 7d .total.usd)" 4.06 "(3) the 7 d total after l1"
for agent in "$agent_h1" "$agent_h2" "$agent_l1"; do
    assert_eq "$(j 7d "[.agents[] | select(.agent == \"$agent\") | .tokens | [.input, .output, .cache_write, .cache_read] | map(tostring) | join(\" \")][0] // \"none\"")" \
        "$(archive_tokens "$agent" "$(j 7d .since_ms)" "$(j 7d .until_ms)")" "(3) $agent's 7 d tokens against a recount of the archive"
done
figures >"$WORK/figures-3.txt"

# --- (4) once only ------------------------------------------------------------------------

h1_native=$(find "$archive/$agent_h1" -name '*.jsonl' ! -name '*.*.jsonl' -type f | head -n 1)
[ -n "$h1_native" ] || harness "no archived transcript of h1 under $archive/$agent_h1"
cp "$h1_native" "${h1_native%.jsonl}.1.jsonl"
for round in 1 2; do
    stop_loop
    start_loop
    sleep 10
    cost 24h "(4)"
    cost 7d "(4)"
    figures >"$WORK/figures-4.txt"
    if ! cmp -s "$WORK/figures-3.txt" "$WORK/figures-4.txt"; then
        fail "(4) after a second archived copy of h1's transcript and restart $round of the loop, the figures changed (was, now): $(diff "$WORK/figures-3.txt" "$WORK/figures-4.txt" | grep '^[<>]' | head -n 6 | tr '\n' ' ')"
    fi
done

# --- (5) the page's words -------------------------------------------------------------------

[ -f "$tree/web/src/cost.ts" ] || fail "(5) there is no web/src/cost.ts"
cat >"$WORK/since.test.mjs" <<'TEST'
// costView's `since`: the window starts before the earliest counted message.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { costView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/cost.ts`).href);
const read = (name) => JSON.parse(readFileSync(`${env.BB_WORK}/${name}.json`, "utf8"));

test("before history is counted, both windows say since when", () => {
  for (const name of ["before-24h", "before-7d"]) {
    assert.equal(costView(read(name)).since, `Counted since ${env.BB_KEPT} UTC`, name);
  }
});

test("once it is, neither does", () => {
  for (const name of ["cost-24h", "cost-7d"]) {
    assert.equal(costView(read(name)).since, null, name);
  }
});

test("the rule, on its own", () => {
  const answer = { ...read("cost-24h") };
  assert.equal(costView({ ...answer, counted_since_ms: null }).since, null, "nothing counted: no note");
  assert.equal(costView({ ...answer, counted_since_ms: answer.since_ms }).since, null, "counted from the window's start");
  assert.equal(
    costView({ ...answer, since_ms: Date.UTC(2026, 10, 1), counted_since_ms: Date.UTC(2026, 10, 10, 17, 19, 59) }).since,
    "Counted since 2026-11-10 17:19 UTC",
  );
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_KEPT="$(minute "$kept_since")" node --test "$WORK/since.test.mjs" >"$WORK/since.out" 2>&1; then
    if grep -q "ERR_MODULE_NOT_FOUND\|SyntaxError\|ERR_UNKNOWN_FILE_EXTENSION\|ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX" "$WORK/since.out"; then
        fail "(5) web/src/cost.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/since.out" | tr '\n' ' ')"
    fi
    fail "(5) the page's words: $(grep -E 'AssertionError|expected|actual|is not a function|^not ok' "$WORK/since.out" | head -n 6 | tr '\n' ' ')"
fi

echo "048: messages only the archive held are counted once by the loop, by 041's rules, and again adds nothing; the answer says since when it has counted and the page says it while a window starts before"
exit 0
