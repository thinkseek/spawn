#!/bin/sh
# Black box: `spawn scorecard` measures the fleet on every dimension at once,
# from the record alone, and says "not enough data" rather than a number
# below its minimum n.
#
# The scenario seeds a fresh database with a known history and no loop at
# all: control records (`control.recorded` events of session_control, written
# straight into spawn_events with the times chosen here, then folded by
# `spawn_control_refold()`), command rows (spawn_commands) and archived
# transcripts (`$SPAWN_WORKSPACE_ROOT/.transcripts/<agent>/<native>[.<n>].jsonl`).
# Nothing else exists: no hand-kept counter could hold the answer.
#
# The window is [2026-10-02T00:00Z, 2026-10-04T00:00Z) (48 h); the previous
# window is the 48 h before it. Times below are hours from the window's
# start (T0).
#
# Mission 1 (gate, accept = gate), mission 2 (no gate, review).
#   previous window, mission 1, resident r1:
#     task 1 (objective 1): assigned -40, delivered -40, reported -38, gated
#       accepted -37 (gate 900 s), landed -37.
#     task 2 (objective 2): assigned -30, rejected -28 (gate 300 s), accepted
#       -26 (gate 900 s), landed -26.
#     task 3 (objective 3): assigned -20, accepted -15 (gate 1500 s), landed -15.
#   window, mission 1:
#     task 4 (objective 11, r1): assigned 1, rejected 4 (gate 300 s),
#       accepted 7 (gate 600 s), landed 7.
#     task 5 (objective 12, ephemeral): assigned 2, conflict 8 (no gate run),
#       accepted 10 (gate 900 s), landed 10.
#     task 6 (objective 13, ephemeral): assigned 20; its delivery fails four
#       times (deliver-6-0 and three retries, a host error, nothing typed):
#       spent. Cancelled 24 (by nobody named: a person).
#     task 7 (objective 13, ephemeral): assigned 24 (the re-issue), accepted
#       27 (gate 600 s), landed 27.
#     task 8 (objective 14, r1): assigned 30, one delivery refused (pane
#       busy) then delivered, gate aborted 32, mission halted 32.2 and resumed
#       32.5, no_room 33, accepted 34 (gate 1200 s), landed 34.
#     task 9 (objective 15, r1): assigned 40, delivered 40, open; one stall
#       digest for it (tag `stall-9-activity-1`, sent twice).
#   window, mission 2:
#     task 10 (ephemeral, created by human): assigned 12, sent back 15,
#       accepted 17.
#     task 11 (ephemeral, created by the supervisor): assigned 18, accepted 20.
#     task 12 (ephemeral, created by human): assigned 46, open.
#   commands: human input to r1 at 5; `spawn` (the CLI's principal) stop to
#   task 6's session at 21, failed; human input to the supervisor at 22.
#   transcripts: one assistant message per line group, deduplicated by
#   message id (a message split over two lines, a copy file repeating one).
#
# Checked, every number exactly, for the fleet and for each mission, over the
# window and, for the fleet, per UTC day: each of the eight dimensions'
# value, n, enough, better, previous and trend, and its parts (see EXPECTED
# below, worked out by hand from the history above). Then: `--mission 2`
# gives mission 2 alone; the text form says "not enough data"; and the web
# server's read-only `/api/scorecard` answers the same JSON as the CLI.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (06cc205, the deployed binary built
# from it): `spawn scorecard` is an unknown command. It passes once `spawn
# scorecard [--mission N] [--since <time>] [--until <time>] [--json]`
# computes the eight dimensions as objective 017 defines them, from the
# event log, the mission records, the command rows and the archived
# transcripts, and `spawn serve` answers `/api/scorecard` with the same JSON.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no tmux, no
# network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "017: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_017_scorecard_$run"
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
        psql -d postgres -tA -c "drop database if exists $SPAWN_DB_NAME with (force)" \
            >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/card.json" ]; then
        echo "017: the scorecard's answer (first 60 lines):" >&2
        head -n 60 "$WORK/card.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "017: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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

cat >"$WORK/seed.sql" <<'SQL'
\set ON_ERROR_STOP 1
begin;
-- Hours from T0 = 2026-10-02T00:00:00Z, in ms.
create function pg_temp.ms(h numeric) returns bigint language sql as $$
    select (extract(epoch from timestamptz '2026-10-02 00:00:00+00') * 1000)::bigint
           + round(h * 3600000)::bigint $$;
