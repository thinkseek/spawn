#!/bin/sh
# Objective 205 [observability][storage]: a checkpoint that cannot commit is
# reported once per state change, and a branch deleted under its mounted
# workspace is made again from the workspace, recorded.
# Live since 2026-11-15 (incident I-006): agent_supervisor's branch was
# deleted from ws/.sources/scratch.git while its workspace was mounted. Every
# pass of the Bridge retried the turn-end checkpoint, held its transcript
# line back, and logged "commit unavailable: git rev-parse
# refs/heads/agent_supervisor: fatal: ambiguous argument ..." again: about 4
# a second, 14,000 an hour, three journal lines each, and the supervisor's
# transcript was not followed past that line.
# Required: (a) a checkpoint that finds the branch gone makes it again from
# the workspace: in a mission's mirror, taken back first from what the host
# published (the source's spawn/<agent>, fetched without force), so the
# checkpoint goes on top of it; with nothing published, the git store makes
# it at a commit of the workspace's tree on the store's main (no main: no
# parent), creating the ref only while it is absent. The Bridge records
# `artifact.branch_recreated {artifact, parent}` and then the checkpoint's
# `artifact.committed`, logs one line, and the branch is published again.
# (b) a commit that fails is retried every pass as before, but logged when
# it starts failing or fails for another cause (its kind and its message's
# first line, ids masked), and once ("commit available again") when a
# commit gets through after it.
#
# The world: a real `spawn loop` runs one ephemeral worker on a fake engine;
# its mission's source is a scratch repository, which receives the agents'
# branches (spawn/<agent>); a prompt holding BB:WRITE <name> makes the
# engine write <name> into its workspace before its turn ends.
# Checked:
#   (1) one.txt's checkpoint published, the worker's branch deleted from the
#       mirror under its mounted workspace, a turn that writes two.txt:
#       within 20 s the branch is back holding two.txt; the newest
#       artifact.branch_recreated names a commit that holds one.txt, on the
#       published head or a later published commit, never main (or that head
#       itself, taken back before
#       two.txt was written: a checkpoint can come first under load); the
#       branch's head is that commit or descends from it (a later
#       checkpoint may be on top); an
#       artifact.committed of it; the source's spawn/<agent> reaches it; the
#       loop logged one line saying the branch was made again and no
#       "commit unavailable". (3) and (4) read the made-again commit the
#       same way, from its record, never from the branch's head.
#   (2) a turn after it (three.txt): an ordinary checkpoint, descending
#       from the made-again commit, and no new artifact.branch_recreated.
#   (2b) three.txt's checkpoint published, the branch deleted, a turn that
#       writes nothing new: within 20 s the branch is back at the published
#       head, and one more artifact.branch_recreated names that head as both
#       its commit and its parent (taken back with nothing new).
#   (2c) the source moved away and an empty directory in its place (a
#       directory git cannot read), the branch deleted, a turn (eight.txt):
#       once the line is logged (30 s at most) and 3 s more, the branch is
#       still absent (never made again on main
#       beside what was published), the loop logged one "commit
#       unavailable" saying it could not take the branch back from what
#       was published, and a source_away alert is fired (the directory is
#       there: only git's reading finds it away). The empty directory
#       removed (the source not a directory now): once logged and 3 s more,
#       still absent, one more line, saying so. The source back: the branch is made again
#       on the published head, as in (1) (objective 205 R5).
#   (2d) the source's spawn/<agent> written by hand as an id it has no
#       object for (ls-remote lists it, the fetch fails), the branch
#       deleted, a turn (ten.txt): once logged and 3 s more, the branch is
#       still absent, one
#       more "commit unavailable" saying it could not be taken back, and a
#       new source_away alert (the failed fetch marks the source unreadable;
#       ls-remote alone would not); the ref mended: taken back on what was
#       published (objective 205 R5).
#   (3) nothing published (the source's spawn/<agent> deleted), the branch
#       deleted, a turn (four.txt): made again on main, recorded with main as
#       its parent, and published.
#   (4) nothing published and no main (main deleted in the source and the
#       mirror), the branch deleted, a turn (six.txt): made again at a
#       commit with no parent, recorded with a null parent, and published.
#   (5) the mirror's object store made unwritable, a turn (five.txt): 12 s
#       after the turn ended, the loop has logged exactly one "commit
#       unavailable" line (a pass is well under a second); a5.txt written
#       into the workspace (the same cause at another file): still one;
#       then the store writable but the branch's ref locked (another cause,
#       a fresh commit id at each pass): 8 s later, exactly two.
#   (6) the lock gone: within 20 s the branch holds five.txt, an
#       artifact.committed of it is recorded, and the loop has logged one
#       "commit available again" for the worker's session; still two "commit
#       unavailable".
#   (7) the branch's ref written by hand as a blob's id, a turn: 10 s
#       later the ref is as written (neither the take-back nor the re-create
#       writes over it) and one more "commit unavailable".
#   (8) the log made to refuse artifact.branch_recreated (a constraint on
#       this scenario's own database), the branch deleted ((7)'s checkpoint
#       is still due, so no turn): the branch is made again with seven.txt
#       and nothing is recorded (the session's records wait behind it); the
#       constraint dropped: within 10 s an artifact.branch_recreated names
#       the made-again commit and an artifact.committed of it is recorded
#       (objective 205 R4: neither record is lost when the first append
#       fails).
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure. Any unexpected error exits 2, never 1.
#
# Fails with 1 at (1) on main b7e3d31f: the branch is not made again.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN,
# SPAWN_TEST_DATABASE_URL, tmux, jq, python3 and git; creates and drops its
# own database, uses a private tmux socket, loopback only. Every wait is
# bounded in time. Not run as root: (4) relies on file modes.

