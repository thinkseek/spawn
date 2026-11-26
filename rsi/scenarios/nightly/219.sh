#!/bin/sh
# Black box: an open unverified_landings alert says the count as it is now
# (objective 219 [observability], after 135).
#
# The problem (rev-i, 11-17): the alert fired 11-14 saying "10 landed
# objectives unverified for over a day" and still said 10 while 13 or more
# were. It fired once with the count of its first reading, and every later
# reading only saw that one was open.
#
# Required (read through `GET /api/alerts` and the control log):
#   (1) one objective old: one unverified_landings alert opens, "1 landed
#       objective unverified for over 10 s: spawn verify list names it".
#   (2) while it is open a second objective becomes unverified and old (a
#       fail recorded after its pass): the SAME alert (one alert, its key and
#       fired time unchanged, still open) now says "2 landed objectives
#       unverified for over 10 s: spawn verify list names them", evidence old
#       2 and fired_old 1.
#   (3) the first passes: the same alert says "1 landed objective …", old 1.
#   (4) readings that find the same count write nothing: the log holds
#       exactly 2 alert.updated records for it, and one alert.fired.
#   (4b) (R2, rev-k's F1) the same count, a newer old landing: objective
#       12's fail and objective 10's pass, written in ONE transaction so no
#       reading sees one without the other: the same alert still says 1
#       (old 1, fired_old 1), and its newest_landed_ms moves from N-60 s to
#       N-30 s.
#   (5) the last one passes: it clears, as in 135 (2).
#
# The history (N = the seed's now), with SPAWN_ALERT_UNVERIFIED_SECS=10:
# mission 1's objective 7 landed at N-60 s, not verified; objective 10
# landed at N-60 s and passed (spawn verify); objective 12 landed at N-30 s
# and passed. A real `spawn loop` and `spawn serve` run on a scratch
# database.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "219: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
SPAWN_HC=${SPAWN_HERDR_CORE_PROGRAM-}
if [ -z "$SPAWN_HC" ]; then
    spawn_dir=$(dirname "$SPAWN_BIN")
    if [ -x "$spawn_dir/spawn-hc" ]; then
        SPAWN_HC=$spawn_dir/spawn-hc
    elif [ -x "$(dirname "$spawn_dir")/spawn-hc" ]; then
        SPAWN_HC=$(dirname "$spawn_dir")/spawn-hc
    elif command -v spawn-hc >/dev/null 2>&1; then
        SPAWN_HC=$(command -v spawn-hc)
    fi
fi
[ -n "$SPAWN_HC" ] && [ -x "$SPAWN_HC" ] ||
    harness "spawn-hc is not installed (set SPAWN_HERDR_CORE_PROGRAM or build sibling spawn-hc beside SPAWN_BIN)"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/r
XDG_STATE_HOME=$WORK/state
XDG_CONFIG_HOME=$WORK/config
mkdir -m 700 "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME"
export XDG_RUNTIME_DIR XDG_STATE_HOME XDG_CONFIG_HOME
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
hc_id=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
SPAWN_DB_NAME="bb_219_unverified_count_$run"
SPAWN_HERDR_SESSION="bb-219-$hc_id"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_HERDR_SESSION SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
hc() {
    env -u HERDR_SOCKET_PATH -u HERDR_SESSION -u HERDR_PANE_ID \
        HC_SESSION="$SPAWN_HERDR_SESSION" \
        HC_SOCKET="$XDG_RUNTIME_DIR/herdr-core/$SPAWN_HERDR_SESSION.sock" \
        "$SPAWN_HC" "$@"
}
stop_herdr() {
    [ -n "${SPAWN_HERDR_SESSION-}" ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ] || return 0
    for addr in $(hc ls --json --label "spawn_owner=$SPAWN_HERDR_SESSION" 2>/dev/null |
        jq -r '.terminals[]? | select(.state == "live") | .address // empty' 2>/dev/null || true); do
        hc kill "$addr" >/dev/null 2>&1 || true
    done
    hc server stop >/dev/null 2>&1 || true
    hc_wait=0
    while [ "$hc_wait" -lt 20 ] && [ -S "${XDG_RUNTIME_DIR-}/herdr-core/$SPAWN_HERDR_SESSION.sock" ]; do
        hc_wait=$((hc_wait + 1))
        sleep 0.1
    done
}
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    stop_herdr
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "219: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "219: the unverified_landings alerts (key, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'unverified_landings' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "219: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

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
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}

