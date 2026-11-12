#!/bin/sh
# Black box: the Core lines frame's observability line. Its guardrails are
# the share of tokens no price covers (7 days), the human interventions per
# agent-hour (24 h), and how many of the card's own figures are not
# measured.
#
# The problem (owner, 2026-11-12 13:30, ranges corrected 13:50;
# sync/roadmap-effects.md, "North-star and guardrail metrics",
# Observability): objective 084 built the frame with the observability line
# empty. Its guardrails are on the Cost page (use with no price), on Home
# (interventions per agent-hour) and nowhere as a number ("not measured
# yet" on the page). The north star (Needs you is right) waits for the
# alerts' outcome classes and stays not collected. "Page effects that work
# on first sight" has no product record and is not a figure here.
# Monitoring only: no gate, integrator or deploy reads any of it.
#
# Required (read through `GET /api/core-lines`, `/api/cost?window=7d` and
# `/api/fleet`):
#   (1) the observability line: north star `observability.needs_you_right`
#       (not collected), guardrails in this order
#       `observability.unpriced_share` (share: the 7-day window's
#       unpriced_tokens over all its tokens, input, output, cache write and
#       cache read, as /api/cost?window=7d's total says; null with no
#       tokens), `observability.interventions_per_agent_hour` (ratio:
#       /api/fleet's health.human_per_agent_hour), `observability.not_measured`
#       (count: the figures of this answer with no value, itself left out).
#       State and range as for every figure.
#
# The history: model calls an hour ago, 300,000 input tokens on a priced
# model and 100,000 on a model with no price (a quarter unpriced); one
# agent's execution from 10 h to 2 h ago and one person's command 3 h ago.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "088: harness: $*" >&2
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
SPAWN_DB_NAME="bb_088_observability_$run"
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
        echo "088: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "088: harness: unexpected exit $rc" >&2
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


note() { echo "088: $*" >&2; }
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
-- A worker's model call as the Bridge records it: one message's usage (068).
create function pg_temp.usage(h numeric, n int, model text, input bigint) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_objective_' || n, 'session_worker_objective_' || n,
            'exec_bb_088_' || n, 'bridge', null,
            jsonb_build_object('name', 'model.usage', 'detail', jsonb_build_object(
                'message_id', 'msg_bb_088_' || n, 'model', model, 'at_ms', pg_temp.ms(h),
                'input_tokens', input, 'output_tokens', 0,
                'cache_write_tokens', 0, 'cache_read_tokens', 0)),
            nextval('seed_seq'), pg_temp.ms(h)) \$\$;
select pg_temp.usage(-1, 1, 'bb-model-x', 300000);
select pg_temp.usage(-1, 2, 'bb-model-y', 100000);
-- Agent time as the Bridge records it (049): one execution from 10 h to 2 h
-- ago, 8 agent-hours in the last 24 h; and one person's command in it.
create function pg_temp.bridge(h numeric, name text, detail jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_w_1', 'session_worker_w_1', 'exec_bb_088_w', 'bridge', null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) \$\$;
select pg_temp.bridge(-10, 'execution.started', '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.bridge(-2, 'execution.ended', '{"cause":"exited","exit_code":0}');
insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                            succeeded, error, created_at_ms, finished_at_ms)
values ('human', 'board-1', 'agent_worker_w_1', 'session_worker_w_1',
        '{"type":"input","text":"a note"}', 'finished', true, null, pg_temp.ms(-3), pg_temp.ms(-3) + 1000);
commit;
select spawn_cost_record('{"bb-model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}}');
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
placed=$(scalar "select string_agg(model || ':' || (input_tokens)::text || ':' || coalesce(usd::numeric(10,2)::text, 'unpriced'), ' ' order by model) from spawn_cost_messages")
[ "$placed" = "bb-model-x:300000:0.90 bb-model-y:100000:unpriced" ] ||
    harness "the cost step did not place the seeded calls as one priced and one unpriced: [$placed]"
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {
  "observability.unpriced_share": {"max": 0.05},
  "observability.interventions_per_agent_hour": {"max": 0.05},
  "observability.not_measured": {"max": 0}
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
    curl -s --noproxy '*' -o "$2" -w '%{http_code}' "http://127.0.0.1:$port$1" 2>/dev/null || echo 000
}
code=$(get "/api/cost?window=7d" "$WORK/cost.json")
[ "$code" = 200 ] || harness "GET /api/cost?window=7d answered $code"
code=$(get /api/fleet "$WORK/fleet.json")
[ "$code" = 200 ] || harness "GET /api/fleet answered $code"
code=$(get /api/core-lines "$WORK/api.json")
if [ "$code" != 200 ]; then
    check_failed "(1) GET /api/core-lines answered $code: $(head -c 200 "$WORK/api.json")"
else
    obs() { jq -c ".lines[] | select(.line == \"observability\") | $1" "$WORK/api.json"; }
    figure() { obs "([.north_star] + .guardrails)[] | select(.key == \"$1\") | {unit, value, range, state}"; }
    expect_eq "$(obs '[.north_star.key] + [.guardrails[].key]')" \
        '["observability.needs_you_right","observability.unpriced_share","observability.interventions_per_agent_hour","observability.not_measured"]' \
        "(1) the observability line's north star and guardrails, in order"
    # The north star's value is the alert outcomes' objective's (B4): only its
    # key is this line's, and 084 holds "not collected exactly when no value".
    share=$(jq -c '.total | (.tokens | .input + .output + .cache_write + .cache_read) as $all | if $all > 0 then .unpriced_tokens / $all else null end' "$WORK/cost.json")
    expect_eq "$share" 0.25 "(1) the seed: /api/cost?window=7d has a quarter of its tokens unpriced"
    expect_eq "$(figure observability.unpriced_share)" '{"unit":"share","value":0.25,"range":{"max":0.05},"state":"out"}' \
        "(1) use with no price: 100,000 of 400,000 tokens, out of its range of 5% or less"
    rate=$(jq -c '.health.human_per_agent_hour' "$WORK/fleet.json")
    case "$rate" in '' | null) harness "/api/fleet has no human_per_agent_hour for 8 seeded agent-hours and one command: [$rate]" ;; esac
    rate_state=$(awk -v v="$rate" 'BEGIN { print (v <= 0.05 ? "in" : "out") }')
    expect_eq "$(figure observability.interventions_per_agent_hour)" \
        "{\"unit\":\"ratio\",\"value\":$rate,\"range\":{\"max\":0.05},\"state\":\"$rate_state\"}" \
        "(1) interventions per agent-hour: /api/fleet's human_per_agent_hour ($rate: one command over 8 agent-hours)"
    others=$(jq -c '[.lines[] | ([.north_star] + .guardrails)[] | select(.key != "observability.not_measured" and .value == null)] | length' "$WORK/api.json")
    [ "$others" -gt 0 ] || harness "every other figure has a value: the count cannot be told from zero"
    expect_eq "$(figure observability.not_measured)" "{\"unit\":\"count\",\"value\":$others,\"range\":{\"max\":0},\"state\":\"out\"}" \
        "(1) not measured: the $others other figures of this answer with no value"
    note "(1) checked: the observability line"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "088: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "088: the Core lines observability line gives use with no price, interventions per agent-hour and the figures not measured"
exit 0
