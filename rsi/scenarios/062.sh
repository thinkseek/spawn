#!/bin/sh
# Black box: the scorecard's "Idle hours by mission" flags idle only a
# mission that takes its work from a backlog; an owner-work mission is said
# to be owner work.
#
# The problem, as the owner reads the Scorecard after 73e3596d (lead-mac,
# sync/queue.md 23:45; sync/page-scorecard-2026-11-11-2336.png): the card
# flags the two owner-work missions "idle" (#7 107 / 144 h, 75%; #8 106 /
# 140 h, 76%), the only two badges on it, while their work comes from the
# owner when there is some: no task there is no fault. The live
# `/api/scorecard/fleet?window=7d` says `"idle": true` for 7 and 8 and
# false for 4 (63 / 168 h) and 6 (61 / 151 h), the backlog missions whose
# idle hours are the fleet's real loss (objective 058).
#
# Required (read through `GET /api/scorecard/fleet?window=24h` and
# web/src/fleetDecisions.ts's decisionsFromApi under node over it):
#   (1) every `missions` row carries `owner_work`: true for a mission with
#       no objective backlog. Such a row is never `idle`; a backlog
#       mission's `idle` is as today.
#   (2) decisionsFromApi(answer).idleHoursByMission carries `ownerWork`
#       beside `idle`, so the card says "owner work" where it flagged idle.
#
# Records are written as the integrator writes them, through
# spawn_control_record(); `spawn serve` answers. Missions 1 and 3 take work
# from a backlog, mission 2 is owner work; mission 3 has a task in work all
# along, 1 and 2 none. Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "062: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
[ -f "$tree/web/src/home.ts" ] || harness "no web/src/home.ts under $tree (run from the candidate's tree)"
for tool in psql git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_062_runs_$run"
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
        echo "062: serve's last lines:" >&2
        tail -n 10 "$WORK/serve.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "062: harness: unexpected exit $rc" >&2
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

note() { echo "062: $*" >&2; }

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}

assign() { # <task> <mission>: prints the event id
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":$2,\"objective\":$1,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$1\",\"session_id\":\"session_worker_objective_$1\",\"scenario\":\"$WORK/ctl/scenarios/objective-$1\",\"brief\":\"Objective $1 of 062\"}"
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

# --- the missions ----------------------------------------------------------------

# Missions 1 and 3 take their work from an objective backlog, as the live
# RSI and Supply PA missions do; mission 2 is owner work (no backlog: its
# tasks come from the owner). Mission 3 has a task in work from the start;
# missions 1 and 2 have none.
mkdir -p "$WORK/ctl/backlog"
for id in 1 2 3; do
    record "{\"type\":\"mission.added\",\"id\":$id,\"text\":\"062 mission $id\",\"origin\":\"human\",\"sources\":[]}" >/dev/null
done
for id in 1 3; do
    record "{\"type\":\"mission.configured\",\"id\":$id,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"bbres\",\"workers\":1}}" >/dev/null
done
record '{"type":"mission.configured","id":2,"accept":"review"}' >/dev/null
assign 1 3 >/dev/null
start_serve
# Some open time for every mission.
sleep 6

fleet_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/fleet.json" -w '%{http_code}' "$base/api/scorecard/fleet?window=24h" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/scorecard/fleet?window=24h answered $code: $(head -c 300 "$WORK/fleet.json" 2>/dev/null)"
}
fleet_json
row() { jq -c --argjson m "$1" '[.missions[] | select(.mission == $m) | {idle, owner_work}]' "$WORK/fleet.json"; }
[ "$(jq -c '[.missions[] | .mission] | sort' "$WORK/fleet.json")" = '[1,2,3]' ] ||
    harness "the idle hours rows are not missions 1, 2 and 3: $(jq -c '.missions' "$WORK/fleet.json" | head -c 400)"

# --- (1) the answer's rows -------------------------------------------------------------

expect_eq "$(row 2)" '[{"idle":false,"owner_work":true}]' \
    "(1) mission 2's idle hours row (owner work, no task all along): owner_work, and never flagged idle"
expect_eq "$(row 1)" '[{"idle":true,"owner_work":false}]' \
    "(1) mission 1's row (a backlog mission with no task all along): idle"
expect_eq "$(row 3)" '[{"idle":false,"owner_work":false}]' \
    "(1) mission 3's row (a backlog mission with a task in work all along): not idle"
note "(1) checked: the rows"

# --- (2) the card's model ------------------------------------------------------------------

cat >"$WORK/idle.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { decisionsFromApi } = await import(pathToFileURL(`${env.BB_TREE}/web/src/fleetDecisions.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/fleet.json`, "utf8"));

test("the card's rows: owner work is said, never flagged idle", () => {
  const rows = decisionsFromApi(answer).idleHoursByMission;
  const by = (id) => rows.find((row) => row.mission === id);
  assert.deepEqual([by(1)?.idle, by(1)?.ownerWork], [true, false], "mission 1");
  assert.deepEqual([by(2)?.idle, by(2)?.ownerWork], [false, true], "mission 2");
  assert.deepEqual([by(3)?.idle, by(3)?.ownerWork], [false, false], "mission 3");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/idle.test.mjs" >"$WORK/idle.out" 2>&1; then
    check_failed "(2) web/src/fleetDecisions.ts decisionsFromApi over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/idle.out" | head -n 8 | tr '\n' ' ' | head -c 700)"
fi
note "(2) checked: the card's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "062: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "062: the idle hours card says owner work for a mission with no backlog and flags only backlog missions idle"
exit 0
