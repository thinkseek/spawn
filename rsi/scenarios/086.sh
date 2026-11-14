#!/bin/sh
# Black box: the Core lines frame's performance line. Its north star is the
# time from report to live (objective 081), and its guardrails are the gate
# time per run, the gate time that decided nothing, the landings that pass
# first time, and the dollars per landing, all over the last 7 days.
#
# The problem (owner, 2026-11-12 13:30, ranges corrected 13:50;
# sync/roadmap-effects.md, "North-star and guardrail metrics", Performance):
# objective 084 built the frame (`GET /api/core-lines`, ranges only from
# core-lines.json, the Home card) with the performance line empty. Its
# figures exist on two pages: the Scorecard (7 days: report to live, gate
# time, decided nothing, first pass) and the Cost page (dollars per landing).
# Monitoring only: no gate, integrator or deploy reads any of it.
#
# Required (read through `GET /api/core-lines`, `/api/scorecard` and
# `/api/cost?window=7d`, and web/src/coreLines.ts under node):
#   (1) the performance line: north star `performance.report_to_live` (ms),
#       guardrails in this order `performance.gate_time` (ms),
#       `performance.decided_nothing` (share), `performance.first_pass`
#       (share), `performance.usd_per_landing` (usd). Their values are the
#       Scorecard's fleet scope over its default 7 days: report_to_live's
#       value, gate_time's value, gate_time's undecided_ms over its total_ms
#       (null when the total is 0), first_pass's value; and the Cost page's
#       7-day usd_per_landing. State and range as for every figure (084).
#       Since objective 176 (the owner's core metric set) dollars per
#       landing is a diagnostic, read from the answer's `diagnostics`, and
#       the line is the other four.
#   (2) coreLinesView says the new units: ms as a span ("45 s", "14 min",
#       "1 h 32 min", "2 h"), share as a percent to one decimal ("74.9%"),
#       usd as dollars and cents ("$8.08"), ranges in the same words.
#
# The history is 081's (five landings, three deploys), with each gating's
# wall clock (`gate_ms`), and one aborted gating that decided nothing.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "086: harness: $*" >&2
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
SPAWN_DB_NAME="bb_086_conflicts_$run"
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
    if [ "$rc" -ne 0 ] && [ -s "$WORK/lines.json" ]; then
        echo "086: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/lines.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "086: harness: unexpected exit $rc" >&2
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


note() { echo "086: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

B=$(printf 'bb-086-base' | sha1sum | cut -c1-40)
C=$(printf 'bb-086-candidate' | sha1sum | cut -c1-40)
# One "now" for the whole seed, so the intervals are exact to the millisecond.
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
create function pg_temp.assigned(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', task, 'mission_id', 1,
        'kind', 'ephemeral', 'created_by', 'human',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'brief', 'Task ' || task || ' of 081')) \$\$;
create function pg_temp.reported(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.reported', 'id', task, 'commit', '$C')) \$\$;
create function pg_temp.gated(h numeric, task int, verdict text, revision int, gate_s int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', task, 'verdict', verdict,
        'revision', revision, 'base', '$B', 'scenarios', 4, 'candidate', '$C', 'reason', verdict,
        'gate_ms', gate_s * 1000)) \$\$;
create function pg_temp.landed(h numeric, task int) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.landed', 'id', task, 'from', '$B', 'to', '$C',
        'missions', '[]'::jsonb)) \$\$;
create function pg_temp.deployed(h numeric, ok boolean) returns void language sql as \$\$
    select pg_temp.rec(h, jsonb_build_object('type', 'deploy.finished', 'by', 'landing', 'head', '$C',
        'ok', ok, 'reason', case when ok then '' else 'the build failed' end)) \$\$;

