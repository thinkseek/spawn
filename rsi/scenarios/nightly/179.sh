#!/bin/sh
# Objective 179 [recoverability][observability]: the transcript archive's
# failures are recorded. 175 fails closed: a pass that cannot have every
# credential value copies nothing and says so in the loop's log only.
# Required: the Bridge records `archive.failed {native, reason}` (paths,
# never a value) when a transcript's pass fails for a reason other than the
# one it recorded last, and `archive.resumed {native, copied}` at the first
# pass that copies after one. (Objective 193 raises the alert from them.)
#
# The world is 175.sh's: a real `spawn loop` runs one ephemeral worker on a
# fake engine; the credential file and the location tool are made up (the
# tool lists the file as unreadable when it cannot read it). The engine,
# asked BB:SAY, writes the value it knows in three forms.
# Checked:
#   (1) the credential rotated and its file unreadable, a BB:SAY turn:
#       within 20 s the store holds one archive.failed, its reason naming
#       the credential file's path; no archive event holds any form or part
#       of the value.
#   (2) a second BB:SAY turn, still unreadable: 8 s after it, still one
#       archive.failed.
#   (3) the loop killed and started again by its runner (as systemd does),
#       still unreadable, a third turn: 8 s after it, still one
#       archive.failed (the restarted Bridge reads the failure back from the
#       log), and no archive.resumed.
#   (4) the file readable again: within 20 s the store holds one
#       archive.resumed, of the same native session as the failures; no
#       archive event holds a form of the value.
#   (5) unreadable again and a turn: a second archive.failed. The loop
#       killed, the file made readable before its runner starts it again,
#       and a turn: within 20 s a second archive.resumed (the restarted
#       Bridge ends the failure recorded before it), still two
#       archive.failed.
#   (6) cc-astra A78 (rev-e's case): unreadable again and a turn, a third
#       archive.failed; then the session's transcript removed (as Claude
#       Code's cleanup removes it) while the archive lacks that turn's
#       lines, and the file made readable: 20 s later still two
#       archive.resumed, and the store's last archive event of that native
#       session is archive.failed (nothing shows the lines were copied).
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure. Any unexpected error exits 2, never 1.
#
# Fails with 1 at (1) on 175 alone: no archive.failed.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN,
# SPAWN_TEST_DATABASE_URL, tmux, jq, python3, base64 and git; creates and
# drops its own database, uses a private tmux socket, loopback only. Every
# wait is bounded in time.

set -eu

