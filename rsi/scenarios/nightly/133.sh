#!/bin/sh
# Black box: an objective's verification after its landing is recorded,
# and what is landed and not verified is listed (objective 133a).
#
# The problem (lead-mac's review §2.8, sync/review-spawn-2026-11-13.md):
# "Verified is rarer than landed". An objective counts as done when its
# gate passes; the look at the running product is a separate step, and the
# store records none: sync/features.md tracks Verified by hand. Objectives
# are numbered per mission (Flake Doctor's 028 is not Spawn's 028).
#
# Required:
#   (1) `spawn verify <mission>/<objective> --by <name> --how <text>
#       --result pass|fail` records one `objective.verified` and prints
#       `{event, objective}`; an objective that never landed in that
#       mission, a result neither pass nor fail and a name not
#       <mission>/<objective> are refused (non-zero exit, nothing recorded).
#   (2) `spawn verify list` prints, and `GET /api/verifications` answers:
#       `unverified`, the objectives whose newest `task.landed` is on or
#       after 2026-11-13 00:00 +08:00 whose newest verification after that
#       landing is not a pass (none, or a fail: the newest decides), oldest
#       landing first, each {objective "<mission>/<NNN>", task, landed_ms,
#       age_ms, over_a_day, verified_by (the brief's last line that starts
#       with "Verified by:", not the phrase quoted inside another line, or
#       null), last (the newest verification after the landing, or null)}; `over_a_day`, how many landed a day or more
#       ago; `older_unverified`, how many unverified ones landed before
#       11-13 00:00; `deploy`, Spawn's own newest `deploy.finished`
#       {what, at_ms, head, ok, reason}, no claim about one landing.
#
# The history (C = 2026-11-13 00:00 +08:00, N = the seed's now):
#   mission 1: objective 130 (task 1, its brief quoting "Verified by:" in
#   its first line, then the line "Verified by: the lead reads the card.")
#   landed C+1 h; 131 (task 3) landed C-48 h; 132 (task 4) landed C-50 h,
#   verified fail, then pass by (1); 133 (task 5) landed N-1 h, verified
#   pass, then fail by (1).
#   mission 2: objective 130 (task 2, no such line) landed C+2 h; 28 (task
#   6) landed C+3 h, verified pass by (1), then landed again (task 7) after
#   that pass: open again.
#   deploys: ok at C+1.5 h (head A), failed at N-0.5 h (head B, "the build
#   failed").
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "133: harness: $*" >&2
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
SPAWN_DB_NAME="bb_133_verify_$run"
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
    if [ "$rc" -ne 0 ] && [ -s "$WORK/list.json" ]; then
        echo "133: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/list.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "133: harness: unexpected exit $rc" >&2
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


note() { echo "133: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the history -------------------------------------------------------------------------

C=1794499200000
H=3600000
A=$(printf 'bb-133-a' | sha1sum | cut -c1-40)
B=$(printf 'bb-133-b' | sha1sum | cut -c1-40)
seed_now=$(scalar "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
[ "$seed_now" -gt $((C + 4 * H)) ] || harness "the clock reads before 2026-11-13 04:00 +08:00: [$seed_now]"
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
begin;
create temp sequence seed_seq start 100000;
create function pg_temp.rec(at bigint, r jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            nextval('seed_seq'), at) \$\$;
create function pg_temp.objective(at bigint, task int, mission int, objective int, brief text)
returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.assigned', 'id', task,
        'mission_id', mission, 'kind', 'ephemeral', 'created_by', 'resident',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'objective', objective, 'brief', brief)) \$\$;
create function pg_temp.landed(at bigint, task int) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.landed', 'id', task,
        'from', '$A', 'to', '$B')) \$\$;
create function pg_temp.deployed(at bigint, head text, ok boolean) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'deploy.finished', 'by', 'landing',
        'head', head, 'ok', ok, 'reason', case when ok then '' else 'the build failed' end)) \$\$;

