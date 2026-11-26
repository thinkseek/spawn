#!/bin/sh
# Objective 202 [storage] (rev-f's n2 on the cc-astra A82 fix, the lead's
# ask of 2026-11-15): a storage source the extents script did not measure
# keeps the script's own reason in the loop's daily reading.
#
# The problem: since A82, scripts/storage-extents.py stops walking once a
# source is over its bound, and answers each later source with
# {"name", "not_measured": "not walked: <earlier>, before it, was over its
# bound, so which extents it held first is unknown"}. The loop's reading
# (objective 180, `host.storage_bytes`) dropped that reason and recorded
# "the extents script's answer for it has no bytes", so the record, and
# S1's note that 185 builds from it, says not measured without why.
#
# Required (through `spawn loop`'s records): the loop runs the extents
# script SPAWN_STORAGE_EXTENTS names, here a stand-in that answers in the
# real script's shapes (transcripts measured, build_cache over its bound,
# scratch not walked, naming build_cache), and records one reading where:
#   (1) scratch is not measured with the script's reason, word for word;
#   (2) build_cache is not measured "over its bound of 1200 s (7 files
#       read)", and transcripts has its bytes (as before).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
#
# Fails with 1 on the base (live main, with 180): "(1) scratch, not walked
# after build_cache was over its bound: expected [{"not_measured":"not
# walked: build_cache, ..."}], got [{"not_measured":"the extents script's
# answer for it has no bytes"}]".
set -eu

