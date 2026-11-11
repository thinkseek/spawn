#!/bin/sh
# Black box: the scorecard says from when the owner's judgings are counted,
# so a window that starts before then does not read as complete.
#
# The problem, as the owner reads the Scorecard after 061 went live (lead,
# 11-12 04:00; lead-mac's screenshot): the fleet's gate time says "owner
# judgings 1" of 253 runs over 7 days, on a night with about 8 owner pushes.
# 061 counts an owner judging only when its record carries the gate's wall
# clock (`gate_ms`), which the integrator writes from the 03:31:50 deploy
# on: the live log's 36 owner landings and 12 refusals before it carry none
# (rev-b's live check), and their gate time was never recorded. The card
# shows "1" with nothing to say it counts from 03:48 only.
#
# Required (read through `GET /api/scorecard` and web/src/scorecardFormat.ts
# under node):
#   (1) /api/scorecard carries `owner_runs_from_ms`: the time of the log's
#       earliest owner judging (`source.moved` or `source.push_refused`) that
#       carries `gate_ms`, whatever the window; null when none does.
#   (2) scorecardFormat.ts exports `ownerRunsNote(since_ms,
#       owner_runs_from_ms)`: "Owner judgings counted from <localDateTime>
#       only: earlier ones carry no gate time." when the window starts
#       before that time; "Owner judgings not counted yet: none carries a
#       gate time." when it is null; null when the window starts at or
#       after it. The Scorecard page shows it beside the gate time's owner
#       runs.
#
# Records are written as the integrator writes them (spawn_control_record()
# for the requests, spawn_integrator_record() for their outcomes); `spawn
# serve` answers. Gate mission 1's task 1 is judged once. Then, a second
# apart: an owner landing recorded as before 061 (no gate_ms), an owner
# refusal its gate judged (gate_ms, mission 1: the first counted), and an
# owner landing its gate judged. Each check is reported; the run fails if
# any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "076: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test: the gate runs a scenario from the candidate's tree,
# and $controller is the deployed controller's copy.
tree=$(pwd)
[ -f "$tree/web/src/scorecardFormat.ts" ] || harness "run from the tree under test: no web/src/scorecardFormat.ts under $tree"
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_076_runs_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# A scratch HOME and Claude Code projects dir: nothing here reads or writes
# the real ones.
real_home=$HOME
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"

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
        echo "076: serve's last lines:" >&2
        tail -n 10 "$WORK/serve.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "076: harness: unexpected exit $rc" >&2
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

note() { echo "076: $*" >&2; }

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}

