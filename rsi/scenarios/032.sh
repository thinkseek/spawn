#!/bin/sh
# Black box: a soak. One agent that runs forever (an approved resident with
# a delivered task) works on a synthetic workload while faults are injected
# at seeded, deterministic times: an engine kill, a memory-limit kill, a
# database restart, a model relay outage and a disk-full episode. It
# measures the work lost per fault, the time to resume (p50, p90) and the
# uptime, and holds them to thresholds: no work lost, every fault followed
# by resumed work, and the p90 of the times to resume under a bound.
#
# The agent is a resident, `soak`, with a delivered task of a mission whose
# source is a local repository: residents are the agents Spawn keeps alive
# for good (crates/spawn-cli/src/supervise.rs, `keep_residents`), each with
# one workspace and one branch for its whole life, so a soak of one
# resident measures the core: one agent that runs forever. (The supervisor
# is kept by the same rule but works on digests, not on a task; an
# ephemeral worker exists for one task only.) A real `spawn loop` (the
# Bridge, its pusher, the dispatcher, the liveness rule; no model judge, no
# supervisor) runs it on a fake engine, and is run as spawn-loop.service
# runs it: `spawn init-db` first, started again 5 s after it ends
# (Restart=always, RestartSec=5). The loop reaches the database through a
# forwarder the scenario can cut; the engine reaches its model through a
# fake relay (SPAWN_MODEL_RELAY) the scenario can turn to 529; `df` on the
# loop's PATH reads 1 GiB free during a disk-full episode, while the
# engine's tool calls fail with Claude Code's ENOSPC message.
#
# The workload: once its task has arrived the engine makes one work unit a
# second, a model request to the relay and then a tool call that appends
# `unit <n> <execution> <ms>` to units/<n div 100>.txt in its workspace (and
# to the scenario's journal, outside it); each tool call is a checkpoint.
# It numbers on from the last unit its workspace holds, so a workspace that
# lost units would write their numbers again and the loss would show.
#
# The faults, at seeded times (BB_032_SEED, default 32): rounds of the five
# kinds, each round in a seeded order; a fault's slot is a jitter of 0-3 s,
# its outage (relay 5-10 s, database 8-20 s, disk 5-10 s; the kills are
# instants) and a 20 s window in which work must resume:
#   - engine kill: the engine's process SIGKILLed (its PID from a file it
#     writes at start, checked against its command line; never by name);
#   - memory-limit kill: with a systemd user manager, objective 029's
#     envelope (SPAWN_ENVELOPE=systemd, SPAWN_SLICE a slice of the
#     scenario's own, SPAWN_EXECUTION_MEMORY_HIGH=124M, _MAX=128M and
#     _SWAP_MAX=16M, and gate envelopes that fit under the slice: every
#     envelope is set, since the owner's defaults of 2026-11-06, 10G, 12G
#     and 4G, do not fit under its 2G slice, and MemoryHigh sits close
#     below MemoryMax because a process that grows page by page past
#     MemoryHigh is throttled, not killed: on this host 124M/128M/16M killed
#     the tool in 0.74 s, 120M/128M/16M in 13.1 s) and a tool that allocates
#     256 MiB; where there is none (the gate's sandbox has no /run and no
#     bus, and the gate gets no delegated cgroup: the owner's ruling), a
#     stand-in: the execution's runner and engine SIGKILLed at once, as the
#     kernel's group OOM kill does; the output says which. The real
#     memory-limit kill is judged by the owner's run outside the sandbox
#     (mission 7), as objective 029 is;
#   - database restart: the loop's database socket gone and every
#     connection through it cut, until the outage ends;
#   - model relay outage: the relay answers 529 overloaded_error; the
#     engine writes Claude Code's API error line and stops at its prompt;
#   - disk-full episode: as above; an engine started meanwhile exits 1.
#
# Measured:
#   - work lost: units the engine wrote that the agent's branch head does
#     not hold at the end (after the soak is over and the last turn's
#     checkpoint), split into those a recorded checkpoint held (dropped)
#     and those none did; and checkpoints recorded in the log
#     (artifact.committed) that are not on the branch's history;
#   - time to resume, per fault: from the fault (a kill) or from its
#     clearing (relay, database, disk: objective 028's wait-path causes) to
#     the recording of the first checkpoint that holds a unit written after
#     it; p50 and p90, nearest rank;
#   - uptime: 1 - (the time between consecutive checkpoints that added work,
#     beyond 5 s) / the soak's span; also outside the time a cause held.
# Checked:
#   (1) every fault is followed by resumed work within 20 s;
#   (2) no work lost: 0 units, and 0 recorded checkpoints off the branch;
#   (3) the p90 of the times to resume is at most 15 s.
# It prints a line per fault, the per-fault table (with the cause objective
# 028 recorded, not judged here: 028.sh judges it) and one line
# `METRIC soak ...`.
#
# The long run, for the owner (not part of the gate): BB_032_DURATION_SECS
# (default 180) and BB_032_SEED; the soak runs whole rounds while they fit,
# at least one. BB_032_ENVELOPE=auto|systemd|stand-in picks the memory-limit
# fault (auto: systemd when `systemd-run --user --scope` works).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1. BB_032_MEASURE=1 (only to show today's behaviour)
# reports every failed check and goes on, then exits 1 if any failed; it
# also looks on past a fault's window (up to 60 s, or the next fault) to
# say how long work took to resume.
#
# Fails with 1 at (1) on the live main (b950e25; the deployed binary is
# built from it), at the first fault of seed 32, a disk-full episode:
# the engine stops at its prompt and nothing ever tells it to go on. It
# passes once every fault is followed by resumed work within 20 s (028's
# paths: a resume, or a wait that ends when its cause clears), no work is
# lost, and the p90 of the times to resume is at most 15 s.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket and loopback only. Every wait is bounded in time.
#
# Today (2026-11-06; seed 32 and seed 7, the memory-limit fault stood in
# for): no work is lost (0 of 64 and 0 of 90 units; every recorded
# checkpoint on the branch; the source's spawn/agent_resident_soak at the
# head), but the agent stops: a disk-full episode (5.4-5.8 s) and a relay
# outage (5.9-8.7 s) leave it idle at its prompt for good (without a model
# judge nothing types into a resident, and the dispatcher's guarded nudge is
# no wait for a cause to clear; the disk floor only holds revivals, and free
# space is read once a minute); an engine kill is resumed in 2.7-3.1 s; a
# memory-limit kill in 3.0 s, but in 23.2 s when it hits an execution
# revived less than a minute before (`died_young`: a revival that ends
# within 60 s, lost, counts as a failed one and backs off 20 s, though it
# had worked); a database restart of 9.1 s costs 19.5 s after the database
# is back, one of 15.8 s costs 12.6 s (the store's connection is never
# reopened: ten failed passes, about 22.5 s, end the loop, started again 5 s
# later).
# Times to resume p50 2.7-3.1 s, p90 12.6-19.5 s over 2-3 of 5 faults;
# uptime 0.29-0.40. The plain run fails at (1) in 41 s; a full measured
# soak takes 3 min 12 s - 3 min 24 s.
# With a systemd user manager (BB_032_ENVELOPE=auto on this host, seed 32,
# two runs) the memory-limit kill is not delivered: the tool allocates
# 256 MiB past SPAWN_EXECUTION_MEMORY_MAX=128M and no execution ends (no
# envelope yet: 029 puts each execution in `<slice stem>-<execution
# id>.scope` in SPAWN_SLICE); the engine kill 2.6-2.8 s, the database
# restart 19.6-20.3 s ("10 failed passes in a row; exiting"); 84 units, 0
# lost; uptime 0.380-0.389; 3 min 6 s.
set -eu

