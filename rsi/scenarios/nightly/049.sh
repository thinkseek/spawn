#!/bin/sh
# Black box: the north star counts what a person meant, once. A person's
# command refused and sent again is one intervention, not one per attempt;
# `GET /api/interventions` gives the figure per agent-hour for 24 h and 7 d
# with the list of interventions behind it, and every count of a person's
# commands (the scorecard's autonomy, `/api/fleet`'s ladder and human per
# agent-hour, 043's views) counts intents.
#
# Today each command counts: in the week to 2026-11-11 one owner note was
# refused 99 times while its pane was busy and typed once, and the 100
# commands were 100 interventions (228 in all that week). Required (the
# objective's prompt says each rule): a person's command (principal `human`
# or `spawn`) continues the intent of the latest earlier command of the
# same principal, session and action (the whole JSON) when that one finished
# failed at or before this one was created, at most 60 minutes before it;
# otherwise it starts one. An intent counts at its first command's
# creation; its outcome is its last command's (taken, failed, pending).
#
# The scenario seeds a fresh database with no loop, as 043 does: command
# rows, executions and one mission's resume, at hours before U, U being the
# database's now (to the second) when it seeds, so `/api/fleet`'s own 24 h
# sees the same history. Every command row finishes 1 s after its creation
# unless said otherwise. Hours before U:
#   executions: w_1's from -30 to -6; w_2's from -2, still running.
#   mission 1 added -200, resumed (mission.resumed) -50.
#   commands (principal key session action, at; ok/failed/pending):
#     c1  human board-1  w_1 input "note A <130 x's>"       -100   ok
#     c2  human board-2  w_1 input "note B"                 -10    failed
#     c3  spawn-dispatch d-1 w_1 input "note B"             -9.95  failed
#     c4  human board-3  w_1 input "note B"                 -9.9   failed
#     c5  human board-4  w_1 input "note B"                 -9.8   failed
#     c6  human board-5  w_1 input "note B"                 -9.7   failed
#     c7  human board-6  w_1 input "note B"                 -9.6   ok
#     c8  human board-7  w_1 input "note B"                 -9.2   ok
#     c9  spawn cli-1    w_2 input "note C"                 -3     failed
#     c10 spawn cli-2    w_2 input "note C"                 -1.5   failed
#     c11 human board-8  w_2 input "note D"                 -1     failed
#     c12 spawn cli-3    w_2 input "note D"                 -0.95  ok
#     c13 human board-9  w_1 input "note D"                 -0.9   ok
#     c14 human board-10 w_2 stop                           -0.7   failed
#     c15 human board-11 w_2 stop                           -0.6   ok
#     c16 human board-12 w_1 input "note F"                 -0.45  failed at -0.35
#     c17 human board-13 w_1 input "note F"                 -0.4   ok
#     c18 human board-14 w_2 input "note E\nsecond line"    -0.3   failed
#     c19 human board-15 w_2 input "note E\nsecond line"    -0.2   pending
# Intents (rule (a), worked by hand):
#   I1 [c1] -100 taken; I2 [c2 c4 c5 c6 c7] -10 taken (c3 is the
#   dispatcher's: not a person's, and no break in the chain); I3 [c8] -9.2
#   taken (sent again 24 min after it was taken); I4 [c9] -3 failed; I5 [c10] -1.5
#   failed (90 min after c9's failure: over 60); I6 [c11] -1 failed; I7
#   [c12] -0.95 taken (another principal); I8 [c13] -0.9 taken (another
#   session); I9 [c14 c15] -0.7 taken (stop); I10 [c16] -0.45 failed; I11
#   [c17] -0.4 taken (c16 had not finished when c17 was created); I12 [c18
#   c19] -0.3 pending.
#   24h: 11 interventions (command 11), the 17 commands less 11: 6 retries;
#        agent hours 18 + 2 = 20; per agent-hour 0.55.
#   7d:  13 (command 12, resume 1), 6 retries; agent hours 24 + 2 = 26;
#        per agent-hour 0.5.
# Checked:
#   (1) `/api/interventions?window=24h|7d&until=U`: those figures, and the
#       items newest first, each with its kind, agent, line (an input's first
#       line, at most 120 characters; "stop"; "resumed mission #1"), its
#       command ids in order, attempts and outcome; 400 for window=2d.
#   (2) every figure equals this scenario's own SQL recount of rule (a) over
#       spawn_commands and of the executions' hours.
#   (3) `spawn scorecard --json` over the same windows: autonomy's
#       interventions, `by_kind.command` and `retries` are the API's.
#   (4) `/api/fleet`: `ladder_24h.human` is 11 and `human_per_agent_hour` is
#       11 over its own `agent_hours_24h`.
#   (5) 043's views, when the tree has them (049 comes after 043, so at the
#       gate it does): `spawn_interventions` kind `command` and
#       `spawn_decisions` tier `human` hold one row per intent in each
#       window.
#   (6) web/src/interventions.ts's `interventionsView`, under node alone,
#       gives the page's words for both answers and for fixed cases.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# error exits 2, never 1.
#
# Fails with 1 at (1) on the live main (ea3a2e9f): there is no
# `/api/interventions` (404). It passes once intents are counted as above,
# everywhere a person's commands are counted, and the page says so.
# Runs inside the gate's sandbox (scripts/rsi/gate.sh): needs SPAWN_BIN and
# SPAWN_TEST_DATABASE_URL, creates and drops its own database, no tmux, no
# network but loopback. Every wait is bounded in time.
set -eu

