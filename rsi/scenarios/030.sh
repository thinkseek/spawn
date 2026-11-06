#!/bin/sh
# Black box: Spawn raises its own alerts. Today the supervisor runs a
# hand-written monitor (/var/tmp/spawn/fleet-monitor.sh, a bash loop every
# 30 s) that prints, in one pane, task and mission state changes (halts and
# the rejection and abort counters included), tasks assigned over 5 minutes
# whose agent is free, failed commands, Postgres crash lines (`terminated by
# signal`, `No space left`, `could not resize`), MemAvailable under 4 GB and
# /tmp (RAM) over 8 GB. Nothing of it is recorded: its state (`prev`, `seen`,
# `memlow`) lives in its process, a restart re-reports or forgets, and no
# view of Spawn shows it. Required: `spawn alerts` (`--once`, `--follow`) and
# the dashboard's `GET /api/alerts` show the same alerts, computed by the
# loop from records and host facts and recorded (`alert.fired`,
# `alert.cleared`): each fires once per occurrence and a condition clears
# when it clears, with its evidence. Added to the monitor's: an agent stuck
# by its latest judgement (objective 011), an admission wait over its bound
# (029's records), a backup older than its schedule and a failed restore
# drill (031's records), and a model login that expired (028's record).
# Two alerts the owner ruled (2026-11-06) for objectives solved before this
# one are fields of their producers' records until it exists, and alerts
# here: 033's (a `task.gated` host fault with `alert` true: the third base
# in a row that disagreed with itself) and 031's (a backup record with
# `over_alert` true: R2 over its 8 GB alert threshold). The PostgreSQL log
# files the crash alert reads are the owner's to provide (deploy/); the
# scenario writes its own.
#
# A real `spawn loop` (the Bridge, the dispatcher, the supervisor) runs a
# resident `r1` of one gated mission and the supervisor on a fake engine,
# with a fake model judge (SPAWN_JUDGE_URL, a loopback HTTP server that
# answers WAITING, needs_human 0.3, and stuck 0.9 for the sessions named in
# $WORK/stuck-sessions, else 0.05). `spawn serve` answers the dashboard's
# API on a loopback port; `spawn alerts --follow --json` runs from the start.
# The loop runs with SPAWN_ALERT_CHECK_SECS=2, SPAWN_ALERT_UNDELIVERED_SECS=3,
# SPAWN_ALERT_ADMISSION_SECS=3, SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS=4,
# SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS=4, SPAWN_TEST_DATABASE_LOG and
# SPAWN_DATABASE_LOG naming two log files of its own, and host thresholds
# that cannot fire (SPAWN_ALERT_MEMORY_LOW_MB=1, SPAWN_ALERT_TMPFS_HIGH_MB=
# 100000000 on SPAWN_ALERT_TMPFS_PATH=$WORK/ram) except while it is
# restarted to make them fire. The records 028, 029, 031 and 033 define are
# appended to the log as their producers write them (spawn_append_trusted,
# no fold), so this objective does not wait for theirs. Checked:
#   (1) `spawn alerts --once --json` exits 0 with a JSON array, `GET
#       /api/alerts` answers 200 with one, and `spawn alerts --follow
#       --json` keeps running.
#   (2) task #T1 (r1, `BB:WRITE`) goes assigned -> delivered -> reported:
#       exactly one `task_state` notice for each of its three records
#       (evidence `to` and `event_id`), each within 5 s of its record.
#   (3) an input to a session that has no execution fails: one
#       `command_failed` notice, key `command_failed:<command id>`, its
#       evidence holding the command's error, within 5 s.
#   (4) the gate aborts T1 three passes in a row and the mission halts:
#       one `mission_state` notice per aborted gating (record `task.gated`,
#       after.gate_aborts 1, 2, 3); one
#       `mission_halted` alert (key `mission_halted:<mission>:<mission.halted
#       event id>`, evidence the reason) within 5 s, named by the text form
#       too; `spawn mission resume` clears it, the evidence naming the
#       `mission.resumed` record.
#   (5) crash lines appended to the test and the live database's logs: one
#       `postgres_crash` alert each (subjects `test`, `live`) within 10 s,
#       none again on later readings; a `database system is ready to accept
#       connections` line after them clears each, naming that line.
#   (6) the loop restarted with thresholds the host crosses (and
#       SPAWN_DISK_FLOOR_GB holding new executions): one `memory_low` and one
#       `tmpfs_high` alert within 20 s of the restart; task #T2, assigned to
#       a new worker that cannot start, gets one `task_undelivered` alert no
#       sooner than 3 s after its `task.assigned` and within 8 s; restarted
#       with the usual thresholds, the three clear (T2's naming its
#       `task.delivered` record), and nothing fires twice.
#   (7) r1 judged stuck (0.9) after a turn: one `agent_stuck` alert within 5
#       s of that judgement; a second stuck turn fires nothing new; judged
#       not stuck (0.05) after a third turn, it clears.
#   (8) appended records: `admission.waiting` -> one `admission_wait` no
#       sooner than 3 s after it and within 8 s, cleared by its
#       `admission.admitted`; `backup.dumped` and `backup.bundled` -> one
#       `backup_stale` each (subjects `dump`, `bundle`) no sooner than 4 s
#       after them and within 9 s, cleared by newer ones; `backup.verified`
#       ok false -> one `drill_failed` within 5 s, cleared by one ok true;
#       `execution.interrupted` cause `model_auth` -> one `model_auth`
#       within 5 s, cleared by its `execution.cleared`; a `task.gated`
#       host fault with `alert` true (033's) -> one `gate_base_disagrees`
#       notice within 5 s; the dump that clears the stale one carries
#       `over_alert` true (031's) -> one `backup_budget` within 5 s, cleared
#       by a later dump with `over_alert` false.
#   (9) every alert above appears once, with the same state and event ids,
#       in `spawn alerts --once --json`, in `GET /api/alerts` and in the
#       `--follow` stream (one `fired` line, and one `cleared` line after
#       it for a condition); the log holds at most one `alert.fired` and one
#       `alert.cleared` per key.
#
# Measured on the live main (b950e25; the deployed binary is built from it),
# 2026-11-06: see "Today" at the end of this header.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1. BB_030_MEASURE=1 (only to show today's behaviour)
# reports every failed check and goes on, notes what today's Spawn shows of
# each condition instead (the supervisor's digest, `spawn fleet`), then exits
# 1 if any check failed.
#
# Fails with 1 at (1) on the live main: `spawn alerts` is no command
# ("spawn: unknown command alerts"). It passes once the loop records each
# alert's transitions with their evidence, once per occurrence across
# restarts, and `spawn alerts --once`, `--follow` and `/api/alerts` show them.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, uses a
# private tmux socket and loopback only. Every wait is bounded in time.
#
# Today (b950e25, two runs each way; three more on 13668eb, the deploy
# before it, behaved the same): it fails at (1) in 1-2 s. With
# BB_030_MEASURE=1 it runs 118-147 s and every check fails: no alert of any
# kind is recorded or shown, GET /api/alerts answers 404 and the follow mode
# exits at once. What shows the conditions today, in Spawn's own record: the
# halt reaches the supervisor's digest as a `halted` stall 155-362 ms after
# `mission.halted` (SPAWN_STALL_HALTED_SECS 0); r1's stuck judgement,
# recorded 1.7-1.8 s after its line was submitted, sets `spawn fleet`'s
# attention but reaches no digest within 38 s (dispatch.rs skips a managed
# worker between tasks); nothing shows the task changes, the failed
# command, the crash lines, memory, /tmp, the undelivered task (its delivery
# stall is 600 s), the admission wait, the backups, the drill, the model
# login, 033's base-disagreement record or 031's over-alert record (one more
# measure run on b950e25 with them, 2026-11-06 18:25 CST: 147 s, the halt's
# digest 362 ms after its record, nothing else shown).
set -eu

