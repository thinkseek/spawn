#!/bin/sh
# Black box: an alert, and a storage line, when a database outgrows what
# the live event log justifies: the live database, or a test database the
# loop knows (the gates', or the fleet's, which tests leave schemas in).
#
# The problem (the lead's read-only SELECTs on the live cluster, 2026-11-12;
# queue.md 11-11 09:37; analysis-storage-rev-b.md): the fleet's test
# database (spawn_fleet_test, the database the loop hands the engines from
# SPAWN_FLEET_TEST_DATABASE_URL) had grown to 2.7 GB. It held 2533 schemas
# that crate tests created and never dropped. Nothing measured it and
# nothing alerted; it was found by hand on the host's disk and first read
# as live-database bloat. The live database is 159 MB for a 35 MB event log
# (spawn_events.kind). P19 stops that leak at its source; nothing says when
# it, or any other cause, makes a database outgrow its data again.
#
# Required (read through `GET /api/alerts`, `GET /api/fleet` and
# web/src/resources.ts's storageView under node over it):
#   (1) the loop's alert step reads, every SPAWN_ALERT_DATABASE_SECS
#       (default 600), the live database's size and its event log's bytes
#       (the sum of spawn_events.kind's stored size), and the size and
#       schemas (other than pg_*, information_schema and public) of each
#       test database it is given: the gates' (SPAWN_TEST_DATABASE_URL, role
#       "test") and the engines' (SPAWN_FLEET_TEST_DATABASE_URL, role
#       "fleet_test"), each when set.
#       A database at or above SPAWN_ALERT_DATABASE_FLOOR_MB (default
#       1024) and above SPAWN_ALERT_DATABASE_RATIO (default 10) times the
#       log's bytes opens one `database_size` alert, subject "database
#       <name>", evidence {database, role ("live", "test" or "fleet_test"), bytes,
#       log_bytes, ratio (bytes over log_bytes, one decimal), schemas}; its
#       text names the database, its size, the ratio and "<n> schemas".
#       One open per database.
#   (2) it clears at the first reading where the database no longer
#       qualifies, its cleared evidence that reading.
#   (3) /api/fleet's `health.storage` carries `test_databases`: [{role,
#       name, bytes, schemas}], the gates' first, each only when set, and
#       `database.log_bytes`; storageView adds a row "Gate test database"
#       and a row "Fleet test database", each with note "<n> schemas", and
#       the "Database" row's note says the log's size ("log <size>").
#
# A real `spawn loop` and `spawn serve` run on a scratch database (the
# live one), with two more scratch databases: the gates' test database
# (empty) and the fleet's (40 padded schemas, above both the floor and the
# ratio). The live database holds about 2 MB of log and is above the floor
# (set from its own size) but within the ratio. Then 35 of the fleet test
# database's schemas are dropped. Each check is reported; the run
# fails if any failed.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "074: harness: $*" >&2
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_074_dbsize_$run"
SPAWN_TMUX_SOCKET="bb-074-$run"
SPAWN_PSQL=psql
TEST_DB="bb_074_testdb_$run"
FLEET_DB="bb_074_fleetdb_$run"
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
# The tmux server's socket lives in WORK, so the cleanup removes nothing
# outside it (cc-rules (a), 2026-11-12).
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$TMUX_TMPDIR"
export TMUX_TMPDIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
test_db_made=""
fleet_db_made=""
loop_pid=""
serve_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$test_db_made" ]; then
        psql -d postgres -tAq -c "drop database if exists $TEST_DB" >/dev/null 2>&1 || true
    fi
    if [ -n "$fleet_db_made" ]; then
        psql -d postgres -tAq -c "drop database if exists $FLEET_DB" >/dev/null 2>&1 || true
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
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "074: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "074: the database_size alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'database_size' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "074: harness: unexpected exit $rc" >&2
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
# A scratch HOME: nothing here reads or writes the real one.
real_home=$HOME
HOME=$WORK/home
mkdir -p "$HOME" "$SPAWN_CLAUDE_PROJECTS"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# The database reading under test, every 2 s (the floor is set below,
# from the live database's own size).
SPAWN_ALERT_DATABASE_SECS=2
SPAWN_ALERT_DATABASE_RATIO=10
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
export HOME SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_DATABASE_SECS SPAWN_ALERT_DATABASE_RATIO SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
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
note() { echo "074: $*" >&2; }
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
alerts_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/alerts.json" -w '%{http_code}' "$base/api/alerts" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/alerts answered $code: $(head -c 300 "$WORK/alerts.json" 2>/dev/null)"
}

# --- the live log and the test database ------------------------------------------------

