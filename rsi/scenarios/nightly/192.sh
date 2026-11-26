#!/bin/sh
# Black box: a guardrail's raise hold survives a loop restart (objective 192).
#
# The problem (rev-e, 2026-11-14 23:4x, live): no guardrail_out_of_range
# alert had ever fired on live, although storage.days_left (8 against 180
# or more) and storage.leaked_test_schemas (1239 against 200 or less) were
# out of range all day. The raise hold (SPAWN_GUARDRAIL_RAISE_SECS, 2 h) was
# the loop's memory, and every deploy restarts the loop: 37 deploys in 24 h,
# almost no gap of 2 h, so the hold never completed.
#
# Required (read through `GET /api/alerts`):
#   (1) at the loop's first reading, a figure out of its range takes its
#       out-since from the hourly host.core_lines samples: the oldest sample
#       of the newest unbroken run of samples out of its current range, the
#       run reaching the loop's start. Its alert fires once that time is a
#       hold old: not a hold after the restart, and never before a hold.
#   (2) a sample in range ends the run, and so does a gap of more than two
#       sample periods (SPAWN_CORE_LINES_SAMPLE_SECS): then the hold starts
#       at the first reading, as before.
#
# A real `spawn loop` and `spawn serve` run on a scratch database, with a
# ranges file giving leaked test schemas 10^18 or more (out at every reading:
# its one test server, a second scratch database named by a URL file, counts
# 0). The figure is leaked test schemas, not leaked test schemas, so that 192 holds on
# main and with objective 182 alike (182 makes leaked test schemas a diagnostic with no
# range; leaked test schemas raises under storage in both, as 182's 089.sh).
# In range is 0 or more; a sample "out" carries 2 GiB-worth (under 10^18), a
# sample "in" 2 x 10^18.
# The samples are written by the store's own writer (spawn_host_record) and
# placed in the past. Four loop runs, each on its own samples:
#   (a) one sample out 2.5 h ago, inside the seeding read's window (the hold
#       plus two periods, 3700 s + 7200 s) but more than two periods before
#       the start (a gap): nothing within 20 s;
#   (b) samples out 3 h and 2 h ago, in 1 h ago (broken): nothing within 20 s;
#   (d) one sample out 3690 s ago, but leaked test schemas in range when the loop
#       starts (a ranges file of 0 or more), then out from a later
#       reading (the file back at 10^18): seeded once, at the loop's first
#       reading, so the hold starts at the reading that found it out:
#       nothing within 30 s (run before (c), whose open alert a later loop
#       would adopt);
#   (c) one sample out 3650 s ago, the hold 3700 s: the alert fires within
#       120 s of the start, its out_since_ms that sample's time.
#   (e) (cc-astra A87) (c)'s alert judged false, its sample then older than
#       the window, samples still out after it (and one in range from before
#       it began): no new alert within 20 s of a restart;
#   (f) a sample back in range after the judged stretch began, then out (the
#       hold 60 s for this run): a new alert, out since the sample after.
#   (g) (f)'s alert cleared by its condition (the clear hold 2 s), then out
#       again in a sample after it: a new alert, out since that sample (a
#       cleared alert is not a judged stretch).
#   (h) (R3) (g)'s alert judged false, no samples, the hold 15 s: (h1) out
#       from the first reading, (h2) no range at the first reading then out,
#       each for more than two holds: no new alert; (h3) no range at the
#       first reading, in range at later ones, then out: a new alert, out
#       since after the switch; (h4) (R4, flipped by objective 209) (h3)'s
#       alert judged false, out from the first reading, then no range, then
#       out for more than two holds: no new alert (only a fresh reading in
#       range ends a judged stretch, taken or not; 209.sh holds the rest).
# On the base, (c) gets no alert: its hold starts at the loop's start; and (e)
#   fires a second alert at once.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "192: harness: $*" >&2
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
SPAWN_DB_NAME="bb_192_hold_$run"
SPAWN_TMUX_SOCKET="bb-192-$run"
export SPAWN_TERMINAL_BACKEND=tmux
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
            echo "192: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "192: the guardrail alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'guardrail_out_of_range' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "192: harness: unexpected exit $rc" >&2
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
note() { echo "192: $*" >&2; }
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

# --- (a) a gap: one sample out 2.5 h ago, none after --------------------------------------------------
# Inside the seeding read's window (3700 s + 2 x 3600 s), so the gap rule,
# not the window, is what ends the run.

