#!/bin/sh
# Objective 148 [observability]: "Needs you is right", the observability
# line's north star, is collected from the owner's judgements and never from
# a clearing (A23): over the last 7 days, of the needs (alerts, never
# notices) judged, the share judged right, said with how many were judged
# and how many not, and not collected under 5 judged. Before, it was never
# collected (null since A23, owner 11-12), and `spawn alert judge` took only
# false, stale and duplicate: a need could not be judged right.
#
# Black box, through `spawn alert judge`, `spawn serve`'s GET
# /api/core-lines and GET /api/alerts, and web/src/coreLines.ts under node.
# Six needs and one notice are fired through spawn_alert_record, the writer
# the loop's alert step uses.
#   (1) `spawn alert judge <key> right` is taken; an open need judged right
#       stays open (its outcome right); one judged false is cleared.
#   (2) 4 judged (right, right, false, stale): the north star has no value
#       and says judged {fired 6, judged 4, unjudged 2}; the notice is
#       refused a verdict.
#   (3) a 5th, right: the north star is 3 / 5 = 0.6, judged {6, 5, 1}.
#   (4) the card says "Needs you is right (5 judged, 1 unjudged of 6)".
#   (5) a need fired 2 days ago and one fired 8 days ago (seeded records,
#       folded), each judged false: the 2-day one counts, the 8-day one
#       does not: 3 / 6 = 0.5, judged {7, 6, 1}.
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "148: harness: $*" >&2
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
for tool in psql tmux git jq od bash sed grep awk curl node df head; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_148_judged_$run"
SPAWN_TMUX_SOCKET="bb-148-$run"
# The tmux server's socket lives in WORK, so the cleanup removes nothing
# outside it (cc-rules (a), 2026-11-12).
TMUX_TMPDIR=$WORK/tmux-sock
mkdir -p "$TMUX_TMPDIR"
export TMUX_TMPDIR
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

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
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
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
            echo "148: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "148: the alerts (key, state, verdict):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, state, verdict from spawn_alerts order by key" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/api.json" ]; then
            echo "148: the answer's observability line:" >&2
            jq -c '.lines[] | select(.line == "sustainability")' "$WORK/api.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "148: harness: unexpected exit $rc" >&2
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
SPAWN_TMUX_PROGRAM=$WORK/tmux
SPAWN_WORKER=worker-bb
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_ALERT_CHECK_SECS=2
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
    SPAWN_CLAUDE_PROJECTS SPAWN_TMUX_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
    SPAWN_ALERT_CHECK_SECS SPAWN_STALL_ACTIVITY_SECS SPAWN_STALL_DELIVERY_SECS \
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
note() { echo "148: $*" >&2; }
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
near() { # <got> <want> <slack>: |got - want| <= slack
    [ -n "$1" ] && [ "$1" != null ] && awk -v g="$1" -v w="$2" -v s="$3" 'BEGIN { d = g - w; if (d < 0) d = -d; exit !(d <= s) }'
}

