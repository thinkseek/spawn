#!/bin/sh
# Black box: the Core lines frame's recoverability line. Its guardrails are
# the age of the newest database copy and of the newest bundle off the host
# (objective 082), the age of the oldest nightly judging, and how many
# sources the nightly holds red.
#
# The problem (owner, 2026-11-12 13:30, ranges corrected 13:50;
# sync/roadmap-effects.md, "North-star and guardrail metrics",
# Recoverability): objective 084 built the frame with the recoverability
# line empty. Its guardrails are read today in two places: the backup
# records (082 puts the newest of each kind on /api/fleet) and the nightly's
# fact files (<workspace root>/nightly/<source>.json, written by
# scripts/rsi/nightly.sh: `judged_at`, the last run that judged; `red_since`,
# set while main is red). The north star (critical resources with a restore
# passed in 7 days) waits for the drills' records and stays not collected.
# Monitoring only: no gate, integrator or deploy reads any of it.
#
# Required (read through `GET /api/core-lines`, and web/src/coreLines.ts
# under node):
#   (1) the recoverability line: north star `recoverability.restores_passed`
#       (not collected), guardrails in this order
#       `recoverability.cloud_dump_age` (ms: the answer's moment less the
#       newest `backup.dumped`'s at_ms, as /api/fleet's health.backups.dump),
#       `recoverability.cloud_bundle_age` (ms: the same for the newest
#       `backup.bundled`), `recoverability.nightly_age` (ms: the answer's
#       moment less the oldest `judged_at` among the fact files
#       <workspace root>/nightly/*.json; files of other names, such as the
#       scenario logs beside them, are not facts), `recoverability.nightly_red`
#       (count: the fact files whose `red_since` is set). Each is null
#       without its record or files. State and range as for every figure.
#   (2) coreLinesView over the answer gives the row its tones.
#
# The history: database copies 1.5 h and 0.5 h before "now", one bundle 30 h
# before; two fact files, spawn (green, judged 3 h ago) and supply (red,
# judged 30 h ago), and a scenario log beside them.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "087: harness: $*" >&2
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
SPAWN_DB_NAME="bb_087_recoverability_$run"
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
        echo "087: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "087: harness: unexpected exit $rc" >&2
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


note() { echo "087: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

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
-- An upload as backup.rs records it (spawn_backup_record).
create function pg_temp.dumped(h numeric, key text, bytes bigint) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'backup.dumped', 'keys', jsonb_build_array(key),
        'bytes', bytes, 'stored_bytes', bytes, 'written_bytes', bytes, 'tiers', '["hourly"]'::jsonb,
        'sha256', repeat('a', 64), 'events', 100, 'max_event_id', 100, 'skipped', '[]'::jsonb,
        'dump_ms', 1500, 'upload_ms', 3000, 'duration_ms', 6000,
        'held_bytes', 1000000, 'alert_bytes', 8000000000, 'limit_bytes', 10000000000, 'over_alert', false)) \$\$;
create function pg_temp.bundled(h numeric, key text, bytes bigint) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'backup.bundled', 'kind', 'incremental',
        'keys', jsonb_build_array(key), 'bytes', bytes, 'base_event_id', 50, 'repos', '[]'::jsonb,
        'missing', '[]'::jsonb, 'duration_ms', 10000,
        'held_bytes', 1000000, 'alert_bytes', 8000000000, 'limit_bytes', 10000000000, 'over_alert', false)) \$\$;
select pg_temp.dumped(-1.5, 'postgres/hourly/spawn-bb-1.dump.gpg', 9000000);
select pg_temp.dumped(-0.5, 'postgres/hourly/spawn-bb-2.dump.gpg', 9400000);
select pg_temp.bundled(-30, 'bundles/incr/spawn-bundle-bb-1.tar.zst.gpg', 5000000);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
dump_at=$((seed_now - 1800000))
bundle_at=$((seed_now - 108000000))
# The nightly's fact files, as scripts/rsi/nightly.sh writes them.
facts=$SPAWN_WORKSPACE_ROOT/nightly
mkdir -p "$facts"
spawn_judged=$((seed_now - 10800000))
supply_judged=$((seed_now - 108000000))
printf '{"v":1,"main":"aaaa","at":%s,"state":"green","failed":[],"last_green":"aaaa","red_since":null,"flaky":[],"skipped":[],"gates_beside":0,"skipped_runs":0,"judged_at":%s}\n' \
    "$spawn_judged" "$spawn_judged" >"$facts/spawn.json"
