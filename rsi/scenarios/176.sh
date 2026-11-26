#!/bin/sh
# Objective 176 [storage][performance][sustainability]: the core metric set
# the owner adopted on 2026-11-14 18:42 (sync/core-metric-set-2026-11-14.md).
#
# The problem: the Core lines answer and card still show the frame of
# 2026-11-12, four lines (performance, storage, observability,
# recoverability) and every figure later objectives added to them. The owner
# adopted three lines in the order storage, performance, sustainability
# (which absorbs recoverability, observability and cost), 3 north stars and
# 9 guardrails, dropped some figures, and demoted others to an alert only or
# to a diagnostic on the storage page. All of it is monitoring only.
#
# Required (read through `GET /api/core-lines`, a real `spawn loop`'s
# alerts in `GET /api/alerts`, and web/src/coreLines.ts under node):
#   (1) `lines` is the three lines in order, each its north star and
#       guardrails by key:
#         storage: storage.bytes_per_landing; storage.rebuildable_bytes,
#           storage.kept_share
#         performance: performance.report_to_live; performance.gate_time,
#           performance.decided_nothing, performance.first_pass
#         sustainability: observability.interventions_per_agent_hour
#           (window 24h); recoverability.restores_passed,
#           observability.needs_you_right, observability.unpriced_share,
#           recoverability.cloud_dump_age
#   (2) A figure no code computes yet (the storage north star, S1 and S2)
#       has value null and state `not_measured_yet`, with its range if the
#       ranges file gives one. A figure with a real value is never that: the
#       sustainability north star reads its value (0.125 here) and its
#       state against its range.
#   (3) `alert_figures` lists the figures demoted to an alert, each with its
#       line and its state against its range (objective 182: free disk is a
#       diagnostic now, with no range and no line):
#       recoverability.cloud_bundle_age, recoverability.nightly_age,
#       recoverability.nightly_red (sustainability: here 1, out); the drills' two ages
#       only with a recovery table (none here). `diagnostics` lists, in
#       order, storage.free_bytes, storage.unowned_scratch_bytes,
#       storage.disk_fall, storage.grew_most, storage.archive_growth,
#       storage.leaked_test_schemas (dollars per landing is a reading of the
#       sustainability line since objective 182). Leaked
#       test schemas, shown here, still raises (rev-e's 176 R1 F1: "stays
#       visible until it has read 0 for 7 days, then an alert only", 182):
#       it carries its line, storage; no other diagnostic does.
#   (4) The dropped figures (storage.days_left, storage.database_ratio,
#       storage.test_database_bytes, storage.test_database_schemas,
#       observability.not_measured) are in none of the three lists. A range
#       the file still gives one is listed in `ignored_ranges` with the
#       reason "retired by the owner's core metric set (2026-11-14)"; an
#       unknown key stays "unknown figure".
#   (5) coreLinesView(answer): rows Storage, Performance, Sustainability;
#       "not measured yet" for the three storage figures; the alert figures
#       on no row; `diagnostics` the six in words, the out-of-range one in
#       the warning tone.
#   (6) The loop's alert step reads the lines and the alert figures: a line
#       figure or an alert figure held out of its range raises one
#       `guardrail_out_of_range` alert whose evidence names its line (here
#       the interventions, a line's figure, and the
#       nightly's reds, an alert figure, under sustainability; leaked test
#       schemas, a lined diagnostic, under storage); any other
#       diagnostic held out of its range raises nothing.
#
# The world: one execution from 10 h to 2 h ago (8 agent-hours in the day)
# and a mission resumed 5 h ago (one intervention: 0.125 an agent-hour); a
# scratch root holding one idle unmarked folder (2 days old), so scratch
# nobody owns has bytes; one nightly fact file held red (supply); a ranges
# file giving free disk 10^18 bytes or more, the interventions 0.1 or less,
# scratch nobody owns 0 bytes or less, nightly reds 0 or less, leaked test
# schemas 0 or less (all out), the storage north star 50 MiB or less, and
# ranges for two dropped figures and one unknown key; and a test database
# of the scenario's own holding one leaked schema (its pid gone, made 25 h
# ago).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "176: harness: $*" >&2
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

