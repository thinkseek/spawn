#!/bin/sh
# Black box: how well the fleet does over weeks. The experience views in SQL
# (`spawn_decisions`, `spawn_interventions`, `spawn_outcomes`,
# `spawn_escalations`) and one answer per window over them,
# `GET /api/scorecard/fleet?window=24h|7d|30d|90d[&until=<RFC 3339>]`, which
# the Scorecard's "How the fleet decides" reads through web/src/fleetDecisions.ts
# (`decisionsFromApi`): the decision ladder per tier, human decisions per
# agent-hour, escalations, interventions, outcomes, tasks landed per day and
# each mission's idle hours.
#
# Today (13fc93a7 and rev-b's df2021c3) the ladder and human decisions per
# agent-hour exist for the last 24 h only (`/api/fleet`'s health, computed
# in its own query), nothing counts escalations or idle hours, and no view
# derives decisions, interventions or outcomes from the log (docs/fleet-
# architecture.md §4 and §9 "Not built").
#
# The scenario seeds a fresh database with a known history over 30 days and
# no loop, as 017 does: control records written straight into spawn_events
# at chosen times and folded by spawn_control_refold(), judgements
# (`observation.recorded`), executions (`execution.started`/`ended`) and
# command rows. Every window ends at U = 2026-10-31T00:00:00Z (`until`), so
# the numbers are fixed. Times below are hours before U.
#   M1 (added -720): t1 assigned -700, landed -690; t7 assigned -60, aborted
#      -50; t2 assigned -100, landed -90; t3 assigned -20, landed -10; t4
#      assigned -5, delivered, open. Resumed (mission.resumed) at -70.
#   M2 (added -48): t5 assigned -40 by human, accepted by review -24.
#   M3 (added -400, closed -300): t6 assigned -360, cancelled by human -350;
#      t8 assigned -349 with t6's brief (a cancel-and-reassign), accepted by
#      review -320.
#   judgements: rules at -600 and -2 (-2: JUDGE_UNAVAILABLE); jev at -150
#      (500 ms), -1 (300 ms, rules said WAITING) and -0.5 (500 ms).
#   commands: spawn-dispatch at -650, -30, -3, and two digests to the
#      supervisor at -160 (tags A, B) and -2 (tags A, C); spawn_supervisor at
#      -200 and -4; human at -500; spawn (the CLI) at -12; spawn-liveness at
#      -6; spawn-integrate at -8.
#   executions: one from -30 to -6, one from -2, still running at U.
# Worked by hand (since inclusive, until exclusive; a ratio to 3 decimals as
# the scorecard rounds):
#   24h: ladder rules 1, model 2, dispatcher 2, supervisor 1, human 1,
#        liveness 1, extra spawn-integrate 1; agent hours 20; human per
#        agent-hour 0.05; escalations 1 (C; A was first told before);
#        interventions 1 (command 1); outcomes landed 1, accepted 1; landed
#        per day 2 (a landing as the scorecard counts it: landed, or accepted
#        by review on a mission without a gate); judgements 3 (rules 1, jev 2), unavailable 1, disagreed
#        1, p50 400 ms; missions M1 9 idle of 24 h, M2 24 of 24 (idle).
#   7d:  rules 1, model 3, dispatcher 4, supervisor 1, human 1, liveness 1,
#        extra 1; agent hours 26; 0.038; escalations 3; interventions 3
#        (command 1, resume 1, owner_task 1); landed 2, accepted 1, aborted
#        1; landed per day 0.429; judgements 4, p50 500 ms; M1 133 of 168
#        (idle), M2 32 of 48 (idle).
#   30d: rules 2, model 3, dispatcher 5, supervisor 2, human 2, liveness 1,
#        extra 1; agent hours 26; 0.077; escalations 3; interventions 5
#        (command 2, resume 1, owner_task 1, cancel_reassign 1); landed 3,
#        accepted 2, cancelled 1, aborted 1; landed per day 0.167; judgements
#        5; M1 675 of 720 (idle), M2 32 of 48 (idle), M3 61 of 100 (idle).
# Checked:
#   (1) the views exist and, filtered to each window, give those numbers.
#   (2) the API answers each window with exactly those numbers, and 400 for
#       a window it does not know.
#   (3) its interventions and landings equal what `spawn scorecard --json`
#       counts as autonomy's interventions and throughput's landings for the
#       same window (one definition of each).
#   (4) web/src/fleetDecisions.ts's `decisionsFromApi`, under node alone,
#       turns the 7d answer into the page's DecisionWindow: the ladder and
#       judgements labelled "7 d", the counts, human per agent-hour, landed
#       per day, escalations, and each mission's idle hours with M2 marked
#       idle.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on rev-b's df2021c3 (and on 13fc93a7): there is no
# `spawn_decisions` view. It passes once the views, the API and the page's
# adapter exist as objective 043 defines them.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no tmux, no
# network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "043: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_043_health_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

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
        echo "043: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "043: harness: unexpected exit $rc" >&2
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

fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
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

# --- the history ----------------------------------------------------------------

U_ISO=2026-10-31T00:00:00Z
cat >"$WORK/seed.sql" <<'SQL'
\set ON_ERROR_STOP 1
begin;
-- Hours before U = 2026-10-31T00:00:00Z, in ms.
create function pg_temp.ms(h numeric) returns bigint language sql as $$
    select (extract(epoch from timestamptz '2026-10-31 00:00:00+00') * 1000)::bigint
           + round(h * 3600000)::bigint $$;
create temp sequence seed_seq;
create function pg_temp.event(h numeric, agent text, session text, execution text, source text,
                              name text, detail jsonb) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values (agent, session, execution, source, null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) $$;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as $$
    select pg_temp.event(h, 'agent_control', 'session_control', null, 'control', 'control.recorded',
                         jsonb_build_object('record', r)) $$;
create function pg_temp.cmd(p text, k text, a text, s text, act jsonb, h numeric) returns void
language sql as $$
    insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                                succeeded, error, created_at_ms, finished_at_ms)
    values (p, k, a, s, act, 'finished', true, null, pg_temp.ms(h), pg_temp.ms(h) + 1000) $$;
create function pg_temp.assign(h numeric, id int, m int, by_ text, brief text) returns void
language sql as $$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', id, 'mission_id', m,
        'kind', 'ephemeral', 'agent_id', 'agent_worker_w_' || id, 'session_id', 'session_worker_w_' || id,
        'brief', brief, 'created_by', by_, 'model', 'default')) $$;
create function pg_temp.judged(h numeric, judge text, state text, extra jsonb) returns void
language sql as $$
    select pg_temp.event(h, 'agent_worker_w_4', 'session_worker_w_4', null, 'judge',
        'observation.recorded',
        jsonb_build_object('observation', jsonb_build_object('state', state, 'judge', judge) || extra)) $$;

select pg_temp.rec(-720, '{"type":"mission.added","id":1,"text":"043 busy","origin":"human","sources":[]}');
select pg_temp.rec(-400, '{"type":"mission.added","id":3,"text":"043 closed","origin":"human","sources":[]}');
select pg_temp.rec(-48, '{"type":"mission.added","id":2,"text":"043 idle","origin":"human","sources":[]}');

select pg_temp.assign(-700, 1, 1, 'spawn_supervisor', 'Task one');
select pg_temp.rec(-700, '{"type":"task.delivered","id":1,"command_id":1}');
select pg_temp.rec(-695, '{"type":"task.reported","id":1,"report":"done","commit":null}');
select pg_temp.rec(-690, '{"type":"task.landed","id":1,"from":"m0","to":"m1"}');
select pg_temp.assign(-360, 6, 3, 'spawn_supervisor', 'Task six');
select pg_temp.rec(-350, '{"type":"task.cancelled","id":6,"by":"human"}');
select pg_temp.assign(-349, 8, 3, 'spawn_supervisor', 'Task six');
select pg_temp.rec(-349, '{"type":"task.delivered","id":8,"command_id":2}');
select pg_temp.rec(-330, '{"type":"task.reported","id":8,"report":"done","commit":null}');
select pg_temp.rec(-320, '{"type":"task.reviewed","id":8,"accepted":true,"note":null}');
select pg_temp.rec(-300, '{"type":"mission.closed","id":3,"state":"done","summary":"043"}');
select pg_temp.assign(-100, 2, 1, 'spawn_supervisor', 'Task two');
select pg_temp.rec(-100, '{"type":"task.delivered","id":2,"command_id":3}');
select pg_temp.rec(-95, '{"type":"task.reported","id":2,"report":"done","commit":null}');
select pg_temp.rec(-90, '{"type":"task.landed","id":2,"from":"m1","to":"m2"}');
select pg_temp.rec(-70, '{"type":"mission.resumed","id":1}');
select pg_temp.assign(-60, 7, 1, 'spawn_supervisor', 'Task seven');
select pg_temp.rec(-60, '{"type":"task.delivered","id":7,"command_id":4}');
select pg_temp.rec(-50, '{"type":"task.aborted","id":7,"reason":"043"}');
select pg_temp.assign(-40, 5, 2, 'human', 'Task five');
select pg_temp.rec(-40, '{"type":"task.delivered","id":5,"command_id":5}');
select pg_temp.rec(-30, '{"type":"task.reported","id":5,"report":"done","commit":null}');
select pg_temp.rec(-24, '{"type":"task.reviewed","id":5,"accepted":true,"note":null}');
select pg_temp.assign(-20, 3, 1, 'spawn_supervisor', 'Task three');
select pg_temp.rec(-20, '{"type":"task.delivered","id":3,"command_id":6}');
select pg_temp.rec(-15, '{"type":"task.reported","id":3,"report":"done","commit":null}');
select pg_temp.rec(-10, '{"type":"task.landed","id":3,"from":"m2","to":"m3"}');
select pg_temp.assign(-5, 4, 1, 'spawn_supervisor', 'Task four');
select pg_temp.rec(-5, '{"type":"task.delivered","id":4,"command_id":7}');

