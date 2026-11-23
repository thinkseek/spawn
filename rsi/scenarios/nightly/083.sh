#!/bin/sh
# Black box: while the archive's growth cannot be measured yet, the storage
# card says from when it will be, instead of "not measured yet".
#
# The problem (owner via lead-mac, 2026-11-12 11:35 and 13:50;
# sync/roadmap-effects.md, "North-star and guardrail metrics", Storage):
# the storage north star is the archive's growth a day and how long the disk
# lasts at it (objective 065). Both need one `host.storage` sample at least
# a day old (crates/spawn-cli/src/serve/storage.rs, GROWTH_MIN_MS), so on a
# fresh install, or after the samples start, the card says "not measured
# yet" for a day, and the reader cannot tell a figure that is coming from
# one that is broken. The samples say when it comes: the oldest of the last
# 7 days, plus a day.
#
# Required (read through `GET /api/fleet`, and web/src/resources.ts under
# node):
#   (1) `health.storage.growth_from_ms` is the moment the growth is first
#       measured: the at_ms of the oldest `host.storage` sample of the last
#       7 days (the one the growth is measured from), plus one day; null
#       when there is no such sample.
#   (2) resources.ts `storageView(storage)`: while `growth_bytes_per_day`
#       is null and `growth_from_ms` is after the storage's `read_at_ms`,
#       the "Transcript archive" row's note is "measured from <the local
#       date and time of growth_from_ms, as 2026-11-13 07:31>", and the view
#       carries that date and time as `measured_from` (`lasts` stays null).
#       Otherwise `measured_from` is null and the notes are as today ("grows
#       about <size> a day", or "not measured yet"). The page says "How long
#       the disk lasts: measured from <it>" under the rows (read off a
#       screenshot: no check here reads the page's source).
#
# The history is seeded as the store holds it (control records, hours
# before one fixed "now"); `spawn serve` answers. Two samples, 10 h and 2 h
# old: the growth is first measured 14 h from now.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "083: harness: $*" >&2
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
SPAWN_DB_NAME="bb_083_growth_from_$run"
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
        echo "083: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "083: harness: unexpected exit $rc" >&2
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


note() { echo "083: $*" >&2; }
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
create function pg_temp.sample(h numeric, archive bigint) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'host.storage', 'archive_bytes', archive,
        'archive_files', 1, 'projects_bytes', 0, 'projects_files', 0)) \$\$;
select pg_temp.sample(-10, 1048576);
select pg_temp.sample(-2, 2097152);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
growth_from=$((seed_now - 36000000 + 86400000))
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

expect_eq "$(jq -c '.health.storage | {growth_bytes_per_day, growth_from_ms}' "$WORK/api.json")" \
    "{\"growth_bytes_per_day\":null,\"growth_from_ms\":$growth_from}" \
    "(1) health.storage: no growth yet, and growth_from_ms is the oldest sample (10 h ago) plus a day"
note "(1) checked: the answer"

# --- (2) the page's model ------------------------------------------------------------------

cat >"$WORK/storage.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { storageView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/resources.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const storage = answer.health.storage;
const archive = (view) => view.rows.find((row) => row.name === "Transcript archive");
// 2026-11-12 23:31:20 UTC is 2026-11-13 07:31 in Shanghai (this test runs with TZ=Asia/Shanghai).
const at = Date.UTC(2026, 10, 12, 23, 31, 20);
const MIB = 1024 ** 2;

test("(2) over the answer: the archive's row says from when its growth is measured", () => {
  assert.equal(typeof storage.growth_from_ms, "number", "the answer carries growth_from_ms");
  const view = storageView(storage);
  const local = new Date(storage.growth_from_ms);
  const pad = (n) => String(n).padStart(2, "0");
  const want = `${local.getFullYear()}-${pad(local.getMonth() + 1)}-${pad(local.getDate())} ${pad(local.getHours())}:${pad(local.getMinutes())}`;
  assert.deepEqual([archive(view).note, view.measured_from, view.lasts], [`measured from ${want}`, want, null]);
});

test("(2) the local date and time, to the minute", () => {
  const view = storageView({ ...storage, read_at_ms: at - 3_600_000, growth_bytes_per_day: null, days_left: null, growth_from_ms: at });
  assert.deepEqual([archive(view).note, view.measured_from, view.lasts], ["measured from 2026-11-13 07:31", "2026-11-13 07:31", null]);
});

test("(2) once measured, or with no sample, as before", () => {
  const grown = storageView({ ...storage, read_at_ms: at + 3_600_000, growth_bytes_per_day: 5 * MIB, days_left: 40, growth_from_ms: at });
  assert.deepEqual([archive(grown).note, grown.measured_from], ["grows about 5.0 MB a day", null]);
  // The words after the days are 065's and change with objective 085 (the
  // growth of every persistent grower): only the days are this check's.
  assert.ok(typeof grown.lasts === "string" && grown.lasts.startsWith("about 40 days at "), `lasts: ${grown.lasts}`);
  const bare = storageView({ ...storage, growth_bytes_per_day: null, days_left: null, growth_from_ms: null });
  assert.deepEqual([archive(bare).note, bare.measured_from, bare.lasts], ["not measured yet", null, null]);
  const past = storageView({ ...storage, read_at_ms: at + 3_600_000, growth_bytes_per_day: null, days_left: null, growth_from_ms: at });
  assert.deepEqual([archive(past).note, past.measured_from], ["not measured yet", null], "a moment already past promises nothing");
});
TEST
if ! TZ=Asia/Shanghai BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/storage.test.mjs" >"$WORK/storage.out" 2>&1; then
    check_failed "(2) web/src/resources.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/storage.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(2) checked: the page's model"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "083: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "083: the storage card says from when the growth is measured"
exit 0
