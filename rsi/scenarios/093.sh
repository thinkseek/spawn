#!/bin/sh
# Black box: the recoverability north star, critical resources with a
# restore that passed in the last 7 days, from the drills the host really
# runs; with the age of the newest session backup and of the oldest passed
# drill.
#
# The problem (owner, 2026-11-12 13:30; sync/roadmap-effects.md,
# Recoverability; sync/recovery.md): the north star is read by hand from
# recovery.md ("5 of 6; configuration and credentials has no drill"). The
# drills exist but report in three places: the database drill writes
# `backup.verified` records Spawn reads; the session backups' restore drill
# is a root systemd unit (spawn-session-backup-drill.service) whose result
# only systemd holds; the session backups themselves write
# /var/backups/spawn-sessions/status.json (`at`, epoch seconds, 0644).
# Which resources count is the recovery table's, so the rows go in the
# Core lines file beside the ranges. Monitoring only: no gate, integrator
# or deploy reads any of it.
#
# Required (read through `GET /api/core-lines`):
#   (1) core-lines.json's `recoverability` {drills: [{name, record?: <type>,
#       unit?: <name>.service}], session_backup_status?: <path>}. A row's
#       last pass: `record` "backup.verified", the newest such record with
#       ok true; `unit`, read without root through `$SPAWN_SYSTEMCTL show
#       <unit> -p Result,ExecMainStatus,ExecMainExitTimestamp
#       --timestamp=unix -- <unit>` (default systemctl, its arguments as argv,
#       the unit after --, the call bounded in time): passed when Result is
#       success and ExecMainStatus 0, at its exit timestamp; a unit name
#       other than [A-Za-z0-9][A-Za-z0-9@._-]*.service (no leading
#       "-", which systemctl would read as an option: -H<host> is SSH to a
#       host) is never run, and serve's log names it as refused; neither:
#       no drill. The answer
#       lists `drills` [{name, source ("record <type>", "unit <name>", or
#       null), passed_at_ms}] in the file's order.
#   (2) the figures: north star `recoverability.restores_passed` (count: the
#       rows whose last pass is within 7 days; null with no rows; since
#       objective 105, a row whose newest drill failed is not counted);
#       `recoverability.session_backup_age` (ms: the answer's moment less the
#       status file's `at`; null without one); `recoverability.oldest_drill_age`
#       (ms: the answer's moment less the oldest of the rows' last passes,
#       rows that never passed left out; null with none).
#       Since objective 176 (the owner's core metric set) the north star is
#       guardrail U1 of the sustainability line, and the two ages are alert
#       figures (`alert_figures`, line sustainability).
#
# The history: a passing database drill 3 h ago and a failed one 1 h ago;
# a fake systemctl with three units: one passed 10 h ago, one failed, one
# passed 8 days ago; three names it must never be called with (a shell
# command, "-Hbb.invalid.service" and "--host=bb.invalid.service"), and a
# timer, which has no ExecMain* result and so is refused too; a
# configuration row with no drill;
# a status file 10 minutes old.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "093: harness: $*" >&2
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
SPAWN_DB_NAME="bb_093_drills_$run"
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
        echo "093: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "093: harness: unexpected exit $rc" >&2
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


note() { echo "093: $*" >&2; }
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
-- A drill as backup.rs records it (record_verified).
create function pg_temp.verified(h numeric, ok boolean) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'backup.verified', 'ok', ok,
        'reason', case when ok then '' else 'the restore failed' end, 'key', 'postgres/hourly/spawn-bb.dump.gpg',
        'sha256', repeat('a', 64), 'dumped_event_id', 1, 'expected', null, 'events', 100, 'max_event_id', 100,
        'database', 'bb_drill', 'server', null, 'bundle', null, 'duration_ms', 5000)) \$\$;
