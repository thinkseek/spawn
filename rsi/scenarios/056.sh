#!/bin/sh
# Black box: a validator that fails on main too is one item in Needs you
# per source and scenario, never about a closed mission, and it clears from
# the record once the scenario passes again or is gone.
#
# The problem, as the owner saw it on Home at 2026-11-11 21:55 SGT
# (sync/home-2026-11-11-2155.png): "Needs you" held 7 items, and 6 were the
# same "A validator fails on main too" alert, one per mission (#1, #2, #3,
# #4, #5, #7), 11 h old, all about one owner landing (request #129675)
# accepted without one scenario that failed on main too. Missions 1, 2, 3
# and 5 were closed (done). The scenario was repaired at 12:39 and retired
# at 13:23, and passed in every gate between; the alerts could never clear:
# an owner landing's validator_defect clears only once a gate names its
# scenario `quarantined` (crates/spawn-cli/src/alerts.rs, owner_landing),
# because the gate's verdict did not say which regressions it ran and
# passed. It does now: the verdict lists them in `passed` (run on the
# candidate and passed) and `reused` (not run: solved on the base, from the
# cache), and the integrator carries both onto the records it writes.
#
# Required (read through `GET /api/alerts`, `GET /api/missions` and
# web/src/home.ts's needsYou under node, over those answers):
#   (1) one owner landing (`source.moved`) accepted with a validator defect,
#       moving missions 1 and 2 (open) and 3 (closed), fires ONE open
#       `validator_defect` alert for its source and scenario: its evidence
#       names the `source`, the `scenario`, and `missions` [1, 2], the open
#       ones only.
#   (2) Needs you shows it as one item, its title and detail naming
#       missions #1 and #2 and not #3.
#   (3) a landing that moves only closed missions fires none.
#   (4) a later gating of a mission it names (`task.gated`) clears it when
#       the gating lists the scenario in `passed` or in `reused`
#       (cleared_evidence `cause` "passed", `event_id` that record), and
#       when the gating's `scenario_names` (the gate directory at that
#       gating) lacks it (`cause` "gone"). A gating that holds the scenario
#       and passed nothing of it leaves it open.
#   (5) once every mission it names is closed, it clears (`cause`
#       "closed", `event_id` the last `mission.closed`); one closed of two
#       leaves it open.
#   (6) after all that, Needs you holds no validator item.
#
# A real `spawn loop` runs (its alert step reads every 2 s) and `spawn serve`
# answers the API. The records are written as the integrator writes them,
# through spawn_control_record(); missions are closed with `spawn mission
# close`. Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "056: harness: $*" >&2
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
for tool in psql tmux git jq od bash sed grep awk curl node; do
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
SPAWN_DB_NAME="bb_056_runs_$run"
SPAWN_TMUX_SOCKET="bb-056-$run"
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
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$database_made" ]; then
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
            echo "056: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "056: the validator alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'validator_defect' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "056: harness: unexpected exit $rc" >&2
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

# A quiet engine: the loop may deliver the tasks below to a worker; this
# one shows a prompt, takes what is typed and does nothing with it.
cat >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if IFS= read -r -t 1 typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
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
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
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