# --- the world ----------------------------------------------------------------

cat >"$WORK/gitconfig" <<'EOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
EOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL
repo=$WORK/repo.git
git init -q --bare --initial-branch=main "$repo"
git init -q --initial-branch=main "$WORK/seed"
printf 'base\n' >"$WORK/seed/README.md"
git -C "$WORK/seed" add -A
git -C "$WORK/seed" commit -qm base
git -C "$WORK/seed" push -q "$repo" main:main

# A quiet engine: the loop may deliver the tasks below to a worker; this
# one shows a prompt, takes what is typed and does nothing with it.
bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
[ -n "${BB_PATH-}" ] && PATH=$BB_PATH && export PATH
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if bb_read_line typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE \
    SPAWN_HERDR_SHARDS SPAWN_HERDR_PROGRAM SPAWN_TMUX_SOCKET SPAWN_TMUX_PROGRAM TMUX \
    TMUX_PANE TMUX_TMPDIR; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
mkdir -p "$SPAWN_CLAUDE_PROJECTS"
BB_PATH=$PATH
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# The bound under test: 10 s (default a day).
SPAWN_ALERT_UNVERIFIED_SECS=10
SPAWN_STALL_ACTIVITY_SECS=1800
SPAWN_STALL_DELIVERY_SECS=1800
SPAWN_STALL_REVIEW_SECS=1800
SPAWN_STALL_GATE_SECS=1800
SPAWN_STALL_LANDING_SECS=1800
SPAWN_STALL_STOP_SECS=1800
SPAWN_STALL_HALTED_SECS=1800
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
export HOME SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS BB_PATH SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_UNVERIFIED_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
    SPAWN_STALL_REVIEW_SECS SPAWN_STALL_GATE_SECS SPAWN_STALL_LANDING_SECS \
    SPAWN_STALL_STOP_SECS SPAWN_STALL_HALTED_SECS SPAWN_MISSION_FILE \
    SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB
mkdir -p "$SPAWN_WORKSPACE_ROOT" "$WORK/ctl/scenarios" "$WORK/web"
: >"$SPAWN_MISSION_FILE"
printf '<!doctype html><title>spawn</title>\n' >"$WORK/web/index.html"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

# --- helpers ----------------------------------------------------------------

now() { date +%s; }
q() { scalar "$1"; }
note() { echo "219: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

record() { # <JSON>: written as the integrator writes it; prints the event id
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_control_record('$1'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    echo "$id"
}

start_serve() {
    "$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$WORK/web" >"$WORK/serve.log" 2>&1 &
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
    base="http://127.0.0.1:$port"
}
alerts_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/alerts.json" -w '%{http_code}' "$base/api/alerts" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/alerts answered $code: $(head -c 300 "$WORK/alerts.json" 2>/dev/null)"
}


# --- the history ---------------------------------------------------------------------------

C=1794499200000
seed_now=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]" ;; esac
[ "$seed_now" -gt $((C + 3600000)) ] || harness "the clock reads before 2026-11-13 01:00 +08:00: [$seed_now]"
A=$(printf 'bb-219-a' | sha1sum | cut -c1-40)
B=$(printf 'bb-219-b' | sha1sum | cut -c1-40)
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
create function pg_temp.objective(at bigint, task int, objective int) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.assigned', 'id', task,
        'mission_id', 1, 'kind', 'ephemeral', 'created_by', 'resident',
        'agent_id', 'agent_worker_' || task, 'session_id', 'session_worker_' || task,
        'objective', objective, 'brief', 'Objective ' || objective || E'.\nVerified by: the lead.')) \$\$;