# About 2 MB of live log: owner-work missions, each with a long text that
# does not compress away (nothing acts on an owner-work mission with no
# task).
cat >"$WORK/pad.sql" <<'SQL'
\set ON_ERROR_STOP 1
begin;
create temp sequence seed_seq start 100000;
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                          kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
           jsonb_build_object('type', 'mission.added', 'id', m, 'origin', 'human', 'sources', '[]'::jsonb,
               'text', '074 owner work ' || m || ' ' ||
                   (select string_agg(md5(m::text || '-' || k::text), '') from generate_series(1, 600) k)))),
       nextval('seed_seq'), (floor(extract(epoch from clock_timestamp()) * 1000))::bigint - 3600000
  from generate_series(10, 109) m;
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/pad.sql"
[ "$status" -eq 0 ] || harness "seeding the live log failed: $(err)"

psql -d postgres -tAq -c "create database $TEST_DB" >/dev/null 2>&1 || harness "cannot create the gates' test database"
test_db_made=1
psql -d postgres -tAq -c "create database $FLEET_DB" >/dev/null 2>&1 || harness "cannot create the fleet test database"
fleet_db_made=1
# 40 schemas of about 1.1 MB each in the fleet's, as tests leave them.
cat >"$WORK/left.sql" <<'SQL'
\set ON_ERROR_STOP 1
do $$ begin
    for s in 1..40 loop
        execute format('create schema bb_left_%s', s);
        execute format('create table bb_left_%s.pad (b text)', s);
        execute format($f$insert into bb_left_%s.pad
                          select string_agg(md5(g::text || '-' || k::text), '')
                            from generate_series(1, 1000) g, generate_series(1, 32) k group by g$f$, s);
    end loop;
end $$;
SQL
capture psql -d "$FLEET_DB" -q -f "$WORK/left.sql"
[ "$status" -eq 0 ] || harness "padding the fleet test database failed: $(err)"

live_bytes=$(q "select pg_database_size(current_database())")
log_bytes=$(q "select sum(pg_column_size(kind)) from spawn_events")
test_bytes=$(psql -d "$FLEET_DB" -tA -c "select pg_database_size(current_database())")
# The floor: 1 MB under the live database, so it is above the floor and
# only the ratio keeps it quiet.
floor_mb=$(((live_bytes / 1048576) - 1))
[ "$floor_mb" -ge 1 ] || harness "the live database is too small: $live_bytes"
[ "$live_bytes" -lt $((10 * log_bytes)) ] ||
    harness "the live database ($live_bytes) is not within 10 times its log ($log_bytes): the seed is too small"
[ "$test_bytes" -gt $((10 * log_bytes + 10485760)) ] ||
    harness "the fleet test database ($test_bytes) is not well above 10 times the log ($log_bytes)"
SPAWN_ALERT_DATABASE_FLOOR_MB=$floor_mb
export SPAWN_ALERT_DATABASE_FLOOR_MB
note "live $live_bytes B, log $log_bytes B, fleet test $test_bytes B, floor $floor_mb MB"
# The loop and serve are given the two test databases as theirs; this
# script reaches the server through the PG* variables pg_url_export set
# above.
SPAWN_TEST_DATABASE_URL="$server_url/$TEST_DB"
SPAWN_FLEET_TEST_DATABASE_URL="$server_url/$FLEET_DB"
export SPAWN_TEST_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve

alert_of() { # <database>: its database_size alerts in /api/alerts, any state
    alerts_json
    jq -c --arg s "database $1" '[.[] | select(.kind == "database_size" and .subject == $s)]' "$WORK/alerts.json"
}
open_for_fleet() { [ "$(alert_of "$FLEET_DB" | jq '[.[] | select(.state == "open")] | length')" = 1 ]; }

# --- (1) the fleet test database opens one alert; the others do not -----------------------

if wait_until 40 open_for_fleet; then
    expect_eq "$(alert_of "$FLEET_DB" | jq -c --argjson floor "$((floor_mb * 1048576))" --argjson log "$log_bytes" --arg db "$FLEET_DB" '[.[] | .evidence as $e | {
        database: $e.database, role: $e.role, schemas: $e.schemas,
        above_floor: ($e.bytes >= $floor), above_ratio: ($e.bytes > 10 * $e.log_bytes),
        ratio: ($e.ratio == ((($e.bytes / $e.log_bytes) * 10 | round) / 10)),
        log_from_live: ($e.log_bytes >= $log),
        text: ((.text | contains($db)) and (.text | contains("40 schemas")))}]')" \
        "[{\"database\":\"$FLEET_DB\",\"role\":\"fleet_test\",\"schemas\":40,\"above_floor\":true,\"above_ratio\":true,\"ratio\":true,\"log_from_live\":true,\"text\":true}]" \
        "(1) the fleet test database's database_size alert: its evidence, and a text naming it and its 40 schemas"