-- A deploy that failed 8 days ago: the live record began before this
-- window, so every landing in it counts under objective 095's rule as
-- under 081's (a failed deploy ships nothing).
select pg_temp.deployed(-192, false);
select pg_temp.rec(-12, '{"type":"mission.added","id":1,"text":"081 mission","origin":"human","sources":[]}');
select pg_temp.rec(-12, '{"type":"mission.configured","id":1,"accept":"gate","gate":"$WORK/ctl/scenarios"}');
select pg_temp.assigned(-11, k) from generate_series(1, 5) k;
select pg_temp.reported(-10, 1);
select pg_temp.gated(-9.6, 1, 'accepted', 0, 600);
select pg_temp.landed(-9.5, 1);
select pg_temp.deployed(-9, true);
select pg_temp.reported(-8.5, 2);
select pg_temp.gated(-8.2, 2, 'rejected', 0, 900);
select pg_temp.reported(-8, 2);
select pg_temp.gated(-7.5, 2, 'accepted', 1, 600);
select pg_temp.landed(-7.4, 2);
select pg_temp.deployed(-7, false);
select pg_temp.deployed(-6, true);
select pg_temp.reported(-5, 3);
select pg_temp.reported(-4.8, 4);
select pg_temp.gated(-4.6, 3, 'accepted', 0, 1200);
select pg_temp.landed(-4.5, 3);
select pg_temp.gated(-4.3, 4, 'accepted', 0, 600);
select pg_temp.landed(-4.2, 4);
select pg_temp.deployed(-4, true);
select pg_temp.reported(-2, 5);
select pg_temp.gated(-1.8, 5, 'aborted', 0, 300);
select pg_temp.gated(-1.6, 5, 'accepted', 1, 900);
select pg_temp.landed(-1.5, 5);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:landed 2:landed 3:landed 4:landed 5:landed" ] || harness "the seed folded to tasks [$tasks]"

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
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<'JSON'
{"ranges": {
  "performance.gate_time": {"max": 900000},
  "performance.decided_nothing": {"max": 0.05},
  "performance.first_pass": {"min": 0.7},
  "performance.usd_per_landing": {"max": 10}
}}
JSON
code=$(get /api/scorecard "$WORK/scorecard.json")
[ "$code" = 200 ] || harness "GET /api/scorecard answered $code"
code=$(get "/api/cost?window=7d" "$WORK/cost.json")
[ "$code" = 200 ] || harness "GET /api/cost?window=7d answered $code"
fleet() { jq -c ".scopes[] | select(.scope == \"fleet\") | .dimensions | $1" "$WORK/scorecard.json"; }
[ "$(fleet '.gate_time.n')" = 7 ] ||
    harness "the scorecard does not count the seven seeded gatings: $(fleet '.gate_time' | head -c 400)"
code=$(get /api/core-lines "$WORK/lines.json")
if [ "$code" != 200 ]; then
    check_failed "(1) GET /api/core-lines answered $code: $(head -c 200 "$WORK/lines.json")"
    printf '{}' >"$WORK/lines.json"