create temp sequence seed_seq;
-- One control record at hour h, in the order written.
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), pg_temp.ms(h)) $$;
-- One finished command: (principal, key, agent, session, action, succeeded,
-- error, created at hour h, finished one second later).
create function pg_temp.cmd(p text, k text, a text, s text, act jsonb, ok boolean, e text,
                            h numeric) returns void language sql as $$
    insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                                succeeded, error, created_at_ms, finished_at_ms)
    values (p, k, a, s, act, 'finished', ok, e, pg_temp.ms(h), pg_temp.ms(h) + 1000) $$;

create function pg_temp.assign(h numeric, id int, m int, kind text, agent text, session text,
                               objective int, by_ text) returns void language sql as $$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.assigned', 'id', id, 'mission_id', m,
        'kind', kind, 'agent_id', agent, 'session_id', session,
        'brief', coalesce('Objective ' || objective, 'Task ' || id), 'created_by', by_,
        'objective', objective, 'model', 'default')) $$;
create function pg_temp.gated(h numeric, id int, verdict text, gate_ms bigint) returns void
language sql as $$
    select pg_temp.rec(h, jsonb_build_object('type', 'task.gated', 'id', id, 'verdict', verdict,
        'base', 'b' || id, 'candidate', 'c' || id, 'reason', 'r', 'note', 'fix it',
        'log', '| == candidate: build' || chr(10) || '| ok' || chr(10)
               || case when gate_ms is null then '! the gate gave no verdict'
                       else '| stage gate ' || verdict || ' ' || gate_ms || 'ms total' end)) $$;
create function pg_temp.simple(h numeric, type_ text, id int) returns void language sql as $$
    select pg_temp.rec(h, jsonb_build_object('type', type_, 'id', id)) $$;

select pg_temp.rec(-47, '{"type":"mission.added","id":1,"text":"017 gated","origin":"human","sources":[]}');
select pg_temp.rec(-47, '{"type":"mission.configured","id":1,"gate":"/nonexistent/017","accept":"gate","budget":null,"objectives":null}');
select pg_temp.rec(-47, '{"type":"mission.added","id":2,"text":"017 review","origin":"human","sources":[]}');
select pg_temp.rec(-47, '{"type":"resident.proposed","name":"r1","charter":"c","proposed_by":"human"}');
select pg_temp.rec(-47, '{"type":"resident.approved","name":"r1"}');

-- The previous window.
select pg_temp.assign(-40, 1, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 1, 'spawn-integrate');
select pg_temp.rec(-40, '{"type":"task.delivered","id":1,"command_id":1}');
select pg_temp.rec(-38, '{"type":"task.reported","id":1,"report":"done","commit":null}');
select pg_temp.gated(-37, 1, 'accepted', 900000);
select pg_temp.rec(-37, '{"type":"task.landed","id":1,"from":"m0","to":"m1"}');
select pg_temp.assign(-30, 2, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 2, 'spawn-integrate');
select pg_temp.rec(-30, '{"type":"task.delivered","id":2,"command_id":2}');
select pg_temp.rec(-29, '{"type":"task.reported","id":2,"report":"done","commit":null}');
select pg_temp.gated(-28, 2, 'rejected', 300000);
select pg_temp.rec(-27.5, '{"type":"task.delivered","id":2,"command_id":3}');
select pg_temp.rec(-27, '{"type":"task.reported","id":2,"report":"done","commit":null}');
select pg_temp.gated(-26, 2, 'accepted', 900000);
select pg_temp.rec(-26, '{"type":"task.landed","id":2,"from":"m1","to":"m2"}');
select pg_temp.assign(-20, 3, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 3, 'spawn-integrate');
select pg_temp.rec(-20, '{"type":"task.delivered","id":3,"command_id":4}');
select pg_temp.rec(-16, '{"type":"task.reported","id":3,"report":"done","commit":null}');
select pg_temp.gated(-15, 3, 'accepted', 1500000);
select pg_temp.rec(-15, '{"type":"task.landed","id":3,"from":"m2","to":"m3"}');