asserted=""
harness() {
    echo "032: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE=${BB_032_MEASURE-}
SEED=${BB_032_SEED:-32}
DURATION=${BB_032_DURATION_SECS:-180}
ENVELOPE=${BB_032_ENVELOPE:-auto}
case "$SEED" in '' | *[!0-9]*) harness "BB_032_SEED must be a whole number" ;; esac
case "$DURATION" in '' | *[!0-9]*) harness "BB_032_DURATION_SECS must be a whole number of seconds" ;; esac
case "$ENVELOPE" in auto | systemd | stand-in) ;; *) harness "BB_032_ENVELOPE is auto, systemd or stand-in" ;; esac

# The thresholds (the gate's contract; not settings).
RESUME_P90_MS=15000 # the p90 of the times to resume
WINDOW_MS=20000     # a fault with no resumed work this long after it never recovered
CADENCE_MS=1000     # the engine's work unit cadence
GAP_MS=5000         # a gap in durable progress longer than this is downtime

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed awk sort comm curl python3; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
real_df=$(command -v df) || harness "df is not installed"

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_032_soak_$run"
SPAWN_TMUX_SOCKET="bb-032-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_parse "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
case "$pg_url_host" in
/*) db_target="$pg_url_host/.s.PGSQL.$pg_url_port" ;;
*) db_target="$pg_url_host:$pg_url_port" ;;
esac
# The loop reaches the database through a forwarder the scenario cuts to
# restart the database (a Unix socket in $WORK/pg); everything the scenario
# reads goes to the server directly.
loop_db_url="$pg_url_scheme://${pg_url_userinfo:+$pg_url_userinfo@}$(pg_url_encode_host "$WORK/pg"):5432/$SPAWN_DB_NAME"

database_made=""
supervisor_pid=""
relay_pid=""
forwarder_pid=""
slice=""
stop_pid() { # stop_pid <pid>: TERM, up to 10 s, then KILL
    [ -n "$1" ] || return 0
    kill "$1" 2>/dev/null || return 0
    i=0
    while kill -0 "$1" 2>/dev/null && [ "$i" -lt 100 ]; do
        sleep 0.1
        i=$((i + 1))
    done
    kill -9 "$1" 2>/dev/null || true
}
cleanup() {
    : >"$WORK/loop-stop" 2>/dev/null || true
    if [ -f "$WORK/loop.pid" ]; then stop_pid "$(cat "$WORK/loop.pid")"; fi
    stop_pid "$supervisor_pid"
    if [ -f "$WORK/loop.pid" ]; then stop_pid "$(cat "$WORK/loop.pid")"; fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    stop_pid "$relay_pid"
    stop_pid "$forwarder_pid"
    if [ -n "$slice" ]; then
        systemctl --user stop "$slice" >/dev/null 2>&1 || true
    fi
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
            echo "032: the loop's last lines:" >&2
            grep -v '^\s*$' "$WORK/loop.log" | tail -n 12 >&2
        fi
        if [ -s "$WORK/loop-runs" ]; then
            echo "032: the loop's runs (started_ms ended_ms status):" >&2
            tail -n 8 "$WORK/loop-runs" >&2
        fi
        if [ -s "$WORK/engine.log" ]; then
            echo "032: the engine's last starts and turns:" >&2
            tail -n 10 "$WORK/engine.log" >&2
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "032: harness: unexpected exit $rc" >&2
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
# A check that BB_032_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "032: (measure) would fail: $*"
    else
        fail "$*"
    fi
}

now_ms() { echo $(($(date +%s%N) / 1000000)); }
q() { scalar "$1"; }
secs() { # secs <ms>: seconds with one decimal
    awk -v ms="$1" 'BEGIN { printf "%.1f", ms / 1000 }'
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
source=$WORK/source.git
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
for bare in "$repo" "$source"; do
    git init -q --bare --initial-branch=main "$bare"
    git -C "$WORK/seed" push -q "$bare" main:main
done

# The fake engine. A prompt; text typed in opens a turn, which starts after
# a second of quiet. It writes its session transcript where Claude Code
# does, so the Bridge and the Observer see turns and tool calls as they see
# a real engine's. Once its task has arrived (`BB:SOAK`, kept as soak.task
# in the workspace), every turn is work: one work unit per second, each a
# model request to the relay its settings name, then a tool call that
# appends `unit <n> <execution> <ms>` to units/<n div 100>.txt and to the
# scenario's journal ($BB_WORK/journal, outside the workspace). It numbers
# on from the last unit its workspace holds. It fails as Claude Code does:
# - a relay answer other than 200 (after Claude Code's own retries): the
#   API error line (`isApiErrorMessage`, `error` overloaded for 529,
#   `apiErrorStatus`), `API Error: ...` on the pane, the turn failed, the
#   engine idle at its prompt;
# - a full disk ($BB_WORK/disk-full): the tool call fails with Claude
#   Code's ENOSPC message, the turn ends, the engine idles at its prompt;
#   an engine started on a full disk exits 1 at once (Claude Code cannot
#   create its temporary directory);
# - $BB_WORK/fault-alloc: the next tool allocates 256 MiB (the memory-limit
#   fault, where an envelope can confine it).
# $BB_WORK/soak-over ends the work: the turn ends and no new one works.
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
native=
settings=
resumed=
while [ $# -gt 0 ]; do
  case "$1" in
  --resume) native=$2; resumed=resume ;;
  --session-id) native=$2 ;;
  --settings) settings=$2 ;;
  esac
  shift
done
stamp() { date +%s%3N; }
printf '%s %s %s\n' "$$" "$PPID" "$SPAWN_EXECUTION_ID" >"$BB_WORK/engine.pid"
printf '%s start %s %s\n' "$(stamp)" "$SPAWN_EXECUTION_ID" "$resumed" >>"$BB_WORK/engine.log"
if [ -e "$BB_WORK/disk-full" ]; then
  printf '%s exit-enospc %s\n' "$(stamp)" "$SPAWN_EXECUTION_ID" >>"$BB_WORK/engine.log"
  printf "Error: Claude Code's temporary directory /tmp/claude-%s could not be created: the volume holding /tmp is full (ENOSPC). Free space on that volume, or set CLAUDE_CODE_TMPDIR to a directory on another one, then try again.\n" "$(id -u)"
  exit 1
fi
relay=$(printf '%s' "$settings" | jq -r '.env.ANTHROPIC_BASE_URL // empty' 2>/dev/null)
[ -n "$relay" ] || relay=$BB_RELAY
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
last_unit() {
  shard=$(ls units 2>/dev/null | sort | tail -n 1)
  if [ -n "$shard" ]; then tail -n 1 "units/$shard" | cut -d' ' -f2; else echo 0; fi
}
api_error() {
  case "$1" in
  529)
    kind=overloaded
    text='API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}'
    json='API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}'
    ;;
  401 | 403)
    kind=authentication_failed
    text="Please run /login · API Error: $1 Request not allowed"
    json=$text
    ;;
  *)
    kind=server_error
    text="API Error: $1 the model relay did not answer"
    json=$text
    ;;
  esac
  status=$1
  case "$status" in '' | *[!0-9]*) status=null ;; esac
  line "\"type\":\"assistant\",\"isApiErrorMessage\":true,\"error\":\"$kind\",\"apiErrorStatus\":$status,\"message\":{\"model\":\"<synthetic>\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"$json\"}]}"
  printf '%s api-error %s %s\n' "$(stamp)" "$SPAWN_EXECUTION_ID" "$1" >>"$BB_WORK/engine.log"
  printf '\n%s\n\n❯ ' "$text"
}
work() {
  printf '\n✻ Working… (esc to interrupt)\n\n❯ '
  n=$(last_unit)
  while :; do
    [ -e "$BB_WORK/soak-over" ] && return 0
    started=$(stamp)
    code=$(curl --noproxy '*' -s -o /dev/null -m 5 -w '%{http_code}' -X POST \
      -H 'content-type: application/json' --data '{"model":"bb-soak","max_tokens":1}' \
      "$relay/v1/messages" 2>/dev/null) || code=000
    if [ "$code" != 200 ]; then
      api_error "$code"
      return 1
    fi
    if [ -e "$BB_WORK/fault-alloc" ]; then
      rm -f "$BB_WORK/fault-alloc"
      id="toolu_${SPAWN_EXECUTION_ID}_alloc_$started"
      line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Bash\",\"input\":{\"command\":\"allocate\"}}],\"stop_reason\":\"tool_use\"}"
      printf '%s alloc %s\n' "$(stamp)" "$SPAWN_EXECUTION_ID" >>"$BB_WORK/engine.log"
      python3 -I -c "import time; b = b'x' * (256 << 20); time.sleep(3)" || true
      line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
    fi
    n=$((n + 1))
    id="toolu_${SPAWN_EXECUTION_ID}_$n"
    line "\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"$id\",\"name\":\"Write\",\"input\":{\"unit\":$n}}],\"stop_reason\":\"tool_use\"}"
    if [ -e "$BB_WORK/disk-full" ]; then
      line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"is_error\":true,\"content\":\"Disk full: a test write beside this command's output file, on the volume holding /tmp (Claude Code's temporary directory), failed with ENOSPC (the volume, or the user's quota on it, has no space left)\"}]}"
      printf '%s disk-full %s\n' "$(stamp)" "$SPAWN_EXECUTION_ID" >>"$BB_WORK/engine.log"
      printf '\ntool: Disk full: failed with ENOSPC\nclaude: The disk is full; I cannot go on until there is room.\n'
      return 2
    fi
    unit="unit $n $SPAWN_EXECUTION_ID $(stamp)"
    mkdir -p units
    printf '%s\n' "$unit" >>"units/$(printf '%04d' $((n / 100))).txt"
    printf '%s\n' "$unit" >>"$BB_WORK/journal"
    line "\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"$id\",\"content\":\"ok\"}]}"
    left=$((1000 - ($(stamp) - started)))
    if [ "$left" -gt 0 ]; then sleep "$((left / 1000)).$(printf '%03d' $((left % 1000)))"; fi
  done
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
    case "$typed" in *"BB:SOAK"*) : >soak.task ;; esac
  elif [ "$pending" = 1 ]; then
    pending=0
    rc=0
    if [ -f soak.task ] && [ ! -e "$BB_WORK/soak-over" ]; then
      printf '%s turn %s\n' "$(stamp)" "$SPAWN_EXECUTION_ID" >>"$BB_WORK/engine.log"
      work || rc=$?
    fi
    if [ "$rc" != 1 ]; then
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

# `df` as the loop runs it (`df -Pk -- <path>`): 1 GiB available on every
# path while $WORK/disk-full exists, the real `df` otherwise.
mkdir -p "$WORK/bin"
cat >"$WORK/bin/df" <<EOF
#!/bin/sh
if [ -e '$WORK/disk-full' ]; then
    for path; do :; done
    printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n'
    printf 'bbfull 104857600 103809024 1048576 99%% %s\n' "\$path"
    exit 0
fi
exec '$real_df' "\$@"
EOF
chmod 755 "$WORK/bin/df"

# The model relay: 529 overloaded_error on /v1/ while $WORK/relay-down
# exists, 200 otherwise; /healthz always answers.
cat >"$WORK/relay.py" <<'EOF'
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

work = sys.argv[1]
DOWN = json.dumps({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}).encode()
UP = json.dumps({"type": "message", "role": "assistant", "content": [{"type": "text", "text": "ok"}]}).encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, status, body):
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._send(200 if self.path == "/healthz" else 404, b"{}")

    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        if not self.path.startswith("/v1/"):
            self._send(404, b"{}")
        elif os.path.exists(os.path.join(work, "relay-down")):
            self._send(529, DOWN)
        else:
            self._send(200, UP)

    def log_message(self, *_args):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(os.path.join(work, "relay.port.partial"), "w") as out:
    out.write(str(server.server_address[1]))
os.rename(os.path.join(work, "relay.port.partial"), os.path.join(work, "relay.port"))
server.serve_forever()
EOF

# The database the loop sees: a Unix socket forwarding to the test server.
# While $WORK/db-down exists the socket is gone and every connection made
# through it is cut, as when the server restarts.
cat >"$WORK/forward.py" <<'EOF'
import os
import select
import socket
import sys
import threading
import time

listen_path, target, flag, ready = sys.argv[1:5]
lock = threading.Lock()
live = set()


def dial():
    if target.startswith("/"):
        upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        upstream.connect(target)
        return upstream
    host, port = target.rsplit(":", 1)
    return socket.create_connection((host.strip("[]"), int(port)))


def pump(source, sink):
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            sink.sendall(data)
    except OSError:
        pass
    for end in (source, sink):
        try:
            end.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        end.close()
    with lock:
        live.discard(source)
        live.discard(sink)


def serve(client):
    try:
        upstream = dial()
    except OSError:
        client.close()
        return
    with lock:
        live.update((client, upstream))
    threading.Thread(target=pump, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pump, args=(upstream, client), daemon=True).start()


listener = None
while True:
    if os.path.exists(flag):
        if listener is not None:
            listener.close()
            listener = None
            try:
                os.unlink(listen_path)
            except FileNotFoundError:
                pass
            with lock:
                victims = list(live)
                live.clear()
            for end in victims:
                try:
                    end.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        time.sleep(0.1)
        continue
    if listener is None:
        try:
            os.unlink(listen_path)
        except FileNotFoundError:
            pass
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(listen_path)
        listener.listen(64)
        open(ready, "w").close()
    readable, _, _ = select.select([listener], [], [], 0.2)
    if readable:
        try:
            client, _ = listener.accept()
        except OSError:
            continue
        serve(client)
EOF

mkdir -p "$WORK/pg"
python3 -I "$WORK/forward.py" "$WORK/pg/.s.PGSQL.5432" "$db_target" "$WORK/db-down" \
    "$WORK/forward.ready" >"$WORK/forward.log" 2>&1 &
forwarder_pid=$!
python3 -I "$WORK/relay.py" "$WORK" >"$WORK/relay.log" 2>&1 &
relay_pid=$!
i=0
while [ ! -f "$WORK/relay.port" ] || [ ! -f "$WORK/forward.ready" ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || harness "the fake relay or the database forwarder did not start: $(cat "$WORK/relay.log" "$WORK/forward.log")"
    sleep 0.1
done
relay_url="http://127.0.0.1:$(cat "$WORK/relay.port")"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_LOOP_INTERVAL_MS \
    SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS \
    SPAWN_STALL_HALTED_SECS SPAWN_ENVELOPE SPAWN_SLICE SPAWN_EXECUTION_MEMORY_HIGH \
    SPAWN_EXECUTION_MEMORY_MAX SPAWN_EXECUTION_MEMORY_SWAP_MAX SPAWN_GATE_MEMORY_HIGH \
    SPAWN_GATE_MEMORY_MAX SPAWN_GATE_MEMORY_SWAP_MAX SPAWN_SLICE_MEMORY_HIGH SPAWN_SLICE_MEMORY_MAX \
    SPAWN_SLICE_CPU_WEIGHT; do
    unset "$name" 2>/dev/null || true
done
# The scenario's own commands reach the database directly; the loop goes
# through the forwarder (below).
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_MODEL_RELAY=$relay_url
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
BB_RELAY=$relay_url
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_MISSION_FILE=$WORK/mission.md
# The loop's interval is production's default (500 ms). The disk floor is
# production's (2 GiB); the warning threshold is lowered so that only the
# fault's reading crosses it.
SPAWN_DISK_WARN_GB=3
SPAWN_DISK_FLOOR_GB=2
# Objective 028's settings, small, as 020's scenario sets its backoff.
SPAWN_RECOVER_BACKOFF_SECS=1
SPAWN_RECOVER_MODEL_BACKOFF_SECS=2
SPAWN_RECOVER_AUTH_RETRY_SECS=5
SPAWN_RECOVER_PROBE_SECS=2
# No stall is under test.
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_MODEL_RELAY BB_PROJECTS BB_WORK BB_RELAY SPAWN_TMUX_PROGRAM \
    SPAWN_WORKER SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB \
    SPAWN_RECOVER_BACKOFF_SECS SPAWN_RECOVER_MODEL_BACKOFF_SECS SPAWN_RECOVER_AUTH_RETRY_SECS \
    SPAWN_RECOVER_PROBE_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS SPAWN_STALL_REVIEW_SECS \
    SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$SPAWN_CLAUDE_PROJECTS"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/journal"
: >"$WORK/engine.log"
: >"$WORK/loop-runs"

# The memory-limit fault needs objective 029's envelope, which needs a
# systemd user manager; the gate's sandbox has none (no /run, no bus) and
# gets no delegated cgroup (the owner's ruling, 2026-11-06). There the fault
# is stood in for by what the kernel's group OOM kill does to an execution:
# its processes (the runner and the engine) killed at once; the real kill is
# judged by the owner's run outside the sandbox.
if [ "$ENVELOPE" = auto ]; then
    ENVELOPE=stand-in
    if command -v systemd-run >/dev/null 2>&1 &&
        timeout 10 systemd-run --user --scope --quiet --collect -p MemoryMax=64M true \
            >/dev/null 2>&1; then
        ENVELOPE=systemd
    fi
fi
if [ "$ENVELOPE" = systemd ]; then
    # Digits and `_` only: a `-` in a slice's name nests it in another slice.
    slice="bb032_$run.slice"
    SPAWN_ENVELOPE=systemd
    SPAWN_SLICE=$slice
    SPAWN_SLICE_MEMORY_HIGH=1536M
    SPAWN_SLICE_MEMORY_MAX=2G
    SPAWN_SLICE_CPU_WEIGHT=50
    # Every envelope set: the defaults (10G, 12G, 4G) do not fit under this
    # slice; MemoryHigh close below MemoryMax, so the tool reaches the kill.
    SPAWN_EXECUTION_MEMORY_HIGH=124M
    SPAWN_EXECUTION_MEMORY_MAX=128M
    SPAWN_EXECUTION_MEMORY_SWAP_MAX=16M
    SPAWN_GATE_MEMORY_HIGH=960M
    SPAWN_GATE_MEMORY_MAX=1G
    SPAWN_GATE_MEMORY_SWAP_MAX=64M
    export SPAWN_ENVELOPE SPAWN_SLICE SPAWN_SLICE_MEMORY_HIGH SPAWN_SLICE_MEMORY_MAX \
        SPAWN_SLICE_CPU_WEIGHT SPAWN_EXECUTION_MEMORY_HIGH SPAWN_EXECUTION_MEMORY_MAX \
        SPAWN_EXECUTION_MEMORY_SWAP_MAX SPAWN_GATE_MEMORY_HIGH SPAWN_GATE_MEMORY_MAX \
        SPAWN_GATE_MEMORY_SWAP_MAX
    echo "032: memory-limit fault: an execution envelope (systemd user manager, slice $slice, SPAWN_EXECUTION_MEMORY_HIGH=124M, _MAX=128M, _SWAP_MAX=16M) and a tool allocating 256 MiB"
else
    echo "032: memory-limit fault: stood in for (no systemd user manager here: objective 029's envelope cannot run): the execution's runner and engine killed at once, as a group OOM kill does"
fi

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- the agent ----------------------------------------------------------------

agent=agent_resident_soak
session=session_resident_soak
capture "$SPAWN_BIN" resident propose --name soak --charter "Writes numbered work units for the 032 soak."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name soak
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" mission add --text "032 soak" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"
capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker soak \
    --brief "Write numbered work units, one a second, until you are told the soak is over. BB:SOAK"
[ "$status" -eq 0 ] || harness "task assign failed: $(err)"

# The loop runs as spawn-loop.service runs it: `spawn init-db` first, and
# started again 5 s after it ends (Restart=always, RestartSec=5).
(
    while [ ! -e "$WORK/loop-stop" ]; do
        if env SPAWN_DATABASE_URL="$loop_db_url" PATH="$WORK/bin:$PATH" \
            "$SPAWN_BIN" init-db >>"$WORK/loop.log" 2>&1; then
            env SPAWN_DATABASE_URL="$loop_db_url" PATH="$WORK/bin:$PATH" \
                "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" \
                >>"$WORK/loop.log" 2>&1 &
            loop=$!
            echo "$loop" >"$WORK/loop.pid"
            started=$(now_ms)
            code=0
            wait "$loop" || code=$?
            echo "$started $(now_ms) $code" >>"$WORK/loop-runs"
        else
            echo "$(now_ms) $(now_ms) init-db" >>"$WORK/loop-runs"
        fi
        [ -e "$WORK/loop-stop" ] && break
        sleep 5
    done
) &
supervisor_pid=$!

# --- reading progress ----------------------------------------------------------

# The repository that holds the agent's branch (a mission's mirror).
branch_repo=""
find_branch() {
    [ -z "$branch_repo" ] || return 0
    for candidate in "$SPAWN_WORKSPACE_ROOT"/.sources/*.git "$repo"; do
        [ -d "$candidate" ] || continue
        if git --git-dir="$candidate" rev-parse -q --verify "refs/heads/$agent" >/dev/null 2>&1; then
            branch_repo=$candidate
            return 0
        fi
    done
    return 1
}
# progress: $WORK/progress, one line per checkpoint recorded in the log
# (artifact.committed of the agent's session) whose commit added work
# units: `<recorded_at_ms> <highest unit added> <that unit's ms>`, by
# recording time; and $WORK/added (every unit line any commit of the
# branch added) and $WORK/recorded (`<commit> <recorded_at_ms>`).
progress() {
    : >"$WORK/progress"
    : >"$WORK/added"
    find_branch || return 0
    sql "select (e.kind -> 'detail' -> 'artifact' ->> 'commit') || ' ' || e.recorded_at_ms
           from spawn_events e
          where e.session_id = '$session' and e.event_name = 'artifact.committed'
          order by e.session_seq" >"$WORK/recorded"
    git --git-dir="$branch_repo" log --first-parent --no-color --format='C:%H' -p \
        "refs/heads/$agent" -- units/ 2>/dev/null |
        awk -v added="$WORK/added" '
            /^C:/ { commit = substr($0, 3); next }
            /^\+unit [0-9]+ / {
                line = substr($0, 2)
                print line >added
                n = $2 + 0
                if (!(commit in best) || n > best[commit]) { best[commit] = n; at[commit] = $4 }
            }
            END { for (c in best) print c, best[c], at[c] }' >"$WORK/commit-units"
    awk 'NR == FNR { units[$1] = $2 " " $3; next }
         ($1 in units) { print $2, units[$1] }' "$WORK/commit-units" "$WORK/recorded" |
        sort -n -k1,1 >"$WORK/progress"
}
# resumed_after <ms>: the recording time of the first checkpoint after
# <ms> that holds a unit the engine wrote after <ms>; empty when none.
resumed_after() {
    awk -v t="$1" '$1 > t && $3 > t { print $1; exit }' "$WORK/progress"
}

# --- the soak's schedule ----------------------------------------------------------

# A linear congruential generator, so a seed gives the same schedule with
# any shell and any awk.
rng=$SEED
rand() { # rand <n>: sets r to an integer in [0, n)
    rng=$(((rng * 1103515245 + 12345) % 2147483648))
    r=$(((rng / 65536) % $1))
}
# Rounds of the five fault kinds, each round in a seeded order. A fault's
# slot: a jitter of 0-3 s, its outage (relay 5-10 s, database 8-20 s, disk
# 5-10 s; the kills are instants), then the window in which work must
# resume. Offsets are from the soak's start (the agent's first checkpointed
# unit, after a 10 s warm-up); the soak runs whole rounds while they fit
# in BB_032_DURATION_SECS, at least one.
WARMUP_MS=10000
TAIL_MS=5000
: >"$WORK/schedule"
offset=$WARMUP_MS
round=0
while :; do
    set -- kill memory relay database disk
    k1=$1 k2=$2 k3=$3 k4=$4 k5=$5
    for i in 5 4 3 2; do
        rand "$i"
        j=$((r + 1))
        eval "a=\$k$i b=\$k$j"
        eval "k$i=\$b k$j=\$a"
    done
    lines=""
    at=$offset
    for kind in "$k1" "$k2" "$k3" "$k4" "$k5"; do
        rand 3000
        inject=$((at + r))
        case "$kind" in
        relay | disk) rand 5001; outage=$((5000 + r)) ;;
        database) rand 12001; outage=$((8000 + r)) ;;
        *) outage=0 ;;
        esac
        at=$((inject + outage + WINDOW_MS))
        lines="$lines$kind $inject $outage
"
    done
    if [ "$round" -gt 0 ] && [ $((at + TAIL_MS)) -gt $((DURATION * 1000)) ]; then break; fi
    printf '%s' "$lines" >>"$WORK/schedule"
    offset=$at
    round=$((round + 1))
done
soak_ms=$((offset + TAIL_MS))
faults=$(wc -l <"$WORK/schedule" | tr -d ' ')
echo "032: seed $SEED: $faults faults in $round round(s) over $(secs "$soak_ms") s: $(awk '{ printf "%s%s@%.1fs%s", (NR > 1) ? ", " : "", $1, $2 / 1000, ($3 > 0) ? sprintf("+%.1fs", $3 / 1000) : "" }' "$WORK/schedule")"

# --- the agent starts working ----------------------------------------------------

deadline=$(($(now_ms) + 120000))
while :; do
    progress
    [ -s "$WORK/progress" ] && break
    [ "$(now_ms)" -lt "$deadline" ] ||
        harness "the resident did not start working within 120 s (no checkpoint of a work unit; engine: $(tail -n 3 "$WORK/engine.log" | tr '\n' ' '))"
    sleep 1
done
start=$(head -n 1 "$WORK/progress" | cut -d' ' -f1)
echo "032: the resident's first unit checkpointed; the soak starts"

# --- the faults ----------------------------------------------------------------

engine_pids() { # the recorded engine and runner, when they are still that engine's
    [ -f "$WORK/engine.pid" ] || return 1
    read -r engine_pid runner_pid engine_exec <"$WORK/engine.pid" || return 1
    kill -0 "$engine_pid" 2>/dev/null || return 1
    tr '\0' ' ' <"/proc/$engine_pid/cmdline" 2>/dev/null | grep -qF "$WORK/claude" || return 1
}
wait_until_ms() {
    while [ "$(now_ms)" -lt "$1" ]; do sleep 0.2; done
}
: >"$WORK/results"
number=0
undelivered=0
while read -r kind inject outage; do
    number=$((number + 1))
    wait_until_ms $((start + inject))
    injected=$(now_ms)
    note=""
    case "$kind" in
    kill)
        if engine_pids; then
            kill -9 "$engine_pid" 2>/dev/null || true
            note="engine $engine_exec SIGKILLed"
        else
            note="no engine running: not injected"
        fi
        ;;
    memory)
        if [ "$ENVELOPE" = systemd ]; then
            : >"$WORK/fault-alloc"
            note="a tool allocates 256 MiB under SPAWN_EXECUTION_MEMORY_MAX=128M"
        elif engine_pids; then
            if tr '\0' ' ' <"/proc/$runner_pid/cmdline" 2>/dev/null | grep -qF " run -- $WORK/claude"; then
                kill -9 "$runner_pid" "$engine_pid" 2>/dev/null || true
            else
                kill -9 "$engine_pid" 2>/dev/null || true
            fi
            note="stand-in: runner and engine of $engine_exec SIGKILLed at once"
        else
            note="no engine running: not injected"
        fi
        ;;
    relay) : >"$WORK/relay-down" ;;
    database) : >"$WORK/db-down" ;;
    disk) : >"$WORK/disk-full" ;;
    esac
    reference=$injected
    cleared=""
    if [ "$outage" -gt 0 ]; then
        wait_until_ms $((injected + outage))
        case "$kind" in
        relay) rm -f "$WORK/relay-down" ;;
        database) rm -f "$WORK/db-down" ;;
        disk) rm -f "$WORK/disk-full" ;;
        esac
        cleared=$(now_ms)
        reference=$cleared
    fi
    # BB_032_MEASURE=1 looks on, up to 60 s or until the next fault, to
    # report how long work took to resume when it took longer than the
    # window.
    look=$WINDOW_MS
    if [ -n "$MEASURE" ]; then
        look=60000
        next=$(sed -n "$((number + 1))p" "$WORK/schedule" | cut -d' ' -f2)
        if [ -n "$next" ] && [ $((start + next - reference)) -lt "$look" ]; then
            look=$((start + next - reference))
        fi
        [ "$look" -ge "$WINDOW_MS" ] || look=$WINDOW_MS
    fi
    resumed=""
    while :; do
        progress
        resumed=$(resumed_after "$reference")
        [ -z "$resumed" ] || break
        [ "$(now_ms)" -lt $((reference + look)) ] || break
        sleep 1
    done
    late=""
    if [ -n "$resumed" ] && [ $((resumed - reference)) -gt "$WINDOW_MS" ]; then
        late=$((resumed - reference))
        resumed=""
    fi
    if [ "$kind" = memory ] && [ "$ENVELOPE" = systemd ]; then
        ended=$(q "select count(*) from spawn_events where session_id = '$session'
                    and event_name = 'execution.ended' and recorded_at_ms between $injected and $((injected + 15000))")
        if [ "$ended" -eq 0 ]; then
            note="$note; NOT DELIVERED: no execution ended within 15 s"
            resumed="" late=""
            undelivered=$((undelivered + 1))
        fi
    fi
    causes=$(q "select coalesce(string_agg(distinct e.kind -> 'detail' -> 'record' ->> 'cause', ','), '-')
                  from spawn_events e
                 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                   and e.kind -> 'detail' -> 'record' ->> 'type' = 'execution.interrupted'
                   and e.kind -> 'detail' -> 'record' ->> 'session_id' = '$session'
                   and e.recorded_at_ms between $((injected - 1000)) and $((reference + WINDOW_MS))")
    restarts=$(awk -v a="$injected" -v b="${resumed:-$((reference + WINDOW_MS))}" '$2 >= a && $2 <= b' "$WORK/loop-runs" | wc -l | tr -d ' ')
    took=""
    [ -z "$resumed" ] || took=$((resumed - reference))
    [ -z "$late" ] || note="${note:+$note; }work resumed only after $(secs "$late") s"
    printf '%s|%s|%s|%s|%s|%s|%s|%s|%s\n' "$number" "$kind" "$injected" "${cleared:--}" \
        "${took:--}" "$causes" "$restarts" "$outage" "$note" >>"$WORK/results"
    case "$kind" in
    kill) what="an engine kill" ;;
    memory) what="a memory-limit kill" ;;
    relay) what="a model relay outage of $(secs "$outage") s" ;;
    database) what="a database restart of $(secs "$outage") s" ;;
    disk) what="a disk-full episode of $(secs "$outage") s" ;;
    esac
    from="the fault"
    [ -z "$cleared" ] || from="its clearing"
    if [ -n "$took" ] || [ "${note#*NOT DELIVERED}" != "$note" ]; then
        echo "032: fault $number/$faults, $what, at $(secs $((injected - start))) s: $([ -n "$took" ] && echo "work resumed $(secs "$took") s after $from" || echo "not delivered, the work went on (not timed)"); recorded cause [$causes]; loop runs ended $restarts${note:+; $note}"
    else
        echo "032: fault $number/$faults, $what, at $(secs $((injected - start))) s: NO work resumed within $(secs "$WINDOW_MS") s of $from; recorded cause [$causes]; loop runs ended $restarts${note:+; $note}"
        last=$(awk -v t="$injected" '$1 <= t { u = $2 } END { print u + 0 }' "$WORK/progress")
        measured_fail "(1) fault $number/$faults, $what: the resident's work did not resume within $(secs "$WINDOW_MS") s of $from (no checkpoint holds a unit written after it; the last before the fault: unit $last); the engine's last: $(tail -n 1 "$WORK/engine.log" | cut -d' ' -f2-)"
    fi
    case "$note" in *"NOT DELIVERED"*)
        measured_fail "(1) fault $number/$faults (memory): the tool allocated 256 MiB past SPAWN_EXECUTION_MEMORY_MAX=128M and no execution of the resident ended within 15 s: no envelope confined it (objective 029)"
        ;;
    esac
done <"$WORK/schedule"

# --- the end: work lost, time to resume, uptime --------------------------------------

wait_until_ms $((start + soak_ms))
: >"$WORK/soak-over"
over=$(now_ms)
# The last units reach a checkpoint: the turn ends, the Bridge checkpoints.
deadline=$((over + 30000))
while :; do
    find_branch || true
    if [ -n "$branch_repo" ]; then
        git --git-dir="$branch_repo" ls-tree --name-only "refs/heads/$agent" units/ 2>/dev/null |
            while read -r path; do git --git-dir="$branch_repo" show "refs/heads/$agent:$path"; done |
            LC_ALL=C sort >"$WORK/head-units"
    else
        : >"$WORK/head-units"
    fi
    LC_ALL=C sort "$WORK/journal" >"$WORK/journal.sorted"
    LC_ALL=C comm -23 "$WORK/journal.sorted" "$WORK/head-units" >"$WORK/lost"
    [ -s "$WORK/lost" ] || break
    [ "$(now_ms)" -lt "$deadline" ] || break
    sleep 1
done
progress
written=$(wc -l <"$WORK/journal" | tr -d ' ')
lost=$(wc -l <"$WORK/lost" | tr -d ' ')
LC_ALL=C sort -u "$WORK/added" >"$WORK/added.sorted"
dropped=$(LC_ALL=C comm -12 "$WORK/lost" "$WORK/added.sorted" | wc -l | tr -d ' ')
never=$((lost - dropped))
off_branch=0
if [ -n "$branch_repo" ]; then
    git --git-dir="$branch_repo" rev-list "refs/heads/$agent" | LC_ALL=C sort >"$WORK/history"
    cut -d' ' -f1 "$WORK/recorded" | LC_ALL=C sort -u >"$WORK/recorded-commits"
    off_branch=$(LC_ALL=C comm -23 "$WORK/recorded-commits" "$WORK/history" | wc -l | tr -d ' ')
fi
# Each lost unit belongs to the first fault injected after it was written.
awk -F'|' 'NR == FNR { at[NR] = $3; n = NR; next }
     { split($0, f, " "); ts = f[4]; owner = "end"
       for (i = 1; i <= n; i++) if (at[i] > ts) { owner = i; break }
       lost[owner]++ }
     END { for (o in lost) print o, lost[o] }' "$WORK/results" "$WORK/lost" >"$WORK/lost-by-fault"

# Times to resume, p50 and p90 (nearest rank), over the faults that resumed.
cut -d'|' -f5 "$WORK/results" | grep -v '^-$' | sort -n >"$WORK/times" || true
recovered=$(wc -l <"$WORK/times" | tr -d ' ')
rank() { # rank <percent>: the nearest-rank value of $WORK/times
    awk -v p="$1" '{ v[NR] = $1 } END { if (NR == 0) { print "-"; exit }
        r = int((p * NR + 99) / 100); if (r < 1) r = 1; print v[r] }' "$WORK/times"
}
p50=$(rank 50)
p90=$(rank 90)
# Uptime: the soak's span minus every gap between checkpoints that added
# work, beyond GAP_MS; "outside holds" leaves out the time a wait-path
# cause (relay, database, disk) still held.
awk -F'|' '$4 != "-" { print $3, $4 }' "$WORK/results" >"$WORK/holds"
uptime=$(awk -v s="$start" -v e="$over" -v g="$GAP_MS" '
    NR == FNR { hs[++nh] = $1; he[nh] = $2; next }
    { if ($1 > s && $1 <= e) t[++n] = $1 }
    function held(a, b,   i, x, y, sum) {
        sum = 0
        for (i = 1; i <= nh; i++) {
            x = hs[i] > a ? hs[i] : a; y = he[i] < b ? he[i] : b
            if (y > x) sum += y - x
        }
        return sum
    }
    END {
        prev = s; down = 0; outside = 0
        for (i = 1; i <= n + 1; i++) {
            cur = (i <= n) ? t[i] : e
            if (cur - prev > g) { down += cur - prev - g; outside += cur - prev - g - held(prev + g, cur) }
            prev = cur
        }
        span = e - s
        printf "%.3f %.3f %d", 1 - down / span, 1 - outside / span, down
    }' "$WORK/holds" "$WORK/progress")
set -- $uptime
up=$1
up_outside=$2
down_ms=$3
loop_restarts=$(wc -l <"$WORK/loop-runs" | tr -d ' ')

echo "032: fault | kind | at (s) | outage (s) | resumed after (s) | from | lost units | recorded cause | loop runs ended | note"
while IFS='|' read -r number kind injected cleared took causes restarts outage note; do
    lostn=$(awk -v f="$number" '$1 == f { print $2 }' "$WORK/lost-by-fault")
    from=injection
    [ "$cleared" = - ] || from=clearing
    printf '032: %s | %s | %s | %s | %s | %s | %s | %s | %s | %s\n' "$number" "$kind" \
        "$(secs $((injected - start)))" "$(secs "$outage")" \
        "$(case "$took:$note" in -:*"NOT DELIVERED"*) echo "not delivered" ;; -:*) echo never ;; *) secs "$took" ;; esac)" "$from" "${lostn:-0}" "$causes" \
        "$restarts" "${note:--}"
done <"$WORK/results"
endlost=$(awk '$1 == "end" { print $2 }' "$WORK/lost-by-fault")
[ -z "$endlost" ] || echo "032: units lost after the last fault: $endlost"
published=none
if [ -n "$branch_repo" ]; then
    head=$(git --git-dir="$branch_repo" rev-parse "refs/heads/$agent")
    pub=$(git --git-dir="$source" rev-parse -q --verify "refs/heads/spawn/$agent" 2>/dev/null || true)
    if [ "$pub" = "$head" ]; then
        published="at the head"
    elif [ -n "$pub" ]; then
        published="$(git --git-dir="$branch_repo" rev-list --count "$pub..$head" 2>/dev/null || echo '?') commits behind"
    fi
fi
echo "032: $written units written, $lost lost ($dropped checkpointed then dropped, $never never checkpointed); $off_branch recorded checkpoints off the branch; the source's spawn/$agent: $published"
echo "032: time to resume p50 $([ "$p50" = - ] && echo - || secs "$p50") s, p90 $([ "$p90" = - ] && echo - || secs "$p90") s over $recovered of $faults faults; uptime $up ($up_outside outside the time a cause held), $(secs "$down_ms") s down; $loop_restarts loop runs ended"
echo "METRIC soak seed=$SEED faults=$faults recovered=$recovered work_lost=$lost resume_p50_ms=$p50 resume_p90_ms=$p90 uptime=$up uptime_outside_holds=$up_outside loop_restarts=$loop_restarts envelope=$ENVELOPE"

[ "$lost" -eq 0 ] && [ "$off_branch" -eq 0 ] ||
    measured_fail "(2) work lost: $lost of $written units the engine wrote are not on $agent's branch at the end ($dropped of them were in a recorded checkpoint, $never never were), and $off_branch checkpoints recorded in the log are not on the branch"
[ "$recovered" -eq $((faults - undelivered)) ] ||
    measured_fail "(1) $((faults - undelivered - recovered)) of $faults faults were not followed by resumed work within $(secs "$WINDOW_MS") s"
if [ "$p90" != - ] && [ "$p90" -gt "$RESUME_P90_MS" ]; then
    measured_fail "(3) the p90 of the times to resume is $(secs "$p90") s, over the bound of $(secs "$RESUME_P90_MS") s (p50 $(secs "$p50") s)"
fi

if [ -n "$failed_any" ]; then
    asserted=1
    exit 1
fi
echo "032: the resident survived $faults faults: no work lost, every fault followed by resumed work, p90 $(secs "$p90") s"
exit 0