else
    perf() { jq -c ".lines[] | select(.line == \"performance\") | $1" "$WORK/lines.json"; }
    # Objective 176: dollars per landing is read from the diagnostics.
    figure() { jq -c "([.lines[] | select(.line == \"performance\") | .north_star, .guardrails[]] + (.diagnostics // []))[] | select(.key == \"$1\") | {unit, value, range, state}" "$WORK/lines.json"; }
    expect_eq "$(perf '[.north_star.key] + [.guardrails[].key]')" \
        '["performance.report_to_live","performance.gate_time","performance.decided_nothing","performance.first_pass"]' \
        "(1) the performance line's north star and guardrails, in order"
    state() { # <value> <in?>: the state a value with this range has
        if [ "$1" = null ]; then echo not_collected; elif [ "$2" = in ]; then echo in; else echo out; fi
    }
    live=$(fleet '.report_to_live.value')
    expect_eq "$(figure performance.report_to_live)" \
        "{\"unit\":\"ms\",\"value\":$live,\"range\":null,\"state\":\"$( [ "$live" = null ] && echo not_collected || echo no_range)\"}" \
        "(1) report to live: the Scorecard's fleet value, no range set"
    [ "$live" != null ] || check_failed "(1) the Scorecard has no report_to_live value for the seeded history (objective 081)"
    gate=$(fleet '.gate_time.value')
    expect_eq "$(figure performance.gate_time)" \
        "{\"unit\":\"ms\",\"value\":$gate,\"range\":{\"max\":900000},\"state\":\"$(state "$gate" "$( [ "$gate" != null ] && [ "$gate" -le 900000 ] && echo in)")\"}" \
        "(1) gate time per run: the Scorecard's fleet median (10 min of these runs), in its range of 15 min or less"
    share=$(fleet '.gate_time | if (.total_ms // 0) > 0 then .undecided_ms / .total_ms else null end')
    got=$(figure performance.decided_nothing)
    expect_eq "$(printf '%s' "$got" | jq -c '{unit, range, state}')" '{"unit":"share","range":{"max":0.05},"state":"out"}' \
        "(1) decided nothing: the 5 min aborted run of 85 min gated (5.9%) is out of its range of 5% or less"
    awk -v g="$(printf '%s' "$got" | jq -r '.value')" -v w="$share" 'BEGIN { d = g - w; if (d < 0) d = -d; exit !(g != "null" && d < 1e-9) }' ||
        check_failed "(1) decided nothing [$(printf '%s' "$got" | jq -r '.value')]: the Scorecard's undecided_ms over total_ms [$share]"
    pass=$(fleet '.first_pass.value')
    expect_eq "$(figure performance.first_pass)" \
        "{\"unit\":\"share\",\"value\":$pass,\"range\":{\"min\":0.7},\"state\":\"$(state "$pass" "$(awk -v v="$pass" 'BEGIN { if (v != "null" && v >= 0.7) print "in" }')")\"}" \
        "(1) landings that pass first time: the Scorecard's fleet value"
    usd=$(jq -c '.usd_per_landing' "$WORK/cost.json")
    expect_eq "$(figure performance.usd_per_landing)" \
        "{\"unit\":\"usd\",\"value\":$usd,\"range\":{\"max\":10},\"state\":\"$(state "$usd" "$(awk -v v="$usd" 'BEGIN { if (v != "null" && v <= 10) print "in" }')")\"}" \
        "(1) dollars per landing: the Cost page's 7-day usd_per_landing"
    note "(1) checked: the performance line"
fi

# --- (2) the card's words ------------------------------------------------------------------

cat >"$WORK/lines.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const fig = (key, name, unit, value, range, state) => ({ key, name, unit, value, range, state });
const answer = {
  ranges_file: "/ws/core-lines.json", ranges_error: null, ignored_ranges: [],
  lines: [
    { line: "performance",
      north_star: fig("performance.report_to_live", "Time from report to live", "ms", 5_520_000, { max: 3_600_000 }, "out"),
      guardrails: [
        fig("performance.gate_time", "Gate time per run", "ms", 840_000, { max: 900_000 }, "in"),
        fig("performance.decided_nothing", "Gate time that decided nothing", "share", 0.08, { max: 0.1 }, "in"),
        fig("performance.first_pass", "Landings that pass first time", "share", 0.749, { min: 0.7 }, "in"),
        fig("performance.usd_per_landing", "Dollars per landing", "usd", 8.08, { max: 10 }, "in"),
      ] },
  ],
};

test("(2) the performance line in words", () => {
  const [row] = coreLinesView(answer).rows;
  assert.deepEqual(row.north_star, { name: "Time from report to live", value: "1 h 32 min", range: "1 h or less", tone: "warning" });
  assert.deepEqual(row.guardrails, [
    { name: "Gate time per run", value: "14 min", range: "15 min or less", tone: "plain" },
    { name: "Gate time that decided nothing", value: "8.0%", range: "10.0% or less", tone: "plain" },
    { name: "Landings that pass first time", value: "74.9%", range: "70.0% or more", tone: "plain" },
    { name: "Dollars per landing", value: "$8.08", range: "$10.00 or less", tone: "plain" },
  ]);
});

test("(2) a span under a minute", () => {
  const short = { ...answer, lines: [{ ...answer.lines[0], north_star: fig("performance.report_to_live", "Time from report to live", "ms", 45_000, null, "no_range") }] };
  assert.equal(coreLinesView(short).rows[0].north_star.value, "45 s");
});
TEST
if ! BB_TREE=$tree node --test "$WORK/lines.test.mjs" >"$WORK/lines.out" 2>&1; then
    check_failed "(2) web/src/coreLines.ts: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/lines.out" | head -n 14 | tr '\n' ' ' | head -c 1200)"
fi
note "(2) checked: the card's words"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "086: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "086: the Core lines performance line gives report to live, gate time, decided nothing, first pass and dollars per landing"
exit 0