asserted=""
harness() {
    echo "179: harness: $*" >&2
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
SPAWN_DB_NAME="bb_179_archive_$run"
SPAWN_TMUX_SOCKET="bb-179-$run"
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
                echo "179: the $log's last lines:" >&2
                grep -v '^\s*$' "$WORK/$log.log" | tail -n 8 >&2
            fi
        done
        if [ -s "$WORK/engine.log" ]; then
            echo "179: the engines' last lines (ms agent what):" >&2
            tail -n 8 "$WORK/engine.log" >&2
        fi
        # What says a window is settling (flake notes, sync/flake-047-056.md):
        # each progress record as it stands, with the time it was written.
        if [ -d "${SPAWN_WORKSPACE_ROOT-}/.transcripts-progress" ]; then
            echo "179: the archive's progress records at $(date +%s%3N) (written-ms agent live known_ms behind_ms archived):" >&2
            find "$SPAWN_WORKSPACE_ROOT/.transcripts-progress" -type f -name '*.json' 2>/dev/null |
                while IFS= read -r record; do
                    printf '%s %s\n' "$(stat -c %.3Y "$record" | tr -d .)" \
                        "$(jq -r '"\(.agent) \(.live) \(.known_ms) \(.behind_ms) \(.archived)"' "$record" 2>/dev/null)"
                done >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "179: harness: unexpected exit $rc" >&2
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
note() { echo "179: $*"; }

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
# prompt. A turn writes one assistant line and ends 2 s after the prompt;
# a prompt holding BB:SAY makes that line hold the credential value the
# agent knows ($BB_WORK/said), as written, percent-encoded and base64-encoded. Each line
# is also appended to $BB_WORK/mirror/<native>.jsonl. Every prompt and turn
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
mkdir -p "$dir" "$BB_WORK/mirror"
transcript=$dir/$native.jsonl
count=0
line() {
  count=$((count + 1))
  text=$(printf '{"uuid":"%s-%s-%s","timestamp":"%s","sessionId":"%s",%s}' \
    "$SPAWN_EXECUTION_ID" "$$" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$native" "$1")
  printf '%s\n' "$text" >>"$BB_WORK/mirror/$native.jsonl"
  printf '%s\n' "$text" >>"$transcript"
}
# split_line <json fields> <value>: a line written in two parts, cut eight
# bytes into the value, 1.5 s apart: a pass may read the first part alone.
split_line() {
  count=$((count + 1))
  text=$(printf '{"uuid":"%s-%s-%s","timestamp":"%s","sessionId":"%s",%s}' \
    "$SPAWN_EXECUTION_ID" "$$" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$native" "$1")
  prefix=${text%%"$2"*}
  cut=$((${#prefix} + 8))
  printf '%s' "${text:0:cut}" | tee -a "$BB_WORK/mirror/$native.jsonl" >>"$transcript"
  sleep 1.5
  printf '%s\n' "${text:cut}" | tee -a "$BB_WORK/mirror/$native.jsonl" >>"$transcript"
}
log() { printf '%s %s %s\n' "$(date +%s%3N)" "$agent" "$1" >>"$BB_WORK/engine.log"; }
turn() { # <the prompt's text>
  said="working"
  case $1 in
  *BB:SAY*)
    value=$(cat "$BB_WORK/said")
    pct=$(printf '%s' "$value" | jq -sRr @uri)
    b64=$(printf '%s' "$value" | base64 -w0)
    said="the token is $value; in a link: https://h.example/?k=$pct; encoded: $b64; linked: $(cat "$BB_WORK/linked"); leaf: $(cat "$BB_WORK/leaf"); db: postgres://bb:mUp5x@127.0.0.1/d"
    ;;
  esac
  fields="\"type\":\"assistant\",\"message\":{\"id\":\"msg_${SPAWN_EXECUTION_ID}_$$_$count\",\"model\":\"bb-model\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":$(printf '%s' "$said" | jq -Rs .)}]}"
  case $1 in
  *BB:SAY*) split_line "$fields" "$value" ;;
  *) line "$fields" ;;
  esac
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

# The stand-in location tool: the credential file's one place and stamp, as
# emit-locations.py prints them (never the value); the file listed as
# unreadable when it cannot be read.
cat >"$WORK/locations.py" <<'PY'
import glob, json, os
cred, doc = os.environ["BB_CRED"], os.environ["BB_DOC"]
out = {"sources": [], "entries": [], "missing": [], "unreadable": [], "accepted": [],
       "accepted_paths": [], "watch": [], "probe": []}
def read(path):
    try:
        with open(path, "rb") as f:
            st = os.fstat(f.fileno())
            data = f.read()
    except PermissionError:
        out["unreadable"].append(path)
        return None
    out["sources"].append({"path": path, "size": st.st_size, "mtime_ns": st.st_mtime_ns,
                           "dev": st.st_dev, "ino": st.st_ino, "ctime_ns": st.st_ctime_ns})
    return data
def place(path, number, ptr, **rest):
    out["entries"].append(dict(path=path, line=number, ptr=ptr, **rest))
# The file and its older copies beside it (spawn.env.<anything>), as the collector reads them.
for path in [cred] + sorted(glob.glob(glob.escape(cred) + ".*")):
  data = read(path)
  cred_now = path
  # As the collector's read() splits: universal newlines (a lone CR ends a line) and NUL.
  for number, line in enumerate(__import__("re").split(rb"\r\n|\r|\n|\0", data or b"")):
      if line.startswith(b"API_TOKEN="):
          place(cred_now, number, None, t="raw", span=[10, len(line)], name="spawn.env:API_TOKEN")
      elif line.startswith(b"LINK="):
          at = line.index(b"?k=") + 3
          plain = __import__("urllib.parse").parse.unquote(line[at:].decode())
          place(cred_now, number, None, t="pct", span=[at, len(line)], sub=[0, len(plain.encode())],
                name="spawn.env:LINK")
      elif line.startswith(b"DB="):
          user = line.index(b"://") + 3
          colon = line.index(b":", user)
          at = line.index(b"@", colon)
          place(cred_now, number, None, t="short", form="pair", pre="", post="@", k=0,
                user={"t": "raw", "span": [user, colon]}, pw={"t": "raw", "span": [colon + 1, at]},
                name="spawn.env:DB")
if read(doc) is not None:
    leaf = json.loads(open(doc).read())["zeta"]["token"]
    # "zeta" is the second key in sorted order, "token" its only one.
    place(doc, None, [1, 0], t="raw", span=[0, len(leaf.encode())], name="creds.json:token")
# The directory new copies appear in.
out["watch"].append({"path": os.path.dirname(cred), "mtime_ns": os.stat(os.path.dirname(cred)).st_mtime_ns})
# Workspaces under a base, as a settings glob's: the base watched, each workspace's .claude probed
# (never the workspace itself), and a .claude's creds.env read like the file.
base = os.environ["BB_WSROOT"]
out["watch"].append({"path": base, "mtime_ns": os.stat(base).st_mtime_ns})
for workspace in sorted(glob.glob(glob.escape(base) + "/*")):
    claude = os.path.join(workspace, ".claude")
    out["probe"].append({"path": claude, "exists": os.path.isdir(claude)})
    path = os.path.join(claude, "creds.env")
    if os.path.isfile(path):
        data = read(path)
        for number, line in enumerate((data or b"").split(b"\n")):
            if line.startswith(b"API_TOKEN="):
                place(path, number, None, t="raw", span=[10, len(line)], name="creds.env:API_TOKEN")
print(json.dumps(out))
PY
mkdir -p "$WORK/cred"
value="made-up/token+0123456789abcdefXYZ"
value2="made-up/rotated+9876543210zyxwvuQRS"
linked="made-up/linked=4242424242aabbcc"
leaf="made-up-json-leaf-55556666777788"
# cred_file <API_TOKEN value> [unreadable]: the made-up credential file,
# written beside it and renamed over it, so a pass sees the old file or the
# new one whole; `unreadable` makes the new one unreadable before the
# rename (a rotation that cannot be read, never one a pass read between).
cred_file() {
    printf 'NOTE=a lone CR ends this line\rAPI_TOKEN=%s\nLINK=https://h.example/?k=%s\nDB=postgres://bb:mUp5x@127.0.0.1/d\n' \
        "$1" "$(printf '%s' "$linked" | jq -sRr @uri)" >"$WORK/cred/.spawn.env.new"
    if [ "${2-}" = unreadable ]; then chmod 000 "$WORK/cred/.spawn.env.new"; else chmod 600 "$WORK/cred/.spawn.env.new"; fi
    mv -f "$WORK/cred/.spawn.env.new" "$WORK/cred/spawn.env"
}
cred_file "$value"
chmod 600 "$WORK/cred/spawn.env"
jq -cn --arg leaf "$leaf" '{zeta: {token: $leaf}, alpha: 1}' >"$WORK/cred/creds.json"
printf '%s' "$value" >"$WORK/said"
printf '%s' "$linked" >"$WORK/linked"
printf '%s' "$leaf" >"$WORK/leaf"
# The three forms of each value, one per line, as a check reads them.
for v in "$value" "$value2"; do
    printf '%s\n' "$v" "$(printf '%s' "$v" | jq -sRr @uri)" "$(printf '%s' "$v" | base64 -w0)"
done >"$WORK/forms"
[ "$(sort -u "$WORK/forms" | wc -l)" -eq 6 ] || harness "the made-up values' forms are not six distinct strings"
BB_CRED=$WORK/cred/spawn.env
BB_DOC=$WORK/cred/creds.json
BB_WSROOT=$WORK/wsroot
mkdir -p "$BB_WSROOT/a/src"
SPAWN_ARCHIVE_LOCATIONS_TOOL=$WORK/locations.py
export BB_CRED BB_DOC BB_WSROOT SPAWN_ARCHIVE_LOCATIONS_TOOL
# 175 R5/R6: a stand-in is honoured only with the gate's marker (and roots
# outside the account's home: this world's are under $WORK).
SPAWN_GATE_SANDBOX=1
export SPAWN_GATE_SANDBOX

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
# forms_in <file> [first|second|both]: how many of the values' forms
# (three each: as written, percent-encoded, base64) the file holds.
forms_in() {
    case ${2-both} in first) range=1,3 ;; second) range=4,6 ;; *) range=1,6 ;; esac
    n=0
    while IFS= read -r form; do
        if grep -qF -- "$form" "$1"; then n=$((n + 1)); fi
    done <<FORMS
