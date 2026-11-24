#!/bin/sh
# Black box: a mission that was only ever added, with no configuration
# record at all, is owner work on the scorecard's idle hours card, as a
# mission configured with no backlog is.
#
# The problem, as the owner reads the Scorecard after 062 went live
# (bf1f5c56, deployed 04:35; lead-mac): the card still flags the two
# owner-work missions idle. The live `/api/scorecard/fleet?window=7d` says
# {"idle": true, "owner_work": false} for #7 (112.5 / 148.6 h) and #8
# (111.0 / 145.2 h), and false for the backlog missions 4 and 6. Missions 7
# and 8 have a `mission.added` record (events 4617 and 7514, 11-06) and
# no `mission.configured` at all. The owner_work query in
# crates/spawn-postgres/src/experience.rs counts a never-configured mission
# as not owner work, so 062 changed nothing for the two missions it was
# written for.
#
# Required (read through `GET /api/scorecard/fleet?window=24h` and
# web/src/fleetDecisions.ts's decisionsFromApi under node over it):
#   (1) a mission with no configuration record (only `mission.added`) has
#       no objective backlog: its row is {idle false, owner_work true},
#       whatever its text says.
#   (2) a mission configured with a backlog and no task all along is still
#       {idle true, owner_work false}.
#   (3) the card's rows say the same: [idle, ownerWork] [false, true] for
#       the never-configured missions, [true, false] for the backlog one.
#
# Seeded like live, through spawn_control_record() as the integrator
# writes records: mission 4 takes its work from a backlog
# (`mission.configured` with `objectives`); missions 7 and 8 have only
# their `mission.added`, 7 with live's "Owner work on ..." text and 8 with
# a text that says nothing of owner work. No mission has a task. `spawn
# serve` answers. Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "077: harness: $*" >&2
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
[ -f "$tree/web/src/fleetDecisions.ts" ] || harness "no web/src/fleetDecisions.ts under $tree (run from the candidate's tree)"
for tool in psql git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_077_runs_$run"
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
        echo "077: serve's last lines:" >&2
        tail -n 10 "$WORK/serve.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "077: harness: unexpected exit $rc" >&2
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

note() { echo "077: $*" >&2; }

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
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

# As live: 4 takes its work from an objective backlog; 7 and 8 were added
# and never configured.
mkdir -p "$WORK/ctl/backlog"
record "{\"type\":\"mission.added\",\"id\":4,\"text\":\"Bb RSI: improve the product by its objectives\",\"origin\":\"human\",\"sources\":[]}" >/dev/null
record "{\"type\":\"mission.configured\",\"id\":4,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\",\"objectives\":{\"backlog\":\"$WORK/ctl/backlog\",\"resident\":\"bbres\",\"workers\":1}}" >/dev/null
record '{"type":"mission.added","id":7,"text":"Owner work on Bb Spawn: changes the owner requests","origin":"human","sources":[]}' >/dev/null
record '{"type":"mission.added","id":8,"text":"Bb Supply: the second mission the owner added","origin":"human","sources":[]}' >/dev/null
start_serve
# Some open time for every mission.
sleep 6

fleet_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/fleet.json" -w '%{http_code}' "$base/api/scorecard/fleet?window=24h" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/scorecard/fleet?window=24h answered $code: $(head -c 300 "$WORK/fleet.json" 2>/dev/null)"
}
fleet_json
row() { jq -c --argjson m "$1" '[.missions[] | select(.mission == $m) | {idle, owner_work}]' "$WORK/fleet.json"; }
[ "$(jq -c '[.missions[] | .mission] | sort' "$WORK/fleet.json")" = '[4,7,8]' ] ||
    harness "the idle hours rows are not missions 4, 7 and 8: $(jq -c '.missions' "$WORK/fleet.json" | head -c 400)"

# --- (1) the never-configured missions ----------------------------------------------

expect_eq "$(row 7)" '[{"idle":false,"owner_work":true}]' \
    "(1) mission 7's row (only mission.added, no configuration, no task): owner_work, never flagged idle"
expect_eq "$(row 8)" '[{"idle":false,"owner_work":true}]' \
    "(1) mission 8's row (only mission.added, a text that says nothing of owner work): owner_work, never flagged idle"
note "(1) checked: the never-configured missions"

# --- (2) the backlog mission ---------------------------------------------------------

expect_eq "$(row 4)" '[{"idle":true,"owner_work":false}]' \
    "(2) mission 4's row (configured with a backlog, no task all along): idle, not owner work"
note "(2) checked: the backlog mission"

# --- (3) the card's model ------------------------------------------------------------

cat >"$WORK/idle.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { decisionsFromApi } = await import(pathToFileURL(`${env.BB_TREE}/web/src/fleetDecisions.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/fleet.json`, "utf8"));

test("the card's rows: a never-configured mission is owner work", () => {
  const rows = decisionsFromApi(answer).idleHoursByMission;
  const by = (id) => rows.find((row) => row.mission === id);
  assert.deepEqual([by(4)?.idle, by(4)?.ownerWork], [true, false], "mission 4");
  assert.deepEqual([by(7)?.idle, by(7)?.ownerWork], [false, true], "mission 7");
  assert.deepEqual([by(8)?.idle, by(8)?.ownerWork], [false, true], "mission 8");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/idle.test.mjs" >"$WORK/idle.out" 2>&1; then
    check_failed "(3) web/src/fleetDecisions.ts decisionsFromApi over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/idle.out" | head -n 8 | tr '\n' ' ' | head -c 700)"
fi
note "(3) checked: the card's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "077: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "077: a never-configured mission is owner work on the idle hours card; a backlog mission is flagged idle as before"
exit 0
