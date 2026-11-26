#!/bin/sh
# Objective 103 [observability] (A22's 100b): the Fleet page's human
# interventions per agent-hour is the Home card's. /api/fleet's
# health.human_per_agent_hour counted a person's commands only (the
# decisions ladder's human tier), so a mission resumed or a task a person
# assigned moved the Home card (/api/interventions?window=24h) and not the
# Fleet page's strip, under the same name. With 100 (Core lines) this
# completes A22's one definition; 102 adds a pause to the kinds.
#
# Black box, through `GET /api/interventions?window=24h` and `GET
# /api/fleet` on a seeded history: one agent's execution from 9 h to 2 h
# ago (7 agent-hours in the last 24 h); mission 1 resumed by a person 5 h
# ago (mission.resumed); a task a person assigned 4 h ago (task.assigned,
# created_by human); no command in the last 24 h, and one person's command
# 30 h ago (in 7 days, not in the day).
#   (1) /api/interventions?window=24h counts them: total 2, resume 1,
#       owner_task 1, command 0, per_agent_hour 0.286 (2 over 7 agent-hours);
#       over 7 days 3 and 0.429. A different answer is the seed's fault:
#       harness.
#   (2) /api/fleet's health.human_per_agent_hour is that same
#       per_agent_hour, 0.286 (it read 0: no command in the day), over the
#       same 7 agent hours (agent_hours_24h 7).
#   (3) The decisions ladder is unchanged: health.ladder_24h.human stays 0
#       (no person's command in the day: a resume or an assigned task is an
#       intervention, not a decision of the ladder).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "103: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep sha1sum cut; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_103_fleet_interventions_$run"
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
        echo "103: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "103: harness: unexpected exit $rc" >&2
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


note() { echo "103: $*" >&2; }
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
-- Agent time as the Bridge records it (049): one execution from 9 h to 2 h
-- ago, 7 agent-hours in the last 24 h.
create function pg_temp.bridge(h numeric, name text, detail jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_w_1', 'session_worker_w_1', 'exec_bb_103_w', 'bridge', null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) \$\$;
select pg_temp.bridge(-9, 'execution.started', '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.bridge(-2, 'execution.ended', '{"cause":"exited","exit_code":0}');
-- A person's interventions that are not commands (049's record shapes).
select pg_temp.rec(-5, '{"type":"mission.resumed","id":1}');
select pg_temp.rec(-4, '{"type":"task.assigned","id":7,"mission_id":1,"kind":"ephemeral","agent_id":"agent_worker_w_7","session_id":"session_worker_w_7","brief":"a person asked for this","created_by":"human","objective":null,"model":"default"}');
insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                            succeeded, error, created_at_ms, finished_at_ms)
values ('human', 'board-1', 'agent_worker_w_1', 'session_worker_w_1',
        '{"type":"input","text":"a note"}', 'finished', true, null, pg_temp.ms(-30), pg_temp.ms(-30) + 1000);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
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
get() { # <path> <file>: the answer's code
    bb_http "$2" --noproxy '*' "http://127.0.0.1:$port$1"
}
code=$(get "/api/interventions?window=24h" "$WORK/interventions.json")
[ "$code" = 200 ] || harness "GET /api/interventions?window=24h answered $code"
week=$(get "/api/interventions?window=7d" "$WORK/interventions-7d.json")
[ "$week" = 200 ] || harness "GET /api/interventions?window=7d answered $week"
[ "$(jq -c '{total, per_agent_hour}' "$WORK/interventions-7d.json")" = '{"total":3,"per_agent_hour":0.429}' ] ||
    harness "(1) the seed: /api/interventions?window=7d does not count the command 30 h ago too: $(jq -c '{total, per_agent_hour}' "$WORK/interventions-7d.json")"
card=$(jq -c '{total, by_kind: (.by_kind | {command, resume, owner_task}), per_agent_hour}' "$WORK/interventions.json")
[ "$card" = '{"total":2,"by_kind":{"command":0,"resume":1,"owner_task":1},"per_agent_hour":0.286}' ] ||
    harness "(1) the seed: /api/interventions?window=24h does not count one resume and one owner task over 7 agent-hours: $card"
note "(1) checked: the Home card's figure is 0.286 (a resume and a person's task over 7 agent-hours)"

code=$(get /api/fleet "$WORK/api.json")
if [ "$code" != 200 ]; then
    check_failed "(2) GET /api/fleet answered $code: $(head -c 200 "$WORK/api.json")"
else
    got=$(jq -c '.health | {human_per_agent_hour, agent_hours_24h}' "$WORK/api.json")
    expect_eq "$got" '{"human_per_agent_hour":0.286,"agent_hours_24h":7}' \
        "(2) /api/fleet's human interventions per agent-hour is the Home card's (/api/interventions?window=24h per_agent_hour 0.286)"
    note "(2) checked: the Fleet page reads the Home card's figure"
    expect_eq "$(jq -c '.health.ladder_24h.human' "$WORK/api.json")" 0 \
        "(3) the decisions ladder's human tier (a person's commands in the day: none)"
    note "(3) checked: the ladder is unchanged"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "103: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "103: the Fleet page's human interventions per agent-hour is the Home card's, resumes and a person's tasks included"
exit 0
