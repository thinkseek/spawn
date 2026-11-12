#!/bin/sh
# Black box: report to live counts only the landings the live record can
# see, the answer names where its window starts, and the Core lines card
# says "measuring since" until the record covers the whole 7 days.
#
# The problem (lead-mac, 2026-11-12 15:30; objective 081 live): the
# Scorecard's 7-day report to live read 107 h. Every landing from before
# the first `deploy.finished` record counts as going live with the first
# deploy recorded on 11-12, so the 7-day figure measures the record's
# start, not the wait; today alone reads 94 min. The Core lines card (086)
# shows that figure as the performance north star.
#
# Required:
#   (1) the Scorecard's report_to_live counts a landing only if it landed
#       at or after the live record's start: the first `deploy.finished`
#       record of any outcome. An earlier landing is neither live nor
#       not_live. Its parts gain `measured_from_ms`, the later of the
#       window's start and the record's start (null with no deploy record),
#       and `covered`, whether the record starts at or before the window.
#   (2) the Core lines performance north star `performance.report_to_live`
#       has no value (not collected) while the 7-day window is not covered,
#       and carries `measuring_since_ms` (the window's measured_from_ms);
#       web/src/coreLines.ts says "measuring since <MM-DD HH:MM>" (local
#       time) for such a figure in place of "not collected yet". Covered,
#       the figure is the Scorecard's value, as 086 says.
#
# The history: 081's (five landings, deploys at -9 h, a failed one at -7 h,
# -6 h and -4 h), with the first deploy record at -9 h, and task 1 landing
# just before it (-9.5 h). One more task (6) reported at -31 h and landed
# at -30 h. Under 081's rule six landings, five live: median 1.0 h, p90 22 h.
# Under this one tasks 1 and 6 are before the record: three live (2.0 h,
# 1.0 h, 0.8 h), median 1.0 h, p90 2.0 h, task 5 not live.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "095: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test: the gate runs a scenario from the candidate's tree,
# and $controller is the deployed controller's copy, which lacks the
# candidate's web/src until it lands.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep sha1sum cut node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_095_conflicts_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# A scratch HOME and Claude Code projects dir: nothing here reads or writes
# the real ones.
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
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
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "095: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "095: harness: unexpected exit $rc" >&2
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

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done


note() { echo "095: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

B=$(printf 'bb-095-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-095-candidate' | sha1sum | cut -c1-40)
# One "now" for the whole seed, so the intervals are exact to the millisecond.
seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select $seed_now::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;
create function pg_temp.assigned(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', 1,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'brief', 'Task ' || task || ' of 095')) \$\$;
create function pg_temp.reported(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', '$C')) \$\$;
create function pg_temp.gated(h numeric, task int, verdict text, revision int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', verdict,
        'revision', revision, 'base', '$B', 'scenarios', 4, 'candidate', '$C', 'reason', verdict)) \$\$;
create function pg_temp.landed(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.landed', 'id', task, 'from', '$B', 'to', '$C',
        'missions', '[]'::jsonb)) \$\$;
create function pg_temp.deployed(h numeric, ok boolean) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'deploy.finished', 'by', 'landing', 'head', '$C',
        'ok', ok, 'reason', case when ok then '' else 'the build failed' end)) \$\$;