$(sed -n "${range}p" "$WORK/forms")
FORMS
    echo "$n"
}
marker='[REDACTED:spawn.env:API_TOKEN]'
markers_in() { grep -oF -- "$marker" "$1" | wc -l; }
# parts_in <file>: lines holding any part of a made-up value (each begins
# with `made-up`) or the short password.
parts_in() { grep -cE 'made-up|mUp5x' "$1" || true; }
# others_in <file>: how many of the other places' markers it holds.
others_in() {
    n=0
    for name in spawn.env:LINK spawn.env:DB creds.json:token; do
        if grep -qF -- "[REDACTED:$name]" "$1"; then n=$((n + 1)); fi
    done
    echo "$n"
}

# --- the agent -----------------------------------------------------------------

capture "$SPAWN_BIN" mission add --text "179 archive alert"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker teller \
    --brief "Do the 179 task: work as you are asked."
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"
task=$(q "select max(id) from mission_tasks where mission_id = $mission")
put teller agent "$(q "select agent_id from mission_tasks where id = $task")"
put teller session "$(q "select session_id from mission_tasks where id = $task")"

start_loop
until_ended teller 1 120 || harness "the engine did not take its task and end its turn within 120 s"
note "the worker took its task"


# --- the alerts and the archive events ---------------------------------------------

