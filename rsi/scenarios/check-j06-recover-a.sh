#!/bin/sh
# Journey j06, recover (a): an engine that crashes, exits, runs out of
# context, meets an overloaded model or an expired login is brought back,
# each by the path its cause takes, and no line of its work is lost; a
# 20 MiB file one of them wrote comes back byte for byte (design-gate-v2 C1:
# the owner's data-loss path).
#
# Folds Spawn scenario 028's faults (1)-(5) and its (10) for these agents
# (028 is split by fault kind, design-gate-journeys-spawn.md §2 and §3: its
# faults (6)-(9) and (10) for their agents are check-j07-recover-b), and
# scenario 003 whole ((a), (b), (e)), on 028's exit worker:
#   028 (1) crash: a worker's engine is killed (SIGKILL): within 5 s of its
#       end an `execution.interrupted`, cause `crashed` (evidence: signal 9),
#       path `resume`, due at once; within 5 s of it the resume named
#       `recover-<execution>-<attempt>`; the engine works again in the same
#       native session within 20 s of the end, its workspace holding every
#       line it wrote (work.txt is `work 1..n`, n at least the last written).
#   028 (2) exit: a worker's and the resident's engines exit 0 with their
#       tasks unfinished: the same, cause `exited` (evidence: exit_code 0).
#   028 (3) context: the model answers a worker's native session "prompt is
#       too long" (400): cause `context_exhausted` (evidence: its text), path
#       `handoff`, the revival a `start`; the engine works in a new native
#       session within 30 s of the failed turn, with every line.
#   028 (4) model: the model answers a worker 529 for 10 s: cause
#       `model_unavailable` (evidence: status 529), path `wait`; the j-th
#       failure in a row due no sooner than 2 x 2^(j-1) s (at most 8 s, the
#       cap of 4 x the base) after its record (each failed retry a record of
#       its own, each retry sent by its key, not before it is due); at most 4
#       requests in the 10 s; an `execution.cleared`; the same native session
#       works again within 12 s of the model's recovery; no handoff.
#   028 (5) auth: the model answers a worker 401 until the credentials file
#       ($CLAUDE_CONFIG_DIR/.credentials.json) holds a new token, written
#       14 s in: cause `model_auth` (evidence: status 401, the credentials
#       file's path), path `wait`; no revival and at most one more 401
#       before the change; `execution.cleared` within 6 s of it; the same
#       native session works again within 12 s of it.
#   028 (10) no work lost: every agent's branch (this half's six) holds
#       work.txt as `work 1..n`.
#   003 (a) the 20 MiB file (deterministic, incompressible) the exit worker
#       wrote in its first turn, and its text file, are there again, byte for
#       byte (sha256), in the fresh mount its recovery from (2) made; the
#       workspace was unmounted between the end and that mount.
#   003 (b) the text file is on the agent's branch; a 1 MiB in-place edit,
#       checkpointed when the engine exits, is a new checkpoint commit whose
#       content survives another remount (Spawn's own revival here).
#   003 (e) old checkpoints still restore: a second agent started from the
#       first checkpoint commit (`start {"from": <commit>}`) gets the
#       original bytes in its fresh mount.
#
# One database, one tmux socket, one real loop (`spawn loop`: the Bridge,
# the dispatcher, the supervisor, no model judge), run as systemd runs
# spawn-loop.service (started again 5 s after it exits), with 028's
# settings unchanged (SPAWN_RECOVER_BACKOFF_SECS=2,
# SPAWN_RECOVER_MODEL_BACKOFF_SECS=2, SPAWN_RECOVER_AUTH_RETRY_SECS=600,
# SPAWN_RECOVER_PROBE_SECS=2, SPAWN_DISK_FLOOR_GB=1). Five ephemeral workers
# and one resident, each with a delivered task, on 028's fake engine; the
# engines reach 028's fake model relay. Four workers and the resident are
# tasks of one mission (028's, no source); the exit worker is the task of a
# second mission whose source is the scenario's repository, so that its
# checkpoints are published there and 003 (e)'s fork (an agent no mission
# owns, in that repository) can start from its first checkpoint. This half
# cuts no database and fakes no disk, so it has no database forwarder and
# no fake `df` (028's (7) and (9) are j07's). Faults (1)-(5) strike at once,
# one agent each, with 028's windows (15 s, 45 s, the 10 s outage, the 14 s
# renewal); 003's edit, stop and fork follow on the exit worker.
# The control records (spawn_events), agent_executions, spawn_commands and
# mission_tasks are read where 028 and 003 read them, with their assertions.
# Traceability: /var/tmp/spawn/sync/gate-journeys-spawn-proof/trace/j06.tsv
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
# Needs SPAWN_BIN, SPAWN_TEST_DATABASE_URL, python3, curl and openssl;
# creates and drops its own database, uses a private tmux socket, loopback
# only. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "j06: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk curl python3 sha256sum openssl dd; do
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_j06_recover_$run"
SPAWN_TMUX_SOCKET="bb-j06-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
wrapper_pid=""
relay_pid=""
watch_pid=""
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
    for pid in $relay_pid $watch_pid; do
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
            echo "j06: the loop's last lines:" >&2
            grep -v '^\s*$' "$WORK/loop.log" | tail -n 12 >&2
        fi
        if [ -s "$WORK/turns.log" ]; then
            echo "j06: the engines' last turns (ms agent session execution native how lines):" >&2
            tail -n 8 "$WORK/turns.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "j06: interruption records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select r ->> 'type', r ->> 'execution_id', r ->> 'cause', r ->> 'path'
                from spawn_events e, lateral (select e.kind -> 'detail' -> 'record' as r) x
                where e.session_id = 'session_control' and r ->> 'type' like 'execution.%'
                order by e.session_seq" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j06: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this journey's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the journey database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# journey read can be trusted, and that is the harness's failure.
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
note() { echo "j06: $*"; }

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

