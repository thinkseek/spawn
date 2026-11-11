#!/bin/sh
# Black box: the scorecard's gate time counts the owner's judgings.
#
# The problem, as the owner reads the Scorecard after 73e3596d (lead-mac,
# sync/queue.md 23:45; sync/page-scorecard-2026-11-11-2336.png): owner
# pushes have been judged by the integrator's gate since 11-10, 42
# judgings in 48 h, on the same gate slots as the fleet's tasks, and none
# is counted anywhere. The fleet card says "Gate time 15m per gate run, n
# 242" from task gatings alone; mission #7 (owner work on Spawn) says "not
# enough data (n 0)", "gate time 0ms". An owner judging's record
# (`source.moved`, crates/spawn-cli/src/integrate/owner.rs) carries its
# verdict and no gate time, and names no mission whose gate judged it; the
# scorecard (crates/spawn-cli/src/scorecard/history.rs) reads gate runs from
# `task.gated` records only.
#
# Required (read through `GET /api/scorecard`):
#   (1) the fleet's `gate_time` counts every gate run, a task's gating and
#       an owner judging alike (an owner judging's record carries `gate_ms`
#       and `mission`, the mission whose gate judged it), with `runs` split
#       into `task_runs` and `owner_runs`.
#   (2) the judging mission's `gate_time` counts them the same way, and
#       no other mission's does: the missions an owner landing moves (the
#       store's `missions` on its record) are not the mission whose gate
#       judged it.
#
# Task records are written through spawn_control_record(); each owner
# landing is requested as `spawn mission push` records it and its outcome is
# recorded through spawn_integrator_record(), as the integrator records it,
# with the `gate_ms` and `mission` the integrator must now write. `spawn
# serve` answers. Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "061: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_061_runs_$run"
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
        echo "061: serve's last lines:" >&2
        tail -n 10 "$WORK/serve.log" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "061: harness: unexpected exit $rc" >&2
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
# A scratch HOME: nothing here reads or writes the real one (cc-rules (a)).
real_home=$HOME
HOME=$WORK/home
mkdir -p "$HOME"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
export HOME SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT \
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

note() { echo "061: $*" >&2; }

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}

