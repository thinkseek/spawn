#!/bin/sh
# Black box: dollars per landing takes its range while use with no price is
# under 10% (objective 207).
#
# The problem (the owner's core metric set of 2026-11-14 18:42: dollars per
# landing has "no range until use with no price is under 10%"; lead-mac's
# queue answer of 2026-11-17 04:24): since objective 182, dollars per
# landing (`performance.usd_per_landing`) is a plain reading of the
# sustainability line, always with no range. The condition the set names
# was never read: the "no range" is hard-coded.
#
# Required (read through `GET /api/core-lines` and `/api/cost?window=7d`,
# the ranges file `{"performance.usd_per_landing": {"max": 10, "target": 5}}`,
# one landing in the 7 days, a fresh `spawn serve` per case; model use
# recorded as the Bridge records it, priced by spawn_cost_record):
#   (1) no model use in the 7 days (use with no price,
#       `observability.unpriced_share`, is null): dollars per landing is the
#       one reading with that key, its value the Cost page's 7-day
#       usd_per_landing, range null, no target, state no_range, range_note
#       "no range until use with no price is under 10%"; the file's entry is
#       in ignored_ranges with the reason "shown with no range until use with
#       no price is under 10% (the owner's core metric set)".
#   (2) 900,000 tokens priced ($2.70) and 100,000 with no price: exactly
#       10%, not under it, so as (1), the value 2.7.
#   (3) 1,100,000 more priced (100,000 of 2,100,000, 4.8%; $6.00): dollars
#       per landing takes the file's range and target as configured,
#       {"max": 10} and 5, state in, no range_note; nothing for it in
#       ignored_ranges.
#   (4) 400,000 more with no price (500,000 of 2,500,000, 20%): as (1).
# In every case dollars per landing is a reading only: never a north star,
# a guardrail, an alert figure or a diagnostic (it raises nothing).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "207: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep cut awk; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_207_priced_$run"
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
    if [ "$rc" -ne 0 ] && [ -s "$WORK/lines.json" ]; then
        echo "207: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/lines.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "207: harness: unexpected exit $rc" >&2
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


note() { echo "207: $*" >&2; }
expect_eq() { # <got> <want> <what>
    # A filter that does not compile or run is the harness's fault, not a red.
    case "$1" in *jq-failed*) harness "$3: a jq filter failed" ;; esac
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the world: one mission, one landing, the ranges file -------------------------------------

cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {"performance.usd_per_landing": {"max": 10, "target": 5}}}
JSON
cat >"$WORK/seed.sql" <<'SQL'
\set ON_ERROR_STOP 1
begin;
create function pg_temp.ms(h numeric) returns bigint language sql as $$
    select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint + round(h * 3600000)::bigint $$;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) $$;
select pg_temp.rec(-5, '{"type":"mission.added","id":1,"text":"207 mission","origin":"human","sources":[]}');
select pg_temp.rec(-5, '{"type":"mission.configured","id":1,"accept":"review"}');
select pg_temp.rec(-4, '{"type":"task.assigned","id":1,"mission_id":1,"kind":"ephemeral","created_by":"human","agent_id":"agent_worker_objective_1","session_id":"session_worker_objective_1","brief":"Task 1 of 207"}');
select pg_temp.rec(-3, '{"type":"task.reported","id":1,"commit":"2072072072072072072072072072072072072072"}');
select pg_temp.rec(-2, '{"type":"task.landed","id":1,"from":"1071071071071071071071071071071071071071","to":"2072072072072072072072072072072072072072","missions":[]}');
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
landed=$(scalar "select count(*) from spawn_task_spans where outcome in ('landed', 'accepted')")
[ "$landed" = 1 ] || harness "the seed has $landed landed task spans, not 1"

# One model call as the Bridge records it, then priced by spawn_cost_record
# (bb-model-x: $3 per million input tokens; no price for bb-model-free).
usage() { # <message> <model> <input tokens>
    cat >"$WORK/usage.sql" <<SQL
\\set ON_ERROR_STOP 1
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                          kind, session_seq, recorded_at_ms)
select 'agent_worker_objective_1', 'session_worker_objective_1', 'exec_bb_207_1', 'bridge', null,
       jsonb_build_object('name', 'model.usage', 'detail', jsonb_build_object(
           'message_id', 'msg_bb_207_$1', 'model', '$2', 'at_ms', now_ms,
           'input_tokens', $3, 'output_tokens', 0, 'cache_write_tokens', 0, 'cache_read_tokens', 0)),
       200000 + $1, now_ms
  from (select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint - 3600000 as now_ms) t;
select spawn_cost_record('{"bb-model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}}');
SQL
    capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/usage.sql"
    [ "$status" -eq 0 ] || harness "recording message $1 failed: $(err)"
}

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
# serve keeps the performance figures a minute: a fresh serve per case.
serve_start() {
    : >"$WORK/serve.log"
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >>"$WORK/serve.log" 2>&1 &
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
}
serve_stop() {
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
    serve_pid=""
}
get() { # <path> <file>: the answer's code
    bb_http "$2" --noproxy '*' "http://127.0.0.1:$port$1"
}