asserted=""
harness() {
    echo "049: harness: $*" >&2
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
for tool in psql jq od curl sed grep node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME in WORK: nothing here reads or writes the real home, nor
# is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
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
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_049_intents_$run"
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
    set +e
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "049: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "049: harness: unexpected exit $rc" >&2
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

U_MS=$(scalar "select (floor(extract(epoch from clock_timestamp())) * 1000)::bigint")
case "$U_MS" in '' | *[!0-9]*) harness "no database time: [$U_MS]" ;; esac
U_ISO=$(date -u -d "@$((U_MS / 1000))" +%Y-%m-%dT%H:%M:%SZ)
LONG=$(printf 'x%.0s' $(seq 1 130))
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
-- Hours before U, in ms.
create function pg_temp.ms(h numeric) returns bigint language sql as \$\$
    select $U_MS::bigint + round(h * 3600000)::bigint \$\$;
create temp sequence seed_seq;
create function pg_temp.event(h numeric, agent text, session text, execution text, source text,
                              name text, detail jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values (agent, session, execution, source, null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) \$\$;
create function pg_temp.rec(h numeric, r jsonb) returns void language sql as \$\$
    select pg_temp.event(h, 'agent_control', 'session_control', null, 'control', 'control.recorded',
                         jsonb_build_object('record', r)) \$\$;
-- cmd <principal> <key> <worker> <action> <h> <ok|failed|pending> [<h finished>]
create function pg_temp.cmd(p text, k text, w text, act jsonb, h numeric, how text,
                            done numeric default null) returns void language sql as \$\$
    insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action, state,
                                succeeded, error, created_at_ms, finished_at_ms)
    values (p, k, 'agent_worker_' || w, 'session_worker_' || w, act,
            case when how = 'pending' then 'pending' else 'finished' end,
            case how when 'ok' then true when 'failed' then false end,
            case when how = 'failed' then 'pane state Busy does not accept input' end,
            pg_temp.ms(h),
            case when how = 'pending' then null
                 else coalesce(pg_temp.ms(done), pg_temp.ms(h) + 1000) end) \$\$;
create function pg_temp.input(t text) returns jsonb language sql as \$\$
    select jsonb_build_object('type', 'input', 'text', t) \$\$;

select pg_temp.rec(-200, '{"type":"mission.added","id":1,"text":"049 mission","origin":"human","sources":[]}');
select pg_temp.rec(-50, '{"type":"mission.resumed","id":1}');
select spawn_control_refold();