# The alerts now, from `spawn alerts --once --json`, in $WORK/alerts.json.
read_alerts() {
    capture "$SPAWN_BIN" alerts --once --json
    if [ "$status" -ne 0 ] || ! jq -e 'type == "array"' "$WORK/out.txt" >/dev/null 2>&1; then
        harness "\`spawn alerts --once --json\` exited $status: $(err | head -n 2 | tr '\n' ' ')"
    fi
    cp "$WORK/out.txt" "$WORK/alerts.json"
}
# archive_alerts <state>: how many archive_failed alerts of the worker are
# in that state (open, cleared) in the last read.
archive_alerts() {
    jq --arg s "$(get teller agent)" --arg st "$1" \
        '[.[] | select(.kind == "archive_failed" and .subject == $s and .state == $st)] | length' \
        "$WORK/alerts.json"
}
archive_events() { q "select count(*) from spawn_events where event_name = '$1'"; }
# Every archive event as the store holds it, for the value checks.
dump_archive_events() {
    psql -d "$SPAWN_DB_NAME" -XAtc "select kind::text from spawn_events
        where event_name in ('archive.failed', 'archive.resumed') order by id" >"$WORK/archive-events.txt"
}
# wait_alerts <seconds> <state> <count>: read the alerts every 0.5 s until
# that many of the worker's archive_failed are in that state; 1 when the
# time ran out.
wait_alerts() {
    limit=$(($(date +%s) + $1))
    while :; do
        read_alerts
        [ "$(archive_alerts "$2")" -eq "$3" ] && return 0
        [ "$(date +%s)" -lt "$limit" ] || return 1
        sleep 0.5
    done
}

# --- (1) a pass that copies nothing records archive.failed ---------------------------