USD=performance.usd_per_landing
SHARE=observability.unpriced_share
NOTE="no range until use with no price is under 10%"
REASON="shown with no range until use with no price is under 10% (the owner's core metric set)"
# <case> <what>: read /api/core-lines and /api/cost?window=7d on a fresh
# serve; prints nothing, leaves lines.json and cost.json.
read_case() {
    serve_start
    code=$(get "/api/cost?window=7d" "$WORK/cost.json")
    [ "$code" = 200 ] || harness "($1) GET /api/cost?window=7d answered $code"
    code=$(get /api/core-lines "$WORK/lines.json")
    serve_stop
    [ "$code" = 200 ] || harness "($1) GET /api/core-lines answered $code: $(head -c 300 "$WORK/lines.json")"
}
reading() { jq -c "[.lines[] | .readings[]? | select(.key == \"$USD\")] | if length == 1 then .[0] | {value: (.value | if type == \"number\" then . + 0 else . end), range, target, state, range_note} else {count: length} end" "$WORK/lines.json" || echo jq-failed; }
share_value() { jq -c "[.lines[] | (.north_star, .guardrails[]?) | select(.key == \"$SHARE\") | .value] | .[0]" "$WORK/lines.json" || echo jq-failed; }
ignored() { jq -c "[.ignored_ranges[]? | select(.key == \"$USD\")]" "$WORK/lines.json" || echo jq-failed; }
# Elsewhere than a reading: a guardrail, an alert figure or a diagnostic.
elsewhere() { jq -c "[(.lines[] | .north_star, .guardrails[]?), .alert_figures[]?, .diagnostics[]? | select(.key == \"$USD\") | .key]" "$WORK/lines.json" || echo jq-failed; }
share_is() { # <case> <awk expression> <what>: use with no price is that share
    share=$(share_value)
    awk -v s="$share" "BEGIN { d = s - ($2); if (d < 0) d = -d; exit !(s != \"null\" && d < 1e-9) }" ||
        harness "($1) use with no price is [$share], not $3"
}
no_range_case() { # <case> <what>
    usd=$(jq -c '.usd_per_landing | if type == "number" then . + 0 else . end' "$WORK/cost.json") || harness "($1) cost.json"
    expect_eq "$(reading)" \
        "{\"value\":$usd,\"range\":null,\"target\":null,\"state\":\"no_range\",\"range_note\":\"$NOTE\"}" \
        "($1) $2: dollars per landing (the Cost page's 7-day value) a reading with no range, its note"
    expect_eq "$(ignored)" "[{\"key\":\"$USD\",\"reason\":\"$REASON\"}]" \
        "($1) $2: the file's entry for dollars per landing listed as ignored, with the reason"
    expect_eq "$(elsewhere)" '[]' "($1) $2: dollars per landing is a reading only"
}

# --- (1) missing: no model use in the window -----------------------------------------------------

read_case 1
expect_eq "$(share_value)" null "(1) use with no price, with no model use in the 7 days"
no_range_case 1 "use with no price missing"
note "(1) checked: missing"

# --- (2) exactly 10%: 900,000 tokens priced ($2.70), 100,000 unpriced ------------------------------

usage 1 bb-model-x 900000
usage 2 bb-model-free 100000
read_case 2
share_is 2 0.1 "100,000 of 1,000,000 tokens"
expect_eq "$(jq -c '.usd_per_landing | if type == "number" then . + 0 else . end' "$WORK/cost.json" || echo jq-failed)" 2.7 \
    "(2) the Cost page's 7-day dollars per landing (\$2.70, one landing)"
no_range_case 2 "use with no price at exactly 10%"
note "(2) checked: exactly 10%"

# --- (3) under 10%: 1,100,000 more priced (100,000 of 2,100,000, 4.8%; $6.00) ----------------------

usage 3 bb-model-x 1100000
read_case 3
share_is 3 "100000 / 2100000" "100,000 of 2,100,000 tokens"
expect_eq "$(reading)" '{"value":6,"range":{"max":10},"target":5,"state":"in","range_note":null}' \
    "(3) use with no price at 4.8%: dollars per landing takes the file's range and target as configured, \$6.00 in {\"max\": 10}, no note"
expect_eq "$(ignored)" '[]' "(3) use with no price at 4.8%: dollars per landing's entry is not listed as ignored"
expect_eq "$(elsewhere)" '[]' "(3) use with no price at 4.8%: dollars per landing is still a reading only (it raises nothing)"
note "(3) checked: under 10%"

# --- (4) over 10%: 400,000 more unpriced (500,000 of 2,500,000, 20%) ------------------------------

usage 4 bb-model-free 400000
read_case 4
share_is 4 0.2 "500,000 of 2,500,000 tokens"
no_range_case 4 "use with no price at 20%"
note "(4) checked: over 10%"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "207: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "207: dollars per landing takes its configured range while use with no price is under 10%, and has none when it is over or missing"
exit 0