set -eu

asserted=""
harness() {
    echo "205: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk python3 base64 cmp; do
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
SPAWN_DB_NAME="bb_205_branch_$run"
SPAWN_TMUX_SOCKET="bb-205-$run"
export SPAWN_TERMINAL_BACKEND=tmux
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
# _stop_runner ends the loop's runner and its loop; cleanup calls it by this
# name, which no step defines, so a step's helper can never take its place
# (a shadowing stop_loop let the runner outlive the script, 2026-11-16).
# stop_loop, the name the end calls, is its public form.
_stop_runner() {
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
stop_loop() { _stop_runner; }
stop_serve() {
    [ -n "$serve_pid" ] || return 0
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
cleanup() {
    # The private tmux server first: it daemonizes out of the script's
    # session, so nothing after a failure below may stand between it and
    # its end.
    "$real_tmux" -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    stop_serve
    _stop_runner
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
                echo "205: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "205: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "205: harness: unexpected exit $rc" >&2
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
note() { echo "205: $*"; }

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



mkdir -p "$WORK/claude-config" "$WORK/ctl" "$WORK/state"

# The fake engine: a prompt; what is typed into it is one message, written
# after a second of quiet to its session transcript as Claude Code writes a
# prompt. A turn writes one assistant line and ends 2 s after the prompt; a
# prompt holding BB:WRITE <name> first writes <name> (its name as its text)
# into the engine's directory, the agent's workspace. Every prompt and turn
# end is logged (engine.log: ms agent what).
bb_engine >"$WORK/claude" <<'ENGINE'
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
    "$SPAWN_EXECUTION_ID" "$$" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$native" "$1" >>"$transcript"
}
log() { printf '%s %s %s\n' "$(date +%s%3N)" "$agent" "$1" >>"$BB_WORK/engine.log"; }
turn() { # <the prompt's text>
  case $1 in
  *BB:WRITE\ *)
    name=${1#*BB:WRITE }
    name=${name%%[!A-Za-z0-9.]*}
    printf '%s\n' "$name" >"$PWD/$name"
    ;;
  esac
  line "\"type\":\"assistant\",\"message\":{\"id\":\"msg_${SPAWN_EXECUTION_ID}_$$_$count\",\"model\":\"bb-model\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}"
  sleep 2
  line '"type":"system","subtype":"turn_duration","durationMs":2000'
  log "turn-ended"
  printf '\r\033[2Kdone\n\n❯ '
}
printf 'Welcome to Claude Code\n\n❯ '
buffer=""
while :; do
  if bb_read_line typed; then
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

# The transcript archive's location tool (175): a stand-in that lists no
# credential, so no pass reads the account's real files.
cat >"$WORK/locations.py" <<'PY'
import json
print(json.dumps({"sources": [], "entries": [], "missing": [], "unreadable": [], "accepted": [],
                  "accepted_paths": [], "watch": [], "probe": []}))
PY
SPAWN_ARCHIVE_LOCATIONS_TOOL=$WORK/locations.py
SPAWN_GATE_SANDBOX=1
export SPAWN_ARCHIVE_LOCATIONS_TOOL SPAWN_GATE_SANDBOX

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

# --- helpers ----------------------------------------------------------------

q() { scalar "$1"; }
put() { printf '%s\n' "$3" >"$WORK/state/$1.$2"; }
get() { cat "$WORK/state/$1.$2" 2>/dev/null || true; }
# send <label> <key> <text>: a person's input.
send() {
    capture "$SPAWN_BIN" submit --principal bb-person --agent "$(get "$1" agent)" \
        --session "$(get "$1" session)" --key "$2" \
        --action "$(jq -cn --arg text "$3" '{type: "input", text: $text}')"
    [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
}
ended() { # <label>: how many turns the agent's engine has ended
    awk -v a="$(get "$1" agent)" '$2 == a && $3 == "turn-ended"' "$WORK/engine.log" | wc -l
}
# until_ended <label> <n> <seconds>: until the engine has ended n turns.
until_ended() {
    deadline=$(($(date +%s) + $3))
    until [ "$(ended "$1")" -ge "$2" ]; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
archive=$SPAWN_WORKSPACE_ROOT/.transcripts
archived() { # every archived byte of the agent, copies in order
    find "$archive/$(get "$1" agent)" -name '*.jsonl' -type f 2>/dev/null | sort | xargs -r cat
}

# --- the agent -----------------------------------------------------------------

# A mission with a source: its mirror under the root holds main. (With none,
# the store is an empty scratch repository, as agent_supervisor's is live.)
capture "$SPAWN_BIN" mission add --text "205 branch made again" --source "$repo"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker writer \
    --brief "Do the 205 task: work as you are asked."
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(q "select max(id) from mission_tasks where mission_id = $mission")
agent=$(q "select agent_id from mission_tasks where id = $task")
session=$(q "select session_id from mission_tasks where id = $task")
put writer agent "$agent"
put writer session "$session"
ref=refs/heads/$agent

start_loop
until_ended writer 1 120 || harness "the engine did not take its task and end its turn within 120 s"
send writer bb-write-1 "BB:WRITE one.txt"
until_ended writer 2 60 || harness "the one.txt turn did not end within 60 s"
# head_holds <file>: the branch's head holds it, with its name as its text.
head_holds() { [ "$(git --git-dir="$store" show "$ref:$1" 2>/dev/null)" = "$1" ]; }
wait_head_holds() { # <file> <seconds>
    limit=$(($(date +%s) + $2))
    until head_holds "$1"; do
        [ "$(date +%s)" -lt "$limit" ] || return 1
        sleep 0.5
    done
}
# The workspace store: the mission's mirror under the root, the one that
# holds the worker's branch.
store=""
limit=$(($(date +%s) + 20))
while [ -z "$store" ]; do
    for candidate in "$SPAWN_WORKSPACE_ROOT"/.sources/*.git; do
        if git --git-dir="$candidate" rev-parse --verify --quiet "$ref" >/dev/null 2>&1; then store=$candidate; fi
    done
    [ -n "$store" ] && break
    [ "$(date +%s)" -lt "$limit" ] || harness "no repository under $SPAWN_WORKSPACE_ROOT/.sources holds $ref 20 s after the turn"
    sleep 0.5
done
git --git-dir="$store" rev-parse --verify --quiet refs/heads/main >/dev/null || harness "the store $store has no main"
wait_head_holds one.txt 20 || harness "the worker's first checkpoint (one.txt) is not on its branch 20 s after the turn"
note "the worker checkpointed one.txt"

events() { # <name>: how many of the worker's events of that name
    q "select count(*) from spawn_events where event_name = '$1' and agent_id = '$agent'"
}
committed_of() { # <commit>: how many artifact.committed name it
    q "select count(*) from spawn_events where event_name = 'artifact.committed'
        and agent_id = '$agent' and kind -> 'detail' -> 'artifact' ->> 'commit' = '$1'"
}
logged() { grep -cF -- "$1" "$WORK/loop.log" || true; }

published() { git --git-dir="$repo" rev-parse --verify --quiet "refs/heads/spawn/$agent" 2>/dev/null || true; }
wait_published() { # <commit> <seconds>: until the source's spawn/<agent> is that commit
    limit=$(($(date +%s) + $2))
    until [ "$(published)" = "$1" ]; do
        [ "$(date +%s)" -lt "$limit" ] || return 1
        sleep 0.5
    done
}
# newest_recreated: the commit the newest artifact.branch_recreated of the
# worker names (empty: none).
newest_recreated() {
    q "select kind -> 'detail' -> 'artifact' ->> 'commit' from spawn_events
        where event_name = 'artifact.branch_recreated' and agent_id = '$agent' order by id desc limit 1"
}
# wait_recreated <count> <seconds>: until the worker has that many
# artifact.branch_recreated.
wait_recreated() {
    limit=$(($(date +%s) + $2))
    until [ "$(events artifact.branch_recreated)" -ge "$1" ]; do
        [ "$(date +%s)" -lt "$limit" ] || return 1
        sleep 0.5
    done
}
# descends <ancestor> <commit> <git dir>: <commit> is <ancestor> or after it.
descends() { git --git-dir="$3" merge-base --is-ancestor "$1" "$2" 2>/dev/null; }
# wait_published_from <commit> <seconds>: until the source's spawn/<agent>
# is that commit or descends from it.
wait_published_from() {
    limit=$(($(date +%s) + $2))
    until [ -n "$(published)" ] && descends "$1" "$(published)" "$repo"; do
        [ "$(date +%s)" -lt "$limit" ] || return 1
        sleep 0.5
    done
}
recreated_of() { # <commit> <parent or null>: artifact.branch_recreated naming them
    if [ "$2" = null ]; then parent="kind -> 'detail' -> 'parent' = 'null'::jsonb"; else parent="kind -> 'detail' ->> 'parent' = '$2'"; fi
    q "select count(*) from spawn_events where event_name = 'artifact.branch_recreated'
        and agent_id = '$agent' and kind -> 'detail' -> 'artifact' ->> 'commit' = '$1' and $parent"
}
# made_again <step> <file> <expected parent, or null> <records before>:
# after a turn that wrote <file>, the branch is back holding it; the made-again
# commit is the one the newest artifact.branch_recreated names (one more than
# before), never the branch's head, which a later checkpoint may have moved
# on (rev-f's F1 on 205 R2). That commit holds one.txt (the workspace's), is
# on that parent, has an artifact.committed; the branch's head holds <file>
# and is that commit or descends from it; the source's spawn/<agent> reaches
# it (published). <file> need not be in the made-again commit itself: under
# load a checkpoint can run between the deletion and the engine's write and
# make the branch again before <file> exists (rev-g's R3 run under load);
# <file> then comes in the next, ordinary checkpoint. Sets $made.
made_again() {
    wait_head_holds "$2" 20 ||
        fail "($1) the branch deleted under its mounted workspace is not made again with $2 20 s after the turn (ref: $(git --git-dir="$store" rev-parse --verify --quiet "$ref" || echo absent)); the loop logged $(logged 'commit unavailable') commit unavailable"
    wait_recreated $(($4 + 1)) 10 ||
        fail "($1) no new artifact.branch_recreated of the worker 10 s after the branch holds $2 ($(events artifact.branch_recreated) in all, $4 before)"
    made=$(newest_recreated)
    [ "$(git --git-dir="$store" show "$made:one.txt" 2>/dev/null)" = one.txt ] ||
        fail "($1) the made-again commit $made lacks one.txt, which the workspace holds"
    if [ "$3" = null ]; then
        [ "$(git --git-dir="$store" rev-list --parents -n 1 "$made")" = "$made" ] ||
            fail "($1) the made-again commit has a parent: $(git --git-dir="$store" rev-list --parents -n 1 "$made")"
    elif [ "$made" != "$3" ]; then
        # (The published head itself is a take-back with nothing new yet.)
        # Its parent is <expected parent> or a later commit on top of it: a
        # checkpoint can still be published between the scenario's read of
        # the published head and the deletion (rev-g's R4 run under load).
        # A branch made again on main instead would not descend from it.
        parent=$(git --git-dir="$store" rev-parse "$made^" 2>/dev/null || true)
        [ -n "$parent" ] && descends "$3" "$parent" "$store" ||
            fail "($1) the made-again commit's parent is neither $3 nor after it: $(git --git-dir="$store" rev-list --parents -n 1 "$made") (the store's main: $(git --git-dir="$store" rev-parse --verify --quiet refs/heads/main || echo none); the source's spawn/$agent: $(published))"
    fi
    descends "$made" "$ref" "$store" ||
        fail "($1) the branch's head $(git --git-dir="$store" rev-parse "$ref") does not descend from the made-again commit $made"
    # The record's parent: none, or the made-again commit's own parent, or
    # (taken back with nothing new) the made-again commit itself.
    if [ "$3" = null ]; then
        [ "$(recreated_of "$made" null)" -eq 1 ] ||
            fail "($1) no artifact.branch_recreated names $made with no parent ($(events artifact.branch_recreated) of the worker in all)"
    else
        [ "$(recreated_of "$made" "$(git --git-dir="$store" rev-parse "$made^" 2>/dev/null)")" -eq 1 ] ||
            [ "$(recreated_of "$made" "$made")" -eq 1 ] ||
            fail "($1) no artifact.branch_recreated names $made with its parent ($(events artifact.branch_recreated) of the worker in all)"
    fi
    limit=$(($(date +%s) + 10))
    until [ "$(committed_of "$made")" -ge 1 ] || [ "$(date +%s)" -ge "$limit" ]; do sleep 0.5; done
    [ "$(committed_of "$made")" -eq 1 ] || fail "($1) no artifact.committed names the made-again commit $made"
    wait_published_from "$made" 20 ||
        fail "($1) the made-again branch is not published: the source's spawn/$agent is $(published), which does not reach $made, 20 s later"
}

# --- (1) deleted under its workspace, published: taken back, on what was pushed ---

wait_published "$(git --git-dir="$store" rev-parse "$ref")" 30 ||
    harness "the worker's one.txt checkpoint was not published to the source within 30 s"
pushed=$(published)
git --git-dir="$store" update-ref -d "$ref" || harness "cannot delete $ref"
send writer bb-write-2 "BB:WRITE two.txt"
until_ended writer 3 60 || harness "the two.txt turn did not end within 60 s"
made_again 1 two.txt "$pushed" 0
[ "$(events artifact.branch_recreated)" -eq 1 ] ||
    fail "(1) the store holds $(events artifact.branch_recreated) artifact.branch_recreated of the worker, not one"
[ "$(logged 'commit unavailable')" -eq 0 ] ||
    fail "(1) the loop logged $(logged 'commit unavailable') commit unavailable for a branch it could make again"
[ "$(grep -F -- "$session" "$WORK/loop.log" | grep -cF -- "branch $agent was gone")" -eq 1 ] ||
    fail "(1) the loop did not log once that branch $agent was gone and made again: $(grep -F 'was gone' "$WORK/loop.log" | head -n 3)"
note "(1) published: taken back on the pushed head, recorded, published again, logged once"

# --- (2) after it, an ordinary checkpoint --------------------------------------

first=$made
send writer bb-write-3 "BB:WRITE three.txt"
until_ended writer 4 60 || harness "the three.txt turn did not end within 60 s"
wait_head_holds three.txt 20 || fail "(2) three.txt is not on the branch 20 s after the turn"
[ "$(git --git-dir="$store" rev-parse "$ref")" != "$first" ] && descends "$first" "$ref" "$store" ||
    fail "(2) the checkpoint after the made-again commit is not on it"
[ "$(events artifact.branch_recreated)" -eq 1 ] ||
    fail "(2) an ordinary checkpoint recorded artifact.branch_recreated ($(events artifact.branch_recreated))"
note "(2) the next checkpoint is an ordinary one"

# --- (2b) taken back with nothing new: the published head again, recorded ------

wait_published "$(git --git-dir="$store" rev-parse "$ref")" 30 ||
    harness "the three.txt checkpoint was not published within 30 s"
again=$(published)
git --git-dir="$store" update-ref -d "$ref" || harness "cannot delete $ref"
send writer bb-nothing "nothing to write this turn"
until_ended writer 5 60 || harness "the turn with nothing to write did not end within 60 s"
wait_recreated 2 20 ||
    fail "(2b) the branch taken back with nothing new is not recorded: $(events artifact.branch_recreated) artifact.branch_recreated of the worker 20 s after the turn, not two (ref: $(git --git-dir="$store" rev-parse --verify --quiet "$ref" || echo absent))"
[ "$(recreated_of "$again" "$again")" -eq 1 ] ||
    fail "(2b) no artifact.branch_recreated names the published head $again as both its commit and its parent: the newest names $(newest_recreated)"
descends "$again" "$ref" "$store" ||
    fail "(2b) the branch is not back on the published head $again: $(git --git-dir="$store" rev-parse --verify --quiet "$ref" || echo absent)"
note "(2b) taken back with nothing new: the published head again, recorded"

# --- (2c) a source that cannot be read: the checkpoint waits, raised; then taken back ---

source_away() { # how many source_away alerts were fired
    q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'alert.fired'
        and kind -> 'detail' -> 'record' ->> 'kind' = 'source_away'"
}
before2c=$(logged 'commit unavailable')
again2c=$(published)
mv "$repo" "$repo.away" || harness "cannot move the source away"
mkdir "$repo" || harness "cannot make an empty directory at the source"
git --git-dir="$store" update-ref -d "$ref" || harness "cannot delete $ref"
send writer bb-write-8 "BB:WRITE eight.txt"
until_ended writer 6 60 || harness "the eight.txt turn did not end within 60 s"
# settle <before> <n>: until the loop has logged <n> commit unavailable since
# <before> (30 s at most, the loop's passes are slower under load), then 3 s
# more, so a line logged on every pass would show; and until a source_away
# alert is fired, if <alerts> is given (20 s at most).
settle() { # <before> <n> [<alerts at least>]
    limit=$(($(date +%s) + 30))
    until [ "$(($(logged 'commit unavailable') - $1))" -ge "$2" ] || [ "$(date +%s)" -ge "$limit" ]; do sleep 0.5; done
    if [ -n "${3-}" ]; then
        limit=$(($(date +%s) + 20))
        until [ "$(source_away)" -ge "$3" ] || [ "$(date +%s)" -ge "$limit" ]; do sleep 0.5; done
    fi
    sleep 3
}
settle "$before2c" 1 1
still_absent() { # <step>
    [ -z "$(git --git-dir="$store" rev-parse --verify --quiet "$ref")" ] ||
        fail "($1) with the source unreadable, the branch was made again (at $(git --git-dir="$store" rev-parse "$ref"), on $(git --git-dir="$store" rev-parse "$ref^" 2>/dev/null || echo nothing)) beside what was published"
}
last_unavailable() { grep -F 'commit unavailable' "$WORK/loop.log" | tail -n 1 | cut -c1-300; }
still_absent 2c
[ "$(($(logged 'commit unavailable') - before2c))" -eq 1 ] ||
    fail "(2c) with an empty directory at the source, the loop logged $(($(logged 'commit unavailable') - before2c)) commit unavailable, not one"
last_unavailable | grep -qF 'back from what was published' ||
    fail "(2c) the commit unavailable line does not say the branch could not be taken back: $(last_unavailable)"
[ "$(source_away)" -ge 1 ] ||
    fail "(2c) no source_away alert 20 s after the source became a directory git cannot read (alert checks every second)"
rmdir "$repo" || harness "cannot remove the empty directory at the source"
settle "$before2c" 2
still_absent 2c
[ "$(($(logged 'commit unavailable') - before2c))" -eq 2 ] ||
    fail "(2c) with the source not a directory, the loop logged $(($(logged 'commit unavailable') - before2c)) commit unavailable in all, not two"
last_unavailable | grep -qF 'the source is not a directory now' ||
    fail "(2c) the second commit unavailable line does not say the source is not a directory now: $(last_unavailable)"
mv "$repo.away" "$repo" || harness "cannot move the source back"
made_again 2c eight.txt "$again2c" 2
note "(2c) a source away: the branch waited (one line, source_away raised), then was taken back on what was published"

# --- (2d) listed but not fetched: the checkpoint waits, raised; then taken back ---

# The source's spawn/<agent> written by hand as an id the source has no
# object for: `git ls-remote` lists it, the take-back's fetch fails.
before2d=$(logged 'commit unavailable')
away2d=$(source_away)
again2d=$(published)
[ -n "$again2d" ] || harness "(2d) nothing published to break"
git --git-dir="$repo" update-ref -d "refs/heads/spawn/$agent" || harness "cannot delete the source's spawn/$agent"
mkdir -p "$repo/refs/heads/spawn"
printf '1111111111111111111111111111111111111111\n' >"$repo/refs/heads/spawn/$agent"
git --git-dir="$store" update-ref -d "$ref" || harness "cannot delete $ref"
send writer bb-write-10 "BB:WRITE ten.txt"
until_ended writer 7 60 || harness "the ten.txt turn did not end within 60 s"
settle "$before2d" 1 $((away2d + 1))
still_absent 2d
[ "$(($(logged 'commit unavailable') - before2d))" -eq 1 ] ||
    fail "(2d) with the published ref unfetchable, the loop logged $(($(logged 'commit unavailable') - before2d)) commit unavailable, not one"
last_unavailable | grep -qF 'back from what was published' ||
    fail "(2d) the commit unavailable line does not say the branch could not be taken back: $(last_unavailable)"
[ "$(source_away)" -gt "$away2d" ] ||
    fail "(2d) no new source_away alert 20 s after the take-back's fetch failed (ls-remote still lists the ref; alert checks every second)"
printf '%s\n' "$again2d" >"$repo/refs/heads/spawn/$agent"
made_again 2d ten.txt "$again2d" 3
note "(2d) listed but not fetched: the branch waited (one line, source_away raised), then was taken back on what was published"

# --- (3) nothing published: made again on main ----------------------------------

wait_published "$(git --git-dir="$store" rev-parse "$ref")" 30 ||
    harness "the three.txt checkpoint was not published within 30 s"
main=$(git --git-dir="$store" rev-parse refs/heads/main)
git --git-dir="$repo" update-ref -d "refs/heads/spawn/$agent" || harness "cannot delete the source's spawn/$agent"
git --git-dir="$store" update-ref -d "$ref" || harness "cannot delete $ref"
send writer bb-write-4 "BB:WRITE four.txt"
until_ended writer 8 60 || harness "the four.txt turn did not end within 60 s"
made_again 3 four.txt "$main" 4
[ "$(events artifact.branch_recreated)" -eq 5 ] ||
    fail "(3) the store holds $(events artifact.branch_recreated) artifact.branch_recreated, not five"
note "(3) nothing published: made again on main, recorded, published"

# --- (4) nothing published and no main: a commit with no parent --------------------

git --git-dir="$repo" update-ref -d "refs/heads/spawn/$agent" || harness "cannot delete the source's spawn/$agent"
# main deleted in the source too, so no refresh of the mirror brings it back.
git --git-dir="$repo" update-ref -d refs/heads/main || harness "cannot delete the source's main"
git --git-dir="$store" update-ref -d refs/heads/main || harness "cannot delete main"
git --git-dir="$store" update-ref -d "$ref" || harness "cannot delete $ref"
send writer bb-write-6 "BB:WRITE six.txt"
until_ended writer 9 60 || harness "the six.txt turn did not end within 60 s"
made_again 4 six.txt null 5
[ "$(events artifact.branch_recreated)" -eq 6 ] ||
    fail "(4) the store holds $(events artifact.branch_recreated) artifact.branch_recreated, not six"
note "(4) nothing published, no main: made again with no parent"

# --- (5) a store that stays unavailable is logged once --------------------------

before=$(logged 'commit unavailable')
again_before=$(grep -F -- "$session" "$WORK/loop.log" | grep -cF 'commit available again' || true)
chmod -R a-w "$store/objects"
send writer bb-write-5 "BB:WRITE five.txt"
until_ended writer 10 60 || harness "the five.txt turn did not end within 60 s"
sleep 12
head_holds five.txt && harness "the object store made unwritable still took five.txt (run as root?)"
failing=$(($(logged 'commit unavailable') - before))
[ "$failing" -eq 1 ] ||
    fail "(5) with the object store unwritable, the loop logged $failing commit unavailable in 12 s, not one: $(grep -F 'commit unavailable' "$WORK/loop.log" | tail -n 2 | cut -c1-240)"
# The same cause at another file: a file the engine might write, written
# into the workspace (it sorts first, so git add now fails at it: the same
# first line, other later lines). Still one line.
printf 'a5.txt\n' >"$SPAWN_WORKSPACE_ROOT/$agent/a5.txt" || harness "cannot write into the workspace"
sleep 8
failing=$(($(logged 'commit unavailable') - before))
[ "$failing" -eq 1 ] ||
    fail "(5) the same cause at another file: the loop logged $failing commit unavailable in all, not one: $(grep -F 'commit unavailable' "$WORK/loop.log" | tail -n 2 | cut -c1-240)"
# A second cause: the object store writable again, but the branch's ref
# locked (a lock file left behind), so update-ref fails at each pass,
# naming the pass's fresh commit (the commit-tree's id changes each second).
: >"$store/refs/heads/$agent.lock"
chmod -R u+w "$store/objects"
sleep 8
failing=$(($(logged 'commit unavailable') - before))
[ "$failing" -eq 2 ] ||
    fail "(5) a second cause (the branch's ref locked, a fresh commit id each pass): the loop logged $failing commit unavailable in all, not two: $(grep -F 'commit unavailable' "$WORK/loop.log" | tail -n 3 | cut -c1-240)"
note "(5) unavailable: logged once per cause"

# --- (6) available again: the commit, and one line saying so --------------------

rm -f "$store/refs/heads/$agent.lock"
wait_head_holds five.txt 20 || fail "(6) writable again, five.txt is not on the branch 20 s later"
sleep 2
[ "$(committed_of "$(git --git-dir="$store" rev-parse "$ref")")" -eq 1 ] ||
    fail "(6) no artifact.committed names the checkpoint that holds five.txt"
[ "$(($(grep -F -- "$session" "$WORK/loop.log" | grep -cF 'commit available again' || true) - again_before))" -eq 1 ] ||
    fail "(6) the loop logged $(($(grep -F -- "$session" "$WORK/loop.log" | grep -cF 'commit available again' || true) - again_before)) commit available again for the session since (5), not one"
[ "$(($(logged 'commit unavailable') - before))" -eq 2 ] ||
    fail "(6) commit unavailable was logged again after the store came back"
note "(6) available again: committed, logged once"

# --- (7) a branch ref that is no commit is never overwritten ---------------------

# What the branch's ref holds, written by hand as a blob's id (as a broken
# tool might): neither the take-back from what was published nor the store's
# re-create may write over it; the commit fails, once.
blob=$(printf 'not a commit\n' | git --git-dir="$store" hash-object -w --stdin)
before7=$(logged 'commit unavailable')
git --git-dir="$store" pack-refs --all >/dev/null 2>&1 || true
printf '%s\n' "$blob" >"$store/refs/heads/$agent"
printf 'seven.txt\n' >"$SPAWN_WORKSPACE_ROOT/$agent/seven.txt"
send writer bb-write-7 "BB:WRITE seven.txt"
until_ended writer 11 60 || harness "the seven.txt turn did not end within 60 s"
sleep 10
[ "$(cat "$store/refs/heads/$agent")" = "$blob" ] ||
    fail "(7) the branch's ref, a blob's id written by hand, was written over: $(cat "$store/refs/heads/$agent")"
[ "$(($(logged 'commit unavailable') - before7))" -eq 1 ] ||
    fail "(7) the loop logged $(($(logged 'commit unavailable') - before7)) commit unavailable for a ref that is no commit, not one"
note "(7) a ref that is no commit: never written over, logged once"

# --- (8) the log refuses the made-again record: neither record is lost ----------

# A constraint on this scenario's own database refuses every
# artifact.branch_recreated while the branch is made again. The branch moves
# first, so its two records wait and are appended, in order, once the log
# takes them again (objective 205 R4, rev-e's F1 on R3: R3 lost the
# artifact.committed when the first append failed).
# The constraint first: (7)'s checkpoint is still due, so the pass after the
# delete makes the branch again at once.
before8=$(events artifact.branch_recreated)
psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "alter table spawn_events add constraint scenario_205_refuses_recreated
    check (event_name is distinct from 'artifact.branch_recreated') not valid" >/dev/null ||
    harness "cannot make the log refuse artifact.branch_recreated"
git --git-dir="$store" update-ref -d "$ref" || harness "cannot delete $ref, which holds a blob's id"
# No turn: the session's records wait behind the refused one, its turn ends
# too, so the scenario waits on the branch, not on a recorded turn.
wait_head_holds seven.txt 20 ||
    fail "(8) the branch deleted under its workspace is not made again with seven.txt 20 s after its delete, with the log refusing artifact.branch_recreated"
sleep 3
[ "$(events artifact.branch_recreated)" -eq "$before8" ] ||
    harness "(8) the log took an artifact.branch_recreated while the constraint refused it"
psql -d "$SPAWN_DB_NAME" -qtA -v ON_ERROR_STOP=1 -c "alter table spawn_events drop constraint scenario_205_refuses_recreated" >/dev/null ||
    harness "cannot let the log take artifact.branch_recreated again"
wait_recreated $((before8 + 1)) 10 ||
    fail "(8) no artifact.branch_recreated 10 s after the log takes it again: the made-again record was lost"
made=$(newest_recreated)
descends "$made" "$ref" "$store" ||
    fail "(8) the branch's head $(git --git-dir="$store" rev-parse "$ref") does not descend from the recorded made-again commit $made"
limit=$(($(date +%s) + 10))
until [ "$(committed_of "$made")" -ge 1 ] || [ "$(date +%s)" -ge "$limit" ]; do sleep 0.5; done
[ "$(committed_of "$made")" -eq 1 ] ||
    fail "(8) no artifact.committed names the made-again commit $made 10 s after the log takes records again: lost while its artifact.branch_recreated was refused"
note "(8) the log refused the made-again record: both records appended once it took them again"

# Nothing this script started outlives it: the loop's runner is ended here,
# and no background job of this shell is left. Not $(jobs -p): a command
# substitution is a subshell, which holds no jobs under dash. dash also
# lists a job that has ended until it is reported, so only a listed pid
# still alive counts. The runner's own pid is checked too: stop_loop clears
# it.
stop_loop
jobs -p >"$WORK/jobs.left"
alive=""
while IFS= read -r job; do
    if [ -n "$job" ] && kill -0 "$job" 2>/dev/null; then alive="$alive $job"; fi
done <"$WORK/jobs.left"
if [ -n "$wrapper_pid" ] && kill -0 "$wrapper_pid" 2>/dev/null; then alive="$alive runner $wrapper_pid"; fi
[ -z "$alive" ] || fail "a background job outlives the scenario:$alive"
echo "205: a deleted branch is made again and recorded; a failing commit is logged once per change"
exit 0