# wait_events <seconds> <name> <count>: until the store holds that many.
wait_events() {
    limit=$(($(date +%s) + $1))
    until [ "$(archive_events "$2")" -eq "$3" ]; do
        [ "$(date +%s)" -lt "$limit" ] || return 1
        sleep 0.5
    done
}
# Each archive.failed's reason names the credential file.
reasons_name_the_file() {
    [ "$(q "select count(*) from spawn_events where event_name = 'archive.failed'
        and position('$WORK/cred/spawn.env' in kind -> 'detail' ->> 'reason') = 0")" -eq 0 ]
}

cred_file "$value2" unreadable
printf '%s' "$value2" >"$WORK/said"
send teller bb-say-1 "BB:SAY the token, please"
until_ended teller 2 60 || harness "the first BB:SAY turn did not end within 60 s"
wait_events 20 archive.failed 1 ||
    fail "(1) with the credential file unreadable, the store holds $(archive_events archive.failed) archive.failed 20 s after the turn, not one"
reasons_name_the_file || fail "(1) archive.failed's reason does not name the credential file"
dump_archive_events
[ "$(forms_in "$WORK/archive-events.txt")" -eq 0 ] && [ "$(parts_in "$WORK/archive-events.txt")" -eq 0 ] ||
    fail "(1) an archive event holds a form or a part of a value"
native=$(q "select kind -> 'detail' ->> 'native' from spawn_events where event_name = 'archive.failed' order by id limit 1")
note "(1) unreadable: one archive.failed, naming the file; no value"

# --- (2) still failing: nothing more ---------------------------------------------------

send teller bb-say-2 "BB:SAY the token again"
until_ended teller 3 60 || harness "the second BB:SAY turn did not end within 60 s"
sleep 8
[ "$(archive_events archive.failed)" -eq 1 ] ||
    fail "(2) a second failing pass for the same reason recorded archive.failed again ($(archive_events archive.failed))"
note "(2) still failing: one archive.failed"

# --- (3) a restarted loop reads the failure back ------------------------------------

# crash_loop: kill the loop and wait until it is gone (its runner starts it
# again 5 s after); restarted_loop: wait until the runner has started it.
# Not named stop_loop: cleanup's stop_loop (above) must stay the one that
# ends the runner, or the runner outlives the script and keeps restarting
# `spawn loop` after its WORK is gone (rev-g, 2026-11-16).
crash_loop() {
    old=$(cat "$WORK/loop.pid")
    kill "$old" 2>/dev/null || harness "cannot stop the loop $old"
    deadline=$(($(date +%s) + 10))
    while kill -0 "$old" 2>/dev/null; do
        [ "$(date +%s)" -lt "$deadline" ] || harness "the loop $old did not stop within 10 s"
        sleep 0.1
    done
}
restarted_loop() {
    deadline=$(($(date +%s) + 30))
    until [ -s "$WORK/loop.pid" ] && [ "$(cat "$WORK/loop.pid")" != "$old" ] && kill -0 "$(cat "$WORK/loop.pid")" 2>/dev/null; do
        [ "$(date +%s)" -lt "$deadline" ] || harness "the loop's runner did not start it again within 30 s"
        sleep 0.5
    done
    sleep 5
}
crash_loop
restarted_loop
send teller bb-say-3 "BB:SAY the token once more"
until_ended teller 4 90 || harness "the third BB:SAY turn did not end within 90 s"
sleep 8
failed=$(archive_events archive.failed)
[ "$failed" -eq 1 ] ||
    fail "(3) after a loop restart and a turn, still unreadable, the store holds $failed archive.failed, not one: the restarted Bridge did not read the failure back"
reasons_name_the_file || fail "(3) an archive.failed's reason does not name the credential file"
[ "$(archive_events archive.resumed)" -eq 0 ] || fail "(3) archive.resumed recorded while the file is unreadable"
note "(3) a restarted loop: one archive.failed, no archive.resumed"

# --- (4) readable again: archive.resumed ------------------------------------------------

chmod 600 "$WORK/cred/spawn.env"
wait_events 20 archive.resumed 1 ||
    fail "(4) the credential file readable again, the store holds $(archive_events archive.resumed) archive.resumed 20 s later, not one"
[ "$(q "select kind -> 'detail' ->> 'native' from spawn_events where event_name = 'archive.resumed'")" = "$native" ] ||
    fail "(4) archive.resumed is not of the failed transcript's native session"
