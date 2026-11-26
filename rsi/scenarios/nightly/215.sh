#!/bin/sh
# Objective 215 [observability] (GitHub #66 part 2): tmpfs_high reads a tmpfs
# alone. It measured the used space of the filesystem holding
# SPAWN_ALERT_TMPFS_PATH (default /tmp) and cleared under
# SPAWN_ALERT_TMPFS_OK_MB. On a host whose /tmp is a directory of the root
# filesystem, "used" is the whole disk: the alert opened and never cleared
# (the issue: 61053 MiB "in /tmp" on a 118 GB root, open for days).
#
# Required (read through `GET /api/alerts` and the store's alert rows):
#   (1) control: with SPAWN_ALERT_TMPFS_PATH a tmpfs (this run's WORK when it
#       is on one, as in the gate, else /dev/shm), HIGH_MB=1 and OK_MB=1, and
#       2 MiB written there, the loop's alert step opens one tmpfs_high,
#       subject host, its evidence path the tmpfs.
#   (2) the loop started again with SPAWN_ALERT_TMPFS_PATH=/usr (a directory
#       of the root filesystem, which `stat -f` says is no tmpfs; checked
#       first, else a harness error), HIGH_MB=1 and OK_MB=1: the open alert
#       clears within 30 s, its cleared evidence {path: "/usr", not_tmpfs:
#       true, fs_type: a "0x..." string}.
#   (3) and over three more checks no new tmpfs_high opens, though the root
#       filesystem holds far more than 1 MiB.
#
# A real `spawn loop` and `spawn serve` on a scratch database (204's
# harness), the alert step's check every 2 s.
#
# Exit status: 0 every check passed; 1 an assertion failed; 2 a harness or
# infrastructure failure. Any unexpected exit is reported as 2.
#
# Fails with 1 on live main a599100c: in (2) the alert stays open (/usr's
# filesystem holds more than 1 MiB used) and in (3) none clears.

set -eu

