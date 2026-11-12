#!/bin/sh
# Black box: while how long the disk lasts cannot be measured yet, the
# storage card says from when it will be.
#
# The problem (rev-g N3 on 085, the lead 2026-11-12; lead-mac's 11:35
# question): since objective 085, how long the disk lasts runs on the growth
# of the archive, Claude Code's transcripts and the database, each measured
# against the oldest sample of the last 7 days that carries it, at least a
# day old. Samples before 085 carry no database size, so after the deploy
# "How long the disk lasts" is blank for about a day, and the card says
# "not measured yet" as if nothing were coming. Objective 083 says when the
# archive's own growth comes; nothing says when days left does.
#
# Required (read through `GET /api/fleet`, and web/src/resources.ts under
# node):
#   (1) `health.storage.persistent_from_ms`: the moment the persistent growth
#       is first measured, the latest of its three parts' (each the at_ms of
#       the oldest sample of the last 7 days carrying it, plus one day); null
#       while any part has no such sample.
#   (2) resources.ts `storageView(storage)` carries `lasts_from`: while
#       `days_left` is null and persistent_from_ms is after the storage's
#       read_at_ms, the local date and time of persistent_from_ms (as
#       "2026-11-13 07:31"); otherwise null. `lasts` is unchanged. The card's
#       last line says "How long the disk lasts: measured from <lasts_from>"
#       (read off a screenshot).
#
# The history: a sample 30 h old in the old shape (archive and projects) and
# one 10 h old with the database's size too, so the archive's and the
# projects' growth are measured and the database's comes 14 h from now.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "091: harness: $*" >&2
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
SPAWN_DB_NAME="bb_091_lasts_from_$run"
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
        echo "091: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "091: harness: unexpected exit $rc" >&2
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


note() { echo "091: $*" >&2; }
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
-- A sample as the loop records it (storage.rs, spawn_host_record).
select pg_temp.rec(-30, jsonb_build_object('type', 'host.storage', 'archive_bytes', 1048576,
    'archive_files', 1, 'projects_bytes', 0, 'projects_files', 0));
select pg_temp.rec(-10, jsonb_build_object('type', 'host.storage', 'archive_bytes', 2097152,
    'archive_files', 1, 'projects_bytes', 0, 'projects_files', 0, 'database_bytes', 1048576));
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
growth_from=$((seed_now - 36000000 + 86400000))
persistent_from=$((seed_now - 36000000 + 86400000))
# The archive the sizes are read from: 3 MiB.
mkdir -p "$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a"
head -c 3145728 /dev/zero >"$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a/a1.jsonl"

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

expect_eq "$(jq -c '.health.storage | {days_left, persistent_growth_bytes_per_day, persistent_from_ms}' "$WORK/api.json")" \
    "{\"days_left\":null,\"persistent_growth_bytes_per_day\":null,\"persistent_from_ms\":$persistent_from}" \
    "(1) health.storage: no days left yet; the persistent growth is first measured when the database's oldest sample (10 h ago) is a day old"
note "(1) checked: the answer"

# --- (2) the page's model ------------------------------------------------------------------

cat >"$WORK/lasts.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const storage = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8")).health.storage;
// 2026-11-12 23:31:20 UTC is 2026-11-13 07:31 in Shanghai (this test runs with TZ=Asia/Shanghai).
const at = Date.UTC(2026, 10, 12, 23, 31, 20);

test("(2) over the answer: from when the disk's days are measured", () => {
  assert.equal(typeof storage.persistent_from_ms, "number", "the answer carries persistent_from_ms");
  const view = storageView(storage);
  const local = new Date(storage.persistent_from_ms);
  const pad = (n) => String(n).padStart(2, "0");
  const want = `${local.getFullYear()}-${pad(local.getMonth() + 1)}-${pad(local.getDate())} ${pad(local.getHours())}:${pad(local.getMinutes())}`;
  assert.deepEqual([view.lasts, view.lasts_from], [null, want]);
});

test("(2) the local date and time, to the minute; nothing once measured or already past", () => {
  const pending = storageView({ ...storage, read_at_ms: at - 3_600_000, days_left: null, persistent_from_ms: at });
  assert.deepEqual([pending.lasts, pending.lasts_from], [null, "2026-11-13 07:31"]);
  const measured = storageView({ ...storage, read_at_ms: at + 3_600_000, days_left: 40, persistent_from_ms: at });
  assert.equal(measured.lasts_from, null);
  assert.ok(typeof measured.lasts === "string" && measured.lasts.startsWith("about 40 days at "), `lasts: ${measured.lasts}`);
  assert.equal(storageView({ ...storage, read_at_ms: at + 3_600_000, days_left: null, persistent_from_ms: at }).lasts_from, null);
  assert.equal(storageView({ ...storage, days_left: null, persistent_from_ms: null }).lasts_from, null);
});
TEST
if ! TZ=Asia/Shanghai BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/lasts.test.mjs" >"$WORK/lasts.out" 2>&1; then
    check_failed "(2) web/src/resources.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/lasts.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(2) checked: the page's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "091: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "091: the storage card says from when how long the disk lasts is measured"
exit 0
