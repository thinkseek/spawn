#!/bin/sh
# Black box: landed objectives left unverified raise one alert, which clears
# when they are verified (objective 135, after 133).
#
# The problem (lead-mac's review §2.8, sync/review-spawn-2026-11-13.md):
# "Verified is rarer than landed". 133a records verifications (`spawn
# verify`) and lists what is landed and not verified (`spawn verify list`,
# `/api/verifications`), but only to someone who asks. One alert says so in
# Needs you while any landed objective stays unverified for over a day.
#
# Required (read through `GET /api/alerts`):
#   (1) the loop's alert step reads, at every reading, the objectives landed
#       since 2026-11-13 00:00 +08:00 whose newest verification after their
#       newest landing is not a pass (`spawn verify list`'s rule). When any
#       landed SPAWN_ALERT_UNVERIFIED_SECS (default 86400) or more ago, one
#       `unverified_landings` alert opens: subject "objectives", text "<n>
#       landed objective(s) unverified for over <bound>: spawn verify list
#       names it/them" ("a day" at the default), evidence {old, unverified,
#       unverified_ms, newest_landed_ms, listed_since_ms, reading_ms}. One
#       open at a time: more readings fire nothing more.
#   (2) it clears at the first reading with none that old (a pass recorded
#       after the landing), its cleared evidence that reading.
#   (3) a fail recorded after that pass opens a new one.
#
# The history (C = 2026-11-13 00:00 +08:00, N = the seed's now), with
# SPAWN_ALERT_UNVERIFIED_SECS=10: mission 1's objective 7 landed at N-60 s,
# not verified; objective 8 landed at N-60 s and passed (spawn verify);
# objective 9 landed at C-1 h, before the window, not verified. A real
# `spawn loop` and `spawn serve` run on a scratch database.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "135: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_135_unverified_$run"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "135"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# The tmux server's socket lives in WORK, so the cleanup removes nothing
# outside it (cc-rules (a), 2026-11-12).

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
serve_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    herdr_cleanup
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "135: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "135: the unverified_landings alerts (key, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'unverified_landings' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "135: harness: unexpected exit $rc" >&2
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
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if bb_read_line typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
# A scratch HOME: nothing here reads or writes the real one.
real_home=$HOME
HOME=$WORK/home
mkdir -p "$HOME" "$SPAWN_CLAUDE_PROJECTS"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
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
    SPAWN_CLAUDE_PROJECTS SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
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
note() { echo "135: $*" >&2; }
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
    case "$id" in '' | *[!0-9]*) harness "writing $1 failed: $(head -c 300 "$WORK/rec.err")";; esac
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
case "$seed_now" in '' | *[!0-9]*) harness "no time from the database: [$seed_now]";; esac
[ "$seed_now" -gt $((C + 3600000)) ] || harness "the clock reads before 2026-11-13 01:00 +08:00: [$seed_now]"
A=$(printf 'bb-135-a' | sha1sum | cut -c1-40)
B=$(printf 'bb-135-b' | sha1sum | cut -c1-40)
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
select pg_temp.rec($C - 7200000, '{"type":"mission.added","id":1,"text":"135 mission","origin":"human","sources":[]}');
select pg_temp.objective($C - 7200000, 3, 9);
select pg_temp.landed($C - 3600000, 3);
select pg_temp.objective($seed_now - 120000, 1, 7);
select pg_temp.landed($seed_now - 60000, 1);
select pg_temp.objective($seed_now - 120000, 2, 8);
select pg_temp.landed($seed_now - 60000, 2);
commit;
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"
tasks=$(q "select string_agg(id || ':' || mission_id || '/' || objective || ':' || state, ' ' order by id) from mission_tasks")
[ "$tasks" = "1:1/7:landed 2:1/8:landed 3:1/9:landed" ] || harness "the seed folded to tasks [$tasks]"
verify() { # <name> <result>
    capture "$SPAWN_BIN" verify "$1" --by lead --how "read the running product" --result "$2"
    [ "$status" -eq 0 ] || harness "spawn verify $1 --result $2 exited $status: $(err)"
}
verify 1/008 pass

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

if wait_until 60 one_open; then
    expect_eq "$(ours | jq -c '[.[] | {subject, state, text}]')" \
        '[{"subject":"objectives","state":"open","text":"1 landed objective unverified for over 10 s: spawn verify list names it"}]' \
        "(1) the unverified_landings alert: 1/007 (1/008 passed; 1/009 landed before 11-13)"
    expect_eq "$(ours | jq -c --argjson l "$((seed_now - 60000))" --argjson c "$C" '[.[] | .evidence | {old, unverified, unverified_ms, newest: (.newest_landed_ms == $l), since: (.listed_since_ms == $c), read: (.reading_ms | type)}]')" \
        '[{"old":1,"unverified":1,"unverified_ms":10000,"newest":true,"since":true,"read":"number"}]' \
        "(1) its evidence"
    # Three more readings (every 2 s): nothing more fires.
    sleep 7
    expect_eq "$(ours | jq 'length')" 1 "(1) one alert, after more readings"
else
    check_failed "(1) no open unverified_landings alert within 60 s: $(ours | head -c 400)"
fi
note "(1) checked: one alert"

# --- (2) a pass clears it ----------------------------------------------------------------------------

verify 1/007 pass
if wait_until 60 none_open; then
    expect_eq "$(ours | jq -c '[.[] | {state, cleared: (.cleared_evidence | {old, unverified, read: (.reading_ms | type)})}]')" \
        '[{"state":"cleared","cleared":{"old":0,"unverified":0,"read":"number"}}]' \
        "(2) cleared at a reading with none unverified"
else
    check_failed "(2) the alert is still open 60 s after 1/007 passed: $(ours | head -c 400)"
fi
note "(2) checked: cleared"

# --- (3) a fail after the pass opens a new one -------------------------------------------------

verify 1/007 fail
if wait_until 60 one_open; then
    expect_eq "$(ours | jq -c '[.[] | .state] | sort')" '["cleared","open"]' "(3) a new alert after the fail, the first one cleared"
else
    check_failed "(3) no new alert within 60 s of 1/007's fail: $(ours | head -c 400)"
fi
note "(3) checked: a fail after a pass"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "135: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "135: a landed objective left unverified raises one alert, cleared by a pass, raised again by a fail"
exit 0