assign() { # <task> <mission>: prints the event id
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":$2,\"objective\":$1,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$1\",\"session_id\":\"session_worker_objective_$1\",\"scenario\":\"$WORK/ctl/scenarios/objective-$1\",\"brief\":\"Objective $1 of 076\"}"
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

SOURCE=$WORK/source-bb
B=$(printf 'bb-076-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-076-candidate' | sha1sum | cut -c1-40)
record "{\"type\":\"mission.added\",\"id\":1,\"text\":\"076 mission\",\"origin\":\"human\",\"sources\":[\"$SOURCE\"]}" >/dev/null
record "{\"type\":\"mission.configured\",\"id\":1,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\"}" >/dev/null
assign 1 1 >/dev/null
record "{\"type\":\"task.reported\",\"id\":1,\"commit\":\"$C\"}" >/dev/null
record "{\"type\":\"task.gated\",\"id\":1,\"verdict\":\"rejected\",\"revision\":0,\"base\":\"$B\",\"scenarios\":4,\"reason\":\"regression: a scenario fails on the candidate\",\"gate_ms\":600000}" >/dev/null
outcome() { # <record JSON>: through spawn_integrator_record(); prints its event id
    quoted=$(printf '%s' "$1" | sed "s/'/''/g")
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_integrator_record('$quoted'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "recording $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}
requested() { # <commit>: the owner's request, as `spawn mission push` records it
    record "{\"type\":\"source.push_requested\",\"source\":\"$SOURCE\",\"commit\":\"$1\",\"ref\":\"refs/spawn-owner/$1\",\"by\":\"human\",\"gated\":true}"
}
# Before 061: the landing names no gate time.
r1=$(requested "$C")
outcome "{\"type\":\"source.moved\",\"source\":\"$SOURCE\",\"from\":\"$B\",\"to\":\"$C\",\"request\":$r1,\"by\":\"human\",\"verdict\":\"accepted\"}" >/dev/null
sleep 1.2
# After: a refusal its gate judged, then a landing.
r2=$(requested "$B")
refused=$(outcome "{\"type\":\"source.push_refused\",\"request\":$r2,\"reason\":\"the owner's gate rejected it: regression: a scenario fails on the candidate\",\"mission\":1,\"gate_ms\":403471}")
sleep 1.2
r3=$(requested "$B")
outcome "{\"type\":\"source.moved\",\"source\":\"$SOURCE\",\"from\":\"$C\",\"to\":\"$B\",\"request\":$r3,\"by\":\"human\",\"verdict\":\"accepted\",\"mission\":1,\"gate_ms\":300000}" >/dev/null
from_ms=$(scalar "select recorded_at_ms from spawn_events where id = $refused")
[ -n "$from_ms" ] || harness "no time for the refusal's record $refused"
sleep 1.2
start_serve

card() { # <since ISO> <file>
    code=$(curl -s --noproxy '*' -o "$2" -w '%{http_code}' "$base/api/scorecard?since=$1" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/scorecard?since=$1 answered $code: $(head -c 300 "$2" 2>/dev/null)"
}
card "$(date -u -d '-1 hour' +%Y-%m-%dT%H:%M:%SZ)" "$WORK/hour.json"
card "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$WORK/now.json"
[ "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.gate_time | {task_runs, owner_runs}]' "$WORK/hour.json")" = '[{"task_runs":1,"owner_runs":2}]' ] ||
    harness "the last hour's fleet gate time is not 1 task run and 2 owner runs: $(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.gate_time]' "$WORK/hour.json")"
[ "$(jq '.since_ms > '"$from_ms" "$WORK/now.json")" = true ] || harness "the second window does not start after the refusal: $(jq -c '.since_ms' "$WORK/now.json") vs $from_ms"

# --- (1) from when the owner's judgings are counted -----------------------------------

expect_eq "$(jq -c '.owner_runs_from_ms' "$WORK/hour.json")" "$from_ms" \
    "(1) the last hour's owner_runs_from_ms: the time of the first owner judging with a gate time (the refusal), not the earlier landing without one"
expect_eq "$(jq -c '.owner_runs_from_ms' "$WORK/now.json")" "$from_ms" \
    "(1) a window that starts after every record: owner_runs_from_ms is the log's, whatever the window"
note "(1) checked: the answer"

# --- (2) the note -----------------------------------------------------------------------

cat >"$WORK/note.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { ownerRunsNote } = await import(pathToFileURL(`${env.BB_TREE}/web/src/scorecardFormat.ts`).href);
const { localDateTime } = await import(pathToFileURL(`${env.BB_TREE}/web/src/format.ts`).href);
const read = (name) => JSON.parse(readFileSync(`${env.BB_WORK}/${name}.json`, "utf8"));
const hour = read("hour");
const now = read("now");
const from = Number(env.BB_FROM_MS);

test("(2) a window that starts before the first counted judging says so", () => {
  assert.equal(
    ownerRunsNote(hour.since_ms, hour.owner_runs_from_ms),
    `Owner judgings counted from ${localDateTime(from)} only: earlier ones carry no gate time.`,
  );
});

test("(2) a window that starts after it says nothing; none counted says so", () => {
  assert.equal(ownerRunsNote(now.since_ms, now.owner_runs_from_ms), null);
  assert.equal(ownerRunsNote(from, from), null, "a window that starts at it");
  assert.equal(ownerRunsNote(hour.since_ms, null), "Owner judgings not counted yet: none carries a gate time.");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_FROM_MS=$from_ms node --test "$WORK/note.test.mjs" >"$WORK/note.out" 2>&1; then
    check_failed "(2) web/src/scorecardFormat.ts ownerRunsNote over the answers: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/note.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(2) checked: the note"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "076: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "076: the scorecard says from when the owner's judgings are counted, and the page says so for a window that starts before then"
exit 0