no_samples
sample 9000000 $((2 * GIB))
: >"$WORK/loop.log"
start_loop
wait_until 60 read_twice || harness "(a) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
sleep 20
expect_eq "$(alerts_of | jq -c length)" 0 "(a) a sample 2.5 h old, more than two sample periods before the start: no alert (the hold starts at the first reading)"
stop_loop
note "(a) checked: a gap ends the run"

# --- (b) broken: out 3 h and 2 h ago, in 1 h ago -------------------------------------------------------

no_samples
sample $((3 * HOUR)) $((2 * GIB))
sample $((2 * HOUR)) $((2 * GIB))
sample $((1 * HOUR)) 2000000000000000000
: >"$WORK/loop.log"
start_loop
wait_until 60 read_twice || harness "(b) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
sleep 20
expect_eq "$(alerts_of | jq -c length)" 0 "(b) the newest sample in range: no alert (the run out ended an hour ago)"
stop_loop
note "(b) checked: a sample in range ends the run"

# --- (d) seeded once: in range at the start, out from a later reading ----------------------------------

# The sample 3690 s old: a seed from it would be a hold old within seconds
# of the switch, so a loop that seeds again at a later reading fires well
# inside the 30 s; one that seeded once starts the hold at the switch.
no_samples
sample 3690000 $((2 * GIB))
write_ranges '{"min": 0}'
: >"$WORK/loop.log"
start_loop
wait_until 60 read_twice || harness "(d) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
sleep 3
write_ranges '{"min": 1000000000000000000}'
sleep 30
expect_eq "$(alerts_of | jq -c '[.[] | select(.state == "open")] | length')" 0 \
    "(d) leaked test schemas in range at the loop's first reading, out from a later one: no alert 30 s after (the samples seed once, at the first reading; the hold starts when the loop found it out)"
stop_loop
note "(d) checked: the samples seed once"

# --- (c) a restart mid-hold: out 3650 s ago, the hold 3700 s -------------------------------------------

no_samples
sample 3650000 $((2 * GIB))
first_out=$(q "select recorded_at_ms from spawn_events where session_id = 'session_control'
    and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines' order by recorded_at_ms limit 1")
: >"$WORK/loop.log"
start_loop
if wait_until 120 any_alert; then
    expect_eq "$(alerts_of | jq -c --argjson first "$first_out" --argjson started "$loop_started" '[.[] | {
        state, since: (.evidence.out_since_ms == $first),
        held: (.fired_at_ms - .evidence.out_since_ms >= 3700000),
        before_a_hold_from_the_start: (.fired_at_ms - $started < 3700000)}]')" \
        '[{"state":"open","since":true,"held":true,"before_a_hold_from_the_start":true}]' \
        "(c) one open alert, out since the sample of 3650 s ago, fired a hold after it and well before a hold from the restart"
else
    check_failed "(c) leaked test schemas, out in a sample 3650 s before the loop started (hold 3700 s), has no guardrail_out_of_range alert 120 s after the start: the hold restarted with the loop"
fi
stop_loop
note "(c) checked: the hold survives the restart"

# --- (e) a judged stretch, still out, after the samples' window slid (cc-astra A87) ------------------

# (c)'s alert, judged false by the owner while open. Its sample is then
# placed older than the next loop's window (3700 s + 7200 s), as an hour or
# more of a slid window would leave it, and leaked test schemas stays out: samples
# out 7000 s and 3600 s ago, and one in range 9000 s ago, before the judged
# stretch began. A loop that takes the oldest sample of the window as the
# stretch's start fires a new alert at once (7000 s is over the hold); the
# judged stretch never saw the figure back in range, so it goes on.
judged_key=$(alerts_of | jq -r '[.[] | select(.state == "open")][0].key // empty')
# Without (c)'s alert (the base: its hold restarts with the loop) there is
# nothing to judge: (e) and (f) are failed checks, not a harness fault.
if [ -z "$judged_key" ]; then
    check_failed "(e) no open alert from (c) to judge: (e) and (f) not run"
