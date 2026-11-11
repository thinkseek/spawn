#!/bin/sh
# Black box: the scorecard says how much gate time decided nothing.
#
# The problem (sync/roadmap-effects.md, "Core tracks: compute": "no machine
# time is spent on runs that decide nothing"; rev-b's compute study
# 2026-11-11: gates use about 33 CPU-hours a day, about 6 of them on
# judgings that ended with no verdict). The live gate verdicts (read-only
# /api/rsi, 2026-11-12) count 12 aborted, 13 host_fault, 1 no_room and 18
# stale beside 170 accepted and 104 rejected, conflict or incompatible. The
# Scorecard's gate time ("15m per gate run, n 242") counts every run alike
# (crates/spawn-cli/src/scorecard/history.rs `gated`, measure.rs
# `gate_time`): what the gate spent on runs that decided nothing, and so
# whether the compute work is paying off, is on no page.
#
# Required (read through `GET /api/scorecard` and web/src/scorecardFormat.ts
# under node, which loads it alone):
#   (1) every scope's `gate_time` carries `total_ms` (the gate time of its
#       runs in the window, summed), `undecided_ms` and `undecided_runs`
#       (those whose verdict decided nothing: aborted, no_room, host_fault
#       or stale), beside `value`, `n` and `runs` as today.
#   (2) scorecardFormat.ts exports `undecidedText(gate_time)`: "<undecided>
#       of <total> gate time decided nothing (<percent>%)", a duration
#       "<n>m" under an hour and "<n.n> h" from one, the percent whole;
#       null without gate time.
#
# Records are written as the integrator writes them, through
# spawn_control_record(); `spawn serve` answers. Gate mission 1's tasks 1
# to 6 are judged once each: accepted (10 min), rejected (15 min),
# conflict (5 min), aborted (20 min), host_fault (8 min) and stale (7 min,
# main moved while the gate ran): 65 min, 35 of them deciding nothing. Each check is reported; the run fails if any
# failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "067: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$tree/web/src/scorecardFormat.ts" ] || harness "no web/src/scorecardFormat.ts under $tree (run from the candidate's tree)"
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl node; do
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
SPAWN_DB_NAME="bb_067_undecided_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
serve_pid=""
cleanup() {
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/serve.log" ]; then
        echo "067: serve's last lines:" >&2
        tail -n 10 "$WORK/serve.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "067: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# A failed check is reported and the run goes on; the run fails at its end
# if any did. Unless the database is gone: then nothing read can be trusted.
: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
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
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main


for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT \
    SPAWN_CLAUDE_PROJECTS SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl/scenarios" "$WORK/web"
: >"$SPAWN_MISSION_FILE"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

note() { echo "067: $*" >&2; }

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}

assign() { # <task> <mission>: prints the event id
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":$2,\"objective\":$1,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$1\",\"session_id\":\"session_worker_objective_$1\",\"scenario\":\"$WORK/ctl/scenarios/objective-$1\",\"brief\":\"Objective $1 of 067\"}"
}

start_serve() {
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >"$WORK/serve.log" 2>&1 &
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

# --- the records ----------------------------------------------------------------

B=$(printf 'bb-067-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-067-candidate' | sha1sum | cut -c1-40)
record '{"type":"mission.added","id":1,"text":"067 mission","origin":"human","sources":[]}' >/dev/null
record "{\"type\":\"mission.configured\",\"id\":1,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\"}" >/dev/null
judged() { # <task> <verdict> <gate_ms>: assigned, reported, then judged once
    assign "$1" 1 >/dev/null
    record "{\"type\":\"task.reported\",\"id\":$1,\"commit\":\"$C\",\"report\":\"Task $1 is done.\"}" >/dev/null
    record "{\"type\":\"task.gated\",\"id\":$1,\"verdict\":\"$2\",\"revision\":0,\"base\":\"$B\",\"scenarios\":4,\"candidate\":\"$C\",\"reason\":\"bb-067 $2\",\"gate_ms\":$3}" >/dev/null
}
judged 1 accepted 600000
judged 2 rejected 900000
judged 3 conflict 300000
judged 4 aborted 1200000
judged 5 host_fault 480000
# Stale as the integrator writes it when main moved while the gate ran
# (integrate.rs stale_on_main_move): what the gate found, and main now.
assign 6 1 >/dev/null
record "{\"type\":\"task.reported\",\"id\":6,\"commit\":\"$C\",\"report\":\"Task 6 is done.\"}" >/dev/null
M=$(printf 'bb-067-main-now' | sha1sum | cut -c1-40)
record "{\"type\":\"task.gated\",\"id\":6,\"verdict\":\"stale\",\"found\":\"accepted\",\"revision\":0,\"base\":\"$B\",\"main_now\":\"$M\",\"scenarios\":4,\"reason\":\"main moved while the gate ran: its accepted is no verdict on the new main; judged again\",\"gate_ms\":420000}" >/dev/null
start_serve

since=$(date -u -d '-1 hour' +%Y-%m-%dT%H:%M:%SZ)
code=$(curl -s --noproxy '*' -o "$WORK/card.json" -w '%{http_code}' "$base/api/scorecard?since=$since" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/scorecard?since=$since answered $code: $(head -c 300 "$WORK/card.json" 2>/dev/null)"
[ "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.gate_time.runs]' "$WORK/card.json")" = '[6]' ] ||
    harness "the fleet's gate_time does not count the six runs: $(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.gate_time]' "$WORK/card.json")"
gate() { # <scope filter>: that scope's {total_ms, undecided_ms, undecided_runs, runs}
    jq -c "[.scopes[] | select($1) | .dimensions.gate_time | {total_ms, undecided_ms, undecided_runs, runs}]" "$WORK/card.json"
}

# --- (1) the answer ----------------------------------------------------------------------

expect_eq "$(gate '.scope == "fleet"')" '[{"total_ms":3900000,"undecided_ms":2100000,"undecided_runs":3,"runs":6}]' \
    "(1) the fleet's gate time: 65 min over six runs, 35 min of it (aborted, host_fault, stale) deciding nothing"
expect_eq "$(gate '.scope == "mission" and .mission == 1')" '[{"total_ms":3900000,"undecided_ms":2100000,"undecided_runs":3,"runs":6}]' \
    "(1) mission 1's gate time, the same six runs"
note "(1) checked: the answer"

# --- (2) the page's words ----------------------------------------------------------------------

jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.gate_time][0]' "$WORK/card.json" >"$WORK/gate.json"
cat >"$WORK/undecided.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { undecidedText } = await import(pathToFileURL(`${env.BB_TREE}/web/src/scorecardFormat.ts`).href);
const gate = JSON.parse(readFileSync(`${env.BB_WORK}/gate.json`, "utf8"));

test("(2) what the gate spent on runs that decided nothing", () => {
  assert.equal(undecidedText(gate), "35m of 1.1 h gate time decided nothing (54%)");
  assert.equal(
    undecidedText({ ...gate, total_ms: 36_000_000, undecided_ms: 5_400_000 }),
    "1.5 h of 10.0 h gate time decided nothing (15%)",
  );
  assert.equal(undecidedText({ ...gate, total_ms: 0, undecided_ms: 0, undecided_runs: 0 }), null);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/undecided.test.mjs" >"$WORK/undecided.out" 2>&1; then
    check_failed "(2) web/src/scorecardFormat.ts undecidedText over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/undecided.out" | head -n 8 | tr '\n' ' ' | head -c 700)"
fi
note "(2) checked: the page's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "067: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "067: the scorecard says how much gate time decided nothing, for the fleet and each mission"
exit 0