-- The window.
select pg_temp.assign(1, 4, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 11, 'spawn-integrate');
select pg_temp.rec(1, '{"type":"task.delivered","id":4,"command_id":5}');
select pg_temp.assign(2, 5, 1, 'ephemeral', 'agent_worker_b_5', 'session_worker_b_5', 12, 'spawn-integrate');
select pg_temp.rec(2, '{"type":"task.delivered","id":5,"command_id":6}');
select pg_temp.rec(3, '{"type":"task.reported","id":4,"report":"done","commit":null}');
select pg_temp.gated(4, 4, 'rejected', 300000);
select pg_temp.rec(4.5, '{"type":"task.delivered","id":4,"command_id":7}');
select pg_temp.rec(5, '{"type":"task.reported","id":5,"report":"done","commit":null}');
select pg_temp.rec(6, '{"type":"task.reported","id":4,"report":"done","commit":null}');
select pg_temp.gated(7, 4, 'accepted', 600000);
select pg_temp.rec(7, '{"type":"task.landed","id":4,"from":"m3","to":"m4"}');
select pg_temp.gated(8, 5, 'conflict', null);
select pg_temp.rec(8.5, '{"type":"task.delivered","id":5,"command_id":8}');
select pg_temp.rec(9, '{"type":"task.reported","id":5,"report":"done","commit":null}');
select pg_temp.gated(10, 5, 'accepted', 900000);
select pg_temp.rec(10, '{"type":"task.landed","id":5,"from":"m4","to":"m5"}');
select pg_temp.assign(12, 10, 2, 'ephemeral', 'agent_worker_g_10', 'session_worker_g_10', null, 'human');
select pg_temp.rec(12, '{"type":"task.delivered","id":10,"command_id":9}');
select pg_temp.rec(14, '{"type":"task.reported","id":10,"report":"done","commit":null}');
select pg_temp.rec(15, '{"type":"task.reviewed","id":10,"accepted":false,"note":"redo it"}');
select pg_temp.rec(15, '{"type":"task.delivered","id":10,"command_id":10}');
select pg_temp.rec(16, '{"type":"task.reported","id":10,"report":"done","commit":null}');
select pg_temp.rec(17, '{"type":"task.reviewed","id":10,"accepted":true,"note":null}');
select pg_temp.assign(18, 11, 2, 'ephemeral', 'agent_worker_h_11', 'session_worker_h_11', null, 'spawn_supervisor');
select pg_temp.rec(18, '{"type":"task.delivered","id":11,"command_id":11}');
select pg_temp.rec(19, '{"type":"task.reported","id":11,"report":"done","commit":null}');
select pg_temp.rec(20, '{"type":"task.reviewed","id":11,"accepted":true,"note":null}');
select pg_temp.assign(20, 6, 1, 'ephemeral', 'agent_worker_c_6', 'session_worker_c_6', 13, 'spawn-integrate');
select pg_temp.simple(24, 'task.cancelled', 6);
select pg_temp.assign(24, 7, 1, 'ephemeral', 'agent_worker_c_7', 'session_worker_c_7', 13, 'spawn-integrate');
select pg_temp.rec(24, '{"type":"task.delivered","id":7,"command_id":12}');
select pg_temp.rec(26, '{"type":"task.reported","id":7,"report":"done","commit":null}');
select pg_temp.gated(27, 7, 'accepted', 600000);
select pg_temp.rec(27, '{"type":"task.landed","id":7,"from":"m5","to":"m6"}');
select pg_temp.assign(30, 8, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 14, 'spawn-integrate');
select pg_temp.rec(30.01, '{"type":"task.delivered","id":8,"command_id":13}');
select pg_temp.rec(31, '{"type":"task.reported","id":8,"report":"done","commit":null}');
select pg_temp.gated(32, 8, 'aborted', null);
select pg_temp.rec(32.2, '{"type":"mission.halted","id":1,"reason":"3 gate aborts in a row"}');
select pg_temp.rec(32.5, '{"type":"mission.resumed","id":1}');
select pg_temp.rec(33, '{"type":"task.gated","id":8,"verdict":"no_room","free_kb":1,"min_gb":10,"tmpdir":"/t","reason":"no room to judge now","log":"","escalate":false}');
select pg_temp.gated(34, 8, 'accepted', 1200000);
select pg_temp.rec(34, '{"type":"task.landed","id":8,"from":"m6","to":"m7"}');
select pg_temp.assign(40, 9, 1, 'resident', 'agent_resident_r1', 'session_resident_r1', 15, 'spawn-integrate');
select pg_temp.rec(40, '{"type":"task.delivered","id":9,"command_id":14}');
select pg_temp.assign(46, 12, 2, 'ephemeral', 'agent_worker_i_12', 'session_worker_i_12', null, 'human');

