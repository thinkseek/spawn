#!/bin/sh
# Journey j07, recover (b): a person's stop is final and keeps the work; an
# outage of the loop's database, a host restart and a full disk are each
# recorded with their cause and brought back by the path it takes, and no
# line of work is lost (design-gate-v2 C1: the owner's data-loss path).
#
# Folds Spawn scenario 028's faults (6)-(9) and its (10) for these agents
# (028 is split by fault kind, design-gate-journeys-spawn.md §2 and §3: its
# faults (1)-(5) and (10) for their agents are check-j06-recover-a):
#   028 (6) stop: a person stops a worker: cause `stopped` (evidence: the
#       Stop's command id), path `final`; no revival; its branch holds every
#       line.
#   028 (7) database: the loop's database is cut for 30 s: the loop does not
#       exit; no engine ends; each execution that ran through it gets cause
#       `database_unavailable` (evidence: from_ms, to_ms, failed_passes 2 or
#       more), path `wait`, and an `execution.cleared`; the work done in the
#       outage reaches each branch within 15 s of its end.
#   028 (8) host restart: the loop is stopped as systemd stops it (SIGTERM,
#       its pass finished), the terminal server is killed with every engine
#       in it, and the loop starts 2 s later: each execution gets cause
#       `host_restart` (evidence: terminal_server `gone`), path `resume`, its
#       resume by key, and works again in its native session within 20 s of
#       the loop's start, with every line.
#   028 (9) disk full: the free space reads 100 MiB under the 1 GiB floor and
#       a worker's engine fails with ENOSPC (exit 1): cause `disk_full`
#       (evidence: free_bytes under floor_bytes 1073741824), path `wait`; no
#       revival in the 15 s the disk stays full; `execution.cleared` within
#       6 s of room; the same native session works again within 15 s.
#   028 (10) no work lost: every agent's branch (this half's seven) holds
#       work.txt as `work 1..n`.
#
# One database, one tmux socket, one real loop (`spawn loop`: the Bridge,
# the dispatcher, the supervisor, no model judge), run as systemd runs
# spawn-loop.service (started again 5 s after it exits), with 028's
# settings unchanged. As in 028: six ephemeral workers of one mission, each
# with a delivered task, and one resident with a task, on 028's fake engine,
# which reaches 028's fake model relay (it answers 200 throughout here); the
# loop reaches its database through 028's Unix socket forwarder the journey
# can cut (the journey's own psql goes direct); a `df` first on the loop's
# PATH fakes the free space. The workers w1..w5 stand where 028's crash,
# exit, context, model and auth workers stood (028 faults them in (1)-(5),
# j06 here), and `stop` is 028's stopped worker. The stop strikes first;
# once it is recorded, (7)-(9) follow in turn on the agents that work, with
# 028's windows (30 s outage, 2 s restart, 15 s full disk). (6) is judged
# after (7), no sooner than 45 s after the stop, as 028 judged it.
# The control records (spawn_events), agent_executions, spawn_commands and
# mission_tasks are read where 028 reads them, with its assertions.
# Traceability: /var/tmp/spawn/sync/gate-journeys-spawn-proof/trace/j07.tsv
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
# Needs SPAWN_BIN, SPAWN_TEST_DATABASE_URL, python3 and curl; creates and
# drops its own database, uses a private tmux socket, loopback only. Every
# wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "j07: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk curl python3 sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_df=$(command -v df) || harness "df is not installed"

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
SPAWN_DB_NAME="bb_j07_recover_$run"
SPAWN_TMUX_SOCKET="bb-j07-$run"
export SPAWN_TERMINAL_BACKEND=tmux
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
pg_url_parse "$SPAWN_TEST_DATABASE_URL"
# The loop reaches its database through a forwarder the scenario can cut;
# the scenario's own psql and commands go to the server directly.
case "$pg_url_host" in
/*) upstream="$pg_url_host/.s.PGSQL.$pg_url_port" ;;
"") upstream="/run/postgresql/.s.PGSQL.$pg_url_port" ;;
*) upstream="$pg_url_host:$pg_url_port" ;;
esac
forward_dir=$WORK/db
loop_url="$pg_url_scheme://${pg_url_userinfo:+$pg_url_userinfo@}$(pg_url_encode_host "$forward_dir"):$pg_url_port/$SPAWN_DB_NAME"
[ -z "$pg_url_options" ] || loop_url="$loop_url?$pg_url_options"

database_made=""
wrapper_pid=""
relay_pid=""
forward_pid=""
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
cleanup() {
    stop_loop
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    for pid in $relay_pid $forward_pid; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
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
            echo "j07: the loop's last lines:" >&2
            grep -v '^\s*$' "$WORK/loop.log" | tail -n 12 >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "j07: the engines' last turns (ms agent session execution native how lines):" >&2
            tail -n 8 "$WORK/turns.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "j07: interruption records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'type', r ->> 'execution_id', r ->> 'cause', r ->> 'path'
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' like 'execution.%'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j07: harness: unexpected exit $rc" >&2
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
# 028's BB_028_MEASURE is not carried: a check fails as it does in the gate.
measured_fail() { fail "$*"; }
note() { echo "j07: $*"; }

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

mkdir -p "$WORK/relay" "$WORK/ctl" "$WORK/bin" "$WORK/claude-config" "$forward_dir"

# The model relay engines reach (SPAWN_MODEL_RELAY): it answers each agent
# (the engine names itself in x-bb-agent) by the mode in relay/<agent>:
# `ok` (200, the default), `overloaded` (529), `auth <token>` (401 unless the
# request carries that token) or `too_long` (400 "prompt is too long" to the
# native session it first sees after the mode is set, 200 to any other).
# Every request is logged: ms, agent, native session, status, token.
cat >"$WORK/relay.py" <<'EOF'
import json, os, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

work = sys.argv[1]
relay = os.path.join(work, "relay")
BODIES = {
    200: {"id": "msg_bb", "type": "message", "role": "assistant", "model": "claude-bb",
          "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
          "usage": {"input_tokens": 1, "output_tokens": 1}},
    529: {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}},
    401: {"type": "error", "error": {"type": "authentication_error",
          "message": "OAuth token has expired. Please obtain a new token or refresh your existing token."}},
    400: {"type": "error", "error": {"type": "invalid_request_error",
          "message": "prompt is too long: 210511 tokens > 200000 maximum"}},
}


def words(path):
    try:
        with open(path) as f:
            return f.read().split()
    except OSError:
        return []


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def answer(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.answer(200, {"ok": True})

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        if length:
            self.rfile.read(length)
        agent = self.headers.get("x-bb-agent", "-")
        native = self.headers.get("x-claude-code-session-id", "-")
        token = self.headers.get("authorization", "").replace("Bearer ", "", 1) or "-"
        mode = words(os.path.join(relay, agent)) or ["ok"]
        status = 200
        if mode[0] == "overloaded":
            status = 529
        elif mode[0] == "auth" and token != (mode[1] if len(mode) > 1 else ""):
            status = 401
        elif mode[0] == "too_long":
            path = os.path.join(relay, agent + ".exhausted")
            exhausted = words(path)
            if not exhausted:
                with open(path, "w") as f:
                    f.write(native + "\n")
                exhausted = [native]
            if native in exhausted:
                status = 400
        with open(os.path.join(work, "relay.log"), "a") as f:
            f.write("%d %s %s %d %s\n" % (time.time() * 1000, agent, native, status, token))
        self.answer(status, BODIES[status])


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(os.path.join(work, "relay.port.partial"), "w") as f:
    f.write(str(server.server_address[1]))
os.rename(os.path.join(work, "relay.port.partial"), os.path.join(work, "relay.port"))
server.serve_forever()
EOF

# The loop's database connection: a Unix socket forwarded to the server.
# While `db.cut` exists every forwarded connection is closed and new ones are
# closed at once, as when the database server stops.
cat >"$WORK/forward.py" <<'EOF'
import os, socket, sys, threading, time

listen, upstream, cut = sys.argv[1:4]
lock = threading.Lock()
pairs = []


def connect():
    if upstream.startswith("/"):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(upstream)
        return s
    host, port = upstream.rsplit(":", 1)
    return socket.create_connection((host, int(port)))


def close(s):
    try:
        s.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    try:
        s.close()
    except OSError:
        pass


def pump(a, b):
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except OSError:
        pass
    close(a)
    close(b)


def watch():
    while True:
        if os.path.exists(cut):
            with lock:
                live = list(pairs)
                pairs.clear()
            for a, b in live:
                close(a)
                close(b)
        time.sleep(0.1)


server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(listen)
server.listen(128)
threading.Thread(target=watch, daemon=True).start()
while True:
    client, _ = server.accept()
    if os.path.exists(cut):
        close(client)
        continue
    try:
        up = connect()
    except OSError:
        close(client)
        continue
    with lock:
        pairs.append((client, up))
    threading.Thread(target=pump, args=(client, up), daemon=True).start()
    threading.Thread(target=pump, args=(up, client), daemon=True).start()
EOF

# `df` for the loop: while `df.low` exists every path reads as having the
# KiB it holds available; otherwise the real df.
cat >"$WORK/bin/df" <<EOF
#!/bin/sh
if [ -f "\$BB_WORK/df.low" ]; then
    last=
    for word in "\$@"; do last=\$word; done
    free=\$(cat "\$BB_WORK/df.low")
    printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n'
    printf 'bbfs 10485760 %s %s 99%% %s\n' "\$((10485760 - free))" "\$free" "\$last"
    exit 0
fi
exec '$real_df' "\$@"
EOF
chmod 755 "$WORK/bin/df"

# The fake engine: a prompt; text typed in opens a turn, which starts after
# a second of quiet. It writes its session transcript where Claude Code
# does, so the Bridge and the Observer see turns and tool calls as they see
# a real engine's. Once its agent was given work (a typed `BB:WORK`), every
# turn works: one step a second, each a model request to the relay its
# --settings name (ANTHROPIC_BASE_URL) with the credentials in
# $CLAUDE_CONFIG_DIR/.credentials.json, then a tool call (assistant
# `tool_use`, then its `tool_result`) that appends the next line `work <n>`
# to work.txt in its workspace (n = the lines already there + 1). A request
# that fails ends the turn as Claude Code does: the API error line in the
# transcript (isApiErrorMessage, error, apiErrorStatus, the text Claude Code
# shows), and the engine stays at its prompt. A turn logs, as it starts, the
# lines work.txt holds and whether they are exactly `work 1..n` (turns.log).
# Control files: ctl/<agent>.exit (removed, then exit 0), ctl/<agent>.enospc
# (exit 1 as Node does on a full disk). Its PID is in pid-<agent>.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
mode=new
settings=
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=${2-}; mode=resume; shift ;;
  --session-id) native=${2-}; mode=new; shift ;;
  --settings) settings=${2-}; shift ;;
  esac
  shift
done
agent=$SPAWN_AGENT_ID
base=$(printf '%s' "$settings" | jq -r '.env.ANTHROPIC_BASE_URL // empty' 2>/dev/null)
printf '%s\n' "$$" >"$BB_WORK/pid-$agent"
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
api_error() { # <error> <status or null> <text>
  text=$(printf '%s' "$3" | jq -Rs .)
  line "\"type\":\"assistant\",\"isApiErrorMessage\":true,\"error\":\"$1\",\"apiErrorStatus\":$2,\"message\":{\"id\":\"msg_$count\",\"model\":\"<synthetic>\",\"role\":\"assistant\",\"type\":\"message\",\"stop_reason\":\"stop_sequence\",\"content\":[{\"type\":\"text\",\"text\":$text}]}"
  printf '\nclaude: %s\n\n❯ ' "$3"
}
ask() {
  token=$(jq -r '.claudeAiOauth.accessToken // empty' "$CLAUDE_CONFIG_DIR/.credentials.json" 2>/dev/null)
  code=$(curl -s --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 10 -X POST \
    -H "x-bb-agent: $agent" -H "x-claude-code-session-id: $native" \
    -H "authorization: Bearer $token" -H 'content-type: application/json' \
    --data '{"model":"claude-bb","max_tokens":1,"messages":[{"role":"user","content":"next step"}]}' \
    "$base/v1/messages" 2>/dev/null) || code=000
}
step() {
  if [ -f "$BB_WORK/ctl/$agent.exit" ]; then
    rm -f "$BB_WORK/ctl/$agent.exit"
    printf '\nbye\n'
    exit 0
  fi
  if [ -f "$BB_WORK/ctl/$agent.enospc" ]; then
    printf '\nError: ENOSPC: no space left on device, write\n    at Object.writeFileSync (node:fs:2425:20)\n'
    exit 1
  fi
  ask
  case "$code" in
  200) ;;
  529) api_error overloaded 529 'API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}'; return 1 ;;
  401) api_error authentication_failed 401 'API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"OAuth token has expired. Please obtain a new token or refresh your existing token."}} · Please run /login'; return 1 ;;
  400) api_error invalid_request 400 'Prompt is too long'; return 1 ;;
  *) api_error unknown null "API Error: Connection error. (HTTP $code)"; return 1 ;;
  esac
  n=0
  if [ -f work.txt ]; then n=$(wc -l <work.txt); fi
  n=$((n + 1))
  id="toolu_${SPAWN_EXECUTION_ID}_$count"
  line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{}}],\"stop_reason\":\"tool_use\"}"
  printf 'work %s\n' "$n" >>work.txt
  # Atomically (a reader never sees it truncated: `>` empties the file before
  # it writes, and working() read it empty once, exit 2 on the base, task 209).
  printf '%s %s\n' "$(date +%s%3N)" "$n" >"$BB_WORK/last-$agent.$$" && mv -f "$BB_WORK/last-$agent.$$" "$BB_WORK/last-$agent"
  sleep 0.2
  line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
  sleep 0.8
  return 0
}
turns=0
turn() {
  turns=$((turns + 1))
  how=$mode
  if [ "$turns" -gt 1 ]; then how=input; fi
  lines=0
  valid=valid
  if [ -f work.txt ]; then
    lines=$(wc -l <work.txt)
    valid=$(awk 'BEGIN { ok = 1 } $0 != "work " NR { ok = 0 } END { print ok ? "valid" : "invalid" }' work.txt)
  fi
  printf '%s %s %s %s %s %s %s %s\n' "$(date +%s%3N)" "$agent" "$SPAWN_SESSION_ID" \
    "$SPAWN_EXECUTION_ID" "$native" "$how" "$lines" "$valid" >>"$BB_WORK/turns.log"
  printf '\n✻ Working… (esc to interrupt)\n\n❯ '
  while step; do :; done
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *BB:WORK*) : >"$BB_WORK/working-$agent" ;; esac
  elif [ "$pending" = 1 ]; then
    pending=0
    if [ -f "$BB_WORK/working-$agent" ]; then
      turn
    else
      line '"type":"system","subtype":"turn_duration","durationMs":1'
      printf '\nidle\n❯ '
    fi
  fi
done
ENGINE
chmod 755 "$WORK/claude"

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

# `spawn loop` as systemd runs spawn-loop.service: started again 5 s after
# it exits (Restart=always, RestartSec=5). Each start and exit is logged.
cat >"$WORK/run-loop.sh" <<'EOF'
#!/bin/sh
while [ ! -f "$WORK/loop.stop" ]; do
    SPAWN_DATABASE_URL=$BB_LOOP_DATABASE_URL "$SPAWN_BIN" loop --repo "$BB_REPO" \
        --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    child=$!
    printf '%s\n' "$child" >"$WORK/loop.pid"
    printf '%s start %s\n' "$(date +%s%3N)" "$child" >>"$WORK/loop-runs.log"
    status=0
    wait "$child" || status=$?
    printf '%s exit %s %s\n' "$(date +%s%3N)" "$child" "$status" >>"$WORK/loop-runs.log"
    [ ! -f "$WORK/loop.stop" ] || exit 0
    sleep 5
done
EOF

python3 -I "$WORK/relay.py" "$WORK" >"$WORK/relay.out" 2>&1 &
relay_pid=$!
python3 -I "$WORK/forward.py" "$forward_dir/.s.PGSQL.$pg_url_port" "$upstream" "$WORK/db.cut" \
    >"$WORK/forward.out" 2>&1 &
forward_pid=$!
i=0
until [ -s "$WORK/relay.port" ] && [ -S "$forward_dir/.s.PGSQL.$pg_url_port" ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || harness "the fake relay or the database forwarder did not start: $(cat "$WORK/relay.out" "$WORK/forward.out")"
    sleep 0.1
done
relay_port=$(cat "$WORK/relay.port")
token_old="bb-token-1-$run"
token_new="bb-token-2-$run"
credentials() {
    printf '{"claudeAiOauth":{"accessToken":"%s","expiresAt":%s}}\n' "$1" "$2" \
        >"$WORK/claude-config/.credentials.json.partial"
    mv "$WORK/claude-config/.credentials.json.partial" "$WORK/claude-config/.credentials.json"
}
credentials "$token_old" "$(($(date +%s) * 1000 + 3600000))"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_GATE SPAWN_DISPATCH \
    SPAWN_LOOP_INTERVAL_MS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
BB_LOOP_DATABASE_URL=$loop_url
BB_REPO=$repo
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
CLAUDE_CONFIG_DIR=$WORK/claude-config
SPAWN_MODEL_RELAY=http://127.0.0.1:$relay_port
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
PATH=$WORK/bin:$PATH
# The disk-full episode reads 100 MiB free against a floor of 1 GiB.
SPAWN_DISK_WARN_GB=2
SPAWN_DISK_FLOOR_GB=1
# The recovery's settings, short for the scenario (their defaults are 10,
# 60, 1800 and 10 s): the resume backoff's base, the model retry backoff's
# base, the slow retry while the credentials are unchanged, and how often a
# wait reads its cause again.
SPAWN_RECOVER_BACKOFF_SECS=2
SPAWN_RECOVER_MODEL_BACKOFF_SECS=2
SPAWN_RECOVER_AUTH_RETRY_SECS=600
SPAWN_RECOVER_PROBE_SECS=2
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL BB_LOOP_DATABASE_URL BB_REPO SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT \
    SPAWN_CLAUDE_PROGRAM SPAWN_CLAUDE_PROJECTS CLAUDE_CONFIG_DIR SPAWN_MODEL_RELAY BB_PROJECTS \
    BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_SUPERVISOR SPAWN_MISSION_FILE PATH \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_RECOVER_BACKOFF_SECS \
    SPAWN_RECOVER_MODEL_BACKOFF_SECS SPAWN_RECOVER_AUTH_RETRY_SECS SPAWN_RECOVER_PROBE_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/state"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/turns.log"
: >"$WORK/relay.log"
: >"$WORK/loop-runs.log"

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
    sh "$WORK/run-loop.sh" &
    wrapper_pid=$!
    i=0
    until [ -s "$WORK/loop.pid" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || harness "the loop did not start"
        sleep 0.1
    done
}

# --- helpers ----------------------------------------------------------------

now_ms() { date +%s%3N; }
q() { scalar "$1"; }
put() { printf '%s\n' "$3" >"$WORK/state/$1.$2"; }
get() { cat "$WORK/state/$1.$2" 2>/dev/null || true; }
secs() { awk -v ms="$1" 'BEGIN { printf "%.1f", ms / 1000 }'; }

# The live execution of a session, if any.
live_exec() {
    q "select execution_id from agent_executions where session_id = '$1' and ended_at_ms is null
        order by started_at_ms desc, execution_id desc limit 1"
}
ended_at() { q "select ended_at_ms from agent_executions where session_id = '$1' and execution_id = '$2'"; }
end_of() { # how the Bridge recorded an execution's end
    q "select coalesce(cause, '-') || ' exit_code=' || coalesce(exit_code::text, 'null')
         from agent_executions where session_id = '$1' and execution_id = '$2'"
}
# Control records of a type for an execution: `<at_ms>|<record>`, oldest first.
records_of() {
    sql "select e.recorded_at_ms || '|' || (e.kind -> 'detail' -> 'record')::text
           from spawn_events e
          where e.session_id = 'session_control' and e.event_name = 'control.recorded'
            and e.kind -> 'detail' -> 'record' ->> 'type' = '$1'
            and e.kind -> 'detail' -> 'record' ->> 'execution_id' = '$2'
          order by e.session_seq"
}
# The first record of a type for an execution since a time.
interrupted() { records_of execution.interrupted "$1" | awk -F'|' -v t="${2:-0}" '$1 >= t { print; exit }'; }
cleared() { records_of execution.cleared "$1" | awk -F'|' -v t="${2:-0}" '$1 >= t { print; exit }'; }
attempt_of() { printf '%s' "$1" | jq -r '.attempt // empty'; }
max_command() { q "select coalesce(max(id), 0) from spawn_commands"; }
# The commands into a session after a command id, but the scenario's own:
# `id|principal|key|type|created_at_ms|succeeded`.
commands_after() {
    sql "select id || '|' || principal || '|' || idempotency_key || '|' || (action ->> 'type') || '|'
                || created_at_ms || '|' || coalesce(succeeded::text, 'open')
           from spawn_commands
          where session_id = '$1' and id > $2 and principal not like 'bb-%'
            and action ->> 'type' in ('resume', 'start', 'input')
          order by id"
}
revival_key() { # the command with the key `recover-<execution>-<attempt>`: `id|type|created|succeeded`
    q "select id || '|' || (action ->> 'type') || '|' || created_at_ms || '|' || coalesce(succeeded::text, 'open')
         from spawn_commands where session_id = '$1' and idempotency_key = 'recover-$2-$3'"
}
# The engine's first turn of an agent that started after a time:
# `ms agent session execution native how lines valid`.
turn_after() { awk -v a="$1" -v t="$2" '$2 == a && $1 > t { print; exit }' "$WORK/turns.log"; }
last_line() { # the last line an agent's engine wrote: `ms n` ("0 0" before its first)
    _l=$(cat "$WORK/last-$1" 2>/dev/null) || _l=""
    case "$_l" in [0-9]*\ [0-9]*) echo "$_l" ;; *) echo "0 0" ;; esac
}
working() { # the agent's engine wrote a line within the last 3 s
    set -- "$(last_line "$1")"
    [ "$(($(now_ms) - ${1%% *}))" -lt 3000 ]
}
# An agent's branch: in the repository the workspace store keeps for its
# mission's source (a mission with none: its scratch repository), or in
# SPAWN_GIT_REPO.
branch_show() {
    for git_dir in "$SPAWN_WORKSPACE_ROOT"/.sources/*.git "$repo"; do
        if git --git-dir="$git_dir" rev-parse -q --verify "refs/heads/$1" >/dev/null 2>&1; then
            git --git-dir="$git_dir" show "refs/heads/$1:work.txt" 2>/dev/null || true
            return 0
        fi
    done
}
branch_lines() { branch_show "$1" | wc -l | tr -d ' '; }
branch_valid() {
    branch_show "$1" |
        awk 'BEGIN { ok = 1 } $0 != "work " NR { ok = 0 } END { print ok ? "valid" : "invalid" }'
}
relay_requests() { # <agent> <from ms> <to ms> [status]: requests the relay answered
    awk -v a="$1" -v f="$2" -v t="$3" -v s="${4-}" \
        '$2 == a && $1 >= f && $1 <= t && (s == "" || $4 == s) { n++ } END { print n + 0 }' "$WORK/relay.log"
}
loop_exits_since() { awk -v t="$1" '$2 == "exit" && $1 >= t { n++ } END { print n + 0 }' "$WORK/loop-runs.log"; }
# What happened to an agent after a fault, today or required: the revivals
# Spawn sent and the engine's first turn after it.
describe() { # <label> <fault ms> <command id at the fault>
    agent=$(get "$1" agent)
    session=$(get "$1" session)
    sent=$(commands_after "$session" "$3" | awk -F'|' -v t="$2" '{ printf "%s%s %s by %s %.1f s after", (n++ ? "; " : ""), $4, $3, $2, ($5 - t) / 1000 }')
    turn=$(turn_after "$agent" "$2")
    if [ -n "$turn" ]; then
        set -- "$2" $turn
        printf 'revivals [%s]; its engine worked again %s s after (%s, native %s, found %s lines, %s)' \
            "${sent:-none}" "$(secs $(($2 - $1)))" "$7" "$(printf '%s' "$6" | cut -c1-8)" "$8" "$9"
    else
        printf 'revivals [%s]; its engine never worked again' "${sent:-none}"
    fi
}
submit_as_person() { # <label> <key> <action json>
    capture "$SPAWN_BIN" submit --principal bb-person --agent "$(get "$1" agent)" \
        --session "$(get "$1" session)" --key "$2" --action "$3"
    [ "$status" -eq 0 ] || harness "submit $2 failed: $(err)"
}
# Note an agent's execution, native session, last line and the newest
# command before a fault.
snapshot() {
    session=$(get "$1" session)
    put "$1" exec "$(live_exec "$session")"
    put "$1" native "$(q "select e.kind -> 'detail' ->> 'native_session_id' from spawn_events e
        where e.session_id = '$session' and e.event_name = 'session.native_started'
        order by e.id desc limit 1")"
    put "$1" command "$(max_command)"
    # What it had written when the fault came: every line of it must be found
    # again. Read now, not after the end: by then a resumed engine has written
    # more, and the lines it found would be miscounted as lost.
    put "$1" written "$(last_line "$(get "$1" agent)" | cut -d' ' -f2)"
    put "$1" fault "$(now_ms)"
}


# --- the agents ----------------------------------------------------------------

# Six ephemeral workers of one mission, each with a delivered task (028's
# worst case) and one resident (kept by supervise.rs's liveness rule); the
# supervisor runs too, as on the host. w1..w5 stand for 028's crash, exit,
# context, model and auth workers, `stop` for its stopped one.
capture "$SPAWN_BIN" resident propose --name keeper --charter "Holds a task of the 028 mission."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name keeper
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" mission add --text "028 recovery"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
labels="w1 w2 w3 w4 w5 stop keeper"
for label in $labels; do
    if [ "$label" = keeper ]; then
        capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker keeper \
            --brief "Work through the steps until stopped. BB:WORK"
    else
        capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
            --brief "Work through the steps until stopped. BB:WORK"
    fi
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $mission")
    put "$label" task "$task"
    put "$label" agent "$(q "select agent_id from mission_tasks where id = $task")"
    put "$label" session "$(q "select session_id from mission_tasks where id = $task")"
done

start_loop

deadline=$(($(date +%s) + 120))
for label in $labels; do
    agent=$(get "$label" agent)
    until [ "$(branch_lines "$agent")" -ge 3 ] && working "$agent"; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            harness "$agent did not start working within 120 s (tasks: $(q "select string_agg(id || ':' || state || ':' || agent_id, ' ' order by id) from mission_tasks"))"
        sleep 0.5
    done
done
note "every agent works: $(for l in $labels; do printf '%s=%s ' "$l" "$(branch_lines "$(get "$l" agent)")"; done)"

# The engine worked again within <bound> ms of <from>: in the same native
# session (`resume`) or a new one (`new`), finding every line it wrote.
check_worked() { # <check> <label> <from ms> <resume|new> <bound ms>
    agent=$(get "$2" agent)
    until [ -n "$(turn_after "$agent" "$(get "$2" fault)")" ] || [ "$(now_ms)" -ge $(($3 + $5)) ]; do
        sleep 0.5
    done
    turn=$(turn_after "$agent" "$(get "$2" fault)")
    written=$(get "$2" written)
    if [ -z "$turn" ]; then
        measured_fail "($1) $agent's engine never worked again"
        return 0
    fi
    set -- "$@" $turn
    # $6 ms, $7 agent, $8 session, $9 execution, ${10} native, ${11} how, ${12} lines, ${13} valid
    put "$2" resumed "$6"
    put "$2" took "$(($6 - $3))"
    [ "$(($6 - $3))" -le "$5" ] ||
        measured_fail "($1) $agent's engine worked again $(secs $(($6 - $3))) s after; within $(secs "$5") s"
    native=$(get "$2" native)
    if [ "$4" = resume ]; then
        [ "${10}" = "$native" ] ||
            measured_fail "($1) $agent went on in another native session (${10}, was $native); it resumes its own"
    else
        [ "${10}" != "$native" ] && [ "${11}" = new ] ||
            measured_fail "($1) $agent went on in its exhausted native session ($native, ${11}); a handoff starts a new one"
    fi
    [ "${12}" -ge "${written:-0}" ] && [ "${13}" = valid ] ||
        measured_fail "($1) $agent's workspace held ${12} lines (${13}) when it went on; it had written ${written:-0}: work was lost"
}

# --- (6) a person stops a worker ------------------------------------------------------

echo "j07: [028 (6)] a person stops a worker"
snapshot stop
t0=$(now_ms)
submit_as_person stop "bb-stop-$(get stop task)" '{"type":"stop"}'
put stop stop_command "$(q "select id from spawn_commands where idempotency_key = 'bb-stop-$(get stop task)'")"
# The database is cut only once the stop is carried out and recorded (in
# 028 the outage came 45 s after it); 15 s at most, as 028's first wait.
until { [ -n "$(ended_at "$(get stop session)" "$(get stop exec)")" ] &&
    [ -n "$(interrupted "$(get stop exec)" "$(get stop fault)")" ]; } ||
    [ "$(now_ms)" -ge $((t0 + 15000)) ]; do
    sleep 0.5
done

# --- phase 2: the database is unavailable for 30 s ----------------------------------

subjects=""
for label in w1 w2 w3 w4 w5 keeper; do
    agent=$(get "$label" agent)
    if working "$agent"; then subjects="$subjects $label"; fi
done
[ -n "$subjects" ] || harness "no agent works before the database outage"
for label in $subjects; do snapshot "$label"; done
cut=$(now_ms)
: >"$WORK/db.cut"
while [ "$(now_ms)" -lt $((cut + 30000)) ]; do sleep 0.5; done
for label in $subjects; do put "$label" written "$(last_line "$(get "$label" agent)" | cut -d' ' -f2)"; done
uncut=$(now_ms)
rm -f "$WORK/db.cut"
deadline=$((uncut + 30000))
for label in $subjects; do
    agent=$(get "$label" agent)
    until [ "$(branch_lines "$agent")" -ge "$(get "$label" written)" ] || [ "$(now_ms)" -ge "$deadline" ]; do
        sleep 0.5
    done
    put "$label" caught "$(now_ms)"
done
echo "j07: [028 (7)] database cut 30 s: the loop does not exit, no engine ends, interrupted database_unavailable, wait, cleared, the outage's work on each branch within 15 s"
exits=$(loop_exits_since "$cut")
exited_at=$(awk -v t="$cut" '$2 == "exit" && $1 >= t { print $1; exit }' "$WORK/loop-runs.log")
[ "$exits" -eq 0 ] ||
    measured_fail "(7) the loop exited $(secs $((exited_at - cut))) s into a 30 s outage of its database ($(grep -c . "$WORK/loop-runs.log") starts and exits logged); it waits for the database instead"
for label in $subjects; do
    agent=$(get "$label" agent)
    exec=$(get "$label" exec)
    caught=$(get "$label" caught)
    [ "$(branch_lines "$agent")" -ge "$(get "$label" written)" ] && [ "$((caught - uncut))" -le 15000 ] ||
        measured_fail "(7) $agent's work in the outage reached its branch $(secs $((caught - uncut))) s after the database came back ($(branch_lines "$agent") of $(get "$label" written) lines); within 15 s"
    [ -z "$(ended_at "$(get "$label" session)" "$exec")" ] ||
        measured_fail "(7) $agent's execution $exec ended in the database outage ($(end_of "$(get "$label" session)" "$exec")); the engines run on"
    record=$(interrupted "$exec" "$cut")
    if [ -z "$record" ]; then
        measured_fail "(7) $agent ran through a 30 s outage of the database and no execution.interrupted names it (database_unavailable)"
    else
        json=${record#*|}
        [ "$(printf '%s' "$json" | jq -r .cause)" = database_unavailable ] &&
            [ "$(printf '%s' "$json" | jq -r .path)" = wait ] &&
            [ "$(printf '%s' "$json" | jq -r --argjson c "$cut" --argjson u "$uncut" \
                '.evidence.from_ms >= $c - 2000 and .evidence.to_ms <= $u + 15000 and .evidence.failed_passes >= 2')" = true ] ||
            measured_fail "(7) $agent's interruption is not database_unavailable, path wait, with the outage's window and failed passes: $json"
        [ -n "$(cleared "$exec" "$cut")" ] ||
            measured_fail "(7) no execution.cleared for $agent once the database answered"
    fi
    note "(7) $agent: the outage's work on its branch $(secs $((caught - uncut))) s after it ended"
done
note "(7) the loop exited $exits times in and after the outage${exited_at:+, first $(secs $((exited_at - cut))) s into it}; starts: $(awk -v t="$cut" '$2 == "start" && $1 >= t { printf "%.1f s ", ($1 - t) / 1000 }' "$WORK/loop-runs.log")"

# --- (6) judged: no sooner than 45 s after the stop (028 judged it after its
# 45 s wait), with the database outage in between ------------------------------------

while [ "$(now_ms)" -lt $((t0 + 45000)) ]; do sleep 0.5; done
echo "j07: [028 (6)] stop: interrupted stopped (the Stop's command id), final, no revival, the branch holds every line"

# (6) a person's stop: final, the work kept on its branch.
label=stop
agent=$(get $label agent)
session=$(get $label session)
exec=$(get $label exec)
written=$(last_line "$agent" | cut -d' ' -f2)
record=$(interrupted "$exec" "$(get $label fault)")
if [ -z "$record" ]; then
    measured_fail "(6) $agent was stopped by a person (command $(get $label stop_command)) and no execution.interrupted names it (execution.ended $(end_of "$session" "$exec"))"
else
    json=${record#*|}
    [ "$(printf '%s' "$json" | jq -r .cause)" = stopped ] &&
        [ "$(printf '%s' "$json" | jq -r .path)" = final ] &&
        [ "$(printf '%s' "$json" | jq -r '.evidence.command_id')" = "$(get $label stop_command)" ] ||
        measured_fail "(6) $agent's interruption is not stopped, path final, naming the stop command: $json"
fi
[ -z "$(commands_after "$session" "$(get $label command)")" ] && [ -z "$(turn_after "$agent" "$t0")" ] ||
    measured_fail "(6) $agent was revived after a person stopped it: $(describe $label "$t0" "$(get $label command)")"
[ "$(branch_lines "$agent")" -ge "$written" ] && [ "$(branch_valid "$agent")" = valid ] ||
    measured_fail "(6) $agent's branch holds $(branch_lines "$agent") lines ($(branch_valid "$agent")) after its stop; it wrote $written"
note "(6) stop: $(describe $label "$t0" "$(get $label command)"); its branch holds $(branch_lines "$agent") of $written lines"

# --- phase 3: the host restarts ----------------------------------------------------

sleep 3
subjects=""
for label in w1 w2 w3 w4 w5 keeper; do
    if working "$(get "$label" agent)"; then subjects="$subjects $label"; fi
done
[ -n "$subjects" ] || harness "no agent works before the host restart"
for label in $subjects; do snapshot "$label"; done
restart=$(now_ms)
# As systemd stops the host: the loop gets SIGTERM and finishes its pass,
# then the terminal server dies with every engine in it.
stop_loop
tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
sleep 2
for label in $subjects; do put "$label" written "$(last_line "$(get "$label" agent)" | cut -d' ' -f2)"; done
started=$(now_ms)
start_loop
deadline=$((started + 40000))
for label in $subjects; do
    agent=$(get "$label" agent)
    until [ -n "$(turn_after "$agent" "$started")" ] || [ "$(now_ms)" -ge "$deadline" ]; do sleep 0.5; done
done
echo "j07: [028 (8)] host restart: interrupted host_restart (terminal server gone), resume by key, works again in its native session within 20 s with every line"
for label in $subjects; do
    agent=$(get "$label" agent)
    session=$(get "$label" session)
    exec=$(get "$label" exec)
    record=$(interrupted "$exec" "$restart")
    if [ -z "$record" ]; then
        measured_fail "(8) $agent's engine died with the host and no execution.interrupted names it (host_restart; the Bridge recorded execution.ended $(end_of "$session" "$exec")): $(describe "$label" "$restart" "$(get "$label" command)")"
    else
        json=${record#*|}
        [ "$(printf '%s' "$json" | jq -r .cause)" = host_restart ] &&
            [ "$(printf '%s' "$json" | jq -r .path)" = resume ] &&
            [ "$(printf '%s' "$json" | jq -r '.evidence.terminal_server')" = gone ] ||
            measured_fail "(8) $agent's interruption is not host_restart, path resume, its terminal server gone: $json"
        attempt=$(attempt_of "$json")
        [ "$(revival_key "$session" "$exec" "$attempt" | cut -d'|' -f2)" = resume ] ||
            measured_fail "(8) no resume recover-$exec-$attempt for $agent after the host restart"
    fi
    check_worked 8 "$label" "$started" resume 20000
    note "(8) $agent: $(describe "$label" "$restart" "$(get "$label" command)") (the loop started $(secs $((started - restart))) s after the restart)"
done

# --- phase 4: the disk is full -------------------------------------------------------

sleep 2
label=""
for candidate in w2 w1 keeper w4 w5; do
    if working "$(get "$candidate" agent)"; then label=$candidate && break; fi
done
[ -n "$label" ] || harness "no agent works before the disk-full episode"
agent=$(get "$label" agent)
session=$(get "$label" session)
snapshot "$label"
exec=$(get "$label" exec)
full=$(now_ms)
printf '102400\n' >"$WORK/df.low"
: >"$WORK/ctl/$agent.enospc"
until [ -n "$(ended_at "$session" "$exec")" ] || [ "$(now_ms)" -ge $((full + 15000)) ]; do sleep 0.5; done
put "$label" written "$(last_line "$agent" | cut -d' ' -f2)"
while [ "$(now_ms)" -lt $((full + 15000)) ]; do sleep 0.5; done
early=$(commands_after "$session" "$(get "$label" command)")
freed=$(now_ms)
rm -f "$WORK/df.low" "$WORK/ctl/$agent.enospc"
until [ -n "$(turn_after "$agent" "$full")" ] || [ "$(now_ms)" -ge $((freed + 25000)) ]; do sleep 0.5; done
echo "j07: [028 (9)] disk full: interrupted disk_full (free under the 1 GiB floor), wait, no revival while full, cleared within 6 s, the same native session within 15 s"
record=$(interrupted "$exec" "$full")
if [ -z "$record" ]; then
    measured_fail "(9) $agent's engine failed on a full disk (ENOSPC, the workspace's filesystem read 100 MiB free under a 1 GiB floor) and no execution.interrupted names it (execution.ended $(end_of "$session" "$exec")): $(describe "$label" "$full" "$(get "$label" command)")"
else
    json=${record#*|}
    [ "$(printf '%s' "$json" | jq -r .cause)" = disk_full ] &&
        [ "$(printf '%s' "$json" | jq -r .path)" = wait ] &&
        [ "$(printf '%s' "$json" | jq -r '.evidence.free_bytes < .evidence.floor_bytes and .evidence.floor_bytes == 1073741824')" = true ] ||
        measured_fail "(9) $agent's interruption is not disk_full, path wait, with the free space under the 1 GiB floor: $json"
    [ -z "$early" ] ||
        measured_fail "(9) $agent was revived while the disk was still full: $early"
    cleared_record=$(cleared "$exec" "$full")
    if [ -z "$cleared_record" ]; then
        measured_fail "(9) no execution.cleared for $agent once the disk had room"
    else
        [ "$((${cleared_record%%|*} - freed))" -le 6000 ] ||
            measured_fail "(9) $agent's disk wait was cleared $(secs $((${cleared_record%%|*} - freed))) s after the disk had room; within 6 s"
    fi
fi
check_worked 9 "$label" "$freed" resume 15000
note "(9) disk full for 15 s ($agent): ended $(end_of "$session" "$exec"); $(describe "$label" "$full" "$(get "$label" command)")"

# --- (10) no work lost --------------------------------------------------------------

sleep 3
echo "j07: [028 (10)] no work lost: every branch of this half holds work 1..n"
for label in $labels; do
    agent=$(get "$label" agent)
    [ "$(branch_valid "$agent")" = valid ] ||
        measured_fail "(10) $agent's branch work.txt is not lines 1..n in order: work was lost or doubled"
done

note "every end recorded with its cause and path, every agent revived with its work (no line lost)"
exit 0