select pg_temp.verified(-3, true);
select pg_temp.verified(-1, false);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
verified_at=$((seed_now - 10800000))
unit_a_s=$(((seed_now - 36000000) / 1000))
unit_c_s=$(((seed_now - 8 * 86400000) / 1000))
mkdir -p "$WORK/bin"
cat >"$WORK/bin/systemctl" <<EOF
#!/bin/sh
# A fake systemctl, first on PATH and named by SPAWN_SYSTEMCTL: every call's arguments are logged, one per line; the
# unit is the last argument (after --, wherever the options are).
printf '%s\n' "\$*" >>"$WORK/systemctl.calls"
for a; do unit=\$a; done
case "\$unit" in
bb-drill-a.service) printf 'Result=success\nExecMainStatus=0\nExecMainExitTimestamp=@$unit_a_s\n' ;;
bb-drill-b.service) printf 'Result=exit-code\nExecMainStatus=1\nExecMainExitTimestamp=@$unit_a_s\n' ;;
bb-drill-c.service) printf 'Result=success\nExecMainStatus=0\nExecMainExitTimestamp=@$unit_c_s\n' ;;
*) printf 'Result=success\nExecMainStatus=0\nExecMainExitTimestamp=@$unit_a_s\n' ;;
esac
EOF
chmod 755 "$WORK/bin/systemctl"
SPAWN_SYSTEMCTL=$WORK/bin/systemctl
PATH=$WORK/bin:$PATH
export SPAWN_SYSTEMCTL PATH
status_s=$(((seed_now - 600000) / 1000))
printf '{"at":%s,"accounts":[]}\n' "$status_s" >"$WORK/status.json"
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {},
 "recoverability": {
   "session_backup_status": "$WORK/status.json",
   "drills": [
     {"name": "Database", "record": "backup.verified"},
     {"name": "Session files", "unit": "bb-drill-a.service"},
     {"name": "Off-host copy", "unit": "bb-drill-b.service"},
     {"name": "Old drill", "unit": "bb-drill-c.service"},
     {"name": "Not a unit", "unit": "bb drill; touch $WORK/ran"},
     {"name": "An option", "unit": "-Hbb.invalid.service"},
     {"name": "A long option", "unit": "--host=bb.invalid.service"},
     {"name": "A timer", "unit": "bb-drill-a.timer"},
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
asked=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
if [ "$code" != 200 ]; then
    check_failed "(1) GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"
else
    expect_eq "$(jq -c '.drills | map({name, source, passed_at_ms})' "$WORK/api.json")" \
        "[{\"name\":\"Database\",\"source\":\"record backup.verified\",\"passed_at_ms\":$verified_at},{\"name\":\"Session files\",\"source\":\"unit bb-drill-a.service\",\"passed_at_ms\":$((unit_a_s * 1000))},{\"name\":\"Off-host copy\",\"source\":\"unit bb-drill-b.service\",\"passed_at_ms\":null},{\"name\":\"Old drill\",\"source\":\"unit bb-drill-c.service\",\"passed_at_ms\":$((unit_c_s * 1000))},{\"name\":\"Not a unit\",\"source\":\"unit bb drill; touch $WORK/ran\",\"passed_at_ms\":null},{\"name\":\"An option\",\"source\":\"unit -Hbb.invalid.service\",\"passed_at_ms\":null},{\"name\":\"A long option\",\"source\":\"unit --host=bb.invalid.service\",\"passed_at_ms\":null},{\"name\":\"A timer\",\"source\":\"unit bb-drill-a.timer\",\"passed_at_ms\":null},{\"name\":\"Configuration and credentials\",\"source\":null,\"passed_at_ms\":null}]" \
        "(1) the drills, in the file's order: the database's newest passing record (not the failed one after it), two units passed, one failed, a name never run, no drill"
    [ ! -e "$WORK/ran" ] || check_failed "(1) a unit name that is a shell command ran"
    if grep -qE 'bb drill|bb\.invalid' "$WORK/systemctl.calls" 2>/dev/null; then
        check_failed "(1) a refused unit name reached systemctl: $(grep -E 'bb drill|bb\.invalid' "$WORK/systemctl.calls" | head -n 2 | tr '\n' ' ' | head -c 300)"
    fi
    expect_eq "$(grep -c . "$WORK/systemctl.calls" 2>/dev/null || echo 0)" "$(grep -c -- '-- bb-drill-' "$WORK/systemctl.calls" 2>/dev/null || echo 0)" \
        "(1) every systemctl call names its unit after --"
    expect_eq "$(sed -n 's/.* -- //p' "$WORK/systemctl.calls" | sort -u | tr '\n' ' ')" "bb-drill-a.service bb-drill-b.service bb-drill-c.service " \
        "(1) systemctl is asked for the three well-named units only"
    for refused in "-Hbb.invalid.service" "--host=bb.invalid.service" "bb drill; touch" "bb-drill-a.timer"; do
        grep -qF -- "$refused" "$WORK/serve.log" ||
            check_failed "(1) serve's log does not name the refused unit [$refused]"
    done
    # Objective 176: restores passed is sustainability's U1; the two ages are alert figures.
    rec() { jq -c "((.lines[] | select(.line == \"sustainability\") | ([.north_star] + .guardrails)[]), (.alert_figures // [])[]) | select(.key == \"$1\")" "$WORK/api.json"; }
    expect_eq "$(rec recoverability.restores_passed | jq -c '{unit, value}')" '{"unit":"count","value":1}' \
        "(2) restores passed in 7 days: the session files (the old drill is 8 days old; since 105 the database's newest drill, failed 1 h ago, is not counted)"
    between() { # <key> <at_ms> <what>
        value=$(rec "$1" | jq -r '.value')
        awk -v v="$value" -v lo="$((seed_now - $2))" -v hi="$((asked - $2))" 'BEGIN { exit !(v ~ /^[0-9]+$/ && v >= lo && v <= hi) }' ||
            check_failed "$3: value [$value], between [$((seed_now - $2))] and [$((asked - $2))]"
    }
    between recoverability.session_backup_age "$((status_s * 1000))" "(2) the newest session backup's age, from the status file (10 min)"
    between recoverability.oldest_drill_age "$((unit_c_s * 1000))" "(2) the oldest passed drill's age (the old drill, 8 days)"
    note "(1) and (2) checked"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "093: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "093: the recoverability north star counts the drills that passed in 7 days, with the session backup and oldest drill ages"
exit 0
