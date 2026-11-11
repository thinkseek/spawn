#!/bin/sh
# Black box: the scorecard counts a running turn's model calls within a
# pass, and says a window is settling while the archive may still lack a
# call made before its end.
#
# The scorecard's cost reads only the transcript archive
# (`<root>/.transcripts`), and the Bridge copies an engine's transcript into
# it only when a turn ends or the execution ends. A call made in a running
# turn is missing from every window until that turn ends: up to an hour on
# the live fleet, and never if the turn never ends. Nothing says a window's
# cost may still grow. Required (the objective's prompt says each rule):
# the Bridge archives a live execution's transcript on its passes while it
# has grown, not only at a turn's end, so a call is counted within a pass of
# being written; the scorecard's JSON (`spawn scorecard --json`, and
# `/api/scorecard`) says `settling: true` while the archive may lack a call
# some execution made before the window's end (its transcript, as Spawn
# last knew it, held lines before that end that are not archived, or Spawn
# has not read it since that end) and `settling: false` once it cannot. Live
# or ended, an execution holds a window open exactly while lines it wrote
# before the window's end are not archived: one that is idle, or ended,
# with everything archived holds none open, and the Bridge goes on trying
# an ended execution's copy until it is made, across its own restarts.
# web/src/settling.ts's `settlingNote(card)` says it in words on the
# scorecard page.
#
# A real `spawn loop` (the Bridge, the dispatcher; no model judge, no gate,
# no supervisor) runs ephemeral workers on the fake engine below;
# `spawn serve` answers on a loopback port. Inputs come from a person
# (`spawn submit`). The live transcripts are the truth a window is checked
# against: each assistant message counted once, at its earliest line, with
# its input, output and cache tokens, as the scorecard counts the archive.
# Checked:
#   (1) a running turn: worker `long` is in a turn of 40 calls, one every
#       2 s. After its third call, the window [start, U) with U the current
#       second reads, within 15 s, the tokens the live transcripts hold
#       before U, and `settling` false, while the turn still runs.
#   (2) an archive held back: a second U0 is taken, then worker `held` is
#       given its task with a plain file put at its archive path from the
#       start (nothing the Bridge does can write there, and nothing it
#       writes is before U0). In a turn of 12 calls, after its third
#       call, the window to the current second U2 reads `settling` true, and
#       fewer tokens than the live transcripts hold before U2, for 6 s
#       (three readings), while the window to U0, a second taken before
#       `held`'s turn began, reads the live tokens before U0 and `settling`
#       false at each reading (only a call before a window's end holds it
#       open). Then `held`'s task is cancelled mid-turn and its execution
#       ends, its archive still held: the window to U2 still reads
#       `settling` true and fewer tokens, at three readings over 6 s; the
#       loop is restarted, the path still held, and the window still reads
#       `settling` true 5 s on. The scenario checks the plain file is
#       untouched. Once it is removed,
#       within 20 s the window reads the live tokens before U2 and
#       `settling` false.
#   (3) the window of (1) still reads what it settled at. With `long`'s
#       turn ended (and `held`'s execution), a window to the current second
#       reads the live tokens and `settling` false within 15 s.
#   (4) the page's words: web/src/settling.ts's `settlingNote(card)`: null
#       for a card that is not settling; "Still settling: a turn that was
#       running at the window's end may add to its cost." for one that is.
#       It must load with node alone.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (4dc5e6ec): the running turn's calls
# are not counted until it ends, and the card has no `settling`.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN,
# SPAWN_TEST_DATABASE_URL, tmux, jq, curl and node; creates and drops its
# own database, uses a private tmux socket, loopback only. Every wait is
# bounded in time. On a passing tree it takes about two minutes.
set -eu

