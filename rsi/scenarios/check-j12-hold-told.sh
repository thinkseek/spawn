#!/bin/sh
# Journey j12, hold-told: Spawn raises, records and clears its own alerts;
# the event log, the mission sources and the transcripts are backed up off
# the host and a restore drill proves them.
#
# It folds these Spawn scenarios (rsi/scenarios/NNN.sh), whole, every
# numbered check and every assertion of each:
#   030 (1)-(9)  `spawn alerts --once|--follow` and GET /api/alerts: each
#                alert fires once per occurrence with its evidence and clears
#                with its evidence, across loop restarts (task state, failed
#                command, aborts and a halt, Postgres crash lines, memory,
#                tmpfs, an undelivered task, a stuck agent, and the records of
#                028, 029, 031 and 033);
#   031 (1)-(6)  `spawn backup dump|bundle|verify` against a local R2
#                stand-in: encrypted hourly and daily dumps, full and
#                incremental bundles, the restore drill (and its refusals),
#                the alert and the limit.
# 034 (the owner's hold) is the journey's third scenario in the design, but
# objective 034 is still open on the integrator, so its scenario is a task's
# acceptance test and is not folded yet. Its section is written:
# /data/tmp/ccd/rev-g-tmp/sj/j12/section-034.patch adds it (034 then 030 in
# sequence, 031 beside), to apply when 034 lands.
# BB_030_MEASURE and BB_031_MEASURE are not carried: a check always fails
# here, as it does in the gate (the measure-only reports are gone; no check
# went with them).
#
# Shape. Two sections side by side, each with its own database, scratch
# directory and processes, each the scenario's own world, helpers and checks
# as written:
#   030 runs a loop (restarted as 030 restarts it), a fake model judge,
#       `spawn serve` and `spawn alerts --follow` on its own database and Herdr
#       session;
#   031 has no loop, no terminal backend and no timing bound: only `spawn backup` runs,
#       against its own R2 stand-in. It shares nothing with 030, so it runs
#       beside it from the start.
#   Both run to their end, a failing one too.
# Each step echoes `j12: [NNN (n)]` before the checks it carries. The
# traceability is /var/tmp/spawn/sync/gate-journeys-spawn-proof/trace/j12.tsv.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
# Named check-*: the gate runs a failing check again, and on the base if it fails twice (design-gate-v2.md C2)
#
# Runs inside the gate's sandbox (scripts/rsi/gate.sh), from the root of the
# tree under test: needs SPAWN_BIN and SPAWN_TEST_DATABASE_URL, creates and
# drops its own databases, uses private Herdr sessions and loopback ports, no
# network. Every wait is bounded in time.
set -eu