# A scratch HOME: nothing here reads or writes the real one.
real_home=$HOME
HOME=$WORK/home
export HOME
mkdir -p "$HOME"
[ "$HOME" != "$real_home" ] && [ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
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

# --- the needs -----------------------------------------------------------------------------

record() { # <json>: through the alert step's own writer
    capture psql -d "$SPAWN_DB_NAME" -v ON_ERROR_STOP=1 -tA -c "select spawn_alert_record('$1'::jsonb)"
    [ "$status" -eq 0 ] || harness "spawn_alert_record refused $1: $(err)"
}
for n in 1 2 3 4 5 6; do
    record "{\"type\": \"alert.fired\", \"key\": \"memory_low:$n\", \"kind\": \"memory_low\", \"subject\": \"host\", \"evidence\": {\"measured_mb\": 1000, \"threshold_mb\": 4096}, \"text\": \"MemAvailable is 1000 MiB, under 4096\", \"notice\": false}"
done
record '{"type": "alert.fired", "key": "task_state:7:10", "kind": "task_state", "subject": "task #7", "evidence": {"event_id": 10}, "text": "task #7 assigned", "notice": true}'
judge() { # <key> <verdict>: the owner's verb
    capture "$SPAWN_BIN" alert judge "$1" "$2"
}
get_answer() { # <file>
    code=$(curl -s --noproxy '*' -o "$1" -w '%{http_code}' "$base/api/core-lines" 2>/dev/null) || code=000
    [ "$code" = 200 ] || harness "GET /api/core-lines answered $code: $(head -c 200 "$1")"
}
star() { # <file>: "Needs you is right" (since objective 176, sustainability's guardrail U2)
    jq -c '[.lines[] | select(.line == "sustainability") | .guardrails[] | select(.key == "observability.needs_you_right")][0] | {key, value, judged}' "$1"
}
row() { # <key>: its state and outcome on GET /api/alerts
    curl -s --noproxy '*' "$base/api/alerts" | jq -c --arg k "$1" '[.[] | select(.key == $k) | {state, outcome}][0]'
}
start_serve

# --- (1) the verb takes `right` ------------------------------------------------------------

judge memory_low:1 right
if [ "$status" -ne 0 ]; then
    check_failed "(1) spawn alert judge memory_low:1 right exited $status: $(err | head -c 300)"
fi
judge memory_low:2 right
judge memory_low:3 false
judge memory_low:4 stale
expect_eq "$(row memory_low:1)" '{"state":"open","outcome":"right"}' "(1) an open need judged right stays open, its outcome right"
expect_eq "$(row memory_low:3)" '{"state":"cleared","outcome":"false"}' "(1) a need judged false is cleared"
note "(1) checked: the verb"

# --- (2) four judged: not collected -------------------------------------------------------

judge task_state:7:10 right
[ "$status" -ne 0 ] || check_failed "(2) a notice was judged right; a notice is never judged"
get_answer "$WORK/api.json"
expect_eq "$(star "$WORK/api.json")" \
    '{"key":"observability.needs_you_right","value":null,"judged":{"fired":6,"judged":4,"unjudged":2}}' \
    "(2) with 4 judged, the north star is not collected and says what it is read from"
note "(2) checked: 4 judged"

# --- (3) five judged ------------------------------------------------------------------------

judge memory_low:5 right
get_answer "$WORK/api.json"
expect_eq "$(star "$WORK/api.json")" \
    '{"key":"observability.needs_you_right","value":0.6,"judged":{"fired":6,"judged":5,"unjudged":1}}' \
    "(3) with 5 judged, 3 of them right, the north star is 0.6"
note "(3) checked: 5 judged"

# --- (4) the card's words -------------------------------------------------------------------

cat >"$WORK/card.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { coreLinesView } = await import(pathToFileURL(`${env.BB_TREE}/web/src/coreLines.ts`).href);
const answer = JSON.parse(readFileSync(`${env.BB_WORK}/api.json`, "utf8"));

test("(4) the card says what the share is read from", () => {
  const rows = coreLinesView(answer).rows;
  // Objective 176: a guardrail of the sustainability row.
  const names = rows.flatMap((row) => [row.north_star, ...row.guardrails]).map((mark) => mark.name);
  assert.ok(names.includes("Needs you is right (5 judged, 1 unjudged of 6)"), JSON.stringify(names));
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/card.test.mjs" >"$WORK/card.out" 2>&1; then
    check_failed "(4) web/src/coreLines.ts: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/card.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(4) checked: the card's words"

# --- (5) the 7 days ------------------------------------------------------------------------
# Two needs fired 2 days and 8 days ago: alert.fired records in the control
# log at those times, folded as the store folds its log (spawn_control_refold).
# The writer stamps its own clock, so a past firing is a seeded record. Each
# is judged false. Only the 2-day one is within the 7 days.
now_ms=$(q "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint")
cat >"$WORK/seed.sql" <<SQL
\set ON_ERROR_STOP 1
insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id, kind, session_seq, recorded_at_ms)
select 'agent_control', 'session_control', null, 'control', null,
       jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record',
           jsonb_build_object('type', 'alert.fired', 'key', k, 'kind', 'memory_low', 'subject', 'host',
                              'evidence', jsonb_build_object('measured_mb', 1000, 'threshold_mb', 4096),
                              'text', 'MemAvailable is 1000 MiB, under 4096', 'notice', false))),
       (select coalesce(max(session_seq), 0) from spawn_events where session_id = 'session_control') + n, at_ms
  from (values (1, 'memory_low:7', $((now_ms - 2 * 86400000))::bigint),
               (2, 'memory_low:8', $((now_ms - 8 * 86400000))::bigint)) s(n, k, at_ms);
select spawn_control_refold();
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the needs of 2 and 8 days ago failed: $(err)"
expect_eq "$(q "select string_agg(key || ' ' || ((($now_ms - fired_at_ms) / 86400000)::text), ',' order by key) from spawn_alerts where key in ('memory_low:7', 'memory_low:8')")" \
    "memory_low:7 2,memory_low:8 8" "(5) the seeded needs' days since they fired"
judge memory_low:7 false
[ "$status" -eq 0 ] || harness "judge memory_low:7 false exited $status: $(err)"
judge memory_low:8 false
[ "$status" -eq 0 ] || harness "judge memory_low:8 false exited $status: $(err)"
get_answer "$WORK/api5.json"
expect_eq "$(star "$WORK/api5.json")" \
    '{"key":"observability.needs_you_right","value":0.5,"judged":{"fired":7,"judged":6,"unjudged":1}}' \
    "(5) a need fired 2 days ago counts and one fired 8 days ago does not (3 right of 6 judged)"
note "(5) checked: the 7 days"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "148: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "148: Needs you is right is the share of the judged needs judged right, never a clearing"
exit 0