select pg_temp.event(-30, 'agent_worker_w_1', 'session_worker_w_1', 'exec_a', 'bridge', 'execution.started',
                     '{"engine":"claude","workspace":"/w/a"}');
select pg_temp.event(-6, 'agent_worker_w_1', 'session_worker_w_1', 'exec_a', 'bridge', 'execution.ended',
                     '{"cause":"exited","exit_code":0}');
select pg_temp.event(-2, 'agent_worker_w_2', 'session_worker_w_2', 'exec_b', 'bridge', 'execution.started',
                     '{"engine":"claude","workspace":"/w/b"}');

select pg_temp.cmd('human', 'board-1', 'w_1', pg_temp.input('note A $LONG'), -100, 'ok');
select pg_temp.cmd('human', 'board-2', 'w_1', pg_temp.input('note B'), -10, 'failed');
select pg_temp.cmd('spawn-dispatch', 'd-1', 'w_1', pg_temp.input('note B'), -9.95, 'failed');
select pg_temp.cmd('human', 'board-3', 'w_1', pg_temp.input('note B'), -9.9, 'failed');
select pg_temp.cmd('human', 'board-4', 'w_1', pg_temp.input('note B'), -9.8, 'failed');
select pg_temp.cmd('human', 'board-5', 'w_1', pg_temp.input('note B'), -9.7, 'failed');
select pg_temp.cmd('human', 'board-6', 'w_1', pg_temp.input('note B'), -9.6, 'ok');
select pg_temp.cmd('human', 'board-7', 'w_1', pg_temp.input('note B'), -9.2, 'ok');
select pg_temp.cmd('spawn', 'cli-1', 'w_2', pg_temp.input('note C'), -3, 'failed');
select pg_temp.cmd('spawn', 'cli-2', 'w_2', pg_temp.input('note C'), -1.5, 'failed');
select pg_temp.cmd('human', 'board-8', 'w_2', pg_temp.input('note D'), -1, 'failed');
select pg_temp.cmd('spawn', 'cli-3', 'w_2', pg_temp.input('note D'), -0.95, 'ok');
select pg_temp.cmd('human', 'board-9', 'w_1', pg_temp.input('note D'), -0.9, 'ok');
select pg_temp.cmd('human', 'board-10', 'w_2', '{"type":"stop"}', -0.7, 'failed');
select pg_temp.cmd('human', 'board-11', 'w_2', '{"type":"stop"}', -0.6, 'ok');
select pg_temp.cmd('human', 'board-12', 'w_1', pg_temp.input('note F'), -0.45, 'failed', -0.35);
select pg_temp.cmd('human', 'board-13', 'w_1', pg_temp.input('note F'), -0.4, 'ok');
select pg_temp.cmd('human', 'board-14', 'w_2', pg_temp.input(E'note E\nsecond line'), -0.3, 'failed');
select pg_temp.cmd('human', 'board-15', 'w_2', pg_temp.input(E'note E\nsecond line'), -0.2, 'pending');
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
[ "$(scalar "select count(*) from spawn_commands")" = 19 ] || harness "the seed made $(scalar "select count(*) from spawn_commands") commands, not 19"
[ "$(scalar "select state from mission_missions where id = 1")" != "" ] || harness "mission 1 did not fold"

# c <n>: the id of command c<n> (by its key).
KEYS="board-1 board-2 d-1 board-3 board-4 board-5 board-6 board-7 cli-1 cli-2 board-8 cli-3 board-9 board-10 board-11 board-12 board-13 board-14 board-15"
c() {
    key=$(echo $KEYS | cut -d' ' -f"$1")
    scalar "select id from spawn_commands where idempotency_key = '$key'"
}
window_ms() {
    case "$1" in
    24h) echo "$((U_MS - 86400000)) $U_MS" ;;
    7d) echo "$((U_MS - 7 * 86400000)) $U_MS" ;;
    esac
}

# --- (1) the API --------------------------------------------------------------------

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
get() { # <path> <file>: the status code
    bb_http "$2" --noproxy '*' "http://127.0.0.1:$port$1"
}
n3() { awk -v v="$1" 'BEGIN { printf "%.3f\n", v }'; }