select spawn_control_refold();

-- The commands.
select pg_temp.cmd('spawn-dispatch', 'deliver-1-0', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #1"}', true, null, -40);
select pg_temp.cmd('spawn-dispatch', 'deliver-4-0', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #4"}', true, null, 1);
select pg_temp.cmd('human', 'board-1', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"a note from the board"}', true, null, 5);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_1 failed: command too long', 20 + 10 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0-retry-1', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_2 failed: command too long', 20 + 80 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0-retry-2', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_3 failed: command too long', 20 + 210 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-6-0-retry-3', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"start","engine":"claude","prompt":"Task #6"}', false,
                   'start terminal: new-session for spawn-exec_4 failed: command too long', 20 + 460 / 3600.0);
select pg_temp.cmd('spawn', 'cli-stop-1', 'agent_worker_c_6', 'session_worker_c_6',
                   '{"type":"stop"}', false, 'session_worker_c_6 runs no execution', 21);
select pg_temp.cmd('human', 'board-2', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"how is mission 1 doing?"}', true, null, 22);
select pg_temp.cmd('spawn-dispatch', 'deliver-7-0', 'agent_worker_c_7', 'session_worker_c_7',
                   '{"type":"start","engine":"claude","prompt":"Task #7"}', true, null, 24);
select pg_temp.cmd('spawn-dispatch', 'deliver-8-0', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #8"}', false, 'pane state Busy does not accept input',
                   30 + 5 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'deliver-8-0-retry-1', 'agent_resident_r1', 'session_resident_r1',
                   '{"type":"input","text":"Task #8"}', true, null, 30 + 20 / 3600.0);
select pg_temp.cmd('spawn-dispatch', 'dispatch-digest-1-a', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"stalled: task #9","tags":["session_resident_r1 stall-9-activity-1"]}',
                   true, null, 45);
select pg_temp.cmd('spawn-dispatch', 'dispatch-digest-2-b', 'agent_supervisor', 'session_supervisor',
                   '{"type":"input","text":"stalled: task #9","tags":["session_resident_r1 stall-9-activity-1","session_worker_i_12 report-12-0"]}',
                   true, null, 45.5);
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
[ "$(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")" = \
    "1:landed 2:landed 3:landed 4:landed 5:landed 6:cancelled 7:landed 8:landed 9:delivered 10:accepted 11:accepted 12:assigned" ] ||
    harness "the seeded history folds to unexpected tasks: $(scalar "select string_agg(id || ':' || state, ' ' order by id) from mission_tasks")"

# The transcripts. One assistant message may span several lines (one per
# content block, each repeating the usage), and a copy file may repeat a line:
# a message counts once, by its id.
tr_dir=$SPAWN_WORKSPACE_ROOT/.transcripts
msg() { # <iso time> <message id> <model> <input> <cache creation> <cache read> <output>
    printf '{"type":"assistant","timestamp":"%s","sessionId":"s","message":{"id":"%s","model":"%s","role":"assistant","usage":{"input_tokens":%s,"cache_creation_input_tokens":%s,"cache_read_input_tokens":%s,"output_tokens":%s}}}\n' \
        "$@"
}
user_line() { printf '{"type":"user","timestamp":"%s","sessionId":"s","message":{"role":"user","content":"go"}}\n' "$1"; }
mkdir -p "$tr_dir/agent_resident_r1" "$tr_dir/agent_worker_b_5" "$tr_dir/agent_worker_c_7" \
    "$tr_dir/agent_worker_g_10" "$tr_dir/agent_worker_h_11" "$tr_dir/agent_supervisor"
