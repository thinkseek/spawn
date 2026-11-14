#!/bin/sh
# Objective 105 [recoverability, observability] (part 1 of 105/105b): a
# drill row says how its newest drill went, not only its last pass; the
# card (105b) follows.
#
# The problem (lead, 2026-11-13, made visible by 104): a Core lines drill
# row carries only its last pass (serve/core_lines/drills.rs: the newest
# record of its type with ok true, or a unit's last success), so a drill
# that failed yesterday after passing two days ago still reads as passed
# for up to 7 days, the north star counts it, and nothing on the card says
# it failed.
#
# Required (read through `GET /api/core-lines`):
#   (1) each `drills` row gains `last`: its newest drill's own result,
#       {at_ms, ok, reason}, or null when it has none (no drill, or a record
#       type with no record). A `record` row: the newest control record of
#       that type whatever its ok: at_ms its time, ok true exactly when its
#       `ok` is true, reason its `reason` when that is a string, else "". A
#       `unit` row: the unit's last run, from the same `systemctl show` the
#       pass is read from (no other call): ok true exactly when Result is
#       success and ExecMainStatus 0, at_ms its ExecMainExitTimestamp (null
#       when it has none), reason "" when ok, else "Result=<Result>,
#       ExecMainStatus=<status>". `passed_at_ms` is unchanged.
#   (2) the north star `recoverability.restores_passed` counts a row only
#       when its last pass is within 7 days and its newest drill did not
#       fail (`last` null or ok true); the oldest-passed-drill age is
#       unchanged.
#
# The history (seeded control records and a fake systemctl; times from the
# database's clock): Database (backup.verified) passed 3 h ago and failed
# 1 h ago; Transcript archive (backup.drilled.transcripts) passed 5 h ago;
# Mission repositories (backup.drilled.missions) no record; Session files
# (unit, passed 10 h ago); Off-host copy (unit, failed: Result=exit-code,
# status 1, 10 h ago); Old drill (unit, passed 8 days ago); Configuration
# and credentials (no drill). Under 093's rule the north star reads 3
# (Database, Transcript archive, Session files); under 105's, 2.
#
# Polled-read cost: a record row adds one read per Core lines reading, the
# newest control record of its type (any ok), beside NEWEST_PASSED; with
# rev-f's index for control records of a type, one indexed lookup. A unit
# row adds no call: the last run comes from the same `systemctl show`.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "105: harness: $*" >&2
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
SPAWN_DB_NAME="bb_105_drills_$run"
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
        echo "105: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "105: harness: unexpected exit $rc" >&2
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


note() { echo "105: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
-- The store's own append lock, held to commit, as every writer of
-- session_control takes it.
select pg_advisory_xact_lock(1399957335);
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select $seed_now::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;
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

# --- (1) each row's newest drill ---------------------------------------------------------------
got=$(jq -c '[.drills[] | {name, passed_at_ms, last}]' "$WORK/api.json")
a_ms=$((unit_a_s * 1000))
c_ms=$((unit_c_s * 1000))
want="[{\"name\":\"Database\",\"passed_at_ms\":$db_pass,\"last\":{\"at_ms\":$db_fail,\"ok\":false,\"reason\":\"the restore failed: pg_restore exited 1\"}},"
want="$want{\"name\":\"Transcript archive\",\"passed_at_ms\":$tr_pass,\"last\":{\"at_ms\":$tr_pass,\"ok\":true,\"reason\":\"\"}},"
want="$want{\"name\":\"Mission repositories\",\"passed_at_ms\":null,\"last\":null},"
want="$want{\"name\":\"Session files\",\"passed_at_ms\":$a_ms,\"last\":{\"at_ms\":$a_ms,\"ok\":true,\"reason\":\"\"}},"
want="$want{\"name\":\"Off-host copy\",\"passed_at_ms\":null,\"last\":{\"at_ms\":$a_ms,\"ok\":false,\"reason\":\"Result=exit-code, ExecMainStatus=1\"}},"
want="$want{\"name\":\"Old drill\",\"passed_at_ms\":$c_ms,\"last\":{\"at_ms\":$c_ms,\"ok\":true,\"reason\":\"\"}},"
want="$want{\"name\":\"Configuration and credentials\",\"passed_at_ms\":null,\"last\":null}]"
expect_eq "$got" "$want" "(1) the drills rows with their newest drill"
calls=$(grep -c . "$WORK/systemctl.calls" 2>/dev/null || true)
expect_eq "$calls" 3 "(1) systemctl calls for one reading (one per unit row: the last run comes from the same call)"
note "(1) checked: each row's newest drill"

# --- (2) the north star --------------------------------------------------------------------------
# Objective 176: restores passed is guardrail U1 of the sustainability line.
got=$(jq -c '.lines[] | select(.line == "sustainability") | .guardrails[] | select(.key == "recoverability.restores_passed") | {key, value}' "$WORK/api.json")
expect_eq "$got" '{"key":"recoverability.restores_passed","value":2}' \
    "(2) the north star counts only the rows passed in 7 days whose newest drill did not fail (Transcript archive, Session files)"
note "(2) checked: the north star"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "105: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "105: each drill row says how its newest drill went, and a failed one is not counted"
exit 0