# items <window>: one line per item, "<kind>|<agent>|<line>|<ids>|<attempts>|<outcome>".
items() {
    jq -r '.items[] | "\(.kind)|\(.agent // "")|\(.line)|\(.commands | map(tostring) | join(" "))|\(.attempts // "")|\(.outcome // "")"' \
        "$WORK/api-$1.json"
}
LINE_A=$(printf 'note A %s' "$LONG" | cut -c1-120)
expect_24h=$(cat <<EOF
command|agent_worker_w_2|note E|$(c 18) $(c 19)|2|pending
command|agent_worker_w_1|note F|$(c 17)|1|taken
command|agent_worker_w_1|note F|$(c 16)|1|failed
command|agent_worker_w_2|stop|$(c 14) $(c 15)|2|taken
command|agent_worker_w_1|note D|$(c 13)|1|taken
command|agent_worker_w_2|note D|$(c 12)|1|taken
command|agent_worker_w_2|note D|$(c 11)|1|failed
command|agent_worker_w_2|note C|$(c 10)|1|failed
command|agent_worker_w_2|note C|$(c 9)|1|failed
command|agent_worker_w_1|note B|$(c 8)|1|taken
command|agent_worker_w_1|note B|$(c 2) $(c 4) $(c 5) $(c 6) $(c 7)|5|taken
EOF
)
expect_7d=$(printf '%s\nresume||resumed mission #1|||\ncommand|agent_worker_w_1|%s|%s|1|taken' "$expect_24h" "$LINE_A" "$(c 1)")
expect_api() { # <window> <total> <by_kind> <retries> <agent hours> <per agent-hour>
    code=$(get "/api/interventions?window=$1&until=$U_ISO" "$WORK/api.json")
    [ "$code" = 200 ] || fail "(1) GET /api/interventions?window=$1 answered $code: $(head -c 300 "$WORK/api.json" 2>/dev/null)"
    cp "$WORK/api.json" "$WORK/api-$1.json"
    set -- "$@" $(window_ms "$1")
    assert_eq "$(jq -r '"\(.window) \(.since_ms) \(.until_ms)"' "$WORK/api.json")" "$1 $7 $8" "(1) the $1 answer's window, since_ms and until_ms"
    assert_eq "$(jq -r '.total' "$WORK/api.json")" "$2" "(1) the $1 interventions (one per intent)"
    assert_eq "$(jq -r '.by_kind | to_entries | sort_by(.key) | map("\(.key)=\(.value)") | join(",")' "$WORK/api.json")" "$3" "(1) the $1 interventions by kind"
    assert_eq "$(jq -r '.retries' "$WORK/api.json")" "$4" "(1) the $1 retries (a person's commands beyond each intent's first)"
    assert_eq "$(n3 "$(jq -r '.agent_hours' "$WORK/api.json")")" "$(n3 "$5")" "(1) the $1 agent hours"
    assert_eq "$(n3 "$(jq -r '.per_agent_hour' "$WORK/api.json")")" "$(n3 "$6")" "(1) the $1 interventions per agent-hour"
    if [ "$1" = 24h ]; then want=$expect_24h; else want=$expect_7d; fi
    got=$(items "$1")
    [ "$got" = "$want" ] ||
        fail "(1) the $1 items (kind|agent|line|commands|attempts|outcome), newest first: expected
$want
got
$got"
}
expect_api 24h 11 "cancel_reassign=0,command=11,owner_task=0,resume=0" 6 20 0.55
expect_api 7d 13 "cancel_reassign=0,command=12,owner_task=0,resume=1" 6 26 0.5
code=$(get "/api/interventions?window=2d" "$WORK/bad.json")
assert_eq "$code" 400 "(1) GET /api/interventions?window=2d (not a window it knows): $(head -c 200 "$WORK/bad.json")"

# --- (2) a recount in SQL --------------------------------------------------------------