native=0a0a0a0a-0000-4000-8000-000000000001
{
    msg 2026-09-30T09:00:00.000Z msg_p1 claude-opus-5-5 0 0 0 3000
    user_line 2026-10-02T01:30:00.000Z
    msg 2026-10-02T02:00:00.000Z msg_a1 claude-opus-5-5 100 1000 5000 400
    msg 2026-10-02T02:00:01.000Z msg_a1 claude-opus-5-5 100 1000 5000 400
    msg 2026-10-03T07:00:00.000Z msg_e1 claude-sonnet-5-5 10 0 2000 90
    msg 2026-10-03T14:00:00.000Z msg_x1 claude-opus-5-5 0 0 0 1000
} >"$tr_dir/agent_resident_r1/$native.jsonl"
{
    msg 2026-10-03T07:00:00.000Z msg_e1 claude-sonnet-5-5 10 0 2000 90
    msg 2026-10-03T17:00:00.000Z msg_f1 claude-opus-5-5 50 0 1000 50
} >"$tr_dir/agent_resident_r1/$native.1.jsonl"
msg 2026-10-02T03:00:00.000Z msg_b1 claude-opus-5-5 200 0 3000 300 \
    >"$tr_dir/agent_worker_b_5/0a0a0a0a-0000-4000-8000-000000000002.jsonl"
msg 2026-10-03T01:00:00.000Z msg_d1 claude-sonnet-5-5 20 500 1000 80 \
    >"$tr_dir/agent_worker_c_7/0a0a0a0a-0000-4000-8000-000000000003.jsonl"
msg 2026-10-02T13:00:00.000Z msg_g1 claude-sonnet-5-5 30 0 0 70 \
    >"$tr_dir/agent_worker_g_10/0a0a0a0a-0000-4000-8000-000000000004.jsonl"
msg 2026-10-02T19:00:00.000Z msg_h1 claude-haiku-5-5 5 0 0 15 \
    >"$tr_dir/agent_worker_h_11/0a0a0a0a-0000-4000-8000-000000000005.jsonl"
{
    msg 2026-10-02T22:00:00.000Z msg_s1 claude-opus-5-5 10 0 0 90
    printf '{"type":"assistant","timestamp":"2026-10-02T22:00:02.000Z","sessionId":"s","message":{"id":"msg_s2","model":"<synthetic>","role":"assistant","content":[]}}\n'
    msg 2026-10-04T02:00:00.000Z msg_s3 claude-opus-5-5 0 0 0 99999
} >"$tr_dir/agent_supervisor/0a0a0a0a-0000-4000-8000-000000000006.jsonl"

# --- (1) the scorecard answers --------------------------------------------------

SINCE=2026-10-02T00:00:00Z
UNTIL=2026-10-04T00:00:00Z
capture "$SPAWN_BIN" scorecard --since "$SINCE" --until "$UNTIL" --json
cp "$WORK/out.txt" "$WORK/card.json"
[ "$status" -eq 0 ] ||
    fail "(1) \`spawn scorecard --since $SINCE --until $UNTIL --json\` exited $status: $(err | head -c 400)"
jq -e . "$WORK/card.json" >/dev/null 2>&1 || fail "(1) the scorecard's answer is not JSON: $(head -c 400 "$WORK/card.json")"

# check <file> <jq path> <expected JSON value> <what>: the path must give
# exactly one value, compared as a JSON value (1 equals 1.0; a missing field
# is not null).
check() {
    got=$(jq -c "[ $2 ]" "$1" 2>/dev/null) || got="(no such path)"
    case "$got" in
    "[]" | "(no such path)") fail "$4: $2 is missing, expected $3" ;;
    esac
    jq -e -n --argjson got "$got" --argjson want "$3" \
        '($got | length) == 1 and $got[0] == $want' >/dev/null 2>&1 ||
        fail "$4: $2 is ${got#[}, expected $3"
}
card=$WORK/card.json
check "$card" '.min_n' 3 "(1) the minimum n"
check "$card" '.since_ms' "$(scalar "select (extract(epoch from timestamptz '2026-10-02 00:00:00+00') * 1000)::bigint")" "(1) the window's start"
check "$card" '.until_ms' "$(scalar "select (extract(epoch from timestamptz '2026-10-04 00:00:00+00') * 1000)::bigint")" "(1) the window's end"
check "$card" '.previous_since_ms' "$(scalar "select (extract(epoch from timestamptz '2026-09-30 00:00:00+00') * 1000)::bigint")" "(1) the previous window's start"
check "$card" '[.scopes[] | [.scope, .mission]]' '[["fleet",null],["mission",1],["mission",2]]' "(1) the scopes"