dump_archive_events
[ "$(forms_in "$WORK/archive-events.txt")" -eq 0 ] || fail "(4) an archive event holds a form of a value"
note "(4) readable again: one archive.resumed, of the same transcript"

# --- (5) fixed while the loop is down: the restarted Bridge ends it ---------------------

chmod 000 "$WORK/cred/spawn.env"
send teller bb-say-4 "BB:SAY the token, unreadable again"
until_ended teller 5 60 || harness "the fourth BB:SAY turn did not end within 60 s"
wait_events 20 archive.failed 2 ||
    fail "(5) unreadable again after a resume, the store holds $(archive_events archive.failed) archive.failed 20 s after the turn, not two"
crash_loop
chmod 600 "$WORK/cred/spawn.env"
restarted_loop
send teller bb-say-5 "BB:SAY the token, readable while the loop was down"
until_ended teller 6 90 || harness "the fifth BB:SAY turn did not end within 90 s"
wait_events 20 archive.resumed 2 ||
    fail "(5) the file made readable while the loop was down, the store holds $(archive_events archive.resumed) archive.resumed 20 s after the restarted loop's turn, not two: the failure recorded before the restart is never ended"
[ "$(archive_events archive.failed)" -eq 2 ] ||
    fail "(5) the store holds $(archive_events archive.failed) archive.failed after the restart, not two"
dump_archive_events
[ "$(forms_in "$WORK/archive-events.txt")" -eq 0 ] || fail "(5) an archive event holds a form of a value"
note "(5) fixed while the loop was down: the restarted Bridge recorded archive.resumed"

# --- (6) a transcript gone with lines unarchived never reads resumed -------------------

chmod 000 "$WORK/cred/spawn.env"
send teller bb-say-6 "BB:SAY the token, then the transcript goes"
until_ended teller 7 60 || harness "the sixth BB:SAY turn did not end within 60 s"
wait_events 20 archive.failed 3 ||
    fail "(6) unreadable again, the store holds $(archive_events archive.failed) archive.failed 20 s after the turn, not three"
gone=$(find "$SPAWN_CLAUDE_PROJECTS" -type f -name "$native.jsonl" | head -n 1)
[ -n "$gone" ] || harness "(6) no transcript $native.jsonl under $SPAWN_CLAUDE_PROJECTS"
rm -f -- "$gone" || harness "(6) cannot remove the transcript"
chmod 600 "$WORK/cred/spawn.env"
sleep 20
[ "$(archive_events archive.resumed)" -eq 2 ] ||
    fail "(6) the transcript removed with lines the archive lacks, the store holds $(archive_events archive.resumed) archive.resumed 20 s later, not two: a resume claims lines that were never copied"
last=$(q "select event_name from spawn_events where event_name in ('archive.failed', 'archive.resumed')
    and kind -> 'detail' ->> 'native' = '$native' order by id desc limit 1")
[ "$last" = archive.failed ] || fail "(6) the store's last archive event of the removed transcript is $last, not archive.failed"
note "(6) a transcript gone with lines unarchived: the failure stands, no archive.resumed"

# Nothing this script started outlives it: the loop's runner and serve are
# ended here, and no background job of this shell is left.
stop_serve
stop_loop
# Not $(jobs -p): a command substitution is a subshell, which holds no jobs
# under dash. dash also lists a job that has ended until it is reported, so
# only a listed pid still alive counts. The runner's own pid is checked too:
# stop_loop clears it.
jobs -p >"$WORK/jobs.left"
alive=""
while IFS= read -r job; do
    if [ -n "$job" ] && kill -0 "$job" 2>/dev/null; then alive="$alive $job"; fi
done <"$WORK/jobs.left"
if [ -n "$wrapper_pid" ] && kill -0 "$wrapper_pid" 2>/dev/null; then alive="$alive runner $wrapper_pid"; fi
[ -z "$alive" ] || fail "a background job outlives the scenario:$alive"
echo "179: the archive's failures and its resume are recorded, once per change"
exit 0