asserted=""
harness() {
    echo "202: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

# The candidate's tree: the gate runs a scenario from inside it, and its
# scripts/storage-extents.py is what the loop would run.
tree=$(pwd)
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od sed grep head cp sync python3 nice; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
[ -d "$WORK" ] || harness "mktemp gave no directory"
trap 'rm -rf "$WORK"' EXIT
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_202_storage_why_$run"
. "$controller/scripts/blackbox/lib-herdr.sh"
herdr_setup "$WORK" "202"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
stop_loop() {
    [ -n "$loop_pid" ] || return 0
    kill "$loop_pid" 2>/dev/null || true
    wait "$loop_pid" 2>/dev/null || true
    loop_pid=""
}
cleanup() {
    stop_loop
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
            echo "202: the loop's last storage lines:" >&2
            grep -i "storage" "$WORK/loop.log" | tail -n 8 >&2 || true
        fi
        if [ -n "$database_made" ]; then
            echo "202: the host.storage_bytes records:" >&2
            psql -d "$SPAWN_DB_NAME" -tA -c "select recorded_at_ms, kind -> 'detail' -> 'record' -> 'sources' from spawn_events
                where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes' order by id" >&2 2>/dev/null || true
        fi
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "202: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

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

bb_engine >"$WORK/claude" <<'ENGINE'
#!/usr/bin/env bash
printf 'Welcome to Claude Code\n\n❯ '
while :; do
  if bb_read_line typed; then printf 'you: %s\n\nidle\n❯ ' "$typed"; fi
done
ENGINE
chmod 755 "$WORK/claude"

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_CHEAP_MODEL SPAWN_SMALL_MODEL \
    SPAWN_MODEL_PRICES SPAWN_API_TOKEN SPAWN_SESSION_BACKUP_STATUS SPAWN_FLEET_TEST_DATABASE_URL \
    SPAWN_STORAGE_EXTENTS SPAWN_QUIET_HOLD_FILE XDG_CACHE_HOME \
    http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
HOME=$WORK/home
export HOME
mkdir -p "$HOME"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home";; esac

SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_GIT_REPO=$repo
SPAWN_WORKSPACE_ROOT=$WORK/ws
SPAWN_CLAUDE_PROGRAM=$WORK/claude
SPAWN_CLAUDE_PROJECTS=$WORK/projects
SPAWN_WORKER=worker-bb
SPAWN_DISPATCH=0
SPAWN_LOOP_INTERVAL_MS=300
SPAWN_MISSION_FILE=$WORK/mission.md
SPAWN_DISK_WARN_GB=0
SPAWN_DISK_FLOOR_GB=0
SPAWN_CORE_LINES_FILE=$WORK/core-lines.json
quiet=$WORK/owner-gate.state
SPAWN_STORAGE_EXTENTS=$WORK/extents-standin.py
export SPAWN_DATABASE_URL SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT SPAWN_CLAUDE_PROGRAM \
    SPAWN_CLAUDE_PROJECTS SPAWN_WORKER SPAWN_DISPATCH SPAWN_LOOP_INTERVAL_MS \
    SPAWN_MISSION_FILE SPAWN_DISK_WARN_GB SPAWN_DISK_FLOOR_GB SPAWN_CORE_LINES_FILE \
    SPAWN_STORAGE_EXTENTS
mkdir -p "$SPAWN_WORKSPACE_ROOT"
: >"$SPAWN_MISSION_FILE"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

now() { date +%s; }
q() { scalar "$1"; }
note() { echo "202: $*" >&2; }
wait_until() { # <seconds> <command...>
    limit=$1
    shift
    deadline=$(($(now) + limit))
    until "$@"; do
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}
start_loop() {
    env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" loop --repo "$repo" --root "$SPAWN_WORKSPACE_ROOT" >>"$WORK/loop.log" 2>&1 &
    loop_pid=$!
}
readings() { q "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes'"; }

# --- the disk and the stand-in script ---------------------------------------------
# Each of the three sources has a root, so the loop asks the script for each.
scratch=$WORK/scratch
cache=$HOME/.cache
mkdir -p "$scratch" "$cache" "$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a" "$SPAWN_CLAUDE_PROJECTS/-bb"
printf 'x\n' >"$scratch/a.txt"
printf 'x\n' >"$cache/c.txt"
printf 'x\n' >"$SPAWN_WORKSPACE_ROOT/.transcripts/agent_a/t.jsonl"
WHY="not walked: build_cache, before it, was over its bound, so which extents it held first is unknown"
# The real script's shapes (scripts/storage-extents.py, main() since A82):
# a measured source, one over its bound, and a later one not walked.
cat >"$SPAWN_STORAGE_EXTENTS" <<'PY'
import json, sys
config = json.load(sys.stdin)
readings = []
for source in config["sources"]:
    name = source["name"]
    if name == "transcripts":
        readings.append({"name": name, "bytes": 4096, "files": 1, "unreadable": 0, "seconds": 0.1})
    elif name == "build_cache":
        readings.append({"name": name, "over_bound": True, "files": 7, "seconds": 1200.2})
    else:
        readings.append({"name": name, "not_measured": (
            "not walked: build_cache, before it, was over its bound, so which extents it "
            "held first is unknown")})
json.dump({"sources": readings}, sys.stdout)
sys.stdout.write("\n")
PY
printf '{"storage": {"scratch_roots": ["%s"], "quiet_hold_file": "%s"}}\n' "$scratch" "$quiet" >"$SPAWN_CORE_LINES_FILE"
printf 'ENDED fd-bb at 10:05\n' >"$quiet"

# --- (1), (2) the reading -------------------------------------------------------------
reading_taken() { [ "$(readings)" -ge 1 ]; }
start_loop
if wait_until 120 reading_taken; then
    sources=$(q "select kind -> 'detail' -> 'record' -> 'sources' from spawn_events
        where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes' order by id desc limit 1")
    expect_eq "$(printf '%s' "$sources" | jq -c '.scratch')" "$(jq -cn --arg why "$WHY" '{not_measured: $why}')" \
        "(1) scratch, not walked after build_cache was over its bound"
    expect_eq "$(printf '%s' "$sources" | jq -c '.build_cache')" '{"not_measured":"over its bound of 1200 s (7 files read)"}' \
        "(2) build_cache, over its bound"
    expect_eq "$(printf '%s' "$sources" | jq -c '.transcripts.bytes')" 4096 "(2) transcripts, measured"
    note "(1), (2) checked: the reading keeps the script's reason for a source it did not walk"
else
    check_failed "(1) the loop recorded no host.storage_bytes reading within 120 s of its start"
fi
stop_loop

if [ -s "$WORK/failures" ]; then
    asserted=1
    exit 1
fi
echo "202: a source the extents script did not walk keeps the script's reason in the loop's reading"