# --- (2) every number, over the window -------------------------------------------
#
# EXPECTED, one line per field: <scope> <dimension> <field> <JSON value>.
# scope: fleet, m1, m2; day1, day2 are the fleet's two days.
# Worked out by hand (hours from T0):
#   landings: fleet 6 (tasks 4@7 5@10 10@17 11@20 7@27 8@34), m1 4, m2 2;
#     previous: 3 (tasks 1 2 3), all mission 1.
#   open hours (a task is open from its assignment to its landing, final
#     acceptance or cancellation; the union, clipped to the window): fleet
#     [1,10]+[12,17]+[18,27]+[30,34]+[40,48] = 35; m1 [1,10]+[20,27]+[30,34]+
#     [40,48] = 28; m2 [12,17]+[18,20]+[46,48] = 9; previous 3+4+5 = 12.
#   lead times (h; task 7 from task 6's assignment, the objective's first
#     try): fleet 2 4 5 6 7 8 -> median (nearest rank) 5, p90 8; m1 4 6 7 8
#     -> 6, 8; previous 3 4 5 -> 4.
#   send-backs: tasks 4 (rejected@4), 5 (conflict@8), 10 (sent back@15);
#     previous 1. First pass: tasks 11 7 8 of the fleet's 6, 7 8 of m1's 4;
#     previous 2 of 3.
#   interventions: human input@5 (task 4's session: m1), `spawn` stop@21
#     (task 6's: m1), human input to the supervisor@22 (fleet only), owner
#     tasks 10@12 and 12@46 (m2, a review mission), cancel-and-reassign
#     6->7@24 (m1), resume@32.5 (m1): fleet 7, m1 4, m2 2; previous 0.
#   reliability: failed commands deliver-6-0 and its 3 retries, the `spawn`
#     stop, deliver-8-0 (6); spent deliveries deliver-6-0 (1); stalls
#     stall-9-activity-1 (1, sent twice); infrastructure gate outcomes
#     aborted@32, no_room@33 (2): fleet 10, all of them m1's; previous 0.
#   tokens (input + cache creation + cache read + output, one per message
#     id, by line time; a mission's are its tasks' agents' between the task's
#     first delivery and its close): r1 6500@2 (task 4) 2100@31 (task 8)
#     1000@38 (no task) 1100@41 (task 9); b_5 3500@3; c_7 1600@25; g_10
#     100@13; h_11 20@19; supervisor 100@22 (fleet only); 99999@50 is after
#     the window. Fleet 16020 (opus 12200, sonnet 3800, haiku 20), m1 14800
#     (opus 11100, sonnet 3700), m2 120; previous 3000.
#   gate runs (the log's `stage gate <result> <ms>ms total`): 300000@4
#     600000@7 900000@10 600000@27 1200000@34 -> median 600000, machine time
#     3600000; previous 900000 300000 900000 1500000 -> median 900000.
cat >"$WORK/expected" <<'EOF'
fleet throughput value 0.171
fleet throughput n 6
fleet throughput landed 6
fleet throughput open_hours 35
fleet throughput better "higher"
fleet throughput enough true
fleet throughput previous 0.25
fleet throughput trend "worse"
fleet lead_time value 18000000
fleet lead_time p90 28800000
fleet lead_time n 6
fleet lead_time better "lower"
fleet lead_time previous 14400000
fleet lead_time trend "worse"
fleet first_pass value 0.5
fleet first_pass first_pass 3
fleet first_pass n 6
fleet first_pass better "higher"
fleet first_pass previous 0.667
fleet first_pass trend "worse"
fleet rework value 0.5
fleet rework send_backs 3
fleet rework by_verdict {"rejected":1,"conflict":1,"incompatible":0,"reviewed":1}
fleet rework n 6
fleet rework better "lower"
fleet rework previous 0.333
fleet rework trend "worse"
fleet autonomy value 1.167
fleet autonomy interventions 7
fleet autonomy by_kind {"command":3,"resume":1,"owner_task":2,"cancel_reassign":1}
fleet autonomy n 6
fleet autonomy better "lower"
fleet autonomy previous 0
fleet autonomy trend "worse"
fleet reliability value 1.667
fleet reliability events 10
fleet reliability by_kind {"failed_command":6,"spent_delivery":1,"stall":1,"gate_infrastructure":2}
fleet reliability n 6
fleet reliability better "lower"
fleet reliability previous 0
fleet reliability trend "worse"
fleet cost value 2670
fleet cost tokens 16020
fleet cost by_model {"claude-opus-5-5":{"tokens":12200,"per_landed":2033.333},"claude-sonnet-5-5":{"tokens":3800,"per_landed":633.333},"claude-haiku-5-5":{"tokens":20,"per_landed":3.333}}
fleet cost gate_ms 3600000
fleet cost gate_ms_per_landed 600000
fleet cost n 6
fleet cost better "lower"
fleet cost previous 1000
fleet cost trend "worse"
fleet gate_time value 600000
fleet gate_time runs 5
fleet gate_time n 5
fleet gate_time better "lower"
fleet gate_time previous 900000
fleet gate_time trend "better"
m1 throughput value 0.143
m1 throughput n 4
m1 throughput open_hours 28
m1 throughput previous 0.25
m1 throughput trend "worse"
m1 lead_time value 21600000
m1 lead_time p90 28800000
m1 lead_time n 4
m1 lead_time previous 14400000
m1 lead_time trend "worse"
m1 first_pass value 0.5
m1 first_pass first_pass 2
m1 first_pass previous 0.667
m1 rework value 0.5
m1 rework by_verdict {"rejected":1,"conflict":1,"incompatible":0,"reviewed":0}
m1 rework previous 0.333
m1 autonomy value 1
m1 autonomy interventions 4
m1 autonomy by_kind {"command":2,"resume":1,"owner_task":0,"cancel_reassign":1}
m1 autonomy previous 0
m1 reliability value 2.5
m1 reliability events 10
m1 reliability by_kind {"failed_command":6,"spent_delivery":1,"stall":1,"gate_infrastructure":2}
m1 cost value 3700
m1 cost tokens 14800
m1 cost by_model {"claude-opus-5-5":{"tokens":11100,"per_landed":2775},"claude-sonnet-5-5":{"tokens":3700,"per_landed":925}}
m1 cost gate_ms 3600000
m1 cost gate_ms_per_landed 900000
m1 cost previous 1000
m1 cost trend "worse"
m1 gate_time value 600000
m1 gate_time n 5
m1 gate_time previous 900000
m1 gate_time trend "better"
m2 throughput value null
m2 throughput enough false
m2 throughput n 2
m2 throughput landed 2
m2 throughput open_hours 9
m2 throughput previous null
m2 throughput trend null
m2 lead_time value null
m2 lead_time p90 null
m2 lead_time n 2
m2 lead_time enough false
m2 first_pass value null
m2 first_pass first_pass 1
m2 first_pass n 2
m2 rework value null
m2 rework send_backs 1
m2 rework by_verdict {"rejected":0,"conflict":0,"incompatible":0,"reviewed":1}
m2 autonomy value null
m2 autonomy interventions 2
m2 autonomy by_kind {"command":0,"resume":0,"owner_task":2,"cancel_reassign":0}
m2 autonomy n 2
m2 autonomy enough false
m2 reliability value null
m2 reliability events 0
m2 reliability n 2
m2 cost value null
m2 cost tokens 120
m2 cost n 2
m2 cost enough false
m2 gate_time value null
m2 gate_time n 0
m2 gate_time enough false
m2 gate_time previous null
m2 gate_time trend null
day1 throughput value 0.2
day1 throughput n 4
day1 throughput open_hours 20
day1 lead_time value 18000000
day1 lead_time p90 28800000
day1 lead_time n 4
day1 first_pass value 0.25
day1 rework value 0.75
day1 autonomy value 1
day1 autonomy interventions 4
day1 reliability value 1.5
day1 reliability events 6
day1 cost value 2555
day1 cost tokens 10220
day1 cost gate_ms 1800000
day1 cost gate_ms_per_landed 450000
day1 gate_time value 600000
day1 gate_time n 3
day1 gate_time enough true
day2 throughput value null
day2 throughput n 2
day2 throughput open_hours 15
day2 throughput enough false
day2 lead_time value null
day2 first_pass value null
day2 rework value null
day2 autonomy value null
day2 autonomy interventions 3
day2 reliability value null
day2 reliability events 4
day2 cost value null
day2 gate_time value null
day2 gate_time n 2
day2 gate_time enough false
EOF