# Rule (a), written here on its own: each person's command, and whether it
# continues the latest earlier command of its principal, session and action.
cat >"$WORK/recount.sql" <<'SQL'
with person as (
    select c.*, lag(c.id) over k as prev_id
      from spawn_commands c
     where c.principal in ('human', 'spawn')
    window k as (partition by c.principal, c.session_id, c.action order by c.id)
), marked as (
    select p.id, p.created_at_ms,
           not (q.id is not null and q.succeeded is false and q.finished_at_ms is not null
                and q.finished_at_ms <= p.created_at_ms
                and p.created_at_ms - q.finished_at_ms <= 3600000) as starts
      from person p left join spawn_commands q on q.id = p.prev_id
), chained as (
    select m.*, sum(case when starts then 1 else 0 end)
               over (partition by c.principal, c.session_id, c.action order by m.id) as n,
           c.principal, c.session_id, c.action
      from marked m join spawn_commands c using (id)
), intents as (
    select principal, session_id, action, n, min(created_at_ms) as at_ms, count(*) as attempts
      from chained group by 1, 2, 3, 4
)
select (select count(*) from intents where at_ms >= :since and at_ms < :until) || ' ' ||
       (select coalesce(sum(attempts - 1), 0) from intents where at_ms >= :since and at_ms < :until)
SQL
recount() { # <window>: "<intents> <retries>"
    set -- $(window_ms "$1")
    psql -d "$SPAWN_DB_NAME" -tA -v since="$1" -v until="$2" -f "$WORK/recount.sql" 2>&1
}
hours() { # <window>: the executions' hours in it, to 3 decimals
    set -- $(window_ms "$1")
    scalar "select round(coalesce(sum(greatest(0, least(coalesce(e.ended_at_ms, $2), $2)
                                              - greatest(e.started_at_ms, $1))), 0) / 3600000.0, 3)
              from agent_executions e"
}
for w in 24h 7d; do
    assert_eq "$(jq -r '"\(.by_kind.command) \(.retries)"' "$WORK/api-$w.json")" "$(recount "$w")" \
        "(2) the $w intents and retries, the API's against a recount in SQL of rule (a)"
    assert_eq "$(n3 "$(jq -r '.agent_hours' "$WORK/api-$w.json")")" "$(n3 "$(hours "$w")")" \
        "(2) the $w agent hours, the API's against a recount from agent_executions"
done

# --- (3) the scorecard ------------------------------------------------------------------

for w in 24h 7d; do
    capture "$SPAWN_BIN" scorecard --since "$w" --until "$U_ISO" --json
    [ "$status" -eq 0 ] || harness "spawn scorecard --since $w failed: $(err)"
    assert_eq "$(jq -r '.scopes[] | select(.scope == "fleet") | .dimensions.autonomy | "\(.interventions) \(.by_kind.command) \(.retries)"' "$WORK/out.txt")" \
        "$(jq -r '"\(.total) \(.by_kind.command) \(.retries)"' "$WORK/api-$w.json")" \
        "(3) the scorecard's autonomy interventions, by_kind.command and retries over $w, against the API's"
done

# --- (4) /api/fleet ----------------------------------------------------------------------

code=$(get /api/fleet "$WORK/fleet.json")
[ "$code" = 200 ] || harness "GET /api/fleet answered $code"
assert_eq "$(jq -r '.health.ladder_24h.human' "$WORK/fleet.json")" 11 "(4) /api/fleet's ladder_24h.human (a person's intents in 24 h)"
hpa=$(jq -r '.health.human_per_agent_hour' "$WORK/fleet.json")
ah=$(jq -r '.health.agent_hours_24h' "$WORK/fleet.json")
awk -v h="$hpa" -v a="$ah" 'BEGIN { d = h - 11 / a; exit !(a > 0 && d < 0.001 && d > -0.001) }' ||
    fail "(4) /api/fleet's human_per_agent_hour is $hpa with $ah agent hours: 11 intents over them is $(awk -v a="$ah" 'BEGIN { print 11 / a }')"

# --- (5) 043's views ----------------------------------------------------------------------

