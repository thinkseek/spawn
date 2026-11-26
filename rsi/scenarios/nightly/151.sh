#!/bin/sh
# Objective 151 [observability]: the notice fold. A task's or mission's
# notice (task_state, mission_state: news, never in Needs you) is
# superseded by its subject's next one: GET /api/alerts says
# `superseded_by` (the next notice's key) on each that a later notice of
# its subject follows, and keeps every row; the Alerts view's notices list
# shows each subject's newest with how many came before. Read time only:
# nothing is recorded or deleted. Before, the list held every state of
# every task (on live 11-14: 238 notices for 48 subjects).
#
# Black box, through `spawn serve`'s GET /api/alerts and web/src/notices.ts
# under node. Notices and a need are fired through spawn_alert_record, the
# writer the loop's alert step uses: task #7 moves through 4 states
# (events 10 to 13, fired out of order), task #8 has 1, mission #4 has 2;
# one need (memory_low:1) is open.
#   (1) GET /api/alerts holds all 8 rows; task #7's 10, 11 and 12 say
#       superseded_by 11, 12 and 13; its 13 says none; task #8's none;
#       mission #4's 20 says 21; the open need is open and says none.
#   (2) the store holds the same 8 rows, 7 of them notices: nothing
#       recorded or removed by the read.
#   (3) the view's notices: task #7's 13 with "3 earlier", task #8's with
#       none, mission #4's 21 with "1 earlier", in the answer's order.
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure.

set -eu

asserted=""
harness() {
    echo "151: harness: $*" >&2
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
for tool in psql git jq od bash sed grep awk curl node df head; do
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
SPAWN_DB_NAME="bb_151_notices_$run"
SPAWN_HERDR_SESSION="bb-151-$hc_id"
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
            echo "151: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "151: the alerts (key, state):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, state from spawn_alerts order by key" >&2 2>/dev/null || true
        fi
        if [ -s "$WORK/alerts.json" ]; then
            echo "151: the answer's rows (key, state, superseded_by):" >&2
            jq -c '.[] | [.key, .state, .superseded_by]' "$WORK/alerts.json" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "151: harness: unexpected exit $rc" >&2
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
BB_PATH=$PATH
SPAWN_HERDR_CORE_PROGRAM=$SPAWN_HC
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
    SPAWN_CLAUDE_PROJECTS BB_PATH SPAWN_HERDR_CORE_PROGRAM SPAWN_WORKER SPAWN_LOOP_INTERVAL_MS \
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
note() { echo "151: $*" >&2; }
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


# --- the notices and a need ----------------------------------------------------------------

record() { # <json>: through the alert step's own writer
    capture psql -d "$SPAWN_DB_NAME" -v ON_ERROR_STOP=1 -tA -c "select spawn_alert_record('$1'::jsonb)"
    [ "$status" -eq 0 ] || harness "spawn_alert_record refused $1: $(err)"
}
notice() { # <kind> <subject id> <event> <word>
    if [ "$1" = task_state ]; then subject="task #$2"; else subject="mission #$2"; fi
    record "{\"type\": \"alert.fired\", \"key\": \"$1:$2:$3\", \"kind\": \"$1\", \"subject\": \"$subject\", \"evidence\": {\"event_id\": $3}, \"text\": \"$subject $4\", \"notice\": true}"
}
# Out of order: the fold reads the event in the key, not the firing order.
notice task_state 7 10 assigned
notice task_state 7 12 reported
notice task_state 7 11 delivered
notice task_state 8 15 assigned
notice mission_state 4 20 active
notice task_state 7 13 landed
notice mission_state 4 21 halted
record '{"type": "alert.fired", "key": "memory_low:1", "kind": "memory_low", "subject": "host", "evidence": {"measured_mb": 1000, "threshold_mb": 4096}, "text": "MemAvailable is 1000 MiB, under 4096", "notice": false}'
rows_before=$(q "select count(*) || ' ' || count(*) filter (where state = 'notice') from spawn_alerts")
[ "$rows_before" = "8 7" ] || harness "the store holds [$rows_before] rows and notices, not [8 7]"
start_serve

# --- (1) the answer ------------------------------------------------------------------------

code=$(curl -s --noproxy '*' -o "$WORK/alerts.json" -w '%{http_code}' "$base/api/alerts" 2>/dev/null) || code=000
[ "$code" = 200 ] || harness "GET /api/alerts answered $code: $(head -c 200 "$WORK/alerts.json")"
got=$(jq -c '[.[] | {key, state, superseded_by}] | sort_by(.key)' "$WORK/alerts.json")
want='[{"key":"memory_low:1","state":"open","superseded_by":null},{"key":"mission_state:4:20","state":"notice","superseded_by":"mission_state:4:21"},{"key":"mission_state:4:21","state":"notice","superseded_by":null},{"key":"task_state:7:10","state":"notice","superseded_by":"task_state:7:11"},{"key":"task_state:7:11","state":"notice","superseded_by":"task_state:7:12"},{"key":"task_state:7:12","state":"notice","superseded_by":"task_state:7:13"},{"key":"task_state:7:13","state":"notice","superseded_by":null},{"key":"task_state:8:15","state":"notice","superseded_by":null}]'
expect_eq "$got" "$want" "(1) every row kept, each notice superseded by its subject's next one"
note "(1) checked: the answer"

# --- (2) the store -------------------------------------------------------------------------

expect_eq "$(q "select count(*) || ' ' || count(*) filter (where state = 'notice') from spawn_alerts")" "8 7" \
    "(2) the store's rows and notices after the read"
note "(2) checked: the store"

# --- (3) the view's notices ----------------------------------------------------------------

cat >"$WORK/view.test.mjs" <<'TEST'
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const env = process.env;
const { currentNotices, earlierWords } = await import(pathToFileURL(`${env.BB_TREE}/web/src/notices.ts`).href);
// The answer's rows as served (api.ts's normalizeAlerts keeps these fields as they are).
const rows = JSON.parse(readFileSync(`${env.BB_WORK}/alerts.json`, "utf8"));

test("(3) the view shows each subject's newest notice with how many came before", () => {
  const got = currentNotices(rows).map(({ alert, earlier }) => [alert.key, earlierWords(earlier)]);
  const want = [
    ["task_state:7:13", "3 earlier"],
    ["task_state:8:15", null],
    ["mission_state:4:21", "1 earlier"],
  ];
  // In the answer's order (by firing time, then key).
  const order = rows.filter((row) => row.state === "notice").map((row) => row.key);
  want.sort((a, b) => order.indexOf(a[0]) - order.indexOf(b[0]));
  assert.deepEqual(got, want);
});
TEST
if ! BB_TREE=$tree BB_WORK=$WORK node --test "$WORK/view.test.mjs" >"$WORK/view.out" 2>&1; then
    check_failed "(3) web/src/notices.ts: $(grep -E '^not ok|AssertionError|expected|actual|Error' "$WORK/view.out" | head -n 12 | tr '\n' ' ' | head -c 1000)"
fi
note "(3) checked: the view's notices"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "151: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "151: each task's and mission's notices fold to its newest, every row kept"
exit 0