select pg_temp.rec($C - 60 * $H, '{"type":"mission.added","id":1,"text":"133 mission one","origin":"human","sources":[]}');
select pg_temp.rec($C - 60 * $H, '{"type":"mission.added","id":2,"text":"133 mission two","origin":"human","sources":[]}');
select pg_temp.objective($C - 55 * $H, 4, 1, 132, 'Objective 132.');
select pg_temp.landed($C - 50 * $H, 4);
select pg_temp.objective($C - 49 * $H, 3, 1, 131, 'Objective 131.');
select pg_temp.landed($C - 48 * $H, 3);
select pg_temp.objective($C, 1, 1, 130, E'Objective 130: a card. Every objective carries a "Verified by:" paragraph in its prompt.\n\nVerified by: the lead reads the card.\nrev-g reviews.');
select pg_temp.landed($C + $H, 1);
select pg_temp.deployed($C + 3 * $H / 2, '$A', true);
select pg_temp.objective($C, 2, 2, 130, 'Objective 130 of another mission, with no such line.');
select pg_temp.landed($C + 2 * $H, 2);
select pg_temp.objective($C, 6, 2, 28, 'Objective 28.');
select pg_temp.landed($C + 3 * $H, 6);
select pg_temp.objective($seed_now - 2 * $H, 5, 1, 133, 'Objective 133.');
select pg_temp.landed($seed_now - $H, 5);
select pg_temp.deployed($seed_now - $H / 2, '$B', false);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(scalar "select string_agg(id || ':' || mission_id || '/' || objective || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:1/130:landed 2:2/130:landed 3:1/131:landed 4:1/132:landed 5:1/133:landed 6:2/28:landed" ] ||
    harness "the seed folded to tasks [$tasks]"

# --- (1) spawn verify ----------------------------------------------------------------------------

recorded() { scalar "select count(*) from spawn_events where session_id = 'session_control' and kind -> 'detail' -> 'record' ->> 'type' = 'objective.verified'"; }
verify() { # <name> <result>
    capture "$SPAWN_BIN" verify "$1" --by lead --how "read the running product" --result "$2"
}
verify 1/132 fail
[ "$status" -eq 0 ] || check_failed "(1) spawn verify 1/132 --result fail exited $status: $(err)"
verify 1/132 pass
if [ "$status" -ne 0 ]; then
    check_failed "(1) spawn verify 1/132 --result pass exited $status: $(err)"
else
    expect_eq "$(jq -c '{objective, event: (.event | type)}' "$WORK/out.txt")" '{"objective":"1/132","event":"number"}' \
        "(1) spawn verify prints the record's event and the objective's name"
fi
verify 1/133 pass
[ "$status" -eq 0 ] || check_failed "(1) spawn verify 1/133 --result pass exited $status: $(err)"
verify 1/133 fail
[ "$status" -eq 0 ] || check_failed "(1) spawn verify 1/133 --result fail exited $status: $(err)"
verify 2/028 pass
[ "$status" -eq 0 ] || check_failed "(1) spawn verify 2/028 --result pass exited $status: $(err)"
before=$(recorded)
for refused in "2/131 pass" "1/134 pass" "1/130 maybe" "130 pass" "0/130 pass"; do
    # shellcheck disable=SC2086 # two words on purpose
    verify $refused
    [ "$status" -ne 0 ] || check_failed "(1) spawn verify $refused was taken (2/131 and 1/134 never landed; maybe is no result; 130 and 0/130 are no names)"
done
expect_eq "$(recorded)" "$before" "(1) a refused verification records nothing"
expect_eq "$before" 5 "(1) five verifications recorded"

# Objective 2/028 lands again after its pass: open again.
# Right after the pass (its record's own time), so the landing is newer;
# with no verification recorded (a check above has failed), after the newest
# control record, so the list's checks still run and fail as checks.
again=$(scalar "select coalesce(max(recorded_at_ms) filter (where kind -> 'detail' -> 'record' ->> 'type' = 'objective.verified'), max(recorded_at_ms)) from spawn_events where session_id = 'session_control'")
case "$again" in '' | *[!0-9]*) harness "no verification time: [$again]" ;; esac
capture psql -d "$SPAWN_DB_NAME" -q -c "begin;
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
    select 'agent_control', 'session_control', null, 'control', null,
           jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
           (select max(session_seq) from spawn_events where session_id = 'session_control') + n, $again + n
      from (values (1, '{\"type\":\"task.assigned\",\"id\":7,\"mission_id\":2,\"kind\":\"ephemeral\",\"created_by\":\"resident\",\"agent_id\":\"agent_worker_7\",\"session_id\":\"session_worker_7\",\"objective\":28,\"brief\":\"Objective 28 again.\"}'::jsonb),
                   (2, '{\"type\":\"task.landed\",\"id\":7,\"from\":\"$A\",\"to\":\"$B\"}'::jsonb)) v(n, r);
    commit; select spawn_control_refold();"