if [ -n "$(scalar "select to_regclass('spawn_interventions')")" ]; then
    for w in 24h 7d; do
        set -- $(window_ms "$w")
        want=$(jq -r '.by_kind.command' "$WORK/api-$w.json")
        assert_eq "$(scalar "select count(*) from spawn_interventions where kind = 'command' and at_ms >= $1 and at_ms < $2")" "$want" \
            "(5) spawn_interventions' command rows over $w (one per intent)"
        assert_eq "$(scalar "select count(*) from spawn_decisions where tier = 'human' and at_ms >= $1 and at_ms < $2")" "$want" \
            "(5) spawn_decisions' human rows over $w (one per intent)"
    done
else
    echo "049: (5) this tree has no spawn_interventions (043 not landed in it): not checked" >&2
fi

# --- (6) the page's words -------------------------------------------------------------------

[ -f "$tree/web/src/interventions.ts" ] || fail "(6) there is no web/src/interventions.ts"
cat >"$WORK/view.test.mjs" <<'NODE'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { interventionsView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/interventions.ts`).href);
const answer = (w) => JSON.parse(readFileSync(`${env.BB_WORK}/api-${w}.json`, "utf8"));

test("the 24 h answer", () => {
  const view = interventionsView(answer("24h"));
  assert.equal(view.figure, "0.550");
  assert.equal(view.count, "11 interventions");
  assert.equal(view.retries, "6 retries not counted");
  assert.equal(view.rows.length, 11);
  assert.deepEqual(view.rows[0], {
    kind: "command", who: "agent_worker_w_2", line: "note E",
    commands: `#${env.BB_C18}, #${env.BB_C19} (2 tries)`, outcome: "pending",
  });
  assert.deepEqual(view.rows[10], {
    kind: "command", who: "agent_worker_w_1", line: "note B",
    commands: `#${env.BB_C2}, #${env.BB_C4}, #${env.BB_C5}, #${env.BB_C6}, #${env.BB_C7} (5 tries)`, outcome: "taken",
  });
  assert.equal(view.rows[1].commands, `#${env.BB_C17}`);
});

test("the 7 d answer", () => {
  const view = interventionsView(answer("7d"));
  assert.equal(view.figure, "0.500");
  assert.equal(view.count, "13 interventions");
  assert.deepEqual(view.rows[11], { kind: "resume", who: "", line: "resumed mission #1", commands: "", outcome: "" });
});

test("fixed cases", () => {
  const base = answer("24h");
  const one = interventionsView({ ...base, total: 1, retries: 1, per_agent_hour: null, items: [] });
  assert.equal(one.figure, "—");
  assert.equal(one.count, "1 intervention");
  assert.equal(one.retries, "1 retry not counted");
  assert.deepEqual(one.rows, []);
  assert.equal(interventionsView({ ...base, retries: 0 }).retries, null);
  assert.equal(interventionsView({ ...base, per_agent_hour: 0.0004 }).figure, "0.000");
});
NODE
if ! BB_TREE=$tree BB_WORK=$WORK BB_C2=$(c 2) BB_C4=$(c 4) BB_C5=$(c 5) BB_C6=$(c 6) BB_C7=$(c 7) \
    BB_C17=$(c 17) BB_C18=$(c 18) BB_C19=$(c 19) node --test "$WORK/view.test.mjs" >"$WORK/view.out" 2>&1; then
    if grep -q 'ERR_MODULE_NOT_FOUND\|ERR_UNKNOWN_FILE_EXTENSION\|SyntaxError\|ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX' "$WORK/view.out"; then
        fail "(6) web/src/interventions.ts does not load with node alone (relative .ts imports, import type for types, no packages, erasable TypeScript only): $(grep -m 3 'Error' "$WORK/view.out" | tr '\n' ' ')"
    fi
    fail "(6) interventionsView does not give the page's words: $(grep -m 8 -E 'AssertionError|expected|actual|\+ |- |^not ok' "$WORK/view.out" | tr '\n' ' ' | head -c 700)"
fi

echo "049: a person's command refused and sent again is one intervention; the API, a recount in SQL, the scorecard, /api/fleet and the page agree on 24 h and 7 d"
exit 0