harness() {
    echo "j12: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

# =================================================================================
# Section 030, the alerts: 030.sh from its first line of code to its last
# check, in a subshell. Its own database, Herdr session, fake judge, loop
# (restarted as 030 restarts it), serve and `spawn alerts --follow`.
# Edits: the database and session names (j12's), the assertion prefix
# `blackbox: [030]`, `j12: ` before 030's own lines, and BB_030_MEASURE
# not carried: today() and its calls (they did nothing without it), the
# measure blocks of (7), (8) and the end are gone; no check went with them.
# =================================================================================
section_030() {
asserted=""
harness() {
    echo "j12: 030: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
SPAWN_HC=${SPAWN_HERDR_CORE_PROGRAM-}
if [ -z "$SPAWN_HC" ]; then
    spawn_dir=$(dirname "$SPAWN_BIN")
    if [ -x "$spawn_dir/spawn-hc" ]; then
        SPAWN_HC=$spawn_dir/spawn-hc
    elif [ -x "$(dirname "$spawn_dir")/spawn-hc" ]; then
        SPAWN_HC=$(dirname "$spawn_dir")/spawn-hc
    elif command -v spawn-hc >/dev/null 2>&1; then
        SPAWN_HC=$(command -v spawn-hc)
    fi
fi
[ -n "$SPAWN_HC" ] && [ -x "$SPAWN_HC" ] ||
    harness "spawn-hc is not installed (set SPAWN_HERDR_CORE_PROGRAM or build sibling spawn-hc beside SPAWN_BIN)"
MEASURE="" # BB_030_MEASURE is not carried: every check fails here, as in the gate

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed curl python3 dd awk sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and isolated XDG dirs, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, herdr-core sockets/state) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/r
XDG_STATE_HOME=$WORK/state
XDG_CONFIG_HOME=$WORK/config
mkdir -m 700 "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
export XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
hc_id=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
SPAWN_DB_NAME="bb_j12_030_alerts_$run"
SPAWN_HERDR_SESSION="bb-j12-$hc_id"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_HERDR_SESSION SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
follow_pid=""
judge_pid=""
# (6)'s tmpfs directory when it is not in WORK (/dev/shm): removed by cleanup.
ram_outside=""
stop_pid() { # <pid>: TERM, then KILL after 30 s; only our own children
    [ -n "$1" ] || return 0
    kill "$1" 2>/dev/null || true
    tries=0
    while :; do
        state=$(ps -o stat= -p "$1" 2>/dev/null | cut -c1) || state=""
        [ -n "$state" ] && [ "$state" != Z ] || break
        tries=$((tries + 1))
        if [ "$tries" -ge 300 ]; then
            kill -9 "$1" 2>/dev/null || true
            break
        fi
        sleep 0.1
    done
    wait "$1" 2>/dev/null || true
}
hc() {
    env -u HERDR_SOCKET_PATH -u HERDR_SESSION -u HERDR_PANE_ID \
        HC_SESSION="$SPAWN_HERDR_SESSION" \
        HC_SOCKET="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock" \
        "$SPAWN_HC" "$@"
}
stop_herdr() {
    [ -n "${SPAWN_HERDR_SESSION-}" ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ] || return 0
    for addr in $(hc ls --json --label "spawn_owner=$SPAWN_HERDR_SESSION" 2>/dev/null |
        jq -r '.terminals[]? | select(.state == "live") | .address // empty' 2>/dev/null || true); do
        hc kill "$addr" >/dev/null 2>&1 || true
    done
    hc server stop >/dev/null 2>&1 || true
    hc_wait=0
    while [ "$hc_wait" -lt 20 ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ]; do
        hc_wait=$((hc_wait + 1))
        sleep 0.1
    done
}
cleanup() {
    stop_pid "$follow_pid"
    stop_pid "$serve_pid"
    stop_pid "$loop_pid"
    stop_pid "$judge_pid"
    stop_herdr
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    [ -z "$ram_outside" ] || rm -rf "$ram_outside"
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "j12: 030: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/follow.err" ]; then
            echo "j12: 030: spawn alerts --follow said:" >&2
            tail -n 4 "$WORK/follow.err" >&2
        fi
        if [ "$(jq length "$WORK/alerts.json" 2>/dev/null || echo 0)" -gt 0 ]; then
            echo "j12: 030: the last alerts read:" >&2
            jq -c '.[] | [.key, .state]' "$WORK/alerts.json" 2>/dev/null | tail -n 20 >&2 || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j12: 030: harness: unexpected exit $rc" >&2
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
    echo "blackbox: [030] $*" >&2
    exit 1
}
# A check that BB_030_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "j12: 030: (measure) would fail: $*"
    else
        fail "$*"
    fi
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

gate=$WORK/gate
mkdir -p "$gate"
printf '#!/bin/sh\nexit 0\n' >"$gate/check-ok.sh"
printf '# nothing protected\n' >"$gate/protected"
# The gate: no verdict (exit 3, an abort that counts) while $WORK/gate-abort
# exists, else accepted.
cat >"$WORK/fake-gate.sh" <<EOF
#!/bin/sh
if [ -e '$WORK/gate-abort' ]; then
    echo "| bb: the gate's host failed before any verdict"
    exit 3
fi
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
EOF
chmod 755 "$WORK/fake-gate.sh"

# The fake engine: a prompt; text typed in opens a turn, which ends after a
# second of quiet, the transcript's end of the turn first, then the prompt.
# A turn whose text named `BB:WRITE` writes `<name>.txt`, waits for the
# pusher to publish it (at most 30 s), then writes the report. Any other
# turn changes nothing.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
[ -n "${BB_PATH-}" ] && PATH=$BB_PATH && export PATH
native=
while [ $# -gt 0 ]; do
  case "$1" in --resume | --session-id) native=$2 ;; esac
  shift
done
dir=$BB_PROJECTS/$(printf '%s' "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$dir"
transcript=$dir/$native.jsonl
name=${SPAWN_SESSION_ID#session_resident_}
agent=agent_resident_$name
count=0
line() {
  count=$((count + 1))
  printf '{"uuid":"%s-%s","timestamp":"%s","sessionId":"%s",%s}\n' \
    "$SPAWN_EXECUTION_ID" "$count" "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$native" "$1" \
    >>"$transcript"
}
turn_ends() {
  printf '\n%s\n❯ ' "$1"
  sleep 1
  line '"type":"system","subtype":"turn_duration","durationMs":1'
}
printf 'Welcome to Claude Code\n\n❯ '
pending=0
job=
while :; do
  if bb_read_line typed; then
    printf 'you: %s\n' "$typed"
    if [ "$pending" = 0 ]; then
      line '"type":"user","message":{"role":"user","content":"prompt"}'
    fi
    pending=1
    case "$typed" in *"BB:WRITE"*) job=write ;; esac
  elif [ "$pending" = 1 ]; then
    if [ -n "$job" ]; then
      file=$name.txt
      printf '%s was here\n' "$name" >"$file"
      turn_ends "wrote $file"
      start=$(date +%s)
      until [ "$(git --git-dir="$BB_WORK/source.git" show "refs/heads/spawn/$agent:$file" 2>/dev/null)" = "$(cat "$file")" ]; do
        [ $(($(date +%s) - start)) -ge 30 ] && break
        sleep 0.2
      done
      line '"type":"user","message":{"role":"user","content":"report"}'
      sleep 2
      mkdir -p .spawn
      printf '## Outcome\ndone\n## What changed\n%s\n## Open issues\nnone\n' "$file" >.spawn/report.md
      job=
    fi
    turn_ends idle
    pending=0
  fi
done
ENGINE
chmod 755 "$WORK/claude"

# The fake model judge (crates/spawn-jev's wire shape), in a directory of
# its own.
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
        try:
            with open(os.path.join(work, "stuck-sessions")) as names:
                stuck_sessions = names.read().split()
        except OSError:
            stuck_sessions = []
        stuck = 0.9 if session in stuck_sessions else 0.05
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
: >"$WORK/stuck-sessions"
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

# The two database logs the loop reads, as postgres writes them (`%m [%p] `).
testlog=$WORK/test-pg.log
livelog=$WORK/live-pg.log
stamp() { date -u '+%Y-%m-%d %H:%M:%S.%3N UTC'; }
printf '%s [1] LOG:  database system is ready to accept connections\n' "$(stamp)" >"$testlog"
printf '%s [1] LOG:  database system is ready to accept connections\n' "$(stamp)" >"$livelog"
# (6) fills a tmpfs and expects tmpfs_high on it. Since 215, tmpfs_high reads
# a tmpfs alone: a path on any other filesystem never alerts, and in the gate
# WORK is not on a tmpfs. So: WORK/ram when WORK is on a tmpfs, else a
# directory of the scenario's own under /dev/shm (removed by cleanup). With
# neither, the host cannot run (6) as written: a harness exit (2), never a
# quiet pass without tmpfs_high (rev-j's n1 on the j12 fix).
fstype() { stat -f -c %T "$1" 2>/dev/null || echo unknown; }
ram=""
if [ "$(fstype "$WORK")" = tmpfs ]; then
    ram=$WORK/ram
    mkdir -p "$ram"
elif [ "$(fstype /dev/shm)" = tmpfs ] && [ -w /dev/shm ]; then
    ram_outside=$(mktemp -d /dev/shm/bb-j12-XXXXXX) || ram_outside=""
    ram=$ram_outside
fi
host_kinds="memory_low tmpfs_high"
# Which of the two (6) takes, always said.
if [ "$ram" = "$WORK/ram" ]; then
    echo "j12: [030 (6)] tmpfs_high reads WORK/ram (WORK is a tmpfs)"
elif [ -n "$ram" ]; then
    echo "j12: [030 (6)] tmpfs_high reads $ram (WORK is $(fstype "$WORK"), /dev/shm is a tmpfs)"
fi
[ -n "$ram" ] ||
    harness "[030 (6)] no writable tmpfs for tmpfs_high (WORK is $(fstype "$WORK"), /dev/shm is $(fstype /dev/shm)$([ -w /dev/shm ] || echo ', not writable'))"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH CLAUDE_CONFIG_DIR SPAWN_LOOP_INTERVAL_MS SPAWN_SERVE_TOKEN \
    SPAWN_SERVE_DATABASE_URL SPAWN_SERVE_LOCAL_OPEN SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS \
    SPAWN_STALL_LANDING_SECS SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS \
    SPAWN_TEST_DATABASE_LOG SPAWN_DATABASE_LOG SPAWN_ALERT_CHECK_SECS \
    SPAWN_ALERT_UNDELIVERED_SECS SPAWN_ALERT_ADMISSION_SECS \
    SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS \
    SPAWN_ALERT_MEMORY_LOW_MB SPAWN_ALERT_MEMORY_OK_MB SPAWN_ALERT_TMPFS_PATH \
    SPAWN_ALERT_TMPFS_HIGH_MB SPAWN_ALERT_TMPFS_OK_MB SPAWN_ALERT_WINDOW_SECS \
    SPAWN_ALERT_PG_QUIET_SECS SPAWN_ALERT_STUCK_IDLE_DWELL_SECS \
    SPAWN_ALERT_STUCK_QUIET_DWELL_SECS SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE \
    SPAWN_HERDR_SHARDS SPAWN_HERDR_PROGRAM SPAWN_TMUX_SOCKET SPAWN_TMUX_PROGRAM \
    TMUX TMUX_PANE TMUX_TMPDIR HERDR_SOCKET_PATH HERDR_SESSION HERDR_PANE_ID \
    HC_SOCKET HC_SESSION; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
BB_PROJECTS=$SPAWN_CLAUDE_PROJECTS
BB_WORK=$WORK
BB_PATH=$PATH
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
SPAWN_WORKER=worker-bb
SPAWN_SUPERVISOR=1
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_JUDGE_URL=http://127.0.0.1:$judge_port
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
# The alert settings this scenario runs the loop with (030's; see the header).
SPAWN_ALERT_CHECK_SECS=2
SPAWN_ALERT_UNDELIVERED_SECS=3
SPAWN_ALERT_ADMISSION_SECS=3
SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS=4
SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS=4
SPAWN_ALERT_STUCK_IDLE_DWELL_SECS=0
SPAWN_ALERT_STUCK_QUIET_DWELL_SECS=0
SPAWN_TEST_DATABASE_LOG=$testlog
SPAWN_DATABASE_LOG=$livelog
SPAWN_ALERT_TMPFS_PATH=${ram:-$WORK/ram}
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK BB_PATH SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER \
    SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_LOOP_INTERVAL_MS SPAWN_JUDGE_URL \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_ALERT_CHECK_SECS \
    SPAWN_ALERT_UNDELIVERED_SECS SPAWN_ALERT_ADMISSION_SECS \
    SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS \
    SPAWN_ALERT_STUCK_IDLE_DWELL_SECS SPAWN_ALERT_STUCK_QUIET_DWELL_SECS \
    SPAWN_TEST_DATABASE_LOG SPAWN_DATABASE_LOG SPAWN_ALERT_TMPFS_PATH
# Host thresholds the host never crosses (the usual run) and ones it always
# does (the restarted loop of (6)).
USUAL="SPAWN_ALERT_MEMORY_LOW_MB=1 SPAWN_ALERT_MEMORY_OK_MB=1 SPAWN_ALERT_TMPFS_HIGH_MB=100000000 SPAWN_ALERT_TMPFS_OK_MB=100000000 SPAWN_DISK_FLOOR_GB=0"
CROSSED="SPAWN_ALERT_MEMORY_LOW_MB=100000000 SPAWN_ALERT_MEMORY_OK_MB=100000000 SPAWN_ALERT_TMPFS_HIGH_MB=1 SPAWN_ALERT_TMPFS_OK_MB=1 SPAWN_DISK_FLOOR_GB=1000000"
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/integrator"
: >"$SPAWN_MISSION_FILE"
: >"$WORK/integrate.log"
: >"$WORK/today.txt"
: >"$WORK/provoked.txt"
echo '[]' >"$WORK/alerts.json"

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
now_ms() { echo $(($(date +%s%N) / 1000000)); }
q() { scalar "$1"; }
event_at() { q "select recorded_at_ms from spawn_events where id = $1"; }
# The event id of the first control record of a type for a task.
record_id() {
    q "select min(e.id) from spawn_events e
        where e.session_id = 'session_control'
          and e.kind -> 'detail' -> 'record' ->> 'type' = '$1'
          and e.kind -> 'detail' -> 'record' ->> 'id' = '$2'"
}
task_state() { q "select state from mission_tasks where id = $1"; }
halted() { q "select coalesce(halted, '') from mission_missions where id = $mission"; }
# Append a control record to the log as its producer writes it, without the
# fold (028's, 029's and 031's records are folded nowhere yet): its event id.
append_record() {
    q "select out_id from spawn_append_trusted('agent_control', 'session_control', null,
         'control', null, jsonb_build_object('name', 'control.recorded',
         'detail', jsonb_build_object('record', \$j\$$1\$j\$::jsonb)))"
}

start_loop() { # [NAME=value ...]: the loop, with these settings over the exported ones
    env "$@" "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}
stop_loop() {
    stop_pid "$loop_pid"
    loop_pid=""
}

integrate_once() {
    capture env -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        "$SPAWN_BIN" integrate --once --root "$WORK/integrator"
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate exited $status: $(err | tail -n 3)"
    return 0
}

# The supervisor gets a digest only while its latest judgement covers its
# latest fact: a line is typed into it now and then (not as `human`), as
# objective 013's and 020's scenarios do. Only today's measures read digests.
pings=0
next_ping=0
ping_supervisor() {
    [ "$(now)" -ge "$next_ping" ] || return 0
    next_ping=$(($(now) + 8))
    pings=$((pings + 1))
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_supervisor \
        --session session_supervisor --key "bb-ping-$pings" \
        --action '{"type":"input","text":"ping"}'
    [ "$status" -eq 0 ] || harness "submit of a ping to the supervisor failed: $(err)"
}
pause() { # <seconds>, pinging the supervisor
    until_s=$(($(now) + $1))
    while [ "$(now)" -lt "$until_s" ]; do
        ping_supervisor
        sleep 0.5
    done
}
missing=""
# The alerts now, from `spawn alerts --once --json`, in $WORK/alerts.json.
refresh() {
    if [ -n "$missing" ]; then
        echo '[]' >"$WORK/alerts.json"
        return 0
    fi
    capture "$SPAWN_BIN" alerts --once --json
    if [ "$status" -ne 0 ] || ! jq -e 'type == "array"' "$WORK/out.txt" >/dev/null 2>&1; then
        fail "\`spawn alerts --once --json\` exited $status: $(err | head -n 2 | tr '\n' ' ')$(head -c 200 "$WORK/out.txt")"
    fi
    cp "$WORK/out.txt" "$WORK/alerts.json"
}
# The alerts of a kind and subject in the latest read, as a JSON array.
matching() {
    jq -c --arg k "$1" --arg s "$2" '[.[] | select(.kind == $k and .subject == $s)]' "$WORK/alerts.json"
}
# wait_for <seconds> <kind> <subject> <state: open, cleared, notice or any>
# [<count>]: read the alerts every 0.5 s until that many of that kind and
# subject are in that state; 1 when the time ran out. Without `spawn alerts`
# (today, BB_030_MEASURE=1) it only lets the time pass, at most 8 s, so that
# today's paths can show the condition.
wait_for() {
    limit=$(($(now) + $1))
    if [ -n "$missing" ]; then
        if [ "$1" -lt 8 ]; then pause "$1"; else pause 8; fi
        refresh
        return 1
    fi
    while :; do
        refresh
        got=$(matching "$2" "$3" | jq --arg w "$4" '[.[] | select($w == "any" or .state == $w)] | length')
        [ "$got" -ge "${5:-1}" ] && return 0
        [ "$(now)" -lt "$limit" ] || return 1
        ping_supervisor
        sleep 0.5
    done
}
# one <kind> <subject> <what>: exactly one alert of that kind and subject in
# the latest read; sets $alert (its JSON) and $key. 1 when not.
alert=""
key=""
one() {
    alert=""
    key=""
    list=$(matching "$1" "$2")
    count=$(printf '%s' "$list" | jq length)
    if [ "$count" -ne 1 ]; then
        measured_fail "$3: $count alerts of kind $1 for $2, wanted exactly one: $(printf '%s' "$list" | jq -c '[.[] | {key, state, fired_at_ms}]' | head -c 400)"
        return 1
    fi
    alert=$(printf '%s' "$list" | jq -c '.[0]')
    key=$(printf '%s' "$alert" | jq -r '.key')
    return 0
}
field() { printf '%s' "$alert" | jq -r "$1"; }
# fired <seconds> <kind> <subject> <what> <condition>: wait for the alert,
# then exactly one of it (sets $alert, $key); 1 (reported) when not.
fired() {
    if ! wait_for "$1" "$2" "$3" any; then
        measured_fail "$4: $5, and no $2 alert for $3 within $1 s"
        return 1
    fi
    one "$2" "$3" "$4"
}
# provoked <final state>: the alert in $alert is one the scenario made; (9)
# checks it again everywhere.
provoked() { printf '%s %s\n' "$key" "$1" >>"$WORK/provoked.txt"; }
# within <what> <from ms> <at most ms> [<at least ms>]: the alert in $alert
# fired within that window after <from ms>.
within() {
    fired=$(field '.fired_at_ms // empty')
    [ -n "$fired" ] || {
        measured_fail "$1: the alert has no fired_at_ms: $alert"
        return 0
    }
    delay=$((fired - $2))
    echo "j12: 030: $1: fired $delay ms after its condition"
    if [ "$delay" -gt "$3" ] || [ "$delay" -lt "${4:-0}" ]; then
        measured_fail "$1: fired $delay ms after its condition; wanted ${4:-0}-$3 ms"
    fi
}
# cleared <seconds> <kind> <subject> <what>: wait until that alert is
# cleared; sets $alert.
cleared() {
    if ! wait_for "$1" "$2" "$3" cleared; then
        measured_fail "$4: the $2 alert for $3 was not cleared within $1 s"
        return 1
    fi
    one "$2" "$3" "$4" || return 1
    [ "$(field '.cleared_evidence | type')" = object ] && [ "$(field '.cleared_evidence | length')" -gt 0 ] ||
        measured_fail "$4: the clearing of $key names no evidence: $alert"
    return 0
}

# --- the mission ----------------------------------------------------------------

capture "$SPAWN_BIN" resident propose --name r1 --charter "Owns the 030 file."
[ "$status" -eq 0 ] || harness "resident propose failed: $(err)"
capture "$SPAWN_BIN" resident approve --name r1
[ "$status" -eq 0 ] || harness "resident approve failed: $(err)"
capture "$SPAWN_BIN" mission add --text "030 alerts" --source "$source"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
mission=$(jq -r .mission "$WORK/out.txt")
[ -n "$mission" ] && [ "$mission" != null ] || harness "mission add printed no id: $(out)"

# shellcheck disable=SC2086
start_loop $USUAL
capture "$SPAWN_BIN" mission configure --id "$mission" --gate "$gate" --accept gate
[ "$status" -eq 0 ] || harness "mission configure failed: $(err)"

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
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
api() { # GET /api/alerts into $WORK/api.json; prints the status code
    code=""
    i=0
    while [ "$i" -lt 50 ]; do
        i=$((i + 1))
        code=$(curl -s -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/alerts" 2>/dev/null) || code=""
        [ -n "$code" ] && [ "$code" != 000 ] && break
        sleep 0.1
    done
    printf '%s' "$code"
}

"$SPAWN_BIN" alerts --follow --json >"$WORK/follow.jsonl" 2>"$WORK/follow.err" &
follow_pid=$!

# --- (1) the command, the stream and the API ----------------------------------------
echo "j12: [030 (1)] spawn alerts --once --json, GET /api/alerts and spawn alerts --follow --json answer"

capture "$SPAWN_BIN" alerts --once --json
if [ "$status" -ne 0 ] || ! jq -e 'type == "array"' "$WORK/out.txt" >/dev/null 2>&1; then
    missing=1
    measured_fail "(1) \`spawn alerts --once --json\` exited $status ($(err | head -n 1)$(head -c 120 "$WORK/out.txt")): Spawn raises no alerts of its own; today the supervisor's hand-run monitor (fleet-monitor.sh) prints them in one pane, unrecorded"
fi
code=$(api)
if [ "$code" != 200 ] || ! jq -e 'type == "array"' "$WORK/api.json" >/dev/null 2>&1; then
    measured_fail "(1) GET /api/alerts answered $code, not a JSON array: $(head -c 120 "$WORK/api.json" | tr '\n' ' ')"
fi
sleep 1
kill -0 "$follow_pid" 2>/dev/null && [ "$(ps -o stat= -p "$follow_pid" 2>/dev/null | cut -c1)" != Z ] ||
    measured_fail "(1) \`spawn alerts --follow --json\` ended at once: $(head -n 1 "$WORK/follow.err")"

# --- (2) task state notices ---------------------------------------------------------
echo "j12: [030 (2)] task #T1's task_state notices, one per record, each within 5 s"

capture "$SPAWN_BIN" task assign --mission "$mission" --kind resident --worker r1 \
    --brief "Write your file. BB:WRITE"
[ "$status" -eq 0 ] || harness "task assign to r1 failed: $(err)"
t1=$(jq -r .task "$WORK/out.txt")
[ -n "$t1" ] && [ "$t1" != null ] || harness "task assign printed no id"
deadline=$(($(now) + 120))
until [ "$(task_state "$t1")" = reported ]; do
    [ "$(now)" -lt "$deadline" ] || harness "task #$t1 was not reported within 120 s (state $(task_state "$t1"))"
    ping_supervisor
    sleep 0.5
done
git --git-dir="$source" cat-file -e "refs/heads/spawn/agent_resident_r1:r1.txt" 2>/dev/null ||
    harness "r1's pushed branch does not hold r1.txt"
t1_assigned=$(record_id task.assigned "$t1")
t1_delivered=$(record_id task.delivered "$t1")
t1_reported=$(record_id task.reported "$t1")
wait_for 5 task_state "task #$t1" notice 3 || true
notices=$(matching task_state "task #$t1")
if [ "$(printf '%s' "$notices" | jq length)" -ne 3 ] ||
    [ "$(printf '%s' "$notices" | jq -r 'sort_by(.fired_event_id) | map(.evidence.to) | join(" ")')" != "assigned delivered reported" ]; then
    measured_fail "(2) task #$t1 went assigned -> delivered -> reported; its task_state notices are $(printf '%s' "$notices" | jq -c 'map({key, to: .evidence.to})'), wanted one per record, in that order"
else
    for pair in "assigned $t1_assigned" "delivered $t1_delivered" "reported $t1_reported"; do
        set -- $pair
        alert=$(printf '%s' "$notices" | jq -c --arg to "$1" 'map(select(.evidence.to == $to)) | .[0]')
        key=$(field .key)
        [ "$key" = "task_state:$t1:$2" ] && [ "$(field .evidence.event_id)" = "$2" ] &&
            [ "$(field .state)" = notice ] ||
            measured_fail "(2) task #$t1's notice of its task.$1 record (event $2) is $alert; wanted key task_state:$t1:$2, state notice, evidence.event_id $2"
        within "(2) task #$t1 $1" "$(event_at "$2")" 5000
        provoked notice
    done
fi

# --- (3) a failed command -------------------------------------------------------------
echo "j12: [030 (3)] a failed command: one command_failed notice within 5 s"

capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_resident_nobody \
    --session session_resident_nobody --key "bb-030-fail-$run" \
    --action '{"type":"input","text":"anyone there?"}'
[ "$status" -eq 0 ] || harness "submit of the input to session_resident_nobody failed: $(err)"
cmd=$(q "select id from spawn_commands where idempotency_key = 'bb-030-fail-$run'")
deadline=$(($(now) + 20))
until [ "$(q "select coalesce(succeeded::text, '') from spawn_commands where id = $cmd")" = false ]; do
    [ "$(now)" -lt "$deadline" ] ||
        harness "the input to a session with no execution did not fail within 20 s (command $cmd: $(q "select state || ' ' || coalesce(succeeded::text, '-') from spawn_commands where id = $cmd"))"
    sleep 0.3
done
cmd_error=$(q "select left(coalesce(error, ''), 60) from spawn_commands where id = $cmd")
cmd_done=$(q "select finished_at_ms from spawn_commands where id = $cmd")
if fired 5 command_failed "command #$cmd" "(3) the failed command" "command #$cmd failed ($cmd_error)"; then
    [ "$key" = "command_failed:$cmd" ] && field '.evidence | tostring' | grep -qF -- "$cmd_error" ||
        measured_fail "(3) the failed command's notice is $alert; wanted key command_failed:$cmd and its error ($cmd_error)"
    within "(3) command #$cmd failed" "$cmd_done" 5000
    provoked notice
fi

# --- (4) aborts and a halt -------------------------------------------------------------
echo "j12: [030 (4)] one mission_state notice per aborted gating, the halt's alert, cleared by the resume"

touch "$WORK/gate-abort"
passes=0
until [ -n "$(halted)" ]; do
    passes=$((passes + 1))
    [ "$passes" -le 20 ] ||
        harness "the mission did not halt after 20 passes of an aborting gate (gate_aborts $(q "select gate_aborts from mission_missions where id = $mission"); integrate: $(tail -n 3 "$WORK/integrate.log" | tr '\n' ' '))"
    integrate_once
    sleep 0.3
done
halt_event=$(q "select max(e.id) from spawn_events e
                 where e.session_id = 'session_control'
                   and e.kind -> 'detail' -> 'record' ->> 'type' = 'mission.halted'
                   and e.kind -> 'detail' -> 'record' ->> 'id' = '$mission'")
halt_at=$(event_at "$halt_event")
reason=$(halted)
echo "j12: 030: the mission halted after $passes integrator passes: $reason"
if fired 5 mission_halted "mission #$mission" "(4) the halt" "mission #$mission halted ($reason)"; then
    [ "$key" = "mission_halted:$mission:$halt_event" ] && [ "$(field .evidence.reason)" = "$reason" ] &&
        [ "$(field .state)" = open ] ||
        measured_fail "(4) the halt's alert is $alert; wanted key mission_halted:$mission:$halt_event, state open, evidence.reason [$reason]"
    within "(4) mission #$mission halted" "$halt_at" 5000
    capture "$SPAWN_BIN" alerts --once
    [ "$status" -eq 0 ] && out | grep -F "mission_halted" | grep -qF "mission #$mission" ||
        measured_fail "(4) \`spawn alerts --once\` (text) does not show the open mission_halted alert of mission #$mission: $(head -c 300 "$WORK/out.txt")"
    for n in 1 2 3; do
        c=$(matching mission_state "mission #$mission" | jq --argjson n "$n" 'map(select(.evidence.record == "task.gated" and .evidence.after.gate_aborts == $n)) | length')
        [ "$c" -eq 1 ] ||
            measured_fail "(4) $c mission_state notices of a task.gated record for mission #$mission with after.gate_aborts $n; wanted one per abort"
    done
fi
capture "$SPAWN_BIN" mission resume --id "$mission"
[ "$status" -eq 0 ] || harness "mission resume failed: $(err)"
resumed_event=$(q "select max(e.id) from spawn_events e
                    where e.session_id = 'session_control'
                      and e.kind -> 'detail' -> 'record' ->> 'type' = 'mission.resumed'
                      and e.kind -> 'detail' -> 'record' ->> 'id' = '$mission'")
if [ -z "$missing" ] && cleared 5 mission_halted "mission #$mission" "(4) the resumed halt"; then
    [ "$(field .cleared_evidence.event_id)" = "$resumed_event" ] ||
        measured_fail "(4) the halt's clearing names $(field '.cleared_evidence | tostring'), not the mission.resumed record (event $resumed_event)"
    provoked cleared
fi

# --- (5) Postgres crash lines --------------------------------------------------------------
echo "j12: [030 (5)] crash lines in the test and live logs: one postgres_crash each, cleared by the ready line"

crash_at=$(now_ms)
s=$(stamp)
{
    printf '%s [1] LOG:  server process (PID 4242) was terminated by signal 11: Segmentation fault\n' "$s"
    printf '%s [1] LOG:  terminating any other active server processes\n' "$s"
    printf '%s [1] LOG:  all server processes terminated; reinitializing\n' "$s"
} >>"$testlog"
printf '%s [77] ERROR:  could not resize shared memory segment "/PostgreSQL.2051" to 8388608 bytes: No space left on device\n' \
    "$s" >>"$livelog"
for pair in "test terminated by signal 11" "live could not resize shared memory"; do
    which=${pair%% *}
    text=${pair#* }
    if fired 10 postgres_crash "$which" "(5) the $which database's crash line" "a crash line ($text) in the $which database's log"; then
        field '.evidence | tostring' | grep -qF -- "$text" ||
            measured_fail "(5) the $which database's postgres_crash alert does not quote the line ($text): $alert"
        within "(5) the $which database's crash line" "$crash_at" 10000
    fi
done
if [ -z "$missing" ]; then
    pause 5 # two more readings: nothing fires again
    refresh
    for which in test live; do
        one postgres_crash "$which" "(5) the $which database's log read again" || true
    done
    s=$(stamp)
    printf '%s [1] LOG:  database system is ready to accept connections\n' "$s" >>"$testlog"
    printf '%s [1] LOG:  database system is ready to accept connections\n' "$s" >>"$livelog"
    for which in test live; do
        if cleared 10 postgres_crash "$which" "(5) the $which database back"; then
            field '.cleared_evidence | tostring' | grep -qF "ready to accept connections" ||
                measured_fail "(5) the $which database's clearing does not quote the ready line: $(field '.cleared_evidence | tostring')"
            provoked cleared
        fi
    done
fi

# --- (6) host thresholds and an undelivered task, across restarts ------------------------------
echo "j12: [030 (6)] memory_low, tmpfs_high and task_undelivered across restarts, then cleared"

[ -z "$ram" ] || dd if=/dev/zero of="$ram/fill" bs=1048576 count=4 2>/dev/null || harness "cannot write $ram/fill"
stop_loop
restart_at=$(now_ms)
# shellcheck disable=SC2086
start_loop $CROSSED
for kind in $host_kinds; do
    if fired 20 "$kind" host "(6) $kind" "the loop restarted with a $kind threshold the host crosses"; then
        within "(6) $kind after the restart" "$restart_at" 20000
    fi
done
if [ -z "$missing" ]; then
    one memory_low host "(6) memory_low" &&
        [ "$(field '.evidence.measured_mb < .evidence.threshold_mb')" = true ] &&
        [ "$(field .evidence.threshold_mb)" = 100000000 ] ||
        measured_fail "(6) the memory_low alert's evidence lacks the measured MiB under the threshold (100000000): $alert"
    if [ -n "$ram" ]; then
        one tmpfs_high host "(6) tmpfs_high" &&
            [ "$(field '.evidence.measured_mb > .evidence.threshold_mb')" = true ] &&
            [ "$(field .evidence.threshold_mb)" = 1 ] && [ "$(field .evidence.path)" = "$ram" ] ||
            measured_fail "(6) the tmpfs_high alert's evidence lacks the used MiB over the threshold (1) and the path ($ram): $alert"
    fi
fi
capture "$SPAWN_BIN" task assign --mission "$mission" --kind ephemeral --worker hold \
    --brief "Nothing to do. BB:NOOP"
[ "$status" -eq 0 ] || harness "task assign to a new worker failed: $(err)"
t2=$(jq -r .task "$WORK/out.txt")
t2_assigned=$(record_id task.assigned "$t2")
t2_at=$(event_at "$t2_assigned")
if fired 11 task_undelivered "task #$t2" "(6) task #$t2 undelivered" "task #$t2 stayed assigned (its new worker cannot start under the disk floor)"; then
    [ "$key" = "task_undelivered:$t2:$t2_assigned" ] ||
        measured_fail "(6) task #$t2's undelivered alert has key $key, not task_undelivered:$t2:$t2_assigned"
    within "(6) task #$t2 undelivered" "$t2_at" 8000 3000
fi
[ "$(task_state "$t2")" = assigned ] ||
    harness "task #$t2 left assigned ($(task_state "$t2")) while the disk floor held new executions"
if [ -z "$missing" ]; then
    pause 5 # more readings: nothing fires again
    refresh
    for kind in $host_kinds; do one "$kind" host "(6) $kind read again" || true; done
fi
stop_loop
# shellcheck disable=SC2086
start_loop $USUAL
for kind in $host_kinds; do
    if [ -z "$missing" ] && cleared 20 "$kind" host "(6) $kind under the usual thresholds"; then
        provoked cleared
    fi
done
deadline=$(($(now) + 60))
until [ "$(task_state "$t2")" != assigned ]; do
    [ "$(now)" -lt "$deadline" ] || harness "task #$t2 was not delivered within 60 s of the usual restart"
    sleep 0.5
done
t2_delivered=$(record_id task.delivered "$t2")
if [ -z "$missing" ] && cleared 10 task_undelivered "task #$t2" "(6) task #$t2 delivered"; then
    [ "$(field .cleared_evidence.event_id)" = "$t2_delivered" ] ||
        measured_fail "(6) task #$t2's undelivered alert was cleared naming $(field '.cleared_evidence | tostring'), not its task.delivered record (event $t2_delivered)"
    provoked cleared
fi

# --- (7) an agent stuck by its latest judgement -------------------------------------------
echo "j12: [030 (7)] agent_stuck fires once after a stuck judgement and clears after a not-stuck one"

poke() { # <text>: a line typed into r1 as an outsider (not a loop)
    capture "$SPAWN_BIN" submit --principal bb-harness --agent agent_resident_r1 \
        --session session_resident_r1 --key "bb-030-poke-$run-$1" \
        --action "{\"type\":\"input\",\"text\":\"BB:NOOP $1\"}"
    [ "$status" -eq 0 ] || harness "submit of a line to r1 failed: $(err)"
}
echo session_resident_r1 >"$WORK/stuck-sessions"
stuck_from=$(now_ms)
poke stuck-1
if fired 30 agent_stuck session_resident_r1 "(7) r1 stuck" "r1 was judged stuck (0.9) after a turn"; then
    obs=$(field '.evidence.event_id // empty')
    stuck1_key=$key
    [ -n "$obs" ] && [ "$(field '.evidence.stuck >= 0.5')" = true ] ||
        measured_fail "(7) r1's agent_stuck alert names no judgement (evidence.event_id) with stuck >= 0.5: $alert"
    if [ -n "$obs" ]; then within "(7) r1 judged stuck" "$(event_at "$obs")" 5000; fi
fi
[ "$(grep -c 'session_resident_r1 0.9' "$WORK/judge.log" || true)" -ge 1 ] ||
    harness "the fake judge was never asked about r1 after its turn"
if [ -z "$missing" ]; then
    pause 3
    refresh
    one agent_stuck session_resident_r1 "(7) r1 stays one alert within the same stuck stretch" || true
    # Starting another working turn (#111 / PR #133) clears the open stuck
    # alert on rules/progress; finishing that new turn still stuck (0.9)
    # opens one new stuck alert.
    poke stuck-2
    deadline=$(($(now) + 20))
    while :; do
        refresh
        stuck1_state=$(matching agent_stuck session_resident_r1 | jq -r --arg k "$stuck1_key" 'map(select(.key == $k)) | .[0].state // empty')
        open_stuck=$(matching agent_stuck session_resident_r1 | jq '[.[] | select(.state == "open")] | length')
        [ "$stuck1_state" = cleared ] && [ "$open_stuck" -eq 1 ] && break
        [ "$(now)" -lt "$deadline" ] || break
        ping_supervisor
        sleep 0.5
    done
    alert=$(matching agent_stuck session_resident_r1 | jq -c --arg k "$stuck1_key" 'map(select(.key == $k)) | .[0] // {}')
    key=$stuck1_key
    [ "$(field '.state // empty')" = cleared ] &&
        [ "$(field '(.cleared_evidence.judge == "rules") or (.cleared_evidence.progress_event != null)')" = true ] ||
        measured_fail "(7) r1's first agent_stuck ($stuck1_key) was not cleared by working/progress after stuck-2: $alert"
    provoked cleared
    open_list=$(matching agent_stuck session_resident_r1 | jq -c '[.[] | select(.state == "open")]')
    [ "$(printf '%s' "$open_list" | jq length)" -eq 1 ] ||
        measured_fail "(7) expected 1 open agent_stuck for r1 after stuck-2, got $(matching agent_stuck session_resident_r1)"
    alert=$(printf '%s' "$open_list" | jq -c '.[0]')
    key=$(printf '%s' "$alert" | jq -r '.key')
    stuck2_key=$key
    : >"$WORK/stuck-sessions"
    poke unstuck
    deadline=$(($(now) + 30))
    while :; do
        refresh
        stuck2_state=$(matching agent_stuck session_resident_r1 | jq -r --arg k "$stuck2_key" 'map(select(.key == $k)) | .[0].state // empty')
        open_stuck=$(matching agent_stuck session_resident_r1 | jq '[.[] | select(.state == "open")] | length')
        [ "$stuck2_state" = cleared ] && [ "$open_stuck" -eq 0 ] && break
        [ "$(now)" -lt "$deadline" ] || break
        ping_supervisor
        sleep 0.5
    done
    alert=$(matching agent_stuck session_resident_r1 | jq -c --arg k "$stuck2_key" 'map(select(.key == $k)) | .[0] // {}')
    key=$stuck2_key
    if [ "$(field '.state // empty')" = cleared ]; then
        [ "$(field '((.cleared_evidence.stuck // 1) < 0.5) or (.cleared_evidence.judge == "rules") or (.cleared_evidence.progress_event != null)')" = true ] ||
            measured_fail "(7) r1's agent_stuck clearing names no not-stuck or working evidence: $(field '.cleared_evidence | tostring')"
        provoked cleared
    else
        measured_fail "(7) r1 no longer stuck: $stuck2_key was not cleared within 30 s: $alert"
    fi
fi

# --- (8) the records of 028, 029, 031 and 033 ------------------------------------------
echo "j12: [030 (8)] the records of 028, 029, 031 and 033 fire their alerts and the later ones clear them"

max_id=$(q "select max(id) from spawn_events")
sha=$(printf 'bb-030-%s' "$run" | sha256sum | cut -d' ' -f1)
wait_rec=$(append_record '{"type": "admission.waiting", "kind": "execution", "agent_id": "agent_worker_bb_wait", "session_id": "session_worker_bb_wait", "envelope_bytes": 6442450944, "measure": "host", "measured_bytes": 2147483648, "threshold_bytes": 6442450944}')
dump1=$(append_record "{\"type\": \"backup.dumped\", \"keys\": [\"postgres/hourly/spawn-bb-1.dump.gpg\"], \"sha256\": \"$sha\", \"bytes\": 2400000, \"events\": 8106, \"max_event_id\": $max_id}")
bundle1=$(append_record "{\"type\": \"backup.bundled\", \"kind\": \"full\", \"keys\": [\"bundles/full/spawn-bb-1.tar.gpg\"], \"sha256\": \"$sha\", \"bytes\": 31000000}")
drill_bad=$(append_record "{\"type\": \"backup.verified\", \"key\": \"postgres/hourly/spawn-bb-1.dump.gpg\", \"ok\": false, \"reason\": \"the restored database holds 8105 events; the dump's record says 8106\", \"events\": 8105, \"max_event_id\": $max_id, \"expected\": {\"events\": 8106, \"max_event_id\": $max_id}}")
auth=$(append_record '{"type": "execution.interrupted", "agent_id": "agent_worker_bb_auth", "session_id": "session_worker_bb_auth", "execution_id": "exec_bb_auth", "cause": "model_auth", "evidence": {"error": "authentication_failed", "status": 401, "text": "Please run /login · API Error: 401"}, "path": "wait", "attempt": 1, "due_ms": null}')
# 033's alert before this objective: the third base in a row on T1's mission
# that disagreed with itself, a host fault with `alert` true (no fold: T1's
# state and the mission's counters do not move, so no other notice).
gbd=$(append_record "{\"type\": \"task.gated\", \"id\": $t1, \"verdict\": \"host_fault\", \"evidence\": \"base_disagrees\", \"scenario\": \"011.sh\", \"exits\": [1, 0], \"setup_s\": 244, \"base_disagrees\": 3, \"host_faults\": 3, \"escalate\": false, \"alert\": true, \"backoff_ms\": 16000, \"due_ms\": $(($(now_ms) + 16000)), \"reason\": \"host fault: the base disagrees with itself: 011.sh exited 1, then 0 on the base (setup 244 s); not a verdict, judged again later\"}")
for id in "$wait_rec" "$dump1" "$bundle1" "$drill_bad" "$auth" "$gbd"; do
    [ -n "$id" ] || harness "a record could not be appended to the log"
done
check_fire() { # <what> <kind> <subject> <record id> <wait s> <at least ms> <at most ms> <key>
    if fired "$5" "$2" "$3" "$1" "its record is event $4"; then
        [ "$key" = "$8" ] && [ "$(field .evidence.event_id)" = "$4" ] ||
            measured_fail "$1: the alert is $alert; wanted key $8 and evidence.event_id $4"
        within "$1" "$(event_at "$4")" "$7" "$6"
    fi
}
check_fire "(8) the admission wait" admission_wait agent_worker_bb_wait "$wait_rec" 10 3000 8000 "admission_wait:$wait_rec"
check_fire "(8) the hourly dump's age" backup_stale dump "$dump1" 10 4000 9000 "backup_stale:dump:$dump1"
check_fire "(8) the daily bundle's age" backup_stale bundle "$bundle1" 10 4000 9000 "backup_stale:bundle:$bundle1"
check_fire "(8) the failed drill" drill_failed drill "$drill_bad" 5 0 5000 "drill_failed:$drill_bad"
check_fire "(8) the model login" model_auth session_worker_bb_auth "$auth" 5 0 5000 "model_auth:$auth"
if fired 5 gate_base_disagrees "mission #$mission" "(8) 033's base-disagreement alert" "its record is event $gbd"; then
    [ "$key" = "gate_base_disagrees:$gbd" ] && [ "$(field .evidence.event_id)" = "$gbd" ] &&
        [ "$(field .state)" = notice ] && [ "$(field .evidence.scenario)" = 011.sh ] ||
        measured_fail "(8) 033's base-disagreement alert is $alert; wanted key gate_base_disagrees:$gbd, state notice, evidence.event_id $gbd and scenario 011.sh"
    within "(8) 033's base-disagreement alert" "$(event_at "$gbd")" 5000
    provoked notice
fi
if [ -z "$missing" ]; then
    waited=$(($(now_ms) - $(event_at "$wait_rec")))
    admitted=$(append_record "{\"type\": \"admission.admitted\", \"kind\": \"execution\", \"agent_id\": \"agent_worker_bb_wait\", \"session_id\": \"session_worker_bb_wait\", \"envelope_bytes\": 6442450944, \"waited_ms\": $waited}")
    # The dump that clears the stale one also finds R2 over its alert
    # threshold (031's `over_alert`).
    dump2=$(append_record "{\"type\": \"backup.dumped\", \"keys\": [\"postgres/hourly/spawn-bb-2.dump.gpg\"], \"sha256\": \"$sha\", \"bytes\": 2400100, \"events\": 8107, \"max_event_id\": $max_id, \"over_alert\": true, \"held_bytes\": 8100000000, \"written_bytes\": 2400100, \"alert_bytes\": 8000000000, \"limit_bytes\": 10000000000, \"skipped\": []}")
    bundle2=$(append_record "{\"type\": \"backup.bundled\", \"kind\": \"incremental\", \"keys\": [\"bundles/incr/spawn-bb-2.tar.gpg\"], \"sha256\": \"$sha\", \"bytes\": 31000100}")
    drill_ok=$(append_record "{\"type\": \"backup.verified\", \"key\": \"postgres/hourly/spawn-bb-2.dump.gpg\", \"ok\": true, \"reason\": \"\", \"events\": 8107, \"max_event_id\": $max_id, \"expected\": {\"events\": 8107, \"max_event_id\": $max_id}}")
    auth_ok=$(append_record "{\"type\": \"execution.cleared\", \"execution_id\": \"exec_bb_auth\", \"session_id\": \"session_worker_bb_auth\", \"cause\": \"model_auth\", \"evidence\": {\"credentials\": \"changed\"}, \"waited_ms\": $waited}")
    for check in "admission_wait agent_worker_bb_wait $admitted" "backup_stale dump $dump2" \
        "backup_stale bundle $bundle2" "drill_failed drill $drill_ok" \
        "model_auth session_worker_bb_auth $auth_ok"; do
        set -- $check
        # A later occurrence (the newer dump growing old) has a key of its
        # own: the cleared one is the first.
        if wait_for 8 "$1" "$2" cleared; then
            alert=$(matching "$1" "$2" | jq -c 'sort_by(.fired_event_id) | .[0]')
            key=$(field .key)
            [ "$(field .state)" = cleared ] && [ "$(field .cleared_evidence.event_id)" = "$3" ] ||
                measured_fail "(8) $1 for $2 was cleared naming $(field '.cleared_evidence | tostring'), not the record that cleared it (event $3)"
            provoked cleared
        else
            measured_fail "(8) $1 for $2 was not cleared within 8 s of the record that clears it (event $3)"
        fi
    done
    if fired 5 backup_budget r2 "(8) R2 over its alert threshold (031)" "its record is event $dump2 (over_alert true)"; then
        [ "$key" = "backup_budget:$dump2" ] && [ "$(field .evidence.event_id)" = "$dump2" ] &&
            [ "$(field .evidence.held_bytes)" = 8100000000 ] && [ "$(field .evidence.alert_bytes)" = 8000000000 ] ||
            measured_fail "(8) R2's alert is $alert; wanted key backup_budget:$dump2, evidence.event_id $dump2, held_bytes 8100000000 and alert_bytes 8000000000"
        within "(8) R2 over its alert threshold (031)" "$(event_at "$dump2")" 5000
        dump3=$(append_record "{\"type\": \"backup.dumped\", \"keys\": [\"postgres/hourly/spawn-bb-3.dump.gpg\"], \"sha256\": \"$sha\", \"bytes\": 2400200, \"events\": 8108, \"max_event_id\": $max_id, \"over_alert\": false, \"held_bytes\": 7900000000, \"written_bytes\": 2400200, \"alert_bytes\": 8000000000, \"limit_bytes\": 10000000000, \"skipped\": []}")
        if cleared 8 backup_budget r2 "(8) R2 back under its alert threshold (031)"; then
            [ "$(field .cleared_evidence.event_id)" = "$dump3" ] ||
                measured_fail "(8) R2's alert was cleared naming $(field '.cleared_evidence | tostring'), not the record under the threshold (event $dump3)"
            provoked cleared
        fi
    fi
fi

# --- (9) the same alerts everywhere, once ---------------------------------------------------
echo "j12: [030 (9)] every alert once, the same in spawn alerts --once, GET /api/alerts and --follow"

if [ -z "$missing" ]; then
    refresh
    cp "$WORK/alerts.json" "$WORK/once.json"
    code=$(api)
    [ "$code" = 200 ] || measured_fail "(9) GET /api/alerts answered $code"
    sleep 1
    while read -r k want; do
        o=$(jq -c --arg k "$k" '[.[] | select(.key == $k)]' "$WORK/once.json")
        a=$(jq -c --arg k "$k" '[.[] | select(.key == $k)] | map({state, fired_event_id, cleared_event_id})' "$WORK/api.json" 2>/dev/null || echo '[]')
        [ "$(printf '%s' "$o" | jq length)" -eq 1 ] && [ "$(printf '%s' "$o" | jq -r '.[0].state')" = "$want" ] ||
            measured_fail "(9) \`spawn alerts --once\` shows $k as $(printf '%s' "$o" | jq -c 'map(.state)'), wanted one, $want"
        [ "$(printf '%s' "$o" | jq -c 'map({state, fired_event_id, cleared_event_id})')" = "$a" ] ||
            measured_fail "(9) GET /api/alerts shows $k as $a; spawn alerts --once as $(printf '%s' "$o" | jq -c 'map({state, fired_event_id, cleared_event_id})')"
        f=$(jq -c --arg k "$k" 'select(.key == $k and .transition == "fired")' "$WORK/follow.jsonl" 2>/dev/null | wc -l)
        c=$(jq -c --arg k "$k" 'select(.key == $k and .transition == "cleared")' "$WORK/follow.jsonl" 2>/dev/null | wc -l)
        first_fired=$(jq -r --arg k "$k" 'select(.key == $k and .transition == "fired") | input_line_number' "$WORK/follow.jsonl" 2>/dev/null | head -n 1)
        first_cleared=$(jq -r --arg k "$k" 'select(.key == $k and .transition == "cleared") | input_line_number' "$WORK/follow.jsonl" 2>/dev/null | head -n 1)
        if [ "$want" = cleared ]; then wc=1; else wc=0; fi
        [ "$f" -eq 1 ] && [ "$c" -eq "$wc" ] && { [ "$wc" -eq 0 ] || [ "$first_fired" -lt "$first_cleared" ]; } ||
            measured_fail "(9) the --follow stream holds $f fired and $c cleared lines for $k (fired at line ${first_fired:-none}, cleared at ${first_cleared:-none}); wanted 1 fired and $wc cleared after it"
    done <"$WORK/provoked.txt"
    twice=$(q "select coalesce(string_agg(t || ' ' || k || ' x' || n, '; '), '') from (
                 select e.kind -> 'detail' -> 'record' ->> 'type' as t,
                        e.kind -> 'detail' -> 'record' ->> 'key' as k, count(*) as n
                   from spawn_events e
                  where e.session_id = 'session_control'
                    and e.kind -> 'detail' -> 'record' ->> 'type' in ('alert.fired', 'alert.cleared')
                  group by 1, 2 having count(*) > 1) d")
    [ -z "$twice" ] || measured_fail "(9) the log records an alert transition more than once: $twice"
    echo "j12: 030: $(wc -l <"$WORK/provoked.txt") alerts checked in spawn alerts --once, /api/alerts and --follow"
fi
echo "j12: 030: every alert fired once with its evidence and cleared with its evidence, the same in spawn alerts --once, --follow and /api/alerts"
exit 0
}

# =================================================================================
# Section 031, the backups: 031.sh from its first line of code to its last
# check, in a subshell, beside the others. Its own database, R2 stand-in,
# HOME and GNUPGHOME. Edits: the database name (j12's), the assertion
# prefix `blackbox: [031]`, `j12: ` before 031's own lines, and
# BB_031_MEASURE not carried: measure_today() and the measure branch of
# (1) are gone; the check that branch reported is the fail after it.
# =================================================================================
section_031() {
asserted=""
harness() {
    echo "j12: 031: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE="" # BB_031_MEASURE is not carried: every check fails here, as in the gate

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql pg_dump pg_restore gpg git jq od sha256sum python3 tar gzip awk sed cmp; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and isolated XDG dirs, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
# The runtime directory too (backlog S20): what Spawn keeps there stays
# in WORK, never in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/r
XDG_STATE_HOME=$WORK/state
XDG_CONFIG_HOME=$WORK/config
mkdir -m 700 "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
export XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_j12_031_backup_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
db_password=${PGPASSWORD-}

database_made=""
r2_pid=""
R2=$WORK/r2
GNUPGHOME=$WORK/gnupg
export GNUPGHOME
cleanup() {
    if [ -n "$r2_pid" ]; then
        kill "$r2_pid" 2>/dev/null || true
        wait "$r2_pid" 2>/dev/null || true
    fi
    if command -v gpgconf >/dev/null 2>&1 && [ -d "$GNUPGHOME" ]; then
        gpgconf --homedir "$GNUPGHOME" --kill gpg-agent >/dev/null 2>&1 || true
        # gpg puts a scratch home's agent sockets in /run/user/<uid>/gnupg/d.*
        # whatever XDG_RUNTIME_DIR says (S20): that directory goes too.
        gpgconf --homedir "$GNUPGHOME" --remove-socketdir >/dev/null 2>&1 || true
    fi
    if [ -n "$database_made" ]; then
        # A scratch database a drill named and left behind goes too, but only
        # a name of this run's (it contains this run's id): candidate code may
        # name any database in a backup.verified record, and a database a
        # parallel gate is using is not this run's to drop.
        for left in $(psql -d "$SPAWN_DB_NAME" -tA -c "select distinct e.kind -> 'detail' -> 'record' ->> 'database'
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' = 'backup.verified'" 2>/dev/null); do
            case "$left" in
            *"$run"*) sh "$controller/scripts/blackbox/scenario-db.sh" drop "$left" >/dev/null 2>&1 || true ;;
            esac
        done
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    if [ "$rc" -ne 0 ]; then
        if [ -s "$R2/requests.log" ]; then
            echo "j12: 031: the stand-in's last requests:" >&2
            tail -n 12 "$R2/requests.log" >&2
        fi
        if [ -s "$R2/err" ]; then
            echo "j12: 031: the stand-in's errors:" >&2
            tail -n 8 "$R2/err" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "j12: 031: the backup records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select e.id, left((e.kind -> 'detail' -> 'record')::text, 300)
                from spawn_events e where e.session_id = 'session_control'
                 and e.kind -> 'detail' -> 'record' ->> 'type' like 'backup.%' order by e.id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "j12: 031: harness: unexpected exit $rc" >&2
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
    echo "blackbox: [031] $*" >&2
    exit 1
}
# A check that BB_031_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "j12: 031: (measure) would fail: $*"
    else
        fail "$*"
    fi
}

# --- the world ----------------------------------------------------------------

# Nothing of the owner's: no R2 or backup setting from outside, HOME and
# git's config in the scratch.
for name in SPAWN_R2_ENDPOINT SPAWN_R2_BUCKET SPAWN_R2_ACCESS_KEY_ID SPAWN_R2_SECRET_ACCESS_KEY \
    SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_FILE SPAWN_BACKUP_DRILL_DATABASE_URL SPAWN_BACKUP_R2_BUDGET_MB \
    SPAWN_BACKUP_R2_ALERT_MB SPAWN_BACKUP_R2_LIMIT_MB SPAWN_BACKUP_TRANSCRIPT_DAYS \
    SPAWN_BACKUP_PG_BIN SPAWN_BACKUP_DUMP_COMPRESS SPAWN_ENV_FILE SPAWN_BACKUP_DIR XDG_CONFIG_HOME \
    SPAWN_WORKSPACE_ROOT SPAWN_GIT_REPO; do
    unset "$name" 2>/dev/null || true
done
HOME=$WORK/home
mkdir -p "$HOME/.config/spawn"
export HOME
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
mkdir -m 700 "$GNUPGHOME"

umask 077
od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$WORK/backup.key"
passphrase=$(cat "$WORK/backup.key")
r2_secret=$(od -An -N20 -tx1 /dev/urandom | tr -d ' \n')
printf '%s' "$r2_secret" >"$WORK/r2.secret"
access_key="bb031$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
bucket=spawn-bb
umask 022

# The stand-in for R2 (its header says what it answers).
mkdir -p "$R2"
cat >"$WORK/r2.py" <<'PY'
"""A local stand-in for Cloudflare R2's S3 API, for one scenario.

argv: <directory> <bucket> <access key id> <secret file>
Writes <directory>/port once it listens on 127.0.0.1. Objects live in
<directory>/objects/<hex of the key> with a .json beside each (ETag, size,
last modified, x-amz-meta-* headers). Every request is one line of
<directory>/requests.log: method, key or ?query, status, why.

Every request must carry a valid AWS Signature Version 4 (region auto or
us-east-1, service s3) for the access key: anything else is 403
SignatureDoesNotMatch, as R2 answers. PUT stores (ETag = MD5 of the body);
an existing object under a locked prefix is not overwritten (409
ObjectLockedByBucketPolicy), and DELETE is always 409, as the bucket's lock
rules answer the host's key. GET and HEAD read an object; GET on the bucket
with list-type=2 lists, at most 4 keys a page (S3 may return fewer than
max-keys), with continuation tokens.
"""
import datetime, hashlib, hmac, json, os, sys, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root, bucket, access_key = sys.argv[1], sys.argv[2], sys.argv[3]
secret = open(sys.argv[4]).read().strip()
objects = os.path.join(root, "objects")
os.makedirs(objects, exist_ok=True)
LOCKED = ("postgres/", "bundles/")
PAGE = 4
UNRESERVED = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~"


def enc(text, slash):
    safe = UNRESERVED + ("/" if slash else "")
    return "".join(c if c in safe else "".join("%%%02X" % b for b in c.encode()) for c in text)


def path_of(key):
    return os.path.join(objects, key.encode().hex())


def log(handler, key, status, why=""):
    with open(os.path.join(root, "requests.log"), "a") as out:
        out.write("%s %s %d %s\n" % (handler.command, key, status, why))


def hm(key, msg):
    return hmac.new(key, msg.encode(), hashlib.sha256).digest()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def reply(self, status, body=b"", headers=(), key="-", why=""):
        log(self, key, status, why)
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def error(self, status, code, key, why=""):
        body = ("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>%s</Code><Message>%s</Message></Error>"
                % (code, why or code)).encode()
        self.reply(status, body, [("Content-Type", "application/xml")], key, code + (": " + why if why else ""))

    def body(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip(), 16)
                if size == 0:
                    self.rfile.readline()
                    return data
                data += self.rfile.read(size)
                self.rfile.readline()
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def signed(self, payload):
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("AWS4-HMAC-SHA256 "):
            return "no AWS4-HMAC-SHA256 Authorization"
        fields = {}
        for part in auth[len("AWS4-HMAC-SHA256 "):].split(","):
            name, _, value = part.strip().partition("=")
            fields[name] = value
        credential = fields.get("Credential", "").split("/")
        if len(credential) != 5 or credential[0] != access_key or credential[3] != "s3" \
                or credential[2] not in ("auto", "us-east-1") or credential[4] != "aws4_request":
            return "credential scope %s" % "/".join(credential[1:])
        amzdate = self.headers.get("x-amz-date", "")
        try:
            when = datetime.datetime.strptime(amzdate, "%Y%m%dT%H%M%SZ").replace(tzinfo=datetime.timezone.utc)
        except ValueError:
            return "no x-amz-date"
        if abs((datetime.datetime.now(datetime.timezone.utc) - when).total_seconds()) > 900 \
                or not amzdate.startswith(credential[1]):
            return "x-amz-date out of range"
        signed_headers = fields.get("SignedHeaders", "")
        names = signed_headers.split(";")
        if "host" not in names:
            return "host is not signed"
        canonical_headers = ""
        for name in names:
            value = self.headers.get(name)
            if value is None:
                return "signed header %s missing" % name
            canonical_headers += "%s:%s\n" % (name, " ".join(value.strip().split()))
        hashed = self.headers.get("x-amz-content-sha256")
        if hashed is None:
            hashed = hashlib.sha256(payload).hexdigest()
        elif hashed != "UNSIGNED-PAYLOAD" and hashed != hashlib.sha256(payload).hexdigest():
            return "x-amz-content-sha256 is not the body's"
        raw_path, _, raw_query = self.path.partition("?")
        uri = enc(urllib.parse.unquote(raw_path), True)
        pairs = sorted((enc(k, False), enc(v, False))
                       for k, v in urllib.parse.parse_qsl(raw_query, keep_blank_values=True))
        query = "&".join("%s=%s" % pair for pair in pairs)
        request = "\n".join([self.command, uri, query, canonical_headers, signed_headers, hashed])
        scope = "/".join(credential[1:])
        to_sign = "\n".join(["AWS4-HMAC-SHA256", amzdate, scope,
                             hashlib.sha256(request.encode()).hexdigest()])
        key = hm(("AWS4" + secret).encode(), credential[1])
        for part in credential[2:]:
            key = hm(key, part)
        signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
        if not hmac.compare_digest(signature, fields.get("Signature", "")):
            return "signature"
        return None

    def target(self):
        raw_path, _, raw_query = self.path.partition("?")
        path = urllib.parse.unquote(raw_path)
        query = dict(urllib.parse.parse_qsl(raw_query, keep_blank_values=True))
        prefix = "/" + bucket
        if path == prefix or path == prefix + "/":
            return "", query
        if path.startswith(prefix + "/"):
            return path[len(prefix) + 1:], query
        return None, query

    def handle_any(self):
        payload = self.body() if self.command == "PUT" else b""
        key, query = self.target()
        shown = key if key else "?" + urllib.parse.urlencode(sorted(query.items()))
        why = self.signed(payload)
        if why:
            return self.error(403, "SignatureDoesNotMatch", shown, why)
        if key is None:
            return self.error(404, "NoSuchBucket", shown)
        if key == "":
            if self.command == "GET" and query.get("list-type") == "2":
                return self.listing(query, shown)
            return self.error(400, "InvalidRequest", shown)
        file = path_of(key)
        if self.command == "PUT":
            if os.path.exists(file) and key.startswith(LOCKED):
                return self.error(409, "ObjectLockedByBucketPolicy", key)
            meta = {name.lower(): value for name, value in self.headers.items()
                    if name.lower().startswith("x-amz-meta-")}
            etag = hashlib.md5(payload).hexdigest()
            with open(file + ".part", "wb") as out:
                out.write(payload)
            os.replace(file + ".part", file)
            info = {"key": key, "etag": etag, "size": len(payload), "meta": meta,
                    "modified": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"}
            with open(file + ".json.part", "w") as out:
                json.dump(info, out)
            os.replace(file + ".json.part", file + ".json")
            return self.reply(200, b"", [("ETag", '"%s"' % etag)], key)
        if self.command == "DELETE":
            return self.error(409, "ObjectLockedByBucketPolicy", key, "delete")
        if self.command in ("GET", "HEAD"):
            if not os.path.exists(file + ".json"):
                return self.error(404, "NoSuchKey", key)
            info = json.load(open(file + ".json"))
            data = open(file, "rb").read()
            headers = [("ETag", '"%s"' % info["etag"]), ("Content-Type", "application/octet-stream"),
                       ("Last-Modified", datetime.datetime.strptime(info["modified"], "%Y-%m-%dT%H:%M:%S.%fZ")
                        .strftime("%a, %d %b %Y %H:%M:%S GMT"))]
            headers += sorted(info["meta"].items())
            return self.reply(200, data, headers, key)
        return self.error(405, "MethodNotAllowed", shown)

    def listing(self, query, shown):
        prefix = query.get("prefix", "")
        infos = []
        for name in os.listdir(objects):
            if name.endswith(".json"):
                info = json.load(open(os.path.join(objects, name)))
                if info["key"].startswith(prefix):
                    infos.append(info)
        infos.sort(key=lambda info: info["key"])
        after = query.get("continuation-token") or ""
        after = bytes.fromhex(after).decode() if after else query.get("start-after", "")
        infos = [info for info in infos if info["key"] > after]
        try:
            limit = max(1, min(int(query.get("max-keys", "1000")), 1000, PAGE))
        except ValueError:
            return self.error(400, "InvalidArgument", shown, "max-keys")
        page, more = infos[:limit], len(infos) > limit
        parts = ['<?xml version="1.0" encoding="UTF-8"?>',
                 '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">',
                 "<Name>%s</Name><Prefix>%s</Prefix><KeyCount>%d</KeyCount><MaxKeys>%d</MaxKeys><IsTruncated>%s</IsTruncated>"
                 % (bucket, prefix, len(page), limit, "true" if more else "false")]
        if more:
            parts.append("<NextContinuationToken>%s</NextContinuationToken>" % page[-1]["key"].encode().hex())
        for info in page:
            parts.append("<Contents><Key>%s</Key><LastModified>%s</LastModified><ETag>&quot;%s&quot;</ETag>"
                         "<Size>%d</Size><StorageClass>STANDARD</StorageClass></Contents>"
                         % (info["key"], info["modified"], info["etag"], info["size"]))
        parts.append("</ListBucketResult>")
        self.reply(200, "".join(parts).encode(), [("Content-Type", "application/xml")], shown)

    do_GET = do_PUT = do_HEAD = do_DELETE = do_POST = handle_any


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(os.path.join(root, "port.part"), "w") as out:
    out.write(str(server.server_address[1]))
os.replace(os.path.join(root, "port.part"), os.path.join(root, "port"))
server.serve_forever()
PY
: >"$R2/requests.log"
python3 -I "$WORK/r2.py" "$R2" "$bucket" "$access_key" "$WORK/r2.secret" 2>"$R2/err" &
r2_pid=$!
tries=0
while [ ! -s "$R2/port" ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 100 ] || harness "the R2 stand-in did not start within 10 s: $(cat "$R2/err")"
    kill -0 "$r2_pid" 2>/dev/null || harness "the R2 stand-in exited: $(cat "$R2/err")"
    sleep 0.1
done
r2_endpoint="http://127.0.0.1:$(cat "$R2/port")"
umask 077
cat >"$WORK/r2.env" <<EOF
SPAWN_R2_ENDPOINT=$r2_endpoint
SPAWN_R2_BUCKET=$bucket
SPAWN_R2_ACCESS_KEY_ID=$access_key
SPAWN_R2_SECRET_ACCESS_KEY=$r2_secret
EOF
umask 022

# Two mission sources: Spawn's, with an agent's branch holding its work,
# and another.
src_spawn=$WORK/spawn.git
src_other=$WORK/other.git
git init -q --initial-branch=main "$WORK/seed"
printf 'spawn\n' >"$WORK/seed/README.md"
mkdir -p "$WORK/seed/crates"
printf 'fn main() {}\n' >"$WORK/seed/crates/main.rs"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git init -q --bare --initial-branch=main "$src_spawn"
git -C "$WORK/seed" push -q "$src_spawn" main:main
git -C "$WORK/seed" checkout -q -b work
printf 'the agent work, checkpointed\n' >"$WORK/seed/work.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "spawn checkpoint exec_7"
git -C "$WORK/seed" push -q "$src_spawn" work:refs/heads/spawn/agent_worker_objective_7
git init -q --initial-branch=main "$WORK/seed2"
printf 'other\n' >"$WORK/seed2/README.md"
git -C "$WORK/seed2" add -A
git -C "$WORK/seed2" commit -qm other
git init -q --bare --initial-branch=main "$src_other"
git -C "$WORK/seed2" push -q "$src_other" main:main
# As the live missions/supply-pa.git was seeded (a mirror clone leaves its
# remote-tracking ref): not a branch, not the mission's history.
remote_ref=refs/remotes/github/main
git --git-dir="$src_other" update-ref "$remote_ref" "$(git -C "$WORK/seed2" rev-parse HEAD)"
# A third source with nothing in it yet (a mission added before its first
# push): no branches, no tags. `git bundle create` refuses to make an empty
# bundle, so the record has to say what there is by name, not a git error.
src_empty=$WORK/empty.git
git init -q --bare --initial-branch=main "$src_empty"
# A fourth source that will change in the increment in ways that need no
# bytes: a tag added at a commit the full already holds, and a branch
# removed. Its main and its scratch branch are in the full bundle.
src_tag=$WORK/tag.git
git init -q --bare --initial-branch=main "$src_tag"
git -C "$WORK/seed2" push -q "$src_tag" main:main
git --git-dir="$src_tag" update-ref refs/heads/scratch "$(git -C "$WORK/seed2" rev-parse HEAD)"

# The transcript archive, as the Bridge keeps it beside the workspaces.
SPAWN_WORKSPACE_ROOT=$WORK/ws
archive=$SPAWN_WORKSPACE_ROOT/.transcripts
mkdir -p "$archive/agent_supervisor" "$archive/agent_worker_objective_7"
awk 'BEGIN { for (i = 1; i <= 1500; i++)
    printf "{\"type\":\"assistant\",\"uuid\":\"s-%d\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"step %d: I read the fleet and the missions, and nothing needs a person\"}]}}\n", i, i }' \
    >"$archive/agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0001.jsonl"
awk 'BEGIN { for (i = 1; i <= 400; i++)
    printf "{\"type\":\"user\",\"uuid\":\"w-%d\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_%d\",\"content\":\"ok %d\"}]}}\n", i, i, i * 7 }' \
    >"$archive/agent_worker_objective_7/5f2a9e10-7c44-4b8e-8f00-2a4e9d3c0002.jsonl"

# A third agent's transcript, last written 20 days ago: outside the
# bundles' 14-day window (SPAWN_BACKUP_TRANSCRIPT_DAYS).
old_transcript=agent_worker_objective_3/3e8b1f20-4a6c-4d2e-9b70-6c1a2f8e0004.jsonl
mkdir -p "$archive/agent_worker_objective_3"
awk 'BEGIN { for (i = 1; i <= 50; i++)
    printf "{\"type\":\"user\",\"uuid\":\"o-%d\",\"message\":{\"role\":\"user\",\"content\":\"an old session, line %d\"}}\n", i, i }' \
    >"$archive/$old_transcript"
touch -d '20 days ago' "$archive/$old_transcript"

SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_R2_ENV_FILE=$WORK/r2.env
SPAWN_BACKUP_KEY_FILE=$WORK/backup.key
DRILL_URL="$server_url/postgres"
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT SPAWN_R2_ENV_FILE SPAWN_BACKUP_KEY_FILE

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

q() { scalar "$1"; }
now_ms() { echo $(($(date +%s%N) / 1000000)); }
# The backup records of a type, `<event id>|<record>`, oldest first.
records() {
    sql "select e.id || '|' || (e.kind -> 'detail' -> 'record')::text from spawn_events e
          where e.session_id = 'session_control' and e.event_name = 'control.recorded'
            and e.kind -> 'detail' -> 'record' ->> 'type' = '$1' order by e.id"
}
newest() { records "$1" | tail -n 1; }
count_records() { records "$1" | grep -c . || true; }
field() { printf '%s' "$1" | cut -d'|' -f2- | jq -r "$2"; }
event_of() { printf '%s' "$1" | cut -d'|' -f1; }
# The events of the source that are not backup records: what a drill must
# leave alone.
others() {
    q "select count(*) || ' ' || coalesce(max(id), 0) from spawn_events
        where coalesce(kind -> 'detail' -> 'record' ->> 'type', '') not like 'backup.%'"
}
events_now() { q "select count(*) || ' ' || coalesce(max(id), 0) from spawn_events"; }
add_missions() { # add_missions <from> <to>: control records by a real command
    i=$1
    while [ "$i" -le "$2" ]; do
        if [ $((i % 2)) -eq 0 ]; then src=$src_spawn; else src=$src_other; fi
        capture "$SPAWN_BIN" mission add --source "$src" \
            --text "031 mission $i: back the fleet's record up off this host, every hour, and prove it restores"
        [ "$status" -eq 0 ] || harness "mission add failed: $(err)"
        i=$((i + 1))
    done
}
hexkey() { printf '%s' "$1" | od -An -tx1 | tr -d ' \n'; }
object() { echo "$R2/objects/$(hexkey "$1")"; }
has_object() { [ -f "$(object "$1").json" ]; }
meta() { jq -r --arg h "$2" '.meta[$h] // empty' "$(object "$1").json"; }
held() {
    set -- "$R2"/objects/*.json
    if [ -f "$1" ]; then jq -s 'map(.size) | add' "$@"; else echo 0; fi
}
log_lines() { wc -l <"$R2/requests.log" | tr -d ' '; }
requests_since() { tail -n "+$(($1 + 1))" "$R2/requests.log"; }
decrypt() { # decrypt <file> <out>
    gpg --batch --quiet --no-tty --pinentry-mode loopback --no-symkey-cache \
        --passphrase-file "$SPAWN_BACKUP_KEY_FILE" -o "$2" -d "$1" 2>"$WORK/gpg.err"
}
sha() { sha256sum <"$1" | cut -d' ' -f1; }
size() { wc -c <"$1" | tr -d ' '; }
# The events and the highest event id in a pg_dump archive.
dump_events() {
    pg_restore -a -t spawn_events -f - "$1" | awk '
        /^COPY [a-z_."]*spawn_events / {
            on = 1; col = 0
            line = $0; sub(/^[^(]*\(/, "", line); sub(/\).*$/, "", line)
            n = split(line, names, /, */)
            for (i = 1; i <= n; i++) if (names[i] == "id") col = i
            next
        }
        on && /^\\\.$/ { on = 0; next }
        on { rows++; split($0, f, "\t"); if (f[col] + 0 > max) max = f[col] + 0 }
        END { print rows + 0, max + 0 }'
}
# The run in a subshell whose environment holds the NAME=value settings it
# is given: never on a command line (`env VAR=value`), which every local
# user reads in /proc. The drill's database URL is one of them.
backup_env() { # backup_env <verb> [NAME=value ...]
    bv_verb=$1
    shift
    (
        for bv_setting in "$@"; do export "$bv_setting"; done
        exec "$SPAWN_BIN" backup "$bv_verb"
    )
}
backup() { # backup <verb> [NAME=value ...]: run `spawn backup <verb>`, timed
    verb=$1
    shift
    started=$(now_ms)
    capture backup_env "$verb" "$@"
    took=$(($(now_ms) - started))
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/backup.log"
}
secrets_leaked() { # where a secret shows: the output or the database
    for secret in "$passphrase" "$r2_secret"; do
        if grep -qF -- "$secret" "$WORK/backup.log" 2>/dev/null; then echo "the command's output"; return; fi
        if [ "$(q "select count(*) from spawn_events where kind::text like '%$secret%'")" != 0 ] ||
            [ "$(q "select count(*) from spawn_commands where action::text like '%$secret%'")" != 0 ]; then
            echo "the database"
            return
        fi
    done
}
db_exists() { [ "$(psql -d postgres -tA -c "select count(*) from pg_database where datname = '$1'")" != 0 ]; }

# --- the source database ----------------------------------------------------------

add_missions 1 80
# The id sequence skips, as a rolled-back append makes it skip: the
# watermark is not the row count.
psql -d "$SPAWN_DB_NAME" -tA -c "select setval(pg_get_serial_sequence('spawn_events', 'id'),
    (select last_value from spawn_events_id_seq) + 1000)" >/dev/null 2>&1 ||
    psql -d "$SPAWN_DB_NAME" -tA -c "select setval(pg_get_serial_sequence('spawn_events', 'id'),
        coalesce((select max(id) from spawn_events), 0) + 1000)" >/dev/null ||
    harness "cannot move the events' id sequence"
