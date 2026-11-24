#!/bin/sh
# Black box: the open-alerts banner at the top of every page says how long
# a mission has been without work as of now, as Home's Needs you does.
#
# The problem, as the owner reads the live pages (lead-mac, sync/queue.md
# 1503 and 1631; sync/home-2026-11-12-1044.png). 071 is verified on Home:
# Needs you reads "mission #6 has had no task in work for 31 h", matching
# the Missions card. The red banner at the top of the other pages (Show at
# 10:44) still reads "mission #6 has had no task in work for 21 h 25 min",
# the words of the moment the alert fired, beside a "since 5h ago" that
# moves. The banner (web/src/components/alerts/AlertsView.tsx
# `OpenAlerts`) prints the newest open alert's stored `text`, and so does
# each row of the Alerts page. Only Home goes through web/src/home.ts
# `alertWords(alert, now)` (objective 071).
#
# Required (read through `GET /api/alerts`, and web/src/home.ts under node
# over it):
#   (1) home.ts exports `openAlertsLine(alerts, now)`: null for no alert;
#       else {count, text, fired_at_ms}: how many alerts there are, and
#       the newest one's (by fired_at_ms) words as of `now` (alertWords)
#       and the time it fired.
#   (2) a `mission_idle` alert's words count on ("31 h", then "3 d"); an
#       alert of any other kind keeps its own text.
# The banner (open alerts only) renders from openAlertsLine, and the Alerts
# page's open rows say alertWords(alert, now); a cleared or notice row keeps
# its stored text (its mission is no longer without work, so its time must
# not count on). Both are read off a screenshot (no check here reads the
# page's source).
#
# The history is seeded as the store holds it (071's way: control records
# folded by spawn_control_refold(); the alert as the loop's alert step
# writes it, 15 min after the mission's last task ended); `spawn serve`
# answers. Mission 1 takes its work from a backlog; its one task was
# cancelled 31 h ago.
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
    echo "080: harness: $*" >&2
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
SPAWN_DB_NAME="bb_080_banner_$run"
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
        echo "080: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "080: harness: unexpected exit $rc" >&2
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


note() { echo "080: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}



# --- the history -------------------------------------------------------------------------

ctl=$WORK/ctl
mkdir -p "$ctl/rsi/backlog" "$ctl/rsi/scenarios"
printf '{"id": 1, "title": "Objective 1 of 080", "prompt": "Make objective 1 pass."}\n' >"$ctl/rsi/backlog/001.json"
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

select pg_temp.rec(-40, '{"type":"mission.added","id":1,"text":"080 objectives","origin":"human","sources":[]}');
select pg_temp.rec(-40, jsonb_build_object('type', 'mission.configured', 'id', 1, 'accept', 'gate',
    'gate', '$ctl/rsi/scenarios',
    'objectives', jsonb_build_object('backlog', '$ctl/rsi/backlog', 'resident', 'rsi', 'workers', 1)));
select pg_temp.rec(-35, jsonb_build_object('type', 'task.assigned', 'id', 1, 'mission_id', 1,
    'objective', 1, 'kind', 'ephemeral', 'created_by', 'spawn-integrate',
    'agent_id', 'agent_worker_objective_1', 'session_id', 'session_worker_objective_1',
    'scenario', '$ctl/rsi/scenarios/objective-1', 'brief', 'Objective 1 of 080'));
select pg_temp.rec(-31, '{"type":"task.cancelled","id":1,"by":"human","reason":"080: the last task ends"}');
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

# --- (1), (2) the banner's line, as of now --------------------------------------------------

cat >"$WORK/banner.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const home = await import(pathToFileURL(`${env.BB_TREE}/web/src/home.ts`).href);
const alerts = JSON.parse(readFileSync(`${env.BB_WORK}/alerts.json`, "utf8")).filter((alert) => alert.state === "open");
const since = Number(env.BB_SINCE_MS);
const idle = alerts.find((alert) => alert.kind === "mission_idle");
const words = (how) =>
  `mission #1 has had no task in work for ${how} since its last task ended: its objective backlog has nothing open to assign`;

test("(1) openAlertsLine: how many, and the newest one's words as of now", () => {
  assert.equal(typeof home.openAlertsLine, "function", "web/src/home.ts exports openAlertsLine");
  assert.deepEqual(home.openAlertsLine(alerts, since + 31 * 3_600_000),
    { count: 1, text: words("31 h"), fired_at_ms: idle.fired_at_ms });
  assert.equal(home.openAlertsLine([], since), null, "no alert, no line");
});

test("(2) the mission's time counts on; another kind keeps its own words", () => {
  assert.equal(home.openAlertsLine(alerts, since + 72 * 3_600_000).text, words("3 d"));
  const other = { ...idle, key: "other:1", kind: "host_disk", subject: "host",
    text: "the disk has 3 GB free", evidence: { since_ms: since }, fired_at_ms: idle.fired_at_ms + 60_000 };
  assert.deepEqual(home.openAlertsLine([idle, other], since + 31 * 3_600_000),
    { count: 2, text: "the disk has 3 GB free", fired_at_ms: other.fired_at_ms }, "a newer alert of another kind: its own text");
  const older = { ...other, fired_at_ms: idle.fired_at_ms - 60_000 };
  assert.deepEqual(home.openAlertsLine([older, idle], since + 31 * 3_600_000),
    { count: 2, text: words("31 h"), fired_at_ms: idle.fired_at_ms }, "the mission_idle alert, newest of two");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_SINCE_MS=$since_ms node --test "$WORK/banner.test.mjs" >"$WORK/banner.out" 2>&1; then
    check_failed "(1)/(2) web/src/home.ts openAlertsLine over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/banner.out" | head -n 12 | tr '\n' ' ' | head -c 1200)"
fi
note "(1), (2) checked: the banner's line"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "080: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "080: the open-alerts banner says a mission's time without work as of now, from its alert's since_ms"
exit 0