path_of() { # <scope> <dimension>: the jq path of that dimension
    case "$1" in
    fleet) echo ".scopes[] | select(.scope == \"fleet\") | .dimensions.$2" ;;
    m1) echo ".scopes[] | select(.scope == \"mission\" and .mission == 1) | .dimensions.$2" ;;
    m2) echo ".scopes[] | select(.scope == \"mission\" and .mission == 2) | .dimensions.$2" ;;
    day1) echo ".scopes[] | select(.scope == \"fleet\") | .days[] | select(.day == \"2026-10-02\") | .dimensions.$2" ;;
    day2) echo ".scopes[] | select(.scope == \"fleet\") | .days[] | select(.day == \"2026-10-03\") | .dimensions.$2" ;;
    *) harness "unknown scope $1" ;;
    esac
}
check "$card" '[.scopes[] | select(.scope == "fleet") | .days[].day]' '["2026-10-02","2026-10-03"]' \
    "(2) the fleet's days"
while read -r scope dimension field want; do
    [ -n "$scope" ] || continue
    # The field is there (a null is written, not left out), then its value.
    check "$card" "$(path_of "$scope" "$dimension") | has(\"$field\")" true \
        "(2) $scope $dimension has a field $field"
    check "$card" "$(path_of "$scope" "$dimension") | .$field" "$want" "(2) $scope $dimension $field"