add_missions 81 120
capture "$SPAWN_BIN" mission add --source "$src_empty" \
    --text "031 mission 146: a mission whose source holds nothing yet"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"
capture "$SPAWN_BIN" mission add --source "$src_tag" \
    --text "031 mission 147: a mission whose source changes without new history"
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"

# --- (1) an hourly encrypted dump ------------------------------------------------------
echo "j12: [031 (1)] an hourly encrypted dump, with the day's daily copy"

before=$(events_now)
n0=${before% *}
max0=${before#* }
lines=$(log_lines)
held0=$(held)
backup dump
if [ "$status" -ne 0 ]; then
    fail "(1) \`spawn backup dump\` exited $status ($(err | tail -n 1)): no encrypted dump reached R2 and no backup.dumped record was written ($n0 events, max id $max0)"
fi
dump1_ms=$took
rec1=$(newest backup.dumped)
[ -n "$rec1" ] || fail "(1) \`spawn backup dump\` exited 0 and recorded no backup.dumped"
key1=$(field "$rec1" '.keys[0] // empty')
printf '%s' "$key1" | grep -Eq '^postgres/hourly/spawn-[0-9]{8}-[0-9]{6}\.dump\.gpg$' ||
    measured_fail "(1) the dump's first key is [$key1], not postgres/hourly/spawn-<YYYYMMDD-HHMMSS>.dump.gpg"
for key in $(field "$rec1" '.keys[]?'); do
    has_object "$key" || measured_fail "(1) the record names $key, which R2 does not hold"
done
field "$rec1" '.keys[]?' | grep -Eq '^postgres/daily/spawn-[0-9]{8}-[0-9]{6}\.dump\.gpg$' ||
    measured_fail "(1) the day's first dump has no copy under postgres/daily/: keys $(field "$rec1" '.keys' | tr -d ' \n')"
has_object "$key1" || fail "(1) R2 does not hold the dump's key [$key1]"
stored=$(object "$key1")
[ "$(head -c 5 "$stored")" != PGDMP ] ||
    measured_fail "(1) the object $key1 is a plain pg_dump archive: the dump reached R2 unencrypted"
decrypt "$stored" "$WORK/dump1" ||
    fail "(1) the object $key1 does not decrypt with the passphrase file: $(head -c 300 "$WORK/gpg.err")"
[ "$(head -c 5 "$WORK/dump1")" = PGDMP ] ||
    measured_fail "(1) $key1 decrypts to no pg_dump custom archive"
sha1=$(sha "$WORK/dump1")
[ "$sha1" = "$(field "$rec1" '.sha256')" ] && [ "$sha1" = "$(meta "$key1" x-amz-meta-sha256)" ] ||
    measured_fail "(1) the dump's sha256 $sha1 is not the record's [$(field "$rec1" '.sha256')] and the object's x-amz-meta-sha256 [$(meta "$key1" x-amz-meta-sha256)]"
[ "$(size "$WORK/dump1")" = "$(field "$rec1" '.bytes')" ] &&
    [ "$(size "$stored")" = "$(field "$rec1" '.stored_bytes')" ] ||
    measured_fail "(1) the record's bytes [$(field "$rec1" '.bytes')] and stored_bytes [$(field "$rec1" '.stored_bytes')] are not the dump's $(size "$WORK/dump1") and the object's $(size "$stored")"
in_dump=$(dump_events "$WORK/dump1")
echo "j12: 031: dump 1: $(size "$WORK/dump1") bytes ($(size "$stored") stored), $in_dump (events, max id), in $dump1_ms ms; the database held $n0 events, max id $max0"
[ "$in_dump" = "$(field "$rec1" '.events') $(field "$rec1" '.max_event_id')" ] ||
    measured_fail "(1) the archive holds $in_dump (events, max id), the record says events [$(field "$rec1" '.events')] max_event_id [$(field "$rec1" '.max_event_id')]"
[ "${in_dump% *}" -ge "$n0" ] && [ "${in_dump#* }" -ge "$max0" ] ||
    measured_fail "(1) the archive holds $in_dump (events, max id), less than the $n0 events (max id $max0) the database held before the run"
[ "$(meta "$key1" x-amz-meta-spawn-events)" = "$(field "$rec1" '.events')" ] ||
    measured_fail "(1) the object's x-amz-meta-spawn-events [$(meta "$key1" x-amz-meta-spawn-events)] is not the record's events"
[ "$(field "$rec1" '.held_bytes')" = "$held0" ] ||
    measured_fail "(1) the record's held_bytes [$(field "$rec1" '.held_bytes')] is not the $held0 bytes the bucket held before the run"
bad=$(requests_since "$lines" | awk '$3 == 403 || $1 == "DELETE"' | head -n 3)
[ -z "$bad" ] || measured_fail "(1) requests R2 refused or a delete: $bad"
leak=$(secrets_leaked)
[ -z "$leak" ] || measured_fail "(1) a secret (the passphrase or the R2 secret key) shows in $leak"

# --- (2) every hour ------------------------------------------------------------------
echo "j12: [031 (2)] a second dump the same UTC day stores one hourly object"

add_missions 121 135
before=$(events_now)
n1=${before% *}
day1=$(date -u +%Y%m%d)
lines=$(log_lines)
held1=$(held)
backup dump
[ "$status" -eq 0 ] || fail "(2) the second \`spawn backup dump\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
dump2_ms=$took
rec2=$(newest backup.dumped)
[ "$(event_of "$rec2")" != "$(event_of "$rec1")" ] || fail "(2) the second dump recorded nothing"
key2=$(field "$rec2" '.keys[0] // empty')
keys2=$(field "$rec2" '.keys | length')
puts2=$(requests_since "$lines" | awk '$1 == "PUT" && $3 == 200' | wc -l | tr -d ' ')
printf '%s' "$key2" | grep -Eq '^postgres/hourly/' && [ "$key2" != "$key1" ] && has_object "$key2" ||
    measured_fail "(2) the second dump's key [$key2] is no new object under postgres/hourly/ (the first: $key1)"
if [ "$(date -u +%Y%m%d)" = "$day1" ] && [ "$day1" = "$(printf '%s' "$key1" | sed -E 's#.*spawn-([0-9]{8})-.*#\1#')" ]; then
    [ "$keys2" = 1 ] && [ "$puts2" = 1 ] ||
        measured_fail "(2) the second dump of the UTC day stored $puts2 objects, keys $(field "$rec2" '.keys' | tr -d ' \n'): one, under postgres/hourly/, the daily copy being the day's first"
fi
[ "$(field "$rec2" '.events')" -ge "$n1" ] && [ "$(field "$rec2" '.events')" -gt "$(field "$rec1" '.events')" ] ||
    measured_fail "(2) the second dump holds [$(field "$rec2" '.events')] events: the database held $n1, the first dump $(field "$rec1" '.events')"
[ "$(field "$rec2" '.held_bytes')" = "$held1" ] ||
    measured_fail "(2) the second record's held_bytes [$(field "$rec2" '.held_bytes')] is not the $held1 bytes the bucket held before it"
echo "j12: 031: dump 2: $(field "$rec2" '.events') events, $(field "$rec2" '.stored_bytes') bytes stored, in $dump2_ms ms"

# --- (3) the daily bundle: full, then incremental ------------------------------------
echo "j12: [031 (3)] the full bundle, the drill after it, then an increment"

# unpack_bundle <key> <dir>: the object decrypted (its sha256 checked against
# the record $2's) and untarred into <dir>; 1 when it does not decrypt.
unpack_bundle() {
    stored=$(object "$1")
    rm -f "$WORK/bundle.raw" "$WORK/bundle.tar"
    decrypt "$stored" "$WORK/bundle.raw" || return 1
    magic=$(head -c 4 "$WORK/bundle.raw" | od -An -tx1 | tr -d ' \n')
    case "$magic" in
    1f8b*) gzip -dc <"$WORK/bundle.raw" >"$WORK/bundle.tar" ;;
    28b52ffd)
        command -v zstd >/dev/null 2>&1 || harness "the bundle is zstd-compressed and zstd is not installed"
        zstd -q -dc <"$WORK/bundle.raw" >"$WORK/bundle.tar"
        ;;
    *) cp "$WORK/bundle.raw" "$WORK/bundle.tar" ;;
    esac
    mkdir -p "$2"
    tar -xf "$WORK/bundle.tar" -C "$2" 2>"$WORK/tar.err" ||
        fail "(3) the bundle $1 decrypts to no tar archive (magic $magic): $(head -c 200 "$WORK/tar.err")"
}
heads_of() { git --git-dir="$1" for-each-ref --format='%(objectname) %(refname)' refs/heads refs/tags | sort; }
bundle_heads() { git bundle list-heads "$1" 2>/dev/null | awk '$2 != "HEAD"' | sort; }
# The files of the archive modified within the window (14 days), sorted.
window_files() { (cd "$archive" && find . -type f -mtime -14 | sed 's#^\./##' | sort); }