[ "$status" -eq 0 ] || harness "landing 2/028 again failed: $(err)"
note "(1) checked: spawn verify"

# --- (2) the list ---------------------------------------------------------------------------------

capture "$SPAWN_BIN" verify list
[ "$status" -eq 0 ] || check_failed "(2) spawn verify list exited $status: $(err)"
cp "$WORK/out.txt" "$WORK/list.json"
over() { if [ $((seed_now - $1)) -ge $((24 * H)) ]; then echo true; else echo false; fi; }
o1=$(over $((C + H)))
o2=$(over $((C + 2 * H)))
count=0
[ "$o1" = true ] && count=$((count + 1))
[ "$o2" = true ] && count=$((count + 1))
want="[{\"objective\":\"1/130\",\"task\":1,\"landed_ms\":$((C + H)),\"over_a_day\":$o1,\"verified_by\":\"Verified by: the lead reads the card.\",\"last\":null},\
{\"objective\":\"2/130\",\"task\":2,\"landed_ms\":$((C + 2 * H)),\"over_a_day\":$o2,\"verified_by\":null,\"last\":null},\
{\"objective\":\"1/133\",\"task\":5,\"landed_ms\":$((seed_now - H)),\"over_a_day\":false,\"verified_by\":null,\"last\":\"fail\"},\
{\"objective\":\"2/028\",\"task\":7,\"landed_ms\":$((again + 2)),\"over_a_day\":false,\"verified_by\":null,\"last\":null}]"
shape='[.unverified[] | {objective, task, landed_ms, over_a_day, verified_by, last: (.last.result // null)}]'
expect_eq "$(jq -c "$shape" "$WORK/list.json")" "$(printf '%s' "$want" | tr -d '\n')" \
    "(2) the unverified objectives, oldest landing first: 1/130 (its Verified by: line, not the quote) and 2/130 (one number, two missions), 1/133 (passed, then failed: its fail listed), 2/028 (landed again after its pass); 1/132 failed, then passed; 1/131 is before 11-13"
expect_eq "$(jq -c '[.unverified[] | .age_ms >= 0 and .age_ms == (.age_ms | floor)] | all' "$WORK/list.json")" true \
    "(2) each age is a whole number of ms"
expect_eq "$(jq -c '{over_a_day, older_unverified, listed_since_ms}' "$WORK/list.json")" \
    "{\"over_a_day\":$count,\"older_unverified\":1,\"listed_since_ms\":$C}" \
    "(2) the counts: over a day as the landings' ages say; one older (1/131; 1/132 passed after its fail)"
expect_eq "$(jq -cS '.deploy' "$WORK/list.json")" \
    "$(printf '%s' "{\"what\":\"Spawn's own deploys (deploy/deploy.sh)\",\"at_ms\":$((seed_now - H / 2)),\"head\":\"$B\",\"ok\":false,\"reason\":\"the build failed\"}" | jq -cS .)" \
    "(2) the newest deploy record, Spawn's own"
expect_eq "$(jq -c '.unverified[] | select(.objective == "1/133") | .last | {result, by, how}' "$WORK/list.json")" \
    '{"result":"fail","by":"lead","how":"read the running product"}' "(2) 1/133's last verification"

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
code=$(curl -s --noproxy '*' -o "$WORK/api.json" -w '%{http_code}' "http://127.0.0.1:$port/api/verifications" 2>/dev/null) || code=000
if [ "$code" != 200 ]; then
    check_failed "(2) GET /api/verifications answered $code: $(head -c 200 "$WORK/api.json")"
else
    same='del(.unverified[].age_ms)'
    expect_eq "$(jq -cS "$same" "$WORK/api.json")" "$(jq -cS "$same" "$WORK/list.json")" \
        "(2) /api/verifications answers what spawn verify list prints (ages aside)"
fi
note "(2) checked: the list"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "133: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "133: verifications are recorded by mission and objective, and landed objectives not verified are listed"
exit 0
