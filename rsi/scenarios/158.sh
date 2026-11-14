#!/bin/sh
# Objective 158 [observability] (after 156): the Fleet page's strip and the
# scorecard's autonomy count the owner's posts in the queue as the Home card
# does. With 156, `/api/interventions` counts lead-mac's posts in the queue
# the ranges file names; /api/fleet's health.human_per_agent_hour (the
# strip) and `spawn scorecard`'s autonomy are counted in SQL and did not, so
# with the queue named two numbers stood under one name (the split 103
# closed). The live core-lines.json names no queue until this lands.
#
# Required (the objective's prompt): when the ranges file names
# `observability.queue_file`,
# - /api/fleet's health says `interventions_24h` (the store's interventions
#   in its day plus the queue's posts), `queue_posts_24h` and
#   `agent_ms_24h`, and `human_per_agent_hour` is the total over those
#   agent-hours, the Home card's figure;
# - `spawn scorecard --json`'s fleet autonomy counts the posts in
#   `interventions` (and its per-landing value), and `by_kind` says
#   `owner_decision` and `steering`; a mission's scope counts none of them
#   (the posts are the fleet's only) and says them at 0.
# Without `queue_file`, both are as before: the strip has no
# `queue_posts_24h` and autonomy's `by_kind` only its four kinds.
#
# Black box, through `spawn serve` and `spawn scorecard` on 156's seeded
# history and queue (8 agent-hours in the day, mission 1 resumed 5 h ago,
# lead-mac's 4 posts in the day, 2 owner decisions and 2 steering, one more
# 30 h ago, the lead's and a reviewer's not counted; offset 480).
#   (1) /api/fleet: interventions_24h 5, queue_posts_24h 4, agent_ms_24h
#       28800000, human_per_agent_hour 0.625, equal to
#       /api/interventions?window=24h's per_agent_hour.
#   (2) `spawn scorecard --since 24h --json`: the fleet's autonomy
#       interventions 5, the API's total, with by_kind owner_decision 2,
#       steering 2, resume 1; mission 1's autonomy: interventions 1,
#       owner_decision 0, steering 0. Its text says "interventions 5:
#       command 0, resume 1, owner_task 0, cancel_reassign 0, owner_decision
#       2, steering 2" for the fleet. The previous day (3 landings of
#       mission 1 seeded 46 to 39 h ago) counts the post 30 h ago: the
#       fleet's autonomy previous is 0.333.
#   (3) With the ranges file naming no queue: /api/fleet says no
#       queue_posts_24h, interventions_24h 1 and human_per_agent_hour 0.125;
#       the scorecard's fleet autonomy is 1 with by_kind exactly command,
#       resume, owner_task, cancel_reassign.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 at (1) and (2) on the base (156's throwaway): the strip reads
# 0.125 with no interventions_24h, and autonomy has no queue kinds.

set -eu

