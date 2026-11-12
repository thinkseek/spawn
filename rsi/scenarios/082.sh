#!/bin/sh
# Black box: the page says how old the newest off-host backups are: the
# database's hourly copy and the daily bundle of transcripts and sources.
#
# The problem (owner, 2026-11-12 13:30; sync/roadmap-effects.md,
# "North-star and guardrail metrics", Recoverability): "age of the newest
# cloud backup" is a guardrail, and no figure says it ("Not read today; to
# be collected"). The record has it: every upload is a `backup.dumped` (the
# database, hourly) or a `backup.bundled` (transcripts and sources, daily),
# with its time and its keys. The alert step reads them for `backup_stale`
# (crates/spawn-cli/src/alerts.rs: a dump older than 2 h, a bundle older
# than 26 h), but no answer carries them, so the page cannot show how old
# they are until something is already late.
#
# Required (read through `GET /api/fleet`, and web/src/resources.ts under
# node over it):
#   (1) `health.backups` is {dump, bundle}: each the newest record of its
#       kind, {at_ms, key, bytes} (`key` its first uploaded key, `bytes`
#       its record's bytes), or null when there is none.
#   (2) resources.ts exports `backupView(backups, now)`: rows
#       [{name, age_ms, key}] for "Database copy" (the dump) and
#       "Transcripts and sources" (the bundle), in that order, `age_ms` now
#       less its at_ms; a kind with no record is a row with age_ms null and
#       key null. The page shows them on the storage card (read off a
#       screenshot: no check here reads the page's source).
#
# The history is seeded as the store holds it (control records, hours
# before one fixed "now"); `spawn serve` answers. Dumps at -1.5 h and
# -0.5 h; one bundle at -10 h.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "082: harness: $*" >&2
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
SPAWN_DB_NAME="bb_082_conflicts_$run"
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
        echo "082: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "082: harness: unexpected exit $rc" >&2
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


note() { echo "082: $*" >&2; }
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
select pg_temp.bundled(-10, 'bundles/incr/spawn-bundle-bb-1.tar.zst.gpg', 5000000);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
dump_at=$((seed_now - 1800000))
bundle_at=$((seed_now - 36000000))

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/fleet" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/fleet answered $code"
[ "$(jq -c '.health | type' "$WORK/api.json")" = '"object"' ] || harness "the answer has no health: $(head -c 300 "$WORK/api.json")"

# --- (1) the answer ----------------------------------------------------------------------

expect_eq "$(jq -c '.health.backups.dump | if . == null then null else {at_ms, key, bytes} end' "$WORK/api.json")" \
    "{\"at_ms\":$dump_at,\"key\":\"postgres/hourly/spawn-bb-2.dump.gpg\",\"bytes\":9400000}" \
    "(1) health.backups.dump: the newest of the two dumps (0.5 h ago)"
expect_eq "$(jq -c '.health.backups.bundle | if . == null then null else {at_ms, key, bytes} end' "$WORK/api.json")" \
    "{\"at_ms\":$bundle_at,\"key\":\"bundles/incr/spawn-bundle-bb-1.tar.zst.gpg\",\"bytes\":5000000}" \
    "(1) health.backups.bundle: the one bundle (10 h ago)"
note "(1) checked: the answer"

# --- (2) the page's model ------------------------------------------------------------------

cat >"$WORK/backups.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const resources = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const backups = answer.health.backups;
const now = Number(env.BB_NOW);

test("(2) backupView: the database copy and the bundle, with their ages", () => {
  assert.equal(typeof resources.backupView, "function", "web/src/resources.ts exports backupView");
  assert.deepEqual(resources.backupView(backups, now), [
    { name: "Database copy", age_ms: 1_800_000, key: "postgres/hourly/spawn-bb-2.dump.gpg" },
    { name: "Transcripts and sources", age_ms: 36_000_000, key: "bundles/incr/spawn-bundle-bb-1.tar.zst.gpg" },
  ]);
  assert.deepEqual(resources.backupView({ ...backups, bundle: null }, now)[1],
    { name: "Transcripts and sources", age_ms: null, key: null }, "a kind with no record");
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK BB_NOW=$seed_now node --test "$WORK/backups.test.mjs" >"$WORK/backups.out" 2>&1; then
    check_failed "(2) web/src/resources.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/backups.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(2) checked: the page's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "082: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "082: the page says how old the newest database copy and bundle are"
exit 0