asserted=""
harness() {
    echo "047: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_tmux=$(command -v tmux)

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
SPAWN_DB_NAME="bb_047_settling_$run"
SPAWN_TMUX_SOCKET="bb-047-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
wrapper_pid=""
serve_pid=""
stop_loop() {
    [ -n "$wrapper_pid" ] || return 0
    : >"$WORK/loop.stop"
    child=$(cat "$WORK/loop.pid" 2>/dev/null || true)
    if [ -n "$child" ]; then kill -TERM "$child" 2>/dev/null || true; fi
    i=0
    while kill -0 "$wrapper_pid" 2>/dev/null; do
        i=$((i + 1))
        if [ "$i" -eq 150 ] && [ -n "$child" ]; then kill -KILL "$child" 2>/dev/null || true; fi
        if [ "$i" -ge 200 ]; then
            kill -KILL "$wrapper_pid" 2>/dev/null || true
            break
        fi
        sleep 0.2
    done
    wait "$wrapper_pid" 2>/dev/null || true
    wrapper_pid=""
}
stop_serve() {
    [ -n "$serve_pid" ] || return 0
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
cleanup() {
    stop_serve
    stop_loop
    "$real_tmux" -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
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
        for log in loop serve; do
            if [ -s "$WORK/$log.log" ]; then
                echo "047: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "047: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "047: harness: unexpected exit $rc" >&2
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
note() { echo "047: $*"; }

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
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$repo"
git -C "$WORK/seed" push -q "$repo" main:main

mkdir -p "$WORK/claude-config" "$WORK/ctl" "$WORK/state"


# The fake engine: a prompt; what is typed into it is one message, written
# after a second of quiet to its session transcript as Claude Code writes a
# prompt. Its turn writes model calls as Claude Code does: an assistant
# line with `message.id`, `message.model` and `message.usage`. A task's
# turn makes one call and ends 3 s after the prompt; a prompt holding
# BB:LONG <n> makes <n> calls, one every 2 s, and then ends the turn
# (`turn_duration`). Call k of a turn uses 1000 + k input tokens and 7
# output tokens. Every prompt, call and turn end is logged (engine.log: ms
# agent what).
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
while [ $# -gt 0 ]; do
  case "$1" in
  --resume | --session-id) native=${2-}; shift ;;
  esac
  shift
done
agent=$SPAWN_AGENT_ID
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$$" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$native" "$1" \
    >>"$transcript"
}
log() { printf '%s %s %s\n' "$(date +%s%3N)" "$agent" "$1" >>"$BB_WORK/engine.log"; }
call() { # <k>: one model call
  line "\"type\":\"assistant\",\"message\":{\"id\":\"msg_${SPAWN_EXECUTION_ID}_$$_$count\",\"model\":\"bb-model\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working $1\"}],\"usage\":{\"input_tokens\":$((1000 + $1)),\"output_tokens\":7,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}"
  log "call"
}
turn() { # <the prompt's text>
  calls=$(printf '%s\n' "$1" | sed -n 's/.*BB:LONG \([0-9][0-9]*\).*/\1/p' | head -n 1)
  if [ -n "$calls" ]; then
    log "long-turn"
    k=1
    while [ "$k" -le "$calls" ]; do
      call "$k"
      k=$((k + 1))
      sleep 2
    done
  else
    call 1
    sleep 3
  fi
  line '"type":"system","subtype":"turn_duration","durationMs":3000'
  log "turn-ended"
  # The spinner is drawn over in place: the screen reader scans the lines
  # above the prompt, and one left there reads busy.
  printf '\r\033[2Kdone\n\n❯ '
}
printf 'Welcome to Claude Code\n\n❯ '
buffer=""
while :; do
  if IFS= read -r -t 1 typed; then
    buffer="${buffer:+$buffer
}$typed"
  elif [ -n "$buffer" ]; then
    raw=$buffer
    buffer=""
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":$(printf '%s' "$raw" | jq -Rs .)}"
    log "prompt"
    printf '\n✻ Working… (esc to interrupt)'
    turn "$raw"
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# Spawn's tmux, on the private socket.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec '$real_tmux' -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

# `spawn loop` as systemd runs spawn-loop.service: started again 5 s after
# it exits (Restart=always, RestartSec=5).
cat >"$WORK/run-loop" <<'EOF'
#!/bin/sh
while [ ! -f "$WORK/loop.stop" ]; do
    "$SPAWN_BIN" loop --repo "$BB_REPO" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    child=$!
    printf '%s\n' "$child" >"$WORK/loop.pid"
    status=0
    wait "$child" || status=$?
    [ ! -f "$WORK/loop.stop" ] || exit 0
    sleep 5
done
EOF

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_GATE SPAWN_DISPATCH \
    SPAWN_SUPERVISOR SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_RELAY http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
BB_REPO=$repo
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
CLAUDE_CONFIG_DIR=$WORK/claude-config
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
BB_SPAWN=$SPAWN_BIN
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_MESSAGE_TAKEN_SECS=6
SPAWN_ALERT_CHECK_SECS=1
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL BB_REPO SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS CLAUDE_CONFIG_DIR BB_PROJECTS BB_WORK BB_SPAWN SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_MESSAGE_TAKEN_SECS SPAWN_ALERT_CHECK_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$WORK/engine.log"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

start_loop() {
    rm -f "$WORK/loop.stop" "$WORK/loop.pid"
    sh "$WORK/run-loop" &
    wrapper_pid=$!
    i=0
    until [ -s "$WORK/loop.pid" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "the loop did not start"
        sleep 0.1
    done
}
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

# --- helpers ----------------------------------------------------------------

now_ms() { date +%s%3N; }
q() { scalar "$1"; }
put() { printf '%s\n' "$3" >"$WORK/state/$1.$2"; }
get() { cat "$WORK/state/$1.$2" 2>/dev/null || true; }
# api <path> <file>: GET from serve; its status code.
api() { curl -s --noproxy '*' -o "$2" -w '%{http_code}' "$base$1" 2>/dev/null || echo 000; }
# commands <label>: the session's commands into $WORK/commands-<label>.json.
commands() {
    code=$(api "/api/fleet/$(get "$1" session)/commands?limit=100" "$WORK/commands-$1.json")
    [ "$code" = 200 ] || harness "GET /api/fleet/$(get "$1" session)/commands answered $code"
    jq -e 'type == "array"' "$WORK/commands-$1.json" >/dev/null 2>&1 ||
        harness "the commands of $1 are no JSON array: $(head -c 200 "$WORK/commands-$1.json")"
}
# m <label> <command id> <jq filter on the message>: from the last commands read.
m() { jq -r --argjson id "$2" "[.[] | select(.command_id == \$id)][0] | $3" "$WORK/commands-$1.json"; }
# send <label> <key> <text>: a person's input; prints its command id.
send() {
    capture "$SPAWN_BIN" submit --principal bb-person --agent "$(get "$1" agent)" \
        --session "$(get "$1" session)" --key "$2" \
        --action "$(jq -cn --arg text "$3" '{type: "input", text: $text}')"
    [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
    id=$(q "select id from spawn_commands where idempotency_key = '$2'")
    [ -n "$id" ] || harness "submit $2 recorded no command"
    echo "$id"
}
# until_message <label> <id> <jq condition> <seconds>: poll until the
# message meets it; 0 when it did.
until_message() {
    deadline=$(($(date +%s) + $4))
    while :; do
        commands "$1"
        [ "$(m "$1" "$2" "(.message // {}) | ($3)")" = true ] && return 0
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
idle() { # the agent's engine ended a turn and has had none since
    awk -v a="$(get "$1" agent)" '$2 == a { last = $3 } END { exit !(last == "turn-ended" || last == "deaf") }' "$WORK/engine.log"
}
terminal_of() { echo "spawn-$(q "select execution_id from agent_executions where session_id = '$(get "$1" session)' and ended_at_ms is null order by started_at_ms desc limit 1")"; }

iso() { date -u -d "@$(($1 / 1000))" +%Y-%m-%dT%H:%M:%SZ; }
# live_total <until ms>: the tokens of the model calls the live transcripts
# hold before it, each message once (its earliest line, its largest usage),
# as the scorecard counts the archive.
live_total() {
    find "$BB_PROJECTS" -name '*.jsonl' -type f -exec cat {} + 2>/dev/null | jq -s --argjson u "$1" '
        [.[] | select(.type == "assistant" and .message.id != null and .message.usage != null)
         | {id: .message.id,
            at: (((.timestamp[0:19] + "Z") | fromdateiso8601) * 1000 + (.timestamp[20:23] | tonumber)),
            tokens: ((.message.usage.input_tokens // 0) + (.message.usage.output_tokens // 0)
                     + (.message.usage.cache_creation_input_tokens // 0)
                     + (.message.usage.cache_read_input_tokens // 0))}]
        | group_by(.id) | map({at: (map(.at) | min), tokens: (map(.tokens) | max)})
        | map(select(.at < $u)) | map(.tokens) | add // 0'
}
# card <until ms>: the scorecard of [start, until) into $WORK/card.json;
# prints "<fleet tokens> <settling>".
card() {
    code=$(api "/api/scorecard?since=$(iso "$since_ms")&until=$(iso "$1")" "$WORK/card.json")
    [ "$code" = 200 ] || harness "GET /api/scorecard answered $code: $(head -c 300 "$WORK/card.json")"
    jq -r '"\([.scopes[] | select(.scope == "fleet")][0].dimensions.cost.tokens // "none") \(if has("settling") then .settling else "none" end)"' "$WORK/card.json"
}
# until_card <until ms> <tokens> <settling> <seconds>: poll until the card
# reads them; 0 when it did. The last reading is in $reading.
until_card() {
    deadline=$(($(date +%s) + $4))
    while :; do
        reading=$(card "$1")
        [ "$reading" = "$2 $3" ] && return 0
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
calls_of() { awk -v a="$(get "$1" agent)" '$2 == a && $3 == "call"' "$WORK/engine.log" | wc -l; }
running() { # the agent's long turn has not ended
    awk -v a="$(get "$1" agent)" '$2 == a { last = $3 } END { exit !(last == "call" || last == "long-turn") }' "$WORK/engine.log"
}
second_now() { echo $((($(date +%s%3N) / 1000) * 1000)); }

# --- the agents ----------------------------------------------------------------

since_ms=$(( ($(date +%s) - 120) * 1000 ))
capture "$SPAWN_BIN" mission add --text "047 settling"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
assign() { # <label>
    capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$1" \
        --brief "Do the 047 task: work as you are asked."
    [ "$status" -eq 0 ] || harness "task assign ($1) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    put "$1" agent "$(q "select agent_id from mission_tasks where id = $task")"
    put "$1" session "$(q "select session_id from mission_tasks where id = $task")"
}
assign long
labels=long

start_loop
start_serve

deadline=$(($(date +%s) + 120))
for label in $labels; do
    until idle "$label"; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            harness "$label's engine did not take its task and end its turn within 120 s"
        sleep 0.5
    done
done
archive=$SPAWN_WORKSPACE_ROOT/.transcripts
deadline=$(($(date +%s) + 20))
until [ -d "$archive/$(get long agent)" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "long's task turn was never archived ($archive/$(get long agent))"
    sleep 0.5
done
note "long took its task"

# --- (1) a running turn is counted within a pass ------------------------------------------------

send long bb-long "BB:LONG 40 (work for a while)" >/dev/null
deadline=$(($(date +%s) + 30))
until [ "$(calls_of long)" -ge 4 ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "long's engine made no 3 calls of its long turn within 30 s"
    sleep 0.5
done
u1=$(second_now)
want=$(live_total "$u1")
until_card "$u1" "$want" false 15 ||
    fail "(1) the window to $(iso "$u1"), while long's turn runs, reads [tokens settling] [$reading] 15 s on; the live transcripts hold $want tokens before it: a model call is counted once a pass has archived it, not when its turn ends"
running long || harness "long's turn ended before (1) was read; it must still be running"
note "(1) a running turn's calls are counted within a pass: $want tokens, settled"

# --- (2) an archive held back: settling ------------------------------------------------------

# Worker `held` starts after U0, its archive path a plain file from the
# start: everything it writes is after U0, and nothing of it can be archived.
u0=$(second_now)
sleep 1.2
want0=$(live_total "$u0")
# The plain file goes first, at the agent id the next task gets, so no
# pass of the Bridge can make the directory before it.
next=$(q "select coalesce(max(id), 0) + 1 from mission_tasks")
printf 'held by the scenario: no archive here\n' >"$archive/agent_worker_held_$next"
assign held
labels="long held"
[ "$(get held agent)" = "agent_worker_held_$next" ] ||
    harness "held's agent is $(get held agent), not agent_worker_held_$next where its archive path was held"
deadline=$(($(date +%s) + 120))
until idle held; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "held's engine did not take its task and end its turn within 120 s"
    sleep 0.5
done
send held bb-held "BB:LONG 12 (work while your archive is held)" >/dev/null
deadline=$(($(date +%s) + 30))
until [ "$(calls_of held)" -ge 4 ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "held's engine made no 3 calls of its long turn within 30 s"
    sleep 0.5
done
u2=$(second_now)
want2=$(live_total "$u2")
sleep 6
for i in 1 2 3; do
    reading=$(card "$u2")
    [ "${reading#* }" = true ] ||
        fail "(2) held's calls before $(iso "$u2") cannot be archived (its archive is held) and the window to it reads [tokens settling] [$reading]; it is settling"
    [ "${reading%% *}" -lt "$want2" ] ||
        harness "the window to $(iso "$u2") reads $reading while held's archive is held; the hold did not hold"
    reading=$(card "$u0")
    [ "$reading" = "$want0 false" ] ||
        fail "(2) the window to $(iso "$u0"), which ended before held's turn began, reads [tokens settling] [$reading] while held's archive is held; it holds $want0 tokens and is settled: only a call before a window's end holds it open"
    sleep 2
done
# held's execution ends, mid-turn, while its archive is still held: what it
# wrote before U2 is not archived, so the window stays open.
capture "$SPAWN_BIN" task cancel --id "$(q "select id from mission_tasks where agent_id = '$(get held agent)'")"
[ "$status" -eq 0 ] || harness "task cancel (held) failed: $(err)"
deadline=$(($(date +%s) + 30))
until [ "$(q "select count(*) from agent_executions where agent_id = '$(get held agent)' and ended_at_ms is null")" = 0 ]; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "held's execution did not end within 30 s of its task's cancel"
    sleep 0.5
done
sleep 3
for i in 1 2 3; do
    reading=$(card "$u2")
    [ "${reading#* }" = true ] ||
        fail "(2) held's execution ended with its calls before $(iso "$u2") unarchived (its archive is held) and the window to it reads [tokens settling] [$reading]; an ended execution with unarchived lines before a window's end keeps it settling"
    [ "${reading%% *}" -lt "$want2" ] ||
        harness "the window to $(iso "$u2") reads $reading after held ended with its archive held; the hold did not hold"
    sleep 2
done
# The loop restarts (as every deploy restarts it) with the path still held:
# what it must still copy is the log's and the disk's, not its memory's.
stop_loop
start_loop
sleep 5
reading=$(card "$u2")
[ "${reading#* }" = true ] ||
    fail "(2) after a restart of the loop, held's lines before $(iso "$u2") still unarchived, the window to it reads [tokens settling] [$reading]; it is settling"
[ -f "$archive/$(get held agent)" ] && [ "$(cat "$archive/$(get held agent)")" = "held by the scenario: no archive here" ] ||
    harness "held's archive path was changed while it was held"
rm -f "$archive/$(get held agent)"
until_card "$u2" "$want2" false 20 ||
    fail "(2) held's archive path is free again, its execution ended before a restart of the loop, and the window to $(iso "$u2") reads [tokens settling] [$reading] 20 s on; the live transcripts hold $want2 tokens before it, settled: an ended execution's copy is tried again until it is made, across a restart"
note "(2) held's archive held: settling, live, after it ended and across a restart; freed: $want2 tokens, settled"

# --- (3) settled stays settled; idle executions settle a window ---------------------------------

reading=$(card "$u1")
[ "$reading" = "$want false" ] ||
    fail "(3) the window to $(iso "$u1"), settled at $want tokens in (1), now reads [$reading]"
deadline=$(($(date +%s) + 120))
until idle long; do
    [ "$(date +%s)" -lt "$deadline" ] || harness "long's long turn did not end within 120 s"
    sleep 0.5
done
sleep 2
u3=$(second_now)
want3=$(live_total "$u3")
until_card "$u3" "$want3" false 15 ||
    fail "(3) long's turn has ended, held's execution too, and the window to $(iso "$u3") reads [tokens settling] [$reading] 15 s on; the live transcripts hold $want3 tokens before it, settled (an idle execution holds no window open)"
note "(3) a settled window stays settled; with every turn ended a window settles: $want3 tokens"

# --- (4) the page's words ----------------------------------------------------------------

[ -f "$tree/web/src/settling.ts" ] || fail "(4) there is no web/src/settling.ts"
cp "$WORK/card.json" "$WORK/card-settled.json"
cat >"$WORK/settling.test.mjs" <<'TEST'
// The scorecard's settling, as web/src/settling.ts's settlingNote says it.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { settlingNote } = await import(pathToFileURL(`${env.BB_TREE}/web/src/settling.ts`).href);

test("settled: no note; settling: the note", () => {
  assert.equal(settlingNote({ settling: false, until_ms: 0 }), null);
  assert.equal(
    settlingNote({ settling: true, until_ms: 0 }),
    "Still settling: a turn that was running at the window's end may add to its cost.",
  );
});

test("the scenario's own card", () => {
  const card = JSON.parse(readFileSync(`${env.BB_WORK}/card-settled.json`, "utf8"));
  assert.equal(settlingNote(card), null);
  assert.equal(settlingNote({ ...card, settling: true }) !== null, true);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/settling.test.mjs" >"$WORK/settling.out" 2>&1; then
    if grep -q "ERR_MODULE_NOT_FOUND\|SyntaxError\|ERR_UNKNOWN_FILE_EXTENSION\|ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX" "$WORK/settling.out"; then
        fail "(4) web/src/settling.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/settling.out" | tr '\n' ' ')"
    fi
    fail "(4) the page's words: $(grep -E 'AssertionError|expected|actual|is not a function' "$WORK/settling.out" | head -n 6 | tr '\n' ' ')"
fi
note "(4) the page says a window is settling"

note "a running turn's cost is counted within a pass; a window says it is settling while the archive may lack a call before its end"
exit 0