# The candidate's tree: the gate runs a scenario from inside it, and its
# web/src is what the page would run (the controller holds main's copy).
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
[ -f "$tree/web/src/home.ts" ] || harness "no web/src/home.ts under $tree (run from the candidate's tree)"
for tool in psql git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
# A scratch HOME, and isolated XDG dirs, in WORK: nothing here reads
# or writes the real home, nor is left outside it (cc-rules (a), 2026-11-12).
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
mkdir -p "$HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
export HOME
# The runtime directory too (backlog S20): what Spawn keeps there stays in
# WORK. systemd's user manager stays reachable by its bus.
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
SPAWN_DB_NAME="bb_176_core_set_$run"
TESTS_DB="bb_176_tests_$run"
SPAWN_HERDR_SESSION="bb-176-$hc_id"
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
tests_made=""
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
    if [ -n "$tests_made" ]; then
        psql -d postgres -q -c "drop database if exists $TESTS_DB with (force)" >/dev/null 2>&1 || true
    fi
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
        if [ -s "$WORK/lines.json" ]; then
            echo "176: the last answer (first 4000 bytes):" >&2
            head -c 4000 "$WORK/lines.json" >&2
            echo >&2
        fi
        if [ -s "$WORK/loop.log" ]; then
            echo "176: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "176: the guardrail alerts (subject, state, evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select subject, state, evidence::text
                from spawn_alerts where kind = 'guardrail_out_of_range' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "176: harness: unexpected exit $rc" >&2
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
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_TERMINAL_BACKEND SPAWN_HERDR_MODE SPAWN_HERDR_SHARDS \
    SPAWN_HERDR_PROGRAM SPAWN_TMUX_SOCKET SPAWN_TMUX_PROGRAM TMUX TMUX_PANE TMUX_TMPDIR \
    HERDR_SOCKET_PATH HERDR_SESSION HERDR_PANE_ID HC_SOCKET HC_SESSION \
    http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
mkdir -p "$SPAWN_CLAUDE_PROJECTS"
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
# The alert step reads every 2 s (default 30 s).
SPAWN_ALERT_CHECK_SECS=2
# The guardrails under test: read every second, raised after 6 s out of
# range, cleared after 6 s back in (defaults 300 s, 2 h and 1 h).
SPAWN_ALERT_GUARDRAIL_SECS=1
SPAWN_GUARDRAIL_RAISE_SECS=6
SPAWN_GUARDRAIL_CLEAR_SECS=6
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
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_ALERT_GUARDRAIL_SECS SPAWN_GUARDRAIL_RAISE_SECS SPAWN_GUARDRAIL_CLEAR_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
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
note() { echo "176: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
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
lines_json() { # the answer into $WORK/lines.json; its code
    curl -s --noproxy '*' -o "$WORK/lines.json" -w '%{http_code}' "$base/api/core-lines" 2>/dev/null || echo 000
}
alerts_json() {
    code=$(curl -s --noproxy '*' -o "$WORK/alerts.json" -w '%{http_code}' "$base/api/alerts" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/alerts answered $code: $(head -c 300 "$WORK/alerts.json" 2>/dev/null)"
}
answer() { jq -c "$1" "$WORK/lines.json"; }

# --- the world: agent time, one intervention, idle scratch, the ranges ----------------------------

seed_now=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
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
-- Agent time as the Bridge records it (049, 100): 8 agent-hours in the day.
-- The workspace is a real identifier: the loop below reads it.
create function pg_temp.bridge(h numeric, name text, detail jsonb) returns void language sql as \$\$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_w_1', 'session_worker_w_1', 'exec_bb_176_w', 'bridge', null,
            jsonb_build_object('name', name, 'detail', detail), nextval('seed_seq'), pg_temp.ms(h)) \$\$;
select pg_temp.bridge(-10, 'execution.started', '{"engine":"claude","workspace":"local:/w/a","command_id":176}');
select pg_temp.bridge(-2, 'execution.ended', '{"cause":"exited","exit_code":0}');
select pg_temp.rec(-5, '{"type":"mission.resumed","id":1}');
commit;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the history failed: $(err)"

# Scratch nobody owns: one idle unmarked folder, two days without a change.
S=$WORK/scratch
mkdir -p "$S/idle"
head -c 400000 /dev/zero >"$S/idle/data"
find "$S/idle" -exec touch -h -d '2 days ago' {} + || harness "cannot age the idle folder"
idle_bytes=$(find "$S/idle" -type f -printf '%s\n' | awk '{ s += $1 } END { print s }')

# One nightly fact file held red, as scripts/rsi/nightly.sh writes it (087).
mkdir -p "$SPAWN_WORKSPACE_ROOT/nightly"
printf '{"v":1,"main":"bbbb","at":%s,"state":"red","failed":["023.sh"],"last_green":"aaaa","red_since":"bbbb","flaky":[],"skipped":[],"gates_beside":0,"skipped_runs":0,"judged_at":%s}\n' \
    "$((seed_now - 3600000))" "$((seed_now - 3600000))" >"$SPAWN_WORKSPACE_ROOT/nightly/supply.json"

# A test database of the scenario's own, handed to serve and the loop as
# the "test" server (never the shared one): one schema left by a test that
# is gone (its pid exited, made 25 h ago), so leaked test schemas reads 1.
psql -d postgres -tA -c "create database $TESTS_DB" >/dev/null 2>&1 ||
    harness "cannot create the scenario's test database"
tests_made=1
SPAWN_TEST_DATABASE_URL="$server_url/$TESTS_DB"
export SPAWN_TEST_DATABASE_URL
unset SPAWN_FLEET_TEST_DATABASE_URL 2>/dev/null || true
sh -c 'exit 0' &
dead_pid=$!
wait "$dead_pid" || true
[ ! -e "/proc/$dead_pid" ] || harness "the exited process $dead_pid is still there"
psql -d "$TESTS_DB" -q -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL || harness "seeding the test database failed"
create schema spawn_test_${dead_pid}_a_0_old;
comment on schema spawn_test_${dead_pid}_a_0_old is '{"made_at_ms": $((seed_now - 25 * 3600000))}';
SQL
unset SPAWN_CORE_LINES_FILE 2>/dev/null || true
cat >"$SPAWN_WORKSPACE_ROOT/core-lines.json" <<JSON
{"ranges": {
  "storage.bytes_per_landing": {"max": 52428800},
  "observability.interventions_per_agent_hour": {"max": 0.1},
  "storage.free_bytes": {"min": 1000000000000000000},
  "storage.unowned_scratch_bytes": {"max": 0},
  "recoverability.nightly_red": {"max": 0},
  "storage.leaked_test_schemas": {"max": 0},
  "storage.days_left": {"min": 180},
  "storage.database_ratio": {"max": 10},
  "storage.free_bites": {"min": 1}
 },
 "storage": {"scratch_roots": ["$S"]}}
JSON
start_serve

# --- (1) to (4): the answer ---------------------------------------------------------------------

# The scratch walk is read beside the request, never in it: the first
# answers may not have its bytes yet.
walked() {
    [ "$(lines_json)" = 200 ] &&
        [ "$(answer '[(.diagnostics // [])[] | select(.key == "storage.unowned_scratch_bytes") | .value | numbers] | length')" = 1 ]
}
if ! wait_until 60 walked; then
    code=$(lines_json)
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 300 "$WORK/lines.json")"
    check_failed "(3) no diagnostics' storage.unowned_scratch_bytes with a value within 60 s: $(answer '.diagnostics' | head -c 400)"
fi
cp "$WORK/lines.json" "$WORK/answer.json"

expect_eq "$(answer '[.lines[] | {line, north_star: .north_star.key, guardrails: [.guardrails[].key]}]')" \
    '[{"line":"storage","north_star":"storage.bytes_per_landing","guardrails":["storage.rebuildable_bytes","storage.kept_share"]},{"line":"performance","north_star":"performance.report_to_live","guardrails":["performance.gate_time","performance.decided_nothing","performance.first_pass"]},{"line":"sustainability","north_star":"observability.interventions_per_agent_hour","guardrails":["recoverability.restores_passed","observability.needs_you_right","observability.unpriced_share","recoverability.cloud_dump_age"]}]' \
    "(1) the three lines in order, each north star and its guardrails"
note "(1) checked: the lines"

expect_eq "$(answer '[.lines[0] | ([.north_star] + .guardrails)[] | {key, value, range, state}]')" \
    '[{"key":"storage.bytes_per_landing","value":null,"range":{"max":52428800},"state":"not_measured_yet"},{"key":"storage.rebuildable_bytes","value":null,"range":null,"state":"not_measured_yet"},{"key":"storage.kept_share","value":null,"range":null,"state":"not_measured_yet"}]' \
    "(2) the storage north star, S1 and S2: not measured yet, with a range when the file gives one"
expect_eq "$(answer '[.lines[1:][] | ([.north_star] + .guardrails)[] | select(.state == "not_measured_yet") | .key]')" '[]' \
    "(2) no performance or sustainability figure is not measured yet"
expect_eq "$(answer '.lines[2].north_star | {value, window, range, state}')" \
    '{"value":0.125,"window":"24h","range":{"max":0.1},"state":"out"}' \
    "(2) the sustainability north star: one intervention over 8 agent-hours in the day, out of 0.1 or less"
note "(2) checked: not measured yet, and a real value"

expect_eq "$(answer '[(.alert_figures // [])[] | {line, key}]')" \
    '[{"line":"sustainability","key":"recoverability.cloud_bundle_age"},{"line":"sustainability","key":"recoverability.nightly_age"},{"line":"sustainability","key":"recoverability.nightly_red"}]' \
    "(3) the alert figures, each with its line (no recovery table: no drill ages)"
expect_eq "$(answer '[(.diagnostics // [])[] | select(.key == "storage.free_bytes") | {valued: (.value | type == "number"), range, state}]')" \
    '[{"valued":true,"range":null,"state":"no_range"}]' \
    "(3) free disk: read, a diagnostic with no range (objective 182), whatever the file gives"
expect_eq "$(answer '[(.alert_figures // [])[] | select(.key == "recoverability.nightly_red") | {line, value, range, state}]')" \
    '[{"line":"sustainability","value":1,"range":{"max":0},"state":"out"}]' \
    "(3) nightly sources held red: one (supply), out of its range of none, under sustainability"
expect_eq "$(answer '[(.diagnostics // [])[].key]')" \
    '["storage.free_bytes","storage.unowned_scratch_bytes","storage.disk_fall","storage.grew_most","storage.archive_growth","storage.leaked_test_schemas","storage.landed_scratch_bytes"]' \
    "(3) the diagnostics, in order (objective 232: landed objectives' scratch last)"
expect_eq "$(answer '[(.diagnostics // [])[] | select(.key == "storage.unowned_scratch_bytes") | {name, value, range, state}]')" \
    "[{\"name\":\"Scratch nobody owns\",\"value\":$idle_bytes,\"range\":{\"max\":0},\"state\":\"out\"}]" \
    "(3) scratch nobody owns: the idle folder's bytes, out of 0 or less"
expect_eq "$(answer '[.lines[] | ([.north_star] + .guardrails)[].key] | map(select(. == "storage.free_bytes" or . == "storage.unowned_scratch_bytes" or startswith("storage.disk_") or . == "performance.usd_per_landing"))')" '[]' \
    "(3) no alert figure or diagnostic is on a line"
expect_eq "$(answer '[(.diagnostics // [])[] | select(.line != null) | {line, key, value, range, state}]')" \
    '[{"line":"storage","key":"storage.leaked_test_schemas","value":1,"range":{"max":0},"state":"out"}]' \
    "(3) the one diagnostic that still raises: leaked test schemas, 1, out of 0 or less, under storage"
note "(3) checked: the alert figures and the diagnostics"

dropped='["storage.days_left","storage.database_ratio","storage.test_database_bytes","storage.test_database_schemas","observability.not_measured"]'
expect_eq "$(jq -c --argjson d "$dropped" '[(.lines[] | ([.north_star] + .guardrails)[]), (.alert_figures // [])[], (.diagnostics // [])[] | .key | select(IN($d[]))]' "$WORK/lines.json")" '[]' \
    "(4) no dropped figure in the lines, the alert figures or the diagnostics"
expect_eq "$(answer '.ignored_ranges | sort_by(.key)')" \
    "[{\"key\":\"storage.database_ratio\",\"reason\":\"retired by the owner's core metric set (2026-11-14)\"},{\"key\":\"storage.days_left\",\"reason\":\"retired by the owner's core metric set (2026-11-14)\"},{\"key\":\"storage.free_bites\",\"reason\":\"unknown figure\"},{\"key\":\"storage.free_bytes\",\"reason\":\"an alert of its own (disk_low, objective 204)\"}]" \
    "(4) a dropped figure's range is retired by name; an unknown key stays unknown"
note "(4) checked: the dropped figures"

# --- (5) the card's model -----------------------------------------------------------------------

cat >"$WORK/set.test.mjs" <<'TEST'
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
const { coreLinesView } = await import(`${process.env.BB_TREE}/web/src/coreLines.ts`);
const answer = JSON.parse(readFileSync(`${process.env.BB_WORK}/answer.json`, "utf8"));

test("(5) the three rows, not measured yet, the diagnostics apart", () => {
  const view = coreLinesView(answer);
  assert.deepEqual(view.rows.map((row) => row.line), ["Storage", "Performance", "Sustainability"]);
  assert.deepEqual(view.rows[0].north_star,
    { name: "Bytes a landing leaves behind", value: "not measured yet", range: "50.0 MB or less", tone: "muted" });
  assert.deepEqual(view.rows[0].guardrails.map((mark) => mark.value), ["not measured yet", "not measured yet"]);
  const star = view.rows[2].north_star;
  assert.match(star.name, /^Human interventions per agent-hour/);
  assert.equal(star.tone, "warning", "0.125 is out of 0.1 or less");
  assert.ok(!["not measured yet", "not collected yet"].includes(star.value), `a value in words: ${star.value}`);
  const names = view.rows.flatMap((row) => [row.north_star, ...row.guardrails]).map((mark) => mark.name);
  assert.ok(!names.includes("Free disk") && !names.some((name) => name.startsWith("Scratch nobody owns")), `no alert figure or diagnostic on a row: ${names}`);
  assert.equal(view.diagnostics.length, 7, "the six and landed objectives' scratch (objective 232)");
  assert.match(view.diagnostics[0].name, /^Free disk/);
  assert.notEqual(view.diagnostics[0].tone, "warning", "free disk has no range (objective 182)");
  assert.deepEqual(view.diagnostics[1].tone, "warning", "scratch nobody owns is out of its range");
  assert.match(view.diagnostics[1].name, /^Scratch nobody owns/);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/set.test.mjs" >"$WORK/set.out" 2>&1; then
    check_failed "(5) web/src/coreLines.ts: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/set.out" | head -n 14 | tr '\n' ' ' | head -c 1200)"
fi
note "(5) checked: the card's model"

# --- (6) the loop's alerts ------------------------------------------------------------------------

"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
alert_of() { # <key>: its guardrail_out_of_range alerts, any state
    alerts_json
    jq -c --arg s "guardrail $1" '[.[] | select(.kind == "guardrail_out_of_range" and .subject == $s)]' "$WORK/alerts.json"
}
open_for() { [ "$(alert_of "$1" | jq '[.[] | select(.state == "open")] | length')" = 1 ]; }
for key in observability.interventions_per_agent_hour recoverability.nightly_red storage.leaked_test_schemas; do
    wait_until 60 open_for "$key" ||
        check_failed "(6) $key (out of its range from the first reading) has no open guardrail_out_of_range alert 60 s after the loop started: $(alert_of "$key" | head -c 400)"
done
expect_eq "$(alert_of observability.interventions_per_agent_hour | jq -c '[.[] | .evidence | {line, key}]')" \
    '[{"line":"sustainability","key":"observability.interventions_per_agent_hour"}]' "(6) the interventions' alert names the sustainability line"
expect_eq "$(alert_of recoverability.nightly_red | jq -c '[.[] | .evidence | {line, key}]')" \
    '[{"line":"sustainability","key":"recoverability.nightly_red"}]' "(6) the nightly reds' alert (an alert figure) names the sustainability line"
expect_eq "$(alert_of storage.leaked_test_schemas | jq -c '[.[] | .evidence | {line, key}]')" \
    '[{"line":"storage","key":"storage.leaked_test_schemas"}]' "(6) leaked test schemas (a diagnostic still raising) names the storage line"
# Scratch nobody owns has been out as long by now (its walk is read beside
# the loop's reading too); 12 s more is twice the raise time.
sleep 12
alerts_json
expect_eq "$(jq -c '[.[] | select(.kind == "guardrail_out_of_range") | .subject] | unique' "$WORK/alerts.json")" \
    '["guardrail observability.interventions_per_agent_hour","guardrail recoverability.nightly_red","guardrail storage.leaked_test_schemas"]' \
    "(6) no alert for a plain diagnostic (scratch nobody owns, out of its range; free disk, whose file range is not applied, objective 182), nor any other"
note "(6) checked: the alerts"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "176: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "176: the Core lines answer and card are the owner's core metric set: three lines, figures not measured yet said so, alert figures and diagnostics apart, dropped figures retired"
exit 0