asserted=""
harness() {
    echo "030: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
MEASURE=${BB_030_MEASURE-}

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed curl python3 dd awk sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_030_alerts_$run"
SPAWN_TMUX_SOCKET="bb-030-$run"
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
follow_pid=""
judge_pid=""
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
cleanup() {
    stop_pid "$follow_pid"
    stop_pid "$serve_pid"
    stop_pid "$loop_pid"
    stop_pid "$judge_pid"
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "$database_made" ]; then
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "030: the loop's last lines:" >&2
            tail -n 12 "$WORK/loop.log" >&2
        fi
        if [ -s "$WORK/follow.err" ]; then
            echo "030: spawn alerts --follow said:" >&2
            tail -n 4 "$WORK/follow.err" >&2
        fi
        if [ "$(jq length "$WORK/alerts.json" 2>/dev/null || echo 0)" -gt 0 ]; then
            echo "030: the last alerts read:" >&2
            jq -c '.[] | [.key, .state]' "$WORK/alerts.json" 2>/dev/null | tail -n 20 >&2 || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "030: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
# A check that BB_030_MEASURE=1 only reports.
failed_any=""
measured_fail() {
    if [ -n "$MEASURE" ]; then
        failed_any=1
        echo "030: (measure) would fail: $*"
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
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
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
  if IFS= read -r -t 1 typed; then
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

cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

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
mkdir -p "$WORK/ram"

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
    SPAWN_ALERT_PG_QUIET_SECS; do
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
SPAWN_TEST_DATABASE_LOG=$testlog
SPAWN_DATABASE_LOG=$livelog
SPAWN_ALERT_TMPFS_PATH=$WORK/ram
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PROJECTS BB_WORK SPAWN_TMUX_PROGRAM SPAWN_WORKER \
    SPAWN_SUPERVISOR SPAWN_MISSION_FILE SPAWN_LOOP_INTERVAL_MS SPAWN_JUDGE_URL \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_ALERT_CHECK_SECS \
    SPAWN_ALERT_UNDELIVERED_SECS SPAWN_ALERT_ADMISSION_SECS \
    SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS \
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
# today <label> <since ms> <regex> [<seconds>]: what today's Spawn showed
# of a condition provoked at <since ms>: a digest to the supervisor whose
# text matches (waited for up to <seconds> more), or nothing. Written for
# BB_030_MEASURE=1.
today() {
    [ -n "$MEASURE" ] || return 0
    until_s=$(($(now) + ${4:-0}))
    while :; do
        delay=$(q "select min(created_at_ms) - $2 from spawn_commands
                    where session_id = 'session_supervisor' and principal = 'spawn-dispatch'
                      and action ->> 'type' = 'input' and created_at_ms >= $2
                      and action ->> 'text' ~* \$r\$$3\$r\$")
        [ -z "$delay" ] && [ "$(now)" -lt "$until_s" ] || break
        ping_supervisor
        sleep 0.5
    done
    if [ -n "$delay" ]; then
        echo "$1: the supervisor's digest named it $delay ms later" >>"$WORK/today.txt"
    else
        echo "$1: nothing in Spawn showed it (no digest, no record) within $((($(now_ms) - $2) / 1000)) s" \
            >>"$WORK/today.txt"
    fi
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
    echo "030: $1: fired $delay ms after its condition"
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
today "task state change (#$t1 reported)" "$(event_at "$t1_reported")" "#$t1([^0-9]|\$)"

# --- (3) a failed command -------------------------------------------------------------

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
today "failed command #$cmd" "$cmd_done" "session_resident_nobody|command #?$cmd([^0-9]|\$)"

# --- (4) aborts and a halt -------------------------------------------------------------

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
echo "030: the mission halted after $passes integrator passes: $reason"
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
today "mission halt" "$halt_at" "halt"
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
today "test database crash line" "$crash_at" "terminated by signal|could not resize"
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

dd if=/dev/zero of="$WORK/ram/fill" bs=1048576 count=4 2>/dev/null || harness "cannot write $WORK/ram/fill"
stop_loop
restart_at=$(now_ms)
# shellcheck disable=SC2086
start_loop $CROSSED
for kind in memory_low tmpfs_high; do
    if fired 20 "$kind" host "(6) $kind" "the loop restarted with a $kind threshold the host crosses"; then
        within "(6) $kind after the restart" "$restart_at" 20000
    fi
done
if [ -z "$missing" ]; then
    one memory_low host "(6) memory_low" &&
        [ "$(field '.evidence.measured_mb < .evidence.threshold_mb')" = true ] &&
        [ "$(field .evidence.threshold_mb)" = 100000000 ] ||
        measured_fail "(6) the memory_low alert's evidence lacks the measured MiB under the threshold (100000000): $alert"
    one tmpfs_high host "(6) tmpfs_high" &&
        [ "$(field '.evidence.measured_mb > .evidence.threshold_mb')" = true ] &&
        [ "$(field .evidence.threshold_mb)" = 1 ] && [ "$(field .evidence.path)" = "$WORK/ram" ] ||
        measured_fail "(6) the tmpfs_high alert's evidence lacks the used MiB over the threshold (1) and the path ($WORK/ram): $alert"
fi
today "MemAvailable under its threshold" "$restart_at" "memory|MemAvailable"
today "/tmp over its threshold" "$restart_at" "tmpfs|/tmp|$WORK/ram"
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
    for kind in memory_low tmpfs_high; do one "$kind" host "(6) $kind read again" || true; done
fi
stop_loop
# shellcheck disable=SC2086
start_loop $USUAL
for kind in memory_low tmpfs_high; do
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
today "task #$t2 undelivered" "$t2_at" "#$t2([^0-9]|\$)"

# --- (7) an agent stuck by its latest judgement -------------------------------------------

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
    [ -n "$obs" ] && [ "$(field '.evidence.stuck >= 0.5')" = true ] ||
        measured_fail "(7) r1's agent_stuck alert names no judgement (evidence.event_id) with stuck >= 0.5: $alert"
    if [ -n "$obs" ]; then within "(7) r1 judged stuck" "$(event_at "$obs")" 5000; fi
fi
[ "$(grep -c 'session_resident_r1 0.9' "$WORK/judge.log" || true)" -ge 1 ] ||
    harness "the fake judge was never asked about r1 after its turn"
if [ -n "$MEASURE" ]; then
    judged=$(q "select min(recorded_at_ms) - $stuck_from from spawn_events
                 where session_id = 'session_resident_r1' and event_name = 'observation.recorded'
                   and recorded_at_ms >= $stuck_from
                   and (kind -> 'detail' -> 'observation' ->> 'stuck')::float >= 0.5")
    echo "agent stuck (r1): the Observer recorded the stuck judgement ${judged:-never} ms after the line was submitted" >>"$WORK/today.txt"
    attention=$("$SPAWN_BIN" fleet 2>/dev/null | jq -r '[.. | objects | select(.session_id? == "session_resident_r1") | .attention?] | map(select(. != null)) | first // "absent"' 2>/dev/null || echo unreadable)
    echo "agent stuck (r1): spawn fleet shows attention=$attention" >>"$WORK/today.txt"
fi
today "agent stuck (r1)" "$stuck_from" "session_resident_r1" 30
if [ -z "$missing" ]; then
    poke stuck-2
    pause 8
    refresh
    one agent_stuck session_resident_r1 "(7) r1 stuck again after another turn" || true
    : >"$WORK/stuck-sessions"
    poke unstuck
    if cleared 30 agent_stuck session_resident_r1 "(7) r1 no longer stuck"; then
        [ "$(field '.cleared_evidence.stuck < 0.5')" = true ] ||
            measured_fail "(7) r1's agent_stuck clearing names no judgement with stuck < 0.5: $(field '.cleared_evidence | tostring')"
        provoked cleared
    fi
fi

# --- (8) the records of 028, 029, 031 and 033 ------------------------------------------

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
today "admission wait" "$(event_at "$wait_rec")" "agent_worker_bb_wait|admission"
today "backup older than its schedule" "$(event_at "$dump1")" "backup|bundle"
today "restore drill failed" "$(event_at "$drill_bad")" "drill|8105"
today "model login expired" "$(event_at "$auth")" "exec_bb_auth|session_worker_bb_auth|login"
today "three bases in a row that disagreed with themselves (033)" "$(event_at "$gbd")" "base|011\\.sh"
if [ -n "$missing" ]; then
    over=$(append_record "{\"type\": \"backup.dumped\", \"keys\": [\"postgres/hourly/spawn-bb-over.dump.gpg\"], \"sha256\": \"$sha\", \"bytes\": 2400100, \"events\": 8107, \"max_event_id\": $max_id, \"over_alert\": true, \"held_bytes\": 8100000000, \"written_bytes\": 2400100, \"alert_bytes\": 8000000000, \"limit_bytes\": 10000000000, \"skipped\": []}")
    today "R2 over its alert threshold (031)" "$(event_at "$over")" "R2|budget|over" 5
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
    echo "030: $(wc -l <"$WORK/provoked.txt") alerts checked in spawn alerts --once, /api/alerts and --follow"
fi

if [ -n "$MEASURE" ] && [ -s "$WORK/today.txt" ]; then
    echo "030: (measure) what today's Spawn shows of each condition:"
    sed 's/^/030:   /' "$WORK/today.txt"
    echo "030: (measure) the digests the supervisor got (ms since the first, their items):"
    sql "select c.created_at_ms - min(c.created_at_ms) over () || ' ' || coalesce(c.succeeded::text, '-') || ' '
                || coalesce((select string_agg(t, ', ') from jsonb_array_elements_text(
                     case when jsonb_typeof(c.action -> 'tags') = 'array' then c.action -> 'tags'
                          else '[]'::jsonb end) t), '')
           from spawn_commands c
          where c.session_id = 'session_supervisor' and c.principal = 'spawn-dispatch'
            and c.action ->> 'type' = 'input'
          order by c.id" | sed 's/^/030:   /'
fi
if [ -n "$failed_any" ]; then
    asserted=1
    exit 1
fi
echo "030: every alert fired once with its evidence and cleared with its evidence, the same in spawn alerts --once, --follow and /api/alerts"
exit 0
