#!/bin/sh
# Objective 112 [recoverability, observability] (part 2 of 105): the
# Core lines card says a drill row whose newest drill failed. After 105,
# whose API gives each drills row `last` {at_ms, ok, reason}; this
# scenario's harness requires it (exit 2 without it).
#
# Required (read through `GET /api/core-lines` and web/src/coreLines.ts):
#   (1) web/src/coreLines.ts: a row whose newest drill failed reads
#       "failed MM-DD HH:MM (<span> ago); last passed MM-DD HH:MM (<span>
#       ago)", or "...; never passed" without a pass ("failed (time
#       unknown); ..." without a time), tone warning, and its mark carries
#       the reason as `detail` (the card shows it as that row's title); a
#       row whose newest drill passed reads as before ("passed ..."), with
#       `detail` "".
#
# The history: 105's (seeded control records and a fake systemctl):
# Database (backup.verified) passed 3 h ago and failed 1 h ago; Off-host
# copy (unit) failed 10 h ago and never passed; Transcript archive passed
# 5 h ago; Mission repositories no record; Configuration and credentials
# no drill.
#
# Polled-read cost: none (the card reads the answer it already polls).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "112: harness: $*" >&2
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
SPAWN_DB_NAME="bb_112_drills_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "112: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "112: harness: unexpected exit $rc" >&2
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


note() { echo "112: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
-- session_control's L3 append lock, held to commit, as every writer of
-- session_control takes it.
select pg_advisory_xact_lock(1399957401, hashtext('session_control'));
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select $seed_now::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms, commit_xid)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h), pg_current_xact_id()) \$\$;
-- A database drill as backup.rs records it (record_verified).
create function pg_temp.verified(h numeric, ok boolean) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'backup.verified', 'ok', ok,
        'reason', case when ok then '' else 'the restore failed: pg_restore exited 1' end,
        'key', 'postgres/hourly/spawn-bb.dump.gpg', 'sha256', repeat('a', 64), 'dumped_event_id', 1,
        'expected', null, 'events', 100, 'max_event_id', 100, 'database', 'bb_drill', 'server', null,
        'bundle', null, 'duration_ms', 5000)) \$\$;
select pg_temp.verified(-3, true);
select pg_temp.verified(-1, false);
-- A restore drill's record (objective 104's shape).
select pg_temp.rec(-5, jsonb_build_object('type', 'backup.drilled.transcripts', 'ok', true, 'reason', '',
    'bundle', 'bundles/full/spawn-bundle-bb.tar.gpg', 'bundle_event_id', 7, 'mismatches', 0, 'duration_ms', 900));
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
H=3600000
db_pass=$((seed_now - 3 * H))
db_fail=$((seed_now - 1 * H))
tr_pass=$((seed_now - 5 * H))
unit_a_s=$(((seed_now - 10 * H) / 1000))
unit_c_s=$(((seed_now - 8 * 86400000) / 1000))
mkdir -p "$WORK/bin"
{
    echo '#!/bin/sh'
    echo '# A fake systemctl, named by SPAWN_SYSTEMCTL: each call logged, the unit its last argument.'
    echo "printf '%s\\n' \"\$*\" >>\"$WORK/systemctl.calls\""
    echo 'for a; do unit=$a; done'
    echo 'case "$unit" in'
    echo "bb-drill-a.service) printf 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@$unit_a_s\\n' ;;"
    echo "bb-drill-b.service) printf 'Result=exit-code\\nExecMainStatus=1\\nExecMainExitTimestamp=@$unit_a_s\\n' ;;"
    echo "bb-drill-c.service) printf 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@$unit_c_s\\n' ;;"
    echo '*) exit 1 ;;'
    echo 'esac'
} >"$WORK/bin/systemctl"
chmod 755 "$WORK/bin/systemctl"
SPAWN_SYSTEMCTL=$WORK/bin/systemctl
export SPAWN_SYSTEMCTL
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {},
 "recoverability": {
   "drills": [
     {"name": "Database", "record": "backup.verified"},
     {"name": "Transcript archive", "record": "backup.drilled.transcripts"},
     {"name": "Mission repositories", "record": "backup.drilled.missions"},
     {"name": "Session files", "unit": "bb-drill-a.service"},
     {"name": "Off-host copy", "unit": "bb-drill-b.service"},
     {"name": "Old drill", "unit": "bb-drill-c.service"},
     {"name": "Configuration and credentials"}
   ]}}
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
[ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"

# --- the API (part 1, 105): every row carries `last` -----------------------------------------
[ "$(jq '[.drills[] | has("last")] | all' "$WORK/api.json")" = true ] ||
    harness "the API has no drills[].last: run on a tree with objective 105 (part 1)"
a_ms=$((unit_a_s * 1000))

# --- (1) the card ----------------------------------------------------------------------------------
cat >"$WORK/card.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));
const now = Number(env.BB_NOW);
const words = (ms) => {
  const d = new Date(ms);
  const p = (n) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
};
const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

test("(1) a row whose newest drill failed says so, with its last pass and its reason", () => {
  const realNow = Date.now;
  Date.now = () => now;
  try {
    const view = coreLinesView(answer, now);
    const mark = (name) => view.drills.find((d) => d.name === name);
    const db = mark("Database");
    assert.match(db.text, new RegExp(`^failed ${esc(words(Number(env.BB_DB_FAIL)))} \\(1 h[^)]*ago\\); last passed ${esc(words(Number(env.BB_DB_PASS)))} \\(3 h[^)]*ago\\)$`));
    assert.equal(db.tone, "warning");
    assert.equal(db.detail, "the restore failed: pg_restore exited 1");
    const off = mark("Off-host copy");
    assert.match(off.text, new RegExp(`^failed ${esc(words(Number(env.BB_UNIT_A)))} \\(10 h[^)]*ago\\); never passed$`));
    assert.equal(off.tone, "warning");
    assert.equal(off.detail, "Result=exit-code, ExecMainStatus=1");
    const tr = mark("Transcript archive");
    assert.match(tr.text, /^passed /);
    assert.equal(tr.tone, "plain");
    assert.equal(tr.detail, "");
    assert.equal(mark("Mission repositories").text, "never passed");
    assert.equal(mark("Configuration and credentials").text, "no drill");
  } finally {
    Date.now = realNow;
  }
});
TEST
if ! TZ=Asia/Shanghai BB_TREE=$tree BB_WORK=$WORK BB_NOW="$seed_now" BB_DB_FAIL="$db_fail" BB_DB_PASS="$db_pass" \
    BB_UNIT_A="$a_ms" node --test "$WORK/card.test.mjs" >"$WORK/card.out" 2>&1; then
    check_failed "(1) web/src/coreLines.ts over the answer: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/card.out" | head -n 12 | tr '\n' ' ' | head -c 1200)"
fi
note "(1) checked: the card"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "112: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "112: the card says a failed newest drill, with its last pass and its reason"
exit 0