else
    check_failed "(1) the fleet test database ($test_bytes B, 40 schemas, over 10 times the live log's $log_bytes B and over the $floor_mb MB floor) has no open database_size alert 40 s after the loop started: $(alert_of "$FLEET_DB" | head -c 400)"
fi
expect_eq "$(alert_of "$TEST_DB" | jq -c 'length')" 0 \
    "(1) no database_size alert for the gates' test database (under the floor)"
expect_eq "$(alert_of "$SPAWN_DB_NAME" | jq -c 'length')" 0 \
    "(1) no database_size alert for the live database (above the floor, within 10 times its log)"
note "(1) checked: the alert"

# --- (3) the storage line --------------------------------------------------------------------

code=$(curl -s --noproxy '*' -o "$WORK/fleet.json" -w '%{http_code}' "$base/api/fleet" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/fleet answered $code"
expect_eq "$(jq -c '.health.storage | {tests: [.test_databases[]? | {role, name, schemas, sized: (.bytes > 0)}], log: (.database.log_bytes | type)}' "$WORK/fleet.json" 2>/dev/null)" \
    "{\"tests\":[{\"role\":\"test\",\"name\":\"$TEST_DB\",\"schemas\":0,\"sized\":true},{\"role\":\"fleet_test\",\"name\":\"$FLEET_DB\",\"schemas\":40,\"sized\":true}],\"log\":\"number\"}" \
    "(3) /api/fleet health.storage.test_databases (the gates', then the fleet's) and database.log_bytes"
cat >"$WORK/storage.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const fleet = JSON.parse(readFileSync(`${env.BB_WORK}/fleet.json`, "utf8"));

test("(3) the storage rows name the test databases' schemas and the log's size", () => {
  const rows = storageView(fleet.health.storage).rows;
  const gateRow = rows.find((row) => row.name === "Gate test database");
  const fleetRow = rows.find((row) => row.name === "Fleet test database");
  assert.ok(gateRow && fleetRow, `the two test database rows: ${JSON.stringify(rows)}`);
  assert.equal(gateRow.note, "0 schemas");
  assert.equal(fleetRow.note, "40 schemas");
  const database = rows.find((row) => row.name === "Database");
  assert.match(database?.note ?? "", /log \d+(\.\d)? MB/);
  const without = storageView({ ...fleet.health.storage, test_databases: [] }).rows;
  assert.equal(without.find((row) => /test database/.test(row.name)), undefined, "no row without a test database");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/storage.test.mjs" >"$WORK/storage.out" 2>&1; then
    check_failed "(3) web/src/resources.ts storageView over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/storage.out" | head -n 10 | tr '\n' ' ' | head -c 900)"
fi
note "(3) checked: the storage line"

# --- (2) the leftovers dropped, it clears ------------------------------------------------------

cat >"$WORK/drop.sql" <<'SQL'
\set ON_ERROR_STOP 1
do $$ begin
    for s in 6..40 loop execute format('drop schema bb_left_%s cascade', s); end loop;
end $$;
SQL
capture psql -d "$FLEET_DB" -q -f "$WORK/drop.sql"
[ "$status" -eq 0 ] || harness "dropping the fleet test database's schemas failed: $(err)"
after_bytes=$(psql -d "$FLEET_DB" -tA -c "select pg_database_size(current_database())")
[ "$after_bytes" -lt $((10 * log_bytes)) ] || harness "the fleet test database ($after_bytes) is still over 10 times the log ($log_bytes) after the drop"
cleared() { [ "$(alert_of "$FLEET_DB" | jq -c '[.[] | .state]')" = '["cleared"]' ]; }
if wait_until 40 cleared; then
    expect_eq "$(alert_of "$FLEET_DB" | jq -c '[.[] | .cleared_evidence | {schemas, within: (.bytes <= 10 * .log_bytes)}]')" \
        '[{"schemas":5,"within":true}]' \
        "(2) the alert's cleared evidence: the reading that cleared it (5 schemas left, within 10 times the log)"
else
    check_failed "(2) the fleet test database's alert did not clear within 40 s of dropping 35 of its 40 schemas ($after_bytes B now): $(alert_of "$FLEET_DB" | head -c 400)"
fi
note "(2) checked: the clearing"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "074: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "074: a database that outgrows the live log opens a database_size alert and clears when it no longer does; storage names the test databases' schemas and the log's size"
exit 0