now() { date +%s; }
q() { scalar "$1"; }
note() { echo "056: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}

# The scenario names of this run (not files of any gate directory).
ext=sh
V1="vone.$ext"
V0="vzero.$ext"
V2="vtwo.$ext"
V3="vthree.$ext"
V4="vfour.$ext"
V5="vfive.$ext"
OTHER="other.$ext"
B=$(printf 'bb-056-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-056-candidate' | sha1sum | cut -c1-40)
SOURCE=$WORK/source-bb

landing=0
moved() { # <missions JSON array> <validator_defects JSON array>: prints the event id
    landing=$((landing + 1))
    record "{\"type\":\"source.moved\",\"source\":\"$SOURCE\",\"request\":$((900 + landing)),\"from\":\"$B\",\"to\":\"$C\",\"missions\":$1,\"verdict\":\"accepted\",\"validator_defects\":$2,\"by\":\"human\"}"
}
gated() { # <task> <revision> <extra JSON fields>: a rejection; prints the event id
    record "{\"type\":\"task.reported\",\"id\":$1,\"commit\":\"$C\"}" >/dev/null
    record "{\"type\":\"task.gated\",\"id\":$1,\"verdict\":\"rejected\",\"revision\":$2,\"base\":\"$B\",\"scenarios\":4,\"reason\":\"regression: a scenario fails on the candidate\",$3}"
}
close_mission() { # <id>: prints the mission.closed event id
    capture "$SPAWN_BIN" mission close --id "$1" --state done --summary "056 closes it"
    [ "$status" -eq 0 ] || harness "spawn mission close --id $1 failed: $(err)"
    q "select max(id) from spawn_events where event_name = 'control.recorded'
         and kind -> 'detail' -> 'record' ->> 'type' = 'mission.closed'
         and kind -> 'detail' -> 'record' ->> 'id' = '$1'"
}

# The alert step has read up to <event id>: every alert step pass reads in
# order, so an alert fired on a later record proves it. A task notice is
# fired on every task record; this one is the marker. Any notice of the
# marker's task fired on a record from it on counts: a step that starts
# after the record was written reads from where it starts, and sees the
# task's next record (its delivery) instead.
marker=100
read_past() { # <what>: writes a marker task record and waits for its notice
    marker=$((marker + 1))
    m=$(record "{\"type\":\"task.assigned\",\"id\":$marker,\"mission_id\":4,\"objective\":$marker,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$marker\",\"session_id\":\"session_worker_objective_$marker\",\"scenario\":\"$WORK/ctl/scenarios/objective-$marker\",\"brief\":\"marker\"}")
    wait_until 30 marker_seen "$m" ||
        harness "$1: 30 s after record $m the loop's alert step has fired no task notice on it"
}
marker_seen() {
    [ "$(q "select count(*) from spawn_alerts where kind = 'task_state' and subject = 'task #$marker'
              and (evidence ->> 'event_id')::bigint >= $1")" -gt 0 ]
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
get() { # <path> <file>
    code=$(curl -s --noproxy '*' -o "$2" -w '%{http_code}' "$base$1" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET $1 answered $code: $(head -c 300 "$2" 2>/dev/null)"
}
alerts_json() { get "/api/alerts" "$WORK/alerts.json"; }
# The validator alerts of <scenario> in /api/alerts, open ones: a JSON array.
open_of() {
    alerts_json
    jq -c --arg s "$1" '[.[] | select(.kind == "validator_defect" and .state == "open"
        and .evidence.scenario == $s)]' "$WORK/alerts.json"
}
all_of() {
    alerts_json
    jq -c --arg s "$1" '[.[] | select(.kind == "validator_defect" and .evidence.scenario == $s)]' "$WORK/alerts.json"
}
cleared_by() { # <scenario>: the cleared evidence of its alert(s), or "open" / "none"
    alerts_json
    jq -c --arg s "$1" '[.[] | select(.kind == "validator_defect" and .evidence.scenario == $s)
        | if .state == "open" then "open" else {cause: .cleared_evidence.cause, event_id: .cleared_evidence.event_id} end]
        | if length == 0 then "none" else . end' "$WORK/alerts.json"
}