select pg_temp.rec(-48, '{"type":"mission.added","id":1,"text":"095 mission","origin":"human","sources":[]}');
select pg_temp.rec(-48, '{"type":"mission.configured","id":1,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
-- Task 6 lands before the live record begins.
select pg_temp.assigned(-40, 6);
select pg_temp.reported(-31, 6);
select pg_temp.gated(-30.5, 6, 'accepted', 0);
select pg_temp.landed(-30, 6);
select pg_temp.assigned(-11, k) from generate_series(1, 5) k;
select pg_temp.reported(-10, 1);
select pg_temp.gated(-9.6, 1, 'accepted', 0);
select pg_temp.landed(-9.5, 1);
select pg_temp.deployed(-9, true);
select pg_temp.reported(-8.5, 2);
select pg_temp.gated(-8.2, 2, 'rejected', 0);
select pg_temp.reported(-8, 2);
select pg_temp.gated(-7.5, 2, 'accepted', 1);
select pg_temp.landed(-7.4, 2);
select pg_temp.deployed(-7, false);
select pg_temp.deployed(-6, true);
select pg_temp.reported(-5, 3);
select pg_temp.reported(-4.8, 4);
select pg_temp.gated(-4.6, 3, 'accepted', 0);
select pg_temp.landed(-4.5, 3);
select pg_temp.gated(-4.3, 4, 'accepted', 0);
select pg_temp.landed(-4.2, 4);
select pg_temp.deployed(-4, true);
select pg_temp.reported(-2, 5);
select pg_temp.gated(-1.6, 5, 'accepted', 0);
select pg_temp.landed(-1.5, 5);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:landed 2:landed 3:landed 4:landed 5:landed 6:landed" ] || harness "the seed folded to tasks [$tasks]"

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/scorecard" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/scorecard answered $code"
[ "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.lead_time.n]' "$WORK/api.json")" = '[6]' ] ||
    harness "the scorecard does not count the six seeded landings: $(jq -c '[.scopes[] | {scope, mission, lead: .dimensions.lead_time}]' "$WORK/api.json" | head -c 500)"

# --- (1) the Scorecard ---------------------------------------------------------------------

H=3600000
record_from=$((seed_now - 9 * H))
expect_eq "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.report_to_live | {value, p90, n, not_live, measured_from_ms, covered}]' "$WORK/api.json")" \
    "[{\"value\":$H,\"p90\":$((2 * H)),\"n\":3,\"not_live\":1,\"measured_from_ms\":$record_from,\"covered\":false}]" \
    "(1) the 7-day report_to_live: tasks 1 and 6 landed before the first deploy record (-9 h) and are left out; median 1.0 h, p90 2.0 h over three, task 5 not live; measured from the record's start, not covered"
code=$(curl -s --noproxy '*' -o "$WORK/since.json" -w '%{http_code}' "http://127.0.0.1:$port/api/scorecard?since=8h" 2>/dev/null) || code=000
asked=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
if [ "$code" != 200 ]; then
    check_failed "(1) GET /api/scorecard?since=8h answered $code"
else
    expect_eq "$(jq -c '[.scopes[] | select(.scope == "fleet") | .dimensions.report_to_live | {value, n, not_live, covered}]' "$WORK/since.json")" \
        "[{\"value\":$H,\"n\":3,\"not_live\":1,\"covered\":true}]" \
        "(1) an 8-hour window starts after the record: covered, tasks 2, 3 and 4 live, task 5 not"
    from=$(jq -r '[.scopes[] | select(.scope == "fleet") | .dimensions.report_to_live.measured_from_ms][0]' "$WORK/since.json")
    awk -v v="$from" -v lo="$((seed_now - 8 * H))" -v hi="$((asked - 8 * H))" 'BEGIN { exit !(v ~ /^[0-9]+$/ && v >= lo && v <= hi) }' ||
        check_failed "(1) the 8-hour window is measured from its own start: measured_from_ms [$from], between [$((seed_now - 8 * H))] and [$((asked - 8 * H))]"
fi
note "(1) checked: the Scorecard"

# --- (2) the Core lines card -----------------------------------------------------------------

code=$(curl -s --noproxy '*' -o "$WORK/lines.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
if [ "$code" != 200 ]; then
    check_failed "(2) GET /api/core-lines answered $code: $(head -c 200 "$WORK/lines.json")"
else
    expect_eq "$(jq -c '.lines[] | select(.line == "performance") | .north_star | {key, value, state, measuring_since_ms}' "$WORK/lines.json")" \
        "{\"key\":\"performance.report_to_live\",\"value\":null,\"state\":\"not_collected\",\"measuring_since_ms\":$record_from}" \
        "(2) the north star while the 7 days are not covered: no value, measuring since the record's start"
    since_words=$(TZ=Asia/Shanghai date -d "@$((record_from / 1000))" '+%m-%d %H:%M')
    cat >"$WORK/lines.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/lines.json`, "utf8"));

test("(2) the card says measuring since, and the figure once covered", () => {
  const view = coreLinesView(answer);
  const row = view.rows.find((r) => r.line === "Performance");
  assert.equal(row.north_star.value, `measuring since ${env.BB_SINCE}`);
  const star = answer.lines.find((l) => l.line === "performance").north_star;
  const covered = { ...answer, lines: answer.lines.map((l) => l.line === "performance"
    ? { ...l, north_star: { ...star, value: 5_400_000, state: "no_range", measuring_since_ms: undefined } } : l) };
  assert.equal(coreLinesView(covered).rows.find((r) => r.line === "Performance").north_star.value, "1 h 30 min");
});
TEST
    if ! TZ=Asia/Shanghai BB_TREE=$tree BB_WORK=$WORK BB_SINCE="$since_words" node --test "$WORK/lines.test.mjs" >"$WORK/lines.out" 2>&1; then
        check_failed "(2) web/src/coreLines.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/lines.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
    fi
fi
note "(2) checked: the Core lines card"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "095: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "095: report to live counts only what the live record sees, and the card says measuring since until the 7 days are covered"
exit 0