asserted=""
harness() {
    echo "215: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql git jq od bash sed grep awk curl stat; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_215_tmpfs_$run"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "215"
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
# The 2 MiB fill, when it is outside WORK (/dev/shm): removed by cleanup,
# which every exit runs through finish.
fill_outside=""
cleanup() {
    [ -z "$fill_outside" ] || rm -f "$fill_outside"
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
            echo "215: the loop's last lines:" >&2
            tail -n 10 "$WORK/loop.log" >&2
        fi
        if [ -n "$database_made" ]; then
            echo "215: the tmpfs_high alerts (key, state, evidence, cleared evidence):" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select key, state, evidence::text, coalesce(cleared_evidence::text, '')
                from spawn_alerts where kind = 'tmpfs_high' order by fired_event_id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "215: harness: unexpected exit $rc" >&2
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
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    SPAWN_ALERT_TMPFS_PATH SPAWN_ALERT_TMPFS_HIGH_MB SPAWN_ALERT_TMPFS_OK_MB SPAWN_TEST_DATABASE_URL SPAWN_FLEET_TEST_DATABASE_URL; do
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
note() { echo "215: $*" >&2; }
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


# --- the alerts -------------------------------------------------------------------

tmpfs_alerts() { # every tmpfs_high alert in /api/alerts, oldest first
    alerts_json
    jq -c '[.[] | select(.kind == "tmpfs_high")] | sort_by(.fired_at_ms)' "$WORK/alerts.json"
}
states() { tmpfs_alerts | jq -c '[.[] | .state]'; }
start_loop() { # [VAR=value ...]: the loop with these settings
    env "$@" "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}
stop_loop() {
    kill "$loop_pid" 2>/dev/null || true
    wait "$loop_pid" 2>/dev/null || true
    loop_pid=""
}

# The two paths: a tmpfs, and a directory that is not on one.
fstype() { stat -f -c %T "$1" 2>/dev/null || echo unknown; }
tmpfs_dir=""
for candidate in "$WORK" /dev/shm; do
    if [ -d "$candidate" ] && [ -w "$candidate" ] && [ "$(fstype "$candidate")" = tmpfs ]; then
        tmpfs_dir=$candidate
        break
    fi
done
[ -n "$tmpfs_dir" ] || harness "no writable tmpfs here (WORK $WORK is $(fstype "$WORK"), /dev/shm $(fstype /dev/shm))"
[ -d /usr ] || harness "no /usr"
[ "$(fstype /usr)" != tmpfs ] || harness "/usr is a tmpfs here: (2) needs a directory that is not on one"
used_kib=$(df -Pk /usr | awk 'NR == 2 { print $3 }')
case "$used_kib" in '' | *[!0-9]*) harness "cannot read the used space of /usr's filesystem";; esac
[ "$used_kib" -gt 4096 ] || harness "/usr's filesystem has only $used_kib KiB used: (3) would prove nothing"
if [ "$tmpfs_dir" = "$WORK" ]; then fill=$WORK/fill; else fill=$tmpfs_dir/bb-215-$run; fi
# A fill outside WORK goes with it, in cleanup (never a second EXIT trap: under
# set -e its own `(exit $rc)` ended the shell before finish, leaking the loop,
# serve and the database of every failing run; rev-k, 19:41).
[ "$tmpfs_dir" = "$WORK" ] || fill_outside=$fill
dd if=/dev/zero of="$fill" bs=1048576 count=2 2>/dev/null || harness "cannot write 2 MiB to $tmpfs_dir"

start_serve

# --- (1) control: a tmpfs over its threshold ------------------------------------------

start_loop SPAWN_ALERT_TMPFS_PATH="$tmpfs_dir" SPAWN_ALERT_TMPFS_HIGH_MB=1 SPAWN_ALERT_TMPFS_OK_MB=1
one_open() { [ "$(states)" = '["open"]' ]; }
wait_until 60 one_open ||
    harness "(1) no tmpfs_high opened within 60 s on the tmpfs $tmpfs_dir with HIGH_MB=1 (the control): $(tmpfs_alerts | head -c 400)"
expect_eq "$(tmpfs_alerts | jq -c --arg p "$tmpfs_dir" '[.[] | {subject, path: (.evidence.path == $p)}]')" \
    '[{"subject":"host","path":true}]' "(1) the control alert: subject and path"
note "(1) checked: the control alert on $tmpfs_dir"

# --- (2) a path that is not on a tmpfs: the open one clears ---------------------------

stop_loop
start_loop SPAWN_ALERT_TMPFS_PATH=/usr SPAWN_ALERT_TMPFS_HIGH_MB=1 SPAWN_ALERT_TMPFS_OK_MB=1
cleared() { [ "$(states)" = '["cleared"]' ]; }
if wait_until 30 cleared; then
    expect_eq "$(tmpfs_alerts | jq -c '[.[] | .cleared_evidence | {path, not_tmpfs, hex: (.fs_type | type == "string" and test("^0x[0-9a-f]+$"))}]')" \
        '[{"path":"/usr","not_tmpfs":true,"hex":true}]' \
        "(2) the clearing's evidence: the path, not_tmpfs, the filesystem's type"
else
    check_failed "(2) the tmpfs_high alert did not clear within 30 s of the loop reading /usr ($(fstype /usr), $used_kib KiB used): $(tmpfs_alerts | jq -c '[.[] | {state, cleared_evidence}]' | head -c 400)"
fi
note "(2) checked: the clearing"

# --- (3) and nothing new opens, however full that disk is ------------------------------

sleep 6
expect_eq "$(tmpfs_alerts | jq -c '[.[] | select(.state == "open")] | length')" 0 \
    "(3) open tmpfs_high alerts over three more checks of /usr ($used_kib KiB used, threshold 1 MiB)"
note "(3) checked: none opens on a path that is not a tmpfs"

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "215: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "215: tmpfs_high reads a tmpfs alone, and clears on a path that is not one"
exit 0
