#!/bin/sh
# Black box: Needs you says how long a mission has been without work as of
# now, not as of when its alert fired.
#
# The problem, on the live fleet (read-only /api/alerts and Home's needsYou
# under node over the live answers, 2026-11-12 01:17 +08; rev-b's 058 live
# check, lead 01:25): mission #6's `mission_idle` alert fired at 11-11
# 16:54Z, and its text says "mission #6 has had no task in work for 21 h 25
# min since its last task ended". Home's Needs you shows that text as it
# is (web/src/home.ts needsYou: an alert's detail is its `text`), so it
# says "21 h 25 min" an hour later, a day later, for as long as the alert
# is open. The alert's evidence carries `since_ms`, when the mission's
# last task ended.
#
# Required (read through `GET /api/alerts` and web/src/home.ts's needsYou
# under node over it):
#   (1) a `mission_idle` item's detail is "<subject> has had no task in
#       work for <how long> since its last task ended: its objective
#       backlog has nothing open to assign", <how long> being now less the
#       evidence's `since_ms` in the page's words (home.ts ageText: "25
#       min", "31 h", "3 d"). An alert whose evidence has no numeric
#       `since_ms` keeps its text.
#
# The history is seeded as the store holds it (control records folded by
# spawn_control_refold(); the alert as the loop's alert step writes it,
# 15 min after the mission's last task ended); `spawn serve` answers.
# Mission 1 takes its work from a backlog; its one task was cancelled 31 h
# ago. Each check is reported; the run fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no tmux, no
# network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "071: harness: $*" >&2
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_071_idlefor_$run"
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
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "071: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "071: harness: unexpected exit $rc" >&2
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


note() { echo "071: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}



# --- the history -------------------------------------------------------------------------

ctl=$WORK/ctl
mkdir -p "$ctl/rsi/backlog" "$ctl/rsi/scenarios"
printf '{"id": 1, "title": "Objective 1 of 071", "prompt": "Make objective 1 pass."}\n' >"$ctl/rsi/backlog/001.json"
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;

select pg_temp.rec(-40, '{"type":"mission.added","id":1,"text":"071 objectives","origin":"human","sources":[]}');
select pg_temp.rec(-40, jsonb_build_object('type', 'mission.configured', 'id', 1, 'accept', 'gate',
    'gate', '$ctl/rsi/scenarios',
    'objectives', jsonb_build_object('backlog', '$ctl/rsi/backlog', 'resident', 'rsi', 'workers', 1)));
select pg_temp.rec(-35, jsonb_build_object('type', 'task.assigned', 'id', 1, 'mission_id', 1,
    'objective', 1, 'kind', 'ephemeral', 'created_by', 'spawn-integrate',
    'agent_id', 'agent_worker_objective_1', 'session_id', 'session_worker_objective_1',
    'scenario', '$ctl/rsi/scenarios/objective-1', 'brief', 'Objective 1 of 071'));
select pg_temp.rec(-31, '{"type":"task.cancelled","id":1,"by":"human","reason":"071: the last task ends"}');
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
ended=$(scalar "select id || ' ' || recorded_at_ms from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'")
end_id=${ended% *}
since_ms=${ended#* }
case "$end_id$since_ms" in '' | *[!0-9]*) harness "no task.cancelled record was seeded: [$ended]" ;; esac
[ "$(scalar "select state from mission_tasks where id = 1")" = cancelled ] || harness "task 1 did not fold to cancelled"

# The alert as the alert step fires it (crates/spawn-cli/src/alerts.rs,
# mission_idle) 15 min after the task ended, its words then.
fired_h=$(awk -v s="$since_ms" -v now="$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")" \
    'BEGIN { printf "%.6f", (s + 900000 - now) / 3600000 }')
cat >"$WORK/alert.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 200000;
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                          kind, session_seq, recorded_at_ms)
values ('agent_control', 'session_control', null, 'control', null,
        jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
            jsonb_build_object('type', 'alert.fired', 'key', 'mission_idle:1:$end_id', 'kind', 'mission_idle',
                'subject', 'mission #1', 'notice', false,
                'text', 'mission #1 has had no task in work for 15 min since its last task ended: its objective backlog has nothing open to assign',
                'evidence', jsonb_build_object('mission', 1, 'since_ms', $since_ms, 'event_id', $end_id,
                    'idle_ms', 900000, 'reading_ms', $since_ms + 900000)))),
        nextval('seed_seq'), pg_temp.ms($fired_h));
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/alert.sql"
[ "$status" -eq 0 ] || harness "seeding the alert failed: $(err)"

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
for path in alerts missions; do
    code=$(curl -s --noproxy '*' -o "$WORK/$path.json" -w '%{http_code}' "http://127.0.0.1:$port/api/$path" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/$path answered $code"
done
cp "$WORK/alerts.json" "$WORK/api.json"
[ "$(jq -c --argjson s "$since_ms" '[.[] | select(.kind == "mission_idle") | [.subject, .state, .evidence.since_ms == $s, (.text | test("for 15 min since"))]]' "$WORK/alerts.json")" = '[["mission #1","open",true,true]]' ] ||
    harness "/api/alerts does not hold the seeded mission_idle alert: $(jq -c '[.[] | {kind, subject, state, evidence}]' "$WORK/alerts.json" | head -c 600)"

# --- (1) the item's words, as of now -----------------------------------------------------

cat >"$WORK/idle.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { needsYou } = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const alerts = JSON.parse(readFileSync(`${env.BB_WORK}/alerts.json`, "utf8"));
const missions = JSON.parse(readFileSync(`${env.BB_WORK}/missions.json`, "utf8"));
const since = Number(env.BB_SINCE_MS);
const words = (how) =>
  `mission #1 has had no task in work for ${how} since its last task ended: its objective backlog has nothing open to assign`;
const detail = (list, now) => {
  const items = needsYou({
    attention: null, missions, alerts: list, agents: [], health: null, missingAlwaysOn: 0, now, nameOf: () => null,
  }).filter((item) => item.key.startsWith("alert:mission_idle:"));
  assert.equal(items.length, 1, `one mission_idle item: ${JSON.stringify(items)}`);
  return items[0].detail;
};

test("(1) how long the mission has been without work, as of now", () => {
  assert.equal(detail(alerts, since + 31 * 3_600_000), words("31 h"));
  assert.equal(detail(alerts, since + 72 * 3_600_000), words("3 d"));
  assert.equal(detail(alerts, since + 25 * 60_000), words("25 min"));
});

test("(1) an alert without a numeric since_ms keeps its text", () => {
  const without = alerts.map((alert) =>
    alert.kind === "mission_idle" ? { ...alert, evidence: { ...alert.evidence, since_ms: null } } : alert,
  );
  assert.equal(detail(without, since + 31 * 3_600_000), words("15 min"));
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_SINCE_MS=$since_ms node --test "$WORK/idle.test.mjs" >"$WORK/idle.out" 2>&1; then
    check_failed "(1) web/src/home.ts needsYou over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/idle.out" | head -n 12 | tr '\n' ' ' | head -c 1200)"
fi
note "(1) checked: the item's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "071: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "071: Needs you says how long a mission has been without work as of now, from its alert's since_ms"
exit 0