mkdir -p "$WORK/relay" "$WORK/ctl" "$WORK/claude-config"

# 003's large file: deterministic and incompressible (an AES-CTR keystream
# with a fixed key), so it is as large as it says; and its 1 MiB edit.
MIB=1048576
keystream() {
    head -c "$2" /dev/zero |
        openssl enc -aes-256-ctr -nosalt -K "$1" -iv 00000000000000000000000000000000
}
keystream "$(printf '%064d' 3)" $((20 * MIB)) >"$WORK/big.v1"
keystream "$(printf '%064d' 7)" "$MIB" >"$WORK/patch"
cp "$WORK/big.v1" "$WORK/big.v2"
dd if="$WORK/patch" of="$WORK/big.v2" bs="$MIB" seek=7 conv=notrunc 2>/dev/null
[ "$(wc -c <"$WORK/big.v1" | tr -d ' ')" -eq $((20 * MIB)) ] ||
    harness "openssl did not produce 20 MiB"
sha_v1=$(sha256sum "$WORK/big.v1" | cut -d' ' -f1)
sha_v2=$(sha256sum "$WORK/big.v2" | cut -d' ' -f1)
[ "$sha_v1" != "$sha_v2" ] || harness "the edit changed nothing"

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

# 028's fake engine: a prompt; text typed in opens a turn, which starts
# after a second of quiet. It writes its session transcript where Claude
# Code does, so the Bridge and the Observer see turns and tool calls as they
# see a real engine's. Once its agent was given work (a typed `BB:WORK`),
# every turn works: one step a second, each a model request to the relay its
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
# 003's part, for the agent named in big-agent only: its first working turn
# (once per journey, never again after a remount) writes 003's 20 MiB file
# data/big.bin and data/notes.txt before its first step; ctl/<agent>.edit
# (removed) rewrites 1 MiB of data/big.bin in place, then exits 0, as 003's
# `edit-large` does. Both are logged in engine.log (`wrote`, `edited`).
cat >"$WORK/claude" <<'ENGINE'
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
  if [ -f "$BB_WORK/ctl/$agent.edit" ]; then
    rm -f "$BB_WORK/ctl/$agent.edit"
    dd if="$BB_WORK/patch" of=data/big.bin bs=1048576 seek=7 conv=notrunc 2>/dev/null || exit 3
    printf 'edited\n' >>"$BB_WORK/engine.log"
    printf '\nedited\n'
    exit 0
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
  if [ "$agent" = "$(cat "$BB_WORK/big-agent" 2>/dev/null)" ] && [ ! -f "$BB_WORK/big.written" ]; then
    mkdir -p data || exit 3
    cp "$BB_WORK/big.v1" data/big.bin || exit 3
    printf 'small text\n' >data/notes.txt || exit 3
    printf 'wrote\n' >>"$BB_WORK/engine.log"
    : >"$BB_WORK/big.written"
  fi
  printf '\n✻ Working… (esc to interrupt)\n\n❯ '
  while step; do :; done
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
while :; do
  if IFS= read -r -t 1 typed; then
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
i=0
until [ -s "$WORK/relay.port" ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || harness "the fake relay did not start: $(cat "$WORK/relay.out")"
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
    SPAWN_EXEC_IMAGE SPAWN_SUPERVISED http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
# No forwarder in this half: the loop reaches its database directly.
BB_LOOP_DATABASE_URL=$SPAWN_DATABASE_URL
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
# 028's disk settings, loop-wide, unchanged (no disk is faulted here).
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
    BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_SUPERVISOR SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_RECOVER_BACKOFF_SECS \
    SPAWN_RECOVER_MODEL_BACKOFF_SECS SPAWN_RECOVER_AUTH_RETRY_SECS SPAWN_RECOVER_PROBE_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/state"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/turns.log"
: >"$WORK/relay.log"
: >"$WORK/loop-runs.log"
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
# The commands into a session after a command id, but the journey's own:
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
# The repository that holds an agent's branch: the one the workspace store
# keeps for its mission's source (a mission with none: its scratch
# repository), or SPAWN_GIT_REPO; `none` when none does.
branch_git() {
    for git_dir in "$SPAWN_WORKSPACE_ROOT"/.sources/*.git "$repo"; do
        if git --git-dir="$git_dir" rev-parse -q --verify "refs/heads/$1" >/dev/null 2>&1; then
            echo "$git_dir"
            return 0
        fi
    done
    echo none
}
branch_show() {
    git_dir=$(branch_git "$1")
    [ "$git_dir" = none ] || git --git-dir="$git_dir" show "refs/heads/$1:work.txt" 2>/dev/null || true
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
# What happened to an agent after a fault: the revivals Spawn sent and the
# engine's first turn after it.
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

# Five ephemeral workers, each with a delivered task (028's worst case: the
# dispatcher resumes a worker only when its execution was `lost`, once) and
# one resident (kept by supervise.rs's liveness rule); the supervisor runs
# too, as on the host. The exit worker's task is in the second mission (its
# source the scenario's repository, for 003 (e)).
capture "$SPAWN_BIN" resident propose --name keeper --charter "Holds a task of the 028 mission."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name keeper
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" mission add --text "028 recovery"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" mission add --text "028 recovery, 003's large file" --source "$repo"
[ "$status" -eq 0 ] || harness "mission add (the large file's) failed: $(err)"
big_mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$big_mission" ] && [ "$big_mission" != null ] || harness "mission add printed no id: $(out)"
labels="crash exit context model auth keeper"
for label in $labels; do
    if [ "$label" = keeper ]; then
        capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker keeper \
            --brief "Work through the steps until stopped. BB:WORK"
        of=$mission
    elif [ "$label" = exit ]; then
        capture "$SPAWN_BIN" task assign --mission "$big_mission" --kind ephemeral --worker "$label" \
            --brief "Work through the steps until stopped. BB:WORK"
        of=$big_mission
    else
        capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker "$label" \
            --brief "Work through the steps until stopped. BB:WORK"
        of=$mission
    fi
    [ "$status" -eq 0 ] || harness "task assign ($label) failed: $(err)"
    task=$(q "select max(id) from mission_tasks where mission_id = $of")
    put "$label" task "$task"
    put "$label" agent "$(q "select agent_id from mission_tasks where id = $task")"
    put "$label" session "$(q "select session_id from mission_tasks where id = $task")"
done
big_agent=$(get exit agent)
big_session=$(get exit session)
printf '%s\n' "$big_agent" >"$WORK/big-agent"
big_workspace=$SPAWN_WORKSPACE_ROOT/$big_agent

# Whether the large file's workspace is mounted, as it changes (every
# 50 ms): `ms present|absent`. 003 looks right after each end, when no
# loop would remount it; here Spawn's revival remounts it at once.
: >"$WORK/mount.log"
(
    last=""
    while [ ! -f "$WORK/watch.stop" ]; do
        if [ -e "$big_workspace" ]; then now=present; else now=absent; fi
        if [ "$now" != "$last" ]; then
            printf '%s %s\n' "$(date +%s%3N)" "$now" >>"$WORK/mount.log"
            last=$now
        fi
        sleep 0.05
    done
) &
watch_pid=$!
# The workspace was seen unmounted between two times.
unmounted_between() { awk -v f="$1" -v t="$2" '$2 == "absent" && $1 >= f && $1 <= t { n++ } END { exit n ? 0 : 1 }' "$WORK/mount.log"; }

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
grep -qx wrote "$WORK/engine.log" || harness "the engine did not write 003's files"
note "every agent works: $(for l in $labels; do printf '%s=%s ' "$l" "$(branch_lines "$(get "$l" agent)")"; done)"

# --- the faults of one agent each, at once ------------------------------------------

for label in $labels; do snapshot "$label"; done
t0=$(now_ms)
# (1) crash: the engine is killed.
kill -KILL "$(cat "$WORK/pid-$(get crash agent)")"
# (2) exit: the engine exits 0 with its task unfinished (a worker and the resident).
: >"$WORK/ctl/$(get exit agent).exit"
: >"$WORK/ctl/$(get keeper agent).exit"
# (3) context: the model answers its native session "prompt is too long".
printf 'too_long\n' >"$WORK/relay/$(get context agent)"
# (4) model: the model API is overloaded (529) for 10 s.
printf 'overloaded\n' >"$WORK/relay/$(get model agent)"
# (5) auth: the login expired: 401 until the credentials are renewed (at 14 s).
printf 'auth %s\n' "$token_new" >"$WORK/relay/$(get auth agent)"

model_cleared=""
renewed=""
phase1() { # <until ms>: the journey's side of the timeline, every 0.5 s
    while [ "$(now_ms)" -lt "$1" ]; do
        if [ -z "$model_cleared" ] && [ "$(now_ms)" -ge $((t0 + 10000)) ]; then
            printf 'ok\n' >"$WORK/relay/$(get model agent)"
            model_cleared=$(now_ms)
        fi
        if [ -z "$renewed" ] && [ "$(now_ms)" -ge $((t0 + 14000)) ]; then
            credentials "$token_new" "$(($(date +%s) * 1000 + 3600000))"
            renewed=$(now_ms)
        fi
        sleep 0.5
    done
}
phase1 $((t0 + 15000))

# The end of an execution, and how the Bridge recorded it.
ended_info() { # <label>
    session=$(get "$1" session)
    exec=$(get "$1" exec)
    put "$1" ended "$(ended_at "$session" "$exec")"
}

# A resume path: recorded within 5 s of the end with its cause and evidence,
# due at once, resumed (`recover-<execution>-<attempt>`, the same native
# session) within 5 s of the record, its engine working again within 20 s
# of the end with every line it wrote.
check_resume() { # <check> <label> <cause> <jq test of the evidence> <what was done>
    n=$1 label=$2 cause=$3 test=$4 what=$5
    agent=$(get "$label" agent)
    session=$(get "$label" session)
    exec=$(get "$label" exec)
    ended_info "$label"
    ended=$(get "$label" ended)
    [ -n "$ended" ] || measured_fail "($n) $agent's execution $exec did not end after $what"
    written=$(get "$label" written)
    record=$(interrupted "$exec" "$(get "$label" fault)")
    if [ -z "$record" ]; then
        measured_fail "($n) $agent's execution $exec ended after $what (the Bridge recorded execution.ended $(end_of "$session" "$exec")), and no execution.interrupted names its cause; every end records its cause ($cause) and the path it takes: $(describe "$label" "$(get "$label" fault)" "$(get "$label" command)")"
        return 0
    fi
    at=${record%%|*}
    json=${record#*|}
    [ "$(printf '%s' "$json" | jq -r .cause)" = "$cause" ] &&
        [ "$(printf '%s' "$json" | jq -r .path)" = resume ] &&
        [ "$(printf '%s' "$json" | jq -r "(.evidence // {}) | $test")" = true ] ||
        measured_fail "($n) $agent's interruption after $what is not cause $cause, path resume, with its evidence: $json"
    [ -n "$ended" ] && [ "$((at - ended))" -le 5000 ] ||
        measured_fail "($n) $agent's interruption was recorded $(secs $((at - ${ended:-0}))) s after its end; within 5 s"
    [ "$(printf '%s' "$json" | jq -r --argjson at "$at" '(.due_ms // 0) <= $at + 1000')" = true ] ||
        measured_fail "($n) $agent's first resume is not due at once (due_ms by its record's time): $json"
    attempt=$(attempt_of "$json")
    revival=$(revival_key "$session" "$exec" "$attempt")
    if [ -z "$revival" ]; then
        measured_fail "($n) no command recover-$exec-$attempt resumed $agent: $(describe "$label" "$(get "$label" fault)" "$(get "$label" command)")"
    else
        [ "$(printf '%s' "$revival" | cut -d'|' -f2)" = resume ] &&
            [ "$(($(printf '%s' "$revival" | cut -d'|' -f3) - at))" -le 5000 ] ||
            measured_fail "($n) recover-$exec-$attempt is not a resume within 5 s of the record: $revival"
    fi
    check_worked "$n" "$label" "$ended" resume 20000
}
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

echo "j06: [028 (1)] crash: interrupted crashed (signal 9), resume by key, works again with every line"
check_resume 1 crash crashed '(.signal == 9 or .signal == "SIGKILL" or .signal == "KILL")' \
    "its engine was killed (SIGKILL)"
note "(1) crash: $(describe crash "$t0" "$(get crash command)")"

phase1 $((t0 + 45000))

echo "j06: [028 (2)] exit: a worker and the resident, interrupted exited (exit_code 0), resumed with every line"
check_resume 2 exit exited '.exit_code == 0' "its engine exited 0 with its task unfinished"
note "(2) exit (worker): $(describe exit "$t0" "$(get exit command)")"
check_resume 2 keeper exited '.exit_code == 0' "its engine exited 0 with its task unfinished"
note "(2) exit (resident): $(describe keeper "$t0" "$(get keeper command)")"

# (3) context exhausted: a handoff to a new native session in the same workspace.
echo "j06: [028 (3)] context: interrupted context_exhausted, handoff, a start, a new native session with every line"
label=context
agent=$(get $label agent)
session=$(get $label session)
exec=$(get $label exec)
failed_at=$(q "select min(recorded_at_ms) from spawn_events where session_id = '$session'
    and execution_id = '$exec' and event_name = 'turn.failed' and recorded_at_ms >= $t0")
# `written` stays the snapshot's, taken with the fault: the turn that
# follows must find every line the engine had written then. Reading it
# here, tens of seconds on, would count the resumed session's own work
# as lines it failed to find (task 202: "held 5 ... had written 47").
record=$(interrupted "$exec" "$(get $label fault)")
if [ -z "$record" ]; then
    measured_fail "(3) $agent's model answered 'prompt is too long' (turn.failed at ${failed_at:-never}), and no execution.interrupted names it: $(describe $label "$t0" "$(get $label command)")"
else
    json=${record#*|}
    [ "$(printf '%s' "$json" | jq -r .cause)" = context_exhausted ] &&
        [ "$(printf '%s' "$json" | jq -r .path)" = handoff ] &&
        printf '%s' "$json" | jq -e '.evidence.text | test("Prompt is too long")' >/dev/null ||
        measured_fail "(3) $agent's interruption is not context_exhausted, path handoff, naming the text: $json"
    attempt=$(attempt_of "$json")
    revival=$(revival_key "$session" "$exec" "$attempt")
    [ "$(printf '%s' "$revival" | cut -d'|' -f2)" = start ] ||
        measured_fail "(3) no start recover-$exec-$attempt began a new native session for $agent (got [$revival])"
fi
check_worked 3 $label "${failed_at:-$t0}" new 30000
note "(3) context: $(describe $label "$t0" "$(get $label command)")"

# (4) the model API overloaded for 10 s: a wait, retried on a backoff, the
# same native session going on once it answers.
echo "j06: [028 (4)] model: interrupted model_unavailable (529), wait, the backoff, at most 4 requests, cleared, the same native session"
label=model
agent=$(get $label agent)
session=$(get $label session)
exec=$(get $label exec)
# `written` stays the snapshot's (see (3)).
records=$(records_of execution.interrupted "$exec" | awk -F'|' -v t="$(get $label fault)" '$1 >= t')
first=$(printf '%s\n' "$records" | sed -n 1p)
if [ -z "$first" ]; then
    measured_fail "(4) $agent's turn failed with the model's 529 and no execution.interrupted names it: $(describe $label "$t0" "$(get $label command)")"
else
    json=${first#*|}
    [ "$(printf '%s' "$json" | jq -r .cause)" = model_unavailable ] &&
        [ "$(printf '%s' "$json" | jq -r .path)" = wait ] &&
        [ "$(printf '%s' "$json" | jq -r '.evidence.status')" = 529 ] ||
        measured_fail "(4) $agent's interruption is not model_unavailable, path wait, status 529: $json"
    # The j-th failure in a row is retried after the backoff (2 s, doubled
    # per failed retry, at most 4 x 2 s), by the command its attempt names.
    # The backoff runs from the failed turn the record names (its evidence's
    # event_id), as recover.rs computes due_ms: the failure's time, or the
    # pass's clock if later, plus the backoff. The record itself is written
    # later in that pass, how much later depends on the host's load (0.3 s
    # and 3.3 s seen at load 15 to 28), so measuring from the record's own
    # time failed correct backoffs under load (owner gate, batch 18).
    j=0
    while IFS='|' read -r at body; do
        [ -n "$at" ] || continue
        [ "$(printf '%s' "$body" | jq -r .cause)" = model_unavailable ] || continue
        j=$((j + 1))
        k=$(attempt_of "$body")
        due=$(printf '%s' "$body" | jq -r '.due_ms // 0')
        step=$((1 << (j - 1)))
        [ "$step" -le 4 ] || step=4
        turn=$(printf '%s' "$body" | jq -r '.evidence.event_id // empty')
        case "$turn" in
        '' | *[!0-9]*) turn_at="" ;;
        *) turn_at=$(sql "select recorded_at_ms from spawn_events
                             where id = $turn and event_name = 'turn.failed'") ;;
        esac
        if [ -z "$turn_at" ]; then
            measured_fail "(4) $agent's retry $j names no turn.failed as its evidence: $body"
        else
            [ "$due" -ge $((turn_at + 2000 * step)) ] ||
                measured_fail "(4) $agent's retry $j is due $(secs $((due - turn_at))) s after its failed turn (event $turn); the backoff is $((2 * step)) s"
        fi
        sent=$(revival_key "$session" "$exec" "$k")
        if [ -n "$sent" ]; then
            [ "$(printf '%s' "$sent" | cut -d'|' -f3)" -ge $((due - 500)) ] ||
                measured_fail "(4) recover-$exec-$k was sent before it was due ($sent, due $due)"
        fi
    done <<EOF
$records
EOF
fi
during=$(relay_requests "$agent" "$t0" "$model_cleared")
[ "$during" -le 4 ] ||
    measured_fail "(4) $agent's engine asked the overloaded model $during times in its 10 s; a backoff from 2 s allows 4"
ok_at=$(awk -v a="$agent" -v t="$model_cleared" '$2 == a && $1 >= t && $4 == 200 { print $1; exit }' "$WORK/relay.log")
cleared_record=$(cleared "$exec" "$model_cleared")
[ -n "$cleared_record" ] ||
    measured_fail "(4) no execution.cleared for $agent's model wait once the model answered again"
check_worked 4 $label "$model_cleared" resume 12000
[ -z "$(commands_after "$session" "$(get $label command)" | awk -F'|' '$4 == "start"')" ] ||
    measured_fail "(4) $agent was handed off to a new session for a model outage; a wait is not counted toward a handoff"
note "(4) model 529 for 10 s: $during requests in it, first answer ${ok_at:+$(secs $((ok_at - model_cleared))) s after it ended}; $(describe $label "$t0" "$(get $label command)")"

# (5) the model login expired: a wait until the credentials change.
echo "j06: [028 (5)] auth: interrupted model_auth (401, the credentials file), wait, no early revival, cleared within 6 s, the same native session"
label=auth
agent=$(get $label agent)
session=$(get $label session)
exec=$(get $label exec)
# `written` stays the snapshot's (see (3)).
record=$(interrupted "$exec" "$(get $label fault)")
if [ -z "$record" ]; then
    measured_fail "(5) $agent's turn failed with the model's 401 (login expired) and no execution.interrupted names it: $(describe $label "$t0" "$(get $label command)")"
else
    at=${record%%|*}
    json=${record#*|}
    [ "$(printf '%s' "$json" | jq -r .cause)" = model_auth ] &&
        [ "$(printf '%s' "$json" | jq -r .path)" = wait ] &&
        [ "$(printf '%s' "$json" | jq -r '.evidence.status')" = 401 ] &&
        printf '%s' "$json" | jq -e '.evidence.credentials.path | test("credentials.json$")' >/dev/null ||
        measured_fail "(5) $agent's interruption is not model_auth, path wait, status 401, naming the credentials file: $json"
    before=$(commands_after "$session" "$(get $label command)" | awk -F'|' -v r="$renewed" '$5 < r')
    [ -z "$before" ] ||
        measured_fail "(5) $agent was revived before its credentials changed: $before"
    [ "$(relay_requests "$agent" $((at + 1)) "$renewed" 401)" -le 1 ] ||
        measured_fail "(5) $agent asked the model $(relay_requests "$agent" $((at + 1)) "$renewed" 401) times with the expired login"
    cleared_record=$(cleared "$exec" "$at")
    if [ -z "$cleared_record" ]; then
        measured_fail "(5) no execution.cleared for $agent's expired login after the credentials changed"
    else
        [ "$((${cleared_record%%|*} - renewed))" -le 6000 ] ||
            measured_fail "(5) $agent's login wait was cleared $(secs $((${cleared_record%%|*} - renewed))) s after the credentials changed; within 6 s"
    fi
fi
check_worked 5 $label "$renewed" resume 12000
note "(5) login expired, renewed after $(secs $((renewed - t0))) s: $(describe $label "$t0" "$(get $label command)")"

# --- 003: the large file across the exit worker's remounts ------------------------

# 003's helpers. A command the journey submits is carried out by the loop
# (003 ran `spawn tick` itself): command_ok waits for it to finish.
submit() { # <agent> <session> <key> <action>
    capture "$SPAWN_BIN" submit --agent "$1" --session "$2" --key "$3" --action "$4"
    [ "$status" -eq 0 ] || fail "submit $3 exited $status: $(err)"
}
ended() { # ended executions of one session
    scalar "select count(*) from agent_executions
            where session_id = '$1' and ended_at_ms is not null"
}
# until_ended <session> <n>: until the session has <n> ended executions.
# Generous for a loaded host; a run that ends on time never waits.
until_ended() {
    deadline=$(($(date +%s) + 180))
    until [ "$(ended "$1")" -ge "$2" ]; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            harness "execution $2 of $1 did not end within 180 s"
        sleep 0.25
    done
}
engine_did() { grep -qx "$1" "$WORK/engine.log" 2>/dev/null; }
command_ok() { # <key> <what>: the command finished and succeeded
    deadline=$(($(date +%s) + 90))
    until [ "$(scalar "select state from spawn_commands where idempotency_key = '$1'")" = finished ]; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            harness "$2: the loop did not finish command $1 within 90 s ($(scalar "select state || ' ' || coalesce(error, '') from spawn_commands where idempotency_key = '$1'"))"
        sleep 0.25
    done
    state=$(scalar "select state || ' ' || coalesce(succeeded::text, '') || ' ' || coalesce(error, '')
                    from spawn_commands where idempotency_key = '$1'")
    case "$state" in
    "finished true "*) ;;
    *"pane never became ready"* | *"does not accept input"* | *"terminal disappeared"*)
        harness "$2: $state" ;;
    *) fail "$2: $state" ;;
    esac
}
# sha256 of a mount's large file, or "missing".
sha_in() {
    if [ -f "$1/data/big.bin" ]; then sha256sum "$1/data/big.bin" | cut -d' ' -f1; else echo missing; fi
}
head_commit() {
    git_dir=$(branch_git "$big_agent")
    [ "$git_dir" != none ] && git --git-dir="$git_dir" rev-parse --verify --quiet "refs/heads/$big_agent" || echo none
}
loop_from=$(now_ms)

# (a): Spawn's revival from (2) mounted the workspace afresh.
echo "j06: [003 (a)] the 20 MiB file and the text file are back, byte for byte, after the exit worker's recovery remounted it"
unmounted_between "$(get exit fault)" "$(get exit resumed)" ||
    fail "the workspace is still mounted after the execution ended"
[ -d "$big_workspace" ] || fail "the resume mounted no workspace"
assert_eq "$(cat "$big_workspace/data/notes.txt" 2>/dev/null || echo missing)" "small text" \
    "(a) the text file after the remount"
got=$(sha_in "$big_workspace")
[ "$got" != missing ] ||
    fail "(a) data/big.bin is missing after the remount: the large file was lost at unmount"
assert_eq "$got" "$sha_v1" "(a) sha256 of data/big.bin after the remount"

# (b) the text file is on the branch.
echo "j06: [003 (b)] the text file is on the branch; a 1 MiB in-place edit, checkpointed at the engine's exit, survives the next remount"
git --git-dir="$(branch_git "$big_agent")" cat-file -e "refs/heads/$big_agent:data/notes.txt" 2>/dev/null ||
    fail "(b) data/notes.txt is not on the branch"
commit1=$(head_commit)

# The edit, then the engine exits; Spawn revives it (a new remount).
edit_at=$(now_ms)
ended_before=$(ended "$big_session")
: >"$WORK/ctl/$big_agent.edit"
until_ended "$big_session" $((ended_before + 1))
engine_did edited || harness "the engine did not edit the file"
deadline=$(($(now_ms) + 30000))
until [ -n "$(turn_after "$big_agent" "$edit_at")" ] || [ "$(now_ms)" -ge "$deadline" ]; do sleep 0.5; done
remounted=$(turn_after "$big_agent" "$edit_at" | cut -d' ' -f1)
[ -n "$remounted" ] ||
    fail "(b) second resume: the exit worker was not revived within 30 s of its exit after the edit"
unmounted_between "$edit_at" "$remounted" ||
    fail "the workspace is still mounted after the second execution ended"
commit2=$(head_commit)
[ "$commit2" != "$commit1" ] &&
    [ "$(git --git-dir="$(branch_git "$big_agent")" show "$commit2:data/big.bin" 2>/dev/null | sha256sum | cut -d' ' -f1)" = "$sha_v2" ] ||
    fail "(b) the edit of the large file made no checkpoint commit"
assert_eq "$(sha_in "$big_workspace")" "$sha_v2" "(b) sha256 of the edited data/big.bin after the remount"

submit "$big_agent" "$big_session" k_stop '{"type":"stop"}'
command_ok k_stop "stop"
until_ended "$big_session" $((ended_before + 2))
assert_eq "$(ended "$big_session")" 3 "three executions of the session ended"
[ ! -e "$big_workspace" ] || fail "the workspace is still mounted after the stop"

# (e): the first checkpoint still restores, in a fork from its commit. The
# fork is an agent no mission owns: its branch is made in SPAWN_GIT_REPO,
# where the exit worker's checkpoints are published (spawn/<agent>).
echo "j06: [003 (e)] a second agent started from the first checkpoint commit gets the original bytes"
deadline=$(($(date +%s) + 60))
until git --git-dir="$repo" cat-file -e "$commit1^{commit}" 2>/dev/null; do
    [ "$(date +%s)" -lt "$deadline" ] ||
        harness "the first checkpoint $commit1 was not published to the scenario's repository within 60 s"
    sleep 0.5
done
fork_agent=agent_fork
fork_session=session_fork
fork_workspace=$SPAWN_WORKSPACE_ROOT/$fork_agent
submit "$fork_agent" "$fork_session" k_fork \
    "{\"type\":\"start\",\"engine\":\"claude\",\"prompt\":\"\",\"from\":\"$commit1\"}"
command_ok k_fork "start of a second agent from the first checkpoint"
assert_eq "$(git --git-dir="$repo" rev-parse --verify --quiet "refs/heads/$fork_agent" || echo none)" \
    "$commit1" "(e) the fork's branch starts at the first checkpoint"
assert_eq "$(cat "$fork_workspace/data/notes.txt" 2>/dev/null || echo missing)" "small text" \
    "(e) the text file in the fork"
assert_eq "$(sha_in "$fork_workspace")" "$sha_v1" \
    "(e) sha256 of data/big.bin in a mount of the first checkpoint (older versions must stay restorable)"
submit "$fork_agent" "$fork_session" k_fork_stop '{"type":"stop"}'
command_ok k_fork_stop "stop of the fork"
# 003's `tick exited` check: here the loop runs the passes; it must not have
# exited (and been restarted) while it carried out 003's commands.
[ "$(loop_exits_since "$loop_from")" -eq 0 ] ||
    fail "tick exited: the loop exited $(loop_exits_since "$loop_from") times: $(grep -v '^\s*$' "$WORK/loop.log" | tail -n 3)"
note "003: a 20 MiB file kept across Spawn's remounts, an edit and a fork"

# --- (10) no work lost --------------------------------------------------------------

sleep 3
echo "j06: [028 (10)] no work lost: every branch of this half holds work 1..n"
for label in $labels; do
    agent=$(get "$label" agent)
    [ "$(branch_valid "$agent")" = valid ] ||
        measured_fail "(10) $agent's branch work.txt is not lines 1..n in order: work was lost or doubled"
done

: >"$WORK/watch.stop"
note "every end recorded with its cause and path, every agent revived with its work (no line lost)"
exit 0