assign() { # <task> <mission>: prints the event id
    record "{\"type\":\"task.assigned\",\"id\":$1,\"mission_id\":$2,\"objective\":$1,\"kind\":\"ephemeral\",\"created_by\":\"spawn-integrate\",\"agent_id\":\"agent_worker_objective_$1\",\"session_id\":\"session_worker_objective_$1\",\"scenario\":\"$WORK/ctl/scenarios/objective-$1\",\"brief\":\"Objective $1 of 061\"}"
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

# Missions 1 and 2 are gate missions on the same source, at base B. Mission
# 1's tasks 1 and 2 are judged by its gate (10 and 15 min), mission 2's task
# 3 by its own (6.7 min). The owner's three landings on the source are judged
# by mission 1's gate, as the integrator judges an owner push between task
# gates (20, 5 and 7.5 min). Each is requested as `spawn mission push`
# records it, and its outcome is recorded through spawn_integrator_record()
# as the integrator records it, plus the gate's wall clock (`gate_ms`) and
# the mission whose gate judged it (`mission`). The store names the missions
# the landing moves (`missions`: 1 and 2), which is not the mission whose
# gate judged it.
SOURCE=$WORK/source-bb
B=$(printf 'bb-061-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-061-candidate' | sha1sum | cut -c1-40)
for m in 1 2; do
    record "{\"type\":\"mission.added\",\"id\":$m,\"text\":\"061 mission $m\",\"origin\":\"human\",\"sources\":[\"$SOURCE\"]}" >/dev/null
    record "{\"type\":\"mission.configured\",\"id\":$m,\"accept\":\"gate\",\"gate\":\"$WORK/ctl/scenarios\"}" >/dev/null
done
gated() { # <task> <gate_ms>: the task's gate rejected it at base B
    record "{\"type\":\"task.reported\",\"id\":$1,\"commit\":\"$C\"}" >/dev/null
    record "{\"type\":\"task.gated\",\"id\":$1,\"verdict\":\"rejected\",\"revision\":0,\"base\":\"$B\",\"scenarios\":4,\"reason\":\"regression: a scenario fails on the candidate\",\"gate_ms\":$2}" >/dev/null
}
assign 1 1 >/dev/null
assign 2 1 >/dev/null
assign 3 2 >/dev/null
gated 1 600000
gated 2 900000
gated 3 400000
owner_landing() { # <from> <to> <gate_ms>: requested, judged by mission 1's gate, moved
    request=$(record "{\"type\":\"source.push_requested\",\"source\":\"$SOURCE\",\"commit\":\"$2\",\"ref\":\"refs/spawn-owner/$2\",\"by\":\"human\",\"gated\":true}")
    moved=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_integrator_record('{\"type\":\"source.moved\",\"source\":\"$SOURCE\",\"from\":\"$1\",\"to\":\"$2\",\"request\":$request,\"by\":\"human\",\"verdict\":\"accepted\",\"mission\":1,\"gate_ms\":$3}'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$moved" in '' | *[!0-9]*) harness "recording the owner landing of request $request failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    [ "$(scalar "select kind -> 'detail' -> 'record' ->> 'missions' from spawn_events where id = $moved")" = '[1, 2]' ] ||
        harness "the store did not name missions 1 and 2 on the owner landing: $(scalar "select kind -> 'detail' -> 'record' from spawn_events where id = $moved")"
}
owner_landing "$B" "$C" 1200000
owner_landing "$C" "$B" 300000
owner_landing "$B" "$C" 450000
start_serve

since=$(date -u -d '-1 hour' +%Y-%m-%dT%H:%M:%SZ)
code=$(curl -s --noproxy '*' -o "$WORK/card.json" -w '%{http_code}' "$base/api/scorecard?since=$since" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/scorecard?since=$since answered $code: $(head -c 300 "$WORK/card.json" 2>/dev/null)"
gate() { # <scope filter>: that scope's gate_time {value, n, runs, task_runs, owner_runs}
    jq -c "[.scopes[] | select($1) | .dimensions.gate_time | {value, n, runs, task_runs, owner_runs}]" "$WORK/card.json"
}
[ "$(jq -c '[.scopes[] | select(.scope == "mission") | .mission] | sort' "$WORK/card.json")" = '[1,2]' ] ||
    harness "the card's mission scopes are not missions 1 and 2: $(jq -c '[.scopes[] | {scope, mission}]' "$WORK/card.json")"

# --- (1) the fleet's gate time counts the owner's judgings ----------------------------

expect_eq "$(gate '.scope == "fleet"')" '[{"value":450000,"n":6,"runs":6,"task_runs":3,"owner_runs":3}]' \
    "(1) the fleet's gate_time over three task gatings (10, 15, 6.7 min) and three owner judgings (20, 5, 7.5 min): the median of the six (nearest rank), with the runs split by whose they were"
note "(1) checked: the fleet"

# --- (2) and the mission's, whose gate judged them ----------------------------------------

expect_eq "$(gate '.scope == "mission" and .mission == 1')" '[{"value":600000,"n":5,"runs":5,"task_runs":2,"owner_runs":3}]' \
    "(2) mission 1's gate_time: its two tasks' gatings and the three owner judgings its gate ran"
expect_eq "$(gate '.scope == "mission" and .mission == 2')" '[{"value":null,"n":1,"runs":1,"task_runs":1,"owner_runs":0}]' \
    "(2) mission 2's gate_time: its one task's gating and none of the owner judgings, though the landings moved its baseline too (\`missions\` names it)"
note "(2) checked: the missions"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "061: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "061: the scorecard's gate time counts the owner's judgings beside the tasks', for the fleet and for the mission whose gate judged them"
exit 0