lines=$(log_lines)
backup bundle
[ "$status" -eq 0 ] || fail "(3) \`spawn backup bundle\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
bundle_ms=$took
bun=$(newest backup.bundled)
[ -n "$bun" ] || fail "(3) \`spawn backup bundle\` exited 0 and recorded no backup.bundled"
bkey=$(field "$bun" '.keys[0] // empty')
week1=$(date -u +%G%V)
printf '%s' "$bkey" | grep -Eq '^bundles/full/' && has_object "$bkey" ||
    fail "(3) the first bundle's key [$bkey] is no object under bundles/full/ (the first bundle, with none before it, is a full one)"
[ "$(field "$bun" '.kind // empty')" = full ] ||
    measured_fail "(3) the first bundle is recorded kind [$(field "$bun" '.kind // empty')], not full"
unpack_bundle "$bkey" "$WORK/unpacked" ||
    fail "(3) the bundle $bkey does not decrypt with the passphrase file: $(head -c 300 "$WORK/gpg.err")"
[ "$(sha "$WORK/bundle.raw")" = "$(field "$bun" '.sha256')" ] ||
    measured_fail "(3) the bundle's sha256 is not the record's"
[ "$(jq -r '.kind // empty' "$WORK/unpacked/manifest.json" 2>/dev/null)" = full ] ||
    measured_fail "(3) the full bundle holds no manifest.json of kind full: $(head -c 200 "$WORK/unpacked/manifest.json" 2>/dev/null)"