done <"$WORK/expected"

# --- (3) one mission alone, and the words "not enough data" ------------------------

capture "$SPAWN_BIN" scorecard --mission 2 --since "$SINCE" --until "$UNTIL" --json
[ "$status" -eq 0 ] || fail "(3) \`spawn scorecard --mission 2 ... --json\` exited $status: $(err | head -c 300)"
cp "$WORK/out.txt" "$WORK/m2.json"
check "$WORK/m2.json" '[.scopes[] | [.scope, .mission]]' '[["mission",2]]' "(3) the scopes of --mission 2"
check "$WORK/m2.json" '.scopes[0].dimensions.cost.tokens' 120 "(3) mission 2's tokens, asked alone"
capture "$SPAWN_BIN" scorecard --mission 2 --since "$SINCE" --until "$UNTIL"
[ "$status" -eq 0 ] || fail "(3) \`spawn scorecard --mission 2\` (text) exited $status: $(err | head -c 300)"
assert_contains "$(out)" "not enough data" "(3) the text scorecard of mission 2 (2 landings, under the minimum n of 3)"
if out | grep -E 'lead' | grep -qE '[0-9]+(\.[0-9]+)? ?(h|ms|min)'; then
    fail "(3) the text scorecard of mission 2 shows a lead time below the minimum n: $(out | grep -E 'lead')"
fi

# --- (4) the dashboard's Scorecard reads the same numbers ---------------------------

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
code=""
i=0
while [ "$i" -lt 50 ]; do
    i=$((i + 1))
    code=$(curl -s -o "$WORK/api.json" -w '%{http_code}' \
        "http://127.0.0.1:$port/api/scorecard?since=$SINCE&until=$UNTIL" 2>/dev/null) || code=""
    [ -n "$code" ] && [ "$code" != 000 ] && break
    sleep 0.1
done
[ "$code" = 200 ] || fail "(4) GET /api/scorecard answered $code: $(head -c 300 "$WORK/api.json" 2>/dev/null)"
jq -S . "$WORK/api.json" >"$WORK/api.sorted" 2>/dev/null ||
    fail "(4) /api/scorecard is not JSON: $(head -c 300 "$WORK/api.json")"
jq -S . "$card" >"$WORK/card.sorted"
cmp -s "$WORK/api.sorted" "$WORK/card.sorted" ||
    fail "(4) /api/scorecard differs from \`spawn scorecard --json\` for the same window: $(diff "$WORK/card.sorted" "$WORK/api.sorted" | head -n 12 | tr '\n' ' ')"

echo "017: the scorecard measured every dimension for the fleet, each mission and each day, from the record alone, said 'not enough data' under the minimum n, and the dashboard read the same numbers"
exit 0