else
    capture "$SPAWN_BIN" alert judge "$judged_key" false
    [ "$status" -eq 0 ] || harness "(e) judging $judged_key failed: $(err)"
    no_samples
    sample 11000000 $((2 * GIB))
    sample 9000000 2000000000000000000
    sample 7000000 $((2 * GIB))
    sample 3600000 $((2 * GIB))
    : >"$WORK/loop.log"
    start_loop
    wait_until 60 read_twice || harness "(e) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
    sleep 20
    expect_eq "$(alerts_of | jq -c --arg judged "$judged_key" '[.[] | {judged: (.key == $judged), state, verdict}]')" \
        '[{"judged":true,"state":"cleared","verdict":"false"}]' \
        "(e) the stretch judged false, still out with no sample back in range after it began: no new alert 20 s after a restart whose samples begin later"
    stop_loop
    note "(e) checked: a judged stretch is not raised again"

    # --- (f) a sample back in range after the judged stretch began: a new stretch ----------------------

    # The hold 60 s for this loop run: in range 600 s ago (after the judged
    # stretch began), out 300 s ago, so the new stretch is a hold old at the
    # loop's first reading.
    no_samples
    sample 600000 2000000000000000000
    sample 300000 $((2 * GIB))
    new_out=$(q "select max(recorded_at_ms) from spawn_events where session_id = 'session_control'
        and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'")
    SPAWN_GUARDRAIL_RAISE_SECS=60
    export SPAWN_GUARDRAIL_RAISE_SECS
    : >"$WORK/loop.log"
    start_loop
    if wait_until 60 open_alert; then
        expect_eq "$(alerts_of | jq -c --argjson new "$new_out" '[.[] | select(.state == "open") | {since: (.evidence.out_since_ms == $new)}]')" \
            '[{"since":true}]' \
            "(f) one open alert, out since the sample after the one back in range"
    else
        check_failed "(f) a sample back in range after the judged stretch began, then out a hold: no new alert within 60 s (a new stretch was taken for the judged one)"
    fi
    stop_loop
    note "(f) checked: a sample back in range starts a new stretch"

    # --- (g) an alert cleared by its condition is not a judged stretch ---------------------------------
    # (f)'s alert, cleared by the figure back in range (the clear hold 2 s for
    # this run), then leaked test schemas out again in a sample 250 s ago, after (f)'s
    # out-since and with no sample in range since: the samples seed as in (1),
    # a new stretch under its own key (the hold still 60 s). Taking the cleared
    # alert for a judged one would keep (f)'s key and raise nothing.
    SPAWN_GUARDRAIL_CLEAR_SECS=2
    export SPAWN_GUARDRAIL_CLEAR_SECS
    write_ranges '{"min": 0}'
    : >"$WORK/loop.log"
    start_loop
    wait_until 60 read_twice || harness "(g) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
    if wait_until 60 no_open_alert; then
        stop_loop
        no_samples
        sample 250000 $((2 * GIB))
        cleared_out=$(q "select max(recorded_at_ms) from spawn_events where session_id = 'session_control'
            and kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'")
        write_ranges '{"min": 1000000000000000000}'
        : >"$WORK/loop.log"
        start_loop
        if wait_until 60 open_alert; then
            expect_eq "$(alerts_of | jq -c --argjson out "$cleared_out" '[.[] | select(.state == "open") | {since: (.evidence.out_since_ms == $out)}]')" \
                '[{"since":true}]' \
                "(g) one open alert, out since the sample after the alert its condition cleared"
        else
            check_failed "(g) an alert cleared by its condition, then out again in a sample after it: no new alert within 60 s (the cleared alert was taken for a judged one)"
        fi
    else
        check_failed "(g) (f)'s alert was not cleared within 60 s of leaked test schemas back in range (the clear hold 2 s)"
    fi
    stop_loop
    note "(g) checked: an alert cleared by its condition is not a judged stretch"

    # --- (h) (R3, cc-astra A87) the judged out-since waits for a fresh reading out -------------------
    # (g)'s alert, judged false; no samples at all, the hold 15 s. Each loop
    # run below starts from the store's rows alone, so the judged stretch is
    # all it knows of the figure.
    #   (h1) out from the first reading, for more than two holds: the judged
    #        stretch goes on (a loop that drops the judged out-since starts a
    #        new hold at its first reading and fires within 15 s);
    #   (h2) no range for leaked test schemas at the first reading (`no_range`), then
    #        out for more than two holds: still the judged stretch (a reading
    #        with no range is no evidence it ended);
    #   (h3) no range at the first reading, in range at later readings, then
    #        out: a fresh reading in range (not the first reading's samples,
    #        which (h2)'s rule makes no evidence) ended the judged stretch, so
    #        a new alert fires a hold after the reading that found it out
    #        again.
    judged_h=$(alerts_of | jq -r '[.[] | select(.state == "open")][0].key // empty')
    if [ -z "$judged_h" ]; then
        check_failed "(h) no open alert from (g) to judge: (h) not run"
    else
        capture "$SPAWN_BIN" alert judge "$judged_h" false
        [ "$status" -eq 0 ] || harness "(h) judging $judged_h failed: $(err)"
        no_samples
        SPAWN_GUARDRAIL_RAISE_SECS=15
        export SPAWN_GUARDRAIL_RAISE_SECS
        ranges_out() { write_ranges '{"min": 1000000000000000000}'; }
        ranges_out
        : >"$WORK/loop.log"
        start_loop
        wait_until 60 read_twice || harness "(h1) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
        sleep 35
        expect_eq "$(alerts_of | jq -c '[.[] | select(.state == "open")] | length')" 0 \
            "(h1) the stretch judged false, out from the restarted loop's first reading for more than two holds (15 s): no new alert (the judged out-since is kept until a fresh reading out takes it)"
        stop_loop
        note "(h1) checked: a fresh reading out goes on with the judged stretch"

        write_ranges
        : >"$WORK/loop.log"
        start_loop
        wait_until 60 read_twice || harness "(h2) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
        sleep 3
        ranges_out
        sleep 35
        expect_eq "$(alerts_of | jq -c '[.[] | select(.state == "open")] | length')" 0 \
            "(h2) leaked test schemas with no range at the restarted loop's first reading, then out for more than two holds: no new alert (a reading with no range does not end the judged stretch)"
        stop_loop
        note "(h2) checked: a reading with no range is no evidence"

        # No range at the first reading (no evidence, as (h2)), so the loop's
        # own first sample cannot settle anything; then in range for a few
        # readings, then out.
        write_ranges
        : >"$WORK/loop.log"
        start_loop
        wait_until 60 read_twice || harness "(h3) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
        sleep 3
        write_ranges '{"min": 0}'
        sleep 5
        switched=$(db_now)
        ranges_out
        if wait_until 60 open_alert; then
            expect_eq "$(alerts_of | jq -c --arg judged "$judged_h" --argjson switched "$switched" '[.[] | select(.state == "open") | {new: (.key != $judged), after_the_switch: (.evidence.out_since_ms >= $switched)}]')" \
                '[{"new":true,"after_the_switch":true}]' \
                "(h3) one new open alert, out since a reading after leaked test schemas was found out again"
        else
            check_failed "(h3) leaked test schemas with no range at the restarted loop's first reading, in range at later readings, then out: no new alert within 60 s (a fresh reading in range did not end the judged stretch)"
        fi
        stop_loop
        note "(h3) checked: a fresh reading in range ends the judged stretch"

        # (h4) (R4; objective 209 flips its rule (a)): (h3)'s alert judged
        # false; out from the restarted loop's first reading (it takes the
        # judged out-since), then no range, then out for more than two holds:
        # still the judged stretch, so no new alert. (A loop that ends a taken
        # stretch at a reading with no range fires a hold after the switch.)
        judged_h4=$(alerts_of | jq -r '[.[] | select(.state == "open")][0].key // empty')
        if [ -z "$judged_h4" ]; then
            check_failed "(h4) no open alert from (h3) to judge: (h4) not run"
        else
            capture "$SPAWN_BIN" alert judge "$judged_h4" false
            [ "$status" -eq 0 ] || harness "(h4) judging $judged_h4 failed: $(err)"
            ranges_out
            : >"$WORK/loop.log"
            start_loop
            wait_until 60 read_twice || harness "(h4) the loop made no guardrail reading within 60 s: $(tail -n 5 "$WORK/loop.log")"
            sleep 3
            write_ranges
            sleep 5
            ranges_out
            sleep 35
            expect_eq "$(alerts_of | jq -c '[.[] | select(.state == "open")] | length')" 0 \
                "(h4) leaked test schemas out at the restarted loop's first reading (taking the judged out-since), then no range, then out for more than two holds: no new alert (a reading with no range does not end a taken judged stretch)"
            stop_loop
            note "(h4) checked: a taken judged stretch outlives a reading with no range"
        fi
    fi
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "192: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "192: a guardrail's raise hold survives a loop restart"
exit 0