git init -q --bare "$WORK/check.git"
for src in "$src_spawn" "$src_other" "$src_tag"; do
    want=$(heads_of "$src")
    found=""
    for b in "$WORK"/unpacked/repos/*.bundle; do
        [ -f "$b" ] || continue
        if [ "$(bundle_heads "$b")" = "$want" ]; then found=$b; break; fi
    done
    [ -n "$found" ] ||
        measured_fail "(3) no repos/*.bundle in the bundle lists exactly the branches of $(basename "$src") ($(printf '%s\n' "$want" | wc -l | tr -d ' ') refs; bundles: $(cd "$WORK/unpacked" && find . -name '*.bundle' | tr '\n' ' '))"
    [ -n "$found" ] || continue
    git --git-dir="$WORK/check.git" bundle verify -q "$found" >/dev/null 2>"$WORK/verify.err" ||
        measured_fail "(3) the bundle of $(basename "$src") does not verify alone: $(head -c 200 "$WORK/verify.err")"
    if [ "$src" = "$src_spawn" ]; then
        git clone -q -b spawn/agent_worker_objective_7 "$found" "$WORK/clone" 2>"$WORK/clone.err" &&
            [ "$(cat "$WORK/clone/work.txt" 2>/dev/null)" = "the agent work, checkpointed" ] ||
            measured_fail "(3) the agent's branch spawn/agent_worker_objective_7 does not come back from the bundle with its work.txt"
        full_spawn=$found
    fi
done
files=0
bytes=0
for path in $(window_files); do
    files=$((files + 1))
    bytes=$((bytes + $(size "$archive/$path")))
    cmp -s "$archive/$path" "$WORK/unpacked/transcripts/$path" ||
        measured_fail "(3) the bundle has no transcripts/$path with the archive's bytes"
done
[ ! -e "$WORK/unpacked/transcripts/$old_transcript" ] ||
    measured_fail "(3) the full bundle holds $old_transcript, last written 20 days ago: outside the 14-day window"
[ "$(field "$bun" '.transcripts.files')" = "$files" ] && [ "$(field "$bun" '.transcripts.bytes')" = "$bytes" ] ||
    measured_fail "(3) the record's transcripts [$(field "$bun" '.transcripts | {files, bytes}' | tr -d ' \n')] are not the window's $files files, $bytes bytes"
named=$(field "$bun" '[.repos[]?.source] | sort | join(" ")')
[ "$named" = "$(printf '%s\n%s\n%s\n%s\n' "$src_empty" "$src_other" "$src_spawn" "$src_tag" | sort | tr '\n' ' ' | sed 's/ $//')" ] ||
    measured_fail "(3) the record names the sources [$named], not the four missions' $src_spawn, $src_other, $src_empty and $src_tag"
# The empty source: named `empty` with no refs and no file in the archive
# (there is nothing of it to hold, and nothing to leave to a git error).
empty_entry=$(field "$bun" '.repos[] | select(.source == "'"$src_empty"'")' 2>/dev/null)
[ -n "$empty_entry" ] && [ "$(printf '%s' "$empty_entry" | jq -r '.empty // false')" = true ] &&
    [ "$(printf '%s' "$empty_entry" | jq -r '.refs')" = 0 ] &&
    [ "$(printf '%s' "$empty_entry" | jq -r '.bytes')" = 0 ] &&
    [ "$(printf '%s' "$empty_entry" | jq -c '.heads')" = '{}' ] ||
    measured_fail "(3) the record does not name the empty source $src_empty as empty, 0 refs, no bytes, {}: $(printf '%s' "$empty_entry" | head -c 300)"
[ "$(jq -r --arg s "$src_empty" '[.repos[]? | select(.source == $s) | (.empty == true and .file == null)][0] // false' "$WORK/unpacked/manifest.json" 2>/dev/null)" = true ] ||
    measured_fail "(3) the manifest does not name the empty source $src_empty as empty with no file: $(jq -c '.repos' "$WORK/unpacked/manifest.json" 2>/dev/null | head -c 400)"
echo "j12: 031: full bundle: $(field "$bun" '.stored_bytes') bytes stored for $bytes bytes of transcripts and three sources (one empty), in $bundle_ms ms"

# The remote-tracking ref other.git carries is not the mission's history: the
# bundle holds no ref the manifest does not name, and the record names it not.
[ -n "$(git --git-dir="$src_other" for-each-ref --format='%(refname)' "$remote_ref")" ] ||
    harness "the fixture lost $src_other's $remote_ref"
field "$bun" '.repos[].heads | keys[]' | grep -qxF -- "$remote_ref" &&
    measured_fail "(3) the record names $remote_ref, which no source's bundle holds"
for b in "$WORK"/unpacked/repos/*.bundle; do
    [ -f "$b" ] || continue
    git bundle list-heads "$b" 2>/dev/null | awk '{ print $2 }' | grep -qxF -- "$remote_ref" &&
        measured_fail "(3) the bundle $(basename "$b") holds $remote_ref, which its manifest does not name"
done

# The drill opens the newest bundle too: now it is the full one.
backup verify SPAWN_BACKUP_DRILL_DATABASE_URL="$DRILL_URL"
[ "$status" -eq 0 ] ||
    fail "(3) \`spawn backup verify\` after the full bundle exited $status: $(err | tail -n 1)"
ver_full=$(newest backup.verified)
[ "$(field "$ver_full" '.ok')" = true ] && [ "$(field "$ver_full" '.bundle.checked')" = true ] &&
    [ "$(field "$ver_full" '.bundle.kind')" = full ] && [ "$(field "$ver_full" '.bundle.key')" = "$(field "$bun" '.keys[0]')" ] ||
    measured_fail "(3) the drill after the full bundle recorded $(field "$ver_full" '.bundle // empty' | head -c 300), not the full bundle $bkey opened as kind full"

# The archive grows (one file appended to, one new), the agent's branch
# moves, other.git does not: the next bundle the same day is an increment.
sup=agent_supervisor/0b7e3c52-5d1e-4c1a-9a36-1f0c6f2d0001.jsonl
wrk=agent_worker_objective_7/5f2a9e10-7c44-4b8e-8f00-2a4e9d3c0002.jsonl
new=agent_worker_objective_7/9c1d4e77-2b6a-4f0e-8d11-7e5a3c9b0003.jsonl
sup_before=$(size "$archive/$sup")
awk 'BEGIN { for (i = 1501; i <= 1700; i++)
    printf "{\"type\":\"assistant\",\"uuid\":\"s-%d\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"step %d: the next day, still nothing needs a person\"}]}}\n", i, i }' \
    >>"$archive/$sup"
awk 'BEGIN { for (i = 1; i <= 120; i++)
    printf "{\"type\":\"user\",\"uuid\":\"n-%d\",\"message\":{\"role\":\"user\",\"content\":\"a new session, line %d\"}}\n", i, i }' \
    >"$archive/$new"
printf 'more agent work\n' >>"$WORK/seed/work.txt"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm "spawn checkpoint exec_8"
git -C "$WORK/seed" push -q "$src_spawn" work:refs/heads/spawn/agent_worker_objective_7
# tag.git moves without new history: a tag at a commit the full bundle
# already holds, and a branch removed. `git bundle create` would refuse an
# empty increment, so the run has to name it.
main_tag=$(git --git-dir="$src_tag" rev-parse refs/heads/main)
git --git-dir="$src_tag" update-ref refs/tags/v1 "$main_tag"
git --git-dir="$src_tag" update-ref -d refs/heads/scratch
lines=$(log_lines)
backup bundle
[ "$status" -eq 0 ] || fail "(3) the second \`spawn backup bundle\` exited $status: $(err | tail -n 2 | tr '\n' ' ')"
incr_ms=$took
inc=$(newest backup.bundled)
[ "$(event_of "$inc")" != "$(event_of "$bun")" ] || fail "(3) the second \`spawn backup bundle\` recorded nothing"
ikey=$(field "$inc" '.keys[0] // empty')
puts=$(requests_since "$lines" | awk '$1 == "PUT" && $3 == 200 && $2 ~ /^bundles\//' | wc -l | tr -d ' ')
if [ "$(date -u +%G%V)" != "$week1" ]; then
    echo "j12: 031: the ISO week turned between the two bundles: the second is a week's first, so a full one; the increment is not judged in this run"
else
    printf '%s' "$ikey" | grep -Eq '^bundles/incr/' && has_object "$ikey" && [ "$puts" = 1 ] ||
        measured_fail "(3) the second bundle of the day stored $puts objects, keys $(field "$inc" '.keys' | tr -d ' \n'): one, under bundles/incr/ (an increment of the full one)"
    [ "$(field "$inc" '.kind // empty')" = incremental ] && [ "$(field "$inc" '.base_event_id // empty')" = "$(event_of "$bun")" ] ||
        measured_fail "(3) the second bundle is recorded kind [$(field "$inc" '.kind // empty')] base_event_id [$(field "$inc" '.base_event_id // empty')], not incremental of the full one (event $(event_of "$bun"))"
    if [ -n "$ikey" ] && has_object "$ikey"; then
        unpack_bundle "$ikey" "$WORK/unpacked2" ||
            fail "(3) the increment $ikey does not decrypt with the passphrase file: $(head -c 300 "$WORK/gpg.err")"
        manifest=$WORK/unpacked2/manifest.json
        [ "$(jq -r '.kind // empty' "$manifest" 2>/dev/null)" = incremental ] ||
            measured_fail "(3) the increment holds no manifest.json of kind incremental: $(head -c 200 "$manifest" 2>/dev/null)"
        # The grown file: its appended bytes alone, at the full one's size.
        tail -c "+$((sup_before + 1))" "$archive/$sup" >"$WORK/appended"
        [ "$(jq -r --arg p "$sup" '.transcripts[]? | select(.path == $p) | .offset' "$manifest" 2>/dev/null)" = "$sup_before" ] &&
            cmp -s "$WORK/appended" "$WORK/unpacked2/transcripts/$sup" ||
            measured_fail "(3) the increment does not hold exactly the $(size "$WORK/appended") bytes appended to $sup at offset $sup_before (manifest: $(jq -c --arg p "$sup" '[.transcripts[]? | select(.path == $p)]' "$manifest" 2>/dev/null); held: $(size "$WORK/unpacked2/transcripts/$sup" 2>/dev/null || echo none) bytes)"
        [ "$(jq -r --arg p "$new" '.transcripts[]? | select(.path == $p) | .offset' "$manifest" 2>/dev/null)" = 0 ] &&
            cmp -s "$archive/$new" "$WORK/unpacked2/transcripts/$new" ||
            measured_fail "(3) the increment does not hold the new transcript $new whole at offset 0"
        [ ! -e "$WORK/unpacked2/transcripts/$wrk" ] ||
            measured_fail "(3) the increment holds $wrk, which did not change since the full bundle"
        # The full bundle and the increment give the window back.
        for path in $(window_files); do
            if [ -f "$WORK/unpacked2/transcripts/$path" ]; then
                off=$(jq -r --arg p "$path" '[.transcripts[]? | select(.path == $p) | .offset][0] // 0' "$manifest")
                { if [ "$off" -gt 0 ]; then head -c "$off" "$WORK/unpacked/transcripts/$path"; fi
                  cat "$WORK/unpacked2/transcripts/$path"; } >"$WORK/restored"
            else
                cp "$WORK/unpacked/transcripts/$path" "$WORK/restored" 2>/dev/null || : >"$WORK/restored"
            fi
            cmp -s "$WORK/restored" "$archive/$path" ||
                measured_fail "(3) the full bundle and the increment do not give $path back byte for byte"
        done
        # spawn.git: the bundle its manifest names (an increment lists only
        # the refs that moved), with prerequisites the full one holds, which
        # fetched after the full one gives exactly the source's branches now;
        # other.git unchanged.
        want=$(heads_of "$src_spawn")
        found=""
        f=$(jq -r --arg s "$src_spawn" '[.repos[]? | select(.source == $s) | .file][0] // empty' "$manifest" 2>/dev/null)
        [ -n "$f" ] && [ -f "$WORK/unpacked2/$f" ] && found=$WORK/unpacked2/$f
        if [ -z "$found" ]; then
            measured_fail "(3) the increment's manifest names no bundle of spawn.git, whose agent branch moved: $(jq -c '.repos' "$manifest" 2>/dev/null) (bundles: $(cd "$WORK/unpacked2" && find . -name '*.bundle' | tr '\n' ' '))"
        else
            # A v2 bundle's header: its prerequisites are the lines that
            # start with `-`, before the first blank line.
            prerequisites=$(awk 'NR == 1 { next } /^$/ { exit } /^-/ { n++ } END { print n + 0 }' "$found" 2>/dev/null || echo 0)
            [ "$prerequisites" -ge 1 ] ||
                measured_fail "(3) spawn.git's bundle in the increment names no prerequisites: a whole copy, not an increment"
            if [ -n "${full_spawn-}" ]; then
                git init -q --bare "$WORK/restore.git"
                git --git-dir="$WORK/restore.git" fetch -q "$full_spawn" 'refs/*:refs/*' 2>"$WORK/fetch.err" &&
                    git --git-dir="$WORK/restore.git" fetch -q "$found" 'refs/*:refs/*' 2>>"$WORK/fetch.err" &&
                    [ "$(heads_of "$WORK/restore.git")" = "$want" ] ||
                    measured_fail "(3) the full bundle of spawn.git and the increment's, fetched in turn, do not give the source's branches: $(head -c 200 "$WORK/fetch.err")"
            fi
        fi
        [ "$(jq -r --arg s "$src_other" '[.repos[]? | select(.source == $s) | .unchanged][0] // empty' "$manifest" 2>/dev/null)" = true ] &&
            [ "$(printf '%s' "$inc" | cut -d'|' -f2- | jq -r --arg s "$src_other" '[.repos[]? | select(.source == $s) | .unchanged][0] // empty' 2>/dev/null)" = true ] ||
            measured_fail "(3) the increment and its record do not mark other.git unchanged (no ref of it moved): $(jq -c '.repos' "$manifest" 2>/dev/null)"
        # spawn.git's bundle holds one ref, and the record says so: `bundled`
        # is what the bundle lists, not what a reader infers from the base.
        bundled=$(printf '%s' "$inc" | cut -d'|' -f2- | jq -c --arg s "$src_spawn" '[.repos[]? | select(.source == $s)][0].bundled // empty')
        moved_commit=$(git --git-dir="$src_spawn" rev-parse refs/heads/spawn/agent_worker_objective_7)
        [ "$(printf '%s' "$bundled" | jq -c 'length')" = 1 ] &&
            [ "$(printf '%s' "$bundled" | jq -r --arg c "$moved_commit" '.[0] | select(.ref == "refs/heads/spawn/agent_worker_objective_7" and .commit == $c) | .ref')" = refs/heads/spawn/agent_worker_objective_7 ] ||
            measured_fail "(3) the increment's record does not name its bundle's one ref (refs/heads/spawn/agent_worker_objective_7 at $moved_commit): $bundled"
        # tag.git changed without new history: the record and the manifest name
        # the delta (the tag added, the branch removed), bundled [], and no
        # file, rather than leaving git's empty-bundle refusal to land in the run.
        tag_record=$(printf '%s' "$inc" | cut -d'|' -f2- | jq -c --arg s "$src_tag" '[.repos[]? | select(.source == $s)][0] // empty')
        printf '%s' "$tag_record" | jq -e '.file == null and .bytes == 0 and .delta.added == ["refs/tags/v1"] and .delta.removed == ["refs/heads/scratch"] and .bundled == []' >/dev/null 2>&1 ||
            measured_fail "(3) the increment's record does not name tag.git's change as a delta with no file (the tag v1 added at a held commit, the branch scratch removed): $tag_record"
        jq -e --arg s "$src_tag" '[.repos[]? | select(.source == $s)][0] | .file == null and .delta.added == ["refs/tags/v1"] and .delta.removed == ["refs/heads/scratch"] and .bundled == []' "$manifest" >/dev/null 2>&1 ||
            measured_fail "(3) the increment's manifest does not name tag.git's delta with no file: $(jq -c '.repos' "$manifest" 2>/dev/null)"
        [ ! -e "$WORK/unpacked2/repos/tag.git.bundle" ] ||
            measured_fail "(3) the increment holds a bundle of tag.git, whose change needs no bytes"
    fi
    echo "j12: 031: increment: $(field "$inc" '.stored_bytes') bytes stored for $(field "$inc" '.transcripts.bytes') bytes of transcripts, in $incr_ms ms"
fi

# --- (4) the restore drill -------------------------------------------------------------
echo "j12: [031 (4)] the restore drill restores the newest dump to its record's counts"

add_missions 136 145
latest=$(newest backup.dumped)
others_before=$(others)
now_events=$(events_now)
verified_before=$(count_records backup.verified)
backup verify SPAWN_BACKUP_DRILL_DATABASE_URL="$DRILL_URL"
[ "$status" -eq 0 ] ||
    fail "(4) \`spawn backup verify\` exited $status on an intact dump ($(err | tail -n 1)); its record: $(newest backup.verified | cut -d'|' -f2- | head -c 300)"
drill_ms=$took
ver=$(newest backup.verified)
[ "$(count_records backup.verified)" -gt "$verified_before" ] ||
    fail "(4) \`spawn backup verify\` exited 0 and recorded no backup.verified"
[ "$(field "$ver" '.ok')" = true ] ||
    measured_fail "(4) the drill recorded ok [$(field "$ver" '.ok')]: $(field "$ver" '.reason // empty')"
[ "$(field "$ver" '.dumped_event_id')" = "$(event_of "$latest")" ] &&
    field "$latest" '.keys[]' | grep -qxF -- "$(field "$ver" '.key')" ||
    measured_fail "(4) the drill names key [$(field "$ver" '.key')] and dumped_event_id [$(field "$ver" '.dumped_event_id')], not the newest dump's record (event $(event_of "$latest"), $(field "$latest" '.keys[0]'))"
[ "$(field "$ver" '.events') $(field "$ver" '.max_event_id')" = "$(field "$latest" '.events') $(field "$latest" '.max_event_id')" ] &&
    [ "$(field "$ver" '.expected.events') $(field "$ver" '.expected.max_event_id')" = "$(field "$latest" '.events') $(field "$latest" '.max_event_id')" ] ||
    measured_fail "(4) the drill restored events [$(field "$ver" '.events')] max id [$(field "$ver" '.max_event_id')], expected [$(field "$ver" '.expected' | tr -d ' \n')]; the dump's record says $(field "$latest" '.events') and $(field "$latest" '.max_event_id') (the database now: $now_events)"
want_kind=incremental
if [ "$(date -u +%G%V)" != "$week1" ]; then want_kind=full; fi
[ "$(field "$ver" '.bundle.checked')" = true ] && [ "$(field "$ver" '.bundle.kind')" = "$want_kind" ] ||
    measured_fail "(4) the drill recorded bundle [$(field "$ver" '.bundle | {checked, kind, key}' | tr -d ' \n')], not the newest bundle ($want_kind)"
scratch=$(field "$ver" '.database // empty')
case "$scratch" in
"" | "$SPAWN_DB_NAME" | postgres | template* | *[!A-Za-z0-9_]*)
    measured_fail "(4) the drill names [$scratch] as the database it restored into: a database of its own, not the source $SPAWN_DB_NAME"
    ;;
*)
    ! db_exists "$scratch" ||
        measured_fail "(4) the drill's scratch database $scratch is still there after it"
    ;;
esac
[ "$(others)" = "$others_before" ] ||
    measured_fail "(4) the source's other records changed during the drill: $others_before (count, max id) -> $(others)"
if [ -n "$db_password" ] && records backup.verified | grep -qF -- "$db_password"; then
    measured_fail "(4) the drill's record holds the test database's password"
fi
echo "j12: 031: drill: restored $(field "$ver" '.events') events (max id $(field "$ver" '.max_event_id')) into $scratch in $drill_ms ms; the database now holds $now_events"

# --- (5) refusals and a corrupted dump -----------------------------------------------------
echo "j12: [031 (5)] the drill refuses without a drill server and fails a corrupted dump"

others_before=$(others)
verified_before=$(count_records backup.verified)
backup verify
[ "$status" -eq 2 ] ||
    measured_fail "(5) without SPAWN_BACKUP_DRILL_DATABASE_URL the drill exited $status, not 2 (it could not run)"
[ "$(count_records backup.verified)" -gt "$verified_before" ] &&
    [ "$(field "$(newest backup.verified)" '.ok')" = false ] &&
    field "$(newest backup.verified)" '.reason // ""' | grep -q SPAWN_BACKUP_DRILL_DATABASE_URL ||
    measured_fail "(5) without a drill server no backup.verified ok=false names SPAWN_BACKUP_DRILL_DATABASE_URL: $(newest backup.verified | cut -d'|' -f2- | head -c 300)"
[ "$(others)" = "$others_before" ] ||
    measured_fail "(5) the drill without a drill server changed the source's records: $others_before -> $(others)"

latest=$(newest backup.dumped)
for key in $(field "$latest" '.keys[]'); do
    file=$(object "$key")
    python3 -I -c 'import sys
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
data[len(data) // 2] ^= 0xFF
open(path, "wb").write(data)' "$file" || harness "cannot corrupt $key"
done
verified_before=$(count_records backup.verified)
backup verify SPAWN_BACKUP_DRILL_DATABASE_URL="$DRILL_URL"
corrupt_ms=$took
bad=$(newest backup.verified)
[ "$status" -eq 1 ] ||
    measured_fail "(5) the drill of a corrupted dump ($(field "$latest" '.keys[0]'), one byte flipped) exited $status, not 1"
[ "$(count_records backup.verified)" -gt "$verified_before" ] && [ "$(field "$bad" '.ok')" = false ] &&
    [ -n "$(field "$bad" '.reason // empty')" ] ||
    measured_fail "(5) the drill of a corrupted dump recorded no backup.verified ok=false with a reason: $(printf '%s' "$bad" | cut -d'|' -f2- | head -c 300)"
left=$(field "$bad" '.database // empty')
if [ -n "$left" ] && [ "$left" != "$SPAWN_DB_NAME" ] && db_exists "$left"; then
    measured_fail "(5) the failed drill left its scratch database $left behind"
fi
echo "j12: 031: corrupted dump: the drill failed in $corrupt_ms ms: $(field "$bad" '.reason // empty' | head -c 200)"

# --- (6) the alert and the limit -------------------------------------------------------
echo "j12: [031 (6)] the alert and the limit"

junk=junk/kept-elsewhere.bin
head -c 1500000 /dev/urandom >"$(object "$junk")"
jq -n --arg key "$junk" --argjson size 1500000 \
    '{key: $key, etag: "0", size: $size, meta: {}, modified: "2026-01-01T00:00:00.000Z"}' >"$(object "$junk").json"
held6=$(held)
lines=$(log_lines)
backup dump SPAWN_BACKUP_R2_ALERT_MB=1
[ "$status" -eq 0 ] || fail "(6) a dump over the alert (and under the limit) exited $status: $(err | tail -n 2 | tr '\n' ' ')"
rec6=$(newest backup.dumped)
key6=$(field "$rec6" '.keys[0] // empty')
printf '%s' "$key6" | grep -Eq '^postgres/hourly/' && has_object "$key6" && [ "$key6" != "$key2" ] &&
    [ "$(field "$rec6" '.over_alert')" = true ] && [ "$(field "$rec6" '.alert_bytes')" = 1000000 ] &&
    [ "$(field "$rec6" '.limit_bytes')" = 10000000000 ] && [ "$(field "$rec6" '.held_bytes')" = "$held6" ] ||
    measured_fail "(6) with $held6 bytes held and an alert at 1 MB the dump must store its hourly copy and record over_alert true, alert_bytes 1000000, limit_bytes 10000000000 and held_bytes $held6: key [$key6], $(printf '%s' "$rec6" | cut -d'|' -f2- | jq -c '{over_alert, alert_bytes, limit_bytes, held_bytes, written_bytes}' 2>/dev/null)"
held7=$(held)
lines=$(log_lines)
backup bundle SPAWN_BACKUP_R2_ALERT_MB=1 SPAWN_BACKUP_R2_LIMIT_MB=1
bun2=$(newest backup.bundled)
puts=$(requests_since "$lines" | awk '$1 == "PUT" && $2 ~ /^bundles\//' | head -n 3)
[ -z "$puts" ] ||
    measured_fail "(6) with $held7 bytes held and a limit of 1 MB the bundle was still stored: $puts"
[ "$(event_of "$bun2")" != "$(event_of "$inc")" ] && [ "$(field "$bun2" '.keys | length')" = 0 ] &&
    [ "$(field "$bun2" '.skipped | length')" -ge 1 ] && [ "$(field "$bun2" '.limit_bytes')" = 1000000 ] &&
    [ "$(field "$bun2" '.held_bytes')" = "$held7" ] &&
    field "$bun2" '[.skipped[]?.reason] | join(" ")' | grep -qi limit ||
    measured_fail "(6) the bundle over the limit recorded no skipped tier with the limit (limit_bytes 1000000, held_bytes $held7): $(printf '%s' "$bun2" | cut -d'|' -f2- | head -c 300)"
dumped_before=$(count_records backup.dumped)
lines=$(log_lines)
backup dump SPAWN_BACKUP_R2_ALERT_MB=1 SPAWN_BACKUP_R2_LIMIT_MB=1
[ "$status" -eq 4 ] ||
    measured_fail "(6) a dump whose hourly copy the limit kept out exited $status, not 4: $(err | tail -n 2 | tr '\n' ' ')"
rec7=$(newest backup.dumped)
puts=$(requests_since "$lines" | awk '$1 == "PUT"' | head -n 3)
[ -z "$puts" ] ||
    measured_fail "(6) with $held7 bytes held and a limit of 1 MB the dump still wrote to R2: $puts"
[ "$(count_records backup.dumped)" -gt "$dumped_before" ] && [ "$(field "$rec7" '.keys | length')" = 0 ] &&
    [ "$(field "$rec7" '[.skipped[]? | select(.tier == "hourly")] | length')" -ge 1 ] &&
    [ "$(field "$rec7" '.limit_bytes')" = 1000000 ] &&
    field "$rec7" '[.skipped[]?.reason] | join(" ")' | grep -qi limit ||
    measured_fail "(6) the dump the limit kept out recorded no hourly tier skipped for the limit (keys [], limit_bytes 1000000): $(printf '%s' "$rec7" | cut -d'|' -f2- | head -c 300)"

echo "j12: 031: hourly and daily dumps encrypted in R2 with their records, the sources and transcripts bundled whole and then as an increment, the drill restored the newest dump to its record's counts and failed a corrupted one, the alert was recorded and nothing was written past the limit"
exit 0
}

# =================================================================================
# The run: 031 beside 030
# =================================================================================

started=$(date +%s)
pid_031=""
pid_cur=""
stop_sections() {
    for pid in $pid_cur $pid_031; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in $pid_cur $pid_031; do
        wait "$pid" 2>/dev/null || true
    done
}
trap 'trap - EXIT; set +e; stop_sections' EXIT
trap 'exit 130' INT TERM

echo "j12: [031] the backups, beside 030 (no loop)"
(section_031) &
pid_031=$!

echo "j12: [030] the alerts, its own database and loop"
rc_030=0
(section_030) &
pid_cur=$!
wait "$pid_cur" || rc_030=$?
pid_cur=""
echo "j12: 030 ended with exit $rc_030 after $(($(date +%s) - started)) s"

rc_031=0
wait "$pid_031" || rc_031=$?
pid_031=""
echo "j12: 031 ended with exit $rc_031 after $(($(date +%s) - started)) s"
trap - EXIT

if [ "$rc_030" -eq 1 ] || [ "$rc_031" -eq 1 ]; then
    echo "j12: an assertion failed (030 exit $rc_030, 031 exit $rc_031)" >&2
    exit 1
fi
if [ "$rc_030" -ne 0 ] || [ "$rc_031" -ne 0 ]; then
    echo "j12: harness: 030 exit $rc_030, 031 exit $rc_031" >&2
    exit 2
fi
echo "j12: the hold-told journey passed in $(($(date +%s) - started)) s"
exit 0