select spawn_control_refold();

select pg_temp.judged(-600, 'rules', 'WORKING', '{}');
select pg_temp.judged(-150, 'jev', 'WORKING', '{"latency_ms":500}');
select pg_temp.judged(-2, 'rules', 'WORKING', '{"reason":"JUDGE_UNAVAILABLE"}');
select pg_temp.judged(-1, 'jev', 'WORKING', '{"latency_ms":300,"rules_state":"WAITING"}');
select pg_temp.judged(-0.5, 'jev', 'WORKING', '{"latency_ms":500,"rules_state":"WORKING"}');

select pg_temp.event(-30, 'agent_worker_w_3', 'session_worker_w_3', 'exec_a', 'bridge', 'execution.started',
                     '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.event(-6, 'agent_worker_w_3', 'session_worker_w_3', 'exec_a', 'bridge', 'execution.ended',
                     '{"cause":"exited","exit_code":0}');
select pg_temp.event(-2, 'agent_worker_w_4', 'session_worker_w_4', 'exec_b', 'bridge', 'execution.started',
                     '{"engine":"claude","workspace":"/w/b"}');

select pg_temp.cmd('spawn-dispatch', 'deliver-1-0', 'agent_worker_w_1', 'session_worker_w_1', '{"type":"start","prompt":"Task one"}', -650);
select pg_temp.cmd('spawn-dispatch', 'deliver-3-0', 'agent_worker_w_3', 'session_worker_w_3', '{"type":"start","prompt":"Task three"}', -30);
select pg_temp.cmd('spawn-dispatch', 'dispatch-nudge-1', 'agent_worker_w_4', 'session_worker_w_4', '{"type":"input","text":"continue"}', -3);
select pg_temp.cmd('spawn-dispatch', 'dispatch-digest-1', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"two items","tags":["session_worker_w_2 report-2-0","session_worker_w_7 stall-7-activity-1"]}', -160);
select pg_temp.cmd('spawn-dispatch', 'dispatch-digest-2', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"two items","tags":["session_worker_w_2 report-2-0","session_worker_w_4 stall-4-activity-1"]}', -2);
select pg_temp.cmd('spawn_supervisor', 'sup-1', 'agent_worker_w_1', 'session_worker_w_1', '{"type":"input","text":"go on"}', -200);
select pg_temp.cmd('spawn_supervisor', 'sup-2', 'agent_worker_w_4', 'session_worker_w_4', '{"type":"input","text":"go on"}', -4);
select pg_temp.cmd('human', 'board-1', 'agent_worker_w_1', 'session_worker_w_1', '{"type":"input","text":"a note"}', -500);
select pg_temp.cmd('spawn', 'cli-1', 'agent_worker_w_3', 'session_worker_w_3', '{"type":"input","text":"a note"}', -12);
select pg_temp.cmd('spawn-liveness', 'live-1', 'agent_supervisor', 'session_supervisor', '{"type":"resume"}', -6);
select pg_temp.cmd('spawn-integrate', 'integrate-1', 'agent_worker_w_3', 'session_worker_w_3', '{"type":"input","text":"merged main"}', -8);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
[ "$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")" = \
    "1:landed 2:landed 3:landed 4:delivered 5:accepted 6:cancelled 7:cancelled 8:accepted" ] ||
    harness "the seeded history folds to unexpected tasks: $(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")"

# window_ms <window>: "<since> <until>" in ms.
U_MS=$(scalar "select (extract(epoch from timestamptz '2026-10-31 00:00:00+00') * 1000)::bigint")
window_ms() {
    case "$1" in
    24h) echo "$((U_MS - 86400000)) $U_MS" ;;
    7d) echo "$((U_MS - 7 * 86400000)) $U_MS" ;;
    30d) echo "$((U_MS - 30 * 86400000)) $U_MS" ;;
    esac
}

