#!/bin/sh
# Objective 102 [observability], a pause is an intervention (A22, part 3 of
# 100/103/102): the owner's hold on a mission (`mission.paused`, which only
# the owner may record) counts as a human intervention, and releasing it
# (`mission.unpaused`) as a resume, in the one definition the Home card,
# the Fleet page and Core lines now share (100, 103): all three move with
# it, and stay equal.
#
# Black box, through `GET /api/interventions?window=24h|7d`, `GET
# /api/fleet` and `GET /api/core-lines` on a seeded history: one agent's
# execution from 9 h to 2 h ago (7 agent-hours in the last 24 h); mission
# 1 paused 6 h ago and released 3 h ago; mission 2 paused 30 h ago (in 7
# days, not in the day); no command, resume or task in the history.
#   (1) /api/interventions?window=24h counts the pause and its release:
#       total 2, by_kind pause 1 and resume 1, per_agent_hour 0.286 (2 over
#       7 agent-hours), items newest first "resumed mission #1" (resume)
#       then "paused mission #1" (pause); over 7 days total 3, 0.429.
#   (2) /api/fleet's health.human_per_agent_hour is the same 0.286.
#   (3) Core lines' observability.interventions_per_agent_hour is the same
#       0.286, over its 24 h window.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "102: harness: $*" >&2
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
SPAWN_DB_NAME="bb_102_pause_$run"
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
        echo "102: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "102: harness: unexpected exit $rc" >&2
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


note() { echo "102: $*" >&2; }
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
    values ('agent_worker_w_1', 'session_worker_w_1', 'exec_bb_102_w', 'bridge', null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) \$\$;
select pg_temp.bridge(-9, 'execution.started', '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.bridge(-2, 'execution.ended', '{"cause":"exited","exit_code":0}');
-- The owner's hold on mission 1 and its release in the day; mission 2's
-- pause before it.
select pg_temp.rec(-6, '{"type":"mission.paused","id":1,"reason":"owner hold"}');
select pg_temp.rec(-3, '{"type":"mission.unpaused","id":1,"pause_event":1}');
select pg_temp.rec(-30, '{"type":"mission.paused","id":2,"reason":"owner hold"}');
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {
  "observability.interventions_per_agent_hour": {"max": 0.05}
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
get() { # <path> <file>: the answer's code
    bb_http "$2" --noproxy '*' "http://127.0.0.1:$port$1"
}
code=$(get "/api/interventions?window=24h" "$WORK/interventions.json")
[ "$code" = 200 ] || harness "GET /api/interventions?window=24h answered $code"
week=$(get "/api/interventions?window=7d" "$WORK/interventions-7d.json")
[ "$week" = 200 ] || harness "GET /api/interventions?window=7d answered $week"
card=$(jq -c '{total, by_kind: {pause: .by_kind.pause, resume: .by_kind.resume}, per_agent_hour, items: [.items[] | {kind, line}]}' "$WORK/interventions.json")
expect_eq "$card" '{"total":2,"by_kind":{"pause":1,"resume":1},"per_agent_hour":0.286,"items":[{"kind":"resume","line":"resumed mission #1"},{"kind":"pause","line":"paused mission #1"}]}' \
    "(1) /api/interventions?window=24h counts the owner's pause and its release"
expect_eq "$(jq -c '{total, per_agent_hour}' "$WORK/interventions-7d.json")" '{"total":3,"per_agent_hour":0.429}' \
    "(1) /api/interventions?window=7d counts mission 2's pause 30 h ago too"
note "(1) checked: the Home card"

code=$(get /api/fleet "$WORK/fleet.json")
if [ "$code" != 200 ]; then
    check_failed "(2) GET /api/fleet answered $code: $(head -c 200 "$WORK/fleet.json")"
else
    expect_eq "$(jq -c '.health.human_per_agent_hour' "$WORK/fleet.json")" 0.286 \
        "(2) /api/fleet's human interventions per agent-hour, the pause and its release included"
    note "(2) checked: the Fleet page"
fi

code=$(get /api/core-lines "$WORK/api.json")
if [ "$code" != 200 ]; then
    check_failed "(3) GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"
else
    # Objective 176: the sustainability line's north star.
    got=$(jq -c '.lines[] | select(.line == "sustainability") | .north_star | select(.key == "observability.interventions_per_agent_hour") | {value, window}' "$WORK/api.json")
    expect_eq "$got" '{"value":0.286,"window":"24h"}' \
        "(3) Core lines' human interventions per agent-hour, the pause and its release included"
    note "(3) checked: Core lines"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "102: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "102: a pause and its release are interventions on the Home card, the Fleet page and Core lines alike"
exit 0