create function pg_temp.landed(at bigint, task int) returns void language sql as \$\$
    select pg_temp.rec(at, jsonb_build_object('type', 'task.landed', 'id', task,
        'from', '$A', 'to', '$B')) \$\$;
select pg_temp.rec($C - 7200000, '{"type":"mission.added","id":1,"text":"219 mission","origin":"human","sources":[]}');
select pg_temp.objective($seed_now - 120000, 1, 7);
select pg_temp.landed($seed_now - 60000, 1);
select pg_temp.objective($seed_now - 120000, 2, 10);
select pg_temp.landed($seed_now - 60000, 2);
select pg_temp.objective($seed_now - 120000, 3, 12);
select pg_temp.landed($seed_now - 30000, 3);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(q "select string_agg(id || ':' || mission_id || '/' || objective || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:1/7:landed 2:1/10:landed 3:1/12:landed" ] || harness "the seed folded to tasks [$tasks]"
verify() { # <name> <result>
    capture "$SPAWN_BIN" verify "$1" --by lead --how "read the running product" --result "$2"
    [ "$status" -eq 0 ] || harness "spawn verify $1 --result $2 exited $status: $(err)"
}
verify 1/010 pass
verify 1/012 pass

# --- the loop ---------------------------------------------------------------------------------

unset SPAWN_TEST_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL 2>/dev/null || true
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve

ours() { # its unverified_landings alerts in /api/alerts, any state
    alerts_json
    jq -c '[.[] | select(.kind == "unverified_landings")]' "$WORK/alerts.json"
}
open_count() { ours | jq '[.[] | select(.state == "open")] | length'; }
one_open() { [ "$(open_count)" = 1 ]; }
none_open() { [ "$(open_count)" = 0 ]; }

# --- (1) one alert ---------------------------------------------------------------------------------

key=""
fired_at=""
said() { # the open alert's text and evidence {old, fired_old}, and its key and fired time
    ours | jq -c '[.[] | select(.state == "open") | {key, fired_at_ms, text, old: .evidence.old, fired_old: .evidence.fired_old}]'
}
says() { # <n>: the open alert's evidence says n old
    [ "$(ours | jq '[.[] | select(.state == "open") | .evidence.old] | .[0] // empty')" = "$1" ]
}
updates() {
    q "select count(*) from spawn_events where session_id = 'session_control'
         and kind -> 'detail' -> 'record' ->> 'type' = 'alert.updated'
         and kind -> 'detail' -> 'record' ->> 'key' = '$key'"
}
if wait_until 60 one_open; then
    key=$(ours | jq -r '[.[] | select(.state == "open")][0].key')
    fired_at=$(ours | jq -r '[.[] | select(.state == "open")][0].fired_at_ms')
    expect_eq "$(ours | jq -c '[.[] | {subject, state, text}]')" \
        '[{"subject":"objectives","state":"open","text":"1 landed objective unverified for over 10 s: spawn verify list names it"}]' \
        "(1) the unverified_landings alert: 1/007 (1/010 passed)"
else
    check_failed "(1) no open unverified_landings alert within 60 s: $(ours | head -c 400)"
fi
note "(1) checked: one alert, key $key"

# --- (2) a second one unverified: the same alert says 2 --------------------------------------------

verify 1/010 fail
if wait_until 60 says 2; then
    expect_eq "$(said)" \
        "[{\"key\":\"$key\",\"fired_at_ms\":$fired_at,\"text\":\"2 landed objectives unverified for over 10 s: spawn verify list names them\",\"old\":2,\"fired_old\":1}]" \
        "(2) the same alert, open, with the count now"
    expect_eq "$(ours | jq 'length')" 1 "(2) still one unverified_landings alert"
