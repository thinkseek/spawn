#!/bin/sh
# Objective 204 [storage]: free disk under 40 GiB is an alert of its own
# (the owner's core metric set of 2026-11-14: "Free disk: an ALERT only
# (under 40 GB)"; the lead's rulings of 2026-11-16, objective 182's split).
#
# Required (read through `GET /api/alerts`):
#   (1) with SPAWN_ALERT_DISK_LOW_GB=1000000 and SPAWN_ALERT_DISK_OK_GB=2000000
#       (over any disk), the loop's alert step opens one `disk_low` alert,
#       subject `host`, evidence {path: the workspace root, free_bytes (a
#       number), threshold_bytes: 1000000 GiB, reading_ms}, its text naming
#       the path, the free GiB and "under 1000000 GiB"; it stays one while
#       it holds.
#   (2a) the loop started again with SPAWN_ALERT_DISK_LOW_GB=1 and
#       SPAWN_ALERT_DISK_OK_GB=1000000 (above the threshold, under the
#       margin): the alert stays open over three checks.
#   (2) the loop started again with SPAWN_ALERT_DISK_LOW_GB=1 and
#       SPAWN_ALERT_DISK_OK_GB=2 (a gate's 15 GiB tmpfs has 2 GiB free): the
#       alert clears (2 GiB free under the root is checked first, else a
#       harness error), its cleared evidence {path, free_bytes,
#       threshold_bytes: 2 GiB, the margin}. The two differ in (1) and (2)
#       so a record that names the other one fails (rev-e's F1).
#   (3) the loop started again without either (the default, 0): no new
#       disk_low opens over two of the step's checks, whatever the disk
#       holds (every gate scenario's root is on a 15 GiB tmpfs).
#   (4) the margin's default, 45 GiB (rev-b's F1): a new alert opened with
#       SPAWN_ALERT_DISK_LOW_GB=1000000 SPAWN_ALERT_DISK_OK_GB=2000000, then
#       the loop started again with SPAWN_ALERT_DISK_LOW_GB=1 alone, three
#       checks later: with under 45 GiB free under the root (a gate's
#       tmpfs) it is still open; with 45 GiB or more it is cleared, its
#       cleared evidence's threshold_bytes 45 GiB. A default margin of 0
#       clears it in either case, with threshold_bytes 0.
#
# A real `spawn loop` and `spawn serve` on a scratch database (089's
# harness), the alert step's check every 2 s.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on live main (73cbdd8e, and the base 6f8ceb8c): no disk_low
# alert opens in (1).

set -eu

asserted=""
harness() {
    echo "204: harness: $*" >&2
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
SPAWN_DB_NAME="bb_204_disk_$run"
SPAWN_TMUX_SOCKET="bb-204-$run"
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
            echo "204: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "204: the database_size alerts (key, subject, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, subject, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'database_size' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "204: harness: unexpected exit $rc" >&2
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
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    SPAWN_ALERT_DISK_LOW_GB SPAWN_ALERT_DISK_OK_GB SPAWN_TEST_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL; do
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
note() { echo "204: $*" >&2; }
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


# --- (1) under its threshold: one alert ------------------------------------------------------

GIB=1073741824
disk_alerts() { # every disk_low alert in /api/alerts, oldest first
    alerts_json
    jq -c '[.[] | select(.kind == "disk_low")] | sort_by(.fired_at_ms)' "$WORK/alerts.json"
}
open_count() { disk_alerts | jq '[.[] | select(.state == "open")] | length'; }
one_open() { [ "$(open_count)" = 1 ]; }
start_loop() { # [VAR=value ...]: the loop with these thresholds
    env "$@" "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}
stop_loop() {
    kill "$loop_pid" 2>/dev/null || true
    wait "$loop_pid" 2>/dev/null || true
    loop_pid=""
}
start_serve
start_loop SPAWN_ALERT_DISK_LOW_GB=1000000 SPAWN_ALERT_DISK_OK_GB=2000000
if wait_until 60 one_open; then
    expect_eq "$(disk_alerts | jq -c --arg root "$SPAWN_WORKSPACE_ROOT" --argjson t $((1000000 * GIB)) '[.[] | {
        subject, path: (.evidence.path == $root), free: (.evidence.free_bytes | type),
        threshold: (.evidence.threshold_bytes == $t), read: (.evidence.reading_ms | type),
        text: ((.text | contains($root)) and (.text | test("[0-9]+\\.[0-9] GiB free, under 1000000 GiB")))}]')" \
        '[{"subject":"host","path":true,"free":"number","threshold":true,"read":"number","text":true}]' \
        "(1) the disk_low alert: subject, evidence and text"
    sleep 5
    expect_eq "$(disk_alerts | jq -c 'length')" 1 "(1) one disk_low while it holds, over two more checks"
else
    check_failed "(1) no open disk_low alert 60 s after the loop started with SPAWN_ALERT_DISK_LOW_GB=1000000: $(disk_alerts | head -c 400)"
fi
note "(1) checked: the alert"

# --- (2a) above its threshold but under its margin: it stays open ---------------------------------

stop_loop
start_loop SPAWN_ALERT_DISK_LOW_GB=1 SPAWN_ALERT_DISK_OK_GB=1000000
sleep 6
expect_eq "$(disk_alerts | jq -c '[.[] | .state]')" '["open"]' "(2a) above 1 GiB but under the 1000000 GiB margin, still open over three checks"
note "(2a) checked: the margin"

# --- (2) at or above its margin: it clears --------------------------------------------------------

stop_loop
free_kib=$(df -Pk "$SPAWN_WORKSPACE_ROOT" | awk 'NR == 2 { print $4 }')
case "$free_kib" in '' | *[!0-9]*) harness "cannot read the free space under $SPAWN_WORKSPACE_ROOT" ;; esac
[ "$free_kib" -ge $((2 * 1024 * 1024)) ] ||
    harness "(2) needs 2 GiB free under $SPAWN_WORKSPACE_ROOT, which has $free_kib KiB"