# --- (1) the views ------------------------------------------------------------------

for view in spawn_decisions spawn_interventions spawn_outcomes spawn_escalations; do
    [ -n "$(scalar "select to_regclass('$view')")" ] || fail "(1) there is no view $view"
done
# view_counts <view> <column> <window>: `<value>:<n>` pairs in the window, by value.
view_counts() {
    set -- "$1" "$2" $(window_ms "$3")
    scalar "select coalesce(string_agg(v || ':' || n, ' ' order by v), '') from (
              select $2::text as v, count(*) as n from $1
               where at_ms >= $3 and at_ms < $4 group by 1) t"
}
expect_views() { # <window> <decisions> <interventions> <outcomes> <escalations>
    assert_eq "$(view_counts spawn_decisions tier "$1")" "$2" "(1) spawn_decisions by tier over $1"
    assert_eq "$(view_counts spawn_interventions kind "$1")" "$3" "(1) spawn_interventions by kind over $1"
    assert_eq "$(view_counts spawn_outcomes outcome "$1")" "$4" "(1) spawn_outcomes by outcome over $1"
    assert_eq "$(view_counts spawn_escalations "'all'" "$1")" "$5" "(1) spawn_escalations over $1"
}
expect_views 24h "dispatcher:2 human:1 liveness:1 model:2 rules:1 spawn-integrate:1 supervisor:1" \
    "command:1" "accepted:1 landed:1" "all:1"
expect_views 7d "dispatcher:4 human:1 liveness:1 model:3 rules:1 spawn-integrate:1 supervisor:1" \
    "command:1 owner_task:1 resume:1" "aborted:1 accepted:1 landed:2" "all:3"
expect_views 30d "dispatcher:5 human:2 liveness:1 model:3 rules:2 spawn-integrate:1 supervisor:2" \
    "cancel_reassign:1 command:2 owner_task:1 resume:1" "aborted:1 accepted:2 cancelled:1 landed:3" "all:3"

# --- (2) the API, per window -----------------------------------------------------------

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
fleet_api() { # <window> <file>: the status code
    curl -s --noproxy '*' -o "$2" -w '%{http_code}' \
        "http://127.0.0.1:$port/api/scorecard/fleet?window=$1&until=$U_ISO" 2>/dev/null || echo 000
}
# The figures the page shows, in one line, from an answer.
# Numbers to 3 decimals and objects with sorted keys, so the same figures
# print the same whatever the server's number format or key order.
FIGURES='def n: if type == "number" then ((. * 1000 | round) / 1000 | tostring) else tostring end;
         def o: to_entries | sort_by(.key) | map("\(.key)=\(.value | n)") | join(",");
         [.window, (.ladder.rules | n), (.ladder.model | n), (.ladder.dispatcher | n),
          (.ladder.supervisor | n), (.ladder.human | n), (.ladder.liveness | n),
          (.ladder_extra | map("\(.[0])=\(.[1] | n)") | join(",")), (.agent_hours | n),
          (.human_per_agent_hour | n), (.escalations | n), (.interventions.total | n),
          (.interventions.by_kind | o), (.outcomes | o), (.landed_per_day | n),
          (.judgements.total | n), (.judgements.by_judge | o), (.judgements.judge_unavailable | n),
          (.judgements.disagreements | n), (.judgements.model_latency_p50_ms | n),
          (.missions | map("\(.mission):\(.idle_hours | n)/\(.hours | n):\(.idle)") | join(","))]
         | join(" ")'
