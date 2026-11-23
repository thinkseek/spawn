#!/bin/sh
# Black box: each line of the storage card says from when its own growth
# is measured.
#
# The problem (lead-mac, 2026-11-12 17:20, A5): since 085 each persistent
# grower (the transcript archive, Claude Code's transcripts, the live
# database) is measured from its own oldest sample, and a part is measured
# only once that sample is a day old. 083 dates the archive's growth and
# 091 dates the forecast as a whole ("measured from <latest part>"), but a
# line whose own growth is still pending says nothing: the database's line
# after 085's deploy, or any new grower's later. Each line should carry its
# own date.
#
# Required (through `GET /api/fleet` and web/src/resources.ts under node):
#   (1) `health.storage.growth_from_parts`: for every part of growth_parts,
#       the moment it is first measured: the at_ms of the oldest sample of
#       the last 7 days that carries the part's field, plus one day; null
#       without such a sample (the archive's also without a workspace
#       root). The archive's equals 083's growth_from_ms.
#   (2) storageView: each grower's row ("Transcript archive", "Claude Code
#       transcripts", "Database", and any later part's row) carries
#       `measured_from`: the local date and time (localDateTime) of its
#       part's moment while its growth is null and that moment is after
#       the sizes' read, else null. The rows' size and note are unchanged.
#
# The history: a host.storage sample 30 h old in the old shape (archive and
# projects) and one 10 h old with the database's size too. So the archive's
# and the projects' growth are measured (from 30 h ago, plus a day), the
# database's is not yet (10 h ago plus a day is 14 h ahead).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "098: harness: $*" >&2
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
SPAWN_DB_NAME="bb_098_lasts_from_$run"
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
        echo "098: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "098: harness: unexpected exit $rc" >&2
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


note() { echo "098: $*" >&2; }
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
from30=$((seed_now - 108000000 + 86400000))
from10=$((seed_now - 36000000 + 86400000))
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

expect_eq "$(jq -c '.health.storage.growth_from_parts | {archive, projects, database}' "$WORK/api.json")" \
    "{\"archive\":$from30,\"projects\":$from30,\"database\":$from10}" \
    "(1) growth_from_parts: the archive and the projects from the 30 h sample plus a day, the database from the 10 h sample plus a day"
expect_eq "$(jq -c '.health.storage | (.growth_from_parts.archive == .growth_from_ms)' "$WORK/api.json")" true \
    "(1) the archive's moment is 083's growth_from_ms"
expect_eq "$(jq -c '.health.storage.growth_parts | {archive: (.archive != null), projects: (.projects != null), database}' "$WORK/api.json")" \
    '{"archive":true,"projects":true,"database":null}' "(1) the seed: the archive's and the projects' growth measured, the database's not yet"
note "(1) checked: the answer"

# --- (2) the card's lines --------------------------------------------------------------------

cat >"$WORK/lines.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const storage = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8")).health.storage;
const pad = (n) => String(n).padStart(2, "0");
const local = (ms) => { const d = new Date(ms); return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`; };
const row = (view, name) => view.rows.find((r) => r.name === name);
// 2026-11-12 23:31:20 UTC is 2026-11-13 07:31 in Shanghai (TZ=Asia/Shanghai).
const at = Date.UTC(2026, 10, 12, 23, 31, 20);

test("(2) over the answer: the database's line is dated, the measured lines are not", () => {
  const view = storageView(storage);
  assert.equal(row(view, "Transcript archive").measured_from, null, "the archive's growth is measured");
  assert.equal(row(view, "Claude Code transcripts").measured_from, null, "the projects' growth is measured");
  assert.equal(row(view, "Database").measured_from, local(storage.growth_from_parts.database));
});

test("(2) every pending line its own date; none once measured or already past", () => {
  const parts = { archive: at, projects: at + 60_000, database: at + 120_000 };
  const pending = storageView({ ...storage, read_at_ms: at - 3_600_000, growth_bytes_per_day: null,
    growth_parts: { archive: null, projects: null, database: null }, growth_from_ms: at, growth_from_parts: parts });
  assert.deepEqual(["Transcript archive", "Claude Code transcripts", "Database"].map((n) => row(pending, n).measured_from),
    ["2026-11-13 07:31", "2026-11-13 07:32", "2026-11-13 07:33"]);
  const past = storageView({ ...storage, read_at_ms: at + 3_600_000, growth_bytes_per_day: null,
    growth_parts: { archive: null, projects: null, database: null }, growth_from_ms: at, growth_from_parts: parts });
  assert.deepEqual(["Transcript archive", "Claude Code transcripts", "Database"].map((n) => row(past, n).measured_from), [null, null, null]);
  const none = storageView({ ...storage, growth_parts: { archive: null, projects: null, database: null },
    growth_from_parts: { archive: null, projects: null, database: null }, growth_from_ms: null });
  assert.deepEqual(["Transcript archive", "Claude Code transcripts", "Database"].map((n) => row(none, n).measured_from), [null, null, null]);
});
TEST
if ! TZ=Asia/Shanghai BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/lines.test.mjs" >"$WORK/lines.out" 2>&1; then
    check_failed "(2) web/src/resources.ts storageView: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/lines.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(2) checked: the card's lines"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "098: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "098: each line of the storage card says from when its own growth is measured"
exit 0