start_loop SPAWN_ALERT_DISK_LOW_GB=1 SPAWN_ALERT_DISK_OK_GB=2
cleared() { [ "$(disk_alerts | jq -c '[.[] | .state]')" = '["cleared"]' ]; }
if wait_until 60 cleared; then
    expect_eq "$(disk_alerts | jq -c --arg root "$SPAWN_WORKSPACE_ROOT" --argjson t $((2 * GIB)) '[.[] | .cleared_evidence | {
        path: (.path == $root), free: (.free_bytes | type), threshold: (.threshold_bytes == $t)}]')" \
        '[{"path":true,"free":"number","threshold":true}]' \
        "(2) the clearing's evidence: the path, the free bytes, the margin (2 GiB, not the 1 GiB threshold)"
else
    check_failed "(2) the disk_low alert did not clear within 60 s of the loop starting with its threshold at 1 GiB and its margin at 2 GiB: $(disk_alerts | head -c 400)"
fi
note "(2) checked: the clearing"

# --- (3) the default: off ----------------------------------------------------------------------------

stop_loop
start_loop
sleep 6
expect_eq "$(disk_alerts | jq -c '[.[] | .state]')" '["cleared"]' "(3) with no threshold set, no new disk_low over three checks"
note "(3) checked: off by default"

# --- (4) the threshold alone: the margin is its default, 45 GiB ---------------------------------------

stop_loop
start_loop SPAWN_ALERT_DISK_LOW_GB=1000000 SPAWN_ALERT_DISK_OK_GB=2000000
two_alerts_one_open() { [ "$(disk_alerts | jq -c '[.[] | .state]')" = '["cleared","open"]' ]; }
if ! wait_until 60 two_alerts_one_open; then
    # A failed check, not a harness error: on a main without disk_low, (1)
    # has failed already and this one fails the same way (exit 1).
    check_failed "(4) no second disk_low opened within 60 s with SPAWN_ALERT_DISK_LOW_GB=1000000: $(disk_alerts | head -c 400)"
else
stop_loop
start_loop SPAWN_ALERT_DISK_LOW_GB=1
sleep 6
free_kib=$(df -Pk "$SPAWN_WORKSPACE_ROOT" | awk 'NR == 2 { print $4 }')
case "$free_kib" in '' | *[!0-9]*) harness "cannot read the free space under $SPAWN_WORKSPACE_ROOT" ;; esac
newest=$(disk_alerts | jq -c 'last | {state, threshold: .cleared_evidence.threshold_bytes}')
if [ "$free_kib" -lt $((45 * 1024 * 1024)) ]; then
    expect_eq "$newest" '{"state":"open","threshold":null}' \
        "(4) with SPAWN_ALERT_DISK_LOW_GB=1 alone and $free_kib KiB free (under the 45 GiB default margin), the alert"
else
    expect_eq "$newest" "{\"state\":\"cleared\",\"threshold\":$((45 * GIB))}" \
        "(4) with SPAWN_ALERT_DISK_LOW_GB=1 alone and $free_kib KiB free (45 GiB or more), the alert cleared at the default margin"
fi
note "(4) checked: the default margin ($free_kib KiB free)"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "204: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "204: free disk under its threshold is one disk_low alert, cleared at its margin, off by default"
exit 0