asserted=""
harness() {
    echo "158: harness: $*" >&2
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
for tool in psql jq od curl sed grep cut date; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_158_strip_$run"
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
        echo "158: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "158: harness: unexpected exit $rc" >&2
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


note() { echo "158: $*" >&2; }
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
-- Agent time as the Bridge records it (049, 100): 8 agent-hours in the day.
create function pg_temp.bridge(h numeric, name text, detail jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_w_1', 'session_worker_w_1', 'exec_bb_158_w', 'bridge', null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) \$\$;
select pg_temp.bridge(-10, 'execution.started', '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.bridge(-2, 'execution.ended', '{"cause":"exited","exit_code":0}');
select pg_temp.rec(-200, '{"type":"mission.added","id":1,"text":"158 mission","origin":"human","sources":[]}');
select pg_temp.rec(-5, '{"type":"mission.resumed","id":1}');
-- Three landings of mission 1 in the previous day (48 h to 24 h ago), so the
-- scorecard's autonomy has a previous value: Spawn's tasks, not a person's.
select pg_temp.rec(-46 + 3 * t, jsonb_build_object('type', 'task.assigned', 'id', 10 + t, 'mission_id', 1,
        'kind', 'ephemeral', 'agent_id', 'agent_worker_w_9', 'session_id', 'session_worker_w_9',
        'brief', 'Task ' || (10 + t), 'created_by', 'spawn-integrate', 'objective', null, 'model', 'default'))
  from generate_series(0, 2) t;
select pg_temp.rec(-45 + 3 * t, jsonb_build_object('type', 'task.landed', 'id', 10 + t, 'from', 'm' || t, 'to', 'm' || (t + 1)))
  from generate_series(0, 2) t;
select spawn_control_refold();
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"

# The queue, stamped as qpost stamps it, by the wall clock, here 8 h east of
# UTC.
offset=480
now_s=$(date +%s)
clock() { # <hours before now>: HH:MM in the queue's local time
    date -u -d "@$((now_s - $1 * 3600 + offset * 60))" +%H:%M
}
queue=$WORK/sync/queue.md
mkdir -p "$WORK/sync"
long=$(printf 'y%.0s' $(seq 1 200))
esc=$(printf '\033')
{
    printf '# The queue\n\nA line that is no heading.\n\n'
    printf '## lead-mac %s — OWNER DECISIONS: older than the day\n\nbody\n\n' "$(clock 30)"
    printf '## lead %s — owner gate PENDING fd0e3296\n\n' "$(clock 29)"
    printf '## lead-mac %s — [recoverability] Leak check done\n\n' "$(clock 20)"
    printf '## rev-b %s — a reviewer'"'"'s post\n\n' "$(clock 12)"
    printf '## lead %s — Owner question: an ask, not counted\n\n' "$(clock 6)"
    printf '## lead-mac %s — OWNER DECISIONS (by the owner): no second host\n\n' "$(clock 5)"
    printf '## lead %s — Owner decision: relayed, not counted\n\n' "$(clock 4)"
    printf '## lead-mac %s — F4 PASSES\n\n- 00:10 a list line, no heading\n\n' "$(clock 3)"
    printf '## lead-mac %s — x%s[31m <b>Decisions</b> & "the owner" %s\n' "$(clock 1)" "$esc" "$long"
} >"$queue"
ranges_with_queue() {
    cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {"observability.interventions_per_agent_hour": {"max": 0.05}},
 "observability": {"queue_file": "$queue", "queue_utc_offset_minutes": $offset}}
JSON
}
ranges_with_queue

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
answer() { # <window> <file>
    code=$(get "/api/interventions?window=$1" "$2")
    [ "$code" = 200 ] || harness "GET /api/interventions?window=$1 answered $code: $(head -c 200 "$2")"
    cp "$2" "$WORK/api.json"
}
kinds() { # <file>: by_kind as key=value, sorted
    jq -r '.by_kind | to_entries | sort_by(.key) | map("\(.key)=\(.value)") | join(",")' "$1"
}

# --- (1) the strip ----------------------------------------------------------------------

answer 24h "$WORK/day.json"
[ "$(jq -r '.total' "$WORK/day.json")" = 5 ] ||
    harness "the seed: /api/interventions?window=24h does not count a resume and 4 posts (156): $(jq -c '{total, by_kind}' "$WORK/day.json")"
code=$(get /api/fleet "$WORK/api.json")
[ "$code" = 200 ] || harness "GET /api/fleet answered $code: $(head -c 200 "$WORK/api.json")"
cp "$WORK/api.json" "$WORK/fleet.json"
expect_eq "$(jq -c '.health | {interventions_24h, queue_posts_24h, agent_ms_24h, human_per_agent_hour}' "$WORK/fleet.json")" \
    '{"interventions_24h":5,"queue_posts_24h":4,"agent_ms_24h":28800000,"human_per_agent_hour":0.625}' \
    "(1) the Fleet strip counts the queue's posts: 5 interventions over 8 agent-hours"
expect_eq "$(jq -r '.health.human_per_agent_hour' "$WORK/fleet.json")" "$(jq -r '.per_agent_hour' "$WORK/day.json")" \
    "(1) the Fleet strip's figure is the Home card's"
note "(1) done"

# --- (2) the scorecard --------------------------------------------------------------------

autonomy() { # <scorecard file> <scope jq filter>: the autonomy's interventions and queue kinds
    jq -c ".scopes[] | select($2) | .dimensions.autonomy | {interventions, owner_decision: .by_kind.owner_decision, steering: .by_kind.steering, resume: .by_kind.resume}" "$1"
}
capture "$SPAWN_BIN" scorecard --since 24h --json
[ "$status" -eq 0 ] || harness "spawn scorecard --since 24h failed: $(err)"
cp "$WORK/out.txt" "$WORK/card.json"
expect_eq "$(autonomy "$WORK/card.json" '.scope == "fleet"')" \
    '{"interventions":5,"owner_decision":2,"steering":2,"resume":1}' \
    "(2) the scorecard's fleet autonomy counts the queue's posts, as the API (total 5)"
expect_eq "$(autonomy "$WORK/card.json" '.scope == "mission" and .mission == 1')" \
    '{"interventions":1,"owner_decision":0,"steering":0,"resume":1}' \
    "(2) mission 1's autonomy counts none of the posts (the fleet's only)"
expect_eq "$(jq -c '.scopes[] | select(.scope == "fleet") | .dimensions.autonomy.previous' "$WORK/card.json")" 0.333 \
    "(2) the fleet's previous day: the post 30 h ago over its 3 landings"
capture "$SPAWN_BIN" scorecard --since 24h
[ "$status" -eq 0 ] || harness "spawn scorecard --since 24h (text) failed: $(err)"
grep -q "interventions 5: command 0, resume 1, owner_task 0, cancel_reassign 0, owner_decision 2, steering 2" "$WORK/out.txt" ||
    check_failed "(2) the scorecard's text says the fleet's 5 interventions with the queue's kinds: $(grep -m 3 'interventions' "$WORK/out.txt" | tr '\n' ' ' | head -c 400)"
note "(2) done"

# --- (3) no queue named ---------------------------------------------------------------------

cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {"observability.interventions_per_agent_hour": {"max": 0.05}}}
JSON
code=$(get /api/fleet "$WORK/api.json")
[ "$code" = 200 ] || harness "GET /api/fleet answered $code"
expect_eq "$(jq -c '.health | {interventions_24h, queue_posts_24h, human_per_agent_hour}' "$WORK/api.json")" \
    '{"interventions_24h":1,"queue_posts_24h":null,"human_per_agent_hour":0.125}' \
    "(3) with no queue named, the strip is the store's"
capture "$SPAWN_BIN" scorecard --since 24h --json
[ "$status" -eq 0 ] || harness "spawn scorecard --since 24h failed: $(err)"
expect_eq "$(jq -c '.scopes[] | select(.scope == "fleet") | .dimensions.autonomy | {interventions, kinds: (.by_kind | keys)}' "$WORK/out.txt")" \
    '{"interventions":1,"kinds":["cancel_reassign","command","owner_task","resume"]}' \
    "(3) with no queue named, the scorecard's autonomy is as before"
note "(3) done"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "158: $(wc -l <"$WORK/failures") checks failed" >&2
    exit 1
fi
echo "158: the Fleet strip and the scorecard count the owner's posts in the queue as the Home card does"
exit 0