# Needs you under node, over the API's answers: a check script per call.
needs() { # <name> <test body>: runs it with the alerts and missions read now
    alerts_json
    get "/api/missions" "$WORK/missions.json"
    cat >"$WORK/$1.test.mjs" <<EOF
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { needsYou } = await import(pathToFileURL(\`\${env.BB_TREE}/web/src/home.ts\`).href);
const alerts = JSON.parse(readFileSync(env.BB_ALERTS, "utf8"));
const missions = JSON.parse(readFileSync(env.BB_MISSIONS, "utf8"));
const items = needsYou({ attention: null, missions, alerts, agents: [], health: null, missingAlwaysOn: 0, now: Date.now() });
const validator = items.filter((item) => String(item.key).startsWith("alert:validator_defect"));
const words = (item) => \`\${item.title ?? ""} \${item.detail ?? ""}\`;
$2
EOF
    BB_TREE=$tree BB_ALERTS=$WORK/alerts.json BB_MISSIONS=$WORK/missions.json BB_V1=$V1 \
        node --test "$WORK/$1.test.mjs" >"$WORK/$1.out" 2>&1
}
needs_failure() { grep -E 'not ok|AssertionError|Error|expected|actual' "$WORK/$1.out" | head -n 8 | tr '\n' ' ' | head -c 700; }

# --- the missions ----------------------------------------------------------------

for id in 1 2 3; do
    record "{\"type\":\"mission.added\",\"id\":$id,\"text\":\"056 mission $id\",\"origin\":\"human\",\"sources\":[]}" >/dev/null
    record "{\"type\":\"mission.configured\",\"id\":$id,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\"}" >/dev/null
done
# Mission 4 carries the markers only.
record '{"type":"mission.added","id":4,"text":"056 markers","origin":"human","sources":[]}' >/dev/null
for id in 1 2; do
    record "{\"type\":\"task.assigned\",\"id\":$id,\"mission_id\":$id,\"objective\":$id,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$id\",\"session_id\":\"session_worker_objective_$id\",\"scenario\":\"$WORK/ctl/scenarios/objective-$id\",\"brief\":\"Objective $id of 056\"}" >/dev/null
done

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve
read_past "the start"
close_mission 3 >/dev/null
read_past "mission 3 closed"

# --- (1) one landing, one alert, its open missions ------------------------------

moved '[1,2,3]' "[\"$V1\"]" >/dev/null
read_past "(1) the landing"
got=$(open_of "$V1")
expect_eq "$(printf '%s' "$got" | jq 'length')" 1 \
    "(1) open validator_defect alerts for $V1 after one owner landing that moved missions 1, 2 (open) and 3 (closed): $(printf '%s' "$got" | jq -c '[.[] | {key, subject}]')"
expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .evidence.missions]')" '[[1,2]]' \
    "(1) the alert's evidence.missions (the open missions the landing moved)"
expect_eq "$(printf '%s' "$got" | jq -c '[.[] | .evidence.source]')" "[\"$SOURCE\"]" \
    "(1) the alert's evidence.source (the source the landing moved)"
note "(1) checked: one alert for one landing's defect, its open missions"

# --- (2) Needs you: one item, missions #1 and #2 -------------------------------------

if ! needs one 'test("one validator item naming the open missions", () => {
  const mine = validator.filter((item) => words(item).includes(env.BB_V1));
  assert.equal(mine.length, 1, `Needs you items for ${env.BB_V1}: ${JSON.stringify(mine.map((i) => i.title))}`);
  const text = words(mine[0]);
  assert.match(text, /#1\b/, `item names mission #1: ${text}`);
  assert.match(text, /#2\b/, `item names mission #2: ${text}`);
  assert.doesNotMatch(text, /#3\b/, `item names closed mission #3: ${text}`);
});'; then
    check_failed "(2) web/src/home.ts needsYou over /api/alerts and /api/missions: $(needs_failure one)"
fi
note "(2) checked: Needs you shows the defect once"

# --- (3) a landing that moves only closed missions -----------------------------------

moved '[3]' "[\"$V0\"]" >/dev/null
read_past "(3) the closed mission's landing"
expect_eq "$(all_of "$V0" | jq 'length')" 0 \
    "(3) validator_defect alerts for $V0, from a landing that moved only closed mission 3"
note "(3) checked: no alert about a closed mission"

# --- (4) passed, reused, gone ----------------------------------------------------------

moved '[1,2]' "[\"$V2\",\"$V3\",\"$V4\"]" >/dev/null
read_past "(4) the second landing"
g1=$(gated 1 0 "\"scenario_names\":[\"$OTHER\",\"$V2\",\"$V3\",\"$V4\"],\"passed\":[\"$OTHER\",\"$V2\"],\"reused\":[\"$V3\"]")
read_past "(4) mission 1's gating"
expect_eq "$(cleared_by "$V2")" "[{\"cause\":\"passed\",\"event_id\":$g1}]" \
    "(4) $V2's alert after mission 1's gating ran it and it passed (\"passed\")"
expect_eq "$(cleared_by "$V3")" "[{\"cause\":\"passed\",\"event_id\":$g1}]" \
    "(4) $V3's alert after mission 1's gating took it as solved on the base (\"reused\")"
expect_eq "$(cleared_by "$V4")" '["open"]' \
    "(4) $V4's alert after a gating whose gate directory still holds it and which passed nothing of it"
g2=$(gated 2 0 "\"scenario_names\":[\"$OTHER\",\"$V2\",\"$V3\"]")
read_past "(4) mission 2's gating"
expect_eq "$(cleared_by "$V4")" "[{\"cause\":\"gone\",\"event_id\":$g2}]" \
    "(4) $V4's alert after mission 2's gating, whose gate directory no longer holds it"
note "(4) checked: passed and reused clear it, gone clears it, a gating that passed nothing of it does not"

# --- (5) every mission it names closed ------------------------------------------------

moved '[1,2]' "[\"$V5\"]" >/dev/null
read_past "(5) the third landing"
expect_eq "$(open_of "$V5" | jq 'length')" 1 "(5) open alerts for $V5 after its landing"
close_mission 1 >/dev/null
read_past "(5) mission 1 closed"
expect_eq "$(cleared_by "$V5")" '["open"]' "(5) $V5's alert after one of its two missions closed"
c2=$(close_mission 2)
read_past "(5) mission 2 closed"
expect_eq "$(cleared_by "$V5")" "[{\"cause\":\"closed\",\"event_id\":$c2}]" \
    "(5) $V5's alert once both of its missions are closed"
note "(5) checked: closing every mission it names clears it"

# --- (6) Needs you is clear of validators ---------------------------------------------

if ! needs none 'test("no validator item is left", () => {
  assert.deepEqual(validator.map((item) => words(item)), []);
});'; then
    check_failed "(6) web/src/home.ts needsYou after every defect cleared: $(needs_failure none)"
fi
note "(6) checked: Needs you holds no validator item"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "056: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "056: a validator defect is one item per source and scenario, about open missions only, and it clears when the scenario passes again, is gone, or every mission it names is closed"
exit 0
