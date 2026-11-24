#!/bin/sh
# Black box: a guardrail alert judged false stays judged until a fresh
# reading finds its figure in range, in a loop that keeps running as in one
# restarted (objective 209, after 192).
#
# The problem (192 R4, rev-f's n1 and the lead's follow-up order,
# 2026-11-17): 192 keeps a judged stretch through a restart. A restarted
# loop keeps the judged out-since through readings with no range or no
# value, until a fresh reading out takes it; after that take, a reading with
# no range ends the stretch (192 R4's rule (a)). A loop that kept running
# keeps nothing for an alert judged while it runs: a reading with no range
# drops the stretch's count, and the next reading out starts a new stretch,
# raised again after its hold. One judgement then gives a second alert, or
# not, by whether the loop restarted in between.
#
# Required (read through `GET /api/alerts`), one rule for both loops: only a
# fresh reading with the figure in range (or, at a restarted loop's first
# reading, a sample in range after the out-since) ends a judged stretch. A
# reading with no range or no value ends nothing, before or after a fresh
# reading out. A stretch not judged follows 089's clear hold as before.
#
# A real `spawn loop` and `spawn serve` on a scratch database, as 192.sh:
# leaked test schemas, its range from a ranges file (10^18 or more: out at
# every reading; 0 or more: in; none: `no_range`). No samples, the raise
# hold 15 s, readings every second, the clear hold an hour.
#   (A) continuous: an alert fires a hold after the loop's start; judged
#       false while the loop runs; then no range, then out for more than two
#       holds: no new alert (fails on main: a new one fires a hold after the
#       reading out);
#   (B) same loop: in range, then out: a new alert, out since after the
#       switch (a fresh reading in range ends the judged stretch);
#   (C) restarted: (B)'s alert judged false; a new loop, out at its first
#       reading (it takes the judged out-since), then no range, then out for
#       more than two holds: no new alert (fails on main, 192 R4's rule (a):
#       a new one fires);
#   (D) a stretch not judged, same loop as (C) after a reading in range and
#       out again: its alert fires; then no range with the clear hold 2 s
#       (a new loop): cleared by its condition (089), and out again: a new
#       alert after its hold (a cleared alert is not a judged one);
#   (E) (rev-i's F1 on R1) (D)'s alert, open in its clear hold (an hour
#       again) while leaked test schemas reads in range, then no range;
#       judged false; then out: a new alert after its hold (the stretch ended
#       at the reading in range; on main too);
#   (F) (E)'s new alert judged "right" while the loop runs, then no range
#       (the clear hold 2 s): cleared by its condition; then out: a new alert
#       after its hold (a "right" judgement keeps nothing judged).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "209: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
[ -f "$tree/web/src/home.ts" ] || harness "no web/src/home.ts under $tree (run from the candidate's tree)"
for tool in psql tmux git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_209_judged_$run"
SPAWN_TMUX_SOCKET="bb-209-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL
# The tmux server's socket lives in WORK, so the cleanup removes nothing
# outside it (cc-rules (a), 2026-11-12).
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$TMUX_TMPDIR"
export TMUX_TMPDIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
test_made=""
loop_pid=""
serve_pid=""
cleanup() {
    for pid in "$loop_pid" "$serve_pid"; do
        [ -n "$pid" ] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    if [ -n "$test_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "${SPAWN_DB_NAME}_t" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "209: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "209: the guardrail alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'guardrail_out_of_range' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "209: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1
psql -d postgres -tA -c "create database ${SPAWN_DB_NAME}_t" >/dev/null 2>&1 ||
    harness "cannot create the scenario's test database"
test_made=1

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
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"

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
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# The guardrails under test: read every second; raised after 3700 s out of
# range (a hold no run of this scenario can wait out from its own start),
# cleared after an hour; samples an hour apart (the defaults: 300 s, 2 h,
# 1 h, 1 h).
SPAWN_ALERT_GUARDRAIL_SECS=1
SPAWN_GUARDRAIL_RAISE_SECS=3700
SPAWN_GUARDRAIL_CLEAR_SECS=3600
SPAWN_CORE_LINES_SAMPLE_SECS=3600
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
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_GUARDRAIL_SECS SPAWN_GUARDRAIL_RAISE_SECS SPAWN_GUARDRAIL_CLEAR_SECS SPAWN_CORE_LINES_SAMPLE_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
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
note() { echo "209: $*" >&2; }
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


# --- the ranges, the samples, the loop runs --------------------------------------------------------

# No test database is handed to the loop in its environment: leaked test
# schemas reads the scenario's own test database, named by a URL file
# (mode 0600, never printed).
test_url=$WORK/test.url
(umask 077 && printf '%s\n' "$server_url/${SPAWN_DB_NAME}_t" >"$test_url") || harness "cannot write the URL file"
unset SPAWN_TEST_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL 2>/dev/null || true
write_ranges() { # <leaked test schemas' range as JSON, or nothing for no range>
    if [ -n "${1-}" ]; then
        figure="\"storage.leaked_test_schemas\": $1"
    else
        figure=""
    fi
    printf '{"ranges": {%s},\n "storage": {"test_database_url_files": [{"name": "bb", "url_file": "%s"}]}}\n' \
        "$figure" "$test_url" >"$SPAWN_WORKSPACE_ROOT/core-lines.json"
}
write_ranges '{"min": 1000000000000000000}'
start_serve

db_now() { q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint"; }
sample() { # <ms before now> <value>: a host.core_lines sample by the store's writer, placed then
    id=$(psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "select spawn_host_record('{\"type\": \"host.core_lines\", \"values\": {\"storage.leaked_test_schemas\": $2}}'::jsonb)" 2>"$WORK/rec.err" | head -n 1)
    case "$id" in '' | *[!0-9]*) harness "writing a sample failed: $(head -c 300 "$WORK/rec.err")" ;; esac
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "update spawn_events set recorded_at_ms = $(($(db_now) - $1)) where id = $id" >/dev/null 2>"$WORK/rec.err" ||
        harness "placing a sample failed: $(head -c 300 "$WORK/rec.err")"
}
no_samples() {
    psql -d "$SPAWN_DB_NAME" -tA -v ON_ERROR_STOP=1 -c "delete from spawn_events where session_id = 'session_control'
        and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'" >/dev/null 2>"$WORK/rec.err" ||
        harness "clearing the samples failed: $(head -c 300 "$WORK/rec.err")"
}
start_loop() {
    loop_started=$(db_now)
    "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}
stop_loop() {
    kill "$loop_pid" 2>/dev/null || true
    wait "$loop_pid" 2>/dev/null || true
    loop_pid=""
}
alerts_of() { # leaked test schemas' guardrail_out_of_range alerts in /api/alerts, any state
    alerts_json
    jq -c '[.[] | select(.kind == "guardrail_out_of_range" and .subject == "guardrail storage.leaked_test_schemas")]' "$WORK/alerts.json"
}
any_alert() { [ "$(alerts_of | jq length)" != 0 ]; }
open_alert() { [ "$(alerts_of | jq '[.[] | select(.state == "open")] | length')" != 0 ]; }
no_open_alert() { ! open_alert; }
read_twice() { # the loop has made at least one guardrail reading
    grep -q "alerts: core lines read" "$WORK/loop.log" 2>/dev/null
}

GIB=1073741824
HOUR=3600000

SPAWN_GUARDRAIL_RAISE_SECS=15
SPAWN_GUARDRAIL_CLEAR_SECS=3600
export SPAWN_GUARDRAIL_RAISE_SECS SPAWN_GUARDRAIL_CLEAR_SECS
ranges_out() { write_ranges '{"min": 1000000000000000000}'; }
ranges_in() { write_ranges '{"min": 0}'; }
open_count() { alerts_of | jq -c '[.[] | select(.state == "open")] | length'; }
open_key() { alerts_of | jq -r '[.[] | select(.state == "open")][0].key // empty'; }
judge() { # <key> <case>
    capture "$SPAWN_BIN" alert judge "$1" false
    [ "$status" -eq 0 ] || harness "($2) judging $1 failed: $(err)"
}
readings() { grep -c "alerts: core lines read" "$WORK/loop.log" 2>/dev/null || true; }
# Two more guardrail readings than when it was called (a reading begun
# before a ranges file change may still show the old one, so the second is
# the first surely after it; rev-g's n1 on R1: a fixed sleep proves no
# reading came). Bounded: none within 60 s is a harness failure.
settle() { # <case> <what the readings must see>
    settle_from=$(readings)
    wait_until 60 more_readings || harness "($1) the loop made no two readings within 60 s ($2): $(tail -n 3 "$WORK/loop.log")"
}
more_readings() { [ "$(readings)" -ge $((settle_from + 2)) ]; }
new_after() { # <old key> <switched ms> <case>: one open alert, new, out since after the switch
    expect_eq "$(alerts_of | jq -c --arg old "$1" --argjson switched "$2" '[.[] | select(.state == "open") | {new: (.key != $old), after_the_switch: (.evidence.out_since_ms >= $switched)}]')" \
        '[{"new":true,"after_the_switch":true}]' \
        "($3) one new open alert, out since a reading after leaked test schemas was found out again"
}

# --- (A) judged while the loop runs, then no range, then out -----------------------------------------
no_samples
ranges_out
: >"$WORK/loop.log"
start_loop
wait_until 60 read_twice || harness "(A) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
wait_until 60 open_alert || harness "(A) no alert within 60 s of the loop's start, out from its first reading (the hold 15 s)"
judged_a=$(open_key)
judge "$judged_a" A
# The loop reads the judgement at its next pass (every 2 s); the clear hold
# is an hour, so no reading below clears the alert by its condition first.
sleep 6
write_ranges
settle A "no range"
ranges_out
settle A "out"
sleep 35
settle A "out, more than two holds on"
expect_eq "$(open_count)" 0 \
    "(A) the alert judged false while the loop runs, then no range, then out for more than two holds (15 s): no new alert (a reading with no range does not end a judged stretch)"
note "(A) checked: a judged stretch outlives a reading with no range in a running loop"

# --- (B) same loop: in range ends it ------------------------------------------------------------------
ranges_in
settle B "in range"
switched=$(db_now)
ranges_out
if wait_until 60 open_alert; then
    new_after "$judged_a" "$switched" B
else
    check_failed "(B) in range, then out for a hold: no new alert within 60 s (a fresh reading in range did not end the judged stretch)"
fi
stop_loop
note "(B) checked: a fresh reading in range ends a judged stretch"

# --- (C) restarted: a taken judged stretch outlives a reading with no range --------------------------
judged_c=$(open_key)
if [ -z "$judged_c" ]; then
    check_failed "(C) no open alert from (B) to judge: (C) and (D) not run"
else
    judge "$judged_c" C
    ranges_out
    : >"$WORK/loop.log"
    start_loop
    wait_until 60 read_twice || harness "(C) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
    settle C "out at the first readings"
    write_ranges
    settle C "no range"
    ranges_out
    settle C "out"
    sleep 35
    settle C "out, more than two holds on"
    expect_eq "$(open_count)" 0 \
        "(C) the stretch judged false, out at the restarted loop's first reading (taking the judged out-since), then no range, then out for more than two holds: no new alert (the taken stretch ended at a reading with no range)"
    note "(C) checked: a taken judged stretch outlives a reading with no range after a restart"

    # --- (D) a stretch not judged: 089's clear hold, unchanged --------------------------------------
    ranges_in
    settle D "in range"
    switched=$(db_now)
    ranges_out
    if wait_until 60 open_alert; then
        new_after "$judged_c" "$switched" D
        stop_loop
        unjudged=$(open_key)
        SPAWN_GUARDRAIL_CLEAR_SECS=2
        export SPAWN_GUARDRAIL_CLEAR_SECS
        write_ranges
        : >"$WORK/loop.log"
        start_loop
        if wait_until 60 no_open_alert; then
            switched=$(db_now)
            ranges_out
            if wait_until 60 open_alert; then
                new_after "$unjudged" "$switched" D
            else
                check_failed "(D) an alert cleared by its condition, then out again for a hold: no new alert within 60 s (a cleared alert was taken for a judged one)"
            fi
        else
            check_failed "(D) an alert not judged, then no range (the clear hold 2 s): not cleared within 60 s"
        fi
    else
        check_failed "(D) in range, then out for a hold after (C): no new alert within 60 s"
    fi
    stop_loop
    note "(D) checked: a stretch not judged clears by its condition and is raised again"

    # --- (E) judged after its figure came back in range (rev-i's F1 on R1) --------------------------
    # (D)'s last alert, open; the clear hold an hour again, so it stays open
    # while leaked test schemas is in range, then has no range; judged false
    # then (with no range, so no reading in range comes after the judgement
    # to end anything by itself); then out: the stretch had ended at the
    # reading in range, so a new alert fires a hold after the switch (a loop
    # that keeps the judged out-since raises nothing). Judged while still in
    # range, the readings in range after the judgement would end the stretch
    # in either loop, and the case would test nothing (rev-e's own R3 trace).
    late=$(open_key)
    if [ -z "$late" ]; then
        check_failed "(E) no open alert from (D) to judge: (E) not run"
    else
        SPAWN_GUARDRAIL_CLEAR_SECS=3600
        export SPAWN_GUARDRAIL_CLEAR_SECS
        ranges_out
        : >"$WORK/loop.log"
        start_loop
        wait_until 60 read_twice || harness "(E) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
        ranges_in
        settle E "in range"
        write_ranges
        settle E "no range"
        expect_eq "$(open_count)" 1 "(E) (D)'s alert still open in its clear hold (an hour) after leaked test schemas read in range, then no range"
        judge "$late" E
        sleep 6
        switched=$(db_now)
        ranges_out
        if wait_until 60 open_alert; then
            new_after "$late" "$switched" E
        else
            check_failed "(E) back in range, judged false while still open, then out for a hold: no new alert within 60 s (the ended stretch was kept as judged)"
        fi
        stop_loop
        note "(E) checked: an alert judged after its figure came back in range keeps no stretch"

        # --- (F) a judgement "right" keeps nothing judged (rev-g's right-kept) -----------------------
        # (E)'s new alert, open; the clear hold 2 s. Judged right while the
        # loop runs (the alert stays open: it was right), then no range: the
        # alert clears by its condition (089); then out: a new alert after its
        # hold. A loop that kept a judged stretch for a "right" judgement keeps
        # the cleared alert's out-since and raises nothing.
        right_key=$(open_key)
        if [ -z "$right_key" ]; then
            check_failed "(F) no open alert from (E) to judge: (F) not run"
        else
            SPAWN_GUARDRAIL_CLEAR_SECS=2
            export SPAWN_GUARDRAIL_CLEAR_SECS
            ranges_out
            : >"$WORK/loop.log"
            start_loop
            wait_until 60 read_twice || harness "(F) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
            capture "$SPAWN_BIN" alert judge "$right_key" right
            [ "$status" -eq 0 ] || harness "(F) judging $right_key right failed: $(err)"
            sleep 6
            write_ranges
            if wait_until 60 no_open_alert; then
                switched=$(db_now)
                ranges_out
                if wait_until 60 open_alert; then
                    new_after "$right_key" "$switched" F
                else
                    check_failed "(F) judged right, cleared by its condition, then out for a hold: no new alert within 60 s (a judgement 'right' kept a judged stretch)"
                fi
            else
                check_failed "(F) the alert judged right, then no range (the clear hold 2 s): not cleared within 60 s"
            fi
            stop_loop
            note "(F) checked: a judgement 'right' keeps nothing judged"
        fi
    fi
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "209: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "209: a judged guardrail stretch ends only on a reading in range, restarted or not"
exit 0