else
    check_failed "(2) the open alert does not say 2 within 60 s of 1/010's fail: $(ours | head -c 400)"
fi
note "(2) checked: 2"

# --- (3) the first passes: the same alert says 1 ----------------------------------------------------

verify 1/007 pass
if wait_until 60 says 1; then
    expect_eq "$(said)" \
        "[{\"key\":\"$key\",\"fired_at_ms\":$fired_at,\"text\":\"1 landed objective unverified for over 10 s: spawn verify list names it\",\"old\":1,\"fired_old\":1}]" \
        "(3) the same alert, back to 1"
else
    check_failed "(3) the open alert does not say 1 within 60 s of 1/007's pass: $(ours | head -c 400)"
fi
note "(3) checked: 1"

# --- (4) the same count writes nothing -------------------------------------------------------------

# Three more readings (every 2 s), the count unchanged.
sleep 7
expect_eq "$(updates)" 2 "(4) alert.updated records for $key (one per change)"
expect_eq "$(q "select count(*) from spawn_events where session_id = 'session_control'
    and kind -> 'detail' -> 'record' ->> 'type' = 'alert.fired'
    and kind -> 'detail' -> 'record' ->> 'kind' = 'unverified_landings'")" 1 "(4) one alert.fired"
note "(4) checked: no update without a change"

# --- (4b, rev-k) the same count, a newer old landing ---------------------------------------------
# 1/012 (landed N-30 s) fails and 1/010 (landed N-60 s) passes in ONE
# transaction, so no reading sees one without the other: old stays 1, the
# newest old landing moves from N-60 s to N-30 s.
newest() { [ "$(ours | jq '[.[] | select(.state == "open") | .evidence.newest_landed_ms] | .[0] // empty')" = "$1" ]; }
newest $((seed_now - 60000)) || check_failed "(4b) before: newest_landed_ms is not N-60 s: $(said | head -c 300)"
rec=$(q "select kind -> 'detail' -> 'record' from spawn_events where session_id = 'session_control'
    and kind -> 'detail' -> 'record' ->> 'type' = 'objective.verified' order by id desc limit 1")
r12=$(printf '%s' "$rec" | jq -c '.objective = 12 | .result = "fail"')
r10=$(printf '%s' "$rec" | jq -c '.objective = 10 | .result = "pass"')
capture psql -d "$SPAWN_DB_NAME" -q -v ON_ERROR_STOP=1 -c "begin" \
    -c "select spawn_verification_record('$r12'::jsonb)" -c "select spawn_verification_record('$r10'::jsonb)" -c "commit"
[ "$status" -eq 0 ] || harness "(4b) the swap failed: $(err)"
if wait_until 60 newest $((seed_now - 30000)); then
    expect_eq "$(said | jq -c '.[] | {text, old, fired_old}')" \
        '{"text":"1 landed objective unverified for over 10 s: spawn verify list names it","old":1,"fired_old":1}' \
        "(4b) the same alert, 1, the newer landing"
else
    check_failed "(4b) the open alert's newest_landed_ms did not move to N-30 s within 60 s: $(ours | head -c 400)"
fi
note "(4b) checked: newest moved, count 1"

# --- (5) none old: cleared ---------------------------------------------------------------------------

verify 1/012 pass
if wait_until 60 none_open; then
    expect_eq "$(ours | jq -c '[.[] | {key, state, cleared: (.cleared_evidence | {old})}]')" \
        "[{\"key\":\"$key\",\"state\":\"cleared\",\"cleared\":{\"old\":0}}]" \
        "(5) cleared at a reading with none old"
else
    check_failed "(5) the alert is still open 60 s after 1/010 passed: $(ours | head -c 400)"
fi
note "(5) checked: cleared"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "219: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "219: an open unverified_landings alert says the count as it is now, up and down, and clears as before"
exit 0