expect_api() { # <window> <figures>
    code=$(fleet_api "$1" "$WORK/api.json")
    [ "$code" = 200 ] || fail "(2) GET /api/scorecard/fleet?window=$1 answered $code: $(head -c 300 "$WORK/api.json" 2>/dev/null)"
    set -- "$1" "$2" $(window_ms "$1")
    assert_eq "$(jq -r '"\(.since_ms) \(.until_ms)"' "$WORK/api.json")" "$3 $4" "(2) the $1 answer's since_ms and until_ms"
    assert_eq "$(jq -r "$FIGURES" "$WORK/api.json")" "$2" "(2) the $1 answer's figures"
    cp "$WORK/api.json" "$WORK/api-$1.json"
}
expect_api 24h '24h 1 2 2 1 1 1 spawn-integrate=1 20 0.05 1 1 cancel_reassign=0,command=1,owner_task=0,resume=0 aborted=0,accepted=1,cancelled=0,landed=1,over_budget=0 2 3 jev=2,rules=1 1 1 400 1:9/24:false,2:24/24:true'
expect_api 7d '7d 1 3 4 1 1 1 spawn-integrate=1 26 0.038 3 3 cancel_reassign=0,command=1,owner_task=1,resume=1 aborted=1,accepted=1,cancelled=0,landed=2,over_budget=0 0.429 4 jev=3,rules=1 1 1 500 1:133/168:true,2:32/48:true'
expect_api 30d '30d 2 3 5 2 2 1 spawn-integrate=1 26 0.077 3 5 cancel_reassign=1,command=2,owner_task=1,resume=1 aborted=1,accepted=2,cancelled=1,landed=3,over_budget=0 0.167 5 jev=3,rules=2 1 1 500 1:675/720:true,2:32/48:true,3:61/100:true'
code=$(fleet_api 2d "$WORK/bad.json")
assert_eq "$code" 400 "(2) GET /api/scorecard/fleet?window=2d (not a window it knows): $(head -c 200 "$WORK/bad.json")"

# --- (3) one definition of an intervention -----------------------------------------------

for window in 7d 30d; do
    capture "$SPAWN_BIN" scorecard --since "$window" --until "$U_ISO" --json
    [ "$status" -eq 0 ] || harness "spawn scorecard --since $window failed: $(err)"
    assert_eq "$(jq -r '.scopes[] | select(.scope == "fleet") | .dimensions.autonomy.interventions' "$WORK/out.txt")" \
        "$(jq -r '.interventions.total' "$WORK/api-$window.json")" \
        "(3) the scorecard's autonomy interventions and the API's over $window"
    assert_eq "$(jq -r '.scopes[] | select(.scope == "fleet") | .dimensions.throughput.landed' "$WORK/out.txt")" \
        "$(jq -r '.outcomes.landed + .outcomes.accepted' "$WORK/api-$window.json")" \
        "(3) the scorecard's landings and the API's landed plus accepted over $window"
done

# --- (4) the page's adapter, under node alone -------------------------------------------

tree=$controller
cat >"$WORK/adapter.test.mjs" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const tree = process.env.BB_TREE;
const answer = JSON.parse(readFileSync(process.env.BB_ANSWER, "utf8"));
const module = await import(pathToFileURL(`${tree}/web/src/fleetDecisions.ts`).href);

test("decisionsFromApi turns the 7d answer into the page's DecisionWindow", () => {
  assert.equal(typeof module.decisionsFromApi, "function", "fleetDecisions.ts exports decisionsFromApi");
  const window = module.decisionsFromApi(answer);
  assert.equal(window.ladder.window, "7 d");
  assert.equal(window.judgements.window, "7 d");
  assert.deepEqual(window.ladder.counts, { rules: 1, model: 3, dispatcher: 4, supervisor: 1, human: 1, liveness: 1 });
  assert.deepEqual(window.ladder.extra, [["spawn-integrate", 1]]);
  assert.equal(window.ladder.total, 11);
  assert.equal(window.ladder.humanPerAgentHour, 0.038);
  assert.equal(window.ladder.agentHours, 26);
  assert.equal(window.judgements.total, 4);
  assert.equal(window.judgements.jev, 3);
  assert.equal(window.judgements.rules, 1);
  assert.equal(window.judgements.jevP50Ms, 500);
  assert.equal(window.landedPerDay, 0.429);
  assert.equal(window.escalations, 3);
  assert.deepEqual(
    window.idleHoursByMission.map((m) => [m.mission, m.idleHours, m.hours, m.idle]),
    [[1, 133, 168, true], [2, 32, 48, true]],
  );
});
NODE
if ! BB_TREE=$tree BB_ANSWER=$WORK/api-7d.json node --test "$WORK/adapter.test.mjs" >"$WORK/adapter.out" 2>&1; then
    if grep -q 'ERR_MODULE_NOT_FOUND\|ERR_UNKNOWN_FILE_EXTENSION\|SyntaxError' "$WORK/adapter.out"; then
        fail "(4) web/src/fleetDecisions.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/adapter.out" | tr '\n' ' ')"
    fi
    fail "(4) decisionsFromApi does not give the page's figures: $(grep -m 6 -E 'AssertionError|expected|actual|\+ |- ' "$WORK/adapter.out" | tr '\n' ' ' | head -c 600)"
fi

echo "043: the experience views, the fleet API over 24 h, 7 d and 30 d, the scorecard and the page's adapter agree on every figure of a seeded month, and an idle mission shows as idle"
exit 0
