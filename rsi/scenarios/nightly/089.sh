#!/bin/sh
# Black box: a guardrail that stays out of its healthy range raises one
# alert in Needs you, and it clears by itself once the figure has stayed
# back in range; neither flaps on a single reading.
#
# The problem (owner, 2026-11-12 13:30; sync/roadmap-effects.md, "Showing
# and alerting"; rev-e's hysteresis note, sync/notes-guardrails-rev-e.md):
# the Core lines card (objective 084) says which figures are out of their
# range, but only to someone looking at it. The owner asked for one alert
# kind, "guardrail out of range", naming the line, the figure, its value
# and its range, raised once, shown in Needs you, clearing by itself. A
# range within about 5% of today would raise and clear every hour, which
# would make Needs you wrong, the observability north star; so a figure
# must stay out for a while before it raises and back in for a while before
# it clears. Monitoring only: no gate, integrator or deploy reads it.
#
# Required (read through `GET /api/alerts`):
#   (1) the loop's alert step reads the Core lines figures (the same as
#       `GET /api/core-lines`, with the ranges file read at each reading)
#       every SPAWN_ALERT_GUARDRAIL_SECS (default 300). A figure whose state
#       has been `out` at every reading for SPAWN_GUARDRAIL_RAISE_SECS
#       (default 7200) opens one `guardrail_out_of_range` alert, subject
#       "guardrail <key>", evidence {line, key, name, value, range,
#       out_since_ms (its first reading out), reading_ms}; its text names
#       the line, the figure's name, its value and its range. One open per
#       figure. A figure in range, without a range or without a value opens
#       nothing.
#   (2) an open one clears once its figure has not been `out` at any reading
#       for SPAWN_GUARDRAIL_CLEAR_SECS (default 3600), its cleared evidence
#       that reading ({key, state, value, range, reading_ms}).
#
# A real `spawn loop` and `spawn serve` run on a scratch database with a
# ranges file at <workspace root>/core-lines.json: leaked test schemas at
# most -1 (out from the first reading, whatever it counts), the database
# against its log at most 10^12 (in). Then leaked test schemas' range is
# moved to 10^18 or less. Its one test server is a second scratch database,
# named by a URL file (objective 182: free disk, the figure this used
# before, has no range now; its alert is disk_low's).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "089: harness: $*" >&2
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
for tool in psql git jq od bash sed grep awk curl node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_089_guardrail_$run"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "089"
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
test_made=""
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
    if [ -n "$test_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "${SPAWN_DB_NAME}_t" >/dev/null 2>&1 || true
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
            echo "089: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "089: the database_size alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'database_size' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "089: harness: unexpected exit $rc" >&2
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
export HOME SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
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
note() { echo "089: $*" >&2; }
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


# --- the ranges, the loop -------------------------------------------------------------------

# No test database is handed to the loop in its environment: leaked test
# schemas reads the scenario's own test database, named by a URL file
# (mode 0600, never printed).
test_url=$WORK/test.url
(umask 077 && printf '%s\n' "$server_url/${SPAWN_DB_NAME}_t" >"$test_url") || harness "cannot write the URL file"
unset SPAWN_TEST_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL 2>/dev/null || true
ranges=$SPAWN_WORKSPACE_ROOT/core-lines.json
write_ranges() { # <leaked test schemas' max>
    printf '{"ranges": {"storage.leaked_test_schemas": {"max": %s}, "storage.database_ratio": {"max": 1000000000000}},\n "storage": {"test_database_url_files": [{"name": "bb", "url_file": "%s"}]}}\n' \
        "$1" "$test_url" >"$ranges"
}
write_ranges -1
started=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
"$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
loop_pid=$!
start_serve

alert_of() { # <key>: its guardrail_out_of_range alerts in /api/alerts, any state
    alerts_json
    jq -c --arg s "guardrail $1" '[.[] | select(.kind == "guardrail_out_of_range" and .subject == $s)]' "$WORK/alerts.json"
}
open_for() { [ "$(alert_of "$1" | jq '[.[] | select(.state == "open")] | length')" = 1 ]; }

# --- (1) leaked test schemas stays out: one alert, not before 6 s -------------------------------

if wait_until 60 open_for storage.leaked_test_schemas; then
    expect_eq "$(alert_of storage.leaked_test_schemas | jq -c '[.[] | .evidence | {line, key, name, range, valued: (.value | type == "number")}]')" \
        '[{"line":"storage","key":"storage.leaked_test_schemas","name":"Leaked test schemas","range":{"max":-1},"valued":true}]' \
        "(1) leaked test schemas' guardrail_out_of_range alert: its evidence"
    expect_eq "$(alert_of storage.leaked_test_schemas | jq -c --argjson t "$started" '[.[] | {
        held: (.fired_at_ms - .evidence.out_since_ms >= 6000),
        since: (.evidence.out_since_ms >= $t),
        text: ((.text | test("[Ss]torage")) and (.text | contains("Leaked test schemas")))}]')" \
        '[{"held":true,"since":true,"text":true}]' \
        "(1) raised only after 6 s out of range, counted from a reading of this loop; its text names the line and the figure"
else
    check_failed "(1) leaked test schemas (out of a range of -1 or less from the first reading) has no open guardrail_out_of_range alert 60 s after the loop started: $(alert_of storage.leaked_test_schemas | head -c 400)"
fi
expect_eq "$(alert_of storage.database_ratio | jq -c 'length')" 0 "(1) no alert for the database against its log (in its range)"
alerts_json
expect_eq "$(jq -c '[.[] | select(.kind == "guardrail_out_of_range") | .subject] | unique' "$WORK/alerts.json")" '["guardrail storage.leaked_test_schemas"]' \
    "(1) no other guardrail alert (the other figures have no range)"
note "(1) checked: the alert"

# --- (2) back in range: it clears, not before 6 s ------------------------------------------------

moved=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
write_ranges 1000000000000000000
cleared() { [ "$(alert_of storage.leaked_test_schemas | jq -c '[.[] | .state]')" = '["cleared"]' ]; }
if wait_until 60 cleared; then
    expect_eq "$(alert_of storage.leaked_test_schemas | jq -c --argjson t "$moved" '[.[] | {
        held: (.cleared_at_ms - $t >= 6000),
        evidence: (.cleared_evidence | {key, state, range})}]')" \
        '[{"held":true,"evidence":{"key":"storage.leaked_test_schemas","state":"in","range":{"max":1000000000000000000}}}]' \
        "(2) cleared only after 6 s back in range; its cleared evidence the reading"
else
    check_failed "(2) leaked test schemas' alert did not clear within 60 s of its range moving to 10^18 or less: $(alert_of storage.leaked_test_schemas | head -c 400)"
fi
note "(2) checked: the clearing"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "089: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "089: a guardrail held out of range raises one alert, and it clears once held back in range"
exit 0