printf '{"v":1,"main":"bbbb","at":%s,"state":"red","failed":["040.sh"],"last_green":null,"red_since":"bbbb","flaky":[],"skipped":[],"gates_beside":0,"skipped_runs":0,"judged_at":%s}\n' \
    "$supply_judged" "$supply_judged" >"$facts/supply.json"
printf 'a scenario log, not a fact\n' >"$facts/supply.040.sh.log"
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {
  "recoverability.cloud_dump_age": {"max": 7200000},
  "recoverability.cloud_bundle_age": {"max": 93600000},
  "recoverability.nightly_age": {"max": 93600000},
  "recoverability.nightly_red": {"max": 0}
}}
JSON

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/core-lines" 2>/dev/null) || code=000
asked=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
if [ "$code" != 200 ]; then
    check_failed "(1) GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"
    printf '{}' >"$WORK/api.json"
else
    rec() { jq -c ".lines[] | select(.line == \"recoverability\") | $1" "$WORK/api.json"; }
    figure() { rec "([.north_star] + .guardrails)[] | select(.key == \"$1\")"; }
    expect_eq "$(rec '[.north_star.key] + [.guardrails[].key]')" \
        '["recoverability.restores_passed","recoverability.cloud_dump_age","recoverability.cloud_bundle_age","recoverability.nightly_age","recoverability.nightly_red"]' \
        "(1) the recoverability line's north star and guardrails, in order"
    expect_eq "$(figure recoverability.restores_passed | jq -c '{value, state}')" '{"value":null,"state":"not_collected"}' \
        "(1) the north star waits for the drills' records"
    # An age is read at the answer: at least seed_now less the record's
    # moment, at most the moment the answer was back less it.
    age() { # <key> <at_ms> <range> <state> <what>
        got=$(figure "$1")
        expect_eq "$(printf '%s' "$got" | jq -c '{unit, range, state}')" "{\"unit\":\"ms\",\"range\":$3,\"state\":\"$4\"}" "$5"
        value=$(printf '%s' "$got" | jq -r '.value')
        awk -v v="$value" -v lo="$((seed_now - $2))" -v hi="$((asked - $2))" 'BEGIN { exit !(v ~ /^[0-9]+$/ && v >= lo && v <= hi) }' ||
            check_failed "$5: value [$value], between [$((seed_now - $2))] and [$((asked - $2))]"
    }
    age recoverability.cloud_dump_age "$dump_at" '{"max":7200000}' in "(1) the newest database copy, 0.5 h old, in its range of 2 h or less"
    age recoverability.cloud_bundle_age "$bundle_at" '{"max":93600000}' out "(1) the newest bundle, 30 h old, out of its range of 26 h or less"
    age recoverability.nightly_age "$supply_judged" '{"max":93600000}' out "(1) the oldest nightly judging (supply, 30 h), out of its range of 26 h or less; the log beside it is no fact"
    expect_eq "$(figure recoverability.nightly_red | jq -c '{unit, value, range, state}')" \
        '{"unit":"count","value":1,"range":{"max":0},"state":"out"}' "(1) one source held red (supply), out of its range of none"
    note "(1) checked: the recoverability line"
fi

# --- (2) the card ---------------------------------------------------------------------------

cat >"$WORK/lines.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));

test("(2) the recoverability row over the answer", () => {
  assert.ok(Array.isArray(answer.lines), "GET /api/core-lines answered");
  const row = coreLinesView(answer).rows.find((r) => r.line === "Recoverability");
  assert.deepEqual(row.guardrails.map((mark) => [mark.range, mark.tone]), [
    ["2 h or less", "plain"], ["26 h or less", "warning"], ["26 h or less", "warning"], ["0 or less", "warning"],
  ]);
  assert.equal(row.north_star.value, "not collected yet");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/lines.test.mjs" >"$WORK/lines.out" 2>&1; then
    check_failed "(2) web/src/coreLines.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/lines.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(2) checked: the card"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "087: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "087: the Core lines recoverability line gives the cloud backup ages and the nightly's age and reds"
exit 0
